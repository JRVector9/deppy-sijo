# PR4 review corrections — Fleet forms and reservations

## Objective and source

Worktree `/private/tmp/deppy-audit-pr4r-20261004`, branch `fix/audit-pr4r-fleet-review-20261004`, exact baseline `1c4304f8f06bfaf8b50dbabe31a6d88656935b90`; verified against manifest worktrees4r. Root owns handoff/PR5 integration/independent review/release. No native app launch, stop/restart, user data/PTY/clipboard, version/lock, push or delegation.

Scope: follow-up App admission/map, fleet viewmodel/UI, backward-compatible shared bounded text style wrapper, five append-only locales, this report. Composer/draft_store/lifecycle belong to PR5 and are not edited.

## RED evidence

The exact baseline includes root's three actual egui regressions, all failed in `/tmp/deppy-root-fleet-review-red2-20261004.log`: batch host16KiB form limit, follow-up paste keeps original, parameter count/expanded result limit. Their source is preserved.

Fresh isolated RED `/tmp/deppy-pr4r-red-forms-map-20261004.log` exited101: all three actual Fleet regressions failed on the original behavior. The gate stops on failure, so the map RED ran separately. `/tmp/deppy-pr4r-red-map-20261004.log` exited101: new reusable admission scaffold accepted a257th item (`Ok(())` versus expected limit rejection), before actual map limits were implemented.

First GREEN attempt `/tmp/deppy-pr4r-green-initial-20261004.log` failed compilation on `Arc<str>` hover text; corrected to borrowed `&str`. No tests passed in that attempt.

## Implemented interfaces

- Shared `fleet::FLEET_PROMPT_MAX_BYTES =16KiB`; batch rendered result checks before action/closure. Broadcast still uses1MiB.
- `bounded_edit_with_style` preserves the old wrapper and adds normal-font four-row follow-up input, shared conditional rollback and bounded8undo. Fixed field ID resets once per actual open. Stable trim is borrowed; template append checks separator plus body before mutation.
- `FleetUi::settle_followup(target, accepted)` settles only the exact frozen target. Host refusal keeps the draft with a shared popup Error notice.
- `admit_followup_reservation` atomically checks256items,4MiB logical prompt bytes,256KiB logical metadata, per-prompt16KiB and replacement deltas. Unknown admission is sticky and cannot be replaced.
- `QueuedFollowUp.prompt` and `FleetSession.followup` share `Arc<str>` bodies. Visible blocked projection is count/key-only, deterministically ordered, with no kernel liveness polling. Only absent original runtime, changed workspace, confirmed exit/removal from an authoritative complete runtime MuxUpdated, or observed execution mismatch prove unavailable for the reservation; missing agent projections retain reservations.
- Inline blocked Cancel removes only the reservation. It cannot undo already admitted/unknown input, retry, or rebind to a replacement session. Original sender/ACK exact reservation equality remains unchanged.
- Read popup contract and linked HTML inventory. Existing Fleet Window layout remains; the follow-up notice contract and matching case metadata are updated without a shell migration.

## Verification so far

Fresh GREEN `/tmp/deppy-pr4r-green2-20261004.log`: fmt and all24 named `pr4_` tests passed (including actual Fleet forms, map capacity/replacement/metadata/unknown, exact form settlement, template separator, missing projection, fixed-ID reopen/undo). New final boundary coverage additionally verifies the same1MiB broadcast remains accepted while16KiB batch rejects it, plus actual follow-up overflow IME.

## Actual event-order correction

Self review found the actual tracker compacts `exited_sessions` after a same-drain pane-removal `MuxUpdated`. Checking only an exit marker could leave an inaccessible closed reservation unblocked. Added `pr4_followup_removed_session_stays_cancelable_after_exit_marker_cleanup` on actual `LiveSessionTracker::observe` before fixing that condition. RED `/tmp/deppy-pr4r-red-session-removal-20261004.log` exited101: the complete original runtime snapshot removed the session and cleared the exit marker, but the availability helper still returned false.

