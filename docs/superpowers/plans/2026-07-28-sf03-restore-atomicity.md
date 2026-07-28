# SF03 Restore Atomicity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Consume an archived session's persisted row only after a complete read-only session has been validated and prepared.

**Architecture:** Keep the existing restore format and fallback. Move the destructive `session_rebound_archived` commit from the beginning of `restore_archived_pane` to the final commit point immediately before inserting the prepared session/pane, and add regression-only cwd coverage for the existing SQL join path.

**Tech Stack:** Rust 2024, SQLite/rusqlite persistence, bounded scrollback archive reader, in-process runtime tests.

---

### Task 1: Add failing archive-order regressions

**Files:**
- Test: `crates/runtime/src/in_process.rs:6360`

- [ ] **Step 1: Add invalid archive metadata test**

Seed one persisted agent row and pane, write a syntactically readable archive whose metadata fails
`validate_host_command`, invoke full workspace restore, and assert the fallback shell remains bound to
the original persistent UUID instead of creating a second session row. The test must fail against
current ordering because `session_rebound_archived` removes the row before the shell fallback calls
`session_restored`.

- [ ] **Step 2: Add truncated stream test**

Write an archive header with a declared payload longer than the compressed stream. Assert no
half-bound runtime id remains and the persistent UUID is still used by the accepted fallback.

- [ ] **Step 3: Verify failure**

```bash
cargo test -p runtime --locked archived_restore -- --test-threads=1
```

Expected: at least the invalid-metadata row-preservation assertion fails.

### Task 2: Move rebind to the commit point

**Files:**
- Modify: `crates/runtime/src/in_process.rs:2374`
- Modify comments only if needed: `crates/runtime/src/persistence.rs:190`

- [ ] **Step 1: Remove the early destructive call**

Delete the call at the beginning of `restore_archived_pane`:

```rust
pipe.session_rebound_archived(id, persistent_id)
```

Do not change archive metadata or fallback behavior.

- [ ] **Step 2: Commit persistence immediately before runtime insertion**

After `let restored = ...` is fully constructed and before `self.sessions.insert`:

```rust
if let Some(pipe) = &mut self.persist
    && !pipe.session_rebound_archived(id, persistent_id)
{
    return false;
}
```

Only after that succeeds may the code insert into `sessions`, `exited_order`, and `mux.panes`.

- [ ] **Step 3: Run archive restore tests**

```bash
cargo test -p runtime --locked archived_restore -- --test-threads=1
cargo test -p runtime --locked restore -- --test-threads=1
```

Expected: PASS; no persistent row is consumed before a session exists.

### Task 3: Lock the valid cwd behavior with tests

**Files:**
- Test: `crates/persist/src/repo.rs:1204`
- Test: `crates/runtime/src/in_process.rs:6360`

- [ ] **Step 1: Add bounded loader regression**

Create a workspace/window/tab/pane whose session row contains a real temporary directory. Call
`load_workspace_restore_bounded` and assert:

```rust
assert_eq!(restore.window.unwrap().tabs[0].panes[0].cwd.as_deref(), Some(cwd));
```

- [ ] **Step 2: Add runtime spawn-cwd assertion**

Use the existing test PTY backend/resolver seam to restore the pane and assert the captured
`CommandSpec.cwd` equals the persisted directory. Do not change production cwd code unless this test fails.

- [ ] **Step 3: Run cwd tests**

```bash
cargo test -p persist --locked load_workspace_restore -- --test-threads=1
cargo test -p runtime --locked restore_cwd -- --test-threads=1
```

Expected: PASS on current production logic.

### Task 4: Validate and document SF03

**Files:**
- Create: `docs/build/PR-SF03-summary.md`

- [ ] **Step 1: Run required gates**

```bash
cargo test -p persist --locked load_workspace_restore -- --test-threads=1
cargo test -p runtime --locked restore -- --test-threads=1
cargo clippy -p persist -p runtime --all-targets --locked -- -D warnings
git diff --check
```

- [ ] **Step 2: Record the corrected cwd finding**

State explicitly that cwd production behavior was already correct unless the regression forced a minimal fix.

- [ ] **Step 3: Commit**

```bash
git add crates/runtime/src/persistence.rs crates/runtime/src/in_process.rs crates/persist/src/repo.rs docs/build/PR-SF03-summary.md
git commit -m "fix(runtime): commit archived restore atomically"
```
