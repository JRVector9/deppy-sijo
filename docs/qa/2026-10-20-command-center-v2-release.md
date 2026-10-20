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

검사 source는 `d2b4605c053f423b5d5eac8e5f809f3d92907bc8`다. 이후 문서 커밋은 제품 source를 변경하지 않는다. root 전용 `target`, `CARGO_BUILD_JOBS=2`, Rust 검사 `--test-threads=1` 조건에서 아래 **11개 gate 모두 exit0**을 실제 확인했다.

| 검사 | 실제 결과 |
|---|---|
| `cargo fmt --all -- --check` | PASS |
| `xtask check-boundary` / `check-deps` | 각각 PASS |
| `xtask i18n-check` | PASS, 리터럴1694·동적173 호출, 5개 locale |
| `cargo clippy --offline --locked --workspace --all-targets -- -D warnings` | PASS |
| `cargo test --offline --locked --workspace --no-fail-fast -- --test-threads=1` | **6041 PASS / 0 FAIL / 67 ignored**, 79개 Cargo target, 339.492초 |
| `xtask smoke-db-migrations` | PASS |
| native ignored render fixture 명시 실행 | **1 PASS**, 최종10 PNG |
| shared PWA 실제 Chrome | **106 assertions PASS**, 390/760/1280, dark/light |
| Relay Node | **17 PASS / 0 FAIL / 0 skipped** |
| production PWA·Relay·mobile 실제 Chrome | **5 PASS / 0 FAIL / 0 ignored** |

workspace 수치는 Cargo target별 마지막 결과를 집계하여 내부 subprocess helper의 중복 보고를 제외했다. 기본 suite의67 ignored를 통과로 합산하지 않는다. native render와 production browser5개는 위 별도 gate에서 명시 실행했다. 다른 performance/수동 검사는 계속 미실행이다.

최종 명령·source·exit·시간·로그: `/private/tmp/deppy-cc-acceptance-gates-20261020.json`. 원본 로그는 `/private/tmp/deppy-cc-acceptance-{gate}-20261020.log`, 집계는 `/private/tmp/deppy-cc-acceptance-workspace-counts-20261020.json`이다. native10 PNG는 `/private/tmp/deppy-command-center-native-postfreeze-evidence-20261020`에 있으며 root가 10장 모두 열어 대조했다. 760/font1.25 설정 버튼이 머리글 안에 유지되는 것을 확인했다. 이는 fixture의 실제 egui 렌더이며 실행 중 사용자 앱 캡처가 아니다.

화면·브라우저 gate의 재현 명령:

```sh
export CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-command-center-20261020/target
export CARGO_BUILD_JOBS=2
export DEPPY_CC_EVIDENCE_DIR=/private/tmp/deppy-command-center-native-postfreeze-evidence-20261020
cargo test --offline --locked -p deppy-sijo command_center_render_evidence -- --ignored --nocapture --test-threads=1
node web/shared/tests/command-center.browser.mjs
node --test web/relay-shell/tests/reconnect.test.mjs web/relay-shell/tests/channel-concurrency.test.mjs
cargo test --offline --locked -p web-remote --test command_center_chrome --test relay_shell_chrome --test pwa_unified_chrome --test mobile_shell_chrome -- --ignored --test-threads=1
```

## 발견·수정·재검토

- 닫힌 recovered idle 실행이 새 유휴 실행을 가리는 선택 오류와 stale native 생성 요청의 UI 우회는 공통 exact-owner 선택기·host guard로 수정했다. actual RED→GREEN 및 독립 리뷰를 완료했다.
- 원래 실제 접수 receipt가 늦은 SQLite load/CAS 실패로 Uncertain으로 돌아가던 경로는 query/load 직렬화와 정확한 immutable attempt/config/binding/task/source-result 대조로 수정했다. 미래 프로젝트 설정은 원래 실행 identity로 사용하지 않는다. 독립 리뷰에서 남은 차단 결함을 찾지 못했다.
- View-only Relay 관측 누락, lost read ACK 뒤 갱신 정지, 숨김 DOM의0크기가 원격 스크롤 위치를 덮는 문제, 대형 projection budget, 좁은 머리글 버튼 잘림을 실제 재현하고 회귀로 검증했다.
- 최초9b workspace 실행은6039 PASS/1 FAIL/67 ignored였다. 실패 원인은 capacity lease epoch가 바뀌면 이미 실행된 exact worker도 거부해야 한다는 오래된 테스트 assertion이었다. 테스트만 분리하여 stale initial launch는 계속 거부하고, 실제 connection/grant 변경도 기존 proof를 무효화하며, capacity-only 관측에서는 기존 실행 입력을 보존하는 것을 확인했다. 제품 guard를 완화하지 않았으며 최종d2 전체 검사로 재확인했다. 최초 실패 로그는 `/private/tmp/deppy-cc-final-workspace-20261020.log`에 보존했다.

## 배포 전 별도 확인

실제 유료 provider 실행, 사용자 데이터로 앱 재시작/복구, 물리 기기 간 연결, 동일 조건 main 대비 장시간 idle CPU·retained memory·frame p95 비교는 이번 자동 harness 검증에 포함하지 않았다. 제품 artifact를 사용자에게 배포할 때는 버전 증가·서명·bundle 검증을 함께 수행한다.
