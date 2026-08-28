# Mobile Web Remote Full-Screen Viewer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the existing Tailscale-backed mobile session viewer a full-viewport, accessible application state with safe Back navigation, reconnect input locking, privacy protection, and width-and-height-aware terminal rendering.

**Architecture:** Move the viewer out of the dashboard `<main>` so the dashboard can become inert while a fixed sibling shell owns the visual viewport. Keep WebSocket protocol v3 and `viewer.watching` unchanged, centralize viewer entry/cleanup, project the existing socket state into a reconnect overlay and input gates, and coalesce viewport/canvas work through one animation-frame scheduler.

**Tech Stack:** Embedded HTML/CSS/vanilla JavaScript in Rust, Canvas 2D, WebSocket protocol v3, Visual Viewport API with window fallback, ResizeObserver with fallback, Rust unit/integration tests, Node syntax checking.

---

## Scope and file map

- Modify `crates/web-remote/src/static_srv.rs`: embedded shell regression contracts only; no routing or protocol behavior changes.
- Modify `crates/web-remote/assets/index.html`: dashboard/viewer sibling boundary and full-screen viewer structure.
- Modify `crates/web-remote/assets/app.css`: fixed visual viewport, safe areas, overlays, controls, and canvas letterboxing.
- Modify `crates/web-remote/assets/app.js`: lifecycle, history/focus, connection and input safety, privacy, and coalesced rendering.
- Modify `docs/CODEX_HANDOFF.md`: actual implementation, test, measurement, failure, and remaining Relay state.

Changing the three assets automatically changes the existing shell content hash. Server, Tailscale, pairing, repository, and WebSocket protocol production files stay out of scope.

### Task 1: Lock the dashboard/viewer DOM boundary

**Files:**
- Modify: `crates/web-remote/src/static_srv.rs:220-410`
- Modify: `crates/web-remote/assets/index.html:20-99`
- Modify: `crates/web-remote/assets/app.js:630-645`

- [ ] **Step 1: Write the failing sibling and accessibility contract**

Add this test to the existing `static_srv.rs` `tests` module:

```rust
#[test]
fn 앱셸은_대시보드와_전체화면_뷰어를_형제로_둔다() {
    let response = respond("/", &format!("token={TOKEN}"), TOKEN);
    let html = String::from_utf8(response.body.into_owned()).unwrap();
    let main_close = html.find("</main>").expect("dashboard </main> 없음");
    let viewer_open = html
        .find(r#"<section id="viewer""#)
        .expect("viewer section 없음");
    assert!(viewer_open > main_close, "viewer는 inert dashboard 뒤의 형제여야 함");
    for marker in [
        r#"<main id="dashboard-shell" class="app">"#,
        r#"class="viewer-shell""#,
        r#"role="dialog""#,
        r#"aria-modal="true""#,
        r#"aria-labelledby="viewer-title viewer-session""#,
        r#"id="viewer-back""#,
        r#"id="viewer-stage""#,
        r#"id="viewer-connection-overlay""#,
        r#"id="viewer-privacy-curtain""#,
        r#"role="group" aria-label="터미널 특수키""#,
        r#"aria-label="터미널에 보낼 메시지""#,
    ] {
        assert!(html.contains(marker), "전체화면 viewer marker 누락: {marker}");
    }
    let js = std::str::from_utf8(APP_JS).unwrap();
    assert!(js.contains("'viewer-back'"), "새 back listener 누락");
    assert!(!js.contains("'viewer-close'"), "삭제한 close listener가 남음");
}
```

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```bash
cargo test -p web-remote --locked 앱셸은_대시보드와_전체화면_뷰어를_형제로_둔다 -- --nocapture
```

Expected: FAIL because the current viewer is inside `<main>` and the new markers are absent.

- [ ] **Step 3: Move the viewer into a sibling shell**

Change the dashboard opening and session heading to:

```html
<main id="dashboard-shell" class="app">
<h2 id="sessions-title" tabindex="-1">세션</h2>
```

Only add the id to the existing main and the id/tabindex to its existing session heading; do not duplicate the heading or remove the intervening dashboard sections.

Remove the old inline viewer section, close `main` after the sessions panel, and insert this complete sibling before the script:

```html
<section id="viewer" class="viewer-shell" role="dialog" aria-modal="true"
         aria-labelledby="viewer-title viewer-session" hidden>
  <header class="viewer-header">
    <button id="viewer-back" class="viewer-back" type="button"
            aria-label="세션 목록으로 돌아가기">‹</button>
    <div class="viewer-heading">
      <h2 id="viewer-title">터미널</h2>
      <p id="viewer-session" class="viewer-session">세션</p>
    </div>
    <p id="viewer-connection" class="viewer-connection" aria-hidden="true">연결 중</p>
  </header>
  <div id="viewer-stage" class="viewer-stage">
    <div class="viewer-wrap"><canvas id="viewer-canvas" aria-label="원격 터미널 화면"></canvas></div>
    <div id="viewer-scroll-note" class="viewer-scroll-note" hidden>
      <span id="viewer-offset-text"></span>
      <button id="viewer-bottom" type="button">맨 아래로</button>
    </div>
    <div id="viewer-connection-overlay" class="viewer-overlay" role="status"
         aria-live="polite" hidden>
      <strong id="viewer-overlay-title">연결 중</strong>
      <span id="viewer-overlay-detail">터미널 화면을 준비하고 있습니다.</span>
    </div>
    <div id="viewer-privacy-curtain" class="viewer-privacy-curtain"
         aria-hidden="true" hidden><span>화면이 보호되었습니다</span></div>
  </div>
  <footer class="viewer-controls">
    <div class="viewer-keys" role="group" aria-label="터미널 특수키">
      <button type="button" data-key="esc">Esc</button>
      <button type="button" data-key="tab">Tab</button>
      <button type="button" data-key="up">↑</button>
      <button type="button" data-key="down">↓</button>
      <button type="button" data-key="left">←</button>
      <button type="button" data-key="right">→</button>
      <button type="button" data-key="ctrl_c">Ctrl‑C</button>
      <button type="button" data-key="ctrl_d">Ctrl‑D</button>
      <button type="button" data-key="enter">Enter</button>
    </div>
    <div class="composer">
      <input id="composer-file" type="file" hidden
             accept="image/png,image/jpeg,image/gif,image/webp,image/heic,.pdf,.doc,.docx,.xls,.xlsx,.ppt,.pptx,.txt,.csv,.md">
      <button id="composer-attach" type="button" class="composer-attach" aria-label="파일 첨부">📎</button>
      <textarea id="composer-text" rows="1" placeholder="메시지 입력…"
                aria-label="터미널에 보낼 메시지"
                autocapitalize="off" autocorrect="off" spellcheck="false"></textarea>
      <button id="composer-send" type="button">전송</button>
    </div>
    <p id="composer-note" class="composer-note" hidden></p>
  </footer>
</section>
```

Do not retain `#viewer-close`; `#viewer-back` is the single explicit exit control.

In the same step, keep the current `closeViewer` lifecycle temporarily but move its listener to the new button so the shell remains runnable before Task 3 replaces the lifecycle:

```javascript
document.getElementById('viewer-back').addEventListener('click', closeViewer);
```

- [ ] **Step 4: Run the focused test and verify GREEN**

Run the Step 2 command again. Expected: the matching test PASS.

- [ ] **Step 5: Commit the DOM boundary**

```bash
git add crates/web-remote/src/static_srv.rs crates/web-remote/assets/index.html crates/web-remote/assets/app.js
git commit -m "feat(web-remote): separate full-screen viewer shell"
```

### Task 2: Implement the fixed visual viewport layout

**Files:**
- Modify: `crates/web-remote/src/static_srv.rs:220-430`
- Modify: `crates/web-remote/assets/app.css:120-220`

- [ ] **Step 1: Write the failing CSS contract**

