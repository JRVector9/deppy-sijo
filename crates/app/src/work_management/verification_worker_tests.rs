use super::super::verification::{
    approve_local,
    tests::{fixture, spec},
};
use super::*;
#[test]
fn verification_worker_executes_literal_command_and_captures_exit_streams() {
    let dir = std::env::temp_dir();
    let state = fixture(&dir);
    let command = "printf 'actual stdout'; printf 'actual stderr' >&2";
    let approved = approve_local(
        &state,
        &state.tasks[0].id,
        ResultRevision(1),
        spec(&dir, command),
    )
    .unwrap();
    let evidence = run(approved, &RedactionService::new());
    assert_eq!(evidence.outcome, CommandVerificationOutcome::Passed);
    assert_eq!(evidence.stdout, "actual stdout");
    assert_eq!(evidence.stderr, "actual stderr");
    assert_eq!(evidence.exit_code, Some(0));
    assert_eq!(evidence.spec.command, command);
}

struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("deppy-verification-run-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn approve(&self, command: &str) -> ApprovedVerification {
        let state = fixture(&self.0);
        approve_local(
            &state,
            &state.tasks[0].id,
            ResultRevision(1),
            spec(&self.0, command),
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn verification_worker_distinguishes_nonzero_exit_and_spawn_failure() {
    let dir = Fixture::new();
    let evidence = run(
        dir.approve("printf failed >&2; exit 17"),
        &RedactionService::new(),
    );
    assert_eq!(evidence.outcome, CommandVerificationOutcome::NonzeroExit);
    assert_eq!(evidence.exit_code, Some(17));
    assert_eq!(evidence.stderr, "failed");
    let approved = dir.approve("printf should-not-run");
    std::fs::remove_dir(&dir.0).unwrap();
    let evidence = run(approved, &RedactionService::new());
    assert_eq!(evidence.outcome, CommandVerificationOutcome::SpawnFailed);
    assert_eq!(evidence.exit_code, None);
}

#[test]
fn verification_worker_redacts_before_capping_each_stream_and_drains_without_deadlock() {
    let dir = Fixture::new();
    let redaction = RedactionService::new();
    let synthetic = "registered-test-secret-value";
    redaction
        .register_permanent(&secret::SecretString::new(synthetic.into()))
        .unwrap();
    std::fs::write(dir.0.join("secret.txt"), synthetic).unwrap();
    let mut approved = dir.approve("cat secret.txt; cat secret.txt >&2; i=0; while [ \"$i\" -lt 4000 ]; do printf abcdefghijklmnop; printf abcdefghijklmnop >&2; i=$((i+1)); done");
    approved.initial.spec.output_limit_bytes = 128;
    let evidence = run(approved, &redaction);
    assert_eq!(evidence.outcome, CommandVerificationOutcome::Passed);
    assert!(evidence.stdout.len() <= 128 && evidence.stderr.len() <= 128);
    assert!(evidence.stdout_truncated && evidence.stderr_truncated);
    assert!(evidence.stdout.contains("[REDACTED]") && evidence.stderr.contains("[REDACTED]"));
    assert!(!evidence.stdout.contains(synthetic) && !evidence.stderr.contains(synthetic));
}

#[test]
fn verification_worker_timeout_cleans_background_descendants_and_returns_promptly() {
    let dir = Fixture::new();
    let mut approved = dir.approve("sleep 30 & printf '%s' \"$!\" > child.pid; wait");
    approved.initial.spec.timeout_ms = 80;
    let start = std::time::Instant::now();
    let evidence = run(approved, &RedactionService::new());
    assert_eq!(evidence.outcome, CommandVerificationOutcome::TimedOut);
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    assert_child_gone(&dir.0.join("child.pid"));
}
#[test]
fn verification_worker_success_cleans_children_after_the_shell_exits() {
    let dir = Fixture::new();
    let evidence = run(
        dir.approve("sleep 30 & printf '%s' \"$!\" > child.pid; exit 0"),
        &RedactionService::new(),
    );
    assert_eq!(evidence.outcome, CommandVerificationOutcome::Passed);
    assert_child_gone(&dir.0.join("child.pid"));
}
fn assert_child_gone(path: &std::path::Path) {
    let pid = std::fs::read_to_string(path)
        .unwrap()
        .parse::<u32>()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while unsafe { libc::kill(pid as i32, 0) } == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_ne!(
        unsafe { libc::kill(pid as i32, 0) },
        0,
        "owned descendant {pid} leaked"
    );
}

fn recv(worker: &mut VerificationWorker) -> VerificationResult {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = worker.try_recv() {
            return result.unwrap();
        }
        assert!(Instant::now() < deadline, "verification worker deadline");
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn wait_file(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !path.exists() {
        assert!(Instant::now() < deadline, "child did not start");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn verification_async_persists_actual_evidence_and_restart_load_never_reruns() {
    let dir = Fixture::new();
    let data = dir.0.join("journal");
    let mut worker = VerificationWorker::new(data.clone(), RedactionService::new(), || {});
    assert!(!data.exists(), "constructor must be inert");
    let approved = dir.approve("printf once >> runs.txt; printf actual");
    let workspace = approved.target().workspace_id.clone();
    worker.try_request(VerificationJob::Run(approved)).unwrap();
    let VerificationResult::Finished {
        evidence,
        cancelled,
    } = recv(&mut worker)
    else {
        panic!("command did not execute");
    };
    assert!(!cancelled);
    assert_eq!(evidence.outcome, CommandVerificationOutcome::Passed);
    let store = storage::work_management::CommandCenterStore::open(&data).unwrap();
    assert_eq!(
        store.load_verifications(&workspace).unwrap(),
        vec![VerificationEvidence::AppLocal((*evidence).clone())]
    );
    drop(store);
    drop(worker);
    let mut worker = VerificationWorker::new(data, RedactionService::new(), || {});
    worker
        .try_request(VerificationJob::Load {
            workspace_id: workspace,
            token: 42,
        })
        .unwrap();
    let VerificationResult::Loaded { token, result, .. } = recv(&mut worker) else {
        panic!("expected history");
    };
    assert_eq!(token, 42);
    assert_eq!(
        result.unwrap(),
        vec![VerificationEvidence::AppLocal(*evidence)]
    );
    assert_eq!(
        std::fs::read_to_string(dir.0.join("runs.txt")).unwrap(),
        "once"
    );
}

#[test]
fn verification_async_cancel_and_drop_clean_owned_process_groups() {
    let dir = Fixture::new();
    let data = dir.0.join("journal");
    let mut worker = VerificationWorker::new(data.clone(), RedactionService::new(), || {});
    let approved = dir.approve("sleep 30 & printf '%s' \"$!\" > child.pid; wait");
    let id = approved.id().clone();
    let workspace = approved.target().workspace_id.clone();
    worker.try_request(VerificationJob::Run(approved)).unwrap();
    wait_file(&dir.0.join("child.pid"));
    let store = storage::work_management::CommandCenterStore::open(&data).unwrap();
    assert!(
        matches!(&store.load_verifications(&workspace).unwrap()[..],[VerificationEvidence::AppLocal(e)] if e.outcome==CommandVerificationOutcome::Running)
    );
    drop(store);
    assert!(!worker.cancel(&VerificationId::new()));
    assert!(worker.cancel(&id));
    let VerificationResult::Finished {
        evidence,
        cancelled,
    } = recv(&mut worker)
    else {
        panic!("expected cancelled evidence");
    };
    assert!(cancelled);
    assert_eq!(evidence.outcome, CommandVerificationOutcome::Cancelled);
    assert_child_gone(&dir.0.join("child.pid"));
    std::fs::remove_file(dir.0.join("child.pid")).unwrap();
    let second = dir.approve("sleep 30 & printf '%s' \"$!\" > child.pid; wait");
    let second_workspace = second.target().workspace_id.clone();
    worker.try_request(VerificationJob::Run(second)).unwrap();
    wait_file(&dir.0.join("child.pid"));
    let started = Instant::now();
    drop(worker);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_child_gone(&dir.0.join("child.pid"));
    let store = storage::work_management::CommandCenterStore::open(&data).unwrap();
    assert!(store.load_verifications(&evidence.target.workspace_id).unwrap().iter().all(|e|matches!(e,VerificationEvidence::AppLocal(e) if e.outcome==CommandVerificationOutcome::Cancelled)));
    assert!(
        matches!(&store.load_verifications(&second_workspace).unwrap()[..],[VerificationEvidence::AppLocal(e)] if e.outcome==CommandVerificationOutcome::Cancelled)
    );
}

#[test]
fn verification_async_restore_interrupts_unfinished_claim_without_execution() {
    let dir = Fixture::new();
    let data = dir.0.join("journal");
    let approved = dir.approve("printf unauthorized-replay > replay.txt");
    let workspace = approved.target().workspace_id.clone();
    let mut store = storage::work_management::CommandCenterStore::open(&data).unwrap();
    store.start_command_verification(&approved.initial).unwrap();
    drop(store);
    let mut worker = VerificationWorker::new(data.clone(), RedactionService::new(), || {});
    worker
        .try_request(VerificationJob::Load {
            workspace_id: workspace.clone(),
            token: 7,
        })
        .unwrap();
    let VerificationResult::Loaded { result, .. } = recv(&mut worker) else {
        panic!("expected restored history");
    };
    let history = result.unwrap();
    assert!(
        matches!(&history[..],[VerificationEvidence::AppLocal(e)] if e.outcome==CommandVerificationOutcome::Interrupted)
    );
    assert!(!dir.0.join("replay.txt").exists());
    let store = storage::work_management::CommandCenterStore::open(&data).unwrap();
    assert_eq!(store.load_verifications(&workspace).unwrap(), history);
}

#[test]
fn verification_async_rejects_secret_command_before_persistence_and_execution() {
    let dir = Fixture::new();
    let data = dir.0.join("journal");
    let redaction = RedactionService::new();
    redaction
        .register_permanent(&secret::SecretString::new(
            "dummy-registered-secret-command".into(),
        ))
        .unwrap();
    let mut worker = VerificationWorker::new(data.clone(), redaction, || {});
    worker
        .try_request(VerificationJob::Run(dir.approve(
            "printf dummy-registered-secret-command > should-not-run.txt",
        )))
        .unwrap();
    assert!(matches!(
        recv(&mut worker),
        VerificationResult::ExecutionFailed {
            error: VerificationError::UnsafeMetadata,
            ..
        }
    ));
    assert!(!dir.0.join("should-not-run.txt").exists());
    assert!(!data.exists());
}

#[test]
fn verification_async_cancel_after_exit_keeps_evidence_but_blocks_application() {
    let dir = Fixture::new();
    let wake = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let signal = wake.clone();
    let mut worker =
        VerificationWorker::new(dir.0.join("journal"), RedactionService::new(), move || {
            signal.fetch_add(1, Ordering::SeqCst);
        });
    let approved = dir.approve("printf done");
    let id = approved.id().clone();
    worker.try_request(VerificationJob::Run(approved)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while wake.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(matches!(
        worker.try_request(VerificationJob::Run(dir.approve("printf duplicate"))),
        Err(LazyWorkerSubmitError::Full(_))
    ));
    assert!(worker.cancel(&id));
    let VerificationResult::Finished {
        evidence,
        cancelled,
    } = recv(&mut worker)
    else {
        panic!("expected exact completed evidence");
    };
    assert_eq!(evidence.outcome, CommandVerificationOutcome::Passed);
    assert!(cancelled);
}

#[test]
fn verification_async_manual_evidence_remains_redacted_and_separate_from_app_runs() {
    let dir = Fixture::new();
    let data = dir.0.join("journal");
    let redaction = RedactionService::new();
    redaction
        .register_permanent(&secret::SecretString::new(
            "synthetic-manual-secret-value".into(),
        ))
        .unwrap();
    let approved = dir.approve("printf must-not-execute > manual-ran.txt");
    let manual = ManualVerificationEvidence {
        id: VerificationId::new(),
        target: approved.target().clone(),
        recorded_at_ms: now_ms(),
        passed: true,
        evidence: "inspected synthetic-manual-secret-value manually".into(),
    };
    let mut worker = VerificationWorker::new(data.clone(), redaction, || {});
    worker
        .try_request(VerificationJob::RecordManual { evidence: manual })
        .unwrap();
    let VerificationResult::ManualRecorded { evidence, result } = recv(&mut worker) else {
        panic!("expected manual record");
    };
    result.unwrap();
    assert!(evidence.evidence.contains("[REDACTED]"));
    assert!(!dir.0.join("manual-ran.txt").exists());
    let store = storage::work_management::CommandCenterStore::open(&data).unwrap();
    assert_eq!(
        store
            .load_verifications(&evidence.target.workspace_id)
            .unwrap(),
        vec![VerificationEvidence::UserManual(evidence)]
    );
}

#[test]
fn verification_async_journal_failure_never_executes_the_command() {
    let dir = Fixture::new();
    let data = dir.0.join("journal");
    std::fs::create_dir(&data).unwrap();
    std::fs::write(
        data.join(storage::work_management::COMMAND_CENTER_FILE_NAME),
        b"not a valid sidecar",
    )
    .unwrap();
    let mut worker = VerificationWorker::new(data, RedactionService::new(), || {});
    worker
        .try_request(VerificationJob::Run(
            dir.approve("printf must-not-run > persistence-failed.txt"),
        ))
        .unwrap();
    assert!(matches!(
        recv(&mut worker),
        VerificationResult::ExecutionFailed {
            error: VerificationError::Persistence,
            ..
        }
    ));
    assert!(!dir.0.join("persistence-failed.txt").exists());
}
