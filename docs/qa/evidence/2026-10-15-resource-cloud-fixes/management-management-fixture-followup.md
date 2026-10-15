# Fixture qualification follow-up

Root's first combined App cloud gate actually ran: 174 PASS / 1 FAIL / 3 ignored. The sole failure was `shutdown_drains_one_accepted_locked_stale_read_without_retrying`: ended before lock release in 223ms, but two completions rather than the expected one. Source/log inspection confirms both were valid already accepted jobs: the fixture's first `drain_history()` opens history, but its wait loop calls `dispatch_history()` rather than `poll_management()`, so initial run bootstrap was still uninitialized. `poll_management()` under the later SQLite lock admitted both ListRuns bootstrap and the intended GetRun stale query.

The fixture now explicitly polls and drains the empty run bootstrap before seeding the stale run or holding the lock, and asserts successful bootstrap initialization. It also observes all three initial actor wake callbacks (OpenPage, ListRuns, SaveRun) before starting the completion measurement, qualifying the worker's send-result-before-wake publication order. The lock is acquired only after this baseline barrier. The actual EXCLUSIVE lock, 700ms finite-close deadline, exact-one accepted stale completion assertion, unlocked control, rollback and worker join remain intact. No product code or assertion was weakened.

Apply `management-fixture-followup.patch` after the already integrated initial management patch. Its only path is `crates/app/src/cloud_agent/management.rs`, relative to previous final source SHA256 `b6c09a7a9ce98fcf72fd6a47c2d3b78594a0c5bb9620b10f36aa7f233db5ca04`; resulting SHA256 `73b2aef9a706fe46e6a64d231a2c44829fb93a575868d36f337f64688459a38f`. The original `management-fixes.patch` and its source-scope manifest are retained as the first delivery; this patch is the ordered fixture-only follow-up.

Actual worker checks: rustfmt on the owned module exit0; scoped diff --check exit0; follow-up patch applies to a private copy of the previous final source with git apply --check exit0. No Cargo/build/app/DB/provider/source-canonical operation was executed. Root rerun remains required; no GREEN claim.

Exact Root next commands, canonical cwd (Root sole Cargo):

```sh
git apply --check /private/tmp/deppy-cloud-fix-management-results-20261015/management-fixture-followup.patch
git apply /private/tmp/deppy-cloud-fix-management-results-20261015/management-fixture-followup.patch
CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-sijo --bin deppy-sijo cloud_agent::management::review_shutdown_20261015 -- --test-threads=1 --nocapture
```
