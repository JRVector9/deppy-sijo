//! Operator-only bounded cloud work projection and App-owned intents.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const MAX_CLOUD_ROWS: usize = 100;
pub const MAX_CLOUD_TEXT_BYTES: usize = 32 * 1024;
pub const MAX_CLOUD_SNAPSHOT_BYTES: usize = 1024 * 1024;

/// Public projection only. The App must redact display text before publication.
/// No credential, token fingerprint, or arbitrary JSON field exists in this DTO.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudSnapshot {
    pub connections: Vec<CloudConnectionView>,
    pub runs: Vec<CloudRunView>,
    pub answers: Vec<CloudAnswerView>,
    pub approvals: Vec<CloudApprovalView>,
    pub captured_at: i64,
    pub truncated: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudConnectionView {
    pub id: String,
    pub name: String,
    pub auth_kind: String,
    pub state: String,
    pub input_scope: String,
    pub last_activity_at: Option<i64>,
    pub expires_at: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudRunView {
    pub id: String,
    pub connection_id: String,
    pub connection_name: String,
    pub workspace_id: String,
    pub session_id: String,
    pub generation: String,
    pub title: String,
    pub state: String,
    pub message: String,
    pub progress: Option<u8>,
    pub local_status: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub stale: bool,
    pub result: Option<String>,
    pub artifacts: Vec<CloudArtifactView>,
    pub cancel_requested_at: Option<i64>,
    pub cancel_acknowledged_at: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudAnswerView {
    pub id: String,
    pub connection_id: String,
    pub connection_name: String,
    pub workspace_id: String,
    pub session_id: String,
    pub generation: String,
    pub title: String,
    pub body: String,
    pub run_id: Option<String>,
    pub created_at: i64,
    pub artifacts: Vec<CloudArtifactView>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudApprovalView {
    pub id: String,
    pub connection_name: String,
    pub scope: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudArtifactView {
    pub label: String,
    pub uri: String,
}

/// Nonblocking App-owned bounded mailbox. True means queue admission only.
pub type CloudControlSink = Arc<dyn Fn(CloudControlIntent) -> bool + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum CloudControlIntent {
    RevokeConnection {
        connection_id: String,
    },
    TakeControl {
        session_id: String,
        generation: String,
    },
    CancelRun {
        connection_id: String,
        run_id: String,
    },
    Navigate {
        workspace_id: String,
        session_id: String,
    },
    ApproveConnection {
        approval_id: String,
        allow: bool,
    },
}

impl CloudControlIntent {
    pub fn validate(&self) -> bool {
        match self {
            Self::RevokeConnection { connection_id } => valid_identity(connection_id),
            Self::TakeControl {
                session_id,
                generation,
            } => valid_identity(session_id) && valid_identity(generation),
            Self::CancelRun {
                connection_id,
                run_id,
            } => valid_identity(connection_id) && valid_identity(run_id),
            Self::Navigate {
                workspace_id,
                session_id,
            } => valid_identity(workspace_id) && valid_identity(session_id),
            Self::ApproveConnection { approval_id, .. } => valid_identity(approval_id),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloudSnapshotError {
    TooManyRows,
    FieldTooLarge,
    InvalidIdentity,
    InvalidValue,
    InvalidArtifact,
    PayloadTooLarge,
}
impl std::fmt::Display for CloudSnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cloud snapshot rejected: {self:?}")
    }
}
impl std::error::Error for CloudSnapshotError {}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}
fn identities(values: &[&str]) -> Result<(), CloudSnapshotError> {
    if values.iter().all(|value| valid_identity(value)) {
        Ok(())
    } else {
        Err(CloudSnapshotError::InvalidIdentity)
    }
}
fn fields(values: &[&str], max: usize) -> Result<(), CloudSnapshotError> {
    if values.iter().all(|value| value.len() <= max) {
        Ok(())
    } else {
        Err(CloudSnapshotError::FieldTooLarge)
    }
}
fn artifacts(values: &[CloudArtifactView]) -> Result<(), CloudSnapshotError> {
    if values.len() > 16 {
        return Err(CloudSnapshotError::TooManyRows);
    }
    for artifact in values {
        fields(&[&artifact.label], 512)?;
        fields(&[&artifact.uri], 2048)?;
        let authority = artifact
            .uri
            .split_once("://")
            .filter(|(scheme, _)| {
                scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http")
            })
            .and_then(|(_, rest)| rest.split(['/', '?', '#']).next());
        if authority.is_none_or(|value| value.is_empty() || value.contains('@'))
            || artifact
                .uri
                .chars()
                .any(|value| value.is_control() || value.is_whitespace() || value == '\\')
        {
            return Err(CloudSnapshotError::InvalidArtifact);
        }
    }
    Ok(())
}

impl CloudSnapshot {
    pub fn validate(&self) -> Result<(), CloudSnapshotError> {
        if [
            self.connections.len(),
            self.runs.len(),
            self.answers.len(),
            self.approvals.len(),
        ]
        .into_iter()
        .any(|count| count > MAX_CLOUD_ROWS)
        {
            return Err(CloudSnapshotError::TooManyRows);
        }
        for connection in &self.connections {
            identities(&[&connection.id])?;
            fields(&[&connection.name], 512)?;
            fields(
                &[
                    &connection.auth_kind,
                    &connection.state,
                    &connection.input_scope,
                ],
                64,
            )?;
        }
        for run in &self.runs {
            identities(&[
                &run.id,
                &run.connection_id,
                &run.workspace_id,
                &run.session_id,
                &run.generation,
            ])?;
            fields(&[&run.title, &run.connection_name], 512)?;
            fields(
                &[&run.message, run.result.as_deref().unwrap_or_default()],
                MAX_CLOUD_TEXT_BYTES,
            )?;
            fields(&[run.local_status.as_deref().unwrap_or_default()], 64)?;
            if !matches!(
                run.state.as_str(),
                "queued" | "running" | "waiting" | "completed" | "failed" | "cancelled" | "unknown"
            ) || run.progress.is_some_and(|progress| progress > 100)
            {
                return Err(CloudSnapshotError::InvalidValue);
            }
            artifacts(&run.artifacts)?;
        }
        for answer in &self.answers {
            identities(&[
                &answer.id,
                &answer.connection_id,
                &answer.workspace_id,
                &answer.session_id,
                &answer.generation,
            ])?;
            if let Some(id) = &answer.run_id {
                identities(&[id])?;
            }
            fields(&[&answer.title, &answer.connection_name], 512)?;
            fields(&[&answer.body], MAX_CLOUD_TEXT_BYTES)?;
            artifacts(&answer.artifacts)?;
        }
        for approval in &self.approvals {
            identities(&[&approval.id])?;
            fields(&[&approval.connection_name, &approval.scope], 512)?;
        }
        Ok(())
    }

    /// Encode one permission-scoped frame; failure leaves the published slot intact.
    pub fn encode_frame(
        &self,
        can_control: bool,
        can_approve: bool,
    ) -> Result<String, CloudSnapshotError> {
        self.validate()?;
        let mut snapshot = self.clone();
        if !can_approve {
            snapshot.approvals.clear();
        }
        let frame = crate::protocol::ServerMsg::CloudSnapshot {
            snapshot,
            can_control,
            can_approve,
        }
        .encode();
        if frame.len() > MAX_CLOUD_SNAPSHOT_BYTES {
            return Err(CloudSnapshotError::PayloadTooLarge);
        }
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> CloudRunView {
        CloudRunView {
            id: "run-a".into(),
            connection_id: "actor-a".into(),
            workspace_id: "workspace-original".into(),
            session_id: "session-original".into(),
            generation: "generation-original".into(),
            title: "<script>alert(1)</script> 한글".into(),
            state: "unknown".into(),
            ..Default::default()
        }
    }

    #[test]
    fn cloud_projection_retains_original_identity_and_exposes_no_auth_fields() {
        let snapshot = CloudSnapshot {
            runs: vec![run()],
            ..Default::default()
        };
        let frame = snapshot.encode_frame(true, false).unwrap();
        let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(
            value["snapshot"]["runs"][0]["session_id"],
            "session-original"
        );
        assert_eq!(value["snapshot"]["runs"][0]["connection_id"], "actor-a");
        assert_eq!(
            value["snapshot"]["runs"][0]["title"],
            "<script>alert(1)</script> 한글"
        );
        for credential in ["token", "fingerprint", "bearer", "client_secret"] {
            assert!(!frame.contains(&format!("\"{credential}\"")));
        }
    }

    #[test]
    fn cloud_projection_rejects_excess_rows_and_oversize_without_truncating_identity() {
        let mut snapshot = CloudSnapshot {
            runs: vec![run(); MAX_CLOUD_ROWS + 1],
            ..Default::default()
        };
        assert_eq!(snapshot.validate(), Err(CloudSnapshotError::TooManyRows));
        snapshot.runs.truncate(MAX_CLOUD_ROWS);
        snapshot.runs[0].result = Some("x".repeat(MAX_CLOUD_TEXT_BYTES + 1));
        assert_eq!(snapshot.validate(), Err(CloudSnapshotError::FieldTooLarge));
        for entry in &mut snapshot.runs {
            entry.result = Some("\0".repeat(MAX_CLOUD_TEXT_BYTES));
        }
        assert_eq!(
            snapshot.encode_frame(true, true),
            Err(CloudSnapshotError::PayloadTooLarge)
        );
    }

    #[test]
    fn cloud_controls_are_bounded_explicit_intents_without_input_text() {
        let intent = CloudControlIntent::CancelRun {
            connection_id: "actor-a".into(),
            run_id: "run-a".into(),
        };
        assert!(intent.validate());
        assert!(
            !CloudControlIntent::TakeControl {
                session_id: "session".into(),
                generation: String::new()
            }
            .validate()
        );
        assert!(
            !CloudControlIntent::Navigate {
                workspace_id: "x".repeat(129),
                session_id: "original".into()
            }
            .validate()
        );
        assert!(
            serde_json::from_str::<CloudControlIntent>(
                r#"{"action":"terminal_input","text":"rm -rf"}"#
            )
            .is_err()
        );
        assert!(serde_json::from_str::<CloudControlIntent>(r#"{"action":"cancel_run","connection_id":"actor-a","run_id":"run-a","text":"unexpected"}"#).is_err());
    }

    #[test]
    fn cloud_artifacts_preserve_safe_links_and_reject_embedded_credentials() {
        let mut run = run();
        run.artifacts.push(CloudArtifactView {
            label: "proof".into(),
            uri: "HtTpS://example.com/proof".into(),
        });
        let mut snapshot = CloudSnapshot {
            runs: vec![run],
            ..Default::default()
        };
        assert_eq!(snapshot.validate(), Ok(()));
        for uri in [
            "https://user:password@example.com/proof",
            "javascript:alert(1)",
            "https://example.com\\@other.com/proof",
        ] {
            snapshot.runs[0].artifacts[0].uri = uri.into();
            assert_eq!(
                snapshot.validate(),
                Err(CloudSnapshotError::InvalidArtifact)
            );
        }
    }

    #[test]
    fn failed_publication_keeps_the_last_authoritative_snapshot() {
        let core = crate::session_core::SessionCore::spawn(None);
        core.publish_cloud_snapshot(CloudSnapshot {
            runs: vec![run()],
            ..Default::default()
        })
        .unwrap();
        let (version, original) = core.dashboard().cloud_snapshot_if_newer(0).unwrap();
        let invalid = CloudSnapshot {
            runs: vec![run(); MAX_CLOUD_ROWS + 1],
            ..Default::default()
        };
        assert_eq!(
            core.publish_cloud_snapshot(invalid),
            Err(CloudSnapshotError::TooManyRows)
        );
        assert!(core.dashboard().cloud_snapshot_if_newer(version).is_none());
        assert_eq!(
            core.dashboard().cloud_snapshot_if_newer(0).unwrap().1,
            original
        );
        core.shutdown();
    }

    #[test]
    fn read_only_projection_omits_connection_approvals() {
        let core = crate::session_core::SessionCore::spawn(None);
        core.publish_cloud_snapshot(CloudSnapshot {
            approvals: vec![CloudApprovalView {
                id: "private-approval".into(),
                ..Default::default()
            }],
            ..Default::default()
        })
        .unwrap();
        let (_, frame) = core
            .dashboard()
            .cloud_snapshot_for_permissions(0, crate::relay::contract::RelayPermissions::default())
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(value["snapshot"]["approvals"], serde_json::json!([]));
        assert_eq!(value["can_control"], false);
        assert_eq!(value["can_approve"], false);
        core.shutdown();
    }
}
