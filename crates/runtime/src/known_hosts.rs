//! remote TLS known_hosts (단계 C-3 잔여, 설계doc §2.2 · Open Question 3/5).
//!
//! SSH known_hosts와 같은 TOFU 저장소: 클라이언트가 attach한 서버(host = "ip:port")별로
//! 신뢰한 인증서 지문을 기억한다. 포맷은 한 줄에 `host 지문` (공백 구분, `#` 주석 허용) —
//! 사용자가 열어 보고 손으로 지울 수 있는 단순 텍스트. 저장은 tmp+rename 원자.
//!
//! 정책(호출측 UX가 소비):
//!   - FirstUse: 항목 없음 — TOFU로 핀 가능. **최초 접속은 무검증 창**(SSH와 동일 한계,
//!     설계 §6) — 대역외 지문 대조를 사용자에게 명시 요구하는 것은 앱 UI 소관.
//!   - Match: 저장된 지문과 일치 — 진행.
//!   - Mismatch: 다른 지문 — **거부**가 기본값(서버 교체 또는 MITM). 사용자가 의도한
//!     변경이면 `forget` 후 재-TOFU.
//!
//! 저장 temp는 process-id + 단조 counter로 충돌을 피하고 모든 정상/오류 반환 경로에서
//! 정리하므로 건강한 장기 실행에서는 누적되지 않는다. 프로세스가 write와 rename 사이에
//! 강제 종료되면 temp 하나가 남을 수 있다. GUI/proxy의 동시 writer 및 PID 재사용과 경쟁하지
//! 않는 portable liveness 판정이 없으므로 자동 orphan 삭제는 의도적으로 하지 않는다.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// 디스크에서 읽는 known_hosts 파일의 최대 크기.
pub const MAX_KNOWN_HOSTS_FILE_BYTES: usize = 1024 * 1024;
/// 한 저장소가 보유할 수 있는 고유 host 수.
pub const MAX_KNOWN_HOSTS_ENTRIES: usize = 4_096;
/// host 필드의 UTF-8 byte 상한.
pub const MAX_KNOWN_HOST_BYTES: usize = 1024;
/// fingerprint 필드의 UTF-8 byte 상한.
pub const MAX_KNOWN_HOST_FINGERPRINT_BYTES: usize = 512;
/// 모든 host/fingerprint 문자열이 차지할 수 있는 총 byte 상한.
pub const MAX_KNOWN_HOSTS_RETAINED_BYTES: usize = 512 * 1024;

const FILE_HEADER: &str = "# deppy remote TLS known_hosts — host 지문(SHA-256)\n";
const REDACTED: &str = "[REDACTED]";
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// check 결과 — 호출측(attach/TOFU UX)이 분기한다.
#[derive(Clone, PartialEq, Eq)]
pub enum TofuDecision {
    /// 이 host의 항목이 없다 — 최초 접속(TOFU 핀 대상).
    FirstUse,
    /// 저장된 지문과 일치.
    Match,
    /// 저장된 지문과 다르다 — 서버 교체/MITM 가능성. stored는 기존 핀.
    Mismatch { stored: String },
}

impl fmt::Debug for TofuDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FirstUse => f.write_str("FirstUse"),
            Self::Match => f.write_str("Match"),
            Self::Mismatch { .. } => f
                .debug_struct("Mismatch")
                .field("stored", &REDACTED)
                .finish(),
        }
    }
}

/// host("ip:port") → 지문("ab:cd:…", 소문자) 저장소.
pub struct KnownHosts {
    path: PathBuf,
    entries: BTreeMap<String, String>,
    retained_bytes: usize,
}

impl fmt::Debug for KnownHosts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KnownHosts")
            .field("path", &REDACTED)
            .field("entry_count", &self.entries.len())
            .field("retained_bytes", &self.retained_bytes)
            .finish()
    }
}

