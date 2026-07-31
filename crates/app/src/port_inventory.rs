#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn lsof_field_parser_groups_pid_command_and_ipv4_ipv6_listeners() {
        let input = b"p41\ncworkerd\nn*:3000\nn127.0.0.1:8443\np42\ncnode\nn[::1]:9229\n";
        let rows = parse_lsof_fields(input).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!((rows[0].pid, rows[0].port), (41, 3000));
        assert_eq!((rows[1].pid, rows[1].bind.as_ref()), (41, "127.0.0.1"));
        assert_eq!((rows[2].pid, rows[2].bind.as_ref()), (42, "::1"));
    }

    #[test]
    fn listener_admission_accepts_200_and_rejects_201_without_partial_result() {
        let exact = listener_fixture(200).unwrap();
        assert_eq!(parse_lsof_fields(&exact).unwrap().len(), 200);
        let plus_one = listener_fixture(201).unwrap();
        assert_eq!(
            parse_lsof_fields(&plus_one),
            Err(PortErrorCode::TooManyRows)
        );
        assert_eq!(listener_fixture(PORT_ROW_MAX + 2), None);
    }

    #[test]
    fn longest_workspace_root_owns_nested_listener() {
        let roots = roots_fixture([
            ("root", "/tmp/project"),
            ("nested", "/tmp/project/apps/api"),
        ])
        .unwrap();
        let owner = assign_workspace(Path::new("/tmp/project/apps/api/src"), &roots).unwrap();
        assert_eq!(owner.id.as_ref(), "nested");
    }

    #[test]
    fn scanner_accepts_exact_2mib_and_rejects_plus_one() {
        let exact = FakeCommandRunner::stdout(vec![b'x'; PORT_OUTPUT_MAX_BYTES]);
        assert!(run_lsof_with(&exact, Duration::from_millis(10)).is_ok());
        let plus_one = FakeCommandRunner::stdout(vec![b'x'; PORT_OUTPUT_MAX_BYTES + 1]);
        assert_eq!(
            run_lsof_with(&plus_one, Duration::from_millis(10)),
            Err(PortErrorCode::OutputTooLarge)
        );
    }

    #[test]
    fn timeout_kills_process_group_and_joins_readers() {
        let before = test_reader_thread_count();
        let result = run_test_hanging_command(Duration::from_millis(100));
        assert_eq!(result, Err(PortErrorCode::Timeout));
        assert!(wait_until_process_group_is_gone(Duration::from_secs(1)));
        assert_eq!(test_reader_thread_count(), before);
    }

    #[test]
    fn terminate_rejects_when_fresh_pid_port_or_workspace_differs() {
        let changed = row_fixture("ws-b", 77, 3000, PortOwnership::Workspace);
        let scanner = FakeScanner::new([Ok(vec![changed])]);
        let signaler = FakeSignaler::default();
        let mut backend = PortBackend::new(scanner, signaler, u32::MAX);

        assert_eq!(
            backend.terminate(termination_fixture("ws-a", 77, 3000)),
            Err(PortErrorCode::OwnershipChanged)
        );
        assert!(backend.signaler().signals().is_empty());
    }

    #[test]
    fn protected_and_external_listeners_never_receive_sigterm() {
        for ownership in [PortOwnership::Protected, PortOwnership::External] {
            let scanner = FakeScanner::new([Ok(vec![row_fixture("ws-a", 77, 3000, ownership)])]);
            let signaler = FakeSignaler::default();
            let mut backend = PortBackend::new(scanner, signaler, u32::MAX);
            assert_eq!(
                backend.terminate(termination_fixture("ws-a", 77, 3000)),
                Err(PortErrorCode::NotTerminable)
            );
            assert!(backend.signaler().signals().is_empty());
        }
    }

    #[test]
    fn exact_revalidation_sends_only_sigterm() {
        let row = row_fixture("ws-a", 77, 3000, PortOwnership::Workspace);
        let scanner = FakeScanner::new([Ok(vec![row]), Ok(Vec::new())]);
        let signaler = FakeSignaler::default();
        let mut backend = PortBackend::new(scanner, signaler, u32::MAX);

        backend
            .terminate(termination_fixture("ws-a", 77, 3000))
            .unwrap();
        assert_eq!(backend.signaler().signals(), &[(77, libc::SIGTERM)]);
    }

    #[test]
    fn worker_is_inert_until_first_request_and_allows_one_outstanding_job() {
        let mut worker = worker(|| {});
        assert!(!worker.has_live_worker());
        let roots: Arc<[PortWorkspaceRoot]> = Arc::from([]);
        worker
            .try_request(PortJob::Scan {
                generation: 1,
                roots: Arc::clone(&roots),
            })
            .unwrap();
        assert!(worker.has_live_worker());
        assert!(
            worker
                .try_request(PortJob::Scan {
                    generation: 2,
                    roots,
                })
                .is_err()
        );
    }

    fn listener_fixture(count: usize) -> Option<Vec<u8>> {
        if count > PORT_ROW_MAX + 1 {
            return None;
        }
        let mut bytes = Vec::with_capacity(count.saturating_mul(32));
        for index in 0..count {
            let name = match index {
                0 => "*:3000".to_owned(),
                1 => "127.0.0.1:8443".to_owned(),
                2 => "[::1]:9229".to_owned(),
                _ => format!("127.0.0.1:{}", 10_000 + index),
            };
            bytes.extend_from_slice(format!("p{}\ncfixture\nn{name}\n", index + 1).as_bytes());
        }
        Some(bytes)
    }

    fn roots_fixture<const N: usize>(roots: [(&str, &str); N]) -> Option<Arc<[PortWorkspaceRoot]>> {
        if N > PORT_ROW_MAX {
            return None;
        }
        Some(Arc::from(
            roots
                .into_iter()
                .map(|(id, path)| PortWorkspaceRoot {
                    id: Arc::from(id),
                    name: Arc::from(id),
                    path: Arc::from(PathBuf::from(path)),
                })
                .collect::<Vec<_>>(),
        ))
    }

    fn row_fixture(workspace_id: &str, pid: u32, port: u16, ownership: PortOwnership) -> PortRow {
        PortRow {
            pid,
            port,
            bind: Arc::from("127.0.0.1"),
            protocol: PortProtocol::Tcp,
            process: Arc::from("fixture"),
            workspace_id: Some(Arc::from(workspace_id)),
            workspace_name: Some(Arc::from(workspace_id)),
            ownership,
        }
    }

    fn termination_fixture(workspace_id: &str, pid: u32, port: u16) -> PortTerminationTarget {
        PortTerminationTarget {
            workspace_id: Arc::from(workspace_id),
            pid,
            port,
            bind: Arc::from("127.0.0.1"),
            protocol: PortProtocol::Tcp,
        }
    }

    struct FakeCommandRunner {
        stdout: Vec<u8>,
    }

    impl FakeCommandRunner {
        fn stdout(stdout: Vec<u8>) -> Self {
            Self { stdout }
        }
    }

    impl CommandRunner for FakeCommandRunner {
        fn run(
            &self,
            _program: &str,
            _args: &[String],
            _timeout: Duration,
        ) -> Result<CommandOutput, PortErrorCode> {
            Ok(CommandOutput {
                stdout: self.stdout.clone(),
                stderr: Vec::new(),
                overflow: false,
                success: true,
            })
        }
    }

    #[derive(Default)]
    struct FakeSignaler {
        signals: Vec<(u32, i32)>,
    }

    impl ProcessSignaler for FakeSignaler {
        fn signal(&mut self, pid: u32, signal: i32) -> Result<(), PortErrorCode> {
            self.signals.push((pid, signal));
            Ok(())
        }
    }

    impl FakeSignaler {
        fn signals(&self) -> &[(u32, i32)] {
            &self.signals
        }
    }

    struct FakeScanner {
        scans: std::collections::VecDeque<Result<Vec<PortRow>, PortErrorCode>>,
    }

    impl FakeScanner {
        fn new<const N: usize>(scans: [Result<Vec<PortRow>, PortErrorCode>; N]) -> Self {
            Self {
                scans: scans.into(),
            }
        }
    }

    impl PortScanner for FakeScanner {
        fn scan(&mut self) -> Result<Vec<PortRow>, PortErrorCode> {
            self.scans.pop_front().expect("fixture scan")
        }
    }
}

