# PWA 모바일 공간·맞춤 수정 계획 — 2026-10-11

사용자 후속 작업4건을 구현한다. 하단 desktop composer 원인 조사는 완료된 문서/PR 계획으로 유지하며 제품 수정을 하지 않는다. 이번 PWA 작업은 source-only, 앱 재시작·릴리스·커밋·푸시 요청 없음.

1. 공용 mobile theme에서 document 가로 넘침을 차단하고 flex/grid 자식의 최소 폭을 제한한다. 터미널의 내부 가로 스크롤과 pinch zoom은 유지한다.
2. loopback 더보기의 불러온 화면 검색 DOM·이벤트·전용 CSS를 제거한다. workspace 검색과 shared core의 다른 소비자 API는 유지한다.
3. 화면 맞춤을 history 총 높이가 아닌 가로 너비에 맞춘다. 긴 grid는 내부 세로 스크롤. 명시적 휴대폰 크기 제어 grant 후 축소/읽기 모드를 종료하고 live 응답을 기다린 뒤 현재 font의 stage geometry를 전송한다. 최소32열을 넘는 큰 font는 폰 너비에 맞게 조정한다. lease/late reply/읽기 중 자동 resize 방지 계약을 보존한다.
4. loopback/Relay terminal header의 제목·연결 상태를 한 줄로 배치하고 공통 header height45px(+safe area)를 적용한다.44px 터치 버튼은 유지한다.

검증: 실제 production assets를 쓰는 기존 isolated Chromium fixtures 수정/확장.320×568,390×844,844×390/keyboard, 긴 workspace/title, document scrollX0, 검색 DOM 없음, 긴 history width fit+vertical scroll, owned phone fit width, exact lease/geometry/late reply 및 입력/읽기 회귀. shared renderer readability/reading/history·Relay·mobile shell 회귀를 변경 범위에 맞게 실행. 현재 앱/사용자 세션 입력 없음. 실제 iOS 설치형 PWA 실기기 확인은 별도 미검증으로 기록한다.
