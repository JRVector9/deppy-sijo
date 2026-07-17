//! 워크트리 격리 셀 (PR-W) — 세션 레포에 격리 git worktree를 만들어 에이전트 여럿을
//! 같은 레포에서 병렬로 돌릴 때 파일 충돌을 없앤다.
//!
//! - 위치: `<repo>/.deppy/worktrees/<slug>`, 브랜치: `deppy/<slug>`.
//! - `.deppy/`는 `<repo>/.git/info/exclude`에 자동 추가(멱등) — 사이드바 파일트리가
//!   exclude를 읽으므로 트리가 오염되지 않는다.
//! - git 실행은 [`crate::git_cli`] 공용 헬퍼만 사용 — 블로킹이라 **백그라운드 스레드
//!   전용**(UI 스레드 호출 금지, 호출측 App이 mpsc로 결과를 수령).
//! - 1차 범위는 생성+스폰뿐이다. **워크트리 정리(`git worktree remove`)는 후속 PR** —
//!   여기서는 만들기만 하고 지우지 않는다.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// repo_root 조회 타임아웃 — rev-parse는 즉답 명령이다.
const ROOT_TIMEOUT: Duration = Duration::from_secs(10);
/// worktree add 타임아웃 — 체크아웃을 동반하므로 넉넉히.
const ADD_TIMEOUT: Duration = Duration::from_secs(30);

/// exclude에 추가하는 항목 — `.deppy/` 하위 전체(워크트리·후속 메타데이터).
const EXCLUDE_ENTRY: &str = ".deppy/";

/// 세션 cwd에서 격리 워크트리를 만들고 절대 경로를 돌려준다.
/// 블로킹(git 실행) — 반드시 백그라운드 스레드에서 호출한다. 에러(레포 아님·git 거부·
/// CLT 없음)는 그대로 올린다 — 표면화(notify)는 호출측 App 몫.
pub fn create_worktree(cwd: &Path) -> anyhow::Result<PathBuf> {
    let root = crate::git_cli::repo_root(cwd, ROOT_TIMEOUT)?;
    ensure_exclude(&root)?;
    let base = root.join(".deppy/worktrees");
    let stamp = slug_stamp(deppy_core::time::unix_secs());
    // 충돌 판정은 대상 디렉터리 + 브랜치 둘 다 — 브랜치 네임스페이스(deppy/*)는 레포
    // 전체 공용이라, 워크트리 안에서 다시 만들 때(root가 다름) 같은 초면 디렉터리는
    // 비어 있어도 `-b`가 기존 브랜치와 충돌한다. rev-parse --verify는 없으면 비0
    // 종료(run_git Err) → 미사용으로 판정.
    let slug = unique_slug(&stamp, |s| {
        base.join(s).exists()
            || crate::git_cli::run_git(
                &root,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/deppy/{s}"),
                ],
                ROOT_TIMEOUT,
            )
            .is_ok()
    });
    let rel = format!(".deppy/worktrees/{slug}");
    let branch = format!("deppy/{slug}");
    crate::git_cli::run_git(
        &root,
        &["worktree", "add", "-b", &branch, &rel],
        ADD_TIMEOUT,
    )?;
    Ok(root.join(rel))
}

/// 생성 완료 시 App 폴링부의 처리 결정 (codex P2 두 건의 순수 판정 — 유닛 테스트 대상).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnDecision {
    /// 지금 그 폴더에서 셸을 연다.
    Spawn,
    /// 응답 대기 중인 spawn이 있다 — pending_spawn_cd가 그 셸에 붙으므로 결과를
    /// 채널에 남겨두고 다음 프레임에 다시 판정한다(1칸 대기 큐).
    Defer,
    /// 요청 워크스페이스가 더 이상 활성이 아니다 — 스폰하지 않고 생성 사실만 알린다.
    NotifyOnly,
}

/// `same_workspace` = 요청 시점 워크스페이스가 여전히 활성, `spawn_busy` = 응답을
/// 못 받은 셸 spawn 존재. 워크스페이스 불일치는 종결 판정이라 슬롯 상태보다
/// 우선한다 — 다른 워크스페이스의 슬롯을 기다려 봐야 스폰하지 않기 때문.
pub fn spawn_decision(same_workspace: bool, spawn_busy: bool) -> SpawnDecision {
    if !same_workspace {
        SpawnDecision::NotifyOnly
    } else if spawn_busy {
        SpawnDecision::Defer
    } else {
        SpawnDecision::Spawn
    }
}

