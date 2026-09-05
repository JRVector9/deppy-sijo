// Deppy Relay 신뢰 셸 — **보기 전용** 모바일 클라이언트.
//
// 이 문서가 하는 일과 하지 않는 일:
// - 한다: 페어링 링크 판독 → 즉시 URL 세탁 → 기기 신원 확보 → Relay 입장 → 종단 간 핸드셰이크
//   → 6자리 대조 코드 표시 → Mac 승인 대기 → 활성화 후에야 세션 화면 생성.
// - 하지 않는다: 세션 화면 마크업을 미리 만들어 두는 것. 채널이 활성화되기 전에는 세션/뷰포트
//   DOM이 **존재하지 않는다**.
// - 화면은 deppy-mux 모바일 PWA와 같은 구조다: 워크스페이스 카드 목록 → 터미널 화면(헤더·
//   DOM 행 렌더·⋯ 메뉴·비활성 작성기 바). 렌더러는 relay-terminal.js, 부품 스타일은
//   web/shared/mobile-theme.css.
// - 하지 않는다: 키·스크롤·워크스페이스 전환·승인 처리·파일 전송. 첫 릴리스의 기기 권한은
//   보기 전용이며, 아래 `OUTBOUND_MESSAGE_TYPES` 밖의 어떤 메시지도 만들 수 없다.
//   (Mac이 독립적으로 다시 막는다 — 여기 검사는 심층 방어일 뿐이다.)
//
// 최소 프로토콜 버전 강제는 **이 파일**의 몫이다. 서비스워커는 버전을 알지도, 판단하지도 않는다.

import {
  DIRECTION,
  FRAME_TYPE,
  HELLO_TAG,
  CONNECTION_ID_BYTES,
  IDENTITY_HELLO_BYTES,
  PROTOCOL_VERSION,
  ROLE,
  RelaySecureChannel,
  createEphemeral,
  decodeFrame,
  decodeIdentityHello,
  encodeOffer,
  deriveSessionMaterial,
  deriveSharedSecret,
  encodeFrame,
  encodeIdentityHello,
  encodeKnownDevice,
  encodePairingProof,
  importGcmKey,
  loadKnownDeviceId,
  loadOrCreateIdentity,
  parsePairingLink,
  rejectionCodeOf,
  resetIdentity,
  sha256,
  signTranscript,
  verifyPeerSignature,
  zeroBytes,
  buildTranscript,
} from "./relay-crypto.js";
import {
  TERMINAL_FONT_MAX_PX,
  TERMINAL_FONT_MIN_PX,
  TERMINAL_FONT_DEFAULT_PX,
  createTerminalView,
} from "./relay-terminal.js";

/// 이 셸이 받아들이는 가장 낮은 프로토콜 버전. 낡은 셸이 새 계약을 흉내 내지 못하게 하고,
/// 낡은 Mac이 이 셸을 끌어내리지도 못하게 한다.
export const MIN_PROTOCOL_VERSION = 1;

/// 이 셸이 **만들 수 있는** 메시지의 전부. 보기 전용 권한 행렬 그대로다.
export const OUTBOUND_MESSAGE_TYPES = Object.freeze(["watch", "unwatch", "request_keyframe"]);

/// 이 셸이 해석하는 수신 메시지의 전부.
export const INBOUND_MESSAGE_TYPES = Object.freeze(["dashboard", "viewport"]);

export const SCREEN = Object.freeze({
  BOOT: "boot",
  LINK: "link",
  IDENTITY: "identity",
  CONNECT: "connect",
  VERIFY: "verify",
  SESSION: "session",
  REVOKED: "revoked",
  RECOVERY: "recovery",
});

const ADMISSION_DEADLINE_MS = 15_000;
const HELLO_DEADLINE_MS = 20_000;
// Mac 사용자가 승인 버튼을 누를 때까지 기다리는 상한. 페어링 티켓 자체가 5분이다.
const APPROVAL_DEADLINE_MS = 5 * 60_000;
/// 생존 신호 주기. 서버는 **DRLY 프레임을 마지막으로 받은 시각**으로만 유휴를 판정하며
/// (WebSocket ping은 세지 않는다) 기본 유휴 상한이 60초다. 이 셸은 보기 전용이라 승인을
/// 기다리는 5분 동안 보낼 것이 하나도 없으므로, 이 신호가 없으면 60초에 끊기고 그 뒤 도착한
/// 승인은 죽은 세션의 채널이 되어 조용히 버려진다. 20초면 한 번을 통째로 놓쳐도(40초) 아직
/// 상한 안이다. Mac 쪽 `HEARTBEAT_INTERVAL_SECS`와 같은 값이다.
const HEARTBEAT_INTERVAL_MS = 20_000;

const ZERO_ID = new Uint8Array(16);

/// 이 접속의 연결 id는 **기기가 정한다**.
///
/// 서버는 `DeviceAdmission` 헤더의 값을 그대로 라우트 세션 id로 삼아 기기에게는
/// `Admitted`로, Mac에게는 `PeerJoined`로 되돌려 준다(relay-server `admit_device`).
/// 즉 세 당사자가 같은 값에 합의하는 유일한 출처가 이 난수다. 예전에는 셸이 `PeerJoined`가
/// 오기를 기다렸는데 **서버는 그 프레임을 Mac에게만 보낸다** — 그래서 기기는 연결 id를
/// 영영 받지 못하고 15초 입장 시한에 걸려 죽었다(2026-09-03 실측).
function newConnectionId() {
  const id = new Uint8Array(CONNECTION_ID_BYTES);
  crypto.getRandomValues(id);
  // 전0은 계약상 "연결 없음"(Mac이 입장 프레임에 쓰는 값)이라 세션 id가 될 수 없다.
  if (id.every((byte) => byte === 0)) id[0] = 1;
  return id;
}

