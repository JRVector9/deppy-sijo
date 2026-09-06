# Git 패널 2차 — pane 보조 탭 배치 + 워크트리 섹션 구현 계획

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 사이드바에 갇혀 좁던 git 패널을 이력 탭과 같은 **pane 보조 탭**으로 옮겨
「목록 + diff」를 나란히 보이게 하고, 워크트리 목록과 「그 워크트리에서 셸 열기」를 더한다.

**Architecture:** 스펙 §8(docs/superpowers/specs/2026-08-15-git-panel-design.md)이 계약이다.
`WorkspaceUi`의 보조 탭 슬롯을 1개 → 최대 2개(`PaneAuxTabKind::{History, Git}`)로 넓히고,
Git 탭 본문 안에서 `GitPanelUi`(좌 300pt)와 `DiffViewerUi`(우 나머지)를 함께 그린다.
진입점은 사이드바 도구 탭에서 **내비게이션 레일**로 옮긴다. leaf는 intent만 반환하고
git 실행·셸 스폰은 App이 소유한다(저장소 관례).

**Tech Stack:** Rust 2024, egui/eframe 0.35, kittest(egui 테스트 하네스), `git_cli::run_git_limited`.

**작업 위치:** 워크트리 `/Users/jr/Desktop/projects/deppy-git-panel` (브랜치 `feat/git-panel`).
메인 워크트리 `/Users/jr/Desktop/projects/deppy-sijo`는 **건드리지 않는다**(사용자가 그 빌드로
앱을 돌린다). 앱 실행·`pkill`·`scripts/dev-run.sh`·`cargo build --release`·`git push`·PR 생성은
서브에이전트가 하지 않는다 — 오케스트레이터/사용자만 한다.

---

## 파일 구조

| 파일 | 책임 | 이번 변경 |
| --- | --- | --- |
| `crates/app/src/ui/git_panel.rs` | git 상태 수집·패널 렌더(leaf) | 워크트리 파싱·수집·섹션 렌더·`OpenWorktreeShell` intent |
| `crates/app/src/ui/workspace.rs` | pane 헤더·보조 탭 기구 | `PaneAuxTabKind`/`PaneAuxTabState`, 탭 목록화, 기하 루프 |
| `crates/app/src/ui/work_history.rs` | 작업 이력 탭 | `WorkHistoryTabState` 제거(workspace로 이관) |
| `crates/app/src/ui/file_tree.rs` | 사이드바(레일 + 도구 탭) | `SidebarTool::Git` 제거, 레일 Git 행 추가 |
| `crates/app/src/ui/diff_viewer.rs` | diff 뷰어 | 「터미널로」 버튼·액션 제거 |
| `crates/app/src/app.rs` | 상태 소유·IO·배선 | git 탭 상태·본문 렌더·워크트리 셸 스폰 |
| `crates/i18n/locales/*/messages.txt` | 문구 | 키 10개 추가, 2개 제거 (5로케일) |

---

### Task 1: 워크트리 파서와 수집

**Files:**
- Modify: `crates/app/src/ui/git_panel.rs`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`crates/app/src/ui/git_panel.rs`의 `#[cfg(test)] mod tests` 안에 추가한다.

```rust
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
```

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test -p deppy-sijo --lib ui::git_panel 2>&1 | tail -20`
Expected: FAIL — `cannot find function parse_worktree_list` / `MAX_WORKTREE_ROWS`

- [ ] **Step 3: 최소 구현**

`git_panel.rs` 상단 상수 옆에 추가한다.

```rust
/// 워크트리 목록 상한 — 목록은 사람이 훑는 것이라 32면 충분하고, 초과분은 잘림 표시만 한다.
pub const MAX_WORKTREE_ROWS: usize = 32;
```

`GitFileRow` 아래에 모델을 추가한다.

```rust
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
```

`GitPanelSnapshot`에 필드 두 개를 더한다(기존 필드 뒤).

```rust
    pub worktrees: Vec<GitWorktreeRow>,
    pub worktrees_truncated: bool,
