# Agent Terminal Table Resize Implementation Plan

> **For agentic workers:** Use the available native sub-agent orchestration and execute the checkboxes task by task. The user explicitly selected parallel execution. Root reviews the behavioral contract and implementation separately before integration.

**Goal:** 에이전트 터미널 pane의 크기가 바뀔 때 원문을 보유한 CLI의 표가 새 너비로 재배치되도록 하고, 고정 출력 및 공급자별 한계를 정확히 검증한다.

**Architecture:** pane→tracked resize→runtime→PTY→CLI redraw와 CLI가 보유한 대화 원문의 렌더링을 별도로 검증한다. terminal renderer는 임의 ANSI 행을 표로 추측해 수정하지 않는다. 공급자 모드 변경은 실제 과거 표 재배치와 입력·스크롤 계약이 확인된 경로에만 적용한다.

**Tech Stack:** Rust, egui, Alacritty, portable-pty, existing runtime resize protocol, installed Claude/Codex/Grok CLI, macOS sandbox-exec, Python evidence runner.

---

## Basis and boundaries

- Base: `feat/agent-command-center-v2-20261020` /7584ed75 /0.15.1.
- Read `docs/qa/2026-10-22-agent-terminal-table-resize.md` before implementation. Claude classic old output stayed76cols at120cols with an onboarding dialog visible. Codex/Grok synthetic assistant tables reflowed. These facts do not establish the user's exact Codex root cause.
- No app restart, user-session input, paid model request, real credentials, persistent user CLI preference changes or main merge/push. No release artifact in this phase; a delivered artifact later needs a version bump and artifact version verification.
- Preserve exact session ownership, latest resize target, remote control fences, permissions, cursor, selection/copy and Korean text. No automatic Ctrl+L, Enter/prompt injection, guessing tables from VT cells, or Markdown document-tab substitute.
- PR means independently reviewable change group here; do not create a remote PR against main containing unrelated Command Center branch work.

## Parallel ownership

| Group | Worktree / branch | Files owned |
|---|---|---|
| PR1 resize | `/private/tmp/deppy-table-resize-20261022` / `fix/table-resize-resize-20261022` | `crates/app/src/ui/workspace.rs`, `crates/runtime/src/resize.rs`, related resize-only tests; exclude launcher and global handoff |
| PR2 Claude | `/private/tmp/deppy-table-claude-20261022` / `fix/table-resize-claude-20261022` | `crates/app/src/agent_launcher.rs`, Claude-specific tests and private probe/evidence; propose cross-layer contract to root before touching shared terminal/runtime files |
| PR3 harness | `/private/tmp/deppy-table-harness-20261022` / `fix/table-resize-harness-20261022` | `crates/session/tests/terminal_table_resize.rs`, new `scripts/qa/agent-terminal-table-resize/` runner, harness README/tests; exclude product resize/launcher |
| Root | integration worktree above | plan, final report/assets, cross-layer input/scroll contract if required, integrated tests/review |

Each agent uses `gpt-6.1-sol`, `xhigh`, its own Cargo target. Root-only integration; no agent may edit another group or root handoff. Agent notes use a separate named handoff/report in its own tree. Independent work is parallel; protocol-dependent edits wait for an agreed contract.

## PR1 — actual pane/runtime resize delivery

**Files:** inspect and narrowly edit `crates/app/src/ui/workspace.rs` tracked resize helpers; `crates/runtime/src/resize.rs` and actual dispatch call sites found with rg. Tests use the actual state machine and real shell PTY rather than a duplicate algorithm.

- [ ] Read `SessionView::prepare_tracked_resize`, `live_resize_step_pending`, stage/queue helpers, resize applied/fence event handlers, runtime resize authority checks and existing tests. Record the expected target after each event.
- [ ] Exercise actual state sequence: visible80×24 → drag42×20 while request80 is in flight → drag120×32 → old ACK/frame → final ACK/frame. Repeat with hidden→shown, pane split, and remote owner acquire/release. Assert that the final desired and applied dimensions are120×32, old acknowledgements cannot regress them and unrelated session IDs remain unchanged.
- [ ] Add an application-level fixture that reaches `Session::resize_checked` through runtime with a WINCH-aware shell. Assert child `stty size`, table border width and snapshot cols/rows at each stable width. Label any harness edge stubs precisely.
- [ ] For an observed contract failure, execute the failing regression first, fix the source of stale target/ack handling, rerun. If existing implementation passes, preserve regression coverage and report no proven resize defect; do not invent a production change.
- [ ] Run `cargo test --offline --locked -p deppy-sijo resize -- --test-threads=1`, affected runtime resize tests and strict lint for touched crate. Record counts and actual commands. Root reviews the minimal diff and reproduces the focused checks.

