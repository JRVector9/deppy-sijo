# PR1 — Tracked atomic prompt delivery

## Scope and source

- Worktree: `/private/tmp/deppy-audit-pr1-20261004`.
- Branch: `fix/audit-pr1-prompt-delivery-20261004`.
- Baseline: `85631a845d733b340e281df976522be4d4468959`, approved current 0.5.5 source snapshot.
- Local Composer, Fleet broadcast, queued follow-up delivery only; common runtime batch primitive is ready for PR9.
- App version/Cargo.lock unchanged. Root owns integration, release version, final review and build. No app launch, stop, restart, real AI session, user terminal action or push.

## Completed checklist

- [x] Composer keeps exact original draft when Send is staged; accepted history is not updated prematurely.
- [x] Actual single host slot rejects without replacing another action, losing text or disabling a later manual attempt.
- [x] Exact workspace/runtime/pane/session validation precedes dispatch. Stale runtime/session yields a correlated rejection.
- [x] One `WriteInputBatchTracked` command and one PTY byte/message reservation cover paste body and submit. Body and CR remain separate writer messages in FIFO order.
- [x] Provider-specific `plan_composer_input` encoding and existing sanitization stay shared. `ComposerInputPlan::into_parts()` is the common encoding output.
- [x] InputAdmitted correlates actual PTY queue admission by operation ID. Queue acceptance is explicitly not AI execution or completion.
- [x] Composer pending and unknown states block duplicate sending; unknown requires explicit confirmation before manual resend.
- [x] Composer submission generation prevents an old ACK from consuming a same-text new submission. Accepted ACK clears only an unchanged original draft; concurrent edits survive.
- [x] Ten-second pending deadline is rearmed each logic frame, including after an intervening repaint.
- [x] Unknown target/payload is retained within 256-entry / 8 MiB budgets. Retirement frees live admission capacity while preserving a bounded exact-target unknown receipt and shared payload.
- [x] Follow-up reservations have independent UUIDs, remain until accepted and survive refusal/unknown/closed-target as blocked actionable work. Replacement reservations cannot be removed by old ACKs.
- [x] Removed the eight-second optimistic `Idle/Off → Active` override. Runtime detectors/hooks own execution state. Composer shows “input accepted; execution not confirmed”.
- [x] Guarded batch retains InputAdmission permit/deadline through actual reservation; one authorization callback covers the entire batch.
- [x] Possible partial writer failure is `AdmissionUnknown`, never retryable queue refusal. Definitive zero-effect refusal stays rejected.
- [x] Runtime wire version 21 → 22; append-only command/reject variants preserve existing discriminants. Exact-version handshake rejects old peers before unknown-variant decoding.
- [x] Cloud receipt and web pressure mapping recognize unknown admission. No raw prompt payload in Debug or tracing.

## Shared APIs for subsequent PRs

```rust
ComposerInputPlan::into_parts(self) -> Vec<Vec<u8>>
RuntimeCommand::WriteInputBatchTracked { session, operation_id, parts }
InProcessRuntimeClient::send_guarded_input_batch(session, operation_id, parts, admission)
// Same existing RuntimeEvent::InputAdmitted { session, operation_id, result }.

ComposerSubmission::into_parts(self)
    -> (Arc<str>, Arc<[Arc<str>]>, u64 /* submission_id */)
ComposerUi::settle_submission(workspace_id, submission_id, prompt, outcome)
    -> Option<Arc<[Arc<str>]>>
PromptAdmissionOutcome::{Accepted, Rejected, Unknown}
```

Runtime accepts 1–32 parts, total ≤4 MiB, valid bounded operation ID. App outstanding operations and rolling receipts each have explicit 256-entry / 8 MiB budgets. A receipt’s unknown payload shares Arc storage. Accepted/rejected receipts retain metadata only; logs contain IDs, lengths and admission enum, never prompt bytes. These receipts are in-memory and bounded; durable session drafts are PR5 and durable MCP receipts are the existing cloud history contract.

## Observed RED

1. Actual default PTY policy: separate 4 MiB body and CR admitted the body then refused the CR; assertion failed `over-budget whole submission must refuse both parts`. `/tmp/deppy-pr1-red-queue-20261004.log`.
2. Actual Composer try_submit consumed a Korean multiline draft before host/PTY acceptance; assertion failed with left empty, right original text. `/tmp/deppy-pr1-red-composer-20261004.log`.
3. Defensive batch writer failure with deliberately mismatched channel capacity left a prefix but returned backpressure instead of unknown; regression failed. `/tmp/deppy-pr1-red-partial-20261004.log`. This is a deterministic defensive-path fixture, not a claim that production queue capacity normally mismatches its policy.

The final queue regression uses the new single batch admission and proves no prefix or CR is queued when the whole reservation fails. Additional default-policy near-full queue test proves body refusal cannot submit old input. Provider boundary test proves bracketed body and CR stay separate ordered messages.

## Final gated GREEN

