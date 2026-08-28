# Mobile Relay Fullscreen Prototype Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build one self-contained Korean HTML prototype that demonstrates secure non-Tailscale pairing, mobile session selection, full-viewport terminal use, and recovery states.

**Architecture:** The artifact is a single HTML document with inline CSS and JavaScript so it opens directly from Finder without a server or dependencies. A small explicit state machine drives seven scenario states; the terminal state becomes a real viewport-fixed overlay inside the browser while all network, QR, and terminal data remain deterministic simulations.

**Tech Stack:** Semantic HTML5, CSS custom properties, responsive CSS, vanilla JavaScript, inline SVG icons, local-only simulated state

---

## File structure

- Create `docs/mockups/mobile-relay-fullscreen-scenario.html`: owns the complete visual artifact, responsive layout, scenario state machine, simulated terminal, and interaction copy.
- Modify `docs/CODEX_HANDOFF.md`: records the artifact path, design choices, explicit no-test instruction, open command, and remaining production scope.

The prototype intentionally does not modify `crates/web-remote` or production Rust code.

### Task 1: Build the self-contained scenario shell

**Files:**

- Create: `docs/mockups/mobile-relay-fullscreen-scenario.html`

- [x] **Step 1: Create a semantic document with fixed scenario states**

Use these exact state identifiers and order so navigation, labels, and progress remain consistent:

```js
const FLOW = [
  'access-off',
  'relay-connecting',
  'pairing-qr',
  'pairing-confirm',
  'session-list',
  'terminal',
  'recovery',
];
```

The document must contain:

```html
<main class="prototype-shell">
  <header class="prototype-header">...</header>
  <nav class="flow-rail" aria-label="연결부터 사용까지">...</nav>
  <section id="desktop-stage" aria-label="Mac 설정 화면">...</section>
  <section id="phone-stage" aria-label="모바일 화면">...</section>
</main>
<section id="terminal-view" role="dialog" aria-modal="true" hidden>...</section>
```

Each state renders from local fixture objects. No network call, external font, external image, camera API, WebSocket, or storage API is allowed.

- [x] **Step 2: Apply the Deppy Sijo visual system**

Define the exact shared palette at the top of the inline stylesheet:

```css
:root {
  color-scheme: dark;
  --bg: #181b20;
  --surface: #20242a;
  --surface-2: #0f1115;
  --border: #32363e;
  --text: #dcdee2;
  --muted: #828893;
  --cyan: #39b8e8;
  --green: #56d364;
  --amber: #e0a43a;
  --red: #ef6671;
  --blue: #58a6ff;
}
```

Use Apple system UI fonts and `D2Coding, SFMono-Regular, Menlo, monospace` for terminal content. Use flat 1px dividers and restrained 8–14px radii. Do not use gradients, glass effects, oversized marketing headings, or decorative card grids.

- [x] **Step 3: Make desktop and phone frames responsive**

For viewports at least 920px wide, show the desktop and phone frames side by side. Below 920px, show a compact stage switcher and make the phone frame fill the available width. Respect safe-area insets and keep every interactive target at least 44px high.

### Task 2: Implement the seven-state interaction flow

**Files:**

- Modify: `docs/mockups/mobile-relay-fullscreen-scenario.html`

- [x] **Step 1: Implement deterministic state rendering**

Use one state object and one render entry point:

```js
const state = {
  step: 0,
  phoneSurface: 'pairing',
  permissions: { view: true, input: true, approval: false },
  terminalMode: 'connected',
  selectedSession: null,
};

function setStep(index) {
  state.step = Math.max(0, Math.min(FLOW.length - 1, index));
  render();
}

function render() {
  renderRail();
  renderDesktop();
  renderPhone();
  syncTerminalView();
}
```

All step navigation, device switching, permission toggles, session selection, Back, disconnect, reconnect, and session-end controls must use `data-action` event delegation.

- [x] **Step 2: Render the connection and pairing states**

The first four states must show these product truths:

- `access-off`: mobile access is opt-in and opens no inbound router port.
- `relay-connecting`: three progress rows—Internet check, relay connection, encrypted channel.
- `pairing-qr`: five-minute one-time QR representation, countdown label, cancel and regenerate actions.
- `pairing-confirm`: identical verification code `741 206` on Mac and phone; permissions for view, input, and approvals; view remains mandatory.

