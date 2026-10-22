//! Explicit local verification permits and exact-result applicability. No command is inferred.
use super::ProjectWorkState;
use deppy_core::work_management::*;
use std::sync::{Arc, atomic::AtomicBool};

pub(crate) struct ApprovedVerification {
    pub(super) initial: CommandVerificationEvidence,
    pub(super) cancel: Arc<AtomicBool>,
    pub(super) canonical_folder: std::path::PathBuf,
    pub(super) folder_handle: std::fs::File,
}
impl ApprovedVerification {
    pub fn id(&self) -> &VerificationId {
        &self.initial.id
    }
    pub fn target(&self) -> &VerificationTarget {
        &self.initial.target
    }
    pub fn spec(&self) -> &VerificationSpec {
        &self.initial.spec
    }
    pub fn evidence(&self) -> &CommandVerificationEvidence {
        &self.initial
    }
}

/// Call only in response to the native user's explicit review/execute action.
pub(crate) fn approve_local(
    state: &ProjectWorkState,
    task_id: &TaskId,
    revision: ResultRevision,
    spec: VerificationSpec,
) -> Result<ApprovedVerification, ValidationError> {
    spec.validate()?;
    let task = state
        .tasks
        .iter()
        .find(|t| &t.id == task_id && t.workspace_id == state.workspace_id)
        .ok_or_else(|| ValidationError("verification task is unavailable".into()))?;
    if task.execution_device.is_some() || !verifiable_state(task.state) {
        return Err(ValidationError(
            "verification requires a current local submitted result".into(),
        ));
    }
    let result = task
        .result
        .as_ref()
        .filter(|r| r.revision == revision)
        .ok_or_else(|| ValidationError("verification result revision changed".into()))?;
    let attempt = state
        .attempts
        .iter()
        .find(|a| {
            Some(&a.id) == task.current_attempt_id.as_ref()
                && a.task_id == task.id
                && a.workspace_id == state.workspace_id
                && verifiable_state(a.state)
        })
        .ok_or_else(|| ValidationError("verification attempt is unavailable".into()))?;
    let canonical_folder = std::fs::canonicalize(&attempt.folder)
        .map_err(|_| ValidationError("approved local attempt folder is unavailable".into()))?;
    let requested_folder = std::fs::canonicalize(&spec.cwd)
        .map_err(|_| ValidationError("verification cwd is unavailable".into()))?;
    if canonical_folder != requested_folder || !canonical_folder.is_dir() {
        return Err(ValidationError(
            "verification cwd must be the exact local attempt folder".into(),
        ));
    }
    let expected_metadata = std::fs::metadata(&canonical_folder)
        .map_err(|_| ValidationError("approved local attempt folder is unavailable".into()))?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let folder_handle = options
        .open(&canonical_folder)
        .map_err(|_| ValidationError("approved local attempt folder cannot be pinned".into()))?;
    let pinned_metadata = folder_handle
        .metadata()
        .map_err(|_| ValidationError("approved local attempt folder cannot be inspected".into()))?;
    if !same_directory(&expected_metadata, &pinned_metadata)
        || std::fs::canonicalize(&spec.cwd).ok().as_ref() != Some(&canonical_folder)
    {
        return Err(ValidationError(
            "approved local attempt folder changed during review".into(),
        ));
    }
    let initial = CommandVerificationEvidence {
        id: VerificationId::new(),
        target: VerificationTarget {
            workspace_id: state.workspace_id.clone(),
            task_id: task.id.clone(),
            attempt_id: attempt.id.clone(),
            result_revision: revision,
            source_revision: result.source_revision.clone(),
            artifact_revision: result.artifact_revision.clone(),
        },
        spec,
        started_at_ms: now_ms(),
        ended_at_ms: None,
        outcome: CommandVerificationOutcome::Running,
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        error: None,
    };
    initial.validate()?;
    Ok(ApprovedVerification {
        initial,
        canonical_folder,
        folder_handle,
        cancel: Arc::new(AtomicBool::new(false)),
    })
}

pub(super) fn same_directory(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        a.is_dir() && b.is_dir() && a.dev() == b.dev() && a.ino() == b.ino()
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        false
    }
}

fn verifiable_state(state: WorkState) -> bool {
    matches!(
        state,
        WorkState::ResultSubmitted
            | WorkState::VerificationFailed
            | WorkState::AwaitingUserConfirmation
    )
}

pub(crate) fn matches_current(
    state: &ProjectWorkState,
    evidence: &CommandVerificationEvidence,
) -> bool {
    let target = &evidence.target;
    evidence.validate().is_ok()
        && state.workspace_id == target.workspace_id
        && state.tasks.iter().any(|task| {
            task.id == target.task_id
                && task.workspace_id == target.workspace_id
                && task.execution_device.is_none()
                && verifiable_state(task.state)
                && task.current_attempt_id.as_ref() == Some(&target.attempt_id)
                && task.result.as_ref().is_some_and(|result| {
                    result.revision == target.result_revision
                        && result.source_revision == target.source_revision
                        && result.artifact_revision == target.artifact_revision
                })
        })
        && state.attempts.iter().any(|attempt| {
            attempt.id == target.attempt_id
                && attempt.task_id == target.task_id
                && attempt.workspace_id == target.workspace_id
                && verifiable_state(attempt.state)
        })
}

