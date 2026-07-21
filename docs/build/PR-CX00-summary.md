# PR-CX00 — Connector contract, dependency laws, and baseline

## Outcome

- Added `connector-contract`, `connector-ui`, and `connector-service` workspace slots.
- Froze transport-neutral snapshot, intent, event, operation/error, permission, failure-injection,
  and resource-limit types before service/UI extraction begins.
- `SensitiveInput` is non-`Clone`, non-`Serialize`, prints only `REDACTED`, and overwrites its
  owned bytes on drop.
- The production resource ceiling is encoded in `ResourceLimits`; runtime configuration may only
  lower it. Snapshot backlog is exactly one latest value.
- `xtask check-deps` now enforces the exact direct dependency set for all three Connector crates,
  including dev/build/target dependency sections.

## Frozen dependency laws

- `connector-contract`: `serde` only.
- `connector-ui`: `connector-contract`, `egui`, and `i18n` only.
- `connector-service`: `connector-contract`, `mcp`, `auth`, `audit`, `secret`, and `tracing` only.
- Concrete `storage`, `mcp-store`, `app`, `runtime`, `terminal`, and `egui` types cannot enter the
  service crate through a direct dependency. UI cannot acquire service/storage/runtime effects.

## Initial production ceilings

| Resource | Ceiling |
| --- | ---: |
| Command queue | 8 |
| Snapshot backlog | 1 latest-only |
| Active MCP operations | 2 |
| Active OAuth flows | 1 |
| Import input | 1 MiB |
| Imported servers | 256 |
| Tools per server | 4,096 |
| Tool descriptors | 8 MiB |
| Tool input | 32 KiB |
| Raw MCP response | 8 MiB |
| UI result | 1 MiB |
| Diagnostic transitions | 64 |
| Backend leases | 2 |

Raising a ceiling requires a measured source/document change. Lowering remains allowed.

## Baseline on 2026-07-22

- Start: clean `main@ef4abf01996a5261b91f475d0f740b0d50a4cc26`.
- `cargo run -q -p xtask -- check-boundary`: pass, 53 explicit exceptions.
- `cargo run -q -p xtask -- check-deps`: pass, 20 pre-CX00 crates.
- `cargo run -q -p xtask -- perf-smoke`: pass; app perf 3/3 and runtime hidden 2/2.
  The current runtime `backpressure` filter selected zero tests, so it is not counted as coverage.
- Focused tests: audit 32/32 and storage 73/73 pass.
- MCP: 43 pass, 28 managed-sandbox localhost-bind failures.
- mcp-proxy: 19 pass, 2 managed-sandbox localhost-bind failures.
- All 30 focused failures occur while creating local listeners with `Operation not permitted`;
  they are retained as an environment limitation rather than waived product failures.

## Performance approval boundary

Historical release measurements exist in `docs/render-resource-benchmark-report.md`, but current
Scenario A-E RSS/CPU/frame-p95 numbers have not been captured for this refactor baseline in an
unrestricted GUI environment. Automated `perf-smoke` does not constitute release approval. The
same release-hardware procedure must be run before PR-BG01 and compared against a matching baseline.

## Failure-injection API

`connector_contract::FailurePoint` names repository pre-commit, prepared-audit crash, secret write
and pointer swap, audit preflight commit, pre-send, unknown post-send, HTTP timeout, worker panic,
and process crash boundaries. `FailureInjection { point, skip }` injects once after a deterministic
number of matching operations. Production has no runtime enable switch; owning crates provide
test-only injectors at these named seams.

## Wave-1 parity amendment

The first pure renderer proved four missing commands would otherwise force a second app-to-service
path. The same transport-neutral boundary was therefore extended, without adding dependencies or
exposing storage/runtime types:

- `ImportConfiguration { source_name, contents: SensitiveInput }` lets the app-owned picker/read
  path re-enter the single intent dispatcher.
- `selected_server_config: Option<ServerDraft>` loads editable configuration only for the selected
  server rather than bloating every overview row.
- `EnsureSlackServer` represents first-time built-in registration without inventing a server ID in
  the UI.
- `OAuthClientPrompt` and correlated `SubmitOAuthClient`/`Cancel` preserve a single OAuth state
  machine and operation ID.

This is an additive completeness correction found by UI01, not a change to the crate direction.
Import bytes and OAuth/tool inputs remain non-Clone/non-Serialize and redacted in Debug output.
