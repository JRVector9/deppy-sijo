use super::*;
use deppy_core::work_management::{
    AgentSelection, ConfigRevision, GoalId, ResultRevision, SubmissionId, TaskDependency, WorkState,
};

fn snapshot(workspace_id: &str) -> CommandCenterSnapshot {
    let workspace_id = WorkspaceId(workspace_id.into());
    let agent = AgentSelection {
        provider: "codex".into(),
        model: Some("gpt-6.1-sol".into()),
        effort: Some("xhigh".into()),
    };
    let config = ProjectConfig {
        workspace_id: workspace_id.clone(),
        revision: ConfigRevision(1),
        name: "Project".into(),
        folder: "/fixture/project".into(),
        main_agent: agent.clone(),
        worker_agent: agent,
        max_workers: 2,
        confirm_plan: true,
        use_worktrees: true,
    };
    let task_id = TaskId("task-a".into());
    let goal_id = GoalId("goal-a".into());
    let attempt_id = AttemptId("attempt-a".into());
    CommandCenterSnapshot {
        config: config.clone(),
        main_binding: Some(PersistentBinding::Pane {
            pane_id: deppy_core::MuxPaneId("main-pane".into()),
        }),
        plan_confirmations: vec![],
        goals: vec![Goal {
            workspace_id: workspace_id.clone(),
            id: goal_id.clone(),
            submission_id: SubmissionId("submission-a".into()),
            title: "Goal".into(),
            prompt: "Build a fixture".into(),
            state: WorkState::Running,
        }],
        tasks: vec![Task {
            workspace_id: workspace_id.clone(),
            id: task_id.clone(),
            goal_id,
            parent_task_id: None,
            title: "Task".into(),
            role: "implementation".into(),
            scope: "fixture only".into(),
            assigned_agent: config.worker_agent.clone(),
            execution_device: None,
            instructions: "Implement fixture".into(),
            acceptance_criteria: "Tests pass".into(),
            state: WorkState::Running,
            current_attempt_id: Some(attempt_id.clone()),
            dependencies: vec![],
            result: None,
        }],
        attempts: vec![Attempt {
            workspace_id,
            id: attempt_id.clone(),
            task_id: task_id.clone(),
            number: 1,
            operation_id: OperationId("operation-a".into()),
            config,
            folder: "/fixture/project/worktree".into(),
            persistent_binding: None,
            runtime_binding: None,
            state: WorkState::Running,
            last_event_sequence: 4,
        }],
        events: vec![StoredEvent {
            id: EventId("event-a".into()),
            kind: String::new(),
            task_id: Some(task_id),
            attempt_id: Some(attempt_id.clone()),
            display_payload: "Working".into(),
        }],
        outbox: vec![DispatchOutbox {
            attempt_id,
            operation_id: OperationId("operation-a".into()),
            state: DispatchOutboxState::Admitted,
        }],
        drafts: vec![
            WorkDraft {
                target_key: "main".into(),
                text: "메인 초안".into(),
                delivery_uncertain: false,
            },
            WorkDraft {
                target_key: "task:task-a".into(),
                text: "작업 초안".into(),
                delivery_uncertain: true,
            },
        ],
    }
}

fn saved(outcome: SaveOutcome) -> ProjectRevision {
    match outcome {
        SaveOutcome::Saved { revision } => revision,
        other => panic!("expected saved, got {other:?}"),
    }
}

struct Fixture(std::path::PathBuf);

