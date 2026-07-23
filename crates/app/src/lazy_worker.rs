//! Standard-library-only lazy bounded worker used by app-owned adapters.
//!
//! Construction stores closure ports and a duration only. It creates no thread, channel, timer,
//! persistence/keyring handle, or repaint. The executor factory itself must therefore be an
//! I/O-free constructor: it should only build a `FnMut(J) -> O`. That persistent executor may open
//! a DB/keyring handle on its first invocation and retain it for reuse until idle TTL or `Drop`
//! destroys the worker generation.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

type Executor<J, O> = dyn FnMut(J) -> O + Send + 'static;
type ExecutorFactory<J, O> = dyn Fn() -> Box<Executor<J, O>> + Send + Sync + 'static;
type CompletionWake = dyn Fn() + Send + Sync + 'static;

/// Static, low-cardinality infrastructure failures. Job execution errors belong in `O`; this type
/// intentionally cannot carry raw errors, paths, identifiers, or job/output data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LazyWorkerErrorCode {
    SpawnFailed,
    WorkerPanicked,
    Disconnected,
}

impl LazyWorkerErrorCode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::SpawnFailed => "worker_spawn_failed",
            Self::WorkerPanicked => "worker_panicked",
            Self::Disconnected => "worker_disconnected",
        }
    }
}

/// One bounded result. Its `Debug` implementation exposes status only, never `O`.
pub(crate) struct LazyWorkerOutcome<O> {
    result: Result<O, LazyWorkerErrorCode>,
}

impl<O> LazyWorkerOutcome<O> {
    pub(crate) fn into_result(self) -> Result<O, LazyWorkerErrorCode> {
        self.result
    }

    pub(crate) fn error_code(&self) -> Option<LazyWorkerErrorCode> {
        self.result.as_ref().err().copied()
    }
}

impl<O> std::fmt::Debug for LazyWorkerOutcome<O> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let status = self
            .error_code()
            .map_or("completed", LazyWorkerErrorCode::as_str);
        formatter
            .debug_struct("LazyWorkerOutcome")
            .field("status", &status)
            .finish()
    }
}

/// Non-blocking admission failure. Both variants return the exact known-unsent job to the caller.
pub(crate) enum LazyWorkerSubmitError<J> {
    Full(J),
    Unavailable { job: J, code: LazyWorkerErrorCode },
}

impl<J> LazyWorkerSubmitError<J> {
    pub(crate) const fn error_code(&self) -> Option<LazyWorkerErrorCode> {
        match self {
            Self::Full(_) => None,
            Self::Unavailable { code, .. } => Some(*code),
        }
    }

    pub(crate) fn into_job(self) -> J {
        match self {
            Self::Full(job) | Self::Unavailable { job, .. } => job,
        }
    }
}

impl<J> std::fmt::Debug for LazyWorkerSubmitError<J> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let status = match self {
            Self::Full(_) => "full",
            Self::Unavailable { code, .. } => code.as_str(),
        };
        formatter
            .debug_struct("LazyWorkerSubmitError")
            .field("status", &status)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerLifecycle {
    Running,
    Publishing,
    Exited,
    Stopping,
}

struct WorkerSlot<J, O> {
    jobs: mpsc::SyncSender<J>,
    results: mpsc::Receiver<LazyWorkerOutcome<O>>,
    lifecycle: Arc<Mutex<WorkerLifecycle>>,
    handle: JoinHandle<()>,
}

enum AdmitOnceError<J> {
    Full(J),
    SpawnFailed(J),
    Disconnected(J),
}

