//! 종료 세션 스크롤백 압축 아카이브 (§14.3 확장 — A트랙 PR-A1).
//! 인메모리 아카이브(runtime)의 디스크 연장 — 워커 종료(suspend)·앱 재시작 후에도
//! 열람 복원(PR-A2)의 원천이 된다. logs.rs와 같은 계약: **호출측(runtime worker)이
//! redaction을 끝낸 바이트만 넘긴다** — 이 모듈은 평문 secret을 받지 않는다.
//!
//! 파일: `logs_root/<세션 UUID>/scrollback.zlib` (세션 로그와 같은 수명 정책)
//! 포맷: LE 고정 헤더 + zlib 스트림. 본문 무결성은 zlib(RFC 1950) adler32가 검증하고,
//! 헤더는 magic/version/필드 범위로 검증한다. 손상은 graceful skip(파일 삭제) —
//! 복원 실패가 앱 동작을 해치지 않는다. (헤더 메타 자급은 asciinema v2 관례 차용)

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::logs::SessionLogWriter;

const MAGIC: &[u8; 4] = b"DPSA";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 4 + 1 + 1 + 2 + 2 + 4 + 1 + 4 + 4;
/// 압축 해제 크기 상한 — 오염/압축 폭탄 방어 (visible byte budget 16MB의 2배 여유)
pub const MAX_UNCOMPRESSED_BYTES: u32 = 32 * 1024 * 1024;
/// terminal.size sidecar와 동일한 grid 크기 상한 (범위 밖 = 오염 판정)
const MAX_GRID_DIM: u16 = 500;
/// 워크스페이스(logs_root)당 아카이브 총 바이트 예산 — 초과 시 mtime 오래된 것부터 GC
pub const ARCHIVE_DISK_BUDGET_BYTES: u64 = 64 * 1024 * 1024;

const ARCHIVE_FILE: &str = "scrollback.zlib";
const ARCHIVE_SCAN_ENTRY_LIMIT: usize = 4_096;
const ARCHIVE_SCAN_LIMIT_ERROR: &str = "scrollback_archive_scan_entry_limit";
#[cfg(unix)]
const ARCHIVE_TEMP_ATTEMPTS: usize = 16;
/// 전체 사용량을 안전하게 확정할 수 없음을 나타내는 증분 캐시 값.
pub const ARCHIVE_DISK_USAGE_UNKNOWN: u64 = u64::MAX;

/// 복원에 필요한 세션 메타 — 파일 헤더에 자급한다 (sidecar 의존 없음).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveMeta {
    /// 0=shell, 1=agent (session::SessionKind 대응 — storage는 session에 의존하지 않는다)
    pub kind: u8,
    pub cols: u16,
    pub rows: u16,
    pub scrollback_lines: u32,
    pub exit_code: Option<u32>,
}

struct ArchiveRecord {
    modified: std::time::SystemTime,
    bytes: u64,
    path: PathBuf,
}

pub fn archive_path(logs_root: &Path, session_key: &str) -> anyhow::Result<PathBuf> {
    Ok(SessionLogWriter::session_dir_key(logs_root, session_key)?.join(ARCHIVE_FILE))
}

/// 이미 기록된 아카이브가 있는가 (exited grid는 불변 — 있으면 재기록하지 않는다).
pub fn exists(logs_root: &Path, session_key: &str) -> bool {
    #[cfg(unix)]
    {
        let Ok(Some(directory)) = pin_session_directory(logs_root, session_key, false) else {
            return false;
        };
        regular_entry_exists_at(&directory, ARCHIVE_FILE).unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = (logs_root, session_key);
        false
    }
}

pub fn remove(logs_root: &Path, session_key: &str) -> anyhow::Result<bool> {
    remove_with_session_hook(logs_root, session_key, || {})
}

fn remove_with_session_hook(
    logs_root: &Path,
    session_key: &str,
    after_session_pin: impl FnOnce(),
) -> anyhow::Result<bool> {
    #[cfg(unix)]
    {
        let Some(directory) = pin_session_directory(logs_root, session_key, false)? else {
            return Ok(false);
        };
        after_session_pin();
        match unlink_archive_at(&directory) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).context("scrollback archive remove failed"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (logs_root, session_key, after_session_pin);
        anyhow::bail!("scrollback_archive_descriptor_io_unsupported")
    }
}

/// redaction이 끝난 ANSI 덤프를 압축해 원자적으로 기록한다 (tmp+rename).
/// 반환: 디스크에 기록된 파일 바이트 수 (헤더+zlib) — 호출측 증분 예산 캐시가
/// metadata() 호출 없이 더할 수 있게 한다 (A1 리뷰 P2).
pub fn write(
    logs_root: &Path,
    session_key: &str,
    meta: &ArchiveMeta,
    redacted_ansi: &[u8],
) -> anyhow::Result<u64> {
    write_with_session_hook(logs_root, session_key, meta, redacted_ansi, || {})
}

