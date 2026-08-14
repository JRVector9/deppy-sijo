# 에이전트 작업 이력 pane-tab 전환 핸드오프

## 문서 목적

이 문서는 2026-08-13에 배포된 에이전트 작업 이력 기능을 다음 에이전트가 안전하게 재설계하기 위한 코드 기준 핸드오프다. 새 작업의 핵심은 이력을 별도의 전체 중앙 화면으로 보여주는 현재 구조를 버리고, 현재 세션 상단의 `Saju_On ×` 헤더 옆에 `이력` 탭을 추가해 같은 작업면 안에서 터미널과 이력을 전환하는 것이다.

이 단계에서는 저장소·transcript·Git 수집 기능을 다시 만드는 것이 목표가 아니다. 이미 작동하는 이력 데이터와 액션은 유지하고, 표시 위치·탭 상태·레이아웃·디자인을 재구성한다.

## 사용자 확인 사항

- 현재 `이력`을 누르면 터미널 전체 화면이 별도 정보 페이지로 바뀌는 방식은 원하지 않는다.
- 현재 세션의 상단 바 옆에 탭 하나를 더 추가한다.
- 추가 탭에서 에이전트 작업 이력 내용을 보여준다.
- 이후 이 화면을 기준으로 깨지는 레이아웃과 세부 디자인을 정리한다.
- 데이터 수집, 이어 실행, 현재 세션 이동, 새 실행, Git 변경 보기 등 기존 이력 기능은 유지한다.

목표 개념은 다음과 같다.

```text
┌──────────────────┬──────────────────┐
│ Saju_On       ×  │ 이력          ×  │
└──────────────────┴──────────────────┘
                    ▲ 선택됨
┌─────────────────────────────────────┐
│ 현재 워크스페이스의 에이전트 이력   │
│ 검색 / 필터 / 작업 카드 / 액션       │
└─────────────────────────────────────┘
```

세션 탭을 누르면 같은 세션의 터미널로 돌아가고, `이력` 탭을 누르면 이력 본문으로 전환한다. 두 탭은 같은 상단 chrome에 있어야 하며, 전체 중앙 화면이 갑자기 다른 페이지로 바뀐 인상을 주면 안 된다.

## 현재 배포 상태

기준 브랜치는 `main`, 원격 기준 커밋은 `8489527`이다. 주요 구현 커밋은 다음과 같다.

- `5d45f1d`: bounded SQLite 작업 이력 저장소
- `46b7558`: Claude/Codex/Kimi transcript turn 추출
- `a8b52dc`: Git branch/change/diff 기반
- `427ddd2`: 작업 이력 카드 UI와 레일 항목
- `760918f`: App projection, 액션, 렌더 통합
- `1ad9895`: export·compile·Clippy 통합 보정
- `0e934c0`: cwd 귀속, 카드 클릭, 접근성 최종 보정
- `8489527`: release 검증 기록

현재 구현은 기능적으로 완성되어 있다.

- 카드 한 장은 실제 사용자 지시 한 턴이다.
- 현재 워크스페이스의 최근 이력을 저장하고 재시작 후 복원한다.
- Claude, Codex, Kimi transcript를 bounded worker에서 읽는다.
- 검색, 상태 필터, 카드 확장, 새로고침을 제공한다.
- 정확한 durable identity를 재검증한 뒤 현재 세션 이동, 이어 실행, 새 실행을 수행한다.
- 저장된 cwd 기준 branch, 현재 working-tree 변경 수, bounded diff를 제공한다.
- 작업 이력은 워크스페이스당 최대 256행, transcript당 최근 24턴으로 제한된다.

이 데이터·보안·상한 계약은 이번 UI 재설계에서 변경하지 않는다.

## 현재 화면이 전체 교체되는 이유

현재 `이력`은 터미널 내부 탭이 아니라 전역 중앙 view다.

- `crates/app/src/ui/agent_terminal.rs`
  - `AgentTerminalView::{Home, Fleet, History, Terminal}`
- `crates/app/src/ui/file_tree.rs`
  - 레일 클릭이 `SidebarAction::ShowHistory`를 만든다.
- `crates/app/src/app.rs`
  - `ShowHistory`가 `AgentTerminalView::History`와 `Terminal`을 토글한다.
  - `history_visible`이면 `terminal_visible`이 false가 된다.
  - terminal owner, composer, pane 렌더가 모두 비활성화된다.
  - `CentralPanel`에서 `WorkHistoryUi::show`가 WorkspaceUi 대신 전체 중앙 영역을 차지한다.

