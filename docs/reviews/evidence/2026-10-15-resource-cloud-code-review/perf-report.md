| Priority | Location | Finding | Impact | Next step |
|---|---|---|---|---|
| high | crates/pty/src/lib.rs:1836 | 일반 전경 작업의 PGID가 종료 정리에서 빠짐 | pane/세션 종료 후 프로세스·메모리·CPU가 잔존 | master 닫기 전 현재 전경 그룹을 신원 고정하고 종료 |
| medium | crates/app/src/cloud_agent.rs:306 | 답변 전체를 매 UI frame 복사·재마스킹 | 지원되는 기록량에서 최적화 component 호출 104ms | history/corpus 변경에만 안전하게 캐시 갱신 |

Read-only performance/resource lifecycle re-review. Canonical `/Users/jr/Desktop/projects/deppy-sijo-performance`, HEAD `d8826f0bdb8d89255f3448b5b61b6eee83faa87a`, source/test0.12.1 plus current dirty fixes. AGENTS.md, current handoff, relevant dirty diff, caller/cleanup paths inspected. No product or canonical documentation edit, Cargo invocation, Deppy launch/restart, user session/key/provider operation, or child agent. Private rustc harnesses and synthetic owned PTYs only. R01–R12/P01 fixed findings were not re-reported.

## Details

### F-PERF-01 — non-frozen foreground workload survives close (high, confidence9.5/10)

Exact production locations: `crates/pty/src/lib.rs:1836` (Drop), `:1841` (master/worker shutdown before normal group cleanup), `:1844` (cached group SIGHUP), `:1851`–`:1861` (TERM/KILL escalation only cached group); `:2023`–`:2044` (explicit kill same limitation). The cached `process_group` is established at spawn (`:604`) and documented as spawn-time tcgetpgrp (`:1586`); actual current foreground is independently available at `:1901`–`:1909`. `terminate_frozen_workloads` (`:1611`) only covers groups previously inserted in `frozen_groups`, so the recent R09 fix protects frozen jobs but leaves this normal non-frozen path.

Reachable product chain: runtime `close_pane` `crates/runtime/src/in_process.rs:4585`–`:4586`, `kill_session_owned` `:4634`–`:4636`, and `close_tab` `:4675`–`:4676` remove Session; `remove_session` `:2071` returns owned Session which is discarded by these callers, invoking PTY Drop. Interactive shell job-control places its foreground command in a group different from shell PGID. A foreground program that ignores HUP (such as a foreground `nohup` workload) receives terminal/shell HUP but no subsequent TERM/KILL because those escalations target the now-gone shell group. The Session disappears while its work continues. This breaks the existing policy comment at `:1847` that pane/session close cleans the process tree.

Actual isolated reproduction: immutable existing `libpty-129cd13a2c64482a.rlib`, SHA `ba8abcf8d9f509b28a233f93ac666192689e8d0a61f528b67520d0b9ee2e752a`, built after the inspected pty source mtime. Real PortablePtyBackend, `/bin/bash --noprofile --norc -i`, private cwd and HISTFILE=/dev/null; no user shell profile/history. Private Python foreground fixture records PID/PGID/SID; only ignores SIGHUP (TERM remains effective), sleeps, and has a15s SIGALRM hard lifetime. macOS proc_pidinfo start time additionally pins each process; cleanup signals only the exact fixture PID when birth/PGID/SID still match.

Observed after teardown plus300ms:

- Normal non-frozen workload Drop: gone; teardown52.606ms, cleanup PASS.
- Ignore-HUP non-frozen workload Drop: SAME live PID/birth/PGID/SID survives; teardown51.938ms, identity-checked cleanup PASS.
- Ignore-HUP non-frozen workload kill+Drop: SAME live execution survives; teardown53.354ms, cleanup PASS.
- Ignore-HUP frozen workload Drop: gone; teardown234.962ms, cleanup PASS.

The protective final expectation intentionally failed (process exit101). This is a measured current defect, not a failing source test that was added to the repository. Existing source frozen tests `full_fix_lifecycle_frozen_foreground_is_reaped_by_kill`/`...by_drop` were read; not executed by this reviewer. Both normal HUP termination and fixed frozen termination are preserved in the harness controls. I/O workers themselves join and cleanup; the leak is the separate job-control workload process.

Fix direction: before releasing/closing master or reaping shell, capture the current foreground group and pin all member birth/PGID/SID against the owned shell session via the existing workload_signal helpers. Apply normal HUP→TERM→KILL teardown to both owned shell and pinned foreground groups (or a deliberate identity-checked owned session-tree policy); do not signal an unvalidated PGID after master close. Keep frozen rollback/termination, stale identity protection, self-PGID exclusion, bounded reap, and I/O cancellation intact. Add normal non-frozen HUP-ignoring foreground close/kill regression beside the existing frozen regressions. Background/escaped descendants were not exercised or reported as a separate issue.

