//! Bounded PNG work executed by the App-owned image lane, never by rendering.
use std::collections::HashSet;
use std::fs::{File, Metadata};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) const MAX_IMAGE_ENCODED_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) const MAX_IMAGE_DIMENSION_PX: u32 = 6000;
pub(crate) const MAX_IMAGE_PIXELS: u64 = 16_000_000;
pub(crate) const MAX_DOCUMENT_ENCODED_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_DOCUMENT_PIXELS: u64 = 8_000_000;
pub(crate) const MAX_DOCUMENT_IMAGES: usize = 32;
pub(crate) const MAX_REQUEST_PATH_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageRejection {
    AbsolutePath,
    WrongExtension,
    OutsideWorkspace,
    NotARegularFile,
    TooLarge,
    ReadFailed,
    DecodeRejected,
    Changed,
}

#[derive(Clone, PartialEq, Eq)]
struct FileStamp {
    target: PathBuf,
    len: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    changed: (i64, i64),
    #[cfg(windows)]
    windows_identity: (u32, u64, i64),
}

impl FileStamp {
    fn new(target: PathBuf, metadata: &Metadata, file: &File) -> Result<Self, ImageRejection> {
        #[cfg(not(windows))]
        let _ = file;
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        Ok(Self {
            target,
            len: metadata.len(),
            modified: metadata
                .modified()
                .map_err(|_| ImageRejection::ReadFailed)?,
            #[cfg(unix)]
            dev: metadata.dev(),
            #[cfg(unix)]
            ino: metadata.ino(),
            #[cfg(unix)]
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            #[cfg(windows)]
            windows_identity: windows_identity(file)?,
        })
    }
}

pub(crate) struct LoadedImage {
    stamp: FileStamp,
    pub(crate) encoded: Arc<[u8]>,
    pub(crate) decoded: Arc<egui::ColorImage>,
    pub(crate) pixels: u64,
}

pub(crate) struct ImageRequest {
    pub(crate) token: u64,
    pub(crate) slot: u64,
    pub(crate) revision: u64,
    pub(crate) root: PathBuf,
    pub(crate) base: PathBuf,
    pub(crate) refs: Vec<String>,
    pub(crate) previous: Vec<(String, Arc<LoadedImage>)>,
}

pub(crate) struct ImageOutcome {
    pub(crate) token: u64,
    pub(crate) slot: u64,
    pub(crate) revision: u64,
    pub(crate) images: Vec<(String, Arc<LoadedImage>)>,
}

pub(crate) type ImageWorker = crate::lazy_worker::LazyBoundedWorker<ImageRequest, ImageOutcome>;

pub(crate) fn new_worker(ctx: egui::Context) -> ImageWorker {
    ImageWorker::new(
        "markdown-images",
        std::time::Duration::from_secs(30),
        || load_images,
        move || ctx.request_repaint(),
    )
}

/// A single outstanding request/result,32 references,16MiB encoded and8million
/// decoded pixels. Reused images spend the same budget as newly read images.
/// The old display and new result may coexist, but at most two bounded sets.
pub(crate) fn load_images(request: ImageRequest) -> ImageOutcome {
    let mut outcome = ImageOutcome {
        token: request.token,
        slot: request.slot,
        revision: request.revision,
        images: Vec::new(),
    };
    if request.refs.len() > MAX_DOCUMENT_IMAGES
        || request.previous.len() > MAX_DOCUMENT_IMAGES
        || request.refs.iter().map(String::len).sum::<usize>() > MAX_REQUEST_PATH_BYTES
        || request.root.as_os_str().len() + request.base.as_os_str().len() > 8192
    {
        return outcome;
    }
    let mut seen = HashSet::new();
    let mut encoded = 0_u64;
    let mut pixels = 0_u64;
    for relative in &request.refs {
        if !seen.insert(relative) {
            continue;
        }
        let Ok((mut file, stamp)) = open_validated(&request.root, &request.base, relative) else {
            continue;
        };
        let remaining_bytes = MAX_DOCUMENT_ENCODED_BYTES - encoded;
        let remaining_pixels = MAX_DOCUMENT_PIXELS - pixels;
        let previous = request
            .previous
            .iter()
            .find(|(name, image)| name == relative && image.stamp == stamp);
        let image = if let Some((_, image)) = previous {
            if image.encoded.len() as u64 > remaining_bytes || image.pixels > remaining_pixels {
                continue;
            }
            image.clone()
        } else {
            let Ok(image) = read_image(&mut file, stamp, remaining_bytes, remaining_pixels, || {})
            else {
                continue;
            };
            Arc::new(image)
        };
        encoded += image.encoded.len() as u64;
        pixels += image.pixels;
        outcome.images.push((relative.clone(), image));
    }
    outcome
}

