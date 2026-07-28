# SF02 Web Push Admission Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep web-push session notification work bounded and coalesced under slow or failing delivery.

**Architecture:** Preserve the single push worker and its retry cadence. Centralize all `SessionJob` insertion through one helper that coalesces by session, gives `Done` precedence over `Waiting`, and enforces one combined 256-job budget across fresh and retry queues.

**Tech Stack:** Rust 2024, `VecDeque`, `Mutex`/`Condvar`, existing `PushTransport` fake, crate-local tests.

---

### Task 1: Add failing queue-admission tests

**Files:**
- Modify: `crates/web-remote/src/push.rs:455`
- Test: `crates/web-remote/src/push.rs:1800`

- [ ] **Step 1: Define the production limit**

```rust
const MAX_SESSION_JOBS: usize = 256;
```

- [ ] **Step 2: Add pure `PushInner` tests**

Construct `PushInner` with empty queues and call a new helper:

```rust
fn enqueue_session_job(
    inner: &mut PushInner,
    job: SessionJob,
    destination: SessionJobQueue,
) -> bool
```

Tests must cover:

```rust
assert_eq!(inner.jobs.len() + inner.retry_jobs.len(), MAX_SESSION_JOBS);
assert_eq!(job_for("same").kind, SessionKind::Done);
```

Add cases for 1,000 identical admissions, `Waiting -> Done`, retry replacement, and 257 unique
sessions.

- [ ] **Step 3: Verify failure**

```bash
cargo test -p web-remote --locked session_job_admission -- --test-threads=1
```

Expected: FAIL because direct `push_back` has no helper or cap.

### Task 2: Implement latest-state coalescing

**Files:**
- Modify: `crates/web-remote/src/push.rs:455`
- Modify: `crates/web-remote/src/push.rs:655`
- Modify: `crates/web-remote/src/push.rs:740`

- [ ] **Step 1: Add queue helper functions**

Implement:

```rust
#[derive(Clone, Copy)]
enum SessionJobQueue {
    Fresh,
    Retry,
}

fn remove_session_jobs(queue: &mut VecDeque<SessionJob>, session: &str) -> Option<SessionJob>;
fn enqueue_session_job(
    inner: &mut PushInner,
    mut job: SessionJob,
    destination: SessionJobQueue,
) -> bool;
```

Rules:

1. Remove the same session from both queues before insert.
2. If either old or new kind is `Done`, retain `Done`.
3. `SessionJobQueue::Fresh` resets `attempts` to 0 and inserts into `jobs`.
4. `SessionJobQueue::Retry` preserves the incremented attempt count and inserts into `retry_jobs`.
5. Before insertion, evict oldest `jobs`, then oldest `retry_jobs`, until combined size is below 256.
6. Return `true` when an eviction occurred so the caller can increment one low-cardinality counter/log.

- [ ] **Step 2: Replace both direct `push_back` paths**

Replace `notify_session`'s `inner.jobs.push_back` and the retry branch's
`inner.retry_jobs.push_back` with the centralized helper. Keep `notify_all` and retry cadence unchanged.

- [ ] **Step 3: Run admission tests**

```bash
cargo test -p web-remote --locked session_job_admission -- --test-threads=1
```

Expected: PASS.

### Task 3: Prove slow/failing worker behavior

**Files:**
- Test: `crates/web-remote/src/push.rs:1800`

- [ ] **Step 1: Add stalled transport regression**

Reuse the existing fake transport and short poll interval. Admit more than 256 unique session jobs
while delivery fails. Assert retained jobs stay bounded, attempts stop at `SESSION_SEND_ATTEMPTS`, and
dropping the manager joins the worker.

- [ ] **Step 2: Run push worker tests**

```bash
cargo test -p web-remote --locked push -- --test-threads=1
```

Expected: PASS with no test waiting on a real endpoint.

### Task 4: Validate and document SF02

**Files:**
- Create: `docs/build/PR-SF02-summary.md`

- [ ] **Step 1: Run required gates**

```bash
cargo test -p web-remote --locked push -- --test-threads=1
cargo clippy -p web-remote --all-targets --locked -- -D warnings
git diff --check
```

- [ ] **Step 2: Record exact results and rollback**

Document queue cap, coalescing precedence, retry count, tests, and that no schema/API changed.

- [ ] **Step 3: Commit**

```bash
git add crates/web-remote/src/push.rs docs/build/PR-SF02-summary.md
git commit -m "fix(web-remote): bound push session jobs"
```
