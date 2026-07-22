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

## Bounded sanitized app logger

Production startup now atomically replaces the legacy uncapped `rolling::daily` plus default
128,000-line nonblocking queue with one bounded sink and exactly one tracing worker. The queue is
lossy at 1,024 complete lines so diagnostics cannot backpressure terminal work or retain an
unbounded corpus. There is no old/new dual logger.

The worker bounds each formatted line at 64 KiB, the active UTC-day file at 8 MiB, all managed app
logs at 32 MiB, and retention at seven UTC days with at most 32 managed candidates. Every complete
line is checked by the bounded secret-like scanner; unsafe, truncated, oversized, or crash-partial
input is replaced by a fixed low-cardinality record and the original bytes are not persisted.
Existing active files are bounded-scanned at startup/day rollover, unsafe files are truncated,
and incomplete tails are cut to the last newline. Strict managed filenames, no-follow opens, and
symlink/nonregular checks prevent the GC from following or deleting unmanaged targets.

Rotation and oldest-first GC run only at construction, an observed day transition, or size
pressure. Opening the sink creates no worker/timer/poller; the explicit constructor creates the
same single worker used by tracing. Root verification passes 14 in-module plus 15 integration
policy tests, app all-target check, strict Clippy, fmt, and scoped diff-check. Errors, Debug, and
stats expose only static codes and counts, never paths, source lines, or raw I/O errors.

## MCP proxy diagnostic sanitization

The proxy process boundary now converts every lower-level source into a private
`ProxyRunFailure { phase, error_code }` and emits only fixed `kind`, `phase`, and `error_code`
fields. Startup, protocol, shutdown, approval maintenance, and authorization-owner shutdown no
longer format raw `anyhow` chains, dynamic cleanup counts, server/session identifiers, or backend
values.

CLI and backend configuration/session errors use static codes. Hostile marker regressions cover
CLI arguments, server ID/name/kind/URL, credentials, backend sources, `Display`, alternate/debug
formatting, and a captured tracing subscriber. No thread, poller, runtime, or dependency was added.

Root verification passes mcp-proxy 51/51, all-target check, strict Clippy, package rustfmt, scoped
diff-check, and the production trace scan. The existing stdio fixture measured cold 96.80 ms,
warm 131.67 microseconds, and 2,064 KiB retained RSS in this run; these local fixture values are
evidence, not the final hardware release gate.

## Remaining OD01 scope

The env-only RSS/thread/socket/queue slope soak harness and MCP-owned dynamic traces at
`mcp/src/http.rs:323,416,742,757` and `mcp/src/transport.rs:494,496` remain for the final OD01
cutover. Static but schema-inconsistent MCP events remain at HTTP 531/554/931 and transport
263/282. The completed slices add no production timer, polling loop, network, process, or retained
source/sample backlog.
