//! 앱에서 git CLI를 실행하는 공용 헬퍼 (2026-07-17, PR-D/PR-W 선행 배치).
//!
//! 이 앱은 지금까지 git 바이너리를 실행한 적이 없다(gitignore는 파일 파싱만) — diff
//! 패널(PR-D)과 워크트리 셀(PR-W)이 최초 사용자다. 두 PR이 병렬로 개발되므로 중복
//! 구현을 막기 위해 헬퍼를 먼저 둔다. 규칙:
//!
//! - **절대경로 [`GIT_BIN`]**: launchd로 뜬 GUI 앱의 PATH는 빈약해 `git` 이름 해석을
//!   믿을 수 없다. macOS는 Command Line Tools가 있으면 `/usr/bin/git` 셔틀이 항상 있다
//!   (미설치면 실행이 에러로 떨어지고, 호출측이 사용자에게 표면화한다 — 조용한 실패 금지).
//! - **UI 스레드 호출 금지**: 블로킹 함수다. 백그라운드 스레드에서만 호출한다
//!   (hover_cwd / inbox tail과 같은 관례 — 스레드는 호출측이 소유).
//! - **타임아웃**: 자식이 매달리면 kill — 좀비/무한 대기 방지. stdout/stderr는 파이프
//!   가득참 데드락을 피하려고 리더 스레드로 계속 비운다(diff는 64KB 파이프 버퍼를
//!   쉽게 넘는다).

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;

/// macOS CLT의 git 셔틀 — PATH 비의존.
pub const GIT_BIN: &str = "/usr/bin/git";

/// Test/setup callers that do not need a tighter semantic ceiling still cannot retain arbitrary
/// command output. Every production consumer uses [`run_git_bounded`] with a command-specific
/// limit and validates item count before acting on the result.
#[cfg(test)]
const DEFAULT_STDOUT_MAX_BYTES: usize = 8 * 1024 * 1024;
const STDERR_MAX_BYTES: usize = 64 * 1024;
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug)]
struct BoundedCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

fn read_bounded(
    mut reader: impl Read,
    max_bytes: usize,
    limit_reached: &AtomicBool,
    retain: bool,
) -> std::io::Result<BoundedCapture> {
    let mut bytes = Vec::with_capacity(if retain { max_bytes.min(64 * 1024) } else { 0 });
    let capture_bytes = max_bytes.saturating_add(1);
    let mut observed_bytes = 0usize;
    let mut chunk = [0u8; 64 * 1024];
    while observed_bytes < capture_bytes {
        let remaining = capture_bytes - observed_bytes;
        let chunk_len = chunk.len();
        let read = reader.read(&mut chunk[..remaining.min(chunk_len)])?;
        if read == 0 {
            break;
        }
        observed_bytes += read;
        if retain {
            bytes.extend_from_slice(&chunk[..read]);
        }
    }
    let truncated = observed_bytes > max_bytes;
    if truncated {
        if retain {
            bytes.truncate(max_bytes);
        }
        limit_reached.store(true, Ordering::Release);
    }
    Ok(BoundedCapture { bytes, truncated })
}

struct RunningGit {
    child: Child,
    reaped: bool,
    stdout_reader: Option<std::thread::JoinHandle<std::io::Result<BoundedCapture>>>,
    stderr_reader: Option<std::thread::JoinHandle<std::io::Result<BoundedCapture>>>,
    stdout_limit_reached: Arc<AtomicBool>,
    stderr_limit_reached: Arc<AtomicBool>,
    #[cfg(test)]
    active_readers: Arc<std::sync::atomic::AtomicUsize>,
}

