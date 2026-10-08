# PWA 터미널 사용성 조사 — 2026-10-08

검토 소스는 `651ac9ffe6d44dacbb85a992b5e4023a5393a40f`다. 목표는 휴대폰에서도 일반 터미널처럼 읽고 입력하고 이동하는 사용성이다. 이번 단계는 원인 조사·프로젝트 비교·개선 방향 제안이며 제품 코드를 변경하지 않았다.

| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| high | `web/shared/viewer-core.js:438` | 원격 전체 그리드를 폭·높이에 맞춰 축소, 글자 최소 크기/설정 없음 | 390px에서 80열도7.99px,180열은3.55px | 고정 가독 크기와 로컬 뷰포트, 전체 맞춤은 별도 옵션 |
| high | `crates/web-remote/src/protocol.rs:30` | 모바일 행·열 요청 계약 없음 | 일반 터미널처럼 폰 크기에 맞게 프로그램 재배치 불가 | 세션별 resize 제어권과 실제 PTY 크기 동기화 |
| medium | `crates/web-remote/assets/app.js:942` | 작성기 전송 중심이며 직접 터미널 입력 경로 없음 | 입력 수정·키 조합·TUI 조작이 일반 터미널과 다름 | 직접 입력 모드, 기존 긴 지시 작성기 유지 |
| medium | `web/shared/viewer-core.js:434` | 글자를 canvas에만 그리며 선택/복사 레이어 없음 | 터미널 출력의 드래그·길게 눌러 복사 불가 | 셀 기반 선택 또는 DOM 접근성/텍스트 레이어 |
| medium | `crates/web-remote/src/dashboard.rs:755` | 모바일 스크롤이 공용 RuntimeCommand::Scroll을 변경 | 폰의 과거 열람이 Mac/다른 뷰의 표시 위치와 연결됨 | 연결별 표시 오프셋과 읽기 전용 history window 요청 |
| medium | `crates/web-remote/assets/app.css:309` | 메뉴가 이동한 visualViewport를 따르지 않음 | 키보드가 올라오면 메뉴가 보이는 영역 밖으로 이동 | 메뉴를 viewer 좌표/키보드 viewport에 배치 |

## 현재 화면을 작게 만드는 정확한 경로

서버가 보내는 것은 ANSI 스트림이 아닌 **Mac 터미널 그리드의 셀 snapshot/delta**다. 코어는 `min(availableWidth / cols, availableHeight / (rows * 2))`로 셀 폭을 계산하고 글자 크기를 `cellHeight * 0.82`로 정한다. 모든 행·열을 동시에 넣으려고 글자를 줄이는 정책이며 휴대폰에서는 읽기 크기와 충돌한다. `.viewer-wrap`은 내용 전체를 중앙 정렬하고 overflow를 숨긴다. 글자 크기·폭 맞춤·읽기 줄바꿈 메뉴가 로컬 쓰기 PWA에는 없다.

최신 제품 HTML/CSS/JS 그대로, 가짜180열×40행 프레임으로 격리 Chromium에서 재현했다. 실제 Mac 앱, 사용자 세션, 폰을 조작하지 않았다.

| 브라우저 표시 크기 | 실제 글자 크기 | 그린 내용 크기 |
| --- | --- | --- |
| 390×844 | 3.55px | 390×173.33px |
| 844×390 | 6.25px | 686.25×305px |
| 320×568 | 2.92px | 320×142.22px |

390×844의 터미널 stage는759px인데 canvas는173px만 사용한다. 추가 열 수 실험도80열7.99px/120열5.33px/180열3.55px였다. 일반적인80열도 휴대폰에서는 너무 작아진다. JS 예외는0건. 키보드 모의 상황(top160,height360)에서 viewer는160..520에 맞지만 메뉴는46..160에 남는다.