Final proof uses the revised cross-worktree gate: it cleans all workspace package artifacts on a source-worktree switch and holds a global fcntl lock through compilation **and** test execution. This fixes both fingerprint reuse across worktrees and replacement during concurrent execution. Final consolidated batch exited **0**; log `/tmp/deppy-pr1-source-clean-final-batch-v3-20261004.log`. The first source-clean batch explicitly removed 6,971 workspace artifact files (2.2 GiB); final v3 stayed in the same locked source owner and recompiled the subsequent source fixes. Its `pr1_ -- --list` shows exactly this PR’s ten named App/Composer tests. All earlier ungated and old-gate logs below are superseded as final proof:

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test --offline --locked -q -p deppy-sijo --bin deppy-sijo pr1_ -- --test-threads=1 --nocapture
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test --offline --locked -q -p deppy-sijo --bin deppy-sijo ui::composer::tests -- --test-threads=1
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test --offline --locked -q -p runtime -p pty pr1_ -- --test-threads=1 --nocapture
cargo fmt --all -- --check
git diff --check
```

- PR1 App/Composer regression tests: **10 passed, 0 failed** (0.01s), `/tmp/deppy-pr1-gated-app-focused-final-20261004.log`.
- All Composer tests: **57 passed, 0 failed** (0.50s), `/tmp/deppy-pr1-gated-composer-final-20261004.log`.
- Default/defensive PTY queue regressions: **4 passed, 0 failed** (0.01s).
- Runtime batch/permission/stale-session/command validation tests: **3 passed, 0 failed** (0.13s), `/tmp/deppy-pr1-gated-runtime-pty-final-20261004.log`.
- Strict Clippy `--all-targets -- -D warnings` across App/runtime/PTY/session/web-remote/i18n passed with exit0.
- Final format and whitespace checks passed.

Earlier ungated full runs produced App 2,536 passed / 30 ignored, runtime 321 passed, and PTY/session/web-remote/i18n package runs passed. **They are provisional evidence only:** root identified that Cargo’s ordinary build lock ends before test execution and that workspace fingerprints can reuse another worktree’s artifacts even without overlapping test processes. An ungated or old-gate full count can therefore refer to another source. The orchestrator must rerun final integrated affected/full suites under `cargo_gate.py`.

## Independent review and corrections

Read-only configured-model Codex CLI review returned:

| Priority | Location | Finding | Impact | Next step |
|---|---|---|---|---|
| medium | app.rs / deadline pump | One-shot repaint was not rearmed | Pending UI could outlive timeout | Fixed; deadline regression passed |
| medium | app.rs / unknown capacity | Retired targets retained live budget | New submissions could be refused | Fixed; retired receipt/budget regression passed |
| low | app.rs / working marker | Composer/follow-up markers were not pruned | Session keys retained | Removed optimistic marker and override |

Review log: `/tmp/deppy-pr1-codex-review-fallback-20261004.log`. Review did not run tests in its read-only environment. Root performs the final independent review of integrated nine-PR source; no clean final integration claim is made by this isolated PR report.

Failed review-tool attempts: current CLI rejects `codex review -m`; use `-c model=...`. The skill-prescribed gpt-5.6 is unsupported by this ChatGPT account (HTTP400); the configured gpt-6.1-sol review ran successfully. Intermediate exhaustive-match/import/test-call compile errors were fixed before final gated GREEN. Strict Clippy then found two unnecessary borrowed references and that retained-snapshot try_submit no longer needs `&mut String`; it now takes `&str`. The matching fixtures use immutable borrows. Follow-up unnecessary-mut fixture findings were fixed; final v3 Clippy and all named/Composer/runtime/PTY gates pass. No product workaround or sandbox relaxation was used.

## Limits / remaining integration work

- Native Deppy GUI and real provider TUI are not launched. FIFO message boundaries, real isolated `/bin/cat` PTY admission, permission and controller bookkeeping are verified; real AI execution/completion is intentionally unclaimed.
- PR2 owns live AI eligibility/admission revalidation/draft-dialog guard. This PR does not turn ordinary shell input into an implicit automatic AI launch.
- PR5 owns persistent per-session draft identity and budgets; current Composer scope remains workspace-based until that integration.
- PR9 should use guarded batch plus shared encoding, preserve unknown/no-auto-retry semantics and original visible-session identity.
- Root should review the combined changes, run the gated full suites and build/version-verify without launching.

## Final revised-gate command

Executed from the PR1 worktree. The whole list runs under one source owner and one lock:

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","pr1_","--","--list"],["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","pr1_","--","--test-threads=1","--nocapture"],["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","ui::composer::tests","--","--test-threads=1"],["test","--offline","--locked","-q","-p","runtime","-p","pty","pr1_","--","--test-threads=1","--nocapture"],["clippy","--offline","--locked","-q","-p","deppy-sijo","-p","runtime","-p","pty","-p","session","-p","web-remote","-p","i18n","--all-targets","--","-D","warnings"]]'
```

Rolling receipts are bounded (256 / 8 MiB unknown payload bytes) and may discard their oldest entries. Composer and blocked follow-up text are separately retained; receipt eviction is never an automatic retry or a delivery-confirmed result. PR5 remains responsible for persistent, per-session draft recovery.
