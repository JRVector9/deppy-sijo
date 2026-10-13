# 클라우드 관리 리뷰 7건 수정·검증

2026-10-13. [원본 리뷰](../reviews/2026-10-13-cloud-agent-management-parallel-review.md)의 R1–R7을 모두 수정했다. 기준은 `fcf237f7`, canonical worktree는 `/Users/jr/Desktop/projects/deppy-sijo-performance`이다. 세 작업자를 **gpt-6.1-sol / xhigh / fork none**로 실행하고 파일 소유권을 분리했다. Root가 공통 App 계약을 통합하고 실제 검사 및 최종 리뷰를 수행했다.

## 수정 결과

| 항목 | 변경 | 검증 근거 |
| --- | --- | --- |
| R1 비밀 값 보호 | 원문 wrapped 마스킹 → 완전한 ANSI 제거 → 제어 문자 정규화 → 재마스킹. 기존 run/receipt 읽기에도 wrapped 보호 적용; 바뀌는 artifact URI는 제외 | 실제 SQLite 저장·재조회·cache·snapshot 및 legacy bootstrap/receipt 회귀 |
| R2 정상 유휴와 절전 구분 | 절전 포함/제외 native clock의 차이를 사용. PTY 쓰기 직전에 다시 확인해 App frame 이전에 재개한 대기 입력도 차단 | 정상 유휴·clock 오류 회귀 및 legacy/named 두 경로의 실제 queued PTY 검사 |
| R3 로컬 상태 | Sidebar/Fleet과 같은 active/warm `session_entries` 상태를 사용. provider/model은 별도 identity 필드; 없는 관측 시각을 만들지 않음 | 상태 8종 DTO/producer 계약, native 설정·결과 harness, browser labels |
| R4 PWA 갱신 | 작업·연결·답변 ID로 기존 DOM 유지, 최신 action binding 갱신. 중복 key도 별도 노드로 소비 | 실제 Chrome에서 focus·선택·내용 변경·권한 회수·동일 ID/다른 연결·중복 key 검사 |
| R5 키 교체 | 폐기된 named ACL 제거 및 lease/permit 회수; legacy 읽기 동의·철회 상태 유지 | 256세션×8연결×4회 교체, legacy consent 및 queue 차단 회귀 |
| R6 OAuth 표시 이름 | 승인한 client 이름을 Access/Refresh 표시 정보로 보존. UUID 권한 기준과 owner rename 유지 | 서로 다른 봇 승인·refresh 및 실제 HTTP refresh/rename 회귀 |
| R7 원본 세션 이동 | 요청별 결과와 실패 사유 반환. 최신 mux/원본 UUID/runtime 및 실제 focus를 확인한 뒤 완료; 최대 30초 대기 | native 실패/성공/superseded harness, completion policy, 세션별 보조 탭 회귀·호출 흐름 리뷰 |

R7의 PWA 효과는 operator 권한을 재검증한 동일 frame에서 적용한다. 관측된 성공 이후 목적지의 `SessionAuxScope`를 먼저 동기화하고 터미널을 드러낸다. 실패는 결과 화면을 유지하고, 새 요청은 이전 요청의 포커스 복원을 취소한다.

## 실제 실행 결과

모든 아래 명령은 canonical worktree에서 실행했다. 로그는 [evidence 디렉터리](evidence/2026-10-13-cloud-management-review-fixes/)에 보관했다.

| 명령 | 결과 | 로그 |
| --- | --- | --- |
| `CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo cloud_agent::` | **135 PASS / 0 FAIL / 3 ignored**, exit 0 | [app-green.log](evidence/2026-10-13-cloud-management-review-fixes/app-green.log) |
| `CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo cloud_navigation_completion` | 1 PASS, exit 0 | [navigation-green.log](evidence/2026-10-13-cloud-management-review-fixes/navigation-green.log) |
| `CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo 세션이동` | 1 PASS, exit 0 | [reveal-green.log](evidence/2026-10-13-cloud-management-review-fixes/reveal-green.log) |
| `CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo session_aux::` | 3 PASS, exit 0 | [session-aux-green.log](evidence/2026-10-13-cloud-management-review-fixes/session-aux-green.log) |
| `CARGO_BUILD_JOBS=2 cargo test --locked -p agent-mcp -p web-remote -p i18n` | agent-mcp **67 PASS/1 ignored**; web **400 PASS/13 ignored**; i18n **8 PASS**; doc-tests 0; exit 0 | [crates-green.log](evidence/2026-10-13-cloud-management-review-fixes/crates-green.log) |
| `node crates/web-remote/tests/fixtures/run-cloud-results-mobile.mjs` | **CLOUD_RESULTS_MOBILE_OK:50**, exit 0 | [browser-green.log](evidence/2026-10-13-cloud-management-review-fixes/browser-green.log) |
| `CARGO_BUILD_JOBS=2 cargo clippy --locked -p deppy-sijo -p agent-mcp -p web-remote -p runtime -p secret --all-targets -- -D warnings` | exit 0 | [clippy.log](evidence/2026-10-13-cloud-management-review-fixes/clippy.log) |
| `CARGO_BUILD_JOBS=2 cargo run --locked -p xtask -- check-boundary` | boundary OK, exit 0 | [boundary.log](evidence/2026-10-13-cloud-management-review-fixes/boundary.log) |
| `cargo fmt --all --check`; `git diff --check` | 각각 exit 0 | 최종 도구 실행 |

