use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

use connector_contract::{
    ApprovalDecision, ApprovalPrompt, ConnectorIntent, ConnectorSnapshot, DiagnosticTransition,
    DiagnosticsSnapshot, EndpointDisplay, EndpointFingerprint, ErrorCode, ExternalLinkKind,
    ImportOutcome, ImportReport, ImportReportItem, ImportSource, ImportSourceRequest,
    OAuthRecoveryAction, OAuthUiPhase, OAuthUiState, OperationId, OperationKind, OperationPhase,
    OperationResult, OperationSummary, RemoteTrustPrompt, RemoteTrustPurpose, ResourceLimits,
    Revision, SensitiveInput, ServerDraft, ServerId, SlackRecoveryKind, ToolId, ToolPage,
    TransportDraft,
};

use crate::ports::{
    AuthorizedInvokeRequest, CancellationToken, ConnectorMcp, ConnectorOAuth, ConnectorRepository,
    ConnectorRepositoryFactory, ConnectorSecrets, CredentialResolutionRequest, DiscoverOutput,
    LiveToolSchema, McpRequestTarget, McpTransportSnapshot, OAuthAuthorizeOutput,
    OAuthContinuation, OAuthDiscovery, OAuthEventSink, OAuthFailure, OAuthRecoveryTarget,
    ServiceError, StoredOAuthClient,
};
use crate::snapshot::{SnapshotCell, SnapshotReader};

const TOOL_PAGE_SIZE: usize = 256;
const APPROVAL_PREVIEW_CHARS: usize = 500;

pub trait ConnectorHost: Send + Sync + 'static {
    /// Event-only wake. Implementations may coalesce calls but must not poll or retain payloads.
    fn wake(&self);
}

/// App-owned one-shot work. Dynamic URLs deliberately use the non-Clone/non-Serialize/redacted
/// contract input type and are moved exactly once into the app composition root.
pub enum HostAction {
    RequestImportSource {
        operation_id: OperationId,
        source: ImportSourceRequest,
    },
    OpenExternalLink {
        operation_id: OperationId,
        kind: ExternalLinkKind,
    },
    OpenOAuthBrowser {
        operation_id: OperationId,
        config_revision: Revision,
        url: SensitiveInput,
    },
    OpenSlackRecovery {
        operation_id: OperationId,
        server_id: ServerId,
        kind: SlackRecoveryKind,
        url: Option<SensitiveInput>,
    },
}

impl std::fmt::Debug for HostAction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RequestImportSource {
                operation_id,
                source,
            } => formatter
                .debug_struct("RequestImportSource")
                .field("operation_id", operation_id)
                .field("source", source)
                .finish(),
            Self::OpenExternalLink { operation_id, kind } => formatter
                .debug_struct("OpenExternalLink")
                .field("operation_id", operation_id)
                .field("kind", kind)
                .finish(),
            Self::OpenOAuthBrowser {
                operation_id,
                config_revision,
                ..
            } => formatter
                .debug_struct("OpenOAuthBrowser")
                .field("operation_id", operation_id)
                .field("config_revision", config_revision)
                .field("url", &"REDACTED")
                .finish(),
            Self::OpenSlackRecovery {
                operation_id,
                server_id,
                kind,
                ..
            } => formatter
                .debug_struct("OpenSlackRecovery")
                .field("operation_id", operation_id)
                .field("server_id", server_id)
                .field("kind", kind)
                .field("url", &"REDACTED")
                .finish(),
        }
    }
}

struct HostActionQueue {
    capacity: usize,
    queue: Mutex<VecDeque<HostAction>>,
    host: Arc<dyn ConnectorHost>,
}

impl HostActionQueue {
    fn new(capacity: usize, host: Arc<dyn ConnectorHost>) -> Self {
        Self {
            capacity,
            queue: Mutex::new(VecDeque::with_capacity(capacity)),
            host,
        }
    }

    fn push(&self, action: HostAction) -> Result<(), ServiceError> {
        let mut queue = self.queue.lock().expect("connector host action lock");
        if queue.len() >= self.capacity {
            return Err(ServiceError::new(
                ErrorCode::Backpressure,
                "connector host action queue is full",
            ));
        }
        queue.push_back(action);
        drop(queue);
        self.host.wake();
        Ok(())
    }

    fn pop(&self) -> Option<HostAction> {
        self.queue
            .lock()
            .expect("connector host action lock")
            .pop_front()
    }

    fn len(&self) -> usize {
        self.queue.lock().expect("connector host action lock").len()
    }
}

#[derive(Clone, PartialEq, Eq)]
struct TrustEntry {
    server_id: ServerId,
    config_revision: Revision,
    fingerprint: EndpointFingerprint,
}

struct SessionTrustRegistry {
    capacity: usize,
    entries: VecDeque<TrustEntry>,
}

impl SessionTrustRegistry {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: VecDeque::with_capacity(capacity),
        }
    }

    fn contains(&self, entry: &TrustEntry) -> bool {
        self.entries.contains(entry)
    }

    fn insert(&mut self, entry: TrustEntry) {
        self.entries.retain(|candidate| candidate != &entry);
        while self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    fn clear(&mut self) {
        self.entries.clear();
    }

    fn retain_revision(&mut self, revision: Revision) {
        self.entries
            .retain(|entry| entry.config_revision == revision);
    }
}

pub trait CoordinatorClock: Send + Sync + 'static {
    fn now(&self) -> Duration;

    /// Production returns `requested`. Tests may advance a fake monotonic clock and
    /// return zero so idle expiry is deterministic without sleeping.
    fn wait_duration(&self, requested: Duration) -> Duration {
        requested
    }
}

pub trait OperationIdFactory: Send + Sync + 'static {
    fn next_id(&self) -> OperationId;
}

pub struct SystemOperationIdFactory {
    prefix: String,
    counter: AtomicU64,
}

impl Default for SystemOperationIdFactory {
    fn default() -> Self {
        static FACTORY_SEQUENCE: AtomicU64 = AtomicU64::new(1);
        let sequence = FACTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        Self {
            prefix: format!("connector-{:x}-{nanos:x}-{sequence:x}", std::process::id()),
            counter: AtomicU64::new(1),
        }
    }
}

impl OperationIdFactory for SystemOperationIdFactory {
    fn next_id(&self) -> OperationId {
        let counter = self.counter.fetch_add(1, Ordering::Relaxed);
        OperationId::new(format!("{}-{counter:x}", self.prefix))
    }
}

pub struct SystemCoordinatorClock {
    origin: Instant,
}

impl Default for SystemCoordinatorClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl CoordinatorClock for SystemCoordinatorClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

pub struct ConnectorCoordinatorConfig {
    pub limits: ResourceLimits,
    pub idle_ttl: Duration,
    pub repository_factory: Arc<dyn ConnectorRepositoryFactory>,
    pub secrets: Arc<dyn ConnectorSecrets>,
    pub mcp: Arc<dyn ConnectorMcp>,
    pub oauth: Arc<dyn ConnectorOAuth>,
    pub host: Arc<dyn ConnectorHost>,
    pub clock: Arc<dyn CoordinatorClock>,
    pub operation_ids: Arc<dyn OperationIdFactory>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchOutcome {
    Queued,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchError {
    Backpressure,
    WorkerUnavailable,
    InvalidLimits,
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backpressure => f.write_str("connector command queue is full"),
            Self::WorkerUnavailable => f.write_str("connector worker is unavailable"),
            Self::InvalidLimits => f.write_str("connector resource limits are invalid"),
        }
    }
}

impl std::error::Error for DispatchError {}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConnectorServiceMetrics {
    pub worker_starts: usize,
    pub worker_alive: usize,
    pub command_queue_depth: usize,
    pub active_mcp_operations: usize,
    pub active_oauth_flows: usize,
    pub generation_entries: usize,
    pub stale_results: u64,
    pub backpressure_rejections: u64,
    pub transport: McpTransportSnapshot,
}

struct Metrics {
    worker_starts: AtomicUsize,
    worker_alive: AtomicUsize,
    queue_depth: AtomicUsize,
    active_mcp: AtomicUsize,
    active_oauth: AtomicUsize,
    generation_entries: AtomicUsize,
    stale_results: AtomicU64,
    backpressure: AtomicU64,
    cancellations: AtomicU64,
    timeouts: AtomicU64,
}

impl Metrics {
    fn new() -> Self {
        Self {
            worker_starts: AtomicUsize::new(0),
            worker_alive: AtomicUsize::new(0),
            queue_depth: AtomicUsize::new(0),
            active_mcp: AtomicUsize::new(0),
            active_oauth: AtomicUsize::new(0),
            generation_entries: AtomicUsize::new(0),
            stale_results: AtomicU64::new(0),
            backpressure: AtomicU64::new(0),
            cancellations: AtomicU64::new(0),
            timeouts: AtomicU64::new(0),
        }
    }
}

struct WorkerSlot {
    sender: SyncSender<CommandEnvelope>,
    thread: JoinHandle<()>,
    unparker: Thread,
}

struct CommandEnvelope {
    intent: ConnectorIntent,
    dispatch_epoch: u64,
}

struct Inner {
    config: ConnectorCoordinatorConfig,
    snapshot: Arc<SnapshotCell>,
    metrics: Arc<Metrics>,
    worker: Mutex<Option<WorkerSlot>>,
    dispatch_serialization: Mutex<()>,
    dispatch_epoch: Arc<AtomicU64>,
    cancellations: Arc<Mutex<HashMap<OperationId, CancellationToken>>>,
    host_actions: Arc<HostActionQueue>,
    session_trust: Arc<Mutex<SessionTrustRegistry>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let slot = self.worker.get_mut().expect("connector worker lock").take();
        if let Some(slot) = slot {
            drop(slot.sender);
            slot.unparker.unpark();
            let _ = slot.thread.join();
        }
    }
}

#[derive(Clone)]
pub struct ConnectorCoordinator {
    inner: Arc<Inner>,
}

impl ConnectorCoordinator {
    pub fn new(config: ConnectorCoordinatorConfig) -> Result<Self, DispatchError> {
        let limits = config
            .limits
            .validate()
            .map_err(|_| DispatchError::InvalidLimits)?;
        if config.idle_ttl.is_zero() {
            return Err(DispatchError::InvalidLimits);
        }
        let config = ConnectorCoordinatorConfig { limits, ..config };
        let host_actions = Arc::new(HostActionQueue::new(
            config.limits.host_actions,
            Arc::clone(&config.host),
        ));
        Ok(Self {
            inner: Arc::new(Inner {
                snapshot: Arc::new(SnapshotCell::new(Arc::clone(&config.host))),
                host_actions,
                session_trust: Arc::new(Mutex::new(SessionTrustRegistry::new(
                    config.limits.session_trust_entries,
                ))),
                config,
                metrics: Arc::new(Metrics::new()),
                worker: Mutex::new(None),
                dispatch_serialization: Mutex::new(()),
                dispatch_epoch: Arc::new(AtomicU64::new(0)),
                cancellations: Arc::new(Mutex::new(HashMap::new())),
            }),
        })
    }

    pub fn snapshot_reader(&self) -> SnapshotReader {
        SnapshotReader::new(Arc::clone(&self.inner.snapshot))
    }

    pub fn current_snapshot(&self) -> Arc<ConnectorSnapshot> {
        self.inner.snapshot.current()
    }

    pub fn try_take_host_action(&self) -> Option<HostAction> {
        self.inner.host_actions.pop()
    }

    pub fn host_action_depth(&self) -> usize {
        self.inner.host_actions.len()
    }

    pub fn dispatch(&self, intent: ConnectorIntent) -> Result<DispatchOutcome, DispatchError> {
        match intent {
            ConnectorIntent::RequestImportSource(source) => {
                self.inner
                    .host_actions
                    .push(HostAction::RequestImportSource {
                        operation_id: self.inner.config.operation_ids.next_id(),
                        source,
                    })
                    .map_err(|_| DispatchError::Backpressure)?;
                return Ok(DispatchOutcome::Queued);
            }
            ConnectorIntent::OpenExternalLink(kind) => {
                self.inner
                    .host_actions
                    .push(HostAction::OpenExternalLink {
                        operation_id: self.inner.config.operation_ids.next_id(),
                        kind,
                    })
                    .map_err(|_| DispatchError::Backpressure)?;
                return Ok(DispatchOutcome::Queued);
            }
            intent => {
                let _dispatch = self
                    .inner
                    .dispatch_serialization
                    .lock()
                    .expect("connector dispatch serialization");
                if let ConnectorIntent::Cancel(operation_id) = &intent
                    && let Some(cancellation) = self
                        .inner
                        .cancellations
                        .lock()
                        .expect("connector cancellation registry")
                        .get(operation_id)
                {
                    cancellation.cancel();
                }
                let dispatch_epoch = if invalidates_inflight(&intent) {
                    self.inner
                        .session_trust
                        .lock()
                        .expect("connector trust registry lock")
                        .clear();
                    let epoch = self.inner.dispatch_epoch.fetch_add(1, Ordering::AcqRel) + 1;
                    for cancellation in self
                        .inner
                        .cancellations
                        .lock()
                        .expect("connector cancellation registry")
                        .values()
                    {
                        cancellation.cancel();
                    }
                    epoch
                } else {
                    self.inner.dispatch_epoch.load(Ordering::Acquire)
                };
                self.enqueue(CommandEnvelope {
                    intent,
                    dispatch_epoch,
                })?;
            }
        }
        Ok(DispatchOutcome::Queued)
    }

    pub fn metrics(&self) -> ConnectorServiceMetrics {
        let metrics = &self.inner.metrics;
        ConnectorServiceMetrics {
            worker_starts: metrics.worker_starts.load(Ordering::Acquire),
            worker_alive: metrics.worker_alive.load(Ordering::Acquire),
            command_queue_depth: metrics.queue_depth.load(Ordering::Acquire),
            active_mcp_operations: metrics.active_mcp.load(Ordering::Acquire),
            active_oauth_flows: metrics.active_oauth.load(Ordering::Acquire),
            generation_entries: metrics.generation_entries.load(Ordering::Acquire),
            stale_results: metrics.stale_results.load(Ordering::Acquire),
            backpressure_rejections: metrics.backpressure.load(Ordering::Acquire),
            transport: self.inner.config.mcp.transport_metrics(),
        }
    }

    fn enqueue(&self, mut command: CommandEnvelope) -> Result<(), DispatchError> {
        for _attempt in 0..2 {
            let mut worker = self.inner.worker.lock().expect("connector worker lock");
            if worker
                .as_ref()
                .is_some_and(|slot| slot.thread.is_finished())
                && let Some(slot) = worker.take()
            {
                let _ = slot.thread.join();
            }
            if worker.is_none() {
                *worker = Some(self.spawn_worker()?);
            }
            let slot = worker.as_ref().expect("worker was initialized");
            self.inner
                .metrics
                .queue_depth
                .fetch_add(1, Ordering::AcqRel);
            match slot.sender.try_send(command) {
                Ok(()) => {
                    slot.unparker.unpark();
                    return Ok(());
                }
                Err(TrySendError::Full(returned)) => {
                    self.inner
                        .metrics
                        .queue_depth
                        .fetch_sub(1, Ordering::AcqRel);
                    self.inner
                        .metrics
                        .backpressure
                        .fetch_add(1, Ordering::AcqRel);
                    drop(returned);
                    return Err(DispatchError::Backpressure);
                }
                Err(TrySendError::Disconnected(returned)) => {
                    self.inner
                        .metrics
                        .queue_depth
                        .fetch_sub(1, Ordering::AcqRel);
                    command = returned;
                    if let Some(slot) = worker.take() {
                        let _ = slot.thread.join();
                    }
                }
            }
        }
        Err(DispatchError::WorkerUnavailable)
    }

    fn spawn_worker(&self) -> Result<WorkerSlot, DispatchError> {
        let (sender, receiver) = sync_channel(self.inner.config.limits.command_queue);
        let snapshot = Arc::clone(&self.inner.snapshot);
        let metrics = Arc::clone(&self.inner.metrics);
        let factory = Arc::clone(&self.inner.config.repository_factory);
        let secrets = Arc::clone(&self.inner.config.secrets);
        let mcp = Arc::clone(&self.inner.config.mcp);
        let oauth = Arc::clone(&self.inner.config.oauth);
        let clock = Arc::clone(&self.inner.config.clock);
        let limits = self.inner.config.limits;
        let idle_ttl = self.inner.config.idle_ttl;
        let operation_ids = Arc::clone(&self.inner.config.operation_ids);
        let dispatch_epoch = Arc::clone(&self.inner.dispatch_epoch);
        let cancellations = Arc::clone(&self.inner.cancellations);
        let host_actions = Arc::clone(&self.inner.host_actions);
        let session_trust = Arc::clone(&self.inner.session_trust);
        let thread = std::thread::Builder::new()
            .name("connector-coordinator".to_owned())
            .spawn(move || {
                metrics.worker_starts.fetch_add(1, Ordering::AcqRel);
                metrics.worker_alive.fetch_add(1, Ordering::AcqRel);
                let _alive = WorkerAlive(Arc::clone(&metrics));
                let repository = match factory.open() {
                    Ok(repository) => repository,
                    Err(error) => {
                        metrics.queue_depth.store(0, Ordering::Release);
                        publish_boot_error(&snapshot, error, operation_ids.next_id());
                        return;
                    }
                };
                Worker::new(
                    receiver,
                    snapshot,
                    metrics,
                    repository,
                    secrets,
                    mcp,
                    oauth,
                    clock,
                    limits,
                    idle_ttl,
                    operation_ids,
                    dispatch_epoch,
                    cancellations,
                    host_actions,
                    session_trust,
                )
                .run();
            })
            .map_err(|_| DispatchError::WorkerUnavailable)?;
        let unparker = thread.thread().clone();
        Ok(WorkerSlot {
            sender,
            thread,
            unparker,
        })
    }
}

struct WorkerAlive(Arc<Metrics>);

impl Drop for WorkerAlive {
    fn drop(&mut self) {
        self.0.worker_alive.fetch_sub(1, Ordering::AcqRel);
    }
}

fn publish_boot_error(snapshot: &SnapshotCell, error: ServiceError, operation_id: OperationId) {
    let mut next = ConnectorSnapshot {
        result: Some(OperationResult {
            operation_id,
            text: error.message.to_owned(),
            truncated: false,
        }),
        ..ConnectorSnapshot::default()
    };
    next.diagnostics.transitions = Arc::from([DiagnosticTransition {
        kind: OperationKind::SaveServer,
        phase: OperationPhase::Failed,
        error_code: Some(error.code),
    }]);
    snapshot.publish(next);
}