impl KnownHosts {
    /// 파일에서 로드한다. 파일이 없으면 빈 저장소(첫 사용). 형식이 깨진 개별 라인은
    /// 한 번의 정적 경고만 남기고 스킵한다. 크기/개수 한도를 넘거나 UTF-8이 아니면
    /// 신뢰 기록을 부분 적용하지 않고 전체 로드를 실패시킨다.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let Some(bytes) = read_bounded(path)? else {
            return Ok(Self {
                path: path.to_path_buf(),
                entries: BTreeMap::new(),
                retained_bytes: 0,
            });
        };
        let text =
            std::str::from_utf8(&bytes).map_err(|_| static_error("known_hosts_invalid_utf8"))?;
        let mut entries: BTreeMap<String, String> = BTreeMap::new();
        let mut retained_bytes = 0usize;
        let mut ignored_malformed = false;

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (Some(host), Some(fingerprint)) = (parts.next(), parts.next()) else {
                ignored_malformed = true;
                continue;
            };
            validate_host(host)?;
            validate_fingerprint(fingerprint)?;
            let normalized = fingerprint.to_ascii_lowercase();

            if let Some(previous) = entries.get_mut(host) {
                retained_bytes = retained_bytes
                    .checked_sub(previous.len())
                    .and_then(|bytes| bytes.checked_add(normalized.len()))
                    .ok_or_else(|| static_error("known_hosts_retained_bytes_exceeded"))?;
                if retained_bytes > MAX_KNOWN_HOSTS_RETAINED_BYTES {
                    return Err(static_error("known_hosts_retained_bytes_exceeded"));
                }
                *previous = normalized;
                continue;
            }

            if entries.len() == MAX_KNOWN_HOSTS_ENTRIES {
                return Err(static_error("known_hosts_entry_limit_exceeded"));
            }
            retained_bytes = retained_bytes
                .checked_add(host.len())
                .and_then(|bytes| bytes.checked_add(normalized.len()))
                .ok_or_else(|| static_error("known_hosts_retained_bytes_exceeded"))?;
            if retained_bytes > MAX_KNOWN_HOSTS_RETAINED_BYTES {
                return Err(static_error("known_hosts_retained_bytes_exceeded"));
            }
            entries.insert(host.to_owned(), normalized);
        }

        if ignored_malformed {
            tracing::warn!(
                kind = "known_hosts",
                error_code = "malformed_entry",
                "known_hosts entry ignored"
            );
        }
        Ok(Self {
            path: path.to_path_buf(),
            entries,
            retained_bytes,
        })
    }

    /// 저장된 지문 (소문자 정규화).
    pub fn lookup(&self, host: &str) -> Option<&str> {
        self.entries.get(host).map(String::as_str)
    }

    /// 표시 등 read-only projection이 저장소와 동일한 effective view를 쓰게 한다.
    /// 순서는 host 사전순이며 항목 수와 각 필드 크기는 위 상한으로 제한되어 있다.
    pub fn entries(&self) -> impl ExactSizeIterator<Item = (&str, &str)> {
        self.entries
            .iter()
            .map(|(host, fingerprint)| (host.as_str(), fingerprint.as_str()))
    }

    /// 저장소를 소비해 정렬된 소유 projection으로 옮긴다. 호출측 캐시에 넣을 때
    /// host/fingerprint 문자열을 다시 clone하지 않는다.
    pub fn into_entries(self) -> impl ExactSizeIterator<Item = (String, String)> {
        self.entries.into_iter()
    }

    /// 관찰된 지문을 저장 기록과 대조한다.
    pub fn check(&self, host: &str, fingerprint: &str) -> TofuDecision {
        match self.entries.get(host) {
            None => TofuDecision::FirstUse,
            Some(stored) if stored.eq_ignore_ascii_case(fingerprint) => TofuDecision::Match,
            Some(stored) => TofuDecision::Mismatch {
                stored: stored.clone(),
            },
        }
    }

    /// host의 지문을 핀하고 즉시 파일에 반영한다 (tmp+rename 원자).
    /// 저장 실패 시 in-memory 변경을 **롤백**한다 — 파일과 메모리가 갈라져 재시도가
    /// 영속 없이 Verified로 통과하는 것 방지 (codex P3).
    pub fn pin(&mut self, host: &str, fingerprint: &str) -> anyhow::Result<()> {
        validate_host(host)?;
        validate_fingerprint(fingerprint)?;
        let normalized = fingerprint.to_ascii_lowercase();
        let previous_len = self.entries.get(host).map_or(0, String::len);
        if previous_len == 0 && self.entries.len() == MAX_KNOWN_HOSTS_ENTRIES {
            return Err(static_error("known_hosts_entry_limit_exceeded"));
        }
        let next_retained = self
            .retained_bytes
            .checked_sub(previous_len)
            .and_then(|bytes| {
                if previous_len == 0 {
                    bytes.checked_add(host.len())
                } else {
                    Some(bytes)
                }
            })
            .and_then(|bytes| bytes.checked_add(normalized.len()))
            .ok_or_else(|| static_error("known_hosts_retained_bytes_exceeded"))?;
        if next_retained > MAX_KNOWN_HOSTS_RETAINED_BYTES {
            return Err(static_error("known_hosts_retained_bytes_exceeded"));
        }

        let previous = self.entries.insert(host.to_owned(), normalized);
        let previous_retained = self.retained_bytes;
        self.retained_bytes = next_retained;
        if let Err(error) = self.save() {
            match previous {
                Some(value) => {
                    self.entries.insert(host.to_owned(), value);
                }
                None => {
                    self.entries.remove(host);
                }
            }
            self.retained_bytes = previous_retained;
            return Err(error);
        }
        Ok(())
    }

    /// host 항목을 제거한다 (지문 변경 시 재-TOFU 경로). 없는 host는 no-op.
    /// 저장 실패 시 롤백 — pin과 동일 원칙.
    pub fn forget(&mut self, host: &str) -> anyhow::Result<()> {
        validate_host(host)?;
        if let Some(previous) = self.entries.remove(host) {
            let previous_retained = self.retained_bytes;
            self.retained_bytes -= host.len() + previous.len();
            if let Err(error) = self.save() {
                self.entries.insert(host.to_owned(), previous);
                self.retained_bytes = previous_retained;
                return Err(error);
            }
        }
        Ok(())
    }

    fn save(&self) -> anyhow::Result<()> {
        let serialized_bytes = FILE_HEADER
            .len()
            .checked_add(self.retained_bytes)
            .and_then(|bytes| bytes.checked_add(self.entries.len() * 2))
            .ok_or_else(|| static_error("known_hosts_file_size_exceeded"))?;
        if serialized_bytes > MAX_KNOWN_HOSTS_FILE_BYTES {
            return Err(static_error("known_hosts_file_size_exceeded"));
        }
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .map_err(|_| static_error("known_hosts_create_dir_failed"))?;
        }

        let temp_path = temp_path_for(&self.path)?;
        write_temp_file(&temp_path, &self.entries)?;
        if std::fs::rename(&temp_path, &self.path).is_err() {
            let _ = std::fs::remove_file(&temp_path);
            return Err(static_error("known_hosts_atomic_replace_failed"));
        }
        // rename은 이미 관찰 가능한 commit이다. parent sync는 durability를 강화하는
        // best-effort이고, 실패를 반환해 caller가 memory만 rollback하면 오히려 disk와
        // 갈라지므로 commit 이후 오류로 승격하지 않는다.
        sync_parent_after_rename(&self.path);
        Ok(())
    }
}