fn write_with_session_hook(
    logs_root: &Path,
    session_key: &str,
    meta: &ArchiveMeta,
    redacted_ansi: &[u8],
    after_session_pin: impl FnOnce(),
) -> anyhow::Result<u64> {
    anyhow::ensure!(
        redacted_ansi.len() <= MAX_UNCOMPRESSED_BYTES as usize,
        "scrollback 아카이브 크기 초과: {} bytes",
        redacted_ansi.len()
    );
    // parse와 대칭인 불변식 — 범위 밖 meta는 기록 후 읽기에서 손상으로 오인돼
    // 조용히 삭제되므로, 쓰기 시점에 거부해 데이터 유실을 막는다 (codex 리뷰 P3).
    anyhow::ensure!(
        meta.kind <= 1
            && (1..=MAX_GRID_DIM).contains(&meta.cols)
            && (1..=MAX_GRID_DIM).contains(&meta.rows),
        "scrollback 아카이브 meta 범위 위반: kind={} cols={} rows={}",
        meta.kind,
        meta.cols,
        meta.rows
    );

    let mut buf = Vec::with_capacity(HEADER_LEN + redacted_ansi.len() / 4);
    buf.extend_from_slice(MAGIC);
    buf.push(VERSION);
    buf.push(meta.kind);
    buf.extend_from_slice(&meta.cols.to_le_bytes());
    buf.extend_from_slice(&meta.rows.to_le_bytes());
    buf.extend_from_slice(&meta.scrollback_lines.to_le_bytes());
    buf.push(meta.exit_code.is_some() as u8);
    buf.extend_from_slice(&meta.exit_code.unwrap_or(0).to_le_bytes());
    buf.extend_from_slice(&(redacted_ansi.len() as u32).to_le_bytes());
    let mut encoder = flate2::write::ZlibEncoder::new(buf, flate2::Compression::default());
    encoder
        .write_all(redacted_ansi)
        .context("scrollback 압축 실패")?;
    let bytes = encoder.finish().context("scrollback 압축 마감 실패")?;

    #[cfg(unix)]
    {
        let directory = pin_session_directory(logs_root, session_key, true)?
            .context("session archive directory missing after creation")?;
        after_session_pin();
        atomic_write_archive_at(&directory, &bytes).with_context(|| {
            format!(
                "아카이브 원자 기록 실패: {}",
                directory.path.join(ARCHIVE_FILE).display()
            )
        })?;
        Ok(bytes.len() as u64)
    }
    #[cfg(not(unix))]
    {
        let _ = (logs_root, session_key, after_session_pin);
        anyhow::bail!("scrollback_archive_descriptor_io_unsupported")
    }
}

/// 아카이브를 읽어 (메타, redacted ANSI 덤프)를 돌려준다.
/// 파일 없음 → Ok(None). 손상(헤더/범위/inflate 실패) → 파일 삭제 후 Ok(None)
/// (graceful skip — 복원 실패가 치명이 되지 않게).
pub fn read(logs_root: &Path, session_key: &str) -> anyhow::Result<Option<(ArchiveMeta, Vec<u8>)>> {
    let Some(mut stream) = open(logs_root, session_key)? else {
        return Ok(None);
    };
    let meta = stream.meta;
    let mut dump = Vec::with_capacity(stream.expected_len as usize);
    if stream.read_to_end(&mut dump).is_err() {
        stream.discard();
        return Ok(None);
    }
    if !stream.finish() {
        return Ok(None);
    }
    Ok(Some((meta, dump)))
}

/// 아카이브를 스트리밍으로 연다 — 헤더를 검증하고 해제 [`Read`] 핸들을 돌려준다.
/// dump 전체(≤32MB)를 메모리에 올리지 않고 소비측이 청크 단위로 feed할 수 있다
/// (복원 시 순간 메모리 스파이크 방지, 2026-07-16). 파일 없음 → Ok(None),
/// 헤더 손상 → 파일 삭제 후 Ok(None) — [`read`]와 동일 규약.
pub fn open(logs_root: &Path, session_key: &str) -> anyhow::Result<Option<ArchiveStream>> {
    open_with_session_hook(logs_root, session_key, || {})
}

fn open_with_session_hook(
    logs_root: &Path,
    session_key: &str,
    after_session_pin: impl FnOnce(),
) -> anyhow::Result<Option<ArchiveStream>> {
    #[cfg(not(unix))]
    {
        let _ = (logs_root, session_key, after_session_pin);
        anyhow::bail!("scrollback_archive_descriptor_io_unsupported")
    }
    #[cfg(unix)]
    {
        let Some(directory) = pin_session_directory(logs_root, session_key, false)? else {
            return Ok(None);
        };
        let path = directory.path.join(ARCHIVE_FILE);
        after_session_pin();
        let file = match open_regular_archive_at(&directory) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("아카이브 열기 실패: {}", path.display()));
            }
        };
        let mut reader = std::io::BufReader::new(file);
        let mut header = [0u8; HEADER_LEN];
        let parsed = reader
            .read_exact(&mut header)
            .ok()
            .and_then(|()| parse_header(&header));
        let Some((meta, uncompressed_len)) = parsed else {
            tracing::warn!(path = %path.display(), "scrollback 아카이브 손상 — 폐기");
            let _ = unlink_archive_at(&directory);
            return Ok(None);
        };
        // take로 선언 길이 초과 해제를 차단 (압축 폭탄/오염 방어) — 정확 길이 검증은 finish
        let decoder = flate2::read::ZlibDecoder::new(reader).take(u64::from(uncompressed_len) + 1);
        Ok(Some(ArchiveStream {
            meta,
            decoder,
            expected_len: uncompressed_len,
            fed: 0,
            saw_error: false,
            path,
            directory,
        }))
    }
}

