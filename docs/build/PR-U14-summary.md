# PR-U14 Build Summary

## Input Findings

- PR-U14 requires explicit visible/hidden/exited terminal cache limits, default global terminal cache budget 128MB, hidden/exited cache recovery, and active visible preservation.
- Existing code already had hidden scrollback line cap and exited backend count LRU, but no byte accounting, cache footprint API, or testable trim/archive decision event.
- PR-R03/B02a hidden snapshot constraints remain non-regression requirements: hidden sessions must not create `TerminalViewportSnapshot`.
- PR-R08 identified terminal cache/RAM as medium risk: line count alone did not bound RAM for wide terminals or many sessions.

## Scope

- Changed PR-U14-owned terminal/session files:
  - `crates/terminal/src/backend.rs`
  - `crates/terminal/src/alacritty_backend.rs`
  - `crates/terminal/src/lib.rs`
  - `crates/session/src/session.rs`
- Minimal runtime integration in `crates/runtime/src/in_process.rs` only, tied to existing visibility reconciliation and exited LRU policy.
- Added this build summary at `docs/build/PR-U14-summary.md`.

## Changes

- Added terminal cache budget model:
  - global budget: 128MB
  - visible retained budget: 10,000 scrollback lines or 16MB
  - hidden retained budget: 1,000 scrollback lines or 2MB
  - exited retained budget: 1,000 scrollback lines or 2MB, with existing LRU archive/drop policy
- Added approximate cache footprint accounting based on terminal columns, visible rows, history lines, and alacritty cell/row size.
- `AlacrittyBackend` now applies effective scrollback limits at creation and on cache class transitions.
- `Session` now tracks current `TerminalCacheClass`, exposes footprint, records trim events, and reapplies budget after resize.
- Runtime now reconciles visible/hidden/exited cache class explicitly and preserves visible sessions before hidden/exited eviction.
- Existing exited LRU now also considers global estimated terminal cache bytes and archives non-visible exited backends first.
- Added tests for visible cap, hidden byte-budget trim event, session-level trim event tracking, and global exited archive decision.

## Tests

- `cargo test -p terminal` - pass, 30 tests.
- `cargo test -p session` - pass, 22 tests.
- `cargo test -p runtime exited_archive --lib` - pass, 2 tests.
- `cargo check --workspace --all-targets` - pass.
- Integrated verification after PR-U19 completed: `cargo fmt --check` - pass.
- Integrated verification after PR-U19 completed: `cargo test -p runtime remote` - pass, 44 tests.

## Acceptance Criteria Check

- visible/hidden/exited cache limits are explicitly tracked by `TerminalCacheClass`, `TerminalCacheBudget`, and `TerminalCacheFootprint`.
- hidden scrollback trim is testable through `TerminalCacheEvent`; exited/archive decision is testable through the runtime LRU/global-budget helper.
- active visible sessions are preserved before hidden/exited archive decisions.
- Existing terminal CJK/clipboard/path tests in `cargo test -p terminal` pass.

## Regression Risks

- Visible scrollback requests above PR-U14 policy are now capped by 10,000 lines or 16MB, whichever is lower.
- Approximate byte accounting is allocator-independent and intentionally conservative; it is a budget signal, not exact RSS.
- If total cache pressure is caused only by visible sessions, runtime logs/limits per visible budget but does not archive visible sessions.

## Resource Impact

- Hidden and exited retained sessions now shrink by line and byte budget instead of line count only.
- Global estimated terminal cache pressure can trigger non-visible exited backend archival before count LRU alone would.
- No new DB storage or background snapshot/render work was added.

## Security Impact

- No DB schema, secret, log redaction, or remote protocol changes were introduced by PR-U14.
- Terminal output remains process data; no new persistence path was added.

## I18n/CJK Impact

- CJK/wide character snapshot and selection tests continue to pass.
- Cache trimming operates on scrollback history limits and does not alter cell composition, width, or clipboard text extraction.

## Rollback Plan

- Revert the terminal cache budget types/methods and restore `AlacrittyBackend::set_visible` to the previous hidden line-cap-only behavior.
- Revert `Session` cache class/event fields and runtime `in_process` cache reconciliation/global-budget archive additions.
- Keep unrelated workspace changes untouched.

## Follow-up

- Add measured RSS/per-session process tree integration when PR-U12/U20 resource telemetry is finalized.
- Consider surfacing cache trim/archive telemetry to UI or diagnostics once the runtime event protocol has a planned compatibility update.
