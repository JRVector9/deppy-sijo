# PR-U05b Build Summary

## Input Findings

- PR-U05 remained partial: folder tree listing and watcher had generated-file
  default ignores, but not `.gitignore`, `.git/info/exclude`, or global excludes.
- Code check confirmed listing used plain `read_dir`, while watcher only checked
  default names and caller-provided ignore prefixes.

## Scope

- Add a lightweight cached ignore matcher for folder tree listing and watcher
  events.
- Avoid scanning the whole workspace up front.
- Do not change folder tree DnD path insertion or terminal paste semantics.

## Changes

- Added shared `GitIgnoreCache` for `FileTreeUi`.
- Root changes reset the cache and load:
  - workspace `.gitignore`
  - workspace `.git/info/exclude`
  - common global git excludes (`$XDG_CONFIG_HOME/git/ignore`,
    `$HOME/.config/git/ignore`, `$HOME/.gitignore_global`)
- Directory listing workers apply ignore rules before emitting `TreeNode`s.
- Nested `.gitignore` files are loaded lazily per expanded/listed directory and
  cached by directory path.
- Watcher event filtering uses the same cache, so ignored paths do not schedule
  repaint/reload work.
- Existing generated-file defaults and caller-provided ignore prefixes remain.

## Tests

- Preflight before implementation:
  - `cargo test -p deppy-sijo file_tree` - pass
  - `cargo run -p xtask -- perf-smoke` - pass
- After implementation:
  - `cargo fmt --check` - pass
  - `cargo check --workspace --all-targets` - pass
  - `cargo clippy --workspace --all-targets` - pass
  - `cargo test -p deppy-sijo gitignore` - pass
  - `cargo test -p deppy-sijo file_tree` - pass
  - `cargo run -p xtask -- perf-smoke` - pass

## Acceptance Criteria Check

- [x] Folder tree listing respects root `.gitignore`.
- [x] Folder tree listing respects `.git/info/exclude`.
- [x] Nested `.gitignore` rules are applied lazily for expanded directories.
- [x] Watcher events under ignored paths are discarded before repaint scheduling.
- [x] No eager full-workspace scan was added.
- [x] Existing path DnD and no-auto-enter behavior is unchanged.

## Regression Risks

- Medium. The matcher is intentionally lightweight and does not implement every
  advanced gitignore edge case such as bracket character classes.
- Rules are cached per directory; editing `.gitignore` is observed by watcher as
  a file event, and the next root refresh/reopen resets the cache.

## Resource Impact

- No recursive workspace scan.
- Ignore files are read only for the root/global set and for directories that
  are actually listed.
- Watcher firehose is reduced for ignored paths.

## Security Impact

No secret/env/log persistence behavior changed.

## I18n/CJK Impact

Path display and terminal insertion behavior are unchanged. Existing CJK/emoji
path tests continue to pass through the broader file-tree suite.

## Rollback Plan

Remove `GitIgnoreCache`, revert `read_children`/watcher filtering to the previous
default-ignore-only behavior, and remove the gitignore tests.

## Follow-up

- If exact git semantics become necessary, replace the lightweight matcher with
  a dedicated ignore engine behind the same cache boundary.
