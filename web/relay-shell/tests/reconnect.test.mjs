import assert from "node:assert/strict";
import { test } from "node:test";
import * as C from "../relay-crypto.js";
import { createShell } from "../relay-shell.js";

const bytes = (n, size) => new Uint8Array(size).fill(n);
const identity = bytes(1, 32);
function registration() {
  return { version: 2, deviceId: bytes(2, 16), routeId: bytes(3, 16),
    desktopFingerprint: bytes(4, 32), identityFingerprint: identity,
    grant: bytes(5, 32), expiresAt: 2000 };
}

test("재접속 등록은 현재 identity와 만료를 엄격히 묶는다", () => {
  assert.equal(typeof C.validateRegistration, "function", "영속 등록 검증이 필요하다");
  assert.deepEqual(C.validateRegistration(registration(), identity, 1000), registration());
  assert.throws(() => C.validateRegistration(registration(), bytes(9, 32), 1000));
  assert.throws(() => C.validateRegistration(registration(), identity, 2000));
  for (const change of [{version: 1}, {grant: bytes(5, 31)}, {expiresAt: NaN},
    {deviceId: bytes(0, 16)}, {desktopFingerprint: bytes(4, 31)}]) {
    assert.throws(() => C.validateRegistration({...registration(), ...change}, identity, 1000));
  }
});

test("승인했던 Mac 공개키와 다른 서명자는 재접속하지 못한다", () => {
  assert.equal(typeof C.assertPinnedDesktop, "function", "Mac 신원 고정이 필요하다");
  C.assertPinnedDesktop(registration(), bytes(4, 32));
  assert.throws(() => C.assertPinnedDesktop(registration(), bytes(9, 32)));
});

test("등록 저장은 요청 성공만으로 완료되지 않고 transaction commit을 기다린다", async () => {
  assert.equal(typeof C.saveRegistration, "function", "원자 등록 저장이 필요하다");
  let transaction;
  let request;
  let stored;
  const db = {close() {}, transaction() {
    transaction = {objectStore: () => ({put(value) {stored = value; request = {}; return request;}}),
      abort() {transaction.onabort?.();}};
    return transaction;
  }};
  globalThis.indexedDB = {open() {
    const open = {result: db}; queueMicrotask(() => open.onsuccess()); return open;
  }};
  const valid = {...registration(), expiresAt: Math.floor(Date.now()/1000)+3600};
  let done = false;
  const saving = C.saveRegistration(valid, identity).then(() => {done = true;});
  await new Promise((resolve) => setImmediate(resolve));
  request.onsuccess?.();
  await Promise.resolve();
  assert.equal(done, false);
  transaction.oncomplete();
  await saving;
  assert.equal(done, true);
  assert.deepEqual(stored, valid);
  delete globalThis.indexedDB;
});

test("등록 transaction이 실패하거나 세대가 취소되면 저장 성공을 보고하지 않는다", async () => {
  for (const failure of ["storage", "generation"]) {
    let transaction;
    globalThis.indexedDB = {open() {
      const open = {result:{close() {}, transaction() {
        transaction = {objectStore:()=>({put:()=>({})}), abort() {transaction.onabort();}};
        return transaction;
      }}};
      queueMicrotask(()=>open.onsuccess()); return open;
    }};
    const abort = new AbortController();
    const saving = C.saveRegistration({...registration(), expiresAt:Math.floor(Date.now()/1000)+3600}, identity, abort.signal);
    const rejected = assert.rejects(saving, /registration-commit-failed/);
    await new Promise((resolve)=>setImmediate(resolve));
    if (failure === "generation") abort.abort();
    else transaction.onerror();
    await rejected;
    delete globalThis.indexedDB;
  }
});