use crate::lazy_worker::LazyBoundedWorker;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) const PORT_ROW_MAX: usize = 200;
const PORT_OUTPUT_MAX_BYTES: usize = 2 * 1024 * 1024;
const PORT_COMMAND_TIMEOUT: Duration = Duration::from_secs(4);
const PORT_WORKER_IDLE_TTL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PortErrorCode {
    UnsupportedPlatform,
    SpawnFailed,
    Io,
    Timeout,
    OutputTooLarge,
    CommandFailed,
    MalformedOutput,
    TooManyRows,
    OwnershipChanged,
    NotTerminable,
    SignalFailed,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PortWorkspaceRoot {
    pub id: Arc<str>,
    pub name: Arc<str>,
    pub path: Arc<Path>,
}

impl fmt::Debug for PortWorkspaceRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PortWorkspaceRoot")
            .field("status", &"redacted")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PortOwnership {
    Workspace,
    External,
    Protected,
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PortProtocol {
    Tcp,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PortRow {
    pub pid: u32,
    pub port: u16,
    pub bind: Arc<str>,
    pub protocol: PortProtocol,
    pub process: Arc<str>,
    pub workspace_id: Option<Arc<str>>,
    pub workspace_name: Option<Arc<str>>,
    pub ownership: PortOwnership,
}

impl fmt::Debug for PortRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PortRow")
            .field("protocol", &self.protocol)
            .field("ownership", &self.ownership)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PortSnapshot {
    pub generation: u64,
    pub sampled_at_ms: u64,
    pub rows: Arc<[PortRow]>,
}

impl fmt::Debug for PortSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PortSnapshot")
            .field("generation", &self.generation)
            .field("row_count", &self.rows.len())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PortTerminationTarget {
    pub workspace_id: Arc<str>,
    pub pid: u32,
    pub port: u16,
    pub bind: Arc<str>,
    pub protocol: PortProtocol,
}

impl fmt::Debug for PortTerminationTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PortTerminationTarget")
            .field("status", &"redacted")
            .finish()
    }
}

pub(crate) enum PortJob {
    Scan {
        generation: u64,
        roots: Arc<[PortWorkspaceRoot]>,
    },
    Terminate {
        generation: u64,
        roots: Arc<[PortWorkspaceRoot]>,
        target: PortTerminationTarget,
    },
}

impl fmt::Debug for PortJob {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            Self::Scan { .. } => "scan",
            Self::Terminate { .. } => "terminate",
        };
        formatter
            .debug_struct("PortJob")
            .field("kind", &kind)
            .finish()
    }
}

