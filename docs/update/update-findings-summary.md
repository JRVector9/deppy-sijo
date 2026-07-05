# Update Findings Summary

작성일: 2026-07-05

기준 커밋: `b7bab02 Implement third build track wave`

## Purpose

This document is the PR-U00 baseline for the update-only hardening track. It maps
the completed Review Track findings and prior Build Track work to the PR-Uxx plan
in `ai_agent_workspace_v3_2_update_only_final_pr_plan.md`.

Review is complete. New work should use this file as the intake map, skip already
implemented items, and continue with the remaining Build/Hardening/Gate PRs only.

## Source Inputs

- `ai_agent_workspace_final_architecture_v2_6_FOLDER_TREE.md`
- `ai_agent_workspace_v2_8_persistence_store_improvement_FINAL.md`
- `ai_agent_workspace_v3_2_update_only_final_pr_plan.md`
- `docs/review/PR-R00-current-implementation-inventory.md`
- `docs/review/PR-R01-boundary-violation-findings.md`
- `docs/review/PR-R02-dependency-graph-store-cycle-findings.md`
- `docs/review/PR-R03-pane-mux-visibility-findings.md`
- `docs/review/PR-R04-folder-tree-dnd-findings.md`
- `docs/review/PR-R05-terminal-clipboard-paste-cjk-findings.md`
- `docs/review/PR-R06-redaction-secret-env-leak-findings.md`
- `docs/review/PR-R07-mcp-permission-audit-findings.md`
- `docs/review/PR-R08-resource-performance-findings.md`
- `docs/review/PR-R09-i18n-readiness-findings.md`
- `docs/review/review-summary.md`
- `docs/build/*-summary.md`
- `docs/build/build-track-orchestration.md`

## Non-Regression Baseline

- Pane-level workspace/session operation remains supported.
- Folder tree rendering remains enabled for valid workspace roots.
- Folder tree and sidebar path insertion into terminal must not auto-execute.
- Terminal selection/copy/paste and bracketed paste behavior must remain intact.
- Required CJK/emoji path fixtures must remain covered.
- UI leaf modules must not directly own secret-store operations.
- UI terminal actions continue through the `RuntimeClient` boundary.
- Hidden panes/workspaces must not create terminal viewport snapshots.
- Raw plaintext logs remain disabled by default.
- Secret/env/API key values must not be persisted in DB/config/log/export plain text.
- `mcp`/`audit`/`persist`/`storage` crate cycles must remain absent.

## Completed Build Work Mapped To PR-U

