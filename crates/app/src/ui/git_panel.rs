//! 사이드바 Git 탭 본문 — orca 스타일 읽기 전용 상태 패널 (2026-08-15 스펙:
//! docs/superpowers/specs/2026-08-15-git-panel-design.md).
//! leaf는 intent(GitPanelAction)만 반환하고 git 실행·뷰 전환은 App이 소유한다.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// 섹션당 수집 상한 — diff_panel의 MAX_DIFF_FILES와 같은 값(512). 표시 기본값은
/// SECTION_COLLAPSED_ROWS이고 「모두 보기」로 펼친다.
pub const MAX_PANEL_FILES: usize = 512;
/// 접힘 상태에서 섹션당 보여주는 행 수 — orca 스크린샷 기준 한 화면 분량.
pub const SECTION_COLLAPSED_ROWS: usize = 10;
/// 워크트리 목록 상한 — 목록은 사람이 훑는 것이라 32면 충분하고, 초과분은 잘림 표시만 한다.
pub const MAX_WORKTREE_ROWS: usize = 32;
/// 셸 스폰 경로 상한 — App의 `spawn_shell_at`이 같은 검사를 하지만 leaf도 넘기지 않는다.
pub const WORKTREE_PATH_MAX_BYTES: usize = 4096;
/// 브랜치 커밋 목록 상한 — 사람이 훑는 목록이라 64면 충분하고, 초과분은 잘림 표시만 한다.
pub const MAX_BRANCH_COMMITS: usize = 64;
/// [`CommitSha`] 최대 길이 — SHA-256 저장소의 전체 sha(64자)까지 담는다.
const COMMIT_SHA_MAX_LEN: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitFileRow {
    pub rel_path: String,
    /// porcelain XY 중 워킹트리(Y) 우선, Y가 공백이면 X. untracked(??)는 '?'.
    pub status: char,
    /// None = 바이너리 또는 untracked(numstat 없음).
    pub adds: Option<u32>,
    pub dels: Option<u32>,
}

/// `git worktree list --porcelain` 한 항목. 클릭하면 App이 이 경로에서 셸을 연다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitWorktreeRow {
    pub path: String,
    /// 표시용 마지막 경로 요소. 경로가 루트라 요소가 없으면 경로 전체를 쓴다.
    pub name: String,
    /// None = detached HEAD.
    pub branch: Option<String>,
    /// 지금 보고 있는 워크트리(= snapshot.repo_root)인가.
    pub current: bool,
}

/// 커밋 sha(16진 문자열). `DiffMode`가 `Copy`라 String이 아니라 고정 길이 배열에 담는다.
/// `parse`만 값을 만들므로 안의 바이트는 항상 ASCII 16진수다.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CommitSha {
    hex: [u8; COMMIT_SHA_MAX_LEN],
    len: u8,
}

impl CommitSha {
    /// 4~64자 16진수만 받는다(git의 최소 약어 길이 4). git 인자로 그대로 넘기므로
    /// 옵션(`-`)이나 경로로 해석될 값은 여기서 걸러진다.
    pub fn parse(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if !(4..=COMMIT_SHA_MAX_LEN).contains(&bytes.len())
            || !bytes.iter().all(u8::is_ascii_hexdigit)
        {
            return None;
        }
        let mut hex = [0; COMMIT_SHA_MAX_LEN];
        hex[..bytes.len()].copy_from_slice(bytes);
        Some(Self {
            hex,
            len: u8::try_from(bytes.len()).ok()?,
        })
    }

    pub fn as_str(&self) -> &str {
        // parse가 ASCII 16진수만 담으므로 실패하지 않는다.
        std::str::from_utf8(&self.hex[..usize::from(self.len)]).unwrap_or_default()
    }
}

impl std::fmt::Debug for CommitSha {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 브랜치 커밋 한 항목 — `git log`의 짧은 sha·제목·커밋 시각(2026-10-11 PR 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitCommitRow {
    pub sha: CommitSha,
    pub subject: String,
    /// 커밋(committer) 시각, unix 초.
    pub time: i64,
}

/// 커밋 하나가 바꾼 파일 — committed 섹션과 같은 행 모델, 같은 상한(MAX_PANEL_FILES).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitCommitFiles {
    pub rows: Vec<GitFileRow>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Default)]
pub struct GitPanelSnapshot {
    // 스펙 §2 데이터 모델의 일부 — 수집 시점의 `git_cli::repo_root` 결과를 그대로
    // 담아 둔다. 렌더는 상대 경로(rel_path)만 쓰고, 파일 diff 수집은 App이 넘긴
    // cwd로 repo_root를 다시 구해(collect_file_diff) 이 필드를 재사용하지 않는다.
    // 소비자가 없어도 스냅샷 모델의 일부로 유지한다 — 2026-08-15.
    #[allow(dead_code)]
    pub repo_root: PathBuf,
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub changes: Vec<GitFileRow>,
    pub committed: Vec<GitFileRow>,
    pub changes_truncated: bool,
    pub committed_truncated: bool,
    /// `origin` remote가 GitHub면 `https://github.com/OWNER/REPO`로 정규화한 값.
    /// GitHub가 아니거나 remote 조회 실패 시 None(스펙 §4 — ↗ 아이콘 숨김 조건).
    /// (Task 10 Step 7 소급 요구 — collect_snapshot에서 remote 조회 실패해도
    /// 스냅샷 전체를 죽이지 않고 None으로만 담는다.)
    pub remote_https_base: Option<String>,
    pub worktrees: Vec<GitWorktreeRow>,
    pub worktrees_truncated: bool,
    /// `committed`와 같은 기준(merge-base..HEAD)의 커밋, 최신순(2026-10-11 PR 4).
    /// upstream이 없거나 조회가 실패하면 비어 있다.
    pub commits: Vec<GitCommitRow>,
    pub commits_truncated: bool,
}

/// `status --porcelain -z -uall` + `diff --numstat HEAD`를 경로로 병합한다.
/// porcelain 등장 순서를 유지한다(사용자가 보는 안정된 순서). 반환하는 bool은
/// 상한(MAX_PANEL_FILES) 때문에 항목을 실제로 버렸는지다 — 딱 상한만큼만 있고
/// 더는 없으면 false(diff_panel::build_file_rows와 같은 패턴, 중요 리뷰 지적 3번).
fn merge_status_rows(porcelain_z: &str, numstat: &str) -> (Vec<GitFileRow>, bool) {
    let counts = parse_numstat(numstat);
    let mut rows = Vec::new();
    let mut truncated = false;
    let mut fields = porcelain_z.split('\0').filter(|s| !s.is_empty());
    while let Some(entry) = fields.next() {
        if entry.len() < 4 {
            continue; // "XY " 접두 미달 — 손상 항목은 건너뛴다(패널 전체를 죽이지 않음).
        }
        let (xy, path) = entry.split_at(3);
        let mut chars = xy.chars();
        let x = chars.next().unwrap_or(' ');
        let y = chars.next().unwrap_or(' ');
        // rename/copy는 다음 NUL 필드가 원경로다 — 소비만 하고 표시는 새 경로.
        if x == 'R' || x == 'C' {
            let _ = fields.next();
        }
        let status = if x == '?' {
            '?'
        } else if y != ' ' {
            y // 워킹트리 우선 (스펙 §3)
        } else {
            x
        };
        // 새 항목을 담기 *전에* 상한을 검사한다 — 이미 상한만큼 담은 뒤에 검사하면
        // 정확히 상한일 때와 넘칠 때를 구분할 수 없다.
        if rows.len() >= MAX_PANEL_FILES {
            truncated = true;
            break;
        }
        let (adds, dels) = counts.get(path).copied().unwrap_or((None, None));
        rows.push(GitFileRow {
            rel_path: path.to_owned(),
            status,
            adds,
            dels,
        });
    }
    (rows, truncated)
}

/// `diff --numstat` 한 줄 = "adds\tdels\tpath" (바이너리는 "-\t-").
fn parse_numstat(numstat: &str) -> std::collections::HashMap<String, (Option<u32>, Option<u32>)> {
    let mut out = std::collections::HashMap::new();
    for line in numstat.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(a), Some(d), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        out.insert(path.to_owned(), (a.parse().ok(), d.parse().ok()));
    }
    out
}

/// committed 섹션: `diff --numstat base..HEAD` + `diff --name-status base..HEAD` 병합.
/// name-status 등장 순서를 유지한다. 반환하는 bool의 의미는 merge_status_rows와
/// 같다 — 정확히 상한만큼이면 false, 하나라도 더 버렸으면 true.
fn merge_committed_rows(numstat: &str, name_status: &str) -> (Vec<GitFileRow>, bool) {
    let counts = parse_numstat(numstat);
    let mut rows = Vec::new();
    let mut truncated = false;
    for line in name_status.lines() {
        let mut parts = line.splitn(2, '\t');
        let (Some(status), Some(path)) = (parts.next(), parts.next()) else {
            continue;
        };
        // rename 라인("R100\told\tnew")은 마지막 필드가 새 경로다.
        let path = path.rsplit('\t').next().unwrap_or(path);
        let status = status.chars().next().unwrap_or('M');
        if rows.len() >= MAX_PANEL_FILES {
            truncated = true;
            break;
        }
        let (adds, dels) = counts.get(path).copied().unwrap_or((None, None));
        rows.push(GitFileRow {
            rel_path: path.to_owned(),
            status,
            adds,
            dels,
        });
    }
    (rows, truncated)
}

/// `git worktree list --porcelain -z` 파싱. `-z`는 git-worktree(1) 표현으로 "경로에
/// 개행이 있을 때 출력을 파싱할 수 있게 한다" — 필드가 줄바꿈이 아니라 NUL로
/// 끝난다(빈 줄 대신 빈 필드가 항목 경계). `worktree <path>` 필드가 새 항목을 열고,
/// `branch refs/heads/<name>`이 브랜치, `detached`는 None, `bare`는 **버린다**
/// (체크아웃이 없어 셸을 열 수 없다). `locked`/`prunable` 필드는 무시한다.
/// 반환하는 bool은 상한(MAX_WORKTREE_ROWS) 때문에 항목을 실제로 버렸는지다 —
/// bare로 버린 항목은 잘림이 아니라 애초에 표시 대상이 아니므로 포함하지 않는다.
fn parse_worktree_list(porcelain: &str, repo_root: &Path) -> (Vec<GitWorktreeRow>, bool) {
    let mut rows: Vec<GitWorktreeRow> = Vec::new();
    let mut truncated = false;
    let mut path: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut bare = false;
    let mut flush = |path: &mut Option<String>, branch: &mut Option<String>, bare: &mut bool| {
        let taken = path.take();
        let taken_branch = branch.take();
        let was_bare = std::mem::replace(bare, false);
        let Some(taken) = taken else { return };
        if was_bare {
            return;
        }
        // 새 항목을 담기 *전에* 상한을 검사한다 — merge_status_rows와 같은 이유.
        if rows.len() >= MAX_WORKTREE_ROWS {
            truncated = true;
            return;
        }
        let as_path = Path::new(&taken);
        let name = as_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| taken.clone());
        let current = as_path == repo_root;
        rows.push(GitWorktreeRow {
            path: taken,
            name,
            branch: taken_branch,
            current,
        });
    };
    // NUL 구분 필드 — 경로에 개행이 있어도 안 잘린다(빈 문자열 필터는 안 쓴다: 빈
    // 필드 자체가 non-z의 빈 줄과 같은 항목 경계 신호라 flush 판단에 쓰인다).
    for field in porcelain.split('\0') {
        if let Some(rest) = field.strip_prefix("worktree ") {
            flush(&mut path, &mut branch, &mut bare);
            path = Some(rest.to_owned()); // trim 금지 — 경로 끝 공백도 유효한 경로다.
        } else if let Some(rest) = field.strip_prefix("branch refs/heads/") {
            branch = Some(rest.to_owned());
        } else if field == "bare" {
            bare = true;
        }
    }
    flush(&mut path, &mut branch, &mut bare);
    (rows, truncated)
}

/// `git log -z --format=%h%x00%s%x00%ct` 파싱 — 필드도 항목도 NUL로 끝나므로 NUL로만
/// 나누면 제목에 탭·따옴표·유니코드가 있어도 안 어긋난다(git은 커밋 메시지에 NUL을
/// 허용하지 않는다). 순서는 git log 출력(최신순) 그대로다. sha·시각이 손상된 항목은
/// 건너뛰고, 반환하는 bool은 상한(MAX_BRANCH_COMMITS) 때문에 항목을 실제로 버렸는지다.
fn parse_branch_commits(log_z: &str) -> (Vec<GitCommitRow>, bool) {
    // 마지막 NUL 뒤는 바이트 상한에서 잘린 미완성 조각이다 — 버린다.
    let terminated = log_z.rfind('\0').map_or("", |end| &log_z[..end]);
    let mut fields = terminated.split('\0');
    let mut rows = Vec::new();
    let mut truncated = false;
    while let (Some(sha), Some(subject), Some(time)) = (fields.next(), fields.next(), fields.next())
    {
        let (Some(sha), Ok(time)) = (CommitSha::parse(sha), time.parse()) else {
            continue;
        };
        // 새 항목을 담기 *전에* 상한을 검사한다 — merge_status_rows와 같은 이유.
        if rows.len() >= MAX_BRANCH_COMMITS {
            truncated = true;
            break;
        }
        rows.push(GitCommitRow {
            sha,
            subject: subject.to_owned(),
            time,
        });
    }
    (rows, truncated)
}