pub(crate) enum PortOutcome {
    Scanned(Result<PortSnapshot, PortErrorCode>),
    Terminated(Result<PortSnapshot, PortErrorCode>),
}

impl fmt::Debug for PortOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, status) = match self {
            Self::Scanned(result) => ("scan", outcome_status(result)),
            Self::Terminated(result) => ("terminate", outcome_status(result)),
        };
        formatter
            .debug_struct("PortOutcome")
            .field("kind", &kind)
            .field("status", &status)
            .finish()
    }
}

fn outcome_status(result: &Result<PortSnapshot, PortErrorCode>) -> &'static str {
    if result.is_ok() { "ok" } else { "error" }
}

pub(crate) fn worker(
    wake: impl Fn() + Send + Sync + 'static,
) -> LazyBoundedWorker<PortJob, PortOutcome> {
    LazyBoundedWorker::new("port-inventory", PORT_WORKER_IDLE_TTL, || execute_job, wake)
}

fn execute_job(job: PortJob) -> PortOutcome {
    match job {
        PortJob::Scan { generation, roots } => {
            PortOutcome::Scanned(scan_snapshot(generation, &roots))
        }
        PortJob::Terminate {
            generation,
            roots,
            target,
        } => PortOutcome::Terminated(terminate_and_rescan(generation, &roots, target)),
    }
}

