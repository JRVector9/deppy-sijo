use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

use connector_contract::{
    ConnectorIntent, ConnectorSnapshot, DiagnosticTransition, DiagnosticsSnapshot, ErrorCode,
    OperationId, OperationKind, OperationPhase, OperationResult, OperationSummary, ResourceLimits,
    Revision, ServerDraft, ServerId, ToolPage, TransportDraft,
};

use crate::ports::{
    CancellationToken, ConnectorMcp, ConnectorOAuth, ConnectorRepository,
    ConnectorRepositoryFactory, ConnectorSecrets, DiscoverOutput, InvokeRequest,
    McpTransportSnapshot, OAuthOutput, ServiceError, StoredOAuthClient,
};
use crate::snapshot::{SnapshotCell, SnapshotReader};

const TOOL_PAGE_SIZE: usize = 256;

pub trait CoordinatorClock: Send + Sync + 'static {
    fn now(&self) -> Duration;

    /// Production returns `requested`. Tests may advance a fake monotonic clock and
    /// return zero so idle expiry is deterministic without sleeping.
    fn wait_duration(&self, requested: Duration) -> Duration {
        requested
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
    pub clock: Arc<dyn CoordinatorClock>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppRequest {
    PickImportFile,
    OpenExternalUrl(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchOutcome {
    Queued,
    AppRequest(AppRequest),
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
    sender: SyncSender<ConnectorIntent>,
    thread: JoinHandle<()>,
    unparker: Thread,
}

struct Inner {
    config: ConnectorCoordinatorConfig,
    snapshot: Arc<SnapshotCell>,
    metrics: Arc<Metrics>,
    worker: Mutex<Option<WorkerSlot>>,
    next_operation: Arc<AtomicU64>,
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
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                snapshot: Arc::new(SnapshotCell::new()),
                metrics: Arc::new(Metrics::new()),
                worker: Mutex::new(None),
                next_operation: Arc::new(AtomicU64::new(1)),
            }),
        })
    }

    pub fn snapshot_reader(&self) -> SnapshotReader {
        SnapshotReader::new(Arc::clone(&self.inner.snapshot))
    }

    pub fn current_snapshot(&self) -> Arc<ConnectorSnapshot> {
        self.inner.snapshot.current()
    }

    pub fn dispatch(&self, intent: ConnectorIntent) -> Result<DispatchOutcome, DispatchError> {
        match intent {
            ConnectorIntent::RequestImportPicker => {
                return Ok(DispatchOutcome::AppRequest(AppRequest::PickImportFile));
            }
            ConnectorIntent::OpenExternalUrl { url } => {
                return Ok(DispatchOutcome::AppRequest(AppRequest::OpenExternalUrl(
                    url,
                )));
            }
            intent => self.enqueue(intent)?,
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

    fn enqueue(&self, mut intent: ConnectorIntent) -> Result<(), DispatchError> {
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
            match slot.sender.try_send(intent) {
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
                    intent = returned;
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
        let mcp = Arc::clone(&self.inner.config.mcp);
        let oauth = Arc::clone(&self.inner.config.oauth);
        let clock = Arc::clone(&self.inner.config.clock);
        let limits = self.inner.config.limits;
        let idle_ttl = self.inner.config.idle_ttl;
        let next_operation = Arc::clone(&self.inner.next_operation);
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
                        let seed = next_operation.fetch_add(1, Ordering::Relaxed);
                        publish_boot_error(&snapshot, error, seed);
                        return;
                    }
                };
                Worker::new(
                    receiver,
                    snapshot,
                    metrics,
                    repository,
                    mcp,
                    oauth,
                    clock,
                    limits,
                    idle_ttl,
                    next_operation,
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

fn publish_boot_error(snapshot: &SnapshotCell, error: ServiceError, seed: u64) {
    let mut next = ConnectorSnapshot {
        result: Some(OperationResult {
            operation_id: OperationId::new(format!("connector-{seed}")),
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
    Invoke(Result<String, ServiceError>),
    OAuth(Result<OAuthOutput, ServiceError>),
}

struct JobCompletion {
    operation_id: OperationId,
    server_id: ServerId,
    generation: u64,
    config_revision: Revision,
    kind: OperationKind,
    payload: JobPayload,
}

struct ActiveJob {
    server_id: ServerId,
    generation: u64,
    config_revision: Revision,
    kind: OperationKind,
    cancellation: CancellationToken,
    thread: Option<JoinHandle<()>>,
}

struct Worker {
    commands: Receiver<ConnectorIntent>,
    result_sender: SyncSender<JobCompletion>,
    results: Receiver<JobCompletion>,
    snapshot_cell: Arc<SnapshotCell>,
    snapshot: ConnectorSnapshot,
    metrics: Arc<Metrics>,
    repository: Box<dyn ConnectorRepository>,
    mcp: Arc<dyn ConnectorMcp>,
    oauth: Arc<dyn ConnectorOAuth>,
    clock: Arc<dyn CoordinatorClock>,
    limits: ResourceLimits,
    idle_ttl: Duration,
    last_activity: Duration,
    next_operation: Arc<AtomicU64>,
    generations: HashMap<ServerId, u64>,
    jobs: HashMap<OperationId, ActiveJob>,
    operations: Vec<OperationSummary>,
    transitions: VecDeque<DiagnosticTransition>,
    needs_initial_overview: bool,
    shutting_down: bool,
}

impl Worker {
    #[allow(clippy::too_many_arguments)]
    fn new(
        commands: Receiver<ConnectorIntent>,
        snapshot_cell: Arc<SnapshotCell>,
        metrics: Arc<Metrics>,
        repository: Box<dyn ConnectorRepository>,
        mcp: Arc<dyn ConnectorMcp>,
        oauth: Arc<dyn ConnectorOAuth>,
        clock: Arc<dyn CoordinatorClock>,
        limits: ResourceLimits,
        idle_ttl: Duration,
        next_operation: Arc<AtomicU64>,
    ) -> Self {
        let (result_sender, results) = sync_channel(limits.mcp_operations + limits.oauth_flows);
        let last_activity = clock.now();
        let snapshot = (*snapshot_cell.current()).clone();
        let needs_initial_overview = snapshot.revision == Revision::ZERO;
        Self {
            commands,
            result_sender,
            results,
            snapshot_cell,
            snapshot,
            metrics,
            repository,
            mcp,
            oauth,
            clock,
            limits,
            idle_ttl,
            last_activity,
            next_operation,
            generations: HashMap::new(),
            jobs: HashMap::new(),
            operations: Vec::new(),
            transitions: VecDeque::new(),
            needs_initial_overview,
            shutting_down: false,
        }
    }

    fn run(mut self) {
        loop {
            let mut progressed = self.drain_results();
            progressed |= self.drain_commands();
            if self.shutting_down && self.jobs.is_empty() {
                break;
            }
            if progressed {
                continue;
            }
            if self.jobs.is_empty() {
                let elapsed = self.clock.now().saturating_sub(self.last_activity);
                let remaining = self.idle_ttl.saturating_sub(elapsed);
                if remaining.is_zero() && self.mcp.active_leases() == 0 {
                    break;
                }
                let wait = self.clock.wait_duration(if remaining.is_zero() {
                    self.idle_ttl
                } else {
                    remaining
                });
                match self.commands.recv_timeout(wait) {
                    Ok(command) => self.accept_command(command),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        self.mcp.reap_idle_leases();
                    }
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

    fn accept_command(&mut self, command: ConnectorIntent) {
        self.metrics.queue_depth.fetch_sub(1, Ordering::AcqRel);
        self.last_activity = self.clock.now();
        if self.needs_initial_overview {
            self.needs_initial_overview = false;
            if !matches!(&command, ConnectorIntent::Activate) {
                self.reload_overview();
            }
        }
        self.handle_command(command);
    }

    fn handle_command(&mut self, command: ConnectorIntent) {
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
            ConnectorIntent::BeginOAuth(server_id) => self.start_oauth(server_id),
            ConnectorIntent::SubmitOAuthClient {
                operation_id,
                server_id,
                client_id,
                client_secret,
                workspace_hint,
            } => self.submit_oauth(
                operation_id,
                StoredOAuthClient {
                    server_id,
                    client_id,
                    client_secret,
                    workspace_hint,
                },
            ),
            ConnectorIntent::SetPermission {
                server_id,
                tool_id,
                rule,
            } => self.set_permission(server_id, tool_id, rule),
            ConnectorIntent::EnsureSlackServer => self.ensure_slack(),
            ConnectorIntent::ImportConfiguration {
                source_name,
                contents,
            } => self.import_configuration(source_name, contents),
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
            ConnectorIntent::ResolveApproval { .. } => {
                self.publish_error(
                    OperationKind::Invoke,
                    ErrorCode::Internal,
                    "approval pending AU01",
                );
            }
            ConnectorIntent::RequestImportPicker | ConnectorIntent::OpenExternalUrl { .. } => {}
        }
    }

    fn reload_overview(&mut self) {
        match self.repository.load_overview() {
            Ok(overview) => {
                if let Err(error) = validate_overview(&overview, self.limits) {
                    self.publish_service_error(OperationKind::SaveServer, error);
                    return;
                }
                self.snapshot.config_revision = overview.config_revision;
                self.snapshot.slack_status = overview.slack_status;
                self.snapshot.slack_tool_count = overview.slack_tool_count;
                self.snapshot.servers = Arc::from(overview.servers);
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

    fn ensure_slack(&mut self) {
        if let Err(error) = self.repository.ensure_slack_server() {
            self.publish_service_error(OperationKind::SaveServer, error);
            return;
        }
        self.reload_overview();
    }

    fn import_configuration(
        &mut self,
        source_name: String,
        contents: connector_contract::SensitiveInput,
    ) {
        if contents.len() > self.limits.import_input_bytes {
            self.publish_error(
                OperationKind::Import,
                ErrorCode::LimitExceeded,
                "import input limit exceeded",
            );
            return;
        }
        if source_name.len() > self.limits.import_input_bytes {
            self.publish_error(
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
            self.publish_error(
                OperationKind::Import,
                ErrorCode::LimitExceeded,
                "import server limit exceeded",
            );
            return;
        }
        if let Err(error) = validate_server_drafts(&plan.servers, self.limits) {
            self.publish_service_error(OperationKind::Import, error);
            return;
        }
        if let Err(error) = self.repository.import_servers(plan.servers) {
            self.publish_service_error(OperationKind::Import, error);
            return;
        }
        self.reload_overview();
    }

    fn start_discover(&mut self, server_id: ServerId) {
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
        if let Err(error) = validate_server_draft(&server, self.limits) {
            self.publish_service_error(OperationKind::Discover, error);
            return;
        }
        let operation_id = self.new_operation_id();
        let generation = self.bump_generation(&server_id);
        let config_revision = self.snapshot.config_revision;
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
                    mcp.discover(&operation_for_thread, server, cancellation_for_thread)
                }));
                let _ = sender.send(JobCompletion {
                    operation_id: operation_for_thread,
                    server_id: server_for_thread,
                    generation,
                    config_revision,
                    kind: OperationKind::Discover,
                    payload,
                });
                unparker.unpark();
            });
        self.insert_job(
            operation_id,
            server_id,
            generation,
            config_revision,
            OperationKind::Discover,
            cancellation,
            thread,
        );
    }

    fn start_invoke(
        &mut self,
        server_id: ServerId,
        tool_id: connector_contract::ToolId,
        arguments_json: connector_contract::SensitiveInput,
    ) {
        if arguments_json.len() > self.limits.tool_input_bytes {
            self.publish_error(
                OperationKind::Invoke,
                ErrorCode::LimitExceeded,
                "tool input limit exceeded",
            );
            return;
        }
        if self.metrics.active_mcp.load(Ordering::Acquire) >= self.limits.mcp_operations {
            self.reject_backpressure(OperationKind::Invoke);
            return;
        }
        let server = match self.repository.load_server(&server_id) {
            Ok(server) => server,
            Err(error) => {
                self.publish_service_error(OperationKind::Invoke, error);
                return;
            }
        };
        if let Err(error) = validate_server_draft(&server, self.limits) {
            self.publish_service_error(OperationKind::Invoke, error);
            return;
        }
        let operation_id = self.new_operation_id();
        let generation = self.bump_generation(&server_id);
        let config_revision = self.snapshot.config_revision;
        let cancellation = CancellationToken::default();
        let mcp = Arc::clone(&self.mcp);
        let sender = self.result_sender.clone();
        let unparker = std::thread::current();
        let operation_for_thread = operation_id.clone();
        let server_for_thread = server_id.clone();
        let cancellation_for_thread = cancellation.clone();
        let thread = std::thread::Builder::new()
            .name("connector-mcp-invoke".to_owned())
            .spawn(move || {
                let payload = JobPayload::Invoke(run_backend_job(|| {
                    mcp.load_schema_and_invoke(
                        InvokeRequest {
                            operation_id: operation_for_thread.clone(),
                            server,
                            tool_id,
                            arguments_json,
                        },
                        cancellation_for_thread,
                    )
                }));
                let _ = sender.send(JobCompletion {
                    operation_id: operation_for_thread,
                    server_id: server_for_thread,
                    generation,
                    config_revision,
                    kind: OperationKind::Invoke,
                    payload,
                });
                unparker.unpark();
            });
        self.insert_job(
            operation_id,
            server_id,
            generation,
            config_revision,
            OperationKind::Invoke,
            cancellation,
            thread,
        );
    }

    fn start_oauth(&mut self, server_id: ServerId) {
        if self.metrics.active_oauth.load(Ordering::Acquire) >= self.limits.oauth_flows {
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
        if let Err(error) = validate_server_draft(&server, self.limits) {
            self.publish_service_error(OperationKind::OAuth, error);
            return;
        }
        let operation_id = self.new_operation_id();
        self.spawn_oauth_job(operation_id, server_id, move |oauth, op, cancellation| {
            oauth.begin(op, server, cancellation)
        });
    }

    fn submit_oauth(&mut self, operation_id: Option<OperationId>, client: StoredOAuthClient) {
        if self.metrics.active_oauth.load(Ordering::Acquire) >= self.limits.oauth_flows {
            self.reject_backpressure(OperationKind::OAuth);
            return;
        }
        if let Err(error) = validate_oauth_client(&client, self.limits) {
            self.publish_service_error(OperationKind::OAuth, error);
            return;
        }
        let operation_id = operation_id.unwrap_or_else(|| self.new_operation_id());
        let server_id = client.server_id.clone();
        self.snapshot.oauth_client_prompt = None;
        self.spawn_oauth_job(operation_id, server_id, move |oauth, op, cancellation| {
            oauth.submit_client(op, client, cancellation)
        });
    }

    fn spawn_oauth_job(
        &mut self,
        operation_id: OperationId,
        server_id: ServerId,
        run: impl FnOnce(
            Arc<dyn ConnectorOAuth>,
            &OperationId,
            CancellationToken,
        ) -> Result<OAuthOutput, ServiceError>
        + Send
        + 'static,
    ) {
        let generation = self.bump_generation(&server_id);
        let config_revision = self.snapshot.config_revision;
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
                let payload = JobPayload::OAuth(run_backend_job(|| {
                    run(oauth, &operation_for_thread, cancellation_for_thread)
                }));
                let _ = sender.send(JobCompletion {
                    operation_id: operation_for_thread,
                    server_id: server_for_thread,
                    generation,
                    config_revision,
                    kind: OperationKind::OAuth,
                    payload,
                });
                unparker.unpark();
            });
        self.insert_job(
            operation_id,
            server_id,
            generation,
            config_revision,
            OperationKind::OAuth,
            cancellation,
            thread,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_job(
        &mut self,
        operation_id: OperationId,
        server_id: ServerId,
        generation: u64,
        config_revision: Revision,
        kind: OperationKind,
        cancellation: CancellationToken,
        thread: Result<JoinHandle<()>, std::io::Error>,
    ) {
        let thread = match thread {
            Ok(thread) => thread,
            Err(_) => {
                self.publish_error(
                    kind,
                    ErrorCode::Internal,
                    "connector job thread unavailable",
                );
                return;
            }
        };
        match kind {
            OperationKind::Discover | OperationKind::Invoke => {
                self.metrics.active_mcp.fetch_add(1, Ordering::AcqRel);
            }
            OperationKind::OAuth => {
                self.metrics.active_oauth.fetch_add(1, Ordering::AcqRel);
            }
            _ => {}
        }
        self.operations.push(OperationSummary {
            id: operation_id.clone(),
            server_id: server_id.clone(),
            kind,
            phase: OperationPhase::Queued,
            error_code: None,
        });
        self.jobs.insert(
            operation_id,
            ActiveJob {
                server_id,
                generation,
                config_revision,
                kind,
                cancellation,
                thread: Some(thread),
            },
        );
        self.transition(kind, OperationPhase::Queued, None);
        self.publish();
    }

    fn finish_job(&mut self, completion: JobCompletion) {
        let Some(mut active) = self.jobs.remove(&completion.operation_id) else {
            self.metrics.stale_results.fetch_add(1, Ordering::AcqRel);
            return;
        };
        if let Some(thread) = active.thread.take() {
            let _ = thread.join();
        }
        match active.kind {
            OperationKind::Discover | OperationKind::Invoke => {
                self.metrics.active_mcp.fetch_sub(1, Ordering::AcqRel);
            }
            OperationKind::OAuth => {
                self.metrics.active_oauth.fetch_sub(1, Ordering::AcqRel);
            }
            _ => {}
        }
        self.operations
            .retain(|operation| operation.id != completion.operation_id);
        self.prune_generations();
        let current_generation = self
            .generations
            .get(&completion.server_id)
            .copied()
            .unwrap_or_default();
        if active.cancellation.is_cancelled()
            || current_generation != completion.generation
            || self.snapshot.config_revision != completion.config_revision
            || active.server_id != completion.server_id
            || active.generation != completion.generation
            || active.config_revision != completion.config_revision
            || active.kind != completion.kind
        {
            self.metrics.stale_results.fetch_add(1, Ordering::AcqRel);
            self.transition(
                completion.kind,
                OperationPhase::Cancelled,
                Some(ErrorCode::StaleResult),
            );
            self.publish();
            return;
        }
        match completion.payload {
            JobPayload::Discover(result) => self.finish_discover(completion.server_id, result),
            JobPayload::Invoke(result) => {
                self.finish_invoke(completion.operation_id, result);
            }
            JobPayload::OAuth(result) => self.finish_oauth(result),
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

    fn finish_invoke(&mut self, operation_id: OperationId, result: Result<String, ServiceError>) {
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
            Err(error) => self.publish_service_error(OperationKind::Invoke, error),
        }
    }

    fn finish_oauth(&mut self, result: Result<OAuthOutput, ServiceError>) {
        match result {
            Ok(OAuthOutput::Completed) => {
                self.snapshot.oauth_client_prompt = None;
                self.transition(OperationKind::OAuth, OperationPhase::Succeeded, None);
                self.reload_overview();
            }
            Ok(OAuthOutput::ClientInputRequired(prompt)) => {
                if let Err(error) = validate_oauth_prompt(&prompt, self.limits) {
                    self.publish_service_error(OperationKind::OAuth, error);
                    return;
                }
                self.snapshot.oauth_client_prompt = Some(prompt);
                self.transition(OperationKind::OAuth, OperationPhase::Succeeded, None);
                self.publish();
            }
            Err(error) => self.publish_service_error(OperationKind::OAuth, error),
        }
    }

    fn cancel(&mut self, operation_id: OperationId) {
        let Some(job) = self.jobs.get(&operation_id) else {
            return;
        };
        job.cancellation.cancel();
        let server_id = job.server_id.clone();
        let kind = job.kind;
        self.bump_generation(&server_id);
        match kind {
            OperationKind::Discover | OperationKind::Invoke => self.mcp.cancel(&operation_id),
            OperationKind::OAuth => self.oauth.cancel(&operation_id),
            _ => {}
        }
        if let Some(operation) = self
            .operations
            .iter_mut()
            .find(|operation| operation.id == operation_id)
        {
            operation.phase = OperationPhase::Cancelled;
            operation.error_code = Some(ErrorCode::Cancelled);
        }
        self.metrics.cancellations.fetch_add(1, Ordering::AcqRel);
        self.transition(kind, OperationPhase::Cancelled, Some(ErrorCode::Cancelled));
        self.publish();
    }

    fn begin_shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        let jobs: Vec<(OperationId, ServerId, OperationKind)> = self
            .jobs
            .iter()
            .map(|(id, job)| (id.clone(), job.server_id.clone(), job.kind))
            .collect();
        for (operation_id, server_id, kind) in jobs {
            if let Some(job) = self.jobs.get(&operation_id) {
                job.cancellation.cancel();
            }
            self.bump_generation(&server_id);
            match kind {
                OperationKind::Discover | OperationKind::Invoke => self.mcp.cancel(&operation_id),
                OperationKind::OAuth => self.oauth.cancel(&operation_id),
                _ => {}
            }
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
        let value = self.next_operation.fetch_add(1, Ordering::Relaxed);
        OperationId::new(format!("connector-{value}"))
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
    let (bytes, items) = server_draft_size(draft)?;
    if bytes > limits.import_input_bytes || items > limits.tools_per_server {
        return Err(ServiceError::new(
            ErrorCode::LimitExceeded,
            "server configuration resource limit exceeded",
        ));
    }
    Ok(())
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

fn validate_oauth_prompt(
    prompt: &connector_contract::OAuthClientPrompt,
    limits: ResourceLimits,
) -> Result<(), ServiceError> {
    let mut bytes = checked_add(prompt.server_id.as_str().len(), prompt.server_name.len())?;
    if let Some(workspace_hint) = &prompt.workspace_hint {
        bytes = checked_add(bytes, workspace_hint.len())?;
    }
    if let Some(reason) = &prompt.reason {
        bytes = checked_add(bytes, reason.len())?;
    }
    if bytes > limits.ui_result_bytes {
        return Err(ServiceError::new(
            ErrorCode::LimitExceeded,
            "OAuth prompt limit exceeded",
        ));
    }
    Ok(())
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
        ConnectionState, CredentialId, PermissionRule, SensitiveInput, ServerSummary, SlackStatus,
        ToolId, ToolListItem, TransportDraft, TransportKind,
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
                slack_status: if self.state.slack_ensures.load(Ordering::Acquire) == 0 {
                    SlackStatus::NotConfigured
                } else {
                    SlackStatus::Ready
                },
                slack_tool_count: 0,
                servers: vec![server_summary("server-1"), server_summary("server-2")],
            })
        }

        fn load_server(&mut self, server_id: &ServerId) -> Result<ServerDraft, ServiceError> {
            Ok(server_draft(server_id.as_str()))
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
            })
        }

        fn import_servers(&mut self, _servers: Vec<ServerDraft>) -> Result<Revision, ServiceError> {
            self.state.imports.fetch_add(1, Ordering::AcqRel);
            Ok(self.mutate())
        }
    }

    struct FakeSecrets;

    impl ConnectorSecrets for FakeSecrets {
        fn resolve_credential(
            &self,
            _credential_id: &CredentialId,
        ) -> Result<SensitiveInput, ServiceError> {
            Err(ServiceError::new(
                ErrorCode::SecretUnavailable,
                "secret unavailable in fixture",
            ))
        }

        fn store_oauth_client(&self, _client: StoredOAuthClient) -> Result<(), ServiceError> {
            Ok(())
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
    }

    impl FakeMcp {
        fn new(gate: Arc<Gate>) -> Self {
            Self {
                gate,
                active: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                tool_count: AtomicUsize::new(1),
                descriptor_bytes: AtomicUsize::new(1),
                cancel_releases: AtomicBool::new(true),
                panic_on_discover: AtomicBool::new(false),
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
            _server: ServerDraft,
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

        fn load_schema_and_invoke(
            &self,
            _request: InvokeRequest,
            _cancellation: CancellationToken,
        ) -> Result<String, ServiceError> {
            let _active = self.enter();
            self.gate.wait();
            Ok("ok".to_owned())
        }

        fn cancel(&self, _operation_id: &OperationId) {
            if self.cancel_releases.load(Ordering::Acquire) {
                self.gate.release();
            }
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

        fn run(&self) -> Result<OAuthOutput, ServiceError> {
            let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
            self.peak.fetch_max(active, Ordering::AcqRel);
            self.gate.wait();
            self.active.fetch_sub(1, Ordering::AcqRel);
            Ok(OAuthOutput::Completed)
        }
    }

    impl ConnectorOAuth for FakeOAuth {
        fn begin(
            &self,
            _operation_id: &OperationId,
            _server: ServerDraft,
            _cancellation: CancellationToken,
        ) -> Result<OAuthOutput, ServiceError> {
            self.run()
        }

        fn submit_client(
            &self,
            _operation_id: &OperationId,
            _client: StoredOAuthClient,
            _cancellation: CancellationToken,
        ) -> Result<OAuthOutput, ServiceError> {
            self.run()
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

    fn fixture(
        open_gate: Arc<Gate>,
        mcp_gate: Arc<Gate>,
        oauth_gate: Arc<Gate>,
        clock: Arc<dyn CoordinatorClock>,
    ) -> Fixture {
        let repo = Arc::new(RepoState::default());
        let mcp = Arc::new(FakeMcp::new(mcp_gate));
        let oauth = Arc::new(FakeOAuth::new(oauth_gate));
        let coordinator = ConnectorCoordinator::new(ConnectorCoordinatorConfig {
            limits: ResourceLimits::default(),
            idle_ttl: Duration::from_millis(100),
            repository_factory: Arc::new(FakeFactory {
                state: Arc::clone(&repo),
                open_gate: Arc::clone(&open_gate),
            }),
            secrets: Arc::new(FakeSecrets),
            mcp: mcp.clone(),
            oauth: oauth.clone(),
            clock,
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
            transport: TransportDraft::Http {
                url: "https://example.invalid/mcp".to_owned(),
            },
            enabled: true,
        }
    }

    fn server_summary(id: &str) -> ServerSummary {
        ServerSummary {
            id: ServerId::new(id),
            name: id.to_owned(),
            transport: TransportKind::Http,
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
            .dispatch(ConnectorIntent::EnsureSlackServer)
            .unwrap();
        wait_until(|| fixture.repo.slack_ensures.load(Ordering::Acquire) == 1);
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
            .dispatch(ConnectorIntent::BeginOAuth(ServerId::new("server-1")))
            .unwrap();
        wait_until(|| fixture.oauth.active.load(Ordering::Acquire) == 1);
        fixture
            .coordinator
            .dispatch(ConnectorIntent::BeginOAuth(ServerId::new("server-2")))
            .unwrap();
        wait_until(|| fixture.coordinator.metrics().backpressure_rejections >= 2);
        assert_eq!(fixture.oauth.peak.load(Ordering::Acquire), 1);

        mcp_gate.release();
        oauth_gate.release();
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
            .dispatch(ConnectorIntent::EnsureSlackServer)
            .unwrap();
        wait_until(|| fixture.coordinator.metrics().worker_starts == 2);
        wait_until(|| fixture.repo.slack_ensures.load(Ordering::Acquire) == 1);
        wait_until(|| fixture.coordinator.metrics().worker_alive == 0);
        assert_eq!(fixture.coordinator.metrics().worker_starts, 2);
        assert_eq!(fixture.repo.opens.load(Ordering::Acquire), 2);
        let resumed = fixture.coordinator.current_snapshot();
        assert!(resumed.revision.0 > first_revision);
        assert_eq!(resumed.slack_status, SlackStatus::Ready);
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
                source_name: "too-large.json".to_owned(),
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
                source_name: "many.json".to_owned(),
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
