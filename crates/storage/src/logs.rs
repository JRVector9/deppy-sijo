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

struct BoundedLogFile {
    file: File,
    path: PathBuf,
    max_bytes: u64,
}

impl BoundedLogFile {
    fn open(path: &Path, max_bytes: u64) -> anyhow::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)
            .with_context(|| format!("로그 파일 열기 실패: {}", path.display()))?;
        let mut bounded = Self {
            file,
            path: path.to_path_buf(),
            max_bytes,
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
        let current = self.len().unwrap_or(0);
        if current.saturating_add(incoming) > self.max_bytes {
            let retain = self
                .max_bytes
                .saturating_sub(incoming)
                .min(self.max_bytes / 2);
            compact_open_file_to_tail(&mut self.file, retain)
                .with_context(|| format!("로그 tail 압축 실패: {}", self.path.display()))?;
        }
        if incoming >= self.max_bytes {
            let start = bytes.len().saturating_sub(self.max_bytes as usize);
            self.file
                .write_all(&bytes[start..])
                .with_context(|| format!("로그 기록 실패: {}", self.path.display()))?;
        } else {
            self.file
                .write_all(bytes)
                .with_context(|| format!("로그 기록 실패: {}", self.path.display()))?;
        }
        Ok(())
    }

    fn compact_if_oversized(&mut self, retain_bytes: u64) -> anyhow::Result<()> {
        if self.len().unwrap_or(0) > self.max_bytes {
            compact_open_file_to_tail(&mut self.file, retain_bytes)
                .with_context(|| format!("기존 로그 tail 압축 실패: {}", self.path.display()))?;
        }
        Ok(())
    }

    fn flush(&mut self) {
        self.file.flush().ok();
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
        let dir = Self::session_dir_key(logs_root, session_key)?;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("로그 디렉터리 생성 실패: {}", dir.display()))?;
        Ok(Self {
            ansi: BoundedLogFile::open(&dir.join(ANSI_LOG_FILE), ANSI_LOG_MAX_BYTES)?,
            plain: BoundedLogFile::open(&dir.join(PLAIN_LOG_FILE), PLAIN_LOG_MAX_BYTES)?,
            events: BoundedLogFile::open(&dir.join(EVENTS_LOG_FILE), EVENTS_LOG_MAX_BYTES)?,
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
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("터미널 크기 읽기 실패: {}", path.display()));
            }
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
        std::fs::write(&path, format!("{cols} {rows}\n"))
            .with_context(|| format!("터미널 크기 기록 실패: {}", path.display()))
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

/// 열린 파일의 끝 `retain_bytes`만 같은 inode에 다시 쓴다. writer handle을 교체/rename하지
/// 않으므로 런타임이 계속 가진 append handle에도 즉시 적용된다. 첫 불완전 줄은 버려 ANSI
/// escape·UTF-8·JSONL 중간에서 재생/미리보기가 시작될 가능성을 줄인다.
fn compact_open_file_to_tail(file: &mut File, retain_bytes: u64) -> std::io::Result<u64> {
    file.flush()?;
    let len = file.metadata()?.len();
    if len <= retain_bytes {
        return Ok(len);
    }
    if retain_bytes == 0 {
        file.set_len(0)?;
        return Ok(0);
    }
    let start = len.saturating_sub(retain_bytes);
    file.seek(std::io::SeekFrom::Start(start))?;
    let mut tail = Vec::with_capacity(usize::try_from(len - start).unwrap_or(0));
    file.read_to_end(&mut tail)?;
    if start > 0 {
        if let Some(newline) = tail.iter().position(|byte| *byte == b'\n') {
            tail.drain(..=newline);
        } else {
            tail.clear();
        }
    }
    file.set_len(0)?;
    file.write_all(&tail)?;
    file.flush()?;
    Ok(tail.len() as u64)
}

struct SessionLogBundle {
    paths: Vec<(PathBuf, u64)>,
    bytes: u64,
    modified: std::time::SystemTime,
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
    let mut bundles = collect_session_log_bundles(logs_root)?;
    let mut total = bundles.iter().map(|bundle| bundle.bytes).sum::<u64>();
    if total <= budget_bytes {
        return Ok(total);
    }
    bundles.sort_by_key(|bundle| bundle.modified);
    for bundle in bundles {
        if total <= budget_bytes {
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
    Ok(total)
}

fn path_len_or_zero(path: &Path) -> u64 {
    path.metadata().map(|metadata| metadata.len()).unwrap_or(0)
}

fn collect_session_log_bundles(logs_root: &Path) -> anyhow::Result<Vec<SessionLogBundle>> {
    let mut by_dir = std::collections::HashMap::<PathBuf, SessionLogBundle>::new();
    let mut stack = vec![(logs_root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("로그 디렉터리 나열 실패: {}", dir.display()));
            }
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() && depth < 4 {
                stack.push((entry.path(), depth + 1));
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some((max_bytes, retain_bytes)) = log_limits_for_name(&name) else {
                continue;
            };
            let path = entry.path();
            let original = path
                .metadata()
                .with_context(|| format!("세션 로그 metadata 실패: {}", path.display()))?;
            if original.len() > max_bytes {
                let mut file = OpenOptions::new().read(true).append(true).open(&path)?;
                compact_open_file_to_tail(&mut file, retain_bytes).with_context(|| {
                    format!("기존 세션 로그 상한 적용 실패: {}", path.display())
                })?;
            }
            let len = path_len_or_zero(&path);
            let bundle = by_dir.entry(dir.clone()).or_default();
            bundle.paths.push((path, len));
            bundle.bytes = bundle.bytes.saturating_add(len);
            let modified = original.modified().unwrap_or(std::time::UNIX_EPOCH);
            bundle.modified = bundle.modified.max(modified);
        }
    }
    Ok(by_dir.into_values().collect())
}

fn log_limits_for_name(name: &std::ffi::OsStr) -> Option<(u64, u64)> {
    match name.to_str()? {
        ANSI_LOG_FILE => Some((ANSI_LOG_MAX_BYTES, ANSI_LOG_MAX_BYTES / 2)),
        PLAIN_LOG_FILE => Some((PLAIN_LOG_MAX_BYTES, PLAIN_LOG_MAX_BYTES / 2)),
        EVENTS_LOG_FILE => Some((EVENTS_LOG_MAX_BYTES, EVENTS_LOG_MAX_BYTES / 2)),
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
    fn bounded_log는_상한을_넘으면_최근_완전한_줄만_남긴다() {
        let root = temp_root("bounded-tail");
        let path = root.join("bounded.log");
        let mut log = BoundedLogFile::open(&path, 64).unwrap();
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
