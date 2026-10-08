# PWA Terminal Usability Implementation Plan

> **For agentic workers:** Use the available collaboration agents for subagent-driven development. The requested execution choice is already parallel delegation; unavailable superpowers aliases are replaced by the built-in collaboration tools. Steps use checkbox syntax for tracking.

**Goal:** Make the writable mobile PWA readable and usable as a terminal while keeping session identity, native geometry, and reading positions independent.

**Architecture:** Retain the cell snapshot/delta renderer and UUID-targeted authenticated WebSocket. Render at a measured fixed font unless overview is explicitly chosen. Keep direct input, resize ownership, and connection-local history as explicit server contracts, with read-only core rendering and writable shell controls.

**Tech Stack:** Rust runtime/web-remote, existing vanilla JavaScript/CSS PWA, isolated Chromium behavior fixtures; no new terminal library.

## Coordination and file ownership

| Owner | Exclusive files | Integration order |
| --- | --- | --- |
| pwa_renderer | web/shared/viewer-core.js and .css; new viewer-readability fixtures/runner/wrapper | PR1 fixed font, PR4 selection/reading/search |
| pwa_input_ui | crates/web-remote/assets/app.js, app.css, index.html; new mobile-usability fixtures/wrapper | PR1 settings/menu, PR2 direct input, PR3 ownership UI, PR4 reading controls |
| pwa_backend | necessary Rust in crates/web-remote, runtime, terminal, app, excluding browser wrappers | PR2 direct contract, PR3 resize lease, PR4 local history |
| root | this plan, audit progress, handoff, review outputs, staging and commits | review/test each unit before next dependent phase |

PR1 committed27186086, PR2 fcfbbfd1, PR3 00cb838a. PR4 implementation, reviews and final gates completed on 2026-10-09; its source unit is committed as `feat(pwa): PR4 독립 이력과 모바일 읽기 도구`. Resolve its exact SHA with `git log -1 --format='%H %s' --grep='PR4 독립 이력과 모바일 읽기 도구'`. Phase4 renderer developed ahead in isolated checkout /Users/jr/Desktop/projects/deppy-sijo-pwa-reading-20261008 atPR1; root imported its seven exclusively owned files afterPR3. Those files now belong to renderer in the main tasktree; the isolated source stays frozen and must not run Cargo or use the main target. Integration/commit order remains PR1→PR2→PR3→PR4.

All agents use gpt-6.1-sol with xhigh effort. Agents do not commit, restart Deppy, invoke review CLIs, or modify other owners' files. Root directly reviews code via Codex CLI, assigns corrections back to the owner, and commits reviewed units. Rust source is quiescent during each serialized Cargo run. UI JavaScript edits pause for web/App Cargo gates because assets are embedded via include_str; isolated renderer work can continue. No release artifact is delivered, so source version stays 0.8.7; any future delivery requires a version bump and artifact verification.

## PR1: Fixed font, local grid movement, settings and keyboard-safe menu

**Files:** web/shared/viewer-core.js, viewer-core.css; crates/web-remote/assets/app.js, app.css, index.html; new viewer-readability/mobile-usability tests.

- [x] Write and execute a RED fixture asserting a 180-column frame stays at 15px on 320/390/430px portrait and landscape, original grapheme owner-cell advances and cursor shape are preserved, and keyboard viewport offsets keep the menu visible.
- [x] Implement the shared read-only API below; persist font/overview in the core only and store local positions per session.

```js
viewer.settings(); // {fontSize: 15, overview: false}
viewer.setFontSize(15); // clamp to 12..24
viewer.setOverview(false);
viewer.getCellMetrics(); // {cellWidth, cellHeight, fontSize, stageWidth, stageHeight}
// hooks.layoutChanged(metrics) informs shell layout without sending write commands.
```

- [x] Run isolated behavior fixtures GREEN and existing viewer/grapheme/relay compatibility gates.
- [x] Root reviews only this source scope using codex exec -m gpt-6.1-sol -c model_reasoning_effort=xhigh -s read-only; fix material findings, update handoff, commit PR1 source and evidence.

## PR2: Direct terminal input with IME and explicit keys

**Files:** assets/app.js, app.css, index.html; web-remote/src/protocol.rs, dashboard.rs, ws_api.rs and focused backend tests; mobile-usability fixture.

