# 모바일 웹 리모트 전체화면 뷰어 설계

작성일: 2026-08-28  
상태: A안 승인 — 구현 계획 작성 전 제품 상세 설계

## 1. 목적

현재 `crates/web-remote`의 세션 화면은 대시보드 안의 패널을 펼친 뒤 해당 위치로 스크롤한다. 모바일에서 세션을 선택하면 대시보드를 숨기고 브라우저가 제공하는 전체 viewport를 터미널에 사용하도록 바꾼다.

이 작업은 기존 Tailscale Serve 경로와 WebSocket protocol v3를 그대로 사용한다. Relay, 기기 identity, E2EE, 권한·폐기 persistence는 독립된 후속 단계이며 이 변경에 포함하지 않는다.

## 2. 목표와 성공 조건

- 세션의 `보기` 또는 승인 카드의 `화면 보기`를 누르면 별도의 full-viewport 뷰어 상태로 진입한다.
- 일반 Safari/Chrome에서는 현재 visual viewport를, 설치형 PWA에서는 standalone 창 전체를 사용한다.
- 브라우저 Back, 상단 뒤로 버튼, 선택 세션 종료를 하나의 종료 경로로 수렴시킨다.
- 재연결 중에는 마지막 terminal canvas와 작성 중 draft를 보존하고 모든 원격 입력을 잠근다.
- 키보드, 회전, safe-area 변화에 맞춰 terminal grid 전체가 가용 폭과 높이 안에 들어오도록 다시 계산한다.
- 종료 시 대시보드 접근성·스크롤을 복구하고 원래 세션의 `보기` 버튼으로 focus를 돌린다.
- 기존 페어링, 승인, 세션 목록, 업로드, 알림 딥링크, Tailscale 동작과 protocol v3 메시지는 의미가 바뀌지 않는다.

## 3. 비목표

- 브라우저 Fullscreen API 사용
- Relay URL 또는 outbound WSS 연결
- 새로운 인증·페어링·권한 프로토콜
- WebSocket 메시지 형식 또는 서버의 watch ownership 변경
- 데스크톱 앱의 Tailscale 설정 및 Serve lifecycle 변경
- 터미널 데이터를 별도의 DOM 텍스트 렌더러로 재작성

## 4. 선택한 구조

### 4.1 DOM 경계

`#viewer`를 `main.app`의 자식에서 `body`의 직계 자식으로 옮긴다.

```text
body
├── main#dashboard-shell.app
└── section#viewer.viewer-shell[hidden]
    ├── header.viewer-header
    ├── div.viewer-stage
    │   ├── div.viewer-wrap > canvas
    │   ├── div.viewer-scroll-note
    │   ├── div.viewer-connection-overlay
    │   └── div.viewer-privacy-curtain
    └── footer.viewer-controls
        ├── div.viewer-keys
        ├── div.composer
        └── p.composer-note
```

대시보드와 뷰어를 형제 경계로 분리해야 뷰어 자신을 비활성화하지 않고 `#dashboard-shell` 하나에 `inert`와 `aria-hidden`을 적용할 수 있다. 뷰어는 `role="dialog"`, `aria-modal="true"`, `aria-labelledby`를 갖는다.

### 4.2 레이아웃

- `.viewer-shell`은 `position: fixed`, `inset-inline: 0`, CSS 변수 기반 `top`과 `height`, 높은 `z-index`를 사용한다.
- 기본 높이는 `100dvh`; JavaScript가 `visualViewport`를 지원하는 브라우저에서는 실제 `offsetTop`과 `height`를 CSS 변수에 반영한다.
- 상단과 하단은 `env(safe-area-inset-*)`를 포함한다.
- 내부는 `header / minmax(0, 1fr) / footer` 3행 grid다.
- 특수키는 44px 이상 touch target을 유지하고 가로로 스크롤한다.
- composer 글자 크기는 iOS focus 확대를 피하도록 16px 이상으로 한다.
- `body.viewer-open`은 배경 스크롤과 overscroll을 잠근다.

## 5. 상태 계약

`viewer.watching`을 선택한 세션의 단일 진실 소스로 유지한다. 별도의 boolean `viewerOpen`을 만들지 않는다.

뷰어가 소유하는 보조 상태는 다음으로 제한한다.

- `viewer.screen`: 마지막 완성 또는 delta 반영 화면
- `viewer.returnSession`: 종료 후 focus를 복구할 session id
- `viewer.connection`: `connecting | connected | reconnecting | paused`
- 기존 `inputBlocked`, `lastSent`, upload 상태