즉 현재 구조는 다음과 같다.

```text
AgentTerminalView::Terminal
  └─ WorkspaceUi::show
       └─ pane header + terminal surface

AgentTerminalView::History
  └─ WorkHistoryUi::show
       └─ 전체 central content canvas
```

사용자가 본 불연속은 구현 버그가 아니라 기존 설계 자체의 결과다.

## 중요한 용어와 경계

스크린샷의 `Saju_On ×`는 runtime mux의 별도 탭 widget이 아니다. 실제 코드는 `crates/app/src/ui/workspace.rs`의 `WorkspaceUi::render_pane_header`가 그리는 32pt pane header다.

- 높이: `TERMINAL_PANE_HEADER_HEIGHT = 32.0`
- 제목·상단 accent·하단 separator·toolbar·닫기 X가 한 header에 있다.
- 세션 X는 `RuntimeCommand::ClosePane`으로 이어지는 파괴적 동작이다.
- runtime의 `MuxTabId`와 `TabSnapshot`은 터미널 분할 및 pane lifecycle의 권위다.

따라서 `이력`을 runtime tab이나 가짜 pane로 만들면 안 된다.

- `RuntimeCommand::ClosePane`, `KillSession`, `FocusPane`으로 이력 탭 수명을 관리하지 않는다.
- 이력 탭을 열거나 닫아도 PTY, session, mux tab, pane을 생성·종료하지 않는다.
- 세션 X와 이력 X는 서로 다른 액션이어야 한다.
- 이력 X는 UI 상태만 닫고 기존 세션 터미널로 돌아간다.

권장 명칭은 runtime tab과 혼동되지 않는 `WorkspaceContentTab`, `PaneAuxiliaryTab`, `WorkHistoryTabState` 중 하나다.

## 권장 UX 상태 계약

다음 상태 기계를 기본안으로 사용한다.

```rust
enum WorkHistoryTabState {
    Closed,
    OpenInactive,
    OpenActive,
}
```

동작은 다음과 같다.

| 입력 | 이전 상태 | 결과 |
|---|---|---|
| 레일 `이력` 클릭 | Closed | 이력 탭을 만들고 활성화 |
| 레일 `이력` 클릭 | OpenInactive | 기존 이력 탭 활성화 |
| 레일 `이력` 재클릭 | OpenActive | 세션 탭으로 복귀하되 이력 탭은 유지 |
| 세션 탭 클릭 | OpenActive | 터미널 활성화, 이력 탭 유지 |
| 이력 탭 클릭 | OpenInactive | 이력 활성화 |
| 이력 탭 X | OpenActive/OpenInactive | 이력 탭 제거, 터미널 활성화 |
| 세션 탭 X | 어느 상태든 | 기존 pane close 확인·명령만 수행 |
| 이력 카드 `현재 세션으로 이동` | OpenActive | exact focus 성공 후 터미널 탭 활성화 |

이력 탭은 기본적으로 현재 워크스페이스 범위다. 시각적으로 현재 세션 옆에 붙지만, 데이터까지 해당 세션 하나로 필터링하지 않는다. 기존 저장·projection 계약이 현재 워크스페이스 기준이기 때문이다. 세션별 필터가 필요하면 별도 제품 변경으로 다뤄야 한다.

### 워크스페이스·세션 전환 권장값

- 같은 워크스페이스에서 다른 세션을 선택하면 터미널을 활성화하고, 열려 있던 이력 탭은 새 focused pane의 header 옆에 유지한다.
- 워크스페이스를 바꾸면 기존 행을 즉시 지우는 현재 stale 방지 규칙을 유지한다.
- 이력 탭이 열려 있었다면 새 워크스페이스에서도 탭은 유지하고 loading/last-good 규칙으로 새 projection을 요청하는 것이 기존 제품 계약과 가장 가깝다.
- 활성 pane이 하나도 없는 워크스페이스의 동작은 구현 전에 사용자와 확정한다. 권장 fallback은 `이력` 단독 탭을 표시하는 것이다. 데이터는 workspace-scoped라 세션이 없어도 유효하다.

## 권장 구현 방향

### 1. 데이터 계층은 그대로 둔다

다음 파일은 UI 요구가 바뀌지 않는 한 수정하지 않는다.

- `crates/storage/src/db.rs`
- `crates/app/src/agent_transcript.rs`
- `crates/app/src/agent_detect_worker.rs`
- `crates/app/src/agent_state_worker.rs`
- `crates/app/src/agent_work_git.rs`

