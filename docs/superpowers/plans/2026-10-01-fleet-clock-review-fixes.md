# Fleet clock review fixes implementation plan

**Goal:** Fix all three independently reproduced Fleet clock bugs; rebuild without starting the app.

**Architecture:** Treat future wall-clock values as untrusted time while retaining structural bounded-read validation. Store completion and actual request-resolution boundaries independently from quiet observations. An additive runtime event reports only the accepted submission timestamp, using the existing bracketed-paste-aware input detector. Per-session UI state retains that scalar and rejects earlier idle completions even before Fleet is first opened; delayed input/status delivery must preserve a newer completion.

**Scope:** Existing reviewed recommendations are approved by the user’s “수정해”. Execute inline; preserve all prior dirty work. No unrelated popup changes, restart, commit or push. Source starts0.5.0, final patch release must exceed all local shipped versions.

## 1. Future idle time isolation

- [x] Add in-memory regression from the review: future completion on session7, valid waiting on session8, projection succeeds and excludes only future idle clock.
- [x] Run `cargo test --offline --locked -q -p storage fleet_review_fix` and capture RED.
- [x] In `crates/storage/src/db.rs`, keep preflight type/key/flag/size limits, remove future-time structural error, pass the same snapshot epoch to SELECT and omit future clocks. Normalize legacy idle generation to microseconds without changing legacy alert CAS generations.
- [x] Keep invalid type/negative-time tests; verify future-clock omission and legacy generation units. Run affected storage tests to GREEN.

## 2. Request-resolution time

- [x] Add exact100→120→150→delayed130 reproduction and two-request reverse-delivery case.
- [x] In `crates/storage/src/agent_attention.rs`, add defaulted completion timestamp and latest actual pending-resolution timestamp. Reset on new turns, record completed time once per completion, advance resolution watermark when pending requests decrease or a delayed question proves a matching earlier result; preserve newer child boundaries across delayed parent activity.
- [x] Start idle after max(completion,resolution), never IdleObserved. Unknown migrated completion stays unknown.
- [x] Run storage clock/reducer regressions to GREEN.

## 3. Unhooked input boundary

- [x] Add regressions for accepted submit, bracketed-paste text/no-submit, rejected input, delayed input after newer completion, first Fleet open after an unhooked submit, hidden/warm workspace retention and session pruning.
- [x] `crates/session/src/status.rs`: return whether on_user_input consumed an actual submit; preserve choice-screen and bracketed-paste behavior.
- [x] `crates/runtime/src/{event,in_process,remote}.rs`: append timestamp-only SessionInputSubmitted at the enum end; emit only after real input acceptance, no input bytes; update wire order guard and exhaustive test helper.
- [x] `crates/app/src/ui/workspace.rs`: keep timestamp inside existing SessionView in both active and hidden ingestion; no separate unbounded map.
- [x] `crates/app/src/ui/fleet.rs` and `app.rs`: invalidate older completion generations using submit boundary, keep newer completions even if events arrive late, and filter source before first Fleet render. Bare Running retains prior delayed-event protection.
- [x] Run targeted session/runtime/app regressions to GREEN.

## 4. Review, test and release

- [x] Review actual changed source with Codex CLI; address concrete findings and rerun affected checks.
- [x] Run full App/Storage/Session/Runtime tests, strict relevant Clippy, fmt and diff gates.
- [x] Bump canonical workspace version, regenerate inherited lock versions; build App and proxy in release mode. Create a separate versioned local signed bundle so the running old bundle stays untouched.
- [x] Verify binary version marker, both macOS plist versions and package signatures; update handoff and final review report with actual commands/results, source hash, version and remaining work.

## Final result

Completed0.5.1 signed local bundle/ZIP verification without launching Deppy. Core tests3299 passed,28 App tests ignored; final source rereview has no concrete unresolved finding. See `docs/reviews/2026-10-01-fleet-clock-review-fixes.md` for actual commands/results, ordering regressions and source hash.
