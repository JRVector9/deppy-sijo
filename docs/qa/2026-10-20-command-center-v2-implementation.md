# Command Center v2 구현 검증

## 최신 통합 checkpoint

기능 브랜치 `feat/agent-command-center-v2-20261020`에서 PR별 구현·교차 리뷰·자동 통합 검증을 완료했다. 검사 source `d2b4605c053f423b5d5eac8e5f809f3d92907bc8`에서 최종11 gate가 모두 exit0이다. 기본 화면은 원안이고 `DEPPY_COMMAND_CENTER_V2=1` opt-in, rollback branch/tag는 원본 `f78ebb69`를 보존한다. 제품 릴리스 완료는 아니다.

최종 workspace는 **6041 PASS / 0 FAIL / 67 ignored**(79 Cargo target, nested helper 중복 제외), full App3449 PASS/40 ignored, runtime420 PASS/1 ignored, storage509 PASS다. 별도 native1 PASS/10 PNG, shared Chrome106 assertions, Relay Node17 PASS, production Chrome5 PASS를 실제 확인했다. 상세 명령·지원 경계·최초 실패 및 수정 근거는 [브랜치 인수 기록](2026-10-20-command-center-v2-release.md)에 정리했다.

최종 결과 JSON `/private/tmp/deppy-cc-acceptance-gates-20261020.json`, 로그 `/private/tmp/deppy-cc-acceptance-{gate}-20261020.log`, native PNG `/private/tmp/deppy-command-center-native-postfreeze-evidence-20261020`. root가10장을 모두 열어 대조했으며 760/font1.25 머리글 설정 버튼 잘림 수정도 확인했다.

| 영역 | 구현·리뷰 결과 | 실제 검사 | 남은 확인 |
|---|---|---|---|
| 로컬 배정 | 저장된 claim→profile 준비→idle agent→정확한 binding→durable outbox→guarded 입력 | 최종 full App3449 PASS에 포함 | 실제 유료 provider 호출 미실행 |
| 관측·결과 | Codex exact session/turn/접수 sequence/final response, 타 provider 수동 보고 | stale attempt/lateRejected-after-Admitted 회귀 | 유료 실제 provider 호출 미실행 |
| 로컬 종료 | retained original launch+현재 attempt/runtime/binding+opaque handle만 종료 | 단위 회귀, 독립 리뷰 차단 결함 없음 | 종료는 작업 완료·검증 통과를 뜻하지 않음 |
| 실행 서버 | 명시 project ID 매핑/폴더/agents 허용, durable epoch, exact scoped token 복사 | root server4 PASS | loopback listener; 다른 컴퓨터는 보안 터널 사용 |
| 원격 실행 | pinned TLS/scoped auth/actual PTY/journal, 입력·viewport·종료 | 최종 runtime420 PASS/1 ignored, storage 전체509 PASS, full App3449 PASS | 서로 다른 물리 컴퓨터 검증 미실행 |
| 원요청 복구 | 원래 launch/input immutable request만 SELECT/query, 원래 접수 sequence/time 반환 | actual TLS+SQLite lostACK 및 SQLite late-load 회귀, 최종 full App PASS | 재시작에 원요청 없으면 Unknown |
| PWA 원격 화면 | opaque remote route, View permit 유지, 읽기 전용 실제 화면, max8 watches/5s expiry | work focused11 PASS, Chrome106 assertions PASS | Relay View-only bridge 통합·Node17 PASS·production Chrome PASS |
| PWA 안정성 | ACK 유실 관측만2s 만료, 숨김/재연결 정리, 정확한 route 스크롤 보존 | reconnect RED→GREEN, hidden120/70→0/0 RED→GREEN | 변경·입력 pending fence는 유지 |
| 화면 크기 | native10 PNG/1440·1280·1000·760·390/scale1·1.25/양 테마 | 최종 actual offscreen1 PASS,10장 직접 대조 | 좁은 머리글 버튼 잘림 수정·확인 완료 |
| 데이터·복구 | lazy sidecar3, metadata migration 불변, Unknown 자동 replay 금지 | core/storage/reducer 회귀 | 제품 패키지·앱 재실행 미실행 |

발견한 HIGH 경로는 수정됐다. legacy unrestricted TLS token을 새 연결 화면에서 내보내지 않으며, exact device/workspace/grant scoped token만 복사한다. scoped command/event 필터, 기존 연결 회수, queue 최종 effect fence 및 실패한 replacement 보존을 실제 TLS 검사와 독립 리뷰로 확인했다. PWA 원격 입력은 cross-TLS cloud origin revoke fence 미지원으로 비활성화한다. native의 scoped 원격 입력과 별개다.

이전 통합 cohort의 실제 증거(최종 source 결과와 별도로 보존):

- `/private/tmp/deppy-cc-final-production-browser-20261020.log`: root production PWA·Relay·mobile Chrome5 PASS/0 FAIL/0 ignored.
- `/private/tmp/deppy-cc-reconcile-host-green-20261020.log`: UI owner native79 PASS/0 FAIL/1 ignored.
- `/private/tmp/deppy-cc-integrated-i18n-20261020.log`: root i18n-check exit0.

- `/private/tmp/deppy-cc-local-close-green-20261020.log`: UI owner command_center75 PASS/0 FAIL/1 ignored.
- `/private/tmp/deppy-cc-pr07-reconcile-final-20261020-results.json`: storage26/runtime420(1 ignored)/App31 및 strict runtime/storage Clippy exit0.
- `/private/tmp/deppy-cc-server-copy-integrated-green2-20261020.log`: root server4 PASS. 이전 `command_center_server` 필터0 tests는 증거가 아니다.
- `/private/tmp/deppy-cc-web-remote-readonly-suite-green-20261020.log`: root focused11 PASS.
- `/private/tmp/deppy-cc-readonly-lifecycle-green-20261020.log`: shared PWA Chrome106 assertions PASS. 실제 lostACK RED는 `...lifecycle-red...log`.
- `/private/tmp/deppy-cc-native-final-offscreen-20261020.log`: actual offscreen1 PASS,10 PNG `/private/tmp/deppy-command-center-native-final-evidence-20261020`.
- `/private/tmp/deppy-cc-freeze-fmt-check-20261020.log`: root fmt check exit0; `/private/tmp/deppy-cc-final-boundary-20261020.log`: boundary exit0 at856b132b.

서로 다른 source cohort의 검사 수를 합쳐 최종 전체 통과로 취급하지 않는다. 최종 source freeze 뒤 fmt/boundary/deps/i18n/strict workspace Clippy/full workspace 및 화면·브라우저 gate를 모두 실행했다. 실제 provider 유료 호출·실앱 GUI·서로 다른 물리 컴퓨터·사용자 DB·main 대비 장시간 idle CPU/메모리 비교는 실행하지 않았다. 원격 fixture는 실제 TLS/PTY/SQLite를 사용하지만 물리 컴퓨터 검증을 대신하지 않는다. native PNG는 fixture의 실제 egui 렌더이며 실행 중인 사용자 앱 캡처가 아니다. source0.13.0은 검사 빌드로, 사용자용 릴리스·버전 검증된 패키지를 전달하지 않았다.

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
