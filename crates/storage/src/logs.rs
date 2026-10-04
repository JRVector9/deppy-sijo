//! 세션당 tail-bounded redacted 로그 3종 (설계문서 7장):
//! redacted.ansi.log / redacted.plain.txt / events.redacted.jsonl
//! 호출측(runtime worker)이 redaction을 끝낸 바이트만 넘긴다 —
//! 이 모듈은 평문 secret을 받지 않는 것이 계약이다.

use std::fs::File;
#[cfg(any(test, not(unix)))]
use std::fs::OpenOptions;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use deppy_core::SessionId;

const ANSI_LOG_FILE: &str = "redacted.ansi.log";
const PLAIN_LOG_FILE: &str = "redacted.plain.txt";
const EVENTS_LOG_FILE: &str = "events.redacted.jsonl";
const TERMINAL_SIZE_FILE: &str = "terminal.size";
const TERMINAL_SIZE_BYTES_MAX: usize = 64;
#[cfg(unix)]
const LOG_TEMP_ATTEMPTS: usize = 16;

/// 런타임 복원이 읽는 ANSI tail 상한과 동일하다. 이보다 오래된 출력은 재시작 때도
/// 사용되지 않으므로 디스크에 무기한 중복 보관하지 않는다.
pub const ANSI_LOG_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// inbox 미리보기는 마지막 16KiB만 읽는다. 사람이 최근 출력을 넉넉히 확인할 수 있게
/// 4MiB를 남기되 ANSI 원본과 같은 전체 스트림을 계속 중복 저장하지 않는다.
pub const PLAIN_LOG_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// lifecycle 이벤트는 한 줄이 작고 최근 이벤트가 복구/진단에 중요하다.
pub const EVENTS_LOG_MAX_BYTES: u64 = 1024 * 1024;
/// 모든 워크스페이스의 redacted 세션 로그 합계 예산. 앱 시작 시 오래된 세션 묶음부터
/// 제거한다. provider transcript/agent session mapping과 scrollback.zlib은 대상이 아니다.
pub const SESSION_LOG_DISK_BUDGET_BYTES: u64 = 256 * 1024 * 1024;
const SESSION_LOG_SCAN_ENTRY_LIMIT: usize = 4_096;
const SESSION_LOG_SCAN_LIMIT_ERROR: &str = "session_log_scan_entry_limit";

#[cfg(test)]
thread_local! { static LOG_IO_COUNTS: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0,0)) }; }
#[cfg(test)]
thread_local! {
    static LOG_METADATA_FAIL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static LOG_WRITE_FAIL_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static LOG_POSITION_FAIL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
fn log_metadata(file: &File) -> std::io::Result<std::fs::Metadata> {
    #[cfg(test)]
    LOG_IO_COUNTS.with(|counts| {
        let (meta, writes) = counts.get();
        counts.set((meta + 1, writes));
    });
    #[cfg(test)]
    if LOG_METADATA_FAIL.with(std::cell::Cell::get) {
        return Err(std::io::Error::other("fixture_metadata_failed"));
    }
    file.metadata()
}
fn log_write_all(file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(test)]
    LOG_IO_COUNTS.with(|counts| {
        let (meta, writes) = counts.get();
        counts.set((meta, writes + 1));
    });
    #[cfg(test)]
    if let Some(limit) = LOG_WRITE_FAIL_AFTER.with(std::cell::Cell::take) {
        file.write_all(&bytes[..limit.min(bytes.len())])?;
        return Err(std::io::Error::other("fixture_partial_write"));
    }
    file.write_all(bytes)
}
fn log_stream_position(file: &mut File) -> std::io::Result<u64> {
    #[cfg(test)]
    if LOG_POSITION_FAIL.with(std::cell::Cell::take) {
        return Err(std::io::Error::other("fixture_position_failed"));
    }
    file.stream_position()
}

struct BoundedLogFile {
    file: File,
    path: PathBuf,
    max_bytes: u64,
    tail_boundary: TailBoundary,
    /// Confirmed same-handle length. Errors invalidate it; never assume an unknown file is empty.
    known_len: Option<u64>,
}

#[derive(Clone, Copy)]
enum TailBoundary {
    /// ANSI는 line-oriented 데이터가 아니다. CR/CSI만으로 그리는 TUI의 tail도 보존하되
    /// UTF-8 문자나 CSI/OSC/string escape 한가운데서는 시작하지 않는다.
    Ansi,
    /// 사람이 읽는 text/JSONL은 가능하면 첫 완전한 LF 뒤에서 시작한다. LF가 전혀 없는
    /// 구간은 전량 삭제하지 않고 그대로 보존해 pathological giant line의 데이터 손실을 막는다.
    NextNewlineIfPresent,
}

impl BoundedLogFile {
    #[cfg(any(test, not(unix)))]
    fn open(path: &Path, max_bytes: u64, tail_boundary: TailBoundary) -> anyhow::Result<Self> {
        let file = open_regular_log_file(path, true)
            .with_context(|| format!("로그 파일 열기 실패: {}", path.display()))?;
        let known_len = log_metadata(&file)?.len();
        let mut bounded = Self {
            file,
            path: path.to_path_buf(),
            max_bytes,
            tail_boundary,
            known_len: Some(known_len),
        };
        bounded.compact_if_oversized(max_bytes / 2)?;
        Ok(bounded)
    }

    #[cfg(unix)]
    fn open_at(
        directory: &PinnedLogDirectory,
        name: &str,
        max_bytes: u64,
        tail_boundary: TailBoundary,
    ) -> anyhow::Result<Self> {
        let path = directory.path.join(name);
        let file = open_regular_log_at(directory, std::ffi::OsStr::new(name), true)
            .with_context(|| format!("로그 파일 열기 실패: {}", path.display()))?;
        let known_len = log_metadata(&file)?.len();
        let mut bounded = Self {
            file,
            path,
            max_bytes,
            tail_boundary,
            known_len: Some(known_len),
        };
        bounded.compact_if_oversized(max_bytes / 2)?;
        Ok(bounded)
    }

    fn len(&self) -> std::io::Result<u64> {
        self.known_len
            .ok_or_else(|| std::io::Error::other("log_length_unknown"))
    }
    fn recover_len(&mut self) -> std::io::Result<u64> {
        if let Some(len) = self.known_len {
            return Ok(len);
        }
        let len = log_metadata(&self.file)?.len();
        self.known_len = Some(len);
        Ok(len)
    }
    fn compact(&mut self, retain: u64) -> std::io::Result<()> {
        self.known_len = None;
        self.known_len = Some(compact_open_file_to_tail(
            &mut self.file,
            retain,
            self.tail_boundary,
        )?);
        Ok(())
    }
    fn append(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        if let Err(error) = self.append_inner(bytes) {
            // External writes can make the cached length stale, and a failed write or position
            // query can leave new bytes behind. Reconcile the pinned inode even on failure;
            // never replay the original slice or report a repaired write as successful.
            self.known_len = None;
            if let Err(repair_error) = self.repair_cap_after_error() {
                return Err(error.context(format!(
                    "로그 오류 후 상한 복구 실패: {}: {repair_error}",
                    self.path.display()
                )));
            }
            return Err(error);
        }
        Ok(())
    }
    fn repair_cap_after_error(&mut self) -> std::io::Result<()> {
        if log_metadata(&self.file)?.len() > self.max_bytes {
            compact_open_file_to_tail(&mut self.file, self.max_bytes / 2, self.tail_boundary)?;
        }
        Ok(())
    }
    fn append_inner(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let incoming = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if incoming >= self.max_bytes {
            self.known_len = None;
            let len = replace_open_file_with_appended_tail(
                &mut self.file,
                bytes,
                self.max_bytes,
                self.tail_boundary,
            )
            .with_context(|| format!("로그 tail 기록 실패: {}", self.path.display()))?;
            self.known_len = Some(len);
            return Ok(());
        }
        let current = self
            .recover_len()
            .with_context(|| format!("로그 길이 조회 실패: {}", self.path.display()))?;
        if current.saturating_add(incoming) > self.max_bytes {
            let retain = self
                .max_bytes
                .saturating_sub(incoming)
                .min(self.max_bytes / 2);
            self.compact(retain)
                .with_context(|| format!("로그 tail 압축 실패: {}", self.path.display()))?;
        }
        // Reserve capacity from the cached length; failures also reconcile external changes.
        self.known_len = None;
        log_write_all(&mut self.file, bytes)
            .with_context(|| format!("로그 기록 실패: {}", self.path.display()))?;
        // O_APPEND leaves this handle at its actual new EOF, including an external truncate or
        // append. A cheap position query reconciles successful writes without per-chunk fstat.
        let len = log_stream_position(&mut self.file)?;
        self.known_len = Some(len);
        if len > self.max_bytes {
            self.compact(self.max_bytes / 2)?;
        }
        Ok(())
    }
    fn compact_if_oversized(&mut self, retain_bytes: u64) -> anyhow::Result<()> {
        if self.recover_len()? > self.max_bytes {
            self.compact(retain_bytes)
                .with_context(|| format!("기존 로그 tail 압축 실패: {}", self.path.display()))?;
        }
        Ok(())
    }

    fn flush(&mut self) {
        self.file.flush().ok();
    }
}

#[cfg(any(test, not(unix)))]
fn open_regular_log_file(path: &Path, create: bool) -> std::io::Result<File> {
    let before = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Some(metadata),
        Ok(_) => return Err(std::io::Error::other("log_file_not_regular")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => None,
        Err(error) => return Err(error),
    };

    let mut options = OpenOptions::new();
    options.read(true).append(true).create(create);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let opened = file.metadata()?;
    if !opened.file_type().is_file() {
        return Err(std::io::Error::other("log_file_not_regular"));
    }
    let path_after = std::fs::symlink_metadata(path)?;
    if !path_after.file_type().is_file() {
        return Err(std::io::Error::other("log_file_replaced"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if before.as_ref().is_some_and(|metadata| {
            metadata.dev() != opened.dev() || metadata.ino() != opened.ino()
        }) || opened.dev() != path_after.dev()
            || opened.ino() != path_after.ino()
        {
            return Err(std::io::Error::other("log_file_replaced"));
        }
    }
    Ok(file)
}

#[cfg(unix)]
struct PinnedLogDirectory {
    fd: std::os::fd::OwnedFd,
    path: PathBuf,
}

#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
fn pinned_log_directory_identity(directory: &PinnedLogDirectory) -> std::io::Result<FileIdentity> {
    use std::os::fd::AsRawFd as _;

    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(directory.fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    #[cfg(target_vendor = "apple")]
    let device = u64::try_from(stat.st_dev)
        .map_err(|_| std::io::Error::other("session_log_device_invalid"))?;
    #[cfg(not(target_vendor = "apple"))]
    let device = stat.st_dev;
    Ok(FileIdentity {
        device,
        inode: stat.st_ino,
    })
}

#[cfg(unix)]
fn metadata_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt as _;

    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[cfg(unix)]
fn pin_log_directory(
    logs_root: &Path,
    directory: &Path,
    create_leaf: bool,
) -> anyhow::Result<Option<PinnedLogDirectory>> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::ffi::OsStrExt as _;

    let relative = directory
        .strip_prefix(logs_root)
        .context("session log directory escaped logs_root")?;
    let components = relative
        .components()
        .map(|component| match component {
            std::path::Component::Normal(name) => Ok(name.to_os_string()),
            _ => anyhow::bail!("session log directory component invalid"),
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    anyhow::ensure!(
        !create_leaf || components.len() == 1,
        "session log create path must be one component"
    );
    if create_leaf {
        std::fs::create_dir_all(logs_root)
            .with_context(|| format!("로그 루트 생성 실패: {}", logs_root.display()))?;
    }
    let root_name = std::ffi::CString::new(logs_root.as_os_str().as_bytes())
        .map_err(|_| anyhow::anyhow!("logs_root_contains_nul"))?;
    let root_fd = unsafe {
        libc::open(
            root_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if root_fd < 0 {
        let error = std::io::Error::last_os_error();
        if !create_leaf && error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error).context("logs_root directory open failed");
    }
    let mut current = unsafe { std::os::fd::OwnedFd::from_raw_fd(root_fd) };
    for (index, component) in components.iter().enumerate() {
        let component = std::ffi::CString::new(component.as_bytes())
            .map_err(|_| anyhow::anyhow!("session_log_component_contains_nul"))?;
        if create_leaf && index + 1 == components.len() {
            let created = unsafe { libc::mkdirat(current.as_raw_fd(), component.as_ptr(), 0o755) };
            if created != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error).context("session log directory create failed");
                }
            }
        }
        let next = unsafe {
            libc::openat(
                current.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if next < 0 {
            let error = std::io::Error::last_os_error();
            if !create_leaf && error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(error).context("session log directory open failed");
        }
        current = unsafe { std::os::fd::OwnedFd::from_raw_fd(next) };
    }
    Ok(Some(PinnedLogDirectory {
        fd: current,
        path: directory.to_path_buf(),
    }))
}

#[cfg(unix)]
fn open_regular_log_at(
    directory: &PinnedLogDirectory,
    name: &std::ffi::OsStr,
    create: bool,
) -> std::io::Result<File> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::ffi::OsStrExt as _;

    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::other("log_name_contains_nul"))?;
    let mut flags =
        libc::O_RDWR | libc::O_APPEND | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    if create {
        flags |= libc::O_CREAT;
    }
    let fd = unsafe { libc::openat(directory.fd.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::other("log_file_not_regular"));
    }
    Ok(file)
}

#[cfg(unix)]
fn open_readonly_log_at(
    directory: &PinnedLogDirectory,
    name: &std::ffi::OsStr,
) -> std::io::Result<File> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::ffi::OsStrExt as _;

    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::other("log_name_contains_nul"))?;
    let fd = unsafe {
        libc::openat(
            directory.fd.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::other("log_file_not_regular"));
    }
    Ok(file)
}

