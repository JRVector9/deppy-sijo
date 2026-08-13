# Agent Work History B-Layout Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist recent agent work by user instruction and render it as an interactive `작업과 변경` page in the central terminal area while preserving the workspace sidebar.

**Architecture:** Add one bounded SQLite work-turn store, populate it from the existing off-thread transcript detector, and project only the active workspace into a dedicated `AgentTerminalView::History` leaf UI. Existing exact pane focus, native resume/new-run decisions, Git CLI helper, and `DiffPanelUi` remain the execution authorities; the new page only emits typed intents that App revalidates.

**Tech Stack:** Rust, egui, SQLite/rusqlite, existing `agent_detect_worker`, `agent_state_worker`, `git_cli`, `DiffPanelUi`, and five locale catalogs.

---

## Frozen product contract

- The left workspace/session and Files/Git/Notes sidebar remains visible. The rail `이력` button replaces only the central terminal content.
- The scope is the currently selected workspace. Switching a workspace while History is visible keeps History visible; selecting a concrete session returns to Terminal.
- One card is one real user instruction, not one process and not one project.
- Default order is running, waiting, then recently completed. Search covers instruction, agent summary, agent, model, effort, and branch.
- A card expands inline. There is no permanent list/detail split.
- `현재 세션으로 이동`, `이어서 실행`, and `새로 실행` reuse existing focus/resume/launcher paths and always revalidate identity at click time.
- Branch is a captured Git fact. `작업 폴더 변경 N` is the current working-tree count observed for that cwd; it is not presented as proof that the selected agent authored those files.
- `Git 변경 보기` opens the current bounded diff for the card's saved cwd. Historical per-turn patches and attribution are outside this release.
- The first release stores at most 256 work turns per workspace, extracts at most 24 recent turns per transcript, admits at most 24 mutations per worker job, and retains at most 4 MiB in any projected snapshot.
- First-release transcript ingestion covers Claude, Codex, and Kimi, whose transcript bindings already exist. Storage and card rendering accept a validated bounded provider ID so adding Grok or another parser later does not require a schema migration.

## PR dependency map

| PR | Deliverable | Depends on | Merge gate |
|---|---|---|---|
| PR 1 | Bounded durable work-turn store | — | Storage migration/API tests |
| PR 2 | Transcript turn extraction and persistence pipeline | PR 1 | Parser/worker/App pipeline tests |
| PR 3 | Rail entry and read-only central card page | PR 2 | UI/navigation/i18n tests |
| PR 4 | Focus, resume, and new-run actions | PR 3 | Exact identity/race tests |
| PR 5 | Branch, working-tree count, diff integration, final hardening | PR 4 | Full app quality gates |

Each PR updates `docs/CODEX_HANDOFF.md` with exact commands and actual results. Do not combine PRs before their individual merge gates pass.

---

## PR 1 — Persist a bounded work-turn catalog

**Branch:** `feat/agent-work-history-store`

**Files:**

- Modify: `crates/storage/src/db.rs`
- Test: `crates/storage/src/db.rs` test module
- Modify: `docs/CODEX_HANDOFF.md`

### Task 1: Add schema v35 and public bounded row types

- [ ] **Step 1: Write the failing migration and row-contract tests**

Add tests named `agent_work_history_v35_migrates_and_reopens`, `agent_work_history_upsert_is_idempotent_by_turn_key`, `agent_work_history_accepts_bounded_future_provider_id`, and `agent_work_history_debug_redacts_content`. Assert the table, recency index, exact primary key, reopen behavior, forward-compatible provider IDs, and absence of instruction/summary/cwd text in `Debug`.

