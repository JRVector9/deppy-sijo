// 프로덕션 셸 모듈(web/relay-shell/relay-crypto.js, relay-shell.js)을 **그대로 import**해
// 격리된 실제 브라우저에서 Rust 고정 벡터(relay-hello-v1.json)와 대조하고, 가짜 소켓 위에서
// 페어링 전 과정을 끝까지 돌린다. 복사본 구현은 없다 — 여기서 검증되는 것이 배포되는 것이다.
//
// 1부 벡터: offer/hello 레코드 바이트, transcript, Mac 서명 검증, 기기 서명 생성(Rust가 검증),
//   SAS, 페어링 증명, 기존 기기 레코드, 페어링 링크 판독, 거절 목록.
// 2부 흐름: 링크 판독 → 입장 → Offer → (Mac 역할을 원시 요소로 수행) 서명 hello 교환 → 증명
//   → SAS 표시 → 첫 암호문에 세션 활성화 → 보기 전용 송신 → 재생 시 폐쇄.

import * as C from "./relay-crypto.js";
import { SCREEN, createShell } from "./relay-shell.js";

const fixture = JSON.parse(document.getElementById("relay-fixture").textContent);
const hex = C.bytesFromHex;
const toHex = C.hexFromBytes;
const ZERO16 = new Uint8Array(16);

// 러너 자신의 대기는 **진짜** 타이머로 한다. 아래 가짜 시계가 globalThis를 갈아 끼워도
// waitFor가 함께 멈추면 안 된다.
const realSetTimeout = globalThis.setTimeout.bind(globalThis);

// 셸의 타이머만 가로채는 가짜 시계. 생존 신호는 20초 주기라 실제로 기다릴 수 없다 —
// 셸이 `after()`로 거는 모든 타이머를 여기 모아 두고 시간을 손으로 민다.
const clock = { now: 0, next: 1, scheduled: new Map() };

function installFakeClock() {
  globalThis.setTimeout = (body, ms) => {
    const handle = clock.next;
    clock.next += 1;
    clock.scheduled.set(handle, { due: clock.now + (Number(ms) || 0), body });
    return handle;
  };
  globalThis.clearTimeout = (handle) => {
    clock.scheduled.delete(handle);
  };
}

/// 시간을 `ms`만큼 민다. 마감이 지난 타이머를 이른 순서대로 한 번씩 부른다.
function advanceClock(ms) {
  clock.now += ms;
  const due = [...clock.scheduled.entries()]
    .filter(([, entry]) => entry.due <= clock.now)
    .sort((left, right) => left[1].due - right[1].due);
  for (const [handle, entry] of due) {
    if (!clock.scheduled.delete(handle)) continue;
    entry.body();
  }
}

function done(status, text) {
  document.body.dataset.status = status;
  document.body.textContent = text;
}
function eq(actual, expected, label) {
  if (actual !== expected) throw new Error(`${label}: expected ${expected}, got ${actual}`);
}
function ok(condition, label) {
  if (!condition) throw new Error(label);
}
async function waitFor(condition, label, attempts = 800) {
  for (let index = 0; index < attempts; index += 1) {
    if (condition()) return;
    await new Promise((resolve) => realSetTimeout(resolve, 5));
  }
  throw new Error(`timeout: ${label}`);
}
function jwkFromScalar(publicSec1Hex, privateHex) {
  const pub = hex(publicSec1Hex);
  return {
    kty: "EC",
    crv: "P-256",
    x: C.bytesToBase64Url(pub.slice(1, 33)),
    y: C.bytesToBase64Url(pub.slice(33, 65)),
    d: C.bytesToBase64Url(hex(privateHex)),
  };
}
async function importIdentity(publicSec1Hex, privateHex) {
  const privateKey = await crypto.subtle.importKey(
    "jwk",
    jwkFromScalar(publicSec1Hex, privateHex),
    { name: "ECDSA", namedCurve: "P-256" },
    false,
    ["sign"],
  );
  const publicSec1 = hex(publicSec1Hex);
  return { privateKey, publicSec1, fingerprint: await C.sha256(publicSec1) };
}

// ── 1부: 고정 벡터 ─────────────────────────────────────────────────────────────────

