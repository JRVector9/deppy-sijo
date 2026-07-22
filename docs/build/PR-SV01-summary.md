# PR-SV01 — Lazy Connector coordinator and latest-only snapshots

## 2026-07-22 invocation-envelope amendment

The bounded command envelope now carries an `InvocationContext` only for `InvokeTool`; every other
intent retains no subject payload. Remote trust, credential refresh, approval wait, and resumed
work transfer that exact context without coordinator-wide mutable identity. Regression coverage
proves sequential workspace A/B dispatch isolation, legacy global compatibility, pending and
refresh continuation preservation, and stale/mismatched call-zero behavior. The amended service
suite passes 69/69 under root verification.

## 2026-07-22 production-adapter freeze amendment

The service core now includes the production MCP and OAuth adapters that were still follow-up work
in the original summary. MCP schema discovery and an authorized call are pinned to one exact
configuration/auth revision and one of two bounded leases; idle reaping releases the connection or
subprocess. OAuth uses one typed, bounded continuation, a bounded token-exchange timeout, exact
reservation transfer, and checked retained-capacity accounting. Cancellation and configuration
invalidation remove queued dynamic browser/recovery URLs before returning. Known-delivered tool
success remains `Succeeded` after a late UI cancellation, while an unknown delivery is never
retried.

All public OAuth port `Debug` implementations and Connector intent/draft diagnostics expose only
low-cardinality state, booleans, counts, revisions, and `REDACTED`; raw provider/user coordinates,
URLs, commands, environment values, operation IDs, client data, and workspace hints are excluded.
The coordinator still performs zero thread/repository open/wake on construction and retains only
the latest snapshot.

Root verification after this amendment:

- `cargo test -p connector-contract`: 8/8 plus doc-tests passed.
- `cargo test -p connector-service -- --test-threads=1`: 61/61 plus doc-tests passed.
- All-target check and strict Clippy passed for both crates.
- Package rustfmt and `git diff --check` passed.
- `check-deps` passed for 23 crates with no forbidden edge/cycle.
- `check-boundary` passed at the unchanged 53 existing exceptions.

The app-host import continuation is now exact and bounded as well. Paste remains a direct
`ImportConfiguration`; file-picker and Claude Desktop requests reserve one outstanding host slot
and return an operation ID. Only a matching source plus that exact ID may complete or fail the
request. Cancel clears it without publishing an error, while stale, mismatched, direct-file bypass,
and double completion fail before worker/repository start. Success and low-cardinality host failure
reuse the original operation ID in the snapshot. Host action `Debug` omits operation/server IDs and
dynamic URLs. The amended connector-service suite passes 62/62 serially.

## Outcome

`connector-service` now owns a synchronous, bounded Connector coordinator without knowing a
concrete database, keyring, application, UI, runtime, terminal, or storage row. Constructing the
coordinator, reading its initial snapshot, and rendering an unopened Connector surface start no
thread and open no repository. The first service-backed intent lazily starts one worker and calls
the app-provided `ConnectorRepositoryFactory` on that worker, so the adapter can open exactly one
dedicated SQLite connection for each active worker lifetime.

The worker publishes through one `AtomicU64` revision plus one `Mutex<Arc<ConnectorSnapshot>>`.
There is no snapshot/event channel or historical backlog. `SnapshotReader::refresh` performs only
an atomic load on unchanged frames and clones the current `Arc` only after revision changes.

## Public capability boundary

- `ConnectorRepositoryFactory` and `ConnectorRepository` expose storage-neutral overview,
  selected-server, selected-tool-page, atomic mutation, Slack ensure, and bounded import methods.
- `ConnectorSecrets` exposes logical credential resolution and OAuth client storage only through
  non-Clone/non-Serialize/redacted `SensitiveInput`; no `SecretString`, OAuth token, or physical
  keyring type crosses the service API.
- `ConnectorMcp` exposes discover, invoke-time live-schema loading, cancellation, lease reaping,
  and sanitized MC01 transport counters. Implementations must not retry
  `mcp::McpDeliveryUnknown`; `cancel_live_mcp_connection` consumes MC01's process kill/reap API.
- `ConnectorOAuth` exposes one correlated begin/submit/cancel flow. AU01 will extend the coordinator
  with the shared authorization/audit lifecycle without changing these resource primitives.
- App-only effects (`RequestImportPicker`, `OpenExternalUrl`) return `AppRequest` immediately and
  do not start the coordinator worker.

The production adapter must be implemented at the app composition root: its factory opens the
dedicated repository connection inside `open`, and its secret adapter resolves logical credential
IDs to SC01 physical slots. Neither concrete adapter belongs in this crate or a UI leaf.

