# 클라우드 관리 2차 리뷰 잔여 3건 수정·검증

2026-10-13. 사용자 “코드 수정해”에 따라 [2차 리뷰](../reviews/2026-10-13-cloud-management-fixes-rereview.md)의 N1–N3을 수정했다. 기준 HEAD `bf13eb48`, canonical `/Users/jr/Desktop/projects/deppy-sijo-performance`. 기존 gpt-6.1-sol/xhigh 작업자 3명과 Root가 파일 소유권을 나눠 구현·교차 리뷰했다. 원래 dirty checkout과 기존 untracked HTML을 보존했다.

## 변경 결과

| 항목 | 변경 | 검증 |
| --- | --- | --- |
| N1 늦은 키 등록·교체 | raw 작업/답변 캐시는 private으로 유지하고 현재 corpus로 표시·복사·publication을 다시 보호 | 실제 SQLite/bootstrap/Get/egui display·copy·URI, late/replacement 회귀 |
| N1 동일 원인의 notify 범위 | 답변 수신 시 wrapped→ANSI 제거→controls 정리→wrapped. 설정·세션·PWA 답변도 현재 corpus 사용 | 실제 ClaimScoped→FinishScoped→OpenPage, AnswerNotice/history 및 native/session/PWA 회귀 |
| N1 알림 미리보기 | 전체 source(title8KiB/message16KiB)를 제한해 보존. 현재 키로 마스킹한 뒤 title120/preview240char로 자르고 localize. 알림 목록/대기 native delivery 모두 재검사 | actual egui 최근 목록·설정 목록·클릭 target 및 native intent pop, late/replacement/clip 회귀 |
| N1 작업 참조 표시 | 원본 run/run_id는 유지하고 현재 키로 보호한 run_label만 native/PWA에 표시. footer·hover도 마스킹. 연결 이름·세션 제목은 fallback 선택 후 wrapped 보호 | 실제 header/footer/tooltip, legacy optional label, 마스킹 확대·DTO budget·원본 참조 보존 회귀 |
| N2 clock 샘플 실패 | 마지막 정상 baseline 보존. 지원 OS에서 unknown frame이면 input grant/lease/queued permit 회수. 실제 suspend일 때만 tunnel revalidation | 합성 baseline, fresh legacy/named, 실제 queued PTY, 정상 idle/unsupported fallback 대조 |
| N3 PWA 원본 이동 | queued 접수와 App observed completion 분리. 요청 소켓별 final result, 닫힘·실패·시간 초과·superseded 안내 | 실제 authenticated WS, authority/core/origin/policy fence, production browser 회귀 |

권한 판단용 connection/run/session/generation/operation ID와 native notification target은 보존했다. artifact URI는 정확히 안전한 원본 또는 제외한다. cheap metadata helper로 매 frame authority 확인을 위해 전체 작업을 재마스킹하지 않는다. 세션 답변 projection은 App branch마다 한 번 생성해 active/warm runtime에 동일 Arc를 배분한다.

N3는 `cloud_navigation_result`의 bounded enum(opened/unavailable/timed_out/superseded/rejected)을 추가한다. App이 현재 original UUID/runtime/mux/focus를 확인한 결과만 전달한다. callback은 원래 transport activation/core/권한/epoch/만료를 다시 확인하고 socket-local 16-slot `try_send`에 넣는다. native activation은 최대30초, PWA queued 후 final 결과는40초, opened 뒤 dashboard 동기화는30초로 제한한다. 늦은 답변은 새 이동을 덮어쓰지 않고, snapshot은 실패 안내를 지우지 않는다. 다른 새 제어 요청은 이전 실패 안내만 해제한다. 현재 relay shell은 view-only/send=false라 새 제어 기능을 활성화하지 않았다.

## 실제 실행 결과

모든 명령은 canonical worktree에서 실제 실행했다. 구조 경계 수정 뒤 영향받는 App/알림/strict/fmt/boundary/diff를 다시 통과했다. 변경되지 않은 웹·브라우저·답변·completion 검사는 앞선 실제 최종 실행 결과를 유지한다. 최종 소스 커밋 후 `git diff --exit-code -- crates web`도 exit0이다.

| 명령 | 실제 결과 | 로그 |
| --- | --- | --- |
| `CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo cloud_agent::` | 155 PASS /3 ignored, exit0 | [app-green](evidence/2026-10-13-cloud-residual-fixes/app-green.log) |
| 같은 App 대상 `ui::cloud_answer::` | 2 PASS, exit0 | [answer-green](evidence/2026-10-13-cloud-residual-fixes/answer-green.log) |
| 같은 App 대상 `ui::notifications::` | 28 PASS, exit0 | [notifications-green](evidence/2026-10-13-cloud-residual-fixes/notifications-green.log) |
| 같은 App 대상 `cloud_navigation_completion` | 1 PASS, exit0 | [navigation-green](evidence/2026-10-13-cloud-residual-fixes/navigation-green.log) |
| `CARGO_BUILD_JOBS=2 cargo test --locked -p web-remote` | 403 PASS /13 ignored /doc0, exit0 | [web-green](evidence/2026-10-13-cloud-residual-fixes/web-green.log) |
| `node crates/web-remote/tests/fixtures/run-cloud-results-mobile.mjs` | actual Chrome77 PASS, exit0 | [browser-green](evidence/2026-10-13-cloud-residual-fixes/browser-green.log) |
| 5crate `clippy --locked --all-targets -- -D warnings` | exit0: deppy-sijo/agent-mcp/web-remote/runtime/secret | [strict](evidence/2026-10-13-cloud-residual-fixes/strict.log) |
| `cargo fmt --all -- --check` / `cargo run --locked -p xtask -- check-boundary` / `git diff --check` | 모두 exit0 | [검사 기록](evidence/2026-10-13-cloud-residual-fixes/README.md) |

