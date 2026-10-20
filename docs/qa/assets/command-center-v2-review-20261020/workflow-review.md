| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| high | crates/app/src/command_center_observe.rs:294 | Deferred local input is sent after its attempt is retired | Completed work can begin executing afterward | Revalidate exact assignment immediately before send and inside retained admission |
| high | crates/app/src/command_center_host.rs:1186 | One volatile confirmation token strands earlier and restored goals | Saved queued/dependency tasks cannot launch | Keep confirmation per goal and add validated restore/reconfirmation |
| medium | crates/app/src/command_center_host.rs:2006 | Completed projects permanently retain cache eviction blockers | A fifth project cannot load after four used projects | Release transient request/confirmation when their work is terminal |
| medium | crates/app/src/ui/command_center/main.rs:500 | VerificationFailed hides the native remote retry action | Failed remote verification cannot start a fresh attempt | Include VerificationFailed in CreateWorker control gate |
| medium | crates/app/src/command_center_host.rs:1209 | Approved exact approval flow remains fixture-only | Native and PWA cannot review/approve an assigned task request | Connect exact supported owner APIs or declare scope incomplete |

## Review boundary

Read-only review of HEAD `9c0a820a7805c4f25a1a6674f3b657fff464eb06`, baseline `f78ebb69c94ec7cb72ae9a1159d2137c0d6a5679`, in `/Users/jr/Desktop/projects/deppy-sijo-command-center-20261020`. Read AGENTS.md, latest handoff, approved design, differential implementation and actual host/runtime call sites. No source edits, Cargo, app launch/stop/restart, release, push or paid provider calls. Temporary isolated harnesses and an owned harmless PTY fixture only. `docs/CODEX_HANDOFF.md` was modified by the parent during review; this reviewer did not edit it.

## 1. Deferred local input outlives manual task retirement

**Trigger:** create/dispatch a local Codex task; its durable outbox ACK passes `assignment_current` at host1454–1460 and parks the prompt in `Observations.delayed` while the async NativeIdentity settings request runs. Before that response arrives, record a manual result, verification and user confirmation for the same attempt. The runtime CLI remains alive at its unchanged empty editor.

**Proof / exact locations:**

- Task `result_report` is enabled as soon as an attempt exists, even while `host.inputs` owns initial input (host790–791). ReportResult/VerifyResult/ConfirmResult only test `remote::handoff_pending` at host1211/1223/1233; local initial input is absent from that predicate.
- Actual reducer accepts `ResultSubmitted` from Queued without an InputAdmitted event (reducer293–302), then verification/user confirmation. The isolated actual-source harness reaches Completed without any PTY admission.
- NativeIdentity callback at observe283–294 checks runtime, session, token, deadline and same route only. At observe323 it unconditionally schedules the saved prompt. Its send helper checks only `host.inputs` and optional cloud permit (observe343–348). It never rechecks `assignment_current` or terminal attempt state.
- Downstream `broadcast_prompt_to` forwards to `queue_prompt_delivery` (app33945); app34008–34108 checks pending bounds, current process identity, agent equality and runtime owner, then builds an agent/cloud guard and queues `send_guarded_input_batch`. No workflow revalidation occurs. `agent_detect.rs:197–220` admission captures execution process identity only. Actual runtime `input_admission.rs:235–306` checks foreground/editor/draft/dialog, with no task/attempt reference. A still-idle original CLI passes these checks.
- Executed source harness: `DELAYED_RETIRED_INPUT: exact production callback and send gate call broadcast_prompt_to for already Completed attempt`.
- Independently executed real owned PTY fixture using production `InputAdmission` + `AgentInputGuard`: AutomaticPrompt passes at empty Codex editor and actual PTY receives the marker. This verifies the downstream gate is not a hidden workflow fence; it does not simulate a paid provider or the full desktop app.

**Impact:** a user can finish or supersede the workflow's result while old task instructions are still unsent; a delayed callback can then enqueue those instructions into its original still-current CLI after the UI says Completed. This is a post-save race, distinct from the existing pre-save `assignment_current` regression test.