fn scan_snapshot(
    generation: u64,
    roots: &[PortWorkspaceRoot],
) -> Result<PortSnapshot, PortErrorCode> {
    let rows = scan_local_ports(roots)?;
    Ok(snapshot(generation, rows))
}

fn terminate_and_rescan(
    generation: u64,
    roots: &[PortWorkspaceRoot],
    target: PortTerminationTarget,
) -> Result<PortSnapshot, PortErrorCode> {
    let scanner = SystemPortScanner::new(roots);
    let signaler = SystemSignaler;
    let mut backend = PortBackend::new(scanner, signaler, std::process::id());
    let rows = backend.terminate(target)?;
    Ok(snapshot(generation, rows))
}

fn snapshot(generation: u64, rows: Vec<PortRow>) -> PortSnapshot {
    let sampled_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    PortSnapshot {
        generation,
        sampled_at_ms,
        rows: rows.into(),
    }
}

trait PortScanner {
    fn scan(&mut self) -> Result<Vec<PortRow>, PortErrorCode>;
}

trait ProcessSignaler {
    fn signal(&mut self, pid: u32, signal: i32) -> Result<(), PortErrorCode>;
}

struct PortBackend<S, G> {
    scanner: S,
    signaler: G,
    protected_pid: u32,
}

impl<S, G> PortBackend<S, G>
where
    S: PortScanner,
    G: ProcessSignaler,
{
    fn new(scanner: S, signaler: G, protected_pid: u32) -> Self {
        Self {
            scanner,
            signaler,
            protected_pid,
        }
    }

    fn terminate(&mut self, target: PortTerminationTarget) -> Result<Vec<PortRow>, PortErrorCode> {
        if target.pid == self.protected_pid {
            return Err(PortErrorCode::NotTerminable);
        }
        let rows = self.scanner.scan()?;
        let mut exact = rows.iter().filter(|row| target.matches(row));
        let Some(row) = exact.next() else {
            return Err(PortErrorCode::OwnershipChanged);
        };
        if exact.next().is_some() {
            return Err(PortErrorCode::OwnershipChanged);
        }
        if row.ownership != PortOwnership::Workspace {
            return Err(PortErrorCode::NotTerminable);
        }
        self.signaler.signal(target.pid, libc::SIGTERM)?;
        self.scanner.scan()
    }

    #[cfg(test)]
    fn signaler(&self) -> &G {
        &self.signaler
    }
}

impl PortTerminationTarget {
    fn matches(&self, row: &PortRow) -> bool {
        row.pid == self.pid
            && row.port == self.port
            && row.bind == self.bind
            && row.protocol == self.protocol
            && row.workspace_id.as_deref() == Some(self.workspace_id.as_ref())
    }
}

struct SystemPortScanner<'a> {
    roots: &'a [PortWorkspaceRoot],
}

impl<'a> SystemPortScanner<'a> {
    fn new(roots: &'a [PortWorkspaceRoot]) -> Self {
        Self { roots }
    }
}

impl PortScanner for SystemPortScanner<'_> {
    fn scan(&mut self) -> Result<Vec<PortRow>, PortErrorCode> {
        scan_local_ports(self.roots)
    }
}

struct SystemSignaler;

