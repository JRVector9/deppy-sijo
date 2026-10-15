

#[cfg(test)]
mod review_shutdown_20261015 {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    struct LockGuard(Option<rusqlite::Connection>);
    impl LockGuard {
        fn release(&mut self) {
            if let Some(connection) = self.0.take() {
                // Connection Drop also rolls back if this explicit release fails.
                let _ = connection.execute_batch("ROLLBACK");
            }
        }
    }
    impl Drop for LockGuard {
        fn drop(&mut self) { self.release(); }
    }

    fn stale_shutdown_fixture(hold_external_lock: bool) -> (bool, usize, u128) {
        let dir = std::env::temp_dir().join(format!("deppy-review-stale-shutdown-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("cloud.db");
        let completions = Arc::new(AtomicUsize::new(0));
        let observed = completions.clone();
        let mut bridge = CloudAgent::with_worker(
            crate::cloud_history_worker::HistoryWorker::new(Some(path.clone()), move || {
                observed.fetch_add(1, Ordering::SeqCst);
            }),
            secret::RedactionService::new(),
        );
        bridge.drain_history(|_, _| panic!("shutdown fixture never types"));
        assert!(bridge.history_ready);
        let run = CloudRun::new(
            "stale-run", agent_mcp::LEGACY_CONNECTION_ID, "Legacy", "workspace", "session", "g1", "Inspect", "", now() as i64 - 120,
        ).unwrap();
        let token = bridge.operation_token();
        assert!(bridge.history.request(token, Job::SaveRun { run: run.clone() }).is_ok());
        assert!(matches!(bridge.history.wait().unwrap().result, ResultKind::SaveRun(Ok(()))));
        bridge.install_cloud_run(run);
        assert!(bridge.management.pending.is_empty(), "no initial outstanding management jobs");
        assert_eq!(bridge.history.retained_bytes(), 0, "no initial outstanding actor jobs");
        assert!(bridge.management_poll_after().is_some());
        let external = rusqlite::Connection::open(&path).unwrap();
        let mut lock = if hold_external_lock {
            external.execute_batch("BEGIN EXCLUSIVE").unwrap();
            LockGuard(Some(external))
        } else {
            drop(external);
            LockGuard(None)
        };
        let baseline_completions = completions.load(Ordering::SeqCst);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let started = Instant::now();
        let worker = std::thread::spawn(move || {
            bridge.shutdown();
            let _ = done_tx.send(());
        });
        // Only one stale run exists, and its first GetRun has a 100ms SQLite
        // busy timeout. Give finite draining seven times that budget. Always
        // release the fixture lock and join before asserting on the baseline.
        let ended_before_release = done_rx.recv_timeout(Duration::from_millis(700)).is_ok();
        let completions_before_release = completions.load(Ordering::SeqCst).saturating_sub(baseline_completions);
        lock.release();
        worker.join().unwrap();
        let elapsed_ms = started.elapsed().as_millis();
        std::fs::remove_dir_all(&dir).unwrap();
        eprintln!("stale-shutdown: lock={hold_external_lock}, ended_before_release={ended_before_release}, completions_before_release={completions_before_release}, total_elapsed_ms={elapsed_ms}");
        (ended_before_release, completions_before_release, elapsed_ms)
    }

    #[test]
    fn review_20261015_shutdown_unlocked_stale_run_finishes() {
        let (ended, _, elapsed) = stale_shutdown_fixture(false);
        assert!(ended, "unlocked control must finish within 700ms, elapsed={elapsed}ms");
    }

    #[test]
    fn review_20261015_shutdown_does_not_reenqueue_failed_stale_reads_forever() {
        let (ended, repeated_completions, _) = stale_shutdown_fixture(true);
        assert!(ended, "shutdown kept creating failed stale read jobs after close began ({repeated_completions} completions while the external lock remained held)");
    }
}