Commands (macOS, canonical cwd):

```sh
rustc --edition=2024 -O /private/tmp/deppy-cloud-review-perf-20261015/pty_nonfrozen.rs -L dependency=/Users/jr/Desktop/projects/deppy-sijo-performance/target/debug/deps --extern pty=/Users/jr/Desktop/projects/deppy-sijo-performance/target/debug/deps/libpty-129cd13a2c64482a.rlib --extern libc=/Users/jr/Desktop/projects/deppy-sijo-performance/target/debug/deps/liblibc-e8b4ceaeb7c818d7.rlib -o /private/tmp/deppy-cloud-review-perf-20261015/pty_nonfrozen
/private/tmp/deppy-cloud-review-perf-20261015/pty_nonfrozen
```

Sources `pty_nonfrozen.rs`, `owned-workload.py`; evidence `pty_nonfrozen.log`; complete identity hashes and profile metadata in `identity.json`. All four fixture cleanup checks completed before the intentional final assertion. Root can rerun directly or compile the derived regression with its sole Cargo slot. This component run is not a full runtime/native close-button E2E test.

### F-PERF-02 — unchanged answer corpus is fully rescanned on every frame (medium, confidence9/10)

Exact location `crates/app/src/cloud_agent.rs:306`–`:323`: `cloud_answers()` clones every answer, re-redacts its entire message and connection, regenerates/redacts run_label, allocates a new Vec and Arc. The caller `crates/app/src/app.rs:35371` runs `pump_cloud_agent` every App update. Both the normal projection `:28445`–`:28448` and the early stopped/hidden path `:28215`–`:28222` call this method, so no open cloud settings, active tunnel, or new answer is required. The answers Arc is then cheaply cloned into each WorkspaceUi, but the expensive prior projection was rebuilt once each frame.

Late secret registration is correctly protected and must remain so. `late_secret_notify_answer_projection_preserves_identity_and_rescrubs_display_and_run_label` (`cloud_agent.rs:2110`) explicitly expects raw answer source preserved and current corpus display scrub; source test read only, not executed here. The problem is unconditionally recomputing that projection when neither source history nor secret corpus has changed.

Supported scenario: a user retains100 completed `notify` responses each16KiB (MAX_ANSWER=`agent-mcp/src/lib.rs:20`; pruning/latest100 in `agent-mcp/src/history.rs:11`, `:153`, `:274`; snapshot cap3MiB in HistoryWorker). Our100×16,384B message fixture totals1,638,400B and fits that bounded snapshot budget including metadata.32 registered synthetic tokens create64 corpus patterns/2,656B, far below production4096items/4MiB cap (`secret/redaction.rs:46`). Fixture content is ordinary repeated paragraphs with newlines and contains none of the registered tokens. No real secret was read. Even unchanged unrelated safe text causes repeated corpus scans and temporary buffers (`secret/redaction.rs:618`–`:678`).

Actual measurement: exact current `cloud_answers` method extracted unedited; actual Answer struct extracted; original redaction.rs copied byte-for-byte to private path; actual SecretString source copied byte-for-byte to harness root (crate path adaptation only). `rustc -O` optimizes both App method and redaction production module. It links an existing debug serde_json dependency, but that JSON API is not exercised in measured calls. System allocator plus atomic allocation counters, not the app's allocator.10samples each; timing includes projection allocation/publication and excludes fixture setup/output-drop. Source hashes are recorded, and copied redaction SHA exactly equals inspected production source. This is optimized synthetic component timing; no full app release build, GUI frame timing, or real-user workload is claimed.

| Answers × message bytes | Synthetic raw tokens / corpus items | Mean / median ms per call | Allocation calls | Cumulative allocated bytes per call |
|---|---|---|---|---|
| 0 ×0 | 32 /64 | 0.000 /0.000 | 1 | 16 |
| 1 ×1,024 | 1 /2 | 0.009 /0.009 | 27 | 22,668 |
| 10 ×1,024 | 32 /64 | 0.857 /0.819 | 1,492 | 638,216 |
| 100 ×16,384 | 1 /2 | 8.663 /8.337 | 2,502 | 32,985,306 |
| 100 ×16,384 | 32 /64 | 104.487 /104.288 | 14,902 | 37,102,106 |
| 100 ×16,384 | 128 /256 | 408.007 /405.551 | 53,302 | 49,850,906 |