impl ProcessSignaler for SystemSignaler {
    fn signal(&mut self, pid: u32, signal: i32) -> Result<(), PortErrorCode> {
        let pid = i32::try_from(pid).map_err(|_| PortErrorCode::NotTerminable)?;
        // SAFETY: `kill` receives a positive, range-checked PID and the fixed SIGTERM signal.
        let result = unsafe { libc::kill(pid, signal) };
        if result == 0 {
            Ok(())
        } else {
            Err(PortErrorCode::SignalFailed)
        }
    }
}

fn scan_local_ports(roots: &[PortWorkspaceRoot]) -> Result<Vec<PortRow>, PortErrorCode> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = roots;
        return Err(PortErrorCode::UnsupportedPlatform);
    }

    #[cfg(target_os = "macos")]
    {
        let runner = SystemCommandRunner;
        let output = run_lsof_with(&runner, PORT_COMMAND_TIMEOUT)?;
        let mut rows = parse_lsof_fields(&output)?;
        let pids = admitted_pids(&rows);
        let cwd_by_pid = query_cwds(&runner, &pids).unwrap_or_default();
        let command_by_pid = query_commands(&runner, &pids).unwrap_or_default();
        let protected_pid = std::process::id();

        for row in &mut rows {
            let command = command_by_pid.get(&row.pid).map(String::as_str);
            if row.pid == protected_pid
                || is_protected_agent_command(&row.process)
                || command.is_some_and(is_protected_agent_command)
            {
                row.ownership = PortOwnership::Protected;
                continue;
            }
            let cwd_owner = cwd_by_pid
                .get(&row.pid)
                .and_then(|path| workspace_match(path, roots));
            let command_owner = command.and_then(|value| workspace_from_command(value, roots));
            match cwd_owner.or(command_owner) {
                Some(WorkspaceMatch::Owned(root)) => {
                    row.workspace_id = Some(Arc::clone(&root.id));
                    row.workspace_name = Some(Arc::clone(&root.name));
                    row.ownership = PortOwnership::Workspace;
                }
                Some(WorkspaceMatch::Ambiguous) => row.ownership = PortOwnership::Ambiguous,
                None => row.ownership = PortOwnership::External,
            }
        }
        Ok(rows)
    }
}

fn admitted_pids(rows: &[PortRow]) -> Vec<u32> {
    let mut seen = HashSet::with_capacity(rows.len());
    rows.iter()
        .filter_map(|row| seen.insert(row.pid).then_some(row.pid))
        .collect()
}

fn query_cwds(
    runner: &impl CommandRunner,
    pids: &[u32],
) -> Result<HashMap<u32, PathBuf>, PortErrorCode> {
    if pids.is_empty() {
        return Ok(HashMap::new());
    }
    let joined = join_pids(pids);
    let args = vec![
        "-nP".to_owned(),
        "-a".to_owned(),
        "-p".to_owned(),
        joined,
        "-d".to_owned(),
        "cwd".to_owned(),
        "-F".to_owned(),
        "pn".to_owned(),
    ];
    let output = checked_output(runner.run("lsof", &args, PORT_COMMAND_TIMEOUT)?)?;
    parse_pid_paths(&output)
}

fn query_commands(
    runner: &impl CommandRunner,
    pids: &[u32],
) -> Result<HashMap<u32, String>, PortErrorCode> {
    if pids.is_empty() {
        return Ok(HashMap::new());
    }
    let args = vec![
        "-p".to_owned(),
        join_pids(pids),
        "-o".to_owned(),
        "pid=".to_owned(),
        "-o".to_owned(),
        "command=".to_owned(),
    ];
    let output = checked_output(runner.run("ps", &args, PORT_COMMAND_TIMEOUT)?)?;
    let text = std::str::from_utf8(&output).map_err(|_| PortErrorCode::MalformedOutput)?;
    let mut commands = HashMap::with_capacity(pids.len());
    for line in text.lines() {
        let Some((pid, command)) = line.trim().split_once(char::is_whitespace) else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        commands.insert(pid, command.trim().to_owned());
    }
    Ok(commands)
}

