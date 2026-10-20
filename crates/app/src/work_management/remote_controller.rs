//! Native execution-device trust/setup and real pinned TLS dispatch, exclusively off-frame.
use super::devices::{
    DeviceRegistry, DeviceView, ExecutionLease, LeaseRequest, RegisteredDevice,
    VerifiedExecutionDevice, VerifiedExecutionLease, WorkerObservation, WorkerProfile,
};
use crate::lazy_worker::{LazyBoundedWorker, LazyWorkerSubmitError};
use deppy_core::WorkspaceId;
use deppy_core::work_management::{AttemptId, OperationId, RuntimeIncarnationId};
use runtime::remote_input::{
    RemoteExecutionProof, RemoteSessionCloseResult, RemoteTaskInputReply, RemoteTaskInputRequest,
    RemoteTaskInputResult, RemoteTaskTarget,
};
use runtime::remote_worker::{
    RemoteWorkIdentity, RemoteWorkRequest, RemoteWorkResult, RemoteWorkerCapabilities,
};
use runtime::{
    RemoteRuntimeClient, RuntimeCommand, RuntimeCommandSink, RuntimeEvent, RuntimeEventReceiver,
    RuntimeEventStream,
};
use secret::{SecretStore, SecretString};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use storage::work_management::{
    CommandCenterStore, RemoteOperationClaim, RemoteOperationIdentity, RemoteOperationResult,
    StoredExecutionDevice,
};

