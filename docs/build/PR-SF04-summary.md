# PR-SF04 Remote Runtime Liveness Summary

## Scope

- Added a pure `LivenessTracker` for client-side received-frame deadlines.
- Bounded plain and TLS initial TCP connect with a 30 second `TcpStream::connect_timeout` deadline.
- Converted the plain client reader from blocking `read_frame` to timed `FrameDecoder` polling.
- Added client liveness checks to plain and TLS receive loops. Empty heartbeat frames and ordinary frames both refresh `last_received`.
- Preserved existing public wrappers: `attach`, `attach_tls`, and `attach_tls_tofu`.

## Scope exclusions

- No automatic reconnect manager.
- No SSH transport or relay-owned PTY behavior.
- No protocol version or wire-byte changes.
- No app UI changes.
- No detached retry thread, timer thread, or background reconnect/backoff loop.

## Test-first record

1. `cargo test -p runtime --locked liveness_tracker -- --test-threads=1`
   - Result: FAILED as expected.
   - Count: 0 passed; compile failed before running tests.
   - Failure: `E0433` unresolved `LivenessTracker` in `crates/runtime/src/remote/liveness.rs`.
   - Correction: implemented `LivenessTracker`, `CLIENT_LIVENESS_TIMEOUT`, and `mod liveness`.

2. `cargo test -p runtime --locked liveness_tracker -- --test-threads=1`
   - Result: PASSED.
   - Count: 4 passed; 0 failed; 208 filtered out.

3. `cargo test -p runtime --locked connect_timeout -- --test-threads=1`
   - Result: FAILED as expected.
   - Count: 0 passed; compile failed before running tests.
   - Failure: `E0425` unresolved `CONNECT_TIMEOUT` and `connect_with`.
   - Correction: added `CONNECT_TIMEOUT`, `connect_with`, and routed plain/TLS attach through `TcpStream::connect_timeout`.

4. `cargo test -p runtime --locked connect_timeout -- --test-threads=1`
   - Result: PASSED.
   - Count: 1 passed; 0 failed; 212 filtered out.

5. `cargo test -p runtime --locked remote -- --test-threads=1`
   - Result: FAILED as expected.
   - Count: 0 passed; compile failed before running tests.
   - Failure: `E0425` unresolved liveness helper functions for client frame observation and expiry.
   - Correction: added helper functions, decoder `TimedOut` handling, partial-read `Pending`, plain timed reader polling, and TLS liveness expiry checks.

6. `cargo test -p runtime --locked remote -- --test-threads=1`
   - Result: PASSED.
   - Count: 56 passed; 0 failed; 162 filtered out.

7. Added explicit client drop join regression tests for plain reader/liveness and TLS IO/liveness paths.

8. `cargo test -p runtime --locked remote -- --test-threads=1`
   - Result: PASSED.
   - Count: 58 passed; 0 failed; 162 filtered out.

## Required gates

1. `cargo test -p runtime --locked remote -- --test-threads=1`
   - Result: PASSED.
   - Count: 58 passed; 0 failed; 162 filtered out.

2. `cargo clippy -p runtime --all-targets --locked -- -D warnings`
   - Result: PASSED.
   - Count: command completed successfully with no warnings.

3. `git diff --check`
   - Result: PASSED.
   - Count: command completed successfully with no whitespace errors.

## Additional checks

1. `rustfmt --check crates/runtime/src/remote.rs crates/runtime/src/remote/liveness.rs`
   - Result: FAILED.
   - Failure: rustfmt defaulted below Rust 2024 and rejected existing let-chain syntax; it also showed a formatting diff in the new liveness test.
   - Correction: formatted the touched lines manually and reran with the explicit project edition.

2. `rustfmt --edition 2024 --check crates/runtime/src/remote.rs crates/runtime/src/remote/liveness.rs`
   - Result: FAILED on the first run.
   - Failure: three local line-wrap diffs in touched code.
   - Correction: applied the exact line-wrap changes manually.

3. `rustfmt --edition 2024 --check crates/runtime/src/remote.rs crates/runtime/src/remote/liveness.rs`
   - Result: PASSED.
   - Count: command completed successfully with no formatting diff.

## Modified files

- `crates/runtime/src/remote.rs`
- `crates/runtime/src/remote/liveness.rs`
- `docs/build/PR-SF04-summary.md`

## Residual risks

- The silent-peer 45 second disconnect behavior is covered with fake-time liveness decisions and decoder polling tests; the suite does not wait 45 real seconds on a live socket.
- TLS liveness uses the existing single IO loop; no separate liveness worker was added.
