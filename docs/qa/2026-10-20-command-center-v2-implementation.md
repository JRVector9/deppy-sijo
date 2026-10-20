# Command Center v2 구현 검증

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

아직 진행 중: 자동 작업자 생성, 실제 provider 응답/승인 연결, owned worktree/기기 설정 연동, 전체 회귀/성능 비교, 검사 빌드. UI leaf 통과를 서비스 전체 완료로 취급하지 않는다.
