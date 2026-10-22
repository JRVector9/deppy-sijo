# PR2 local verification worker

Objective: execute an explicitly reviewed local command for one immutable project/task/attempt/result, retain bounded durable evidence, and leave final confirmation separate. Branch `feat/cc-pr2-verification-20261022`, base `1d5db6a7`.

Ownership: core verification DTO additions; additive opt-in sidecar schema and evidence methods; `work_management/verification.rs`, `verification_worker.rs`, and module registration. Root owns host/app wiring, shared plan and handoff; UI sibling owns DTO presentation/native/PWA UI. No app launch, stop, restart, release/version bump, paid calls or user terminal input.

Plan (approved by root):

- [x] RED/GREEN bounded DTO validation.
- [x] RED/GREEN durable immutable evidence journal, v3 migration and restart read without execution.
- [x] RED/GREEN exact local result approval and stale/cancel rejection.
- [x] RED/GREEN real shell success/nonzero/output cap/timeout/cancellation/descendant cleanup and streamed secret redaction.
- [x] Worker integration, narrow regressions and exact host API handoff.

Design: verification target stores provider/source identity, not an unproven Git commit. Command/cwd are literal user reviewed fields; cwd must resolve to the current local attempt folder. No remote runner. A lazy bounded worker persists Running before spawning; terminal evidence is persisted before host delivery. Running evidence after restart is Interrupted, never a retry permit. App command and user manual evidence have separate tagged origins. Each stream is redacted before applying byte limits.

Actual tests (all use isolated target `/private/tmp/deppy-cc-verification-target-20261022`, jobs2):

- `cargo test --offline --locked -p deppy-core verification_spec -- --nocapture`: expected RED, blank command accepted by initial validation stub.
- `cargo test --offline --locked -p deppy-core verification_ -- --nocapture`: expected evidence-validation RED, then **2 passed** after implementation.
- `cargo test --offline --locked -p storage verification_journal -- --nocapture`: expected RED, duplicate claim returned New. After implementation, `cargo test --offline --locked -p storage work_management -- --test-threads=1`: **32 passed**.
- `cargo test --offline --locked -p deppy-sijo verification_ -- --test-threads=1 --nocapture`: approval RED against the explicit unimplemented stub, then approval GREEN with five runner tests RED against Interrupted stub. Actual shell implementation yielded **9 passed** (new runner/approval plus existing verification regressions).
- `cargo test --offline --locked -p deppy-sijo --bin deppy-sijo verification_async_ -- --test-threads=1 --nocapture`: **5 expected RED** against asynchronous stub, then **5 passed** with durable runner.
- Final `cargo test --offline --locked -p deppy-sijo --bin deppy-sijo work_management:: -- --test-threads=1`: **96 passed / 0 failed**, final rerun3.43s execution after the boxed claim refinement. New tests cover separate final confirmation, Running persisted before child observation, real cancel/Drop and child PID disappearance, corrupt journal prevents spawn, redacted manual evidence and no manual command execution, actual output caps/nonzero/timeout/success, restore Interrupted and no restart replay, and cancel after process completion before delivery.
- Final `cargo test --offline --locked -p deppy-core -p storage --lib work_management -- --test-threads=1`: **3 core + 32 storage passed**, including direct manual validate extraction used by root host. Error payload bounds and impossible zero-code NonzeroExit are checked.
- First final regression attempt had test-only `state.apply` instead of existing `apply_event`, causing E0599; corrected and the 96-test run above passed. A no-op patch also initially missed a rustfmt-expanded test line; no product change resulted.
- First `cargo fmt --all --check` printed a module ordering diff; corrected with cargo fmt. The original shell chain's final diff-check returned0, so that0 is not counted as a format pass. Final separate `cargo fmt --all --check` **exit0**, separate `git diff --check` **exit0**.
- `cargo clippy --offline --locked -p deppy-core -p storage --lib -- -D warnings`: initially rejected the 328-byte VerificationClaim enum; changed Existing payload to Box, rerun **exit0**. App host glue uses the worker types; strict app Clippy is an integration check owned by root. Narrow pre-integration app tests warn only about result fields consumed by root's forthcoming host glue.