```

파서를 `merge_committed_rows` 아래에 둔다.

```rust
/// `git worktree list --porcelain` 파싱. `worktree <path>` 줄이 새 항목을 열고,
/// `branch refs/heads/<name>`이 브랜치, `detached`는 None, `bare`는 **버린다**
/// (체크아웃이 없어 셸을 열 수 없다). `locked`/`prunable` 줄은 무시한다.
fn parse_worktree_list(porcelain: &str, repo_root: &Path) -> Vec<GitWorktreeRow> {
    let mut rows: Vec<GitWorktreeRow> = Vec::new();
    let mut path: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut bare = false;
    let mut flush = |path: &mut Option<String>, branch: &mut Option<String>, bare: &mut bool, rows: &mut Vec<GitWorktreeRow>| {
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
            flush(&mut path, &mut branch, &mut bare, &mut rows);
            path = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("branch refs/heads/") {
            branch = Some(rest.trim().to_owned());
        } else if line.trim() == "bare" {
            bare = true;
        }
    }
    flush(&mut path, &mut branch, &mut bare, &mut rows);
    rows
}
```

- [ ] **Step 4: 통과를 확인한다**

Run: `cargo test -p deppy-sijo --lib ui::git_panel 2>&1 | tail -20`
Expected: PASS (기존 git_panel 테스트도 전부 — `GitPanelSnapshot` 새 필드 때문에 구조체
리터럴이 깨지면 `..Default::default()`가 아니라 **명시적으로** 두 필드를 채워 고친다.
`GitPanelSnapshot`은 `Default` 파생이 있으므로 테스트 픽스처는 `..Default::default()`를 써도 된다.)

- [ ] **Step 5: 수집에 연결한다**

`collect_snapshot`의 `remote_https_base` 계산 **뒤**, `Ok(GitPanelSnapshot {` **앞**에 넣는다.

```rust
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
```

`Ok(GitPanelSnapshot { ... })`에 `worktrees,` 와 `worktrees_truncated,`를 추가한다.

- [ ] **Step 6: 통합 테스트를 더한다**

기존 `git init` 픽스처 테스트 옆에 추가한다(기존 테스트에서 임시 repo를 만드는 헬퍼
이름을 그대로 쓴다 — 파일 안에서 확인할 것).

```rust
    #[test]
    fn 스냅샷은_자기_워크트리를_현재로_표시한다() {
        let repo = 임시_repo_픽스처();   // 파일에 이미 있는 헬퍼 이름으로 바꿔 쓴다
        let snap = collect_snapshot(repo.path()).expect("스냅샷");
        assert_eq!(snap.worktrees.len(), 1, "새 repo는 메인 워크트리 하나뿐");
        assert!(snap.worktrees[0].current);
    }
```

- [ ] **Step 7: 게이트와 커밋**

Run: `cargo test -p deppy-sijo --lib ui::git_panel 2>&1 | tail -5`
Run: `cargo clippy -p deppy-sijo --all-targets -- -D warnings 2>&1 | tail -5`
Expected: 둘 다 통과, 경고 0

```bash
git add crates/app/src/ui/git_panel.rs
git commit -m "feat(app): git 패널 스냅샷에 워크트리 목록 추가"
```

---

### Task 2: 워크트리 섹션 렌더와 클릭 intent

**Files:**
- Modify: `crates/app/src/ui/git_panel.rs`

- [ ] **Step 1: 실패하는 kittest를 쓴다**

기존 kittest 관례(`egui_kittest::Harness::new_ui`, `Role::Button` + 라벨 겨냥)를 그대로 쓴다.
파일 안 기존 kittest 테스트를 먼저 읽고 하네스 만드는 형태를 복사할 것.

```rust
    #[test]
    fn kittest_워크트리_행_클릭은_셸_열기를_올린다() {
        let mut snap = GitPanelSnapshot { branch: "main".into(), ..Default::default() };
        snap.worktrees = vec![
            GitWorktreeRow { path: "/repo".into(), name: "repo".into(), branch: Some("main".into()), current: true },
            GitWorktreeRow { path: "/repo/wt".into(), name: "wt".into(), branch: Some("feat/x".into()), current: false },
        ];
        let mut ui = GitPanelUi::default();
        ui.set_snapshot(Ok(snap));
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut (GitPanelUi, Vec<GitPanelAction>)| {
                if let Some(action) = state.0.render(ui, &catalog()) {
                    state.1.push(action);
                }
            },
            (ui, Vec::new()),
        );
        harness.run();
        harness.get_by_role_and_label(egui::accesskit::Role::Button, "/repo/wt").click();
        harness.run();
        assert!(
            harness.state().1.contains(&GitPanelAction::OpenWorktreeShell { path: "/repo/wt".into() }),
            "행 클릭이 OpenWorktreeShell을 올려야 한다: {:?}",
            harness.state().1
        );
    }

    #[test]
    fn 워크트리가_하나면_섹션을_숨긴다() {
        // 정보가 0인 섹션은 그리지 않는다(스펙 §8-4).
        let mut snap = GitPanelSnapshot { branch: "main".into(), ..Default::default() };
        snap.worktrees = vec![GitWorktreeRow {
            path: "/repo".into(), name: "repo".into(), branch: Some("main".into()), current: true,
        }];
        assert!(!worktree_section_visible(&snap));
        snap.worktrees.push(GitWorktreeRow {
            path: "/repo/wt".into(), name: "wt".into(), branch: None, current: false,
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
```

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test -p deppy-sijo --lib ui::git_panel 2>&1 | tail -20`
Expected: FAIL — `OpenWorktreeShell` / `worktree_section_visible` / `worktree_path_is_spawnable` 없음

- [ ] **Step 3: 구현한다**

`GitPanelAction`에 변형을 추가한다.

```rust
    /// 워크트리 행 클릭 — App이 그 경로에서 새 셸을 연다(워크트리를 만들지도 지우지도 않는다).
    OpenWorktreeShell { path: String },
```

상수와 헬퍼를 파일 상단 상수 옆에 둔다.

```rust
/// 셸 스폰 경로 상한 — App의 `spawn_shell_at`이 같은 검사를 하지만 leaf도 넘기지 않는다.
pub const WORKTREE_PATH_MAX_BYTES: usize = 4096;

/// 셸을 열 수 있는 경로인가 — 빈 값·상한 초과·NUL은 클릭 intent를 만들지 않는다.
fn worktree_path_is_spawnable(path: &str) -> bool {
    !path.is_empty() && path.len() <= WORKTREE_PATH_MAX_BYTES && !path.as_bytes().contains(&0)
}

/// 워크트리가 메인 하나뿐이면 섹션을 그리지 않는다 — 정보가 0이다.
fn worktree_section_visible(snapshot: &GitPanelSnapshot) -> bool {
    snapshot.worktrees.len() > 1
}
```

`render`의 `ScrollArea` 안, `committed` 섹션과 `committed_truncated` 표시 **뒤**에 워크트리
섹션을 그린다. 행 렌더는 파일 행과 **같은 패턴**(`scope_builder(UiBuilder::sense(click))`
+ `widget_info(Role::Button, path)`)을 쓴다 — 자식 Label이 클릭을 삼키지 않게.

```rust
            if worktree_section_visible(&snap) {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let arrow = if self.worktrees_collapsed { "›" } else { "∨" };
                    if ui
                        .selectable_label(
                            false,
                            format!("{arrow} {} {}", catalog.t("git.section.worktrees", &[]), snap.worktrees.len()),
                        )
                        .clicked()
                    {
                        self.worktrees_collapsed = !self.worktrees_collapsed;
                    }
                });
                if !self.worktrees_collapsed {
                    for row in &snap.worktrees {
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
```

`GitPanelUi`에 `worktrees_collapsed: bool` 필드를 추가한다(`#[derive(Default)]`라 초기값 false = 펼침).

**중요:** `render`의 빈 상태 조기 반환
`if snap.changes.is_empty() && snap.committed.is_empty() { ui.weak(diff.clean); return; }`은
워크트리 섹션까지 감춘다. 조건을 `&& !worktree_section_visible(&snap)`로 좁히고,
`diff.clean`만 그린 뒤 **계속 진행**하도록 고친다.

- [ ] **Step 4: 통과를 확인한다**

Run: `cargo test -p deppy-sijo --lib ui::git_panel 2>&1 | tail -20`
Expected: PASS

- [ ] **Step 5: 게이트와 커밋**

Run: `cargo clippy -p deppy-sijo --all-targets -- -D warnings 2>&1 | tail -5`

```bash
git add crates/app/src/ui/git_panel.rs
git commit -m "feat(app): git 패널 워크트리 섹션과 셸 열기 intent"
```

> i18n 키(`git.section.worktrees` 등)는 Task 8에서 5로케일에 한 번에 넣는다. 그 전까지
> `catalog.t`는 키 이름을 그대로 돌려주므로 테스트는 통과한다.

---

### Task 3: 보조 탭 상태 기계를 workspace로 이관

**Files:**
- Modify: `crates/app/src/ui/work_history.rs` (제거)
- Modify: `crates/app/src/ui/workspace.rs` (추가)
- Modify: `crates/app/src/app.rs` (참조 2곳)

- [ ] **Step 1: 옮긴다**

`work_history.rs`의 `pub enum WorkHistoryTabState`와 `impl WorkHistoryTabState` 전체(약 90~136행)를
잘라 `workspace.rs`의 `PaneAuxTabIntent` 정의 **아래**에 붙이고 이름을 `PaneAuxTabState`로 바꾼다.
doc 주석의 "이력"을 "보조 탭"으로 일반화한다(전이 규칙은 **한 글자도 바꾸지 않는다**).

```rust
/// 보조 탭 하나의 표시 상태. 이력·Git이 각자 하나씩 갖는다.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PaneAuxTabState {
    #[default]
    Closed,
    OpenInactive,
    OpenActive,
}
```

- [ ] **Step 2: 참조를 고친다**

```bash
grep -rn "WorkHistoryTabState" crates/ --include='*.rs'
```

나오는 5곳을 `ui::workspace::PaneAuxTabState`로 바꾼다. `work_history.rs`의 상태 기계
테스트(`use WorkHistoryTabState::{...}`)도 `workspace.rs`로 함께 옮긴다.

- [ ] **Step 3: 통과를 확인한다**

Run: `cargo test -p deppy-sijo --lib 2>&1 | tail -10`
Expected: PASS (전이 테스트 포함)

- [ ] **Step 4: 커밋**

```bash
git add crates/app/src/ui/work_history.rs crates/app/src/ui/workspace.rs crates/app/src/app.rs
git commit -m "refactor(app): 보조 탭 상태 기계를 workspace로 이관"
```

---

### Task 4: 보조 탭 다중화 (최대 2개)

**Files:**
- Modify: `crates/app/src/ui/workspace.rs`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`workspace.rs` 테스트 모듈에 추가한다. 기존 기하 테스트(`pane_aux_tab_geometry`를 직접 부르는
7376/7418/7460/7672행 근처)를 먼저 읽고 헬퍼를 재사용한다.

```rust
    #[test]
    fn 보조_탭_두_개는_겹치지_않고_순서대로_놓인다() {
        let header = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 24.0));
        let close = egui::Rect::from_center_size(egui::pos2(120.0, 12.0), egui::vec2(20.0, 20.0));
        let placements = layout_aux_tabs(header, close, 700.0, &[
            aux_tab(PaneAuxTabKind::History, "이력", false),
            aux_tab(PaneAuxTabKind::Git, "Git", true),
        ], |_| 30.0);
        assert_eq!(placements.len(), 2);
        assert_eq!(placements[0].kind, PaneAuxTabKind::History);
        assert_eq!(placements[1].kind, PaneAuxTabKind::Git);
        assert!(
            placements[0].geometry.tab.right() <= placements[1].geometry.tab.left(),
            "두 탭이 겹친다: {:?}",
            placements.iter().map(|p| p.geometry.tab).collect::<Vec<_>>()
        );
        assert!(placements[1].geometry.tab.right() <= 700.0, "toolbar_left를 넘지 않는다");
    }

    #[test]
    fn 폭이_모자라면_뒤_탭부터_사라진다() {
        let header = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(220.0, 24.0));
        let close = egui::Rect::from_center_size(egui::pos2(120.0, 12.0), egui::vec2(20.0, 20.0));
        let placements = layout_aux_tabs(header, close, 200.0, &[
            aux_tab(PaneAuxTabKind::History, "이력", false),
            aux_tab(PaneAuxTabKind::Git, "Git", true),
        ], |_| 30.0);
        assert!(placements.len() < 2, "좁은 헤더에서 두 탭이 다 들어갔다: {placements:?}");
    }

    #[test]
    fn 탭이_세_개면_두_개만_남는다() {
        // 상한은 계약이다 — 헤더는 세션 제목이 우선이라 그 이상은 놓지 않는다.
        assert_eq!(PANE_AUX_TAB_MAX, 2);
    }
```

`aux_tab`은 테스트 헬퍼다:

```rust
    fn aux_tab(kind: PaneAuxTabKind, label: &str, active: bool) -> PaneAuxTab {
        PaneAuxTab { kind, label: label.to_owned(), active }
    }
```

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test -p deppy-sijo --lib ui::workspace 2>&1 | tail -20`
Expected: FAIL — `PaneAuxTabKind`/`layout_aux_tabs`/`PANE_AUX_TAB_MAX` 없음

- [ ] **Step 3: 타입을 넓힌다**

```rust
/// 보조 탭 종류 — 헤더에 붙는 순서이자 hover 문구 키의 근거다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneAuxTabKind {
    History,
    Git,
}

impl PaneAuxTabKind {
    fn hint_key(self) -> &'static str {
        match self {
            Self::History => "workspace.tab.history_hint",
            Self::Git => "workspace.tab.git_hint",
        }
    }

    fn close_key(self) -> &'static str {
        match self {
            Self::History => "workspace.tab.history_close",
            Self::Git => "workspace.tab.git_close",
        }
    }
}

/// 헤더에 놓는 보조 탭 상한 — 세션 제목이 우선이라 그 이상은 받지 않는다.
pub const PANE_AUX_TAB_MAX: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneAuxTab {
    pub kind: PaneAuxTabKind,
    pub label: String,
    pub active: bool,
}
```

`WorkspaceUi`의 `aux_tab: Option<PaneAuxTab>` → `aux_tabs: Vec<PaneAuxTab>`.
`set_aux_tab(Option<PaneAuxTab>)` → `set_aux_tabs(Vec<PaneAuxTab>)`(들어온 목록을
`PANE_AUX_TAB_MAX`로 자른다).

출력 타입을 바꾼다(`WorkspaceSurfaceOutput`, `PaneRenderOutput` 둘 다):

```rust
    pub aux_tab_intent: Option<(PaneAuxTabKind, PaneAuxTabIntent)>,
```

- [ ] **Step 4: 기하를 루프로 바꾼다**

`pane_aux_tab_geometry`의 첫 인자를 `session_close: egui::Rect` → `left: f32`로 바꾸고
(`pane_header_active_boundary` 호출은 호출부로 올린다), 라벨 폭 예산은 탭 수로 나눈다.

```rust
fn pane_aux_tab_label_width(header_width: f32, natural_label_width: f32, tab_count: usize) -> f32 {
    let count = tab_count.max(1) as f32;
    let budget = (header_width * 0.5 / count - pane_aux_tab_width(0.0)).max(PANE_AUX_TAB_MIN_LABEL);
    natural_label_width.max(0.0).min(budget)
}
```

배치 함수를 새로 만든다.

```rust
#[derive(Clone, Debug, PartialEq)]
struct AuxTabPlacement {
    kind: PaneAuxTabKind,
    label: String,
    active: bool,
    geometry: PaneAuxTabGeometry,
}

/// 세션 ×의 accent 경계에서 시작해 왼→오로 이어 붙인다. 한 탭이라도 자리를 못 만들면
/// 거기서 멈춘다 — 뒤 탭부터 사라지고, 남은 탭은 레일로 계속 전환할 수 있다.
fn layout_aux_tabs(
    header: egui::Rect,
    session_close: egui::Rect,
    toolbar_left: f32,
    tabs: &[PaneAuxTab],
    natural_width: impl Fn(&str) -> f32,
) -> Vec<AuxTabPlacement> {
    let mut left = pane_header_active_boundary(header, session_close);
    let mut out = Vec::new();
    for tab in tabs.iter().take(PANE_AUX_TAB_MAX) {
        let width = pane_aux_tab_label_width(header.width(), natural_width(&tab.label), tabs.len());
        let Some(geometry) = pane_aux_tab_geometry(header, left, toolbar_left, width) else {
            break;
        };
        left = geometry.tab.right();
        out.push(AuxTabPlacement {
            kind: tab.kind,
            label: tab.label.clone(),
            active: tab.active,
            geometry,
        });
    }
    out
}
```

- [ ] **Step 5: 두 렌더 경로를 고친다**

(a) 세션 없는 스트립(약 3796~3890행): `aux_label`/`aux` 단일 계산을 `layout_aux_tabs`
호출로 바꾼다. `pseudo_close`를 `session_close`로 넘긴다. accent_range는 **활성 placement**의
`tab` 범위를 쓰고, 없으면 기존 폴백. 구분선은 **첫 placement**의 `tab.left()`에 그린다.
빈 라벨 클릭 히트박스의 오른쪽 끝은 첫 placement의 `tab.left()`(없으면 `header.right()`).
렌더 루프는 placement마다 `render_aux_tab(..., placement.kind, id.with(index))`를 부르고,
반환 intent를 `(placement.kind, intent)`로 싣는다.

(b) 세션 헤더(약 3941~4134행): `aux_reserved_width`는 **모든 탭 합**이다.

```rust
        let aux_tabs: Vec<PaneAuxTab> = if owns_aux_tab { self.aux_tabs.clone() } else { Vec::new() };
        let aux_reserved_width: f32 = aux_tabs
            .iter()
            .take(PANE_AUX_TAB_MAX)
            .map(|tab| {
                let natural = ui.painter()
                    .layout_no_wrap(tab.label.clone(), font.clone(), egui::Color32::WHITE)
                    .size().x;
                pane_aux_tab_width(pane_aux_tab_label_width(header.width(), natural, aux_tabs.len()))
                    + PANE_AUX_TAB_RIGHT_PAD
            })
            .sum();
```

`aux_active`는 `placements.iter().any(|p| p.active)`.

`render_aux_tab`은 `kind: PaneAuxTabKind` 인자를 받아 hover 문구를 `kind.hint_key()` /
`kind.close_key()`로 고른다(하드코딩된 `workspace.tab.history_*` 제거).

`aux_body_rect`를 세우는 4480행 근처 조건 `self.aux_tab.as_ref().is_some_and(|tab| tab.active)`
는 `self.aux_tabs.iter().any(|tab| tab.active)`로 바꾼다. 3403/3451행의
`self.aux_tab.as_ref().map(|tab| tab.active)` match도 같은 규칙으로 고친다.

- [ ] **Step 6: 기존 테스트를 갱신한다**

7295/7324/7483/7551/7608행의 `set_aux_tab(Some(PaneAuxTab { label, active }))`를
`set_aux_tabs(vec![PaneAuxTab { kind: PaneAuxTabKind::History, label, active }])`로,
`Some(PaneAuxTabIntent::X)` 단언은 `Some((PaneAuxTabKind::History, PaneAuxTabIntent::X))`로 바꾼다.
`pane_aux_tab_geometry`를 직접 부르는 테스트는 새 시그니처(`left: f32`)에 맞춰
`pane_header_active_boundary(header, close)`를 호출부에서 계산해 넘긴다.

- [ ] **Step 7: 통과를 확인한다**

Run: `cargo test -p deppy-sijo --lib ui::workspace 2>&1 | tail -20`
Expected: PASS
Run: `cargo clippy -p deppy-sijo --all-targets -- -D warnings 2>&1 | tail -5`
Expected: 경고 0 (`app.rs`가 아직 `set_aux_tab`을 부르면 이 단계에서 컴파일이 깨진다 —
Task 5에서 고치므로, 여기서는 `app.rs`의 호출부를 `set_aux_tabs(vec![...History...])`로
**최소 변경**해 컴파일만 통과시킨다.)

- [ ] **Step 8: 커밋**

```bash
git add crates/app/src/ui/workspace.rs crates/app/src/app.rs
git commit -m "feat(app): pane 보조 탭을 최대 2개로 넓힌다"
```

---

### Task 5: 레일 진입 + Git 탭 상태 (사이드바에서 Git 제거)

**Files:**
- Modify: `crates/app/src/ui/file_tree.rs`
- Modify: `crates/app/src/app.rs`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`file_tree.rs` 테스트에서 기존 `SIDEBAR_TOOLS` 단언(6836행 근처)을 바꾼다.

```rust
    #[test]
    fn 사이드바_도구는_파일과_메모_둘뿐이다() {
        // Git은 2026-08-15 2차에서 레일로 옮겼다 — 사이드바 폭이 목록에 모자랐다(스펙 §8-1).
        assert_eq!(SIDEBAR_TOOLS, [SidebarTool::Files, SidebarTool::Notes]);
    }
```

`app.rs` 테스트에 상호배타 계약을 추가한다(기존 `work_history_tab은_전역view가…` 테스트 옆).

```rust
    #[test]
    fn 보조_탭은_동시에_활성되지_않는다() {
        use ui::workspace::PaneAuxTabState::{OpenActive, OpenInactive};
        let (history, git) = resolve_aux_tab_exclusivity(OpenActive, OpenActive, AuxTabWinner::Git);
        assert_eq!(git, OpenActive);
        assert_eq!(history, OpenInactive, "본문은 하나뿐이라 진 쪽은 물러난다");
        let (history, git) = resolve_aux_tab_exclusivity(OpenActive, OpenActive, AuxTabWinner::History);
        assert_eq!(history, OpenActive);
        assert_eq!(git, OpenInactive);
    }
```

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test -p deppy-sijo --lib 2>&1 | tail -20`
Expected: FAIL

- [ ] **Step 3: 사이드바에서 Git을 뺀다**

`file_tree.rs`에서:
- `enum SidebarTool`의 `Git` 변형 삭제, `SIDEBAR_TOOLS`를 `[SidebarTool; 2]`로.
- `sidebar_tool_label_key`의 Git 분기 삭제.
- `sidebar_tool_action`을 `Files | Notes => None`으로.
- 도구 탭 렌더의 `if *tool == SidebarTool::Git { action = Some(GitPanelRefresh) }` 삭제.
- 본문 교체 블록 `if self.selected_tool == SidebarTool::Git { ... }` 전체 삭제.
- `FileTreeUi`의 `git_panel: GitPanelUi` 필드와 `select_git_tool()` 삭제.
- `SidebarAction`의 `GitPanelRefresh`/`GitPanelOpenRemote`/`ShowFileDiff`도 삭제한다 —
  이제 그 intent들은 **git 패널이 App에 직접** 올린다(App이 `GitPanelUi`를 소유).

- [ ] **Step 4: 레일에 Git 행을 넣는다**

`enum NavIcon`에 `Git` 추가. `paint_nav_icon`에 글리프를 그린다(기존 획 스타일 유지).

```rust
        // Git — 가지: 위 점에서 아래 점으로 내려오는 줄기 + 오른쪽으로 갈라지는 가지.
        NavIcon::Git => {
            p.line_segment([egui::pos2(c.x - 3.5, c.y - 5.0), egui::pos2(c.x - 3.5, c.y + 5.0)], stroke);
            p.circle_stroke(egui::pos2(c.x - 3.5, c.y - 5.0), 1.8, stroke);
            p.circle_stroke(egui::pos2(c.x - 3.5, c.y + 5.0), 1.8, stroke);
            p.circle_stroke(egui::pos2(c.x + 4.0, c.y - 1.0), 1.8, stroke);
            p.line_segment([egui::pos2(c.x - 3.5, c.y + 1.5), egui::pos2(c.x + 4.0, c.y - 1.0)], stroke);
        }
```

`navigation()`에서 「이력」 행 **뒤**, 「AI」 행 **앞**에 넣는다.

```rust
        if nav_row(
            ui,
            NavIcon::Git,
            &catalog.t("sidebar.nav.git", &[]),
            // 이력과 같은 규칙 — 보조 탭이 **활성**일 때만 켠다.
            sidebar.git_tab_active,
            None,
        )
        .clicked()
        {
            action = Some(SidebarAction::ShowGit);
        }
```

`SidebarSnapshot`에 `git_tab_active: bool`을 더한다(`history_tab_active` 바로 옆).
`SidebarAction`에 `ShowGit`을 더한다.

- [ ] **Step 5: App에 Git 탭 상태를 만든다**

`app.rs`:
- 필드 `git_tab: ui::workspace::PaneAuxTabState`(기본 `Closed`)와
  `git_panel_ui: ui::git_panel::GitPanelUi`(file_tree에서 옮겨 온 소유권)를 추가한다.
  `new()` 초기화도 함께.
- `work_history_tab` 필드 타입도 `PaneAuxTabState`다(Task 3에서 이관).
- 상호배타 헬퍼를 자유 함수로 둔다(테스트가 부른다).

```rust
/// 어느 보조 탭이 방금 활성이 됐는지.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AuxTabWinner {
    History,
    Git,
}

/// 보조 본문은 하나뿐이라 두 탭이 동시에 활성일 수 없다. 진 쪽은 세션 탭으로 물러나되
/// 탭 자체는 남는다(`on_session_tab_click`).
fn resolve_aux_tab_exclusivity(
    history: ui::workspace::PaneAuxTabState,
    git: ui::workspace::PaneAuxTabState,
    winner: AuxTabWinner,
) -> (ui::workspace::PaneAuxTabState, ui::workspace::PaneAuxTabState) {
    match winner {
        AuxTabWinner::History if git.is_active() => (history, git.on_session_tab_click()),
        AuxTabWinner::Git if history.is_active() => (history.on_session_tab_click(), git),
        _ => (history, git),
    }
}
```

- `SidebarAction::ShowGit` 핸들러:

```rust
                Some(ui::file_tree::SidebarAction::ShowGit) => {
                    let previous = self.git_tab;
                    self.git_tab = previous.on_rail_click();
                    if self.git_tab.is_active() {
                        // 이력과 같은 진입 — 활성이 될 때만 스냅샷을 새로 받는다(폴링 없음).
                        (self.work_history_tab, self.git_tab) = resolve_aux_tab_exclusivity(
                            self.work_history_tab,
                            self.git_tab,
                            AuxTabWinner::Git,
                        );
                        self.reveal_terminal_view_for_aux_tab();
                        self.request_git_panel_io(ui.ctx(), ui::git_panel::GitPanelIoRequest::Snapshot);
                    }
                }
```

`ShowHistory` 핸들러에도 같은 상호배타 호출을 `AuxTabWinner::History`로 넣는다.
`reveal_terminal_view_for_aux_tab()`은 보조 본문이 보이려면 중앙이 `Terminal` 뷰여야 하므로
`agent_terminal_ui.set_view(Terminal)`을 부르는 헬퍼다 — 이력의 `ShowHistory` 핸들러가
이미 같은 일을 하고 있으면 그 코드를 그대로 재사용한다.

- `SidebarSnapshot`을 만드는 자리(23783행 근처)에 `git_tab_active: self.git_tab.is_active()`.
- 보조 탭 목록을 세운다(24391행 근처, `set_aux_tab` 호출 대체):

```rust
        let mut aux_tabs = Vec::new();
        if terminal_visible && self.work_history_tab.is_open() {
            aux_tabs.push(ui::workspace::PaneAuxTab {
                kind: ui::workspace::PaneAuxTabKind::History,
                label: text.t("workspace.tab.history", &[]),
                active: history_tab_active,
            });
        }
        if terminal_visible && self.git_tab.is_open() {
            aux_tabs.push(ui::workspace::PaneAuxTab {
                kind: ui::workspace::PaneAuxTabKind::Git,
                label: text.t("workspace.tab.git", &[]),
                active: git_tab_active,
            });
        }
        self.active.workspace_ui.set_aux_tabs(aux_tabs);
```

- 탭 intent 적용을 `kind`로 분기한다(24942행 근처):

```rust
        if let Some((kind, intent)) = aux_tab_intent {
            match kind {
                ui::workspace::PaneAuxTabKind::History => self.apply_work_history_tab_intent(intent),
                ui::workspace::PaneAuxTabKind::Git => self.apply_git_tab_intent(intent),
            }
            ui.ctx().request_repaint();
        }
```

`apply_git_tab_intent`는 `apply_work_history_tab_intent`와 같은 모양이고, 활성이 될 때
상호배타를 걸고 스냅샷을 요청한다.

- `history_tab_active`를 쓰던 fail-closed 조건 3곳(24401 / 24421 / 24869·24904 / 24937)은
  전부 `aux_body_active = history_tab_active || git_tab_active`로 바꾼다.

- [ ] **Step 6: 통과를 확인한다**

Run: `cargo test -p deppy-sijo 2>&1 | tail -20`
Expected: PASS

- [ ] **Step 7: 커밋**

```bash
git add crates/app/src/ui/file_tree.rs crates/app/src/app.rs
git commit -m "feat(app): Git 진입을 사이드바 도구에서 레일로 옮긴다"
```

---

### Task 6: Git 보조 본문 — 마스터-디테일 + 전면 뷰 은퇴

**Files:**
- Modify: `crates/app/src/app.rs`
- Modify: `crates/app/src/ui/diff_viewer.rs`
- Modify: `crates/app/src/ui/agent_terminal.rs`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`app.rs` 테스트에 폭 계산과 소스 계약을 고정한다.

```rust
    #[test]
    fn git_본문은_목록_300에_diff_나머지다() {
        assert_eq!(git_tab_list_width(1200.0), 300.0);
        assert_eq!(git_tab_list_width(600.0), 240.0, "좁으면 40%");
        assert_eq!(git_tab_list_width(300.0), 180.0, "최소 폭 밑으로는 안 내려간다");
    }

    #[test]
    fn diff는_전면_뷰가_아니라_보조_본문에서만_산다() {
        let source = include_str!("app.rs");
        assert!(
            !source.contains("AgentTerminalView::Diff"),
            "전면 diff 뷰는 2026-08-15 2차에서 은퇴했다(스펙 §8-3)"
        );
    }
```

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test -p deppy-sijo --lib 2>&1 | tail -20`
Expected: FAIL

- [ ] **Step 3: 폭 계산과 본문 렌더를 만든다**

```rust
/// Git 보조 본문 좌측 목록 폭 — 목록은 경로가 읽히는 최소 폭이 있고, diff는 넓을수록 좋다.
/// 넓은 창에서는 300pt 고정, 좁아지면 40%로 따라 줄되 180pt 밑으로는 내려가지 않는다.
fn git_tab_list_width(body_width: f32) -> f32 {
    const FIXED: f32 = 300.0;
    const MIN: f32 = 180.0;
    (body_width * 0.4).min(FIXED).max(MIN).min(body_width.max(MIN))
}

/// Git 보조 탭 본문 — 좌 목록 / 우 diff. 이력 본문(render_work_history_tab_body)과 같은
/// 자리에 같은 규칙으로 그린다.
fn render_git_tab_body(
    &mut self,
    ui: &mut egui::Ui,
    body: egui::Rect,
    text: &i18n::Catalog,
) -> Option<ui::git_panel::GitPanelAction> {
    let list_width = git_tab_list_width(body.width());
    let (list_rect, diff_rect) = body.split_left_right_at_x(body.left() + list_width);
    let mut list = ui.new_child(
        egui::UiBuilder::new().max_rect(list_rect).id_salt("git_panel_pane_tab"),
    );
    list.set_clip_rect(list_rect.intersect(ui.clip_rect()));
    let action = self.git_panel_ui.render(&mut list, text);

    crate::ui::designall::vertical_separator(ui, diff_rect.left(), body.y_range());

    let mut detail = ui.new_child(
        egui::UiBuilder::new().max_rect(diff_rect.shrink2(egui::vec2(6.0, 0.0)))
            .id_salt("git_diff_pane_tab"),
    );
    detail.set_clip_rect(diff_rect.intersect(ui.clip_rect()));
    self.diff_viewer_ui.render(&mut detail, text);
    action
}
```

`designall::vertical_separator`의 실제 시그니처는 파일에서 확인해 맞춘다 — 없으면
`ui.painter().vline(x, y_range, designall::separator_stroke(ui.visuals()))`로 그린다.

- [ ] **Step 4: 렌더 배선을 고친다**

24874·24909행의 `if let Some(body) = primary_output.aux_body_rect { ... }` 두 곳에서
활성 탭에 따라 갈라 부른다.

```rust
                    if let Some(body) = primary_output.aux_body_rect {
                        if git_tab_active {
                            git_panel_action = self.render_git_tab_body(&mut primary, body, &text);
                        } else {
                            work_history_action = self.render_work_history_tab_body(
                                &mut primary, body, &work_history_presentations,
                                &work_history_workspace_name,
                                work_history_current_branch.as_deref(), &text,
                            );
                        }
                    }
```

`git_panel_action`을 프레임 끝에서 처리한다.

```rust
        match git_panel_action {
            Some(ui::git_panel::GitPanelAction::Refresh) => {
                self.request_git_panel_io(ui.ctx(), ui::git_panel::GitPanelIoRequest::Snapshot);
            }
            Some(ui::git_panel::GitPanelAction::OpenRemoteBranch) => self.open_git_panel_remote(),
            Some(ui::git_panel::GitPanelAction::ShowFileDiff { rel_path, mode }) => {
                self.diff_viewer_ui.open(rel_path.clone(), mode);
                self.request_git_panel_io(
                    ui.ctx(),
                    ui::git_panel::GitPanelIoRequest::FileDiff { rel_path, mode },
                );
            }
            Some(ui::git_panel::GitPanelAction::OpenWorktreeShell { .. }) => { /* Task 7 */ }
            None => {}
        }
```

기존 `SidebarAction::{GitPanelRefresh, GitPanelOpenRemote, ShowFileDiff}` 핸들러(24027~24040행)는
삭제한다 — 이제 패널이 App에 직접 올린다.

- [ ] **Step 5: 전면 뷰를 은퇴시킨다**

- `agent_terminal.rs`의 `AgentTerminalView::Diff` 변형과 그 match 분기 삭제.
- `app.rs`의 `diff_visible`(24306행), `diff_viewer_action`, 24551행 렌더, 25139~25145행
  액션 처리에서 `Diff` 관련 코드 삭제.
- `diff_viewer.rs`에서 `DiffViewerAction::BackToTerminal`과 「터미널로」 버튼 삭제.
  `render`가 더 이상 액션을 안 내면 반환 타입을 `()`로 바꾼다. hunk ↑↓는 **유지**한다.
- `SidebarAction::ShowDiff { session }` 핸들러(24164행)는 Git 탭을 열도록 바꾼다.

```rust
                Some(ui::file_tree::SidebarAction::ShowDiff { session }) => {
                    // 이 세션의 repo를 연다 — 포커스 세션이 아니다(2026-08-15 회귀 수정 유지).
                    self.git_tab = self.git_tab.on_tab_click().max_open_active();
                    ...
                }
```

`max_open_active()` 같은 새 메서드를 만들지 말고, 있는 것만 쓴다:
`self.git_tab = ui::workspace::PaneAuxTabState::OpenActive;` 로 직접 세운 뒤
`resolve_aux_tab_exclusivity`를 걸고 `reveal_terminal_view_for_aux_tab()`을 부른다.
cwd는 계속 `self.cached_session_cwd(session)`을 `request_git_panel_io_at`에 넘긴다.

- [ ] **Step 6: 통과를 확인한다**

Run: `cargo test -p deppy-sijo 2>&1 | tail -20`
Run: `cargo clippy -p deppy-sijo --all-targets -- -D warnings 2>&1 | tail -5`
Expected: 통과, 경고 0

- [ ] **Step 7: 커밋**

```bash
git add -A crates/app/src
git commit -m "feat(app): git 패널을 보조 탭 마스터-디테일로 옮긴다"
```

---

### Task 7: 워크트리 클릭 → 그 워크트리에서 셸 열기

**Files:**
- Modify: `crates/app/src/app.rs`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

기존 `spawn_shell_at`을 검증하는 테스트(app.rs 32300행 근처가 같은 호출을 쓴다)의 관례를 따라,
소스 계약으로 고정한다.

```rust
    #[test]
    fn 워크트리_클릭은_그_경로에서_셸을_연다() {
        let source = include_str!("app.rs");
        let handler = source
            .split_once("GitPanelAction::OpenWorktreeShell { path }")
            .expect("워크트리 클릭 핸들러가 있어야 한다")
            .1;
        let handler = &handler[..handler.len().min(600)];
        assert!(handler.contains("spawn_shell_at"), "새 셸을 그 경로에서 연다");
        assert!(
            !handler.contains("CreateWorktree") && !handler.contains("RemoveWorktree"),
            "이번 범위는 기존 워크트리로 들어가는 것뿐이다(스펙 §8-5)"
        );
    }
```

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test -p deppy-sijo --lib 워크트리_클릭 2>&1 | tail -10`
Expected: FAIL

- [ ] **Step 3: 구현한다**

Task 6에서 비워 둔 분기를 채운다.

```rust
            Some(ui::git_panel::GitPanelAction::OpenWorktreeShell { path }) => {
                // 「새 워크트리에서 셸」(PR-W)이 생성 직후 부르는 바로 그 경로다
                // (poll_worktree_jobs). 워크트리를 만들지도 지우지도 않는다.
                self.reveal_active_workspace_for_new_session();
                self.git_tab = self.git_tab.on_session_tab_click();
                self.active
                    .workspace_ui
                    .spawn_shell_at(self.config.terminal.scrollback_lines as usize, Some(path));
            }
```

- [ ] **Step 4: 통과를 확인한다**

Run: `cargo test -p deppy-sijo 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 5: 커밋**

```bash
git add crates/app/src/app.rs
git commit -m "feat(app): 워크트리 행 클릭으로 그 워크트리 셸을 연다"
```

---

### Task 8: i18n 5로케일 + 전체 게이트

**Files:**
- Modify: `crates/i18n/locales/{en-US,ko-KR,ja-JP,zh-Hans,zh-Hant}/messages.txt`

- [ ] **Step 1: 키를 넣고 뺀다**

**제거:** `sidebar.tool.git`, `git.back_to_terminal` (5로케일 전부)

**추가 (ko-KR 기준, 나머지 로케일도 같은 자리에):**

```
sidebar.nav.git = Git
workspace.tab.git = Git
workspace.tab.git_hint = git 변경 사항 보기
workspace.tab.git_close = Git 탭 닫기
git.section.worktrees = 워크트리
git.worktree.current = 현재
git.worktree.detached = 분리됨
git.worktree.open_hint = 이 워크트리에서 셸 열기
git.worktrees_truncated = 워크트리 목록이 잘렸습니다
git.diff.empty = 파일을 선택하면 변경 내용이 여기에 표시됩니다
```

en-US:

```
sidebar.nav.git = Git
workspace.tab.git = Git
workspace.tab.git_hint = View git changes
workspace.tab.git_close = Close Git tab
git.section.worktrees = Worktrees
git.worktree.current = current
git.worktree.detached = detached
git.worktree.open_hint = Open a shell in this worktree
git.worktrees_truncated = Worktree list truncated
git.diff.empty = Select a file to see its changes
```

ja-JP / zh-Hans / zh-Hant도 같은 키를 각 언어로 채운다. **키 순서는 각 파일의 기존
알파벳 정렬을 따른다** — `validate_required_locale_completeness`는 순서를 보지 않지만
diff를 읽기 쉽게 유지한다.

- [ ] **Step 2: 로케일 정합성을 확인한다**

Run: `cargo test -p deppy-i18n 2>&1 | tail -10`
Expected: PASS (`{locale} locale key mismatch` 없음)

- [ ] **Step 3: 전체 게이트**

Run: `cargo test -p deppy-sijo 2>&1 | tail -15`
Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5`
Expected: 실패 0, 경고 0

- [ ] **Step 4: 커밋**

```bash
git add crates/i18n/locales
git commit -m "feat(i18n): git 보조 탭·워크트리 문구 (5로케일)"
```

- [ ] **Step 5: 화면 확인 (오케스트레이터가 한다 — 서브에이전트 금지)**

```sh
pkill -x deppy-sijo; sleep 1; (nohup sh scripts/dev-run.sh > /tmp/dr.log 2>&1 &)
```

확인 항목:
1. 사이드바 도구 탭이 「파일 / 메모」 둘뿐이다.
2. 레일에 Git 행이 이력과 AI 사이에 있고, 누르면 세션 헤더 옆에 `Git ✕` 탭이 붙는다.
3. Git 본문이 좌측 목록 + 우측 diff로 갈라지고, 파일 경로가 `crates/…`로 잘리지 않는다.
4. 파일 행을 누르면 우측에 diff가 뜬다. 세션 탭을 누르면 터미널로 돌아가고 탭은 남는다.
5. 이력 탭과 Git 탭이 동시에 열려 있어도 하나만 활성이다.
6. 워크트리 섹션에 이 저장소의 워크트리가 전부 뜨고, 「현재」가 맞다.
7. 워크트리 행을 누르면 그 폴더에서 새 셸 pane이 열린다.

---

## 자체 검토 결과

- **스펙 커버리지:** §8-1 → Task 5, §8-2 → Task 3·4, §8-3 → Task 6, §8-4 → Task 1·2,
  §8-5 → Task 7, §8-6 → Task 8, §8-7 → 각 Task의 테스트 단계.
- **타입 일관성:** `PaneAuxTabKind`(Task 4 정의) → Task 5·6에서 같은 이름으로 사용.
  `PaneAuxTabState`(Task 3) → Task 5의 `resolve_aux_tab_exclusivity` 인자와 일치.
  `GitPanelAction::OpenWorktreeShell { path }`(Task 2) → Task 6의 빈 분기 → Task 7에서 채움.
- **알려진 순서 제약:** Task 4는 `app.rs` 호출부를 최소 수정해 컴파일만 통과시키고,
  Task 5가 제대로 배선한다. Task 6은 Task 5의 `git_panel_ui` 소유권 이전에 의존한다.
