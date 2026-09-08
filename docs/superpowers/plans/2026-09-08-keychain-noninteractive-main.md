# Keychain Noninteractive Main Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** #146의 macOS Keychain 비대화형 정책만 exact main45e66cc에 독립 이관한다.

**Architecture:** crates/secret의 macOS 어댑터에서 모든 SecItem query에 UIFail을 적용한다. 기존 User(login) keychain/service/account 범위를 유지하며 inventory도 UISkip 대신 실패를 전달한다. 기존 테스트 mock과 다른 플랫폼 구현, startup 초기화 API는 유지한다.

**Tech Stack:** Rust, CoreFoundation0.10, Security.framework3, macOS SecItem API.

---

승인된 범위를 현재 에이전트가 inline 실행한다. 파일: crates/secret/src/macos.rs(새 어댑터·순수 query/status 회귀), crates/secret/src/lib.rs(macOS 위임), crates/secret/Cargo.toml와 Cargo.lock(이미 lock된 macOS 직접 의존성), 이 계획과 handoff. app/설정/storage migration은 수정하지 않는다.

### Task 1: query 및 오류 RED
- [x] 순수 `query(operation, id, keychain)`의 기존 UIFail 없는 stub와 아래 회귀를 작성한다. 테스트 keychain은 CFString sentinel로 실제 OS 호출을 하지 않는다.
```rust
for operation in [Operation::Read, Operation::Add, Operation::Update, Operation::Delete, Operation::Inventory] {
    let q = query(operation, Some("policy-test"), &sentinel);
    assert_eq!(q.find(auth_key), Some(auth_fail));
}
assert!(missing_or_error(-25300).unwrap());
assert!(missing_or_error(-25308).is_err());
```
- [x] `cargo test -p secret --locked macos::tests -- --test-threads=1`로 assertion RED를 확인한다. 잠금 파일은 기존 버전만 직접 의존성 연결한다.

### Task 2: 최소 구현과 GREEN
- [x] query 공통 구성에 `kSecUseAuthenticationUI = kSecUseAuthenticationUIFail`을 추가한다. Add는 kSecUseKeychain, 다른 작업은 kSecMatchSearchList=[User keychain]을 사용한다. Read는 returnData, Inventory는 returnAttributes+matchAll을 추가한다.
```rust
pairs.push((cf(kSecUseAuthenticationUI), cf(kSecUseAuthenticationUIFail).into_CFType()));
```
- [x] SecItemAdd duplicate(-25299)만 scoped SecItemUpdate로 이어간다. get/has/delete/inventory는 -25300만 absence로 인정하고 -25308/기타 오류는 전달한다. 원시 결과 CFType은 create rule로 소유하고 예상 CFData/Array/Dictionary 타입을 검사한다. 임시 plaintext Vec 대신 CFData에서 UTF-8 검증 후 SecretString으로 직접 복사한다.
- [x] lib.rs 기존 전역 직렬화 안에서 macOS production만 어댑터에 위임하고 test mock/타 플랫폼은 보존한다. `cargo test -p secret --locked` GREEN을 확인한다.

### Task 3: 최종 검증·리뷰·게시
- [x] CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-deps-target-20260907로 secret 전체 test, `cargo clippy -p secret --locked --all-targets -- -D warnings`, `cargo run -p xtask --locked -- check-boundary`, `cargo fmt --all --check`, `git diff --check`를 실행한다. 실제 keychain/GUI build/launch는 하지 않는다.
- [x] 실제 source diff로 `codex review --uncommitted`를 300초 유계 실행한다. 결론이 없으면 정확 PID 종료 후 좁은 readonly CLI 리뷰로 결론을 확보한다. 확정 finding은 RED→GREEN 및 재검증한다.
- [ ] handoff와 Obsidian 일지에 결과 기록, 한국어 commit/push, `gh pr create --base main --head fix/keychain-noninteractive-main --body-file /private/tmp/deppy-keychain-noninteractive-pr.md`로 Ready PR 생성. exact HEAD/check annotations를 확인해 Actions 미실행은 BLOCKED로 기록한다.

공식 근거: https://developer.apple.com/documentation/security/ksecmatchsearchlist , https://developer.apple.com/documentation/security/ksecusekeychain , https://developer.apple.com/documentation/security/ksecuseauthenticationuifail