projection, exact write, generation/epoch guard, cwd 귀속, Git worker 상한을 재작성하지 않는다.

### 2. 전역 History page와 pane-tab 상태를 분리한다

현재 `AgentTerminalView::History`는 `terminal_visible = false`를 만드는 정보 페이지다. 새 구현에서는 History가 Home/Fleet과 같은 전역 정보 페이지가 아니어야 한다.

권장 구조는 다음 둘 중 하나다.

#### A안 — WorkspaceUi에 보조 탭 seam 추가, 권장

- `WorkspaceUi`가 focused pane의 session header와 App 전용 auxiliary tab header를 같은 32pt row에 그린다.
- App은 `WorkHistoryTabState`와 이력 snapshot/action을 소유한다.
- WorkspaceUi는 탭 클릭 intent만 반환하고 저장소나 WorkHistory action을 소유하지 않는다.
- 이력 활성 상태에서는 terminal surface 대신 App이 `WorkHistoryUi`를 같은 pane body rect에 렌더한다.
- terminal input owner와 composer는 이력 활성 상태에서 명시적으로 None/hidden이다.

이 방식은 실제 header와 정확히 같은 높이·색·separator를 공유할 수 있고, 가짜 runtime tab을 만들지 않는다. 다만 App-owned `WorkHistoryUi`를 body rect에 넣기 위한 얇은 composition seam이 필요하다.

#### B안 — App이 공통 content tab strip을 소유

- App이 terminal/history 위에 별도 32pt strip을 그린다.
- terminal 렌더에서는 기존 embedded pane header를 숨기거나 재구성한다.
- history 렌더에서도 같은 strip을 쓴다.

이 방식은 App composition이 단순하지만 split pane의 각 header, toolbar, close semantics를 다시 조합할 위험이 크다. 기존 pane chrome을 복제하지 말고 공용 pure helper로 추출할 때만 선택한다.

### 3. 기존 pane header를 통째로 복사하지 않는다

`render_pane_header`에는 제목 clipping, toolbar 축소, top-line pixel snap, close hover error tone, context menu, exact focus claim이 이미 있다. 이 코드를 별도 History header로 복사하면 두 구현이 바로 어긋난다.

필요하면 다음처럼 순수 geometry/style helper를 추출한다.

- session tab rect
- optional history tab rect
- session close rect
- history close rect
- toolbar rects
- narrow-width priority

좁은 폭에서는 우선순위를 명시한다.

1. 세션 제목 최소 폭
2. 활성 탭 구분
3. 각 탭의 닫기 버튼
4. toolbar 아이콘
5. 긴 제목 생략

닫기 버튼이 겹치거나 탭 label이 0폭이 되면 안 된다.

### 4. WorkHistoryUi는 pane body에 맞게 재배치한다

현재 `WorkHistoryUi::show`는 전체 정보 페이지를 전제로 한다.

- 좌우 22pt, 상하 18pt margin
- 큰 `작업과 변경` header
- workspace/branch subheader
- 검색·필터·count
- 전체 높이 ScrollArea
- `available_height - 36.0` 보정

pane-tab 버전에서는 상단 32pt tab strip이 별도로 존재한다. `-36.0` 같은 마법값을 추가하지 말고 실제 전달받은 body rect의 available height를 사용한다.

디자인 정리 권장값:

- full-width 정보 페이지 느낌의 큰 상단 여백을 줄인다.
- workspace명과 branch는 탭 아래 compact context row로 만든다.
- 검색과 필터가 좁은 폭에서 겹치지 않도록 wrap/stack breakpoint를 둔다.
- 카드 instruction, summary, metadata는 각각 최대 행 수와 ellipsis 규칙을 둔다.
- card expansion 안의 action 버튼은 wrap되되 카드 toggle의 접근성 자손이 되지 않게 유지한다.
- 하나의 vertical ScrollArea만 사용한다. body와 card 내부에 중첩 스크롤을 만들지 않는다.
- 현재 working-tree 변경 수는 계속 “이 에이전트가 만든 변경”으로 표현하지 않는다.

### 5. 입력·focus 권한은 fail-closed로 유지한다

현재 History 전역 view에서는 다음이 명시적으로 꺼진다.

- `frame_terminal_owner = None`
- composer hidden
- pane input/render hidden
- hidden workspace event는 `update_hidden`으로 계속 소비

pane-tab으로 옮겨도 이 계약은 유지한다.

