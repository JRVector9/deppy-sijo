//! Bounded, lazy execution boundary for App-owned agent state I/O.
//!
//! This module deliberately knows neither a concrete storage type nor egui. The composition root supplies
//! a backend factory and a completion-edge wake callback. Projection requests are independently
//! latest-only per section, while durable mutations are retained in a small exact FIFO. At most
//! one job is in flight, so the capacity-one result channel can never accumulate a history.

use std::array;
use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

pub(crate) const AGENT_STATE_IDLE_TTL: Duration = Duration::from_secs(30);
pub(crate) const AGENT_STATE_CONTINUATION_MAX: usize = 8;
pub(crate) const AGENT_STATE_PENDING_BYTES_MAX: usize = 4 * 1024 * 1024;
pub(crate) const AGENT_STATE_STRUCTURED_BATCH_MAX: usize = 16;
pub(crate) const AGENT_STATE_STRUCTURED_BATCH_BYTES_MAX: usize = 512 * 1024;
const AGENT_STATE_SINGLE_EXACT_BYTES_MAX: usize = 32 * 1024;
const AGENT_STATE_PROJECTION_JOB_BYTES_MAX: usize = 4 * 1024 * 1024;
const AGENT_STATE_PROJECTION_RESULT_BYTES_MAX: usize = 4 * 1024 * 1024;

/// Stable low-cardinality failures. Backend implementations must map raw storage/filesystem
/// errors to one of these values before crossing the worker boundary.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentStateErrorCode {
    StorageUnavailable,
    InvalidData,
    ResourceLimit,
    Backpressure,
    Stale,
    WorkerUnavailable,
}

impl AgentStateErrorCode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::StorageUnavailable => "storage_unavailable",
            Self::InvalidData => "invalid_data",
            Self::ResourceLimit => "resource_limit",
            Self::Backpressure => "backpressure",
            Self::Stale => "stale",
            Self::WorkerUnavailable => "worker_unavailable",
        }
    }
}

impl fmt::Debug for AgentStateErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl fmt::Display for AgentStateErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Payloads report the backing memory they retain. Production backends must enforce their item
/// and byte ceilings before allocating or returning a payload; the worker performs a second
/// aggregate check before retaining it.
pub(crate) trait RetainedBytes {
    fn retained_bytes(&self) -> usize;
}

/// Independently coalesced projections. Adding a section is a protocol change and should remain
/// deliberate: the fixed array is what proves the latest backlog cannot exceed one per section.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AgentStateSection {
    Hooks,
    Attention,
    Restore,
    BindingSync,
    ResumeProbe,
    Catalog,
    ProjectNames,
}

impl AgentStateSection {
    const COUNT: usize = 7;

    const fn index(self) -> usize {
        match self {
            Self::Hooks => 0,
            Self::Attention => 1,
            Self::Restore => 2,
            Self::BindingSync => 3,
            Self::ResumeProbe => 4,
            Self::Catalog => 5,
            Self::ProjectNames => 6,
        }
    }

    #[cfg(test)]
    const ALL: [Self; Self::COUNT] = [
        Self::Hooks,
        Self::Attention,
        Self::Restore,
        Self::BindingSync,
        Self::ResumeProbe,
        Self::Catalog,
        Self::ProjectNames,
    ];
}

impl fmt::Debug for AgentStateSection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Hooks => "hooks",
            Self::Attention => "attention",
            Self::Restore => "restore",
            Self::BindingSync => "binding_sync",
            Self::ResumeProbe => "resume_probe",
            Self::Catalog => "catalog",
            Self::ProjectNames => "project_names",
        })
    }
}

/// Workspace generation plus a monotonic section revision. `workspace_epoch` changes invalidate
/// work from an older active workspace; `revision` orders requests within that epoch.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct AgentStateRevision {
    workspace_epoch: u64,
    revision: u64,
}

impl AgentStateRevision {
    pub(crate) const fn new(workspace_epoch: u64, revision: u64) -> Self {
        Self {
            workspace_epoch,
            revision,
        }
    }

    pub(crate) const fn workspace_epoch(self) -> u64 {
        self.workspace_epoch
    }

    pub(crate) const fn revision(self) -> u64 {
        self.revision
    }
}

impl fmt::Debug for AgentStateRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AgentStateRevision(..)")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExactKind {
    TurnDoneClear,
    BindingDelete,
    StructuredBatch { items: usize },
}

impl fmt::Debug for ExactKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TurnDoneClear => formatter.write_str("turn_done_clear"),
            Self::BindingDelete => formatter.write_str("binding_delete"),
            Self::StructuredBatch { items } => formatter
                .debug_struct("structured_batch")
                .field("items", items)
                .finish(),
        }
    }
}

pub(crate) struct ExactContinuation<E> {
    operation_id: u64,
    kind: ExactKind,
    payload: Arc<E>,
    retained_bytes: usize,
}

impl<E> ExactContinuation<E> {
    pub(crate) fn operation_id(&self) -> u64 {
        self.operation_id
    }

    pub(crate) fn kind(&self) -> ExactKind {
        self.kind
    }

    pub(crate) fn payload(&self) -> &E {
        &self.payload
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
}

impl<E> fmt::Debug for ExactContinuation<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExactContinuation")
            .field("kind", &self.kind)
            .field("payload", &"[REDACTED]")
            .finish()
    }
}

struct ProjectionRequest<P> {
    section: AgentStateSection,
    key: AgentStateRevision,
    payload: Arc<P>,
    retained_bytes: usize,
}

impl<P> fmt::Debug for ProjectionRequest<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProjectionRequest")
            .field("section", &self.section)
            .field("payload", &"[REDACTED]")
            .finish()
    }
}

/// Borrowed exact input for one aggregate backend transaction.
pub(crate) struct AgentStateExactInput<'a, E> {
    kind: ExactKind,
    payload: &'a E,
}

impl<E> AgentStateExactInput<'_, E> {
    pub(crate) fn kind(&self) -> ExactKind {
        self.kind
    }

    pub(crate) fn payload(&self) -> &E {
        self.payload
    }
}

/// Borrowed latest-only projection input for one aggregate backend transaction.
pub(crate) struct AgentStateProjectionInput<'a, P> {
    section: AgentStateSection,
    payload: &'a P,
}

impl<P> AgentStateProjectionInput<'_, P> {
    pub(crate) fn section(&self) -> AgentStateSection {
        self.section
    }

    pub(crate) fn payload(&self) -> &P {
        self.payload
    }
}

