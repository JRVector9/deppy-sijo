# Pane 분할·리사이즈 깜빡임 제거 설계

작성일: 2026-08-24
상태: 사용자 동작 선택 완료(A: drag 중 기존 grid 유지, release 후 1회 resize)

## 1. 목표

터미널 pane을 새로 분할하거나 divider를 드래그할 때 터미널 내용·배경·divider가 이전
상태로 되튀거나, 중간 clear snapshot·빈 grid·반복 SIGWINCH redraw를 노출하지 않는다.

사용자가 승인한 동작 계약은 다음과 같다.

- divider는 포인터를 따라 실시간으로 움직인다.
- drag 중 PTY의 cols/rows는 바꾸지 않는다. 터미널 출력은 기존 grid 크기로 계속 처리한다.
- pane을 좁히면 기존 화면을 현재 rect로 clip하고, 넓히면 새 영역을 고정 터미널 배경으로
  채운다. 내용과 scrollback은 삭제하지 않는다.
- release 후 최종 크기를 정확히 한 번 적용한다.
- 최종 크기의 표시 가능한 viewport가 준비될 때까지 마지막 안정 화면을 유지한다.
- resize에 반응한 child TUI의 clear→redraw 중간 viewport는 화면에 내보내지 않는다.

이 계약은 좌우/상하, root/nested divider, 50px minimum 경계, 빠른 연속 drag, 선택 중
resize, split 생성 직후를 포함한다. OS 창 테두리 자체를 드래그하는 동작은 기존
`RESIZE_DRAG_DEBOUNCE` 경로의 별도 문제이므로 이번 범위에 포함하지 않는다.

## 2. 조사로 확정한 원인

### 2.1 release snap-back

`WorkspaceUi::split_handle`은 `drag_stopped`에서 `split_drag.take()`로 preview를 즉시
버리고 `ResizeSplit`을 비동기로 보낸다. App은 runtime 이벤트를 drain한 뒤 render에서
protocol intent를 전달하므로, ACK인 `MuxUpdated`가 오기 전에 저장된 이전 ratio를 다시
그리는 프레임이 존재한다. 결과는 `최종 preview → 이전 ratio → 최종 persisted ratio`다.

### 2.2 drag 도중 실제 PTY resize

기존 120ms debounce는 pointer drag 상태가 아니라 목표 cols/rows가 같은 시간만 본다.
안정 상태의 첫 mismatch는 즉시 전송되고, 마우스를 누른 채 한 cell 구간에 120ms 머물러도
중간 크기가 전송된다. 각 `Session::resize`는 backend reflow, PTY resize(SIGWINCH),
full-dirty를 만들며 TUI가 clear/redraw한 중간 화면까지 Viewport로 나갈 수 있다.

### 2.3 rect와 snapshot grid의 시점 불일치

pane rect는 pointer frame마다 즉시 바뀌지만 snapshot cols/rows는 resize 이후에 바뀐다.
확대 시 옛 grid 뒤가 배경 띠로 보이고 축소 시 clip된다. 이 자체는 A 동작의 의도된 안정
preview지만, drag 중간에 PTY resize가 섞이면 grid가 여러 번 바뀌어 띠와 내용이 왕복한다.

### 2.4 split 직후 빈 seed viewport

새 split shell은 80×24 빈 backend로 생성된다. runtime이 `MuxUpdated → ShellSpawned →
push_watched_viewports`를 즉시 수행하면서 출력 전 빈 snapshot을 발행한다. UI는 snapshot이
있다는 이유로 연결 중 상태를 건너뛰어 새 pane 전체가 잠깐 빈 terminal surface가 된다.

### 2.5 선택 freeze와 늦은 ACK

선택 중 Viewport는 `pending_snapshot`에만 쌓이므로 최종 resize 뒤에도 옛 grid가 남을 수
있다. 또한 첫 drag의 늦은 `MuxUpdated`가 두 번째 drag를 구조 변경으로 오인해 preview를
지우면 빠른 연속 drag에서 snap-back이 재발한다.

## 3. 검토한 접근

### 접근 A — UI split transaction + bounded presentation fence (채택)

divider drag의 수명을 명시적인 상태로 만들고, active/committed 상태에서는 PTY resize를
보류한다. matching layout ACK 뒤 최종 resize를 한 번 보내며, 짧고 유계인 presentation
fence가 resize 직후 viewport burst의 최신값만 승격한다.

- 장점: wire protocol을 크게 바꾸지 않고 모든 사용자 가시 중간 상태를 한곳에서 제어한다.
- 단점: release 뒤 최종 reflow 표시가 수십 ms 늦을 수 있다.

