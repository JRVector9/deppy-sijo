# Resource/cloud re-review evidence — 2026-10-15

Canonical: `/Users/jr/Desktop/projects/deppy-sijo-performance`, HEAD `d8826f0bdb8d89255f3448b5b61b6eee83faa87a`, source/test0.12.1 plus previous dirty fixes. [Report](../../2026-10-15-resource-cloud-code-review.md) and [manifest](manifest.json) are authoritative. Root owns all Cargo execution. Private source and fixtures were reviewed by GPT-6.1-Sol/xhigh workers.

## What actually ran

Existing canonical tests: **642 PASS / 0 FAIL / 17 ignored**, three commands exit0. MCP nested child1 PASS is excluded. `root-test-results.json` contains commands/exit codes; `root-existing-test-counts.json` gives per-harness counts. This is not a whole-workspace test result.

```sh
CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-sijo --bin deppy-sijo cloud_agent:: -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-sijo --bin deppy-sijo cloud_history_worker:: -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test --offline --locked -p agent-mcp -p web-remote --no-fail-fast -- --test-threads=1
```

New protection cases: isolated tests-only scratch `/private/tmp/deppy-cloud-review-worktree-20261015`; canonical product code was never changed. All five final tests compiled and failed their protective assertions on current product behavior (each Cargo exit101). Shutdown includes one passing unlocked control. PTY is a separate actual-production-rlib harness: normal/frozen controls passed, two non-frozen ignored-HUP scenarios survived, final protective assertion exit101; all fixture cleanup checks completed. Optimized answer component exited0, passed late-redaction/routing/equivalence contracts, and measured the bottleneck.

| Finding | Final authoritative evidence | Actual outcome |
| --- | --- | --- |
| R01 shutdown repeated admission | `root-app-shutdown-stale-repro.log` | 1 control PASS / 1 protection FAIL;6 completions under700ms lock; ends717ms after rollback; control2ms |
| R02 normal foreground process leak | `root-pty_nonfrozen-independent.log`, `perf-pty_nonfrozen.log` | Drop/kill same PID/birth/PGID/SID live; controls gone; cleanup PASS; exit101 |
| R03 session metadata disclosure | `root-app-metadata-repro.log` | 0 PASS / 1 FAIL; fake known-secret title remains raw |
| R04 direct WS whole deadline | `root-direct-ws-qualified-repro.log` | 0 PASS / 1 FAIL;5s deadline,6.363s accepted valid auth |
| R05 bootstrap recovery | `root-app-bootstrap-repro.log` | 0 PASS / 1 FAIL; recovered durable query1, native cache0 |
| R06 ended read-consent checkbox | `root-app-ended-consent-repro.log` | 0 PASS / 1 FAIL; actual egui click retains read=true |
| P01 answer projection | `root-cloud_answers_optimized-independent.log` | component exit0;100×16KiB/key32 mean110.782ms/37,102,106 cumulative alloc bytes |

`root-repro-results.json` is the initial batch. Its first direct-WS run used an unqualified fixture missing `type:"auth"`; only read overshoot5.871s was demonstrated. It is retained for audit, **not** valid-auth-after-deadline proof. `root-repro-followup-results.json` contains qualified WS and shutdown results. Batch driver exit0 only means it collected constituent exits; it does not mean the protective tests passed. `root-independent-probe-results.json` contains Root PTY/optimized component exits.

## Reproduction source and applicability

`root-combined-review-regressions.tests-only.patch` contains the exact final five protection cases plus shutdown control in four source files. Root `git apply --check` against the canonical reviewed dirty source succeeded; see `root-combined-patch-check.json`. Individual logic/quality/WS patches are original proposals and must not be blindly stacked with the combined patch or reapplied to the already-patched scratch. The combined patch is authoritative for final fixtures.

The reviewed baseline includes untracked modules and earlier dirty fixes: clean HEAD alone is insufficient. `root-source-start.json` pins422 source files, `root-products-start.patch` pins tracked dirty product diffs, and the prior full-fix evidence in `docs/qa/evidence/2026-10-14-full-review-fixes/` identifies those prior changes. To recreate a scratch, use the exact reviewed current source or reconstruct all those baseline files, then apply only the combined tests-only patch. Do not apply these failing review tests to user source during a read-only review.

Actually executed in the prepared scratch with `CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-performance/target` and `CARGO_BUILD_JOBS=2`:

```sh
cargo test --offline --locked -p deppy-sijo --bin deppy-sijo review_20261015_list_sessions_redacts_known_secret_display_metadata -- --nocapture --test-threads=1
cargo test --offline --locked -p deppy-sijo --bin deppy-sijo review_20261015_owner_can_revoke_read_consent_after_session_exit -- --nocapture --test-threads=1
cargo test --offline --locked -p deppy-sijo --bin deppy-sijo quality_review_20261015_bootstrap_recovers_after_transient_sqlite_lock -- --nocapture --test-threads=1
cargo test --offline --locked -p deppy-sijo --bin deppy-sijo review_20261015_shutdown_ -- --nocapture --test-threads=1
cargo test --offline --locked -p web-remote --lib review_20261015_auth_rejects_valid_message_completed_after_whole_deadline -- --nocapture --test-threads=1
```

