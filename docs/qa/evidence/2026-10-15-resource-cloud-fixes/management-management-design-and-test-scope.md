# R01 + R05 management lane — ready for Root validation

Objective: seal management admission during final shutdown and recover native run bootstrap after transient SQLite failures. Source baseline: d8826f0b plus the exact dirty 422-source snapshot in `/private/tmp/deppy-cloud-fix-baseline-20261015/files/`.

Delivered patch: `management-fixes.patch` (unified diff against baseline bytes, not git HEAD). Only `crates/app/src/cloud_agent/management.rs` and the shutdown-entry hunk in `crates/app/src/cloud_agent.rs` change. `source-scope.json` verified all 422 source hashes: exactly these two changed; zero ownership violations. Canonical files/docs/Cargo, app processes, real DBs, providers, commits, and builds were not touched.

Design and invariants:

- `seal_management()` permanently marks management stopping and clears its retry deadline before connection shutdown or any completion drain begins.
- New public management preparations, local cancel requests, and automatic bootstrap/stale admissions stop after sealing. `poll_management()` first dispatches accepted local queued work, so sealed admission does not discard a retained durable save.
- `complete_management()` still processes accepted GetRun/Lookup/Save responses and finite local save continuations. Existing auth fence checks, durable Save cache installation, terminal lease retirement, claim cancellation, answer finish queue, and HistoryWorker shutdown are preserved.
- Bootstrap has independent success and in-flight state. Only successful ListRuns initializes it. Failed bootstrap/stale reads and failed stale saves set one 250ms maintenance retry deadline while active. It retains no job and creates no new work after shutdown; pending/byte/mutation caps stay intact.
- `management_poll_after()` wakes an empty cache for a failed bootstrap retry, merges queued/stale deadlines, and returns no active maintenance deadline after sealing. Successful bootstrap clears its prior bootstrap-read error.
- A fixed 250ms retry delay is deliberate: bounded per-failure delay, one in-flight bootstrap, no added task/thread, no retry attempt growth or exponential state. Active maintenance may retry until storage recovers; final shutdown never retries.

Existing behavioral RED evidence actually executed by the review and reused:

- `root-app-shutdown-stale-repro.log`: unlocked control PASS; locked shutdown FAIL, six completions before lock release, then cleanup/join.
- `root-app-bootstrap-repro.log`: FAIL after actual exclusive-lock ListRuns error plus successful same-worker recovery probe; native cache stayed empty.

Fixtures imported and adapted:

1. `review_shutdown_20261015::review_20261015_shutdown_unlocked_stale_run_finishes` — original unlocked control.
2. `review_shutdown_20261015::review_20261015_shutdown_does_not_reenqueue_failed_stale_reads_forever` — original 700ms externally locked close, preserving rollback and thread join before final assertions.
3. `tests::quality_review_20261015_bootstrap_recovers_after_transient_sqlite_lock` — actual locked ListRuns error, no extra jobs during the bounded retry interval, unlock, direct query on the same worker, production retry delay, durable completed result appears in cache, actor joined before cleanup.
4. `tests::shutdown_drains_accepted_stale_read_and_durable_save_continuation` — accepted stale GetRun becomes Unknown, external reopened SQLite confirms the follow-up save is durable before shutdown returns.
5. `tests::shutdown_does_not_retry_an_accepted_failed_bootstrap` — actual accepted bootstrap fails under an external lock while shutdown drains it, then no retry timer/job/repaint remains.
6. `review_shutdown_20261015::shutdown_drains_one_accepted_locked_stale_read_without_retrying` — exactly one admitted query is drained before lock release; no replacement query is created.

Actual worker checks:

- `rustfmt --edition 2024 --config skip_children=true crates/app/src/cloud_agent.rs crates/app/src/cloud_agent/management.rs` — exit 0.
- `git diff --check -- crates/app/src/cloud_agent.rs crates/app/src/cloud_agent/management.rs` — exit 0.
- `git apply --check /private/tmp/deppy-cloud-fix-management-results-20261015/management-fixes.patch` against private copies of the baseline files — exit 0.
- Scope/hash comparison of all 422 baseline sources — two owned changes, zero violations.
- No Cargo test or compilation was executed by this worker. The added tests are pending Root execution; no GREEN claim is made.

Failed approaches: no executed failed implementation or test command. Initial design considered rejecting non-Save stages in the common enqueue function; source audit rejected that design before implementation so accepted finite continuations are preserved instead.

Root integration/verification commands (Root alone, canonical cwd):

```sh
git apply --check /private/tmp/deppy-cloud-fix-management-results-20261015/management-fixes.patch
git apply /private/tmp/deppy-cloud-fix-management-results-20261015/management-fixes.patch
CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-sijo --bin deppy-sijo cloud_agent::management -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-sijo --bin deppy-sijo pr7_shutdown_cancels_pending_input_claim_and_drains_accepted_answer_finish -- --test-threads=1
```

Root may need explicit shared cloud_agent.rs hunk integration because its metadata/cache lane edits that file concurrently. Do not replace the entire file with this worktree's copy. Update canonical handoff with real command logs/results after integration. Remaining work: Root Cargo, independent review, integration gates, and cross-review of the transport lane when assigned. No release build or restart is part of this task; source version remains 0.12.1.
