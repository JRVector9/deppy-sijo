# 모바일 PWA 화면·설정 점검 — 2026-10-07

검토 소스: `4a18a843`(제품 소스 `c4c1807d`, 0.8.6). 실행 중 Mac 앱은 0.8.3이며 이번 점검에서 재시작하지 않았다. 비교 대상은 `/Users/jr/Desktop/projects/deppy-mux`의 모바일 연결 화면이다.

| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| high | `crates/web-remote/assets/index.html`, `web/shared/viewer-core.js:438` | 로컬 쓰기 PWA에 글자 크기·폭 맞춤·읽기 줄바꿈 설정 없음 | 390px 화면의 180열 터미널 글자가 3.55px로 줄어 읽기 어려움 | 표시 설정과 영속 저장 구현, 실제 terminal viewport로 검증 |
| medium | `crates/web-remote/assets/app.css:309` | 터미널 메뉴가 visualViewport 이동을 따르지 않음 | 키보드 표시 후 메뉴가 보이는 화면 위쪽으로 벗어남 | 이동한 viewer 안에 메뉴 배치, 키보드 오프셋 회귀 검증 |
| medium | `crates/web-remote/assets/app.js:701` | 빈 작성기의 ↵ 버튼은 아무 입력도 보내지 않음 | 터미널에서 Enter만 보내는 deppy-mux 동작과 다름 | 빈 입력일 때 Enter 전송 구현 |
| medium | `crates/web-remote/assets/app.js:121` | `/` 버튼에 명령 선택 대신 특수키 표시 | deppy-mux의 /model·/review 등 명령 선택 기능 누락 | 명령 선택 UI와 전송 동작 구현 |
| low | `crates/web-remote/assets/app.css:122` | 세션 칩이 세로 전체 폭 행으로 배치됨 | 기준 화면의 가로 칩 배치와 다름 | ws-sessions flex 방향 수정 |

## 실제 검증

- 현재 제품 HTML/CSS/JS 및 공용 canvas 렌더러를 임시 로컬 HTTP 서버에 그대로 올려 격리된 headless Chromium에서 점검했다. 실제 Deppy 앱 및 사용자 세션에 입력하지 않았다.
- 390×844, 844×390, 320×568 크기에서 레이아웃을 측정했다. 180열×40행 terminal viewport를 수신했을 때 글자 크기는 각각 3.55px, 6.25px, 2.92px였다. 세로 화면 canvas는 390×173px로 줄어 세로 중앙에 놓인다.
- visualViewport offsetTop=160, height=360을 주입한 키보드 상황에서 viewer는 y=160..520, 작성기는 y=484..514로 올바르게 이동했다. 메뉴는 y=46..160으로 남았다. 이는 Chromium의 모의 키보드 viewport 검증이며 실기기 Safari 검증은 아니다.
- 워크스페이스 진입, 제목, 세션 전환, 세션별 초안 복원은 동작했다. 브라우저 JavaScript 예외는 없었다.
- 별도 보기 전용 Relay 화면은 글자 크기·폭 맞춤·읽기 줄바꿈 설정이 실제로 동작했다. 설정을 14px/fitWidth=false/readableWrap=true로 바꾸자 렌더와 localStorage가 갱신됐고 새 shell 인스턴스에도 복원됐다. Relay의 입력 비활성은 의도된 정책이다.
- `cargo test --offline --locked -q -p web-remote --test mobile_shell_chrome -- --ignored --nocapture` 및 `relay_shell_chrome`을 실제 실행: 각각 1 PASS. 기존 로컬 화면 fixture는 viewport 데이터를 보내지 않아 설정 누락과 글자 축소를 검증하지 못했다. 기존 테스트 통과만으로 전체 기능 동등성을 주장할 수 없다.

## 증거와 한계

측정 결과 및 스크린샷: `/private/tmp/deppy-mobile-pwa-audit-20261007/standalone/results.json`, 같은 폴더의 `01-workspaces-390x844.png`부터 `10-relay-updated-settings.png`. 재현 스크립트: `/private/tmp/deppy-mobile-pwa-audit-20261007/audit.mjs`.

이번 단계는 보고 목적의 점검으로 위 PWA 제품 코드 수정은 하지 않았다. 실기기 iPhone/Android 설치 PWA, 실제 소프트 키보드, 실제 네트워크 원격 연결은 미검증이다. 후속 사용자 요청에 따라 Mac 하단 Composer 높이 증가 시 터미널 내용 손실 문제를 이어서 조사한다.