test("재접속 wire는 페어링 admission과 구분하고 고정 길이를 강제한다", () => {
  assert.equal(C.FRAME_TYPE.RECONNECT_ADMISSION, 0x16);
  assert.equal(C.FRAME_TYPE.RECONNECT_SYNC, 0x17);
  const record = {frameType: C.FRAME_TYPE.RECONNECT_ADMISSION,
    routeId: bytes(3,16), connectionId: bytes(6,16), sequence: 0n, payload: bytes(5,32)};
  assert.equal(C.decodeFrame(C.encodeFrame(record)).frame.frameType, 0x16);
  assert.throws(() => C.encodeFrame({...record, payload: bytes(5,33)}));
  assert.equal(C.decodeFrame(C.encodeFrame({...record, frameType:C.FRAME_TYPE.RECONNECT_SYNC,
    payload:new Uint8Array()})).frame.payload.length, 0);
  assert.throws(() => C.encodeFrame({...record, frameType:C.FRAME_TYPE.RECONNECT_SYNC,
    payload:bytes(1,1)}));
});

test("URL 없는 등록 기기는 재접속 입장을 보내고 새 링크는 페어링이 우선한다", async () => {
  globalThis.document = {body: {dataset: {}}, getElementById: () => null};
  globalThis.history = {replaceState() {}};
  const saved = {...registration(), expiresAt: Math.floor(Date.now()/1000)+3600};
  for (const pairing of [false, true]) {
    globalThis.location = {pathname: "/", hash: pairing ? `#${Buffer.alloc(80, 7).toString("base64url")}` : ""};
    const handlers = new Map();
    const sent = [];
    const socket = {readyState: 1, addEventListener: (name, fn) => handlers.set(name, fn),
      send: (bytes) => sent.push(C.decodeFrame(bytes).frame), close() {}};
    const shell = createShell({endpoint: "wss://relay.example.test", socketFactory: () => socket,
      identityStore: {loadOrCreateIdentity: async () => ({fingerprint: identity}),
        loadRegistration: async () => saved}});
    await shell.start();
    assert.equal(typeof handlers.get("open"), "function", "등록 기기가 새 링크 없이 소켓을 열어야 한다");
    handlers.get("open")();
    assert.equal(sent[0].frameType, pairing ? C.FRAME_TYPE.DEVICE_ADMISSION : C.FRAME_TYPE.RECONNECT_ADMISSION);
    shell.dispose();
  }
  delete globalThis.document;
  delete globalThis.history;
  delete globalThis.location;
});