/// Deliberately neither Debug nor Serialize. The native host transfers the secret once.
pub(crate) struct RemoteDeviceSetup {
    pub request_id: u64,
    pub name: String,
    pub endpoint: SocketAddr,
    pub pinned_fingerprint: String,
    pub workspace_id: WorkspaceId,
    pub expected_folder: String,
    pub auth_token: SecretString,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteDeviceCandidate {
    pub candidate_id: String,
    pub device_id: String,
    pub name: String,
    pub endpoint: SocketAddr,
    pub pinned_fingerprint: String,
    pub workspace_id: WorkspaceId,
    pub folder: String,
    pub folder_anchor: String,
    pub grant_epoch: u64,
    pub incarnation: RuntimeIncarnationId,
    pub profiles: Vec<WorkerProfile>,
    pub max_workers: u8,
    pub active_workers: u8,
    pub expires_in_ms: u64,
}
/// Host-only, operation-bound authority. The callback must hold its own revocation fence
/// through the action. Neither the callback nor any credential crosses a wire or enters storage.
type SendAuthorizer = dyn Fn(&mut dyn FnMut()) + Send + Sync;
pub(crate) struct RemoteSendAuthority {
    operation_id: OperationId,
    deadline: Instant,
    authorize: Arc<SendAuthorizer>,
}
impl RemoteSendAuthority {
    pub(crate) fn new(
        operation_id: OperationId,
        deadline: Instant,
        authorize: impl Fn(&mut dyn FnMut()) + Send + Sync + 'static,
    ) -> Result<Self, RemoteDeviceError> {
        OperationId::from_string(operation_id.0.clone()).map_err(|_| RemoteDeviceError::Invalid)?;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(RemoteDeviceError::Stale)?;
        if remaining > Duration::from_secs(30) {
            return Err(RemoteDeviceError::Invalid);
        }
        Ok(Self {
            operation_id,
            deadline,
            authorize: Arc::new(authorize),
        })
    }
    fn deadline_unix_ms(&self) -> Option<u64> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())?
            .as_millis();
        runtime::remote_input::unix_millis()?.checked_add(u64::try_from(remaining).ok()?)
    }
    fn admit<T>(&self, operation: &OperationId, action: impl FnOnce() -> T) -> Option<T> {
        if operation != &self.operation_id || Instant::now() >= self.deadline {
            return None;
        }
        let mut action = Some(action);
        let mut result = None;
        (self.authorize)(&mut || {
            if Instant::now() < self.deadline
                && let Some(action) = action.take()
            {
                result = Some(action());
            }
        });
        result
    }
}
pub(crate) enum RemoteDeviceJob {
    Inspect {
        setup: RemoteDeviceSetup,
    },
    Grant {
        request_id: u64,
        candidate_id: String,
        allowed_profile_ids: Vec<String>,
        max_workers: u8,
    },
    Load {
        workspace_id: WorkspaceId,
    },
    Poll {
        workspace_id: WorkspaceId,
    },
    Revoke {
        request_id: u64,
        workspace_id: WorkspaceId,
        device_id: String,
    },
    CloseSession {
        request_id: u64,
        lease: ExecutionLease,
        execution: Box<RemoteExecutionProof>,
        operation_id: OperationId,
        authority: RemoteSendAuthority,
    },
    ObserveSession {
        request_id: u64,
        lease: ExecutionLease,
        session: deppy_core::SessionId,
        execution_generation: u64,
    },
    ObserveExecution {
        request_id: u64,
        lease: ExecutionLease,
        session: deppy_core::SessionId,
        execution_generation: u64,
    },
    DeliverInput {
        request_id: u64,
        lease: ExecutionLease,
        execution: Box<RemoteExecutionProof>,
        operation_id: OperationId,
        prompt: String,
        authority: RemoteSendAuthority,
    },
    ReconcileLaunch {
        request_id: u64,
        lease: ExecutionLease,
    },
    ReconcileInput {
        request_id: u64,
        lease: ExecutionLease,
        operation_id: OperationId,
    },
    ReserveLaunch {
        request_id: u64,
        request: LeaseRequest,
        initial_prompt: Option<String>,
        authority: RemoteSendAuthority,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteDeviceError {
    Invalid,
    Full,
    Disconnected,
    WrongTarget,
    Untrusted,
    Stale,
    Persistence,
    Credential,
    Transport,
    NotGranted,
    Conflict,
}
pub(crate) enum RemoteDeviceOutcome {
    ObservationUnknown {
        correlation: RemoteDeviceCorrelation,
    },
    Candidate {
        request_id: u64,
        candidate: RemoteDeviceCandidate,
    },
    Granted {
        request_id: u64,
        device: StoredExecutionDevice,
        devices: Vec<DeviceView>,
        policies: Vec<VerifiedExecutionDevice>,
    },
    Loaded {
        workspace_id: WorkspaceId,
        devices: Vec<DeviceView>,
    },
    Snapshot {
        workspace_id: WorkspaceId,
        devices: Vec<DeviceView>,
        receipts: Vec<RemoteLaunchReceipt>,
        policies: Vec<VerifiedExecutionDevice>,
        inputs: Vec<RemoteInputReceipt>,
        sessions: Vec<RemoteSessionObservation>,
    },
    Revoked {
        request_id: u64,
        device: StoredExecutionDevice,
    },
    LaunchPending {
        request_id: u64,
        lease: ExecutionLease,
        verified: VerifiedExecutionLease,
    },
    LaunchReceipt(RemoteLaunchReceipt),
    CloseReceipt(RemoteCloseReceipt),
    SessionObserved {
        request_id: u64,
        observation: Box<RemoteSessionObservation>,
    },
    ExecutionObserved {
        request_id: u64,
        lease: ExecutionLease,
        execution: Option<Box<RemoteExecutionProof>>,
        verified: Option<VerifiedExecutionLease>,
    },
    InputPending {
        request_id: u64,
        lease: ExecutionLease,
        operation_id: OperationId,
    },
    InputReceipt(RemoteInputReceipt),
    Failed {
        request_id: u64,
        code: RemoteDeviceError,
    },
}
/// Read-only evidence from one exact trusted remote execution; status never proves task success.
#[derive(Debug, Clone)]
pub(crate) struct RemoteSessionObservation {
    pub lease: ExecutionLease,
    pub session: deppy_core::SessionId,
    pub execution_generation: u64,
    pub terminal: Option<Arc<terminal::TerminalViewportSnapshot>>,
    pub detected_status: Option<runtime::SessionStatus>,
    pub exit_code: Option<Option<u32>>,
    pub connected: bool,
}
#[derive(Debug, Clone)]
pub(crate) struct RemoteLaunchReceipt {
    pub request_id: u64,
    pub lease: ExecutionLease,
    pub result: RemoteWorkResult,
}
#[derive(Debug, Clone)]
pub(crate) struct RemoteCloseReceipt {
    pub request_id: u64,
    pub lease: ExecutionLease,
    pub operation_id: OperationId,
    pub result: RemoteSessionCloseResult,
}
#[derive(Debug, Clone)]
pub(crate) struct RemoteInputAcceptance {
    pub sequence: u64,
    pub at_micros: i64,
}
#[derive(Debug, Clone)]
pub(crate) struct RemoteInputReceipt {
    pub request_id: u64,
    pub lease: ExecutionLease,
    pub operation_id: OperationId,
    pub result: RemoteTaskInputResult,
    pub accepted: Option<RemoteInputAcceptance>,
}
#[derive(Debug, Clone)]
pub(crate) struct RemoteDeviceCorrelation {
    pub request_id: u64,
    pub workspace_id: Option<WorkspaceId>,
    pub device_id: Option<String>,
    pub attempt_id: Option<AttemptId>,
    pub operation_id: Option<OperationId>,
    pub candidate_id: Option<String>,
}
pub(crate) struct RemoteDeviceWorker {
    worker: LazyBoundedWorker<RemoteDeviceJob, RemoteDeviceOutcome>,
    pending: Option<RemoteDeviceCorrelation>,
    candidate_correlations: Vec<(String, WorkspaceId, String)>,
}
impl RemoteDeviceWorker {
    pub(crate) fn new(
        data_dir: PathBuf,
        secrets: Arc<dyn SecretStore>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            worker: LazyBoundedWorker::new(
                "command-center-devices",
                Duration::from_secs(300),
                move || {
                    let mut state = ControllerState::new(data_dir.clone(), Arc::clone(&secrets));
                    move |job| state.execute(job)
                },
                wake,
            ),
            pending: None,
            candidate_correlations: Vec::new(),
        }
    }
    pub(crate) fn try_submit(
        &mut self,
        job: RemoteDeviceJob,
    ) -> Result<(), Box<LazyWorkerSubmitError<RemoteDeviceJob>>> {
        let correlation = match &job {
            RemoteDeviceJob::Inspect { setup } => RemoteDeviceCorrelation {
                request_id: setup.request_id,
                workspace_id: Some(setup.workspace_id.clone()),
                device_id: None,
                attempt_id: None,
                operation_id: None,
                candidate_id: None,
            },
            RemoteDeviceJob::Grant {
                request_id,
                candidate_id,
                ..
            } => RemoteDeviceCorrelation {
                request_id: *request_id,
                workspace_id: self
                    .candidate_correlations
                    .iter()
                    .find(|c| c.0 == *candidate_id)
                    .map(|c| c.1.clone()),
                device_id: self
                    .candidate_correlations
                    .iter()
                    .find(|c| c.0 == *candidate_id)
                    .map(|c| c.2.clone()),
                attempt_id: None,
                operation_id: None,
                candidate_id: Some(candidate_id.clone()),
            },
            RemoteDeviceJob::ObserveSession {
                request_id, lease, ..
            }
            | RemoteDeviceJob::ObserveExecution {
                request_id, lease, ..
            }
            | RemoteDeviceJob::DeliverInput {
                request_id, lease, ..
            }
            | RemoteDeviceJob::CloseSession {
                request_id, lease, ..
            }
            | RemoteDeviceJob::ReconcileLaunch { request_id, lease }
            | RemoteDeviceJob::ReconcileInput {
                request_id, lease, ..
            } => RemoteDeviceCorrelation {
                request_id: *request_id,
                workspace_id: Some(lease.request.workspace_id.clone()),
                device_id: Some(lease.request.device_id.clone()),
                attempt_id: Some(lease.request.attempt_id.clone()),
                operation_id: Some(match &job {
                    RemoteDeviceJob::DeliverInput { operation_id, .. }
                    | RemoteDeviceJob::CloseSession { operation_id, .. }
                    | RemoteDeviceJob::ReconcileInput { operation_id, .. } => operation_id.clone(),
                    _ => lease.request.operation_id.clone(),
                }),
                candidate_id: None,
            },
            RemoteDeviceJob::ReserveLaunch {
                request_id,
                request,
                ..
            } => RemoteDeviceCorrelation {
                request_id: *request_id,
                workspace_id: Some(request.workspace_id.clone()),
                device_id: Some(request.device_id.clone()),
                attempt_id: Some(request.attempt_id.clone()),
                operation_id: Some(request.operation_id.clone()),
                candidate_id: None,
            },
            RemoteDeviceJob::Revoke {
                request_id,
                workspace_id,
                device_id,
            } => RemoteDeviceCorrelation {
                request_id: *request_id,
                workspace_id: Some(workspace_id.clone()),
                device_id: Some(device_id.clone()),
                attempt_id: None,
                operation_id: None,
                candidate_id: None,
            },
            RemoteDeviceJob::Load { workspace_id } | RemoteDeviceJob::Poll { workspace_id } => {
                RemoteDeviceCorrelation {
                    request_id: 0,
                    workspace_id: Some(workspace_id.clone()),
                    device_id: None,
                    attempt_id: None,
                    operation_id: None,
                    candidate_id: None,
                }
            }
        };
        self.worker.try_request(job).map_err(Box::new)?;
        self.pending = Some(correlation);
        Ok(())
    }
    pub(crate) fn try_recv(&mut self) -> Option<RemoteDeviceOutcome> {
        let outcome = self.worker.try_recv()?;
        let pending = self.pending.take()?;
        let outcome = outcome
            .into_result()
            .unwrap_or(RemoteDeviceOutcome::ObservationUnknown {
                correlation: pending,
            });
        if let RemoteDeviceOutcome::Candidate { candidate, .. } = &outcome {
            if self.candidate_correlations.len() >= 32 {
                self.candidate_correlations.remove(0);
            }
            self.candidate_correlations.push((
                candidate.candidate_id.clone(),
                candidate.workspace_id.clone(),
                candidate.device_id.clone(),
            ));
        }
        Some(outcome)
    }
}
#[path = "remote_controller_worker.rs"]
mod executor;
use executor::ControllerState;

#[cfg(all(test, unix))]
mod tests {
    use super::super::remote_execution::journal::tls_tests::worker_for_workspace;
    use super::*;
    use deppy_core::work_management::{AgentSelection, AttemptId, OperationId};
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
    fn native_authority(operation: &OperationId) -> RemoteSendAuthority {
        RemoteSendAuthority::new(
            operation.clone(),
            Instant::now() + Duration::from_secs(10),
            |action| action(),
        )
        .unwrap()
    }
    fn outcome(worker: &mut RemoteDeviceWorker) -> RemoteDeviceOutcome {
        let until = Instant::now() + Duration::from_secs(8);
        loop {
            if let Some(outcome) = worker.try_recv() {
                return outcome;
            }
            assert!(Instant::now() < until, "controller worker stalled");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn remote_controller_actual_two_tls_workers_require_explicit_durable_grants_and_do_not_replay()
    {
        let root =
            std::env::temp_dir().join(format!("deppy-remote-controller-{}", uuid::Uuid::new_v4()));
        let a_dir = root.join("a");
        let b_dir = root.join("b");
        let data_dir = root.join("controller");
        let workspace = WorkspaceId("shared-project".into());
        let (a, a_pin) = worker_for_workspace(&a_dir, "a", &workspace.0);
        let (b, b_pin) = worker_for_workspace(&b_dir, "b", &workspace.0);
        let secrets = Arc::new(Secrets::default());
        let mut worker = RemoteDeviceWorker::new(data_dir.clone(), secrets.clone(), || {});
        assert!(!data_dir.exists());
        for (id, server, pin, folder) in [(1, &a, a_pin, &a_dir), (2, &b, b_pin, &b_dir)] {
            worker
                .try_submit(RemoteDeviceJob::Inspect {
                    setup: RemoteDeviceSetup {
                        request_id: id,
                        name: format!("Fixture {id}"),
                        endpoint: server.local_addr(),
                        pinned_fingerprint: pin,
                        workspace_id: workspace.clone(),
                        expected_folder: std::fs::canonicalize(folder)
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .into(),
                        auth_token: SecretString::new(server.auth_token().into()),
                    },
                })
                .unwrap();
            let RemoteDeviceOutcome::Candidate { candidate, .. } = outcome(&mut worker) else {
                panic!("trusted TLS inspection did not produce candidate")
            };
            assert!(
                secrets.0.lock().unwrap().is_empty() || id == 2,
                "candidate must not write keychain"
            );
            worker
                .try_submit(RemoteDeviceJob::Grant {
                    request_id: id,
                    candidate_id: candidate.candidate_id,
                    allowed_profile_ids: vec!["fixture-profile".into()],
                    max_workers: 2,
                })
                .unwrap();
            assert!(matches!(
                outcome(&mut worker),
                RemoteDeviceOutcome::Granted { .. }
            ));
        }
        let denied = LeaseRequest {
            device_id: "a".into(),
            workspace_id: workspace.clone(),
            attempt_id: AttemptId("denied-attempt".into()),
            operation_id: OperationId("denied-operation".into()),
            folder: std::fs::canonicalize(&a_dir)
                .unwrap()
                .to_str()
                .unwrap()
                .into(),
            agent: AgentSelection {
                provider: "fixture".into(),
                model: None,
                effort: None,
            },
        };
        worker
            .try_submit(RemoteDeviceJob::ReserveLaunch {
                request_id: 9,
                authority: RemoteSendAuthority::new(
                    denied.operation_id.clone(),
                    Instant::now() + Duration::from_secs(10),
                    |_| {},
                )
                .unwrap(),
                request: denied,
                initial_prompt: None,
            })
            .unwrap();
        assert!(matches!(
            outcome(&mut worker),
            RemoteDeviceOutcome::LaunchReceipt(RemoteLaunchReceipt {
                result: RemoteWorkResult::Rejected {
                    code: runtime::remote_worker::RemoteWorkError::Disabled
                },
                ..
            })
        ));
        assert!(
            !a_dir.join("launches").exists(),
            "revoked delayed authorization must not enqueue a spawn"
        );
        let mut requests = vec![];
        for (id, folder, device) in [(10, &a_dir, "a"), (20, &b_dir, "b")] {
            let request = LeaseRequest {
                device_id: device.into(),
                workspace_id: workspace.clone(),
                attempt_id: AttemptId(format!("attempt-{id}")),
                operation_id: OperationId(format!("op-{id}")),
                folder: std::fs::canonicalize(folder)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .into(),
                agent: AgentSelection {
                    provider: "fixture".into(),
                    model: None,
                    effort: None,
                },
            };
            worker
                .try_submit(RemoteDeviceJob::ReserveLaunch {
                    request_id: id,
                    request: request.clone(),
                    initial_prompt: None,
                    authority: native_authority(&request.operation_id),
                })
                .unwrap();
            assert!(matches!(
                outcome(&mut worker),
                RemoteDeviceOutcome::LaunchPending { .. }
            ));
            requests.push((id, request));
        }
        let until = Instant::now() + Duration::from_secs(8);
        let mut receipts = vec![];
        while receipts.len() < 2 {
            worker
                .try_submit(RemoteDeviceJob::Poll {
                    workspace_id: workspace.clone(),
                })
                .unwrap();
            if let RemoteDeviceOutcome::Snapshot { receipts: new, .. } = outcome(&mut worker) {
                receipts.extend(new);
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(receipts.iter().all(|r| matches!(
            r.result,
            RemoteWorkResult::Launched {
                session: deppy_core::SessionId(1),
                ..
            }
        )));
        assert_ne!(receipts[0].lease.incarnation, receipts[1].lease.incarnation);
        worker
            .try_submit(RemoteDeviceJob::ReserveLaunch {
                request_id: 10,
                request: requests[0].1.clone(),
                initial_prompt: None,
                authority: native_authority(&requests[0].1.operation_id),
            })
            .unwrap();
        assert!(matches!(
            outcome(&mut worker),
            RemoteDeviceOutcome::LaunchReceipt(RemoteLaunchReceipt {
                result: RemoteWorkResult::Launched { .. },
                ..
            })
        ));
        for folder in [&a_dir, &b_dir] {
            assert_eq!(
                std::fs::read_to_string(folder.join("launches")).unwrap(),
                "launch"
            );
        }
        worker
            .try_submit(RemoteDeviceJob::Revoke {
                request_id: 30,
                workspace_id: workspace.clone(),
                device_id: "a".into(),
            })
            .unwrap();
        assert!(matches!(
            outcome(&mut worker),
            RemoteDeviceOutcome::Revoked { .. }
        ));
        worker
            .try_submit(RemoteDeviceJob::ReserveLaunch {
                request_id: 10,
                request: requests[0].1.clone(),
                initial_prompt: None,
                authority: native_authority(&requests[0].1.operation_id),
            })
            .unwrap();
        assert!(
            matches!(
                outcome(&mut worker),
                RemoteDeviceOutcome::LaunchReceipt(RemoteLaunchReceipt {
                    result: RemoteWorkResult::ObservationUnknown,
                    ..
                })
            ),
            "revocation must disarm an already launched observation without replay"
        );
        drop(worker);
        let mut restored = RemoteDeviceWorker::new(data_dir.clone(), secrets, || {});
        restored
            .try_submit(RemoteDeviceJob::Load {
                workspace_id: workspace.clone(),
            })
            .unwrap();
        let RemoteDeviceOutcome::Loaded { devices, .. } = outcome(&mut restored) else {
            panic!("stored registry not hydrated")
        };
        assert_eq!(devices.len(), 2);
        assert!(devices.iter().all(|d| !d.can_execute));
        restored
            .try_submit(RemoteDeviceJob::ReserveLaunch {
                request_id: 10,
                request: requests[0].1.clone(),
                initial_prompt: None,
                authority: native_authority(&requests[0].1.operation_id),
            })
            .unwrap();
        assert!(matches!(
            outcome(&mut restored),
            RemoteDeviceOutcome::Failed { .. }
                | RemoteDeviceOutcome::LaunchReceipt(RemoteLaunchReceipt {
                    result: RemoteWorkResult::ObservationUnknown,
                    ..
                })
        ));
        drop(restored);
        a.shutdown();
        b.shutdown();
        std::fs::remove_dir_all(root).unwrap();
    }
    struct PromptLauncher;
    impl runtime::remote_worker::RemoteWorkLauncher for PromptLauncher {
        fn prepare(
            &self,
            profile: &runtime::remote_worker::RemoteWorkerProfile,
            prompt: Option<&str>,
        ) -> anyhow::Result<RuntimeCommand> {
            let mut command = super::super::remote_execution::journal::tls_tests::Launcher
                .prepare(profile, prompt)?;
            if let RuntimeCommand::SpawnAgent { args, .. } = &mut command {
                *args=vec!["-c".into(),"stty raw -echo; printf '\\033[?2004h❯ '; dd bs=1 count=25 of=accepted 2>/dev/null; sleep 2".into()];
            }
            Ok(command)
        }
    }
    #[test]
    fn remote_controller_delivers_only_captured_guarded_input_and_preserves_actual_receipt() {
        use super::super::remote_execution::journal::tls_tests::worker_for_workspace_with_launcher;
        let root =
            std::env::temp_dir().join(format!("deppy-controller-input-{}", uuid::Uuid::new_v4()));
        let workspace = WorkspaceId("workspace-input".into());
        let agent = AgentSelection {
            provider: "claude".into(),
            model: None,
            effort: None,
        };
        let (server, pin) = worker_for_workspace_with_launcher(
            &root.join("worker"),
            "input-device",
            &workspace.0,
            Arc::new(PromptLauncher),
            agent.clone(),
        );
        let mut worker =
            RemoteDeviceWorker::new(root.join("controller"), Arc::new(Secrets::default()), || {});
        let folder = std::fs::canonicalize(root.join("worker"))
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        worker
            .try_submit(RemoteDeviceJob::Inspect {
                setup: RemoteDeviceSetup {
                    request_id: 1,
                    name: "Input worker".into(),
                    endpoint: server.local_addr(),
                    pinned_fingerprint: pin,
                    workspace_id: workspace.clone(),
                    expected_folder: folder.clone(),
                    auth_token: SecretString::new(server.auth_token().into()),
                },
            })
            .unwrap();
        let RemoteDeviceOutcome::Candidate { candidate, .. } = outcome(&mut worker) else {
            panic!("candidate missing")
        };
        worker
            .try_submit(RemoteDeviceJob::Grant {
                request_id: 2,
                candidate_id: candidate.candidate_id,
                allowed_profile_ids: vec!["fixture-profile".into()],
                max_workers: 2,
            })
            .unwrap();
        assert!(matches!(
            outcome(&mut worker),
            RemoteDeviceOutcome::Granted { .. }
        ));
        let request = LeaseRequest {
            device_id: "input-device".into(),
            workspace_id: workspace.clone(),
            attempt_id: AttemptId::new(),
            operation_id: OperationId::new(),
            folder,
            agent,
        };
        worker
            .try_submit(RemoteDeviceJob::ReserveLaunch {
                request_id: 3,
                authority: native_authority(&request.operation_id),
                request,
                initial_prompt: None,
            })
            .unwrap();
        let RemoteDeviceOutcome::LaunchPending {
            lease, verified, ..
        } = outcome(&mut worker)
        else {
            panic!("launch not pending")
        };
        let until = Instant::now() + Duration::from_secs(5);
        let (session, execution_generation) = loop {
            worker
                .try_submit(RemoteDeviceJob::Poll {
                    workspace_id: workspace.clone(),
                })
                .unwrap();
            if let RemoteDeviceOutcome::Snapshot { receipts, .. } = outcome(&mut worker)
                && let Some(RemoteLaunchReceipt {
                    result:
                        RemoteWorkResult::Launched {
                            session,
                            execution_generation,
                        },
                    ..
                }) = receipts.first()
            {
                break (*session, *execution_generation);
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        };
        let execution = loop {
            worker
                .try_submit(RemoteDeviceJob::ObserveExecution {
                    request_id: 4,
                    lease: lease.clone(),
                    session,
                    execution_generation,
                })
                .unwrap();
            if let RemoteDeviceOutcome::ExecutionObserved {
                request_id: 4,
                execution: Some(proof),
                verified: Some(verified),
                ..
            } = outcome(&mut worker)
            {
                assert_eq!(verified.lease(), &lease);
                assert!(verified.validate_fresh().is_ok());
                break proof;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        };
        worker
            .try_submit(RemoteDeviceJob::ObserveSession {
                request_id: 41,
                lease: lease.clone(),
                session,
                execution_generation,
            })
            .unwrap();
        let RemoteDeviceOutcome::SessionObserved {
            request_id: 41,
            observation,
        } = outcome(&mut worker)
        else {
            panic!("no exact read-only session observation");
        };
        assert_eq!(observation.lease, lease);
        assert_eq!(observation.session, session);
        assert_eq!(observation.execution_generation, execution_generation);
        assert!(observation.connected);
        let terminal = observation
            .terminal
            .expect("real TLS worker must return its actual terminal");
        assert!(terminal.history.is_some_and(|h| h.generation > 0));
        assert!(terminal.visible_cells.iter().any(|cell| cell.c == '❯'));
        let input_operation = OperationId::new();
        worker
            .try_submit(RemoteDeviceJob::DeliverInput {
                request_id: 5,
                lease: lease.clone(),
                execution: execution.clone(),
                operation_id: input_operation.clone(),
                prompt: "bounded task".into(),
                authority: native_authority(&input_operation),
            })
            .unwrap();
        assert!(matches!(
            outcome(&mut worker),
            RemoteDeviceOutcome::InputPending { .. }
        ));
        let receipt = loop {
            worker
                .try_submit(RemoteDeviceJob::Poll {
                    workspace_id: workspace.clone(),
                })
                .unwrap();
            if let RemoteDeviceOutcome::Snapshot { inputs, .. } = outcome(&mut worker)
                && let Some(receipt) = inputs.first()
            {
                break receipt.clone();
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(receipt.request_id, 5);
        assert_eq!(receipt.operation_id, input_operation);
        assert_eq!(receipt.result, RemoteTaskInputResult::Admitted);
        assert_eq!(receipt.accepted.as_ref().unwrap().sequence, 1);
        assert!(receipt.accepted.as_ref().unwrap().at_micros > 0);
        worker
            .try_submit(RemoteDeviceJob::DeliverInput {
                request_id: 6,
                lease: lease.clone(),
                execution: execution.clone(),
                operation_id: input_operation.clone(),
                prompt: "bounded task".into(),
                authority: native_authority(&input_operation),
            })
            .unwrap();
        assert!(matches!(
            outcome(&mut worker),
            RemoteDeviceOutcome::InputReceipt(RemoteInputReceipt {
                result: RemoteTaskInputResult::Admitted,
                ..
            })
        ));
        assert_eq!(
            std::fs::read(root.join("worker/accepted")).unwrap(),
            b"\x1b[200~bounded task\x1b[201~\r"
        );
        let close_operation = OperationId::new();
        worker
            .try_submit(RemoteDeviceJob::CloseSession {
                request_id: 6,
                lease: lease.clone(),
                execution: execution.clone(),
                operation_id: close_operation.clone(),
                authority: native_authority(&close_operation),
            })
            .unwrap();
        assert!(matches!(
            outcome(&mut worker),
            RemoteDeviceOutcome::CloseReceipt(RemoteCloseReceipt {
                result: RemoteSessionCloseResult::Closed,
                ..
            })
        ));
        worker
            .try_submit(RemoteDeviceJob::Poll {
                workspace_id: workspace.clone(),
            })
            .unwrap();
        let RemoteDeviceOutcome::Snapshot {
            devices, sessions, ..
        } = outcome(&mut worker)
        else {
            panic!("no closed snapshot");
        };
        assert!(
            devices
                .iter()
                .any(|d| d.id == lease.request.device_id && d.can_execute)
        );
        assert!(
            sessions
                .iter()
                .any(|s| s.lease == lease && s.exit_code == Some(None))
        );
        drop(worker);
        let until = Instant::now() + Duration::from_secs(2);
        while verified.validate_fresh().is_ok() {
            assert!(
                Instant::now() < until,
                "dropping route owner must revoke proof clones"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        server.shutdown();
        std::fs::remove_dir_all(root).unwrap();
    }
}
