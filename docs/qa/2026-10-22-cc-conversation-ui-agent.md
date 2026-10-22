# MAIN conversation and verification UI implementation

Worktree: `/private/tmp/deppy-cc-conversation-20261022`; base `1d5db6a7`.

Objective: render the exact bound main session's bounded user, agent, and tool records in the approved MAIN layout. Keep native and PWA projection equivalent. Add exact-result command verification controls after the core contract is finalized. Root owns host sourcing, admission, integration, and `docs/CODEX_HANDOFF.md`.

Approved decisions: reuse MAIN/CHECK/LIVE, worker monitor, and composer. Actual conversation entries have a stable ID, exact turn when known, true source generation, and optional host timestamp. Project summaries remain plan context. Missing or stale session ownership never selects another session. The host supplies at most 100 entries, 8 KiB text per entry and 128 KiB total; the render boundary repeats these limits.

Implementation plan:

- [x] Add native and real-Chrome regressions for actual user/agent/tool rendering and stale binding exclusion; execute and record RED.
- [x] Add the shared serde DTO and bounded projection helper in `ui/command_center/conversation.rs`, exposing `Project.main_conversation`.
- [x] Render the projection in existing MAIN thread in native and shared web view; keep plan/assignment context separate and preserve true order/time.
- [x] Run meaningful native/Chrome bounds and ownership regressions, then render offscreen images without starting the app.
- [ ] Coordinate PR2 exact verification DTO/intents with root and core/storage sibling, write regressions, implement evidence and explicit native execute/cancel controls.
- [ ] Commit tested groups and report exact source commits, commands, results, limitations and next integration commands.

No app start, stop, restart, release, version increase, provider call, remote push or merge is authorized in this agent phase.

## Results

- Native RED: `CARGO_TARGET_DIR=/private/tmp/deppy-cc-conversation-target-20261022 CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-sijo --bin deppy-sijo main_conversation_renders_bound -- --test-threads=1` executed on baseline behavior, exit 101, 0 passed / 1 failed. Failure was the missing actual user-message label, as intended. Fresh target compiled in 3m39s.
- Chrome RED: `node web/shared/tests/command-center.browser.mjs`, exit 1, expected `MAIN renders actual user, tool and agent records in host order` after one baseline check.
- Chrome GREEN before extra viewport checks: same command, exit 0, 250 checks across existing 390/760/1280/1440 and dark/light cases. Added checks cover actual text/order/tool command+output/state, true optional timestamps, generation/turn, exact route exclusions, latest 100, UTF-8/item/total byte limits, untrusted text, no emitted mutations.
- Native first GREEN attempt: 2 passed / 1 failed / 1 ignored; actual new content rendered correctly, final assertion was ambiguous because approved progress flow and new explicit plan context share `계획·배정`. Test now checks both matching labels; full Command Center suite is running.
- `cargo fmt --all` and `git diff --check` executed successfully after formatting.
- DTO source foundation: `5136e9f4`, `conversation.rs` and `mod.rs`; root has its hash. Root explicitly approved a mechanical `main_conversation: None` host initializer for standalone compilation, to be replaced by actual host sourcing during integration.

## Failed approaches

- An initial exploration requested `main_view.rs`; the real existing module is `main.rs`. No product edit resulted.
- Initial added Chrome regression used `cp/cq/cr` names already declared later in the large fixture; the syntax error caused a report timeout. Wrapped the regression block in its own scope, verified the actual expected behavior RED, then implemented. The timeout is not a product failure or valid RED proof.
- First CSS patch targeted a partial minified line and was rejected atomically. Reapplied the JS patch and appended scoped CSS; no partial edit was relied upon.
- Native first GREEN fixture queried a duplicate caption as unique; corrected the query without changing the rendered product.

## Remaining and next commands

Start with the failing native and browser regressions. Use `CARGO_TARGET_DIR=/private/tmp/deppy-cc-conversation-target-20261022 CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-sijo --bin deppy-sijo main_conversation -- --test-threads=1` and `node web/shared/tests/command-center.browser.mjs`.

### PR1 final source gate

- Full affected native gate: `CARGO_TARGET_DIR=/private/tmp/deppy-cc-conversation-target-20261022 CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-sijo --bin deppy-sijo command_center -- --test-threads=1`, exit 0, **149 passed / 0 failed / 2 ignored**, 11.18s.
- Shared real Chrome: `node web/shared/tests/command-center.browser.mjs`, exit 0, **252 checks**, including actual conversation geometry at 1440/390.
- Actual offscreen native evidence: `DEPPY_CC_CONVERSATION_EVIDENCE_DIR=$PWD/docs/qa/assets/cc-conversation-20261022 CARGO_TARGET_DIR=/private/tmp/deppy-cc-conversation-target-20261022 CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-sijo --bin deppy-sijo main_conversation_render_evidence -- --ignored --test-threads=1`, exit 0, **1 passed**, four desktop/mobile dark/light PNGs and exact snapshots. All four PNGs inspected: actual user/agent/tool records, true timestamp, plan context, composer and monitor fit the existing layout. These are synthetic bound-host fixtures, not the running user app or live provider evidence.
- Host initializer is only compilation plumbing and remains `None` here. Root must integrate the actual source before claiming PR1 feature complete.