/// The composition-root adapter owns the concrete SQLite connection and filesystem helpers.
/// One dispatched worker job maps to exactly one aggregate backend call. The backend must apply
/// the optional exact mutation before reading projections and atomically return either the full
/// post-mutation snapshot or one sanitized failure for the entire job.
pub(crate) trait AgentStateBackend: Send + 'static {
    type ProjectionRequest: RetainedBytes + Send + Sync + 'static;
    type Snapshot: RetainedBytes + Send + Sync + 'static;
    type ExactRequest: RetainedBytes + Send + Sync + 'static;

    fn execute_job(
        &mut self,
        exact: Option<AgentStateExactInput<'_, Self::ExactRequest>>,
        projections: &[AgentStateProjectionInput<'_, Self::ProjectionRequest>],
    ) -> Result<Self::Snapshot, AgentStateErrorCode>;
}

type BackendFactory<B> = Arc<dyn Fn() -> Result<B, AgentStateErrorCode> + Send + Sync>;
type CompletionWake = Arc<dyn Fn() + Send + Sync>;

struct AgentStateJob<B: AgentStateBackend> {
    worker_generation: u64,
    dispatch_id: u64,
    exact: Option<Arc<ExactContinuation<B::ExactRequest>>>,
    projections: Vec<ProjectionRequest<B::ProjectionRequest>>,
}

struct WorkerOutcome<O> {
    worker_generation: u64,
    dispatch_id: u64,
    result: Result<Arc<O>, AgentStateErrorCode>,
}

struct InFlight<E> {
    worker_generation: u64,
    dispatch_id: u64,
    exact: Option<Arc<ExactContinuation<E>>>,
    projection_keys: Vec<(AgentStateSection, AgentStateRevision)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkerLifecycle {
    Running,
    Exited,
}

struct WorkerExitGuard(Arc<Mutex<WorkerLifecycle>>);

impl Drop for WorkerExitGuard {
    fn drop(&mut self) {
        *lock_unpoisoned(&self.0) = WorkerLifecycle::Exited;
    }
}

struct WorkerSlot<B: AgentStateBackend> {
    tx: mpsc::SyncSender<AgentStateJob<B>>,
    rx: mpsc::Receiver<WorkerOutcome<B::Snapshot>>,
    handle: JoinHandle<()>,
    lifecycle: Arc<Mutex<WorkerLifecycle>>,
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn failed_outcome<B: AgentStateBackend>(
    job: AgentStateJob<B>,
    error: AgentStateErrorCode,
) -> WorkerOutcome<B::Snapshot> {
    WorkerOutcome {
        worker_generation: job.worker_generation,
        dispatch_id: job.dispatch_id,
        result: Err(error),
    }
}

fn execute_job<B: AgentStateBackend>(
    backend: &mut B,
    job: AgentStateJob<B>,
) -> WorkerOutcome<B::Snapshot> {
    let exact = job.exact.as_ref().map(|request| AgentStateExactInput {
        kind: request.kind,
        payload: request.payload(),
    });
    let projections = job
        .projections
        .iter()
        .map(|request| AgentStateProjectionInput {
            section: request.section,
            payload: request.payload.as_ref(),
        })
        .collect::<Vec<_>>();
    let result = backend
        .execute_job(exact, &projections)
        .and_then(|snapshot| {
            if snapshot.retained_bytes() > AGENT_STATE_PROJECTION_RESULT_BYTES_MAX {
                return Err(AgentStateErrorCode::ResourceLimit);
            }
            Ok(Arc::new(snapshot))
        });

    WorkerOutcome {
        worker_generation: job.worker_generation,
        dispatch_id: job.dispatch_id,
        result,
    }
}

fn worker_loop<B: AgentStateBackend>(
    jobs: mpsc::Receiver<AgentStateJob<B>>,
    results: mpsc::SyncSender<WorkerOutcome<B::Snapshot>>,
    factory: BackendFactory<B>,
    wake: CompletionWake,
    lifecycle: Arc<Mutex<WorkerLifecycle>>,
    idle_ttl: Duration,
) {
    let _exit_guard = WorkerExitGuard(Arc::clone(&lifecycle));
    let mut backend = None;
    loop {
        let job = match jobs.recv_timeout(idle_ttl) {
            Ok(job) => job,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Serialize the final empty check with admission. A producer either observes
                // Running and its job is consumed here, or observes Exited and respawns a slot.
                let mut state = lock_unpoisoned(&lifecycle);
                match jobs.try_recv() {
                    Ok(job) => {
                        drop(state);
                        job
                    }
                    Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => {
                        *state = WorkerLifecycle::Exited;
                        return;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };

        if backend.is_none() {
            match factory() {
                Ok(opened) => backend = Some(opened),
                Err(error) => {
                    if results.send(failed_outcome(job, error)).is_err() {
                        return;
                    }
                    wake();
                    continue;
                }
            }
        }
        let outcome = execute_job(
            backend
                .as_mut()
                .expect("agent state backend initialized above"),
            job,
        );
        if results.send(outcome).is_err() {
            return;
        }
        wake();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum StageError {
    Stale,
    DuplicateOperation,
    Backpressure,
    ResourceLimit,
    Closed,
}

impl StageError {
    pub(crate) const fn error_code(self) -> AgentStateErrorCode {
        match self {
            Self::Stale => AgentStateErrorCode::Stale,
            Self::DuplicateOperation | Self::Backpressure => AgentStateErrorCode::Backpressure,
            Self::ResourceLimit => AgentStateErrorCode::ResourceLimit,
            Self::Closed => AgentStateErrorCode::WorkerUnavailable,
        }
    }
}

impl fmt::Debug for StageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.error_code().as_str())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmissionError {
    Busy,
    Empty,
    WorkerUnavailable,
    Closed,
}

impl fmt::Debug for AdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Busy => "busy",
            Self::Empty => "empty",
            Self::WorkerUnavailable => "worker_unavailable",
            Self::Closed => "closed",
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AgentStateWorkerState {
    Open,
    Draining,
    Closed,
}

pub(crate) struct ProjectionCompletion<O> {
    section: AgentStateSection,
    key: AgentStateRevision,
    result: Result<Arc<O>, AgentStateErrorCode>,
}

impl<O> ProjectionCompletion<O> {
    pub(crate) fn section(&self) -> AgentStateSection {
        self.section
    }

    pub(crate) fn key(&self) -> AgentStateRevision {
        self.key
    }

    pub(crate) fn into_result(self) -> Result<Arc<O>, AgentStateErrorCode> {
        self.result
    }
}

impl<O> fmt::Debug for ProjectionCompletion<O> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProjectionCompletion")
            .field("section", &self.section)
            .field("result", &self.result.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

pub(crate) struct ExactCompletion<E> {
    continuation: Arc<ExactContinuation<E>>,
    result: Result<(), AgentStateErrorCode>,
}

impl<E> ExactCompletion<E> {
    pub(crate) fn continuation(&self) -> &Arc<ExactContinuation<E>> {
        &self.continuation
    }

    pub(crate) fn result(&self) -> Result<(), AgentStateErrorCode> {
        self.result
    }

    pub(crate) fn into_parts(self) -> (Arc<ExactContinuation<E>>, Result<(), AgentStateErrorCode>) {
        (self.continuation, self.result)
    }
}

impl<E> fmt::Debug for ExactCompletion<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExactCompletion")
            .field("continuation", &self.continuation)
            .field("result", &self.result)
            .finish()
    }
}

pub(crate) struct AgentStateOutcome<E, O> {
    exact: Option<ExactCompletion<E>>,
    projections: Vec<ProjectionCompletion<O>>,
    stale_sections: usize,
}

impl<E, O> AgentStateOutcome<E, O> {
    pub(crate) fn exact(&self) -> Option<&ExactCompletion<E>> {
        self.exact.as_ref()
    }

    pub(crate) fn take_exact(&mut self) -> Option<ExactCompletion<E>> {
        self.exact.take()
    }

    pub(crate) fn projections(&self) -> &[ProjectionCompletion<O>] {
        &self.projections
    }

    pub(crate) fn into_projections(self) -> Vec<ProjectionCompletion<O>> {
        self.projections
    }

    pub(crate) fn stale_sections(&self) -> usize {
        self.stale_sections
    }
}

impl<E, O> fmt::Debug for AgentStateOutcome<E, O> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentStateOutcome")
            .field("has_exact", &self.exact.is_some())
            .field("projection_count", &self.projections.len())
            .field("stale_sections", &self.stale_sections)
            .finish()
    }
}

/// App-side admission state. Construction allocates no channel and spawns no thread.
pub(crate) struct AgentStateWorker<B: AgentStateBackend> {
    factory: BackendFactory<B>,
    wake: CompletionWake,
    idle_ttl: Duration,
    slot: Option<WorkerSlot<B>>,
    worker_generation: u64,
    next_dispatch_id: u64,
    latest_keys: [Option<AgentStateRevision>; AgentStateSection::COUNT],
    pending_projections:
        [Option<ProjectionRequest<B::ProjectionRequest>>; AgentStateSection::COUNT],
    pending_projection_bytes: usize,
    exact_queue: VecDeque<Arc<ExactContinuation<B::ExactRequest>>>,
    exact_operation_ids: HashSet<u64>,
    exact_retained_bytes: usize,
    in_flight: Option<InFlight<B::ExactRequest>>,
    buffered_outcome: Option<WorkerOutcome<B::Snapshot>>,
    state: AgentStateWorkerState,
}

impl<B: AgentStateBackend> AgentStateWorker<B> {
    pub(crate) fn new(factory: BackendFactory<B>, wake: CompletionWake) -> Self {
        Self::with_idle_ttl(factory, wake, AGENT_STATE_IDLE_TTL)
    }