/// 셸을 열 수 있는 경로인가 — 빈 값·상한 초과·NUL은 클릭 intent를 만들지 않는다.
fn worktree_path_is_spawnable(path: &str) -> bool {
    !path.is_empty() && path.len() <= WORKTREE_PATH_MAX_BYTES && !path.as_bytes().contains(&0)
}

/// 워크트리가 메인 하나뿐이면 섹션을 그리지 않는다 — 정보가 0이다.
fn worktree_section_visible(snapshot: &GitPanelSnapshot) -> bool {
    snapshot.worktrees.len() > 1
}

/// `rev-list --left-right --count upstream...HEAD` → (ahead, behind).
/// 출력은 "behind\tahead"(왼쪽=upstream) 순서다.
fn parse_ahead_behind(output: &str) -> Option<(u32, u32)> {
    let mut parts = output.trim().split('\t');
    let behind: u32 = parts.next()?.trim().parse().ok()?;
    let ahead: u32 = parts.next()?.trim().parse().ok()?;
    Some((ahead, behind))
}

/// 렌더용 파일명/디렉터리 분리 — "crates/app/src/app.rs" → ("app.rs", "crates/app/src").
fn split_row_path(rel_path: &str) -> (&str, &str) {
    match rel_path.rsplit_once('/') {
        Some((dir, name)) => (name, dir),
        None => (rel_path, ""),
    }
}

/// 목록용 폴더 표시 — 끝 두 단계만 남기고 앞은 「…/」로 줄인다. 300pt 목록에서
/// 앞부분부터 잘리면 `crates/…`만 남아 같은 저장소의 파일끼리 구분되지 않았다
/// (2026-10-11 시안). 전체 경로는 행 hover와 diff 헤더에 그대로 있다.
fn short_dir(dir: &str) -> std::borrow::Cow<'_, str> {
    let mut parts = dir.rsplitn(3, '/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(last), Some(second), Some(_)) => format!("…/{second}/{last}").into(),
        _ => dir.into(),
    }
}

/// 좌측 목록 필터 — `rel_path`가 `filter`에 대소문자 무시로 걸리는 행만 남긴다
/// (2026-08-18 스펙 §좌측 목록 필터). 빈 `filter`는 필터 없음과 같아 전부 통과시킨다
/// — `aux_search::contains_match`는 빈 needle이면 항상 거짓을 돌려주므로 이 분기가
/// 없으면 필터가 없을 때도 전부 걸러진다. `changes`/`committed` 섹션에만 쓰고
/// 워크트리 섹션에는 쓰지 않는다(경로 목록이라 검색 대상이 아니다).
fn filter_file_rows<'a>(rows: &'a [GitFileRow], filter: &str) -> Vec<&'a GitFileRow> {
    rows.iter()
        .filter(|row| {
            filter.is_empty() || crate::ui::aux_search::contains_match(&row.rel_path, filter)
        })
        .collect()
}

/// ahead/추가는 성공색, behind/삭제는 에러색 — 세션/에이전트 진행 상태가 아니라
/// 일반 git 통계라 `agent_visuals::status_color`가 아니라 designall 토큰을 쓴다.
/// `agent_visuals::status_color`는 dark_mode 분기가 없는 다크 전용 파스텔이라
/// 라이트 테마에서 대비가 낮았다(중요 리뷰 지적 4번) — designall.rs의
/// `Tokens::success`/`error`는 정확히 이 "체계 밖의 일반 성공/실패 표시" 용도로
/// 예약돼 있다(그 파일 상단 주석 참고).
fn stat_color(tokens: crate::ui::designall::Tokens, positive: bool) -> egui::Color32 {
    if positive {
        tokens.success
    } else {
        tokens.error
    }
}

/// porcelain 상태 문자 → (배지 글자, 색, hover 설명 i18n 키). 행 끝의 `M`·`?` 글자만으로는
/// 의미가 읽히지 않아(2026-10-11 시안) 행 앞의 색 배지와 설명으로 바꾼다.
fn status_badge_style(
    status: char,
    tokens: crate::ui::designall::Tokens,
) -> (char, egui::Color32, &'static str) {
    match status {
        'M' => ('M', tokens.warning, "git.status.modified"),
        'A' => ('A', tokens.success, "git.status.added"),
        'D' => ('D', tokens.error, "git.status.deleted"),
        'R' => ('R', tokens.accent, "git.status.renamed"),
        'C' => ('C', tokens.accent, "git.status.copied"),
        'U' => ('U', tokens.error, "git.status.conflict"),
        '?' => ('N', tokens.muted_text, "git.status.untracked"),
        other => (other, tokens.muted_text, "git.status.other"),
    }
}

/// 18pt 색 배지 하나. hover 전용 sense라 행 클릭을 가로채지 않는다.
fn status_badge(
    ui: &mut egui::Ui,
    status: char,
    tokens: crate::ui::designall::Tokens,
    catalog: &i18n::Catalog,
) {
    let (letter, color, key) = status_badge_style(status, tokens);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, 4.0, color.gamma_multiply(0.16));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        letter,
        egui::FontId::monospace(10.5),
        color,
    );
    response.on_hover_text(catalog.t(key, &[]));
}

/// `git remote get-url origin` 출력을 OWNER/REPO 기준 GitHub HTTPS URL로 정규화한다.
/// 지원 형식: `https://github.com/OWNER/REPO(.git)`, `git@github.com:OWNER/REPO(.git)`.
/// github.com이 아니거나 OWNER/REPO 형태가 아니면 None(스펙 §4 — ↗ 아이콘 숨김 조건).
fn normalize_github_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("git@github.com:"))?;
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let rest = rest.trim_matches('/');
    let mut parts = rest.split('/');
    let (Some(owner), Some(repo), None) = (parts.next(), parts.next(), parts.next()) else {
        return None; // OWNER/REPO 정확히 2세그먼트가 아니면(빈 값 포함) 거부.
    };
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("https://github.com/{owner}/{repo}"))
}

/// git 수집 타임아웃/바이트 상한 — diff_panel과 동일 정책(2026-08-15 스펙 §3).
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_LIST_BYTES: usize = 200 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitPanelErrorCode {
    NoRepo,
    CollectionFailed,
}

