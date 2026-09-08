# Resize Applied Generation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** 실제 backend/PTY 적용 세대와 viewport를 연결해 queue acceptance 및 stale resize가 화면을 역행시키지 않게 한다.

**Architecture:** 기존 debounce와 presentation fence에 실제 적용 조건을 추가한다. append-only v14 token/stamp를 worker→plain/delta→UI로 전달하며 legacy API와 크기 계산은 유지한다.

**Tech Stack:** Rust, egui, vendored Alacritty, Ghostty optional backend, postcard, bounded std channels.

---

실행은 승인된 설계에 따라 현재 에이전트가 inline으로 진행한다. E base HEAD=9af1b80 (1dfef2a에서 일반 merge), PR base=feat/scrollback-live-policy. 모든 Cargo 명령은 `CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-deps-target-20260907` 환경이다. 앱 실행은 금지한다.

### Task 1: 현재 admission 버그 RED

**Files:** Modify/Test `crates/app/src/ui/workspace.rs`의 SessionView/resize tests.

- [x] 안정 snapshot으로 시작하고 실제 queue completion만 성공시킨 회귀를 작성한다.
```rust
// complete_protocol(Ok(()))만 받은 시점은 Applied가 아니다.
assert!(view.resize_presentation.as_ref().is_some());
assert!(view.settle_resize_presentation(now + Duration::from_millis(300)).is_none()); // 적용 전 polling 없이 stable 유지
assert_eq!(view.snapshot_gen, stable_generation);
```
- [x] `cargo test -p deppy-sijo --bin deppy-sijo --locked resize_admission -- --test-threads=1`로 기존 fence가 만료되는 RED를 기록한다.
- [x] queue admission과 applied 시간을 분리할 최소 필드/호출을 준비하고 다음 Task의 wire 증거와 연결한 뒤 GREEN한다.

### Task 2: checked resize 및 최소 wire 타입

**Files:** Create `crates/runtime/src/resize.rs`; Modify `crates/terminal/src/backend.rs`, `alacritty_backend.rs`, `ghostty_backend.rs`, `crates/session/src/session.rs`, `crates/session/src/lib.rs`, runtime command/event/lib/protocol.