#[cfg(unix)]
struct PinnedSessionDirectory {
    fd: std::os::fd::OwnedFd,
    path: PathBuf,
}

#[cfg(unix)]
fn pin_session_directory(
    logs_root: &Path,
    session_key: &str,
    create: bool,
) -> anyhow::Result<Option<PinnedSessionDirectory>> {
    let path = SessionLogWriter::session_dir_key(logs_root, session_key)?;
    let session_name = path
        .file_name()
        .context("session archive directory name missing")?;
    pin_session_directory_entry(logs_root, session_name, create)
}

#[cfg(unix)]
fn pin_session_directory_entry(
    logs_root: &Path,
    session_name: &std::ffi::OsStr,
    create: bool,
) -> anyhow::Result<Option<PinnedSessionDirectory>> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::ffi::OsStrExt as _;

    let path = logs_root.join(session_name);
    if create {
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
        if !create && error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error).context("logs_root directory open failed");
    }
    let root_fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(root_fd) };
    let session_name = std::ffi::CString::new(session_name.as_bytes())
        .map_err(|_| anyhow::anyhow!("session_key_contains_nul"))?;
    if create {
        let created = unsafe { libc::mkdirat(root_fd.as_raw_fd(), session_name.as_ptr(), 0o755) };
        if created != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error).context("session archive directory create failed");
            }
        }
    }
    let session_fd = unsafe {
        libc::openat(
            root_fd.as_raw_fd(),
            session_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if session_fd < 0 {
        let error = std::io::Error::last_os_error();
        if !create && error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error).context("session archive directory open failed");
    }
    Ok(Some(PinnedSessionDirectory {
        fd: unsafe { std::os::fd::OwnedFd::from_raw_fd(session_fd) },
        path,
    }))
}

#[cfg(unix)]
fn open_regular_archive_at(directory: &PinnedSessionDirectory) -> std::io::Result<std::fs::File> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let name = std::ffi::CString::new(ARCHIVE_FILE).expect("archive file is static ASCII");
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
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::other("archive_file_not_regular"));
    }
    Ok(file)
}

#[cfg(unix)]
fn regular_entry_exists_at(
    directory: &PinnedSessionDirectory,
    name: &str,
) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd as _;

    let name = std::ffi::CString::new(name).expect("archive entry name is static ASCII");
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
fn atomic_write_archive_at(
    directory: &PinnedSessionDirectory,
    bytes: &[u8],
) -> std::io::Result<()> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let final_name = std::ffi::CString::new(ARCHIVE_FILE).expect("archive file is static ASCII");
    for attempt in 0..ARCHIVE_TEMP_ATTEMPTS {
        let temp_name = if attempt == 0 {
            format!("{ARCHIVE_FILE}.deppytmp")
        } else {
            format!(
                ".{ARCHIVE_FILE}.deppytmp.{}.{}.{}",
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
            if !regular_entry_exists_at(directory, temp_name.to_str().unwrap_or_default())? {
                return Err(std::io::Error::other("archive_temp_not_regular"));
            }
            continue;
        }
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
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
        "archive_temp_collision_limit",
    ))
}

#[cfg(unix)]
fn unlink_archive_at(directory: &PinnedSessionDirectory) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;

    let name = std::ffi::CString::new(ARCHIVE_FILE).expect("archive file is static ASCII");
    let result = unsafe { libc::unlinkat(directory.fd.as_raw_fd(), name.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// [`open`]이 돌려주는 스트리밍 핸들. [`Read`]로 해제 바이트를 내보내며, 소비가 끝나면
/// [`ArchiveStream::finish`]로 완결성을 확인한다 — false면 부분 feed 결과물을 버릴 것.
pub struct ArchiveStream {
    pub meta: ArchiveMeta,
    decoder: std::io::Take<flate2::read::ZlibDecoder<std::io::BufReader<std::fs::File>>>,
    expected_len: u32,
    fed: u64,
    saw_error: bool,
    path: PathBuf,
    #[cfg(unix)]
    directory: PinnedSessionDirectory,
}

impl Read for ArchiveStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.decoder.read(buf) {
            Ok(read) => {
                self.fed += read as u64;
                Ok(read)
            }
            Err(error) => {
                self.saw_error = true;
                Err(error)
            }
        }
    }
}

