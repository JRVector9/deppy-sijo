//! 워크트리 격리 셀 (PR-W) — 세션 레포에 격리 git worktree를 만들어 에이전트 여럿을
//! 같은 레포에서 병렬로 돌릴 때 파일 충돌을 없앤다.
//!
//! - 위치: `<repo>/.deppy/worktrees/<slug>`, 브랜치: `deppy/<slug>`.
//! - `.deppy/`는 `<repo>/.git/info/exclude`에 자동 추가(멱등) — 사이드바 파일트리가
//!   exclude를 읽으므로 트리가 오염되지 않는다.
//! - git 실행은 [`crate::git_cli`] 공용 헬퍼만 사용 — 블로킹이라 **백그라운드 스레드
//!   전용**(UI 스레드 호출 금지, 호출측 App이 mpsc로 결과를 수령).
//! - [`remove_worktree`]는 작업 디렉터리를 지우고, `deppy/<slug>` 브랜치는 **고유
//!   커밋이 없을 때만**(tip이 다른 로컬/원격 브랜치에서 도달 가능) 함께 지운다 —
//!   미병합 커밋이 있으면 보존하고 [`BranchCleanup`]으로 알린다(2026-07-18 논의
//!   "무조건 보존"의 후속: 도달 가능성이 확인된 삭제는 손실이 아니다).

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

/// 삭제 성공 시 `deppy/<slug>` 브랜치 처리 결과 — App 알림 문구 분기용.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchCleanup {
    /// 고유 커밋이 없어 브랜치도 지웠다 — tip이 자기 자신 외 다른 ref(로컬/원격
    /// 브랜치)에서 도달 가능함을 확인한 뒤라 커밋 손실이 없다.
    Deleted,
    /// tip이 자기 자신에서만 도달 가능(미병합 커밋)이라 브랜치를 보존했다 —
    /// App이 알림에 "브랜치 보존됨"을 표시한다.
    PreservedUnmerged,
    /// 손대지 않음 — HEAD가 `deppy/*` 브랜치가 아니거나(사용자가 체크아웃을 바꿈,
    /// detached) 판정/삭제 git 명령이 실패. 모두 보존 쪽 안전 실패라, 워크트리는
    /// 이미 지워진 뒤이므로 에러로 올리지 않고 로그만 남긴다.
    Kept,
}

