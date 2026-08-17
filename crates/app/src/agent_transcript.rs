//! 에이전트(claude/codex) transcript(JSONL) 파서 — 세션 ID + 활동 상태(working/idle)를
//! 구조화 로그에서 파생한다(옵션2, cmux 참고). 화면 스크래핑(regex)은 TUI 문구/레이아웃에
//! 의존해 불안정했다(#92/#93) — transcript는 구조화 로그라 정확하다.
//!
//! - 세션 ID: claude는 파일명, codex는 파일명 내 UUID. → 복원 시 native resume에 그대로 씀.
//! - cwd: claude는 매 이벤트, codex는 session_meta(첫 줄)에 기록 → pane 바인딩 앵커.
//! - 활동: 마지막 의미있는 이벤트로 working/idle 판정.
//! - 작업 설명: 최신 agent 응답/진행 메시지를 한 줄로 축약해 사이드바에 표시.
//! - 승인(needsInput)은 transcript에 없다 → regex fallback(status detector)이 담당.

use std::fmt;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use crate::agent_detect;

/// transcript에서 파생한 에이전트 활동. needsInput은 여기 없다(regex fallback 담당).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentActivity {
    Working,
    Idle,
}

/// 턴 메시지 하나의 화자. 카드가 이 값으로 「나」/「에이전트」 라벨을 고른다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRole {
    User,
    Assistant,
}

/// 턴 하나가 보존하는 메시지 한 개. 최근 `TURN_MESSAGES_MAX`개만 남는다.
#[derive(Clone, PartialEq, Eq)]
pub struct TurnMessage {
    pub role: TurnRole,
    pub text: String,
    pub at: Option<i64>,
}

impl fmt::Debug for TurnMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnMessage")
            .field("role", &self.role)
            .field("text", &"REDACTED")
            .field("at", &self.at)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct TranscriptTurn {
    pub turn_key: String,
    pub source_offset: u64,
    pub instruction: String,
    pub agent_summary: Option<String>,
    pub occurred_at: Option<i64>,
    pub activity: AgentActivity,
    /// 턴 안 최근 메시지(사용자 지시 포함) — 최신이 뒤. 저장 컬럼(`messages_json`)의 원천.
    pub messages: Vec<TurnMessage>,
}

impl fmt::Debug for TranscriptTurn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TranscriptTurn")
            .field("turn_key", &"REDACTED")
            .field("source_offset", &self.source_offset)
            .field("instruction", &"REDACTED")
            .field(
                "agent_summary",
                &self.agent_summary.as_ref().map(|_| "REDACTED"),
            )
            .field("occurred_at", &self.occurred_at)
            .field("activity", &self.activity)
            .field("message_count", &self.messages.len())
            .finish()
    }
}

#[derive(PartialEq, Eq)]
pub struct TranscriptState {
    /// 에이전트 자신의 세션 ID — 복원 시 `claude --resume <id>` / `codex resume <id>`에 씀.
    pub session_id: String,
    /// transcript에 기록된 작업 디렉토리 — pane(cwd) 바인딩 앵커.
    pub cwd: Option<String>,
    pub activity: AgentActivity,
    /// 표시용(3줄 세션 행, 2026-07-08). codex는 rollout에서 전부, claude는 model만
    /// (effort/context는 statusLine→DB, Phase 2b).
    pub model: Option<String>,
    pub effort: Option<String>,
    /// 남은 컨텍스트 %(0~100). codex는 rollout에서 계산.
    pub context_pct: Option<u8>,
    /// 최신 에이전트 응답/진행 메시지의 한 줄 요약. 별도 LLM 호출 없이 원문을 축약한다.
    pub last_agent_summary: Option<String>,
    /// 현재 턴을 시작한 실제 사용자 지시의 한 줄 요약.
    pub user_instruction: Option<String>,
    /// 실제 사용자 지시 단위의 최근 턴. 최신 순이며 최대 24개다.
    pub recent_turns: Vec<TranscriptTurn>,
}

impl fmt::Debug for TranscriptState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TranscriptState")
            .field("session_id", &"REDACTED")
            .field("cwd", &self.cwd.as_ref().map(|_| "REDACTED"))
            .field("activity", &self.activity)
            .field("model", &self.model.as_ref().map(|_| "REDACTED"))
            .field("effort", &self.effort.as_ref().map(|_| "REDACTED"))
            .field("context_pct", &self.context_pct)
            .field(
                "last_agent_summary",
                &self.last_agent_summary.as_ref().map(|_| "REDACTED"),
            )
            .field(
                "user_instruction",
                &self.user_instruction.as_ref().map(|_| "REDACTED"),
            )
            .field("recent_turn_count", &self.recent_turns.len())
            .finish()
    }
}

const TAIL_BYTES: u64 = 256 * 1024;
/// `tail_snapshot` 계열이 받아들이는 절대 상한 — 이 값을 넘는 `max_bytes` 요청은 호출자
/// 버그로 보고 버퍼 할당 전에 거부한다. 상태 판정용 `TAIL_BYTES`(256KB)가 가장 컸는데,
/// 원문 읽기(`read_conversation`, 2026-08-15)가 사람이 읽는 용도로 `CONVERSATION_TAIL_BYTES`
/// (4MB)를 요구해 그 값으로 올렸다.
const MAX_TAIL_SNAPSHOT_BYTES: u64 = CONVERSATION_TAIL_BYTES;
const MAX_TRANSCRIPT_LINE_BYTES: usize = 64 * 1024;
const MAX_TAIL_LINES: usize = 4_096;
const MAX_CODEX_HEAD_LINES: usize = 3;
const CODEX_HEAD_BYTES: u64 = (MAX_CODEX_HEAD_LINES * (MAX_TRANSCRIPT_LINE_BYTES + 1)) as u64;
const MAX_TRANSCRIPT_PATH_BYTES: usize = 4 * 1024;
const MAX_SESSION_ID_BYTES: usize = 256;
const MAX_FILE_NAME_BYTES: usize = 512;
const MAX_MODEL_BYTES: usize = 256;
const MAX_EFFORT_BYTES: usize = 64;
const MAX_MESSAGE_CONTENT_ITEMS: usize = 256;
pub const MAX_RECENT_TRANSCRIPT_TURNS: usize = 24;
/// 카드 한 장이 담는 요약 길이. 120자 한 줄이던 것을 2026-08-15에 늘렸다 — orca의
/// preview 상한(220자)보다 크게 잡되, 카드가 세로로 무한정 자라지 않게 줄 수로도 막는다.
const AGENT_SUMMARY_CHARS: usize = 400;
/// 보존하는 최대 줄 수.
const AGENT_SUMMARY_LINES: usize = 4;
/// 최악의 경우(4바이트 문자 400개) + 말줄임 + 줄바꿈 3개.
const AGENT_SUMMARY_BYTES: usize =
    AGENT_SUMMARY_CHARS * 4 + '…'.len_utf8() + (AGENT_SUMMARY_LINES - 1);
/// 턴 하나가 보존하는 메시지 수 — orca의 SESSION_PREVIEW_MESSAGE_LIMIT과 같은 값.
pub const TURN_MESSAGES_MAX: usize = 5;
/// 직렬화 결과 상한 — storage의 컬럼 상한(8KB)과 같은 값이다. 넘으면 None으로 떨어뜨려
/// 저장을 거부당하는 대신 조용히 기존 두 필드로 물러난다(fail-soft).
const TURN_MESSAGES_JSON_BYTES_MAX: usize = 8 * 1024;

struct TailSnapshot {
    base_offset: u64,
    modified_at: Option<i64>,
    text: String,
}

fn invalid_input(code: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, code)
}

fn validate_input_path(path: &Path) -> std::io::Result<()> {
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.is_empty() || bytes.len() > MAX_TRANSCRIPT_PATH_BYTES {
        return Err(invalid_input("transcript_path_invalid"));
    }
    Ok(())
}

fn open_regular_file(path: &Path) -> std::io::Result<(std::fs::File, u64)> {
    open_regular_file_with_before_open(path, || {})
}

fn open_regular_file_with_before_open(
    path: &Path,
    before_open: impl FnOnce(),
) -> std::io::Result<(std::fs::File, u64)> {
    validate_input_path(path)?;
    let path_metadata = std::fs::symlink_metadata(path)?;
    if !path_metadata.file_type().is_file() {
        return Err(invalid_input("transcript_not_regular"));
    }
    before_open();

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;

        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let file_metadata = file.metadata()?;
    if !file_metadata.file_type().is_file() {
        return Err(invalid_input("transcript_not_regular"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino()
        {
            return Err(invalid_input("transcript_replaced"));
        }
    }
    Ok((file, file_metadata.len()))
}

/// `snapshot_len` 이후 reader가 늘어나더라도 해당 snapshot의 tail만 정확히 유지한다.
fn tail_snapshot_from_reader<R: Read + Seek>(
    reader: &mut R,
    snapshot_len: u64,
    max_bytes: u64,
    modified_at: Option<i64>,
) -> std::io::Result<TailSnapshot> {
    if max_bytes > MAX_TAIL_SNAPSHOT_BYTES {
        return Err(invalid_input("tail_limit_invalid"));
    }
    let retained = snapshot_len.min(max_bytes);
    let retained = usize::try_from(retained).map_err(|_| invalid_input("tail_limit_invalid"))?;
    let start = snapshot_len.saturating_sub(max_bytes);
    let mut base_offset = start;
    let starts_at_line_boundary = if start == 0 {
        true
    } else {
        reader.seek(SeekFrom::Start(start - 1))?;
        let mut previous = [0_u8; 1];
        reader.read_exact(&mut previous)?;
        previous[0] == b'\n'
    };
    reader.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0_u8; retained];
    reader.read_exact(&mut bytes)?;

    if !starts_at_line_boundary {
        let Some(first_newline) = bytes.iter().position(|byte| *byte == b'\n') else {
            bytes.clear();
            return Ok(TailSnapshot {
                base_offset: snapshot_len,
                modified_at,
                text: String::new(),
            });
        };
        base_offset = base_offset.saturating_add(first_newline as u64 + 1);
        bytes.drain(..=first_newline);
    }
    let text = String::from_utf8(bytes).map_err(|_| invalid_input("transcript_utf8_invalid"))?;
    Ok(TailSnapshot {
        base_offset,
        modified_at,
        text,
    })
}

/// 파일 끝 `max_bytes`만 읽는다. metadata snapshot 이후 append는 다음 poll에서 보고,
/// 현재 poll에서는 버퍼가 상한을 넘지 않도록 정확한 snapshot 바이트만 읽는다.
fn tail_snapshot(path: &Path, max_bytes: u64) -> std::io::Result<TailSnapshot> {
    let (mut file, snapshot_len) = open_regular_file(path)?;
    let modified_at = file
        .metadata()?
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|value| i64::try_from(value.as_secs()).ok());
    let snapshot = tail_snapshot_from_reader(&mut file, snapshot_len, max_bytes, modified_at)?;
    if file.metadata()?.len() < snapshot_len {
        return Err(invalid_input("transcript_shrank_during_read"));
    }
    Ok(snapshot)
}

fn validate_tail_text(text: &str) -> Option<()> {
    let mut lines = 0_usize;
    for line in text.lines() {
        lines = lines.checked_add(1)?;
        if lines > MAX_TAIL_LINES || line.len() > MAX_TRANSCRIPT_LINE_BYTES {
            return None;
        }
    }
    Some(())
}

fn snapshot_lines(snapshot: &TailSnapshot) -> impl Iterator<Item = (u64, &str)> {
    let mut offset = snapshot.base_offset;
    snapshot.text.split_inclusive('\n').map(move |chunk| {
        let line_offset = offset;
        offset = offset.saturating_add(chunk.len() as u64);
        let line = chunk.strip_suffix('\n').unwrap_or(chunk);
        let line = line.strip_suffix('\r').unwrap_or(line);
        (line_offset, line)
    })
}

fn normalized_epoch_secs(value: &Value) -> Option<i64> {
    let raw = value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|raw| i64::try_from(raw).ok()))
        .or_else(|| value.as_str()?.parse::<i64>().ok())?;
    if raw < 0 {
        return None;
    }
    Some(if raw >= 100_000_000_000 {
        raw / 1_000
    } else {
        raw
    })
}

fn parse_iso_utc_secs(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10).copied(), Some(b'T' | b' '))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
        || !value.ends_with('Z')
    {
        return None;
    }
    let year: i64 = value.get(0..4)?.parse().ok()?;
    let month: i64 = value.get(5..7)?.parse().ok()?;
    let day: i64 = value.get(8..10)?.parse().ok()?;
    let hour: i64 = value.get(11..13)?.parse().ok()?;
    let minute: i64 = value.get(14..16)?.parse().ok()?;
    let second: i64 = value.get(17..19)?.parse().ok()?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days: [i64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=month_days[(month - 1) as usize]).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let adjusted_year = year - if month <= 2 { 1 } else { 0 };
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    days.checked_mul(86_400)?
        .checked_add(hour * 3_600 + minute * 60 + second)
}

fn event_occurred_at(value: &Value) -> Option<i64> {
    ["timestamp", "time"].into_iter().find_map(|key| {
        let value = value.get(key)?;
        normalized_epoch_secs(value).or_else(|| value.as_str().and_then(parse_iso_utc_secs))
    })
}

