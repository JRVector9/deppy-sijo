use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProcessResourceSnapshot {
    pub pid: u32,
    pub sampled_at_ms: u64,
    pub rss_bytes: u64,
    /// CPU percent over the previous sample window. The first sample has no
    /// baseline and reports `None`.
    pub cpu_percent: Option<f32>,
    pub high_cpu: bool,
    pub high_rss: bool,
}

#[derive(Debug, Clone)]
pub struct ProcessResourceMonitorConfig {
    pub sample_interval: Duration,
    pub high_cpu_percent: f32,
    pub high_rss_bytes: u64,
}

impl Default for ProcessResourceMonitorConfig {
    fn default() -> Self {
        Self {
            sample_interval: Duration::from_secs(2),
            high_cpu_percent: 200.0,
            high_rss_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

pub struct ProcessResourceMonitor {
    config: ProcessResourceMonitorConfig,
    last_wall: Option<Instant>,
    last_cpu_seconds: Option<f64>,
    next_sample: Instant,
}

impl ProcessResourceMonitor {
    pub fn new(config: ProcessResourceMonitorConfig) -> Self {
        Self {
            config,
            last_wall: None,
            last_cpu_seconds: None,
            next_sample: Instant::now(),
        }
    }

    pub fn sample_if_due(&mut self) -> Option<ProcessResourceSnapshot> {
        let now = Instant::now();
        if now < self.next_sample {
            return None;
        }
        self.next_sample = now + self.config.sample_interval;
        Some(self.sample(now))
    }

    fn sample(&mut self, now: Instant) -> ProcessResourceSnapshot {
        let cpu_seconds = process_cpu_seconds();
        let cpu_percent = match (self.last_wall, self.last_cpu_seconds, cpu_seconds) {
            (Some(last_wall), Some(last_cpu), Some(cpu)) => {
                let elapsed = now.saturating_duration_since(last_wall).as_secs_f64();
                (elapsed > 0.0).then(|| (((cpu - last_cpu).max(0.0) / elapsed) * 100.0) as f32)
            }
            _ => None,
        };
        self.last_wall = Some(now);
        self.last_cpu_seconds = cpu_seconds;

        let rss_bytes = current_rss_bytes().unwrap_or(0);
        ProcessResourceSnapshot {
            pid: std::process::id(),
            sampled_at_ms: unix_ms(),
            rss_bytes,
            cpu_percent,
            high_cpu: cpu_percent.is_some_and(|cpu| cpu >= self.config.high_cpu_percent),
            high_rss: rss_bytes >= self.config.high_rss_bytes,
        }
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn process_cpu_seconds() -> Option<f64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    let usage = unsafe { usage.assume_init() };
    Some(timeval_seconds(usage.ru_utime) + timeval_seconds(usage.ru_stime))
}

#[cfg(unix)]
fn timeval_seconds(value: libc::timeval) -> f64 {
    value.tv_sec as f64 + value.tv_usec as f64 / 1_000_000.0
}

#[cfg(not(unix))]
fn process_cpu_seconds() -> Option<f64> {
    None
}

#[cfg(target_os = "linux")]
fn current_rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages = statm.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    Some(resident_pages.saturating_mul(page_size()))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn current_rss_bytes() -> Option<u64> {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let kib = text.trim().parse::<u64>().ok()?;
    Some(kib.saturating_mul(1024))
}

#[cfg(not(unix))]
fn current_rss_bytes() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn page_size() -> u64 {
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 { size as u64 } else { 4096 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_has_no_cpu_baseline() {
        let mut monitor = ProcessResourceMonitor::new(ProcessResourceMonitorConfig {
            sample_interval: Duration::ZERO,
            high_cpu_percent: 0.0,
            high_rss_bytes: u64::MAX,
        });
        let first = monitor.sample_if_due().unwrap();
        assert_eq!(first.pid, std::process::id());
        assert!(first.cpu_percent.is_none());
        let second = monitor.sample_if_due().unwrap();
        assert!(second.cpu_percent.is_some() || process_cpu_seconds().is_none());
    }

    #[test]
    fn high_rss_warning_uses_threshold() {
        let mut monitor = ProcessResourceMonitor::new(ProcessResourceMonitorConfig {
            sample_interval: Duration::ZERO,
            high_cpu_percent: f32::MAX,
            high_rss_bytes: 0,
        });
        let sample = monitor.sample_if_due().unwrap();
        assert!(sample.high_rss);
        assert!(!sample.high_cpu);
    }
}