#[cfg(unix)]
fn regular_log_entry_exists_at(
    directory: &PinnedLogDirectory,
    name: &std::ffi::CStr,
) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd as _;

    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            directory.fd.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(false);
        }
        return Err(error);
    }
    let stat = unsafe { stat.assume_init() };
    Ok((stat.st_mode & libc::S_IFMT) == libc::S_IFREG)
}

#[cfg(unix)]
fn atomic_write_log_at(
    directory: &PinnedLogDirectory,
    target: &str,
    bytes: &[u8],
) -> std::io::Result<()> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let final_name = std::ffi::CString::new(target).expect("log target names are static ASCII");
    for attempt in 0..LOG_TEMP_ATTEMPTS {
        let temp_name = if attempt == 0 {
            format!("{target}.deppytmp")
        } else {
            format!(
                ".{target}.deppytmp.{}.{}.{}",
                std::process::id(),
                sequence,
                attempt
            )
        };
        let temp_name = std::ffi::CString::new(temp_name).expect("generated temp name has no NUL");
        let fd = unsafe {
            libc::openat(
                directory.fd.as_raw_fd(),
                temp_name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error);
            }
            if !regular_log_entry_exists_at(directory, &temp_name)? {
                return Err(std::io::Error::other("log_temp_not_regular"));
            }
            continue;
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        let result = (|| {
            file.write_all(bytes)?;
            drop(file);
            let renamed = unsafe {
                libc::renameat(
                    directory.fd.as_raw_fd(),
                    temp_name.as_ptr(),
                    directory.fd.as_raw_fd(),
                    final_name.as_ptr(),
                )
            };
            if renamed != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = unsafe { libc::unlinkat(directory.fd.as_raw_fd(), temp_name.as_ptr(), 0) };
        }
        return result;
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "log_temp_collision_limit",
    ))
}

#[cfg(unix)]
fn unlink_log_at(
    directory: &PinnedLogDirectory,
    name: &std::ffi::OsStr,
    expected: FileIdentity,
) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;

    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::other("log_name_contains_nul"))?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let status = unsafe {
        libc::fstatat(
            directory.fd.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if status != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(false);
        }
        return Err(error);
    }
    let stat = unsafe { stat.assume_init() };
    #[cfg(target_vendor = "apple")]
    let device = u64::try_from(stat.st_dev)
        .map_err(|_| std::io::Error::other("session_log_device_invalid"))?;
    #[cfg(not(target_vendor = "apple"))]
    let device = stat.st_dev;
    let inode = stat.st_ino;
    if (stat.st_mode & libc::S_IFMT) != libc::S_IFREG
        || device != expected.device
        || inode != expected.inode
    {
        return Err(std::io::Error::other("session_log_file_replaced"));
    }
    let result = unsafe { libc::unlinkat(directory.fd.as_raw_fd(), name.as_ptr(), 0) };
    if result == 0 {
        Ok(true)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub struct SessionLogWriter {
    ansi: BoundedLogFile,
    plain: BoundedLogFile,
    events: BoundedLogFile,
    /// chunk 경계에 걸친 escape 시퀀스 대응 — plain 변환기 상태 유지
    strip_state: StripState,
}

impl SessionLogWriter {
    /// `logs_root/<session id>/` 아래에 세 파일을 append 모드로 연다.
    pub fn open(logs_root: &Path, session: SessionId) -> anyhow::Result<Self> {
        Self::open_key(logs_root, &session.0.to_string())
    }

    /// 영속 세션 id를 디렉터리 키로 사용해 로그를 연다. 런타임의 숫자 SessionId는
    /// 프로세스 재시작마다 다시 1부터 시작하므로, 재시작 복원 대상은 이 API를 써야
    /// 이전 ANSI 로그와 같은 파일에 계속 append할 수 있다.
    pub fn open_key(logs_root: &Path, session_key: &str) -> anyhow::Result<Self> {
        Self::open_key_with_session_hook(logs_root, session_key, || {})
    }

    fn open_key_with_session_hook(
        logs_root: &Path,
        session_key: &str,
        after_session_pin: impl FnOnce(),
    ) -> anyhow::Result<Self> {
        #[cfg(unix)]
        {
            let dir = Self::session_dir_key(logs_root, session_key)?;
            let directory = pin_log_directory(logs_root, &dir, true)?
                .context("session log directory missing after creation")?;
            after_session_pin();
            Ok(Self {
                ansi: BoundedLogFile::open_at(
                    &directory,
                    ANSI_LOG_FILE,
                    ANSI_LOG_MAX_BYTES,
                    TailBoundary::Ansi,
                )?,
                plain: BoundedLogFile::open_at(
                    &directory,
                    PLAIN_LOG_FILE,
                    PLAIN_LOG_MAX_BYTES,
                    TailBoundary::NextNewlineIfPresent,
                )?,
                events: BoundedLogFile::open_at(
                    &directory,
                    EVENTS_LOG_FILE,
                    EVENTS_LOG_MAX_BYTES,
                    TailBoundary::NextNewlineIfPresent,
                )?,
                strip_state: StripState::default(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (logs_root, session_key, after_session_pin);
            anyhow::bail!("session_log_descriptor_io_unsupported")
        }
    }

    pub fn session_dir(logs_root: &Path, session: SessionId) -> PathBuf {
        logs_root.join(session.0.to_string())
    }

    /// 저장된 ANSI 로그를 복원할 때 사용하는 경로. key는 DB가 발급한 UUID지만,
    /// 방어적으로 단일 파일명 성분만 허용해 경로 탈출을 막는다.
    pub fn session_dir_key(logs_root: &Path, session_key: &str) -> anyhow::Result<PathBuf> {
        let mut components = Path::new(session_key).components();
        let valid = matches!(components.next(), Some(std::path::Component::Normal(_)))
            && components.next().is_none();
        anyhow::ensure!(
            valid && !session_key.is_empty(),
            "유효하지 않은 session log key"
        );
        Ok(logs_root.join(session_key))
    }

    pub fn ansi_path(logs_root: &Path, session_key: &str) -> anyhow::Result<PathBuf> {
        Ok(Self::session_dir_key(logs_root, session_key)?.join(ANSI_LOG_FILE))
    }

    /// 마지막으로 UI가 확정한 터미널 grid 크기. ANSI 로그의 zsh/ZLE redraw는 당시
    /// 열 수에 의존하므로 재시작 복원도 같은 크기에서 먼저 파싱해야 한다.
    pub fn terminal_size_path(logs_root: &Path, session_key: &str) -> anyhow::Result<PathBuf> {
        Ok(Self::session_dir_key(logs_root, session_key)?.join("terminal.size"))
    }

    pub fn load_terminal_size(
        logs_root: &Path,
        session_key: &str,
    ) -> anyhow::Result<Option<(u16, u16)>> {
        let Some(raw) = load_terminal_size_raw(logs_root, session_key)? else {
            return Ok(None);
        };
        let mut fields = raw.split_whitespace();
        let cols: u16 = fields
            .next()
            .context("터미널 열 수 없음")?
            .parse()
            .context("터미널 열 수 파싱 실패")?;
        let rows: u16 = fields
            .next()
            .context("터미널 행 수 없음")?
            .parse()
            .context("터미널 행 수 파싱 실패")?;
        anyhow::ensure!(fields.next().is_none(), "터미널 크기 필드가 너무 많음");
        // 손상된 sidecar가 재시작 시 과도한 terminal grid 할당으로 이어지지 않게
        // UI/runtime이 허용하는 범위와 같은 상한을 둔다.
        anyhow::ensure!((1..=500).contains(&cols), "터미널 열 수 범위 초과");
        anyhow::ensure!((1..=500).contains(&rows), "터미널 행 수 범위 초과");
        Ok(Some((cols, rows)))
    }

    pub fn save_terminal_size(
        logs_root: &Path,
        session_key: &str,
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<()> {
        Self::save_terminal_size_with_session_hook(logs_root, session_key, cols, rows, || {})
    }

    fn save_terminal_size_with_session_hook(
        logs_root: &Path,
        session_key: &str,
        cols: u16,
        rows: u16,
        after_session_pin: impl FnOnce(),
    ) -> anyhow::Result<()> {
        anyhow::ensure!((1..=500).contains(&cols), "터미널 열 수 범위 초과");
        anyhow::ensure!((1..=500).contains(&rows), "터미널 행 수 범위 초과");
        #[cfg(unix)]
        {
            let dir = Self::session_dir_key(logs_root, session_key)?;
            let directory = pin_log_directory(logs_root, &dir, true)?
                .context("session log directory missing after creation")?;
            after_session_pin();
            atomic_write_log_at(
                &directory,
                TERMINAL_SIZE_FILE,
                format!("{cols} {rows}\n").as_bytes(),
            )
            .map_err(|_| anyhow::anyhow!("terminal_size_write_failed"))
        }
        #[cfg(not(unix))]
        {
            let _ = (logs_root, session_key, after_session_pin);
            anyhow::bail!("session_log_descriptor_io_unsupported")
        }
    }

    /// append 재개 시 offset을 기존 파일 길이부터 이어가기 위한 길이 조회.
    pub fn ansi_len(&self) -> std::io::Result<u64> {
        self.ansi.len()
    }

    /// redaction이 끝난 출력 chunk를 기록한다.
    /// ansi.log에는 그대로, plain.txt에는 ANSI escape 제거본을 쓴다.
    pub fn append_output(&mut self, redacted: &[u8]) -> anyhow::Result<()> {
        if redacted.is_empty() {
            return Ok(());
        }
        self.ansi.append(redacted).context("ansi.log 기록 실패")?;
        let plain = strip_ansi_stateful(redacted, &mut self.strip_state);
        self.plain.append(&plain).context("plain.txt 기록 실패")?;
        Ok(())
    }

    /// 세션 이벤트 한 줄 (jsonl). detail은 이미 redaction된 값만 받는다.
    pub fn append_event(&mut self, kind: &str, detail: Option<&str>) -> anyhow::Result<()> {
        let ts = deppy_core::time::unix_ms();
        let line = match detail {
            Some(detail) => format!(
                "{{\"ts_ms\":{ts},\"event\":\"{}\",\"detail\":\"{}\"}}\n",
                json_escape(kind),
                json_escape(detail)
            ),
            None => format!("{{\"ts_ms\":{ts},\"event\":\"{}\"}}\n", json_escape(kind)),
        };
        self.events
            .append(line.as_bytes())
            .context("events.jsonl 기록 실패")?;
        self.events.flush();
        Ok(())
    }

    /// 종료/주기 flush.
    pub fn flush(&mut self) {
        self.ansi.flush();
        self.plain.flush();
        self.events.flush();
    }
}

fn load_terminal_size_raw(logs_root: &Path, session_key: &str) -> anyhow::Result<Option<String>> {
    #[cfg(unix)]
    {
        let dir = SessionLogWriter::session_dir_key(logs_root, session_key)?;
        let Some(directory) = pin_log_directory(logs_root, &dir, false)? else {
            return Ok(None);
        };
        let file = match open_readonly_log_at(&directory, std::ffi::OsStr::new(TERMINAL_SIZE_FILE))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => anyhow::bail!("terminal_size_open_failed"),
        };
        read_terminal_size_open_file(file)
    }
    #[cfg(not(unix))]
    {
        let _ = (logs_root, session_key);
        anyhow::bail!("session_log_descriptor_io_unsupported")
    }
}

#[cfg(unix)]
fn read_terminal_size_open_file(mut file: File) -> anyhow::Result<Option<String>> {
    let opened = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("terminal_size_metadata_failed"))?;
    anyhow::ensure!(opened.file_type().is_file(), "terminal_size_not_regular");
    anyhow::ensure!(
        opened.len() <= TERMINAL_SIZE_BYTES_MAX as u64,
        "terminal_size_bytes_exceeded"
    );
    let declared_len = usize::try_from(opened.len())
        .map_err(|_| anyhow::anyhow!("terminal_size_bytes_exceeded"))?;
    let mut bytes = vec![0_u8; declared_len];
    file.read_exact(&mut bytes)
        .map_err(|_| anyhow::anyhow!("terminal_size_changed"))?;
    let mut overflow = [0_u8; 1];
    anyhow::ensure!(
        file.read(&mut overflow)
            .map_err(|_| anyhow::anyhow!("terminal_size_read_failed"))?
            == 0,
        "terminal_size_changed"
    );
    anyhow::ensure!(
        file.metadata()
            .map_err(|_| anyhow::anyhow!("terminal_size_metadata_failed"))?
            .len()
            == opened.len(),
        "terminal_size_changed"
    );
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| anyhow::anyhow!("terminal_size_utf8_invalid"))
}

#[cfg(any(test, not(unix)))]
fn read_terminal_size_bounded(path: &Path) -> anyhow::Result<Option<String>> {
    read_terminal_size_bounded_with_hook(path, || {})
}

#[cfg(any(test, not(unix)))]
fn read_terminal_size_bounded_with_hook(
    path: &Path,
    after_snapshot: impl FnOnce(),
) -> anyhow::Result<Option<String>> {
    let before = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => anyhow::bail!("terminal_size_not_regular"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => anyhow::bail!("terminal_size_metadata_failed"),
    };
    anyhow::ensure!(
        before.len() <= TERMINAL_SIZE_BYTES_MAX as u64,
        "terminal_size_bytes_exceeded"
    );

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = options
        .open(path)
        .map_err(|_| anyhow::anyhow!("terminal_size_open_failed"))?;
    let opened = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("terminal_size_metadata_failed"))?;
    anyhow::ensure!(opened.file_type().is_file(), "terminal_size_not_regular");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        anyhow::ensure!(
            before.dev() == opened.dev() && before.ino() == opened.ino(),
            "terminal_size_replaced"
        );
    }
    anyhow::ensure!(
        opened.len() <= TERMINAL_SIZE_BYTES_MAX as u64,
        "terminal_size_bytes_exceeded"
    );

    let declared_len = usize::try_from(opened.len())
        .map_err(|_| anyhow::anyhow!("terminal_size_bytes_exceeded"))?;
    after_snapshot();
    let mut bytes = vec![0_u8; declared_len];
    file.read_exact(&mut bytes)
        .map_err(|_| anyhow::anyhow!("terminal_size_changed"))?;
    let mut overflow = [0_u8; 1];
    anyhow::ensure!(
        file.read(&mut overflow)
            .map_err(|_| anyhow::anyhow!("terminal_size_read_failed"))?
            == 0,
        "terminal_size_changed"
    );
    let after = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("terminal_size_metadata_failed"))?;
    anyhow::ensure!(after.len() == opened.len(), "terminal_size_changed");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let path_after = std::fs::symlink_metadata(path)
            .map_err(|_| anyhow::anyhow!("terminal_size_replaced"))?;
        anyhow::ensure!(
            path_after.file_type().is_file()
                && opened.dev() == after.dev()
                && opened.ino() == after.ino()
                && after.dev() == path_after.dev()
                && after.ino() == path_after.ino(),
            "terminal_size_replaced"
        );
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| anyhow::anyhow!("terminal_size_utf8_invalid"))
}