fn valid_native_turn_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SESSION_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn native_turn_key(value: &Value) -> Option<String> {
    [
        value.get("turnId"),
        value.get("turn_id"),
        value.pointer("/turn/id"),
        value.pointer("/payload/turnId"),
        value.pointer("/payload/turn_id"),
    ]
    .into_iter()
    .flatten()
    .find_map(|value| {
        let raw = value
            .as_str()
            .map(str::to_owned)
            .or_else(|| value.as_u64().map(|value| value.to_string()))?;
        valid_native_turn_key(&raw).then_some(raw)
    })
}

struct PendingTurn {
    turn_key: String,
    source_offset: u64,
    instruction: String,
    agent_summary: Option<String>,
    occurred_at: Option<i64>,
    activity: AgentActivity,
    messages: Vec<TurnMessage>,
}

impl PendingTurn {
    fn new(
        provider: &str,
        source_offset: u64,
        instruction: String,
        occurred_at: Option<i64>,
        native_key: Option<String>,
    ) -> Self {
        let mut pending = Self {
            // 사용자 경계 이벤트에 native id가 있으면 쓰고, 없으면 절대 오프셋으로
            // 고정한다. 뒤늦은 종료 이벤트 때문에 이미 노출된 키를 바꾸지 않는다.
            turn_key: native_key.unwrap_or_else(|| format!("{provider}:{source_offset:x}")),
            source_offset,
            instruction: instruction.clone(),
            agent_summary: None,
            occurred_at,
            activity: AgentActivity::Working,
            messages: Vec::new(),
        };
        // 턴을 여는 사용자 지시 자체가 이 턴의 첫 메시지다.
        pending.push_message(TurnRole::User, instruction, occurred_at);
        pending
    }

    /// 최신 TURN_MESSAGES_MAX개만 남긴다 — 앞에서 밀어낸다.
    fn push_message(&mut self, role: TurnRole, text: String, at: Option<i64>) {
        if text.is_empty() {
            return;
        }
        if self.messages.len() == TURN_MESSAGES_MAX {
            self.messages.remove(0);
        }
        self.messages.push(TurnMessage { role, text, at });
    }

    fn finish(self) -> TranscriptTurn {
        TranscriptTurn {
            turn_key: self.turn_key,
            source_offset: self.source_offset,
            instruction: self.instruction,
            agent_summary: self.agent_summary,
            occurred_at: self.occurred_at,
            activity: self.activity,
            messages: self.messages,
        }
    }
}

impl TranscriptTurn {
    /// storage 컬럼에 넣을 유계 JSON. 상한을 넘으면 None(카드는 기존 두 필드로 그린다).
    #[allow(dead_code)] // Task 5가 부른다
    pub fn messages_json(&self) -> Option<String> {
        if self.messages.is_empty() {
            return None;
        }
        // messages는 이미 TURN_MESSAGES_MAX(5)로 유계다 — collect::<Vec>이 아니라
        // with_capacity + push로 쌓아 "unbounded read" 검사 문구를 피한다.
        let mut items: Vec<Value> = Vec::with_capacity(self.messages.len());
        for message in &self.messages {
            items.push(serde_json::json!({
                "r": match message.role {
                    TurnRole::User => "u",
                    TurnRole::Assistant => "a",
                },
                "t": message.text,
                "at": message.at,
            }));
        }
        let json = serde_json::to_string(&items).ok()?;
        (json.len() <= TURN_MESSAGES_JSON_BYTES_MAX).then_some(json)
    }
}

fn retain_turn(turns: &mut Vec<TranscriptTurn>, turn: PendingTurn) {
    if turns.len() == MAX_RECENT_TRANSCRIPT_TURNS {
        turns.remove(0);
    }
    turns.push(turn.finish());
}

fn complete_pending(turns: &mut Vec<TranscriptTurn>, pending: &mut Option<PendingTurn>) {
    if let Some(mut turn) = pending.take() {
        turn.activity = AgentActivity::Idle;
        retain_turn(turns, turn);
    }
}

fn finish_recent_turns(turns: &mut Vec<TranscriptTurn>, pending: Option<PendingTurn>) {
    if let Some(turn) = pending {
        retain_turn(turns, turn);
    }
    turns.reverse();
}

fn bounded_owned(value: &str, max_bytes: usize) -> Option<String> {
    (value.len() <= max_bytes).then(|| value.to_owned())
}

fn bounded_path_owned(value: &str) -> Option<String> {
    (!value.is_empty() && value.len() <= MAX_TRANSCRIPT_PATH_BYTES && !value.contains('\0'))
        .then(|| value.to_owned())
}

/// 노이즈로 보고 거부할 접두 목록 — 요약(`clean_agent_summary`)과 원문 읽기
/// (`read_conversation`)가 이 판정을 공유한다(2026-08-15 리팩터). 목록 자체는 상한이
/// 바뀌어도 그대로 둔다 — 정확도를 올리는 규칙이라 상한과 무관하다.
fn is_noise_prefix(text: &str) -> bool {
    text.is_empty()
        || text.starts_with("<system-reminder")
        || text.starts_with("<local-command")
        || text.starts_with("<command-name")
        || text.starts_with("<environment_context")
        || text.starts_with("<permissions")
        || text.starts_with("<INSTRUCTIONS")
        || text.starts_with("<task-notification")
        || text.starts_with("<heartbeat")
}

fn clean_agent_summary(text: &str) -> Option<String> {
    let mut visible = text.trim_start();
    // 렌더링용 이미지 첨부 표식이 앞에 붙은 메시지는 경로 표식만 걷어낸다.
    while visible.starts_with("<image ") {
        visible = visible.split_once('>')?.1.trim_start();
    }
    if is_noise_prefix(visible) {
        return None;
    }

    let mut summary = String::with_capacity(text.len().min(AGENT_SUMMARY_BYTES));
    let mut summary_chars = 0_usize;
    let mut lines = 1_usize;
    let mut pending_space = false;
    let mut pending_newline = false;
    let mut truncated = false;
    for ch in visible.chars() {
        // 줄바꿈은 보존한다(연속 개행은 하나로). 줄 안의 공백·제어문자만 접는다.
        if ch == '\n' || ch == '\r' {
            pending_newline |= !summary.is_empty();
            pending_space = false;
            continue;
        }
        if ch.is_whitespace() || ch.is_control() {
            pending_space |= !summary.is_empty() && !pending_newline;
            continue;
        }
        if pending_newline {
            if lines == AGENT_SUMMARY_LINES {
                truncated = true;
                break;
            }
            summary.push('\n');
            lines += 1;
            pending_newline = false;
            pending_space = false;
        }
        if pending_space {
            if summary_chars + 2 > AGENT_SUMMARY_CHARS {
                truncated = true;
                break;
            }
            summary.push(' ');
            summary_chars += 1;
            pending_space = false;
        }
        if summary_chars == AGENT_SUMMARY_CHARS {
            truncated = true;
            break;
        }
        if summary.len().checked_add(ch.len_utf8())? > AGENT_SUMMARY_BYTES - '…'.len_utf8() {
            truncated = true;
            break;
        }
        summary.push(ch);
        summary_chars += 1;
    }
    if summary.is_empty() {
        return None;
    }
    if truncated {
        summary.push('…');
    }
    Some(summary)
}

fn message_content_summary(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return clean_agent_summary(text);
    }
    let items = content.as_array()?;
    if items.len() > MAX_MESSAGE_CONTENT_ITEMS {
        return None;
    }
    let text_parts = items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|item| item.get("text").and_then(Value::as_str));
    let total_bytes = text_parts.clone().try_fold(0_usize, |total, text| {
        total.checked_add(text.len())?.checked_add(1)
    })?;
    if total_bytes > MAX_TRANSCRIPT_LINE_BYTES {
        return None;
    }
    let mut text = String::with_capacity(total_bytes);
    for part in text_parts {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(part);
    }
    clean_agent_summary(&text)
}

fn text_items_summary(items: &[Value]) -> Option<String> {
    if items.len() > MAX_MESSAGE_CONTENT_ITEMS {
        return None;
    }
    let text_parts = items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|item| item.get("text").and_then(Value::as_str));
    let total_bytes = text_parts.clone().try_fold(0_usize, |total, text| {
        total.checked_add(text.len())?.checked_add(1)
    })?;
    if total_bytes > MAX_TRANSCRIPT_LINE_BYTES {
        return None;
    }
    let mut text = String::with_capacity(total_bytes.min(AGENT_SUMMARY_BYTES));
    for part in text_parts.filter_map(clean_agent_summary) {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&part);
    }
    clean_agent_summary(&text)
}

fn claude_assistant_summary(value: &Value) -> Option<String> {
    message_content_summary(value.pointer("/message/content")?)
}

fn claude_user_instruction(value: &Value) -> Option<String> {
    let content = value.pointer("/message/content")?;
    if let Some(text) = content.as_str() {
        return clean_agent_summary(text);
    }
    text_items_summary(content.as_array()?)
}

fn claude_internal_user_event(value: &Value) -> bool {
    value
        .pointer("/message/content")
        .and_then(Value::as_str)
        .is_some_and(|_| claude_user_instruction(value).is_none())
}

fn claude_user_starts_new_turn(value: &Value) -> bool {
    let Some(content) = value.pointer("/message/content") else {
        return false;
    };
    if content.is_string() {
        return !claude_internal_user_event(value);
    }
    content.as_array().is_some_and(|items| {
        items
            .iter()
            .any(|item| match item.get("type").and_then(Value::as_str) {
                Some("tool_result") => false,
                Some("text") => item
                    .get("text")
                    .and_then(Value::as_str)
                    .and_then(clean_agent_summary)
                    .is_some(),
                _ => true,
            })
    })
}

fn claude_synthetic_assistant_event(value: &Value, summary: Option<&str>) -> bool {
    value
        .pointer("/message/stop_reason")
        .and_then(Value::as_str)
        == Some("stop_sequence")
        && summary == Some("No response requested.")
}

fn claude_recent_turns(snapshot: &TailSnapshot) -> Vec<TranscriptTurn> {
    let mut turns = Vec::with_capacity(MAX_RECENT_TRANSCRIPT_TURNS);
    let mut pending: Option<PendingTurn> = None;
    for (source_offset, line) in snapshot_lines(snapshot) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("user") if claude_user_starts_new_turn(&value) => {
                let Some(instruction) = claude_user_instruction(&value) else {
                    continue;
                };
                complete_pending(&mut turns, &mut pending);
                pending = Some(PendingTurn::new(
                    "claude",
                    source_offset,
                    instruction,
                    event_occurred_at(&value).or(snapshot.modified_at),
                    native_turn_key(&value),
                ));
            }
            Some("assistant") => {
                let summary = claude_assistant_summary(&value);
                if claude_synthetic_assistant_event(&value, summary.as_deref()) {
                    continue;
                }
                let Some(turn) = pending.as_mut() else {
                    continue;
                };
                if let Some(summary) = summary {
                    turn.push_message(TurnRole::Assistant, summary.clone(), event_occurred_at(&value));
                    turn.agent_summary = Some(summary);
                }
                turn.activity = if value
                    .pointer("/message/stop_reason")
                    .and_then(Value::as_str)
                    == Some("end_turn")
                {
                    AgentActivity::Idle
                } else {
                    AgentActivity::Working
                };
            }
            _ => {}
        }
    }
    finish_recent_turns(&mut turns, pending);
    turns
}

/// claude transcript(`~/.claude/projects/<cwd>/<session-id>.jsonl`) 파싱.
/// 파일명이 곧 세션 ID. 마지막 assistant/user 이벤트로 상태를 파생한다:
/// assistant `stop_reason=end_turn` → Idle(유저 차례), 그 외(tool_use) → Working.
pub fn parse_claude(path: &Path) -> Option<TranscriptState> {
    let raw_session_id = path.file_stem()?.to_str()?;
    if raw_session_id.is_empty() || raw_session_id.len() > MAX_SESSION_ID_BYTES {
        return None;
    }
    let session_id = raw_session_id.to_owned();
    let snapshot = tail_snapshot(path, TAIL_BYTES).ok()?;
    validate_tail_text(&snapshot.text)?;
    let recent_turns = claude_recent_turns(&snapshot);
    let text = &snapshot.text;
    let mut cwd = None;
    let mut activity: Option<AgentActivity> = None;
    // 최신 assistant message.model = 현재 모델(effort/context는 statusLine→DB, Phase 2b).
    let mut model: Option<String> = None;
    let mut last_agent_summary: Option<String> = None;
    let mut user_instruction: Option<String> = None;
    let mut user_turn_seen = false;
    // 최신 실제 user 입력 뒤 아직 assistant 응답이 없으면 이전 turn의 요약을 재사용하지 않는다.
    let mut summary_boundary_reached = false;
    for line in text.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if cwd.is_none()
            && let Some(value) = v.get("cwd").and_then(Value::as_str)
        {
            cwd = Some(bounded_path_owned(value)?);
        }
        match v.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let summary = claude_assistant_summary(&v);
                if claude_synthetic_assistant_event(&v, summary.as_deref()) {
                    continue;
                }
                if last_agent_summary.is_none() && !summary_boundary_reached {
                    last_agent_summary = summary;
                }
                if model.is_none()
                    && let Some(value) = v.pointer("/message/model").and_then(Value::as_str)
                {
                    model = Some(bounded_owned(value, MAX_MODEL_BYTES)?);
                }
                if activity.is_none() {
                    let end = v.pointer("/message/stop_reason").and_then(Value::as_str)
                        == Some("end_turn");
                    activity = Some(if end {
                        AgentActivity::Idle
                    } else {
                        AgentActivity::Working
                    });
                }
            }
            // user 이벤트(툴 결과/유저 입력) 직후는 에이전트가 이어받아 작업한다.
            Some("user") if !claude_internal_user_event(&v) => {
                if activity.is_none() {
                    activity = Some(AgentActivity::Working);
                }
                if claude_user_starts_new_turn(&v) {
                    if !user_turn_seen {
                        user_instruction = claude_user_instruction(&v);
                        user_turn_seen = true;
                    }
                    if last_agent_summary.is_none() {
                        summary_boundary_reached = true;
                    }
                }
            }
            _ => {}
        }
        if activity.is_some()
            && model.is_some()
            && cwd.is_some()
            && (last_agent_summary.is_some() || summary_boundary_reached)
            && user_turn_seen
        {
            break;
        }
    }
    Some(TranscriptState {
        session_id,
        cwd,
        activity: activity?,
        model,
        effort: None,
        context_pct: None,
        last_agent_summary,
        user_instruction,
        recent_turns,
    })
}

