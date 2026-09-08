// Deppy Relay 신뢰 셸의 **단 하나뿐인** 프로덕션 암호 모듈.
//
// Task 1이 고정한 고정 벡터(transcript / ECDSA / ECDH / HKDF-SHA256 / SAS / nonce / AAD /
// AES-256-GCM)를 이 파일 하나가 재현한다. Rust ↔ 브라우저 대조 하니스도 **이 파일을 그대로**
// 가져다 쓴다 — 테스트용 사본을 따로 두면 배포된 바이트가 검증된 적 없는 바이트가 된다.
//
// 신뢰 경계:
// - 기기 장기 신원은 **추출 불가능한** ECDSA P-256 `CryptoKey` 하나다. IndexedDB에 그 핸들만
//   담기며, 개인키 바이트는 어떤 경로로도 이 문서 밖으로 나가지 않는다.
// - 페어링 비밀은 HMAC-SHA256 안에서만 쓰이고 그대로 전송되지 않는다.
// - 수신 시퀀스는 **정확히 단조**다. 어긋난 프레임은 열지 않고 채널을 닫는다.
// - 모든 길이 상한은 **할당 이전에** 검사한다. 선언 길이를 믿고 버퍼를 잡지 않는다.

const encoder = new TextEncoder();

// ── 계약 상수 (crates/relay-protocol, crates/web-remote/src/relay/crypto.rs와 짝) ──────
export const PROTOCOL_VERSION = 1;
export const MAX_PLAINTEXT_BYTES = 1024 * 1024;
export const AES_GCM_TAG_BYTES = 16;
export const MAX_CIPHERTEXT_BYTES = MAX_PLAINTEXT_BYTES + AES_GCM_TAG_BYTES;

// WebCrypto 대기를 포함한 방향별 상한. 메시지 수와 보관 바이트를 함께 제한한다.
const MAX_PENDING_CHANNEL_FRAMES = 32;
const MAX_PENDING_CHANNEL_BYTES = 4 * MAX_CIPHERTEXT_BYTES;

export const MAGIC = "DRLY";
export const HEADER_BYTES = 52;
export const MAX_HELLO_BYTES = 512;
export const MAX_FRAME_BYTES = HEADER_BYTES + MAX_CIPHERTEXT_BYTES;

export const ROUTE_ID_BYTES = 16;
export const CONNECTION_ID_BYTES = 16;
export const ADMISSION_HANDLE_BYTES = 32;
export const PAIRING_ID_BYTES = 16;
export const PAIRING_SECRET_BYTES = 32;
export const DEVICE_ID_BYTES = 16;
export const IDENTITY_SEC1_BYTES = 65;
export const SIGNATURE_BYTES = 64;
export const FINGERPRINT_BYTES = 32;

export const FRAME_TYPE = Object.freeze({
  HELLO: 0x01,
  CIPHERTEXT: 0x02,
  HEARTBEAT: 0x03,
  CLOSE: 0x04,
  DESKTOP_ADMISSION: 0x10,
  DEVICE_ADMISSION: 0x11,
  TICKET_PUBLISH: 0x12,
  TICKET_REVOKE: 0x13,
  RECONNECT_PUBLISH: 0x14,
  RECONNECT_REVOKE: 0x15,
  RECONNECT_ADMISSION: 0x16,
  RECONNECT_SYNC: 0x17,
  RECONNECT_PUBLISHED: 0x24,
  ADMITTED: 0x20,
  REJECTED: 0x21,
  PEER_JOINED: 0x22,
  PEER_LEFT: 0x23,
});

// 종료·거절은 코드 하나다. 사람이 읽는 문자열은 와이어에 실리지 않는다.
export const REJECTION_CODE = Object.freeze({
  0x0001: "malformed-frame",
  0x0002: "unsupported-version",
  0x0003: "credential-rejected",
  0x0004: "ticket-unknown",
  0x0005: "ticket-consumed",
  0x0006: "route-busy",
  0x0007: "route-unknown",
  0x0008: "rate-limited",
  0x0009: "queue-overflow",
  0x000a: "idle-timeout",
  0x000b: "capacity-reached",
  0x000c: "peer-disconnected",
  0x000d: "shutting-down",
});

export const ROLE = Object.freeze({ DESKTOP: 1, DEVICE: 2 });

export const DIRECTION = Object.freeze({
  DESKTOP_TO_DEVICE: "desktop-to-device",
  DEVICE_TO_DESKTOP: "device-to-desktop",
});

// Hello 레코드 첫 바이트 = 태그. 하나의 DRLY `Hello` 프레임이 세 모양 중 정확히 하나를 싣는다.
export const HELLO_TAG = Object.freeze({
  /// 서명 없는 제시 — 서명은 양쪽 offer를 덮으므로 어느 쪽도 상대 임시키를 보기 전에는
  /// 서명할 수 없다. 기기가 이것으로 먼저 문을 연다.
  OFFER: 0x00,
  IDENTITY: 0x01,
  PAIRING_PROOF: 0x02,
  KNOWN_DEVICE: 0x03,
});

/// `0x00 || version u32 || role u8 || connection 16 || identity 65 || ephemeral 65` = 152.
export const OFFER_RECORD_BYTES = 1 + 4 + 1 + 16 + 65 + 65;
export const IDENTITY_HELLO_BYTES =
  1 + 4 + 1 + CONNECTION_ID_BYTES + IDENTITY_SEC1_BYTES * 2 + SIGNATURE_BYTES;