[현재 화면 재현 PNG](pwa-terminal-readability-2026-10-08-assets/current-180-cols.png), [정량 결과](pwa-terminal-readability-2026-10-08-assets/metrics.json). 합성 데이터의 실제 제품 렌더링이며 실행 중 앱이나 실기기 화면이 아니다. 스크립트 `node /private/tmp/deppy-mobile-pwa-readability-20261008/audit.mjs`와 추가 `probe.mjs` 모두 exit0. 같은 조사 중 읽기 전용 Relay의14px/fitfalse/wraptrue 설정 적용·저장·복원도 확인했으나 그 화면의 입력 지원을 확인한 것으로 해석하지 않는다.

## 참고 프로젝트 비교

| 참고 대상 | 확인한 방식 | 가져올 부분 | 적용 판단 |
| --- | --- | --- | --- |
| 로컬 deppy-mux PWA | DOM grid, 기본15px/10–24px, 글자·폭 맞춤·읽기 줄바꿈 저장 | 설정/키보드 viewport/모바일 화면 구성 | 설정 부품 재사용에 적합; fit은 최저4px까지 축소하므로 기본 정책 재설계 |
| 현재 Deppy 읽기 전용 Relay | DOM run/행, 글자·fit·wrap 설정과 저장 | 셀 텍스트 표시·선택 가능성·설정 분리 | 빠른 내부 재사용 후보; 쓰기 PWA 입력과 별개이고 CJK/owner-cell 정렬 검증 필요 |
| xterm.js + ttyd | 측정한 셀 크기로 행·열 계산, resize를 서버로 전달, 키 입력과 선택을 터미널에 연결 | 터미널의 크기·입력·복사 계약 | 일반 웹 터미널 사용성의 가장 직접적인 참고 |
| Blink | 글자 크기 핀치, 텍스트 선택·복사, Ctrl/Alt SmartKeys | 폰의 제스처와 보조 키 조작 | 모바일 UX 참고; PWA에 바로 넣는 컴포넌트는 아님 |
| Warp 현재 공개 코드 | Rust terminal/UI 구조, WASM 번들 서빙 경로 | 데스크톱/웹에서 터미널 의미를 공유하는 구조 연구 | 장기 후보; 실제 모바일 PWA 입력/성능은 이번 조사에서 검증하지 않음 |

로컬 deppy-mux는 `web/app/[locale]/w/[slug]/MobileTerminalViewport.tsx`와 `web-access-session-client.tsx`를 읽었다. 폭 맞춤과 읽기 줄바꿈이 기본true이며, fit은 fontSize와 viewport/columns 중 작은 쪽을 취한다. **그대로 복제하면 가독성 문제를 완전히 해결하지 못한다.** 읽기 줄바꿈에서는 원래 터미널의 셀 좌표·커서 위치와 표시 행이 달라지므로 TUI 조작 기본 모드로 쓰지 않는 것이 적절하다.