impl RunningGit {
    fn spawn(
        repo: &Path,
        args: &[&str],
        stdout_max_bytes: usize,
        stderr_max_bytes: usize,
    ) -> anyhow::Result<Self> {
        let mut command = Command::new(GIT_BIN);
        command
            .arg("-C")
            .arg(repo)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Each command owns a process group. Timeout/output-limit cleanup therefore closes pipes
        // inherited by helpers too, so joining the two fixed readers cannot wait on descendants.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .map_err(|_| anyhow::anyhow!("git_spawn_failed"))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_limit_reached = Arc::new(AtomicBool::new(false));
        let stderr_limit_reached = Arc::new(AtomicBool::new(false));
        #[cfg(test)]
        let active_readers = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut running = Self {
            child,
            reaped: false,
            stdout_reader: None,
            stderr_reader: None,
            stdout_limit_reached,
            stderr_limit_reached,
            #[cfg(test)]
            active_readers,
        };

        let stdout = stdout.context("git stdout pipe missing")?;
        let stdout_limit = Arc::clone(&running.stdout_limit_reached);
        #[cfg(test)]
        let stdout_active = Arc::clone(&running.active_readers);
        running.stdout_reader = Some(
            std::thread::Builder::new()
                .name("git-stdout".to_owned())
                .spawn(move || {
                    #[cfg(test)]
                    let _reader = TestReaderGuard::new(stdout_active);
                    read_bounded(stdout, stdout_max_bytes, &stdout_limit, true)
                })
                .context("git stdout reader thread spawn failed")?,
        );
        let stderr = stderr.context("git stderr pipe missing")?;
        let stderr_limit = Arc::clone(&running.stderr_limit_reached);
        #[cfg(test)]
        let stderr_active = Arc::clone(&running.active_readers);
        running.stderr_reader = Some(
            std::thread::Builder::new()
                .name("git-stderr".to_owned())
                .spawn(move || {
                    #[cfg(test)]
                    let _reader = TestReaderGuard::new(stderr_active);
                    read_bounded(stderr, stderr_max_bytes, &stderr_limit, false)
                })
                .context("git stderr reader thread spawn failed")?,
        );
        Ok(running)
    }

    fn output_limit_reached(&self) -> bool {
        self.stdout_limit_reached.load(Ordering::Acquire)
            || self.stderr_limit_reached.load(Ordering::Acquire)
    }

    #[cfg(unix)]
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        // WNOWAIT observes the exit while deliberately keeping the group leader as a zombie.
        // Its pid/pgid therefore cannot be reused before descendants are killed and pipes close.
        // SAFETY: a zeroed siginfo_t is valid storage for waitid to initialize.
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        // SAFETY: info is valid writable storage, the id is the live child group leader pid, and
        // the flags only observe an exited child without reaping it.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: waitid initialized info on success; si_pid == 0 is WNOHANG's no-event marker.
        if unsafe { info.si_pid() } == 0 {
            return Ok(None);
        }
        self.kill_descendants();
        let status = self.child.wait()?;
        self.reaped = true;
        Ok(Some(status))
    }

    #[cfg(not(unix))]
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            self.reaped = true;
        }
        Ok(status)
    }

    #[cfg(unix)]
    fn kill_descendants(&self) {
        // SAFETY: spawn configured the child's pid as a new process-group id. ESRCH is the normal
        // no-descendant case after a clean exit; every error is best-effort cleanup only.
        unsafe {
            libc::killpg(self.child.id() as libc::pid_t, libc::SIGKILL);
        }
    }

    #[cfg(not(unix))]
    fn kill_descendants(&self) {}

    fn kill_and_reap(&mut self) -> std::io::Result<ExitStatus> {
        if !self.reaped {
            self.kill_descendants();
            let _ = self.child.kill();
            let status = self.child.wait()?;
            self.reaped = true;
            Ok(status)
        } else {
            self.child.wait()
        }
    }

    fn join_readers(&mut self) -> anyhow::Result<(BoundedCapture, BoundedCapture)> {
        fn join(
            kind: &str,
            handle: Option<std::thread::JoinHandle<std::io::Result<BoundedCapture>>>,
        ) -> anyhow::Result<BoundedCapture> {
            handle
                .context(format!("git_{kind}_reader_missing"))?
                .join()
                .map_err(|_| anyhow::anyhow!("git_{kind}_reader_panicked"))?
                .map_err(|_| anyhow::anyhow!("git_{kind}_capture_failed"))
        }
        let stdout = join("stdout", self.stdout_reader.take())?;
        let stderr = join("stderr", self.stderr_reader.take())?;
        Ok((stdout, stderr))
    }
}

impl Drop for RunningGit {
    fn drop(&mut self) {
        let _ = self.kill_and_reap();
        let _ = self.join_readers();
    }
}

struct GitExecution {
    status: ExitStatus,
    stdout: BoundedCapture,
    stderr: BoundedCapture,
    killed_for_limit: bool,
    #[cfg(test)]
    active_readers_after_join: usize,
}

#[cfg(test)]
struct TestReaderGuard(Arc<std::sync::atomic::AtomicUsize>);