fn read_bounded(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    read_bounded_with_hook(path, || {})
}

fn read_bounded_with_hook(
    path: &Path,
    after_admission: impl FnOnce(),
) -> anyhow::Result<Option<Vec<u8>>> {
    let Some(mut file) = open_known_hosts_nofollow(path)? else {
        return Ok(None);
    };
    let before = file
        .metadata()
        .map_err(|_| static_error("known_hosts_metadata_failed"))?;
    if !is_stable_regular_file(&before) {
        return Err(static_error("known_hosts_not_regular"));
    }
    let declared_len = usize::try_from(before.len())
        .map_err(|_| static_error("known_hosts_file_size_exceeded"))?;
    if declared_len > MAX_KNOWN_HOSTS_FILE_BYTES {
        return Err(static_error("known_hosts_file_size_exceeded"));
    }

    after_admission();

    let mut bytes = vec![0u8; declared_len];
    file.read_exact(&mut bytes)
        .map_err(|_| static_error("known_hosts_changed_during_read"))?;
    let mut plus_one = [0u8; 1];
    let extra = file
        .read(&mut plus_one)
        .map_err(|_| static_error("known_hosts_read_failed"))?;
    if extra != 0 {
        return Err(static_error("known_hosts_changed_during_read"));
    }

    let after = file
        .metadata()
        .map_err(|_| static_error("known_hosts_metadata_failed"))?;
    let path_after = std::fs::symlink_metadata(path)
        .map_err(|_| static_error("known_hosts_replaced_during_read"))?;
    if !stable_metadata_matches(&before, &after) || !stable_metadata_matches(&after, &path_after) {
        return Err(static_error("known_hosts_replaced_during_read"));
    }
    Ok(Some(bytes))
}

