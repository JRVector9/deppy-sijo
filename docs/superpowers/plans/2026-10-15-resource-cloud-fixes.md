# Resource and Cloud Defect Fixes Implementation Plan

> **For agentic workers:** Execute the explicitly requested parallel work with the available collaboration tools. The `superpowers:subagent-driven-development` and `superpowers:executing-plans` skills named by the generic template are not installed; use the repository model/ownership rules and Root integration gates below.

**Goal:** Fix all six defects and the measured answer-projection bottleneck from the 2026-10-15 review without regressing session identity, consent, durable outcomes, or late-secret redaction.

**Architecture:** Three isolated GPT-6.1-Sol/xhigh worktrees own management, PTY, and transport/permissions. Root owns metadata redaction, corpus revision/projection caching, canonical integration, Cargo, and shared documentation. Changes are relative to the current dirty source snapshot, never clean HEAD; no worker edits canonical files or runs Cargo.

**Tech Stack:** Rust workspace, egui, rusqlite, portable-pty, tungstenite, shared redaction service; macOS synthetic PTY/SQLite/loopback fixtures.

---

## Source and verification contract

- Baseline: `/private/tmp/deppy-cloud-fix-baseline-20261015/source-baseline.json` and `files/`, canonical `/Users/jr/Desktop/projects/deppy-sijo-performance`, HEAD d8826f0b/source0.12.1 plus all prior dirty fixes.
- Existing RED evidence and final fixtures: `docs/reviews/evidence/2026-10-15-resource-cloud-code-review/root-combined-review-regressions.tests-only.patch`, per-case logs and `perf-pty_nonfrozen.rs`.
- Actual previous REDs qualify the five cloud/UI/deadline protections and PTY ownership defect. New cache/revision regression must be observed failing before product implementation. Compilation/cache errors do not qualify.
- Root alone runs Cargo with2 jobs. Switching shared target worktrees requires touching all crate entry files and xtask in the chosen source before compiling; zero selected tests never count as validation.
- Keep file contents from the baseline outside this task's owned files. Do not run the application, touch real sessions/DB/keys, deploy, commit, or push. Source/test builds only; a future user release needs a higher version and artifact-version checks.

### Task 1 — Management shutdown and bootstrap recovery (worker management)

**Files:** `crates/app/src/cloud_agent/management.rs`; `crates/app/src/cloud_agent.rs` shutdown function and only required management lifecycle initialization.

- [x] Import/reuse the reviewed SQLite stale-shutdown and bootstrap protection cases. Keep EXCLUSIVE-lock qualification, unlocked control, rollback/join before assertions, and same-worker recovered query.
- [x] Seal new management admission before shutdown drain; preserve accepted durable writes and required finite follow-up stages. Add no new bootstrap/stale jobs while stopping.
- [x] Separate bootstrap success from in-flight/error state; on transient failure schedule a bounded-delay retry only while active. Retry must not grow pending work or make shutdown loop infinite.
- [x] Root runs `cargo test --offline --locked -p deppy-sijo --bin deppy-sijo cloud_agent::management -- --test-threads=1` and existing shutdown/claim/finish cases; recovered durable run becomes visible and the locked close does not create repeated jobs.
- [x] Deliver scoped baseline-relative patch and invariant notes. Cross-review transport worker after implementation.

### Task 2 — Owned normal foreground cleanup (worker pty)

**Files:** `crates/pty/src/lib.rs`, `crates/pty/src/workload_signal.rs` if needed; PTY tests beside existing frozen workload tests.

- [x] Adapt the actual private bash/Python HUP-ignoring foreground reproduction into an automated owned-process regression, retaining normal/frozen controls and hard fixture lifetime/identity cleanup.
- [x] Pin current foreground workload ownership before master close, using birth/PGID/SID and shell session identity; bounded HUP/TERM/KILL for owned normal and frozen groups.
- [x] Preserve PID/PGID reuse safety, self/foreign group exclusion, worker cancellation/join, bounded reap, frozen rollback, and platform cfg behavior.
- [x] Root runs `cargo test --offline --locked -p pty -- --test-threads=1`; independently rerun the real production PTY fixture against the rebuilt current rlib.
- [x] Deliver baseline-relative patch and platform/identity notes. Cross-review management shutdown after implementation.

### Task 3 — Direct WS deadline and ended read-consent UI (worker transport)

**Files:** `crates/web-remote/src/ws_api.rs` and a narrow transport helper if needed; `crates/app/src/cloud_agent/ui/permissions.rs`, `ui/management_tests.rs`.

