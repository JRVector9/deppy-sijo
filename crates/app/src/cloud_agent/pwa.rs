//! One redacted projection for native results and authenticated mobile operators.
use super::CloudAgent;
use agent_mcp::{AuthKind, ConnectionState, RunState, now};
use std::sync::Arc;
use web_remote::cloud::*;

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
        snapshot.runs = self
            .cloud_runs()
            .iter()
            .take(MAX_CLOUD_ROWS)
            .map(|run| CloudRunView {
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
                local_status: self
                    .targets
                    .iter()
                    .find(|t| t.id == run.session && t.generation == run.generation)
                    .and_then(|t| t.agent_line.as_deref())
                    .map(|line| {
                        let mut text = self.redact(line);
                        super::truncate_utf8(&mut text, 64);
                        text
                    }),
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
                CloudAnswerView {
                    id: agent_mcp::operation_key(connection, &record.id)
                        .unwrap_or_else(|_| record.id.clone()),
                    connection_id: connection.into(),
                    connection_name: infos
                        .iter()
                        .find(|i| i.principal.id == connection)
                        .map(|i| self.redact(&i.principal.name))
                        .unwrap_or_else(|| connection.into()),
                    workspace_id: record.workspace.clone(),
                    session_id: record.session.clone(),
                    generation: record
                        .generation
                        .clone()
                        .unwrap_or_else(|| "unknown".into()),
                    title: self
                        .targets
                        .iter()
                        .find(|t| t.id == record.session)
                        .map(|t| self.redact(&t.title))
                        .unwrap_or_else(|| record.session.clone()),
                    body: self.redact(&record.message),
                    run_id: record.run_id.clone(),
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
        snapshot.truncated |= self
            .cloud_runs()
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