```rust
#[test]
fn 전체화면_뷰어_css는_viewport와_safe_area_계약을_포함한다() {
    let css = std::str::from_utf8(APP_CSS).unwrap();
    for marker in [
        "body.viewer-open",
        ".viewer-shell",
        "position: fixed",
        "left: var(--viewer-left, 0px)",
        "width: var(--viewer-width, 100vw)",
        "height: var(--viewer-height, 100dvh)",
        "env(safe-area-inset-top)",
        "env(safe-area-inset-bottom)",
        "max(12px, env(safe-area-inset-right))",
        "max(12px, env(safe-area-inset-left))",
        "max(10px, env(safe-area-inset-right))",
        "max(10px, env(safe-area-inset-left))",
        "grid-template-rows: auto minmax(0, 1fr) auto",
        ".viewer-overlay",
        ".viewer-privacy-curtain",
    ] {
        assert!(css.contains(marker), "전체화면 CSS marker 누락: {marker}");
    }
}
```

- [ ] **Step 2: Run the focused test and verify RED**

```bash
cargo test -p web-remote --locked 전체화면_뷰어_css는_viewport와_safe_area_계약을_포함한다 -- --nocapture
```

Expected: FAIL at the first new full-screen marker.

- [ ] **Step 3: Replace inline viewer/action styles with the shell boundary**

Keep dashboard styles and existing key/composer colors. Replace the old viewer/action block and add:

```css
body.viewer-open { overflow: hidden; overscroll-behavior: none; }
.viewer-shell[hidden] { display: none; }
.viewer-shell {
  --viewer-top: 0px;
  --viewer-left: 0px;
  --viewer-width: 100vw;
  --viewer-height: 100dvh;
  position: fixed;
  left: var(--viewer-left, 0px);
  top: var(--viewer-top);
  width: var(--viewer-width, 100vw);
  height: var(--viewer-height, 100dvh);
  z-index: 1000;
  display: grid;
  grid-template-rows: auto minmax(0, 1fr) auto;
  min-width: 0;
  overflow: hidden;
  background: #101010;
  color: #d4d4d4;
}
.viewer-header {
  display: grid;
  grid-template-columns: 44px minmax(0, 1fr) auto;
  align-items: center;
  gap: 10px;
  min-height: calc(54px + env(safe-area-inset-top));
  padding: env(safe-area-inset-top) max(12px, env(safe-area-inset-right)) 8px
    max(12px, env(safe-area-inset-left));
  border-bottom: 1px solid #303030;
  background: #1b1b1b;
}
.viewer-back {
  width: 44px; height: 44px; border: 0; border-radius: 9px;
  background: #2b2b2b; color: #f0f0f0; font-size: 30px;
  line-height: 1; cursor: pointer;
}
.viewer-heading { min-width: 0; }
.viewer-heading h2 { font-size: 13px; font-weight: 600; color: #9a9a9a; }
.viewer-session {
  overflow: hidden; color: #f0f0f0; font-size: 15px; font-weight: 600;
  text-overflow: ellipsis; white-space: nowrap;
}
.viewer-connection { color: #79c48a; font-size: 12px; white-space: nowrap; }
.viewer-connection.reconnecting, .viewer-connection.paused { color: #e0b85d; }
.viewer-stage { position: relative; min-width: 0; min-height: 0; overflow: hidden; background: #000; }
.viewer-wrap {
  width: 100%; height: 100%; display: flex; align-items: center;
  justify-content: center; overflow: hidden; background: #000;
}
.viewer-wrap canvas { display: block; flex: none; touch-action: none; }
.viewer-overlay, .viewer-privacy-curtain {
  position: absolute; inset: 0; z-index: 3; display: flex;
  flex-direction: column; align-items: center; justify-content: center;
  gap: 8px; padding: 24px; text-align: center; background: rgba(8, 8, 8, 0.82);
}
.viewer-overlay[hidden], .viewer-privacy-curtain[hidden] { display: none; }
.viewer-overlay span { color: #a8a8a8; font-size: 13px; }
.viewer-privacy-curtain { z-index: 4; background: #111; color: #c8c8c8; }
.viewer-scroll-note {
  position: absolute; inset-inline: 10px; top: 8px; z-index: 2;
  display: flex; align-items: center; justify-content: space-between;
  gap: 10px; padding: 6px 10px; background: #23303f;
  border: 1px solid #34506a; border-radius: 8px; color: #cfe0ef; font-size: 12px;
}
.viewer-scroll-note[hidden] { display: none; }
.viewer-scroll-note button {
  min-height: 44px; padding: 0 12px; border-radius: 7px;
  background: #2c3e50; color: #a8c8e8; border: 1px solid #3a5068;
  font-size: 12px; font-weight: 600; cursor: pointer;
}
.viewer-controls {
  min-width: 0;
  padding: 8px max(10px, env(safe-area-inset-right))
    calc(8px + env(safe-area-inset-bottom)) max(10px, env(safe-area-inset-left));
  border-top: 1px solid #303030; background: #1b1b1b;
}
.viewer-keys {
  display: flex; gap: 6px; margin-top: 0; overflow-x: auto;
  -webkit-overflow-scrolling: touch; padding-bottom: 2px;
}
.composer { display: flex; gap: 8px; align-items: flex-end; margin-top: 8px; }
.composer textarea {
  flex: 1; min-height: 44px; max-height: 130px; resize: none;
  padding: 10px 12px; border-radius: 9px; line-height: 22px;
  background: #1b1b1b; color: #e4e4e4; border: 1px solid #454545;
  font-family: inherit; font-size: 16px;
}
```

Remove `.viewer-actions` and `.viewer-close`. Replace the later existing `.viewer-keys`, `.composer`, and `.composer textarea` blocks with the complete versions above instead of adding earlier duplicates; leave their button, focus, disabled, attachment, and note rules in place.

- [ ] **Step 4: Run the focused test and verify GREEN**

Run the Step 2 command again. Expected: the matching test PASS.

- [ ] **Step 5: Commit the viewport shell styles**

```bash
git add crates/web-remote/src/static_srv.rs crates/web-remote/assets/app.css
git commit -m "feat(web-remote): fill the mobile visual viewport"
```

### Task 3: Centralize lifecycle, browser Back, focus, and session disappearance

**Files:**
- Modify: `crates/web-remote/src/static_srv.rs:220-450`
- Modify: `crates/web-remote/assets/app.js:70-280,630-925`

- [ ] **Step 1: Write the failing lifecycle source contract**

```rust
#[test]
fn 전체화면_뷰어_js는_단일_lifecycle과_back_계약을_포함한다() {
    let js = std::str::from_utf8(APP_JS).unwrap();
    for marker in [
        "function activateViewerShell()",
        "function finishCloseViewer(",
        "function requestCloseViewer(",
        "history.pushState",
        "window.addEventListener('popstate'",
        "dashboardShell.inert = true",
        "dashboardShell.inert = false",
        "viewBtn.dataset.sessionId = s.id",
        "선택한 세션이 종료되었습니다",
        "function clearViewerCanvas()",
        "function clearStaleViewerHistory()",
        "viewer.pendingClose = options",
        "setViewerClosing(true)",
        "function setViewerClosing(",
        "function stopAllKeyRepeats()",
        "if (viewer.closing) return false;",
        "if (!openViewer(id",
        "queueMicrotask(() => consumePendingWatch(lastSessions))",
        "if (pendingWatch === viewer.watching)",
        "if (!target || target.exited)",
        "if (viewer.watching !== endedSession) return;",
        "queueMicrotask(() => {",
        "if (s.id && !s.exited)",
        "else if (!s.id) {",
        "if (known && !known.exited) {",
        "if (!row || row.exited) return;",
        "let pointerActive = false;",
        "repeated = repeated || pointerActive;",
    ] {
        assert!(js.contains(marker), "viewer lifecycle marker 누락: {marker}");
    }
    assert!(!js.contains("scrollIntoView"), "inline viewer 스크롤 진입이 남아 있음");
}
```

- [ ] **Step 2: Run the focused test and verify RED**

```bash
cargo test -p web-remote --locked 전체화면_뷰어_js는_단일_lifecycle과_back_계약을_포함한다 -- --nocapture
```

Expected: FAIL at `activateViewerShell`; the old source still contains `scrollIntoView`.

