#[path = "../src/lazy_worker.rs"]
mod lazy_worker;
#[path = "../src/panic_policy.rs"]
mod panic_policy;

use lazy_worker::{
    LazyBoundedWorker, LazyWorkerErrorCode, LazyWorkerOutcome, LazyWorkerSubmitError,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(2);
const PANIC_CHILD_ENV: &str = "DEPPY_TEST_SANITIZED_PANIC_CHILD";

fn receive_after_wake<J: Send + 'static, O: Send + 'static>(
    worker: &mut LazyBoundedWorker<J, O>,
    wake_rx: &mpsc::Receiver<()>,
) -> LazyWorkerOutcome<O> {
    wake_rx
        .recv_timeout(WAIT)
        .expect("worker must wake after publishing a result");
    worker
        .try_recv()
        .expect("published result must be ready when wake is observed")
}

#[test]
fn production_panic_hook_drops_payload_before_diagnostics() {
    const PRIVATE_MARKER: &str = "panic-payload-must-not-appear";
    if std::env::var_os(PANIC_CHILD_ENV).is_some() {
        panic_policy::install_sanitized_panic_hook();
        panic!("{PRIVATE_MARKER}");
    }

    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("production_panic_hook_drops_payload_before_diagnostics")
        .arg("--nocapture")
        .env(PANIC_CHILD_ENV, "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let mut diagnostics = output.stdout;
    diagnostics.extend(output.stderr);
    assert!(!String::from_utf8_lossy(&diagnostics).contains(PRIVATE_MARKER));
}

#[test]
fn production_main_installs_sanitized_hook_before_startup_work() {
    let source = include_str!("../src/main.rs");
    let hook = source
        .find("panic_policy::install_sanitized_panic_hook();")
        .unwrap();
    assert!(hook < source.find("bench::init_log()").unwrap());
    assert!(hook < source.find("paths::AppPaths::init()").unwrap());
    assert!(hook < source.find("init_logging(&paths)").unwrap());
}

#[test]
fn construction_starts_no_thread_executor_timer_or_wake() {
    let starts = Arc::new(AtomicUsize::new(0));
    let wakes = Arc::new(AtomicUsize::new(0));
    let starts_for_factory = Arc::clone(&starts);
    let wakes_for_callback = Arc::clone(&wakes);
    let mut worker = LazyBoundedWorker::<u64, u64>::new(
        "test-constructor-inert",
        Duration::from_millis(10),
        move || {
            starts_for_factory.fetch_add(1, Ordering::SeqCst);
            |job| job
        },
        move || {
            wakes_for_callback.fetch_add(1, Ordering::SeqCst);
        },
    );

    assert_eq!(starts.load(Ordering::SeqCst), 0);
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
    assert!(!worker.has_live_worker());
    assert_eq!(starts.load(Ordering::SeqCst), 0);
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
}

#[test]
fn first_request_starts_one_persistent_fn_mut_executor() {
    let starts = Arc::new(AtomicUsize::new(0));
    let starts_for_factory = Arc::clone(&starts);
    let (wake_tx, wake_rx) = mpsc::channel();
    let mut worker = LazyBoundedWorker::new(
        "test-persistent-executor",
        Duration::from_secs(1),
        move || {
            starts_for_factory.fetch_add(1, Ordering::SeqCst);
            let mut calls = 0_u64;
            move |job| {
                calls += 1;
                (job, calls, std::thread::current().name().map(str::to_owned))
            }
        },
        move || {
            let _ = wake_tx.send(());
        },
    );

    worker.try_request(10).unwrap();
    assert_eq!(
        receive_after_wake(&mut worker, &wake_rx).into_result(),
        Ok((10, 1, Some("test-persistent-executor".to_owned())))
    );
    worker.try_request(20).unwrap();
    assert_eq!(
        receive_after_wake(&mut worker, &wake_rx).into_result(),
        Ok((20, 2, Some("test-persistent-executor".to_owned())))
    );
    assert_eq!(starts.load(Ordering::SeqCst), 1);
}

#[test]
fn active_job_returns_exact_full_payload_until_outcome_is_consumed() {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let gate_for_factory = Arc::clone(&gate);
    let (started_tx, started_rx) = mpsc::channel();
    let (wake_tx, wake_rx) = mpsc::channel();
    let mut worker = LazyBoundedWorker::new(
        "test-job-bound",
        Duration::from_secs(1),
        move || {
            let gate = Arc::clone(&gate_for_factory);
            let started_tx = started_tx.clone();
            move |job| {
                let _ = started_tx.send(job);
                let (lock, changed) = &*gate;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = changed.wait(released).unwrap();
                }
                job
            }
        },
        move || {
            let _ = wake_tx.send(());
        },
    );

    worker.try_request(1_u64).unwrap();
    assert_eq!(started_rx.recv_timeout(WAIT).unwrap(), 1);
    let full = worker.try_request(2).unwrap_err();
    assert_eq!(full.error_code(), None);
    assert_eq!(full.into_job(), 2);

    let (lock, changed) = &*gate;
    *lock.lock().unwrap() = true;
    changed.notify_all();
    assert_eq!(
        receive_after_wake(&mut worker, &wake_rx).into_result(),
        Ok(1)
    );
    worker.try_request(2).unwrap();
    assert_eq!(
        receive_after_wake(&mut worker, &wake_rx).into_result(),
        Ok(2)
    );
}

#[test]
fn published_unread_outcome_keeps_aggregate_result_bound_at_one() {
    let (wake_tx, wake_rx) = mpsc::channel();
    let mut worker = LazyBoundedWorker::new(
        "test-published-backpressure",
        Duration::from_secs(1),
        || |job: u64| job + 10,
        move || {
            let _ = wake_tx.send(());
        },
    );

    worker.try_request(1).unwrap();
    wake_rx.recv_timeout(WAIT).unwrap();
    let full = worker.try_request(2).unwrap_err();
    assert_eq!(full.error_code(), None);
    assert_eq!(full.into_job(), 2);
    assert_eq!(worker.try_recv().unwrap().into_result(), Ok(11));

    worker.try_request(2).unwrap();
    assert_eq!(
        receive_after_wake(&mut worker, &wake_rx).into_result(),
        Ok(12)
    );
}

#[test]
fn consumed_result_during_wake_cannot_admit_into_retiring_generation() {
    let starts = Arc::new(AtomicUsize::new(0));
    let starts_for_factory = Arc::clone(&starts);
    let wake_gate = Arc::new((Mutex::new(false), Condvar::new()));
    let wake_gate_for_callback = Arc::clone(&wake_gate);
    let (wake_entered_tx, wake_entered_rx) = mpsc::sync_channel(1);
    let mut worker = LazyBoundedWorker::new(
        "test-publish-race",
        Duration::from_secs(1),
        move || {
            starts_for_factory.fetch_add(1, Ordering::SeqCst);
            |job: u64| job
        },
        move || {
            let _ = wake_entered_tx.send(());
            let (lock, changed) = &*wake_gate_for_callback;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = changed.wait(released).unwrap();
            }
        },
    );

    worker.try_request(1).unwrap();
    wake_entered_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(worker.try_recv().unwrap().into_result(), Ok(1));

    let gate_for_release = Arc::clone(&wake_gate);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(10));
        let (lock, changed) = &*gate_for_release;
        *lock.lock().unwrap() = true;
        changed.notify_all();
    });
    worker.try_request(2).unwrap();
    wake_entered_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(worker.try_recv().unwrap().into_result(), Ok(2));
    assert_eq!(starts.load(Ordering::SeqCst), 2);
}

