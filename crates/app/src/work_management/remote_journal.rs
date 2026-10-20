//! Runtime callbacks only enqueue/poll. SQLite runs on this lazy bounded worker.
use crate::lazy_worker::{LazyBoundedWorker, LazyWorkerSubmitError};
use runtime::remote_worker::{
    RemoteWorkClaim, RemoteWorkJournal, RemoteWorkRequest, RemoteWorkResult,
};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use storage::work_management::{
    CommandCenterStore, RemoteOperationClaim, RemoteOperationIdentity, RemoteOperationResult,
};

#[derive(Clone)]
enum JobKind {
    Claim,
    Resolve(RemoteOperationResult),
}

#[cfg(all(test, unix))]
#[path = "remote_journal_tls_tests.rs"]
mod tls_tests;
struct Job {
    identity: RemoteOperationIdentity,
    kind: JobKind,
}
enum Reply {
    Claim(RemoteOperationClaim),
    Resolved(RemoteOperationResult),
}
struct Outcome {
    identity: RemoteOperationIdentity,
    reply: Result<Reply, ()>,
}
enum Status {
    QueuedClaim,
    Claimed(RemoteWorkClaim),
    Issued,
    QueuedResolve(RemoteWorkResult),
    Resolved(RemoteWorkResult),
    Failed,
}
struct Record {
    identity: RemoteOperationIdentity,
    status: Status,
}
struct State {
    worker: LazyBoundedWorker<Job, Outcome>,
    records: Vec<Record>,
}
pub(crate) struct AsyncRemoteJournal {
    state: Mutex<State>,
}
impl AsyncRemoteJournal {
    /// Inert construction. Pass InProcessRuntimeClient::remote_worker_wake to wake its poll loop.
    pub(crate) fn new(data_dir: PathBuf, wake: impl Fn() + Send + Sync + 'static) -> Self {
        let worker = LazyBoundedWorker::new(
            "command-center-admission",
            Duration::from_secs(20),
            move || {
                let data_dir = data_dir.clone();
                let mut store = None;
                move |job| execute(&data_dir, &mut store, job)
            },
            wake,
        );
        Self {
            state: Mutex::new(State {
                worker,
                records: vec![],
            }),
        }
    }
}
fn execute(data_dir: &Path, store: &mut Option<CommandCenterStore>, job: Job) -> Outcome {
    let result = (|| -> anyhow::Result<Reply> {
        if store.is_none() {
            *store = Some(CommandCenterStore::open(data_dir)?);
        }
        let store = store
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("journal unavailable"))?;
        match job.kind {
            JobKind::Claim => Ok(Reply::Claim(store.claim_remote_operation(&job.identity)?)),
            JobKind::Resolve(result) => {
                store.resolve_remote_operation(&job.identity, &result)?;
                Ok(Reply::Resolved(result))
            }
        }
    })();
    Outcome {
        identity: job.identity,
        reply: result.map_err(|_| ()),
    }
}
fn identity(request: &RemoteWorkRequest) -> anyhow::Result<RemoteOperationIdentity> {
    anyhow::ensure!(request.is_valid(), "remote journal request invalid");
    Ok(RemoteOperationIdentity {
        workspace_id: request.identity.workspace_id.clone(),
        device_id: request.identity.device_id.clone(),
        attempt_id: request.identity.attempt_id.clone(),
        operation_id: request.identity.operation_id.clone(),
        worker_incarnation: request.identity.incarnation.clone(),
        request_digest: request.digest()?,
        grant_epoch: request.identity.grant_epoch,
        lease_epoch: request.identity.lease_epoch,
    })
}
fn to_stored(result: &RemoteWorkResult) -> anyhow::Result<RemoteOperationResult> {
    Ok(match result {
        RemoteWorkResult::Launched {
            session,
            execution_generation,
        } => RemoteOperationResult::Launched {
            session_id: session.0,
            execution_generation: *execution_generation,
        },
        RemoteWorkResult::Rejected { code } => RemoteOperationResult::Rejected {
            code: serde_json::to_string(code)?,
        },
        RemoteWorkResult::ObservationUnknown => RemoteOperationResult::ObservationUnknown,
    })
}
fn from_stored(result: RemoteOperationResult) -> anyhow::Result<RemoteWorkResult> {
    Ok(match result {
        RemoteOperationResult::Launched {
            session_id,
            execution_generation,
        } => RemoteWorkResult::Launched {
            session: deppy_core::SessionId(session_id),
            execution_generation,
        },
        RemoteOperationResult::Rejected { code } => RemoteWorkResult::Rejected {
            code: serde_json::from_str(&code)?,
        },
        RemoteOperationResult::ObservationUnknown => RemoteWorkResult::ObservationUnknown,
    })
}
impl State {
    fn poll(&mut self) {
        for _ in 0..4 {
            let Some(outcome) = self.worker.try_recv() else {
                break;
            };
            match outcome.into_result() {
                Err(_) => {
                    for record in &mut self.records {
                        if matches!(
                            record.status,
                            Status::QueuedClaim | Status::QueuedResolve(_)
                        ) {
                            record.status = Status::Failed;
                        }
                    }
                }
                Ok(outcome) => {
                    if let Some(record) = self
                        .records
                        .iter_mut()
                        .find(|r| r.identity == outcome.identity)
                    {
                        record.status = match outcome.reply {
                            Err(()) => Status::Failed,
                            Ok(Reply::Claim(claim)) => match claim {
                                RemoteOperationClaim::New => Status::Claimed(RemoteWorkClaim::New),
                                RemoteOperationClaim::Conflict => {
                                    Status::Claimed(RemoteWorkClaim::Conflict)
                                }
                                RemoteOperationClaim::Existing(result) => match from_stored(result)
                                {
                                    Ok(result) => {
                                        Status::Claimed(RemoteWorkClaim::Existing(result))
                                    }
                                    Err(_) => Status::Failed,
                                },
                            },
                            Ok(Reply::Resolved(result)) => match from_stored(result) {
                                Ok(result) => Status::Resolved(result),
                                Err(_) => Status::Failed,
                            },
                        };
                    }
                }
            }
        }
    }
    fn submit(&mut self, job: Job) -> anyhow::Result<bool> {
        match self.worker.try_request(job) {
            Ok(()) => Ok(true),
            Err(LazyWorkerSubmitError::Full(_)) => Ok(false),
            Err(LazyWorkerSubmitError::Unavailable { .. }) => {
                anyhow::bail!("remote journal worker unavailable")
            }
        }
    }
    fn find(&self, identity: &RemoteOperationIdentity) -> Option<usize> {
        self.records.iter().position(|r| {
            r.identity.workspace_id == identity.workspace_id
                && (r.identity.operation_id == identity.operation_id
                    || r.identity.attempt_id == identity.attempt_id)
        })
    }
}
impl RemoteWorkJournal for AsyncRemoteJournal {
    fn claim(&self, request: &RemoteWorkRequest) -> anyhow::Result<RemoteWorkClaim> {
        let identity = identity(request)?;
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(RemoteWorkClaim::Pending),
            Err(_) => anyhow::bail!("remote journal state unavailable"),
        };
        state.poll();
        if let Some(index) = state.find(&identity) {
            let record = &mut state.records[index];
            if record.identity != identity {
                return Ok(RemoteWorkClaim::Conflict);
            }
            return match &record.status {
                Status::QueuedClaim => Ok(RemoteWorkClaim::Pending),
                Status::Claimed(claim) => {
                    let claim = claim.clone();
                    match &claim {
                        RemoteWorkClaim::New => record.status = Status::Issued,
                        RemoteWorkClaim::Existing(result) => {
                            record.status = Status::Resolved(result.clone())
                        }
                        _ => {}
                    }
                    Ok(claim)
                }
                Status::Issued | Status::QueuedResolve(_) => Ok(RemoteWorkClaim::Existing(
                    RemoteWorkResult::ObservationUnknown,
                )),
                Status::Resolved(result) => Ok(RemoteWorkClaim::Existing(result.clone())),
                Status::Failed => anyhow::bail!("remote journal operation unavailable"),
            };
        }
        anyhow::ensure!(state.records.len() < 256, "remote journal full");
        if state.submit(Job {
            identity: identity.clone(),
            kind: JobKind::Claim,
        })? {
            state.records.push(Record {
                identity,
                status: Status::QueuedClaim,
            });
        }
        Ok(RemoteWorkClaim::Pending)
    }
    fn resolve(
        &self,
        request: &RemoteWorkRequest,
        result: &RemoteWorkResult,
    ) -> anyhow::Result<bool> {
        let identity = identity(request)?;
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(false),
            Err(_) => anyhow::bail!("remote journal state unavailable"),
        };
        state.poll();
        let index = state
            .find(&identity)
            .ok_or_else(|| anyhow::anyhow!("remote journal claim missing"))?;
        anyhow::ensure!(
            state.records[index].identity == identity,
            "remote journal identity conflict"
        );
        match &state.records[index].status {
            Status::Resolved(previous) => {
                anyhow::ensure!(previous == result, "remote journal result conflict");
                Ok(true)
            }
            Status::QueuedResolve(previous) => {
                anyhow::ensure!(previous == result, "remote journal result conflict");
                Ok(false)
            }
            Status::Issued => {
                if state.submit(Job {
                    identity,
                    kind: JobKind::Resolve(to_stored(result)?),
                })? {
                    state.records[index].status = Status::QueuedResolve(result.clone());
                }
                Ok(false)
            }
            _ => anyhow::bail!("remote journal not admitted"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deppy_core::WorkspaceId;
    use deppy_core::work_management::{AttemptId, OperationId, RuntimeIncarnationId};
    use runtime::remote_worker::RemoteWorkIdentity;
    use std::time::{Duration, Instant};
    fn request() -> RemoteWorkRequest {
        RemoteWorkRequest {
            identity: RemoteWorkIdentity {
                device_id: "worker-a".into(),
                workspace_id: WorkspaceId("workspace-a".into()),
                attempt_id: AttemptId("attempt-a".into()),
                operation_id: OperationId("operation-a".into()),
                incarnation: RuntimeIncarnationId("runtime-a".into()),
                grant_epoch: 1,
                lease_epoch: 1,
            },
            folder: "/fixture/a".into(),
            folder_anchor: "anchor-a".into(),
            profile_id: "profile-revision-1".into(),
            initial_prompt: None,
        }
    }
    fn committed_claim(
        journal: &AsyncRemoteJournal,
        request: &RemoteWorkRequest,
    ) -> RemoteWorkClaim {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let claim = journal.claim(request).unwrap();
            if claim != RemoteWorkClaim::Pending {
                return claim;
            }
            assert!(Instant::now() < until, "journal never committed");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn async_remote_journal_waits_off_thread_and_restores_uncertain_without_replay() {
        let root =
            std::env::temp_dir().join(format!("deppy-async-journal-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("metadata.sqlite3"), b"legacy fixture").unwrap();
        drop(storage::work_management::CommandCenterStore::open(&root).unwrap());
        let blocker = rusqlite::Connection::open(
            root.join(storage::work_management::COMMAND_CENTER_FILE_NAME),
        )
        .unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let journal = AsyncRemoteJournal::new(root.clone(), || {});
        let request = request();
        assert_eq!(journal.claim(&request).unwrap(), RemoteWorkClaim::Pending);
        assert_eq!(journal.claim(&request).unwrap(), RemoteWorkClaim::Pending);
        blocker.execute_batch("ROLLBACK").unwrap();
        drop(blocker);
        assert_eq!(committed_claim(&journal, &request), RemoteWorkClaim::New);
        drop(journal);
        let journal = AsyncRemoteJournal::new(root.clone(), || {});
        assert_eq!(
            committed_claim(&journal, &request),
            RemoteWorkClaim::Existing(RemoteWorkResult::ObservationUnknown)
        );
        assert_eq!(
            std::fs::read(root.join("metadata.sqlite3")).unwrap(),
            b"legacy fixture"
        );
        drop(journal);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn async_remote_journal_commits_exact_result_before_acknowledging_and_reopens_it() {
        let root = std::env::temp_dir().join(format!(
            "deppy-async-journal-result-{}",
            uuid::Uuid::new_v4()
        ));
        let journal = AsyncRemoteJournal::new(root.clone(), || {});
        let request = request();
        assert_eq!(committed_claim(&journal, &request), RemoteWorkClaim::New);
        let result = RemoteWorkResult::Launched {
            session: deppy_core::SessionId(7),
            execution_generation: 3,
        };
        assert!(!journal.resolve(&request, &result).unwrap());
        assert!(
            journal
                .resolve(&request, &RemoteWorkResult::ObservationUnknown)
                .is_err()
        );
        let until = Instant::now() + Duration::from_secs(5);
        while !journal.resolve(&request, &result).unwrap() {
            assert!(Instant::now() < until, "result never committed");
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(journal);
        let journal = AsyncRemoteJournal::new(root.clone(), || {});
        assert_eq!(
            committed_claim(&journal, &request),
            RemoteWorkClaim::Existing(result.clone())
        );
        assert!(journal.resolve(&request, &result).unwrap());
        let mut changed = request.clone();
        changed.initial_prompt = Some("different immutable request".into());
        assert_eq!(journal.claim(&changed).unwrap(), RemoteWorkClaim::Conflict);
        drop(journal);
        std::fs::remove_dir_all(root).unwrap();
    }
}