- [ ] **Step 3: Replace the viewer references and entry/cleanup functions**

```javascript
const dashboardShell = document.getElementById('dashboard-shell');
const sessionsTitle = document.getElementById('sessions-title');

function clearStaleViewerHistory() {
  if (!(history.state && history.state.deppyViewer)) return;
  const cleanState = { ...history.state };
  delete cleanState.deppyViewer;
  history.replaceState(Object.keys(cleanState).length ? cleanState : null, '', location.href);
}

clearStaleViewerHistory();

const viewer = {
  el: document.getElementById('viewer'),
  label: document.getElementById('viewer-session'),
  canvas: document.getElementById('viewer-canvas'),
  wrap: document.querySelector('#viewer .viewer-wrap'),
  back: document.getElementById('viewer-back'),
  keys: Array.from(document.querySelectorAll('.viewer-keys button')),
  watching: null,
  returnSession: null,
  screen: null,
  closing: false,
  pendingClose: null,
};

const keyRepeatCancels = [];

function stopAllKeyRepeats() {
  for (const cancel of keyRepeatCancels) cancel();
}

function setViewerClosing(closing) {
  viewer.closing = closing;
  viewer.back.disabled = closing;
  inputBlocked = closing;
  for (const button of viewer.keys) button.disabled = closing;
  updateComposerEnabled();
}

function clearViewerCanvas() {
  const canvas = viewer.canvas;
  const context = canvas.getContext('2d');
  context.setTransform(1, 0, 0, 1, 0, 0);
  context.fillStyle = '#000000';
  context.fillRect(0, 0, canvas.width, canvas.height);
}

function activateViewerShell() {
  dashboardShell.inert = true;
  dashboardShell.setAttribute('aria-hidden', 'true');
  document.body.classList.add('viewer-open');
  viewer.el.hidden = false;
}

function restoreViewerFocus(sessionId) {
  queueMicrotask(() => {
    const button = Array.from(document.querySelectorAll('.view-btn'))
      .find((item) => item.dataset.sessionId === sessionId);
    (button || sessionsTitle).focus();
  });
}

function finishCloseViewer({ rerender = true, notice = '' } = {}) {
  if (!viewer.watching) return;
  setViewerClosing(true);
  stopAllKeyRepeats();
  const returnSession = viewer.returnSession;
  send({ type: 'unwatch' });
  viewer.watching = null;
  viewer.returnSession = null;
  viewer.screen = null;
  viewer.pendingClose = null;
  resetScroll();
  updateScrollNote();
  inputBlocked = false;
  setComposerNote('');
  viewer.el.hidden = true;
  document.body.classList.remove('viewer-open');
  dashboardShell.inert = false;
  dashboardShell.removeAttribute('aria-hidden');
  if (rerender) renderWorkspaces(lastWorkspaces, lastResource);
  if (notice) showNotice(notice);
  setViewerClosing(false);
  restoreViewerFocus(returnSession);
  queueMicrotask(() => consumePendingWatch(lastSessions));
}

function requestCloseViewer(options = {}) {
  if (!viewer.watching || viewer.closing) return;
  const ownsHistory = !!(history.state && history.state.deppyViewer);
  if (ownsHistory) {
    setViewerClosing(true);
    viewer.pendingClose = options;
    stopAllKeyRepeats();
    resetScroll();
    history.back();
    return;
  }
  finishCloseViewer(options);
}

function openViewer(sessionId, title) {
  if (!sessionId || viewer.closing || viewer.watching === sessionId) return false;
  stopAllKeyRepeats();
  setViewerClosing(false);
  viewer.watching = sessionId;
  viewer.returnSession = sessionId;
  viewer.screen = null;
  clearViewerCanvas();
  resetScroll();
  updateScrollNote();
  inputBlocked = false;
  setComposerNote('');
  viewer.label.textContent = title || '세션';
  activateViewerShell();
  if (!(history.state && history.state.deppyViewer)) {
    history.pushState({ ...(history.state || {}), deppyViewer: true }, '', location.href);
  }
  updateComposerEnabled();
  viewer.back.focus();
  send({ type: 'watch', session: sessionId });
  return true;
}

viewer.back.addEventListener('click', () => requestCloseViewer());
window.addEventListener('popstate', () => {
  if (viewer.watching && !(history.state && history.state.deppyViewer)) {
    finishCloseViewer(viewer.pendingClose || {});
  } else if (!viewer.watching && history.state && history.state.deppyViewer) {
    clearStaleViewerHistory();
  }
});
```

Remove the old `closeViewer`, `scrollIntoView`, and `#viewer-back` temporary listener from Task 1; the new listener above is authoritative.

- [ ] **Step 4: Register repeat cancellation, preserve pending deep links, and reconcile sessions**

In the existing special-key loop, move `let repeated = false` before `stopRepeat` and register the exact cancellation used by close and later reconnect handling:

```javascript
let repeated = false;
let pointerActive = false;
const stopRepeat = () => {
  clearTimeout(repeatTimer);
  clearInterval(repeatInterval);
  repeatTimer = null;
  repeatInterval = null;
};
keyRepeatCancels.push(() => {
  repeated = repeated || pointerActive;
  pointerActive = false;
  stopRepeat();
});
```

Remove the later duplicate `let repeated = false`. Set `pointerActive = true` at the start of `startRepeat`. Replace pointer termination listeners with:

```javascript
btn.addEventListener('pointerup', () => {
  pointerActive = false;
  stopRepeat();
});
for (const eventName of ['pointerleave', 'pointercancel']) {
  btn.addEventListener(eventName, () => {
    pointerActive = false;
    stopRepeat();
    repeated = false;
  });
}
```

A global session/close cancellation while the pointer is down sets `repeated=true`; if a prior repeat already fired, the OR assignment preserves that suppression during an intervening session transition. The trailing click is therefore consumed by the existing click handler instead of reaching a new session. Pointer leave/cancel clears suppression because those paths do not produce a valid click.

Change `consumePendingWatch` so closing never consumes the pending id and a rejected open leaves it queued:

```javascript
function consumePendingWatch(sessions) {
  if (!pendingWatch) return false;
  if (Date.now() > pendingWatchDeadline) {
    pendingWatch = null;
    return false;
  }
  if (viewer.closing) return false;
  if (pendingWatch === viewer.watching) {
    pendingWatch = null;
    return true;
  }
  const target = sessions.find((session) => session.id === pendingWatch);
  if (!target || target.exited) {
    if (target && target.exited) pendingWatch = null;
    return false;
  }
  const id = pendingWatch;
  if (!openViewer(id, target.title || ('세션 ' + id))) return false;
  pendingWatch = null;
  return true;
}
```

When creating a view button, add:

```javascript
viewBtn.dataset.sessionId = s.id;
```

Immediately after `lastSessions` is computed in `renderWorkspaces`, defer disappearance cleanup until the current render has completed. Capture the watched id and guard the microtask so a newer viewer cannot be closed by the stale task:

```javascript
const endedSession = viewer.watching;
const watched = endedSession
  ? lastSessions.find((session) => session.id === endedSession)
  : null;
if (endedSession && (!watched || watched.exited)) {
  queueMicrotask(() => {
    if (viewer.watching !== endedSession) return;
    requestCloseViewer({
      rerender: true,
      notice: '선택한 세션이 종료되었습니다 — 세션 목록으로 돌아왔습니다.',
    });
  });
}
```

Only active sessions may expose the view action:

```javascript
if (s.id && !s.exited) {
  // view button
} else if (!s.id) {
  // inactive workspace note
}
```

An exited session remains visible with its status badge but cannot open, receive restored focus, trigger an open-then-close flash, or fall through to the inactive-workspace `대기`/`절전` note.

Apply the same active-session invariant to the approval card's contextual `화면 보기` action. The card can outlive a dashboard update, so gate both initial rendering and click-time execution:

```javascript
if (known && !known.exited) {
  // create contextual view button
  view.addEventListener('click', () => {
    const row = lastSessions.find((session) => session.id === item.session);
    if (!row || row.exited) return;
    openViewer(item.session, row.title || item.session_title || '세션');
  });
}
```

