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
use std::time::{Duration, Instant};

use anyhow::Context;

/// macOS CLT의 git 셔틀 — PATH 비의존.
pub const GIT_BIN: &str = "/usr/bin/git";

/// `git -C <repo> <args…>`를 실행해 stdout(UTF-8 lossy)을 돌려준다.
/// 비정상 종료는 stderr를 담은 에러, 타임아웃은 kill 후 에러.
pub fn run_git(repo: &Path, args: &[&str], timeout: Duration) -> anyhow::Result<String> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(GIT_BIN)
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("{GIT_BIN} 실행 실패 (Command Line Tools 미설치?)"))?;

    // 파이프를 즉시 비우는 리더 — 자식이 파이프 가득참으로 블록되지 않게 한다.
    fn drain<R: Read + Send + 'static>(src: Option<R>) -> std::sync::mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        if let Some(mut r) = src {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = r.read_to_end(&mut buf);
                let _ = tx.send(buf);
            });
        }
        rx
    }
    let out_rx = drain(child.stdout.take());
    let err_rx = drain(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!(
                    "git {}가 {:?} 안에 끝나지 않아 중단했습니다",
                    args.first().unwrap_or(&""),
                    timeout
                );
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let stdout = out_rx.recv().unwrap_or_default();
    let stderr = err_rx.recv().unwrap_or_default();
    if !status.success() {
        anyhow::bail!(
            "git {} 실패 ({status}): {}",
            args.join(" "),
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

/// [`run_git`]의 stdout 상한판 — 리더가 `max_bytes`에 닿으면 읽기를 멈추고 부모가
/// 자식을 kill해 `(지금까지의 출력, 잘림 여부)`를 돌려준다. 전량 버퍼링 후 클립은
/// 거대 diff에서 수백 MB를 상주시키므로 diff 수집 계열은 이 함수를 쓴다 (codex 리뷰).
///
/// [`run_git`]과 달리 종료코드 1을 성공으로 본다 — diff 계열의 `--exit-code` 관례
/// (`--no-index`가 이를 함축: 1 = 차이 있음)이고, git의 실제 오류는 128/129로
/// 떨어진다. 상한 kill로 죽은 자식도 성공이다(필요한 출력은 이미 확보됨).
/// 출력이 정확히 상한 길이로 끝나는 경계는 잘림으로 본다(오탐 1회가 무한 버퍼링보다
/// 낫다).
pub fn run_git_limited(
    repo: &Path,
    args: &[&str],
    timeout: Duration,
    max_bytes: usize,
) -> anyhow::Result<(String, bool)> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(GIT_BIN)
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("{GIT_BIN} 실행 실패 (Command Line Tools 미설치?)"))?;

    // stdout 리더 — 상한까지만 담고 즉시 (버퍼, 잘림)을 보고한다. 상한 도달 시 리더가
    // 파이프를 닫고(자식은 다음 write에서 EPIPE) 보고를 받은 부모 루프가 kill한다.
    let (out_tx, out_rx) = std::sync::mpsc::channel::<(Vec<u8>, bool)>();
    let stdout = child.stdout.take();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut truncated = false;
        if let Some(mut src) = stdout {
            let mut chunk = [0u8; 64 * 1024];
            loop {
                match src.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let take = n.min(max_bytes.saturating_sub(buf.len()));
                        buf.extend_from_slice(&chunk[..take]);
                        if buf.len() >= max_bytes {
                            truncated = true;
                            break;
                        }
                    }
                }
            }
        }
        let _ = out_tx.send((buf, truncated));
    });
    let (err_tx, err_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    if let Some(mut src) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = src.read_to_end(&mut buf);
            let _ = err_tx.send(buf);
        });
    }

    let deadline = Instant::now() + timeout;
    let mut collected: Option<(Vec<u8>, bool)> = None;
    let mut limit_killed = false;
    let status = loop {
        if collected.is_none()
            && let Ok(result) = out_rx.try_recv()
        {
            if result.1 {
                let _ = child.kill();
                limit_killed = true;
            }
            collected = Some(result);
        }
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!(
                    "git {}가 {:?} 안에 끝나지 않아 중단했습니다",
                    args.first().unwrap_or(&""),
                    timeout
                );
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let (stdout, truncated) = match collected {
        Some(result) => result,
        None => out_rx.recv().unwrap_or_default(),
    };
    let stderr = err_rx.recv().unwrap_or_default();
    if !status.success() && !limit_killed && status.code() != Some(1) {
        anyhow::bail!(
            "git {} 실패 ({status}): {}",
            args.join(" "),
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    Ok((String::from_utf8_lossy(&stdout).into_owned(), truncated))
}

/// 세션 cwd에서 git 레포 루트를 찾는다. git 레포가 아니면 Err.
pub fn repo_root(cwd: &Path, timeout: Duration) -> anyhow::Result<std::path::PathBuf> {
    let out = run_git(cwd, &["rev-parse", "--show-toplevel"], timeout)?;
    let root = out.trim();
    anyhow::ensure!(!root.is_empty(), "git 레포가 아님: {}", cwd.display());
    Ok(std::path::PathBuf::from(root))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let out = run_git(Path::new("."), &["version"], Duration::from_secs(10)).unwrap();
        assert!(out.contains("git version"), "{out}");
    }

    #[test]
    fn run_git_실패는_stderr가_에러에_실린다() {
        let err = run_git(
            Path::new("."),
            &["definitely-not-a-subcommand"],
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert!(err.to_string().contains("git"), "{err:#}");
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
}