export const PAIRING_PROOF_BYTES = 1 + PAIRING_ID_BYTES + 32;
export const KNOWN_DEVICE_BYTES = 1 + DEVICE_ID_BYTES;

export const PAIRING_LINK_BYTES =
  ADMISSION_HANDLE_BYTES + PAIRING_ID_BYTES + PAIRING_SECRET_BYTES;
// base64url 80바이트 = 107자. 상한을 넉넉히 두되 **디코딩 전에** 잘라 낸다.
export const MAX_PAIRING_FRAGMENT_CHARS = 128;

const HANDSHAKE_DOMAIN = encoder.encode("deppy-relay-handshake-v1\0");
const HKDF_SALT_DOMAIN = encoder.encode("deppy-relay-hkdf-salt-v1\0");
const DESKTOP_TO_DEVICE_INFO = encoder.encode("deppy-relay-desktop-to-device-v1\0");
const DEVICE_TO_DESKTOP_INFO = encoder.encode("deppy-relay-device-to-desktop-v1\0");
const SAS_INFO = encoder.encode("deppy-relay-sas-v1\0");
const ENVELOPE_AAD_DOMAIN = encoder.encode("deppy-relay-envelope-aad-v1\0");
const MAGIC_BYTES = encoder.encode(MAGIC);

const DIRECTION_CONTRACT = new Map([
  [DIRECTION.DESKTOP_TO_DEVICE, { code: 1, nonceDomain: encoder.encode("D2DV") }],
  [DIRECTION.DEVICE_TO_DESKTOP, { code: 2, nonceDomain: encoder.encode("V2DS") }],
]);

// 종류별 페이로드 길이 범위. relay-protocol의 `payload_bounds`와 한 글자도 다르면 안 된다.
const PAYLOAD_BOUNDS = new Map([
  [FRAME_TYPE.HELLO, [1, MAX_HELLO_BYTES]],
  [FRAME_TYPE.CIPHERTEXT, [1, MAX_CIPHERTEXT_BYTES]],
  [FRAME_TYPE.HEARTBEAT, [0, 0]],
  [FRAME_TYPE.CLOSE, [2, 2]],
  [FRAME_TYPE.DESKTOP_ADMISSION, [ADMISSION_HANDLE_BYTES, ADMISSION_HANDLE_BYTES]],
  [FRAME_TYPE.DEVICE_ADMISSION, [ADMISSION_HANDLE_BYTES, ADMISSION_HANDLE_BYTES]],
  [FRAME_TYPE.TICKET_PUBLISH, [ADMISSION_HANDLE_BYTES, ADMISSION_HANDLE_BYTES]],
  [FRAME_TYPE.TICKET_REVOKE, [ADMISSION_HANDLE_BYTES, ADMISSION_HANDLE_BYTES]],
  [FRAME_TYPE.RECONNECT_PUBLISH, [40, 40]],
  [FRAME_TYPE.RECONNECT_REVOKE, [32, 32]],
  [FRAME_TYPE.RECONNECT_ADMISSION, [32, 32]],
  [FRAME_TYPE.RECONNECT_PUBLISHED, [32, 32]],
  [FRAME_TYPE.RECONNECT_SYNC, [0, 0]],
  [FRAME_TYPE.ADMITTED, [0, 0]],
  [FRAME_TYPE.REJECTED, [2, 2]],
  [FRAME_TYPE.PEER_JOINED, [0, 0]],
  [FRAME_TYPE.PEER_LEFT, [0, 0]],
]);

const U64_MAX = 0xffffffffffffffffn;

// ── 바이트 유틸 ──────────────────────────────────────────────────────────────────────

export function asBytes(source) {
  if (source instanceof Uint8Array) return source;
  if (source instanceof ArrayBuffer) return new Uint8Array(source);
  if (ArrayBuffer.isView(source)) {
    return new Uint8Array(source.buffer, source.byteOffset, source.byteLength);
  }
  throw new TypeError("바이트가 아닌 값이 들어왔다");
}

export function concatBytes(...parts) {
  let total = 0;
  for (const part of parts) total += part.length;
  const merged = new Uint8Array(total);
  let offset = 0;
  for (const part of parts) {
    merged.set(part, offset);
    offset += part.length;
  }
  return merged;
}

export function fixedBytes(value, byteLength, label) {
  const bytes = asBytes(value);
  if (bytes.length !== byteLength) {
    throw new RangeError(`${label}: ${byteLength}바이트여야 하는데 ${bytes.length}바이트다`);
  }
  return bytes;
}

export function u32be(value) {
  if (!Number.isSafeInteger(value) || value < 0 || value > 0xffffffff) {
    throw new RangeError("u32 범위를 벗어났다");
  }
  const bytes = new Uint8Array(4);
  new DataView(bytes.buffer).setUint32(0, value, false);
  return bytes;
}

export function u64be(value) {
  const wide = BigInt(value);
  if (wide < 0n || wide > U64_MAX) throw new RangeError("u64 범위를 벗어났다");
  const bytes = new Uint8Array(8);
  new DataView(bytes.buffer).setBigUint64(0, wide, false);
  return bytes;
}

export function hexFromBytes(value) {
  return Array.from(asBytes(value), (byte) => byte.toString(16).padStart(2, "0")).join("");
}

export function bytesFromHex(value) {
  if (typeof value !== "string" || value.length % 2 !== 0 || !/^(?:[0-9a-fA-F]{2})*$/.test(value)) {
    throw new RangeError("16진 문자열이 아니다");
  }
  const bytes = new Uint8Array(value.length / 2);
  for (let index = 0; index < bytes.length; index += 1) {
    bytes[index] = Number.parseInt(value.slice(index * 2, index * 2 + 2), 16);
  }
  return bytes;
}