`viewer.el.hidden`과 `body.viewer-open`은 파생 UI 상태이며 `viewer.watching`과 수동으로 따로 진행시키지 않는다.
history ownership은 별도 boolean으로 복제하지 않고 현재 `history.state.deppyViewer`에서 판정한다.

## 6. 진입·종료 lifecycle

### 6.1 진입

`openViewer(sessionId, title)`은 다음 순서로 동작한다.

1. 같은 세션을 이미 보고 있으면 중복 history entry나 중복 초기화를 만들지 않는다.
2. `viewer.watching`, `returnSession`, 제목을 설정하고 화면·스크롤 상태를 초기화한다.
3. `#dashboard-shell`을 `inert`/`aria-hidden`으로 만들고 `body.viewer-open`을 설정한다.
4. 뷰어를 표시하고 현재 history entry가 뷰어 entry가 아니면 `pushState`를 한 번 호출한다.
5. 상단 뒤로 버튼으로 focus를 옮긴다.
6. 연결돼 있으면 기존 `{ type: "watch", session }` 메시지를 보낸다. 연결 전이면 welcome 처리에서 보낸다.
7. 첫 layout 측정과 canvas redraw를 예약한다.

알림 딥링크와 승인 카드도 같은 함수로 진입하므로 별도 화면 상태를 만들지 않는다.

### 6.2 종료

상단 뒤로 버튼은 뷰어가 만든 history entry가 있으면 `history.back()`을 요청한다. `popstate`, 직접 종료가 필요한 세션 소멸, history entry가 없는 방어 경로는 모두 하나의 실제 cleanup 함수로 수렴한다.

cleanup은 다음을 한 번만 수행한다.

1. 연결돼 있으면 기존 `{ type: "unwatch" }`를 보낸다.
2. `viewer.watching`, 화면, 스크롤, 입력 압박 안내를 초기화한다.
3. 뷰어를 숨기고 `body.viewer-open`을 제거한다.
4. 대시보드의 `inert`와 `aria-hidden`을 제거한다.
5. 필요한 경우 세션 목록을 다시 렌더해 `보는 중` 표시를 제거한다.
6. `data-session-id`가 같은 현재 `보기` 버튼을 찾아 focus를 복구한다. 세션이 사라졌으면 `tabindex="-1"`인 세션 제목으로 fallback한다.

history에서 이미 빠진 `popstate` 처리 중에는 다시 `history.back()`을 호출하지 않는다. 종료 함수는 중복 호출돼도 추가 `unwatch`나 잘못된 focus 이동을 만들지 않아야 한다.

### 6.3 세션 소멸

새 dashboard frame에서 `viewer.watching`에 해당하는 세션이 없거나 `exited` 상태면 세션 종료 사유로 같은 cleanup을 실행하고 세션 목록에 안내를 표시한다. 이 경로에서는 현재 dashboard render를 그대로 완료하고 cleanup의 재렌더 옵션을 끄므로 재귀 렌더링을 만들지 않는다. 연결 자체가 끊긴 경우에는 dashboard 부재를 세션 소멸로 간주하지 않으며 뷰어를 유지한다.

## 7. 연결과 입력 안전성

기존 WebSocket 연결 상태를 뷰어에도 반영한다.

- `connect()` 시작: `connecting`
- 인증된 `welcome`: `connected`, overlay 제거, 보고 있던 세션을 다시 `watch`
- 예상하지 않은 `close`: `reconnecting`, 마지막 canvas 유지, overlay 표시
- background 전환: `paused`, privacy curtain 표시 후 연결 종료

특수키, composer 전송, textarea, 첨부는 `viewer.watching && connection === connected`일 때만 활성화한다. 기존 PTY `inputBlocked`와 upload busy 조건은 추가로 유지한다.

연결이 끊겨도 textarea value를 비우지 않고, 복구 후 자동 전송하지 않는다. WebSocket write 직후 PTY 거부에 사용하는 기존 `lastSent` 복구 계약도 유지한다. 재연결 overlay는 `role="status"`와 `aria-live="polite"`로 상태를 전달한다.

## 8. viewport 측정과 terminal 렌더링

`visualViewport.resize`와 `visualViewport.scroll`을 우선 구독하고, 미지원 환경은 `window.resize`로 동작한다. 연속 이벤트는 `requestAnimationFrame` 하나로 합쳐 layout thrashing과 canvas 깜빡임을 막는다. 뷰어 stage의 실제 크기 변화는 `ResizeObserver`로 같은 redraw 예약 함수에 합친다.

