# PR2a application wheel — implementation and evidence

Product commit: `3670eed3` on `fix/table-resize-claude-20261022`, isolated worktree `/private/tmp/deppy-table-claude-20261022`. Test-only cleanup `e176b10c` replaces four equivalent map assertions to satisfy strict Clippy. The preceding evidence-only commit `27c3f31e` documents clean manual-mode Claude table rendering in [the source-rendering report](2026-10-22-claude-table-source-rendering.md). Root owns native UI integration, shared handoff, and renderer launch policy.

The source investigation found that Claude classic native scrollback preserves old table borders at their original width, while Claude fullscreen app-owned history redraws an old table as the terminal width changes. The clean fixtures used explicit manual permission mode, denied external IP traffic, dummy authentication, and synthetic histories only. Actual fullscreen SGR wheel navigation preserved an unsent Korean draft. This supports adding an application navigation route. It does not justify forcing a renderer or overriding saved preferences; launch policy remains unchanged.

The appended `TerminalInput::Wheel { cols, rows, column, row, delta }` has no heap payload. Public validation requires bounded terminal dimensions, 1-based in-bounds coordinates, and a nonzero positive-up delta with magnitude at most 32. The backend also requires the displayed grid to match its live grid, alternate screen, SGR mouse encoding, and a negotiated mouse-reporting mode. Unsupported backends or modes return `None`; no legacy mouse or cursor-key guess is made.

The worker refreshes queued modes under the existing strict output budget and refuses input while unread output remains. Encoded wheel packets pass through the existing PTY admission and queue. An explicit internal `Navigation` record preserves draft/status state, prompt submission, completion intervention sequence, and accepted-input events. This distinction matters because raw SGR packets end in `M`, which the ordinary input detector can interpret as draft text. `RuntimeCommand::Scroll` remains a native view operation. Runtime wire version increases 35→36; existing Text/Key and Scroll bytes remain unchanged, and exact hello negotiation rejects old version 35 before decoding.

Modified product files are `crates/terminal/src/{backend,alacritty_backend}.rs`, `crates/session/src/session.rs`, and `crates/runtime/src/{terminal_input,command,in_process,protocol,remote}.rs`. No launcher, workspace UI, app release version, user configuration, or live session was changed by this agent.

Every Cargo command below ran with `CARGO_TARGET_DIR=/private/tmp/deppy-table-claude-target-20261022 CARGO_BUILD_JOBS=2` from the isolated worktree, offline and locked.

| Gate | Actual result | Log |
| --- | --- | --- |
| Terminal regression RED | exit101; 0 passed, 2 failed; safe-default backend returned no packets | `terminal-red.log` |
| Terminal regression GREEN | exit0; 2 passed | `terminal-green.log` |
| Corrected runtime regression RED | exit101; 4 passed, 2 failed; missing actual PTY wheel bytes and wire version 35 | `runtime-red-corrected.log` |
| Expanded runtime regression GREEN | exit0; 8 passed | `runtime-green-expanded.log` |
| Initial affected suites | exit101; runtime 427 passed, 1 failed, 1 ignored; stale exact-version assertion 35 | `affected-suites-initial.log` |
| Exact hello regression after correction | exit0; 1 passed; current 36 and old 35 rejection | `hello-green.log` |
| Final terminal/session/runtime suites | exit0; runtime 428 passed/1 ignored, session 82 passed, terminal 140 passed/4 ignored; doc targets 0 tests | `affected-suites-final.log` |
| Runtime focused after test-only cleanup | exit0; 8 passed | `runtime-final-focused.log` |
| Initial strict Clippy | exit101; four new test-only unnecessary_get_then_check errors | `clippy-initial.log` |
| Strict terminal/session/runtime all-target Clippy after cleanup | exit0 | `clippy-final.log` |
| Workspace formatting | exit0 | `fmt.log` |
| Final diff check | exit0 | `diff-check.log` |

The logs above are archived under [application-wheel-pr2a-20261022](assets/application-wheel-pr2a-20261022/manifest.json), with exact commands, exit codes, source hashes, raw-capture hashes, and normalized-log hashes. Full affected suites ran on `3670eed3`; focused runtime, strict lint, and formatting ran on the equivalent four-assertion cleanup committed as `e176b10c`. No full-suite rerun is claimed for that test-only cleanup. Do not add nested/filtered test counts to the full suite totals.

Actual `python3 docs/qa/assets/application-wheel-pr2a-20261022/verify-evidence.py` exited0 and verified13 archived log hashes, 8 pinned source hashes, and final suite/gate summaries. It does not rerun a provider or a Cargo gate.

The real PTY regressions prove the following:

- Queued application mode changes are parsed before the first wheel. Up/down events arrive as exactly repeated SGR buttons 64/65; stale displayed dimensions, invalid coordinates, and native Scroll do not add PTY bytes.
- A first wheel leaves an empty detector draft clean. Text `앞` → wheel → text `뒤` advances the intervention sequence 1→1→2, emits two accepted-input events and no submit event, preserves the status view across navigation, and retains the displayed Korean draft.
- A revoked input permit returns `AdmissionDenied` through the same Navigation admission path, leaves the queue idle with no accepted/submit event or sequence, and the child subsequently receives only ordinary `x` (`78` in hex).
- A full real PTY queue rejects the 10-byte wheel packet as `QueueFull` without prompt evidence. A 768KiB pending-output fixture drains exactly the existing guard budget, rejects navigation before any PTY write, and later receives only ordinary `x`.
- Terminal regressions cover mode on/off, main-screen refusal, supported reporting modes 1000/1002/1003, SGR disabled, stale grids, resize, invalid coordinates, zero/extreme deltas, and both maximum deltas. Session delegation does not consume native damage or alter cells, cursor, or native scroll position.

Failed approaches are retained honestly: the first runtime RED used a non-raw Rust shell string that interpreted `\033` incorrectly and produced a fixture spawn failure. That log is not the meaningful regression proof; the corrected raw-string RED above is. The first full runtime suite exposed an overlooked test-only version expectation; it was updated without changing production handshake behavior, and old version 35 was explicitly added to the rejection array.

Completed: final affected suites, strict lint, formatting, and evidence archive. Root integrates the frozen commits and independently validates native UI routing. The revoked-permit regression exercises the shared deep admission helper; local Wheel follows the same authority boundary as existing local direct Text/Key and does not add a new remote input producer. This agent makes no claim of actual native pointer selection/copy, app launch/restart, streaming provider execution, remote GUI coverage, or released artifact delivery.

Exact next commands:

```sh
git status --short
git show --stat 3670eed3
git show --stat e176b10c
cat docs/qa/assets/application-wheel-pr2a-20261022/manifest.json
python3 docs/qa/assets/application-wheel-pr2a-20261022/verify-evidence.py
git diff --check
```

The shared root handoff/plan are maintained by root. Do not run dev-run, stop the user's app, or open a replacement application as part of this work.
