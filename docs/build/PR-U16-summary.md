# PR-U16 Build Summary

## Input Findings
- PR-R04 Finding 2 / PR-R08 Finding 2: large folder-tree work must stay off the UI thread and preserve async listing token cancellation.
- PR-U16 plan: watcher debounce, ignore rules, batch events, minimal invalidation, `.env` change detection, and generated file storm prevention.
- PR-B03a/PR-B03b baseline: shell quoting, no auto-Enter, empty-root behavior, async listing chunks, and stale listing cancellation must not regress.

## Scope
- Limited implementation to `crates/app/src/ui/file_tree.rs`.
- Added focused folder-tree watcher tests only.
- Did not change hidden pane render/snapshot, runtime/session/storage/remote/i18n, or terminal copy/paste paths.

## Changes
- Replaced watcher channel payloads with typed `WatchEvent` values for dirty directory reloads and `.env*` change signals.
- Added default watcher ignore rules for `.git`, `node_modules`, `target`, `dist`, `build`, `.next`, `.turbo`, `vendor`, `logs`, `.cache`, and `.DS_Store`.
- Kept caller-provided watch ignore prefixes for app data/log paths.
- Preserved hidden path filtering, but exempted `.env`, `.env.*`, `.env-*`, and `.envrc` so env-warning candidates are recorded.
- Added `take_env_warning_candidates()` as the testable signal for future Project Environment warning UI.
- Coalesced dirty directory invalidation with ancestor/descendant deduplication.
- Limited watcher reload requests to 8 dirty directories per debounce window to avoid excessive listing invalidation in one frame.
- Skipped watch registration for expanded directories that match the default generated-path ignore rules.

## Tests
- `cargo test -p deppy-sijo file_tree` — passed, 27 tests.
- `cargo fmt --check` — passed.
- `cargo check --workspace --all-targets` — passed.

## Acceptance Criteria Check
- Default generated paths are ignored for watcher events: covered by unit tests.
- Watcher events remain debounce/batch processed: existing throttle test preserved; new per-batch invalidation cap test added.
- `.env` family changes are not hidden-filtered away: covered by classification and UI-state signal tests.
- Async listing/token behavior remains intact: existing stale root and collapse stale-result tests still pass.
- Existing shell quoting, empty-root, and no-auto-Enter invariants still pass under `file_tree` tests.

## Regression Risks
- The `.env*` signal is recorded in file-tree state but not yet surfaced in Project Environment UI.
- Default ignore rules apply to watcher activity, not a full `.gitignore` matcher.
- Ignored generated directories can still be manually listed if visible and expanded; they are not watched for storm events.

## Resource Impact
- Reduces callback-to-UI channel pressure by filtering generated paths before enqueueing.
- Dirty directory reloads are deduplicated and capped per debounce window.
- No new background thread beyond the existing notify watcher and async listing workers.

## Security Impact
- `.env*` file contents are never read or logged.
- Env file changes produce only path-level warning candidates.
- No secret/env persistence behavior changed.

## I18n/CJK Impact
- No user-facing localized strings were added.
- Existing CJK path quoting tests remain unchanged and passing.

## Rollback Plan
- Revert `crates/app/src/ui/file_tree.rs` changes from this PR to restore the prior `PathBuf` watcher channel and throttle-only reload behavior.
- Keep PR-B03a/B03b unchanged; this PR does not alter shell insertion, empty-root handling, or async listing core.

## Follow-up
- Wire `take_env_warning_candidates()` into Project Environment warning/notification UI.
- Add full `.gitignore`/global ignore matcher if PR-U05 ignore scope is expanded beyond watcher storm prevention.
- Add a release/perf-gate filesystem smoke with generated directory storm scenarios outside normal unit tests.
