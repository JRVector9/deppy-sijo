# PR-SF03 Restore Atomicity Summary

## Objective

Consume archived session persisted rows only after a complete read-only restore session and pane binding are ready.

## Changed Files

- `crates/runtime/src/in_process.rs`
- `crates/persist/src/repo.rs`
- `docs/build/PR-SF03-summary.md`

`crates/runtime/src/persistence.rs` did not require a production or comment change.

## Implementation

- Moved `session_rebound_archived` from the start of `restore_archived_pane` to the final commit point after archive metadata validation, bounded stream completion, or log-tail fallback session construction.
- Deferred `archived_on_disk` mutation until after successful persistence rebind and runtime session insertion, using a local `restored_from_disk` boolean to preserve whether the prepared session came from disk.
- Added archive regressions for invalid metadata and truncated stream fallback preserving the original persistent UUID.
- Added failed-rebind regression asserting a prepared disk archive does not leave a stale runtime disk marker when persistence rebind fails.
- Added `load_workspace_restore_bounded` cwd join regression.
- Added runtime restore-cwd regression using `/bin/pwd` output to verify shell spawn starts in the persisted `sessions.cwd`.

## Design Notes

- Archived row kind, exited status, agent id, and UUID remain preserved by `session_rebound_archived`.
- `archived_on_disk` is now committed only after the persistence row is rebound and the runtime session exists, so failed rebinds cannot leave marker-only runtime state.
- Cwd production behavior was already correct. No production cwd code was changed; only regression coverage was added for the SQL join and runtime shell spawn cwd.
- A `cargo fmt` run reformatted unrelated files. Those accidental out-of-scope formatting edits were reversed with a reverse patch, leaving only SF03-owned files modified.

## Test-First Evidence

- `cargo test -p runtime --locked archived_restore -- --test-threads=1`
  - First run failed to compile due a test helper returning `MuxSnapshot` while the event carried `Arc<MuxSnapshot>`.
  - Correction: changed the test helper return type to `Arc<MuxSnapshot>`.
- `cargo test -p runtime --locked archived_restore -- --test-threads=1`
  - Red result before production change: 2 tests run; 1 passed, 1 failed.
  - Failure: `archived_restore_invalid_metadata_preserves_persistent_row_for_fallback` found 2 session rows instead of 1.
- `cargo test -p runtime --locked archived_restore_failed_rebind_leaves_no_runtime_marker -- --test-threads=1`
  - Red result before production change: 1 test run; 0 passed, 1 failed.
  - Failure: `archived_restore_failed_rebind_leaves_no_runtime_marker` found a stale `archived_on_disk` marker after failed rebind.
- `cargo test -p runtime --locked archived_restore -- --test-threads=1`
  - Green result after production change: 2 passed; 0 failed; 209 filtered out.
- `cargo test -p runtime --locked archived_restore_failed_rebind_leaves_no_runtime_marker -- --test-threads=1`
  - Green result after root review fix: 1 passed; 0 failed; 211 filtered out.
- `cargo test -p persist --locked load_workspace_restore -- --test-threads=1`
  - Initial result after adding cwd loader test: 0 tests run because the filter did not match the test name.
  - Correction: renamed the regression to include `load_workspace_restore_bounded`.

## Final Gate Results

- `cargo test -p persist --locked load_workspace_restore -- --test-threads=1`
  - 1 passed; 0 failed; 28 filtered out.
- `cargo test -p runtime --locked archived_restore -- --test-threads=1`
  - 3 passed; 0 failed; 209 filtered out.
- `cargo test -p runtime --locked restore -- --test-threads=1`
  - 8 passed; 0 failed; 204 filtered out.
- `cargo clippy -p persist -p runtime --all-targets --locked -- -D warnings`
  - Passed.
- `git diff --check`
  - Passed.

## Residual Risks

- No residual SF03-specific risk identified in the focused gates.
- The repository uses a linked worktree whose common `.git` directory is outside this sandbox's writable roots; index-writing git operations may require the coordinator environment.
