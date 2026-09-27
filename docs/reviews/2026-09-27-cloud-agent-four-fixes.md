# 클라우드 MCP 미해결 네 건 수정 결과

## 요청과 변경

사용자가 두 차례 리뷰에서 미해결로 남은 네 문제를 모두 수정하도록 요청했다. 작업은 `feat/cloud-agent-mcp`, `/Users/jr/Desktop/projects/deppy-sijo-cloud-agent-mcp`, 기준 `0d28fb1a`에서 수행했다. 다른 worktree의 제품 소스는 변경하지 않았다.

| 기존 우선순위 | 문제 | 수정 | 검증 |
| --- | --- | --- | --- |
| high | 제어 회수 후 runtime 큐의 입력이 PTY에 입장 | 큐 envelope에 회수 가능한 허가를 보존하고 실제 입장 시 검사 | 실제 워커 gate로8개 취소·만료 사례 |
| medium | claim 뒤 만료 시 영구 unknown | rejected/no-effect 수신증을 완료하고 화면 기록 갱신 | 실제 SQLite 잠금 뒤 동일 ID 재조회 |
| medium | 긴 OAuth Location에서 승인 유실 | 공유 헤더 한도로 최종 인코딩 검증, 승인 소비 전에 응답 생성 | 과대 조합 거부·허용 state의 승인/거부 응답 |
| medium | refresh scope 문자열 비교 | 권한 집합 비교·부분집합 access·원래 refresh 범위 유지 | 순서 변경·축소·확대/미지 권한 거부 |

## 입력 회수와 인증 동기화

- `runtime::InputPermit`은 grant의 수명에 연결된다. 입력 허용 해제, take-control, unshare, 대상 세대 제거, stop/rotate, drop이 기존 permit을 폐기한다. 다시 허용하면 새 permit을 만들므로 이전 큐의 입력이 살아나지 않는다.
- permit은 기존 `WriteInputTracked`의 wire payload에 넣지 않고 로컬 `QueuedRuntimeCommand` envelope에만 보존한다. 평시 burst와 종료 drain 모두 같은 guarded 처리 경로를 사용한다. App 워크스페이스의 runtime은 SSH child PTY를 포함해 InProcessRuntimeClient다.
- 실제 queue write는 permit mutex와 인증 mutex 아래 수행된다. 이벤트·pressure·detector·UI wake는 두 lock을 해제한 뒤 실행한다. deadline은 인증 lock 대기 후 다시 검사한다.
- 최종 리뷰에서 추가로 발견한 OAuth access 교체 경로도 수정했다. Request에는 원문 token 대신 access-key fingerprint를 보관하고, App/worker는 개별 token의 만료·교체를 확인한다. 토큰 refresh/rotate/revoke와 queue write를 인증 mutex로 직렬화한다.
- typed `AdmissionDenied`가 PTY reject enum 끝에 추가되어 runtime wire 버전을19로 올렸다. 이전18 피어는 hello에서 거부한다. 기존 명령/event variant 번호와 payload bytes는 유지했다. runtime에 MCP 의존성을 추가하지 않았다.

## 실제 실행한 RED / GREEN

| 명령/검증 | 수정 전 | 수정 후 | 로그 |
| --- | --- | --- | --- |
| MCP `redirect_bounds` | expected400 / actual200 | 통과 | `/tmp/deppy-four-redirect-red.log`, OAuth GREEN |
| MCP `refresh_scope` | 정상 순서 변경400 | 통과 | `/tmp/deppy-four-scope-red.log`, OAuth GREEN |
| App `claim_expiry` | rejected 기대에 unknown | 통과 | `/tmp/deppy-four-claim-red.log`, App GREEN |
| App `queued_cloud_input` | take-control 뒤 queued | 통과 | `/tmp/deppy-four-revoke-red.log`, App GREEN |
| 실제 HTTP pending OAuth request + read-only refresh | 폐기된 access인데 Request.live=true | 통과 | `/tmp/deppy-four-access-proof-{red,green}.log` |
| `cargo test -p agent-mcp` 최종 동기화 버전 | — | **24 passed**, exit0 | `/tmp/deppy-four-atomic-mcp.log` |
| `cargo test -p deppy-sijo --bin deppy-sijo cloud_agent` | — | 최종 **12 passed**, exit0 | `/tmp/deppy-four-atomic-app.log` |
| `cargo test -p runtime --lib tracked` | — | **12 passed**, exit0 | `/tmp/deppy-four-runtime-tracked.log` |
| `cargo test -p runtime --lib command` | — | **27 passed**, exit0 | `/tmp/deppy-four-runtime-command.log` |
| `cargo test -p runtime --lib protocol` | — | **2 passed**, exit0 | `/tmp/deppy-four-runtime-protocol.log` |
| `cargo test -p runtime --lib v12_v13_peer` | — | **1 passed**, exit0,18 포함 | `/tmp/deppy-four-old-peer.log` |
| xtask `check-boundary` / `check-deps` | — | 통과, allowlist 변경 없음 | `/tmp/deppy-four-{boundary,deps}.log` |

