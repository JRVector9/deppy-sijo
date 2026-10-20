use super::*;
use deppy_core::work_management::*;
use deppy_core::{SessionId, WorkspaceId};

#[test]
fn work_management_same_text_new_submission_is_new_goal() {
    let mut state = ProjectWorkState::new(WorkspaceId::new());
    let first = state
        .submit_goal(SubmissionId::new(), "Goal".into(), "같은 문장".into())
        .unwrap();
    let second = state
        .submit_goal(SubmissionId::new(), "Goal".into(), "같은 문장".into())
        .unwrap();
    assert_ne!(first, second);
    assert_ne!(state.goals[0].submission_id, state.goals[1].submission_id);
}

fn agent() -> AgentSelection {
    AgentSelection {
        provider: "codex".into(),
        model: Some("configured".into()),
        effort: Some("xhigh".into()),
    }
}

fn config(ws: &WorkspaceId) -> ProjectConfig {
    ProjectConfig {
        workspace_id: ws.clone(),
        revision: ConfigRevision(1),
        name: "Project".into(),
        folder: "/fixture/project".into(),
        main_agent: agent(),
        worker_agent: agent(),
        max_workers: 2,
        confirm_plan: true,
        use_worktrees: true,
    }
}

fn runtime() -> RuntimeBinding {
    RuntimeBinding {
        incarnation: RuntimeIncarnationId::new(),
        session_id: SessionId(1),
        execution_generation: 1,
    }
}

fn task(ws: &WorkspaceId, goal: &GoalId) -> Task {
    Task {
        workspace_id: ws.clone(),
        id: TaskId::new(),
        goal_id: goal.clone(),
        parent_task_id: None,
        role: "implement".into(),
        scope: "src".into(),
        assigned_agent: agent(),
        execution_device: None,
        title: "Work".into(),
        instructions: "Do bounded work".into(),
        acceptance_criteria: "Tests pass".into(),
        state: WorkState::PlanReview,
        current_attempt_id: None,
        dependencies: Vec::new(),
        result: None,
    }
}

fn fixture(count: usize) -> ProjectWorkState {
    let mut state = ProjectWorkState::new(WorkspaceId::new());
    let goal = state
        .submit_goal(SubmissionId::new(), "Goal".into(), "Build".into())
        .unwrap();
    let plan = WorkPlan {
        workspace_id: state.workspace_id.clone(),
        goal_id: goal.clone(),
        tasks: (0..count)
            .map(|_| task(&state.workspace_id, &goal))
            .collect(),
    };
    state.install_plan(plan).unwrap();
    state.confirm_plan(&goal).unwrap();
    state
}

fn begin(state: &mut ProjectWorkState, index: usize) -> AttemptId {
    state
        .begin_attempt(
            &state.tasks[index].id.clone(),
            OperationId::new(),
            config(&state.workspace_id),
            "/fixture/worktree".into(),
            None,
            runtime(),
        )
        .unwrap()
}

#[test]
fn work_management_preallocated_attempt_is_exact_and_retries_cannot_rebind_identity() {
    let mut state = fixture(2);
    let task = state.tasks[0].id.clone();
    let reserved = AttemptId::new();
    let operation = OperationId::new();
    let cfg = config(&state.workspace_id);
    let binding = runtime();
    let start = |state: &mut ProjectWorkState, id: AttemptId, op: OperationId| {
        state.begin_attempt_with_id(
            &task,
            id,
            op,
            cfg.clone(),
            "/fixture/worktree".into(),
            None,
            binding.clone(),
        )
    };
    assert_eq!(
        start(&mut state, reserved.clone(), operation.clone()).unwrap(),
        reserved
    );
    assert_eq!(state.attempts[0].id, reserved);
    assert_eq!(
        start(&mut state, reserved.clone(), operation.clone()).unwrap(),
        reserved
    );
    let before = format!("{state:?}");
    assert!(start(&mut state, AttemptId::new(), operation.clone()).is_err());
    assert!(start(&mut state, reserved.clone(), OperationId::new()).is_err());
    assert_eq!(format!("{state:?}"), before);
    assert_eq!(
        state
            .begin_attempt(
                &task,
                operation,
                cfg,
                "/fixture/worktree".into(),
                None,
                binding
            )
            .unwrap(),
        reserved
    );
}

