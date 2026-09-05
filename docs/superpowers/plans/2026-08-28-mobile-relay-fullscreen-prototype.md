# Mobile Relay Fullscreen Prototype Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build one self-contained Korean HTML prototype that preserves the existing Tailscale path, adds an optional Relay path, and demonstrates shared mobile session selection, full-viewport terminal use, and recovery states.

**Architecture:** The artifact is a single HTML document with inline CSS and JavaScript so it opens directly from Finder without a server or dependencies. A small explicit state machine drives seven scenario states plus independent transport selection: Tailscale keeps its existing Serve/QR/token semantics, Relay uses one-time device pairing, and both converge only after authentication into the shared session and terminal UI.

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

### Task 5: Show Tailscale and Relay as coexisting connection paths

**Files:**

- Modify: `docs/mockups/mobile-relay-fullscreen-scenario.html`
- Modify: `docs/CODEX_HANDOFF.md`

- [x] **Step 1: Add independent transport and pairing state**

Replace the first flow state with `connection-method` and extend the prototype state with these exact fields:

```js
const FLOW = [
  'connection-method',
  'transport-connecting',
  'pairing-qr',
  'pairing-confirm',
  'session-list',
  'terminal',
  'recovery',
];

const state = {
  transportMode: 'tailscale',
  pairingTransport: 'tailscale',
};
```

`transportMode` accepts only `tailscale`, `relay`, or `both`. Selecting `tailscale` or `relay` also sets `pairingTransport` to the same value. Selecting `both` preserves the current valid `pairingTransport`, defaulting to `tailscale`.

- [x] **Step 2: Render connection method selection and independent status**

The first desktop screen must render a flat radio-style list with these three choices:

```js
const TRANSPORT_OPTIONS = [
  { id: 'tailscale', title: 'Tailscale만', note: '기존 Serve와 ts.net 주소를 그대로 사용' },
  { id: 'relay', title: 'Deppy Relay만', note: 'Tailscale 없이 외부 네트워크에서 접속' },
  { id: 'both', title: '둘 다 사용', note: '두 경로를 명시적으로 동시에 활성화' },
];
```

The screen must state that Relay is an addition, Tailscale is not removed, and a failed path never silently switches to the other path. The second state renders transport-specific progress:

```js
const TAILSCALE_PROGRESS = ['Tailscale 상태 확인', 'Serve 상태 확인', 'ts.net HTTPS 준비'];
const RELAY_PROGRESS = ['인터넷 연결 확인', 'Deppy 보안 릴레이 연결', '종단 간 암호화 채널 준비'];
```

When `transportMode === 'both'`, render two separately labelled progress groups and separate status results.

- [x] **Step 3: Preserve distinct pairing semantics**

The pairing screen must derive available tabs from the selected transport mode:

```js
function availablePairingTransports() {
  if (state.transportMode === 'both') return ['tailscale', 'relay'];
  return [state.transportMode];
}
```

Tailscale pairing shows the existing `https://mac.tail.example.ts.net/?token=••••` QR path and explains that it reuses the existing local pairing token behavior. It must not show a five-minute expiry or claim one-time credentials. Relay pairing shows the five-minute one-time QR, confirmation code, per-device permissions, expiry, and revocation.

`qr-scanned` transitions directly to `session-list` for Tailscale and to `pairing-confirm` for Relay. Directly opening `pairing-confirm` while Tailscale is selected renders a short “기존 Tailscale 인증 완료” state rather than Relay permission controls.

- [x] **Step 4: Show the active transport in shared session and connected views**

The connected desktop screen renders one status row per enabled transport. The mobile session header uses `transportLabel()` to show one of:

```js
function transportLabel() {
  if (state.transportMode === 'both') return 'Tailscale + Relay';
  return state.transportMode === 'tailscale' ? 'Tailscale' : 'Relay · 종단 간 암호화';
}
```

Session selection, full-viewport terminal, input, Back, reconnect, and recovery behavior remain transport-independent. The terminal header includes the same transport label.

- [x] **Step 5: Record the clarification, reopen, and commit**

Update `docs/CODEX_HANDOFF.md` with the coexistence behavior and explicit no-test status. Deliver the updated file with:

```bash
open docs/mockups/mobile-relay-fullscreen-scenario.html
```

Then stage only the plan and prototype and commit:

```bash
git add docs/superpowers/plans/2026-08-28-mobile-relay-fullscreen-prototype.md docs/mockups/mobile-relay-fullscreen-scenario.html
git commit -m "docs: Tailscale 공존 프로토타입 보완"
```

### Task 6: Correct authentication, permission, and mobile target-state defects

**Files:**

- Modify: `docs/mockups/mobile-relay-fullscreen-scenario.html`
- Modify: `docs/CODEX_HANDOFF.md`

- [x] **Step 1: Separate enabled transports from device authentication**

Add a `pairedTransports` record to the local state and update it only after the corresponding QR flow succeeds:

```js
pairedTransports: { tailscale: false, relay: false },
```

Tailscale QR completion marks only `tailscale`; Relay approval marks only `relay`. Connected device rows must derive from this record, while transport service rows continue to derive from `transportMode`.

- [x] **Step 2: Keep rejected and revoked devices outside the session list**

Relay rejection in dual mode returns to the Tailscale QR instead of assuming Tailscale authentication. Relay rejection or revocation in single mode shows a recovery screen whose only valid continuation is re-pairing. `recover-to-sessions` must ignore authentication-failure reasons, and the desktop recovery stage must not render a rejected or revoked device as connected.

- [x] **Step 3: Enforce the permission model in every surface**

Set the initial Relay permission state to view-only:

```js
permissions: { view: true, input: false, approval: false },
```

Hide the approval tab when `approval` is false, force `phoneSurface` back to `sessions` if approval is removed, and guard both the tab event and approval action event against unauthorized state.

- [x] **Step 4: Restore the 44px mobile touch-target contract**

Change every button-specific `min-height` override below 44px to 44px, including the compact device switcher and pairing transport tabs.

- [x] **Step 5: Record static verification and reopen the prototype**

Per the user's explicit prototype constraint, do not run automated tests, browser tests, HTML validators, or interaction automation. Perform read-only source inspection, update `docs/CODEX_HANDOFF.md`, and deliver with:

```bash
open docs/mockups/mobile-relay-fullscreen-scenario.html
```

Opening the file is delivery only and must not be reported as visual or functional verification. Do not commit unless the user explicitly asks.
