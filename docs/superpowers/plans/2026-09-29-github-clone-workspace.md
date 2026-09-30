# GitHub Clone Workspace Implementation Plan

> **For agentic workers:** Implement inline with test-driven development. This task has already received user approval after the HTML proposal and local-clone behavior review. Preserve pre-existing uncommitted Fleet/file-tree changes in this worktree.

**Goal:** A workspace Add action opens a source dialog; a pasted GitHub HTTPS or SSH repository URL clones to a chosen local folder, then registers and opens that folder as a Deppy workspace without relaunching the app.

**Architecture:** `workspace_add.rs` owns URL/name validation, the clone operation, and the dialog view state. `git_cli.rs` supplies its bounded, cancellable Git process runner. `app.rs` owns a single clone task and reuses the existing folder-to-workspace settings worker for registration and switching. No cloud agent or remote SSH workspace is created.

**Tech Stack:** Rust, egui, existing bounded Git CLI helper, settings worker, i18n catalogs.

---

### Task 1: Clone request and operation

**Files:** `crates/app/src/workspace_add.rs`, `crates/app/src/main.rs`, `crates/app/src/git_cli.rs`.

- [x] Add failing focused tests: valid `https://github.com/owner/repo(.git)` and `git@github.com:owner/repo.git` yield one canonical owner/repo and default folder; credentials, query strings, extra path segments, traversal folder names, and non-GitHub hosts fail; a local bare fixture clones and `origin` is verified; the second request reuses that matching folder; a different existing folder is never overwritten.
- [x] Run `cargo test --locked -p deppy-sijo --bin deppy-sijo workspace_add::tests -- --test-threads=1` and capture RED before implementation.
- [x] Implement `CloneRequest::prepare(url, parent, folder)` and `clone_or_reuse(request, cancelled)` using `git -C <parent> clone -- <url> <temporary-folder>` with a bounded 5-minute Git runner; clean only the operation-owned temporary folder, then move to the final path after confirming it is absent. For a matching existing origin, return the existing path. Use `AtomicBool` to cancel and reap the Git child process group.
- [x] Rerun focused tests and capture GREEN.

### Task 2: Add dialog and app wiring

**Files:** `crates/app/src/workspace_add.rs`, `crates/app/src/app.rs`, `crates/app/src/ui/file_tree.rs`, five `crates/i18n/locales/*/messages.txt` files.

- [x] Add a failing UI test: Add opens a dialog with Local folder and GitHub repository choices; choosing GitHub exposes URL, parent, folder name, and Clone button; clicking local folder emits the existing folder-picker intent rather than opening Finder immediately.
- [x] Route titlebar/sidebar/settings Add actions to the same dialog. Keep existing direct folder-path recovery actions intact.
- [x] Launch one dedicated clone worker from the dialog; show busy, cancellation, and localized error states. On clone/reuse success, feed the path to the existing `FindOrCreateWorkspace` settings action. Close the dialog only after the workspace was registered and switched; preserve the path and show a retryable error if registration fails.
- [x] Run focused UI, workspace, and i18n tests.

### Task 3: Review, version, and build

**Files:** `Cargo.toml`, `Cargo.lock`, `docs/CODEX_HANDOFF.md`.

- [x] Review exact source diff for unsafe URL/path handling, process cleanup, and duplicate registrations; fix findings.
- [x] Run `cargo fmt --all -- --check`, `cargo clippy --locked -p deppy-sijo --bin deppy-sijo -- -D warnings`, focused tests, and `git diff --check`.
- [x] Increase feature version 0.3.0 to 0.4.0; update lock; release-build app and MCP proxy; stage a separate `target/bundle-0.4.0` with plist/binary version verification. Do not stop, launch, or replace the currently running Deppy app.
- [x] Update handoff with exact test results, modified files, remaining limitations, and next commands.