The QR is a clearly labelled visual placeholder made from a deterministic CSS grid. It must state that it is not scannable and must never encode a real token.

- [x] **Step 3: Render the mobile session list**

Use these deterministic fixtures:

```js
const WORKSPACES = [
  {
    name: 'deppy-sijo',
    state: 'active',
    sessions: [
      { id: 'codex-review', title: '코드 리뷰', agent: 'Codex · 작업 중', status: 'running' },
      { id: 'release-build', title: '릴리즈 빌드', agent: 'Shell', status: 'waiting' },
    ],
  },
  {
    name: 'design-system',
    state: 'warm',
    sessions: [
      { id: null, title: '모바일 화면 설계', agent: 'Claude', status: 'idle' },
    ],
  },
];
```

The phone header must show `종단 간 암호화`, the Mac online state, and permission chips. Selecting an active session opens the terminal overlay. Selecting a warm workspace displays a confirmation that the Mac workspace will also change.

### Task 3: Implement the full-viewport terminal and recovery behavior

**Files:**

- Modify: `docs/mockups/mobile-relay-fullscreen-scenario.html`

- [x] **Step 1: Open terminal as a viewport-fixed application state**

Use this layout contract:

```css
.terminal-view {
  position: fixed;
  inset: 0;
  z-index: 100;
  height: 100dvh;
  display: grid;
  grid-template-rows: auto minmax(0, 1fr) auto;
  background: var(--surface-2);
  padding: env(safe-area-inset-top) 0 env(safe-area-inset-bottom);
}

body.terminal-open { overflow: hidden; }
```

The top bar contains Back, workspace/session title, connection indicator, and permission state. The center is a scrollable simulated terminal. The bottom contains horizontally scrollable special keys and a composer. If input permission is off, replace composer controls with an input-permission request row.

- [x] **Step 2: Implement accessible open and close behavior**

Implement these exact functions:

```js
let terminalReturnFocus = null;

function openTerminal(sessionId, trigger) {
  terminalReturnFocus = trigger || document.activeElement;
  state.selectedSession = sessionId;
  state.terminalMode = 'connected';
  document.body.classList.add('terminal-open');
  history.pushState({ deppyView: 'terminal' }, '');
  syncTerminalView();
  document.querySelector('[data-action="close-terminal"]').focus();
}

function closeTerminal({ fromHistory = false } = {}) {
  if (!state.selectedSession) return;
  state.selectedSession = null;
  document.body.classList.remove('terminal-open');
  syncTerminalView();
  if (!fromHistory && history.state?.deppyView === 'terminal') history.back();
  terminalReturnFocus?.focus();
}
```

Handle `popstate` so mobile Back closes only the terminal. Apply `inert` and `aria-hidden` to the prototype shell while the overlay is open, and restore both on exit.

- [x] **Step 3: Implement recovery simulations**

`연결 끊김 시뮬레이션` must freeze the terminal and show an overlay with last-received time, `다시 연결`, and `세션 목록으로`. Disable special keys and send actions while disconnected. Reconnect must restore the terminal but never auto-send the composer draft. `세션 종료 시뮬레이션` must return to the list and show a non-destructive notice.

### Task 4: Deliver the artifact without automated testing

**Files:**

- Modify: `docs/CODEX_HANDOFF.md`

- [x] **Step 1: Record delivery and explicit verification status**

Update the current handoff with the artifact path, implemented state flow, modified files, design decisions, skipped automated/browser tests, and exact open command. Do not claim visual or functional verification.

- [x] **Step 2: Open the standalone HTML directly**

Run:

```bash
open docs/mockups/mobile-relay-fullscreen-scenario.html
```

Expected result: the operating system opens the artifact in the default browser. This is delivery only, not test evidence.

- [x] **Step 3: Commit only the prototype plan and artifact**

Run:

```bash
git add docs/superpowers/plans/2026-08-28-mobile-relay-fullscreen-prototype.md docs/mockups/mobile-relay-fullscreen-scenario.html
git commit -m "docs: 모바일 원격 접속 프로토타입 추가"
```

Do not stage the pre-existing Keychain, notarization, secret-store, or handoff changes.
