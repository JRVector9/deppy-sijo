# R02 normal foreground teardown worker handoff

Objective: fix the qualified non-frozen job-control foreground process leak on pane/session Drop and explicit kill, without touching existing frozen/query protections.

Owned source: `crates/pty/src/lib.rs`, `crates/pty/src/workload_signal.rs`, new `crates/pty/src/foreground_teardown_tests.rs`. Only those existing source files differ from the exact supplied baseline (scope.txt); no canonical source/docs, Cargo, app/restart/build/provider/userDB, commit/version operations by worker.

Design:
- Before frozen cleanup, master closure or shell reap, take child mutex and require current shell PID/birth/PGID/SID to equal spawn identity; capture its current shell group and tcgetpgrp foreground group. App-own, foreign-session, empty and inspection-failed groups fail closed. Capture failure does not stop frozen cleanup, worker cancellation/join or bounded direct-child reap.
- Normal groups signal only members whose captured PID/birth/PGID/SID still match at signal time. Direct per-member signals avoid either rejecting captured members or broadening signal authority when a running group forks. Newly born/escaped/background members are outside this bounded current-member fix.
- HUP grace20ms, TERM grace200ms, then KILL, stop early once no captured execution is live. Existing frozen group KILL and rollback stay unchanged. I/O is cancelled before normal signaling and joins are retained.
- Direct Unix shell kill checks cached try_wait status under the child mutex before kill, ignores ESRCH, and reaps with the existing2s bound. This prevents the new HUP/TERM phase from killing a reaped/reused shell PID.
- macOS/Linux use identity capture. Existing unsupported-Unix raw shell-group fallback is retained behind cfg; Windows/other paths untouched.

Behavioral regressions (not run by worker): six real bash --noprofile --norc -i / Python job-control fixtures, private cwd and history/env values. Python has15s hard lifetime and records actual PID/PGID/SID; Rust pins full identity and rescues only that exact PID before asserting. Cases: normal HUP control; ignored HUP Drop and kill-alone; ignored HUP+TERM Drop and kill-alone; frozen control. Additional helper regression alters birth/group/session independently, requests KILL, requires actual child survive, then checks valid captured TERM termination; own group capture rejects. Existing unrelated-session/frozen/rollback tests remain.

Executed worker validation: rustfmt --edition2024 --config skip_children=true on three owned source files exit0; git diff --check -- three owned paths exit0; baseline byte comparison found only owned lib.rs/workload_signal.rs existing-file changes. No Rust compile or product test result is claimed.

Existing actual RED: qualified private reproduction /private/tmp/deppy-cloud-review-perf-20261015/pty_nonfrozen.rs + owned-workload.py/report.md/identity.json and Root independent log in docs/reviews/evidence/2026-10-15-resource-cloud-code-review. Source regression tests-only.patch is preserved separately for Root RED; complete baseline-relative final patch is pty-fix.patch. If tests-only already applied, product-after-tests.patch adds only product/helper changes and module-order formatting. The final fixture has rustfmt-only formatting changes from tests-only.patch; final canonical source can copy only this owned new file after source review.

Root next commands (canonical directory, sole Cargo owner):
1. Apply tests-only.patch against original exact baseline, then cargo test --offline --locked -p pty foreground_teardown_tests -- --test-threads=1; record actual RED counts/exit before product integration.
2. Apply product-after-tests.patch (or pty-fix.patch only against original baseline); import final formatted new fixture file; inspect only baseline task diff.
3. cargo test --offline --locked -p pty foreground_teardown_tests -- --test-threads=1
4. cargo test --offline --locked -p pty -- --test-threads=1
5. Independently compile and run original qualified production PTY harness against the newly rebuilt current production rlib; record binary/source SHA and exact rlib path. No Deppy restart/launch.
6. Run workspace/lint/fmt gates per Root plan, update canonical docs/CODEX_HANDOFF.md with actual results.