## PR2 — Claude past-table source rendering

**Files:** `crates/app/src/agent_launcher.rs`; existing provider launch tests in the same file. Test-owned profiles and conversations only. Cross-layer terminal wheel changes require root ownership/approval of the code contract, not a user approval gate.

- [ ] Resolve the test-owned Claude onboarding without enabling automatic approvals; then repeat classic/fullscreen with a table followed by40 paragraphs in a distinct turn,80→42→120→80cols. Inspect raw redraw and table borders, using application scroll for alternate screen. No terminal input in user sessions.
- [ ] Determine whether the confirmed old-table defect persists without the modal and whether fullscreen preserves/re-renders the original table. Run the same full mode fixture for PageUp/PageDown, wheel capability, Korean draft, selection/copy and returning to the latest prompt.
- [ ] If fullscreen solves the confirmed case, propose the exact launch capability rule and nonpersistent per-process flag/env change, plus the required wheel routing contract. Preserve explicit user CLI mode overrides and unsupported/remote CLI behavior; never silently retry a failed launch with broader permissions.
- [ ] Before product edits, add a failing launch-spec test showing the chosen supported Claude path uses the validated source renderer and does not alter Codex/Grok, model, effort or approval arguments. Implement the minimal launcher change only after the cross-layer scroll/input contract is ready. If exact CLI support or history behavior remains unproven, report the unsupported boundary and implement only proven diagnostics/coverage rather than force an unverified mode.
- [ ] Execute provider launch regression tests and strict lint. Root checks that saved user preferences, live sessions and permission flags are unchanged, then integrates after PR1/PR3 evidence and scroll/input checks.

## PR3 — reproducible three-provider table harness

**Files:** migrate existing `crates/session/tests/terminal_table_resize.rs` manual diagnostic; create `scripts/qa/agent-terminal-table-resize/` executable runner with fixture setup, snapshot analyzer and README. Keep normal deterministic shell characterization tests.

- [ ] Copy the existing investigation test into the agent worktree and inspect `/private/tmp/deppy-agent-table-probe-20261022` only as test-owned evidence. Do not read/copy user provider transcripts or credentials.
- [ ] Add failing runner/analyzer tests for child failure exit propagation, missing fixture output, frozen table after growth, cropped alternate-screen history, narrow identifier wraps, terminal padding and mixed Korean width. Freeze expected table-border measurements from actual synthetic evidence; a table visibility PASS must never imply resize PASS.
- [ ] Implement one fresh isolated profile/project per run and explicit installed provider binary selection; obtain versions, deny external IP traffic, strip inherited provider credentials, use dummy fixture auth and disable hooks/MCP/updaters/telemetry. Abort unsupported sandbox/platform instead of falling back to real networking. Do not repurpose the user's HOME or overwrite real CLI config.
- [ ] Generate synthetic resume history for each provider, run80→42→120→80 through real Session/PTY, capture bounded output/snapshots plus metadata, select native/application history correctly, and analyze measured borders separately from visibility. Failed child/analysis must produce a nonzero runner exit. Store source hashes and mode/version provenance.
- [ ] Run the normal Rust tests, Python analyzer/runner tests, strict test-target Clippy, and actual isolated Claude/Codex/Grok matrix. Expected current Claude classic old-table failure is an explicit known issue, not a passing responsiveness result. Root reviews and re-executes a representative fixture.

## Root integration / final acceptance

- [ ] Review each submitted commit: source-level root cause, RED/GREEN evidence, actual scope and no unrelated changes; request corrections before cherry-pick.
- [ ] Integrate PR3 and PR1 independently; integrate PR2 only with a proven source-rendering/input-scroll contract. Review cross-agent interfaces again after integration.
- [ ] Run the affected Rust suites, analyzer tests, strict lint and formatting on final source. Repeat three-provider matrix when relevant product/harness changes affect it; do not repeatedly run unchanged expensive checks.
- [ ] Check120/42/80 width, old and live assistant tables, arbitrary tool stdout preservation, two independent session IDs, rapid drag/split/hidden restore, remote authority, Korean draft, selection/copy, alternate/native scroll and last prompt reachability.
- [ ] Update `docs/CODEX_HANDOFF.md`, this checklist and QA report after meaningful changes/tests/failures. Report completed PR groups, actual remaining gaps and exact evidence. Do not call the overall bug fixed while the user's Codex case or mandatory history/input conditions remain unverified.

## Rollback

Keep every change group separate. Revert only its recorded integration commit(s); preserve original Command Center branch and other pre-existing docs. No resets, force pushes, application restarts or changes to user CLI files.
