# PR-U12 Build Summary

## Input Findings

- PR-R08 reported missing app/child process CPU/RAM/RSS monitoring.
- Phase D requires low-cost sampling at 1-2 second cadence and no frame-by-frame refresh.
- No `sysinfo` dependency exists in the lockfile, so this PR avoids adding a new external dependency.

## Scope

- Add a runtime-owned app process CPU/RSS sampler.
- Emit low-cadence `RuntimeEvent::ResourceUsage` events.
- Keep UI display, child process tree aggregation, kill/restart/suspend controls, and workspace rollups out of this first slice.

## Changes

- Added `crates/runtime/src/resource_monitor.rs`.
- Added `ProcessResourceMonitor`, `ProcessResourceMonitorConfig`, and `ProcessResourceSnapshot`.
- CPU percent is calculated from process CPU time deltas, so the first sample has no CPU baseline.
- RSS is sampled from `/proc/self/statm` on Linux and `ps -o rss` on other Unix platforms.
- Runtime worker emits `ResourceUsage` at the configured interval, independent from UI frame cadence.
- Workspace UI ignores the event until PR-U25/global activity UI consumes it.

## Tests

- `cargo test -p runtime resource_monitor` - pass

## Acceptance Criteria Check

- [x] App process RSS/CPU sampling foundation exists.
- [x] CPU uses at least two samples before reporting a percentage.
- [x] Monitor is not refreshed per UI frame.
- [x] High CPU/RSS warning flags are represented in the snapshot.
- [ ] Session process tree RSS/CPU aggregation remains pending.
- [ ] UI display and kill/restart/suspend controls remain pending.

## Regression Risks

- Adding a `RuntimeEvent` variant requires UI/remote exhaustive matches to ignore or forward it correctly.
- macOS RSS sampling shells out to `ps` at low cadence; this is acceptable for the foundation but should be replaced if it shows measurable cost.

## Resource Impact

Adds one lightweight sample per interval in the runtime worker. No extra thread is introduced.

## Security Impact

No command args, env values, paths, or secret data are stored or logged by the sampler.

## I18n/CJK Impact

No user-facing strings are added.

## Rollback Plan

Remove the `resource_monitor` module, `RuntimeEvent::ResourceUsage`, and the worker sampler field/call.

## Follow-up

- Expose PTY child pid/process group through the pty/session boundary.
- Add session/workspace resource aggregation.
- Surface resource warnings in PR-U25 Global Activity View.