App suite는 실제 OAuth HTTP→임시 실제 PTY→실행 출력→봇 자체 답변을 포함한다. 워커 취소 사례는 take-control, unshare, 입력 해제, rotate, stop, generation 변경, deadline 만료, 재허용의8가지다. MCP에는 인증 상태를 유지한 입장 중에는 revoke가 완료되지 않고 완료 뒤 새 입장이 거부되는 동기화 검증도 포함된다.

전체 workspace 또는 실제 외부 Grok Bot 계정/인터넷 터널을 검증했다고 주장하지 않는다. 제품 GUI는 띄우거나 재실행하지 않았다. 테스트는 임시 일반 셸을 사용하고 자체 AI 에이전트/LLM을 실행하지 않는다.

## 코드 리뷰와 실패 접근

- OAuth 범위 CLI 리뷰는 concrete bug 없음으로 완료했다 (`/tmp/deppy-four-oauth-review.log`). OAuth source commit `36e987c9`.
- 첫 전체 CLI 리뷰는 OAuth access identity 누락을 지적했다. 실제 HTTP로 RED를 확인하고 fingerprint를 추가했다. 다음 리뷰는 Boolean 검사와 write 사이의 credential-lock gap을 지적했고, 이미 진행한 atomic authorization callback refinement로 해결했다.
- 이전 리뷰가 제안한 “PTY writer가 이미 수락한 바이트까지 취소”는 기존 API의 receipt boundary 확장 제안이다. 이번 버그는 **runtime 명령 큐에서 아직 PTY에 입장하지 않은 입력**이었다. 도구 계약은 `queued`를 PTY queue admission으로 정의하고 이미 수락된 바이트의 회수를 보증하지 않는다. 따라서 이 조건부 제안은 미해결 제품 결함으로 세지 않는다. 가이드에도 이 경계를 명시했다.
- 현재 atomic callback 파일을 다시 읽은 최종 리뷰 `/tmp/deppy-four-atomic-review.log`는 **추가 actionable defect 없음**, exit0으로 완료됐다. permit·credential lock 직렬화, 양쪽 drain의 permit 보존, 이벤트 전에 lock 해제 경로를 확인했다. 이전 snapshot 리뷰를 최신 소스 결과로 대체해 주장한 것이 아니다.
- 실패 접근: exact-context patch 한 차례 실패 후 올바른 문맥으로 재적용; 추가 expiry assertion의 fmt wrap 실패 후 cargo fmt로 해결. 리뷰 CLI의 sibling find는 범위 밖 read-only 검색이었고 종료 시도 때 이미 프로세스가 끝나 있었다. 제품/사용자 프로세스를 중단하지 않았다. 실제 RED assertion 외 제품 테스트 실패는 없었다.

## 최종 완료 단계

요청된 네 결함과 후속 리뷰의 credential identity/직렬화 결함을 모두 수정했다. 남은 제품 수정 항목은 없다.

- Source commits: `36e987c9` OAuth redirect/scope; `37c808c1` guarded admission, durable no-effect receipt, access identity/credential synchronization.
- `cargo build -p deppy-sijo --release`: **exit0**, optimized build **24.46초**. 로그 `/tmp/deppy-four-release.log`.
- Binary: `/Users/jr/Desktop/projects/deppy-sijo-cloud-agent-mcp/target/release/deppy-sijo`.
- 최종 `cargo fmt --all -- --check`, `git diff --check` 통과. Guide/plan/handoff와 프로젝트 일지를 갱신했다.
- 앱 재실행, 공개 터널, push는 수행하지 않았다. 실제 외부 Grok Bot/public HTTPS 설정은 제공되지 않았으며 별도 환경 검증 항목이다.

다음 에이전트는 `git status --short`, `git log -3 --oneline`, 이 문서를 확인한다. 사용자가 별도로 요청한 경우에만 push 또는 앱 재실행을 진행한다.