export function bytesToBase64Url(value) {
  const bytes = asBytes(value);
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replaceAll("=", "");
}

export function base64UrlToBytes(text) {
  if (typeof text !== "string" || !/^[A-Za-z0-9_-]*$/.test(text)) {
    throw new RangeError("base64url이 아니다");
  }
  const padded = text.replaceAll("-", "+").replaceAll("_", "/").padEnd(
    text.length + ((4 - (text.length % 4)) % 4),
    "=",
  );
  const binary = atob(padded);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index);
  return bytes;
}

/// 키 바이트를 쓰고 나면 즉시 덮는다. GC를 기다리지 않는다.
export function zeroBytes(value) {
  asBytes(value).fill(0);
}

// ── 해시 / transcript / 파생 ──────────────────────────────────────────────────────────

export async function sha256(value) {
  return new Uint8Array(await crypto.subtle.digest("SHA-256", asBytes(value)));
}

/// Task 1 고정 벡터의 transcript. 프로토콜 버전·연결 id·양쪽 역할/신원/임시키를 모두 묶는다.
export function buildTranscript({ protocolVersion, connectionId, desktop, device }) {
  return concatBytes(
    HANDSHAKE_DOMAIN,
    u32be(protocolVersion),
    fixedBytes(connectionId, CONNECTION_ID_BYTES, "연결 id"),
    Uint8Array.of(ROLE.DESKTOP),
    fixedBytes(desktop.identitySec1, IDENTITY_SEC1_BYTES, "데스크톱 신원키"),
    fixedBytes(desktop.ephemeralSec1, IDENTITY_SEC1_BYTES, "데스크톱 임시키"),
    Uint8Array.of(ROLE.DEVICE),
    fixedBytes(device.identitySec1, IDENTITY_SEC1_BYTES, "기기 신원키"),
    fixedBytes(device.ephemeralSec1, IDENTITY_SEC1_BYTES, "기기 임시키"),
  );
}

export async function transcriptSalt(transcript) {
  return sha256(concatBytes(HKDF_SALT_DOMAIN, asBytes(transcript)));
}

async function expand(hkdfKey, salt, info, bitLength) {
  return new Uint8Array(
    await crypto.subtle.deriveBits(
      { name: "HKDF", hash: "SHA-256", salt, info },
      hkdfKey,
      bitLength,
    ),
  );
}

/// 공유 비밀 + transcript → 방향별 AES-256 키 바이트와 6자리 대조 코드(SAS).
export async function deriveSessionMaterial(sharedSecret, transcript) {
  const salt = await transcriptSalt(transcript);
  const hkdfKey = await crypto.subtle.importKey("raw", asBytes(sharedSecret), "HKDF", false, [
    "deriveBits",
  ]);
  const desktopToDeviceKeyBytes = await expand(hkdfKey, salt, DESKTOP_TO_DEVICE_INFO, 256);
  const deviceToDesktopKeyBytes = await expand(hkdfKey, salt, DEVICE_TO_DESKTOP_INFO, 256);
  const sasBytes = await expand(hkdfKey, salt, SAS_INFO, 32);
  const sasNumber = new DataView(sasBytes.buffer, sasBytes.byteOffset, 4).getUint32(0, false);
  return {
    salt,
    desktopToDeviceKeyBytes,
    deviceToDesktopKeyBytes,
    sas: String(sasNumber % 1_000_000).padStart(6, "0"),
  };
}

export async function importGcmKey(keyBytes) {
  return crypto.subtle.importKey("raw", fixedBytes(keyBytes, 32, "AES-256 키"), { name: "AES-GCM" }, false, [
    "encrypt",
    "decrypt",
  ]);
}

// ── 봉투(nonce / AAD) ────────────────────────────────────────────────────────────────

function directionContract(direction) {
  const contract = DIRECTION_CONTRACT.get(direction);
  if (!contract) throw new RangeError(`계약 밖 방향: ${String(direction)}`);
  return contract;
}

export function envelopeNonce(direction, sequence) {
  return concatBytes(directionContract(direction).nonceDomain, u64be(sequence));
}

export function envelopeAad({
  protocolVersion,
  connectionId,
  direction,
  sequence,
  senderFingerprint,
  recipientFingerprint,
  ciphertextLength,
}) {
  if (!Number.isSafeInteger(ciphertextLength) || ciphertextLength < 0) {
    throw new RangeError("암호문 길이가 정수가 아니다");
  }
  if (ciphertextLength > MAX_CIPHERTEXT_BYTES) {
    throw new RangeError("암호문 길이가 고정 상한을 넘었다");
  }
  return concatBytes(
    ENVELOPE_AAD_DOMAIN,
    u32be(protocolVersion),
    fixedBytes(connectionId, CONNECTION_ID_BYTES, "연결 id"),
    fixedBytes(senderFingerprint, FINGERPRINT_BYTES, "발신자 지문"),
    fixedBytes(recipientFingerprint, FINGERPRINT_BYTES, "수신자 지문"),
    Uint8Array.of(directionContract(direction).code),
    u64be(sequence),
    u32be(ciphertextLength),
  );
}

// ── 기기 장기 신원 (추출 불가능 CryptoKey + IndexedDB) ────────────────────────────────

