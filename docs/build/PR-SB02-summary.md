# PR-SB02 lazy bounded environment workers

## Production cutover

- The environment project and secret-reveal paths now use the shared `LazyBoundedWorker`; the prior eager workers and oversized queues are no longer active production paths.
- Worker construction retains only a static thread name, the 30-second idle TTL, and closures. It creates no thread, channel, database handle, keyring handle, timer, polling loop, or repaint source until the first accepted request.
- The first accepted request starts one standard-library worker thread with bounded job and result channels. Its persistent executor lazily opens one `Db` on first execution and reuses it until idle retirement.
- Fixed low-cardinality thread names are used. After 30 idle seconds, a lifecycle-locked final empty check lets the worker exit; the next explicit request reaps the old generation and starts a fresh one.
- Results are published before the bounded wake callback. No Tokio runtime, background polling, or periodic repaint was added.

## Exact resource and admission contract

- Each worker permits exactly one outstanding request across running, queued, published-but-unread, and retired-pending states. This is stricter than channel capacity alone and prevents a second output from blocking behind an unread result.
- A second request returns the exact known-unsent job as `Full(job)` without replacement or automatic retry. A new request is accepted only after the previous outcome is consumed.
- A known-unsent disconnected job may retry once on a fresh generation. `Full` is never retried. A panicked operation is never retried automatically; the next explicit request is considered only after the error outcome is consumed.
- Jobs and outputs are excluded from diagnostics. Spawn, panic, and disconnect failures use static error codes only.
- Drop closes the result receiver and job sender before joining, so a blocked result send or idle receive is released.
- Generic limitation: if an arbitrary factory, executor, or wake callback never returns, joining its standard-library thread can still wait indefinitely because Rust provides no safe forced thread cancellation. Production adapters therefore keep factories path-only, wakes bounded, and operations bounded or cooperative.

## Render-to-logic boundary

- Render reads immutable `Arc`-backed cache state and emits an `EnvAction` intent. Secret reveal has at most one pending intent.
- `logic()` drains outcomes, admits intents and project jobs, and delegates database, keyring, and path work to the lazy workers. Render performs none of those operations.
- Project data is requested only while Settings Environment is open, the cache is absent, and no project request is outstanding. The former 25 ms retry repaint is absent.
- Secret infrastructure failures fail closed, invalidate the affected generation/cache, and reject the reveal. Generation checks discard stale outcomes.
- Project invalidation never clears an exact in-flight generation. A stale completion releases its
  one outstanding slot and the same logic pass admits the newest generation; Settings mutation
  outcomes drain before project admission so completion-time invalidation cannot strand an empty
  cache.
- Wake requests repaint only after an outcome has been published.

## Rejected races and hardening

- Rejected the eager always-live worker model and its oversized secret request/result queues.
- A channel-capacity-only design allowed one queued result plus a second blocked result; explicit cross-state outstanding admission now prevents it.
- Idle timeout and concurrent submission are serialized by the lifecycle lock and a final empty check, preventing a job from entering a generation that has committed to exit.
- Consuming an outcome while its worker was still inside the wake callback could admit a job to an exiting generation. A non-admitting `Publishing` lifecycle state now forces retirement and a fresh generation first.
- `catch_unwind` still invokes the process panic hook. A sanitized hook is installed before application startup, emits only static low-cardinality fields, and a subprocess test verifies that panic payloads are absent.

## Verification

- Focused lazy-worker and panic-policy suite: 13/13 passed, covering zero-resource construction, fixed thread names, persistent executor state, exact `Full(job)` behavior while active and published, publish/wake ordering, idle restart, drop, panic/no-spin behavior, secret-safe `Debug`, panic-hook sanitization, and startup ordering.
- Integrated Environment stale-generation source regression: 1/1 passed.
- `cargo check -p deppy-sijo --all-targets` passed.
- Strict focused Clippy passed: `cargo clippy -p deppy-sijo --test lazy_bounded_worker -- -D warnings`.
- Rustfmt checks for the touched worker and focused test passed, and `git diff --check` was clean.
- Long-duration soak and release resource measurements remain rollout evidence; this PR does not claim them from unit-level verification.