App cloud suite에는 격리 HTTP/OAuth → 실제 PTY roundtrip 및 신규 대기 입력 입장 검사가 포함된다. 브라우저 검사는 제품 HTML/JS/CSS와 독립 headless profile을 사용했다. ignored browser suite를 통과했다고 주장하지 않으며 직접 실행한 cloud-results fixture 결과를 따로 보고한다. agent-mcp 로그에는 자식 프로세스 검사 1건이 포함되므로 중복 합산하지 않는다.

## RED 및 리뷰 기록

- 초기 App RED: 114 PASS/8 FAIL/3 ignored, exit 101. R1/R2/R3/R5/R7의 보호 기대 위반을 재현했다.
- OAuth 이름과 refresh는 각각 0 PASS/1 FAIL, DTO 상태는 0 PASS/1 FAIL을 확인하고 수정했다.
- R2 App frame 이전 PTY 재개는 0 PASS/2 FAIL → 양쪽 경로 PASS. 실제 OS sleep은 발생시키지 않았다.
- R1 BEL+CSI/OSC 혼합 누락을 교차 리뷰에서 발견하고 durable RED를 확인했다. 기존 complete ANSI stripper를 재사용했다. legacy wrapped run/receipt는 0 PASS/2 FAIL → 2 PASS.
- R4 실제 브라우저 RED → 38 → 상태 분리·중복 key 회귀 추가 후 50 PASS. 이전 focus proof도 담당자가 33 PASS를 실행했다.
- 중간 131 PASS/1 FAIL은 같은 identity Label이 두 곳에 있어 harness query가 모호해진 결과다. `get_all_by_label`로 두 곳을 검증하도록 수정했다.
- R7 교차 리뷰의 supersession·권한의 2단 queue·오래된 mux·정보 페이지/보조 탭 유지 문제를 수정했다. 최종 제품 호출 흐름 리뷰와 policy/UI harness 검사를 수행했다.
- 초기 fixture의 `Job` Debug 제약과 legacy `SaveRun` 초기 상태 오류는 fixture만 고친 뒤 행동 RED를 재실행했다. Done hook revision을 시각으로 쓰는 접근은 폐기하고 current generation의 confirmed idle Unix seconds만 사용한다.

## 검증 범위와 소스

실 사용자 DB/토큰/클립보드나 실행 중 앱을 변경하지 않았다. 실제 클라우드 제공자 계정·공개 터널·실제 OS sleep·Windows 전체 앱 runtime·실행 중 앱의 원본 세션 이동 E2E는 미실행이다. 이번 native 검사는 격리 egui harness이며, App 원본 이동은 completion policy와 제품 호출 흐름 리뷰로 확인했다.

과거 코드가 ESC를 파괴해 저장한 임의의 `[31m` 문자열은 안전하게 복원·판별할 수 없어 추측 삭제나 데이터 이관을 추가하지 않았다. 등록 비밀의 legacy wrapped 값은 표시·관리 조회 시 보호한다.

소스 커밋: R1 `cd614286`, R6 `18491c2e`, R2/R3/R4/R5/R7의 컴파일 가능한 공통 계약 통합 `a3843d99`. source/test version **0.12.1** 유지. 사용자용 artifact 릴리스·배포·push·앱 재실행은 이번에 수행하지 않았다. 다음 제품 릴리스에는 별도의 버전 증가와 artifact 버전 검증이 필요하다.
