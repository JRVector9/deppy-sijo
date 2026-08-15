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
}

/// `status --porcelain -z -uall` + `diff --numstat HEAD`를 경로로 병합한다.
/// porcelain 등장 순서를 유지한다(사용자가 보는 안정된 순서).
fn merge_status_rows(porcelain_z: &str, numstat: &str) -> Vec<GitFileRow> {
    let counts = parse_numstat(numstat);
    let mut rows = Vec::new();
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
        let (adds, dels) = counts.get(path).copied().unwrap_or((None, None));
        rows.push(GitFileRow { rel_path: path.to_owned(), status, adds, dels });
        if rows.len() >= MAX_PANEL_FILES {
            break;
        }
    }
    rows
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
/// name-status 등장 순서를 유지한다.
fn merge_committed_rows(numstat: &str, name_status: &str) -> Vec<GitFileRow> {
    let counts = parse_numstat(numstat);
    let mut rows = Vec::new();
    for line in name_status.lines() {
        let mut parts = line.splitn(2, '\t');
        let (Some(status), Some(path)) = (parts.next(), parts.next()) else {
            continue;
        };
        // rename 라인("R100\told\tnew")은 마지막 필드가 새 경로다.
        let path = path.rsplit('\t').next().unwrap_or(path);
        let status = status.chars().next().unwrap_or('M');
        let (adds, dels) = counts.get(path).copied().unwrap_or((None, None));
        rows.push(GitFileRow { rel_path: path.to_owned(), status, adds, dels });
        if rows.len() >= MAX_PANEL_FILES {
            break;
        }
    }
    rows
}

/// `git worktree list --porcelain` 파싱. `worktree <path>` 줄이 새 항목을 열고,
/// `branch refs/heads/<name>`이 브랜치, `detached`는 None, `bare`는 **버린다**
/// (체크아웃이 없어 셸을 열 수 없다). `locked`/`prunable` 줄은 무시한다.
fn parse_worktree_list(porcelain: &str, repo_root: &Path) -> Vec<GitWorktreeRow> {
    let mut rows: Vec<GitWorktreeRow> = Vec::new();
    let mut path: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut bare = false;
    let mut flush = |path: &mut Option<String>, branch: &mut Option<String>, bare: &mut bool| {
        let taken = path.take();
        let taken_branch = branch.take();
        let was_bare = std::mem::replace(bare, false);
        let Some(taken) = taken else { return };
        if was_bare || rows.len() >= MAX_WORKTREE_ROWS {
            return;
        }
        let as_path = Path::new(&taken);
        let name = as_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| taken.clone());
        let current = as_path == repo_root;
        rows.push(GitWorktreeRow { path: taken, name, branch: taken_branch, current });
    };
    for line in porcelain.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            flush(&mut path, &mut branch, &mut bare);
            path = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("branch refs/heads/") {
            branch = Some(rest.trim().to_owned());
        } else if line.trim() == "bare" {
            bare = true;
        }
    }
    flush(&mut path, &mut branch, &mut bare);
    rows
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
            committed = merge_committed_rows(&numstat, &names);
            committed_truncated = t1 || t2 || committed.len() >= MAX_PANEL_FILES;
        }
    }

    let (porcelain, t3) = run(&["status", "--porcelain", "-z", "-uall"])?;
    let (numstat, t4) = run(&["diff", "--no-ext-diff", "--numstat", "HEAD"])?;
    let changes = merge_status_rows(&porcelain, &numstat);
    let changes_truncated = t3 || t4 || changes.len() >= MAX_PANEL_FILES;

    // origin remote → GitHub HTTPS 정규화. 실패(원격 없음/비GitHub)해도 None만 담고
    // 스냅샷 전체는 죽이지 않는다(Task 10 Step 7 소급 요구, 2026-08-15).
    let remote_https_base = run(&["remote", "get-url", "origin"])
        .ok()
        .and_then(|(s, _)| normalize_github_remote(s.trim()));

    // 워크트리 목록 — 스냅샷과 같은 IO 왕복에서 한 번만 부른다(스펙 §8-4).
    // 실패해도 스냅샷 전체를 죽이지 않는다(섹션 단위 오류 원칙, §6).
    let (worktrees, worktrees_truncated) = match run(&["worktree", "list", "--porcelain"]) {
        Ok((listing, truncated)) => {
            let rows = parse_worktree_list(&listing, &repo_root);
            let hit_cap = rows.len() >= MAX_WORKTREE_ROWS;
            (rows, truncated || hit_cap)
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
    })
}

