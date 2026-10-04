use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const MAX_CLIPBOARD_TEXT_BYTES: usize = 1024 * 1024;
use super::file_tree::{
    FILE_TREE_PATH_LIST_MAX_BYTES as MAX_CLIPBOARD_FILE_LIST_BYTES,
    FILE_TREE_PATH_LIST_MAX_ITEMS as MAX_CLIPBOARD_FILE_ITEMS,
    FILE_TREE_PATH_MAX_BYTES as MAX_CLIPBOARD_PATH_BYTES,
};
const MAX_PNG_BYTES: usize = 32 * 1024 * 1024;
const MAX_RGBA_PIXELS: usize = 40_000_000;

const CLIPBOARD_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const CLIPBOARD_CACHE_MAX_FILES: usize = 64;
const CLIPBOARD_CACHE_MAX_BYTES: u64 = 256 * 1024 * 1024;
const CLIPBOARD_CACHE_MAX_SCAN_ENTRIES: usize = 4_096;
const CLIPBOARD_CACHE_LOCK_NAME: &str = ".deppy-sijo-clipboard-cache.lock";
const CLIPBOARD_IMAGE_PREFIX: &str = "clipboard-image-";

fn clipboard_error(code: &'static str) -> anyhow::Error {
    anyhow::anyhow!(code)
}

pub(crate) fn paste_clipboard_paths_or_image_to_paths() -> anyhow::Result<Option<Vec<PathBuf>>> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|_| clipboard_error("clipboard.open_failed"))?;
    if let Some(paths) = clipboard_file_list(&mut clipboard)? {
        return Ok(Some(paths));
    }
    paste_clipboard_image_to_png_with(&mut clipboard).map(|path| path.map(|path| vec![path]))
}

/// OS clipboard text fallback for terminal panes. The platform API materializes the source
/// string before its size is observable, so this boundary rejects it before returning/retaining
/// an oversized value.
pub fn read_clipboard_text() -> Option<String> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    clipboard.get_text().ok().and_then(bounded_clipboard_text)
}

fn bounded_clipboard_text(text: String) -> Option<String> {
    (!text.is_empty() && text.len() <= MAX_CLIPBOARD_TEXT_BYTES).then_some(text)
}

fn validate_clipboard_paths(paths: &[PathBuf]) -> anyhow::Result<()> {
    if paths.is_empty() || paths.len() > MAX_CLIPBOARD_FILE_ITEMS {
        return Err(clipboard_error("clipboard.file_list.item_limit"));
    }
    let mut retained_bytes = 0usize;
    for path in paths {
        let path_bytes = path.as_os_str().as_encoded_bytes().len();
        if path_bytes == 0
            || path_bytes > MAX_CLIPBOARD_PATH_BYTES
            || path.as_os_str().as_encoded_bytes().contains(&0)
        {
            return Err(clipboard_error("clipboard.file_list.path_limit"));
        }
        retained_bytes = retained_bytes
            .checked_add(path_bytes)
            .ok_or_else(|| clipboard_error("clipboard.file_list.byte_limit"))?;
        if retained_bytes > MAX_CLIPBOARD_FILE_LIST_BYTES {
            return Err(clipboard_error("clipboard.file_list.byte_limit"));
        }
    }
    Ok(())
}

fn clipboard_file_list(clipboard: &mut arboard::Clipboard) -> anyhow::Result<Option<Vec<PathBuf>>> {
    match clipboard.get().file_list() {
        Ok(paths) if paths.is_empty() => Ok(None),
        Ok(paths) => {
            validate_clipboard_paths(&paths)?;
            Ok(Some(paths))
        }
        Err(arboard::Error::ContentNotAvailable)
        | Err(arboard::Error::ClipboardNotSupported)
        | Err(arboard::Error::ConversionFailure) => Ok(None),
        Err(_) => Err(clipboard_error("clipboard.file_list.read_failed")),
    }
}

/// Reads only a bounded OS clipboard file list. The platform API owns the initial allocation;
/// oversized lists are discarded before this boundary returns them.
pub fn read_clipboard_file_list() -> Option<Vec<PathBuf>> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    clipboard_file_list(&mut clipboard).ok().flatten()
}

