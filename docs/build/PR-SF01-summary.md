# PR-SF01 Build Summary

## Input Findings

- Warm/hidden `pending_events` retained replay-only lifecycle events without a hard cap.
- `workspace_git_label` used a process-global cache with 2s freshness but no path-count bound.

## Scope

- Modify only `crates/app/src/app.rs`.
- Bound replay retention to 1,024 events per workspace without moving notification handling after compaction.
- Preserve latest-wins mux/status/view/viewport replay semantics.
- Bound the workspace Git-label cache to 256 paths while preserving the existing 2s freshness window.

## Changes

- Added `PENDING_REPLAY_EVENT_CAP = 1_024` and `ReplayCompaction`.
- Changed `coalesce_mux_updated` to return overflow state, keep the latest mux first, drop stale session events absent from the latest mux, and cap replay events by dropping transient spawn acknowledgements before required state.
- Added `pending_replay_resync` to `WorkspaceRuntime`; warm/hidden compaction sets it on overflow, and successful Warm to Active delivery clears it.
- Replaced the raw global Git-label `HashMap` with `WorkspaceGitLabelCache`, including `last_accessed` refresh and oldest-access eviction at 256 entries.

## Tests

- RED: `cargo test -p deppy-sijo pending_replay --locked -- --test-threads=1`
  - Result: failed to compile as expected.
  - Evidence: 2 `E0609` errors because `coalesce_mux_updated` returned `()` and tests read `result.overflowed`.
- GREEN: `cargo test -p deppy-sijo pending_replay --locked -- --test-threads=1`
  - Result: passed, 3 passed, 0 failed, 917 filtered out.
- Regression correction: `cargo test -p deppy-sijo coalesce_ --locked -- --test-threads=1`
  - Result: failed after initial implementation, 6 passed, 2 failed.
  - Correction: existing test fixtures that claimed a mux introduced a session were changed to use a mux snapshot containing that live session.
- GREEN: `cargo test -p deppy-sijo coalesce_ --locked -- --test-threads=1`
  - Result: passed, 8 passed, 0 failed, 912 filtered out.
- RED: `cargo test -p deppy-sijo workspace_git_label_cache --locked -- --test-threads=1`
  - Result: failed to compile as expected.
  - Evidence: missing `WorkspaceGitLabelCache` and `WORKSPACE_GIT_LABEL_CACHE_CAP`.
- GREEN: `cargo test -p deppy-sijo workspace_git_label_cache --locked -- --test-threads=1`
  - Result: passed, 1 passed, 0 failed, 920 filtered out.
  - Correction after pass: marked the test-only `entry_count` helper with `#[cfg(test)]` to remove a dead-code warning.
- Focused: `cargo test -p deppy-sijo workspace_git_label --locked -- --test-threads=1`
  - Result: passed, 1 passed, 0 failed, 920 filtered out.
- Gate: `cargo test -p deppy-sijo coalesce_mux_updated --locked -- --test-threads=1`
  - Result: passed, 2 passed, 0 failed, 919 filtered out.
- Gate: `cargo test -p deppy-sijo pending_replay --locked -- --test-threads=1`
  - Result: passed, 3 passed, 0 failed, 918 filtered out.
- Gate: `cargo test -p deppy-sijo coalesce_ --locked -- --test-threads=1`
  - Result: passed, 10 passed, 0 failed, 911 filtered out.
- Gate: `cargo test -p deppy-sijo workspace_git_label --locked -- --test-threads=1`
  - Result: passed, 1 passed, 0 failed, 920 filtered out.
- Gate: `cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings`
  - Result: passed.
- Gate: `git diff --check`
  - Result: passed.

## Non-Gate Commands

- `cargo fmt --check`
  - Result: failed.
  - Reason: repo-wide formatting differences in many out-of-scope files, including `crates/app/src/alloc.rs`, `crates/app/src/claude_usage.rs`, `crates/runtime/src/in_process.rs`, and others. No formatting was applied because SF01 may edit only `crates/app/src/app.rs` and this summary.

## Acceptance Criteria Check

- [x] `pending_events` state-like events still use latest-wins coalescing.
- [x] Notifications remain processed before replay compaction.
- [x] Replay retained events are capped at 1,024 per workspace.
- [x] Overflow sets a replay-resync flag instead of silently relying on partial replay.
- [x] Hidden overflow does not set `event_resync_pending` or reactivate rendering.
- [x] Successful activation clears replay-resync state.
- [x] Git-label cache retains at most 256 paths and evicts the oldest accessed entry.
- [x] Existing 2s Git-label freshness is preserved.

## Unrun Broad Gates

- Full workspace test suite was not run; SF01 plan required focused app tests, Clippy for `deppy-sijo`, and `git diff --check`.

## Regression Risks

- If a replay overflow drops required non-transient events after the transient pass, the overflow flag relies on the next successful Warm to Active transition for the full runtime snapshot refresh.
- `cargo fmt --check` remains red for pre-existing out-of-scope formatting differences.

## Rollback Plan

- Revert `crates/app/src/app.rs` changes that add replay cap compaction, `pending_replay_resync`, and `WorkspaceGitLabelCache`.
- Remove this build summary.