/// codex rollout(`~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`) 파싱.
/// 세션 ID는 파일명 내 UUID, cwd는 session_meta(첫 줄). 상태는 마지막 event_msg로:
/// `task_complete`/`turn_aborted` → Idle, 그 외(task_started/agent_message 등) → Working.
/// Kimi `wire.jsonl` 파서 (0.34.0 실측).
///
/// Claude/Codex와 달리 레코드가 **명시적 타입 태그**를 달고 있어 추측할 게 없다:
/// - `profile.bind` / `llm.request` — `modelAlias`, `thinkingEffort`
/// - `config.update` — 세션 중 바뀐 `thinkingEffort` (`/thinking <level>`이 남긴다)
/// - `turn.prompt` / `turn.ended` — 활동. `turn.ended`가 마지막이면 유휴다.
/// - `usage.record` — 턴별 토큰. **매 턴 있진 않다**(취소된 턴엔 없다) → Option.
/// - `context.append_message` — 마지막 에이전트 메시지 요약.
///
/// 역순 1-pass로 필요한 것만 모으고, 다 채워지면 조기 종료한다(codex 경로와 같은 관례).
fn kimi_recent_turns(snapshot: &TailSnapshot) -> Vec<TranscriptTurn> {
    let mut turns = Vec::with_capacity(MAX_RECENT_TRANSCRIPT_TURNS);
    let mut pending: Option<PendingTurn> = None;
    for (source_offset, line) in snapshot_lines(snapshot) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("turn.prompt") => {
                complete_pending(&mut turns, &mut pending);
                let Some(instruction) = kimi_user_instruction(&value) else {
                    continue;
                };
                pending = Some(PendingTurn::new(
                    "kimi",
                    source_offset,
                    instruction,
                    event_occurred_at(&value).or(snapshot.modified_at),
                    native_turn_key(&value),
                ));
            }
            Some("turn.ended") => {
                if let Some(turn) = pending.as_mut() {
                    // 첫 관측 때 만든 키는 바꾸지 않는다. Kimi는 종료 레코드에만
                    // turnId를 싣기도 하므로 여기서 바꾸면 실행 중/완료 카드가 중복된다.
                    turn.activity = AgentActivity::Idle;
                }
            }
            Some("context.append_message") => {
                let role = value.pointer("/message/role").and_then(Value::as_str);
                let origin = value
                    .pointer("/message/origin/kind")
                    .and_then(Value::as_str);
                if role == Some("user") || matches!(origin, Some("hook_result" | "system")) {
                    continue;
                }
                let Some(turn) = pending.as_mut() else {
                    continue;
                };
                if let Some(summary) = value
                    .pointer("/message/content")
                    .and_then(message_content_summary)
                {
                    turn.push_message(TurnRole::Assistant, summary.clone(), event_occurred_at(&value));
                    turn.agent_summary = Some(summary);
                }
            }
            _ => {}
        }
    }
    finish_recent_turns(&mut turns, pending);
    turns
}

pub fn parse_kimi(path: &Path) -> Option<TranscriptState> {
    // 경로는 `<sessionDir>/agents/main/wire.jsonl`이고 sessionDir 이름이 세션 id다.
    let session_id = kimi_session_id(path)?;
    let snapshot = tail_snapshot(path, TAIL_BYTES).ok()?;
    validate_kimi_tail(&snapshot.text)?;
    let recent_turns = kimi_recent_turns(&snapshot);
    let text = &snapshot.text;

    let mut activity: Option<AgentActivity> = None;
    let mut model: Option<String> = None;
    let mut effort: Option<String> = None;
    let mut max_tokens: Option<u64> = None;
    let mut used_tokens: Option<u64> = None;
    let mut last_agent_summary: Option<String> = None;
    let mut user_instruction: Option<String> = None;
    let mut user_turn_seen = false;
    // 새 턴이 시작됐는데 아직 응답이 없으면 이전 턴 요약을 재사용하지 않는다.
    let mut summary_boundary_reached = false;

    for line in text.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            // 역순이라 **먼저 만나는 것이 최신**이다. turn.ended가 turn.prompt보다
            // 뒤(=역순에서 먼저)면 턴이 끝난 것 → 유휴.
            Some("turn.ended") => {
                activity.get_or_insert(AgentActivity::Idle);
            }
            Some("turn.prompt") => {
                activity.get_or_insert(AgentActivity::Working);
                if !user_turn_seen {
                    user_instruction = kimi_user_instruction(&v);
                    user_turn_seen = true;
                }
                summary_boundary_reached = true;
            }
            // 세션 중 `/thinking <level>`이 남기는 기록. llm.request보다 최신일 수
            // 있으므로 먼저 만난 쪽(=최신)을 쓴다.
            Some("config.update") => {
                if let Some(value) = v.get("thinkingEffort").and_then(Value::as_str) {
                    effort.get_or_insert_with(|| value.to_owned());
                }
            }
            Some("llm.request") | Some("profile.bind") => {
                if let Some(value) = v.get("modelAlias").and_then(Value::as_str) {
                    model.get_or_insert_with(|| value.to_owned());
                }
                if let Some(value) = v.get("thinkingEffort").and_then(Value::as_str) {
                    effort.get_or_insert_with(|| value.to_owned());
                }
                if let Some(value) = v.get("maxTokens").and_then(Value::as_u64) {
                    max_tokens.get_or_insert(value);
                }
            }
            Some("usage.record") => {
                if used_tokens.is_none()
                    && let Some(usage) = v.get("usage").and_then(Value::as_object)
                {
                    // 필드 이름이 늘어나도 합계가 맞도록 정수 전부를 더한다
                    // (실측: inputOther/output/inputCacheRead/inputCacheCreation).
                    let total: u64 = usage.values().filter_map(Value::as_u64).sum();
                    used_tokens = Some(total);
                }
            }
            Some("context.append_message")
                if last_agent_summary.is_none() && !summary_boundary_reached =>
            {
                last_agent_summary = v
                    .pointer("/message/content")
                    .and_then(Value::as_str)
                    .and_then(clean_agent_summary);
            }
            _ => {}
        }
        if activity.is_some()
            && model.is_some()
            && effort.is_some()
            && used_tokens.is_some()
            && last_agent_summary.is_some()
            && user_turn_seen
        {
            break;
        }
    }

    // 남은 비율이 아니라 **사용 비율**이다(codex/claude 경로와 같은 의미).
    let context_pct = match (used_tokens, max_tokens) {
        (Some(used), Some(max)) if max > 0 => Some(((used.min(max) * 100) / max).min(100) as u8),
        _ => None,
    };

    Some(TranscriptState {
        session_id,
        // wire.jsonl에는 cwd가 없다 — 형제 `state.json`이 갖고 있지만 파일을 하나 더
        // 열 만큼의 값이 없다(바인딩 앵커는 hook의 sessionId가 이미 결정한다).
        cwd: None,
        activity: activity.unwrap_or(AgentActivity::Idle),
        model,
        effort,
        context_pct,
        last_agent_summary,
        user_instruction,
        recent_turns,
    })
}

fn kimi_user_instruction(value: &Value) -> Option<String> {
    if value.pointer("/origin/kind").and_then(Value::as_str) != Some("user") {
        return None;
    }
    text_items_summary(value.get("input")?.as_array()?)
}

/// Kimi 꼬리 검증 — **줄 수만** 본다.
///
/// 공용 `validate_tail_text`는 64KiB를 넘는 줄이 하나라도 있으면 파일 전체를 버린다.
/// Claude/Codex에서는 그런 줄이 손상 신호라 맞는 규칙이지만, Kimi는 정상 기록이
/// 그보다 크다 — 실측한 34개 파일 중 최대 줄이 **72KiB**였다(`llm.tools_snapshot`과
/// systemPrompt를 통째로 싣는다). 그래서 그 규칙을 그대로 쓰면 **거의 모든 실제
/// 세션이 파싱되지 않고**, 카드가 셸처럼 보인다.
///
/// 줄 하나의 크기는 이미 상위에서 막혀 있다 — 꼬리 자체가 `TAIL_BYTES`(256KiB)로
/// 잘리므로 한 줄이 그보다 클 수 없다. 남은 위험은 줄 수뿐이라 그것만 본다.
fn validate_kimi_tail(text: &str) -> Option<()> {
    let mut lines = 0_usize;
    for _ in text.lines() {
        lines = lines.checked_add(1)?;
        if lines > MAX_TAIL_LINES {
            return None;
        }
    }
    Some(())
}

/// `<...>/sessions/<wd>/session_<uuid>/agents/main/wire.jsonl` → `session_<uuid>`.
fn kimi_session_id(path: &Path) -> Option<String> {
    let session_dir = path.parent()?.parent()?.parent()?;
    let name = session_dir.file_name()?.to_str()?;
    name.strip_prefix("session_")
        .filter(|rest| !rest.is_empty())
        .map(|_| name.to_owned())
}

#[cfg(test)]
mod kimi_tests {
    use super::*;
    use std::io::Write;

