#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::{Child, Command, ExitStatus, Stdio};
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use deppy_core::SessionId;
use deppy_core::time::unix_ms;
use pty::{ProcessIdentity, ProcessIdentitySource};

const MAX_PROCESS_ROWS: usize = 16_384;
const MAX_PROCESS_LINE_BYTES: usize = 1024;
const MAX_DESCENDANT_IDS: usize = MAX_PROCESS_ROWS + 1;
#[cfg(unix)]
const MAX_PS_STDOUT_BYTES: usize = 2 * 1024 * 1024;
#[cfg(unix)]
const MAX_PS_STDERR_BYTES: usize = 16 * 1024;
#[cfg(unix)]
const PROCESS_CAPTURE_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(unix)]
const PROCESS_CAPTURE_POLL_INTERVAL: Duration = Duration::from_millis(5);
#[cfg(unix)]
const PIPE_READER_STACK_BYTES: usize = 128 * 1024;
#[cfg(target_os = "linux")]
const MAX_STATM_BYTES: usize = 256;
#[cfg(all(unix, not(target_os = "linux")))]
const MAX_PS_RSS_STDOUT_BYTES: usize = 128;

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
    /// 세션 자손 프로세스들의 메모리 합. macOS는 pid별 phys_footprint 합 —
    /// ps RSS 합산은 공유 페이지(dyld 캐시·런타임)를 자손 수만큼 중복 가산해
    /// 수천 프로세스에서 수백 GiB 허수를 만든다(2026-07-23 실증). 조회 실패한
    /// pid와 그 외 플랫폼은 ps RSS로 폴백한다(상한 근사치).
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
        let table = ProcessTable::new(&rows);
        // 샘플마다 새로 만드는 pid→footprint 메모 — 여러 세션 타깃이 같은 자손을
        // 공유해도 pid당 syscall 1회로 바운드된다(보존 없음).
        let mut footprint_cache = std::collections::HashMap::new();
        targets
            .iter()
            .map(|target| {
                aggregate_session_usage_with_table(
                    *target,
                    &table,
                    sampled_at_ms,
                    self.config.high_cpu_percent,
                    self.config.high_rss_bytes,
                    &mut |row| descendant_rss_bytes(row, &mut footprint_cache),
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

#[cfg(test)]
fn aggregate_session_usage(
    target: SessionResourceTarget,
    rows: &[ProcessRow],
    sampled_at_ms: u64,
    high_cpu_percent: f32,
    high_rss_bytes: u64,
) -> SessionResourceUsage {
    let table = ProcessTable::new(rows);
    // 토폴로지 테스트는 합성 pid를 쓰므로 footprint 조회 없이 행 값 그대로 합산 —
    // 실행 uid(예: root CI)에 따라 결과가 달라지지 않게 결정적으로 유지한다.
    aggregate_session_usage_with_table(
        target,
        &table,
        sampled_at_ms,
        high_cpu_percent,
        high_rss_bytes,
        &mut |row| row.rss_bytes,
    )
}

fn aggregate_session_usage_with_table(
    target: SessionResourceTarget,
    table: &ProcessTable,
    sampled_at_ms: u64,
    high_cpu_percent: f32,
    high_rss_bytes: u64,
    rss_of: &mut dyn FnMut(&ProcessRow) -> u64,
) -> SessionResourceUsage {
    let matched = table.matching_rows(target.identity);
    let rss_bytes = matched
        .iter()
        .fold(0u64, |acc, row| acc.saturating_add(rss_of(row)));
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

struct ProcessTable {
    by_pid: std::collections::BTreeMap<u32, ProcessRow>,
    children: std::collections::BTreeMap<u32, Vec<u32>>,
    by_group: std::collections::BTreeMap<u32, Vec<u32>>,
}

impl ProcessTable {
    fn new(rows: &[ProcessRow]) -> Self {
        let mut by_pid = std::collections::BTreeMap::new();
        let mut children = std::collections::BTreeMap::<u32, Vec<u32>>::new();
        let mut by_group = std::collections::BTreeMap::<u32, Vec<u32>>::new();
        for row in rows.iter().copied().take(MAX_PROCESS_ROWS) {
            by_pid.insert(row.pid, row);
            if let Some(parent) = row.ppid {
                children.entry(parent).or_default().push(row.pid);
            }
            if let Some(group) = row.pgid {
                by_group.entry(group).or_default().push(row.pid);
            }
        }
        Self {
            by_pid,
            children,
            by_group,
        }
    }

    fn matching_rows(&self, identity: ProcessIdentity) -> Vec<ProcessRow> {
        let mut matched_pids = std::collections::BTreeSet::new();
        if let Some(process_group) = identity.process_group
            && let Some(group_pids) = self.by_group.get(&process_group)
        {
            matched_pids.extend(group_pids.iter().copied());
        }
        if let Some(root_pid) = identity.pid {
            let mut wanted = std::collections::BTreeSet::from([root_pid]);
            let mut pending = std::collections::VecDeque::from([root_pid]);
            while let Some(parent) = pending.pop_front() {
                if let Some(child_pids) = self.children.get(&parent) {
                    for child_pid in child_pids {
                        if wanted.len() == MAX_DESCENDANT_IDS {
                            break;
                        }
                        if wanted.insert(*child_pid) {
                            pending.push_back(*child_pid);
                        }
                    }
                }
            }
            matched_pids.extend(wanted);
        }
        matched_pids
            .into_iter()
            .filter_map(|pid| self.by_pid.get(&pid).copied())
            .collect()
    }
}

#[cfg(test)]
fn matching_process_rows(identity: ProcessIdentity, rows: &[ProcessRow]) -> Vec<ProcessRow> {
    ProcessTable::new(rows).matching_rows(identity)
}

#[cfg(unix)]
fn process_rows() -> Vec<ProcessRow> {
    let Ok(bytes) = run_command_bounded(
        Path::new("ps"),
        &["-axo", "pid=,ppid=,pgid=,rss=,pcpu="],
        MAX_PS_STDOUT_BYTES,
    ) else {
        return Vec::new();
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Vec::new();
    };
    parse_process_rows(text).unwrap_or_default()
}

#[cfg(not(unix))]
fn process_rows() -> Vec<ProcessRow> {
    Vec::new()
}

fn parse_process_rows(text: &str) -> Option<Vec<ProcessRow>> {
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.len() > MAX_PROCESS_LINE_BYTES {
            return None;
        }
        let Some(row) = parse_process_row(line) else {
            continue;
        };
        if rows.len() == MAX_PROCESS_ROWS {
            return None;
        }
        rows.push(row);
    }
    Some(rows)
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureError {
    SpawnFailed,
    PipeUnavailable,
    ReaderSpawnFailed,
    ReadFailed,
    OutputTooLarge,
    WaitFailed,
    TimedOut,
    ReaderPanicked,
    CommandFailed,
}

#[cfg(unix)]
fn read_pipe_bounded(
    mut reader: impl Read,
    max_bytes: usize,
    limit_reached: &AtomicBool,
    retain: bool,
) -> Result<Vec<u8>, CaptureError> {
    let hard_limit = max_bytes
        .checked_add(1)
        .ok_or(CaptureError::OutputTooLarge)?;
    let mut retained = Vec::with_capacity(if retain { max_bytes.min(64 * 1024) } else { 0 });
    let mut observed = 0usize;
    let mut chunk = [0u8; 16 * 1024];
    while observed < hard_limit {
        let read_len = (hard_limit - observed).min(chunk.len());
        let count = reader
            .read(&mut chunk[..read_len])
            .map_err(|_| CaptureError::ReadFailed)?;
        if count == 0 {
            break;
        }
        observed += count;
        if retain {
            retained.extend_from_slice(&chunk[..count]);
        }
    }
    if observed > max_bytes {
        limit_reached.store(true, Ordering::Release);
        return Err(CaptureError::OutputTooLarge);
    }
    Ok(retained)
}

#[cfg(all(unix, test))]
static ACTIVE_CAPTURE_READERS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(all(unix, test))]
struct ActiveCaptureReader;

#[cfg(all(unix, test))]
impl ActiveCaptureReader {
    fn enter() -> Self {
        ACTIVE_CAPTURE_READERS.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

#[cfg(all(unix, test))]
impl Drop for ActiveCaptureReader {
    fn drop(&mut self) {
        ACTIVE_CAPTURE_READERS.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(unix)]
struct RunningCapture {
    child: Child,
    reaped: bool,
    stdout_reader: Option<std::thread::JoinHandle<Result<Vec<u8>, CaptureError>>>,
    stderr_reader: Option<std::thread::JoinHandle<Result<Vec<u8>, CaptureError>>>,
    output_limit_reached: Arc<AtomicBool>,
}

#[cfg(unix)]
impl RunningCapture {
    fn spawn(
        program: &Path,
        args: &[&str],
        stdout_max_bytes: usize,
        stderr_max_bytes: usize,
    ) -> Result<Self, CaptureError> {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);

        let mut child = command.spawn().map_err(|_| CaptureError::SpawnFailed)?;
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let _ = kill_and_reap_capture(&mut child);
                return Err(CaptureError::PipeUnavailable);
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                drop(stdout);
                let _ = kill_and_reap_capture(&mut child);
                return Err(CaptureError::PipeUnavailable);
            }
        };

        let output_limit_reached = Arc::new(AtomicBool::new(false));
        let stdout_limit = Arc::clone(&output_limit_reached);
        let stdout_reader = match std::thread::Builder::new()
            .name("resource-ps-stdout".to_owned())
            .stack_size(PIPE_READER_STACK_BYTES)
            .spawn(move || {
                #[cfg(test)]
                let _active = ActiveCaptureReader::enter();
                read_pipe_bounded(stdout, stdout_max_bytes, &stdout_limit, true)
            }) {
            Ok(reader) => reader,
            Err(_) => {
                drop(stderr);
                let _ = kill_and_reap_capture(&mut child);
                return Err(CaptureError::ReaderSpawnFailed);
            }
        };
        let stderr_limit = Arc::clone(&output_limit_reached);
        let stderr_reader = match std::thread::Builder::new()
            .name("resource-ps-stderr".to_owned())
            .stack_size(PIPE_READER_STACK_BYTES)
            .spawn(move || {
                #[cfg(test)]
                let _active = ActiveCaptureReader::enter();
                read_pipe_bounded(stderr, stderr_max_bytes, &stderr_limit, false)
            }) {
            Ok(reader) => reader,
            Err(_) => {
                let _ = kill_and_reap_capture(&mut child);
                let _ = stdout_reader.join();
                return Err(CaptureError::ReaderSpawnFailed);
            }
        };
        Ok(Self {
            child,
            reaped: false,
            stdout_reader: Some(stdout_reader),
            stderr_reader: Some(stderr_reader),
            output_limit_reached,
        })
    }

    fn output_limit_reached(&self) -> bool {
        self.output_limit_reached.load(Ordering::Acquire)
    }

    fn try_wait(&mut self) -> Result<Option<ExitStatus>, CaptureError> {
        // Parent가 먼저 종료하고 descendant가 pipe를 상속한 경우에도 WNOWAIT로 leader pid를
        // 보존한 채 group 전체를 닫은 뒤 reap한다. pid/pgid 재사용 창도 만들지 않는다.
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            return Err(CaptureError::WaitFailed);
        }
        if unsafe { info.si_pid() } == 0 {
            return Ok(None);
        }
        self.kill_group();
        let status = self.child.wait().map_err(|_| CaptureError::WaitFailed)?;
        self.reaped = true;
        Ok(Some(status))
    }

    fn kill_group(&self) {
        unsafe {
            libc::killpg(self.child.id() as libc::pid_t, libc::SIGKILL);
        }
    }

    fn kill_and_reap(&mut self) -> Result<(), CaptureError> {
        if !self.reaped {
            self.kill_group();
            let _ = self.child.kill();
            self.child.wait().map_err(|_| CaptureError::WaitFailed)?;
            self.reaped = true;
        }
        Ok(())
    }

    fn join_readers(&mut self) -> Result<(Vec<u8>, Vec<u8>), CaptureError> {
        fn join(
            reader: Option<std::thread::JoinHandle<Result<Vec<u8>, CaptureError>>>,
        ) -> Result<Vec<u8>, CaptureError> {
            reader
                .ok_or(CaptureError::PipeUnavailable)?
                .join()
                .map_err(|_| CaptureError::ReaderPanicked)?
        }
        // 첫 reader가 실패해도 다른 pipe reader까지 반드시 join한다. JoinHandle drop은
        // thread detach이므로 `?`로 조기 반환하면 반복 sample에서 reader가 남을 수 있다.
        let stdout = join(self.stdout_reader.take());
        let stderr = join(self.stderr_reader.take());
        match (stdout, stderr) {
            (Ok(stdout), Ok(stderr)) => Ok((stdout, stderr)),
            (Err(error), _) | (_, Err(error)) => Err(error),
        }
    }
}

#[cfg(unix)]
impl Drop for RunningCapture {
    fn drop(&mut self) {
        let _ = self.kill_and_reap();
        let _ = self.join_readers();
    }
}

#[cfg(unix)]
fn kill_and_reap_capture(child: &mut Child) -> Result<(), CaptureError> {
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = child.kill();
    child.wait().map_err(|_| CaptureError::WaitFailed)?;
    Ok(())
}

#[cfg(unix)]
fn run_command_bounded(
    program: &Path,
    args: &[&str],
    stdout_max_bytes: usize,
) -> Result<Vec<u8>, CaptureError> {
    run_command_bounded_with_limits(
        program,
        args,
        stdout_max_bytes,
        MAX_PS_STDERR_BYTES,
        PROCESS_CAPTURE_TIMEOUT,
    )
}

#[cfg(unix)]
fn run_command_bounded_with_limits(
    program: &Path,
    args: &[&str],
    stdout_max_bytes: usize,
    stderr_max_bytes: usize,
    timeout: Duration,
) -> Result<Vec<u8>, CaptureError> {
    let mut running = RunningCapture::spawn(program, args, stdout_max_bytes, stderr_max_bytes)?;
    let started = Instant::now();
    let status = loop {
        if running.output_limit_reached() {
            let _ = running.kill_and_reap();
            let _ = running.join_readers();
            return Err(CaptureError::OutputTooLarge);
        }
        match running.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                let _ = running.kill_and_reap();
                let _ = running.join_readers();
                return Err(CaptureError::TimedOut);
            }
            Ok(None) => std::thread::sleep(PROCESS_CAPTURE_POLL_INTERVAL.min(timeout)),
            Err(error) => {
                let _ = running.kill_and_reap();
                let _ = running.join_readers();
                return Err(error);
            }
        }
    };
    let (stdout, _) = running.join_readers()?;
    if !status.success() {
        return Err(CaptureError::CommandFailed);
    }
    Ok(stdout)
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
    let file = std::fs::File::open("/proc/self/statm").ok()?;
    let capture = read_statm_bounded(file)?;
    let statm = std::str::from_utf8(capture.as_bytes()).ok()?;
    let resident_pages = statm.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    Some(resident_pages.saturating_mul(page_size()))
}