async function vectors() {
  const desktop = fixture.desktop;
  const device = fixture.device;
  const proofFixture = fixture.pairing_proof;
  const protocolVersion = fixture.protocol_version;
  const connectionId = hex(fixture.connection_id_hex);

  eq(C.PROTOCOL_VERSION, protocolVersion, "protocol version");
  eq(C.OFFER_RECORD_BYTES, fixture.record_bytes.offer, "offer bytes");
  eq(C.IDENTITY_HELLO_BYTES, fixture.record_bytes.identity_hello, "identity hello bytes");
  eq(C.PAIRING_PROOF_BYTES, fixture.record_bytes.pairing_proof, "pairing proof bytes");
  eq(C.KNOWN_DEVICE_BYTES, fixture.record_bytes.known_device, "known device bytes");
  eq(C.HELLO_TAG.OFFER, 0, "offer tag");
  eq(C.HELLO_TAG.IDENTITY, fixture.tags.identity_hello, "identity tag");
  eq(C.HELLO_TAG.PAIRING_PROOF, fixture.tags.pairing_proof, "proof tag");
  eq(C.HELLO_TAG.KNOWN_DEVICE, fixture.tags.known_device, "known tag");
  eq(C.ROLE.DESKTOP, fixture.roles.desktop, "desktop role");
  eq(C.ROLE.DEVICE, fixture.roles.device, "device role");
  eq(C.PAIRING_LINK_BYTES, 80, "link bytes");

  eq(
    toHex(
      C.encodeOffer({
        protocolVersion,
        role: C.ROLE.DEVICE,
        connectionId,
        identitySec1: hex(device.identity_public_sec1_hex),
        ephemeralSec1: hex(device.ephemeral_public_sec1_hex),
      }),
    ),
    device.offer_record_hex,
    "device offer record",
  );
  eq(
    toHex(
      C.encodeOffer({
        protocolVersion,
        role: C.ROLE.DESKTOP,
        connectionId,
        identitySec1: hex(desktop.identity_public_sec1_hex),
        ephemeralSec1: hex(desktop.ephemeral_public_sec1_hex),
      }),
    ),
    desktop.offer_record_hex,
    "desktop offer record",
  );

  const desktopHello = C.decodeIdentityHello(hex(desktop.identity_hello_record_hex));
  eq(desktopHello.protocolVersion, protocolVersion, "desktop hello version");
  eq(desktopHello.role, C.ROLE.DESKTOP, "desktop hello role");
  eq(toHex(desktopHello.connectionId), fixture.connection_id_hex, "desktop hello connection");
  eq(toHex(desktopHello.identitySec1), desktop.identity_public_sec1_hex, "desktop hello identity");
  eq(toHex(desktopHello.ephemeralSec1), desktop.ephemeral_public_sec1_hex, "desktop hello ephemeral");
  eq(toHex(desktopHello.signature), desktop.signature_raw_hex, "desktop hello signature");

  const transcript = C.buildTranscript({
    protocolVersion,
    connectionId,
    desktop: { identitySec1: desktopHello.identitySec1, ephemeralSec1: desktopHello.ephemeralSec1 },
    device: {
      identitySec1: hex(device.identity_public_sec1_hex),
      ephemeralSec1: hex(device.ephemeral_public_sec1_hex),
    },
  });
  eq(toHex(transcript), fixture.transcript_hex, "transcript");
  eq(toHex(await C.sha256(transcript)), fixture.transcript_hash_hex, "transcript hash");
  ok(
    await C.verifyPeerSignature(desktopHello.identitySec1, desktopHello.signature, transcript),
    "desktop signature must verify",
  );
  const tampered = new Uint8Array(desktopHello.signature);
  tampered[63] ^= 0x01;
  ok(
    !(await C.verifyPeerSignature(desktopHello.identitySec1, tampered, transcript)),
    "tampered desktop signature must fail",
  );
  const deviceHello = C.decodeIdentityHello(hex(device.identity_hello_record_hex));
  ok(
    await C.verifyPeerSignature(deviceHello.identitySec1, deviceHello.signature, transcript),
    "fixture device signature must verify",
  );

  // 브라우저가 고정 기기 신원으로 서명한다 — Rust가 이 서명을 검증한다.
  const deviceIdentity = await importIdentity(
    device.identity_public_sec1_hex,
    device.identity_private_scalar_hex,
  );
  eq(toHex(deviceIdentity.fingerprint), device.identity_fingerprint_hex, "device fingerprint");
  const signature = await C.signTranscript(deviceIdentity, transcript);
  const signedRecord = C.encodeIdentityHello({
    protocolVersion,
    role: C.ROLE.DEVICE,
    connectionId,
    identitySec1: deviceIdentity.publicSec1,
    ephemeralSec1: hex(device.ephemeral_public_sec1_hex),
    signature,
  });
  eq(signedRecord.length, C.IDENTITY_HELLO_BYTES, "signed record length");
  eq(
    toHex(signedRecord.slice(0, C.OFFER_RECORD_BYTES)),
    `01${device.offer_record_hex.slice(2)}`,
    "identity hello = offer with tag 1",
  );

  // SAS: 고정 임시 스칼라로 ECDH → HKDF → 6자리.
  const deviceEphemeral = await crypto.subtle.importKey(
    "jwk",
    jwkFromScalar(device.ephemeral_public_sec1_hex, device.ephemeral_private_scalar_hex),
    { name: "ECDH", namedCurve: "P-256" },
    false,
    ["deriveBits"],
  );
  const shared = await C.deriveSharedSecret(deviceEphemeral, hex(desktop.ephemeral_public_sec1_hex));
  const material = await C.deriveSessionMaterial(shared, transcript);
  eq(material.sas, fixture.confirmation_code, "confirmation code");

  const proof = await C.encodePairingProof({
    pairingId: hex(proofFixture.pairing_id_hex),
    pairingSecret: hex(proofFixture.secret_hex),
    transcriptHash: hex(fixture.transcript_hash_hex),
    connectionId,
    deviceFingerprint: hex(device.identity_fingerprint_hex),
  });
  eq(toHex(proof), proofFixture.record_hex, "pairing proof record");
  eq(
    toHex(C.encodeKnownDevice(hex(fixture.known_device.device_id_hex))),
    fixture.known_device.record_hex,
    "known device record",
  );

  const link = C.parsePairingLink(`#${fixture.pairing_link.fragment}`);
  ok(link !== null, "pairing link must parse");
  eq(toHex(link.admissionHandle), fixture.pairing_link.admission_handle_hex, "link handle");
  eq(toHex(link.pairingId), proofFixture.pairing_id_hex, "link pairing id");
  eq(toHex(link.pairingSecret), proofFixture.secret_hex, "link secret");
  ok(C.parsePairingLink(`#${fixture.pairing_link.fragment}A`) === null, "over-long link rejected");
  ok(C.parsePairingLink("") === null, "empty link rejected");

  for (const reject of fixture.reject) {
    const bytes = hex(reject.record_hex);
    let rejected = bytes.length !== C.IDENTITY_HELLO_BYTES || bytes[0] !== C.HELLO_TAG.IDENTITY;
    if (!rejected) {
      try {
        const hello = C.decodeIdentityHello(bytes);
        rejected = !(await C.verifyPeerSignature(hello.identitySec1, hello.signature, transcript));
      } catch {
        rejected = true;
      }
    }
    ok(rejected, `reject vector must be refused: ${reject.name}`);
  }
  return toHex(signature);
}

