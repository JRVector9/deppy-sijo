//! Controlled processes only: actual pinned TLS + async SQLite admission through public ports.
use super::*;
use deppy_core::WorkspaceId;
use deppy_core::work_management::{AgentSelection, AttemptId, OperationId};
use runtime::remote_worker::{
    RemoteWorkIdentity, RemoteWorkLauncher, RemoteWorkerCapabilities, RemoteWorkerConfig,
    RemoteWorkerHost, RemoteWorkerProfile,
};
use runtime::{
    InProcessRuntimeClient, RemoteRuntimeClient, RemoteRuntimeServer, RuntimeCommand,
    RuntimeCommandSink, RuntimeEvent, RuntimeEventReceiver, RuntimeEventStream, RuntimeSecret,
    RuntimeSecretResolver,
};
use secret::{RedactionService, SecretStore, SecretString};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
struct MemorySecrets(Mutex<HashMap<String, String>>);
impl SecretStore for MemorySecrets {
    fn set_secret(&self, id: &str, value: &SecretString) -> anyhow::Result<()> {
        self.0
            .lock()
            .unwrap()
            .insert(id.into(), value.expose().into());
        Ok(())
    }
    fn get_secret(&self, id: &str) -> anyhow::Result<SecretString> {
        self.0
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .map(SecretString::new)
            .ok_or_else(|| anyhow::anyhow!("fixture secret missing"))
    }
    fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
        self.0.lock().unwrap().remove(id);
        Ok(())
    }
    fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
        Ok(self.0.lock().unwrap().contains_key(id))
    }
}
struct Resolver;
impl RuntimeSecretResolver for Resolver {
    fn resolve(&self, _: &str) -> anyhow::Result<RuntimeSecret> {
        anyhow::bail!("fixture has no launch secrets")
    }
}
pub(crate) struct Launcher;
impl RemoteWorkLauncher for Launcher {
    fn prepare(
        &self,
        _: &RemoteWorkerProfile,
        prompt: Option<&str>,
    ) -> anyhow::Result<RuntimeCommand> {
        anyhow::ensure!(prompt.is_none(), "fixture only permits idle launch");
        Ok(RuntimeCommand::SpawnAgent {
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
            agent_config_id: None,
            command: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "pwd > actual-cwd; printf launch >> launches; sleep 2".into(),
            ],
            env_plain: vec![],
            env_secrets: vec![],
            waiting_regex: None,
            approval_regex: None,
            error_regex: None,
            done_regex: None,
            transient_args: vec![],
        })
    }
}
fn worker(root: &Path, id: &str) -> (RemoteRuntimeServer, String) {
    worker_for_workspace(root, id, &format!("workspace-{id}"))
}
pub(crate) fn worker_for_workspace(
    root: &Path,
    id: &str,
    workspace: &str,
) -> (RemoteRuntimeServer, String) {
    worker_for_workspace_with_launcher(
        root,
        id,
        workspace,
        Arc::new(Launcher),
        AgentSelection {
            provider: "fixture".into(),
            model: None,
            effort: None,
        },
    )
}
pub(crate) fn worker_for_workspace_with_launcher(
    root: &Path,
    id: &str,
    workspace: &str,
    launcher: Arc<dyn RemoteWorkLauncher>,
    agent: AgentSelection,
) -> (RemoteRuntimeServer, String) {
    let (server, pin, _) = worker_for_workspace_scoped(root, id, workspace, launcher, agent);
    (server, pin)
}
pub(crate) fn worker_for_workspace_scoped(
    root: &Path,
    id: &str,
    workspace: &str,
    launcher: Arc<dyn RemoteWorkLauncher>,
    agent: AgentSelection,
) -> (RemoteRuntimeServer, String, SecretString) {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(root.join("metadata.sqlite3"), b"legacy untouched").unwrap();
    let backend = InProcessRuntimeClient::try_new_with_resolver(
        4,
        Arc::new(Resolver),
        root.join("logs"),
        RedactionService::new(),
        None,
        None,
        vec![],
    )
    .unwrap();
    let wake = backend.remote_worker_wake();
    let journal = Arc::new(AsyncRemoteJournal::new(root.to_path_buf(), move || {
        if let Some(wake) = &wake {
            wake();
        }
    }));
    backend
        .configure_remote_worker(Some(
            RemoteWorkerHost::new(
                RemoteWorkerConfig {
                    device_id: id.into(),
                    workspace_id: WorkspaceId(workspace.into()),
                    folder: root.to_path_buf(),
                    grant_epoch: 1,
                    max_workers: 2,
                    profiles: vec![RemoteWorkerProfile {
                        id: "fixture-profile".into(),
                        agent,
                    }],
                },
                launcher,
                journal,
            )
            .unwrap(),
        ))
        .unwrap();
    let token = backend
        .remote_worker_control()
        .issue_scoped_token(id, &WorkspaceId(workspace.into()), 1)
        .unwrap();
    let identity = runtime::tls_identity::get_or_create_identity(
        &MemorySecrets::default(),
        &root.join("fixture-cert.der"),
    )
    .unwrap();
    let pin = identity.fingerprint();
    (
        RemoteRuntimeServer::serve_tls(backend, "127.0.0.1:0".parse().unwrap(), identity, false)
            .unwrap(),
        pin,
        token,
    )
}
fn event(rx: &RuntimeEventReceiver, accept: impl Fn(&RuntimeEvent) -> bool) -> RuntimeEvent {
    let until = Instant::now() + Duration::from_secs(8);
    loop {
        for event in rx.drain() {
            if accept(&event) {
                return event;
            }
        }
        assert!(Instant::now() < until, "TLS fixture timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn request(
    client: &RemoteRuntimeClient,
    rx: &RuntimeEventReceiver,
) -> (RemoteWorkerCapabilities, RemoteWorkRequest) {
    client
        .send_command(RuntimeCommand::InspectRemoteWorker { request_id: 1 })
        .unwrap();
    let RuntimeEvent::RemoteWorkerObserved {
        capabilities: Some(caps),
        ..
    } = event(rx, |e| {
        matches!(e, RuntimeEvent::RemoteWorkerObserved { request_id: 1, .. })
    })
    else {
        panic!("fixture compute not granted")
    };
    let request = RemoteWorkRequest {
        identity: RemoteWorkIdentity {
            device_id: caps.device_id.clone(),
            workspace_id: caps.workspace_id.clone(),
            attempt_id: AttemptId("fixture-attempt".into()),
            operation_id: OperationId("fixture-op".into()),
            incarnation: caps.incarnation.clone(),
            grant_epoch: caps.grant_epoch,
            lease_epoch: caps.lease_epoch,
        },
        folder: caps.folder.clone(),
        folder_anchor: caps.folder_anchor.clone(),
        profile_id: caps.profiles[0].id.clone(),
        initial_prompt: None,
    };
    (caps, request)
}
#[test]
fn async_remote_journal_two_tls_workers_durably_deduplicate_lost_reply() {
    let root = std::env::temp_dir().join(format!("deppy-journal-tls-{}", uuid::Uuid::new_v4()));
    let a_root = root.join("a");
    let b_root = root.join("b");
    let (a, a_pin) = worker(&a_root, "a");
    let (b, b_pin) = worker(&b_root, "b");
    assert!(RemoteRuntimeClient::attach_tls(a.local_addr(), a.auth_token(), &b_pin).is_err());
    let a_client = RemoteRuntimeClient::attach_tls(a.local_addr(), a.auth_token(), &a_pin).unwrap();
    let a_rx = a_client.subscribe();
    let b_client = RemoteRuntimeClient::attach_tls(b.local_addr(), b.auth_token(), &b_pin).unwrap();
    let b_rx = b_client.subscribe();
    let (a_caps, a_request) = request(&a_client, &a_rx);
    let (b_caps, b_request) = request(&b_client, &b_rx);
    assert_ne!(a_caps.incarnation, b_caps.incarnation);
    b_client
        .send_command(RuntimeCommand::SpawnRemoteWork {
            request: a_request.clone(),
        })
        .unwrap();
    assert!(matches!(
        event(&b_rx, |e| matches!(
            e,
            RuntimeEvent::RemoteWorkResolved { .. }
        )),
        RuntimeEvent::RemoteWorkResolved {
            result: RemoteWorkResult::Rejected {
                code: runtime::remote_worker::RemoteWorkError::WrongWorker
            },
            ..
        }
    ));
    a_client
        .send_command(RuntimeCommand::SpawnRemoteWork {
            request: a_request.clone(),
        })
        .unwrap();
    let until = Instant::now() + Duration::from_secs(8);
    while !a_root.join("launches").exists() {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    }
    // Drop the response receiver before reading any admission result.
    drop(a_rx);
    drop(a_client);
    let reconnected =
        RemoteRuntimeClient::attach_tls(a.local_addr(), a.auth_token(), &a_pin).unwrap();
    let reply = reconnected.subscribe();
    reconnected
        .send_command(RuntimeCommand::SpawnRemoteWork {
            request: a_request.clone(),
        })
        .unwrap();
    let RuntimeEvent::RemoteWorkResolved {
        result:
            a_result @ RemoteWorkResult::Launched {
                session: deppy_core::SessionId(1),
                ..
            },
        ..
    } = event(&reply, |e| {
        matches!(e, RuntimeEvent::RemoteWorkResolved { .. })
    })
    else {
        panic!("exact replay did not return durable original")
    };
    b_client
        .send_command(RuntimeCommand::SpawnRemoteWork {
            request: b_request.clone(),
        })
        .unwrap();
    let RuntimeEvent::RemoteWorkResolved {
        result:
            b_result @ RemoteWorkResult::Launched {
                session: deppy_core::SessionId(1),
                ..
            },
        ..
    } = event(&b_rx, |e| {
        matches!(e, RuntimeEvent::RemoteWorkResolved { .. })
    })
    else {
        panic!("worker B did not launch its own operation")
    };
    for (directory, request, result) in [
        (&a_root, &a_request, a_result),
        (&b_root, &b_request, b_result),
    ] {
        assert_eq!(
            std::fs::read_to_string(directory.join("launches")).unwrap(),
            "launch"
        );
        assert_eq!(
            std::fs::read_to_string(directory.join("actual-cwd"))
                .unwrap()
                .trim(),
            std::fs::canonicalize(directory).unwrap().to_str().unwrap()
        );
        assert_eq!(
            std::fs::read(directory.join("metadata.sqlite3")).unwrap(),
            b"legacy untouched"
        );
        let mut store = CommandCenterStore::open(directory).unwrap();
        assert_eq!(
            store
                .claim_remote_operation(&identity(request).unwrap())
                .unwrap(),
            RemoteOperationClaim::Existing(to_stored(&result).unwrap())
        );
    }
    drop(reconnected);
    drop(b_client);
    a.shutdown();
    b.shutdown();
    let restored = AsyncRemoteJournal::new(a_root.clone(), || {});
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        match restored.claim(&a_request).unwrap() {
            RemoteWorkClaim::Pending => {}
            RemoteWorkClaim::Existing(RemoteWorkResult::Launched {
                session: deppy_core::SessionId(1),
                ..
            }) => break,
            other => panic!("restored admission replayed: {other:?}"),
        };
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(restored);
    std::fs::remove_dir_all(root).unwrap();
}