/// 세션 cwd에서 패널 스냅샷을 수집한다. **App host 스레드에서만 부른다**(blocking git).
pub fn collect_snapshot(cwd: &Path) -> Result<GitPanelSnapshot, GitPanelErrorCode> {
    let repo_root =
        crate::git_cli::repo_root(cwd, GIT_TIMEOUT).map_err(|_| GitPanelErrorCode::NoRepo)?;
    let run = |args: &[&str]| -> Result<(String, bool), GitPanelErrorCode> {
        crate::git_cli::run_git_limited(&repo_root, args, GIT_TIMEOUT, MAX_LIST_BYTES)
            .map_err(|_| GitPanelErrorCode::CollectionFailed)
    };

    // 빈 repo(커밋 0개)는 여기서 CollectionFailed로 떨어진다(HEAD가 없어 rev-parse 실패).
    // 빈 repo에서 status만이라도 보여주는 건 범위 외(스펙 §6 "섹션 단위 오류" 대상, 2026-08-15).
    let (branch_raw, _) = run(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    let branch = if branch_raw.trim() == "HEAD" {
        // detached — 짧은 SHA로 표시.
        run(&["rev-parse", "--short", "HEAD"])?.0.trim().to_owned()
    } else {
        branch_raw.trim().to_owned()
    };

    // 업스트림: 추적 브랜치 → origin/HEAD 폴백 → None(스펙 §3).
    let upstream = run(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
        .ok()
        .map(|(s, _)| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            run(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
                .ok()
                .map(|(s, _)| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        });

    let (mut ahead, mut behind) = (0, 0);
    let mut committed = Vec::new();
    let mut committed_truncated = false;
    let mut commits = Vec::new();
    let mut commits_truncated = false;
    if let Some(upstream) = upstream.as_deref() {
        let range = format!("{upstream}...HEAD");
        if let Ok((counts, _)) = run(&["rev-list", "--left-right", "--count", &range])
            && let Some((a, b)) = parse_ahead_behind(&counts)
        {
            (ahead, behind) = (a, b);
        }
        if let Ok((base, _)) = run(&["merge-base", upstream, "HEAD"]) {
            let base = base.trim().to_owned();
            let range = format!("{base}..HEAD");
            let (numstat, t1) = run(&["diff", "--no-ext-diff", "--numstat", &range])?;
            let (names, t2) = run(&["diff", "--no-ext-diff", "--name-status", &range])?;
            let (rows, rows_truncated) = merge_committed_rows(&numstat, &names);
            committed = rows;
            committed_truncated = t1 || t2 || rows_truncated;
            // 같은 기준의 커밋 목록. 이 조회가 실패해도 스냅샷 전체는 죽이지 않는다
            // (remote 조회와 같은 규칙). 상한보다 하나 더 받아야 정확히 상한인지
            // 넘쳤는지를 가를 수 있다(2026-10-11 PR 4).
            let max_count = format!("--max-count={}", MAX_BRANCH_COMMITS + 1);
            if let Ok((log, log_truncated)) =
                run(&["log", "-z", &max_count, "--format=%h%x00%s%x00%ct", &range])
            {
                let (rows, rows_truncated) = parse_branch_commits(&log);
                commits = rows;
                commits_truncated = log_truncated || rows_truncated;
            }
        }
    }

    let (porcelain, t3) = run(&["status", "--porcelain", "-z", "-uall"])?;
    let (numstat, t4) = run(&["diff", "--no-ext-diff", "--numstat", "HEAD"])?;
    let (changes, changes_rows_truncated) = merge_status_rows(&porcelain, &numstat);
    let changes_truncated = t3 || t4 || changes_rows_truncated;

    // origin remote → GitHub HTTPS 정규화. 실패(원격 없음/비GitHub)해도 None만 담고
    // 스냅샷 전체는 죽이지 않는다(Task 10 Step 7 소급 요구, 2026-08-15).
    let remote_https_base = run(&["remote", "get-url", "origin"])
        .ok()
        .and_then(|(s, _)| normalize_github_remote(s.trim()));

    // 워크트리 목록 — 스냅샷과 같은 IO 왕복에서 한 번만 부른다(스펙 §8-4).
    // 실패해도 스냅샷 전체를 죽이지 않는다(섹션 단위 오류 원칙, §6). -z: 경로에
    // 개행이 있어도 항목이 안 잘리게(중요 리뷰 지적 2번).
    let (worktrees, worktrees_truncated) = match run(&["worktree", "list", "--porcelain", "-z"]) {
        Ok((listing, truncated)) => {
            let (rows, parse_truncated) = parse_worktree_list(&listing, &repo_root);
            (rows, truncated || parse_truncated)
        }
        Err(_) => (Vec::new(), false),
    };

    Ok(GitPanelSnapshot {
        repo_root,
        branch,
        upstream,
        ahead,
        behind,
        changes,
        committed,
        changes_truncated,
        committed_truncated,
        remote_https_base,
        worktrees,
        worktrees_truncated,
        commits,
        commits_truncated,
    })
}

/// App host 스레드에서 실행할 git 패널 IO. capacity-1 — App이 in-flight 1개만 유지
/// (기존 `pending_app_host_action` 큐 규칙, 2026-08-15 Task 10).
#[derive(Debug)]
pub enum GitPanelIoRequest {
    Snapshot,
    /// Git 보조 본문의 파일 행 클릭이 요청한다(2026-08-15 2차, 스펙 §8-3).
    FileDiff {
        rel_path: String,
        mode: crate::ui::diff_viewer::DiffMode,
    },
    /// 커밋 목록에서 커밋을 펼칠 때 그 커밋이 바꾼 파일을 요청한다(2026-10-11 PR 4).
    CommitFiles {
        sha: CommitSha,
    },
}

#[derive(Debug)]
pub struct GitPanelIoIntent {
    pub generation: u64,
    pub cwd: PathBuf,
    pub request: GitPanelIoRequest,
}

#[derive(Debug)]
pub enum GitPanelIoResult {
    Snapshot(Result<GitPanelSnapshot, GitPanelErrorCode>),
    FileDiff(Result<crate::ui::diff_viewer::FileDiffView, GitPanelErrorCode>),
    /// 요청한 sha를 함께 돌려준다 — 패널은 지금 기다리는 커밋의 결과만 받는다.
    CommitFiles {
        sha: CommitSha,
        result: Result<GitCommitFiles, GitPanelErrorCode>,
    },
}

#[derive(Debug)]
pub struct GitPanelIoCompletion {
    pub generation: u64,
    pub result: GitPanelIoResult,
}

/// host 스레드 실행 — collect_snapshot, 파일 diff 또는 커밋 파일 수집(스펙 §3).
pub fn execute_io(intent: GitPanelIoIntent) -> GitPanelIoCompletion {
    let result = match &intent.request {
        GitPanelIoRequest::Snapshot => GitPanelIoResult::Snapshot(collect_snapshot(&intent.cwd)),
        GitPanelIoRequest::FileDiff { rel_path, mode } => {
            GitPanelIoResult::FileDiff(collect_file_diff(&intent.cwd, rel_path, *mode))
        }
        GitPanelIoRequest::CommitFiles { sha } => GitPanelIoResult::CommitFiles {
            sha: *sha,
            result: collect_commit_files(&intent.cwd, *sha),
        },
    };
    GitPanelIoCompletion {
        generation: intent.generation,
        result,
    }
}

/// 파일 하나의 diff를 수집한다 — Working은 워킹트리(untracked는 파일 전량 추가로
/// 합성), Branch는 upstream과의 merge-base 기준(스펙 §3).
fn collect_file_diff(
    cwd: &Path,
    rel_path: &str,
    mode: crate::ui::diff_viewer::DiffMode,
) -> Result<crate::ui::diff_viewer::FileDiffView, GitPanelErrorCode> {
    use crate::ui::diff_viewer::{DiffMode, parse_unified};
    let repo_root =
        crate::git_cli::repo_root(cwd, GIT_TIMEOUT).map_err(|_| GitPanelErrorCode::NoRepo)?;
    // 경로 인젝션 방어: rel_path는 스냅샷의 porcelain 출력에서 온 값이지만,
    // "--" 뒤에 둬 옵션 해석을 차단하고 NUL/절대경로는 거부한다.
    if rel_path.is_empty() || rel_path.contains('\0') || rel_path.starts_with('/') {
        return Err(GitPanelErrorCode::CollectionFailed);
    }
    let run = |args: &[&str]| {
        crate::git_cli::run_git_limited(&repo_root, args, GIT_TIMEOUT, MAX_LIST_BYTES)
            .map_err(|_| GitPanelErrorCode::CollectionFailed)
    };
    match mode {
        DiffMode::Working => {
            let (text, truncated) = run(&["diff", "--no-ext-diff", "HEAD", "--", rel_path])?;
            if text.trim().is_empty() {
                // untracked — `git diff HEAD`는 untracked를 보여주지 않는다. 예전엔
                // std::fs::read로 파일 전체를 무계 메모리에 올린 뒤 MAX_LIST_BYTES로
                // 잘랐다(치명 리뷰 지적 — 읽기 자체가 무계). diff_panel의
                // append_untracked_section과 같은 방식으로 `--no-index /dev/null`을
                // 유계 실행기(run_git_limited)로 돌려 이미 unified diff 형태의
                // 전량 추가 출력을 받는다. `--`로 옵션 오인을 막는다.
                let (untracked, untracked_truncated) = run(&[
                    "diff",
                    "--no-ext-diff",
                    "--no-index",
                    "--",
                    "/dev/null",
                    rel_path,
                ])?;
                return Ok(parse_unified(&untracked, untracked_truncated));
            }
            Ok(parse_unified(&text, truncated))
        }
        DiffMode::Branch => {
            let (upstream, _) =
                run(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])?;
            let (base, _) = run(&["merge-base", upstream.trim(), "HEAD"])?;
            let range = format!("{}..HEAD", base.trim());
            let (text, truncated) = run(&["diff", "--no-ext-diff", &range, "--", rel_path])?;
            Ok(parse_unified(&text, truncated))
        }
        DiffMode::Commit(sha) => {
            // 루트 커밋은 부모가 없어도 `git show`가 새 파일 diff로 보여 준다. 머지 커밋은
            // collect_commit_files와 같은 기준(첫 부모 대비)이다.
            let (text, truncated) = run(&[
                "show",
                "--no-ext-diff",
                "--first-parent",
                "--format=",
                sha.as_str(),
                "--",
                rel_path,
            ])?;
            Ok(parse_unified(&text, truncated))
        }
    }
}

/// 커밋 하나가 바꾼 파일 — committed 섹션과 같은 numstat + name-status 병합
/// (merge_committed_rows, 상한 MAX_PANEL_FILES). 루트 커밋은 `git show`가 빈 트리
/// 대비로 보여 준다. 머지 커밋은 첫 부모 대비로 본다 — 기본값(--cc)은 충돌 없는
/// 머지의 name-status를 비워 머지가 들여온 파일이 목록에서 사라진다(2026-10-11 실측).
/// `--diff-merges=first-parent`(git 2.31+)가 아니라 `--first-parent`를 쓴다 — 최신 git에서는
/// 같은 뜻이고, 옛 git에서도 옵션 오류로 모든 커밋이 실패하지 않고 기본값으로 돈다.
fn collect_commit_files(cwd: &Path, sha: CommitSha) -> Result<GitCommitFiles, GitPanelErrorCode> {
    let repo_root =
        crate::git_cli::repo_root(cwd, GIT_TIMEOUT).map_err(|_| GitPanelErrorCode::NoRepo)?;
    let run = |format: &str| {
        crate::git_cli::run_git_limited(
            &repo_root,
            &[
                "show",
                "--no-ext-diff",
                "--first-parent",
                format,
                "--format=",
                sha.as_str(),
            ],
            GIT_TIMEOUT,
            MAX_LIST_BYTES,
        )
        .map_err(|_| GitPanelErrorCode::CollectionFailed)
    };
    let (numstat, t1) = run("--numstat")?;
    let (names, t2) = run("--name-status")?;
    let (rows, rows_truncated) = merge_committed_rows(&numstat, &names);
    Ok(GitCommitFiles {
        rows,
        truncated: t1 || t2 || rows_truncated,
    })
}

/// 패널이 App에 요청하는 intent — leaf는 git도 뷰 전환도 직접 하지 않는다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitPanelAction {
    Refresh,
    /// upstream의 GitHub 브랜치 페이지 열기 — URL 구성은 App이 remote를 보고 한다.
    OpenRemoteBranch,
    ShowFileDiff {
        rel_path: String,
        mode: crate::ui::diff_viewer::DiffMode,
    },
    /// 워크트리 행 클릭 — App이 그 경로에서 새 셸을 연다(워크트리를 만들지도 지우지도 않는다).
    OpenWorktreeShell {
        path: String,
    },
    /// 커밋 행을 펼쳤는데 그 커밋의 파일 목록이 없다 — App이 불러와
    /// `set_commit_files`로 돌려준다(2026-10-11 PR 4).
    LoadCommitFiles {
        sha: CommitSha,
    },
}

#[derive(Default)]
pub struct GitPanelUi {
    snapshot: Option<Result<GitPanelSnapshot, GitPanelErrorCode>>,
    loading: bool,
    changes_show_all: bool,
    committed_show_all: bool,
    changes_collapsed: bool,
    committed_collapsed: bool,
    worktrees_collapsed: bool,
    /// 오른쪽 diff가 보여 주는 파일 — 이 패널이 `ShowFileDiff`를 낼 때 기록한다.
    /// diff 뷰어를 여는 경로가 그 액션 하나뿐이고 두 UI는 `reset_git_surfaces`에서
    /// 함께 초기화되므로 따로 동기화할 필요가 없다. 같은 경로도 모드가 다르면 다른 행이다.
    selected: Option<(String, crate::ui::diff_viewer::DiffMode)>,
    commit_list: CommitListState,
}

impl GitPanelUi {
    pub fn set_loading(&mut self) {
        self.loading = true;
    }

    pub fn set_snapshot(&mut self, result: Result<GitPanelSnapshot, GitPanelErrorCode>) {
        self.loading = false;
        self.snapshot = Some(result);
    }

    /// `LoadCommitFiles`의 결과 — 지금 기다리는 커밋의 것만 받고, 다른 커밋으로 넘어간
    /// 뒤 늦게 온 결과는 버린다.
    pub fn set_commit_files(
        &mut self,
        sha: CommitSha,
        result: Result<GitCommitFiles, GitPanelErrorCode>,
    ) {
        if let Some((waiting, state)) = &mut self.commit_list.loaded
            && *waiting == sha
        {
            *state = match result {
                Ok(files) => CommitFilesState::Ready(files),
                Err(_) => CommitFilesState::Failed,
            };
        }
    }

    /// ↗ 버튼이 쓸 (remote_https_base, branch) — 둘 다 있어야 Some. 스냅샷이 이미
    /// remote를 정규화해 담아 두므로 App이 IO 없이 즉시 URL을 구성할 수 있다
    /// (스펙 §4, Task 10 Step 7).
    pub fn remote_target(&self) -> Option<(String, String)> {
        let snap = self.snapshot.as_ref()?.as_ref().ok()?;
        let base = snap.remote_https_base.clone()?;
        Some((base, snap.branch.clone()))
    }

    /// `filter`: 보조 검색 좌측 목록 필터(2026-08-18 스펙) — 빈 문자열이면 필터 없음.
    /// `changes`/`committed` 행만 `rel_path` 기준으로 거르고, 워크트리 섹션은 그대로
    /// 둔다(경로 목록이라 검색 대상이 아니고, 통째로 사라지면 혼란스럽다).
    pub fn render(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        filter: &str,
    ) -> Option<GitPanelAction> {
        // `section` 클로저 매개변수용 — 전체 경로로 쓰면 rustfmt가 클로저 전체를 다시
        // 들여써 무관한 행까지 diff에 섞인다.
        use crate::ui::diff_viewer::DiffMode;
        // 필드별로 분해해 빌린다 — snapshot은 &만 필요하고 changes_collapsed 등은
        // 동시에 &mut가 필요해, self를 통째로 들고 있으면(예전엔 self.snapshot을
        // clone()해 이 충돌을 피했다) 프레임마다 최대 ~1056개 String을 복제했다
        // (중요 리뷰 지적 5번). 필드마다 서로 다른 경로라 disjoint borrow로 풀린다.
        let Self {
            snapshot,
            loading,
            changes_show_all,
            committed_show_all,
            changes_collapsed,
            committed_collapsed,
            worktrees_collapsed,
            selected,
            commit_list,
        } = self;
        let snap = match snapshot.as_ref() {
            None => {
                ui.weak(catalog.t("diff.loading", &[]));
                return if *loading {
                    None
                } else {
                    Some(GitPanelAction::Refresh)
                };
            }
            Some(Err(GitPanelErrorCode::NoRepo)) => {
                ui.weak(catalog.t("diff.no_cwd", &[]));
                return None;
            }
            Some(Err(GitPanelErrorCode::CollectionFailed)) => {
                ui.weak(catalog.t("git.error.snapshot", &[]));
                return None;
            }
            Some(Ok(snap)) => snap,
        };

        let tokens = crate::ui::designall::tokens(ui.visuals());
        let mut action = None;
        let filter_active = !filter.is_empty();
        let filtered_changes = filter_file_rows(&snap.changes, filter);
        let filtered_committed = filter_file_rows(&snap.committed, filter);

        // ── 헤더: 브랜치 / → upstream ↑a ↓b ↗ ──────────────────────────
        ui.strong(&snap.branch);
        ui.horizontal(|ui| {
            match snap.upstream.as_deref() {
                Some(upstream) => {
                    ui.weak("→");
                    ui.monospace(upstream);
                    if snap.ahead > 0 {
                        ui.colored_label(stat_color(tokens, true), format!("↑{}", snap.ahead));
                    }
                    if snap.behind > 0 {
                        ui.colored_label(stat_color(tokens, false), format!("↓{}", snap.behind));
                    }
                    // remote가 GitHub일 때만 보인다(스펙 §4 숨김 조건). 계획서 원안은
                    // upstream만으로 항상 그렸는데, collect_snapshot이 이미
                    // remote_https_base로 이 조건을 계산해 두므로 그걸 쓴다
                    // (2026-08-15 조정 — 타입 계약은 그대로, 렌더 조건만 보강).
                    if snap.remote_https_base.is_some()
                        && ui
                            .small_button("↗")
                            .on_hover_text(catalog.t("git.open_remote", &[]))
                            .clicked()
                    {
                        action = Some(GitPanelAction::OpenRemoteBranch);
                    }
                }
                None => {
                    ui.weak(catalog.t("git.upstream_none", &[]));
                }
            }
            if ui
                .small_button("⟳")
                .on_hover_text(catalog.t("diff.refresh", &[]))
                .clicked()
            {
                action = Some(GitPanelAction::Refresh);
            }
        });
        ui.separator();

        // ── 섹션 2개 ─────────────────────────────────────────────────
        let section = |ui: &mut egui::Ui,
                       title_key: &str,
                       rows: &[&GitFileRow],
                       collapsed: &mut bool,
                       show_all: &mut bool,
                       mode: crate::ui::diff_viewer::DiffMode,
                       action: &mut Option<GitPanelAction>,
                       selected: &mut Option<(String, DiffMode)>| {
            ui.horizontal(|ui| {
                let arrow = if *collapsed { "›" } else { "∨" };
                if ui
                    .selectable_label(
                        false,
                        format!("{arrow} {} {}", catalog.t(title_key, &[]), rows.len()),
                    )
                    .clicked()
                {
                    *collapsed = !*collapsed;
                }
                if !*collapsed && rows.len() > SECTION_COLLAPSED_ROWS {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let label = catalog.t("git.show_all", &[]);
                        if ui.selectable_label(*show_all, label).clicked() {
                            *show_all = !*show_all;
                        }
                    });
                }
            });
            if *collapsed {
                return;
            }
            let visible = if *show_all {
                rows.len()
            } else {
                rows.len().min(SECTION_COLLAPSED_ROWS)
            };
            for row in &rows[..visible] {
                let id_salt = ("git-panel-row", title_key, row.rel_path.as_str());
                if let Some(clicked) = file_row(ui, id_salt, row, mode, selected, tokens, catalog) {
                    *action = Some(clicked);
                }
            }
        };

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if filter_active {
                    // 필터가 걸려 있는데 두 섹션 모두 0행이면 안내 한 줄(2026-08-18 스펙
                    // §좌측 목록 필터) — "왜 안 보이지"가 되지 않게.
                    if filtered_changes.is_empty() && filtered_committed.is_empty() {
                        ui.weak(catalog.t("search.no_match", &[]));
                        // 워크트리 섹션에는 필터와 무관하게 정보가 있을 수 있다 — 여기서
                        // 돌아가면 그 섹션까지 감춘다(diff.clean과 같은 처리, 스펙 §8-4).
                        if !worktree_section_visible(snap) {
                            return;
                        }
                    }
                } else if snap.changes.is_empty() && snap.committed.is_empty() {
                    ui.weak(catalog.t("diff.clean", &[]));
                    // 워크트리 섹션에는 clean과 무관하게 정보가 있을 수 있다 — 여기서
                    // 돌아가면 그 섹션까지 감춘다(스펙 §8-4).
                    if !worktree_section_visible(snap) {
                        return;
                    }
                }
                section(
                    ui,
                    "git.section.changes",
                    &filtered_changes,
                    changes_collapsed,
                    changes_show_all,
                    crate::ui::diff_viewer::DiffMode::Working,
                    &mut action,
                    selected,
                );
                if snap.changes_truncated {
                    ui.weak(catalog.t("diff.truncated", &[]));
                }
                ui.add_space(6.0);
                section(
                    ui,
                    "git.section.committed",
                    &filtered_committed,
                    committed_collapsed,
                    committed_show_all,
                    crate::ui::diff_viewer::DiffMode::Branch,
                    &mut action,
                    selected,
                );
                if snap.committed_truncated {
                    ui.weak(catalog.t("diff.truncated", &[]));
                }
                // 브랜치 커밋 목록 — 파일 행 아래, 섹션을 접으면 함께 숨는다(2026-10-11 PR 4).
                if !*committed_collapsed {
                    render_commit_list(
                        ui,
                        catalog,
                        tokens,
                        snap,
                        commit_list,
                        selected,
                        &mut action,
                    );
                }

                // ── 섹션 3: 워크트리 ─────────────────────────────────────────
                if worktree_section_visible(snap) {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let arrow = if *worktrees_collapsed { "›" } else { "∨" };
                        if ui
                            .selectable_label(
                                false,
                                format!(
                                    "{arrow} {} {}",
                                    catalog.t("git.section.worktrees", &[]),
                                    snap.worktrees.len()
                                ),
                            )
                            .clicked()
                        {
                            *worktrees_collapsed = !*worktrees_collapsed;
                        }
                    });
                    if !*worktrees_collapsed {
                        for row in &snap.worktrees {
                            // 파일 행과 같은 패턴 — 자식 Label이 클릭을 삼키지 않도록
                            // 스코프 자체를 하나의 논리 위젯으로 만든다(위 file row 주석 참고).
                            let scope = ui.scope_builder(
                                egui::UiBuilder::new()
                                    .id_salt(("git-panel-worktree", row.path.as_str()))
                                    .sense(egui::Sense::click()),
                                |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.horizontal(|ui| {
                                        ui.label(&row.name);
                                        match row.branch.as_deref() {
                                            Some(branch) => ui.weak(branch),
                                            None => {
                                                ui.weak(catalog.t("git.worktree.detached", &[]))
                                            }
                                        };
                                        if row.current {
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    ui.weak(catalog.t("git.worktree.current", &[]))
                                                },
                                            );
                                        }
                                    });
                                },
                            );
                            let response = scope
                                .response
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .on_hover_text(catalog.t("git.worktree.open_hint", &[]));
                            // 접근성 이름을 경로로 명시한다 — kittest가 Role::Button + 이
                            // 라벨로 행 전체를 정확히 겨냥할 수 있다(위 file row 주석과 동일 이유).
                            response.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Button,
                                    ui.is_enabled(),
                                    row.path.as_str(),
                                )
                            });
                            if response.clicked() && worktree_path_is_spawnable(&row.path) {
                                action = Some(GitPanelAction::OpenWorktreeShell {
                                    path: row.path.clone(),
                                });
                            }
                        }
                        if snap.worktrees_truncated {
                            ui.weak(catalog.t("git.worktrees_truncated", &[]));
                        }
                    }
                }
            });
        action
    }
}

