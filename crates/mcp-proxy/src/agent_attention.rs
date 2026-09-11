//! 세 CLI 훅을 관찰 전용 상태 이벤트로 바꾼다. 도구 인자와 답변 본문은 보관하지 않는다.
use serde_json::Value;
use storage::{AgentAttentionEvent, AttentionEventKind as K};

use std::hash::{Hash, Hasher};

fn tool_fingerprint(v: &Value, tool: &str) -> String {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    tool.hash(&mut hash);
    tool_input_hash(v).hash(&mut hash);
    format!("tool:{:016x}", hash.finish())
}

fn tool_input_hash(v: &Value) -> Option<String> {
    if v.get("tool_input_hash").is_some() {
        field(v, &["tool_input_hash"]).map(str::to_owned)
    } else {
        v.get("tool_input")
            .or_else(|| v.get("toolInput"))
            .and_then(crate::hook_payload::input_hash)
    }
}

fn field<'a>(v: &'a Value, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| v.get(name).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
}

pub fn normalize(v: &Value, at_micros: i64) -> Option<AgentAttentionEvent> {
    if field(v, &["subagentType"]).is_some() {
        return None;
    }
    let native = field(v, &["session_id", "sessionId"])?;
    let name = field(v, &["hook_event_name", "hookEventName"])?
        .replace('_', "")
        .to_ascii_lowercase();
    let tool = field(v, &["tool_name", "toolName"]).unwrap_or("");
    let short = tool.rsplit('.').next().unwrap_or(tool);
    let question = matches!(
        short,
        "AskUserQuestion"
            | "ExitPlanMode"
            | "request_user_input"
            | "ask_user_question"
            | "exit_plan_mode"
    );
    let mut request = field(
        v,
        &[
            "tool_use_id",
            "toolUseId",
            "call_id",
            "tool_call_id",
            "elicitation_id",
        ],
    )
    .map(str::to_owned)
    .unwrap_or_else(|| tool_fingerprint(v, tool));
    if matches!(name.as_str(), "elicitation" | "elicitationresult")
        && v.get("elicitation_id").is_none()
    {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        field(v, &["mcp_server_name", "mcpServerName"])?.hash(&mut hash);
        field(v, &["mode"]).unwrap_or("form").hash(&mut hash);
        request = format!("elicitation:{:016x}", hash.finish());
    }
    let kind = match name.as_str() {
        "sessionstart" => K::SessionStart,
        "userpromptsubmit" => K::TurnStart,
        "pretooluse" => {
            if question {
                K::ResponseRequired
            } else {
                K::Working
            }
        }
        "posttooluse" | "posttoolusefailure" | "permissiondenied" => K::Resolved,
        "permissionrequest" => K::ApprovalRequired,
        "elicitation" => K::ResponseRequired,
        "elicitationresult" => K::Resolved,
        "notification" => match field(v, &["notification_type", "notificationType"])? {
            "idle_prompt" => K::IdleObserved,
            "task_complete" => K::Completed,
            "permission_prompt" => {
                request = "notification:permission".into();
                K::ApprovalRequired
            }
            "elicitation_dialog" => {
                request = "notification:elicitation".into();
                K::ResponseRequired
            }
            "elicitation_complete" | "elicitation_response" => {
                request = "notification:elicitation".into();
                K::Resolved
            }
            _ => return None,
        },
        "stop" => K::Completed,
        "stopfailure" | "stopcancelled" | "interrupt" => K::Cancelled,
        "sessionend" => K::SessionEnd,
        _ => return None,
    };
    let kind = if field(v, &["agent_id"]).is_some() {
        match kind {
            K::ResponseRequired | K::ApprovalRequired | K::Resolved | K::Working => kind,
            K::Cancelled | K::SessionEnd => K::Cancelled,
            _ => return None,
        }
    } else {
        kind
    };
    let request = scoped_request_id(v, &request);
    let turn_id = field(v, &["turn_id", "turnId", "promptId", "prompt_id"]).map(str::to_owned);
    let tool_group = (!tool.is_empty()).then(|| {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        tool.hash(&mut hash);
        scoped_request_id(v, &format!("group:{:016x}", hash.finish()))
    });
    Some(AgentAttentionEvent {
        native_session_id: native.into(),
        turn_id,
        request_id: request,
        kind,
        at_micros,
        tool_group,
        tool_input_hash: tool_input_hash(v),
        child_id: field(v, &["agent_id"]).map(str::to_owned),
    })
}