enum JobPayload {
    Discover(Result<DiscoverOutput, ServiceError>),
    InvokeSchema(Result<LiveToolSchema, ServiceError>),
    InvokeCall(Result<String, ServiceError>),
    OAuthDiscover(Result<OAuthDiscovery, ServiceError>),
    OAuthAuthorize(Result<OAuthAuthorizeOutput, ServiceError>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobStage {
    Discover,
    InvokeSchema,
    InvokeCall,
    OAuthDiscover,
    OAuthAuthorize,
}

impl JobStage {
    fn kind(self) -> OperationKind {
        match self {
            Self::Discover => OperationKind::Discover,
            Self::InvokeSchema | Self::InvokeCall => OperationKind::Invoke,
            Self::OAuthDiscover | Self::OAuthAuthorize => OperationKind::OAuth,
        }
    }
}

struct JobCompletion {
    operation_id: OperationId,
    server_id: ServerId,
    generation: u64,
    config_revision: Revision,
    dispatch_epoch: u64,
    stage: JobStage,
    payload: JobPayload,
}

struct ActiveJob {
    server_id: ServerId,
    generation: u64,
    config_revision: Revision,
    dispatch_epoch: u64,
    stage: JobStage,
    cancellation: CancellationToken,
    thread: Option<JoinHandle<()>>,
}

struct PendingInvocation {
    server_id: ServerId,
    server: ServerDraft,
    tool_id: ToolId,
    tool_name: String,
    live_schema_hash: String,
    arguments_json: connector_contract::SensitiveInput,
    generation: u64,
    config_revision: Revision,
    dispatch_epoch: u64,
    authorization: Option<audit::PendingAuthorization>,
    arguments_preview: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct PendingOutcome {
    outcome: audit::AuthorizationOutcome,
    stage: JobStage,
}

enum DeferredRemote {
    Discover {
        server: ServerDraft,
    },
    Invoke {
        server: ServerDraft,
        tool_id: ToolId,
        arguments_json: SensitiveInput,
    },
    OAuth {
        server: ServerDraft,
        choose_workspace: bool,
    },
}

struct PendingRemote {
    prompt: RemoteTrustPrompt,
    action: DeferredRemote,
}

struct PendingOAuth {
    operation_id: OperationId,
    server_id: ServerId,
    server_name: String,
    continuation: OAuthContinuation,
    generation: u64,
    config_revision: Revision,
    dispatch_epoch: u64,
}

struct StoredRecoveryTarget {
    operation_id: OperationId,
    server_id: ServerId,
    kind: SlackRecoveryKind,
    url: Option<SensitiveInput>,
}

struct OAuthWorkerEvent {
    operation_id: OperationId,
    server_id: ServerId,
    generation: u64,
    config_revision: Revision,
    dispatch_epoch: u64,
    authorization_url: SensitiveInput,
    acknowledgement: SyncSender<Result<(), ServiceError>>,
}

struct CoordinatorOAuthEventSink {
    sender: SyncSender<OAuthWorkerEvent>,
    unparker: Thread,
    operation_id: OperationId,
    server_id: ServerId,
    generation: u64,
    config_revision: Revision,
    dispatch_epoch: u64,
}

impl OAuthEventSink for CoordinatorOAuthEventSink {
    fn callback_bound(&self, authorization_url: SensitiveInput) -> Result<(), ServiceError> {
        let (acknowledgement, result) = sync_channel(0);
        self.sender
            .try_send(OAuthWorkerEvent {
                operation_id: self.operation_id.clone(),
                server_id: self.server_id.clone(),
                generation: self.generation,
                config_revision: self.config_revision,
                dispatch_epoch: self.dispatch_epoch,
                authorization_url,
                acknowledgement,
            })
            .map_err(|_| {
                ServiceError::new(
                    ErrorCode::Backpressure,
                    "connector OAuth event queue is unavailable",
                )
            })?;
        self.unparker.unpark();
        result.recv().unwrap_or_else(|_| {
            Err(ServiceError::new(
                ErrorCode::HostUnavailable,
                "connector host action acknowledgement is unavailable",
            ))
        })
    }
}

struct Worker {
    commands: Receiver<CommandEnvelope>,
    result_sender: SyncSender<JobCompletion>,
    results: Receiver<JobCompletion>,
    oauth_event_sender: SyncSender<OAuthWorkerEvent>,
    oauth_events: Receiver<OAuthWorkerEvent>,
    snapshot_cell: Arc<SnapshotCell>,
    snapshot: ConnectorSnapshot,
    metrics: Arc<Metrics>,
    repository: Box<dyn ConnectorRepository>,
    secrets: Arc<dyn ConnectorSecrets>,
    mcp: Arc<dyn ConnectorMcp>,
    oauth: Arc<dyn ConnectorOAuth>,
    clock: Arc<dyn CoordinatorClock>,
    limits: ResourceLimits,
    idle_ttl: Duration,
    last_activity: Duration,
    operation_ids: Arc<dyn OperationIdFactory>,
    dispatch_epoch: Arc<AtomicU64>,
    current_dispatch_epoch: u64,
    cancellations: Arc<Mutex<HashMap<OperationId, CancellationToken>>>,
    host_actions: Arc<HostActionQueue>,
    session_trust: Arc<Mutex<SessionTrustRegistry>>,
    generations: HashMap<ServerId, u64>,
    jobs: HashMap<OperationId, ActiveJob>,
    pending_invocations: HashMap<OperationId, PendingInvocation>,
    pending_outcomes: HashMap<OperationId, PendingOutcome>,
    pending_remote: Option<PendingRemote>,
    pending_oauth: Option<PendingOAuth>,
    slack_recovery: Option<StoredRecoveryTarget>,
    approval_order: VecDeque<OperationId>,
    operations: Vec<OperationSummary>,
    transitions: VecDeque<DiagnosticTransition>,
    needs_initial_overview: bool,
    shutting_down: bool,
}

impl Worker {
    #[allow(clippy::too_many_arguments)]
    fn new(
        commands: Receiver<CommandEnvelope>,
        snapshot_cell: Arc<SnapshotCell>,
        metrics: Arc<Metrics>,
        repository: Box<dyn ConnectorRepository>,
        secrets: Arc<dyn ConnectorSecrets>,
        mcp: Arc<dyn ConnectorMcp>,
        oauth: Arc<dyn ConnectorOAuth>,
        clock: Arc<dyn CoordinatorClock>,
        limits: ResourceLimits,
        idle_ttl: Duration,
        operation_ids: Arc<dyn OperationIdFactory>,
        dispatch_epoch: Arc<AtomicU64>,
        cancellations: Arc<Mutex<HashMap<OperationId, CancellationToken>>>,
        host_actions: Arc<HostActionQueue>,
        session_trust: Arc<Mutex<SessionTrustRegistry>>,
    ) -> Self {
        let (result_sender, results) = sync_channel(limits.mcp_operations + limits.oauth_flows);
        let (oauth_event_sender, oauth_events) = sync_channel(limits.oauth_flows);
        let last_activity = clock.now();
        let snapshot = (*snapshot_cell.current()).clone();
        let needs_initial_overview = snapshot.revision == Revision::ZERO;
        Self {
            commands,
            result_sender,
            results,
            oauth_event_sender,
            oauth_events,
            snapshot_cell,
            snapshot,
            metrics,
            repository,
            secrets,
            mcp,
            oauth,
            clock,
            limits,
            idle_ttl,
            last_activity,
            operation_ids,
            current_dispatch_epoch: dispatch_epoch.load(Ordering::Acquire),
            dispatch_epoch,
            cancellations,
            host_actions,
            session_trust,
            generations: HashMap::new(),
            jobs: HashMap::new(),
            pending_invocations: HashMap::new(),
            pending_outcomes: HashMap::new(),
            pending_remote: None,
            pending_oauth: None,
            slack_recovery: None,
            approval_order: VecDeque::new(),
            operations: Vec::new(),
            transitions: VecDeque::new(),
            needs_initial_overview,
            shutting_down: false,
        }
    }

    fn run(mut self) {
        loop {
            let mut progressed = self.drain_oauth_events();
            progressed |= self.drain_results();
            progressed |= self.drain_commands();
            if self.shutting_down && self.jobs.is_empty() && self.pending_invocations.is_empty() {
                break;
            }
            if progressed {
                continue;
            }
            if self.jobs.is_empty() {
                let elapsed = self.clock.now().saturating_sub(self.last_activity);
                let remaining = self.idle_ttl.saturating_sub(elapsed);
                if remaining.is_zero() {
                    self.expire_pending_waits();
                    self.retry_pending_outcomes();
                    self.mcp.reap_idle_leases();
                    if self.pending_remote.is_none()
                        && self.pending_oauth.is_none()
                        && self.pending_invocations.is_empty()
                        && self.mcp.active_leases() == 0
                    {
                        break;
                    }
                }
                let wait = self.clock.wait_duration(if remaining.is_zero() {
                    self.idle_ttl
                } else {
                    remaining
                });
                match self.commands.recv_timeout(wait) {
                    Ok(command) => self.accept_command(command),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        self.begin_shutdown();
                    }
                }
            } else {
                std::thread::park();
            }
        }
        self.begin_shutdown();
        while !self.jobs.is_empty() {
            match self.results.recv() {
                Ok(completion) => self.finish_job(completion),
                Err(_) => break,
            }
        }
        self.retry_pending_outcomes();
        self.release_pending_outcome_slots();
        if self.repository.shutdown().is_err() {
            self.publish_error(
                OperationKind::Invoke,
                ErrorCode::AuditUnavailable,
                "connector authorization shutdown could not be persisted",
            );
        }
    }

    fn drain_commands(&mut self) -> bool {
        let mut progressed = false;
        loop {
            match self.commands.try_recv() {
                Ok(command) => {
                    progressed = true;
                    self.accept_command(command);
                }
                Err(TryRecvError::Empty) => return progressed,
                Err(TryRecvError::Disconnected) => {
                    self.begin_shutdown();
                    return progressed;
                }
            }
        }
    }

    fn drain_results(&mut self) -> bool {
        let mut progressed = false;
        while let Ok(completion) = self.results.try_recv() {
            progressed = true;
            self.finish_job(completion);
        }
        progressed
    }

    fn drain_oauth_events(&mut self) -> bool {
        let mut progressed = false;
        while let Ok(event) = self.oauth_events.try_recv() {
            progressed = true;
            self.accept_oauth_event(event);
        }
        progressed
    }

    fn accept_command(&mut self, command: CommandEnvelope) {
        self.metrics.queue_depth.fetch_sub(1, Ordering::AcqRel);
        self.last_activity = self.clock.now();
        self.current_dispatch_epoch = command.dispatch_epoch;
        self.retry_pending_outcomes();
        self.discard_cancelled_pending();
        if self.needs_initial_overview {
            self.needs_initial_overview = false;
            if !matches!(&command.intent, ConnectorIntent::Activate) {
                self.reload_overview();
            }
        }
        self.handle_command(command.intent);
    }

    fn handle_command(&mut self, command: ConnectorIntent) {
        if !self.pending_outcomes.is_empty()
            && matches!(&command, ConnectorIntent::InvokeTool { .. })
        {
            self.publish_error(
                OperationKind::Invoke,
                ErrorCode::AuditUnavailable,
                "a prior tool outcome is awaiting durable audit persistence",
            );
            return;
        }
        match command {
            ConnectorIntent::Activate => self.reload_overview(),
            ConnectorIntent::SelectServer(server_id) => self.select_server(server_id),
            ConnectorIntent::RequestToolPage { server_id, offset } => {
                self.load_tool_page(server_id, offset);
            }
            ConnectorIntent::SaveServer(draft) => self.save_server(draft),
            ConnectorIntent::DeleteServer(server_id) => self.delete_server(server_id),
            ConnectorIntent::Discover(server_id) => self.start_discover(server_id),
            ConnectorIntent::InvokeTool {
                server_id,
                tool_id,
                arguments_json,
            } => self.start_invoke(server_id, tool_id, arguments_json),
            ConnectorIntent::Cancel(operation_id) => self.cancel(operation_id),
            ConnectorIntent::ResolveRemoteTrust {
                operation_id,
                config_revision,
                endpoint_fingerprint,
                accepted,
            } => self.resolve_remote_trust(
                operation_id,
                config_revision,
                endpoint_fingerprint,
                accepted,
            ),
            ConnectorIntent::BeginOAuth(server_id) | ConnectorIntent::RetryOAuth(server_id) => {
                self.start_oauth(server_id, false)
            }
            ConnectorIntent::ResolveOAuthConsent {
                operation_id,
                config_revision,
                accepted,
            } => self.resolve_oauth_consent(operation_id, config_revision, accepted),
            ConnectorIntent::SubmitOAuthClient {
                operation_id,
                config_revision,
                server_id,
                client_id,
                client_secret,
                workspace_hint,
            } => self.submit_oauth(
                operation_id,
                config_revision,
                StoredOAuthClient {
                    server_id,
                    client_id,
                    client_secret,
                    workspace_hint,
                },
            ),
            ConnectorIntent::SubmitSlackWorkspace {
                operation_id,
                config_revision,
                workspace,
            } => self.submit_slack_workspace(operation_id, config_revision, workspace),
            ConnectorIntent::ResolveOAuthRecovery {
                operation_id,
                action,
            } => self.resolve_oauth_recovery(operation_id, action),
            ConnectorIntent::SetPermission {
                server_id,
                tool_id,
                rule,
            } => self.set_permission(server_id, tool_id, rule),
            ConnectorIntent::ConnectSlack => self.connect_slack(),
            ConnectorIntent::ChooseSlackWorkspace(server_id) => self.start_oauth(server_id, true),
            ConnectorIntent::OpenSlackRecovery { server_id, kind } => {
                self.open_slack_recovery(server_id, kind)
            }
            ConnectorIntent::ImportConfiguration {
                source,
                display_name,
                contents,
            } => self.import_configuration(source, display_name, contents),
            ConnectorIntent::DismissResult(operation_id) => {
                if self
                    .snapshot
                    .result
                    .as_ref()
                    .is_some_and(|result| result.operation_id == operation_id)
                {
                    self.snapshot.result = None;
                    self.publish();
                }
            }
            ConnectorIntent::ResolveApproval {
                operation_id,
                decision,
            } => self.resolve_approval(operation_id, decision),
            ConnectorIntent::DismissImportReport(operation_id) => {
                if self
                    .snapshot
                    .import_report
                    .as_ref()
                    .is_some_and(|report| report.operation_id == operation_id)
                {
                    self.snapshot.import_report = None;
                    self.publish();
                }
            }
            ConnectorIntent::RequestImportSource(_) | ConnectorIntent::OpenExternalLink(_) => {}
        }
    }

    fn reload_overview(&mut self) {
        match self.repository.load_overview() {
            Ok(overview) => {
                if let Err(error) = validate_overview(&overview, self.limits) {
                    self.publish_service_error(OperationKind::SaveServer, error);
                    return;
                }
                let previous_revision = self.snapshot.config_revision;
                self.snapshot.config_revision = overview.config_revision;
                self.snapshot.slack = overview.slack;
                self.snapshot.servers = Arc::from(overview.servers);
                if previous_revision != self.snapshot.config_revision {
                    self.session_trust
                        .lock()
                        .expect("connector trust registry lock")
                        .retain_revision(self.snapshot.config_revision);
                }
                self.invalidate_stale_pending_invocations();
                self.prune_generations();
                if self
                    .snapshot
                    .selected_server
                    .as_ref()
                    .is_some_and(|selected| {
                        !self
                            .snapshot
                            .servers
                            .iter()
                            .any(|server| &server.id == selected)
                    })
                {
                    self.snapshot.selected_server = None;
                    self.snapshot.selected_server_config = None;
                    self.snapshot.tool_page = None;
                }
                self.publish();
            }
            Err(error) => self.publish_service_error(OperationKind::SaveServer, error),
        }
    }

    fn select_server(&mut self, server_id: Option<ServerId>) {
        self.snapshot.selected_server = server_id.clone();
        self.snapshot.tool_page = None;
        self.snapshot.selected_server_config = match server_id {
            Some(server_id) => match self.repository.load_server(&server_id) {
                Ok(server) => {
                    if let Err(error) = validate_loaded_server_id(&server_id, &server) {
                        self.publish_service_error(OperationKind::SaveServer, error);
                        return;
                    }
                    if let Err(error) = validate_server_draft(&server, self.limits) {
                        self.publish_service_error(OperationKind::SaveServer, error);
                        return;
                    }
                    Some(server)
                }
                Err(error) => {
                    self.publish_service_error(OperationKind::SaveServer, error);
                    return;
                }
            },
            None => None,
        };
        self.publish();
    }

    fn load_tool_page(&mut self, server_id: ServerId, offset: usize) {
        match self
            .repository
            .load_tool_page(&server_id, offset, TOOL_PAGE_SIZE)
        {
            Ok(page) if validate_tool_page(&page, self.limits).is_ok() => {
                self.snapshot.tool_page = Some(ToolPage {
                    server_id,
                    offset,
                    total: page.total,
                    items: Arc::from(page.items),
                });
                self.publish();
            }
            Ok(_) => self.publish_error(
                OperationKind::Discover,
                ErrorCode::LimitExceeded,
                "tool page limit exceeded",
            ),
            Err(error) => self.publish_service_error(OperationKind::Discover, error),
        }
    }

    fn save_server(&mut self, draft: ServerDraft) {
        if let Err(error) = validate_server_draft(&draft, self.limits) {
            self.publish_service_error(OperationKind::SaveServer, error);
            return;
        }
        if let Err(error) = self.repository.save_server(draft) {
            self.publish_service_error(OperationKind::SaveServer, error);
            return;
        }
        self.reload_overview();
    }

    fn delete_server(&mut self, server_id: ServerId) {
        self.bump_generation(&server_id);
        if let Err(error) = self.repository.delete_server(&server_id) {
            self.publish_service_error(OperationKind::DeleteServer, error);
            return;
        }
        self.reload_overview();
    }

    fn set_permission(
        &mut self,
        server_id: ServerId,
        tool_id: connector_contract::ToolId,
        rule: connector_contract::PermissionRule,
    ) {
        if let Err(error) = self.repository.set_permission(&server_id, &tool_id, rule) {
            self.publish_service_error(OperationKind::UpdatePermission, error);
            return;
        }
        self.reload_overview();
        self.load_tool_page(server_id, 0);
    }

    fn connect_slack(&mut self) {
        if let Err(error) = self.repository.ensure_slack_server() {
            self.publish_service_error(OperationKind::SaveServer, error);
            return;
        }
        self.reload_overview();
        let Some(server_id) = self.snapshot.slack.server_id.clone() else {
            self.publish_error(
                OperationKind::OAuth,
                ErrorCode::StorageUnavailable,
                "Slack server was not available after registration",
            );
            return;
        };
        self.start_oauth(server_id, false);
    }

    fn import_configuration(
        &mut self,
        source: ImportSource,
        display_name: Option<String>,
        contents: connector_contract::SensitiveInput,
    ) {
        let operation_id = self.new_operation_id();
        if contents.len() > self.limits.import_input_bytes {
            self.publish_operation_error(
                operation_id,
                OperationKind::Import,
                ErrorCode::LimitExceeded,
                "import input limit exceeded",
            );
            return;
        }
        let source_name = display_name.unwrap_or_else(|| match source {
            ImportSource::Paste => "pasted configuration".to_owned(),
            ImportSource::File => "configuration file".to_owned(),
            ImportSource::ClaudeDesktop => "Claude Desktop".to_owned(),
        });
        if source_name.len() > self.limits.import_report_bytes {
            self.publish_operation_error(
                operation_id,
                OperationKind::Import,
                ErrorCode::LimitExceeded,
                "import source name limit exceeded",
            );
            return;
        }
        let plan = match self
            .repository
            .parse_import(&source_name, contents.expose_bytes())
        {
            Ok(plan) => plan,
            Err(error) => {
                self.publish_service_error(OperationKind::Import, error);
                return;
            }
        };
        if plan.servers.len() > self.limits.import_servers {
            self.publish_operation_error(
                operation_id,
                OperationKind::Import,
                ErrorCode::LimitExceeded,
                "import server limit exceeded",
            );
            return;
        }
        if let Err(error) = validate_server_drafts(&plan.servers, self.limits) {
            self.publish_operation_error(
                operation_id,
                OperationKind::Import,
                error.code,
                error.message,
            );
            return;
        }
        if let Err(error) = validate_import_report_items(&plan.report, self.limits) {
            self.publish_operation_error(
                operation_id,
                OperationKind::Import,
                error.code,
                error.message,
            );
            return;
        }
        let mut report_items = plan.report;
        let added_servers = plan.servers.len();
        for server in &plan.servers {
            report_items.push(ImportReportItem {
                name: server.name.clone(),
                outcome: ImportOutcome::Added,
                error_code: None,
                omitted_secret_env_count: 0,
            });
        }
        let (report_items, truncated) = bound_import_report(report_items, self.limits);
        if let Err(error) = self.repository.import_servers(plan.servers) {
            self.publish_operation_error(
                operation_id,
                OperationKind::Import,
                error.code,
                error.message,
            );
            return;
        }
        let skipped = report_items
            .iter()
            .filter(|item| {
                matches!(
                    item.outcome,
                    ImportOutcome::SkippedDuplicate | ImportOutcome::SkippedUnsupported
                )
            })
            .count();
        let failed = report_items
            .iter()
            .filter(|item| item.outcome == ImportOutcome::Failed)
            .count();
        self.snapshot.import_report = Some(ImportReport {
            operation_id,
            source,
            added: added_servers,
            skipped,
            failed,
            items: Arc::from(report_items),
            truncated,
        });
        self.transition(OperationKind::Import, OperationPhase::Succeeded, None);
        self.reload_overview();
    }

    fn remote_after_trust(
        &mut self,
        operation_id: OperationId,
        server: &ServerDraft,
        purpose: RemoteTrustPurpose,
        action: DeferredRemote,
    ) -> Option<DeferredRemote> {
        let TransportDraft::Http { url } = &server.transport else {
            return Some(action);
        };
        let Some(server_id) = server.id.clone() else {
            self.publish_operation_error(
                operation_id,
                purpose_kind(purpose),
                ErrorCode::StorageUnavailable,
                "remote connector server has no stable identifier",
            );
            return None;
        };
        let fingerprint = endpoint_fingerprint_for(&server_id, self.snapshot.config_revision, url)
            .expect("validated HTTP endpoint has a SHA-256 fingerprint");
        let entry = TrustEntry {
            server_id: server_id.clone(),
            config_revision: self.snapshot.config_revision,
            fingerprint: fingerprint.clone(),
        };
        if self
            .session_trust
            .lock()
            .expect("connector trust registry lock")
            .contains(&entry)
        {
            return Some(action);
        }
        if self.pending_remote.is_some() {
            self.metrics.backpressure.fetch_add(1, Ordering::AcqRel);
            self.publish_operation_error(
                operation_id,
                purpose_kind(purpose),
                ErrorCode::Backpressure,
                "another remote trust decision is pending",
            );
            return None;
        }
        let prompt = RemoteTrustPrompt {
            operation_id: operation_id.clone(),
            server_id: server_id.clone(),
            server_name: server.name.clone(),
            purpose,
            display_endpoint: EndpointDisplay::new(url.clone()),
            endpoint_fingerprint: fingerprint,
            config_revision: self.snapshot.config_revision,
        };
        self.operations.push(OperationSummary {
            id: operation_id.clone(),
            server_id,
            kind: purpose_kind(purpose),
            phase: OperationPhase::AwaitingTrust,
            error_code: None,
        });
        self.pending_remote = Some(PendingRemote {
            prompt: prompt.clone(),
            action,
        });
        self.snapshot.remote_trust = Some(prompt);
        self.transition(OperationKind::Trust, OperationPhase::AwaitingTrust, None);
        self.publish();
        None
    }

    fn resolve_remote_trust(
        &mut self,
        operation_id: OperationId,
        config_revision: Revision,
        endpoint_fingerprint: EndpointFingerprint,
        accepted: bool,
    ) {
        let Some(pending) = self.pending_remote.take() else {
            self.metrics.stale_results.fetch_add(1, Ordering::AcqRel);
            return;
        };
        self.snapshot.remote_trust = None;
        let exact = pending.prompt.operation_id == operation_id
            && pending.prompt.config_revision == config_revision
            && pending.prompt.endpoint_fingerprint == endpoint_fingerprint
            && self.snapshot.config_revision == config_revision
            && self.dispatch_epoch.load(Ordering::Acquire) == self.current_dispatch_epoch;
        if !exact {
            self.finish_waiting_operation(
                &operation_id,
                OperationKind::Trust,
                OperationPhase::Cancelled,
                Some(ErrorCode::StaleResult),
            );
            self.metrics.stale_results.fetch_add(1, Ordering::AcqRel);
            return;
        }
        if !accepted {
            self.finish_waiting_operation(
                &operation_id,
                OperationKind::Trust,
                OperationPhase::Denied,
                Some(ErrorCode::TrustDenied),
            );
            return;
        }
        let server_id = pending.prompt.server_id.clone();
        let server = match self.repository.load_server(&server_id) {
            Ok(server) => server,
            Err(error) => {
                self.finish_waiting_operation(
                    &operation_id,
                    purpose_kind(pending.prompt.purpose),
                    OperationPhase::Failed,
                    Some(error.code),
                );
                return;
            }
        };
        let live_fingerprint = match &server.transport {
            TransportDraft::Http { url } => {
                endpoint_fingerprint_for(&server_id, config_revision, url)
            }
            TransportDraft::Stdio { .. } => None,
        };
        if validate_loaded_server_id(&server_id, &server).is_err()
            || validate_server_draft(&server, self.limits).is_err()
            || live_fingerprint.as_ref() != Some(&endpoint_fingerprint)
        {
            self.finish_waiting_operation(
                &operation_id,
                OperationKind::Trust,
                OperationPhase::Cancelled,
                Some(ErrorCode::StaleResult),
            );
            self.metrics.stale_results.fetch_add(1, Ordering::AcqRel);
            return;
        }
        self.session_trust
            .lock()
            .expect("connector trust registry lock")
            .insert(TrustEntry {
                server_id: server_id.clone(),
                config_revision,
                fingerprint: endpoint_fingerprint,
            });
        match pending.action {
            DeferredRemote::Discover { .. } => {
                self.start_discover_with_id(operation_id, server_id);
            }
            DeferredRemote::Invoke {
                tool_id,
                arguments_json,
                ..
            } => self.start_invoke_with_id(operation_id, server_id, tool_id, arguments_json),
            DeferredRemote::OAuth {
                choose_workspace, ..
            } => self.start_oauth_with_id(operation_id, server_id, choose_workspace),
        }
    }

    fn finish_waiting_operation(
        &mut self,
        operation_id: &OperationId,
        kind: OperationKind,
        phase: OperationPhase,
        error_code: Option<ErrorCode>,
    ) {
        self.operations
            .retain(|operation| &operation.id != operation_id);
        self.cancellations
            .lock()
            .expect("connector cancellation registry")
            .remove(operation_id);
        self.transition(kind, phase, error_code);
        self.publish();
    }

    fn start_discover(&mut self, server_id: ServerId) {
        let operation_id = self.new_operation_id();
        self.start_discover_with_id(operation_id, server_id);
    }

    fn start_discover_with_id(&mut self, operation_id: OperationId, server_id: ServerId) {
        if self.metrics.active_mcp.load(Ordering::Acquire) >= self.limits.mcp_operations {
            self.reject_backpressure(OperationKind::Discover);
            return;
        }
        let server = match self.repository.load_server(&server_id) {
            Ok(server) => server,
            Err(error) => {
                self.publish_service_error(OperationKind::Discover, error);
                return;
            }
        };
        if let Err(error) = validate_loaded_server_id(&server_id, &server) {
            self.publish_service_error(OperationKind::Discover, error);
            return;
        }
        if let Err(error) = validate_server_draft(&server, self.limits) {
            self.publish_service_error(OperationKind::Discover, error);
            return;
        }
        let Some(DeferredRemote::Discover { server }) = self.remote_after_trust(
            operation_id.clone(),
            &server,
            RemoteTrustPurpose::Discover,
            DeferredRemote::Discover {
                server: server.clone(),
            },
        ) else {
            return;
        };
        let target = match self.prepare_mcp_target(server) {
            Ok(target) => target,
            Err(error) => {
                self.publish_operation_error(
                    operation_id,
                    OperationKind::Discover,
                    error.code,
                    error.message,
                );
                return;
            }
        };
        let generation = self.bump_generation(&server_id);
        let config_revision = self.snapshot.config_revision;
        let dispatch_epoch = self.current_dispatch_epoch;
        let cancellation = CancellationToken::default();
        let mcp = Arc::clone(&self.mcp);
        let sender = self.result_sender.clone();
        let unparker = std::thread::current();
        let operation_for_thread = operation_id.clone();
        let server_for_thread = server_id.clone();
        let cancellation_for_thread = cancellation.clone();
        let thread = std::thread::Builder::new()
            .name("connector-mcp-discover".to_owned())
            .spawn(move || {
                let payload = JobPayload::Discover(run_backend_job(|| {
                    mcp.discover(&operation_for_thread, target, cancellation_for_thread)
                }));
                let _ = sender.send(JobCompletion {
                    operation_id: operation_for_thread,
                    server_id: server_for_thread,
                    generation,
                    config_revision,
                    dispatch_epoch,
                    stage: JobStage::Discover,
                    payload,
                });
                unparker.unpark();
            });
        let _ = self.insert_job(
            operation_id,
            server_id,
            generation,
            config_revision,
            JobStage::Discover,
            cancellation,
            thread,
        );
    }

    fn start_invoke(
        &mut self,
        server_id: ServerId,
        tool_id: ToolId,
        arguments_json: connector_contract::SensitiveInput,
    ) {
        let operation_id = self.new_operation_id();
        self.start_invoke_with_id(operation_id, server_id, tool_id, arguments_json);
    }

    fn start_invoke_with_id(
        &mut self,
        operation_id: OperationId,
        server_id: ServerId,
        tool_id: ToolId,
        arguments_json: connector_contract::SensitiveInput,
    ) {
        if audit::validate_tool_input(arguments_json.expose_bytes()).is_err() {
            self.publish_operation_error(
                operation_id,
                OperationKind::Invoke,
                ErrorCode::InvalidInput,
                "tool input must be a bounded JSON object",
            );
            return;
        }
        if self.metrics.active_mcp.load(Ordering::Acquire) >= self.limits.mcp_operations {
            self.publish_operation_error(
                operation_id,
                OperationKind::Invoke,
                ErrorCode::Backpressure,
                "connector operation limit reached",
            );
            self.metrics.backpressure.fetch_add(1, Ordering::AcqRel);
            return;
        }
        let server = match self.repository.load_server(&server_id) {
            Ok(server) => server,
            Err(error) => {
                self.publish_operation_error(
                    operation_id,
                    OperationKind::Invoke,
                    error.code,
                    error.message,
                );
                return;
            }
        };
        if let Err(error) = validate_loaded_server_id(&server_id, &server) {
            self.publish_operation_error(
                operation_id,
                OperationKind::Invoke,
                error.code,
                error.message,
            );
            return;
        }
        if let Err(error) = validate_server_draft(&server, self.limits) {
            self.publish_operation_error(
                operation_id,
                OperationKind::Invoke,
                error.code,
                error.message,
            );
            return;
        }
        let Some(DeferredRemote::Invoke {
            server,
            tool_id,
            arguments_json,
        }) = self.remote_after_trust(
            operation_id.clone(),
            &server,
            RemoteTrustPurpose::Invoke,
            DeferredRemote::Invoke {
                server: server.clone(),
                tool_id: tool_id.clone(),
                arguments_json,
            },
        )
        else {
            return;
        };
        let target = match self.prepare_mcp_target(server.clone()) {
            Ok(target) => target,
            Err(error) => {
                self.publish_operation_error(
                    operation_id,
                    OperationKind::Invoke,
                    error.code,
                    error.message,
                );
                return;
            }
        };
        let tool_name = match self.repository.load_tool_name(&server_id, &tool_id) {
            Ok(tool_name) if !tool_name.trim().is_empty() => tool_name,
            Ok(_) => {
                self.publish_operation_error(
                    operation_id,
                    OperationKind::Invoke,
                    ErrorCode::StorageUnavailable,
                    "stored tool name is invalid",
                );
                return;
            }
            Err(error) => {
                self.publish_operation_error(
                    operation_id,
                    OperationKind::Invoke,
                    error.code,
                    error.message,
                );
                return;
            }
        };
        let generation = self.bump_generation(&server_id);
        let config_revision = self.snapshot.config_revision;
        let dispatch_epoch = self.current_dispatch_epoch;
        let cancellation = CancellationToken::default();
        let mcp = Arc::clone(&self.mcp);
        let sender = self.result_sender.clone();
        let unparker = std::thread::current();
        let operation_for_thread = operation_id.clone();
        let server_for_thread = server_id.clone();
        let tool_for_schema = tool_id.clone();
        let tool_name_for_schema = tool_name.clone();
        let cancellation_for_thread = cancellation.clone();
        let thread = std::thread::Builder::new()
            .name("connector-mcp-invoke".to_owned())
            .spawn(move || {
                let payload = JobPayload::InvokeSchema(run_backend_job(|| {
                    mcp.load_live_schema(
                        &operation_for_thread,
                        target,
                        tool_for_schema,
                        tool_name_for_schema,
                        cancellation_for_thread,
                    )
                }));
                let _ = sender.send(JobCompletion {
                    operation_id: operation_for_thread,
                    server_id: server_for_thread,
                    generation,
                    config_revision,
                    dispatch_epoch,
                    stage: JobStage::InvokeSchema,
                    payload,
                });
                unparker.unpark();
            });
        if !self.insert_job(
            operation_id.clone(),
            server_id.clone(),
            generation,
            config_revision,
            JobStage::InvokeSchema,
            cancellation,
            thread,
        ) {
            return;
        }
        if let Some(job) = self.jobs.get_mut(&operation_id) {
            job.stage = JobStage::InvokeSchema;
        }
        if let Some(operation) = self
            .operations
            .iter_mut()
            .find(|operation| operation.id == operation_id)
        {
            operation.phase = OperationPhase::DiscoveringSchema;
        }
        // Keep the only raw arguments owner in the worker while live schema is loaded.
        self.pending_invocations.insert(
            operation_id,
            PendingInvocation {
                server_id,
                server,
                tool_id,
                tool_name,
                live_schema_hash: String::new(),
                arguments_json,
                generation,
                config_revision,
                dispatch_epoch,
                authorization: None,
                arguments_preview: None,
            },
        );
        self.publish();
    }

    fn prepare_mcp_target(
        &mut self,
        server: ServerDraft,
    ) -> Result<McpRequestTarget, ServiceError> {
        let server_id = server.id.as_ref().ok_or_else(|| {
            ServiceError::new(
                ErrorCode::StorageUnavailable,
                "connector server has no stable identifier",
            )
        })?;
        let requests = match &server.transport {
            TransportDraft::Stdio { secret_env, .. } => {
                let expected_ids = secret_env
                    .iter()
                    .map(|(_, credential_id)| credential_id.clone())
                    .collect::<Vec<_>>();
                let requests = self.secrets.credential_revisions(expected_ids.clone())?;
                if requests.len() != expected_ids.len()
                    || requests
                        .iter()
                        .zip(&expected_ids)
                        .any(|(request, expected)| {
                            &request.credential_id != expected
                                || request.expected_physical_slot.is_none()
                        })
                {
                    return Err(ServiceError::new(
                        ErrorCode::SecretUnavailable,
                        "credential pointer index returned a mismatched revision",
                    ));
                }
                requests
            }
            TransportDraft::Http { url } => self
                .repository
                .load_http_auth_binding(server_id, url)?
                .map_or_else(Vec::new, |binding| {
                    vec![CredentialResolutionRequest {
                        credential_id: binding.credential_id,
                        expected_physical_slot: Some(binding.physical_slot),
                    }]
                }),
        };
        Ok(McpRequestTarget {
            server,
            config_revision: self.snapshot.config_revision,
            credential_revisions: requests,
        })
    }

    fn start_oauth(&mut self, server_id: ServerId, choose_workspace: bool) {
        let operation_id = self.new_operation_id();
        self.start_oauth_with_id(operation_id, server_id, choose_workspace);
    }

    fn start_oauth_with_id(
        &mut self,
        operation_id: OperationId,
        server_id: ServerId,
        choose_workspace: bool,
    ) {
        if self.metrics.active_oauth.load(Ordering::Acquire) >= self.limits.oauth_flows
            || self.pending_oauth.is_some()
        {
            self.reject_backpressure(OperationKind::OAuth);
            return;
        }
        let server = match self.repository.load_server(&server_id) {
            Ok(server) => server,
            Err(error) => {
                self.publish_service_error(OperationKind::OAuth, error);
                return;
            }
        };
        if let Err(error) = validate_loaded_server_id(&server_id, &server) {
            self.publish_service_error(OperationKind::OAuth, error);
            return;
        }
        if let Err(error) = validate_server_draft(&server, self.limits) {
            self.publish_service_error(OperationKind::OAuth, error);
            return;
        }
        let Some(DeferredRemote::OAuth {
            server,
            choose_workspace,
        }) = self.remote_after_trust(
            operation_id.clone(),
            &server,
            RemoteTrustPurpose::OAuth,
            DeferredRemote::OAuth {
                server: server.clone(),
                choose_workspace,
            },
        )
        else {
            return;
        };
        self.spawn_oauth_discovery(operation_id, server_id, server, choose_workspace);
    }

    fn submit_oauth(
        &mut self,
        operation_id: OperationId,
        config_revision: Revision,
        client: StoredOAuthClient,
    ) {
        if let Err(error) = validate_oauth_client(&client, self.limits) {
            self.publish_service_error(OperationKind::OAuth, error);
            return;
        }
        let Some(pending) =
            self.take_exact_pending_oauth(&operation_id, config_revision, Some(&client.server_id))
        else {
            self.publish_stale_oauth(operation_id);
            return;
        };
        self.spawn_oauth_authorize(pending, Some(client), None);
    }

    fn submit_slack_workspace(
        &mut self,
        operation_id: OperationId,
        config_revision: Revision,
        workspace: String,
    ) {
        if workspace.trim().is_empty() || workspace.len() > self.limits.import_report_bytes {
            self.publish_operation_error(
                operation_id,
                OperationKind::OAuth,
                ErrorCode::InvalidInput,
                "OAuth workspace input is invalid",
            );
            return;
        }
        let Some(pending) = self.take_exact_pending_oauth(&operation_id, config_revision, None)
        else {
            self.publish_stale_oauth(operation_id);
            return;
        };
        self.spawn_oauth_authorize(pending, None, Some(workspace));
    }

    fn spawn_oauth_discovery(
        &mut self,
        operation_id: OperationId,
        server_id: ServerId,
        server: ServerDraft,
        choose_workspace: bool,
    ) {
        let generation = self.bump_generation(&server_id);
        let config_revision = self.snapshot.config_revision;
        let dispatch_epoch = self.current_dispatch_epoch;
        let cancellation = CancellationToken::default();
        let oauth = Arc::clone(&self.oauth);
        let sender = self.result_sender.clone();
        let unparker = std::thread::current();
        let operation_for_thread = operation_id.clone();
        let server_for_thread = server_id.clone();
        let cancellation_for_thread = cancellation.clone();
        let thread = std::thread::Builder::new()
            .name("connector-oauth".to_owned())
            .spawn(move || {
                let payload = JobPayload::OAuthDiscover(run_backend_job(|| {
                    oauth.discover(
                        &operation_for_thread,
                        server,
                        choose_workspace,
                        cancellation_for_thread,
                    )
                }));
                let _ = sender.send(JobCompletion {
                    operation_id: operation_for_thread,
                    server_id: server_for_thread,
                    generation,
                    config_revision,
                    dispatch_epoch,
                    stage: JobStage::OAuthDiscover,
                    payload,
                });
                unparker.unpark();
            });
        let _ = self.insert_job(
            operation_id,
            server_id,
            generation,
            config_revision,
            JobStage::OAuthDiscover,
            cancellation,
            thread,
        );
    }

    fn spawn_oauth_authorize(
        &mut self,
        pending: PendingOAuth,
        client: Option<StoredOAuthClient>,
        workspace: Option<String>,
    ) {
        let PendingOAuth {
            operation_id,
            server_id,
            server_name,
            continuation,
            generation,
            config_revision,
            dispatch_epoch,
        } = pending;
        if self.snapshot.config_revision != config_revision
            || self.current_dispatch_epoch != dispatch_epoch
        {
            self.publish_stale_oauth(operation_id);
            return;
        }
        self.snapshot.oauth = Some(OAuthUiState {
            operation_id: operation_id.clone(),
            server_id: server_id.clone(),
            server_name,
            config_revision,
            phase: OAuthUiPhase::PreparingCallback,
        });
        self.update_operation_phase(&operation_id, OperationPhase::PreparingCallback, None);
        self.transition(
            OperationKind::OAuth,
            OperationPhase::PreparingCallback,
            None,
        );
        self.publish();

        let cancellation = CancellationToken::default();
        let oauth = Arc::clone(&self.oauth);
        let sender = self.result_sender.clone();
        let event_sender = self.oauth_event_sender.clone();
        let unparker = std::thread::current();
        let operation_for_thread = operation_id.clone();
        let server_for_thread = server_id.clone();
        let cancellation_for_thread = cancellation.clone();
        let event_unparker = unparker.clone();
        let thread = std::thread::Builder::new()
            .name("connector-oauth".to_owned())
            .spawn(move || {
                let events: Arc<dyn OAuthEventSink> = Arc::new(CoordinatorOAuthEventSink {
                    sender: event_sender,
                    unparker: event_unparker,
                    operation_id: operation_for_thread.clone(),
                    server_id: server_for_thread.clone(),
                    generation,
                    config_revision,
                    dispatch_epoch,
                });
                let payload = JobPayload::OAuthAuthorize(run_backend_job(|| {
                    oauth.authorize(
                        &operation_for_thread,
                        continuation,
                        client,
                        workspace,
                        events,
                        cancellation_for_thread,
                    )
                }));
                let _ = sender.send(JobCompletion {
                    operation_id: operation_for_thread,
                    server_id: server_for_thread,
                    generation,
                    config_revision,
                    dispatch_epoch,
                    stage: JobStage::OAuthAuthorize,
                    payload,
                });
                unparker.unpark();
            });
        let _ = self.insert_job(
            operation_id,
            server_id,
            generation,
            config_revision,
            JobStage::OAuthAuthorize,
            cancellation,
            thread,
        );
    }

    fn resolve_oauth_consent(
        &mut self,
        operation_id: OperationId,
        config_revision: Revision,
        accepted: bool,
    ) {
        let exact_phase = self.snapshot.oauth.as_ref().is_some_and(|state| {
            state.operation_id == operation_id
                && state.config_revision == config_revision
                && matches!(state.phase, OAuthUiPhase::AwaitingConsent { .. })
        });
        if !exact_phase {
            self.publish_stale_oauth(operation_id);
            return;
        }
        let Some(pending) = self.take_exact_pending_oauth(&operation_id, config_revision, None)
        else {
            self.publish_stale_oauth(operation_id);
            return;
        };
        if !accepted {
            self.snapshot.oauth = None;
            self.finish_oauth_operation(
                &operation_id,
                OperationPhase::Denied,
                Some(ErrorCode::TrustDenied),
            );
            return;
        }
        self.spawn_oauth_authorize(pending, None, None);
    }

    fn take_exact_pending_oauth(
        &mut self,
        operation_id: &OperationId,
        config_revision: Revision,
        server_id: Option<&ServerId>,
    ) -> Option<PendingOAuth> {
        let pending = self.pending_oauth.take()?;
        let exact = &pending.operation_id == operation_id
            && pending.config_revision == config_revision
            && self.snapshot.config_revision == config_revision
            && pending.dispatch_epoch == self.current_dispatch_epoch
            && self.dispatch_epoch.load(Ordering::Acquire) == pending.dispatch_epoch
            && server_id.is_none_or(|server_id| server_id == &pending.server_id)
            && self
                .generations
                .get(&pending.server_id)
                .copied()
                .unwrap_or_default()
                == pending.generation;
        exact.then_some(pending)
    }

    fn accept_oauth_event(&mut self, event: OAuthWorkerEvent) {
        let exact = self.jobs.get(&event.operation_id).is_some_and(|job| {
            job.stage == JobStage::OAuthAuthorize
                && !job.cancellation.is_cancelled()
                && job.server_id == event.server_id
                && job.generation == event.generation
                && job.config_revision == event.config_revision
                && job.dispatch_epoch == event.dispatch_epoch
        }) && self.snapshot.config_revision == event.config_revision
            && self.dispatch_epoch.load(Ordering::Acquire) == event.dispatch_epoch
            && self
                .generations
                .get(&event.server_id)
                .copied()
                .unwrap_or_default()
                == event.generation;
        if !exact {
            let _ = event.acknowledgement.send(Err(ServiceError::new(
                ErrorCode::StaleResult,
                "stale OAuth browser action rejected",
            )));
            self.metrics.stale_results.fetch_add(1, Ordering::AcqRel);
            return;
        }
        if event.authorization_url.is_empty()
            || event.authorization_url.len() > self.limits.tool_input_bytes
        {
            if let Some(job) = self.jobs.get(&event.operation_id) {
                job.cancellation.cancel();
            }
            self.oauth.cancel(&event.operation_id);
            let _ = event.acknowledgement.send(Err(ServiceError::new(
                ErrorCode::LimitExceeded,
                "OAuth authorization URL limit exceeded",
            )));
            return;
        }
        self.update_oauth_phase(&event.operation_id, OAuthUiPhase::BrowserReady);
        self.update_operation_phase(&event.operation_id, OperationPhase::BrowserReady, None);
        self.transition(OperationKind::OAuth, OperationPhase::BrowserReady, None);
        self.publish();
        let queued = self.host_actions.push(HostAction::OpenOAuthBrowser {
            operation_id: event.operation_id.clone(),
            config_revision: event.config_revision,
            url: event.authorization_url,
        });
        if let Err(error) = queued {
            if let Some(job) = self.jobs.get(&event.operation_id) {
                job.cancellation.cancel();
            }
            self.oauth.cancel(&event.operation_id);
            self.metrics.backpressure.fetch_add(1, Ordering::AcqRel);
            self.update_oauth_phase(
                &event.operation_id,
                OAuthUiPhase::Failed {
                    error_code: ErrorCode::HostUnavailable,
                    recovery: Arc::from([OAuthRecoveryAction::Retry]),
                },
            );
            self.update_operation_phase(
                &event.operation_id,
                OperationPhase::Failed,
                Some(ErrorCode::HostUnavailable),
            );
            self.publish();
            let _ = event.acknowledgement.send(Err(error));
            return;
        }
        self.update_oauth_phase(&event.operation_id, OAuthUiPhase::AwaitingCallback);
        self.update_operation_phase(&event.operation_id, OperationPhase::AwaitingCallback, None);
        self.transition(OperationKind::OAuth, OperationPhase::AwaitingCallback, None);
        self.publish();
        let _ = event.acknowledgement.send(Ok(()));
    }

    fn update_oauth_phase(&mut self, operation_id: &OperationId, phase: OAuthUiPhase) {
        if let Some(state) = self
            .snapshot
            .oauth
            .as_mut()
            .filter(|state| &state.operation_id == operation_id)
        {
            state.phase = phase;
        }
    }

    fn publish_stale_oauth(&mut self, operation_id: OperationId) {
        self.pending_oauth = None;
        if self
            .snapshot
            .oauth
            .as_ref()
            .is_some_and(|state| state.operation_id == operation_id)
        {
            self.snapshot.oauth = None;
        }
        self.metrics.stale_results.fetch_add(1, Ordering::AcqRel);
        self.finish_oauth_operation(
            &operation_id,
            OperationPhase::Cancelled,
            Some(ErrorCode::StaleResult),
        );
    }

    fn finish_oauth_operation(
        &mut self,
        operation_id: &OperationId,
        phase: OperationPhase,
        error_code: Option<ErrorCode>,
    ) {
        let before = self.operations.len();
        self.operations
            .retain(|operation| &operation.id != operation_id);
        self.cancellations
            .lock()
            .expect("connector cancellation registry")
            .remove(operation_id);
        if self.operations.len() != before {
            self.metrics.active_oauth.fetch_sub(1, Ordering::AcqRel);
        }
        self.transition(OperationKind::OAuth, phase, error_code);
        self.prune_generations();
        self.publish();
    }

    fn resolve_oauth_recovery(&mut self, operation_id: OperationId, action: OAuthRecoveryAction) {
        let Some(state) = self.snapshot.oauth.as_ref() else {
            self.publish_stale_oauth(operation_id);
            return;
        };
        if state.operation_id != operation_id {
            self.publish_stale_oauth(operation_id);
            return;
        }
        let server_id = state.server_id.clone();
        match action {
            OAuthRecoveryAction::Retry => self.start_oauth(server_id, false),
            OAuthRecoveryAction::ChooseWorkspace => self.start_oauth(server_id, true),
            OAuthRecoveryAction::OpenSlackMcpSettings => {
                self.open_slack_recovery(server_id, SlackRecoveryKind::EnableMcpAccess)
            }
        }
    }

    fn open_slack_recovery(&mut self, server_id: ServerId, kind: SlackRecoveryKind) {
        let Some(target) = self.slack_recovery.take() else {
            self.publish_error(
                OperationKind::OAuth,
                ErrorCode::HostUnavailable,
                "Slack recovery action is unavailable",
            );
            return;
        };
        if target.server_id != server_id || target.kind != kind {
            self.metrics.stale_results.fetch_add(1, Ordering::AcqRel);
            return;
        }
        if let Err(error) = self.host_actions.push(HostAction::OpenSlackRecovery {
            operation_id: target.operation_id,
            server_id,
            kind,
            url: target.url,
        }) {
            self.publish_service_error(OperationKind::OAuth, error);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_job(
        &mut self,
        operation_id: OperationId,
        server_id: ServerId,
        generation: u64,
        config_revision: Revision,
        stage: JobStage,
        cancellation: CancellationToken,
        thread: Result<JoinHandle<()>, std::io::Error>,
    ) -> bool {
        let kind = stage.kind();
        let thread = match thread {
            Ok(thread) => thread,
            Err(_) => {
                self.publish_operation_error(
                    operation_id,
                    kind,
                    ErrorCode::Internal,
                    "connector job thread unavailable",
                );
                return false;
            }
        };
        let existing_operation = self
            .operations
            .iter_mut()
            .find(|operation| operation.id == operation_id);
        let reserve_capacity = existing_operation.is_none();
        match stage {
            JobStage::Discover | JobStage::InvokeSchema => {
                if reserve_capacity {
                    self.metrics.active_mcp.fetch_add(1, Ordering::AcqRel);
                }
            }
            JobStage::OAuthDiscover | JobStage::OAuthAuthorize => {
                if reserve_capacity {
                    self.metrics.active_oauth.fetch_add(1, Ordering::AcqRel);
                }
            }
            JobStage::InvokeCall => {}
        }
        if let Some(operation) = existing_operation {
            operation.phase = OperationPhase::Queued;
            operation.error_code = None;
        } else {
            self.operations.push(OperationSummary {
                id: operation_id.clone(),
                server_id: server_id.clone(),
                kind,
                phase: OperationPhase::Queued,
                error_code: None,
            });
        }
        self.cancellations
            .lock()
            .expect("connector cancellation registry")
            .insert(operation_id.clone(), cancellation.clone());
        self.jobs.insert(
            operation_id,
            ActiveJob {
                server_id,
                generation,
                config_revision,
                dispatch_epoch: self.current_dispatch_epoch,
                stage,
                cancellation,
                thread: Some(thread),
            },
        );
        self.transition(kind, OperationPhase::Queued, None);
        self.publish();
        true
    }

    fn finish_job(&mut self, completion: JobCompletion) {
        let Some(mut active) = self.jobs.remove(&completion.operation_id) else {
            self.metrics.stale_results.fetch_add(1, Ordering::AcqRel);
            return;
        };
        if let Some(thread) = active.thread.take() {
            let _ = thread.join();
        }
        let current_generation = self
            .generations
            .get(&completion.server_id)
            .copied()
            .unwrap_or_default();
        let stale = active.cancellation.is_cancelled()
            || completion.dispatch_epoch != self.dispatch_epoch.load(Ordering::Acquire)
            || current_generation != completion.generation
            || self.snapshot.config_revision != completion.config_revision
            || active.server_id != completion.server_id
            || active.generation != completion.generation
            || active.config_revision != completion.config_revision
            || active.dispatch_epoch != completion.dispatch_epoch
            || active.stage != completion.stage;
        match (completion.stage, completion.payload) {
            (JobStage::Discover, JobPayload::Discover(result)) => {
                self.finish_operation_slot(&completion.operation_id, JobStage::Discover);
                if stale {
                    self.publish_stale(JobStage::Discover);
                } else {
                    self.finish_discover(completion.server_id, result);
                }
            }
            (JobStage::InvokeSchema, JobPayload::InvokeSchema(result)) => {
                if stale {
                    self.remove_pending_invocation(&completion.operation_id);
                    self.finish_operation_slot(&completion.operation_id, JobStage::InvokeSchema);
                    self.publish_stale(JobStage::InvokeSchema);
                } else {
                    self.finish_invoke_schema(completion.operation_id, result);
                }
            }
            (JobStage::InvokeCall, JobPayload::InvokeCall(result)) => {
                self.finish_invoke_call(completion.operation_id, result, stale);
            }
            (JobStage::OAuthDiscover, JobPayload::OAuthDiscover(result)) => {
                if stale {
                    self.snapshot.oauth = None;
                    self.finish_oauth_operation(
                        &completion.operation_id,
                        OperationPhase::Cancelled,
                        Some(ErrorCode::StaleResult),
                    );
                    self.metrics.stale_results.fetch_add(1, Ordering::AcqRel);
                } else {
                    self.finish_oauth_discovery(
                        completion.operation_id,
                        completion.server_id,
                        completion.generation,
                        completion.config_revision,
                        completion.dispatch_epoch,
                        result,
                    );
                }
            }
            (JobStage::OAuthAuthorize, JobPayload::OAuthAuthorize(result)) => {
                if stale {
                    self.snapshot.oauth = None;
                    self.finish_oauth_operation(
                        &completion.operation_id,
                        OperationPhase::Cancelled,
                        Some(ErrorCode::StaleResult),
                    );
                    self.metrics.stale_results.fetch_add(1, Ordering::AcqRel);
                } else {
                    self.finish_oauth_authorize(
                        completion.operation_id,
                        completion.server_id,
                        completion.generation,
                        completion.config_revision,
                        completion.dispatch_epoch,
                        result,
                    );
                }
            }
            (stage, _) => {
                self.finish_operation_slot(&completion.operation_id, stage);
                self.publish_operation_error(
                    completion.operation_id,
                    stage.kind(),
                    ErrorCode::Internal,
                    "connector job completion type mismatch",
                );
            }
        }
    }

    fn publish_stale(&mut self, stage: JobStage) {
        self.metrics.stale_results.fetch_add(1, Ordering::AcqRel);
        self.transition(
            stage.kind(),
            OperationPhase::Cancelled,
            Some(ErrorCode::StaleResult),
        );
        self.publish();
    }

    fn finish_operation_slot(&mut self, operation_id: &OperationId, stage: JobStage) {
        if stage == JobStage::InvokeSchema {
            // A successful schema read pins the exact connection until authorization resolves.
            // Every terminal pre-call path funnels through this function and releases that pin.
            self.mcp.cancel(operation_id);
        }
        let operation_count = self.operations.len();
        self.operations
            .retain(|operation| &operation.id != operation_id);
        if self.operations.len() == operation_count {
            return;
        }
        self.cancellations
            .lock()
            .expect("connector cancellation registry")
            .remove(operation_id);
        match stage {
            JobStage::Discover | JobStage::InvokeSchema | JobStage::InvokeCall => {
                self.metrics.active_mcp.fetch_sub(1, Ordering::AcqRel);
            }
            JobStage::OAuthDiscover | JobStage::OAuthAuthorize => {
                self.metrics.active_oauth.fetch_sub(1, Ordering::AcqRel);
            }
        }
        self.remove_pending_invocation(operation_id);
        self.prune_generations();
    }

    fn remove_pending_invocation(
        &mut self,
        operation_id: &OperationId,
    ) -> Option<PendingInvocation> {
        self.approval_order.retain(|queued| queued != operation_id);
        let pending = self.pending_invocations.remove(operation_id);
        self.refresh_approval_snapshot();
        pending
    }

    fn refresh_approval_snapshot(&mut self) {
        while self
            .approval_order
            .front()
            .is_some_and(|operation_id| !self.pending_invocations.contains_key(operation_id))
        {
            self.approval_order.pop_front();
        }
        self.snapshot.approval = self.approval_order.front().and_then(|operation_id| {
            let pending = self.pending_invocations.get(operation_id)?;
            Some(ApprovalPrompt {
                operation_id: operation_id.clone(),
                server_id: pending.server_id.clone(),
                server_name: pending.server.name.clone(),
                tool_name: pending.tool_name.clone(),
                arguments_preview: pending.arguments_preview.clone()?,
                reason: map_approval_reason(pending.authorization.as_ref()?.reason()),
            })
        });
    }

    fn update_operation_phase(
        &mut self,
        operation_id: &OperationId,
        phase: OperationPhase,
        error_code: Option<ErrorCode>,
    ) {
        if let Some(operation) = self
            .operations
            .iter_mut()
            .find(|operation| &operation.id == operation_id)
        {
            operation.phase = phase;
            operation.error_code = error_code;
        }
    }

    fn invocation_is_stale(&self, operation_id: &OperationId, pending: &PendingInvocation) -> bool {
        self.cancellations
            .lock()
            .expect("connector cancellation registry")
            .get(operation_id)
            .is_some_and(CancellationToken::is_cancelled)
            || self.dispatch_epoch.load(Ordering::Acquire) != pending.dispatch_epoch
            || self.snapshot.config_revision != pending.config_revision
            || self
                .generations
                .get(&pending.server_id)
                .copied()
                .unwrap_or_default()
                != pending.generation
    }

    fn expire_pending_invocations(&mut self) {
        let expired: Vec<OperationId> = self
            .approval_order
            .iter()
            .filter(|operation_id| !self.jobs.contains_key(*operation_id))
            .cloned()
            .collect();
        if expired.is_empty() {
            return;
        }
        for operation_id in expired {
            let Some(mut pending) = self.remove_pending_invocation(&operation_id) else {
                continue;
            };
            let Some(authorization) = pending.authorization.take() else {
                self.finish_operation_slot(&operation_id, JobStage::InvokeSchema);
                continue;
            };
            let plan = authorization.resolve(audit::ApprovalDecision::DenyOnce);
            self.commit_preflight(operation_id, pending, plan);
        }
    }

    fn expire_pending_waits(&mut self) {
        self.expire_pending_invocations();
        if let Some(pending) = self.pending_remote.take() {
            self.snapshot.remote_trust = None;
            self.finish_waiting_operation(
                &pending.prompt.operation_id,
                OperationKind::Trust,
                OperationPhase::Cancelled,
                Some(ErrorCode::Cancelled),
            );
        }
        if let Some(pending) = self.pending_oauth.take() {
            self.oauth.cancel(&pending.operation_id);
            self.snapshot.oauth = None;
            self.finish_oauth_operation(
                &pending.operation_id,
                OperationPhase::Cancelled,
                Some(ErrorCode::Cancelled),
            );
        }
    }

    fn invalidate_stale_pending_invocations(&mut self) {
        let stale: Vec<OperationId> = self
            .pending_invocations
            .iter()
            .filter(|(operation_id, pending)| self.invocation_is_stale(operation_id, pending))
            .map(|(operation_id, _)| operation_id.clone())
            .collect();
        for operation_id in stale {
            if let Some(job) = self.jobs.get(&operation_id) {
                job.cancellation.cancel();
                self.mcp.cancel(&operation_id);
                self.remove_pending_invocation(&operation_id);
                self.update_operation_phase(
                    &operation_id,
                    OperationPhase::Cancelled,
                    Some(ErrorCode::StaleResult),
                );
            } else {
                self.finish_operation_slot(&operation_id, JobStage::InvokeSchema);
                self.metrics.stale_results.fetch_add(1, Ordering::AcqRel);
                self.transition(
                    OperationKind::Invoke,
                    OperationPhase::Cancelled,
                    Some(ErrorCode::StaleResult),
                );
            }
        }
    }

    fn finish_discover(
        &mut self,
        server_id: ServerId,
        result: Result<DiscoverOutput, ServiceError>,
    ) {
        let output = match result {
            Ok(output) => output,
            Err(error) => {
                self.publish_service_error(OperationKind::Discover, error);
                return;
            }
        };
        if let Err(error) = validate_discovered_tools(&output, self.limits) {
            self.publish_service_error(OperationKind::Discover, error);
            return;
        }
        if let Err(error) = self.repository.replace_tools(&server_id, &output.tools) {
            self.publish_service_error(OperationKind::Discover, error);
            return;
        }
        self.transition(OperationKind::Discover, OperationPhase::Succeeded, None);
        self.snapshot.tool_page = None;
        self.reload_overview();
    }

    fn finish_invoke_schema(
        &mut self,
        operation_id: OperationId,
        result: Result<LiveToolSchema, ServiceError>,
    ) {
        let Some(mut pending) = self.pending_invocations.remove(&operation_id) else {
            self.finish_operation_slot(&operation_id, JobStage::InvokeSchema);
            self.publish_stale(JobStage::InvokeSchema);
            return;
        };
        let live = match result {
            Ok(live) => live,
            Err(error) => {
                self.finish_operation_slot(&operation_id, JobStage::InvokeSchema);
                self.publish_operation_error(
                    operation_id,
                    OperationKind::Invoke,
                    error.code,
                    error.message,
                );
                return;
            }
        };
        if live.tool_id != pending.tool_id
            || live.tool_name != pending.tool_name
            || live.tool_name.trim().is_empty()
            || live
                .tool_name
                .len()
                .checked_add(live.input_schema_json.len())
                .is_none_or(|bytes| bytes > self.limits.tool_descriptor_bytes)
        {
            self.finish_operation_slot(&operation_id, JobStage::InvokeSchema);
            self.publish_operation_error(
                operation_id,
                OperationKind::Invoke,
                ErrorCode::ProtocolViolation,
                "live tool schema is invalid or exceeds limits",
            );
            return;
        }
        pending.live_schema_hash = audit::schema_hash(&live.input_schema_json);
        self.update_operation_phase(&operation_id, OperationPhase::Authorizing, None);
        self.transition(OperationKind::Invoke, OperationPhase::Authorizing, None);

        let state = match self
            .repository
            .load_authorization_state(&pending.server_id, &pending.tool_name)
        {
            Ok(state) => state,
            Err(error) => {
                self.finish_operation_slot(&operation_id, JobStage::InvokeSchema);
                self.publish_operation_error(
                    operation_id,
                    OperationKind::Invoke,
                    error.code,
                    error.message,
                );
                return;
            }
        };
        let authorization = match audit::evaluate_authorization_with_fingerprint(
            operation_id.as_str().to_owned(),
            pending.server_id.as_str().to_owned(),
            pending.tool_name.clone(),
            state.permission,
            pending.live_schema_hash.clone(),
        ) {
            Ok(authorization) => authorization,
            Err(_) => {
                self.finish_operation_slot(&operation_id, JobStage::InvokeSchema);
                self.publish_operation_error(
                    operation_id,
                    OperationKind::Invoke,
                    ErrorCode::PermissionDenied,
                    "live schema permission evaluation failed",
                );
                return;
            }
        };
        match authorization {
            audit::AuthorizationEvaluation::Plan(plan) => {
                self.commit_preflight(operation_id, pending, plan)
            }
            audit::AuthorizationEvaluation::NeedsApproval(authorization) => {
                let preview = match self
                    .secrets
                    .sanitized_input_preview(&pending.arguments_json, APPROVAL_PREVIEW_CHARS)
                {
                    Ok(preview) if preview.chars().count() <= APPROVAL_PREVIEW_CHARS => preview,
                    Ok(_) | Err(_) => {
                        self.finish_operation_slot(&operation_id, JobStage::InvokeSchema);
                        self.publish_operation_error(
                            operation_id,
                            OperationKind::Invoke,
                            ErrorCode::SecretUnavailable,
                            "approval preview redaction failed",
                        );
                        return;
                    }
                };
                pending.authorization = Some(authorization);
                pending.arguments_preview = Some(preview);
                self.last_activity = self.clock.now();
                self.approval_order.push_back(operation_id.clone());
                self.pending_invocations.insert(operation_id, pending);
                self.refresh_approval_snapshot();
                self.publish();
            }
        }
    }

    fn resolve_approval(&mut self, operation_id: OperationId, decision: ApprovalDecision) {
        let Some(mut pending) = self.pending_invocations.remove(&operation_id) else {
            return;
        };
        self.approval_order.retain(|queued| queued != &operation_id);
        self.refresh_approval_snapshot();
        if self.invocation_is_stale(&operation_id, &pending) {
            self.finish_operation_slot(&operation_id, JobStage::InvokeSchema);
            self.publish_stale(JobStage::InvokeSchema);
            return;
        }
        let Some(authorization) = pending.authorization.take() else {
            self.finish_operation_slot(&operation_id, JobStage::InvokeSchema);
            self.publish_operation_error(
                operation_id,
                OperationKind::Invoke,
                ErrorCode::PermissionDenied,
                "approval authorization is unavailable",
            );
            return;
        };
        let plan = authorization.resolve(map_approval_decision(decision));
        self.commit_preflight(operation_id, pending, plan);
    }

    fn commit_preflight(
        &mut self,
        operation_id: OperationId,
        pending: PendingInvocation,
        plan: audit::AuthorizationPlan,
    ) {
        if self.invocation_is_stale(&operation_id, &pending) {
            self.finish_operation_slot(&operation_id, JobStage::InvokeSchema);
            self.publish_stale(JobStage::InvokeSchema);
            return;
        }
        self.update_operation_phase(&operation_id, OperationPhase::AuditPreflight, None);
        self.transition(OperationKind::Invoke, OperationPhase::AuditPreflight, None);
        let preflight = match self
            .repository
            .commit_authorization_preflight(plan, &pending.arguments_json)
        {
            Ok(preflight) => preflight,
            Err(error) => {
                self.finish_operation_slot(&operation_id, JobStage::InvokeSchema);
                self.publish_operation_error(
                    operation_id,
                    OperationKind::Invoke,
                    ErrorCode::AuditUnavailable,
                    error.message,
                );
                return;
            }
        };
        match preflight {
            audit::AuthorizationPreflight::Denied(receipt) => {
                debug_assert_eq!(receipt.operation_id(), operation_id.as_str());
                self.finish_operation_slot(&operation_id, JobStage::InvokeSchema);
                self.snapshot.result = Some(OperationResult {
                    operation_id,
                    text: "tool call denied".to_owned(),
                    truncated: false,
                });
                self.transition(
                    OperationKind::Invoke,
                    OperationPhase::Denied,
                    Some(ErrorCode::PermissionDenied),
                );
                self.publish();
            }
            audit::AuthorizationPreflight::Prepared(grant) => {
                self.start_authorized_call(operation_id, pending, grant);
            }
        }
    }

    fn start_authorized_call(
        &mut self,
        operation_id: OperationId,
        pending: PendingInvocation,
        grant: audit::AuthorizationGrant,
    ) {
        if self.invocation_is_stale(&operation_id, &pending) {
            let outcome = audit::AuthorizationOutcome::Failed {
                error_code: "cancelled_before_call",
            };
            let completion = self.persist_authorization_outcome(&operation_id, outcome);
            if completion.is_err() {
                self.retain_pending_outcome(operation_id.clone(), outcome, JobStage::InvokeSchema);
            } else {
                self.finish_operation_slot(&operation_id, JobStage::InvokeSchema);
            }
            self.publish_stale(JobStage::InvokeSchema);
            return;
        }
        if grant.operation_id() != operation_id.as_str()
            || grant.server_id() != pending.server_id.as_str()
            || grant.tool_name() != pending.tool_name.as_str()
            || grant.live_schema_hash() != pending.live_schema_hash.as_str()
        {
            let completion = self.persist_authorization_outcome(
                &operation_id,
                audit::AuthorizationOutcome::Failed {
                    error_code: "authorization_binding_mismatch",
                },
            );
            if completion.is_err() {
                self.retain_pending_outcome(
                    operation_id.clone(),
                    audit::AuthorizationOutcome::Failed {
                        error_code: "authorization_binding_mismatch",
                    },
                    JobStage::InvokeSchema,
                );
                self.publish_unpersisted_outcome(operation_id);
                return;
            }
            self.finish_operation_slot(&operation_id, JobStage::InvokeSchema);
            self.publish_operation_error(
                operation_id,
                OperationKind::Invoke,
                ErrorCode::PermissionDenied,
                "authorization binding mismatch; call was not attempted",
            );
            return;
        }
        let cancellation = self
            .cancellations
            .lock()
            .expect("connector cancellation registry")
            .get(&operation_id)
            .cloned()
            .unwrap_or_default();
        let mcp = Arc::clone(&self.mcp);
        let sender = self.result_sender.clone();
        let unparker = std::thread::current();
        let operation_for_thread = operation_id.clone();
        let server_id = pending.server_id.clone();
        let active_server_id = server_id.clone();
        let generation = pending.generation;
        let config_revision = pending.config_revision;
        let dispatch_epoch = pending.dispatch_epoch;
        let cancellation_for_thread = cancellation.clone();
        let request = match AuthorizedInvokeRequest::new(
            grant,
            &pending.server_id,
            pending.server,
            pending.tool_name,
            pending.arguments_json,
        ) {
            Ok(request) => request,
            Err(error) => {
                let completion = self.persist_authorization_outcome(
                    &operation_id,
                    audit::AuthorizationOutcome::Failed {
                        error_code: "authorization_input_mismatch",
                    },
                );
                if completion.is_err() {
                    self.retain_pending_outcome(
                        operation_id.clone(),
                        audit::AuthorizationOutcome::Failed {
                            error_code: "authorization_input_mismatch",
                        },
                        JobStage::InvokeSchema,
                    );
                    self.publish_unpersisted_outcome(operation_id);
                    return;
                }
                self.finish_operation_slot(&operation_id, JobStage::InvokeSchema);
                self.publish_operation_error(
                    operation_id,
                    OperationKind::Invoke,
                    error.code,
                    "authorization input binding mismatch; call was not attempted",
                );
                return;
            }
        };
        let thread = std::thread::Builder::new()
            .name("connector-mcp-call".to_owned())
            .spawn(move || {
                let payload = JobPayload::InvokeCall(run_backend_job(|| {
                    mcp.invoke_authorized(request, cancellation_for_thread)
                }));
                let _ = sender.send(JobCompletion {
                    operation_id: operation_for_thread,
                    server_id,
                    generation,
                    config_revision,
                    dispatch_epoch,
                    stage: JobStage::InvokeCall,
                    payload,
                });
                unparker.unpark();
            });
        let thread = match thread {
            Ok(thread) => thread,
            Err(_) => {
                let completion = self.persist_authorization_outcome(
                    &operation_id,
                    audit::AuthorizationOutcome::Failed {
                        error_code: "call_thread_unavailable",
                    },
                );
                if completion.is_err() {
                    self.retain_pending_outcome(
                        operation_id.clone(),
                        audit::AuthorizationOutcome::Failed {
                            error_code: "call_thread_unavailable",
                        },
                        JobStage::InvokeSchema,
                    );
                    self.publish_unpersisted_outcome(operation_id);
                    return;
                }
                self.finish_operation_slot(&operation_id, JobStage::InvokeSchema);
                self.publish_operation_error(
                    operation_id,
                    OperationKind::Invoke,
                    ErrorCode::Internal,
                    "authorized call thread unavailable; call was not retried",
                );
                return;
            }
        };
        self.jobs.insert(
            operation_id.clone(),
            ActiveJob {
                server_id: active_server_id,
                generation,
                config_revision,
                dispatch_epoch,
                stage: JobStage::InvokeCall,
                cancellation,
                thread: Some(thread),
            },
        );
        self.update_operation_phase(&operation_id, OperationPhase::Calling, None);
        self.transition(OperationKind::Invoke, OperationPhase::Calling, None);
        self.publish();
    }

    fn finish_invoke_call(
        &mut self,
        operation_id: OperationId,
        result: Result<String, ServiceError>,
        stale: bool,
    ) {
        let (outcome, result_code) = match &result {
            Ok(_) => (audit::AuthorizationOutcome::Succeeded, None),
            Err(error) if error.code == ErrorCode::UnknownDelivery => (
                audit::AuthorizationOutcome::Unknown {
                    error_code: "unknown_delivery",
                },
                Some(ErrorCode::UnknownDelivery),
            ),
            Err(error) => (
                audit::AuthorizationOutcome::Failed {
                    error_code: error_code_label(error.code),
                },
                Some(error.code),
            ),
        };
        // The backend call is never retried. Only the idempotent durable outcome write gets one
        // immediate retry before the UI receives a non-retryable unknown audit state.
        let completion = self.persist_authorization_outcome(&operation_id, outcome);
        if completion.is_err() {
            self.retain_pending_outcome(operation_id.clone(), outcome, JobStage::InvokeCall);
            self.publish_unpersisted_outcome(operation_id);
            return;
        }
        self.finish_operation_slot(&operation_id, JobStage::InvokeCall);
        if stale {
            self.publish_stale(JobStage::InvokeCall);
            return;
        }
        match result {
            Ok(text) => {
                let (text, truncated) = truncate_utf8(text, self.limits.ui_result_bytes);
                self.snapshot.result = Some(OperationResult {
                    operation_id,
                    text,
                    truncated,
                });
                self.transition(OperationKind::Invoke, OperationPhase::Succeeded, None);
                self.publish();
            }
            Err(_error) if result_code == Some(ErrorCode::UnknownDelivery) => {
                self.snapshot.result = Some(OperationResult {
                    operation_id,
                    text: "tool call delivery is unknown; it was not retried".to_owned(),
                    truncated: false,
                });
                self.transition(
                    OperationKind::Invoke,
                    OperationPhase::Unknown,
                    Some(ErrorCode::UnknownDelivery),
                );
                self.publish();
            }
            Err(error) => self.publish_operation_error(
                operation_id,
                OperationKind::Invoke,
                error.code,
                error.message,
            ),
        }
    }

    fn persist_authorization_outcome(
        &mut self,
        operation_id: &OperationId,
        outcome: audit::AuthorizationOutcome,
    ) -> Result<(), ServiceError> {
        match self
            .repository
            .complete_authorization(operation_id, outcome)
        {
            Ok(()) => Ok(()),
            Err(_) => self
                .repository
                .complete_authorization(operation_id, outcome),
        }
    }

    fn publish_unpersisted_outcome(&mut self, operation_id: OperationId) {
        self.update_operation_phase(
            &operation_id,
            OperationPhase::Unknown,
            Some(ErrorCode::AuditUnavailable),
        );
        self.snapshot.result = Some(OperationResult {
            operation_id,
            text: "tool call outcome could not be persisted; it will not be retried".to_owned(),
            truncated: false,
        });
        self.transition(
            OperationKind::Invoke,
            OperationPhase::Unknown,
            Some(ErrorCode::AuditUnavailable),
        );
        self.publish();
    }

    fn retain_pending_outcome(
        &mut self,
        operation_id: OperationId,
        outcome: audit::AuthorizationOutcome,
        stage: JobStage,
    ) {
        debug_assert!(self.pending_outcomes.len() < self.limits.mcp_operations);
        self.pending_outcomes
            .insert(operation_id, PendingOutcome { outcome, stage });
    }

    fn retry_pending_outcomes(&mut self) {
        let pending: Vec<(OperationId, PendingOutcome)> = self
            .pending_outcomes
            .iter()
            .map(|(operation_id, pending)| (operation_id.clone(), *pending))
            .collect();
        let mut changed = false;
        for (operation_id, pending) in pending {
            if self
                .repository
                .complete_authorization(&operation_id, pending.outcome)
                .is_ok()
            {
                self.pending_outcomes.remove(&operation_id);
                self.finish_operation_slot(&operation_id, pending.stage);
                changed = true;
            }
        }
        if changed {
            self.publish();
        }
    }

    fn release_pending_outcome_slots(&mut self) {
        let pending: Vec<(OperationId, JobStage)> = self
            .pending_outcomes
            .drain()
            .map(|(operation_id, pending)| (operation_id, pending.stage))
            .collect();
        for (operation_id, stage) in pending {
            self.finish_operation_slot(&operation_id, stage);
        }
    }

    fn discard_cancelled_pending(&mut self) {
        let cancelled: Vec<OperationId> = self
            .pending_invocations
            .keys()
            .filter(|operation_id| !self.jobs.contains_key(*operation_id))
            .filter(|operation_id| {
                self.cancellations
                    .lock()
                    .expect("connector cancellation registry")
                    .get(*operation_id)
                    .is_some_and(CancellationToken::is_cancelled)
            })
            .cloned()
            .collect();
        let mut changed = false;
        for operation_id in cancelled {
            if let Some(pending) = self.remove_pending_invocation(&operation_id) {
                self.bump_generation(&pending.server_id);
                self.finish_operation_slot(&operation_id, JobStage::InvokeSchema);
                self.metrics.cancellations.fetch_add(1, Ordering::AcqRel);
                self.transition(
                    OperationKind::Invoke,
                    OperationPhase::Cancelled,
                    Some(ErrorCode::Cancelled),
                );
                changed = true;
            }
        }
        if changed {
            self.publish();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_oauth_discovery(
        &mut self,
        operation_id: OperationId,
        server_id: ServerId,
        generation: u64,
        config_revision: Revision,
        dispatch_epoch: u64,
        result: Result<OAuthDiscovery, ServiceError>,
    ) {
        let discovery = match result {
            Ok(discovery) => discovery,
            Err(error) => {
                self.snapshot.oauth = Some(OAuthUiState {
                    operation_id: operation_id.clone(),
                    server_id,
                    server_name: "OAuth".to_owned(),
                    config_revision,
                    phase: OAuthUiPhase::Failed {
                        error_code: error.code,
                        recovery: Arc::from([OAuthRecoveryAction::Retry]),
                    },
                });
                self.finish_oauth_operation(
                    &operation_id,
                    OperationPhase::Failed,
                    Some(error.code),
                );
                return;
            }
        };
        if let Err(error) = validate_oauth_discovery(&discovery, self.limits) {
            self.snapshot.oauth = None;
            self.finish_oauth_operation(&operation_id, OperationPhase::Failed, Some(error.code));
            return;
        }
        let server_name = self
            .snapshot
            .servers
            .iter()
            .find(|server| server.id == server_id)
            .map_or_else(|| "OAuth".to_owned(), |server| server.name.clone());
        let authority = discovery.authority.clone();
        let resource = discovery.resource.clone();
        let scopes = Arc::from(discovery.scopes);
        self.pending_oauth = Some(PendingOAuth {
            operation_id: operation_id.clone(),
            server_id: server_id.clone(),
            server_name: server_name.clone(),
            continuation: discovery.continuation,
            generation,
            config_revision,
            dispatch_epoch,
        });
        self.snapshot.oauth = Some(OAuthUiState {
            operation_id: operation_id.clone(),
            server_id,
            server_name,
            config_revision,
            phase: OAuthUiPhase::AwaitingConsent {
                authority,
                resource,
                scopes,
            },
        });
        self.update_operation_phase(&operation_id, OperationPhase::AwaitingConsent, None);
        self.transition(OperationKind::OAuth, OperationPhase::AwaitingConsent, None);
        self.publish();
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_oauth_authorize(
        &mut self,
        operation_id: OperationId,
        server_id: ServerId,
        generation: u64,
        config_revision: Revision,
        dispatch_epoch: u64,
        result: Result<OAuthAuthorizeOutput, ServiceError>,
    ) {
        let output = match result {
            Ok(output) => output,
            Err(error) => OAuthAuthorizeOutput::Failed(OAuthFailure {
                error_code: error.code,
                recovery: vec![OAuthRecoveryTarget {
                    kind: SlackRecoveryKind::RetryAuthorization,
                    url: None,
                }],
            }),
        };
        match output {
            OAuthAuthorizeOutput::Completed(completion) => {
                let workspace_label = completion.workspace_label.clone();
                let can_choose_workspace = completion.can_choose_workspace;
                if let Err(error) = self.publish_oauth_credential(completion) {
                    self.snapshot.oauth = Some(OAuthUiState {
                        operation_id: operation_id.clone(),
                        server_id,
                        server_name: "OAuth".to_owned(),
                        config_revision,
                        phase: OAuthUiPhase::Failed {
                            error_code: error.code,
                            recovery: Arc::from([OAuthRecoveryAction::Retry]),
                        },
                    });
                    self.finish_oauth_operation(
                        &operation_id,
                        OperationPhase::Failed,
                        Some(error.code),
                    );
                    return;
                }
                self.snapshot.oauth = None;
                self.slack_recovery = None;
                if self.snapshot.slack.server_id.as_ref() == Some(&server_id) {
                    self.snapshot.slack.workspace_label = workspace_label;
                    self.snapshot.slack.can_choose_workspace = can_choose_workspace;
                    self.snapshot.slack.recovery = None;
                }
                self.finish_oauth_operation(&operation_id, OperationPhase::Succeeded, None);
                self.reload_overview();
            }
            OAuthAuthorizeOutput::ClientInputRequired(request) => {
                if request.workspace_hint.as_ref().is_some_and(|hint| {
                    hint.len() > self.limits.import_report_bytes || hint.contains('\0')
                }) || request.continuation.expose_bytes().len()
                    > self.limits.raw_mcp_response_bytes
                {
                    self.snapshot.oauth = None;
                    self.finish_oauth_operation(
                        &operation_id,
                        OperationPhase::Failed,
                        Some(ErrorCode::LimitExceeded),
                    );
                    return;
                }
                let server_name = self
                    .snapshot
                    .oauth
                    .as_ref()
                    .map_or_else(|| "OAuth".to_owned(), |state| state.server_name.clone());
                self.pending_oauth = Some(PendingOAuth {
                    operation_id: operation_id.clone(),
                    server_id: server_id.clone(),
                    server_name: server_name.clone(),
                    continuation: request.continuation,
                    generation,
                    config_revision,
                    dispatch_epoch,
                });
                self.snapshot.oauth = Some(OAuthUiState {
                    operation_id: operation_id.clone(),
                    server_id,
                    server_name,
                    config_revision,
                    phase: OAuthUiPhase::AwaitingClient {
                        reason: request.reason,
                        workspace_hint: request.workspace_hint,
                    },
                });
                self.update_operation_phase(&operation_id, OperationPhase::AwaitingClient, None);
                self.transition(OperationKind::OAuth, OperationPhase::AwaitingClient, None);
                self.publish();
            }
            OAuthAuthorizeOutput::Failed(failure) => {
                self.apply_oauth_failure(operation_id, server_id, config_revision, failure);
            }
        }
    }

    fn publish_oauth_credential(
        &mut self,
        completion: crate::ports::OAuthCompletion,
    ) -> Result<(), ServiceError> {
        let update = completion.credential;
        if update.oauth_metadata_json.len() > self.limits.raw_mcp_response_bytes
            || update.masked_hint.as_ref().is_some_and(|hint| {
                hint.len() > self.limits.import_report_bytes || hint.contains('\0')
            })
        {
            return Err(ServiceError::new(
                ErrorCode::LimitExceeded,
                "OAuth publish metadata limit exceeded",
            ));
        }
        let previous = self.repository.load_oauth_secret_slot(&update.logical_id)?;
        let staged =
            self.secrets
                .stage_oauth_bundle(update.logical_id, previous.clone(), update.bundle)?;
        let published = self.repository.publish_oauth_secret_slot(
            &staged,
            previous.as_ref(),
            &update.oauth_metadata_json,
            update.masked_hint.as_deref(),
        );
        match published {
            Ok(true) => {
                if let Some(previous) = previous {
                    self.secrets.delete_oauth_bundle(&previous)?;
                }
                Ok(())
            }
            Ok(false) => {
                self.secrets.delete_oauth_bundle(&staged.new_slot)?;
                Err(ServiceError::new(
                    ErrorCode::StaleResult,
                    "OAuth credential pointer changed before publish",
                ))
            }
            Err(error) => {
                let _ = self.secrets.delete_oauth_bundle(&staged.new_slot);
                Err(error)
            }
        }
    }

    fn apply_oauth_failure(
        &mut self,
        operation_id: OperationId,
        server_id: ServerId,
        config_revision: Revision,
        failure: OAuthFailure,
    ) {
        let mut recovery_actions = Vec::new();
        self.slack_recovery = None;
        for target in failure.recovery.into_iter().take(4) {
            if target
                .url
                .as_ref()
                .is_some_and(|url| url.is_empty() || url.len() > self.limits.tool_input_bytes)
            {
                continue;
            }
            let action = match target.kind {
                SlackRecoveryKind::RetryAuthorization => OAuthRecoveryAction::Retry,
                SlackRecoveryKind::ConfigureApp | SlackRecoveryKind::EnableMcpAccess => {
                    OAuthRecoveryAction::OpenSlackMcpSettings
                }
            };
            if self.slack_recovery.is_none() {
                self.slack_recovery = Some(StoredRecoveryTarget {
                    operation_id: operation_id.clone(),
                    server_id: server_id.clone(),
                    kind: target.kind,
                    url: target.url,
                });
                self.snapshot.slack.recovery = Some(target.kind);
            }
            if !recovery_actions.contains(&action) {
                recovery_actions.push(action);
            }
        }
        if recovery_actions.is_empty() {
            recovery_actions.push(OAuthRecoveryAction::Retry);
        }
        let server_name = self
            .snapshot
            .oauth
            .as_ref()
            .map_or_else(|| "OAuth".to_owned(), |state| state.server_name.clone());
        self.snapshot.oauth = Some(OAuthUiState {
            operation_id: operation_id.clone(),
            server_id,
            server_name,
            config_revision,
            phase: OAuthUiPhase::Failed {
                error_code: failure.error_code,
                recovery: Arc::from(recovery_actions),
            },
        });
        self.finish_oauth_operation(
            &operation_id,
            OperationPhase::Failed,
            Some(failure.error_code),
        );
    }

    fn cancel(&mut self, operation_id: OperationId) {
        if let Some((server_id, stage, cancellation)) = self
            .jobs
            .get(&operation_id)
            .map(|job| (job.server_id.clone(), job.stage, job.cancellation.clone()))
        {
            cancellation.cancel();
            if stage == JobStage::InvokeSchema {
                self.remove_pending_invocation(&operation_id);
            }
            self.bump_generation(&server_id);
            match stage {
                JobStage::Discover | JobStage::InvokeSchema | JobStage::InvokeCall => {
                    self.mcp.cancel(&operation_id);
                }
                JobStage::OAuthDiscover | JobStage::OAuthAuthorize => {
                    self.oauth.cancel(&operation_id);
                }
            }
            self.update_operation_phase(
                &operation_id,
                OperationPhase::Cancelled,
                Some(ErrorCode::Cancelled),
            );
            self.metrics.cancellations.fetch_add(1, Ordering::AcqRel);
            self.transition(
                stage.kind(),
                OperationPhase::Cancelled,
                Some(ErrorCode::Cancelled),
            );
            self.publish();
            return;
        }
        if let Some(pending) = self.remove_pending_invocation(&operation_id) {
            self.bump_generation(&pending.server_id);
            self.finish_operation_slot(&operation_id, JobStage::InvokeSchema);
            self.metrics.cancellations.fetch_add(1, Ordering::AcqRel);
            self.transition(
                OperationKind::Invoke,
                OperationPhase::Cancelled,
                Some(ErrorCode::Cancelled),
            );
            self.publish();
            return;
        }
        if self
            .pending_remote
            .as_ref()
            .is_some_and(|pending| pending.prompt.operation_id == operation_id)
        {
            self.pending_remote = None;
            self.snapshot.remote_trust = None;
            self.metrics.cancellations.fetch_add(1, Ordering::AcqRel);
            self.finish_waiting_operation(
                &operation_id,
                OperationKind::Trust,
                OperationPhase::Cancelled,
                Some(ErrorCode::Cancelled),
            );
            return;
        }
        if self
            .pending_oauth
            .as_ref()
            .is_some_and(|pending| pending.operation_id == operation_id)
        {
            let pending = self
                .pending_oauth
                .take()
                .expect("checked OAuth pending state");
            self.bump_generation(&pending.server_id);
            self.oauth.cancel(&operation_id);
            self.snapshot.oauth = None;
            self.metrics.cancellations.fetch_add(1, Ordering::AcqRel);
            self.finish_oauth_operation(
                &operation_id,
                OperationPhase::Cancelled,
                Some(ErrorCode::Cancelled),
            );
        }
    }

    fn begin_shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        let unjoined_pending: Vec<OperationId> = self
            .pending_invocations
            .keys()
            .filter(|operation_id| !self.jobs.contains_key(*operation_id))
            .cloned()
            .collect();
        for operation_id in unjoined_pending {
            self.finish_operation_slot(&operation_id, JobStage::InvokeSchema);
        }
        let jobs: Vec<(OperationId, ServerId, JobStage)> = self
            .jobs
            .iter()
            .map(|(id, job)| (id.clone(), job.server_id.clone(), job.stage))
            .collect();
        for (operation_id, server_id, stage) in jobs {
            if let Some(job) = self.jobs.get(&operation_id) {
                job.cancellation.cancel();
            }
            if stage == JobStage::InvokeSchema {
                self.remove_pending_invocation(&operation_id);
            }
            self.bump_generation(&server_id);
            match stage {
                JobStage::Discover | JobStage::InvokeSchema | JobStage::InvokeCall => {
                    self.mcp.cancel(&operation_id);
                }
                JobStage::OAuthDiscover | JobStage::OAuthAuthorize => {
                    self.oauth.cancel(&operation_id);
                }
            }
        }
        if let Some(pending) = self.pending_remote.take() {
            self.snapshot.remote_trust = None;
            self.finish_waiting_operation(
                &pending.prompt.operation_id,
                OperationKind::Trust,
                OperationPhase::Cancelled,
                Some(ErrorCode::Cancelled),
            );
        }
        if let Some(pending) = self.pending_oauth.take() {
            self.oauth.cancel(&pending.operation_id);
            self.snapshot.oauth = None;
            self.finish_oauth_operation(
                &pending.operation_id,
                OperationPhase::Cancelled,
                Some(ErrorCode::Cancelled),
            );
        }
    }

    fn reject_backpressure(&mut self, kind: OperationKind) {
        self.metrics.backpressure.fetch_add(1, Ordering::AcqRel);
        self.publish_error(
            kind,
            ErrorCode::Backpressure,
            "connector operation limit reached",
        );
    }

    fn publish_service_error(&mut self, kind: OperationKind, error: ServiceError) {
        self.publish_error(kind, error.code, error.message);
    }

    fn publish_error(&mut self, kind: OperationKind, code: ErrorCode, message: &'static str) {
        let operation_id = self.new_operation_id();
        self.publish_operation_error(operation_id, kind, code, message);
    }

    fn publish_operation_error(
        &mut self,
        operation_id: OperationId,
        kind: OperationKind,
        code: ErrorCode,
        message: &'static str,
    ) {
        self.snapshot.result = Some(OperationResult {
            operation_id,
            text: message.to_owned(),
            truncated: false,
        });
        self.transition(kind, OperationPhase::Failed, Some(code));
        self.publish();
    }

    fn transition(
        &mut self,
        kind: OperationKind,
        phase: OperationPhase,
        error_code: Option<ErrorCode>,
    ) {
        self.transitions.push_back(DiagnosticTransition {
            kind,
            phase,
            error_code,
        });
        while self.transitions.len() > self.limits.diagnostic_transitions {
            self.transitions.pop_front();
        }
    }

    fn publish(&mut self) {
        self.snapshot.operations = Arc::from(self.operations.clone());
        self.snapshot.diagnostics = DiagnosticsSnapshot {
            command_queue_depth: self.metrics.queue_depth.load(Ordering::Acquire),
            active_mcp_operations: self.metrics.active_mcp.load(Ordering::Acquire),
            active_oauth_flows: self.metrics.active_oauth.load(Ordering::Acquire),
            backend_leases: self.mcp.active_leases().min(self.limits.backend_leases),
            timeouts: self.metrics.timeouts.load(Ordering::Acquire),
            cancellations: self.metrics.cancellations.load(Ordering::Acquire),
            stale_results: self.metrics.stale_results.load(Ordering::Acquire),
            backpressure_rejections: self.metrics.backpressure.load(Ordering::Acquire),
            transitions: Arc::from(self.transitions.iter().cloned().collect::<Vec<_>>()),
        };
        self.snapshot_cell.publish(self.snapshot.clone());
    }

    fn new_operation_id(&mut self) -> OperationId {
        self.operation_ids.next_id()
    }

    fn bump_generation(&mut self, server_id: &ServerId) -> u64 {
        let generation = {
            let generation = self.generations.entry(server_id.clone()).or_default();
            *generation = generation.saturating_add(1);
            *generation
        };
        self.metrics
            .generation_entries
            .store(self.generations.len(), Ordering::Release);
        generation
    }

    fn prune_generations(&mut self) {
        let mut retained: HashSet<ServerId> = self
            .snapshot
            .servers
            .iter()
            .map(|server| server.id.clone())
            .collect();
        retained.extend(self.jobs.values().map(|job| job.server_id.clone()));
        retained.extend(
            self.pending_invocations
                .values()
                .map(|pending| pending.server_id.clone()),
        );
        self.generations
            .retain(|server_id, _| retained.contains(server_id));
        self.metrics
            .generation_entries
            .store(self.generations.len(), Ordering::Release);
    }
}

fn validate_overview(
    overview: &crate::OverviewData,
    limits: ResourceLimits,
) -> Result<(), ServiceError> {
    if overview.servers.len() > limits.import_servers {
        return Err(ServiceError::new(
            ErrorCode::LimitExceeded,
            "server overview limit exceeded",
        ));
    }
    let mut bytes = 0usize;
    for server in &overview.servers {
        if server.tool_count > limits.tools_per_server {
            return Err(ServiceError::new(
                ErrorCode::LimitExceeded,
                "server tool count limit exceeded",
            ));
        }
        bytes = bytes
            .checked_add(server.id.as_str().len())
            .and_then(|total| total.checked_add(server.name.len()))
            .ok_or_else(|| {
                ServiceError::new(ErrorCode::LimitExceeded, "server overview byte overflow")
            })?;
        if bytes > limits.import_input_bytes {
            return Err(ServiceError::new(
                ErrorCode::LimitExceeded,
                "server overview byte limit exceeded",
            ));
        }
    }
    Ok(())
}

fn invalidates_inflight(intent: &ConnectorIntent) -> bool {
    matches!(
        intent,
        ConnectorIntent::SaveServer(_)
            | ConnectorIntent::DeleteServer(_)
            | ConnectorIntent::SetPermission { .. }
            | ConnectorIntent::ConnectSlack
            | ConnectorIntent::ImportConfiguration { .. }
    )
}

fn map_approval_reason(reason: audit::ApprovalReason) -> connector_contract::ApprovalReason {
    match reason {
        audit::ApprovalReason::AskRule => connector_contract::ApprovalReason::AskRule,
        audit::ApprovalReason::FirstUse => connector_contract::ApprovalReason::FirstUse,
        audit::ApprovalReason::SchemaChanged => connector_contract::ApprovalReason::SchemaChanged,
    }
}

fn map_approval_decision(decision: ApprovalDecision) -> audit::ApprovalDecision {
    match decision {
        ApprovalDecision::AllowOnce => audit::ApprovalDecision::AllowOnce,
        ApprovalDecision::AllowAlways => audit::ApprovalDecision::AllowAlways,
        ApprovalDecision::DenyOnce => audit::ApprovalDecision::DenyOnce,
        ApprovalDecision::DenyAlways => audit::ApprovalDecision::DenyAlways,
    }
}

fn error_code_label(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::InvalidInput => "invalid_input",
        ErrorCode::InvalidUrl => "invalid_url",
        ErrorCode::LimitExceeded => "limit_exceeded",
        ErrorCode::Backpressure => "backpressure",
        ErrorCode::StorageUnavailable => "storage_unavailable",
        ErrorCode::SecretUnavailable => "secret_unavailable",
        ErrorCode::TrustDenied => "trust_denied",
        ErrorCode::HostUnavailable => "host_unavailable",
        ErrorCode::PermissionDenied => "permission_denied",
        ErrorCode::AuditUnavailable => "audit_unavailable",
        ErrorCode::AuthenticationRequired => "authentication_required",
        ErrorCode::AuthenticationFailed => "authentication_failed",
        ErrorCode::OAuthCallbackFailed => "oauth_callback_failed",
        ErrorCode::NetworkTimeout => "network_timeout",
        ErrorCode::TransportFailed => "transport_failed",
        ErrorCode::ProtocolViolation => "protocol_violation",
        ErrorCode::StaleResult => "stale_result",
        ErrorCode::Cancelled => "cancelled",
        ErrorCode::UnknownDelivery => "unknown_delivery",
        ErrorCode::Internal => "internal",
    }
}

fn validate_discovered_tools(
    output: &DiscoverOutput,
    limits: ResourceLimits,
) -> Result<(), ServiceError> {
    if output.tools.len() > limits.tools_per_server {
        return Err(ServiceError::new(
            ErrorCode::LimitExceeded,
            "discovered tool count limit exceeded",
        ));
    }
    let mut total = 0usize;
    for tool in &output.tools {
        let mut actual_bytes = checked_add(tool.id.as_str().len(), tool.name.len())?;
        if let Some(description) = &tool.description {
            actual_bytes = checked_add(actual_bytes, description.len())?;
        }
        total = checked_add(total, tool.descriptor_bytes.max(actual_bytes))?;
        if total > limits.tool_descriptor_bytes {
            return Err(ServiceError::new(
                ErrorCode::LimitExceeded,
                "discovered descriptor limit exceeded",
            ));
        }
    }
    Ok(())
}

fn validate_tool_page(
    page: &crate::RepositoryToolPage,
    limits: ResourceLimits,
) -> Result<(), ServiceError> {
    if page.items.len() > TOOL_PAGE_SIZE || page.total > limits.tools_per_server {
        return Err(ServiceError::new(
            ErrorCode::LimitExceeded,
            "tool page item limit exceeded",
        ));
    }
    let mut bytes = 0usize;
    for item in &page.items {
        bytes = add_bounded(bytes, item.id.as_str().len(), limits.tool_descriptor_bytes)?;
        bytes = add_bounded(bytes, item.name.len(), limits.tool_descriptor_bytes)?;
        if let Some(description) = &item.description {
            bytes = add_bounded(bytes, description.len(), limits.tool_descriptor_bytes)?;
        }
    }
    Ok(())
}

fn validate_server_drafts(
    drafts: &[ServerDraft],
    limits: ResourceLimits,
) -> Result<(), ServiceError> {
    let mut bytes = 0usize;
    let mut items = 0usize;
    for draft in drafts {
        let (draft_bytes, draft_items) = server_draft_size(draft)?;
        bytes = add_bounded(bytes, draft_bytes, limits.import_input_bytes)?;
        items = add_bounded(items, draft_items, limits.tools_per_server)?;
    }
    Ok(())
}

fn validate_server_draft(draft: &ServerDraft, limits: ResourceLimits) -> Result<(), ServiceError> {
    if draft.name.trim().is_empty() || draft.name.contains('\0') {
        return Err(ServiceError::new(
            ErrorCode::InvalidInput,
            "connector server name is invalid",
        ));
    }
    match &draft.transport {
        TransportDraft::Stdio {
            command,
            args,
            plain_env,
            secret_env,
            ..
        } => {
            let has_nul = command.contains('\0')
                || args.iter().any(|value| value.contains('\0'))
                || plain_env
                    .iter()
                    .any(|(key, value)| key.contains('\0') || value.contains('\0'))
                || secret_env.iter().any(|(key, credential_id)| {
                    key.contains('\0') || credential_id.as_str().contains('\0')
                });
            if command.trim().is_empty() || has_nul {
                return Err(ServiceError::new(
                    ErrorCode::InvalidInput,
                    "connector stdio command or environment is invalid",
                ));
            }
        }
        TransportDraft::Http { url } => {
            mcp::validate_mcp_url(url).map_err(|_| {
                ServiceError::new(ErrorCode::InvalidUrl, "connector MCP URL is invalid")
            })?;
        }
    }
    let (bytes, items) = server_draft_size(draft)?;
    if bytes > limits.import_input_bytes || items > limits.tools_per_server {
        return Err(ServiceError::new(
            ErrorCode::LimitExceeded,
            "server configuration resource limit exceeded",
        ));
    }
    Ok(())
}

fn validate_loaded_server_id(
    requested: &ServerId,
    draft: &ServerDraft,
) -> Result<(), ServiceError> {
    if draft.id.as_ref().map(ServerId::as_str) == Some(requested.as_str()) {
        Ok(())
    } else {
        Err(ServiceError::new(
            ErrorCode::StorageUnavailable,
            "repository returned a different server configuration",
        ))
    }
}

fn server_draft_size(draft: &ServerDraft) -> Result<(usize, usize), ServiceError> {
    let mut bytes = draft.name.len();
    let mut items = 1usize;
    if let Some(id) = &draft.id {
        bytes = checked_add(bytes, id.as_str().len())?;
    }
    match &draft.transport {
        TransportDraft::Http { url } => Ok((checked_add(bytes, url.len())?, items)),
        TransportDraft::Stdio {
            command,
            args,
            plain_env,
            secret_env,
            ..
        } => {
            bytes = checked_add(bytes, command.len())?;
            for arg in args {
                bytes = checked_add(bytes, arg.len())?;
            }
            items = checked_add(items, args.len())?;
            for (key, value) in plain_env {
                bytes = checked_add(bytes, key.len())?;
                bytes = checked_add(bytes, value.len())?;
            }
            items = checked_add(items, plain_env.len())?;
            for (key, credential_id) in secret_env {
                bytes = checked_add(bytes, key.len())?;
                bytes = checked_add(bytes, credential_id.as_str().len())?;
            }
            items = checked_add(items, secret_env.len())?;
            Ok((bytes, items))
        }
    }
}

fn validate_oauth_client(
    client: &StoredOAuthClient,
    limits: ResourceLimits,
) -> Result<(), ServiceError> {
    if client.client_secret.len() > limits.tool_input_bytes {
        return Err(ServiceError::new(
            ErrorCode::LimitExceeded,
            "OAuth secret input limit exceeded",
        ));
    }
    let mut bytes = checked_add(client.server_id.as_str().len(), client.client_id.len())?;
    if let Some(workspace_hint) = &client.workspace_hint {
        bytes = checked_add(bytes, workspace_hint.len())?;
    }
    if bytes > limits.import_input_bytes {
        return Err(ServiceError::new(
            ErrorCode::LimitExceeded,
            "OAuth metadata input limit exceeded",
        ));
    }
    Ok(())
}

fn validate_oauth_discovery(
    discovery: &OAuthDiscovery,
    limits: ResourceLimits,
) -> Result<(), ServiceError> {
    if discovery.continuation.expose_bytes().len() > limits.raw_mcp_response_bytes
        || discovery.scopes.len() > limits.import_servers
    {
        return Err(ServiceError::new(
            ErrorCode::LimitExceeded,
            "OAuth discovery limit exceeded",
        ));
    }
    let authority = discovery.authority.as_str();
    let resource = discovery.resource.as_str();
    if authority.is_empty()
        || resource.is_empty()
        || authority.contains('\0')
        || resource.contains('\0')
    {
        return Err(ServiceError::new(
            ErrorCode::InvalidInput,
            "OAuth endpoint display is invalid",
        ));
    }
    let mut bytes = checked_add(authority.len(), resource.len())?;
    for scope in &discovery.scopes {
        if scope.is_empty() || scope.contains('\0') {
            return Err(ServiceError::new(
                ErrorCode::InvalidInput,
                "OAuth scope is invalid",
            ));
        }
        bytes = add_bounded(bytes, scope.len(), limits.import_report_bytes)?;
    }
    if bytes > limits.import_report_bytes {
        return Err(ServiceError::new(
            ErrorCode::LimitExceeded,
            "OAuth discovery display limit exceeded",
        ));
    }
    Ok(())
}

fn validate_import_report_items(
    items: &[ImportReportItem],
    limits: ResourceLimits,
) -> Result<(), ServiceError> {
    if items.len() > limits.import_report_items {
        return Err(ServiceError::new(
            ErrorCode::LimitExceeded,
            "import report item limit exceeded",
        ));
    }
    let mut bytes = 0usize;
    for item in items {
        if item.name.contains('\0') || item.omitted_secret_env_count > limits.tools_per_server {
            return Err(ServiceError::new(
                ErrorCode::InvalidInput,
                "import report item is invalid",
            ));
        }
        bytes = add_bounded(bytes, item.name.len(), limits.import_report_bytes)?;
        bytes = add_bounded(bytes, 32, limits.import_report_bytes)?;
    }
    Ok(())
}

fn bound_import_report(
    items: Vec<ImportReportItem>,
    limits: ResourceLimits,
) -> (Vec<ImportReportItem>, bool) {
    let original_len = items.len();
    let mut retained = Vec::with_capacity(original_len.min(limits.import_report_items));
    let mut bytes = 0usize;
    for item in items {
        let Some(next_bytes) = bytes
            .checked_add(item.name.len())
            .and_then(|n| n.checked_add(32))
        else {
            return (retained, true);
        };
        if retained.len() >= limits.import_report_items || next_bytes > limits.import_report_bytes {
            return (retained, true);
        }
        bytes = next_bytes;
        retained.push(item);
    }
    let truncated = retained.len() < original_len;
    (retained, truncated)
}

fn purpose_kind(purpose: RemoteTrustPurpose) -> OperationKind {
    match purpose {
        RemoteTrustPurpose::Discover => OperationKind::Discover,
        RemoteTrustPurpose::Invoke => OperationKind::Invoke,
        RemoteTrustPurpose::OAuth => OperationKind::OAuth,
    }
}

fn endpoint_fingerprint_for(
    server_id: &ServerId,
    config_revision: Revision,
    url: &str,
) -> Option<EndpointFingerprint> {
    let identity = format!("{}\u{0}{}\u{0}{url}", server_id.as_str(), config_revision.0);
    EndpointFingerprint::parse(audit::schema_hash(&identity)).ok()
}

fn checked_add(total: usize, additional: usize) -> Result<usize, ServiceError> {
    total.checked_add(additional).ok_or_else(|| {
        ServiceError::new(
            ErrorCode::LimitExceeded,
            "connector resident byte count overflow",
        )
    })
}

fn add_bounded(total: usize, additional: usize, limit: usize) -> Result<usize, ServiceError> {
    let total = checked_add(total, additional)?;
    if total > limit {
        return Err(ServiceError::new(
            ErrorCode::LimitExceeded,
            "connector resident byte limit exceeded",
        ));
    }
    Ok(total)
}

fn truncate_utf8(mut text: String, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    (text, true)
}

fn run_backend_job<T>(run: impl FnOnce() -> Result<T, ServiceError>) -> Result<T, ServiceError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).unwrap_or_else(|_| {
        Err(ServiceError::new(
            ErrorCode::Internal,
            "connector backend job panicked",
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Condvar;
    use std::sync::atomic::AtomicBool;

    use connector_contract::{
        ConnectionState, PermissionRule, SensitiveInput, ServerSummary, SlackProjection,
        SlackStatus, ToolId, ToolListItem, TransportDraft, TransportKind,
    };

    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        changed: Condvar,
    }

    impl Gate {
        fn closed() -> Self {
            Self::default()
        }

        fn opened() -> Self {
            Self {
                open: Mutex::new(true),
                changed: Condvar::new(),
            }
        }

        fn wait(&self) {
            let mut open = self.open.lock().expect("gate lock");
            while !*open {
                open = self.changed.wait(open).expect("gate wait");
            }
        }

        fn release(&self) {
            *self.open.lock().expect("gate lock") = true;
            self.changed.notify_all();
        }
    }

    struct RepoState {
        opens: AtomicUsize,
        parses: AtomicUsize,
        imports: AtomicUsize,
        replacements: AtomicUsize,
        slack_ensures: AtomicUsize,
        config_revision: AtomicU64,
        import_server_count: AtomicUsize,
        server_loads: AtomicUsize,
        authorization_loads: AtomicUsize,
        preflights: AtomicUsize,
        completions: AtomicUsize,
        shutdowns: AtomicUsize,
        permission: Mutex<audit::PermissionFingerprint>,
        permission_mutation_on_preflight: Mutex<Option<audit::PermissionFingerprint>>,
        loaded_server_override: Mutex<Option<ServerDraft>>,
        outcome_failure: AtomicBool,
        preflight_input_override: Mutex<Option<Vec<u8>>>,
        authorization_ledger: audit::InMemoryAuthorizationLedger,
        oauth_slot: Mutex<Option<secret::PhysicalSecretSlot>>,
        order: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Default for RepoState {
        fn default() -> Self {
            Self {
                opens: AtomicUsize::new(0),
                parses: AtomicUsize::new(0),
                imports: AtomicUsize::new(0),
                replacements: AtomicUsize::new(0),
                slack_ensures: AtomicUsize::new(0),
                config_revision: AtomicU64::new(1),
                import_server_count: AtomicUsize::new(1),
                server_loads: AtomicUsize::new(0),
                authorization_loads: AtomicUsize::new(0),
                preflights: AtomicUsize::new(0),
                completions: AtomicUsize::new(0),
                shutdowns: AtomicUsize::new(0),
                permission: Mutex::new(audit::PermissionFingerprint::Absent),
                permission_mutation_on_preflight: Mutex::new(None),
                loaded_server_override: Mutex::new(None),
                outcome_failure: AtomicBool::new(false),
                preflight_input_override: Mutex::new(None),
                authorization_ledger: audit::InMemoryAuthorizationLedger::new(
                    "connector-service-test",
                )
                .expect("authorization ledger"),
                oauth_slot: Mutex::new(None),
                order: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    struct FakeFactory {
        state: Arc<RepoState>,
        open_gate: Arc<Gate>,
    }

    impl ConnectorRepositoryFactory for FakeFactory {
        fn open(&self) -> Result<Box<dyn ConnectorRepository>, ServiceError> {
            self.state.opens.fetch_add(1, Ordering::AcqRel);
            self.open_gate.wait();
            Ok(Box::new(FakeRepository {
                state: Arc::clone(&self.state),
            }))
        }
    }

    struct FakeRepository {
        state: Arc<RepoState>,
    }

    impl FakeRepository {
        fn revision(&self) -> Revision {
            Revision(self.state.config_revision.load(Ordering::Acquire))
        }

        fn mutate(&self) -> Revision {
            Revision(self.state.config_revision.fetch_add(1, Ordering::AcqRel) + 1)
        }
    }

    impl ConnectorRepository for FakeRepository {
        fn load_overview(&mut self) -> Result<crate::OverviewData, ServiceError> {
            Ok(crate::OverviewData {
                config_revision: self.revision(),
                slack: SlackProjection {
                    server_id: (self.state.slack_ensures.load(Ordering::Acquire) != 0)
                        .then(|| ServerId::new("server-1")),
                    status: if self.state.slack_ensures.load(Ordering::Acquire) == 0 {
                        SlackStatus::NotConfigured
                    } else {
                        SlackStatus::Ready
                    },
                    ..SlackProjection::default()
                },
                servers: vec![server_summary("server-1"), server_summary("server-2")],
            })
        }

        fn load_server(&mut self, server_id: &ServerId) -> Result<ServerDraft, ServiceError> {
            self.state.server_loads.fetch_add(1, Ordering::AcqRel);
            let loaded = self
                .state
                .loaded_server_override
                .lock()
                .expect("server override lock")
                .clone()
                .unwrap_or_else(|| server_draft(server_id.as_str()));
            Ok(loaded)
        }

        fn load_tool_page(
            &mut self,
            server_id: &ServerId,
            offset: usize,
            _limit: usize,
        ) -> Result<crate::RepositoryToolPage, ServiceError> {
            Ok(crate::RepositoryToolPage {
                total: 1,
                items: vec![ToolListItem {
                    id: ToolId::new(format!("{}-tool", server_id.as_str())),
                    name: format!("tool-{offset}"),
                    description: None,
                    permission: PermissionRule::Ask,
                }],
            })
        }

        fn load_tool_name(
            &mut self,
            _server_id: &ServerId,
            tool_id: &ToolId,
        ) -> Result<String, ServiceError> {
            Ok(tool_id.as_str().to_owned())
        }

        fn save_server(&mut self, _draft: ServerDraft) -> Result<Revision, ServiceError> {
            Ok(self.mutate())
        }

        fn delete_server(&mut self, _server_id: &ServerId) -> Result<Revision, ServiceError> {
            Ok(self.mutate())
        }

        fn replace_tools(
            &mut self,
            _server_id: &ServerId,
            _tools: &[crate::DiscoveredTool],
        ) -> Result<Revision, ServiceError> {
            self.state.replacements.fetch_add(1, Ordering::AcqRel);
            Ok(self.mutate())
        }

        fn set_permission(
            &mut self,
            _server_id: &ServerId,
            _tool_id: &ToolId,
            _rule: PermissionRule,
        ) -> Result<Revision, ServiceError> {
            Ok(self.mutate())
        }

        fn ensure_slack_server(&mut self) -> Result<Revision, ServiceError> {
            self.state.slack_ensures.fetch_add(1, Ordering::AcqRel);
            Ok(self.mutate())
        }

        fn parse_import(
            &mut self,
            _source_name: &str,
            _bytes: &[u8],
        ) -> Result<crate::ImportPlan, ServiceError> {
            self.state.parses.fetch_add(1, Ordering::AcqRel);
            let count = self.state.import_server_count.load(Ordering::Acquire);
            Ok(crate::ImportPlan {
                servers: (0..count)
                    .map(|index| server_draft(&format!("import-{index}")))
                    .collect(),
                report: Vec::new(),
            })
        }

        fn import_servers(&mut self, _servers: Vec<ServerDraft>) -> Result<Revision, ServiceError> {
            self.state.imports.fetch_add(1, Ordering::AcqRel);
            Ok(self.mutate())
        }

        fn load_http_auth_binding(
            &mut self,
            _server_id: &ServerId,
            _exact_url: &str,
        ) -> Result<Option<crate::ports::HttpAuthBinding>, ServiceError> {
            Ok(None)
        }

        fn load_oauth_secret_slot(
            &mut self,
            _logical_id: &secret::LogicalCredentialId,
        ) -> Result<Option<secret::PhysicalSecretSlot>, ServiceError> {
            Ok(self
                .state
                .oauth_slot
                .lock()
                .expect("OAuth slot lock")
                .clone())
        }

        fn publish_oauth_secret_slot(
            &mut self,
            staged: &secret::StagedSecretBundle,
            expected_previous: Option<&secret::PhysicalSecretSlot>,
            _oauth_metadata_json: &str,
            _masked_hint: Option<&str>,
        ) -> Result<bool, ServiceError> {
            let mut slot = self.state.oauth_slot.lock().expect("OAuth slot lock");
            if slot.as_ref() != expected_previous {
                return Ok(false);
            }
            *slot = Some(staged.new_slot.clone());
            Ok(true)
        }

        fn load_authorization_state(
            &mut self,
            _server_id: &ServerId,
            _tool_name: &str,
        ) -> Result<crate::AuthorizationState, ServiceError> {
            self.state
                .authorization_loads
                .fetch_add(1, Ordering::AcqRel);
            self.state
                .order
                .lock()
                .expect("order lock")
                .push("permission");
            Ok(crate::AuthorizationState {
                permission: self
                    .state
                    .permission
                    .lock()
                    .expect("permission lock")
                    .clone(),
            })
        }

        fn commit_authorization_preflight(
            &mut self,
            plan: audit::AuthorizationPlan,
            arguments_json: &SensitiveInput,
        ) -> Result<audit::AuthorizationPreflight, ServiceError> {
            self.state.preflights.fetch_add(1, Ordering::AcqRel);
            self.state
                .order
                .lock()
                .expect("order lock")
                .push("preflight");
            if let Some(replacement) = self
                .state
                .permission_mutation_on_preflight
                .lock()
                .expect("permission mutation lock")
                .take()
            {
                *self.state.permission.lock().expect("permission lock") = replacement;
            }
            if self
                .state
                .permission
                .lock()
                .expect("permission lock")
                .ne(plan.expected_permission())
            {
                return Err(ServiceError::new(
                    ErrorCode::AuditUnavailable,
                    "fixture permission changed before preflight",
                ));
            }
            self.state
                .authorization_ledger
                .preflight(
                    plan,
                    self.state
                        .preflight_input_override
                        .lock()
                        .expect("preflight input override lock")
                        .as_deref()
                        .unwrap_or_else(|| arguments_json.expose_bytes()),
                )
                .map_err(|_| {
                    ServiceError::new(
                        ErrorCode::AuditUnavailable,
                        "fixture authorization preflight failed",
                    )
                })
        }

        fn complete_authorization(
            &mut self,
            operation_id: &OperationId,
            outcome: audit::AuthorizationOutcome,
        ) -> Result<(), ServiceError> {
            self.state.completions.fetch_add(1, Ordering::AcqRel);
            if self.state.outcome_failure.load(Ordering::Acquire) {
                return Err(ServiceError::new(
                    ErrorCode::AuditUnavailable,
                    "fixture outcome failure",
                ));
            }
            self.state
                .authorization_ledger
                .complete(operation_id.as_str(), outcome)
                .map_err(|_| {
                    ServiceError::new(
                        ErrorCode::AuditUnavailable,
                        "fixture authorization outcome failed",
                    )
                })?;
            self.state.order.lock().expect("order lock").push("outcome");
            Ok(())
        }

        fn shutdown(&mut self) -> Result<(), ServiceError> {
            self.state.shutdowns.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    struct FakeSecrets;

    impl ConnectorSecrets for FakeSecrets {
        fn credential_revisions(
            &self,
            credential_ids: Vec<connector_contract::CredentialId>,
        ) -> Result<Vec<CredentialResolutionRequest>, ServiceError> {
            credential_ids
                .into_iter()
                .map(|credential_id| {
                    let logical = secret::LogicalCredentialId::new(credential_id.as_str())
                        .map_err(|_| {
                            ServiceError::new(
                                ErrorCode::SecretUnavailable,
                                "fixture logical credential failed",
                            )
                        })?;
                    Ok(CredentialResolutionRequest {
                        credential_id,
                        expected_physical_slot: Some(secret::PhysicalSecretSlot::allocate(
                            &logical,
                        )),
                    })
                })
                .collect()
        }

        fn resolve_credentials(
            &self,
            requests: Vec<CredentialResolutionRequest>,
        ) -> Result<crate::ports::ResolvedCredentials, ServiceError> {
            let values = requests
                .iter()
                .map(|_| secret::SecretString::new("fixture-secret-value".to_owned()))
                .collect::<Vec<_>>();
            let redaction = secret::RedactionService::new()
                .acquire_execution_lease(&values.iter().collect::<Vec<_>>())
                .map_err(|_| {
                    ServiceError::new(
                        ErrorCode::SecretUnavailable,
                        "fixture redaction lease failed",
                    )
                })?;
            let entries = requests
                .into_iter()
                .zip(values)
                .map(|(request, value)| {
                    let logical = secret::LogicalCredentialId::new(request.credential_id.as_str())
                        .map_err(|_| {
                            ServiceError::new(
                                ErrorCode::SecretUnavailable,
                                "fixture logical credential failed",
                            )
                        })?;
                    let slot = request
                        .expected_physical_slot
                        .unwrap_or_else(|| secret::PhysicalSecretSlot::allocate(&logical));
                    crate::ports::ResolvedCredential::new(request.credential_id, slot, value)
                })
                .collect::<Result<Vec<_>, _>>()?;
            crate::ports::ResolvedCredentials::new(entries, redaction)
        }

        fn stage_oauth_bundle(
            &self,
            logical_id: secret::LogicalCredentialId,
            previous_slot: Option<secret::PhysicalSecretSlot>,
            bundle: secret::SecretBundle,
        ) -> Result<secret::StagedSecretBundle, ServiceError> {
            let plan = secret::SecretBundleStagePlan::allocate(logical_id, previous_slot)
                .map_err(|_| ServiceError::new(ErrorCode::Internal, "fixture stage plan failed"))?;
            let entries = secret::BundleEntryPresence {
                access: true,
                refresh: bundle.refresh().is_some(),
                dcr: bundle.dcr().is_some(),
            };
            drop(bundle);
            Ok(secret::StagedSecretBundle {
                logical_id: plan.logical_id().clone(),
                new_slot: plan.new_slot().clone(),
                previous_slot: plan.previous_slot().cloned(),
                entries,
            })
        }

        fn delete_oauth_bundle(
            &self,
            _slot: &secret::PhysicalSecretSlot,
        ) -> Result<(), ServiceError> {
            Ok(())
        }

        fn sanitized_input_preview(
            &self,
            _arguments_json: &SensitiveInput,
            max_chars: usize,
        ) -> Result<String, ServiceError> {
            Ok("{}".chars().take(max_chars).collect())
        }
    }

    struct FakeMcp {
        gate: Arc<Gate>,
        active: AtomicUsize,
        peak: AtomicUsize,
        tool_count: AtomicUsize,
        descriptor_bytes: AtomicUsize,
        cancel_releases: AtomicBool,
        panic_on_discover: AtomicBool,
        schema_calls: AtomicUsize,
        invoke_calls: AtomicUsize,
        schema_tool_override: Mutex<Option<ToolId>>,
        invoke_error: Mutex<Option<ServiceError>>,
        call_operation_ids: Mutex<Vec<String>>,
        pinned: Mutex<HashSet<OperationId>>,
        order: Arc<Mutex<Vec<&'static str>>>,
        leases: AtomicUsize,
        reap_calls: AtomicUsize,
    }

    impl FakeMcp {
        fn new(gate: Arc<Gate>, order: Arc<Mutex<Vec<&'static str>>>) -> Self {
            Self {
                gate,
                active: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                tool_count: AtomicUsize::new(1),
                descriptor_bytes: AtomicUsize::new(1),
                cancel_releases: AtomicBool::new(true),
                panic_on_discover: AtomicBool::new(false),
                schema_calls: AtomicUsize::new(0),
                invoke_calls: AtomicUsize::new(0),
                schema_tool_override: Mutex::new(None),
                invoke_error: Mutex::new(None),
                call_operation_ids: Mutex::new(Vec::new()),
                pinned: Mutex::new(HashSet::new()),
                order,
                leases: AtomicUsize::new(0),
                reap_calls: AtomicUsize::new(0),
            }
        }

        fn enter(&self) -> ActiveFixture<'_> {
            let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
            self.peak.fetch_max(active, Ordering::AcqRel);
            ActiveFixture(&self.active)
        }
    }

    struct ActiveFixture<'a>(&'a AtomicUsize);

    impl Drop for ActiveFixture<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }

    impl ConnectorMcp for FakeMcp {
        fn discover(
            &self,
            _operation_id: &OperationId,
            _target: McpRequestTarget,
            _cancellation: CancellationToken,
        ) -> Result<DiscoverOutput, ServiceError> {
            let _active = self.enter();
            if self.panic_on_discover.load(Ordering::Acquire) {
                panic!("injected connector backend panic");
            }
            self.gate.wait();
            let count = self.tool_count.load(Ordering::Acquire);
            let descriptor_bytes = self.descriptor_bytes.load(Ordering::Acquire);
            Ok(DiscoverOutput {
                tools: (0..count)
                    .map(|index| crate::DiscoveredTool {
                        id: ToolId::new(format!("tool-{index}")),
                        name: format!("tool-{index}"),
                        description: None,
                        descriptor_bytes,
                    })
                    .collect(),
            })
        }

        fn load_live_schema(
            &self,
            operation_id: &OperationId,
            _target: McpRequestTarget,
            tool_id: ToolId,
            tool_name: String,
            cancellation: CancellationToken,
        ) -> Result<LiveToolSchema, ServiceError> {
            self.schema_calls.fetch_add(1, Ordering::AcqRel);
            self.order.lock().expect("order lock").push("schema");
            self.gate.wait();
            if cancellation.is_cancelled() {
                return Err(ServiceError::new(
                    ErrorCode::Cancelled,
                    "schema load cancelled",
                ));
            }
            let tool_id = self
                .schema_tool_override
                .lock()
                .expect("schema tool lock")
                .clone()
                .unwrap_or(tool_id);
            let tool_name = if self
                .schema_tool_override
                .lock()
                .expect("schema tool lock")
                .is_some()
            {
                tool_id.as_str().to_owned()
            } else {
                tool_name
            };
            self.pinned
                .lock()
                .expect("fixture pinned lease lock")
                .insert(operation_id.clone());
            Ok(LiveToolSchema {
                tool_name,
                tool_id,
                input_schema_json: r#"{"type":"object"}"#.to_owned(),
            })
        }

        fn invoke_authorized(
            &self,
            request: AuthorizedInvokeRequest,
            cancellation: CancellationToken,
        ) -> Result<String, ServiceError> {
            if cancellation.is_cancelled() {
                return Err(ServiceError::new(
                    ErrorCode::Cancelled,
                    "authorized call cancelled before send",
                ));
            }
            let _active = self.enter();
            self.invoke_calls.fetch_add(1, Ordering::AcqRel);
            self.order.lock().expect("order lock").push("call");
            self.call_operation_ids
                .lock()
                .expect("call ids lock")
                .push(request.operation_id().to_owned());
            self.pinned
                .lock()
                .expect("fixture pinned lease lock")
                .remove(&OperationId::new(request.operation_id().to_owned()));
            self.gate.wait();
            if let Some(error) = *self.invoke_error.lock().expect("invoke error lock") {
                return Err(error);
            }
            Ok("ok".to_owned())
        }

        fn cancel(&self, operation_id: &OperationId) {
            self.pinned
                .lock()
                .expect("fixture pinned lease lock")
                .remove(operation_id);
            if self.cancel_releases.load(Ordering::Acquire) {
                self.gate.release();
            }
        }

        fn active_leases(&self) -> usize {
            self.leases.load(Ordering::Acquire)
                + self.pinned.lock().expect("fixture pinned lease lock").len()
        }

        fn reap_idle_leases(&self) {
            self.reap_calls.fetch_add(1, Ordering::AcqRel);
            self.leases.store(0, Ordering::Release);
        }
    }

    struct FakeOAuth {
        gate: Arc<Gate>,
        active: AtomicUsize,
        peak: AtomicUsize,
    }

    impl FakeOAuth {
        fn new(gate: Arc<Gate>) -> Self {
            Self {
                gate,
                active: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
            }
        }

        fn enter(&self) -> ActiveFixture<'_> {
            let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
            self.peak.fetch_max(active, Ordering::AcqRel);
            ActiveFixture(&self.active)
        }
    }

    impl ConnectorOAuth for FakeOAuth {
        fn discover(
            &self,
            _operation_id: &OperationId,
            _server: ServerDraft,
            _choose_workspace: bool,
            cancellation: CancellationToken,
        ) -> Result<OAuthDiscovery, ServiceError> {
            let _active = self.enter();
            self.gate.wait();
            if cancellation.is_cancelled() {
                return Err(ServiceError::new(
                    ErrorCode::Cancelled,
                    "OAuth discovery cancelled",
                ));
            }
            Ok(OAuthDiscovery {
                continuation: OAuthContinuation::new(SensitiveInput::from(
                    "fixture-state".to_owned(),
                )),
                authority: EndpointDisplay::new("https://auth.invalid"),
                resource: EndpointDisplay::new("https://resource.invalid"),
                scopes: vec!["tools:read".to_owned()],
            })
        }

        fn authorize(
            &self,
            _operation_id: &OperationId,
            _continuation: OAuthContinuation,
            _client: Option<StoredOAuthClient>,
            workspace: Option<String>,
            events: Arc<dyn OAuthEventSink>,
            cancellation: CancellationToken,
        ) -> Result<OAuthAuthorizeOutput, ServiceError> {
            let _active = self.enter();
            self.gate.wait();
            if cancellation.is_cancelled() {
                return Err(ServiceError::new(
                    ErrorCode::Cancelled,
                    "OAuth authorization cancelled",
                ));
            }
            events.callback_bound(SensitiveInput::from(
                "https://auth.invalid/authorize?state=REDACTED".to_owned(),
            ))?;
            let logical_id = secret::LogicalCredentialId::new("fixture-oauth")
                .map_err(|_| ServiceError::new(ErrorCode::Internal, "fixture logical id failed"))?;
            Ok(OAuthAuthorizeOutput::Completed(
                crate::ports::OAuthCompletion {
                    credential: crate::ports::OAuthCredentialUpdate {
                        logical_id,
                        bundle: secret::SecretBundle::new(
                            secret::SecretString::new("fixture-access".to_owned()),
                            Some(secret::SecretString::new("fixture-refresh".to_owned())),
                            None,
                        ),
                        oauth_metadata_json: "{}".to_owned(),
                        masked_hint: Some("****ture".to_owned()),
                    },
                    workspace_label: workspace,
                    can_choose_workspace: true,
                },
            ))
        }

        fn cancel(&self, _operation_id: &OperationId) {
            self.gate.release();
        }
    }

    #[derive(Default)]
    struct FakeClock {
        millis: AtomicU64,
    }

    impl CoordinatorClock for FakeClock {
        fn now(&self) -> Duration {
            Duration::from_millis(self.millis.load(Ordering::Acquire))
        }

        fn wait_duration(&self, requested: Duration) -> Duration {
            self.millis.fetch_add(
                u64::try_from(requested.as_millis()).unwrap_or(u64::MAX),
                Ordering::AcqRel,
            );
            Duration::ZERO
        }
    }

    struct Fixture {
        coordinator: ConnectorCoordinator,
        repo: Arc<RepoState>,
        mcp: Arc<FakeMcp>,
        oauth: Arc<FakeOAuth>,
    }

    #[derive(Default)]
    struct FakeHost {
        wakes: AtomicUsize,
    }

    impl ConnectorHost for FakeHost {
        fn wake(&self) {
            self.wakes.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn fixture(
        open_gate: Arc<Gate>,
        mcp_gate: Arc<Gate>,
        oauth_gate: Arc<Gate>,
        clock: Arc<dyn CoordinatorClock>,
    ) -> Fixture {
        let repo = Arc::new(RepoState::default());
        let mcp = Arc::new(FakeMcp::new(mcp_gate, Arc::clone(&repo.order)));
        let oauth = Arc::new(FakeOAuth::new(oauth_gate));
        let host = Arc::new(FakeHost::default());
        let coordinator = ConnectorCoordinator::new(ConnectorCoordinatorConfig {
            limits: ResourceLimits::default(),
            idle_ttl: Duration::from_secs(5),
            repository_factory: Arc::new(FakeFactory {
                state: Arc::clone(&repo),
                open_gate: Arc::clone(&open_gate),
            }),
            secrets: Arc::new(FakeSecrets),
            mcp: mcp.clone(),
            oauth: oauth.clone(),
            host,
            clock,
            operation_ids: Arc::new(SystemOperationIdFactory::default()),
        })
        .unwrap();
        Fixture {
            coordinator,
            repo,
            mcp,
            oauth,
        }
    }

    fn server_draft(id: &str) -> ServerDraft {
        ServerDraft {
            id: Some(ServerId::new(id)),
            name: id.to_owned(),
            transport: TransportDraft::Stdio {
                command: "fixture-mcp".to_owned(),
                args: Vec::new(),
                plain_env: Vec::new(),
                secret_env: Vec::new(),
                inherit_env: false,
            },
            enabled: true,
        }
    }

    fn server_summary(id: &str) -> ServerSummary {
        ServerSummary {
            id: ServerId::new(id),
            name: id.to_owned(),
            transport: TransportKind::Stdio,
            enabled: true,
            connection: ConnectionState::Idle,
            tool_count: 0,
            error_code: None,
        }
    }

    fn wait_until(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !predicate() {
            assert!(Instant::now() < deadline, "condition timed out");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn unused_coordinator_has_zero_thread_and_repository_open() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        let mut reader = fixture.coordinator.snapshot_reader();
        assert!(!reader.refresh());
        assert_eq!(fixture.coordinator.metrics().worker_starts, 0);
        assert_eq!(fixture.coordinator.metrics().worker_alive, 0);
        assert_eq!(fixture.repo.opens.load(Ordering::Acquire), 0);
    }

    #[test]
    fn command_queue_is_exactly_eight_and_backpressures() {
        let open_gate = Arc::new(Gate::closed());
        let fixture = fixture(
            Arc::clone(&open_gate),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        fixture
            .coordinator
            .dispatch(ConnectorIntent::Activate)
            .unwrap();
        wait_until(|| fixture.repo.opens.load(Ordering::Acquire) == 1);
        for _ in 1..8 {
            fixture
                .coordinator
                .dispatch(ConnectorIntent::Activate)
                .unwrap();
        }
        assert_eq!(fixture.coordinator.metrics().command_queue_depth, 8);
        assert_eq!(
            fixture.coordinator.dispatch(ConnectorIntent::Activate),
            Err(DispatchError::Backpressure)
        );
        open_gate.release();
    }

    #[test]
    fn snapshot_reader_clones_only_after_revision_change() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        let mut reader = fixture.coordinator.snapshot_reader();
        fixture
            .coordinator
            .dispatch(ConnectorIntent::Activate)
            .unwrap();
        wait_until(|| fixture.coordinator.current_snapshot().revision.0 > 0);
        assert!(reader.refresh());
        let first = Arc::clone(reader.snapshot());
        assert!(!reader.refresh());
        assert!(Arc::ptr_eq(&first, reader.snapshot()));

        fixture
            .coordinator
            .dispatch(ConnectorIntent::DeleteServer(ServerId::new(
                "snapshot-change",
            )))
            .unwrap();
        wait_until(|| fixture.coordinator.current_snapshot().revision.0 > first.revision.0);
        assert!(reader.refresh());
        assert!(!Arc::ptr_eq(&first, reader.snapshot()));
    }

    #[test]
    fn mcp_two_and_oauth_one_are_hard_concurrency_caps() {
        let mcp_gate = Arc::new(Gate::closed());
        let oauth_gate = Arc::new(Gate::closed());
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::clone(&mcp_gate),
            Arc::clone(&oauth_gate),
            Arc::new(SystemCoordinatorClock::default()),
        );
        fixture
            .coordinator
            .dispatch(ConnectorIntent::Activate)
            .unwrap();
        wait_until(|| fixture.coordinator.current_snapshot().config_revision.0 == 1);
        fixture
            .coordinator
            .dispatch(ConnectorIntent::Discover(ServerId::new("server-1")))
            .unwrap();
        fixture
            .coordinator
            .dispatch(ConnectorIntent::Discover(ServerId::new("server-2")))
            .unwrap();
        wait_until(|| fixture.mcp.active.load(Ordering::Acquire) == 2);
        fixture
            .coordinator
            .dispatch(ConnectorIntent::Discover(ServerId::new("server-3")))
            .unwrap();
        wait_until(|| fixture.coordinator.metrics().backpressure_rejections >= 1);
        assert_eq!(fixture.mcp.peak.load(Ordering::Acquire), 2);

        fixture
            .coordinator
            .dispatch(ConnectorIntent::BeginOAuth(ServerId::new("oauth-server-1")))
            .unwrap();
        wait_until(|| fixture.oauth.active.load(Ordering::Acquire) == 1);
        fixture
            .coordinator
            .dispatch(ConnectorIntent::BeginOAuth(ServerId::new("oauth-server-2")))
            .unwrap();
        wait_until(|| fixture.coordinator.metrics().backpressure_rejections >= 2);
        assert_eq!(fixture.oauth.peak.load(Ordering::Acquire), 1);

        let mcp_operations = fixture
            .coordinator
            .current_snapshot()
            .operations
            .iter()
            .filter(|operation| operation.kind == OperationKind::Discover)
            .map(|operation| operation.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(mcp_operations.len(), 2);
        for operation_id in mcp_operations {
            fixture
                .coordinator
                .dispatch(ConnectorIntent::Cancel(operation_id))
                .unwrap();
        }
        oauth_gate.release();
        wait_until(|| {
            fixture
                .coordinator
                .current_snapshot()
                .oauth
                .as_ref()
                .is_some_and(|state| matches!(state.phase, OAuthUiPhase::AwaitingConsent { .. }))
        });
        let oauth = fixture
            .coordinator
            .current_snapshot()
            .oauth
            .clone()
            .expect("OAuth consent state");
        fixture
            .coordinator
            .dispatch(ConnectorIntent::ResolveOAuthConsent {
                operation_id: oauth.operation_id,
                config_revision: oauth.config_revision,
                accepted: true,
            })
            .unwrap();
        wait_until(|| {
            fixture.coordinator.metrics().active_mcp_operations == 0
                && fixture.coordinator.metrics().active_oauth_flows == 0
        });
    }

    #[test]
    fn config_revision_change_discards_stale_discovery() {
        let mcp_gate = Arc::new(Gate::closed());
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::clone(&mcp_gate),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        fixture
            .coordinator
            .dispatch(ConnectorIntent::Activate)
            .unwrap();
        wait_until(|| fixture.coordinator.current_snapshot().config_revision.0 == 1);
        fixture
            .coordinator
            .dispatch(ConnectorIntent::Discover(ServerId::new("server-1")))
            .unwrap();
        wait_until(|| fixture.mcp.active.load(Ordering::Acquire) == 1);
        fixture
            .coordinator
            .dispatch(ConnectorIntent::SaveServer(server_draft("server-1")))
            .unwrap();
        wait_until(|| fixture.coordinator.current_snapshot().config_revision.0 == 2);
        mcp_gate.release();
        wait_until(|| fixture.coordinator.metrics().active_mcp_operations == 0);
        assert_eq!(fixture.coordinator.metrics().stale_results, 1);
        assert_eq!(fixture.repo.replacements.load(Ordering::Acquire), 0);
    }

    fn invoke_intent(server_id: &str, tool_id: &str, input: &[u8]) -> ConnectorIntent {
        ConnectorIntent::InvokeTool {
            server_id: ServerId::new(server_id),
            tool_id: ToolId::new(tool_id),
            arguments_json: SensitiveInput::new(input.to_vec()),
        }
    }

    fn enable_auto_allow(fixture: &Fixture) {
        *fixture.repo.permission.lock().expect("permission lock") =
            audit::PermissionFingerprint::Persisted {
                rule: audit::PermissionRule::Allow,
                approved_schema_hash: Some(audit::schema_hash(r#"{"type":"object"}"#)),
            };
    }

    fn first_called_operation(fixture: &Fixture) -> String {
        fixture
            .mcp
            .call_operation_ids
            .lock()
            .expect("call ids lock")
            .first()
            .expect("one call operation")
            .clone()
    }

    #[test]
    fn malformed_scalar_and_array_inputs_stop_before_all_authorization_work() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        for input in [b"{".as_slice(), b"42".as_slice(), b"[]".as_slice()] {
            let previous = fixture.coordinator.current_snapshot().revision.0;
            fixture
                .coordinator
                .dispatch(invoke_intent("server-1", "tool-a", input))
                .unwrap();
            wait_until(|| fixture.coordinator.current_snapshot().revision.0 > previous);
            assert_eq!(
                fixture
                    .coordinator
                    .current_snapshot()
                    .diagnostics
                    .transitions
                    .last()
                    .and_then(|transition| transition.error_code),
                Some(ErrorCode::InvalidInput)
            );
        }
        assert_eq!(fixture.repo.server_loads.load(Ordering::Acquire), 0);
        assert_eq!(fixture.mcp.schema_calls.load(Ordering::Acquire), 0);
        assert_eq!(fixture.repo.authorization_loads.load(Ordering::Acquire), 0);
        assert_eq!(fixture.repo.preflights.load(Ordering::Acquire), 0);
        assert_eq!(fixture.mcp.invoke_calls.load(Ordering::Acquire), 0);
        assert_eq!(fixture.coordinator.metrics().active_mcp_operations, 0);
    }

    #[test]
    fn authorized_success_uses_exact_order_operation_and_durable_outcome() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        enable_auto_allow(&fixture);

        fixture
            .coordinator
            .dispatch(invoke_intent("server-1", "tool-a", br#"{"value":1}"#))
            .unwrap();
        wait_until(|| fixture.repo.completions.load(Ordering::Acquire) == 1);
        wait_until(|| fixture.coordinator.metrics().active_mcp_operations == 0);
        let operation_id = first_called_operation(&fixture);

        assert_eq!(fixture.mcp.invoke_calls.load(Ordering::Acquire), 1);
        assert_eq!(
            fixture.repo.order.lock().expect("order lock").as_slice(),
            ["schema", "permission", "preflight", "call", "outcome"]
        );
        assert_eq!(
            fixture
                .repo
                .authorization_ledger
                .lifecycle(&operation_id)
                .unwrap(),
            Some(audit::AuditLifecycle::Succeeded)
        );
        assert_eq!(
            fixture
                .coordinator
                .current_snapshot()
                .result
                .as_ref()
                .map(|result| result.operation_id.as_str()),
            Some(operation_id.as_str())
        );
    }

    #[test]
    fn durable_denial_never_reaches_external_call() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        *fixture.repo.permission.lock().expect("permission lock") =
            audit::PermissionFingerprint::Persisted {
                rule: audit::PermissionRule::Deny,
                approved_schema_hash: None,
            };

        fixture
            .coordinator
            .dispatch(invoke_intent("server-1", "tool-a", br#"{"value":1}"#))
            .unwrap();
        wait_until(|| fixture.repo.preflights.load(Ordering::Acquire) == 1);
        wait_until(|| fixture.coordinator.metrics().active_mcp_operations == 0);
        let operation_id = fixture
            .coordinator
            .current_snapshot()
            .result
            .as_ref()
            .expect("denied result")
            .operation_id
            .clone();

        assert_eq!(fixture.repo.preflights.load(Ordering::Acquire), 1);
        assert_eq!(fixture.repo.completions.load(Ordering::Acquire), 0);
        assert_eq!(fixture.mcp.invoke_calls.load(Ordering::Acquire), 0);
        assert_eq!(
            fixture
                .repo
                .authorization_ledger
                .lifecycle(operation_id.as_str())
                .unwrap(),
            Some(audit::AuditLifecycle::Denied)
        );
    }

    #[test]
    fn known_tool_error_is_failed_and_backend_is_not_retried() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        enable_auto_allow(&fixture);
        *fixture.mcp.invoke_error.lock().expect("invoke error lock") = Some(ServiceError::new(
            ErrorCode::TransportFailed,
            "known tool error",
        ));

        fixture
            .coordinator
            .dispatch(invoke_intent("server-1", "tool-a", br#"{"value":1}"#))
            .unwrap();
        wait_until(|| fixture.repo.completions.load(Ordering::Acquire) == 1);
        let operation_id = first_called_operation(&fixture);

        assert_eq!(fixture.mcp.invoke_calls.load(Ordering::Acquire), 1);
        assert_eq!(
            fixture
                .repo
                .authorization_ledger
                .lifecycle(&operation_id)
                .unwrap(),
            Some(audit::AuditLifecycle::Failed)
        );
    }

    #[test]
    fn unknown_delivery_is_durable_unknown_and_backend_is_not_retried() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        enable_auto_allow(&fixture);
        *fixture.mcp.invoke_error.lock().expect("invoke error lock") = Some(ServiceError::new(
            ErrorCode::UnknownDelivery,
            "delivery unknown",
        ));

        fixture
            .coordinator
            .dispatch(invoke_intent("server-1", "tool-a", br#"{"value":1}"#))
            .unwrap();
        wait_until(|| fixture.repo.completions.load(Ordering::Acquire) == 1);
        let operation_id = first_called_operation(&fixture);

        assert_eq!(fixture.mcp.invoke_calls.load(Ordering::Acquire), 1);
        assert_eq!(
            fixture
                .repo
                .authorization_ledger
                .lifecycle(&operation_id)
                .unwrap(),
            Some(audit::AuditLifecycle::Unknown)
        );
        assert_eq!(
            fixture
                .coordinator
                .current_snapshot()
                .diagnostics
                .transitions
                .last()
                .and_then(|transition| transition.error_code),
            Some(ErrorCode::UnknownDelivery)
        );
    }

    #[test]
    fn exact_input_grant_mismatch_is_failed_before_external_call() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        enable_auto_allow(&fixture);
        *fixture
            .repo
            .preflight_input_override
            .lock()
            .expect("preflight input override lock") = Some(br#"{"value":2}"#.to_vec());

        fixture
            .coordinator
            .dispatch(invoke_intent("server-1", "tool-a", br#"{"value":1}"#))
            .unwrap();
        wait_until(|| fixture.repo.completions.load(Ordering::Acquire) == 1);
        let operation_id = fixture
            .coordinator
            .current_snapshot()
            .result
            .as_ref()
            .expect("binding mismatch result")
            .operation_id
            .clone();

        assert_eq!(fixture.repo.preflights.load(Ordering::Acquire), 1);
        assert_eq!(fixture.mcp.invoke_calls.load(Ordering::Acquire), 0);
        assert_eq!(
            fixture
                .repo
                .authorization_ledger
                .lifecycle(operation_id.as_str())
                .unwrap(),
            Some(audit::AuditLifecycle::Failed)
        );
    }

    #[test]
    fn mismatched_live_tool_target_stops_before_permission_preflight_and_call() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        enable_auto_allow(&fixture);
        *fixture
            .mcp
            .schema_tool_override
            .lock()
            .expect("schema tool lock") = Some(ToolId::new("different-tool"));

        fixture
            .coordinator
            .dispatch(invoke_intent("server-1", "tool-a", br#"{"value":1}"#))
            .unwrap();
        wait_until(|| fixture.coordinator.metrics().active_mcp_operations == 0);

        assert_eq!(fixture.repo.authorization_loads.load(Ordering::Acquire), 0);
        assert_eq!(fixture.repo.preflights.load(Ordering::Acquire), 0);
        assert_eq!(fixture.mcp.invoke_calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn repeated_outcome_failure_retains_one_slot_and_fails_closed_until_recovery() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        enable_auto_allow(&fixture);
        fixture.repo.outcome_failure.store(true, Ordering::Release);

        fixture
            .coordinator
            .dispatch(invoke_intent("server-1", "tool-a", br#"{"value":1}"#))
            .unwrap();
        wait_until(|| fixture.repo.completions.load(Ordering::Acquire) == 2);
        wait_until(|| fixture.coordinator.metrics().active_mcp_operations == 1);
        let operation_id = first_called_operation(&fixture);
        assert_eq!(fixture.mcp.invoke_calls.load(Ordering::Acquire), 1);
        assert_eq!(
            fixture
                .repo
                .authorization_ledger
                .lifecycle(&operation_id)
                .unwrap(),
            Some(audit::AuditLifecycle::Prepared)
        );

        fixture
            .coordinator
            .dispatch(invoke_intent("server-2", "tool-b", br#"{"value":2}"#))
            .unwrap();
        wait_until(|| fixture.repo.completions.load(Ordering::Acquire) == 3);
        assert_eq!(fixture.mcp.invoke_calls.load(Ordering::Acquire), 1);
        assert_eq!(fixture.coordinator.metrics().active_mcp_operations, 1);

        fixture.repo.outcome_failure.store(false, Ordering::Release);
        fixture
            .coordinator
            .dispatch(ConnectorIntent::DismissResult(OperationId::new(
                "retry-audit",
            )))
            .unwrap();
        wait_until(|| fixture.coordinator.metrics().active_mcp_operations == 0);
        assert_eq!(fixture.mcp.invoke_calls.load(Ordering::Acquire), 1);
        assert_eq!(
            fixture
                .repo
                .authorization_ledger
                .lifecycle(&operation_id)
                .unwrap(),
            Some(audit::AuditLifecycle::Succeeded)
        );
    }

    #[test]
    fn dispatched_cancel_is_visible_before_blocked_schema_completion() {
        let schema_gate = Arc::new(Gate::closed());
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::clone(&schema_gate),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        enable_auto_allow(&fixture);
        fixture
            .coordinator
            .dispatch(invoke_intent("server-1", "tool-a", br#"{"value":1}"#))
            .unwrap();
        wait_until(|| fixture.mcp.schema_calls.load(Ordering::Acquire) == 1);
        let operation_id = fixture.coordinator.current_snapshot().operations[0]
            .id
            .clone();
        fixture
            .coordinator
            .dispatch(ConnectorIntent::Cancel(operation_id))
            .unwrap();
        schema_gate.release();
        wait_until(|| fixture.coordinator.metrics().active_mcp_operations == 0);

        assert_eq!(fixture.repo.authorization_loads.load(Ordering::Acquire), 0);
        assert_eq!(fixture.repo.preflights.load(Ordering::Acquire), 0);
        assert_eq!(fixture.mcp.invoke_calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn dispatched_config_mutation_is_visible_before_blocked_schema_completion() {
        let schema_gate = Arc::new(Gate::closed());
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::clone(&schema_gate),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        enable_auto_allow(&fixture);
        fixture
            .coordinator
            .dispatch(invoke_intent("server-1", "tool-a", br#"{"value":1}"#))
            .unwrap();
        wait_until(|| fixture.mcp.schema_calls.load(Ordering::Acquire) == 1);
        fixture
            .coordinator
            .dispatch(ConnectorIntent::SaveServer(server_draft("server-1")))
            .unwrap();
        schema_gate.release();
        wait_until(|| fixture.coordinator.metrics().active_mcp_operations == 0);

        assert_eq!(fixture.repo.authorization_loads.load(Ordering::Acquire), 0);
        assert_eq!(fixture.repo.preflights.load(Ordering::Acquire), 0);
        assert_eq!(fixture.mcp.invoke_calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn mismatched_repository_server_stops_before_schema_authorization_and_call() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        *fixture
            .repo
            .loaded_server_override
            .lock()
            .expect("server override lock") = Some(server_draft("server-b"));

        fixture
            .coordinator
            .dispatch(invoke_intent("server-a", "tool-a", br#"{"value":1}"#))
            .unwrap();
        wait_until(|| fixture.coordinator.current_snapshot().result.is_some());

        assert_eq!(fixture.repo.server_loads.load(Ordering::Acquire), 1);
        assert_eq!(fixture.mcp.schema_calls.load(Ordering::Acquire), 0);
        assert_eq!(fixture.repo.authorization_loads.load(Ordering::Acquire), 0);
        assert_eq!(fixture.repo.preflights.load(Ordering::Acquire), 0);
        assert_eq!(fixture.mcp.invoke_calls.load(Ordering::Acquire), 0);
        assert_eq!(fixture.coordinator.metrics().active_mcp_operations, 0);
        assert_eq!(
            fixture
                .coordinator
                .current_snapshot()
                .diagnostics
                .transitions
                .last()
                .and_then(|transition| transition.error_code),
            Some(ErrorCode::StorageUnavailable)
        );
    }

    #[test]
    fn invalid_http_url_stops_before_persistence_schema_and_call() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        let invalid = ServerDraft {
            id: Some(ServerId::new("server-a")),
            name: "server-a".to_owned(),
            transport: TransportDraft::Http {
                url: "http://example.com/mcp".to_owned(),
            },
            enabled: true,
        };
        let revision = fixture.repo.config_revision.load(Ordering::Acquire);
        fixture
            .coordinator
            .dispatch(ConnectorIntent::SaveServer(invalid.clone()))
            .unwrap();
        wait_until(|| fixture.coordinator.current_snapshot().result.is_some());
        assert_eq!(
            fixture.repo.config_revision.load(Ordering::Acquire),
            revision
        );

        *fixture
            .repo
            .loaded_server_override
            .lock()
            .expect("server override lock") = Some(invalid);
        fixture
            .coordinator
            .dispatch(invoke_intent("server-a", "tool-a", br#"{"value":1}"#))
            .unwrap();
        wait_until(|| fixture.repo.server_loads.load(Ordering::Acquire) == 1);
        assert_eq!(fixture.mcp.schema_calls.load(Ordering::Acquire), 0);
        assert_eq!(fixture.repo.preflights.load(Ordering::Acquire), 0);
        assert_eq!(fixture.mcp.invoke_calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn empty_stdio_command_stops_before_persistence_schema_and_call() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        let invalid = ServerDraft {
            id: Some(ServerId::new("server-a")),
            name: "server-a".to_owned(),
            transport: TransportDraft::Stdio {
                command: " ".to_owned(),
                args: Vec::new(),
                plain_env: Vec::new(),
                secret_env: Vec::new(),
                inherit_env: false,
            },
            enabled: true,
        };
        let revision = fixture.repo.config_revision.load(Ordering::Acquire);
        fixture
            .coordinator
            .dispatch(ConnectorIntent::SaveServer(invalid.clone()))
            .unwrap();
        wait_until(|| fixture.coordinator.current_snapshot().result.is_some());
        assert_eq!(
            fixture.repo.config_revision.load(Ordering::Acquire),
            revision
        );

        *fixture
            .repo
            .loaded_server_override
            .lock()
            .expect("server override lock") = Some(invalid);
        fixture
            .coordinator
            .dispatch(invoke_intent("server-a", "tool-a", br#"{"value":1}"#))
            .unwrap();
        wait_until(|| fixture.repo.server_loads.load(Ordering::Acquire) == 1);
        assert_eq!(fixture.mcp.schema_calls.load(Ordering::Acquire), 0);
        assert_eq!(fixture.repo.preflights.load(Ordering::Acquire), 0);
        assert_eq!(fixture.mcp.invoke_calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn pending_approvals_retain_exactly_two_slots_and_cancel_drops_them() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        fixture
            .coordinator
            .dispatch(invoke_intent("server-1", "tool-a", br#"{"a":1}"#))
            .unwrap();
        fixture
            .coordinator
            .dispatch(invoke_intent("server-2", "tool-b", br#"{"b":2}"#))
            .unwrap();
        wait_until(|| fixture.coordinator.current_snapshot().operations.len() == 2);
        wait_until(|| fixture.repo.authorization_loads.load(Ordering::Acquire) == 2);
        assert_eq!(fixture.coordinator.metrics().active_mcp_operations, 2);
        assert!(fixture.coordinator.current_snapshot().approval.is_some());

        fixture
            .coordinator
            .dispatch(invoke_intent("server-3", "tool-c", br#"{"c":3}"#))
            .unwrap();
        wait_until(|| fixture.coordinator.metrics().backpressure_rejections >= 1);
        assert_eq!(fixture.repo.server_loads.load(Ordering::Acquire), 2);
        assert_eq!(fixture.mcp.schema_calls.load(Ordering::Acquire), 2);

        let first = fixture
            .coordinator
            .current_snapshot()
            .approval
            .as_ref()
            .unwrap()
            .operation_id
            .clone();
        fixture
            .coordinator
            .dispatch(ConnectorIntent::Cancel(first))
            .unwrap();
        wait_until(|| fixture.coordinator.metrics().active_mcp_operations == 1);
        let second = fixture
            .coordinator
            .current_snapshot()
            .approval
            .as_ref()
            .unwrap()
            .operation_id
            .clone();
        fixture
            .coordinator
            .dispatch(ConnectorIntent::Cancel(second))
            .unwrap();
        wait_until(|| fixture.coordinator.metrics().active_mcp_operations == 0);
        assert!(fixture.coordinator.current_snapshot().approval.is_none());
        assert!(fixture.coordinator.current_snapshot().operations.is_empty());
        assert_eq!(fixture.repo.preflights.load(Ordering::Acquire), 0);
        assert_eq!(fixture.mcp.invoke_calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn pending_approval_idle_ttl_denies_without_call_and_releases_pinned_lease() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(FakeClock::default()),
        );
        fixture
            .coordinator
            .dispatch(invoke_intent("server-1", "tool-a", br#"{"a":1}"#))
            .unwrap();

        wait_until(|| fixture.mcp.schema_calls.load(Ordering::Acquire) == 1);
        wait_until(|| fixture.coordinator.metrics().worker_alive == 0);

        let operation_id = fixture
            .coordinator
            .current_snapshot()
            .result
            .as_ref()
            .expect("idle-expired approval result")
            .operation_id
            .clone();
        assert_eq!(fixture.repo.preflights.load(Ordering::Acquire), 1);
        assert_eq!(fixture.mcp.invoke_calls.load(Ordering::Acquire), 0);
        assert_eq!(fixture.mcp.active_leases(), 0);
        assert_eq!(fixture.coordinator.metrics().active_mcp_operations, 0);
        assert!(fixture.coordinator.current_snapshot().operations.is_empty());
        assert!(fixture.coordinator.current_snapshot().approval.is_none());
        assert_eq!(
            fixture
                .repo
                .authorization_ledger
                .lifecycle(operation_id.as_str())
                .unwrap(),
            Some(audit::AuditLifecycle::Denied)
        );
    }

    #[test]
    fn approval_preflight_failure_and_double_resolve_never_call() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        fixture
            .coordinator
            .dispatch(invoke_intent("server-1", "tool-a", br#"{"a":1}"#))
            .unwrap();
        wait_until(|| fixture.coordinator.current_snapshot().approval.is_some());
        fixture.repo.authorization_ledger.fail_next_preflight();
        let operation_id = fixture
            .coordinator
            .current_snapshot()
            .approval
            .as_ref()
            .unwrap()
            .operation_id
            .clone();
        fixture
            .coordinator
            .dispatch(ConnectorIntent::ResolveApproval {
                operation_id: operation_id.clone(),
                decision: ApprovalDecision::AllowOnce,
            })
            .unwrap();
        fixture
            .coordinator
            .dispatch(ConnectorIntent::ResolveApproval {
                operation_id,
                decision: ApprovalDecision::AllowOnce,
            })
            .unwrap();
        wait_until(|| fixture.coordinator.metrics().active_mcp_operations == 0);
        assert_eq!(fixture.repo.preflights.load(Ordering::Acquire), 1);
        assert_eq!(fixture.mcp.invoke_calls.load(Ordering::Acquire), 0);
        assert!(fixture.coordinator.current_snapshot().approval.is_none());
    }

    #[test]
    fn absent_to_persisted_ask_mutation_fails_preflight_before_external_call() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        assert_eq!(
            *fixture.repo.permission.lock().expect("permission lock"),
            audit::PermissionFingerprint::Absent
        );

        fixture
            .coordinator
            .dispatch(invoke_intent("server-1", "tool-a", br#"{"a":1}"#))
            .unwrap();
        wait_until(|| fixture.coordinator.current_snapshot().approval.is_some());
        let operation_id = fixture
            .coordinator
            .current_snapshot()
            .approval
            .as_ref()
            .expect("approval prompt")
            .operation_id
            .clone();
        *fixture
            .repo
            .permission_mutation_on_preflight
            .lock()
            .expect("permission mutation lock") = Some(audit::PermissionFingerprint::Persisted {
            rule: audit::PermissionRule::Ask,
            approved_schema_hash: None,
        });

        fixture
            .coordinator
            .dispatch(ConnectorIntent::ResolveApproval {
                operation_id: operation_id.clone(),
                decision: ApprovalDecision::AllowOnce,
            })
            .unwrap();
        wait_until(|| fixture.coordinator.metrics().active_mcp_operations == 0);

        assert_eq!(fixture.repo.preflights.load(Ordering::Acquire), 1);
        assert_eq!(fixture.mcp.invoke_calls.load(Ordering::Acquire), 0);
        assert_eq!(
            fixture
                .repo
                .authorization_ledger
                .lifecycle(operation_id.as_str())
                .unwrap(),
            None
        );
        assert_eq!(
            *fixture.repo.permission.lock().expect("permission lock"),
            audit::PermissionFingerprint::Persisted {
                rule: audit::PermissionRule::Ask,
                approved_schema_hash: None,
            }
        );
        assert_eq!(
            fixture
                .coordinator
                .current_snapshot()
                .diagnostics
                .transitions
                .last()
                .and_then(|transition| transition.error_code),
            Some(ErrorCode::AuditUnavailable)
        );
    }

    #[test]
    fn config_change_immediately_drops_pending_without_audit_or_call() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        fixture
            .coordinator
            .dispatch(invoke_intent("server-1", "tool-a", br#"{"a":1}"#))
            .unwrap();
        wait_until(|| fixture.coordinator.current_snapshot().approval.is_some());
        fixture
            .coordinator
            .dispatch(ConnectorIntent::SaveServer(server_draft("server-1")))
            .unwrap();
        wait_until(|| fixture.coordinator.current_snapshot().config_revision.0 == 2);
        wait_until(|| fixture.coordinator.metrics().active_mcp_operations == 0);
        assert!(fixture.coordinator.current_snapshot().approval.is_none());
        assert!(fixture.coordinator.current_snapshot().operations.is_empty());
        assert_eq!(fixture.repo.preflights.load(Ordering::Acquire), 0);
        assert_eq!(fixture.mcp.invoke_calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn system_operation_ids_are_unique_across_coordinator_instances() {
        let first = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        let second = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        first
            .coordinator
            .dispatch(invoke_intent("server-1", "tool-a", b"[]"))
            .unwrap();
        second
            .coordinator
            .dispatch(invoke_intent("server-1", "tool-a", b"[]"))
            .unwrap();
        wait_until(|| first.coordinator.current_snapshot().result.is_some());
        wait_until(|| second.coordinator.current_snapshot().result.is_some());
        let first_id = first
            .coordinator
            .current_snapshot()
            .result
            .as_ref()
            .unwrap()
            .operation_id
            .clone();
        let second_id = second
            .coordinator
            .current_snapshot()
            .result
            .as_ref()
            .unwrap()
            .operation_id
            .clone();
        assert_ne!(first_id, second_id);
        assert!(first_id.as_str().len() <= 128);
        assert!(second_id.as_str().len() <= 128);
    }

    #[test]
    fn cancelled_job_holds_capacity_until_backend_exits_then_is_removed() {
        let mcp_gate = Arc::new(Gate::closed());
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::clone(&mcp_gate),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        fixture.mcp.cancel_releases.store(false, Ordering::Release);
        fixture
            .coordinator
            .dispatch(ConnectorIntent::Activate)
            .unwrap();
        wait_until(|| fixture.coordinator.current_snapshot().config_revision.0 == 1);
        fixture
            .coordinator
            .dispatch(ConnectorIntent::Discover(ServerId::new("server-1")))
            .unwrap();
        fixture
            .coordinator
            .dispatch(ConnectorIntent::Discover(ServerId::new("server-2")))
            .unwrap();
        wait_until(|| fixture.coordinator.current_snapshot().operations.len() == 2);
        let cancelled = fixture.coordinator.current_snapshot().operations[0]
            .id
            .clone();
        fixture
            .coordinator
            .dispatch(ConnectorIntent::Cancel(cancelled))
            .unwrap();
        wait_until(|| {
            fixture
                .coordinator
                .current_snapshot()
                .diagnostics
                .cancellations
                == 1
        });
        assert_eq!(fixture.coordinator.metrics().active_mcp_operations, 2);

        fixture
            .coordinator
            .dispatch(ConnectorIntent::Discover(ServerId::new("server-3")))
            .unwrap();
        wait_until(|| fixture.coordinator.metrics().backpressure_rejections >= 1);
        assert_eq!(fixture.mcp.peak.load(Ordering::Acquire), 2);

        mcp_gate.release();
        wait_until(|| fixture.coordinator.metrics().active_mcp_operations == 0);
        wait_until(|| fixture.coordinator.current_snapshot().operations.is_empty());
    }

    #[test]
    fn backend_panic_completes_and_releases_operation_capacity() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        fixture.mcp.panic_on_discover.store(true, Ordering::Release);
        fixture
            .coordinator
            .dispatch(ConnectorIntent::Activate)
            .unwrap();
        wait_until(|| fixture.coordinator.current_snapshot().config_revision.0 == 1);
        fixture
            .coordinator
            .dispatch(ConnectorIntent::Discover(ServerId::new("server-1")))
            .unwrap();
        wait_until(|| {
            fixture
                .coordinator
                .current_snapshot()
                .diagnostics
                .transitions
                .iter()
                .any(|transition| transition.error_code == Some(ErrorCode::Internal))
        });
        assert_eq!(fixture.coordinator.metrics().active_mcp_operations, 0);
        assert!(fixture.coordinator.current_snapshot().operations.is_empty());
    }

    #[test]
    fn generation_entries_are_pruned_after_server_churn() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        fixture
            .coordinator
            .dispatch(ConnectorIntent::Activate)
            .unwrap();
        wait_until(|| fixture.coordinator.current_snapshot().config_revision.0 == 1);
        for index in 0..32 {
            let previous = fixture.coordinator.current_snapshot().config_revision.0;
            fixture
                .coordinator
                .dispatch(ConnectorIntent::DeleteServer(ServerId::new(format!(
                    "deleted-{index}"
                ))))
                .unwrap();
            wait_until(|| fixture.coordinator.current_snapshot().config_revision.0 > previous);
            assert!(fixture.coordinator.metrics().generation_entries <= 2);
        }
    }

    #[test]
    fn fake_clock_expires_idle_worker_without_sleep_or_polling() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(FakeClock::default()),
        );
        fixture
            .coordinator
            .dispatch(ConnectorIntent::Activate)
            .unwrap();
        wait_until(|| fixture.coordinator.metrics().worker_starts == 1);
        wait_until(|| fixture.coordinator.metrics().worker_alive == 0);
        assert_eq!(fixture.repo.opens.load(Ordering::Acquire), 1);
        assert_eq!(fixture.coordinator.metrics().active_mcp_operations, 0);
        assert_eq!(fixture.coordinator.metrics().active_oauth_flows, 0);
    }

    #[test]
    fn idle_deadline_reaps_backend_lease_before_exit_without_second_ttl() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(FakeClock::default()),
        );
        fixture.mcp.leases.store(1, Ordering::Release);
        fixture
            .coordinator
            .dispatch(ConnectorIntent::Activate)
            .unwrap();

        wait_until(|| fixture.coordinator.metrics().worker_starts == 1);
        wait_until(|| fixture.coordinator.metrics().worker_alive == 0);
        assert_eq!(fixture.mcp.reap_calls.load(Ordering::Acquire), 1);
        assert_eq!(fixture.mcp.leases.load(Ordering::Acquire), 0);
        assert_eq!(fixture.repo.opens.load(Ordering::Acquire), 1);
        assert_eq!(fixture.repo.shutdowns.load(Ordering::Acquire), 1);
    }

    #[test]
    fn idle_restart_resumes_latest_snapshot_and_opens_one_new_repository() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(FakeClock::default()),
        );
        fixture
            .coordinator
            .dispatch(ConnectorIntent::Activate)
            .unwrap();
        wait_until(|| fixture.coordinator.metrics().worker_starts == 1);
        wait_until(|| fixture.coordinator.metrics().worker_alive == 0);
        let first_revision = fixture.coordinator.current_snapshot().revision.0;
        assert_eq!(fixture.repo.opens.load(Ordering::Acquire), 1);

        fixture
            .coordinator
            .dispatch(ConnectorIntent::DeleteServer(ServerId::new(
                "restart-change",
            )))
            .unwrap();
        wait_until(|| fixture.coordinator.metrics().worker_starts == 2);
        wait_until(|| fixture.repo.config_revision.load(Ordering::Acquire) == 2);
        wait_until(|| fixture.coordinator.metrics().worker_alive == 0);
        assert_eq!(fixture.coordinator.metrics().worker_starts, 2);
        assert_eq!(fixture.repo.opens.load(Ordering::Acquire), 2);
        let resumed = fixture.coordinator.current_snapshot();
        assert!(resumed.revision.0 > first_revision);
        assert_eq!(resumed.config_revision, Revision(2));
    }

    #[test]
    fn import_bytes_and_server_count_are_bounded_before_persistence() {
        let fixture = fixture(
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(Gate::opened()),
            Arc::new(SystemCoordinatorClock::default()),
        );
        fixture
            .coordinator
            .dispatch(ConnectorIntent::ImportConfiguration {
                source: ImportSource::File,
                display_name: Some("too-large.json".to_owned()),
                contents: SensitiveInput::new(vec![
                    0;
                    ResourceLimits::PRODUCTION_CEILING
                        .import_input_bytes
                        + 1
                ]),
            })
            .unwrap();
        wait_until(|| {
            fixture
                .coordinator
                .current_snapshot()
                .diagnostics
                .transitions
                .iter()
                .any(|transition| transition.error_code == Some(ErrorCode::LimitExceeded))
        });
        assert_eq!(fixture.repo.parses.load(Ordering::Acquire), 0);
        assert_eq!(fixture.repo.imports.load(Ordering::Acquire), 0);

        fixture
            .repo
            .import_server_count
            .store(257, Ordering::Release);
        fixture
            .coordinator
            .dispatch(ConnectorIntent::ImportConfiguration {
                source: ImportSource::File,
                display_name: Some("many.json".to_owned()),
                contents: SensitiveInput::new(b"fixture".to_vec()),
            })
            .unwrap();
        wait_until(|| fixture.repo.parses.load(Ordering::Acquire) == 1);
        wait_until(|| fixture.coordinator.current_snapshot().revision.0 >= 2);
        assert_eq!(fixture.repo.imports.load(Ordering::Acquire), 0);
    }

    #[test]
    fn discovery_item_and_descriptor_byte_ceilings_are_inclusive() {
        let limits = ResourceLimits::default();
        let tool = |index, descriptor_bytes| crate::DiscoveredTool {
            id: ToolId::new(format!("tool-{index}")),
            name: format!("tool-{index}"),
            description: None,
            descriptor_bytes,
        };
        let exact_items = DiscoverOutput {
            tools: (0..limits.tools_per_server)
                .map(|index| tool(index, 0))
                .collect(),
        };
        assert!(validate_discovered_tools(&exact_items, limits).is_ok());
        let over_items = DiscoverOutput {
            tools: (0..=limits.tools_per_server)
                .map(|index| tool(index, 0))
                .collect(),
        };
        assert_eq!(
            validate_discovered_tools(&over_items, limits)
                .unwrap_err()
                .code,
            ErrorCode::LimitExceeded
        );
        assert!(
            validate_discovered_tools(
                &DiscoverOutput {
                    tools: vec![tool(0, limits.tool_descriptor_bytes)],
                },
                limits,
            )
            .is_ok()
        );
        assert_eq!(
            validate_discovered_tools(
                &DiscoverOutput {
                    tools: vec![tool(0, limits.tool_descriptor_bytes + 1)],
                },
                limits,
            )
            .unwrap_err()
            .code,
            ErrorCode::LimitExceeded
        );
    }
}
