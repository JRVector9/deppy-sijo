# Cloud agent settings redesign implementation plan

> **For agentic workers:** Execute this plan inline, task by task, using the execute-plan workflow. Do not delegate unless requested. The user approved implementation on 2026-10-13.

**Goal:** Implement the approved HTML settings content, then compare the actual native rendering and interactions against it.

**Architecture:** CloudAgent keeps the existing auth, access, lease, run, history, and navigation controller. Its settings leaf owns four tab states, an internal body scroll area, and list/detail selection. The settings host lets this page own its fixed header/body/footer. Shared settings metrics remain the source of control sizes.

**Tech Stack:** Rust, egui 0.36.1, i18n Catalog, egui_kittest with real controller fixtures and offscreen native rendering.

**Reference:** `docs/mockups/cloud-agent-settings-redesign-2026-10-13.html`. Match its product settings content; the HTML report toolbar, fabricated provider data, and browser/OS chrome are demonstration scaffolding, not product UI.

## Task 1: Shared shell and navigation

Files: `crates/app/src/ui/settings.rs`, `crates/app/src/cloud_agent/ui.rs`, new `ui/layout.rs`, new `ui/redesign_tests.rs`, five `crates/i18n/locales/*/messages.txt`.

- [x] Add a failing real UI test: `get_by_role_and_label(Role::Tab, "Access permissions")`; click; assert the connection-add form is absent; go Advanced; assert permissions absent.
- [x] Execute `CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo redesign_ -- --nocapture`, verify behavior RED.
- [x] Expose shared settings typography/metrics for this leaf. Use 15px page title, 14px label/control, 13px helper, 34px controls, 68px rows, 26px horizontal padding, 8px action gap. Route CloudAgents outside the host's outer ScrollArea.
- [x] Implement fixed page header, 43px tabs, scrollable body and 35px inline feedback footer; use stable IDs and accessible native controls; narrow pane stacks list and details instead of clipping.

## Task 2: Connections, permissions, advanced settings

Files: `ui/settings.rs`, new `ui/connections.rs`, new `ui/permissions.rs`, new `ui/advanced.rs`.

- [x] Preserve real authentication evidence and separate local listener/public verification/manual-unverified/auth/tool states; display name, state and next action first. Empty/add inline guide, OAuth request inspect/approve/deny, rename and revoke selected ID only.
- [x] Test selected actor read/input consent, scope/expiry/closed-session disabled controls, read-off clears input/lease, input-on never reacquires a lease. Run existing controller fixtures with navigation updated for the new tabs.
- [x] Use 68px session rows with workspace/session, local state, read/input, actual lease owner/expiry. Reclaim a selected actor's consent without widening or borrowing another actor's grant.
- [x] Advanced uses the existing profile save/load/delete and reconnect setter, locks address/port while busy, retains start/stop/rotate/copy legacy token, ended sessions and the complete MCP instruction behind disclosures. Do not remove existing functionality.

## Task 3: Work, answers, receipts and visual verification

Files: `ui/runs.rs`, new `ui/history.rs`, existing `ui/management_tests.rs`, QA report and `docs/CODEX_HANDOFF.md`.

- [x] Add work/answers/receipt selection and compact list/detail rows, preserving bounded search, actual cancellation and observed navigation result. Results are redacted at render/copy time; raw identity is never used as a visible fallback.
- [x] Keep original identity scoped by connection and operation/run; notification selection activates answers and reveals once. Cancel is a request, not terminal completion. Copy is explicit and never repeats across frames.
- [x] Execute focused CloudAgent tests, i18n tests, affected settings tests, `cargo fmt --all --check`, scoped strict clippy and `cargo run --locked -p xtask -- check-boundary`. Record only actually executed results.
- [x] Render actual native desktop/narrow, dark/light, empty/approval/expired, permission/work/advanced fixtures. Inspect screenshots and measure control/row/padding bounds against the approved reference. Fix mismatches and rerun affected checks.
- [x] Review final source with `codex -c model='"gpt-6.1-sol"' -c model_reasoning_effort='"xhigh"' review --uncommitted`. Resolve findings, commit reviewed changes and docs; no push/restart/release without current authorization. No user artifact release in this task, so no version bump.

Actual results and 18 native images: `docs/qa/2026-10-13-cloud-settings-redesign.md`. No literal browser/native glyph raster equivalence is claimed.
