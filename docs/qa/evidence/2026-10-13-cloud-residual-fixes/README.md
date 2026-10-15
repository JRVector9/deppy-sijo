# Actual verification evidence — 2026-10-13

Source commit: `7e02a46d9b904be755691da82321d5089251a378`. Source/test version 0.12.1. No release artifact, push or App restart.

Run from `/Users/jr/Desktop/projects/deppy-sijo-performance`. Cargo jobs=2, locked dependencies. Logs retain actual tool stdout/stderr; only empty lines at EOF were removed. Empty fmt/diff logs correspond to actual exit0 commands.

## Final executed commands and results

```sh
CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo cloud_agent::
CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo ui::cloud_answer::
CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo ui::notifications::
CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo cloud_navigation_completion
CARGO_BUILD_JOBS=2 cargo test --locked -p web-remote
node crates/web-remote/tests/fixtures/run-cloud-results-mobile.mjs
CARGO_BUILD_JOBS=2 cargo clippy --locked -p deppy-sijo -p agent-mcp -p web-remote -p runtime -p secret --all-targets -- -D warnings
cargo fmt --all -- --check
cargo run --locked -p xtask -- check-boundary
git diff --check
```

Every final command above exited0. App counts155+2+28+1 PASS,3 ignored. Web403 PASS,13 ignored,doc0. Browser77 checks. Rust total589 PASS,16 ignored.

The first sequential final gate stopped at the boundary failure after all tests/browser/strict/fmt passed. After pure callback injection, affected App cloud/notifications/strict/fmt/boundary/diff were rerun successfully. Unchanged answer/completion/web/browser results are retained from the actual preceding execution; no old result is presented as a new run.

## Failed approaches and RED evidence

`notify-red.log`: four late-key answer regressions failed before fixes. `notify-ingress-notifications-red.log`: 5PASS/5FAIL before ingress/notification completion. `notifications-fragment-failed.log`: 7PASS/3FAIL from unsuitable clipped-fragment masking. Full bounded sources fixed this without changing SecretService.

`run-label-red.log`:0PASS/3FAIL. `run-reference-red.log`:0PASS/2FAIL footer/hover. `legacy-label-red.log`:0PASS/1FAIL. `answer-fallback-red.log`:0PASS/1FAIL. Actual producer/harness paths were used with synthetic credentials; routing IDs remain exact.

The two fixture-failed logs are not product regression RED: one-byte secrets are refused; adjacent secret spans merge into one marker. The valid separated fixture now passes. `boundary-first-failed.log` records the two actual leaf UI capability violations, repaired without an allowlist exception.

Earlier isolated N1/N2/N3 review evidence remains under `docs/reviews/evidence/2026-10-13-cloud-fixes-rereview/`. Runtime input used isolated PTY/SQLite; UI copy used egui OutputCommand. No provider accounts, live user DB, real OS sleep, native OS delivery or user App-to-PWA E2E was exercised.