fn scoped_request_id(v: &Value, id: &str) -> String {
    if let Some(agent) = field(v, &["agent_id"]) {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        agent.hash(&mut hash);
        format!("child:{:016x}:{id}", hash.finish())
    } else {
        id.to_owned()
    }
}

/// 실행 전 거절·실행 중 취소는 결과 훅 대신 tool_result에 남는다.
fn claude_finished_requests(text: &str, pending: &[String], context: &Value) -> Vec<String> {
    let rows: Vec<Value> = text
        .lines()
        .rev()
        .take(512)
        .filter_map(crate::hook_payload::transcript_row)
        .collect();
    let mut result_ids = Vec::new();
    for row in &rows {
        if let Some(content) = row.pointer("/message/content").and_then(Value::as_array) {
            for item in content {
                if item.get("type").and_then(Value::as_str) == Some("tool_result")
                    && let Some(id) = field(item, &["tool_use_id"])
                {
                    result_ids.push(id);
                }
            }
        }
    }
    let mut done = Vec::new();
    let mut add = |id: String| {
        if pending.contains(&id) && !done.contains(&id) {
            done.push(id);
        }
    };
    for id in &result_ids {
        add(scoped_request_id(context, id));
    }
    done
}

pub fn reconcile_claude_results(db: &storage::Db, key: &str, v: &Value, at: i64) {
    let Some(native) = field(v, &["session_id"]) else {
        return;
    };
    if v.get("hookEventName").is_some() {
        return;
    }
    let Ok(pending) = db.agent_result_candidate_ids(key, native) else {
        return;
    };
    if pending.is_empty() {
        return;
    }
    let mut done = Vec::new();
    // 배치 결과를 먼저 확인하여 transcript flush 지연에도 정확한 ID를 해제한다.
    if let Some(calls) = v.get("tool_calls").and_then(Value::as_array) {
        for call in calls.iter().take(64) {
            if let Some(id) = field(call, &["tool_use_id"]) {
                let id = scoped_request_id(v, id);
                if pending.contains(&id) && !done.contains(&id) {
                    done.push(id);
                }
            }
        }
    }
    let path = field(v, &["agent_transcript_path", "transcript_path"]).map(std::path::Path::new);
    if let Some(path) = path {
        let expected =
            field(v, &["agent_id"]).map_or_else(|| native.to_owned(), |id| format!("agent-{id}"));
        if path.file_stem().and_then(|s| s.to_str()) == Some(expected.as_str())
            && path.extension().and_then(|s| s.to_str()) == Some("jsonl")
            && let Some(text) = bounded_tail(path)
        {
            for id in claude_finished_requests(&text, &pending, v) {
                if !done.contains(&id) {
                    done.push(id);
                }
            }
        }
    }
    if field(v, &["agent_id"]).is_none()
        && let Some(path) = path
        && path.file_stem().and_then(|s| s.to_str()) == Some(native)
        && path.extension().and_then(|s| s.to_str()) == Some("jsonl")
        && let Ok(children) = db.pending_agent_children(key, native)
    {
        for child in children {
            if child.is_empty()
                || child.len() > 128
                || !child
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                continue;
            }
            let child_path = path
                .with_extension("")
                .join("subagents")
                .join(format!("agent-{child}.jsonl"));
            if let Some(text) = bounded_tail(&child_path) {
                let context = serde_json::json!({"agent_id":child});
                for id in claude_finished_requests(&text, &pending, &context) {
                    if !done.contains(&id) {
                        done.push(id);
                    }
                }
            }
        }
    }
    for id in done {
        let _ = db.record_agent_attention(
            key,
            &AgentAttentionEvent {
                native_session_id: native.into(),
                turn_id: field(v, &["turn_id"]).map(str::to_owned),
                request_id: id,
                kind: K::Resolved,
                at_micros: at,
                tool_group: None,
                tool_input_hash: None,
                child_id: None,
            },
        );
    }
}

