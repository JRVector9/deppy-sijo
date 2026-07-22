# PR-AU01 — Shared Authorization and Durable Audit Lifecycle

## Outcome

PR-AU01 gives the GUI Connector service and `deppy-mcp-proxy` one shared authorization model and
one durable audit lifecycle. An external tool call is reachable only after exact live-schema
discovery, exact permission-state evaluation, and a committed audit preflight. A transmitted call
whose delivery cannot be proven is persisted as `Unknown` and is never automatically retried.

This PR deliberately does not cut the application UI over. The old mixed `connectors.rs` path and
its 53 existing boundary exceptions remain until PR-IN01 can replace them atomically. No allowlist
entry, Connector `RuntimeCommand`, Tokio runtime, background polling loop, or second production
authorization path was added.

## Shared authorization protocol

The common order is:

1. Validate the bounded JSON-RPC/tool input before permission, approval, audit, or transport work.
2. Read the live schema over the exact backend connection/version that will be used for the call.
3. Load an exact `PermissionFingerprint`: an absent row is distinct from persisted Ask/Allow/Deny.
4. Evaluate permission and, when required, resolve one opaque approval token.
5. In one storage transaction, revalidate the permission fingerprint, apply AllowAlways or
   DenyAlways, and commit a Prepared or Denied audit preflight.
6. Consume a non-Clone grant bound to operation ID, server, tool, live schema, decision, and the
   digest of the exact validated input bytes.
7. Execute at most one external call and durably record Succeeded, Failed, or Unknown.

Invalid JSON, a scalar/array/null argument, stale config/auth/schema, cancellation, permission
mutation, audit preflight failure, redaction-lease failure, and grant mismatch all stop before the
external call. Denied preflights produce no callable grant. Raw encrypted audit input remains NULL
unless an explicit encryptor is supplied; production Connector/proxy paths do not supply one.

`AuthorizationPlan`, pending approval, grant, and authorized-call types have no public raw
constructors. Rust has no friend-crate visibility, so the one owner-aware cross-crate preflight
bridge is hidden and the root xtask law enforces exactly one production callsite in the storage
transaction wrapper. The same law rejects raw proof APIs and the lossy compatibility evaluator
outside audit, requires exact-fingerprint calls in both GUI service and proxy, scans complete files
instead of dropping code after `#[cfg(test)]`, and catches direct/imported identifiers.

## Owner and crash recovery

- One non-Clone `ActiveAuthorizationOwner` holds an exclusive OS file lock for its complete
  lifetime and binds scope, random run ID, and the exact physical database identity.
- Unix uses device+inode identity; Windows uses volume+file identity. Unsupported targets have a
  canonical-path fallback. Hard links share one identity; atomic file replacement obtains a new
  identity.
- Each database uses exactly 256 hash-striped lock files. High-cardinality scopes cannot grow a
  lock-file registry; collisions serialize conservatively.
- Startup/takeover reconciles only Prepared rows from the same scope and a different run. It never
  performs a global sweep that could mark another live process Unknown.
- Graceful close and crash recovery are scope/run exact. Same-outcome completion is idempotent;
  conflicting outcomes and stale/cross-database owners are rejected.
- The legacy completion path can update only owner-null legacy rows and cannot finalize an
  owner-scoped AU01 operation.

## Delivery and secret safety

- MCP proxy input has a 64 KiB envelope ceiling and a 32 KiB tool-argument ceiling. Parsing uses a
  borrowed raw value, then transfers one non-Clone/non-Serialize/redacted-Debug sensitive input.
- Growing buffers wipe their old allocation before replacement. Parsed values, request-line
  buffers, serialized HTTP/stdin bodies, panic-unwind paths, and detached timeout bodies are
  zeroized on release.
- Stdio and HTTP distinguish deterministic pre-I/O failure from post-send ambiguity. Timeout,
  partial write, EOF, redirect/session loss, malformed response, cancellation, and other untrusted
  post-send outcomes are `Unknown`; a matching JSON-RPC tool error is known `Failed`.
- Unknown is never resent. A failed outcome write retains one bounded obligation and one MCP
  capacity slot; later calls fail closed until the outcome-only write succeeds.
- Proxy startup does not enumerate credentials or read keyring secrets. The first actual cold
  connection resolves only the exact logical credential, parses its versioned physical slot,
  proves `belongs_to`, and acquires one fail-closed redaction lease.
- Storage performs the same logical-ID/physical-slot ownership proof before starting the atomic
  pointer/OAuth metadata transaction. Cross-owner, corrupt, unversioned, and legacy logical
  pointers cannot enter the new secret-backed execution path; corrupted pointers cause keyring
  read zero.