#[cfg(target_os = "linux")]
struct StatmCapture {
    bytes: [u8; MAX_STATM_BYTES + 1],
    len: usize,
}

#[cfg(target_os = "linux")]
impl StatmCapture {
    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

#[cfg(target_os = "linux")]
fn read_statm_bounded(mut reader: impl Read) -> Option<StatmCapture> {
    let mut capture = StatmCapture {
        bytes: [0; MAX_STATM_BYTES + 1],
        len: 0,
    };
    while capture.len < capture.bytes.len() {
        let count = reader.read(&mut capture.bytes[capture.len..]).ok()?;
        if count == 0 {
            break;
        }
        capture.len += count;
    }
    (capture.len <= MAX_STATM_BYTES).then_some(capture)
}

/// macOS는 phys_footprint — 활성 상태 보기(Activity Monitor) '메모리' 열과 같은 지표.
/// 압축 메모리 포함 + 공유 코드 페이지(dylib) 제외라, 기계 램 크기/메모리 압박과
/// 무관하게 일관되고 사용자가 활성 상태 보기와 대조할 수 있다. RSS(ps)는 여유 램이
/// 많은 기계일수록 부풀어 "무거운 앱"으로 오해됐다 (2026-07-16). syscall이라 2초마다
/// ps 서브프로세스를 스폰하던 비용도 없다. 실패 시 ps RSS 폴백.
#[cfg(target_os = "macos")]
fn current_rss_bytes() -> Option<u64> {
    phys_footprint_for_pid(std::process::id()).or_else(ps_rss_bytes)
}

/// 같은 uid 소유 프로세스는 특권 없이 조회 가능(2026-07-23 실측). 다른 uid(sudo 자손
/// 등)나 이미 종료된 pid는 None — 호출부가 ps RSS로 폴백한다.
#[cfg(target_os = "macos")]
fn phys_footprint_for_pid(pid: u32) -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v4>::uninit();
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid as libc::c_int,
            libc::RUSAGE_INFO_V4,
            info.as_mut_ptr().cast(),
        )
    };
    (rc == 0).then(|| {
        // SAFETY: rc == 0이면 커널이 요청한 flavor 구조체 전체를 채웠다.
        let info = unsafe { info.assume_init() };
        info.ri_phys_footprint
    })
}

