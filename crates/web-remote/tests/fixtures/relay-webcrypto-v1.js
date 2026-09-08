// Task 1 고정 벡터를 **실제 브라우저 WebCrypto**로 재현하는 러너.
//
// 계획 Task 6 Step 5의 규정: transcript·ECDSA/ECDH·HKDF·SAS·nonce/AAD·AES-GCM은 배포되는
// 브라우저 크립토 모듈 **하나**에만 있고, 이 하네스는 그 모듈을 그대로 import한다. 복사본을
// 두면 셸이 틀려도 벡터가 초록으로 남는다 — 그건 게이트가 아니라 장식이다.
//
// 그래서 이 파일에는 도메인 분리 문자열도, `crypto.subtle`의 계약 연산 호출도 없다.
// 픽스처 JWK를 CryptoKey로 바꾸는 `importKey`만 러너의 몫이다: 실제 기기 신원키는
// 추출 불가능하게 만들어지므로 프로덕션 모듈이 JWK를 볼 일이 애초에 없다.
import {
  DIRECTION,
  RelaySecureChannel,
  buildTranscript,
  bytesFromHex,
  deriveSessionMaterial,
  deriveSharedSecret,
  envelopeAad,
  envelopeNonce,
  fixedBytes,
  hexFromBytes,
  importGcmKey,
  sha256,
  signTranscript,
  transcriptSalt,
  verifyPeerSignature,
} from "./relay-crypto.js";

const fixture = JSON.parse(document.getElementById("relay-fixture").textContent);

function assertEqual(actual, expected, label) {
  if (actual !== expected) {
    throw new Error(`${label}: expected ${expected}, got ${actual}`);
  }
}

/// 픽스처 JWK → CryptoKey. 오직 테스트 배선이다.
function importJwk(jwk, algorithm, usages) {
  return crypto.subtle.importKey("jwk", jwk, algorithm, false, usages);
}

const ECDSA_P256 = { name: "ECDSA", namedCurve: "P-256" };
const ECDH_P256 = { name: "ECDH", namedCurve: "P-256" };

function peerContract(peer) {
  return {
    identitySec1: fixedBytes(bytesFromHex(peer.identity_public_sec1_hex), 65, `${peer.role} 신원키`),
    ephemeralSec1: fixedBytes(
      bytesFromHex(peer.ephemeral_public_sec1_hex),
      65,
      `${peer.role} 임시키`,
    ),
  };
}

/// 봉투 한 방향. nonce/AAD는 프로덕션 함수로 직접 대조하고, 실제 봉인/해제는 프로덕션
/// `RelaySecureChannel`로 왕복시킨다 — 셸이 실제로 쓰는 바로 그 경로다.
async function verifyEnvelope(vector, sender, receiver, label) {
  const ciphertext = bytesFromHex(vector.ciphertext_and_tag_hex);
  const sequence = BigInt(vector.sequence);
  assertEqual(
    hexFromBytes(envelopeNonce(vector.direction, sequence)),
    vector.nonce_hex,
    `${label} nonce`,
  );
  assertEqual(
    hexFromBytes(
      envelopeAad({
        protocolVersion: fixture.protocol_version,
        connectionId: bytesFromHex(fixture.connection_id_hex),
        direction: vector.direction,
        sequence,
        senderFingerprint: sender.fingerprint,
        recipientFingerprint: receiver.fingerprint,
        ciphertextLength: ciphertext.length,
      }),
    ),
    vector.aad_hex,
    `${label} AAD`,
  );

  const sealed = await sender.channel.seal(bytesFromHex(vector.plaintext_hex));
  assertEqual(sealed.direction, vector.direction, `${label} 봉인 방향`);
  assertEqual(String(sealed.sequence), String(sequence), `${label} 봉인 시퀀스`);
  assertEqual(hexFromBytes(sealed.ciphertext), vector.ciphertext_and_tag_hex, `${label} encrypt`);

  const opened = await receiver.channel.open({
    sequence,
    direction: vector.direction,
    ciphertext,
  });
  assertEqual(hexFromBytes(opened), vector.plaintext_hex, `${label} decrypt`);
}