impl ArchiveStream {
    /// 소비 완료 후 완결성 확인 — 잔여 바이트를 마저 세고(소비측 조기 중단 대비)
    /// 선언 길이와 정확히 일치 + 해제 오류(adler 불일치 등) 없음이어야 true.
    /// 손상이면 파일을 삭제하고 false — 호출측은 feed된 부분 결과물을 버려야 한다.
    pub fn finish(mut self) -> bool {
        let mut sink = [0u8; 16 * 1024];
        loop {
            match self.read(&mut sink) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let ok = !self.saw_error && self.fed == u64::from(self.expected_len);
        if !ok {
            self.discard();
        }
        ok
    }

    /// 손상 확정 — 경고 후 파일 삭제 (graceful skip 규약).
    fn discard(self) {
        tracing::warn!(path = %self.path.display(), "scrollback 아카이브 손상 — 폐기");
        #[cfg(unix)]
        let _ = unlink_archive_at(&self.directory);
    }
}

fn parse_header(bytes: &[u8; HEADER_LEN]) -> Option<(ArchiveMeta, u32)> {
    if &bytes[0..4] != MAGIC || bytes[4] != VERSION {
        return None;
    }
    let kind = bytes[5];
    let cols = u16::from_le_bytes([bytes[6], bytes[7]]);
    let rows = u16::from_le_bytes([bytes[8], bytes[9]]);
    let scrollback_lines = u32::from_le_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]);
    let has_exit = bytes[14];
    let exit_code = u32::from_le_bytes([bytes[15], bytes[16], bytes[17], bytes[18]]);
    let uncompressed_len = u32::from_le_bytes([bytes[19], bytes[20], bytes[21], bytes[22]]);
    let valid = kind <= 1
        && (1..=MAX_GRID_DIM).contains(&cols)
        && (1..=MAX_GRID_DIM).contains(&rows)
        && has_exit <= 1
        && uncompressed_len <= MAX_UNCOMPRESSED_BYTES;
    if !valid {
        return None;
    }
    Some((
        ArchiveMeta {
            kind,
            cols,
            rows,
            scrollback_lines,
            exit_code: (has_exit == 1).then_some(exit_code),
        },
        uncompressed_len,
    ))
}

/// logs_root 아래 각 세션 디렉터리의 scrollback.zlib를 (mtime, len, path)로 모은다.
/// logs_root 부재는 빈 목록으로 취급한다 (scan_total·gc 공용 스캔).
fn collect_archives_with_limit(
    logs_root: &Path,
    entry_limit: usize,
) -> anyhow::Result<Vec<ArchiveRecord>> {
    let entries = match std::fs::read_dir(logs_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("logs_root 나열 실패"),
    };
    let mut archives = Vec::new();
    for entry in entries {
        let entry = entry.context("logs_root 항목 조회 실패")?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("세션 디렉터리 유형 조회 실패: {}", entry.path().display()))?;
        anyhow::ensure!(
            !file_type.is_symlink(),
            "scrollback_archive_session_directory_not_regular"
        );
        if !file_type.is_dir() {
            continue;
        }
        if let Some(archive) = archive_record(&entry.path())? {
            if archives.len() >= entry_limit {
                anyhow::bail!("{}", ARCHIVE_SCAN_LIMIT_ERROR);
            }
            archives.push(archive);
        }
    }
    Ok(archives)
}

struct ArchiveScan {
    archives: Vec<ArchiveRecord>,
    directories: Vec<PathBuf>,
    complete: bool,
}

fn scan_archive_batch_with_limit(
    logs_root: &Path,
    entry_limit: usize,
) -> anyhow::Result<ArchiveScan> {
    let entries = match std::fs::read_dir(logs_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ArchiveScan {
                archives: Vec::new(),
                directories: Vec::new(),
                complete: true,
            });
        }
        Err(error) => return Err(error).context("logs_root 나열 실패"),
    };
    let mut scan = ArchiveScan {
        archives: Vec::new(),
        directories: Vec::new(),
        complete: true,
    };
    for entry in entries {
        let entry = entry.context("logs_root 항목 조회 실패")?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("세션 디렉터리 유형 조회 실패: {}", entry.path().display()))?;
        anyhow::ensure!(
            !file_type.is_symlink(),
            "scrollback_archive_session_directory_not_regular"
        );
        if !file_type.is_dir() {
            continue;
        }
        let directory = entry.path();
        if let Some(archive) = archive_record(&directory)? {
            if scan.archives.len() >= entry_limit {
                scan.complete = false;
                break;
            }
            scan.archives.push(archive);
            scan.directories.push(directory);
        }
    }
    Ok(scan)
}

fn archive_record(directory: &Path) -> anyhow::Result<Option<ArchiveRecord>> {
    let directory_metadata = directory
        .symlink_metadata()
        .with_context(|| format!("세션 디렉터리 metadata 실패: {}", directory.display()))?;
    anyhow::ensure!(
        directory_metadata.file_type().is_dir(),
        "scrollback_archive_session_directory_not_regular"
    );
    let path = directory.join(ARCHIVE_FILE);
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("아카이브 metadata 실패: {}", path.display()));
        }
    };
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "scrollback_archive_file_not_regular"
    );
    Ok(Some(ArchiveRecord {
        modified: metadata
            .modified()
            .with_context(|| format!("아카이브 수정시각 조회 실패: {}", path.display()))?,
        bytes: metadata.len(),
        path,
    }))
}

/// logs_root 아래 아카이브 총 바이트를 센다 (읽기 전용 — 삭제하지 않는다).
/// 워커 시작 시 증분 예산 캐시를 1회 시드하는 용도 (A1 리뷰 P2). 나열 실패나 합산
/// overflow는 `ARCHIVE_DISK_USAGE_UNKNOWN`으로 반환해 신규 기록을 fail closed한다.
pub fn scan_total(logs_root: &Path) -> u64 {
    scan_total_with_limit(logs_root, ARCHIVE_SCAN_ENTRY_LIMIT)
}