    fn with_idle_ttl(factory: BackendFactory<B>, wake: CompletionWake, idle_ttl: Duration) -> Self {
        Self {
            factory,
            wake,
            idle_ttl,
            slot: None,
            worker_generation: 0,
            next_dispatch_id: 0,
            latest_keys: [None; AgentStateSection::COUNT],
            pending_projections: array::from_fn(|_| None),
            pending_projection_bytes: 0,
            exact_queue: VecDeque::new(),
            exact_operation_ids: HashSet::new(),
            exact_retained_bytes: 0,
            in_flight: None,
            buffered_outcome: None,
            state: AgentStateWorkerState::Open,
        }
    }

    pub(crate) fn stage_projection(
        &mut self,
        section: AgentStateSection,
        key: AgentStateRevision,
        payload: Arc<B::ProjectionRequest>,
    ) -> Result<(), StageError> {
        if self.state != AgentStateWorkerState::Open {
            return Err(StageError::Closed);
        }
        let index = section.index();
        if self.latest_keys[index].is_some_and(|latest| key <= latest) {
            return Err(StageError::Stale);
        }
        let retained_bytes = payload.retained_bytes();
        if retained_bytes > AGENT_STATE_PROJECTION_JOB_BYTES_MAX {
            return Err(StageError::ResourceLimit);
        }
        let previous = self.pending_projections[index]
            .as_ref()
            .map_or(0, |request| request.retained_bytes);
        let next_bytes = self
            .pending_projection_bytes
            .saturating_sub(previous)
            .checked_add(retained_bytes)
            .ok_or(StageError::ResourceLimit)?;
        if next_bytes > AGENT_STATE_PROJECTION_JOB_BYTES_MAX {
            return Err(StageError::Backpressure);
        }
        self.latest_keys[index] = Some(key);
        self.pending_projection_bytes = next_bytes;
        self.pending_projections[index] = Some(ProjectionRequest {
            section,
            key,
            payload,
            retained_bytes,
        });
        Ok(())
    }

    pub(crate) fn stage_exact(
        &mut self,
        operation_id: u64,
        kind: ExactKind,
        payload: Arc<B::ExactRequest>,
    ) -> Result<(), StageError> {
        if self.state != AgentStateWorkerState::Open {
            return Err(StageError::Closed);
        }
        if self.exact_operation_ids.contains(&operation_id) {
            return Err(StageError::DuplicateOperation);
        }
        if self.exact_operation_ids.len() >= AGENT_STATE_CONTINUATION_MAX {
            return Err(StageError::Backpressure);
        }
        let retained_bytes = payload.retained_bytes();
        match kind {
            ExactKind::TurnDoneClear | ExactKind::BindingDelete => {
                if retained_bytes > AGENT_STATE_SINGLE_EXACT_BYTES_MAX {
                    return Err(StageError::ResourceLimit);
                }
            }
            ExactKind::StructuredBatch { items } => {
                if items == 0
                    || items > AGENT_STATE_STRUCTURED_BATCH_MAX
                    || retained_bytes > AGENT_STATE_STRUCTURED_BATCH_BYTES_MAX
                {
                    return Err(StageError::ResourceLimit);
                }
            }
        }
        let next_bytes = self
            .exact_retained_bytes
            .checked_add(retained_bytes)
            .ok_or(StageError::ResourceLimit)?;
        if next_bytes > AGENT_STATE_PENDING_BYTES_MAX {
            return Err(StageError::Backpressure);
        }
        let continuation = Arc::new(ExactContinuation {
            operation_id,
            kind,
            payload,
            retained_bytes,
        });
        self.exact_operation_ids.insert(operation_id);
        self.exact_retained_bytes = next_bytes;
        self.exact_queue.push_back(continuation);
        Ok(())
    }