| PR-U | Status | Completed By | Notes |
|---|---|---|---|
| PR-U00 Findings Intake & Update Baseline | Complete in this PR | PR-U00 | This document freezes the update baseline. |
| PR-U01 Boundary Violation Fix | Complete | PR-B00 | Leaf UI secret-store direct calls removed; `xtask check-boundary` added with frozen allowlist. |
| PR-U02 Store Crate Cycle Fix | Mostly complete / policy skip | PR-B01b and v2.8 reality addendum | `mcp -> rusqlite` removed; `xtask check-deps` passes. Full store-crate explosion is intentionally deferred by v2.8 final addendum. |
| PR-U03 DB Migration & Store Repository Gate | Gate command complete | Current wave | `cargo run -p xtask -- smoke-db-migrations` added and passed. Store-specific repository coverage remains incremental. |
| PR-U04 Pane & Mux Resource Guard | Complete for high findings | PR-B02a | Hidden stale viewport and remote baselines pruned by visible session set. |
| PR-U05 Folder Tree Scalability & DnD Hardening | Partial complete | PR-B03a, PR-B03b | Shell quoting, empty root, async/chunk listing done. Ignore matcher and watcher storm handling pending under PR-U16. |
| PR-U06 Terminal Clipboard / Paste / DnD Hardening | Partial complete | PR-B04b | Shared paste bytes and bracketed path insertion done. Clipboard failure abstraction and terminal internal DnD contract remain backlog. |
| PR-U07 CJK / IME / Unicode Terminal QA | Complete for selection fixtures | PR-B04a, PR-B04b | Wide-char selection normalization and required paste/DnD fixture coverage added. IME smoke remains a release gate concern. |
| PR-U08 Redaction Pipeline Hardening | Complete for high findings | PR-B05a, PR-B05a review fix | Secret-like env/args persistence guards strengthened. Manual Debug redaction remains backlog. |
| PR-U09 Project Env Safety & Production Guard | Partial | PR-B05a | Plain secret-like env blocked. Production guard UX remains pending/backlog. |
| PR-U10 MCP Permission / Audit Hardening | Partial complete | PR-B06a | Audit encrypted blob default-off, JSON validation, debug redaction done. Schema reapproval cache and scoped env injection remain pending. |
| PR-U11 Final Security Gate | Gate command complete | Current wave | `cargo run -p xtask -- security-scan` added and passed against boundary, deps, secret persistence, audit/MCP/proxy tests. |
| PR-U12 Process Resource Monitor | Foundation complete / child tree pending | Current wave | Low-cadence app process CPU/RSS sampler and `RuntimeEvent::ResourceUsage` added; per-session child process tree and UI controls remain follow-up. |
| PR-U13 Workspace Auto Suspend | Pending | None | Not implemented. |
| PR-U14 Terminal Cache Budget Manager | Complete | Current wave | Visible/hidden/exited cache classes, byte/line budgets, trim events, and global exited archive pressure added. |
| PR-U15 Output Pipeline Backpressure | Partial complete | Current wave | In-process runtime command queue is bounded and overflow surfaces as an error. PTY input queue policy and visible backpressure badge remain follow-up. |
| PR-U16 File Watcher Debounce & Ignore Rules | Complete | Current wave | Default watcher ignores, debounce batching, `.env*` warning signal, and per-window invalidation cap added. |
| PR-U17 Status Detector Cost Control | Partial complete | Current wave | Regex-empty sessions skip screen-text scans; detector cost stats added. Confidence/user override remain deferred. |
| PR-U18 SQLite Write Batching | Foundation complete / runtime wiring pending | Current wave | Bounded/debounced `DbWriteWorker` and pending approval batch insert added; runtime/app hot-path rewiring remains follow-up. |
| PR-U19 Remote Slow Consumer Backpressure | Complete | PR-U19 | Remote outbound durable queue is bounded, viewport slots coalesce, and slow durable overflow disconnects the affected client. |
| PR-U20 Final Performance Gate | Smoke command complete / full scenario pending | Current wave | `cargo run -p xtask -- perf-smoke` added and passed; full RSS/CPU scenario report still pending. |
| PR-U21 I18n Infrastructure | Pending | None | Not implemented. |
| PR-U22 UI String Migration | Pending | None | Not implemented. |
| PR-U23 Runtime Message Localization | Pending | None | Not implemented. |
| PR-U24 I18n / CJK Layout Gate | Pending | None | Requires i18n-check and CJK layout evidence. |
| PR-U25 Global Activity View | Pending | None | Not implemented. |
| PR-U26 Terminal Dirty-Range Partial Render | Planned | `docs/build/PR-U26-terminal-dirty-range-partial-render.md` | Design document exists; implementation pending before PR-U20 if frame p95 requires it. |

## Release Blockers Remaining

- PR-U13, PR-U15, and full PR-U20 Scenario A-E measurements remain pending in Phase D.
- PR-U12 child process tree aggregation and PR-U18 runtime/app hot-path write wiring remain follow-up.
- PR-U21 through PR-U24 i18n infrastructure, message boundary, and layout gate are pending.
- PR-U26 dirty-range partial render is documented but not implemented.

## Deferred / Backlog Items

- Full store crate split beyond the v2.8 accepted DAG is deferred unless a new cycle appears.
- Clipboard failure UI abstraction and terminal internal selected-text DnD contract.
- Manual `Debug` redaction for all sensitive runtime/config rows.
- MCP proxy schema hash reapproval cache invalidation.
- Project production environment warning UX.
- Pseudo-locale visual smoke and generated default title localization.

## Next Operating Wave

1. Finish PR-U16 and PR-U18 because they are already isolated by file ownership.
2. Split remaining PR-U13 and PR-U15 carefully because both touch runtime worker state.
3. Run full PR-U20 Scenario A-E measurements after PR-U13/PR-U15 and PR-U26 land.
4. Start i18n only after the performance wave has stable runtime/event contracts.
5. Run final gates in order: PR-U11, PR-U20, PR-U24.