This prevents a stale approval DOM node from opening a black viewer after the final dashboard frame for a disappeared session.

Because owned-history cleanup is deferred until `popstate`, the current dashboard render finishes first and cleanup then requests one fresh render with cleared `viewer.watching`. The lifecycle notice intentionally runs after that frame and takes priority over a same-frame server notice.

- [ ] **Step 5: Run lifecycle and static tests**

```bash
cargo test -p web-remote --locked 전체화면_뷰어_js는_단일_lifecycle과_back_계약을_포함한다 -- --nocapture
cargo test -p web-remote --locked static_srv::tests -- --nocapture
```

Expected: the lifecycle test and all static server tests PASS.

- [ ] **Step 6: Commit lifecycle behavior**

```bash
git add crates/web-remote/src/static_srv.rs crates/web-remote/assets/app.js
git commit -m "feat(web-remote): add full-screen viewer lifecycle"
```

### Task 4: Project connection state into overlays, input gates, and privacy

**Files:**
- Modify: `crates/web-remote/src/static_srv.rs:220-470`
- Modify: `crates/web-remote/assets/app.js:80-220,410-650,950-1040`

- [ ] **Step 1: Write the failing connection safety contract**

```rust
#[test]
fn 전체화면_뷰어는_재연결_입력잠금과_privacy_계약을_포함한다() {
    let js = std::str::from_utf8(APP_JS).unwrap();
    for marker in [
        "function setViewerConnection(",
        "viewer.connection === 'connected'",
        "&& !document.hidden",
        "inputBlocked = false; // reset per connection generation",
        "viewer.overlay.hidden = connected",
        "viewer.privacy.hidden = false",
        "viewer.privacy.hidden = true",
        "setViewerConnection('reconnecting')",
        "setViewerConnection('paused')",
        "intentionallyClosedSockets.has(socket)",
        "function isCurrentSocket(",
        "function stopAllKeyRepeats()",
        "if (!remoteInputReady()) return;",
        "const viewerDrafts = new Map()",
        "composerText.maxLength = MAX_DRAFT_CHARS",
        "const recoveredDraftSessions = new Set()",
        "const evictedDraftSessions = new Set()",
        "let draftCacheNotice = ''",
        "const closeNotices = [notice, draftCacheNotice]",
        "const RECENT_SEND_TTL_MS = 30_000",
        "function saveComposerDraft()",
        "function preserveComposerDraftForTransition()",
        "function loadComposerDraft(",
        "function discardSessionDraft(",
        "discardDraft: !!(watched && watched.exited)",
        "const recentSentBySession = new Map()",
        "function canRememberRecentSent(",
        "if (!canRememberRecentSent(target, text))",
        "function restoreDraft(note, sessionId)",
        "if (inputBlocked && !restoreDraft(",
        "연결이 바뀌어 최근 입력을 복원했습니다",
        "const uploadSession = pickerSession",
        "let pickerSession = null;",
        "let pendingUploadSelection = null;",
        "function consumePendingUploadSelection()",
        "pickerSession = viewer.watching;",
        "viewer.watching !== uploadSession",
        "function cancelActiveUpload()",
        "signal: upload.controller.signal",
        "if (activeUpload !== upload) return;",
        "if (nextComposerValue.length > MAX_DRAFT_CHARS)",
        "function projectVisibility()",
        "if (document.hidden) projectVisibility();",
    ] {
        assert!(js.contains(marker), "connection safety marker 누락: {marker}");
    }
    assert!(
        js.matches("if (!isCurrentSocket(socket)) return;").count() >= 2,
        "old socket open/message generation guard 누락"
    );
    assert!(
        js.matches("if (activeUpload !== upload) return;").count() >= 3,
        "stale upload continuation guard 누락"
    );
}
```

- [ ] **Step 2: Run the focused test and verify RED**

```bash
cargo test -p web-remote --locked 전체화면_뷰어는_재연결_입력잠금과_privacy_계약을_포함한다 -- --nocapture
```

Expected: FAIL at `setViewerConnection`.

- [ ] **Step 3: Add connection/privacy references and state projection**

Extend `viewer` with:

```javascript
connection: 'connecting',
connectionLabel: document.getElementById('viewer-connection'),
overlay: document.getElementById('viewer-connection-overlay'),
overlayTitle: document.getElementById('viewer-overlay-title'),
overlayDetail: document.getElementById('viewer-overlay-detail'),
privacy: document.getElementById('viewer-privacy-curtain'),
```

Add:

```javascript
const VIEWER_CONNECTION_COPY = {
  connecting: ['연결 중', '터미널 화면을 준비하고 있습니다.'],
  reconnecting: ['재연결 중', '마지막 화면을 유지합니다. 연결되기 전에는 입력할 수 없습니다.'],
  paused: ['일시정지', '앱으로 돌아오면 다시 연결합니다.'],
};

function setViewerConnection(state) {
  viewer.connection = state;
  const connected = state === 'connected';
  const copy = connected ? ['연결됨', ''] : VIEWER_CONNECTION_COPY[state];
  viewer.connectionLabel.textContent = copy[0];
  viewer.connectionLabel.className = 'viewer-connection ' + state;
  viewer.overlay.hidden = connected;
  if (!connected) {
    viewer.overlayTitle.textContent = copy[0];
    viewer.overlayDetail.textContent = copy[1];
    stopAllKeyRepeats();
    resetScroll();
    cancelActiveUpload();
    if (viewer.watching) {
      restoreDraft(
        '연결이 바뀌어 최근 입력을 복원했습니다 — 중복 여부를 확인하세요',
        viewer.watching,
      );
    }
    inputBlocked = false; // reset per connection generation
  }
  updateComposerEnabled();
  if (connected) consumePendingUploadSelection();
}
```

- [ ] **Step 4: Wire socket transitions without changing protocol messages**

In `connect()`, call `setViewerConnection('connecting')` after the global connecting status. In `welcome`, call `setViewerConnection('connected')` before the existing reconnect `watch`. In unexpected socket `close`, call `setViewerConnection('reconnecting')` before `scheduleReconnect()`:

Declare this next to the socket state:

```javascript
const intentionallyClosedSockets = new WeakSet();

function isCurrentSocket(socket) {
  return ws === socket && !intentionallyClosedSockets.has(socket);
}
```

```javascript
setStatus('', '연결 중…');
setViewerConnection('connecting');
```

```javascript
setStatus('ok', '연결됨');
setViewerConnection('connected');
if (viewer.watching) {
  viewer.screen = null;
  send({ type: 'watch', session: viewer.watching });
}
```

```javascript
socket.addEventListener('open', () => {
  if (!isCurrentSocket(socket)) return;
  reconnectDelay = 1000;
  socket.send(JSON.stringify({ type: 'auth', v: PROTOCOL_VERSION, token }));
});

socket.addEventListener('message', (event) => {
  if (!isCurrentSocket(socket)) return;
  let msg;
  try {
    msg = JSON.parse(event.data);
  } catch {
    return;
  }
  handleMessage(msg);
});

socket.addEventListener('close', () => {
  const wasCurrent = ws === socket;
  if (wasCurrent) ws = null;
  if (!wasCurrent || intentionallyClosedSockets.has(socket)) return;
  setStatus('bad', '연결 끊김 — 재연결 중…');
  setViewerConnection('reconnecting');
  scheduleReconnect();
});
```

The current-socket guard is required on both `open` and `message`, not only `close`: a deliberately closed or replaced socket may already have queued an event, and that event must not authenticate, mark the viewer connected, render an old dashboard, or apply stale input pressure to the new generation.

Replace the socket-close part of `disconnect()` so intentional ownership belongs to that exact socket even if a new connection starts before its late close event:

```javascript
if (ws) {
  const socket = ws;
  ws = null;
  intentionallyClosedSockets.add(socket);
  try { socket.close(); } catch {}
}
```

Replace Task 3's unconditional watch at the end of `openViewer` with:

```javascript
if (viewer.connection === 'connected') {
  send({ type: 'watch', session: sessionId });
}
```

- [ ] **Step 5: Gate every remote input**

