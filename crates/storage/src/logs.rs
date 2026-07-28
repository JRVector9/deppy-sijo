//! 세션당 tail-bounded redacted 로그 3종 (설계문서 7장):
//! redacted.ansi.log / redacted.plain.txt / events.redacted.jsonl
//! 호출측(runtime worker)이 redaction을 끝낸 바이트만 넘긴다 —
//! 이 모듈은 평문 secret을 받지 않는 것이 계약이다.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use deppy_core::SessionId;

const ANSI_LOG_FILE: &str = "redacted.ansi.log";
const PLAIN_LOG_FILE: &str = "redacted.plain.txt";
const EVENTS_LOG_FILE: &str = "events.redacted.jsonl";
const TERMINAL_SIZE_BYTES_MAX: usize = 64;

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

struct BoundedLogFile {
    file: File,
    path: PathBuf,
    max_bytes: u64,
    tail_boundary: TailBoundary,
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
    fn open(path: &Path, max_bytes: u64, tail_boundary: TailBoundary) -> anyhow::Result<Self> {
        let file = open_regular_log_file(path, true)
            .with_context(|| format!("로그 파일 열기 실패: {}", path.display()))?;
        let mut bounded = Self {
            file,
            path: path.to_path_buf(),
            max_bytes,
            tail_boundary,
        };
        bounded.compact_if_oversized(max_bytes / 2)?;
        Ok(bounded)
    }

    fn len(&self) -> std::io::Result<u64> {
        self.file.metadata().map(|metadata| metadata.len())
    }

    fn append(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let incoming = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if incoming >= self.max_bytes {
            // giant chunk 자체가 이전 chunk의 UTF-8/ANSI sequence를 이어갈 수 있으므로
            // 임의 slice를 먼저 만들지 않는다. 전체 stream을 append한 다음 시작부터
            // parser state를 계산해 안전한 최근 tail로 줄인다.
            self.file
                .write_all(bytes)
                .with_context(|| format!("로그 기록 실패: {}", self.path.display()))?;
            compact_open_file_to_tail(&mut self.file, self.max_bytes, self.tail_boundary)
                .with_context(|| format!("로그 tail 압축 실패: {}", self.path.display()))?;
            return Ok(());
        }
        let current = self.len().unwrap_or(0);
        if current.saturating_add(incoming) > self.max_bytes {
            let retain = self
                .max_bytes
                .saturating_sub(incoming)
                .min(self.max_bytes / 2);
            compact_open_file_to_tail(&mut self.file, retain, self.tail_boundary)
                .with_context(|| format!("로그 tail 압축 실패: {}", self.path.display()))?;
        }
        self.file
            .write_all(bytes)
            .with_context(|| format!("로그 기록 실패: {}", self.path.display()))?;
        Ok(())
    }

    fn compact_if_oversized(&mut self, retain_bytes: u64) -> anyhow::Result<()> {
        if self.len().unwrap_or(0) > self.max_bytes {
            compact_open_file_to_tail(&mut self.file, retain_bytes, self.tail_boundary)
                .with_context(|| format!("기존 로그 tail 압축 실패: {}", self.path.display()))?;
        }
        Ok(())
    }

    fn flush(&mut self) {
        self.file.flush().ok();
    }
}

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
        let dir = Self::session_dir_key(logs_root, session_key)?;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("로그 디렉터리 생성 실패: {}", dir.display()))?;
        Ok(Self {
            ansi: BoundedLogFile::open(
                &dir.join(ANSI_LOG_FILE),
                ANSI_LOG_MAX_BYTES,
                TailBoundary::Ansi,
            )?,
            plain: BoundedLogFile::open(
                &dir.join(PLAIN_LOG_FILE),
                PLAIN_LOG_MAX_BYTES,
                TailBoundary::NextNewlineIfPresent,
            )?,
            events: BoundedLogFile::open(
                &dir.join(EVENTS_LOG_FILE),
                EVENTS_LOG_MAX_BYTES,
                TailBoundary::NextNewlineIfPresent,
            )?,
            strip_state: StripState::default(),
        })
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
        let path = Self::terminal_size_path(logs_root, session_key)?;
        let Some(raw) = read_terminal_size_bounded(&path)? else {
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
        anyhow::ensure!((1..=500).contains(&cols), "터미널 열 수 범위 초과");
        anyhow::ensure!((1..=500).contains(&rows), "터미널 행 수 범위 초과");
        let dir = Self::session_dir_key(logs_root, session_key)?;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("로그 디렉터리 생성 실패: {}", dir.display()))?;
        let path = dir.join("terminal.size");
        deppy_core::fs::atomic_write(&path, format!("{cols} {rows}\n").as_bytes())
            .map_err(|_| anyhow::anyhow!("terminal_size_write_failed"))
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

fn read_terminal_size_bounded(path: &Path) -> anyhow::Result<Option<String>> {
    read_terminal_size_bounded_with_hook(path, || {})
}

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
    let len = file.metadata()?.len();
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
    if file.metadata()?.len() != len {
        return Err(std::io::Error::other("log_changed_during_compaction"));
    }
    file.set_len(0)?;
    file.write_all(&tail)?;
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
    paths: Vec<(PathBuf, u64)>,
    bytes: u64,
    modified: std::time::SystemTime,
}

struct SessionLogCandidate {
    dir: PathBuf,
    path: PathBuf,
    original_len: u64,
    modified: std::time::SystemTime,
    max_bytes: u64,
    retain_bytes: u64,
    tail_boundary: TailBoundary,
}

