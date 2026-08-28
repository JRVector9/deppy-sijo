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
  setViewerClosing(false);
  if (rerender) renderWorkspaces(lastWorkspaces, lastResource);
  if (notice) showNotice(notice);
  restoreViewerFocus(returnSession);
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
const stopRepeat = () => {
  clearTimeout(repeatTimer);
  clearInterval(repeatInterval);
  repeatTimer = null;
  repeatInterval = null;
};
keyRepeatCancels.push(() => {
  stopRepeat();
  repeated = false;
});
```

Remove the later duplicate `let repeated = false`. Pointer-up/leave/cancel continue to call `stopRepeat`, preserving trailing-click suppression for a real long press.

Change `consumePendingWatch` so closing never consumes the pending id and a rejected open leaves it queued:

```javascript
function consumePendingWatch(sessions) {
  if (!pendingWatch) return false;
  if (Date.now() > pendingWatchDeadline) {
    pendingWatch = null;
    return false;
  }
  if (viewer.closing) return false;
  const target = sessions.find((session) => session.id === pendingWatch);
  if (!target) return false;
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

Immediately after `lastSessions` is computed in `renderWorkspaces`, add:

```javascript
const watched = viewer.watching
  ? lastSessions.find((session) => session.id === viewer.watching)
  : null;
if (viewer.watching && (!watched || watched.exited)) {
  requestCloseViewer({
    rerender: true,
    notice: '선택한 세션이 종료되었습니다 — 세션 목록으로 돌아왔습니다.',
  });
}
```

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
        "viewer.overlay.hidden = connected",
        "viewer.privacy.hidden = false",
        "viewer.privacy.hidden = true",
        "setViewerConnection('reconnecting')",
        "setViewerConnection('paused')",
        "intentionallyClosedSockets.has(socket)",
        "function stopAllKeyRepeats()",
        "if (!remoteInputReady()) return;",
        "const uploadSession = viewer.watching",
        "function cancelActiveUpload()",
        "signal: upload.controller.signal",
    ] {
        assert!(js.contains(marker), "connection safety marker 누락: {marker}");
    }
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
  }
  updateComposerEnabled();
}
```

- [ ] **Step 4: Wire socket transitions without changing protocol messages**

In `connect()`, call `setViewerConnection('connecting')` after the global connecting status. In `welcome`, call `setViewerConnection('connected')` before the existing reconnect `watch`. In unexpected socket `close`, call `setViewerConnection('reconnecting')` before `scheduleReconnect()`:

Declare this next to the socket state:

```javascript
const intentionallyClosedSockets = new WeakSet();
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
socket.addEventListener('close', () => {
  const wasCurrent = ws === socket;
  if (wasCurrent) ws = null;
  if (!wasCurrent || intentionallyClosedSockets.has(socket)) return;
  setStatus('bad', '연결 끊김 — 재연결 중…');
  setViewerConnection('reconnecting');
  scheduleReconnect();
});
```

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

function cancelActiveUpload() {
  if (!activeUpload) return;
  activeUpload.controller.abort();
  activeUpload = null;
  uploadBusy = false;
  updateComposerEnabled();
}
```

Call `cancelActiveUpload()` at the start of an accepted new `openViewer` transition and immediately after the guards in `requestCloseViewer`, so session changes and close requests cannot leave an orphan upload.

At the start of the file-input `change` handler, after confirming `file` exists, capture and validate the exact session:

```javascript
const uploadSession = viewer.watching;
if (!uploadSession || !remoteInputReady()) {
  setComposerNote('연결이 끊겼습니다 — 재연결 후 파일을 선택하세요');
  return;
}
```

Before setting `uploadBusy`, create exact ownership:

```javascript
const upload = { session: uploadSession, controller: new AbortController() };
activeUpload = upload;
```

Add the signal to fetch:

```javascript
signal: upload.controller.signal,
```

After validating `result.path` and before inserting it into the composer, revalidate:

```javascript
if (viewer.watching !== uploadSession || !remoteInputReady()) {
  setComposerNote('연결 또는 세션이 바뀌어 업로드 경로를 입력하지 않았습니다');
  return;
}
```

Use a named catch value and ownership-aware finalization:

```javascript
} catch (error) {
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

Replace the existing visibility handler:

```javascript
document.addEventListener('visibilitychange', () => {
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
});
```

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