#[cfg(test)]
impl TestReaderGuard {
    fn new(active: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        active.fetch_add(1, Ordering::AcqRel);
        Self(active)
    }
}

#[cfg(test)]
impl Drop for TestReaderGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn execute_bounded(
    repo: &Path,
    args: &[&str],
    timeout: Duration,
    stdout_max_bytes: usize,
) -> anyhow::Result<GitExecution> {
    let mut running = RunningGit::spawn(repo, args, stdout_max_bytes, STDERR_MAX_BYTES)?;
    let started = Instant::now();
    let (status, killed_for_limit) = loop {
        if running.output_limit_reached() {
            let status = running
                .kill_and_reap()
                .map_err(|_| anyhow::anyhow!("git_reap_failed"))?;
            break (status, true);
        }
        match running.try_wait() {
            Ok(Some(status)) => break (status, false),
            Ok(None) if started.elapsed() >= timeout => {
                let _ = running.kill_and_reap();
                let _ = running.join_readers();
                anyhow::bail!("git_timeout");
            }
            Ok(None) => std::thread::sleep(PROCESS_POLL_INTERVAL),
            Err(error) => {
                let _ = running.kill_and_reap();
                let _ = running.join_readers();
                let _ = error;
                anyhow::bail!("git_wait_failed");
            }
        }
    };
    let (stdout, stderr) = running.join_readers()?;
    let killed_for_limit = killed_for_limit || stdout.truncated || stderr.truncated;
    #[cfg(test)]
    let active_readers_after_join = running.active_readers.load(Ordering::Acquire);
    Ok(GitExecution {
        status,
        stdout,
        stderr,
        killed_for_limit,
        #[cfg(test)]
        active_readers_after_join,
    })
}

/// 테스트/fixture용 기본 상한 실행기. Production은 항상 명령별 상한을 직접 고른다.
#[cfg(test)]
pub fn run_git(repo: &Path, args: &[&str], timeout: Duration) -> anyhow::Result<String> {
    run_git_bounded(repo, args, timeout, DEFAULT_STDOUT_MAX_BYTES)
}

/// Exact-output Git command with a caller-selected retained-byte ceiling. Reaching `max_bytes + 1`
/// kills and reaps the command group and fails closed; exactly `max_bytes` is accepted.
pub fn run_git_bounded(
    repo: &Path,
    args: &[&str],
    timeout: Duration,
    max_bytes: usize,
) -> anyhow::Result<String> {
    let execution = execute_bounded(repo, args, timeout, max_bytes)?;
    if execution.stderr.truncated {
        anyhow::bail!("git_stderr_limit");
    }
    if execution.stdout.truncated {
        anyhow::bail!("git_stdout_limit");
    }
    if !execution.status.success() {
        anyhow::bail!("git_command_failed");
    }
    Ok(String::from_utf8_lossy(&execution.stdout.bytes).into_owned())
}

/// [`run_git_bounded`]의 stdout 잘림 허용판 — 리더가 `max_bytes`에 닿으면 부모가
/// 자식을 kill해 `(지금까지의 출력, 잘림 여부)`를 돌려준다. 전량 버퍼링 후 클립은
/// 거대 diff에서 수백 MB를 상주시키므로 diff 수집 계열은 이 함수를 쓴다 (codex 리뷰).
///
/// [`run_git_bounded`]와 달리 종료코드 1을 성공으로 본다 — diff 계열의 `--exit-code` 관례
/// (`--no-index`가 이를 함축: 1 = 차이 있음)이고, git의 실제 오류는 128/129로
/// 떨어진다. 상한 kill로 죽은 자식도 성공이다(필요한 출력은 이미 확보됨).
/// 출력이 정확히 상한 길이로 끝나면 온전한 결과이고, 한 바이트라도 넘을 때만 잘림이다.
pub fn run_git_limited(
    repo: &Path,
    args: &[&str],
    timeout: Duration,
    max_bytes: usize,
) -> anyhow::Result<(String, bool)> {
    let execution = execute_bounded(repo, args, timeout, max_bytes)?;
    if execution.stderr.truncated {
        anyhow::bail!("git_stderr_limit");
    }
    if !execution.status.success()
        && !execution.killed_for_limit
        && execution.status.code() != Some(1)
    {
        anyhow::bail!("git_command_failed");
    }
    Ok((
        String::from_utf8_lossy(&execution.stdout.bytes).into_owned(),
        execution.stdout.truncated,
    ))
}