/// App host 스레드에서 실행할 git 패널 IO. capacity-1 — App이 in-flight 1개만 유지
/// (기존 `pending_app_host_action` 큐 규칙, 2026-08-15 Task 10).
#[derive(Debug)]
pub enum GitPanelIoRequest {
    Snapshot,
    /// Git 보조 본문의 파일 행 클릭이 요청한다 — 그 배선은 Task 6이 한다(2026-08-15
    /// 2차, 스펙 §8-3). 그 전까지는 아무도 만들지 않아 dead_code를 허용한다.
    #[allow(dead_code)]
    FileDiff {
        rel_path: String,
        mode: crate::ui::diff_viewer::DiffMode,
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
}

#[derive(Debug)]
pub struct GitPanelIoCompletion {
    pub generation: u64,
    pub result: GitPanelIoResult,
}

/// host 스레드 실행 — collect_snapshot 또는 파일 diff 수집(스펙 §3).
pub fn execute_io(intent: GitPanelIoIntent) -> GitPanelIoCompletion {
    let result = match &intent.request {
        GitPanelIoRequest::Snapshot => GitPanelIoResult::Snapshot(collect_snapshot(&intent.cwd)),
        GitPanelIoRequest::FileDiff { rel_path, mode } => {
            GitPanelIoResult::FileDiff(collect_file_diff(&intent.cwd, rel_path, *mode))
        }
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
    use crate::ui::diff_viewer::{parse_unified, synth_added, DiffMode};
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
                // untracked — 파일 내용을 전량 추가로(유계: MAX_LIST_BYTES).
                let bytes = std::fs::read(repo_root.join(rel_path))
                    .map_err(|_| GitPanelErrorCode::CollectionFailed)?;
                let truncated = bytes.len() > MAX_LIST_BYTES;
                let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_LIST_BYTES)])
                    .into_owned();
                return Ok(synth_added(&text, truncated));
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
    }
}

/// 패널이 App에 요청하는 intent — leaf는 git도 뷰 전환도 직접 하지 않는다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitPanelAction {
    Refresh,
    /// upstream의 GitHub 브랜치 페이지 열기 — URL 구성은 App이 remote를 보고 한다.
    OpenRemoteBranch,
    ShowFileDiff { rel_path: String, mode: crate::ui::diff_viewer::DiffMode },
    /// 워크트리 행 클릭 — App이 그 경로에서 새 셸을 연다(워크트리를 만들지도 지우지도 않는다).
    OpenWorktreeShell { path: String },
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
}

impl GitPanelUi {
    pub fn set_loading(&mut self) {
        self.loading = true;
    }

    pub fn set_snapshot(&mut self, result: Result<GitPanelSnapshot, GitPanelErrorCode>) {
        self.loading = false;
        self.snapshot = Some(result);
    }

    /// ↗ 버튼이 쓸 (remote_https_base, branch) — 둘 다 있어야 Some. 스냅샷이 이미
    /// remote를 정규화해 담아 두므로 App이 IO 없이 즉시 URL을 구성할 수 있다
    /// (스펙 §4, Task 10 Step 7).
    pub fn remote_target(&self) -> Option<(String, String)> {
        let snap = self.snapshot.as_ref()?.as_ref().ok()?;
        let base = snap.remote_https_base.clone()?;
        Some((base, snap.branch.clone()))
    }