- [x] Execute RED tests for confirmed Korean composition exactly once, paste once, Enter/Backspace/Tab/Esc/arrows/modifiers, correct watched UUID, disconnected rejection, and application cursor/bracketed paste behavior.
- [x] Implement a clear direct-input mode retaining the long-instruction composer and independent draft. Confirm exact wire fields with backend before using the following proposed contract.

```js
send({type: 'direct_input', session, text: '한글', paste: false});
send({type: 'direct_key', session, key: 'up', ctrl: false, alt: false, shift: false, meta: false});
```

- [x] Server authenticates/watch-gates target, bounds input, sanitizes free text controls, maps allowed keys to current terminal modes, and does not replay direct input after reconnect.
- [x] Run focused Rust and browser tests GREEN; root code review, corrections, handoff and PR2 commit.

## PR3: Single-owner actual PTY resize

**Files:** web-remote protocol/dashboard/ws_api, runtime resize handling and snapshots as needed; shell ownership UI and settled geometry.

- [x] Execute RED tests for one owner per session, competing sockets, native desired size restoration, disconnect/unwatch/workspace invalidation, rotation/keyboard changes, and no repeated identical resizes.
- [x] Implement an explicit lease with bounds and status; exact response fields are coordinated before UI integration.

```js
send({type: 'resize_control', session, action: 'acquire', request: 1});
send({type: 'resize', session, request: 1, cols: 40, rows: 24});
send({type: 'resize_control', session, action: 'release', request: 2});
// terminal_control echoes request, owned and reason; resize waits for owned=true.
// cols/rows derive from measured cells and available stage; debounce before send.
```

- [x] Native layout records desired dimensions while remote owns geometry; release restores native desire. Socket/session cleanup must release safely; request a keyframe after geometry changes. No ownership means display-only preserved host grid.
- [x] Run focused ownership/runtime/browser tests GREEN; root review, corrections, handoff and PR3 commit.

## PR4: Selection, reading mode, search and independent history

**Files:** core renderer/CSS; shell controls; web-remote viewport/history path and runtime/terminal read-only offset snapshot API.

- [x] Execute RED tests for copy/select/search, readable wrap without misleading TUI cursor, retained reading position under new output, independent offsets on two sockets and unchanged native scroll offset.
- [x] Add a DOM text/selection layer preserving owner-cell geometry in grid mode; readable wrap is opt-in. Keep history reads immutable and per connection. Scroll continues to use the existing session-targeted frame shape with local offset semantics.

```js
send({type: 'scroll', session, request: 1, delta: 20,
  anchor: {generation: displayedGeneration, first_line: displayedFirstLine}});
// While in flight, retain this displayed anchor and accumulate delta for newer requests.
send({type: 'scroll', session, request: 2, delta: 0, reset: true});
// Absolute live reset ignores any anchor. Only the matching reply changes the read window.
viewer.setReadableWrap(true); // reading mode; grid remains default
```

- [x] Run focused and full affected crate gates plus isolated browser viewport/input/history scenarios GREEN; review correction loop and PR4 commit.

## Final validation and delivery boundaries

- [x] Run fmt, strict affected all-target Clippy, runtime/terminal/web-remote/App tests as warranted and boundary checks via the serialized gate:

```sh
RUST_TEST_THREADS=1 python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["test","--offline","--locked","-q","-p","web-remote"],["fmt","--all","--","--check"]]'
```

- [x] Confirm git diff --check, PR-unit commit SHAs, audit acceptance progress and handoff exact next commands.
- [x] Record actual executed tests and remaining real iPhone/Android validation explicitly. No Deppy launch/restart, no release packaging or push in this task.

Final evidence: terminal130 (4 ignored), session76, runtime377, Web349 unit (1 ignored) plus20 default integration, App2809 (38 ignored), vendored terminal202 (1 ignored), and all7 explicitly executed Chrome wrappers passed. Browser assertions: reading180/history89/readability113/UI281 and sequence-limit216; runner failure regressions2 passed. Real authenticated two-WebSocket/temporary-PTY independent-history regression passed. Strict all-target Clippy for all5 affected packages, workspace fmt and boundary check passed in the final serialized gate. Native iPhone/Android installed-PWA testing remains unexecuted; optional Ghostty feature build remains unexecuted because Zig is unavailable. Full commands/results and intermediate failed gates are recorded in the handoff and audit.