### 접근 B — runtime revision/ACK와 원자적 layout+viewport transaction

`ResizeSplit`에 revision과 모든 child grid 크기를 넣고 runtime이 layout·PTY·viewport를
원자적으로 응답한다.

- 장점: 가장 강한 계층 간 원자성이다.
- 단점: command/event/persistence/remote protocol 전부를 바꾸며 pane UI 버그에 비해 범위가
  지나치게 크다.

### 접근 C — 기존 debounce 시간만 증가

120ms를 늘려 중간 resize 확률만 낮춘다.

- 장점: 변경이 작다.
- 단점: pointer를 누른 채 멈추는 경우, release snap-back, 빈 seed viewport를 해결하지
  못하며 깜빡임이 사라진다는 보장을 제공하지 않는다.

## 4. 상태와 데이터 흐름

### 4.1 split transaction

현재 `Option<(path, ratio)>` 대신 tab/path/ratio/phase를 가진 bounded 단일 상태를 둔다.

- `Active`: divider가 pointer를 따라가는 중이다.
- `Committed`: pointer는 놓였고 `ResizeSplit`이 수락됐으며 matching `MuxUpdated`를 기다린다.

렌더는 두 phase 모두 preview ratio를 사용한다. 따라서 release와 ACK 사이에도 divider가
이전 ratio로 되돌아가지 않는다.

`drag_stopped`에서 command admission이 실패하면 preview를 유지하고 자연스러운 다음
protocol wake에서 재시도한다. 큐 실패 때문에 저장 ratio로 조용히 되돌아가지 않는다.

`MuxUpdated` 처리 규칙:

1. 현재 tab/path가 사라졌으면 transaction을 취소한다.
2. `Committed`의 tab/path ratio와 snapshot ratio가 일치하면 ACK로 보고 transaction을
   끝낸다.
3. ratio가 다르면 오래된 ACK이므로 최신 preview를 유지한다.
4. 새 `Active` drag는 이전 command의 ACK으로 절대 지우지 않는다.

### 4.2 drag 중 resize suppression

split transaction이 `Active`인 동안 visible terminal session의 `Resize` 생성을 모두
보류한다. subtree를 별도로 계산하는 복잡도를 만들지 않는다. drag는 짧고, 영향 없는 pane의
rect는 변하지 않아 suppression으로 유실되는 실제 목표도 없다.

`Committed` 동안도 matching layout ACK까지 보류한다. ACK가 적용된 첫 render에서 최종
rect로 계산한 서로 다른 `(cols, rows)`를 각 session당 정확히 한 번 전송한다. 기존
`sent_sizes` latest-only 규칙은 중복 전송 방어로 계속 사용한다.

최종 grid가 바뀌는 session에 선택이 걸려 있으면 resize admission 시 선택을 해제한다.
grid 좌표가 바뀐 뒤 옛 좌표로 snapshot을 freeze하는 것보다 일관된 동작이다.

### 4.3 viewport presentation fence

최종 `Resize`가 수락되면 session별로 다음 정보만 유계 저장한다.

- 마지막 안정 `snapshot`
- 목표 `(cols, rows)`
- 목표 크기로 도착한 최신 pending snapshot 하나
- 시작 시각, 32ms settle deadline, 250ms hard deadline

목표와 다른 viewport는 표시 상태를 바꾸지 않는다. 목표 크기의 첫 viewport가 와도 즉시
승격하지 않고 32ms(현재 active viewport cadence 8ms의 4배) 동안 최신값으로 덮어쓴다.
child TUI가 clear와 redraw를 여러 pump tick에 나눠 보내도 사용자는 마지막 안정 화면을
본다. 마지막 target-sized viewport 뒤 32ms가 조용하면 최신값 하나만 승격한다. 기존 안정
화면이 nonblank인데 최신 후보가 완전히 blank면 32ms가 지나도 기다리며, nonblank 후보가
오거나 첫 target-sized viewport로부터 250ms hard deadline에 도달했을 때 승격한다. 따라서
정상 clear 명령도 영구 차단하지 않는다. 출력이 계속되는 경우에도 같은 hard deadline에서
반드시 최신값을 승격해 화면이 freeze되지 않는다.

이 fence는 divider transaction이 만든 최종 resize에만 적용한다. 일반 출력, 입력, scroll,
OS window resize에는 새 지연을 추가하지 않는다. session/tab 소멸 시 즉시 정리한다.

시간 기반 테스트에서는 clock 주입 또는 순수 판정 helper를 써서 sleep 의존을 만들지 않는다.

