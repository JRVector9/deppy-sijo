| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| medium | crates/agent-mcp/src/oauth.rs:271 | 긴 Location을 만들기 전에 승인 요청을 소모함 | 연결 승인이 유실되고 재시도도 실패 | 인코딩 후 길이 검증과 회귀 테스트 |
| medium | crates/agent-mcp/src/oauth.rs:384 | 갱신 scope를 문자열 그대로 비교함 | 같은 권한의 순서 변경·범위 축소가 거부됨 | 권한 집합과 원래 승인 범위로 검증 |

# 클라우드 에이전트 MCP 재리뷰 — 2026-09-27

> 후속 상태: 아래 두 OAuth 결함은 `36e987c9`에서 수정했다. [네 건 수정 결과](2026-09-27-cloud-agent-four-fixes.md)를 현재 상태로 참조한다. 아래 내용은 수정 전 재리뷰 기록이다.

## 범위와 결론

- 사용자 요청: 다섯 결함 수정 후 한 번 더 코드 리뷰.
- 실제 소스: `/Users/jr/Desktop/projects/deppy-sijo-cloud-agent-mcp`, `feat/cloud-agent-mcp`, HEAD `0d28fb1a`.
- 검토 범위: `1edfb957..0d28fb1a`의 소스 수정 전체. `c9d4fbb2`, `57c0bb80`의 Rust/Cargo 변경 및 관련 호출 경로를 검토했다. 마지막 OAuth 수정만 검토한 것이 아니다.
- 문서를 제외한 Codex CLI 소스 리뷰와 직접 검토를 수행했다. CLI가 제시한 두 P2를 실제 컴파일된 `agent_mcp::Server`에 대한 로컬 HTTP 요청으로 독립 재현했다.
- 확인된 추가 결함은 **medium 두 건**이다. 검토한 범위에서 critical/high 결함은 확인하지 못했다. 이 결과가 실제 Grok Bot 계정·공개 HTTPS 연결 성공을 보증하지는 않는다.
- 이번 요청에서는 제품 소스·테스트 수정, 커밋, release 빌드, GUI 실행/재실행, 공개 터널, push를 수행하지 않았다. 리뷰 보고서와 handoff만 갱신했다.

## Details

### 1. 승인된 리다이렉트가 응답 헤더 한도를 넘으면 승인이 유실됨

- 위치: `crates/agent-mcp/src/oauth.rs:271`의 승인/거부 완료 분기.
- 해당 분기는 `pending.remove`로 요청을 먼저 제거한 뒤 redirect URL에 `code`와 `state`를 추가한다.
- `crates/web-remote/src/http.rs:196`은 헤더 값이 8192바이트를 넘으면 쓰기 전에 오류를 반환한다. `crates/agent-mcp/src/server.rs:259`은 그 반환값을 무시하므로 클라이언트에는 HTTP 오류 응답조차 전달되지 않는다.
- 실제 재현: 등록 가능한 2048바이트 HTTPS redirect와 2048개의 `~` state로 승인 요청을 만들었다. `~`는 요청 URI에 그대로 넣어도 유효하므로 최초 HTTP 요청은 8KiB 제한 안에서 수락된다. 응답 URL의 form 인코딩은 각 `~`를 `%7E`로 확장하여 `Location`이 8192바이트를 넘는다.
- 로컬 승인 후 polling 첫 응답은 **0바이트**, 같은 요청 재시도는 **HTTP 400 `authorization_expired`**였다. 최초 등록·authorize·사용자 승인은 모두 성공했다.
- 영향: 유효한 등록/승인 조합에서 OAuth 연결 완료가 실패한다. 재시도로도 이미 제거한 승인 요청을 복구할 수 없다.
- 수정 방향: 승인 요청을 수락/소모하기 전에 최종 인코딩된 리다이렉트 길이를 공통 헤더 한도와 대조하고, 응답 생성 실패로 요청을 소모하지 않도록 한다. 성공과 거부 리다이렉트 모두 경계값을 검증한다. 실제 소켓 단절과 별개로, 수락한 요청의 응답이 구조적으로 쓰기 불가능한 상태를 없애야 한다.

### 2. refresh의 scope 문자열 비교가 정상 권한 요청을 거부함

