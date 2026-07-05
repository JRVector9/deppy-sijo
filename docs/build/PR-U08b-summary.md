# PR-U08b Build Summary

## Input Findings

- Follow-up backlog called out manual Debug redaction checks for sensitive
  runtime/config rows.
- Code inspection found `RuntimeCommand` and `EnvValue` using derived `Debug`
  while they can carry paste bytes, plain env values, args, regex strings, or
  credential ids.

## Scope

- Add defensive Debug redaction only.
- Do not change serialization, DB schema, runtime command handling, or UI
  behavior.

## Changes

- Replaced derived `Debug` for `runtime::RuntimeCommand` with a manual
  implementation.
- `SpawnAgent` Debug now reports counts/flags instead of args, env values,
  env keys, credential ids, or regex contents.
- `WriteInput` Debug now reports `bytes_len` instead of input bytes.
- `SeedRedaction` Debug now reports credential count instead of credential ids.
- Replaced derived `Debug` for `storage::EnvValue` with a manual implementation
  that hides plain values and credential ids.

## Tests

- `cargo fmt --check` - pass
- `cargo check --workspace --all-targets` - pass
- `cargo clippy --workspace --all-targets` - pass
- `cargo test -p runtime runtime_command_debug` - pass
- `cargo test -p storage env_value_debug` - pass
- `cargo run -p xtask -- security-scan` - pass
- `cargo test --workspace --no-run` - pass

## Acceptance Criteria Check

- [x] Runtime command Debug does not expose paste/input bytes.
- [x] Runtime command Debug does not expose agent args/env values/credential ids.
- [x] EnvValue Debug does not expose plain values or credential ids.
- [x] Wire format remains unchanged because only Debug derives changed.

## Regression Risks

Low. This changes developer-facing Debug output only.

## Resource Impact

None.

## Security Impact

Reduces accidental log/debug leakage risk for runtime commands and env values.

## I18n/CJK Impact

None.

## Rollback Plan

Restore derived `Debug` on `RuntimeCommand` and `EnvValue`, and remove the tests.

## Follow-up

- Continue avoiding `Debug` logging of command/config rows at call sites.
- Extend this pattern if new runtime commands carry user input or secret-adjacent
  metadata.
