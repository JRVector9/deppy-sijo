# deppy-sijo H트랙(MCP HTTP) PR 계획 v2 — VS Code MCP 구현 조사 반영

작성일: 2026-07-11 (Fable 5 서브에이전트 — microsoft/vscode main 실코드 조사 기반)
대상: main(bc85324) — 커넥터 온보딩 3종 완료, HTTP 트랙 미착수 상태
문서 성격: Build PR 계획서 (코드 변경 없음)

## 0. 조사 요약

**VS Code MCP 구조** (microsoft/vscode, 2026-07 main 기준):
- 상위 오케스트레이션: `src/vs/workbench/contrib/mcp/common/` — `mcpServer.ts`(서버 수명·도구 호출·재시도), `mcpServerConnection.ts`(상태머신 Stopped/Starting/Running/Error), `mcpServerRequestHandler.ts`(initialize 핸드셰이크·JSON-RPC 상관·페이지네이션), `mcpRegistry.ts`(신뢰 판정)
- HTTP transport 본체: `src/vs/workbench/api/common/extHostMcp.ts`의 `McpHTTPHandle` — Streamable HTTP + legacy SSE 폴백을 한 클래스에서 모드 상태머신(`Unknown → Http | SSE`)으로 처리
- OAuth 프리미티브: `src/vs/base/common/oauth.ts`(RFC 9728/8414 발견, RFC 7591 DCR, WWW-Authenticate 파서), 토큰/세션은 `mainThreadMcp.ts` + `extHostAuthentication.ts`의 `DynamicAuthProvider`(인증서버+리소스 쌍당 1개 동적 생성)
- 프로토콜 상수: `src/vs/platform/mcp/common/modelContextProtocol.ts:43` — `LATEST_PROTOCOL_VERSION = "2025-11-25"` (deppy 기준 스펙과 동일)

**핵심 발견 (계획에 반영)**:
1. VS Code는 initialize에 항상 최신 버전만 보내고, **응답의 protocolVersion을 검증하지 않는다**(관대 수용). 또 **MCP-Protocol-Version 헤더를 일반 요청에 보내지 않는다**(스펙 이탈 — 저장소 전체에서 인증 메타데이터 발견 요청에만 존재). 두 가지 모두 차용하지 않는다.
2. 세션 관리: `Mcp-Session-Id` 응답 헤더 캡처 → 이후 요청에 부착 → 세션 보유 중 400/404면 새 initialize로 재수립 후 **정확히 1회** 도구 호출 재시도(`mcpServer.ts:1293`의 `shouldRetry` + `allowRetry=false` 재귀) → 종료 시 DELETE.
3. 401/403 인증 사다리(`_fetchWithAuthRetry`): 메타데이터 발견→재시도 1회 / scope 챌린지 변경→재시도 1회 / 그래도 401이면 클라이언트 재등록(forceNewRegistration)→재시도 1회. 각 단계가 유한해서 무한 루프가 없다.
4. 클라이언트 등록 체인: 저장된 등록 → CIMD(AS가 `client_id_metadata_document_supported`면 호스팅 URL을 client_id로) → DCR → **사용자에게 client_id/secret 직접 입력 프롬프트**(`extHostAuthentication.ts:257-291`).
5. 신뢰 모델: 서버 정의 해시(`cacheNonce = McpServerLaunch.hash(launch)`)에 신뢰를 묶고, 정의가 바뀌면 재신뢰 프롬프트(`mcpRegistry.ts:214`). 토큰은 **동의한 시점의 서버 URL에 바인딩** — URL이 바뀌면 재동의(`mainThreadMcp.ts:368-370` 주석).

---

## 1. PR 재구성 개요

| 구 초안 | 신 계획 | 이유 |
|---|---|---|
| PR-H1 | **PR-H1** (유지·보강) | 협상 결과를 H2의 헤더 값으로 넘기는 연결 고리 추가 |
| PR-H2 | **PR-H2**(transport 코어) + **PR-H3**(UI·가져오기·proxy 통합) | VS Code도 transport(`extHostMcp`)와 통합(`mcpServer`/UI)을 분리. 코어는 목 서버로 헤드리스 검증 가능 — 완료 기준이 명확해짐 |
| PR-H3 | **PR-H4**(OAuth 발견+DCR+refresh, crates/auth 확장) + **PR-H5**(401 사다리+승인 UX) | 발견/DCR/refresh는 네트워크 프리미티브로 단독 테스트 가능. UX 통합은 transport(H2)와 UI(H3)에 의존하므로 마지막 |

