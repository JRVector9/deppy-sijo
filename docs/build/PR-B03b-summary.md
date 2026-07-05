# Build PR Summary

## Input Findings
- PR-R04 Finding 2: root or expanded single-directory listing can block the UI thread when a directory has a very large number of direct children.
- PR-R08 Finding 2: `read_children()` synchronously enumerates and sorts all direct children; `show_rows` only virtualizes rendering, not listing cost.

## Scope
- Limited to `crates/app/src/ui/file_tree.rs` and focused tests.
- Kept PR-B03a shell quoting and empty-root behavior unchanged.
- Left tree-internal move semantics, gitignore matching, multi-path payloads, and PR-B04b paste/bracketed work out of scope.

## Changes
- Moved folder tree directory listing to background worker threads.
- Added root-level epoch and per-directory listing tokens so stale results from root switches, refreshes, and collapses are discarded.
- Added chunked result delivery (`LISTING_CHUNK_SIZE`) so UI-side tree application is bounded instead of applying a very large directory result in one message.
- Capped listing result application per frame so a backlog of chunks cannot be drained all at once on the UI thread.
- Preserved expanded/collapsed state across async refresh/reload by capturing expanded paths and re-requesting expanded child listings after parent results arrive.
- Kept watcher throttle and file operation refresh paths, but changed their directory reloads to enqueue async listings instead of synchronously calling `read_dir`.
- Added a small pending-listing spinner label for user feedback while a directory is loading.

## Tests
- Added synthetic async/stale tests for:
  - `set_root()` enqueues listing without applying directory contents synchronously, then discards stale results from the previous root.
  - collapsing a directory invalidates its in-flight listing so late results do not repopulate children.
- Updated existing watcher/partial reload tests to drain async listing results.
- Verification run:
  - `cargo test -p deppy-sijo file_tree`
  - `cargo test -p deppy-sijo`
  - `cargo check -p deppy-sijo --all-targets`
  - `cargo fmt --check`
  - `cargo check --workspace --all-targets`

## Risk Notes
- No real 100k-file CI smoke was added to avoid heavy filesystem setup in normal tests; coverage is synthetic nonblocking/stale cancellation plus chunked delivery.
- The worker still enumerates and sorts the full direct child set before chunking results, but that cost is off the UI thread.
- Applying many chunks still produces repeated flat rebuilds for very large directories; per-frame chunk capping limits frame cost, but the data model is not yet fully virtualized.

## Rollback Plan
- Revert this PR to restore synchronous `refresh()` / `toggle_dir()` / `reload_dir()` behavior.
- If only async listing races regress, remove the async listing channel/token path and keep unrelated PR-B03a shell quoting helpers intact.

## Follow-up Review Requests
- Review against PR-R04 Finding 2 and PR-R08 Finding 2.
- Verify root switching, manual refresh, watcher refresh, expand/collapse, and tree-internal move refreshes do not accept stale listing results.
- Consider a release/perf-gate 100k direct-child smoke outside normal unit tests.
