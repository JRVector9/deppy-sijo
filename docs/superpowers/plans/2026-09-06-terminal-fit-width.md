# 터미널 너비 맞춤 Implementation Plan

**Goal:** 기존 줄과 표의 모양을 유지하면서 가로 스크롤 없이 pane 너비 안에 표시한다.

**Architecture:** WorkspaceUi는 이미 전송한 논리 열 수 또는 현재 snapshot의 열 수를 사용한다. renderer는 원본 셀 그리드에 균일 축소를 적용하며 호출측에 화면 좌표를 반환한다. Backend/storage/wire는 변경하지 않는다.

**Tech Stack:** Rust, egui 0.35, 기존 Alacritty backend.

## 1. 크기 요청 경로 재현

- [x] `crates/app/src/ui/workspace.rs`의 기존 egui harness로 80열 snapshot이 있는 pane을 좁게 렌더한다.
- [x] 실제 staged Resize의 cols가 80인지 확인한다. 수정 전 코드에서 pane 너비로 새 cols를 계산해 실패한 것을 확인했다.
- [x] 전송 중인 크기가 옛 snapshot으로 되돌아가지 않는 경우와 snapshot 없이 기존 sent_sizes만 있는 경우를 확인한다.
- [x] 첫 Viewport가 늦게 도착하는 복원 순서의 실제 RED를 확인하고, 폭을 추측하지 않도록 수정한다.

실행: `/tmp/deppy-sijo-agents-20260906/cargo-serial test -p deppy-sijo --bin deppy-sijo --locked 기존_출력_너비 -- --test-threads=1`.

## 2. 렌더러 너비 맞춤

- [x] `crates/terminal/src/renderer_egui.rs`에 `fit_width_scale(available_width, cell_width, cols)`를 추가한다. 유효한 가용 폭과 그리드 폭의 비율을 1 이하로 제한한다. 폭이 0이거나 비유한 입력이면 기존 clip 경로를 유지한다.
- [x] 실제 렌더 결과의 텍스트·선택·커서와 IME 후보창 좌표가 같은 변환을 사용하는지 검사한다. 가로/세로 배율은 동일해야 한다.
- [x] 반환하는 `RenderOutput.cell_size`는 원본 셀 크기×배율이고 origin은 화면 좌표다.

## 3. WorkspaceUi 배선

- [x] 세션별 `sent_sizes`의 cols, 없으면 표시 snapshot.cols, 둘 다 없으면 첫 실제 snapshot을 기다린다. 초기 gate가 후반에 열리면 repaint로 다음 프레임 예약을 보장한다. sizing/discard pass의 부수 효과 제약을 유지한다.
- [x] 위 열 수와 renderer의 동일한 배율로 표시 셀 높이를 구하고 `grid_rows_for_available`로 행 수를 계산한다.
- [x] 기존 stage/flush/debounce/fence 경로에 논리 크기를 전달한다. 마우스·검색 강조는 기존 RenderOutput의 화면 셀 좌표를 사용한다.

- [x] 축소 갤리를 run당 하나만 보관하고 실제 발행된 캐시 text shape를 구간 변환에서 제외한다. 같은 배율 Arc 재사용과 실제 paint Arc 동일성을 검사한다.
- [x] 휠/드래그 스크롤 환산을 표시 셀 높이에 맞춘다.
- [x] 리뷰 N1의 328×200 Resize 거부를 실제 RED로 확인하고 runtime 셀 상수를 공유해 행 목표를 제한한다. 1/80/328/360/500열 입장 검사까지 최종 app 검사에서 통과했다.

## 4. 최종 검증과 리뷰

- [x] `cargo-serial test -p terminal --lib --locked`: 87 PASS/4 ignored. 초기 workspace 그룹 232 PASS 뒤 복원 경로를 보강해 최종 `cargo-serial test -p deppy-sijo --bin deppy-sijo --locked -- --test-threads=1` 전체 app 검사 2109 PASS/14 ignored. 추가 셀 수 상한 보강 뒤 최종 소스를 고정한 전체 app 검사는 2110 PASS/14 ignored(50.22초).
- [x] `cargo fmt --all --check`, `cargo-serial clippy -p terminal -p runtime -p deppy-sijo --all-targets -- -D warnings`, `git diff --check`.
- [x] Opus/high 독립 리뷰에서 첫 화면·복원·분할·IME·포인터·크기 롤백을 확인하고 필요한 수정만 반영한다.
- [x] docs/CODEX_HANDOFF.md에 변경·실행 결과·실패·남은 시각 검증을 기록한다.

## 5. 별도 PR

- [x] `fix/preserve-terminal-layout`을 정상 push하고 PR #148의 `fix/window-resize-flicker`를 base로 draft PR #149를 생성했다. 구현 커밋 16e642a.
- [x] 앱은 재빌드/재실행하지 않았다. 시각 검증 대기를 PR 본문에 명시한다.