impl Fixture {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("deppy-command-center-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn work_management_opens_only_sidecar_without_altering_main_database() {
    let fixture = Fixture::new();
    let main_path = fixture.0.join("metadata.sqlite3");
    let main = rusqlite::Connection::open(&main_path).unwrap();
    main.execute_batch("CREATE TABLE original(id INTEGER PRIMARY KEY, value TEXT); INSERT INTO original VALUES(1, 'preserved'); PRAGMA user_version=73;").unwrap();
    drop(main);
    let original = std::fs::read(&main_path).unwrap();

    let store = CommandCenterStore::open(&fixture.0).unwrap();
    drop(store);

    assert!(fixture.0.join(COMMAND_CENTER_FILE_NAME).exists());
    assert_eq!(std::fs::read(&main_path).unwrap(), original);
    let main = rusqlite::Connection::open(&main_path).unwrap();
    assert_eq!(
        main.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        73
    );
    assert_eq!(
        main.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[test]
fn work_management_roundtrip_keeps_main_task_drafts_and_dispatch_identity() {
    let fixture = Fixture::new();
    let expected = snapshot("workspace-a");
    let mut store = CommandCenterStore::open(&fixture.0).unwrap();
    let revision = saved(store.save_workspace(&expected, None).unwrap());
    drop(store);
    let store = CommandCenterStore::open(&fixture.0).unwrap();
    assert_eq!(
        store.load_workspace(&expected.config.workspace_id).unwrap(),
        Some(StoredProject {
            revision,
            snapshot: expected
        })
    );
}

#[test]
fn work_management_history_kind_roundtrips_without_classifying_legacy_prose() {
    let fixture = Fixture::new();
    let mut legacy = serde_json::to_value(snapshot("workspace-a")).unwrap();
    legacy["events"][0].as_object_mut().unwrap().remove("kind");
    legacy["events"][0]["display_payload"] =
        serde_json::json!("승인 설정 작업이라는 단어만 있는 오래된 기록");
    let decoded: CommandCenterSnapshot = serde_json::from_value(legacy.clone()).unwrap();
    let encoded = serde_json::to_value(decoded).unwrap();
    assert_eq!(
        encoded["events"][0]
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
        ""
    );
    legacy["events"][0]["kind"] = serde_json::json!("approval");
    let classified: CommandCenterSnapshot = serde_json::from_value(legacy).unwrap();
    let mut store = CommandCenterStore::open(&fixture.0).unwrap();
    saved(store.save_workspace(&classified, None).unwrap());
    drop(store);
    let store = CommandCenterStore::open(&fixture.0).unwrap();
    let loaded = store
        .load_workspace(&classified.config.workspace_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(loaded.snapshot).unwrap()["events"][0]["kind"],
        "approval",
        "an explicit persisted history category must survive the real sidecar round trip"
    );
}

#[test]
fn work_management_two_workspaces_and_cas_never_overwrite_each_other() {
    let fixture = Fixture::new();
    let mut first = CommandCenterStore::open(&fixture.0).unwrap();
    let mut second = CommandCenterStore::open(&fixture.0).unwrap();
    let mut a = snapshot("workspace-a");
    let mut b = snapshot("workspace-b");
    b.drafts[0].text = "B only".into();
    let a_revision = saved(first.save_workspace(&a, None).unwrap());
    let b_revision = saved(second.save_workspace(&b, None).unwrap());
    assert_eq!(
        second.save_workspace(&a, None).unwrap(),
        SaveOutcome::Conflict {
            actual: Some(a_revision)
        }
    );
    a.drafts[0].text = "latest A".into();
    let latest = saved(first.save_workspace(&a, Some(a_revision)).unwrap());
    let mut stale = a.clone();
    stale.drafts[0].text = "stale edit preserved".into();
    assert_eq!(
        second.save_workspace(&stale, Some(a_revision)).unwrap(),
        SaveOutcome::Conflict {
            actual: Some(latest)
        }
    );
    assert_eq!(stale.drafts[0].text, "stale edit preserved");
    assert_eq!(
        second.load_workspace(&b.config.workspace_id).unwrap(),
        Some(StoredProject {
            revision: b_revision,
            snapshot: b
        })
    );
    assert_eq!(
        second
            .load_workspace(&a.config.workspace_id)
            .unwrap()
            .unwrap()
            .snapshot,
        a
    );
}

#[test]
fn work_management_failed_mid_transaction_preserves_previous_snapshot() {
    let fixture = Fixture::new();
    let mut store = CommandCenterStore::open(&fixture.0).unwrap();
    let original = snapshot("workspace-a");
    let revision = saved(store.save_workspace(&original, None).unwrap());
    store.conn.execute_batch("CREATE TRIGGER fixture_interrupt BEFORE INSERT ON work_attempts BEGIN SELECT RAISE(ABORT, 'fixture interruption'); END;").unwrap();
    let mut update = original.clone();
    update.drafts[0].text = "new draft".into();
    assert!(store.save_workspace(&update, Some(revision)).is_err());
    assert_eq!(
        store.load_workspace(&original.config.workspace_id).unwrap(),
        Some(StoredProject {
            revision,
            snapshot: original
        })
    );
}

#[test]
fn work_management_rejects_missing_cross_workspace_and_cyclic_dependencies() {
    let fixture = Fixture::new();
    let mut store = CommandCenterStore::open(&fixture.0).unwrap();
    let original = snapshot("workspace-a");
    let revision = saved(store.save_workspace(&original, None).unwrap());
    let mut invalid = original.clone();
    invalid.tasks[0].dependencies.push(TaskDependency {
        task_id: TaskId("missing".into()),
        required_result_revision: ResultRevision(1),
    });
    assert!(store.save_workspace(&invalid, Some(revision)).is_err());
    invalid = original.clone();
    invalid.tasks[0].workspace_id = WorkspaceId("other".into());
    assert!(store.save_workspace(&invalid, Some(revision)).is_err());
    invalid = original.clone();
    let mut b = invalid.tasks[0].clone();
    b.id = TaskId("task-b".into());
    b.current_attempt_id = None;
    b.dependencies.push(TaskDependency {
        task_id: invalid.tasks[0].id.clone(),
        required_result_revision: ResultRevision(1),
    });
    invalid.tasks[0].dependencies.push(TaskDependency {
        task_id: b.id.clone(),
        required_result_revision: ResultRevision(1),
    });
    invalid.tasks.push(b);
    assert!(store.save_workspace(&invalid, Some(revision)).is_err());
    assert_eq!(
        store
            .load_workspace(&original.config.workspace_id)
            .unwrap()
            .unwrap()
            .snapshot,
        original
    );
}

#[test]
fn work_management_configuration_changes_do_not_rewrite_attempt_snapshot() {
    let fixture = Fixture::new();
    let mut store = CommandCenterStore::open(&fixture.0).unwrap();
    let original = snapshot("workspace-a");
    let revision = saved(store.save_workspace(&original, None).unwrap());
    let mut update = original.clone();
    update.config.max_workers = 3;
    assert!(store.save_workspace(&update, Some(revision)).is_err());
    update.config.revision = ConfigRevision(2);
    let revision = saved(store.save_workspace(&update, Some(revision)).unwrap());
    let restored = store
        .load_workspace(&original.config.workspace_id)
        .unwrap()
        .unwrap();
    assert_eq!(restored.snapshot.attempts[0].config, original.config);
    let mut rewritten = update.clone();
    rewritten.attempts[0].config = update.config.clone();
    assert!(store.save_workspace(&rewritten, Some(revision)).is_err());
    let mut reused_operation = update.clone();
    reused_operation.attempts[0].operation_id = OperationId("reassigned".into());
    reused_operation.outbox[0].operation_id = OperationId("reassigned".into());
    assert!(
        store
            .save_workspace(&reused_operation, Some(revision))
            .is_err()
    );
}

#[test]
fn work_management_outbox_cannot_ack_an_operation_owned_by_another_attempt() {
    let fixture = Fixture::new();
    let mut store = CommandCenterStore::open(&fixture.0).unwrap();
    let mut project = snapshot("workspace-a");
    project.outbox[0].operation_id = OperationId("wrong-operation".into());
    assert!(store.save_workspace(&project, None).is_err());
    assert!(
        store
            .load_workspace(&project.config.workspace_id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn work_management_snapshot_bounds_reject_without_truncating_drafts() {
    let fixture = Fixture::new();
    let mut store = CommandCenterStore::open(&fixture.0).unwrap();
    let original = snapshot("workspace-a");
    let revision = saved(store.save_workspace(&original, None).unwrap());
    let mut invalid = original.clone();
    invalid.drafts[0].text = "가".repeat(deppy_core::work_management::WORK_DRAFT_MAX_BYTES / 3 + 1);
    let text = invalid.drafts[0].text.clone();
    assert!(store.save_workspace(&invalid, Some(revision)).is_err());
    assert_eq!(invalid.drafts[0].text, text);
    invalid = original.clone();
    invalid.events[0].display_payload =
        "x".repeat(deppy_core::work_management::WORK_EVENT_MAX_BYTES + 1);
    assert!(store.save_workspace(&invalid, Some(revision)).is_err());
    assert_eq!(
        store
            .load_workspace(&original.config.workspace_id)
            .unwrap()
            .unwrap()
            .snapshot,
        original
    );
}

#[test]
fn work_management_acknowledged_or_uncertain_outbox_never_becomes_pending_again() {
    let fixture = Fixture::new();
    let mut store = CommandCenterStore::open(&fixture.0).unwrap();
    let mut project = snapshot("workspace-a");
    project.outbox[0].state = DispatchOutboxState::Acknowledged;
    let revision = saved(store.save_workspace(&project, None).unwrap());
    let mut rollback = project.clone();
    rollback.outbox[0].state = DispatchOutboxState::Pending;
    assert!(store.save_workspace(&rollback, Some(revision)).is_err());
    rollback = project.clone();
    rollback.outbox.clear();
    assert!(store.save_workspace(&rollback, Some(revision)).is_err());
    let mut uncertain = snapshot("workspace-b");
    uncertain.outbox[0].state = DispatchOutboxState::Uncertain;
    let revision = saved(store.save_workspace(&uncertain, None).unwrap());
    uncertain.outbox[0].state = DispatchOutboxState::Pending;
    assert!(store.save_workspace(&uncertain, Some(revision)).is_err());
}

#[test]
fn work_management_runtime_binding_is_historical_and_cannot_be_reassigned() {
    use deppy_core::work_management::{RuntimeBinding, RuntimeIncarnationId};
    let fixture = Fixture::new();
    let mut store = CommandCenterStore::open(&fixture.0).unwrap();
    let mut project = snapshot("workspace-a");
    project.attempts[0].runtime_binding = Some(RuntimeBinding {
        incarnation: RuntimeIncarnationId("runtime-one".into()),
        session_id: deppy_core::SessionId(1),
        execution_generation: 1,
    });
    let revision = saved(store.save_workspace(&project, None).unwrap());
    let mut changed = project.clone();
    changed.attempts[0]
        .runtime_binding
        .as_mut()
        .unwrap()
        .execution_generation = 2;
    assert!(store.save_workspace(&changed, Some(revision)).is_err());
    assert_eq!(
        store
            .load_workspace(&project.config.workspace_id)
            .unwrap()
            .unwrap()
            .snapshot,
        project
    );
}

#[test]
fn work_management_altered_sidecar_schema_is_rejected_before_writing() {
    let fixture = Fixture::new();
    drop(CommandCenterStore::open(&fixture.0).unwrap());
    let path = fixture.0.join(COMMAND_CENTER_FILE_NAME);
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("DROP TABLE work_dispatch_outbox;")
        .unwrap();
    drop(conn);
    assert!(CommandCenterStore::open(&fixture.0).is_err());
}

#[test]
fn work_management_snapshot_replacement_cannot_drop_active_attempt_or_task() {
    let fixture = Fixture::new();
    let mut store = CommandCenterStore::open(&fixture.0).unwrap();
    let original = snapshot("workspace-a");
    let revision = saved(store.save_workspace(&original, None).unwrap());
    let mut removal = original.clone();
    removal.attempts.clear();
    removal.events.clear();
    removal.outbox.clear();
    removal.tasks[0].current_attempt_id = None;
    assert!(store.save_workspace(&removal, Some(revision)).is_err());
    removal.tasks.clear();
    assert!(store.save_workspace(&removal, Some(revision)).is_err());
    assert_eq!(
        store
            .load_workspace(&original.config.workspace_id)
            .unwrap()
            .unwrap()
            .snapshot,
        original
    );
}

#[test]
fn work_management_terminal_attempt_with_uncertain_delivery_cannot_be_archived() {
    let fixture = Fixture::new();
    let mut store = CommandCenterStore::open(&fixture.0).unwrap();
    let mut original = snapshot("workspace-a");
    original.tasks[0].state = WorkState::Failed;
    original.attempts[0].state = WorkState::Failed;
    original.outbox[0].state = DispatchOutboxState::Uncertain;
    let revision = saved(store.save_workspace(&original, None).unwrap());
    let mut archived = original.clone();
    archived.tasks.clear();
    archived.attempts.clear();
    archived.events.clear();
    archived.outbox.clear();
    assert!(store.save_workspace(&archived, Some(revision)).is_err());
}

#[test]
fn work_management_result_revision_cannot_regress_or_disappear() {
    use deppy_core::work_management::{TaskResult, VerificationState};
    let fixture = Fixture::new();
    let mut store = CommandCenterStore::open(&fixture.0).unwrap();
    let mut original = snapshot("workspace-a");
    original.tasks[0].result = Some(TaskResult {
        revision: ResultRevision(2),
        summary: "latest result".into(),
        source_revision: None,
        artifact_revision: None,
        verification: VerificationState::Passed,
    });
    let revision = saved(store.save_workspace(&original, None).unwrap());
    let mut stale = original.clone();
    stale.tasks[0].result.as_mut().unwrap().revision = ResultRevision(1);
    assert!(store.save_workspace(&stale, Some(revision)).is_err());
    stale.tasks[0].result = None;
    assert!(store.save_workspace(&stale, Some(revision)).is_err());
}

#[test]
fn work_management_concurrent_cas_has_exactly_one_winner() {
    let fixture = Fixture::new();
    let mut store = CommandCenterStore::open(&fixture.0).unwrap();
    let original = snapshot("workspace-a");
    let revision = saved(store.save_workspace(&original, None).unwrap());
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|index| {
            let path = fixture.0.clone();
            let barrier = barrier.clone();
            let mut project = original.clone();
            std::thread::spawn(move || {
                let mut store = CommandCenterStore::open(&path).unwrap();
                project.drafts[0].text = format!("writer {index}");
                barrier.wait();
                store.save_workspace(&project, Some(revision)).unwrap()
            })
        })
        .collect();
    let outcomes: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, SaveOutcome::Saved { .. }))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, SaveOutcome::Conflict { .. }))
            .count(),
        1
    );
}