fn open_known_hosts_nofollow(path: &Path) -> anyhow::Result<Option<File>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !is_stable_regular_file(&metadata) => {
            return Err(static_error("known_hosts_not_regular"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(static_error("known_hosts_metadata_failed")),
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    match options.open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => match std::fs::symlink_metadata(path) {
            Ok(metadata) if !is_stable_regular_file(&metadata) => {
                Err(static_error("known_hosts_not_regular"))
            }
            _ => Err(static_error("known_hosts_open_failed")),
        },
    }
}

fn is_stable_regular_file(metadata: &std::fs::Metadata) -> bool {
    if !metadata.file_type().is_file() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
}

#[cfg(unix)]
fn stable_metadata_matches(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    is_stable_regular_file(left)
        && is_stable_regular_file(right)
        && left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(windows)]
fn stable_metadata_matches(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    is_stable_regular_file(left)
        && is_stable_regular_file(right)
        && left.file_size() == right.file_size()
        && left.creation_time() == right.creation_time()
        && left.last_write_time() == right.last_write_time()
        && left.file_attributes() == right.file_attributes()
}

#[cfg(not(any(unix, windows)))]
fn stable_metadata_matches(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    is_stable_regular_file(left)
        && is_stable_regular_file(right)
        && left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
}

fn validate_host(host: &str) -> anyhow::Result<()> {
    validate_field(
        host,
        MAX_KNOWN_HOST_BYTES,
        "known_hosts_host_invalid",
        "known_hosts_host_size_exceeded",
    )
}

fn validate_fingerprint(fingerprint: &str) -> anyhow::Result<()> {
    validate_field(
        fingerprint,
        MAX_KNOWN_HOST_FINGERPRINT_BYTES,
        "known_hosts_fingerprint_invalid",
        "known_hosts_fingerprint_size_exceeded",
    )
}

fn validate_field(
    value: &str,
    max_bytes: usize,
    invalid_code: &'static str,
    too_large_code: &'static str,
) -> anyhow::Result<()> {
    if value.is_empty()
        || value.chars().any(char::is_control)
        || value.contains(char::is_whitespace)
    {
        return Err(static_error(invalid_code));
    }
    if value.len() > max_bytes {
        return Err(static_error(too_large_code));
    }
    Ok(())
}

fn temp_path_for(path: &Path) -> anyhow::Result<PathBuf> {
    let Some(file_name) = path.file_name() else {
        return Err(static_error("known_hosts_target_invalid"));
    };
    let id = NEXT_TEMP_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .map_err(|_| static_error("known_hosts_temp_id_exhausted"))?;
    let mut temp_name = file_name.to_os_string();
    temp_name.push(format!(".deppytmp.{}.{id}", std::process::id()));
    Ok(path.with_file_name(temp_name))
}

#[cfg(unix)]
fn sync_parent_after_rename(path: &Path) {
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC);
    if let Ok(directory) = options.open(parent) {
        let _ = directory.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_parent_after_rename(_path: &Path) {}

fn write_temp_file(path: &Path, entries: &BTreeMap<String, String>) -> anyhow::Result<()> {
    write_temp_file_with(path, entries, write_temp_contents)
}

fn write_temp_file_with(
    path: &Path,
    entries: &BTreeMap<String, String>,
    operation: impl FnOnce(File, &BTreeMap<String, String>) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|_| static_error("known_hosts_temp_create_failed"))?;
    let result = operation(file, entries);
    if result.is_err() {
        // `create_new`가 성공한 뒤의 실패만 이 함수가 정리한다. Open 자체가 충돌한
        // 경우에는 여기 도달하지 않아 다른 writer/crash orphan을 지우지 않는다.
        let _ = std::fs::remove_file(path);
    }
    result
}

fn write_temp_contents(file: File, entries: &BTreeMap<String, String>) -> anyhow::Result<()> {
    let mut writer = BufWriter::new(file);
    writer
        .write_all(FILE_HEADER.as_bytes())
        .map_err(|_| static_error("known_hosts_write_failed"))?;
    for (host, fingerprint) in entries {
        writer
            .write_all(host.as_bytes())
            .and_then(|()| writer.write_all(b" "))
            .and_then(|()| writer.write_all(fingerprint.as_bytes()))
            .and_then(|()| writer.write_all(b"\n"))
            .map_err(|_| static_error("known_hosts_write_failed"))?;
    }
    writer
        .flush()
        .map_err(|_| static_error("known_hosts_write_failed"))?;
    writer
        .get_ref()
        .sync_all()
        .map_err(|_| static_error("known_hosts_sync_failed"))?;
    Ok(())
}

fn static_error(code: &'static str) -> anyhow::Error {
    anyhow::Error::msg(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("deppy-kh-{}-{tag}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn temp_path(tag: &str) -> PathBuf {
        temp_dir(tag).join("known_hosts")
    }

    fn remove_parent(path: &Path) {
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    fn fixed_width_host(index: usize, width: usize) -> String {
        let prefix = format!("h{index:04}-");
        format!("{prefix}{}", "x".repeat(width - prefix.len()))
    }

    fn aggregate_fixture(extra_byte: bool) -> String {
        let mut text = String::with_capacity(MAX_KNOWN_HOSTS_RETAINED_BYTES + 16_384);
        for index in 0..MAX_KNOWN_HOSTS_ENTRIES {
            let host = fixed_width_host(index, 64);
            let fingerprint_len = 64 + usize::from(extra_byte && index == 0);
            text.push_str(&host);
            text.push(' ');
            text.push_str(&"a".repeat(fingerprint_len));
            text.push('\n');
        }
        text
    }

    #[test]
    fn 없는_파일은_빈_저장소이고_first_use() {
        let path = temp_path("empty");
        let kh = KnownHosts::load(&path).unwrap();
        assert_eq!(kh.check("127.0.0.1:7777", "aa:bb"), TofuDecision::FirstUse);
        remove_parent(&path);
    }

    #[test]
    fn pin은_저장되고_재로드_후_match_불일치는_mismatch() {
        let path = temp_path("roundtrip");
        let mut kh = KnownHosts::load(&path).unwrap();
        kh.pin("127.0.0.1:7777", "AA:BB:CC").unwrap();
        let kh2 = KnownHosts::load(&path).unwrap();
        assert_eq!(kh2.check("127.0.0.1:7777", "aa:bb:cc"), TofuDecision::Match);
        assert_eq!(
            kh2.check("127.0.0.1:7777", "dd:ee:ff"),
            TofuDecision::Mismatch {
                stored: "aa:bb:cc".to_owned()
            }
        );
        assert!(!format!("{kh2:?}").contains("127.0.0.1"));
        assert!(!format!("{:?}", kh2.check("127.0.0.1:7777", "dd")).contains("aa:bb"));
        remove_parent(&path);
    }

    #[test]
    fn forget후_재tofu_가능하고_회계가_감소한다() {
        let path = temp_path("forget");
        let mut kh = KnownHosts::load(&path).unwrap();
        kh.pin("h:1", "aa").unwrap();
        assert_eq!(kh.retained_bytes, 5);
        kh.forget("h:1").unwrap();
        assert_eq!(kh.retained_bytes, 0);
        assert_eq!(kh.check("h:1", "bb"), TofuDecision::FirstUse);
        let kh2 = KnownHosts::load(&path).unwrap();
        assert_eq!(kh2.lookup("h:1"), None);
        remove_parent(&path);
    }

    #[test]
    fn 손상_라인은_스킵하고_나머지는_로드() {
        let path = temp_path("corrupt");
        std::fs::write(
            &path,
            "# 주석\nh:1 aa:bb\n망가진줄만있음\nh:2 cc:dd extra\nh:3 ee:ff\n",
        )
        .unwrap();
        let kh = KnownHosts::load(&path).unwrap();
        assert_eq!(kh.lookup("h:1"), Some("aa:bb"));
        assert_eq!(kh.lookup("h:2"), Some("cc:dd"));
        assert_eq!(kh.lookup("h:3"), Some("ee:ff"));
        remove_parent(&path);
    }

    #[test]
    fn 파일크기_exact는_허용하고_plus_one은_거부한다() {
        let path = temp_path("file-limit");
        std::fs::write(&path, vec![b'#'; MAX_KNOWN_HOSTS_FILE_BYTES]).unwrap();
        assert_eq!(KnownHosts::load(&path).unwrap().entries().len(), 0);
        std::fs::write(&path, vec![b'#'; MAX_KNOWN_HOSTS_FILE_BYTES + 1]).unwrap();
        assert_eq!(
            KnownHosts::load(&path).unwrap_err().to_string(),
            "known_hosts_file_size_exceeded"
        );
        remove_parent(&path);
    }

    #[test]
    fn 엔트리수_exact는_허용하고_plus_one은_거부한다() {
        let path = temp_path("entry-limit");
        let mut text = String::new();
        for index in 0..MAX_KNOWN_HOSTS_ENTRIES {
            text.push_str(&format!("h{index} aa\n"));
        }
        std::fs::write(&path, &text).unwrap();
        assert_eq!(KnownHosts::load(&path).unwrap().entries().len(), 4_096);
        text.push_str("overflow aa\n");
        std::fs::write(&path, text).unwrap();
        assert_eq!(
            KnownHosts::load(&path).unwrap_err().to_string(),
            "known_hosts_entry_limit_exceeded"
        );
        remove_parent(&path);
    }

    #[test]
    fn 필드크기_exact는_허용하고_plus_one은_거부한다() {
        let path = temp_path("field-limit");
        let mut kh = KnownHosts::load(&path).unwrap();
        let host = "h".repeat(MAX_KNOWN_HOST_BYTES);
        let fingerprint = "A".repeat(MAX_KNOWN_HOST_FINGERPRINT_BYTES);
        kh.pin(&host, &fingerprint).unwrap();
        assert_eq!(
            kh.lookup(&host),
            Some("a".repeat(MAX_KNOWN_HOST_FINGERPRINT_BYTES).as_str())
        );
        assert_eq!(
            kh.pin(&"h".repeat(MAX_KNOWN_HOST_BYTES + 1), "aa")
                .unwrap_err()
                .to_string(),
            "known_hosts_host_size_exceeded"
        );
        assert_eq!(
            kh.pin("other", &"a".repeat(MAX_KNOWN_HOST_FINGERPRINT_BYTES + 1))
                .unwrap_err()
                .to_string(),
            "known_hosts_fingerprint_size_exceeded"
        );
        remove_parent(&path);
    }

    #[test]
    fn 총보유byte_exact는_허용하고_plus_one은_거부한다() {
        let path = temp_path("aggregate-limit");
        let exact = aggregate_fixture(false);
        assert_eq!(exact.lines().count(), MAX_KNOWN_HOSTS_ENTRIES);
        std::fs::write(&path, exact).unwrap();
        let kh = KnownHosts::load(&path).unwrap();
        assert_eq!(kh.retained_bytes, MAX_KNOWN_HOSTS_RETAINED_BYTES);
        std::fs::write(&path, aggregate_fixture(true)).unwrap();
        assert_eq!(
            KnownHosts::load(&path).unwrap_err().to_string(),
            "known_hosts_retained_bytes_exceeded"
        );
        remove_parent(&path);
    }

    #[test]
    fn duplicate는_last_wins이고_업데이트는_보유byte를_교체한다() {
        let path = temp_path("duplicate");
        std::fs::write(&path, "host LONGER\nhost b\n").unwrap();
        let mut kh = KnownHosts::load(&path).unwrap();
        assert_eq!(kh.entries().len(), 1);
        assert_eq!(kh.lookup("host"), Some("b"));
        assert_eq!(kh.retained_bytes, 5);
        kh.pin("host", "CCCC").unwrap();
        assert_eq!(kh.retained_bytes, 8);
        assert_eq!(kh.lookup("host"), Some("cccc"));
        let rows: Vec<_> = kh.into_entries().collect();
        assert_eq!(rows, vec![("host".to_owned(), "cccc".to_owned())]);
        remove_parent(&path);
    }

    #[test]
    fn invalid_utf8과_제어문자_필드는_fail_closed() {
        let path = temp_path("invalid");
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        assert_eq!(
            KnownHosts::load(&path).unwrap_err().to_string(),
            "known_hosts_invalid_utf8"
        );
        let kh_path = path.parent().unwrap().join("valid");
        let mut kh = KnownHosts::load(&kh_path).unwrap();
        assert_eq!(
            kh.pin("host\0", "aa").unwrap_err().to_string(),
            "known_hosts_host_invalid"
        );
        remove_parent(&path);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_fifo_directory는_즉시_fail_closed한다() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::symlink;

        let path = temp_path("special-inputs");
        let target = path.with_extension("target");
        std::fs::write(&target, "host aa\n").unwrap();
        symlink(&target, &path).unwrap();
        assert_eq!(
            KnownHosts::load(&path).unwrap_err().to_string(),
            "known_hosts_not_regular"
        );
        std::fs::remove_file(&path).unwrap();

        let fifo = path.with_extension("fifo");
        let fifo_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        assert_eq!(
            KnownHosts::load(&fifo).unwrap_err().to_string(),
            "known_hosts_not_regular"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));

        let directory = path.with_extension("directory");
        std::fs::create_dir(&directory).unwrap();
        assert_eq!(
            KnownHosts::load(&directory).unwrap_err().to_string(),
            "known_hosts_not_regular"
        );
        remove_parent(&path);
    }

    #[cfg(unix)]
    #[test]
    fn replacement_growth_shrink는_snapshot_load를_거부한다() {
        use std::io::Write as _;

        let replacement_path = temp_path("replacement");
        std::fs::write(&replacement_path, "h:1 aa\n").unwrap();
        let replacement = replacement_path.with_extension("new");
        std::fs::write(&replacement, "h:2 bb\n").unwrap();
        assert_eq!(
            read_bounded_with_hook(&replacement_path, || {
                std::fs::rename(&replacement, &replacement_path).unwrap();
            })
            .unwrap_err()
            .to_string(),
            "known_hosts_replaced_during_read"
        );
        remove_parent(&replacement_path);

        let growth_path = temp_path("growth");
        std::fs::write(&growth_path, "h:1 aa\n").unwrap();
        assert_eq!(
            read_bounded_with_hook(&growth_path, || {
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&growth_path)
                    .unwrap()
                    .write_all(b"x")
                    .unwrap();
            })
            .unwrap_err()
            .to_string(),
            "known_hosts_changed_during_read"
        );
        remove_parent(&growth_path);

        let shrink_path = temp_path("shrink");
        std::fs::write(&shrink_path, "h:1 aa\n").unwrap();
        assert_eq!(
            read_bounded_with_hook(&shrink_path, || {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&shrink_path)
                    .unwrap()
                    .set_len(2)
                    .unwrap();
            })
            .unwrap_err()
            .to_string(),
            "known_hosts_changed_during_read"
        );
        remove_parent(&shrink_path);
    }

    #[test]
    fn 반복_save는_healthy_process에서_temp를_누적하지_않는다() {
        let path = temp_path("repeated-save");
        let mut known_hosts = KnownHosts::load(&path).unwrap();
        for index in 0..64 {
            known_hosts
                .pin("host", &format!("fingerprint-{index}"))
                .unwrap();
            assert!(
                std::fs::read_dir(path.parent().unwrap())
                    .unwrap()
                    .all(|entry| {
                        !entry
                            .unwrap()
                            .file_name()
                            .to_string_lossy()
                            .contains(".deppytmp.")
                    })
            );
        }
        remove_parent(&path);
    }

    #[test]
    fn temp_create_collision은_기존파일을_삭제하지_않는다() {
        let parent = temp_dir("temp-collision");
        let collision = parent.join("known_hosts.deppytmp.existing");
        std::fs::write(&collision, b"existing-writer-data").unwrap();
        let entries = BTreeMap::from([("host".to_owned(), "aa".to_owned())]);
        assert_eq!(
            write_temp_file(&collision, &entries)
                .unwrap_err()
                .to_string(),
            "known_hosts_temp_create_failed"
        );
        assert_eq!(std::fs::read(&collision).unwrap(), b"existing-writer-data");
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn post_create_operation_실패는_자신의_temp를_정리한다() {
        let parent = temp_dir("post-create-failure");
        let temp = parent.join("known_hosts.deppytmp.injected");
        let entries = BTreeMap::from([("host".to_owned(), "aa".to_owned())]);
        assert_eq!(
            write_temp_file_with(&temp, &entries, |_file, _entries| {
                Err(static_error("known_hosts_injected_write_failure"))
            })
            .unwrap_err()
            .to_string(),
            "known_hosts_injected_write_failure"
        );
        assert!(!temp.exists());
        assert_eq!(std::fs::read_dir(&parent).unwrap().count(), 0);
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn atomic_replace_실패는_memory를_rollback하고_temp를_정리한다() {
        let parent = temp_dir("atomic-failure");
        let target = parent.join("target-directory");
        std::fs::create_dir(&target).unwrap();
        let mut kh = KnownHosts {
            path: target,
            entries: BTreeMap::new(),
            retained_bytes: 0,
        };
        assert_eq!(
            kh.pin("host", "aa").unwrap_err().to_string(),
            "known_hosts_atomic_replace_failed"
        );
        assert_eq!(kh.lookup("host"), None);
        assert_eq!(kh.retained_bytes, 0);

        kh.entries.insert("old".to_owned(), "aa".to_owned());
        kh.retained_bytes = 5;
        assert_eq!(
            kh.pin("old", "bbbb").unwrap_err().to_string(),
            "known_hosts_atomic_replace_failed"
        );
        assert_eq!(kh.lookup("old"), Some("aa"));
        assert_eq!(kh.retained_bytes, 5);
        assert_eq!(
            kh.forget("old").unwrap_err().to_string(),
            "known_hosts_atomic_replace_failed"
        );
        assert_eq!(kh.lookup("old"), Some("aa"));
        assert_eq!(kh.retained_bytes, 5);
        assert!(std::fs::read_dir(&parent).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".deppytmp.")
        }));
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn 저장파일은_owner_only_권한이다() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_path("permissions");
        let mut kh = KnownHosts::load(&path).unwrap();
        kh.pin("host", "aa").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        remove_parent(&path);
    }

    #[test]
    fn production_source는_unbounded_read와_raw_diagnostic을_금지한다() {
        let source = include_str!("known_hosts.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains("read_to_string"));
        assert!(!production.contains("HashMap"));
        assert!(!production.contains("with_context"));
        assert!(!production.contains("line:?"));
        assert!(!production.contains("File::open(path)"));
        assert!(!production.contains("read_to_end"));
        assert!(production.contains("read_exact"));
        assert!(production.contains("plus_one"));
        assert!(production.contains("libc::O_NOFOLLOW"));
        assert!(production.contains("libc::O_NONBLOCK"));
        assert!(production.contains("FILE_FLAG_OPEN_REPARSE_POINT"));
        assert!(production.contains("stable_metadata_matches"));
        assert!(production.contains("sync_parent_after_rename"));
        assert!(production.contains("create_new(true)"));
    }
}