/// 파일 행 하나 — 상태 배지·파일명·줄인 폴더·증감, 선택이면 면과 2pt accent 레일.
/// 변경/브랜치 커밋 섹션과 커밋별 파일 목록이 같이 쓴다. 클릭하면 선택으로 기록하고
/// 그 모드의 `ShowFileDiff`를 돌려준다.
fn file_row(
    ui: &mut egui::Ui,
    id_salt: impl egui::AsIdSalt,
    row: &GitFileRow,
    mode: crate::ui::diff_viewer::DiffMode,
    selected: &mut Option<(String, crate::ui::diff_viewer::DiffMode)>,
    tokens: crate::ui::designall::Tokens,
    catalog: &i18n::Catalog,
) -> Option<GitPanelAction> {
    let (name, dir) = split_row_path(&row.rel_path);
    let is_selected = selected
        .as_ref()
        .is_some_and(|(path, m)| path == &row.rel_path && *m == mode);
    // 배경은 행 크기를 안 뒤에 칠해야 하므로 자리만 먼저 잡는다(글자 아래에 깔림).
    let background = ui.painter().add(egui::Shape::Noop);
    // 계획서 원안은 `ui.horizontal(..).response.interact(Sense::click())`로
    // 행 전체를 클릭 가능하게 했는데, kittest로 돌려보면 자식 Label의
    // 텍스트 자체를 클릭했을 때 부모로 새지 않는다 — egui는 포인터 아래
    // "가장 안쪽" 위젯을 hit-test로 고르고, 그 위젯이 Sense::hover뿐이라도
    // 부모로 폴백하지 않는다(빈 여백을 클릭하면 잡히는 것으로 실측
    // 확인). 이 저장소는 이미 같은 문제를 겪었고(work_history.rs 카드/
    // 헤더 토글, 커밋 0e934c0) `scope_builder(UiBuilder::sense(click))` +
    // 명시적 `widget_info(Role::Button)`로 스코프 자체를 하나의 논리
    // 위젯으로 만드는 패턴을 쓴다 — 그 관례로 맞춘다
    // (2026-08-15 조정).
    let toggle = ui.scope_builder(
        egui::UiBuilder::new()
            .id_salt(id_salt)
            .sense(egui::Sense::click()),
        |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                status_badge(ui, row.status, tokens, catalog);
                ui.label(name);
                if !dir.is_empty() {
                    ui.weak(short_dir(dir));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(d) = row.dels.filter(|d| *d > 0) {
                        ui.colored_label(stat_color(tokens, false), format!("−{d}"));
                    }
                    if let Some(a) = row.adds.filter(|a| *a > 0) {
                        ui.colored_label(stat_color(tokens, true), format!("+{a}"));
                    }
                });
            });
        },
    );
    let response = toggle
        .response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(row.rel_path.as_str());
    let rect = response.rect;
    if let Some(fill) = crate::ui::designall::row_fill(tokens, is_selected, response.hovered()) {
        ui.painter()
            .set(background, egui::Shape::rect_filled(rect, 4.0, fill));
    }
    if is_selected {
        // 사이드 레일 선택과 같은 2pt accent 레일.
        let rail = egui::Rect::from_min_max(
            rect.left_top(),
            egui::pos2(rect.left() + 2.0, rect.bottom()),
        );
        ui.painter().rect_filled(rail, 0.0, tokens.accent);
    }
    // 접근성 이름을 rel_path로 명시한다 — kittest가 Role::Button + 이
    // 라벨로 행 전체(자식 Label이 아니라)를 정확히 겨냥할 수 있다.
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Button,
            ui.is_enabled(),
            is_selected,
            row.rel_path.as_str(),
        )
    });
    if !response.clicked() {
        return None;
    }
    *selected = Some((row.rel_path.clone(), mode));
    Some(GitPanelAction::ShowFileDiff {
        rel_path: row.rel_path.clone(),
        mode,
    })
}

/// 브랜치 커밋 목록의 펼침 상태(2026-10-11 PR 4). 한 번에 커밋 하나만 펼치고 파일
/// 목록도 그 하나분만 둔다 — 메모리가 MAX_PANEL_FILES 행으로 유계이고, App의 git IO는
/// latest-only라 여러 커밋을 동시에 불러오면 먼저 요청한 쪽 완료가 버려져 「불러오는
/// 중」에 멈춘 커밋이 화면에 남는다.
#[derive(Default)]
struct CommitListState {
    expanded: Option<CommitSha>,
    /// 마지막으로 요청한 커밋과 그 파일 목록 상태 — 접어도 남겨 둬 다시 펼칠 때 쓴다.
    loaded: Option<(CommitSha, CommitFilesState)>,
}

enum CommitFilesState {
    Loading,
    Ready(GitCommitFiles),
    Failed,
}

impl CommitListState {
    /// 커밋 행 클릭 — 펼침을 토글하고, 펼쳤는데 그 커밋의 파일 목록이 아직 없으면
    /// 로드 액션을 돌려준다. 실패했거나 결과가 오지 않은(latest-only로 버려졌을 수 있는)
    /// 커밋은 다시 펼칠 때 재요청한다 — 접었다 펴기가 곧 재시도다.
    fn toggle(&mut self, sha: CommitSha) -> Option<GitPanelAction> {
        if self.expanded == Some(sha) {
            self.expanded = None;
            return None;
        }
        self.expanded = Some(sha);
        if matches!(&self.loaded, Some((loaded, CommitFilesState::Ready(_))) if *loaded == sha) {
            return None;
        }
        self.loaded = Some((sha, CommitFilesState::Loading));
        Some(GitPanelAction::LoadCommitFiles { sha })
    }
}

/// 「브랜치에 COMMIT 됨」 파일 행 아래의 커밋 목록 — 행: ›/∨ · sha(모노, accent) ·
/// 제목(말줄임, 전체는 hover) · 상대 시각. 커밋 행 클릭은 펼침을 토글하고, 펼친 커밋의
/// 파일 행 클릭은 그 커밋 기준 diff를 연다. 좌측 목록 필터는 걸지 않는다(파일 경로
/// 필터라 커밋은 대상이 아니다 — 워크트리 섹션과 같은 규칙).
fn render_commit_list(
    ui: &mut egui::Ui,
    catalog: &i18n::Catalog,
    tokens: crate::ui::designall::Tokens,
    snap: &GitPanelSnapshot,
    state: &mut CommitListState,
    selected: &mut Option<(String, crate::ui::diff_viewer::DiffMode)>,
    action: &mut Option<GitPanelAction>,
) {
    if snap.commits.is_empty() {
        return;
    }
    let now = crate::ui::work_history::unix_now();
    ui.add_space(4.0);
    ui.weak(catalog.t(
        "git.commits.title",
        &[("count", &snap.commits.len().to_string())],
    ));
    for commit in &snap.commits {
        let open = state.expanded == Some(commit.sha);
        // 파일 행과 같은 패턴 — 자식 Label이 클릭을 삼키지 않도록 스코프 자체를 하나의
        // 논리 위젯으로 만든다(render의 file row 주석 참고).
        let scope = ui.scope_builder(
            egui::UiBuilder::new()
                .id_salt(("git-panel-commit", commit.sha.as_str()))
                .sense(egui::Sense::click()),
            |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.weak(if open { "∨" } else { "›" });
                    ui.label(
                        egui::RichText::new(commit.sha.as_str())
                            .monospace()
                            .color(tokens.accent),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.weak(crate::ui::work_history::relative_age_text(
                            catalog,
                            commit.time,
                            now,
                        ));
                        // 시각을 뺀 남은 폭에 왼쪽부터 그리고, 넘치면 말줄임한다.
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.add(egui::Label::new(commit.subject.as_str()).truncate());
                        });
                    });
                });
            },
        );
        let response = scope
            .response
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(commit.subject.as_str());
        // 접근성 이름을 「sha 제목」으로 명시한다 — kittest가 Role::Button + 이 라벨로
        // 행 전체를 겨냥한다(파일 행과 같은 이유).
        response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                ui.is_enabled(),
                format!("{} {}", commit.sha.as_str(), commit.subject),
            )
        });
        if response.clicked()
            && let Some(load) = state.toggle(commit.sha)
        {
            *action = Some(load);
        }
        if state.expanded == Some(commit.sha) {
            ui.indent(("git-panel-commit-files", commit.sha.as_str()), |ui| {
                render_commit_files(ui, catalog, tokens, commit.sha, state, selected, action);
            });
        }
    }
    if snap.commits_truncated {
        ui.weak(catalog.t(
            "git.commits_truncated",
            &[("count", &MAX_BRANCH_COMMITS.to_string())],
        ));
    }
}

