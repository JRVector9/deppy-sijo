# 세션 점프 강조 구현 계획

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 사이드바에서 세션 행을 누르면 그 세션의 pane이 **잠깐 강조**되어 어디로 갔는지
눈에 보인다.

**작업 위치:** 워크트리 `/Users/jr/Desktop/projects/deppy-jump` (브랜치 `feat/session-jump-highlight`, `main` 기준).

---

## 배경 — 이미 있는 것과 없는 것

**포커스 이동은 이미 동작한다.** `SidebarAction::FocusSession` →
`WorkspaceControllerAction::FocusSession`(app.rs 약 15419)이 워크스페이스를 필요하면 전환하고,
`RuntimeCommand::FocusPane`을 보내고, `arm_terminal_focus` + `reveal_terminal_session`까지 한다.
`live_pane_target_is_current`는 탭이 활성인지까지 요구하지 않고, 런타임의 `FocusPane`이
`window.active_tab`도 함께 옮긴다.

**없는 것은 시각적 확인이다.** pane이 여러 개면 방금 어디로 갔는지 알 수 없다.

`crates/app/src/ui/workspace.rs:5118`에 이 문제의 과거 기록이 있다:

> 포커스 pane 파란 테두리는 사용자 요청으로 제거(2026-07-04) — 단일 pane 사용 시 **항상 보여
> 거슬림**. 다중 pane에서 포커스 식별이 다시 필요해지면 "pane 2개 이상일 때만 표시" 조건으로
> 복원할 것.

그래서 **항상 켜진 테두리는 만들지 않는다.** 클릭 직후 잠깐 나타났다 사라지는 강조여야
그때의 불만을 되살리지 않는다. 이 선택을 코드 주석에 남겨라.

---

## 설계 — [2026-08-18 정정] 새 기구를 만들지 않는다

**최초 계획은 새 강조 기구(`pane_highlight` + 세기 계산 + 렌더)를 만드는 것이었다. 그건
잘못된 조사였다.** 이 저장소에는 이미 `WorkspaceUi::session_flash`가 있고, 포커스 이동
(`FOCUS_FLASH`, 1.2초)과 상태 전이(`PANE_FLASH`)가 그걸 쓴다 — 만료 정리·렌더·repaint 예약이
전부 갖춰져 있다. 새 기구를 두면 여러 pane 사이를 점프하는 **흔한 경우에 같은 pane에 같은
길이의 테두리가 두 겹**으로 그려진다.

진짜 공백은 하나였다 — `FOCUS_FLASH`는 `mux.focused_pane`이 **바뀔 때만** 뜨므로, 이미 보고
있던 세션(특히 pane이 하나뿐인 워크스페이스)을 다시 눌러도 확인 신호가 없었다.

**그래서 `WorkspaceUi::flash_pane(&pane)` 하나를 더해 기존 기구에 얹는다.** 아래 원래 설계는
기록으로 남긴다.

### 원래 설계 (폐기)

- App이 `pane_highlight: Option<(runtime::MuxPaneId, std::time::Instant)>`를 소유한다.
  `WorkspaceControllerAction::FocusSession`이 **실제로 포커스에 성공한 경로**에서만 세운다
  (`else`로 빠지는 실패 경로에서는 세우지 않는다 — 아무 데도 안 갔는데 번쩍이면 거짓말이다).
- 지속 시간 상수 `PANE_HIGHLIGHT_DURATION`(권장 1.2초). 지나면 `None`으로 지운다.
- **leaf는 시간을 모른다.** App이 0.0~1.0 세기를 계산해 넘긴다 — 저장소 관례(leaf는 정책만).
  순수 함수로 뽑아 값으로 테스트한다:
  ```rust
  /// 남은 강조 세기 — 0.0이면 그리지 않는다. 끝으로 갈수록 부드럽게 사라진다.
  fn pane_highlight_strength(elapsed: std::time::Duration, duration: std::time::Duration) -> f32;
  ```
- `WorkspaceUi`에 `set_pane_highlight(Option<(MuxPaneId, f32)>)`. pane 렌더에서 그 pane이면
  세기에 비례한 알파로 **외곽선**을 그린다.
- 색은 `crate::ui::designall::tokens`에서 고른다(**하드코딩 금지**, 라이트/다크 둘 다).
  `tokens.accent`가 자연스럽다.
- 강조가 살아 있는 동안 `ctx.request_repaint()` — egui는 이벤트 드리븐이라 예약하지 않으면
  페이드가 멈춘다. **강조가 끝나면 예약도 멈춰야 한다**(idle 0 유지 — 이 저장소가 지키는 규칙).
- pane이 하나뿐이어도 강조한다. 짧게 지나가므로 2026-07-04의 불만(항상 보임)과 다르고,
  "여기로 갔다"는 확인이 된다. 이 판단을 주석에 남겨라.

## 하지 않는 것

- 항상 켜진 포커스 테두리(위 기록 참조)
- 사이드바 행 쪽 강조·스크롤
- 포커스 로직 변경 — 이미 동작한다. **건드리지 마라.**

---

### Task 1: 세기 계산 + App 상태

**Files:** `crates/app/src/app.rs`

- [ ] 실패하는 테스트: `pane_highlight_strength`가 0초→1.0, 절반→0과 1 사이, 지속시간 초과→0.0,
      duration이 0이면 0.0(0 나누기 금지).
- [ ] 구현 + `pane_highlight` 필드, `FocusSession` **성공 경로**에서만 세우기.
- [ ] 워크스페이스 전환·pane 종료 시 남은 강조를 지운다(엉뚱한 pane이 번쩍이지 않게).
- [ ] 커밋

### Task 2: 렌더

**Files:** `crates/app/src/ui/workspace.rs`

- [ ] `set_pane_highlight`, pane 외곽선 렌더(세기 비례 알파, 토큰 색).
- [ ] 5118행 주석 옆에 "왜 상시가 아니라 일시인가"를 남긴다.
- [ ] kittest 또는 순수 테스트로 고정: 강조 대상 pane만 외곽선을 받는다.
- [ ] 커밋

### Task 3: 반복 예약 + 게이트

**Files:** `crates/app/src/app.rs`

- [ ] 강조가 살아 있는 동안만 `request_repaint`. 끝나면 멈추는지 소스/테스트로 고정.
- [ ] 게이트: `cargo test -p deppy-sijo`, `cargo clippy --workspace --all-targets -- -D warnings`,
      `cargo run -q -p xtask -- check-boundary`.
- [ ] 커밋
