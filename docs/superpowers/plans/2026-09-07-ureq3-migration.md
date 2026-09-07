# ureq 3.4 Migration Implementation Plan

> **For agentic workers:** Execute this plan inline, task-by-task; parent has already assigned this lane. Steps use checkbox syntax for tracking.

**Goal:** ureq 3.4.0으로 모든 직접 HTTP 소비자를 이관하고 오류 본문 및 SSE idle 계약을 보존한다.

**Architecture:** Native ureq 3 request/response API로 소비자를 변경한다. 작은 공유 http-client crate가 default connector의 TLS/proxy 위에 idle transport wrapper만 추가한다. 기존 caller의 오류 분류, cap, retry/redirect 및 secret 경계는 유지한다.

**Tech Stack:** Rust 1.96.1, ureq 3.4.0, std synchronous I/O, existing loopback test fixtures.

---

### Task 1: RED 계약과 transport

**Files:** Create `crates/http-client/Cargo.toml`, `crates/http-client/src/lib.rs`; modify root `Cargo.toml` and lockfile.

- [ ] 기존 PR141 CI의 auth13/mcp21 compile 오류를 baseline으로 기록한다. GUI lane 종료 후 `cargo check -p auth -p mcp --all-targets --locked`로 local RED를 확보한다.
- [ ] 아래 공개 API를 호출하는 loopback 회귀를 먼저 작성한다. 서버는 40ms 간격으로 총 12 bytes를 보내고 idle은 200ms로 설정한다. 전체 480ms 동안 `read_to_end`가 정확히 12 bytes를 반환해야 한다. 별도 서버는 header 이후 600ms 동안 무응답이고 reader는 200ms idle에서 에러를 반환해야 한다. 서버/클라이언트는 loopback TcpListener/TcpStream이며 서버 thread는 종료 시 join한다.

```rust
pub fn agent_with_idle_timeouts(
    config: ureq::config::Config,
    read_idle: std::time::Duration,
    write_idle: Option<std::time::Duration>,
) -> ureq::Agent;
```

- [ ] transport의 timeout 제한 단위 회귀를 작성한다. incoming 50ms/global + idle200ms는 50ms/global을 유지하고, incoming NotHappening + idle200ms는 200ms/RecvBody가 된다. `is_tls=true` fake transport의 flag와 buffer도 위임되는지 검사한다.
- [ ] Rust 실행권을 확보한 뒤 RED를 실행한다: `cargo test -p http-client --locked -- --test-threads=1`. 아직 구현되지 않은 idle 계약이 실패하는 것을 확인한다.
- [ ] 최소 wrapper를 구현한다. `DefaultConnector::default().chain(IdleConnector { read_idle, write_idle })`와 `DefaultResolver::default()`를 `Agent::with_parts`에 넘긴다. `IdleTransport<T>`는 `Transport`의 buffers/transmit_output/await_input/is_open/is_tls를 구현한다. 각 I/O 호출의 `NextTimeout.after`와 idle의 min을 전달하고 짧은 기존 reason은 보존한다.
- [ ] 같은 회귀를 GREEN으로 실행하고 `cargo clippy -p http-client --all-targets --locked -- -D warnings`를 실행한다.

### Task 2: OAuth native API와 오류 본문

**Files:** `crates/auth/src/lib.rs`, `http.rs`, `discovery.rs`, `registration.rs`, `slack.rs`; root `Cargo.toml`.

- [ ] `oauth_http_agent`의 기존 redirect=0 및 전체/connect 제한을 ConfigBuilder로 이관한다. `http_status_as_error(false)`를 설정한다.
- [ ] `BoundedOAuthHttpClient::call`은 허용 method(GET/POST) 검증 후 `ureq::http::Request::builder().method(...).uri(...)`로 요청을 만들고 header를 복사한다. body를 보존하여 `agent.run(request)`를 호출한다. 모든 HTTP status는 기존 oauth2 response builder로 옮기되 bounded read 및 정적 오류 문자열을 유지한다.
- [ ] discovery의 status>=400 오류, 300..400 redirect 거부를 명시한다. registration은 status404→Unsupported, 400..500→bounded rejection_reason, >=500→generic error를 유지한다. Slack도 error status가 성공 JSON으로 처리되지 않게 직접 분기한다.
- [ ] `.set`→`.header`, `.send_string/.send_bytes`→`.send`, `response.header`→`headers().get(...).and_then(|v|v.to_str().ok())`, response reader→`into_body().into_reader()`로 이관한다. status 숫자는 `as_u16()`이다.
- [ ] `cargo test -p auth --locked -- --test-threads=1`로 redirect, DCR4xx, token4xx, cap 및 redaction 회귀를 확인한다. 실패한 계약은 먼저 좁은 RED로 고정 후 수정한다.
- [ ] oauth2 builtin ureq adapter feature를 제거하고 모든 flow/refresh가 기존 BoundedOAuthHttpClient를 계속 사용하는지 확인한다.

