# PR-IN01 atomic Connector cutover

## Auth refresh exchange prerequisite

- Added and re-exported `auth::exchange_refresh_token_once(Duration, SecretString,
  &RefreshParams) -> anyhow::Result<RefreshOutcome>`.
- The function performs one synchronous request through the existing redirect-disabled
  `BoundedOAuthHttpClient`; it retains the existing timeout and 1 MiB OAuth response ceiling.
- The API has no store, database, keyring, coordinator, or publish parameter. It does not retry;
  the caller owns single-flight coordination and physical-slot publication.
- Legacy and typed-slot refresh paths now call the same public primitive, so this prerequisite does
  not introduce a second production exchange path.
- Focused tests cover success, exact one-request accounting, timeout, provider/transport error
  sanitization, no retry, and oversized-response rejection.

## Verification

- `cargo test -p auth one_shot_exchange --no-fail-fast -- --test-threads=1`: 5 passed.
- `cargo test -p auth --no-fail-fast -- --test-threads=1`: 101 passed; doc-tests passed.
- `cargo check -p auth --all-targets`: passed.
- `cargo clippy -p auth --all-targets -- -D warnings`: passed.
- `cargo fmt --package auth -- --check`: passed.
- `git diff --check -- crates/auth docs/build/PR-IN01-summary.md`: passed before this summary was
  added and rerun afterward.

The first focused run encountered the managed sandbox's transient localhost bind denial in all four
listener-backed cases. No product/test bypass was added; the subsequent full and focused reruns
executed those same cases successfully.
