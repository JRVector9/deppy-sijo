//! One background owner for SSH processes, authentication, leases and service replies.
//! UI callers only use bounded channels; no process, filesystem or socket waits occur there.
use crate::{
    remote_bootstrap::{self, HelperInstallConsent, VerifiedHelperArtifact},
    ssh_workspace_add::{
        SshWorkspaceError, SshWorkspaceIntent, SshWorkspaceSnapshot, SshWorkspaceUpdate,
    },
};
use deppy_core::workspace_target::{RemotePath, SshConnectionProfile, SshWorkspaceTarget};
use deppy_remote_host::services::{self, Agent, Reply, Request, Response};
use runtime::{SshClientHello, SshRpcReply, SshRuntimeClient, SshTargetIdentity};
use ssh_transport::{
    FailureKind, HelperVersion, RemoteCommand, ResolvedIdentity, SshDestination, SshEvent,
    SshLaunchOptions, SshStream, SshWorker,
};
use std::{
    collections::HashMap,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    },
    time::{Duration, Instant},
};
const COMMANDS_MAX: usize = 32;
const EVENTS_MAX: usize = 64;
const PENDING_MAX: usize = 24;
const RPC_TIMEOUT: Duration = Duration::from_secs(15);
const FIRST_INTERNAL_REQUEST: u64 = u64::MAX / 2;
type Interrupt = Arc<dyn Fn() + Send + Sync>;

/// The app composition root supplies storage ownership; registration runs on this host's worker.
pub trait SshWorkspaceRegistration: Send + Sync {
    fn register(
        &self,
        name: &str,
        profile: &SshConnectionProfile,
        target: &SshWorkspaceTarget,
    ) -> Result<storage::WorkspaceTargetRow, SshWorkspaceError>;
}

pub enum SshHostEvent {
    Ui(SshWorkspaceSnapshot),
    Connected {
        generation: u64,
        client: Arc<SshRuntimeClient>,
        profile: SshConnectionProfile,
        target: SshWorkspaceTarget,
        home: RemotePath,
        agents: Vec<Agent>,
    },
    Registered {
        generation: u64,
        row: storage::WorkspaceTargetRow,
    },
    Rpc {
        generation: u64,
        reply: SshRpcReply,
    },
    Offline {
        generation: u64,
    },
}
enum HostCommand {
    Ui(SshWorkspaceIntent),
    Rpc { request_id: u64, request: Request },
}
struct Cancellation {
    cancelled: AtomicBool,
    interrupt: Mutex<Option<Interrupt>>,
}
impl Cancellation {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let interrupt = self
            .interrupt
            .lock()
            .expect("SSH cancellation lock")
            .clone();
        if let Some(interrupt) = interrupt {
            interrupt();
        }
    }
    fn replace(&self, interrupt: Interrupt) {
        *self.interrupt.lock().expect("SSH cancellation lock") = Some(interrupt.clone());
        if self.cancelled.load(Ordering::Acquire) {
            interrupt();
        }
    }
}
pub(crate) enum SshHostTarget {
    Open(Option<SshWorkspaceTarget>),
    Resume(SshWorkspaceTarget),
}
pub struct SshWorkspaceHost {
    generation: u64,
    commands: SyncSender<HostCommand>,
    events: Receiver<SshHostEvent>,
    cancellation: Arc<Cancellation>,
}
impl SshWorkspaceHost {
    #[cfg(test)]
    pub(crate) fn event_fixture(generation: u64) -> (Self, SyncSender<SshHostEvent>) {
        let (commands, _requests) = mpsc::sync_channel(COMMANDS_MAX);
        let (output, events) = mpsc::sync_channel(EVENTS_MAX);
        (
            Self {
                generation,
                commands,
                events,
                cancellation: Arc::new(Cancellation {
                    cancelled: AtomicBool::new(false),
                    interrupt: Mutex::new(None),
                }),
            },
            output,
        )
    }