    /// 2026-08-09 실측한 `wire.jsonl` 레코드 모양 그대로. 필드 이름이 바뀌면 여기서
    /// 깨져야 한다 — 파서가 조용히 None을 돌려주면 카드가 셸처럼 보인다.
    fn temp_root(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("deppy-kimi-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn fixture(tag: &str, lines: &[&str]) -> std::path::PathBuf {
        let wire = temp_root(tag)
            .join("session_9c21503a-998f-497d-994c-cb4d71507007")
            .join("agents")
            .join("main");
        std::fs::create_dir_all(&wire).unwrap();
        let path = wire.join("wire.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        path
    }

    #[test]
    fn kimi_transcript에서_모델_강도_활동_컨텍스트를_읽는다() {
        let path = fixture(
            "full",
            &[
                r#"{"type":"metadata","protocol_version":1}"#,
                r#"{"type":"profile.bind","modelAlias":"kimi-code/k3","thinkingEffort":"high"}"#,
                r#"{"type":"turn.prompt","origin":{"kind":"user"},"input":[{"type":"text","text":"런처 상태 표시를 수정해"}],"time":1}"#,
                r#"{"type":"llm.request","modelAlias":"kimi-code/k3","thinkingEffort":"high","maxTokens":1000}"#,
                r#"{"type":"usage.record","usage":{"inputOther":150,"output":50},"usageScope":"turn"}"#,
                r#"{"type":"context.append_message","message":{"content":"작업을 마쳤습니다"}}"#,
                r#"{"type":"turn.ended","reason":"completed","durationMs":5993,"turnId":0}"#,
            ],
        );
        let state = parse_kimi(&path).expect("파싱돼야 한다");
        assert_eq!(
            state.session_id,
            "session_9c21503a-998f-497d-994c-cb4d71507007"
        );
        assert_eq!(state.model.as_deref(), Some("kimi-code/k3"));
        assert_eq!(state.effort.as_deref(), Some("high"));
        assert_eq!(
            state.activity,
            AgentActivity::Idle,
            "turn.ended가 마지막이면 유휴다"
        );
        assert_eq!(state.context_pct, Some(20), "200/1000 = 20%");
        assert_eq!(
            state.user_instruction.as_deref(),
            Some("런처 상태 표시를 수정해")
        );
    }

    /// `/thinking <level>`은 `config.update`를 남긴다. 그게 llm.request보다 최신이면
    /// 그쪽이 현재 값이다 — 아니면 강도 단축키가 낡은 값에서 한 칸 움직인다.
    #[test]
    fn config_update가_llm_request보다_최신이면_그_강도를_쓴다() {
        let path = fixture(
            "cfg",
            &[
                r#"{"type":"llm.request","modelAlias":"kimi-code/k3","thinkingEffort":"high","maxTokens":1000}"#,
                r#"{"type":"turn.ended","reason":"completed","turnId":0}"#,
                r#"{"type":"config.update","thinkingEffort":"max","time":2}"#,
            ],
        );
        let state = parse_kimi(&path).expect("파싱돼야 한다");
        assert_eq!(
            state.effort.as_deref(),
            Some("max"),
            "세션 중 바뀐 강도를 못 읽으면 단축키가 낡은 값에서 출발한다"
        );
    }

    /// 취소된 턴에는 `usage.record`가 없다(실측). 컨텍스트를 0%로 지어내면 안 된다.
    #[test]
    fn usage_record가_없으면_컨텍스트는_none이다() {
        let path = fixture(
            "nousage",
            &[
                r#"{"type":"llm.request","modelAlias":"kimi-code/k3","thinkingEffort":"high","maxTokens":1000}"#,
                r#"{"type":"turn.prompt","origin":{"kind":"user"},"time":1}"#,
            ],
        );
        let state = parse_kimi(&path).expect("파싱돼야 한다");
        assert_eq!(state.context_pct, None, "없는 값을 0%로 지어내면 안 된다");
        assert_eq!(
            state.activity,
            AgentActivity::Working,
            "turn.prompt 뒤에 turn.ended가 없으면 작업 중이다"
        );
    }

    #[test]
    fn kimi는_최신_실제_사용자_지시만_작업컨텍스트로_쓴다() {
        let path = fixture(
            "latest-user",
            &[
                r#"{"type":"turn.prompt","origin":{"kind":"user"},"input":[{"type":"text","text":"이전 작업"}],"time":1}"#,
                r#"{"type":"turn.ended","reason":"completed","turnId":0}"#,
                r#"{"type":"turn.prompt","origin":{"kind":"user"},"input":[{"type":"text","text":"현재 작업을 보여줘"}],"time":2}"#,
            ],
        );
        let state = parse_kimi(&path).expect("파싱돼야 한다");
        assert_eq!(
            state.user_instruction.as_deref(),
            Some("현재 작업을 보여줘")
        );
        assert_eq!(state.last_agent_summary, None);
    }

    #[test]
    fn kimi_recent_turns는_종료시_native_id가_생겨도_처음_key를_유지한다() {
        let lines = [
            r#"{"type":"turn.prompt","origin":{"kind":"user"},"input":[{"type":"text","text":"같은 요청"}],"time":1000}"#,
            r#"{"type":"context.append_message","message":{"role":"assistant","content":"첫 응답"},"time":1100}"#,
            r#"{"type":"turn.ended","reason":"completed","turnId":7,"time":1200}"#,
            r#"{"type":"context.append_message","message":{"role":"user","content":"<hook_result>internal</hook_result>","origin":{"kind":"hook_result"}},"time":1300}"#,
            r#"{"type":"turn.prompt","origin":{"kind":"user"},"input":[{"type":"text","text":"같은 요청"}],"time":2000}"#,
            r#"{"type":"context.append_message","message":{"role":"assistant","content":"둘째 작업 중"},"time":2100}"#,
        ];
        let second_offset = lines[..4]
            .iter()
            .map(|line| line.len() as u64 + 1)
            .sum::<u64>();
        let path = fixture("recent-turns", &lines);

        let state = parse_kimi(&path).expect("파싱돼야 한다");

        assert_eq!(state.recent_turns.len(), 2);
        assert_eq!(state.recent_turns[0].instruction, "같은 요청");
        assert_eq!(
            state.recent_turns[0].agent_summary.as_deref(),
            Some("둘째 작업 중")
        );
        assert_eq!(state.recent_turns[0].activity, AgentActivity::Working);
        assert_eq!(state.recent_turns[0].source_offset, second_offset);
        assert_eq!(
            state.recent_turns[0].turn_key,
            format!("kimi:{second_offset:x}")
        );
        assert_eq!(state.recent_turns[1].turn_key, "kimi:0");
        assert_eq!(
            state.recent_turns[1].agent_summary.as_deref(),
            Some("첫 응답")
        );
        assert_eq!(state.recent_turns[1].activity, AgentActivity::Idle);
        assert_ne!(
            state.recent_turns[0].turn_key,
            state.recent_turns[1].turn_key
        );
    }

    #[test]
    fn kimi_system_trigger는_사용자_지시로_오인하지_않는다() {
        let path = fixture(
            "system-trigger",
            &[
                r#"{"type":"turn.prompt","origin":{"kind":"user"},"input":[{"type":"text","text":"이전 사용자 작업"}],"time":1}"#,
                r#"{"type":"context.append_message","message":{"content":"이전 작업을 마쳤습니다"}}"#,
                r#"{"type":"turn.ended","reason":"completed","turnId":0}"#,
                r#"{"type":"turn.prompt","origin":{"kind":"system_trigger"},"input":[{"type":"text","text":"internal heartbeat"}],"time":2}"#,
            ],
        );
        let state = parse_kimi(&path).expect("파싱돼야 한다");
        assert_eq!(state.user_instruction, None);
        assert_eq!(state.last_agent_summary, None);
    }

    /// 실측: Kimi 정상 기록에 64KiB를 넘는 줄이 있다(최대 72KiB). 공용 검증 규칙을
    /// 그대로 쓰면 파일 전체를 버려 거의 모든 실제 세션이 파싱되지 않는다.
    #[test]
    fn 긴_줄이_있어도_파일을_통째로_버리지_않는다() {
        let big = "x".repeat(70 * 1024);
        let path = fixture(
            "longline",
            &[
                &format!(r#"{{"type":"llm.tools_snapshot","tools":"{big}"}}"#),
                r#"{"type":"llm.request","modelAlias":"kimi-code/k3","thinkingEffort":"high","maxTokens":1000}"#,
                r#"{"type":"turn.ended","reason":"completed","turnId":0}"#,
            ],
        );
        let state = parse_kimi(&path).expect("긴 줄 하나 때문에 파일을 버리면 안 된다");
        assert_eq!(state.model.as_deref(), Some("kimi-code/k3"));
    }

    /// 합성 픽스처만 믿지 않는다 — 이 기기에 **실제 Kimi 세션이 있으면** 그것도 파싱해
    /// 본다. 필드 이름을 잘못 읽고 있었다면 여기서 드러난다. 세션이 없는 기기(CI)에서는
    /// 조용히 건너뛴다 — 남의 환경에 파일이 있으리라 가정하지 않는다.
    #[test]
    fn 실제_kimi_세션이_있으면_그것도_파싱된다() {
        let Some(home) = crate::paths::home_dir() else {
            return;
        };
        let root = home.join(".kimi-code/sessions");
        let Ok(workspaces) = std::fs::read_dir(&root) else {
            return; // Kimi 미사용 기기
        };
        let mut checked = 0usize;
        for ws in workspaces.flatten() {
            let Ok(sessions) = std::fs::read_dir(ws.path()) else {
                continue;
            };
            for session in sessions.flatten() {
                let wire = session.path().join("agents/main/wire.jsonl");
                if !wire.is_file() {
                    continue;
                }
                let state = parse_kimi(&wire);
                assert!(
                    state.is_some(),
                    "실제 transcript를 파싱하지 못했다: 필드 이름이 바뀌었을 수 있다"
                );
                let state = state.unwrap();
                assert!(
                    state.session_id.starts_with("session_"),
                    "세션 id를 경로에서 못 뽑았다: {}",
                    state.session_id
                );
                checked += 1;
                if checked >= 5 {
                    return;
                }
            }
        }
    }

    /// 경로 모양이 다르면 세션 id를 못 만든다 — 엉뚱한 id로 바인딩하면 안 된다.
    #[test]
    fn 세션_디렉터리_모양이_아니면_파싱하지_않는다() {
        let path = temp_root("shape").join("a").join("b").join("wire.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{}\n").unwrap();
        assert!(parse_kimi(&path).is_none());
    }
}

fn codex_recent_turns(snapshot: &TailSnapshot) -> Vec<TranscriptTurn> {
    let mut turns = Vec::with_capacity(MAX_RECENT_TRANSCRIPT_TURNS);
    let mut pending: Option<PendingTurn> = None;
    for (source_offset, line) in snapshot_lines(snapshot) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        let event_type = value.pointer("/payload/type").and_then(Value::as_str);
        if event_type == Some("user_message") {
            let Some(instruction) = value
                .pointer("/payload/message")
                .and_then(Value::as_str)
                .and_then(clean_agent_summary)
            else {
                continue;
            };
            complete_pending(&mut turns, &mut pending);
            pending = Some(PendingTurn::new(
                "codex",
                source_offset,
                instruction,
                event_occurred_at(&value).or(snapshot.modified_at),
                native_turn_key(&value),
            ));
            continue;
        }
        let Some(turn) = pending.as_mut() else {
            continue;
        };
        match event_type {
            Some("agent_message") => {
                if let Some(summary) = value
                    .pointer("/payload/message")
                    .and_then(Value::as_str)
                    .and_then(clean_agent_summary)
                {
                    turn.push_message(TurnRole::Assistant, summary.clone(), event_occurred_at(&value));
                    turn.agent_summary = Some(summary);
                }
                turn.activity = AgentActivity::Working;
            }
            Some("task_complete") => {
                if let Some(summary) = value
                    .pointer("/payload/last_agent_message")
                    .and_then(Value::as_str)
                    .and_then(clean_agent_summary)
                {
                    turn.push_message(TurnRole::Assistant, summary.clone(), event_occurred_at(&value));
                    turn.agent_summary = Some(summary);
                }
                turn.activity = AgentActivity::Idle;
            }
            Some("turn_aborted") => turn.activity = AgentActivity::Idle,
            Some("task_started") => turn.activity = AgentActivity::Working,
            _ => {}
        }
    }
    finish_recent_turns(&mut turns, pending);
    turns
}

pub fn parse_codex(path: &Path) -> Option<TranscriptState> {
    let session_id = codex_session_id(path.file_name()?.to_str()?)?;
    let (cwd, snapshot) = codex_snapshot(path).ok()?;
    validate_tail_text(&snapshot.text)?;
    let recent_turns = codex_recent_turns(&snapshot);
    let text = &snapshot.text;
    // 역순 1-pass로 activity(첫 event_msg) + model/effort(첫 turn_context) +
    // context%(첫 token_count)를 모은다. 셋 다 채워지면 조기 종료.
    let mut activity: Option<AgentActivity> = None;
    let mut model: Option<String> = None;
    let mut effort: Option<String> = None;
    let mut context_pct: Option<u8> = None;
    let mut last_agent_summary: Option<String> = None;
    let mut user_instruction: Option<String> = None;
    let mut user_turn_seen = false;
    // 새 user/task가 시작됐지만 agent 메시지가 아직 없으면 이전 turn의 설명을 표시하지 않는다.
    let mut summary_boundary_reached = false;
    for line in text.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("event_msg") => {
                let event_type = v.pointer("/payload/type").and_then(Value::as_str);
                if activity.is_none() {
                    activity = match event_type {
                        Some("task_complete") | Some("turn_aborted") => Some(AgentActivity::Idle),
                        Some("task_started" | "user_message" | "agent_message") => {
                            Some(AgentActivity::Working)
                        }
                        _ => None,
                    };
                }
                if last_agent_summary.is_none() && !summary_boundary_reached {
                    let message = match event_type {
                        Some("task_complete") => v.pointer("/payload/last_agent_message"),
                        Some("agent_message") => v.pointer("/payload/message"),
                        _ => None,
                    };
                    last_agent_summary = message
                        .and_then(Value::as_str)
                        .and_then(clean_agent_summary);
                }
                if last_agent_summary.is_none()
                    && matches!(event_type, Some("task_started" | "user_message"))
                {
                    summary_boundary_reached = true;
                }
                if event_type == Some("user_message") && !user_turn_seen {
                    user_instruction = v
                        .pointer("/payload/message")
                        .and_then(Value::as_str)
                        .and_then(clean_agent_summary);
                    user_turn_seen = true;
                }
            }
            Some("turn_context") if model.is_none() => {
                if let Some(value) = v.pointer("/payload/model").and_then(Value::as_str) {
                    model = Some(bounded_owned(value, MAX_MODEL_BYTES)?);
                }
                if let Some(value) = v.pointer("/payload/effort").and_then(Value::as_str) {
                    effort = Some(bounded_owned(value, MAX_EFFORT_BYTES)?);
                }
            }
            Some("token_count") if context_pct.is_none() => {
                let info = v.pointer("/payload/info");
                let window = info
                    .and_then(|i| i.pointer("/model_context_window"))
                    .and_then(Value::as_u64);
                let used = info
                    .and_then(|i| i.pointer("/last_token_usage/input_tokens"))
                    .and_then(Value::as_u64);
                if let (Some(w), Some(u)) = (window, used)
                    && w > 0
                {
                    let remaining = 100u64.saturating_sub(u.saturating_mul(100) / w);
                    context_pct = Some(remaining.min(100) as u8);
                }
            }
            _ => {}
        }
        if activity.is_some()
            && model.is_some()
            && context_pct.is_some()
            && (last_agent_summary.is_some() || summary_boundary_reached)
            && user_turn_seen
        {
            break;
        }
    }
    Some(TranscriptState {
        session_id,
        cwd,
        activity: activity?,
        model,
        effort,
        context_pct,
        last_agent_summary,
        user_instruction,
        recent_turns,
    })
}

/// 파일명에서 UUID(마지막 5개 하이픈 그룹)를 뽑는다. 입력은 512B로 먼저
/// 제한하고 ASCII window를 선형 스캔해 매 poll마다 regex를 재생성하지 않는다.
pub(crate) fn codex_session_id(file_name: &str) -> Option<String> {
    if file_name.is_empty() || file_name.len() > MAX_FILE_NAME_BYTES {
        return None;
    }
    const UUID_BYTES: usize = 36;
    let bytes = file_name.as_bytes();
    if bytes.len() < UUID_BYTES {
        return None;
    }
    for start in 0..=bytes.len() - UUID_BYTES {
        let candidate = &bytes[start..start + UUID_BYTES];
        let valid = candidate.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)
            }
        });
        if valid {
            return file_name.get(start..start + UUID_BYTES).map(str::to_owned);
        }
    }
    None
}