fn open_validated(
    root: &Path,
    base: &Path,
    relative: &str,
) -> Result<(File, FileStamp), ImageRejection> {
    let candidate = Path::new(relative);
    if candidate.is_absolute() {
        return Err(ImageRejection::AbsolutePath);
    }
    if !candidate
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
    {
        return Err(ImageRejection::WrongExtension);
    }
    let root = root
        .canonicalize()
        .map_err(|_| ImageRejection::OutsideWorkspace)?;
    let target = base
        .join(candidate)
        .canonicalize()
        .map_err(|_| ImageRejection::OutsideWorkspace)?;
    let inside = target
        .strip_prefix(&root)
        .map_err(|_| ImageRejection::OutsideWorkspace)?;
    // Open below the root descriptor, refusing symlinks on every canonical
    // component. A swapped parent cannot redirect the validated read outside.
    let file = open_below_root(&root, inside)?;
    let metadata = file.metadata().map_err(|_| ImageRejection::ReadFailed)?;
    if !metadata.is_file() {
        return Err(ImageRejection::NotARegularFile);
    }
    if metadata.len() > MAX_IMAGE_ENCODED_BYTES {
        return Err(ImageRejection::TooLarge);
    }
    let stamp = FileStamp::new(target, &metadata, &file)?;
    Ok((file, stamp))
}

#[cfg(unix)]
fn open_below_root(root: &Path, inside: &Path) -> Result<File, ImageRejection> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
    let mut folder = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)
        .map_err(|_| ImageRejection::OutsideWorkspace)?;
    let opened_root = folder
        .metadata()
        .map_err(|_| ImageRejection::OutsideWorkspace)?;
    let current_root = std::fs::metadata(root).map_err(|_| ImageRejection::OutsideWorkspace)?;
    if root
        .canonicalize()
        .map_err(|_| ImageRejection::OutsideWorkspace)?
        != root
        || opened_root.dev() != current_root.dev()
        || opened_root.ino() != current_root.ino()
    {
        return Err(ImageRejection::OutsideWorkspace);
    }
    let mut components = inside.components().peekable();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(name) = component else {
            return Err(ImageRejection::OutsideWorkspace);
        };
        let name = std::ffi::CString::new(name.as_bytes())
            .map_err(|_| ImageRejection::OutsideWorkspace)?;
        let is_last = components.peek().is_none();
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if is_last {
                libc::O_NONBLOCK
            } else {
                libc::O_DIRECTORY
            };
        // SAFETY: owned folder descriptor is live, CString is NUL-terminated,
        // no creation flags are used, and each returned descriptor gets1owner.
        let fd = unsafe { libc::openat(folder.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(ImageRejection::OutsideWorkspace);
        }
        // SAFETY: openat returned a fresh owned descriptor, consumed exactly once.
        let opened = unsafe { File::from_raw_fd(fd) };
        if is_last {
            return Ok(opened);
        }
        folder = opened;
    }
    Err(ImageRejection::NotARegularFile)
}

#[cfg(windows)]
fn open_below_root(root: &Path, inside: &Path) -> Result<File, ImageRejection> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    let folder = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(root)
        .map_err(|_| ImageRejection::OutsideWorkspace)?;
    let file = File::open(root.join(inside)).map_err(|_| ImageRejection::ReadFailed)?;
    // Resolve the live handles, not a path re-opened after validation. Windows
    // canonicalize and GetFinalPathNameByHandle use the same extended DOS form.
    let opened_root = windows_final_path(&folder)?;
    let opened_target = windows_final_path(&file)?;
    if opened_root != root || !opened_target.starts_with(&opened_root) {
        return Err(ImageRejection::OutsideWorkspace);
    }
    Ok(file)
}

