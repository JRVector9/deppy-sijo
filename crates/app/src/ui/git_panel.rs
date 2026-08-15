//! 사이드바 Git 탭 본문 — orca 스타일 읽기 전용 상태 패널 (2026-08-15 스펙:
//! docs/superpowers/specs/2026-08-15-git-panel-design.md).
//! leaf는 intent(GitPanelAction)만 반환하고 git 실행·뷰 전환은 App이 소유한다.

// Task 2·3은 데이터 모델·파서·수집만 구현한다. UI 렌더(Task 6)와 app.rs 배선
// (Task 10)이 아직 이 모듈을 소비하지 않아 전부 dead_code로 잡힌다 —
// agent_surface.rs:7-9와 같은 관례. 렌더/배선 태스크가 끝나면 이 allow를 제거한다.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

/// 섹션당 수집 상한 — diff_panel의 MAX_DIFF_FILES와 같은 값(512). 표시 기본값은
/// SECTION_COLLAPSED_ROWS이고 「모두 보기」로 펼친다.
pub const MAX_PANEL_FILES: usize = 512;
/// 접힘 상태에서 섹션당 보여주는 행 수 — orca 스크린샷 기준 한 화면 분량.
pub const SECTION_COLLAPSED_ROWS: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitFileRow {
    pub rel_path: String,
    /// porcelain XY 중 워킹트리(Y) 우선, Y가 공백이면 X. untracked(??)는 '?'.
    pub status: char,
    /// None = 바이너리 또는 untracked(numstat 없음).
    pub adds: Option<u32>,
    pub dels: Option<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct GitPanelSnapshot {
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
    })
}

/// 패널이 App에 요청하는 intent — leaf는 git도 뷰 전환도 직접 하지 않는다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitPanelAction {
    Refresh,
    /// upstream의 GitHub 브랜치 페이지 열기 — URL 구성은 App이 remote를 보고 한다.
    OpenRemoteBranch,
    ShowFileDiff { rel_path: String, mode: crate::ui::diff_viewer::DiffMode },
}

#[derive(Default)]
pub struct GitPanelUi {
    snapshot: Option<Result<GitPanelSnapshot, GitPanelErrorCode>>,
    loading: bool,
    changes_show_all: bool,
    committed_show_all: bool,
    changes_collapsed: bool,
    committed_collapsed: bool,
}

impl GitPanelUi {
    pub fn set_loading(&mut self) {
        self.loading = true;
    }

    pub fn set_snapshot(&mut self, result: Result<GitPanelSnapshot, GitPanelErrorCode>) {
        self.loading = false;
        self.snapshot = Some(result);
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
                return;
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
}