pub(crate) fn result_state(evidence: &CommandVerificationEvidence) -> Option<VerificationState> {
    if evidence.validate().is_err() {
        return None;
    }
    match evidence.outcome {
        CommandVerificationOutcome::Passed => Some(VerificationState::Passed),
        CommandVerificationOutcome::NonzeroExit
        | CommandVerificationOutcome::SpawnFailed
        | CommandVerificationOutcome::TimedOut
        | CommandVerificationOutcome::IoFailed => Some(VerificationState::Failed),
        _ => None,
    }
}

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use deppy_core::{SessionId, WorkspaceId};
    pub fn fixture(folder: &std::path::Path) -> ProjectWorkState {
        let (mut state, config, _, _) = crate::work_management::dispatch::tests::fixture();
        let task = &mut state.tasks[0];
        let attempt_id = AttemptId::new();
        task.current_attempt_id = Some(attempt_id.clone());
        task.state = WorkState::ResultSubmitted;
        task.result = Some(TaskResult {
            revision: ResultRevision(1),
            summary: "Done".into(),
            source_revision: Some("provider-turn-actual".into()),
            artifact_revision: None,
            verification: VerificationState::NotRun,
        });
        state.attempts.push(Attempt {
            workspace_id: state.workspace_id.clone(),
            id: attempt_id,
            task_id: task.id.clone(),
            number: 1,
            operation_id: OperationId::new(),
            config,
            folder: folder.to_string_lossy().into_owned(),
            persistent_binding: None,
            runtime_binding: Some(RuntimeBinding {
                incarnation: RuntimeIncarnationId::new(),
                session_id: SessionId(100),
                execution_generation: 1,
            }),
            state: WorkState::ResultSubmitted,
            last_event_sequence: 1,
        });
        state
    }
    pub fn spec(folder: &std::path::Path, command: &str) -> VerificationSpec {
        VerificationSpec {
            command: command.into(),
            cwd: folder.to_string_lossy().into_owned(),
            timeout_ms: 1000,
            output_limit_bytes: 1024,
        }
    }
    #[test]
    fn verification_approval_is_bound_to_exact_local_result_and_folder() {
        let dir = std::env::temp_dir();
        let mut state = fixture(&dir);
        let id = state.tasks[0].id.clone();
        let approved =
            approve_local(&state, &id, ResultRevision(1), spec(&dir, "printf ok")).unwrap();
        assert_eq!(
            approved.target().source_revision.as_deref(),
            Some("provider-turn-actual")
        );
        assert!(matches_current(&state, &approved.initial));
        assert_eq!(approved.evidence().id, *approved.id());
        assert!(approved.evidence().started_at_ms > 0);
        assert!(approve_local(&state, &id, ResultRevision(2), spec(&dir, "printf ok")).is_err());
        assert!(
            approve_local(
                &state,
                &id,
                ResultRevision(1),
                spec(std::path::Path::new("/"), "printf ok")
            )
            .is_err()
        );
        state.tasks[0].execution_device = Some("other-device".into());
        assert!(approve_local(&state, &id, ResultRevision(1), spec(&dir, "printf ok")).is_err());
        state.tasks[0].execution_device = None;
        state.tasks[0].state = WorkState::Cancelled;
        assert!(!matches_current(&state, &approved.initial));
        state.tasks[0].state = WorkState::ResultSubmitted;
        state.tasks[0].result.as_mut().unwrap().source_revision = Some("different-source".into());
        assert!(!matches_current(&state, &approved.initial));
        state.workspace_id = WorkspaceId::new();
        assert!(!matches_current(&state, &approved.initial));
    }

    #[test]
    fn verification_pass_needs_current_revision_and_separate_user_confirmation() {
        let dir = std::env::temp_dir();
        let mut state = fixture(&dir);
        let approved = approve_local(
            &state,
            &state.tasks[0].id,
            ResultRevision(1),
            spec(&dir, "printf ok"),
        )
        .unwrap();
        let mut evidence = approved.initial;
        evidence.outcome = CommandVerificationOutcome::Passed;
        evidence.exit_code = Some(0);
        evidence.ended_at_ms = Some(now_ms());
        assert!(matches_current(&state, &evidence));
        assert_eq!(result_state(&evidence), Some(VerificationState::Passed));
        let attempt = &state.attempts[0];
        state
            .apply_event(WorkEvent {
                id: EventId::new(),
                workspace_id: state.workspace_id.clone(),
                task_id: attempt.task_id.clone(),
                attempt_id: attempt.id.clone(),
                runtime_binding: attempt.runtime_binding.clone().unwrap(),
                sequence: 2,
                kind: WorkEventKind::VerificationFinished {
                    result_revision: ResultRevision(1),
                    outcome: VerificationState::Passed,
                },
            })
            .unwrap();
        assert_eq!(state.tasks[0].state, WorkState::AwaitingUserConfirmation);
        state.tasks[0].result.as_mut().unwrap().revision = ResultRevision(2);
        assert!(!matches_current(&state, &evidence));
        evidence.outcome = CommandVerificationOutcome::Cancelled;
        assert_eq!(result_state(&evidence), None);
        evidence.outcome = CommandVerificationOutcome::Interrupted;
        assert_eq!(result_state(&evidence), None);
    }
}
