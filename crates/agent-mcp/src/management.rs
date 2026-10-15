//! Bounded, pure cloud work model. Only the App-owned history worker persists it.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_RUN_BYTES: usize = 32 * 1024;
pub const MAX_RUNS: usize = 1000;
pub const MAX_RUN_PAGE_BYTES: usize = 1024 * 1024;
const RECOVERY_METADATA_BYTES: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationContext {
    pub connection_id: String,
    pub operation_id: String,
    pub run_id: Option<String>,
    pub generation: String,
}
impl OperationContext {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_identity(&self.connection_id)?;
        validate_operation_id(&self.operation_id)?;
        validate_identity(&self.generation)?;
        if let Some(id) = &self.run_id {
            validate_identity(id)?;
        }
        Ok(())
    }
}

pub(crate) fn validate_identity(value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control),
        "invalid_identity"
    );
    Ok(())
}
pub(crate) fn validate_operation_id(id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !id.is_empty()
            && id.len() <= 128
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':')),
        "invalid_operation_id"
    );
    Ok(())
}

/// The legacy bearer retains its original tombstones. All new credential identities
/// use a namespace which legacy claim IDs cannot enter, without exposing identity.
pub fn operation_key(connection_id: &str, external_id: &str) -> anyhow::Result<String> {
    validate_identity(connection_id)?;
    validate_operation_id(external_id)?;
    if connection_id == "manual-legacy" {
        return Ok(external_id.to_owned());
    }
    let hash = Sha256::digest(serde_json::to_vec(&(connection_id, external_id))?);
    let mut key = String::from("@scoped:");
    use std::fmt::Write;
    for byte in hash {
        write!(&mut key, "{byte:02x}")?;
    }
    Ok(key)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
    Unknown,
}
impl RunState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
    pub(crate) fn permits(self, next: Self) -> bool {
        use RunState::*;
        match self {
            Queued => matches!(
                next,
                Queued | Running | Waiting | Failed | Cancelled | Unknown
            ),
            Running | Waiting => matches!(
                next,
                Running | Waiting | Completed | Failed | Cancelled | Unknown
            ),
            Unknown => matches!(next, Unknown | Running | Waiting | Failed | Cancelled),
            Completed | Failed | Cancelled => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressProvenance {
    AgentReported,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactReference {
    pub label: String,
    pub uri: String,
}
impl ArtifactReference {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.label.is_empty()
                && self.label.len() <= 128
                && !self.label.chars().any(char::is_control)
                && self.uri.len() <= 2048,
            "invalid_artifact"
        );
        let uri = url::Url::parse(&self.uri)?;
        anyhow::ensure!(
            matches!(uri.scheme(), "https" | "http")
                && uri.host_str().is_some()
                && uri.username().is_empty()
                && uri.password().is_none(),
            "invalid_artifact_uri"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunActor {
    pub connection_id: String,
    pub workspace: String,
    pub session: String,
    pub generation: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunUpdate {
    pub state: RunState,
    pub progress: Option<u8>,
    pub message: String,
    pub result: Option<String>,
    pub artifacts: Vec<ArtifactReference>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudRun {
    pub id: String,
    pub connection_id: String,
    pub connection_name: String,
    pub workspace: String,
    pub session: String,
    pub generation: String,
    pub title: String,
    pub instruction: String,
    pub state: RunState,
    pub progress: Option<u8>,
    pub progress_provenance: ProgressProvenance,
    pub message: String,
    pub created: i64,
    pub updated: i64,
    pub heartbeat: i64,
    pub result: Option<String>,
    pub artifacts: Vec<ArtifactReference>,
    pub cancel_requested_at: Option<i64>,
    pub cancel_acknowledged_at: Option<i64>,
    pub revision: u64,
}
impl CloudRun {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: &str,
        connection_id: &str,
        connection_name: &str,
        workspace: &str,
        session: &str,
        generation: &str,
        title: &str,
        instruction: &str,
        now: i64,
    ) -> anyhow::Result<Self> {
        let run = Self {
            id: id.into(),
            connection_id: connection_id.into(),
            connection_name: connection_name.into(),
            workspace: workspace.into(),
            session: session.into(),
            generation: generation.into(),
            title: title.into(),
            instruction: instruction.into(),
            state: RunState::Queued,
            progress: None,
            progress_provenance: ProgressProvenance::AgentReported,
            message: String::new(),
            created: now,
            updated: now,
            heartbeat: now,
            result: None,
            artifacts: vec![],
            cancel_requested_at: None,
            cancel_acknowledged_at: None,
            revision: 0,
        };
        run.validate()?;
        run.ensure_recovery_headroom()?;
        Ok(run)
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        for value in [
            &self.id,
            &self.connection_id,
            &self.workspace,
            &self.session,
            &self.generation,
        ] {
            validate_identity(value)?;
        }
        anyhow::ensure!(
            self.connection_name.len() <= 128
                && !self.connection_name.chars().any(char::is_control)
                && !self.title.is_empty()
                && self.title.len() <= 256
                && self.instruction.len() <= 8192
                && self.message.len() <= 4096
                && self
                    .result
                    .as_ref()
                    .is_none_or(|s| s.len() <= crate::MAX_ANSWER)
                && self.progress.is_none_or(|p| p <= 100)
                && self.artifacts.len() <= 16,
            "run_too_large"
        );
        anyhow::ensure!(
            self.created >= 0
                && self.updated >= self.created
                && self.heartbeat >= self.created
                && self.heartbeat <= self.updated,
            "invalid_run_time"
        );
        anyhow::ensure!(
            self.cancel_requested_at
                .is_none_or(|t| t >= self.created && t <= self.updated)
                && self.cancel_acknowledged_at.is_none_or(|t| self
                    .cancel_requested_at
                    .is_some_and(|r| t >= r && t <= self.updated))
                && (self.state != RunState::Cancelled || self.cancel_acknowledged_at.is_some())
                && (self.cancel_acknowledged_at.is_none() || self.state == RunState::Cancelled),
            "invalid_cancel_state"
        );
        for artifact in &self.artifacts {
            artifact.validate()?;
        }
        anyhow::ensure!(
            serde_json::to_vec(self)?.len() <= MAX_RUN_BYTES,
            "run_too_large"
        );
        Ok(())
    }
    pub(crate) fn ensure_recovery_headroom(&self) -> anyhow::Result<()> {
        if !self.state.is_terminal() {
            anyhow::ensure!(
                serde_json::to_vec(self)?.len() <= MAX_RUN_BYTES - RECOVERY_METADATA_BYTES,
                "run_recovery_headroom"
            );
        }
        Ok(())
    }
    pub fn actor(&self) -> RunActor {
        RunActor {
            connection_id: self.connection_id.clone(),
            workspace: self.workspace.clone(),
            session: self.session.clone(),
            generation: self.generation.clone(),
        }
    }
    fn check_actor(&self, actor: &RunActor, now: i64) -> anyhow::Result<()> {
        anyhow::ensure!(self.actor() == *actor, "run_actor_mismatch");
        self.check_time(now)
    }
    fn check_time(&self, now: i64) -> anyhow::Result<()> {
        anyhow::ensure!(!self.state.is_terminal(), "run_terminal");
        anyhow::ensure!(now >= self.updated, "run_report_out_of_order");
        Ok(())
    }
    fn advance(&mut self, now: i64) -> anyhow::Result<()> {
        self.updated = now;
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("run_revision_full"))?;
        self.validate()
    }
    pub fn apply_report(
        &mut self,
        actor: &RunActor,
        report: RunUpdate,
        now: i64,
    ) -> anyhow::Result<()> {
        self.check_actor(actor, now)?;
        anyhow::ensure!(self.state.permits(report.state), "invalid_run_transition");
        anyhow::ensure!(
            report.state != RunState::Unknown,
            "run_unknown_is_local_observation"
        );
        if report.state == RunState::Cancelled {
            anyhow::ensure!(self.cancel_requested_at.is_some(), "cancel_not_requested");
        }
        let mut next = self.clone();
        next.state = report.state;
        next.progress = report.progress;
        next.progress_provenance = ProgressProvenance::AgentReported;
        next.message = report.message;
        next.result = report.result;
        next.artifacts = report.artifacts;
        next.heartbeat = now;
        if next.state == RunState::Cancelled {
            next.cancel_acknowledged_at = Some(now);
        }
        next.advance(now)?;
        next.ensure_recovery_headroom()?;
        *self = next;
        Ok(())
    }
    pub fn heartbeat(&mut self, actor: &RunActor, now: i64) -> anyhow::Result<()> {
        self.check_actor(actor, now)?;
        let mut next = self.clone();
        next.heartbeat = now;
        next.advance(now)?;
        *self = next;
        Ok(())
    }
    /// Owner intent only. The reported state remains unchanged until actor acknowledgment.
    pub fn request_cancel(&mut self, now: i64) -> anyhow::Result<()> {
        self.check_time(now)?;
        if self.cancel_requested_at.is_some() {
            return Ok(());
        }
        let mut next = self.clone();
        next.cancel_requested_at = Some(now);
        next.advance(now)?;
        *self = next;
        Ok(())
    }
    pub fn acknowledge_cancel(
        &mut self,
        actor: &RunActor,
        message: &str,
        now: i64,
    ) -> anyhow::Result<()> {
        self.apply_report(
            actor,
            RunUpdate {
                state: RunState::Cancelled,
                progress: self.progress,
                message: message.into(),
                result: self.result.clone(),
                artifacts: self.artifacts.clone(),
            },
            now,
        )
    }
    pub fn mark_stale(&mut self, now: i64, timeout: i64) -> bool {
        if self.state.is_terminal()
            || self.state == RunState::Unknown
            || now < self.updated
            || timeout <= 0
            || now.saturating_sub(self.heartbeat) < timeout
        {
            return false;
        }
        self.restore(now).is_ok()
    }
    /// Restored unfinished records have no evidence that the previous actor is still running.
    pub fn restore(&mut self, now: i64) -> anyhow::Result<()> {
        if self.state.is_terminal() || self.state == RunState::Unknown {
            return Ok(());
        }
        let mut next = self.clone();
        next.state = RunState::Unknown;
        next.advance(now.max(self.updated))?;
        *self = next;
        Ok(())
    }
    pub(crate) fn validate_successor(&self, next: &Self) -> anyhow::Result<()> {
        self.validate()?;
        next.validate()?;
        anyhow::ensure!(
            self.id == next.id
                && self.actor() == next.actor()
                && self.connection_name == next.connection_name
                && self.title == next.title
                && self.instruction == next.instruction
                && self.created == next.created,
            "run_identity_changed"
        );
        if self == next {
            return Ok(());
        }
        anyhow::ensure!(
            self.revision.checked_add(1) == Some(next.revision)
                && next.updated >= self.updated
                && next.heartbeat >= self.heartbeat,
            "run_revision_conflict"
        );
        anyhow::ensure!(self.state.permits(next.state), "invalid_run_transition");
        if next.state != RunState::Unknown
            && (self.state != next.state
                || self.progress != next.progress
                || self.message != next.message
                || self.result != next.result
                || self.artifacts != next.artifacts)
        {
            next.ensure_recovery_headroom()?;
        }
        anyhow::ensure!(
            self.cancel_requested_at.is_none()
                || self.cancel_requested_at == next.cancel_requested_at,
            "cancel_request_changed"
        );
        anyhow::ensure!(
            next.state != RunState::Cancelled || self.cancel_requested_at.is_some(),
            "cancel_not_requested"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunQuery {
    pub connection_id: Option<String>,
    pub workspace: Option<String>,
    pub session: Option<String>,
    pub generation: Option<String>,
    pub state: Option<RunState>,
    pub before: Option<i64>,
    pub limit: usize,
    pub max_bytes: usize,
}
impl Default for RunQuery {
    fn default() -> Self {
        Self {
            connection_id: None,
            workspace: None,
            session: None,
            generation: None,
            state: None,
            before: None,
            limit: 100,
            max_bytes: MAX_RUN_PAGE_BYTES,
        }
    }
}
impl RunQuery {
    pub fn validate(&self) -> anyhow::Result<()> {
        for value in [
            &self.connection_id,
            &self.workspace,
            &self.session,
            &self.generation,
        ]
        .into_iter()
        .flatten()
        {
            validate_identity(value)?;
        }
        anyhow::ensure!(
            self.limit > 0
                && self.limit <= 100
                && self.max_bytes > 0
                && self.max_bytes <= MAX_RUN_PAGE_BYTES
                && self.before.is_none_or(|c| c > 0),
            "invalid_run_query"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunPage {
    pub runs: Vec<CloudRun>,
    pub next_cursor: Option<i64>,
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_run() -> CloudRun {
        CloudRun::new(
            "run-1",
            "connection-a",
            "Agent A",
            "workspace",
            "session",
            "generation-1",
            "Analyze",
            "Find the error",
            100,
        )
        .unwrap()
    }
    fn actor() -> RunActor {
        RunActor {
            connection_id: "connection-a".into(),
            workspace: "workspace".into(),
            session: "session".into(),
            generation: "generation-1".into(),
        }
    }
    fn report(state: RunState) -> RunUpdate {
        RunUpdate {
            state,
            progress: Some(50),
            message: "agent report".into(),
            result: None,
            artifacts: vec![],
        }
    }
    #[test]
    fn queued_receipt_cannot_complete_a_run_and_terminal_runs_reject_late_reports() {
        let mut run = make_run();
        assert_eq!(run.state, RunState::Queued);
        assert!(
            run.apply_report(&actor(), report(RunState::Completed), 101)
                .is_err()
        );
        run.apply_report(&actor(), report(RunState::Running), 101)
            .unwrap();
        run.apply_report(&actor(), report(RunState::Completed), 102)
            .unwrap();
        assert_eq!(run.progress_provenance, ProgressProvenance::AgentReported);
        assert!(
            run.apply_report(&actor(), report(RunState::Failed), 103)
                .is_err()
        );
    }
    #[test]
    fn wrong_actor_session_and_generation_cannot_mutate_original_run() {
        let original = make_run();
        for wrong in [
            RunActor {
                connection_id: "connection-b".into(),
                ..actor()
            },
            RunActor {
                session: "other".into(),
                ..actor()
            },
            RunActor {
                generation: "generation-2".into(),
                ..actor()
            },
            RunActor {
                workspace: "other".into(),
                ..actor()
            },
        ] {
            let mut run = original.clone();
            assert!(
                run.apply_report(&wrong, report(RunState::Running), 101)
                    .is_err()
            );
            assert_eq!(run, original);
        }
    }
    #[test]
    fn stale_and_restored_runs_require_resume_before_completion() {
        let mut run = make_run();
        run.apply_report(&actor(), report(RunState::Running), 101)
            .unwrap();
        assert!(!run.mark_stale(110, 10));
        assert!(run.mark_stale(111, 10));
        assert_eq!(run.state, RunState::Unknown);
        assert!(
            run.apply_report(&actor(), report(RunState::Completed), 112)
                .is_err()
        );
        run.apply_report(&actor(), report(RunState::Running), 112)
            .unwrap();
        run.restore(120).unwrap();
        assert_eq!(run.state, RunState::Unknown);
        run.apply_report(&actor(), report(RunState::Running), 121)
            .unwrap();
        run.apply_report(&actor(), report(RunState::Completed), 122)
            .unwrap();
        let terminal = run.clone();
        run.restore(130).unwrap();
        assert_eq!(run, terminal);
    }
    #[test]
    fn cancel_request_is_distinct_from_same_actor_acknowledgment() {
        let mut run = make_run();
        run.apply_report(&actor(), report(RunState::Running), 101)
            .unwrap();
        run.request_cancel(102).unwrap();
        assert_eq!(run.state, RunState::Running);
        assert!(run.cancel_acknowledged_at.is_none());
        let mut wrong = actor();
        wrong.connection_id = "connection-b".into();
        assert!(run.acknowledge_cancel(&wrong, "stopped", 103).is_err());
        run.acknowledge_cancel(&actor(), "stopped", 103).unwrap();
        assert_eq!(run.state, RunState::Cancelled);
        assert_eq!(run.cancel_acknowledged_at, Some(103));
        assert!(
            run.apply_report(&actor(), report(RunState::Completed), 104)
                .is_err()
        );
        assert!(
            make_run()
                .acknowledge_cancel(&actor(), "unsolicited", 101)
                .is_err()
        );
    }
    #[test]
    fn run_and_artifact_bounds_fail_without_partial_mutation() {
        let mut run = make_run();
        let original = run.clone();
        let mut update = report(RunState::Running);
        update.progress = Some(101);
        assert!(run.apply_report(&actor(), update, 101).is_err());
        assert_eq!(run, original);
        let mut update = report(RunState::Running);
        update.artifacts = vec![
            ArtifactReference {
                label: "file".into(),
                uri: "https://example.test/result".into()
            };
            17
        ];
        assert!(run.apply_report(&actor(), update, 101).is_err());
        let mut update = report(RunState::Running);
        update.artifacts = vec![ArtifactReference {
            label: "unsafe".into(),
            uri: "javascript:alert(1)".into(),
        }];
        assert!(run.apply_report(&actor(), update, 101).is_err());
        assert_eq!(run, original);
        assert!(
            CloudRun::new(
                "run",
                "a",
                "A",
                "w",
                "s",
                "g",
                &"x".repeat(257),
                "instruction",
                100
            )
            .is_err()
        );
    }
    #[test]
    fn scoped_key_is_deterministic_disjoint_and_legacy_compatible() {
        assert_eq!(operation_key("manual-legacy", "action").unwrap(), "action");
        let a = operation_key("a", "action").unwrap();
        assert_eq!(a, operation_key("a", "action").unwrap());
        assert_ne!(a, operation_key("b", "action").unwrap());
        assert_ne!(a, "action");
        assert!(operation_key("a", "bad/id").is_err());
    }

    #[test]
    fn heartbeat_preserves_unknown_status_and_rejects_wrong_actor_or_older_reports() {
        let mut run = make_run();
        run.apply_report(&actor(), report(RunState::Running), 101)
            .unwrap();
        run.heartbeat(&actor(), 105).unwrap();
        assert!(!run.mark_stale(114, 10));
        assert!(run.mark_stale(115, 10));
        run.heartbeat(&actor(), 116).unwrap();
        assert_eq!(run.state, RunState::Unknown);
        let saved = run.clone();
        let wrong = RunActor {
            connection_id: "connection-b".into(),
            ..actor()
        };
        assert!(run.heartbeat(&wrong, 117).is_err());
        assert!(run.heartbeat(&actor(), 114).is_err());
        assert_eq!(run, saved);
    }

    fn maximum_sized_running_run() -> CloudRun {
        let mut run = make_run();
        run.state = RunState::Running;
        run.revision = 9;
        run.result = Some("x".repeat(crate::MAX_ANSWER));
        run.artifacts = vec![
            ArtifactReference {
                label: "file".into(),
                uri: format!("https://example.test/{}", "x".repeat(1024))
            };
            12
        ];
        let size = serde_json::to_vec(&run).unwrap().len();
        assert!(MAX_RUN_BYTES - size <= 4096);
        run.message = "x".repeat(MAX_RUN_BYTES - size);
        assert_eq!(serde_json::to_vec(&run).unwrap().len(), MAX_RUN_BYTES);
        run.validate().unwrap();
        run
    }
    #[test]
    fn cloud_review_rejects_reports_without_recovery_metadata_headroom() {
        let mut run = make_run();
        let mut candidate = maximum_sized_running_run();
        candidate.revision = 1;
        candidate.updated = 101;
        candidate.heartbeat = 101;
        let update = RunUpdate {
            state: RunState::Running,
            progress: candidate.progress,
            message: candidate.message,
            result: candidate.result,
            artifacts: candidate.artifacts,
        };
        assert!(
            run.apply_report(&actor(), update, 101).is_err(),
            "accepted reports must leave room for stale/restart metadata growth"
        );
        assert_eq!(run.state, RunState::Queued);
    }
    #[test]
    fn cloud_review_failed_recovery_cannot_claim_successful_stale_transition() {
        let mut run = maximum_sized_running_run();
        let original = run.clone();
        assert!(
            !run.mark_stale(1_000_000, 60),
            "stale transition must report failure if bounded recovery cannot be committed"
        );
        assert_eq!(run, original);
    }
}
