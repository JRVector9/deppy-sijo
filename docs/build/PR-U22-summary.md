# PR-U22 Build Summary

## Input Findings

- PR-R09 found hardcoded UI strings and no runtime-configurable locale path.
- PR-U21 added locale catalogs, fallback lookup, pseudo-locale support, and `xtask i18n-check`.

## Scope

- Migrate user-facing UI labels, buttons, headings, and static guidance text to `i18n::Catalog`.
- Add a settings locale selector wired to `config.i18n.locale`.
- Keep terminal output, paths, command names, env keys, MCP server/tool names, and JSON input/output unmodified.
- Defer structured runtime/event message payload changes to PR-U23.

## Changes

- Added an `i18n::Catalog` to `App` and load/reload it from the persisted locale.
- Routed the catalog through top bar, settings, workspace manager, workspace pane UI, notification center, credentials, approvals, env profiles, agents, connectors, and file tree UI.
- Added a settings language/locale section for `en-US`, `ja-JP`, `zh-Hans`, `zh-Hant`, `ko-KR`, and pseudo-locale `en-XA`.
- Expanded required locale catalogs with migrated UI keys and kept optional `ko-KR` populated.
- Preserved raw terminal text, paths, commands, MCP names, env keys, and credential labels as non-translated content.

## Tests

- `cargo check --workspace --all-targets`
- `cargo run -p xtask -- i18n-check`

## Acceptance Criteria Check

- [x] Core UI strings render through message keys.
- [x] Locale can be changed from settings and saved to config.
- [x] Required locale completeness is enforced.
- [x] Pseudo-locale remains available for layout smoke work.
- [x] Terminal output/path/command/env/MCP names remain untranslated.
- [x] Existing pane/folder tree/DnD/copy-paste paths are not changed.

## Regression Risks

- Some dynamic validation and filesystem error strings remain plain text because they need structured error/message work rather than label substitution.
- Locale switching reloads the catalog after config changes; already-open egui frame text updates on the next frame.

## Resource Impact

- One catalog clone per frame is used to avoid UI borrow conflicts. Catalogs are small maps loaded from embedded strings.

## Security Impact

- No secret/env/API key storage behavior changed.
- Token, credential labels, MCP tool names, paths, and JSON arguments remain raw values and are not translated or persisted differently.

## I18n/CJK Impact

- Required locale key completeness stays covered by `i18n-check`.
- Pseudo-locale can now exercise real UI surfaces for PR-U24.

## Rollback Plan

- Revert the catalog plumbing and locale key additions. PR-U21 infrastructure can remain without migrated UI consumers.

## Follow-up

- PR-U23 should convert runtime/event/notification status messages to `message_id + args`.
- PR-U24 should run pseudo-locale/CJK layout gate checks against the migrated UI.