#[test]
fn wake_observation_always_follows_result_publication() {
    let (wake_tx, wake_rx) = mpsc::sync_channel(1);
    let mut worker = LazyBoundedWorker::new(
        "test-wake-order",
        Duration::from_secs(1),
        || |job: u64| job + 1,
        move || {
            wake_tx.send(()).unwrap();
        },
    );

    worker.try_request(41).unwrap();
    wake_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(worker.try_recv().unwrap().into_result(), Ok(42));
}

#[test]
fn repeated_idle_exit_and_restart_races_never_lose_or_duplicate_jobs() {
    let starts = Arc::new(AtomicUsize::new(0));
    let starts_for_factory = Arc::clone(&starts);
    let (wake_tx, wake_rx) = mpsc::channel();
    let mut worker = LazyBoundedWorker::new(
        "test-idle-race",
        Duration::from_millis(2),
        move || {
            starts_for_factory.fetch_add(1, Ordering::SeqCst);
            |job| job
        },
        move || {
            let _ = wake_tx.send(());
        },
    );

    for job in 0_u64..64 {
        worker.try_request(job).unwrap();
        assert_eq!(
            receive_after_wake(&mut worker, &wake_rx).into_result(),
            Ok(job)
        );
        // Alternate around the deadline so submissions exercise reuse and idle-generation races.
        std::thread::sleep(if job % 2 == 0 {
            Duration::from_millis(3)
        } else {
            Duration::from_millis(1)
        });
    }
    assert!(starts.load(Ordering::SeqCst) > 1);
    assert!(starts.load(Ordering::SeqCst) <= 64);

    std::thread::sleep(Duration::from_millis(5));
    assert!(!worker.has_live_worker());
}

