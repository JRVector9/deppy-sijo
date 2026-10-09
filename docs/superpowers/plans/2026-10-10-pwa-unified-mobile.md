# 승인 시안의 PWA 구현 — PR 단위 병렬 계획

기준: `docs/mockups/pwa-mobile-unified-input-2026-10-10.html` 및 승인된 `docs/superpowers/specs/2026-10-10-pwa-unified-mobile-design.md`.
사용자 승인: 2026-10-10, 시안대로100% 구현하고 PR 단위로 계획/서브에이전트 병렬 진행. 추가 스킬 사용 금지.
모든 서브에이전트: `gpt-6.1-sol`, reasoning effort `xhigh`. 다른 모델로 대체하지 않는다.

## 목표와 완료 조건

- 과거 출력 안내 바/비어 있는44px 행 제거. 과거를 읽을 때만 `↓ 맨 아래로` 버튼 표시.
- 저장된 이력이 있으면 휴대폰 가용 세로 높이를 실제 내용으로 채우고 연속으로 이전 출력을 조회한다. native PTY 크기/스크롤/alternate screen을 임의 변경하지 않는다.
- 입력창 하나:44px 한 줄,16px 글자, 최대5줄 후 내부 스크롤, Enter/명시적 전송, Shift+Enter 개행, IME 조합 보호. 특수키/첨부/세션별 초안/ACK 보호 유지.
- 워크스페이스 검색/펼침/세션 전체 폭76px 행/14px 제목/12px 에이전트 상태, 하단 세션 선택/설정 패널을 시안대로 구현.
-320×568/390×844/844×390 및 키보드/입력 확장/세션 전환/재연결 회귀 통과. 루프백/Relay 권한 차이 유지.

## 소유권과 병렬 실행

공유 체크아웃 `/Users/jr/Desktop/projects/deppy-sijo-performance`에서 파일 소유권을 분리한다. 서브에이전트는 자기 영역만 수정하며 다른 담당자의 파일이 필요하면 root에게 제안한다. root만 공통 계획/QA/handoff/git 및 전체 Cargo gate를 관리한다. 서브에이전트는 커밋/푸시/앱 실행/전체 fmt를 하지 않는다.

