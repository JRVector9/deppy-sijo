# Archived Agent Resume Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make restored agent panes accurately resume an exact or recent CLI conversation, or clearly offer a new run when continuation is unsupported.

**Architecture:** Keep runtime wire types unchanged. Project bounded archived-agent metadata through the existing storage worker, join it to `PaneSnapshot::persistent_session_id` in App, derive an explicit resume strategy, and render/dispatch from that strategy. Harden generated shims so legacy saved launch specs resolve real Claude/Codex executables without their original launch-only environment.

**Tech Stack:** Rust 2024, rusqlite, egui/egui_kittest, existing AgentStateWorker, Cargo tests.

---

### Task 1: Explicit provider resume strategy

**Files:**
- Modify: `crates/app/src/agent_resume.rs`
- Modify: `crates/app/src/agent_launcher.rs`

- [x] **Step 1: Write failing pure-function tests**

Add tests asserting that `resume_plan(agent_id, binding)` returns exact arguments for matching Claude/Codex/Kimi/Qwen tokens, recent arguments without a token, and `Unsupported` for unknown or mismatched providers. Add invalid-token cases for empty, over-1024-byte, and ASCII-control input. Add a test for `AgentKind::from_stable_config_id`.

- [x] **Step 2: Verify RED**

Run: `cargo test -p deppy-sijo --locked agent_resume -- --nocapture`

Expected: compilation fails because `ResumePlan`, `ResumeMode`, `resume_plan`, and `from_stable_config_id` do not exist.

- [x] **Step 3: Implement the minimal strategy**

Define `ResumeMode::{Exact, RecentInCwd, Unsupported}`, a bounded `ResumePlan { mode, extra_args }`, provider mapping from stable built-in ID with a matching-binding fallback for custom configurations, and the verified argument table. Keep all I/O out of this module.

- [x] **Step 4: Verify GREEN**

Run: `cargo test -p deppy-sijo --locked agent_resume -- --nocapture`

Expected: all `agent_resume` tests pass.

### Task 2: Bounded archived metadata projection

**Files:**
- Modify: `crates/storage/src/db.rs`
- Modify: `crates/storage/src/lib.rs`

- [x] **Step 1: Write failing storage tests**

Create a workspace, built-in agent configuration, archived agent `sessions` row, and `mux_panes` row. Assert that an Agent State job with `include_agent_sessions` returns `ArchivedAgentResumeRow` with the persisted agent ID when no auxiliary binding exists, and adds the exact kind/token after inserting `agent_sessions`. Add a plus-one row-limit rejection test.

- [x] **Step 2: Verify RED**

Run: `cargo test -p storage --locked archived_agent_resume -- --nocapture`

Expected: compilation fails because the row and snapshot field do not exist.

- [x] **Step 3: Implement the bounded join**

Add `ArchivedAgentResumeRow`, bounded preflight/select SQL joining `sessions`, `mux_panes`, and `agent_sessions`, output materialization, retained-byte accounting, debug counts, and the public re-export. Reuse `AGENT_SESSION_ROWS_MAX`, ID/text ceilings, and the existing snapshot aggregate cap.

- [x] **Step 4: Verify GREEN**

Run: `cargo test -p storage --locked archived_agent_resume -- --nocapture`

Expected: all new projection tests pass.

### Task 3: Persistent-ID mapping and dispatch policy

**Files:**
- Modify: `crates/app/src/app.rs`
- Modify: `crates/app/src/ui/workspace.rs`

- [x] **Step 1: Write failing App mapping tests**

Add tests constructing mux panes with `persistent_session_id` and archived rows. Assert exact, recent, unsupported, unavailable, and checking presentations; ensure a row for a different persistent ID cannot affect a session. Assert dispatch policy returns exact/recent args, empty args only for unsupported, and no command for unavailable/checking.

- [x] **Step 2: Verify RED**

Run: `cargo test -p deppy-sijo --locked archived_resume -- --nocapture`