    pub fn spawn(
        profile: SshConnectionProfile,
        generation: u64,
        target: SshHostTarget,
        db_path: PathBuf,
        resources: PathBuf,
        wake: Interrupt,
        registrar: Arc<dyn SshWorkspaceRegistration>,
    ) -> io::Result<Self> {
        let (target, resume_required) = match target {
            SshHostTarget::Open(target) => (target, false),
            SshHostTarget::Resume(target) => (Some(target), true),
        };
        if generation == 0
            || profile.validate().is_err()
            || target.as_ref().is_some_and(|target| {
                target.validate().is_err() || target.connection_id != profile.id
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid SSH workspace host",
            ));
        }
        let (commands, requests) = mpsc::sync_channel(COMMANDS_MAX);
        let (output, events) = mpsc::sync_channel(EVENTS_MAX);
        let cancellation = Arc::new(Cancellation {
            cancelled: AtomicBool::new(false),
            interrupt: Mutex::new(None),
        });
        let thread_cancel = cancellation.clone();
        std::thread::Builder::new()
            .name("ssh-workspace".into())
            .spawn(move || {
                let mut owner = Owner {
                    profile,
                    generation,
                    target,
                    resume_required,
                    db_path,
                    resources,
                    requests,
                    output,
                    cancellation: thread_cancel,
                    wake,
                    next_request: FIRST_INTERNAL_REQUEST,
                    registrar,
                    #[cfg(test)]
                    test_program: None,
                };
                let result = owner.run();
                if let Err(error) = result {
                    owner.ui(SshWorkspaceUpdate::Failed(error));
                }
                owner.publish(SshHostEvent::Offline { generation });
                (owner.wake)();
            })?;
        Ok(Self {
            generation,
            commands,
            events,
            cancellation,
        })
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn send(&self, intent: SshWorkspaceIntent) -> Result<(), SshWorkspaceError> {
        if intent_generation(&intent) != self.generation {
            return Err(SshWorkspaceError::RequestFailed);
        }
        if matches!(
            intent,
            SshWorkspaceIntent::Cancel { .. } | SshWorkspaceIntent::Close { .. }
        ) {
            self.cancel();
            return Ok(());
        }
        self.commands
            .try_send(HostCommand::Ui(intent))
            .map_err(|_| SshWorkspaceError::WorkerUnavailable)
    }
    pub fn rpc(&self, request_id: u64, request: Request) -> Result<(), SshWorkspaceError> {
        if request_id == 0 || request_id >= FIRST_INTERNAL_REQUEST {
            return Err(SshWorkspaceError::RequestFailed);
        }
        self.commands
            .try_send(HostCommand::Rpc {
                request_id,
                request,
            })
            .map_err(|_| SshWorkspaceError::WorkerUnavailable)
    }
    pub fn poll(&self) -> Option<SshHostEvent> {
        self.events.try_recv().ok()
    }
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }
}
impl Drop for SshWorkspaceHost {
    fn drop(&mut self) {
        self.cancel();
    }
}
fn intent_generation(intent: &SshWorkspaceIntent) -> u64 {
    match intent {
        SshWorkspaceIntent::Connect { generation, .. }
        | SshWorkspaceIntent::PromptReply { generation, .. }
        | SshWorkspaceIntent::InstallHelper { generation }
        | SshWorkspaceIntent::Browse { generation, .. }
        | SshWorkspaceIntent::VerifyRoot { generation, .. }
        | SshWorkspaceIntent::Add { generation, .. }
        | SshWorkspaceIntent::Cancel { generation }
        | SshWorkspaceIntent::Close { generation } => *generation,
    }
}
struct Owner {
    profile: SshConnectionProfile,
    generation: u64,
    target: Option<SshWorkspaceTarget>,
    resume_required: bool,
    db_path: PathBuf,
    resources: PathBuf,
    requests: Receiver<HostCommand>,
    output: SyncSender<SshHostEvent>,
    cancellation: Arc<Cancellation>,
    wake: Interrupt,
    next_request: u64,
    registrar: Arc<dyn SshWorkspaceRegistration>,
    #[cfg(test)]
    test_program: Option<PathBuf>,
}
#[derive(Default)]
struct PeerProof {
    resolved: Option<ResolvedIdentity>,
    key: Option<String>,
}
impl PeerProof {
    fn target(
        &self,
        profile: &SshConnectionProfile,
        account: &str,
        root: RemotePath,
    ) -> Result<SshWorkspaceTarget, SshWorkspaceError> {
        let peer = self
            .resolved
            .as_ref()
            .ok_or(SshWorkspaceError::ConnectionFailed)?;
        if peer.account != account {
            return Err(SshWorkspaceError::AuthenticationFailed);
        }
        SshWorkspaceTarget::new(
            profile.id.clone(),
            peer.host.clone(),
            peer.port,
            account.into(),
            self.key.clone().ok_or(SshWorkspaceError::HostKeyChanged)?,
            services::workspace_id(account, root.as_str()),
            root,
        )
        .map_err(|_| SshWorkspaceError::HostKeyChanged)
    }
    fn verify(&self, expected: &SshWorkspaceTarget) -> Result<(), SshWorkspaceError> {
        let actual = self.target(
            &SshConnectionProfile {
                id: expected.connection_id.clone(),
                destination: expected.host.clone(),
                port: None,
                identity_file: None,
            },
            &expected.account,
            expected.canonical_root.clone(),
        )?;
        if actual.identity() != expected.identity()
            || actual.remote_workspace_id != expected.remote_workspace_id
        {
            return Err(SshWorkspaceError::HostKeyChanged);
        }
        Ok(())
    }
}
enum PhaseError {
    Transport(FailureKind),
    Peer(SshWorkspaceError),
    Task(anyhow::Error),
    Cancelled,
}
impl PhaseError {
    fn for_target(&self, data: &Path, target: &SshWorkspaceTarget) -> SshWorkspaceError {
        if matches!(self, Self::Task(error) if error.downcast_ref::<runtime::SshHandshakeError>() == Some(&runtime::SshHandshakeError::ResumeExpired))
            && crate::ssh_resume_store::clear(data, target).is_err()
        {
            return SshWorkspaceError::HelperFailed;
        }
        self.ui_error()
    }