Cumulative allocation bytes are allocation/reallocation traffic, not retained heap, peak RSS, or a memory leak. Allocator counter overhead is included. Existing debug secret rlib independently measured the same current method, producing143.190ms (1token),1520.525ms (32tokens),5590.233ms (128tokens) at maximum answer body count/size; these debug-component values must not be presented as release performance. See separate logs for every sample/min/max. Optimized104ms alone exceeds a16.7ms frame budget by over6× at a supported workload, while repeat calls have byte-equivalent output. Static caller trace establishes this work is on the UI update path; further end-to-end frame measurement would refine actual user-visible latency but is not needed to identify the repeated expensive algorithm.

Contract checks actually executed and PASS in both harnesses: safe message equivalence for every fixture; current output preserves routing identities and creation timestamp; a wrapped synthetic secret remains intact before registration, is redacted on the very next call after registration, and connection/run_label update while raw run identity remains unchanged. This preserves the reason the rescanning was introduced.

Fix direction: keep raw source answer Arc and a redacted projection keyed by history revision plus a monotonic redaction corpus change generation, with explicit invalidation on registration/replacement/leases. Stable source+corpus should return Arc::clone of cached safe projection. Any corpus change that adds a secret must invalidate before display/publication; do not cache only by history revision and do not remove late-registration safeguards. A worker may build a large projection, but the UI must not publish an old cache after corpus mutation. Keep128-byte routing identities and raw run ID untouched. Add stable generation identity/allocation regression and late-registration/replacement regression; performance measure must use actual production redaction source or a pinned current library.

Commands executed:

```sh
rustc --edition=2024 -O /private/tmp/deppy-cloud-review-perf-20261015/cloud_answers.rs -L dependency=/Users/jr/Desktop/projects/deppy-sijo-performance/target/debug/deps --extern secret=/Users/jr/Desktop/projects/deppy-sijo-performance/target/debug/deps/libsecret-dbcd127907eff675.rlib -o /private/tmp/deppy-cloud-review-perf-20261015/cloud_answers
/private/tmp/deppy-cloud-review-perf-20261015/cloud_answers
rustc --edition=2024 -O -A dead_code /private/tmp/deppy-cloud-review-perf-20261015/cloud_answers_optimized.rs -L dependency=/Users/jr/Desktop/projects/deppy-sijo-performance/target/debug/deps --extern serde_json=/Users/jr/Desktop/projects/deppy-sijo-performance/target/debug/deps/libserde_json-470d943c96af7edd.rlib -o /private/tmp/deppy-cloud-review-perf-20261015/cloud_answers_optimized
/private/tmp/deppy-cloud-review-perf-20261015/cloud_answers_optimized
```

Actual logs `cloud_answers.log`, `cloud_answers_optimized.log`; source `cloud_answers.rs`, `cloud_answers_optimized.rs`, `redaction-production.rs`; component identities `identity.json`. Both measurement commands exited0; source test suites not run by this reviewer.

## Other reviewed paths / exclusions

- R01 Codex command/input/events/pending state queues, cancellation-aware nonblocking child stdin write and direct child kill→reader joins reviewed. No repeated report of repaired stdin hang. A descendant retaining pipe handles is theoretical with the actual bundled command; insufficient concrete supported scenario for another finding.
- Cloud HistoryWorker enforces32 admitted jobs/results and4MiB charged budget until Completion drops; claims/finish stages are nonblocking in UI, terminal effects are App-owned and shutdown revokes before DB drain. Normal DB shutdown can wait on bounded SQL workload; no new leak established.
- Ended-session loader explicitly retains one in-flight receiver across settings navigation; no unbounded worker spawn on quick tab switching. History/cache/output cursors/screens all have count or byte caps.
- Tunnel single-slot coalesced updates and signal channel are bounded;2s HTTPS metadata deadline and16KiB body bound reviewed. A stderr reader can theoretically outlive direct cloudflared child if some descendant inherits it, but bundled cloudflared does not currently supply a concrete descendant-spawning path; excluded.
- agent-mcp HTTP handlers bounded to4 concurrent sessions, read deadlines and request queue16. Detached handlers may exist until deadline after server stop but no permanent leak verified.
- PTY reader/writer self-pipe cancellation and output queue cancellation join workers after owner stop; guarded input runtime path defers destructors until admission/credential locks release. New issue F-PERF-01 is workload ownership, not I/O worker join.
- Runtime final_drain is explicitly byte/pump bounded; viewport/resource pressure latest slots bound slow subscribers; archive/cache budgets preserve normal paths. P01 per-snapshot children adjacency already fixed and not re-reported.
- Repeated set_targets/screen/run projections and SQL history query scaling remain optimization candidates without a new measured independent defect. Do not inflate findings with those candidates.
