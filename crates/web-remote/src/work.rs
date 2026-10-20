//! Bounded public Command Center projection; no independent transcript or worker.
use crate::relay::contract::RelayAction;
use serde_json::Value;

pub const MAX_WORK_SNAPSHOT_BYTES: usize = 480 * 1024;

fn identity(v: &Value) -> bool {
    v.as_str()
        .is_some_and(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
}
fn keys(v: &Value, allowed: &[&str]) -> bool {
    v.as_object()
        .is_some_and(|m| m.keys().all(|k| allowed.contains(&k.as_str())))
}
fn target(v: &Value) -> bool {
    keys(v, &["workspace_id", "session_id", "task_id"])
        && identity(&v["workspace_id"])
        && ["session_id", "task_id"]
            .iter()
            .all(|k| v[*k].is_null() || identity(&v[*k]))
}
fn text(v: &Value, max: usize) -> bool {
    v.as_str().is_some_and(|s| {
        !s.trim().is_empty()
            && s.len() <= max
            && !s
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    })
}
fn bounded(v: &Value, depth: usize, nodes: &mut usize) -> bool {
    bounded_with_limit(v, depth, nodes, 32768)
}
fn bounded_with_limit(v: &Value, depth: usize, nodes: &mut usize, limit: usize) -> bool {
    *nodes += 1;
    if depth > 10 || *nodes > limit {
        return false;
    }
    match v {
        Value::String(s) => s.len() <= 32 * 1024,
        Value::Array(a) => {
            a.len() <= 256
                && a.iter()
                    .all(|v| bounded_with_limit(v, depth + 1, nodes, limit))
        }
        Value::Object(m) => {
            m.len() <= 256
                && m.iter()
                    .all(|(k, v)| k.len() <= 128 && bounded_with_limit(v, depth + 1, nodes, limit))
        }
        _ => true,
    }
}

/// This allowlist is a transport ceiling. The App still decodes the typed intent,
/// validates current capabilities and exact route, and records the operation once.
pub fn validate_intent(v: &Value) -> bool {
    if v["type"] == "install_plan_json" {
        return keys(v, &["type", "workspace_id", "json"])
            && identity(&v["workspace_id"])
            && v["json"]
                .as_str()
                .is_some_and(|s| !s.trim().is_empty() && s.len() <= 128 * 1024);
    }
    if !bounded(v, 0, &mut 0) {
        return false;
    }
    match v["type"].as_str() {
        Some("load_project") => keys(v, &["type", "workspace_id"]) && identity(&v["workspace_id"]),
        Some("submit") => {
            keys(v, &["type", "target", "text"])
                && target(&v["target"])
                && v["text"].as_str().is_some_and(|s| {
                    !s.trim().is_empty()
                        && s.len() <= 16 * 1024
                        && !s
                            .chars()
                            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
                })
        }
        Some("approval") => {
            keys(v, &["type", "target", "approval_id", "allow"])
                && target(&v["target"])
                && identity(&v["approval_id"])
                && v["allow"].is_boolean()
        }
        Some("confirm_result") => {
            keys(v, &["type", "target", "revision"])
                && target(&v["target"])
                && identity(&v["revision"])
        }
        Some("report_result") => {
            keys(v, &["type", "target", "attempt_id", "text"])
                && target(&v["target"])
                && identity(&v["target"]["task_id"])
                && identity(&v["attempt_id"])
                && text(&v["text"], 8 * 1024)
        }
        Some("verify_result") => {
            keys(
                v,
                &[
                    "type",
                    "target",
                    "attempt_id",
                    "revision",
                    "passed",
                    "evidence",
                ],
            ) && target(&v["target"])
                && identity(&v["target"]["task_id"])
                && identity(&v["attempt_id"])
                && identity(&v["revision"])
                && v["passed"].is_boolean()
                && text(&v["evidence"], 4 * 1024)
        }
        Some("dispatch_task") => {
            keys(v, &["type", "target"])
                && target(&v["target"])
                && identity(&v["target"]["task_id"])
        }
        Some("confirm_plan") => {
            keys(v, &["type", "workspace_id", "goal_id"])
                && identity(&v["workspace_id"])
                && identity(&v["goal_id"])
        }
        Some("save_project") => {
            keys(v, &["type", "workspace_id", "config"])
                && identity(&v["workspace_id"])
                && keys(
                    &v["config"],
                    &[
                        "name",
                        "directory",
                        "main_session_id",
                        "provider",
                        "model",
                        "effort",
                        "worker_providers",
                        "allowed_scope_roots",
                        "parallel_limit",
                        "automation",
                        "use_worktrees",
                        "revision",
                    ],
                )
        }
        _ => false,
    }
}

pub fn required_action(v: &Value) -> RelayAction {
    if v["type"] == "load_project" {
        RelayAction::View
    } else if v["type"] == "approval" {
        RelayAction::Approval
    } else {
        RelayAction::Input
    }
}

/// Projection is generated only from the public native view DTO. Encode outside
/// the publication mutex; malformed/oversize updates cannot evict the last good one.
pub fn encode_frame(v: &Value, input: bool, approval: bool) -> Result<String, WorkError> {
    if !v.is_null()
        && (!keys(
            v,
            &[
                "projects",
                "selected_workspace_id",
                "devices",
                "dark_mode",
                "font_scale",
                "labels",
                "add_project",
                "manage_devices",
                "status",
                "receipts",
            ],
        ) || !bounded_with_limit(v, 0, &mut 0, 131072))
    {
        return Err(WorkError);
    }
    let mut projection = v.clone();
    if !input || !approval {
        filter(&mut projection, input, approval);
    }
    let frame = |projection: &Value| {
        serde_json::to_string(&serde_json::json!({"type":"work_snapshot","snapshot":projection}))
            .map_err(|_| WorkError)
    };
    let encoded = frame(&projection)?;
    if encoded.len() <= MAX_WORK_SNAPSHOT_BYTES && bounded(&projection, 0, &mut 0) {
        return Ok(encoded);
    }
    compact_projection(&mut projection)?;
    let encoded = frame(&projection)?;
    if encoded.len() > MAX_WORK_SNAPSHOT_BYTES || !bounded(&projection, 0, &mut 0) {
        return Err(WorkError);
    }
    Ok(encoded)
}
/// Summarize display text only. Exact targets, permissions, attempts, revisions, planner JSON,
/// project configuration and approval evidence remain intact. Session choices are interned once
/// per project instead of repeating a full DTO for every task. Never truncate executable data.
fn compact_projection(projection: &mut Value) -> Result<(), WorkError> {
    fn short(value: &mut Value, limit: usize) {
        if let Some(text) = value.as_str()
            && text.len() > limit
        {
            let mut end = limit.saturating_sub(3);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            *value = Value::String(format!("{}…", &text[..end]));
        }
    }
    let Some(projects) = projection.get_mut("projects").and_then(Value::as_array_mut) else {
        return Err(WorkError);
    };
    for project in projects {
        for key in ["goal", "main_summary"] {
            if let Some(value) = project.get_mut(key) {
                short(value, 512);
            }
        }
        if let Some(history) = project.get_mut("history").and_then(Value::as_array_mut) {
            history.truncate(20);
            for event in history {
                for key in ["text", "detail"] {
                    if let Some(value) = event.get_mut(key) {
                        short(value, 256);
                    }
                }
            }
        }
        let mut workers = serde_json::Map::new();
        if let Some(tasks) = project.get_mut("tasks").and_then(Value::as_array_mut) {
            for task in tasks {
                for (key, limit) in [
                    ("title", 128),
                    ("role", 64),
                    ("scope", 128),
                    ("acceptance_criteria", 192),
                    ("result", 320),
                    ("directory", 256),
                    ("activity", 128),
                ] {
                    if let Some(value) = task.get_mut(key) {
                        short(value, limit);
                    }
                }
                if let Some(choices) = task
                    .as_object_mut()
                    .and_then(|t| t.remove("eligible_sessions"))
                    .and_then(|v| v.as_array().cloned())
                {
                    let mut keys = Vec::with_capacity(choices.len());
                    for mut worker in choices {
                        let key = worker["target"]["session_id"]
                            .as_str()
                            .filter(|id| !id.is_empty())
                            .ok_or(WorkError)?
                            .to_owned();
                        if let Some(value) = worker.get_mut("title") {
                            short(value, 128);
                        }
                        if let Some(existing) = workers.get(&key) {
                            if existing != &worker {
                                return Err(WorkError);
                            }
                        } else {
                            workers.insert(key.clone(), worker);
                        }
                        keys.push(Value::String(key));
                    }
                    task["eligible_session_keys"] = Value::Array(keys);
                }
            }
        }
        if workers.len() > 256 {
            return Err(WorkError);
        }
        if !workers.is_empty() {
            project["worker_sessions"] = Value::Array(workers.into_values().collect());
        }
    }
    let existing = projection
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("");
    projection["status"] = Value::String(format!(
        "{existing} · 긴 설명과 이력은 요약 표시합니다. 원문은 앱과 터미널에서 확인해 주세요."
    ));
    Ok(())
}
/// Second outbound gate for callers that already hold a typed ServerMsg.
pub fn projection_allowed(value: &Value, input: bool, approval: bool) -> bool {
    let mut filtered = value.clone();
    filter(&mut filtered, input, approval);
    filtered == *value
}

fn filter(value: &mut Value, input: bool, approval: bool) {
    match value {
        Value::Array(a) => {
            for v in a {
                filter(v, input, approval);
            }
        }
        Value::Object(m) => {
            for (key, v) in m {
                if key == "approval" && !approval {
                    *v = Value::Null;
                    continue;
                }
                if !input
                    && [
                        "main_input",
                        "input",
                        "dispatch",
                        "confirm_result",
                        "plan_confirmation",
                        "manual_plan",
                        "storage_retry",
                        "result_report",
                        "verify_result",
                        "launch",
                        "create_main",
                        "create_worker",
                        "close_worker",
                        "revoke",
                        "save_settings",
                        "add_project",
                        "manage_devices",
                    ]
                    .contains(&key.as_str())
                    && let Some(cap) = v.as_object_mut()
                {
                    cap.insert("enabled".into(), Value::Bool(false));
                    cap.insert(
                        "reason".into(),
                        Value::String("이 연결은 보기 전용입니다.".into()),
                    );
                }
                filter(v, input, approval);
            }
        }
        _ => {}
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkError;
impl std::fmt::Display for WorkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid work projection")
    }
}
impl std::error::Error for WorkError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn work_intents_require_exact_target_and_bound_text() {
        let valid = json!({"type":"submit","target":{"workspace_id":"w","session_id":"opaque:1","task_id":null},"text":"새 입력"});
        assert!(validate_intent(&valid));
        let mut bad = valid.clone();
        bad["target"]["workspace_id"] = json!("");
        assert!(!validate_intent(&bad));
        let mut bad = valid.clone();
        bad["target"]["runtime_id"] = json!(1);
        assert!(!validate_intent(&bad));
        let mut bad = valid.clone();
        bad["text"] = json!("x".repeat(16385));
        assert!(!validate_intent(&bad));
        let mut bad = valid;
        bad["token"] = json!("secret");
        assert!(!validate_intent(&bad));
    }
    #[test]
    fn work_approval_requires_approval_permission_not_input() {
        let approval = json!({"type":"approval","target":{"workspace_id":"w","session_id":"opaque:1","task_id":"t"},"approval_id":"a","allow":true});
        assert!(validate_intent(&approval));
        assert_eq!(required_action(&approval), RelayAction::Approval);
        assert!(!validate_intent(&json!({"type":"use_original_layout"})));
        let view = json!({"type":"load_project","workspace_id":"workspace-b"});
        assert!(validate_intent(&view));
        assert_eq!(required_action(&view), RelayAction::View);
        assert!(!validate_intent(
            &json!({"type":"load_project","workspace_id":"w","path":"/untrusted"})
        ));
    }
    #[test]
    fn work_result_forms_require_exact_attempt_and_bounded_user_evidence() {
        let target = json!({"workspace_id":"w","session_id":null,"task_id":"t"});
        let mut report = json!({"type":"report_result","target":target,"attempt_id":"attempt-a","text":"사용자가 확인한 결과"});
        assert!(validate_intent(&report));
        report["attempt_id"] = Value::Null;
        assert!(!validate_intent(&report));
        report["attempt_id"] = json!("attempt-a");
        report["text"] = json!("x".repeat(8193));
        assert!(!validate_intent(&report));
        let mut verify = json!({"type":"verify_result","target":target,"attempt_id":"attempt-a","revision":"2","passed":true,"evidence":"사용자 검증 기록"});
        assert!(validate_intent(&verify));
        verify["revision"] = json!("");
        assert!(!validate_intent(&verify));
        verify["revision"] = json!("2");
        verify["evidence"] = json!("x".repeat(4097));
        assert!(!validate_intent(&verify));
    }
    #[test]
    fn work_read_only_projection_strips_controls_and_approval_command() {
        let value = json!({"projects":[{"id":"w","main_input":{"enabled":true,"reason":""},"create_main":{"enabled":true},"manual_plan":{"enabled":true},"storage_retry":{"enabled":true},"tasks":[{"create_worker":{"enabled":true},"close_worker":{"enabled":true},"approval":{"command":"private command"},"dispatch":{"enabled":true},"result_report":{"enabled":true},"verify_result":{"enabled":true},"confirm_result":{"enabled":true},"input":{"enabled":true}}]}],"devices":[{"revoke":{"enabled":true}}]});
        let frame: Value =
            serde_json::from_str(&encode_frame(&value, false, false).unwrap()).unwrap();
        assert_eq!(frame["type"], "work_snapshot");
        assert_eq!(
            frame["snapshot"]["projects"][0]["main_input"]["enabled"],
            false
        );
        for key in ["manual_plan", "storage_retry", "create_main"] {
            assert_eq!(frame["snapshot"]["projects"][0][key]["enabled"], false);
        }
        for key in [
            "result_report",
            "verify_result",
            "create_worker",
            "close_worker",
        ] {
            assert_eq!(
                frame["snapshot"]["projects"][0]["tasks"][0][key]["enabled"],
                false
            );
        }
        assert_eq!(frame["snapshot"]["devices"][0]["revoke"]["enabled"], false);
        assert!(frame["snapshot"]["projects"][0]["tasks"][0]["approval"].is_null());
        assert_eq!(value["projects"][0]["main_input"]["enabled"], true);
    }
    #[test]
    fn work_projection_is_bounded_and_null_clears_layout() {
        assert!(encode_frame(&json!(null), false, false).is_ok());
        assert!(encode_frame(&json!({"projects":[],"token":"secret"}), true, true).is_err());
        assert!(
            encode_frame(
                &json!({"projects":[{"name":"x".repeat(MAX_WORK_SNAPSHOT_BYTES)}]}),
                true,
                true
            )
            .is_err()
        );
    }
    #[test]
    fn work_large_valid_projects_publish_compact_read_models_without_losing_task_identity() {
        let mut task = json!({"target":{"workspace_id":"workspace-a","session_id":"exact-session","task_id":"task-a"},"attempt_id":"attempt-a","title":"한글".repeat(80),"role":"implementation","scope":"src/".repeat(200),"acceptance_criteria":"검증 근거 ".repeat(100),"result":"실제 응답 ".repeat(180),"result_revision":"7","directory":"/project/".repeat(200),"approval":{"id":"approval-a","command":"cargo test --locked","action":{"enabled":true}},"eligible_sessions":[]});
        task["eligible_sessions"]=json!((0..16).map(|i|json!({"target":{"workspace_id":"workspace-a","session_id":format!("opaque-worker-{i}"),"task_id":null},"title":"실행 세션 ".repeat(40),"provider":"codex","device":"local","state":"queued","input":{"enabled":true}})).collect::<Vec<_>>());
        let projects: Vec<_> = (0..4).map(|p| {
            let tasks:Vec<_>=(0..64).map(|t| { let mut task=task.clone();task["target"]["workspace_id"]=json!(format!("workspace-{p}"));task["target"]["task_id"]=json!(format!("task-{p}-{t}"));for worker in task["eligible_sessions"].as_array_mut().unwrap(){worker["target"]["workspace_id"]=json!(format!("workspace-{p}"));}task }).collect();
            json!({"id":format!("workspace-{p}"),"tasks":tasks,"history":(0..100).map(|i|json!({"id":format!("e-{i}"),"text":"이력 응답 ".repeat(150)})).collect::<Vec<_>>()})
        }).collect();
        let source = json!({"projects":projects,"devices":[],"status":"Observed"});
        let encoded = encode_frame(&source, true, true)
            .expect("valid bounded projects should keep publishing");
        assert!(encoded.len() <= MAX_WORK_SNAPSHOT_BYTES);
        let frame: Value = serde_json::from_str(&encoded).unwrap();
        for p in 0..4 {
            let tasks = frame["snapshot"]["projects"][p]["tasks"]
                .as_array()
                .unwrap();
            assert_eq!(tasks.len(), 64);
            assert_eq!(
                frame["snapshot"]["projects"][p]["worker_sessions"]
                    .as_array()
                    .unwrap()
                    .len(),
                16
            );
            for (t, task) in tasks.iter().enumerate() {
                assert_eq!(task["target"]["task_id"], format!("task-{p}-{t}"));
                assert_eq!(task["attempt_id"], "attempt-a");
                assert_eq!(task["result_revision"], "7");
                assert_eq!(task["approval"]["command"], "cargo test --locked");
            }
        }
        assert!(
            frame["snapshot"]["status"]
                .as_str()
                .unwrap()
                .contains("요약")
        );
        assert_eq!(
            source["projects"][0]["history"].as_array().unwrap().len(),
            100
        );
    }
    #[test]
    fn work_publication_coalesces_and_rejects_bad_update_without_eviction() {
        let core = crate::session_core::SessionCore::spawn(None);
        let perms = crate::relay::contract::RelayPermissions::new(true, true, true);
        let good = json!({"projects":[],"devices":[]});
        core.publish_work_snapshot(good.clone()).unwrap();
        let (version, frame) = core
            .dashboard()
            .work_snapshot_for_permissions(0, perms)
            .unwrap();
        core.publish_work_snapshot(good).unwrap();
        assert!(
            core.dashboard()
                .work_snapshot_for_permissions(version, perms)
                .is_none()
        );
        assert!(
            core.publish_work_snapshot(json!({"token":"secret"}))
                .is_err()
        );
        assert_eq!(
            core.dashboard()
                .work_snapshot_for_permissions(0, perms)
                .unwrap()
                .1,
            frame
        );
        assert!(
            core.dashboard()
                .work_snapshot_for_permissions(
                    0,
                    crate::relay::contract::RelayPermissions::new(false, false, false)
                )
                .is_none()
        );
    }
    #[test]
    fn work_queued_authority_is_revoked_and_view_only_cannot_submit() {
        use crate::cloud::*;
        use crate::relay::contract::RelayPermissions;
        use std::sync::Arc;
        let core = crate::session_core::SessionCore::spawn(None);
        let policy = CloudControlPolicy {
            authorization_epoch: [3; 16],
            permissions: RelayPermissions::new(true, true, true),
            expires_at: None,
        };
        let origin = core.dashboard().cloud_control_origin(
            CloudControlOrigin::DirectWeb { connection_id: 7 },
            policy,
            Arc::new(move || Some(policy)),
        );
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        core.set_cloud_control_sink(Arc::new(move |r| tx.try_send(r).is_ok()));
        let intent = CloudControlIntent::CommandCenter {
            operation_id: uuid::Uuid::new_v4().to_string(),
            intent: json!({"type":"submit","target":{"workspace_id":"w","session_id":"opaque:1","task_id":null},"text":"same text is still a new input"}),
        };
        assert!(
            core.dashboard()
                .request_cloud_control(&origin, intent.clone())
        );
        origin.revoke();
        assert!(
            core.authorize_cloud_control(rx.try_recv().unwrap())
                .is_none()
        );
        let readonly = CloudControlPolicy {
            permissions: RelayPermissions::new(true, false, false),
            ..policy
        };
        let origin = core.dashboard().cloud_control_origin(
            CloudControlOrigin::DirectWeb { connection_id: 8 },
            readonly,
            Arc::new(move || Some(readonly)),
        );
        assert!(!core.dashboard().request_cloud_control(&origin, intent));
        assert!(rx.try_recv().is_err());
        // Hydrating a known project's read model needs view authority only.
        let load = CloudControlIntent::CommandCenter {
            operation_id: uuid::Uuid::new_v4().to_string(),
            intent: json!({"type":"load_project","workspace_id":"workspace-b"}),
        };
        assert!(core.dashboard().request_cloud_control(&origin, load));
        let (_, permit) = core
            .authorize_cloud_control_deferred(rx.try_recv().unwrap())
            .expect("view-only project read is authorized");
        assert!(permit.is_current());
        origin.revoke();
        assert!(!permit.is_current());
    }
    #[test]
    fn work_relay_outbound_rejects_unfiltered_approval_or_input_capability() {
        use crate::relay::contract::RelayPermissions;
        use crate::relay_client::RelayMessageAdapter;
        let readonly = RelayMessageAdapter::new(RelayPermissions::default());
        let unsafe_view = json!({"projects":[{"tasks":[{"create_worker":{"enabled":true},"approval":{"command":"private command"},"input":{"enabled":true}}]}]});
        assert!(
            !readonly.may_emit(&crate::protocol::ServerMsg::WorkSnapshot {
                snapshot: unsafe_view.clone()
            })
        );
        let safe: Value =
            serde_json::from_str(&encode_frame(&unsafe_view, false, false).unwrap()).unwrap();
        assert!(
            readonly.may_emit(&crate::protocol::ServerMsg::WorkSnapshot {
                snapshot: safe["snapshot"].clone()
            })
        );
    }
}