// ── 2부: 가짜 소켓 위의 전체 흐름 ────────────────────────────────────────────────────

function fakeSocket(sent) {
  const listeners = new Map();
  const socket = {
    readyState: 0,
    binaryType: "blob",
    send(bytes) {
      sent.push(C.decodeFrame(bytes).frame);
    },
    close() {
      this.readyState = 3;
    },
    addEventListener(name, listener) {
      if (!listeners.has(name)) listeners.set(name, []);
      listeners.get(name).push(listener);
    },
  };
  const emit = (name, event) => {
    for (const listener of listeners.get(name) ?? []) listener(event);
  };
  return { socket, emit, hasListener: (name) => listeners.has(name) };
}

async function flow() {
  const protocolVersion = fixture.protocol_version;
  const proofFixture = fixture.pairing_proof;
  const route = hex("41".repeat(16));
  const handle = hex(fixture.pairing_link.admission_handle_hex);

  // Mac 역할 — 셸 모듈의 원시 요소만으로 수행한다(Mac 구현의 복사본이 아니다).
  const macKeys = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, false, [
    "sign",
    "verify",
  ]);
  const macPublic = new Uint8Array(await crypto.subtle.exportKey("raw", macKeys.publicKey));
  const mac = { privateKey: macKeys.privateKey, publicSec1: macPublic, fingerprint: await C.sha256(macPublic) };
  const macEphemeral = await C.createEphemeral();

  const sent = [];
  const { socket, emit, hasListener } = fakeSocket(sent);
  const deliver = (frameType, connectionId, payload, sequence = 0) => {
    const frame = C.encodeFrame({ frameType, routeId: route, connectionId, sequence, payload });
    emit("message", { data: frame.buffer });
    return frame;
  };

  location.hash = `#${fixture.pairing_link.fragment}`;
  // 셸이 거는 타이머는 여기서부터 전부 가짜 시계 위에 있다.
  installFakeClock();
  const shell = createShell({ endpoint: "wss://relay.example.test", socketFactory: () => socket });
  void shell.start();
  await waitFor(() => hasListener("open"), "shell opens a socket");
  eq(location.hash, "", "pairing link must be scrubbed from the location");
  socket.readyState = 1;
  emit("open");
  await waitFor(() => sent.length >= 1, "device admission frame");
  eq(sent[0].frameType, C.FRAME_TYPE.DEVICE_ADMISSION, "first frame is admission");
  eq(toHex(sent[0].payload), toHex(handle), "admission carries the link handle");

  // 연결 id는 **기기가 정한다**. 서버는 입장 프레임 헤더의 값을 그대로 `Admitted`로
  // 돌려주고, `PeerJoined`는 **Mac에게만** 보낸다 — 그래서 여기서 기기에게 PeerJoined를
  // 먹이면 실제로 일어나지 않는 일을 검증하게 된다(2026-09-03: 그 가짜 프레임 때문에
  // 프로덕션 페어링이 15초 입장 시한에 죽는 것을 이 게이트가 못 잡고 있었다).
  const connection = sent[0].connectionId;
  ok(
    !connection.every((byte) => byte === 0),
    "기기는 전0이 아닌 연결 id를 스스로 만들어 입장 프레임에 실어야 한다",
  );
  deliver(C.FRAME_TYPE.ADMITTED, connection, new Uint8Array(0));
  await waitFor(() => sent.length >= 2, "device offer");
  const offer = sent[1];
  eq(offer.frameType, C.FRAME_TYPE.HELLO, "offer is a hello frame");
  eq(offer.payload.length, C.OFFER_RECORD_BYTES, "offer length");
  eq(offer.payload[0], C.HELLO_TAG.OFFER, "offer tag");
  eq(offer.payload[5], C.ROLE.DEVICE, "offer role");
  eq(toHex(offer.payload.slice(6, 22)), toHex(connection), "offer connection");
  const deviceIdentitySec1 = offer.payload.slice(22, 87);
  const deviceEphemeralSec1 = offer.payload.slice(87, 152);

  const transcript = C.buildTranscript({
    protocolVersion,
    connectionId: connection,
    desktop: { identitySec1: mac.publicSec1, ephemeralSec1: macEphemeral.publicSec1 },
    device: { identitySec1: deviceIdentitySec1, ephemeralSec1: deviceEphemeralSec1 },
  });
  deliver(
    C.FRAME_TYPE.HELLO,
    connection,
    C.encodeIdentityHello({
      protocolVersion,
      role: C.ROLE.DESKTOP,
      connectionId: connection,
      identitySec1: mac.publicSec1,
      ephemeralSec1: macEphemeral.publicSec1,
      signature: await C.signTranscript(mac, transcript),
    }),
  );
  await waitFor(() => sent.length >= 4, "device hello and proof");
  const deviceHello = C.decodeIdentityHello(sent[2].payload);
  eq(deviceHello.role, C.ROLE.DEVICE, "device hello role");
  eq(toHex(deviceHello.identitySec1), toHex(deviceIdentitySec1), "device hello identity = offer");
  eq(toHex(deviceHello.ephemeralSec1), toHex(deviceEphemeralSec1), "device hello ephemeral = offer");
  ok(
    await C.verifyPeerSignature(deviceHello.identitySec1, deviceHello.signature, transcript),
    "device signature must verify over both offers",
  );
  const expectedProof = await C.encodePairingProof({
    pairingId: hex(proofFixture.pairing_id_hex),
    pairingSecret: hex(proofFixture.secret_hex),
    transcriptHash: await C.sha256(transcript),
    connectionId: connection,
    deviceFingerprint: await C.sha256(deviceIdentitySec1),
  });
  eq(sent[3].frameType, C.FRAME_TYPE.HELLO, "proof is a hello frame");
  eq(toHex(sent[3].payload), toHex(expectedProof), "pairing proof over the live transcript");

  const shared = await C.deriveSharedSecret(macEphemeral.privateKey, deviceEphemeralSec1);
  const material = await C.deriveSessionMaterial(shared, transcript);
  eq(document.body.dataset.screen, SCREEN.VERIFY, "verification screen after proof");
  eq(document.getElementById("verify-code").dataset.digits, material.sas, "same SAS on both sides");
  ok(document.getElementById("relay-session") === null, "no session DOM before activation");

  const macChannel = new C.RelaySecureChannel({
    protocolVersion,
    connectionId: connection,
    sendKey: await C.importGcmKey(material.desktopToDeviceKeyBytes),
    receiveKey: await C.importGcmKey(material.deviceToDesktopKeyBytes),
    sendDirection: C.DIRECTION.DESKTOP_TO_DEVICE,
    receiveDirection: C.DIRECTION.DEVICE_TO_DESKTOP,
    ownFingerprint: mac.fingerprint,
    peerFingerprint: await C.sha256(deviceIdentitySec1),
  });
  const registeredDevice = "02".repeat(16);
  const registeredExpiry = Math.floor(Date.now() / 1000) + 3600;
  const registration = await macChannel.seal(new TextEncoder().encode(JSON.stringify({
    type: "relay_registered", version: 2, device_id: registeredDevice,
    route_id: toHex(route), expires_at: registeredExpiry,
  })));
  deliver(C.FRAME_TYPE.CIPHERTEXT, connection, registration.ciphertext, registration.sequence);
  await waitFor(() => sent.length >= 5, "encrypted verifier registration");
  ok(document.getElementById("relay-session") === null, "no DOM before publication ACK");
  const register = JSON.parse(new TextDecoder().decode(await macChannel.open({
    sequence: sent[4].sequence, direction: C.DIRECTION.DEVICE_TO_DESKTOP, ciphertext: sent[4].payload,
  })));
  eq(register.type, "relay_register", "only verifier sent to Mac");
  eq(Object.keys(register).sort().join(","), "type,verifier,version", "raw grant never goes to Mac");
  const ready = await macChannel.seal(new TextEncoder().encode(JSON.stringify({
    type: "relay_ready", version: 2, device_id: registeredDevice,
    route_id: toHex(route), expires_at: registeredExpiry,
  })));
  deliver(C.FRAME_TYPE.CIPHERTEXT, connection, ready.ciphertext, ready.sequence);
  const dashboard = new TextEncoder().encode(
    JSON.stringify({
      type: "dashboard",
      workspaces: [
        {
          id: "ws-1",
          name: "deppy-sijo",
          state: "active",
          sessions: [
            { id: "s1", title: "codex", agent: "Codex · high", status: "running" },
            { id: "s2", title: "claude", agent: "Claude", status: "waiting" },
          ],
        },
        { id: "ws-2", name: "잠든 워크스페이스", state: "suspended", sessions: [] },
      ],
    }),
  );
  const sealed = await macChannel.seal(dashboard);
  const first = deliver(C.FRAME_TYPE.CIPHERTEXT, connection, sealed.ciphertext, sealed.sequence);
  await waitFor(() => document.body.dataset.screen === SCREEN.SESSION, "session screen");
  ok(document.getElementById("relay-session") !== null, "session DOM after activation");

  // 진입 화면 — 워크스페이스 카드. 터미널 화면은 아직 **실제로** 감춰져 있어야 한다.
  const home = document.getElementById("relay-home");
  const terminal = document.getElementById("relay-terminal");
  const cards = document.querySelectorAll("#relay-workspaces .m-card");
  eq(cards.length, 2, "one card per workspace");
  eq(cards[0].querySelector(".m-card-name").textContent, "deppy-sijo", "card name");
  eq(cards[0].querySelector(".m-badge-count").textContent, "2", "session count badge");
  eq(cards[0].querySelectorAll(".m-chip").length, 2, "one chip per session");
  eq(document.getElementById("relay-count").textContent, "2 workspaces", "workspace pill");
  // `hidden` 속성이 저자 CSS의 display를 실제로 이기는지 — 이기지 못하면 빈 터미널이 목록을 덮는다.
  eq(getComputedStyle(terminal).display, "none", "hidden terminal must not paint");
  eq(getComputedStyle(home).display, "flex", "home screen visible");

  // 카드 → 터미널 화면 + watch.
  const openFromMac = async (index) =>
    new TextDecoder().decode(
      await macChannel.open({
        sequence: sent[index].sequence,
        direction: C.DIRECTION.DEVICE_TO_DESKTOP,
        ciphertext: sent[index].payload,
      }),
    );
  cards[0].click();
  await waitFor(() => sent.length >= 6, "watch ciphertext");
  eq(sent[5].frameType, C.FRAME_TYPE.CIPHERTEXT, "watch is ciphertext");
  eq(await openFromMac(5), '{"type":"watch","session":"s1"}', "watch plaintext");
  eq(getComputedStyle(home).display, "none", "home hidden while watching");
  eq(getComputedStyle(terminal).display, "flex", "terminal visible while watching");
  eq(document.getElementById("term-name").textContent, "deppy-sijo", "terminal header name");
  eq(document.getElementById("term-session-chip").textContent, "codex", "session chip");

  // 시청 불가 워크스페이스는 열리지 않는다(토스트만).
  cards[1].click();
  await new Promise((resolve) => realSetTimeout(resolve, 30));
  eq(sent.length, 6, "an unwatchable workspace sends nothing");

  // Mac → keyframe viewport → DOM 행 렌더.
  const viewport = JSON.stringify({
    type: "viewport",
    session: "s1",
    seq: 1,
    keyframe: true,
    cols: 6,
    rows: 2,
    cursor: { visible: true, row: 1, col: 2, shape: "block" },
    alt: false,
    offset: 0,
    lines: [
      { row: 0, runs: [{ s: 0, t: "ok", fg: "#8ee6a3", bg: "#000000", a: 1, w: false }] },
      { row: 1, runs: [{ s: 0, t: "$ ", fg: "#d4d4d4", bg: "#000000", a: 0, w: false }] },
    ],
  });
  const sealedViewport = await macChannel.seal(new TextEncoder().encode(viewport));
  deliver(C.FRAME_TYPE.CIPHERTEXT, connection, sealedViewport.ciphertext, sealedViewport.sequence);
  await waitFor(
    () => document.querySelectorAll("#relay-viewport .m-term-row").length === 2,
    "viewport rendered as DOM rows",
  );
  const firstRow = document.querySelector("#relay-viewport .m-term-row");
  eq(firstRow.textContent, "ok    ", "row padded to the column count");
  const styled = firstRow.querySelector("span");
  eq(styled.style.color, "rgb(142, 230, 163)", "run foreground applied");
  eq(styled.style.fontWeight, "700", "bold attribute applied");
  ok(!document.querySelector("#relay-viewport .m-term-cursor").hidden, "cursor shown");

  // delta는 바뀐 행만 갈아 끼운다.
  const delta = JSON.stringify({
    type: "viewport",
    session: "s1",
    seq: 2,
    keyframe: false,
    cols: 6,
    rows: 2,
    cursor: { visible: true, row: 1, col: 4, shape: "block" },
    alt: false,
    offset: 3,
    lines: [{ row: 1, runs: [{ s: 0, t: "$ ls", fg: "#d4d4d4", bg: "#000000", a: 0, w: false }] }],
  });
  const sealedDelta = await macChannel.seal(new TextEncoder().encode(delta));
  deliver(C.FRAME_TYPE.CIPHERTEXT, connection, sealedDelta.ciphertext, sealedDelta.sequence);
  await waitFor(
    () => document.querySelectorAll("#relay-viewport .m-term-row")[1].textContent === "$ ls  ",
    "delta row replaced",
  );
  eq(
    document.querySelectorAll("#relay-viewport .m-term-row")[0].textContent,
    "ok    ",
    "untouched row kept",
  );
  ok(!document.querySelector(".m-term-scroll-note").hidden, "scrollback note shown");

  // ⋯ 메뉴 — 표시 설정만 있고 입력 수단은 없다.
  document.getElementById("term-menu-button").click();
  const menu = document.getElementById("relay-menu");
  ok(!menu.hidden, "menu opens");
  eq(getComputedStyle(menu).display, "block", "open menu paints");
  document.getElementById("menu-readable-wrap").click();
  eq(
    document.getElementById("menu-readable-wrap").getAttribute("aria-pressed"),
    "true",
    "readable wrap toggles",
  );
  ok(document.querySelector(".m-term-grid").classList.contains("wrap"), "wrap class applied");
  document.getElementById("menu-readable-wrap").click();
  const beforeFont = document.getElementById("menu-font-size").textContent;
  menu.querySelector(".m-menu-steps button:last-child").click();
  ok(
    document.getElementById("menu-font-size").textContent !== beforeFont,
    "font size step applied",
  );

  // 작성기 바는 자리에 있지만 전부 비활성이다 — 보기 전용.
  const composerControls = document.querySelectorAll(".m-composer button, .m-composer input");
  eq(composerControls.length, 4, "composer keeps the deppy-mux layout");
  ok(
    Array.from(composerControls).every((control) => control.disabled),
    "every composer control is disabled in a view-only shell",
  );

  // 뒤로 → unwatch, 목록 복귀.
  document.getElementById("term-back").click();
  await waitFor(() => sent.length >= 7, "unwatch ciphertext");
  eq(await openFromMac(6), '{"type":"unwatch"}', "unwatch plaintext");
  eq(getComputedStyle(terminal).display, "none", "terminal hidden after back");
  eq(getComputedStyle(home).display, "flex", "home visible after back");
  eq(shell.state.watching, null, "shell forgets the watched session");

  // 셸 API로 보내는 보기 전용 메시지도 같은 채널을 탄다.
  await shell.sendMessage({ type: "request_keyframe" });
  await waitFor(() => sent.length >= 8, "request_keyframe ciphertext");
  eq(await openFromMac(7), '{"type":"request_keyframe"}', "request_keyframe plaintext");

  let refused = false;
  try {
    await shell.sendMessage({ type: "input", session: "s1", text: "x" });
  } catch {
    refused = true;
  }
  ok(refused, "view-only shell cannot construct an input message");

  // 생존 신호: 서버는 **마지막으로 받은 DRLY 프레임** 시각으로만 유휴를 판정한다(기본 60초).
  // 이 셸은 보기 전용이라 승인 대기 5분 동안 보낼 것이 없으므로, 20초 주기의 빈 HEARTBEAT가
  // 없으면 60초에 끊기고 그 뒤의 승인은 죽은 세션에 도착한다.
  const beforeBeats = sent.length;
  advanceClock(20_000);
  eq(sent.length, beforeBeats + 1, "one heartbeat per 20s interval");
  const beat = sent[beforeBeats];
  eq(beat.frameType, C.FRAME_TYPE.HEARTBEAT, "the periodic frame is a heartbeat");
  eq(beat.payload.length, 0, "heartbeat payload is empty");
  eq(toHex(beat.routeId), toHex(route), "heartbeat carries the admitted route");
  eq(toHex(beat.connectionId), toHex(connection), "heartbeat carries this device session");
  advanceClock(20_000);
  eq(sent.length, beforeBeats + 2, "the heartbeat repeats on every interval");
  eq(sent[beforeBeats + 1].frameType, C.FRAME_TYPE.HEARTBEAT, "second heartbeat");

  // 재생: 같은 암호문을 다시 넣으면 채널이 닫히고 세션 DOM이 사라진다.
  emit("message", { data: first.buffer });
  await waitFor(() => document.body.dataset.screen === SCREEN.RECOVERY, "recovery after replay");
  ok(document.getElementById("relay-session") === null, "session DOM torn down after replay");
  eq(socket.readyState, 3, "socket closed after replay");

  // …그리고 생존 신호는 소켓과 함께 멈춘다. 살아남은 타이머는 누수이고, 닫힌 소켓에 send를
  // 걸면 throw한다.
  eq(clock.scheduled.size, 0, "no shell timer outlives the socket");
  const afterFail = sent.length;
  advanceClock(60_000);
  eq(sent.length, afterFail, "no heartbeat after fail()");

  // 새 문서는 링크 없이 같은 IndexedDB identity와 grant로 입장한다.
  const nextSent = [];
  const next = fakeSocket(nextSent);
  const nextShell = createShell({endpoint: "wss://relay.example.test", socketFactory: () => next.socket});
  await nextShell.start();
  next.socket.readyState = 1;
  next.emit("open");
  await waitFor(() => nextSent.length >= 1, "URL-free reconnect admission");
  eq(nextSent[0].frameType, C.FRAME_TYPE.RECONNECT_ADMISSION, "distinct reconnect admission");
  eq(toHex(await C.sha256(nextSent[0].payload)), register.verifier, "durable grant matches Mac verifier");
  const nextConnection = nextSent[0].connectionId;
  ok(toHex(nextConnection) !== toHex(connection), "new connection id after reload");
  const nextDeliver = (kind, payload, sequence = 0) => {
    const bytes = C.encodeFrame({frameType:kind, routeId:route, connectionId:nextConnection, sequence, payload});
    next.emit("message", {data:bytes.buffer});
  };
  nextDeliver(C.FRAME_TYPE.ADMITTED, new Uint8Array(0));
  await waitFor(() => nextSent.length >= 2, "reconnect offer");
  const nextEphemeral = await C.createEphemeral();
  const nextOffer = nextSent[1].payload;
  eq(toHex(nextOffer.slice(22,87)), toHex(deviceIdentitySec1), "same persistent device key");
  ok(toHex(nextOffer.slice(87,152)) !== toHex(deviceEphemeralSec1), "fresh ephemeral after reload");
  const nextTranscript = C.buildTranscript({protocolVersion, connectionId:nextConnection,
    desktop:{identitySec1:mac.publicSec1, ephemeralSec1:nextEphemeral.publicSec1},
    device:{identitySec1:nextOffer.slice(22,87), ephemeralSec1:nextOffer.slice(87,152)}});
  nextDeliver(C.FRAME_TYPE.HELLO, C.encodeIdentityHello({protocolVersion, role:C.ROLE.DESKTOP,
    connectionId:nextConnection, identitySec1:mac.publicSec1, ephemeralSec1:nextEphemeral.publicSec1,
    signature:await C.signTranscript(mac,nextTranscript)}));
  await waitFor(() => nextSent.length >= 4, "known-device claim without fresh SAS approval");
  eq(nextSent[3].payload[0], C.HELLO_TAG.KNOWN_DEVICE, "known device claim");
  eq(toHex(nextSent[3].payload.slice(1)), registeredDevice, "Mac-assigned id survives reload");
  const nextShared = await C.deriveSharedSecret(nextEphemeral.privateKey, nextOffer.slice(87,152));
  const nextMaterial = await C.deriveSessionMaterial(nextShared,nextTranscript);
  const nextMacChannel = new C.RelaySecureChannel({protocolVersion, connectionId:nextConnection,
    sendKey:await C.importGcmKey(nextMaterial.desktopToDeviceKeyBytes),
    receiveKey:await C.importGcmKey(nextMaterial.deviceToDesktopKeyBytes),
    sendDirection:C.DIRECTION.DESKTOP_TO_DEVICE, receiveDirection:C.DIRECTION.DEVICE_TO_DESKTOP,
    ownFingerprint:mac.fingerprint, peerFingerprint:await C.sha256(deviceIdentitySec1)});
  const nextReady = await nextMacChannel.seal(new TextEncoder().encode(JSON.stringify({
    type:"relay_ready",version:2,device_id:registeredDevice,route_id:toHex(route),expires_at:registeredExpiry})));
  nextDeliver(C.FRAME_TYPE.CIPHERTEXT,nextReady.ciphertext,nextReady.sequence);
  await waitFor(() => nextShell.state.screen === SCREEN.SESSION, "authenticated reconnect session");
  nextShell.dispose();

  // 악성 Relay가 자기 Mac 키로 올바르게 서명해도 저장한 pin과 다르면 거절한다.
  const forgedSent = [];
  const forged = fakeSocket(forgedSent);
  const forgedShell = createShell({endpoint:"wss://relay.example.test",socketFactory:()=>forged.socket});
  await forgedShell.start(); forged.socket.readyState = 1; forged.emit("open");
  const forgedConnection = forgedSent[0].connectionId;
  const forgedDeliver = (kind,payload) => {
    const bytes = C.encodeFrame({frameType:kind,routeId:route,connectionId:forgedConnection,sequence:0,payload});
    forged.emit("message",{data:bytes.buffer});
  };
  forgedDeliver(C.FRAME_TYPE.ADMITTED,new Uint8Array(0));
  await waitFor(()=>forgedSent.length>=2,"forged Mac offer");
  const forgedKeys = await crypto.subtle.generateKey({name:"ECDSA",namedCurve:"P-256"},false,["sign","verify"]);
  const forgedPublic = new Uint8Array(await crypto.subtle.exportKey("raw",forgedKeys.publicKey));
  const forgedEphemeral = await C.createEphemeral();
  const forgedOffer = forgedSent[1].payload;
  const forgedTranscript = C.buildTranscript({protocolVersion,connectionId:forgedConnection,
    desktop:{identitySec1:forgedPublic,ephemeralSec1:forgedEphemeral.publicSec1},
    device:{identitySec1:forgedOffer.slice(22,87),ephemeralSec1:forgedOffer.slice(87,152)}});
  forgedDeliver(C.FRAME_TYPE.HELLO,C.encodeIdentityHello({protocolVersion,role:C.ROLE.DESKTOP,
    connectionId:forgedConnection,identitySec1:forgedPublic,ephemeralSec1:forgedEphemeral.publicSec1,
    signature:await C.signTranscript({privateKey:forgedKeys.privateKey},forgedTranscript)}));
  await waitFor(()=>forgedShell.state.screen===SCREEN.RECOVERY,"Mac pin mismatch rejection");
  eq(forgedSent.length,2,"pin mismatch sends no signed identity or KnownDevice claim");
  ok(document.getElementById("relay-session")===null,"pin mismatch creates no session DOM");
  forgedShell.dispose();
}

window.addEventListener("error", (event) => done("error", `RELAY_SHELL_ERROR: uncaught ${event.message}`));
window.addEventListener("unhandledrejection", (event) =>
  done("error", `RELAY_SHELL_ERROR: rejection ${event.reason?.message ?? event.reason}`),
);

(async () => {
  try {
    const signatureHex = await vectors();
    await flow();
    done("ok", `RELAY_SHELL_OK:${signatureHex}`);
  } catch (error) {
    done("error", `RELAY_SHELL_ERROR: ${error?.message ?? error}\n${error?.stack ?? ""}`);
  }
})();
