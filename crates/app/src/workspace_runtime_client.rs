//! Workspace execution ownership. Offline SSH targets never construct a local runtime.
use runtime::{
    InProcessRuntimeClient, InputAdmission, InputReplyProbe, RuntimeClient, RuntimeCommand,
    RuntimeCommandSendError, RuntimeCommandSink, RuntimeEventReceiver, RuntimeEventStream,
    SessionId, SshRuntimeClient,
};
use std::sync::{Arc, Mutex, mpsc::Receiver};
pub enum WorkspaceRuntimeClient {
    Local(InProcessRuntimeClient),
    Remote {
        client: Arc<SshRuntimeClient>,
        token: String,
        closing: Arc<Mutex<bool>>,
    },
    Offline {
        token: String,
    },
}
impl From<InProcessRuntimeClient> for WorkspaceRuntimeClient {
    fn from(client: InProcessRuntimeClient) -> Self {
        Self::Local(client)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteRuntimeUnsupported {
    InspectScreen,
    InputReply,
    WorkspaceClosing,
    CommandCenterExecution,
}
impl std::fmt::Display for RemoteRuntimeUnsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InspectScreen => "ssh_remote_screen_inspection_unsupported",
            Self::InputReply => "ssh_remote_input_reply_unsupported",
            Self::WorkspaceClosing => "ssh_remote_workspace_closing",
            Self::CommandCenterExecution => "ssh_remote_command_center_execution_unsupported",
        })
    }
}
impl std::error::Error for RemoteRuntimeUnsupported {}
impl WorkspaceRuntimeClient {
    pub fn remote(client: Arc<SshRuntimeClient>) -> Self {
        Self::Remote {
            client,
            token: format!("ssh-attachment-{}", uuid::Uuid::new_v4()),
            closing: Arc::new(Mutex::new(false)),
        }
    }
    /// Wire replies carry the persistent server runtime, independently of UI ownership.
    pub fn remote_rpc_token(&self) -> Option<&str> {
        match self {
            Self::Remote { client, .. } => Some(client.instance_token()),
            _ => None,
        }
    }
    pub fn offline() -> Self {
        Self::Offline {
            token: format!("offline-{}", uuid::Uuid::new_v4()),
        }
    }
    /// Offline represents an SSH target too; callers must keep local filesystem fences active.
    pub fn is_remote(&self) -> bool {
        !matches!(self, Self::Local(_))
    }
    /// CC host authority belongs only to an in-process local worker. An SSH
    /// attachment must never create or borrow a worker on this Mac.
    pub fn remote_worker_control(
        &self,
    ) -> Result<runtime::remote_worker::RemoteWorkerControl, RemoteRuntimeUnsupported> {
        match self {
            Self::Local(client) => Ok(client.remote_worker_control()),
            Self::Remote { .. } | Self::Offline { .. } => {
                Err(RemoteRuntimeUnsupported::CommandCenterExecution)
            }
        }
    }
    pub fn remote_worker_wake(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        match self {
            Self::Local(client) => client.remote_worker_wake(),
            Self::Remote { .. } | Self::Offline { .. } => None,
        }
    }
    pub fn is_connected(&self) -> bool {
        match self {
            Self::Local(client) => client.command_sink().is_some(),
            Self::Remote { client, .. } => client.is_connected(),
            Self::Offline { .. } => false,
        }
    }
    pub fn instance_token(&self) -> &str {
        match self {
            Self::Local(client) => client.instance_token(),
            Self::Remote { token, .. } => token,
            Self::Offline { token } => token,
        }
    }
    /// Serialize the close boundary with bounded queue admission, including previously
    /// captured PWA sinks. No network or process operation occurs under this lock.
    pub fn begin_remote_close(&self) {
        if let Self::Remote { closing, .. } = self {
            *closing.lock().expect("SSH closing admission lock") = true;
        }
    }
    pub fn finish_remote_close(&self) {
        if let Self::Remote { closing, .. } = self {
            *closing.lock().expect("SSH closing admission lock") = false;
        }
    }
    pub fn shutdown(&mut self) {
        match self {
            Self::Local(client) => client.shutdown(),
            Self::Remote { client, .. } => client.disconnect(),
            Self::Offline { .. } => {}
        }
    }
    pub fn command_sink(&self) -> Option<Arc<dyn Fn(RuntimeCommand) + Send + Sync>> {
        match self {
            Self::Local(client) => client.command_sink(),
            Self::Remote {
                client, closing, ..
            } => {
                if !client.is_connected() {
                    return None;
                }
                let client = Arc::clone(client);
                let closing = Arc::clone(closing);
                Some(Arc::new(move |command| {
                    let closing = closing.lock().expect("SSH closing admission lock");
                    if !*closing || allowed_during_remote_close(&command) {
                        let _ = client.send_command(command);
                    }
                }))
            }
            Self::Offline { .. } => None,
        }
    }
    pub fn send_command_owned(
        &self,
        command: RuntimeCommand,
    ) -> Result<(), (anyhow::Error, Box<RuntimeCommand>)> {
        match self {
            Self::Local(client) => client.send_command_owned(command),
            Self::Remote {
                client, closing, ..
            } => {
                let closing = closing.lock().expect("SSH closing admission lock");
                if *closing && !allowed_during_remote_close(&command) {
                    Err((
                        RemoteRuntimeUnsupported::WorkspaceClosing.into(),
                        Box::new(command),
                    ))
                } else {
                    client.send_command_owned(command)
                }
            }
            Self::Offline { .. } => Err((
                RuntimeCommandSendError::Disconnected.into(),
                Box::new(command),
            )),
        }
    }
    pub fn inspect_screen(&self, session: SessionId) -> anyhow::Result<Receiver<Option<String>>> {
        match self {
            Self::Local(client) => client.inspect_screen(session),
            Self::Remote { .. } => Err(RemoteRuntimeUnsupported::InspectScreen.into()),
            Self::Offline { .. } => Err(RuntimeCommandSendError::Disconnected.into()),
        }
    }
    pub fn send_guarded_input(
        &self,
        session: SessionId,
        operation_id: String,
        bytes: Vec<u8>,
        admission: InputAdmission,
    ) -> anyhow::Result<()> {
        match self {
            Self::Local(client) => {
                client.send_guarded_input(session, operation_id, bytes, admission)
            }
            Self::Remote {
                client, closing, ..
            } => {
                let closing = closing.lock().expect("SSH closing admission lock");
                if *closing {
                    return Err(RemoteRuntimeUnsupported::WorkspaceClosing.into());
                }
                client.send_guarded_input(session, operation_id, bytes, admission)
            }
            Self::Offline { .. } => Err(RuntimeCommandSendError::Disconnected.into()),
        }
    }
    pub fn send_guarded_input_batch(
        &self,
        session: SessionId,
        operation_id: String,
        parts: Vec<Vec<u8>>,
        admission: InputAdmission,
    ) -> anyhow::Result<()> {
        match self {
            Self::Local(client) => {
                client.send_guarded_input_batch(session, operation_id, parts, admission)
            }
            Self::Remote {
                client, closing, ..
            } => {
                let closing = closing.lock().expect("SSH closing admission lock");
                if *closing {
                    return Err(RemoteRuntimeUnsupported::WorkspaceClosing.into());
                }
                client.send_guarded_input_batch(session, operation_id, parts, admission)
            }
            Self::Offline { .. } => Err(RuntimeCommandSendError::Disconnected.into()),
        }
    }
    pub fn send_guarded_input_batch_with_reply(
        &self,
        session: SessionId,
        operation_id: String,
        parts: Vec<Vec<u8>>,
        admission: InputAdmission,
    ) -> anyhow::Result<InputReplyProbe> {
        match self {
            Self::Local(client) => {
                client.send_guarded_input_batch_with_reply(session, operation_id, parts, admission)
            }
            Self::Remote { closing, .. } => {
                if *closing.lock().expect("SSH closing admission lock") {
                    Err(RemoteRuntimeUnsupported::WorkspaceClosing.into())
                } else {
                    Err(RemoteRuntimeUnsupported::InputReply.into())
                }
            }
            Self::Offline { .. } => Err(RuntimeCommandSendError::Disconnected.into()),
        }
    }
    pub fn subscribe_with_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) -> RuntimeEventReceiver {
        match self {
            Self::Local(client) => client.subscribe_with_wake(wake),
            Self::Remote { client, .. } => client.subscribe_with_wake(wake),
            Self::Offline { .. } => RuntimeEventReceiver::disconnected(),
        }
    }
    pub fn subscribe_with_wake_background(
        &self,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> RuntimeEventReceiver {
        match self {
            Self::Local(client) => client.subscribe_with_wake_background(wake),
            Self::Remote { client, .. } => client.subscribe_with_wake_background(wake),
            Self::Offline { .. } => RuntimeEventReceiver::disconnected(),
        }
    }
}
impl RuntimeCommandSink for WorkspaceRuntimeClient {
    fn send_command(&self, command: RuntimeCommand) -> anyhow::Result<()> {
        match self {
            Self::Local(client) => client.send_command(command),
            Self::Remote { .. } => self.send_command_owned(command).map_err(|(error, _)| error),
            Self::Offline { .. } => Err(RuntimeCommandSendError::Disconnected.into()),
        }
    }
}
fn allowed_during_remote_close(command: &RuntimeCommand) -> bool {
    matches!(
        command,
        RuntimeCommand::ClosePane { .. }
            | RuntimeCommand::CloseTab { .. }
            | RuntimeCommand::KillSession { .. }
            | RuntimeCommand::RequestMuxSnapshot
            | RuntimeCommand::DurableEventBarrier { .. }
    )
}
impl RuntimeEventStream for WorkspaceRuntimeClient {
    fn subscribe(&self) -> RuntimeEventReceiver {
        match self {
            Self::Local(client) => client.subscribe(),
            Self::Remote { client, .. } => client.subscribe(),
            Self::Offline { .. } => RuntimeEventReceiver::disconnected(),
        }
    }
}
impl RuntimeClient for WorkspaceRuntimeClient {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        thread,
        time::{Duration, Instant},
    };
    struct NoSecrets(Arc<AtomicUsize>);
    impl runtime::RuntimeSecretResolver for NoSecrets {
        fn resolve(&self, _: &str) -> anyhow::Result<runtime::RuntimeSecret> {
            self.0.fetch_add(1, Ordering::AcqRel);
            anyhow::bail!("test never resolves secrets")
        }
    }
    fn local() -> (InProcessRuntimeClient, std::path::PathBuf, Arc<AtomicUsize>) {
        let root =
            std::env::temp_dir().join(format!("deppy-runtime-wrapper-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let client = InProcessRuntimeClient::try_new_with_resolver(
            5,
            Arc::new(NoSecrets(calls.clone())),
            root.clone(),
            secret::RedactionService::new(),
            None,
            None,
            vec![],
        )
        .unwrap();
        (client, root, calls)
    }
    #[test]
    fn offline_ssh_has_no_local_fallback_and_preserves_unadmitted_commands() {
        let mut client = WorkspaceRuntimeClient::offline();
        let other = WorkspaceRuntimeClient::offline();
        assert!(client.is_remote());
        assert!(!client.is_connected());
        assert_ne!(client.instance_token(), other.instance_token());
        assert!(client.command_sink().is_none());
        assert_eq!(
            client.remote_worker_control().err(),
            Some(RemoteRuntimeUnsupported::CommandCenterExecution)
        );
        assert!(client.remote_worker_wake().is_none());
        let error = client
            .send_command(RuntimeCommand::RequestMuxSnapshot)
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<RuntimeCommandSendError>(),
            Some(&RuntimeCommandSendError::Disconnected)
        );
        let (error, command) = client
            .send_command_owned(RuntimeCommand::RequestMuxSnapshot)
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<RuntimeCommandSendError>(),
            Some(&RuntimeCommandSendError::Disconnected)
        );
        assert!(matches!(*command, RuntimeCommand::RequestMuxSnapshot));
        assert!(client.subscribe().is_disconnected());
        assert!(
            client
                .subscribe_with_wake(Arc::new(|| panic!("offline has no worker wake")))
                .is_disconnected()
        );
        assert!(
            client
                .subscribe_with_wake_background(Arc::new(|| panic!("offline has no worker wake")))
                .is_disconnected()
        );
        assert!(client.inspect_screen(SessionId(1)).is_err());
        client.shutdown();
        assert!(!client.is_connected());
    }
    #[test]
    fn local_client_retains_instance_and_routes_snapshot_events_without_spawning_a_shell() {
        let (local, root, secrets) = local();
        let token = local.instance_token().to_owned();
        let mut client = WorkspaceRuntimeClient::from(local);
        assert!(!client.is_remote());
        assert!(client.is_connected());
        assert_eq!(client.instance_token(), token);
        client
            .remote_worker_control()
            .unwrap()
            .configure(None)
            .unwrap();
        assert!(client.remote_worker_wake().is_some());
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = wakes.clone();
        let events = client.subscribe_with_wake_background(Arc::new(move || {
            counter.fetch_add(1, Ordering::AcqRel);
        }));
        client.command_sink().unwrap()(RuntimeCommand::RequestMuxSnapshot);
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            if events
                .drain()
                .iter()
                .any(|event| matches!(event, runtime::RuntimeEvent::MuxUpdated { .. }))
            {
                break;
            }
            assert!(Instant::now() < until, "local snapshot delegation");
            thread::sleep(Duration::from_millis(2));
        }
        assert!(wakes.load(Ordering::Acquire) > 0);
        assert_eq!(secrets.load(Ordering::Acquire), 0);
        client.shutdown();
        assert!(!client.is_connected());
        std::fs::remove_dir_all(root).unwrap();
    }
}
#[cfg(all(test, unix))]
mod remote_tests {
    use super::*;
    use std::{
        os::unix::net::UnixStream,
        thread,
        time::{Duration, Instant},
    };
    struct NoSecrets;
    impl runtime::RuntimeSecretResolver for NoSecrets {
        fn resolve(&self, _: &str) -> anyhow::Result<runtime::RuntimeSecret> {
            anyhow::bail!("fixture has no credentials")
        }
    }
    fn admission() -> InputAdmission {
        InputAdmission::new(
            runtime::InputPermit::new(),
            Instant::now() + Duration::from_secs(2),
            |write| write(),
        )
    }
    #[test]
    fn remote_wrapper_delegates_runtime_but_rejects_local_inspection_and_automatic_input() {
        let root =
            std::env::temp_dir().join(format!("deppy-remote-wrapper-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let backend = InProcessRuntimeClient::try_new_with_resolver(
            5,
            Arc::new(NoSecrets),
            root.clone(),
            secret::RedactionService::new(),
            None,
            None,
            vec![],
        )
        .unwrap();
        let token = backend.instance_token().to_owned();
        let config = runtime::SshServerConfig::new(
            runtime::SshTargetIdentity {
                remote_workspace_id: "fixture".into(),
                account: "fixture".into(),
                canonical_root: "/tmp".into(),
                host_key_sha256: format!("SHA256:{}", "a".repeat(43)),
            },
            token.clone(),
        );
        let hello = runtime::SshClientHello::new(config.identity.clone(), 1);
        let (peer, server) = UnixStream::pair().unwrap();
        let server_close = server.try_clone().unwrap();
        let serve = thread::spawn(move || {
            runtime::serve_ssh_stream(
                &backend,
                server.try_clone().unwrap(),
                server,
                &config,
                Arc::new(|_, bytes| Ok(bytes.to_vec())),
                Arc::new(move || {
                    let _ = server_close.shutdown(std::net::Shutdown::Both);
                }),
            )
        });
        let close = peer.try_clone().unwrap();
        let remote = Arc::new(
            SshRuntimeClient::connect(
                peer.try_clone().unwrap(),
                peer,
                hello,
                Arc::new(move || {
                    let _ = close.shutdown(std::net::Shutdown::Both);
                }),
            )
            .unwrap(),
        );
        let mut client = WorkspaceRuntimeClient::remote(remote.clone());
        assert!(client.is_remote());
        assert!(client.is_connected());
        assert_eq!(
            client.remote_worker_control().err(),
            Some(RemoteRuntimeUnsupported::CommandCenterExecution)
        );
        assert!(client.remote_worker_wake().is_none());
        assert_eq!(client.remote_rpc_token(), Some(token.as_str()));
        assert_ne!(
            client.instance_token(),
            token,
            "UI attachment identity must differ from the persistent server runtime"
        );
        let next_attachment = WorkspaceRuntimeClient::remote(remote.clone());
        assert_ne!(
            client.instance_token(),
            next_attachment.instance_token(),
            "each attachment must invalidate prior UI/PWA capabilities"
        );
        assert!(client.command_sink().is_some());
        let events = client.subscribe();
        client
            .send_command(RuntimeCommand::RequestMuxSnapshot)
            .unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            if events
                .drain()
                .iter()
                .any(|e| matches!(e, runtime::RuntimeEvent::MuxUpdated { .. }))
            {
                break;
            }
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(2));
        }
        let error = match client.inspect_screen(SessionId(1)) {
            Ok(_) => panic!("remote inspection must be unsupported"),
            Err(error) => error,
        };
        assert_eq!(
            error.downcast_ref::<RemoteRuntimeUnsupported>(),
            Some(&RemoteRuntimeUnsupported::InspectScreen)
        );
        let error = match client.send_guarded_input_batch_with_reply(
            SessionId(1),
            "reply".into(),
            vec![b"input".to_vec()],
            admission(),
        ) {
            Ok(_) => panic!("remote output reply must be unsupported"),
            Err(error) => error,
        };
        assert_eq!(
            error.downcast_ref::<RemoteRuntimeUnsupported>(),
            Some(&RemoteRuntimeUnsupported::InputReply)
        );
        let automatic = admission().with_agent_guard(runtime::AgentInputGuard {
            foreground_process_group: 1,
            provider: runtime::AgentPromptKind::Codex,
            intent: runtime::AgentInputIntent::AutomaticPrompt,
        });
        assert!(
            client
                .send_guarded_input_batch(
                    SessionId(1),
                    "automatic".into(),
                    vec![b"input".to_vec()],
                    automatic
                )
                .unwrap_err()
                .to_string()
                .contains("automatic_or_tracked_input_unsupported")
        );
        let captured_sink = client.command_sink().unwrap();
        client.begin_remote_close();
        assert!(
            client
                .send_command(RuntimeCommand::FocusPane {
                    pane: runtime::MuxPaneId("closing-fixture".into()),
                })
                .is_err(),
            "a closing SSH attachment must reject focus and new native effects"
        );
        let retained = RuntimeCommand::WriteInput {
            session: SessionId(1),
            bytes: b"closing native input".to_vec(),
        };
        let (_, retained) = client.send_command_owned(retained).unwrap_err();
        assert!(matches!(*retained, RuntimeCommand::WriteInput { .. }));
        assert!(
            client
                .send_guarded_input_batch(
                    SessionId(1),
                    "closing-guarded-input".into(),
                    vec![b"closing guarded input".to_vec()],
                    admission(),
                )
                .is_err()
        );
        // The sink captured by PWA before closing still permits the confirmation snapshot.
        captured_sink(RuntimeCommand::RequestMuxSnapshot);
        client
            .send_command(RuntimeCommand::ClosePane {
                pane: runtime::MuxPaneId("closing-fixture".into()),
            })
            .unwrap();
        client.finish_remote_close();
        client
            .send_command(RuntimeCommand::RequestMuxSnapshot)
            .unwrap();
        let start = Instant::now();
        client.shutdown();
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "remote shutdown must signal only"
        );
        assert!(!client.is_connected());
        assert!(!remote.is_connected());
        serve.join().unwrap().unwrap();
        drop(client);
        drop(remote);
        std::fs::remove_dir_all(root).unwrap();
    }
}