/// One-thread, one-job-channel, one-result-channel execution primitive.
///
/// `try_request` is the only spawn point. A request that races an idle exit remains known-unsent:
/// the lifecycle check and `try_send` share the same mutex as the worker's final empty check. Such
/// a disconnected request is admitted to one fresh worker at most once. A `Full` request is never
/// retried or replaced. At most one request is outstanding across execution, channel publication,
/// idle retirement, and caller consumption; the next request remains `Full` until that exact
/// outcome is consumed. Executor panics publish one static failure result and stop the generation;
/// recovery requires a later explicit request.
pub(crate) struct LazyBoundedWorker<J: Send + 'static, O: Send + 'static> {
    thread_name: &'static str,
    idle_ttl: Duration,
    factory: Arc<ExecutorFactory<J, O>>,
    wake: Arc<CompletionWake>,
    slot: Option<WorkerSlot<J, O>>,
    pending_outcome: Option<LazyWorkerOutcome<O>>,
    outstanding: bool,
}

impl<J: Send + 'static, O: Send + 'static> LazyBoundedWorker<J, O> {
    /// Creates an inert worker. `thread_name` must be a fixed low-cardinality diagnostic label, not
    /// an identifier derived from a job/user/resource. It is stored without allocation and used by
    /// `Builder` only at lazy spawn. `executor_factory` is stored, not invoked. It must construct
    /// only the persistent `FnMut` executor and perform no I/O; defer DB/keyring opening to that
    /// executor's first call so an unused worker owns no external resource.
    pub(crate) fn new<E>(
        thread_name: &'static str,
        idle_ttl: Duration,
        executor_factory: impl Fn() -> E + Send + Sync + 'static,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self
    where
        E: FnMut(J) -> O + Send + 'static,
    {
        let factory: Arc<ExecutorFactory<J, O>> = Arc::new(move || Box::new(executor_factory()));
        Self {
            thread_name,
            idle_ttl,
            factory,
            wake: Arc::new(wake),
            slot: None,
            pending_outcome: None,
            outstanding: false,
        }
    }

    /// Attempts one non-blocking admission. There is no automatic work retry: only a job proven
    /// unsent by a disconnected channel is tried once on a fresh worker generation.
    pub(crate) fn try_request(&mut self, job: J) -> Result<(), LazyWorkerSubmitError<J>> {
        self.reap_finished();
        if self.outstanding {
            return Err(LazyWorkerSubmitError::Full(job));
        }

        let job = match self.try_admit_once(job) {
            Ok(()) => {
                self.outstanding = true;
                return Ok(());
            }
            Err(AdmitOnceError::Full(job)) => {
                return Err(LazyWorkerSubmitError::Full(job));
            }
            Err(AdmitOnceError::SpawnFailed(job)) => {
                return Err(LazyWorkerSubmitError::Unavailable {
                    job,
                    code: LazyWorkerErrorCode::SpawnFailed,
                });
            }
            Err(AdmitOnceError::Disconnected(job)) => job,
        };

        // The disconnected send did not transfer ownership to a receiver. Join/reap that exact
        // generation before making the sole fresh-worker re-admission attempt.
        self.retire_slot();
        if self.pending_outcome.is_some() {
            return Err(LazyWorkerSubmitError::Full(job));
        }
        match self.try_admit_once(job) {
            Ok(()) => {
                self.outstanding = true;
                Ok(())
            }
            Err(AdmitOnceError::Full(job)) => Err(LazyWorkerSubmitError::Full(job)),
            Err(AdmitOnceError::SpawnFailed(job)) => Err(LazyWorkerSubmitError::Unavailable {
                job,
                code: LazyWorkerErrorCode::SpawnFailed,
            }),
            Err(AdmitOnceError::Disconnected(job)) => {
                self.retire_slot();
                Err(LazyWorkerSubmitError::Unavailable {
                    job,
                    code: LazyWorkerErrorCode::Disconnected,
                })
            }
        }
    }

    /// Receives at most one result without waiting or polling. Idle-exited worker handles are
    /// joined before their queued result is returned.
    pub(crate) fn try_recv(&mut self) -> Option<LazyWorkerOutcome<O>> {
        self.reap_finished();
        if self.pending_outcome.is_some() {
            return self.take_pending_outcome();
        }
        let received = self.slot.as_ref().map(|slot| slot.results.try_recv())?;
        match received {
            Ok(outcome) => {
                self.outstanding = false;
                Some(outcome)
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.retire_slot();
                self.take_pending_outcome()
            }
        }
    }

    /// Lifecycle probe for integration diagnostics/tests. It performs only a non-blocking finished
    /// check and joins solely when the standard library reports the handle finished.
    #[cfg(test)]
    pub(crate) fn has_live_worker(&mut self) -> bool {
        self.reap_finished();
        self.slot.is_some()
    }

    fn try_admit_once(&mut self, job: J) -> Result<(), AdmitOnceError<J>> {
        if self.slot.is_none() && self.spawn_slot().is_err() {
            return Err(AdmitOnceError::SpawnFailed(job));
        }
        let slot = self.slot.as_ref().expect("worker slot spawned above");
        let lifecycle = lock_unpoisoned(&slot.lifecycle);
        if *lifecycle != WorkerLifecycle::Running {
            return Err(AdmitOnceError::Disconnected(job));
        }
        match slot.jobs.try_send(job) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(job)) => Err(AdmitOnceError::Full(job)),
            Err(mpsc::TrySendError::Disconnected(job)) => Err(AdmitOnceError::Disconnected(job)),
        }
    }

    fn spawn_slot(&mut self) -> Result<(), LazyWorkerErrorCode> {
        debug_assert!(self.slot.is_none());
        let (jobs, job_rx) = mpsc::sync_channel(1);
        let (result_tx, results) = mpsc::sync_channel(1);
        let lifecycle = Arc::new(Mutex::new(WorkerLifecycle::Running));
        let worker_lifecycle = Arc::clone(&lifecycle);
        let factory = Arc::clone(&self.factory);
        let wake = Arc::clone(&self.wake);
        let idle_ttl = self.idle_ttl;
        let handle = std::thread::Builder::new()
            .name(self.thread_name.to_owned())
            .spawn(move || {
                run_worker(job_rx, result_tx, worker_lifecycle, idle_ttl, factory, wake);
            })
            .map_err(|_| LazyWorkerErrorCode::SpawnFailed)?;
        self.slot = Some(WorkerSlot {
            jobs,
            results,
            lifecycle,
            handle,
        });
        Ok(())
    }

    fn reap_finished(&mut self) {
        let finished = self
            .slot
            .as_ref()
            .is_some_and(|slot| slot.handle.is_finished());
        if finished {
            self.retire_slot();
        }
    }

    fn retire_slot(&mut self) {
        let Some(slot) = self.slot.take() else {
            return;
        };
        let WorkerSlot {
            jobs,
            results,
            lifecycle: _,
            handle,
        } = slot;
        drop(jobs);
        let panicked = handle.join().is_err();
        let outcome = results.try_recv().ok();
        drop(results);
        if self.pending_outcome.is_none() {
            self.pending_outcome = outcome.or_else(|| {
                panicked.then_some(LazyWorkerOutcome {
                    result: Err(LazyWorkerErrorCode::WorkerPanicked),
                })
            });
            if self.pending_outcome.is_some() {
                self.outstanding = true;
            }
        }
    }

    fn take_pending_outcome(&mut self) -> Option<LazyWorkerOutcome<O>> {
        let outcome = self.pending_outcome.take()?;
        self.outstanding = false;
        Some(outcome)
    }
}