#[test]
fn work_management_preallocated_attempt_collision_and_invalid_input_are_atomic() {
    let mut state = fixture(2);
    let reserved = begin(&mut state, 0);
    let task = state.tasks[1].id.clone();
    let before = format!("{state:?}");
    for (id, folder) in [
        (reserved, "/fixture/worktree".to_string()),
        (AttemptId::new(), String::new()),
        (AttemptId(String::new()), "/fixture/worktree".to_string()),
    ] {
        assert!(
            state
                .begin_attempt_with_id(
                    &task,
                    id,
                    OperationId::new(),
                    config(&state.workspace_id),
                    folder,
                    None,
                    runtime()
                )
                .is_err()
        );
        assert_eq!(format!("{state:?}"), before);
    }
}

fn event(
    state: &ProjectWorkState,
    attempt: &AttemptId,
    sequence: u64,
    kind: WorkEventKind,
) -> WorkEvent {
    let a = state.attempts.iter().find(|a| &a.id == attempt).unwrap();
    WorkEvent {
        id: EventId::new(),
        workspace_id: state.workspace_id.clone(),
        task_id: a.task_id.clone(),
        attempt_id: attempt.clone(),
        runtime_binding: a.runtime_binding.clone().unwrap(),
        sequence,
        kind,
    }
}

fn apply(
    state: &mut ProjectWorkState,
    attempt: &AttemptId,
    sequence: u64,
    kind: WorkEventKind,
) -> ApplyOutcome {
    let event = event(state, attempt, sequence, kind);
    state.apply_event(event).unwrap()
}

fn result(revision: u64) -> TaskResult {
    TaskResult {
        revision: ResultRevision(revision),
        summary: "Result".into(),
        source_revision: Some("source-1".into()),
        artifact_revision: Some(format!("artifact-{revision}")),
        verification: VerificationState::NotRun,
    }
}

#[test]
fn work_management_submission_retry_returns_original_without_rewriting() {
    let mut state = ProjectWorkState::new(WorkspaceId::new());
    let submission = SubmissionId::new();
    let id = state
        .submit_goal(submission.clone(), "Original".into(), "One".into())
        .unwrap();
    assert_eq!(
        state
            .submit_goal(submission.clone(), "Original".into(), "One".into())
            .unwrap(),
        id
    );
    assert!(
        state
            .submit_goal(submission.clone(), "Changed".into(), "One".into())
            .is_err()
    );
    assert!(
        state
            .submit_goal(submission, "Original".into(), "Two".into())
            .is_err()
    );
    assert_eq!(state.goals.len(), 1);
    assert_eq!(state.goals[0].prompt, "One");
}

#[test]
fn work_management_same_workspace_provider_events_are_task_isolated() {
    let mut state = fixture(2);
    let a = begin(&mut state, 0);
    begin(&mut state, 1);
    let before_b = state.tasks[1].clone();
    assert_eq!(
        apply(&mut state, &a, 1, WorkEventKind::Started),
        ApplyOutcome::Applied
    );
    assert_eq!(state.tasks[0].state, WorkState::Running);
    assert_eq!(state.tasks[1], before_b);
}

#[test]
fn work_management_operation_retry_and_duplicate_ack_do_not_allocate() {
    let mut state = fixture(1);
    let op = OperationId::new();
    let id = state.tasks[0].id.clone();
    let cfg = config(&state.workspace_id);
    let binding = runtime();
    let first = state
        .begin_attempt(
            &id,
            op.clone(),
            cfg.clone(),
            cfg.folder.clone(),
            None,
            binding.clone(),
        )
        .unwrap();
    let duplicate = state
        .begin_attempt(
            &id,
            op.clone(),
            cfg.clone(),
            cfg.folder.clone(),
            None,
            binding,
        )
        .unwrap();
    assert_eq!(first, duplicate);
    assert_eq!(state.attempts.len(), 1);
    assert_eq!(
        apply(
            &mut state,
            &first,
            1,
            WorkEventKind::InputAdmitted {
                operation_id: op.clone()
            }
        ),
        ApplyOutcome::Applied
    );
    assert_eq!(
        apply(
            &mut state,
            &first,
            1,
            WorkEventKind::InputAdmitted { operation_id: op }
        ),
        ApplyOutcome::Ignored
    );
    assert_eq!(state.tasks[0].state, WorkState::Queued);
}