Foundation commits: core `e6903b43`, journal `6f051e95`, local exact-result approval `1a3ccfa7`. Root and UI sibling received these and the final signatures. No runner stubs remain.

Cleanup boundary: dedicated owned process group, kill before leader reap, including ordinary background descendants after shell success/timeout/cancel. This is not an adversarial process sandbox and does not promise to contain daemons that deliberately detach into a new session. Root approved this explicit scope.

Recovery boundary: Interrupted means prior execution was not observed to finish, not proof a process was terminated after abrupt app death. Normal worker Drop/cancel kills and reaps; restart history never spawns or reapplies commands. Only explicit native user Execute with literal spec can create a fresh permit. Model plan responses and remote execution do not enter this runner.

Bounds: command16KiB, literal cwd8192bytes resolving to exact local Attempt.folder, timeout1..600000ms, per-stream output1..32768bytes after registered-secret streaming redaction, one outstanding job, 256 durable records/workspace, encoded record max1MiB. Running is persisted before spawn; terminal evidence is persisted before Finished. Journal errors prevent start or state application. Exact spec metadata containing registered secrets is rejected rather than storing an altered literal command. Manual records have distinct tagged origin and are redacted without executing anything.

Production file changes: `crates/core/src/work_management.rs`; `crates/storage/src/work_management.rs` and new `work_management_verification.rs`; `crates/app/src/work_management/mod.rs`, new `verification.rs`, `verification_worker.rs`, `verification_worker_tests.rs`. No existing metadata DB migration, snapshot DTO changes, release/version changes or app lifecycle actions.

Root host API:

```rust
verification::approve_local(&ProjectWorkState, &TaskId, ResultRevision, VerificationSpec)
    -> Result<ApprovedVerification, ValidationError>
ApprovedVerification::{id, target, spec, evidence} // borrowed immutable getters
verification::matches_current(&ProjectWorkState, &CommandVerificationEvidence) -> bool
verification::result_state(&CommandVerificationEvidence) -> Option<VerificationState>
VerificationWorker::new(PathBuf, secret::RedactionService, impl Fn() + Send + Sync + 'static)
VerificationWorker::try_request(VerificationJob) -> Result<(), LazyWorkerSubmitError<VerificationJob>>
VerificationWorker::cancel(&VerificationId) -> bool
VerificationWorker::try_recv() -> Option<Result<VerificationResult, VerificationError>>
```

Jobs: Run(ApprovedVerification), Load{workspace_id,token}, RecordManual{evidence}. Results: Finished{evidence,cancelled}, Loaded{workspace_id,token,result}, ManualRecorded{evidence,result}, ExecutionFailed{id,target,error}. Finished is durable; caller must reject cancelled or stale/root-revoked results and apply a VerificationFinished event separately. Loaded records are display-only. Passed transitions only to AwaitingUserConfirmation. The worker resets started_at_ms at actual admission, so the immediate approved DTO has a preparation timestamp; UI sibling changed Running to “실행 접수 · 결과 대기”. Source revision is a provider/source identifier, never an unproven Git commit.

Read-only sibling review sent to root: preserve Completed evidence display while disabling new execution; use origin-neutral Passed label; compare captured Attempt.folder and Finished spec in addition to config/runtime/target; exact ExecutionFailed target check. UI review found no additional DTO drift. Root owns implementing/retesting these host changes and shared `docs/CODEX_HANDOFF.md` updates.

Remaining: root cherry-picks final worker commit, validates app/native/PWA integration and reports only executed checks. No release is authorized or claimed. Exact local recheck commands:

```sh
CARGO_TARGET_DIR=/private/tmp/deppy-cc-verification-target-20261022 CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-sijo --bin deppy-sijo work_management:: -- --test-threads=1
CARGO_TARGET_DIR=/private/tmp/deppy-cc-verification-target-20261022 CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-core -p storage --lib work_management -- --test-threads=1
cargo fmt --all --check
git diff --check
```
