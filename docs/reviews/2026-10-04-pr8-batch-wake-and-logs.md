# PR8 — Batch completion wakes and bounded session log writes

## Scope and ownership

- Worktree: `/private/tmp/deppy-audit-pr8-20261004`.
- Branch: `fix/audit-pr8-batch-wake-logs-20261004`.
- Baseline: `30a57a535ee409c1344d893990b4a0fd32bd89df`, integrated PR1/3/6/7, inherited version 0.5.5.
- Owned code: `crates/app/src/app.rs`, `crates/runtime/src/in_process.rs`, `crates/storage/src/logs.rs`.
- Root performs coherent integration, final independent review, whole-App gates, version increase and release build. This scoped report records child handoff; no shared `docs/CODEX_HANDOFF.md` edit.
- No native Deppy stop/launch/restart, user files/logs/PTYs, tunnel, secret, network, version/lock change or push.

## Design

### Batch scheduling

`PendingBatchSpawn` shares its bounded 16 KiB prompt through `Arc<str>` with accepted settings jobs. The settings worker materializes actual argv once per accepted launch. The production `pump_batch_spawn_step` checks staged workspace and busy state before copying job identifiers or constructing a request. Busy frames request only a delayed wake, never `request_repaint()`.

`App::logic` polls settings outcomes first, runs the batch step, then admits the next settings job in the same tick. The existing worker completion callback wakes that tick; accepted jobs still decrease the remaining count exactly once and launch in order. Workspace switches cancel the unqueued remainder. A known-unsent/busy operation has a 250 ms fallback. Each intervening pass rearms the remaining delay; otherwise egui drops its deadline after an earlier input frame. Default egui frame prediction can turn a near-deadline delayed wake into zero delay; the fixture observes 8 such instants per 120 frames over 2 seconds, versus the baseline's 120 immediate wakes. It does not claim zero total repaints.

### Log I/O

`BoundedLogFile` tracks the last confirmed pinned-handle length. Normal appends no longer call metadata before every chunk. A successful O_APPEND write reconciles the actual same-handle EOF with `stream_position()` (still a syscall); external append/truncate is accounted for and successful appends compact if needed. Any failed write/compaction invalidates the length, so a later append must recover actual metadata; an error never becomes an assumed zero length or successful append.

Incoming chunks at least as large as the cap select a bounded borrowed tail *before* writing. ANSI state is scanned across the previous pinned stream and incoming bytes, preserving split CSI/OSC/UTF8 boundaries. A partial giant write therefore cannot write more than the cap, and a metadata failure aborts instead of appending without bounds. Pinned descriptors, no-follow/type checks, file permissions, concurrent-compaction change checks and startup tail compaction are preserved. Other processes can mutate files outside this writer; this does not promise control over an independent writer between appends.

A production `RedactedLogBatch` groups only already-redacted bytes into at most 32 KiB, inside one normal/final PTY pump. It allocates lazily, releases its buffer at the pump's end, and does not keep one buffer per idle session or create another thread. The final partial batch is flushed before lifecycle/output/status decisions and persisted offsets. Failed batches are cleared, because a partial prefix may already be on disk and must not be replayed. Shutdown's redactor carry/event/flush path is unchanged. Bounds here refer to logical buffer bytes, not allocator metadata or whole-App RSS.

## Observed RED evidence

All Cargo execution uses the exclusive `cargo_gate.py` lock through compilation and execution, with the shared target supplied by the gate.

- `/tmp/deppy-pr8-red-app-20261004.log`: 120 busy production steps copied/materialized 1,966,080 prompt bytes and requested 120 immediate wakes; intended zero-copy assertion failed.
- `/tmp/deppy-pr8-red-storage-20261004.log`: 2,048 actual 128-byte file appends read metadata 2,048 times and wrote 2,048 times; 5.221625 ms single sample; intended metadata-count assertion failed.
- `/tmp/deppy-pr8-red-storage-expanded-20261004.log`: partial giant write left 75 bytes in a 64-byte cap; simulated metadata failure incorrectly reported append success; repeated metadata count again failed (single sample 6.190959 ms).
- `/tmp/deppy-pr8-red-runtime-20261004.log`: the production pump log adapter made 512 appends for 512 128-byte chunks instead of at most 2; exact output order matched.
- `/tmp/deppy-pr8-red-rearm-20261004.log`: delayed retry lost its wake after an intervening 100 ms frame (`Duration::MAX`); this was found during review of the initial fix and then corrected.