의존 순서: H1 → H2 → H3(H2 의존) / H4(독립, H1·H2와 병행 가능) → H5(H2+H3+H4 의존).

---

## PR-H1: protocolVersion 협상 완화

**목표**: `crates/mcp/src/manager.rs:105`의 "2025-11-25 정확 일치" 게이트를 지원 목록 협상으로 완화. stdio·HTTP 공용 기반.

**범위**: `crates/mcp/src/manager.rs` (+ `lib.rs` re-export). transport/UI 무변경.

**구현 요점**:
- `SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"]` (최신 우선 정렬). initialize 요청에는 계속 최신(`PROTOCOL_VERSION`)만 전송.
  - 차용: VS Code도 요청에는 `MCP.LATEST_PROTOCOL_VERSION` 하나만 보낸다 — `mcpServerRequestHandler.ts:116`. 버전별 요청 분기 없음(서버가 자기 버전으로 응답하는 것이 스펙 협상 규칙).
- 응답 버전이 목록에 있으면 수용하고 `McpConnection`에 `negotiated_version: String` 보관 — H2에서 `MCP-Protocol-Version` 헤더 값으로 사용. 목록 밖이면 에러: `"MCP 서버 '{name}' protocolVersion 미지원: {server_version:?} (지원: 2025-11-25, 2025-06-18, 2025-03-26, 2024-11-05)"`.
  - VS Code의 "응답 버전 무검증 수용"은 차용하지 않음(§차용 안 함 #3).
- ~~initialize 응답 대기 중 5초 간격 진행 로그~~ → **H2로 이월** (2026-07-11). stdio는 request가 짧게 끝나 실익이 작고, HTTP long-poll/SSE 대기(H2)에서 "죽었는지 기다리는지" 구분의 필요성이 실질적으로 커진다. transport 공용 진행 로그로 H2에서 구현.
  - 차용: `mcpServerRequestHandler.ts:106-109`의 `IntervalTimer` 5초 "Waiting for server to respond to `initialize` request...".

**완료 기준**:
- 목 서버(기존 `/bin/sh` 스크립트 관례)가 4개 버전 각각으로 응답 → 모두 connect 성공, `negotiated_version` 일치.
- `"1999-01-01"` 응답 → 에러 메시지에 서버 버전과 지원 목록이 모두 포함.
- 기존 manager.rs 테스트 전부 무수정 통과(2025-11-25 경로 불변).

**리스크**: 구버전(2024-11-05) 서버와의 기능 차이 — deppy는 tools/list·tools/call만 쓰므로 실질 차이 없음. capabilities 차이는 기존처럼 무시. 낮음.

---

## PR-H2: Streamable HTTP transport 코어 (crates/mcp)

**목표**: ureq(sync) 기반 Streamable HTTP 클라이언트. `StdioClient`와 대칭인 `HttpClient`를 만들고 `LocalMcpManager`가 kind별로 선택. tokio 금지 관례 유지.

**범위**: `crates/mcp/src/http.rs`(신규: HTTP transport + SSE 파서), `manager.rs`(config enum화·connect 분기), `Cargo.toml`(ureq — 이미 workspace에 `ureq = "2"` 있음, `crates/mcp`에 추가만). UI/store 무변경.

**구현 요점**:

*요청/응답*
- POST 단일 JSON-RPC 메시지(2025-11-25 스펙: batch 전송 금지 — 설계문서 §1.5 명시), 헤더 `Content-Type: application/json`, `Accept: text/event-stream, application/json`.
  - 차용: `extHostMcp.ts:440-460` `_sendStreamableHttp`의 헤더 구성.
- 응답 분기: **202** → 바디 없음(notification 전송 성공). **application/json** → 단일 메시지 파싱. **text/event-stream** → SSE 이벤트를 순차 파싱, 요청 id의 response가 나올 때까지 소비(중간 notification/서버 request는 stdio 경로와 동일 규칙: notification 무시+debug, id 있는 서버 request는 -32601 회신). 그 외 content-type → 에러(§차용 안 함 #9: VS Code의 "일단 JSON으로 파싱 시도" 관대함은 미채택).
  - 차용: 분기 구조는 `extHostMcp.ts:513-546` `_handleSuccessfulStreamableHttp`.
- initialize 이후 모든 요청에 `MCP-Protocol-Version: {H1의 negotiated_version}` 헤더(스펙 필수). VS Code는 이를 생략하지만 따르지 않는다(§차용 안 함 #4).

*SSE 파서 (sync, 신규)*
- `Read` 기반 증분 파서: `data:` 멀티라인 조립, `event:`(기본 `message`), `id:`(**NUL(\0) 포함 시 무시**), `retry:`(숫자만), 빈 줄에서 dispatch, CR/LF/CRLF 모두 처리.
  - 차용: 필드 규칙은 `src/vs/base/common/sseParser.ts`(NUL id 무시 179-181행, retry 186행, dispatch 199행). 구현은 async ReadableStream 대신 sync `BufRead`로 재작성.
- deppy 강화(차용 아님·기존 관례 이식): 이벤트/라인 크기 상한(stdio `MAX_LINE_BYTES` 8MiB 관례), 누적 응답 바디 상한. VS Code에는 크기 상한이 없다.

*세션 (Mcp-Session-Id)*
- initialize 응답 헤더의 `Mcp-Session-Id` 캡처 → 이후 모든 요청에 부착.
  - 차용: `extHostMcp.ts:464-467` — "세션 헤더가 Streamable 모드의 가장 강한 신호".
- 세션 부착 요청이 **400 또는 404** → "세션 만료"로 분류, 새 initialize로 세션 재수립 후 실패한 요청을 **정확히 1회** 재시도. 404만이 스펙이지만 400도 포함(실서버 편차).
  - 차용: `extHostMcp.ts:481-491`(400 포함 근거: modelcontextprotocol/typescript-sdk#389 인용 주석) + `mcpServer.ts:1292-1297`(재시도는 `allowRetry` 플래그로 1회 한정).
- 연결 종료(drop) 시 `DELETE` + `Mcp-Session-Id` 전송 — 베스트에포트, 실패 무시, 이 경로에서는 인증 재시도 안 함.
  - 차용: `extHostMcp.ts:389-424` `close()`/`_closeSession`.
- connect-per-call 관례 유지: HTTP도 "connect(initialize+세션 수립) → 요청들 → drop(DELETE)"가 한 호출 단위. `discover_tools`/`call_tool`의 외부 API 불변.

*보안 정책*
- URL 검증: https 필수, `localhost`/루프백 IP만 http 허용 — `crates/auth/src/lib.rs`의 `validate_redirect_uri`와 동일 규칙(공용 함수로 추출 또는 복제).
- redirect: **ureq 자동 redirect 비활성(`redirects(0)`) + 수동 처리** — 최대 5회, Location이 http(s) 외 스킴이면 즉시 에러(fail-closed), **cross-origin이면 `Authorization`/`Mcp-Session-Id`(+cookie류) 헤더 제거**, 303(및 POST의 301/302)은 GET 전환. redirect 대상에도 deppy의 https(localhost 예외) 정책 적용(VS Code보다 엄격).
  - 차용: `extHostMcp.ts:334-342` 상수(`MAX_FOLLOW_REDIRECTS`, `ALLOWED_REDIRECT_PROTOCOLS`, `CROSS_ORIGIN_STRIPPED_HEADERS`)와 `_fetch`(853-901행)의 수동 redirect 루프. ureq 2의 자동 redirect는 자격 헤더 제거를 보장하지 않으므로 수동 전환이 안전.
- 로그: 요청/응답 trace 로그에서 Authorization 값 마스킹 + Bearer 토큰을 `RedactionService`에 등록해 전 로그 경로 일괄 마스킹(VS Code의 개별 `'***'` 마스킹보다 강함 — deppy 기존 관례).
  - 차용(마스킹 원칙): `extHostMcp.ts:847-849`.
- `User-Agent: deppy-sijo/{CARGO_PKG_VERSION}` 부착. 차용: `extHostMcp.ts:840`.

*타임아웃 (ureq 2 함정 주의)*
- ureq 2의 overall timeout은 SSE 스트리밍 바디를 30초에 절단한다 → agent에는 `timeout_connect`/`timeout_read`(SSE idle 타임아웃 겸용)만 설정하고, "요청 전체 30s" 상한은 비스트리밍(JSON) 경로의 호출측 deadline으로 구현. SSE 경로는 read idle + 총 바디 상한으로 방어. VS Code는 하드 타임아웃이 아예 없고 진행 로그(H1 차용분)로 대체한다 — deppy는 상한 유지.

**완료 기준**:
- std `TcpListener` 스레드 목 서버로: JSON 응답 왕복 / SSE 스트림 응답(멀티 이벤트, notification 섞임) / 202 / 세션 캡처·부착 / 400·404 → 세션 재수립 후 1회 재시도(2회째 실패는 에러) / DELETE 발신 / cross-origin redirect에서 Authorization·세션 헤더 소거 / 비-http 스킴 redirect 거부 / 크기 상한 / idle 타임아웃 — 전부 단위 테스트.
- `cargo tree -p mcp | grep -c tokio` = 0.
- Bearer 값이 어떤 에러 메시지/로그에도 평문 노출 없음(테스트로 고정).

**리스크**: sync SSE 읽기의 스레드/블로킹 수명 관리(호출 스레드에서 직접 읽으므로 상주 스레드는 불필요 — read timeout이 유일한 탈출구임을 테스트로 고정). 서버별 세션 편차(재수립 1회 재시도로 흡수). 중간 수준.

---

## PR-H3: HTTP 커넥터 통합 (mcp-store 활용 + UI + 가져오기 + proxy)

**목표**: kind='http' 서버를 UI에서 등록·발견·실행하고, 가져오기와 에이전트 브리지(deppy-mcp-proxy)까지 관통시킨다.

**범위**: `crates/app/src/ui/connectors.rs`, `crates/app/src/mcp_import.rs`, `crates/mcp-proxy/src/forwarder.rs`(+`cli.rs`), i18n 카탈로그. **mcp-store 마이그레이션 불필요** — `mcp_servers.kind`/`url` 컬럼 기보유(`crates/mcp-store/src/lib.rs:19,22`).

**구현 요점**:
- 추가 폼: 종류 선택(stdio/http). http 선택 시 URL 필드(H2의 https/localhost 검증을 저장 전 실행), command/args/env 필드는 숨김. 서버 카드에 "연결 테스트"(initialize+tools/list) 버튼과 결과 상태.
- **원격 신뢰 확인**: http 서버 최초 연결(연결 테스트/도구 발견/proxy 경유 첫 호출) 전에 1회 확인 모달 — "이 서버로 도구 호출 데이터가 전송됩니다: {url}". 이후 **url 편집 저장 시** 해당 서버의 `tool_permission_rules` Allow 규칙 초기화 + 재확인.
  - 차용(개념): VS Code의 정의-해시 신뢰 — `cacheNonce = McpServerLaunch.hash(launch)`(`discovery/installedMcpServersDiscovery.ts:108`)가 바뀌면 재신뢰 프롬프트(`mcpRegistry.ts:232-244`, `TrustedOnNonce`). **적응**: VS Code는 외부 파일(mcp.json)이 언제든 바뀌어 nonce 저장이 필요하지만, deppy 정의는 자체 UI/가져오기로만 바뀌므로 "편집 저장 시점 훅"으로 등가 구현 — 스키마 추가 없음. 도구 목록 캐시(mcp_tools)도 이 시점에 무효화(차용: `mcpTypes.ts:164-165` "nonce 변경 = tools 재조회 신호").
- 가져오기(`mcp_import.rs`): 현재 `"http" | "sse" | "streamable-http" | "streamable_http"` 일괄 스킵을 해제하되 — `http`/`streamable-http`/`streamable_http`(+`url`만 있는 항목)는 url 매핑으로 등록, **legacy `sse` kind는 계속 스킵**(사유 문자열을 "v1 예정"에서 "구 SSE transport 미지원"으로 교체, §차용 안 함 #1). 등록된 http 서버도 위 최초 연결 확인 대상.
- proxy(`forwarder.rs`): `ManagerToolForwarder`가 http config 수용 — 호출당 connect 관행 그대로(forwarder.rs 상단 주석의 수명 규약 유지). `cli.rs`에 서버 지정이 stdio 전제라면 kind 분기 추가. 승인 경로(`pending_approvals` IPC)는 transport 무관이므로 무변경.
- 도구 실행 승인은 기존 `audit::PermissionPolicy`(Allow/Deny/Ask + schema hash 재승인, `crates/audit/src/policy.rs`) 그대로 http 도구에 적용 — VS Code의 chat 도구 확인 흐름 대비 부족한 것이 없어(schema 변경 재승인은 오히려 deppy가 더 강함) 추가 차용 없음.

**완료 기준**:
- UI E2E(수동): http 서버 등록 → 최초 연결 확인 모달 → 도구 발견 → 도구 실행(Ask 다이얼로그) → 결과 표시. url 수정 저장 → Allow 규칙 리셋 확인.
- `mcp_import` 테스트: http 항목이 url로 등록되고 sse 항목만 스킵되는 케이스로 기존 `http_항목은_v1_사유로_건너뜀` 테스트 대체.
- proxy 단위 테스트: http 목 서버 백엔드로 tools/list·tools/call 중계.

**리스크**: egui 폼 상태 추가로 인한 connectors.rs 비대화(폼/카드 모듈 분리 검토, 단 리팩터링은 최소). url 변경 시 규칙 리셋의 사용자 놀람(모달 문구로 고지). 중간-낮음.

---

## PR-H4: OAuth 발견 + DCR + refresh (crates/auth 확장)

**목표**: 401 응답에서 출발해 토큰 획득까지의 네트워크 프리미티브를 crates/auth에 추가. 기존 PKCE flow(begin/complete/run_flow)와 keyring 규약(`{id}.refresh`) 재사용. UI/transport 연동은 H5.

**범위**: `crates/auth/src/`(신규 모듈: `www_authenticate.rs`, `discovery.rs`, `registration.rs`, `refresh.rs`), `flow.rs`(resource 파라미터 추가). ureq는 이미 의존(`crates/auth/Cargo.toml:16`).

**구현 요점**:
- **WWW-Authenticate 파서**: 다중 챌린지·quoted-string 내 콤마 처리, Bearer 챌린지의 `resource_metadata`/`scope` 파라미터 추출.
  - 차용: `src/vs/base/common/oauth.ts:1007-1103` `parseWWWAuthenticateHeader`(따옴표 인지 콤마 분할 → scheme/param 재조립 알고리즘 그대로 이식 가능).
- **PRM 발견 (RFC 9728)**: ①챌린지의 resource_metadata URL → ②`{origin}/.well-known/oauth-protected-resource{path}` → ③root. **응답의 `resource` 필드가 서버 URL과 정규화 후 정확히 일치하지 않으면 거부**(토큰 오발급 방지 핵심), 모든 실패를 수집해 종합 에러로 보고.
  - 차용: `oauth.ts:1198-1288` `fetchResourceMetadata`(1239-1243행의 RFC 9728 일치 강제, 에러 수집→AggregateError).
- **AS 메타데이터 (RFC 8414)**: ①`/.well-known/oauth-authorization-server` path-insertion → ②`/.well-known/openid-configuration` path-insertion → ③path-addition → ④전부 실패 시 기본 엔드포인트(`/authorize`,`/token`,`/register`) 폴백.
  - 차용: `oauth.ts:1348-1420` `fetchAuthorizationServerMetadata` + `oauth.ts:919-929` `getDefaultMetadataForUrl`.
- 발견 요청에서 커스텀 헤더(MCP-Protocol-Version 등)는 **대상이 서버와 same-origin일 때만** 부착(교차 출처 헤더 누출 방지).
  - 차용: `extHostMcp.ts:802-809`의 `sameOriginHeaders` + `oauth.ts:1216-1222`.
- **DCR (RFC 7591)**: `token_endpoint_auth_method: "none"`(공개 클라이언트), `grant_types`는 AS 지원 목록과 `["authorization_code","refresh_token"]`의 교집합, `redirect_uris`에 **루프백 임의 포트 형태와 고정 포트 형태를 함께 등록**(redirect URI 정확 일치를 요구하는 비스펙 서버 대비 — deppy 고정 포트 1개 선정, 기존 `LocalhostCallbackServer::bind`의 임의 포트에 "고정 포트 우선 시도 → 실패 시 임의 포트" 추가). 등록 응답의 client_id는 비밀이 아니므로 credentials 메타데이터(SQLite)에, client_secret이 오면 keyring `{id}.dcr`에.
  - 차용: `oauth.ts:934-1000` `fetchDynamicRegistration`(auth method none, grant 교집합, `DEFAULT_AUTH_FLOW_PORT 33418` 고정 포트 주석 937-942행).
- **flow.rs 확장**: authorize URL·token 교환·refresh 교환 3곳 모두에 `resource` 파라미터(RFC 8707) 첨부.
  - 차용: `extHostAuthentication.ts` DynamicAuthProvider의 674-676행(authorize)·761-763행(token)·816-818행(refresh) 3곳 동일 패턴.
- **refresh**: oauth2 5의 `exchange_refresh_token`(ureq feature) 사용. 정책 — 만료 **5분 전** 선제 갱신(만료 시각은 credentials 테이블 메타데이터로, 비밀 아님), credential id 단위 **mutex single-flight**(동시 도구 호출이 refresh를 중복 발사하지 않게), **refresh 실패 시 access+refresh 폐기 → "재승인 필요" 상태 반환**.
  - 차용: DynamicAuthProvider `getSessions`의 5분 마진(535행)과 실패 시 폐기(538-541, 553-555행). single-flight는 deppy 방식(VS Code는 단일 스레드 이벤트 루프 + provider 단위 Sequencer로 암묵 해결).
- keyring 규약 불변: access = credential id, refresh = `refresh_entry_id()`, 삭제 시 두 entry 모두 정리.

**완료 기준**:
- 스레드 목 AS/RS로: 발견 3경로+기본 폴백 각각 / RFC 9728 resource 불일치 거부 / DCR 성공·미지원(404)·거부 / refresh 성공·invalid_grant 폐기 / 2-스레드 동시 refresh가 1회만 발사 — 전부 단위 테스트.
- WWW-Authenticate 파서: RFC 예제 + quoted comma + 다중 챌린지 케이스.

**리스크**: 실서버 발견 편차(3단계+기본 폴백과 에러 수집 보고로 흡수). oauth2 5 typestate에 refresh 클라이언트 조립 추가. 중간.

---

## PR-H5: Bearer 연동 + 승인 UX (401 사다리, 커넥터 흡수)

**목표**: H2 transport와 H4 프리미티브를 묶어 "401 → 브라우저 승인 → Bearer → 재시도"를 완성하고, 별도 OAuth 폼을 서버 카드 흐름으로 흡수.

**범위**: `crates/mcp/src/http.rs`(401 처리 훅), `crates/app/src/ui/connectors.rs`(OAuth 폼 제거·카드 상태·폴백 다이얼로그), credentials 연계.

**구현 요점**:
- **401/403 인증 사다리** (각 단계 1회 한정, 무한 루프 구조적 봉쇄):
  1. 인증 메타데이터 없음 → H4 발견 체인 실행 → 토큰 확보 → 원요청 재시도 1회
  2. 메타데이터 있음 + WWW-Authenticate의 scope 챌린지가 기존과 다름 → scope 갱신 → 새 토큰 → 재시도 1회
  3. Authorization을 붙였는데도 401/403 → 클라이언트 등록 폐기 + 재등록(DCR부터) → 재시도 1회
  - 차용: `extHostMcp.ts:796-837` `_fetchWithAuthRetry` 사다리 전체(403도 인증 대상으로 취급하는 `isAuthStatusCode` 포함) + `AuthMetadata.update`(1003-1011행)의 scope 챌린지 비교, `mainThreadMcp.ts:315-323`의 forceNewRegistration(등록 삭제 후 재생성).
- **브라우저 승인 진입 동의**: 최초 브라우저를 열기 전 다이얼로그 — "MCP 서버 '{name}'이(가) {인증 서버 authority}에 인증을 요구합니다. 브라우저로 승인할까요?".
  - 차용: `mainThreadMcp.ts:551-572` `loginPrompt` — 서버가 임의 인증 서버로 사용자를 보내는 것을 막는 마지막 게이트.
- **토큰 방출 URL 바인딩**: credential 메타데이터에 동의 시점의 서버 URL 저장. Bearer 부착 전 현재 `mcp_servers.url`과 비교 — 다르면 부착 거부 + 재동의 요구.
  - 차용: `mainThreadMcp.ts:368-370` 주석("A token is only released to a server whose current URL matches the one the user consented to"). H3의 url 변경 규칙 리셋과 한 쌍.
- **서버 카드 상태 모델**: 실패를 "에러"와 "**승인 필요**"로 구분. 401 사다리가 사용자 개입 지점에 도달하면 에러가 아니라 승인 필요 상태 + [브라우저로 승인] 버튼 → 클릭 시 백그라운드 스레드에서 H4 flow(기존 connectors.rs의 `OAuthStatus` + mpsc 배선 재사용) → 완료 후 자동 재시도.
  - 차용: `extHostMcp.ts:741-743` — 인증 필요 시 Error가 아닌 `Stopped(reason: 'needs-user-interaction')`로 전이해 UI가 별도 어포던스를 그리게 하는 상태 분리.
- **수동 client_id 폴백**: DCR이 미지원/실패면 카드에서 client_id(+선택 client_secret) 입력 다이얼로그 → 이후 동일 PKCE flow. 기존 OAuth 폼(auth_url/token_url/client_id 수동 입력)은 **제거** — auth/token URL은 이제 발견으로 얻으므로 사용자 입력이 필요 없고, 남는 것은 client_id 폴백뿐.
  - 차용: `extHostAuthentication.ts:265-291` — DCR 시도 → 실패 warn 로그 → `$promptForClientRegistration`(사용자 입력) 체인.
- 획득 토큰은 기존 `OAuthCredentialStore` 경로로 keyring 저장(H4 규약), `RedactionService` 등록.

**완료 기준**:
- 목 보호 서버 E2E(자동): 401(+resource_metadata) → PRM → AS 메타데이터 → DCR → PKCE(테스트는 브라우저 대신 콜백 직접 호출) → Bearer → 200. scope 챌린지 변경 → 1회 재시도. 재등록 사다리 1회 후 종료(3연속 401이면 최종 에러).
- URL 바인딩: url 변경 후 Bearer 부착 거부 테스트.
- UI 수동 시나리오: 승인 필요 카드 → 브라우저 승인 → 도구 실행 성공 / DCR 실패 → client_id 입력 → 성공. OAuth 폼 부재 확인.
- **실서버 검증: agent-match(mcp.ahto.city) 연결 성공.**

**리스크**: 사다리(H5)와 세션 재수립(H2)의 중첩 — 순서 규정 필요(**인증 사다리를 먼저 해소한 뒤 세션 재수립 판단**; VS Code도 4xx 폴백 판정에서 401/403을 제외해 같은 순서를 강제 — `extHostMcp.ts:470-475`). 브라우저 왕복 동안의 UI 응답성(기존 백그라운드+mpsc 패턴으로 기해결). 중간.

---

## 차용하지 않기로 한 것 (VS Code 조사 결과 중)

1. **Legacy HTTP+SSE transport 폴백** (`extHostMcp.ts` `HttpMode.SSE`/`_attachSSE`): 구스펙(2024-11-05) HTTP 서버용. 세션 수명 = 상주 GET 스트림이라 sync 환경에서 상주 스레드가 필요하고 connect-per-call 모델과 상충. 생태계가 Streamable로 이전 완료 단계. 대신 **폴백 신호 감지만 차용** — POST가 4xx(401/403 제외)이거나 POST 응답 SSE에 `endpoint` 이벤트가 오면(`extHostMcp.ts:470-479, 523-528`) "구 SSE transport 서버 — 미지원" 명시 에러로 안내(H2).
2. **GET backchannel + Last-Event-ID resume + 재접속 백오프** (`_attachStreamableBackchannel`): 서버발 비동기 알림(list_changed 등)의 소비처가 deppy에 아직 없고, 호출 단위 세션이라 스트림이 짧다. 후속 도입 시 규칙만 기록해 둠: 백오프 `min(retry*1s, 30s)`, SSE `retry` 필드 존중, **content-type이 실제 event-stream일 때만 재시도 카운터 리셋**(오작동 서버 폭주 방지, 598-601행), 4xx면 조용히 비활성화("MAY" 기능).
3. **protocolVersion 무검증 수용**: VS Code는 initialize 응답 버전을 아예 검사하지 않는다(저장소 전체에 검증 코드 부재). deppy는 transport 엄격 검증 관례(§1.5, stdout strictness)와 일관되게 지원 목록 검증 유지.
4. **MCP-Protocol-Version 헤더 생략**: VS Code는 인증 메타데이터 발견 요청 외에는 이 헤더를 보내지 않는다(스펙 이탈). 2025-11-25 스펙은 initialize 후 필수 — deppy는 전송.
5. **엔터프라이즈 XAA/ID-JAG, 다계정 세션 선택, 워크스페이스 신뢰**(`mainThreadMcp.ts` enterpriseManaged 경로 등): 단일 사용자 데스크톱 앱에 과설계.
6. **CIMD(Client ID Metadata Documents)** (`mainThreadAuthentication.ts:176-178` — AS가 지원하면 호스팅된 메타데이터 URL을 client_id로 사용, DCR 생략): deppy가 통제하는 https 정적 JSON 호스팅이 선행 조건. 2025-11-25 스펙이 CIMD 중심이므로 **기각이 아닌 보류** — 도메인 확보 시 H5 후속 PR로(등록 체인상 "저장된 등록 → CIMD → DCR → 수동 입력"의 두 번째 슬롯에 끼워 넣는 구조는 H5에서 이미 호환).
7. **다중 인증 플로우 폴백**("다른 방법으로 시도" — URL handler↔루프백 전환, `extHostAuthentication.ts:572-604`): deppy는 커스텀 URI scheme 미등록, 루프백 콜백 단일 플로우로 충분.
8. **일괄 신뢰 프롬프트**(`McpStartServerInteraction` — 동시 기동 서버들의 신뢰 질문을 다이얼로그 1개로 병합): deppy는 서버 연결이 개별 UI 동작이라 병합 상황이 없음.
9. **content-type 무시 JSON 재파싱**(`extHostMcp.ts:539-544` isJSON 시도): 관대한 파싱은 deppy의 엄격 transport 검증 관례와 상충 — 명시된 content-type만 수용.

## 참고: 근거 경로 요약

- deppy: `crates/mcp/src/{manager,transport,proxy}.rs`, `crates/mcp-proxy/src/forwarder.rs`, `crates/auth/src/{lib,flow,callback}.rs`, `crates/mcp-store/src/lib.rs`, `crates/audit/src/policy.rs`, `crates/app/src/{mcp_import.rs,ui/connectors.rs}`, 루트 `Cargo.toml`(ureq 2 / oauth2 5-ureq), 설계문서 `ai_agent_workspace_final_architecture_v2_5_FINAL.md` §1.5
- VS Code(main): `src/vs/workbench/api/common/extHostMcp.ts`, `src/vs/base/common/{oauth,sseParser}.ts`, `src/vs/platform/mcp/common/modelContextProtocol.ts`, `src/vs/workbench/contrib/mcp/common/{mcpServerRequestHandler,mcpServer,mcpServerConnection,mcpRegistry,mcpTypes}.ts`, `src/vs/workbench/contrib/mcp/common/discovery/installedMcpServersDiscovery.ts`, `src/vs/workbench/api/browser/{mainThreadMcp,mainThreadAuthentication}.ts`, `src/vs/workbench/api/common/extHostAuthentication.ts`
