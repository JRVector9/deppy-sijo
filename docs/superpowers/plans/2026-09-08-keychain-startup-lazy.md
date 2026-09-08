# Keychain 시작 접근 지연 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** 앱 생성과 Relay OFF, Settings 단순 열기에서 Keychain 접근을 없애고 기존 OAuth 이관을 명시적 기능 사용으로 지연한 main 독립 PR을 게시한다.

**Architecture:** App::new의 eager reconciliation을 제거한다. 기존 bounded ledger/migration 함수를 explicit credential/connector controller 작업에서 실행하여 물리 슬롯 계약과 오류 시 fail-closed를 보존한다. 실제 keyring-core 기본 저장소를 counting mock으로 교체하는 테스트는 제품 KeyringSecretStore와 실제 App 생성 경로를 관찰한다.

**Tech Stack:** Rust, SQLite, keyring-core test store, egui headless Context.

---

## 범위

기준 origin/main45e66cc, 전용 /private/tmp/deppy-keychain-startup-lazy-20260908, fix/keychain-startup-lazy. 수정은 crates/app/src/app.rs의 시작 및 explicit controller 접근 경계, 새 crates/app/src/keychain_startup_tests.rs, app Cargo.toml의 기존 keyring-core dev 의존성과 lock, 문서다. secret crate/원본146/R3 UI/기타 lane을 이관하지 않는다. native store 등록은 라이브러리 Store::new가 OS 조회 없이 구조체를 만드는 것으로 로컬 소스를 확인했다.

### Task 1: 실제 시작 경로 RED

- [x] app dev-dependencies에 `keyring-core = { workspace = true }`를 추가하고 test-only 모듈을 연결한다.
- [x] `CredentialStoreApi::build/search` 계수기를 기본 store로 설치한다. 같은 production KeyringSecretStore 명시적 get은 양성 대조다. RAII로 이전 default store를 복구한다.
- [x] legacy credential metadata가 있는 임시 DB로 실제 `App::new`를 호출하고 `assert_eq!(calls, 0)`을 실행한다. Relay OFF와 Settings Load/렌더도 같은 객체에서 확인한다. 테스트 종료 시 shutdown_on_exit와 임시 경로 정리를 수행한다.

```sh
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-keychain-startup-lazy-20260908/target cargo test -p deppy-sijo --bin deppy-sijo keychain_startup -- --test-threads=1
```

예상 RED: 기존 reconcile_startup_secrets_best_effort가 legacy get으로 store.build를 호출하여 계수가 0보다 크다. 컴파일 실패는 RED가 아니다.

### Task 2: 지연 이관과 회귀

- [x] App::new의 unconditional reconciliation을 제거하고 기존 best-effort wrapper는 explicit 사용 함수로 바꾼다. SecretStore 원본 I/O 정책은 변경하지 않는다.
- [x] Settings credential add/delete/reveal/orphan 작업은 기존 bounded reconciliation을 controller worker에서 수행한다. Load/일반 설정에는 호출하지 않는다. 오류는 기존 typed outcome으로 반환하며 비밀/payload를 로그에 넣지 않는다.
- [x] Connector의 실제 load_mcp_target/load_http_auth_binding 및 OAuth credential 사용 경계에서 이관을 실행한다. overview/server 설정 단순 조회는 DB만 읽는다. runtime resolver의 logical fallback 금지를 유지한다.
- [x] 기존 OAuth access/refresh/DCR 전체 migration/idempotence 테스트를 보존하고 explicit credential 사용에서 legacy가 실제 physical 슬롯으로 바뀌는 회귀를 추가한다. 거절 시 pointer/ledger 보존 및 재시도 성공을 검증한다.

```rust
assert_eq!(spy.calls(), 0);
let outcome = execute_settings_job(&mut db, &path, &redaction, reveal_job);
assert!(matches!(outcome.kind, SettingsOutcomeKind::CredentialRevealed { result: Ok(_), .. }));
assert!(spy.calls() > 0);
```

### Task 3: 검증과 리뷰

- [x] focused keychain/credential/OAuth/connector/settings 테스트를 직렬 실행한다.
- [x] bounded Codex CLI source 리뷰 후 확정 결함을 RED→수정→GREEN한다.
- [x] 최종 app 전체 테스트, app/secret strict Clippy, fmt/boundary/diff를 실행한다.

```sh
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-keychain-startup-lazy-20260908/target cargo test -p deppy-sijo -p secret --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-keychain-startup-lazy-20260908/target cargo clippy -p deppy-sijo -p secret --all-targets --locked -- -D warnings
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-keychain-startup-lazy-20260908/target cargo run --locked -p xtask -- check-boundary
cargo fmt --all -- --check
git diff --check
```

### Task 4: 게시

- [x] handoff/Obsidian 일지에 실제 결과·실패·후속 한계를 기록한다.
- [x] 한국어 commit/push 후 main 대상 PR을 생성하고 check 상태를 보고한다.

```sh
git commit -m 'fix(secret): 시작 Keychain 정리를 명시적 사용 시점으로 지연한다'
git push -u origin fix/keychain-startup-lazy
gh pr create --base main --head fix/keychain-startup-lazy --body-file /private/tmp/deppy-keychain-startup-pr-body.md
```

앱 빌드·재실행/외부 배포/merge/rebase/force-push는 하지 않는다. tests의 App::new는 GUI window 없이 임시 데이터만 사용한다. DNS/TLS/credentials 실환경 검증은 BLOCKED다.

## 승인된 추가 경계 및 회귀

- storage/db.rs v38 recovery_generation: startup snapshot의 정확한 이전 행을 삭제/재생성 ABA와 구분한다. IMMEDIATE transaction CAS/cleanup/ack와 현재 Staging 보존, callback rollback, v37 backfill을 검증한다.
- app/ui/agent_sessions.rs: App::new의 추가 has_secret 원인을 제거하기 위한 미조회 snapshot 및 명시적 버튼 조건. 실제 headless 렌더0/버튼 intent를 검증한다.
- connector-service/src/ports.rs + coordinator.rs: 이관 자신이 만든 revision 증가만 증명해서 최초 intent를 내부1회 재평가한다. 외부 writer가 migration 중/증명 이후 overview reload 중/target 재조회 중 개입하면 기존 stale 거절을 유지한다.
- Codex HIGH 수정: 이관이 직접 만든 exact slot+generation을 bounded pending에 기록하여 legacy cleanup 및 partial staging 실패를 같은 실행에서 재시도한다. 다른 worker 슬롯을 후보로 확대하지 않는다.
- 최종 gate에는 storage/connector-service 전체 test 및 strict Clippy도 포함한다. v38 번호 충돌은 일반 main merge + 재배치 + 전체 migration 재검증으로 해결한다.
