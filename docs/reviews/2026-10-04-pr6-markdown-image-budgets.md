# PR6 · Markdown PNG budgets and async reuse — 2026-10-04

## Scope and ownership

- Baseline: `85631a845d733b340e281df976522be4d4468959`, version `0.5.5`.
- Isolated branch: `fix/audit-pr6-image-budgets-20261004`, worktree `/private/tmp/deppy-audit-pr6-20261004`.
- Files: new `crates/app/src/markdown_image_io.rs`; `ui/markdown_viewer.rs`; minimal App field/construction/logic wiring in `app.rs` and module declaration in `main.rs`; this report.
- No Cargo/version/lock update, native App launch/stop/restart, user PTY interaction, user-file access, push, or shared handoff edit. Root owns integration, final review, release version and packaging.

## Completed implementation

### Validated descriptor and bounded decode

The worker canonicalizes relative `.png` references beneath the workspace. Unix opens each canonical path component below an owned root descriptor with `openat`, `O_NOFOLLOW`, directory flags and a nonblocking final open; a changed parent cannot turn the validated path into an outside read. Internal symlinks remain usable because canonicalization resolves them before descriptor traversal. Type and length come from the opened `File`; the same handle supplies a `take(limit + 1)` read and post-read metadata comparison. File growth is refused, not returned as an oversized byte vector.

PNG header dimensions, actual PNG decoding and decoder allocation limits are enforced off the UI thread. A custom egui image loader returns completed `Arc<ColorImage>` values; rendering does not read or decode PNG files. URLs and unapproved schemes do not acquire a file/network loader.

Windows uses existing `windows-sys` APIs to compare the opened root and file handles' final paths, and records volume/file-index/change-time identity. The APIs and structures are provided by the existing `Win32_Storage_FileSystem` feature; its feature chain activates `Win32_Storage`, `Win32`, and `Win32_Foundation`. No dependency or feature addition is needed. Actual Windows compilation/runtime was not executed.

### Explicit working-set budgets

| Resource | Limit |
|---|---|
| One encoded PNG | 8 MiB |
| One PNG dimension | 6,000 px |
| Legacy individual pixel ceiling | 16,000,000 px |
| Entire active document encoded data | 16 MiB |
| Entire active document decoded pixels | 8,000,000 px (32 MB RGBA) |
| Unique image references per request | 32 |
| Combined reference string bytes | 64 KiB |
| Combined root/base path bytes | 8 KiB |
| Worker lane | One outstanding request/result |
| Coalesced unsubmitted request | One latest request |
| PNG decoder allocation allowance | 64 MB |

Reused images spend encoded/pixel budgets too. The old displayed set and one new worker set may overlap; pending requests share the old set's Arcs. These limits describe logical image ownership, not total process RSS or GPU/driver allocation. References above the budgets keep the existing unavailable-image placeholder behavior. This PR loads the bounded reference set; it does not introduce an unrestricted offscreen image prefetch.

### Reuse and stale completion

- URI identity is stable per document slot and relative reference, independent of text revision. Worker file stamps include canonical target, length, modification time, and Unix device/inode/ctime (Windows volume/file-index/change time).
- Matching files reuse the exact encoded/decoded Arcs; their existing texture URI is never forgotten/recreated. Changed/removed resources are forgotten before replacement. Loader eviction can restore unchanged Arcs without decoding again.
- Visible previews schedule a two-second worker stamp refresh, so image replacement/deletion can update without editing Markdown. There is no per-frame filesystem probe. A worker held beyond the refresh deadline relies on completion wake, not a zero-delay repaint loop.
- Completion must match broker request token plus original slot/revision and the App's current active document slot/revision. New text revisions coalesce pending requests; old completions cannot register their resources. Closing clears loader bytes/decoded/texture entries and invalidates pending application; a late result is dropped.

## Tests and observed changes

Initial actual `pr6_` tests observed 0 passed / 3 failed (`/tmp/deppy-pr6-red-20261004.log`):

| Fixture | Before | After contract |
|---|---|---|
| Valid PNG grows after stat | 8,388,609 bytes returned | `TooLarge` refusal using the pinned, limited handle |
| Text-only revision | URI replaced | Same image Arc and same texture ID |
| 50 valid 1M-pixel images | 50 registered, admitting dimensions totaling50M pixels | Exactly8 registered and8M decoded pixels |

Additional tests cover 15MiB accepted / 20MiB refused aggregate encoded data including reused entries; same-revision atomic asset replacement; context-loader eviction; 100 coalesced source revisions and stale outcomes; wrong active slot; real gated worker and repeated UI rendering while I/O is blocked; close-before-completion; post-validation path replacement; and encoded/decoded Arc release on close. Existing escape/symlink/extension/PNG-dimension/link/scroll/forget regressions remain covered.

### Verification provenance

Early nongated runs reported Markdown31 then38 passes and a full App count of2,540 passed /30 ignored. The orchestrator found a shared-target executable race between worktrees: Cargo's build lock does not cover the complete test process. Those early broad counts are **not final source-specific proof**.

The first gate revision still reused another worktree's dep-info: PR6 list contained no PR6 tests and the viewer filter ran28baseline tests with another PR's `PromptReceipt` warning. Those outputs are verification failures, not PR6 passes.

The orchestrator corrected `/private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py`: it holds an `fcntl` lock, cleans workspace-package artifacts whenever worktree ownership changes, retains external dependency artifacts, and supports one locked batch across compile/list/execution/Clippy. Final PR6 validation used that corrected batch. Root will run a fresh integrated full suite.

Commands (from this worktree), executed as one corrected gated batch:

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[
 ["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","pr6_","--","--list"],
 ["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","pr6_","--","--test-threads=1"],
 ["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","markdown_viewer","--","--test-threads=1"],
 ["clippy","--offline","--locked","-q","-p","deppy-sijo","--all-targets","--","-D","warnings"]
]'
cargo fmt --all -- --check
git diff --check
```

Final gated results:

- Exact PR6 test list:11 unique `pr6_` functions present in the compiled binary.
- `pr6_`:11 passed /0 failed,4.52s.
- `markdown_viewer`:39 passed /0 failed,5.16s.
- Strict all-target Clippy: exit0.
- Formatter and diff whitespace gates: exit0.
- Consolidated fresh log: `/tmp/deppy-pr6-gated-fresh-batch-20261004.log`, including the acquired worktree, clean, exact commands/test names/counts. Prior nongated/first-gate logs are historical only.

### Failed approaches and limits

- The growth RED initially used equality on an 8MiB byte vector, producing a large fixture-only assertion log; the test now checks the error variant without dumping bytes.
- A path-replacement test first expected successful reading of the original descriptor. Rename changes its ctime on macOS, so the production implementation correctly returned `Changed`; the fixture now allows safe change refusal or original pinned bytes, never outside bytes.
- Strict Clippy first flagged a nested parser admission `if`; it was collapsed before the final gate.
- No live native GUI/RSS/GPU-memory/Windows-runtime measurement is claimed. The real headless UI/worker test demonstrates nonblocking progress and resource ownership. Final integrated source review and release packaging remain with root.

## Next agent

```sh
git diff 85631a845d733b340e281df976522be4d4468959..fix/audit-pr6-image-budgets-20261004 --stat
git show fix/audit-pr6-image-budgets-20261004 -- crates/app/src/markdown_image_io.rs crates/app/src/ui/markdown_viewer.rs crates/app/src/app.rs crates/app/src/main.rs
```

Apply the scoped commit sequentially, preserve other PRs' App wiring, rerun gated integrated package tests, independently review the new image ownership path, then update the release version/build/package without launch.
