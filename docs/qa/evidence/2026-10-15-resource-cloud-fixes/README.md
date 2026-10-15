# Resource/cloud fix evidence — 2026-10-15

[수정 보고서](../../2026-10-15-resource-cloud-fixes.md) · [계획](../../../superpowers/plans/2026-10-15-resource-cloud-fixes.md) · [원본 리뷰](../../../reviews/2026-10-15-resource-cloud-code-review.md)

Canonical source: `/Users/jr/Desktop/projects/deppy-sijo-performance`, HEAD `d8826f0bdb8d89255f3448b5b61b6eee83faa87a`, source/test0.12.1 + preserved dirty baseline. This is a source/test fix, not a released or restarted app.

## Authoritative final source

- [Final task delta](root-final-task-changes.patch) is relative to the dirty task-start bytes, not clean HEAD. It contains all Root integration and test-only follow-ups. Do not reapply worker patches to the already integrated tree.
- [Start source inventory](baseline-source-baseline.json) and [Final source inventory](root-final-source-snapshot.json): 422→423 files, existing10 changed, newPTY fixture1, unrelated412 byte-identical. Root Cargo.toml/lock are unchanged from task start.
- [manifest.json](manifest.json) pins stored and original SHA256 for every archived original. gzip records retain the decoded original SHA. The manifest does not hash itself; the generated README and verification result are separate final documents.
- Worker artifacts are intermediate deliveries, scoped design/syntax checks and read-only reviews. The final delta and source inventory above supersede them.

## Actual final results

- [Harness counts](root-final-test-counts.json): workspace **5732 PASS / 0 FAIL / 65 ignored**, 78 Cargo harness targets. Excluded 3 nested subprocess summaries. Earlier affected runs are shown separately and are never added to the workspace count.
- [Workspace log](root-workspace-green.log.gz); [Command/exits](root-integration-gates-results.json): one final workspace command exit0, then strict workspace/all-targets clippy, fmt, boundary and dependency checks each exit0.
- Earlier affected GREEN: secret80 + pty64/1ignored; Appcloud175/3ignored; Web412/13ignored. [secret/PTY](root-secret-pty-green.log), [App cloud](root-app-cloud-green.log), [Web](root-web-green.log).
- Final PTY follow-up:68PASS/1ignored plus unchanged runtime authorization handshake1PASS, both exit0. These affected runs are not added to workspace totals.
- [Independent rebuilt-library PTY probe](root-pty-production-green.log) + [Probe source/library identity](root-pty-production-identity.json): freshly linked canonical rlib, compile0/run0, all four qualified private cases terminate. This is separate from Cargo test totals.

Commands actually executed, canonical cwd with `CARGO_BUILD_JOBS=2`:

```sh
cargo test --offline --locked --workspace --no-fail-fast -- --test-threads=1
cargo clippy --offline --locked --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo run --offline --locked -p xtask -- check-boundary
cargo run --offline --locked -p xtask -- check-deps
```

## Actual RED and failed attempts