Shared Cargo target provenance: scratch/canonical switching can reuse stale path-insensitive rlibs. All crate `src/lib.rs`/`src/main.rs` plus xtask entry were touched in the chosen worktree before building; Root serializes Cargo. Current shared target holds scratch tests-only App/web test binaries, not a user release. If canonical tests need rerunning, touch those canonical entry files before its sole Cargo invocation. Touch changes mtime only; baseline hashes remain unchanged.

Actual isolated component commands (paths are the original run locations):

```sh
rustc --edition=2024 -O /private/tmp/deppy-cloud-review-perf-20261015/pty_nonfrozen.rs -L dependency=/Users/jr/Desktop/projects/deppy-sijo-performance/target/debug/deps --extern pty=/Users/jr/Desktop/projects/deppy-sijo-performance/target/debug/deps/libpty-129cd13a2c64482a.rlib --extern libc=/Users/jr/Desktop/projects/deppy-sijo-performance/target/debug/deps/liblibc-e8b4ceaeb7c818d7.rlib -o /private/tmp/deppy-cloud-review-perf-20261015/pty_nonfrozen
/private/tmp/deppy-cloud-review-perf-20261015/pty_nonfrozen
rustc --edition=2024 -O -A dead_code /private/tmp/deppy-cloud-review-perf-20261015/cloud_answers_optimized.rs -L dependency=/Users/jr/Desktop/projects/deppy-sijo-performance/target/debug/deps --extern serde_json=/Users/jr/Desktop/projects/deppy-sijo-performance/target/debug/deps/libserde_json-470d943c96af7edd.rlib -o /private/tmp/deppy-cloud-review-perf-20261015/cloud_answers_optimized
/private/tmp/deppy-cloud-review-perf-20261015/cloud_answers_optimized
```

Archived `perf-*` source bytes retain their original absolute private fixture paths. For another private directory, adapt only the prefix/module path and copy `perf-owned-workload.py` as `owned-workload.py` and `perf-redaction-production.rs` as `redaction-production.rs`. Do not silently change the measured production method. Old target rlib filenames/hashes are pinned in `perf-identity.json`; after rebuilding, select matching current dependency cohorts instead of assuming filenames persist. PTY birth inspection uses macOS `proc_pidinfo`, so this component is not a Linux/Windows runtime proof.

`perf-cloud_answers_optimized.rs` uses the exact current method/Answer struct and copied redaction module compiled with `rustc -O`; linked debug serde_json is not exercised in timing. Its System allocator/atomic counters differ from the app allocator. This is optimized synthetic component timing, **not** full-release GUI frame latency/RSS. Per-call cumulative allocation counts are traffic, **not** retained heap/peak-memory leak. `perf-cloud_answers.log` is the separate debug-secret-rlib measurement and is not used for release performance claims.

## Preservation and review limitations

`root-preservation-check.json`: actual422 source SHA checks, zero missing/changed, HEAD identical, tracked product diff bytes identical. Product and user resources remain untouched. New permanent writes are only this review/evidence and `docs/CODEX_HANDOFF.md`.

Independent CLI: `root-codex-review-result.json` exit124/300s timeout. `root-codex-review.log.gz` decodes to the original raw log SHA in manifest. It contains no completed final review and is not counted as a clean review. Security worker stopped at a content filter; Root used source examination and a benign valid-frame private loopback test. No public-service slot-exhaustion operation was performed. The four views were attempted subject to4 concurrency slots; they were not5 simultaneous completed reviews.

Actual provider accounts dot/Meta Muse/Grok Bot/custom remote agent, public quick tunnel, real-browser/device UI, hardware sleep/wake, Windows/Linux execution were not tested.17 ignored tests remain ignored. Existing production Relay BLOCKED is unchanged. Selected private HTTP/OAuth/PTY fixtures show those implemented contracts, not provider integration compatibility or overall service correctness.

All subprocess/loopback probes finished. SQLite locks were released before assertions; PTY fixture lifetime capped15s and identity-scoped cleanup completed. No actual app stop/restart, deploy, version bump, commit, push, user key read/rotation, or cloud-account task was performed.

Manifest stores original/stored SHA and transformation per artifact, with unique owner-prefixed filenames. Source/patch/JSON bytes are preserved; textual log/report trailing whitespace is normalized; CLI log is raw gzip. Manifest deliberately excludes its own hash. Worker reports are supporting views; Root's consolidated report and actual executed logs resolve any proposal-time expectations or count differences.