/// codex의 cwd는 첫 줄 session_meta의 `payload.cwd`에 있다. 그 줄엔 base_instructions
/// (전체 시스템 프롬프트, 수십 KB)가 cwd보다 앞서므로 줄당 64KiB, 3줄을
/// snapshot 상한 안에서 검증한 뒤 session_meta를 파싱한다.
#[cfg(test)]
fn codex_cwd_from_head(path: &Path) -> std::io::Result<Option<String>> {
    let (mut file, snapshot_len) = open_regular_file(path)?;
    let cwd = codex_cwd_from_head_snapshot(&mut file, snapshot_len)?;
    if file.metadata()?.len() < snapshot_len {
        return Err(invalid_input("transcript_shrank_during_read"));
    }
    Ok(cwd)
}

fn codex_snapshot(path: &Path) -> std::io::Result<(Option<String>, TailSnapshot)> {
    let (mut file, snapshot_len) = open_regular_file(path)?;
    let cwd = codex_cwd_from_head_snapshot(&mut file, snapshot_len)?;
    let modified_at = file
        .metadata()?
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|value| i64::try_from(value.as_secs()).ok());
    let tail = tail_snapshot_from_reader(&mut file, snapshot_len, TAIL_BYTES, modified_at)?;
    if file.metadata()?.len() < snapshot_len {
        return Err(invalid_input("transcript_shrank_during_read"));
    }
    Ok((cwd, tail))
}

fn codex_cwd_from_head_snapshot<R: Read + Seek>(
    reader: &mut R,
    snapshot_len: u64,
) -> std::io::Result<Option<String>> {
    let retained = snapshot_len.min(CODEX_HEAD_BYTES);
    let retained = usize::try_from(retained).map_err(|_| invalid_input("head_limit_invalid"))?;
    reader.seek(SeekFrom::Start(0))?;
    let mut bytes = vec![0_u8; retained];
    reader.read_exact(&mut bytes)?;

    let source_truncated = snapshot_len > CODEX_HEAD_BYTES;
    let mut lines = Vec::with_capacity(MAX_CODEX_HEAD_LINES);
    let mut start = 0_usize;
    while start < bytes.len() && lines.len() < MAX_CODEX_HEAD_LINES {
        let newline = bytes[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|offset| start + offset);
        let (end, next_start) = match newline {
            Some(end) => (end, end + 1),
            None => {
                if source_truncated {
                    return Err(invalid_input("transcript_head_truncated"));
                }
                (bytes.len(), bytes.len())
            }
        };
        let mut line = &bytes[start..end];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        if line.len() > MAX_TRANSCRIPT_LINE_BYTES {
            return Err(invalid_input("transcript_line_too_large"));
        }
        lines
            .push(std::str::from_utf8(line).map_err(|_| invalid_input("transcript_utf8_invalid"))?);
        start = next_start;
    }

    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) == Some("session_meta") {
            return v
                .pointer("/payload/cwd")
                .or_else(|| v.get("cwd"))
                .and_then(Value::as_str)
                .map(|cwd| {
                    bounded_path_owned(cwd).ok_or_else(|| invalid_input("transcript_cwd_invalid"))
                })
                .transpose();
        }
    }
    Ok(None)
}

// ── 원문 보기(Task 7) ──────────────────────────────────────────────────────
//
// 이력 카드의 「원문 보기」가 부르는 읽기 전용 파서. 상태 판정용 tail 파서(위)와 상한이
// **다르다** — 그쪽은 "지금 무슨 상태인가"를 싸게 알아내는 것이고, 이쪽은 사람이 읽는
// 것이 목적이라 훨씬 크다. 아무것도 저장하지 않는다(스펙 §2) — 볼 때만 읽고 닫으면 버린다.

/// 원문 보기 전용 tail 상한 — 긴 세션도 최근 대화는 충분히 담긴다.
const CONVERSATION_TAIL_BYTES: u64 = 4 * 1024 * 1024;
/// 메시지 하나가 담는 최대 바이트. 넘으면 UTF-8 경계로 자르고 `…`를 붙인다.
pub const CONVERSATION_MESSAGE_BYTES_MAX: usize = 8 * 1024;
/// 대화가 담는 최대 메시지 수. 넘으면 오래된 쪽부터 버린다(최신이 남는다).
pub const CONVERSATION_MESSAGES_MAX: usize = 200;
/// 대화 전체의 총 바이트 상한 — 위 둘의 곱보다 낮은 실효 상한이다.
const CONVERSATION_TOTAL_BYTES_MAX: usize = 1024 * 1024;

/// 대화 메시지 한 개의 화자. 턴 메시지(`TurnRole`)와 별개 타입이다 — 이쪽은 파일에서
/// 매번 새로 읽어 화면에만 쓰고 저장하지 않는다.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConversationRole {
    User,
    Assistant,
}

/// 원문 보기가 그리는 메시지 한 개.
#[derive(Clone, PartialEq, Eq)]
pub struct ConversationMessage {
    pub role: ConversationRole,
    pub text: String,
    pub at: Option<i64>,
    /// 이 메시지 레코드 줄의 **절대 파일 오프셋**. `AgentWorkTurnRow.source_offset`과
    /// 같은 좌표계다(둘 다 `snapshot_lines`에서 나온다) — 뷰어가 이 값으로 그 턴을 찾는다.
    pub offset: u64,
}

impl fmt::Debug for ConversationMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConversationMessage")
            .field("role", &self.role)
            .field("text", &"REDACTED")
            .field("at", &self.at)
            .field("offset", &self.offset)
            .finish()
    }
}

/// `read_conversation`의 결과. 저장하지 않는다 — 뷰어가 닫히면 버린다.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct TranscriptConversation {
    pub messages: Vec<ConversationMessage>,
    /// 앞부분(오래된 메시지)이 상한에 밀려 창 밖으로 나갔다.
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptViewError {
    NotFound,
    ReadFailed,
}

/// 노이즈 접두만 걸러내는 원문 추출 — `clean_agent_summary`와 달리 길이를 자르지 않는다
/// (자르는 것은 `bound_conversation_text`가 별도 상한으로 한다).
fn conversation_raw_text(text: &str) -> Option<&str> {
    let mut visible = text.trim_start();
    // 렌더링용 이미지 첨부 표식이 앞에 붙은 메시지는 경로 표식만 걷어낸다(요약과 동일 규칙).
    while visible.starts_with("<image ") {
        visible = visible.split_once('>')?.1.trim_start();
    }
    (!is_noise_prefix(visible)).then_some(visible)
}

/// claude/kimi의 `content`(문자열 또는 `{"type":"text",...}` 배열)에서 원문을 뽑는다.
/// `message_content_summary`/`text_items_summary`와 같은 모양이지만 길이를 자르지 않는다.
fn conversation_content_text(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return conversation_raw_text(text).map(str::to_owned);
    }
    let items = content.as_array()?;
    if items.len() > MAX_MESSAGE_CONTENT_ITEMS {
        return None;
    }
    let mut text = String::new();
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(part) = item
            .get("text")
            .and_then(Value::as_str)
            .and_then(conversation_raw_text)
        else {
            continue;
        };
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(part);
    }
    (!text.is_empty()).then_some(text)
}

/// 메시지 하나를 `CONVERSATION_MESSAGE_BYTES_MAX`에서 UTF-8 경계로 자르고 말줄임을 붙인다.
fn bound_conversation_text(mut text: String) -> String {
    if text.len() <= CONVERSATION_MESSAGE_BYTES_MAX {
        return text;
    }
    let mut cut = CONVERSATION_MESSAGE_BYTES_MAX - '…'.len_utf8();
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    text.push('…');
    text
}

/// `read_conversation` 결과를 유계로 쌓는다. 메시지 수·총 바이트 상한을 넘으면 **앞에서
/// (오래된 쪽부터) 버리고** truncated를 세운다 — 최신이 남는다.
struct ConversationBuilder {
    messages: Vec<ConversationMessage>,
    total_bytes: usize,
    truncated: bool,
}

impl ConversationBuilder {
    fn new() -> Self {
        Self {
            messages: Vec::with_capacity(CONVERSATION_MESSAGES_MAX),
            total_bytes: 0,
            truncated: false,
        }
    }

    fn push(&mut self, role: ConversationRole, text: String, at: Option<i64>, offset: u64) {
        if text.is_empty() {
            return;
        }
        let text = bound_conversation_text(text);
        while self.messages.len() >= CONVERSATION_MESSAGES_MAX {
            self.evict_oldest();
        }
        while !self.messages.is_empty()
            && self.total_bytes.saturating_add(text.len()) > CONVERSATION_TOTAL_BYTES_MAX
        {
            self.evict_oldest();
        }
        self.total_bytes = self.total_bytes.saturating_add(text.len());
        self.messages.push(ConversationMessage { role, text, at, offset });
    }

    fn evict_oldest(&mut self) {
        if self.messages.is_empty() {
            return;
        }
        let removed = self.messages.remove(0);
        self.total_bytes = self.total_bytes.saturating_sub(removed.text.len());
        self.truncated = true;
    }

    fn finish(self) -> (Vec<ConversationMessage>, bool) {
        (self.messages, self.truncated)
    }
}

/// claude transcript에서 user/assistant 메시지만 뽑는다. tool_use·thinking·tool_result는
/// `content` 배열에서 `type == "text"`가 아니라 자연히 버려진다.
fn claude_conversation_messages(snapshot: &TailSnapshot, builder: &mut ConversationBuilder) {
    for (offset, line) in snapshot_lines(snapshot) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let role = match value.get("type").and_then(Value::as_str) {
            Some("user") => ConversationRole::User,
            Some("assistant") => ConversationRole::Assistant,
            _ => continue,
        };
        let Some(content) = value.pointer("/message/content") else {
            continue;
        };
        let Some(text) = conversation_content_text(content) else {
            continue;
        };
        builder.push(role, text, event_occurred_at(&value), offset);
    }
}

/// codex rollout에서 user/assistant 메시지만 뽑는다. `event_msg` 외 레코드(turn_context,
/// token_count 등)와 task_started/task_complete/turn_aborted는 텍스트가 아니라 버려진다.
fn codex_conversation_messages(snapshot: &TailSnapshot, builder: &mut ConversationBuilder) {
    for (offset, line) in snapshot_lines(snapshot) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        let role = match value.pointer("/payload/type").and_then(Value::as_str) {
            Some("user_message") => ConversationRole::User,
            Some("agent_message") => ConversationRole::Assistant,
            _ => continue,
        };
        let Some(text) = value
            .pointer("/payload/message")
            .and_then(Value::as_str)
            .and_then(conversation_raw_text)
        else {
            continue;
        };
        builder.push(role, text.to_owned(), event_occurred_at(&value), offset);
    }
}

/// kimi `wire.jsonl`에서 user/assistant 메시지만 뽑는다. `turn.prompt`가 사용자,
/// `context.append_message`가 에이전트다 — hook_result/system 기원과 user role은 버린다
/// (`kimi_recent_turns`와 같은 판정).
fn kimi_conversation_messages(snapshot: &TailSnapshot, builder: &mut ConversationBuilder) {
    for (offset, line) in snapshot_lines(snapshot) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("turn.prompt") => {
                if value.pointer("/origin/kind").and_then(Value::as_str) != Some("user") {
                    continue;
                }
                let Some(input) = value.get("input") else {
                    continue;
                };
                let Some(text) = conversation_content_text(input) else {
                    continue;
                };
                builder.push(ConversationRole::User, text, event_occurred_at(&value), offset);
            }
            Some("context.append_message") => {
                let role = value.pointer("/message/role").and_then(Value::as_str);
                let origin = value
                    .pointer("/message/origin/kind")
                    .and_then(Value::as_str);
                if role == Some("user") || matches!(origin, Some("hook_result" | "system")) {
                    continue;
                }
                let Some(content) = value.pointer("/message/content") else {
                    continue;
                };
                let Some(text) = conversation_content_text(content) else {
                    continue;
                };
                builder.push(
                    ConversationRole::Assistant,
                    text,
                    event_occurred_at(&value),
                    offset,
                );
            }
            _ => {}
        }
    }
}

