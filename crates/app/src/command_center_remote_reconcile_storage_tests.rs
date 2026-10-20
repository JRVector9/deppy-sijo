use super::*;

fn fixture() -> (CommandCenterHost, ExecutionLease, String, cc::RouteTarget) {
    let (mut host, lease, id) = super::super::tests::host_with_pending(Stage::Done);
    let pending = host.remotes.get_mut(&id).unwrap();
    pending.record.phase = Phase::Unknown;
    let assignment = pending.assignment.as_ref().unwrap().clone();
    apply_delivery_outcome(
        host.projects.get_mut("workspace-a").unwrap(),
        &assignment,
        ui::composer::PromptAdmissionOutcome::Unknown,
    );
    let task = &host.projects["workspace-a"].snapshot.tasks[0];
    let target = target_for(&host, task).unwrap();
    (host, lease, id, target)
}

fn query(host: &mut CommandCenterHost, lease: ExecutionLease, id: &str) {
    host.reconciles.pending.insert(
        101,
        Query {
            click: uuid::Uuid::new_v4().to_string(),
            source: Source::Dispatch(id.into()),
            lease,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(15),
        },
    );
}

fn admit(host: &mut CommandCenterHost, lease: ExecutionLease) {
    assert!(input_receipt(
        host,
        &RemoteInputReceipt {
            request_id: 101,
            operation_id: lease.request.operation_id.clone(),
            lease,
            result: runtime::remote_input::RemoteTaskInputResult::Admitted,
            accepted: Some(wm::remote_controller::RemoteInputAcceptance {
                sequence: 4,
                at_micros: 1234,
            }),
        }
    ));
}

#[test]
fn receipt_query_blocks_only_its_workspace_reload_until_reply() {
    let (mut host, lease, id, _) = fixture();
    query(&mut host, lease.clone(), &id);
    assert!(super::super::project_pending(&host, "workspace-a"));
    assert!(!super::super::project_pending(&host, "workspace-b"));
    assert!(host.request_project_load("retry", "workspace-a").is_err());
    assert!(!host.requested_loads.contains_key("workspace-a"));
    assert!(host.request_project_load("other", "workspace-b").is_ok());
    admit(&mut host, lease);
    assert!(!super::super::project_pending(&host, "workspace-a"));
}

#[test]
fn receipt_query_cannot_enter_while_workspace_load_or_storage_recovery_is_pending() {
    for loading in ["unloaded", "blocked", "queued", "inflight"] {
        let (mut host, _, _, target) = fixture();
        match loading {
            "unloaded" => host.projects.get_mut("workspace-a").unwrap().loaded = false,
            "blocked" => {
                host.projects
                    .get_mut("workspace-a")
                    .unwrap()
                    .storage_blocked = true
            }
            "queued" => {
                host.requested_loads
                    .insert("workspace-a".into(), "retry".into());
            }
            "inflight" => {
                host.flight = Some(Flight::Load {
                    workspace: "workspace-a".into(),
                    token: 77,
                    receipt: None,
                });
            }
            _ => unreachable!(),
        }
        assert!(begin(&mut host, "status", &target).is_err(), "{loading}");
        assert!(!pending(&host), "{loading}");
    }
}

