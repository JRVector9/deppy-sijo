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
    fn lsof_parser_deduplicates_identical_listener_file_descriptors_before_cap() {
        let mut input = b"p41\ncworkerd\n".to_vec();
        for file_descriptor in 0..=PORT_ROW_MAX {
            input.extend_from_slice(format!("f{file_descriptor}\nn127.0.0.1:3000\n").as_bytes());
        }
        input.extend_from_slice(b"n[::1]:3000\nn*:8443\n");

        let rows = parse_lsof_fields(&input).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows.iter().filter(|row| row.port == 3000).count(), 2);
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
    fn command_ownership_ignores_path_tokens_after_the_fixed_cap() {
        let roots = roots_fixture([("owned", "/tmp/project")]).unwrap();
        let normalized = normalize_roots(&roots).unwrap();
        let mut command = (0..COMMAND_PATH_TOKEN_MAX)
            .map(|index| format!("/external/{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        command.push_str(" /tmp/project/server.js");

        assert!(workspace_from_command(&command, &normalized).is_none());
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
    fn scanner_uses_only_trusted_absolute_lsof_and_ps_paths() {
        let runner = RecordingCommandRunner::new([
            b"p41\ncnode\nn*:3000\n".to_vec(),
            b"p41\nn/tmp/project\n".to_vec(),
            b"41 /usr/bin/node server.js\n".to_vec(),
        ]);

        run_lsof_with(&runner, Duration::from_millis(10)).unwrap();
        query_cwds(&runner, &[41]).unwrap();
        query_processes(&runner, &[41]).unwrap();

        assert_eq!(
            runner.programs(),
            [TRUSTED_LSOF_PATH, TRUSTED_LSOF_PATH, TRUSTED_PS_PATH]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn process_birth_token_distinguishes_same_second_reuse() {
        let original = encode_process_birth_identity(1_722_424_800, 41);
        let replacement = encode_process_birth_identity(1_722_424_800, 42);

        assert_ne!(original, replacement);
        assert_eq!(original.len(), PROCESS_BIRTH_TOKEN_LEN);
        assert_eq!(replacement.len(), PROCESS_BIRTH_TOKEN_LEN);
        let live = query_process_birth_with_operation(
            std::process::id(),
            &OperationContext::new(Duration::from_secs(1)),
        )
        .unwrap();
        assert_eq!(live.len(), PROCESS_BIRTH_TOKEN_LEN);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn timeout_kills_process_group_and_joins_readers() {
        let _process_guard = test_process_guard();
        let before = test_reader_thread_count();
        let result = run_test_hanging_command(Duration::from_millis(100));
        assert_eq!(result, Err(PortErrorCode::Timeout));
        assert!(wait_until_process_group_is_gone(Duration::from_secs(1)));
        assert_eq!(test_reader_thread_count(), before);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn operation_deadline_is_shared_across_multiple_commands() {
        let _process_guard = test_process_guard();
        // 느린 공유 CI 러너 대응(2026-08-04, GHA run 30868793934): 원래 120ms 예산은
        // 스폰+파이프+리더 스레드 준비에 50ms 이상이 드는 러너에서 첫 명령부터 예산을
        // 넘겨 실패했다. 예산을 2s로 키워 스폰 지연 ~1.8s까지 흡수하고(관측치의 20배+),
        // 두 번째 명령의 sleep 길이는 고정값 대신 `remaining() + 50ms`로 정해 스케줄링
        // 속도와 무관하게 "남은 예산 초과 → Timeout"이 결정적으로 발생하게 한다.
        // 50ms 마진은 원래 테스트(70ms sleep / 120ms 예산)와 같은 수준의
        // fresh-budget 회귀 식별력을 유지한다.
        let operation = OperationContext::new(Duration::from_secs(2));
        let started = std::time::Instant::now();
        assert!(run_test_sleeping_command(&operation, Duration::from_millis(70)).is_ok());
        let remaining = operation
            .remaining()
            .expect("first command fits the shared budget");
        assert_eq!(
            run_test_sleeping_command(&operation, remaining + Duration::from_millis(50)),
            Err(PortErrorCode::Timeout)
        );
        // kill+reap+리더 join 오버헤드만 허용하는 상한 — 데드라인 미적용 회귀는
        // Timeout 결과 assert가, 무한 대기 회귀는 이 상한이 잡는다.
        // 로컬 실측 오버헤드는 10ms 미만이라 500ms 여유도 매우 크다.
        assert!(started.elapsed() < Duration::from_millis(2500));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn output_overflow_kills_and_reaps_child_immediately() {
        let _process_guard = test_process_guard();
        reset_test_process_group();
        let before = test_reader_thread_count();
        let operation = OperationContext::new(Duration::from_secs(4));
        let started = std::time::Instant::now();

        assert_eq!(
            run_test_overflowing_command(&operation),
            Err(PortErrorCode::OutputTooLarge)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(wait_until_process_group_is_gone(Duration::from_secs(1)));
        assert_eq!(test_reader_thread_count(), before);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn clean_parent_descendant_cannot_hold_reader_join_past_deadline() {
        let _process_guard = test_process_guard();
        reset_test_process_group();
        let before = test_reader_thread_count();
        // `--test-threads=64` 과다구독(18 logical core) 아래서 간헐 실패 관측
        // (2026-08-15, 전체 스위트 64스레드 반복 캡처 fail64_25/52 — 둘 다 이 assert에서
        // panic, 즉 아래 run_test_clean_parent_descendant가 Err를 반환). 120ms 예산은
        // 스폰+파이프+리더 스레드 준비 지연을 못 견뎌 첫 폴에 Timeout으로 진다. 정상
        // 실행은 동일 조건 39회 실측 median 19.4ms/max 24.9ms로 120ms에 크게 못
        // 미치지만, 스케줄러 지연이 이 여유를 가끔 잡아먹는다. 예산을 2s로 키운다 —
        // 같은 파일의 operation_deadline_is_shared_across_multiple_commands가 동일한
        // 원인으로 이미 2s로 키워진 전례(e42ad7e, 2026-08-04)를 따른다. 이 예산은 실제
        // 회귀 탐지와 무관하다 — descendant(900ms)가 파이프를 붙잡는 회귀는
        // kill_process_group 이후의 join_reader에서 발생하고 거긴 deadline을 보지
        // 않으므로, 회귀 탐지는 전적으로 아래 elapsed<500ms assert가 담당한다.
        let operation = OperationContext::new(Duration::from_secs(2));
        let started = std::time::Instant::now();

        assert!(run_test_clean_parent_descendant(&operation, Duration::from_millis(900)).is_ok());
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(wait_until_process_group_is_gone(Duration::from_secs(1)));
        assert_eq!(test_reader_thread_count(), before);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dropping_inventory_worker_cancels_and_reaps_active_process_group() {
        let _process_guard = test_process_guard();
        reset_test_process_group();
        let before = test_reader_thread_count();
        let mut worker = test_hanging_inventory_worker();
        worker
            .try_request(PortJob::Scan {
                generation: 1,
                roots: Arc::from([]),
            })
            .unwrap();
        assert!(wait_until_test_process_group_starts(Duration::from_secs(1)));

        let started = std::time::Instant::now();
        drop(worker);

        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(wait_until_process_group_is_gone(Duration::from_secs(1)));
        assert_eq!(test_reader_thread_count(), before);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dropping_worker_does_not_wait_for_clean_parent_descendant_pipes() {
        let _process_guard = test_process_guard();
        reset_test_process_group();
        let before = test_reader_thread_count();
        let mut worker = test_clean_parent_descendant_worker(Duration::from_millis(900));
        worker
            .try_request(PortJob::Scan {
                generation: 1,
                roots: Arc::from([]),
            })
            .unwrap();
        // 스폰 기록(pid 등록)을 기다린다. 이 픽스처는 부모가 즉시 exit하고 러너가 그룹을
        // 곧바로 kill하므로 그룹 생존 창(~10-20ms)이 kill(-pgid, 0) 프로브 간격(10ms)
        // 사이에 빠질 수 있다 — wait_until_test_process_group_starts로는 그룹 생존을
        // 놓쳐 실패했다 (2026-08-04, 로컬 워크스페이스 런에서 재현). pid 기록은 스폰
        // 시점에 남고 그룹 사망 후에도 지워지지 않으므로 결정적이다.
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while LAST_TEST_PROCESS_GROUP.load(Ordering::Acquire) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "test process group not recorded"
            );
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(100));

        let started = std::time::Instant::now();
        drop(worker);

        assert!(started.elapsed() < Duration::from_millis(500));
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
        assert_eq!(backend.signaler().signals(), &[(77, TERMINATE_SIGNAL)]);
    }

    #[test]
    fn terminate_rejects_reused_pid_with_changed_birth_identity() {
        let mut reused = row_fixture("ws-a", 77, 3000, PortOwnership::Workspace);
        reused.process_started_at = encode_process_birth_identity(1_722_424_800, 42);
        let mut target = termination_fixture("ws-a", 77, 3000);
        target.process_started_at = encode_process_birth_identity(1_722_424_800, 41);
        let scanner = FakeScanner::new([Ok(vec![reused])]);
        let signaler = FakeSignaler::default();
        let mut backend = PortBackend::new(scanner, signaler, u32::MAX);

        assert_eq!(
            backend.terminate(target),
            Err(PortErrorCode::OwnershipChanged)
        );
        assert!(backend.signaler().signals().is_empty());
    }

    #[test]
    fn terminate_rechecks_birth_identity_immediately_before_sigterm() {
        let row = row_fixture("ws-a", 77, 3000, PortOwnership::Workspace);
        let scanner = FakeScanner::new([Ok(vec![row])]).with_births([Ok(Arc::from("birth-b"))]);
        let signaler = FakeSignaler::default();
        let mut backend = PortBackend::new(scanner, signaler, u32::MAX);

        assert_eq!(
            backend.terminate(termination_fixture("ws-a", 77, 3000)),
            Err(PortErrorCode::OwnershipChanged)
        );
        assert!(backend.signaler().signals().is_empty());
    }

    #[test]
    fn worker_is_inert_until_first_request_and_allows_one_outstanding_job() {
        #[cfg(target_os = "macos")]
        let _process_guard = test_process_guard();
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
            process_started_at: Arc::from("birth-a"),
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
            process_started_at: Arc::from("birth-a"),
        }
    }

    struct FakeCommandRunner {
        stdout: Vec<u8>,
    }

    struct RecordingCommandRunner {
        outputs: std::sync::Mutex<std::collections::VecDeque<Vec<u8>>>,
        programs: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingCommandRunner {
        fn new<const N: usize>(outputs: [Vec<u8>; N]) -> Self {
            Self {
                outputs: std::sync::Mutex::new(outputs.into()),
                programs: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn programs(&self) -> Vec<String> {
            self.programs.lock().unwrap().clone()
        }
    }

    impl CommandRunner for RecordingCommandRunner {
        fn run(
            &self,
            program: &str,
            _args: &[String],
            _operation: &OperationContext,
        ) -> Result<CommandOutput, PortErrorCode> {
            self.programs.lock().unwrap().push(program.to_owned());
            Ok(CommandOutput {
                stdout: self.outputs.lock().unwrap().pop_front().unwrap(),
                stderr: Vec::new(),
                overflow: false,
                success: true,
            })
        }
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
            _operation: &OperationContext,
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
        births: std::collections::VecDeque<Result<Arc<str>, PortErrorCode>>,
    }

    impl FakeScanner {
        fn new<const N: usize>(scans: [Result<Vec<PortRow>, PortErrorCode>; N]) -> Self {
            Self {
                scans: scans.into(),
                births: std::iter::repeat_n(Ok(Arc::from("birth-a")), N.max(1)).collect(),
            }
        }

        fn with_births<const N: usize>(
            mut self,
            births: [Result<Arc<str>, PortErrorCode>; N],
        ) -> Self {
            self.births = births.into();
            self
        }
    }

    impl PortScanner for FakeScanner {
        fn scan(&mut self, _operation: &OperationContext) -> Result<Vec<PortRow>, PortErrorCode> {
            self.scans.pop_front().expect("fixture scan")
        }

        fn birth_identity(
            &mut self,
            _pid: u32,
            _operation: &OperationContext,
        ) -> Result<Arc<str>, PortErrorCode> {
            self.births.pop_front().expect("fixture birth identity")
        }
    }
}

use crate::lazy_worker::LazyBoundedWorker;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(target_os = "macos")]
use std::io::Read;
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};
#[cfg(target_os = "macos")]
use std::sync::atomic::AtomicUsize;
#[cfg(target_os = "macos")]
use std::thread::JoinHandle;

pub(crate) const PORT_ROW_MAX: usize = 200;
const PORT_OUTPUT_MAX_BYTES: usize = 2 * 1024 * 1024;
const PORT_COMMAND_TIMEOUT: Duration = Duration::from_secs(4);
const PORT_WORKER_IDLE_TTL: Duration = Duration::from_secs(30);
const TRUSTED_LSOF_PATH: &str = "/usr/sbin/lsof";
const TRUSTED_PS_PATH: &str = "/bin/ps";
const COMMAND_PATH_TOKEN_MAX: usize = 64;
const PROCESS_BIRTH_TOKEN_LEN: usize = 32;

#[cfg(target_os = "macos")]
const TERMINATE_SIGNAL: i32 = libc::SIGTERM;
#[cfg(not(target_os = "macos"))]
const TERMINATE_SIGNAL: i32 = 15;

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
    Cancelled,
}

#[derive(Clone)]
struct OperationContext {
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

impl OperationContext {
    fn new(timeout: Duration) -> Self {
        Self::with_cancellation(timeout, Arc::new(AtomicBool::new(false)))
    }

    fn with_cancellation(timeout: Duration, cancelled: Arc<AtomicBool>) -> Self {
        Self {
            deadline: Instant::now() + timeout,
            cancelled,
        }
    }

    fn check(&self) -> Result<(), PortErrorCode> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(PortErrorCode::Cancelled)
        } else if Instant::now() >= self.deadline {
            Err(PortErrorCode::Timeout)
        } else {
            Ok(())
        }
    }

    fn remaining(&self) -> Result<Duration, PortErrorCode> {
        self.check()?;
        Ok(self.deadline.saturating_duration_since(Instant::now()))
    }
}

fn encode_process_birth_identity(seconds: u64, microseconds: u64) -> Arc<str> {
    let encoded = format!("{seconds:016x}{microseconds:016x}");
    debug_assert_eq!(encoded.len(), PROCESS_BIRTH_TOKEN_LEN);
    Arc::from(encoded)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    pub process_started_at: Arc<str>,
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
    pub process_started_at: Arc<str>,
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

pub(crate) struct PortInventoryWorker {
    inner: Option<LazyBoundedWorker<PortJob, PortOutcome>>,
    cancellation: Arc<AtomicBool>,
}

impl PortInventoryWorker {
    pub(crate) fn try_request(
        &mut self,
        job: PortJob,
    ) -> Result<(), crate::lazy_worker::LazyWorkerSubmitError<PortJob>> {
        self.inner
            .as_mut()
            .expect("port worker exists until drop")
            .try_request(job)
    }

    pub(crate) fn try_recv(
        &mut self,
    ) -> Option<crate::lazy_worker::LazyWorkerOutcome<PortOutcome>> {
        self.inner
            .as_mut()
            .expect("port worker exists until drop")
            .try_recv()
    }

    #[cfg(test)]
    fn has_live_worker(&mut self) -> bool {
        self.inner
            .as_mut()
            .expect("port worker exists until drop")
            .has_live_worker()
    }
}

impl fmt::Debug for PortInventoryWorker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PortInventoryWorker")
            .field("has_inner", &self.inner.is_some())
            .finish()
    }
}

impl Drop for PortInventoryWorker {
    fn drop(&mut self) {
        self.cancellation.store(true, Ordering::Release);
        drop(self.inner.take());
    }
}

pub(crate) fn worker(wake: impl Fn() + Send + Sync + 'static) -> PortInventoryWorker {
    let cancellation = Arc::new(AtomicBool::new(false));
    let executor_cancellation = Arc::clone(&cancellation);
    let inner = LazyBoundedWorker::new(
        "port-inventory",
        PORT_WORKER_IDLE_TTL,
        move || {
            let cancellation = Arc::clone(&executor_cancellation);
            move |job| execute_job(job, &cancellation)
        },
        wake,
    );
    PortInventoryWorker {
        inner: Some(inner),
        cancellation,
    }
}

fn execute_job(job: PortJob, cancellation: &Arc<AtomicBool>) -> PortOutcome {
    let operation =
        OperationContext::with_cancellation(PORT_COMMAND_TIMEOUT, Arc::clone(cancellation));
    match job {
        PortJob::Scan { generation, roots } => {
            PortOutcome::Scanned(scan_snapshot(generation, &roots, &operation))
        }
        PortJob::Terminate {
            generation,
            roots,
            target,
        } => PortOutcome::Terminated(terminate_and_rescan(generation, &roots, target, &operation)),
    }
}

fn scan_snapshot(
    generation: u64,
    roots: &[PortWorkspaceRoot],
    operation: &OperationContext,
) -> Result<PortSnapshot, PortErrorCode> {
    let normalized_roots = normalize_roots(roots)?;
    let rows = scan_local_ports(&normalized_roots, operation)?;
    Ok(snapshot(generation, rows))
}

fn terminate_and_rescan(
    generation: u64,
    roots: &[PortWorkspaceRoot],
    target: PortTerminationTarget,
    operation: &OperationContext,
) -> Result<PortSnapshot, PortErrorCode> {
    let scanner = SystemPortScanner::new(normalize_roots(roots)?);
    let signaler = SystemSignaler;
    let mut backend = PortBackend::new(scanner, signaler, std::process::id());
    let rows = backend.terminate_with_operation(target, operation)?;
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
    fn scan(&mut self, operation: &OperationContext) -> Result<Vec<PortRow>, PortErrorCode>;
    fn birth_identity(
        &mut self,
        pid: u32,
        operation: &OperationContext,
    ) -> Result<Arc<str>, PortErrorCode>;
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
        let operation = OperationContext::new(PORT_COMMAND_TIMEOUT);
        self.terminate_with_operation(target, &operation)
    }

    fn terminate_with_operation(
        &mut self,
        target: PortTerminationTarget,
        operation: &OperationContext,
    ) -> Result<Vec<PortRow>, PortErrorCode> {
        if target.pid == self.protected_pid {
            return Err(PortErrorCode::NotTerminable);
        }
        let rows = self.scanner.scan(operation)?;
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
        if self.scanner.birth_identity(target.pid, operation)? != target.process_started_at {
            return Err(PortErrorCode::OwnershipChanged);
        }
        operation.check()?;
        self.signaler.signal(target.pid, TERMINATE_SIGNAL)?;
        self.scanner.scan(operation)
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
            && row.process_started_at == self.process_started_at
            && row.workspace_id.as_deref() == Some(self.workspace_id.as_ref())
    }
}

struct SystemPortScanner {
    roots: Vec<NormalizedRoot>,
}

impl SystemPortScanner {
    fn new(roots: Vec<NormalizedRoot>) -> Self {
        Self { roots }
    }
}

impl PortScanner for SystemPortScanner {
    fn scan(&mut self, operation: &OperationContext) -> Result<Vec<PortRow>, PortErrorCode> {
        scan_local_ports(&self.roots, operation)
    }

    fn birth_identity(
        &mut self,
        pid: u32,
        operation: &OperationContext,
    ) -> Result<Arc<str>, PortErrorCode> {
        query_process_birth_with_operation(pid, operation)
    }
}

struct SystemSignaler;

impl ProcessSignaler for SystemSignaler {
    fn signal(&mut self, pid: u32, signal: i32) -> Result<(), PortErrorCode> {
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (pid, signal);
            return Err(PortErrorCode::UnsupportedPlatform);
        }

        #[cfg(target_os = "macos")]
        {
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
}

fn scan_local_ports(
    roots: &[NormalizedRoot],
    operation: &OperationContext,
) -> Result<Vec<PortRow>, PortErrorCode> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (roots, operation);
        return Err(PortErrorCode::UnsupportedPlatform);
    }

    #[cfg(target_os = "macos")]
    {
        let runner = SystemCommandRunner;
        let output = run_lsof_with_operation(&runner, operation)?;
        let mut rows = parse_lsof_fields(&output)?;
        let pids = admitted_pids(&rows);
        let cwd_by_pid = query_cwds_with_operation(&runner, &pids, operation)?;
        let birth_by_pid = query_process_births(&pids, operation)?;
        let process_by_pid = query_processes_with_operation(&runner, &pids, operation)?;
        classify_rows(
            &mut rows,
            roots,
            &cwd_by_pid,
            &birth_by_pid,
            &process_by_pid,
            std::process::id(),
        );
        Ok(rows)
    }
}

#[derive(Clone)]
struct PidClassification {
    process_started_at: Arc<str>,
    workspace_id: Option<Arc<str>>,
    workspace_name: Option<Arc<str>>,
    ownership: PortOwnership,
}

fn classify_rows(
    rows: &mut [PortRow],
    roots: &[NormalizedRoot],
    cwd_by_pid: &HashMap<u32, PathBuf>,
    birth_by_pid: &HashMap<u32, Arc<str>>,
    process_by_pid: &HashMap<u32, ProcessInfo>,
    protected_pid: u32,
) {
    let mut classifications = HashMap::with_capacity(rows.len().min(PORT_ROW_MAX));
    for row in rows.iter() {
        classifications.entry(row.pid).or_insert_with(|| {
            classify_pid(
                row,
                roots,
                cwd_by_pid.get(&row.pid),
                birth_by_pid.get(&row.pid),
                process_by_pid.get(&row.pid),
                protected_pid,
            )
        });
    }
    for row in rows {
        let Some(classification) = classifications.get(&row.pid) else {
            continue;
        };
        row.process_started_at = Arc::clone(&classification.process_started_at);
        row.workspace_id = classification.workspace_id.clone();
        row.workspace_name = classification.workspace_name.clone();
        row.ownership = classification.ownership;
    }
}

fn classify_pid(
    row: &PortRow,
    roots: &[NormalizedRoot],
    cwd: Option<&PathBuf>,
    process_birth: Option<&Arc<str>>,
    process: Option<&ProcessInfo>,
    protected_pid: u32,
) -> PidClassification {
    let process_started_at = process_birth.map_or_else(|| Arc::from(""), Arc::clone);
    let command = process.map(|process| process.command.as_str());
    if row.pid == protected_pid
        || is_protected_agent_command(&row.process)
        || command.is_some_and(is_protected_agent_command)
    {
        return PidClassification {
            process_started_at,
            workspace_id: None,
            workspace_name: None,
            ownership: PortOwnership::Protected,
        };
    }
    if process_started_at.is_empty() {
        return PidClassification {
            process_started_at,
            workspace_id: None,
            workspace_name: None,
            ownership: PortOwnership::External,
        };
    }
    let cwd_owner = cwd.and_then(|path| workspace_match(path, roots));
    let command_owner = command.and_then(|value| workspace_from_command(value, roots));
    let (workspace_id, workspace_name, ownership) = match cwd_owner.or(command_owner) {
        Some(WorkspaceMatch::Owned(root)) => (
            Some(Arc::clone(&root.id)),
            Some(Arc::clone(&root.name)),
            PortOwnership::Workspace,
        ),
        Some(WorkspaceMatch::Ambiguous) => (None, None, PortOwnership::Ambiguous),
        None => (None, None, PortOwnership::External),
    };
    PidClassification {
        process_started_at,
        workspace_id,
        workspace_name,
        ownership,
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
    let operation = OperationContext::new(PORT_COMMAND_TIMEOUT);
    query_cwds_with_operation(runner, pids, &operation)
}

fn query_cwds_with_operation(
    runner: &impl CommandRunner,
    pids: &[u32],
    operation: &OperationContext,
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
    let output = checked_output(runner.run(TRUSTED_LSOF_PATH, &args, operation)?)?;
    parse_pid_paths(&output)
}

struct ProcessInfo {
    command: String,
}

fn query_processes(
    runner: &impl CommandRunner,
    pids: &[u32],
) -> Result<HashMap<u32, ProcessInfo>, PortErrorCode> {
    let operation = OperationContext::new(PORT_COMMAND_TIMEOUT);
    query_processes_with_operation(runner, pids, &operation)
}

fn query_processes_with_operation(
    runner: &impl CommandRunner,
    pids: &[u32],
    operation: &OperationContext,
) -> Result<HashMap<u32, ProcessInfo>, PortErrorCode> {
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
    let output = checked_output(runner.run(TRUSTED_PS_PATH, &args, operation)?)?;
    let text = std::str::from_utf8(&output).map_err(|_| PortErrorCode::MalformedOutput)?;
    let mut processes = HashMap::with_capacity(pids.len());
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(pid) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        processes.insert(
            pid,
            ProcessInfo {
                command: fields
                    .take(COMMAND_PATH_TOKEN_MAX)
                    .collect::<Vec<_>>()
                    .join(" "),
            },
        );
    }
    Ok(processes)
}

fn query_process_birth_with_operation(
    pid: u32,
    operation: &OperationContext,
) -> Result<Arc<str>, PortErrorCode> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (pid, operation);
        return Err(PortErrorCode::UnsupportedPlatform);
    }

    #[cfg(target_os = "macos")]
    {
        // proc_pidinfo(PROC_PIDTBSDINFO) FFI는 crate::proc_info::pid_start_time으로 옮겼다 —
        // 이 함수가 같은 syscall을 별도 unsafe 블록으로 중복 구현하고 있었다
        // (2026-08-14, proc-info-consolidate). 취소/타임아웃 확인 지점(FFI 앞뒤 2회)은
        // 그대로 유지한다.
        operation.check()?;
        let birth = crate::proc_info::pid_start_time(pid).ok_or(PortErrorCode::OwnershipChanged)?;
        operation.check()?;
        Ok(encode_process_birth_identity(
            birth.seconds,
            birth.microseconds,
        ))
    }
}

fn query_process_births(
    pids: &[u32],
    operation: &OperationContext,
) -> Result<HashMap<u32, Arc<str>>, PortErrorCode> {
    let mut births = HashMap::with_capacity(pids.len());
    for pid in pids {
        match query_process_birth_with_operation(*pid, operation) {
            Ok(identity) => {
                births.insert(*pid, identity);
            }
            Err(PortErrorCode::OwnershipChanged) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(births)
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
    roots: &'a [NormalizedRoot],
) -> Option<WorkspaceMatch<'a>> {
    let mut result = None;
    for token in command.split_whitespace().take(COMMAND_PATH_TOKEN_MAX) {
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
    Owned(&'a NormalizedRoot),
    Ambiguous,
}

#[derive(Clone)]
struct NormalizedRoot {
    id: Arc<str>,
    name: Arc<str>,
    path: PathBuf,
    depth: usize,
}

fn normalize_roots(roots: &[PortWorkspaceRoot]) -> Result<Vec<NormalizedRoot>, PortErrorCode> {
    if roots.len() > PORT_ROW_MAX {
        return Err(PortErrorCode::TooManyRows);
    }
    Ok(roots
        .iter()
        .filter_map(|root| {
            let path = lexical_path(&root.path)?;
            let depth = path.components().count();
            Some(NormalizedRoot {
                id: Arc::clone(&root.id),
                name: Arc::clone(&root.name),
                path,
                depth,
            })
        })
        .collect())
}

fn assign_workspace(path: &Path, roots: &[PortWorkspaceRoot]) -> Option<NormalizedRoot> {
    let normalized = normalize_roots(roots).ok()?;
    match workspace_match(path, &normalized) {
        Some(WorkspaceMatch::Owned(root)) => Some(root.clone()),
        Some(WorkspaceMatch::Ambiguous) | None => None,
    }
}

fn workspace_match<'a>(path: &Path, roots: &'a [NormalizedRoot]) -> Option<WorkspaceMatch<'a>> {
    let path = lexical_path(path)?;
    let mut best: Option<(&NormalizedRoot, usize)> = None;
    let mut ambiguous = false;
    for root in roots {
        if !path.starts_with(&root.path) {
            continue;
        }
        let depth = root.depth;
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
    let mut admitted = HashSet::new();
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
                let stable_key = (pid, Arc::clone(&bind), port, PortProtocol::Tcp);
                if !admitted.insert(stable_key) {
                    continue;
                }
                if rows.len() == PORT_ROW_MAX {
                    return Err(PortErrorCode::TooManyRows);
                }
                rows.push(PortRow {
                    pid,
                    port,
                    bind,
                    protocol: PortProtocol::Tcp,
                    process: Arc::clone(&current_command),
                    process_started_at: Arc::from(""),
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
        operation: &OperationContext,
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
    let operation = OperationContext::new(timeout);
    run_lsof_with_operation(runner, &operation)
}

fn run_lsof_with_operation(
    runner: &impl CommandRunner,
    operation: &OperationContext,
) -> Result<Vec<u8>, PortErrorCode> {
    let args = [
        "-nP".to_owned(),
        "-iTCP".to_owned(),
        "-sTCP:LISTEN".to_owned(),
        "-F".to_owned(),
        "pcn".to_owned(),
    ];
    checked_output(runner.run(TRUSTED_LSOF_PATH, &args, operation)?)
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

#[cfg(target_os = "macos")]
impl CommandRunner for SystemCommandRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        operation: &OperationContext,
    ) -> Result<CommandOutput, PortErrorCode> {
        run_system_command_with_operation(program, args, operation)
    }
}

#[cfg(not(target_os = "macos"))]
impl CommandRunner for SystemCommandRunner {
    fn run(
        &self,
        _program: &str,
        _args: &[String],
        _operation: &OperationContext,
    ) -> Result<CommandOutput, PortErrorCode> {
        Err(PortErrorCode::UnsupportedPlatform)
    }
}

#[cfg(target_os = "macos")]
fn run_system_command(
    program: &str,
    args: &[String],
    timeout: Duration,
) -> Result<CommandOutput, PortErrorCode> {
    let operation = OperationContext::new(timeout);
    run_system_command_with_operation(program, args, &operation)
}

#[cfg(target_os = "macos")]
fn run_system_command_with_operation(
    program: &str,
    args: &[String],
    operation: &OperationContext,
) -> Result<CommandOutput, PortErrorCode> {
    use std::os::unix::process::CommandExt;

    operation.check()?;
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
    let mut terminal_error = None;
    let status = loop {
        if overflow.load(Ordering::Acquire) {
            terminal_error = Some(PortErrorCode::OutputTooLarge);
            kill_process_group(pid);
            break child.wait().map_err(|_| PortErrorCode::Io);
        }
        match observe_child_exit_without_reaping(pid) {
            Ok(true) => {
                kill_process_group(pid);
                break child.wait().map_err(|_| PortErrorCode::Io);
            }
            Ok(false) => match operation.remaining() {
                Ok(remaining) => std::thread::sleep(remaining.min(Duration::from_millis(10))),
                Err(error) => {
                    terminal_error = Some(error);
                    kill_process_group(pid);
                    break child.wait().map_err(|_| PortErrorCode::Io);
                }
            },
            Err(error) => {
                terminal_error = Some(error);
                kill_process_group(pid);
                let _ = child.wait();
                break Err(error);
            }
        }
    };
    let stdout = join_reader(stdout_reader)?;
    let stderr = join_reader(stderr_reader)?;
    if let Some(error) = terminal_error {
        return Err(error);
    }
    let status = status?;
    Ok(CommandOutput {
        stdout,
        stderr,
        overflow: overflow.load(Ordering::Acquire),
        success: status.success(),
    })
}

#[cfg(target_os = "macos")]
fn observe_child_exit_without_reaping(pid: u32) -> Result<bool, PortErrorCode> {
    let pid = i32::try_from(pid).map_err(|_| PortErrorCode::Io)?;
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: `info` is a valid writable siginfo buffer. `WNOWAIT` keeps the exact child zombie
    // owned by this process, preventing PID/process-group reuse until `Child::wait` reaps it.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return Err(PortErrorCode::Io);
    }
    // SAFETY: successful `waitid` initialized `info`; with `WNOHANG`, `si_pid == 0` means no
    // matching state change, while this exact PID means the still-unreaped child exited.
    let observed_pid = unsafe { info.assume_init().si_pid() };
    Ok(observed_pid == pid)
}

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
fn join_reader(
    handle: JoinHandle<Result<Vec<u8>, PortErrorCode>>,
) -> Result<Vec<u8>, PortErrorCode> {
    handle.join().map_err(|_| PortErrorCode::Io)?
}

#[cfg(target_os = "macos")]
fn kill_process_group(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: a negative PID targets only the fresh child process group created above.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

#[cfg(target_os = "macos")]
static READER_THREAD_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(target_os = "macos")]
struct ReaderCountGuard;

#[cfg(target_os = "macos")]
impl ReaderCountGuard {
    fn new() -> Self {
        READER_THREAD_COUNT.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

#[cfg(target_os = "macos")]
impl Drop for ReaderCountGuard {
    fn drop(&mut self) {
        READER_THREAD_COUNT.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(all(test, target_os = "macos"))]
static LAST_TEST_PROCESS_GROUP: AtomicUsize = AtomicUsize::new(0);
#[cfg(all(test, target_os = "macos"))]
static TEST_PROCESS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(all(test, target_os = "macos"))]
fn test_process_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_PROCESS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(all(test, target_os = "macos"))]
fn record_test_process_group(pid: u32) {
    LAST_TEST_PROCESS_GROUP.store(pid as usize, Ordering::Release);
}

#[cfg(all(not(test), target_os = "macos"))]
fn record_test_process_group(_pid: u32) {}

#[cfg(all(test, target_os = "macos"))]
fn test_reader_thread_count() -> usize {
    READER_THREAD_COUNT.load(Ordering::Acquire)
}

#[cfg(all(test, target_os = "macos"))]
fn run_test_hanging_command(timeout: Duration) -> Result<(), PortErrorCode> {
    let args = ["-c".to_owned(), "sleep 30".to_owned()];
    run_system_command("/bin/sh", &args, timeout).map(|_| ())
}

#[cfg(all(test, target_os = "macos"))]
fn run_test_sleeping_command(
    operation: &OperationContext,
    duration: Duration,
) -> Result<(), PortErrorCode> {
    let seconds = format!("{:.3}", duration.as_secs_f64());
    run_system_command_with_operation("/bin/sleep", &[seconds], operation).map(|_| ())
}

#[cfg(all(test, target_os = "macos"))]
fn run_test_overflowing_command(operation: &OperationContext) -> Result<(), PortErrorCode> {
    run_system_command_with_operation(
        "/usr/bin/yes",
        &["port-inventory-overflow".to_owned()],
        operation,
    )
    .map(|_| ())
}

#[cfg(all(test, target_os = "macos"))]
fn run_test_clean_parent_descendant(
    operation: &OperationContext,
    descendant_lifetime: Duration,
) -> Result<(), PortErrorCode> {
    let command = format!("sleep {:.3} & exit 0", descendant_lifetime.as_secs_f64());
    run_system_command_with_operation("/bin/sh", &["-c".to_owned(), command], operation).map(|_| ())
}

#[cfg(all(test, target_os = "macos"))]
fn test_hanging_inventory_worker() -> PortInventoryWorker {
    let cancellation = Arc::new(AtomicBool::new(false));
    let executor_cancellation = Arc::clone(&cancellation);
    let inner = LazyBoundedWorker::new(
        "port-inventory-test",
        PORT_WORKER_IDLE_TTL,
        move || {
            let cancellation = Arc::clone(&executor_cancellation);
            move |_job| {
                let operation = OperationContext::with_cancellation(
                    Duration::from_secs(30),
                    Arc::clone(&cancellation),
                );
                let result = run_system_command_with_operation(
                    "/bin/sh",
                    &["-c".to_owned(), "sleep 30".to_owned()],
                    &operation,
                )
                .map(|_| snapshot(1, Vec::new()));
                PortOutcome::Scanned(result)
            }
        },
        || {},
    );
    PortInventoryWorker {
        inner: Some(inner),
        cancellation,
    }
}

#[cfg(all(test, target_os = "macos"))]
fn test_clean_parent_descendant_worker(descendant_lifetime: Duration) -> PortInventoryWorker {
    let cancellation = Arc::new(AtomicBool::new(false));
    let executor_cancellation = Arc::clone(&cancellation);
    let inner = LazyBoundedWorker::new(
        "port-descendant-test",
        PORT_WORKER_IDLE_TTL,
        move || {
            let cancellation = Arc::clone(&executor_cancellation);
            move |_job| {
                let operation = OperationContext::with_cancellation(
                    Duration::from_secs(30),
                    Arc::clone(&cancellation),
                );
                let result = run_test_clean_parent_descendant(&operation, descendant_lifetime)
                    .map(|_| snapshot(1, Vec::new()));
                PortOutcome::Scanned(result)
            }
        },
        || {},
    );
    PortInventoryWorker {
        inner: Some(inner),
        cancellation,
    }
}

#[cfg(all(test, target_os = "macos"))]
fn reset_test_process_group() {
    LAST_TEST_PROCESS_GROUP.store(0, Ordering::Release);
}

#[cfg(all(test, target_os = "macos"))]
fn wait_until_test_process_group_starts(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let pid = LAST_TEST_PROCESS_GROUP.load(Ordering::Acquire);
        if let Ok(pid) = i32::try_from(pid)
            && pid > 0
        {
            // SAFETY: signal zero probes only the fresh test process group and sends no signal.
            let result = unsafe { libc::kill(-pid, 0) };
            if result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[cfg(all(test, target_os = "macos"))]
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
