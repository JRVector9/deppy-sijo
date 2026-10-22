# Product review fixes implementation plan

> **For agentic workers:** User selected parallel subagents. Root coordinates isolated source ownership, performs code review, serial Cargo, local commits and integration. Existing audit report is the approved design; do not request the same approval again.

**Goal:** Fix confirmed MAIN lifetime exhaustion, preserve approved settings/cloud source and make Command Center and SSH compile and behave safely together in a separate integration branch.

**Architecture:** Keep original branches and dirty trees unchanged. MAIN state is reclaimed only after authority revocation, active capacity and replay rejection remain enforced. WorkspaceRuntimeClient delegates local capabilities only for Local; Remote/Offline remain explicit fail-closed, with combined helper wire compatibility checked.

**Tech Stack:** Rust/Cargo, egui/eframe, existing OpenSSH stdio transport, native offscreen fixture and PWA browser harness.

## PR1 — MAIN registration lifecycle

Owner `/root/fix_main_lifecycle`, source `/private/tmp/deppy-main-lifecycle-fix-20261023`.
Files: `crates/app/src/command_center_main_tools.rs`, narrowly justified auth helper changes only if essential, `docs/qa/2026-10-23-main-lifecycle-fix.md`.

- [x] Apply existing failing lifecycle regression from SSH QA evidence. Previous actual RED was0P1F/exit101 after32retirements with active0 and slots32. Preserve bearer revocation assertions.
- [x] Review current prepare/reconcile/operation lifetime paths; fix obsolete entry retention without increasing/removing active capacity or permitting stale Arc/old credential reuse.
- [x] Add same-tuple repeated launch and operation/principal retirement checks if the same root cause affects128/256caps. Keep active capacity/replay tests.
- [x] Root runs `CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-sijo --bin deppy-sijo command_center_main_ -- --test-threads=1` with shared CC target cache. Review diff and test output before root commits.

## PR2 — Preserve approved settings/cloud changes

Owner `/root/fix_settings_preserve`, source `/private/tmp/deppy-settings-integrate-20261023`.
Read-only original `/Users/jr/Desktop/projects/deppy-sijo-ime-cloud-20261015`.
Files: actual scoped settings/cloud16existingfiles, App3line token disposal hook,5locales, new `cloud_agent/ui/one_screen.rs`, `one_screen_tests.rs`, `ui/settings/design_rule_tests.rs`.

- [x] Copy only approved product diffs and new3files into CC base. Merge App caller hunk rather than overwrite App file; retain CC0.15.1 workspace version and inherited lockfile.
- [x] Compare every product change with original source. Preserve token readout/copy/renewal and clearing on close/category transition. Do not import unrelated mockups/doc snapshots/version downgrade.
- [x] Add meaningful actual App transition regression if feasible; root runs `design_rule_` and `cloud_agent::` tests on the integrated source.
- [x] Root reviews exact manifest/diff and commits the scoped change independently.

## PR3 — SSH + Command Center compatibility

Owner `/root/fix_cc_ssh_integration`, source `/private/tmp/deppy-cc-ssh-integrate-20261023`.
Merge target SSH48db9bf4, also includes local-mainbc950e88. Product conflicts `app.rs`, corelib,5locales; handoff conflict is documentation.

- [x] Merge without committing, preserving both implementations. Do not replace files wholesale with ours/theirs.
- [x] Add fail-closed Local/Remote/Offline control capability tests; adapt CC launch/guard/control callers to explicit runtime ownership.
- [x] Root observes actual combined compilation and reports type errors to owner, then runs SSH actualApp/control/protocol/transport tests.
- [x] Preserve shared runtime wire36. Update exact source guards where31was valid only for pre-CC source. Review Linux helper build/seal/package freshness and old-wire refusal, without installing into user servers.
- [x] Root reviews/commits worker merge after checks; preserve parent history.

## PR4 — Final integration and regression

Owner root, branch `fix/product-review-integration-20261023` at `/Users/jr/Desktop/projects/deppy-sijo-integration-20261023`.

- [x] Integrate the reviewed worker commits, resolve only overlapping scoped hunks, preserve all three feature groups and local-main workspace-close fix.
- [x] Bring test-only MCP cleanup strengthening1a42fc41 if its diff remains relevant; validate exact bounded remainder regression. Preserve historical table evidence pinned to its tested source; create new final-source verification evidence instead of overwriting old hashes.
- [x] Run actual integrated App/core/runtime/SSH/cloud tests plus fmt/strict Clippy/boundaries/i18n and isolated browser. Expand checks when actual failures require it, not by repeating old-source PASS.
- [x] Review final source snapshot, record all commands/results/failures, root commits changes by PR scope. No main movement, remote push, app release or App restart is implied by this product-fix task.

## Release boundary and rollback

This is source integration plus test builds, no delivered app build. Existing original301refs and dirty source trees stay unchanged; rollback is selecting the original retained branch. A later delivered build requires app version greater than all shipped versions and binary/plist verification. Restart still requires a current explicit request.

Completed checkpoint: MAIN17195e51, settingsc777e160, SSH37d82b5c merged66f0aef1. MCP fixture92bbf634, SSH exact barrierafe60070, additional active-only MAIN guard9fd07d0d, byte-identical UI fixture relocationa1fc8f49, atomic PID fixture06db5a9e. Final frozen source1072files at06db5a9e: wholeworkspace6405PASS/0FAIL/76ignored (95harnesses;3nested child summaries excluded), strictalltargetsClippy/fmt/boundary/deps/i18nPASS; prior unchanged-source browser265/Python21/native1 gatesPASS. Final proof verifies1072source/34rawlogs/9gates/301originalrefs. Root reviewed all worker scopes and actual RED/GREEN before commits. Product source implementation complete. No main move, push, versioned release or App restart.