const IDENTITY_DB = "deppy-relay-shell";
const IDENTITY_DB_VERSION = 1;
const IDENTITY_STORE = "identity";
const IDENTITY_RECORD_KEY = "device-identity-v1";
const DEVICE_RECORD_KEY = "known-device-v1";
const REGISTRATION_RECORD_KEY = "registration-v2";
const IDENTITY_RECORD_VERSION = 1;
const IDENTITY_PROBE = encoder.encode("deppy-relay-identity-probe-v1\0");

/// 저장된 신원이 쓸 수 없을 때 던진다. **절대 새 키로 조용히 갈아타지 않는다** — 사용자가
/// "신원 초기화"를 명시적으로 고를 때만 갈아탄다.
export class IdentityUnusableError extends Error {
  constructor(reason) {
    super(`저장된 기기 신원을 쓸 수 없다 (${reason})`);
    this.name = "IdentityUnusableError";
    this.reason = reason;
  }
}

function requestToPromise(request) {
  return new Promise((succeed, fail) => {
    request.onsuccess = () => succeed(request.result);
    request.onerror = () => fail(request.error ?? new Error("IndexedDB 요청이 실패했다"));
  });
}

function openIdentityDb() {
  return new Promise((succeed, fail) => {
    let request;
    try {
      request = indexedDB.open(IDENTITY_DB, IDENTITY_DB_VERSION);
    } catch (error) {
      fail(error);
      return;
    }
    request.onupgradeneeded = () => {
      const db = request.result;
      if (!db.objectStoreNames.contains(IDENTITY_STORE)) db.createObjectStore(IDENTITY_STORE);
    };
    request.onsuccess = () => succeed(request.result);
    request.onerror = () => fail(request.error ?? new Error("IndexedDB를 열 수 없다"));
    request.onblocked = () => fail(new Error("IndexedDB가 다른 탭에 잠겨 있다"));
  });
}

async function withStore(mode, body) {
  let db;
  try {
    db = await openIdentityDb();
  } catch {
    throw new IdentityUnusableError("storage-unavailable");
  }
  try {
    return await body(db.transaction(IDENTITY_STORE, mode).objectStore(IDENTITY_STORE));
  } finally {
    db.close();
  }
}

async function adoptIdentity(record) {
  if (!record || typeof record !== "object") throw new IdentityUnusableError("record-missing");
  if (record.version !== IDENTITY_RECORD_VERSION) {
    throw new IdentityUnusableError("record-version");
  }
  const { privateKey, publicKey } = record;
  if (!(privateKey instanceof CryptoKey) || !(publicKey instanceof CryptoKey)) {
    throw new IdentityUnusableError("not-a-cryptokey");
  }
  if (privateKey.type !== "private" || publicKey.type !== "public") {
    throw new IdentityUnusableError("key-type");
  }
  // 추출 가능한 개인키가 저장돼 있다면 그건 이 모듈이 만든 신원이 아니다. 열지 않는다.
  if (privateKey.extractable !== false) throw new IdentityUnusableError("extractable-private-key");
  if (privateKey.algorithm?.name !== "ECDSA" || privateKey.algorithm?.namedCurve !== "P-256") {
    throw new IdentityUnusableError("algorithm");
  }
  if (!privateKey.usages.includes("sign")) throw new IdentityUnusableError("usages");

  let publicSec1;
  try {
    publicSec1 = new Uint8Array(await crypto.subtle.exportKey("raw", publicKey));
  } catch {
    throw new IdentityUnusableError("public-key-unreadable");
  }
  if (publicSec1.length !== IDENTITY_SEC1_BYTES || publicSec1[0] !== 0x04) {
    throw new IdentityUnusableError("public-key-shape");
  }
  // 자기검사: 저장된 개인키가 저장된 공개키에 실제로 대응하는지 서명 한 번으로 확인한다.
  let probe;
  try {
    probe = await crypto.subtle.sign({ name: "ECDSA", hash: "SHA-256" }, privateKey, IDENTITY_PROBE);
  } catch {
    throw new IdentityUnusableError("sign-failed");
  }
  let matched = false;
  try {
    matched = await crypto.subtle.verify(
      { name: "ECDSA", hash: "SHA-256" },
      publicKey,
      probe,
      IDENTITY_PROBE,
    );
  } catch {
    throw new IdentityUnusableError("verify-failed");
  }
  if (!matched) throw new IdentityUnusableError("key-pair-mismatch");

  return {
    privateKey,
    publicKey,
    publicSec1,
    fingerprint: await sha256(publicSec1),
  };
}

/// 저장된 신원을 읽는다. 없으면 `null`, 깨졌으면 `IdentityUnusableError`.
export async function loadIdentity() {
  const record = await withStore("readonly", (store) =>
    requestToPromise(store.get(IDENTITY_RECORD_KEY)),
  );
  if (record === undefined || record === null) return null;
  return adoptIdentity(record);
}

/// 새 신원을 만들어 저장한다. 개인키는 `extractable: false`라 바이트가 존재하지 않는다.
export async function createIdentity() {
  const keyPair = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, false, [
    "sign",
    "verify",
  ]);
  const record = {
    version: IDENTITY_RECORD_VERSION,
    privateKey: keyPair.privateKey,
    publicKey: keyPair.publicKey,
  };
  await withStore("readwrite", (store) =>
    requestToPromise(store.put(record, IDENTITY_RECORD_KEY)),
  );
  return adoptIdentity(record);
}

export async function loadOrCreateIdentity() {
  return (await loadIdentity()) ?? (await createIdentity());
}

