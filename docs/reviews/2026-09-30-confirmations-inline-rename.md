# Inline workspace rename and confirmation migration review

Scope: sidebar inline rename and numbered popup cases 07, 08, 10, 11, 12, 13. Base HEAD `166f8dae`, with the earlier uncommitted 0.4.5 popup changes preserved. Manual source review and egui interaction/render harnesses; no Deppy launch.

## Findings fixed

| Location | Finding | Resolution and evidence |
| --- | --- | --- |
| `ui/workspace.rs` | Foreground Modal did not match the older Middle-window input guard. | Gate search, ordinary input and IME with `top_modal_layer`. Real WorkspaceUi test first proves ordinary PTY input, then checks no WriteInput behind the confirmation; RED then GREEN. |
| `ui/ports.rs`, `ui/resource_manager.rs`, `ui/agent_terminal.rs` | Popover-local confirmations could disappear when the source popover closes. | Preserve captured target; render confirmation after popover state synchronization. Popup-backed Ports test closes its source and cancels by Esc; passes. |
| `ui/file_tree.rs` | A rejected permanent-delete retry left its error behind the modal backdrop. | Keep confirmation until IO admission and display the rejection inside its warning. Target/queue/visible-error assertion RED then GREEN. |
| `ui/popup/confirmation.rs` | Giving X and Cancel the same accessibility label makes actions ambiguous. | Separate localized `popup.dismiss` for X in all five locales. |

## Review checks

- One shared presentation component uses the existing shell, notice and action footer. Danger has dark/light palettes; no filesystem, network or process operation runs inside it.
- Callers preserve pane/workspace identity, runtime instance, exact socket bind/protocol/process start identity, and original delete path/label/type. Existing host validation and asynchronous workers remain.
- Inline rename reuses the session editor, focuses once, consumes commit/cancel keys, releases TextEdit state, and queues alias persistence through the existing settings worker. It does not rename the actual folder.
- Dangerous actions require an explicit button action; unfocused Enter does not submit. Esc cancels the top modal even with an open popover behind it.
- Normalized styles inspected in all six PNGs plus the inline workspace row. Case08 renders App's equivalent shared specification and localized body, not a full App integration fixture.

## Commands actually executed

- Full app: `cargo test --locked -q -p deppy-sijo --bin deppy-sijo -- --test-threads=1` — 2463 passed, 0 failed, 26 ignored.
- Render: `cargo test --locked -q -p deppy-sijo --bin deppy-sijo popup_parity_render -- --ignored --test-threads=1` — 7 passed. Initial case13 fixture had no root and drew no confirmation; corrected root/CJK setup and added an area assertion, reran and inspected the corrected image.
- i18n: `cargo test --locked -q -p i18n` — 8 passed.
- Strict Clippy — passed after collapsing one inline commit condition while retaining cleanup on Cancel.
- Offline check, fmt/diff checks and extracted HTML JavaScript syntax — passed.

No unresolved review finding. Native live interaction and destructive process/file operations were not executed; the harnesses verify their existing intents.
