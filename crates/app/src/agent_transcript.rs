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

/// transcript에서 파생한 에이전트 활동. needsInput은 여기 없다(regex fallback 담당).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentActivity {
    Working,
    Idle,
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
            .finish()
    }
}

const TAIL_BYTES: u64 = 256 * 1024;
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
const AGENT_SUMMARY_CHARS: usize = 120;
const AGENT_SUMMARY_BYTES: usize = AGENT_SUMMARY_CHARS * 4 + '…'.len_utf8();

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
fn tail_text_from_snapshot<R: Read + Seek>(
    reader: &mut R,
    snapshot_len: u64,
    max_bytes: u64,
) -> std::io::Result<String> {
    if max_bytes > TAIL_BYTES {
        return Err(invalid_input("tail_limit_invalid"));
    }
    let retained = snapshot_len.min(max_bytes);
    let retained = usize::try_from(retained).map_err(|_| invalid_input("tail_limit_invalid"))?;
    let start = snapshot_len.saturating_sub(max_bytes);
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
            return Ok(String::new());
        };
        bytes.drain(..=first_newline);
    }
    String::from_utf8(bytes).map_err(|_| invalid_input("transcript_utf8_invalid"))
}

/// 파일 끝 `max_bytes`만 읽는다. metadata snapshot 이후 append는 다음 poll에서 보고,
/// 현재 poll에서는 버퍼가 상한을 넘지 않도록 정확한 snapshot 바이트만 읽는다.
fn tail_text(path: &Path, max_bytes: u64) -> std::io::Result<String> {
    let (mut file, snapshot_len) = open_regular_file(path)?;
    let text = tail_text_from_snapshot(&mut file, snapshot_len, max_bytes)?;
    if file.metadata()?.len() < snapshot_len {
        return Err(invalid_input("transcript_shrank_during_read"));
    }
    Ok(text)
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

fn bounded_owned(value: &str, max_bytes: usize) -> Option<String> {
    (value.len() <= max_bytes).then(|| value.to_owned())
}

fn bounded_path_owned(value: &str) -> Option<String> {
    (!value.is_empty() && value.len() <= MAX_TRANSCRIPT_PATH_BYTES && !value.contains('\0'))
        .then(|| value.to_owned())
}

fn clean_agent_summary(text: &str) -> Option<String> {
    let mut visible = text.trim_start();
    // 렌더링용 이미지 첨부 표식이 앞에 붙은 메시지는 경로 표식만 걷어낸다.
    while visible.starts_with("<image ") {
        visible = visible.split_once('>')?.1.trim_start();
    }
    if visible.is_empty()
        || visible.starts_with("<system-reminder")
        || visible.starts_with("<local-command")
        || visible.starts_with("<command-name")
        || visible.starts_with("<environment_context")
        || visible.starts_with("<permissions")
        || visible.starts_with("<INSTRUCTIONS")
    {
        return None;
    }

    let mut summary = String::with_capacity(text.len().min(AGENT_SUMMARY_BYTES));
    let mut summary_chars = 0_usize;
    let mut pending_space = false;
    let mut truncated = false;
    for ch in visible.chars() {
        if ch.is_whitespace() || ch.is_control() {
            pending_space |= !summary.is_empty();
            continue;
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

fn claude_assistant_summary(value: &Value) -> Option<String> {
    let content = value.pointer("/message/content")?;
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

fn claude_internal_user_event(value: &Value) -> bool {
    value
        .pointer("/message/content")
        .and_then(Value::as_str)
        .is_some_and(|content| {
            let content = content.trim_start();
            content.starts_with("<local-command") || content.starts_with("<command-name")
        })
}

fn claude_user_starts_new_turn(value: &Value) -> bool {
    let Some(content) = value.pointer("/message/content") else {
        return false;
    };
    if content.is_string() {
        return !claude_internal_user_event(value);
    }
    content.as_array().is_some_and(|items| {
        items.iter().any(|item| {
            !matches!(
                item.get("type").and_then(Value::as_str),
                Some("tool_result")
            )
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

/// claude transcript(`~/.claude/projects/<cwd>/<session-id>.jsonl`) 파싱.
/// 파일명이 곧 세션 ID. 마지막 assistant/user 이벤트로 상태를 파생한다:
/// assistant `stop_reason=end_turn` → Idle(유저 차례), 그 외(tool_use) → Working.
pub fn parse_claude(path: &Path) -> Option<TranscriptState> {
    let raw_session_id = path.file_stem()?.to_str()?;
    if raw_session_id.is_empty() || raw_session_id.len() > MAX_SESSION_ID_BYTES {
        return None;
    }
    let session_id = raw_session_id.to_owned();
    let text = tail_text(path, TAIL_BYTES).ok()?;
    validate_tail_text(&text)?;
    let mut cwd = None;
    let mut activity: Option<AgentActivity> = None;
    // 최신 assistant message.model = 현재 모델(effort/context는 statusLine→DB, Phase 2b).
    let mut model: Option<String> = None;
    let mut last_agent_summary: Option<String> = None;
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
                if last_agent_summary.is_none() && claude_user_starts_new_turn(&v) {
                    summary_boundary_reached = true;
                }
            }
            _ => {}
        }
        if activity.is_some()
            && model.is_some()
            && cwd.is_some()
            && (last_agent_summary.is_some() || summary_boundary_reached)
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
pub fn parse_kimi(path: &Path) -> Option<TranscriptState> {
    // 경로는 `<sessionDir>/agents/main/wire.jsonl`이고 sessionDir 이름이 세션 id다.
    let session_id = kimi_session_id(path)?;
    let text = tail_text(path, TAIL_BYTES).ok()?;
    validate_tail_text(&text)?;

    let mut activity: Option<AgentActivity> = None;
    let mut model: Option<String> = None;
    let mut effort: Option<String> = None;
    let mut max_tokens: Option<u64> = None;
    let mut used_tokens: Option<u64> = None;
    let mut last_agent_summary: Option<String> = None;
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
                summary_boundary_reached = false;
            }
            Some("turn.prompt") => {
                activity.get_or_insert(AgentActivity::Working);
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
    })
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
                r#"{"type":"turn.prompt","origin":{"kind":"user"},"time":1}"#,
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

pub fn parse_codex(path: &Path) -> Option<TranscriptState> {
    let session_id = codex_session_id(path.file_name()?.to_str()?)?;
    let (cwd, text) = codex_snapshot(path).ok()?;
    validate_tail_text(&text)?;
    // 역순 1-pass로 activity(첫 event_msg) + model/effort(첫 turn_context) +
    // context%(첫 token_count)를 모은다. 셋 다 채워지면 조기 종료.
    let mut activity: Option<AgentActivity> = None;
    let mut model: Option<String> = None;
    let mut effort: Option<String> = None;
    let mut context_pct: Option<u8> = None;
    let mut last_agent_summary: Option<String> = None;
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

fn codex_snapshot(path: &Path) -> std::io::Result<(Option<String>, String)> {
    let (mut file, snapshot_len) = open_regular_file(path)?;
    let cwd = codex_cwd_from_head_snapshot(&mut file, snapshot_len)?;
    let tail = tail_text_from_snapshot(&mut file, snapshot_len, TAIL_BYTES)?;
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

    #[test]
    fn tail_snapshot_never_retains_concurrent_append() {
        let snapshot = b"snapshot";
        let mut bytes = snapshot.to_vec();
        bytes.extend_from_slice(b"-appended-after-metadata");
        let mut reader = std::io::Cursor::new(bytes);

        let text = tail_text_from_snapshot(&mut reader, snapshot.len() as u64, 64).unwrap();

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
            tail_text_from_snapshot(&mut exact_reader, 32, 32).unwrap(),
            exact
        );

        let mut short_reader = std::io::Cursor::new(b"short".as_slice());
        assert!(tail_text_from_snapshot(&mut short_reader, 6, 32).is_err());

        let mut empty_reader = std::io::Cursor::new(Vec::<u8>::new());
        assert!(
            tail_text_from_snapshot(&mut empty_reader, 0, TAIL_BYTES + 1).is_err(),
            "configured tail cap + 1 must fail before allocation"
        );
    }

    #[test]
    fn tail_cut_discards_only_partial_first_line() {
        let content = b"old-partial\nnew-line\n";
        let mut reader = std::io::Cursor::new(content.as_slice());
        let text = tail_text_from_snapshot(&mut reader, content.len() as u64, 12).unwrap();
        assert_eq!(text, "new-line\n");

        let boundary = b"old\nnew-line\n";
        let mut reader = std::io::Cursor::new(boundary.as_slice());
        let text = tail_text_from_snapshot(&mut reader, boundary.len() as u64, 9).unwrap();
        assert_eq!(text, "new-line\n");
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
        assert!(tail_text(dir, TAIL_BYTES).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn transcript_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let target = write_tmp("symlink-target.jsonl", "{}\n");
        let link = target.with_file_name("symlink-input.jsonl");
        let _ = std::fs::remove_file(&link);
        symlink(&target, &link).unwrap();
        assert!(tail_text(&link, TAIL_BYTES).is_err());
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
        };
        let debug = format!("{state:?}");
        for raw in [
            "secret-session",
            "/private/project",
            "hostile-model",
            "hidden-effort",
            "private transcript text",
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
        assert_eq!(
            s.last_agent_summary.as_deref(),
            Some("Updated the sidebar status and tests.")
        );
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
        assert_eq!(clean_agent_summary("   \n\t"), None);
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
        assert_eq!(
            s.last_agent_summary.as_deref(),
            Some("Reviewed PR #124 and found two issues")
        );
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
