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

## Lazy diagnostic and resource sampling amendment

The Connector transition ring has an end-to-end 65-event regression: it retains exactly the
latest 64 `{kind, phase, error_code}` entries, evicts the oldest on event 65, and never retains a
4-KiB-class raw identifier/payload marker. The production ceiling remains 64.

Runtime now checks a side-effect-free `ProcessResourceMonitor::is_due(now)` before collecting any
session target. The same captured `Instant` advances the existing sample deadline, preserving the
immediate first sample, two-second cadence, CPU baseline, emitted events, and wire format. A fake
time/counter regression proves 300 pre-deadline pumps collect zero targets, the exact deadline
collects once, and the next 300 pre-deadline pumps collect zero. Root verification passes
Connector 70/70 and the 84-test socket-independent runtime suite, plus all-target check, strict
Clippy, fmt, and scoped diff-check. No timer, thread, polling loop, or retained sample backlog was
added.

## Remaining OD01 scope

App log days/bytes retention and GC, diagnostic scan integration, and the env-only
RSS/thread/socket/queue slope soak harness remain for the final OD01 cutover. The completed slices
add no production timer, polling loop, network, process, or retained source/sample backlog.
