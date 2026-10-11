# PWA 모바일 공간·맞춤 수정 검증 — 2026-10-11

Canonical source: `/Users/jr/Desktop/projects/deppy-sijo-performance`, base `799fae89` + working tree. Source-only. 앱 재시작·릴리스·커밋·푸시 없음. 다른 Claude 작업의 desktop history/resize 수정은 보존.

## 구현

- shared theme에 document 가로 clip/overscroll 차단과 shell 최소 폭 제한. 터미널 내부의 가로 pan/pinch는 유지.
- loopback 더보기의 「불러온 화면 검색」 DOM/이벤트/CSS 제거. 워크스페이스 검색은 유지. shared renderer 검색 API는 다른 소비자용으로 유지.
- 가로 너비 맞춤을 native 열 너비로만 계산. 긴 이력은 내부 세로 스크롤하며 위치 anchor/따라가기/이력 요청도 동작. 실제 scaled glyph 높이로 보이는 행+여유분을 요청하며256행/65536셀 상한 유지.
- 명시적 휴대폰 맞춤의 유효 grant만 overview/readable mode를 종료하고 live 화면을 회복. 큰 font 때문에 최소32열이 폰보다 넓으면 표시 설정을 맞는 크기로 낮춘다(하한12px). 갱신/거절/오래된 grant와 잘못된session은 이 동작을 실행하지 않는다. 소유권/복원/자동resize 중단 계약 유지.
- loopback/Relay terminal 제목·연결상태를 한줄에 배치. 공통 높이45px+safe area,44px touch control 유지.

## 실행한 검사

production assets를 isolated HTTP server와 실제 Chromium에서 실행. 사용자 앱/PTY에 입력하지 않음.

| Target | Result |
| --- | --- |
| pwa_unified_chrome | 2PASS |
| mobile_usability_chrome | 1PASS |
| viewer_readability_chrome | 1PASS |
| mobile_shell_chrome | 1PASS |
| relay_shell_chrome | 1PASS |
| viewer_core_chrome | 1PASS |
| viewer_reading_chrome | 1PASS |
| viewer_history_chrome | 1PASS |

합계9PASS/0FAIL. `pwa_unified_chrome` 최종source는 `/private/tmp/deppy-pwa-mobile-layout-final-20261011.log`의2PASS, 나머지7PASS는 `/private/tmp/deppy-pwa-mobile-layout-regression-20261011.log`(전체 batch exit0).

320×568/390×844/844×390 및 keyboard viewport, 긴 workspace/title, document scrollX0, 검색 DOM 제거,200행 이력 가로폭/세로 overflow,24px에서 phonefit/최소32열/1셀 이내 가로 여백, 세션별 초안·IME·재연결·입력 거절 보존, 정확한lease UUID/request 및 lateACK/sequence exhaustion, retained history/selection/copy/앵커/새출력, Relay 한줄45px header를 확인.

실행 명령(cwd performance):

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["test","-p","web-remote","--test","pwa_unified_chrome","--","--ignored","--test-threads=1"]]'
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["test","-p","web-remote","--test","mobile_usability_chrome","--","--ignored","--test-threads=1"],["test","-p","web-remote","--test","viewer_readability_chrome","--test","mobile_shell_chrome","--test","relay_shell_chrome","--test","viewer_core_chrome","--test","viewer_reading_chrome","--test","viewer_history_chrome","--","--ignored","--test-threads=1"]]'
node --check crates/web-remote/assets/app.js
node --check web/shared/viewer-core.js
node --check web/relay-shell/relay-shell.js
git diff --check
```

위Node syntax 및diff check도 exit0. 변경은 assets/fixtures만이므로 Rust full suite는 실행하지 않았다.

## 실패 후 확인

첫mobile usability는 tiny-stage의 캔버스가0높이일때 옛 font를 측정하는 oracle와 새configured font가 달라32vs33열 FAIL. oracle를 현재font 직접 측정으로 교정. 이후 overview 종료 paint를 기다리지 않아 새 행수의delta0 layout request를 상대scroll5와 함께 세는 fixture FAIL. paint settle 뒤 요청을 시작하도록 수정했고 기존 상관관계/정확한전송횟수 assertion은 유지. readability 테스트에는200행 source를 명시해 짧은40행의 높이를 억지로 길다고 가정하지 않았다.

실제 iOS Safari/설치형 PWA 실기기 검증 및 실행 중0.10.1 반영은 하지 않았다. 변경 artifact를 전달할 때 버전은 반드시0.10.1보다 증가해야 하며 앱 재시작은 당시 명시 요청 필요.