/// 사용자가 명시적으로 고른 초기화. 기억된 기기 등록도 함께 버린다 — 새 신원은 새 기기다.
export async function resetIdentity() {
  await withStore("readwrite", async (store) => {
    await requestToPromise(store.delete(IDENTITY_RECORD_KEY));
    await requestToPromise(store.delete(DEVICE_RECORD_KEY));
    await requestToPromise(store.delete(REGISTRATION_RECORD_KEY));
  });
  return createIdentity();
}

/// Mac이 암호화 채널 안에서 알려 준 기기 등록 id. 없으면 `null`.
export async function loadKnownDeviceId() {
  const record = await withStore("readonly", (store) =>
    requestToPromise(store.get(DEVICE_RECORD_KEY)),
  );
  if (!(record instanceof Uint8Array) || record.length !== DEVICE_ID_BYTES) return null;
  return record;
}

export async function rememberDeviceId(deviceId) {
  const bytes = fixedBytes(deviceId, DEVICE_ID_BYTES, "기기 등록 id");
  await withStore("readwrite", (store) =>
    requestToPromise(store.put(new Uint8Array(bytes), DEVICE_RECORD_KEY)),
  );
}

export async function forgetKnownDeviceId() {
  await withStore("readwrite", (store) => requestToPromise(store.delete(DEVICE_RECORD_KEY)));
}

// 재접속 등록은 신원과 한 묶음이다. 예전 id 단독 레코드는 재접속 자격이 아니다.
export function validateRegistration(record, identityFingerprint, now = Math.floor(Date.now() / 1000)) {
  if (!record || record.version !== 2 || !Number.isSafeInteger(record.expiresAt) ||
      record.expiresAt <= now) throw new IdentityUnusableError("registration-expired-or-invalid");
  for (const [key, size] of [["deviceId",16], ["routeId",16], ["desktopFingerprint",32],
      ["identityFingerprint",32], ["grant",32]]) {
    const value = record[key];
    if (!(value instanceof Uint8Array) || value.length !== size || value.every((v) => v === 0)) {
      throw new IdentityUnusableError("registration-shape");
    }
  }
  if (!sameBytes(record.identityFingerprint, identityFingerprint)) {
    throw new IdentityUnusableError("registration-identity-mismatch");
  }
  return record;
}

export async function loadRegistration(identityFingerprint) {
  const record = await withStore("readonly", (store) => requestToPromise(store.get(REGISTRATION_RECORD_KEY)));
  if (record === undefined || record === null) return null;
  return validateRegistration(record, identityFingerprint);
}

// request 성공은 commit이 아니다. 디스크 오류/취소가 있으면 성공 등록을 남기지 않는다.
export async function saveRegistration(record, identityFingerprint, signal) {
  validateRegistration(record, identityFingerprint);
  const db = await openIdentityDb();
  try {
    if (signal?.aborted) throw new IdentityUnusableError("registration-cancelled");
    await new Promise((resolve, reject) => {
      const transaction = db.transaction(IDENTITY_STORE, "readwrite");
      const abort = () => { try { transaction.abort(); } catch {} };
      const clean = () => signal?.removeEventListener("abort", abort);
      transaction.oncomplete = () => { clean(); resolve(); };
      transaction.onabort = transaction.onerror = () => {
        clean(); reject(new IdentityUnusableError("registration-commit-failed"));
      };
      signal?.addEventListener("abort", abort, {once: true});
      transaction.objectStore(IDENTITY_STORE).put(structuredClone(record), REGISTRATION_RECORD_KEY);
    });
  } finally { db.close(); }
}

function sameBytes(left, right) {
  return left instanceof Uint8Array && right instanceof Uint8Array && left.length === right.length &&
    left.every((byte, index) => byte === right[index]);
}

export function assertPinnedDesktop(record, fingerprint) {
  if (!sameBytes(record.desktopFingerprint, fingerprint)) {
    throw new IdentityUnusableError("desktop-identity-changed");
  }
}

// ── 연결마다 새로 만드는 ECDH 임시키 ──────────────────────────────────────────────────

export async function createEphemeral() {
  const keyPair = await crypto.subtle.generateKey({ name: "ECDH", namedCurve: "P-256" }, false, [
    "deriveBits",
  ]);
  const publicSec1 = new Uint8Array(await crypto.subtle.exportKey("raw", keyPair.publicKey));
  if (publicSec1.length !== IDENTITY_SEC1_BYTES || publicSec1[0] !== 0x04) {
    throw new RangeError("임시 공개키가 비압축 SEC1이 아니다");
  }
  return { privateKey: keyPair.privateKey, publicSec1 };
}

export async function deriveSharedSecret(ephemeralPrivateKey, peerEphemeralSec1) {
  const peer = await crypto.subtle.importKey(
    "raw",
    fixedBytes(peerEphemeralSec1, IDENTITY_SEC1_BYTES, "상대 임시키"),
    { name: "ECDH", namedCurve: "P-256" },
    false,
    [],
  );
  return new Uint8Array(
    await crypto.subtle.deriveBits({ name: "ECDH", public: peer }, ephemeralPrivateKey, 256),
  );
}

export async function signTranscript(identity, transcript) {
  const signature = new Uint8Array(
    await crypto.subtle.sign(
      { name: "ECDSA", hash: "SHA-256" },
      identity.privateKey,
      asBytes(transcript),
    ),
  );
  if (signature.length !== SIGNATURE_BYTES) {
    throw new RangeError("ECDSA 서명이 raw r||s 64바이트가 아니다");
  }
  return signature;
}