#[test]
fn work_management_sqlite_enforces_internal_fk_uniques_and_dependency_cycles() {
    let fixture = Fixture::new();
    let mut store = CommandCenterStore::open(&fixture.0).unwrap();
    let mut project = snapshot("workspace-a");
    let mut second = project.tasks[0].clone();
    second.id = TaskId("task-b".into());
    second.current_attempt_id = None;
    project.tasks.push(second);
    saved(store.save_workspace(&project, None).unwrap());
    assert!(
        store
            .conn
            .execute(
                "INSERT INTO work_goals VALUES('workspace-a','other-goal','submission-a')",
                []
            )
            .is_err()
    );
    assert!(store.conn.execute("INSERT INTO work_attempts VALUES('workspace-other','attempt-b','task-a',1,'operation-a')", []).is_err());
    assert!(store.conn.execute("INSERT INTO work_dispatch_outbox VALUES('workspace-a','attempt-a','wrong-operation','acknowledged')", []).is_err());
    assert!(
        store
            .conn
            .execute(
                "INSERT INTO work_events VALUES('workspace-a','other-event','task-b','attempt-a')",
                []
            )
            .is_err()
    );
    assert!(
        store
            .conn
            .execute(
                "INSERT INTO work_dependencies VALUES('workspace-a','task-a','task-a',1)",
                []
            )
            .is_err()
    );
    store
        .conn
        .execute(
            "INSERT INTO work_dependencies VALUES('workspace-a','task-a','task-b',1)",
            [],
        )
        .unwrap();
    assert!(
        store
            .conn
            .execute(
                "INSERT INTO work_dependencies VALUES('workspace-a','task-b','task-a',1)",
                []
            )
            .is_err()
    );
}