Replace the enabling and key guards with:

```javascript
function remoteInputReady() {
  return !!viewer.watching
    && !viewer.closing
    && !document.hidden
    && viewer.connection === 'connected'
    && !inputBlocked;
}

function updateComposerEnabled() {
  const ready = remoteInputReady();
  composerSend.disabled = !ready;
  composerText.disabled = !ready;
  composerAttach.disabled = !ready || uploadBusy;
  for (const button of viewer.keys) button.disabled = !ready;
}

function sendKey(key) {
  if (!remoteInputReady()) return;
  send({ type: 'key', session: viewer.watching, key });
}
```

Change `sendComposer`'s initial guard to `if (!target || !remoteInputReady()) return;`. No connection transition may clear `composerText.value`.

The composer draft belongs to a session, not to the viewer DOM. Add a bounded per-session draft store next to the composer state:

```javascript
const MAX_CACHED_DRAFTS = 20;
const MAX_EVICTED_DRAFT_FLAGS = 256;
const MAX_DRAFT_CHARS = 256 * 1024;
const MAX_RECENT_SENDS_PER_SESSION = 32;
const MAX_RECENT_SENT_CHARS = MAX_DRAFT_CHARS * 2;
const MAX_STORED_DRAFT_CHARS = MAX_DRAFT_CHARS
  + MAX_RECENT_SENT_CHARS
  + MAX_RECENT_SENDS_PER_SESSION;
const viewerDrafts = new Map();
const recentSentBySession = new Map();
const recoveredDraftSessions = new Set();
const evictedDraftSessions = new Set();
let draftCacheNotice = '';
const RECENT_SEND_TTL_MS = 30_000;
let composerSession = null;
composerText.maxLength = MAX_DRAFT_CHARS;

function setBoundedSessionValue(store, sessionId, value) {
  store.delete(sessionId);
  const bounded = typeof value === 'string'
    ? value.slice(0, MAX_STORED_DRAFT_CHARS)
    : value;
  if (bounded) store.set(sessionId, bounded);
  else if (store === viewerDrafts) recoveredDraftSessions.delete(sessionId);
  while (store.size > MAX_CACHED_DRAFTS) {
    const evicted = store.keys().next().value;
    store.delete(evicted);
    if (store === viewerDrafts) {
      recoveredDraftSessions.delete(evicted);
      evictedDraftSessions.delete(evicted);
      evictedDraftSessions.add(evicted);
      while (evictedDraftSessions.size > MAX_EVICTED_DRAFT_FLAGS) {
        evictedDraftSessions.delete(evictedDraftSessions.values().next().value);
      }
      draftCacheNotice = '메모리 제한으로 가장 오래된 세션 초안 1개를 정리했습니다.';
    }
  }
}

function saveComposerDraft() {
  if (!composerSession) return;
  setBoundedSessionValue(viewerDrafts, composerSession, composerText.value);
}

function preserveComposerDraftForTransition() {
  if (!composerSession) return;
  const outgoing = lastSessions.find((session) => session.id === composerSession);
  if (outgoing && outgoing.exited) discardSessionDraft(composerSession);
  else saveComposerDraft();
}

function loadComposerDraft(sessionId) {
  composerSession = sessionId;
  composerText.value = viewerDrafts.get(sessionId) || '';
  autoGrow();
  const notices = [];
  if (draftCacheNotice) {
    notices.push(draftCacheNotice);
    draftCacheNotice = '';
  }
  if (evictedDraftSessions.delete(sessionId)) {
    notices.push('이 세션의 이전 초안을 복원하지 못했습니다.');
  }
  if (recoveredDraftSessions.delete(sessionId)) {
    notices.push('최근 입력을 복원했습니다 — 중복 여부를 확인하세요.');
  }
  if (notices.length) setComposerNote(notices.join(' '));
}

function discardSessionDraft(sessionId) {
  viewerDrafts.delete(sessionId);
  recentSentBySession.delete(sessionId);
  recoveredDraftSessions.delete(sessionId);
  evictedDraftSessions.delete(sessionId);
}

function pruneRecentSent(now = Date.now()) {
  const cutoff = now - RECENT_SEND_TTL_MS;
  for (const [sessionId, entries] of recentSentBySession) {
    const recent = entries.filter((item) => item.at >= cutoff);
    if (recent.length) recentSentBySession.set(sessionId, recent);
    else recentSentBySession.delete(sessionId);
  }
}

function canRememberRecentSent(sessionId, text) {
  pruneRecentSent();
  const recent = recentSentBySession.get(sessionId) || [];
  if (!recentSentBySession.has(sessionId)
      && recentSentBySession.size >= MAX_CACHED_DRAFTS) return false;
  if (recent.length >= MAX_RECENT_SENDS_PER_SESSION) return false;
  return recent.reduce((sum, item) => sum + item.text.length, 0) + text.length
    <= MAX_RECENT_SENT_CHARS;
}

function rememberRecentSent(sessionId, text) {
  const cutoff = Date.now() - RECENT_SEND_TTL_MS;
  const recent = (recentSentBySession.get(sessionId) || [])
    .filter((item) => item.at >= cutoff);
  recent.push({ text, at: Date.now() });
  recentSentBySession.delete(sessionId);
  recentSentBySession.set(sessionId, recent);
}
```

At the start of an accepted `openViewer` transition, call `preserveComposerDraftForTransition()` before changing `viewer.watching`, then use the exact note/load order `setComposerNote(''); loadComposerDraft(sessionId);`. The transition helper discards an outgoing session that the latest dashboard explicitly marks exited; otherwise it saves. This matters when the same dashboard render queues A's disappearance cleanup but synchronously consumes a pending deep link to B: B's transition must not reinsert A's dead draft before the stale close microtask returns. The note/load order clears an old session's note first and lets `loadComposerDraft` install a pending recovery warning afterward. In `finishCloseViewer`, save the current draft unless its explicit `discardDraft` option is set, clear `composerSession` and the visible textarea, and keep live-session drafts so reopening the same session restores them. Connection-only transitions preserve the current draft untouched.

Extend `finishCloseViewer` with `discardDraft = false`. For a confirmed exited session, call `discardSessionDraft(returnSession)` instead of saving; ordinary Back and a merely missing session keep their draft because another workspace can make that session temporarily absent. In the disappearance request use:

```javascript
discardDraft: !!(watched && watched.exited),
```

While rendering dashboard sessions, purge draft/recent/recovery metadata for every explicit `session.exited` row as well. This prevents permanently dead sessions from occupying bounded cache slots and evicting an older live unsent draft.

In `finishCloseViewer`, after removing dashboard inertness and before returning focus, consume a cache notice into the now-visible dashboard banner without overwriting a lifecycle notice:

```javascript
const closeNotices = [notice, draftCacheNotice].filter(Boolean);
draftCacheNotice = '';
if (closeNotices.length) showNotice(closeNotices.join(' '));
```

Do not emit this notice immediately from the cache helper: during A→B viewer transitions the dashboard banner is inert, aria-hidden, and covered by the fixed viewer. `loadComposerDraft` consumes it into the visible composer; a close consumes it only after the dashboard is visible.

Replace the single `lastSent` slot with the bounded per-session recent-send journal. Before `send()`, require `canRememberRecentSent(target, text)`; if it is false, leave the textarea unchanged and show `최근 전송 확인 중입니다 — 잠시 후 다시 보내세요`. Only after `send()` returns true, call `rememberRecentSent(target, text)` and clear the textarea. A pressure event has no client message id and may coalesce multiple rejections, so overwriting or evicting an unconfirmed candidate can silently lose an earlier rapid send. The pre-send admission guard bounds both entry count and characters without dropping admitted input. Restore every still-recent candidate for that session without discarding text the user typed afterward:

```javascript
function restoreDraft(note, sessionId) {
  const cutoff = Date.now() - RECENT_SEND_TTL_MS;
  const recent = (recentSentBySession.get(sessionId) || [])
    .filter((item) => item.at >= cutoff);
  recentSentBySession.delete(sessionId);
  if (!recent.length) return false;
  const uncertain = recent.map((item) => item.text).join('\n');
  const current = composerSession === sessionId
    ? composerText.value
    : (viewerDrafts.get(sessionId) || '');
  const separator = uncertain && current && !/\s$/.test(uncertain) ? '\n' : '';
  const restored = uncertain + separator + current;
  setBoundedSessionValue(viewerDrafts, sessionId, restored);
  if (composerSession === sessionId) {
    composerText.value = restored;
    autoGrow();
    setComposerNote(note);
  } else {
    recoveredDraftSessions.delete(sessionId);
    recoveredDraftSessions.add(sessionId);
    while (recoveredDraftSessions.size > MAX_CACHED_DRAFTS) {
      recoveredDraftSessions.delete(recoveredDraftSessions.values().next().value);
    }
  }
  return true;
}
```

Because the protocol does not acknowledge individual inputs, this journal is deliberately described as recent input that may need review before resending; it prefers visible recovery over silent loss. Both the per-session entry count and session count are bounded so repeated switching cannot create an unbounded client-side retention path.

`MAX_DRAFT_CHARS` bounds direct textarea input. The recent-send character budget, current maximum-size draft, and join separators fit `MAX_STORED_DRAFT_CHARS`; the storage helper enforces that per-entry ceiling as a final defense. Together with `MAX_CACHED_DRAFTS`, both the number and size of retained values are bounded rather than merely the number of `Map` keys. If the 21st nonempty live draft evicts the oldest, the app immediately surfaces a global notice and keeps a bounded per-session eviction flag so returning to that session cannot misleadingly look like an intact empty draft.

Protocol v3 has no per-input acknowledgement, so exact accepted-versus-rejected classification is impossible without a protocol revision. Keep candidates for a conservative bounded 30 seconds (longer than the 15-second reconnect ceiling), recover them immediately on connection generation change or rejection pressure, and block additional sends when the count/character journal is full. This slice does not claim exactly-once delivery under an indefinitely throttled-but-open tab; that requires a later client sequence/ack protocol change rather than more client inference.

Pass `msg.session` into every `restoreDraft` call. If an `input_pressure` message belongs to a no-longer-watched session, restore its matching cached draft without changing the current session's `inputBlocked` or note; do not silently discard it at the old top-level session guard. This preserves A's rejected input even after the user has switched to B.

```javascript
case 'input_pressure':
  if (!msg.session) break;
  if (msg.session !== viewer.watching) {
    if (msg.reason !== 'queue_full' || (msg.queued || 0) > 0) {
      restoreDraft('', msg.session);
    }
    break;
  }
  handleInputPressure(msg);
  break;
```

In `handleInputPressure`, the `queue_full` branch must restore only while `queued > 0`:

```javascript
inputBlocked = queued > 0;
if (inputBlocked && !restoreDraft(
    '입력 대기열이 차 최근 입력을 복원했습니다 — 중복 여부를 확인하세요',
    msg.session)) {
  setComposerNote('입력 대기열이 찼습니다 — 잠시 후 다시 보내세요');
} else if (!inputBlocked) {
  recentSentBySession.delete(msg.session);
  setComposerNote('');
}
```

`queue_full` with `queued == 0` is a resolution event, not proof that the latest input was rejected. Restoring on that frame would duplicate an accepted command.

Gate scrolling at enqueue, flush, and the bottom button. Replace the start of `queueScroll` and its send condition with:

```javascript
function queueScroll(lines) {
  if (!remoteInputReady()) return;
  scrollAcc += lines;
  if (scrollTimer) return;
  scrollTimer = setTimeout(() => {
    scrollTimer = null;
    const whole = Math.trunc(scrollAcc);
    scrollAcc -= whole;
    if (whole !== 0 && remoteInputReady()) {
      send({ type: 'scroll', session: viewer.watching, delta: whole });
    }
  }, 60);
}
```

Use `if (offset > 0 && remoteInputReady())` in the `viewer-bottom` click handler.

- [ ] **Step 6: Abort active uploads and revalidate delayed file selection**

Add upload ownership next to `uploadBusy`:

```javascript
let activeUpload = null;
let pickerSession = null;
let pendingUploadSelection = null;

function cancelActiveUpload() {
  if (!activeUpload) return;
  activeUpload.controller.abort();
  activeUpload = null;
  uploadBusy = false;
  setComposerNote('');
  updateComposerEnabled();
}

function cancelPendingUploadSelection() {
  pickerSession = null;
  pendingUploadSelection = null;
}
```

Call `cancelActiveUpload()` and `cancelPendingUploadSelection()` at the start of an accepted new `openViewer` transition and immediately after the guards in `requestCloseViewer`, so session changes and close requests cannot leave an orphan upload or delayed file selection. Also call both from central `finishCloseViewer`: native browser Back enters through `popstate` and bypasses `requestCloseViewer`, so cleanup must be idempotent at the convergence point. Clearing the note is part of active-upload cancellation ownership, preventing stale `업로드 중…` copy after reconnect.

Capture delayed file-picker ownership when the picker opens, not when its later `change` event fires:

```javascript
composerAttach.addEventListener('click', () => {
  if (composerAttach.disabled) return;
  pickerSession = viewer.watching;
  composerFile.click();
});
```

At the start of the file-input `change` handler, consume the click-time owner before reading the file, then validate both the file and exact session. Validate the 10 MiB limit before retaining the `File`. If the native picker temporarily hid the page and the WebSocket has not welcomed the foreground connection yet, retain exactly one bounded selection instead of rejecting it:

```javascript
const uploadSession = pickerSession;
pickerSession = null;
const file = composerFile.files && composerFile.files[0];
composerFile.value = '';
if (!file) return;
if (!uploadSession || viewer.watching !== uploadSession) {
  setComposerNote('세션이 바뀌어 선택한 파일을 업로드하지 않았습니다');
  return;
}
if (file.size > MAX_UPLOAD_BYTES) {
  setComposerNote('파일이 너무 큽니다 (10MB 초과)');
  return;
}
if (!remoteInputReady()) {
  pendingUploadSelection = { session: uploadSession, file };
  setComposerNote('재연결 후 선택한 파일을 업로드합니다…');
  return;
}
beginSelectedUpload(uploadSession, file);
```

Move the existing async fetch body into `async function beginSelectedUpload(uploadSession, file)`. It must revalidate the same session and `remoteInputReady()` before creating `activeUpload`.

```javascript
function consumePendingUploadSelection() {
  if (!pendingUploadSelection || !remoteInputReady()) return;
  const pending = pendingUploadSelection;
  if (viewer.watching !== pending.session) {
    pendingUploadSelection = null;
    return;
  }
  pendingUploadSelection = null;
  beginSelectedUpload(pending.session, pending.file);
}
```

The pending slot holds at most one already-size-checked `File` and is preserved only across connection/visibility transitions. Session switch and every close path clear it. Do not clear `pickerSession` merely because `visibilitychange` projected `paused`: mobile native file pickers commonly hide the document, and their `change` can arrive before WebSocket `welcome` on return.

Before setting `uploadBusy`, create exact ownership:

```javascript
const upload = { session: uploadSession, controller: new AbortController() };
activeUpload = upload;
```

Add the signal to fetch:

```javascript
signal: upload.controller.signal,
```

An abort may race with an already-resolved fetch or response-body parse. After each `await` and before any upload-owned note or composer mutation, require exact ownership:

```javascript
const res = await fetch(/* ... */);
if (activeUpload !== upload) return;
// validate res
const result = await res.json();
if (activeUpload !== upload) return;
// validate and insert result
```

After validating `result.path` and before inserting it into the composer, revalidate:

```javascript
if (activeUpload !== upload) return;
if (viewer.watching !== uploadSession || !remoteInputReady()) {
  setComposerNote('연결 또는 세션이 바뀌어 업로드 경로를 입력하지 않았습니다');
  return;
}
```

Programmatic `.value` assignment is not constrained by `textarea.maxLength`. Build and check the upload-path mutation before assigning it:

```javascript
const separator = composerText.value && !/\s$/.test(composerText.value) ? '\n' : '';
const nextComposerValue = composerText.value + separator + result.path + ' ';
if (nextComposerValue.length > MAX_DRAFT_CHARS) {
  setComposerNote('입력이 너무 길어 업로드 경로를 추가하지 않았습니다');
  return;
}
composerText.value = nextComposerValue;
```

This keeps both direct input and upload-appended composer content inside the same per-session cache bound.

Use a named catch value and ownership-aware finalization:

```javascript
} catch (error) {
  if (activeUpload !== upload) return;
  if (!(error && error.name === 'AbortError')) {
    setComposerNote('업로드 실패 — 네트워크를 확인하세요');
  }
} finally {
  if (activeUpload === upload) {
    activeUpload = null;
    uploadBusy = false;
    updateComposerEnabled();
  }
}
```

- [ ] **Step 7: Show privacy before background disconnect**

Replace the existing visibility handler with one projection function and invoke it for an initially hidden document as well:

```javascript
function projectVisibility() {
  if (document.hidden) {
    if (viewer.watching) viewer.privacy.hidden = false;
    setViewerConnection('paused');
    disconnect();
    setStatus('', '일시정지(백그라운드)');
  } else {
    viewer.privacy.hidden = true;
    setViewerConnection('connecting');
    connect();
  }
}

document.addEventListener('visibilitychange', projectVisibility);
if (document.hidden) projectVisibility();
else connect();
```

Remove the old unconditional final `connect()`. A page restored or opened in the background must begin paused, disconnected, and privacy-projected without waiting for a visibility event that may never fire.

- [ ] **Step 8: Run the focused and static tests**

```bash
cargo test -p web-remote --locked 전체화면_뷰어는_재연결_입력잠금과_privacy_계약을_포함한다 -- --nocapture
cargo test -p web-remote --locked static_srv::tests -- --nocapture
```

Expected: the focused test and all static server tests PASS.

- [ ] **Step 9: Commit connection safety**

```bash
git add crates/web-remote/src/static_srv.rs crates/web-remote/assets/app.js
git commit -m "fix(web-remote): lock viewer input during reconnect"
```

### Task 5: Coalesce visual viewport work and fit both terminal axes

**Files:**
- Modify: `crates/web-remote/src/static_srv.rs:220-490`
- Modify: `crates/web-remote/assets/app.js:250-430,620-650`

- [ ] **Step 1: Write the failing renderer contract**

```rust
#[test]
fn 전체화면_렌더러는_visual_viewport와_단일_frame_스케줄러를_사용한다() {
    let js = std::str::from_utf8(APP_JS).unwrap();
    for marker in [
        "function scheduleViewerRender()",
        "requestAnimationFrame",
        "window.visualViewport",
        "visualViewport.addEventListener('resize'",
        "visualViewport.addEventListener('scroll'",
        "new ResizeObserver",
        "visualViewport.offsetLeft",
        "visualViewport.width",
        "viewer.el.style.setProperty('--viewer-left'",
        "viewer.el.style.setProperty('--viewer-width'",
        "availableHeight / (screen.rows * CELL_ASPECT_RATIO)",
        "ctx.setTransform(dpr, 0, 0, dpr, 0, 0)",
        "scheduleViewerRender(); // composer layout changed",
    ] {
        assert!(js.contains(marker), "viewport renderer marker 누락: {marker}");
    }
}
```

- [ ] **Step 2: Run the focused test and verify RED**

```bash
cargo test -p web-remote --locked 전체화면_렌더러는_visual_viewport와_단일_frame_스케줄러를_사용한다 -- --nocapture
```

Expected: FAIL at `scheduleViewerRender`.

- [ ] **Step 3: Add one animation-frame scheduler and viewport measurement**

Add before the renderer:

```javascript
const CELL_ASPECT_RATIO = 2;
let viewerRenderFrame = 0;

function syncViewerViewport() {
  const visualViewport = window.visualViewport;
  const top = visualViewport ? visualViewport.offsetTop : 0;
  const left = visualViewport ? visualViewport.offsetLeft : 0;
  const width = visualViewport ? visualViewport.width : window.innerWidth;
  const height = visualViewport ? visualViewport.height : window.innerHeight;
  const topPx = Math.max(0, Math.round(top)) + 'px';
  const leftPx = Math.max(0, Math.round(left)) + 'px';
  const widthPx = Math.max(1, Math.round(width)) + 'px';
  const heightPx = Math.max(1, Math.round(height)) + 'px';
  if (viewer.el.style.getPropertyValue('--viewer-top') !== topPx) {
    viewer.el.style.setProperty('--viewer-top', topPx);
  }
  if (viewer.el.style.getPropertyValue('--viewer-height') !== heightPx) {
    viewer.el.style.setProperty('--viewer-height', heightPx);
  }
  if (viewer.el.style.getPropertyValue('--viewer-left') !== leftPx) {
    viewer.el.style.setProperty('--viewer-left', leftPx);
  }
  if (viewer.el.style.getPropertyValue('--viewer-width') !== widthPx) {
    viewer.el.style.setProperty('--viewer-width', widthPx);
  }
}

function scheduleViewerRender() {
  if (!viewer.watching || viewerRenderFrame) return;
  viewerRenderFrame = requestAnimationFrame(() => {
    viewerRenderFrame = 0;
    syncViewerViewport();
    drawScreenNow();
  });
}
```

Add `scheduleViewerRender();` after `viewer.back.focus();` in `openViewer`, before the connection-gated watch.

- [ ] **Step 4: Split immediate drawing and fit width plus height**

Replace the current `drawScreen` with this complete immediate renderer:

```javascript
function drawScreenNow() {
  const screen = viewer.screen;
  if (!screen || viewer.el.hidden) return;
  const canvas = viewer.canvas;
  const dpr = window.devicePixelRatio || 1;
  const availableWidth = viewer.wrap.clientWidth;
  const availableHeight = viewer.wrap.clientHeight;
  if (availableWidth <= 0 || availableHeight <= 0 || screen.cols <= 0 || screen.rows <= 0) return;
  const cellW = Math.min(
    availableWidth / screen.cols,
    availableHeight / (screen.rows * CELL_ASPECT_RATIO),
  );
  const cellH = cellW * CELL_ASPECT_RATIO;
  viewer.cellH = cellH;
  const cssWidth = cellW * screen.cols;
  const cssHeight = cellH * screen.rows;
  const pixelWidth = Math.max(1, Math.round(cssWidth * dpr));
  const pixelHeight = Math.max(1, Math.round(cssHeight * dpr));
  if (canvas.width !== pixelWidth) canvas.width = pixelWidth;
  if (canvas.height !== pixelHeight) canvas.height = pixelHeight;
  canvas.style.width = cssWidth + 'px';
  canvas.style.height = cssHeight + 'px';
  const ctx = canvas.getContext('2d');
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.fillStyle = '#000000';
  ctx.fillRect(0, 0, cssWidth, cssHeight);
  const A_BOLD = 1;
  const A_ITALIC = 2;
  const A_UNDERLINE = 4;
  const A_STRIKE = 8;
  const A_DIM = 16;
  const fontPx = (cellH * 0.82).toFixed(2);
  const fontFor = (attrs) => {
    const style = attrs & A_ITALIC ? 'italic ' : '';
    const weight = attrs & A_BOLD ? '700 ' : '';
    return style + weight + fontPx + 'px ui-monospace, Menlo, monospace';
  };
  const dimmed = (hex) => {
    const match = /^#?([0-9a-f]{6})$/i.exec(hex || '');
    if (!match) return hex;
    const value = parseInt(match[1], 16);
    const fade = (channel) => Math.round(channel * 0.6);
    return `rgb(${fade((value >> 16) & 255)},${fade((value >> 8) & 255)},${fade(value & 255)})`;
  };
  ctx.font = fontFor(0);
  ctx.textBaseline = 'middle';
  for (let row = 0; row < screen.rows; row++) {
    const runs = screen.lines[row];
    if (!runs) continue;
    const y = row * cellH;
    for (const run of runs) {
      const advance = run.w ? cellW * 2 : cellW;
      const chars = Array.from(run.t || '');
      const attrs = run.a || 0;
      ctx.fillStyle = run.bg || '#000000';
      ctx.fillRect(run.s * cellW, y, chars.length * advance, cellH);
      const foreground = attrs & A_DIM
        ? dimmed(run.fg || '#d4d4d4')
        : (run.fg || '#d4d4d4');
      ctx.fillStyle = foreground;
      ctx.font = fontFor(attrs);
      for (let index = 0; index < chars.length; index++) {
        if (chars[index] === ' ') continue;
        ctx.fillText(chars[index], run.s * cellW + index * advance, y + cellH / 2, advance);
      }
      if (attrs & (A_UNDERLINE | A_STRIKE)) {
        const x = run.s * cellW;
        const width = chars.length * advance;
        ctx.fillStyle = foreground;
        if (attrs & A_UNDERLINE) ctx.fillRect(x, y + cellH - 1.5, width, 1);
        if (attrs & A_STRIKE) ctx.fillRect(x, y + cellH / 2, width, 1);
      }
    }
  }
  ctx.font = fontFor(0);
  const cursor = screen.cursor;
  if (cursor && cursor.visible) {
    ctx.fillStyle = 'rgba(212, 212, 212, 0.45)';
    ctx.fillRect(cursor.col * cellW, cursor.row * cellH, cellW, cellH);
  }
}
```