export async function verifyPeerSignature(peerIdentitySec1, signature, transcript) {
  const key = await crypto.subtle.importKey(
    "raw",
    fixedBytes(peerIdentitySec1, IDENTITY_SEC1_BYTES, "상대 신원키"),
    { name: "ECDSA", namedCurve: "P-256" },
    false,
    ["verify"],
  );
  return crypto.subtle.verify(
    { name: "ECDSA", hash: "SHA-256" },
    key,
    fixedBytes(signature, SIGNATURE_BYTES, "상대 서명"),
    asBytes(transcript),
  );
}

// ── Hello 레코드 ────────────────────────────────────────────────────────────────────

export function encodeIdentityHello({
  protocolVersion,
  role,
  connectionId,
  identitySec1,
  ephemeralSec1,
  signature,
}) {
  if (role !== ROLE.DESKTOP && role !== ROLE.DEVICE) throw new RangeError("계약 밖 역할이다");
  const record = concatBytes(
    Uint8Array.of(HELLO_TAG.IDENTITY),
    u32be(protocolVersion),
    Uint8Array.of(role),
    fixedBytes(connectionId, CONNECTION_ID_BYTES, "연결 id"),
    fixedBytes(identitySec1, IDENTITY_SEC1_BYTES, "신원키"),
    fixedBytes(ephemeralSec1, IDENTITY_SEC1_BYTES, "임시키"),
    fixedBytes(signature, SIGNATURE_BYTES, "서명"),
  );
  if (record.length !== IDENTITY_HELLO_BYTES) throw new RangeError("IdentityHello 길이가 틀렸다");
  return record;
}

/// 서명 없는 제시. IdentityHello에서 서명만 뺀 모양이다.
export function encodeOffer({ protocolVersion, role, connectionId, identitySec1, ephemeralSec1 }) {
  if (role !== ROLE.DESKTOP && role !== ROLE.DEVICE) throw new RangeError("계약 밖 역할이다");
  const record = concatBytes(
    Uint8Array.of(HELLO_TAG.OFFER),
    u32be(protocolVersion),
    Uint8Array.of(role),
    fixedBytes(connectionId, CONNECTION_ID_BYTES, "연결 id"),
    fixedBytes(identitySec1, IDENTITY_SEC1_BYTES, "신원키"),
    fixedBytes(ephemeralSec1, IDENTITY_SEC1_BYTES, "임시키"),
  );
  if (record.length !== OFFER_RECORD_BYTES) throw new RangeError("Offer 길이가 틀렸다");
  return record;
}

export function decodeIdentityHello(source) {
  const bytes = asBytes(source);
  if (bytes.length !== IDENTITY_HELLO_BYTES) throw new RangeError("IdentityHello 길이가 틀렸다");
  if (bytes[0] !== HELLO_TAG.IDENTITY) throw new RangeError("IdentityHello 태그가 아니다");
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const role = bytes[5];
  if (role !== ROLE.DESKTOP && role !== ROLE.DEVICE) throw new RangeError("계약 밖 역할이다");
  return {
    protocolVersion: view.getUint32(1, false),
    role,
    connectionId: bytes.slice(6, 22),
    identitySec1: bytes.slice(22, 87),
    ephemeralSec1: bytes.slice(87, 152),
    signature: bytes.slice(152, 216),
  };
}