| PR | 담당 | 파일 범위 | 내용 | 통합 조건 |
|---|---|---|---|---|
| PR1 | backend | Rust terminal/runtime/web-remote 이력·프로토콜 및 해당 Rust 테스트 | 독립 유계 구간 조회와 표시 행 수, live/cursor/identity/trim 보존 | UI와 renderer가 사용할 프레임 계약 확정 |
| PR2 | renderer | web/shared/viewer-core.js, viewer-core.css 및 독립 renderer fixture | 전체 높이/연속 스크롤/유계 이력 연결/하단 이동/읽기 위치 보존 | PR1 프레임 계약 및 PR3 훅과 호환 |
| PR3 | mobile_ui | crates/web-remote/assets/*, web/shared/mobile-theme.css, web/relay-shell/* 및 독립 UI fixture | 통합 입력/모바일 목록/검색/하단 패널/보조 키/세션 초안 | PR1/PR2 확장 API를 shell에 연결 |
| PR4 | root | 공통 러너/기존 fixture 통합/계약 접점/QA/계획/handoff | 독립 코드 리뷰, 교차 회귀·시안 대조, 미해결 사항 수정 | 실제 실행한 테스트와 각 PR 단위 커밋 기록 |

기존 renderer fixture와 UI fixture를 공유 수정하지 않는다. 새 검증은 `pwa-unified-renderer.js`, `pwa-unified-mobile-ui.js` 등 담당별 파일로 작성하고 root가 기존 러너/fixture와 통합한다. 각 소스 PR은 독립 범위로 리뷰하고 커밋하되 PR1→PR2→PR3→PR4 순서로 의존성을 기록한다. 사용자 요청에 없는 공개 PR/푸시/배포는 만들지 않는다.

## PR1 — 실제 이력 구간 확대

1. 기존 native 행 수에 묶인 조회를 더 큰 독립 구간으로 확장한다. 요청 행 수는 선택적이며 기존 요청은 기존 동작을 유지한다.
2. 상한: 요청 표시 구간은 최대256행/65536셀 중 더 작은 한도, 기존 지원 native 프레임도 기존 한도 내에서 보존. 잘못된 범위는 명확히 거절하며 무제한 할당을 하지 않는다.
3. 확대 구간의 최신 상태도 offset0 의미를 유지한다. generation/first_line/total_lines, 실제 native cursor 위치와 프레임 cursor 위치, 조회 anchor/trim/alternate screen을 구분한다. 프로토콜 변경은 필요한 wire version을 증가한다.
4. 프레임 계약을 root/renderer/mobile_ui에게 먼저 보내고 고정한다. live 출력 업데이트에서도 확대 표시가 native 단일 화면으로 축소되지 않게 한다.
5. 실제 terminal+runtime+인증 WebSocket 테스트로 native rows/scroll/damage 불변, 작은 native 화면+큰 모바일 영역, 조회 경계/만료/재연결/출력 중 읽기 보존을 검증한다.

## PR2 — 표시 높이와 연속 이력

1. 시안의 전체 높이 레이아웃과 하단 overlay 버튼을 공용 코어에 적용한다. 숨긴 안내 바를 위해 행을 예약하지 않는다.
2. 가용 높이/셀 높이로 표시 요청 행 수와 앞뒤 여유를 계산한다. 출력이 존재하면 내용으로 채우고 글자/행 간격을 늘려 빈 공간을 감추지 않는다.
3. overlapping history를 identity로 연결하고 스크롤 위치를 보존한다. 캐시는 최대512행/65536셀의 작은 한도로 유지하고 캔버스 픽셀 상한을 지킨다. cache trim 때 사용자가 보는 identity를 먼저 보존한다.
4. native live delta와 history keyframe의 세대/행 수가 다를 때 baseline을 섞지 않는다. 늦은 응답/신규 출력/글자 크기 변경/읽기 줄바꿈/selection/search/keyboard 변경에서도 읽던 행을 유지한다.
5. 이전 출력 조회와 하단 복귀를 shell 훅으로 제공하고 이력 안내/오류는 메뉴용 상태로 보존한다. 기존 Relay 읽기 권한은 확대하지 않는다.

## PR3 — 승인된 모바일 UI와 입력

1. 두 모드 버튼/두 입력 DOM을 제거하고 하나의 buffered 입력창을 사용한다. 기존 전송량/ACK/IME/초안·전송 journal 보호를 유지한다. 특수키는 기존 허용 키 계약을 유지한다.
2. 조합 Enter, 긴 한글/NFD 경로/붙여넣기, 입력 중 세션 종료/전환/재연결·거절, 작성 중 특수키 순서와 전송 후 초안 처리를 검증한다.
3. 워크스페이스는 활성만 기본 펼침, 전체 폭 세션 행과 이름·경로·제목·에이전트 검색, 정확한 상태/비활성 전환 안내를 제공한다. 검색/펼침 상태는 dashboard 갱신 때 유지한다.
4. 헤더의 세션 선택을 목록 패널로 바꾸고 설정을 하단 패널로 재배치한다. keyboard/visualViewport/safe-area·포커스·ESC/배경 닫기·접근성을 검증한다.
5. renderer API와 확정된 PR1 이력 계약을 루프백에 연결하고 Relay에서는 지원 가능한 읽기 기능만 연결한다. root와 접점 파일을 수정하기 전에 조율한다.

## root의 검증과 종료

- 시작/종료 시 model 설정과 사용 가능한 도구의 실제 spawn 결과를 기록한다.
- agent 결과별 diff를 읽고 실행한 검사와 미실행 검사를 구분한다. source-law만으로 UI/기능 통과를 주장하지 않는다.
- Cargo는 root가 직렬 실행한다. 담당자가 요청하는 좁은 테스트를 먼저 실행하고 이후 관련 전체 패키지/Clippy/fmt/boundary/실제 Chromium 통합을 실행한다. 브라우저 자동 테스트는 격리된 테스트 환경만 사용한다.
- 시안과 실제 소스에서 높이/입력/목록/설정/하단 복귀 상태를 대조하고 QA 문서에 결과·제약을 기록한다. 실제 모바일 hardware 미검증은 사실대로 표시한다.
- 이 작업은 소스 구현과 PR 단위 커밋이다. 앱 자동 재시작/배포를 하지 않는다. 변경된 실행 빌드를 사용자에게 전달하는 후속 요청에서는0.9.0보다 높은 버전과 binary/bundle 검증이 필요하다.

## 진행 기록

- 세 서브에이전트 실제 spawn 성공: unified_backend, unified_renderer, unified_mobile_ui, 모두 model=gpt-6.1-sol/reasoning_effort=xhigh/fork_turns=none. 도구에서 지원하는 모델을 지정했고 다른 모델로 대체하지 않았다.
- PR1 계약 승인: Scroll optional rows1..256, frame bounds65536cells/available source rows; latest offset0 is end-distance from live tail. metadata native_rows/live_first_line separates native dimensions/live origin from loaded frame first_line/rows. Anchor preserves top on row change. Alt native-only. Runtime wire26→27. Backend connection-local widened passive frames keep native baseline separate.
- PR2/3 API: historyRowsChanged(rows,metrics), historyNavigate({delta,rows,reason}); getDesiredHistoryRows(); expectHistoryWindow(request,{reason,rows,preservePosition,follow}); getHistoryState adds followingLive/offLatest/desiredRows/cachedRows/historyMessage. Legacy hooks retain compatibility; expanded metadata opts into cache.
- Root added distinct Chromium runner pwa_unified_chrome.rs with success markers PWA_UNIFIED_RENDERER_OK:/PWA_UNIFIED_MOBILE_UI_OK:. Fixture implementations pending; no test passed claim yet.
