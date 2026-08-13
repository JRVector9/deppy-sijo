# Agent Current Work Display Implementation Plan

**Goal:** Show agent work first, then the latest real user instruction, then project/folder
context in sidebar service-status rows, without promoting terminal telemetry into the headline.

**Scope:** PTY Claude, Codex, and Kimi rows only. Preserve existing lifecycle state and secondary
metadata. Do not rebuild, package, sign, relaunch, or visually verify the app in this phase.

### Task 1: Separate user instruction from agent progress

**Files:** `crates/app/src/agent_transcript.rs`, `crates/app/src/agent_detect.rs`,
`crates/app/src/agent_detect_worker.rs`, `crates/app/src/app.rs`

- [x] Add RED assertions for Claude, Codex, and Kimi user instructions.
- [x] Add `user_instruction` to the bounded transcript/display projection.
- [x] Exclude Claude internal injection and Kimi `system_trigger` records.
- [x] Preserve current-turn boundaries and redacted debug output.
- [x] Route the field through the existing detector projection and constructors.

### Task 2: Replace the terminal-text fallback

**File:** `crates/app/src/ui/workspace.rs`

- [x] Add RED priority assertions.
- [x] Select `last_agent_summary → user_instruction → project/folder → lifecycle`.
- [x] Remove terminal viewport summary from the agent headline input.
- [x] Reuse the existing immutable project-name snapshot and detected cwd.
- [x] Preserve status labels and the existing provider/model/effort/context line.

### Task 3: Verify without rebuilding the app

- [x] Run focused transcript, workspace, project-context, and worker tests.
- [x] Run `cargo check -p deppy-sijo --all-targets --locked`.
- [x] Run `cargo fmt --all -- --check` and `git diff --check`.
- [x] Complete uncommitted code review and address the reliable project-fallback finding.
- [x] Update `docs/CODEX_HANDOFF.md` with final evidence.

### Deferred until explicit user instruction

- App bundle/release rebuild
- Code signing and relaunch
- Visual verification against the running native UI
