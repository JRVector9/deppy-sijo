use std::time::{Duration, Instant};

use deppy_core::SessionId;
use deppy_core::time::unix_ms;
use pty::{ProcessIdentity, ProcessIdentitySource};

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProcessResourceSnapshot {
    pub pid: u32,
    pub sampled_at_ms: u64,
    /// 앱 프로세스 메모리. macOS는 phys_footprint(활성 상태 보기 '메모리' 열과 동일),
    /// 그 외 플랫폼은 RSS — 필드명은 wire 호환을 위해 유지한다.
    pub rss_bytes: u64,
    /// CPU percent over the previous sample window. The first sample has no
    /// baseline and reports `None`.
    pub cpu_percent: Option<f32>,
    pub high_cpu: bool,
    pub high_rss: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionResourceUsage {
    pub session: SessionId,
    pub pid: Option<u32>,
    pub process_group: Option<u32>,
    pub identity_source: ProcessIdentitySource,
    pub sampled_at_ms: u64,
    pub process_count: usize,
    pub rss_bytes: u64,
    pub cpu_percent: Option<f32>,
    pub high_cpu: bool,
    pub high_rss: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct SessionResourceTarget {
    pub session: SessionId,
    pub identity: ProcessIdentity,
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
    /// 마지막으로 발행한 스냅샷 — 변화 게이트 기준.
    last_emitted: Option<ProcessResourceSnapshot>,
    last_emitted_sessions: Vec<SessionResourceUsage>,
}

impl ProcessResourceMonitor {
    pub fn new(config: ProcessResourceMonitorConfig) -> Self {
        Self {
            config,
            last_wall: None,
            last_cpu_seconds: None,
            next_sample: Instant::now(),
            last_emitted: None,
            last_emitted_sessions: Vec::new(),
        }
    }

    pub fn sample_if_due(&mut self) -> Option<ProcessResourceSnapshot> {
        self.sample_if_due_with_sessions(&[])
            .map(|(snapshot, _)| snapshot)
    }

    /// Cheap, side-effect-free cadence check for callers that would otherwise allocate or inspect
    /// session targets on every worker pump.
    pub fn is_due(&self, now: Instant) -> bool {
        now >= self.next_sample
    }

    pub fn sample_if_due_with_sessions(
        &mut self,
        targets: &[SessionResourceTarget],
    ) -> Option<(ProcessResourceSnapshot, Vec<SessionResourceUsage>)> {
        let now = Instant::now();
        self.sample_if_due_with_sessions_at(now, targets)
    }

    pub(crate) fn sample_if_due_with_sessions_at(
        &mut self,
        now: Instant,
        targets: &[SessionResourceTarget],
    ) -> Option<(ProcessResourceSnapshot, Vec<SessionResourceUsage>)> {
        if !self.is_due(now) {
            return None;
        }
        self.next_sample = now + self.config.sample_interval;
        let snapshot = self.sample(now);
        let session_usage = self.sample_session_usage(targets, snapshot.sampled_at_ms);
        if !should_emit(self.last_emitted.as_ref(), &snapshot)
            && !should_emit_sessions(&self.last_emitted_sessions, &session_usage)
        {
            return None;
        }
        self.last_emitted = Some(snapshot);
        self.last_emitted_sessions = session_usage.clone();
        Some((snapshot, session_usage))
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

    fn sample_session_usage(
        &self,
        targets: &[SessionResourceTarget],
        sampled_at_ms: u64,
    ) -> Vec<SessionResourceUsage> {
        if targets.is_empty() {
            return Vec::new();
        }
        let rows = process_rows();
        targets
            .iter()
            .map(|target| {
                aggregate_session_usage(
                    *target,
                    &rows,
                    sampled_at_ms,
                    self.config.high_cpu_percent,
                    self.config.high_rss_bytes,
                )
            })
            .collect()
    }
}

/// 변화 게이트: 직전 발행 대비 CPU ±0.5%p / RSS ±1MiB / high 플래그 변화가 없으면
/// 재발행하지 않는다 — idle에서 2초마다 wake/repaint를 유발하지 않기 위함
/// (codex: resource monitor는 idle-silent여야 한다). 첫 샘플은 항상 발행.
fn should_emit(last: Option<&ProcessResourceSnapshot>, next: &ProcessResourceSnapshot) -> bool {
    let Some(last) = last else {
        return true;
    };
    let cpu_delta = match (last.cpu_percent, next.cpu_percent) {
        (Some(a), Some(b)) => (a - b).abs(),
        (None, None) => 0.0,
        _ => f32::INFINITY,
    };
    cpu_delta >= 0.5
        || last.rss_bytes.abs_diff(next.rss_bytes) >= 1024 * 1024
        || last.high_cpu != next.high_cpu
        || last.high_rss != next.high_rss
}

fn should_emit_sessions(last: &[SessionResourceUsage], next: &[SessionResourceUsage]) -> bool {
    if last.len() != next.len() {
        return true;
    }
    for next_usage in next {
        let Some(last_usage) = last
            .iter()
            .find(|usage| usage.session == next_usage.session)
        else {
            return true;
        };
        let cpu_delta = match (last_usage.cpu_percent, next_usage.cpu_percent) {
            (Some(a), Some(b)) => (a - b).abs(),
            (None, None) => 0.0,
            _ => f32::INFINITY,
        };
        if cpu_delta >= 0.5
            || last_usage.rss_bytes.abs_diff(next_usage.rss_bytes) >= 1024 * 1024
            || last_usage.process_count != next_usage.process_count
            || last_usage.high_cpu != next_usage.high_cpu
            || last_usage.high_rss != next_usage.high_rss
        {
            return true;
        }
    }
    false
}

#[derive(Debug, Clone, Copy)]
struct ProcessRow {
    pid: u32,
    ppid: Option<u32>,
    pgid: Option<u32>,
    rss_bytes: u64,
    cpu_percent: Option<f32>,
}

fn aggregate_session_usage(
    target: SessionResourceTarget,
    rows: &[ProcessRow],
    sampled_at_ms: u64,
    high_cpu_percent: f32,
    high_rss_bytes: u64,
) -> SessionResourceUsage {
    let matched = matching_process_rows(target.identity, rows);
    let rss_bytes = matched
        .iter()
        .fold(0u64, |acc, row| acc.saturating_add(row.rss_bytes));
    let mut cpu_seen = false;
    let cpu_total = matched.iter().fold(0.0f32, |acc, row| {
        if let Some(cpu) = row.cpu_percent {
            cpu_seen = true;
            acc + cpu
        } else {
            acc
        }
    });
    let cpu_percent = cpu_seen.then_some(cpu_total);
    SessionResourceUsage {
        session: target.session,
        pid: target.identity.pid,
        process_group: target.identity.process_group,
        identity_source: target.identity.source,
        sampled_at_ms,
        process_count: matched.len(),
        rss_bytes,
        cpu_percent,
        high_cpu: cpu_percent.is_some_and(|cpu| cpu >= high_cpu_percent),
        high_rss: rss_bytes >= high_rss_bytes,
    }
}

fn matching_process_rows(identity: ProcessIdentity, rows: &[ProcessRow]) -> Vec<ProcessRow> {
    let mut matched = std::collections::BTreeMap::new();
    if let Some(process_group) = identity.process_group {
        for row in rows
            .iter()
            .copied()
            .filter(|row| row.pgid == Some(process_group))
        {
            matched.insert(row.pid, row);
        }
    }
    let Some(root_pid) = identity.pid else {
        return matched.into_values().collect();
    };
    let mut wanted = std::collections::HashSet::from([root_pid]);
    let mut changed = true;
    while changed {
        changed = false;
        for row in rows {
            if row.ppid.is_some_and(|ppid| wanted.contains(&ppid)) && wanted.insert(row.pid) {
                changed = true;
            }
        }
    }
    rows.iter()
        .copied()
        .filter(|row| wanted.contains(&row.pid))
        .for_each(|row| {
            matched.insert(row.pid, row);
        });
    matched.into_values().collect()
}

#[cfg(unix)]
fn process_rows() -> Vec<ProcessRow> {
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,pgid=,rss=,pcpu="])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_process_row)
        .collect()
}

#[cfg(not(unix))]
fn process_rows() -> Vec<ProcessRow> {
    Vec::new()
}

fn parse_process_row(line: &str) -> Option<ProcessRow> {
    let mut parts = line.split_whitespace();
    let pid = parts.next()?.parse().ok()?;
    let ppid = parts.next().and_then(|value| value.parse().ok());
    let pgid = parts.next().and_then(|value| value.parse().ok());
    let rss_kib = parts.next()?.parse::<u64>().ok()?;
    let cpu_percent = parts.next().and_then(|value| value.parse().ok());
    Some(ProcessRow {
        pid,
        ppid,
        pgid,
        rss_bytes: rss_kib.saturating_mul(1024),
        cpu_percent,
    })
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

/// macOS는 phys_footprint — 활성 상태 보기(Activity Monitor) '메모리' 열과 같은 지표.
/// 압축 메모리 포함 + 공유 코드 페이지(dylib) 제외라, 기계 램 크기/메모리 압박과
/// 무관하게 일관되고 사용자가 활성 상태 보기와 대조할 수 있다. RSS(ps)는 여유 램이
/// 많은 기계일수록 부풀어 "무거운 앱"으로 오해됐다 (2026-07-16). syscall이라 2초마다
/// ps 서브프로세스를 스폰하던 비용도 없다. 실패 시 ps RSS 폴백.
#[cfg(target_os = "macos")]
fn current_rss_bytes() -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v4>::uninit();
    let rc = unsafe {
        libc::proc_pid_rusage(
            std::process::id() as libc::c_int,
            libc::RUSAGE_INFO_V4,
            info.as_mut_ptr().cast(),
        )
    };
    if rc == 0 {
        // SAFETY: rc == 0이면 커널이 요청한 flavor 구조체 전체를 채웠다.
        let info = unsafe { info.assume_init() };
        return Some(info.ri_phys_footprint);
    }
    ps_rss_bytes()
}

#[cfg(all(unix, not(target_os = "linux"), not(target_os = "macos")))]
fn current_rss_bytes() -> Option<u64> {
    ps_rss_bytes()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn ps_rss_bytes() -> Option<u64> {
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
    fn 변화_게이트는_idle에서_재발행하지_않는다() {
        let snap = |cpu: Option<f32>, rss: u64| ProcessResourceSnapshot {
            pid: 1,
            sampled_at_ms: 0,
            rss_bytes: rss,
            cpu_percent: cpu,
            high_cpu: false,
            high_rss: false,
        };
        // 첫 샘플은 항상 발행
        assert!(should_emit(None, &snap(Some(1.0), 100 << 20)));
        // 변화 없음(임계 미만) → 침묵
        let last = snap(Some(1.0), 100 << 20);
        assert!(!should_emit(
            Some(&last),
            &snap(Some(1.2), (100 << 20) + 4096)
        ));
        // CPU ±0.5%p 이상 → 발행
        assert!(should_emit(Some(&last), &snap(Some(1.6), 100 << 20)));
        // RSS ±1MiB 이상 → 발행
        assert!(should_emit(Some(&last), &snap(Some(1.0), 101 << 20)));
        // cpu 기준선 등장(None→Some) → 발행
        assert!(should_emit(
            Some(&snap(None, 100 << 20)),
            &snap(Some(1.0), 100 << 20)
        ));
        // high 플래그 전이 → 발행
        let mut hot = snap(Some(1.0), 100 << 20);
        hot.high_rss = true;
        assert!(should_emit(Some(&last), &hot));
    }

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

    #[test]
    fn session_usage_aggregates_process_group() {
        let rows = vec![
            ProcessRow {
                pid: 10,
                ppid: Some(1),
                pgid: Some(10),
                rss_bytes: 10 << 20,
                cpu_percent: Some(12.5),
            },
            ProcessRow {
                pid: 11,
                ppid: Some(10),
                pgid: Some(10),
                rss_bytes: 20 << 20,
                cpu_percent: Some(7.5),
            },
            ProcessRow {
                pid: 99,
                ppid: Some(1),
                pgid: Some(99),
                rss_bytes: 99 << 20,
                cpu_percent: Some(99.0),
            },
        ];
        let usage = aggregate_session_usage(
            SessionResourceTarget {
                session: SessionId(1),
                identity: ProcessIdentity {
                    pid: Some(10),
                    process_group: Some(10),
                    source: ProcessIdentitySource::PortablePty,
                },
            },
            &rows,
            123,
            19.0,
            25 << 20,
        );
        assert_eq!(usage.process_count, 2);
        assert_eq!(usage.rss_bytes, 30 << 20);
        assert_eq!(usage.cpu_percent, Some(20.0));
        assert!(usage.high_cpu);
        assert!(usage.high_rss);
    }

    #[test]
    fn session_usage_falls_back_to_pid_descendants() {
        let rows = vec![
            ProcessRow {
                pid: 10,
                ppid: Some(1),
                pgid: Some(77),
                rss_bytes: 10,
                cpu_percent: Some(1.0),
            },
            ProcessRow {
                pid: 11,
                ppid: Some(10),
                pgid: Some(77),
                rss_bytes: 20,
                cpu_percent: Some(2.0),
            },
        ];
        let usage = aggregate_session_usage(
            SessionResourceTarget {
                session: SessionId(1),
                identity: ProcessIdentity {
                    pid: Some(10),
                    process_group: Some(999),
                    source: ProcessIdentitySource::PlatformFallback,
                },
            },
            &rows,
            123,
            100.0,
            100,
        );
        assert_eq!(usage.process_count, 2);
        assert_eq!(usage.rss_bytes, 30);
        assert_eq!(usage.cpu_percent, Some(3.0));
    }

    #[test]
    fn session_usage_unions_process_group_and_pid_descendants() {
        let rows = vec![
            ProcessRow {
                pid: 10,
                ppid: Some(1),
                pgid: Some(10),
                rss_bytes: 10,
                cpu_percent: Some(1.0),
            },
            ProcessRow {
                pid: 11,
                ppid: Some(10),
                pgid: Some(77),
                rss_bytes: 20,
                cpu_percent: Some(2.0),
            },
            ProcessRow {
                pid: 12,
                ppid: Some(11),
                pgid: Some(77),
                rss_bytes: 30,
                cpu_percent: Some(3.0),
            },
        ];
        let usage = aggregate_session_usage(
            SessionResourceTarget {
                session: SessionId(1),
                identity: ProcessIdentity {
                    pid: Some(10),
                    process_group: Some(10),
                    source: ProcessIdentitySource::PortablePty,
                },
            },
            &rows,
            123,
            100.0,
            100,
        );
        assert_eq!(usage.process_count, 3);
        assert_eq!(usage.rss_bytes, 60);
        assert_eq!(usage.cpu_percent, Some(6.0));
    }
}