fn join_pids(pids: &[u32]) -> String {
    pids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_pid_paths(output: &[u8]) -> Result<HashMap<u32, PathBuf>, PortErrorCode> {
    let text = std::str::from_utf8(output).map_err(|_| PortErrorCode::MalformedOutput)?;
    let mut current_pid = None;
    let mut paths = HashMap::new();
    for line in text.lines() {
        match line.as_bytes().first().copied() {
            Some(b'p') => current_pid = line[1..].parse::<u32>().ok(),
            Some(b'n') => {
                if let Some(pid) = current_pid {
                    paths.insert(pid, PathBuf::from(&line[1..]));
                }
            }
            _ => {}
        }
    }
    Ok(paths)
}

fn workspace_from_command<'a>(
    command: &str,
    roots: &'a [PortWorkspaceRoot],
) -> Option<WorkspaceMatch<'a>> {
    let mut result = None;
    for token in command.split_whitespace() {
        let token = token.trim_matches(['\'', '"', ',', ';']);
        let Some(start) = token.find('/') else {
            continue;
        };
        let candidate = Path::new(&token[start..]);
        result = merge_workspace_matches(result, workspace_match(candidate, roots));
    }
    result
}

fn merge_workspace_matches<'a>(
    left: Option<WorkspaceMatch<'a>>,
    right: Option<WorkspaceMatch<'a>>,
) -> Option<WorkspaceMatch<'a>> {
    match (left, right) {
        (None, other) | (other, None) => other,
        (Some(WorkspaceMatch::Ambiguous), _) | (_, Some(WorkspaceMatch::Ambiguous)) => {
            Some(WorkspaceMatch::Ambiguous)
        }
        (Some(WorkspaceMatch::Owned(left)), Some(WorkspaceMatch::Owned(right))) => {
            if left.id == right.id {
                Some(WorkspaceMatch::Owned(left))
            } else {
                Some(WorkspaceMatch::Ambiguous)
            }
        }
    }
}

fn is_protected_agent_command(command: &str) -> bool {
    let command = command.to_ascii_lowercase();
    [
        "deppy", "claude", "codex", "opencode", "gemini", "kimi", "copilot",
    ]
    .iter()
    .any(|marker| command.contains(marker))
}

enum WorkspaceMatch<'a> {
    Owned(&'a PortWorkspaceRoot),
    Ambiguous,
}

fn assign_workspace<'a>(
    path: &Path,
    roots: &'a [PortWorkspaceRoot],
) -> Option<&'a PortWorkspaceRoot> {
    match workspace_match(path, roots) {
        Some(WorkspaceMatch::Owned(root)) => Some(root),
        Some(WorkspaceMatch::Ambiguous) | None => None,
    }
}

fn workspace_match<'a>(path: &Path, roots: &'a [PortWorkspaceRoot]) -> Option<WorkspaceMatch<'a>> {
    let path = lexical_path(path)?;
    let mut best: Option<(&PortWorkspaceRoot, usize)> = None;
    let mut ambiguous = false;
    for root in roots {
        let Some(root_path) = lexical_path(&root.path) else {
            continue;
        };
        if !path.starts_with(&root_path) {
            continue;
        }
        let depth = root_path.components().count();
        match best {
            Some((_, best_depth)) if best_depth > depth => {}
            Some((best_root, best_depth)) if best_depth == depth => {
                ambiguous |= best_root.id != root.id;
            }
            _ => {
                best = Some((root, depth));
                ambiguous = false;
            }
        }
    }
    if ambiguous {
        Some(WorkspaceMatch::Ambiguous)
    } else {
        best.map(|(root, _)| WorkspaceMatch::Owned(root))
    }
}

fn lexical_path(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
        }
    }
    Some(normalized)
}