/// Writes bounded file URLs to the macOS pasteboard for Finder-compatible paste.
#[cfg(target_os = "macos")]
pub fn copy_file_urls_to_clipboard(paths: &[PathBuf]) -> anyhow::Result<()> {
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{NSPasteboard, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSString, NSURL};

    validate_clipboard_paths(paths)?;
    let pasteboard = NSPasteboard::generalPasteboard();
    let urls: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = paths
        .iter()
        .map(|path| {
            let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
            ProtocolObject::from_retained(url)
        })
        .collect();
    let array = NSArray::from_retained_slice(&urls);
    pasteboard.clearContents();
    if !pasteboard.writeObjects(&array) {
        return Err(clipboard_error("clipboard.file_list.write_failed"));
    }
    Ok(())
}

/// Non-macOS fallback. It keeps the same item/path/aggregate limits while projecting paths to
/// text, without retaining an intermediate vector of strings.
#[cfg(not(target_os = "macos"))]
pub fn copy_file_urls_to_clipboard(paths: &[PathBuf]) -> anyhow::Result<()> {
    validate_clipboard_paths(paths)?;
    let mut text = String::new();
    for (index, path) in paths.iter().enumerate() {
        let display = path.to_string_lossy();
        let separator_bytes = usize::from(index > 0);
        let next_len = text
            .len()
            .checked_add(separator_bytes)
            .and_then(|len| len.checked_add(display.len()))
            .ok_or_else(|| clipboard_error("clipboard.file_list.byte_limit"))?;
        if next_len > MAX_CLIPBOARD_FILE_LIST_BYTES {
            return Err(clipboard_error("clipboard.file_list.byte_limit"));
        }
        if index > 0 {
            text.push('\n');
        }
        text.push_str(&display);
    }
    let mut clipboard =
        arboard::Clipboard::new().map_err(|_| clipboard_error("clipboard.open_failed"))?;
    clipboard
        .set_text(text)
        .map_err(|_| clipboard_error("clipboard.file_list.write_failed"))
}

/// On macOS, reject an oversized NSData before copying it into Rust-owned memory.
#[cfg(target_os = "macos")]
fn clipboard_png_bytes() -> anyhow::Result<Option<Vec<u8>>> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypePNG};

    // SAFETY: generalPasteboard is a shared singleton and dataForType is an immutable read.
    unsafe {
        let pasteboard = NSPasteboard::generalPasteboard();
        let Some(data) = pasteboard.dataForType(NSPasteboardTypePNG) else {
            return Ok(None);
        };
        if data.is_empty() {
            return Ok(None);
        }
        if data.len() > MAX_PNG_BYTES {
            return Err(clipboard_error("clipboard.image.png_byte_limit"));
        }
        Ok(Some(data.to_vec()))
    }
}

fn paste_clipboard_image_to_png_with(
    clipboard: &mut arboard::Clipboard,
) -> anyhow::Result<Option<PathBuf>> {
    #[cfg(target_os = "macos")]
    if let Some(png) = clipboard_png_bytes()? {
        let path = next_clipboard_image_path();
        write_clipboard_cache_file(&path, |file| {
            file.write_all(&png)
                .map_err(|_| clipboard_error("clipboard.image.write_failed"))
        })?;
        return Ok(Some(path));
    }

    // The arboard fallback necessarily materializes RGBA before dimensions are observable. We
    // perform checked arithmetic immediately, retain its borrowed bytes without another full
    // clone, and cap the encoded output writer at the same 32 MiB cache object ceiling.
    let image = match clipboard.get_image() {
        Ok(image) => image,
        Err(arboard::Error::ContentNotAvailable) => return Ok(None),
        Err(_) => return Err(clipboard_error("clipboard.image.read_failed")),
    };
    let expected_bytes = checked_rgba_bytes(image.width, image.height)?;
    if image.bytes.len() != expected_bytes {
        return Err(clipboard_error("clipboard.image.rgba_size_mismatch"));
    }

    let path = next_clipboard_image_path();
    write_rgba_png(&path, image.width, image.height, image.bytes.as_ref())?;
    Ok(Some(path))
}

