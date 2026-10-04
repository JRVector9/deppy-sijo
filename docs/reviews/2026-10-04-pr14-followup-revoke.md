# PR14 — Follow-up reservation revocation

## Objective and source

Worktree `/private/tmp/deppy-audit-pr14-20261004`, branch `fix/audit-pr14-followup-revoke-20261004`, exact baseline `f64d8d9c1b9fac10a0e02756b97dacbb3abcb8ac`. Fix independent CLI medium2: Cancel/replacement removed reservations while already-queued tracked input retained an unrelated permit.

Changed only `crates/app/src/app.rs`, the existing `fleet.followup.blocked` notice in five locales, and this report. Root owns combined CLI review, global handoff, journal, integration and release. Workstep/TDD guidance applied. No native Deppy/user PTY, real AI, secrets, push, version/lock, runtime pump or Composer/library edits. This is source verification, not a release or native UI performance claim.

## Actual RED before implementation

Behavior-preserving production scaffolds first routed explicit Cancel through map removal and actual FollowUp dispatch through the original fresh-permit execution helper. Two private-runtime tests held a harmless admission barrier ahead of the original UTF-8 body plus one CR. A zero-byte positive control used the same original execution/admission path and succeeded before the candidate was queued. Cancel/successful bounded replacement then ran through the production helpers before releasing the worker.

`/tmp/deppy-pr14-red-runtime-revoke-20261004.log` exited **101**: **2 failed**, both canceled/replaced original batches returned `Ok(())` instead of `AdmissionDenied`. The tests compiled and reached actual runtime admission. Its one unused test import warning was removed before subsequent verification.

Lifetime RED used the same real queued-command fixture and a no-op revocation scaffold. `/tmp/deppy-pr14-red-lifetime-20261004.log` exited **101**: **6 failed**; each candidate admitted `Ok(())` when Cancel, replacement, Accepted/Unknown settlement, retirement or final reservation-owner Drop should have revoked pending authorization. Accepted/Unknown cases exercise the production settlement helper while a command is pending; they do not claim the fixture generated an early successful ACK for that candidate.