- Config revision and exact physical auth-slot revision jointly bind schema discovery to call.
  URL/config/credential rotation drops the stale lease and causes call zero until a fresh schema
  request succeeds.
- Logs and diagnostics use low-cardinality error codes and never record raw URL, tool arguments,
  OAuth/token material, environment values, or raw backend errors.

## Bounded resources and proxy reuse

| Resource | Ceiling / behavior |
| --- | --- |
| Connector command queue | 8, backpressure on overflow |
| Active Connector MCP operations | 2 |
| Pending approval input | Occupies one of the two MCP slots; canceled/stale input is dropped |
| Proxy unresolved outcome obligations | 1; later calls fail closed |
| Proxy backend leases | 2-entry LRU |
| Proxy idle TTL | 30 seconds in production; no periodic ticker |
| Authorization owner locks | 256 files per physical database |
| Proxy request envelope | 64 KiB |
| Tool argument input | 32 KiB |

Live schema discovery and `tools/call` share the same warm backend connection. A known tool/server
error keeps a healthy lease; delivery ambiguity poisons and releases it. Missing, disabled, or
malformed targets fail before connection, and target lookup is a bounded point read rather than a
full server scan.

The actual local stdio fixture measured approximately 94.04 ms cold discovery and 120.46 µs warm
call latency with one backend spawn. The retained backend process reported approximately 2,064 KiB
RSS and was reclaimed after the fixture's shortened 50 ms TTL. These are focused local measurements,
not release Scenario A–E approval; PR-OD01/PR-BG01 still own 30-minute slope and real-hardware
terminal frame/CPU/RSS non-regression gates.

## Verification

- Root combined/split focused suites: audit 48, connector-service 31, mcp-store 18, storage 87,
  and mcp-proxy 39 — 223 tests pass.
- MCP authorized proxy: 20/20 pass.
- MCP stdio transport/delivery classification: 14/14 pass.
- Sensitive buffer/body filter: 3/3 pass; two overlap the proxy suite and one covers HTTP body
  cleanup.
- Authorization xtask gates: 2/2 pass, including mid-file `#[cfg(test)]` and import-alias bypass
  fixtures.
- `cargo check` passes for audit, storage, mcp-store, mcp, connector-service, mcp-proxy, and xtask.
- Strict all-target Clippy with `-D warnings` passes for the same packages, with audit
  `test-support` enabled only for tests.
- Normal/build feature trees for connector-service and mcp-proxy contain no audit `test-support`.
- Storage secret-like persistence 3/3, database plaintext-secret 1/1, and mcp-store secret-like
  persistence 2/2 pass.
- `check-deps` passes for 23 crates with zero forbidden edges/cycles.
- `check-boundary` passes at exactly 53 pre-existing exceptions; allowlist increase is zero.
- Workspace rustfmt and `git diff --check` pass.

One aggregate parallel run passed 222/223 but the real-HTTP mock thread reset the connection before
product assertions after failing to read its initialize request. The exact test then passed alone,
and the complete proxy suite passed 39/39 with one test thread. The managed sandbox also denies
many MCP localhost listener fixtures with `EPERM`; root therefore used the non-listener MCP suites
above. A loopback-capable environment must rerun the complete MCP HTTP suite before production
rollout.

## Failed approaches and follow-up gates

- Reopening the same WAL database through a hard-link alias produced SQLite disk-I/O behavior
  before reaching the lock assertion. Physical dev+inode equivalence plus exact OS-lock exclusion
  now proves the intended invariant without claiming hard-link SQLite access is supported.
- A feature-tree scan that included dev dependencies falsely reported audit `test-support` in
  production. The final gate uses Cargo's normal/build edge set.
- A first focused xtask command used `--exact` without the Rust module prefix and selected zero
  tests; the corrected unique filter selected and passed the intended tests.
- The pre-IN01 `insert_credential` path still stores a logical ID in `keyring_username`, and old
  app/dotenv/Connector callers still use it. PR-IN01 must stage/publish a typed physical slot before
  enabling the new proxy/service path and then remove the legacy interpretation.
- Secret-bearing `McpServerConfig` and `McpHttpServerConfig` still implement `Clone` for the old app
  Connector. PR-IN01 must delete that old path and remove these Clone requirements atomically.
- Remote-endpoint trust, OAuth/Slack host actions, event-driven egui wake, and same-snapshot Home
  projection must be complete before the app cutover. No long-lived old/new feature-flag split is
  permitted.