canvas cell 크기는 폭만 보지 않고 다음 두 제약 중 작은 값을 사용한다.

```text
cellWidth <= availableWidth / columns
cellWidth <= availableHeight / (rows * cellAspectRatio)
cellHeight = cellWidth * cellAspectRatio
```

따라서 terminal grid 전체가 stage 안에 들어오며 남는 축은 검은 배경으로 letterbox한다. canvas backing store는 device pixel ratio를 반영하되 CSS 크기와 분리한다. 화면 frame과 resize가 동시에 와도 한 animation frame에 한 번만 그린다.

## 9. 모바일 Back과 focus

- 진입 시 `history.pushState({ deppyViewer: true }, "", 현재 경로)`를 한 번만 추가한다.
- 브라우저 Back의 `popstate`는 페이지 이탈 대신 뷰어 cleanup을 실행한다.
- 상단 뒤로 버튼도 같은 history entry를 소비한다.
- 직접 URL로 들어온 알림 딥링크는 기존 URL 위생 처리 후 동일하게 entry를 추가한다.
- 배경은 `inert`라 뷰어가 열린 동안 focus가 대시보드로 이동하지 않는다.
- 닫을 때 재렌더된 현재 버튼을 session id로 찾아 focus를 돌려 stale element 참조를 피한다.

## 10. privacy curtain

뷰어가 열린 상태에서 문서가 hidden이 되면 canvas보다 위에 불투명 curtain을 즉시 표시한 뒤 기존 `disconnect()`를 호출한다. visible 복귀 시 연결 상태 overlay를 유지한 상태로 curtain을 내리고 재연결한다. terminal 출력·입력·토큰은 로그에 남기지 않는다.

## 11. 오류 처리

- watch 전송 전 연결이 없으면 뷰어를 닫지 않고 welcome 뒤 재시도한다.
- 재연결 중 입력은 비활성화하고 draft를 보존한다.
- protocol version 불일치는 기존처럼 셸 reload로 복구한다.
- 선택 세션 종료는 목록 복귀와 안내로 복구한다.
- 뷰어 cleanup 중 `unwatch` 전송 실패는 별도 재시도하지 않는다. 끊긴 socket에는 watch ownership이 남지 않고 새 연결에서도 `viewer.watching`이 비어 있기 때문이다.
- `visualViewport` 또는 `ResizeObserver` 미지원은 window resize와 직접 redraw로 폴백한다.

## 12. 변경 경계

제품 변경:

- `crates/web-remote/assets/index.html`
- `crates/web-remote/assets/app.css`
- `crates/web-remote/assets/app.js`

회귀 테스트 변경은 `crates/web-remote/src/static_srv.rs`의 임베드 셸 계약에 한정할 수 있다. 서버 routing, WebSocket protocol, repository, Tailscale 설정 코드는 변경하지 않는다.

## 13. 검증 전략

1. TDD 정적 셸 계약: 뷰어가 대시보드의 형제인지, modal 접근성·연결 overlay·privacy curtain 요소가 존재하는지, CSS/JS 전체화면 lifecycle marker가 임베드 자산에 포함되는지 먼저 실패하는 Rust 테스트로 고정한다.
2. 기존 서버 회귀: `cargo test -p web-remote --locked -- --test-threads=1`로 정적 서빙, 인증, WebSocket, watch/input/upload 경로를 확인한다.
3. 구문·정적 품질: `node --check crates/web-remote/assets/app.js`, `cargo fmt --all -- --check`, `cargo clippy -p web-remote --locked --all-targets -- -D warnings`, `git diff --check`를 실행한다.
4. 브라우저 실측: 모바일 폭/높이, 세로·가로, Back, keyboard/visualViewport, 재연결 overlay, draft 유지, 세션 종료, focus 복귀, background privacy curtain을 실제 렌더링 상태로 확인한다. 자동화 가능 범위와 수동 확인 범위를 결과에 구분한다.
5. 렌더링 실측: 연속 viewport resize가 한 animation frame으로 합쳐지는지와 canvas가 stage 경계를 넘지 않는지를 브라우저 측정값으로 확인한다.

테스트를 실행하지 않은 항목은 통과했다고 기록하지 않는다.

## 14. 배포 순서

이 전체화면 셸은 기존 Tailscale 경로에서 독립적으로 배포한다. 검증과 코드 리뷰가 끝난 후 Relay 단계는 별도 설계·계획으로 시작한다. Relay 구현이 지연되거나 비활성화돼도 이 뷰어와 기존 Tailscale 접속은 정상 동작해야 한다.
