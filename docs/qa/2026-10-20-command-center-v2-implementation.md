# Command Center v2 구현 검증

## 최신 통합 checkpoint

이 checkpoint는 아래 초기 leaf 검사 기록보다 우선한다. 통합 브랜치 `feat/agent-command-center-v2-20261020`에서 PR별 커밋을 검토·통합 중이다. 전체 완료·최종 workspace 검사 통과·제품 릴리스로 보고하지 않는다. 기본 화면은 원안이고 `DEPPY_COMMAND_CENTER_V2=1` opt-in이며 rollback source/tag를 유지한다.

| 영역 | 현재 실제 동작 | 최신 실제 증거 | 남은 통합 |
|---|---|---|---|
| 로컬 실행 | 저장된 launch claim→실제 profile 준비→idle agent 생성→정확한 binding→outbox 저장→guarded 입력 | App focused host/관측/plan 연결 통과, 후속63 PASS/1 ignored | 전체 source gate |
| 관측·결과 | Codex exact session/turn/accepted sequence/final response 연결, 미관측 provider 수동 보고 | 오래된 Capture·attempt 격리 회귀, lateRejected-after-Admitted RED→GREEN | 다른 provider를 구조화 자동 완료 지원으로 과장하지 않음 |
| 원격 기기 | pinned TLS 실제 등록, 폴더/profile/epoch 명시 허용, mixed plan·durable launch·guarded input | runtime417 PASS/1 ignored cohort, controller33 PASS; storage commit 전 fence 회수 결함 수정 | scoped token·실제 viewport/composer/close·원요청 조회 통합 중 |
| 실행 서버 | TLS listener와 프로젝트 허용 분리, 다른 컴퓨터 project ID 명시 매핑, durable epoch | root server 실제3 PASS; pending copy 추가 회귀 최초 compile RED | scoped credential API 도착 후 실제4 tests/통합 검사 |
| PWA | 원본 실행 ID 유지, 표시 텍스트 요약·공유 session pool로480KiB 이내, 보기 전용 controls 차단 | web404 PASS/1 ignored, strict web Clippy PASS, Chrome93 assertions PASS | close/revoke cap 회귀 실제1 FAIL→1 PASS, 최종 browser 재검사 |
| 격리·복원 | sidecar3, workspace/task/attempt/runtime/op 분리, Unknown 자동 replay 금지 | 기존 source rollback 가능, 원안 metadata schema 불변 | immutable 원요청이 없는 재시작 복구는 Unknown 유지 |

중요한 발견: root 미커밋 server copy가 legacy unrestricted runtime token을 사용하던 경로는 독립 리뷰에서 HIGH로 지적됐다. 프로젝트 전용 token을 별도 발급하고 정확한 device/workspace/grant에 결속하며 command·event 필터와 기존 연결 회수를 통합하는 중이다. 이 수정 전 상태를 프로젝트 제한이 보장된 서비스로 전달하지 않는다.

최신 로그:

- `/private/tmp/deppy-cc-remote-owned-followup-final-20261020.log`: cc_ui actual focused63 PASS/0 FAIL/1 ignored.
- `/private/tmp/deppy-cc-pr07-grant-observe-green-20261020.log`: cc_storage actual controller33 PASS/0 FAIL.
- `/private/tmp/deppy-cc-server-grant-actual-green-20261020.log`: root actual server3 PASS/0 FAIL. 이전 `command_center_server` 필터는0 tests이므로 증거에 포함하지 않는다.
- `/private/tmp/deppy-cc-web-budget-authority-green-20261020.log`: root web404 PASS/0 FAIL/1 ignored.
- `/private/tmp/deppy-cc-web-budget-clippy-20261020.log`: root strict web all-target Clippy exit0.
- `/private/tmp/deppy-cc-budget-browser-final-20261020.log`: shared PWA Chrome93 assertions PASS,390/760/1280·dark/light.
- `/private/tmp/deppy-cc-web-newcaps-{red,green}-20261020.log`: root readonly close/revoke capability targeted1 FAIL→1 PASS.

