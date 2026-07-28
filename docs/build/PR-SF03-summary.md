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
- Added archive regressions for invalid metadata and truncated stream fallback preserving the original persistent UUID.
- Added `load_workspace_restore_bounded` cwd join regression.
- Added runtime restore-cwd regression using `/bin/pwd` output to verify shell spawn starts in the persisted `sessions.cwd`.

## Design Notes

- Archived row kind, exited status, agent id, and UUID remain preserved by `session_rebound_archived`.
- Cwd production behavior was already correct. No production cwd code was changed; only regression coverage was added for the SQL join and runtime shell spawn cwd.
- A `cargo fmt` run reformatted unrelated files. Those accidental out-of-scope formatting edits were reversed with a reverse patch, leaving only SF03-owned files modified.

## Test-First Evidence

- `cargo test -p runtime --locked archived_restore -- --test-threads=1`
  - First run failed to compile due a test helper returning `MuxSnapshot` while the event carried `Arc<MuxSnapshot>`.
  - Correction: changed the test helper return type to `Arc<MuxSnapshot>`.
- `cargo test -p runtime --locked archived_restore -- --test-threads=1`
  - Red result before production change: 2 tests run; 1 passed, 1 failed.
  - Failure: `archived_restore_invalid_metadata_preserves_persistent_row_for_fallback` found 2 session rows instead of 1.
- `cargo test -p runtime --locked archived_restore -- --test-threads=1`
  - Green result after production change: 2 passed; 0 failed; 209 filtered out.
- `cargo test -p persist --locked load_workspace_restore -- --test-threads=1`
  - Initial result after adding cwd loader test: 0 tests run because the filter did not match the test name.
  - Correction: renamed the regression to include `load_workspace_restore_bounded`.

## Final Gate Results

- `cargo test -p persist --locked load_workspace_restore -- --test-threads=1`
  - 1 passed; 0 failed; 28 filtered out.
- `cargo test -p runtime --locked restore_cwd -- --test-threads=1`
  - 1 passed; 0 failed; 210 filtered out.
- `cargo test -p runtime --locked restore -- --test-threads=1`
  - 7 passed; 0 failed; 204 filtered out.
- `cargo clippy -p persist -p runtime --all-targets --locked -- -D warnings`
  - Passed.
- `git diff --check`
  - Passed.

## Residual Risks

- No residual SF03-specific risk identified in the focused gates.
- The repository uses a linked worktree whose common `.git` directory is outside this sandbox's writable roots; index-writing git operations may require the coordinator environment.