async function run() {
  if (!globalThis.crypto?.subtle) {
    throw new Error("WebCrypto SubtleCrypto is unavailable");
  }

  const connectionId = bytesFromHex(fixture.connection_id_hex);
  const transcript = buildTranscript({
    protocolVersion: fixture.protocol_version,
    connectionId,
    desktop: peerContract(fixture.desktop),
    device: peerContract(fixture.device),
  });
  assertEqual(hexFromBytes(transcript), fixture.transcript_hex, "handshake transcript");

  // 신원 지문 — 프로덕션 sha256으로 뽑는다.
  const [desktopFingerprint, deviceFingerprint] = await Promise.all([
    sha256(bytesFromHex(fixture.desktop.identity_public_sec1_hex)),
    sha256(bytesFromHex(fixture.device.identity_public_sec1_hex)),
  ]);

  // 양쪽 고정 서명이 프로덕션 검증기를 통과하는가.
  for (const peer of [fixture.desktop, fixture.device]) {
    const verified = await verifyPeerSignature(
      bytesFromHex(peer.identity_public_sec1_hex),
      bytesFromHex(peer.signature_raw_hex),
      transcript,
    );
    if (!verified) {
      throw new Error(`${peer.role} ECDSA signature did not verify`);
    }
  }

  // 그리고 브라우저가 **새로** 만든 서명을 Rust가 되검증한다(하네스 바깥에서).
  const deviceIdentity = {
    privateKey: await importJwk(fixture.device.identity_private_jwk, ECDSA_P256, ["sign"]),
  };
  const browserSignature = await signTranscript(deviceIdentity, transcript);

  const [desktopEphemeral, deviceEphemeral] = await Promise.all([
    importJwk(fixture.desktop.ephemeral_private_jwk, ECDH_P256, ["deriveBits"]),
    importJwk(fixture.device.ephemeral_private_jwk, ECDH_P256, ["deriveBits"]),
  ]);
  const [desktopShared, deviceShared] = await Promise.all([
    deriveSharedSecret(desktopEphemeral, bytesFromHex(fixture.device.ephemeral_public_sec1_hex)),
    deriveSharedSecret(deviceEphemeral, bytesFromHex(fixture.desktop.ephemeral_public_sec1_hex)),
  ]);
  assertEqual(hexFromBytes(desktopShared), fixture.shared_secret_hex, "desktop ECDH");
  assertEqual(hexFromBytes(deviceShared), fixture.shared_secret_hex, "device ECDH");

  assertEqual(
    hexFromBytes(await transcriptSalt(transcript)),
    fixture.hkdf_salt_hex,
    "HKDF salt",
  );

  const material = await deriveSessionMaterial(desktopShared, transcript);
  assertEqual(hexFromBytes(material.salt), fixture.hkdf_salt_hex, "세션 재료의 HKDF salt");
  assertEqual(
    hexFromBytes(material.desktopToDeviceKeyBytes),
    fixture.desktop_to_device_key_hex,
    "desktop-to-device HKDF",
  );
  assertEqual(
    hexFromBytes(material.deviceToDesktopKeyBytes),
    fixture.device_to_desktop_key_hex,
    "device-to-desktop HKDF",
  );
  assertEqual(material.sas, fixture.sas, "SAS");

  // 양쪽 채널을 프로덕션 클래스로 세운다. 같은 키를 두 역할이 반대 방향으로 잡는다.
  const [desktopToDeviceKey, deviceToDesktopKey] = await Promise.all([
    importGcmKey(material.desktopToDeviceKeyBytes),
    importGcmKey(material.deviceToDesktopKeyBytes),
  ]);
  const desktop = {
    fingerprint: desktopFingerprint,
    channel: new RelaySecureChannel({
      protocolVersion: fixture.protocol_version,
      connectionId,
      sendKey: desktopToDeviceKey,
      receiveKey: deviceToDesktopKey,
      sendDirection: DIRECTION.DESKTOP_TO_DEVICE,
      receiveDirection: DIRECTION.DEVICE_TO_DESKTOP,
      ownFingerprint: desktopFingerprint,
      peerFingerprint: deviceFingerprint,
    }),
  };
  const device = {
    fingerprint: deviceFingerprint,
    channel: new RelaySecureChannel({
      protocolVersion: fixture.protocol_version,
      connectionId,
      sendKey: deviceToDesktopKey,
      receiveKey: desktopToDeviceKey,
      sendDirection: DIRECTION.DEVICE_TO_DESKTOP,
      receiveDirection: DIRECTION.DESKTOP_TO_DEVICE,
      ownFingerprint: deviceFingerprint,
      peerFingerprint: desktopFingerprint,
    }),
  };

  await verifyEnvelope(fixture.desktop_to_device, desktop, device, "desktop-to-device");
  await verifyEnvelope(fixture.device_to_desktop, device, desktop, "device-to-desktop");

  document.body.dataset.status = "ok";
  document.body.textContent = `RELAY_WEBCRYPTO_OK:${hexFromBytes(browserSignature)}`;
}

run().catch((error) => {
  document.body.dataset.status = "error";
  document.body.textContent = `RELAY_WEBCRYPTO_ERROR:${error?.stack ?? error}`;
});