/// 세션 cwd(워크트리 내부 아무 경로)로부터 그 워크트리를 제거하고 (지운 워크트리
/// 루트, 브랜치 처리 결과)를 돌려준다(루트는 호출측이 같은 폴더를 쓰던 다른 pane을
/// 찾을 때 씀). 블로킹(git 실행) — 반드시 백그라운드 스레드에서 호출한다. deppy가
/// 만든 `.deppy/worktrees/` 하위가 아니면 거부한다(다른 워크트리를 잘못 지우는 사고
/// 방지). 삭제되지 않은 커밋/변경이 있으면 거부 에러를 그대로 올린다 — 조용한
/// 데이터 손실 금지.
///
/// 서브모듈: 상주(populated) 서브모듈이 있으면 git이 깨끗해도 `worktree remove`를
/// 거부한다("working trees containing submodules cannot be moved or removed",
/// git 2.50 실측 — `check_clean_worktree`가 `validate_no_submodules`를 부른다).
/// 그래서 이 경우에만 `--force`를 쓰는데, `--force`는 git 자체의 추적 변경 dirty
/// 검사까지 통째로 끈다 — 따라서 앱이 루트는 **모든** status 줄을, 각 서브모듈은
/// 재귀적으로 `--ignored` 포함 전체를 검사해 전부 깨끗할 때만 진행한다(서브모듈
/// 안의 ignored 파일은 바깥 status에 전혀 안 보인다 — 실측; ccac979의 "무시된 파일
/// 조용한 삭제 금지"와 같은 시나리오다). 서브모듈 git 저장소는 워크트리 메타데이터
/// (`.git/worktrees/<id>/modules/`) 안에 있어 워크트리와 함께 지워지므로, push 안 된
/// HEAD 커밋이 있으면(원격 ref 어디에도 없음) 그것도 거부 사유다.
///
/// 알려진 한계(codex 리뷰, 모두 안전 실패 쪽이라 범위 밖으로 남김 — 데이터 손실이
/// 아니라 "삭제가 거부되거나 정리가 한 프레임 늦는" 쪽):
/// - cwd가 워크트리 안의 중첩 서브모듈/레포 안이면 `repo_root`가 그 안쪽 레포를
///   반환해 `is_deppy_worktree`가 거부한다 — 삭제가 안 될 뿐 잘못 지우지는 않는다.
/// - 상주 서브모듈의 HEAD 외 로컬 브랜치에만 있는 커밋은 push 검사에 안 걸린다 —
///   서브모듈 안에서 브랜치 작업까지 한 극단 사례라 HEAD 검사만 둔다(그 커밋도
///   워크트리 메타데이터와 함께 지워지는 건 동일 — 필요해지면 전 브랜치 검사로 확장).
/// - 미상주(빈 폴더) 서브모듈이어도 과거 상주 이력이 있으면(`.git/worktrees/<id>/
///   modules/` 잔존) git이 force 없는 삭제를 거부한다 — 앱은 상주만 세므로 force를
///   안 쓰고, git의 거부가 그대로 표면화된다(거부일 뿐 손실 아님).
/// - 전처리 스캔과 `worktree remove` 실행 사이에 그 폴더의 셸/에이전트가 새
///   무시된 파일을 쓰면 그 파일은 걸러지지 않는다(TOCTOU) — 창이 git 프로세스
///   두 번 호출 사이로 매우 좁고, 막으려면 그 폴더의 모든 프로세스를 먼저 멈춰야
///   해 사용자가 요청한 "삭제 메뉴" 범위를 넘는다.
/// - `session_cwd_lookup`은 캐시(수 초 지연 가능)라, 방금 다른 폴더로 cd한 세션의
///   메뉴가 아주 짧게 이전 워크트리를 대상으로 남을 수 있다 — 이 앱의 cwd 의존
///   메뉴 전부(diff 보기·같은 폴더 새 셸 등)가 공유하는 기존 신뢰 모델이라 이
///   기능만 별도로 고치지 않는다.
pub fn remove_worktree(cwd: &Path) -> anyhow::Result<(PathBuf, BranchCleanup)> {
    let worktree_root = crate::git_cli::repo_root(cwd, ROOT_TIMEOUT)?;
    anyhow::ensure!(
        is_deppy_worktree(&worktree_root),
        "deppy 워크트리가 아님: {}",
        worktree_root.display()
    );
    // 브랜치 정리 판정용 — 삭제 후에는 이 워크트리에서 물을 수 없으니 먼저 캡처.
    // detached면 "HEAD"가 나온다(→ Kept).
    let head_branch = crate::git_cli::run_git(
        &worktree_root,
        &["rev-parse", "--abbrev-ref", "HEAD"],
        ROOT_TIMEOUT,
    )?
    .trim()
    .to_owned();
    let submodules = populated_submodules(&worktree_root)?;
    // git worktree remove의 dirty 판정은 추적/미추적 변경만 본다 — gitignore된
    // 내용(.env.local, 빌드 산출물, 심지어 중첩 워크트리)은 "깨끗함"으로 보고
    // 그대로 rm -rf에 딸려 지워진다(codex P1 실증). `--untracked-files=`을 명시
    // 해야 한다 — `status.showUntrackedFiles=no` 설정이 있으면 플래그 없이는
    // `??`/`!!` 둘 다 안 뜬다(codex 재검증, 같은 설정이 worktree remove 자체의
    // dirty 판정에도 적용돼 무방비로 지운다). `normal`(git 기본값)을 쓴다 — `all`은
    // 거대 미추적 디렉터리 전체를 한 줄씩 나열해 거부 판정 하나에 출력을 통째로
    // 버퍼링시킨다(codex 재검증 2); `normal`도 디렉터리를 한 줄로 묶을 뿐 존재
    // 여부 판정(및 config 우회 방지)은 동일하게 한다.
    const STATUS_ARGS: [&str; 4] = [
        "status",
        "--porcelain",
        "--ignored",
        "--untracked-files=normal",
    ];
    let status = crate::git_cli::run_git(&worktree_root, &STATUS_ARGS, ROOT_TIMEOUT)?;
    if submodules.is_empty() {
        // 추적 변경(` M` 등)은 git 자신이 --force 없는 remove에서 거부하므로 앱은
        // git이 못 보는 미추적/무시 파일만 판정한다.
        if let Some(first) = status
            .lines()
            .find(|l| l.starts_with("!! ") || l.starts_with("?? "))
        {
            anyhow::bail!(
                "정리 안 된 파일이 있어 삭제를 거부합니다({}…) — 직접 정리 후 다시 시도하세요",
                &first[3..]
            );
        }
    } else {
        // 서브모듈 모드 — 아래에서 --force를 쓰면 git 자체 dirty 검사가 통째로
        // 꺼지므로, 추적 변경 포함 모든 status 줄이 앱의 거부 사유로 승격된다.
        // (서브모듈 내부의 미추적/커밋 변경도 바깥에는 ` M <sub>` 한 줄로 보인다 —
        // 실측. 그래서 이 검사 하나가 서브모듈의 추적/미추적 변경까지 함께 막는다.)
        if let Some(first) = status.lines().find(|l| !l.trim().is_empty()) {
            anyhow::bail!(
                "정리 안 된 변경이 있어 삭제를 거부합니다({}…) — 서브모듈이 있는 워크트리는 완전히 깨끗해야 합니다",
                first.get(3..).unwrap_or(first)
            );
        }
        for sub in &submodules {
            let rel = sub
                .strip_prefix(&worktree_root)
                .unwrap_or(sub.as_path())
                .display();
            // 서브모듈 안의 ignored 파일은 바깥 status에 전혀 안 보인다(실측) —
            // 재귀 스캔 없이는 --force가 그대로 지워버린다(루트의 codex P1과 동일).
            let sub_status = crate::git_cli::run_git(sub, &STATUS_ARGS, ROOT_TIMEOUT)?;
            if let Some(first) = sub_status.lines().find(|l| !l.trim().is_empty()) {
                anyhow::bail!(
                    "서브모듈에 정리 안 된 파일이 있어 삭제를 거부합니다({rel}: {}…) — 직접 정리 후 다시 시도하세요",
                    first.get(3..).unwrap_or(first)
                );
            }
            // 서브모듈 git 저장소는 `.git/worktrees/<id>/modules/` 안에 있어
            // 워크트리와 함께 지워진다 — HEAD 커밋이 어떤 원격 ref에도 없으면
            // 그 커밋 객체의 유일한 사본이 사라진다(바깥 status는 gitlink가
            // 커밋돼 있으면 깨끗하다). push로 사본이 생긴 뒤에만 지운다.
            let remote_refs = crate::git_cli::run_git(
                sub,
                &[
                    "branch",
                    "--remotes",
                    "--format=%(refname:short)",
                    "--contains",
                    "HEAD",
                ],
                ROOT_TIMEOUT,
            )?;
            if remote_refs.lines().all(|l| l.trim().is_empty()) {
                anyhow::bail!(
                    "서브모듈에 push 안 된 커밋이 있어 삭제를 거부합니다({rel}) — push 후 다시 시도하세요"
                );
            }
        }
    }
    // worktree remove는 지울 경로 안에서는 실행할 수 없다 — 메인 워크트리에서
    // 실행해야 한다. `--git-common-dir`의 부모로 추정하면 서브모듈/
    // `--separate-git-dir` 레포에서 틀린 경로가 나온다(codex P2) — 대신
    // `worktree list --porcelain`의 첫 항목이 항상 메인 워크트리라는 git 자체
    // 보장을 쓴다.
    let listing = crate::git_cli::run_git(
        &worktree_root,
        &["worktree", "list", "--porcelain"],
        ROOT_TIMEOUT,
    )?;
    let main_root = listing
        .lines()
        .find_map(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("워크트리 목록에서 메인 레포를 찾지 못함"))?;
    let target = worktree_root.to_str().ok_or_else(|| {
        anyhow::anyhow!("워크트리 경로가 UTF-8이 아님: {}", worktree_root.display())
    })?;
    // 상주 서브모듈이 있으면 git이 깨끗해도 거부한다(위 doc 참조) — 위에서 루트·
    // 서브모듈 전부를 앱이 검사한 뒤에만 --force로 그 판정을 대신한다. 없으면
    // 기존대로 force 미사용(추적 변경 거부를 git에 맡긴다).
    let remove_args: &[&str] = if submodules.is_empty() {
        &["worktree", "remove", target]
    } else {
        &["worktree", "remove", "--force", target]
    };
    crate::git_cli::run_git(&main_root, remove_args, ADD_TIMEOUT)?;
    Ok((worktree_root, cleanup_branch(&main_root, &head_branch)))
}