/// 훅이 없는 실패 결과도 정확한 call_id로 확인한다. 답변 대기 중인 호출은 남긴다.
fn codex_finished_requests(text: &str, pending: &[String]) -> Vec<String> {
    let mut done = Vec::new();
    for line in text.lines().rev().take(512) {
        if line.len() > 64 * 1024 {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("response_item")
            || v.pointer("/payload/type").and_then(Value::as_str) != Some("function_call_output")
        {
            continue;
        }
        if let Some(id) = v.pointer("/payload/call_id").and_then(Value::as_str)
            && pending.iter().any(|p| p == id)
            && !done.iter().any(|p| p == id)
        {
            done.push(id.to_owned());
        }
    }
    done
}

fn bounded_tail(path: &std::path::Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let offset = meta.len().saturating_sub(512 * 1024);
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = Vec::new();
    file.take(512 * 1024).read_to_end(&mut bytes).ok()?;
    let bytes = if offset > 0 {
        &bytes[bytes.iter().position(|b| *b == b'\n')? + 1..]
    } else {
        &bytes
    };
    Some(String::from_utf8_lossy(bytes).into_owned())
}

pub fn reconcile_codex_results(db: &storage::Db, key: &str, v: &Value, at: i64) {
    let Some(native) = field(v, &["session_id"]) else {
        return;
    };
    let Some(path) = field(v, &["transcript_path"]) else {
        return;
    };
    let path = std::path::Path::new(path);
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return;
    };
    if !name.starts_with("rollout-") || !name.contains(native) || !name.ends_with(".jsonl") {
        return;
    }
    let Ok(pending) = db.pending_agent_request_ids(key, native) else {
        return;
    };
    if pending.is_empty() {
        return;
    }
    let Some(text) = bounded_tail(path) else {
        return;
    };
    for id in codex_finished_requests(&text, &pending) {
        let _ = db.record_agent_attention(
            key,
            &AgentAttentionEvent {
                native_session_id: native.into(),
                turn_id: field(v, &["turn_id"]).map(str::to_owned),
                request_id: id,
                kind: K::Resolved,
                at_micros: at,
                tool_group: None,
                tool_input_hash: None,
                child_id: None,
            },
        );
    }
}

/// Grok 유휴는 성공을 뜻하지 않는다. 최신 실제 턴 결과가 completed일 때만 완료로 승격한다.
fn grok_completed(text: &str) -> bool {
    for line in text.lines().rev().take(512) {
        if line.len() > 64 * 1024 {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("turn_started" | "interjected") => return false,
            Some("turn_ended") => {
                return v.get("outcome").and_then(Value::as_str) == Some("completed");
            }
            _ => {}
        }
    }
    false
}

