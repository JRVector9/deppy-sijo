use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::app::ProviderUsage;
use crate::lazy_worker::LazyBoundedWorker;

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const MAX_SNAPSHOT_BYTES: u64 = 4096;

#[derive(Clone, Copy, Debug, PartialEq)]
struct SnapshotUsage {
    usage: ProviderUsage,
    age_minutes: f64,
    measured_at: Instant,
}

impl SnapshotUsage {
    fn current(self) -> Option<ProviderUsage> {
        let usage = crate::app::fresh_usage_windows(
            self.usage,
            self.age_minutes + crate::app::usage_age_minutes(self.measured_at.elapsed()),
        );
        (usage.0.is_some() || usage.1.is_some()).then_some(usage)
    }
}

/// UI-owned cache. Construction is inert and all path resolution/file reads happen on its worker.
pub(crate) struct ClaudeUsageSnapshot {
    worker: LazyBoundedWorker<(), Option<SnapshotUsage>>,
    cached: Option<SnapshotUsage>,
    checked_at: Option<Instant>,
    pending: bool,
}

impl ClaudeUsageSnapshot {
    pub(crate) fn new(ctx: &egui::Context) -> Self {
        Self::with_loader(ctx, || {
            |_| {
                let path = crate::paths::home_dir()?
                    .join(".deppy-sijo")
                    .join("claude-usage.json");
                read_snapshot(&path)
            }
        })
    }

