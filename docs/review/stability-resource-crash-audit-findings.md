# Stability / Resource / Crash Audit Findings

## Summary

- Overall verdict: Pass with Issues
- Critical: 0
- High: 2
- Medium: 1
- Low: 1

The main stability gates pass on commit `fab0f61`: formatting, clippy, check,
test no-run, dependency checks, security scan, performance smoke, i18n check,
DB migration smoke, targeted PTY input queue tests, and targeted remote outbound
queue tests all passed.

No direct evidence of hidden pane viewport snapshot generation, raw plaintext log
default enablement, crate cycles, or remote slow-consumer unbounded outbound
growth was found. The remaining risks are narrower: the local runtime subscriber
event channel is still unbounded, file-tree listing/watch delivery can backlog
without an explicit cap, and PTY teardown intentionally allows SIGHUP-ignoring
grandchildren to survive.

## Environment

- OS: macOS 26.4.1 build 25E253
- Kernel: Darwin 25.4.0 arm64
- CPU: unavailable; `sysctl -n hw.ncpu` was denied by the sandbox
- RAM: unavailable; `sysctl -n hw.memsize` was denied by the sandbox
- Build mode: Cargo dev/test profile
- Commit: `fab0f61`
- Test duration: command-level timings only; full GUI soak was not run

## Commands Run

- `cargo fmt --check`: pass
- `cargo clippy --workspace --all-targets`: pass
- `cargo check --workspace --all-targets`: pass
- `cargo test --workspace --no-run`: pass
- `cargo tree --workspace --edges normal,build`: pass
- `cargo tree --workspace --edges normal,build,dev`: pass
- `cargo metadata --format-version 1 > target/cargo-metadata.json`: pass
- `cargo run -p xtask -- check-deps`: pass, 19 crates, no forbidden edge/cycle
- `cargo run -p xtask -- security-scan`: pass
- `cargo run -p xtask -- perf-smoke`: pass
- `cargo run -p xtask -- i18n-check`: pass
- `cargo run -p xtask -- smoke-db-migrations`: pass
- `cargo test -p deppy-sijo file_tree -- --nocapture`: pass, 29 tests, 101.59s
- `cargo test -p pty input_queue -- --nocapture`: pass, 3 tests
- `cargo test -p runtime outbound_queue -- --nocapture`: pass, 2 tests
- `cargo test -p runtime receiver_drain -- --nocapture`: pass, 2 tests
- `sw_vers`: pass
- `uname -a`: pass
- `sysctl -n hw.model`: denied by sandbox
- `sysctl -n hw.memsize`: denied by sandbox
- `sysctl -n hw.ncpu`: denied by sandbox

## Scenarios Run

### Scenario A

Result: Not fully run.

Metrics: Empty-app GUI idle 5 minute RSS/CPU/repaint count was not measured in
this sandbox. Existing `docs/performance/final-gate.md` keeps Scenario A pending.

### Scenario B

Result: Not fully run.

Metrics: 5 workspaces, 20 panes, 10 sessions, and active visible pane count were
not measured in GUI. Compile/smoke gates passed.

### Scenario C

Result: Partially covered by automated smoke.

Metrics: `cargo run -p xtask -- perf-smoke` passed and included
`status_화면_패턴_hidden에서_snapshot_없이_감지`, which verifies hidden status
detection without viewport snapshot creation. Full 10 hidden sessions plus
10MB/min output soak was not run.

### Scenario D

Result: Partially covered by file-tree unit tests.

Metrics: file-tree watcher, ignore, quoting, no-enter DnD, stale listing, and
collapse handling tests passed. A generated 100k-file GUI workspace run was not
performed.

### Scenario E

Result: Partially covered by remote queue unit tests.

Metrics: runtime remote outbound queue tests passed for viewport coalescing,
durable queue cap, and receiver drain cap. A long slow-client soak was not run.

## Findings

### Finding 1

Severity: High

Area: Runtime event queue / local slow consumer backpressure

Files:
- `crates/runtime/src/client.rs:11`
- `crates/runtime/src/client.rs:12`
- `crates/runtime/src/in_process.rs:192`
- `crates/runtime/src/in_process.rs:218`
- `crates/runtime/src/in_process.rs:503`
- `crates/runtime/src/in_process.rs:531`

Evidence:

`RuntimeEventReceiver` stores non-slotted events in
`std::sync::mpsc::Receiver<RuntimeEvent>`. `subscribe()` and
`subscribe_with_wake()` create this path with `channel()`, which is unbounded.
`emit()` correctly slots/coalesces `Viewport` and `PtyInputPressure`, but every
other event goes through `subscriber.events.send(event.clone())`. Resource usage
events are emitted through the same path.

Remote outbound code adds a bounded cap after draining from this receiver, and
remote tests pass. That protects the remote wire queue, but it does not bound the
upstream local subscriber channel if a subscriber is slow or not draining.

Why it matters:

The audit requirement is that output, render event, DB write, notification, and
remote queues must not grow without bound. A stalled UI subscriber or future
secondary subscriber can accumulate lifecycle/status/resource events
indefinitely. In the normal app path the UI drains events regularly, so this is
not currently reproduced as an everyday leak, but the API contract remains
unbounded.

Reproduction:

Add a test-only subscriber via `subscribe()` or `subscribe_with_wake()` and do not
drain `RuntimeEventReceiver.events`. Then trigger repeated state/resource
changes, for example by running sessions while resource monitor samples. The
channel has no cap or overflow behavior.

Suggested fix:

Replace the general event path with a bounded queue or split it into durable
bounded events plus coalesced slots for latest-value events such as resource
usage and session status. On overflow, disconnect/degrade the stale subscriber or
surface a pressure event; do not silently drop durable command acknowledgements.

Suggested test:

Add a runtime slow-subscriber test that stops draining non-viewport events,
generates more than the configured cap, and asserts bounded memory/queue length
and explicit degraded/disconnected behavior.

Suggested PR: New PR, for example `PR-U15d Runtime Event Queue Backpressure`.

### Finding 2

Severity: High

Area: Folder tree listing/watch queue and worker pressure

Files:
- `crates/app/src/ui/file_tree.rs:449`
- `crates/app/src/ui/file_tree.rs:450`
- `crates/app/src/ui/file_tree.rs:538`
- `crates/app/src/ui/file_tree.rs:1492`
- `crates/app/src/ui/file_tree.rs:1504`
- `crates/app/src/ui/file_tree.rs:1531`
- `crates/app/src/ui/file_tree.rs:1533`
- `crates/app/src/ui/file_tree.rs:2434`
- `crates/app/src/ui/file_tree.rs:2435`
- `crates/app/src/ui/file_tree.rs:2437`
- `crates/app/src/ui/file_tree.rs:2447`
- `crates/app/src/ui/file_tree.rs:2494`
- `crates/app/src/ui/file_tree.rs:2508`

Evidence:

The file tree uses unbounded `std::sync::mpsc::channel()` for ops, listing
results, and watcher events. Each listing request immediately spawns a new
thread. The worker reads the full directory into a `Vec<TreeNode>`, sorts it,
then sends 2048-entry chunks into the unbounded listing channel. The UI consumes
only `LISTING_RESULTS_PER_FRAME = 4` outcomes per frame.

Existing file-tree tests passed, including watcher ignore/debounce, stale root,
stale collapse, shell quoting, and no-enter insertion. However, they do not cap
the number of in-flight listing workers or queued listing/watch outcomes. The
targeted file-tree test run took 101.59s, which is not a runtime failure but
shows this area is already comparatively heavy under unit-test coverage.

Why it matters:

Large workspaces, rapid expand/collapse, root switching, or a burst of watcher
events can create stale worker output faster than the UI drains it. Because stale
results are discarded only after receipt, stale chunks can still occupy memory.
For a 100k-file workspace this can become RSS growth or UI catch-up latency.

Reproduction:

Create a workspace with many large directories, expand multiple directories,
collapse them, switch roots, and generate watcher bursts before the UI drains
listing results. Instrument thread count, listing channel backlog, and RSS.

Suggested fix:

Use bounded channels or a bounded worker pool for listings, add cancellation
tokens that are checked before and during chunk sending, limit concurrent
listings, and drain stale outcomes aggressively. For watcher events, keep the
current debounce/deduping but add an input cap or overflow-to-root-refresh mode.

Suggested test:

Add a synthetic large-tree test that asserts max concurrent listing workers,
max queued listing chunks, stale-result cancellation, and bounded watcher burst
behavior.

Suggested PR: `PR-U16b Folder Tree Listing/Watcher Backpressure`.

### Finding 3

Severity: Medium

Area: PTY child process cleanup / SIGHUP-ignoring descendants

Files:
- `crates/pty/src/lib.rs:77`
- `crates/pty/src/lib.rs:87`
- `crates/pty/src/lib.rs:212`
- `crates/pty/src/lib.rs:218`
- `crates/pty/src/lib.rs:221`
- `crates/pty/src/lib.rs:224`
- `crates/pty/src/lib.rs:228`

Evidence:

PTY teardown sends `SIGHUP` to the process group and then calls a bounded
`kill_and_reap_bounded()` for the direct child. The code explicitly documents
that SIGHUP-ignoring processes can survive with nohup-like semantics and that
thread join is intentionally avoided because a grandchild can hold the slave fd.
The bounded reap logs and gives up after 2 seconds.

Why it matters:

This avoids shutdown deadlock, which is correct, but it means a session close can
leave SIGHUP-ignoring background descendants running. This is a resource leak
from the product perspective if users expect pane/session close to clean up all
descendants.

Reproduction:

In a terminal session, run a background job that ignores SIGHUP, close the
pane/session, and inspect `ps` for surviving descendants.