#[test]
fn work_management_late_old_attempt_cannot_finish_new_attempt() {
    let mut state = fixture(1);
    let old = begin(&mut state, 0);
    apply(
        &mut state,
        &old,
        1,
        WorkEventKind::Failed {
            message: "failed".into(),
        },
    );
    let new = begin(&mut state, 0);
    apply(&mut state, &new, 1, WorkEventKind::Started);
    let before = state.tasks[0].clone();
    assert_eq!(
        apply(
            &mut state,
            &old,
            2,
            WorkEventKind::ResultSubmitted { result: result(1) }
        ),
        ApplyOutcome::Ignored
    );
    assert_eq!(state.tasks[0], before);
    assert_eq!(state.attempts[1].number, 2);
}

#[test]
fn work_management_stale_generation_and_foreign_workspace_are_ignored() {
    let mut state = fixture(1);
    let id = begin(&mut state, 0);
    let mut stale = event(&state, &id, 1, WorkEventKind::Started);
    stale.runtime_binding.execution_generation += 1;
    assert_eq!(state.apply_event(stale).unwrap(), ApplyOutcome::Ignored);
    let mut foreign = event(&state, &id, 2, WorkEventKind::Started);
    foreign.workspace_id = WorkspaceId::new();
    assert_eq!(state.apply_event(foreign).unwrap(), ApplyOutcome::Ignored);
    assert_eq!(state.attempts[0].last_event_sequence, 0);
}

#[test]
fn work_management_parent_exit_never_completes_running_child() {
    let mut state = fixture(2);
    state.tasks[1].parent_task_id = Some(state.tasks[0].id.clone());
    let parent = begin(&mut state, 0);
    let child = begin(&mut state, 1);
    apply(&mut state, &child, 1, WorkEventKind::Started);
    apply(&mut state, &parent, 1, WorkEventKind::ProcessExited);
    assert_eq!(state.tasks[1].state, WorkState::Running);
    assert_eq!(state.tasks[0].state, WorkState::ObservationUnknown);
    assert!(!state.goal_is_complete(&state.goals[0].id));
}

#[test]
fn work_management_disconnect_late_result_only_matches_original_execution() {
    let mut state = fixture(1);
    let id = begin(&mut state, 0);
    apply(&mut state, &id, 1, WorkEventKind::ObservationLost);
    assert_eq!(state.tasks[0].state, WorkState::ObservationUnknown);
    let mut wrong = event(
        &state,
        &id,
        2,
        WorkEventKind::ResultSubmitted { result: result(1) },
    );
    wrong.runtime_binding.incarnation = RuntimeIncarnationId::new();
    assert_eq!(state.apply_event(wrong).unwrap(), ApplyOutcome::Ignored);
    assert_eq!(
        apply(
            &mut state,
            &id,
            2,
            WorkEventKind::ResultSubmitted { result: result(1) }
        ),
        ApplyOutcome::Applied
    );
    assert_eq!(state.tasks[0].state, WorkState::ResultSubmitted);
    assert!(!state.goal_is_complete(&state.goals[0].id));
}

#[test]
fn work_management_dependency_needs_exact_explicitly_verified_revision() {
    let mut state = fixture(2);
    state.tasks[1].dependencies = vec![TaskDependency {
        task_id: state.tasks[0].id.clone(),
        required_result_revision: ResultRevision(1),
    }];
    let downstream = state.tasks[1].id.clone();
    assert!(!state.can_dispatch(&downstream));
    let first = begin(&mut state, 0);
    apply(
        &mut state,
        &first,
        1,
        WorkEventKind::ResultSubmitted { result: result(1) },
    );
    assert!(!state.can_dispatch(&downstream));
    apply(
        &mut state,
        &first,
        2,
        WorkEventKind::VerificationFinished {
            result_revision: ResultRevision(1),
            outcome: VerificationState::Passed,
        },
    );
    assert!(state.can_dispatch(&downstream));
    apply(
        &mut state,
        &first,
        3,
        WorkEventKind::ResultSubmitted { result: result(2) },
    );
    assert!(!state.can_dispatch(&downstream));
}