Root approved complete actual runtime snapshot absence as reservation unavailability, distinct from claiming a process is dead. `followup_session_unavailable` now checks confirmed exit OR `seen_mux && !mux_sessions.contains(original_session)`. It blocks/retains for explicit Cancel, with no lifecycle mutation or purge; initial snapshot absence and missing agent projection still remain unproven. Capacity test also independently exercises257 tiny prompts to prove the item bound without the byte cap.

## Final verification and handoff

Final source-fresh gate `/tmp/deppy-pr4r-final3-gates-20261004.log` exited0:

| Gate | Actual result |
| --- | --- |
| Named `pr4_ --list` |26 exact tests,0 benchmarks|
| `pr4_ --nocapture --test-threads=1` |26 passed,0 failed/ignored|
| Full `ui::fleet::tests` |43 passed,1 existing ignored|
| `pr1_ --test-threads=1` |10 passed,0 failed/ignored; original reservation/ACK and unknown contracts preserved|
| `-p i18n` |8 passed,0 failed; five locales match key order/placeholders|
| `clippy -p deppy-sijo --all-targets -- -D warnings` |passed with no dead_code allowance|
| `fmt --all -- --check`, `git diff --check` |passed|

The complete prior gate `/tmp/deppy-pr4r-final2-gates-20261004.log` also passed25/list25, Fleet43+1ignored, PR1_10, i18n8 and strict Clippy before the actual exit/removal regression was added. The new regression's RED/GREEN evidence is separate above. No native App process or user PTY was used. Tests exercise real egui forms, the actual admission/map helper, actual runtime-event tracker, and the original receipt settlement. No claim of native GUI/FPS/RSS, process death from a removed pane, or completed root integration review.

Actual final command (all Cargo through the shared gate):

```sh
cd /private/tmp/deppy-audit-pr4r-20261004
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["fmt","--all"],["test","--offline","--locked","-p","deppy-sijo","--bin","deppy-sijo","pr4_","--","--list"],["test","--offline","--locked","-p","deppy-sijo","--bin","deppy-sijo","pr4_","--","--nocapture","--test-threads=1"],["test","--offline","--locked","-p","deppy-sijo","--bin","deppy-sijo","ui::fleet::tests","--","--test-threads=1"],["test","--offline","--locked","-p","deppy-sijo","--bin","deppy-sijo","pr1_","--","--test-threads=1"],["test","--offline","--locked","-p","i18n"],["clippy","--offline","--locked","-p","deppy-sijo","--all-targets","--","-D","warnings"],["fmt","--all","--","--check"]]' > /tmp/deppy-pr4r-final3-gates-20261004.log 2>&1
git diff --check
```

Self review checked all production map writes now use the bounded helper; rejected admission never mutates the existing map; unknown status is sticky; explicit Cancel is the sole key removal outside accepted correlated delivery settlement. Full Fleet rendering exposes blocked Cancel even with no session cards. Missing initial runtime snapshots or agent/UI projections do not delete/block by themselves; authoritative complete original runtime removal does block/retain. No process liveness syscall occurs in follow-up summary projection. Exact sender/runtime/session/execution checks and reservation-ID ACK equality remain in the original shared sender/settlement.

Bounds are logical payload/metadata bounds, not an allocator or whole-App RSS limit. Existing pending delivery storage has its own separate bound. Immutable Arc sharing avoids full-body view/origin clones; copying occurs only when opening an editable form or admitting actual input.

Modified files: `app.rs` follow-up admission/projection; `fleet.rs` shared byte constant/Arc viewmodel; `ui/fleet.rs` forms/settlement/blocked Cancel/tests; `ui/text_input.rs` compatible style wrapper; five locale files; popup design/numbered HTML notice contract; this scoped report. No Composer, draft store, lifecycle, version/lock, native app, root global handoff or push changes.

Root next: integrate the clean scoped commit against its evolving PR5 source; independently review actual combined source with `gpt-6.1-sol`/`xhigh`; run combined gates/release version/build/package verification without launching Deppy. This isolated source is frozen after the meaningful gates.
