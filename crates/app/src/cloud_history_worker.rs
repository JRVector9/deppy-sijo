//! App-owned bounded SQLite actor. Jobs never contain runtime handles or input callbacks.
//! Claim commits before App is allowed to dispatch an effect. Drop drains admitted DB jobs;
//! closing the result receiver releases publication without running any terminal input.
use agent_mcp::{
    Claim, CloudRun, CompletionIdentity, CompletionReceipt, History, HistoryPage, HistoryQuery,
    OperationContext, Record, RunPage, RunQuery,
};
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
const COMPLETION_RECEIPT_BYTES: usize = 4096;

pub(crate) enum Job {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "Legacy worker compatibility fixtures")
    )]
    Open,
    OpenPage,
    RecentPage,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "Legacy worker compatibility fixtures")
    )]
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
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "Legacy worker compatibility fixtures")
    )]
    Recent,
    ClaimScoped {
        id: String,
        tool: String,
        args: Value,
        workspace: String,
        session: String,
        context: OperationContext,
    },
    FinishScoped {
        id: String,
        connection_id: String,
        outcome: Value,
        message: String,
    },
    ClaimCompletion {
        tool: String,
        args: Value,
        identity: CompletionIdentity,
    },
    FinishCompletion {
        identity: CompletionIdentity,
        receipt: Box<CompletionReceipt>,
    },
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "Legacy worker compatibility fixtures")
    )]
    Lookup {
        id: String,
        connection_id: String,
    },
    LookupTarget {
        id: String,
        connection_id: String,
        workspace: String,
        session: String,
        generation: String,
    },
    SaveRun {
        run: CloudRun,
    },
    GetRun {
        id: String,
        connection_id: String,
    },
    ListRuns {
        query: RunQuery,
    },
    // Additive actor-scoped paging API; the desktop currently uses snapshot pages.
    #[allow(dead_code)]
    Query {
        query: HistoryQuery,
    },
}
impl Job {
    fn charge(&self) -> Option<usize> {
        Some(match self {
            Self::Open | Self::Recent | Self::OpenPage | Self::RecentPage => SNAPSHOT_BYTES,
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
            Self::ClaimScoped {
                id,
                tool,
                args,
                workspace,
                session,
                context,
            } => {
                context.validate().ok()?;
                let args = serde_json::to_vec(args).ok()?.len();
                if args > MAX_ARGS
                    || id != &context.operation_id
                    || tool.len() > 64
                    || workspace.len() > 128
                    || session.len() > 128
                {
                    return None;
                }
                args + tool.len()
                    + workspace.len()
                    + session.len()
                    + serde_json::to_vec(context).ok()?.len()
                    + id.len()
                    + RECEIPT_BYTES
                    + 1024
            }
            Self::FinishScoped {
                id,
                connection_id,
                outcome,
                message,
            } => {
                agent_mcp::operation_key(connection_id, id).ok()?;
                let receipt = serde_json::to_vec(outcome).ok()?.len();
                if receipt > RECEIPT_BYTES || message.len() > agent_mcp::MAX_ANSWER {
                    return None;
                }
                id.len() + connection_id.len() + receipt + message.len() + RECEIPT_BYTES + 1024
            }
            Self::ClaimCompletion {
                tool,
                args,
                identity,
            } => {
                identity.validate().ok()?;
                let args = serde_json::to_vec(args).ok()?.len();
                if args > MAX_ARGS || tool.len() > 64 {
                    return None;
                }
                args + tool.len() + identity.encoded_len().ok()? + RECEIPT_BYTES + 1024
            }
            Self::FinishCompletion { identity, receipt } => {
                identity.validate().ok()?;
                if identity != &receipt.identity {
                    return None;
                }
                let bytes = serde_json::to_vec(receipt).ok()?.len();
                if bytes > COMPLETION_RECEIPT_BYTES {
                    return None;
                }
                receipt.validate().ok()?;
                identity.encoded_len().ok()? + bytes + 1024
            }
            Self::Lookup { id, connection_id } => {
                agent_mcp::operation_key(connection_id, id).ok()?;
                id.len() + connection_id.len() + RECEIPT_BYTES + 1024
            }
            Self::LookupTarget {
                id,
                connection_id,
                workspace,
                session,
                generation,
            } => {
                agent_mcp::operation_key(connection_id, id).ok()?;
                if [workspace, session, generation].into_iter().any(|value| {
                    value.is_empty() || value.len() > 128 || value.chars().any(char::is_control)
                }) {
                    return None;
                }
                id.len()
                    + connection_id.len()
                    + workspace.len()
                    + session.len()
                    + generation.len()
                    + RECEIPT_BYTES
                    + 1024
            }
            Self::SaveRun { run } => {
                run.validate().ok()?;
                serde_json::to_vec(run).ok()?.len() + 1024
            }
            Self::GetRun { id, connection_id } => {
                if [id, connection_id].into_iter().any(|value| {
                    value.is_empty() || value.len() > 128 || value.chars().any(char::is_control)
                }) {
                    return None;
                }
                id.len() + connection_id.len() + agent_mcp::MAX_RUN_BYTES + 1024
            }
            Self::ListRuns { query } => {
                query.validate().ok()?;
                query.max_bytes + serde_json::to_vec(query).ok()?.len() + 1024
            }
            Self::Query { query } => {
                query.validate().ok()?;
                query.max_bytes + serde_json::to_vec(query).ok()?.len() + 1024
            }
        })
    }
}
pub(crate) enum ResultKind {
    Open(Result<Vec<Record>, ()>),
    OpenPage(Result<HistoryPage, ()>),
    RecentPage(Result<HistoryPage, ()>),
    Claim(Result<Claim, ()>),
    Finish(Result<(), ()>),
    Recent(Result<Vec<Record>, ()>),
    ClaimScoped(Result<Claim, ()>),
    FinishScoped(Result<(), ()>),
    ClaimCompletion(Result<Claim, ()>),
    FinishCompletion(Result<(), ()>),
    Lookup(Result<Option<Value>, ()>),
    LookupTarget(Result<Option<Value>, ()>),
    SaveRun(Result<(), ()>),
    GetRun(Result<Option<Box<CloudRun>>, ()>),
    ListRuns(Result<RunPage, ()>),
    Query(Result<HistoryPage, ()>),
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
                        Job::OpenPage => ResultKind::OpenPage(
                            db.as_ref()
                                .ok_or(())
                                .and_then(|db| db.recent_page(SNAPSHOT_BYTES).map_err(|_| ())),
                        ),
                        Job::RecentPage => ResultKind::RecentPage(
                            db.as_ref()
                                .ok_or(())
                                .and_then(|db| db.recent_page(SNAPSHOT_BYTES).map_err(|_| ())),
                        ),
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
                        Job::ClaimScoped {
                            id,
                            tool,
                            args,
                            workspace,
                            session,
                            context,
                        } => ResultKind::ClaimScoped(db.as_ref().ok_or(()).and_then(|db| {
                            db.claim_scoped(&id, &tool, &args, &workspace, &session, &context)
                                .map_err(|_| ())
                        })),
                        Job::FinishScoped {
                            id,
                            connection_id,
                            outcome,
                            message,
                        } => ResultKind::FinishScoped(db.as_ref().ok_or(()).and_then(|db| {
                            db.finish_scoped(&id, &connection_id, &outcome, &message)
                                .map_err(|_| ())
                        })),
                        Job::ClaimCompletion {
                            tool,
                            args,
                            identity,
                        } => ResultKind::ClaimCompletion(db.as_ref().ok_or(()).and_then(|db| {
                            db.claim_completion_scoped(&tool, &args, &identity)
                                .map_err(|_| ())
                        })),
                        Job::FinishCompletion { identity, receipt } => {
                            ResultKind::FinishCompletion(db.as_ref().ok_or(()).and_then(|db| {
                                db.finish_completion(&identity, &receipt).map_err(|_| ())
                            }))
                        }
                        Job::Lookup { id, connection_id } => {
                            ResultKind::Lookup(db.as_ref().ok_or(()).and_then(|db| {
                                db.get_operation(&id, &connection_id).map_err(|_| ())
                            }))
                        }
                        Job::LookupTarget {
                            id,
                            connection_id,
                            workspace,
                            session,
                            generation,
                        } => ResultKind::LookupTarget(db.as_ref().ok_or(()).and_then(|db| {
                            db.get_operation_for_target(
                                &id,
                                &connection_id,
                                &workspace,
                                &session,
                                &generation,
                            )
                            .map_err(|_| ())
                        })),
                        Job::SaveRun { run } => ResultKind::SaveRun(
                            db.as_ref()
                                .ok_or(())
                                .and_then(|db| db.save_run(&run).map_err(|_| ())),
                        ),
                        Job::GetRun { id, connection_id } => ResultKind::GetRun(
                            db.as_ref()
                                .ok_or(())
                                .and_then(|db| db.get_run(&connection_id, &id).map_err(|_| ()))
                                .map(|run| run.map(Box::new)),
                        ),
                        Job::ListRuns { query } => ResultKind::ListRuns(
                            db.as_ref()
                                .ok_or(())
                                .and_then(|db| db.list_runs(&query).map_err(|_| ())),
                        ),
                        Job::Query { query } => ResultKind::Query(
                            db.as_ref()
                                .ok_or(())
                                .and_then(|db| db.query(&query).map_err(|_| ())),
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
    fn completion_identity() -> CompletionIdentity {
        CompletionIdentity {
            connection_id: "completion-actor".into(),
            transport_session_id: uuid::Uuid::new_v4().to_string(),
            stream_id: uuid::Uuid::new_v4().to_string(),
            request_id: json!(42),
            auth_epoch: 1,
            scope: agent_mcp::CompletionScope {
                workspace: "completion-workspace".into(),
                session_id: "completion-session".into(),
                generation: "completion-generation".into(),
                operation_id: "completion-operation".into(),
                run_id: Some("completion-run".into()),
            },
        }
    }
    fn completion_receipt(identity: &CompletionIdentity) -> CompletionReceipt {
        let now = deppy_core::time::unix_secs_i64() as u64;
        CompletionReceipt {
            identity: identity.clone(),
            event_id: identity.event_id(),
            admission: json!({"status":"submitted","operation_id":identity.scope.operation_id}),
            outcome: json!({"status":"completed","reason":"native_turn_verified"}),
            created_at: now,
            completed_at: Some(now),
            expires_at: now + agent_mcp::COMPLETION_RECEIPT_TTL,
        }
    }
    #[test]
    fn completion_history_worker_claims_and_finishes_private_original_identity() {
        let directory = dir();
        let path = directory.join("completion-history.db");
        let mut worker = HistoryWorker::new(Some(path.clone()), || {});
        let identity = completion_identity();
        let args = json!({"text":"isolated completion fixture","submit":true});
        let receipt = completion_receipt(&identity);
        request(
            &mut worker,
            1,
            Job::ClaimCompletion {
                tool: "send_text".into(),
                args: args.clone(),
                identity: identity.clone(),
            },
        );
        assert!(matches!(
            worker.wait().unwrap().result,
            ResultKind::ClaimCompletion(Ok(Claim::New))
        ));
        let mut foreign = identity.clone();
        foreign.transport_session_id = uuid::Uuid::new_v4().to_string();
        request(
            &mut worker,
            2,
            Job::ClaimCompletion {
                tool: "send_text".into(),
                args,
                identity: foreign.clone(),
            },
        );
        assert!(matches!(
            worker.wait().unwrap().result,
            ResultKind::ClaimCompletion(Err(()))
        ));
        request(
            &mut worker,
            3,
            Job::FinishCompletion {
                identity: identity.clone(),
                receipt: Box::new(receipt.clone()),
            },
        );
        assert!(matches!(
            worker.wait().unwrap().result,
            ResultKind::FinishCompletion(Ok(()))
        ));
        request(
            &mut worker,
            4,
            Job::LookupTarget {
                id: identity.scope.operation_id.clone(),
                connection_id: identity.connection_id.clone(),
                workspace: identity.scope.workspace.clone(),
                session: identity.scope.session_id.clone(),
                generation: identity.scope.generation.clone(),
            },
        );
        let completion = worker.wait().unwrap();
        let ResultKind::LookupTarget(Ok(Some(public))) = &completion.result else {
            panic!("public operation receipt");
        };
        assert_eq!(
            public, &receipt.admission,
            "public lookup remains admission-only"
        );
        drop(completion);
        assert_eq!(worker.retained_bytes(), 0);
        drop(worker);
        let db = History::open(&path).unwrap();
        assert_eq!(db.get_completion(&identity).unwrap(), Some(receipt));
        assert_eq!(db.get_completion(&foreign).unwrap(), None);
        drop(db);
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn completion_history_worker_rejects_invalid_and_oversized_jobs_before_queue() {
        let identity = completion_identity();
        let receipt = completion_receipt(&identity);
        let claim = |identity, tool: &str, args| Job::ClaimCompletion {
            identity,
            tool: tool.into(),
            args,
        };
        let finish = |identity, receipt| Job::FinishCompletion {
            identity,
            receipt: Box::new(receipt),
        };
        let mut invalid_identity = identity.clone();
        invalid_identity.auth_epoch = 0;
        let mut mismatched_receipt = receipt.clone();
        mismatched_receipt.identity.request_id = json!(43);
        let mut invalid_receipt = receipt.clone();
        invalid_receipt.event_id.push('x');
        let mut oversized_receipt = receipt.clone();
        oversized_receipt.outcome = json!({"status":"unknown","message":"x".repeat(4096)});
        for job in [
            claim(invalid_identity.clone(), "send_text", json!({})),
            claim(identity.clone(), &"t".repeat(65), json!({})),
            claim(
                identity.clone(),
                "send_text",
                json!({"text":"x".repeat(MAX_ARGS)}),
            ),
            finish(invalid_identity, receipt.clone()),
            finish(identity.clone(), mismatched_receipt),
            finish(identity.clone(), invalid_receipt),
            finish(identity, oversized_receipt),
        ] {
            let mut worker = HistoryWorker::new(None, || {});
            assert!(
                worker.request(1, job).is_err(),
                "invalid completion job admitted"
            );
            assert_eq!(worker.retained_bytes(), 0);
            assert_eq!(worker.outstanding, 0);
            assert!(
                worker.handle.is_none(),
                "invalid job opened the database worker"
            );
        }
    }
    #[test]
    fn completion_history_worker_charges_full_jobs_until_result_consumed() {
        let identity = completion_identity();
        let receipt = completion_receipt(&identity);
        let args = json!({"text":"x".repeat(8192)});
        let mut worker = HistoryWorker::new(None, || {});
        let retained_claim = serde_json::to_vec(&args).unwrap().len()
            + "send_text".len()
            + identity.encoded_len().unwrap();
        request(
            &mut worker,
            1,
            Job::ClaimCompletion {
                tool: "send_text".into(),
                args,
                identity: identity.clone(),
            },
        );
        assert!(worker.retained_bytes() >= retained_claim);
        let result = worker.wait().unwrap();
        assert!(
            worker.retained_bytes() >= retained_claim,
            "result released its reservation early"
        );
        drop(result);
        assert_eq!(worker.retained_bytes(), 0);
        let retained_finish =
            identity.encoded_len().unwrap() + serde_json::to_vec(&receipt).unwrap().len();
        request(
            &mut worker,
            2,
            Job::FinishCompletion {
                identity,
                receipt: Box::new(receipt),
            },
        );
        assert!(worker.retained_bytes() >= retained_finish);
        let result = worker.wait().unwrap();
        assert!(worker.retained_bytes() >= retained_finish);
        drop(result);
        assert_eq!(worker.retained_bytes(), 0);
    }
    #[test]
    fn cloud_management_actor_scopes_receipts_and_persists_runs_without_effects() {
        let mut worker = HistoryWorker::new(None, || {});
        let context = agent_mcp::OperationContext {
            connection_id: "actor-a".into(),
            operation_id: "same-id".into(),
            run_id: Some("run".into()),
            generation: "g".into(),
        };
        request(
            &mut worker,
            1,
            Job::ClaimScoped {
                id: "same-id".into(),
                tool: "notify".into(),
                args: json!({"message":"answer"}),
                workspace: "w".into(),
                session: "s".into(),
                context,
            },
        );
        assert!(matches!(
            worker.wait().unwrap().result,
            ResultKind::ClaimScoped(Ok(Claim::New))
        ));
        request(
            &mut worker,
            2,
            Job::FinishScoped {
                id: "same-id".into(),
                connection_id: "actor-a".into(),
                outcome: json!({"status":"stored"}),
                message: "answer".into(),
            },
        );
        assert!(matches!(
            worker.wait().unwrap().result,
            ResultKind::FinishScoped(Ok(()))
        ));
        request(
            &mut worker,
            3,
            Job::Lookup {
                id: "same-id".into(),
                connection_id: "actor-b".into(),
            },
        );
        assert!(matches!(
            worker.wait().unwrap().result,
            ResultKind::Lookup(Ok(None))
        ));
        let run = agent_mcp::CloudRun::new(
            "run",
            "actor-a",
            "A",
            "w",
            "s",
            "g",
            "title",
            "instruction",
            100,
        )
        .unwrap();
        request(&mut worker, 4, Job::SaveRun { run });
        assert!(matches!(
            worker.wait().unwrap().result,
            ResultKind::SaveRun(Ok(()))
        ));
        request(
            &mut worker,
            5,
            Job::ListRuns {
                query: agent_mcp::RunQuery::default(),
            },
        );
        let result = worker.wait().unwrap();
        let ResultKind::ListRuns(Ok(page)) = &result.result else {
            panic!("run page");
        };
        assert_eq!(page.runs.len(), 1);
        assert_eq!(page.runs[0].state, agent_mcp::RunState::Queued);
        drop(result);
        request(
            &mut worker,
            6,
            Job::GetRun {
                id: "run".into(),
                connection_id: "actor-a".into(),
            },
        );
        assert!(matches!(
            worker.wait().unwrap().result,
            ResultKind::GetRun(Ok(Some(_)))
        ));
        request(
            &mut worker,
            7,
            Job::GetRun {
                id: "run".into(),
                connection_id: "actor-b".into(),
            },
        );
        assert!(matches!(
            worker.wait().unwrap().result,
            ResultKind::GetRun(Ok(None))
        ));
        assert_eq!(worker.retained_bytes(), 0);
    }
    #[test]
    fn history_page_snapshot_keeps_valid_newlines_and_reports_retained_memory_truncation() {
        let mut worker = HistoryWorker::new(None, || {});
        let mut token = 0;
        for (count, tool) in [(100, "notify"), (300, "send_text")] {
            for index in 0..count {
                let id = format!("{tool}-{index}");
                request(
                    &mut worker,
                    token,
                    Job::Claim {
                        id: id.clone(),
                        tool: tool.into(),
                        args: json!({}),
                        workspace: "w".into(),
                        session: "s".into(),
                    },
                );
                token += 1;
                assert!(matches!(
                    worker.wait().unwrap().result,
                    ResultKind::Claim(Ok(Claim::New))
                ));
                request(
                    &mut worker,
                    token,
                    Job::Finish {
                        id,
                        outcome: json!({"status":"stored"}),
                        message: "\n".repeat(agent_mcp::MAX_ANSWER),
                    },
                );
                token += 1;
                assert!(matches!(
                    worker.wait().unwrap().result,
                    ResultKind::Finish(Ok(()))
                ));
            }
            let job = if tool == "notify" {
                Job::OpenPage
            } else {
                Job::RecentPage
            };
            request(&mut worker, token, job);
            token += 1;
            let snapshot = worker.wait().unwrap();
            let page = match &snapshot.result {
                ResultKind::OpenPage(Ok(page)) | ResultKind::RecentPage(Ok(page)) => page,
                _ => panic!("valid snapshots must produce a page"),
            };
            assert_eq!(worker.retained_bytes(), SNAPSHOT_BYTES);
            assert_eq!(
                page.records
                    .iter()
                    .filter(|row| row.tool == "notify")
                    .count(),
                100
            );
            assert!(
                page.records
                    .iter()
                    .all(|row| row.message.len() == agent_mcp::MAX_ANSWER)
            );
            assert_eq!(page.truncated, tool != "notify");
            if page.truncated {
                assert!(page.records.len() < 400);
            } else {
                assert_eq!(page.records.len(), 100);
            }
            assert!(worker.request(token, Job::RecentPage).is_err());
            drop(snapshot);
            assert_eq!(worker.retained_bytes(), 0);
        }
    }
    #[test]
    fn cloud_management_actor_rejects_oversized_context_run_and_query_before_admission() {
        let mut worker = HistoryWorker::new(None, || {});
        let mut run = agent_mcp::CloudRun::new(
            "run",
            "actor-a",
            "A",
            "w",
            "s",
            "g",
            "title",
            "instruction",
            100,
        )
        .unwrap();
        run.instruction = "x".repeat(8193);
        assert!(worker.request(1, Job::SaveRun { run }).is_err());
        assert!(
            worker
                .request(
                    2,
                    Job::ListRuns {
                        query: agent_mcp::RunQuery {
                            limit: 101,
                            ..agent_mcp::RunQuery::default()
                        }
                    }
                )
                .is_err()
        );
        assert!(
            worker
                .request(
                    3,
                    Job::Lookup {
                        id: "id".into(),
                        connection_id: "x".repeat(129)
                    }
                )
                .is_err()
        );
        assert_eq!(worker.retained_bytes(), 0);
        assert!(worker.handle.is_none(), "invalid jobs must not open any DB");
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
