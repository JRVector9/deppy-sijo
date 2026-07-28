# SF04 RemoteRuntime Deadlines And Liveness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound initial RemoteRuntime TCP connection time and terminate silent half-open clients after three missed heartbeat intervals.

**Architecture:** Add a pure time-based liveness tracker under `remote/liveness.rs`, use `TcpStream::connect_timeout` through a testable connector helper, and check the tracker in plain/TLS client IO loops. Do not add automatic reconnect or change wire bytes.

**Tech Stack:** Rust 2024, `TcpStream`, rustls, `Instant`/`Duration`, existing framed heartbeat protocol and runtime tests.

---

### Task 1: Add a pure liveness state module

**Files:**
- Create: `crates/runtime/src/remote/liveness.rs`
- Modify: `crates/runtime/src/remote.rs:40`

- [ ] **Step 1: Write the failing module tests**

Define tests for exact boundary, just-before boundary, and observed frame reset.

```rust
pub(super) struct LivenessTracker {
    last_received: Instant,
    timeout: Duration,
}

impl LivenessTracker {
    pub(super) fn new(now: Instant, timeout: Duration) -> Self;
    pub(super) fn observe_frame(&mut self, now: Instant);
    pub(super) fn expired(&self, now: Instant) -> bool;
}
```

The exact rule is `now.saturating_duration_since(last_received) >= timeout`.

- [ ] **Step 2: Run and verify failure**

```bash
cargo test -p runtime --locked liveness_tracker -- --test-threads=1
```

Expected: compile failure until the module is declared and implemented.

- [ ] **Step 3: Implement the minimal tracker**

Add `mod liveness;` inside `remote.rs`, implement the three methods, and use a constant:

```rust
const CLIENT_LIVENESS_TIMEOUT: Duration = Duration::from_secs(45);
```

Add a test assertion that this remains exactly three `HEARTBEAT_INTERVAL`s.

- [ ] **Step 4: Run tracker tests**

Expected: PASS.

### Task 2: Bound plain and TLS connect

**Files:**
- Modify: `crates/runtime/src/remote.rs:1485`

- [ ] **Step 1: Add injectable connector test**

Add a private helper accepting a closure so the test records the supplied timeout without waiting:

```rust
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

fn connect_with(
    addr: SocketAddr,
    connect: impl FnOnce(&SocketAddr, Duration) -> std::io::Result<TcpStream>,
) -> std::io::Result<TcpStream> {
    connect(&addr, CONNECT_TIMEOUT)
}
```

The test closure must assert `timeout == Duration::from_secs(30)` and return `TimedOut`.

- [ ] **Step 2: Verify the test fails before production wiring**

```bash
cargo test -p runtime --locked connect_timeout -- --test-threads=1
```

- [ ] **Step 3: Replace both direct connects**

Use:

```rust
connect_with(addr, TcpStream::connect_timeout)
```

in plain `attach` and TLS `attach_tls_inner`. Preserve all auth/TLS timeout setup after connect.

### Task 3: Enforce missed-heartbeat shutdown

**Files:**
- Modify: `crates/runtime/src/remote.rs:1530`
- Modify: `crates/runtime/src/remote.rs:1837`

- [ ] **Step 1: Add silent-peer loop tests with injected time**

Factor frame observation/expiry decisions into the pure tracker rather than sleeping 45 seconds.
Tests must prove empty heartbeat frames call `observe_frame`, ordinary frames do too, and an idle poll at
the deadline requests disconnect.

- [ ] **Step 2: Convert the plain reader to resumable timed frames**

After cloning the plain reader socket, call
`set_read_timeout(Some(Duration::from_millis(100)))` and replace the blocking
`BufReader + read_frame` loop with the existing `FrameDecoder`:

```rust
let mut decoder = FrameDecoder::new();
loop {
    match decoder.advance(&mut reader_stream) {
        FramePoll::Frame(frame) => { /* observe + dispatch */ }
        FramePoll::Pending => { /* expiry check; read timeout already bounds the poll */ }
        FramePoll::Closed => break,
    }
}
```

Extend `FrameDecoder::advance` so `TimedOut` is treated like `WouldBlock`, preserving partial frame
bytes. Make `advance` yield `Pending` after each incomplete successful read rather than blocking for an
unbounded sequence of partial reads; this guarantees the liveness deadline is checked even if a peer
trickles an incomplete frame. Using `read_timeout + read_exact` is forbidden because a timeout after a
partial prefix/payload would desynchronize framing.

Do not call `set_nonblocking(true)` on the cloned plain reader: `TcpStream::try_clone` shares socket
state on supported platforms, so that can unexpectedly make the mutex-protected plain writer
non-blocking and change command delivery behavior.

- [ ] **Step 3: Wire plain and TLS liveness checks**

Create a tracker when each client IO/reader loop starts. On every decoded frame—including empty
heartbeat—call `observe_frame(Instant::now())`. Check expiry after every decoder/read iteration, not
only when transport bytes are idle, so an incomplete-frame trickle cannot suppress the deadline. Break
the loop when expired and emit one low-cardinality warning. Socket shutdown and existing `Drop` join
remain the teardown path.

- [ ] **Step 4: Run remote tests**

```bash
cargo test -p runtime --locked remote -- --test-threads=1
```

Expected: auth, heartbeat, slow consumer, TLS, and shutdown tests pass.

### Task 4: Validate and document SF04

**Files:**
- Create: `docs/build/PR-SF04-summary.md`

- [ ] **Step 1: Run required gates**

```bash
cargo test -p runtime --locked remote -- --test-threads=1
cargo clippy -p runtime --all-targets --locked -- -D warnings
git diff --check
```

- [ ] **Step 2: Record scope exclusions**

State that no reconnect manager, SSH transport, protocol version, or app UI was added.

- [ ] **Step 3: Commit**

```bash
git add crates/runtime/src/remote.rs crates/runtime/src/remote/liveness.rs docs/build/PR-SF04-summary.md
git commit -m "fix(runtime): bound remote client liveness"
```