Rust 합계589 PASS,16 ignored. production browser77 checks를 별도 구분한다. ignored는 통과로 합산하지 않았다. 소유권을 나눈 독립 교차 리뷰와 Root producer/consumer/authority 대조를 완료했고 마지막 callback 주입 검토에서도 추가 확정 결함이 없었다.

## RED와 실패 접근

- N1 canonical late_secret: 기존1 PASS / 신규4 FAIL, exit101. 캐시·wrapped snapshot·replacement·실제 UI 표시 기대 위반 확인 후 수정.
- N2 canonical unavailable: 9 PASS /4 FAIL, exit101. baseline 삭제, fresh legacy/named dispatch, queued 실제 PTY 보호 위반 확인 후 수정.
- N3 valid Navigate completion stub: 0 PASS/1 FAIL callback timeout. WS callback 미연결:0 PASS/1 FAIL. 첫 authority fixture가 non-Navigate여서 reply=None으로 실패한 것은 fixture 오류로 고친 뒤 행동 RED를 재실행했다.
- N3 production browser queued-before-final RED, preparing 배너가 실패 뒤 남는 RED, 새 제어 요청이 옛 이동 실패 안내를 유지하는 RED를 각각 확인한 뒤 수정.
- 교차 리뷰가 N1 동일 원인의 notify/알림 누락을 찾아 범위를 보완. 실제 worker/UI late_secret_notify 0 PASS/4 FAIL. 이후 전체 notify5 PASS/5 FAIL(known wrapped/ANSI ingress2 + 알림3) 확인 후 수정.
- Notifications fixture의 iterator `.len()`/index 사용 compile 오류는 fixture를 `.count()`/`.next()`로 고친 뒤 행동 RED를 재실행했다. compile 실패를 보호 행동 RED로 주장하지 않는다.
- clipped source에 terminal fragment masking을 적용한 중간 구현은 notify7 PASS/3 FAIL이었다. 시작이 확정된 일반 문장에도 fragment의 보수적 boundary 규칙이 적용됐다. SecretService를 바꾸지 않고 bounded complete source로 되돌린 뒤 마스킹→clip 순서로 수정했다. 원래 알림3 회귀 기대값은 유지했다.
- 작업 ID 표시의 실제 RED: getter/native header/PWA 0 PASS/3 FAIL, native footer/hover 0 PASS/2 FAIL, legacy label 없음 0 PASS/1 FAIL. 답변 표시 이름의 fallback 추가 RED 0 PASS/1 FAIL 후 선택한 표시값에 현재 wrapped 보호를 적용했다. 원본 식별자는 바꾸지 않았다.
- 추가 길이 fixture의 첫 전체 검사154 PASS/1 FAIL은 한 바이트 비밀값이 최소6바이트 정책에서 거절된 fixture 오류였다. 두 번째154 PASS/1 FAIL은 인접 비밀 구간을 단일 marker로 합치는 기존 정책과 기대값이 달랐다. 제품 수정 없이 구분자를 포함한 유효126바이트 ID/198바이트 표시값으로 보정했다. 합법128바이트 ID가 512바이트를 초과하도록 확대된다고 주장하지 않는다. DTO의 별도512바이트 예산·legacy wire default는 독립 검사한다.
- strict lint 통과 후 boundary가 leaf notifications UI의 MCP/secret 직접 의존2건을 실제 거절했다. allowlist를 바꾸지 않고 App에서 순수 마스킹 callback을 주입하도록 수정했다. UI는 제한된 표시 원문·target·전달 intent만 관리하며 credential 서비스나 transport를 참조하지 않는다. 구조 검사 preflight는 exit0이고, 변경 후 영향 검사도 재실행한다.

## 범위와 기록

새 credential을 알아내기 전에 외부로 이미 전달한 과거 메시지를 회수하거나 OS 알림을 삭제하는 작업은 하지 않았다. 이미 ESC가 파괴된 임의 legacy `[31m` 문자열을 추측 복원하는 migration도 추가하지 않았다. 원본 기록은 기존 보호/보존 정책을 따르며 읽기 경계는 현재 알려진 corpus로 보호한다.

실사용 사용자 DB/키/시스템 클립보드/브라우저 프로필을 쓰지 않았다. UI copy 검사는 egui OutputCommand를 확인한다. 실제 provider 계정·public tunnel·실제 OS sleep·Windows 전체 runtime·사용자 앱에서 App→PWA 원본 이동 E2E는 미실행이다. native 알림 검사는 pending intent까지이며 OS로 실제 전달하지 않았다. 실행 중 앱 종료/launch·push·release 없음. source/test version0.12.1이며 다음 사용자용 release는 별도의 version bump와 artifact 검증이 필요하다.

소스 커밋은 원인별로 분리했다. 공통 파일은 hunk 단위로 stage해 각 변경의 표시·소비자 계약을 함께 커밋했다.

- N2 `5fff2264` — unknown clock에서 입력 동의 회수·정상 baseline 보존.
- N1 `8c5e0db4` — late/replacement credential에 맞춰 작업·답변·알림·참조 표시 보호.
- N3 `7e02a46d` — App observed 결과의 권한 있는 소켓 전달과 PWA 실패 안내.
