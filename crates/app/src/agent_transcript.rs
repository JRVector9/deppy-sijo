//! 에이전트(claude/codex) transcript(JSONL) 파서 — 세션 ID + 활동 상태(working/idle)를
//! 구조화 로그에서 파생한다(옵션2, cmux 참고). 화면 스크래핑(regex)은 TUI 문구/레이아웃에
//! 의존해 불안정했다(#92/#93) — transcript는 구조화 로그라 정확하다.
//!
//! - 세션 ID: claude는 파일명, codex는 파일명 내 UUID. → 복원 시 native resume에 그대로 씀.
//! - cwd: claude는 매 이벤트, codex는 session_meta(첫 줄)에 기록 → pane 바인딩 앵커.
//! - 활동: 마지막 의미있는 이벤트로 working/idle 판정.
//! - 승인(needsInput)은 transcript에 없다 → regex fallback(status detector)이 담당.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

/// transcript에서 파생한 에이전트 활동. needsInput은 여기 없다(regex fallback 담당).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentActivity {
    Working,
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
}

/// 파일 끝 `max_bytes`만 읽는다 — transcript는 수십 MB가 될 수 있어 tail만 본다.
/// seek로 잘린 첫 줄은 역순 파싱에서 JSON 파싱 실패로 자연히 건너뛴다.
fn tail_text(path: &Path, max_bytes: u64) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(max_bytes)))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

const TAIL_BYTES: u64 = 256 * 1024;

/// claude transcript(`~/.claude/projects/<cwd>/<session-id>.jsonl`) 파싱.
/// 파일명이 곧 세션 ID. 마지막 assistant/user 이벤트로 상태를 파생한다:
/// assistant `stop_reason=end_turn` → Idle(유저 차례), 그 외(tool_use) → Working.
pub fn parse_claude(path: &Path) -> Option<TranscriptState> {
    let session_id = path.file_stem()?.to_str()?.to_owned();
    let text = tail_text(path, TAIL_BYTES).ok()?;
    let mut cwd = None;
    let mut activity: Option<AgentActivity> = None;
    // 최신 assistant message.model = 현재 모델(effort/context는 statusLine→DB, Phase 2b).
    let mut model: Option<String> = None;
    for line in text.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if cwd.is_none() {
            cwd = v.get("cwd").and_then(Value::as_str).map(str::to_owned);
        }
        match v.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                if model.is_none() {
                    model = v
                        .pointer("/message/model")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
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
            Some("user") if activity.is_none() => {
                activity = Some(AgentActivity::Working);
            }
            _ => {}
        }
        if activity.is_some() && model.is_some() && cwd.is_some() {
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
    })
}

/// codex rollout(`~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`) 파싱.
/// 세션 ID는 파일명 내 UUID, cwd는 session_meta(첫 줄). 상태는 마지막 event_msg로:
/// `task_complete`/`turn_aborted` → Idle, 그 외(task_started/agent_message 등) → Working.
pub fn parse_codex(path: &Path) -> Option<TranscriptState> {
    let session_id = codex_session_id(path.file_name()?.to_str()?)?;
    let cwd = codex_cwd_from_head(path);
    let text = tail_text(path, TAIL_BYTES).ok()?;
    // 역순 1-pass로 activity(첫 event_msg) + model/effort(첫 turn_context) +
    // context%(첫 token_count)를 모은다. 셋 다 채워지면 조기 종료.
    let mut activity: Option<AgentActivity> = None;
    let mut model: Option<String> = None;
    let mut effort: Option<String> = None;
    let mut context_pct: Option<u8> = None;
    for line in text.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("event_msg") if activity.is_none() => {
                activity = match v.pointer("/payload/type").and_then(Value::as_str) {
                    Some("task_complete") | Some("turn_aborted") => Some(AgentActivity::Idle),
                    Some(_) => Some(AgentActivity::Working),
                    None => None,
                };
            }
            Some("turn_context") if model.is_none() => {
                model = v
                    .pointer("/payload/model")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                effort = v
                    .pointer("/payload/effort")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
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
        if activity.is_some() && model.is_some() && context_pct.is_some() {
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
    })
}

/// 파일명에서 UUID(마지막 5개 하이픈 그룹)를 뽑는다 — 타임스탬프에도 하이픈이 있어 정규식으로.
pub(crate) fn codex_session_id(file_name: &str) -> Option<String> {
    let re =
        regex::Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").ok()?;
    re.find(file_name).map(|m| m.as_str().to_owned())
}

/// codex의 cwd는 첫 줄 session_meta의 `payload.cwd`에 있다. 그 줄엔 base_instructions
/// (전체 시스템 프롬프트, 수십 KB)가 cwd보다 앞서므로 고정 바이트 헤드로는 잘린다 —
/// 앞쪽 몇 줄을 통째로(BufReader) 읽어 session_meta를 파싱한다.
fn codex_cwd_from_head(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let f = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(f);
    for line in reader.lines().take(3).map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) == Some("session_meta") {
            return v
                .pointer("/payload/cwd")
                .or_else(|| v.get("cwd"))
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(name: &str, content: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("deppy-transcript-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        p
    }

    #[test]
    fn claude_idle_when_end_turn() {
        let p = write_tmp(
            "sess-abc.jsonl",
            r#"{"type":"user","cwd":"/proj","message":{"role":"user","content":[{"type":"text"}]}}
{"type":"assistant","cwd":"/proj","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text"}]}}
{"type":"file-history-snapshot"}
"#,
        );
        let s = parse_claude(&p).unwrap();
        assert_eq!(s.session_id, "sess-abc");
        assert_eq!(s.cwd.as_deref(), Some("/proj"));
        assert_eq!(s.activity, AgentActivity::Idle);
    }

    #[test]
    fn codex_model_effort_context_추출() {
        // rollout: turn_context(model/effort) + token_count(window/used) + event_msg(활동)
        let p = write_tmp(
            "rollout-2026-01-01T00-00-00-11111111-2222-3333-4444-555555555555.jsonl",
            r#"{"type":"session_meta","payload":{"cwd":"/proj"}}
{"type":"turn_context","payload":{"model":"gpt-5.5","effort":"xhigh","cwd":"/proj"}}
{"type":"token_count","payload":{"info":{"model_context_window":200000,"last_token_usage":{"input_tokens":60000}}}}
{"type":"event_msg","payload":{"type":"task_complete"}}
"#,
        );
        let s = parse_codex(&p).unwrap();
        assert_eq!(s.activity, AgentActivity::Idle);
        assert_eq!(s.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(s.effort.as_deref(), Some("xhigh"));
        assert_eq!(s.context_pct, Some(70)); // 60000/200000 = 30% used → 70% 남음
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