/// repo 안 상주(populated) 서브모듈의 절대 경로 목록 — 중첩 서브모듈까지 재귀.
/// gitlink(mode 160000) 항목 중 `<path>/.git`이 실존하는 것만 센다 — 미상주(빈
/// 폴더)는 git worktree remove가 force 없이도 지운다(git 2.50 실측).
fn populated_submodules(repo: &Path) -> anyhow::Result<Vec<PathBuf>> {
    // -z: 경로에 특수문자가 있어도 인용 없이 NUL 구분 — 파싱이 흔들리지 않는다.
    let listing = crate::git_cli::run_git(repo, &["ls-files", "-z", "--stage"], ROOT_TIMEOUT)?;
    let mut found = Vec::new();
    for entry in listing.split('\0') {
        // 항목 형식: "<mode> <sha> <stage>\t<path>".
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        if !meta.starts_with("160000 ") {
            continue;
        }
        let sub = repo.join(path);
        // `.git`은 파일(gitdir 포인터)일 수도 디렉터리일 수도 있다 — 존재 = 상주.
        if sub.join(".git").exists() {
            found.extend(populated_submodules(&sub)?);
            found.push(sub);
        }
    }
    Ok(found)
}

/// 워크트리 삭제 성공 후 `deppy/<slug>` 브랜치 정리 — 고유 커밋이 없을 때만 지운다.
/// 판정: `git branch --all --format=%(refname:short) --contains <branch>`가 자기
/// 자신 외의 ref를 나열하면 tip이 다른 브랜치(로컬/원격)에서 도달 가능 = 지워도
/// 커밋 손실 없음. `branch -d`를 안 쓰는 이유: -d는 HEAD/upstream 병합만 보므로
/// 다른 브랜치에 병합된 경우를 놓친다 — 도달 가능성 판정을 직접 한 뒤 -D를 쓴다.
/// 실패는 전부 보존 쪽(Kept)으로 — 워크트리는 이미 지워졌으니 에러로 안 올린다.
fn cleanup_branch(main_root: &Path, branch: &str) -> BranchCleanup {
    if !branch.starts_with("deppy/") {
        // detached("HEAD") 또는 사용자가 체크아웃을 바꾼 브랜치 — 앱 소유가 아니다.
        return BranchCleanup::Kept;
    }
    let contains = crate::git_cli::run_git(
        main_root,
        &[
            "branch",
            "--all",
            "--format=%(refname:short)",
            "--contains",
            branch,
        ],
        ROOT_TIMEOUT,
    );
    let contains = match contains {
        Ok(out) => out,
        Err(e) => {
            tracing::warn!("브랜치 병합 판정 실패({branch}) — 보존: {e:#}");
            return BranchCleanup::Kept;
        }
    };
    let reachable_elsewhere = contains
        .lines()
        .map(str::trim)
        .any(|l| !l.is_empty() && l != branch);
    if !reachable_elsewhere {
        return BranchCleanup::PreservedUnmerged;
    }
    match crate::git_cli::run_git(main_root, &["branch", "-D", branch], ROOT_TIMEOUT) {
        Ok(_) => BranchCleanup::Deleted,
        Err(e) => {
            tracing::warn!("브랜치 삭제 실패({branch}) — 보존: {e:#}");
            BranchCleanup::Kept
        }
    }
}

