# PR-U06b Build Summary

## Input Findings

- PR-U06 remained partial after shared bracketed paste bytes were added.
- Code check confirmed terminal selection/copy exists, file-tree `PathBuf` DnD
  exists, but terminal selected-text DnD had no distinct payload contract.

## Scope

- Add terminal internal selected-text drag/drop paste.
- Keep file-tree path DnD payloads and shell quoting unchanged.
- Keep terminal copy/paste, CJK selection normalization, and bracketed paste
  behavior unchanged.

## Changes

- Added a distinct `TerminalTextDragPayload` for terminal selected text.
- Dragging from inside an existing terminal selection now creates the text
  payload instead of starting a new selection.
- Dropping terminal text onto a pane writes through `RuntimeClient` using the
  same bracketed paste helper as clipboard paste.
- `PathBuf` DnD remains separate and still routes through shell path quoting.
- Pane hover/drop handling now recognizes both path payloads and terminal text
  payloads.

## Tests

- `cargo fmt --check` - pass
- `cargo check --workspace --all-targets` - pass
- `cargo clippy --workspace --all-targets` - pass
- `cargo test -p deppy-sijo terminal_text_dnd` - pass
- `cargo test -p deppy-sijo path_insert_paste_bytes_required_fixtures는_bracketed와_no_enter를_지킨다` - pass
- `cargo test --workspace --no-run` - pass

## Acceptance Criteria Check

- [x] Terminal selected text has a payload distinct from file-tree path DnD.
- [x] Drop-to-terminal paste uses bracketed paste when enabled.
- [x] Selected text bytes are preserved for English, CJK, Korean, emoji, and
  multiline text.
- [x] File-tree path DnD remains path-quoted and no-auto-enter.
- [x] UI still sends terminal input through `RuntimeClient`.

## Regression Risks

- Low to medium. The change shares the terminal drag gesture. It only switches
  to DnD when the drag starts inside an existing selection; drags elsewhere
  still create/update selections.

## Resource Impact

No new background work. Payload allocation is bounded by selected visible
viewport text.

## Security Impact

No persistence or logging behavior changed. Dropped text is terminal input and
is not stored by this PR.

## I18n/CJK Impact

CJK and emoji selected text remains byte-preserved through the paste helper.

## Rollback Plan

Remove `TerminalTextDragPayload`, the selected-text DnD branch, and the added
tests. Existing path DnD and copy/paste paths would remain.

## Follow-up

- Clipboard failure abstraction remains deferred until a concrete platform
  failure mode or product requirement appears.