    pub(crate) fn pending_exact_count(&self) -> usize {
        self.exact_operation_ids.len()
    }

    pub(crate) fn pending_exact_bytes(&self) -> usize {
        self.exact_retained_bytes
    }

    pub(crate) fn pending_projection_count(&self) -> usize {
        self.pending_projections
            .iter()
            .filter(|request| request.is_some())
            .count()
    }

    pub(crate) fn has_in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    pub(crate) fn has_live_slot(&self) -> bool {
        self.slot.is_some()
    }

    pub(crate) fn worker_generation(&self) -> u64 {
        self.worker_generation
    }

    fn spawn_slot(&mut self) -> Result<(), AdmissionError> {
        let (tx, jobs) = mpsc::sync_channel(1);
        let (results, rx) = mpsc::sync_channel(1);
        let lifecycle = Arc::new(Mutex::new(WorkerLifecycle::Running));
        let worker_lifecycle = Arc::clone(&lifecycle);
        let factory = Arc::clone(&self.factory);
        let wake = Arc::clone(&self.wake);
        let idle_ttl = self.idle_ttl;
        let handle = std::thread::Builder::new()
            .name("agent-state".to_owned())
            .spawn(move || worker_loop(jobs, results, factory, wake, worker_lifecycle, idle_ttl))
            .map_err(|_| AdmissionError::WorkerUnavailable)?;
        self.worker_generation = self.worker_generation.wrapping_add(1);
        self.slot = Some(WorkerSlot {
            tx,
            rx,
            handle,
            lifecycle,
        });
        Ok(())
    }

    fn close_slot(&mut self) {
        let Some(slot) = self.slot.take() else {
            return;
        };
        let WorkerSlot {
            tx,
            rx,
            handle,
            lifecycle: _,
        } = slot;
        drop(rx);
        drop(tx);
        let _ = handle.join();
    }

    fn reap_finished(&mut self) {
        let finished = self
            .slot
            .as_ref()
            .is_some_and(|slot| slot.handle.is_finished());
        if !finished {
            return;
        }
        let slot = self.slot.take().expect("finished slot exists");
        if self.buffered_outcome.is_none() {
            self.buffered_outcome = slot.rx.try_recv().ok();
        }
        drop(slot.tx);
        drop(slot.rx);
        let _ = slot.handle.join();
    }

    fn restore_unsent_job(&mut self, job: AgentStateJob<B>) {
        for request in job.projections {
            let index = request.section.index();
            let keep = self.pending_projections[index]
                .as_ref()
                .is_none_or(|pending| pending.key < request.key);
            if keep {
                if let Some(previous) = self.pending_projections[index].replace(request) {
                    self.pending_projection_bytes = self
                        .pending_projection_bytes
                        .saturating_sub(previous.retained_bytes);
                }
                self.pending_projection_bytes = self.pending_projection_bytes.saturating_add(
                    self.pending_projections[index]
                        .as_ref()
                        .map_or(0, |value| value.retained_bytes),
                );
            }
        }
        if let Some(exact) = job.exact {
            self.exact_queue.push_front(exact);
        }
    }

    pub(crate) fn admit(&mut self) -> Result<(), AdmissionError> {
        if self.state == AgentStateWorkerState::Closed {
            return Err(AdmissionError::Closed);
        }
        self.reap_finished();
        if self.in_flight.is_some() || self.buffered_outcome.is_some() {
            return Err(AdmissionError::Busy);
        }
        if self.exact_queue.is_empty()
            && self
                .pending_projections
                .iter()
                .all(|request| request.is_none())
        {
            return Err(AdmissionError::Empty);
        }
        if self.slot.is_none() {
            self.spawn_slot()?;
        }

        self.next_dispatch_id = self.next_dispatch_id.wrapping_add(1);
        let dispatch_id = self.next_dispatch_id;
        let worker_generation = self.worker_generation;
        let exact = self.exact_queue.pop_front();
        let projections = self
            .pending_projections
            .iter_mut()
            .filter_map(Option::take)
            .collect::<Vec<_>>();
        self.pending_projection_bytes = 0;
        let projection_keys = projections
            .iter()
            .map(|request| (request.section, request.key))
            .collect();
        let job = AgentStateJob {
            worker_generation,
            dispatch_id,
            exact: exact.clone(),
            projections,
        };

        let send_result = {
            let slot = self.slot.as_ref().expect("slot initialized above");
            let lifecycle = lock_unpoisoned(&slot.lifecycle);
            if *lifecycle == WorkerLifecycle::Exited {
                Err(mpsc::TrySendError::Disconnected(job))
            } else {
                slot.tx.try_send(job)
            }
        };
        match send_result {
            Ok(()) => {
                self.in_flight = Some(InFlight {
                    worker_generation,
                    dispatch_id,
                    exact,
                    projection_keys,
                });
                Ok(())
            }
            Err(mpsc::TrySendError::Full(job) | mpsc::TrySendError::Disconnected(job)) => {
                self.restore_unsent_job(job);
                self.close_slot();
                Err(AdmissionError::WorkerUnavailable)
            }
        }
    }

    fn unavailable_outcome(&self) -> Option<WorkerOutcome<B::Snapshot>> {
        let in_flight = self.in_flight.as_ref()?;
        Some(WorkerOutcome {
            worker_generation: in_flight.worker_generation,
            dispatch_id: in_flight.dispatch_id,
            result: Err(AgentStateErrorCode::WorkerUnavailable),
        })
    }

    pub(crate) fn try_recv(
        &mut self,
    ) -> Result<AgentStateOutcome<B::ExactRequest, B::Snapshot>, mpsc::TryRecvError> {
        self.reap_finished();
        let raw = if let Some(outcome) = self.buffered_outcome.take() {
            outcome
        } else {
            let receive = match self.slot.as_ref() {
                Some(slot) => slot.rx.try_recv(),
                None => {
                    return self
                        .unavailable_outcome()
                        .map(Ok)
                        .unwrap_or(Err(mpsc::TryRecvError::Disconnected))
                        .and_then(|raw| self.finish_outcome(raw));
                }
            };
            match receive {
                Ok(outcome) => outcome,
                Err(mpsc::TryRecvError::Disconnected) => self
                    .unavailable_outcome()
                    .ok_or(mpsc::TryRecvError::Disconnected)?,
                Err(error) => return Err(error),
            }
        };
        self.finish_outcome(raw)
    }

