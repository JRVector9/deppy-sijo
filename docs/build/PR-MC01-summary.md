# PR-MC01 — MCP limits, cancellation, and bounded sync HTTP sends

## Outcome

- `tools/list` now fails closed at 4,096 cumulative tools or 8 MiB of cumulative serialized
  descriptors/cursor metadata, detects repeated cursors immediately, and retains the existing
  100-page round-trip ceiling.
- MCP-owned payload primitives expose the frozen 32 KiB tool-input and 8 MiB raw-response ceilings.
  Oversized/non-object tool arguments are rejected before stdio spawn or HTTP connect.
- Stdio response/write timeout and explicit `McpConnection::cancel()` close stdin, kill the process
  group, reap the direct child, drop the bounded event receiver, and join stdout/stderr/writer
  threads.
- Sync ureq sends are bounded to two. A timed-out sender owns its permit until the blocking request
  really exits; its `JoinHandle` is retained in a two-entry opportunistic reaper and joined before a
  later send. No persistent reaper thread or async runtime was added.
- `tools/call` session-expiry/header-timeout paths return `McpDeliveryUnknown` and are never
  automatically retried. Safe discovery requests retain the existing one-time session recovery.
- Process-wide atomic transport metrics count current/peak helper threads, current/peak HTTP send
  permits, and pending timed-out sender handles without polling or idle threads.

## Public API

- Limits: `MAX_TOOLS_PER_SERVER`, `MAX_TOOL_DESCRIPTOR_BYTES`, `MAX_TOOL_INPUT_BYTES`,
  `MAX_RAW_MCP_RESPONSE_BYTES`, `MAX_HTTP_SENDS`.
- Payload checks: `McpPayloadKind`, `enforce_payload_bytes`, `enforce_json_payload`.
- Diagnostics: `McpTransportMetrics`, `transport_metrics()`.
- Cancellation/outcome: `McpConnection::cancel()`, `McpDeliveryUnknown`.

No `connector-contract` dependency was added. The constants intentionally mirror its frozen
production ceiling, so root manifests and `Cargo.lock` did not change.

## Resource semantics

| Resource | Production ceiling / behavior |
| --- | --- |
| Tools per server | 4,096 cumulative across pages |
| Tool descriptors/cursors | 8 MiB cumulative serialized/retained bytes |
| Tool input | 32 KiB serialized arguments, checked before external I/O |
| Raw JSON/SSE response | 8 MiB; SSE total includes intermediate notifications |
| HTTP blocking sends/sockets | 2 including caller-timed-out sends |
| Timed-out sender handles | 2, opportunistically joined; no resident reaper thread |
| Stdio helpers | stdout/stderr/writer counted; cancellation joins all owned helpers |

ureq 2 cannot honestly guarantee hard cancellation of a request already inside blocking socket I/O.
The caller deadline therefore controls return latency, while the background sender retains the
permit until the OS/library call exits. Backpressure rejects/waits within the original deadline
rather than creating another sender or socket.

## Verification

- `cargo check -p mcp`: pass.
- `cargo clippy -p mcp --all-targets -- -D warnings`: pass.
- `cargo fmt -p mcp -- --check`: pass.
- `git diff --check -- crates/mcp`: pass.
- New pure/process focused coverage: payload ceilings 2/2, cumulative discovery 2/2, repeated HTTP
  deadline/backpressure/reaper 1/1 (four cycles), explicit stdio cancel/metrics 1/1, and pre-I/O
  oversized input rejection 1/1.
- One full run reached all loopback fixtures and passed 77/78; its only failure was the old test
  expecting the superseded 64 KiB transport error instead of the new 32 KiB input error. After the
  assertion was corrected, that exact test passed.
- A later full serial run passed 49/78 and the remaining 29 tests all stopped at local
  `TcpListener::bind` with `Operation not permitted`, matching the managed-sandbox baseline. The new
  loopback `tools/call` Unknown/no-retry test had passed in the earlier listener-capable run.

## Scope and follow-up

- Modified only `crates/mcp` plus this summary. No Tokio/runtime, allowlist, app, storage, audit,
  secret, root manifest, lockfile, xtask, or handoff change was made by this lane.
- PR-SV01 can consume the metrics and `McpConnection::cancel()` from its bounded coordinator.
- PR-AU01 must map `McpDeliveryUnknown` to durable audit `Unknown` and must not enqueue a retry.
- PR-MP01 remains responsible for long-lived backend session leases and idle-TTL reclamation; MC01
  bounds individual sync transport operations but does not introduce connection pooling.
