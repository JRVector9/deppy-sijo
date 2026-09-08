# 검증된 터미널 너비 맞춤 main 이관 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** 사용자가 화면을 확인한 PR #149 제품 commit 16e642a의 논리 폭 보존·균일 축소를 main 45e66cc에 독립 이관한다.

**Architecture:** 이미 보낸 Resize의 cols, 없으면 snapshot.cols를 유지한다. pane이 좁으면 그리드·선택·커서·IME를 같은 비율로 축소하며 scale<=1을 유지한다. 넓은 pane의 여백 동작을 바꾸거나 폰트를 확대하지 않는다. main #150의 viewport 안정 디바운스·resize fence와 #154의 egui 0.36/IME 변경을 그대로 보존한다.

**Tech Stack:** Rust, egui 0.36, 기존 TerminalRenderCache/WorkspaceUi, runtime Resize 입장 검사.

---

## 계약과 파일 경계

- `crates/terminal/src/renderer_egui.rs`: fit_width_scale, 논리→화면 변환, run당 배율 갤리 하나, 실제 선택/커서/IME/shape 회귀.
- `crates/app/src/ui/workspace.rs`: 기존 cols 보존, 첫 snapshot 대기, 축소된 셀 높이로 rows 계산, 스크롤 좌표.
- `crates/runtime/src/command.rs`, `crates/runtime/src/lib.rs`: 기존 65,536셀 상수 공개·재수출만 이관한다.
- 이 계획과 `docs/CODEX_HANDOFF.md`: 이 lane 결과를 추가한다. 원본 #149의 handoff/plan/spec와 Relay/옛 resize history를 가져오지 않는다.
- GUI 빌드·실행, 원본 #149 변경/닫기, force push/rebase, backend/storage/wire 변경은 하지 않는다. 이 main 조합의 시각 검증은 대기 상태로 기록한다.
- 부모 확인: #149 제품 화면은 사용자가 이미 확인했다. 확대를 허용하는 새 해석은 제외하며 scale<=1을 유지한다. 가용 폭의 cols/geometry 작업은 별도 체인에서 다룬다.

### Task 1: 정확한 제품 hunk 이관

- [x] **Step 1: 원본 제품 patch를 생성하고 main에 적용 가능한지 확인한다.**

```sh
git diff 16e642a^ 16e642a -- crates/app/src/ui/workspace.rs crates/terminal/src/renderer_egui.rs crates/runtime/src/command.rs crates/runtime/src/lib.rs > /private/tmp/deppy-preserve-layout-product.patch
git apply --check /private/tmp/deppy-preserve-layout-product.patch
```

- [x] **Step 2: 제품 hunk만 적용한다.** 적용 문맥이 달라졌다면 충돌 hunk만 현재 main의 동일 함수에 옮긴다. #150의 4항 pending_resize_target, viewport 안정 검사, arm_or_retarget_resize_presentation, frame_has_active_preedit와 #154의 IMEOutput 필드는 현재 main을 유지한다.

```sh
git apply /private/tmp/deppy-preserve-layout-product.patch
git diff --stat
git diff --check
```

- [x] **Step 3: 이관된 기존 회귀를 확인한다.** 이식 자체는 이미 RED/GREEN을 거친 코드 재사용이다. 새 호환성 결함을 찾으면 구현 수정 전에 회귀를 추가해 실패를 확인한다.

```sh
CARGO_BUILD_JOBS=2 cargo test -p terminal --lib --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo --locked ui::workspace::tests -- --test-threads=1
```

기대: 축소 last-column·selection·IME·인접 shape·캐시 재사용, snapshot 이전 resize 금지, sent cols 우선, runtime 셀 상한, main debounce/fence/한글 입력 테스트 PASS.

### Task 2: main resize 계약과 split 배선 검증

