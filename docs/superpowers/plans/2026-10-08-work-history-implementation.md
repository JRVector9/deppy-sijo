# Work History Browser Implementation Plan

> **For agentic workers:** Execute inline in this session. User approved the existing HTML and requested implementation; no additional approval or subagent dispatch is needed.

**Goal:** Apply `docs/mockups/work-history-proposal-2026-10-08.html` to the actual history tab with accurate ownership and status.

**Architecture:** Keep storage and runtime operations in App. Extend the borrowed history view with pane/path/start/current-state data. A focused browser UI module owns compact date rows, filters, selected summary/conversation, narrow-view navigation and split resizing; all actions retain durable identities.

**Tech Stack:** Rust, egui/egui_kittest, existing bounded storage projection, five i18n catalogs.

### Task 1: Reliable projection and ordering
- [x] Add a regression in `ui/work_history.rs`: an old turn updated later must follow a newer started turn, including default sorting.
- [x] Run `cargo test --offline --locked -q -p deppy-sijo approved_history` through the serialized cargo gate and record actual RED.
- [x] Add `pane_id`, `cwd`, `occurred_at` and optional verified current state to the borrowed row. Sort RecentFirst with `occurred_at.unwrap_or(updated_at)`; default to RecentFirst.
- [x] In `app.rs`, only live exact pane/native binding plus newest owned turn can supply current Working/Waiting; old saved active states stay unknown. Completed remains a recorded terminal result.

### Task 2: Approved browser UI
- [x] Create `ui/work_history/browser.rs`, retain the existing card renderer as the optional card view and action/copy contracts.
- [x] Render title/refresh and search/scope/period/status/provider controls above the full-width body. Use compact date headers and 68pt rows; default selected row displays instruction/result with copy buttons on the right.
- [x] Render selected conversation via the existing transcript viewer, emitting ShowTranscript only when the selected identity changes or the conversation tab opens. Keep disabled reason tooltips and accurate current Git wording.
- [x] Keep list/detail scroll areas bounded to their allocated body. Below 760pt switch between list and detail with a back button; preserve draggable desktop split widths scoped to session.
- [x] Add i18n strings to ko/en/ja/zh-Hans/zh-Hant with matching parameters. Render collection ambiguity, read error, loading, empty/no-match and latest saved time distinctly, with the 256-row retention bound stated accurately.
- [x] Replace the old App split rendering with the browser call; maintain auxiliary search wiring and generation-scoped transcript completions.

### Task 3: Safe shared-transcript collection
- [x] Add tests proving unique exact pane prompt can attribute only its matching transcript turn, while duplicate prompts/duplicate matches/missing proof never cross panes.
- [x] Keep the historical shared-transcript guard. Allow only a unique exact displayable fresh or persisted pane-native prompt match, rejecting turns already attributed to a different pane. All other ambiguous turns remain skipped and are explained in the UI. Do not backfill by guessed ownership.
- [x] Keep attention updates restricted to non-shared bindings; display unverified shared state as unknown rather than silently marking it completed.

### Task 4: Verification and handoff
- [x] Run browser interaction/geometry tests for desktop and narrow views, selection/filter/reset, stable 30-frame height, copy/session/Git/conversation actions, unknown Working exclusion and exact ownership.
- [x] Render actual production egui UI offscreen with app palette/CJK font and inspect screenshots against the approved HTML. No live-app restart.
- [x] Run focused history tests, full App/i18n suites, strict all-target Clippy, fmt, boundary checks and git diff --check through the serial gate. Repair actual failures before reporting completion.
- [x] Update `docs/CODEX_HANDOFF.md` with files, decisions, commands/results, limitations and remaining work. This task is source-only; no bundle/release/version bump, restart, commit or push requested.

Validation: final4 gate exit0 `/private/tmp/deppy-history-final4-20261008.log`. Added actual RED→GREEN regressions for card re-click/selected conversation and scope-dependent card cache after the initial UI implementation. No release, restart or commit performed.
