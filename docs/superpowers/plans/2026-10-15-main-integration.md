# Main Integration Implementation Plan

> **For agentic workers:** Execute sequentially in the isolated integration worktree; retain original dirty worktrees. User has authorized review, necessary fixes, main merge, and rebuild, with unnecessary work excluded. App restart is not authorized.

**Goal:** Integrate each useful outstanding change without regressing main, then produce a version-verified local build.

**Architecture:** Start with the performance branch, which contains origin/main. Copy its exact reviewed dirty snapshot. Classify remaining candidates individually; port only required changes rather than merging obsolete histories.

**Tech Stack:** Rust workspace, Cargo, native macOS bundle, shared PWA JavaScript, GitHub PRs.

- [x] Preserve baseline and copy tracked45/untracked361 changes with byte checks.
- [x] Review each of the 23 commits and current source diff; map regression tests.
- [x] Classify Draft PR191/192/193/194/195/197/198 individually by current implementation and unique missing behavior.
- [x] Inspect dependency PR190/206/207/217 for compatibility, necessity, and actual checks.
- [x] Review residual changes in all existing project worktrees; record include/exclude evidence.
- [x] Apply useful compatible changes only and run targeted regressions if implementation changes.
- [x] Choose version above all shipped bundles/branches; update workspace version, lockfile and CHANGELOG.
- [x] Run cargo test --offline --locked --workspace --no-fail-fast -- --test-threads=1 with CARGO_BUILD_JOBS=2; actual exit and counts required.
- [x] Run strict workspace Clippy, fmt, xtask boundary/deps, and relevant PWA checks.
- [ ] Commit coherent source/docs changes, push integration branch, open reviewable PR and merge after required gates.
- [ ] Build/package the merged main commit without launching; verify binary and both bundle versions.
- [ ] Record source commit, version, included/excluded candidates, executed test logs, artifact path and remaining limitations.
