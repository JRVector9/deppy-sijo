# Dotenv Shell Spawn Main Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** PR #146에서 런처 지연·중복 셸·전환 실패 후 잘못된 셸 생성에 해당하는 변경만 main에 독립 이관한다.

**Architecture:** App의 두 명령 전달 경로가 동일한 dotenv fast-path 판단을 사용한다. 이미 같은 runtime에 전달된 env 상태가 현재 파일 상태와 같고 startup/restore gate가 허용할 때만 shell/split이 worker를 생략한다. 새 workspace bootstrap은 실제 전환 성공 및 해당 workspace의 launcher 상태로 판정한다.

**Tech Stack:** Rust, egui, 기존 bounded dotenv worker 및 runtime command API.

---

승인된 작업은 현재 에이전트가 inline으로 실행한다. 기준은 exact main `45e66ccae313e653fc5dc6df46b80f791f231fc4`, 참고 source는 #146 `75bf2c9`다. 전체 cherry-pick은 하지 않는다. 수정 파일은 `crates/app/src/app.rs`, `crates/app/src/ui/agent_launcher.rs`, 이 계획 및 `docs/CODEX_HANDOFF.md`뿐이다. 앱 build/launch는 하지 않는다. Cargo는 `CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-deps-target-20260907`로 실행한다.

### Task 1: dotenv fast-path RED

- [x] app.rs에 `session_spawn_skips_dotenv_worker(command, delivered, current, creation_blocked)`의 false stub 및 회귀를 추가한다.
```rust
assert!(session_spawn_skips_dotenv_worker(&shell, Some(fresh), fresh, false));
assert!(!session_spawn_skips_dotenv_worker(&shell, None, fresh, false));
assert!(!session_spawn_skips_dotenv_worker(&shell, Some(fresh), fresh, true));
```
- [x] `cargo test -p deppy-sijo --bin deppy-sijo --locked launcher_spawn -- --test-threads=1`로 실제 RED를 확인한다.
- [x] shell/split, 전달 상태 일치, startup gate 통과의 교집합만 허용한다. agent/restore는 기존 경로를 유지한다.
```rust
!creation_blocked && matches!(command,
    runtime::RuntimeCommand::SpawnShell { .. } | runtime::RuntimeCommand::SplitPane { .. }
) && delivered == Some(current)
```
- [x] App wrapper가 정확한 runtime instance, 현재 root 상태, 기존 `startup_catalog_blocks_session_creation`을 전달하게 한다. controller Runtime과 `drain_workspace_protocol_intents`에 같은 조건을 연결해 focused GREEN을 확인한다.

### Task 2: workspace bootstrap 및 launcher RED→GREEN

- [x] `AgentLauncherUi::is_open_for` false stub와 닫힘/일치/다른 workspace/성공 후 닫힘 회귀를 작성한다.
- [x] `should_bootstrap_created_workspace_shell(created, switched, launcher_open)`을 기존 created만 반환하는 stub로 두고 launcher 예정/전환 실패/기존 workspace/일반 bootstrap 회귀 RED를 확인한다.
```rust
assert!(!should_bootstrap_created_workspace_shell(true, true, true));
assert!(!should_bootstrap_created_workspace_shell(true, false, false));
assert!(should_bootstrap_created_workspace_shell(true, true, false));
```
- [x] `self.open && self.workspace_id == workspace_id`, `created && switched && !launcher_open`으로 구현한다. `WorkspaceMutationPurpose::SwitchRuntime`에서 switch 호출 뒤 실제 `workspace_id == self.active.id`와 `is_open_for(&self.active.id)`를 전달한다.
- [x] launcher 및 관련 App focused 테스트 GREEN을 확인한다.

### Task 3: 검증·리뷰·게시

- [x] app 전체 테스트, app all-targets strict Clippy, boundary, fmt, diff check를 실행한다.
- [x] source diff의 bounded Codex CLI 리뷰를 실제 실행한다. 확정 finding은 RED→GREEN으로 수정하고 좁은 재리뷰한다. 300초 제한에 결론이 없으면 정확 PID만 종료하고 미완료 리뷰 사실을 기록한다.
- [x] handoff와 Obsidian `프로젝트 일지/deppy-sijo/`에 실제 결과를 기록하고 한국어 commit·일반 push한다.
- [x] `gh pr create --base main --head perf/dotenv-shell-spawn-main --body-file /private/tmp/deppy-dotenv-shell-spawn-pr.md`로 Ready PR을 생성한다. 원격 HEAD, checks, worktree clean을 확인한다. Actions billing 차단은 BLOCKED로 기록한다.