## Harness corrections / unsuccessful attempts

- First App fixture forgot to consume egui texture deltas and failed the harness, not the product. `textures_delta.clear()` fixed it.
- `/tmp/deppy-pr8-green1-20261004.log` initially counted egui's two automatic setup frames. Counting callback delta around the actual production step separated those from batch requests.
- `/tmp/deppy-pr8-measure-green3-20261004.log` reran fallback correctly, observing 8 predicted zero-delay timer instants; a zero-instant assertion was too strict. The final assertion bounds those instants to the eight 250 ms deadlines, preserving the real measurement.
- First temporary Git-baseline counter missed multiline `.write_all` calls. Metadata counts and timing were real, but write count was 0 and not used as final proof; the counter was corrected and the five-sample measurement rerun.

## Verification and measurements

Final source gate log: `/tmp/deppy-pr8-final-gates-20261004.log`, exit **0**, after removing the temporary baseline module:

- App named PR8: **4 passed**; existing settings worker: **2 passed**. Actual worker completion wake/ordering/prompt pointer sharing, staged-workspace cancellation, known-unsent retry delay, busy frame counts and intervening-frame rearming are covered.
- Full Storage: **404 passed**, 0 failed/ignored; doc-tests 0. This includes all existing symlink/pinning/type/permissions/startup/ANSI/tail/GC tests and the 7 PR8 fixtures.
- Full Runtime: **324 passed**, 0 failed/ignored; doc-tests 0. This includes existing restoration, redaction, shutdown, PTY, admission and remote tests plus the 3 PR8 fixtures.
- Named test inventory: App4, Storage7, Runtime3 verified by `--list` against this exact source path.
- Storage + Runtime strict Clippy `--all-targets -- -D warnings`: **passed**.
- App Clippy `--bin deppy-sijo --all-targets -- -D warnings -A dead_code`: **passed**. The allowance is for existing PR3 `PromptLibrary::{load,try_upsert,save}` methods pending PR4 integration; it is not a claim of unqualified whole-App strict Clippy. Root owns the final coherent strict gate.
- `cargo fmt --all -- --check`: **passed**. `git diff --check`: **passed**.
- No whole-App suite, native App UI/FPS/RSS measurement or cross-platform build was run by this child. Root performs the integrated gates.

Preliminary `green2` passed App3/Storage7/Runtime2 before later retry-rearming/measurement additions and is not used as final source proof.

Five-sample measurement log: `/tmp/deppy-pr8-measure-green4-20261004.log`; the gate passed Git-baseline Storage1, current Storage1, runtime real-writer1 and named App4. Corrected counters and actual medians:

| Fixture | Before | After | Observed change |
| --- | --- | --- | --- |
| 120 busy batch frames / 2s, 16KiB prompt | 1,966,080 materialized bytes; 120 immediate wakes | 0 materialized bytes; 8 predicted deadline instants | No per-frame prompt/request construction; 250ms fallback |
| Real file, 2048 × 128B, Git baseline vs current | 2048 metadata, 2048 write_all; 5.160875ms median | 0 metadata, 2048 write_all; 3.435125ms median | Same file output; position reconciliation remains |
| Real ANSI+plain writer, 512 × 128B; isolate batch using current storage | 512 append_output; 3.789917ms median | 2 append_output; 0.615875ms median | Same exact ANSI and plain bytes; 32KiB max batch |

All five wall-time samples in milliseconds:

- Git-baseline append: 5.667833, 5.582166, 5.153000, 5.057375, 5.160875.
- Current append: 3.394875, 3.420417, 4.372375, 3.435125, 3.465625.
- Actual writer direct pattern: 4.010959, 3.480666, 3.657459, 14.100625, 3.789917.
- Actual writer production batch: 0.780083, 0.597792, 0.922041, 0.615875, 0.597916.