fn parse_lsof_fields(output: &[u8]) -> Result<Vec<PortRow>, PortErrorCode> {
    let text = std::str::from_utf8(output).map_err(|_| PortErrorCode::MalformedOutput)?;
    let mut rows = Vec::new();
    let mut current_pid = None;
    let mut current_command: Arc<str> = Arc::from("");
    for line in text.lines() {
        let Some(field) = line.as_bytes().first().copied() else {
            continue;
        };
        let value = &line[1..];
        match field {
            b'p' => {
                current_pid = value.parse::<u32>().ok();
                current_command = Arc::from("");
            }
            b'c' => current_command = Arc::from(value),
            b'n' => {
                let pid = current_pid.ok_or(PortErrorCode::MalformedOutput)?;
                let (bind, port) = parse_listener_name(value)?;
                if rows.len() == PORT_ROW_MAX {
                    return Err(PortErrorCode::TooManyRows);
                }
                rows.push(PortRow {
                    pid,
                    port,
                    bind,
                    protocol: PortProtocol::Tcp,
                    process: Arc::clone(&current_command),
                    workspace_id: None,
                    workspace_name: None,
                    ownership: PortOwnership::External,
                });
            }
            _ => {}
        }
    }
    Ok(rows)
}

fn parse_listener_name(value: &str) -> Result<(Arc<str>, u16), PortErrorCode> {
    let value = value.strip_suffix(" (LISTEN)").unwrap_or(value);
    let (bind, port) = value
        .rsplit_once(':')
        .ok_or(PortErrorCode::MalformedOutput)?;
    let port = port
        .parse::<u16>()
        .map_err(|_| PortErrorCode::MalformedOutput)?;
    let bind = bind
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(bind);
    if bind.is_empty() {
        return Err(PortErrorCode::MalformedOutput);
    }
    Ok((Arc::from(bind), port))
}

trait CommandRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<CommandOutput, PortErrorCode>;
}

#[derive(PartialEq, Eq)]
struct CommandOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    overflow: bool,
    success: bool,
}

fn run_lsof_with(runner: &impl CommandRunner, timeout: Duration) -> Result<Vec<u8>, PortErrorCode> {
    let args = [
        "-nP".to_owned(),
        "-iTCP".to_owned(),
        "-sTCP:LISTEN".to_owned(),
        "-F".to_owned(),
        "pcn".to_owned(),
    ];
    checked_output(runner.run("lsof", &args, timeout)?)
}

fn checked_output(output: CommandOutput) -> Result<Vec<u8>, PortErrorCode> {
    if output.overflow
        || output.stdout.len().saturating_add(output.stderr.len()) > PORT_OUTPUT_MAX_BYTES
    {
        return Err(PortErrorCode::OutputTooLarge);
    }
    if !output.success {
        return Err(PortErrorCode::CommandFailed);
    }
    Ok(output.stdout)
}

struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<CommandOutput, PortErrorCode> {
        run_system_command(program, args, timeout)
    }
}

fn run_system_command(
    program: &str,
    args: &[String],
    timeout: Duration,
) -> Result<CommandOutput, PortErrorCode> {
    use std::os::unix::process::CommandExt;

    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: this async-signal-safe call runs in the child immediately before exec and only
    // creates a process group whose id is the child's pid.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let mut child = command.spawn().map_err(|_| PortErrorCode::SpawnFailed)?;
    let pid = child.id();
    record_test_process_group(pid);
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            kill_process_group(pid);
            let _ = child.wait();
            return Err(PortErrorCode::Io);
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            kill_process_group(pid);
            let _ = child.wait();
            return Err(PortErrorCode::Io);
        }
    };
    let retained = Arc::new(AtomicUsize::new(0));
    let overflow = Arc::new(AtomicBool::new(false));
    let stdout_reader = match spawn_reader(stdout, Arc::clone(&retained), Arc::clone(&overflow)) {
        Ok(reader) => reader,
        Err(error) => {
            kill_process_group(pid);
            let _ = child.wait();
            return Err(error);
        }
    };
    let stderr_reader = match spawn_reader(stderr, retained, Arc::clone(&overflow)) {
        Ok(reader) => reader,
        Err(error) => {
            kill_process_group(pid);
            let _ = child.wait();
            let _ = join_reader(stdout_reader);
            return Err(error);
        }
    };
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                timed_out = true;
                kill_process_group(pid);
                break child.wait().map_err(|_| PortErrorCode::Io);
            }
            Err(_) => {
                kill_process_group(pid);
                let _ = child.wait();
                break Err(PortErrorCode::Io);
            }
        }
    };
    let stdout = join_reader(stdout_reader)?;
    let stderr = join_reader(stderr_reader)?;
    if timed_out {
        return Err(PortErrorCode::Timeout);
    }
    let status = status?;
    Ok(CommandOutput {
        stdout,
        stderr,
        overflow: overflow.load(Ordering::Acquire),
        success: status.success(),
    })
}

