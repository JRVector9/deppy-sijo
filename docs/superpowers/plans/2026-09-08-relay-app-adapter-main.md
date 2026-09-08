# Relay 앱 어댑터 stacked 이관 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** #162 final206a350 위에 #146의 Relay 앱 활성화 hunk만 연결한 독립 R3 PR을 게시한다.

**Architecture:** UI는 typed intent를 생성하고 controller logic/Relay worker가 DB·키·네트워크를 소유한다. Tailscale과 Relay는 독립 토글과 공유 SessionCore를 사용한다. Relay 식별키는 single-instance lock을 소유한 lazy supplier를 통해 필요할 때만 읽고 DB에는 verifier만 저장한다.

**Tech Stack:** Rust/egui controller, web-remote RelayWorker/handshake, SQLite v38, SecretStore spy, 5 locale catalog.

---

## 확정 범위와 의존성

DAG: main → R1 #161 → R2 #162 → R3 `feat/relay-app-adapter-main`.

- 생성: `crates/app/src/relay_pairing.rs`, `crates/app/src/relay_reconnect_tests.rs`.
- 수정: `crates/app/src/relay_repository.rs`의 single-instance lock/reconnect 메서드/Conflict mapping/테스트.
- 수정: `crates/storage/src/db.rs`의 R1에서 미룬 pending conflict·동일 기기 재승인·회수 행 수거 정책과 해당 테스트. v38 verifier 저장/회수는 R1을 보존한다.
- 수정: `crates/web-remote/src/relay/repository.rs`의 Conflict enum/승인 결과 mapping만.
- 수정: `crates/app/src/app.rs`의 Relay mailbox/state/sink, shared SessionCore, App 필드·생성 주입, enable/disable/페어링/회수·시계, controller intent/logic/설정 projection 및 해당 테스트 hunk.
- 수정: `crates/app/src/main.rs`의 relay_pairing 모듈 및 Arc single-instance lock 전달, `crates/app/Cargo.toml`의 relay-protocol 직접 의존성과 `Cargo.lock`의 앱 의존 목록 한 줄.
- 수정: `crates/app/src/ui/settings.rs`의 Relay 독립 category/뷰모델/intent/상태/QR·승인·회수 및 해당 테스트. 다른 일반 UI 변경은 제외한다.
- 수정: `crates/i18n/locales/{en-US,ko-KR,ja-JP,zh-Hans,zh-Hant}/messages.txt`의 settings.relay.* 추가만. fleet/file-tree 키는 제외한다.
- `crates/app/src/config.rs`는 이미 main에 독립 web.enabled/relay.enabled 계약이 있으므로 원본의 workspace_order hunks를 가져오지 않는다.
- 원본 package/verify/xtask hunks는 일반 공증 변경이며 앱 활성화 필수가 아니다. 제외한다. relay-dev.sh 역시 실제 개발 실행/좌표 주입 도구로 core 활성화에 필수이지 않으므로 실행·이관하지 않는다.
- docs/CODEX_HANDOFF.md, 이 계획, R3 계약 설명만 문서에 추가한다.

제외: fleet/file-tree/IME/font/일반 UI/다른 세션 변경, HTTP 마이그레이션, 원본 secret crate/root manifest 확대. 앱 빌드·재실행/외부 배포/rebase/force-push/원본146변경/새PRmerge는 하지 않는다.

## Keychain 정책 경계

현재 main의 `App::new → reconcile_startup_secrets_best_effort`는 기존 OAuth ledger를 위해 eager list/get/has/delete를 수행한다. 부모가 R3 범위를 **Relay 신규 경로 시작/OFF 접근0**으로 확정했으며 이 기존 경로는 변경하지 않는다.

반복 Keychain 팝업의 앱 전체 시작0 계약은 별도 R4 `fix/keychain-startup-lazy`를 최신 main에서 수행한다. R4는 앱 생성/Relay OFF/Settings 단순 열기에서0, 명시적 credential/connector/Relay controller action에서만 bounded migration/access로 바꾸고 기존 OAuth migration 계약을 보존한다.

### Task 1: adapter 실제 RED와 hunk 지도

- [ ] **Step 1: 원본 reconnect verifier 회귀만 먼저 이관하고 현재 open 시그니처에 맞춘다.**

```rust
assert!(repository.store_reconnect_verifier(paired.device_id, &paired.public_key, &[6; 32], ISSUED_AT + 2).unwrap());
```

- [ ] **Step 2: 실제 assertion/runtime failure를 실행한다.**