/// 열린 파일의 끝 `retain_bytes`만 같은 inode에 다시 쓴다. writer handle을 교체/rename하지
/// 않으므로 런타임이 계속 가진 append handle에도 즉시 적용된다. text/JSONL은 LF가 있으면
/// 첫 완전한 줄로 정렬하고, line-oriented가 아닌 ANSI는 UTF-8/escape 경계에 맞춘다.
/// For a chunk at least as large as the cap, the final retained tail lies wholly within
/// the incoming slice. Scan the previous pinned stream only for ANSI boundary state, then
/// replace it with that bounded slice. Never append a giant chunk and hope later truncation works.
fn replace_open_file_with_appended_tail(
    file: &mut File,
    incoming: &[u8],
    max_bytes: u64,
    boundary: TailBoundary,
) -> std::io::Result<u64> {
    let previous_len = log_metadata(file)?.len();
    let max =
        usize::try_from(max_bytes).map_err(|_| std::io::Error::other("log_tail_size_invalid"))?;
    let mut start = incoming.len().saturating_sub(max);
    match boundary {
        TailBoundary::Ansi => {
            let mut scanner = AnsiBoundaryScanner::default();
            file.seek(std::io::SeekFrom::Start(0))?;
            let mut position = 0u64;
            let mut buffer = [0u8; 8192];
            while position < previous_len {
                let take = (previous_len - position).min(buffer.len() as u64) as usize;
                file.read_exact(&mut buffer[..take])?;
                for byte in &buffer[..take] {
                    scanner.advance(*byte);
                }
                position += take as u64;
            }
            let mut safe = None;
            for (index, byte) in incoming.iter().enumerate() {
                if index >= start && safe.is_none() && scanner.at_boundary() {
                    safe = Some(index);
                }
                scanner.advance(*byte);
            }
            start = safe.unwrap_or(incoming.len());
        }
        TailBoundary::NextNewlineIfPresent => {
            if previous_len.saturating_add(start as u64) > 0 {
                if let Some(newline) = incoming[start..].iter().position(|byte| *byte == b'\n') {
                    start += newline + 1;
                } else {
                    start += incoming[start..]
                        .iter()
                        .take_while(|byte| **byte & 0xc0 == 0x80)
                        .count();
                }
            }
        }
    }
    if log_metadata(file)?.len() != previous_len {
        return Err(std::io::Error::other("log_changed_during_compaction"));
    }
    file.set_len(0)?;
    log_write_all(file, &incoming[start..])?;
    file.flush()?;
    Ok((incoming.len() - start) as u64)
}

fn compact_open_file_to_tail(
    file: &mut File,
    retain_bytes: u64,
    tail_boundary: TailBoundary,
) -> std::io::Result<u64> {
    compact_open_file_to_tail_with_hook(file, retain_bytes, tail_boundary, || {})
}

fn compact_open_file_to_tail_with_hook(
    file: &mut File,
    retain_bytes: u64,
    tail_boundary: TailBoundary,
    after_snapshot: impl FnOnce(),
) -> std::io::Result<u64> {
    file.flush()?;
    let len = log_metadata(file)?.len();
    if len <= retain_bytes {
        return Ok(len);
    }
    if retain_bytes == 0 {
        file.set_len(0)?;
        return Ok(0);
    }
    after_snapshot();
    let mut start = len.saturating_sub(retain_bytes);
    if matches!(tail_boundary, TailBoundary::Ansi) {
        start = seek_ansi_tail_boundary_snapshot(file, start, len, false)?;
    }
    file.seek(std::io::SeekFrom::Start(start))?;
    let tail_len = usize::try_from(len.saturating_sub(start))
        .map_err(|_| std::io::Error::other("log_tail_size_invalid"))?;
    let mut tail = vec![0_u8; tail_len];
    file.read_exact(&mut tail)?;
    if start > 0
        && matches!(tail_boundary, TailBoundary::NextNewlineIfPresent)
        && let Some(newline) = tail.iter().position(|byte| *byte == b'\n')
    {
        tail.drain(..=newline);
    } else if start > 0 && matches!(tail_boundary, TailBoundary::NextNewlineIfPresent) {
        let continuation_bytes = tail.iter().take_while(|byte| **byte & 0xc0 == 0x80).count();
        tail.drain(..continuation_bytes);
    }
    if log_metadata(file)?.len() != len {
        return Err(std::io::Error::other("log_changed_during_compaction"));
    }
    file.set_len(0)?;
    log_write_all(file, &tail)?;
    file.flush()?;
    Ok(tail.len() as u64)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum AnsiBoundaryPhase {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    Osc,
    OscEscape,
    String,
    StringEscape,
}

#[derive(Debug, Clone, Copy, Default)]
struct AnsiBoundaryScanner {
    phase: AnsiBoundaryPhase,
    utf8_remaining: u8,
}

impl AnsiBoundaryScanner {
    fn at_boundary(self) -> bool {
        self.phase == AnsiBoundaryPhase::Ground && self.utf8_remaining == 0
    }

    fn advance(&mut self, byte: u8) {
        use AnsiBoundaryPhase as Phase;

        match self.phase {
            Phase::Ground if self.utf8_remaining > 0 => {
                if byte & 0xc0 == 0x80 {
                    self.utf8_remaining -= 1;
                    return;
                }
                // 손상 UTF-8은 현재 바이트에서 다시 동기화한다.
                self.utf8_remaining = 0;
                self.advance(byte);
            }
            Phase::Ground => match byte {
                0x1b => self.phase = Phase::Escape,
                0xc2..=0xdf => self.utf8_remaining = 1,
                0xe0..=0xef => self.utf8_remaining = 2,
                0xf0..=0xf4 => self.utf8_remaining = 3,
                _ => {}
            },
            Phase::Escape => match byte {
                b'[' => self.phase = Phase::Csi,
                b']' => self.phase = Phase::Osc,
                b'P' | b'X' | b'^' | b'_' => self.phase = Phase::String,
                0x20..=0x2f => self.phase = Phase::EscapeIntermediate,
                0x1b => {}
                _ => self.phase = Phase::Ground,
            },
            Phase::EscapeIntermediate => match byte {
                0x30..=0x7e => self.phase = Phase::Ground,
                0x1b => self.phase = Phase::Escape,
                _ => {}
            },
            Phase::Csi => match byte {
                0x40..=0x7e | 0x18 | 0x1a => self.phase = Phase::Ground,
                0x1b => self.phase = Phase::Escape,
                _ => {}
            },
            Phase::Osc => match byte {
                0x07 | 0x18 | 0x1a => self.phase = Phase::Ground,
                0x1b => self.phase = Phase::OscEscape,
                _ => {}
            },
            Phase::OscEscape => {
                self.phase = if byte == b'\\' {
                    Phase::Ground
                } else if byte == 0x1b {
                    Phase::OscEscape
                } else {
                    Phase::Osc
                };
            }
            Phase::String => match byte {
                0x18 | 0x1a => self.phase = Phase::Ground,
                0x1b => self.phase = Phase::StringEscape,
                _ => {}
            },
            Phase::StringEscape => {
                self.phase = if byte == b'\\' {
                    Phase::Ground
                } else if byte == 0x1b {
                    Phase::StringEscape
                } else {
                    Phase::String
                };
            }
        }
    }
}

/// `requested_start` 이후에서 UTF-8 문자와 ANSI control string이 모두 끝난 첫 경계를
/// 찾는다. `prefer_newline`이면 ground-state LF 뒤를 우선하고, LF가 없는 TUI stream은
/// 첫 안전 경계를 사용한다. 시작부터 상태만 streaming scan하므로 파일 전체를 메모리에
/// 올리지 않는다.
pub fn seek_ansi_tail_boundary(
    file: &mut File,
    requested_start: u64,
    prefer_newline: bool,
) -> std::io::Result<u64> {
    let snapshot_end = file.metadata()?.len();
    seek_ansi_tail_boundary_snapshot(file, requested_start, snapshot_end, prefer_newline)
}

/// `snapshot_end`를 잡은 caller가 concurrent append 이후에도 그 snapshot 바깥을 읽지
/// 않도록 하는 경계 탐색 variant. 파일이 짧아지면 부분 결과를 사용하지 않고 실패한다.
pub fn seek_ansi_tail_boundary_snapshot(
    file: &mut File,
    requested_start: u64,
    snapshot_end: u64,
    prefer_newline: bool,
) -> std::io::Result<u64> {
    let requested_start = requested_start.min(snapshot_end);
    file.seek(std::io::SeekFrom::Start(0))?;

    let mut scanner = AnsiBoundaryScanner::default();
    let mut first_safe = (requested_start == 0).then_some(0);
    let mut position = 0u64;
    let mut buffer = [0u8; 8192];
    while position < snapshot_end {
        let remaining =
            usize::try_from((snapshot_end - position).min(buffer.len() as u64)).unwrap_or(0);
        let read = file.read(&mut buffer[..remaining])?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "ansi_snapshot_short_read",
            ));
        }
        for &byte in &buffer[..read] {
            if position >= requested_start && first_safe.is_none() && scanner.at_boundary() {
                first_safe = Some(position);
            }
            let was_ground = scanner.at_boundary();
            scanner.advance(byte);
            position += 1;
            if position >= requested_start && scanner.at_boundary() {
                first_safe.get_or_insert(position);
                if prefer_newline && was_ground && byte == b'\n' {
                    file.seek(std::io::SeekFrom::Start(position))?;
                    return Ok(position);
                }
            }
        }
    }

    let start = first_safe.unwrap_or(snapshot_end);
    file.seek(std::io::SeekFrom::Start(start))?;
    Ok(start)
}

struct SessionLogBundle {
    paths: Vec<SessionLogPath>,
    bytes: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    directory_identity: Option<FileIdentity>,
}

struct SessionLogPath {
    path: PathBuf,
    len: u64,
    #[cfg(unix)]
    identity: FileIdentity,
}

struct SessionLogCandidate {
    dir: PathBuf,
    path: PathBuf,
    original_len: u64,
    modified: std::time::SystemTime,
    max_bytes: u64,
    retain_bytes: u64,
    tail_boundary: TailBoundary,
    #[cfg(unix)]
    identity: FileIdentity,
    #[cfg(unix)]
    directory_identity: FileIdentity,
}

impl Default for SessionLogBundle {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            bytes: 0,
            modified: std::time::UNIX_EPOCH,
            #[cfg(unix)]
            directory_identity: None,
        }
    }
}

/// 앱 로그 루트 전체를 스캔해 파일별 상한을 먼저 적용하고, 그래도 전체 예산을 넘으면
/// 세션의 redacted 로그 3종을 오래된 묶음부터 제거한다. 앱 자체 `app.log`, provider
/// transcript, terminal.size, scrollback.zlib은 이름이 다르므로 건드리지 않는다.
pub fn gc_session_logs(logs_root: &Path, budget_bytes: u64) -> anyhow::Result<u64> {
    gc_session_logs_with_limit_impl(logs_root, budget_bytes, SESSION_LOG_SCAN_ENTRY_LIMIT)
}

#[cfg(test)]
fn gc_session_logs_with_limit(
    logs_root: &Path,
    budget_bytes: u64,
    entry_limit: usize,
) -> anyhow::Result<u64> {
    gc_session_logs_with_limit_impl(logs_root, budget_bytes, entry_limit)
}

fn gc_session_logs_with_limit_impl(
    logs_root: &Path,
    budget_bytes: u64,
    entry_limit: usize,
) -> anyhow::Result<u64> {
    gc_session_logs_with_limit_and_hook(logs_root, budget_bytes, entry_limit, |_| {})
}

