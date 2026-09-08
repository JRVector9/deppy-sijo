# Relay 비UI 코어 main 독립 이관 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** #146 head 75bf2c9의 Relay 코어·알려진 기기 재접속 계약을 main 45e66cc에 이관해 app 무변경 독립 PR을 만든다.

**Architecture:** DRLY 고정 프레임, SHA-256 verifier 기반 자원 입장, 새 signed ephemeral handshake·저장 기기 키·Mac pin 기반 실제 인증을 분리한다. DB에는 raw grant 없이 verifier만 저장하고 재시작/회수/만료를 검증한다. app adapter는 후속 PR이며 repository 기본 reconnect 메서드는 fail-closed를 유지한다. main #155 HTTP helper와 의존성 버전은 그대로 보존한다.

**Tech Stack:** Rust sync tungstenite, relay-protocol/server, SQLite storage, web-remote, 브라우저 WebCrypto/IndexedDB, Node test runner, shell artifact digest 검증.

---

## 정확한 파일 경계

다음 파일은 `git diff 12ee4c7 75bf2c9 -- <path>` patch로 hunk를 이관한다. 기존 파일을 `git show > file`로 덮어쓰지 않는다. 신규 파일도 같은 patch 경로로 추가한다. web-remote push.rs의 main #155 변경은 patch 목록에 없으므로 유지된다.

- `.github/workflows/build-test.yml`
- `.github/workflows/relay-shell-release.yml`
- `crates/relay-protocol/src/lib.rs`
- `crates/relay-server/Cargo.toml`
- `crates/relay-server/src/core.rs`
- `crates/relay-server/src/main.rs`
- `crates/relay-server/tests/deploy_manifests.rs`
- `crates/relay-server/tests/transport_contract.rs`
- `crates/web-remote/assets/app.css`
- `crates/web-remote/assets/app.js`
- `crates/web-remote/assets/index.html`
- `crates/web-remote/assets/offline.html`
- `crates/web-remote/assets/pairing.html`
- `crates/web-remote/src/relay/crypto.rs`
- `crates/web-remote/src/relay/pairing.rs`
- `crates/web-remote/src/relay/repository.rs`
- `crates/web-remote/src/relay_client/handshake.rs`
- `crates/web-remote/src/relay_client/lifecycle.rs`
- `crates/web-remote/src/relay_client/link.rs`
- `crates/web-remote/src/relay_client/mod.rs`
- `crates/web-remote/src/relay_client/session.rs`
- `crates/web-remote/src/relay_client/worker.rs`
- `crates/web-remote/src/static_srv.rs`
- `crates/web-remote/tests/chrome_support/mod.rs`
- `crates/web-remote/tests/fixtures/relay-hello-v1.json`
- `crates/web-remote/tests/fixtures/relay-shell-v1.js`
- `crates/web-remote/tests/fixtures/relay-webcrypto-v1.js`
- `crates/web-remote/tests/fixtures/viewer-core-contract.js`
- `crates/web-remote/tests/relay_shell_chrome.rs`
- `crates/web-remote/tests/relay_shell_deploy.rs`
- `crates/web-remote/tests/relay_webcrypto_vectors.rs`
- `crates/web-remote/tests/viewer_core_chrome.rs`
- `deploy/relay-shell/README.md`
- `deploy/relay-shell/production/headers.conf`
- `deploy/relay-shell/production/shell.env.example`
- `deploy/relay-shell/staging/headers.conf`
- `deploy/relay-shell/staging/shell.env.example`
- `deploy/relay/production/relay-server.service`
- `deploy/relay/staging/relay-server.service`
- `web/relay-shell/build.sh`
- `web/relay-shell/index.html`
- `web/relay-shell/manifest.webmanifest`
- `web/relay-shell/relay-crypto.js`
- `web/relay-shell/relay-shell.css`
- `web/relay-shell/relay-shell.js`
- `web/relay-shell/relay-terminal.js`
- `web/relay-shell/sw.js`
- `web/relay-shell/tests/channel-concurrency.test.mjs`
- `web/relay-shell/tests/reconnect.test.mjs`
- `web/shared/mobile-theme.css`
- `web/shared/viewer-core.css`
- `web/shared/viewer-core.js`