/// `<repo>/.deppy/worktrees/<slug>` 형태인가 — 이 앱이 만든 워크트리만 지우기 위한 판정.
fn is_deppy_worktree(worktree_root: &Path) -> bool {
    let mut comps = worktree_root.components().rev();
    comps.next().is_some() // slug — 형식은 검사하지 않는다, 부모 경로가 판정 근거.
        && matches!(comps.next(), Some(c) if c.as_os_str() == "worktrees")
        && matches!(comps.next(), Some(c) if c.as_os_str() == ".deppy")
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
    fn is_deppy_worktree는_deppy_worktrees_하위만_인정한다() {
        assert!(is_deppy_worktree(Path::new(
            "/repo/.deppy/worktrees/wt-260718-000000"
        )));
        assert!(!is_deppy_worktree(Path::new("/repo")));
        assert!(!is_deppy_worktree(Path::new("/repo/src")));
        assert!(!is_deppy_worktree(Path::new("/repo/worktrees/wt-x")));
    }

    #[test]
    fn remove_worktree는_깨끗한_워크트리를_지운다() {
        let repo = temp_repo();
        let path = create_worktree(&repo).unwrap();
        assert!(path.is_dir());
        let (root, branch) = remove_worktree(&path).unwrap();
        assert_eq!(root, path);
        assert!(!path.exists(), "{}", path.display());
        // 고유 커밋이 없는 브랜치(tip이 기본 브랜치에서 도달 가능)는 함께 지운다 —
        // 2026-07-18 "무조건 보존" 논의의 후속: 다른 ref에서 전부 도달 가능함을
        // 확인한 삭제는 손실이 아니다. 미병합 커밋이 있으면 아래 보존 테스트.
        assert_eq!(branch, BranchCleanup::Deleted);
        let branches = crate::git_cli::run_git(&repo, &["branch", "--list"], ADD_TIMEOUT).unwrap();
        assert!(!branches.contains("deppy/"), "{branches:?}");
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn remove_worktree는_미병합_커밋_브랜치를_보존한다() {
        let repo = temp_repo();
        let path = create_worktree(&repo).unwrap();
        // 워크트리 브랜치에만 있는 커밋 — tip이 자기 자신에서만 도달 가능해진다.
        crate::git_cli::run_git(
            &path,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "unique",
            ],
            ADD_TIMEOUT,
        )
        .unwrap();
        let (_, branch) = remove_worktree(&path).unwrap();
        assert!(!path.exists(), "{}", path.display());
        assert_eq!(branch, BranchCleanup::PreservedUnmerged);
        let branches = crate::git_cli::run_git(&repo, &["branch", "--list"], ADD_TIMEOUT).unwrap();
        assert!(branches.contains("deppy/"), "{branches:?}");
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn remove_worktree는_무시된_파일이_있으면_거부한다() {
        // git worktree remove의 dirty 판정은 gitignore된 내용을 안 본다 — .env.local
        // 같은 파일이 있어도 "깨끗함"으로 보고 rm -rf에 딸려 지운다(codex P1 실증).
        let repo = temp_repo();
        std::fs::write(repo.join(".gitignore"), "*.local\n").unwrap();
        crate::git_cli::run_git(
            &repo,
            &["add", ".gitignore"],
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        crate::git_cli::run_git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "gitignore",
            ],
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        let path = create_worktree(&repo).unwrap();
        std::fs::write(path.join("secret.local"), "x").unwrap();
        let err = remove_worktree(&path).unwrap_err();
        assert!(
            path.exists(),
            "무시된 파일이 있으면 지워지면 안 된다: {}",
            path.display()
        );
        assert!(format!("{err:#}").contains("정리 안 된 파일"));
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn remove_worktree는_dirty하면_거부한다() {
        // 미추적 파일도 전처리 스캔이 먼저 잡는다(git 자체의 dirty 거부까지
        // 가지 않음 — status.showUntrackedFiles=no 우회를 막기 위해 앱이 먼저
        // --untracked-files=all로 본다, codex P1).
        let repo = temp_repo();
        let path = create_worktree(&repo).unwrap();
        std::fs::write(path.join("dirty.txt"), "x").unwrap();
        let err = remove_worktree(&path).unwrap_err();
        assert!(
            path.exists(),
            "dirty면 지워지면 안 된다: {}",
            path.display()
        );
        assert!(format!("{err:#}").contains("정리 안 된 파일"));
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn remove_worktree는_showuntrackedfiles_no_설정을_우회하지_않는다() {
        // codex 재검증: status.showUntrackedFiles=no면 --ignored만으로는 무시된
        // 파일도 안 보인다 — --untracked-files=all을 명시해야 우회가 안 된다.
        let repo = temp_repo();
        crate::git_cli::run_git(
            &repo,
            &["config", "status.showUntrackedFiles", "no"],
            ADD_TIMEOUT,
        )
        .unwrap();
        let path = create_worktree(&repo).unwrap();
        std::fs::write(path.join("dirty.txt"), "x").unwrap();
        let err = remove_worktree(&path).unwrap_err();
        assert!(path.exists(), "{}", path.display());
        assert!(format!("{err:#}").contains("정리 안 된 파일"));
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn remove_worktree는_deppy_워크트리가_아니면_거부한다() {
        let repo = temp_repo();
        let err = remove_worktree(&repo).unwrap_err();
        assert!(format!("{err:#}").contains("deppy 워크트리가 아님"));
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn remove_worktree는_separate_git_dir_레포에서도_동작한다() {
        // codex 재검증: `--git-common-dir`의 부모를 메인 루트로 가정하면
        // --separate-git-dir/서브모듈 레포에서 틀린 경로가 나와 삭제가 깨진다(P2) —
        // `worktree list --porcelain`의 첫 항목(항상 메인)을 쓰는 게 이 테스트가
        // 지키는 고정 계약.
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let base = std::env::temp_dir().join(format!(
            "deppy-worktree-sep-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        let repo = base.join("repo");
        let gitdir = base.join("meta.git");
        std::fs::create_dir_all(&repo).unwrap();
        crate::git_cli::run_git(
            &repo,
            &["init", "-q", "--separate-git-dir", gitdir.to_str().unwrap()],
            ADD_TIMEOUT,
        )
        .unwrap();
        crate::git_cli::run_git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "init",
            ],
            ADD_TIMEOUT,
        )
        .unwrap();
        let path = create_worktree(&repo).unwrap();
        assert!(path.is_dir());
        remove_worktree(&path).unwrap();
        assert!(!path.exists(), "{}", path.display());
        std::fs::remove_dir_all(&base).ok();
    }

    /// temp_repo + 커밋된 로컬 경로 서브모듈(sub) — 서브모듈 테스트 공용.
    /// 반환 (base, 메인 repo 경로) — base 하나만 지우면 서브레포까지 정리된다.
    /// 서브레포에는 ignored 파일 테스트용 `.gitignore`(*.local)를 커밋해 둔다.
    fn temp_repo_with_submodule() -> (PathBuf, PathBuf) {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let base = std::env::temp_dir().join(format!(
            "deppy-worktree-sub-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        let subrepo = base.join("subrepo");
        let repo = base.join("repo");
        std::fs::create_dir_all(&subrepo).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        let run = |dir: &Path, args: &[&str]| {
            crate::git_cli::run_git(dir, args, ADD_TIMEOUT).unwrap();
        };
        run(&subrepo, &["init", "-q"]);
        std::fs::write(subrepo.join(".gitignore"), "*.local\n").unwrap();
        run(&subrepo, &["add", ".gitignore"]);
        run(
            &subrepo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );
        run(&repo, &["init", "-q"]);
        run(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "init",
            ],
        );
        // 로컬 경로 서브모듈은 file 프로토콜 — git 기본 차단이라 allow=always 필요.
        run(
            &repo,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                subrepo.to_str().unwrap(),
                "sub",
            ],
        );
        run(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "sub",
            ],
        );
        (base, repo)
    }

    /// 워크트리 생성 + 서브모듈 상주(populate) — `worktree add`는 서브모듈 폴더를
    /// 비워 두므로(미상주, 실측) 명시적으로 update --init 해야 상주 케이스가 된다.
    fn worktree_with_populated_submodule(repo: &Path) -> PathBuf {
        let wt = create_worktree(repo).unwrap();
        crate::git_cli::run_git(
            &wt,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "-q",
            ],
            ADD_TIMEOUT,
        )
        .unwrap();
        assert!(wt.join("sub/.git").exists(), "서브모듈 populate 실패");
        wt
    }

    #[test]
    fn remove_worktree는_깨끗한_서브모듈_워크트리를_지운다() {
        // git은 상주 서브모듈이 있으면 깨끗해도 force 없는 remove를 거부한다
        // ("working trees containing submodules cannot be moved or removed",
        // 2.50 실측) — 앱이 루트·서브모듈 전부 검사한 뒤 --force로 지우는 경로.
        let (base, repo) = temp_repo_with_submodule();
        let wt = worktree_with_populated_submodule(&repo);
        let (root, _) = remove_worktree(&wt).unwrap();
        assert_eq!(root, wt);
        assert!(!wt.exists(), "{}", wt.display());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn remove_worktree는_서브모듈이_dirty면_거부한다() {
        let (base, repo) = temp_repo_with_submodule();
        let wt = worktree_with_populated_submodule(&repo);
        // ① 서브모듈 안 미추적 파일 — 바깥 status에 " M sub" 한 줄로 떠(실측)
        //    루트 전체-깨끗 검사가 잡는다.
        std::fs::write(wt.join("sub/dirty.txt"), "x").unwrap();
        let err = remove_worktree(&wt).unwrap_err();
        assert!(wt.exists(), "dirty 서브모듈이면 지워지면 안 된다");
        assert!(format!("{err:#}").contains("거부"), "{err:#}");
        std::fs::remove_file(wt.join("sub/dirty.txt")).unwrap();
        // ② 서브모듈 안 ignored 파일 — 바깥 status에는 전혀 안 보인다(실측).
        //    재귀 스캔만 잡는다 — ccac979 P1(무시된 파일 조용한 삭제)의 서브모듈판.
        std::fs::write(wt.join("sub/secret.local"), "x").unwrap();
        let err = remove_worktree(&wt).unwrap_err();
        assert!(wt.exists(), "ignored 파일이 있으면 지워지면 안 된다");
        assert!(format!("{err:#}").contains("서브모듈"), "{err:#}");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn remove_worktree는_push_안_된_서브모듈_커밋이_있으면_거부한다() {
        // 서브모듈 git 저장소는 워크트리 메타데이터(modules/) 안에 있어 워크트리와
        // 함께 지워진다 — gitlink를 커밋해 바깥이 깨끗해도, 서브모듈 HEAD 커밋이
        // 원격에 없으면 유일한 객체 사본이 사라지므로 거부해야 한다.
        let (base, repo) = temp_repo_with_submodule();
        let wt = worktree_with_populated_submodule(&repo);
        let run = |dir: &Path, args: &[&str]| {
            crate::git_cli::run_git(dir, args, ADD_TIMEOUT).unwrap();
        };
        run(
            &wt.join("sub"),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "local",
            ],
        );
        run(&wt, &["add", "sub"]);
        run(
            &wt,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "bump",
            ],
        );
        let err = remove_worktree(&wt).unwrap_err();
        assert!(wt.exists(), "push 안 된 서브모듈 커밋이면 지워지면 안 된다");
        assert!(format!("{err:#}").contains("push"), "{err:#}");
        std::fs::remove_dir_all(&base).ok();
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
