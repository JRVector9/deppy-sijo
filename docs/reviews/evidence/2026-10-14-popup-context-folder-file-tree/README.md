# Popup/context/folder/file-tree review evidence

Reviewed HEAD: `d8826f0bdb8d89255f3448b5b61b6eee83faa87a` in `/Users/jr/Desktop/projects/deppy-sijo-performance`, including the pre-existing uncommitted wide-cursor change captured in `reviewed-working-tree.patch`.

Three reviewers used `gpt-6.1-sol` / `xhigh`. They did not change canonical product or tests. Root ran Cargo sequentially, verified source preservation, and independently reran the five standalone RED probes and one passing control. Review findings remain unfixed. No app launch, restart, user DB/clipboard/real-secret operation, release artifact delivery, commit, or push occurred.

## Existing checks actually executed

Run from the canonical repository. Each command exited 0:

```sh
CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo popup -- --nocapture
CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo context_menu -- --nocapture
CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo file_tree -- --nocapture
CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo workspace -- --nocapture
CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo picker -- --nocapture
CARGO_BUILD_JOBS=2 cargo test --locked -p connector-ui
```

Logs are named `existing-*.log`. App filters overlap. The connector log also contains an earlier targeted run duplicated by the full crate run. Deduplicate `(crate, test name)` from actual `test ... ok`/`ignored` log lines: 838 unique passing tests, 15 unique existing ignored tests. Full App suite was not run.

## Six scratch behavior regressions

The retained detached worktree is `/private/tmp/deppy-popup-review-worktree-20261014`. Its production source matches the reviewed working tree; only `folders-probes.patch` and `folders-escape.patch` add tests. They use private fixtures and actual production handlers. Trash failure is a controlled completion seam; permanent removal is the production native worker. FSEvents callback delivery, filesystem permission failure and recovery, inode replacement, partial native copying, and settings persistence are checked before the expected behavior assertion.

Actually executed from that worktree:

```sh
CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-performance/target CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo popup_audit_20261014_ -- --nocapture --test-threads=1
```

Compile succeeded; all six behavior assertions failed as expected on the defective code, exit101. `folders-scratch-red.log` preserves the compile and test results. Two delete regressions deduplicate to one finding; the partial-copy test deliberately has no active watcher and supports a conditional low finding.

To recreate in a new unused scratch path, apply the captured source diff first, then tests-only patches. Do not apply these review patches to the running app checkout as a fix:

```sh
git worktree add --detach /private/tmp/deppy-popup-review-repro-20261014 d8826f0bdb8d89255f3448b5b61b6eee83faa87a
git -C /private/tmp/deppy-popup-review-repro-20261014 apply /Users/jr/Desktop/projects/deppy-sijo-performance/docs/reviews/evidence/2026-10-14-popup-context-folder-file-tree/reviewed-working-tree.patch
git -C /private/tmp/deppy-popup-review-repro-20261014 apply /Users/jr/Desktop/projects/deppy-sijo-performance/docs/reviews/evidence/2026-10-14-popup-context-folder-file-tree/folders-probes.patch
git -C /private/tmp/deppy-popup-review-repro-20261014 apply /Users/jr/Desktop/projects/deppy-sijo-performance/docs/reviews/evidence/2026-10-14-popup-context-folder-file-tree/folders-escape.patch
CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-performance/target CARGO_BUILD_JOBS=2 cargo test --locked --manifest-path /private/tmp/deppy-popup-review-repro-20261014/Cargo.toml -p deppy-sijo --bin deppy-sijo popup_audit_20261014_ -- --nocapture --test-threads=1
```

The retained probe code and fixtures are for macOS/non-root execution; permission tests check actual PermissionDenied, and the watcher test verifies physical callback arrival. The reproduction commands above are instructions, not additional executed test claims.

## Standalone egui probes

`file-rename.rs` includes the actual production input mapper by absolute source path. Four menu/tree probes retain the relevant source-derived egui allocation/focus/drag flow; they are not full App GUI E2E. Their compiled binaries remain in `/private/tmp`:

```sh
/private/tmp/deppy-file-rename-probe-20261014
/private/tmp/deppy-tree-focus-probe-20261014
/private/tmp/deppy-file-rename-row-id-probe-20261014
/private/tmp/deppy-workspace-secondary-drag-probe-20261014
/private/tmp/deppy-popup-review-probes-20261014/connector_secret_undo
/private/tmp/deppy-context-close-probe-20261014
```

The first five commands were compiled successfully and reproduced expected behavior assertion failures, exit101; Root independently reran them with the same result. The last command is a production common context-menu Escape control, compile0/run0, disproving a suspected leak. `root-independent-reruns.json` records actual exits and output for all six. `menus-reviewer.md` records the source/gate mapping and exact rustc flags used for those egui probes. Dependency library hashes are environment-specific; rebuild against the matching current Cargo dependency cohort when they change.

The secret probe calls actual public `ConnectorUi::render` using egui_kittest, with fake fixture keys only. Actual rustc argv is recorded in `connector-secret-undo-build-command.json`; its empty compile log means successful compilation without diagnostics. The secret submitted in the second operation exactly matches the first operation's cancelled fake fixture. A production clear and empty-snapshot gap are included; this is not merely rendering the same draft twice.

## Preservation and limitations

Root captured `git diff --binary -- crates` before/after and executed `cmp`, exit0. `manifest.json` records the matching product diff SHA and copied evidence SHA256 values. Existing source changes and unrelated dirty documents remain preserved. Copied log/source text has only trailing whitespace normalized; original hashes are retained. Patch bytes are copied unchanged.

No running-app GUI E2E, real OAuth provider, real secret, user filesystem mutation, or full App suite was executed. Native workers and watcher were exercised against isolated fixtures. The remaining bugs need code changes and corresponding production composite regressions in a later implementation task.
