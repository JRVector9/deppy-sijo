# 워크스페이스 세션 열기 요청 보존 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** 워크스페이스 메뉴의 세션 열기와 현재 세션 선택 배경을 독립 PR로 옮기고, controller가 바쁠 때도 정확한 대상의 세션 열기 요청을 보존한다.

**Architecture:** 기존 capacity-one controller와 SyncDotenv 순서는 유지한다. 세션 열기는 별도 `Option<String>`에 대상을 보관하고 controller 소비 뒤 빈 슬롯으로 재입장한다. 대기 중 재선택은 최신 대상 하나로 병합한다. 실행 직전 대상 존재·launcher busy를 확인하고, 전환을 시도한 뒤 실제 active ID를 재검증한다. 삭제나 전환 거부 시 다른 workspace에 launcher를 열지 않는다.

**Tech Stack:** Rust, egui 0.36, egui_kittest, 기존 App logic/render 경계, GitHub CLI.

---

## 파일 경계

- `crates/app/src/app.rs`: controller 액션, bounded 요청, 실행 검증, 기존 테스트 모듈.
- `crates/app/src/ui/file_tree.rs`: 세션 열기 메뉴·대상 ID·현재 세션 선택 배경·기존 kittest.
- `crates/i18n/locales/{en-US,ja-JP,ko-KR,zh-Hans,zh-Hant}/messages.txt`: `sidebar.menu.open_session`만 추가.
- 이 계획과 `docs/CODEX_HANDOFF.md`: 이 브랜치의 결과만 추가. 기존 기록 보존.
- `panic_policy.rs`, 문서 렌더 격리, numeric settings, terminal/runtime/storage는 변경하지 않는다.

### Task 1: 검증된 메뉴와 배경 변경을 독립 이관

- [x] **Step 1: 원본 두 커밋의 정확한 hunk를 확인한다.**

```sh
git show 6ec8377 -- crates/app/src/app.rs crates/app/src/ui/file_tree.rs crates/i18n/locales
git show f0bd342 -- crates/app/src/ui/file_tree.rs
```

`6ec8377`에서 app의 OpenAgentLauncherForWorkspace 액션·라우팅·open_agent_launcher_for_workspace·세션열기 테스트만 선택한다. file_tree 전체 diff와 locale의 open_session 키를 옮긴다. `f0bd342`는 file_tree만 적용한다. 기존에 RED/GREEN을 거친 기능의 이관이며 신규 회귀 수정은 Task 2에서 테스트부터 진행한다.

- [x] **Step 2: 범위와 기존 focused tests를 확인한다.**

```sh
git diff --stat
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo --locked 세션열기 -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo --locked 보고있는_세션만 -- --test-threads=1
```

예상: 메뉴 대상 ID 및 선택 배경 테스트 통과, panic/settings diff 없음.

### Task 2: 요청 보존 RED → GREEN

- [x] **Step 1: 실제 입장 함수를 사용하는 상태 회귀 테스트를 먼저 추가한다.**

핵심 테스트 상태:

```rust
let mut slot = Some(WorkspaceControllerAction::SyncDotenv);
let mut pending = Some("workspace-b".to_owned());
assert!(!retry_workspace_session_open(&mut slot, &mut pending));
assert!(matches!(slot, Some(WorkspaceControllerAction::SyncDotenv)));
assert_eq!(pending.as_deref(), Some("workspace-b"));
slot.take();
assert!(retry_workspace_session_open(&mut slot, &mut pending));
assert!(matches!(slot, Some(WorkspaceControllerAction::OpenAgentLauncherForWorkspace(ref id)) if id == "workspace-b"));
assert!(pending.is_none());
```

동일 테스트 계약으로 Runtime/ComposerPrompt 선점, 반복 재시도, 최신 대상 병합, 실행 시 존재하지 않는 대상·busy·warm 전환 거부·성공을 검증한다.

- [x] **Step 2: RED를 기록한다.**

```sh
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo --locked 세션열기 -- --test-threads=1
```

예상: 신규 입장/판정 함수 부재 또는 요청 보존 assertion 실패. 단순 문법 오류는 RED로 세지 않는다.

- [x] **Step 3: bounded helper와 App 연결을 추가한다.**

```rust
fn retry_workspace_session_open(
    slot: &mut Option<WorkspaceControllerAction>,
    pending: &mut Option<String>,
) -> bool {
    if slot.is_some() { return false; }
    let Some(id) = pending.take() else { return false; };
    *slot = Some(WorkspaceControllerAction::OpenAgentLauncherForWorkspace(id));
    true
}
```

`App.pending_workspace_session_open: Option<String>`은 None으로 초기화한다. 메뉴 클릭은 최신 ID를 저장하고 재입장 helper를 호출한다. `poll_workspace_controller()` 직후 다시 helper를 호출하고 성공 시 repaint한다. 기존 슬롯이 먼저 실행되므로 SyncDotenv의 대상/순서는 바뀌지 않는다. 화면 전환은 실행 가능한 대상 검증 후 수행한다. 실행 판정은 실제 production helper로 분리해 busy/삭제/전환 성공 여부를 테스트한다.

- [x] **Step 4: GREEN을 기록한다.**

```sh
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo --locked 세션열기 -- --test-threads=1
```

예상: 모든 메뉴·입장·대상 검증 테스트 통과. 실행 뒤 pending이 비어서 동일 요청이 반복되지 않는다.

### Task 3: 리뷰·게이트·착수 기록·PR

- [x] **Step 1: 소스만 대상으로 codex CLI 정적 리뷰를 실행한다.** 테스트/빌드/편집/서브에이전트 실행을 금지하는 프롬프트로 app.rs, file_tree.rs, i18n diff를 리뷰한다. 지적은 재현 근거를 확인하고 수정한다.
- [x] **Step 2: 다음 게이트를 직렬 실행한다.**

```sh
cargo fmt --all -- --check
git diff --check
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p i18n --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
```

workspace 경계 검사 명령은 저장소 CI/xtask 정의를 확인해 동일하게 실행한다. 앱 번들 생성·GUI 실행은 하지 않는다.

- [x] **Step 3: 실제 명령 결과, 실패 접근, 리뷰 반영, 남은 항목을 handoff와 프로젝트 일지에 기록한다.**
- [x] **Step 4: 한국어 Conventional Commit을 만들고 push·PR 생성한다.**

```sh
git add crates/app/src/app.rs crates/app/src/ui/file_tree.rs crates/i18n/locales docs/CODEX_HANDOFF.md docs/superpowers/plans/2026-09-08-workspace-open-intent.md
git commit -m 'fix(app): 워크스페이스 세션 열기 요청을 보존한다'
git push -u origin fix/workspace-open-intent
gh pr create --base main --head fix/workspace-open-intent --title 'fix(app): 워크스페이스 세션 열기 요청 보존' --body-file /private/tmp/deppy-session-intent-pr-body.md
```

PR 본문에는 메뉴/선택 배경 동작, 충돌 시 보존, 실행 전 재검증, 실제 게이트 결과를 적는다. PR merge는 이 작업 범위에 포함하지 않는다.

## 완료 결과

계획 a47b00c, 구현 e114180, PR #156. App 2,080 tests/i18n 8 tests/Clippy/fmt/diff/boundary PASS, Codex 소스 리뷰 확정 결함 없음. GUI 번들 생성·실행 및 PR merge는 수행하지 않았다.
