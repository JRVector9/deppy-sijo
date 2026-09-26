# Cloud agent review fixes Implementation Plan

> **For agentic workers:** Execute inline, task by task. The user explicitly authorized all five reviewed fixes. Do not launch/restart Deppy. Use the existing workstep source-review/commit/journal procedure.

**Goal:** Confirm terminal input admission, display cloud answers in their original sessions, keep answer links valid, support OAuth-capable connectors and stabilize pressure verification.

**Architecture:** Append tracked input command/result variants without renumbering existing wire variants. CloudAgent holds bounded pending requests and completes durable receipts after the worker's PTY admission result. Existing workspace panes render session-bound answer records; notifications focus the persistent session. Retain the latest 100 answers independently of input audit entries. Add bounded OAuth authorization-code/PKCE discovery and local approval to the same MCP server; continue supporting manual Bearer clients.

**Tech Stack:** Rust, existing runtime queues/events, egui, SQLite, HTTP, SHA-256 and URL parsing.

## 1. Answer retention and pressure verification

Files: `crates/agent-mcp/src/history.rs`, `crates/agent-mcp/src/tests.rs`, `crates/runtime/src/in_process.rs`.

- [x] Add a regression that stores an answer, finishes 500 input actions and still finds its body in `recent()`; execute RED.
- [x] Retain latest 100 `notify` bodies independently of latest 500 audit records; `recent()` loads their bounded union. Validate eviction after 100 newer answers.
- [x] Reproduce the existing pressure test's command-admission failure; retry only explicit command-queue Backpressure in that test until its existing 15-second deadline. Never retry an accepted input.
- [x] Run `cargo test -p agent-mcp` and the focused runtime pressure test, review source, commit the unit.

## 2. Operation-correlated PTY admission

Files: `crates/runtime/src/{command,event,in_process,remote,lib}.rs`, `crates/app/src/cloud_agent.rs`, `crates/app/src/app.rs`; directly affected exhaustive matches/codec fixtures.

- [x] Append `WriteInputTracked {session, operation_id, bytes}` and `InputAdmitted {session, operation_id, result}`. Bound and validate operation IDs and payloads. Preserve existing wire discriminants and redact Debug output.
- [x] Test actual worker acceptance, PTY pressure/closure and missing session, with exactly one result per tracked command; execute RED before implementation.
- [x] Complete MCP receipts only after correlated worker result. Keep bounded pending requests and preserve unknown outcomes on timeout/disconnect; exact retries never retype an uncertain operation.
- [x] Exercise HTTP→CloudAgent→real runtime/PTY→output; verify persistent session routing and duplicate operations. Run focused runtime/App tests and source review, commit.

## 3. Original-session answer display

Files: `crates/app/src/cloud_agent.rs`, `crates/app/src/ui/workspace.rs`, `crates/app/src/app.rs`, `crates/app/src/ui/notifications.rs`, locale catalogs.

- [x] Project retained answers by persistent session UUID into existing WorkspaceUi panes; show readable answer text with copy/collapse controls, outside PTY stdin.
- [x] Test pane/session isolation and actual egui visibility, including repeated notification navigation and an unavailable/closed original pane.
- [x] Notification navigation focuses the matching original active/warm session and reveals the answer; fall back to retained Settings history when that session is unavailable.
- [x] Run focused UI/navigation tests and source review, commit.

## 4. Connector authentication compatibility

Files: new `crates/agent-mcp/src/oauth.rs`, agent-mcp server/auth and tests, App cloud-agent consent UI, HTTP response-header helper, locale catalogs and setup guide.

- [x] Read official MCP OAuth/PKCE/resource metadata requirements and verify the connector contract against primary xAI docs.
- [x] Add tests for protected-resource and authorization-server metadata, WWW-Authenticate, public-client registration, exact redirect/resource binding, mandatory S256 PKCE, owner approval/denial, one-use codes, refresh rotation, expiry/revocation and bounded state; execute RED.
- [x] Implement local approval in Deppy before issuing authorization codes; prohibit implicit approval, browser token exposure and unbounded registration. Authenticate access tokens against the current connection epoch and redact issued secrets.
- [x] Validate a standards-based local OAuth client→MCP→real runtime/output/answer roundtrip. Document actual supported flows and remaining user-account/public-tunnel validation separately.
- [x] Run source review, fix findings and commit. Actual Grok Bot certification requires the user's connector environment; do not claim it from local fixtures.

## 5. Final verification and handoff

- [x] Run affected regression suites, wire compatibility/boundary checks, formatting and `git diff --check`.
- [x] Final source-only codex CLI review, address actionable findings.
- [x] `cargo build -p deppy-sijo --release`; record binary location, results, commits and project journal. Do not launch the app.
- [x] Update the review report and `docs/CODEX_HANDOFF.md` with exactly what is fixed and any unverified external connection step.

## Execution notes

- Retention is a separate commit `c9d4fbb2`. Tracked input, pane projection and OAuth change a shared request/effect API and are reviewed/committed together to keep the integration buildable.
- Final source review, negative/real PTY tests and release build passed. Coordinated integration commit: `57c0bb80`.
- Additional Mac inherited socket nonblocking issue was reproduced and fixed with a fragmented HTTP regression. UI boundary uses a pure Answer view model.
- Public HTTPS / actual Grok Bot account validation remains external and unverified. The local OAuth fixture proves the supported flow, not every connector variant.