fn gc_session_logs_with_limit_and_hook(
    logs_root: &Path,
    budget_bytes: u64,
    entry_limit: usize,
    mut after_directory_pin: impl FnMut(&Path),
) -> anyhow::Result<u64> {
    gc_session_logs_with_limit_and_hooks(
        logs_root,
        budget_bytes,
        entry_limit,
        |_| {},
        &mut after_directory_pin,
    )
}

fn gc_session_logs_with_limit_and_hooks(
    logs_root: &Path,
    budget_bytes: u64,
    entry_limit: usize,
    mut before_directory_pin: impl FnMut(&Path),
    mut after_directory_pin: impl FnMut(&Path),
) -> anyhow::Result<u64> {
    #[cfg(not(unix))]
    {
        let _ = (
            logs_root,
            budget_bytes,
            entry_limit,
            &mut before_directory_pin,
            &mut after_directory_pin,
        );
        anyhow::bail!("session_log_descriptor_io_unsupported");
    }
    let slot = session_log_scan_slot(logs_root)?;
    let _operation = slot
        .operation
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let scan = scan_session_log_batch_with_limit(&slot, logs_root, entry_limit)?;
    let scan_complete = scan.complete;
    let bundles = match bundle_session_log_candidates(logs_root, scan.candidates, scan_complete) {
        Ok(bundles) => bundles,
        Err(error) => {
            if !scan_complete {
                reset_session_log_scan_cursor(&slot);
            }
            return Err(error);
        }
    };
    if !scan_complete {
        let remaining = match remove_session_log_bundles(
            logs_root,
            bundles,
            0,
            true,
            &mut before_directory_pin,
            &mut after_directory_pin,
        ) {
            Ok(remaining) => remaining,
            Err(error) => {
                reset_session_log_scan_cursor(&slot);
                return Err(error);
            }
        };
        if remaining > 0 {
            reset_session_log_scan_cursor(&slot);
        }
        remove_empty_directories(scan.directories, logs_root);
        anyhow::bail!("{}", SESSION_LOG_SCAN_LIMIT_ERROR);
    }
    gc_session_log_bundles(
        logs_root,
        bundles,
        budget_bytes,
        &mut before_directory_pin,
        &mut after_directory_pin,
    )
}

fn gc_session_log_bundles(
    logs_root: &Path,
    bundles: Vec<SessionLogBundle>,
    budget_bytes: u64,
    before_directory_pin: &mut impl FnMut(&Path),
    after_directory_pin: &mut impl FnMut(&Path),
) -> anyhow::Result<u64> {
    let total = remove_session_log_bundles(
        logs_root,
        bundles,
        budget_bytes,
        false,
        before_directory_pin,
        after_directory_pin,
    )?;
    if total > budget_bytes {
        anyhow::bail!("session_log_gc_budget_unmet");
    }
    Ok(total)
}

fn remove_session_log_bundles(
    logs_root: &Path,
    mut bundles: Vec<SessionLogBundle>,
    budget_bytes: u64,
    force_all: bool,
    before_directory_pin: &mut impl FnMut(&Path),
    after_directory_pin: &mut impl FnMut(&Path),
) -> anyhow::Result<u64> {
    let mut total = bundles.iter().try_fold(0u64, |total, bundle| {
        total
            .checked_add(bundle.bytes)
            .context("session log byte total overflow")
    })?;
    if !force_all && total <= budget_bytes {
        return Ok(total);
    }
    bundles.sort_by_key(|bundle| bundle.modified);
    for bundle in bundles {
        if !force_all && total <= budget_bytes {
            break;
        }
        let mut removed = 0u64;
        #[cfg(unix)]
        {
            let directory_path = bundle_dir(&bundle);
            before_directory_pin(&directory_path);
            let Some(directory) = pin_log_directory(logs_root, &directory_path, false)? else {
                tracing::warn!(path = %directory_path.display(), "세션 로그 GC 디렉터리 사라짐");
                continue;
            };
            anyhow::ensure!(
                bundle.directory_identity == Some(pinned_log_directory_identity(&directory)?),
                "session_log_directory_replaced"
            );
            after_directory_pin(&directory.path);
            for path in bundle.paths {
                let Some(name) = path.path.file_name() else {
                    tracing::warn!(path = %path.path.display(), "세션 로그 GC 파일명 없음");
                    continue;
                };
                match unlink_log_at(&directory, name, path.identity) {
                    Ok(true) | Ok(false) => removed = removed.saturating_add(path.len),
                    Err(error) => {
                        tracing::warn!(path = %path.path.display(), "세션 로그 GC 삭제 실패: {error:#}")
                    }
                }
            }
        }
        #[cfg(not(unix))]
        {
            before_directory_pin(&bundle_dir(&bundle));
            for path in bundle.paths {
                match std::fs::remove_file(&path.path) {
                    Ok(()) => removed = removed.saturating_add(path.len),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        removed = removed.saturating_add(path.len);
                    }
                    Err(error) => {
                        tracing::warn!(path = %path.path.display(), "세션 로그 GC 삭제 실패: {error:#}")
                    }
                }
            }
        }
        total = total.saturating_sub(removed);
        if removed > 0 {
            tracing::info!(bytes = removed, "오래된 세션 로그 GC — 전체 예산 초과 제거");
        }
    }
    Ok(total)
}

fn bundle_dir(bundle: &SessionLogBundle) -> PathBuf {
    bundle
        .paths
        .first()
        .and_then(|path| path.path.parent())
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

#[cfg(not(unix))]
fn path_len(path: &Path) -> anyhow::Result<u64> {
    path.metadata()
        .map(|metadata| metadata.len())
        .with_context(|| format!("세션 로그 길이 조회 실패: {}", path.display()))
}

#[cfg(test)]
fn collect_session_log_bundles_with_limit(
    logs_root: &Path,
    entry_limit: usize,
) -> anyhow::Result<Vec<SessionLogBundle>> {
    let scan = scan_session_log_candidates_with_limit(logs_root, entry_limit)?;
    if !scan.complete {
        anyhow::bail!("{}", SESSION_LOG_SCAN_LIMIT_ERROR);
    }
    bundle_session_log_candidates(logs_root, scan.candidates, true)
}

fn bundle_session_log_candidates(
    logs_root: &Path,
    candidates: Vec<SessionLogCandidate>,
    compact_oversized: bool,
) -> anyhow::Result<Vec<SessionLogBundle>> {
    let mut by_dir = std::collections::HashMap::<PathBuf, SessionLogBundle>::new();
    for candidate in candidates {
        #[cfg(unix)]
        let len = {
            let directory = pin_log_directory(logs_root, &candidate.dir, false)?
                .context("session log directory disappeared during scan")?;
            anyhow::ensure!(
                pinned_log_directory_identity(&directory)? == candidate.directory_identity,
                "session_log_directory_replaced"
            );
            let name = candidate
                .path
                .file_name()
                .context("session log file name missing")?;
            let mut file = open_regular_log_at(&directory, name, false)?;
            let opened = file.metadata()?;
            let identity = metadata_identity(&opened);
            anyhow::ensure!(
                identity.device == candidate.identity.device
                    && identity.inode == candidate.identity.inode,
                "session_log_file_replaced"
            );
            if compact_oversized && candidate.original_len > candidate.max_bytes {
                compact_open_file_to_tail(
                    &mut file,
                    candidate.retain_bytes,
                    candidate.tail_boundary,
                )
                .with_context(|| {
                    format!(
                        "기존 세션 로그 상한 적용 실패: {}",
                        candidate.path.display()
                    )
                })?;
            }
            file.metadata()?.len()
        };
        #[cfg(not(unix))]
        let len = {
            if compact_oversized && candidate.original_len > candidate.max_bytes {
                let mut file = open_regular_log_file(&candidate.path, false)?;
                compact_open_file_to_tail(
                    &mut file,
                    candidate.retain_bytes,
                    candidate.tail_boundary,
                )
                .with_context(|| {
                    format!(
                        "기존 세션 로그 상한 적용 실패: {}",
                        candidate.path.display()
                    )
                })?;
            }
            path_len(&candidate.path)?
        };
        let bundle = by_dir.entry(candidate.dir).or_default();
        #[cfg(unix)]
        {
            if let Some(identity) = bundle.directory_identity {
                anyhow::ensure!(
                    identity == candidate.directory_identity,
                    "session_log_directory_replaced"
                );
            } else {
                bundle.directory_identity = Some(candidate.directory_identity);
            }
        }
        bundle.paths.push(SessionLogPath {
            path: candidate.path,
            len,
            #[cfg(unix)]
            identity: candidate.identity,
        });
        bundle.bytes = bundle
            .bytes
            .checked_add(len)
            .context("session log bundle byte total overflow")?;
        bundle.modified = bundle.modified.max(candidate.modified);
    }
    Ok(by_dir.into_values().collect())
}

struct SessionLogScan {
    candidates: Vec<SessionLogCandidate>,
    directories: Vec<PathBuf>,
    entries_seen: usize,
    complete: bool,
}

struct SessionLogScanFrame {
    path: PathBuf,
    depth: usize,
    entries: std::fs::ReadDir,
    #[cfg(unix)]
    directory_identity: FileIdentity,
}

struct SessionLogScanCursor {
    stack: Vec<SessionLogScanFrame>,
    pending: Option<std::fs::DirEntry>,
    continued: bool,
    #[cfg(unix)]
    root_identity: FileIdentity,
}

const SESSION_LOG_SCAN_CURSOR_CAP: usize = 16;

struct SessionLogScanSlot {
    operation: std::sync::Mutex<()>,
    cursor: std::sync::Mutex<Option<SessionLogScanCursor>>,
}

type SessionLogScanSlots =
    std::sync::Mutex<std::collections::VecDeque<(PathBuf, std::sync::Arc<SessionLogScanSlot>)>>;

fn session_log_scan_slots() -> &'static SessionLogScanSlots {
    static CURSORS: std::sync::OnceLock<SessionLogScanSlots> = std::sync::OnceLock::new();
    CURSORS.get_or_init(|| std::sync::Mutex::new(std::collections::VecDeque::new()))
}

fn session_log_scan_slot(logs_root: &Path) -> anyhow::Result<std::sync::Arc<SessionLogScanSlot>> {
    let mut slots = session_log_scan_slots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(position) = slots.iter().position(|(root, _)| root == logs_root) {
        let (_, slot) = slots
            .remove(position)
            .context("session log scan slot disappeared")?;
        let result = std::sync::Arc::clone(&slot);
        slots.push_back((logs_root.to_path_buf(), slot));
        return Ok(result);
    }
    if slots.len() >= SESSION_LOG_SCAN_CURSOR_CAP {
        let Some(position) = slots
            .iter()
            .position(|(_, slot)| std::sync::Arc::strong_count(slot) == 1)
        else {
            anyhow::bail!("session_log_scan_cursor_capacity");
        };
        slots.remove(position);
    }
    let slot = std::sync::Arc::new(SessionLogScanSlot {
        operation: std::sync::Mutex::new(()),
        cursor: std::sync::Mutex::new(None),
    });
    slots.push_back((logs_root.to_path_buf(), std::sync::Arc::clone(&slot)));
    Ok(slot)
}