    fn finish_outcome(
        &mut self,
        raw: WorkerOutcome<B::Snapshot>,
    ) -> Result<AgentStateOutcome<B::ExactRequest, B::Snapshot>, mpsc::TryRecvError> {
        let in_flight = self
            .in_flight
            .take()
            .ok_or(mpsc::TryRecvError::Disconnected)?;
        if raw.worker_generation != in_flight.worker_generation
            || raw.dispatch_id != in_flight.dispatch_id
        {
            // Return the exact continuation for correlation; callers must not auto-retry it.
            let exact = in_flight.exact.map(|continuation| ExactCompletion {
                continuation,
                result: Err(AgentStateErrorCode::Stale),
            });
            self.release_exact(exact.as_ref());
            return Ok(AgentStateOutcome {
                exact,
                projections: Vec::new(),
                stale_sections: in_flight.projection_keys.len(),
            });
        }

        let exact = in_flight.exact.map(|continuation| ExactCompletion {
            continuation,
            result: raw.result.as_ref().map(|_| ()).map_err(|error| *error),
        });
        self.release_exact(exact.as_ref());

        let mut stale_sections = 0usize;
        let mut projections = Vec::with_capacity(in_flight.projection_keys.len());
        for (section, key) in in_flight.projection_keys {
            if self.latest_keys[section.index()] != Some(key) {
                stale_sections = stale_sections.saturating_add(1);
                continue;
            }
            projections.push(ProjectionCompletion {
                section,
                key,
                result: raw.result.clone(),
            });
        }
        Ok(AgentStateOutcome {
            exact,
            projections,
            stale_sections,
        })
    }

    fn release_exact(&mut self, exact: Option<&ExactCompletion<B::ExactRequest>>) {
        let Some(exact) = exact else {
            return;
        };
        self.exact_operation_ids
            .remove(&exact.continuation.operation_id);
        self.exact_retained_bytes = self
            .exact_retained_bytes
            .saturating_sub(exact.continuation.retained_bytes);
    }

    fn begin_shutdown(&mut self) {
        if self.state != AgentStateWorkerState::Open {
            return;
        }
        self.state = AgentStateWorkerState::Draining;
        for pending in &mut self.pending_projections {
            pending.take();
        }
        self.pending_projection_bytes = 0;
    }

    fn recv_outcome_blocking(
        &mut self,
    ) -> Result<AgentStateOutcome<B::ExactRequest, B::Snapshot>, mpsc::TryRecvError> {
        self.reap_finished();
        let raw = if let Some(outcome) = self.buffered_outcome.take() {
            outcome
        } else if let Some(slot) = self.slot.as_ref() {
            match slot.rx.recv() {
                Ok(outcome) => outcome,
                Err(_) => self
                    .unavailable_outcome()
                    .ok_or(mpsc::TryRecvError::Disconnected)?,
            }
        } else {
            self.unavailable_outcome()
                .ok_or(mpsc::TryRecvError::Disconnected)?
        };
        self.finish_outcome(raw)
    }

    fn drain_queued_exact_as_failed(
        &mut self,
        error: AgentStateErrorCode,
        completions: &mut Vec<ExactCompletion<B::ExactRequest>>,
    ) {
        while let Some(continuation) = self.exact_queue.pop_front() {
            let completion = ExactCompletion {
                continuation,
                result: Err(error),
            };
            self.release_exact(Some(&completion));
            completions.push(completion);
        }
    }

    /// Stops admission, discards coalescible projections, and deterministically settles every
    /// already queued or in-flight exact continuation in FIFO order. An in-flight job whose result
    /// channel disconnects is reported once as `WorkerUnavailable`; it is never retried because
    /// delivery may have occurred. The returned vector is bounded by the exact admission ceiling.
    ///
    /// This is a blocking composition-root shutdown hook. Normal frame logic must keep using
    /// [`Self::try_recv`] and [`Self::admit`].
    pub(crate) fn shutdown_drain(&mut self) -> Vec<ExactCompletion<B::ExactRequest>> {
        if self.state == AgentStateWorkerState::Closed {
            return Vec::new();
        }
        self.begin_shutdown();
        let mut completions = Vec::with_capacity(self.exact_operation_ids.len());
        loop {
            if self.in_flight.is_some() {
                match self.recv_outcome_blocking() {
                    Ok(mut outcome) => {
                        if let Some(exact) = outcome.take_exact() {
                            completions.push(exact);
                        }
                    }
                    Err(_) => {
                        // Defensive fallback for an internally inconsistent channel state. Do not
                        // retry the possibly delivered in-flight continuation.
                        if let Some(in_flight) = self.in_flight.take()
                            && let Some(continuation) = in_flight.exact
                        {
                            let completion = ExactCompletion {
                                continuation,
                                result: Err(AgentStateErrorCode::WorkerUnavailable),
                            };
                            self.release_exact(Some(&completion));
                            completions.push(completion);
                        }
                    }
                }
                continue;
            }
            if self.exact_queue.is_empty() {
                break;
            }
            match self.admit() {
                Ok(()) => {}
                Err(AdmissionError::WorkerUnavailable) => {
                    // The job is restored before this error is returned, so none of these queued
                    // continuations has an unknown delivery. Fail them once instead of respawning
                    // forever during process shutdown.
                    self.drain_queued_exact_as_failed(
                        AgentStateErrorCode::WorkerUnavailable,
                        &mut completions,
                    );
                    break;
                }
                Err(AdmissionError::Empty | AdmissionError::Busy | AdmissionError::Closed) => {
                    // These states are unreachable while draining a non-empty queue, but fail
                    // closed instead of looping or leaving retained continuation accounting.
                    self.drain_queued_exact_as_failed(
                        AgentStateErrorCode::WorkerUnavailable,
                        &mut completions,
                    );
                    break;
                }
            }
        }
        self.close_slot();
        self.state = AgentStateWorkerState::Closed;
        debug_assert!(self.exact_operation_ids.is_empty());
        debug_assert_eq!(self.exact_retained_bytes, 0);
        completions
    }
}

impl<B: AgentStateBackend> Drop for AgentStateWorker<B> {
    fn drop(&mut self) {
        let _ = self.shutdown_drain();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Barrier, Condvar};
    use std::time::Instant;

    #[derive(Clone)]
    struct TestPayload {
        marker: &'static str,
        bytes: usize,
    }

    impl RetainedBytes for TestPayload {
        fn retained_bytes(&self) -> usize {
            self.bytes
        }
    }

    struct BackendState {
        calls: Mutex<Vec<(&'static str, AgentStateSection)>>,
        aggregate_calls: AtomicUsize,
        fail_next: Mutex<Option<AgentStateErrorCode>>,
        panic_after_record: AtomicBool,
        block_first: AtomicBool,
        entered: (Mutex<bool>, Condvar),
        release: (Mutex<bool>, Condvar),
        dropped: AtomicUsize,
    }

