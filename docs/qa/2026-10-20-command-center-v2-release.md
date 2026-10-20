# Command Center v2 브랜치 인수 기록

## 범위

승인한 1안 발전형을 `feat/agent-command-center-v2-20261020`에 구현했다. GitHub push/PR 생성/main 병합, 실행 앱 교체, 제품 릴리스는 수행하지 않는다. 이 문서는 사용자용 릴리스 완료 보고서가 아니다.

현재 source version은 기준 브랜치의 `0.13.0`이며 검사 빌드에만 사용한다. 제품 전달 시 마지막 shipped 버전을 다시 확인하여 그보다 높은 기능 버전(잠정 `0.14.0`)으로 올리고 CLI 및 macOS bundle 두 version 값을 검증해야 한다.

## PR별 구현 경계

| PR | 구현 | 검증·지원 경계 |
|---|---|---|
| 1 계약 | project/task/attempt/runtime/operation identity와 reducer | 오래된 시도·중복·역순·동일 문장 신규 입력 격리 |
| 2 저장 | lazy SQLite sidecar3, CAS, outbox, 프로젝트 설정·초안 | 원안 metadata migration 불변, 불확실 실행 자동 재시작 금지 |
| 3 화면 | 승인 3단 구조·drawer·양 테마·글자 배율·원안 전환 | native egui fixture; 실행 중 앱 캡처와 구분 |
| 4 로컬 실행 | 실제 profile 준비·durable claim·idle spawn·binding·guarded input·owned worktree | CLI 접수와 실제 실행·완료는 구분, exact owned close |
| 5 계획 | 검증된 계획·명시 확인·독립 작업·의존 결과 취합 | 지원되는 Codex 관측 자동 연결, 다른 provider는 수동 계획·보고 |
| 6 결과·기록 | 원래 owner 승인·결과 revision·검증·사용자 확인·Git·원문·기록 | 실제 provider capability와 현재 시도만 활성화 |
| 7 기기 | pinned TLS·프로젝트 전용 scoped token·folder/profile/capacity·readonly reconcile | 실제 TLS/PTY/SQLite fixture, 물리 컴퓨터 간 검증은 미실행 |
| 8 모바일 | 같은 project/task projection·정확한 View permit·원격 terminal watch·권한별 기존 입력 | 원격 워커 화면은 읽기 전용; cross-TLS cloud 입력 비활성화 |
| 9 통합 | 격리 브랜치·복구 지점·전체 검사·화면 대조·사용 문서 | 제품 버전/서명/패키징 및 실앱 idle 성능 비교는 별도 출시 단계 |

## 시안 대조 기준

| 요소 | 적용·검사 |
|---|---|
| 넓은 화면 | 상단48 / 프로젝트 머리글84 / 좌측220 / 우측312 논리 단위 |
| 중간 화면 | 좌측196 / 우측274, 1050 이하 우측 drawer |
| 좁은 화면 | 760 이하 좌측 drawer·우측 drawer 상호 배타, 단일 입력창 |
| 글자 배율 | 1.0/1.25 fixture, 760의 설정 버튼 잘림을 실제 재현 후 controls-first 배치로 수정 |
| 입력 | 비포커스 축소·IME/NFC·초안 분리·접수 확인과 실행 관측 구분 |
| 원격 화면 | 내부 스크롤·정확한 route별 위치·숨김 중 갱신 중단·lost read ACK 복구 |
| 기존 화면 | 기본값 유지, 명시 opt-in과 원안 복귀·재진입, 작업자 임의 종료 없음 |

승인 HTML/spec/PNG hash와 rollback branch/tag가 원본 `f78ebb69`에 일치하는 것을 실제 확인했다. HTML과 native egui는 서로 다른 renderer이며 pixel 단위 동일성을 주장하지 않는다. 가짜 성공·예시 작업을 production snapshot에 넣지 않는다.

## 최종 검사

최종 source freeze 후 실행한 결과를 여기에 갱신한다. 현재 전체 gate의 최종 결과는 아직 미기록이다. 최신 실행 기록은 [구현 검증](2026-10-20-command-center-v2-implementation.md)과 `docs/CODEX_HANDOFF.md`를 기준으로 한다.

## 배포 전 별도 확인

실제 유료 provider 실행, 사용자 데이터로 앱 재시작/복구, 물리 기기 간 연결, 동일 조건 main 대비 장시간 idle CPU·retained memory·frame p95 비교는 이번 자동 harness 검증에 포함하지 않았다. 제품 artifact를 사용자에게 배포할 때는 버전 증가·서명·bundle 검증을 함께 수행한다.
