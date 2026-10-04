//! App-owned bounded SQLite actor. Jobs never contain runtime handles or input callbacks.
//! Claim commits before App is allowed to dispatch an effect. Drop drains admitted DB jobs;
//! closing the result receiver releases publication without running any terminal input.
use agent_mcp::{Claim, History, Record};
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread::JoinHandle,
};

pub(crate) const MAX_JOBS: usize = 32;
pub(crate) const MAX_BYTES: usize = 4 * 1024 * 1024;
const SNAPSHOT_BYTES: usize = 3 * 1024 * 1024;
pub(crate) const MAX_ARGS: usize = 64 * 1024;
const RECEIPT_BYTES: usize = 2048;

pub(crate) enum Job {
    Open,
    Claim {
        id: String,
        tool: String,
        args: Value,
        workspace: String,
        session: String,
    },
    Finish {
        id: String,
        outcome: Value,
        message: String,
    },
    Recent,
}
impl Job {
    fn charge(&self) -> Option<usize> {
        Some(match self {
            Self::Open | Self::Recent => SNAPSHOT_BYTES,
            Self::Claim {
                id,
                tool,
                args,
                workspace,
                session,
            } => {
                let args = serde_json::to_vec(args).ok()?.len();
                if args > MAX_ARGS
                    || id.len() > 128
                    || tool.len() > 64
                    || workspace.len() > 128
                    || session.len() > 128
                {
                    return None;
                }
                args + id.len() + tool.len() + workspace.len() + session.len() + RECEIPT_BYTES
            }
            Self::Finish {
                id,
                outcome,
                message,
            } => {
                let receipt = serde_json::to_vec(outcome).ok()?.len();
                if id.len() > 128
                    || receipt > RECEIPT_BYTES
                    || message.len() > agent_mcp::MAX_ANSWER
                {
                    return None;
                }
                id.len() + receipt + message.len() + RECEIPT_BYTES
            }
        })
    }
}
pub(crate) enum ResultKind {
    Open(Result<Vec<Record>, ()>),
    Claim(Result<Claim, ()>),
    Finish(Result<(), ()>),
    Recent(Result<Vec<Record>, ()>),
}
pub(crate) struct Completion {
    pub token: u64,
    pub result: ResultKind,
    _charge: Charge,
}
struct Charge {
    bytes: usize,
    budget: Arc<AtomicUsize>,
}
impl Drop for Charge {
    fn drop(&mut self) {
        self.budget.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
struct Envelope {
    token: u64,
    job: Job,
    charge: Charge,
}

pub(crate) struct HistoryWorker {
    path: Option<PathBuf>,
    wake: Arc<dyn Fn() + Send + Sync>,
    jobs: Option<mpsc::SyncSender<Envelope>>,
    results: Option<mpsc::Receiver<Completion>>,
    handle: Option<JoinHandle<()>>,
    budget: Arc<AtomicUsize>,
    outstanding: usize,
}
impl HistoryWorker {
    pub(crate) fn new(path: Option<PathBuf>, wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            path,
            wake: Arc::new(wake),
            jobs: None,
            results: None,
            handle: None,
            budget: Arc::new(AtomicUsize::new(0)),
            outstanding: 0,
        }
    }
    pub(crate) fn request_wake(&self) {
        (self.wake)();
    }
    fn start(&mut self) -> Result<(), ()> {
        if self.jobs.is_some() {
            return Ok(());
        }
        let path = self.path.clone();
        let wake = Arc::clone(&self.wake);
        let (tx, rx) = mpsc::sync_channel::<Envelope>(MAX_JOBS);
        let (result_tx, results) = mpsc::sync_channel(MAX_JOBS);
        let handle = std::thread::Builder::new()
            .name("cloud-history".into())
            .spawn(move || {
                // Opening/migrating the database and every query are worker-owned.
                let mut db: Option<History> = None;
                while let Ok(envelope) = rx.recv() {
                    let Envelope { token, job, charge } = envelope;
                    if db.is_none() {
                        db = match &path {
                            Some(path) => History::open(path),
                            None => History::open_memory(),
                        }
                        .ok();
                    }
                    let result = match job {
                        Job::Open => ResultKind::Open(
                            db.as_ref()
                                .ok_or(())
                                .and_then(|db| db.recent_bounded(SNAPSHOT_BYTES).map_err(|_| ())),
                        ),
                        Job::Recent => ResultKind::Recent(
                            db.as_ref()
                                .ok_or(())
                                .and_then(|db| db.recent_bounded(SNAPSHOT_BYTES).map_err(|_| ())),
                        ),
                        Job::Claim {
                            id,
                            tool,
                            args,
                            workspace,
                            session,
                        } => ResultKind::Claim(db.as_ref().ok_or(()).and_then(|db| {
                            db.claim(&id, &tool, &args, &workspace, &session)
                                .map_err(|_| ())
                        })),
                        Job::Finish {
                            id,
                            outcome,
                            message,
                        } => ResultKind::Finish(
                            db.as_ref()
                                .ok_or(())
                                .and_then(|db| db.finish(&id, &outcome, &message).map_err(|_| ())),
                        ),
                    };
                    if result_tx
                        .send(Completion {
                            token,
                            result,
                            _charge: charge,
                        })
                        .is_ok()
                    {
                        wake();
                    }
                    // A closed receiver is shutdown, but queued finish jobs still drain durably.
                }
            })
            .map_err(|_| ())?;
        self.jobs = Some(tx);
        self.results = Some(results);
        self.handle = Some(handle);
        Ok(())
    }
    /// Known-unsent on any failure. At most 32 jobs/results together retain 4 MiB.
    pub(crate) fn request(&mut self, token: u64, job: Job) -> Result<(), Box<Job>> {
        let Some(bytes) = job.charge() else {
            return Err(Box::new(job));
        };
        if self.outstanding >= MAX_JOBS || self.start().is_err() {
            return Err(Box::new(job));
        }
        if self
            .budget
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used + bytes <= MAX_BYTES).then_some(used + bytes)
            })
            .is_err()
        {
            return Err(Box::new(job));
        }
        let envelope = Envelope {
            token,
            job,
            charge: Charge {
                bytes,
                budget: Arc::clone(&self.budget),
            },
        };
        match self.jobs.as_ref().unwrap().try_send(envelope) {
            Ok(()) => {
                self.outstanding += 1;
                Ok(())
            }
            Err(
                mpsc::TrySendError::Full(envelope) | mpsc::TrySendError::Disconnected(envelope),
            ) => Err(Box::new(envelope.job)),
        }
    }
    pub(crate) fn poll(&mut self) -> Option<Completion> {
        let result = self.results.as_ref()?.try_recv().ok()?;
        self.outstanding -= 1;
        Some(result)
    }
    #[cfg(test)]
    pub(crate) fn wait(&mut self) -> Option<Completion> {
        if self.outstanding == 0 {
            return None;
        }
        let result = self
            .results
            .as_ref()?
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("history actor completion deadline");
        self.outstanding -= 1;
        Some(result)
    }
    /// Only App shutdown may wait. Interactive callers use poll().
    pub(crate) fn drain_one(&mut self) -> Option<Completion> {
        if self.outstanding == 0 {
            return None;
        }
        let result = self.results.as_ref()?.recv().ok()?;
        self.outstanding -= 1;
        Some(result)
    }
    pub(crate) fn shutdown(&mut self) {
        self.jobs.take();
        self.results.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        self.outstanding = 0;
    }
    #[cfg(test)]
    pub(crate) fn retained_bytes(&self) -> usize {
        self.budget.load(Ordering::Acquire)
    }
}
impl Drop for HistoryWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!("deppy-pr7-actor-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        path
    }
    fn request(worker: &mut HistoryWorker, token: u64, job: Job) {
        assert!(
            worker.request(token, job).is_ok(),
            "known-unsent actor admission"
        );
    }
    #[test]
    fn pr7_actor_is_inert_then_opens_on_worker_and_wakes_after_publication() {
        let dir = dir();
        let path = dir.join("history.db");
        let (wake, rx) = mpsc::sync_channel(1);
        let mut worker = HistoryWorker::new(Some(path.clone()), move || {
            wake.try_send(()).unwrap();
        });
        assert!(!path.exists());
        assert!(worker.handle.is_none());
        request(&mut worker, 0, Job::Open);
        rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap();
        let completion = worker.poll().expect("result publication precedes wake");
        assert!(matches!(completion.result, ResultKind::Open(Ok(_))));
        drop(completion);
        assert_eq!(worker.retained_bytes(), 0);
        assert!(path.exists());
        drop(worker);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn pr7_actor_reserves_snapshot_bytes_until_completion_consumed_and_bounds_job_count() {
        let mut worker = HistoryWorker::new(None, || {});
        request(&mut worker, 0, Job::Open);
        let snapshot = worker.wait().unwrap();
        assert_eq!(worker.retained_bytes(), SNAPSHOT_BYTES);
        assert!(
            worker.request(1, Job::Recent).is_err(),
            "second snapshot bypassed result budget"
        );
        drop(snapshot);
        for token in 1..=MAX_JOBS as u64 {
            request(
                &mut worker,
                token,
                Job::Claim {
                    id: format!("budget-{token}"),
                    tool: "send_text".into(),
                    args: json!({"text":"hello"}),
                    workspace: "w".into(),
                    session: "s".into(),
                },
            );
        }
        assert!(
            worker
                .request(
                    100,
                    Job::Claim {
                        id: "over-count".into(),
                        tool: "send_text".into(),
                        args: json!({}),
                        workspace: "w".into(),
                        session: "s".into()
                    }
                )
                .is_err()
        );
        assert!(worker.retained_bytes() <= MAX_BYTES);
        for _ in 0..MAX_JOBS {
            assert!(matches!(
                worker.wait().unwrap().result,
                ResultKind::Claim(Ok(Claim::New))
            ));
        }
        assert_eq!(worker.retained_bytes(), 0);
    }
    #[test]
    fn pr7_actor_drop_drains_admitted_finish_without_any_input_callback() {
        let dir = dir();
        let path = dir.join("history.db");
        let mut worker = HistoryWorker::new(Some(path.clone()), || {});
        request(&mut worker, 0, Job::Open);
        drop(worker.wait().unwrap());
        let args = json!({"message":"last Grok answer"});
        request(
            &mut worker,
            1,
            Job::Claim {
                id: "last-answer".into(),
                tool: "notify".into(),
                args: args.clone(),
                workspace: "w".into(),
                session: "s".into(),
            },
        );
        assert!(matches!(
            worker.wait().unwrap().result,
            ResultKind::Claim(Ok(Claim::New))
        ));
        let receipt = json!({"status":"stored","operation_id":"last-answer"});
        request(
            &mut worker,
            2,
            Job::Finish {
                id: "last-answer".into(),
                outcome: receipt.clone(),
                message: "last Grok answer".into(),
            },
        );
        drop(worker);
        let db = History::open(&path).unwrap();
        assert_eq!(
            db.claim("last-answer", "notify", &args, "w", "s").unwrap(),
            Claim::Existing(receipt)
        );
        assert_eq!(db.recent().unwrap()[0].message, "last Grok answer");
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