- [x] **Step 1: 단일/분할 pane에서 실제 staged Resize와 반환 셀 좌표를 검사한다.** 기존 `기존_출력_너비는_pane을_좁히거나_넓혀도_다시_줄바꿈하지_않는다`와 terminal draw_in_pane 검사를 이용한다. 원본 텍스트에 명시적 줄바꿈/공백이 남고 snapshot Arc가 바뀌지 않아야 한다. 각 pane의 fit 계산은 그 pane ui.available_size에만 의존해야 한다.

```rust
assert_eq!(target.cols, sent_cols.unwrap_or(80));
assert!(Arc::ptr_eq(workspace.sessions[&session].snapshot.as_ref().unwrap(), &original));
assert!((drawn.cell.x * cols as f32 - grid_width_for_available(pane_width)).abs() < 0.05);
```

- [x] **Step 2: main #150 계약 회귀를 함께 실행한다.** 원본 제품 patch가 debounce/fence 함수를 수정하지 않았는지 diff로 확인한다. 동일 격자 안 viewport 움직임, 첫 Resize 즉시 전송, 후속 변경 120ms 안정, deadline 연장 금지와 실패 rollback은 현재 테스트 그대로 유지한다.

```sh
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo --locked 리사이즈 -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p runtime --lib --locked command::tests -- --test-threads=1
```

### Task 3: 리뷰·게이트·PR

- [x] **Step 1: Codex CLI로 source diff만 읽기 전용 리뷰한다.** 테스트/빌드/수정/서브에이전트를 금지하고 4개 제품 파일의 resize/shape/cache/IME 경계를 검토한다. 추가 탐색이 길어지면 정확한 PID를 확인하고 중지를 요청해 실제 결론 유무를 기록한다. 결함을 발견하면 RED→최소 수정→focused GREEN 순서로 처리한다.
- [x] **Step 2: 최종 적정 게이트를 직렬 실행한다.** CARGO_BUILD_JOBS=2, 전용 캐시는 이전 PR A가 사용을 마친 `/private/tmp/deppy-ureq3-20260907/target`을 재사용한다. 다른 lane과 동시 사용하지 않는다.

```sh
cargo fmt --all -- --check
git diff --check
CARGO_BUILD_JOBS=2 cargo clippy -p terminal -p runtime -p deppy-sijo --all-targets --locked -- -D warnings
CARGO_BUILD_JOBS=2 cargo run --locked -p xtask -- check-boundary
```

- [x] **Step 3: 실제 테스트 결과·리뷰·수정·시각 검증 대기를 handoff와 한국어 프로젝트 일지에 기록한다.** 기존 lane 기록은 보존한다.
- [x] **Step 4: 한국어 커밋, push, main 대상 새 PR을 만든다.** 원본 #149는 그대로 둔다. 새 PR 본문은 승인된 scale<=1 제품 동작의 main 이관, 실제 로컬 게이트, 이 main 조합의 시각 검증 대기를 명시한다.

```sh
git add crates/app/src/ui/workspace.rs crates/terminal/src/renderer_egui.rs crates/runtime/src/command.rs crates/runtime/src/lib.rs docs/CODEX_HANDOFF.md docs/superpowers/plans/2026-09-08-preserve-terminal-layout-main.md
git commit -m 'fix(ui): 검증된 터미널 너비 맞춤을 main에 이관한다'
git push -u origin fix/preserve-terminal-layout-main
gh pr create --base main --head fix/preserve-terminal-layout-main --title 'fix(ui): 검증된 터미널 너비 맞춤 main 이관' --body-file /private/tmp/deppy-preserve-layout-pr-body.md
```

## 완료 결과

계획 75a2759, 제품 c651438, main PR #159. terminal 90/workspace 236/runtime command 18 PASS, terminal 4 ignored. strict Clippy/fmt/diff/boundary PASS. Codex 정적 리뷰 확정 결함 없음. 이 main 조합의 시각 검증은 대기이며 GUI 빌드·실행, 기존 #149 변경/닫기, PR merge는 수행하지 않았다.