source cohort가 다른 검사 숫자를 합쳐서 최종 source 전체 통과로 취급하지 않는다. 최종 fmt/boundary/deps/i18n/strict workspace Clippy/full workspace/offscreen은 기능 source가 freeze된 뒤 실행한다. 실제 provider 유료 호출·실앱 GUI·서로 다른 물리 컴퓨터·사용자 DB·장시간 idle CPU 비교는 아직 실행하지 않았다. 원격 fixture는 실제 TLS와 PTY지만 서로 다른 물리 컴퓨터 검증을 대신하지 않는다. TLS listener는 현재 loopback 전용이며 다른 컴퓨터는 보안 터널/포트 전달 주소로 연결한다.

## 초기 leaf 검사 기록

이 문서는 실행한 검사만 기록한다. 기능 브랜치 개발 중이며 main 병합·사용자용 패키지·앱 재실행은 하지 않았다.

| 경계 | 실제 검사 | 결과 | 범위 |
|---|---|---|---|
| Core 작업 계약 | core 전체/strict Clippy | 20 PASS / exit0 | 순수 작업 계약 |
| Reducer + dispatch + planner | standalone 실제 source harness | 32 PASS | task/attempt/runtime/operation/계획 확인 |
| 별도 저장소 | storage targeted/strict Clippy | 20 PASS / exit0 | sidecar, 기존 metadata 불변 |
| 화면 전환 | rustc 실제 source | 2 PASS | opt-in 및 원안 복귀 |
| 세션 route | rustc 실제 source | 3 PASS | stale 대상 폐기/키 재사용 금지 |
| 저장 worker | 실제 source + 임시 sidecar | 1 PASS | lazy 생성·저장 generation·복원 token |
| 네이티브 UI leaf | standalone Cargo | 11 PASS / 1 ignored | serde·IME·레이아웃·입력 슬롯 |
| 네이티브 offscreen | ignored evidence 검사 명시 실행 | 1 PASS / 8 PNG | dark/light·폭·글자 크기·drawer |
| 웹 기존 계층 + 새 projection | cargo test -p web-remote --lib | 402 PASS / 1 ignored | 현재 wire/result forms 포함 |
| 웹 신규 인증/권한/와이어 | cargo test -p web-remote --lib work_ | 9 PASS | 실제 WS, 한글, exact route, 권한 회수, 결과 폼 상한 |
| i18n | cargo test -p i18n | 8 PASS | 키/placeholder/locale completeness |

네이티브 offscreen PNG: `/private/tmp/deppy-command-center-native-evidence-20261020`. 이는 fixture snapshot의 실제 egui 렌더링이며 실행 중인 사용자 앱 캡처가 아니다.

추가 실제 검사: 네이티브 App 초기 `cargo check --tests` 통과, host17 PASS(수동 결과→사용자 검증→최종 확인/미승인 ACK 거부/warm cwd 격리/복원 상태/트랜잭션). 웹 deferred permit1 PASS. 공용 PWA 실제 Chrome86개 assertion 통과(390/760/1280·양 테마·정확한 attempt/revision·IME·접수≠완료·초안 보존·글자 배율·보기 전용). production PWA/Relay/기존 모바일 Chrome7개 test 통과. web-remote 전체402 PASS/1 ignored, all-target strict Clippy exit0. Relay Node16 PASS. 로그 `/private/tmp/deppy-cc-web-cohort-20261020.json`, `/private/tmp/deppy-cc-leaf-browser-results2-20261020.log`.

원격 runtime은 실제 임시 작업자 두 개의 pinnedTLS/동일 SessionId 분리/접수 유실 뒤 중복 실행 방지/Pending journal 동안 기존 명령 응답 검사를 실행했다. 저장 worker adapter와 owned worktree 배정은 아직 통합 중이며 이 검사를 서비스 전체 원격 배정 완료로 확대하지 않는다.

초기 단계에서 미완료였던 자동 작업자 생성/관측/owned worktree/기기 설정은 위 최신 checkpoint를 기준으로 판단한다. UI leaf 통과를 서비스 전체 완료로 취급하지 않는다.
