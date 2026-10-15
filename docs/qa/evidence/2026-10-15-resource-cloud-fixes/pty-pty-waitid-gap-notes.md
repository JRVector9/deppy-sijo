# Waitid stop-notification and teardown-before-reaper correction

## Current objective and failed gate

Close the safe natural-shell-exit descendant cleanup regression without changing runtime authorization or raw cached-PGID fallbacks. Root actual `/private/tmp/deppy-cloud-fix-root-20261015/pty-natural-exit-green.log.attempt2` compiled successfully but exited101:61passed/5failed/1ignored. Both post-reap descendant tests still failed, and3 existing freeze/resume tests returned `PTY child exit identity mismatch`. This gate is failed, not partial GREEN. The10 foreground/exit tests now present include the original6 unchanged,2 strengthened post-reap tests, and2 new deterministic pre-poll tests. Root alone executes Cargo.

## Actual platform evidence and design

Root extended native `/private/tmp/deppy-cloud-fix-root-20261015/macos-zombie-probe.log` confirms WEXITED|WNOWAIT can report CLD_STOPPED5 after owned SIGSTOP on Darwin. Matching stopped/trapped/continued notifications now return pending with no snapshot and no consuming try_wait. Matching exited/killed/dumped permits the previous one-shot member capture and actual reap; unexpected PID/code errors now include exact expected/observed PID and code. Running pid0 and EINTR still defer without group scans or reap.

Drop/kill capture entry now calls the same shared exit observation while holding the child mutex, before taking the retained snapshot and before terminal close. This covers an invisible but unreaped Darwin zombie when teardown beats the25ms periodic reaper. A live-owner path still performs only one actual same-group capture, with the foreground group deduplicated; no periodic full group scan was added.

The raw bounded teardown reap helper retires further exit-based authority under the same child→state lock order before any consuming raw try_wait, including a later error/timeout. Fixed members were already taken for current cleanup and remain usable. Normal try_wait completion and ECHILD also retire new authority. Cached stdChild PID/status can therefore never grant later snapshot authority over a reused direct-child PID. Member birth/PGID/SID rechecks, own/foreign-group fences, existing frozen rollback, HUP20ms→TERM200ms→KILL, I/O cancellation/join and2s direct-child reap bound are unchanged. Capture failure remains fail-closed; no unpinned member or cached raw group is signaled on macOS/Linux.

## Fixture qualification

The old `inspect(shell.pid).is_none()` assertion did not prove reap on Darwin because zombies are hidden from proc_pidinfo/getsid. Existing post-reap Drop/kill tests now use non-consuming waitid and require ECHILD before testing cleanup, while preserving a live exact descendant identity and hard15s lifetime/rescue.

Two new `samegroup_descendant_before_reaper_poll_*` tests construct the same private owned shell using the concrete Unix constructor, stop/join its reaper while idle, then issue the existing same-group background/exit command. A fresh matching exit notification with WNOWAIT qualifies that the shell is exited and unreaped before Drop/kill. This deterministically catches the capture-entry gap with no public API, global signals, sleeps as authorization, or weakening of existing foreground/frozen/foreign tests. Rescue still completes before final assertions. The other8 tests continue through the real backend spawn path.

## Artifacts and validation

`pty-waitid-gap-tests-only.patch` and `pty-waitid-gap-product.patch` are separate increments against canonical first waitid attempt, with exact before/after hashes in `pty-waitid-gap-sha256.json`. Combined incremental `pty-waitid-gap-combined.patch` is frozen for independent review. Original-baseline cumulative archival patch is `pty-fix-waitid-gap-cumulative.patch`; do not apply it over already integrated R02 changes. All prior attempt artifacts and hashes are retained.

Worker executed rustfmt on owned files and scoped git diff --check, both exit0. No Cargo/build/product test or app launch/restart was executed by this worker. Root plans tests-only new2 actualRED, then product, fullPTY, unchanged exact runtime authorization handshake, and remaining workspace checks. Await Root results; no PASS claimed here.


## Final review correction and executed RED

Root executed tests-only deterministic pre-poll cases before product:0passed/2failed/exit101. Their hard-lifetime/rescue protection completed. This is actual RED, reported by Root; worker did not run Cargo.

Peer review found the initial capture-entry observation still had a live waitid(pid0)→Darwin owner inspection unavailable race. Final product now observes exit, inspects owner, and if inspection is unavailable repeats the same non-consuming qualified observation while still holding the child mutex, before taking the retained snapshot. Thus a shell exiting in that interval contributes its fixed members before master close/reaper stop. Already retired state cannot gain fresh PID authority; the matching live-owner path still pins the unreaped PID through its group snapshot. No additional periodic group scan was introduced.

Replacement final product is `pty-waitid-gap-final-product.patch`, against current canonical lib first waitid attempt. Its fixture is already the tests-only final fixture. `pty-waitid-gap-final-combined.patch` combines replacement product with the original tests-only increment for review of the complete follow-up; final cumulative archives the whole R02 scope from the original dirty baseline. Exact latest hashes are `pty-waitid-gap-final-sha256.json`. Earlier gap product/combined are superseded and retained as reviewed failed drafts. Worker rustfmt/diff-check exit0 after final correction; actual GREEN remains pending.