#[test]
fn work_management_verified_result_still_needs_user_confirmation() {
    let mut state = fixture(1);
    let id = begin(&mut state, 0);
    apply(
        &mut state,
        &id,
        1,
        WorkEventKind::ResultSubmitted { result: result(1) },
    );
    assert_eq!(
        apply(
            &mut state,
            &id,
            2,
            WorkEventKind::UserConfirmed {
                result_revision: ResultRevision(1)
            }
        ),
        ApplyOutcome::Ignored
    );
    assert_eq!(
        apply(
            &mut state,
            &id,
            3,
            WorkEventKind::VerificationFinished {
                result_revision: ResultRevision(2),
                outcome: VerificationState::Passed
            }
        ),
        ApplyOutcome::Ignored
    );
    apply(
        &mut state,
        &id,
        4,
        WorkEventKind::VerificationFinished {
            result_revision: ResultRevision(1),
            outcome: VerificationState::Passed,
        },
    );
    assert_eq!(state.tasks[0].state, WorkState::AwaitingUserConfirmation);
    assert!(!state.goal_is_complete(&state.goals[0].id));
    apply(
        &mut state,
        &id,
        5,
        WorkEventKind::UserConfirmed {
            result_revision: ResultRevision(1),
        },
    );
    assert!(state.goal_is_complete(&state.goals[0].id));
    assert_eq!(
        apply(&mut state, &id, 6, WorkEventKind::ObservationLost),
        ApplyOutcome::Ignored
    );
}

#[test]
fn work_management_plan_rejects_cycles_over_limit_and_foreign_tasks_atomically() {
    let mut state = fixture(1);
    let before = state.tasks.clone();
    let mut task = task(&state.workspace_id, &state.goals[0].id);
    task.dependencies.push(TaskDependency {
        task_id: task.id.clone(),
        required_result_revision: ResultRevision(1),
    });
    let invalid = WorkPlan {
        workspace_id: state.workspace_id.clone(),
        goal_id: state.goals[0].id.clone(),
        tasks: vec![task],
    };
    assert!(state.install_plan(invalid).is_err());
    assert_eq!(state.tasks, before);
    let mut plan = WorkPlan {
        workspace_id: state.workspace_id.clone(),
        goal_id: state.goals[0].id.clone(),
        tasks: (0..65)
            .map(|_| super::tests::task(&state.workspace_id, &state.goals[0].id))
            .collect(),
    };
    assert!(plan.validate().is_err());
    plan.tasks.truncate(64);
    assert!(plan.validate().is_ok());
    plan.tasks[0].workspace_id = WorkspaceId::new();
    assert!(plan.validate().is_err());
}

#[test]
fn work_management_oversized_input_and_event_preserve_current_state() {
    let mut state = fixture(1);
    let draft = "한".repeat(WORK_INPUT_MAX_BYTES / 3 + 1);
    let original = draft.clone();
    assert!(
        state
            .submit_goal(SubmissionId::new(), "Goal".into(), draft.clone())
            .is_err()
    );
    assert_eq!(draft, original);
    assert_eq!(state.goals.len(), 1);
    let id = begin(&mut state, 0);
    let e = event(
        &state,
        &id,
        1,
        WorkEventKind::Failed {
            message: "x".repeat(WORK_EVENT_MAX_BYTES + 1),
        },
    );
    assert!(state.apply_event(e).is_err());
    assert_eq!(state.tasks[0].state, WorkState::Queued);
    assert_eq!(state.attempts[0].last_event_sequence, 0);
}

#[test]
fn work_management_one_live_runtime_session_has_one_task_owner() {
    let mut state = fixture(2);
    begin(&mut state, 0);
    let mut binding = state.attempts[0].runtime_binding.clone().unwrap();
    binding.execution_generation += 1;
    let config = config(&state.workspace_id);
    let before = state.tasks[1].clone();
    assert!(
        state
            .begin_attempt(
                &before.id,
                OperationId::new(),
                config.clone(),
                config.folder.clone(),
                None,
                binding.clone()
            )
            .is_err()
    );
    assert_eq!(state.tasks[1], before);
    binding.incarnation = RuntimeIncarnationId::new();
    assert!(
        state
            .begin_attempt(
                &before.id,
                OperationId::new(),
                config.clone(),
                config.folder.clone(),
                None,
                binding
            )
            .is_ok()
    );
}

