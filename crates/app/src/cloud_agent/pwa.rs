//! One redacted projection for native results and authenticated mobile operators.
use super::CloudAgent;
use agent_mcp::{AuthKind, ConnectionState, RunState, now};
use std::sync::Arc;
use web_remote::cloud::*;

impl super::Target {
    /// Reuse the App's visual-state contract; provider/model text is identity only.
    pub(crate) fn local_status_name(&self) -> Option<&'static str> {
        use crate::agent_surface::AgentVisualState as State;
        if !self.live {
            return Some("exited");
        }
        match State::from_pty_with_agent(self.local_status, self.local_agent_present) {
            State::Active => Some("running"),
            State::NeedsResponse => Some("waiting"),
            State::Waiting => Some("needs_approval"),
            State::Idle => Some("idle"),
            State::Error => Some("error"),
            State::Complete => Some("done"),
            State::Off
                if !self.local_agent_present
                    && (!self.known_ai
                        || self.local_status == Some(runtime::SessionStatus::Idle)) =>
            {
                Some("shell")
            }
            State::Off => None,
        }
    }

    pub(crate) fn local_status_label(&self, catalog: &i18n::Catalog) -> String {
        let key = match self.local_status_name() {
            Some("running") => "status.running",
            Some("waiting") => "status.waiting",
            Some("needs_approval") => "status.needs_approval",
            Some("idle") => "status.idle",
            Some("error") => "status.error",
            Some("done") => "status.done",
            Some("exited") => "cloud.local_exited",
            Some("shell") => "cloud.local_shell",
            _ => "cloud.local_unknown",
        };
        catalog.t(key, &[])
    }

    pub(crate) fn local_observation_label(&self, catalog: &i18n::Catalog) -> String {
        let Some(observed) = self
            .local_status_observed_at
            .filter(|_| self.local_status_name().is_some())
        else {
            return catalog.t("cloud.local_observation_unknown", &[]);
        };
        let age = crate::ui::notifications::relative_time_label(
            catalog,
            i64::try_from(now().saturating_sub(observed)).unwrap_or(i64::MAX),
        );
        catalog.t("cloud.local_observed", &[("age", &age)])
    }
}

fn last_connection_activity(info: &agent_mcp::ConnectionInfo) -> Option<i64> {
    [
        info.last_tool_success_at,
        info.last_initialize_at,
        info.last_authenticated_at,
    ]
    .into_iter()
    .flatten()
    .max()
    .map(|at| at as i64)
}