fn spawn_reader(
    reader: impl Read + Send + 'static,
    retained: Arc<AtomicUsize>,
    overflow: Arc<AtomicBool>,
) -> Result<JoinHandle<Result<Vec<u8>, PortErrorCode>>, PortErrorCode> {
    std::thread::Builder::new()
        .name("port-output-reader".to_owned())
        .spawn(move || read_bounded(reader, &retained, &overflow))
        .map_err(|_| PortErrorCode::SpawnFailed)
}

fn read_bounded(
    mut reader: impl Read,
    retained: &AtomicUsize,
    overflow: &AtomicBool,
) -> Result<Vec<u8>, PortErrorCode> {
    let _reader_guard = ReaderCountGuard::new();
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).map_err(|_| PortErrorCode::Io)?;
        if read == 0 {
            return Ok(output);
        }
        let previous = retained
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                Some(value.saturating_add(read).min(PORT_OUTPUT_MAX_BYTES + 1))
            })
            .unwrap_or(PORT_OUTPUT_MAX_BYTES + 1);
        let available = PORT_OUTPUT_MAX_BYTES.saturating_sub(previous);
        let keep = available.min(read);
        output.extend_from_slice(&buffer[..keep]);
        if keep < read || previous > PORT_OUTPUT_MAX_BYTES {
            overflow.store(true, Ordering::Release);
        }
    }
}

fn join_reader(
    handle: JoinHandle<Result<Vec<u8>, PortErrorCode>>,
) -> Result<Vec<u8>, PortErrorCode> {
    handle.join().map_err(|_| PortErrorCode::Io)?
}

fn kill_process_group(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: a negative PID targets only the fresh child process group created above.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

static READER_THREAD_COUNT: AtomicUsize = AtomicUsize::new(0);

struct ReaderCountGuard;

impl ReaderCountGuard {
    fn new() -> Self {
        READER_THREAD_COUNT.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

impl Drop for ReaderCountGuard {
    fn drop(&mut self) {
        READER_THREAD_COUNT.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
static LAST_TEST_PROCESS_GROUP: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
fn record_test_process_group(pid: u32) {
    LAST_TEST_PROCESS_GROUP.store(pid as usize, Ordering::Release);
}

#[cfg(not(test))]
fn record_test_process_group(_pid: u32) {}

#[cfg(test)]
fn test_reader_thread_count() -> usize {
    READER_THREAD_COUNT.load(Ordering::Acquire)
}

#[cfg(test)]
fn run_test_hanging_command(timeout: Duration) -> Result<(), PortErrorCode> {
    let args = ["-c".to_owned(), "sleep 30".to_owned()];
    run_system_command("/bin/sh", &args, timeout).map(|_| ())
}

#[cfg(test)]
fn wait_until_process_group_is_gone(timeout: Duration) -> bool {
    let pid = LAST_TEST_PROCESS_GROUP.load(Ordering::Acquire);
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        // SAFETY: signal zero probes only the recorded test process group and sends no signal.
        let result = unsafe { libc::kill(-pid, 0) };
        if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}
