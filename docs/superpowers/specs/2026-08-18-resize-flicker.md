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

## 2026-10-11 변경 — 드래그 중에도 격자가 따라온다

사용자 요청: "사이즈 조절할때 바로 눈으로 볼 수 있게 반영해." 위 디바운스(와 2026-09-06에
더한 viewport 정지 조건)는 드래그가 멈춘 뒤에야 격자를 바꿔, 드래그 내내 옛 격자가
축소·여백 상태로 남았다.

- `queue_terminal_resize_debounced` → `queue_terminal_resize_live`. 목표가 바뀌면 바로
  보내되, 직전 단계가 PTY에 적용되고 fence가 그 크기의 안정 화면을 표시할 때까지는 겹쳐
  보내지 않는다(`SessionView::live_resize_step_pending`). 중간 크기는 쌓지 않고 단계가
  끝난 뒤 그 순간의 최신 목표 하나만 보낸다.
- `settle_resize_presentation`은 목표가 이미 다음 크기로 옮겨 갔어도 적용된 단계의 안정
  화면을 승격한다. 이전에는 `resize_desired == target`일 때만 승격해 드래그 중에는 어떤
  단계도 표시되지 않았다.
- `RESIZE_DRAG_DEBOUNCE`와 `pending_resize_target`은 제거했다. 대기 중 repaint는 적용
  이벤트와 fence의 one-shot repaint가 만든다.
- 깜빡임 대책은 디바운스 대신 2026-08-24 fence가 맡는다 — TUI의 clear→redraw 중간
  viewport는 표시하지 않고, 단계마다 32ms 조용해진 target 화면(최대 250ms)만 승격한다.
- pane divider 드래그(2026-08-24 계약 A: drag 중 grid 유지, release 후 1회)는 바꾸지
  않았다. `stage_terminal_resize_for_pass`가 split drag 중에는 여전히 staging하지 않는다.
- 남은 비용: 드래그 동안 자식 프로세스가 단계마다 SIGWINCH로 다시 그린다. 실제 앱에서
  Claude/Codex 세션을 띄운 채 창을 드래그해 육안으로 확인해야 한다.

## 2026-10-11 보완 — 단계 화면 수락, 축소 출렁임 제거, 단계당 비용

0.10.1 배포 뒤 점검에서 위 변경이 실제 화면에서는 드래그 중 거의 반영되지 않았을 결함을 찾았다.

- **원인(결함):** `accepts_resize_viewport` → `observe_resize_applied`가 `resize_desired == target`일
  때만 true를 돌려줘, 드래그로 목표가 옮겨 간 뒤 도착한 단계 화면을 이벤트 처리에서 버렸다
  (`continue`). fence는 후보 없이 250ms hard deadline에 아무것도 승격하지 못하고, 화면은 드래그가
  끝날 때까지 그대로였다. 단위 테스트가 `buffer_resize_snapshot`을 직접 불러 이 경로를 건너뛰었다.
  **수정:** 토큰·목표가 맞는 적용 단계는 항상 수락한다. `handle_events` 수준 회귀 테스트
  `창_드래그_중_목표가_옮겨_가도_적용된_단계의_화면을_받아_표시한다`(수정 전 FAIL 확인).
- **단계 사이 렌더:** 옛 grid를 `fit_width_scale`로 축소하던 것이 단계마다 배율이 바뀌어 글자가
  출렁였고, 축소 중에는 매 프레임 갤리를 복제·변환했다. `retained_grid_awaits_local_resize`가
  참인 동안(로컬 세션, 크기 불일치, split 드래그·원격 제어·적용 실패 아님) 축소 없이 원래 크기로
  좌상단에 고정하고 넘치는 행은 커서 쪽으로 민다(`draw_with_preedit_in_viewport`의
  `fit_wider_grid=false`). kitty·ghostty·rio·wezterm이 같은 방식이다. 그 밖의 경우(복원·원격·
  split)는 2026-09-08 「한 화면에 전부 보인다」 축소 맞춤을 그대로 쓴다.
- **단계당 비용:** tracked resize가 매번 `signal_memory_released()`(mi_collect 전체 수집)를
  불렀다. legacy Resize처럼 hidden/exited 세션일 때만 부른다.
- **후속(미적용):**
  - DEC 2026 동기화 출력: vte 0.15 `Processor`가 BSU~ESU를 버퍼링해 Codex(매 draw를 sync로 감쌈)의
    다시 그리기는 이미 원자적이다. 다만 150ms timeout을 아무도 확인하지 않아 ESU 없는 BSU는 2MiB가
    쌓일 때까지 화면을 멈춘다. 런타임에 sync deadline 깨우기와 `stop_sync`가 필요하고, 그 뒤
    Claude Code에 `CLAUDE_CODE_FORCE_SYNC_OUTPUT=1`을 검토한다.
  - `push_watched_viewports`는 한 pane의 단계마다 보이는 모든 pane을 다시 스냅샷한다. 다중 pane
    드래그 비용을 줄이려면 resize 경로만 그 세션으로 좁히는 변경과 2-pane 런타임 테스트가 필요하다.

### 2026-10-11 리뷰 반영

- 한 단계 안에서 축소 맞춤과 잘라 그리기가 섞이던 두 경우를 막았다. (1) 분할 경계 release 뒤
  ack로 `split_drag`가 비고 최종 크기가 나가기·적용되기 전까지 잘라 그렸다 → 판정에서
  `resize_presentation_settlement_deferred`와 `SessionView::live_resize_clip`(분할 최종 크기
  전송 시 끔)을 본다. (2) 첫 크기(복원·탭 재표시)는 보내기 전 한 프레임 축소, 이후 잘라 그리기였다
  → 첫 크기 전송은 `live_resize_clip`을 끄고, 보내기 전 프레임은 「보낸 크기 ≠ 목표」로만 판정한다.
- 드래그가 이미 다음 크기로 넘어간 단계는 hard deadline에도 `candidate_allowed`가 아닌(빈)
  후보를 올리지 않는다. 마지막 크기의 빈 화면은 기존 계약대로 deadline에 올린다.
- 남은 비용(미적용): 적용된 단계마다 `save_terminal_size`가 파일을 쓴다. 런타임 루프에서
  크기가 잠시 안정된 뒤 한 번 쓰도록 모으는 변경이 다음 후보다(종료 시 flush 필요).
