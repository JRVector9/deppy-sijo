# PR-OD01 — Bounded Diagnostics and Soak Gates

## Exact performance smoke selection

`xtask perf-smoke` no longer relies on substring filters that can select zero tests and still
return success. It runs nine exact tests and requires each command to report exactly one pass. The
matrix covers app percentile/harness shape, runtime durable outbound overflow, viewport
coalescing, receiver cap/filtering, and both hidden-session status/remote-view leases.

`cargo run -q -p xtask -- perf-smoke` executed 9/9 exact tests successfully. These deterministic
checks are smoke evidence, not a substitute for the release Scenario A-E hardware
CPU/RSS/frame-p95 gate.

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

## MCP transport trace sanitization

HTTP session reconnect/progress, unsupported SSE/server messages, unknown-delivery outcomes, and
sender-reaper invariants now emit only fixed `kind`, `phase`, and `error_code` fields. Stdio
unknown-delivery and unsupported unsolicited server messages use the same schema. Server name,
method, SSE event type, URL, arguments, headers, payload, and raw errors are never recorded.

Hostile subscriber-capture regressions prove that dynamic event/method/payload markers are absent
from HTTP and stdio traces. Root verification passes both exact tests, MCP all-target check, strict
Clippy, package rustfmt, and scoped diff-check. The first root HTTP invocation used an unqualified
name with `--exact` and selected zero tests; the fully qualified test was rerun and passed 1/1.

## Count-bounded service and proxy soak gates

Connector-service now has a deterministic 24-cycle discover/cancel regression plus explicit
ignored env-count coordinator and local-stdio soaks. At every retained checkpoint it requires the
command queue, MCP/OAuth jobs, host backlog, live repository, worker/job threads, leases,
cancellation and operation registries to return to zero; generation is at most one and diagnostic
transitions remain capped at 64. Checkpoint retention is latest-only with a hard cap of 901. The
local-stdio soak also requires the shell and its sleeping grandchild to be reaped and internal
stdout/stderr/writer/redaction counters to return to baseline.

The normal connector-service suite passes 78 tests with two ignored env soaks. A 900-cycle
coordinator run completed in 4.60 seconds with 901 checkpoints retained; a 902-cycle stdio run
completed in 7.96 seconds with the same cap. The first attempted exact filter selected zero tests,
an initial test helper used a nonexistent redaction accessor, and strict Clippy rejected a
non-minimal comparison; all three were corrected and are not counted as passing evidence.

MCP proxy now has a deterministic 32-cycle reuse/TTL/Unknown-delivery regression and an ignored
env-count soak. It proves warm reuse, idle eviction, poison-only reconnect semantics, zero active
lease/session/expiry state at boundaries, and no automatic retry after Unknown delivery. The
session suite passes 19 tests with one ignored soak. The maximum 900-cycle run completed in 26.46
seconds with 901 retained checkpoints, cold 900, warm 900, evictions 899, Unknown 1, and zero active
lease/thread/permit/HTTP-reaper queue. An initial wrong binary target selected no tests and the
first small-soak expectation used incorrect warm-count semantics; both were corrected before the
accepted runs.

## Remaining OD01 scope

The count-bounded soaks do not sample process RSS or OS socket inventories. A 30-minute wall-clock
RSS/thread/socket/queue slope run and release Scenario A-E hardware CPU/RSS/frame-p95 comparison
remain final production gates. The completed slices add no production timer, polling loop,
network, process, or retained source/sample backlog.

## Notice resource pipeline

Status HTTP bodies are capped before parsing at 64 KiB for service status and 4 MiB for notices.
Projected description/title/status/date/URL values have explicit byte ceilings and invalid or
oversized values fail closed. Persistent read state is a rolling 512-item/512-KiB corpus with a
4-MiB file ceiling instead of an ever-growing set.

Translation cache retention is capped at 256 items/512 KiB with a 4-MiB file ceiling. Translation
batches are limited to 32 items and 128 KiB, result delivery is capacity one, and subprocess
stdout is capped at 256 KiB. The named stdout reader is joined after child reap on success, error,
and timeout. Debug redacts title-like content, and no 30-minute measurement is included here.

Focused status-feed tests pass 17/17 and notice-translation tests pass 11/11. The integrated app
all-target check, strict Clippy, full rustfmt, diff-check, zero-allowlist boundary, and dependency
gates pass.

## App host and external-process resource closeout