```sh
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-relay-app-adapter-main-20260908/target cargo test -p deppy-sijo --bin deppy-sijo --locked relay_reconnect_verifier_survives_adapter_restart_and_revocation_erases_it -- --test-threads=1
```

예상: R1 trait 기본 메서드의 Relay reconnect storage unavailable로 FAIL. compile 실패는 RED로 세지 않는다.

- [ ] **Step 3: 원본 diff를 hunk별로 분류하고 exact file boundary를 확인한다.**

```sh
git diff 12ee4c7 75bf2c9 -- crates/app/src/app.rs crates/app/src/ui/settings.rs
git diff --exit-code HEAD -- crates/secret crates/terminal crates/app/src/fonts.rs crates/app/src/ui/file_tree.rs crates/app/src/ui/fleet.rs
```

### Task 2: 최소 앱 연결과 startup/OFF spy

- [ ] **Step 1: 위 hunk만 git apply --check 후 적용한다.** 기존 app #155/egui0.36 코드를 보존하며 원본 파일 통째로 checkout하지 않는다. 큰 혼합 hunk는 Relay 구간만 재구성한다.
- [ ] **Step 2: 실제 lazy identity supplier/worker 경로를 counting SecretStore로 검사한다.** Relay sink 생성과 worker OFF에서 has/get/set/delete/list 횟수0을 확인하고, 명시적으로 공급자를 호출한 경우에만 키 생성/조회가 발생하는 양성 대조를 둔다. UI 생성/렌더에는 공급자 호출을 넣지 않는다.
- [ ] **Step 3: focused GREEN을 직렬 실행한다.**

```sh
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-relay-app-adapter-main-20260908/target cargo test -p deppy-sijo --bin deppy-sijo --locked relay -- --test-threads=1
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-relay-app-adapter-main-20260908/target cargo test -p storage --locked relay -- --test-threads=1
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-relay-app-adapter-main-20260908/target cargo test -p web-remote --locked relay -- --test-threads=1
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-relay-app-adapter-main-20260908/target cargo test -p i18n --locked
```

예상: pending 충돌/동일기기 재페어링/revoke/restart/verifier/expired/stale claim, 독립 토글/shared core/키접근0 PASS. raw grant가 DB/log/config에 들어가지 않는 기존 scan/계약을 보존한다.

### Task 3: bounded 리뷰·전체 영향 검사

- [ ] **Step 1: 실제 source diff에 Codex CLI read-only 리뷰를 실행한다.** 최대 도구 횟수를 정해 신뢰 경계/키 호출 위치/controller 의도 유실/권한·회수/worker 종료를 검토한다. 확정 finding은 RED→최소수정→GREEN한다.
- [ ] **Step 2: 앱·의존 계약을 검증한다.**

```sh
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-relay-app-adapter-main-20260908/target cargo test -p deppy-sijo --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-relay-app-adapter-main-20260908/target cargo clippy -p deppy-sijo -p storage -p web-remote --all-targets --locked -- -D warnings
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-relay-app-adapter-main-20260908/target cargo run --locked -p xtask -- check-boundary
cargo fmt --all -- --check
git diff --check
git diff --exit-code origin/feat/relay-shell-main -- crates/secret crates/terminal crates/app/src/fonts.rs crates/app/src/ui/file_tree.rs crates/app/src/ui/fleet.rs crates/web-remote/src/push.rs scripts/package-macos.sh scripts/verify-macos-package.sh xtask/src/main.rs Cargo.toml
```

### Task 4: handoff·일지·stacked 게시

- [ ] **Step 1: 실제 결과/실패 접근/수정/외부BLOCKED를 기록한다.** 다른 lane handoff는 보존한다. GitGuardian R2 공개 fixture 상태를 제품secret과 구분한다.
- [ ] **Step 2: 한국어 commit/push/R2 base PR을 게시한다.**

```sh
git diff --name-only
git commit -m 'feat(relay): 앱 어댑터와 독립 연결 설정을 이관한다'
git push -u origin feat/relay-app-adapter-main
gh pr create --base feat/relay-shell-main --head feat/relay-app-adapter-main --title 'feat(relay): 앱 어댑터와 독립 연결 설정' --body-file /private/tmp/deppy-relay-app-adapter-pr-body.md
```

R3 게시 후 부모가 지시한 별도 R4를 최신 main에서 시작한다. DNS/TLS/credentials/외부 배포/실기기/24h soak는 BLOCKED이며 PASS로 대체하지 않는다.