/// 자손 프로세스 한 행의 메모리. macOS는 phys_footprint를 조회해 ps RSS의 공유
/// 페이지 중복 합산을 제거하고, 실패한 행만 ps RSS로 폴백한다 — 절대 0으로
/// 떨어뜨리지 않는다. 캐시는 같은 샘플 주기 안에서 pid당 syscall 1회 바운드용.
#[cfg(target_os = "macos")]
fn descendant_rss_bytes(
    row: &ProcessRow,
    cache: &mut std::collections::HashMap<u32, Option<u64>>,
) -> u64 {
    cache
        .entry(row.pid)
        .or_insert_with(|| phys_footprint_for_pid(row.pid))
        .unwrap_or(row.rss_bytes)
}

#[cfg(not(target_os = "macos"))]
fn descendant_rss_bytes(
    row: &ProcessRow,
    _cache: &mut std::collections::HashMap<u32, Option<u64>>,
) -> u64 {
    row.rss_bytes
}

#[cfg(all(unix, not(target_os = "linux"), not(target_os = "macos")))]
fn current_rss_bytes() -> Option<u64> {
    ps_rss_bytes()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn ps_rss_bytes() -> Option<u64> {
    let pid = std::process::id().to_string();
    let output = run_command_bounded(
        Path::new("ps"),
        &["-o", "rss=", "-p", &pid],
        MAX_PS_RSS_STDOUT_BYTES,
    )
    .ok()?;
    let text = std::str::from_utf8(&output).ok()?;
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
    fn aggregate는_주입된_rss_조회자로_합산한다() {
        let rows = vec![
            ProcessRow {
                pid: 10,
                ppid: Some(1),
                pgid: Some(77),
                rss_bytes: 10,
                cpu_percent: None,
            },
            ProcessRow {
                pid: 11,
                ppid: Some(10),
                pgid: Some(77),
                rss_bytes: 20,
                cpu_percent: None,
            },
        ];
        let table = ProcessTable::new(&rows);
        let usage = aggregate_session_usage_with_table(
            SessionResourceTarget {
                session: SessionId(1),
                identity: ProcessIdentity {
                    pid: Some(10),
                    process_group: Some(77),
                    source: ProcessIdentitySource::PlatformFallback,
                },
            },
            &table,
            123,
            100.0,
            1000,
            &mut |row| row.rss_bytes * 100,
        );
        assert_eq!(usage.rss_bytes, 3000);
        assert!(usage.high_rss);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_phys_footprint는_실행중_자식_pid에_성공한다() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("5")
            .spawn()
            .expect("sleep spawn");
        let footprint = phys_footprint_for_pid(child.id());
        let _ = child.kill();
        let _ = child.wait();
        assert!(footprint.is_some_and(|bytes| bytes > 0));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_reap된_pid는_footprint_조회가_실패해_ps_rss로_폴백한다() {
        let mut child = std::process::Command::new("/usr/bin/true")
            .spawn()
            .expect("true spawn");
        let pid = child.id();
        child.wait().expect("reap");
        let row = ProcessRow {
            pid,
            ppid: Some(1),
            pgid: Some(pid),
            rss_bytes: 1234,
            cpu_percent: None,
        };
        let mut cache = std::collections::HashMap::new();
        assert_eq!(descendant_rss_bytes(&row, &mut cache), 1234);
        // 실패 결과도 메모이즈되어 같은 샘플 안에서 재조회하지 않는다.
        assert_eq!(cache.get(&pid), Some(&None));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_자식_다수의_footprint_합은_ps_rss_합보다_작다() {
        let mut children: Vec<std::process::Child> = (0..5)
            .map(|_| {
                std::process::Command::new("/bin/sleep")
                    .arg("10")
                    .spawn()
                    .expect("sleep spawn")
            })
            .collect();
        let mut footprint_sum = 0u64;
        let mut ps_rss_sum = 0u64;
        for child in &children {
            let pid = child.id();
            footprint_sum += phys_footprint_for_pid(pid).expect("footprint");
            let output = std::process::Command::new("ps")
                .args(["-o", "rss=", "-p", &pid.to_string()])
                .output()
                .expect("ps");
            let kib: u64 = String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse()
                .expect("rss parse");
            ps_rss_sum += kib * 1024;
        }
        for child in &mut children {
            let _ = child.kill();
            let _ = child.wait();
        }
        // dyld 공유 캐시가 자식마다 ps RSS에 중복 가산되므로 footprint 합이
        // 항상 작아야 한다 — 공유 페이지 중복 제거의 회귀 가드.
        assert!(
            footprint_sum < ps_rss_sum,
            "footprint_sum={footprint_sum} ps_rss_sum={ps_rss_sum}"
        );
    }

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

    #[test]
    fn process_rows_accept_exact_row_and_line_limits_and_reject_plus_one() {
        let row = "1 0 1 1 1\n";
        let exact_rows = row.repeat(MAX_PROCESS_ROWS);
        assert_eq!(
            parse_process_rows(&exact_rows).unwrap().len(),
            MAX_PROCESS_ROWS
        );
        let plus_one_rows = format!("{exact_rows}{row}");
        assert!(parse_process_rows(&plus_one_rows).is_none());

        let base = "1 0 1 1 1";
        let exact_line = format!("{base}{}", " ".repeat(MAX_PROCESS_LINE_BYTES - base.len()));
        assert_eq!(parse_process_rows(&exact_line).unwrap().len(), 1);
        let plus_one_line = format!("{exact_line} ");
        assert!(parse_process_rows(&plus_one_line).is_none());
    }

    #[test]
    fn descendant_bfs_is_bounded_by_process_row_limit() {
        let rows: Vec<_> = (0..MAX_PROCESS_ROWS)
            .map(|index| ProcessRow {
                pid: index as u32 + 1,
                ppid: (index > 0).then_some(index as u32),
                pgid: None,
                rss_bytes: 1,
                cpu_percent: Some(0.0),
            })
            .collect();
        let matched = matching_process_rows(
            ProcessIdentity {
                pid: Some(1),
                process_group: None,
                source: ProcessIdentitySource::PlatformFallback,
            },
            &rows,
        );
        assert_eq!(matched.len(), MAX_PROCESS_ROWS);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn statm_reader_accepts_exact_bytes_and_rejects_plus_one() {
        let exact = vec![b'1'; MAX_STATM_BYTES];
        assert_eq!(
            read_statm_bounded(std::io::Cursor::new(&exact))
                .unwrap()
                .as_bytes()
                .len(),
            MAX_STATM_BYTES
        );
        let plus_one = vec![b'1'; MAX_STATM_BYTES + 1];
        assert!(read_statm_bounded(std::io::Cursor::new(plus_one)).is_none());
    }

    #[cfg(unix)]
    static CAPTURE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(unix)]
    #[test]
    fn capture_accepts_exact_stdout_and_stderr_and_rejects_plus_one() {
        let _guard = CAPTURE_TEST_LOCK.lock().unwrap();
        let exact = "x".repeat(64);
        assert_eq!(
            run_command_bounded_with_limits(
                Path::new("/usr/bin/printf"),
                &[&exact],
                64,
                64,
                Duration::from_secs(1),
            )
            .unwrap(),
            exact.as_bytes()
        );
        let plus_one = "x".repeat(65);
        assert_eq!(
            run_command_bounded_with_limits(
                Path::new("/usr/bin/printf"),
                &[&plus_one],
                64,
                64,
                Duration::from_secs(1),
            ),
            Err(CaptureError::OutputTooLarge)
        );

        let exact_stderr = format!("printf %s {} >&2", "x".repeat(64));
        assert_eq!(
            run_command_bounded_with_limits(
                Path::new("/bin/sh"),
                &["-c", &exact_stderr],
                64,
                64,
                Duration::from_secs(1),
            ),
            Ok(Vec::new())
        );
        let plus_one_stderr = format!("printf %s {} >&2", "x".repeat(65));
        assert_eq!(
            run_command_bounded_with_limits(
                Path::new("/bin/sh"),
                &["-c", &plus_one_stderr],
                64,
                64,
                Duration::from_secs(1),
            ),
            Err(CaptureError::OutputTooLarge)
        );
        assert_eq!(ACTIVE_CAPTURE_READERS.load(Ordering::Acquire), 0);
    }

    #[cfg(unix)]
    #[test]
    fn capture_timeout_kills_group_and_inherited_descendant_pipe_does_not_block() {
        let _guard = CAPTURE_TEST_LOCK.lock().unwrap();
        let started = Instant::now();
        assert_eq!(
            run_command_bounded_with_limits(
                Path::new("/bin/sh"),
                &["-c", "sleep 5"],
                64,
                64,
                Duration::from_millis(50),
            ),
            Err(CaptureError::TimedOut)
        );
        assert!(started.elapsed() < Duration::from_secs(2));

        let started = Instant::now();
        assert_eq!(
            run_command_bounded_with_limits(
                Path::new("/bin/sh"),
                &["-c", "(sleep 5) & printf ok"],
                64,
                64,
                Duration::from_secs(1),
            ),
            Ok(b"ok".to_vec())
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(ACTIVE_CAPTURE_READERS.load(Ordering::Acquire), 0);
    }

    #[cfg(unix)]
    #[test]
    fn repeated_capture_cycles_leave_no_reader_growth() {
        let _guard = CAPTURE_TEST_LOCK.lock().unwrap();
        for _ in 0..16 {
            assert_eq!(
                run_command_bounded_with_limits(
                    Path::new("/usr/bin/printf"),
                    &["ok"],
                    2,
                    2,
                    Duration::from_secs(1),
                ),
                Ok(b"ok".to_vec())
            );
            assert_eq!(ACTIVE_CAPTURE_READERS.load(Ordering::Acquire), 0);
        }
    }

    #[test]
    fn production_source_has_no_unbounded_capture_or_raw_diagnostics() {
        let production = include_str!("resource_monitor.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        for forbidden in [
            ".output()",
            "read_to_string",
            "read_to_end",
            "String::from_utf8_lossy",
            "tracing::",
            "eprintln!",
            "println!",
        ] {
            assert!(!production.contains(forbidden), "found {forbidden}");
        }
        assert_eq!(production.matches("Command::new").count(), 1);
        assert!(production.contains("process_group(0)"));
        assert!(production.contains("libc::WNOWAIT"));
        assert!(production.contains("MAX_PS_STDOUT_BYTES"));
        #[cfg(target_os = "linux")]
        assert!(production.contains("MAX_STATM_BYTES + 1"));
    }
}
