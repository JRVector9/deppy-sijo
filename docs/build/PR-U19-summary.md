# PR-U19 Build Summary

## Input Findings

- PR-U19 requires remote/browser slow-consumer backpressure so one slow client cannot stall the runtime.
- PR-R08 Finding 9 identified plain remote command writes as a blocking risk.
- PR-B02a requires remote delta baselines to stay pruned by visible sessions and not retain hidden pane snapshots.

## Scope

- Changed `crates/runtime/src/remote.rs`.
- Added this summary in `docs/build/PR-U19-summary.md`.
- Updated `docs/update/update-findings-summary.md` status for PR-U19.
- `cargo fmt` also normalized existing `crates/runtime/src/lib.rs` resource-monitor ordering in the current worktree; PR-U19 behavior is in `remote.rs`.
- Did not change wire message variants or protocol payload shape.

## Changes

- Added a per-connection outbound queue for server-to-client remote events:
  - durable FIFO queue for lifecycle/status/control events,
  - session-keyed viewport slots for latest-wins coalescing,
  - hard cap on durable events with slow-client disconnect instead of silent drop.
- Changed plain and TLS server pumps to drain runtime receiver events into the bounded queue even when socket writes are backpressured.
- Limited per-tick outbound writes so a large backlog does not starve read-side command processing.
- Kept terminal viewport/delta frames droppable/coalescible while preserving status/lifecycle events up to the bounded durable cap.
- Replaced unbounded exited-session tracking with bounded tombstones so trailing exit viewports cannot recreate delta baselines.
- Added plain transport write timeouts for server event writes and client command writes so plain sockets cannot block indefinitely.
- Adjusted heartbeat/liveness test to accept valid low-frequency status events, including `ResourceUsage`, before a heartbeat frame.

## Tests

- `cargo fmt --check` - pass
- `cargo test -p runtime remote` - pass
- `cargo check --workspace --all-targets` - pass

## Acceptance Criteria Check

- [x] Slow remote client outbound memory is bounded by per-connection durable and viewport caps.
- [x] Viewport/delta events can be coalesced or dropped under pressure.
- [x] Status/lifecycle events are not silently dropped; durable overflow disconnects the slow client.
- [x] Existing TLS slow-consumer command-processing tests remain passing.
- [x] Existing disconnect/reconnect and cleanup tests remain passing.
- [x] Hidden pane snapshot/baseline pruning tests remain passing.

## Regression Risks

- A client that stops reading durable status/lifecycle events is disconnected once its bounded durable queue fills.
- Viewport slot eviction under extreme numbers of visible sessions may require a reconnect/keyframe to recover freshest display state.
- Plain transport now times out slow writes instead of waiting indefinitely.

## Resource Impact

- Per remote client memory is bounded by `OUTBOUND_DURABLE_QUEUE_CAP` plus `OUTBOUND_VIEWPORT_SLOT_CAP`.
- Server pumps keep draining runtime receiver channels during socket backpressure, reducing unbounded per-subscriber buildup.

## Security Impact

- No new remote exposure or auth change.
- Wire validation and localhost/default TLS policies are unchanged.

## I18n/CJK Impact

- No text rendering or terminal cell width behavior changed.
- CJK path/copy/paste paths are not touched.

## Rollback Plan

- Revert `crates/runtime/src/remote.rs` to the prior direct pump drain/write behavior.
- Revert this summary and the PR-U19 status row in `docs/update/update-findings-summary.md`.

## Follow-up

- Add a user-visible remote degraded/backpressure indicator when the UI notification/event contract is expanded.
- Revisit durable queue sizing after PR-U20 full slow remote scenario measurements.