A previous run produced different medians (5.755/4.279ms append and 4.360/0.783ms writer), demonstrating local timing variability. Only the corrected final run above is the recorded comparison. Counts/order/caps are deterministic assertions; timings are measurements without a timing assertion.

The baseline measurement compiles the actual `logs.rs` Git source from the baseline commit as a temporary test-only namespaced module with counter wrappers and the identical five-file fixture. The snapshot and temporary module include are removed before final gates and commit. Counts are Rust metadata/write_all entrypoint calls, not OS syscall tracing. Runtime five-sample direct-versus-batch measurements use actual `SessionLogWriter` files with current storage on both sides; they isolate batching's contribution. These are local debug-build, cache-warm fixture timings, not GUI FPS, App CPU/RSS, fsync durability, or production workload guarantees.

## Exact executed final command

Working directory was `/private/tmp/deppy-audit-pr8-20261004`. Gate holds the lock through every Cargo invocation; target path is set inside the gate.

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["test","--offline","--locked","-p","deppy-sijo","--bin","deppy-sijo","pr8_","--","--list"],["test","--offline","--locked","-p","deppy-sijo","--bin","deppy-sijo","pr8_","--","--nocapture","--test-threads=1"],["test","--offline","--locked","-p","deppy-sijo","--bin","deppy-sijo","settings_snapshot_worker","--","--test-threads=1"],["test","--offline","--locked","-p","storage","pr8_","--","--list"],["test","--offline","--locked","-p","storage","--","--test-threads=1"],["test","--offline","--locked","-p","runtime","pr8_","--","--list"],["test","--offline","--locked","-p","runtime","--","--test-threads=1"],["clippy","--offline","--locked","-p","storage","-p","runtime","--all-targets","--","-D","warnings"],["clippy","--offline","--locked","-p","deppy-sijo","--bin","deppy-sijo","--all-targets","--","-D","warnings","-A","dead_code"],["fmt","--all","--","--check"]]' > /tmp/deppy-pr8-final-gates-20261004.log 2>&1
```

The corrected comparison command, while the temporary Git-source module existed, was:

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["test","--offline","--locked","-p","storage","pr8_baseline_fixture::tests::pr8_measure_append_fixture_five_samples","--","--nocapture","--test-threads=1"],["test","--offline","--locked","-p","storage","logs::tests::pr8_measure_append_fixture_five_samples","--","--nocapture","--test-threads=1"],["test","--offline","--locked","-p","runtime","pr8_measure_real_log_writer_batch_five_samples","--","--nocapture","--test-threads=1"],["test","--offline","--locked","-p","deppy-sijo","--bin","deppy-sijo","pr8_","--","--nocapture","--test-threads=1"]]' > /tmp/deppy-pr8-measure-green4-20261004.log 2>&1
```

The temporary module used `git show 30a57a535ee409c1344d893990b4a0fd32bd89df:crates/storage/src/logs.rs` with only metadata/write_all counter wrappers and the identical current `pr8_measure_append_fixture_five_samples` test. It was deleted before final gates/commit. The shipped current measurement fixtures remain runnable with `pr8_`.

## Review and root integration handoff

Self-review checked admission ordering after settings outcome polling, staged workspace cancellation, delayed-wake rearming, one prompt materialization per accepted worker job, flush before completion/output decisions, redaction before batching, same-handle reconciliation, partial-write invalidation, giant tail boundary equivalence and preserved inode/no-follow behavior. No confirmed remaining PR8 defect was found after the gates above.

Scoped implementation is complete. Root must integrate the scoped commit, perform independent source review and combined whole-App/strict/version/build gates, maintaining no-restart authorization. Exact next commands after selecting this worktree are `git status --short`, `git log -1 --oneline`, `git diff 30a57a535ee409c1344d893990b4a0fd32bd89df..HEAD --stat` and the gated focused final command above if integration changes require it. No final release artifact was produced by this PR.