**Fix:** reject/defer manual retirement while local initial handoff is outstanding, and/or retain an exact immutable attempt/config validity fence in local input admission; recheck the original dispatch at the actual deferred send. Add regression for outbox ACK → delayed NativeIdentity → manual terminal result → callback. Preserve the known-unsent receipt without replay.

## 2. Single nonpersistent confirmation token strands queued goals

**Triggers:** (a) confirm a valid plan, save, and restart before an unstarted task runs; (b) confirm a second goal while the first goal has queued/dependency-blocked tasks; (c) save project settings while a confirmed plan still has undispatched tasks.

**Proof / exact locations:**

- `Project.confirmed` is a single `Option<Confirmed>` at host60, is initialized None at host626, and is not a member of `stored::CommandCenterSnapshot` (storage/work_management.rs177–195).
- ConfirmPlan replaces it at host1186, even though the state supports multiple goals. `Project::ready_tasks` requires that token (assignment19–22); queue iterates only its goal's tasks.
- SQLite reload restores goals/tasks and only creates a planner request for a goal in PlanReview at host1383–1395. A saved confirmed plan's goals/tasks remain Queued; `recover_observation` does not turn undispatched tasks back into PlanReview.
- There is no restore/reconfirm intent. ConfirmPlan additionally requires `p.proposal` at host1181; `ConfirmedPlan::confirm` refuses goals no longer PlanReview (dispatch204–212), and cannot confirm partially started plans (dispatch224–233).
- SaveProject explicitly clears request/proposal/confirmed at host1122, with no path to review/reconfirm remaining queued tasks under new settings.
- Actual SQLite save/load + production reducer/dispatch/queue harness: restored queued task has no PlanReview request; reconfirm error is `goal is not awaiting plan confirmation`. Second goal token excludes a first-goal task which remains ready with its original token; prepare with replacement token returns `task is not ready: confirmation, dependencies, capacity or file scope`.

**Impact:** persistence preserves visible work but not the capability required to continue its undispatched tasks. Confirming a new goal can strand remaining work during the same host lifetime. This affects local and mixed plans; proof harness executes local actual code, with shared host storage/capability gate establishing mixed impact.

**Fix:** retain exact confirmation per goal, and provide a validated explicit restore/reconfirmation flow that preserves completed/running attempts and reviews remaining work under current settings. Do not implicitly replay uncertain operations or recreate already-started work.

## 3. Four completed projects cannot free a loaded slot

**Trigger:** load and fully complete plans in four projects, then select/load a fifth project.

**Proof / exact locations:** ConfirmPlan clears only proposal and retains `request` + `confirmed` (host1186); result completion only replaces state/events and never clears either. `admit_project_load` requires both fields None at host2006/2008 even after all goals/tasks are Completed and generation==saved_generation. The isolated check supplies a **synthesized terminal cache fixture** to the exact extracted production admission function: four loaded map entries share a cloned Completed snapshot, have saved-generation equality, and retain request/confirmation options. It does not assign four unique snapshot workspace IDs or prove four real projects completed end to end. The function returns `작업 중이거나 저장되지 않은 프로젝트가 4개 열려 있습니다...`; clearing those transient fields makes that same call succeed. This establishes the admission-predicate defect with source tracing, not a full application reproduction.

**Impact:** normal completed work permanently occupies the four-project memory admission budget until settings changes/restart. The error incorrectly describes all four as active or unsaved. A restart is a particularly bad workaround because finding2 prevents continuing queued plans.

**Fix:** retire/cull transient request and confirmation data when no live/retryable tasks require it, or make eviction depend on durable active work and pending effects rather than a stale option. Preserve data in SQLite and avoid dropping actual pending work.

## 4. Native remote retries vanish after failed verification

**Trigger:** a remote task submits a result and user verification records failure; its preceding runtime is closed or otherwise not reusable, and the same device remains live/authorized with available capacity.

**Proof / exact locations:** reducer `can_dispatch` permits VerificationFailed at reducer363–378. Host CreateWorker capability uses ready_tasks/can_dispatch (host773–785), but native Work tab emits CreateWorker only for PlanReview|Queued|Failed|Cancelled (main500–506). Remote tasks have no eligible local session because host745 requires `execution_device.is_none()`; DispatchTask therefore supplies no alternative. Follow-up manual input can still run the old execution, but cannot allocate the intended new attempt and cannot recover a closed runtime.

