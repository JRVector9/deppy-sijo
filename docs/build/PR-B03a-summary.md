# Build PR Summary

## Input Findings
- PR-R04 Finding 1: folder-tree path insertion used one POSIX quoting helper for every shell family.
- PR-R04 Finding 6: an empty workspace path silently fell back to Desktop, making Desktop the file-tree and move root.
- PR-R00 baseline: folder-tree DnD/context path insertion must not auto-send Enter/newline.

## Scope
- Added shell-specific quoting helpers for POSIX/bash/zsh, fish, PowerShell, and cmd.
- Kept the existing `shell_quote(path)` wrapper available with its current POSIX/default behavior for call sites that do not know the shell kind.
- Removed Desktop fallback for empty workspace paths.
- Left async/chunked large listings, gitignore matching, multi-path payloads, and tree-internal move UX unchanged.

## Changes
- `crates/app/src/ui/file_tree.rs`
  - Added `ShellKind`, `shell_quote_for`, `shell_path_insert_bytes`, and `shell_path_insert_bytes_for`.
  - Preserved `shell_quote(path)` as a POSIX wrapper.
  - Added table coverage for spaces, single quotes, backslashes/drive-colon paths, Japanese, Chinese, Korean, and emoji paths across supported shell families.
  - Added a no-auto-Enter byte invariant test.
- `crates/app/src/app.rs`
  - Changed empty/whitespace workspace path handling to return `None` instead of Desktop.
  - Made sidebar context path insertion use the shared path-insert bytes helper, including trailing space and no newline.

## Tests
- `cargo test -p deppy-sijo file_tree`
- `cargo test -p deppy-sijo workspace_path_to_tree_root`
- `cargo test -p deppy-sijo`

## Risk Notes
- Current terminal drop paths in `workspace.rs` still use the existing default `shell_quote(path)` wrapper because session shell metadata is out of scope for this PR.
- PowerShell/cmd support is available through `shell_quote_for`, but UI routing to those variants needs a later session metadata PR.
- Empty workspace path now intentionally shows the unset-path guidance instead of a usable Desktop tree.

## Rollback Plan
- Revert this PR to restore the prior POSIX-only helper and Desktop fallback.
- If only shell quoting regresses, keep the empty-root fix and revert the new shell-specific helpers/tests.

## Follow-up Review Requests
- Review against PR-R04 Findings 1 and 6.
- Verify terminal DnD/context insertion still sends quoted path bytes plus a trailing space and never sends Enter/newline.
- Verify empty workspace path renders the unset-path guidance and does not expose Desktop as a move root.