Remaining: Root RED/GREEN/fullPTY/rebuilt-harness/lint integration; peer management review when Root assigns. No failed product implementation experiment yet; tests-only fixture had one statically caught env String conversion corrected before handoff, never claimed as behavioral failure or pass.

Root actual new-fixture RED received after handoff: `/private/tmp/deppy-cloud-fix-root-20261015/pty-foreground-red.log`, compile exit0, behavior exit101, 2PASS normal/frozen controls and4FAIL non-frozen HUP/HUP+TERM Drop/kill,6selected. Root reports every fixture rescued before final assertion. This result was executed by Root, not rerun by worker. Product integration/GREEN remains Root-owned.

Integration compatibility phase: Root affected pty64PASS/1ignored was followed by actual full-workspace exit101, runtime404PASS/1FAIL/1ignored in workspace-green.log. The sole runtime authorization-handshake failure exposed a real natural-owner-exit descendant regression from this worker's initial capture policy. Diagnosis and follow-up exact patches/hashes/tests are in pty-exit-retention-notes.md. Runtime guard/fixture is unchanged. Natural reap now retains exact shell-group member identities once before consuming zombie; all normal status-reap entrypoints route through the helper, avoiding full group scans on activepolls. Worker source is frozen for Root RED/GREEN and management peer review. No full-workspace success or correction GREEN is claimed.


## Waitid follow-up failure and final teardown gap correction

See `pty-waitid-gap-notes.md` and its exact manifest. The first waitid gate failed61PASS/5FAIL/1ignored/exit101; Root native proof qualified Darwin nonterminal notifications despite WEXITED and invalidated inspect(None) as proof of reap. Separate frozen tests/product patches preserve that failure evidence and add deterministic before-poll coverage. Worker ran only rustfmt/diff-check, no Cargo; Root actual verification pending.


## Final shared owned-child observer

Final frozen product is `pty-waitid-owned-final-product.patch`; exact manifest and final design/failed approaches/limits/remaining commands in `pty-waitid-owned-final-{sha256.json,notes.md}`. Supersedes previous gap-final draft: second-peek-only logic cannot qualify a Darwin exiting-but-not-yet-waitable child. Shared observer now gives fresh pending/exited unreaped-child proof for original-shell-group-only teardown fallback, rejects retired/deferred/errors and visible mismatch, and keeps periodic scans terminal-only. Root actual new before-poll RED0/2/exit101 preserved. Worker rustfmt/diff0, no Cargo; peer and Root verification pending.


## Actual final affected GREEN — Root executed, worker inspected

Root completed the exact final frozen shared-observer patch. `/private/tmp/deppy-cloud-fix-root-20261015/pty-followup-green-results.json` records both command exits0; worker inspected both completed logs directly.

- `cargo test --offline --locked -p pty -- --nocapture --test-threads=1`:68passed/0failed/1ignored; doctests0, log `pty-natural-exit-green.log`. All10 actual foreground/natural-exit cases and existing freeze/foreign/identity safeguards passed.
- `cargo test --offline --locked -p runtime pr13_exit_teardown_releases_authorization_before_revocation_handshake -- --nocapture --test-threads=1`:1passed/0failed/405filtered, unchanged authorization ordering test, log `runtime-exit-handshake-green.log`.
- Tests-only new before-poll RED command was `cargo test --offline --locked -p pty samegroup_descendant_before_reaper_poll -- --nocapture --test-threads=1`:0passed/2failed/exit101 before final product; exact result JSON `pty-before-reaper-red-result.json`.

Management peer confirmed exact final source/hash review clean for assigned lifecycle/ownership/performance invariants; no Cargo from peer. Root broader appcloud/workspace/clippy/fmt/boundary/dependency gates are running separately and are not claimed complete here. Worker has no code/test modifications after final source freeze. No app launch/restart, release/version bump, canonical docs/Cargo write or commit from worker.
