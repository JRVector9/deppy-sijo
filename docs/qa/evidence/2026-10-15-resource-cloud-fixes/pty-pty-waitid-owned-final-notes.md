# Final owned-child WNOWAIT correction — frozen source

## Objective and scope

Preserve normal foreground teardown and prior same-shell-group descendant cleanup after natural shell exit, with exact identities and runtime authorization ordering intact. Only owned PTY product/fixture files changed. No runtime fixture, authorization guard, docs/Cargo/app/provider changes from this worker. Root owns canonical writes and all Cargo executions.

## Executed failures and platform evidence

Root initial post-reap follow-up failed64passed/2failed/1ignored. Darwin proc_pidinfo and getsid hide an exited but unreaped owned child, so the procinfo-zombie approach lost its capture before reaping. The first WNOWAIT attempt compiled but failed61passed/5failed/1ignored/exit101 in `pty-natural-exit-green.log.attempt2`: both post-reap tests plus3 existing freeze/resume tests. Root's actual native `macos-zombie-probe.log` confirmed WEXITED|WNOWAIT returns matching CLD_STOPPED5 for an owned stopped child, later matching CLD_EXITED1, then ECHILD after actual reap. First8 test fixture inspect(None) was not proof of reap and has been strengthened. Root applied tests-only and actually ran the2 new before-poll cases RED:0passed/2failed/exit101; rescue completed. These failures remain preserved, not relabeled as successes.

## Final design

A single non-consuming observer returns PendingOwned, ExitedOwned, Retired, or Deferred. Fresh successful P_PID WNOWAIT with pid0 or matching stopped/trapped/continued notification proves the original direct child is still unconsumed. Only matching exited/killed/dumped is terminal and permits the normal polling path to scan exact original-group members once before actual try_wait. EINTR defers with no consuming wait. ECHILD, consumed status or explicit teardown permanently retire exit-based capture authority under the same child→state lock order. Unexpected PID/code errors include diagnostic values and fail closed. Zeroed siginfo avoids stale no-result fields.

Teardown uses that same non-consuming observer under the child mutex before taking the retained fixed-member snapshot. It does not reap at capture entry. Successful pending/terminal observation pins the original direct child's identity through subsequent capture, closing both already-invisible and live→exiting visibility gaps without a retry loop. If procinfo is unavailable, capture is permitted only for the original shell group when saved PID==PGID==SID; the original unreaped session leader keeps those IDs allocated. FrozenGroup capture enforces positive/foreign own-group fences and every captured member's original SID; signaling later rechecks every PID/birth/PGID/SID. A visible mismatching owner rejects capture. Retired/deferred/error states retain only already pinned members.

Limits are deliberate: no fresh foreground authority after owner inspection becomes unavailable or after reap; no all-session scan; no raw cached-PGID fallback on macOS/Linux; no promise to catch all future descendants or unpinned forks. While a matching owner is visible, actual terminal foreground capture remains. Periodic running polls remain scalar WNOWAIT only and never scan groups. At terminal exit, only the original shell-group compatibility snapshot is added. Unknown/new/reused members never widen fixed-member signaling or block a valid pinned member.

Existing HUP20ms→TERM200ms→KILL, frozen rollback/termination, I/O cancel/join and bounded2s shell reap remain. Explicit Unix teardown retires further snapshot authority before any raw try_wait, even error/timeout, and ECHILD skips direct-child kill. This prevents a cached/reused old PID from gaining authority after teardown.

## Fixtures

Original6 real backend foreground/frozen controls are unchanged. Existing post-reap2 now require non-consuming waitid ECHILD, not procinfo absence, and require the same descendant execution still alive. New2 before-poll cases construct the same owned private shell via concrete Unix constructor, stop/join its reaper while idle, then issue the same group descendant/exit command and require matching terminal WNOWAIT before Drop/kill. No public test API or timing-based authorization. Child hard lifetime15s and identity-checked rescue remain; no global signals.

## Frozen artifacts and verification

Only apply `pty-waitid-owned-final-product.patch` over current canonical first WNOWAIT attempt lib `edacafe...48e7c`; tests-only has already been applied by Root. Exact hashes are in `pty-waitid-owned-final-sha256.json`. Review combined includes original tests-only increment; cumulative is archival from the original exact dirty baseline. Earlier waitid/gap/final draft patches are superseded and retained as investigation history.

Worker executed rustfmt and scoped diff-check exit0, no Cargo/build/tests/app launch. Management peer is reviewing exact final source. Root actual fullPTY, unchanged authorization handshake and remaining workspace verification are pending; do not claim PASS before those logs finish.


## Actual final affected GREEN — Root executed, worker inspected

Root completed the exact final frozen shared-observer patch. `/private/tmp/deppy-cloud-fix-root-20261015/pty-followup-green-results.json` records both command exits0; worker inspected both completed logs directly.

- `cargo test --offline --locked -p pty -- --nocapture --test-threads=1`:68passed/0failed/1ignored; doctests0, log `pty-natural-exit-green.log`. All10 actual foreground/natural-exit cases and existing freeze/foreign/identity safeguards passed.
- `cargo test --offline --locked -p runtime pr13_exit_teardown_releases_authorization_before_revocation_handshake -- --nocapture --test-threads=1`:1passed/0failed/405filtered, unchanged authorization ordering test, log `runtime-exit-handshake-green.log`.
- Tests-only new before-poll RED command was `cargo test --offline --locked -p pty samegroup_descendant_before_reaper_poll -- --nocapture --test-threads=1`:0passed/2failed/exit101 before final product; exact result JSON `pty-before-reaper-red-result.json`.

Management peer confirmed exact final source/hash review clean for assigned lifecycle/ownership/performance invariants; no Cargo from peer. Root broader appcloud/workspace/clippy/fmt/boundary/dependency gates are running separately and are not claimed complete here. Worker has no code/test modifications after final source freeze. No app launch/restart, release/version bump, canonical docs/Cargo write or commit from worker.
