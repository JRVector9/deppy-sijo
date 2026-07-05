# Build PR Summary

## Input Findings
- PR-R05 Finding 2: clipboard paste used bracketed paste semantics, but file-tree drop and path insert sent raw quoted bytes directly.
- PR-R05 Finding 6: required English, Japanese, Simplified Chinese, Traditional Chinese, Korean, and emoji path fixtures needed executable paste/DnD byte-generation coverage.
- PR-R04 baseline: terminal DnD/path insert must never auto-send Enter/newline.

## Scope
- Add a terminal crate paste byte helper shared by clipboard paste and path insertion callers.
- Route workspace terminal drops and sidebar/context path insertion through the shared helper while preserving `RuntimeCommand::WriteInput`.
- Keep B03a shell quoting helpers unchanged and avoid `file_tree.rs` changes.

## Changes
- `terminal::input_mapper::paste_bytes` now generates raw or bracketed paste bytes for arbitrary payload bytes.
- Clipboard paste now uses `paste_bytes`, so clipboard and path insertion share the same bracketed wrapper semantics.
- `WorkspaceUi` now exposes cached per-session bracketed paste state plus workspace shell kind and uses both for file-tree path drops.
- Sidebar/context path insertion now uses the focused session's cached bracketed paste state and shell kind.
- Path insertion now calls `shell_path_insert_bytes_for(path, shell_kind)` at the real call sites instead of always using the POSIX/default wrapper.
- Added fixture tests for:
  - `src/main.rs`
  - `プロジェクト/設定ファイル.rs`
  - `项目/配置文件.rs`
  - `專案/設定檔.rs`
  - `프로젝트/설정파일.rs`
  - `project/🚀-deploy/config.json`

## Tests
- `cargo fmt --check` - pass
- `cargo test -p terminal` - pass
- `cargo test -p deppy-sijo workspace` - pass
- `cargo test -p deppy-sijo file_tree` - pass
- `cargo test -p deppy-sijo` - pass
- `cargo check --workspace --all-targets` - pass

## Risk Notes
- Runtime does not yet expose per-session shell-family metadata. Until that lands, `WorkspaceUi` uses a platform/environment default shell kind: PowerShell on Windows, fish when `$SHELL` is fish, otherwise POSIX.
- If a session has not yet delivered a `Viewport` event, app-level path insert defaults to non-bracketed paste.
- The UI continues to send only `RuntimeCommand::WriteInput`; no terminal backend implementation type is exposed.

## Rollback Plan
- Revert the `input_mapper::paste_bytes` helper and restore clipboard paste to the prior private wrapper.
- Revert workspace/app call sites to use raw `shell_path_insert_bytes` or `shell_quote + space`.
- Remove this summary document if the PR is reverted.

## Follow-up Review Requests
- Review against PR-R05 Finding 2 and Finding 6, especially bracketed on/off bytes and no-auto-Enter path insert behavior.
- Follow-up review should decide whether runtime/session should expose exact per-session shell-family metadata instead of the current workspace default heuristic.