/// 세션 cwd에서 git 레포 루트를 찾는다. git 레포가 아니면 Err.
pub fn repo_root(cwd: &Path, timeout: Duration) -> anyhow::Result<std::path::PathBuf> {
    const REPO_ROOT_MAX_BYTES: usize = 32 * 1024;
    let out = run_git_bounded(
        cwd,
        &["rev-parse", "--show-toplevel"],
        timeout,
        REPO_ROOT_MAX_BYTES,
    )?;
    let root = out.trim();
    anyhow::ensure!(!root.is_empty(), "git_repo_root_invalid");
    anyhow::ensure!(out.lines().count() == 1, "git_repo_root_invalid");
    Ok(std::path::PathBuf::from(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn temp_repo() -> std::path::PathBuf {
        // 전역 카운터 — 병렬 러너에서 nanos까지 같아도 경로가 겹치지 않는다
        // (worktree.rs temp_repo와 같은 패턴, codex P1).
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "deppy-gitcli-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        run_git(&dir, &["init", "-q"], Duration::from_secs(10)).unwrap();
        dir
    }

    #[test]
    fn run_git_성공은_stdout을_돌려준다() {
        let execution = execute_bounded(
            Path::new("."),
            &["version"],
            Duration::from_secs(10),
            DEFAULT_STDOUT_MAX_BYTES,
        )
        .unwrap();
        let out = String::from_utf8_lossy(&execution.stdout.bytes);
        assert!(execution.status.success());
        assert!(out.contains("git version"), "{out}");
        assert_eq!(execution.active_readers_after_join, 0);
    }

    #[test]
    fn run_git_실패는_stderr가_에러에_실린다() {
        let err = run_git(
            Path::new("."),
            &["definitely-not-a-subcommand"],
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "git_command_failed");
        assert!(!format!("{err:#}").contains("definitely-not-a-subcommand"));
        let execution = execute_bounded(
            Path::new("."),
            &["definitely-not-a-subcommand"],
            Duration::from_secs(10),
            DEFAULT_STDOUT_MAX_BYTES,
        )
        .unwrap();
        assert!(!execution.status.success());
        assert_eq!(execution.active_readers_after_join, 0);
    }

    #[test]
    fn bounded_capture는_exact를_허용하고_plus_one만_자른다() {
        let exact_flag = AtomicBool::new(false);
        let exact = read_bounded(Cursor::new(vec![b'x'; 64]), 64, &exact_flag, true).unwrap();
        assert_eq!(exact.bytes.len(), 64);
        assert!(!exact.truncated);
        assert!(!exact_flag.load(Ordering::Acquire));

        let plus_one_flag = AtomicBool::new(false);
        let plus_one = read_bounded(Cursor::new(vec![b'x'; 65]), 64, &plus_one_flag, true).unwrap();
        assert_eq!(plus_one.bytes.len(), 64);
        assert!(plus_one.truncated);
        assert!(plus_one_flag.load(Ordering::Acquire));
    }

    #[cfg(unix)]
    #[test]
    fn hostile_stderr는_상한에서_child_group과_reader를_회수한다() {
        let alias =
            "alias.hostile=!/usr/bin/yes hostile-stderr | /usr/bin/head -c 131072 >&2; exit 7";
        let started = Instant::now();
        let error = run_git_bounded(
            Path::new("."),
            &["-c", alias, "hostile"],
            Duration::from_secs(5),
            1024,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "git_stderr_limit");
        assert!(!format!("{error:#}").contains("hostile-stderr"));
        assert!(format!("{error:#}").len() <= 64);
        assert!(started.elapsed() < Duration::from_secs(5));
        let execution = execute_bounded(
            Path::new("."),
            &["-c", alias, "hostile"],
            Duration::from_secs(5),
            1024,
        )
        .unwrap();
        assert!(execution.stderr.truncated);
        assert_eq!(execution.active_readers_after_join, 0);
    }

    #[cfg(unix)]
    #[test]
    fn clean_parent_exit은_inherited_pipe_descendant를_reap전에_정리한다() {
        // 느린 공유 CI 러너 대응(2026-08-04, GHA run 30868793934): git 스폰+alias 해석+
        // 그룹 kill이 2s 예산을 넘겨 git_timeout으로 실패했다. 예산을 10s로 키우는 대신
        // descendant 수명도 5s→30s로 올려 회귀 식별력을 유지한다 — reader join이
        // descendant 파이프를 기다리는 회귀가 생기면 ~30s가 걸려 elapsed 상한에 걸린다.
        // 예산 10s는 이 파일의 다른 git 호출 테스트와 같은 관례(50ms급 작업에 초 단위
        // 상한)이고, descendant 수명 30s와는 3배 차이를 유지한다.
        let alias = "alias.background=!/bin/sleep 30 & exit 0";
        let started = Instant::now();
        let execution = execute_bounded(
            Path::new("."),
            &["-c", alias, "background"],
            Duration::from_secs(10),
            1024,
        )
        .unwrap();
        assert!(execution.status.success());
        assert!(execution.stdout.bytes.is_empty());
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(execution.active_readers_after_join, 0);
    }

    #[cfg(unix)]
    #[test]
    fn running_git_drop은_child_group과_reader를_회수한다() {
        let alias = "alias.slow=!/bin/sleep 5";
        let started = Instant::now();
        let running = RunningGit::spawn(
            Path::new("."),
            &["-c", alias, "slow"],
            1024,
            STDERR_MAX_BYTES,
        )
        .unwrap();
        let active_readers = Arc::clone(&running.active_readers);
        drop(running);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(active_readers.load(Ordering::Acquire), 0);
    }

    #[cfg(unix)]
    #[test]
    fn 반복_timeout은_reader를_남기지_않는다() {
        let alias = "alias.slow=!/bin/sleep 5";
        for _ in 0..8 {
            let error = run_git_bounded(
                Path::new("."),
                &["-c", alias, "slow"],
                Duration::from_millis(40),
                1024,
            )
            .unwrap_err();
            assert_eq!(error.to_string(), "git_timeout");
        }
    }

    #[test]
    fn repo_root은_하위_폴더에서도_루트를_찾는다() {
        let repo = temp_repo();
        let sub = repo.join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        let root = repo_root(&sub, Duration::from_secs(10)).unwrap();
        // macOS tmp는 /private 심링크라 canonicalize로 비교.
        assert_eq!(root.canonicalize().unwrap(), repo.canonicalize().unwrap());
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn run_git_limited는_상한_도달_시_잘라서_돌려준다() {
        // help -a 출력(ASCII)은 확실히 64바이트를 넘는다 — 상한에서 멈추고 잘림 표시.
        let (out, truncated) =
            run_git_limited(Path::new("."), &["help", "-a"], Duration::from_secs(10), 64).unwrap();
        assert!(truncated);
        assert!(out.len() <= 64, "{}", out.len());
    }

    #[test]
    fn run_git_limited는_상한_아래에선_전체를_돌려준다() {
        let (out, truncated) = run_git_limited(
            Path::new("."),
            &["version"],
            Duration::from_secs(10),
            64 * 1024,
        )
        .unwrap();
        assert!(!truncated);
        assert!(out.contains("git version"), "{out}");
    }

    #[test]
    fn run_git_limited는_no_index의_종료코드_1을_성공으로_본다() {
        // --no-index는 --exit-code를 함축 — 차이가 있으면 1로 끝나지만 실패가 아니다.
        let dir = std::env::temp_dir().join(format!(
            "deppy-gitcli-noindex-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("left.txt"), "left\n").unwrap();
        std::fs::write(dir.join("right.txt"), "right\n").unwrap();
        let (out, truncated) = run_git_limited(
            &dir,
            &["diff", "--no-index", "--", "left.txt", "right.txt"],
            Duration::from_secs(10),
            64 * 1024,
        )
        .unwrap();
        assert!(!truncated);
        assert!(out.contains("-left") && out.contains("+right"), "{out}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn production_source는_unbounded_capture와_result_channel을_쓰지_않는다() {
        let source = include_str!("git_cli.rs");
        let production = source
            .rsplit_once("#[cfg(test)]\nmod tests")
            .expect("tests marker")
            .0;
        for forbidden in [
            "read_to_end",
            "std::sync::mpsc::channel",
            "std::thread::spawn(",
        ] {
            assert!(
                !production.contains(forbidden),
                "production git executor contains {forbidden}"
            );
        }
        assert!(production.contains("#[cfg(test)]\nconst DEFAULT_STDOUT_MAX_BYTES"));
        assert!(production.contains("STDERR_MAX_BYTES"));
        assert!(production.contains("libc::WNOWAIT"));
        assert!(production.contains("kill_and_reap"));
        assert!(production.contains("join_readers"));
    }
}