- 이력 탭 활성 중 키 입력, IME, paste, drag/drop, composer send가 숨은 terminal로 가면 안 된다.
- 이력 검색창에 입력 focus가 있으면 terminal shortcut이 작동하면 안 된다.
- session tab으로 돌아온 첫 프레임부터 exact focused pane만 입력 owner가 된다.
- `현재 세션으로 이동`은 controller가 exact identity를 재검증하고 focus command를 수락한 뒤에만 terminal tab을 활성화한다.
- stale action이나 backpressure면 이력 탭을 유지하고 현재 오류/loading 상태를 갱신한다.

### 6. 레일 의미를 바꾼다

`SidebarAction::ShowHistory` 이름은 유지해도 되지만 의미는 “전역 History view 토글”이 아니라 “현재 workspace의 History auxiliary tab 열기/활성화”가 된다.

- 레일 selected 상태는 `OpenActive`일 때만 강조한다.
- `OpenInactive`는 탭이 열려 있어도 레일을 active로 칠하지 않는다.
- 레일 재클릭은 탭을 제거하지 않고 session으로 돌아가는 기본안을 따른다.
- History X만 탭을 제거한다.

## 예상 수정 파일

- `crates/app/src/app.rs`
  - tab state, rail action, workspace switch, render branch, projection trigger, input owner, action completion
- `crates/app/src/ui/workspace.rs`
  - session/history tab geometry와 intent, 기존 pane header seam
- `crates/app/src/ui/work_history.rs`
  - pane body용 compact layout, responsive rules
- `crates/app/src/ui/agent_terminal.rs`
  - `AgentTerminalView::History` 제거 또는 transitional compatibility 정리
- `crates/app/src/ui/file_tree.rs`
  - rail selected/action semantics
- 다섯 i18n catalog
  - `이력` tab label, close tooltip, 접근성 label 등
- `docs/CODEX_HANDOFF.md`
  - 구현 단계별 상태와 실제 검증 결과

저장·parser·Git 파일까지 diff가 커지면 UI 범위를 벗어났는지 먼저 확인한다.

## 제거하거나 바꿔야 할 현재 조건

심볼명 기준으로 다음 지점을 모두 찾아야 한다.

- `AgentTerminalView::History`
- `history_visible`
- `information_visible = home_visible || fleet_visible || history_visible`
- `terminal_visible = central_view == AgentTerminalView::Terminal`
- `ShowHistory` match arm
- workspace switch 시 History 유지 조건
- work-history exact 성공 후 `view() == History` projection 재요청
- scope cutover 후 `view() == History` projection 재요청
- `work_history_loading = view() == History`
- central render의 `else if history_visible`
- `WorkHistoryAction::Activate`와 workspace controller 성공 후 Terminal 전환
- History 관련 source-law tests

이 조건을 단순 삭제하지 말고 새 `WorkHistoryTabState::OpenActive` 또는 동등한 predicate로 치환한다.

## 회귀 위험

### runtime 의미 혼합

가장 큰 위험은 이력 탭을 `MuxTabId`/pane처럼 취급하는 것이다. History X가 세션 종료 명령으로 이어지거나 mux snapshot에 가짜 항목이 생기면 설계 실패다.

### split pane과 cross-workspace pane

현재 WorkspaceUi는 active runtime tab의 split layout을 렌더하고, App은 다른 workspace pane strip도 렌더한다. 이력 탭이 어느 header에 붙는지 명확해야 한다.

기본안은 primary active workspace의 focused pane header다. attached foreign pane header에는 이력 탭을 만들지 않는다. attached pane이 input owner였더라도 레일 History는 active workspace history를 여는 기존 범위를 유지한다.

### stale workspace 데이터

워크스페이스 전환 직후 이전 workspace의 카드가 새 이름 아래 한 프레임이라도 보이면 안 된다. 기존 `work_history_workspace_id`, scope, epoch, Git generation 검증을 유지한다.

### close·focus hit testing

세션 tab, History tab, 두 close button, toolbar가 같은 32pt row에 들어간다. 부모 click target이 자식 close/button을 가로채지 않아야 한다. AccessKit tree에서도 Button 안에 Button을 만들지 않는다. `0e934c0`의 History card hit-test/accessibility 교훈을 그대로 적용한다.

### 높이 계산과 스크롤

tab strip 32pt, status bar, composer 유무를 각각 한 번만 차감한다. History body에서 terminal용 archived notice/composer 공간을 잘못 예약하지 않는다. 좁은 창과 작은 split에서 음수·0폭 rect를 만들지 않는다.