fn scan_total_with_limit(logs_root: &Path, entry_limit: usize) -> u64 {
    collect_archives_with_limit(logs_root, entry_limit)
        .and_then(|archives| {
            archives.iter().try_fold(0u64, |total, archive| {
                total
                    .checked_add(archive.bytes)
                    .context("scrollback archive byte total overflow")
            })
        })
        .unwrap_or(ARCHIVE_DISK_USAGE_UNKNOWN)
}

/// logs_root 아래 아카이브 총량이 예산을 넘으면 mtime 오래된 것부터 삭제한다.
/// 로그 3종(redacted.*)은 건드리지 않는다 — 대상은 scrollback.zlib뿐.
/// 반환: 정리 후 현재 아카이브 총 바이트 — 호출측 증분 예산 캐시 재동기화용 (A1 리뷰 P2).
pub fn gc(logs_root: &Path, budget_bytes: u64) -> anyhow::Result<u64> {
    gc_with_limit(logs_root, budget_bytes, ARCHIVE_SCAN_ENTRY_LIMIT)
}

fn gc_with_limit(logs_root: &Path, budget_bytes: u64, entry_limit: usize) -> anyhow::Result<u64> {
    let scan = scan_archive_batch_with_limit(logs_root, entry_limit)?;
    let mut archives = scan.archives;
    let mut total = archive_total(&archives);
    if total == ARCHIVE_DISK_USAGE_UNKNOWN {
        anyhow::bail!("scrollback_archive_byte_total_overflow");
    }
    if !scan.complete {
        remove_archives_until_budget(logs_root, &mut total, archives, 0, true);
        for directory in scan.directories {
            let _ = std::fs::remove_dir(directory);
        }
        anyhow::bail!("{}", ARCHIVE_SCAN_LIMIT_ERROR);
    }
    if total <= budget_bytes {
        return Ok(total);
    }
    archives.sort_by_key(|archive| archive.modified);
    remove_archives_until_budget(logs_root, &mut total, archives, budget_bytes, false);
    if total > budget_bytes {
        anyhow::bail!("scrollback_archive_gc_budget_unmet");
    }
    Ok(total)
}

fn archive_total(archives: &[ArchiveRecord]) -> u64 {
    archives
        .iter()
        .try_fold(0u64, |total, archive| total.checked_add(archive.bytes))
        .unwrap_or(ARCHIVE_DISK_USAGE_UNKNOWN)
}