공식 자료에서 xterm FitAddon은 글자를 축소하지 않고 측정한 셀 크기와 컨테이너로 rows/cols를 산출해 terminal.resize를 호출한다. [FitAddon 구현](https://github.com/xtermjs/xterm.js/blob/master/addons/addon-fit/src/FitAddon.ts)

ttyd는 FitAddon, 입력 전달, 서버 resize 요청, 선택 복사 이벤트를 연결한다. [ttyd 실제 구현](https://github.com/tsl0922/ttyd/blob/main/html/src/components/terminal/xterm/index.ts), [xterm API](https://xtermjs.org/docs/api/terminal/classes/terminal/)

Blink의 공식 사용 안내는 글자 크기 핀치, 선택 복사, Ctrl/Alt 보조 키를 설명한다. [Blink](https://github.com/blinksh/blink#using-blink)

현재 Warp README는 클라이언트 코드 공개 및 웹 컴파일 터미널을 명시하며, 공개 저장소의 WASM 번들 서빙 코드를 확인했다. 과거 closed-source 가정을 사용하지 않았다. 모바일 기능이 즉시 재사용 가능하다는 결론은 내리지 않았다. [Warp](https://github.com/warpdotdev/warp), [WASM bundle serving](https://github.com/warpdotdev/warp/blob/master/crates/serve-wasm/src/main.rs)

## 권장 방향과 순서

1. **읽을 수 있는 크기를 먼저 보장한다.** 기본15px, 제안 조절 범위12–24px, 실제 폰트의 셀 크기 측정. 전체 맞춤은 사용자가 선택하는 축소 개요로 둔다. 기본 grid에서는 원래 열/커서/TUI 형태를 유지하면서 좌상단 기준 가로·세로 이동을 제공한다. 자동 따라가기는 현재 입력/최신 출력에만 적용하고 과거 열람 중 위치를 빼앗지 않는다. 글자 설정은 기기에 저장하고 세션 전환 시 로컬 위치·초안은 해당 세션에 유지한다.
2. **일반 터미널의 상호작용을 연결한다.** 직접 입력 모드에서 확정된 문자·IME 조합과 Enter/Backspace/방향키/Tab/Esc/수정키를 해당 세션으로 전송한다. 현재 send_key는 Ctrl-C/D,Enter,Esc,Tab/Shift-Tab,방향키만 고정 시퀀스로 지원한다. 단순히 xterm onData를 기존 Input에 연결하면 C0 제거 때문에 제어 키가 사라진다. 입력 계약 및 application cursor/mouse mode를 함께 설계한다. 긴 에이전트 지시 작성기는 유지하되 입력 모드는 명확하게 구분한다. 선택·복사·검색, 누르기 쉬운 Ctrl/Alt/방향키 보조 행을 제공한다.
3. **모바일 화면 크기에 실제 터미널을 맞춘다.** 제어 중에는 고정 글자 크기와 사용 가능한 폭/높이에서 rows/cols를 계산해 PTY에 전달한다. 키보드·회전 변화는 settle/debounce하고 크기 변경 후 keyframe을 받는다. 같은 PTY는 하나의 행·열 크기만 갖기 때문에 Mac과 폰이 각각 resize를 보내면 충돌한다. 세션별 크기 제어권이 필요하며, 제어권이 없을 때는 원격 크기를 유지한 읽기/이동 모드로 표시한다. 실제 resize 계약을 추가하지 않고 CSS 줄바꿈만 적용하는 것은 프로그램 재배치가 아니다.
4. **읽기 모드를 별도 제공한다.** 로그·긴 에이전트 응답은 텍스트 줄바꿈과 선택/검색을 지원한다. Vim, htop, Codex/Claude TUI 등의 커서·박스 좌표는 grid 모드에서 보존한다. 폰과 Mac의 과거 열람 위치도 분리하려면 서버의 offset 기반 history window 읽기 계약이 필요하다. 현재 snapshot을 xterm에 넣는 것만으로 원본 ANSI 이력과 터미널 모드가 복원되지는 않으므로 전면 교체는 별도 adapter/stream 설계를 검증한 뒤 판단한다.

즉시 후보는 현재 renderer/프로토콜을 유지한 고정 가독 크기·이동·설정·키보드 메뉴 위치 개선이다. 사용성의 최종 목표를 달성하려면 직접 입력 및 제어권이 있는 실제 rows/cols 동기화까지 포함한다. 이 순서는 제안이며 구현 완료를 의미하지 않는다.

## 후속 구현의 검증 기준

- 320/390/430px 세로, 가로 및 키보드 열린 화면에서 일반 모드 글자 크기는 선택 값 이하로 자동 축소되지 않는다.
- ANSI 색/강조, 한글 경로·조합 문자열·이모지 owner-cell, 커서 shape, 박스/TUI 정렬이 native 화면의 의미와 일치한다.
- 입력한 문자는 직접 모드에서 즉시 해당 세션에 반영되고, 한글 조합·붙여넣기·Ctrl/Alt·외장 키보드가 중복 전송되지 않는다.
- 읽기 모드 선택 복사와 과거 열람이 정상이고, 새 출력은 사용자가 읽는 위치를 바꾸지 않는다. 모바일 스크롤이 Mac의 표시 위치를 이동시키지 않는다.
- 제어권 획득·회수, Mac 동시 조작, 화면 회전·키보드 열림, 재연결의 resize/keyframe 처리가 내용 삭제나 resize 반복을 만들지 않는다.
- iPhone 설치 PWA/Safari와 Android 설치 PWA/Chrome 실기기에서 검증한다. 현재 조사는 격리 Chromium 및 모의 visualViewport만 검증했다.

제품 수정·배포·재실행·커밋/푸시는 이번 조사에 포함하지 않았다. 변경 파일은 이 보고서, 재현 이미지/정량 자료 및 handoff다. Rust/Web 전체 테스트는 재실행하지 않았으며 과거 통과 결과를 이번 조사의 새 테스트로 주장하지 않는다.


## 후속 구현 순서와 병렬 담당 — 2026-10-08

사용자의 후속 지시로 이 보고서의 권장 순서를 실제 개발 순서로 채택했다. 아래는 구현 추적이며, 위 조사 당시 결과와 구분한다. [세부 실행 계획](../superpowers/plans/2026-10-08-pwa-terminal-usability.md).

| PR 단위 | 우선 개선 | 병렬 담당 | 완료 조건 | 현재 상태 |
| --- | --- | --- | --- | --- |
| 1 | 고정15px/12–24px 설정, 로컬 grid 이동, 명시적 전체 맞춤, 키보드 메뉴 위치 | renderer + input_ui | 원격180열/좁은 화면에서 글자 크기와 Unicode 좌표, viewport 메뉴 검증 | 검증·리뷰 완료 |
| 2 | 직접 입력/IME/붙여넣기/제어 키 | input_ui + backend | 정확한 세션과 실시간 터미널 모드, 중복/재연결 입력 방지 | backend RED 확인, 순서 대기 |
| 3 | 단일 resize 제어권과 실제 PTY 행·열 동기화 | backend + input_ui | 경쟁 소켓/네이티브 resize 복원/회전·키보드·연결 정리 검증 | PR2 이후 |
| 4 | 선택·복사·검색/별도 읽기 모드, 연결별 이력 위치 | renderer + backend + input_ui | 네이티브 스크롤 불변, 새 출력 시 읽기 위치 유지 | PR3 이후 |

각 담당자는 독점 파일만 수정하며 root가 실제 코드 리뷰·검증 후 PR 단위로 커밋한다. 현재 요청에는 앱 재실행과 배포가 포함되지 않는다. 실기기 iPhone/Android 검증은 자동 Chromium 검증과 구분해 기록한다.

### PR1 검증과 리뷰

- 실제 격리 Chromium: renderer113 assertions, UI34 assertions, 기존 core/grapheme 및 신규 renderer/UI Rust browser wrapper 각각1PASS. 브라우저 러너 조기 종료·실행 실패 회귀2PASS. 실기기 결과는 아니다.
- 직접 CLI 리뷰 gpt-6.1-sol/xhigh에서3MEDIUM(폰트 축소 clamp 시 따라가기 해제, wide cursor 일부 잘림, 러너 종료 대기)이 발견됐다. 모두 RED/GREEN으로 수정했고 집중 재리뷰 `CONCLUSION: OK`를 확인했다.
- 15px 고정 grid/12–24px 설정, 명시적 축소 개요, 세션별 로컬 이동·따라가기, Unicode owner-cell/커서 모양, 키보드 viewport 메뉴와 현재 화면 복귀를 적용했다. PR2 서버 변경은 PR1에 포함하지 않는다.
