# Grok 세션 판독 Main 이관 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** #146에서 검증된 Grok 프로세스·세션·모델·강도·원문 판독을 최신 `main`에 독립 이관한다.

**Architecture:** 프로세스 감지는 실행 파일명과 provider별 강도 플래그를 읽고, 세션 결합은 `~/.grok/active_sessions.json`을 우선한 뒤 cwd별 최신 transcript로 제한적으로 폴백한다. JSONL 파서는 마지막 유효 상태만 투영하며, App은 현재 실행 중인 provider가 바뀌면 이전 provider의 모델·강도·요약을 폐기한다.

**Tech Stack:** Rust, serde_json, 기존 `agent_detect`/`agent_transcript`/App 상태 투영, Cargo 테스트.

---

### Task 1: 프로세스와 transcript 계약을 실패로 고정

**Files:**
- Modify: `crates/app/src/agent_detect.rs`
- Modify: `crates/app/src/agent_transcript.rs`

- [ ] **Step 1: Grok 분류·플래그·세션 경로 테스트를 먼저 추가한다**

`classify`가 파일명 `grok`만 `AgentKind::Grok`으로 판정하고 `--reasoning-effort`를 읽으며, `active_sessions.json`의 pid/cwd/session_id와 URL 인코딩 cwd 아래 `chat_history.jsonl`을 결합하는 테스트를 추가한다. `not-grok`과 잘못된 상대 cwd/session id는 거부한다.

- [ ] **Step 2: transcript 파서 테스트를 먼저 추가한다**

`parse_grok`이 `summary.json`의 model/effort와 JSONL의 최근 assistant 원문을 읽고, 잘린 마지막 행·과대 파일·심볼릭 링크를 fail-closed 처리하는 fixture 테스트를 추가한다.

- [ ] **Step 3: RED를 실행한다**

Run: `CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-grok-session-target cargo test -p deppy-sijo --bin deppy-sijo agent_detect::tests::classify는_grok_트램폴린과_실제_바이너리를_모두_잡는다 --locked -- --exact`

Expected: `AgentKind::Grok` 또는 Grok parser API 부재로 compile FAIL.

### Task 2: 최소 Grok 감지와 파서를 이관

**Files:**
- Modify: `crates/app/src/agent_detect.rs`
- Modify: `crates/app/src/agent_transcript.rs`
- Modify: `crates/app/src/agent_surface.rs`
- Modify: `crates/app/src/pty_effort.rs`

- [ ] **Step 1: provider와 실행 인자 계약을 구현한다**

`AgentKind`/`AgentProvider`에 `Grok`을 추가하고, Claude는 `--effort`, Grok은 `--reasoning-effort`, Codex/Kimi는 transcript 기반으로 분기한다. 세션 중 강도 전환을 지원하지 않는 Grok은 `EffortBlocked::Unsupported`로 명시한다.

- [ ] **Step 2: bounded 세션 탐색을 구현한다**

64KiB 이하 `active_sessions.json`의 정확한 pid 항목을 우선하고, 유효한 절대 cwd와 세션 id만 받아 `%XX` cwd 경로를 만든다. 디렉터리 탐색은 기존 `MAX_DIRECTORY_ENTRIES`를 사용하며 일반 파일만 transcript로 채택한다.

- [ ] **Step 3: bounded JSONL 파서를 구현한다**

기존 transcript 읽기 상한과 tail 파서 정책을 재사용해 최근 모델·강도·assistant 원문을 `TranscriptState`로 반환하고, summary가 없거나 일부 필드가 없을 때 JSONL의 유효 정보만 보존한다.

- [ ] **Step 4: GREEN을 실행한다**

Run: `CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-grok-session-target cargo test -p deppy-sijo --bin deppy-sijo agent_detect::tests:: --locked -- --test-threads=1`

Run: `CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-grok-session-target cargo test -p deppy-sijo --bin deppy-sijo agent_transcript::tests:: --locked -- --test-threads=1`

Expected: 추가 Grok 테스트와 기존 provider 회귀 PASS.

### Task 3: 실행 provider 변경 시 오래된 표시를 차단

**Files:**
- Modify: `crates/app/src/app.rs`

- [ ] **Step 1: stale provider RED 테스트를 추가한다**

Codex 표시가 저장된 pane에서 Grok이 실행되면 Grok argv의 model/effort만 남고 Codex context/summary가 사라지며, 이어 Claude가 실행될 때 Grok의 `xhigh`가 Claude `/effort` 계획으로 흐르지 않는 테스트를 추가한다.

- [ ] **Step 2: RED를 실행한다**

Run: `CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-grok-session-target cargo test -p deppy-sijo --bin deppy-sijo running_provider가_바뀌면_이전_provider_표시를_폐기한다 --locked -- --exact`

Expected: 기존 carry-forward가 이전 provider 값을 보존해 assertion FAIL.

- [ ] **Step 3: provider-aware 투영을 구현한다**

`display_for(kind, stored, running)`가 같은 kind에서만 transcript 값을 보존하고, 다른 kind에서는 현재 argv 값과 빈 활동 상태로 새 `AgentDisplay`를 만든다. 모든 provider match arm과 stable id를 Grok까지 완전하게 확장한다.

- [ ] **Step 4: GREEN과 전체 App 회귀를 실행한다**

Run: `CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-grok-session-target cargo test -p deppy-sijo --bin deppy-sijo --locked -- --test-threads=1`

Expected: 전체 App 테스트 PASS(명시적 ignored는 별도 집계).

### Task 4: 경계 검증, 리뷰, 게시

**Files:**
- Modify: `docs/CODEX_HANDOFF.md`

- [ ] **Step 1: 범위와 게이트를 검증한다**

Run: `CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-grok-session-target cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings`

Run: `cargo fmt --all -- --check`

Run: `cargo xtask check-boundary`

Run: `git diff --check`

Expected: 모두 exit 0. Relay/Fleet/file_tree/launcher/font/secret/package 파일이 diff에 없어야 한다.

- [ ] **Step 2: Codex CLI로 실제 소스 diff를 리뷰하고 지적을 반영한다**

Run: `codex review --uncommitted`

Expected: 확정 결함을 수정하고 관련 테스트를 다시 실행한다.

- [ ] **Step 3: 기록하고 게시한다**

`docs/CODEX_HANDOFF.md`와 Obsidian `프로젝트 일지/deppy-sijo/`에 실제 명령·결과·실패 접근·남은 GUI 검증을 기록한다. 한국어 Conventional Commit으로 커밋하고 `feat/grok-session-detection-main`을 push한 뒤 `main` 대상 Ready PR을 만든다. 앱 build/launch는 하지 않는다.

## Self-review

- #146의 Grok 감지·원문·표시 stale 차단을 Tasks 1~3이 각각 담당한다.
- 파일 트리, Relay, Fleet, dotenv 런처, CJK 폰트, Keychain, 패키징은 경계 검사에서 제외한다.
- 모든 코드 변경은 실제 RED 뒤에 이관하며 명령과 예상 결과를 구체적으로 적었다.