### 4.4 split 생성의 최초 표시

새 split session의 출력 전 빈 seed snapshot은 사용자 표시용 snapshot으로 승격하지 않는다.
새 pane은 동일한 terminal surface 배경과 안정된 연결 상태를 유지한다. 첫 output-derived
viewport가 오면 한 번에 터미널 내용으로 전환한다.

shell이 의도적으로 아무 출력도 내지 않는 경우를 위해 seed 억제는 250ms deadline을 가진다.
deadline까지 output이 없으면 빈 grid를 한 번 표시하고 종료한다. 문자열 내용으로 빈 화면을
추측하지 않고, runtime이 알고 있는 `spawn seed`와 `pump output` 원인을 사용한다.

## 5. 렌더·포커스 정책

- pane surface는 매 프레임 기존 terminal 배경으로 먼저 칠한다.
- drag 중 snapshot의 cell 크기와 glyph cache는 바꾸지 않는다.
- 확대된 새 영역은 배경, 축소된 영역은 clip이다. texture stretching은 하지 않는다.
- split으로 새 pane에 focus가 이동하는 기존 accent 표시 자체는 유지한다. 이것은 terminal
  내용의 blank/rollback이 아니며, sidebar jump 등 다른 포커스 피드백과 같은 계약이다.
- discarded/correction render pass는 protocol 또는 resize transaction 상태를 진전시키지
  않는다. 최종 paint pass에서만 command admission을 수행한다.

## 6. 실패·경계 처리

- protocol queue busy: committed preview 유지, 다음 자연스러운 wake에서 재시도한다.
- tab/path 소멸: transaction과 presentation fence를 정리하고 현재 mux snapshot을 따른다.
- session 종료/숨김: 해당 resize fence와 pending snapshot을 제거한다.
- 50px를 물리적으로 만족할 수 없는 rect: 기존 비례 fallback을 그대로 사용하되 resize
  suppression/ACK 규칙은 동일하게 적용한다.
- rapid drag: 최신 Active/Committed 한 건만 소유하며 오래된 ACK은 ratio 불일치로 무시한다.
- non-finite ratio와 invalid path: 기존 protocol 검증을 유지한다.

## 7. 테스트와 실측

구현 전 다음 테스트가 현재 코드에서 RED여야 한다.

1. 안정 상태에서 시작한 divider drag는 첫 cell mismatch도 `Resize`를 보내지 않는다.
2. 120ms보다 오래 멈춘 active drag도 중간 `Resize`를 보내지 않는다.
3. release 뒤 ACK 전 render가 committed ratio를 유지한다.
4. 이전 ACK이 빠른 두 번째 drag preview를 지우지 않는다.
5. matching ACK 뒤 session별 최종 크기만 정확히 한 번 전송된다.
6. selection이 있는 session은 최종 grid resize 시 freeze를 해제한다.
7. target-size clear→redraw viewport burst에서 clear snapshot은 표시되지 않고 최신값만
   settle 후 승격된다.
8. fence hard deadline 뒤에는 viewport가 반드시 승격된다.
9. split의 빈 seed viewport는 표시하지 않고 첫 output viewport를 한 번에 표시한다.
10. root/nested, horizontal/vertical, exact 50px, 물리적으로 부족한 rect를 모두 순회한다.
11. correction/discard pass는 `Resize` 또는 `ResizeSplit`을 중복 admission하지 않는다.

실측 항목:

- drag 한 번의 runtime `ResizeSplit` 수: 1
- drag active 동안 runtime `Resize` 수: 0
- matching ACK 뒤 각 영향 session의 distinct final `Resize` 수: 최대 1
- divider의 표시 ratio sequence에 이전 persisted ratio 재등장: 0
- resize presentation 중 blank/clear snapshot 승격: 0
- renderer row rebuild는 drag 중 snapshot generation 변화가 없을 때 기존 cache hit 계약 유지

최종 검증은 workspace/mux/session/runtime/terminal 관련 집중 테스트, strict Clippy, fmt,
boundary, Codex 독립 리뷰 후 signed debug 재빌드·재실행으로 마친다. 실제 화면에서는 좌우/상하,
root/nested, 느린 drag, 멈췄다 재이동, 빠른 연속 drag, 선택 후 drag, split 생성 순서로 확인한다.

## 8. 비목표

- 터미널 내용을 drag rect에 맞춰 늘이거나 축소하는 texture scaling
- child TUI 자체의 resize 동작 변경
- OS window live-resize 프로토콜 재설계
- pane focus accent와 일반 출력 repaint 제거
- runtime wire protocol에 layout revision을 추가하는 대규모 변경
