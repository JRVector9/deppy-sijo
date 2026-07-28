# PR-SF02 Build Summary

## Objective

Bound web-push session notification admission so slow or failing push endpoints
cannot let fresh and retry session queues grow without limit.

## Scope

- Modified only `crates/web-remote/src/push.rs`.
- Added this build summary.
- Did not change Cargo manifests, lockfiles, schema, public API, approval DB
  polling, subscription limits, retry count, or polling cadence.

## Changes

- Added `MAX_SESSION_JOBS = 256` for the combined fresh + retry session job
  budget.
- Centralized session job insertion through `enqueue_session_job`.
- Removed queued duplicates for the same session from both queues before
  insertion.
- Rejected an already-committed exact `(session, kind)` duplicate while holding
  the `PushInner` lock before admission or eviction.
- Added bounded `PushInner::in_flight_status` tracking for drained session jobs
  keyed by `(session, kind)`.
- Marked the whole drained session batch under the same `PushInner` lock before
  releasing it for network sends.
- Rejected exact in-flight duplicates before queued job removal or queue-cap
  eviction, so a full queue cannot lose an unsent different session to a job the
  worker is already sending.
- Removed in-flight keys on every worker exit path: zero subscriptions,
  already-notified skip, successful commit, retry requeue, and retry exhaustion.
- Preserved latest-state semantics with `Done` taking precedence over
  `Waiting`.
- Preserved the maximum old attempt count when replacing the same final session
  kind, including fresh duplicates of retry jobs, while resetting attempts for
  genuine state transitions such as `Waiting -> Done`.
- Evicted oldest fresh jobs first, then oldest retry jobs, before inserting
  when the combined queue is full.
- Logged a low-cardinality warning when admission evicts an old best-effort
  session state.

## Test-First Record

- Red test command:
  `cargo test -p web-remote --locked session_job_admission -- --test-threads=1`
- Red result: failed before implementation with 12 compile errors because
  `enqueue_session_job` and `SessionJobQueue` did not exist yet (`E0425`,
  `E0433`).
- First green result after helper implementation: 4 passed, 0 failed, 140
  filtered.
- After adding the worker-level stalled sender regression, the same focused
  admission command passed again: 4 passed, 0 failed, 141 filtered.
- Review-fix red test command:
  `cargo test -p web-remote --locked session_job_admission -- --test-threads=1`
- Review-fix red result: failed as expected with 4 passed, 2 failed, 141
  filtered. The failures showed committed duplicate admission still returned
  eviction and fresh duplicate `Done` reset attempts from 2 to 0.
- Review-fix green result after helper correction: 6 passed, 0 failed, 141
  filtered.
- SF02 re-review red test command:
  `cargo test -p web-remote --locked session_job_admission -- --test-threads=1`
- SF02 re-review red result: failed as expected with 7 compile errors because
  `PushInner::in_flight_status` did not exist (`E0560`, `E0609`).
- SF02 re-review first green result after in-flight tracking: 7 passed, 0
  failed, 146 filtered.

## Final Gate Results

- `cargo test -p web-remote --locked session_job_admission -- --test-threads=1`
  - PASS
  - 7 passed, 0 failed, 0 ignored, 146 filtered
- `cargo test -p web-remote --locked push -- --test-threads=1`
  - PASS
  - 42 passed, 0 failed, 0 ignored, 111 filtered
- `cargo clippy -p web-remote --all-targets --locked -- -D warnings`
  - PASS
- `git diff --check`
  - PASS

## Corrections During Implementation

- `cargo clippy -p web-remote --all-targets --locked -- -D warnings` initially
  failed once after a review tightening with `clippy::collapsible-if` in
  `remove_session_jobs`.
- Corrected the helper shape and reran the final test, Clippy, and diff gates
  successfully.
- Standalone `rustfmt crates/web-remote/src/push.rs` failed because it did not
  infer the crate's Rust 2024 edition for an existing let-chain; reran
  `cargo fmt -p web-remote` successfully.

## Coverage Added

- 1,000 same-session admissions coalesce to one job.
- `Waiting -> Done` leaves one final `Done` job.
- Retry admission replaces a fresh duplicate while preserving retry attempts.
- 257 unique queued session admissions retain exactly 256 jobs.
- A full queue plus an already-committed duplicate is rejected without evicting
  an unsent session.
- A full queue plus an exact in-flight duplicate is rejected without evicting an
  unsent session.
- A retry `Done` job at attempts=2 replaced by a fresh duplicate `Done`
  preserves attempts=2.
- A stalled/failing transport keeps retained jobs bounded at 256, drains after
  the existing 3 total send attempts, and does not prevent `stop_and_join`.
- In-flight state is cleared after successful commit, retry requeue, retry
  exhaustion, zero-subscription skip, and already-notified skip.

## Residual Risks

- Queue eviction is intentionally best-effort: if the combined queue is full,
  the oldest queued session state may be dropped to keep memory bounded.
- The retry cadence and three-attempt cap are unchanged; persistent endpoint
  failure can still lose best-effort session notifications after retries.