- [x] 실제 dimensions/오류 및 토큰 충돌 회귀를 먼저 추가하고 stub가 실패함을 확인한다.
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResizeToken { pub owner: [u8; 16], pub generation: u64, pub owner_epoch: u64 }
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResizeStamp { pub epoch: u64, pub owner_epoch: u64, pub token: Option<ResizeToken>, pub cols: u16, pub rows: u16 }
```
- [x] `TerminalBackend::grid_dimensions(&self) -> anyhow::Result<(u16,u16)>`를 실제 backend 조회로 구현한다. Session checked resize는 backend/PTY 실패를 typed error로 반환하고 dirty/cache 재적용은 보존한다.
- [x] 기존 enum 마지막 뒤에 ResizeTracked, ResizeApplied/ResizeFailed/ViewportTracked를 추가하고 v14로 올린다. 기존 bytes 고정 회귀를 쓴다.
- [x] `cargo test -p terminal -p session --locked --lib -- --test-threads=2`; `cargo test -p runtime --locked protocol -- --test-threads=2`로 GREEN한다.

### Task 3: worker 적용 및 viewport slots

**Files:** Modify `crates/runtime/src/in_process.rs`, `event.rs`, `client.rs`, `resize.rs`.

- [x] 같은 token 재전송/다른 크기 충돌/old generation/실제 적용 후 ACK/교체 epoch 회귀를 작성하고 RED를 확인한다.
```rust
// 동일 token 재시도는 같은 epoch이며 실제 resize 호출은 한 번이다.
assert_eq!(first_stamp, retry_stamp);
assert!(replacement_stamp.epoch > first_stamp.epoch);
```
- [x] checked worker epoch와 세션별 마지막 요청/결과 map을 추가한다. 적용 성공 뒤에만 ResizeApplied를 emit하고, 첫 tracked 이후 모든 viewport emit에서 stamp를 붙인다. 실패/legacy/교체는 token을 무효화한다.
- [x] 공통 accessor로 ViewportTracked도 기존 최신값 slot을 이용하고 stamp 변경 시 full dirty를 유지한다.
- [x] `cargo test -p runtime --locked tracked_resize -- --test-threads=2`; event/client focused 후 runtime 전체를 실행한다.

### Task 4: plain/delta 호환 전달

**Files:** Modify `crates/runtime/src/remote.rs`, `protocol.rs`, `crates/web-remote/src/dashboard.rs`, `lib.rs`.

- [x] plain/keyframe/delta stamp 왕복, stamp 전환 keyframe, mismatch resync, 구버전 handshake 거부 회귀를 작성한다. 이 단계는 구현 연결 후 회귀를 추가해 GREEN부터 확인했으며 선행 RED를 주장하지 않는다.
```rust
assert_eq!(decoded.viewport_stamp(), Some(stamp));
assert!(matches!(frame, WireMsg::ViewportKeyframeTracked { .. }));
```
- [x] append-only tracked keyframe/delta와 baseline stamp를 추가한다. stamp/실제 shape가 다른 delta는 publish하지 않고 기존 bounded resync를 따른다.
- [x] web-remote event 소비 두 곳에서 legacy/tracked viewport 공통 accessor를 사용한다.
- [x] `cargo test -p runtime --locked remote -- --test-threads=2`; `cargo test -p web-remote --locked --lib -- --test-threads=2`를 실행한다.

### Task 5: 기존 UI delivery/fence에 세대 적용

**Files:** Modify `crates/app/src/ui/workspace.rs`, `crates/app/src/app.rs`.

- [x] A→B→A, Applied 뒤 late B, ACK/viewport 순서, queue coalescing, runtime/session 교체, 실패/hidden/exit/재시도 상한 RED를 작성한다.
```rust
assert!(!view.accept_stamp(old_b));
assert!(view.accept_stamp(current_a));
// queue 수락으로 시작하지 않고 실제 적용 증거가 deadline을 시작한다.
assert!(view.applied_at.is_none());
```
- [x] UI 수명 nonce+checked generation, 세션별 최신 한 요청을 기존 sent_sizes/rollback/retry와 연결한다. sizing/discard pass와 120ms debounce는 유지한다.
- [x] Applied ACK 또는 동일 stamp viewport만 fence의 적용 시각을 시작하고 current token/epoch/실제 shape를 통과한 viewport만 후보/IME 입력 상태에 반영한다. 승격 뒤 watermark를 유지한다.
- [x] 2초 ACK 재확인 최대2회, 기존 Busy 최대6회를 사용한다. hidden/exit/stream 교체는 상태/후보/타이머를 지운다. app replay와 event_session을 새 variant에 맞춘다.
- [x] `cargo test -p deppy-sijo --bin deppy-sijo --locked resize -- --test-threads=1`; split/debounce 관련 기존 회귀와 app 전체 관련 검사를 실행한다.

### Task 6: 리뷰와 게시

**Files:** Update `docs/CODEX_HANDOFF.md`, 위 spec/plan, Obsidian 프로젝트 일지/deppy-sijo.

- [x] 관련 전체 terminal/session/runtime/web-remote/app 테스트와 `cargo clippy -p terminal -p session -p runtime -p web-remote -p deppy-sijo --locked --all-targets -- -D warnings`, `cargo fmt --all --check`, `cargo run --locked -p xtask -- check-boundary`, `git diff --check`를 실행한다.
- [x] `codex exec --sandbox read-only`로 코드 diff만 읽는 5분 유계 리뷰를 수행한다. 테스트/빌드/편집 금지 프롬프트를 사용하고 중단 시 정확한 PID만 종료한다. 확정 finding은 RED→GREEN으로 고친다.
- [x] 실제 결과/실패 접근/남은 플랫폼 제한을 handoff와 일지에 쓴다. 한국어 커밋 후 일반 push, `gh pr create --base feat/scrollback-live-policy --head fix/resize-applied-generation --body-file <준비한 본문>`으로 stacked PR을 만든다. rebase/force-push/앱 실행은 하지 않는다.

## 실행 중 확정한 계약 보완

- 다른 owner는 정확히 현재 owner_epoch+1에서만 CAS 전환한다. 같은 owner/epoch는 generation으로 비교하고, 이전 owner epoch는 실제 backend 적용 전에 거부한다. O(1) 상태이며 100회 이상 정상 소유자 교체를 허용한다. 랜덤 nonce 대소 비교 및16개 retired owner 제한은 폐기했다.
- Backend/PTY 일부 실패는 같은 token으로 다시 실제 적용한다. 성공한 token만 멱등 ACK를 재발행한다. UI 자동 재확인은2초 간격 최대2회이고 실패/소진 뒤 안정 화면을 남긴 채 fence/요청을 종료한다.
- 큐 coalesce는 payload token과rollback token을 함께 갱신한다. 큐완료에서는수락만기록하고Applied와동일stamp viewport를받은뒤최종표식을해제한다. 선택해제는실제화면승격의기존공통경로를따른다.
- 구형 v12/v13 peer는 exact-version handshake에서 명확히 거부한다. legacy Resize 자체의 byte/API는 유지하지만 실제 적용 보장으로 조용히 fallback하지 않는다.

## 게시 결과

PR #167: https://github.com/JRVector9/deppy-sijo/pull/167 (base `feat/scrollback-live-policy`, Ready). 최종 source commit `5f31d5e`. 원격 Actions는 각 annotation에서 계정 결제/지출 한도로 job 미시작을 확인했으며 GitGuardian은 SUCCESS였다. 선택 Ghostty는 Zig 실행 파일 부재로 check 실패, GUI 실행/시각 검증은 미수행이다. 전체 source Codex 리뷰 P2 수정 및 좁은 재리뷰·독립 재리뷰를 완료했다.