- [x] Reuse qualified valid-auth slow frame and actual egui ended-read checkbox REDs. Keep routing/token checks and pointer event behavior in the tests.
- [x] Bound frame reads by the remaining whole authentication deadline, observe cancellation slices, reject valid frames completed after deadline, and preserve partial frame retries/normal fragmented frames.
- [x] Allow existing retained read consent to be revoked on an ended target while keeping input disabled. Keep actor and other session consents unchanged.
- [x] Root runs `cargo test --offline --locked -p web-remote -- --test-threads=1` and App UI/ended-session consent cases. All fixtures close their sockets and join threads.
- [x] Deliver baseline-relative patch and compatibility notes. Cross-review Root metadata/cache invalidation after integration.

### Task 4 — Outward metadata and safe projection reuse (Root)

**Files:** `crates/app/src/cloud_agent.rs`, `crates/app/src/cloud_agent/bridge_tests.rs`; `crates/secret/src/redaction.rs` and exports only if needed.

- [x] Import metadata protection: authenticated read-only actor, known fake credential, unchanged UUID/generation, redact both title and workspace_name. Source fixture previously failed at raw title assertion.
- [x] Add cache identity regression: two calls with unchanged history/corpus return the same safe Arc; changing history or registering/rotating/expiring a secret invalidates projection before display. Observe the old implementation fail.
- [x] Expose a coherent monotonic corpus revision covering additions, fail-closed state, grace pruning and lease mutations; retain source raw history so late credentials can be masked.
- [x] Cache safe answers by source identity/revision plus corpus revision; use no old projection after a new corpus change. Preserve routing fields and run label cap. Avoid unbounded retries under concurrent registration.
- [x] Root runs `cargo test --offline --locked -p secret -- --test-threads=1` and App cloud cases; repeat the supported100×16KiB/key32 component measurement with exact modified production code and unchanged synthetic fixtures.

### Task 5 — Integration, review and final evidence (Root)

- [x] Integrate only baseline-relative owned changes; resolve shared cloud_agent.rs hunks explicitly. Preserve every unrelated dirty file and untracked module.
- [x] Review worker patches and peer review results against all seven report items; remedy any new regression.
- [x] Run meaningful affected tests first, then workspace tests, strict clippy, rustfmt, dependency/boundary checks. Record actual commands/exits/counts and avoid combining nested subprocess results.
- [x] Update `docs/CODEX_HANDOFF.md` after meaningful changes/tests; add QA/evidence with start/end SHA, exact task diff and failed approaches. Link the original review to the final fix results without rewriting its RED evidence.
- [x] Report completed fixes, measurement scope, actual tests, and remaining platform/provider limits. No app restart or release artifact is part of this request.

## Plan self-review

All R01–R06 and P01 have an owner and an actual behavioral validation path. R01/R05 share the lifecycle gate so recovery cannot recreate shutdown work. PTY ownership is captured before descriptor closure. Metadata and cache preserve raw routing identity and late-secret protections. Shared source files are integrated by Root only; worker processes run no concurrent Cargo. Current request already selected parallel implementation, so no execution-choice confirmation is needed.

## Integration progress — 2026-10-15

All owned implementations and read-only peer reviews are integrated. Root actually observed the new cache and PTY REDs. secret80+pty64, Appcloud175 and Web412 tests passed in affected runs; independent freshly relinked production PTY reproduction also passed. Two initial App attempts are retained: missing test trait import (compile error), then an under-qualified accepted-stale fixture that admitted two initial jobs. Empty bootstrap completion and initial callback barrier repaired that fixture without changing product code or the exact-one assertion. Final workspace exit0: 5732PASS/0FAIL/65ignored, nested subprocess summaries excluded. Strict clippy/fmt/boundary/deps all exit0. QA/source-delta/SHA/evidence are complete; prior phase counts are not reused.

### Integration follow-up — natural shell exit

The first full workspace run found a real PTY regression: natural shell reap removed the owner identity before normal Drop could preserve the previously supported same-group descendant cleanup. Runtime authorization-handshake fixture observed no TERM hook. Root qualified and cleaned the owned failed fixture by exact identity after the failed test process exited.

- [x] Retain same-group member identity before normal shell reap; close inspect-alive→try_wait race without scanning the process group every idle poll. Cover reaper, try_exit_code, freeze/resume and teardown entry points; preserve authorization deferral and raw-group prohibition.
- [x] Add hard-lifetime post-shell-exit Drop/kill cases; rerun existing runtime authorization handshake plus full PTY. Peer review lock order, identity ownership and scalar-only live polling.
- [x] Rerun final workspace/lint gates after this product correction and retain the first failed workspace log as an attempt, not a PASS.