/// 계약보다 낮거나 이 셸이 모르는 버전은 **거절**한다. 협상하지 않는다.
export function assertSupportedProtocolVersion(version) {
  if (!Number.isSafeInteger(version)) {
    throw new RangeError("프로토콜 버전이 정수가 아니다");
  }
  if (version < MIN_PROTOCOL_VERSION) {
    throw new RangeError(`프로토콜 버전 ${version}은 최소 ${MIN_PROTOCOL_VERSION} 미만이다`);
  }
  if (version > PROTOCOL_VERSION) {
    throw new RangeError(`프로토콜 버전 ${version}은 이 셸이 모른다`);
  }
  return version;
}

/// 주소에서 조각을 지운다. 네트워크가 한 바이트라도 움직이기 전에 불린다.
export function scrubLocation(target = globalThis) {
  const place = target.location;
  if (!place) return;
  try {
    target.history?.replaceState?.(null, "", place.pathname);
  } catch {
    // 히스토리 API가 막힌 환경에서도 조각만은 지운다.
  }
  if (place.hash) {
    try {
      place.hash = "";
    } catch {
      // 더 할 수 있는 일이 없다. 아래 검사가 사실을 그대로 보고한다.
    }
  }
}

/// 페어링 링크를 읽고 **즉시** 주소를 세탁한 뒤 판독 결과만 남긴다.
export function readPairingLinkFromLocation(target = globalThis) {
  const fragment = target.location?.hash ?? "";
  scrubLocation(target);
  return parsePairingLink(fragment);
}

/// 고정 Relay 오리진. 빌드가 심어 넣으며 `wss://`만 허용한다.
export function relayEndpoint(documentRef = document) {
  const value = documentRef.querySelector('meta[name="relay-origin"]')?.content?.trim() ?? "";
  if (!/^wss:\/\/[a-z0-9](?:[a-z0-9.-]*[a-z0-9])?(?::\d{1,5})?\/?$/i.test(value)) {
    throw new RangeError("Relay 오리진이 고정 wss:// 상수가 아니다");
  }
  return value;
}

// ── DOM 도우미 (innerHTML을 쓰지 않는다 — 문자열이 마크업이 되는 경로를 만들지 않는다) ────

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function byId(id) {
  return document.getElementById(id);
}

// ── 셸 본체 ────────────────────────────────────────────────────────────────────────

