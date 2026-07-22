# PR-MP01 — Reusable MCP proxy backend sessions

## Outcome

- The proxy now creates one lazy `BackendSession` and shares one `BackendClient` between live
  schema discovery in the permission hook and allowed `tools/call` forwarding.
- A successful first request retains the initialized `McpConnection`; subsequent list/call
  requests reuse it without another initialize handshake. Production creates no backend
  connection, subprocess, socket, timer thread, or polling loop before the first backend request.
- The session LRU retains at most two leases. A process-local, non-secret monotonic
  `ConfigRevision` invalidates stale leases and schema cache entries when config is replaced.
- The default idle TTL is 30 seconds. On Unix, the stdin reader blocks in one `poll(2)` call until
  either input arrives or the earliest retained lease expires, then cancels/drops the connection.
  When no lease exists it delegates to the original blocking stdin read, so idle overhead is zero.
- Tool/application errors retain a healthy connection. Protocol/transport state loss poisons and
  releases it. `McpDeliveryUnknown` poisons the connection and is returned without retry.

## Secret and ownership safety

- `BackendConfig` is intentionally non-`Clone`, non-`Serialize`, and has no derived `Debug`.
  `BackendTarget` clones only an `Arc<BackendConfig>`, so resolved stdio env values and HTTP
  bearer bytes are not copied for hook/forwarder requests.
- Revisions do not hash URLs, bearer values, or environment contents. The process-local
  `AtomicU64` generation contains no secret-derived material and is used only for cache/lease
  invalidation.
- No URL, arguments, bearer, environment values, or raw backend errors are logged by the session
  manager.

## Session and resource semantics

| Resource | Production ceiling / behavior |
| --- | --- |
| Backend leases | Two-entry LRU, successful use moves the lease to MRU |
| Lazy connect | First actual list/call only |
| Idle retention | 30 seconds by default |
| Idle scheduling | One deadline-blocking Unix `poll(2)`; no helper/ticker thread |
| Revision change | Cancel/drop old generation before a replacement is used |
| Healthy tool/JSON-RPC error | Lease retained |
| Protocol/transport error | Lease poisoned, canceled, and dropped |
| Delivery unknown | Lease poisoned; exact call count remains one |
| Proxy shutdown | Every retained lease canceled/dropped |

`McpConnection::cancel()` is the single cleanup path for both transports: stdio kills/reaps the
process group and joins its owned pipe threads; HTTP closes/releases the retained session. A
deterministic fake factory verifies TTL cleanup for both stdio and HTTP variants and observes two
cancels and zero live resources after reap.

## TTL selection and benchmark evidence

The deterministic comparison uses operations at seconds `[0, 5, 20, 49, 110]`, a 120 ms
handshake, 5 ms operation cost, and a 32 MiB-per-backend RSS proxy. The RSS number is a model input,
not a hardware measurement.

| TTL | Cold / warm | Projected total latency | Retained session time | Peak leases / RSS proxy |
| --- | --- | --- | --- | --- |
| 15 s | 4 / 1 | 505 ms | 65 lease-seconds | 1 / 32 MiB |
| 30 s | 2 / 3 | 265 ms | 109 lease-seconds | 1 / 32 MiB |
| 60 s | 2 / 3 | 265 ms | 169 lease-seconds | 1 / 32 MiB |

Thirty seconds is the Pareto knee: it has the same projected handshake latency as 60 seconds while
retaining the backend for 60 fewer lease-seconds, and halves cold connects compared with 15
seconds.

An actual local stdio fixture injected an 80 ms initialize delay and measured:

- cold schema discovery: 94.462458 ms;
- warm call on the same process/connection: 134.5 µs;
- backend spawn count: one across discovery and call;
- shortened 50 ms idle-TTL check: the retained child PID existed before expiry and did not exist
  after reap.

The managed sandbox denies `ps`/sysmon process inspection, so actual child RSS was unavailable.
Release RSS approval remains a localhost/process-observation-capable hardware gate; the
deterministic resource proxy must not be presented as measured RSS.

## Verification

- `cargo check -p mcp-proxy` — pass.
- Session focused tests — pass (lazy connect, cold/warm, two-entry LRU, generation invalidation,
  poison/no-retry, stdio+HTTP TTL cleanup, TTL model, actual stdio connection/process reuse).
- Hook/forwarder integration — pass: live-schema permission and forwarded call used one backend
  spawn, one cold connect, and one warm reuse.
- Full proxy suite had one listener-capable run at 30/30 before the final security refinement. The
  post-refinement run passed every non-HTTP test; its two existing HTTP fixtures were denied at
  `TcpListener::bind` with sandbox `Operation not permitted`.
- `cargo clippy -p mcp-proxy --all-targets -- -D warnings` — pass after the security refinement.
- Scoped rustfmt and `git diff --check -- crates/mcp-proxy docs/build/PR-MP01-summary.md` — pass.

## Scope and follow-up

- Modified only `crates/mcp-proxy` and this summary. No app, root manifest, lockfile, xtask,
  storage, audit, secret, mcp, connector-service/UI/contract, or handoff file was changed by this
  lane.
- PR-AU01 still owns the shared GUI/proxy authorization and durable audit lifecycle. This PR does
  not change the current permission/audit order.
- PR-OD01 owns production low-cardinality metrics and long soak/RSS-slope gates.
- The legacy `McpServerConfig`/`McpHttpServerConfig` types in `mcp` remain `Clone` for the
  old Connector path. Root must remove that legacy requirement with the IN01 atomic cutover; MP01
  does not clone them.
- The exact-deadline idle reader is implemented for macOS/Linux/Unix. Non-Unix builds retain the
  original blocking stdin semantics and clean resources on the next request or proxy shutdown.

## Event-driven approval wake amendment

- Local commit `5d145fe` adds an optional `--approval-notify-socket` path. When absent, proxy
  startup and idle behavior remain unchanged: no listener, sender socket, thread, timer, or poll.
- A policy `Ask` emits exactly one fixed one-byte, payload-free Unix datagram only after the
  pending approval row is durable. Malformed JSON, Allow, Deny, insert failure, and audit
  preflight failure emit none.
- The socket path is absolute, UTF-8, NUL-free, and at most 100 bytes. Debug and delivery errors
  redact the path. Delivery failure immediately denies the pending operation and prevents the
  backend call.
- Normal proxy return atomically denies pending approvals for its exact session. App-side
  `SessionExited` cleanup still owns crash/kill convergence and will replace the unconditional
  500 ms GUI watcher during the app boundary cutover.
- Root verification: proxy tests 48/48, all-target check, strict Clippy, package-scoped rustfmt,
  and diff-check passed. The managed sandbox rejected actual Unix socket binding in both the
  workspace and `/tmp` with `EPERM`; deterministic durable-before-wake coverage and a real
  missing-socket fail-closed test passed, while one real send/receive smoke remains a release
  environment gate.
