# PR-OD01 — Bounded Diagnostics and Soak Gates

## Bounded secret-like scanner

The `secret` crate now exports a std-only scanner for diagnostic and log artifacts. One scan
accepts at most 8 MiB and retains at most 64 findings. Oversized input is rejected before prefix
inspection, and a 65th finding marks the report truncated; both cases are explicitly unsafe.

Reports contain only five stable categories, bounded counts, and a truncation bit. They retain no
source bytes, raw keys or values, offsets, paths, operation identifiers, or raw errors. Detection
covers Bearer authorization, exact-boundary sensitive JSON and assignment keys, common provider
token prefixes, and private-key PEM markers while excluding placeholders and embedded key names.

## Verification

- Root full `secret` tests: 55/55 pass, including nine scanner regressions.
- `cargo check -p secret --all-targets` passes.
- Strict all-target Clippy, package rustfmt, and scoped diff-check pass.
- Initial focused tests found an over-broad Hugging Face fixture, an oversized-fixture length bug,
  and an assertion that rejected the allowed low-cardinality category name. All were corrected
  before root verification.

## Deterministic failure matrix

`cargo run -q -p xtask -- od01-failure-matrix` now parses the frozen contract's ten
`FailurePoint` variants and requires an exact one-to-one mapping to deterministic package tests.
Duplicate, missing, or unknown mappings fail before test execution. Each cargo invocation uses the
fully qualified test name with `--exact --test-threads=1`, and xtask also rejects a successful
process that did not report exactly one selected passing test.

The matrix covers repository rollback before commit and after permission mutation/audit prepare,
secret bundle write rollback, secret pointer-swap rollback, audit-preflight call-zero, MCP
pre-send rejection, post-send Unknown delivery, bounded HTTP timeout/reaper behavior, worker-panic
capacity release, and authorization-owner crash/failure recovery. Root ran all ten entries
successfully. Xtask passes 8/8 plus all-target check, strict Clippy, fmt, and diff-check. The gate is
test-only and adds zero production runtime work or failure-injection switch.

## Remaining OD01 scope

The low-cardinality transition-ring boundary regression, app log days/bytes retention and GC,
diagnostic scan integration, and RSS/thread/socket/queue slope soak harness remain for the final
OD01 cutover. The scanner and failure matrix add no production thread, timer, polling, network,
process, or retained source buffer.