#[test]
fn delayed_storage_load_preserves_exact_admission_after_receipt_save_failure() {
    let (mut host, lease, id, _) = fixture();
    let directory = std::env::temp_dir().join(format!("cc-receipt-load-{}", uuid::Uuid::new_v4()));
    let mut store = stored::CommandCenterStore::open(&directory).unwrap();
    let stored::SaveOutcome::Saved { revision } = store
        .save_workspace(&host.projects["workspace-a"].snapshot, None)
        .unwrap()
    else {
        panic!("new fixture must save");
    };
    // Capture the old repository answer, then pause its admission until after
    // the actual original receipt and failed write have arrived.
    let mut loaded = store
        .load_workspace(&lease.request.workspace_id)
        .unwrap()
        .unwrap()
        .snapshot;
    assert_eq!(
        loaded.outbox[0].state,
        stored::DispatchOutboxState::Uncertain
    );
    query(&mut host, lease.clone(), &id);
    admit(&mut host, lease.clone());
    let project = host.projects.get_mut("workspace-a").unwrap();
    assert!(matches!(
        store.save_workspace(&project.snapshot, None).unwrap(),
        stored::SaveOutcome::Conflict { actual: Some(_) }
    ));
    project.storage_blocked = true;
    // Future settings do not change the config captured by this admitted attempt.
    project.snapshot.config.revision = project.snapshot.config.revision.next().unwrap();
    project
        .snapshot
        .config
        .name
        .push_str(" unsaved future settings");
    project.dirty();
    assert_eq!(
        project.ledger.stage(&lease.request.operation_id),
        Some(wm::dispatch::DeliveryState::Admitted)
    );
    assert!(preserve_known_delivery_receipts(&mut loaded, project));
    assert_eq!(
        loaded.outbox[0].state,
        stored::DispatchOutboxState::Admitted
    );
    assert_eq!(loaded.attempts[0], project.snapshot.attempts[0]);
    assert_eq!(loaded.tasks[0], project.snapshot.tasks[0]);
    assert_ne!(loaded.config, project.snapshot.config);
    recover_observation(&mut loaded);
    assert!(matches!(
        store.save_workspace(&loaded, Some(revision)).unwrap(),
        stored::SaveOutcome::Saved { .. }
    ));
    let durable = store
        .load_workspace(&lease.request.workspace_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        durable.snapshot.outbox[0].state,
        stored::DispatchOutboxState::Admitted
    );
    assert_eq!(
        durable.snapshot.tasks[0].state,
        work::WorkState::ObservationUnknown
    );
    assert!(host.remotes[&id].stage == Stage::Done);
    assert!(host.inputs.is_empty());
    drop(store);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn delayed_storage_load_preserves_actual_rejection_without_rearming_failed_attempt() {
    let (mut host, lease, id, _) = fixture();
    let mut loaded = host.projects["workspace-a"].snapshot.clone();
    query(&mut host, lease.clone(), &id);
    assert!(input_receipt(
        &mut host,
        &RemoteInputReceipt {
            request_id: 101,
            operation_id: lease.request.operation_id.clone(),
            lease,
            result: runtime::remote_input::RemoteTaskInputResult::Rejected {
                reason: pty::PtyInputRejectReason::AdmissionDenied
            },
            accepted: None,
        }
    ));
    let project = &host.projects["workspace-a"];
    assert!(preserve_known_delivery_receipts(&mut loaded, project));
    recover_observation(&mut loaded);
    assert_eq!(
        loaded.outbox[0].state,
        stored::DispatchOutboxState::Acknowledged
    );
    assert_eq!(loaded.tasks[0].state, work::WorkState::Failed);
    assert!(host.remotes[&id].stage == Stage::Done);
    assert!(host.inputs.is_empty());
}

#[test]
fn delayed_storage_load_never_merges_receipt_into_changed_assignment_or_binding() {
    let (mut host, lease, id, _) = fixture();
    let original = host.projects["workspace-a"].snapshot.clone();
    query(&mut host, lease.clone(), &id);
    admit(&mut host, lease);
    let project = &host.projects["workspace-a"];
    for changed in ["config", "attempt", "binding", "folder", "prompt"] {
        let mut loaded = original.clone();
        match changed {
            "config" => {
                loaded.attempts[0].config.revision =
                    loaded.attempts[0].config.revision.next().unwrap()
            }
            "attempt" => loaded.tasks[0].current_attempt_id = Some(work::AttemptId::new()),
            "binding" => {
                loaded.attempts[0]
                    .runtime_binding
                    .as_mut()
                    .unwrap()
                    .execution_generation += 1
            }
            "folder" => loaded.attempts[0].folder.push_str("/replacement"),
            "prompt" => loaded.tasks[0].instructions.push_str(" replacement"),
            _ => unreachable!(),
        }
        assert!(
            !preserve_known_delivery_receipts(&mut loaded, project),
            "{changed}"
        );
        assert_eq!(
            loaded.outbox[0].state,
            stored::DispatchOutboxState::Uncertain,
            "{changed}"
        );
    }
}