fn remove_archives_until_budget(
    logs_root: &Path,
    total: &mut u64,
    archives: Vec<ArchiveRecord>,
    budget_bytes: u64,
    force_all: bool,
) {
    for archive in archives {
        if !force_all && *total <= budget_bytes {
            break;
        }
        let Some(session_name) = archive.path.parent().and_then(Path::file_name) else {
            tracing::warn!(path = %archive.path.display(), "아카이브 GC 세션 경로 없음");
            continue;
        };
        #[cfg(unix)]
        let removal = pin_session_directory_entry(logs_root, session_name, false).and_then(|dir| {
            let Some(dir) = dir else {
                return Ok(false);
            };
            match unlink_archive_at(&dir) {
                Ok(()) => Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error).context("scrollback archive GC remove failed"),
            }
        });
        #[cfg(not(unix))]
        let removal: anyhow::Result<bool> = Err(anyhow::anyhow!(
            "scrollback_archive_descriptor_io_unsupported"
        ));
        match removal {
            Ok(true) => {
                *total = total.saturating_sub(archive.bytes);
                tracing::info!(path = %archive.path.display(), "scrollback 아카이브 GC — 예산 초과 제거");
            }
            Ok(false) => {
                *total = total.saturating_sub(archive.bytes);
            }
            Err(e) => {
                tracing::warn!(path = %archive.path.display(), "아카이브 GC 삭제 실패: {e:#}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "deppy-scrollback-archive-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn meta() -> ArchiveMeta {
        ArchiveMeta {
            kind: 1,
            cols: 120,
            rows: 40,
            scrollback_lines: 1_000,
            exit_code: Some(0),
        }
    }

    #[test]
    fn 라운드트립() {
        let root = temp_root();
        let dump = "한글 \x1b[31mred\x1b[0m line\r\nnext".as_bytes();
        let written = write(&root, "uuid-1", &meta(), dump).unwrap();
        // write 반환값은 디스크 파일 크기와 일치해야 한다 (증분 캐시가 이 값을 더한다).
        let on_disk = archive_path(&root, "uuid-1")
            .unwrap()
            .metadata()
            .unwrap()
            .len();
        assert_eq!(written, on_disk);
        let (read_meta, read_dump) = read(&root, "uuid-1").unwrap().unwrap();
        assert_eq!(read_meta, meta());
        assert_eq!(read_dump, dump);
        // tmp 잔재 없음 (원자 기록)
        assert!(
            !root
                .join("uuid-1")
                .join("scrollback.zlib.deppytmp")
                .exists()
        );
    }

    #[test]
    fn 파일_없음은_none() {
        assert!(read(&temp_root(), "missing").unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn restore_rejects_symlinked_archive_without_following_it() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        let outside = temp_root();
        write(&outside, "target", &meta(), b"outside").unwrap();
        let session_dir = root.join("session");
        std::fs::create_dir_all(&session_dir).unwrap();
        symlink(
            archive_path(&outside, "target").unwrap(),
            session_dir.join(ARCHIVE_FILE),
        )
        .unwrap();

        let result = open(&root, "session");

        assert!(result.is_err(), "restore must reject an archive symlink");
        assert!(archive_path(&outside, "target").unwrap().exists());
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn restore_rejects_fifo_without_blocking() {
        use std::io::Write as _;
        use std::os::unix::ffi::OsStrExt as _;

        let root = temp_root();
        let session_dir = root.join("session");
        std::fs::create_dir_all(&session_dir).unwrap();
        let fifo = session_dir.join(ARCHIVE_FILE);
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        let open_root = root.clone();
        std::thread::spawn(move || {
            tx.send(open(&open_root, "session").map(|stream| stream.is_some()))
                .unwrap();
        });

        let result = match rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let mut writer = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();
                writer.write_all(&[0; HEADER_LEN]).unwrap();
                drop(writer);
                let _ = rx.recv_timeout(std::time::Duration::from_secs(1));
                panic!("archive restore blocked while opening a FIFO");
            }
            Err(error) => panic!("archive restore worker disconnected: {error}"),
        };

        assert!(result.is_err(), "restore must reject a FIFO");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn 손상_파일은_삭제_후_none() {
        let root = temp_root();
        // 압축이 잘 안 되는 payload — 절단이 확실히 deflate 본문을 자르게 한다
        // (동일 바이트 반복은 수십 바이트로 압축돼 트레일러만 잘릴 수 있음)
        let payload: Vec<u8> = (0..4096u32).map(|i| (i * 31 % 251) as u8).collect();
        write(&root, "u", &meta(), &payload).unwrap();
        let path = archive_path(&root, "u").unwrap();
        // 본문 중간 절단
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
        assert!(read(&root, "u").unwrap().is_none());
        assert!(!path.exists(), "손상 파일은 graceful skip으로 삭제");
        // magic 불일치
        std::fs::write(&path, b"XXXXjunkjunkjunkjunkjunkjunk").unwrap();
        assert!(read(&root, "u").unwrap().is_none());
        assert!(!path.exists());
    }

    #[test]
    fn 경로_탈출_거부() {
        let root = temp_root();
        assert!(write(&root, "../evil", &meta(), b"x").is_err());
        assert!(read(&root, "a/b").is_err());
    }

    #[test]
    fn gc는_오래된_것부터_예산까지_제거하고_로그는_불가침() {
        let root = temp_root();
        let payload = vec![b'x'; 4096];
        for (i, key) in ["old", "mid", "new"].iter().enumerate() {
            write(&root, key, &meta(), &payload).unwrap();
            let path = archive_path(&root, key).unwrap();
            // mtime을 명시적으로 벌린다 (연속 기록의 mtime 해상도 문제 회피)
            let time = std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(1_000_000 + i as u64 * 1000);
            let file = std::fs::File::options().append(true).open(&path).unwrap();
            file.set_modified(time).unwrap();
        }
        // 같은 디렉터리의 로그 파일은 GC 대상이 아니다
        let log = root.join("old").join("redacted.ansi.log");
        std::fs::write(&log, b"log").unwrap();

        let one = archive_path(&root, "old")
            .unwrap()
            .metadata()
            .unwrap()
            .len();
        let total_after = gc(&root, one * 2).unwrap(); // 3개 중 2개 예산 → 가장 오래된 1개 제거
        assert!(!archive_path(&root, "old").unwrap().exists());
        assert!(archive_path(&root, "mid").unwrap().exists());
        assert!(archive_path(&root, "new").unwrap().exists());
        assert!(log.exists(), "로그 파일 불가침");
        // gc는 정리 후 총 바이트를 돌려준다 (증분 캐시 재동기화용) — 남은 2개 = 2*one.
        assert_eq!(total_after, one * 2);
    }

    #[test]
    fn scan_total은_아카이브_크기_합만_세고_로그는_제외한다() {
        let root = temp_root();
        assert_eq!(scan_total(&root.join("nonexistent")), 0, "부재 루트는 0");
        let payload = vec![b'x'; 4096];
        let a = write(&root, "s1", &meta(), &payload).unwrap();
        let b = write(&root, "s2", &meta(), &payload).unwrap();
        // 같은 세션 디렉터리의 로그 파일은 scan_total 대상이 아니다.
        std::fs::write(root.join("s1").join("redacted.ansi.log"), b"log noise").unwrap();
        // 재시드: 이미 존재하는 아카이브 총량을 정확히 복원한다 (워커 재생성 시드 경로).
        assert_eq!(scan_total(&root), a + b);
    }

    #[cfg(unix)]
    #[test]
    fn archive_scan_fails_closed_for_existing_nonregular_archive() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        let session_dir = root.join("session");
        std::fs::create_dir_all(&session_dir).unwrap();
        symlink("missing", session_dir.join(ARCHIVE_FILE)).unwrap();

        assert_eq!(scan_total(&root), ARCHIVE_DISK_USAGE_UNKNOWN);
        assert!(gc(&root, ARCHIVE_DISK_BUDGET_BYTES).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn archive_write_rejects_symlinked_session_directory() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        let outside = temp_root();
        symlink(&outside, root.join("session")).unwrap();

        let result = write(&root, "session", &meta(), b"archive");

        assert!(result.is_err());
        assert!(!outside.join(ARCHIVE_FILE).exists());
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn archive_write_rejects_planted_atomic_temp_symlink() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        let outside = temp_root();
        let outside_target = outside.join("target");
        std::fs::write(&outside_target, b"outside-safe").unwrap();
        let session_dir = root.join("session");
        std::fs::create_dir_all(&session_dir).unwrap();
        symlink(
            &outside_target,
            session_dir.join(format!("{ARCHIVE_FILE}.deppytmp")),
        )
        .unwrap();

        let result = write(&root, "session", &meta(), b"archive");

        assert!(result.is_err(), "a planted temp symlink must fail closed");
        assert_eq!(std::fs::read(&outside_target).unwrap(), b"outside-safe");
        assert!(
            archive_path(&root, "session")
                .unwrap()
                .symlink_metadata()
                .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn exists_rejects_symlink_dangling_and_nonregular_archive_paths() {
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::symlink;

        let root = temp_root();
        let target = root.join("target");
        std::fs::write(&target, b"target").unwrap();

        for session in ["linked", "dangling", "directory", "fifo"] {
            std::fs::create_dir_all(root.join(session)).unwrap();
        }
        symlink(&target, root.join("linked").join(ARCHIVE_FILE)).unwrap();
        symlink("missing", root.join("dangling").join(ARCHIVE_FILE)).unwrap();
        std::fs::create_dir(root.join("directory").join(ARCHIVE_FILE)).unwrap();
        let fifo = root.join("fifo").join(ARCHIVE_FILE);
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);

        assert!(!exists(&root, "linked"));
        assert!(!exists(&root, "dangling"));
        assert!(!exists(&root, "directory"));
        assert!(!exists(&root, "fifo"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn pinned_session_directory_prevents_parent_swap_from_redirecting_write() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        let outside = temp_root();
        let session_dir = root.join("session");
        let pinned_dir = root.join("pinned-session");
        std::fs::create_dir_all(&session_dir).unwrap();

        write_with_session_hook(&root, "session", &meta(), b"pinned", || {
            std::fs::rename(&session_dir, &pinned_dir).unwrap();
            symlink(&outside, &session_dir).unwrap();
        })
        .unwrap();

        assert!(pinned_dir.join(ARCHIVE_FILE).is_file());
        assert!(!outside.join(ARCHIVE_FILE).exists());
        std::fs::remove_file(&session_dir).unwrap();
        std::fs::rename(&pinned_dir, &session_dir).unwrap();
        assert_eq!(read(&root, "session").unwrap().unwrap().1, b"pinned");
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn pinned_session_directory_prevents_parent_swap_from_redirecting_read() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        let outside = temp_root();
        write(&root, "session", &meta(), b"inside").unwrap();
        write(&outside, "session", &meta(), b"outside").unwrap();
        let session_dir = root.join("session");
        let pinned_dir = root.join("pinned-session");

        let mut stream = open_with_session_hook(&root, "session", || {
            std::fs::rename(&session_dir, &pinned_dir).unwrap();
            symlink(outside.join("session"), &session_dir).unwrap();
        })
        .unwrap()
        .unwrap();
        let mut dump = Vec::new();
        stream.read_to_end(&mut dump).unwrap();

        assert!(stream.finish());
        assert_eq!(dump, b"inside");
        std::fs::remove_file(&session_dir).unwrap();
        std::fs::remove_dir_all(pinned_dir).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn pinned_session_directory_prevents_parent_swap_from_redirecting_remove() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        let outside = temp_root();
        write(&root, "session", &meta(), b"inside").unwrap();
        write(&outside, "session", &meta(), b"outside").unwrap();
        let session_dir = root.join("session");
        let pinned_dir = root.join("pinned-session");

        assert!(
            remove_with_session_hook(&root, "session", || {
                std::fs::rename(&session_dir, &pinned_dir).unwrap();
                symlink(outside.join("session"), &session_dir).unwrap();
            })
            .unwrap()
        );

        assert!(!pinned_dir.join(ARCHIVE_FILE).exists());
        assert!(outside.join("session").join(ARCHIVE_FILE).is_file());
        std::fs::remove_file(&session_dir).unwrap();
        std::fs::remove_dir_all(pinned_dir).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn scan_entry_limit_exact_boundary_succeeds() {
        let root = temp_root();
        for index in 0..4 {
            let dir = root.join(format!("session-{index}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(ARCHIVE_FILE), b"").unwrap();
        }

        let archives = collect_archives_with_limit(&root, 4).unwrap();

        assert_eq!(archives.len(), 4);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scan_entry_limit_default_exact_boundary_succeeds() {
        let root = temp_root();
        for index in 0..ARCHIVE_SCAN_ENTRY_LIMIT {
            let dir = root.join(format!("session-{index}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(ARCHIVE_FILE), b"").unwrap();
        }

        let total = gc(&root, 0).unwrap();

        assert_eq!(total, 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scan_entry_limit_rejects_limit_plus_one() {
        let root = temp_root();
        for index in 0..5 {
            let dir = root.join(format!("session-{index}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(ARCHIVE_FILE), b"").unwrap();
        }

        let err = match collect_archives_with_limit(&root, 4) {
            Ok(_) => panic!("expected scan entry limit error"),
            Err(error) => error,
        };

        assert_eq!(err.to_string(), ARCHIVE_SCAN_LIMIT_ERROR);
        assert_eq!(
            scan_total_with_limit(&root, 4),
            u64::MAX,
            "scan_total must expose unknown usage on scan errors"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scan_entry_limit_default_rejects_limit_plus_one() {
        let root = temp_root();
        for index in 0..=ARCHIVE_SCAN_ENTRY_LIMIT {
            let dir = root.join(format!("session-{index}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(ARCHIVE_FILE), b"").unwrap();
        }

        assert_eq!(
            scan_total(&root),
            u64::MAX,
            "scan_total must expose unknown usage on scan errors"
        );
        let err = gc(&root, 0).unwrap_err();

        assert_eq!(err.to_string(), ARCHIVE_SCAN_LIMIT_ERROR);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scan_entry_limit_reports_incomplete_after_bounded_progress() {
        let root = temp_root();
        let payload = vec![b'x'; 4096];
        let old_bytes = write(&root, "old", &meta(), &payload).unwrap();
        let old_path = archive_path(&root, "old").unwrap();
        let file = std::fs::File::options()
            .append(true)
            .open(&old_path)
            .unwrap();
        file.set_modified(
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000),
        )
        .unwrap();
        let mut paths = vec![old_path];
        for index in 0..2 {
            let dir = root.join(format!("archive-{index}"));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(ARCHIVE_FILE);
            std::fs::write(&path, b"").unwrap();
            paths.push(path);
        }

        let err = gc_with_limit(&root, old_bytes.saturating_sub(1), 2).unwrap_err();

        assert_eq!(err.to_string(), ARCHIVE_SCAN_LIMIT_ERROR);
        assert!(
            paths.iter().any(|path| !path.exists()),
            "over-limit archive scan must remove a bounded candidate batch"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn non_archive_directories_do_not_starve_bounded_scan() {
        let root = temp_root();
        for index in 0..=ARCHIVE_SCAN_ENTRY_LIMIT {
            let dir = root.join(format!("noise-{index}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("redacted.plain.txt"), b"ignored").unwrap();
        }
        let archive_dir = root.join("session");
        std::fs::create_dir_all(&archive_dir).unwrap();
        std::fs::write(archive_dir.join(ARCHIVE_FILE), b"archive").unwrap();

        let archives = collect_archives_with_limit(&root, 1).unwrap();

        assert_eq!(archives.len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn over_limit_gc_makes_bounded_progress_until_recovered() {
        let root = temp_root();
        let payload = vec![b'x'; 4096];
        let mut paths = Vec::new();
        for index in 0..5 {
            write(&root, &format!("session-{index}"), &meta(), &payload).unwrap();
            paths.push(archive_path(&root, &format!("session-{index}")).unwrap());
        }

        let first = gc_with_limit(&root, 0, 4);

        assert!(
            first.is_err(),
            "incomplete bounded scan must remain explicit"
        );
        assert!(
            paths.iter().filter(|path| path.exists()).count() < paths.len(),
            "an over-limit pass must delete at least one bounded batch"
        );
        for _ in 0..5 {
            if matches!(gc_with_limit(&root, 0, 4), Ok(0)) {
                break;
            }
        }
        assert!(
            paths.iter().all(|path| !path.exists()),
            "repeated bounded passes must eventually recover"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn over_limit_gc_removes_zero_byte_archive_candidates() {
        let root = temp_root();
        let mut paths = Vec::new();
        for index in 0..5 {
            let dir = root.join(format!("session-{index}"));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(ARCHIVE_FILE);
            std::fs::write(&path, b"").unwrap();
            paths.push(path);
        }

        let _ = gc_with_limit(&root, 0, 4);

        assert!(paths.iter().any(|path| !path.exists()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn over_limit_gc_does_not_follow_symlinked_session_directories() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        let outside = temp_root();
        let outside_archive = outside.join(ARCHIVE_FILE);
        std::fs::write(&outside_archive, b"outside").unwrap();
        for index in 0..5 {
            symlink(&outside, root.join(format!("linked-{index}"))).unwrap();
        }

        let _ = gc_with_limit(&root, 0, 4);

        assert!(
            outside_archive.exists(),
            "bounded recovery must not delete through a symlinked session directory"
        );
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn gc_errors_when_delete_failure_leaves_total_over_budget() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root();
        write(&root, "locked", &meta(), b"archive").unwrap();
        let session_dir = root.join("locked");
        std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let result = gc(&root, 0);

        std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            result.is_err(),
            "GC must not report success while retained bytes exceed the budget"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn archive_total_overflow_is_explicit() {
        let archives = vec![
            ArchiveRecord {
                modified: std::time::UNIX_EPOCH,
                bytes: u64::MAX,
                path: PathBuf::from("first"),
            },
            ArchiveRecord {
                modified: std::time::UNIX_EPOCH,
                bytes: 1,
                path: PathBuf::from("second"),
            },
        ];

        assert_eq!(archive_total(&archives), ARCHIVE_DISK_USAGE_UNKNOWN);
    }
}