#[cfg(windows)]
fn windows_final_path(file: &File) -> Result<PathBuf, ImageRejection> {
    use std::os::windows::{ffi::OsStringExt as _, io::AsRawHandle as _};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW,
    };
    let mut buffer = vec![0_u16; 32768];
    // SAFETY: file owns a live handle and buffer is writable for its supplied length.
    let len = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            FILE_NAME_NORMALIZED,
        )
    } as usize;
    if len == 0 || len >= buffer.len() {
        return Err(ImageRejection::OutsideWorkspace);
    }
    Ok(std::ffi::OsString::from_wide(&buffer[..len]).into())
}

#[cfg(windows)]
fn windows_identity(file: &File) -> Result<(u32, u64, i64), ImageRejection> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandle,
        GetFileInformationByHandleEx,
    };
    // SAFETY: both structs are plain Windows output structures, zero initialized
    // and writable for exactly their ABI size while the owned handle stays live.
    let mut identity: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut identity) };
    let basic_ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            std::ptr::from_mut(&mut basic).cast(),
            std::mem::size_of_val(&basic) as u32,
        )
    };
    if ok == 0 || basic_ok == 0 {
        return Err(ImageRejection::ReadFailed);
    }
    Ok((
        identity.dwVolumeSerialNumber,
        (u64::from(identity.nFileIndexHigh) << 32) | u64::from(identity.nFileIndexLow),
        basic.ChangeTime,
    ))
}

#[cfg(not(any(unix, windows)))]
fn open_below_root(_: &Path, _: &Path) -> Result<File, ImageRejection> {
    Err(ImageRejection::OutsideWorkspace)
}

fn read_image(
    file: &mut File,
    stamp: FileStamp,
    remaining_bytes: u64,
    remaining_pixels: u64,
    after_stat: impl FnOnce(),
) -> Result<LoadedImage, ImageRejection> {
    after_stat();
    let limit = MAX_IMAGE_ENCODED_BYTES.min(remaining_bytes);
    if stamp.len > limit {
        return Err(ImageRejection::TooLarge);
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ImageRejection::ReadFailed)?;
    if bytes.len() as u64 > limit {
        return Err(ImageRejection::TooLarge);
    }
    let after = FileStamp::new(
        stamp.target.clone(),
        &file.metadata().map_err(|_| ImageRejection::ReadFailed)?,
        file,
    )?;
    if stamp != after || bytes.len() as u64 != stamp.len {
        return Err(ImageRejection::Changed);
    }
    let dimensions =
        image::ImageReader::with_format(std::io::Cursor::new(&bytes), image::ImageFormat::Png)
            .into_dimensions()
            .map_err(|_| ImageRejection::DecodeRejected)?;
    let (width, height) = dimensions;
    let pixels = u64::from(width) * u64::from(height);
    if width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION_PX
        || height > MAX_IMAGE_DIMENSION_PX
        || pixels > MAX_IMAGE_PIXELS
        || pixels > remaining_pixels
    {
        return Err(ImageRejection::DecodeRejected);
    }
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(&bytes), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION_PX);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION_PX);
    limits.max_alloc = Some(MAX_DOCUMENT_PIXELS * 8);
    reader.limits(limits);
    let rgba = reader
        .decode()
        .map_err(|_| ImageRejection::DecodeRejected)?
        .into_rgba8();
    let decoded =
        egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], rgba.as_raw());
    Ok(LoadedImage {
        stamp,
        encoded: bytes.into(),
        decoded: Arc::new(decoded),
        pixels,
    })
}

#[cfg(test)]
pub(crate) fn validate_and_read_with_hook(
    root: &Path,
    base: &Path,
    relative: &str,
    hook: impl FnOnce(),
) -> Result<Vec<u8>, ImageRejection> {
    let (mut file, stamp) = open_validated(root, base, relative)?;
    read_image(
        &mut file,
        stamp,
        MAX_IMAGE_ENCODED_BYTES,
        MAX_IMAGE_PIXELS,
        hook,
    )
    .map(|image| image.encoded.to_vec())
}