별도 선택 hunk:

- `crates/storage/src/db.rs`: v38 reconnect_verifier migration, store/read method, approve/revoke 시 verifier 초기화, reconnect 테스트 2개, v38 migration 기대값만 포함한다. 기존 pending/재페어링/회수 행 reclamation 정책은 이관하지 않는다.
- `crates/web-remote/src/relay/repository.rs`: 위 목록에서 Conflict/PendingConflict enum·match hunk는 제외한다. reconnect 기본 메서드와 승인 실패 보상만 이관한다.
- `Cargo.lock`: relay-server의 libc/sha2 의존성 목록만 갱신한다. 최신 ureq/egui/quick-xml 및 다른 dependency resolution은 유지한다.
- `.gitignore`: `/web/relay-shell/dist/`만 추가한다.
- 이 계획, `docs/CODEX_HANDOFF.md`, 새 재접속 계약 설명 `docs/relay-core-main-extraction.md`.

제외/후속:

- 모든 `crates/app/`, native UI/settings/i18n/fleet/file-tree/font/IME/terminal 변경.
- `RelayPendingInsert::Conflict` 추가는 main `crates/app/src/relay_repository.rs::insert_pending`의 exhaustive match를 깨므로 부모 승인대로 storage/repository 관련 hunk를 adapter PR에 미룬다.
- secret macOS keychain, root Cargo.toml의 그 의존성, 앱 packaging/notarization, 그 요구만 추가하는 xtask hunk는 비필수이므로 제외한다.
- `scripts/relay-dev.sh`는 app 실행 어댑터까지 포함하므로 후속 app 연결 단계에 미룬다. 코어의 artifact build/verify는 `web/relay-shell/build.sh`로 독립 실행한다.
- `.github/workflows/relay-shell-release.yml`은 relay_shell_deploy 테스트가 참조하는 artifact digest/verify와 명시적 Publish BLOCKED 계약 때문에 포함한다. build-test의 relay-channel Node job은 실제 암호/등록 상태 회귀를 실행하므로 포함한다. 필수 Relay xtask hunk는 현재 없으며 일반 notarization hunk는 포함하지 않는다.
- shared viewer 합성을 위한 web-remote assets/static_srv 변경은 relay-shell/shared viewer 계약에 포함된다. app 실행·실기기 접속은 하지 않는다.
- DNS/TLS/배포 자격증명/외부 배포/실기기/24h soak는 BLOCKED이며 로컬 테스트로 PASS라고 하지 않는다.

### Task 1: 변경 경계 RED

- [ ] **Step 1: 기존 API로 새 wire tag와 DB column 계약을 먼저 검사한다.** #146의 reconnect_wire_accepts_only_the_new_fixed_record_sizes 테스트와 relay_reconnect_schema_stores_only_a_bounded_verifier 테스트만 먼저 이관한다. wire 테스트는 Hello frame의 tag byte를 바꾸므로 새 enum 없이 컴파일된다.

```rust
bytes[6] = 0x16;
assert!(RelayFrame::decode(&bytes).is_ok());
assert!(db.conn.prepare("SELECT reconnect_verifier FROM relay_devices").is_ok());
```

- [ ] **Step 2: 실제 실패를 확인한다.**

```sh
CARGO_BUILD_JOBS=2 cargo test -p relay-protocol --locked reconnect_wire_accepts_only -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p storage --locked relay_reconnect_schema -- --test-threads=1
```

예상: unknown reconnect tag / missing reconnect_verifier column assertion 실패. 문법·컴파일 실수를 기능 RED로 세지 않는다.

