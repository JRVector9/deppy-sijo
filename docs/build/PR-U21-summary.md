# PR-U21 Build Summary

## Input Findings

- PR-R09 found no i18n infrastructure for required locales.
- Phase E requires fallback locale, missing-key checks, pseudo-locale support, and persisted locale setting before UI string migration.

## Scope

- Add i18n infrastructure only.
- Do not migrate broad UI strings yet; PR-U22 owns that.
- Keep terminal output, paths, commands, env keys, and MCP tool names outside translation.

## Changes

- Added `crates/i18n`.
- Added required locale catalogs for `en-US`, `ja-JP`, `zh-Hans`, and `zh-Hant`.
- Added optional `ko-KR` catalog.
- Added `Catalog::load`, fallback lookup, argument interpolation, pseudo-locale generation, and completeness validation.
- Added persisted `config.i18n.locale` with fallback normalization.
- Added `cargo run -p xtask -- i18n-check`.

## Tests

- `cargo test -p i18n`
- `cargo test -p deppy-sijo locale_설정`
- `cargo run -p xtask -- i18n-check`

## Acceptance Criteria Check

- [x] Fallback locale exists.
- [x] Required locale key completeness is checked.
- [x] Pseudo-locale exists.
- [x] Locale setting is persisted in config.
- [x] Current + fallback locale loading is represented by `Catalog`.

## Regression Risks

- Locale seed set is intentionally small; PR-U22 will expand keys while migrating UI strings.
- The catalog format is simple `key = value`; multiline rich text is out of scope.

## Resource Impact

- Catalog loading holds only current and fallback maps.
- No runtime background work is introduced.

## Security Impact

- No secret, env, DB, or log persistence changes.

## I18n/CJK Impact

- Establishes required locale files and pseudo-locale smoke tests.

## Rollback Plan

- Remove the `i18n` crate, config `i18n` field, and `xtask i18n-check`.

## Follow-up

- PR-U22 should migrate user-facing UI strings to these keys.
- PR-U23 should move runtime/event/error messages to `message_id + args`.