The remaining app-owned host pipelines now have explicit finite retention and shutdown rules.
Recursive file operations admit at most 50,000 items, 4 GiB of streamed data, and depth 128, copy
through one 64-KiB buffer, reject special files, and observe cancellation before publishing a
temporary destination. Git and worktree commands share bounded stdout/stderr readers and process-
group kill/reap/join cleanup; submodule discovery, slug probes, and exclude-file updates have
operation-wide caps. Codex App Server handoffs are bounded to 8/64/16/1 items with fatal
backpressure and bounded protocol projections. Local model discovery, Tailscale commands, and
clipboard ingress/cache paths have explicit item, byte, time, file-count, and TTL ceilings.

Root focused verification passes Git 12/12, worktree 27/27, app-server 35/35, local-LLM 10/10,
Tailscale 16/16, clipboard-image 12/12 with one real-clipboard ignore, and AppHost 3/3. The full
app suite passes 720 tests with five explicit hardware/external-resource ignores; logging policy
passes 15/15. App and xtask all-target check, strict Clippy with `-D warnings`, xtask 9/9,
zero-allowlist boundary, the 23-crate dependency DAG, full rustfmt, and diff-check pass. No
30-minute or Scenario A-E measurement was performed; latest-only agent-detection delivery remains
development work before the final hardware gate.

## Agent detection and retained-session resource closeout

Agent detection now publishes one immutable, Arc-backed generation snapshot and retains only one
latest outcome. Identical inputs neither clone the override map nor wake the worker; empty input
parks indefinitely with detector I/O and repaint at zero. Stale generations are discarded, and
partial activity updates preserve the latest full binding/cwd/info projection without building an
event backlog. Input and output maps are capped at 256 admitted sessions.

The detector's ps/lsof boundary uses a three-second bounded process-group runner with explicit
stdout/stderr byte caps, fixed 128-KiB reader stacks, and kill/reap/join cleanup on timeout,
overflow, and inherited-descendant-pipe paths. Process rows, commands, descendant walks, candidate
counts, lsof invocations, recursive transcript discovery, paths, identifiers, and transcript-head
reads all have exact item/byte/depth ceilings and fail closed on hostile or invalid input.

Structured session projection is bounded at 4,096 retained items, 32 approvals, 64 identity-only
skills, and 8 MiB including cached rows. Per-ID, text, item, file-change, nested-value, and depth
limits are enforced before retention. Table rows are cached and borrowed, repeated updates replace
in place, and oldest eviction rebuilds bounded indices. Secret-bearing session/event/approval/file
types are non-Clone with redacted Debug; stale diagnostics use low-cardinality codes.

Focused evidence is detector input 20/20 with one real-process ignore, latest-only worker 10/10,
and session projection 24/24. The integrated app suite passes 752 tests with five explicit ignores,
logging policy passes 15/15, and app/xtask all-target check, strict Clippy, xtask 9/9, zero-
allowlist boundary, the 23-crate dependency DAG, full rustfmt, and diff-check pass. Hardware-duration
measurement remains deferred until the remaining startup/action input seams are structurally
bounded.

## Runtime admission and PTY lifecycle closeout

The in-process runtime now applies one shared 1,024-item/8-MiB queue budget and validates every
command before all enqueue paths and again before dispatch. Sessions stop at 256, terminal grids
at 65,536 cells, and launch, environment, path, regex, identifier, and input payloads have fixed
item/byte aggregates. Dynamic payloads are rebuilt with length-bounded backing before they can
enter mux, cwd, environment, persistence, or event state. Runtime failures expose only static
kind/phase/error-code fields. RuntimeCommand variants, ordering, and wire bytes are unchanged.

PTY output uses a lossless 64-by-8-KiB bounded queue whose cancellation wakes both blocked
producer and consumer. Unix uses cancellable nonblocking duplicated master descriptors and joins
both workers. Windows drains ConPTY output through master/HPCON close, then cancels and joins the
writer and reader; explicit kill attempts exactly once, bounded-reaps even on failure, completes
all teardown, and only then returns the remembered static-context error. No detached reaper,
polling timer, or async runtime was added.

Runtime passes 189/189 plus doc tests, all-target check, runtime-only strict Clippy, scoped fmt,
and raw-diagnostic scans. PTY passes 28/28 and session 50/50 plus doc tests, combined all-target
check, strict Clippy, scoped fmt, and diff-check. Dependency-inclusive strict gates are rerun by
root after the concurrently edited storage lane freezes; Windows compilation/runtime remains a CI
gate because the local Windows target is unavailable.