/// 펼친 커밋의 파일 행 — 불러오는 중·실패·바뀐 파일 없음 안내를 포함한다.
fn render_commit_files(
    ui: &mut egui::Ui,
    catalog: &i18n::Catalog,
    tokens: crate::ui::designall::Tokens,
    sha: CommitSha,
    state: &CommitListState,
    selected: &mut Option<(String, crate::ui::diff_viewer::DiffMode)>,
    action: &mut Option<GitPanelAction>,
) {
    let files = match &state.loaded {
        Some((loaded, CommitFilesState::Ready(files))) if *loaded == sha => files,
        Some((loaded, CommitFilesState::Failed)) if *loaded == sha => {
            ui.weak(catalog.t("git.commit.files_error", &[]));
            return;
        }
        _ => {
            ui.weak(catalog.t("diff.loading", &[]));
            return;
        }
    };
    if files.rows.is_empty() {
        ui.weak(catalog.t("git.commit.files_empty", &[]));
    }
    let mode = crate::ui::diff_viewer::DiffMode::Commit(sha);
    for row in &files.rows {
        // 변경/브랜치 커밋 섹션과 같은 행(배지·줄인 폴더·선택 강조).
        let id_salt = ("git-panel-commit-file", sha.as_str(), row.rel_path.as_str());
        if let Some(clicked) = file_row(ui, id_salt, row, mode, selected, tokens, catalog) {
            *action = Some(clicked);
        }
    }
    if files.truncated {
        ui.weak(catalog.t("diff.truncated", &[]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain과_numstat을_경로로_병합한다() {
        // porcelain -z: "XY path\0" 반복. rename은 "R  new\0old\0".
        let porcelain = " M a.rs\0?? new.txt\0R  moved.rs\0old.rs\0MM both.rs\0";
        let numstat = "3\t1\ta.rs\n456\t221\tmoved.rs\n-\t-\tbin.png\n2\t0\tboth.rs\n";
        let (rows, truncated) = merge_status_rows(porcelain, numstat);
        // 순서는 porcelain 등장 순서를 유지한다.
        assert_eq!(rows.len(), 4);
        assert!(!truncated);
        assert_eq!(
            (
                rows[0].rel_path.as_str(),
                rows[0].status,
                rows[0].adds,
                rows[0].dels
            ),
            ("a.rs", 'M', Some(3), Some(1))
        );
        // untracked: numstat 없음 → 수치 None, 상태 '?'
        assert_eq!(
            (rows[1].rel_path.as_str(), rows[1].status, rows[1].adds),
            ("new.txt", '?', None)
        );
        // rename: 새 경로 기준, 상태는 X('R') — Y가 공백이므로.
        assert_eq!(
            (rows[2].rel_path.as_str(), rows[2].status, rows[2].adds),
            ("moved.rs", 'R', Some(456))
        );
        // staged+unstaged 겹침(XY="MM"): 워킹트리(Y) 우선 → 'M'.
        assert_eq!(
            (rows[3].rel_path.as_str(), rows[3].status),
            ("both.rs", 'M')
        );
    }

    #[test]
    fn numstat만_있는_경로는_committed_파서가_그대로_담는다() {
        // committed 섹션: numstat + name-status 병합. 바이너리는 "-\t-".
        let numstat = "12\t13\tsrc/ui/workspace.rs\n-\t-\tassets/logo.png\n";
        let name_status = "M\tsrc/ui/workspace.rs\nA\tassets/logo.png\n";
        let (rows, truncated) = merge_committed_rows(numstat, name_status);
        assert_eq!(rows.len(), 2);
        assert!(!truncated);
        assert_eq!(
            (
                rows[0].rel_path.as_str(),
                rows[0].status,
                rows[0].adds,
                rows[0].dels
            ),
            ("src/ui/workspace.rs", 'M', Some(12), Some(13))
        );
        assert_eq!(
            (
                rows[1].rel_path.as_str(),
                rows[1].status,
                rows[1].adds,
                rows[1].dels
            ),
            ("assets/logo.png", 'A', None, None)
        );
    }

    // 중요 리뷰 지적 3번 회귀 — 벡터가 이미 상한으로 잘린 뒤에 len() >= 상한을
    // 검사하면 "딱 상한이라 안 잘림"과 "넘쳐서 버림"을 구분 못 한다. 정확히
    // 상한이면 false, 하나 더 있으면 true를 고정한다.
    #[test]
    fn merge_status_rows는_상한에_정확히_닿으면_잘림이_아니다() {
        let mut porcelain = String::new();
        for i in 0..MAX_PANEL_FILES {
            porcelain.push_str(&format!(" M f{i}.rs\0"));
        }
        let (rows, truncated) = merge_status_rows(&porcelain, "");
        assert_eq!(rows.len(), MAX_PANEL_FILES);
        assert!(!truncated, "정확히 상한이면 잃은 항목이 없다");
    }

    #[test]
    fn merge_status_rows는_상한을_하나_넘으면_잘린다() {
        let mut porcelain = String::new();
        for i in 0..(MAX_PANEL_FILES + 1) {
            porcelain.push_str(&format!(" M f{i}.rs\0"));
        }
        let (rows, truncated) = merge_status_rows(&porcelain, "");
        assert_eq!(rows.len(), MAX_PANEL_FILES);
        assert!(truncated, "하나라도 넘치면 잘림 표시가 있어야 한다");
    }

    #[test]
    fn merge_committed_rows는_상한에_정확히_닿으면_잘림이_아니다() {
        let mut name_status = String::new();
        for i in 0..MAX_PANEL_FILES {
            name_status.push_str(&format!("M\tf{i}.rs\n"));
        }
        let (rows, truncated) = merge_committed_rows("", &name_status);
        assert_eq!(rows.len(), MAX_PANEL_FILES);
        assert!(!truncated, "정확히 상한이면 잃은 항목이 없다");
    }

    #[test]
    fn merge_committed_rows는_상한을_하나_넘으면_잘린다() {
        let mut name_status = String::new();
        for i in 0..(MAX_PANEL_FILES + 1) {
            name_status.push_str(&format!("M\tf{i}.rs\n"));
        }
        let (rows, truncated) = merge_committed_rows("", &name_status);
        assert_eq!(rows.len(), MAX_PANEL_FILES);
        assert!(truncated, "하나라도 넘치면 잘림 표시가 있어야 한다");
    }

    #[test]
    fn ahead_behind는_left_right_count_출력을_읽는다() {
        assert_eq!(parse_ahead_behind("73\t4\n"), Some((4, 73)));
        assert_eq!(parse_ahead_behind("0\t0"), Some((0, 0)));
        assert_eq!(parse_ahead_behind("garbage"), None);
    }

    #[test]
    fn 파일명과_디렉터리를_분리한다() {
        assert_eq!(
            split_row_path("crates/app/src/app.rs"),
            ("app.rs", "crates/app/src")
        );
        assert_eq!(split_row_path("Cargo.toml"), ("Cargo.toml", ""));
    }

    #[test]
    fn short_dir는_끝_두_단계만_남기고_앞을_줄인다() {
        assert_eq!(short_dir("crates/app/src/ui"), "…/src/ui");
        assert_eq!(short_dir("crates/app"), "crates/app");
        assert_eq!(short_dir("src"), "src");
        assert_eq!(short_dir(""), "");
    }

    // 중요 리뷰 지적 4번 회귀 — ahead/behind·+/− 색은 designall::tokens에서만
    // 고른 값이라 라이트/다크 둘 다 자동으로 성립해야 한다(transcript_viewer.rs
    // 역할별_배경_테스트와 같은 관례). agent_visuals::status_color는 dark_mode
    // 분기가 없어 라이트 테마에서 대비가 낮았다.
    #[test]
    fn stat_color는_ahead_added는_성공색_behind_dels는_에러색이다() {
        for tokens in [crate::ui::designall::DARK, crate::ui::designall::LIGHT] {
            assert_eq!(stat_color(tokens, true), tokens.success);
            assert_eq!(stat_color(tokens, false), tokens.error);
            assert_ne!(
                tokens.success, tokens.error,
                "성공/에러 색은 서로 달라야 한다"
            );
            assert_ne!(
                tokens.success, tokens.app_background,
                "성공색은 배경과 구분돼야 한다"
            );
            assert_ne!(
                tokens.error, tokens.app_background,
                "에러색은 배경과 구분돼야 한다"
            );
        }
        assert_ne!(
            crate::ui::designall::DARK.success,
            crate::ui::designall::LIGHT.success,
            "다크/라이트 팔레트는 서로 달라야 한다"
        );
        assert_ne!(
            crate::ui::designall::DARK.error,
            crate::ui::designall::LIGHT.error,
            "다크/라이트 팔레트는 서로 달라야 한다"
        );
    }

    #[test]
    fn 상태_배지는_porcelain_문자를_색과_설명_키로_바꾼다() {
        let tokens = crate::ui::designall::DARK;
        assert_eq!(
            status_badge_style('M', tokens),
            ('M', tokens.warning, "git.status.modified")
        );
        assert_eq!(
            status_badge_style('A', tokens),
            ('A', tokens.success, "git.status.added")
        );
        assert_eq!(
            status_badge_style('D', tokens),
            ('D', tokens.error, "git.status.deleted")
        );
        assert_eq!(
            status_badge_style('R', tokens),
            ('R', tokens.accent, "git.status.renamed")
        );
        assert_eq!(
            status_badge_style('C', tokens),
            ('C', tokens.accent, "git.status.copied")
        );
        assert_eq!(
            status_badge_style('U', tokens),
            ('U', tokens.error, "git.status.conflict")
        );
        // untracked는 `?` 대신 「새 파일」의 N으로 보인다.
        assert_eq!(
            status_badge_style('?', tokens),
            ('N', tokens.muted_text, "git.status.untracked")
        );
        assert_eq!(
            status_badge_style('T', tokens),
            ('T', tokens.muted_text, "git.status.other")
        );
    }

    // `git worktree list --porcelain -z` 실물 출력(2026-08-16, `xxd`로 실측)과 같은
    // 형식 — 필드가 NUL로 끝나고, 빈 필드(연속 NUL)가 항목 경계다.
    const WORKTREE_PORCELAIN: &str = "worktree /repo\0HEAD 1111111111111111111111111111111111111111\0branch refs/heads/main\0\0worktree /repo/.deppy/worktrees/alpha\0HEAD 2222222222222222222222222222222222222222\0branch refs/heads/deppy/alpha\0\0worktree /repo/detached\0HEAD 3333333333333333333333333333333333333333\0detached\0\0worktree /repo/bare\0bare\0\0";

    #[test]
    fn 워크트리_목록은_브랜치와_현재를_구분한다() {
        let (rows, truncated) = parse_worktree_list(
            WORKTREE_PORCELAIN,
            Path::new("/repo/.deppy/worktrees/alpha"),
        );
        assert!(!truncated);
        // bare 항목은 체크아웃이 없어 셸을 열 수 없다 — 목록에서 뺀다.
        assert_eq!(rows.len(), 3, "bare는 제외한다: {rows:?}");
        assert_eq!(rows[0].name, "repo");
        assert_eq!(rows[0].branch.as_deref(), Some("main"));
        assert!(!rows[0].current);
        assert_eq!(rows[1].name, "alpha");
        assert_eq!(rows[1].branch.as_deref(), Some("deppy/alpha"));
        assert!(rows[1].current, "repo_root와 같은 경로가 현재 워크트리다");
        assert_eq!(rows[2].branch, None, "detached는 브랜치가 없다");
    }

    #[test]
    fn 워크트리_목록은_상한을_하나_넘으면_잘린다() {
        let mut porcelain = String::new();
        for index in 0..(MAX_WORKTREE_ROWS + 5) {
            porcelain.push_str(&format!(
                "worktree /repo/w{index}\0HEAD {index:040}\0branch refs/heads/b{index}\0\0"
            ));
        }
        let (rows, truncated) = parse_worktree_list(&porcelain, Path::new("/repo"));
        assert_eq!(rows.len(), MAX_WORKTREE_ROWS);
        assert!(truncated, "5개 넘치면 잘림 표시가 있어야 한다");
    }

    // 중요 리뷰 지적 3번 회귀 — 딱 상한만큼이면 아무것도 안 잃었으니 잘림이 아니다.
    #[test]
    fn 워크트리_목록은_상한에_정확히_닿으면_잘림이_아니다() {
        let mut porcelain = String::new();
        for index in 0..MAX_WORKTREE_ROWS {
            porcelain.push_str(&format!(
                "worktree /repo/w{index}\0HEAD {index:040}\0branch refs/heads/b{index}\0\0"
            ));
        }
        let (rows, truncated) = parse_worktree_list(&porcelain, Path::new("/repo"));
        assert_eq!(rows.len(), MAX_WORKTREE_ROWS);
        assert!(!truncated, "정확히 상한이면 잃은 항목이 없다");
    }

    #[test]
    fn 워크트리_잠금_줄은_무시한다() {
        let porcelain =
            "worktree /repo\0HEAD 1111\0branch refs/heads/main\0locked\0prunable gone\0\0";
        let (rows, truncated) = parse_worktree_list(porcelain, Path::new("/repo"));
        assert!(!truncated);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].branch.as_deref(), Some("main"));
    }

    // 중요 리뷰 지적 2번 회귀 — trim()이 경로 끝 공백까지 지워 존재하지 않는
    // 경로를 만들었다. trim을 없앤 뒤에도 실제로 보존되는지 고정한다.
    #[test]
    fn 워크트리_경로_끝_공백은_보존된다() {
        let porcelain = "worktree /repo/trailing \0HEAD 1111\0branch refs/heads/main\0\0";
        let (rows, _truncated) = parse_worktree_list(porcelain, Path::new("/repo"));
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].path, "/repo/trailing ",
            "경로 끝 공백이 지워지면 안 된다"
        );
    }

    // 중요 리뷰 지적 2번 회귀 — `-z` 없이는 `.lines()`가 경로 내부 개행에서 항목을
    // 둘로 쪼갰다. git-worktree(1): "-z ... makes it possible to parse the output
    // when the entry contains a newline". NUL 구분이라 개행이 있어도 필드 하나로 남는다.
    #[test]
    fn 워크트리_경로에_개행이_있어도_안_잘린다() {
        let porcelain = "worktree /repo/weird\nname\0HEAD 1111\0branch refs/heads/main\0\0";
        let (rows, _truncated) = parse_worktree_list(porcelain, Path::new("/repo"));
        assert_eq!(
            rows.len(),
            1,
            "개행이 있어도 항목 하나로 남아야 한다: {rows:?}"
        );
        assert_eq!(rows[0].path, "/repo/weird\nname");
    }

    use std::time::Duration;
    const T: Duration = Duration::from_secs(10);

    fn temp_repo(tag: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "deppy-gitpanel-{tag}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        crate::git_cli::run_git(&dir, &["init", "-q", "-b", "main"], T).unwrap();
        crate::git_cli::run_git(&dir, &["config", "user.email", "t@t"], T).unwrap();
        crate::git_cli::run_git(&dir, &["config", "user.name", "t"], T).unwrap();
        dir
    }

    fn commit_all(repo: &std::path::Path, msg: &str) {
        crate::git_cli::run_git(repo, &["add", "-A"], T).unwrap();
        crate::git_cli::run_git(repo, &["commit", "-q", "-m", msg], T).unwrap();
    }

    #[test]
    fn collect_snapshot은_브랜치와_변경_목록을_수집한다() {
        let repo = temp_repo("snap");
        std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
        commit_all(&repo, "base");
        // 워킹트리 변경 1 + untracked 1
        std::fs::write(repo.join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        std::fs::write(repo.join("new.txt"), "hi\n").unwrap();

        let snap = collect_snapshot(&repo).expect("collect");
        assert_eq!(snap.branch, "main");
        // upstream이 없는 로컬 repo: committed 섹션은 비고 ahead/behind는 0.
        assert_eq!(snap.upstream, None);
        assert_eq!(snap.committed.len(), 0);
        let paths: Vec<&str> = snap.changes.iter().map(|r| r.rel_path.as_str()).collect();
        assert!(paths.contains(&"a.rs") && paths.contains(&"new.txt"));
        let a = snap.changes.iter().find(|r| r.rel_path == "a.rs").unwrap();
        assert_eq!((a.status, a.adds), ('M', Some(1)));
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn collect_snapshot은_upstream이_있으면_committed와_ahead_behind를_채운다() {
        // "원격"을 흉내내는 로컬 클론: origin = 다른 로컬 repo.
        let origin = temp_repo("origin");
        std::fs::write(origin.join("f.rs"), "one\n").unwrap();
        commit_all(&origin, "c1");
        let clone_dir = std::env::temp_dir().join(format!(
            "deppy-gitpanel-clone-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        crate::git_cli::run_git(
            origin.parent().unwrap(),
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                clone_dir.to_str().unwrap(),
            ],
            T,
        )
        .unwrap();
        crate::git_cli::run_git(&clone_dir, &["config", "user.email", "t@t"], T).unwrap();
        crate::git_cli::run_git(&clone_dir, &["config", "user.name", "t"], T).unwrap();
        // 로컬 커밋 1개 → ahead=1, behind=0, committed에 f.rs.
        std::fs::write(clone_dir.join("f.rs"), "one\ntwo\n").unwrap();
        commit_all(&clone_dir, "local work");

        let snap = collect_snapshot(&clone_dir).expect("collect");
        assert!(snap.upstream.as_deref().unwrap_or("").contains("origin/"));
        assert_eq!((snap.ahead, snap.behind), (1, 0));
        assert_eq!(snap.committed.len(), 1);
        assert_eq!(snap.committed[0].rel_path, "f.rs");
        // origin이 로컬 경로(비GitHub)이므로 remote_https_base는 None이어야 한다.
        assert_eq!(snap.remote_https_base, None);
        std::fs::remove_dir_all(&origin).ok();
        std::fs::remove_dir_all(&clone_dir).ok();
    }

    // ── 브랜치 커밋 목록(2026-10-11 PR 4) ─────────────────────────────

    /// `git log -z --format=%h%x00%s%x00%ct` 한 항목 — 필드도 항목도 NUL로 끝난다(실측).
    fn log_record(sha: &str, subject: &str, time: i64) -> String {
        format!("{sha}\0{subject}\0{time}\0")
    }

    #[test]
    fn 커밋_로그_파서는_빈_출력이면_빈_목록이다() {
        assert_eq!(parse_branch_commits(""), (Vec::new(), false));
    }

    #[test]
    fn 커밋_로그_파서는_상한에_정확히_닿으면_잘림이_아니다() {
        let log: String = (0..MAX_BRANCH_COMMITS)
            .map(|i| log_record(&format!("{i:07x}"), "s", 1))
            .collect();
        let (rows, truncated) = parse_branch_commits(&log);
        assert_eq!(rows.len(), MAX_BRANCH_COMMITS);
        assert!(!truncated, "정확히 상한이면 잃은 항목이 없다");
    }

    #[test]
    fn 커밋_로그_파서는_상한을_하나_넘으면_잘린다() {
        let log: String = (0..(MAX_BRANCH_COMMITS + 1))
            .map(|i| log_record(&format!("{i:07x}"), "s", 1))
            .collect();
        let (rows, truncated) = parse_branch_commits(&log);
        assert_eq!(rows.len(), MAX_BRANCH_COMMITS);
        assert!(truncated, "하나라도 넘치면 잘림 표시가 있어야 한다");
        // 순서는 git log 출력 순서(최신순) 그대로다 — 버리는 쪽은 가장 오래된 커밋이다.
        assert_eq!(rows[0].sha.as_str(), "0000000");
    }

    #[test]
    fn 커밋_로그_파서는_탭과_유니코드_제목을_그대로_담는다() {
        let subject = "fix:\t탭 «인용» \"따옴표\" ✓ 🚀";
        let log = log_record("a1b2c3d", subject, 1_791_680_568) + &log_record("9f8e7d6", "", 7);
        let (rows, truncated) = parse_branch_commits(&log);
        assert!(!truncated);
        assert_eq!(
            rows,
            vec![
                GitCommitRow {
                    sha: CommitSha::parse("a1b2c3d").unwrap(),
                    subject: subject.to_owned(),
                    time: 1_791_680_568,
                },
                // 빈 제목(--allow-empty-message)도 필드 정렬을 깨지 않는다.
                GitCommitRow {
                    sha: CommitSha::parse("9f8e7d6").unwrap(),
                    subject: String::new(),
                    time: 7,
                },
            ]
        );
    }

    /// 바이트 상한(run_git_limited)에서 잘린 출력은 마지막 NUL 뒤 조각이 미완성이다 —
    /// 시각 "17"처럼 잘린 숫자를 온전한 값으로 읽으면 안 된다.
    #[test]
    fn 커밋_로그_파서는_nul로_끝나지_않은_조각을_버린다() {
        let log = log_record("a1b2c3d", "온전함", 100) + "9f8e7d6\x00잘린 제목\x0017";
        let (rows, _) = parse_branch_commits(&log);
        assert_eq!(rows.len(), 1, "미완성 항목은 버린다: {rows:?}");
        assert_eq!(rows[0].subject, "온전함");
    }

    #[test]
    fn 커밋_로그_파서는_sha나_시각이_손상된_항목을_건너뛴다() {
        let log = log_record("not-hex", "a", 1)
            + &log_record("a1b2c3d", "b", 2)
            + &log_record("9f8e7d6", "c", -1)
            + "1234567\0d\0x\0"; // 시각이 숫자가 아니다
        let (rows, truncated) = parse_branch_commits(&log);
        assert!(!truncated);
        let subjects: Vec<&str> = rows.iter().map(|r| r.subject.as_str()).collect();
        assert_eq!(subjects, vec!["b", "c"]);
    }

    #[test]
    fn commit_sha는_4자_이상_64자_이하_16진수만_받는다() {
        let sha = CommitSha::parse("a1B2c3d").expect("16진수");
        assert_eq!(sha.as_str(), "a1B2c3d");
        assert_eq!(format!("{sha:?}"), "a1B2c3d");
        let full = "f".repeat(64);
        assert_eq!(CommitSha::parse(&full).unwrap().as_str(), full);
        // git 인자로 넘기므로 옵션·경로로 읽힐 값은 여기서 막는다.
        for bad in [
            "",
            "abc",
            "-n1",
            "a1b2c3d ",
            "a1b2/c3d",
            "g123456",
            &"f".repeat(65),
        ] {
            assert_eq!(CommitSha::parse(bad), None, "{bad:?}");
        }
    }

    /// committed 파일 목록과 **같은 기준**(merge-base..HEAD)의 커밋만, 최신순으로 담는다.
    #[test]
    fn collect_snapshot은_기준_이후_커밋만_최신순으로_담는다() {
        let origin = temp_repo("commits-origin");
        std::fs::write(origin.join("f.rs"), "one\n").unwrap();
        commit_all(&origin, "기준 이전 커밋");
        let clone_dir = std::env::temp_dir().join(format!(
            "deppy-gitpanel-commits-clone-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        crate::git_cli::run_git(
            origin.parent().unwrap(),
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                clone_dir.to_str().unwrap(),
            ],
            T,
        )
        .unwrap();
        crate::git_cli::run_git(&clone_dir, &["config", "user.email", "t@t"], T).unwrap();
        crate::git_cli::run_git(&clone_dir, &["config", "user.name", "t"], T).unwrap();
        std::fs::write(clone_dir.join("f.rs"), "one\ntwo\n").unwrap();
        commit_all(&clone_dir, "첫 로컬\t작업");
        std::fs::write(clone_dir.join("g.rs"), "g\n").unwrap();
        commit_all(&clone_dir, "둘째 로컬 작업 ✓");

        let snap = collect_snapshot(&clone_dir).expect("collect");
        let subjects: Vec<&str> = snap.commits.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, vec!["둘째 로컬 작업 ✓", "첫 로컬\t작업"]);
        assert!(!snap.commits_truncated);
        let head = crate::git_cli::run_git(&clone_dir, &["rev-parse", "HEAD"], T).unwrap();
        assert!(
            head.starts_with(snap.commits[0].sha.as_str()),
            "첫 항목은 HEAD다: {head} / {:?}",
            snap.commits[0].sha
        );
        assert!(snap.commits.iter().all(|c| c.time > 0));
        std::fs::remove_dir_all(&origin).ok();
        std::fs::remove_dir_all(&clone_dir).ok();
    }

    /// 커밋 목록과 같은 형태(`%h`)의 HEAD sha.
    fn head_sha(repo: &std::path::Path) -> CommitSha {
        let out = crate::git_cli::run_git(repo, &["rev-parse", "--short", "HEAD"], T).unwrap();
        CommitSha::parse(out.trim()).unwrap()
    }

    fn file_row(path: &str, status: char, adds: u32, dels: u32) -> GitFileRow {
        GitFileRow {
            rel_path: path.into(),
            status,
            adds: Some(adds),
            dels: Some(dels),
        }
    }

    /// App이 쓰는 IO 진입점(execute_io)으로 본다 — 세대·sha를 그대로 돌려주고, 루트
    /// 커밋도 부모 없이 전량 추가로 보인다.
    #[test]
    fn 커밋_파일_io는_그_커밋이_바꾼_파일만_돌려준다() {
        let repo = temp_repo("commit-files");
        std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
        commit_all(&repo, "root");
        let root = head_sha(&repo);
        std::fs::write(repo.join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        std::fs::write(repo.join("docs/b.md"), "b\n").unwrap();
        commit_all(&repo, "second");
        let second = head_sha(&repo);
        // 커밋하지 않은 변경은 커밋 파일 목록에 섞이면 안 된다.
        std::fs::write(repo.join("a.rs"), "dirty\n").unwrap();

        let files = |sha: CommitSha| match execute_io(GitPanelIoIntent {
            generation: 9,
            cwd: repo.clone(),
            request: GitPanelIoRequest::CommitFiles { sha },
        }) {
            GitPanelIoCompletion {
                generation: 9,
                result: GitPanelIoResult::CommitFiles { sha: got, result },
            } => {
                assert_eq!(got, sha);
                result.expect("collect")
            }
            other => panic!("커밋 파일 결과여야 한다: {other:?}"),
        };
        assert_eq!(
            files(second),
            GitCommitFiles {
                rows: vec![
                    file_row("a.rs", 'M', 1, 0),
                    file_row("docs/b.md", 'A', 1, 0)
                ],
                truncated: false,
            }
        );
        assert_eq!(
            files(root).rows,
            vec![file_row("a.rs", 'A', 1, 0)],
            "루트 커밋은 빈 트리 대비 전량 추가다"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn collect_file_diff는_커밋_모드에서_그_커밋의_변경만_읽는다() {
        use crate::ui::diff_viewer::{DiffMode, LineKind};
        let repo = temp_repo("commit-diff");
        std::fs::write(repo.join("a.rs"), "one\n").unwrap();
        commit_all(&repo, "root");
        let root = head_sha(&repo);
        std::fs::write(repo.join("a.rs"), "one\ntwo\n").unwrap();
        commit_all(&repo, "second");
        let second = head_sha(&repo);
        std::fs::write(repo.join("a.rs"), "one\ntwo\nthree\n").unwrap();

        let lines = |sha: CommitSha| -> Vec<(LineKind, String)> {
            let view = collect_file_diff(&repo, "a.rs", DiffMode::Commit(sha)).expect("collect");
            assert!(!view.truncated && !view.binary);
            view.hunks
                .iter()
                .flat_map(|h| h.lines.iter())
                .map(|l| (l.kind, l.text.clone()))
                .collect()
        };
        assert_eq!(
            lines(second),
            vec![
                (LineKind::Context, "one".to_owned()),
                (LineKind::Add, "two".to_owned())
            ],
            "그 커밋의 변경만 — 워킹트리의 three는 섞이지 않는다"
        );
        assert_eq!(
            lines(root),
            vec![(LineKind::Add, "one".to_owned())],
            "루트 커밋은 부모가 없어도 전량 추가로 보인다"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    /// 머지 커밋은 첫 부모 대비로 본다. `git show` 기본값(--cc)은 충돌 없는 머지의
    /// name-status와 diff를 비워, 머지가 브랜치에 들여온 파일이 통째로 사라졌다
    /// (2026-10-11 실측).
    #[test]
    fn 커밋_수집은_머지_커밋을_첫_부모_대비로_본다() {
        use crate::ui::diff_viewer::{DiffMode, LineKind};
        let repo = temp_repo("commit-merge");
        std::fs::write(repo.join("f.rs"), "f\n").unwrap();
        commit_all(&repo, "root");
        crate::git_cli::run_git(&repo, &["checkout", "-q", "-b", "side"], T).unwrap();
        std::fs::write(repo.join("m.rs"), "m\n").unwrap();
        commit_all(&repo, "side");
        crate::git_cli::run_git(&repo, &["checkout", "-q", "main"], T).unwrap();
        std::fs::write(repo.join("f.rs"), "f\nmain\n").unwrap();
        commit_all(&repo, "main work");
        crate::git_cli::run_git(&repo, &["merge", "-q", "--no-edit", "side"], T).unwrap();
        let merge = head_sha(&repo);

        let files = collect_commit_files(&repo, merge).expect("collect");
        assert_eq!(files.rows, vec![file_row("m.rs", 'A', 1, 0)]);
        let view = collect_file_diff(&repo, "m.rs", DiffMode::Commit(merge)).expect("collect");
        let lines: Vec<(LineKind, &str)> = view
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .map(|l| (l.kind, l.text.as_str()))
            .collect();
        assert_eq!(lines, vec![(LineKind::Add, "m")]);
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn 커밋_펼침은_파일_목록이_없을_때만_로드를_요청한다() {
        let a = CommitSha::parse("a1b2c3d").unwrap();
        let b = CommitSha::parse("9f8e7d6").unwrap();
        let load = |sha| Some(GitPanelAction::LoadCommitFiles { sha });
        let mut panel = GitPanelUi::default();

        // 처음 펼침 → 로드 요청, 다시 클릭 → 접힘(요청 없음).
        assert_eq!(panel.commit_list.toggle(a), load(a));
        assert_eq!(panel.commit_list.toggle(a), None);
        assert_eq!(panel.commit_list.expanded, None);
        // 결과가 오기 전에 다시 펼치면 재요청한다 — App의 git IO는 latest-only라
        // 앞 요청의 완료가 버려졌을 수 있다(접었다 펴기가 곧 재시도).
        assert_eq!(panel.commit_list.toggle(a), load(a));
        panel.set_commit_files(a, Ok(GitCommitFiles::default()));
        // 불러온 뒤에는 접었다 펴도 다시 요청하지 않는다.
        assert_eq!(panel.commit_list.toggle(a), None);
        assert_eq!(panel.commit_list.toggle(a), None);
        assert_eq!(panel.commit_list.expanded, Some(a));
        // 다른 커밋을 펼치면 앞 커밋은 접히고(한 번에 하나) 새로 요청한다.
        assert_eq!(panel.commit_list.toggle(b), load(b));
        assert_eq!(panel.commit_list.expanded, Some(b));
        // 늦게 온 다른 커밋의 결과는 지금 기다리는 커밋 자리를 덮지 않는다.
        panel.set_commit_files(a, Ok(GitCommitFiles::default()));
        assert!(matches!(
            panel.commit_list.loaded,
            Some((sha, CommitFilesState::Loading)) if sha == b
        ));
        // 실패는 다시 펼칠 때 재시도한다.
        panel.set_commit_files(b, Err(GitPanelErrorCode::CollectionFailed));
        assert!(matches!(
            panel.commit_list.loaded,
            Some((sha, CommitFilesState::Failed)) if sha == b
        ));
        assert_eq!(panel.commit_list.toggle(b), None);
        assert_eq!(panel.commit_list.toggle(b), load(b));
    }

    #[test]
    fn kittest_커밋을_펼치면_파일_로드를_요청하고_파일_클릭은_커밋_diff를_연다() {
        use egui_kittest::kittest::Queryable;

        struct State {
            panel: GitPanelUi,
            action: Option<GitPanelAction>,
        }

        let sha = CommitSha::parse("a1b2c3d").unwrap();
        let mut panel = GitPanelUi::default();
        panel.set_snapshot(Ok(GitPanelSnapshot {
            branch: "feat".into(),
            upstream: Some("origin/main".into()),
            committed: vec![file_row("src/branch.rs", 'M', 1, 0)],
            commits: vec![GitCommitRow {
                sha,
                subject: "첫 커밋".into(),
                time: 0,
            }],
            ..Default::default()
        }));
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut State| {
                let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
                if let Some(action) = state.panel.render(ui, &catalog, "") {
                    state.action = Some(action);
                }
            },
            State {
                panel,
                action: None,
            },
        );
        harness.run();

        // 커밋 행 전체를 접근성 이름(sha + 제목)으로 겨냥한다 — 파일 행과 같은 패턴.
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "a1b2c3d 첫 커밋")
            .click();
        harness.run();
        assert_eq!(
            harness.state().action,
            Some(GitPanelAction::LoadCommitFiles { sha }),
            "처음 펼친 커밋은 파일 목록 로드를 요청해야 한다"
        );
        harness.get_by_label(&catalog.t("diff.loading", &[]));

        // 실패는 그 커밋 자리에 오류로 보이고, 접었다 다시 펴면 재요청한다.
        harness
            .state_mut()
            .panel
            .set_commit_files(sha, Err(GitPanelErrorCode::CollectionFailed));
        harness.run();
        harness.get_by_label(&catalog.t("git.commit.files_error", &[]));
        harness.state_mut().action = None;
        for _ in 0..2 {
            harness
                .get_by_role_and_label(egui::accesskit::Role::Button, "a1b2c3d 첫 커밋")
                .click();
            harness.run();
        }
        assert_eq!(
            harness.state().action,
            Some(GitPanelAction::LoadCommitFiles { sha })
        );

        harness.state_mut().panel.set_commit_files(
            sha,
            Ok(GitCommitFiles {
                rows: vec![file_row("src/commit_only.rs", 'A', 3, 0)],
                truncated: false,
            }),
        );
        harness.run();
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "src/commit_only.rs")
            .click();
        harness.run();
        assert_eq!(
            harness.state().action,
            Some(GitPanelAction::ShowFileDiff {
                rel_path: "src/commit_only.rs".to_owned(),
                mode: crate::ui::diff_viewer::DiffMode::Commit(sha),
            }),
            "커밋 파일 행 클릭은 그 커밋 기준 diff를 열어야 한다"
        );
        // 섹션 파일 행과 같은 file_row라 클릭한 커밋 파일이 선택으로 남는다.
        assert_eq!(
            harness.state().panel.selected,
            Some((
                "src/commit_only.rs".to_owned(),
                crate::ui::diff_viewer::DiffMode::Commit(sha)
            ))
        );
    }

    #[test]
    fn repo가_아니면_no_repo_오류다() {
        let dir =
            std::env::temp_dir().join(format!("deppy-gitpanel-norepo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            collect_snapshot(&dir).unwrap_err(),
            GitPanelErrorCode::NoRepo
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // 치명 리뷰 지적 1번 회귀 — 예전엔 untracked 파일을 std::fs::read로 전량
    // 메모리에 올린 뒤 MAX_LIST_BYTES로 잘랐다(읽기 자체가 무계). 지금은
    // diff_panel::append_untracked_section과 같은 `--no-index /dev/null` 유계
    // 실행기를 쓰고, 결과가 이미 unified diff라 parse_unified로 해석된다.
    #[test]
    fn collect_file_diff는_untracked_파일_내용을_git_no_index로_수집한다() {
        let repo = temp_repo("filediff-untracked");
        std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
        commit_all(&repo, "base");
        std::fs::write(repo.join("new.txt"), "hello\nworld\n").unwrap();

        let view = collect_file_diff(&repo, "new.txt", crate::ui::diff_viewer::DiffMode::Working)
            .expect("collect");
        assert!(!view.truncated);
        assert!(!view.binary);
        let lines: Vec<&str> = view
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(lines, vec!["hello", "world"]);
        assert!(
            view.hunks.iter().all(|h| h
                .lines
                .iter()
                .all(|l| matches!(l.kind, crate::ui::diff_viewer::LineKind::Add))),
            "untracked는 전량 추가로 보여야 한다"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn collect_file_diff는_거대_untracked_파일도_상한에서_잘린다() {
        // fs::read 시절엔 파일 전체(수백MB 가능)를 읽고 나서야 잘랐다 — 읽기
        // 자체가 무계였다. 지금은 run_git_limited가 MAX_LIST_BYTES에서 git
        // 프로세스를 죽이므로 수집 바이트 자체가 유계다.
        let repo = temp_repo("filediff-untracked-huge");
        std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
        commit_all(&repo, "base");
        let huge = "x".repeat(MAX_LIST_BYTES * 2);
        std::fs::write(repo.join("big.txt"), &huge).unwrap();

        let view = collect_file_diff(&repo, "big.txt", crate::ui::diff_viewer::DiffMode::Working)
            .expect("collect");
        assert!(view.truncated, "상한을 넘는 파일은 잘림 표시가 있어야 한다");
        let collected_len: usize = view
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .map(|l| l.text.len())
            .sum();
        assert!(
            collected_len < huge.len(),
            "전량이 아니라 상한만큼만 수집돼야 한다: {collected_len}"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn 스냅샷은_자기_워크트리를_현재로_표시한다() {
        let repo = temp_repo("worktree_self");
        std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
        commit_all(&repo, "base");

        let snap = collect_snapshot(&repo).expect("스냅샷");
        assert_eq!(snap.worktrees.len(), 1, "새 repo는 메인 워크트리 하나뿐");
        assert!(snap.worktrees[0].current);
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn normalize_github_remote_https는_git_접미사를_떼고_정규화한다() {
        assert_eq!(
            normalize_github_remote("https://github.com/rust-lang/rust.git"),
            Some("https://github.com/rust-lang/rust".to_string())
        );
        assert_eq!(
            normalize_github_remote("https://github.com/rust-lang/rust"),
            Some("https://github.com/rust-lang/rust".to_string())
        );
    }

    #[test]
    fn normalize_github_remote_ssh_형식도_https로_정규화한다() {
        assert_eq!(
            normalize_github_remote("git@github.com:rust-lang/rust.git"),
            Some("https://github.com/rust-lang/rust".to_string())
        );
        assert_eq!(
            normalize_github_remote("git@github.com:rust-lang/rust"),
            Some("https://github.com/rust-lang/rust".to_string())
        );
    }

    #[test]
    fn normalize_github_remote_비github_remote는_none이다() {
        assert_eq!(
            normalize_github_remote("https://gitlab.com/foo/bar.git"),
            None
        );
        assert_eq!(
            normalize_github_remote("git@bitbucket.org:foo/bar.git"),
            None
        );
        assert_eq!(
            normalize_github_remote("/Users/t/tmp/some-local-repo"),
            None
        );
    }

    // 계획서 원안은 harness 통신에 `ui.ctx().memory_mut(..).insert_temp`와
    // `get_by_label_contains`를 썼는데, 이 저장소의 기존 kittest 관례
    // (fleet.rs `건너뛰기는_다음_항목을_히어로로_올린다`, workspace.rs의 `new_ui_state`
    // 테스트들)는 렌더 결과를 State 구조체 필드에 담아 `harness.state()`로 읽는다.
    // 관례 쪽이 정답이라 그 패턴으로 다시 썼다(2026-08-15 조정).
    #[test]
    fn kittest_행_클릭은_show_file_diff_액션을_낸다() {
        use egui_kittest::kittest::Queryable;

        struct State {
            panel: GitPanelUi,
            action: Option<GitPanelAction>,
        }

        let mut panel = GitPanelUi::default();
        panel.set_snapshot(Ok(GitPanelSnapshot {
            branch: "main".into(),
            changes: vec![GitFileRow {
                rel_path: "src/a.rs".into(),
                status: 'M',
                adds: Some(3),
                dels: Some(1),
            }],
            ..Default::default()
        }));

        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut State| {
                let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
                if let Some(action) = state.panel.render(ui, &catalog, "") {
                    state.action = Some(action);
                }
            },
            State {
                panel,
                action: None,
            },
        );
        harness.run();
        // 자식 Label("a.rs") 자체를 클릭하면 부모 스코프로 이벤트가 새지 않는다
        // (egui hit-test는 포인터 아래 가장 안쪽 위젯을 고른다) — 그래서 행에
        // 명시적으로 심어 둔 accessible 이름(Role::Button + rel_path)으로 행 전체를
        // 겨냥한다. work_history.rs 카드 클릭 테스트와 같은 질의 방식(2026-08-15).
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "src/a.rs")
            .click();
        harness.run();

        assert_eq!(
            harness.state().action,
            Some(GitPanelAction::ShowFileDiff {
                rel_path: "src/a.rs".to_owned(),
                mode: crate::ui::diff_viewer::DiffMode::Working,
            }),
            "변경 사항 행 클릭은 Working 모드 ShowFileDiff를 내야 한다"
        );
    }

    #[test]
    fn kittest_클릭한_파일은_모드까지_구분해_선택으로_남는다() {
        use egui_kittest::kittest::Queryable;

        struct State {
            panel: GitPanelUi,
        }

        let row = |path: &str| GitFileRow {
            rel_path: path.into(),
            status: 'M',
            adds: Some(1),
            dels: Some(1),
        };
        let mut panel = GitPanelUi::default();
        panel.set_snapshot(Ok(GitPanelSnapshot {
            branch: "main".into(),
            changes: vec![row("src/a.rs"), row("src/b.rs")],
            committed: vec![row("src/a.rs")],
            ..Default::default()
        }));
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut State| {
                let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
                state.panel.render(ui, &catalog, "");
            },
            State { panel },
        );
        harness.run();

        harness
            .query_all_by_role_and_label(egui::accesskit::Role::Button, "src/b.rs")
            .next()
            .unwrap()
            .click();
        harness.run();
        assert_eq!(
            harness.state().panel.selected,
            Some((
                "src/b.rs".to_owned(),
                crate::ui::diff_viewer::DiffMode::Working
            ))
        );

        // 같은 경로라도 브랜치 커밋 섹션의 행은 다른 선택이다.
        harness
            .query_all_by_role_and_label(egui::accesskit::Role::Button, "src/a.rs")
            .nth(1)
            .unwrap()
            .click();
        harness.run();
        assert_eq!(
            harness.state().panel.selected,
            Some((
                "src/a.rs".to_owned(),
                crate::ui::diff_viewer::DiffMode::Branch
            ))
        );
    }

    #[test]
    fn kittest_워크트리_행_클릭은_셸_열기를_올린다() {
        use egui_kittest::kittest::Queryable;

        struct State {
            panel: GitPanelUi,
            action: Option<GitPanelAction>,
        }

        let mut panel = GitPanelUi::default();
        panel.set_snapshot(Ok(GitPanelSnapshot {
            branch: "main".into(),
            worktrees: vec![
                GitWorktreeRow {
                    path: "/repo".into(),
                    name: "repo".into(),
                    branch: Some("main".into()),
                    current: true,
                },
                GitWorktreeRow {
                    path: "/repo/wt".into(),
                    name: "wt".into(),
                    branch: Some("feat/x".into()),
                    current: false,
                },
            ],
            ..Default::default()
        }));

        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut State| {
                let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
                if let Some(action) = state.panel.render(ui, &catalog, "") {
                    state.action = Some(action);
                }
            },
            State {
                panel,
                action: None,
            },
        );
        harness.run();
        // 파일 행 테스트와 같은 질의 방식 — 접근성 이름(Role::Button + path)으로
        // 행 전체를 겨냥한다(자식 Label 클릭은 부모로 새지 않는다).
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "/repo/wt")
            .click();
        harness.run();

        assert_eq!(
            harness.state().action,
            Some(GitPanelAction::OpenWorktreeShell {
                path: "/repo/wt".to_owned()
            }),
            "워크트리 행 클릭은 OpenWorktreeShell을 내야 한다"
        );
    }

    #[test]
    fn 워크트리가_하나면_섹션을_숨긴다() {
        // 정보가 0인 섹션은 그리지 않는다(스펙 §8-4).
        let mut snap = GitPanelSnapshot {
            branch: "main".into(),
            ..Default::default()
        };
        snap.worktrees = vec![GitWorktreeRow {
            path: "/repo".into(),
            name: "repo".into(),
            branch: Some("main".into()),
            current: true,
        }];
        assert!(!worktree_section_visible(&snap));
        snap.worktrees.push(GitWorktreeRow {
            path: "/repo/wt".into(),
            name: "wt".into(),
            branch: None,
            current: false,
        });
        assert!(worktree_section_visible(&snap));
    }

    #[test]
    fn 상한을_넘는_경로는_클릭_대상이_아니다() {
        let long = "/".repeat(WORKTREE_PATH_MAX_BYTES + 1);
        assert!(!worktree_path_is_spawnable(&long));
        assert!(!worktree_path_is_spawnable("/repo/\0bad"));
        assert!(worktree_path_is_spawnable("/repo/wt"));
    }

    // ── Task 2: 좌측 목록 필터(2026-08-18 계획) ─────────────────────────

    fn filter_test_rows() -> Vec<GitFileRow> {
        vec![
            GitFileRow {
                rel_path: "src/app.rs".into(),
                status: 'M',
                adds: Some(1),
                dels: None,
            },
            GitFileRow {
                rel_path: "README.md".into(),
                status: 'M',
                adds: None,
                dels: Some(2),
            },
            GitFileRow {
                rel_path: "src/ui/git_panel.rs".into(),
                status: 'A',
                adds: Some(5),
                dels: None,
            },
        ]
    }

    #[test]
    fn filter_file_rows는_경로에_대소문자_무시로_걸리는_행만_남긴다() {
        let rows = filter_test_rows();
        let filtered = filter_file_rows(&rows, "SRC");
        let paths: Vec<&str> = filtered.iter().map(|r| r.rel_path.as_str()).collect();
        assert_eq!(paths, vec!["src/app.rs", "src/ui/git_panel.rs"]);
    }

    #[test]
    fn filter_file_rows는_빈_필터면_모든_행을_순서_그대로_남긴다() {
        let rows = filter_test_rows();
        let filtered = filter_file_rows(&rows, "");
        assert_eq!(filtered.len(), rows.len());
        assert!(filtered.iter().zip(rows.iter()).all(|(a, b)| *a == b));
    }

    #[test]
    fn filter_file_rows는_걸리는_행이_없으면_빈_목록이다() {
        let rows = filter_test_rows();
        assert!(filter_file_rows(&rows, "zzz-no-match").is_empty());
    }

    #[test]
    fn kittest_필터는_변경_사항_행과_헤더_개수를_거른_뒤_기준으로_보여준다() {
        use egui_kittest::kittest::Queryable;

        let mut panel = GitPanelUi::default();
        panel.set_snapshot(Ok(GitPanelSnapshot {
            branch: "main".into(),
            changes: vec![
                GitFileRow {
                    rel_path: "src/app.rs".into(),
                    status: 'M',
                    adds: Some(1),
                    dels: None,
                },
                GitFileRow {
                    rel_path: "README.md".into(),
                    status: 'M',
                    adds: None,
                    dels: Some(2),
                },
            ],
            ..Default::default()
        }));

        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, panel: &mut GitPanelUi| {
                let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
                panel.render(ui, &catalog, "readme");
            },
            panel,
        );
        harness.run();

        harness.get_by_label("README.md");
        assert!(
            harness.query_by_label("src/app.rs").is_none(),
            "필터에 걸리지 않는 행은 보이면 안 된다"
        );
        // 섹션 헤더 개수는 거른 뒤(1) 기준이어야 한다 — 원래 2행 중 1행만 남았다.
        harness.get_by_label("∨ Changes 1");
    }

    #[test]
    fn kittest_모두_보기_버튼은_거른_뒤_행_수_기준으로_나타난다() {
        use egui_kittest::kittest::Queryable;

        let mut rows: Vec<GitFileRow> = (0..(SECTION_COLLAPSED_ROWS + 1))
            .map(|i| GitFileRow {
                rel_path: format!("keep{i}.rs"),
                status: 'M',
                adds: Some(1),
                dels: None,
            })
            .collect();
        // 필터에 걸리는 행을 하나만 남겨 둔다 — 거르기 전(11) 기준이면 여전히
        // 「모두 보기」가 보일 상한(10)을 넘지만, 거른 뒤(1) 기준이면 안 보여야 한다.
        rows[0].rel_path = "only-match.rs".into();

        let mut panel = GitPanelUi::default();
        panel.set_snapshot(Ok(GitPanelSnapshot {
            branch: "main".into(),
            changes: rows,
            ..Default::default()
        }));

        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, panel: &mut GitPanelUi| {
                let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
                panel.render(ui, &catalog, "only-match");
            },
            panel,
        );
        harness.run();

        harness.get_by_label("only-match.rs");
        assert!(
            harness.query_by_label("Show all").is_none(),
            "거른 뒤 1행뿐이면 「모두 보기」가 보이면 안 된다"
        );
    }

    #[test]
    fn kittest_필터가_두_섹션_모두_걸러내면_안내_한_줄만_보인다() {
        use egui_kittest::kittest::Queryable;

        let mut panel = GitPanelUi::default();
        panel.set_snapshot(Ok(GitPanelSnapshot {
            branch: "main".into(),
            changes: vec![GitFileRow {
                rel_path: "src/a.rs".into(),
                status: 'M',
                adds: Some(1),
                dels: None,
            }],
            ..Default::default()
        }));

        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, panel: &mut GitPanelUi| {
                let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
                panel.render(ui, &catalog, "zzz-no-match");
            },
            panel,
        );
        harness.run();

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        harness.get_by_label(&catalog.t("search.no_match", &[]));
        assert!(
            harness.query_by_label("src/a.rs").is_none(),
            "걸러진 행은 보이면 안 된다"
        );
    }

    #[test]
    fn kittest_필터는_워크트리_행을_거르지_않는다() {
        use egui_kittest::kittest::Queryable;

        let mut panel = GitPanelUi::default();
        panel.set_snapshot(Ok(GitPanelSnapshot {
            branch: "main".into(),
            changes: vec![GitFileRow {
                rel_path: "src/a.rs".into(),
                status: 'M',
                adds: Some(1),
                dels: None,
            }],
            worktrees: vec![
                GitWorktreeRow {
                    path: "/repo".into(),
                    name: "repo".into(),
                    branch: Some("main".into()),
                    current: true,
                },
                GitWorktreeRow {
                    path: "/repo/wt".into(),
                    name: "wt".into(),
                    branch: Some("feat/x".into()),
                    current: false,
                },
            ],
            ..Default::default()
        }));

        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, panel: &mut GitPanelUi| {
                let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
                panel.render(ui, &catalog, "zzz-no-match");
            },
            panel,
        );
        harness.run();

        // 필터가 걸려도 워크트리 섹션은 필터하지 않는다(2026-08-18 스펙 §좌측 목록
        // 필터) — 경로 목록이라 검색 대상이 아니고, 통째로 사라지면 혼란스럽다.
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "/repo/wt")
            .click();
    }
}