### Task 2: hunk 이관과 GREEN

- [ ] **Step 1: 위 정확한 경계를 patch로 적용한다.** 모든 patch에 git apply --check를 먼저 실행하고 문맥 충돌은 최신 main 의미를 확인해 수동 hunk 적응한다. 기존 RED 테스트의 같은 hunk는 두 번 적용하지 않는다. root manifest/keychain/app-adapter lock hunk는 제외한다.
- [ ] **Step 2: 경계 보존을 검사한다.**

```sh
git diff --exit-code origin/main -- crates/app crates/secret crates/terminal crates/i18n Cargo.toml crates/web-remote/src/push.rs scripts/package-macos.sh scripts/verify-macos-package.sh xtask/src/main.rs
```

예상: 차이 없음. 새 repository 기본 메서드는 fail-closed이므로 app adapter를 추가하지 않는다. app 의존 필수 충돌이 더 나오면 부모에게 심볼/위치를 보고한다.

- [ ] **Step 3: core와 브라우저 상태 회귀를 직렬 실행한다.**

```sh
CARGO_BUILD_JOBS=2 cargo test -p relay-protocol -p relay-server -p storage -p web-remote --locked -- --test-threads=1
node --test web/relay-shell/tests/*.test.mjs
```

예상: grant 고정 길이·64개/30일 상한·DB restart/회수/만료·route restore sync·Mac pin·IndexedDB commit 실패·새 링크 우선·stale async 결과·채널 nonce 순서 PASS. ignored Chrome/배포/장시간 검사는 별도 미실행으로 기록한다.

- [ ] **Step 4: 로컬 artifact만 생성·검증한다.** relay_shell_deploy가 허용하는 `.test` 오리진과 독립 `/private/tmp` dist로 build.sh/build.sh verify를 실행한다. 실제 DNS/TLS 검증이나 publish가 아니다. dev provisioning/app 실행 스크립트는 실행하지 않는다.

### Task 3: 리뷰·게이트·기록·PR

- [ ] **Step 1: bounded Codex CLI source 리뷰를 실행한다.** scope 파일의 암호/권한/회수·큐·shutdown·SQLite migration 경계를 읽기 전용으로 검토한다. 도구 조회 횟수를 제한하고 테스트/빌드/수정/서브에이전트는 금지한다. 확정 결함은 RED→최소 수정→GREEN으로 처리한다.
- [ ] **Step 2: 적정 게이트를 직렬 실행한다.** 전용 CARGO_TARGET_DIR=/private/tmp/deppy-relay-core-main-20260908/target, CARGO_BUILD_JOBS=2를 사용한다.

```sh
cargo fmt --all -- --check
git diff --check
CARGO_BUILD_JOBS=2 cargo clippy -p relay-protocol -p relay-server -p storage -p web-remote --all-targets --locked -- -D warnings
CARGO_BUILD_JOBS=2 cargo run --locked -p xtask -- check-boundary
```

- [ ] **Step 3: 한글 handoff/Obsidian 일지에 테스트·실패 접근·리뷰와 BLOCKED 항목을 기록한다.** 후속 app adapter 연결이 필요하므로 앱에서 기능이 켜졌다고 보고하지 않는다.
- [ ] **Step 4: 한국어 커밋/push/main 대상 새 PR을 생성한다.**

```sh
git add <위 계획의 실제 변경 파일>
git commit -m 'feat(relay): 비UI 재접속 코어를 main에 이관한다'
git push -u origin feat/relay-reconnect-core-main
gh pr create --base main --head feat/relay-reconnect-core-main --title 'feat(relay): 비UI 코어와 알려진 기기 재접속 main 이관' --body-file /private/tmp/deppy-relay-core-pr-body.md
```

원본 #146 rebase/force-push/수정/닫기, 배포, 앱 빌드·실행, 새 PR merge는 수행하지 않는다.