    fn with_loader<E>(ctx: &egui::Context, factory: impl Fn() -> E + Send + Sync + 'static) -> Self
    where
        E: FnMut(()) -> Option<SnapshotUsage> + Send + 'static,
    {
        let wake = ctx.clone();
        Self {
            worker: LazyBoundedWorker::new(
                "claude-usage-file",
                Duration::from_secs(30),
                factory,
                move || wake.request_repaint(),
            ),
            cached: None,
            checked_at: None,
            pending: false,
        }
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.pending
    }

    pub(crate) fn current(&mut self) -> Option<ProviderUsage> {
        if let Some(result) = self.worker.try_recv() {
            self.pending = false;
            self.cached = result.into_result().ok().flatten();
            // A slow failed read still gets a completed cache interval, allowing the CLI
            // fallback to run before a later refresh instead of immediately resubmitting.
            self.checked_at = Some(Instant::now());
        }
        if !self.pending
            && self
                .checked_at
                .is_none_or(|at| at.elapsed() >= REFRESH_INTERVAL)
        {
            self.checked_at = Some(Instant::now());
            self.pending = self.worker.try_request(()).is_ok();
        }
        self.cached?.current()
    }
}

fn read_snapshot(path: &Path) -> Option<SnapshotUsage> {
    (|| {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = options.open(path).ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || metadata.len() > MAX_SNAPSHOT_BYTES {
            return None;
        }
        // The same opened regular object supplies both metadata and bytes; growth is bounded too.
        let mut bytes = Vec::new();
        file.take(MAX_SNAPSHOT_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
            return None;
        }
        let snapshot: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        let percent = |name: &str| {
            snapshot
                .get(name)
                .and_then(serde_json::Value::as_f64)
                .filter(|value| value.is_finite())
                .map(|value| value.clamp(0.0, 100.0).round() as u8)
        };
        // mcp-proxy가 파일을 쓴 시각(unix seconds). 이 필드가 없는 옛 파일은 나이를
        // 알 길이 없으니 0분으로 보고 그대로 살린다.
        let age_minutes = snapshot
            .get("updated_at")
            .and_then(serde_json::Value::as_i64)
            .map_or(0.0, |updated_at| {
                let now = i64::try_from(deppy_core::time::unix_ms() / 1000).unwrap_or(i64::MAX);
                now.saturating_sub(updated_at).max(0) as f64 / 60.0
            });
        let (five_hour, seven_day) = crate::app::fresh_usage_windows(
            (percent("five_hour"), percent("seven_day")),
            age_minutes,
        );
        // mcp-proxy는 둘 중 하나만 읽혀도 파일을 쓴다(없는 쪽은 null). 한쪽이라도
        // 있으면 스냅샷으로 인정해야 나머지 한쪽이 화면에서 사라지지 않는다.
        (five_hour.is_some() || seven_day.is_some()).then_some(SnapshotUsage {
            usage: (five_hour, seven_day),
            age_minutes,
            measured_at: Instant::now(),
        })
    })()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    #[test]
    fn full_fix_20261014_usage_fifo_returns_without_writer() {
        use std::ffi::CString;
        use std::io::Write;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::OpenOptionsExt;
        let directory = std::env::temp_dir().join(format!(
            "deppy-usage-fifo-{}-{}",
            std::process::id(),
            deppy_core::time::unix_ms()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("usage.json");
        let cpath = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        let (tx, rx) = mpsc::sync_channel(1);
        let worker_path = path.clone();
        let started = Instant::now();
        let thread = std::thread::spawn(move || {
            let _ = tx.send(read_snapshot(&worker_path));
        });
        let completed = rx.recv_timeout(Duration::from_millis(150));
        // Rescue only this synthetic FIFO so a regression cannot strand the test process.
        let mut rescue = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        rescue.write_all(br#"{"five_hour":17}"#).unwrap();
        drop(rescue);
        thread.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        assert!(
            completed.is_ok(),
            "usage file blocked until a writer connected after {:?}",
            started.elapsed()
        );
        assert_eq!(completed.unwrap(), None, "a FIFO is not a usage snapshot");
    }

    #[test]
    fn full_fix_20261014_usage_regular_bounded_file() {
        let path = std::env::temp_dir().join(format!(
            "deppy-usage-normal-{}-{}",
            std::process::id(),
            deppy_core::time::unix_ms()
        ));
        std::fs::write(&path, br#"{"five_hour":17,"seven_day":null}"#).unwrap();
        assert_eq!(
            read_snapshot(&path).and_then(SnapshotUsage::current),
            Some((Some(17), None))
        );
        std::fs::write(&path, vec![b' '; 4097]).unwrap();
        assert_eq!(read_snapshot(&path), None);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn full_fix_20261014_usage_pending_worker_never_waits_on_frame() {
        use std::sync::{Arc, Condvar, Mutex};
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        struct Release(Arc<(Mutex<bool>, Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                let (lock, cv) = &*self.0;
                *lock.lock().unwrap() = true;
                cv.notify_all();
            }
        }
        let worker_gate = gate.clone();
        let ctx = egui::Context::default();
        let mut snapshot = ClaudeUsageSnapshot::with_loader(&ctx, move || {
            let gate = worker_gate.clone();
            move |_| {
                let (lock, cv) = &*gate;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = cv.wait(released).unwrap();
                }
                Some(SnapshotUsage {
                    usage: (Some(17), None),
                    age_minutes: 0.0,
                    measured_at: Instant::now(),
                })
            }
        });
        let release = Release(gate);
        assert!(!snapshot.worker.has_live_worker());
        let started = Instant::now();
        for _ in 0..100 {
            assert_eq!(snapshot.current(), None);
        }
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(snapshot.pending);
        drop(release);
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            if snapshot.current().is_some() {
                break;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(snapshot.current(), Some((Some(17), None)));
        assert!(
            !snapshot.pending,
            "normal reads retain the 2s cache instead of resubmitting each frame"
        );
    }
    #[test]
    fn full_fix_20261014_usage_cache_ages_from_original_snapshot_timestamp() {
        let path = std::env::temp_dir().join(format!(
            "deppy-usage-age-{}-{}",
            std::process::id(),
            deppy_core::time::unix_ms()
        ));
        let now = deppy_core::time::unix_ms() / 1000;
        std::fs::write(
            &path,
            format!(
                r#"{{"five_hour":17,"seven_day":31,"updated_at":{}}}"#,
                now - 299 * 60
            ),
        )
        .unwrap();
        let mut usage = read_snapshot(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        let mut cache = ClaudeUsageSnapshot::new(&egui::Context::default());
        cache.checked_at = Some(Instant::now());
        usage.measured_at = Instant::now() - Duration::from_secs(120);
        cache.cached = Some(usage);
        assert_eq!(
            cache.current(),
            Some((None, Some(31))),
            "old timestamp plus cache time expires each window independently"
        );
    }

    #[test]
    fn full_fix_20261014_usage_slow_missing_result_allows_cli_fallback() {
        let (published, ready) = mpsc::sync_channel(1);
        let mut snapshot = ClaudeUsageSnapshot::new(&egui::Context::default());
        snapshot.worker = LazyBoundedWorker::new(
            "usage-missing-test",
            Duration::from_secs(30),
            || |_| None,
            move || published.send(()).unwrap(),
        );
        assert_eq!(snapshot.current(), None);
        assert!(snapshot.is_pending());
        // Publication precedes wake, so the completed None is now available for consumption.
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        // Let the wake callback return and the worker resume admission; publishing backpressure
        // would otherwise accidentally hide the immediate resubmission being tested.
        std::thread::sleep(Duration::from_millis(20));
        snapshot.checked_at = Some(Instant::now() - REFRESH_INTERVAL - Duration::from_secs(1));
        assert_eq!(snapshot.current(), None);
        assert!(
            !snapshot.is_pending(),
            "a slow absent snapshot must leave a completed interval for the CLI fallback"
        );
    }
}
