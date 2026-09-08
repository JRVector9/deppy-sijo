# Scrollback Policy Contract Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** 숫자 직접 입력을 보존하면서 scrollback 범위와 Auto/Manual 캐시 예산을 일관되게 적용한다.

**Architecture:** terminal의 독립 policy 모듈이 순수 범위·예산 계약을 소유하고 app과 runtime이 가져온다. OS RAM 조회와 설정 마이그레이션은 app이 소유한다. 기존 wire 0 허용·프로토콜과 backend/live apply는 변경하지 않는다.

**Tech Stack:** Rust 1.96.1, serde/TOML, egui 0.36, egui_kittest.

---

### Task 1: 기존 숫자 직접 입력 변경 분리
**Files:** Modify `crates/app/src/ui/settings.rs`.
- [x] `git show 2fa3052 -- crates/app/src/ui/settings.rs`, `git show 7584847 -- crates/app/src/ui/settings.rs`의 patch만 순서대로 적용한다. session/Markdown 파일은 적용하지 않는다.
- [x] 기존 kittest 숫자 입력·포커스·소수 정밀도 테스트를 `CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo ui::settings::tests --locked -- --test-threads=1`로 실행한다. 기대: PASS.

### Task 2: 범위·예산 계약 RED/GREEN
**Files:** Create `crates/terminal/src/policy.rs`; Modify `crates/terminal/src/lib.rs`, `crates/runtime/src/command.rs`, `crates/runtime/src/remote.rs`; Test policy 모듈.
- [x] `auto_cache_budget_mib(None)==128`, `Some(32 GiB)==256`, `Some(u64::MAX)==512` 테스트를 먼저 작성한다.
- [x] `CARGO_BUILD_JOBS=2 cargo test -p terminal policy --locked`로 미구현 RED를 확인한다.
- [x] `SCROLLBACK_SETTING_MIN=100`, `SCROLLBACK_LINES_MAX=100_000`, `SCROLLBACK_DEFAULT=10_000`; Manual 32..2048 MiB, Auto RAM/128 clamp128..512 MiB를 구현한다. `CacheBudgetMode`는 serde snake_case enum이다.
- [x] runtime/remote의 100_000 중복 상수만 공통 import로 바꾸고 spawn 0 허용을 유지한다.
- [x] 같은 focused 명령으로 GREEN을 확인한다.

### Task 3: 설정 마이그레이션 RED/GREEN
**Files:** Modify `crates/app/src/config.rs`, `crates/app/src/app.rs`; Test config 모듈.
- [x] 신규 기본 Auto, 기존 `[terminal] cache_budget_mb=320`은 Manual, 명시 Auto round-trip, 99→100·100/999 보존 테스트를 추가한다.
- [x] `CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo config::tests --locked -- --test-threads=1`로 RED를 확인한다.
- [x] TerminalConfig에 모드를 추가하고 누락 필드는 Manual로 역호환한다. 저장 수동값은 모드 변경 시 보존한다. RAM 실제 조회를 Option으로 분리하여 예산 조회 실패는128 MiB, 기존 warm 권장값은16 GiB fallback을 유지한다.
- [x] `effective_cache_budget_mib()`를 기존 `terminal_cache_policy_command`에서 소비한다. live scrollback 명령은 추가하지 않는다.
- [x] 같은 focused 명령으로 GREEN을 확인한다.

### Task 4: UI·5개 locale
**Files:** Modify `crates/app/src/ui/settings.rs`, `crates/i18n/locales/{en-US,ko-KR,ja-JP,zh-Hans,zh-Hant}/messages.txt`.
- [x] 실제 terminal settings 페이지에서 100 직접 입력, Manual 예산 입력, Auto 전환 후 수동값 보존 테스트를 추가하여 RED를 확인한다.
- [x] scrollback은 공통 100..100_000 범위와 기존 +/-를 사용한다. Auto/Manual 선택, 자동 계산값, 수동 숫자 입력 비활성을 구현한다.
- [x] 안내는 이번 PR의 실제 동작인 새 세션 적용, 감소된 기록 복원 불가, 메모리/숨김 제한, 앱 전체 RAM 상한 아님을 5개 locale에 일치시킨다.
- [x] `CARGO_BUILD_JOBS=2 cargo test -p i18n --locked`와 settings focused 테스트 GREEN을 확인한다. UI 앱 재빌드·실행은 하지 않고 실화면 검증을 대기로 기록한다.

### Task 5: 리뷰·게이트·게시
**Files:** Modify `docs/CODEX_HANDOFF.md`; Create PR.
- [x] `codex review`의 소스 전용 프롬프트로 소스 diff를 읽기 전용 리뷰한다. 지적을 수정하고 해당 테스트를 재실행한다.
- [x] `cargo fmt --all --check`, `git diff --check`, `CARGO_BUILD_JOBS=2 cargo clippy -p terminal -p runtime -p deppy-sijo --all-targets --locked -- -D warnings`를 실행한다. 결과를 실제 실행 여부와 함께 기록한다.
- [x] 한국어 Conventional Commit으로 커밋하고 `git push -u origin feat/scrollback-policy-contract`, `gh pr create --base main --head feat/scrollback-policy-contract --body-file ...`로 PR을 생성한다. rebase/force-push는 하지 않는다.


### 추가 회귀: 소수 직접 입력의 저장 신호
- [x] 1.0→1.0000001 직접 입력은 값이 바뀌지만 epsilon 비교 때문에 저장 신호가 false인 RED를 확인했다.
- [x] 유한 입력값의 정확한 변경 여부(`*value != next`)로 바꿔 표시값과 저장값의 계약을 맞췄다.
- [x] settings 전체 focused20 GREEN과 strict clippy를 재확인했다.

### 리뷰 후속: 플랫폼별 자동 RAM 조회
- [x] Linux 페이지 계산의 정상 입력 실패 RED를 확인하고 음수/0/overflow를 None으로 보존하는 순수 변환을 구현했다.
- [x] Linux sysconf와 Windows GlobalMemoryStatusEx를 기존 의존성으로 연결하고 config focused40 GREEN을 확인했다.
- [x] wire1..99 거부 지적은 합의된 기존0..100000 wire 호환을 깨므로 미수용 사유를 handoff에 기록했다.
- [x] 후속 코드 리뷰 결과(잔여 확정 결함 없음)와 마지막 gate를 기록했다. Windows/Linux 네이티브 실행과 실화면 검증은 미실행으로 유지한다.

## 완료 기록
- 구현 커밋: `862a894`.
- PR: https://github.com/JRVector9/deppy-sijo/pull/157 (base main).
- focused89 tests 통과: settings20 + config40 + policy1 + i18n8 + runtime18 + wire1 + postcard1.
- 최종 strict clippy(all-targets), fmt/diff 통과; codex 후속 리뷰 잔여 확정 결함 없음.
- 앱 재빌드/재실행·실화면/IME 검증과 Windows/Linux 네이티브 실행/cross-target 검증은 미실행 상태로 유지한다.
