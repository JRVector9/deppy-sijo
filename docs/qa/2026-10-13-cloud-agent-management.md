# 클라우드 에이전트 관리 PR 1–6 수용 검증

기준 소스: `3ac1f61f`, worktree `/Users/jr/Desktop/projects/deppy-sijo-performance`.
사용자는 PR 1–6의 병렬 구현, `gpt-6.1-sol`/`xhigh` 서브에이전트, root 리뷰와 커밋을 승인했다.
연결 인증, 작업/이력, 설정 UI, PWA는 별도 소유자로 개발했으며 Cargo는 root가 직렬 실행했다.

## 요구사항과 실행 증거

| PR | 수용 항목 | 구현/검증 근거 | 결과 |
| --- | --- | --- | --- |
| 1 | 로컬 수신 준비·공개 URL 준비·실제 인증·도구 성공 구분 | `cloud_agent::ui::tests::setup_state_*`, 복사 지시문 다중 줄/paste/접수 안내 | UI 11 PASS; 독립 CLI 리뷰 추가 결함 없음; `35e8a264` |
| 2 | 검증된 연결 ID, 독립 권한, 동일 입력 ID의 주체별 분리 | `principal.rs`, `control.rs`, `bridge_tests.rs`; 실제 OAuth HTTP→PTY, revoke/만료/lease 축소/절전 간격 큐 입장 | 실제 OAuth HTTP/PTY·독립 세션·단일 writer·legacy ACL 회귀 최종 PASS |
| 3 | 영속 작업·heartbeat·unknown·취소 요청/응답·원본 책임 주체 | `management.rs`, `history.rs`, App async management/worker; 종료 저장 후 응답 권한 소실, 지연/잘못된 주체·세대 | domain management 9 PASS; App helper 14 PASS; UI 취소/보고 분리 PASS |
| 4 | 화면 밖 이력·커서·비밀 마스킹·관측 시각·접수 조회·폴링 예산 | history 14 PASS/1 ignored; output 3 PASS; 실제 누락 출력·부분 페이지·Unicode·큐 예산 및 scoped receipt | 전체 추출→fragment 마스킹→바이트 제한, cursor는 실제 반환 행 높이; Secret 79 PASS; ANSI/한글·부분 페이지·중첩 비밀 경계 포함 |
| 5 | 비밀 없는 프로필·명시적 재연결·제한 backoff·절전 재검증 | `continuity.rs`, `config.rs`, bridge/profile UI; 1/2/4/8/16/30초 최대 6회, 명시 종료 취소 | 프로필/복구/절전·재연결 해제 회귀 PASS; 승인/입력 재실행 미저장 |
| 6 | 데스크톱/PWA 동일 결과·검색·산출물·원본 세션 이동 | native UI 25 PASS; web-remote 399 PASS/13 ignored, 실제 Chrome 6 PASS; 모바일 fixture 31 checks, relay Node 16 PASS | 웹 커밋 `1878a449`; native 720px 렌더·중복 ID·모바일 요청 인증 재검증 PASS |

### 실행 로그

- `CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo cloud_agent::`
  - diagnostic1: 88 PASS/15 FAIL/3 ignored. 신규 행동 회귀와 기존 fixture 문제를 분리해 기록했다.
  - diagnostic2: 100 PASS/5 FAIL/3 ignored. credential URI 2건 RED, 실제 HTTP/PTY 내부 작업 ID 거절 RED를 확인했다.
  - diagnostic3: 105 PASS/1 FAIL/3 ignored, 7.25초. named-only 읽기 허용의 legacy bearer 전파 회귀를 재현했고 수정했다.
  - 후속 리뷰 RED2: 107 PASS/5 FAIL/3 ignored, 10.11초. named 입력 권한, artifact 투영, 활동 시각, reconnect 해제, native 중복 ID를 재현했다.
  - 최종 GREEN: **112 PASS/0 FAIL/3 ignored**, 7.48초. 모든 후속 회귀 및 실제 HTTP/PTY 경로 PASS. 로그 `/private/tmp/deppy-cloud-final-app-cloud-20261013.log`.
- 최종 추가 검사: history worker **6 PASS**, config **45 PASS**, 답변 UI **1 PASS**, i18n **8 PASS**. 각각 `/private/tmp/deppy-cloud-final-{history-worker,config,answer-ui,i18n}-20261013.log`.
- `CARGO_BUILD_JOBS=2 cargo test --locked -p runtime tracked_input_`: 4 PASS. 내부 scoped ID의 wire roundtrip/사전 검증 및 실제 PTY 큐 입장 검사.
- foundation: agent-mcp 66 PASS/1 ignored, Secret **79 PASS**/doc 0; strict secret/agent-mcp all-targets clippy PASS. 후속 리뷰의 compact newline/중첩 비교 비용/ANSI Unicode 스팬 결함을 실제 RED→GREEN으로 보완했다.
- native 720px 렌더: [실제 720px 렌더](assets/cloud-agent-management-2026-10-13/native-720.png). 테스트 renderer만 실행했으며 실제 앱을 재시작하지 않았다.
- 최종 strict: `cargo clippy --locked -p secret -p agent-mcp -p runtime -p deppy-sijo -p web-remote --all-targets -- -D warnings` **PASS**, 25.33초. 첫 strict 검사에서 신규 App lint가 발견되어 테스트 전용 코드/동일 let-chain/worker 응답 indirection을 정리한 뒤 위 모든 App 검사를 다시 통과했다.
- `cargo fmt --all --check`, `cargo run --locked -p xtask -- check-boundary`, `git diff --check`: **모두 PASS**. 로그 `/private/tmp/deppy-cloud-final-{strict,fmt,boundary,diff}-20261013.log`.
- agent-mcp/Secret/foundation strict의 실제 과거 출력은 `/private/tmp/deppy-cloud-{agent-mcp,secret,foundation-strict}-captured-stdout-20261013.log`에 세션 도구 출력에서 사후 보존했다. web 최종 epoch16 전체 출력은 `/private/tmp/deppy-cloud-web-epoch16-captured-stdout-20261013.log`. 사후 출력 보존이며 검사 재실행으로 표시하지 않는다.
- 실제 Chrome6/모바일31/RelayNode16 결과는 당시 도구 출력으로 확인했다. 별도 원본 로그 파일이 남지 않은 검사는 로그 경로를 만들어냈다고 주장하지 않는다.