## 구현 전 확정할 질문

다른 에이전트는 코드를 쓰기 전에 아래 네 가지를 사용자와 확인하거나 문서에 명시적 기본값으로 고정한다.

1. 이력 탭 X를 항상 표시할지, session tab 클릭만으로 닫을지
2. 이력 탭을 워크스페이스 전환 뒤에도 유지할지
3. 활성 세션이 없는 workspace에서 이력 단독 탭을 허용할지
4. split pane일 때 focused pane header에만 붙일지, 중앙 전체 공통 strip으로 올릴지

이 문서의 권장 기본값은 `X 표시`, `workspace 전환 후 유지`, `세션 없어도 단독 표시`, `primary focused pane에만 부착`이다.

## 검증 계획

구현 에이전트는 먼저 현재 baseline을 기록하고, UI 변경 뒤 다음을 검증한다.

### 상태·동작

- 레일 클릭으로 session 옆 History tab이 생기고 활성화된다.
- session tab과 History tab 전환이 runtime 명령 없이 동작한다.
- History X는 세션을 종료하지 않는다.
- session X는 기존 확인·ClosePane 경로를 그대로 사용한다.
- 현재 세션 이동은 exact focus 성공 뒤 terminal로 돌아간다.
- 이어 실행·새 실행·Git 변경 보기·새로고침은 유지된다.
- workspace 전환 중 stale rows와 stale Git 결과를 적용하지 않는다.

### 입력·접근성

- History 활성 중 타이핑·IME·붙여넣기가 PTY로 가지 않는다.
- 검색창 입력과 전역 terminal shortcut이 충돌하지 않는다.
- 각 tab과 각 X는 독립 hit target과 접근성 label을 가진다.
- nested Button 접근성 구조가 없다.
- 키보드로 session/History tab을 선택할 수 있다.

### 레이아웃

- 폭이 넓은 단일 pane
- 폭이 좁은 단일 pane
- 좌우·상하 split의 focused pane
- 긴 session title
- toolbar 아이콘이 줄어드는 최소 폭
- 카드 0개, 1개, 24개 이상
- 긴 한국어/영어 instruction과 branch
- error/loading/empty/no-results
- macOS Retina에서 top accent와 separator가 흐려지지 않음

### 명령

테스트 이름은 구현 구조에 맞춰 추가하되 최소 게이트는 다음과 같다.

```bash
cargo test -p deppy-sijo --locked work_history -- --nocapture
cargo test -p deppy-sijo --locked ui::workspace::tests:: -- --nocapture
cargo test -p deppy-sijo --locked
cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
cargo check -p deppy-sijo --all-targets --locked
cargo fmt --all -- --check
git diff --check
```

앱 package/relaunch는 사용자가 명시적으로 요청할 때만 수행한다. 테스트를 실행하지 않았다면 통과했다고 기록하지 않는다.

## 완료 기준

- `Saju_On ×` 옆에 실제 같은 chrome을 쓰는 `이력` 탭이 보인다.
- 이력 활성 중에도 session tab이 남아 있어 한 번의 클릭으로 terminal로 돌아간다.
- History X는 UI tab만 닫고 PTY/session에는 어떤 종료 명령도 보내지 않는다.
- 기존 이력 데이터·검색·필터·카드·실행·Git 기능이 유지된다.
- 전역 중앙 정보 페이지처럼 보이던 큰 레이아웃이 pane body에 맞는 compact layout으로 정리된다.
- narrow/split/long-text/loading/error 상태에서 겹침·잘림·이중 스크롤이 없다.
- 입력 owner, workspace epoch, durable identity, bounded worker 계약이 회귀하지 않는다.

## 다음 에이전트 시작 순서

```bash
sed -n '1,260p' AGENTS.md
sed -n '1,260p' docs/CODEX_HANDOFF.md
sed -n '1,320p' docs/superpowers/specs/2026-08-14-agent-work-history-pane-tab-handoff.md
git status --short --branch
git log -12 --oneline
rg -n "AgentTerminalView::History|ShowHistory|history_visible|render_pane_header|WorkHistoryUi" crates/app/src
```

그 다음 현재 배포 화면을 캡처하고, 위 네 가지 미확정 질문을 잠근 뒤 구현 계획을 별도 plan 문서로 작성한다. 저장·ingestion 계층은 UI 계획에서 제외하고, 먼저 tab state와 header geometry RED 테스트를 만든다.