impl Default for SessionLogBundle {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            bytes: 0,
            modified: std::time::UNIX_EPOCH,
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
    let scan = scan_session_log_candidates_with_limit(logs_root, entry_limit)?;
    let bundles = bundle_session_log_candidates(scan.candidates, scan.complete)?;
    if !scan.complete {
        remove_session_log_bundles(bundles, 0, true);
        remove_empty_directories(scan.directories, logs_root);
        anyhow::bail!("{}", SESSION_LOG_SCAN_LIMIT_ERROR);
    }
    gc_session_log_bundles(bundles, budget_bytes)
}

fn gc_session_log_bundles(
    bundles: Vec<SessionLogBundle>,
    budget_bytes: u64,
) -> anyhow::Result<u64> {
    let total = remove_session_log_bundles(bundles, budget_bytes, false);
    if total > budget_bytes {
        anyhow::bail!("session_log_gc_budget_unmet");
    }
    Ok(total)
}

fn remove_session_log_bundles(
    mut bundles: Vec<SessionLogBundle>,
    budget_bytes: u64,
    force_all: bool,
) -> u64 {
    let mut total = bundles.iter().map(|bundle| bundle.bytes).sum::<u64>();
    if !force_all && total <= budget_bytes {
        return total;
    }
    bundles.sort_by_key(|bundle| bundle.modified);
    for bundle in bundles {
        if !force_all && total <= budget_bytes {
            break;
        }
        let mut removed = 0u64;
        for (path, len) in bundle.paths {
            match std::fs::remove_file(&path) {
                Ok(()) => removed = removed.saturating_add(len),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    removed = removed.saturating_add(len);
                }
                Err(error) => {
                    tracing::warn!(path = %path.display(), "세션 로그 GC 삭제 실패: {error:#}")
                }
            }
        }
        total = total.saturating_sub(removed);
        if removed > 0 {
            tracing::info!(bytes = removed, "오래된 세션 로그 GC — 전체 예산 초과 제거");
        }
    }
    total
}

fn path_len_or_zero(path: &Path) -> u64 {
    path.metadata().map(|metadata| metadata.len()).unwrap_or(0)
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
    bundle_session_log_candidates(scan.candidates, true)
}

fn bundle_session_log_candidates(
    candidates: Vec<SessionLogCandidate>,
    compact_oversized: bool,
) -> anyhow::Result<Vec<SessionLogBundle>> {
    let mut by_dir = std::collections::HashMap::<PathBuf, SessionLogBundle>::new();
    for candidate in candidates {
        if compact_oversized && candidate.original_len > candidate.max_bytes {
            let mut file = open_regular_log_file(&candidate.path, false)?;
            compact_open_file_to_tail(&mut file, candidate.retain_bytes, candidate.tail_boundary)
                .with_context(|| {
                format!(
                    "기존 세션 로그 상한 적용 실패: {}",
                    candidate.path.display()
                )
            })?;
        }
        let len = path_len_or_zero(&candidate.path);
        let bundle = by_dir.entry(candidate.dir).or_default();
        bundle.paths.push((candidate.path, len));
        bundle.bytes = bundle.bytes.saturating_add(len);
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
    collect_known_session_logs(dir, entry_limit, scan)?;
    if !scan.complete {
        return Ok(());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("로그 디렉터리 나열 실패: {}", dir.display()));
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() && depth < 4 {
            scan_session_log_directory(&entry.path(), depth + 1, entry_limit, scan)?;
            if !scan.complete {
                return Ok(());
            }
            continue;
        }
    }
    Ok(())
}

fn collect_known_session_logs(
    dir: &Path,
    entry_limit: usize,
    scan: &mut SessionLogScan,
) -> anyhow::Result<()> {
    for name in [ANSI_LOG_FILE, PLAIN_LOG_FILE, EVENTS_LOG_FILE] {
        let path = dir.join(name);
        let metadata = match path.symlink_metadata() {
            Ok(metadata) if metadata.file_type().is_file() => metadata,
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("세션 로그 metadata 실패: {}", path.display()));
            }
        };
        let (max_bytes, retain_bytes, tail_boundary) =
            log_limits_for_name(std::ffi::OsStr::new(name)).expect("known log name");
        scan.entries_seen = scan.entries_seen.saturating_add(1);
        if scan.entries_seen > entry_limit {
            scan.complete = false;
            return Ok(());
        }
        scan.candidates.push(SessionLogCandidate {
            dir: dir.to_path_buf(),
            path,
            original_len: metadata.len(),
            modified: metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
            max_bytes,
            retain_bytes,
            tail_boundary,
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
        for index in 0..4 {
            let dir = root.join(format!("session-{index}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(PLAIN_LOG_FILE), b"log").unwrap();
        }

        let bundles = collect_session_log_bundles_with_limit(&root, 4).unwrap();

        assert_eq!(bundles.len(), 4);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn scan_entry_limit_default_exact_boundary_succeeds() {
        let root = temp_root("scan-entry-limit-default-exact");
        for index in 0..SESSION_LOG_SCAN_ENTRY_LIMIT {
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
        for index in 0..5 {
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
        for index in 0..=SESSION_LOG_SCAN_ENTRY_LIMIT {
            let dir = root.join(format!("session-{index}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(PLAIN_LOG_FILE), b"").unwrap();
        }

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
    fn non_log_entries_do_not_starve_bounded_scan() {
        let root = temp_root("scan-entry-limit-noise");
        for index in 0..=SESSION_LOG_SCAN_ENTRY_LIMIT {
            std::fs::write(root.join(format!("noise-{index}")), b"ignored").unwrap();
        }
        let session_dir = root.join("session");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join(PLAIN_LOG_FILE), b"log").unwrap();

        let bundles = collect_session_log_bundles_with_limit(&root, 1).unwrap();

        assert_eq!(bundles.len(), 1);
        std::fs::remove_dir_all(&root).unwrap();
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
