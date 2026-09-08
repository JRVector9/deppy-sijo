# 파일 트리 다중 선택과 워크스페이스 순서 이관 구현 계획

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** PR #146의 파일 트리 마키/Command 다중 선택·다중 복사/삭제와 워크스페이스 순서·펼침 상태 기능만 최신 `origin/main`에 독립적으로 이관한다.

**Architecture:** `FileTreeUi`가 화면에 보이는 경로의 선택 집합과 마키 제스처, 직렬 삭제 대기열을 소유한다. 사이드바는 드래그가 끝날 때 전체 워크스페이스 ID 순서를 `SidebarAction`으로 내보내고, `App`이 숨긴 ID를 보존해 병합한 뒤 `Config`에 저장한다. 기존 #156 세션 열기, #158 Markdown 격리와 Relay/Fleet/IME/resize 코드는 기준 브랜치 그대로 둔다.

**Tech Stack:** Rust, egui/egui_kittest, serde/TOML, Cargo, GitHub CLI.

---

### Task 1: 선택·마키·다중 파일 작업 계약

**Files:**
- Modify: `crates/app/src/ui/file_tree.rs`

- [x] **Step 1: 실패 테스트를 먼저 추가한다.**

  #146의 순수 함수 및 UI 계약 테스트 가운데 `selection_click_kind`, `selection_range`, `marquee_row_range`, `selection_or`, 직렬 삭제 대기열, 복사/삭제 단축키 소유권 테스트를 현재 테스트 모듈에 이식한다. 테스트는 다중 선택 타입과 함수가 아직 없어서 컴파일에 실패해야 한다.

- [x] **Step 2: RED를 확인한다.**

  Run: `CARGO_TARGET_DIR=/private/tmp/deppy-file-tree-clean-target CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo --locked file_tree --no-run`

  Expected: `SelectionClick`, `selection_range` 또는 새 선택 상태가 없다는 컴파일 실패.

- [x] **Step 3: 최소 구현을 이관한다.**

  `FileTreeUi`에 선택 집합, 기준점, 마키 상태와 직렬 삭제 대기열을 추가한다. 행/빈 영역의 주 버튼 드래그를 마키로 처리하고 Command 토글·Shift 범위를 적용한다. 선택이 있으면 `Command+C`와 삭제가 선택 전체에 작동하되 IO capacity 1을 지키도록 삭제를 완료마다 하나씩 이어 보낸다. 삭제 키의 OS repeat 이벤트는 무시하고, 첫 삭제 요청이 Busy면 접수되지 않은 대기열을 폐기한다. 휴지통 실패 시 단일 `DeleteTarget`에서 확인 문구와 실제 영구 삭제 경로를 함께 만든다.

- [x] **Step 4: GREEN을 확인한다.**

  Run: `CARGO_TARGET_DIR=/private/tmp/deppy-file-tree-clean-target CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo --locked file_tree -- --test-threads=1`

  Expected: file_tree 필터 테스트 전부 PASS.

### Task 2: 워크스페이스 순서와 펼침 상태

**Files:**
- Modify: `crates/app/src/ui/file_tree.rs`
- Modify: `crates/app/src/config.rs`
- Modify: `crates/app/src/app.rs`

- [x] **Step 1: 실패 테스트를 먼저 추가한다.**

  #146의 순서 계산, 드래그 수명, 활성 워크스페이스 전환 시 펼침 상태, 저장 순서 병합/정리/TOML 왕복 테스트를 추가한다. 현재 `SidebarAction::ReorderWorkspaces`와 `UiConfig::workspace_order`가 없어서 컴파일에 실패해야 한다.

- [x] **Step 2: RED를 확인한다.**

  Run: `CARGO_TARGET_DIR=/private/tmp/deppy-file-tree-clean-target CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo --locked workspace_reorder --no-run`

  Expected: 순서 액션/설정 필드/순수 함수가 없다는 컴파일 실패.

- [x] **Step 3: 최소 구현을 이관한다.**

  `ReorderWorkspaces(Vec<String>)`를 추가하고 세 구간의 워크스페이스 행이 하나의 drag 상태를 공유하게 한다. 포인터 중심으로 삽입 위치를 계산하고 가장자리 자동 스크롤 뒤 드롭에서만 액션을 보낸다. 전환할 때 새 활성 그룹만 자동으로 펼치되 사용자가 명시적으로 접은 기록은 유지한다. `workspace_order`를 serde 기본값과 함께 저장하고 App에서 보이는 순서와 숨긴 ID를 병합하며 삭제된 ID만 정리한다.

- [x] **Step 4: GREEN을 확인한다.**

  Run: `CARGO_TARGET_DIR=/private/tmp/deppy-file-tree-clean-target CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo --locked workspace -- --test-threads=1`

  Expected: 관련 workspace/file_tree 테스트 전부 PASS.

### Task 3: 경계·회귀·리뷰·출시 준비

**Files:**
- Modify: `docs/CODEX_HANDOFF.md`
- Create: Obsidian `프로젝트 일지/deppy-sijo/2026-09-08 파일 트리 다중 선택과 순서 clean 이관.md`

- [x] **Step 1: 제외 경계를 검사한다.**

  `git diff --name-only origin/main...HEAD`가 `file_tree.rs`, `config.rs`, `app.rs`, 계획/핸드오프만 포함하는지 확인한다. #146의 해당 함수와 테스트 목록을 비교하고 `agent_*`, Relay, Fleet, launcher, terminal renderer, Markdown 격리 파일이 없는지 검증한다.

- [x] **Step 2: 전체 관련 게이트를 실행한다.**

  Run: `CARGO_TARGET_DIR=/private/tmp/deppy-file-tree-clean-target CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo --locked -- --test-threads=1`

  Run: `CARGO_TARGET_DIR=/private/tmp/deppy-file-tree-clean-target CARGO_BUILD_JOBS=2 cargo clippy -p deppy-sijo --bin deppy-sijo --locked -- -D warnings`

  Run: `cargo fmt --all -- --check && git diff --check`

  Expected: 모두 PASS. GitHub Actions가 계정 billing/spending 제한으로 실행되지 않으면 BLOCKED로 기록한다.

- [x] **Step 3: Codex CLI로 코드 diff를 리뷰하고 지적을 반영한다.**

  Run: `codex review --base origin/main`

  Expected: 확정 finding이 없거나, finding마다 재현 테스트를 RED로 확인하고 수정 후 GREEN.

- [ ] **Step 4: 문서화·커밋·PR을 완료한다.**

  handoff와 Obsidian 일지에 실제 명령/결과/실패 접근/남은 화면 검증을 기록한다. 한국어 커밋을 push하고 `main` 대상 Ready PR을 만든다. 앱 빌드·재실행, rebase, force-push는 수행하지 않는다.
