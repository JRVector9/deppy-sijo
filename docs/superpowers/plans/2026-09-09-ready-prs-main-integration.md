# Ready PR Main 통합 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 검증을 마친 19개 Ready PR을 최신 `main` 위에서 의미 보존 방식으로 통합하고, 단일 통합 PR을 `main`에 반영한 뒤 앱을 빌드·재실행한다.

**Architecture:** 원래 PR head를 merge commit으로 보존해 스택 의존성을 유지한다. 터미널/스크롤백, App/UI, Relay/저장소 순으로 합치고 알려진 충돌은 양쪽 계약을 함께 보존한다. Relay v38·v39 뒤에 Keychain 복구 migration을 v40으로 배치하며, 통합 전체 검증과 Codex 리뷰 후에만 `main`을 갱신한다.

**Tech Stack:** Git merge commits, Rust/Cargo, SQLite forward-only migrations, GitHub CLI, macOS app bundle.

---

### Task 1: 터미널·스크롤백·리사이즈 스택 통합

**Files:** `crates/runtime/src/command.rs`, `crates/app/src/ui/workspace.rs`, `crates/terminal/`, `third_party/alacritty_terminal-0.26.0/`

- [x] #157 → #160 → #165 → #167 순으로 merge한다.
- [x] #159를 merge하고 `TERMINAL_CELL_COUNT_MAX` 공개 계약과 공용 `SCROLLBACK_LINES_MAX`를 함께 보존한다.
- [x] 리사이즈 tracked-generation과 화면 폭/pending resize 검증을 함께 보존한다.
- [x] focused runtime/terminal/workspace 테스트를 실행한다.

### Task 2: 워크스페이스·파일·Fleet·런처·폰트 통합

**Files:** `crates/app/src/app.rs`, `crates/app/src/ui/file_tree.rs`, `crates/app/src/config.rs`, `crates/i18n/locales/*/messages.txt`, `crates/app/src/fonts.rs`

- [x] #156 → #173 → #158 → #164 → #170 → #171 순으로 merge한다.
- [x] `OpenWorkspaceSession`과 `ReorderWorkspaces` action을 모두 유지하고 match arm을 완전하게 만든다.
- [x] 현재 세션 선택면과 활성 워크스페이스 그룹의 밝기 계층을 모두 보존한다.
- [x] i18n 다섯 로케일을 합치고 제거 대상 Fleet 키만 제거됐는지 확인한다.
- [x] focused file-tree와 전체 workspace에서 Markdown drop, Fleet, launcher, font 테스트를 실행한다.

### Task 3: Relay·Keychain·Grok·패키징 통합

**Files:** `crates/storage/src/db.rs`, `crates/app/src/app.rs`, `crates/web-remote/`, `crates/relay-*`, `scripts/package-macos.sh`, `.github/workflows/`

- [x] #161 → #168 → #169 순으로 merge하고 #168 뒤의 독립 runner #175를 merge한다.
- [x] #166을 merge하면서 `recovery_generation` migration을 Relay v38/v39 뒤의 v40으로 재번호한다.
- [x] #172, #174, #176을 모두 merge한다.
- [x] migration 37→40 및 Relay/Keychain/Grok storage 결합 회귀 테스트를 먼저 추가하거나 기존 테스트를 조정해 실패를 확인한 뒤 수정한다.
- [x] 실제 DNS·TLS·배포 자격증명 부재는 BLOCKED로 유지한다.

### Task 4: 전체 검증과 코드 리뷰

**Files:** 통합으로 변경된 전체 코드, `docs/CODEX_HANDOFF.md`

- [x] `cargo fmt --all -- --check`, `git diff --check`, `cargo xtask check-boundary`, `cargo xtask check-deps`를 실행한다.
- [x] app/storage/runtime/terminal/relay/web-remote focused 테스트와 workspace 전체 테스트를 jobs=2·serial로 실행한다.
- [x] workspace all-target strict Clippy를 실행한다.
- [x] 최종 통합 commit에 병렬 읽기 감사와 Codex CLI 리뷰를 실행하고 실제 지적을 반영한다. Codex CLI 두 실행은 sandbox 임시 object 오류와 대형 merge diff 반복 순회로 최종 보고서를 만들지 못한 사실을 기록한다.
- [x] `docs/CODEX_HANDOFF.md`를 실제 결과로 갱신한다. Obsidian 작업 일지는 Task 5의 최종 main/build 결과와 함께 작성한다.

### Task 5: 통합 PR, main 반영, 최종 앱 실행

**Files:** GitHub PR, macOS app bundle

- [ ] 통합 브랜치를 push하고 `main` 대상 Ready PR을 만든다.
- [ ] GitGuardian과 Actions의 실제 runner/steps를 구분해 기록한다.
- [ ] 사용자 승인 범위에 따라 통합 PR을 merge commit으로 `main`에 반영하고 원격 `main`을 확인한다.
- [ ] 새 `main`을 빌드·패키징하고 기존 앱의 정확한 PID만 종료한 뒤 새 bundle을 실행한다.
- [ ] 프로세스와 bundle commit을 확인하고 사용자가 화면·입력·스크롤백을 직접 검증할 수 있게 인계한다.

## Self-review

- 19개 open PR과 두 스택의 선행 관계를 모두 포함했다.
- 확인된 세 제품 충돌과 SQLite migration 번호 충돌을 명시했다.
- 새 통합 결함은 TDD로 수정하고 기존 PR 자체의 이미 검증된 코드는 재작성하지 않는다.
- 실제 외부 Relay 배포는 자격증명 없이 PASS로 기록하지 않는다.