export function createShell({ endpoint, socketFactory }) {
  const state = {
    screen: SCREEN.BOOT,
    identity: null,
    knownDeviceId: null,
    link: null,
    socket: null,
    routeId: ZERO_ID,
    connectionId: null,
    /// 서버가 입장을 확인했는가. 연결 id는 입장 **전에** 정해지므로 시한 판정을
    /// 그 값으로 할 수 없다.
    admitted: false,
    ephemeral: null,
    pendingDesktopHello: null,
    channel: null,
    sas: null,
    watching: null,
    /// DOM 행 터미널 렌더러(relay-terminal.js). 세션 화면이 만들어진 뒤에만 있다.
    terminal: null,
    /// 마지막 대시보드. 터미널 화면의 세션 전환과 종료 감지가 이걸 본다.
    workspaces: [],
    selectedWorkspace: null,
    /// 세션별 마지막 상태 — 상태가 "기다림/승인 필요/종료"로 바뀔 때만 토스트를 띄운다.
    sessionStatus: new Map(),
    prefs: loadPrefs(),
    timers: new Set(),
    /// 생존 신호가 걸려 있는가. 마감 타이머와 달리 이 신호는 세션 내내 살아 있어야 하므로
    /// `clearTimers()`가 지운 뒤 다시 걸지 판단하는 데 쓴다.
    heartbeatArmed: false,
    finished: false,
  };

  function setScreen(screen, note) {
    state.screen = screen;
    document.body.dataset.screen = screen;
    const status = byId("shell-note");
    if (status) status.textContent = note ?? "";
  }

  function after(ms, body) {
    const handle = globalThis.setTimeout(() => {
      state.timers.delete(handle);
      body();
    }, ms);
    state.timers.add(handle);
    return handle;
  }

  function clearTimers() {
    for (const handle of state.timers) globalThis.clearTimeout(handle);
    state.timers.clear();
    // 마감 타이머는 여기서 사라지는 것이 맞지만 생존 신호는 다르다 — 소켓이 살아 있는 한
    // 다시 건다. `fail()`은 `finished`를 먼저 세우므로 여기서 되살아나지 않는다.
    if (state.heartbeatArmed && !state.finished) armHeartbeat();
  }

  /// 빈 `HEARTBEAT` 하나를 주기마다 보낸다. **조건 없이** 보낸다 — 서버는 어떤 종류든 받은
  /// 프레임마다 `last_seen_at`을 갱신하므로 남는 신호는 그저 무해하고(홉 단위라 상대에게
  /// 전달되지도 않는다), "최근에 뭔가 보냈나"를 따로 세면 송신 경로가 하나 늘 때마다 그
  /// 장부가 조용히 틀려진다. 헤더는 `sendFrame`이 만드는 그대로다: 입장 때 받은 라우트와
  /// 이 기기 세션의 연결 id(아직 없으면 0). 서버는 Heartbeat의 라우트만 확인한다.
  function armHeartbeat() {
    state.heartbeatArmed = true;
    after(HEARTBEAT_INTERVAL_MS, () => {
      if (state.finished || !sendFrame(FRAME_TYPE.HEARTBEAT)) {
        // 소켓이 닫혔다. 다시 걸지 않는다 — 소켓보다 오래 사는 타이머는 누수다.
        state.heartbeatArmed = false;
        return;
      }
      armHeartbeat();
    });
  }

  /// 어떤 실패든 여기로 모인다. 채널을 닫고, 세션 DOM을 걷어내고, 사유만 보여 준다.
  function fail(screen, note) {
    if (state.finished) return;
    state.finished = true;
    clearTimers();
    unmountSession();
    state.channel?.close();
    state.channel = null;
    try {
      state.socket?.close();
    } catch {
      // 이미 닫힌 소켓이다.
    }
    state.socket = null;
    setScreen(screen, note);
  }

  // ── 전송 ─────────────────────────────────────────────────────────────────────────

  function sendFrame(frameType, payload) {
    if (!state.socket || state.socket.readyState !== 1) return false;
    state.socket.send(
      encodeFrame({
        frameType,
        routeId: state.routeId,
        connectionId: state.connectionId ?? ZERO_ID,
        // 제어 프레임 시퀀스는 봉투 시퀀스와 무관하다. 암호문만 봉투 시퀀스를 쓴다.
        sequence: 0,
        payload,
      }),
    );
    return true;
  }

  /// 보기 전용 허용 목록 밖의 메시지는 **만들어질 수 없다**.
  async function sendMessage(message) {
    const type = message?.type;
    if (!OUTBOUND_MESSAGE_TYPES.includes(type)) {
      throw new RangeError(`이 셸이 보낼 수 없는 메시지다: ${String(type)}`);
    }
    if (!state.channel || state.channel.closed) return false;
    const sealed = await state.channel.seal(new TextEncoder().encode(JSON.stringify(message)));
    if (!state.socket || state.socket.readyState !== 1) return false;
    state.socket.send(
      encodeFrame({
        frameType: FRAME_TYPE.CIPHERTEXT,
        routeId: state.routeId,
        connectionId: state.connectionId,
        // DRLY 헤더 시퀀스 = 봉투 시퀀스. Mac 쪽 게이트가 이 동일성에 기대어 헤더를 만든다.
        sequence: sealed.sequence,
        payload: sealed.ciphertext,
      }),
    );
    return true;
  }

  // ── 세션 DOM (활성화 이후에만 존재한다) ─────────────────────────────────────────────

  function mountSession() {
    if (byId("relay-session")) return;
    const mount = byId("session-mount");
    if (!mount) return;
    const session = element("section", "session-host");
    session.id = "relay-session";

    // 토스트 — 세션 화면 위에 잠깐 떠서 상태 변화를 알린다.
    const toasts = element("div", "m-toast-stack");
    toasts.id = "relay-toasts";
    toasts.setAttribute("aria-live", "polite");
    session.append(toasts);

    // 진입 화면 — 워크스페이스 카드 목록.
    const home = element("section", "m-screen");
    home.id = "relay-home";
    const topbar = element("header", "m-topbar");
    const brand = element("div", "m-brand");
    brand.append(element("h1", "m-brand-title", "시조새"));
    const homeStatus = element("div", "m-brand-subtitle", "종단 간 암호화 · 보기 전용");
    homeStatus.id = "relay-home-status";
    brand.append(homeStatus);
    topbar.append(brand);
    const actions = element("div", "m-topbar-actions");
    const count = element("span", "m-pill");
    count.id = "relay-count";
    count.append(element("span", "m-dot green"), element("span", "", "0 workspaces"));
    actions.append(count, menuButton("relay-menu-button"));
    topbar.append(actions);
    home.append(topbar);
    const list = element("div", "m-card-list");
    list.id = "relay-workspaces";
    list.setAttribute("aria-label", "워크스페이스 목록");
    home.append(list);
    session.append(home);

    // 터미널 화면 — 활성화 뒤에도 카드를 눌러야 열린다.
    const terminal = element("section", "m-term-shell");
    terminal.id = "relay-terminal";
    terminal.hidden = true;
    const head = element("header", "m-term-head");
    const back = element("button", "m-icon-button small", "‹");
    back.type = "button";
    back.id = "term-back";
    back.setAttribute("aria-label", "워크스페이스 목록");
    back.addEventListener("click", () => {
      void stopWatching({ fromHistory: false });
    });
    const title = element("div", "m-term-title");
    const titleRow = element("div", "m-term-title-row");
    const name = element("span", "m-term-name", "");
    name.id = "term-name";
    titleRow.append(name);
    title.append(titleRow);
    const status = element("div", "m-term-status", "종단 간 암호화 · 보기 전용");
    status.id = "term-status";
    title.append(status);
    const termActions = element("div", "m-term-actions");
    const chip = element("button", "m-term-chip", "");
    chip.type = "button";
    chip.id = "term-session-chip";
    chip.setAttribute("aria-label", "다음 세션으로 전환");
    chip.addEventListener("click", () => {
      void cycleSession();
    });
    termActions.append(chip, menuButton("term-menu-button"));
    head.append(back, title, termActions);
    terminal.append(head);
    const viewport = element("div", "m-term-viewport");
    viewport.id = "relay-viewport";
    terminal.append(viewport);
    terminal.append(composerBar());
    session.append(terminal);

    session.append(menuPanel());
    mount.append(session);
    state.terminal = createTerminalView({
      mount: viewport,
      requestKeyframe: () => {
        void sendMessage({ type: "request_keyframe" }).catch(() => undefined);
      },
    });
    applyPrefs();
  }

  /// deppy-mux와 같은 작성기 바. 이 셸은 보기 전용이라 **전부 비활성**이며, 그 사실을 바 안의
  /// 안내가 말한다. 활성화하는 코드는 이 파일에 존재하지 않는다 — OUTBOUND_MESSAGE_TYPES 참조.
  function composerBar() {
    const bar = element("div", "m-composer-bar");
    bar.append(element("p", "m-composer-note", "보기 전용 — 입력·승인·파일 전송은 Mac에서만 합니다"));
    const composer = element("div", "m-composer");
    const attach = element("button", "m-icon-button small", "+");
    attach.type = "button";
    attach.disabled = true;
    attach.setAttribute("aria-label", "파일 첨부 (보기 전용에서는 불가)");
    const skills = element("button", "m-icon-button small", "/");
    skills.type = "button";
    skills.disabled = true;
    skills.setAttribute("aria-label", "스킬 (보기 전용에서는 불가)");
    const input = element("input");
    input.type = "text";
    input.disabled = true;
    input.placeholder = "보기 전용";
    input.setAttribute("aria-label", "입력 (보기 전용에서는 불가)");
    const send = element("button", "m-send-button", "↵");
    send.type = "button";
    send.disabled = true;
    send.setAttribute("aria-label", "전송 (보기 전용에서는 불가)");
    composer.append(attach, skills, input, send);
    bar.append(composer);
    return bar;
  }

  function menuButton(id) {
    const button = element("button", "m-icon-button round", "⋯");
    button.type = "button";
    button.id = id;
    button.setAttribute("aria-label", "메뉴");
    button.setAttribute("aria-expanded", "false");
    button.addEventListener("click", () => toggleMenu());
    return button;
  }

  /// ⋯ 메뉴 — deppy-mux의 항목을 보기 전용 범위에서 그대로 둔다: 연결 상태, 글꼴 크기,
  /// 폭 맞춤, 읽기 좋은 줄바꿈, 워크스페이스 목록, 버전.
  function menuPanel() {
    const panel = element("div", "m-menu");
    panel.id = "relay-menu";
    panel.hidden = true;

    const status = element("div", "m-menu-section");
    status.append(element("div", "label", "연결 상태"));
    const statusValue = element("div", "value", "연결됨 · 종단 간 암호화");
    statusValue.id = "menu-status";
    status.append(statusValue);
    status.append(element("div", "", "보기 전용 · 입력 없음"));
    panel.append(status);

    const list = element("button", "m-menu-button", "워크스페이스 목록");
    list.type = "button";
    list.id = "menu-workspaces";
    list.addEventListener("click", () => {
      closeMenu();
      void stopWatching({ fromHistory: false });
    });
    panel.append(list);

    const font = element("div", "m-menu-section");
    const fontRow = element("div", "m-menu-row");
    fontRow.append(element("span", "", "글꼴 크기"));
    const fontValue = element("span", "mono", `${state.prefs.fontSizePx}px`);
    fontValue.id = "menu-font-size";
    fontRow.append(fontValue);
    font.append(fontRow);
    const steps = element("div", "m-menu-steps");
    const smaller = element("button", "", "A-");
    smaller.type = "button";
    smaller.setAttribute("aria-label", "글꼴 줄이기");
    smaller.addEventListener("click", () => adjustFont(-1));
    const larger = element("button", "", "A+");
    larger.type = "button";
    larger.setAttribute("aria-label", "글꼴 키우기");
    larger.addEventListener("click", () => adjustFont(1));
    steps.append(smaller, larger);
    font.append(steps);
    panel.append(font);

    panel.append(
      toggleRow("menu-fit-width", "폭 맞춤", () => {
        state.prefs.fitWidth = !state.prefs.fitWidth;
        applyPrefs();
      }),
    );
    panel.append(
      toggleRow("menu-readable-wrap", "읽기 좋은 줄바꿈", () => {
        state.prefs.readableWrap = !state.prefs.readableWrap;
        applyPrefs();
      }),
    );
    panel.append(element("div", "m-menu-version", `시조새 원격 · 프로토콜 v${PROTOCOL_VERSION}`));
    return panel;
  }

  function toggleRow(id, label, onToggle) {
    const button = element("button", "m-menu-toggle");
    button.type = "button";
    button.id = id;
    button.setAttribute("aria-pressed", "false");
    button.append(element("span", "", label), element("span", "state", "OFF"));
    button.addEventListener("click", onToggle);
    return button;
  }

  function toggleMenu() {
    const panel = byId("relay-menu");
    if (!panel) return;
    if (panel.hidden) openMenu();
    else closeMenu();
  }

  function openMenu() {
    const panel = byId("relay-menu");
    if (!panel) return;
    panel.hidden = false;
    for (const id of ["relay-menu-button", "term-menu-button"]) {
      byId(id)?.setAttribute("aria-expanded", "true");
    }
    const inTerminal = state.watching !== null;
    const list = byId("menu-workspaces");
    if (list) list.hidden = !inTerminal;
  }

  function closeMenu() {
    const panel = byId("relay-menu");
    if (!panel) return;
    panel.hidden = true;
    for (const id of ["relay-menu-button", "term-menu-button"]) {
      byId(id)?.setAttribute("aria-expanded", "false");
    }
  }

  function adjustFont(delta) {
    state.prefs.fontSizePx = Math.min(
      TERMINAL_FONT_MAX_PX,
      Math.max(TERMINAL_FONT_MIN_PX, state.prefs.fontSizePx + delta),
    );
    applyPrefs();
  }

  function applyPrefs() {
    const prefs = state.prefs;
    state.terminal?.setFontSize(prefs.fontSizePx);
    state.terminal?.setFitWidth(prefs.fitWidth);
    state.terminal?.setReadableWrap(prefs.readableWrap);
    const fontValue = byId("menu-font-size");
    if (fontValue) fontValue.textContent = `${prefs.fontSizePx}px`;
    for (const [id, enabled] of [
      ["menu-fit-width", prefs.fitWidth],
      ["menu-readable-wrap", prefs.readableWrap],
    ]) {
      const button = byId(id);
      if (!button) continue;
      button.setAttribute("aria-pressed", enabled ? "true" : "false");
      const stateLabel = button.querySelector(".state");
      if (stateLabel) stateLabel.textContent = enabled ? "ON" : "OFF";
    }
    savePrefs(prefs);
  }

  function unmountSession() {
    state.terminal?.destroy?.();
    state.terminal = null;
    state.watching = null;
    state.selectedWorkspace = null;
    byId("relay-session")?.remove();
  }

  function toast(title, body, kind) {
    const stack = byId("relay-toasts");
    if (!stack) return;
    const node = element("div", `m-toast visible${kind ? ` ${kind}` : ""}`);
    node.setAttribute("role", "status");
    node.append(element("span", `m-dot ${kind === "success" ? "green" : "amber"}`));
    const text = element("span");
    text.append(element("span", "m-toast-title", title));
    if (body) text.append(element("span", "m-toast-body", body));
    node.append(text);
    stack.append(node);
    after(2200, () => node.remove());
  }

  // ── 대시보드 → 카드 ────────────────────────────────────────────────────────────

  const STATUS_LABEL = Object.freeze({
    running: "실행 중",
    waiting: "입력 대기",
    needs_approval: "승인 필요",
    error: "오류",
    done: "종료",
    idle: "대기",
  });

  function statusOf(view) {
    if (view.exited) return "done";
    return typeof view.status === "string" ? view.status : null;
  }

  function dotFor(status) {
    switch (status) {
      case "running":
        return "green";
      case "waiting":
      case "needs_approval":
        return "amber";
      case "error":
        return "red";
      default:
        return "";
    }
  }

  function watchable(workspace) {
    return (Array.isArray(workspace?.sessions) ? workspace.sessions : []).filter(
      (view) => typeof view.id === "string" && view.id.length > 0 && !view.exited,
    );
  }

  function renderDashboard(message) {
    const workspaces = Array.isArray(message.workspaces) ? message.workspaces : [];
    state.workspaces = workspaces;
    noticeStatusChanges(workspaces);

    const count = byId("relay-count");
    if (count) {
      const label = count.lastChild;
      if (label) label.textContent = `${workspaces.length} workspaces`;
    }
    const list = byId("relay-workspaces");
    if (list) {
      list.replaceChildren();
      if (workspaces.length === 0) {
        const note = element("div", "m-panel-note");
        note.append(element("strong", "", "표시할 워크스페이스가 없습니다"));
        note.append(document.createTextNode("Mac에서 워크스페이스를 열면 여기에 나타납니다."));
        list.append(note);
      }
      for (const workspace of workspaces) list.append(workspaceCard(workspace));
    }

    // 보던 세션이 사라졌거나 끝났으면 터미널을 닫는다 — 빈 화면을 붙들고 있지 않는다.
    if (state.watching) {
      const stillThere = workspaces
        .flatMap((workspace) => watchable(workspace))
        .some((view) => view.id === state.watching);
      if (!stillThere) {
        toast("세션이 종료되었습니다", "워크스페이스 목록으로 돌아왔습니다");
        void stopWatching({ fromHistory: false });
      } else if (state.selectedWorkspace) {
        const fresh = workspaces.find((workspace) => workspace.id === state.selectedWorkspace.id);
        if (fresh) {
          state.selectedWorkspace = fresh;
          refreshTerminalHeader();
        }
      }
    }
  }

  /// 상태가 "입력 대기/승인 필요/종료"로 **바뀐** 세션만 알린다. 첫 대시보드는 기준선이다.
  function noticeStatusChanges(workspaces) {
    const initialized = state.sessionStatus.size > 0 || state.sessionStatus.has("__init__");
    const next = new Map([["__init__", "seen"]]);
    for (const workspace of workspaces) {
      for (const view of Array.isArray(workspace.sessions) ? workspace.sessions : []) {
        if (typeof view.id !== "string") continue;
        const status = statusOf(view);
        next.set(view.id, status);
        const previous = state.sessionStatus.get(view.id);
        if (
          initialized &&
          status !== previous &&
          (status === "waiting" || status === "needs_approval" || status === "done")
        ) {
          toast(
            String(view.title ?? "세션"),
            `${STATUS_LABEL[status]}${view.agent ? ` · ${String(view.agent)}` : ""}`,
            status === "done" ? "success" : "",
          );
        }
      }
    }
    state.sessionStatus = next;
  }

  function workspaceCard(workspace) {
    const sessions = Array.isArray(workspace.sessions) ? workspace.sessions : [];
    const openable = watchable(workspace);
    const card = element("button", `m-card${openable.length === 0 ? " idle" : ""}`);
    card.type = "button";
    card.dataset.workspace = String(workspace.id ?? "");
    const body = element("span", "m-card-body");
    body.append(element("span", "m-card-name", String(workspace.name ?? workspace.id ?? "")));
    const completed = sessions.some((view) => statusOf(view) === "done" || statusOf(view) === "waiting");
    if (completed) body.append(element("span", "m-done-tag", "완료"));
    const agents = sessions
      .map((view) => (typeof view.agent === "string" ? view.agent : ""))
      .filter((agent, index, all) => agent && all.indexOf(agent) === index);
    body.append(
      element(
        "span",
        "m-card-path",
        agents.length > 0
          ? agents.join(" · ")
          : workspace.state === "active"
            ? "활성 워크스페이스"
            : workspace.state === "warm"
              ? "대기 중 — Mac에서 전환해야 볼 수 있습니다"
              : "절전 중 — Mac에서 깨워야 볼 수 있습니다",
      ),
    );
    const chips = element("span", "m-chip-row");
    for (const view of sessions.slice(0, 3)) {
      const chip = element("span", "m-chip");
      chip.dataset.session = typeof view.id === "string" ? view.id : "";
      chip.append(element("span", `m-dot ${dotFor(statusOf(view))}`));
      chip.append(element("span", "", String(view.title ?? "세션")));
      chips.append(chip);
    }
    if (sessions.length > 3) chips.append(element("span", "m-chip", `+${sessions.length - 3}`));
    body.append(chips);
    card.append(body);
    card.append(element("span", "m-badge-count", String(sessions.length)));
    card.addEventListener("click", () => {
      if (openable.length === 0) {
        toast("이 워크스페이스는 볼 수 없습니다", "Mac에서 활성 워크스페이스로 전환하세요");
        return;
      }
      void openWorkspace(workspace, openable[0]);
    });
    return card;
  }

  // ── 터미널 화면 ──────────────────────────────────────────────────────────────────

  function refreshTerminalHeader() {
    const workspace = state.selectedWorkspace;
    const name = byId("term-name");
    if (name) name.textContent = String(workspace?.name ?? "");
    const chip = byId("term-session-chip");
    if (chip) {
      const openable = watchable(workspace);
      const current = openable.find((view) => view.id === state.watching);
      chip.textContent = String(current?.title ?? "세션");
      chip.disabled = openable.length < 2;
    }
  }

  async function openWorkspace(workspace, view) {
    const terminal = byId("relay-terminal");
    const home = byId("relay-home");
    if (!terminal || !home || !state.terminal) return;
    closeMenu();
    state.selectedWorkspace = workspace;
    home.hidden = true;
    terminal.hidden = false;
    document.body.dataset.stage = "open";
    try {
      globalThis.history?.pushState?.({ relayStage: true }, "");
    } catch {
      // 히스토리를 못 써도 화면 전환 자체는 유효하다.
    }
    await watchSession(view);
  }

  async function watchSession(view) {
    state.watching = view.id;
    state.terminal?.watch(view.id);
    refreshTerminalHeader();
    // Mac은 접속당 시청 하나만 둔다 — 새 watch가 이전 시청을 대체한다.
    await sendMessage({ type: "watch", session: view.id }).catch(() => undefined);
  }

  async function cycleSession() {
    const openable = watchable(state.selectedWorkspace);
    if (openable.length < 2) return;
    const index = openable.findIndex((view) => view.id === state.watching);
    await watchSession(openable[(index + 1) % openable.length]);
  }

  async function startWatching(session, label) {
    // 카드 밖에서 세션 id로 곧장 여는 경로(테스트·알림 딥링크). 그 세션이 속한 워크스페이스를 찾는다.
    const workspace = state.workspaces.find((candidate) =>
      watchable(candidate).some((view) => view.id === session),
    );
    if (!workspace) return;
    const view = watchable(workspace).find((candidate) => candidate.id === session);
    await openWorkspace(workspace, { ...view, title: label ?? view.title });
  }

  /// 터미널 화면을 닫고 목록으로. 뒤로 가기(popstate)로 왔으면 히스토리는 이미 소비됐다.
  async function stopWatching({ fromHistory }) {
    if (state.watching === null) return;
    closeMenu();
    const watched = state.watching;
    state.watching = null;
    state.terminal?.unwatch();
    const terminal = byId("relay-terminal");
    const home = byId("relay-home");
    if (terminal) terminal.hidden = true;
    if (home) home.hidden = false;
    document.body.dataset.stage = "closed";
    if (!fromHistory) {
      try {
        if (globalThis.history?.state?.relayStage) globalThis.history.back();
      } catch {
        // 뒤로 가기가 막혀도 시청은 이미 끊었다.
      }
    }
    if (watched) await sendMessage({ type: "unwatch" }).catch(() => undefined);
  }

  // ── 수신 ─────────────────────────────────────────────────────────────────────────

  function handlePlaintext(bytes) {
    let message;
    try {
      message = JSON.parse(new TextDecoder().decode(bytes));
    } catch {
      return;
    }
    if (!INBOUND_MESSAGE_TYPES.includes(message?.type)) return;
    if (message.type === "dashboard") {
      renderDashboard(message);
      return;
    }
    // 다른 세션의 잔여 프레임은 렌더러가 스스로 버린다.
    state.terminal?.applyViewport(message);
  }

  async function activate() {
    if (state.screen === SCREEN.SESSION) return;
    clearTimers();
    mountSession();
    setScreen(SCREEN.SESSION, "");
  }

  async function handleCiphertext(frame) {
    if (!state.channel) return;
    let plaintext;
    try {
      plaintext = await state.channel.open({
        sequence: frame.sequence,
        direction: DIRECTION.DESKTOP_TO_DEVICE,
        ciphertext: frame.payload,
      });
    } catch (error) {
      fail(SCREEN.RECOVERY, `암호 채널이 끊겼습니다 (${error.message})`);
      return;
    }
    await activate();
    handlePlaintext(plaintext);
  }

  /// 데스크톱 hello 하나를 소화한다. 서명 검증 → 우리 hello 서명·송신 → 키 파생 → 증명 제출.
  ///
  /// **주의(계약 미해결):** Task 1의 서명은 양쪽 offer를 모두 묶은 transcript 위에 있다.
  /// 따라서 어느 쪽도 상대 임시키를 보기 전에는 서명할 수 없다. 이 셸은 데스크톱 hello를
  /// 먼저 받고 나서 자기 hello를 보낸다(도착이 준비보다 빨라도 버퍼링한다). Mac 쪽이 같은
  /// 규칙으로 기다리면 교착이므로, 누가 먼저 보내는지는 Mac 구현과 맞춰야 한다.
  async function consumeDesktopHello(payload) {
    if (payload.length !== IDENTITY_HELLO_BYTES || payload[0] !== HELLO_TAG.IDENTITY) {
      fail(SCREEN.RECOVERY, "Mac이 보낸 첫 레코드가 계약과 다릅니다.");
      return;
    }
    let hello;
    try {
      hello = decodeIdentityHello(payload);
      assertSupportedProtocolVersion(hello.protocolVersion);
    } catch (error) {
      fail(SCREEN.RECOVERY, `프로토콜 버전을 받아들일 수 없습니다 (${error.message})`);
      return;
    }
    if (hello.role !== ROLE.DESKTOP) {
      fail(SCREEN.RECOVERY, "상대 역할이 데스크톱이 아닙니다.");
      return;
    }
    if (hexOf(hello.connectionId) !== hexOf(state.connectionId)) {
      fail(SCREEN.RECOVERY, "연결 식별자가 어긋났습니다.");
      return;
    }

    const transcript = buildTranscript({
      protocolVersion: PROTOCOL_VERSION,
      connectionId: state.connectionId,
      desktop: { identitySec1: hello.identitySec1, ephemeralSec1: hello.ephemeralSec1 },
      device: {
        identitySec1: state.identity.publicSec1,
        ephemeralSec1: state.ephemeral.publicSec1,
      },
    });
    const verified = await verifyPeerSignature(hello.identitySec1, hello.signature, transcript);
    if (!verified) {
      fail(SCREEN.RECOVERY, "Mac의 서명을 확인하지 못했습니다.");
      return;
    }

    const signature = await signTranscript(state.identity, transcript);
    sendFrame(
      FRAME_TYPE.HELLO,
      encodeIdentityHello({
        protocolVersion: PROTOCOL_VERSION,
        role: ROLE.DEVICE,
        connectionId: state.connectionId,
        identitySec1: state.identity.publicSec1,
        ephemeralSec1: state.ephemeral.publicSec1,
        signature,
      }),
    );

    const shared = await deriveSharedSecret(state.ephemeral.privateKey, hello.ephemeralSec1);
    const material = await deriveSessionMaterial(shared, transcript);
    zeroBytes(shared);
    const desktopToDevice = await importGcmKey(material.desktopToDeviceKeyBytes);
    const deviceToDesktop = await importGcmKey(material.deviceToDesktopKeyBytes);
    zeroBytes(material.desktopToDeviceKeyBytes);
    zeroBytes(material.deviceToDesktopKeyBytes);

    state.channel = new RelaySecureChannel({
      protocolVersion: PROTOCOL_VERSION,
      connectionId: state.connectionId,
      sendKey: deviceToDesktop,
      receiveKey: desktopToDevice,
      sendDirection: DIRECTION.DEVICE_TO_DESKTOP,
      receiveDirection: DIRECTION.DESKTOP_TO_DEVICE,
      ownFingerprint: state.identity.fingerprint,
      peerFingerprint: await sha256(hello.identitySec1),
    });

    state.sas = material.sas;
    showVerification(material.sas);

    const transcriptHash = await sha256(transcript);
    const known = state.knownDeviceId;
    if (known) {
      sendFrame(FRAME_TYPE.HELLO, encodeKnownDevice(known));
    } else {
      sendFrame(
        FRAME_TYPE.HELLO,
        await encodePairingProof({
          pairingId: state.link.pairingId,
          pairingSecret: state.link.pairingSecret,
          transcriptHash,
          connectionId: state.connectionId,
          deviceFingerprint: state.identity.fingerprint,
        }),
      );
    }
    // 페어링 비밀은 증명 계산 직후 지운다. 이 문서에는 더 이상 존재하지 않는다.
    zeroBytes(state.link.pairingSecret);

    clearTimers();
    after(APPROVAL_DEADLINE_MS, () => {
      if (state.screen !== SCREEN.SESSION) {
        fail(SCREEN.RECOVERY, "Mac에서 승인되지 않아 페어링이 만료됐습니다.");
      }
    });
  }

  function showVerification(sas) {
    setScreen(SCREEN.VERIFY, "");
    const code = byId("verify-code");
    if (code) {
      code.dataset.digits = sas;
      code.textContent = `${sas.slice(0, 3)} ${sas.slice(3)}`;
    }
  }

  async function handleFrame(frame) {
    switch (frame.frameType) {
      case FRAME_TYPE.ADMITTED: {
        // 서버는 우리가 선언한 연결 id를 그대로 되돌려 준다. 다른 값이 오면 이 접속이
        // 우리 세션이 아니라는 뜻이라 더 진행하지 않는다.
        if (hexOf(frame.connectionId) !== hexOf(state.connectionId)) {
          fail(SCREEN.RECOVERY, "릴레이가 다른 연결 식별자를 돌려줬습니다.");
          return;
        }
        state.routeId = frame.routeId;
        state.admitted = true;
        // 라우트에 입장한 **뒤에야** 서버가 Heartbeat를 받아 준다. 입장 전에 보내면 계약
        // 위반으로 그 자리에서 끊긴다.
        if (!state.heartbeatArmed) armHeartbeat();
        // 라우트는 Mac이 먼저 소유해야 존재하므로, 입장이 허가된 시점에 상대는 이미 있다
        // (`PeerJoined`는 그 사실을 **Mac에게** 알리는 프레임이지 우리에게 오지 않는다).
        // 그러니 여기서 바로 문을 연다.
        state.ephemeral = await createEphemeral();
        // 서명은 양쪽 offer를 덮으므로 어느 쪽도 먼저 서명할 수 없다. 기기가 서명 없는
        // Offer로 문을 열고, Mac이 그에 서명한 hello로 답하면 그때 기기도 서명한다.
        sendFrame(
          FRAME_TYPE.HELLO,
          encodeOffer({
            protocolVersion: PROTOCOL_VERSION,
            role: ROLE.DEVICE,
            connectionId: state.connectionId,
            identitySec1: state.identity.publicSec1,
            ephemeralSec1: state.ephemeral.publicSec1,
          }),
        );
        setScreen(SCREEN.CONNECT, "릴레이에 입장했습니다. Mac을 기다리는 중…");
        clearTimers();
        after(HELLO_DEADLINE_MS, () => {
          if (!state.channel) fail(SCREEN.RECOVERY, "Mac이 응답하지 않습니다.");
        });
        const buffered = state.pendingDesktopHello;
        state.pendingDesktopHello = null;
        if (buffered) await consumeDesktopHello(buffered);
        return;
      }
      case FRAME_TYPE.HELLO:
        if (state.channel) return;
        if (!state.ephemeral || !state.connectionId) {
          state.pendingDesktopHello = frame.payload;
          return;
        }
        await consumeDesktopHello(frame.payload);
        return;
      case FRAME_TYPE.CIPHERTEXT:
        await handleCiphertext(frame);
        return;
      case FRAME_TYPE.HEARTBEAT:
        return;
      case FRAME_TYPE.PEER_LEFT:
        fail(SCREEN.RECOVERY, "Mac과의 연결이 끊겼습니다.");
        return;
      case FRAME_TYPE.REJECTED:
      case FRAME_TYPE.CLOSE: {
        const reason = rejectionCodeOf(frame);
        const revoked = reason === "credential-rejected" || reason === "ticket-consumed";
        fail(
          revoked ? SCREEN.REVOKED : SCREEN.RECOVERY,
          revoked ? "이 기기의 접근이 해제됐습니다." : `릴레이가 연결을 닫았습니다 (${reason ?? "코드 없음"})`,
        );
        return;
      }
      default:
        // 이 전송에서 의미 없는 프레임이다. 조용히 버린다.
        break;
    }
  }

  function open() {
    const socket = socketFactory ? socketFactory(endpoint) : new WebSocket(endpoint);
    socket.binaryType = "arraybuffer";
    state.socket = socket;
    socket.addEventListener("open", () => {
      // 입장 프레임 **헤더**가 이 세션의 연결 id를 선언한다. `sendFrame`이 그 값을 쓴다.
      state.connectionId = newConnectionId();
      sendFrame(FRAME_TYPE.DEVICE_ADMISSION, state.link.admissionHandle);
      // 입장 핸들은 1회용이다. 제출 직후 이 문서에서 지운다.
      zeroBytes(state.link.admissionHandle);
      setScreen(SCREEN.CONNECT, "릴레이에 입장하는 중…");
      after(ADMISSION_DEADLINE_MS, () => {
        if (!state.admitted) fail(SCREEN.RECOVERY, "릴레이가 입장을 확인하지 않았습니다.");
      });
    });
    socket.addEventListener("message", (event) => {
      if (!(event.data instanceof ArrayBuffer)) return;
      let decoded;
      try {
        decoded = decodeFrame(event.data);
      } catch {
        return;
      }
      if (decoded.consumed !== event.data.byteLength) return;
      void handleFrame(decoded.frame).catch((error) => {
        fail(SCREEN.RECOVERY, `처리할 수 없는 프레임입니다 (${error.message})`);
      });
    });
    socket.addEventListener("close", () => {
      fail(SCREEN.RECOVERY, "릴레이 연결이 끊어졌습니다.");
    });
    socket.addEventListener("error", () => {
      fail(SCREEN.RECOVERY, "릴레이에 연결할 수 없습니다.");
    });
  }

  async function start() {
    setScreen(SCREEN.BOOT, "");
    state.link = readPairingLinkFromLocation();
    try {
      state.identity = await loadOrCreateIdentity();
    } catch (error) {
      setScreen(SCREEN.IDENTITY, `이 기기의 신원을 쓸 수 없습니다 (${error.reason ?? "unknown"})`);
      return;
    }
    state.knownDeviceId = await loadKnownDeviceId().catch(() => null);
    if (!state.link) {
      setScreen(SCREEN.LINK, "");
      return;
    }
    open();
  }

  return { start, state, sendMessage, stopWatching, startWatching };
}

