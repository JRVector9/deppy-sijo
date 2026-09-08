# Relay 비UI 코어 main 독립 이관 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** #146 head 75bf2c9의 Relay 코어·알려진 기기 재접속 계약을 main 45e66cc 기반 R1과 그 위 R2 셸의 두 stacked PR로 이관한다.

**Architecture:** DRLY 고정 프레임, SHA-256 verifier 기반 자원 입장, 새 signed ephemeral handshake·저장 기기 키·Mac pin 기반 실제 인증을 분리한다. DB에는 raw grant 없이 verifier만 저장하고 재시작/회수/만료를 검증한다. app adapter는 후속 PR이며 repository 기본 reconnect 메서드는 fail-closed를 유지한다. main #155 HTTP helper와 의존성 버전은 그대로 보존한다.

**Tech Stack:** Rust sync tungstenite, relay-protocol/server, SQLite storage, web-remote, 브라우저 WebCrypto/IndexedDB, Node test runner, shell artifact digest 검증.

---

## 최종 R1 / R2 경계

부모의 제품 커밋 전 재분리 지시를 적용한다. DAG는 main → R1 `feat/relay-reconnect-core-main` → R2 `feat/relay-shell-main`이다. 이 계획은 R1을 추적하며 R2는 별도 `2026-09-08-relay-shell-main-extraction.md` 계획으로 마감한다. 기존 파일은 patch hunk로만 이관했다.

R1 파일:

- `crates/relay-protocol/src/lib.rs`
- `crates/relay-server/Cargo.toml`, `src/core.rs`, `src/main.rs`, `tests/deploy_manifests.rs`, `tests/transport_contract.rs`, `tests/shutdown.rs`
- `crates/storage/src/db.rs`의 아래 reconnect 전용 hunk
- `crates/web-remote/src/relay/{crypto,pairing,repository}.rs`
- `crates/web-remote/src/relay_client/{handshake,lifecycle,link,mod,session,worker}.rs`
- `deploy/relay/{staging,production}/relay-server.service`
- `Cargo.lock`의 relay-server 의존성 2개와 R1 설명/계획/handoff

R2 파일:

- `web/relay-shell/**`, `web/shared/**`, `deploy/relay-shell/**`
- `.github/workflows/relay-shell-release.yml`, build-test.yml의 Node relay-channel job, `.gitignore`의 dist
- `crates/web-remote/assets/{app.css,app.js,index.html,offline.html,pairing.html}`, `src/static_srv.rs`
- 신규 Chrome support/tests/fixtures와 기존 `relay_webcrypto_vectors.rs`/fixture JS 변경
- `handshake.rs::the_v1_hello_fixture_matches_this_implementation` 한 테스트 hunk: 신규 JSON fixture에 의존하므로 R2와 함께 추가한다. R1의 프로덕션 handshake와 나머지 unit 테스트는 그대로 유지한다.

R1 검증은 staged tree를 `git write-tree`와 `git archive`로 별도 `/private/tmp` 디렉터리에 추출해 수행한다. working tree의 R2 hunk는 보존하되, 검증 트리에 R2의 web/신규 fixture가 없음을 확인한다. R1만 commit/push/main PR 생성 후 R2 브랜치를 R1 제품 커밋에서 만들고 남은 hunk를 commit/push/R1 대상 PR로 생성한다.

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

- [x] **Step 1: 기존 API로 새 wire tag와 DB column 계약을 먼저 검사한다.** #146의 reconnect_wire_accepts_only_the_new_fixed_record_sizes 테스트와 relay_reconnect_schema_stores_only_a_bounded_verifier 테스트만 먼저 이관한다. wire 테스트는 Hello frame의 tag byte를 바꾸므로 새 enum 없이 컴파일된다.

```rust
bytes[6] = 0x16;
assert!(RelayFrame::decode(&bytes).is_ok());
assert!(db.conn.prepare("SELECT reconnect_verifier FROM relay_devices").is_ok());
```

- [x] **Step 2: 실제 실패를 확인한다.**

```sh
CARGO_BUILD_JOBS=2 cargo test -p relay-protocol --locked reconnect_wire_accepts_only -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p storage --locked relay_reconnect_schema -- --test-threads=1
```

예상: unknown reconnect tag / missing reconnect_verifier column assertion 실패. 문법·컴파일 실수를 기능 RED로 세지 않는다.

### Task 2: hunk 이관과 GREEN

- [x] **Step 1: 위 정확한 경계를 patch로 적용한다.** 모든 patch에 git apply --check를 먼저 실행하고 문맥 충돌은 최신 main 의미를 확인해 수동 hunk 적응한다. 기존 RED 테스트의 같은 hunk는 두 번 적용하지 않는다. root manifest/keychain/app-adapter lock hunk는 제외한다.
- [x] **Step 2: 경계 보존을 검사한다.**

```sh
git diff --exit-code origin/main -- crates/app crates/secret crates/terminal crates/i18n Cargo.toml crates/web-remote/src/push.rs scripts/package-macos.sh scripts/verify-macos-package.sh xtask/src/main.rs
```

