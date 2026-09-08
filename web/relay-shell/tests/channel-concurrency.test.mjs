import assert from "node:assert/strict";
import { test } from "node:test";
import {
  DIRECTION,
  PROTOCOL_VERSION,
  MAX_PLAINTEXT_BYTES,
  RelaySecureChannel,
  importGcmKey,
} from "../relay-crypto.js";

const encoder = new TextEncoder();

async function channels() {
  const down = await importGcmKey(new Uint8Array(32).fill(1));
  const up = await importGcmKey(new Uint8Array(32).fill(2));
  const make = (desktop) => new RelaySecureChannel({
    protocolVersion: PROTOCOL_VERSION,
    connectionId: new Uint8Array(16).fill(3),
    sendKey: desktop ? down : up,
    receiveKey: desktop ? up : down,
    sendDirection: desktop ? DIRECTION.DESKTOP_TO_DEVICE : DIRECTION.DEVICE_TO_DESKTOP,
    receiveDirection: desktop ? DIRECTION.DEVICE_TO_DESKTOP : DIRECTION.DESKTOP_TO_DEVICE,
    ownFingerprint: new Uint8Array(32).fill(desktop ? 4 : 5),
    peerFingerprint: new Uint8Array(32).fill(desktop ? 5 : 4),
  });
  return { desktop: make(true), device: make(false) };
}

test("동시 송신도 서로 다른 순번과 nonce를 사용한다", async () => {
  const { desktop, device } = await channels();
  const messages = ["첫 메시지", "둘째 메시지", "셋째 메시지"];
  const frames = await Promise.all(messages.map((message) => desktop.seal(encoder.encode(message))));
  assert.deepEqual(frames.map((frame) => frame.sequence), [0n, 1n, 2n]);
  const decoded = [];
  for (const frame of frames) decoded.push(new TextDecoder().decode(await device.open(frame)));
  assert.deepEqual(decoded, messages);
});

test("연속으로 도착한 암호문을 복호화 완료 순서와 무관하게 순서대로 처리한다", async () => {
  const { desktop, device } = await channels();
  const first = await desktop.seal(encoder.encode("하나"));
  const second = await desktop.seal(encoder.encode("둘"));
  const values = await Promise.all([device.open(first), device.open(second)]);
  assert.deepEqual(values.map((value) => new TextDecoder().decode(value)), ["하나", "둘"]);
  assert.equal(device.closed, false);
});

test("같은 암호문의 동시 재생은 한 번만 열리고 채널을 닫는다", async () => {
  const { desktop, device } = await channels();
  const frame = await desktop.seal(encoder.encode("한 번만"));
  const results = await Promise.allSettled([device.open(frame), device.open(frame)]);
  assert.deepEqual(results.map((result) => result.status), ["fulfilled", "rejected"]);
  assert.equal(device.closed, true);
});

test("복호화 도중 닫힌 채널은 늦게 완성된 평문을 내보내지 않는다", async (t) => {
  const { desktop, device } = await channels();
  const frame = await desktop.seal(encoder.encode("폐기할 평문"));
  const decrypt = crypto.subtle.decrypt.bind(crypto.subtle);
  let entered;
  const started = new Promise((resolve) => { entered = resolve; });
  let release;
  const blocked = new Promise((resolve) => { release = resolve; });
  t.mock.method(crypto.subtle, "decrypt", async (...args) => {
    entered();
    await blocked;
    return decrypt(...args);
  });
  const pending = device.open(frame);
  await started;
  device.close();
  release();
  await assert.rejects(pending);
});

test("대기 중인 암호화 작업은 고정 상한을 넘으면 채널을 닫는다", async () => {
  const { desktop } = await channels();
  const pending = Array.from({ length: 100 }, () => desktop.seal(encoder.encode("대기")));
  const results = await Promise.allSettled(pending);
  assert.equal(desktop.closed, true);
  assert(results.some((result) => result.status === "rejected"));
});

test("메시지 수가 적어도 보관할 총 바이트 상한을 넘으면 거부한다", async () => {
  const { desktop } = await channels();
  const payload = new Uint8Array(MAX_PLAINTEXT_BYTES);
  const pending = Array.from({ length: 5 }, () => desktop.seal(payload));
  const results = await Promise.allSettled(pending);
  assert.equal(desktop.closed, true);
  assert(results.some((result) => result.status === "rejected"));
});

test("암호화 실패 뒤 대기 중인 후속 송신도 실패한다", async (t) => {
  const { desktop } = await channels();
  t.mock.method(crypto.subtle, "encrypt", async () => { throw new Error("암호 장치 실패"); });
  const pending = [desktop.seal(encoder.encode("앞")), desktop.seal(encoder.encode("뒤"))];
  const results = await Promise.allSettled(pending);
  assert.deepEqual(results.map((result) => result.status), ["rejected", "rejected"]);
  assert.equal(desktop.closed, true);
});

test("대기하는 송신 데이터는 호출자가 바꿔도 달라지지 않는다", async () => {
  const { desktop, device } = await channels();
  const first = desktop.seal(encoder.encode("앞선 메시지"));
  const bytes = encoder.encode("보존할 메시지");
  const second = desktop.seal(bytes);
  bytes.fill(0);
  await device.open(await first);
  const decoded = await device.open(await second);
  assert.equal(new TextDecoder().decode(decoded), "보존할 메시지");
});