fn checked_rgba_bytes(width: usize, height: usize) -> anyhow::Result<usize> {
    if width == 0 || height == 0 || width > u32::MAX as usize || height > u32::MAX as usize {
        return Err(clipboard_error("clipboard.image.dimension_limit"));
    }
    let pixels = width
        .checked_mul(height)
        .ok_or_else(|| clipboard_error("clipboard.image.pixel_limit"))?;
    if pixels > MAX_RGBA_PIXELS {
        return Err(clipboard_error("clipboard.image.pixel_limit"));
    }
    pixels
        .checked_mul(4)
        .ok_or_else(|| clipboard_error("clipboard.image.rgba_byte_limit"))
}

#[derive(Clone, Copy)]
struct CacheLimits {
    max_files: usize,
    max_bytes: u64,
    max_scan_entries: usize,
    ttl: Duration,
}

const CACHE_LIMITS: CacheLimits = CacheLimits {
    max_files: CLIPBOARD_CACHE_MAX_FILES,
    max_bytes: CLIPBOARD_CACHE_MAX_BYTES,
    max_scan_entries: CLIPBOARD_CACHE_MAX_SCAN_ENTRIES,
    ttl: CLIPBOARD_CACHE_TTL,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum OwnedCacheKind {
    Image,
    Temporary,
}

struct CacheEntry {
    file_name: OsString,
    bytes: u64,
    modified: SystemTime,
    kind: OwnedCacheKind,
}

fn owned_cache_kind(file_name: &OsStr) -> Option<OwnedCacheKind> {
    let name = file_name.to_str()?;
    let (body, kind) = if let Some(body) = name.strip_suffix(".png") {
        (body, OwnedCacheKind::Image)
    } else if let Some(body) = name.strip_suffix(".tmp") {
        (body, OwnedCacheKind::Temporary)
    } else {
        return None;
    };
    let body = body.strip_prefix(CLIPBOARD_IMAGE_PREFIX)?;
    let mut parts = body.splitn(3, '-');
    let pid = parts.next()?;
    let millis = parts.next()?;
    let uuid = parts.next()?;
    if pid.is_empty()
        || pid.len() > 20
        || !pid.bytes().all(|byte| byte.is_ascii_digit())
        || millis.is_empty()
        || millis.len() > 20
        || !millis.bytes().all(|byte| byte.is_ascii_digit())
        || uuid::Uuid::parse_str(uuid).is_err()
    {
        return None;
    }
    Some(kind)
}

fn planned_cache_removals(
    entries: &[CacheEntry],
    now: SystemTime,
    incoming_files: usize,
    incoming_bytes: u64,
    limits: CacheLimits,
) -> anyhow::Result<Vec<usize>> {
    if incoming_files > limits.max_files || incoming_bytes > limits.max_bytes {
        return Err(clipboard_error("clipboard.cache.admission_limit"));
    }

    let mut removals = Vec::with_capacity(entries.len());
    let mut retained = Vec::with_capacity(entries.len());
    let mut retained_bytes = 0u64;
    for (index, entry) in entries.iter().enumerate() {
        let expired = now
            .duration_since(entry.modified)
            .is_ok_and(|age| age >= limits.ttl);
        if entry.kind == OwnedCacheKind::Temporary || expired {
            removals.push(index);
        } else {
            retained_bytes = retained_bytes
                .checked_add(entry.bytes)
                .ok_or_else(|| clipboard_error("clipboard.cache.metadata_overflow"))?;
            retained.push(index);
        }
    }

    retained.sort_unstable_by(|left, right| {
        entries[*left]
            .modified
            .cmp(&entries[*right].modified)
            .then_with(|| entries[*left].file_name.cmp(&entries[*right].file_name))
    });
    let mut retained_files = retained.len();
    let mut oldest = 0usize;
    while retained_files
        .checked_add(incoming_files)
        .is_none_or(|count| count > limits.max_files)
        || retained_bytes
            .checked_add(incoming_bytes)
            .is_none_or(|bytes| bytes > limits.max_bytes)
    {
        let Some(index) = retained.get(oldest).copied() else {
            return Err(clipboard_error("clipboard.cache.admission_limit"));
        };
        oldest += 1;
        retained_files -= 1;
        retained_bytes = retained_bytes
            .checked_sub(entries[index].bytes)
            .ok_or_else(|| clipboard_error("clipboard.cache.metadata_overflow"))?;
        removals.push(index);
    }
    Ok(removals)
}

struct ClipboardCacheAdmission {
    _process_guard: std::sync::MutexGuard<'static, ()>,
    #[cfg(unix)]
    lock_file: std::fs::File,
}

impl Drop for ClipboardCacheAdmission {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            // SAFETY: lock_file is live for this call and flock accepts its owned descriptor.
            let _ = unsafe { libc::flock(self.lock_file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

impl ClipboardCacheAdmission {
    fn acquire(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)
            .map_err(|_| clipboard_error("clipboard.cache.create_failed"))?;
        static CACHE_MUTEX: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let process_guard = CACHE_MUTEX
            .get_or_init(|| std::sync::Mutex::new(()))
            .try_lock()
            .map_err(|_| clipboard_error("clipboard.cache.busy"))?;

        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            let lock_file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(dir.join(CLIPBOARD_CACHE_LOCK_NAME))
                .map_err(|_| clipboard_error("clipboard.cache.lock_failed"))?;
            // SAFETY: lock_file owns a valid descriptor; nonblocking mode cannot park the host
            // worker. The kernel releases the advisory lock if this process crashes.
            let locked =
                unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
            if !locked {
                return Err(clipboard_error("clipboard.cache.busy"));
            }
            Ok(Self {
                _process_guard: process_guard,
                lock_file,
            })
        }

        #[cfg(not(unix))]
        {
            // Non-Unix builds use a process-local, nonblocking guard. A second process can still
            // race this ephemeral cache, so any inconsistent scan/admission fails closed.
            Ok(Self {
                _process_guard: process_guard,
            })
        }
    }
}

fn record_cache_scan_entry(scanned: &mut usize, limit: usize) -> anyhow::Result<()> {
    *scanned = scanned
        .checked_add(1)
        .ok_or_else(|| clipboard_error("clipboard.cache.scan_limit"))?;
    if *scanned > limit {
        return Err(clipboard_error("clipboard.cache.scan_limit"));
    }
    Ok(())
}

fn scan_clipboard_cache(dir: &Path) -> anyhow::Result<Vec<CacheEntry>> {
    let mut read_dir =
        std::fs::read_dir(dir).map_err(|_| clipboard_error("clipboard.cache.scan_failed"))?;
    let mut scanned = 0usize;
    let mut entries = Vec::new();
    loop {
        let Some(entry) = read_dir.next() else {
            break;
        };
        record_cache_scan_entry(&mut scanned, CACHE_LIMITS.max_scan_entries)?;
        let entry = entry.map_err(|_| clipboard_error("clipboard.cache.scan_failed"))?;
        let file_name = entry.file_name();
        let Some(kind) = owned_cache_kind(&file_name) else {
            continue;
        };
        let file_type = entry
            .file_type()
            .map_err(|_| clipboard_error("clipboard.cache.metadata_failed"))?;
        if !file_type.is_file() {
            return Err(clipboard_error("clipboard.cache.owned_entry_invalid"));
        }
        let metadata = entry
            .metadata()
            .map_err(|_| clipboard_error("clipboard.cache.metadata_failed"))?;
        let modified = metadata
            .modified()
            .map_err(|_| clipboard_error("clipboard.cache.metadata_failed"))?;
        entries.push(CacheEntry {
            file_name,
            bytes: metadata.len(),
            modified,
            kind,
        });
    }
    Ok(entries)
}

fn apply_cache_removals(
    dir: &Path,
    entries: &mut Vec<CacheEntry>,
    removals: Vec<usize>,
) -> anyhow::Result<()> {
    let mut remove = vec![false; entries.len()];
    for index in removals {
        let selected = remove
            .get_mut(index)
            .ok_or_else(|| clipboard_error("clipboard.cache.metadata_overflow"))?;
        *selected = true;
    }
    for (index, entry) in entries.iter().enumerate() {
        if remove[index] {
            std::fs::remove_file(dir.join(&entry.file_name))
                .map_err(|_| clipboard_error("clipboard.cache.gc_failed"))?;
        }
    }
    let mut index = 0usize;
    entries.retain(|_| {
        let keep = !remove[index];
        index += 1;
        keep
    });
    Ok(())
}

fn next_clipboard_image_path() -> PathBuf {
    let dir = crate::paths::cache_dir()
        .map(|cache| cache.join("clipboard-images"))
        .unwrap_or_else(|| {
            std::env::temp_dir()
                .join("deppy-sijo")
                .join("clipboard-images")
        });
    let millis = deppy_core::time::unix_ms();
    dir.join(format!(
        "{CLIPBOARD_IMAGE_PREFIX}{}-{millis}-{}.png",
        std::process::id(),
        uuid::Uuid::new_v4()
    ))
}

fn write_clipboard_cache_file(
    path: &Path,
    write: impl FnOnce(&mut ByteLimitWriter<&mut std::fs::File>) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| clipboard_error("clipboard.cache.invalid_path"))?;
    let _admission = ClipboardCacheAdmission::acquire(parent)?;
    let mut entries = scan_clipboard_cache(parent)?;
    let initial_removals = planned_cache_removals(&entries, SystemTime::now(), 0, 0, CACHE_LIMITS)?;
    apply_cache_removals(parent, &mut entries, initial_removals)?;

    let temporary = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|_| clipboard_error("clipboard.image.temp_create_failed"))?;
    let result = {
        let mut bounded = ByteLimitWriter {
            inner: &mut file,
            written: 0,
            limit: MAX_PNG_BYTES,
        };
        write(&mut bounded).and_then(|()| {
            bounded
                .flush()
                .map_err(|_| clipboard_error("clipboard.image.write_failed"))
        })
    };
    let result = result.and_then(|()| {
        file.flush()
            .map_err(|_| clipboard_error("clipboard.image.write_failed"))
    });
    drop(file);
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }

    let incoming_bytes = match std::fs::metadata(&temporary) {
        Ok(metadata) if metadata.len() > 0 && metadata.len() <= MAX_PNG_BYTES as u64 => {
            metadata.len()
        }
        Ok(_) => {
            let _ = std::fs::remove_file(&temporary);
            return Err(clipboard_error("clipboard.image.png_byte_limit"));
        }
        Err(_) => {
            let _ = std::fs::remove_file(&temporary);
            return Err(clipboard_error("clipboard.image.metadata_failed"));
        }
    };
    let removals = match planned_cache_removals(
        &entries,
        SystemTime::now(),
        1,
        incoming_bytes,
        CACHE_LIMITS,
    ) {
        Ok(removals) => removals,
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
    };
    if let Err(error) = apply_cache_removals(parent, &mut entries, removals) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    if std::fs::rename(&temporary, path).is_err() {
        let _ = std::fs::remove_file(&temporary);
        return Err(clipboard_error("clipboard.image.publish_failed"));
    }
    Ok(())
}