    impl BackendState {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                aggregate_calls: AtomicUsize::new(0),
                fail_next: Mutex::new(None),
                panic_after_record: AtomicBool::new(false),
                block_first: AtomicBool::new(false),
                entered: (Mutex::new(false), Condvar::new()),
                release: (Mutex::new(false), Condvar::new()),
                dropped: AtomicUsize::new(0),
            })
        }

        fn block_next(&self) {
            self.block_first.store(true, Ordering::Release);
            *lock_unpoisoned(&self.entered.0) = false;
            *lock_unpoisoned(&self.release.0) = false;
        }

        fn fail_next(&self, error: AgentStateErrorCode) {
            *lock_unpoisoned(&self.fail_next) = Some(error);
        }

        fn panic_after_next_record(&self) {
            self.panic_after_record.store(true, Ordering::Release);
        }

        fn wait_until_entered(&self) {
            let entered = lock_unpoisoned(&self.entered.0);
            let _entered = self
                .entered
                .1
                .wait_while(entered, |entered| !*entered)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }

        fn release(&self) {
            *lock_unpoisoned(&self.release.0) = true;
            self.release.1.notify_all();
        }

        fn maybe_block(&self) {
            if !self.block_first.swap(false, Ordering::AcqRel) {
                return;
            }
            *lock_unpoisoned(&self.entered.0) = true;
            self.entered.1.notify_all();
            let released = lock_unpoisoned(&self.release.0);
            let _released = self
                .release
                .1
                .wait_while(released, |released| !*released)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    struct TestBackend {
        state: Arc<BackendState>,
    }

    impl Drop for TestBackend {
        fn drop(&mut self) {
            self.state.dropped.fetch_add(1, Ordering::AcqRel);
        }
    }

    impl AgentStateBackend for TestBackend {
        type ProjectionRequest = TestPayload;
        type Snapshot = TestPayload;
        type ExactRequest = TestPayload;

        fn execute_job(
            &mut self,
            exact: Option<AgentStateExactInput<'_, Self::ExactRequest>>,
            projections: &[AgentStateProjectionInput<'_, Self::ProjectionRequest>],
        ) -> Result<Self::Snapshot, AgentStateErrorCode> {
            self.state.maybe_block();
            self.state.aggregate_calls.fetch_add(1, Ordering::AcqRel);
            {
                let mut calls = lock_unpoisoned(&self.state.calls);
                if let Some(exact) = exact {
                    let _kind = exact.kind();
                    calls.push((exact.payload().marker, AgentStateSection::BindingSync));
                }
            }
            if self.state.panic_after_record.swap(false, Ordering::AcqRel) {
                panic!("injected backend exit after possible delivery");
            }
            if let Some(error) = lock_unpoisoned(&self.state.fail_next).take() {
                return Err(error);
            }
            let mut calls = lock_unpoisoned(&self.state.calls);
            for projection in projections {
                calls.push((projection.payload().marker, projection.section()));
            }
            Ok(TestPayload {
                marker: "snapshot",
                bytes: projections.iter().map(|value| value.payload().bytes).sum(),
            })
        }
    }

    struct Harness {
        worker: AgentStateWorker<TestBackend>,
        opens: Arc<AtomicUsize>,
        wakes: Arc<AtomicUsize>,
        state: Arc<BackendState>,
    }

    impl Harness {
        fn new(idle_ttl: Duration) -> Self {
            let opens = Arc::new(AtomicUsize::new(0));
            let wakes = Arc::new(AtomicUsize::new(0));
            let state = BackendState::new();
            let factory_opens = Arc::clone(&opens);
            let factory_state = Arc::clone(&state);
            let factory = Arc::new(move || {
                factory_opens.fetch_add(1, Ordering::AcqRel);
                Ok(TestBackend {
                    state: Arc::clone(&factory_state),
                })
            });
            let wake_count = Arc::clone(&wakes);
            let wake = Arc::new(move || {
                wake_count.fetch_add(1, Ordering::AcqRel);
            });
            Self {
                worker: AgentStateWorker::with_idle_ttl(factory, wake, idle_ttl),
                opens,
                wakes,
                state,
            }
        }

        fn payload(marker: &'static str) -> Arc<TestPayload> {
            Arc::new(TestPayload { marker, bytes: 1 })
        }

        fn wait_outcome(&mut self) -> AgentStateOutcome<TestPayload, TestPayload> {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match self.worker.try_recv() {
                    Ok(outcome) => return outcome,
                    Err(mpsc::TryRecvError::Empty) if Instant::now() < deadline => {
                        std::thread::yield_now();
                    }
                    Err(error) => panic!("outcome unavailable: {error:?}"),
                }
            }
        }
    }

    #[test]
    fn construction_is_lazy_and_first_admission_opens_once() {
        let mut harness = Harness::new(Duration::from_secs(30));
        assert!(!harness.worker.has_live_slot());
        assert_eq!(harness.opens.load(Ordering::Acquire), 0);
        assert_eq!(harness.wakes.load(Ordering::Acquire), 0);

        harness
            .worker
            .stage_projection(
                AgentStateSection::Hooks,
                AgentStateRevision::new(1, 1),
                Harness::payload("first"),
            )
            .unwrap();
        harness.worker.admit().unwrap();
        let outcome = harness.wait_outcome();
        assert_eq!(outcome.projections().len(), 1);
        assert_eq!(harness.opens.load(Ordering::Acquire), 1);
        assert_eq!(harness.wakes.load(Ordering::Acquire), 1);
    }

    #[test]
    fn revisions_collapse_to_latest_and_sections_are_independent() {
        let mut harness = Harness::new(Duration::from_secs(30));
        for revision in 1..=1_000 {
            harness
                .worker
                .stage_projection(
                    AgentStateSection::Hooks,
                    AgentStateRevision::new(4, revision),
                    Harness::payload(if revision == 1_000 { "latest" } else { "old" }),
                )
                .unwrap();
        }
        for section in AgentStateSection::ALL.into_iter().skip(1) {
            harness
                .worker
                .stage_projection(
                    section,
                    AgentStateRevision::new(4, 1),
                    Harness::payload("independent"),
                )
                .unwrap();
        }
        assert_eq!(harness.worker.pending_projection_count(), 7);
        harness.worker.admit().unwrap();
        let outcome = harness.wait_outcome();
        assert_eq!(outcome.projections().len(), 7);
        let calls = lock_unpoisoned(&harness.state.calls);
        assert!(calls.contains(&("latest", AgentStateSection::Hooks)));
        assert!(!calls.iter().any(|(marker, _)| *marker == "old"));
    }

    #[test]
    fn newer_inflight_revision_makes_old_outcome_stale() {
        let mut harness = Harness::new(Duration::from_secs(30));
        harness.state.block_next();
        harness
            .worker
            .stage_projection(
                AgentStateSection::Hooks,
                AgentStateRevision::new(1, 1),
                Harness::payload("old"),
            )
            .unwrap();
        harness.worker.admit().unwrap();
        harness.state.wait_until_entered();
        harness
            .worker
            .stage_projection(
                AgentStateSection::Hooks,
                AgentStateRevision::new(1, 2),
                Harness::payload("new"),
            )
            .unwrap();
        harness.state.release();

        let stale = harness.wait_outcome();
        assert_eq!(stale.stale_sections(), 1);
        assert!(stale.projections().is_empty());
        harness.worker.admit().unwrap();
        let current = harness.wait_outcome();
        assert_eq!(current.projections().len(), 1);
        assert_eq!(current.projections()[0].key().revision(), 2);
    }

    #[test]
    fn exact_fifo_and_resource_backpressure_are_bounded() {
        let mut harness = Harness::new(Duration::from_secs(30));
        for operation in 0..AGENT_STATE_CONTINUATION_MAX as u64 {
            harness
                .worker
                .stage_exact(
                    operation,
                    ExactKind::TurnDoneClear,
                    Harness::payload("exact"),
                )
                .unwrap();
        }
        assert_eq!(harness.worker.pending_exact_count(), 8);
        assert_eq!(
            harness
                .worker
                .stage_exact(99, ExactKind::TurnDoneClear, Harness::payload("overflow")),
            Err(StageError::Backpressure)
        );
        for expected in 0..AGENT_STATE_CONTINUATION_MAX as u64 {
            harness.worker.admit().unwrap();
            let mut outcome = harness.wait_outcome();
            let exact = outcome.take_exact().unwrap();
            assert_eq!(exact.continuation().operation_id(), expected);
            assert_eq!(exact.result(), Ok(()));
        }
        assert_eq!(harness.worker.pending_exact_count(), 0);

        let oversized = Arc::new(TestPayload {
            marker: "large",
            bytes: AGENT_STATE_STRUCTURED_BATCH_BYTES_MAX + 1,
        });
        assert_eq!(
            harness
                .worker
                .stage_exact(100, ExactKind::StructuredBatch { items: 1 }, oversized),
            Err(StageError::ResourceLimit)
        );
        assert_eq!(
            harness.worker.stage_exact(
                101,
                ExactKind::StructuredBatch {
                    items: AGENT_STATE_STRUCTURED_BATCH_MAX + 1
                },
                Harness::payload("too_many")
            ),
            Err(StageError::ResourceLimit)
        );

        let binding_delete_at_limit = Arc::new(TestPayload {
            marker: "binding-delete-at-limit",
            bytes: AGENT_STATE_SINGLE_EXACT_BYTES_MAX,
        });
        harness
            .worker
            .stage_exact(102, ExactKind::BindingDelete, binding_delete_at_limit)
            .unwrap();
        harness.worker.admit().unwrap();
        let mut outcome = harness.wait_outcome();
        let exact = outcome.take_exact().unwrap();
        assert_eq!(exact.continuation().kind(), ExactKind::BindingDelete);
        assert_eq!(exact.result(), Ok(()));

        let oversized_binding_delete = Arc::new(TestPayload {
            marker: "binding-delete-too-large",
            bytes: AGENT_STATE_SINGLE_EXACT_BYTES_MAX + 1,
        });
        assert_eq!(
            harness
                .worker
                .stage_exact(103, ExactKind::BindingDelete, oversized_binding_delete),
            Err(StageError::ResourceLimit)
        );
    }

    #[test]
    fn exact_mutation_runs_before_projection_in_one_job() {
        let mut harness = Harness::new(Duration::from_secs(30));
        harness
            .worker
            .stage_exact(7, ExactKind::TurnDoneClear, Harness::payload("mutation"))
            .unwrap();
        harness
            .worker
            .stage_projection(
                AgentStateSection::Attention,
                AgentStateRevision::new(1, 1),
                Harness::payload("projection"),
            )
            .unwrap();
        harness.worker.admit().unwrap();
        let _ = harness.wait_outcome();
        assert_eq!(
            *lock_unpoisoned(&harness.state.calls),
            [
                ("mutation", AgentStateSection::BindingSync),
                ("projection", AgentStateSection::Attention),
            ]
        );
        assert_eq!(harness.state.aggregate_calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn aggregate_failure_rolls_back_exact_and_never_publishes_projection_success() {
        let mut harness = Harness::new(Duration::from_secs(30));
        harness
            .state
            .fail_next(AgentStateErrorCode::StorageUnavailable);
        harness
            .worker
            .stage_exact(8, ExactKind::TurnDoneClear, Harness::payload("mutation"))
            .unwrap();
        harness
            .worker
            .stage_projection(
                AgentStateSection::Attention,
                AgentStateRevision::new(1, 1),
                Harness::payload("projection"),
            )
            .unwrap();

        harness.worker.admit().unwrap();
        let mut outcome = harness.wait_outcome();
        let exact = outcome.take_exact().unwrap();
        assert_eq!(exact.result(), Err(AgentStateErrorCode::StorageUnavailable));
        assert_eq!(outcome.projections().len(), 1);
        assert!(matches!(
            outcome.into_projections().pop().unwrap().into_result(),
            Err(AgentStateErrorCode::StorageUnavailable)
        ));
        assert_eq!(harness.state.aggregate_calls.load(Ordering::Acquire), 1);
        assert_eq!(
            *lock_unpoisoned(&harness.state.calls),
            [("mutation", AgentStateSection::BindingSync)]
        );
    }

    #[test]
    fn shutdown_drain_settles_inflight_and_multiple_queued_exact_fifo() {
        let mut harness = Harness::new(Duration::from_secs(30));
        harness.state.block_next();
        for (operation_id, kind, marker) in [
            (20, ExactKind::TurnDoneClear, "first"),
            (21, ExactKind::BindingDelete, "second"),
            (22, ExactKind::StructuredBatch { items: 1 }, "third"),
        ] {
            harness
                .worker
                .stage_exact(operation_id, kind, Harness::payload(marker))
                .unwrap();
        }
        harness.worker.admit().unwrap();
        harness.state.wait_until_entered();
        harness.state.release();

        let completions = harness.worker.shutdown_drain();
        assert_eq!(
            completions
                .iter()
                .map(|completion| completion.continuation().operation_id())
                .collect::<Vec<_>>(),
            [20, 21, 22]
        );
        assert!(
            completions
                .iter()
                .all(|completion| completion.result() == Ok(()))
        );
        assert_eq!(
            *lock_unpoisoned(&harness.state.calls),
            [
                ("first", AgentStateSection::BindingSync),
                ("second", AgentStateSection::BindingSync),
                ("third", AgentStateSection::BindingSync),
            ]
        );
        assert_eq!(harness.state.aggregate_calls.load(Ordering::Acquire), 3);
        assert_eq!(harness.worker.pending_exact_count(), 0);
        assert_eq!(harness.worker.pending_exact_bytes(), 0);
        assert!(!harness.worker.has_live_slot());
        assert_eq!(
            harness
                .worker
                .stage_exact(23, ExactKind::TurnDoneClear, Harness::payload("closed")),
            Err(StageError::Closed)
        );
        assert!(harness.worker.shutdown_drain().is_empty());
    }

    #[test]
    fn shutdown_never_retries_unknown_delivery_and_continues_known_unsent_fifo() {
        let mut harness = Harness::new(Duration::from_secs(30));
        harness.state.panic_after_next_record();
        harness
            .worker
            .stage_exact(30, ExactKind::BindingDelete, Harness::payload("unknown"))
            .unwrap();
        harness
            .worker
            .stage_exact(
                31,
                ExactKind::TurnDoneClear,
                Harness::payload("known-unsent"),
            )
            .unwrap();
        harness.worker.admit().unwrap();

        let completions = harness.worker.shutdown_drain();
        assert_eq!(completions.len(), 2);
        assert_eq!(completions[0].continuation().operation_id(), 30);
        assert_eq!(
            completions[0].result(),
            Err(AgentStateErrorCode::WorkerUnavailable)
        );
        assert_eq!(completions[1].continuation().operation_id(), 31);
        assert_eq!(completions[1].result(), Ok(()));
        let calls = lock_unpoisoned(&harness.state.calls);
        assert_eq!(
            calls
                .iter()
                .filter(|(marker, _)| *marker == "unknown")
                .count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|(marker, _)| *marker == "known-unsent")
                .count(),
            1
        );
        assert_eq!(harness.state.aggregate_calls.load(Ordering::Acquire), 2);
        assert_eq!(harness.opens.load(Ordering::Acquire), 2);
    }

    #[test]
    fn idle_exit_restarts_with_new_generation_and_no_lost_send() {
        let mut harness = Harness::new(Duration::from_millis(10));
        harness
            .worker
            .stage_projection(
                AgentStateSection::Hooks,
                AgentStateRevision::new(1, 1),
                Harness::payload("one"),
            )
            .unwrap();
        harness.worker.admit().unwrap();
        let _ = harness.wait_outcome();
        let first_generation = harness.worker.worker_generation();
        std::thread::sleep(Duration::from_millis(30));

        harness
            .worker
            .stage_projection(
                AgentStateSection::Hooks,
                AgentStateRevision::new(1, 2),
                Harness::payload("two"),
            )
            .unwrap();
        harness.worker.admit().unwrap();
        let outcome = harness.wait_outcome();
        assert_eq!(outcome.projections().len(), 1);
        assert!(harness.worker.worker_generation() > first_generation);
        assert_eq!(harness.opens.load(Ordering::Acquire), 2);
    }

    #[test]
    fn timeout_send_race_never_reports_success_for_a_lost_job() {
        let mut harness = Harness::new(Duration::from_millis(5));
        for revision in 1..=40 {
            harness
                .worker
                .stage_projection(
                    AgentStateSection::Hooks,
                    AgentStateRevision::new(1, revision),
                    Harness::payload("race"),
                )
                .unwrap();
            loop {
                match harness.worker.admit() {
                    Ok(()) => break,
                    Err(AdmissionError::WorkerUnavailable) => continue,
                    Err(error) => panic!("unexpected admission error: {error:?}"),
                }
            }
            let outcome = harness.wait_outcome();
            assert_eq!(outcome.projections().len(), 1);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            lock_unpoisoned(&harness.state.calls)
                .iter()
                .filter(|(marker, _)| *marker == "race")
                .count(),
            40
        );
    }

    #[test]
    fn drop_closes_and_joins_worker() {
        let state = {
            let mut harness = Harness::new(Duration::from_secs(30));
            harness
                .worker
                .stage_projection(
                    AgentStateSection::Hooks,
                    AgentStateRevision::new(1, 1),
                    Harness::payload("drop"),
                )
                .unwrap();
            harness.worker.admit().unwrap();
            let _ = harness.wait_outcome();
            Arc::clone(&harness.state)
        };
        assert_eq!(state.dropped.load(Ordering::Acquire), 1);
    }

    #[test]
    fn debug_output_is_low_cardinality_and_payload_redacted() {
        let marker = "UNIQUE_SECRET_PATH_SESSION_MARKER";
        let continuation = ExactContinuation {
            operation_id: 9_999,
            kind: ExactKind::StructuredBatch { items: 1 },
            payload: Arc::new(TestPayload {
                marker,
                bytes: marker.len(),
            }),
            retained_bytes: marker.len(),
        };
        let debug = format!("{continuation:?}");
        assert!(!debug.contains(marker));
        assert!(!debug.contains("9999"));
        assert!(debug.contains("[REDACTED]"));

        let source = include_str!("agent_state_worker.rs")
            .split("\nmod tests {")
            .next()
            .unwrap();
        for forbidden in [
            "use storage::Db",
            "RuntimeCommand",
            "tokio::",
            "egui::",
            "request_repaint",
            "thread::sleep",
        ] {
            assert!(
                !source.contains(forbidden),
                "forbidden source edge: {forbidden}"
            );
        }
        assert!(source.contains("sync_channel(1)"));
        assert!(source.contains("recv_timeout(idle_ttl)"));
        assert!(source.contains("fn execute_job("));
        assert!(!source.contains("fn execute_exact("));
        assert!(!source.contains("fn execute_projection("));
    }

    #[test]
    fn lifecycle_final_check_is_serialized_with_sender() {
        let lifecycle = Arc::new(Mutex::new(WorkerLifecycle::Running));
        let barrier = Arc::new(Barrier::new(2));
        let sender_lifecycle = Arc::clone(&lifecycle);
        let sender_barrier = Arc::clone(&barrier);
        let sender = std::thread::spawn(move || {
            sender_barrier.wait();
            let state = lock_unpoisoned(&sender_lifecycle);
            *state
        });
        let mut state = lock_unpoisoned(&lifecycle);
        *state = WorkerLifecycle::Exited;
        barrier.wait();
        drop(state);
        assert!(matches!(sender.join().unwrap(), WorkerLifecycle::Exited));
    }
}
