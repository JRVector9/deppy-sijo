# Provider usage fixes and protection Implementation Plan

> Execute inline in the canonical integration tree, using test-driven-development. User has authorized implementation; no additional execution-choice question or subagent delegation is needed.

**Status:** Completed 2026-10-23. App3835 + proxy121 normal tests PASS; mandatory23 + guard5 PASS; live Claude/Grok collection and all strict checks PASS. See `docs/qa/2026-10-23-provider-usage-fixes.md`. No release/restart performed.

**Goal:** Correct the four verified Claude/Grok defects and prevent accidental edits to approved collection/display behavior with an executable protected contract.

**Architecture:** Preserve worker-only I/O and current status-bar geometry. Use a shared bounded OSC/CSI stripper, timestamp-aware Claude file/probe arbitration, section-bounded parsing and scoped Grok startup handling. Protect the final source and test inventory with a SHA256 manifest, a local verification command, CI and AGENTS policy; intentional authorized updates must explicitly reseal the contract after tests.

**Tech Stack:** Rust/egui/portable PTY, Python3 stdlib contract guard, existing GitHub Actions.

### 1. Reproduce collector failures

Files: `crates/app/src/{claude_usage.rs,grok_usage.rs}`. Add named `provider_usage_harness_*` tests for ST/BEL/mixed OSC, missing Claude session versus next weekly section, and real PTY fake Grok startup with the installed wording. Run `cargo test --offline --locked -p deppy-sijo --bin deppy-sijo provider_usage_harness -- --test-threads=4`; record actual behavioral RED before fixes.

### 2. Correct collection and arbitration

Files: new `crates/app/src/provider_usage_support.rs`, existing collector modules, `claude_usage_snapshot.rs`, `main.rs`, the focused status-bar selection block in `app.rs`.

- Replace duplicated greedy OSC regex with a linear bounded terminal-control scanner. Tests preserve text between consecutive ST/BEL controls, Unicode and incomplete frames.
- Stop Claude percent search at the next usage heading, and prefer the all-model weekly section over model-specific sections.
- Separate Claude refresh age (60seconds) from retained reading age (10minutes). File and CLI readings retain original measurement times. Drain pending CLI results even while reading a file, probe stale/partial files and choose the newer available value per window. Expired/reset values remain unknown, never0. Start no CLI while the initial file read is pending; fresh complete files avoid redundant launches.
- Recognize both old and current Grok trust questions only in its app-owned probe process; retain25second overall deadline and teardown. Do not extend timeouts or send input to a user's session.

Add failing arbitration tests through the actual production entry method, then implement and run GREEN. Include newer file, newer CLI, partial values, missing/future timestamps, pending reads and expired cache cases. Preserve existing provider visibility/order and real egui layout tests.

### 3. Protect the usage contract

Files: `scripts/verify-provider-usage.py`, `docs/testing/provider-usage-contract.json`, `docs/testing/provider-usage-harness.md`, `AGENTS.md`, `.github/workflows/build-test.yml`, UI harness names.

Create a manifest with full-file hashes for focused adapters/test fixtures plus marked hashes for usage-only regions in large app/UI files. The verifier checks source hashes and exact mandatory test names, runs App usage regressions and verifies no mandatory case is ignored. A mutation self-test operates only on a temporary directory and proves edits, missing regions and deleted/ignored tests fail. CI runs the hash guard in fast-checks and the executable harness in macOS build-test. AGENTS forbids unrelated edits or weakening/resealing without explicit user authorization.

### 4. Final checks and evidence

Run focused harness, full App tests, strict Clippy for touched packages/alltargets, fmt, boundary and diff checks. Run current CLI probes in separately owned PTYs with redacted results; do not restart GUI. Record exact source hashes, RED/GREEN results and remaining limits in `docs/qa/2026-10-23-provider-usage-fixes.md` and `docs/CODEX_HANDOFF.md`.

No release/package, commit/push or restart requested. Test builds remain0.18.0; any later changed delivered bundle must bump beyond0.18.0.