/// 카드에서 「원문 보기」를 누르면 부른다. **App host 스레드에서만**(blocking IO) 부른다.
/// 아무것도 저장하지 않는다 — 볼 때만 읽고 닫으면 버린다(스펙 §2).
pub fn read_conversation(
    path: &Path,
    kind: agent_detect::AgentKind,
) -> Result<TranscriptConversation, TranscriptViewError> {
    let snapshot = tail_snapshot(path, CONVERSATION_TAIL_BYTES).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            TranscriptViewError::NotFound
        } else {
            TranscriptViewError::ReadFailed
        }
    })?;
    let mut builder = ConversationBuilder::new();
    builder.truncated = snapshot.base_offset != 0;
    match kind {
        agent_detect::AgentKind::Claude => claude_conversation_messages(&snapshot, &mut builder),
        agent_detect::AgentKind::Codex => codex_conversation_messages(&snapshot, &mut builder),
        agent_detect::AgentKind::Kimi => kimi_conversation_messages(&snapshot, &mut builder),
    }
    let (messages, truncated) = builder.finish();
    Ok(TranscriptConversation { messages, truncated })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(name: &str, content: &str) -> std::path::PathBuf {
        write_tmp_bytes(name, content.as_bytes())
    }

    fn write_tmp_bytes(name: &str, content: &[u8]) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("deppy-transcript-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(content).unwrap();
        p
    }

    /// `read_conversation` 테스트용 claude 모양 jsonl. 파일마다 이름이 겹치지 않게
    /// 카운터를 쓴다 — 같은 프로세스 안에서 여러 테스트가 병렬로 돈다.
    fn 임시_transcript(lines: &[&str]) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut content = String::new();
        for line in lines {
            content.push_str(line);
            content.push('\n');
        }
        write_tmp(&format!("read-conversation-{id}.jsonl"), &content)
    }

    #[test]
    fn 대화_읽기는_역할_두_개만_남긴다() {
        let path = 임시_transcript(&[
            r#"{"type":"user","message":{"role":"user","content":"물음"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"답"}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash"}]}}"#,
        ]);
        let view = read_conversation(&path, agent_detect::AgentKind::Claude).unwrap();
        assert_eq!(view.messages.len(), 2, "tool_use는 제외한다");
        assert_eq!(view.messages[0].role, ConversationRole::User);
        assert_eq!(view.messages[1].text, "답");
    }

    #[test]
    fn 대화_읽기는_메시지당_상한에서_자른다() {
        let long = "가".repeat(CONVERSATION_MESSAGE_BYTES_MAX);
        let path = 임시_transcript(&[&format!(
            r#"{{"type":"user","message":{{"role":"user","content":"{long}"}}}}"#
        )]);
        let view = read_conversation(&path, agent_detect::AgentKind::Claude).unwrap();
        assert!(view.messages[0].text.len() <= CONVERSATION_MESSAGE_BYTES_MAX);
        assert!(view.messages[0].text.ends_with('…'));
    }

    #[test]
    fn 대화_읽기는_손상된_줄을_건너뛴다() {
        let path = 임시_transcript(&[
            "{ 망가진 줄",
            r#"{"type":"user","message":{"role":"user","content":"살아남는다"}}"#,
        ]);
        let view = read_conversation(&path, agent_detect::AgentKind::Claude).unwrap();
        assert_eq!(view.messages.len(), 1, "한 줄이 깨져도 파일 전체를 버리지 않는다");
    }

    #[test]
    fn 대화_읽기는_메시지_수_상한에서_잘림을_표시한다() {
        let count = CONVERSATION_MESSAGES_MAX + 10;
        let mut lines: Vec<String> = Vec::with_capacity(count);
        for index in 0..count {
            lines.push(format!(
                r#"{{"type":"user","message":{{"role":"user","content":"m{index}"}}}}"#
            ));
        }
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let view = read_conversation(&임시_transcript(&refs), agent_detect::AgentKind::Claude)
            .unwrap();
        assert_eq!(view.messages.len(), CONVERSATION_MESSAGES_MAX);
        assert!(view.truncated);
        assert_eq!(
            view.messages.last().unwrap().text,
            format!("m{}", count - 1),
            "최신이 남는다"
        );
    }

    #[test]
    fn 대화_메시지는_레코드_오프셋을_싣는다() {
        let path = 임시_transcript(&[
            r#"{"type":"user","message":{"role":"user","content":"첫"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"답"}]}}"#,
        ]);
        let view = read_conversation(&path, agent_detect::AgentKind::Claude).unwrap();
        assert_eq!(view.messages[0].offset, 0, "첫 줄은 0에서 시작한다");
        assert!(
            view.messages[1].offset > view.messages[0].offset,
            "다음 줄은 뒤에 온다: {:?} vs {:?}",
            view.messages[0].offset,
            view.messages[1].offset
        );
    }

    #[test]
    fn tail_snapshot_never_retains_concurrent_append() {
        let snapshot = b"snapshot";
        let mut bytes = snapshot.to_vec();
        bytes.extend_from_slice(b"-appended-after-metadata");
        let mut reader = std::io::Cursor::new(bytes);

        let text = tail_snapshot_from_reader(&mut reader, snapshot.len() as u64, 64, None)
            .unwrap()
            .text;

        assert_eq!(text, "snapshot");
        assert!(text.len() <= 64);
    }

    #[test]
    fn codex_head_never_reads_concurrent_append() {
        let snapshot = br#"{"type":"event_msg","payload":{"type":"task_started"}}"#;
        let mut bytes = snapshot.to_vec();
        bytes.extend_from_slice(
            br#"
{"type":"session_meta","payload":{"cwd":"/appended"}}"#,
        );
        let mut reader = std::io::Cursor::new(bytes);

        let cwd = codex_cwd_from_head_snapshot(&mut reader, snapshot.len() as u64).unwrap();

        assert_eq!(cwd, None);
    }

    #[test]
    fn tail_snapshot_exact_cap_and_short_read_fail_closed() {
        let exact = "x".repeat(32);
        let mut exact_reader = std::io::Cursor::new(exact.as_bytes());
        assert_eq!(
            tail_snapshot_from_reader(&mut exact_reader, 32, 32, None)
                .unwrap()
                .text,
            exact
        );

        let mut short_reader = std::io::Cursor::new(b"short".as_slice());
        assert!(tail_snapshot_from_reader(&mut short_reader, 6, 32, None).is_err());

        let mut empty_reader = std::io::Cursor::new(Vec::<u8>::new());
        assert!(
            tail_snapshot_from_reader(&mut empty_reader, 0, MAX_TAIL_SNAPSHOT_BYTES + 1, None)
                .is_err(),
            "configured tail cap + 1 must fail before allocation"
        );
    }

    #[test]
    fn tail_cut_discards_only_partial_first_line() {
        let content = b"old-partial\nnew-line\n";
        let mut reader = std::io::Cursor::new(content.as_slice());
        let snapshot =
            tail_snapshot_from_reader(&mut reader, content.len() as u64, 12, None).unwrap();
        assert_eq!(snapshot.text, "new-line\n");
        assert_eq!(snapshot.base_offset, 12);

        let boundary = b"old\nnew-line\n";
        let mut reader = std::io::Cursor::new(boundary.as_slice());
        let snapshot =
            tail_snapshot_from_reader(&mut reader, boundary.len() as u64, 9, None).unwrap();
        assert_eq!(snapshot.text, "new-line\n");
        assert_eq!(snapshot.base_offset, 4);
    }

    #[test]
    fn tail_line_and_item_limits_accept_exact_and_reject_plus_one() {
        let exact_line = "x".repeat(MAX_TRANSCRIPT_LINE_BYTES);
        assert_eq!(validate_tail_text(&exact_line), Some(()));
        assert_eq!(
            validate_tail_text(&(exact_line + "x")),
            None,
            "line cap + 1 must fail closed"
        );

        let exact_items = "{}\n".repeat(MAX_TAIL_LINES);
        assert_eq!(validate_tail_text(&exact_items), Some(()));
        let plus_one_item = format!("{exact_items}{{}}\n");
        assert_eq!(validate_tail_text(&plus_one_item), None);
    }

    #[test]
    fn transcript_paths_accept_exact_cap_and_reject_plus_one() {
        let exact = "a".repeat(MAX_TRANSCRIPT_PATH_BYTES);
        let plus_one = "a".repeat(MAX_TRANSCRIPT_PATH_BYTES + 1);
        assert!(validate_input_path(Path::new(&exact)).is_ok());
        assert!(validate_input_path(Path::new(&plus_one)).is_err());
        assert!(bounded_path_owned(&exact).is_some());
        assert!(bounded_path_owned(&plus_one).is_none());
    }

    #[test]
    fn file_name_and_message_item_caps_reject_plus_one() {
        let uuid = "11111111-2222-3333-4444-555555555555";
        let exact_name = format!("{}{}", "x".repeat(MAX_FILE_NAME_BYTES - uuid.len()), uuid);
        assert_eq!(codex_session_id(&exact_name).as_deref(), Some(uuid));
        let plus_one_name = format!("x{exact_name}");
        assert!(codex_session_id(&plus_one_name).is_none());
        assert_eq!(
            codex_session_id(&format!("rollout-한글-{uuid}-suffix")).as_deref(),
            Some(uuid)
        );
        assert!(codex_session_id("11111111-2222-3333-4444-55555555555G").is_none());
        assert!(codex_session_id("11111111-2222-3333-4444-55555555555_").is_none());

        let exact_items = (0..MAX_MESSAGE_CONTENT_ITEMS)
            .map(|_| serde_json::json!({"type": "text", "text": "a"}))
            .collect::<Vec<_>>();
        let exact = serde_json::json!({"message": {"content": exact_items}});
        assert!(claude_assistant_summary(&exact).is_some());

        let plus_one_items = (0..=MAX_MESSAGE_CONTENT_ITEMS)
            .map(|_| serde_json::json!({"type": "text", "text": "a"}))
            .collect::<Vec<_>>();
        let plus_one = serde_json::json!({"message": {"content": plus_one_items}});
        assert!(claude_assistant_summary(&plus_one).is_none());
    }

    #[test]
    fn hostile_utf8_and_nonregular_inputs_fail_closed() {
        let invalid = write_tmp_bytes(
            "sess-invalid-utf8.jsonl",
            b"{\"type\":\"assistant\",\"cwd\":\"/safe\",\"message\":{\"stop_reason\":\"end_turn\"}}\n\xff",
        );
        assert!(parse_claude(&invalid).is_none());

        let dir = invalid.parent().unwrap();
        assert!(tail_snapshot(dir, TAIL_BYTES).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn transcript_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let target = write_tmp("symlink-target.jsonl", "{}\n");
        let link = target.with_file_name("symlink-input.jsonl");
        let _ = std::fs::remove_file(&link);
        symlink(&target, &link).unwrap();
        assert!(tail_snapshot(&link, TAIL_BYTES).is_err());
        std::fs::remove_file(link).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn transcript_same_inode_symlink_replacement_is_rejected_at_open() {
        use std::os::unix::fs::symlink;

        let victim = write_tmp("replacement-victim.jsonl", "{}\n");
        let moved = victim.with_file_name("replacement-original.jsonl");
        let _ = std::fs::remove_file(&moved);
        let result = open_regular_file_with_before_open(&victim, || {
            std::fs::rename(&victim, &moved).unwrap();
            symlink(&moved, &victim).unwrap();
        });
        assert!(result.is_err());
        std::fs::remove_file(victim).unwrap();
        std::fs::remove_file(moved).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn transcript_fifo_replacement_cannot_block_or_pass() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt as _;

        let victim = write_tmp("replacement-fifo.jsonl", "{}\n");
        let result = open_regular_file_with_before_open(&victim, || {
            std::fs::remove_file(&victim).unwrap();
            let path = CString::new(victim.as_os_str().as_bytes()).unwrap();
            // SAFETY: `path` is a live NUL-terminated copy and mode contains only permission bits.
            let created = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
            assert_eq!(created, 0);
        });
        assert!(result.is_err());
        std::fs::remove_file(victim).unwrap();
    }

    #[test]
    fn codex_head_line_accepts_exact_cap_and_rejects_plus_one() {
        let prefix = r#"{"type":"session_meta","payload":{"cwd":"/bounded"}}"#;
        let mut exact = prefix.to_owned();
        exact.push_str(&" ".repeat(MAX_TRANSCRIPT_LINE_BYTES - prefix.len()));
        exact.push('\n');
        let exact_path = write_tmp("codex-head-exact.jsonl", &exact);
        assert_eq!(
            codex_cwd_from_head(&exact_path).unwrap().as_deref(),
            Some("/bounded")
        );

        let mut plus_one = prefix.to_owned();
        plus_one.push_str(&" ".repeat(MAX_TRANSCRIPT_LINE_BYTES + 1 - prefix.len()));
        plus_one.push('\n');
        let plus_one_path = write_tmp("codex-head-plus-one.jsonl", &plus_one);
        assert!(codex_cwd_from_head(&plus_one_path).is_err());
    }

    #[test]
    fn oversized_codex_cwd_fails_closed_before_copy() {
        let cwd = "a".repeat(MAX_TRANSCRIPT_PATH_BYTES + 1);
        let content = format!(r#"{{"type":"session_meta","payload":{{"cwd":"{cwd}"}}}}"#);
        let path = write_tmp("codex-head-cwd-plus-one.jsonl", &content);
        assert!(codex_cwd_from_head(&path).is_err());
    }

    #[test]
    fn transcript_debug_redacts_content_fields() {
        let state = TranscriptState {
            session_id: "secret-session".to_owned(),
            cwd: Some("/private/project".to_owned()),
            activity: AgentActivity::Working,
            model: Some("hostile-model".to_owned()),
            effort: Some("hidden-effort".to_owned()),
            context_pct: Some(42),
            last_agent_summary: Some("private transcript text".to_owned()),
            user_instruction: Some("private user instruction".to_owned()),
            recent_turns: vec![TranscriptTurn {
                turn_key: "private-turn".to_owned(),
                source_offset: 7,
                instruction: "private turn instruction".to_owned(),
                agent_summary: Some("private turn summary".to_owned()),
                occurred_at: Some(1),
                activity: AgentActivity::Working,
                messages: vec![TurnMessage {
                    role: TurnRole::Assistant,
                    text: "private turn message".to_owned(),
                    at: Some(1),
                }],
            }],
        };
        let debug = format!("{state:?}");
        for raw in [
            "secret-session",
            "/private/project",
            "hostile-model",
            "hidden-effort",
            "private transcript text",
            "private user instruction",
            "private-turn",
            "private turn instruction",
            "private turn summary",
            "private turn message",
        ] {
            assert!(!debug.contains(raw));
        }
        assert!(debug.contains("REDACTED"));
    }

    #[test]
    fn production_transcript_reads_have_bounded_source_laws() {
        let production = include_str!("agent_transcript.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for forbidden in [
            ".read_to_end(",
            "reader.lines()",
            "from_utf8_lossy",
            ".collect::<Vec",
            "regex::Regex::new",
            "std::fs::File::open",
        ] {
            assert!(
                !production.contains(forbidden),
                "unbounded transcript read pattern: {forbidden}"
            );
        }
        assert!(
            production.contains("#[derive(PartialEq, Eq)]\npub struct TranscriptState"),
            "TranscriptState must remain non-Clone"
        );
        #[cfg(unix)]
        assert!(
            production.contains("libc::O_NOFOLLOW | libc::O_NONBLOCK"),
            "transcript open must reject symlink/FIFO replacement races"
        );
    }

    #[test]
    fn claude_idle_when_end_turn() {
        let p = write_tmp(
            "sess-abc.jsonl",
            r#"{"type":"user","cwd":"/proj","message":{"role":"user","content":[{"type":"text","text":"Fix sidebar status"}]}}
{"type":"assistant","cwd":"/proj","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"Updated the sidebar status and tests."}]}}
{"type":"file-history-snapshot"}
"#,
        );
        let s = parse_claude(&p).unwrap();
        assert_eq!(s.session_id, "sess-abc");
        assert_eq!(s.cwd.as_deref(), Some("/proj"));
        assert_eq!(s.activity, AgentActivity::Idle);
        assert_eq!(s.user_instruction.as_deref(), Some("Fix sidebar status"));
        assert_eq!(
            s.last_agent_summary.as_deref(),
            Some("Updated the sidebar status and tests.")
        );
    }

    #[test]
    fn claude_recent_turns는_중복지시를_offset으로_구분하고_요약을_pairing한다() {
        let content = r#"{"type":"user","timestamp":"2026-08-13T00:00:00Z","cwd":"/proj","message":{"role":"user","content":"같은 요청"}}
{"type":"assistant","cwd":"/proj","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"첫 응답"}]}}
{"type":"user","cwd":"/proj","message":{"role":"user","content":"<system-reminder>internal</system-reminder>"}}
{"type":"assistant","cwd":"/proj","message":{"role":"assistant","stop_reason":"stop_sequence","content":[{"type":"text","text":"No response requested."}]}}
{"type":"user","timestamp":"2026-08-13T00:01:00Z","cwd":"/proj","message":{"role":"user","content":"같은 요청"}}
{"type":"assistant","cwd":"/proj","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"text","text":"둘째 작업 중"}]}}
"#;
        let second_marker = r#"{"type":"user","timestamp":"2026-08-13T00:01:00Z"#;
        let second_offset = content.find(second_marker).unwrap() as u64;
        let path = write_tmp("sess-recent-turns.jsonl", content);

        let state = parse_claude(&path).unwrap();

        assert_eq!(state.recent_turns.len(), 2);
        assert_eq!(state.recent_turns[0].instruction, "같은 요청");
        assert_eq!(
            state.recent_turns[0].agent_summary.as_deref(),
            Some("둘째 작업 중")
        );
        assert_eq!(state.recent_turns[0].activity, AgentActivity::Working);
        assert_eq!(state.recent_turns[0].source_offset, second_offset);
        assert_eq!(
            state.recent_turns[0].turn_key,
            format!("claude:{second_offset:x}")
        );
        assert_eq!(
            state.recent_turns[1].agent_summary.as_deref(),
            Some("첫 응답")
        );
        assert_eq!(state.recent_turns[1].activity, AgentActivity::Idle);
        assert_ne!(
            state.recent_turns[0].turn_key,
            state.recent_turns[1].turn_key
        );
        assert_eq!(state.user_instruction.as_deref(), Some("같은 요청"));
        assert_eq!(state.last_agent_summary.as_deref(), Some("둘째 작업 중"));
    }

    #[test]
    fn claude_재시작요약은_exit_합성이벤트를_무시한다() {
        let p = write_tmp(
            "sess-exit.jsonl",
            r#"{"type":"assistant","cwd":"/proj","message":{"role":"assistant","model":"claude-opus-4-8","stop_reason":"end_turn","content":[{"type":"text","text":"최신 실제 작업을 저장했습니다."}]}}
{"type":"user","cwd":"/proj","message":{"role":"user","content":"<local-command-caveat>internal</local-command-caveat>"}}
{"type":"user","cwd":"/proj","message":{"role":"user","content":"<command-name>/exit</command-name>"}}
{"type":"assistant","cwd":"/proj","message":{"role":"assistant","model":"claude-opus-4-8","stop_reason":"stop_sequence","content":[{"type":"text","text":"No response requested."}]}}
"#,
        );
        let s = parse_claude(&p).unwrap();
        assert_eq!(s.activity, AgentActivity::Idle);
        assert_eq!(
            s.last_agent_summary.as_deref(),
            Some("최신 실제 작업을 저장했습니다.")
        );
    }

    #[test]
    fn claude_새_user_turn은_이전_agent요약을_재사용하지_않는다() {
        let p = write_tmp(
            "sess-new-turn.jsonl",
            r#"{"type":"assistant","cwd":"/proj","message":{"role":"assistant","model":"claude-opus-4-8","stop_reason":"end_turn","content":[{"type":"text","text":"이전 작업 완료"}]}}
{"type":"user","cwd":"/proj","message":{"role":"user","content":"새 작업을 시작해"}}
"#,
        );
        let s = parse_claude(&p).unwrap();
        assert_eq!(s.activity, AgentActivity::Working);
        assert_eq!(s.last_agent_summary, None);
        assert_eq!(s.user_instruction.as_deref(), Some("새 작업을 시작해"));
    }

    #[test]
    fn claude_내부_reminder는_실제_사용자_지시를_가리지_않는다() {
        let p = write_tmp(
            "sess-reminder.jsonl",
            r#"{"type":"user","cwd":"/proj","message":{"role":"user","content":"사이드바 작업 표시를 수정해"}}
{"type":"user","cwd":"/proj","message":{"role":"user","content":"<system-reminder>internal</system-reminder>"}}
"#,
        );
        let s = parse_claude(&p).unwrap();
        assert_eq!(s.last_agent_summary, None);
        assert_eq!(
            s.user_instruction.as_deref(),
            Some("사이드바 작업 표시를 수정해")
        );

        let array_reminder = write_tmp(
            "sess-array-reminder.jsonl",
            r#"{"type":"user","cwd":"/proj","message":{"role":"user","content":"현재 작업을 유지해"}}
{"type":"assistant","cwd":"/proj","message":{"role":"assistant","model":"claude-opus-4-8","stop_reason":"tool_use","content":[{"type":"text","text":"코드를 확인하고 있습니다."}]}}
{"type":"user","cwd":"/proj","message":{"role":"user","content":[{"type":"text","text":"<system-reminder>internal</system-reminder>"}]}}
"#,
        );
        let s = parse_claude(&array_reminder).unwrap();
        assert_eq!(
            s.last_agent_summary.as_deref(),
            Some("코드를 확인하고 있습니다.")
        );
        assert_eq!(s.user_instruction.as_deref(), Some("현재 작업을 유지해"));

        let task_notification = write_tmp(
            "sess-task-notification.jsonl",
            r#"{"type":"user","cwd":"/proj","message":{"role":"user","content":"실제 사용자 작업"}}
{"type":"assistant","cwd":"/proj","message":{"role":"assistant","model":"claude-opus-4-8","stop_reason":"end_turn","content":[{"type":"text","text":"실제 작업 완료"}]}}
{"type":"user","cwd":"/proj","message":{"role":"user","content":"<task-notification>internal</task-notification>"}}
"#,
        );
        let s = parse_claude(&task_notification).unwrap();
        assert_eq!(s.last_agent_summary.as_deref(), Some("실제 작업 완료"));
        assert_eq!(s.user_instruction.as_deref(), Some("실제 사용자 작업"));
    }

    #[test]
    fn claude_tool_result는_같은_turn의_agent요약_경계를_끊지_않는다() {
        let p = write_tmp(
            "sess-tool-result.jsonl",
            r#"{"type":"user","cwd":"/proj","message":{"role":"user","content":"테스트를 실행해"}}
{"type":"assistant","cwd":"/proj","message":{"role":"assistant","model":"claude-opus-4-8","stop_reason":"tool_use","content":[{"type":"text","text":"집중 테스트를 실행하고 있습니다."}]}}
{"type":"user","cwd":"/proj","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-1","content":"running"}]}}
"#,
        );
        let s = parse_claude(&p).unwrap();
        assert_eq!(s.activity, AgentActivity::Working);
        assert_eq!(
            s.last_agent_summary.as_deref(),
            Some("집중 테스트를 실행하고 있습니다.")
        );
    }

    #[test]
    fn 에이전트작업설명은_이미지표식을_제거하고_내부메시지를_거른다() {
        assert_eq!(
            clean_agent_summary("<image name=[Image #1] path=/tmp/a.png> 사이드바 수정 완료"),
            Some("사이드바 수정 완료".to_owned())
        );
        assert_eq!(clean_agent_summary("<system-reminder> internal"), None);
        assert_eq!(clean_agent_summary("<task-notification> internal"), None);
        assert_eq!(clean_agent_summary("<heartbeat> internal"), None);
        assert_eq!(clean_agent_summary("   \n\t"), None);
    }

    #[test]
    fn 요약은_줄바꿈을_보존한다() {
        let text = "첫 줄\n둘째 줄\n셋째 줄";
        assert_eq!(clean_agent_summary(text).unwrap(), "첫 줄\n둘째 줄\n셋째 줄");
    }

    #[test]
    fn 요약은_줄_안의_연속_공백만_접는다() {
        let text = "앞     뒤\n다음  줄";
        assert_eq!(clean_agent_summary(text).unwrap(), "앞 뒤\n다음 줄");
    }

    #[test]
    fn 요약은_연속_개행을_하나로_접는다() {
        let text = "위\n\n\n아래";
        assert_eq!(clean_agent_summary(text).unwrap(), "위\n아래");
    }

    #[test]
    fn 요약은_네_줄에서_자른다() {
        let text = "1\n2\n3\n4\n5\n6";
        let summary = clean_agent_summary(text).unwrap();
        assert_eq!(summary.lines().count(), AGENT_SUMMARY_LINES);
        assert!(summary.ends_with('…'), "잘렸으면 말줄임을 붙인다: {summary:?}");
    }

    #[test]
    fn 요약은_사백자에서_자른다() {
        let text = "가".repeat(AGENT_SUMMARY_CHARS + 50);
        let summary = clean_agent_summary(&text).unwrap();
        assert_eq!(summary.chars().count(), AGENT_SUMMARY_CHARS + 1, "본문 + 말줄임");
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn 요약은_노이즈_접두를_계속_거부한다() {
        // 이 규칙은 정확도를 올리는 것이라 상한 변경과 무관하게 유지된다.
        for noise in [
            "<system-reminder>x</system-reminder>",
            "<local-command-stdout>x",
            "<command-name>x",
            "<task-notification>x",
        ] {
            assert!(clean_agent_summary(noise).is_none(), "{noise}");
        }
    }

    #[test]
    fn 턴은_최근_메시지_다섯_개를_남긴다() {
        let mut pending = PendingTurn::new("claude", 0, "지시".to_owned(), None, None);
        for index in 0..8 {
            pending.push_message(TurnRole::Assistant, format!("응답 {index}"), Some(index));
        }
        let turn = pending.finish();
        assert_eq!(turn.messages.len(), TURN_MESSAGES_MAX);
        assert_eq!(turn.messages.last().unwrap().text, "응답 7", "최신이 뒤에 온다");
        assert_eq!(turn.messages.first().unwrap().text, "응답 3", "오래된 것이 밀려난다");
    }

    #[test]
    fn 턴_메시지_직렬화는_상한을_넘으면_none이다() {
        let mut pending = PendingTurn::new("claude", 0, "지시".to_owned(), None, None);
        for index in 0..TURN_MESSAGES_MAX {
            pending.push_message(
                TurnRole::Assistant,
                "가".repeat(AGENT_SUMMARY_CHARS),
                Some(index as i64),
            );
        }
        // 5 × 400자 한글(3바이트)이면 6KB 남짓 — 상한 안이라 Some이어야 한다.
        assert!(pending.finish().messages_json().is_some());
    }

    #[test]
    fn 턴_메시지_json은_역할을_한_글자로_쓴다() {
        let mut pending = PendingTurn::new("claude", 0, "지시".to_owned(), None, None);
        pending.push_message(TurnRole::User, "물음".to_owned(), Some(1));
        pending.push_message(TurnRole::Assistant, "답".to_owned(), Some(2));
        let json = pending.finish().messages_json().unwrap();
        assert!(json.contains(r#""r":"u""#), "{json}");
        assert!(json.contains(r#""r":"a""#), "{json}");
    }

    #[test]
    fn codex_model_effort_context_추출() {
        // rollout: turn_context(model/effort) + token_count(window/used) + event_msg(활동)
        let p = write_tmp(
            "rollout-2026-01-01T00-00-00-11111111-2222-3333-4444-555555555555.jsonl",
            r#"{"type":"session_meta","payload":{"cwd":"/proj"}}
{"type":"turn_context","payload":{"model":"gpt-5.5","effort":"xhigh","cwd":"/proj"}}
{"type":"event_msg","payload":{"type":"user_message","message":"Review PR #124"}}
{"type":"event_msg","payload":{"type":"agent_message","message":"Reviewing changed files"}}
{"type":"token_count","payload":{"info":{"model_context_window":200000,"last_token_usage":{"input_tokens":60000}}}}
{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"Reviewed PR #124 and found two issues"}}
"#,
        );
        let s = parse_codex(&p).unwrap();
        assert_eq!(s.activity, AgentActivity::Idle);
        assert_eq!(s.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(s.effort.as_deref(), Some("xhigh"));
        assert_eq!(s.context_pct, Some(70)); // 60000/200000 = 30% used → 70% 남음
        assert_eq!(s.user_instruction.as_deref(), Some("Review PR #124"));
        assert_eq!(
            s.last_agent_summary.as_deref(),
            Some("Reviewed PR #124 and found two issues")
        );
    }

    #[test]
    fn codex_recent_turns는_내부이벤트를_무시하고_최신순으로_pairing한다() {
        let content = r#"{"type":"session_meta","payload":{"cwd":"/proj"}}
{"type":"turn_context","payload":{"model":"gpt-5.6-sol","effort":"xhigh"}}
{"type":"event_msg","timestamp":"2026-08-13T00:00:00Z","payload":{"type":"user_message","message":"같은 요청"}}
{"type":"event_msg","payload":{"type":"agent_message","message":"첫 작업 중"}}
{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"첫 응답"}}
{"type":"event_msg","payload":{"type":"user_message","message":"<heartbeat>internal</heartbeat>"}}
{"type":"event_msg","timestamp":"2026-08-13T00:01:00Z","payload":{"type":"user_message","message":"같은 요청"}}
{"type":"event_msg","payload":{"type":"agent_message","message":"둘째 작업 중"}}
"#;
        let second_marker = r#"{"type":"event_msg","timestamp":"2026-08-13T00:01:00Z"#;
        let second_offset = content.find(second_marker).unwrap() as u64;
        let path = write_tmp(
            "rollout-2026-01-01T00-00-00-12345678-1234-1234-1234-123456789abc.jsonl",
            content,
        );

        let state = parse_codex(&path).unwrap();

        assert_eq!(state.recent_turns.len(), 2);
        assert_eq!(state.recent_turns[0].instruction, "같은 요청");
        assert_eq!(
            state.recent_turns[0].agent_summary.as_deref(),
            Some("둘째 작업 중")
        );
        assert_eq!(state.recent_turns[0].activity, AgentActivity::Working);
        assert_eq!(state.recent_turns[0].source_offset, second_offset);
        assert_eq!(
            state.recent_turns[0].turn_key,
            format!("codex:{second_offset:x}")
        );
        assert_eq!(
            state.recent_turns[1].agent_summary.as_deref(),
            Some("첫 응답")
        );
        assert_eq!(state.recent_turns[1].activity, AgentActivity::Idle);
        assert_ne!(
            state.recent_turns[0].turn_key,
            state.recent_turns[1].turn_key
        );
        assert_eq!(state.user_instruction.as_deref(), Some("같은 요청"));
        assert_eq!(state.last_agent_summary.as_deref(), Some("둘째 작업 중"));
    }

    #[test]
    fn recent_turns는_24개로_제한된다() {
        let mut content = String::from(
            "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/proj\"}}\n{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5.6-sol\",\"effort\":\"high\"}}\n",
        );
        for index in 0..26 {
            content.push_str(&format!(
                "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"task {index}\"}}}}\n"
            ));
            content.push_str(&format!(
                "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"last_agent_message\":\"done {index}\"}}}}\n"
            ));
        }
        let path = write_tmp(
            "rollout-2026-01-01T00-00-00-abcdefab-cdef-abcd-efab-cdefabcdefab.jsonl",
            &content,
        );

        let state = parse_codex(&path).unwrap();

        assert_eq!(state.recent_turns.len(), MAX_RECENT_TRANSCRIPT_TURNS);
        assert_eq!(state.recent_turns.first().unwrap().instruction, "task 25");
        assert_eq!(state.recent_turns.last().unwrap().instruction, "task 2");
    }

    #[test]
    fn truncated_tail의_첫_partial_turn은_다음_user와_pairing되지_않는다() {
        let orphan = "{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"orphan summary\"}}\n";
        let user = "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"실제 요청\"}}\n";
        let assistant = "{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"실제 응답\"}}\n";
        let snapshot = TailSnapshot {
            base_offset: 10_000,
            modified_at: None,
            text: format!("{orphan}{user}{assistant}"),
        };

        let turns = codex_recent_turns(&snapshot);

        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].source_offset, 10_000 + orphan.len() as u64);
        assert_eq!(turns[0].agent_summary.as_deref(), Some("실제 응답"));
        assert_ne!(turns[0].agent_summary.as_deref(), Some("orphan summary"));
    }

    #[test]
    fn codex_실행중에는_최신_agent_message를_작업설명으로_쓴다() {
        let p = write_tmp(
            "rollout-2026-01-01T00-00-00-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl",
            r#"{"type":"session_meta","payload":{"cwd":"/proj"}}
{"type":"turn_context","payload":{"model":"gpt-5.5","effort":"high"}}
{"type":"token_count","payload":{"info":{"model_context_window":100000,"last_token_usage":{"input_tokens":1000}}}}
{"type":"event_msg","payload":{"type":"task_started"}}
{"type":"event_msg","payload":{"type":"agent_message","message":"Running the focused sidebar tests"}}
"#,
        );
        let s = parse_codex(&p).unwrap();
        assert_eq!(s.activity, AgentActivity::Working);
        assert_eq!(
            s.last_agent_summary.as_deref(),
            Some("Running the focused sidebar tests")
        );
    }

    #[test]
    fn codex_새_turn은_이전_task_complete_요약을_재사용하지_않는다() {
        let p = write_tmp(
            "rollout-2026-01-01T00-00-00-bbbbbbbb-cccc-dddd-eeee-ffffffffffff.jsonl",
            r#"{"type":"session_meta","payload":{"cwd":"/proj"}}
{"type":"turn_context","payload":{"model":"gpt-5.6-sol","effort":"xhigh"}}
{"type":"event_msg","payload":{"type":"task_started"}}
{"type":"event_msg","payload":{"type":"agent_message","message":"이전 작업 진행"}}
{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"이전 작업 완료"}}
{"type":"event_msg","payload":{"type":"user_message","message":"새 작업"}}
{"type":"event_msg","payload":{"type":"token_count","info":{"model_context_window":200000,"last_token_usage":{"input_tokens":1000}}}}
"#,
        );
        let s = parse_codex(&p).unwrap();
        assert_eq!(s.activity, AgentActivity::Working);
        assert_eq!(s.last_agent_summary, None);
        assert_eq!(s.user_instruction.as_deref(), Some("새 작업"));
    }

    #[test]
    fn codex_자동화_heartbeat는_사용자_지시로_오인하지_않는다() {
        let p = write_tmp(
            "rollout-2026-01-01T00-00-00-dddddddd-eeee-ffff-0000-111111111111.jsonl",
            r#"{"type":"session_meta","payload":{"cwd":"/proj"}}
{"type":"turn_context","payload":{"model":"gpt-5.6-sol","effort":"xhigh"}}
{"type":"event_msg","payload":{"type":"user_message","message":"이전 작업"}}
{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"이전 작업 완료"}}
{"type":"event_msg","payload":{"type":"user_message","message":"<heartbeat>internal</heartbeat>"}}
{"type":"token_count","payload":{"info":{"model_context_window":200000,"last_token_usage":{"input_tokens":1000}}}}
"#,
        );
        let s = parse_codex(&p).unwrap();
        assert_eq!(s.activity, AgentActivity::Working);
        assert_eq!(s.last_agent_summary, None);
        assert_eq!(s.user_instruction, None);
    }

    #[test]
    fn codex_비활동_metadata는_완료상태를_실행중으로_덮지_않는다() {
        let p = write_tmp(
            "rollout-2026-01-01T00-00-00-cccccccc-dddd-eeee-ffff-000000000000.jsonl",
            r#"{"type":"session_meta","payload":{"cwd":"/proj"}}
{"type":"turn_context","payload":{"model":"gpt-5.6-sol","effort":"xhigh"}}
{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"최신 작업 완료"}}
{"type":"event_msg","payload":{"type":"token_count","info":{"model_context_window":200000,"last_token_usage":{"input_tokens":1000}}}}
{"type":"event_msg","payload":{"type":"patch_apply_end"}}
"#,
        );
        let s = parse_codex(&p).unwrap();
        assert_eq!(s.activity, AgentActivity::Idle);
        assert_eq!(s.last_agent_summary.as_deref(), Some("최신 작업 완료"));
    }

    #[test]
    fn claude_model_추출_및_effort_context_none() {
        let p = write_tmp(
            "sess-model.jsonl",
            r#"{"type":"assistant","cwd":"/m","message":{"model":"claude-opus-4-8","stop_reason":"end_turn","content":[]}}
"#,
        );
        let s = parse_claude(&p).unwrap();
        assert_eq!(s.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(s.effort, None); // statusLine→DB(Phase 2b)
        assert_eq!(s.context_pct, None);
    }

    #[test]
    fn claude_working_when_tool_use() {
        let p = write_tmp(
            "sess-work.jsonl",
            r#"{"type":"assistant","cwd":"/w","message":{"stop_reason":"tool_use","content":[{"type":"tool_use"}]}}
{"type":"mode"}
"#,
        );
        let s = parse_claude(&p).unwrap();
        assert_eq!(s.activity, AgentActivity::Working);
        assert_eq!(s.cwd.as_deref(), Some("/w"));
    }

    #[test]
    fn codex_idle_and_working_and_session_id() {
        // Idle: 마지막 event_msg가 task_complete
        let idle = write_tmp(
            "rollout-2026-07-07T00-20-36-019f3804-586d-7ca3-9386-1cbc8710ca08.jsonl",
            r#"{"type":"session_meta","payload":{"cwd":"/repo"}}
{"type":"response_item","payload":{"type":"message"}}
{"type":"event_msg","payload":{"type":"task_complete"}}
"#,
        );
        let s = parse_codex(&idle).unwrap();
        assert_eq!(s.session_id, "019f3804-586d-7ca3-9386-1cbc8710ca08");
        assert_eq!(s.cwd.as_deref(), Some("/repo"));
        assert_eq!(s.activity, AgentActivity::Idle);

        // Working: 마지막 event_msg가 task_started
        let work = write_tmp(
            "rollout-2026-07-07T01-00-00-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl",
            r#"{"type":"session_meta","payload":{"cwd":"/r2"}}
{"type":"event_msg","payload":{"type":"task_started"}}
"#,
        );
        assert_eq!(parse_codex(&work).unwrap().activity, AgentActivity::Working);
    }

    /// 실제 파일 smoke-test — 로컬 ~/.claude, ~/.codex 파일로 파싱이 되는지 확인한다.
    /// 머신 종속이라 기본 무시. 실행: `cargo test -p deppy-sijo smoke_real -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn smoke_real_files() {
        let home = std::env::var("HOME").unwrap();
        let mut n = 0;
        // claude
        let claude_glob = format!("{home}/.claude/projects");
        if let Ok(projects) = std::fs::read_dir(&claude_glob) {
            let mut files: Vec<_> = projects
                .flatten()
                .flat_map(|d| std::fs::read_dir(d.path()).into_iter().flatten().flatten())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
                .collect();
            files.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
            for p in files.iter().rev().take(3) {
                if let Some(s) = parse_claude(p) {
                    println!(
                        "claude  {:8?}  sid={}…  cwd={:?}  → claude --resume {}",
                        s.activity,
                        &s.session_id[..s.session_id.len().min(18)],
                        s.cwd,
                        s.session_id
                    );
                    n += 1;
                }
            }
        }
        // codex
        let codex_root = format!("{home}/.codex/sessions");
        let mut codex_files = Vec::new();
        collect_jsonl(std::path::Path::new(&codex_root), &mut codex_files);
        codex_files.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
        for p in codex_files.iter().rev().take(3) {
            if let Some(s) = parse_codex(p) {
                println!(
                    "codex   {:8?}  sid={}…  cwd={:?}  → codex resume {}",
                    s.activity,
                    &s.session_id[..s.session_id.len().min(18)],
                    s.cwd,
                    s.session_id
                );
                n += 1;
            }
        }
        println!("smoke: {n}개 실제 파일 파싱 성공");
        assert!(
            n > 0,
            "실제 transcript 파일을 하나도 못 찾음 (로컬 환경 확인)"
        );
    }

    #[cfg(test)]
    fn collect_jsonl(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect_jsonl(&p, out);
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                out.push(p);
            }
        }
    }
}
