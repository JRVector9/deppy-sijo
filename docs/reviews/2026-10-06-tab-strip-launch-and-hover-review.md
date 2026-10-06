# Tab-strip launch and title hover review — 2026-10-06

## Scope

Reviewed current uncommitted product changes on `feat/audit-nine-pr-v0.6.0-20261004` in `/Users/jr/Desktop/projects/deppy-sijo-performance`: fixed broadcast popup sizing/internal scrolling, full vacant tab-strip click target, exact-pane right-side shell/agent launch, asynchronous target/cancellation/environment gates, command bounds/redaction/retention and protocol compatibility. Initial snapshot: `/tmp/deppy-hover-review-initial-20261006.diff`.

Independent installed Codex CLI review used `gpt-6.1-sol` and `model_reasoning_effort=xhigh` with read-only sandbox, no Cargo/test/process/app/session actions. It completed successfully and identified one confirmed medium finding, matching the parent source inspection and owned runtime reproduction. Output: `/tmp/deppy-hover-review-initial-20261006.md`. No other confirmed findings in that snapshot. Parent subsequently reviewed the minimal correction and hover change.

## Confirmed finding, corrected

| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| medium | `crates/app/src/app.rs:27250`, `crates/runtime/src/in_process.rs:3923` (initial snapshot) | Targeted shell split did not activate its target tab | Tab B stayed visible while new right pane and focus were in tab A | Corrected; regression and full suites passed |

Trigger: choose Blank terminal from tab A's captured-pane launcher; switch to tab B while dotenv work is pending. The command correctly retains A's pane, but existing shell `split_pane` only focused the new pane. The newly added agent split already activates the target tab.

Correction: after a successful shell split, set `mux.window.active_tab` to the validated target tab before changing focus and emitting the mux snapshot. Rejection/spawn-failure paths do not activate a tab. No command payload or launch/environment policy changes.

Owned `/bin/cat` regression creates two tabs, submits an exact first-tab horizontal split while the second is active, and checks original session/layout preservation, right-pane position, active tab, focus, and no third tab. Actual RED `/tmp/deppy-shell-right-split-review-red-20261006.log`: 0 passed, 1 failed at the active-tab assertion. Actual targeted GREEN in `/tmp/deppy-tab-hover-review-full-gates-20261006.log`: 1 passed. Full affected validation completed successfully on the corrected source (see below).

## Requested title hover behavior

Session and auxiliary titles use the same stroke color as their tab line while the pointer is within their individual tab, including the nested close button. Unfocused panes use the actual divider fallback color. Attached pane titles use their source-workspace top-line color; a missing transparent identity retains the normal title. Sessionless strips follow the same rules. Layer-aware rectangle checks respect clipping/other windows. Leaving the tab restores existing selected/inactive colors. Existing click targets and close colors stay intact.

Actual pointer/painted-galley tests cover focused/unfocused panes, active/inactive auxiliary tabs, nested close hover, per-tab isolation, restoration, attached and sessionless headers, and absence of launch/runtime/detach actions. RED2 `/tmp/deppy-tab-hover-red2-20261006.log`: 0 passed, 3 failed with normal muted colors during hover. GREEN `/tmp/deppy-tab-hover-green-20261006.log`: 3 passed.

## Delivery status

Actual final `/tmp/deppy-tab-hover-review-full-gates-20261006.log` exit 0:

- App binary suite: 2,758 passed, 31 ignored, 0 failed (66.39s).
- Runtime suite: 353 passed, 0 ignored, 0 failed (52.08s); doc tests: 0.
- Strict App + Runtime all-target Clippy with `-D warnings`: passed.
- UI boundary and `cargo fmt --all -- --check`: passed.
- Final `git diff --check`: passed.

Deduplicated total: 3,111 passed, 31 ignored. Targeted hover/shell regressions are included in those full counts. Ignored tests were not executed. All Cargo operations used the required shared-target gate. No production changes after this complete batch; final documentation update only.

Source/test changes only. No live app launch/restart/stop, no user prompt sent, no artifact delivered, no commit/push, no version bump. Canonical and last shipped app version remain 0.7.4. A later changed app delivery requires a greater app version and a newly authorized restart.
