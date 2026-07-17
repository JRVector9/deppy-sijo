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
        let dir = std::env::temp_dir().join(format!(
            "deppy-gitcli-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
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
}
