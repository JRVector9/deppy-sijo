//! Operator-only bounded cloud work projection and App-owned intents.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::relay::contract::{ConnectionId, DeviceId, RelayAction, RelayPermissions};

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
    /// Provider/model identity, separate from the observed execution state.
    pub agent_line: Option<String>,
    pub local_status: Option<String>,
    /// Source observation time; absent when the App has no reliable timestamp.
    pub local_status_observed_at: Option<i64>,
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
    /// Safe display text; the original run ID remains an opaque routing reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_label: Option<String>,
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

impl CloudArtifactView {
    pub fn validate(&self) -> Result<(), CloudSnapshotError> {
        artifacts(std::slice::from_ref(self))
    }
}

/// Nonblocking App-owned bounded mailbox. True means queue admission only.
/// The App must consume each request through `SessionCore::authorize_cloud_control`
/// immediately before applying its effect.
pub type CloudControlSink = Arc<dyn Fn(CloudControlRequest) -> bool + Send + Sync>;

/// Actual App-observed navigation outcome, independent of queue admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudNavigationOutcome {
    Opened,
    Unavailable,
    TimedOut,
    Superseded,
    Rejected,
}

/// Transport-owned nonblocking reply callback; request IDs never grant authority.
pub type CloudNavigationCallback = Arc<dyn Fn(CloudNavigationOutcome) + Send + Sync>;

pub struct CloudNavigationReply {
    authority: Arc<CloudControlAuthority>,
    callback: CloudNavigationCallback,
    deadline: Instant,
}

impl CloudNavigationReply {
    pub fn finish(self, outcome: CloudNavigationOutcome) {
        // Completion may outlive the five-second admission deadline, but never
        // the original socket/principal, policy epoch, expiry or core lifetime.
        if self.fence_valid()
            && (self.authority.revalidate)() == Some(self.authority.policy)
            && self.fence_valid()
        {
            (self.callback)(outcome);
        }
    }

    fn fence_valid(&self) -> bool {
        let authority = &self.authority;
        authority.active.load(Ordering::SeqCst)
            && (authority.core_alive)()
            && Instant::now() < self.deadline
            && authority.policy.permissions.allows(RelayAction::View)
            && authority.policy.permissions.allows(RelayAction::Switch)
            && authority
                .policy
                .expires_at
                .is_none_or(|expiry| cloud_unix_seconds() < expiry)
    }
}

/// Current trusted authority, obtained from the transport's authenticated principal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CloudControlPolicy {
    pub authorization_epoch: [u8; 16],
    pub permissions: RelayPermissions,
    pub expires_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloudControlOrigin {
    DirectWeb {
        connection_id: u64,
    },
    RelayDevice {
        device_id: DeviceId,
        channel_id: ConnectionId,
    },
}

/// Returns None when the original authenticated principal is no longer current.
pub type CloudControlRevalidator = Arc<dyn Fn() -> Option<CloudControlPolicy> + Send + Sync>;

struct CloudControlAuthority {
    core_id: String,
    origin: CloudControlOrigin,
    policy: CloudControlPolicy,
    active: AtomicBool,
    core_alive: Arc<dyn Fn() -> bool + Send + Sync>,
    revalidate: CloudControlRevalidator,
}

/// One authenticated transport activation. Replacing or dropping this guard
/// permanently invalidates every request queued from that activation.
pub struct CloudControlOriginGuard {
    authority: Arc<CloudControlAuthority>,
}

impl CloudControlOriginGuard {
    pub(crate) fn new(
        core_id: String,
        origin: CloudControlOrigin,
        policy: CloudControlPolicy,
        core_alive: Arc<dyn Fn() -> bool + Send + Sync>,
        revalidate: CloudControlRevalidator,
    ) -> Self {
        Self {
            authority: Arc::new(CloudControlAuthority {
                core_id,
                origin,
                policy,
                active: AtomicBool::new(true),
                core_alive,
                revalidate,
            }),
        }
    }

