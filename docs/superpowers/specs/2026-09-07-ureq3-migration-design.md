# ureq 3.4 HTTP 이관 설계

## 목적과 범위

PR #141의 단순 의존성 변경에서 발생하는 auth/mcp 컴파일 오류를 해결하고 모든 직접 HTTP 소비자를 ureq 3.4.0으로 이관한다. OAuth/DCR 오류 본문, MCP 인증/세션 상태 분류, redirect 자격 헤더 정책, bounded response, secret redaction과 SSE idle timeout 계약을 보존한다. 기준 main은 `c9bda3932d2475fdebdda216c78abae3dd22692b`이다. 앱 실행 및 외부 배포는 범위에 포함하지 않는다.

## 선택한 구조

ureq 3의 native API를 호출부에서 사용한다. 응답 타입은 `ureq::http::Response<ureq::Body>`이며 body는 `into_body().into_reader()`로 기존 bounded reader에 넘긴다. 헤더는 `headers().get(name).and_then(|v| v.to_str().ok())`, 상태는 `status().as_u16()`으로 읽는다. 동적 method를 쓰는 OAuth/MCP는 `ureq::http::Request`와 `Agent::run`을 사용하여 GET/POST builder typestate 차이를 제거한다.

대안으로 v2 호환 facade 전체를 만드는 방법은 오류/타입 계층을 불필요하게 늘린다. SSE를 별도 reader thread로 감싸는 방법은 취소·join·socket 수 제한을 다시 설계해야 한다. 따라서 공유 `http-client` crate는 idle transport 기능만 제공한다.

## SSE idle 시간

`crates/http-client`의 `agent_with_idle_timeouts(config, read_idle, write_idle)`은 기본 connector 뒤에 transport wrapper를 연결한다. Wrapper는 각 `await_input`에 전달되는 `NextTimeout.after`를 read idle 상한으로 제한한다. write idle이 지정되면 `transmit_output`도 같은 방식으로 제한한다. 더 짧은 ureq global/per-call/phase 제한은 절대 늘리지 않는다. 입력 버퍼, is_open, is_tls는 내부 transport에 그대로 위임한다. 기본 connector의 TLS와 proxy 처리는 유지한다.

MCP에는 read/write idle을 모두 설정하고 기존 bounded sender governor 및 JSON body wall-clock deadline을 유지한다. LLM SSE에는 기존 read idle만 설정한다. `timeout_recv_body`는 SSE에서 설정하지 않는다. 따라서 전체 진행 시간이 idle 상한보다 길어도 데이터가 계속 도착하면 스트림은 유지되고, 입력이 중단되면 idle timeout으로 끝난다. transport API는 upstream에서 unversioned로 제공하므로 `ureq = "=3.4.0"`으로 고정한다.

## HTTP 오류와 redirect

OAuth/DCR 및 MCP agent에는 `http_status_as_error(false)`를 설정하고 4xx/5xx를 직접 분류한다. 응답 본문과 WWW-Authenticate를 확보한 상태로 기존 오류 파서를 호출한다. MCP tools/call은 3xx나 불확실한 전송에서 자동 재전송하지 않는다. OAuth redirect는 계속 0이며 discovery/DCR/refresh/token은 기존 직접 응답 정책을 유지한다. MCP는 기존 same-origin 비교 및 cross-origin 자격 헤더 소거를 유지한다.

그 외 단순 fetch는 기본 status-as-error를 유지할 수 있다. LLM upstream 오류 본문이 필요한 경로만 false로 두고 response status를 명시적으로 분기한다. Web Push는 모든 HTTP 상태를 반환하는 기존 transport 계약을 보존한다.

## 파일 책임

- `Cargo.toml`, `Cargo.lock`: ureq exact 3.4.0, 새 http-client member/dependency, 사용하지 않는 oauth2 ureq adapter feature 제거.
- `crates/http-client/Cargo.toml`, `src/lib.rs`: idle transport 구현과 deterministic/loopback 회귀.
- `crates/auth/src/{lib,http,discovery,registration,slack}.rs`: OAuth native API와 status/body 보존.
- `crates/mcp/Cargo.toml`, `crates/mcp/src/http.rs`: HTTP request/response 이관, 공통 idle transport 사용.
- `crates/app/Cargo.toml`, `crates/app/src/{llm_proxy,local_llm,status_feed,codex_backend_usage}.rs`: upstream HTTP API 및 SSE 이관.
- `crates/web-remote/src/push.rs`: HTTP status를 그대로 반환하는 Web Push 이관.

## 검증

새 transport의 RED 회귀는 무응답 idle 종료, idle보다 긴 정상 chunk 스트림, 기존 deadline 보존, TLS flag와 buffer delegation이다. 기존 OAuth 4xx/redirect/body cap/secret 검사를 모두 실행한다. MCP의 401/DCR/303/cross-origin/unknown delivery/header drip/body drip/SSE idle 검사를 유지한다. LLM 오류 본문 및 stream cap 검사도 실행한다. 마지막에는 fmt/boundary/diff, audit/deny, strict workspace clippy, 직렬 전체 workspace tests와 Linux Relay 게이트를 실행한다. GitHub Actions 미실행은 PASS로 표기하지 않는다.

## 실행 제약

설계와 소스/RED 회귀 준비는 즉시 진행한다. 현재 GUI lane이 Rust 게이트를 소유하므로 그 종료 신호 전에는 새 cargo 명령을 시작하지 않는다. 독립 `/private/tmp/deppy-ureq3-20260907` worktree와 별도 target을 사용하며 기존 사용자 앱을 실행하지 않는다. replacement PR을 준비한 후 #141을 대체 처리한다.