예상: 차이 없음. 새 repository 기본 메서드는 fail-closed이므로 app adapter를 추가하지 않는다. app 의존 필수 충돌이 더 나오면 부모에게 심볼/위치를 보고한다.

- [x] **Step 3: core와 브라우저 상태 회귀를 직렬 실행한다.** (초기 결합 검증 완료, R1은 독립 archive로 추가 검증)

```sh
CARGO_BUILD_JOBS=2 cargo test -p relay-protocol -p relay-server -p storage -p web-remote --locked -- --test-threads=1
node --test web/relay-shell/tests/*.test.mjs
```

예상: grant 고정 길이·64개/30일 상한·DB restart/회수/만료·route restore sync·Mac pin·IndexedDB commit 실패·새 링크 우선·stale async 결과·채널 nonce 순서 PASS. ignored Chrome/배포/장시간 검사는 별도 미실행으로 기록한다.

- [x] **Step 4: 로컬 artifact만 생성·검증한다.** relay_shell_deploy가 허용하는 `.test` 오리진과 독립 `/private/tmp` dist로 build.sh/build.sh verify를 실행한다. 실제 DNS/TLS 검증이나 publish가 아니다. dev provisioning/app 실행 스크립트는 실행하지 않는다.

### Task 3: 리뷰·게이트·기록·PR

- [x] **Step 1: bounded Codex CLI source 리뷰를 실행한다.** scope 파일의 암호/권한/회수·큐·shutdown·SQLite migration 경계를 읽기 전용으로 검토한다. 도구 조회 횟수를 제한하고 테스트/빌드/수정/서브에이전트는 금지한다. 확정 결함은 RED→최소 수정→GREEN으로 처리한다.
- [x] **Step 2: 적정 게이트를 직렬 실행한다.** 전용 CARGO_TARGET_DIR=/private/tmp/deppy-relay-core-main-20260908/target, CARGO_BUILD_JOBS=2를 사용한다.

```sh
cargo fmt --all -- --check
git diff --check
CARGO_BUILD_JOBS=2 cargo clippy -p relay-protocol -p relay-server -p storage -p web-remote --all-targets --locked -- -D warnings
CARGO_BUILD_JOBS=2 cargo run --locked -p xtask -- check-boundary
```

- [x] **Step 3: 한글 handoff/Obsidian 일지에 테스트·실패 접근·리뷰와 BLOCKED 항목을 기록한다.** 후속 app adapter 연결이 필요하므로 앱에서 기능이 켜졌다고 보고하지 않는다.
- [x] **Step 4: 한국어 커밋/push/main 대상 새 PR을 생성한다.**

```sh
# R1 index는 선택 hunk로 구성한다. handshake.rs 끝의 R2 fixture test는 stage하지 않는다.
git diff --cached --name-only
git commit -m 'feat(relay): 비UI 재접속 코어를 main에 이관한다'
git push -u origin feat/relay-reconnect-core-main
gh pr create --base main --head feat/relay-reconnect-core-main --title 'feat(relay): 비UI 코어와 알려진 기기 재접속 main 이관' --body-file /private/tmp/deppy-relay-core-pr-body.md
```

원본 #146 rebase/force-push/수정/닫기, 배포, 앱 빌드·실행, 새 PR merge는 수행하지 않는다.

## 범위 재분리 후 실행 체크

- [x] R1 staged tree에서 셸/신규 fixture 부재와 Rust 독립 compile/test를 확인한다.
- [x] R1 staged tree의 strict Clippy/fmt/boundary와 source 리뷰를 마감한다.
- [x] R1 source만 commit/push/main PR을 생성한다.
- [x] R2 브랜치로 남은 동일 hunk를 옮겨 Chrome/Node/artifact 검증과 stacked PR을 마감한다.

초기 합친 working tree 검증은 732 Rust PASS/4 ignored, Node16 및 로컬 artifact build/verify PASS였으나 R1 독립 게이트를 대신하지 않는다.

## 리뷰 수정 추가 계약

- macOS의 비차단 listener에서 accepted socket 상속으로 정상 분할 HTTP 헤더가 EOF 처리되는 실제 RED를 확인했다. serve에서 차단 모드를 명시한다.
- Linux의 slow-header 도중 SIGTERM은 종료되지 않는 실제 RED였다. WorkerSlot으로 제한된 worker 목록에 핸드셰이크 이전 TCP 복제본도 보존하고 모든 socket.shutdown 후 join한다.
- 초기 20ms당1바이트 재현은 tungstenite 작은 패킷 방어가 먼저 종료시켜 유효하지 않았다. 100ms 간격으로 2초를 관측하는 회귀로 교정했다.
- 신규 tests/shutdown.rs는 실제 자식 relay-server와 루프백만 쓰고 Drop에서 자신이 만든 PID를 정리한다. 앱을 빌드/실행하지 않는다.

- 추가 리뷰 수정: pump는 rejection을 flush한 뒤 dispatch의 직접 shutdown을 호출한다. 실제 malformed admission→Rejected→EOF 회귀가 RED→GREEN이며 문자열 검사 하나를 이 실제 테스트로 교체했다. 좁은 최종 재리뷰에서 남은 확정 결함 없음.
