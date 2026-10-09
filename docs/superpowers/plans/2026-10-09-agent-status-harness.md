# Agent status harness implementation plan

> **For agentic workers:** Execute inline in the existing performance checkout. Root serializes Cargo and reviews each actual source unit with gpt-6.1-sol/xhigh. No app restart or artifact delivery is authorized.

**Goal:** Manage session-status contracts for Claude, Codex, Kimi, Grok and Cursor with replayable, provider-specific regression cases and a CI gate, while correcting verified recording defects.

**Architecture:** Keep the existing bounded hook reducer/AgentStateWorker as the storage authority; provider adapters translate identified events into its independent parent, child and pending-request facts. Replay sanitized JSON payloads through the real bounded reader, normalization, storage reduction and projection, and cover terminal/status-merger precedence separately. Explicit hook evidence, terminal observation, process exit and persisted history have different meaning; silence alone cannot prove successful completion.

**Tech stack:** Rust unit/integration tests, serde JSON fixtures, SQLite test databases, existing Cargo/xtask gates and GitHub Actions.

## Task 1 — Correct the three reproduced recording defects

Files: `crates/storage/src/agent_attention.rs`, `crates/storage/tests/background_agent_lifecycle.rs`, `crates/mcp-proxy/src/agent_attention.rs`.

- [x] Restore isolated storage tests and execute RED: first child approval, ungrouped question, reverse exit delivery.
- [x] Reserve child lifetimes only for accepted pending requests. Retain the maximum accepted live-child exit clock in the current parent activity episode.
- [x] Reproduce missing PreToolUse → permission fingerprint alias → actual result ID mismatch through the real hook pipeline. Resolve only an owned group with an exact input digest when the execution-start record is absent.
- [ ] Execute storage/proxy/App, strict Clippy, format and boundary checks; review source; correct verified findings; commit and journal one fix unit.

## Task 2 — Establish the provider/event contract inventory

Files: create `docs/qa/2026-10-09-agent-status-harness.md`; maintain this plan and handoff.

- [ ] Compare pinned cmux and Orca code/tests, official Claude and Cursor hook docs, OpenAI Codex hook runtime, Moonshot Kimi hooks and installed Grok contracts. Record links, pinned repository revisions, field ownership, termination semantics and uncertainty.
- [ ] Map cases: session start/end/resume, prompt/queued prompt, execution/long silence, question/approval/answer/denial, tool success/failure, success/error/interrupt, parallel children, missing/duplicate/delayed/reverse hooks, native/generation/session isolation, capacity/oversized data, restored/live state, terminal redraw and process exit.
- [ ] Distinguish publicly documented contracts, current installed contracts and captures. Do not copy third-party implementation or claim universal future-version/live-device coverage from fixture tests.

## Task 3 — Add replay harness and close adapter gaps

Files: create `crates/mcp-proxy/src/agent_status_harness.rs`, `crates/mcp-proxy/tests/fixtures/agent-status.json`; modify `crates/mcp-proxy/src/main.rs`, `hook_payload.rs`, `agent_attention.rs`, `crates/app/src/agent_shim.rs` and the existing bounded hook-config helper only as required.

- [ ] Define a test-only replay runner with fixtures containing provider/name and ordered steps `{at, payload, expected}`. Expected facts include working/waiting/response-required/done, pending IDs and idle boundaries; assertions inspect real DB projection after every step.
- [ ] Add cases before fixes. Run `cargo test -p mcp-proxy agent_status_harness` through the serialized gate and retain actual failures.
- [ ] Normalize documented Cursor conversation/generation IDs and events; distinguish completed stop from abort/error and own child IDs. Keep observer output permissive and bounded; do not infer a human approval from an unconditional before-tool event.
- [ ] Migrate Kimi hook installation from legacy flag updates to the observer for its supported events; preserve third-party TOML, idempotence, disabled-setting cleanup and fail-open behavior. Cover Kimi questions, permission results, interrupt/failure and optional modern lifecycle events without conflating agent names with stable child IDs.
- [ ] Verify Codex child hooks, Grok confirmed completion and optional background metadata against actual contracts; add only supported mappings with stale/ownership guards.
- [ ] Re-run the same fixture gate to GREEN and include malformed/unknown payload and cross-session isolation cases.

## Task 4 — Make status verification a maintained gate

Files: modify `.github/workflows/build-test.yml`; add `scripts/verify-agent-status.sh`; extend existing status/App tests as evidence requires; finish QA document and handoff.

- [ ] Provide one local command for storage lifecycle, provider replay, terminal status detector and App display precedence; fail on any regression and emit named cases. No app launch or live provider prompts.
- [ ] Add a named agent-status harness CI job using the same test scopes and retain ordinary full macOS tests.
- [ ] Run actual local gates, source-only Codex review and necessary corrections, then commit/journal the harness unit.
- [ ] Report verified coverage and material unverified/live-provider limitations. Source version stays0.8.7; a later delivered app requires a version increase and explicit restart authorization.

## Exact local commands

All local Cargo calls must use `/private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py`, workdir `/Users/jr/Desktop/projects/deppy-sijo-performance`, `RUST_TEST_THREADS=1`; freeze source during the gate. Storage: `test --offline --locked -q -p storage`; proxy: `test --offline --locked -q -p mcp-proxy`; App: `test --offline --locked -q -p deppy-sijo --bin deppy-sijo`; terminal status: `test --offline --locked -q -p session status::tests`. Finish with three-package all-target Clippy `-- -D warnings`, workspace `fmt --all --check`, `run --offline --locked -q -p xtask -- check-boundary` and `git diff --check`.