#[test]
fn drop_closes_channels_and_joins_with_published_unread_result() {
    let (executed_tx, executed_rx) = mpsc::channel();
    let (wake_tx, wake_rx) = mpsc::channel();
    let mut worker = LazyBoundedWorker::new(
        "test-drop-join",
        Duration::from_secs(30),
        move || {
            let executed_tx = executed_tx.clone();
            move |job| {
                let _ = executed_tx.send(job);
                job
            }
        },
        move || {
            let _ = wake_tx.send(());
        },
    );

    worker.try_request(1_u64).unwrap();
    assert_eq!(executed_rx.recv_timeout(WAIT).unwrap(), 1);
    wake_rx.recv_timeout(WAIT).unwrap();
    // Keep result 1 queued. Aggregate outstanding admission must reject any second job.
    let full = worker.try_request(2).unwrap_err();
    assert_eq!(full.into_job(), 2);

    let (dropped_tx, dropped_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        drop(worker);
        let _ = dropped_tx.send(());
    });
    dropped_rx
        .recv_timeout(WAIT)
        .expect("Drop must disconnect the blocked result send before join");
}

#[test]
fn worker_panic_publishes_static_error_and_waits_for_explicit_restart() {
    let starts = Arc::new(AtomicUsize::new(0));
    let panic_once = Arc::new(AtomicBool::new(true));
    let starts_for_factory = Arc::clone(&starts);
    let panic_for_factory = Arc::clone(&panic_once);
    let (wake_tx, wake_rx) = mpsc::channel();
    let mut worker = LazyBoundedWorker::new(
        "test-panic-recovery",
        Duration::from_millis(2),
        move || {
            starts_for_factory.fetch_add(1, Ordering::SeqCst);
            let panic_once = Arc::clone(&panic_for_factory);
            move |job| {
                if panic_once.swap(false, Ordering::SeqCst) {
                    panic!("injected_worker_panic");
                }
                job * 2
            }
        },
        move || {
            let _ = wake_tx.send(());
        },
    );

    worker.try_request(1_u64).unwrap();
    let failure = receive_after_wake(&mut worker, &wake_rx);
    assert_eq!(
        failure.error_code(),
        Some(LazyWorkerErrorCode::WorkerPanicked)
    );
    assert_eq!(
        failure.into_result(),
        Err(LazyWorkerErrorCode::WorkerPanicked)
    );
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    std::thread::sleep(Duration::from_millis(10));
    assert_eq!(
        starts.load(Ordering::SeqCst),
        1,
        "panic must not spin/restart"
    );

    worker.try_request(2).unwrap();
    assert_eq!(
        receive_after_wake(&mut worker, &wake_rx).into_result(),
        Ok(4)
    );
    assert_eq!(starts.load(Ordering::SeqCst), 2);
}

#[test]
fn debug_never_formats_jobs_or_outputs() {
    struct SecretJob(&'static str);
    struct SecretOutput(&'static str);

    let (wake_tx, wake_rx) = mpsc::channel();
    let mut worker = LazyBoundedWorker::new(
        "test-debug-output",
        Duration::from_secs(1),
        || |job: SecretJob| SecretOutput(job.0),
        move || {
            let _ = wake_tx.send(());
        },
    );
    worker.try_request(SecretJob("job-never-debug")).unwrap();
    let outcome = receive_after_wake(&mut worker, &wake_rx);
    let outcome_debug = format!("{outcome:?}");
    assert!(!outcome_debug.contains("job-never-debug"));
    let output = outcome.into_result().unwrap();
    assert_eq!(output.0, "job-never-debug");

    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let gate_for_factory = Arc::clone(&gate);
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let mut full_worker = LazyBoundedWorker::new(
        "test-debug-full",
        Duration::from_secs(1),
        move || {
            let gate = Arc::clone(&gate_for_factory);
            let started_tx = started_tx.clone();
            move |job: SecretJob| {
                let _ = started_tx.send(());
                let (lock, changed) = &*gate;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = changed.wait(released).unwrap();
                }
                SecretOutput(job.0)
            }
        },
        || {},
    );
    full_worker.try_request(SecretJob("first-secret")).unwrap();
    started_rx.recv_timeout(WAIT).unwrap();
    let full: LazyWorkerSubmitError<SecretJob> = full_worker
        .try_request(SecretJob("full-never-debug"))
        .unwrap_err();
    let full_debug = format!("{full:?}");
    assert!(!full_debug.contains("full-never-debug"));
    assert_eq!(full.into_job().0, "full-never-debug");
    let (lock, changed) = &*gate;
    *lock.lock().unwrap() = true;
    changed.notify_all();

    let worker_debug = format!("{worker:?}");
    assert!(!worker_debug.contains("job-never-debug"));
}

#[test]
fn error_codes_are_static_and_low_cardinality() {
    assert_eq!(
        LazyWorkerErrorCode::SpawnFailed.as_str(),
        "worker_spawn_failed"
    );
    assert_eq!(
        LazyWorkerErrorCode::WorkerPanicked.as_str(),
        "worker_panicked"
    );
    assert_eq!(
        LazyWorkerErrorCode::Disconnected.as_str(),
        "worker_disconnected"
    );
}
