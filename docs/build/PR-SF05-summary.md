# PR-SF05 Session Storage Scan Budgets

## 1. Input Findings

- Session log GC recursively scanned directory entries without an aggregate entry cap.
- Scrollback archive GC scanned all root entries without an entry cap.
- Existing session log GC compacted oversized files during discovery, so a future scan-limit failure could otherwise leave partial mutations.

## 2. Scope

- Modified only `crates/storage/src/logs.rs` and `crates/storage/src/scrollback_archive.rs`.
- Added this build summary at `docs/build/PR-SF05-summary.md`.
- Did not change public GC signatures, Cargo manifests, lockfiles, disk formats, runtime call sites, or handoff docs.

## 3. Changes

- Added a 4,096 aggregate directory-entry scan limit for session log GC.
- Split session log GC into bounded immutable candidate discovery followed by compaction and oldest-first deletion only after scan success.
- Added a 4,096 root-entry scan limit for scrollback archive discovery.
- Added `ArchiveRecord` and retained existing oldest-first archive deletion behavior after successful discovery.
- Added static bounded error codes: `session_log_scan_entry_limit` and `scrollback_archive_scan_entry_limit`.

## 4. Tests

- `cargo test -p storage --locked scan_entry_limit -- --test-threads=1`
  - Red result: failed to compile with 10 missing constant/helper errors before implementation.
  - First green after implementation: 6 passed, 0 failed, 237 filtered out.
  - Final after adding production 4,096/4,097 boundary tests: 10 passed, 0 failed, 237 filtered out.
- `cargo test -p storage --locked logs -- --test-threads=1`
  - First run after implementation: 22 passed, 1 failed, 220 filtered out.
  - Failure: source-inspection assertion still expected `open_regular_log_file(&path, false)` after candidate refactor.
  - Correction: updated assertion to `open_regular_log_file(&candidate.path, false)`.
  - Final: 25 passed, 0 failed, 222 filtered out.
- `cargo test -p storage --locked scrollback_archive -- --test-threads=1`
  - First run after implementation: 9 passed, 0 failed, 234 filtered out.
  - Final after adding production boundary tests: 11 passed, 0 failed, 236 filtered out.
- `cargo clippy -p storage --all-targets --locked -- -D warnings`
  - First run failed: `gc_session_logs_with_limit` was dead code in the non-test target.
  - Correction: gated the injected-limit wrapper with `#[cfg(test)]`.
  - Final: passed.
- `git diff --check`
  - Passed with no whitespace errors.

## 5. Acceptance Criteria Check

- Exactly 4,096 entries: covered by production-boundary tests for log and archive scans.
- 4,097th entry: returns the static bounded error for log and archive scans.
- Recursive log scan cap is aggregate across the scan operation, not per directory.
- Over-limit scan returns before compaction/deletion; tests verify oversized log length and archive file presence are unchanged.
- Existing byte budgets, depth limit, symlink/non-regular handling, and oldest-first GC behavior remain covered by focused tests.

## 6. Regression Risks

- Directory entry order remains filesystem-defined, but ordering is not used for limit correctness or candidate mutation safety.
- Metadata can still change after a successful scan and before mutation; existing compaction/open safeguards remain the enforcement point.

## 7. Resource Impact

- Each session log GC scan now retains at most 4,096 discovered entries/candidates before mutation.
- Each archive GC scan now retains at most 4,096 archive records.
- Over-limit scans return an error instead of allocating or mutating beyond the cap.

## 8. Security Impact

- No raw plaintext log, secret, env, credential, or debug exposure was added.
- Existing path validation and non-regular file policies were not relaxed.

## 9. I18n/CJK Impact

- No user-facing strings or CJK rendering behavior changed.
- Existing Korean diagnostics and tests were preserved.

## 10. Rollback Plan

- Revert this commit to remove the scan entry caps and restore the previous single-phase discovery/mutation behavior.
- No schema, migration, public API, wire-format, or disk-format rollback is required.

## 11. Follow-up

- None for SF05. Integration evidence remains owned by PR-SF06.
