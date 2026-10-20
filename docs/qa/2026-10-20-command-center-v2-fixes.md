# Command Center v2 병렬 수정 결과 — 2026-10-20

GPT-6.1-Sol/xhigh 세 명이 실행·복구, 네이티브, PWA/Relay를 격리된 worktree에서 병렬 수정했다. root가 실제 승인 API를 연결하고 각 변경을 검토·통합했다. 별도 브랜치 `feat/agent-command-center-v2-20261020`에 단위별 커밋을 유지한다. 기존 실행 앱과 원래 작업 폴더는 건드리지 않았다.

원 리뷰의 확인 결함14건을 수정하고 F01 방어 검사를 보강했다. F01의 “사용자가 완료한 뒤 지연 입력이 전송되는 경합” 재현 주장은 실제 host guard를 누락했으므로 철회했다. 실제 `handoff_pending`이 미해결 로컬 입력 중 완료 처리를 막는다는 회귀 검사를 포함했다. [원 리뷰와 정정](2026-10-20-command-center-v2-review.md).

## 변경과 검증 기준

| 항목 | 수정 결과 | 직접 검증 |
| --- | --- | --- |
| F01 방어 보강 | 지연 콜백·실제 전송 직전 원 task/attempt/config/runtime/route/permit 재확인 | 기존 host guard 차단 + 퇴역 assignment 거절 |
| F02 목표별 복구 | 목표별 토큰·bounded 확인 정의 보존, 재시작·설정 변경 후 현재 정책으로 명시 재확인 | 실제 SQLite roundtrip, 두 목표, 진행·완료 attempt 보존, unknown 자동 재전송 금지 |
| F03 검증 실패 재시도 | host capability에 따라 새 워커 버튼 표시 | enabled exact intent / disabled 없음 |
| F04 설정 snapshot | 텍스트 선택 지원 입력만 선택 범위 복원 | 실제 Chrome 체크박스 포커스·연속 갱신 |
| F05 drawer snapshot | 포커스 소유자·Esc·복귀 버튼 보존 | 양쪽 drawer 갱신 후 Esc와 정확한 항목 포커스 |
| F06 완료 프로젝트 캐시 | 저장 완료·실제 완료·미해결 effect 없는 캐시만 퇴출 | SQLite에 독립 저장된 네 프로젝트·다섯 번째 로드, pending 보호9경로 |
| F07 실제 승인 | 실제 structured session의 명령·폴더·정확한 owner/request로 일회성 허용·거절 | UI/worker identity, scope digest, 회수된 permit, one-write, 혼잡·중복 거절 |
| F08 작업 행 | 번호·제목/범위·담당3열, 기본 행 pitch62px | 실제 PNG 분리선421/483/545px |
| F09 drawer 시작 | 앱 헤더 바로 아래, 닫기를 모니터 헤더에 통합 | desktop/mobile geometry·full-size PNG |
| F10 고정 footer | worker scroll 밖에 기기 관리 영역 예약 | 긴 목록·작은 viewport·snapshot scroll |
| F11 모바일 메뉴 | 다섯 메뉴를390px에 표시 | 양 테마·font1/1.25에서 visible geometry |
| F12 설정 규격 | 실제 제한된 자식 영역, 폭 최대620px·입력36px | desktop/mobile 실제 위젯 rect |
| F13 공유 모니터 | 필터·role/scope·정확한 바로가기·규칙·footer·desktop 접기 | shared + production concatenation + encrypted Relay |
| F14 MAIN/CHECK/LIVE | 실제 승인·주의 작업·최근3개 기록·전체 이력 연결 | 실제 target/result revision, 최근 순서·오래된 attempt 연결 금지 |
| F15 탐색 | 메인/작업/터미널/에이전트·기기/이력, 설정은 프로젝트 문맥 | 정확한 선택 task/main terminal, 없는 identity 설명, dashboard 임의 대체 금지 |

추가로 worker 메타·시각·버튼 양끝 정렬, 기기 목록 다음 TLS 고급 설정, composer의 단일 테두리·왼쪽 대상·오른쪽 전송을 맞췄다. 상호 리뷰에서 배정 수정 뒤 이전 확인 정의가 저장을 막는 결함과 모니터 snapshot scroll 손실도 재현 후 수정했다.

## 실제 승인 지원 범위

현재 설치된 `codex-cli 0.163.0`의 로컬 생성 JSON schema와 실제 parser를 대조했다. 일반 로컬 `command`이며 실제 명령·cwd가 있고, 표시 가능한 정확한 요청만 CHECK에서 승인한다. 원 workspace/session/thread/turn/item, provider callback과 전체 params digest를 묶는다. `writeStdin`, 다른 `environmentId`, 네트워크 승인, 파일 변경, 알 수 없는 새 schema 필드와 일반 PTY는 원래 세션의 승인 화면으로 보낸다. 불완전한 파일 변경 정보를 일반 명령 승인으로 취급하지 않는다.

전송 worker가 현재 provider 상태를 다시 읽고 요청 내용과 실행 턴을 검사한다. bounded incoming drain이 다 차면 아직 읽지 않은 취소가 있을 수 있으므로 응답을 거절한다. 실제 쓰기는 한 번의 작은 atomic nonblocking frame이며 회수 검사 안에서 기다리지 않는다. 연결 혼잡은 재시도 가능한 제어 오류로 반환하고, 요청이나 다른 세션의 수명을 종료하지 않는다. 입력 접수·승인 응답 접수·실행 완료는 별도다.