fn reset_session_log_scan_cursor(slot: &SessionLogScanSlot) {
    *slot
        .cursor
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

fn create_session_log_scan_cursor(
    logs_root: &Path,
) -> anyhow::Result<Option<SessionLogScanCursor>> {
    let before = match std::fs::symlink_metadata(logs_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("로그 루트 metadata 실패"),
    };
    anyhow::ensure!(
        before.file_type().is_dir(),
        "session_log_directory_not_regular"
    );
    let entries = std::fs::read_dir(logs_root).context("로그 디렉터리 나열 실패")?;
    let after = std::fs::symlink_metadata(logs_root).context("로그 루트 metadata 재조회 실패")?;
    anyhow::ensure!(
        after.file_type().is_dir(),
        "session_log_directory_not_regular"
    );
    #[cfg(unix)]
    anyhow::ensure!(
        metadata_identity(&before) == metadata_identity(&after),
        "session_log_root_replaced"
    );
    Ok(Some(SessionLogScanCursor {
        stack: vec![SessionLogScanFrame {
            path: logs_root.to_path_buf(),
            depth: 0,
            entries,
            #[cfg(unix)]
            directory_identity: metadata_identity(&after),
        }],
        pending: None,
        continued: false,
        #[cfg(unix)]
        root_identity: metadata_identity(&after),
    }))
}

#[cfg(unix)]
fn session_log_scan_root_matches(
    cursor: &SessionLogScanCursor,
    logs_root: &Path,
) -> anyhow::Result<bool> {
    let metadata = match std::fs::symlink_metadata(logs_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("로그 루트 metadata 실패"),
    };
    Ok(metadata.file_type().is_dir() && metadata_identity(&metadata) == cursor.root_identity)
}

fn scan_session_log_batch_with_limit(
    slot: &SessionLogScanSlot,
    logs_root: &Path,
    entry_limit: usize,
) -> anyhow::Result<SessionLogScan> {
    let mut cursor = slot
        .cursor
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    #[cfg(unix)]
    if let Some(active) = cursor.as_ref()
        && !session_log_scan_root_matches(active, logs_root)?
    {
        *cursor = None;
    }
    if cursor.is_none() {
        let Some(created) = create_session_log_scan_cursor(logs_root)? else {
            return Ok(SessionLogScan {
                candidates: Vec::new(),
                directories: Vec::new(),
                entries_seen: 0,
                complete: true,
            });
        };
        *cursor = Some(created);
    }
    let active = cursor.as_mut().context("session log scan cursor missing")?;
    let result = scan_session_log_cursor(active, entry_limit);
    let mut scan = match result {
        Ok(scan) => scan,
        Err(error) => {
            *cursor = None;
            return Err(error);
        }
    };
    #[cfg(unix)]
    if !session_log_scan_root_matches(active, logs_root)? {
        *cursor = None;
        anyhow::bail!("session_log_root_replaced");
    }
    if scan.complete && active.continued {
        scan.complete = false;
        *cursor = None;
    } else if scan.complete {
        *cursor = None;
    } else {
        active.continued = true;
    }
    Ok(scan)
}

fn scan_session_log_cursor(
    cursor: &mut SessionLogScanCursor,
    entry_limit: usize,
) -> anyhow::Result<SessionLogScan> {
    let mut scan = SessionLogScan {
        candidates: Vec::new(),
        directories: Vec::new(),
        entries_seen: 0,
        complete: true,
    };
    while let Some(frame) = cursor.stack.last_mut() {
        let entry = match cursor.pending.take() {
            Some(entry) => Some(Ok(entry)),
            None => frame.entries.next(),
        };
        let Some(entry) = entry else {
            cursor.stack.pop();
            continue;
        };
        let entry =
            entry.with_context(|| format!("로그 항목 조회 실패: {}", frame.path.display()))?;
        scan.entries_seen = scan.entries_seen.saturating_add(1);
        if scan.entries_seen > entry_limit {
            cursor.pending = Some(entry);
            scan.complete = false;
            break;
        }
        let parent = frame.path.clone();
        let depth = frame.depth;
        #[cfg(unix)]
        let directory_identity = frame.directory_identity;
        let file_type = entry
            .file_type()
            .with_context(|| format!("로그 항목 유형 조회 실패: {}", entry.path().display()))?;
        anyhow::ensure!(
            !file_type.is_symlink(),
            "session_log_subtree_not_regular: {}",
            entry.path().display()
        );
        if file_type.is_dir() && depth < 4 {
            let path = entry.path();
            let entries = std::fs::read_dir(&path)
                .with_context(|| format!("로그 디렉터리 나열 실패: {}", path.display()))?;
            #[cfg(unix)]
            let child_identity = {
                let metadata = path
                    .symlink_metadata()
                    .with_context(|| format!("로그 디렉터리 metadata 실패: {}", path.display()))?;
                anyhow::ensure!(
                    metadata.file_type().is_dir(),
                    "session_log_directory_not_regular"
                );
                metadata_identity(&metadata)
            };
            scan.directories.push(path.clone());
            cursor.stack.push(SessionLogScanFrame {
                path,
                depth: depth + 1,
                entries,
                #[cfg(unix)]
                directory_identity: child_identity,
            });
            continue;
        }
        let Some((max_bytes, retain_bytes, tail_boundary)) =
            log_limits_for_name(&entry.file_name())
        else {
            continue;
        };
        let path = entry.path();
        anyhow::ensure!(
            file_type.is_file(),
            "session_log_file_not_regular: {}",
            path.display()
        );
        let metadata = path
            .symlink_metadata()
            .with_context(|| format!("세션 로그 metadata 실패: {}", path.display()))?;
        anyhow::ensure!(
            metadata.file_type().is_file(),
            "session_log_file_not_regular: {}",
            path.display()
        );
        let modified = metadata
            .modified()
            .with_context(|| format!("세션 로그 수정시각 조회 실패: {}", path.display()))?;
        scan.candidates.push(SessionLogCandidate {
            dir: parent,
            path,
            original_len: metadata.len(),
            modified,
            max_bytes,
            retain_bytes,
            tail_boundary,
            #[cfg(unix)]
            identity: metadata_identity(&metadata),
            #[cfg(unix)]
            directory_identity,
        });
    }
    Ok(scan)
}

#[cfg(test)]
fn scan_session_log_candidates_with_limit(
    logs_root: &Path,
    entry_limit: usize,
) -> anyhow::Result<SessionLogScan> {
    let mut scan = SessionLogScan {
        candidates: Vec::new(),
        directories: Vec::new(),
        entries_seen: 0,
        complete: true,
    };
    scan_session_log_directory(logs_root, 0, entry_limit, &mut scan)?;
    Ok(scan)
}

#[cfg(test)]
fn scan_session_log_directory(
    dir: &Path,
    depth: usize,
    entry_limit: usize,
    scan: &mut SessionLogScan,
) -> anyhow::Result<()> {
    if !scan.complete {
        return Ok(());
    }
    if depth > 0 {
        scan.directories.push(dir.to_path_buf());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("로그 디렉터리 나열 실패: {}", dir.display()));
        }
    };
    #[cfg(unix)]
    let directory_identity = {
        let metadata = dir
            .symlink_metadata()
            .with_context(|| format!("로그 디렉터리 metadata 실패: {}", dir.display()))?;
        anyhow::ensure!(
            metadata.file_type().is_dir(),
            "session_log_directory_not_regular"
        );
        metadata_identity(&metadata)
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("로그 항목 조회 실패: {}", dir.display()))?;
        scan.entries_seen = scan.entries_seen.saturating_add(1);
        if scan.entries_seen > entry_limit {
            scan.complete = false;
            return Ok(());
        }
        let file_type = entry
            .file_type()
            .with_context(|| format!("로그 항목 유형 조회 실패: {}", entry.path().display()))?;
        anyhow::ensure!(
            !file_type.is_symlink(),
            "session_log_subtree_not_regular: {}",
            entry.path().display()
        );
        if file_type.is_dir() && depth < 4 {
            scan_session_log_directory(&entry.path(), depth + 1, entry_limit, scan)?;
            if !scan.complete {
                return Ok(());
            }
            continue;
        }
        let Some((max_bytes, retain_bytes, tail_boundary)) =
            log_limits_for_name(&entry.file_name())
        else {
            continue;
        };
        let path = entry.path();
        anyhow::ensure!(
            file_type.is_file(),
            "session_log_file_not_regular: {}",
            path.display()
        );
        let metadata = path
            .symlink_metadata()
            .with_context(|| format!("세션 로그 metadata 실패: {}", path.display()))?;
        anyhow::ensure!(
            metadata.file_type().is_file(),
            "session_log_file_not_regular: {}",
            path.display()
        );
        let modified = metadata
            .modified()
            .with_context(|| format!("세션 로그 수정시각 조회 실패: {}", path.display()))?;
        scan.candidates.push(SessionLogCandidate {
            dir: dir.to_path_buf(),
            path,
            original_len: metadata.len(),
            modified,
            max_bytes,
            retain_bytes,
            tail_boundary,
            #[cfg(unix)]
            identity: metadata_identity(&metadata),
            #[cfg(unix)]
            directory_identity,
        });
    }
    Ok(())
}

fn remove_empty_directories(mut directories: Vec<PathBuf>, logs_root: &Path) {
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    directories.dedup();
    for directory in directories {
        if directory != logs_root {
            let _ = std::fs::remove_dir(directory);
        }
    }
}

fn log_limits_for_name(name: &std::ffi::OsStr) -> Option<(u64, u64, TailBoundary)> {
    match name.to_str()? {
        ANSI_LOG_FILE => Some((
            ANSI_LOG_MAX_BYTES,
            ANSI_LOG_MAX_BYTES / 2,
            TailBoundary::Ansi,
        )),
        PLAIN_LOG_FILE => Some((
            PLAIN_LOG_MAX_BYTES,
            PLAIN_LOG_MAX_BYTES / 2,
            TailBoundary::NextNewlineIfPresent,
        )),
        EVENTS_LOG_FILE => Some((
            EVENTS_LOG_MAX_BYTES,
            EVENTS_LOG_MAX_BYTES / 2,
            TailBoundary::NextNewlineIfPresent,
        )),
        _ => None,
    }
}

/// 표시용 plain 텍스트 변환기 단계 — escape가 chunk 경계에 걸려도 이어간다.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
enum StripPhase {
    #[default]
    Ground,
    /// ESC 수신 직후 (다음 바이트로 종류 결정)
    Esc,
    /// CSI 본문 (최종 바이트 0x40..=0x7e까지)
    Csi,
    /// OSC 본문 (BEL 또는 ESC\ 까지)
    Osc,
    /// OSC 안에서 ESC 수신 (\이면 종료)
    OscEsc,
}

/// plain 변환기 상태 — 단계 + CSI 파라미터 + 현재 열.
#[derive(Debug, Clone, Default)]
pub(crate) struct StripState {
    phase: StripPhase,
    /// CSI 파라미터 바이트(0x30..=0x3f) 수집 — 커서 이동 폭 계산에 쓴다.
    params: Vec<u8>,
    /// 현재 출력 열(0-based). 커서 이동을 공백으로 되돌릴 때 기준.
    col: usize,
}