    pub fn revoke(&self) {
        self.authority.active.store(false, Ordering::SeqCst);
    }

    pub(crate) fn issue(
        &self,
        core_id: &str,
        intent: CloudControlIntent,
    ) -> Option<CloudControlRequest> {
        if !intent.validate() || self.authority.core_id != core_id {
            return None;
        }
        let request = CloudControlRequest {
            intent,
            authority: self.authority.clone(),
            deadline: Instant::now() + Duration::from_secs(5),
            navigation_callback: None,
        };
        request
            .authorized_at(core_id, Instant::now(), cloud_unix_seconds())
            .then_some(request)
    }
}

impl Drop for CloudControlOriginGuard {
    fn drop(&mut self) {
        self.revoke();
    }
}

/// Trusted, non-serializable, single-use authority envelope. Clients send only
/// an intent; the authenticated transport constructs this private envelope.
pub struct CloudControlRequest {
    intent: CloudControlIntent,
    authority: Arc<CloudControlAuthority>,
    deadline: Instant,
    navigation_callback: Option<CloudNavigationCallback>,
}

impl std::fmt::Debug for CloudControlRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudControlRequest")
            .field("intent", &self.intent)
            .field("origin", &self.authority.origin)
            .finish_non_exhaustive()
    }
}

impl CloudControlRequest {
    pub(crate) fn set_navigation_callback(&mut self, callback: CloudNavigationCallback) {
        if matches!(self.intent, CloudControlIntent::Navigate { .. }) {
            self.navigation_callback = Some(callback);
        }
    }

    /// Take once before consuming the authority envelope. This cannot authorize an effect.
    pub fn take_navigation_reply(&mut self) -> Option<CloudNavigationReply> {
        self.navigation_callback
            .take()
            .map(|callback| CloudNavigationReply {
                authority: self.authority.clone(),
                callback,
                deadline: self.deadline + Duration::from_secs(35),
            })
    }

    pub fn intent(&self) -> &CloudControlIntent {
        &self.intent
    }
    pub fn origin(&self) -> &CloudControlOrigin {
        &self.authority.origin
    }

    pub(crate) fn authorize(self, core_id: &str) -> Option<CloudControlIntent> {
        self.authorized_at(core_id, Instant::now(), cloud_unix_seconds())
            .then_some(self.intent)
    }

    fn authorized_at(&self, core_id: &str, now: Instant, unix_seconds: u64) -> bool {
        if !self.fence_valid(core_id, now, unix_seconds) {
            return false;
        }
        if (self.authority.revalidate)() != Some(self.authority.policy) {
            return false;
        }
        // The callback can take time or revoke this activation/core. Sample
        // both clocks and all local fences again after it returns.
        self.fence_valid(core_id, now, unix_seconds)
    }

    fn fence_valid(&self, core_id: &str, now: Instant, unix_seconds: u64) -> bool {
        let authority = &self.authority;
        let required = match &self.intent {
            CloudControlIntent::Navigate { .. } => RelayAction::Switch,
            CloudControlIntent::ApproveConnection { .. } => RelayAction::Approval,
            CloudControlIntent::CommandCenter { intent, .. } => {
                crate::work::required_action(intent)
            }
            _ => RelayAction::Input,
        };
        authority.core_id == core_id
            && (authority.core_alive)()
            && authority.active.load(Ordering::SeqCst)
            && self.intent.validate()
            && authority.policy.permissions.allows(RelayAction::View)
            && authority.policy.permissions.allows(required)
            && Instant::now().max(now) < self.deadline
            && authority
                .policy
                .expires_at
                .is_none_or(|expires| cloud_unix_seconds().max(unix_seconds) < expires)
    }
}