    pub fn render(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog) -> Option<GitPanelAction> {
        // self.snapshot을 절제한다 — 아래에서 self.changes_collapsed 등을 동시에
        // mut borrow해야 해서 Option<Result<..>>를 들고 있는 채로는 borrow가 충돌한다.
        let snap = match self.snapshot.clone() {
            None => {
                ui.weak(catalog.t("diff.loading", &[]));
                return if self.loading { None } else { Some(GitPanelAction::Refresh) };
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

        let mut action = None;

        // ── 헤더: 브랜치 / → upstream ↑a ↓b ↗ ──────────────────────────
        ui.strong(&snap.branch);
        ui.horizontal(|ui| {
            match snap.upstream.as_deref() {
                Some(upstream) => {
                    ui.weak("→");
                    ui.monospace(upstream);
                    if snap.ahead > 0 {
                        ui.colored_label(
                            crate::ui::agent_visuals::status_color(
                                crate::agent_surface::AgentVisualState::Complete,
                            ),
                            format!("↑{}", snap.ahead),
                        );
                    }
                    if snap.behind > 0 {
                        ui.colored_label(
                            crate::ui::agent_visuals::status_color(
                                crate::agent_surface::AgentVisualState::Error,
                            ),
                            format!("↓{}", snap.behind),
                        );
                    }
                    // remote가 GitHub일 때만 보인다(스펙 §4 숨김 조건). 계획서 원안은
                    // upstream만으로 항상 그렸는데, collect_snapshot이 이미
                    // remote_https_base로 이 조건을 계산해 두므로 그걸 쓴다
                    // (2026-08-15, Task 6 조정 — 타입 계약은 그대로, 렌더 조건만 보강).
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
            if ui.small_button("⟳").on_hover_text(catalog.t("diff.refresh", &[])).clicked() {
                action = Some(GitPanelAction::Refresh);
            }
        });
        ui.separator();

        // ── 섹션 2개 ─────────────────────────────────────────────────
        let section = |ui: &mut egui::Ui,
                        title_key: &str,
                        rows: &[GitFileRow],
                        collapsed: &mut bool,
                        show_all: &mut bool,
                        mode: crate::ui::diff_viewer::DiffMode,
                        action: &mut Option<GitPanelAction>| {
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
            let visible = if *show_all { rows.len() } else { rows.len().min(SECTION_COLLAPSED_ROWS) };
            for row in &rows[..visible] {
                let (name, dir) = split_row_path(&row.rel_path);
                // 계획서 원안은 `ui.horizontal(..).response.interact(Sense::click())`로
                // 행 전체를 클릭 가능하게 했는데, kittest로 돌려보면 자식 Label의
                // 텍스트 자체를 클릭했을 때 부모로 새지 않는다 — egui는 포인터 아래
                // "가장 안쪽" 위젯을 hit-test로 고르고, 그 위젯이 Sense::hover뿐이라도
                // 부모로 폴백하지 않는다(빈 여백을 클릭하면 잡히는 것으로 실측
                // 확인). 이 저장소는 이미 같은 문제를 겪었고(work_history.rs 카드/
                // 헤더 토글, 커밋 0e934c0) `scope_builder(UiBuilder::sense(click))` +
                // 명시적 `widget_info(Role::Button)`로 스코프 자체를 하나의 논리
                // 위젯으로 만드는 패턴을 쓴다 — 그 관례로 맞춘다
                // (2026-08-15, Task 6 Step 3 조정).
                let toggle = ui.scope_builder(
                    egui::UiBuilder::new()
                        .id_salt(("git-panel-row", title_key, row.rel_path.as_str()))
                        .sense(egui::Sense::click()),
                    |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(name);
                            if !dir.is_empty() {
                                ui.weak(dir);
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.monospace(row.status.to_string());
                                if let Some(d) = row.dels.filter(|d| *d > 0) {
                                    ui.colored_label(
                                        crate::ui::agent_visuals::status_color(
                                            crate::agent_surface::AgentVisualState::Error,
                                        ),
                                        format!("−{d}"),
                                    );
                                }
                                if let Some(a) = row.adds.filter(|a| *a > 0) {
                                    ui.colored_label(
                                        crate::ui::agent_visuals::status_color(
                                            crate::agent_surface::AgentVisualState::Complete,
                                        ),
                                        format!("+{a}"),
                                    );
                                }
                            });
                        });
                    },
                );
                let response = toggle.response.on_hover_cursor(egui::CursorIcon::PointingHand);
                // 접근성 이름을 rel_path로 명시한다 — kittest가 Role::Button + 이
                // 라벨로 행 전체(자식 Label이 아니라)를 정확히 겨냥할 수 있다.
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Button,
                        ui.is_enabled(),
                        row.rel_path.as_str(),
                    )
                });
                if response.clicked() {
                    *action = Some(GitPanelAction::ShowFileDiff { rel_path: row.rel_path.clone(), mode });
                }
            }
        };

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            if snap.changes.is_empty() && snap.committed.is_empty() {
                ui.weak(catalog.t("diff.clean", &[]));
                // 워크트리 섹션에는 clean과 무관하게 정보가 있을 수 있다 — 여기서
                // 돌아가면 그 섹션까지 감춘다(스펙 §8-4).
                if !worktree_section_visible(&snap) {
                    return;
                }
            }
            section(
                ui,
                "git.section.changes",
                &snap.changes,
                &mut self.changes_collapsed,
                &mut self.changes_show_all,
                crate::ui::diff_viewer::DiffMode::Working,
                &mut action,
            );
            if snap.changes_truncated {
                ui.weak(catalog.t("diff.truncated", &[]));
            }
            ui.add_space(6.0);
            section(
                ui,
                "git.section.committed",
                &snap.committed,
                &mut self.committed_collapsed,
                &mut self.committed_show_all,
                crate::ui::diff_viewer::DiffMode::Branch,
                &mut action,
            );
            if snap.committed_truncated {
                ui.weak(catalog.t("diff.truncated", &[]));
            }

            // ── 섹션 3: 워크트리 ─────────────────────────────────────────
            if worktree_section_visible(&snap) {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let arrow = if self.worktrees_collapsed { "›" } else { "∨" };
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
                        self.worktrees_collapsed = !self.worktrees_collapsed;
                    }
                });
                if !self.worktrees_collapsed {
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
                                        None => ui.weak(catalog.t("git.worktree.detached", &[])),
                                    };
                                    if row.current {
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| ui.weak(catalog.t("git.worktree.current", &[])),
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
                            action = Some(GitPanelAction::OpenWorktreeShell { path: row.path.clone() });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain과_numstat을_경로로_병합한다() {
        // porcelain -z: "XY path\0" 반복. rename은 "R  new\0old\0".
        let porcelain = " M a.rs\0?? new.txt\0R  moved.rs\0old.rs\0MM both.rs\0";
        let numstat = "3\t1\ta.rs\n456\t221\tmoved.rs\n-\t-\tbin.png\n2\t0\tboth.rs\n";
        let rows = merge_status_rows(porcelain, numstat);
        // 순서는 porcelain 등장 순서를 유지한다.
        assert_eq!(rows.len(), 4);
        assert_eq!(
            (rows[0].rel_path.as_str(), rows[0].status, rows[0].adds, rows[0].dels),
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
        assert_eq!((rows[3].rel_path.as_str(), rows[3].status), ("both.rs", 'M'));
    }

    #[test]
    fn numstat만_있는_경로는_committed_파서가_그대로_담는다() {
        // committed 섹션: numstat + name-status 병합. 바이너리는 "-\t-".
        let numstat = "12\t13\tsrc/ui/workspace.rs\n-\t-\tassets/logo.png\n";
        let name_status = "M\tsrc/ui/workspace.rs\nA\tassets/logo.png\n";
        let rows = merge_committed_rows(numstat, name_status);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (rows[0].rel_path.as_str(), rows[0].status, rows[0].adds, rows[0].dels),
            ("src/ui/workspace.rs", 'M', Some(12), Some(13))
        );
        assert_eq!(
            (rows[1].rel_path.as_str(), rows[1].status, rows[1].adds, rows[1].dels),
            ("assets/logo.png", 'A', None, None)
        );
    }

    #[test]
    fn ahead_behind는_left_right_count_출력을_읽는다() {
        assert_eq!(parse_ahead_behind("73\t4\n"), Some((4, 73)));
        assert_eq!(parse_ahead_behind("0\t0"), Some((0, 0)));
        assert_eq!(parse_ahead_behind("garbage"), None);
    }

    #[test]
    fn 파일명과_디렉터리를_분리한다() {
        assert_eq!(split_row_path("crates/app/src/app.rs"), ("app.rs", "crates/app/src"));
        assert_eq!(split_row_path("Cargo.toml"), ("Cargo.toml", ""));
    }

    const WORKTREE_PORCELAIN: &str = "\
worktree /repo
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /repo/.deppy/worktrees/alpha
HEAD 2222222222222222222222222222222222222222
branch refs/heads/deppy/alpha

worktree /repo/detached
HEAD 3333333333333333333333333333333333333333
detached

worktree /repo/bare
bare
";

    #[test]
    fn 워크트리_목록은_브랜치와_현재를_구분한다() {
        let rows = parse_worktree_list(WORKTREE_PORCELAIN, Path::new("/repo/.deppy/worktrees/alpha"));
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
    fn 워크트리_목록은_상한에서_잘린다() {
        let mut porcelain = String::new();
        for index in 0..(MAX_WORKTREE_ROWS + 5) {
            porcelain.push_str(&format!(
                "worktree /repo/w{index}\nHEAD {index:040}\nbranch refs/heads/b{index}\n\n"
            ));
        }
        let rows = parse_worktree_list(&porcelain, Path::new("/repo"));
        assert_eq!(rows.len(), MAX_WORKTREE_ROWS);
    }

    #[test]
    fn 워크트리_잠금_줄은_무시한다() {
        let porcelain = "worktree /repo\nHEAD 1111\nbranch refs/heads/main\nlocked\nprunable gone\n";
        let rows = parse_worktree_list(porcelain, Path::new("/repo"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].branch.as_deref(), Some("main"));
    }

    use std::time::Duration;
    const T: Duration = Duration::from_secs(10);

    fn temp_repo(tag: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir()
            .join(format!("deppy-gitpanel-{tag}-{}-{nonce}", std::process::id()));
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
            "deppy-gitpanel-clone-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                .unwrap().as_nanos()));
        crate::git_cli::run_git(
            origin.parent().unwrap(),
            &["clone", "-q", origin.to_str().unwrap(), clone_dir.to_str().unwrap()],
            T,
        ).unwrap();
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

    #[test]
    fn repo가_아니면_no_repo_오류다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-gitpanel-norepo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(collect_snapshot(&dir).unwrap_err(), GitPanelErrorCode::NoRepo);
        std::fs::remove_dir_all(&dir).ok();
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
        assert_eq!(normalize_github_remote("https://gitlab.com/foo/bar.git"), None);
        assert_eq!(normalize_github_remote("git@bitbucket.org:foo/bar.git"), None);
        assert_eq!(normalize_github_remote("/Users/t/tmp/some-local-repo"), None);
    }

    // 계획서 원안은 harness 통신에 `ui.ctx().memory_mut(..).insert_temp`와
    // `get_by_label_contains`를 썼는데, 이 저장소의 기존 kittest 관례
    // (fleet.rs `건너뛰기는_다음_항목을_히어로로_올린다`, workspace.rs의 `new_ui_state`
    // 테스트들)는 렌더 결과를 State 구조체 필드에 담아 `harness.state()`로 읽는다.
    // 관례 쪽이 정답이라 그 패턴으로 다시 썼다(2026-08-15, Task 6 Step 1 조정).
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
                if let Some(action) = state.panel.render(ui, &catalog) {
                    state.action = Some(action);
                }
            },
            State { panel, action: None },
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
                if let Some(action) = state.panel.render(ui, &catalog) {
                    state.action = Some(action);
                }
            },
            State { panel, action: None },
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
            Some(GitPanelAction::OpenWorktreeShell { path: "/repo/wt".to_owned() }),
            "워크트리 행 클릭은 OpenWorktreeShell을 내야 한다"
        );
    }

    #[test]
    fn 워크트리가_하나면_섹션을_숨긴다() {
        // 정보가 0인 섹션은 그리지 않는다(스펙 §8-4).
        let mut snap = GitPanelSnapshot { branch: "main".into(), ..Default::default() };
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
}