struct ByteLimitWriter<W> {
    inner: W,
    written: usize,
    limit: usize,
}

impl<W: Write> Write for ByteLimitWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let remaining = self.limit.saturating_sub(self.written);
        if bytes.len() > remaining {
            return Err(io::Error::new(io::ErrorKind::FileTooLarge, "byte limit"));
        }
        let written = self.inner.write(bytes)?;
        self.written = self
            .written
            .checked_add(written)
            .ok_or_else(|| io::Error::new(io::ErrorKind::FileTooLarge, "byte limit"))?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn write_rgba_png(path: &Path, width: usize, height: usize, rgba: &[u8]) -> anyhow::Result<()> {
    let expected_bytes = checked_rgba_bytes(width, height)?;
    if rgba.len() != expected_bytes {
        return Err(clipboard_error("clipboard.image.rgba_size_mismatch"));
    }

    write_clipboard_cache_file(path, |file| {
        use image::ImageEncoder;
        let mut writer = io::BufWriter::new(file);
        image::codecs::png::PngEncoder::new_with_quality(
            &mut writer,
            image::codecs::png::CompressionType::Fast,
            image::codecs::png::FilterType::Adaptive,
        )
        .write_image(
            rgba,
            width as u32,
            height as u32,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|_| clipboard_error("clipboard.image.encode_failed"))?;
        writer
            .flush()
            .map_err(|_| clipboard_error("clipboard.image.write_failed"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 전역 캐시 admission을 쓰는 테스트끼리 직렬화한다.
    ///
    /// `ClipboardCacheAdmission::acquire`의 `CACHE_MUTEX`는 프로세스 전역이고
    /// `try_lock`이라, 다른 테스트가 쥐고 있으면 즉시 `clipboard.cache.busy`로 실패한다.
    /// 이건 UI를 막지 않기 위한 정상 제품 동작이므로 제품을 고치지 않고 테스트만 줄 세운다
    /// (실증: `--workspace` 동시 실행에서 3/5 실패, 앱 크레이트 단독으로는 재현 안 됨).
    fn cache_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static CACHE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn test_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "deppy-sijo-clipboard-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn remove_test_dir(dir: &Path) {
        let _ = std::fs::remove_file(dir.join(CLIPBOARD_CACHE_LOCK_NAME));
        std::fs::remove_dir(dir).unwrap();
    }

    fn cache_name(sequence: u64, extension: &str) -> OsString {
        OsString::from(format!(
            "{CLIPBOARD_IMAGE_PREFIX}1-{sequence}-00000000-0000-4000-8000-000000000000.{extension}"
        ))
    }

    fn cache_entry(sequence: u64, bytes: u64, modified: SystemTime) -> CacheEntry {
        CacheEntry {
            file_name: cache_name(sequence, "png"),
            bytes,
            modified,
            kind: OwnedCacheKind::Image,
        }
    }

    #[test]
    fn clipboard_text_admission_accepts_exact_and_rejects_plus_one() {
        assert!(bounded_clipboard_text("x".repeat(MAX_CLIPBOARD_TEXT_BYTES)).is_some());
        assert!(bounded_clipboard_text("x".repeat(MAX_CLIPBOARD_TEXT_BYTES + 1)).is_none());
    }

    #[test]
    fn clipboard_file_admission_accepts_exact_and_rejects_plus_one() {
        let exact_items = vec![PathBuf::from("x"); MAX_CLIPBOARD_FILE_ITEMS];
        assert!(validate_clipboard_paths(&exact_items).is_ok());
        let plus_one_item = vec![PathBuf::from("x"); MAX_CLIPBOARD_FILE_ITEMS + 1];
        assert_eq!(
            validate_clipboard_paths(&plus_one_item)
                .unwrap_err()
                .to_string(),
            "clipboard.file_list.item_limit"
        );
        assert!(
            validate_clipboard_paths(&[PathBuf::from("x".repeat(MAX_CLIPBOARD_PATH_BYTES))])
                .is_ok()
        );
        assert_eq!(
            validate_clipboard_paths(&[PathBuf::from("x".repeat(MAX_CLIPBOARD_PATH_BYTES + 1))])
                .unwrap_err()
                .to_string(),
            "clipboard.file_list.path_limit"
        );

        let exact_bytes = vec![
            PathBuf::from("x".repeat(MAX_CLIPBOARD_PATH_BYTES));
            MAX_CLIPBOARD_FILE_LIST_BYTES / MAX_CLIPBOARD_PATH_BYTES
        ];
        assert_eq!(
            exact_bytes
                .iter()
                .map(|path| path.as_os_str().as_encoded_bytes().len())
                .sum::<usize>(),
            MAX_CLIPBOARD_FILE_LIST_BYTES
        );
        assert!(validate_clipboard_paths(&exact_bytes).is_ok());
        let mut plus_one_byte = exact_bytes;
        plus_one_byte.push(PathBuf::from("x"));
        assert_eq!(
            validate_clipboard_paths(&plus_one_byte)
                .unwrap_err()
                .to_string(),
            "clipboard.file_list.byte_limit"
        );
    }

    #[test]
    fn cache_scan_budget_accepts_exact_and_rejects_plus_one() {
        let mut scanned = 0usize;
        for _ in 0..CLIPBOARD_CACHE_MAX_SCAN_ENTRIES {
            record_cache_scan_entry(&mut scanned, CLIPBOARD_CACHE_MAX_SCAN_ENTRIES).unwrap();
        }
        assert_eq!(scanned, CLIPBOARD_CACHE_MAX_SCAN_ENTRIES);
        assert_eq!(
            record_cache_scan_entry(&mut scanned, CLIPBOARD_CACHE_MAX_SCAN_ENTRIES)
                .unwrap_err()
                .to_string(),
            "clipboard.cache.scan_limit"
        );
    }

    #[test]
    fn cache_admission_accepts_exact_bytes_and_rejects_plus_one() {
        let limits = CacheLimits {
            max_files: 1,
            max_bytes: 10,
            max_scan_entries: 4,
            ttl: Duration::from_secs(60),
        };
        assert!(planned_cache_removals(&[], SystemTime::UNIX_EPOCH, 1, 10, limits).is_ok());
        assert_eq!(
            planned_cache_removals(&[], SystemTime::UNIX_EPOCH, 1, 11, limits)
                .unwrap_err()
                .to_string(),
            "clipboard.cache.admission_limit"
        );
    }

    #[test]
    fn cache_gc_evicts_expired_temporary_and_oldest_to_exact_limits() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let limits = CacheLimits {
            max_files: 2,
            max_bytes: 20,
            max_scan_entries: 8,
            ttl: Duration::from_secs(100),
        };
        let mut entries = vec![
            cache_entry(1, 8, now - Duration::from_secs(200)),
            cache_entry(2, 8, now - Duration::from_secs(30)),
            cache_entry(3, 8, now - Duration::from_secs(20)),
            cache_entry(4, 8, now - Duration::from_secs(10)),
        ];
        entries.push(CacheEntry {
            file_name: cache_name(5, "tmp"),
            bytes: 1,
            modified: now,
            kind: OwnedCacheKind::Temporary,
        });
        let removals = planned_cache_removals(&entries, now, 1, 4, limits).unwrap();
        assert_eq!(removals, vec![0, 4, 1, 2]);
        let retained: Vec<_> = entries
            .iter()
            .enumerate()
            .filter(|(index, _)| !removals.contains(index))
            .collect();
        assert_eq!(retained.len() + 1, limits.max_files);
        assert_eq!(
            retained.iter().map(|(_, entry)| entry.bytes).sum::<u64>() + 4,
            12
        );
    }

    #[test]
    fn cache_gc_deletes_only_valid_app_owned_names() {
        let _cache_guard = cache_test_guard();
        let dir = test_dir("cache-owned-gc");
        let owned = dir.join(cache_name(1, "tmp"));
        let unrelated = dir.join("clipboard-image-not-owned.png");
        std::fs::write(&owned, [0u8; 8]).unwrap();
        std::fs::write(&unrelated, [0u8; 8]).unwrap();
        let admission = ClipboardCacheAdmission::acquire(&dir).unwrap();
        let mut entries = scan_clipboard_cache(&dir).unwrap();
        let removals =
            planned_cache_removals(&entries, SystemTime::now(), 0, 0, CACHE_LIMITS).unwrap();
        apply_cache_removals(&dir, &mut entries, removals).unwrap();
        assert!(!owned.exists());
        assert!(unrelated.exists());
        drop(admission);
        std::fs::remove_file(unrelated).unwrap();
        remove_test_dir(&dir);
    }

    #[test]
    fn cache_lock_releases_and_can_be_reacquired() {
        let _cache_guard = cache_test_guard();
        let dir = test_dir("cache-lock");
        let first = ClipboardCacheAdmission::acquire(&dir).unwrap();
        let second_error = match ClipboardCacheAdmission::acquire(&dir) {
            Ok(_) => panic!("second cache lock unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(second_error.to_string(), "clipboard.cache.busy");
        drop(first);
        drop(ClipboardCacheAdmission::acquire(&dir).unwrap());
        remove_test_dir(&dir);
    }

    #[test]
    fn byte_limit_writer_accepts_exact_and_rejects_plus_one() {
        let mut exact = ByteLimitWriter {
            inner: Vec::new(),
            written: 0,
            limit: 10,
        };
        exact.write_all(&[0; 10]).unwrap();
        assert_eq!(exact.written, 10);

        let mut plus_one = ByteLimitWriter {
            inner: Vec::new(),
            written: 0,
            limit: 10,
        };
        assert_eq!(
            plus_one.write_all(&[0; 11]).unwrap_err().kind(),
            io::ErrorKind::FileTooLarge
        );
        assert_eq!(plus_one.written, 0);
    }

    #[test]
    fn small_actual_file_does_not_over_evict_cache() {
        let _cache_guard = cache_test_guard();
        let dir = test_dir("cache-small-admission");
        let existing = dir.join(cache_name(1, "png"));
        let next = dir.join(cache_name(2, "png"));
        std::fs::write(&existing, [0u8; 8]).unwrap();
        write_clipboard_cache_file(&next, |file| {
            file.write_all(&[1u8; 4])
                .map_err(|_| clipboard_error("clipboard.image.test_failure"))
        })
        .unwrap();
        assert!(existing.exists());
        assert_eq!(std::fs::metadata(&next).unwrap().len(), 4);
        std::fs::remove_file(existing).unwrap();
        std::fs::remove_file(next).unwrap();
        remove_test_dir(&dir);
    }

    #[test]
    fn atomic_writer_cleans_temporary_file_on_error() {
        let _cache_guard = cache_test_guard();
        let dir = test_dir("atomic-error");
        let path = dir.join(cache_name(1, "png"));
        let error = write_clipboard_cache_file(&path, |file| {
            file.write_all(b"partial").unwrap();
            Err(clipboard_error("clipboard.image.test_failure"))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "clipboard.image.test_failure");
        assert!(!path.exists());
        assert!(!path.with_extension("tmp").exists());
        remove_test_dir(&dir);
    }

    #[test]
    fn rgba_png_writer_rejects_wrong_buffer_size() {
        let _cache_guard = cache_test_guard();
        let path = std::env::temp_dir().join(format!(
            "deppy-sijo-bad-clipboard-image-{}.png",
            std::process::id()
        ));
        let err = write_rgba_png(&path, 2, 2, &[0, 0, 0, 255]).unwrap_err();
        assert_eq!(err.to_string(), "clipboard.image.rgba_size_mismatch");
    }

    /// Manual hardware-only pasteboard round-trip; it mutates the user's clipboard.
    #[test]
    #[ignore = "mutates the real clipboard"]
    fn pasteboard_file_url_round_trip() {
        let path = std::env::temp_dir().join(format!(
            "deppy-sijo-pasteboard-roundtrip-{}.txt",
            std::process::id()
        ));
        std::fs::write(&path, b"roundtrip").unwrap();
        let canonical = path.canonicalize().unwrap();
        copy_file_urls_to_clipboard(std::slice::from_ref(&canonical)).unwrap();
        let listed = read_clipboard_file_list().expect("pasteboard file list");
        assert!(listed.iter().any(|candidate| {
            candidate
                .canonicalize()
                .map(|resolved| resolved == canonical)
                .unwrap_or(candidate == &canonical)
        }));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rgba_png_writer_creates_png_file_atomically() {
        let _cache_guard = cache_test_guard();
        let dir = test_dir("png-writer");
        let path = dir.join(cache_name(1, "png"));
        let rgba = [255, 0, 0, 255, 0, 255, 0, 255];
        write_rgba_png(&path, 2, 1, &rgba).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(!path.with_extension("tmp").exists());
        std::fs::remove_file(path).unwrap();
        remove_test_dir(&dir);
    }
}
