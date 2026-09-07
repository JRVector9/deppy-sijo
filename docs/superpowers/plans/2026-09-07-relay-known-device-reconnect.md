# Relay Known Device Reconnect Implementation Plan

> **For agentic workers:** Execute this approved plan inline, task by task with test-first evidence. Steps use checkbox (`- [ ]`) syntax for tracking. 사용자 승인 범위 안에서 추가 실행 선택 질문 없이 진행한다.

**Goal:** 최초 페어링한 브라우저가 같은 Mac으로 URL 없이 안전하게 재접속한다.

**Architecture:** 별도 bounded admission grant와 기존 E2EE 인증을 조합한다. 브라우저는 raw grant와 Mac pin을 IndexedDB에 원자 저장하고 Mac SQLite는 verifier만 보존한다. 승인·등록 ACK·활성 principal 검사를 기존 앱 싱크에 연결한다.

**Tech Stack:** Rust, relay-protocol/server, SHA-256, SQLite, WebCrypto, IndexedDB, Node/Chrome tests.

**Execution evidence:** protocol wire와 Node 핵심 경계는 실제 RED→GREEN을 확인했다.
server/storage/app RED 실행은 macOS `_dyld_start` 정체로 결과 없이 중단됐고 루트 지시로
Rust 실행을 일시 멈춘 동안 소스 구현을 계속했다. 이를 RED/PASS로 기록하지 않는다.
재개 뒤 protocol 13, server 36, storage relay 18 및 모든 prefix migration 1, app relay 38 PASS다.
독립 리뷰의 재시작 복원 race는 별도 RED1→GREEN으로 확인했고 ReconnectSync(0x17)를 추가했다.
Node 전체 16 PASS, Rust/Chrome shell 1 + WebCrypto vectors 5 PASS로 실제 등록·IndexedDB·
URL 없는 재접속·Mac pin 거절과 브라우저 생성 서명의 Rust 재검증을 확인했다.

---

### Task 1: 별도 입장 wire와 bounded Relay registry

**Files:** `crates/relay-protocol/src/lib.rs`, `crates/relay-server/src/core.rs`, `crates/relay-server/Cargo.toml`, `crates/web-remote/src/relay_client/session.rs`.

- [x] RED: 기존 frame을 인코딩하고 tag를 0x14로 바꾼 40바이트 payload decode가 성공해야 한다. 기존에는 UnknownFrameType 실패다.
  ```rust
  assert!(RelayFrame::decode(&reconnect_publish_bytes).is_ok());
  ```
- [x] Run: `CARGO_BUILD_JOBS=2 cargo test -p relay-protocol --lib --locked -- --test-threads=1`.
- [x] Implement: `ReconnectPublish = 0x14`, `ReconnectRevoke = 0x15`, `ReconnectAdmission = 0x16`, `ReconnectPublished = 0x24`; publish는 정확히 40바이트, 나머지는 32바이트. 서버는 64개 상한 registry, SHA-256 grant 비교, 절대 만료, ACK, 회수와 기존 pairing 독립성을 구현한다.
- [x] Regression GREEN: grant로 반복 입장, 잘못된 grant·만료·회수·다른 route·기기 게시·65번째 grant를 거절하는 실제 RelayCore 테스트. 최초 server RED 실행은 위 환경 제한으로 결과가 없으며 복원 race는 별도 RED→GREEN을 확인했다.
- [x] Run: `CARGO_BUILD_JOBS=2 cargo test -p relay-protocol -p relay-server --lib --locked -- --test-threads=1`.

### Task 2: verifier 영속 저장

**Files:** `crates/storage/src/db.rs`, `crates/web-remote/src/relay/repository.rs`, `crates/app/src/relay_repository.rs`.

- [x] Regression GREEN (RED 실행은 위 환경 제한): 승인된 기기에 verifier를 저장하고 DB를 재개방한 뒤 동일 verifier를 읽는다. 취소·만료된 기기 저장은 실패해야 한다.
  ```rust
  assert_eq!(repository.reconnect_verifier(device_id)?, Some(verifier));
  ```
- [x] Implement: nullable 32바이트 verifier column migration, device 판정과 verifier update를 같은 transaction에서 수행. 목록은 기존 device 상한을 따른다. raw grant는 저장 API에 존재하지 않는다.
- [x] Run: `CARGO_BUILD_JOBS=2 cargo test -p storage --lib --locked relay -- --test-threads=1`.
- [x] Run: adapter 검사는 아래 app `--bin deppy-sijo relay` 명령에 포함해 실행했다.

### Task 3: 승인 제어 메시지와 principal 검사

**Files:** `crates/app/src/app.rs`, `crates/web-remote/src/relay/`.

- [x] Regression GREEN (RED 실행은 위 환경 제한): 승인 채널에 첫 암호 제어 메시지가 dashboard보다 먼저 나온다. 다른 key·취소·만료 principal은 command와 push 모두 거절한다. Rust 싱크 검사와 Chrome의 실제 복호화 검사를 함께 확인했다.
  ```rust
  assert_eq!(first_message["type"], "relay_registered");
  ```
- [x] Implement: mailbox activation에 `RelayDeviceRecord` 전달, sink에 active principal 보존, register verifier 처리, Relay ACK 뒤 `relay_ready`, Relay 재입장마다 verifier 재게시. 저장·게시 실패는 채널 종료.
- [x] Run: `CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo --locked relay -- --test-threads=1`.

### Task 4: 브라우저 영속 등록과 URL 없는 재접속

**Files:** `web/relay-shell/relay-crypto.js`, `web/relay-shell/relay-shell.js`, `web/relay-shell/tests/reconnect.test.mjs`, Chrome fixture.

- [x] RED: 새 pairing URL 없는 정상 저장 레코드는 reconnect admission을 보낸다. 다른 Mac pin이면 KnownDevice/DOM 생성 전에 거절한다. 새 링크가 있으면 pairing proof를 보낸다.
  ```javascript
  assert.equal(sent[0].frameType, C.FRAME_TYPE.RECONNECT_ADMISSION);
  ```
- [x] Implement: IndexedDB transaction complete를 기다리는 versioned 등록 레코드, browser grant 생성/verifier 전달, 등록 완료 ACK 이후 원자 저장, Mac pin 대조, generation fencing, 제한된 네트워크 재시도.
- [x] Run: `node --test web/relay-shell/tests/*.test.mjs`.
- [x] Run: `CARGO_BUILD_JOBS=2 cargo test -p web-remote --test relay_shell_chrome --test relay_webcrypto_vectors --locked -- --include-ignored --nocapture --test-threads=1`.

### Task 5: 검토·기록·PR 갱신

**Files:** `docs/CODEX_HANDOFF.md`, 위 소스·테스트·설계·계획.

- [ ] 최종 strict clippy/boundary 재실행은 사용자 추가 테스트 중단 지시로 미실행. focused Rust/Chrome/Node 통과 후 Clippy의 큰 enum·중첩 if 두 건을 정리했고 마지막 fmt만 PASS다. 정리 후 재검증을 PASS로 주장하지 않는다.
- [x] Review diff for raw grant persistence, pin bypass, stale generation, unauthenticated DOM, expiry/revoke races, queue bounds. 독립 리뷰의 복원 race 수정 뒤 확정 잔여 finding 0.
- [x] Record exact executed results and failures. DNS/TLS·배포·실기기·24h soak remain **BLOCKED**.
- [ ] Commit and normal push to existing PR #146 branch; update PR body around final implementation and actual evidence. No app build/restart, rebase, force-push, merge or deploy.
