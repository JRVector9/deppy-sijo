# SF01 App Retention Bounds Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound warm/hidden runtime replay and the process-global workspace Git-label cache without changing visible terminal/session behavior.

**Architecture:** Keep app ownership in `app.rs`. Extend the existing replay coalescer into a count-bounded admission helper, remember overflow in a dedicated replay-resync flag without waking hidden rendering, clear that flag only after the existing Warm→Active snapshot transition succeeds, and replace the raw static Git-label `HashMap` with a small LRU-like cache object that is testable without global state.

**Tech Stack:** Rust 2024, egui app state, existing `RuntimeEvent` snapshots, `HashMap`, `Instant`, crate-local unit tests.

---

Reference: `docs/superpowers/specs/2026-07-28-stability-pr-wave-design.md` §6 `PR-SF01`.

### Task 1: Add failing replay-cap regressions

**Files:**
- Modify: `crates/app/src/app.rs:19005`
- Test: `crates/app/src/app.rs:20875`

- [ ] **Step 1: Add exact cap constants and tests**

Add beside `coalesce_mux_updated`:

```rust
const PENDING_REPLAY_EVENT_CAP: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReplayCompaction {
    overflowed: bool,
}
```

Add tests that build `PENDING_REPLAY_EVENT_CAP + 1` lifecycle events using
`RuntimeEvent::SpawnFailed`, run the helper, and assert:

```rust
assert!(result.overflowed);
assert!(events.len() <= PENDING_REPLAY_EVENT_CAP);
assert_eq!(mux_tag(&events[0]), Some("latest"));
```

Also add a mixed mux/status/viewport/exit test proving the newest state for a live session survives.

- [ ] **Step 2: Run the new tests and verify failure**

Run:

```bash
cargo test -p deppy-sijo pending_replay --locked -- --test-threads=1
```

Expected: FAIL because the current helper returns `()` and retains every `SpawnFailed`.

### Task 2: Implement bounded replay compaction

**Files:**
- Modify: `crates/app/src/app.rs:16424`
- Modify: `crates/app/src/app.rs:16478`
- Modify: `crates/app/src/app.rs:19005`

- [ ] **Step 1: Change the helper contract**

Change:

```rust
fn coalesce_mux_updated(events: &mut Vec<runtime::RuntimeEvent>)
```

to:

```rust
fn coalesce_mux_updated(events: &mut Vec<runtime::RuntimeEvent>) -> ReplayCompaction
```

Retain the existing latest-wins passes. After them:

1. Derive the latest mux's live session set.
2. Remove stale lifecycle events for sessions absent from that mux.
3. If still over 1,024, remove oldest transient spawn/spawn-failure acknowledgements first.
4. If required state still exceeds the cap, keep the newest capped state and return
   `ReplayCompaction { overflowed: true }`.

The helper must always leave the latest mux at index 0 when one exists.

- [ ] **Step 2: Add a hidden-safe replay resync flag**

Add this field to `WorkspaceRuntime` and initialize it to `false`:

```rust
pending_replay_resync: bool,
```

At the warm and hidden-active compaction call sites, set only this new flag:

```rust
let compacted = coalesce_mux_updated(&mut rt.pending_events);
rt.pending_replay_resync |= compacted.overflowed;
```

Use the active equivalent for `self.active`. Do not set `event_resync_pending`: its current logic
immediately sends `Warm` then `Active`, which would incorrectly reactivate rendering while the app is
hidden. Do not move notification processing after compaction.

- [ ] **Step 3: Clear overflow after the existing full-snapshot transition**

At both existing activation paths—`switch_workspace` and the hidden-window
`want_active != render_active` transition—clear `pending_replay_resync` only when sending
`SetWorkspaceState(Active)` succeeds. The normal Warm→Active runtime transition already emits a full
mux and watched viewport snapshot, so do not add a second command pair or a new protocol command.

Add a pure/state regression proving overflow while hidden leaves `render_active == false`, retains the
flag, and the next successful activation clears it.

- [ ] **Step 4: Run replay tests**

Run:

```bash
cargo test -p deppy-sijo coalesce_ --locked -- --test-threads=1
cargo test -p deppy-sijo pending_replay --locked -- --test-threads=1
```

Expected: PASS; no retained vector exceeds 1,024.

### Task 3: Add bounded Git-label cache

**Files:**
- Modify: `crates/app/src/app.rs:5452`
- Test: `crates/app/src/app.rs:19149`

- [ ] **Step 1: Add failing pure cache tests**

Introduce a testable cache type and test 257 inserts:

```rust
const WORKSPACE_GIT_LABEL_CACHE_CAP: usize = 256;

struct WorkspaceGitLabelCache {
    entries: HashMap<String, WorkspaceGitLabelCacheEntry>,
}
```

The test must assert the first untouched key is evicted and the recently hit key remains.

- [ ] **Step 2: Verify failure**

Run:

```bash
cargo test -p deppy-sijo workspace_git_label_cache --locked -- --test-threads=1
```

Expected: FAIL until bounded insertion exists.

- [ ] **Step 3: Implement bounded insertion and access refresh**

Add `last_accessed: Instant` to `WorkspaceGitLabelCacheEntry`. Implement methods:

```rust
fn get_fresh(&mut self, path: &str, now: Instant) -> Option<Option<String>>;
fn insert(&mut self, path: String, label: Option<String>, now: Instant);
```

`insert` removes the entry with the oldest `last_accessed` while `len() >= 256`; replacing an
existing key must not evict another entry. Change the static to
`OnceLock<Mutex<WorkspaceGitLabelCache>>`.

- [ ] **Step 4: Run cache tests**

Run:

```bash
cargo test -p deppy-sijo workspace_git_label --locked -- --test-threads=1
```

Expected: PASS.

### Task 4: Validate and document SF01

**Files:**
- Create: `docs/build/PR-SF01-summary.md`

- [ ] **Step 1: Run required gates**

```bash
cargo test -p deppy-sijo coalesce_ --locked -- --test-threads=1
cargo test -p deppy-sijo workspace_git_label --locked -- --test-threads=1
cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
git diff --check
```

- [ ] **Step 2: Write the build summary**

Use the section order frozen in the design spec. Record exact counts and any unrun broad gate.

- [ ] **Step 3: Commit**

```bash
git add crates/app/src/app.rs docs/build/PR-SF01-summary.md
git commit -m "fix(app): bound warm replay retention"
```
