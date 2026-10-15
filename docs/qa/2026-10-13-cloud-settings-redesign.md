# 에이전트 연결 설정 시안 구현 검토 — 2026-10-13

승인 HTML: [시안](../mockups/cloud-agent-settings-redesign-2026-10-13.html). 실제 native 화면 18개와 치수·검사 결과: [HTML 검토 화면](cloud-settings-redesign-2026-10-13.html).

Canonical checkout `/Users/jr/Desktop/projects/deppy-sijo-performance`, baseline `70380aea`, source commit `2fd73a5b3c98a17e43a0e5ecfd6839e76db81c3a` (주 구현 `51b23890` 포함). 원래 dirty checkout와 기존 active-work-visibility HTML은 보존했습니다.

## 구현 대조

| 영역 | 승인 시안 | 실제 적용·검증 |
|---|---|---|
| 정보 구조 | 연결 / 접근 권한 / 작업·답변 / 고급 설정 | 기존 단일 긴 페이지를 4개 탭으로 분리, 목록·상세·세션 권한·표준 설정 행 |
| 고정 shell | header80 / tabs43 / footer35 | 본문이 자체 ScrollArea 소유, outer settings scroll 제외, header 위치 80px 실측 |
| 공통 control | 높이34 / 간격8 | 공유 설정 metrics, TextEdit·button 34px 및 권한 Checkbox 34×34 실제 response/hit area 검증 |
| 목록·행 | 최소68 | 연결·작업 row68 실측, 긴 설정 설명은 HTML min-height처럼 필요한 만큼 확장 |
| 본문 | 좌우26 / 위18 / 아래26 | 26px row 좌측 실측, 설정 typography 15/14/13 재사용 |
| 색상 | HTML dark/light palette | 본문·패널·hover·selected·line·text·secondary·accent·good·warn 동일 RGB |
| list/detail | 연결 1:1.22 / 작업1.15:1 | 동일 비율, 좁은 본문에서는 목록/상세를 세로 배치 |
| 작업 상세 | 보고 / 로컬 / 원래 세션 / 진행률 / actions | 세 가지 facts, 3px progress 실측, 취소·원본 이동·복사 유지, raw reference는 disclosure |
| 인증 상태 | 공개주소·인증·도구 성공 구분 | 수동 unverified / 자동 공개 확인 / credential-issued / observed auth / initialized / tool-success 분리 |
| 권한 | 연결별 read·input·실제 lease | 읽기 해제는 input/lease 회수, input 허용은 lease 자동 획득 안 함, actor 회수는 잠재 consent도 제거 |
| 고급 | 기존 기능 보존 | 주소 방식/포트/재연결, profile save/load/delete/name, start/stop, legacy copy/rotate/history/instructions |
| 반응형·상태 | dark/light/narrow 및 empty/approval/expired | 1090·710×730의 12개 기본 화면, 추가 최초·확인 중·실패·만료·만료 권한·실제 OAuth 승인 6개 |

## 실제 테스트 결과

| 검사 | 결과 |
|---|---|
| App CloudAgent 전체 | **161 PASS / 3 기존 ignored**, 실패 0 |
| 설정 공통 테스트 | **37 PASS**, 실패 0 |
| i18n 5개 locale | **8 PASS**, 실패 0 / doc tests0 |
| 총계 | **206 PASS / 3 ignored**; UI44는 CloudAgent161에 포함, 중복 합산 안 함 |
| App+i18n strict clippy all-targets -D warnings | exit0 |
| cargo fmt --all --check | exit0 |
| xtask check-boundary | exit0 |
| git diff --check | exit0 |
| GPT-6.1-Sol xhigh 독립 CLI 코드 리뷰 | exit0, 최신 소스에 남은 actionable defect 없음 |

정확한 실행 명령과 원본 결과: [evidence](evidence/2026-10-13-cloud-settings-redesign/README.md). 실제 loopback OAuth, SQLite/history, queued real PTY 회귀 포함. 사용자 제공 계정의 외부 에이전트 로그인/공개 tunnel/실제 OS sleep/실앱 상호작용 검사는 이 작업에서 실행하지 않았습니다.

## 리뷰로 수정한 문제

1. 긴 작업·답변 상세의 420px child clip이 부모 스크롤을 따라 움직여 끝의 버튼이 계속 잘림 → 부모 viewport의 y clip 유지. 실제 RED5/3 중 해당1 → 최종 GREEN.
2. 검색이 남은 상태에서 알림이 연 답변은 첫 프레임만 보이고 다음 프레임 사라짐 → 알림을 소비하기 전 기존 work 검색 해제, 일회성 선택 유지. 원본 routing key 불변.
3. 전역 입력 ceiling이 닫힌 상태에서 actor 입력 회수는 effective-input=false 때문에 raw consent를 지우지 않음 → raw actor consent를 검사하여 해당 actor만 회수. 다른 actor 입력 허용 후에도 복원되지 않음.
4. 답변·수신증의 검색 안내는 연결 이름을 지원한다고 했지만 실제 filter에는 없음 → 현재 레지스트리의 redacted 연결 이름을 함께 검색·표시. 실제 RED0/1 → 최종 GREEN.

추가 발견·수정: egui0.36.1 TextEdit.min_size.y 무시 → add_sized로 frame/hitarea34 실제 확인; custom arrow와 egui geometric focus 중복 → 키 소비 및 기본 이동 해제; 좁은 설정 설명의 마지막 줄 잘림 → 68px 최소 높이 + 실제 galley 높이; 새 manual connection sorted-last가 legacy를 선택 → 발급 전후 ID差로 새 연결 정확 선택.

역사적 compile 실패(WidgetInfo API/NodeT import/temporary borrow)와 fixture 실수는 행동 RED로 세지 않습니다. 최종 compile·검사·render는 모두 성공했습니다. 독립 리뷰 이후 권한 열80px/34px hit area·세로 중앙 정렬만 직접 추가 대조·수정하고 전체 CloudAgent 및 strict lint를 재실행했습니다.

## 검증 범위와 릴리스

이 결과는 승인 시안의 제품 본문 구조·색상·논리 px 치수에 대한 대조입니다. HTML 보고서 바, 시연 sidebar/OS chrome, 예시 provider/진행률/인증 시간은 실제 데이터·native host와 다릅니다. 브라우저와 egui의 글꼴 래스터까지 픽셀100% 동등하다는 증거는 만들지 않았고 그렇게 주장하지 않습니다.

source/test 버전 **0.12.1**. 이번 작업은 소스·검증 문서이며 사용자용 app artifact를 만들거나 배포·재실행하지 않았습니다. 다음 사용자용 release는 마지막 shipped version보다 높은 버전과 실제 macOS bundle 버전 검증이 필요합니다. 현재 task에 재실행 요청이 없으므로 app 종료/launch, dev-run.sh, push를 실행하지 않았습니다.