// ── 표시 설정 (기기 로컬, 비밀 아님) ───────────────────────────────────────────────

const PREFS_KEY = "deppy-relay-shell:prefs";

function loadPrefs() {
  const prefs = { fontSizePx: TERMINAL_FONT_DEFAULT_PX, fitWidth: true, readableWrap: false };
  try {
    const raw = globalThis.localStorage?.getItem(PREFS_KEY);
    const stored = raw ? JSON.parse(raw) : null;
    if (Number.isInteger(stored?.fontSizePx)) {
      prefs.fontSizePx = Math.min(
        TERMINAL_FONT_MAX_PX,
        Math.max(TERMINAL_FONT_MIN_PX, stored.fontSizePx),
      );
    }
    if (typeof stored?.fitWidth === "boolean") prefs.fitWidth = stored.fitWidth;
    if (typeof stored?.readableWrap === "boolean") prefs.readableWrap = stored.readableWrap;
  } catch {
    // 저장소가 막혀 있어도 기본값으로 동작한다.
  }
  return prefs;
}

function savePrefs(prefs) {
  try {
    globalThis.localStorage?.setItem(PREFS_KEY, JSON.stringify(prefs));
  } catch {
    // 저장 실패는 표시 설정을 잃을 뿐이다.
  }
}

function hexOf(bytes) {
  return Array.from(bytes ?? [], (byte) => byte.toString(16).padStart(2, "0")).join("");
}