- 위치: `crates/agent-mcp/src/oauth.rs:384`의 refresh grant 분기.
- authorization은 `deppy.input deppy.read`를 정상 수락한다. refresh는 input 권한이 있으면 오직 `deppy.read deppy.input` 문자열만 수락한다.
- 실제 재현: 정상 등록 → 로컬 승인 → S256 PKCE 코드 교환(HTTP 200)으로 받은 refresh token에 같은 순서의 `deppy.input deppy.read`를 전달하면 **HTTP 400 `invalid_scope`**가 나왔다.
- 같은 원래 권한의 읽기 전용 부분집합 `deppy.read`도 **HTTP 400 `invalid_scope`**였다. scope를 생략하면 동일한 token으로 **HTTP 200**이 나와 인증 정보나 resource 불일치가 원인이 아님을 확인했다.
- OAuth scope는 순서에 무관한 권한 집합이며, refresh 요청은 기존 승인 범위를 넘지 않는 범위를 요청할 수 있다. [RFC 6749 §3.3](https://www.rfc-editor.org/rfc/rfc6749.html#section-3.3), [§6](https://www.rfc-editor.org/rfc/rfc6749.html#section-6).
- 영향: 정상 OAuth 클라이언트가 scope 순서를 유지하거나 권한을 줄여 refresh할 때 연결 갱신이 실패한다.
- 수정 방향: scope를 권한 집합으로 파싱하고 원래 refresh 승인 범위의 부분집합인지 검증한다. 축소된 access token과 회전된 refresh token의 원래 승인 범위는 구별해야 한다. RFC §6에 따라 새 refresh token의 범위는 기존 refresh token과 동일하게 유지한다. 순서 변경·부분집합·미지 권한·권한 확대·scope 생략 회귀 테스트가 필요하다.

## 전달·세션·백그라운드 동작 재점검

- 상관 ID를 가진 `InputAdmitted`가 활성/비활성 warm runtime에서 소비되고 실제 PTY 수락 결과로 입력 수신증을 완료하는 경로를 다시 확인했다.
- 답변은 저장된 원래 세션 UUID를 따라 표시·이동하며, 답변 완료 순서에 따른 보존과 입력 감사 기록 분리가 유지되는 경로를 확인했다.
- 검토한 클라우드 입력/읽기/답변 경로에서 새 로컬 에이전트 실행·재실행 또는 자체 LLM 요청을 시작하는 동작은 확인하지 못했다. 입력이 허용된 기존 warm 세션에는 해당 화면이 현재 보이지 않아도 전달될 수 있다.
- 실제 OAuth HTTP → App → 임시 실제 셸 PTY → 명령 출력 → 클라우드 자신의 답변 테스트를 다시 실행했다. 사용자 제품 화면이나 사용자 에이전트 세션에는 입력하지 않았다.
- 이 경로의 추가 결함은 이번 검토에서 확인하지 못했다. 실제 외부 Grok Bot 계정/터널 검증은 여전히 별도 미실행 항목이다.

## 실제 실행한 검증

| 명령/검증 | 결과 | 로그 |
| --- | --- | --- |
| `cargo test -p agent-mcp` | 19 passed, exit 0 | `/tmp/deppy-cloud-repeat-mcp-tests-20260927.log` |
| `cargo test -p deppy-sijo --bin deppy-sijo cloud_agent` | 10 passed, exit 0 | `/tmp/deppy-cloud-repeat-app-tests-20260927.log` |
| Codex CLI 전체 수정 소스 리뷰 | P2 두 건, exit 0 | `/tmp/deppy-cloud-repeat-full-review-20260927.log` |
| 실제 MCP Server 로컬 HTTP 재현 | 두 결함 재현, 예상 결과 assertion 성공, exit 0 | `/tmp/deppy-cloud-repeat-probe-20260927.log` |

기존 테스트 29개는 통과했지만 위 두 경계 사례를 포함하지 않는다. 재현 프로그램의 성공은 결함을 확인했다는 의미이며 제품 동작이 정상이라는 뜻이 아니다. 전체 workspace 테스트 또는 실제 외부 Bot 검증을 통과했다고 주장하지 않는다.

재현 프로그램은 `/tmp/deppy-cloud-repeat-probe.rs`, 실행 파일은 `/tmp/deppy-cloud-repeat-probe`에 있다. 기존 debug rlib로 컴파일하고 실제 production Server를 임시 loopback 포트에 올렸다. 테스트 토큰/코드는 로그에 출력하지 않았고 제품 GUI를 띄우지 않았다.

```text
long redirect: redirect_bytes=2048, state_bytes=2048, first_status=0, response_bytes=0, retry_status=400, retry_error="authorization_expired"
refresh scope="deppy.input deppy.read": status=400, error="invalid_scope"
refresh scope="deppy.read": status=400, error="invalid_scope"
refresh scope="": status=200, error=null
```

## 남은 작업

이번 리뷰 요청은 완료했다. 새로 확인한 두 결함의 제품 수정과 회귀 테스트는 적용하지 않았다. 수정 작업을 이어갈 때 위 HTTP 사례를 먼저 실패 테스트로 추가한다. 사용자 허락 없이 Deppy를 재실행하지 않는다.

```sh
cd /Users/jr/Desktop/projects/deppy-sijo-cloud-agent-mcp
git status --short
cat docs/reviews/2026-09-27-cloud-agent-mcp-rereview.md
sed -n '255,290p' crates/agent-mcp/src/oauth.rs
sed -n '375,410p' crates/agent-mcp/src/oauth.rs
cat /tmp/deppy-cloud-repeat-probe.rs
cargo test -p agent-mcp
cargo test -p deppy-sijo --bin deppy-sijo cloud_agent
```