## 실행한 검사

- 통합 source `36caca2e`: `cargo test --workspace -- --test-threads=1` **6,069 PASS / 0 FAIL / 67 ignored**,79 Cargo target,391.348초. App3,474PASS/40ignored, storage511unit+8integrationPASS. target별 마지막 결과만 집계하여 자식 helper 중복을 제외했다.
- 같은 통합 source의 fmt, boundary, dependency, i18n5locale, strict workspace/all-target Clippy 모두 exit0. 새5nav label은 기존 현지화된 문구에서 alias하므로 locale간 레이블도 유지한다.
- 위 전체 검사의 App binary는 마지막 provider-scope 수정 전이다. 최종 source `d50160a6`의 App 전체 재검사 **unit3,475PASS/40ignored + integration39PASS/3ignored, 총3,514PASS/0FAIL/43ignored**가 통과했다(141.483초). strict App/all-target Clippy와 fmt도 exit0이다. 다른 crate 제품 소스는 그 사이 변하지 않았다. 이전 전체 PASS만으로 최종 App PASS라고 표시하지 않는다.
- scope 수정 전 실제 getter RED0/2 → GREEN3, worker/회수 GREEN5, scope parser·bounded cancellation 포함 최신 focused **6PASS**. 독립 실제 parser RED0/3 → GREEN3, 제어 오류가 lifecycle을 실패로 만들지 않는 별도 검사도 통과했다.
- 통합 shared Chrome **150 assertions PASS**. production Chrome1PASS, encrypted Relay Chrome1PASS, 통합 native render1PASS/120PNG도 통과했다. 새 통합 native selected10PNG는 보존된 이미지와 byte-identical이다. native 소유 트리24PASS/1ignored와 별도120PNG render1PASS도 보존한다. PWA 소유 트리 production16checks·RelayOK 및6width×2theme×2font 행렬 통과. 두 결과를 단일 실기기 E2E로 합산하지 않는다.

검사 실행 manifest: `assets/command-center-v2-fixes-20261020/final-validation.json`. 전체 원시 로그는 `/private/tmp/deppy-cc-{fixes-final,final-scope}-*-20261020.log`다.

초기 runner manifest의 source는 실행 시작 commit이다. native render 도중에는 provider-scope WIP가 있었으므로 전체 App source를 그 옛 commit으로 주장하지 않는다. native8UI hash와 새10PNG 동일성으로 화면 증거를 고정하고, 최종 App 테스트는 별도 동결 commit으로 실행했다. scope hash를 inline으로 추가한 첫 strict Clippy는 event enum 크기를 지적했고, bounded digest를 작은 Box로 보존하여 경고 없이 통과했다.

## 시안 비교 증거

[네이티브10PNG·120geometry·SHA](assets/command-center-v2-fixes-20261020/native/README.md), [PWA/production/Relay14PNG·행렬·SHA](assets/command-center-v2-fixes-20261020/pwa/README.md). root는 native10장을 모두 열어 확인했고 approved/shared desktop·390px dark의4장을 직접 대조했다. [실제 provider scope RED/GREEN](assets/command-center-v2-fixes-20261020/protocol/README.md)도 보존했다. 각 서브에이전트의 전체 contact sheet·반응형 검토도 별도로 기록했다. native8UI source hash, selected10PNG, PWA19artifact hash/bytes를 통합 트리에서 다시 확인했다.

[승인 HTML](../mockups/multi-agent-command-center-v2-2026-10-20.html)·spec·원본3PNG, 총6 SHA는 그대로다. 본 증거는 실제 egui/Chrome의 fixture 렌더다. 사용자 DB·현재 실행 앱·물리 휴대폰·유료 provider의 실사용 캡처나 pixel100%동일성 주장이 아니다. 승인 구조·행 규격·고정 footer·권한과 라우팅을 검증했다.

## 커밋과 복구

- 워크플로: `625eead4`, `21a2b59e`, `5aa5ea49`, `fe2ee110`, `8cffb32e`.
- 네이티브: `3a73ae36`, `45333229`, `8e8a3937`, `4d8ed2ef`.
- PWA: `a95914d8`, `704f832c`.
- 실제 승인/이력·scope: `e753efed`, `d50160a6`; 통합 formatting `36caca2e`.
- 증거: `0e9ff623`, `d0cd4431` 및 protocol evidence `f727ad4e`.

원안 기본값과 `DEPPY_COMMAND_CENTER_V2=1` opt-in, rollback branch `rollback/command-center-pre-v2-20261020`, tag `command-center-pre-v2-20261020` 모두 원점 `f78ebb69c94ec7cb72ae9a1159d2137c0d6a5679`를 유지한다. [복구 안내](../command-center-rollback.md).

앱 시작·중지·재시작, 사용자 DB/provider 호출, 릴리스 package, push/main merge는 수행하지 않았다. source0.13.0은 개발 검사 상태이며 사용자에게 전달한 새 artifact가 없다. 실제 배포에는 최신 shipped보다 높은 버전과 binary/bundle 검증이 필요하다.