#[test]
fn work_management_unknown_or_future_schema_is_preserved_and_rejected() {
    let unknown = Fixture::new();
    let unknown_path = unknown.0.join(COMMAND_CENTER_FILE_NAME);
    let conn = Connection::open(&unknown_path).unwrap();
    conn.execute_batch("CREATE TABLE unrelated(value TEXT); INSERT INTO unrelated VALUES('original'); PRAGMA user_version=1;").unwrap();
    drop(conn);
    let bytes = std::fs::read(&unknown_path).unwrap();
    assert!(CommandCenterStore::open(&unknown.0).is_err());
    assert_eq!(std::fs::read(&unknown_path).unwrap(), bytes);
    let future = Fixture::new();
    drop(CommandCenterStore::open(&future.0).unwrap());
    let future_path = future.0.join(COMMAND_CENTER_FILE_NAME);
    let conn = Connection::open(&future_path).unwrap();
    conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();
    drop(conn);
    assert!(CommandCenterStore::open(&future.0).is_err());
    let conn = Connection::open(&future_path).unwrap();
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        SCHEMA_VERSION + 1
    );
}

#[test]
fn work_management_corrupt_snapshot_never_becomes_an_empty_overwrite() {
    let fixture = Fixture::new();
    let mut store = CommandCenterStore::open(&fixture.0).unwrap();
    let original = snapshot("workspace-a");
    let revision = saved(store.save_workspace(&original, None).unwrap());
    store
        .conn
        .execute(
            "UPDATE work_project_configs SET snapshot=?1 WHERE workspace_id='workspace-a'",
            [b"not json".as_slice()],
        )
        .unwrap();
    assert!(store.load_workspace(&original.config.workspace_id).is_err());
    assert!(store.save_workspace(&original, Some(revision)).is_err());
    let stored: Vec<u8> = store
        .conn
        .query_row("SELECT snapshot FROM work_project_configs", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(stored, b"not json");
}

#[test]
fn work_management_encoded_snapshot_and_record_count_limits_preserve_originals() {
    let fixture = Fixture::new();
    let mut store = CommandCenterStore::open(&fixture.0).unwrap();
    let mut project = snapshot("workspace-a");
    project.drafts = (0..7)
        .map(|index| WorkDraft {
            target_key: format!("draft-{index}"),
            text: "\0".repeat(WORK_DRAFT_MAX_BYTES),
            delivery_uncertain: false,
        })
        .collect();
    assert!(project.validate().is_ok());
    assert!(store.save_workspace(&project, None).is_err());
    assert_eq!(project.drafts[0].text.len(), WORK_DRAFT_MAX_BYTES);
    assert!(
        store
            .load_workspace(&project.config.workspace_id)
            .unwrap()
            .is_none()
    );
    project = snapshot("workspace-a");
    project.tasks = vec![project.tasks[0].clone(); WORK_PLAN_MAX_TASKS + 1];
    let bytes = serde_json::to_vec(&project).unwrap();
    assert!(serde_json::from_slice::<CommandCenterSnapshot>(&bytes).is_err());
    assert!(project.validate().is_err());
    project = snapshot("workspace-a");
    project.drafts = vec![project.drafts[0].clone(); WORK_DRAFT_MAX_COUNT + 1];
    let bytes = serde_json::to_vec(&project).unwrap();
    assert!(serde_json::from_slice::<CommandCenterSnapshot>(&bytes).is_err());
}

#[test]
fn work_management_explicit_archival_requires_terminal_acknowledged_records() {
    let fixture = Fixture::new();
    let mut store = CommandCenterStore::open(&fixture.0).unwrap();
    let mut project = snapshot("workspace-a");
    project.tasks[0].state = WorkState::Completed;
    project.attempts[0].state = WorkState::Completed;
    project.outbox[0].state = DispatchOutboxState::Acknowledged;
    let revision = saved(store.save_workspace(&project, None).unwrap());
    project.tasks.clear();
    project.attempts.clear();
    project.events.clear();
    project.outbox.clear();
    let revision = saved(store.save_workspace(&project, Some(revision)).unwrap());
    assert_eq!(
        store.load_workspace(&project.config.workspace_id).unwrap(),
        Some(StoredProject {
            revision,
            snapshot: project
        })
    );
}

#[test]
fn reviewed_plan_records_are_versioned_bounded_and_validate_immutable_definitions() {
    let mut original = snapshot("review-record-workspace");
    let mut task = original.tasks[0].clone();
    task.state = WorkState::PlanReview;
    task.current_attempt_id = None;
    task.result = None;
    original.plan_confirmations.push(PlanConfirmation {
        version: 1,
        config: original.config.clone(),
        plan: WorkPlan {
            workspace_id: original.config.workspace_id.clone(),
            goal_id: original.goals[0].id.clone(),
            tasks: vec![task],
        },
    });
    original.validate().unwrap();
    let encoded = serde_json::to_value(&original).unwrap();
    let mut legacy = encoded.clone();
    legacy.as_object_mut().unwrap().remove("plan_confirmations");
    let decoded: CommandCenterSnapshot = serde_json::from_value(legacy).unwrap();
    assert!(
        decoded.plan_confirmations.is_empty(),
        "legacy storage must not invent review evidence"
    );
    for change in 0..4 {
        let mut invalid = original.clone();
        match change {
            0 => invalid.plan_confirmations[0].version = 2,
            1 => invalid.plan_confirmations[0].plan.tasks[0]
                .instructions
                .push_str("changed"),
            2 => {
                invalid.plan_confirmations[0].plan.tasks[0].current_attempt_id =
                    invalid.tasks[0].current_attempt_id.clone()
            }
            _ => invalid
                .plan_confirmations
                .push(invalid.plan_confirmations[0].clone()),
        }
        assert!(invalid.validate().is_err());
    }
    let mut oversized = encoded;
    oversized["plan_confirmations"] =
        serde_json::Value::Array(vec![
            serde_json::to_value(&original.plan_confirmations[0])
                .unwrap();
            WORK_PROJECT_MAX_GOALS + 1
        ]);
    assert!(serde_json::from_value::<CommandCenterSnapshot>(oversized).is_err());
}

#[test]
fn reviewed_plan_nested_tasks_are_bounded_during_decode() {
    let mut original = snapshot("bounded-reviewed-plan");
    let mut task = original.tasks[0].clone();
    task.state = WorkState::PlanReview;
    task.current_attempt_id = None;
    task.result = None;
    original.plan_confirmations.push(PlanConfirmation {
        version: 1,
        config: original.config.clone(),
        plan: WorkPlan {
            workspace_id: original.config.workspace_id.clone(),
            goal_id: original.goals[0].id.clone(),
            tasks: vec![task.clone()],
        },
    });
    let mut value = serde_json::to_value(original).unwrap();
    let excessive_tasks = vec![task; WORK_PLAN_MAX_TASKS + 1];
    value["plan_confirmations"][0]["plan"]["tasks"] =
        serde_json::to_value(excessive_tasks).unwrap();
    assert!(
        serde_json::from_value::<CommandCenterSnapshot>(value).is_err(),
        "nested review definitions must reject excessive rows before full allocation"
    );
}