/// CSI 파라미터의 첫 숫자(기본값 1).
fn first_param(params: &[u8], default: usize) -> usize {
    let text = std::str::from_utf8(params).unwrap_or("");
    let head = text.split(';').next().unwrap_or("");
    head.parse::<usize>()
        .ok()
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

/// CSI/OSC/2바이트 escape를 제거하고 CR을 열 리셋으로 처리한다. 상태는 호출 간 유지된다.
///
/// **커서 가로 이동은 공백으로 되돌린다** (2026-07-17): claude/codex는 정렬에 공백이
/// 아니라 커서 이동을 쓴다 — `auto ESC[11G mode ESC[16G on`. escape를 통째로 버리면
/// `automodeon`이 되어 plain.txt를 사람이 읽을 수 없다(실측). CHA(`G`)는 목표 열까지,
/// CUF(`C`)는 n칸을 공백으로 채운다.
///
/// 열 계산은 바이트가 아니라 문자 단위다(UTF-8 연속 바이트는 세지 않는다). 전각(한글 등)을
/// 1칸으로 세므로 정렬이 완벽하진 않지만, 단어가 붙어버리는 것보다 훨씬 읽을 만하다 —
/// 이 파일은 사람이 읽는 로그이지 화면 재현이 아니다(화면 재현은 ansi.log 몫).
fn strip_ansi_stateful(buffer: &[u8], state: &mut StripState) -> Vec<u8> {
    let mut out = Vec::with_capacity(buffer.len());
    for &byte in buffer {
        match state.phase {
            StripPhase::Ground => match byte {
                0x1b => state.phase = StripPhase::Esc,
                b'\r' => state.col = 0,
                b'\n' => {
                    state.col = 0;
                    out.push(byte);
                }
                byte => {
                    // UTF-8 연속 바이트(10xxxxxx)는 같은 문자의 일부 — 열을 늘리지 않는다.
                    if byte & 0xc0 != 0x80 {
                        state.col += 1;
                    }
                    out.push(byte);
                }
            },
            StripPhase::Esc => match byte {
                b'[' => {
                    state.phase = StripPhase::Csi;
                    state.params.clear();
                }
                b']' => state.phase = StripPhase::Osc,
                // 2바이트 escape (ESC =, ESC > 등) — 이 바이트로 종료
                _ => state.phase = StripPhase::Ground,
            },
            StripPhase::Csi => {
                if (0x30..=0x3f).contains(&byte) {
                    // 파라미터는 몇 바이트 안 되지만, 깨진 스트림이 무한히 쌓이지 않게 상한.
                    if state.params.len() < 32 {
                        state.params.push(byte);
                    }
                } else if (0x40..=0x7e).contains(&byte) {
                    let pad = match byte {
                        // CHA — 절대 열로 이동(1-based). 뒤로 가는 이동은 공백으로 표현할 수
                        // 없으니 무시한다(덮어쓰기 재그리기라 어차피 내용이 이어진다).
                        b'G' => first_param(&state.params, 1).saturating_sub(1 + state.col),
                        // CUF — 상대 전진.
                        b'C' => first_param(&state.params, 1),
                        _ => 0,
                    };
                    out.resize(out.len() + pad, b' ');
                    state.col += pad;
                    state.phase = StripPhase::Ground;
                }
            }
            StripPhase::Osc => match byte {
                0x07 => state.phase = StripPhase::Ground,
                0x1b => state.phase = StripPhase::OscEsc,
                _ => {}
            },
            StripPhase::OscEsc => {
                // ESC\ 종결. 그 외 바이트는 OSC 본문 계속으로 취급
                state.phase = if byte == b'\\' {
                    StripPhase::Ground
                } else {
                    StripPhase::Osc
                };
            }
        }
    }
    out
}

fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn pr8_repeated_log_append_reuses_writer_length_and_measures_io() {
        let root = temp_root("pr8-counted-append");
        let path = root.join("bounded.log");
        let mut log = BoundedLogFile::open(&path, 1024 * 1024, TailBoundary::Ansi).unwrap();
        LOG_IO_COUNTS.with(|counts| counts.set((0, 0)));
        let started = std::time::Instant::now();
        for _ in 0..2048 {
            log.append(&[b'x'; 128]).unwrap();
        }
        log.flush();
        let elapsed = started.elapsed();
        let (metadata, writes) = LOG_IO_COUNTS.with(std::cell::Cell::get);
        eprintln!("PR8 file2048x128B metadata={metadata} write_all={writes} elapsed={elapsed:?}");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 2048 * 128);
        drop(log);
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            metadata <= 2,
            "metadata was re-read {metadata} times on append"
        );
    }

    #[test]
    fn pr8_failed_giant_append_never_exceeds_file_cap() {
        let root = temp_root("pr8-failed-giant");
        let path = root.join("bounded.log");
        let mut log = BoundedLogFile::open(&path, 64, TailBoundary::Ansi).unwrap();
        LOG_WRITE_FAIL_AFTER.with(|failure| failure.set(Some(75)));
        assert!(log.append(&[b'x'; 128]).is_err());
        let len = std::fs::metadata(&path).unwrap().len();
        drop(log);
        std::fs::remove_dir_all(root).unwrap();
        assert!(len <= 64, "failed giant append left {len} bytes beyond cap");
    }
    #[test]
    fn pr8_metadata_error_must_not_assume_empty_length_or_bypass_cap() {
        let root = temp_root("pr8-metadata-failure");
        let path = root.join("bounded.log");
        let mut log = BoundedLogFile::open(&path, 64, TailBoundary::Ansi).unwrap();
        log.append(&[b'a'; 48]).unwrap();
        LOG_METADATA_FAIL.with(|failure| failure.set(true));
        let result = log.append(&[b'b'; 32]);
        LOG_METADATA_FAIL.with(|failure| failure.set(false));
        let len = std::fs::metadata(&path).unwrap().len();
        drop(log);
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            result.is_err(),
            "metadata failure incorrectly reported append success"
        );
        assert!(len <= 64, "metadata failure left {len} bytes beyond cap");
    }

    #[test]
    fn pr8_external_append_and_truncate_reconcile_same_handle() {
        let root = temp_root("pr8-external-changes");
        let path = root.join("bounded.log");
        let mut log = BoundedLogFile::open(&path, 64, TailBoundary::Ansi).unwrap();
        log.append(&[b'a'; 32]).unwrap();
        let mut external = OpenOptions::new().append(true).open(&path).unwrap();
        external.write_all(&[b'b'; 32]).unwrap();
        log.append(b"c").unwrap();
        assert!(log.len().unwrap() <= 64);
        assert!(std::fs::read(&path).unwrap().ends_with(b"c"));
        external.set_len(0).unwrap();
        log.append(b"new").unwrap();
        assert_eq!(log.len().unwrap(), 3);
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        drop(external);
        drop(log);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pr8_external_append_then_partial_write_error_repairs_file_cap() {
        let root = temp_root("pr8-external-partial-cap");
        let path = root.join("bounded.log");
        let mut log = BoundedLogFile::open(&path, 64, TailBoundary::Ansi).unwrap();
        log.append(&[b'a'; 32]).unwrap();
        let mut external = OpenOptions::new().append(true).open(&path).unwrap();
        external.write_all(&[b'b'; 32]).unwrap();
        LOG_WRITE_FAIL_AFTER.with(|failure| failure.set(Some(5)));
        let result = log.append(&[b'c'; 20]);
        let bytes = std::fs::read(&path).unwrap();
        let length_unknown = log.len().is_err();
        drop(external);
        drop(log);
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            result.is_err(),
            "partial write incorrectly reported success"
        );
        assert!(length_unknown, "partial write retained a confirmed length");
        assert!(
            bytes.len() <= 64,
            "failed partial append left {} bytes beyond cap",
            bytes.len()
        );
        assert!(bytes.ends_with(&[b'c'; 5]));
        assert!(!bytes.ends_with(&[b'c'; 6]), "failed bytes were replayed");
    }

    #[test]
    fn pr8_position_error_repairs_actual_eof_and_keeps_original_error() {
        let root = temp_root("pr8-position-error-cap");
        let path = root.join("bounded.log");
        let mut log = BoundedLogFile::open(&path, 64, TailBoundary::Ansi).unwrap();
        log.append(&[b'a'; 32]).unwrap();
        let mut external = OpenOptions::new().append(true).open(&path).unwrap();
        external.write_all(&[b'b'; 32]).unwrap();
        LOG_POSITION_FAIL.with(|failure| failure.set(true));
        let error = log.append(&[b'c'; 20]).unwrap_err();
        assert!(format!("{error:#}").contains("fixture_position_failed"));
        assert!(log.len().is_err());
        let mut expected = vec![b'b'; 12];
        expected.extend_from_slice(&[b'c'; 20]);
        assert_eq!(std::fs::read(&path).unwrap(), expected);
        drop(external);
        drop(log);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pr8_error_cap_repair_failure_preserves_both_errors_and_unknown_length() {
        let root = temp_root("pr8-error-repair-failure");
        let path = root.join("bounded.log");
        let mut log = BoundedLogFile::open(&path, 64, TailBoundary::Ansi).unwrap();
        log.append(&[b'a'; 32]).unwrap();
        let mut external = OpenOptions::new().append(true).open(&path).unwrap();
        external.write_all(&[b'b'; 32]).unwrap();
        LOG_WRITE_FAIL_AFTER.with(|failure| failure.set(Some(5)));
        LOG_METADATA_FAIL.with(|failure| failure.set(true));
        let result = log.append(&[b'c'; 20]);
        LOG_METADATA_FAIL.with(|failure| failure.set(false));
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("fixture_partial_write"));
        assert!(error.contains("fixture_metadata_failed"));
        assert!(log.len().is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 69);
        log.append(b"d").unwrap();
        assert!(log.len().unwrap() <= 64);
        assert!(std::fs::read(&path).unwrap().ends_with(b"cccccd"));
        drop(external);
        drop(log);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn pr8_error_cap_repair_preserves_pinned_inode_and_tail_boundary() {
        for (index, external_bytes, expected_tail) in [
            ("bbbb\x1b]title-\x07😀한글-endTAIL!", "😀한글-endTAIL!"),
            ("bbbb😀한글abcdefghijklmnopqr", "한글abcdefghijklmnopqr"),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (external, tail))| (i, external, tail))
        {
            assert_eq!(external_bytes.len(), 32);
            let root = temp_root(&format!("pr8-error-pinned-boundary-{index}"));
            let path = root.join("bounded.log");
            let mut log = BoundedLogFile::open(&path, 64, TailBoundary::Ansi).unwrap();
            log.append(&[b'a'; 32]).unwrap();
            let mut external = OpenOptions::new().append(true).open(&path).unwrap();
            external.write_all(external_bytes.as_bytes()).unwrap();
            let pinned = root.join("pinned.log");
            let unrelated = root.join("unrelated.log");
            std::fs::rename(&path, &pinned).unwrap();
            std::fs::write(&unrelated, b"unrelated").unwrap();
            std::os::unix::fs::symlink(&unrelated, &path).unwrap();
            LOG_WRITE_FAIL_AFTER.with(|failure| failure.set(Some(5)));
            assert!(log.append(&[b'c'; 20]).is_err());
            assert!(log.len().is_err());
            let mut expected = expected_tail.as_bytes().to_vec();
            expected.extend_from_slice(&[b'c'; 5]);
            assert_eq!(std::fs::read(&pinned).unwrap(), expected);
            assert_eq!(std::fs::read(&unrelated).unwrap(), b"unrelated");
            drop(external);
            drop(log);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn pr8_partial_write_recovery_uses_actual_length_and_preserves_cap() {
        let root = temp_root("pr8-partial-recovery");
        let path = root.join("bounded.log");
        let mut log = BoundedLogFile::open(&path, 64, TailBoundary::Ansi).unwrap();
        log.append(&[b'a'; 40]).unwrap();
        LOG_WRITE_FAIL_AFTER.with(|failure| failure.set(Some(5)));
        assert!(log.append(&[b'b'; 20]).is_err());
        assert!(log.len().is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 45);
        log.append(&[b'c'; 20]).unwrap();
        assert!(log.len().unwrap() <= 64);
        assert!(std::fs::read(&path).unwrap().ends_with(&[b'c'; 20]));
        drop(log);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pr8_giant_tail_preserves_split_escape_utf8_and_pinned_inode() {
        for (index, prefix, incoming) in [
            (
                &b"old\x1b]title"[..],
                "😀한글\x07abcdef😀한글\x1b[31mred\x1b[0m-end".as_bytes(),
            ),
            (&b"\x1b["[..], &b"31mABCDEFGHIJKLMNO\x1b[0mTAIL-END"[..]),
            (
                &b"old"[..],
                "가나다라마바사아자차카타파하😀😀end".as_bytes(),
            ),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (p, b))| (i, p, b))
        {
            let root = temp_root(&format!("pr8-giant-boundary-{index}"));
            let path = root.join("bounded.log");
            let expected_path = root.join("expected.log");
            let mut combined = prefix.to_vec();
            combined.extend_from_slice(incoming);
            std::fs::write(&expected_path, combined).unwrap();
            let mut expected = open_regular_log_file(&expected_path, false).unwrap();
            compact_open_file_to_tail(&mut expected, 24, TailBoundary::Ansi).unwrap();
            let expected_bytes = std::fs::read(&expected_path).unwrap();
            let mut log = BoundedLogFile::open(&path, 24, TailBoundary::Ansi).unwrap();
            log.append(prefix).unwrap();
            let pinned = root.join("pinned.log");
            std::fs::rename(&path, &pinned).unwrap();
            std::fs::write(&path, b"replacement").unwrap();
            log.append(incoming).unwrap();
            assert_eq!(std::fs::read(&pinned).unwrap(), expected_bytes);
            assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
            drop(log);
            drop(expected);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn pr8_measure_append_fixture_five_samples() {
        let mut samples = Vec::new();
        for n in 0..5 {
            let root = temp_root(&format!("pr8-median-{n}"));
            let path = root.join("bounded.log");
            let mut log = BoundedLogFile::open(&path, 1024 * 1024, TailBoundary::Ansi).unwrap();
            LOG_IO_COUNTS.with(|count| count.set((0, 0)));
            let started = std::time::Instant::now();
            for _ in 0..2048 {
                log.append(&[b'x'; 128]).unwrap();
            }
            log.flush();
            let us = started.elapsed().as_secs_f64() * 1e6;
            let counts = LOG_IO_COUNTS.with(std::cell::Cell::get);
            assert_eq!(std::fs::metadata(&path).unwrap().len(), 2048 * 128);
            eprintln!(
                "PR8 sample{n} file2048x128B metadata={} write_all={} us={us:.3}",
                counts.0, counts.1
            );
            samples.push(us);
            drop(log);
            std::fs::remove_dir_all(root).unwrap();
        }
        samples.sort_by(f64::total_cmp);
        eprintln!("PR8 median5 file2048x128B us={:.3}", samples[2]);
    }

    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deppy-logs-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn escape가_chunk_경계에_걸려도_plain은_깨끗() {
        let root = temp_root("split-esc");
        let session = SessionId(9);
        let mut writer = SessionLogWriter::open(&root, session).unwrap();
        // CSI가 두 chunk에 걸침: "\x1b[3" + "1mRED\x1b[0m ok"
        writer.append_output(b"pre \x1b[3").unwrap();
        writer.append_output(b"1mRED\x1b[0m ok").unwrap();
        // OSC가 걸침: "\x1b]0;ti" + "tle\x07after"
        writer.append_output(b" \x1b]0;ti").unwrap();
        writer.append_output(b"tle\x07after").unwrap();
        writer.flush();
        let plain = std::fs::read_to_string(
            SessionLogWriter::session_dir(&root, session).join("redacted.plain.txt"),
        )
        .unwrap();
        assert_eq!(plain, "pre RED ok after");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn 세_파일_생성과_append() {
        let root = temp_root("basic");
        let session = SessionId(7);
        {
            let mut writer = SessionLogWriter::open(&root, session).unwrap();
            writer
                .append_output(b"line1 \x1b[31mred\x1b[0m\r\n")
                .unwrap();
            writer.append_event("spawned", None).unwrap();
            writer.flush();
        }
        // 재오픈 후 추가 기록 — 기존 내용 보존 (append-only)
        {
            let mut writer = SessionLogWriter::open(&root, session).unwrap();
            writer.append_output(b"line2\n").unwrap();
            writer.append_event("exited", Some("exit code 0")).unwrap();
            writer.flush();
        }
        let dir = SessionLogWriter::session_dir(&root, session);
        let ansi = std::fs::read(dir.join("redacted.ansi.log")).unwrap();
        assert!(ansi.windows(5).any(|w| w == b"line1"));
        assert!(ansi.windows(5).any(|w| w == b"\x1b[31m")); // ansi.log는 escape 보존
        assert!(ansi.windows(5).any(|w| w == b"line2"));

        let plain = std::fs::read_to_string(dir.join("redacted.plain.txt")).unwrap();
        assert_eq!(plain, "line1 red\nline2\n"); // escape/CR 제거

        let events = std::fs::read_to_string(dir.join("events.redacted.jsonl")).unwrap();
        let lines: Vec<&str> = events.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"event\":\"spawned\""));
        assert!(lines[1].contains("exit code 0"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn 영속_session_key는_ansi를_이어쓰고_경로탈출을_거부한다() {
        let root = temp_root("persistent-key");
        let key = "019f3804-586d-7ca3-9386-1cbc8710ca08";
        {
            let mut writer = SessionLogWriter::open_key(&root, key).unwrap();
            writer.append_output(b"\x1b[36mkept\x1b[0m").unwrap();
            writer.flush();
        }
        assert_eq!(
            std::fs::read(SessionLogWriter::ansi_path(&root, key).unwrap()).unwrap(),
            b"\x1b[36mkept\x1b[0m"
        );
        for invalid in ["", ".", "..", "../escape", "nested/session", "/tmp/escape"] {
            assert!(SessionLogWriter::session_dir_key(&root, invalid).is_err());
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn log_and_terminal_writes_reject_symlinked_session_directory() {
        use std::os::unix::fs::symlink;

        let root = temp_root("symlinked-session-directory");
        let outside = temp_root("symlinked-session-directory-outside");
        symlink(&outside, root.join("session")).unwrap();

        assert!(SessionLogWriter::open_key(&root, "session").is_err());
        assert!(SessionLogWriter::save_terminal_size(&root, "session", 80, 24).is_err());
        assert!(!outside.join(ANSI_LOG_FILE).exists());
        assert!(!outside.join("terminal.size").exists());
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn log_and_terminal_writes_stay_in_pinned_directory_after_parent_swap() {
        use std::os::unix::fs::symlink;

        let log_root = temp_root("pinned-log-parent-swap");
        let log_session = log_root.join("session");
        let pinned_log_session = log_root.join("pinned-session");
        let log_outside = temp_root("pinned-log-parent-swap-outside");
        std::fs::create_dir_all(&log_session).unwrap();
        let mut writer = SessionLogWriter::open_key_with_session_hook(&log_root, "session", || {
            std::fs::rename(&log_session, &pinned_log_session).unwrap();
            symlink(&log_outside, &log_session).unwrap();
        })
        .unwrap();
        writer.append_output(b"safe").unwrap();
        writer.flush();
        assert_eq!(
            std::fs::read(pinned_log_session.join(ANSI_LOG_FILE)).unwrap(),
            b"safe"
        );
        assert!(!log_outside.join(ANSI_LOG_FILE).exists());

        let size_root = temp_root("pinned-size-parent-swap");
        let size_session = size_root.join("session");
        let pinned_size_session = size_root.join("pinned-session");
        let size_outside = temp_root("pinned-size-parent-swap-outside");
        std::fs::create_dir_all(&size_session).unwrap();
        SessionLogWriter::save_terminal_size_with_session_hook(
            &size_root,
            "session",
            80,
            24,
            || {
                std::fs::rename(&size_session, &pinned_size_session).unwrap();
                symlink(&size_outside, &size_session).unwrap();
            },
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(pinned_size_session.join("terminal.size")).unwrap(),
            "80 24\n"
        );
        assert!(!size_outside.join("terminal.size").exists());

        std::fs::remove_file(log_session).unwrap();
        std::fs::remove_dir_all(pinned_log_session).unwrap();
        std::fs::remove_dir_all(log_root).unwrap();
        std::fs::remove_dir_all(log_outside).unwrap();
        std::fs::remove_file(size_session).unwrap();
        std::fs::remove_dir_all(pinned_size_session).unwrap();
        std::fs::remove_dir_all(size_root).unwrap();
        std::fs::remove_dir_all(size_outside).unwrap();
    }

    #[test]
    fn 터미널_크기_sidecar를_저장하고_검증한다() {
        let root = temp_root("terminal-size");
        let key = "019f3804-586d-7ca3-9386-1cbc8710ca08";

        assert_eq!(
            SessionLogWriter::load_terminal_size(&root, key).unwrap(),
            None
        );
        SessionLogWriter::save_terminal_size(&root, key, 121, 47).unwrap();
        assert_eq!(
            SessionLogWriter::load_terminal_size(&root, key).unwrap(),
            Some((121, 47))
        );

        std::fs::write(
            SessionLogWriter::terminal_size_path(&root, key).unwrap(),
            "65535 47\n",
        )
        .unwrap();
        assert!(SessionLogWriter::load_terminal_size(&root, key).is_err());
        assert!(SessionLogWriter::save_terminal_size(&root, key, 0, 47).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn terminal_size_reader는_byte상한과_snapshot변경을_거부한다() {
        let root = temp_root("terminal-size-bounds");
        let path = root.join("terminal.size");
        std::fs::write(&path, vec![b'x'; TERMINAL_SIZE_BYTES_MAX]).unwrap();
        assert_eq!(
            read_terminal_size_bounded(&path).unwrap().unwrap().len(),
            TERMINAL_SIZE_BYTES_MAX
        );
        std::fs::write(&path, vec![b'x'; TERMINAL_SIZE_BYTES_MAX + 1]).unwrap();
        assert_eq!(
            read_terminal_size_bounded(&path).unwrap_err().to_string(),
            "terminal_size_bytes_exceeded"
        );

        std::fs::write(&path, b"121 47\n").unwrap();
        assert_eq!(
            read_terminal_size_bounded_with_hook(&path, || {
                OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(b"1")
                    .unwrap();
            })
            .unwrap_err()
            .to_string(),
            "terminal_size_changed"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn terminal_size_reader는_symlink와_fifo를_block없이_거부한다() {
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::symlink;

        let root = temp_root("terminal-size-types");
        let target = root.join("target");
        let link = root.join("link");
        std::fs::write(&target, b"80 24\n").unwrap();
        symlink(&target, &link).unwrap();
        assert_eq!(
            read_terminal_size_bounded(&link).unwrap_err().to_string(),
            "terminal_size_not_regular"
        );

        let fifo = root.join("fifo");
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        assert_eq!(
            read_terminal_size_bounded(&fifo).unwrap_err().to_string(),
            "terminal_size_not_regular"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn bounded_log_open과_gc는_symlink와_fifo를_block없이_거부한다() {
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::symlink;

        let root = temp_root("bounded-log-types");
        let target = root.join("target.log");
        let link = root.join("link.log");
        std::fs::write(&target, b"safe").unwrap();
        symlink(&target, &link).unwrap();
        assert!(open_regular_log_file(&link, true).is_err());
        assert!(open_regular_log_file(&link, false).is_err());

        let fifo = root.join("fifo.log");
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        assert!(open_regular_log_file(&fifo, true).is_err());
        assert!(open_regular_log_file(&fifo, false).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));

        let regular = root.join("regular.log");
        open_regular_log_file(&regular, true)
            .unwrap()
            .write_all(b"created")
            .unwrap();
        assert_eq!(std::fs::read(&regular).unwrap(), b"created");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn log_compaction은_snapshot뒤_append를_truncate하지_않는다() {
        let root = temp_root("compaction-append");
        let path = root.join("bounded.log");
        std::fs::write(&path, vec![b'a'; 128]).unwrap();
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .unwrap();
        assert_eq!(
            compact_open_file_to_tail_with_hook(
                &mut file,
                64,
                TailBoundary::NextNewlineIfPresent,
                || {
                    OpenOptions::new()
                        .append(true)
                        .open(&path)
                        .unwrap()
                        .write_all(b"late")
                        .unwrap();
                },
            )
            .unwrap_err()
            .kind(),
            std::io::ErrorKind::Other
        );
        let retained = std::fs::read(&path).unwrap();
        assert_eq!(retained.len(), 132);
        assert!(retained.ends_with(b"late"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn production_log_restore와_compaction은_eof_following_read를_금지한다() {
        let production = include_str!("logs.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        assert!(!production.contains("std::fs::read_to_string(&path)"));
        assert!(!production.contains("file.read_to_end(&mut tail)"));
        assert!(production.contains("file.read_exact(&mut tail)"));
        assert!(production.contains("TERMINAL_SIZE_BYTES_MAX"));
        assert!(production.contains("open_regular_log_file(path, true)"));
        assert!(production.contains("open_regular_log_file(&candidate.path, false)"));
    }

    #[test]
    fn bounded_log는_상한을_넘으면_최근_완전한_줄만_남긴다() {
        let root = temp_root("bounded-tail");
        let path = root.join("bounded.log");
        let mut log = BoundedLogFile::open(&path, 64, TailBoundary::NextNewlineIfPresent).unwrap();
        for index in 0..12 {
            log.append(format!("line-{index:02}-payload\n").as_bytes())
                .unwrap();
        }
        log.flush();

        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.len() <= 64, "파일별 상한을 넘으면 안 됨");
        assert!(bytes.starts_with(b"line-"), "중간 줄에서 시작하면 안 됨");
        assert!(bytes.ends_with(b"line-11-payload\n"));
        assert!(!bytes.windows(7).any(|window| window == b"line-00"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn ansi_bounded_log는_lf가_없어도_최근_tail을_보존한다() {
        let root = temp_root("bounded-ansi-no-lf");
        let path = root.join("bounded.log");
        let mut log = BoundedLogFile::open(&path, 64, TailBoundary::Ansi).unwrap();
        log.append(&[b'a'; 48]).unwrap();
        log.append(&[b'b'; 48]).unwrap();
        log.flush();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 64);
        assert_eq!(&bytes[..16], &[b'a'; 16]);
        assert_eq!(&bytes[16..], &[b'b'; 48]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn ansi_tail은_csi_osc와_utf8_중간에서_시작하지_않는다() {
        fn compact(data: &[u8], requested_start: usize) -> Vec<u8> {
            let root = temp_root("ansi-safe-boundary");
            let path = root.join(format!("{}.log", requested_start));
            std::fs::write(&path, data).unwrap();
            let mut file = OpenOptions::new()
                .read(true)
                .append(true)
                .open(&path)
                .unwrap();
            compact_open_file_to_tail(
                &mut file,
                (data.len() - requested_start) as u64,
                TailBoundary::Ansi,
            )
            .unwrap();
            let result = std::fs::read(&path).unwrap();
            std::fs::remove_dir_all(root).unwrap();
            result
        }

        let csi = b"prefix\x1b[38;2;12;34;56mVISIBLE";
        assert_eq!(compact(csi, 10), b"VISIBLE");

        let osc = b"prefix\x1b]0;window title\x07BODY";
        assert_eq!(compact(osc, 12), b"BODY");

        let utf8 = "prefix한글-tail".as_bytes();
        assert_eq!(
            std::str::from_utf8(&compact(utf8, "prefix".len() + 1)).unwrap(),
            "글-tail"
        );
    }

    #[test]
    fn ansi_snapshot_boundary는_append를_읽지_않고_shrink를_거부한다() {
        let root = temp_root("ansi-frozen-snapshot");
        let path = root.join("snapshot.log");
        let original = b"prefix\nVISIBLE";
        std::fs::write(&path, original).unwrap();
        let snapshot_end = original.len() as u64;

        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(&vec![b'x'; 128 * 1024]).unwrap();
        let start = seek_ansi_tail_boundary_snapshot(&mut file, 3, snapshot_end, true).unwrap();
        assert_eq!(start, b"prefix\n".len() as u64);
        let mut snapshot_tail = vec![0_u8; original.len() - start as usize];
        file.read_exact(&mut snapshot_tail).unwrap();
        assert_eq!(snapshot_tail, b"VISIBLE");

        file.set_len(snapshot_end - 1).unwrap();
        assert_eq!(
            seek_ansi_tail_boundary_snapshot(&mut file, 0, snapshot_end, false)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::UnexpectedEof
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn 전체_gc는_오래된_세션_로그만_지우고_다른_리소스는_보존한다() {
        let root = temp_root("global-gc");
        let app_log = root.join("app.log.2026-07-19");
        std::fs::write(&app_log, b"application log").unwrap();

        for (index, key) in ["old", "new"].iter().enumerate() {
            let dir = root.join("workspace").join(key);
            std::fs::create_dir_all(&dir).unwrap();
            for name in [ANSI_LOG_FILE, PLAIN_LOG_FILE, EVENTS_LOG_FILE] {
                let path = dir.join(name);
                std::fs::write(&path, vec![b'x'; 40]).unwrap();
                let file = OpenOptions::new().append(true).open(&path).unwrap();
                file.set_modified(
                    std::time::UNIX_EPOCH
                        + std::time::Duration::from_secs(1_000 + index as u64 * 1_000),
                )
                .unwrap();
            }
            std::fs::write(dir.join("scrollback.zlib"), b"archive").unwrap();
            std::fs::write(dir.join("terminal.size"), b"120 40\n").unwrap();
        }

        let total = gc_session_logs(&root, 150).unwrap();
        assert_eq!(total, 120);
        for name in [ANSI_LOG_FILE, PLAIN_LOG_FILE, EVENTS_LOG_FILE] {
            assert!(!root.join("workspace/old").join(name).exists());
            assert!(root.join("workspace/new").join(name).exists());
        }
        assert!(root.join("workspace/old/scrollback.zlib").exists());
        assert!(root.join("workspace/old/terminal.size").exists());
        assert!(app_log.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn scan_entry_limit_exact_boundary_succeeds() {
        let root = temp_root("scan-entry-limit-exact");
        for index in 0..2 {
            let dir = root.join(format!("session-{index}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(PLAIN_LOG_FILE), b"log").unwrap();
        }

        let bundles = collect_session_log_bundles_with_limit(&root, 4).unwrap();

        assert_eq!(bundles.len(), 2);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn scan_entry_limit_default_exact_boundary_succeeds() {
        let root = temp_root("scan-entry-limit-default-exact");
        for index in 0..SESSION_LOG_SCAN_ENTRY_LIMIT / 2 {
            let dir = root.join(format!("session-{index}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(PLAIN_LOG_FILE), b"").unwrap();
        }

        let total = gc_session_logs(&root, 0).unwrap();

        assert_eq!(total, 0);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn scan_entry_limit_rejects_limit_plus_one() {
        let root = temp_root("scan-entry-limit-plus-one");
        for index in 0..3 {
            let dir = root.join(format!("session-{index}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(PLAIN_LOG_FILE), b"log").unwrap();
        }

        let err = match collect_session_log_bundles_with_limit(&root, 4) {
            Ok(_) => panic!("expected scan entry limit error"),
            Err(error) => error,
        };

        assert_eq!(err.to_string(), SESSION_LOG_SCAN_LIMIT_ERROR);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn scan_entry_limit_default_rejects_limit_plus_one() {
        let root = temp_root("scan-entry-limit-default-plus-one");
        for index in 0..SESSION_LOG_SCAN_ENTRY_LIMIT / 2 {
            let dir = root.join(format!("session-{index}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(PLAIN_LOG_FILE), b"").unwrap();
        }
        std::fs::write(root.join("one-entry-over-limit"), b"noise").unwrap();

        let err = gc_session_logs(&root, 0).unwrap_err();

        assert_eq!(err.to_string(), SESSION_LOG_SCAN_LIMIT_ERROR);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn scan_entry_limit_reports_incomplete_after_bounded_progress() {
        let root = temp_root("scan-entry-limit-no-mutation");
        let session_dir = root.join("session");
        std::fs::create_dir_all(&session_dir).unwrap();
        let oversized_log = session_dir.join(ANSI_LOG_FILE);
        let original = vec![b'x'; (ANSI_LOG_MAX_BYTES + 1) as usize];
        std::fs::write(&oversized_log, &original).unwrap();
        std::fs::write(session_dir.join(PLAIN_LOG_FILE), b"plain").unwrap();
        std::fs::write(session_dir.join(EVENTS_LOG_FILE), b"event").unwrap();

        let err = gc_session_logs_with_limit(&root, 0, 2).unwrap_err();

        assert_eq!(err.to_string(), SESSION_LOG_SCAN_LIMIT_ERROR);
        assert!(!oversized_log.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn non_log_entries_consume_bounded_scan_budget() {
        let root = temp_root("scan-entry-limit-noise");
        for index in 0..5 {
            std::fs::write(root.join(format!("noise-{index}")), b"ignored").unwrap();
        }

        let error = match collect_session_log_bundles_with_limit(&root, 4) {
            Ok(_) => panic!("expected scan entry limit error"),
            Err(error) => error,
        };

        assert_eq!(error.to_string(), SESSION_LOG_SCAN_LIMIT_ERROR);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn repeated_gc_advances_past_non_log_batches() {
        let root = temp_root("scan-entry-noise-progress");
        for index in 0..5 {
            std::fs::write(root.join(format!("noise-{index}")), b"ignored").unwrap();
        }
        let session = root.join("session");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(session.join(PLAIN_LOG_FILE), b"remove-me").unwrap();

        let mut reported_success = false;
        for _ in 0..16 {
            if matches!(gc_session_logs_with_limit(&root, 0, 2), Ok(0)) {
                reported_success = true;
                break;
            }
        }

        assert!(!session.join(PLAIN_LOG_FILE).exists());
        assert!(
            !reported_success,
            "an over-limit noise tree cannot produce a complete usage snapshot"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn bounded_gc_discards_cursor_after_logs_root_replacement() {
        let root = temp_root("scan-root-replacement");
        for index in 0..3 {
            std::fs::write(root.join(format!("noise-{index}")), b"ignored").unwrap();
        }
        assert!(gc_session_logs_with_limit(&root, 0, 2).is_err());
        let old_root = root.with_extension("old");
        std::fs::rename(&root, &old_root).unwrap();
        let session = root.join("replacement-session");
        std::fs::create_dir_all(&session).unwrap();
        let replacement_log = session.join(PLAIN_LOG_FILE);
        std::fs::write(&replacement_log, b"remove-me").unwrap();

        let result = gc_session_logs_with_limit(&root, 0, 2);

        assert_eq!(result.unwrap(), 0);
        assert!(
            !replacement_log.exists(),
            "a stale cursor must not publish usage for a replacement root"
        );
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(old_root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn gc_unlinks_from_pinned_directory_after_parent_swap() {
        use std::os::unix::fs::symlink;

        let root = temp_root("gc-pinned-parent-swap");
        let session = root.join("session");
        let pinned_session = root.join("pinned-session");
        let outside = temp_root("gc-pinned-parent-swap-outside");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(session.join(PLAIN_LOG_FILE), b"remove-me").unwrap();
        std::fs::write(outside.join(PLAIN_LOG_FILE), b"keep-me").unwrap();
        let mut swapped = false;

        let total = gc_session_logs_with_limit_and_hook(&root, 0, 16, |directory| {
            if !swapped && directory == session {
                std::fs::rename(&session, &pinned_session).unwrap();
                symlink(&outside, &session).unwrap();
                swapped = true;
            }
        })
        .unwrap();

        assert_eq!(total, 0);
        assert!(!pinned_session.join(PLAIN_LOG_FILE).exists());
        assert_eq!(
            std::fs::read(outside.join(PLAIN_LOG_FILE)).unwrap(),
            b"keep-me"
        );
        std::fs::remove_file(session).unwrap();
        std::fs::remove_dir_all(pinned_session).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn gc_rejects_session_directory_replacement_before_repin() {
        let root = temp_root("gc-parent-replaced-before-repin");
        let session = root.join("session");
        let moved_session = root.join("moved-session");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(session.join(PLAIN_LOG_FILE), b"original").unwrap();

        let result = gc_session_logs_with_limit_and_hooks(
            &root,
            0,
            16,
            |directory| {
                if directory == session {
                    std::fs::rename(&session, &moved_session).unwrap();
                    std::fs::create_dir_all(&session).unwrap();
                    std::fs::write(session.join(PLAIN_LOG_FILE), b"replacement").unwrap();
                }
            },
            |_| {},
        );

        assert!(result.is_err(), "replaced parent identity must fail closed");
        assert_eq!(
            std::fs::read(moved_session.join(PLAIN_LOG_FILE)).unwrap(),
            b"original"
        );
        assert_eq!(
            std::fs::read(session.join(PLAIN_LOG_FILE)).unwrap(),
            b"replacement"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn session_log_scan_fails_closed_for_nonregular_subtree() {
        use std::os::unix::fs::symlink;

        let root = temp_root("scan-nonregular-subtree");
        let outside = temp_root("scan-nonregular-subtree-outside");
        std::fs::write(outside.join(PLAIN_LOG_FILE), b"log").unwrap();
        symlink(&outside, root.join("linked-session")).unwrap();

        assert!(gc_session_logs(&root, 0).is_err());
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn session_log_total_overflow_is_explicit() {
        let bundles = vec![
            SessionLogBundle {
                paths: Vec::new(),
                bytes: u64::MAX,
                modified: std::time::UNIX_EPOCH,
                #[cfg(unix)]
                directory_identity: None,
            },
            SessionLogBundle {
                paths: Vec::new(),
                bytes: 1,
                modified: std::time::UNIX_EPOCH,
                #[cfg(unix)]
                directory_identity: None,
            },
        ];

        assert!(
            gc_session_log_bundles(Path::new("."), bundles, u64::MAX, &mut |_| {}, &mut |_| {},)
                .is_err()
        );
    }

    #[test]
    fn over_limit_gc_makes_bounded_progress_until_recovered() {
        let root = temp_root("scan-entry-limit-progress");
        let mut log_paths = Vec::new();
        for index in 0..5 {
            let dir = root.join(format!("session-{index}"));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(PLAIN_LOG_FILE);
            std::fs::write(&path, b"log").unwrap();
            log_paths.push(path);
        }

        let first = gc_session_logs_with_limit(&root, 0, 4);

        assert!(
            first.is_err(),
            "incomplete bounded scan must remain explicit"
        );
        assert!(
            log_paths.iter().filter(|path| path.exists()).count() < log_paths.len(),
            "an over-limit pass must delete at least one bounded batch"
        );
        for _ in 0..5 {
            if matches!(gc_session_logs_with_limit(&root, 0, 4), Ok(0)) {
                break;
            }
        }
        assert!(
            log_paths.iter().all(|path| !path.exists()),
            "repeated bounded passes must eventually recover"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn incomplete_gc_restarts_after_failed_batch_cleanup() {
        let root = temp_root("scan-entry-limit-failed-cleanup");
        let mut log_paths = Vec::new();
        for index in 0..3 {
            let dir = root.join(format!("session-{index}"));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(PLAIN_LOG_FILE);
            std::fs::write(&path, b"log").unwrap();
            log_paths.push(path);
        }
        let mut swapped = None;

        let first = gc_session_logs_with_limit_and_hooks(
            &root,
            0,
            2,
            |directory| {
                if swapped.is_none() {
                    let moved = root.join("moved-session");
                    std::fs::rename(directory, &moved).unwrap();
                    std::fs::create_dir_all(directory).unwrap();
                    std::fs::write(directory.join(PLAIN_LOG_FILE), b"replacement").unwrap();
                    swapped = Some((directory.to_path_buf(), moved));
                }
            },
            |_| {},
        );

        assert!(first.is_err());
        let (session, moved) = swapped.expect("cleanup hook must replace one session");
        std::fs::remove_dir_all(&session).unwrap();
        std::fs::rename(moved, session).unwrap();
        for _ in 0..8 {
            if matches!(gc_session_logs_with_limit(&root, 0, 2), Ok(0)) {
                break;
            }
        }
        assert!(
            log_paths.iter().all(|path| !path.exists()),
            "a failed incomplete batch must be rescanned before GC reports success"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn over_limit_gc_removes_zero_byte_log_candidates() {
        let root = temp_root("scan-entry-limit-zero-byte");
        let mut log_paths = Vec::new();
        for index in 0..5 {
            let dir = root.join(format!("session-{index}"));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(PLAIN_LOG_FILE);
            std::fs::write(&path, b"").unwrap();
            log_paths.push(path);
        }

        let _ = gc_session_logs_with_limit(&root, 0, 4);

        assert!(log_paths.iter().any(|path| !path.exists()));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn gc_errors_when_delete_failure_leaves_logs_over_budget() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("global-gc-delete-failure");
        let session_dir = root.join("locked");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join(PLAIN_LOG_FILE), b"log").unwrap();
        std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let result = gc_session_logs(&root, 0);

        std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            result.is_err(),
            "GC must not report success while retained logs exceed the budget"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_gc_scan_compacts_oversized_logs_before_budget_check() {
        let root = temp_root("global-gc-oversized-file");
        let session_dir = root.join("session");
        std::fs::create_dir_all(&session_dir).unwrap();
        let path = session_dir.join(PLAIN_LOG_FILE);
        std::fs::write(&path, vec![b'x'; (PLAIN_LOG_MAX_BYTES + 1) as usize]).unwrap();

        gc_session_logs(&root, SESSION_LOG_DISK_BUDGET_BYTES).unwrap();

        assert!(path.metadata().unwrap().len() <= PLAIN_LOG_MAX_BYTES / 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// 2026-07-17 실측 회귀: claude는 정렬에 공백이 아니라 커서 이동을 쓴다
    /// (`auto ESC[11G mode ESC[16G on`). escape를 통째로 버리던 때는 plain.txt에
    /// `automodeon`으로 남아 사람이 읽을 수 없었다 — 벨 인박스 미리보기가 이 파일을
    /// 읽으면서 드러났다.
    #[test]
    fn strip_ansi_커서이동을_공백으로_되돌린다() {
        let mut state = StripState::default();
        // 실제 로그에서 뜬 바이트열 그대로.
        let input = b"auto\x1b[11Gmode\x1b[16Gon";
        let out = strip_ansi_stateful(input, &mut state);
        // auto(0..4) → 11열(0-based 10)까지 공백 6 → mode(10..14) → 16열(15)까지 공백 1 → on
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "auto      mode on",
            "커서가 가리킨 열까지 공백으로 채워 단어가 붙지 않아야 한다"
        );
    }

    #[test]
    fn strip_ansi_cuf는_상대_전진만큼_띄운다() {
        let mut state = StripState::default();
        let out = strip_ansi_stateful(b"a\x1b[3Cb", &mut state);
        assert_eq!(String::from_utf8(out).unwrap(), "a   b");
    }

    /// 뒤로 가는 이동(이미 지난 열)은 공백으로 표현할 수 없다 — 무시하고 내용만 잇는다.
    #[test]
    fn strip_ansi_뒤로가는_cha는_공백을_넣지_않는다() {
        let mut state = StripState::default();
        let out = strip_ansi_stateful(b"abcdef\x1b[2Gx", &mut state);
        assert_eq!(String::from_utf8(out).unwrap(), "abcdefx");
    }

    /// 열 추적은 문자 단위 — UTF-8 연속 바이트를 세면 한글 뒤 정렬이 어긋난다.
    #[test]
    fn strip_ansi_한글_뒤_열계산은_문자수_기준() {
        let mut state = StripState::default();
        let out = strip_ansi_stateful("가나\x1b[5Gx".as_bytes(), &mut state);
        // '가나' = 2문자(col 2) → 5열(0-based 4)까지 공백 2개
        assert_eq!(String::from_utf8(out).unwrap(), "가나  x");
    }

    /// 줄바꿈은 열을 리셋한다 — 안 하면 다음 줄의 CHA가 통째로 무시된다.
    #[test]
    fn strip_ansi_개행_후_열이_리셋된다() {
        let mut state = StripState::default();
        let out = strip_ansi_stateful(b"abcdef\n\x1b[4Gx", &mut state);
        assert_eq!(String::from_utf8(out).unwrap(), "abcdef\n   x");
    }

    /// escape가 chunk 경계에 걸려도 열/파라미터 상태가 이어진다(스트리밍 계약).
    #[test]
    fn strip_ansi_chunk_경계에_걸친_커서이동도_복원된다() {
        let mut state = StripState::default();
        let mut out = strip_ansi_stateful(b"auto\x1b[1", &mut state);
        out.extend(strip_ansi_stateful(b"1Gmode", &mut state));
        assert_eq!(String::from_utf8(out).unwrap(), "auto      mode");
    }
}
