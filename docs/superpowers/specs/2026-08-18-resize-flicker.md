# 창 리사이즈 드래그 중 깜빡임 — 원인과 수정 (2026-08-18)

## 증상

앱 창(전체 화면) 크기를 마우스로 드래그해서 바꿀 때 화면이 깜빡인다(사용자 보고).

## 확정한 원인 — "빈 프레임"이 아니라 "드래그 내내 실제 PTY reflow가 반복"

렌더 경로(`renderer_egui.rs::draw`)와 배경 페인트(`workspace.rs::render_pane`)를 코드로
추적한 결과, **미페인트 영역이 노출되는 경우는 없었다**:

- pane 전체(`pane_layout.surface`)가 매 프레임 `TERMINAL_SURFACE_BG`로 먼저 칠해진다
  (`workspace.rs:4639` 부근, 2026-08-07에 이미 이 이유로 추가된 코드).
- `renderer_egui::draw`의 배경 rect도 이번 프레임의 `avail.y`를 그대로 쓰고(`:266`),
  `avail.x`로 클램프된다(`:271-277`) — 옛 스냅샷보다 가용 폭이 넓어도 그 자리는 같은
  터미널 배경색으로 덮인다. 즉 "다른 색 seam이 한 프레임 노출"되는 경로는 없다.

진짜 원인은 리사이즈 발화 빈도에 있다.

- `render_pane`은 **매 프레임** `ui.available_size()`로 cols/rows를 다시 계산해
  `queue_terminal_resize`를 부른다(`workspace.rs:4762-4766`, 수정 전).
- macOS 라이브 창 리사이즈는 `third_party/winit-0.30.13`의 `drawRect:` →
  `handle_redraw`(`view.rs:202-208`) 경로로 **드래그 중 계속 동기적으로 다시 그린다** —
  즉 드래그 내내 `avail`이 프레임마다 바뀌고, cols/rows도 프레임마다 바뀐다.
- `queue_terminal_resize`는 직전에 **보낸** 크기와만 비교해 중복을 거르므로
  (`sent_sizes`), 드래그 중 매 프레임 다른 크기가 그대로 `RuntimeCommand::Resize`로
  나간다.
- `runtime::in_process`가 이를 받아 `session::resize()`를 부르면 **항상**
  `mark_full_dirty()`가 걸린다(`session.rs:412-421` — 크기가 같은지 확인하는 가드가
  없다). 이건 alacritty grid의 **진짜 reflow**이고, PTY resize는 자식 프로세스에
  SIGWINCH를 보낸다 — 쉘/TUI가 그 신호를 받아 화면을 다시 그린다.

즉 드래그 한 번에 자식 프로세스 화면이 수십 번 다시 그려지고, 그 중간 상태들이
차례로 화면에 반영된다 — 이것이 사용자가 "깜빡임"으로 보는 현상이다. 이건 렌더러
문제가 아니라 **리사이즈 발화 빈도** 문제다.

## 수정

`crates/app/src/ui/workspace.rs`에 디바운스 래퍼 `queue_terminal_resize_debounced`를
추가하고, `render_pane`의 호출부(구 `self.queue_terminal_resize(session, cols, rows)`)를
이걸로 교체했다. `queue_terminal_resize` 자체와 `sent_sizes` 시맨틱은 손대지 않았다
(기존 큐 압력 재시도 테스트가 이 저수준 함수의 "즉시 전송" 계약에 의존하고 있었다).

동작:

1. 세션에 대한 **첫 mismatch**(세션 생성, split 등 1회성 변경)는 지연 없이 즉시
   보낸다 — split/새 세션 흐름에 지연을 추가하지 않는다.
2. 그 뒤로 **목표가 프레임마다 계속 바뀌면**(=드래그 진행 중) 전송을 미루고
   `pending_resize_target`에 목표만 갱신한다.
3. 같은 목표가 `RESIZE_DRAG_DEBOUNCE`(120ms) 동안 안정되면 그때 한 번 더 보낸다.
4. 드래그가 끝나 더 이상 새 프레임이 없어도 최종 크기가 유실되지 않도록, 목표를
   갱신할 때마다 `ctx.request_repaint_after(...)`로 debounce 만료 시점에 다시
   확인하러 오는 repaint를 예약한다.

결과적으로 드래그 한 번당 실제 PTY resize(=reflow+SIGWINCH redraw)는 "드래그 시작
시 1회 + 드래그 종료 후 1회"로 줄어든다(드래그 도중 수십 회 → 2회).

## 테스트

`crates/app/src/ui/workspace.rs` 테스트 모듈에 추가:

- `리사이즈_드래그_중_중간_크기는_보내지_않고_안정된_최종크기만_한번_보낸다` — 드래그를
  흉내내 중간 크기가 전송되지 않음과, debounce 후 최종 크기가 정확히 한 번 전달됨을
  검증한다.
- `세션_생성같은_단일_리사이즈는_지연_없이_즉시_전송된다` — 드래그가 아닌 1회성 변경은
  지연이 없음을 검증한다(회귀 방지 — 모든 resize에 디바운스를 강제하지 않는다).

## 미검증 (실제 화면으로 확인 못함)

- 실측 육안 검증(드래그 중 실제로 깜빡임이 사라지는지)은 하지 못했다 — 오케스트레이터의
  최종 빌드·실행으로 확인 필요.
- `RESIZE_DRAG_DEBOUNCE = 120ms`가 체감상 적절한 값인지는 육안 확인이 필요하다. 너무
  짧으면 드래그 중 여전히 여러 번 reflow될 수 있고(빠른 마우스 이동 프레임 간격이
  120ms보다 촘촘하면 문제 없음), 너무 길면 드래그 종료 후 터미널이 최종 크기로
  맞춰지는 데 걸리는 시간이 체감될 수 있다.