#[test]
fn work_management_upstream_rework_invalidates_completed_descendant_verification() {
    let mut state = fixture(2);
    state.tasks[1].dependencies = vec![TaskDependency {
        task_id: state.tasks[0].id.clone(),
        required_result_revision: ResultRevision(1),
    }];
    let first = begin(&mut state, 0);
    apply(
        &mut state,
        &first,
        1,
        WorkEventKind::ResultSubmitted { result: result(1) },
    );
    apply(
        &mut state,
        &first,
        2,
        WorkEventKind::VerificationFinished {
            result_revision: ResultRevision(1),
            outcome: VerificationState::Passed,
        },
    );
    let second = begin(&mut state, 1);
    apply(
        &mut state,
        &second,
        1,
        WorkEventKind::ResultSubmitted { result: result(1) },
    );
    apply(
        &mut state,
        &second,
        2,
        WorkEventKind::VerificationFinished {
            result_revision: ResultRevision(1),
            outcome: VerificationState::Passed,
        },
    );
    apply(
        &mut state,
        &second,
        3,
        WorkEventKind::UserConfirmed {
            result_revision: ResultRevision(1),
        },
    );
    assert_eq!(state.tasks[1].state, WorkState::Completed);
    apply(
        &mut state,
        &first,
        3,
        WorkEventKind::ResultSubmitted { result: result(2) },
    );
    assert_eq!(state.tasks[1].state, WorkState::VerificationFailed);
    assert_eq!(
        state.tasks[1].result.as_ref().unwrap().verification,
        VerificationState::NotRun
    );
    assert_eq!(
        apply(
            &mut state,
            &second,
            4,
            WorkEventKind::UserConfirmed {
                result_revision: ResultRevision(1)
            }
        ),
        ApplyOutcome::Ignored
    );
}

#[test]
fn work_management_verification_failed_retry_reuses_session_with_new_generation() {
    let mut state = fixture(1);
    let first = begin(&mut state, 0);
    apply(
        &mut state,
        &first,
        1,
        WorkEventKind::ResultSubmitted { result: result(1) },
    );
    apply(
        &mut state,
        &first,
        2,
        WorkEventKind::VerificationFinished {
            result_revision: ResultRevision(1),
            outcome: VerificationState::Failed,
        },
    );
    let mut binding = state.attempts[0].runtime_binding.clone().unwrap();
    binding.execution_generation += 1;
    let cfg = config(&state.workspace_id);
    let next = state
        .begin_attempt(
            &state.tasks[0].id.clone(),
            OperationId::new(),
            cfg.clone(),
            cfg.folder.clone(),
            None,
            binding,
        )
        .unwrap();
    assert_ne!(first, next);
    assert_eq!(state.attempts.len(), 2);
    assert_eq!(state.tasks[0].state, WorkState::Queued);
    assert_eq!(
        apply(
            &mut state,
            &first,
            3,
            WorkEventKind::ResultSubmitted { result: result(2) }
        ),
        ApplyOutcome::Ignored
    );
}

#[test]
fn work_management_operation_retry_rejects_changed_assignment_snapshot() {
    let mut state = fixture(1);
    let first = begin(&mut state, 0);
    let attempt = state.attempts[0].clone();
    let task = attempt.task_id.clone();
    let op = attempt.operation_id.clone();
    let cfg = attempt.config.clone();
    let binding = attempt.runtime_binding.clone().unwrap();
    let mut changed_cfg = cfg.clone();
    changed_cfg.revision = ConfigRevision(2);
    assert!(
        state
            .begin_attempt(
                &task,
                op.clone(),
                changed_cfg,
                attempt.folder.clone(),
                None,
                binding.clone()
            )
            .is_err()
    );
    assert!(
        state
            .begin_attempt(
                &task,
                op.clone(),
                cfg.clone(),
                "/different".into(),
                None,
                binding.clone()
            )
            .is_err()
    );
    assert!(
        state
            .begin_attempt(
                &task,
                op.clone(),
                cfg.clone(),
                attempt.folder.clone(),
                Some(PersistentBinding::ProviderThread {
                    provider: "codex".into(),
                    thread_id: "different".into()
                }),
                binding.clone()
            )
            .is_err()
    );
    let mut changed_runtime = binding;
    changed_runtime.execution_generation += 1;
    assert!(
        state
            .begin_attempt(
                &task,
                op,
                cfg,
                attempt.folder.clone(),
                None,
                changed_runtime
            )
            .is_err()
    );
    assert_eq!(state.attempts, vec![attempt]);
    assert_eq!(state.tasks[0].current_attempt_id, Some(first));
}
