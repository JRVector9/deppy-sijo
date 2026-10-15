# Popup, context menu, folder and file-tree fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Fix the ten confirmed findings in `docs/reviews/2026-10-14-popup-context-folder-file-tree-review.md` while preserving existing source changes, terminal input protection and filesystem identity boundaries.

**Architecture:** Three gpt-6.1-sol/xhigh workers use separate detached worktrees. Root owns canonical watcher/picker changes, source integration, Cargo execution, final review and handoff. Existing production host workers retain filesystem operations; UI only renders state and submits bounded intents.

**Tech Stack:** Rust workspace 0.12.1, egui/egui_kittest 0.36.1, notify 8.2.0/macOS FSEvents, SQLite and native filesystem fixtures.

The user explicitly chose parallel implementation. Execution starts immediately; no execution-choice confirmation is needed. The referenced superpowers delegation skill is not installed in this session; use available collaboration tools, the read systematic-debugging/TDD skills and existing repository contracts. No app restart, user-data mutation, release delivery or push is authorized in this task.

## Task 1: Delete proof and listing recovery — worker delete

**Modify:** `crates/app/src/ui/file_tree.rs` IO/confirmation/listing sections, `crates/app/src/app.rs` filesystem host worker, small helper only if needed.

**Worktree:** `/private/tmp/deppy-popup-fix-delete-20261014`.

- [x] Add the R1/R7/R10 production regressions from `docs/reviews/evidence/2026-10-14-popup-context-folder-file-tree/folders-probes.patch` and `folders-escape.patch`, excluding Root's App watch/picker tests.
- [x] Preserve the recorded actual behavioral RED for replaced inode, redirected parent, restored permission and partially applied copy. Add and execute approval→worker replacement regressions before implementation.
- [x] Carry original root/parent/object proof across failed trash→confirmation→permanent-delete; reject stale identity and root escape in the worker. Read existing filesystem helper contracts; never perform filesystem calls in rendering.
- [x] Permit manual recovery of inaccessible child directories without automatic busy retries. Refresh affected directories after partially applied mutation errors while preserving exact generation/operation matching.
- [x] Root runs `CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-performance/target CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo popup_audit_20261014_ -- --nocapture --test-threads=1` after canonical integration. RED was executed in the isolated worker tree; GREEN must preserve original files and reject replacement deletes; no expected behavior assertion may be weakened to accept deletion.
- [x] Export only owned source/test changes to `/private/tmp/deppy-popup-fix-delete-20261014.patch`, review target proof lifetime, symlink files, directory deletion and cleanup failures.

## Task 2: Tree/editor/terminal input ownership — worker input

**Modify:** `crates/app/src/ui/file_tree.rs` Rename/drag UI, `crates/app/src/ui/workspace.rs` input ownership, `crates/terminal/src/renderer_egui.rs`, App input routing if needed.

**Worktree:** `/private/tmp/deppy-popup-fix-input-20261014`.

- [x] Turn source-derived R2/R4/R5/R6 evidence into production composite regressions: Rename Enter/Escape yields no terminal input; clicked tree retains keyboard owner; watcher insertion preserves same-path edit; Secondary edge drag changes neither scroll nor order.
- [x] Send Root exact RED command and wait for actual behavioral failure. Existing evidence is in `file-rename.rs`, `tree-focus.rs`, `file-rename-row-id.rs` and `workspace-secondary-drag.rs` under the review evidence directory.
- [x] Reuse inline editor/native batch/IME ownership and closing-pass fences. Add stable path/edit-generation widget ID. Gate drag start and continuation on Primary.
- [x] Root executes requested narrow tests, then composite focus/input/IME regressions. Confirm terminal click restores input and popups/other TextEdit owners remain protected.
- [x] Export owned source/test changes to `/private/tmp/deppy-popup-fix-input-20261014.patch`. Leave IO/delete/listing sections to the delete worker.

## Task 3: OAuth secret undo lifecycle — worker secret

**Modify/Test:** `crates/connector-ui/src/lib.rs`, existing popup secret field helper or small private cleanup helper if required.

**Worktree:** `/private/tmp/deppy-popup-fix-secret-20261014`.

- [x] Add the production `ConnectorUi::render` regression from `connector-secret-undo.rs`: fake secret→Cancel→clear→empty snapshot→different server/operation→Cmd+Z→Save must not submit the old secret.
- [x] Root executes `CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-performance/target CARGO_BUILD_JOBS=2 cargo test --locked -p connector-ui oauth -- --nocapture`; record expected behavior RED before changes.
- [x] Reuse masked secret input and clear widget undo/state at cancel, success, operation/server/revision replacement and screen cleanup. Keep App dependencies out of connector-ui.
- [x] Root runs `cargo test --locked -p connector-ui`; retain provider-free synthetic fixtures and validate reopened fields accept new typing.
- [x] Export owned source/test changes to `/private/tmp/deppy-popup-fix-secret-20261014.patch`.

## Task 4: Watch namespace and failed folder selection — Root

**Modify/Test:** `crates/app/src/app.rs` SetProjectPath, AppFileTreeWatcher and watch worker/tests. Existing `folder_identity::probe` remains the identity authority.

- [x] Add the two App tests from `folders-probes.patch` to canonical source. Existing actual RED proves native physical callback filtered out and missing picker erasing saved anchor.
- [x] Strengthen tests to preserve path/anchor/volume on failed selection and check logical watcher event paths, ignore prefixes and stale generation behavior.
- [x] In the background watch worker capture canonical root, return it with logical root at exact completion, and translate callbacks to logical paths before existing filters. Do not canonicalize deleted files or perform FS work in UI snapshot processing.
- [x] Treat non-empty picker paths with failed identity probe as `ProjectPath` error before any DB mutation; intentional empty detach and valid directory persistence stay supported.
- [x] Execute `CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo popup_audit_20261014_ -- --nocapture --test-threads=1`, watch tests and settings path tests in canonical.

## Task 5: Integrate, review and document — Root

- [x] Review each worker diff before merging; use separate worktrees and a common original source to resolve overlapping files. Preserve pre-existing wide-cursor source changes and unrelated documents.
- [x] Run meaningful complete App tests, connector-ui/terminal tests as needed, strict affected-crate Clippy, fmt, boundary audit and diff checks. Exclude existing manually ignored cases from pass claims; record any genuine failure.
- [x] Recheck all ten findings against implementation and executed regressions; correct any remaining defects before completion.
- [x] Save QA/evidence and update `docs/CODEX_HANDOFF.md` after tests, design changes, failures and final integration.
- [x] Report completed source fixes and actual verification. No release/version bump or app launch occurs unless a later current-task request authorizes delivery/restart.

## Completion — 2026-10-14

All ten findings fixed and integrated. Full App3181/39ignored, connector35, terminal138/4ignored PASS. Strict3crate all-targets and UI boundary exit0. Lint-only followup popup87/10ignored plus existing refresh1 PASS. See [QA and evidence](../../qa/2026-10-14-popup-context-folder-fixes.md). No release/restart/commit/push.