impl<J: Send + 'static, O: Send + 'static> std::fmt::Debug for LazyBoundedWorker<J, O> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LazyBoundedWorker")
            .field("has_worker", &self.slot.is_some())
            .field("has_pending_outcome", &self.pending_outcome.is_some())
            .field("has_outstanding", &self.outstanding)
            .finish()
    }
}

impl<J: Send + 'static, O: Send + 'static> Drop for LazyBoundedWorker<J, O> {
    fn drop(&mut self) {
        let Some(slot) = self.slot.take() else {
            return;
        };
        *lock_unpoisoned(&slot.lifecycle) = WorkerLifecycle::Stopping;
        let WorkerSlot {
            jobs,
            results,
            lifecycle: _,
            handle,
        } = slot;
        // Closing the result receiver before joining releases a worker blocked on its bounded
        // result send. Closing jobs releases one blocked in recv_timeout without waiting for TTL.
        drop(results);
        drop(jobs);
        let _ = handle.join();
    }
}

struct WorkerExitGuard {
    lifecycle: Arc<Mutex<WorkerLifecycle>>,
}

impl Drop for WorkerExitGuard {
    fn drop(&mut self) {
        *lock_unpoisoned(&self.lifecycle) = WorkerLifecycle::Exited;
    }
}

fn run_worker<J: Send + 'static, O: Send + 'static>(
    jobs: mpsc::Receiver<J>,
    results: mpsc::SyncSender<LazyWorkerOutcome<O>>,
    lifecycle: Arc<Mutex<WorkerLifecycle>>,
    idle_ttl: Duration,
    factory: Arc<ExecutorFactory<J, O>>,
    wake: Arc<CompletionWake>,
) {
    let _exit = WorkerExitGuard {
        lifecycle: Arc::clone(&lifecycle),
    };
    let mut executor: Option<Box<Executor<J, O>>> = None;
    loop {
        let Some(job) = receive_job(&jobs, &lifecycle, idle_ttl) else {
            return;
        };
        let (outcome, stop_after_publication) = match executor.as_mut() {
            Some(executor) => execute_job(executor.as_mut(), job),
            None => match catch_unwind(AssertUnwindSafe(|| factory())) {
                Ok(created) => execute_job(executor.insert(created).as_mut(), job),
                Err(_) => (
                    LazyWorkerOutcome {
                        result: Err(LazyWorkerErrorCode::WorkerPanicked),
                    },
                    true,
                ),
            },
        };

        {
            let mut worker_lifecycle = lock_unpoisoned(&lifecycle);
            if *worker_lifecycle != WorkerLifecycle::Running {
                return;
            }
            *worker_lifecycle = if stop_after_publication {
                WorkerLifecycle::Exited
            } else {
                WorkerLifecycle::Publishing
            };
        }
        if results.send(outcome).is_err() {
            return;
        }
        // Publication happens-before wake. A broken wake callback cannot make the worker spin.
        let wake_panicked = catch_unwind(AssertUnwindSafe(|| wake())).is_err();
        if stop_after_publication || wake_panicked {
            return;
        }
        let mut worker_lifecycle = lock_unpoisoned(&lifecycle);
        match *worker_lifecycle {
            WorkerLifecycle::Publishing => *worker_lifecycle = WorkerLifecycle::Running,
            WorkerLifecycle::Stopping | WorkerLifecycle::Exited => return,
            WorkerLifecycle::Running => {}
        }
    }
}