## 리뷰와 주요 설계

Root는 소유자별 실제 diff를 대조하고 `codex exec -m gpt-6.1-sol -c model_reasoning_effort=xhigh --sandbox read-only`로 독립 소스 리뷰를 수행했다. 초기 foundation 2건, App 9건, PWA 1건은 각 회귀로 보완했다. 최종 App 6건, Secret 3건의 후속 발견도 회귀 RED→GREEN으로 수정했다. 모바일 origin 재검증의 별도 최종 CLI 리뷰는 추가 구체 결함이 없었으며 root가 최종 16바이트 epoch 계약과 5개 후속 수정 diff를 직접 확인했다.

- 공개 URL 준비나 입력 `queued`를 인증/완료로 표시하지 않는다. 로컬 터미널 상태와 클라우드 에이전트 보고 상태는 별도다.
- 새 내부 작업 키는 연결별로 scope를 부여한다. 외부 operation ID는 `@`를 허용하지 않고 runtime은 검증된 내부 namespace를 opaque receipt ID로 받는다.
- 입력 허용에는 대상/세대/주체/권한/제어 lease/인증/deadline을 모두 확인한다. revoke는 실제 runtime queue admission까지 전달한다.
- DB commit 이후 권한이 없어져도 로컬 durable cache는 저장 사실을 반영한다. 외부 응답은 조회 후 판단해야 하는 unknown으로 반환한다.
- 비밀이 마스킹되어 달라진 artifact URI는 다른 링크로 공개하지 않는다. 신규 보고는 저장 전에 거절하고 복원된 URI는 제거한다.
- complete snapshot과 fragment redaction의 목적을 구분한다. 페이지 경계의 알려진 비밀 조각은 보수적으로 숨기며 공개 출력은 byte cap/truncation을 명시한다.
- 작업 결과는 원본 workspace/session/generation/actor에 귀속한다. native/PWA 이동은 기존 세션을 열며 권한을 새로 부여하지 않는다.
- 모바일 관리 동작은 인증된 origin/전체 16바이트 epoch/정확한 권한/만료/5초 deadline을 가진 단일 소비 envelope로 전달한다. 큐 접수와 App 적용 직전에 재검증하며 연결 해제·서버 종료·계정 회수 후 대기 동작은 거절한다. 웹 스레드가 직접 PTY/SQLite를 수정하지 않는다.
- 일부 복원된 artifact 링크가 웹 검증에서 지원되지 않아도 안전한 링크와 부모 작업/결과를 유지한다. 빠진 링크는 truncated로 공개하며 최근 활동은 관측 시각의 최댓값이다.

## 검증 범위와 한계

- 격리 임시 DB, 실제 로컬 HTTP/OAuth/PTY, 테스트 native renderer, 실제 Chrome의 production frontend fixture를 사용했다.
- 실제 사용자의 토큰/DB/터미널/클립보드를 변경하거나 제공자 계정에서 로그인·작업을 실행하지 않았다.
- dot/Muse/Grok Bot 등 실제 계정별 인증 호환성은 이번 검증으로 보증하지 않는다. 계정별 등록 화면과 지원 인증 방식 확인은 별도 E2E 검증이다.
- 실제 OS sleep은 재현하지 않았다. sleep-sized heartbeat gap과 대기 입력 permit 회수 시나리오는 격리 테스트로 검증했다.
- 앱 version 0.12.1의 소스/조사용 테스트 build이며 변경된 제품 artifact를 사용자에게 배포하지 않았다. 푸시·제품 릴리스·앱 재시작은 실행하지 않았다.
- `docs/mockups/active-work-visibility-2026-10-12.html`은 이번 작업 이전 미추적 파일로 보존한다.

## 최종 커밋과 완료 범위

| 단위 | 커밋 | 내용 |
| --- | --- | --- |
| 계획 | `19fa8df3` | 범위·계약·소유권 |
| PR1 | `35e8a264` | 준비·인증·도구 성공 구분 및 연결 안내 |
| PR6 웹 | `1878a449` | 모바일 작업 결과·검색·원본 이동 |
| PR4 비밀 출력 | `2fdf4a4d` | 줄바꿈/부분 출력/ANSI Unicode/선형 마스킹 |
| PR2 runtime 통합 | `1dd6d16d` | 내부 scoped 입력 수신증 ID |
| PR2–6 통합 | `9d0ed76b` | 인증·권한·작업·이력·복구·native/PWA 인증된 제어 |

교차 crate의 Request/Principal/CloudRun/worker/operator 계약 변경은 컴파일 가능한 통합 커밋으로 묶었다. Root는 각 담당의 변경과 독립 CLI 리뷰 결과를 검토했고, 알려진 결함을 재현·수정한 최종 소스에서 위 검사를 통과했다. 승인된 PR1–6 제품 구현의 남은 작업은 없다. 실제 제공자 계정별 인증/OS sleep/공개 터널 E2E는 위 검증 범위의 한계로 남으며 구현 완료와 구분한다.