### Task 3: MCP 동적 요청과 SSE

**Files:** `crates/mcp/Cargo.toml`, `crates/mcp/src/http.rs`.

- [ ] helper response type은 `ureq::http::Response<ureq::Body>`로 변경한다. request는 `ureq::http::Request<SensitiveBytes>` 또는 borrowed body request를 governor closure 안에서 구성하여 secret body의 기존 소거 lifetime을 유지한다.
- [ ] `attach_common_headers`는 http request builder/header map을 대상으로 유지한다. version은 항상 붙이고 bearer/session은 기존 same-origin gate 안에서만 붙인다.
- [ ] `exchange_once`는 status-as-error(false)의 응답에서 3xx→기존 수동 redirect, 4xx/5xx→classify_error_status, 2xx→기존 parsing으로 분기한다. transport error는 기존 unknown delivery/redaction을 유지한다.
- [ ] `send_with_deadline`은 clone한 Agent와 request를 bounded governor closure로 넘겨 `agent.run(request)`를 실행한다. body를 중복 복사하거나 새 unbounded sender를 만들지 않는다.
- [ ] streaming agent에는 global/body-total timeout을 설정하지 않는다. shared wrapper read/write idle을 기존 request_timeout으로 설정한다. session DELETE는 ConfigBuilder request override로 기존 짧은 상한을 적용한다.
- [ ] `cargo test -p mcp --locked -- --test-threads=1`로 기존 session401/400, unknown delivery, redirect303, same/cross-origin, body cap, header/body drip, SSE idle을 실행한다. 진행 중인 SSE가 idle보다 오래 유지되는 새 회귀도 실행한다.

### Task 4: 앱/Push native API

**Files:** `crates/app/Cargo.toml`, `crates/app/src/llm_proxy.rs`, `local_llm.rs`, `status_feed.rs`, `codex_backend_usage.rs`; `crates/web-remote/src/push.rs`.

- [ ] 단순 fetch의 builder/headers/body reader를 native3으로 변경하고 기존 byte cap을 그대로 사용한다. nonstream agent의 전체 timeout은 timeout_global(Some(duration))으로 설정한다.
- [ ] LLM streaming은 shared idle agent를 사용한다. upstream 오류 본문이 필요한 agent에는 status-as-error(false)를 설정하고 status>=400에서 기존 bounded 에러 설명을 만든다. normal SSE는 total-body timeout을 갖지 않는다.
- [ ] Web Push는 status-as-error(false) agent를 사용하고 모든 HTTP 응답의 numeric status를 반환한다. network error만 TransportError로 축약한다.
- [ ] `cargo test -p deppy-sijo --bin deppy-sijo --locked llm_proxy -- --test-threads=1`, `local_llm`, `status_feed`, `codex_backend_usage`를 실행한다. `cargo test -p web-remote --locked -- --test-threads=1`도 실행한다.

### Task 5: 검증, 리뷰, replacement PR

**Files:** `docs/CODEX_HANDOFF.md`, implementation files, `Cargo.lock`.

- [ ] `cargo fmt --all --check`; `git diff --check`; `cargo run --locked -p xtask -- check-boundary`.
- [ ] `cargo deny check bans licenses sources`; `cargo audit`; `cargo tree -i ureq`로 v3 dependency graph 및 불필요한 v2 adapter 제거를 확인한다.
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings`; `cargo test --workspace --locked -- --test-threads=1`.
- [ ] Linux exact source archive의 Relay strict clippy/55tests/release build를 실행한다.
- [ ] Codex CLI static diff review를 수행하고 C/H/M 지적을 수정 후 필요한 회귀를 재실행한다. 특히 TLS flag, idle-vs-total, status/body 분류, sensitive body lifetime을 리뷰한다.
- [ ] 한국어 Conventional Commit, push 및 replacement PR을 만든다. PR 설명은 실제 호환성 문제와 유지한 계약, 실제 검증 결과를 적는다. 검증하지 않은 테스트는 PASS로 기록하지 않는다.
- [ ] 준비된 replacement PR로 #141을 대체 처리하고 parent에게 URL/HEAD/검증/남은 제한을 보고한다. workstep 일지와 handoff를 갱신한다.