/// unix 초 → 슬러그 본체 `wt-yymmdd-HHMMSS`. 시각을 인자로 받아 유닛 테스트 가능.
/// 로컬 타임존은 std만으로 못 얻어 UTC를 쓴다 — 슬러그는 유일성이 목적이라 충분하다.
fn slug_stamp(unix_secs: u64) -> String {
    let secs = unix_secs % 86_400;
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    // days → (y, m, d): Howard Hinnant civil_from_days (공유 달력 알고리즘).
    let z = (unix_secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    format!("wt-{:02}{mo:02}{d:02}-{h:02}{m:02}{s:02}", y % 100)
}

/// 같은 초에 이미 슬러그가 있으면 `-2`, `-3`… 부번을 붙인다. 존재 판정을 클로저로
/// 받아 파일시스템 없이 유닛 테스트 가능.
fn unique_slug(stamp: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(stamp) {
        return stamp.to_owned();
    }
    (2u32..)
        .map(|n| format!("{stamp}-{n}"))
        .find(|cand| !taken(cand))
        .expect("부번은 언젠가 비어 있다")
}

/// exclude 내용에 `.deppy/` 항목이 이미 있는가 — 멱등 판정(유닛 테스트 대상).
fn exclude_has_entry(content: &str) -> bool {
    content.lines().any(|line| line.trim() == EXCLUDE_ENTRY)
}

/// 레포의 `info/exclude`에 `.deppy/`를 보장한다. 이미 있으면 무변경.
/// 기존 내용은 append로 보존한다 (읽기 실패·비UTF-8이어도 덮어쓰지 않는다).
fn ensure_exclude(root: &Path) -> anyhow::Result<()> {
    use std::io::Write;
    // linked worktree/submodule은 `<root>/.git`이 파일이라 경로를 조립하지 않고
    // git에게 묻는다 (codex P2). info/는 공용(common) 경로라 본 레포의 exclude로
    // 해석된다. 반환이 상대 경로면 root 기준이다 (`git -C root`로 실행하므로).
    let answered = crate::git_cli::run_git(
        root,
        &["rev-parse", "--git-path", "info/exclude"],
        ROOT_TIMEOUT,
    )?;
    let answered = PathBuf::from(answered.trim());
    let path = if answered.is_absolute() {
        answered
    } else {
        root.join(answered)
    };
    let existing = std::fs::read(&path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    if exclude_has_entry(&existing) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let newline = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    writeln!(file, "{newline}{EXCLUDE_ENTRY}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_stamp은_yymmdd_hhmmss_형식이다() {
        // epoch 0 = 1970-01-01 00:00:00 UTC.
        assert_eq!(slug_stamp(0), "wt-700101-000000");
        // 2026-07-17 12:34:56 UTC (date -u +%s로 산출한 상수).
        assert_eq!(slug_stamp(1_784_291_696), "wt-260717-123456");
    }

    #[test]
    fn unique_slug은_충돌_시_부번을_붙인다() {
        assert_eq!(unique_slug("wt-a", |_| false), "wt-a");
        assert_eq!(unique_slug("wt-a", |s| s == "wt-a"), "wt-a-2");
        assert_eq!(
            unique_slug("wt-a", |s| s == "wt-a" || s == "wt-a-2"),
            "wt-a-3"
        );
    }

    #[test]
    fn exclude_판정은_기존_항목을_인식한다() {
        assert!(!exclude_has_entry(""));
        assert!(!exclude_has_entry("target/\n.deppy\n")); // 슬래시 없는 유사 항목은 다르다
        assert!(exclude_has_entry(".deppy/\n"));
        assert!(exclude_has_entry("target/\n  .deppy/  \n")); // 공백 허용(trim)
    }

    fn temp_repo() -> PathBuf {
        // git_cli 테스트의 temp_repo 패턴 + worktree add에 필요한 커밋 하나.
        // 전역 카운터 — 병렬 러너에서 nanos까지 같아도 경로가 겹치지 않는다(codex P1).
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "deppy-worktree-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            crate::git_cli::run_git(&dir, args, Duration::from_secs(10)).unwrap();
        };
        run(&["init", "-q"]);
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "init",
        ]);
        dir
    }

    #[test]
    fn ensure_exclude는_멱등이다() {
        let repo = temp_repo();
        ensure_exclude(&repo).unwrap();
        ensure_exclude(&repo).unwrap();
        let content = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        let count = content
            .lines()
            .filter(|line| line.trim() == EXCLUDE_ENTRY)
            .count();
        assert_eq!(count, 1, "{content:?}");
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn create_worktree는_격리_브랜치의_워크트리를_만든다() {
        let repo = temp_repo();
        let sub = repo.join("src");
        std::fs::create_dir_all(&sub).unwrap();
        // 하위 폴더(세션 cwd 상당)에서 호출해도 레포 루트 기준으로 만든다.
        let path = create_worktree(&sub).unwrap();
        assert!(path.is_dir(), "{}", path.display());
        let slug = path.file_name().unwrap().to_str().unwrap().to_owned();
        assert!(slug.starts_with("wt-"), "{slug}");
        assert!(
            path.parent().unwrap().ends_with(".deppy/worktrees"),
            "{}",
            path.display()
        );
        // 워크트리 HEAD는 자동 생성된 deppy/<slug> 브랜치다.
        let head =
            crate::git_cli::run_git(&path, &["rev-parse", "--abbrev-ref", "HEAD"], ADD_TIMEOUT)
                .unwrap();
        assert_eq!(head.trim(), format!("deppy/{slug}"));
        // exclude에 .deppy/가 들어가 파일트리를 오염시키지 않는다.
        let exclude = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert!(exclude_has_entry(&exclude), "{exclude:?}");
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn temp_repo는_연속_호출에도_경로가_다르다() {
        // 카운터 없이는 병렬 러너에서 nanos 충돌 시 같은 경로를 잡았다(codex P1).
        let a = temp_repo();
        let b = temp_repo();
        assert_ne!(a, b);
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn 워크트리_안에서도_다시_워크트리를_만들_수_있다() {
        // linked worktree는 `.git`이 파일 — exclude 경로 조립이 아니라 git 해석이
        // 필요하다(codex P2). 같은 초 재생성의 브랜치 충돌은 부번으로 피한다.
        let repo = temp_repo();
        let first = create_worktree(&repo).unwrap();
        let second = create_worktree(&first).unwrap();
        assert!(second.is_dir(), "{}", second.display());
        assert!(
            second.parent().unwrap().ends_with(".deppy/worktrees"),
            "{}",
            second.display()
        );
        let head = |wt: &Path| {
            crate::git_cli::run_git(wt, &["rev-parse", "--abbrev-ref", "HEAD"], ADD_TIMEOUT)
                .unwrap()
                .trim()
                .to_owned()
        };
        let (h1, h2) = (head(&first), head(&second));
        assert!(h2.starts_with("deppy/wt-"), "{h2}");
        assert_ne!(h1, h2, "같은 초여도 브랜치 부번으로 갈라져야 한다");
        // 두 번째 호출의 exclude는 본 레포 공용 exclude로 해석돼 멱등이다(줄 1개).
        let exclude = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        let count = exclude
            .lines()
            .filter(|line| line.trim() == EXCLUDE_ENTRY)
            .count();
        assert_eq!(count, 1, "{exclude:?}");
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn spawn_decision은_워크스페이스_불일치를_우선한다() {
        // 워크스페이스가 바뀌었으면 슬롯 상태와 무관하게 스폰하지 않는다(codex P2).
        assert_eq!(spawn_decision(false, false), SpawnDecision::NotifyOnly);
        assert_eq!(spawn_decision(false, true), SpawnDecision::NotifyOnly);
    }

    #[test]
    fn spawn_decision은_슬롯_점유_시_지연한다() {
        // 응답 대기 spawn이 있으면 pending_spawn_cd 경합 — 다음 프레임으로(codex P2).
        assert_eq!(spawn_decision(true, true), SpawnDecision::Defer);
        assert_eq!(spawn_decision(true, false), SpawnDecision::Spawn);
    }
}
