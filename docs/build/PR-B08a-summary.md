# Build PR Summary

## Input Findings
- PR-R08 Finding 1: `App::logic()` always re-registered `request_repaint_after(500ms)` for approval polling, causing idle repaint work in an empty app.
- PR-R08 Finding 3: manual config values could set `performance.output_batch_ms` below the UI/documented 16ms lower bound.

## Scope
- Fixed only approval polling repaint behavior and `output_batch_ms` normalization.
- Did not touch folder tree 100k async listing, SQLite batching, queue/backpressure events, terminal dirty ranges, hidden pane render/snapshot policy, or unrelated UI/runtime boundaries.

## Changes
- Replaced unconditional approval repaint scheduling with an `ApprovalWatcher` thread that polls the approval table and calls `request_repaint()` only when the pending approval ID set changes.
- `App::logic()` now reads pending approvals only when the watcher sets a one-shot flag.
- Startup still performs one pending approval read so already-pending approvals can be shown.
- `output_batch_ms` normalization now clamps values below 16ms up to 16ms.

## Tests
- Added app tests for approval watcher behavior:
  - empty DB does not request repaint;
  - inserting a pending approval wakes the UI with an immediate repaint request.
- Added config test that `output_batch_ms = 1` normalizes to 16ms; updated existing `0` normalization expectation.
- Ran `cargo test -p deppy-sijo`: 60 passed, 0 failed.

## Risk Notes
- The approval watcher still performs low-frequency DB polling, but it no longer schedules recurring UI repaints when the pending set is unchanged or empty.
- Pending approval UI/resolve behavior remains unchanged; decisions still refresh the list immediately after a user action.

## Rollback Plan
- Revert `crates/app/src/app.rs` to the previous `logic()` polling block and remove watcher fields/tests.
- Revert `crates/app/src/config.rs` lower-bound normalization to the previous clamp if necessary.

## Follow-up Review Requests
- Review PR-B08a against PR-R08 Findings 1 and 3.
- Verify empty app frame stats no longer show approval-driven periodic repaint.
- Verify external `deppy-mcp-proxy` pending approvals still wake and display reliably.
