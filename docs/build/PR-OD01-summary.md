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

## Remaining OD01 scope

The low-cardinality transition ring, app log days/bytes retention and GC, failpoint matrix,
diagnostic scan integration, and RSS/thread/socket/queue slope soak harness remain for the final
OD01 cutover. This scanner adds no thread, timer, polling, network, process, or retained source
buffer by itself.
