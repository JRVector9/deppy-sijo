//! Actual scoped TLS + SQLite recovery after deliberately discarded replies.
use super::*;
use deppy_core::work_management::AgentSelection;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
struct Secrets(Mutex<HashMap<String, String>>);
impl SecretStore for Secrets {
    fn set_secret(&self, id: &str, secret: &SecretString) -> anyhow::Result<()> {
        self.0
            .lock()
            .unwrap()
            .insert(id.into(), secret.expose().into());
        Ok(())
    }
    fn get_secret(&self, id: &str) -> anyhow::Result<SecretString> {
        self.0
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .map(SecretString::new)
            .ok_or_else(|| anyhow::anyhow!("missing"))
    }
    fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
        self.0.lock().unwrap().remove(id);
        Ok(())
    }
    fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
        Ok(self.0.lock().unwrap().contains_key(id))
    }
}
struct Launcher;
impl runtime::remote_worker::RemoteWorkLauncher for Launcher {
    fn prepare(
        &self,
        profile: &runtime::remote_worker::RemoteWorkerProfile,
        prompt: Option<&str>,
    ) -> anyhow::Result<RuntimeCommand> {
        let mut command = crate::work_management::remote_execution::journal::tls_tests::Launcher
            .prepare(profile, prompt)?;
        if let RuntimeCommand::SpawnAgent { args, .. } = &mut command {
            *args = vec!["-c".into(), "printf launch >> launches; stty raw -echo; printf '\\033[?2004h❯ '; dd bs=1 count=25 of=accepted 2>/dev/null; sleep 2".into()];
        }
        Ok(command)
    }
}
fn authority(operation: &OperationId) -> RemoteSendAuthority {
    RemoteSendAuthority::new(
        operation.clone(),
        Instant::now() + Duration::from_secs(20),
        |action| action(),
    )
    .unwrap()
}
#[test]
fn explicit_reconcile_recovers_discarded_launch_and_input_receipts_without_replay() {
    let root = std::env::temp_dir().join(format!(
        "deppy-controller-reconcile-{}",
        uuid::Uuid::new_v4()
    ));
    let workspace = WorkspaceId("reconcile-workspace".into());
    let agent = AgentSelection {
        provider: "claude".into(),
        model: None,
        effort: None,
    };
    let (server, pin, token) =
        crate::work_management::remote_execution::journal::tls_tests::worker_for_workspace_scoped(
            &root.join("server"),
            "reconcile-device",
            &workspace.0,
            Arc::new(Launcher),
            agent.clone(),
        );
    let folder = std::fs::canonicalize(root.join("server"))
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let mut state = ControllerState::new(root.join("client"), Arc::new(Secrets::default()));
    let RemoteDeviceOutcome::Candidate { candidate, .. } =
        state.execute(RemoteDeviceJob::Inspect {
            setup: RemoteDeviceSetup {
                request_id: 1,
                name: "Explicit compute grant".into(),
                endpoint: server.local_addr(),
                pinned_fingerprint: pin,
                workspace_id: workspace.clone(),
                expected_folder: folder.clone(),
                auth_token: token,
            },
        })
    else {
        panic!("scoped candidate missing")
    };
    assert!(matches!(
        state.execute(RemoteDeviceJob::Grant {
            request_id: 2,
            candidate_id: candidate.candidate_id,
            allowed_profile_ids: vec!["fixture-profile".into()],
            max_workers: 2
        }),
        RemoteDeviceOutcome::Granted { .. }
    ));
    let request = LeaseRequest {
        device_id: "reconcile-device".into(),
        workspace_id: workspace.clone(),
        attempt_id: AttemptId::new(),
        operation_id: OperationId::new(),
        folder,
        agent,
    };
    let RemoteDeviceOutcome::LaunchPending { lease, .. } =
        state.execute(RemoteDeviceJob::ReserveLaunch {
            request_id: 3,
            authority: authority(&request.operation_id),
            request,
            initial_prompt: None,
        })
    else {
        panic!("idle launch missing")
    };
    let until = Instant::now() + Duration::from_secs(5);
    // Receive the genuine server receipt, intentionally discard it before host processing.
    loop {
        if state.connections[0].events.drain().iter().any(|e| {
            matches!(
                e,
                RuntimeEvent::RemoteWorkResolved {
                    result: RemoteWorkResult::Launched { .. },
                    ..
                }
            )
        }) {
            break;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    }
    state.flights[0].result = Some(RemoteWorkResult::ObservationUnknown);
    let RemoteDeviceOutcome::LaunchReceipt(RemoteLaunchReceipt {
        request_id: 4,
        result:
            RemoteWorkResult::Launched {
                session,
                execution_generation,
            },
        ..
    }) = state.execute(RemoteDeviceJob::ReconcileLaunch {
        request_id: 4,
        lease: lease.clone(),
    })
    else {
        panic!("read-only launch recovery missing")
    };
    assert_eq!(
        std::fs::read_to_string(root.join("server/launches")).unwrap(),
        "launch"
    );
    let until = Instant::now() + Duration::from_secs(5);
    let proof = loop {
        if let RemoteDeviceOutcome::ExecutionObserved {
            execution: Some(proof),
            ..
        } = state.execute(RemoteDeviceJob::ObserveExecution {
            request_id: 5,
            lease: lease.clone(),
            session,
            execution_generation,
        }) {
            break proof;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    };
    let operation = OperationId::new();
    assert!(matches!(
        state.execute(RemoteDeviceJob::DeliverInput {
            request_id: 6,
            lease: lease.clone(),
            execution: proof,
            operation_id: operation.clone(),
            prompt: "bounded task".into(),
            authority: authority(&operation)
        }),
        RemoteDeviceOutcome::InputPending { .. }
    ));
    let mut accepted = None;
    let mut delivered = false;
    let until = Instant::now() + Duration::from_secs(5);
    while accepted.is_none() || !delivered {
        for event in state.connections[0].events.drain() {
            match event {
                RuntimeEvent::SessionInputAccepted {
                    operation_id: Some(id),
                    sequence,
                    at_micros,
                    ..
                } if id == operation.0 => accepted = Some((sequence, at_micros)),
                RuntimeEvent::RemoteTaskInput {
                    reply:
                        RemoteTaskInputReply::Delivered {
                            operation_id,
                            result: RemoteTaskInputResult::Admitted,
                        },
                    ..
                } if operation_id == operation => delivered = true,
                _ => {}
            }
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    }
    state.inputs[0].result = Some(RemoteTaskInputResult::ObservationUnknown);
    state.inputs[0].emitted = true;
    for request_id in [7, 8] {
        let RemoteDeviceOutcome::InputReceipt(receipt) =
            state.execute(RemoteDeviceJob::ReconcileInput {
                request_id,
                lease: lease.clone(),
                operation_id: operation.clone(),
            })
        else {
            panic!("input recovery missing")
        };
        assert_eq!(receipt.request_id, request_id);
        assert_eq!(receipt.result, RemoteTaskInputResult::Admitted);
        let receipt = receipt.accepted.unwrap();
        assert_eq!(Some((receipt.sequence, receipt.at_micros)), accepted);
    }
    let RemoteDeviceOutcome::InputReceipt(missing) =
        state.execute(RemoteDeviceJob::ReconcileInput {
            request_id: 9,
            lease: lease.clone(),
            operation_id: OperationId::new(),
        })
    else {
        unreachable!()
    };
    assert_eq!(missing.result, RemoteTaskInputResult::ObservationUnknown);
    assert!(missing.accepted.is_none());
    assert_eq!(
        std::fs::read(root.join("server/accepted")).unwrap(),
        b"\x1b[200~bounded task\x1b[201~\r"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("server/launches")).unwrap(),
        "launch"
    );
    assert_eq!(
        std::fs::read(root.join("server/metadata.sqlite3")).unwrap(),
        b"legacy untouched"
    );
    // On restart, no original immutable flight exists. Query remains Unknown.
    drop(state);
    let mut restarted = ControllerState::new(root.join("client"), Arc::new(Secrets::default()));
    assert!(matches!(
        restarted.execute(RemoteDeviceJob::ReconcileLaunch {
            request_id: 10,
            lease
        }),
        RemoteDeviceOutcome::LaunchReceipt(RemoteLaunchReceipt {
            result: RemoteWorkResult::ObservationUnknown,
            ..
        })
    ));
    drop(restarted);
    server.shutdown();
    std::fs::remove_dir_all(root).unwrap();
}