Suggested fix:

Decide the product policy explicitly. If close must terminate all descendants,
add an opt-in or default escalation path such as SIGHUP, short grace, SIGTERM,
short grace, SIGKILL to the process group, while preserving a clear detached job
mode for users who intentionally want background jobs to survive.

Suggested test:

Add a Unix integration test that starts a process group with a SIGHUP-ignoring
child and verifies the configured cleanup policy.

Suggested PR: `PR-U12d Child Process Cleanup Policy`.

### Finding 4

Severity: Low

Area: Release performance evidence

Files:
- `docs/performance/final-gate.md`
- `docs/performance/release-hardware-measurements.md`
- `docs/build/PR-U20c-summary.md`

Evidence:

The performance documents correctly state that automated gates pass, but full
Scenario A-E release-hardware measurements remain pending. This audit repeated
the automated gates and reached the same conclusion.

Why it matters:

`perf-smoke` is useful but does not prove release-build RSS/CPU/frame-p95
behavior under GUI idle, multi-workspace, hidden high-output, 100k-file
folder-tree, or remote slow-client soak scenarios.

Reproduction:

Run the manual release measurement procedure in `docs/performance/final-gate.md`.

Suggested fix:

Before release approval, run the documented Scenario A-E measurements on the
release machine and replace the Pending rows with measured pass/fail values.

Suggested test:

Add a future `cargo run -p xtask -- perf-report` harness when GUI scenario setup
can be controlled headlessly.

Suggested PR: `PR-U20d Release Hardware Measurement Run`.

## Resource Metrics

- App RSS: not measured in this sandbox
- Child process RSS: not measured in this sandbox
- CPU: not measured in this sandbox
- Repaint count: not measured in this sandbox
- Snapshot count: hidden snapshot smoke passed; full soak count not measured
- Queue sizes:
  - Runtime command queue: bounded by `IN_PROCESS_CMD_QUEUE_CAP`
  - PTY output queue: bounded `sync_channel(64)`
  - PTY input queue: bounded by default 256 messages and 4 MiB byte budget
  - Remote outbound durable queue: bounded at 1024
  - Remote viewport slots: bounded/coalesced at 256
  - Runtime local non-viewport event queue: unbounded
  - File-tree listing/watch channels: unbounded
- DB write rate: not measured; hot-path status/log-offset writes are wired
  through `DbWriteWorker`, and migration smoke passed
- Watcher event rate: not measured; debounce/deduping tests passed, input
  channel cap is still absent

## Crash / Panic Risks

- No syntax or clippy errors were found.
- No deterministic crash was reproduced.
- Several long-lived components still use `expect()` around poisoned mutexes or
  thread creation. Those are conventional but can panic after a previous panic or
  under OS thread exhaustion. I did not classify this as a finding because it
  requires an earlier failure, but it remains a hardening opportunity.

## Deadlock / Starvation Risks

- PTY input writes are delegated to a writer thread and enqueue with bounded
  `try_send`; targeted `input_queue` tests passed.
- PTY output uses a bounded sync channel, creating natural backpressure instead
  of unbounded memory growth.
- `emit()` intentionally calls wake callbacks after releasing the subscribers
  lock, avoiding a subscriber lock reentrancy deadlock.
- File-tree worker/thread fan-out remains the main starvation/RSS risk under
  large workspace pressure.

## Memory / Handle / Thread Leak Risks

- High: unbounded runtime local non-viewport event channel for slow subscribers.
- High: unbounded file-tree listing/watch channels plus thread-per-listing.
- Medium: SIGHUP-ignoring PTY descendants can survive session close by design.
- No direct evidence was found that hidden panes keep producing
  `TerminalViewportSnapshot`; runtime gates snapshot push on `render_active` and
  watched viewports, and `perf-smoke` covered hidden status without snapshot.

## Secret / Env Leak Risks

- `cargo run -p xtask -- security-scan` passed.
- Storage secret-like env/key tests passed.
- Audit debug/redaction, schema hash, MCP approval, stdout protocol strictness,
  and MCP proxy redaction tests passed.
- No evidence was found that raw plaintext logs are enabled by default.

## Recommended Build PRs

- `PR-U15d Runtime Event Queue Backpressure`
- `PR-U16b Folder Tree Listing/Watcher Backpressure`
- `PR-U12d Child Process Cleanup Policy`
- `PR-U20d Release Hardware Measurement Run`

## Release Blockers

- Fix or formally accept the two High queue/backpressure findings before release
  hardening is considered complete.
- Run full Scenario A-E release-hardware measurements before release approval.

## Open Questions

- Should pane/session close terminate all process-group descendants, or should
  nohup-like descendants be treated as intentionally detached jobs?
- Should file-tree watcher overflow degrade to a root refresh, or should it
  surface a visible pressure warning?
- Should local runtime subscribers be allowed to disconnect automatically when
  they exceed a bounded event backlog?
