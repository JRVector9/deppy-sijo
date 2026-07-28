# SF05 Session Storage Scan Budgets Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound session-log and scrollback-archive GC directory work to 4,096 entries without returning a partial successful GC result.

**Architecture:** Split session-log discovery from compaction/deletion so the complete bounded scan succeeds before mutation. Add an aggregate entry counter to recursive log discovery and the flat archive scan while preserving public GC signatures and existing byte/depth policies.

**Tech Stack:** Rust 2024, bounded filesystem traversal, existing log compaction/archive GC, temporary-directory tests.

---

### Task 1: Add failing scan-limit tests

**Files:**
- Test: `crates/storage/src/logs.rs:850`
- Test: `crates/storage/src/scrollback_archive.rs:298`

- [ ] **Step 1: Define limits and static error codes**

```rust
const SESSION_LOG_SCAN_ENTRY_LIMIT: usize = 4_096;
const ARCHIVE_SCAN_ENTRY_LIMIT: usize = 4_096;
const SESSION_LOG_SCAN_LIMIT_ERROR: &str = "session_log_scan_entry_limit";
const ARCHIVE_SCAN_LIMIT_ERROR: &str = "scrollback_archive_scan_entry_limit";
```

- [ ] **Step 2: Add test-only limit-injection helpers**

Production wrappers call 4,096; tests call small limits:

```rust
fn collect_session_log_bundles_with_limit(root: &Path, entry_limit: usize) -> anyhow::Result<Vec<SessionLogBundle>>;
fn collect_archives_with_limit(root: &Path, entry_limit: usize) -> anyhow::Result<Vec<ArchiveRecord>>;
```

Create exact-limit and limit+1 trees. Assert the latter returns the static error.

- [ ] **Step 3: Verify failure**

```bash
cargo test -p storage --locked scan_entry_limit -- --test-threads=1
```

Expected: FAIL because current traversal has no counter/helper.

### Task 2: Make session-log GC two-phase and bounded

**Files:**
- Modify: `crates/storage/src/logs.rs:603`
- Modify: `crates/storage/src/logs.rs:619`
- Modify: `crates/storage/src/logs.rs:657`

- [ ] **Step 1: Add immutable candidates**

Introduce:

```rust
struct SessionLogCandidate {
    dir: PathBuf,
    path: PathBuf,
    original_len: u64,
    modified: SystemTime,
    max_bytes: u64,
    retain_bytes: u64,
    tail_boundary: TailBoundary,
}
```

The bounded recursive scan only gathers candidates and counts every `read_dir` entry across the
whole operation. It must not compact or delete during discovery.

- [ ] **Step 2: Apply compaction only after scan success**

After discovery returns successfully, compact oversized candidates, re-read lengths, and build
`SessionLogBundle`s. Then run the existing oldest-first bundle deletion. If discovery returns the
limit error, `gc_session_logs` returns before any file mutation.

- [ ] **Step 3: Run log tests**

```bash
cargo test -p storage --locked logs -- --test-threads=1
```

Expected: exact-limit, limit+1, snapshot-growth, symlink/FIFO, and oldest-first tests pass.

### Task 3: Bound archive discovery

**Files:**
- Modify: `crates/storage/src/scrollback_archive.rs:243`

- [ ] **Step 1: Introduce a named archive record**

```rust
struct ArchiveRecord {
    modified: SystemTime,
    bytes: u64,
    path: PathBuf,
}
```

Use `collect_archives_with_limit`; count every root entry before checking for `scrollback.zlib`.
Return the static limit error at entry `limit + 1`. Keep `scan_total` fail-closed at 0 on error and
keep `gc` returning the error without deletion.

- [ ] **Step 2: Run archive tests**

```bash
cargo test -p storage --locked scrollback_archive -- --test-threads=1
```

Expected: roundtrip, corruption, budget GC, exact-limit, and limit+1 tests pass.

### Task 4: Validate and document SF05

**Files:**
- Create: `docs/build/PR-SF05-summary.md`

- [ ] **Step 1: Run required gates**

```bash
cargo test -p storage --locked logs -- --test-threads=1
cargo test -p storage --locked scrollback_archive -- --test-threads=1
cargo clippy -p storage --all-targets --locked -- -D warnings
git diff --check
```

- [ ] **Step 2: Record resource and rollback behavior**

Document aggregate entry count, no partial mutation on over-limit, unchanged public signatures,
and unchanged disk formats.

- [ ] **Step 3: Commit**

```bash
git add crates/storage/src/logs.rs crates/storage/src/scrollback_archive.rs docs/build/PR-SF05-summary.md
git commit -m "fix(storage): bound session log scans"
```
