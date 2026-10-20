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
fn bounded(v: &Value, depth: usize, nodes: &mut usize) -> bool {
    *nodes += 1;
    if depth > 10 || *nodes > 32768 {
        return false;
    }
    match v {
        Value::String(s) => s.len() <= 32 * 1024,
        Value::Array(a) => a.len() <= 256 && a.iter().all(|v| bounded(v, depth + 1, nodes)),
        Value::Object(m) => {
            m.len() <= 256
                && m.iter()
                    .all(|(k, v)| k.len() <= 128 && bounded(v, depth + 1, nodes))
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
    if v["type"] == "approval" {
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
        ) || !bounded(v, 0, &mut 0))
    {
        return Err(WorkError);
    }
    let mut projection = v.clone();
    if !input || !approval {
        filter(&mut projection, input, approval);
    }
    let encoded =
        serde_json::to_string(&serde_json::json!({"type":"work_snapshot","snapshot":projection}))
            .map_err(|_| WorkError)?;
    if encoded.len() > MAX_WORK_SNAPSHOT_BYTES {
        Err(WorkError)
    } else {
        Ok(encoded)
    }
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
    }
    #[test]
    fn work_read_only_projection_strips_controls_and_approval_command() {
        let value = json!({"projects":[{"id":"w","main_input":{"enabled":true,"reason":""},"tasks":[{"approval":{"command":"private command"},"dispatch":{"enabled":true},"confirm_result":{"enabled":true},"input":{"enabled":true}}]}],"devices":[]});
        let frame: Value =
            serde_json::from_str(&encode_frame(&value, false, false).unwrap()).unwrap();
        assert_eq!(frame["type"], "work_snapshot");
        assert_eq!(
            frame["snapshot"]["projects"][0]["main_input"]["enabled"],
            false
        );
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
    }
    #[test]
    fn work_relay_outbound_rejects_unfiltered_approval_or_input_capability() {
        use crate::relay::contract::RelayPermissions;
        use crate::relay_client::RelayMessageAdapter;
        let readonly = RelayMessageAdapter::new(RelayPermissions::default());
        let unsafe_view = json!({"projects":[{"tasks":[{"approval":{"command":"private command"},"input":{"enabled":true}}]}]});
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