- [New safe Arc RED](root-root-projection-red.log): compiled, 0PASS/1FAIL before implementation.
- [New production PTY RED](root-pty-foreground-red.log): compiled, 2 controlsPASS/4 protectionFAIL before product changes; owned fixture rescue completed.
- [First workspace regression](root-workspace-green.log.attempt1.gz): actual exit101 in the unchanged runtime authorization/descendant handshake. The first PTY fix lost same-group cleanup after natural shell reap. [Qualified private fixture cleanup](root-runtime-failed-fixture-cleanup.log) records identity-scoped cleanup.
- [Initial shell-exit protections RED](root-pty-natural-exit-red.log): compiled, 0PASS/2FAIL before the follow-up. This initial fixture proved owner invisibility, which on Darwin is not proof of actual reap; the final fixture requires ECHILD. The unchanged runtime handshake above independently exposed the post-reap regression. The first zombie-query implementation still failed both ([Failed first follow-up](root-pty-natural-exit-green.log.attempt1)). [Native Darwin proof](root-macos-zombie-probe.log) demonstrates that proc_pidinfo/getsid reject an exited but unreaped owned child and WEXITED can still return CLD_STOPPED. The first waitid attempt also failed61PASS/5FAIL/1ignored ([Failed second follow-up](root-pty-natural-exit-green.log.attempt2)); stop notifications, teardown-before-reaper, and actual reaped qualification were corrected. The final implementation uses fresh owned-child WNOWAIT status under the child lock before retaining the shell-group snapshot. [Follow-up command results](root-pty-followup-green-results.json) and [Unchanged runtime handshake GREEN](root-runtime-exit-handshake-green.log) verify the repair. No raw-PGID signaling or new authority after reap.
- [Deterministic unreaped Drop/kill RED](root-pty-before-reaper-red.log): the periodic reaper was stopped/joined before the shell-exit command; matching terminal WNOWAIT qualified an unreaped owned child. Both tests failed before the teardown-entry observation was implemented.
- Prior actually executed cloud REDs remain in the [original evidence](../../../reviews/evidence/2026-10-15-resource-cloud-code-review/README.md): qualified metadata, stale shutdown/control, recovered SQLite bootstrap, slow valid auth and pointer-driven ended consent.
- [Missing test trait compile failure](root-app-cloud-green.log.attempt1): no behavioral test ran. Narrow NodeT import fixed it.
- [Under-qualified fixture result](root-app-cloud-green.log.attempt2): 174PASS/1FAIL/3ignored. Both initial bootstrap and stale read had been accepted. [Fixture correction](management-management-fixture-followup.md) completes empty bootstrap and observes initial callbacks before acquiring the lock; exact-one assertion retained. No product change in that correction.
- Optional gstack review checklist was missing, so that workflow was not completed. Explicitly authorized direct source reviews below were completed; no automated-review approval is claimed.

## Parallel ownership and peer review

- [Management design](management-management-design-and-test-scope.md); [PTY→management review](pty-management-cross-review.md).
- [PTY identity/platform design](pty-NOTES.md); Root also read the exact baseline delta and independently reran the rebuilt production probe.
- [Final natural-exit ownership design](pty-pty-waitid-owned-final-notes.md) and [Final management→PTY review](management-pty-exit-retention-peer-review.md); every normal reap and teardown entry was checked against unconsumed-child authority and PID reuse. [Superseded zombie-query approach](pty-pty-exit-retention-notes.md) and [Intermediate waitid approach](pty-pty-waitid-retention-notes.md) are preserved as failed/intermediate designs.
- [Transport/consent design](transport-report.md); [Management→transport review](management-transport-peer-review.md).
- [Transport→Root metadata/cache review](transport-root-metadata-cache-peer-review.md) pins the reviewed method files. Source review and behavioral testing are separate evidence.

## Performance scope

[Modified optimized projection measurements](root-projection-benchmark-green.log) + [Exact method/cache/redaction identity](root-projection-benchmark-identity.json), compared with [Original Root measurements](review-root-cloud_answers_optimized-independent.log).

100×16KiB/key32, same immutable source and corpus, ten warm calls: original mean110.782ms/14,902alloc/37,102,106 cumulative allocated bytes per call → modified **<0.001ms/0alloc/0bytes**. Three decimal places round the warm times to0.000; this does not claim literal zero time. Late-registration/wrapped-secret/routing and Arc reuse contracts PASS. Modified cold recomputation remains114.108ms for key32 and387.129ms for key128. Actual production components compiled optimized; not a full GUI/release/RSS/retained-heap measurement. The first changed projection holds the corpus mutex while rebuilding.

## Final verification and limits

[verification-result.json](verification-result.json) checks every artifact decoded/stored SHA,423 current sources, baseline preservation, local report links, diff whitespace and reverse-check of the exact task delta. Verification/archive/document-generation scripts are themselves archived to make the checks inspectable.

Actual macOS/private SQLite/loopback/egui fixtures cover these defects. Real provider accounts/public tunnels/mobile devices/OS sleep and Windows/Linux runtime are outside this fix validation. Frozen safeguards remain; normal teardown owns only executions captured before close, not future/escaped/background descendants. No real user keys/DB/clipboard/messages, release, app restart, commit or push were performed.