Expected: compilation fails because archived metadata state, presentation values, and mapping helpers do not exist.

- [x] **Step 3: Implement App integration**

Store the snapshot's archived rows keyed by persistent ID; derive installed availability from `agent_launcher_snapshot`; request detection when archived rows exist; republish presentations when restore/binding or detection completes; re-derive click policy before sending `RespawnArchivedAgent`. Preserve the existing missing-binding new-run fix and cross-workspace guard.

- [x] **Step 4: Verify GREEN**

Run: `cargo test -p deppy-sijo --locked archived_resume -- --nocapture`

Expected: all mapping and dispatch policy tests pass.

### Task 4: Accurate restored-pane copy

**Files:**
- Modify: `crates/app/src/ui/workspace.rs`
- Modify: `crates/i18n/locales/en-US/messages.txt`
- Modify: `crates/i18n/locales/ko-KR/messages.txt`
- Modify: `crates/i18n/locales/ja-JP/messages.txt`
- Modify: `crates/i18n/locales/zh-Hans/messages.txt`
- Modify: `crates/i18n/locales/zh-Hant/messages.txt`

- [x] **Step 1: Write failing kittest coverage**

Render restored panes for each presentation and assert visible helper/button text and enabled state. Exact/recent use the continue action; unsupported uses new run; unavailable/checking cannot emit a respawn request.

- [x] **Step 2: Verify RED**

Run: `cargo test -p deppy-sijo --locked restored_archived_resume -- --nocapture`

Expected: tests fail because the new localized helper keys and presentation-aware rendering are absent.

- [x] **Step 3: Implement presentation rendering**

Replace `resume_agent_kind` with presentation state, select localized helper text from the explicit mode/availability, use `add_enabled` for checking/unavailable, and keep the compact 22px overlay and local-owner safety condition.

- [x] **Step 4: Verify GREEN**

Run: `cargo test -p deppy-sijo --locked restored_archived_resume -- --nocapture`

Expected: all footer tests pass.

### Task 5: Legacy shim executable resolution

**Files:**
- Modify: `crates/app/src/agent_shim.rs`

- [x] **Step 1: Write a failing process-level shim test**

Generate/install shims under a temporary home, place a fake cmux `codex` in `$TMPDIR/cmux-cli-shims/...` and a recording real `codex` later in PATH, invoke the Deppy shim without `DEPPY_AGENT_EXECUTABLE`, and assert only the real executable receives one Deppy hook-flag set.

- [x] **Step 2: Verify RED**

Run: `cargo test -p deppy-sijo --locked transient_cmux_shim -- --nocapture`

Expected: the test fails because the generated shim selects the transient wrapper.

- [x] **Step 3: Harden the shared shim header**

Filter the Deppy shim directory and paths under `${TMPDIR%/}` before resolving `claude`/`codex`, preserving PATH order for all remaining entries and the recursion guard.

- [x] **Step 4: Verify GREEN**

Run: `cargo test -p deppy-sijo --locked transient_cmux_shim -- --nocapture`

Expected: the fake real executable runs and the transient wrapper does not.

### Task 6: Integrated verification and handoff

**Files:**
- Modify: `docs/CODEX_HANDOFF.md`

- [x] **Step 1: Run focused regressions**

Run the Task 1–5 focused commands again plus `cargo test -p runtime --locked respawn_archived_agent -- --nocapture`.

Expected: every focused test passes.

- [x] **Step 2: Run package checks**

Run: `cargo test -p storage --locked`, `cargo test -p deppy-sijo --locked`, `cargo check -p deppy-sijo --all-targets --locked`, `cargo fmt --all -- --check`, `cargo run -p xtask --locked -- i18n-check`, and `git diff --check`.

Expected: all commands pass; any pre-existing unrelated failure is recorded verbatim rather than called green.

- [x] **Step 3: Update handoff**

Record the objective, completed behavior, modified files, exact test results, failed attempts, remaining Kimi exact-binding limitation, and next commands in `docs/CODEX_HANDOFF.md`.