## Resource and lifecycle semantics

| Resource | Bound / behavior |
| --- | --- |
| Command queue | `sync_channel(8)`; ninth queued command fails with backpressure |
| Published snapshots | One latest `Arc`; no event or snapshot backlog |
| MCP jobs | Maximum 2, including cancelled jobs until their backend thread really exits |
| OAuth jobs | Maximum 1, including cancelled jobs until completion |
| Completion queue | Capacity 3, equal to the total possible active jobs |
| Overview | Maximum 256 servers and 1 MiB of retained identity/name bytes |
| Selected tool page | Maximum 256 rows, 4,096 reported total, and 8 MiB actual row text |
| Discovered tools | Maximum 4,096 and 8 MiB using the larger of reported and actual text bytes |
| Import | Input/source each at most 1 MiB, 256 servers, 1 MiB draft text, 4,096 structural items |
| Tool input/result | 32 KiB input; result retained at most 1 MiB on a UTF-8 boundary |
| Diagnostic transitions | Latest 64 sanitized kind/phase/error-code tuples |
| Backend leases | Reported at most 2; idle worker asks the MCP adapter to reap expired leases |

Every job captures operation ID, per-server generation, and configuration revision. A cancelled,
superseded, or configuration-stale completion is discarded before persistence/result publication.
Operation IDs come from one coordinator-wide atomic sequence and remain unique across idle worker
restarts. Job handles and operation summaries are removed only after the backend exits and the
thread is joined. Generation entries retain only current overview servers and active jobs, including
the delete-while-active completion path, so server churn cannot grow the map indefinitely.

Backend calls are wrapped at the job boundary so a backend panic becomes a sanitized `Internal`
completion and releases its job slot instead of stranding a handle forever. No raw backend error,
URL, arguments, OAuth code, or secret is placed in diagnostics.

When the command queue and job maps are empty, the worker waits until the configured idle TTL. It
exits after leases reach zero, dropping its repository connection and worker-owned state. Waiting
uses blocking `recv_timeout`/thread wakeups; there is no Tokio runtime, timer thread, periodic UI
repaint, or polling while the Connector is unopened. A later intent resumes from the latest
snapshot with a new repository connection rather than reverting to a default revision.

## Verification

- Original PR baseline: `cargo test -p connector-service --no-fail-fast`: 12 passed, 0 failed.
- `cargo check -p connector-service`: passed.
- `cargo clippy -p connector-service --all-targets -- -D warnings`: passed.
- `cargo run -q -p xtask -- check-deps`: passed for 23 crates; forbidden edges/cycles 0.
- `cargo run -q -p xtask -- check-boundary`: passed with 53 existing exceptions; increase 0.
- `cargo fmt -p connector-service -- --check`: passed.
- `git diff --check -- crates/connector-service docs/build/PR-SV01-summary.md`: passed.
- Scoped forbidden-symbol scan found no concrete DB/keyring, storage/mcp-store, Tokio, runtime
  command, process, network client, or file-picker use. The only `Db` text is API documentation
  explaining that the port does not expose it.

Deterministic fake repository/MCP/OAuth/clock coverage proves zero unused worker/repository opens,
the exact queue-8 boundary, atomic-only unchanged snapshot reads, MCP-2/OAuth-1 concurrency,
configuration-stale result rejection, no capacity reuse before cancelled backend exit, panic
cleanup, fake-clock idle exit without sleeps/polling, generation pruning under server churn,
pre-persistence import ceilings, and inclusive 4,096-tool/8-MiB descriptor ceilings.

The first compile attempt was blocked before this crate by SC01's temporarily incomplete redaction
exports in the shared tree. After that lane completed its module, connector-service compiled. The
only focused test failure was a test race that observed the initial `active=0` before the injected
panic job started; the assertion now waits for the sanitized failure transition, and the full suite
passes. No production workaround or cross-lane edit was introduced.

## Follow-up integration

- IN01 must construct the concrete repository/secrets adapters only in `app.rs`, pass the UI a
  revision-cached `SnapshotReader`, dispatch at most one post-frame intent, and avoid a parallel old
  Connector path.
- MP01 supplies the lazy connection leases and TTL policy behind `ConnectorMcp`; SV01 owns only the
  coordinator/job bound and invokes `active_leases`/`reap_idle_leases`.
- AU01 replaces the temporary unresolved-approval error with the shared permission/audit state
  machine and maps delivery-unknown outcomes to durable `Unknown` without retry.
- Real-hardware release Scenario A-E CPU/RSS/frame-p95 and 30-minute slope approval remain BG01
  rollout gates; this PR supplies deterministic resource bounds, not hardware performance claims.
