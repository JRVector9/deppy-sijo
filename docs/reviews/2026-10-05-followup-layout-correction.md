# 예약 창 배치와 세션 탭 높이 보정 — 0.7.1

사용자가 제공한0.7.0 실행 화면을 기준으로 변경한다.

- 대상 레이블/세션명8pt 간격, 세션명 강조색. 실제 모델명·예약 작업의 추론 강도 선택은 우측 공용36pt 드롭다운으로 통합한다.480pt 미만 본문은 두 줄로 배치한다. 모델 자체의 변경 메뉴는 아니다.
- 프롬프트는 빈 본문과 한 줄 본문에서도 최소3줄이며, 기존136pt 최소 높이와 크기 조절을 실제 행 수로 반영한다.16KiB 상한·undo8·원래 대상 예약·설정 확인 후 전송 정책은 유지한다.
- 세션·문서 탭 헤더29→27pt. 전체 제목바는 기존36pt다.

## 지난 작업에서 누락된 원인

이전8ebcf3b7은 전체 TOP_BAR_HEIGHT38→36만 변경했고 세션 탭 높이 상수는29였다. 화면2의 세션 탭 요청은 이번에 별도로 적용한다. 이전462c4af5의 WindowEditor는 desired_rows1과 min_size.y 조합이었지만 egui0.35 TextEdit AtomLayout은 세로 min_size를 적용하지 않아 실제 높이가32pt였다. 글자 색/간격도 부모 가로 간격0을 상속했다. 기존 리뷰/테스트는 입력 상한·상호작용을 검증했지만 이 실측 높이와 간격을 검사하지 않았다.

이미 실행 중인0.7.0의 네이티브 버전·실행 경로와 최근 제품 소스의 빌드/버전 증거를 확인했다. 오래된 앱이 실행된 문제가 아니다. 이전 메모 메뉴/더블클릭 선택, 제목바36pt, 중앙 창을 사용하는 Fleet/라이브러리/변경사항/세션 관리/MCP 호출 경로는 소스에 존재한다. 이번 전체 App/UI 회귀 게이트로 관련 기능을 다시 검증한다. 보고서는 확인한 범위만 다루며 실제 사용자 AI CLI 작업을 자동 전송하지 않는다.

## 실제 검증

- 최초 실제 RED:4개 실패 (editor32pt, 모델을 포함한 드롭다운 없음, 대상 간격0, 헤더29pt). /tmp/deppy-followup-layout-red-20261005.log.
- 최초 GREEN4passed. 확장 geometry6passed 및 offscreen 한국어 넓은/좁은PNG 생성 검증1passed. 시각 점검에서 드롭다운 우측 끝 미정렬을 추가로 확인하고 강한 실제 geometry assertion으로 RED 재현. /tmp/deppy-followup-right-edge-red-20261005.log. 좌측 컬럼 최소 너비를 예약해 정렬을 고쳤다.
- 한국어 PNG: /tmp/deppy-followup-layout-wide-20261005.png, /tmp/deppy-followup-layout-narrow-20261005.png. 실제 egui 렌더링이고 HTML 대체 화면이 아니다.
- 두 HTML 시안의 JavaScript node --check passed. git diff --check passed.
- 최종 게이트 exit0: 실제 geometry6passed, 한국어 시각 렌더1passed, 전체 App/Connector/i18n 및 App integration **2809passed/0failed/34ignored**. Strict affected all-target Clippy -D warnings, UI capability boundary,27-crate dependency check, fmt 및 git diff --check 모두 통과. /tmp/deppy-followup-layout-final-gates-20261005.log.
- 최종 수동 diff 검토: 지원/비지원·pending 차단·원래 대상 intent·오류 보존 경로는 그대로이며, 실제 우측 끝 정렬과 좁은 화면 선택/푸터 hit-test가 통과했다. 확인한 이전 적용 범위에서 추가 누락은 발견하지 않았다.

## 버전/커밋/실행

0.7.0→0.7.1, canonical workspace와27 inherited lock entries만 변경한다. 최종 커밋/패키지/실행 증거는 완료 후 기록한다. 로컬 개발 Developer ID 서명 정책이며 공증/공개 배포는 요청 범위에 포함되지 않는다.