fn cloud_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(u64::MAX, |value| value.as_secs())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum CloudControlIntent {
    CommandCenter {
        operation_id: String,
        intent: serde_json::Value,
    },
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
            Self::CommandCenter {
                operation_id,
                intent,
            } => {
                uuid::Uuid::parse_str(operation_id).is_ok() && crate::work::validate_intent(intent)
            }
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
            fields(&[run.agent_line.as_deref().unwrap_or_default()], 512)?;
            if run.local_status.as_deref().is_some_and(|state| {
                !matches!(
                    state,
                    "running"
                        | "waiting"
                        | "needs_approval"
                        | "idle"
                        | "error"
                        | "done"
                        | "exited"
                        | "shell"
                )
            }) || run
                .local_status_observed_at
                .is_some_and(|at| at < 0 || run.local_status.is_none())
            {
                return Err(CloudSnapshotError::InvalidValue);
            }
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
            fields(&[answer.run_label.as_deref().unwrap_or_default()], 512)?;
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

    fn control_policy() -> CloudControlPolicy {
        CloudControlPolicy {
            authorization_epoch: [3; 16],
            permissions: crate::relay::contract::RelayPermissions::new(true, true, true),
            expires_at: None,
        }
    }
    fn control_intent() -> CloudControlIntent {
        CloudControlIntent::RevokeConnection {
            connection_id: "actor-original".into(),
        }
    }
    fn queued_control(
        core: &crate::session_core::SessionCore,
        guard: &CloudControlOriginGuard,
    ) -> CloudControlRequest {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        core.set_cloud_control_sink(Arc::new(move |request| tx.try_send(request).is_ok()));
        assert!(
            core.dashboard()
                .request_cloud_control(guard, control_intent())
        );
        rx.try_recv().unwrap()
    }

    #[test]
    fn local_execution_status_rejects_model_identity() {
        let mut snapshot = CloudSnapshot {
            runs: vec![run()],
            ..Default::default()
        };
        snapshot.runs[0].local_status = Some("Codex · gpt-6.1-sol · xhigh".into());
        assert_eq!(snapshot.validate(), Err(CloudSnapshotError::InvalidValue));
        for state in [
            "running",
            "waiting",
            "needs_approval",
            "idle",
            "error",
            "done",
            "exited",
            "shell",
        ] {
            snapshot.runs[0].local_status = Some(state.into());
            assert_eq!(snapshot.validate(), Ok(()), "{state}");
        }
    }

    #[test]
    fn navigation_reply_reports_observed_completion_and_fences_old_origin() {
        let core = crate::session_core::SessionCore::spawn(None);
        let current = Arc::new(std::sync::Mutex::new(Some(control_policy())));
        let policy = current.clone();
        let guard = core.dashboard().cloud_control_origin(
            CloudControlOrigin::DirectWeb { connection_id: 1 },
            control_policy(),
            Arc::new(move || *policy.lock().unwrap()),
        );
        let (request_tx, request_rx) = std::sync::mpsc::sync_channel(4);
        core.set_cloud_control_sink(Arc::new(move |request| {
            request_tx.try_send(request).is_ok()
        }));
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        for outcome in [
            CloudNavigationOutcome::Opened,
            CloudNavigationOutcome::Unavailable,
            CloudNavigationOutcome::TimedOut,
            CloudNavigationOutcome::Superseded,
            CloudNavigationOutcome::Rejected,
        ] {
            let tx = tx.clone();
            assert!(
                core.dashboard()
                    .request_cloud_control_with_navigation_reply(
                        &guard,
                        CloudControlIntent::Navigate {
                            workspace_id: "workspace".into(),
                            session_id: "session".into()
                        },
                        Arc::new(move |outcome| {
                            let _ = tx.try_send(outcome);
                        }),
                    )
            );
            let mut request = request_rx.recv().unwrap();
            let reply = request.take_navigation_reply().unwrap();
            assert!(request.take_navigation_reply().is_none());
            assert!(
                rx.try_recv().is_err(),
                "admission must not report completion"
            );
            assert!(core.authorize_cloud_control(request).is_some());
            reply.finish(outcome);
            assert_eq!(
                rx.recv_timeout(Duration::from_millis(100)).unwrap(),
                outcome
            );
        }
        for invalidation in 0..4 {
            *current.lock().unwrap() = Some(control_policy());
            let tx = tx.clone();
            assert!(
                core.dashboard()
                    .request_cloud_control_with_navigation_reply(
                        &guard,
                        CloudControlIntent::Navigate {
                            workspace_id: "workspace".into(),
                            session_id: "session".into()
                        },
                        Arc::new(move |outcome| {
                            let _ = tx.try_send(outcome);
                        }),
                    )
            );
            let mut request = request_rx.recv().unwrap();
            let mut reply = request.take_navigation_reply().unwrap();
            assert!(core.authorize_cloud_control(request).is_some());
            match invalidation {
                0 => {
                    current
                        .lock()
                        .unwrap()
                        .as_mut()
                        .unwrap()
                        .authorization_epoch[15] += 1
                }
                1 => {
                    current.lock().unwrap().as_mut().unwrap().permissions =
                        crate::relay::contract::RelayPermissions::default()
                }
                2 => reply.deadline = Instant::now(),
                _ => guard.revoke(),
            }
            reply.finish(CloudNavigationOutcome::Opened);
            assert!(
                rx.try_recv().is_err(),
                "obsolete activation received a result"
            );
        }
        core.shutdown();
    }

    #[test]
    fn cloud_control_mailbox_rechecks_revocation_epoch_and_permissions_at_drain() {
        let core = crate::session_core::SessionCore::spawn(None);
        let current = Arc::new(std::sync::Mutex::new(Some(control_policy())));
        let policy = current.clone();
        let guard = core.dashboard().cloud_control_origin(
            CloudControlOrigin::DirectWeb { connection_id: 1 },
            control_policy(),
            Arc::new(move || *policy.lock().unwrap()),
        );
        let current_request = queued_control(&core, &guard);
        assert_eq!(
            core.authorize_cloud_control(current_request),
            Some(control_intent())
        );
        let request = queued_control(&core, &guard);
        *current.lock().unwrap() = None;
        assert!(
            core.authorize_cloud_control(request).is_none(),
            "revoked origin kept a queued effect"
        );
        *current.lock().unwrap() = Some(control_policy());
        let request = queued_control(&core, &guard);
        // A change in the second half must invalidate the full relay epoch.
        current
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .authorization_epoch[15] += 1;
        assert!(
            core.authorize_cloud_control(request).is_none(),
            "renewed auth admitted an old effect"
        );
        *current.lock().unwrap() = Some(control_policy());
        let request = queued_control(&core, &guard);
        current.lock().unwrap().as_mut().unwrap().permissions =
            crate::relay::contract::RelayPermissions::default();
        assert!(
            core.authorize_cloud_control(request).is_none(),
            "permission downgrade kept a queued effect"
        );
        core.shutdown();
    }

    #[test]
    fn cloud_control_mailbox_rejects_dropped_origin_and_replaced_core() {
        let core = crate::session_core::SessionCore::spawn(None);
        let guard = core.dashboard().cloud_control_origin(
            CloudControlOrigin::DirectWeb { connection_id: 1 },
            control_policy(),
            Arc::new(|| Some(control_policy())),
        );
        let request = queued_control(&core, &guard);
        drop(guard);
        assert!(core.authorize_cloud_control(request).is_none());
        let guard = core.dashboard().cloud_control_origin(
            CloudControlOrigin::DirectWeb { connection_id: 1 },
            control_policy(),
            Arc::new(|| Some(control_policy())),
        );
        let request = queued_control(&core, &guard);
        let next_core = crate::session_core::SessionCore::spawn(None);
        assert!(next_core.authorize_cloud_control(request).is_none());
        let request = queued_control(&core, &guard);
        core.shutdown();
        assert!(core.authorize_cloud_control(request).is_none());
        next_core.shutdown();
    }

    #[test]
    fn cloud_control_mailbox_honors_deadline_and_checks_after_slow_revalidation() {
        let core = crate::session_core::SessionCore::spawn(None);
        let guard = core.dashboard().cloud_control_origin(
            CloudControlOrigin::DirectWeb { connection_id: 1 },
            control_policy(),
            Arc::new(|| Some(control_policy())),
        );
        let mut request = queued_control(&core, &guard);
        request.deadline = std::time::Instant::now();
        assert!(core.authorize_cloud_control(request).is_none());
        let revoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let callback_revoked = revoked.clone();
        let guard = core.dashboard().cloud_control_origin(
            CloudControlOrigin::DirectWeb { connection_id: 2 },
            control_policy(),
            Arc::new(move || {
                if callback_revoked.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Some(control_policy())
            }),
        );
        let mut request = queued_control(&core, &guard);
        request.deadline = std::time::Instant::now() + std::time::Duration::from_millis(5);
        revoked.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            core.authorize_cloud_control(request).is_none(),
            "deadline expired inside authorizer callback"
        );
        core.shutdown();
    }

    #[test]
    fn cloud_control_mailbox_requires_current_origin_permission_and_device_expiry() {
        let core = crate::session_core::SessionCore::spawn(None);
        core.set_cloud_control_sink(Arc::new(|_| true));
        let policy = CloudControlPolicy {
            permissions: crate::relay::contract::RelayPermissions::default(),
            ..control_policy()
        };
        let guard = core.dashboard().cloud_control_origin(
            CloudControlOrigin::DirectWeb { connection_id: 1 },
            policy,
            Arc::new(move || Some(policy)),
        );
        assert!(
            !core
                .dashboard()
                .request_cloud_control(&guard, control_intent())
        );
        let expires = cloud_unix_seconds() + 100;
        let policy = CloudControlPolicy {
            expires_at: Some(expires),
            ..control_policy()
        };
        let guard = core.dashboard().cloud_control_origin(
            CloudControlOrigin::RelayDevice {
                device_id: crate::relay::contract::DeviceId::from_bytes([4; 16]),
                channel_id: crate::relay::contract::ConnectionId::from_bytes([5; 16]),
            },
            policy,
            Arc::new(move || Some(policy)),
        );
        let request = queued_control(&core, &guard);
        assert!(
            !request.authorized_at(
                core.dashboard().cloud_control_core_id(),
                std::time::Instant::now(),
                expires
            ),
            "expired device kept a queued effect"
        );
        core.shutdown();
    }

    #[test]
    fn cloud_control_mailbox_rechecks_guard_and_core_after_revalidation() {
        let core = crate::session_core::SessionCore::spawn(None);
        let armed = Arc::new(AtomicBool::new(false));
        let arm = armed.clone();
        let guard: Arc<CloudControlOriginGuard> =
            Arc::new_cyclic(|weak: &std::sync::Weak<CloudControlOriginGuard>| {
                let weak = weak.clone();
                core.dashboard().cloud_control_origin(
                    CloudControlOrigin::DirectWeb { connection_id: 1 },
                    control_policy(),
                    Arc::new(move || {
                        if arm.load(Ordering::SeqCst) {
                            weak.upgrade().unwrap().revoke();
                        }
                        Some(control_policy())
                    }),
                )
            });
        let request = queued_control(&core, &guard);
        armed.store(true, Ordering::SeqCst);
        assert!(
            core.authorize_cloud_control(request).is_none(),
            "guard revoked inside callback still authorized"
        );

        let armed = Arc::new(AtomicBool::new(false));
        let arm = armed.clone();
        let shutdown = core.clone();
        let guard = core.dashboard().cloud_control_origin(
            CloudControlOrigin::DirectWeb { connection_id: 2 },
            control_policy(),
            Arc::new(move || {
                if arm.load(Ordering::SeqCst) {
                    shutdown.shutdown();
                }
                Some(control_policy())
            }),
        );
        let request = queued_control(&core, &guard);
        armed.store(true, Ordering::SeqCst);
        assert!(
            core.authorize_cloud_control(request).is_none(),
            "core stopped inside callback still authorized"
        );
    }

    #[test]
    fn cloud_control_mailbox_requires_exact_permissions_and_intent_scope() {
        use crate::relay::contract::RelayPermissions;
        let core = crate::session_core::SessionCore::spawn(None);
        core.set_cloud_control_sink(Arc::new(|_| true));
        for (permissions, navigate, approve, mutate) in [
            (RelayPermissions::default(), false, false, false),
            (
                RelayPermissions::new(false, true, true),
                false,
                false,
                false,
            ),
            (RelayPermissions::new(true, false, true), false, true, false),
            (RelayPermissions::new(true, true, false), true, false, true),
        ] {
            let policy = CloudControlPolicy {
                permissions,
                ..control_policy()
            };
            let guard = core.dashboard().cloud_control_origin(
                CloudControlOrigin::DirectWeb { connection_id: 1 },
                policy,
                Arc::new(move || Some(policy)),
            );
            for (intent, expected) in [
                (
                    CloudControlIntent::Navigate {
                        workspace_id: "w".into(),
                        session_id: "s".into(),
                    },
                    navigate,
                ),
                (
                    CloudControlIntent::ApproveConnection {
                        approval_id: "a".into(),
                        allow: true,
                    },
                    approve,
                ),
                (control_intent(), mutate),
            ] {
                assert_eq!(
                    core.dashboard().request_cloud_control(&guard, intent),
                    expected
                );
            }
        }
        let current = Arc::new(std::sync::Mutex::new(Some(control_policy())));
        let state = current.clone();
        let guard = core.dashboard().cloud_control_origin(
            CloudControlOrigin::DirectWeb { connection_id: 2 },
            control_policy(),
            Arc::new(move || *state.lock().unwrap()),
        );
        let request = queued_control(&core, &guard);
        current.lock().unwrap().as_mut().unwrap().permissions =
            control_policy().permissions.with_upload(true);
        assert!(
            core.authorize_cloud_control(request).is_none(),
            "unrelated permission change reused original authorization"
        );
    }

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
    fn answer_display_label_has_separate_budget_and_legacy_default() {
        let mut snapshot = CloudSnapshot {
            answers: vec![CloudAnswerView {
                id: "answer".into(),
                connection_id: "actor".into(),
                workspace_id: "workspace".into(),
                session_id: "session".into(),
                generation: "generation".into(),
                run_id: Some("original-run".into()),
                run_label: Some("[REDACTED]".repeat(51)),
                ..Default::default()
            }],
            ..Default::default()
        };
        let frame: serde_json::Value =
            serde_json::from_str(&snapshot.encode_frame(false, false).unwrap()).unwrap();
        assert_eq!(frame["snapshot"]["answers"][0]["run_id"], "original-run");
        assert_eq!(
            frame["snapshot"]["answers"][0]["run_label"],
            "[REDACTED]".repeat(51)
        );
        snapshot.answers[0].run_label = Some("가".repeat(171));
        assert_eq!(snapshot.validate(), Err(CloudSnapshotError::FieldTooLarge));
        snapshot.answers[0].run_label = None;
        let legacy = serde_json::to_value(&snapshot.answers[0]).unwrap();
        assert!(legacy.get("run_label").is_none());
        let restored: CloudAnswerView = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.run_label, None);
        assert_eq!(restored.run_id.as_deref(), Some("original-run"));
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
        assert_eq!(snapshot.runs[0].artifacts[0].validate(), Ok(()));
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
            assert_eq!(
                snapshot.runs[0].artifacts[0].validate(),
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
