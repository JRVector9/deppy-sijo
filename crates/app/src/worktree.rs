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
    let slug = unique_slug(&stamp, |s| base.join(s).exists());
    let rel = format!(".deppy/worktrees/{slug}");
    let branch = format!("deppy/{slug}");
    crate::git_cli::run_git(
        &root,
        &["worktree", "add", "-b", &branch, &rel],
        ADD_TIMEOUT,
    )?;
    Ok(root.join(rel))
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

/// `<repo>/.git/info/exclude`에 `.deppy/`를 보장한다. 이미 있으면 무변경.
/// 기존 내용은 append로 보존한다 (읽기 실패·비UTF-8이어도 덮어쓰지 않는다).
fn ensure_exclude(root: &Path) -> anyhow::Result<()> {
    use std::io::Write;
    let info = root.join(".git/info");
    let path = info.join("exclude");
    let existing = std::fs::read(&path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    if exclude_has_entry(&existing) {
        return Ok(());
    }
    std::fs::create_dir_all(&info)?;
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
        let dir = std::env::temp_dir().join(format!(
            "deppy-worktree-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
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
}