```rust
#[derive(Clone, PartialEq, Eq)]
pub struct AgentWorkTurnRow {
    pub workspace_id: String,
    pub pane_id: String,
    pub kind: String,
    pub agent_session_id: String,
    pub turn_key: String,
    pub source_offset: u64,
    pub instruction: String,
    pub agent_summary: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub git_change_count: Option<u32>,
    pub state: AgentWorkTurnState,
    pub occurred_at: Option<i64>,
    pub updated_at: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentWorkTurnState {
    Working,
    Waiting,
    Completed,
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```bash
cargo test -p storage --locked agent_work_history -- --nocapture
```

Expected: compile failure because the migration and work-history types do not exist.

- [ ] **Step 3: Add migration 35**

Append one forward-only migration to `MIGRATIONS`:

```sql
CREATE TABLE agent_work_turns (
    workspace_id TEXT NOT NULL,
    pane_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    agent_session_id TEXT NOT NULL,
    turn_key TEXT NOT NULL,
    source_offset INTEGER NOT NULL CHECK (source_offset >= 0),
    instruction TEXT NOT NULL,
    agent_summary TEXT,
    model TEXT,
    effort TEXT,
    cwd TEXT,
    branch TEXT,
    git_change_count INTEGER CHECK (git_change_count IS NULL OR git_change_count >= 0),
    state TEXT NOT NULL CHECK (state IN ('working', 'waiting', 'completed')),
    occurred_at INTEGER,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, kind, agent_session_id, turn_key),
    FOREIGN KEY(workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE INDEX idx_agent_work_turns_workspace_recency
    ON agent_work_turns(workspace_id, updated_at DESC, source_offset DESC);
```

- [ ] **Step 4: Add bounded input and query contracts**

Add `AgentWorkTurnUpsert`, `AgentWorkHistoryMutation`, and `AgentWorkHistoryQuery` with these fixed limits:

```rust
pub const AGENT_WORK_TURNS_PER_WORKSPACE_MAX: usize = 256;
pub const AGENT_WORK_TURN_BATCH_MAX: usize = 24;
pub const AGENT_WORK_TURN_BATCH_BYTES_MAX: usize = 256 * 1024;
pub const AGENT_WORK_HISTORY_SNAPSHOT_BYTES_MAX: usize = 4 * 1024 * 1024;
```

Validate `kind` before SQL as a non-empty, at-most-64-byte lowercase stable provider ID containing only ASCII letters, digits, `-`, or `_`. Validate `source_offset <= i64::MAX` before converting it to SQLite INTEGER. Reject an entire mutation batch before opening its write transaction when any row violates these contracts.

Extend `AgentStateJob` with `work_turn_mutations: Vec<AgentWorkHistoryMutation>` and `include_work_history: bool`; extend `AgentStateSnapshot` with `work_turns: Vec<AgentWorkTurnRow>`. Upsert by the primary key, update mutable presentation fields, and prune oldest rows in the same transaction after a successful batch.

- [ ] **Step 5: Test rejection and pruning boundaries**

Add tests for exact-limit acceptance, limit-plus-one rejection before mutation, oversized strings/aggregate bytes, rollback on one invalid row, stable recency ordering, workspace isolation, and pruning to 256 rows.

- [ ] **Step 6: Run PR 1 gates**

```bash
cargo test -p storage --locked agent_work_history -- --nocapture
cargo test -p storage --locked smoke_db_migrations -- --nocapture
cargo check -p storage --all-targets --locked
cargo clippy -p storage --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Expected: all commands exit 0.

- [ ] **Step 7: Update handoff and commit**

```bash
git add crates/storage/src/db.rs docs/CODEX_HANDOFF.md
git commit -m "feat(storage): persist bounded agent work history"
```

---

## PR 2 — Extract recent user turns and persist them off-thread

**Branch:** `feat/agent-work-history-ingest`

**Files:**

- Modify: `crates/app/src/agent_transcript.rs`
- Modify: `crates/app/src/agent_detect.rs`
- Modify: `crates/app/src/agent_detect_worker.rs`
- Modify: `crates/app/src/agent_state_worker.rs`
- Modify: `crates/app/src/app.rs`
- Modify: `docs/CODEX_HANDOFF.md`

### Task 2: Extract stable, bounded provider turns

- [ ] **Step 1: Write RED provider fixtures**

Add fixtures/tests for Claude, Codex, and Kimi with two real prompts, internal/synthetic events between them, duplicate prompt text, a pending newest turn, and a completed older turn. Require distinct stable keys, correct instruction/summary pairing, newest-first output, and a hard 24-turn cap.

```rust
#[derive(Clone, PartialEq, Eq)]
pub struct TranscriptTurn {
    pub turn_key: String,
    pub source_offset: u64,
    pub instruction: String,
    pub agent_summary: Option<String>,
    pub occurred_at: Option<i64>,
    pub activity: AgentActivity,
}

pub struct TranscriptState {
    // existing fields remain
    pub recent_turns: Vec<TranscriptTurn>,
}
```

- [ ] **Step 2: Run parser tests and verify RED**

```bash
cargo test -p deppy-sijo --locked agent_transcript:: -- --nocapture
```

Expected: compile failure on the missing `recent_turns` contract.

- [ ] **Step 3: Preserve absolute offsets in bounded tail snapshots**

Replace raw tail strings internally with a snapshot that carries the absolute file offset. Generate fallback keys as `{provider}:{absolute_user_line_offset:x}`. Prefer a validated native provider turn ID when the event supplies one. Never hash or store instruction text as identity.

```rust
struct TailSnapshot {
    base_offset: u64,
    modified_at: Option<i64>,
    text: String,
}
```

Keep the existing regular-file, no-follow, nonblocking, UTF-8, line-count, and 256 KiB tail guards. A truncated first partial turn is discarded rather than paired with the wrong assistant message.

- [ ] **Step 4: Parse the recent turn window in one provider pass**

For each provider, pair a real user event with assistant messages until the next real user boundary. Continue using the existing internal-message and image-marker cleaning functions. The newest incomplete turn has `Working`; older observed turns have `Completed`. `TranscriptState.user_instruction` and `last_agent_summary` remain aliases of the newest turn so existing sidebar behavior does not regress.

### Task 3: Carry and store turns through existing workers

- [ ] **Step 1: Add RED worker-retention tests**

Extend `BindingPass` and `DetectOutcome` with `work_turns: Option<HashMap<SessionId, Vec<TranscriptTurn>>>`. Test latest-only mailbox merge behavior, session filtering, 24-per-session bounds, and retained payload rejection.

- [ ] **Step 2: Parse once during the binding pass**

Change `compute_activity_and_info` to return activity, display info, and recent turns from the same `agent_state(binding)` call. Do not reopen a transcript for work history.

- [ ] **Step 3: Add an exact work-history batch to `agent_state_worker`**

Add `ExactKind::WorkHistoryBatch` and `AppAgentStateExactKind::WorkHistoryBatch(Vec<storage::AgentWorkHistoryMutation>)`. Enforce 24 items and 256 KiB before staging. In `AppAgentStateBackend::execute_job`, copy the mutations into `storage::AgentStateJob.work_turn_mutations` and use the existing transaction/ack path.

- [ ] **Step 4: Convert detector turns to durable rows**

In `poll_agent_detect`, resolve the exact pane from the current mux and binding identity. Add model/effort from the existing merged display, cwd from `session_cwds`, and map only the newest live turn to Working; older turns are Completed. Keep a bounded `HashMap<(kind, agent_session_id, turn_key), projection_signature>` so unchanged polls do not write SQLite repeatedly. Clear the cache on workspace epoch change and agent-session identity change.

When an `Attention` projection changes `agent_needs_input`, update only the newest turn belonging to the exact current binding to Waiting or Working. A completion event or an idle transcript changes that same row to Completed. This hook-driven update is event-triggered; it must not create a second timer or a per-frame database write.

- [ ] **Step 5: Prove stale and duplicate events are safe**

Add App tests showing that an old detector epoch cannot persist into a new workspace, the same turn is an idempotent upsert, changed summary/state updates the same row, duplicate prompt text with different offsets remains two rows, and a disappeared binding cannot attach to a reused runtime session ID.

- [ ] **Step 6: Run PR 2 gates**

```bash
cargo test -p deppy-sijo --locked agent_transcript:: -- --nocapture
cargo test -p deppy-sijo --locked agent_detect_worker:: -- --nocapture
cargo test -p deppy-sijo --locked agent_work_history -- --nocapture
cargo test -p storage --locked agent_work_history -- --nocapture
cargo check -p deppy-sijo --all-targets --locked
cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Expected: all commands exit 0.

- [ ] **Step 7: Update handoff and commit**

```bash
git add crates/app/src/agent_transcript.rs crates/app/src/agent_detect.rs \
  crates/app/src/agent_detect_worker.rs crates/app/src/agent_state_worker.rs \
  crates/app/src/app.rs docs/CODEX_HANDOFF.md
git commit -m "feat(app): ingest agent work turns"
```

---

## PR 3 — Add the History rail entry and central card page

**Branch:** `feat/agent-work-history-page`

**Files:**

- Create: `crates/app/src/ui/work_history.rs`
- Modify: `crates/app/src/ui/mod.rs`
- Modify: `crates/app/src/ui/agent_terminal.rs`
- Modify: `crates/app/src/ui/file_tree.rs`
- Modify: `crates/app/src/agent_state_worker.rs`
- Modify: `crates/app/src/app.rs`
- Modify: `crates/i18n/locales/en-US/messages.txt`
- Modify: `crates/i18n/locales/ko-KR/messages.txt`
- Modify: `crates/i18n/locales/ja-JP/messages.txt`
- Modify: `crates/i18n/locales/zh-Hans/messages.txt`
- Modify: `crates/i18n/locales/zh-Hant/messages.txt`
- Modify: `docs/CODEX_HANDOFF.md`

### Task 4: Project history only when requested

- [ ] **Step 1: Write RED projection tests**

Add `AgentStateSection::WorkHistory` to the fixed latest-only section array and `AppAgentStateProjectionKind::WorkHistory`. Tests must show at most one pending History projection, workspace-epoch rejection, and no work-history query for Terminal/Home/Fleet frames.

- [ ] **Step 2: Add the projection path**

When entering History, changing workspace while History is visible, or pressing refresh, stage one projection with `storage_job.include_work_history = true`. Store the resulting immutable rows in App only after scope/revision validation. Keep the last successful snapshot visible while a refresh is pending or fails.

### Task 5: Render the approved B-layout page

- [ ] **Step 1: Write RED pure UI-model tests**

Create `ui/work_history.rs` with pure tests for status ordering, recency tie-break, case-insensitive search, status filtering, selection toggle, missing optional metadata, branch omission, empty state, and snapshot replacement that preserves selection only when the identity remains.

```rust
pub struct WorkHistorySnapshot<'a> {
    pub workspace_name: &'a str,
    pub current_branch: Option<&'a str>,
    pub rows: &'a [storage::AgentWorkTurnRow],
    pub loading: bool,
    pub error: Option<WorkHistoryErrorCode>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct WorkTurnIdentity {
    pub workspace_id: String,
    pub kind: String,
    pub agent_session_id: String,
    pub turn_key: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkHistoryErrorCode {
    Busy,
    InvalidData,
    ResourceLimit,
    ReadFailed,
}

pub enum WorkHistoryAction {
    Refresh,
}

pub struct WorkHistoryUi {
    query: String,
    filter: WorkHistoryFilter,
    selected: Option<WorkTurnIdentity>,
}
```

- [ ] **Step 2: Implement the card surface**

Render header, search, four filter chips, count, and a single bounded vertical list. Each card renders agent badge, instruction, relative time, work/last-response line, model/effort, optional branch, optional working-tree count, and status dot. Expand only the selected card to show first instruction and latest work summary. Use egui IDs derived from the full durable identity, never display/internal numeric runtime IDs.

- [ ] **Step 3: Add `AgentTerminalView::History` and rail navigation**

Add `SidebarAction::ShowHistory` and `NavIcon::History`; place the row between Fleet (`작업`) and Agents (`AI`). Clicking it toggles History ↔ Terminal. Add locale keys for the rail, header, filters, states, empty/loading/error messages, card labels, and refresh action in all five catalogs.

- [ ] **Step 4: Preserve sidebar and runtime semantics**

Replace negative view logic with explicit terminal ownership:

```rust
let history_visible = central_view == AgentTerminalView::History;
let terminal_visible = central_view == AgentTerminalView::Terminal;
let information_visible = home_visible || fleet_visible || history_visible;
```

Call `workspace_ui.update_hidden` for all information views. Do not render panes or composer in History. A workspace row click preserves History and refreshes its scoped projection; a live/persisted session row click sets Terminal before the existing activation path.

- [ ] **Step 5: Run PR 3 gates**

```bash
cargo test -p deppy-sijo --locked ui::work_history:: -- --nocapture
cargo test -p deppy-sijo --locked ui::file_tree::tests:: -- --nocapture
cargo test -p deppy-sijo --locked agent_terminal:: -- --nocapture
cargo test -p i18n --locked -- --nocapture
cargo check -p deppy-sijo --all-targets --locked
cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Expected: all commands exit 0.

- [ ] **Step 6: Update handoff and commit**

```bash
git add crates/app/src/ui/work_history.rs crates/app/src/ui/mod.rs \
  crates/app/src/ui/agent_terminal.rs crates/app/src/ui/file_tree.rs \
  crates/app/src/agent_state_worker.rs crates/app/src/app.rs \
  crates/i18n/locales docs/CODEX_HANDOFF.md
git commit -m "feat(ui): show workspace agent work history"
```

---

## PR 4 — Connect exact focus, resume, and new-run actions

**Branch:** `feat/agent-work-history-actions`

**Files:**

- Modify: `crates/app/src/ui/work_history.rs`
- Modify: `crates/app/src/ui/agent_launcher.rs`
- Modify: `crates/app/src/app.rs`
- Modify: all five `crates/i18n/locales/*/messages.txt`
- Modify: `docs/CODEX_HANDOFF.md`

### Task 6: Resolve presentation without granting the leaf authority

- [ ] **Step 1: Write RED action-resolution tests**

Cover these exact cases: matching live binding → Focus; stopped live shell with matching saved native session → Resume; archived exact/recent-cwd → Resume; unsupported or missing native metadata with installed provider → NewRun; unavailable provider → Disabled; stale pane/session/workspace → RejectAndRefresh.

```rust
pub enum WorkHistoryPrimaryAction {
    Focus,
    Resume,
    NewRun,
    Disabled(WorkHistoryDisabledReason),
}

pub enum WorkHistoryAction {
    Refresh,
    Activate(WorkTurnIdentity),
}
```

App builds `WorkHistoryPrimaryAction` from current mux, `agent_bindings`, persisted resume rows, `archived_resume_targets_from_mux`, and the installed-agent snapshot. The UI receives presentation only and returns the durable work-turn identity.

- [ ] **Step 2: Add buttons and honest unsupported text**

Expanded cards show exactly one primary action and later the Git secondary action. Labels are `현재 세션으로 이동`, `이어서 실행`, or `새로 실행`. Disabled cards show why resume/new run is unavailable. Do not show `이어서 실행` for unsupported providers.

- [ ] **Step 3: Revalidate and dispatch through existing paths**

On activation, App recomputes the resolution from current state:

- Focus sends the existing exact `FocusSession` controller action and changes to Terminal only after admission.
- Resume uses the existing `ResumeAgent` probe for a live shell or `dispatch_respawn_archived_agent` for an archived pane.
- NewRun adds `AgentLauncherUi::open_for_kind(workspace_id, workspace_name, kind, snapshot)` and opens the existing launcher with the provider preselected; model, effort, approval settings, and dotenv flow remain launcher-owned.
- RejectAndRefresh stays in History, displays an inline stale-state message, and requests a fresh WorkHistory projection.

- [ ] **Step 4: Test races and input ownership**

Add tests for workspace switch between render/click, reused runtime session ID, changed pane binding, full controller slot, resume-probe backpressure, missing installed agent, and successful focus returning terminal input ownership to the exact pane. No History click may write PTY bytes directly.

- [ ] **Step 5: Run PR 4 gates**

```bash
cargo test -p deppy-sijo --locked work_history_action -- --nocapture
cargo test -p deppy-sijo --locked archived_resume -- --nocapture
cargo test -p deppy-sijo --locked agent_launcher:: -- --nocapture
cargo test -p deppy-sijo --locked first_session -- --nocapture
cargo check -p deppy-sijo --all-targets --locked
cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Expected: all commands exit 0.

- [ ] **Step 6: Update handoff and commit**

```bash
git add crates/app/src/ui/work_history.rs crates/app/src/ui/agent_launcher.rs \
  crates/app/src/app.rs crates/i18n/locales docs/CODEX_HANDOFF.md
git commit -m "feat(app): activate agent work history entries"
```

---

## PR 5 — Add branch/change context, diff reuse, and final hardening

**Branch:** `feat/agent-work-history-git`

**Files:**

- Create: `crates/app/src/agent_work_git.rs`
- Modify: `crates/app/src/main.rs`
- Modify: `crates/app/src/git_cli.rs`
- Modify: `crates/app/src/ui/work_history.rs`
- Modify: `crates/app/src/ui/diff_panel.rs`
- Modify: `crates/app/src/app.rs`
- Modify: all five `crates/i18n/locales/*/messages.txt`
- Modify: `docs/CODEX_HANDOFF.md`

### Task 7: Collect branch and working-tree counts off-thread

- [ ] **Step 1: Write RED Git parser and resource tests**

Test normal branch, upstream/ahead-behind suffix, detached HEAD, unborn branch, non-repository, rename status, spaces/newlines in filenames via `-z`, exact 16-cwd admission, limit-plus-one rejection, output cap, timeout cleanup, stale generation, and no command execution during egui render.

```rust
pub const WORK_HISTORY_GIT_CWDS_MAX: usize = 16;
pub const WORK_HISTORY_GIT_OUTPUT_MAX: usize = 64 * 1024;

pub struct WorkHistoryGitFact {
    pub cwd: String,
    pub branch: Option<String>,
    pub changed_files: Option<u32>,
}
```

- [ ] **Step 2: Implement one bounded Git command per distinct cwd**

Use the existing absolute Git helper from a background worker:

```text
/usr/bin/git -C <cwd> status --porcelain=v1 --branch -z --untracked-files=normal
```

Parse the first `##` record as branch and count logical status entries. In porcelain-v1 `-z` output, rename/copy entries carry a second path record; consume that path without incrementing the file count twice. Detached HEAD and invalid output produce `branch = None`; a non-repository produces no fact. Deduplicate cwds before spawning and retain only one latest generation.

- [ ] **Step 3: Persist facts without rewriting instruction identity**

On first observation, manual refresh, or cwd change, merge the fact into the newest matching work turn using the existing exact history batch. Store `branch` and `git_change_count`; do not overwrite a non-empty captured branch on a later checkout unless the user explicitly refreshes that work item.

### Task 8: Reuse the existing diff panel for saved cwd

- [ ] **Step 1: Generalize the diff target**

Change `DiffPanelUi::open_for` internals to accept `session: Option<runtime::SessionId>` and add `open_for_path(ctx, workspace_id, cwd, title)`. Keep the same `DiffPathPayload`, generation, capacity-one intent, host worker, clipping, and errors. Existing session callers continue through a wrapper and preserve behavior.

- [ ] **Step 2: Add `Git 변경 보기`**

Extend `WorkHistoryAction` with `ShowDiff(WorkTurnIdentity)`. App re-reads the row from the current snapshot, validates the workspace and saved cwd, and calls `open_for_path`. Label the count `작업 폴더 변경 N` with a tooltip explaining it is the current working-tree snapshot, not agent attribution.

- [ ] **Step 3: Finish accessibility and responsive behavior**

Verify keyboard focus order, Enter/Space card expansion, visible focus rings, non-color status marks, ellipsis for long branch/model text, narrow layout stacking, scroll preservation, locale overflow, and no secret content in `Debug` or logs. Remove the mock-only explanatory callout.

- [ ] **Step 4: Run focused and full gates**

```bash
cargo test -p deppy-sijo --locked agent_work_git:: -- --nocapture
cargo test -p deppy-sijo --locked ui::work_history:: -- --nocapture
cargo test -p deppy-sijo --locked ui::diff_panel:: -- --nocapture
cargo test -p storage --locked agent_work_history -- --nocapture
cargo test -p deppy-sijo --locked
cargo check -p deppy-sijo --all-targets --locked
cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Expected: all commands exit 0. Record exact passed/ignored counts in `docs/CODEX_HANDOFF.md`; do not summarize an interrupted or filtered-zero run as passing.

- [ ] **Step 5: Manual acceptance without changing data**

Use a temporary app data directory or dedicated fixture workspace. Verify Claude, Codex, and Kimi cards; duplicate prompt identity; app restart persistence; workspace switch while History remains open; search/filter; live focus; exact resume; unsupported new run; branch omission outside Git; current diff opening; and immediate terminal typing after focus. Packaging/relaunch of the user's normal app requires a separate explicit instruction.

- [ ] **Step 6: Update handoff and commit**

```bash
git add crates/app/src/agent_work_git.rs crates/app/src/main.rs crates/app/src/git_cli.rs \
  crates/app/src/ui/work_history.rs crates/app/src/ui/diff_panel.rs crates/app/src/app.rs \
  crates/i18n/locales docs/CODEX_HANDOFF.md
git commit -m "feat(ui): add git context to agent work history"
```

---

## Review and landing rules

1. Review each PR against its own base and merge gate; do not review the five-PR stack only as one large diff.
2. PR 1 and PR 2 require storage/resource-boundary review. PR 3 requires visual and input-ownership review. PR 4 requires lifecycle/race review. PR 5 requires process cleanup, path validation, and wording review.
3. Never claim that a changed file was authored by a specific agent unless a future audited attribution mechanism exists.
4. No new polling loop, unbounded channel, per-frame SQLite/Git/transcript I/O, raw transcript persistence, or direct runtime command from a leaf UI is allowed.
5. After the final PR is green, update the design journal and only package/relaunch when the user explicitly asks.
