# Cursor Usage Status Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cursor 개인 플랜의 월간 사용량을 정확한 의미로 터미널 하단 provider 상태바에 표시한다.

**Architecture:** 새 `cursor_usage` 모듈이 감지된 공식 Cursor CLI의 `/usage` 화면을 유계 PTY로 읽어 구조체로 변환한다. 앱은 설치·활성화 상태를 보존해 백그라운드 프로브 결과를 `top_provider_usage`에 전달하고, 전용 renderer가 월간 값과 상세 호버를 그린다.

**Tech Stack:** Rust, egui, portable PTY, regex, Deppy i18n catalog

---

### Task 1: Cursor `/usage` 파서와 유계 프로브

**Files:**
- Create: `crates/app/src/cursor_usage.rs`
- Modify: `crates/app/src/main.rs`
- Test: `crates/app/src/cursor_usage.rs`

- [ ] **Step 1: Write the failing parser tests**

실측 패널에서 Included/Auto/API/플랜/초기화일/On-Demand를 읽고, unrelated percent와
불완전 화면을 거부하는 테스트를 먼저 추가한다. `parse_usage`가 아직 없어 컴파일 실패해야
한다.

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-ready-prs-integration-target cargo test --locked -p deppy-sijo --bin deppy-sijo cursor_usage::tests -- --test-threads=1`

Expected: FAIL because `parse_usage` and `CursorUsage` are not defined.

- [ ] **Step 3: Implement the parser and probe**

`CursorUsage`의 필드는 설계 문서와 같게 만들고, 라벨을 기준으로 마지막 `% used` 값을
선택한다. 프로브는 런처가 준 executable/PATH, 전용 디렉터리, 120x40 PTY, 100 KiB 출력
상한, 25초 timeout을 사용한다. `/usage` 뒤 Enter를 한 번 더 보내 명령 팔레트 선택을
확정하고 패널이 안정되면 session을 종료한다.

- [ ] **Step 4: Run parser tests and ignored live probe**

Run the focused command from Step 2, then:
`CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-ready-prs-integration-target cargo test --locked -p deppy-sijo --bin deppy-sijo cursor_usage::tests::cursor_실측_프로브가_월간_사용량을_읽는다 -- --ignored --exact --test-threads=1`

Expected: parser tests PASS; on this machine the live probe returns an Included percentage.

### Task 2: 상태바 연결과 표시

**Files:**
- Modify: `crates/app/src/app.rs`
- Modify: `crates/app/src/ui/agent_terminal.rs`
- Modify: `crates/i18n/locales/en-US/messages.txt`
- Modify: `crates/i18n/locales/ja-JP/messages.txt`
- Modify: `crates/i18n/locales/ko-KR/messages.txt`
- Modify: `crates/i18n/locales/zh-Hans/messages.txt`
- Modify: `crates/i18n/locales/zh-Hant/messages.txt`

- [ ] **Step 1: Extend the status data path**

`ProviderUsageInputs`와 `status_bar_with_managers`에
`Option<Option<CursorUsage>>`를 추가한다. 바깥 Option은 설치 감지, 안쪽 Option은 프로브
값이다. 앱 프레임은 launcher snapshot에서 Cursor를 찾아 활성화된 경우에만 `current`를
호출한다.

- [ ] **Step 2: Render truthful monthly usage**

전용 Cursor provider는 전체 폭에서 locale의 `월 {value}%`, 좁은 폭에서 `{value}%`를
표시한다. hover/accessibility는 사용 가능한 상세 항목만 결합한다. visible provider count와
폭 계산에 Cursor를 포함한다.

- [ ] **Step 3: Add all locale strings**

다섯 locale에 monthly short, accessibility, hover, unavailable, unavailable hover,
Auto/API/reset/on-demand 상세 라벨을 같은 키 집합으로 추가한다.

- [ ] **Step 4: Verify code contracts**

Run: `CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-ready-prs-integration-target cargo check --locked -p deppy-sijo`

Expected: PASS with every status bar call site updated.

### Task 3: Review, gates, record, and PR

**Files:**
- Modify: `docs/CODEX_HANDOFF.md`
- Create outside repository: `프로젝트 일지/deppy-sijo/2026-09-09 Cursor 사용량 상태바.md`

- [ ] **Step 1: Review source changes**

Run: `codex review --uncommitted`

Expected: no unresolved Critical/High findings. Fix valid findings and rerun focused tests.

- [ ] **Step 2: Run the commit gate once**

Run fmt check, workspace clippy all-targets with warnings denied, `xtask i18n-check`, and
`git diff --check` once after review fixes.

Expected: all commands exit 0.

- [ ] **Step 3: Commit and create the PR**

Commit the implementation with a Korean conventional message, push
`feat/cursor-usage-status`, and create a PR against `main` describing that Cursor supplies a
monthly billing-cycle percentage rather than a weekly window.

- [ ] **Step 4: Wait before rebuilding or restarting**

Do not replace the running app. Report the concrete PR and ask for the explicit rebuild/restart
approval required by the session rule.