fn execute_job<J, O>(executor: &mut Executor<J, O>, job: J) -> (LazyWorkerOutcome<O>, bool) {
    match catch_unwind(AssertUnwindSafe(|| executor(job))) {
        Ok(output) => (LazyWorkerOutcome { result: Ok(output) }, false),
        Err(_) => (
            LazyWorkerOutcome {
                result: Err(LazyWorkerErrorCode::WorkerPanicked),
            },
            true,
        ),
    }
}

fn receive_job<J>(
    jobs: &mpsc::Receiver<J>,
    lifecycle: &Arc<Mutex<WorkerLifecycle>>,
    idle_ttl: Duration,
) -> Option<J> {
    match jobs.recv_timeout(idle_ttl) {
        Ok(job) => {
            if *lock_unpoisoned(lifecycle) == WorkerLifecycle::Running {
                Some(job)
            } else {
                None
            }
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => None,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // This final empty check shares the exact lock held across sender try_send. Either the
            // old worker receives the job, or the sender observes Exited and owns the unsent job.
            let mut worker_lifecycle = lock_unpoisoned(lifecycle);
            if *worker_lifecycle != WorkerLifecycle::Running {
                return None;
            }
            match jobs.try_recv() {
                Ok(job) => Some(job),
                Err(mpsc::TryRecvError::Disconnected) => None,
                Err(mpsc::TryRecvError::Empty) => {
                    *worker_lifecycle = WorkerLifecycle::Exited;
                    None
                }
            }
        }
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}