    fn ui_error(&self) -> SshWorkspaceError {
        match self {
            Self::Peer(error) => *error,
            Self::Transport(FailureKind::HostKeyMismatch | FailureKind::HostVerification) => {
                SshWorkspaceError::HostKeyChanged
            }
            Self::Transport(FailureKind::Authentication) => SshWorkspaceError::AuthenticationFailed,
            Self::Cancelled => SshWorkspaceError::WorkerUnavailable,
            Self::Task(error)
                if error.downcast_ref::<runtime::SshHandshakeError>()
                    == Some(&runtime::SshHandshakeError::ResumeExpired) =>
            {
                SshWorkspaceError::SessionExpired
            }
            Self::Task(_) => SshWorkspaceError::HelperFailed,
            _ => SshWorkspaceError::ConnectionFailed,
        }
    }
}
struct Session {
    worker: SshWorker,
    client: Arc<SshRuntimeClient>,
    target: SshWorkspaceTarget,
}
enum Pending {
    Browse { request_id: u64, path: RemotePath },
    Root { request_id: u64 },
    External { request_id: u64 },
}
impl Owner {
    fn publish(&self, event: SshHostEvent) {
        if self.output.try_send(event).is_err() {
            self.cancellation.cancel();
        } else {
            (self.wake)();
        }
    }
    fn ui(&self, update: SshWorkspaceUpdate) {
        self.publish(SshHostEvent::Ui(SshWorkspaceSnapshot {
            generation: self.generation,
            update,
        }));
    }
    fn cancelled(&self) -> bool {
        self.cancellation.cancelled.load(Ordering::Acquire)
    }
    fn options(&self, command: RemoteCommand) -> Result<SshLaunchOptions, SshWorkspaceError> {
        let destination = SshDestination::parse(&self.profile.destination)
            .map_err(|_| SshWorkspaceError::InvalidAddress)?;
        let executable =
            std::env::current_exe().map_err(|_| SshWorkspaceError::WorkerUnavailable)?;
        let mut options = SshLaunchOptions::new(
            destination,
            crate::ssh_connection::bundled_askpass_path(&executable),
            command,
        );
        options.port = self.profile.port;
        options.identity_file = self.profile.identity_file.as_ref().map(PathBuf::from);
        #[cfg(test)]
        if let Some(program) = &self.test_program {
            options.ssh_program = program.clone();
            options.askpass_program = "/usr/bin/true".into();
        }
        Ok(options)
    }
    fn internal_id(&mut self) -> Result<u64, SshWorkspaceError> {
        self.next_request = self
            .next_request
            .checked_add(1)
            .ok_or(SshWorkspaceError::RequestFailed)?;
        Ok(self.next_request)
    }
    /// Wait for human authentication before starting the helper's five-second hello watchdog.
    /// Probe/install must also observe OpenSSH's successful exit before trusting their output.
    fn phase<T: Send + 'static>(
        &mut self,
        command: RemoteCommand,
        needs_exit: bool,
        expected_peer: Option<&SshWorkspaceTarget>,
        task: impl FnOnce(SshStream, Interrupt) -> anyhow::Result<T> + Send + 'static,
    ) -> Result<(SshWorker, T, PeerProof), PhaseError> {
        let options = self
            .options(command)
            .map_err(|_| PhaseError::Transport(FailureKind::InvalidConfiguration))?;
        let (worker, stream) =
            SshWorker::spawn(options).map_err(|_| PhaseError::Transport(FailureKind::Io))?;
        let interrupt = worker.cancel_handle();
        self.cancellation.replace(interrupt.clone());
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let mut work = Some((stream, task));
        let mut result = None;
        let mut result_at = None;
        let mut ended = None;
        let mut ended_at = None;
        let mut proof = PeerProof::default();
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if self.cancelled() {
                return Err(PhaseError::Cancelled);
            }
            for _ in 0..32 {
                let Some(event) = worker.try_event() else {
                    break;
                };
                match event {
                    SshEvent::Resolved(peer) => proof.resolved = Some(peer),
                    SshEvent::HostKeyObserved(key) => proof.key = Some(key),
                    SshEvent::Prompt(prompt) => self.ui(SshWorkspaceUpdate::Prompt(prompt)),
                    SshEvent::Ended(reason) => {
                        ended = Some(reason);
                        ended_at = Some(Instant::now());
                    }
                }
            }
            // Verify every authenticated phase before writing a hello/resume capability or
            // installation bytes. An alias/config/peer may change between separate SSH calls.
            // A short helper can also reject hello and exit between polls; drain its frame.
            if worker.authenticated()
                && proof.resolved.is_some()
                && proof.key.is_some()
                && (ended.is_none()
                    || ended == Some(FailureKind::Exit)
                    || (!needs_exit
                        && !matches!(
                            ended,
                            Some(
                                FailureKind::HostKeyMismatch
                                    | FailureKind::HostVerification
                                    | FailureKind::Authentication
                                    | FailureKind::RemoteExit(127)
                            )
                        )))
                && let Some((stream, task)) = work.take()
            {
                if let Some(expected) = expected_peer {
                    proof.verify(expected).map_err(PhaseError::Peer)?;
                }
                let result_tx = result_tx.clone();
                let interrupt = interrupt.clone();
                std::thread::Builder::new()
                    .name("ssh-workspace-io".into())
                    .spawn(move || {
                        let _ = result_tx.send(task(stream, interrupt));
                    })
                    .map_err(|_| PhaseError::Transport(FailureKind::Io))?;
            }
            if result.is_none()
                && let Ok(value) = result_rx.try_recv()
            {
                result = Some(value);
                result_at = Some(Instant::now());
            }
            if let Some(reason) = ended.as_ref()
                && *reason != FailureKind::Exit
            {
                // OpenSSH exits after a helper rejects hello. Drain the already-authenticated
                // framed rejection before classifying the process exit, so expiry recovery is
                // distinguishable from a host/lease mismatch. A missing helper still needs 127.
                if !matches!(
                    reason,
                    FailureKind::HostKeyMismatch
                        | FailureKind::HostVerification
                        | FailureKind::Authentication
                        | FailureKind::RemoteExit(127)
                ) {
                    if matches!(result.as_ref(), Some(Err(error)) if error.downcast_ref::<runtime::SshHandshakeError>().is_some())
                    {
                        return Err(PhaseError::Task(
                            result
                                .take()
                                .expect("typed SSH rejection")
                                .err()
                                .expect("typed SSH error"),
                        ));
                    }
                    if work.is_none()
                        && result.is_none()
                        && ended_at.is_some_and(|when| when.elapsed() < Duration::from_millis(300))
                    {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                }
                return Err(PhaseError::Transport(reason.clone()));
            }
            let completed = match result.as_ref() {
                Some(Ok(_)) => !needs_exit || ended == Some(FailureKind::Exit),
                Some(Err(_)) => {
                    ended.is_some()
                        || result_at
                            .is_some_and(|when| when.elapsed() >= Duration::from_millis(300))
                }
                None => false,
            };
            if completed {
                let value = result
                    .take()
                    .expect("completed SSH result")
                    .map_err(PhaseError::Task)?;
                return Ok((worker, value, proof));
            }
            if Instant::now() >= deadline {
                return Err(PhaseError::Transport(FailureKind::Timeout));
            }
            match self.requests.recv_timeout(Duration::from_millis(10)) {
                Ok(HostCommand::Ui(SshWorkspaceIntent::PromptReply {
                    generation,
                    request_id,
                    reply,
                })) if generation == self.generation => {
                    worker
                        .reply(request_id, reply)
                        .map_err(|_| PhaseError::Transport(FailureKind::Authentication))?;
                }
                Ok(HostCommand::Ui(
                    SshWorkspaceIntent::Cancel { .. } | SshWorkspaceIntent::Close { .. },
                )) => return Err(PhaseError::Cancelled),
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(PhaseError::Cancelled),
            }
        }
    }
    fn run(&mut self) -> Result<(), SshWorkspaceError> {
        let (_, output, proof) = self
            .phase(RemoteCommand::Probe, true, None, |stream, _| {
                let (reader, writer) = stream.split();
                drop(writer);
                let mut bytes = Vec::new();
                reader.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
                anyhow::ensure!(bytes.len() <= 16 * 1024, "ssh_probe_too_large");
                Ok(bytes)
            })
            .map_err(|error| error.ui_error())?;
        let probe = remote_bootstrap::parse_server_probe(&output)
            .map_err(|_| SshWorkspaceError::UnsupportedServer)?;
        let root = self
            .target
            .as_ref()
            .map(|target| target.canonical_root.clone())
            .unwrap_or(RemotePath::new("/").map_err(|_| SshWorkspaceError::RequestFailed)?);
        let target = proof.target(&self.profile, &probe.account, root)?;
        if let Some(expected) = &self.target {
            proof.verify(expected)?;
        }
        let data = self
            .db_path
            .parent()
            .ok_or(SshWorkspaceError::WorkerUnavailable)?
            .to_owned();
        let resume = load_resume_for_connection(&data, &target, self.resume_required)?;
        let mut hello = SshClientHello::new(identity(&target), self.generation);
        hello.resume = resume;
        let version = HelperVersion::parse(env!("CARGO_PKG_VERSION"))
            .map_err(|_| SshWorkspaceError::HelperFailed)?;
        let connect = |stream: SshStream, interrupt: Interrupt, hello| {
            let (reader, writer) = stream.split();
            SshRuntimeClient::connect(reader, writer, hello, interrupt).map(Arc::new)
        };
        let first_hello = hello.clone();
        let initial = self.phase(
            RemoteCommand::Helper {
                version: version.clone(),
            },
            false,
            Some(&target),
            move |stream, interrupt| connect(stream, interrupt, first_hello),
        );
        let (worker, client, helper_proof) = match initial {
            Ok(session) => session,
            Err(PhaseError::Transport(FailureKind::RemoteExit(127))) => {
                self.ui(SshWorkspaceUpdate::InstallRequired {
                    install_path: format!("~/.local/share/deppy/remote-host/{}", version.as_str()),
                });
                loop {
                    if self.cancelled() {
                        return Err(SshWorkspaceError::WorkerUnavailable);
                    }
                    match self.requests.recv_timeout(Duration::from_millis(50)) {
                        Ok(HostCommand::Ui(SshWorkspaceIntent::InstallHelper { generation }))
                            if generation == self.generation =>
                        {
                            break;
                        }
                        Err(RecvTimeoutError::Disconnected) => {
                            return Err(SshWorkspaceError::WorkerUnavailable);
                        }
                        _ => {}
                    }
                }
                let resources = if self.resources.as_os_str().is_empty() {
                    std::env::current_exe()
                        .ok()
                        .and_then(|exe| {
                            exe.parent()?
                                .parent()
                                .map(|contents| contents.join("Resources"))
                        })
                        .ok_or(SshWorkspaceError::HelperFailed)?
                } else {
                    self.resources.clone()
                };
                let artifact =
                    VerifiedHelperArtifact::load(&resources, probe.target, version.as_str())
                        .map_err(|_| SshWorkspaceError::HelperFailed)?;
                let plan = remote_bootstrap::prepare_helper_install(
                    &artifact,
                    Some(HelperInstallConsent {
                        connection_generation: self.generation,
                    }),
                    self.generation,
                )
                .map_err(|_| SshWorkspaceError::HelperFailed)?;
                let bytes = plan.bytes.to_vec();
                let marker = plan.ready_marker;
                let (_, (), install_proof) = self
                    .phase(
                        plan.remote_command,
                        true,
                        Some(&target),
                        move |stream, _| {
                            let (reader, mut writer) = stream.split();
                            writer.write_all(&bytes)?;
                            writer.flush()?;
                            drop(writer);
                            let mut ready = Vec::new();
                            reader.take(8193).read_to_end(&mut ready)?;
                            anyhow::ensure!(
                                ready.len() <= 8192
                                    && std::str::from_utf8(&ready)?.lines().last()
                                        == Some(marker.as_str()),
                                "ssh_helper_install_failed"
                            );
                            Ok(())
                        },
                    )
                    .map_err(|error| error.ui_error())?;
                install_proof.verify(&target)?;
                self.phase(
                    RemoteCommand::Helper { version },
                    false,
                    Some(&target),
                    move |stream, interrupt| connect(stream, interrupt, hello),
                )
                .map_err(|error| error.for_target(&data, &target))?
            }
            Err(error) => {
                return Err(error.for_target(&data, &target));
            }
        };
        helper_proof.verify(&target)?;
        crate::ssh_resume_store::save(&data, &target, &client.resume())
            .map_err(|_| SshWorkspaceError::HelperFailed)?;
        let session = Session {
            worker,
            client,
            target,
        };
        let metadata = self.rpc_initial(&session, Request::Metadata)?;
        let expected_architecture = match probe.target {
            ssh_transport::HelperTarget::LinuxArm64 => "aarch64",
            ssh_transport::HelperTarget::LinuxX86_64 => "x86_64",
        };
        if !matches!(&metadata, Response::Metadata(metadata) if metadata.account == probe.account && metadata.helper_version == env!("CARGO_PKG_VERSION") && metadata.os == "linux" && metadata.architecture == expected_architecture)
        {
            return Err(SshWorkspaceError::HelperFailed);
        }
        let agents = match self.rpc_initial(&session, Request::InspectAgents)? {
            Response::Agents(agents) if agents.len() <= 32 => agents,
            _ => return Err(SshWorkspaceError::HelperFailed),
        };
        let home = if self.target.is_some() {
            session.target.canonical_root.clone()
        } else {
            RemotePath::new(probe.home).map_err(|_| SshWorkspaceError::UnsupportedServer)?
        };
        self.ui(SshWorkspaceUpdate::Connected {
            profile: self.profile.clone(),
            target: session.target.clone(),
            home: home.clone(),
            agents: agents.clone(),
        });
        self.publish(SshHostEvent::Connected {
            generation: self.generation,
            client: session.client.clone(),
            profile: self.profile.clone(),
            target: session.target.clone(),
            home,
            agents,
        });
        self.connected(session)
    }
    fn rpc_initial(
        &mut self,
        session: &Session,
        request: Request,
    ) -> Result<Response, SshWorkspaceError> {
        let request_id = self.internal_id()?;
        let bytes = serde_json::to_vec(&request).map_err(|_| SshWorkspaceError::RequestFailed)?;
        session
            .client
            .send_rpc(request_id, bytes)
            .map_err(|_| SshWorkspaceError::ConnectionFailed)?;
        let until = Instant::now() + RPC_TIMEOUT;
        while !self.cancelled() && session.client.is_connected() && Instant::now() < until {
            if let Some(reply) = session.client.try_recv_rpc() {
                if reply.context.request_id != request_id
                    || !reply_matches(&reply, session, self.generation)
                {
                    return Err(SshWorkspaceError::RequestFailed);
                }
                return decode(reply).map_err(map_service_error);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Err(SshWorkspaceError::ConnectionFailed)
    }
    fn connected(&mut self, session: Session) -> Result<(), SshWorkspaceError> {
        let mut pending: HashMap<u64, (Instant, Pending)> = HashMap::new();
        let mut verified = None;
        let mut registered = false;
        while !self.cancelled() && session.client.is_connected() {
            for _ in 0..32 {
                let Some(event) = session.worker.try_event() else {
                    break;
                };
                if matches!(event, SshEvent::Ended(_)) {
                    session.client.disconnect();
                    return Err(SshWorkspaceError::ConnectionFailed);
                }
            }
            for _ in 0..32 {
                let Some(reply) = session.client.try_recv_rpc() else {
                    break;
                };
                if !reply_matches(&reply, &session, self.generation) {
                    return Err(SshWorkspaceError::RequestFailed);
                }
                let Some((_, request)) = pending.remove(&reply.context.request_id) else {
                    return Err(SshWorkspaceError::RequestFailed);
                };
                match request {
                    Pending::External { request_id } => {
                        if reply.context.request_id != request_id {
                            return Err(SshWorkspaceError::RequestFailed);
                        }
                        self.publish(SshHostEvent::Rpc {
                            generation: self.generation,
                            reply,
                        });
                    }
                    Pending::Browse { request_id, path } => match decode(reply) {
                        Ok(Response::Directory(page)) => self.ui(SshWorkspaceUpdate::Directory {
                            request_id,
                            path,
                            page,
                        }),
                        result => self.request_failed(
                            request_id,
                            result
                                .err()
                                .map(map_service_error)
                                .unwrap_or(SshWorkspaceError::RequestFailed),
                        ),
                    },
                    Pending::Root { request_id } => match decode(reply) {
                        Ok(Response::Root(root)) => {
                            let mut target = session.target.clone();
                            target.canonical_root = RemotePath::new(root.canonical_root)
                                .map_err(|_| SshWorkspaceError::RequestFailed)?;
                            target.remote_workspace_id = root.remote_workspace_id;
                            target
                                .validate()
                                .map_err(|_| SshWorkspaceError::RequestFailed)?;
                            if target.remote_workspace_id
                                != services::workspace_id(
                                    &target.account,
                                    target.canonical_root.as_str(),
                                )
                            {
                                return Err(SshWorkspaceError::RequestFailed);
                            }
                            verified = Some(target.clone());
                            self.ui(SshWorkspaceUpdate::RootVerified { request_id, target });
                        }
                        result => self.request_failed(
                            request_id,
                            result
                                .err()
                                .map(map_service_error)
                                .unwrap_or(SshWorkspaceError::RequestFailed),
                        ),
                    },
                }
            }
            if pending
                .values()
                .any(|(started, _)| started.elapsed() >= RPC_TIMEOUT)
            {
                return Err(SshWorkspaceError::ConnectionFailed);
            }
            match self.requests.recv_timeout(Duration::from_millis(10)) {
                Ok(HostCommand::Ui(intent)) if intent_generation(&intent) != self.generation => {}
                Ok(HostCommand::Ui(SshWorkspaceIntent::Browse {
                    request_id,
                    path,
                    cursor,
                    ..
                })) => {
                    let relative = relative_to_root(&session.target.canonical_root, &path)?;
                    self.queue_rpc(
                        &session,
                        &mut pending,
                        Request::ListDirectory {
                            path: relative,
                            cursor,
                            limit: 256,
                        },
                        Pending::Browse { request_id, path },
                    )?;
                }
                Ok(HostCommand::Ui(SshWorkspaceIntent::VerifyRoot {
                    request_id, path, ..
                })) => {
                    verified = None;
                    self.queue_rpc(
                        &session,
                        &mut pending,
                        Request::StatRoot {
                            path: path.as_str().into(),
                        },
                        Pending::Root { request_id },
                    )?;
                }
                Ok(HostCommand::Ui(SshWorkspaceIntent::Add {
                    name,
                    profile,
                    target,
                    ..
                })) => {
                    if registered || verified.as_ref() != Some(&target) || profile != self.profile {
                        return Err(SshWorkspaceError::RequestFailed);
                    }
                    let row = self.registrar.register(&name, &profile, &target)?;
                    registered = true;
                    self.publish(SshHostEvent::Registered {
                        generation: self.generation,
                        row,
                    });
                }
                Ok(HostCommand::Rpc {
                    request_id,
                    request,
                }) => {
                    self.queue_rpc(
                        &session,
                        &mut pending,
                        request,
                        Pending::External { request_id },
                    )?;
                }
                Ok(HostCommand::Ui(
                    SshWorkspaceIntent::Cancel { .. } | SshWorkspaceIntent::Close { .. },
                ))
                | Err(RecvTimeoutError::Disconnected) => break,
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            }
        }
        session.client.disconnect();
        Ok(())
    }
    fn request_failed(&self, request_id: u64, error: SshWorkspaceError) {
        self.ui(SshWorkspaceUpdate::RequestFailed { request_id, error });
    }
    fn queue_rpc(
        &mut self,
        session: &Session,
        pending: &mut HashMap<u64, (Instant, Pending)>,
        request: Request,
        value: Pending,
    ) -> Result<(), SshWorkspaceError> {
        if pending.len() >= PENDING_MAX {
            return Err(SshWorkspaceError::WorkerUnavailable);
        }
        let id = match &value {
            Pending::External { request_id } => *request_id,
            _ => self.internal_id()?,
        };
        if pending.contains_key(&id) {
            return Err(SshWorkspaceError::RequestFailed);
        }
        let bytes = serde_json::to_vec(&request).map_err(|_| SshWorkspaceError::RequestFailed)?;
        if bytes.len() > runtime::SSH_RPC_BYTES_MAX {
            return Err(SshWorkspaceError::RequestFailed);
        }
        session
            .client
            .send_rpc(id, bytes)
            .map_err(|_| SshWorkspaceError::ConnectionFailed)?;
        pending.insert(id, (Instant::now(), value));
        Ok(())
    }
}
fn identity(target: &SshWorkspaceTarget) -> SshTargetIdentity {
    SshTargetIdentity {
        remote_workspace_id: target.remote_workspace_id.clone(),
        account: target.account.clone(),
        canonical_root: target.canonical_root.as_str().into(),
        host_key_sha256: target.host_key_sha256.clone(),
    }
}
fn reply_matches(reply: &SshRpcReply, session: &Session, generation: u64) -> bool {
    reply.context.connection_generation == generation
        && reply.context.identity == identity(&session.target)
        && reply.context.runtime_instance == session.client.instance_token()
}
fn decode(reply: SshRpcReply) -> Reply {
    let bytes = reply
        .result
        .map_err(|_| services::ServiceError::InvalidRequest)?;
    if bytes.len() > runtime::SSH_RPC_BYTES_MAX {
        return Err(services::ServiceError::TooLarge);
    }
    serde_json::from_slice(&bytes).map_err(|_| services::ServiceError::InvalidRequest)?
}
fn map_service_error(error: services::ServiceError) -> SshWorkspaceError {
    match error {
        services::ServiceError::Files(deppy_remote_host::files::FileError::PermissionDenied) => {
            SshWorkspaceError::PermissionDenied
        }
        services::ServiceError::Files(deppy_remote_host::files::FileError::NotFound) => {
            SshWorkspaceError::FolderMissing
        }
        _ => SshWorkspaceError::RequestFailed,
    }
}
fn load_resume_for_connection(
    data: &Path,
    target: &SshWorkspaceTarget,
    required: bool,
) -> Result<Option<runtime::SshResume>, SshWorkspaceError> {
    let resume =
        crate::ssh_resume_store::load(data, target).map_err(|_| SshWorkspaceError::HelperFailed)?;
    if required && resume.is_none() {
        return Err(SshWorkspaceError::SessionExpired);
    }
    Ok(resume)
}

fn relative_to_root(root: &RemotePath, path: &RemotePath) -> Result<Vec<u8>, SshWorkspaceError> {
    let root = root.as_str();
    let path = path.as_str();
    if root == path {
        return Ok(Vec::new());
    }
    if root == "/" {
        return Ok(path.as_bytes()[1..].to_vec());
    }
    path.strip_prefix(root)
        .and_then(|path| path.strip_prefix('/'))
        .map(|path| path.as_bytes().to_vec())
        .ok_or(SshWorkspaceError::PermissionDenied)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct NoRegistration;
    impl SshWorkspaceRegistration for NoRegistration {
        fn register(
            &self,
            _: &str,
            _: &SshConnectionProfile,
            _: &SshWorkspaceTarget,
        ) -> Result<storage::WorkspaceTargetRow, SshWorkspaceError> {
            Err(SshWorkspaceError::RequestFailed)
        }
    }
    #[test]
    fn ssh_workspace_host_automatic_resume_requires_existing_capability() {
        let dir =
            std::env::temp_dir().join(format!("deppy-required-resume-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let target = SshWorkspaceTarget::new(
            "profile".into(),
            "host.test".into(),
            22,
            "fixture".into(),
            format!("SHA256:{}", "a".repeat(43)),
            "workspace".into(),
            RemotePath::new("/project").unwrap(),
        )
        .unwrap();
        assert!(matches!(
            load_resume_for_connection(&dir, &target, true),
            Err(SshWorkspaceError::SessionExpired)
        ));
        assert!(
            load_resume_for_connection(&dir, &target, false)
                .unwrap()
                .is_none()
        );
        let resume = runtime::SshResume {
            runtime_instance: "instance".into(),
            lease: "lease".into(),
        };
        crate::ssh_resume_store::save(&dir, &target, &resume).unwrap();
        let loaded = load_resume_for_connection(&dir, &target, true)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.runtime_instance, resume.runtime_instance);
        assert_eq!(loaded.lease, resume.lease);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn ssh_workspace_host_keeps_typed_lease_rejection_when_remote_exits() {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("deppy-host-rejection-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let program = dir.join("ssh");
        let mut rejection = Vec::new();
        runtime::write_ssh_handshake_rejection(
            &mut rejection,
            runtime::SshHandshakeError::ResumeExpired,
        )
        .unwrap();
        let script = format!(
            "#!/usr/bin/python3\nimport os,sys,time\nif '-G' in sys.argv:\n print('hostname host.test\\nuser fixture\\nport 22');sys.exit(0)\ndebug=open(sys.argv[sys.argv.index('-E')+1],'w',buffering=1)\ndebug.write('debug1: Server host key: ssh-ed25519 SHA256:{}\\nAuthenticated to host.test using publickey.\\ndebug1: Sending command: fixed helper\\n');debug.flush()\nos.read(0,16384)\nos.write(1,bytes({:?}))\ntime.sleep(0.1)\nsys.exit(1)\n",
            "a".repeat(43),
            rejection
        );
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (commands, requests) = mpsc::sync_channel(COMMANDS_MAX);
        let (output, _events) = mpsc::sync_channel(EVENTS_MAX);
        let mut owner = Owner {
            profile: SshConnectionProfile {
                id: "profile".into(),
                destination: "fixture@host.test".into(),
                port: None,
                identity_file: None,
            },
            generation: 1,
            target: None,
            resume_required: false,
            db_path: dir.join("test.sqlite"),
            resources: PathBuf::new(),
            requests,
            output,
            cancellation: Arc::new(Cancellation {
                cancelled: AtomicBool::new(false),
                interrupt: Mutex::new(None),
            }),
            wake: Arc::new(|| {}),
            next_request: FIRST_INTERNAL_REQUEST,
            registrar: Arc::new(NoRegistration),
            test_program: Some(program),
        };
        let target = PeerProof {
            resolved: Some(ResolvedIdentity {
                host: "host.test".into(),
                account: "fixture".into(),
                port: 22,
            }),
            key: Some(format!("SHA256:{}", "a".repeat(43))),
        }
        .target(&owner.profile, "fixture", RemotePath::new("/").unwrap())
        .unwrap();
        let hello = SshClientHello::new(identity(&target), 1);
        let result = owner.phase(
            RemoteCommand::Helper {
                version: HelperVersion::parse("0.13.0").unwrap(),
            },
            false,
            Some(&target),
            move |stream, interrupt| {
                let (reader, writer) = stream.split();
                SshRuntimeClient::connect(reader, writer, hello, interrupt)
            },
        );
        let error = match result {
            Ok(_) => panic!("expired lease was accepted"),
            Err(error) => error,
        };
        assert_eq!(
            error.ui_error(),
            SshWorkspaceError::SessionExpired,
            "helper exit must preserve the framed rejection for safe user recovery"
        );
        drop(commands);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn ssh_workspace_host_background_pipeline_is_not_a_silent_intent_sink() {
        let source = include_str!("app.rs");
        assert!(
            !source.contains("Some(WorkspaceAddIntent::Ssh(_)) => {}"),
            "SSH UI intents must reach the background host"
        );
    }
    #[test]
    fn ssh_workspace_host_paths_and_verified_peers_never_cross_target_scope() {
        let root = RemotePath::new("/remote/project").unwrap();
        assert_eq!(
            relative_to_root(
                &root,
                &RemotePath::new("/remote/project/한글 'file").unwrap()
            )
            .unwrap(),
            "한글 'file".as_bytes()
        );
        assert!(
            relative_to_root(
                &root,
                &RemotePath::new("/remote/project-other/file").unwrap()
            )
            .is_err()
        );
        let profile = SshConnectionProfile {
            id: "profile".into(),
            destination: "alias".into(),
            port: None,
            identity_file: None,
        };
        let mut proof = PeerProof {
            resolved: Some(ResolvedIdentity {
                host: "host.test".into(),
                account: "ubuntu".into(),
                port: 22,
            }),
            key: Some(format!("SHA256:{}", "a".repeat(43))),
        };
        let target = proof.target(&profile, "ubuntu", root).unwrap();
        assert!(proof.verify(&target).is_ok());
        proof.key = Some(format!("SHA256:{}", "b".repeat(43)));
        assert_eq!(
            proof.verify(&target),
            Err(SshWorkspaceError::HostKeyChanged)
        );
        assert!(
            proof
                .target(&profile, "other", RemotePath::new("/").unwrap())
                .is_err()
        );
    }
}

#[cfg(test)]
#[path = "ssh_workspace_host_linux_tests.rs"]
mod linux_fixture_tests;

#[cfg(test)]
#[path = "ssh_workspace_host_peer_tests.rs"]
mod peer_tests;