Change `handleViewport`'s final draw call to `scheduleViewerRender()`.

- [ ] **Step 5: Replace direct resize draws with coalesced observers**

Remove `window.addEventListener('resize', () => drawScreen())` and add:

```javascript
window.addEventListener('resize', scheduleViewerRender);
if (window.visualViewport) {
  window.visualViewport.addEventListener('resize', scheduleViewerRender);
  window.visualViewport.addEventListener('scroll', scheduleViewerRender);
}
if ('ResizeObserver' in window) {
  new ResizeObserver(scheduleViewerRender).observe(viewer.wrap);
}
```

Also schedule after footer height changes so the window-only fallback remains correct. Append this exact line at the end of both `autoGrow()` and `setComposerNote()`:

```javascript
scheduleViewerRender(); // composer layout changed
```

- [ ] **Step 6: Run renderer, static, and syntax tests**

```bash
cargo test -p web-remote --locked 전체화면_렌더러는_visual_viewport와_단일_frame_스케줄러를_사용한다 -- --nocapture
cargo test -p web-remote --locked static_srv::tests -- --nocapture
node --check crates/web-remote/assets/app.js
```

Expected: focused and static tests PASS; Node exits 0 without output.

- [ ] **Step 7: Commit the coalesced renderer**

```bash
git add crates/web-remote/src/static_srv.rs crates/web-remote/assets/app.js
git commit -m "perf(web-remote): coalesce full-screen terminal rendering"
```

### Task 6: Run full regressions and measure browser invariants

**Files:**
- Modify: `docs/CODEX_HANDOFF.md`

- [ ] **Step 1: Run the full scoped test suite**

```bash
cargo test -p web-remote --locked -- --test-threads=1
```

Expected: all web-remote tests and doc tests PASS. Record actual counts from output.

- [ ] **Step 2: Run syntax, lint, formatting, and diff checks**

```bash
node --check crates/web-remote/assets/app.js
cargo clippy -p web-remote --locked --all-targets -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Expected: every command exits 0. Record and fix only an in-scope failure before rerunning it.

- [ ] **Step 3: Measure open and Back invariants in a real browser**

Start the existing application/web-remote path without changing Tailscale settings, pair with its current token, open a session, and evaluate:

```javascript
(() => {
  const viewer = document.getElementById('viewer');
  const stage = document.getElementById('viewer-stage');
  const canvas = document.getElementById('viewer-canvas');
  const dashboard = document.getElementById('dashboard-shell');
  const vr = viewer.getBoundingClientRect();
  const sr = stage.getBoundingClientRect();
  const cr = canvas.getBoundingClientRect();
  return {
    viewerHidden: viewer.hidden,
    bodyOpen: document.body.classList.contains('viewer-open'),
    dashboardInert: dashboard.inert,
    dashboardAriaHidden: dashboard.getAttribute('aria-hidden'),
    viewportHeight: window.visualViewport ? window.visualViewport.height : window.innerHeight,
    viewerHeight: vr.height,
    stageWidth: sr.width,
    stageHeight: sr.height,
    canvasWidth: cr.width,
    canvasHeight: cr.height,
    canvasInsideStage: cr.width <= sr.width + 0.5 && cr.height <= sr.height + 0.5,
    activeElement: document.activeElement && document.activeElement.id,
  };
})()
```

Expected while open: viewer visible, `bodyOpen=true`, dashboard inert and aria-hidden, viewer height within 1 CSS pixel of visual viewport, canvas inside stage, and `activeElement="viewer-back"`.

Exercise browser Back, then evaluate:

```javascript
(() => ({
  viewerHidden: document.getElementById('viewer').hidden,
  bodyOpen: document.body.classList.contains('viewer-open'),
  dashboardInert: document.getElementById('dashboard-shell').inert,
  activeSession: document.activeElement && document.activeElement.dataset
    ? document.activeElement.dataset.sessionId || null
    : null,
}))()
```

Expected: viewer hidden, body unlocked, dashboard active, and focus restored to the closed session when it still exists. Repeat open measurements in portrait and short landscape viewports and record actual dimensions. Do not report keyboard or standalone PWA behavior as passed unless actually exercised.

Open one session, let it paint a recognizable non-black frame, close it, and open a different session while delaying its first viewport frame. Expected: no pixel from the previous session is visible; the canvas is black until the new keyframe arrives.

Reload once with a synthetic or real `history.state.deppyViewer` marker and exercise browser Forward after closing. Expected: the stale marker is removed, a later open still creates exactly one viewer entry, and its Back returns to the dashboard. While a programmatic close is waiting for `popstate`, confirm the inert dashboard cannot start a second viewer transition.

- [ ] **Step 4: Measure reconnect, draft, and privacy invariants**

With a non-empty composer draft, interrupt the existing WebSocket connection. Verify the connection overlay is visible; every special key, composer send, textarea, and attachment control is disabled; canvas dimensions remain non-zero; and the draft value is unchanged. Restore the connection and verify overlay removal and that the draft was not auto-sent.

Perform a quick background→foreground transition so the intentionally closed old socket can deliver its `close` after the new socket starts. Expected: the late old event is ignored and does not return the connected viewer to `reconnecting`.

Start an arrow-key long press and a coalesced scroll, then interrupt the connection before their timers fire. Expected: repeat and scroll timers are canceled and no stale command is emitted after reconnection. Open the file picker while connected, disconnect before selecting a file, then return from the picker. Expected: no upload begins and the UI asks for reconnection.

Trigger a real background/foreground transition where supported. Verify the privacy curtain appears before disconnect and hides on return. If the environment cannot create a genuine visibility transition, record it as unverified.

- [ ] **Step 5: Review protocol and Tailscale isolation**

Set `VIEWER_BASE` to the design commit `0afdc16`, then run:

```bash
VIEWER_BASE=0afdc16
git diff "$VIEWER_BASE" -- crates/web-remote/assets/index.html crates/web-remote/assets/app.css crates/web-remote/assets/app.js crates/web-remote/src/static_srv.rs
git diff "$VIEWER_BASE" -- crates/web-remote/src/lib.rs crates/web-remote/src/ws_api.rs
```

Expected: the first diff contains shell/test work; the second is empty. Tailscale settings live outside the scoped files and must remain untouched.

- [ ] **Step 6: Update the handoff with actual evidence**

Update the current `docs/CODEX_HANDOFF.md` section with the exact objective, completed behavior, modified files, design decisions, command exit codes/test counts, browser dimensions, failed attempts, verified gaps, remaining Relay work, and exact next commands. Do not copy an expected result as test evidence.

- [ ] **Step 7: Commit the verification record**

```bash
git add docs/CODEX_HANDOFF.md
git commit -m "docs: record full-screen viewer verification"
```

Do not stage pre-existing Keychain, packaging, prototype, or unrelated app changes.