Both RED commands ran through the shared gate:

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["test","--offline","--locked","-p","deppy-sijo","--bin","deppy-sijo","pr14_","--","--nocapture","--test-threads=1"]]'
```

Private fixtures spawn only `/bin/sh` → `/bin/cat`, use UUID temporary roots, bound events to 1,024 and waits to five seconds, and clean up only their own runtime/root. They never bind to an existing app or user session. The barrier uses a different permit so Cancel never waits on the barrier's mutex.

## Production interfaces and ordering

- `QueuedFollowUp.input_permit: FollowUpInputPermit` owns one shared permission for the reservation. Map/receipt clones share it; every new production reservation gets a fresh owner. Debug exposes no token/process details; equality compares owner identity without copying the prompt.
- `followup_input_admission(queued, deadline)` passes that same raw `InputPermit` into `InputAdmission::new`, checks the frozen execution's `is_current()`, and retains its `AutomaticPrompt` guard. The dispatch deadline is bounded and not refreshed by the authorization callback. Composer, selected paste and broadcast keep their existing paths.
- `cancel_followup_reservation` revokes **before** map removal. `admit_followup_reservation` preserves all unknown/count/prompt/metadata validation, then revokes a replaced permit **after** every check succeeds and **before** insertion. Failed replacement preserves the old reservation and actual queued authorization.
- `settle_followup_admission` revokes the receipt owner's permission, then checks the reservation ID before mutating the map. Old ACK affects only the old owner. Matching Accepted removes; Rejected/Unknown retain the blocked original. Unknown stays sticky, with no automatic retry/replacement.
- Final `OwnedFollowUpInputPermit::drop` revokes as fallback. A queued `InputAdmission` holds the raw permit, not this owner wrapper, so final-owner cleanup can revoke an already-queued command.
- `revoke_followup_authorizations` handles map owners and pending FollowUp origins. It blocks matching map entries without deleting prompts or clearing Unknown; other input origins are untouched.

The existing runtime permission mutex is held through actual PTY queue retention. Revocation linearizes with admission: after revoke returns, that permit admits no further input. If admission won first, retained bytes cannot be undone. Cancel does not fabricate rejection or resolve Unknown. All five notices explain reservation removal, stopping input before admission, and the inability to undo admitted/unknown input or automatically retry.

### Lifetime seams inspected and wired

1. `shutdown_on_exit` and `Drop for App` revoke as their **first action**, before runtime shutdown/field Drop drains queued commands. RAII-only cleanup after shutdown was inadequate: the runtime consumes queued commands before stopping.
2. `prune_agent_display_for_retired_instance` revokes matching original-runtime reservations/pending origins. Its three definitive retirement callers remain unchanged: benchmark workspace deletion, warm suspension after the live-work return guard, and warm workspace close.
3. `close_workspace_sessions` revokes matching workspace permissions before active close dispatch/warm shutdown, retaining blocked/Unknown reservations. Workspace hiding and Composer draft policy remain unchanged.
4. `drain_workspace_protocol_intents` revokes the exact original session before ClosePane/CloseTab/KillSession dispatch, mapping pane/tab through that runtime's existing mux. Missing mapping does not authorize broad removal. Explicit close attempts leave a retained blocked reservation if close delivery subsequently fails.
5. Resource maintenance and storm Kill revoke matching workspace/runtime/session before KillSession dispatch. Freeze/Resume remain unchanged.
6. Proven-unavailable targets revoke before becoming blocked. Missing agent/UI projection alone remains unproven. No purge, rebinding or automatic retry was added.

Existing bounds remain **256 reservations / 4 MiB logical prompts / 256 KiB logical metadata**. Metadata accounting includes the new owner size. These are logical application budgets, not allocator/RSS claims. `Arc<str>` sharing and exact runtime/session/payload/reservation-ID settlement remain intact.

## GREEN and affected gates

All Cargo used `cargo_gate.py`: lock covers compilation **and execution**, workspace artifacts are cleaned on source path switch, and source stays fixed during each batch. Logs include exact worktree/compile paths and final named-list output.

| Log | Actual result |
|---|---|
| `/tmp/deppy-pr14-green-initial-20261004.log` | exit0; initial seven private-runtime tests and fmt passed |
| `/tmp/deppy-pr14-final-affected-20261004.log` | exit0; named nine/ran nine; PR4 26, PR1 10, PR5 29, Fleet UI 43 + one existing ignored, i18n 8; strict App all-target Clippy and fmt passed |
| `/tmp/deppy-pr14-final-ack-20261004.log` | exit0; final named ten/ran ten private-runtime tests; PR1 10; strict App all-target Clippy and fmt passed |

Actual controls prove rejected replacement and unrelated retirement preserve input, pending-origin-only retirement revokes while retaining Unknown, and a late old ACK does not revoke/remove a queued replacement. The last test observes replacement admission exactly once and its body in the private viewport. The prior PR1 map fixture was corrected to give a new reservation a new permission owner. No production changes followed the affected gate; the final batch verifies those test edits and ACK isolation.

Final affected command:

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["fmt","--all"],["test","--offline","--locked","-p","deppy-sijo","pr14_","--","--list"],["test","--offline","--locked","-p","deppy-sijo","pr14_"],["test","--offline","--locked","-p","deppy-sijo","pr4_"],["test","--offline","--locked","-p","deppy-sijo","pr1_"],["test","--offline","--locked","-p","deppy-sijo","pr5_"],["test","--offline","--locked","-p","deppy-sijo","ui::fleet::tests"],["test","--offline","--locked","-p","i18n"],["clippy","--offline","--locked","-p","deppy-sijo","--all-targets","--","-D","warnings"],["fmt","--all","--","--check"]]'
```

Final ACK isolation command:

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["fmt","--all"],["test","--offline","--locked","-p","deppy-sijo","pr14_","--","--list"],["test","--offline","--locked","-p","deppy-sijo","pr14_"],["test","--offline","--locked","-p","deppy-sijo","pr1_"],["clippy","--offline","--locked","-p","deppy-sijo","--all-targets","--","-D","warnings"],["fmt","--all","--","--check"]]'
```

`git diff --check` passed. Final GREEN logs have no warnings/errors; no lint suppression was added. The first RED unused import and RAII-only shutdown approach were resolved. A report rewrite using delete/add for one path was rejected atomically by apply_patch; retried as one update, no source lost. No full combined workspace suite or independent follow-up CLI review is claimed here; root runs those after integration with `gpt-6.1-sol` / `xhigh`.

## Remaining handoff

Implementation/scoped verification complete. Root integrates the frozen commit, reviews actual source, and runs coherent combined gates. Root owns global handoff/journal/release. No release version or artifact was produced here.