impl CloudAgent {
    pub fn operator_sink(&self, ctx: egui::Context) -> CloudControlSink {
        let tx = self.operator_tx.clone();
        Arc::new(move |request| {
            if !request.intent().validate() || tx.try_send(request).is_err() {
                return false;
            }
            ctx.request_repaint();
            true
        })
    }
    pub fn next_operator_intent(&self) -> Option<CloudControlRequest> {
        self.operator_rx.try_recv().ok()
    }
    pub fn operator_target(&self, id: &str, generation: &str) -> Option<super::Target> {
        self.targets
            .iter()
            .find(|t| t.id == id && t.generation == generation)
            .cloned()
    }
    pub fn publish_cloud_snapshot(&mut self, core: &web_remote::session_core::SessionCore) {
        if self
            .last_cloud_publish
            .is_some_and(|at| at.elapsed().as_secs() < 1)
        {
            return;
        }
        self.last_cloud_publish = Some(std::time::Instant::now());
        if core.publish_cloud_snapshot(self.cloud_snapshot()).is_err() {
            self.error = Some("cloud_snapshot_unavailable".into());
        }
    }
    pub fn cloud_snapshot(&self) -> CloudSnapshot {
        let infos = self.connection_infos();
        let runs = self.cloud_runs();
        let mut snapshot = CloudSnapshot {
            captured_at: now() as i64,
            ..Default::default()
        };
        snapshot.connections = infos
            .iter()
            .take(MAX_CLOUD_ROWS)
            .map(|info| CloudConnectionView {
                id: info.principal.id.clone(),
                name: self.redact(&info.principal.name),
                auth_kind: match info.principal.auth_kind {
                    AuthKind::ManualBearer => "manual_bearer",
                    AuthKind::OAuth => "oauth",
                }
                .into(),
                state: match info.state {
                    ConnectionState::Active if info.last_authenticated_at.is_some() => "active",
                    ConnectionState::Active => "waiting",
                    ConnectionState::Expired => "expired",
                    ConnectionState::Revoked => "revoked",
                }
                .into(),
                input_scope: if info.principal.input_scope {
                    "read+input"
                } else {
                    "read"
                }
                .into(),
                last_activity_at: last_connection_activity(info),
                expires_at: Some(info.principal.expires_at as i64),
            })
            .collect();
        snapshot.runs = runs
            .iter()
            .take(MAX_CLOUD_ROWS)
            .map(|run| {
                let target = self.targets.iter().find(|target| {
                    target.workspace == run.workspace
                        && target.id == run.session
                        && target.generation == run.generation
                });
                CloudRunView {
                    id: run.id.clone(),
                    connection_id: run.connection_id.clone(),
                    connection_name: self.redact(&run.connection_name),
                    workspace_id: run.workspace.clone(),
                    session_id: run.session.clone(),
                    generation: run.generation.clone(),
                    title: self.redact(&run.title),
                    state: serde_json::to_value(run.state)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_else(|| "unknown".into()),
                    message: self.redact(&run.message),
                    progress: run.progress,
                    agent_line: target.and_then(|target| target.agent_line.as_deref()).map(
                        |line| {
                            let mut text = self.redact(line);
                            super::truncate_utf8(&mut text, 512);
                            text
                        },
                    ),
                    local_status: target
                        .and_then(super::Target::local_status_name)
                        .map(str::to_owned),
                    local_status_observed_at: target
                        .filter(|target| target.local_status_name().is_some())
                        .and_then(|target| target.local_status_observed_at)
                        .and_then(|observed| i64::try_from(observed).ok()),
                    created_at: run.created,
                    updated_at: run.updated,
                    stale: run.state == RunState::Unknown,
                    result: run.result.as_deref().map(|result| self.redact(result)),
                    artifacts: run
                        .artifacts
                        .iter()
                        .filter(|a| a.validate().is_ok() && self.redact(&a.uri) == a.uri)
                        .map(|a| CloudArtifactView {
                            label: self.redact(&a.label),
                            uri: a.uri.clone(),
                        })
                        .filter(|a| a.validate().is_ok())
                        .collect(),
                    cancel_requested_at: run.cancel_requested_at,
                    cancel_acknowledged_at: run.cancel_acknowledged_at,
                }
            })
            .collect();
        snapshot.answers = self
            .records
            .iter()
            .filter(|r| r.tool == "notify" && !r.message.is_empty())
            .take(MAX_CLOUD_ROWS)
            .map(|record| {
                let connection = record
                    .connection_id
                    .as_deref()
                    .unwrap_or(agent_mcp::LEGACY_CONNECTION_ID);
                let display_text = |text: &str| {
                    let mut text = self.redaction.redact_wrapped_text(text);
                    super::truncate_utf8(&mut text, 512);
                    text
                };
                CloudAnswerView {
                    id: agent_mcp::operation_key(connection, &record.id)
                        .unwrap_or_else(|_| record.id.clone()),
                    connection_id: connection.into(),
                    connection_name: display_text(
                        infos
                            .iter()
                            .find(|i| i.principal.id == connection)
                            .map(|i| i.principal.name.as_str())
                            .unwrap_or(connection),
                    ),
                    workspace_id: record.workspace.clone(),
                    session_id: record.session.clone(),
                    generation: record
                        .generation
                        .clone()
                        .unwrap_or_else(|| "unknown".into()),
                    title: display_text(
                        self.targets
                            .iter()
                            .find(|t| t.id == record.session)
                            .map(|t| t.title.as_str())
                            .unwrap_or(&record.session),
                    ),
                    body: self.redaction.redact_wrapped_text(&record.message),
                    run_id: record.run_id.clone(),
                    run_label: record.run_id.as_deref().map(display_text),
                    created_at: record.created,
                    artifacts: Vec::new(),
                }
            })
            .collect();
        snapshot.approvals = self
            .server
            .as_ref()
            .map(|s| {
                s.auth
                    .approvals()
                    .into_iter()
                    .take(MAX_CLOUD_ROWS)
                    .map(|approval| CloudApprovalView {
                        id: approval.id,
                        connection_name: self.redact(&approval.client_name),
                        scope: if approval.input { "read+input" } else { "read" }.into(),
                        created_at: 0,
                    })
                    .collect()
            })
            .unwrap_or_default();
        snapshot.truncated = self.history_truncated || infos.len() > MAX_CLOUD_ROWS;
        snapshot.truncated |= runs
            .iter()
            .take(MAX_CLOUD_ROWS)
            .zip(&snapshot.runs)
            .any(|(source, view)| source.artifacts.len() != view.artifacts.len());
        let runs_before = snapshot.runs.len();
        snapshot.runs.retain(|run| {
            CloudSnapshot {
                runs: vec![run.clone()],
                ..Default::default()
            }
            .validate()
            .is_ok()
        });
        let answers_before = snapshot.answers.len();
        snapshot.answers.retain(|answer| {
            CloudSnapshot {
                answers: vec![answer.clone()],
                ..Default::default()
            }
            .validate()
            .is_ok()
        });
        snapshot.truncated |=
            snapshot.runs.len() != runs_before || snapshot.answers.len() != answers_before;
        // Keep complete identities/rows; shrinking a field could change its meaning.
        while snapshot.validate().is_err() || snapshot.encode_frame(true, true).is_err() {
            snapshot.truncated = true;
            if snapshot.answers.pop().is_some()
                || snapshot.runs.pop().is_some()
                || snapshot.approvals.pop().is_some()
                || snapshot.connections.pop().is_some()
            {
                continue;
            }
            break;
        }
        snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn registered_run(target: &super::super::Target) -> CloudAgent {
        let mut bridge = CloudAgent::memory();
        bridge.server = Some(agent_mcp::Server::start(0, "", || {}).unwrap());
        bridge.set_targets(vec![target.clone()]);
        bridge.share(target, true);
        let auth = &bridge.server.as_ref().unwrap().auth;
        let header = format!("Bearer {}", auth.token_for_user().as_str());
        let (reply, receiver) = std::sync::mpsc::sync_channel(1);
        let request = agent_mcp::Request { deferred: None,
            epoch: auth.authenticate(&header, now()).unwrap(),
            principal: auth.authenticate_principal(&header, now()).unwrap(),
            access_key: None,
            input_scope: true,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(3),
            tool: "start_run".into(),
            args: serde_json::json!({"session_id":target.id,"generation":target.generation,"run_id":"observed-run","title":"Observed status"}),
            reply,
        };
        assert!(
            bridge
                .prepare_management(&request, Some(target))
                .unwrap()
                .is_none()
        );
        bridge.drain_history(|_, _| panic!("run registration has no terminal effect"));
        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap()
                .is_ok()
        );
        bridge
    }
    #[test]
    fn local_runtime_status_is_independent_of_model_identity() {
        let mut target = super::super::Target::fixture("observed-session", "generation-original");
        target.agent_line = Some("Codex · gpt-6.1-sol · xhigh".into());
        target.known_ai = true;
        target.local_agent_present = true;
        target.local_status_observed_at = Some(123);
        let mut bridge = registered_run(&target);
        for (status, expected) in [
            (Some(runtime::SessionStatus::Running), Some("running")),
            (Some(runtime::SessionStatus::Waiting), Some("waiting")),
            (
                Some(runtime::SessionStatus::NeedsApproval),
                Some("needs_approval"),
            ),
            (Some(runtime::SessionStatus::Idle), Some("idle")),
            (Some(runtime::SessionStatus::Error), Some("error")),
            (Some(runtime::SessionStatus::Done), Some("done")),
            (None, None),
        ] {
            target.local_status = status;
            bridge.set_targets(vec![target.clone()]);
            let snapshot = bridge.cloud_snapshot();
            assert_eq!(
                snapshot.runs[0].local_status.as_deref(),
                expected,
                "{status:?}"
            );
            assert_eq!(snapshot.runs[0].agent_line, target.agent_line);
            assert_eq!(
                snapshot.runs[0].local_status_observed_at,
                expected.map(|_| 123)
            );
            assert_eq!(
                snapshot.runs[0].state, "queued",
                "local state cannot rewrite the cloud report"
            );
        }
        target.live = false;
        bridge.set_targets(vec![target.clone()]);
        assert_eq!(
            bridge.cloud_snapshot().runs[0].local_status.as_deref(),
            Some("exited")
        );
        target.live = true;
        target.known_ai = false;
        target.local_agent_present = false;
        target.agent_line = None;
        target.local_status = Some(runtime::SessionStatus::Idle);
        bridge.set_targets(vec![target.clone()]);
        assert_eq!(
            bridge.cloud_snapshot().runs[0].local_status.as_deref(),
            Some("shell")
        );
        target.known_ai = true;
        bridge.set_targets(vec![target]);
        assert_eq!(
            bridge.cloud_snapshot().runs[0].local_status.as_deref(),
            Some("shell"),
            "safety classification cannot keep a disappeared agent visually idle"
        );
    }
    #[test]
    fn app_operator_mailbox_rechecks_the_origin_after_queue_admission() {
        let bridge = CloudAgent::memory();
        let core = web_remote::session_core::SessionCore::spawn(None);
        core.set_cloud_control_sink(bridge.operator_sink(egui::Context::default()));
        let policy = CloudControlPolicy {
            authorization_epoch: [1; 16],
            permissions: web_remote::relay::contract::RelayPermissions::new(true, true, true),
            expires_at: None,
        };
        let origin = core.dashboard().cloud_control_origin(
            CloudControlOrigin::DirectWeb { connection_id: 1 },
            policy,
            Arc::new(move || Some(policy)),
        );
        let intent = CloudControlIntent::ApproveConnection {
            approval_id: "pending-approval".into(),
            allow: true,
        };
        assert!(
            core.dashboard()
                .request_cloud_control(&origin, intent.clone())
        );
        let request = bridge.next_operator_intent().unwrap();
        drop(origin);
        assert!(
            core.authorize_cloud_control(request).is_none(),
            "revoked origin must not execute a queued approval"
        );
        let fresh = core.dashboard().cloud_control_origin(
            CloudControlOrigin::DirectWeb { connection_id: 2 },
            policy,
            Arc::new(move || Some(policy)),
        );
        assert!(
            core.dashboard()
                .request_cloud_control(&fresh, intent.clone())
        );
        assert_eq!(
            core.authorize_cloud_control(bridge.next_operator_intent().unwrap()),
            Some(intent)
        );
    }
    #[test]
    fn last_activity_uses_the_newest_observation_regardless_of_kind() {
        let mut info = agent_mcp::ConnectionInfo {
            principal: agent_mcp::Principal {
                id: "independent".into(),
                name: "Bot".into(),
                auth_kind: AuthKind::ManualBearer,
                input_scope: false,
                authenticated_at: 30,
                expires_at: 300,
            },
            last_authenticated_at: Some(30),
            last_initialize_at: Some(20),
            last_tool_success_at: Some(10),
            state: ConnectionState::Active,
        };
        assert_eq!(last_connection_activity(&info), Some(30));
        info.last_initialize_at = Some(40);
        assert_eq!(last_connection_activity(&info), Some(40));
        info.last_tool_success_at = Some(50);
        assert_eq!(last_connection_activity(&info), Some(50));
    }
}
