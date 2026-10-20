//! Explicit, scoped remote compute on the existing pinned TLS runtime channel.
//! Launch profiles and durable admission are installed locally, never by a wire command.
use crate::{RuntimeCommand, SessionId};
use deppy_core::WorkspaceId;
use deppy_core::work_management::{AgentSelection, AttemptId, OperationId, RuntimeIncarnationId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(unix)]
pub(crate) type RemoteDirectory = std::os::fd::OwnedFd;
#[cfg(not(unix))]
pub(crate) type RemoteDirectory = std::fs::File;

pub const REMOTE_WORK_PROFILES_MAX: usize = 32;
pub const REMOTE_WORK_OPERATIONS_MAX: usize = 256;
pub const REMOTE_WORK_PROMPT_MAX: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteWorkerProfile {
    pub id: String,
    pub agent: AgentSelection,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteWorkerCapabilities {
    pub device_id: String,
    pub workspace_id: WorkspaceId,
    pub incarnation: RuntimeIncarnationId,
    pub folder: String,
    pub folder_anchor: String,
    pub grant_epoch: u64,
    pub lease_epoch: u64,
    pub profiles: Vec<RemoteWorkerProfile>,
    pub max_workers: u8,
    pub active_workers: u8,
    pub active_operation_ids: Vec<OperationId>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteWorkIdentity {
    pub device_id: String,
    pub workspace_id: WorkspaceId,
    pub attempt_id: AttemptId,
    pub operation_id: OperationId,
    pub incarnation: RuntimeIncarnationId,
    pub grant_epoch: u64,
    pub lease_epoch: u64,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteWorkRequest {
    pub identity: RemoteWorkIdentity,
    pub folder: String,
    pub folder_anchor: String,
    pub profile_id: String,
    pub initial_prompt: Option<String>,
}
impl std::fmt::Debug for RemoteWorkRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteWorkRequest")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteWorkError {
    Disabled,
    Invalid,
    WrongWorker,
    GrantChanged,
    FolderChanged,
    StaleLease,
    UnsupportedProfile,
    Capacity,
    JournalUnavailable,
    IdentityConflict,
    LaunchFailed,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteWorkResult {
    Launched {
        session: SessionId,
        execution_generation: u64,
    },
    Rejected {
        code: RemoteWorkError,
    },
    ObservationUnknown,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteWorkClaim {
    Pending,
    New,
    Existing(RemoteWorkResult),
    Conflict,
}

/// Nonblocking durable admission port. Backends enqueue SQLite work on their own bounded worker.
/// New is returned only after claim commit; Pending yields to the runtime event loop.
/// Existing unresolved claims return ObservationUnknown and are never reclaimed as New.
/// resolve returns true only after the result commit, false while its worker is pending.
pub trait RemoteWorkJournal: Send + Sync {
    fn claim(&self, request: &RemoteWorkRequest) -> anyhow::Result<RemoteWorkClaim>;
    fn resolve(
        &self,
        request: &RemoteWorkRequest,
        result: &RemoteWorkResult,
    ) -> anyhow::Result<bool>;
}
/// App adapters resolve the approved selection with the existing launcher/credential machinery.
/// This callback must only clone an already resolved launch template and append bounded prompt
/// arguments. No DB, keyring, network, subprocess, or filesystem work is allowed here.
/// Only SpawnAgent is accepted. The runtime pins its cwd atomically to the granted folder.
pub trait RemoteWorkLauncher: Send + Sync {
    fn prepare(
        &self,
        profile: &RemoteWorkerProfile,
        initial_prompt: Option<&str>,
    ) -> anyhow::Result<RuntimeCommand>;
}
pub struct RemoteWorkerConfig {
    pub device_id: String,
    pub workspace_id: WorkspaceId,
    pub folder: PathBuf,
    pub grant_epoch: u64,
    pub max_workers: u8,
    pub profiles: Vec<RemoteWorkerProfile>,
}
#[derive(Clone)]
pub struct RemoteWorkerControl {
    pub(crate) slot: Arc<std::sync::Mutex<RemoteWorkerSlot>>,
    pub(crate) wake: Option<std::thread::Thread>,
}
impl RemoteWorkerControl {
    /// Install from a host setup worker after all filesystem/template work completes.
    pub fn configure(&self, host: Option<RemoteWorkerHost>) -> anyhow::Result<()> {
        let mut slot = self
            .slot
            .lock()
            .map_err(|_| anyhow::anyhow!("remote_worker_lock_unavailable"))?;
        if let Some(host) = &host {
            anyhow::ensure!(
                slot.host.as_ref().is_none_or(|current| {
                    current.active.is_empty() && current.pending.is_empty()
                }),
                "remote_worker_grant_in_use"
            );
            anyhow::ensure!(
                host.config.grant_epoch > slot.grant_floor,
                "remote_worker_grant_epoch_stale"
            );
            slot.grant_floor = host.config.grant_epoch;
        }
        // Invalidate established connections only after all replacement preconditions pass.
        if let Some(access) = slot.access.take() {
            access.revoke();
        }
        slot.access = host
            .as_ref()
            .map(|h| crate::remote_access::ScopedRemoteAccess::new(&h.config));
        if let Some(mut previous) = slot.host.take() {
            let pending = std::mem::take(&mut previous.pending);
            for work in pending {
                match work.phase {
                    PendingPhase::Resolve(_) => previous.pending.push(work),
                    PendingPhase::Claim => slot
                        .notifications
                        .push((work.request.identity, RemoteWorkResult::ObservationUnknown)),
                }
            }
            if !previous.pending.is_empty() {
                slot.retired.push(previous);
            }
        }
        slot.host = host;
        if let Some(wake) = &self.wake {
            wake.unpark();
        }
        Ok(())
    }
    /// Copy on a setup worker. Reuses the current grant token; never rotates on Copy.
    /// The exact expected tuple prevents publishing a newer grant with stale UI metadata.
    pub fn issue_scoped_token(
        &self,
        device_id: &str,
        workspace_id: &WorkspaceId,
        grant_epoch: u64,
    ) -> anyhow::Result<secret::SecretString> {
        let slot = self
            .slot
            .lock()
            .map_err(|_| anyhow::anyhow!("remote_worker_lock_unavailable"))?;
        slot.access
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("remote_worker_disabled"))?
            .credential(device_id, workspace_id, grant_epoch)
    }
    pub(crate) fn authenticate_scoped_token(
        &self,
        provided: &[u8],
    ) -> Option<crate::remote_access::RemoteConnectionScope> {
        self.slot
            .lock()
            .ok()?
            .access
            .as_ref()?
            .authenticate(provided, Arc::downgrade(&self.slot))
    }
    /// Trusted host setup only; never callable by a runtime wire command.
    pub fn grant_attempt_folder(
        &self,
        attempt_id: &AttemptId,
        path: impl AsRef<std::path::Path>,
    ) -> anyhow::Result<AttemptFolderGrant> {
        anyhow::ensure!(
            id_valid(attempt_id.as_str()),
            "remote_worker_attempt_invalid"
        );
        let (device_id, workspace_id, grant_epoch) = {
            let slot = self
                .slot
                .lock()
                .map_err(|_| anyhow::anyhow!("remote_worker_lock_unavailable"))?;
            let host = slot
                .host
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("remote_worker_disabled"))?;
            (
                host.config.device_id.clone(),
                host.config.workspace_id.clone(),
                host.config.grant_epoch,
            )
        };
        // Canonicalization and descriptor acquisition are deliberately outside the runtime lock.
        let (folder, folder_anchor, directory) = open_directory(path.as_ref())?;
        let grant = AttemptFolderGrant {
            device_id,
            workspace_id,
            attempt_id: attempt_id.clone(),
            folder,
            folder_anchor,
            grant_epoch,
        };
        let mut slot = self
            .slot
            .lock()
            .map_err(|_| anyhow::anyhow!("remote_worker_lock_unavailable"))?;
        let host = slot
            .host
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("remote_worker_disabled"))?;
        anyhow::ensure!(
            host.config.device_id == grant.device_id
                && host.config.workspace_id == grant.workspace_id
                && host.config.grant_epoch == grant.grant_epoch,
            "remote_worker_grant_changed"
        );
        if let Some((previous, _)) = host
            .attempt_folders
            .iter()
            .find(|(g, _)| g.attempt_id == grant.attempt_id)
        {
            anyhow::ensure!(previous == &grant, "remote_worker_attempt_folder_immutable");
            return Ok(previous.clone());
        }
        anyhow::ensure!(
            host.attempt_folders.len() < 32,
            "remote_worker_attempt_folders_full"
        );
        host.attempt_folders.push((grant.clone(), directory));
        Ok(grant)
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptFolderGrant {
    pub device_id: String,
    pub workspace_id: WorkspaceId,
    pub attempt_id: AttemptId,
    pub folder: String,
    pub folder_anchor: String,
    pub grant_epoch: u64,
}
#[derive(Default)]
pub(crate) struct RemoteWorkerSlot {
    pub(crate) host: Option<RemoteWorkerHost>,
    pub(crate) grant_floor: u64,
    pub(crate) access: Option<crate::remote_access::ScopedRemoteAccess>,
    pub(crate) retired: Vec<RemoteWorkerHost>,
    pub(crate) notifications: Vec<(RemoteWorkIdentity, RemoteWorkResult)>,
}
pub(crate) enum PendingPhase {
    Claim,
    Resolve(RemoteWorkResult),
}
pub(crate) struct PendingWork {
    pub(crate) request: RemoteWorkRequest,
    pub(crate) phase: PendingPhase,
}
pub(crate) enum Progress {
    Complete(RemoteWorkResult),
    Pending(PendingPhase),
}
pub struct RemoteWorkerHost {
    pub(crate) config: RemoteWorkerConfig,
    pub(crate) folder: String,
    pub(crate) anchor: String,
    pub(crate) launcher: Arc<dyn RemoteWorkLauncher>,
    pub(crate) journal: Arc<dyn RemoteWorkJournal>,
    pub(crate) directory: Arc<RemoteDirectory>,
    pub(crate) leases: Vec<(u64, std::time::Instant)>,
    pub(crate) lease_counter: u64,
    pub(crate) execution_counter: u64,
    pub(crate) active: Vec<(OperationId, SessionId)>,
    pub(crate) executions: Vec<crate::remote_input::ExecutionRecord>,
    pub(crate) pending: Vec<PendingWork>,
    pub(crate) attempt_folders: Vec<(AttemptFolderGrant, Arc<RemoteDirectory>)>,
}
impl RemoteWorkerHost {
    /// Filesystem validation belongs on a host setup worker, outside rendering.
    pub fn new(
        config: RemoteWorkerConfig,
        launcher: Arc<dyn RemoteWorkLauncher>,
        journal: Arc<dyn RemoteWorkJournal>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            id_valid(&config.device_id)
                && id_valid(&config.workspace_id.0)
                && config.grant_epoch > 0
                && (1..=8).contains(&config.max_workers),
            "remote_worker_config_invalid"
        );
        anyhow::ensure!(
            !config.profiles.is_empty() && config.profiles.len() <= REMOTE_WORK_PROFILES_MAX,
            "remote_worker_profiles_invalid"
        );
        for (i, p) in config.profiles.iter().enumerate() {
            anyhow::ensure!(
                id_valid(&p.id)
                    && p.agent.validate().is_ok()
                    && !config.profiles[..i].iter().any(|other| other.id == p.id),
                "remote_worker_profile_invalid"
            );
        }
        let (folder, anchor, directory) = open_directory(&config.folder)?;
        Ok(Self {
            config,
            folder,
            anchor,
            launcher,
            journal,
            directory,
            leases: vec![],
            lease_counter: 0,
            execution_counter: 0,
            active: vec![],
            executions: vec![],
            pending: vec![],
            attempt_folders: vec![],
        })
    }
    pub(crate) fn directory_for(
        &self,
        request: &RemoteWorkRequest,
    ) -> Option<Arc<RemoteDirectory>> {
        if let Some((grant, directory)) = self
            .attempt_folders
            .iter()
            .find(|(grant, _)| grant.attempt_id == request.identity.attempt_id)
        {
            return (grant.folder == request.folder
                && grant.folder_anchor == request.folder_anchor)
                .then(|| Arc::clone(directory));
        }
        (request.folder == self.folder && request.folder_anchor == self.anchor)
            .then(|| Arc::clone(&self.directory))
    }
}
/// Host setup worker boundary: never run canonicalization or filesystem opens on the runtime loop.
fn open_directory(
    path: &std::path::Path,
) -> anyhow::Result<(String, String, Arc<RemoteDirectory>)> {
    let folder = std::fs::canonicalize(path)?;
    let folder = folder
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("remote_worker_folder_invalid"))?
        .to_owned();
    anyhow::ensure!(
        folder.len() <= 4096 && !folder.chars().any(char::is_control),
        "remote_worker_folder_invalid"
    );
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
    }
    let directory = options.open(&folder)?;
    let anchor = folder_anchor(&folder)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = directory.metadata()?;
        anyhow::ensure!(
            anchor == format!("{}:{}", metadata.dev(), metadata.ino()),
            "remote_worker_folder_changed"
        );
    }
    Ok((folder, anchor, Arc::new(directory.into())))
}
fn id_valid(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}
impl RemoteWorkIdentity {
    pub fn is_valid(&self) -> bool {
        id_valid(&self.device_id)
            && id_valid(&self.workspace_id.0)
            && id_valid(self.attempt_id.as_str())
            && id_valid(self.operation_id.as_str())
            && id_valid(self.incarnation.as_str())
            && self.grant_epoch > 0
            && self.lease_epoch > 0
    }
}
impl RemoteWorkRequest {
    pub fn is_valid(&self) -> bool {
        self.identity.is_valid()
            && id_valid(&self.folder_anchor)
            && id_valid(&self.profile_id)
            && self.folder.len() <= 4096
            && std::path::Path::new(&self.folder).is_absolute()
            && !self.folder.chars().any(char::is_control)
            && !self.folder.is_empty()
            && self.initial_prompt.as_ref().is_none_or(|prompt| {
                !prompt.is_empty()
                    && prompt.len() <= REMOTE_WORK_PROMPT_MAX
                    && !prompt.contains('\0')
            })
    }
    pub fn digest(&self) -> anyhow::Result<String> {
        let bytes = postcard::to_allocvec(self)?;
        Ok(Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    }
    pub(crate) fn canonicalize(&mut self) {
        self.identity.device_id.shrink_to_fit();
        self.identity.workspace_id.0.shrink_to_fit();
        self.identity.attempt_id.0.shrink_to_fit();
        self.identity.operation_id.0.shrink_to_fit();
        self.identity.incarnation.0.shrink_to_fit();
        self.folder.shrink_to_fit();
        self.folder_anchor.shrink_to_fit();
        self.profile_id.shrink_to_fit();
        if let Some(prompt) = &mut self.initial_prompt {
            prompt.shrink_to_fit();
        }
    }
    pub(crate) fn retained_bytes(&self) -> usize {
        self.identity.device_id.capacity()
            + self.identity.workspace_id.0.capacity()
            + self.identity.attempt_id.0.capacity()
            + self.identity.operation_id.0.capacity()
            + self.identity.incarnation.0.capacity()
            + self.folder.capacity()
            + self.folder_anchor.capacity()
            + self.profile_id.capacity()
            + self
                .initial_prompt
                .as_ref()
                .map_or(0, |prompt| prompt.capacity())
    }
}
impl RemoteWorkerCapabilities {
    pub fn is_valid(&self) -> bool {
        id_valid(&self.device_id)
            && id_valid(&self.workspace_id.0)
            && id_valid(self.incarnation.as_str())
            && id_valid(&self.folder_anchor)
            && self.folder.len() <= 4096
            && std::path::Path::new(&self.folder).is_absolute()
            && !self.folder.chars().any(char::is_control)
            && self.grant_epoch > 0
            && self.lease_epoch > 0
            && (1..=8).contains(&self.max_workers)
            && self.active_workers <= self.max_workers
            && self.active_operation_ids.len() <= usize::from(self.active_workers)
            && self.active_operation_ids.iter().enumerate().all(|(i, id)| {
                id_valid(id.as_str()) && !self.active_operation_ids[..i].contains(id)
            })
            && !self.profiles.is_empty()
            && self.profiles.len() <= REMOTE_WORK_PROFILES_MAX
            && self.profiles.iter().enumerate().all(|(i, p)| {
                id_valid(&p.id)
                    && p.agent.validate().is_ok()
                    && !self.profiles[..i].iter().any(|other| other.id == p.id)
            })
    }
}
pub(crate) fn folder_anchor(folder: &str) -> anyhow::Result<String> {
    let metadata = std::fs::metadata(folder)?;
    anyhow::ensure!(metadata.is_dir(), "remote_worker_folder_invalid");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(format!("{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        anyhow::bail!("remote_worker_folder_identity_unsupported")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        InProcessRuntimeClient, RemoteRuntimeClient, RemoteRuntimeServer, RuntimeCommandSink,
        RuntimeEvent, RuntimeEventReceiver, RuntimeEventStream, RuntimeSecret,
        RuntimeSecretResolver,
    };
    use secret::RedactionService;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    #[derive(Default)]
    struct Journal(Mutex<Vec<(String, RemoteWorkIdentity, Option<RemoteWorkResult>)>>);
    impl RemoteWorkJournal for Journal {
        fn claim(&self, r: &RemoteWorkRequest) -> anyhow::Result<RemoteWorkClaim> {
            let digest = r.digest()?;
            let mut rows = self.0.lock().unwrap();
            if let Some((old, id, result)) = rows.iter().find(|(_, id, _)| {
                id.operation_id == r.identity.operation_id || id.attempt_id == r.identity.attempt_id
            }) {
                return Ok(if *old == digest && *id == r.identity {
                    RemoteWorkClaim::Existing(
                        result
                            .clone()
                            .unwrap_or(RemoteWorkResult::ObservationUnknown),
                    )
                } else {
                    RemoteWorkClaim::Conflict
                });
            }
            anyhow::ensure!(rows.len() < REMOTE_WORK_OPERATIONS_MAX, "full");
            rows.push((digest, r.identity.clone(), None));
            Ok(RemoteWorkClaim::New)
        }
        fn resolve(
            &self,
            r: &RemoteWorkRequest,
            result: &RemoteWorkResult,
        ) -> anyhow::Result<bool> {
            let digest = r.digest()?;
            let mut rows = self.0.lock().unwrap();
            let row = rows
                .iter_mut()
                .find(|(d, _, _)| *d == digest)
                .ok_or_else(|| anyhow::anyhow!("missing"))?;
            row.2 = Some(result.clone());
            Ok(true)
        }
    }
    struct Resolver;
    impl RuntimeSecretResolver for Resolver {
        fn resolve(&self, _: &str) -> anyhow::Result<RuntimeSecret> {
            anyhow::bail!("fixture_has_no_secrets")
        }
    }
    struct Launcher;
    impl RemoteWorkLauncher for Launcher {
        fn prepare(
            &self,
            _: &RemoteWorkerProfile,
            _: Option<&str>,
        ) -> anyhow::Result<RuntimeCommand> {
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
    fn profile() -> RemoteWorkerProfile {
        RemoteWorkerProfile {
            id: "fixture-profile".into(),
            agent: AgentSelection {
                provider: "fixture".into(),
                model: None,
                effort: None,
            },
        }
    }
    fn identity() -> crate::tls_identity::TlsIdentity {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["deppy-remote".into()]).unwrap();
        crate::tls_identity::TlsIdentity {
            cert_der: cert.der().to_vec(),
            key_der: signing_key.serialize_der(),
        }
    }
    fn worker_with_launcher(
        root: &std::path::Path,
        id: &str,
        journal: Arc<dyn RemoteWorkJournal>,
        launcher: Arc<dyn RemoteWorkLauncher>,
    ) -> (RemoteRuntimeServer, String, RemoteWorkerControl) {
        std::fs::create_dir_all(root).unwrap();
        let backend = InProcessRuntimeClient::try_new_with_resolver(
            5,
            Arc::new(Resolver),
            root.join("logs"),
            RedactionService::new(),
            None,
            None,
            vec![],
        )
        .unwrap();
        backend
            .configure_remote_worker(Some(
                RemoteWorkerHost::new(
                    RemoteWorkerConfig {
                        device_id: id.into(),
                        workspace_id: WorkspaceId(format!("workspace-{id}")),
                        folder: root.to_path_buf(),
                        grant_epoch: 1,
                        max_workers: 2,
                        profiles: vec![profile()],
                    },
                    launcher,
                    journal,
                )
                .unwrap(),
            ))
            .unwrap();
        let control = backend.remote_worker_control();
        let identity = identity();
        let pin = identity.fingerprint();
        (
            RemoteRuntimeServer::serve_tls(
                backend,
                "127.0.0.1:0".parse().unwrap(),
                identity,
                false,
            )
            .unwrap(),
            pin,
            control,
        )
    }
    fn worker_with_control(
        root: &std::path::Path,
        id: &str,
        journal: Arc<dyn RemoteWorkJournal>,
    ) -> (RemoteRuntimeServer, String, RemoteWorkerControl) {
        worker_with_launcher(root, id, journal, Arc::new(Launcher))
    }
    fn worker(
        root: &std::path::Path,
        id: &str,
        journal: Arc<dyn RemoteWorkJournal>,
    ) -> (RemoteRuntimeServer, String) {
        let (server, pin, _) = worker_with_control(root, id, journal);
        (server, pin)
    }
    fn wait_event(
        rx: &RuntimeEventReceiver,
        accept: impl Fn(&RuntimeEvent) -> bool,
    ) -> RuntimeEvent {
        let until = Instant::now() + Duration::from_secs(8);
        loop {
            for event in rx.drain() {
                if accept(&event) {
                    return event;
                }
            }
            assert!(Instant::now() < until, "fixture response timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn inspect(
        client: &RemoteRuntimeClient,
        rx: &RuntimeEventReceiver,
    ) -> RemoteWorkerCapabilities {
        client
            .send_command(RuntimeCommand::InspectRemoteWorker { request_id: 1 })
            .unwrap();
        match wait_event(rx, |e| {
            matches!(e, RuntimeEvent::RemoteWorkerObserved { request_id: 1, .. })
        }) {
            RuntimeEvent::RemoteWorkerObserved {
                capabilities: Some(c),
                ..
            } => c,
            _ => panic!("registered fixture did not prove compute"),
        }
    }
    fn request(c: &RemoteWorkerCapabilities, op: &str) -> RemoteWorkRequest {
        RemoteWorkRequest {
            identity: RemoteWorkIdentity {
                device_id: c.device_id.clone(),
                workspace_id: c.workspace_id.clone(),
                attempt_id: AttemptId(format!("attempt-{op}")),
                operation_id: OperationId(op.into()),
                incarnation: c.incarnation.clone(),
                grant_epoch: c.grant_epoch,
                lease_epoch: c.lease_epoch,
            },
            folder: c.folder.clone(),
            folder_anchor: c.folder_anchor.clone(),
            profile_id: c.profiles[0].id.clone(),
            initial_prompt: Some("controlled fixture only".into()),
        }
    }
    #[test]
    fn scoped_tls_token_filters_unrelated_events_and_denies_raw_commands() {
        let root =
            std::env::temp_dir().join(format!("deppy-scoped-access-{}", uuid::Uuid::new_v4()));
        let (server, pin, control) =
            worker_with_control(&root, "scoped", Arc::new(Journal::default()));
        let workspace = WorkspaceId("workspace-scoped".into());
        let token = control.issue_scoped_token("scoped", &workspace, 1).unwrap();
        assert_eq!(
            token.expose(),
            control
                .issue_scoped_token("scoped", &workspace, 1)
                .unwrap()
                .expose()
        );
        assert!(control.issue_scoped_token("other", &workspace, 1).is_err());
        assert!(control.issue_scoped_token("scoped", &workspace, 2).is_err());
        let admin = RemoteRuntimeClient::attach_tls(server.local_addr(), server.auth_token(), &pin)
            .unwrap();
        let admin_events = admin.subscribe();
        let scoped =
            RemoteRuntimeClient::attach_tls(server.local_addr(), token.expose(), &pin).unwrap();
        let scoped_events = scoped.subscribe();
        let _ = inspect(&scoped, &scoped_events);
        admin
            .send_command(RuntimeCommand::RequestMuxSnapshot)
            .unwrap();
        wait_event(&admin_events, |e| {
            matches!(e, RuntimeEvent::MuxUpdated { .. })
        });
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            scoped_events
                .drain()
                .iter()
                .all(|e| matches!(e, RuntimeEvent::RemoteWorkerObserved { .. })),
            "scoped connection leaked unrelated runtime events"
        );
        let mut raw = Launcher.prepare(&profile(), None).unwrap();
        if let RuntimeCommand::SpawnAgent { command, args, .. } = &mut raw {
            *command = "/bin/sleep".into();
            *args = vec!["1".into()];
        }
        scoped.send_command(raw).unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        while !scoped_events.is_disconnected() && Instant::now() < until {
            scoped_events.drain();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            scoped_events.is_disconnected(),
            "scoped raw spawn must disconnect"
        );
        assert!(
            admin_events
                .drain()
                .iter()
                .all(|e| !matches!(e, RuntimeEvent::AgentSpawned { .. })),
            "scoped token bypassed profile/folder grant"
        );
        assert_eq!(inspect(&admin, &admin_events).active_workers, 0);
        control.configure(None).unwrap();
        assert!(control.issue_scoped_token("scoped", &workspace, 1).is_err());
        assert!(
            RemoteRuntimeClient::attach_tls(server.local_addr(), token.expose(), &pin).is_err()
        );
        drop(scoped);
        drop(admin);
        drop(server);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn remote_compute_two_pinned_tls_workers_are_isolated_and_lost_reply_does_not_respawn() {
        let root =
            std::env::temp_dir().join(format!("deppy-remote-compute-{}", uuid::Uuid::new_v4()));
        let a_dir = root.join("a");
        let b_dir = root.join("b");
        let (a, a_pin) = worker(&a_dir, "a", Arc::new(Journal::default()));
        let (b, b_pin) = worker(&b_dir, "b", Arc::new(Journal::default()));
        assert!(RemoteRuntimeClient::attach_tls(a.local_addr(), a.auth_token(), &b_pin).is_err());
        let client_a =
            RemoteRuntimeClient::attach_tls(a.local_addr(), a.auth_token(), &a_pin).unwrap();
        let rx_a = client_a.subscribe();
        let client_b =
            RemoteRuntimeClient::attach_tls(b.local_addr(), b.auth_token(), &b_pin).unwrap();
        let rx_b = client_b.subscribe();
        let proof_a = inspect(&client_a, &rx_a);
        let proof_b = inspect(&client_b, &rx_b);
        assert_ne!(proof_a.incarnation, proof_b.incarnation);
        let request_a = request(&proof_a, "op-a");
        let request_b = request(&proof_b, "op-b");
        client_b
            .send_command(RuntimeCommand::SpawnRemoteWork {
                request: request_a.clone(),
            })
            .unwrap();
        assert!(matches!(
            wait_event(&rx_b, |e| matches!(
                e,
                RuntimeEvent::RemoteWorkResolved { .. }
            )),
            RuntimeEvent::RemoteWorkResolved {
                result: RemoteWorkResult::Rejected {
                    code: RemoteWorkError::WrongWorker
                },
                ..
            }
        ));
        client_a
            .send_command(RuntimeCommand::SpawnRemoteWork {
                request: request_a.clone(),
            })
            .unwrap();
        // Drop without reading its admission reply, after observing the fixture process side effect.
        let until = Instant::now() + Duration::from_secs(8);
        while !a_dir.join("launches").exists() {
            if Instant::now() >= until {
                for event in rx_a.drain() {
                    if let RuntimeEvent::RemoteWorkResolved { result, .. } = event {
                        panic!("spawn result: {result:?}");
                    }
                }
                panic!("fixture did not create marker");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(client_a);
        client_b
            .send_command(RuntimeCommand::SpawnRemoteWork { request: request_b })
            .unwrap();
        let b_result = wait_event(&rx_b, |e| {
            matches!(
                e,
                RuntimeEvent::RemoteWorkResolved {
                    result: RemoteWorkResult::Launched { .. },
                    ..
                }
            )
        });
        let again =
            RemoteRuntimeClient::attach_tls(a.local_addr(), a.auth_token(), &a_pin).unwrap();
        let again_rx = again.subscribe();
        again
            .send_command(RuntimeCommand::SpawnRemoteWork {
                request: request_a.clone(),
            })
            .unwrap();
        let recovered = wait_event(&again_rx, |e| {
            matches!(e, RuntimeEvent::RemoteWorkResolved { .. })
        });
        assert!(matches!(
            recovered,
            RuntimeEvent::RemoteWorkResolved {
                result: RemoteWorkResult::Launched {
                    session: SessionId(1),
                    execution_generation: 1
                },
                ..
            }
        ));
        assert!(matches!(
            b_result,
            RuntimeEvent::RemoteWorkResolved {
                result: RemoteWorkResult::Launched {
                    session: SessionId(1),
                    ..
                },
                ..
            }
        ));
        let mut conflicting = request_a;
        conflicting.initial_prompt = Some("different work".into());
        again
            .send_command(RuntimeCommand::SpawnRemoteWork {
                request: conflicting,
            })
            .unwrap();
        assert!(matches!(
            wait_event(&again_rx, |e| matches!(
                e,
                RuntimeEvent::RemoteWorkResolved { .. }
            )),
            RuntimeEvent::RemoteWorkResolved {
                result: RemoteWorkResult::Rejected {
                    code: RemoteWorkError::IdentityConflict
                },
                ..
            }
        ));
        assert_eq!(
            std::fs::read_to_string(a_dir.join("launches")).unwrap(),
            "launch"
        );
        let until = Instant::now() + Duration::from_secs(8);
        while !b_dir.join("launches").exists() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::read_to_string(b_dir.join("launches")).unwrap(),
            "launch"
        );
        assert_eq!(
            std::fs::read_to_string(a_dir.join("actual-cwd"))
                .unwrap()
                .trim(),
            std::fs::canonicalize(&a_dir).unwrap().to_str().unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(b_dir.join("actual-cwd"))
                .unwrap()
                .trim(),
            std::fs::canonicalize(&b_dir).unwrap().to_str().unwrap()
        );
        drop(again);
        drop(client_b);
        a.shutdown();
        b.shutdown();
        std::fs::remove_dir_all(root).unwrap();
    }
    struct GatedJournal {
        gate: std::sync::atomic::AtomicBool,
        inner: Journal,
    }
    impl RemoteWorkJournal for GatedJournal {
        fn claim(&self, request: &RemoteWorkRequest) -> anyhow::Result<RemoteWorkClaim> {
            if !self.gate.load(std::sync::atomic::Ordering::SeqCst) {
                Ok(RemoteWorkClaim::Pending)
            } else {
                self.inner.claim(request)
            }
        }
        fn resolve(
            &self,
            request: &RemoteWorkRequest,
            result: &RemoteWorkResult,
        ) -> anyhow::Result<bool> {
            self.inner.resolve(request, result)
        }
    }
    #[test]
    fn scoped_tls_revoke_fences_connected_peers_and_delayed_claims_before_replacement() {
        let root =
            std::env::temp_dir().join(format!("deppy-scoped-revoke-{}", uuid::Uuid::new_v4()));
        let journal = Arc::new(GatedJournal {
            gate: std::sync::atomic::AtomicBool::new(false),
            inner: Journal::default(),
        });
        let (server, pin, control) = worker_with_control(&root, "fenced", journal.clone());
        let workspace = WorkspaceId("workspace-fenced".into());
        let token = control.issue_scoped_token("fenced", &workspace, 1).unwrap();
        let client =
            RemoteRuntimeClient::attach_tls(server.local_addr(), token.expose(), &pin).unwrap();
        let rx = client.subscribe();
        let caps = inspect(&client, &rx);
        client
            .send_command(RuntimeCommand::SpawnRemoteWork {
                request: request(&caps, "delayed-scoped-op"),
            })
            .unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        while control
            .slot
            .lock()
            .unwrap()
            .host
            .as_ref()
            .unwrap()
            .pending
            .is_empty()
        {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        }
        let replacement = || {
            RemoteWorkerHost::new(
                RemoteWorkerConfig {
                    device_id: "fenced".into(),
                    workspace_id: workspace.clone(),
                    folder: root.clone(),
                    grant_epoch: 2,
                    max_workers: 2,
                    profiles: vec![profile()],
                },
                Arc::new(Launcher),
                journal.clone(),
            )
            .unwrap()
        };
        assert!(
            control.configure(Some(replacement())).is_err(),
            "active pending grant cannot be replaced"
        );
        assert_eq!(
            token.expose(),
            control
                .issue_scoped_token("fenced", &workspace, 1)
                .unwrap()
                .expose(),
            "failed replacement must preserve existing credential"
        );
        control.configure(None).unwrap();
        journal
            .gate
            .store(true, std::sync::atomic::Ordering::SeqCst);
        control.configure(Some(replacement())).unwrap();
        let fresh = control.issue_scoped_token("fenced", &workspace, 2).unwrap();
        assert_ne!(token.expose(), fresh.expose());
        assert!(control.issue_scoped_token("fenced", &workspace, 1).is_err());
        assert!(
            RemoteRuntimeClient::attach_tls(server.local_addr(), token.expose(), &pin).is_err()
        );
        let until = Instant::now() + Duration::from_secs(3);
        while !rx.is_disconnected() && Instant::now() < until {
            rx.drain();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            rx.is_disconnected(),
            "revocation must retire established scoped connection"
        );
        let fresh_client =
            RemoteRuntimeClient::attach_tls(server.local_addr(), fresh.expose(), &pin).unwrap();
        let fresh_events = fresh_client.subscribe();
        let current = inspect(&fresh_client, &fresh_events);
        assert_eq!(current.grant_epoch, 2);
        assert_eq!(current.active_workers, 0);
        assert!(
            !root.join("launches").exists(),
            "revoked pending claim must never spawn later"
        );
        drop(client);
        drop(fresh_client);
        drop(server);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn remote_compute_pending_journal_keeps_runtime_responsive_without_early_spawn_or_ack() {
        let root =
            std::env::temp_dir().join(format!("deppy-pending-journal-{}", uuid::Uuid::new_v4()));
        let journal = Arc::new(GatedJournal {
            gate: std::sync::atomic::AtomicBool::new(false),
            inner: Journal::default(),
        });
        let (server, pin) = worker(&root, "pending", journal.clone());
        let client =
            RemoteRuntimeClient::attach_tls(server.local_addr(), server.auth_token(), &pin)
                .unwrap();
        let rx = client.subscribe();
        let proof = inspect(&client, &rx);
        client
            .send_command(RuntimeCommand::SpawnRemoteWork {
                request: request(&proof, "pending-op"),
            })
            .unwrap();
        client
            .send_command(RuntimeCommand::RequestMuxSnapshot)
            .unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        let mut seen_mux = false;
        let mut early_ack = false;
        while !seen_mux {
            for event in rx.drain() {
                match event {
                    RuntimeEvent::MuxUpdated { .. } => seen_mux = true,
                    RuntimeEvent::RemoteWorkResolved { .. } => early_ack = true,
                    _ => {}
                }
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!early_ack, "uncommitted journal emitted launch ACK");
        assert!(!root.join("launches").exists());
        journal
            .gate
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            wait_event(&rx, |e| matches!(
                e,
                RuntimeEvent::RemoteWorkResolved { .. }
            )),
            RuntimeEvent::RemoteWorkResolved {
                result: RemoteWorkResult::Launched { .. },
                ..
            }
        ));
        drop(client);
        server.shutdown();
        std::fs::remove_dir_all(root).unwrap();
    }
    struct ResolveGate {
        gate: std::sync::atomic::AtomicBool,
        inner: Journal,
    }
    impl RemoteWorkJournal for ResolveGate {
        fn claim(&self, request: &RemoteWorkRequest) -> anyhow::Result<RemoteWorkClaim> {
            self.inner.claim(request)
        }
        fn resolve(
            &self,
            request: &RemoteWorkRequest,
            result: &RemoteWorkResult,
        ) -> anyhow::Result<bool> {
            if self.gate.load(std::sync::atomic::Ordering::SeqCst) {
                self.inner.resolve(request, result)
            } else {
                Ok(false)
            }
        }
    }
    #[test]
    fn remote_compute_revoke_during_result_commit_never_claims_known_unsent() {
        let root =
            std::env::temp_dir().join(format!("deppy-revoke-resolve-{}", uuid::Uuid::new_v4()));
        let journal = Arc::new(ResolveGate {
            gate: std::sync::atomic::AtomicBool::new(false),
            inner: Journal::default(),
        });
        let (server, pin, control) = worker_with_control(&root, "revoked", journal.clone());
        let client =
            RemoteRuntimeClient::attach_tls(server.local_addr(), server.auth_token(), &pin)
                .unwrap();
        let rx = client.subscribe();
        let proof = inspect(&client, &rx);
        let request = request(&proof, "revoked-op");
        client
            .send_command(RuntimeCommand::SpawnRemoteWork {
                request: request.clone(),
            })
            .unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while !root.join("launches").exists() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        }
        control.configure(None).unwrap();
        client
            .send_command(RuntimeCommand::SpawnRemoteWork { request })
            .unwrap();
        assert!(matches!(
            wait_event(&rx, |e| matches!(
                e,
                RuntimeEvent::RemoteWorkResolved { .. }
            )),
            RuntimeEvent::RemoteWorkResolved {
                result: RemoteWorkResult::ObservationUnknown,
                ..
            }
        ));
        journal
            .gate
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            wait_event(&rx, |e| matches!(
                e,
                RuntimeEvent::RemoteWorkResolved { .. }
            )),
            RuntimeEvent::RemoteWorkResolved {
                result: RemoteWorkResult::Launched {
                    session: SessionId(1),
                    ..
                },
                ..
            }
        ));
        assert_eq!(
            std::fs::read_to_string(root.join("launches")).unwrap(),
            "launch"
        );
        drop(client);
        server.shutdown();
        std::fs::remove_dir_all(root).unwrap();
    }
    struct RelativeLauncher;
    impl RemoteWorkLauncher for RelativeLauncher {
        fn prepare(
            &self,
            profile: &RemoteWorkerProfile,
            prompt: Option<&str>,
        ) -> anyhow::Result<RuntimeCommand> {
            let mut launch = Launcher.prepare(profile, prompt)?;
            if let RuntimeCommand::SpawnAgent { command, .. } = &mut launch {
                *command = "sh".into();
            }
            Ok(launch)
        }
    }
    #[test]
    fn remote_compute_relative_executable_is_rejected_before_process_creation() {
        let root =
            std::env::temp_dir().join(format!("deppy-relative-launch-{}", uuid::Uuid::new_v4()));
        let (server, pin, _) = worker_with_launcher(
            &root,
            "relative",
            Arc::new(Journal::default()),
            Arc::new(RelativeLauncher),
        );
        let client =
            RemoteRuntimeClient::attach_tls(server.local_addr(), server.auth_token(), &pin)
                .unwrap();
        let rx = client.subscribe();
        let proof = inspect(&client, &rx);
        client
            .send_command(RuntimeCommand::SpawnRemoteWork {
                request: request(&proof, "relative-op"),
            })
            .unwrap();
        assert!(matches!(
            wait_event(&rx, |e| matches!(
                e,
                RuntimeEvent::RemoteWorkResolved { .. }
            )),
            RuntimeEvent::RemoteWorkResolved {
                result: RemoteWorkResult::Rejected {
                    code: RemoteWorkError::Invalid
                },
                ..
            }
        ));
        assert!(!root.join("launches").exists());
        drop(client);
        server.shutdown();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn remote_compute_configuration_cannot_replace_active_or_pending_grant() {
        let root =
            std::env::temp_dir().join(format!("deppy-active-grant-{}", uuid::Uuid::new_v4()));
        let make_host = |epoch| {
            std::fs::create_dir_all(&root).unwrap();
            RemoteWorkerHost::new(
                RemoteWorkerConfig {
                    device_id: "active".into(),
                    workspace_id: WorkspaceId("workspace-active".into()),
                    folder: root.clone(),
                    grant_epoch: epoch,
                    max_workers: 2,
                    profiles: vec![profile()],
                },
                Arc::new(Launcher),
                Arc::new(Journal::default()),
            )
            .unwrap()
        };
        let control = RemoteWorkerControl {
            slot: Arc::default(),
            wake: None,
        };
        control.configure(Some(make_host(1))).unwrap();
        control
            .slot
            .lock()
            .unwrap()
            .host
            .as_mut()
            .unwrap()
            .active
            .push((OperationId("existing-op".into()), SessionId(1)));
        assert!(control.configure(Some(make_host(2))).is_err());
        assert_eq!(control.slot.lock().unwrap().grant_floor, 1);
        control.configure(None).unwrap();
        control.configure(Some(make_host(2))).unwrap();
        {
            let mut slot = control.slot.lock().unwrap();
            let host = slot.host.as_mut().unwrap();
            let caps = RemoteWorkerCapabilities {
                device_id: "active".into(),
                workspace_id: WorkspaceId("workspace-active".into()),
                incarnation: RuntimeIncarnationId("runtime-active".into()),
                folder: host.folder.clone(),
                folder_anchor: host.anchor.clone(),
                grant_epoch: 2,
                lease_epoch: 1,
                profiles: vec![profile()],
                max_workers: 2,
                active_workers: 0,
                active_operation_ids: vec![],
            };
            host.pending.push(PendingWork {
                request: request(&caps, "pending-op"),
                phase: PendingPhase::Claim,
            });
        }
        assert!(control.configure(Some(make_host(3))).is_err());
        drop(control);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn remote_compute_attempt_folders_are_exact_pinned_and_preserve_other_workers() {
        let root =
            std::env::temp_dir().join(format!("deppy-attempt-folders-{}", uuid::Uuid::new_v4()));
        let project = root.join("project");
        let a = root.join("worktree-a");
        let b = root.join("worktree-b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let (server, pin, control) =
            worker_with_control(&project, "folders", Arc::new(Journal::default()));
        let client =
            RemoteRuntimeClient::attach_tls(server.local_addr(), server.auth_token(), &pin)
                .unwrap();
        let rx = client.subscribe();
        let caps = inspect(&client, &rx);
        let mut first = request(&caps, "folder-a");
        let mut second = request(&caps, "folder-b");
        let grant_a = control
            .grant_attempt_folder(&first.identity.attempt_id, &a)
            .unwrap();
        let grant_b = control
            .grant_attempt_folder(&second.identity.attempt_id, &b)
            .unwrap();
        assert_eq!(
            control
                .grant_attempt_folder(&first.identity.attempt_id, &a)
                .unwrap(),
            grant_a
        );
        assert!(
            control
                .grant_attempt_folder(&first.identity.attempt_id, &b)
                .is_err()
        );
        first.folder = grant_a.folder.clone();
        first.folder_anchor = grant_a.folder_anchor.clone();
        second.folder = grant_b.folder.clone();
        second.folder_anchor = grant_b.folder_anchor.clone();
        // The path may be replaced after setup; execution still enters the opened directory.
        let moved = root.join("worktree-a-moved");
        std::fs::rename(&a, &moved).unwrap();
        std::fs::create_dir(&a).unwrap();
        client
            .send_command(RuntimeCommand::SpawnRemoteWork {
                request: first.clone(),
            })
            .unwrap();
        assert!(matches!(
            wait_event(
                &rx,
                |e| matches!(e, RuntimeEvent::RemoteWorkResolved {identity,..} if identity.operation_id == first.identity.operation_id)
            ),
            RuntimeEvent::RemoteWorkResolved {
                result: RemoteWorkResult::Launched {
                    session: SessionId(1),
                    ..
                },
                ..
            }
        ));
        let mut wrong = second.clone();
        wrong.folder = grant_a.folder;
        wrong.folder_anchor = grant_a.folder_anchor;
        client
            .send_command(RuntimeCommand::SpawnRemoteWork { request: wrong })
            .unwrap();
        assert!(matches!(
            wait_event(
                &rx,
                |e| matches!(e, RuntimeEvent::RemoteWorkResolved {identity,..} if identity.operation_id == second.identity.operation_id)
            ),
            RuntimeEvent::RemoteWorkResolved {
                result: RemoteWorkResult::Rejected {
                    code: RemoteWorkError::FolderChanged
                },
                ..
            }
        ));
        client
            .send_command(RuntimeCommand::SpawnRemoteWork {
                request: second.clone(),
            })
            .unwrap();
        assert!(matches!(
            wait_event(
                &rx,
                |e| matches!(e, RuntimeEvent::RemoteWorkResolved {identity,..} if identity.operation_id == second.identity.operation_id)
            ),
            RuntimeEvent::RemoteWorkResolved {
                result: RemoteWorkResult::Launched {
                    session: SessionId(2),
                    ..
                },
                ..
            }
        ));
        let until = Instant::now() + Duration::from_secs(5);
        while !moved.join("actual-cwd").exists() || !b.join("actual-cwd").exists() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            std::fs::read_to_string(moved.join("actual-cwd"))
                .unwrap()
                .trim(),
            std::fs::canonicalize(&moved).unwrap().to_str().unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(b.join("actual-cwd"))
                .unwrap()
                .trim(),
            std::fs::canonicalize(&b).unwrap().to_str().unwrap()
        );
        assert!(!a.join("launches").exists());
        assert!(!project.join("launches").exists());
        let live = inspect(&client, &rx);
        assert_eq!(live.active_workers, 2);
        assert_eq!(live.active_operation_ids.len(), 2);
        drop(client);
        server.shutdown();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn remote_compute_attempt_folder_grants_are_bounded_and_revoked_with_parent() {
        let root =
            std::env::temp_dir().join(format!("deppy-grant-bounds-{}", uuid::Uuid::new_v4()));
        let (server, _, control) =
            worker_with_control(&root, "bounds", Arc::new(Journal::default()));
        assert!(
            control
                .grant_attempt_folder(&AttemptId("".into()), &root)
                .is_err()
        );
        assert!(
            control
                .grant_attempt_folder(&AttemptId("missing".into()), root.join("missing"))
                .is_err()
        );
        for index in 0..32 {
            control
                .grant_attempt_folder(&AttemptId(format!("attempt-{index}")), &root)
                .unwrap();
        }
        assert!(
            control
                .grant_attempt_folder(&AttemptId("overflow".into()), &root)
                .is_err()
        );
        control
            .grant_attempt_folder(&AttemptId("attempt-0".into()), &root)
            .unwrap();
        control.configure(None).unwrap();
        assert!(
            control
                .grant_attempt_folder(&AttemptId("attempt-0".into()), &root)
                .is_err()
        );
        server.shutdown();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn remote_compute_same_worker_previous_boot_is_uncertain_and_never_replayed() {
        let root = std::env::temp_dir().join(format!("deppy-stale-boot-{}", uuid::Uuid::new_v4()));
        let (server, pin) = worker(&root, "boot", Arc::new(Journal::default()));
        let client =
            RemoteRuntimeClient::attach_tls(server.local_addr(), server.auth_token(), &pin)
                .unwrap();
        let rx = client.subscribe();
        let proof = inspect(&client, &rx);
        let mut request = request(&proof, "previous-boot-op");
        request.identity.incarnation = RuntimeIncarnationId("previous-runtime-boot".into());
        client
            .send_command(RuntimeCommand::SpawnRemoteWork { request })
            .unwrap();
        assert!(matches!(
            wait_event(&rx, |e| matches!(
                e,
                RuntimeEvent::RemoteWorkResolved { .. }
            )),
            RuntimeEvent::RemoteWorkResolved {
                result: RemoteWorkResult::ObservationUnknown,
                ..
            }
        ));
        assert!(!root.join("launches").exists());
        drop(client);
        server.shutdown();
        std::fs::remove_dir_all(root).unwrap();
    }
    struct PromptLauncher;
    impl RemoteWorkLauncher for PromptLauncher {
        fn prepare(
            &self,
            _: &RemoteWorkerProfile,
            prompt: Option<&str>,
        ) -> anyhow::Result<RuntimeCommand> {
            anyhow::ensure!(prompt.is_none(), "idle only");
            let mut command = Launcher.prepare(&profile(), None)?;
            if let RuntimeCommand::SpawnAgent { args, .. } = &mut command {
                *args=vec!["-c".into(),"\"$@\"; exec /bin/sleep 2".into(),crate::agent_wrapper::AGENT_SESSION_WRAPPER_NAME.into(),"/bin/sh".into(),"-c".into(),"stty raw -echo; printf '\\033[?2004h❯ '; dd bs=1 count=25 of=accepted 2>/dev/null; sleep 2".into()];
            }
            Ok(command)
        }
    }
    #[test]
    fn remote_compute_guarded_input_requires_server_execution_proof_and_deduplicates() {
        use crate::remote_input::*;
        let root =
            std::env::temp_dir().join(format!("deppy-remote-input-{}", uuid::Uuid::new_v4()));
        let (a, pin, control) = worker_with_launcher(
            &root.join("a"),
            "a",
            Arc::new(Journal::default()),
            Arc::new(PromptLauncher),
        );
        control
            .slot
            .lock()
            .unwrap()
            .host
            .as_mut()
            .unwrap()
            .config
            .profiles[0]
            .agent
            .provider = "claude".into();
        let scoped_token = control
            .issue_scoped_token("a", &WorkspaceId("workspace-a".into()), 1)
            .unwrap();
        let client =
            RemoteRuntimeClient::attach_tls(a.local_addr(), scoped_token.expose(), &pin).unwrap();
        let events = client.subscribe();
        let caps = inspect(&client, &events);
        let mut work = request(&caps, "input-launch");
        work.initial_prompt = None;
        client
            .send_command(RuntimeCommand::SpawnRemoteWork {
                request: work.clone(),
            })
            .unwrap();
        let RuntimeEvent::RemoteWorkResolved {
            result:
                RemoteWorkResult::Launched {
                    session,
                    execution_generation,
                },
            ..
        } = wait_event(&events, |e| {
            matches!(e, RuntimeEvent::RemoteWorkResolved { .. })
        })
        else {
            panic!("launch failed")
        };
        let target = RemoteTaskTarget {
            identity: work.identity,
            session,
            execution_generation,
        };
        let (b, b_pin, b_control) = worker_with_launcher(
            &root.join("b"),
            "b",
            Arc::new(Journal::default()),
            Arc::new(PromptLauncher),
        );
        b_control
            .slot
            .lock()
            .unwrap()
            .host
            .as_mut()
            .unwrap()
            .config
            .profiles[0]
            .agent
            .provider = "claude".into();
        let b_client =
            RemoteRuntimeClient::attach_tls(b.local_addr(), b.auth_token(), &b_pin).unwrap();
        let b_events = b_client.subscribe();
        let b_caps = inspect(&b_client, &b_events);
        let mut b_work = request(&b_caps, "other-input-launch");
        b_work.initial_prompt = None;
        b_client
            .send_command(RuntimeCommand::SpawnRemoteWork {
                request: b_work.clone(),
            })
            .unwrap();
        let RuntimeEvent::RemoteWorkResolved {
            result:
                RemoteWorkResult::Launched {
                    session: b_session,
                    execution_generation: b_generation,
                },
            ..
        } = wait_event(&b_events, |e| {
            matches!(e, RuntimeEvent::RemoteWorkResolved { .. })
        })
        else {
            panic!("second launch failed")
        };
        assert_eq!(
            b_session, session,
            "two real workers intentionally alias numeric session IDs"
        );
        b_client
            .send_command(RuntimeCommand::RemoteTaskInput {
                request: RemoteTaskInputRequest::Observe {
                    target: target.clone(),
                    request_id: 8,
                },
            })
            .unwrap();
        assert!(matches!(
            wait_event(&b_events, |e| matches!(
                e,
                RuntimeEvent::RemoteTaskInput { .. }
            )),
            RuntimeEvent::RemoteTaskInput {
                reply: RemoteTaskInputReply::Observed {
                    execution: None,
                    ..
                },
                ..
            }
        ));
        let until = Instant::now() + Duration::from_secs(4);
        let proof = loop {
            client
                .send_command(RuntimeCommand::RemoteTaskInput {
                    request: RemoteTaskInputRequest::Observe {
                        target: target.clone(),
                        request_id: 7,
                    },
                })
                .unwrap();
            if let RuntimeEvent::RemoteTaskInput {
                reply:
                    RemoteTaskInputReply::Observed {
                        execution: Some(proof),
                        ..
                    },
                ..
            } = wait_event(&events, |e| {
                matches!(e, RuntimeEvent::RemoteTaskInput { .. })
            }) {
                break proof;
            }
            assert!(
                Instant::now() < until,
                "server did not capture a ready native execution"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(proof.foreground.is_current());
        assert_ne!(
            proof.execution, proof.foreground,
            "a known launcher wrapper must pin the inner CLI process, not its persistent shell"
        );
        for (destination, receiver, observed) in [
            (&b_client, &b_events, target.clone()),
            (
                &client,
                &events,
                RemoteTaskTarget {
                    execution_generation: target.execution_generation + 1,
                    ..target.clone()
                },
            ),
        ] {
            destination
                .send_command(RuntimeCommand::RemoteTaskInput {
                    request: RemoteTaskInputRequest::ObserveSession {
                        target: observed,
                        request_id: 9,
                    },
                })
                .unwrap();
            assert!(matches!(
                wait_event(receiver, |e| matches!(
                    e,
                    RuntimeEvent::RemoteTaskInput { .. }
                )),
                RuntimeEvent::RemoteTaskInput {
                    reply: RemoteTaskInputReply::SessionObserved { snapshot: None, .. },
                    ..
                }
            ));
        }
        client
            .send_command(RuntimeCommand::RemoteTaskInput {
                request: RemoteTaskInputRequest::ObserveSession {
                    target: target.clone(),
                    request_id: 10,
                },
            })
            .unwrap();
        let RuntimeEvent::RemoteTaskInput {
            reply:
                RemoteTaskInputReply::SessionObserved {
                    snapshot: Some(snapshot),
                    ..
                },
            ..
        } = wait_event(&events, |e| {
            matches!(e, RuntimeEvent::RemoteTaskInput { .. })
        })
        else {
            panic!("no scoped native terminal");
        };
        let terminal = snapshot.terminal.unwrap();
        assert!(terminal.history.is_some_and(|h| h.generation > 0));
        assert!(terminal.visible_cells.iter().any(|c| c.c == '❯'));
        let operation = OperationId::new();
        let input = RemoteTaskInputRequest::Deliver {
            target: target.clone(),
            execution_handle: proof.execution_handle.clone(),
            operation_id: operation.clone(),
            prompt: "bounded task".into(),
            deadline_unix_ms: unix_millis().unwrap() + 10_000,
        };
        b_client
            .send_command(RuntimeCommand::RemoteTaskInput {
                request: input.clone(),
            })
            .unwrap();
        assert!(matches!(
            wait_event(&b_events, |e| matches!(
                e,
                RuntimeEvent::RemoteTaskInput { .. }
            )),
            RuntimeEvent::RemoteTaskInput {
                reply: RemoteTaskInputReply::Delivered {
                    result: RemoteTaskInputResult::ObservationUnknown,
                    ..
                },
                ..
            }
        ));
        assert!(
            std::fs::read(root.join("b/accepted"))
                .unwrap_or_default()
                .is_empty()
        );
        let mut expired = input.clone();
        if let RemoteTaskInputRequest::Deliver {
            deadline_unix_ms, ..
        } = &mut expired
        {
            *deadline_unix_ms = unix_millis().unwrap() - 1;
        }
        client
            .send_command(RuntimeCommand::RemoteTaskInput { request: expired })
            .unwrap();
        assert!(matches!(
            wait_event(&events, |e| matches!(
                e,
                RuntimeEvent::RemoteTaskInput { .. }
            )),
            RuntimeEvent::RemoteTaskInput {
                reply: RemoteTaskInputReply::Delivered {
                    result: RemoteTaskInputResult::Rejected { .. },
                    ..
                },
                ..
            }
        ));
        let mut forged = input.clone();
        if let RemoteTaskInputRequest::Deliver {
            execution_handle, ..
        } = &mut forged
        {
            *execution_handle = uuid::Uuid::new_v4().to_string();
        }
        client
            .send_command(RuntimeCommand::RemoteTaskInput { request: forged })
            .unwrap();
        assert!(matches!(
            wait_event(&events, |e| matches!(
                e,
                RuntimeEvent::RemoteTaskInput { .. }
            )),
            RuntimeEvent::RemoteTaskInput {
                reply: RemoteTaskInputReply::Delivered {
                    result: RemoteTaskInputResult::Rejected { .. },
                    ..
                },
                ..
            }
        ));
        // A forged handle must not consume this valid operation's idempotency key.
        client
            .send_command(RuntimeCommand::RemoteTaskInput {
                request: input.clone(),
            })
            .unwrap();
        let until = Instant::now() + Duration::from_secs(4);
        let mut admitted = false;
        let mut accepted = 0;
        while !admitted || accepted == 0 {
            for event in events.drain() {
                match event {
                    RuntimeEvent::RemoteTaskInput {
                        reply:
                            RemoteTaskInputReply::Delivered {
                                operation_id,
                                result: RemoteTaskInputResult::Admitted,
                            },
                        ..
                    } if operation_id == operation => admitted = true,
                    RuntimeEvent::SessionInputAccepted {
                        operation_id: Some(id),
                        sequence,
                        at_micros,
                        ..
                    } if id == operation.0 => {
                        assert!(at_micros > 0);
                        assert_eq!(sequence, 1);
                        accepted += 1;
                    }
                    _ => {}
                }
            }
            assert!(
                Instant::now() < until,
                "input did not reach guarded PTY queue"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        client
            .send_command(RuntimeCommand::RemoteTaskInput { request: input })
            .unwrap();
        let until = Instant::now() + Duration::from_millis(100);
        while Instant::now() < until {
            for event in events.drain() {
                if matches!(event, RuntimeEvent::SessionInputAccepted { .. }) {
                    accepted += 1;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(accepted, 1, "duplicate task input must not enter PTY twice");
        assert_eq!(
            std::fs::read(root.join("a/accepted")).unwrap(),
            b"\x1b[200~bounded task\x1b[201~\r"
        );
        let close = RemoteTaskInputRequest::CloseSession {
            target: target.clone(),
            execution_handle: proof.execution_handle.clone(),
            operation_id: OperationId::new(),
            deadline_unix_ms: unix_millis().unwrap() + 10_000,
        };
        client
            .send_command(RuntimeCommand::RemoteTaskInput {
                request: close.clone(),
            })
            .unwrap();
        assert!(matches!(
            wait_event(&events, |e| matches!(
                e,
                RuntimeEvent::RemoteTaskInput { .. }
            )),
            RuntimeEvent::RemoteTaskInput {
                reply: RemoteTaskInputReply::SessionClosed {
                    result: RemoteSessionCloseResult::Closed,
                    ..
                },
                ..
            }
        ));
        assert!(!proof.foreground.is_current());
        let after_close = inspect(&client, &events);
        assert_eq!(after_close.active_workers, 0);
        let mut next = request(&after_close, "after-explicit-close");
        next.initial_prompt = None;
        client
            .send_command(RuntimeCommand::SpawnRemoteWork { request: next })
            .unwrap();
        assert!(matches!(
            wait_event(&events, |e| matches!(
                e,
                RuntimeEvent::RemoteWorkResolved { .. }
            )),
            RuntimeEvent::RemoteWorkResolved {
                result: RemoteWorkResult::Launched { .. },
                ..
            }
        ));
        // An exact duplicate close cannot touch this new worker.
        client
            .send_command(RuntimeCommand::RemoteTaskInput { request: close })
            .unwrap();
        assert!(matches!(
            wait_event(&events, |e| matches!(
                e,
                RuntimeEvent::RemoteTaskInput { .. }
            )),
            RuntimeEvent::RemoteTaskInput {
                reply: RemoteTaskInputReply::SessionClosed {
                    result: RemoteSessionCloseResult::Closed,
                    ..
                },
                ..
            }
        ));
        assert_eq!(inspect(&client, &events).active_workers, 1);
        let b_target = RemoteTaskTarget {
            identity: b_work.identity,
            session: b_session,
            execution_generation: b_generation,
        };
        let until = Instant::now() + Duration::from_secs(4);
        let b_proof = loop {
            b_client
                .send_command(RuntimeCommand::RemoteTaskInput {
                    request: RemoteTaskInputRequest::Observe {
                        target: b_target.clone(),
                        request_id: 11,
                    },
                })
                .unwrap();
            if let RuntimeEvent::RemoteTaskInput {
                reply:
                    RemoteTaskInputReply::Observed {
                        execution: Some(proof),
                        ..
                    },
                ..
            } = wait_event(&b_events, |e| {
                matches!(e, RuntimeEvent::RemoteTaskInput { .. })
            }) {
                break proof;
            }
            assert!(Instant::now() < until);
        };
        assert_ne!(b_proof.execution, b_proof.foreground);
        assert_eq!(
            unsafe { libc::kill(b_proof.execution.pid as i32, libc::SIGKILL) },
            0
        );
        while b_proof.execution.is_current() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            b_proof.foreground.is_current(),
            "persistent wrapper is still alive"
        );
        b_client
            .send_command(RuntimeCommand::RemoteTaskInput {
                request: RemoteTaskInputRequest::Deliver {
                    target: b_target,
                    execution_handle: b_proof.execution_handle.clone(),
                    operation_id: OperationId::new(),
                    prompt: "bounded task".into(),
                    deadline_unix_ms: unix_millis().unwrap() + 10_000,
                },
            })
            .unwrap();
        assert!(matches!(
            wait_event(&b_events, |e| matches!(
                e,
                RuntimeEvent::RemoteTaskInput { .. }
            )),
            RuntimeEvent::RemoteTaskInput {
                reply: RemoteTaskInputReply::Delivered {
                    result: RemoteTaskInputResult::Rejected { .. },
                    ..
                },
                ..
            }
        ));
        assert!(
            std::fs::read(root.join("b/accepted"))
                .unwrap_or_default()
                .is_empty()
        );
        drop(b_client);
        b.shutdown();
        drop(client);
        a.shutdown();
        std::fs::remove_dir_all(root).unwrap();
    }
}