// ── 부팅 ───────────────────────────────────────────────────────────────────────────
//
// 하니스 문서가 이 모듈의 순수 함수만 쓰고 싶을 때는 `#relay-shell-root`가 없으므로 아무 일도
// 일어나지 않는다. 프로덕션 문서에만 그 요소가 있다.

function attachChrome(shell) {
  byId("reset-identity")?.addEventListener("click", () => {
    void resetIdentity()
      .then(() => globalThis.location.reload())
      .catch(() => {
        const note = byId("shell-note");
        if (note) note.textContent = "신원을 초기화하지 못했습니다.";
      });
  });
  for (const button of document.querySelectorAll("[data-restart]")) {
    button.addEventListener("click", () => {
      globalThis.location.reload();
    });
  }
  globalThis.addEventListener("popstate", () => {
    void shell.stopWatching({ fromHistory: true });
  });
  // 프라이버시 커튼 — 앱이 뒤로 가면 화면 내용을 즉시 가린다.
  document.addEventListener("visibilitychange", () => {
    document.body.dataset.curtain = document.visibilityState === "hidden" ? "on" : "off";
  });
}

if (typeof document !== "undefined" && document.getElementById("relay-shell-root")) {
  let shell = null;
  try {
    shell = createShell({ endpoint: relayEndpoint() });
  } catch (error) {
    document.body.dataset.screen = SCREEN.RECOVERY;
    const note = document.getElementById("shell-note");
    if (note) note.textContent = `이 셸의 릴레이 설정이 올바르지 않습니다 (${error.message})`;
  }
  if (shell) {
    attachChrome(shell);
    void shell.start();
  }
  if ("serviceWorker" in navigator) {
    // 정적 자산만 캐시한다. 실패해도 셸 동작에는 영향이 없다.
    navigator.serviceWorker.register("./sw.js").catch(() => undefined);
  }
}