**Impact:** the native UI hides a supported retry precisely for verification failure. Local tasks without an eligible existing session have the same missing-create limitation.

**Fix:** include VerificationFailed in the CreateWorker rendering gate, using the already computed host capability, and check fresh-attempt behavior after a failed verification. This finding is source/callsite verified; no physical remote device UI test was run. Parent UI reviewer was notified for independent corroboration.

## 5. Approved exact approval flow is an unimplemented scope (shared with PWA reviewer)

Confirmed that real native Task.approval always remains Default None; Intent::Approval always rejects. New generated workers are PTY SpawnAgent sessions (`command_center_launch.rs:419–525`); existing `AgentCapabilities::for_transport(Pty)` intentionally disables exact approve/reject. Codex AppServer does have exact request_key approval APIs in `ui/agent_sessions.rs:1507–1554`, but Command Center maps Structured routes to no agent/cwd/watch at host578, and they have no prompt target. They cannot be main or assigned workers. Thus approval fixture flow is an unimplemented approved-design scope, not an accidentally misrouted approval. Parent should deduplicate with PWA reviewer. Classify **medium planned feature omission**, not a PTY capability regression or unsafe approval. The approved design promises exact command/folder/device review and allow/deny. These controls work only in fixtures; the actual task source never populates them and no provider transport is wired into this flow. Fix by mapping a supported exact request/owner into task projection and dispatching the existing exact owner API; keep PTY requests without verifiable ownership manual. Real original UI approval/status bar lies after early return app35744, so Command Center itself provides no alternate structured approval projection.

## Actual commands / results

All commands below were actually executed successfully, except the initial compile-only attempt which missed a local test stub and is recorded below.

```sh
cd /Users/jr/Desktop/projects/deppy-sijo-command-center-20261020
python3 /private/tmp/deppy-cc-review-workflow-20261020.py
rustc --edition=2024 -Awarnings -L dependency=target/debug/deps --extern deppy_core=target/debug/deps/libdeppy_core-9df8a50c81d96771.rlib --extern storage=target/debug/deps/libstorage-42b4523fff7ed40a.rlib /private/tmp/deppy-cc-review-workflow-20261020.rs -o /private/tmp/deppy-cc-review-workflow-20261020
/private/tmp/deppy-cc-review-workflow-20261020 > /private/tmp/deppy-cc-review-workflow-20261020.log
rustc --edition=2024 -Awarnings -L dependency=target/debug/deps --extern deppy_core=target/debug/deps/libdeppy_core-3462f9a7250bf315.rlib --extern session=target/debug/deps/libsession-4bf622aadc3399bc.rlib --extern terminal=target/debug/deps/libterminal-2f38dc08e962ff46.rlib --extern pty=target/debug/deps/libpty-7b2af18c3018f142.rlib --extern tracing=target/debug/deps/libtracing-fe42047bec45d0d2.rlib /private/tmp/deppy-cc-review-admission-20261020.rs -o /private/tmp/deppy-cc-review-admission-20261020
/private/tmp/deppy-cc-review-admission-20261020 > /private/tmp/deppy-cc-review-admission-20261020.log
```

Workflow harness includes actual pure reducer/dispatch/queue source with #[path], uses actual SQLite store rlib, and extracts production recover_observation / admit_project_load / NativeIdentity callback / send bodies. App/fleet/observation services are minimal local stubs; final broadcast is recorded, not an app execution. Guard harness includes actual runtime input_admission.rs and actual session/PTY/terminal rlibs, starts only its own shell/cat fixture, and exercises real guarded retention. Root independently executed the workflow harness exit0.

Initial workflow harness compile failed because stub `reject_delayed` was omitted; corrected only the temp harness. This was not a product test failure, not behavioral RED. Successful executions are **four output scenario checks across three workflow harness functions**, plus one downstream PTY fixture; they are not full App tests or workspace/App Cargo pass counts. Existing full-suite claims were not repeated as fresh validation.
