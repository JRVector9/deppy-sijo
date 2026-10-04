# Centered reusable input windows and next-task settings Implementation Plan

> **For agentic workers:** Execute inline task-by-task. Do not delegate without a new user request. All independent CLI reviews use gpt-6.1-sol / xhigh.

**Goal:** Keep real current/recent Fleet task descriptions visible; apply the accepted centered/resizable popup proposal; show the actual model and expose verified reasoning selection in the follow-up form.

**Architecture:** Shared egui presentation moves to connector-ui so App and Connector forms use one palette/header/fields/footer/window geometry implementation without a reverse dependency. App retains its input ownership, operation state and runtime capabilities. Local Runtime.inspect_screen provides a one-shot bounded current screen through the same worker FIFO without snapshots, GUI rendering, remote leases or wire changes. Follow-up settings are scoped to the original session/execution/reservation and settle through guarded correlated input receipts before the reserved prompt.

**Scope:** The requested control is reasoning effort beside the actual model name. Model replacement is not added. Codex/Claude have verified live confirmation paths; Grok/Kimi keep existing settings and show a disabled chooser.

**Tech Stack:** Rust, egui0.36, existing egui_kittest, existing Cargo gate, local HTML/browser prototype. No native Deppy restart.

## Task1 — task card regression

Files: `crates/app/src/agent_transcript.rs`, `crates/app/src/ui/fleet.rs`, five i18n catalogs.

- [x] Reproduce Working + missing instruction with actual Claude/Codex parsers after300KiB tool output; both RED tests returned None while Working. UI RED also executed.
- [x] Recover latest instruction only from a bounded4MiB tail through the same open handle/EOF snapshot; preserve recent status/model semantics and existing App carry-forward. Display missing description truthfully rather than no work.
- [x] Add boundary/snapshot/race regressions, run focused green gate and independent code review.

Exact tests: gated `cargo test --offline --locked -q -p deppy-sijo agent_transcript::tests`; `ui::fleet::tests`; `carry_forward_agent_activity`; `cargo test -p i18n`. Expected zero failures; record existing ignored cases.

## Task2 — shared resizable popup geometry and presentation

Files: `crates/connector-ui/src/popup/{mod,shell,actions,fields,list,notice}.rs`, `crates/app/src/ui/popup/{mod,input}.rs`, affected callers.

- [x] Move presentation files once; App reexports the same public paths and retains App-specific confirmation/information/input behavior. Connector has no App dependency.
- [x] Implement `WindowSpec { id, title, subtitle, close_label, default_size, min_size }` and `window(ctx, spec, open, contents)`. Stable per-case/viewport geometry; center on each open, remember size, preserve movement thereafter, clamp to16pt screen gutters. Preserve Middle-layer IDs and front-window Esc ownership.
- [x] Reuse common header/frame, 18pt title, 13pt body,36pt inputs and34pt actions. Implement window-body scroll bound from actual available height and measured footer, with fixed header/footer.
- [x] Migrate18/19/20 and06/22/30/31/32, leave unrelated confirmation/approval semantics untouched. Preserve OS pickers, launchers and anchored menus.
- [x] Add offscreen center, reopen, drag/resize, small-viewport, footer reachability and menu/Esc/input-isolation tests. Avoid native app launch.

Commands: gated `cargo test -p connector-ui popup`; `cargo test -p deppy-sijo ui::popup`; `ui::fleet`; `ui::prompt_palette`; `ui::diff_panel`; `ui::agent_sessions`; connector tests. Expected no failures and no extra ignored tests.

## Task3 — follow-up effort selector beside actual model

Files: `crates/app/src/{fleet,followup_settings,app,pty_effort}.rs`, `crates/app/src/ui/fleet.rs`, five catalogs.

- [x] Project actual original provider/current settings to the form, use shared36pt drop-downs and known provider catalog; current value remains available. Selection defaults to keeping existing settings.
- [x] Default application at next-task start (optional user timing clarification pending; elapsed time is not permission but this is an already-authorized routine choice). Unsupported live transitions remain explicit and disabled rather than sending unverified commands. Codex upstream slash Model opens a picker; no argument support is present. Claude documents direct `/model <name>` and `/effort <level>`.
- [x] Keep settings with bounded reservation metadata. Apply one guarded tracked setting operation at a time after fresh original turn completion; verify the fresh native Codex screen or Claude operation-owned output before each next setting/prompt. Latch the observed fresh completion on the reservation and evaluate deadlines independently of waiting/consumed notifications. Reject/Unknown/cancel/replacement/retires retain draft or mark blocked; no blind retries, no auto agent restart and no hidden AI task.
- [x] Tests: same-session settings selection, empty/cancel, supported/unsupported values, current value, rejected/unknown, original identity replacement, budgets and prompt-after-confirmation ordering.

## Task4 — review and local release

- [x] Freeze source; execute actual independent `codex exec -m gpt-6.1-sol -c model_reasoning_effort=xhigh` source-only review; correct confirmed findings and rerun affected gates.
- [x] Strict affected all-target Clippy, fmt, UI capability/dependency boundary, relevant suites. No optional whole-workspace rerun unless required by extraction risk.
- [ ] Version0.6.1→0.7.0 (new feature), inherited workspace lock versions, fresh versioned macOS app+ZIP. Verify compiled version and both bundle version fields; do not execute Deppy.
- [ ] Local conventional commits, Obsidian project journal and continuous `docs/CODEX_HANDOFF.md`. No push or restart.