test("등록 완료 ACK 전에는 화면을 열지 않고 raw grant를 Mac에 보내지 않는다", async () => {
  globalThis.document = {body: {dataset: {}}, getElementById: () => null};
  globalThis.history = {replaceState() {}};
  globalThis.location = {pathname: "/", hash: `#${Buffer.alloc(80, 7).toString("base64url")}`};
  const handlers = new Map();
  const sent = [];
  let saved;
  const socket = {readyState: 1, addEventListener: (name, fn) => handlers.set(name, fn),
    send: (bytes) => sent.push(C.decodeFrame(bytes).frame), close() {}};
  const shell = createShell({endpoint: "wss://relay.example.test", socketFactory: () => socket,
    identityStore: {loadOrCreateIdentity: async () => ({fingerprint: identity}),
      saveRegistration: async (record) => {
        await new Promise((resolve) => setTimeout(resolve, 35)); saved = record;
      }}});
  await shell.start();
  handlers.get("open")();
  shell.state.desktopFingerprint = bytes(4,32);
  shell.state.routeId = bytes(3,16);
  shell.state.channel = {open: async ({ciphertext}) => ciphertext, close() {},
    seal: async (plaintext) => ({sequence: 0n, ciphertext: plaintext})};
  const expiresAt = Math.floor(Date.now()/1000)+3600;
  async function receive(message) {
    const encoded = C.encodeFrame({frameType: C.FRAME_TYPE.CIPHERTEXT,
      routeId: bytes(3,16), connectionId: shell.state.connectionId, sequence: 0n,
      payload: new TextEncoder().encode(JSON.stringify(message))});
    handlers.get("message")({data: encoded.buffer.slice(encoded.byteOffset, encoded.byteOffset+encoded.byteLength)});
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  await receive({type:"relay_registered", version:2, device_id:"02".repeat(16),
    route_id:"03".repeat(16), expires_at:expiresAt});
  assert.notEqual(shell.state.screen, "session", "게시 ACK 전에 화면을 열면 안 된다");
  const request = JSON.parse(new TextDecoder().decode(sent.at(-1).payload));
  assert.equal(request.type, "relay_register");
  assert.deepEqual(Object.keys(request).sort(), ["type","verifier","version"]);
  assert.equal(saved, undefined);
  await receive({type:"relay_ready", version:2, device_id:"02".repeat(16),
    route_id:"03".repeat(16), expires_at:expiresAt});
  await receive({type:"dashboard", workspaces:[]});
  assert.equal(shell.state.screen, "session");
  assert.equal(Buffer.from(await C.sha256(saved.grant)).toString("hex"), request.verifier);
  shell.dispose();
  delete globalThis.document; delete globalThis.history; delete globalThis.location;
});

test("네트워크 재시도는 다섯 번으로 제한하고 이전 socket 이벤트를 버린다", async () => {
  globalThis.document = {body: {dataset: {}}, getElementById: () => null};
  globalThis.history = {replaceState() {}};
  globalThis.location = {pathname: "/", hash: ""};
  const originalTimeout = globalThis.setTimeout;
  let retry;
  const delays = [];
  const sockets = [];
  globalThis.setTimeout = (fn, delay) => {
    if (delay <= 8000) {delays.push(delay); retry = fn; return 123456;}
    return originalTimeout(fn, delay);
  };
  const shell = createShell({endpoint: "wss://relay.example.test", socketFactory: () => {
    const handlers = new Map(); sockets.push(handlers);
    return {readyState:1, addEventListener:(key,fn)=>handlers.set(key,fn), send() {}, close() {}};
  }, identityStore: {loadOrCreateIdentity: async () => ({fingerprint: identity}),
    loadRegistration: async () => ({...registration(), expiresAt: Math.floor(Date.now()/1000)+3600})}});
  try {
    await shell.start();
    for (let attempt=0; attempt<5; attempt++) {
      const old = sockets.at(-1);
      let rejectOld;
      if (attempt === 0) {
        shell.state.connectionId = bytes(6,16);
        shell.state.channel = {close() {}, open: () => new Promise((_, reject) => {rejectOld=reject;})};
        const ciphertext = C.encodeFrame({frameType:C.FRAME_TYPE.CIPHERTEXT,routeId:bytes(3,16),
          connectionId:bytes(6,16),sequence:0n,payload:bytes(5,32)});
        old.get("message")({data:ciphertext.buffer});
        await new Promise((resolve)=>originalTimeout(resolve,0));
      }
      if (attempt === 1) {
        const restoring = C.encodeFrame({frameType:C.FRAME_TYPE.REJECTED,
          routeId:bytes(3,16), connectionId:bytes(6,16), sequence:0n,
          payload:new Uint8Array([0, 6])});
        old.get("message")({data:restoring.buffer});
        await new Promise((resolve)=>originalTimeout(resolve,0));
        assert.notEqual(shell.state.screen, "revoked", "Mac 복원 중 RouteBusy는 회수가 아니다");
      } else old.get("error")();
      assert.equal(typeof retry, "function", "알려진 기기의 일시적 단절은 제한된 재시도를 해야 한다");
      const resume = retry; retry = null; await resume();
      old.get("close")();
      if (rejectOld) {
        rejectOld(new Error("이전 세대 복호화 실패"));
        await new Promise((resolve)=>originalTimeout(resolve,0));
      }
      assert.equal(shell.state.finished, false, "이전 socket이 새 세대를 닫으면 안 된다");
    }
    sockets.at(-1).get("error")();
    assert.equal(retry, null);
    assert.deepEqual(delays, [1000,2000,4000,8000,8000]);
  } finally {
    globalThis.setTimeout = originalTimeout;
    shell.dispose();
    delete globalThis.document; delete globalThis.history; delete globalThis.location;
  }
});
