# Fleet waiting time and scheduled task list

> Execute inline. User requested correction/display using existing cards. Optional clarification remains open; default is actual Deppy follow-up. No restart, no commit/push.

**Goal:** Show trustworthy waiting duration and readable next task on each Fleet card.
**Architecture:** Persist idle Unix seconds independently of notification generation; consume bounded worker projections. Observe existing runtime/hook events without rendering or new polling. Keep scheduled prompt execution unchanged and show its list preview inside measured cards.
**Tech:** Rust/SQLite/egui_kittest, existing renderer/signing scripts.

## 1. True waiting source

- [x] Storage tests: modern Completed at1,800,000,000,000,000µs must expose1,800,000,000s separately from revision; clear badge/reopen DB preserves it; newer Working/TurnStart or response wait removes it; last response resolution restarts it; IdleObserved alone must not invent completion.
- [x] Execute RED, then forward migration and event reducer idle_since; legacy Stop writes seconds, legacy other writes clear via INSERT replacement; keep alert generation/CAS semantics intact.
- [x] Bounded idle snapshot projection: independent of acknowledged turn_done, validate integer/key/type/bytes and count/cap; include retained-byte accounting and snapshot empty sections. App global_idle_since only includes live sessions, clears cache through workspace scope appropriately.

## 2. Observed clock lifecycle

- [x] Actual Fleet tests: delayed exact source replaces estimated first-seen clock; new turn while view hidden resets old clock; same-second completion identities and either runtime/projection delivery order are handled; missing hook shows observation label; workspaces/panes/exit remain isolated/pruned.
- [x] Extend episode clock with confirmed flag and independent completion generation and authoritative attention projection, observe existing runtime events at App drain points (active/warm), and invalidate on true hook turn starts. No per-frame filesystem/DB or extra polling.
- [x] Render exact and observed duration labels with all5 locale keys; keep existing1Hz visible-clock repaint only.

## 3. Scheduled task list

- [x] Actual card harness: queued prompt displayed as numbered1 item, long/multiline text wraps within measured bounds, no queued prompt shows localized empty state; full original accessible via hover and edit initial value; no change to followup_ready/scheduling/cancel.
- [x] Replace chip with heading/list preview, bounded visible text and measured galley height; keep current/last task2 rows, model line fully visible.

## 4. Verify/release

- [x] Full affected App/Storage/i18n suites and strict Clippy/fmt/diff/boundary; actual offscreen PNG visual check. Independent Codex CLI review; fix confirmed findings and rerun affected gates.
- [x] Document findings/test scope; update source and lock to new minor0.5.0 above shipped0.4.11, build App/proxy and stage separate signed bundle; verify compiled/plist/version/archive without launching. No physical user DB edits (live DB readonly only if necessary).
