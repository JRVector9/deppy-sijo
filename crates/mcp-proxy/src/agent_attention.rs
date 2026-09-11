//! 세 CLI 훅을 관찰 전용 상태 이벤트로 바꾼다. 도구 인자와 답변 본문은 보관하지 않는다.
use serde_json::Value;
use storage::{AgentAttentionEvent, AttentionEventKind as K};

use std::hash::{Hash, Hasher};

fn tool_fingerprint(v: &Value, tool: &str) -> String {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    tool.hash(&mut hash);
    v.get("tool_input")
        .or_else(|| v.get("toolInput"))
        .unwrap_or(&Value::Null)
        .to_string()
        .hash(&mut hash);
    format!("tool:{:016x}", hash.finish())
}

fn field<'a>(v: &'a Value, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| v.get(name).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
}

pub fn normalize(v: &Value, at_micros: i64) -> Option<AgentAttentionEvent> {
    if field(v, &["subagentType", "agent_id"]).is_some() {
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
    let turn_id = field(v, &["turn_id", "turnId", "promptId"]).map(str::to_owned);
    Some(AgentAttentionEvent {
        native_session_id: native.into(),
        turn_id,
        request_id: request,
        kind,
        at_micros,
    })
}

/// PermissionRequest는 도구 ID를 생략하기도 한다. 결과의 도구 이름 별칭도 함께 닫는다.
/// 다른 질문 ID나 도구 이름은 건드리지 않는다.
pub fn resolved_alias(v: &Value, at_micros: i64) -> Option<AgentAttentionEvent> {
    let mut event = normalize(v, at_micros)?;
    if event.kind != K::Resolved {
        return None;
    }
    event.request_id = tool_fingerprint(v, field(v, &["tool_name", "toolName"])?);
    Some(event)
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
    fn agent_attention_같은_도구의_서로_다른_승인은_섞이지_않는다() {
        let a = json!({"session_id":"s","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"echo a"}});
        let b = json!({"session_id":"s","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"echo b"}});
        assert_ne!(
            normalize(&a, 1).unwrap().request_id,
            normalize(&b, 2).unwrap().request_id
        );
        let mut result = a.clone();
        result["hook_event_name"] = json!("PostToolUse");
        result["tool_use_id"] = json!("id");
        assert_eq!(
            normalize(&a, 1).unwrap().request_id,
            resolved_alias(&result, 3).unwrap().request_id
        );
    }
}
