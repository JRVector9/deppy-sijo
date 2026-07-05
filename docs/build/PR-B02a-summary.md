# Build PR Summary
## Input Findings
- PR-R03 Finding 2: `MuxUpdated(active_tab=B)` 뒤에 hidden tab A의 stale `RuntimeEvent::Viewport(A)`가 오면 UI `view.snapshot` cache가 다시 살아날 수 있었다.
- PR-R03 Finding 3: remote delta server `last_sent`와 client `recon` baseline이 hidden 전환 뒤에도 마지막 `TerminalViewportSnapshot` Arc를 보존할 수 있었다.
- PR-R03 Finding 1은 정책 정규화만 반영했다. active tab visible panes는 계속 render/snapshot 대상이고, focused pane은 input/IME/scroll target이다.

## Scope
- Changed only PR-B02a owned implementation files:
  - `crates/app/src/ui/workspace.rs`
  - `crates/runtime/src/remote.rs`
- Added this summary: `docs/build/PR-B02a-summary.md`.
- Did not change `crates/session/src/session.rs`.

## Changes
- UI `Viewport` handling now stores snapshots only when the session is in the current mux active tab visible set.
- `MuxUpdated` handling uses explicit alive/visible session sets; hidden snapshots are pruned and stale hidden viewports cannot rehydrate the cache.
- Remote delta pump tracks the latest `MuxUpdated` visible session set and prunes server `last_sent` baselines to that set.
- Remote hidden trailing viewports after `MuxUpdated` are sent as plain full events without creating delta baselines.
- Remote client reconstruction prunes `recon` and pending keyframe state to the `MuxUpdated` visible session set.
- Added tests for stale hidden UI viewport filtering, split visible pane preservation, server baseline prune, and client recon prune.

## Tests
- `cargo test -p runtime muxupdated는_ --lib` - pass, 2 tests.
- `cargo test -p deppy-sijo muxupdated_뒤_hidden_viewport는_snapshot을_되살리지_않는다` - pass.
- `cargo test -p deppy-sijo active_tab_split의_visible_pane들은_viewport를_받는다` - pass.
- `cargo test -p runtime session_exited는_서버_last_sent_정리 --lib` - pass.
- `cargo test -p runtime` - pass, 75 tests.
- `cargo test -p mux` - pass, 9 tests.
- `cargo test -p deppy-sijo workspace` - pass, 4 tests.

## Risk Notes
- Re-visible hidden sessions may need a fresh keyframe/full viewport after prune, which is expected and bounded.
- UI no longer accepts hidden stale viewport snapshots; status/exit events still use `session_alive` so hidden tab state indicators are preserved.
- Existing split-pane behavior is explicitly covered so this does not become focused-only rendering.

## Rollback Plan
- Revert the `workspace.rs` visible-set filter and helper tests to restore prior UI cache behavior.
- Revert the `remote.rs` visible-set tracking/prune changes to restore prior remote baseline retention.
- No DB schema, public command API, or session API rollback is required.

## Follow-up Review Requests
- Review PR-B02a against PR-R03 Findings 2 and 3.
- Confirm hidden stale `Viewport` events cannot rehydrate UI cache after `MuxUpdated`.
- Confirm remote server `last_sent` and client `recon` do not retain hidden session baselines.
- Confirm active tab split panes still render/snapshot when not focused.