pub fn confirm_grok_completion(v: &Value, event: &mut AgentAttentionEvent) {
    if v.get("hookEventName").is_none() || !matches!(event.kind, K::IdleObserved | K::Completed) {
        return;
    }
    // 일반 알림과 이전 턴 보고를 성공으로 단정하지 않는다.
    event.kind = K::IdleObserved;
    let Some(cwd) = field(v, &["cwd"]) else {
        return;
    };
    if !std::path::Path::new(cwd).is_absolute()
        || cwd.len() > 4096
        || event.native_session_id.is_empty()
        || event.native_session_id.len() > 256
        || !event
            .native_session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return;
    }
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let encoded = cwd
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect::<String>();
    let path = std::path::PathBuf::from(home)
        .join(".grok/sessions")
        .join(encoded)
        .join(&event.native_session_id)
        .join("events.jsonl");
    if bounded_tail(&path).is_some_and(|text| grok_completed(&text)) {
        event.kind = K::Completed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use storage::AttentionEventKind as K;

    const KEY: &str = "00000000-0000-0000-0000-000000000001:1";
    fn db() -> storage::Db {
        let dir = std::env::temp_dir().join(format!("deppy-review-db-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        storage::Db::open(&dir.join("metadata.sqlite3")).unwrap()
    }
    fn hook(db: &storage::Db, v: Value, at: i64) {
        let v = crate::hook_payload::read(v.to_string().as_bytes()).unwrap();
        if let Some(e) = super::normalize(&v, at) {
            db.record_agent_attention(KEY, &e).unwrap();
        }
        super::reconcile_claude_results(db, KEY, &v, at);
    }

    #[test]
    fn fix5_응답한_승인의_알림도_다른_자동_호출과_분리한다() {
        let d = db();
        for (v, t) in [
            (
                json!({"session_id":"native","hook_event_name":"PreToolUse","tool_use_id":"slow","tool_name":"Bash","tool_input":{"command":"sleep 100"}}),
                1,
            ),
            (
                json!({"session_id":"native","hook_event_name":"PreToolUse","tool_use_id":"q","tool_name":"Bash","tool_input":{"command":"rm x"}}),
                2,
            ),
            (
                json!({"session_id":"native","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"rm x"}}),
                3,
            ),
            (
                json!({"session_id":"native","hook_event_name":"Notification","notification_type":"permission_prompt"}),
                4,
            ),
            (
                json!({"session_id":"native","hook_event_name":"PostToolUse","tool_use_id":"q","tool_name":"Bash","tool_input":{"command":"rm x"}}),
                5,
            ),
        ] {
            hook(&d, v, t);
        }
        assert!(d.list_waiting_sessions().unwrap().is_empty());
    }

    #[test]
    fn fix5_자동_승인된_다른_호출은_응답한_승인을_붙잡지_않는다() {
        let d = db();
        for (v, t) in [
            (
                json!({"session_id":"native","hook_event_name":"PreToolUse","tool_use_id":"slow","tool_name":"Bash","tool_input":{"command":"sleep 100"}}),
                1,
            ),
            (
                json!({"session_id":"native","hook_event_name":"PreToolUse","tool_use_id":"q","tool_name":"Bash","tool_input":{"command":"rm x"}}),
                2,
            ),
            (
                json!({"session_id":"native","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"rm x"}}),
                3,
            ),
            (
                json!({"session_id":"native","hook_event_name":"PostToolUse","tool_use_id":"q","tool_name":"Bash","tool_input":{"command":"rm x"}}),
                4,
            ),
        ] {
            hook(&d, v, t);
        }
        assert!(d.list_waiting_sessions().unwrap().is_empty());
    }

    #[test]
    fn fix5_취소된_턴의_늦은_시작은_새_승인을_붙잡지_않는다() {
        let d = db();
        for (v, t) in [
            (
                json!({"session_id":"native","hook_event_name":"UserPromptSubmit","prompt_id":"old"}),
                1,
            ),
            (
                json!({"session_id":"native","hook_event_name":"StopFailure","prompt_id":"old"}),
                3,
            ),
            (
                json!({"session_id":"native","hook_event_name":"UserPromptSubmit","prompt_id":"new"}),
                4,
            ),
            (
                json!({"session_id":"native","hook_event_name":"PreToolUse","prompt_id":"old","tool_use_id":"old","tool_name":"Bash"}),
                2,
            ),
            (
                json!({"session_id":"native","hook_event_name":"PreToolUse","prompt_id":"new","tool_use_id":"q","tool_name":"Bash"}),
                5,
            ),
            (
                json!({"session_id":"native","hook_event_name":"PermissionRequest","prompt_id":"new","tool_name":"Bash"}),
                6,
            ),
            (
                json!({"session_id":"native","hook_event_name":"PostToolUse","prompt_id":"new","tool_use_id":"q","tool_name":"Bash"}),
                7,
            ),
        ] {
            hook(&d, v, t);
        }
        assert!(d.list_waiting_sessions().unwrap().is_empty());
    }

    #[test]
    fn fix5_결과_뒤_저장된_과거_승인은_다시_열리지_않는다() {
        let d = db();
        hook(
            &d,
            json!({"session_id":"native","hook_event_name":"PreToolUse","tool_use_id":"q","tool_name":"Bash"}),
            1,
        );
        hook(
            &d,
            json!({"session_id":"native","hook_event_name":"PostToolBatch","tool_calls":[{"tool_use_id":"q","tool_name":"Bash"}]}),
            3,
        );
        // 대기 요청이 없는 배치는 조회하지 않으므로 직접 결과도 기록한다.
        hook(
            &d,
            json!({"session_id":"native","hook_event_name":"PostToolUse","tool_use_id":"q","tool_name":"Bash"}),
            3,
        );
        hook(
            &d,
            json!({"session_id":"native","hook_event_name":"PermissionRequest","tool_name":"Bash"}),
            2,
        );
        assert!(d.list_waiting_sessions().unwrap().is_empty());
        hook(
            &d,
            json!({"session_id":"native","hook_event_name":"PreToolUse","tool_use_id":"next","tool_name":"Bash"}),
            4,
        );
        hook(
            &d,
            json!({"session_id":"native","hook_event_name":"PermissionRequest","tool_name":"Bash"}),
            5,
        );
        // 과거 동일 입력 결과를 다시 읽어도 새 승인은 유지한다.
        hook(
            &d,
            json!({"session_id":"native","hook_event_name":"PostToolBatch","tool_calls":[{"tool_use_id":"q","tool_name":"Bash"}]}),
            6,
        );
        assert_eq!(d.list_waiting_sessions().unwrap().len(), 1);
    }

    #[test]
    fn fix5_identical_parallel_permissions_remain_independent() {
        let d = db();
        for (i, id) in ["q1", "q2"].iter().enumerate() {
            hook(
                &d,
                json!({"session_id":"native","hook_event_name":"PreToolUse","tool_use_id":id,"tool_name":"Bash","tool_input":{"command":"ls"}}),
                i as i64 * 2 + 1,
            );
            hook(
                &d,
                json!({"session_id":"native","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"ls"}}),
                i as i64 * 2 + 2,
            );
        }
        hook(
            &d,
            json!({"session_id":"native","hook_event_name":"PostToolUse","tool_use_id":"q1","tool_name":"Bash","tool_input":{"command":"ls"}}),
            5,
        );
        assert_eq!(
            d.list_waiting_sessions().unwrap().len(),
            1,
            "q2 has no result"
        );
    }
    #[test]
    fn fix5_large_question_result_is_reconciled_by_stop() {
        let d = db();
        hook(
            &d,
            json!({"session_id":"native","hook_event_name":"PreToolUse","tool_use_id":"q","tool_name":"AskUserQuestion","tool_input":{"questions":[]}}),
            1,
        );
        let dir = std::env::temp_dir().join(format!("deppy-f46-large-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("native.jsonl");
        let result = json!({"message":{"content":[{"type":"tool_result","tool_use_id":"q","content":"x".repeat(300*1024)}]}});
        std::fs::write(&path, result.to_string() + "\n").unwrap();
        // 직접 결과 훅이 없는 경우에도 큰 transcript 결과의 ID로 해제한다.
        hook(
            &d,
            json!({"session_id":"native","hook_event_name":"Stop","transcript_path":path}),
            3,
        );
        assert!(
            d.list_waiting_sessions().unwrap().is_empty(),
            "answered question still pending: {:?}",
            d.pending_agent_request_ids(KEY, "native").unwrap()
        );
    }
    #[test]
    fn fix5_child_transcript_result_must_clear_permission_alias() {
        let d = db();
        hook(
            &d,
            json!({"session_id":"native","agent_id":"a","hook_event_name":"PreToolUse","tool_use_id":"q","tool_name":"Bash","tool_input":{"command":"ls"}}),
            1,
        );
        hook(
            &d,
            json!({"session_id":"native","agent_id":"a","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"ls"}}),
            2,
        );
        let dir = std::env::temp_dir().join(format!("deppy-f46-cancel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent-a.jsonl");
        std::fs::write(&path,json!({"message":{"content":[{"type":"tool_result","tool_use_id":"q","is_error":true}]}}).to_string()+"\n").unwrap();
        hook(
            &d,
            json!({"session_id":"native","agent_id":"a","hook_event_name":"SubagentStop","agent_transcript_path":path}),
            3,
        );
        hook(
            &d,
            json!({"session_id":"native","hook_event_name":"Stop"}),
            4,
        );
        assert!(d.list_waiting_sessions().unwrap().is_empty());
    }
    #[test]
    fn fix5_different_permissions_remain_independent_control() {
        let d = db();
        for (i, (id, command)) in [("q1", "ls"), ("q2", "pwd")].iter().enumerate() {
            hook(
                &d,
                json!({"session_id":"native","hook_event_name":"PreToolUse","tool_use_id":id,"tool_name":"Bash","tool_input":{"command":command}}),
                i as i64 * 2 + 1,
            );
            hook(
                &d,
                json!({"session_id":"native","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":command}}),
                i as i64 * 2 + 2,
            );
        }
        hook(
            &d,
            json!({"session_id":"native","hook_event_name":"PostToolUse","tool_use_id":"q1","tool_name":"Bash","tool_input":{"command":"ls"}}),
            5,
        );
        assert_eq!(d.list_waiting_sessions().unwrap().len(), 1);
    }
    #[test]
    fn fix5_small_question_result_is_reconciled_control() {
        let d = db();
        hook(
            &d,
            json!({"session_id":"native","hook_event_name":"PreToolUse","tool_use_id":"q","tool_name":"AskUserQuestion","tool_input":{"questions":[]}}),
            1,
        );
        let dir = std::env::temp_dir().join(format!("deppy-f46-small-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("native.jsonl");
        std::fs::write(&path,json!({"message":{"content":[{"type":"tool_result","tool_use_id":"q","content":"answered"}]}}).to_string()+"\n").unwrap();
        hook(
            &d,
            json!({"session_id":"native","hook_event_name":"Stop","transcript_path":path}),
            3,
        );
        assert!(d.list_waiting_sessions().unwrap().is_empty());
    }

    #[test]
    fn fix5_부모의_다음_훅은_중단된_자식만_해제한다() {
        let d = db();
        let dir =
            std::env::temp_dir().join(format!("deppy-child-followup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("native/subagents")).unwrap();
        for child in ["a", "background"] {
            hook(
                &d,
                json!({"session_id":"native","agent_id":child,"hook_event_name":"PreToolUse","tool_use_id":"q","tool_name":"Bash"}),
                1,
            );
            hook(
                &d,
                json!({"session_id":"native","agent_id":child,"hook_event_name":"PermissionRequest","tool_name":"Bash"}),
                2,
            );
        }
        std::fs::write(dir.join("native/subagents/agent-a.jsonl"), json!({"message":{"content":[{"type":"tool_result","tool_use_id":"q","is_error":true}]}}).to_string()+"\n").unwrap();
        hook(
            &d,
            json!({"session_id":"native","hook_event_name":"UserPromptSubmit","transcript_path":dir.join("native.jsonl")}),
            3,
        );
        let pending = d.pending_agent_request_ids(KEY, "native").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].starts_with(&scoped_request_id(&json!({"agent_id":"background"}), "")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fix5_큰_훅_결과는_메타데이터를_유지한다() {
        let raw = json!({"session_id":"native","hook_event_name":"PostToolUse","tool_use_id":"q","tool_response":"가".repeat(300*1024)}).to_string();
        let metadata = super::super::hook_payload::read(raw.as_bytes()).unwrap();
        assert_eq!(normalize(&metadata, 1).unwrap().request_id, "q");
        assert!(metadata.to_string().len() < 1024);
    }
    #[test]
    fn fix7_자식_도구의_수동_거절_배치는_승인_알림까지_해제한다() {
        let path = std::env::temp_dir().join(format!(
            "deppy-child-batch-{}.sqlite3",
            uuid::Uuid::new_v4()
        ));
        let db = storage::Db::open(&path).unwrap();
        let key = "315f68b6-333f-409f-a2c5-922b9eacfd7e:1";
        let mut v = json!({"session_id":"native","agent_id":"a","hook_event_name":"PreToolUse","tool_use_id":"q","tool_name":"Bash","tool_input":{"command":"ls"}});
        db.record_agent_attention(key, &normalize(&v, 1).unwrap())
            .unwrap();
        v.as_object_mut().unwrap().remove("tool_use_id");
        v["hook_event_name"] = json!("PermissionRequest");
        db.record_agent_attention(key, &normalize(&v, 2).unwrap())
            .unwrap();
        v["hook_event_name"] = json!("Notification");
        v["notification_type"] = json!("permission_prompt");
        db.record_agent_attention(key, &normalize(&v, 3).unwrap())
            .unwrap();
        let batch = json!({"session_id":"native","agent_id":"a","hook_event_name":"PostToolBatch","tool_calls":[{"tool_use_id":"q","tool_name":"Bash","tool_input":{"command":"ls"},"tool_response":"User denied"}]});
        reconcile_claude_results(&db, key, &batch, 4);
        assert!(db.list_waiting_sessions().unwrap().is_empty());
        drop(db);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn second_review_claude_실제_결과_경로는_부모와_자식을_분리한다() {
        let dir =
            std::env::temp_dir().join(format!("deppy-claude-result-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = storage::Db::open(&dir.join("metadata.sqlite3")).unwrap();
        let key = "315f68b6-333f-409f-a2c5-922b9eacfd7e:1";
        let mut parent = json!({"session_id":"native","hook_event_name":"PreToolUse","tool_name":"ExitPlanMode","tool_input":{"plan":"draft"},"tool_use_id":"q"});
        db.record_agent_attention(key, &normalize(&parent, 1).unwrap())
            .unwrap();
        let mut child = parent.clone();
        child["agent_id"] = json!("agent-a");
        db.record_agent_attention(key, &normalize(&child, 2).unwrap())
            .unwrap();
        let mut approval = parent.clone();
        approval.as_object_mut().unwrap().remove("tool_use_id");
        approval["hook_event_name"] = json!("PermissionRequest");
        let alias = normalize(&approval, 3).unwrap();
        db.record_agent_attention(key, &alias).unwrap();
        let path = dir.join("native.jsonl");
        std::fs::write(&path,"{\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"q\",\"is_error\":true}]}}\n").unwrap();
        parent["transcript_path"] = json!(path);
        parent["hook_event_name"] = json!("Stop");
        reconcile_claude_results(&db, key, &parent, 4);
        let pending = db.pending_agent_request_ids(key, "native").unwrap();
        assert!(!pending.contains(&"q".to_owned()));
        assert!(!pending.contains(&alias.request_id));
        assert!(pending.contains(&normalize(&child, 2).unwrap().request_id));
        // 배치 결과는 transcript 없이도 거절된 질문과 승인 별칭을 함께 해제한다.
        let batch = json!({"session_id":"native","hook_event_name":"PostToolBatch","tool_calls":[{"tool_name":"ExitPlanMode","tool_input":{"plan":"draft"},"tool_use_id":"q","tool_response":"User denied"}]});
        reconcile_claude_results(&db, key, &batch, 5);
        assert_eq!(
            db.pending_agent_request_ids(key, "native").unwrap(),
            vec![normalize(&child, 2).unwrap().request_id]
        );
        let child_path = dir.join("agent-agent-a.jsonl");
        std::fs::copy(path, &child_path).unwrap();
        child["agent_transcript_path"] = json!(child_path);
        child["hook_event_name"] = json!("SubagentStop");
        reconcile_claude_results(&db, key, &child, 6);
        assert!(
            db.pending_agent_request_ids(key, "native")
                .unwrap()
                .is_empty()
        );
        assert!(db.list_turn_done_sessions().unwrap().is_empty());
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn second_review_자식_승인은_추적하지만_부모의_완료로_취급하지_않는다() {
        let mut v = json!({"session_id":"parent","agent_id":"a","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_use_id":"q"});
        let a = normalize(&v, 1).expect("자식 승인");
        assert_eq!(a.kind, K::ApprovalRequired);
        v["agent_id"] = json!("b");
        assert_ne!(a.request_id, normalize(&v, 2).unwrap().request_id);
        v["agent_id"] = json!("a");
        v["hook_event_name"] = json!("PostToolUse");
        assert_eq!(a.request_id, normalize(&v, 3).unwrap().request_id);
        v["hook_event_name"] = json!("Stop");
        assert!(normalize(&v, 4).is_none());
    }

    #[test]
    fn second_review_claude_과거_거절_결과는_정확한_요청만_닫는다() {
        let text = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"q","name":"ExitPlanMode","input":{"plan":"draft"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"q","is_error":true,"content":"User denied"}]}}
"#;
        let value = json!({"session_id":"s"});
        let alias = tool_fingerprint(&json!({"tool_input":{"plan":"draft"}}), "ExitPlanMode");
        let result =
            claude_finished_requests(text, &["q".into(), alias.clone(), "other".into()], &value);
        assert!(result.contains(&"q".to_owned()));
        assert!(!result.contains(&alias));
        assert!(!result.contains(&"other".to_owned()));
    }

    #[test]
    fn review_grok_완료는_최신_실제_턴의_성공_결과로만_확인한다() {
        let completed = "{\"type\":\"turn_ended\",\"outcome\":\"completed\"}\n";
        assert!(grok_completed(completed));
        assert!(!grok_completed(&format!(
            "{completed}{{\"type\":\"turn_started\"}}\n"
        )));
        assert!(!grok_completed(
            "{\"type\":\"turn_ended\",\"outcome\":\"cancelled\"}\n"
        ));
    }

    #[test]
    fn review_codex_실패_결과도_기록에서_요청_아이디로_해제한다() {
        let text = r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"bad","output":"request_user_input is not available"}}
{"type":"response_item","payload":{"type":"function_call","name":"request_user_input","call_id":"pending","arguments":"{}"}}
"#;
        assert_eq!(
            codex_finished_requests(text, &["bad".into(), "pending".into()]),
            vec!["bad"]
        );
    }

    #[test]
    fn review_grok_유휴_알림은_성공_완료로_단정하지_않는다() {
        let value = json!({"sessionId":"s","hookEventName":"notification","notificationType":"idle_prompt"});
        assert_eq!(normalize(&value, 1).unwrap().kind, K::IdleObserved);
    }

    #[test]
    fn review_서버가_다른_입력_요청을_한_요청으로_합치지_않는다() {
        let a = json!({"session_id":"s","hook_event_name":"Elicitation","mcp_server_name":"a","mode":"form"});
        let b = json!({"session_id":"s","hook_event_name":"Elicitation","mcp_server_name":"b","mode":"form"});
        assert_ne!(
            normalize(&a, 1).unwrap().request_id,
            normalize(&b, 2).unwrap().request_id
        );
        let mut result = a.clone();
        result["hook_event_name"] = json!("ElicitationResult");
        assert_eq!(
            normalize(&a, 1).unwrap().request_id,
            normalize(&result, 2).unwrap().request_id
        );
    }

    #[test]
    fn agent_attention_세_질문_도구를_응답_필요로_정규화한다() {
        for name in [
            "AskUserQuestion",
            "ExitPlanMode",
            "request_user_input",
            "ask_user_question",
        ] {
            let value = json!({"session_id":"s","hook_event_name":"PreToolUse","tool_name":name,"tool_use_id":"q"});
            let e = normalize(&value, 1).unwrap();
            assert_eq!(e.kind, K::ResponseRequired);
            assert_eq!(e.request_id, "q");
        }
        let grok = json!({"sessionId":"s","hookEventName":"pre_tool_use","toolName":"ask_user_question","toolUseId":"gq","promptId":"turn"});
        let e = normalize(&grok, 2).unwrap();
        assert_eq!(e.kind, K::ResponseRequired);
        assert_eq!(e.request_id, "gq");
        assert_eq!(e.turn_id.as_deref(), Some("turn"));
    }

    #[test]
    fn agent_attention_유휴알림은_질문이나_승인으로_오인하지_않는다() {
        for (notification, expected) in [
            ("idle_prompt", Some(K::IdleObserved)),
            ("permission_prompt", Some(K::ApprovalRequired)),
            ("elicitation_dialog", Some(K::ResponseRequired)),
            ("auth_success", None),
        ] {
            let value = json!({"session_id":"s","hook_event_name":"Notification","notification_type":notification});
            assert_eq!(normalize(&value, 1).map(|e| e.kind), expected);
        }
    }

    #[test]
    fn agent_attention_질문_결과와_실패는_같은_요청만_해제한다() {
        for name in ["PostToolUse", "PostToolUseFailure"] {
            let value = json!({"session_id":"s","hook_event_name":name,"tool_name":"AskUserQuestion","tool_use_id":"q"});
            let e = normalize(&value, 1).unwrap();
            assert_eq!(e.kind, K::Resolved);
            assert_eq!(e.request_id, "q");
        }
    }

    #[test]
    fn fix5_동일_도구는_입력이_수정되어도_실행_그룹을_유지한다() {
        let a = json!({"session_id":"s","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"echo a"}});
        let b = json!({"session_id":"s","hook_event_name":"PreToolUse","tool_name":"Bash","tool_use_id":"q","tool_input":{"command":"echo b"}});
        assert_eq!(
            normalize(&a, 1).unwrap().tool_group,
            normalize(&b, 2).unwrap().tool_group
        );
    }
}