/// 새 페어링 증명. **페어링 비밀은 HMAC 안에서만 쓰이고 절대 전송되지 않는다.**
export async function encodePairingProof({
  pairingId,
  pairingSecret,
  transcriptHash,
  connectionId,
  deviceFingerprint,
}) {
  const key = await crypto.subtle.importKey(
    "raw",
    fixedBytes(pairingSecret, PAIRING_SECRET_BYTES, "페어링 비밀"),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const message = concatBytes(
    fixedBytes(transcriptHash, 32, "transcript 해시"),
    fixedBytes(connectionId, CONNECTION_ID_BYTES, "연결 id"),
    fixedBytes(deviceFingerprint, FINGERPRINT_BYTES, "기기 신원 지문"),
  );
  const mac = new Uint8Array(await crypto.subtle.sign("HMAC", key, message));
  const record = concatBytes(
    Uint8Array.of(HELLO_TAG.PAIRING_PROOF),
    fixedBytes(pairingId, PAIRING_ID_BYTES, "페어링 id"),
    mac,
  );
  if (record.length !== PAIRING_PROOF_BYTES) throw new RangeError("PairingProof 길이가 틀렸다");
  return record;
}

export function encodeKnownDevice(deviceId) {
  return concatBytes(
    Uint8Array.of(HELLO_TAG.KNOWN_DEVICE),
    fixedBytes(deviceId, DEVICE_ID_BYTES, "기기 등록 id"),
  );
}

/// 페어링 링크 조각을 판독한다. **길이를 먼저 자르고** 나서야 디코딩한다.
export function parsePairingLink(fragment) {
  if (typeof fragment !== "string") return null;
  const text = fragment.startsWith("#") ? fragment.slice(1) : fragment;
  if (text.length === 0 || text.length > MAX_PAIRING_FRAGMENT_CHARS) return null;
  if (!/^[A-Za-z0-9_-]+$/.test(text)) return null;
  let bytes;
  try {
    bytes = base64UrlToBytes(text);
  } catch {
    return null;
  }
  if (bytes.length !== PAIRING_LINK_BYTES) return null;
  return {
    admissionHandle: bytes.slice(0, ADMISSION_HANDLE_BYTES),
    pairingId: bytes.slice(ADMISSION_HANDLE_BYTES, ADMISSION_HANDLE_BYTES + PAIRING_ID_BYTES),
    pairingSecret: bytes.slice(ADMISSION_HANDLE_BYTES + PAIRING_ID_BYTES, PAIRING_LINK_BYTES),
  };
}

// ── DRLY 프레이밍 ───────────────────────────────────────────────────────────────────

export class RelayDecodeError extends Error {
  constructor(kind, detail) {
    super(`DRLY 프레임 거절: ${kind}`);
    this.name = "RelayDecodeError";
    this.kind = kind;
    this.detail = detail ?? null;
  }
}

export function encodeFrame({ frameType, routeId, connectionId, sequence, payload }) {
  const bounds = PAYLOAD_BOUNDS.get(frameType);
  if (!bounds) throw new RangeError(`계약 밖 프레임 종류: ${String(frameType)}`);
  const body = payload === undefined || payload === null ? new Uint8Array(0) : asBytes(payload);
  if (body.length < bounds[0] || body.length > bounds[1]) {
    throw new RangeError(`${frameType} 페이로드 ${body.length}바이트는 계약 밖이다`);
  }
  const route = fixedBytes(routeId, ROUTE_ID_BYTES, "라우트 id");
  const connection = fixedBytes(connectionId, CONNECTION_ID_BYTES, "연결 id");
  const frame = new Uint8Array(HEADER_BYTES + body.length);
  const view = new DataView(frame.buffer);
  frame.set(MAGIC_BYTES, 0);
  view.setUint16(4, PROTOCOL_VERSION, false);
  frame[6] = frameType;
  frame[7] = 0;
  frame.set(route, 8);
  frame.set(connection, 24);
  view.setBigUint64(40, BigInt(sequence), false);
  view.setUint32(48, body.length, false);
  frame.set(body, HEADER_BYTES);
  return frame;
}

/// 순서가 곧 보안이다: magic → 버전 → 종류 → 예약 플래그 → **선언 길이 상한** → 그제서야
/// 페이로드 존재 여부. 상한 검사가 버퍼링보다 먼저이므로 거대한 선언은 바이트가 오기 전에 끊긴다.
export function decodeFrame(source) {
  const bytes = asBytes(source);
  if (bytes.length < HEADER_BYTES) throw new RelayDecodeError("Incomplete", HEADER_BYTES);
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  for (let index = 0; index < MAGIC_BYTES.length; index += 1) {
    if (bytes[index] !== MAGIC_BYTES[index]) throw new RelayDecodeError("BadMagic");
  }
  const version = view.getUint16(4, false);
  if (version !== PROTOCOL_VERSION) throw new RelayDecodeError("UnsupportedVersion", version);
  const frameType = bytes[6];
  const bounds = PAYLOAD_BOUNDS.get(frameType);
  if (!bounds) throw new RelayDecodeError("UnknownFrameType", frameType);
  if (bytes[7] !== 0) throw new RelayDecodeError("ReservedFlagsSet");

  const declared = view.getUint32(48, false);
  if (declared > bounds[1]) throw new RelayDecodeError("PayloadTooLarge", declared);
  if (declared < bounds[0]) throw new RelayDecodeError("PayloadTooSmall", declared);

  const total = HEADER_BYTES + declared;
  if (bytes.length < total) throw new RelayDecodeError("Incomplete", total);
  return {
    frame: {
      frameType,
      routeId: bytes.slice(8, 24),
      connectionId: bytes.slice(24, 40),
      sequence: view.getBigUint64(40, false),
      payload: bytes.slice(HEADER_BYTES, total),
    },
    consumed: total,
  };
}

export function rejectionCodeOf(frame) {
  if (frame.frameType !== FRAME_TYPE.CLOSE && frame.frameType !== FRAME_TYPE.REJECTED) return null;
  if (frame.payload.length !== 2) return null;
  const code = (frame.payload[0] << 8) | frame.payload[1];
  return REJECTION_CODE[code] ?? null;
}

// ── AES-256-GCM 채널 ────────────────────────────────────────────────────────────────

/// 한 연결의 양방향 봉투. 수신은 **정확히 단조**이며 어긋나는 순간 채널이 닫힌다.
export class RelaySecureChannel {
  #protocolVersion;
  #connectionId;
  #sendKey;
  #receiveKey;
  #sendDirection;
  #receiveDirection;
  #ownFingerprint;
  #peerFingerprint;
  #sendSequence = 0n;
  #receiveSequence = 0n;
  #closed = false;
  #sendQueue = { tail: Promise.resolve(), frames: 0, bytes: 0 };
  #receiveQueue = { tail: Promise.resolve(), frames: 0, bytes: 0 };

  constructor({
    protocolVersion,
    connectionId,
    sendKey,
    receiveKey,
    sendDirection,
    receiveDirection,
    ownFingerprint,
    peerFingerprint,
  }) {
    this.#protocolVersion = protocolVersion;
    this.#connectionId = fixedBytes(connectionId, CONNECTION_ID_BYTES, "연결 id");
    this.#sendKey = sendKey;
    this.#receiveKey = receiveKey;
    // 계약 밖 방향은 생성 시점에 막는다 — 채널이 만들어진 뒤에는 방향이 바뀌지 않는다.
    directionContract(sendDirection);
    directionContract(receiveDirection);
    if (sendDirection === receiveDirection) throw new RangeError("송수신 방향이 같을 수 없다");
    this.#sendDirection = sendDirection;
    this.#receiveDirection = receiveDirection;
    this.#ownFingerprint = fixedBytes(ownFingerprint, FINGERPRINT_BYTES, "내 지문");
    this.#peerFingerprint = fixedBytes(peerFingerprint, FINGERPRINT_BYTES, "상대 지문");
  }

  get closed() {
    return this.#closed;
  }

  get sendSequence() {
    return this.#sendSequence;
  }

  get receiveSequence() {
    return this.#receiveSequence;
  }

  async #enqueue(queue, source, operation) {
    if (this.#closed) throw new Error("닫힌 채널에는 작업을 넣을 수 없다");
    // 복사나 Promise 연결보다 먼저 제한한다. 느린 암호화가 무한 버퍼가 되어서는 안 된다.
    if (queue.frames >= MAX_PENDING_CHANNEL_FRAMES ||
        queue.bytes + source.length > MAX_PENDING_CHANNEL_BYTES) {
      this.close();
      throw new RangeError("암호 채널 대기열이 고정 상한을 넘었다");
    }
    const bytes = new Uint8Array(source);
    queue.frames += 1;
    queue.bytes += bytes.length;
    const result = queue.tail.then(async () => {
      if (this.#closed) throw new Error("대기 중 채널이 닫혔다");
      try {
        const value = await operation(bytes);
        // close는 WebCrypto 호출을 취소하지 못한다. 뒤늦은 결과는 외부로 넘기지 않는다.
        if (this.#closed) throw new Error("암호 처리 도중 채널이 닫혔다");
        return value;
      } catch (error) {
        this.close();
        throw error;
      }
    });
    // 거절도 다음 대기 작업에 전달한다. 각 작업은 닫힘을 확인한 뒤 자기 Promise를 거절한다.
    queue.tail = result.then(() => undefined, () => undefined);
    try {
      return await result;
    } finally {
      queue.frames -= 1;
      queue.bytes -= bytes.length;
      bytes.fill(0);
    }
  }

  async seal(plaintext) {
    if (this.#closed) throw new Error("닫힌 채널로는 봉인할 수 없다");
    const body = asBytes(plaintext);
    // 상한 검사가 암호화·할당보다 먼저다.
    if (body.length > MAX_PLAINTEXT_BYTES) throw new RangeError("평문이 고정 상한을 넘었다");
    return this.#enqueue(this.#sendQueue, body, (bytes) => this.#seal(bytes));
  }

  async #seal(body) {
    if (this.#sendSequence >= U64_MAX) throw new RangeError("송신 시퀀스가 소진됐다");
    const sequence = this.#sendSequence;
    const ciphertextLength = body.length + AES_GCM_TAG_BYTES;
    const aad = envelopeAad({
      protocolVersion: this.#protocolVersion,
      connectionId: this.#connectionId,
      direction: this.#sendDirection,
      sequence,
      senderFingerprint: this.#ownFingerprint,
      recipientFingerprint: this.#peerFingerprint,
      ciphertextLength,
    });
    const ciphertext = new Uint8Array(
      await crypto.subtle.encrypt(
        {
          name: "AES-GCM",
          iv: envelopeNonce(this.#sendDirection, sequence),
          additionalData: aad,
          tagLength: 128,
        },
        this.#sendKey,
        body,
      ),
    );
    if (ciphertext.length !== ciphertextLength) throw new RangeError("GCM 출력 길이가 어긋났다");
    this.#sendSequence = sequence + 1n;
    return { sequence, direction: this.#sendDirection, ciphertext };
  }

  async open({ sequence, direction, ciphertext }) {
    if (this.#closed) throw new Error("닫힌 채널로는 열 수 없다");
    const body = asBytes(ciphertext);
    // 크기 상한도, 방향도, 시퀀스도 전부 복호화 **이전에** 판정한다.
    if (body.length < AES_GCM_TAG_BYTES || body.length > MAX_CIPHERTEXT_BYTES) {
      this.close();
      throw new RangeError("암호문 길이가 계약 밖이다");
    }
    if (direction !== this.#receiveDirection) {
      this.close();
      throw new RangeError("봉투 방향이 어긋났다");
    }
    return this.#enqueue(this.#receiveQueue, body, (bytes) =>
      this.#open({ sequence, direction, body: bytes }));
  }

  async #open({ sequence, direction, body }) {
    const expected = this.#receiveSequence;
    if (expected >= U64_MAX) {
      this.close();
      throw new RangeError("수신 시퀀스가 소진됐다");
    }
    if (BigInt(sequence) !== expected) {
      // v1은 순서 어긋남을 **받아 주지 않는다**. 재생·건너뜀·중복 전부 여기서 닫힌다.
      this.close();
      throw new RangeError("수신 시퀀스가 정확히 단조가 아니다");
    }
    const aad = envelopeAad({
      protocolVersion: this.#protocolVersion,
      connectionId: this.#connectionId,
      direction,
      sequence: expected,
      senderFingerprint: this.#peerFingerprint,
      recipientFingerprint: this.#ownFingerprint,
      ciphertextLength: body.length,
    });
    let plaintext;
    try {
      plaintext = new Uint8Array(
        await crypto.subtle.decrypt(
          {
            name: "AES-GCM",
            iv: envelopeNonce(direction, expected),
            additionalData: aad,
            tagLength: 128,
          },
          this.#receiveKey,
          body,
        ),
      );
    } catch {
      this.close();
      throw new Error("GCM 검증이 실패했다");
    }
    if (plaintext.length > MAX_PLAINTEXT_BYTES) {
      this.close();
      throw new RangeError("복호된 평문이 고정 상한을 넘었다");
    }
    this.#receiveSequence = expected + 1n;
    return plaintext;
  }

  close() {
    this.#closed = true;
    this.#sendKey = null;
    this.#receiveKey = null;
  }
}
