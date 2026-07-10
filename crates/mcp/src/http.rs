//! Streamable HTTP MCP transport (H트랙 PR-H2, 기준 스펙 2025-11-25).
//! ureq(sync) 기반 — tokio 금지 관례. POST 단일 JSON-RPC 메시지를 보내고
//! 응답을 202(무바디) / application/json(단일 메시지) / text/event-stream(SSE)로
//! 분기 처리한다 (VS Code extHostMcp.ts `McpHTTPHandle` 분기 구조 차용).
//!
//! - 세션: initialize 응답의 `Mcp-Session-Id` 캡처 → 이후 요청 부착. 세션 부착
//!   요청이 400/404면 새 initialize로 재수립 후 **정확히 1회** 재시도. drop 시
//!   DELETE(베스트에포트).
//! - 보안: https 필수(localhost/루프백만 http), ureq 자동 redirect 비활성 후
//!   수동 최대 5회 — cross-origin이면 Authorization/Mcp-Session-Id 소거,
//!   비-http(s) 스킴 fail-closed. Bearer 값은 Debug/에러 문자열에 비노출.
//! - 타임아웃: ureq 2의 agent 전체 timeout은 SSE 스트리밍 바디를 중간 절단하는
//!   함정이 있어 connect/read/write timeout만 설정한다. read timeout이 SSE idle
//!   timeout을 겸하고, 비스트리밍(JSON) 바디는 호출측 deadline으로 상한한다.

use std::io::Read;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use secret::SecretString;
use serde_json::{Value, json};
use url::{Host, Url};

use crate::manager::{PROTOCOL_VERSION, negotiate_protocol_version};

/// SSE 한 라인 최대 길이 — stdio `MAX_LINE_BYTES` 8MiB 관례 이식.
const MAX_SSE_LINE_BYTES: usize = 8 * 1024 * 1024;
/// SSE 이벤트 하나의 data 누적 상한 (멀티라인 data 조립 크기) — 같은 8MiB 관례.
const MAX_SSE_DATA_BYTES: usize = 8 * 1024 * 1024;
/// SSE 스트림 누적 수신 상한 — 요청 응답이 나올 때까지 소비하는 총량 방어.
/// 단일 메시지 상한(8MiB)보다 커야 중간 notification들 + 응답을 수용한다.
const MAX_SSE_STREAM_BYTES: usize = 64 * 1024 * 1024;
/// application/json 응답 바디 상한 (stdio 라인 상한과 동일 관례).
const MAX_JSON_BODY_BYTES: usize = 8 * 1024 * 1024;
/// 에러 메시지에 싣는 서버 텍스트 snippet 최대 문자 수 (transport.rs 관례).
const ERROR_SNIPPET_CHARS: usize = 200;
/// 수동 redirect 추적 상한 (VS Code `MAX_FOLLOW_REDIRECTS` 차용).
const MAX_REDIRECTS: usize = 5;
/// 응답 대기 진행 로그 간격 — H1에서 이월된 항목 (VS Code
/// mcpServerRequestHandler.ts의 5초 IntervalTimer 차용).
const PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(5);
/// drop 시 세션 DELETE의 전체 timeout — DELETE는 스트리밍이 없어 ureq 전체
/// timeout이 안전하고, drop이 request_timeout만큼 블록되지 않게 짧게 상한.
const SESSION_DELETE_TIMEOUT: Duration = Duration::from_secs(5);

/// Streamable HTTP MCP 서버 설정 (§11.4 kind='http' 행에 대응 — H2).
/// stdio용 `McpServerConfig`는 기존 소비처(app/proxy)가 struct literal로 생성해
/// 필드를 추가할 수 없으므로, HTTP는 별도 config 타입으로 분리한다.
pub struct McpHttpServerConfig {
    pub name: String,
    /// 서버 URL — https 필수, http는 localhost/루프백만 (`validate_mcp_url`).
    pub url: String,
    /// Authorization Bearer 토큰. env secret 관례처럼 값은 호출측이 해석해
    /// 넘긴다. RedactionService 등록은 호출측 몫(H3/H5) — 여기서는 Debug/에러
    /// 문자열 비노출만 보장한다.
    pub bearer: Option<SecretString>,
}

impl std::fmt::Debug for McpHttpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpHttpServerConfig")
            .field("name", &self.name)
            .field("url", &self.url)
            .field("bearer", &self.bearer.as_ref().map(|_| "REDACTED"))
            .finish()
    }
}

impl Clone for McpHttpServerConfig {
    /// SecretString은 우발 복제 방지를 위해 Clone을 제공하지 않으므로 명시적으로 재포장한다.
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            url: self.url.clone(),
            bearer: self
                .bearer
                .as_ref()
                .map(|bearer| SecretString::new(bearer.expose().to_owned())),
        }
    }
}

/// MCP 서버 URL 정책 검증: https 필수, http는 localhost/루프백만 허용.
/// crates/auth `validate_redirect_uri`와 동일 규칙 (파일 스코프 분리로 복제).
/// H3의 서버 등록 폼이 저장 전 검증에도 쓴다.
pub fn validate_mcp_url(input: &str) -> anyhow::Result<()> {
    parse_validated_url(input).map(|_| ())
}

fn parse_validated_url(input: &str) -> anyhow::Result<Url> {
    let parsed = Url::parse(input)
        .map_err(|error| anyhow::anyhow!("MCP 서버 URL 파싱 실패: {input} ({error})"))?;
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" => {
            let is_loopback = match parsed.host() {
                Some(Host::Domain(domain)) => domain == "localhost",
                Some(Host::Ipv4(ip)) => ip.is_loopback(),
                Some(Host::Ipv6(ip)) => ip.is_loopback(),
                None => false,
            };
            if is_loopback {
                Ok(parsed)
            } else {
                bail!("http MCP 서버 URL은 localhost/루프백만 허용: {input}")
            }
        }
        other => bail!("MCP 서버 URL scheme 불허: {other} ({input})"),
    }
}

/// exchange 한 번의 성공 결과.
enum Outcome {
    /// 202 Accepted — notification/response 전송 성공 (바디 없음).
    Accepted,
    /// 요청 id에 대응하는 JSON-RPC response의 result.
    Result(Value),
}

/// exchange 실패 분류 — 세션 만료만 재시도 대상으로 구분한다.
enum ExchangeError {
    /// 세션을 부착한 요청이 400/404 — 세션 만료로 분류 (재수립 + 1회 재시도).
    /// 404만이 스펙이지만 실서버 편차로 400 포함 (VS Code extHostMcp.ts:481-491,
    /// modelcontextprotocol/typescript-sdk#389 근거 차용).
    SessionExpired {
        status: u16,
    },
    Other(anyhow::Error),
}

impl ExchangeError {
    fn into_error(self) -> anyhow::Error {
        match self {
            Self::SessionExpired { status } => {
                anyhow::anyhow!("세션 재수립 후에도 HTTP {status} 응답 — 세션 만료 반복")
            }
            Self::Other(error) => error,
        }
    }
}

impl From<anyhow::Error> for ExchangeError {
    fn from(error: anyhow::Error) -> Self {
        Self::Other(error)
    }
}

/// Streamable HTTP MCP 연결 하나. `StdioClient`와 대칭 — connect(initialize +
/// 세션 수립) → 요청들 → drop(DELETE)이 한 호출 단위다 (connect-per-call 관례).
pub(crate) struct HttpClient {
    agent: ureq::Agent,
    /// 검증을 통과한 서버 URL — redirect의 same-origin 판정 기준.
    url: Url,
    server_name: String,
    bearer: Option<SecretString>,
    /// initialize 응답 헤더에서 캡처한 `Mcp-Session-Id` — 이후 모든 요청에 부착.
    session_id: Option<String>,
    /// H1 협상 결과 — initialize 이후 요청의 `MCP-Protocol-Version` 헤더 값.
    /// 스펙 필수 (VS Code는 이 헤더를 생략하지만 따르지 않는다 — 계획 §차용 안 함 #4).
    negotiated_version: Option<String>,
    next_id: u64,
    request_timeout: Duration,
}

impl std::fmt::Debug for HttpClient {
    /// bearer(Authorization 값)는 Debug에 노출하지 않는다.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpClient")
            .field("url", &self.url.as_str())
            .field("server_name", &self.server_name)
            .field("bearer", &self.bearer.as_ref().map(|_| "REDACTED"))
            .field("session_id", &self.session_id)
            .field("negotiated_version", &self.negotiated_version)
            .finish()
    }
}

impl HttpClient {
    /// URL 검증 → initialize 핸드셰이크(세션 캡처 + H1 버전 협상) →
    /// initialized notification. 성공 시 (client, initialize 결과, 협상 버전).
    pub(crate) fn connect(
        config: &McpHttpServerConfig,
        request_timeout: Duration,
    ) -> anyhow::Result<(Self, Value, String)> {
        let url = parse_validated_url(&config.url)?;
        // ureq 2 함정: agent 전체 timeout(.timeout)은 SSE 스트리밍 바디를 중간
        // 절단한다 — connect/read/write timeout만 설정한다. read timeout이 SSE
        // idle timeout을 겸하고, JSON 경로의 전체 상한은 exchange의 deadline.
        let agent = ureq::AgentBuilder::new()
            .redirects(0) // 자동 redirect 금지 — 자격 헤더 소거를 보장하는 수동 처리(exchange_once)
            .user_agent(&format!("deppy-sijo/{}", env!("CARGO_PKG_VERSION")))
            .timeout_connect(request_timeout)
            .timeout_read(request_timeout)
            .timeout_write(request_timeout)
            .build();
        let mut client = Self {
            agent,
            url,
            server_name: config.name.clone(),
            bearer: config
                .bearer
                .as_ref()
                .map(|bearer| SecretString::new(bearer.expose().to_owned())),
            session_id: None,
            negotiated_version: None,
            next_id: 1,
            request_timeout,
        };
        let initialize_result = match client.handshake() {
            Ok(result) => result,
            Err(error) => return Err(client.masked(error)),
        };
        let negotiated = client
            .negotiated_version
            .clone()
            .context("BUG: handshake 후 협상 버전 없음")?;
        Ok((client, initialize_result, negotiated))
    }

    /// JSON-RPC request 전송 → 같은 id의 response 대기 (result 반환).
    /// 세션 만료(400/404)면 세션 재수립 후 정확히 1회 재시도한다.
    pub(crate) fn request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.request_inner(method, &params)
            .map_err(|error| self.masked(error))
    }

    fn request_inner(&mut self, method: &str, params: &Value) -> anyhow::Result<Value> {
        match self.send_request(method, params) {
            Err(ExchangeError::SessionExpired { status }) => {
                // 차용: mcpServer.ts:1292-1297 — 재시도는 1회 한정(allowRetry).
                tracing::info!(
                    server = %self.server_name,
                    status,
                    method,
                    "MCP 세션 만료 응답 — 세션 재수립 후 1회 재시도"
                );
                self.handshake()
                    .context("MCP 세션 재수립(initialize) 실패")?;
                // 두 번째 실패는 그대로 에러 (SessionExpired여도 재시도 없음)
                self.send_request(method, params)
                    .map_err(ExchangeError::into_error)
            }
            other => other.map_err(ExchangeError::into_error),
        }
    }

    /// 요청 1회 전송 (재시도 없음). request에 202가 오면 응답 없는 요청으로 에러.
    fn send_request(&mut self, method: &str, params: &Value) -> Result<Value, ExchangeError> {
        let id = self.take_id();
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        match self.exchange(&message, Some(id), method)? {
            Outcome::Result(value) => Ok(value),
            Outcome::Accepted => Err(ExchangeError::Other(anyhow::anyhow!(
                "{method} 응답 없이 202 수신 (request에는 response가 필요)"
            ))),
        }
    }

    /// initialize → 버전 협상(H1 공용) → initialized notification.
    /// 최초 연결과 세션 만료 후 재수립이 같은 경로를 쓴다. 성공 시
    /// session_id / negotiated_version이 새 값으로 갱신된다.
    fn handshake(&mut self) -> anyhow::Result<Value> {
        self.session_id = None;
        self.negotiated_version = None;
        let id = self.take_id();
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "deppy-sijo",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            },
        });
        let outcome = self
            .exchange(&message, Some(id), "initialize")
            .map_err(ExchangeError::into_error)?;
        let Outcome::Result(initialize_result) = outcome else {
            bail!("initialize 응답이 비어 있음 (202)");
        };
        self.negotiated_version = Some(negotiate_protocol_version(
            &self.server_name,
            &initialize_result,
        )?);
        // 스펙: initialize 이후 모든 요청에 MCP-Protocol-Version 부착 — 이
        // notification부터 협상 버전이 실린다.
        let initialized =
            json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}});
        self.exchange(&initialized, None, "notifications/initialized")
            .map_err(ExchangeError::into_error)
            .context("initialized notification 실패")?;
        Ok(initialize_result)
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// POST 1회(수동 redirect 추적 포함) → 응답 분기(202/JSON/SSE) → Outcome.
    /// 대기 동안 5초 간격 진행 로그를 남긴다 (long-poll/SSE에서 "죽었는지
    /// 기다리는지" 구분 — H1 이월 항목).
    fn exchange(
        &mut self,
        message: &Value,
        expect_id: Option<u64>,
        method: &str,
    ) -> Result<Outcome, ExchangeError> {
        let body =
            serde_json::to_string(message).with_context(|| format!("{method} 요청 직렬화 실패"))?;
        let server = self.server_name.clone();
        let method_name = method.to_owned();
        run_with_progress(
            PROGRESS_LOG_INTERVAL,
            move |elapsed| {
                tracing::info!(
                    server = %server,
                    method = %method_name,
                    elapsed_secs = elapsed.as_secs(),
                    "MCP HTTP 응답 대기 중"
                );
            },
            || self.exchange_once(&body, expect_id, method),
        )
    }

    fn exchange_once(
        &mut self,
        body: &str,
        expect_id: Option<u64>,
        method: &str,
    ) -> Result<Outcome, ExchangeError> {
        // 비스트리밍(JSON) 경로의 전체 상한 — SSE 경로는 read idle timeout이 담당.
        let deadline = Instant::now() + self.request_timeout;
        let mut current = self.url.clone();
        let mut http_method = "POST";
        let mut send_body = true;
        for _hop in 0..=MAX_REDIRECTS {
            let same_origin = current.origin() == self.url.origin();
            let mut request = self
                .agent
                .request(http_method, current.as_str())
                .set("Accept", "text/event-stream, application/json");
            if send_body {
                request = request.set("Content-Type", "application/json");
            }
            if let Some(version) = &self.negotiated_version {
                request = request.set("MCP-Protocol-Version", version);
            }
            // cross-origin redirect 대상에는 자격 헤더를 보내지 않는다
            // (차용: extHostMcp.ts CROSS_ORIGIN_STRIPPED_HEADERS + Mcp-Session-Id).
            let mut session_attached = false;
            if same_origin {
                if let Some(bearer) = &self.bearer {
                    request = request.set("Authorization", &format!("Bearer {}", bearer.expose()));
                }
                if let Some(session) = &self.session_id {
                    request = request.set("Mcp-Session-Id", session);
                    session_attached = true;
                }
            }
            let result = if send_body {
                request.send_string(body)
            } else {
                request.call()
            };
            let response = match result {
                Ok(response) if (300..400).contains(&response.status()) => {
                    let status = response.status();
                    let location = response
                        .header("location")
                        .with_context(|| format!("HTTP {status} redirect에 Location 헤더 없음"))?
                        .to_owned();
                    let next = current.join(&location).map_err(|error| {
                        anyhow::anyhow!("redirect Location 해석 실패: {location} ({error})")
                    })?;
                    // fail-closed: 비-http(s) 스킴 즉시 거부 + redirect 대상에도
                    // deppy의 https(localhost 예외) 정책 재적용 (VS Code보다 엄격).
                    parse_validated_url(next.as_str())
                        .with_context(|| format!("redirect 대상 거부 (HTTP {status})"))?;
                    // 303, 그리고 POST의 301/302는 GET 전환 (fetch/curl 관례).
                    if status == 303 || (http_method == "POST" && (status == 301 || status == 302))
                    {
                        http_method = "GET";
                        send_body = false;
                    }
                    current = next;
                    continue;
                }
                Ok(response) => response,
                Err(ureq::Error::Status(status, response)) => {
                    return Err(self.classify_error_status(
                        status,
                        response,
                        session_attached,
                        method,
                    ));
                }
                Err(error) => {
                    return Err(ExchangeError::Other(
                        anyhow::Error::new(error).context(format!("MCP HTTP {method} 요청 실패")),
                    ));
                }
            };
            return self.handle_success(response, expect_id, method, deadline);
        }
        Err(ExchangeError::Other(anyhow::anyhow!(
            "redirect가 최대 추적 횟수({MAX_REDIRECTS}회)를 초과"
        )))
    }

    /// 4xx/5xx 분류. 세션 부착 요청의 400/404만 세션 만료로 구분한다.
    fn classify_error_status(
        &self,
        status: u16,
        response: ureq::Response,
        session_attached: bool,
        method: &str,
    ) -> ExchangeError {
        if session_attached && (status == 400 || status == 404) {
            return ExchangeError::SessionExpired { status };
        }
        let snippet = self.read_error_snippet(response);
        // initialize 자체가 4xx(401/403 제외)면 구 HTTP+SSE transport 서버일 수
        // 있다 — legacy SSE는 미지원(계획 §차용 안 함 #1), 폴백 신호 감지만 안내.
        let legacy_hint = if method == "initialize"
            && (400..500).contains(&status)
            && status != 401
            && status != 403
        {
            " (구 HTTP+SSE transport 서버일 수 있음 — Streamable HTTP만 지원)"
        } else {
            ""
        };
        ExchangeError::Other(anyhow::anyhow!(
            "{method} 실패 — HTTP {status}{legacy_hint}: {snippet}"
        ))
    }

    /// 에러 바디 snippet — bearer 마스킹 + 길이 제한 후에만 에러 문자열에 싣는다.
    fn read_error_snippet(&self, response: ureq::Response) -> String {
        let mut text = String::new();
        let _ = response.into_reader().take(2048).read_to_string(&mut text);
        self.mask_snippet(&text)
    }

    fn mask_snippet(&self, text: &str) -> String {
        self.mask_text(text)
            .chars()
            .take(ERROR_SNIPPET_CHARS)
            .collect()
    }

    /// 서버가 에코한 Authorization 값 등이 에러 문자열로 전파되지 않도록 마스킹.
    fn mask_text(&self, text: &str) -> String {
        match &self.bearer {
            Some(bearer) if !bearer.expose().is_empty() && text.contains(bearer.expose()) => {
                text.replace(bearer.expose(), "[REDACTED]")
            }
            _ => text.to_owned(),
        }
    }

    /// 최종 방어선: 에러 체인 전체 문자열에 bearer 평문이 섞였으면 마스킹한다.
    /// 오염이 없으면 체인을 그대로 보존한다.
    fn masked(&self, error: anyhow::Error) -> anyhow::Error {
        let Some(bearer) = &self.bearer else {
            return error;
        };
        let token = bearer.expose();
        if token.is_empty() {
            return error;
        }
        let text = format!("{error:#}");
        if text.contains(token) {
            anyhow::anyhow!("{}", text.replace(token, "[REDACTED]"))
        } else {
            error
        }
    }

    /// 2xx 응답 분기: 202 무바디 / JSON 단일 메시지 / SSE 스트림.
    /// 그 외 content-type은 거부 (계획 §차용 안 함 #9 — 관대한 재파싱 미채택).
    fn handle_success(
        &mut self,
        response: ureq::Response,
        expect_id: Option<u64>,
        method: &str,
        deadline: Instant,
    ) -> Result<Outcome, ExchangeError> {
        // 세션 캡처: initialize 응답이 주 경로 — 서버가 갱신해 주면 이후 반영.
        if let Some(session) = response.header("mcp-session-id") {
            self.session_id = Some(session.to_owned());
        }
        if response.status() == 202 {
            return Ok(Outcome::Accepted);
        }
        let Some(id) = expect_id else {
            // notification에 202가 아닌 2xx로 답하는 서버 편차 허용 — 바디는 버린다.
            return Ok(Outcome::Accepted);
        };
        match response.content_type() {
            "application/json" => {
                let body = read_body_capped(
                    response.into_reader(),
                    MAX_JSON_BODY_BYTES,
                    deadline,
                    method,
                )?;
                let value = validate_jsonrpc_message(&body).map_err(|reason| {
                    anyhow::anyhow!(
                        "{method} 응답 프로토콜 위반 ({reason}): {}",
                        self.mask_snippet(&body)
                    )
                })?;
                if value.get("method").is_some() {
                    return Err(anyhow::anyhow!(
                        "{method} JSON 응답이 response가 아님 (request/notification 수신)"
                    )
                    .into());
                }
                if value.get("id").and_then(Value::as_u64) != Some(id) {
                    return Err(anyhow::anyhow!(
                        "HTTP 프로토콜 위반: 요청하지 않은 id의 response (기대 id {id})"
                    )
                    .into());
                }
                Ok(Outcome::Result(unwrap_response(value, method)?))
            }
            "text/event-stream" => self.consume_sse(response, id, method),
            other => Err(anyhow::anyhow!(
                "{method} 응답 content-type 미지원: {other} \
                 (application/json 또는 text/event-stream만 수용)"
            )
            .into()),
        }
    }

    /// SSE 이벤트를 순차 소비해 요청 id의 response가 나올 때까지 읽는다.
    /// 중간 메시지는 stdio 경로와 동일 규칙 — notification 무시(debug),
    /// id 있는 서버 request는 -32601 회신 (별도 POST, 베스트에포트).
    fn consume_sse(
        &mut self,
        response: ureq::Response,
        expect_id: u64,
        method: &str,
    ) -> Result<Outcome, ExchangeError> {
        let mut stream = SseStream::new(response.into_reader(), SseLimits::default());
        loop {
            let Some(event) = stream.next_event()? else {
                return Err(anyhow::anyhow!("{method} 응답 전에 SSE 스트림이 종료됨").into());
            };
            if event.event_type == "endpoint" {
                // 구 HTTP+SSE transport의 첫 이벤트 — Streamable이 아니다
                // (차용: extHostMcp.ts:523-528의 폴백 신호 감지, 폴백 자체는 미지원).
                return Err(anyhow::anyhow!(
                    "서버가 구 HTTP+SSE transport로 응답함 (endpoint 이벤트) — \
                     Streamable HTTP만 지원"
                )
                .into());
            }
            if event.event_type != "message" {
                tracing::debug!(event_type = %event.event_type, "알 수 없는 SSE 이벤트 무시");
                continue;
            }
            let value = validate_jsonrpc_message(&event.data).map_err(|reason| {
                anyhow::anyhow!(
                    "{method} SSE 프로토콜 위반 ({reason}): {}",
                    self.mask_snippet(&event.data)
                )
            })?;
            if value.get("method").is_some() {
                // 원문은 로그에 싣지 않는다 — params에 secret 가능 (§7).
                if let Some(request_id) = value.get("id").cloned() {
                    self.post_method_not_found(request_id);
                }
                let server_method = value.get("method").and_then(Value::as_str).unwrap_or("?");
                tracing::debug!(method = %server_method, "server발 MCP 메시지 (HTTP, v0 미지원)");
                continue;
            }
            if value.get("id").and_then(Value::as_u64) == Some(expect_id) {
                return Ok(Outcome::Result(unwrap_response(value, method)?));
            }
            return Err(anyhow::anyhow!(
                "SSE 프로토콜 위반: 요청하지 않은 id의 response (기대 id {expect_id})"
            )
            .into());
        }
    }

    /// SSE 도중 도착한 server발 request에 method-not-found를 회신한다.
    /// 응답 대기 교착 방지용 — 베스트에포트, 실패해도 본 요청 흐름을 막지 않는다
    /// (stdio의 `let _ = self.write_line(&reply)` 관례).
    fn post_method_not_found(&self, request_id: Value) {
        let reply = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {"code": -32601, "message": "method not found"},
        });
        let Ok(body) = serde_json::to_string(&reply) else {
            return;
        };
        let mut request = self
            .agent
            .post(self.url.as_str())
            .set("Accept", "text/event-stream, application/json")
            .set("Content-Type", "application/json");
        if let Some(version) = &self.negotiated_version {
            request = request.set("MCP-Protocol-Version", version);
        }
        if let Some(bearer) = &self.bearer {
            request = request.set("Authorization", &format!("Bearer {}", bearer.expose()));
        }
        if let Some(session) = &self.session_id {
            request = request.set("Mcp-Session-Id", session);
        }
        let _ = request.send_string(&body);
    }
}

impl Drop for HttpClient {
    /// 연결 종료 시 세션 DELETE (차용: extHostMcp.ts close()/_closeSession).
    /// 베스트에포트 — 실패 무시, 이 경로에서는 인증 재시도 없음.
    fn drop(&mut self) {
        let Some(session) = self.session_id.take() else {
            return;
        };
        let mut request = self
            .agent
            .delete(self.url.as_str())
            .timeout(SESSION_DELETE_TIMEOUT)
            .set("Mcp-Session-Id", &session);
        if let Some(version) = &self.negotiated_version {
            request = request.set("MCP-Protocol-Version", version);
        }
        if let Some(bearer) = &self.bearer {
            request = request.set("Authorization", &format!("Bearer {}", bearer.expose()));
        }
        let _ = request.call();
    }
}

/// f가 도는 동안 interval마다 on_tick(경과 시간)을 호출한다 — 블로킹 HTTP
/// 대기 중 진행 로그용. f 완료 시 ticker 스레드는 채널 disconnect로 끝난다.
fn run_with_progress<T>(
    interval: Duration,
    on_tick: impl Fn(Duration) + Send + 'static,
    f: impl FnOnce() -> T,
) -> T {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let started = Instant::now();
    let ticker = std::thread::Builder::new()
        .name("mcp-http-progress".into())
        .spawn(move || {
            while let Err(mpsc::RecvTimeoutError::Timeout) = stop_rx.recv_timeout(interval) {
                on_tick(started.elapsed());
            }
        });
    let result = f();
    drop(stop_tx); // 채널 disconnect → ticker 종료
    if let Ok(handle) = ticker {
        let _ = handle.join();
    }
    result
}

/// 바디를 상한/deadline 안에서 끝까지 읽는다 (비스트리밍 JSON 경로).
fn read_body_capped(
    mut reader: impl Read,
    cap: usize,
    deadline: Instant,
    method: &str,
) -> anyhow::Result<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if Instant::now() >= deadline {
            bail!("{method} 응답 timeout (바디 수신 중)");
        }
        let n = reader
            .read(&mut chunk)
            .map_err(|error| io_read_error(error, method))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > cap {
            bail!("{method} 응답 바디가 상한({cap} bytes)을 초과");
        }
    }
    String::from_utf8(buf).map_err(|_| anyhow::anyhow!("{method} 응답이 UTF-8이 아님"))
}

/// 소켓 read timeout(idle)과 그 외 IO 오류를 구분해 에러 메시지를 만든다.
fn io_read_error(error: std::io::Error, method: &str) -> anyhow::Error {
    if matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ) {
        anyhow::anyhow!("{method} 응답 수신 idle timeout ({error})")
    } else {
        anyhow::Error::new(error).context(format!("{method} 응답 수신 실패"))
    }
}

/// dispatch된 SSE 이벤트 하나.
#[derive(Debug)]
struct SseEvent {
    /// `event:` 필드 (미지정이면 "message").
    event_type: String,
    /// `data:` 멀티라인을 '\n'으로 조립한 페이로드.
    data: String,
}

/// SSE 파서 상한 — 테스트에서 축소 주입할 수 있게 분리.
struct SseLimits {
    max_line_bytes: usize,
    max_data_bytes: usize,
    max_stream_bytes: usize,
}

impl Default for SseLimits {
    fn default() -> Self {
        Self {
            max_line_bytes: MAX_SSE_LINE_BYTES,
            max_data_bytes: MAX_SSE_DATA_BYTES,
            max_stream_bytes: MAX_SSE_STREAM_BYTES,
        }
    }
}

/// sync SSE 증분 파서. 필드 규칙은 VS Code sseParser.ts 차용(data 멀티라인 조립,
/// event 타입, NUL 포함 id 무시, retry 숫자만, 빈 줄 dispatch)하되 async
/// ReadableStream 대신 `Read` 기반으로 재작성. CR/LF/CRLF 라인 종결을 모두
/// 처리하고(chunk 경계의 CRLF 포함), deppy 강화로 라인/data/스트림 누적 상한을 건다.
struct SseStream<R: Read> {
    reader: R,
    limits: SseLimits,
    /// 미완성 라인 버퍼 (chunk 경계 이월분).
    pending: Vec<u8>,
    /// pending에서 이미 종결자 검사를 마친 길이 — 재스캔 방지.
    scanned: usize,
    /// 직전 라인이 chunk 끝의 CR로 종결됨 — 다음 chunk 선두 LF는 같은 CRLF.
    swallow_lf: bool,
    eof: bool,
    total_read: usize,
    data: String,
    event_type: String,
    /// 마지막 `id:` 값 — 재접속(Last-Event-ID)은 H2 미구현이라 보관만 한다.
    #[allow(dead_code)] // GET backchannel 재개(계획 §차용 안 함 #2) 도입 시 사용
    last_event_id: Option<String>,
    /// 마지막 `retry:` 값(ms) — 재접속 백오프는 H2 미구현이라 보관만 한다.
    #[allow(dead_code)] // GET backchannel 재개(계획 §차용 안 함 #2) 도입 시 사용
    retry_ms: Option<u64>,
}

impl<R: Read> SseStream<R> {
    fn new(reader: R, limits: SseLimits) -> Self {
        Self {
            reader,
            limits,
            pending: Vec::new(),
            scanned: 0,
            swallow_lf: false,
            eof: false,
            total_read: 0,
            data: String::new(),
            event_type: String::new(),
            last_event_id: None,
            retry_ms: None,
        }
    }

    /// 다음 dispatch 이벤트. `Ok(None)`은 스트림 정상 종료(EOF) — 스펙대로
    /// dispatch되지 않은 미완성 이벤트/라인은 버린다.
    fn next_event(&mut self) -> anyhow::Result<Option<SseEvent>> {
        loop {
            while let Some(line) = self.take_line()? {
                if let Some(event) = self.process_line(&line)? {
                    return Ok(Some(event));
                }
            }
            if self.eof {
                return Ok(None);
            }
            self.fill()?;
        }
    }

    fn fill(&mut self) -> anyhow::Result<()> {
        let mut chunk = [0u8; 8192];
        let n = self
            .reader
            .read(&mut chunk)
            .map_err(|error| io_read_error(error, "SSE"))?;
        if n == 0 {
            self.eof = true;
            return Ok(());
        }
        self.total_read += n;
        if self.total_read > self.limits.max_stream_bytes {
            bail!(
                "SSE 스트림 누적 수신이 상한({} bytes)을 초과",
                self.limits.max_stream_bytes
            );
        }
        self.pending.extend_from_slice(&chunk[..n]);
        Ok(())
    }

    /// pending에서 완성된 라인 하나를 꺼낸다 (종결자 제거). 없으면 None.
    fn take_line(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        // 직전 라인이 CR로 끝났고 다음 byte가 LF면 같은 CRLF 종결자 — 삼킨다.
        if self.swallow_lf && !self.pending.is_empty() {
            if self.pending[0] == b'\n' {
                self.pending.remove(0);
                self.scanned = self.scanned.saturating_sub(1);
            }
            self.swallow_lf = false;
        }
        while self.scanned < self.pending.len() {
            let byte = self.pending[self.scanned];
            if byte == b'\n' || byte == b'\r' {
                let line = self.pending[..self.scanned].to_vec();
                let mut consume = self.scanned + 1;
                if byte == b'\r' {
                    if consume < self.pending.len() {
                        if self.pending[consume] == b'\n' {
                            consume += 1;
                        }
                    } else {
                        self.swallow_lf = true; // CR이 chunk 끝 — 다음 선두 LF 삼킴
                    }
                }
                self.pending.drain(..consume);
                self.scanned = 0;
                return Ok(Some(line));
            }
            self.scanned += 1;
        }
        if self.pending.len() > self.limits.max_line_bytes {
            bail!(
                "SSE 라인 길이 상한({} bytes) 초과",
                self.limits.max_line_bytes
            );
        }
        Ok(None)
    }

    /// SSE 필드 한 줄 처리. 빈 줄이면 dispatch — 완성 이벤트를 돌려준다.
    fn process_line(&mut self, line: &[u8]) -> anyhow::Result<Option<SseEvent>> {
        if line.is_empty() {
            // dispatch. data가 비어 있으면 이벤트 없음 — 타입만 리셋 (스펙).
            if self.data.is_empty() {
                self.event_type.clear();
                return Ok(None);
            }
            let mut data = std::mem::take(&mut self.data);
            data.pop(); // data 라인마다 붙인 '\n'의 마지막 하나 제거 (스펙)
            let event_type = std::mem::take(&mut self.event_type);
            return Ok(Some(SseEvent {
                event_type: if event_type.is_empty() {
                    "message".to_owned()
                } else {
                    event_type
                },
                data,
            }));
        }
        let text = String::from_utf8_lossy(line);
        if text.starts_with(':') {
            return Ok(None); // comment 라인
        }
        let (field, value) = match text.find(':') {
            Some(pos) => {
                let value = &text[pos + 1..];
                // 값 선두의 공백 하나만 제거 (스펙)
                (&text[..pos], value.strip_prefix(' ').unwrap_or(value))
            }
            None => (text.as_ref(), ""),
        };
        match field {
            "data" => {
                if self.data.len() + value.len() + 1 > self.limits.max_data_bytes {
                    bail!(
                        "SSE 이벤트 data 상한({} bytes) 초과",
                        self.limits.max_data_bytes
                    );
                }
                self.data.push_str(value);
                self.data.push('\n');
            }
            "event" => self.event_type = value.to_owned(),
            // NUL(\0) 포함 id는 무시 (차용: sseParser.ts 179-181행)
            "id" if !value.contains('\0') => self.last_event_id = Some(value.to_owned()),
            "id" => {}
            // 숫자만 유효 (차용: sseParser.ts 186행)
            "retry" if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
                self.retry_ms = value.parse().ok();
            }
            _ => {} // 비숫자 retry / 알 수 없는 필드 무시 (스펙)
        }
        Ok(None)
    }
}

/// HTTP 바디/SSE data가 valid JSON-RPC 2.0 단일 메시지인지 검증.
/// stdio transport.rs `validate_jsonrpc`와 동일 규칙 — H2 파일 스코프 분리로
/// 복제 (batch 배열은 2025 스펙에서 금지 — 거부).
fn validate_jsonrpc_message(text: &str) -> Result<Value, String> {
    let value: Value =
        serde_json::from_str(text).map_err(|error| format!("JSON 파싱 실패: {error}"))?;
    let Some(obj) = value.as_object() else {
        return Err("JSON object가 아님 (batch 배열 금지)".to_owned());
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err("jsonrpc 필드가 \"2.0\"이 아님".to_owned());
    }
    let has_method = obj.get("method").is_some_and(Value::is_string);
    let has_result = obj.contains_key("result");
    let error_field = obj.get("error");
    let has_error = error_field.is_some();
    // error response의 error는 {code: number, message: string} object여야 한다
    let error_shape_ok = error_field.is_none_or(|err| {
        err.get("code").is_some_and(Value::is_number)
            && err.get("message").is_some_and(Value::is_string)
    });
    let valid = if has_method {
        // request/notification — result/error 동반 금지, id가 있으면 string|number
        !has_result && !has_error && obj.get("id").is_none_or(|id| is_valid_id(id, false))
    } else {
        // response — id 필수(string|number, error response만 null 허용),
        // result와 error 중 정확히 하나
        (has_result ^ has_error)
            && error_shape_ok
            && obj.get("id").is_some_and(|id| is_valid_id(id, has_error))
    };
    if valid {
        Ok(value)
    } else {
        Err("JSON-RPC 메시지 형태가 아님".to_owned())
    }
}

/// JSON-RPC id 타입 검사 — string 또는 비음수 정수 number.
/// null은 파싱 불가 요청에 대한 error response에서만 허용 (transport.rs와 동일).
fn is_valid_id(id: &Value, allow_null: bool) -> bool {
    id.is_string() || id.as_u64().is_some() || (allow_null && id.is_null())
}

/// response에서 result를 꺼낸다. error response는 에러로 변환 (transport.rs와 동일 규칙).
fn unwrap_response(mut value: Value, method: &str) -> anyhow::Result<Value> {
    let obj = value.as_object_mut().context("응답이 JSON object가 아님")?;
    if let Some(err) = obj.get("error") {
        bail!("{method} 실패 — server error: {err}");
    }
    obj.remove("result")
        .with_context(|| format!("{method} 응답에 result 없음"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::LocalMcpManager;
    use secret::RedactionService;
    use std::io::{Cursor, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    // ---------- SSE 파서 단위 테스트 (Cursor / DripRead) ----------

    /// 한 번의 read마다 최대 step byte만 내주는 Read — chunk 경계 재현용.
    struct DripRead {
        data: Vec<u8>,
        pos: usize,
        step: usize,
    }

    impl Read for DripRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.step.min(self.data.len() - self.pos).min(buf.len());
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    fn small_limits() -> SseLimits {
        SseLimits {
            max_line_bytes: 64,
            max_data_bytes: 64,
            max_stream_bytes: 256,
        }
    }

    fn collect_events(raw: &str) -> Vec<(String, String)> {
        let mut stream = SseStream::new(Cursor::new(raw.as_bytes().to_vec()), SseLimits::default());
        let mut events = Vec::new();
        while let Some(event) = stream.next_event().unwrap() {
            events.push((event.event_type, event.data));
        }
        events
    }

    #[test]
    fn sse_멀티라인_data와_기본_이벤트_타입() {
        let events = collect_events("data: hello\ndata: world\n\ndata: next\n\n");
        assert_eq!(
            events,
            vec![
                ("message".to_owned(), "hello\nworld".to_owned()),
                ("message".to_owned(), "next".to_owned()),
            ]
        );
    }

    #[test]
    fn sse_cr_lf_crlf_혼용과_comment_무시() {
        // CR / LF / CRLF 종결 혼용 + comment(:)와 콜론 없는 필드 라인 처리
        let raw = ": comment\r\nevent: custom\rdata: a\r\ndata:b\n\r\n";
        let events = collect_events(raw);
        // "data:b"는 콜론 뒤 공백이 없으므로 값 "b" (선두 공백 하나만 제거)
        assert_eq!(events, vec![("custom".to_owned(), "a\nb".to_owned())]);
    }

    #[test]
    fn sse_이벤트_타입은_dispatch_후_리셋() {
        let events = collect_events("event: one\ndata: x\n\ndata: y\n\n");
        assert_eq!(
            events,
            vec![
                ("one".to_owned(), "x".to_owned()),
                ("message".to_owned(), "y".to_owned()),
            ]
        );
    }

    #[test]
    fn sse_빈_data는_dispatch_없이_타입만_리셋() {
        // event 타입만 설정하고 data 없이 빈 줄 → dispatch 없음, 타입 리셋
        let events = collect_events("event: skipped\n\ndata: real\n\n");
        assert_eq!(events, vec![("message".to_owned(), "real".to_owned())]);
    }

    #[test]
    fn sse_nul_포함_id는_무시_정상_id는_보관() {
        let raw = "id: ok-1\ndata: x\n\nid: bad\0id\ndata: y\n\n";
        let mut stream = SseStream::new(Cursor::new(raw.as_bytes().to_vec()), SseLimits::default());
        stream.next_event().unwrap().unwrap();
        assert_eq!(stream.last_event_id.as_deref(), Some("ok-1"));
        stream.next_event().unwrap().unwrap();
        // NUL 포함 id는 무시되어 이전 값 유지 (sseParser.ts 차용)
        assert_eq!(stream.last_event_id.as_deref(), Some("ok-1"));
    }

    #[test]
    fn sse_retry는_숫자만_유효() {
        let raw = "retry: 1500\ndata: x\n\nretry: 3s\ndata: y\n\n";
        let mut stream = SseStream::new(Cursor::new(raw.as_bytes().to_vec()), SseLimits::default());
        stream.next_event().unwrap().unwrap();
        assert_eq!(stream.retry_ms, Some(1500));
        stream.next_event().unwrap().unwrap();
        assert_eq!(stream.retry_ms, Some(1500)); // 비숫자는 무시
    }

    #[test]
    fn sse_chunk_경계의_crlf도_한_종결자() {
        // "\r" 뒤에서 chunk가 끊기고 다음 chunk가 "\n"으로 시작 — 빈 줄이
        // 생기면 잘못된 조기 dispatch가 난다. byte 단위 drip으로 재현.
        let raw = b"data: a\r\ndata: b\r\n\r\n".to_vec();
        let mut stream = SseStream::new(
            DripRead {
                data: raw,
                pos: 0,
                step: 1,
            },
            SseLimits::default(),
        );
        let event = stream.next_event().unwrap().unwrap();
        assert_eq!(event.data, "a\nb");
        assert!(stream.next_event().unwrap().is_none());
    }

    #[test]
    fn sse_미완성_이벤트는_eof에서_버림() {
        // 빈 줄 dispatch 전에 스트림 종료 — 스펙대로 이벤트 없음
        let events = collect_events("data: incomplete\n");
        assert!(events.is_empty());
    }

    #[test]
    fn sse_라인_상한_초과는_에러() {
        let raw = format!("data: {}", "a".repeat(100)); // 종결자 없는 100+B 라인
        let mut stream = SseStream::new(Cursor::new(raw.into_bytes()), small_limits());
        let error = stream.next_event().unwrap_err();
        assert!(format!("{error:#}").contains("라인 길이 상한"), "{error:#}");
    }

    #[test]
    fn sse_data_누적_상한_초과는_에러() {
        // 라인 하나는 상한 이하지만 data 조립 누적이 64B를 넘는다
        let raw = format!("data: {}\ndata: {}\n\n", "a".repeat(40), "b".repeat(40));
        let mut stream = SseStream::new(Cursor::new(raw.into_bytes()), small_limits());
        let error = stream.next_event().unwrap_err();
        assert!(format!("{error:#}").contains("data 상한"), "{error:#}");
    }

    #[test]
    fn sse_스트림_누적_상한_초과는_에러() {
        // 이벤트 각각은 정상이지만 총 수신량이 256B를 넘는다
        let raw = "data: x\n\n".repeat(50);
        let mut stream = SseStream::new(Cursor::new(raw.into_bytes()), small_limits());
        let error = loop {
            match stream.next_event() {
                Ok(Some(_)) => continue,
                Ok(None) => panic!("상한 초과 에러가 나야 함"),
                Err(error) => break error,
            }
        };
        assert!(
            format!("{error:#}").contains("누적 수신이 상한"),
            "{error:#}"
        );
    }

    // ---------- URL 검증 / 진행 로그 단위 테스트 ----------

    #[test]
    fn url_검증_https필수_http는_루프백만() {
        assert!(validate_mcp_url("https://example.com/mcp").is_ok());
        assert!(validate_mcp_url("http://localhost:8080/mcp").is_ok());
        assert!(validate_mcp_url("http://127.0.0.1:9999/mcp").is_ok());
        assert!(validate_mcp_url("http://[::1]:8080/mcp").is_ok());

        assert!(validate_mcp_url("http://evil.com/mcp").is_err());
        assert!(validate_mcp_url("ftp://127.0.0.1/mcp").is_err());
        assert!(validate_mcp_url("not a url").is_err());
    }

    #[test]
    fn 진행_tick은_interval마다_호출되고_결과를_보존() {
        let ticks = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&ticks);
        let result = run_with_progress(
            Duration::from_millis(10),
            move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
            },
            || {
                std::thread::sleep(Duration::from_millis(80));
                42
            },
        );
        assert_eq!(result, 42);
        assert!(ticks.load(Ordering::SeqCst) >= 2, "{ticks:?}");
    }

    // ---------- 목 HTTP 서버 (std TcpListener, 포트 0) ----------

    /// 목 서버가 캡처한 요청 하나 (헤더 이름은 소문자 정규화).
    #[derive(Debug, Clone)]
    struct CapturedRequest {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl CapturedRequest {
        fn header(&self, name: &str) -> Option<&str> {
            let lower = name.to_ascii_lowercase();
            self.headers
                .iter()
                .find(|(key, _)| *key == lower)
                .map(|(_, value)| value.as_str())
        }

        fn body_json(&self) -> Value {
            serde_json::from_str(&self.body).unwrap_or(Value::Null)
        }

        fn rpc_method(&self) -> String {
            self.body_json()
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        }
    }

    /// n번째 커넥션(요청)에 돌려줄 응답.
    enum Reply {
        Raw(Vec<u8>),
        /// 바디를 쓴 뒤 커넥션을 열어둔 채 대기 (idle timeout 테스트용).
        RawThenHold(Vec<u8>, Duration),
    }

    struct MockServer {
        base_url: String,
        requests: Arc<Mutex<Vec<CapturedRequest>>>,
    }

    impl MockServer {
        fn captured(&self) -> Vec<CapturedRequest> {
            self.requests.lock().unwrap().clone()
        }

        /// n개의 요청이 캡처될 때까지 폴링 (drop DELETE 등 관측용).
        fn wait_captured(&self, n: usize) -> Vec<CapturedRequest> {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let snapshot = self.captured();
                if snapshot.len() >= n {
                    return snapshot;
                }
                assert!(
                    Instant::now() < deadline,
                    "요청 {n}개 미도착: {}개 — {snapshot:?}",
                    snapshot.len()
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    /// 포트 0 TcpListener 목 서버 — 커넥션당 요청 1개, Connection: close 강제.
    fn spawn_mock(
        responder: impl Fn(usize, &CapturedRequest) -> Reply + Send + 'static,
    ) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests: Arc<Mutex<Vec<CapturedRequest>>> = Arc::default();
        let captured = Arc::clone(&requests);
        std::thread::spawn(move || {
            let mut index = 0usize;
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let Some(request) = read_http_request(&mut stream) else {
                    continue;
                };
                captured.lock().unwrap().push(request.clone());
                match responder(index, &request) {
                    Reply::Raw(bytes) => {
                        let _ = stream.write_all(&bytes);
                    }
                    Reply::RawThenHold(bytes, hold) => {
                        let _ = stream.write_all(&bytes);
                        let _ = stream.flush();
                        std::thread::sleep(hold);
                    }
                }
                index += 1;
            }
        });
        MockServer {
            base_url: format!("http://{addr}/mcp"),
            requests,
        }
    }

    fn read_http_request(stream: &mut TcpStream) -> Option<CapturedRequest> {
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                break pos;
            }
            let n = stream.read(&mut chunk).ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        let mut lines = head.split("\r\n");
        let request_line = lines.next()?;
        let mut parts = request_line.split(' ');
        let method = parts.next()?.to_owned();
        let path = parts.next()?.to_owned();
        let mut headers = Vec::new();
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
            }
        }
        let content_length: usize = headers
            .iter()
            .find(|(key, _)| key == "content-length")
            .and_then(|(_, value)| value.parse().ok())
            .unwrap_or(0);
        let mut body = buf[header_end + 4..].to_vec();
        while body.len() < content_length {
            let n = stream.read(&mut chunk).ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        Some(CapturedRequest {
            method,
            path,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        })
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    fn http_response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status} OK\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (name, value) in headers {
            out.push_str(name);
            out.push_str(": ");
            out.push_str(value);
            out.push_str("\r\n");
        }
        out.push_str("\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    /// Content-Length 없는 close-delimited 응답 — 열린 SSE 스트림 재현용.
    fn http_response_streaming(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {status} OK\r\nConnection: close\r\n");
        for (name, value) in headers {
            out.push_str(name);
            out.push_str(": ");
            out.push_str(value);
            out.push_str("\r\n");
        }
        out.push_str("\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    /// 요청 body의 id를 에코해 JSON-RPC result 응답을 만든다.
    fn json_reply(request: &CapturedRequest, result: Value, session: Option<&str>) -> Reply {
        let id = request
            .body_json()
            .get("id")
            .cloned()
            .unwrap_or(Value::Null);
        let body = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
        let mut headers = vec![("Content-Type", "application/json")];
        if let Some(session) = session {
            headers.push(("Mcp-Session-Id", session));
        }
        Reply::Raw(http_response(200, &headers, body.as_bytes()))
    }

    fn sse_reply(events: &str, session: Option<&str>) -> Reply {
        let mut headers = vec![("Content-Type", "text/event-stream")];
        if let Some(session) = session {
            headers.push(("Mcp-Session-Id", session));
        }
        Reply::Raw(http_response(200, &headers, events.as_bytes()))
    }

    fn accepted() -> Reply {
        Reply::Raw(http_response(202, &[], b""))
    }

    fn not_found() -> Reply {
        Reply::Raw(http_response(404, &[], b""))
    }

    fn init_result() -> Value {
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "serverInfo": {"name": "mock", "version": "0"},
        })
    }

    fn tools_result() -> Value {
        json!({"tools": [{"name": "echo_tool", "description": "에코", "inputSchema": {"type": "object"}}]})
    }

    fn manager() -> LocalMcpManager {
        LocalMcpManager::new(RedactionService::new()).with_request_timeout(Duration::from_secs(5))
    }

    fn http_config(server: &MockServer, bearer: Option<&str>) -> McpHttpServerConfig {
        McpHttpServerConfig {
            name: "mock-http".to_owned(),
            url: server.base_url.clone(),
            bearer: bearer.map(|token| SecretString::new(token.to_owned())),
        }
    }

    // ---------- transport 통합 테스트 ----------

    #[test]
    fn json_왕복_세션과_헤더_부착() {
        let server = spawn_mock(|index, request| match index {
            0 => json_reply(request, init_result(), Some("s1")),
            1 => accepted(),
            2 => json_reply(request, tools_result(), None),
            _ => not_found(), // drop DELETE
        });
        let config = http_config(&server, Some("sk-test-token-1"));

        let tools = manager().discover_tools_http(&config).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo_tool");
        assert_eq!(tools[0].description.as_deref(), Some("에코"));

        // drop DELETE까지 4개 요청
        let requests = server.wait_captured(4);
        let init = &requests[0];
        assert_eq!(init.method, "POST");
        assert_eq!(init.rpc_method(), "initialize");
        assert_eq!(
            init.body_json().pointer("/params/protocolVersion"),
            Some(&json!("2025-11-25"))
        );
        assert_eq!(
            init.header("accept"),
            Some("text/event-stream, application/json")
        );
        assert_eq!(init.header("content-type"), Some("application/json"));
        assert!(
            init.header("user-agent")
                .unwrap()
                .starts_with("deppy-sijo/"),
            "{init:?}"
        );
        // initialize 전에는 세션도 협상 버전 헤더도 없다
        assert_eq!(init.header("mcp-session-id"), None);
        assert_eq!(init.header("mcp-protocol-version"), None);
        assert_eq!(init.header("authorization"), Some("Bearer sk-test-token-1"));

        // initialized notification: 202 무바디 성공 + 세션/버전 부착
        let initialized = &requests[1];
        assert_eq!(initialized.rpc_method(), "notifications/initialized");
        assert_eq!(initialized.header("mcp-session-id"), Some("s1"));
        assert_eq!(
            initialized.header("mcp-protocol-version"),
            Some("2025-11-25")
        );

        let list = &requests[2];
        assert_eq!(list.rpc_method(), "tools/list");
        assert_eq!(list.header("mcp-session-id"), Some("s1"));
        assert_eq!(list.header("authorization"), Some("Bearer sk-test-token-1"));

        // drop 시 DELETE + 세션 (베스트에포트)
        let delete = &requests[3];
        assert_eq!(delete.method, "DELETE");
        assert_eq!(delete.header("mcp-session-id"), Some("s1"));
    }

    #[test]
    fn sse_멀티이벤트_notification_섞임_멀티라인_data() {
        let server = spawn_mock(|index, request| match index {
            0 => json_reply(request, init_result(), Some("s1")),
            1 => accepted(),
            2 => {
                let id = request.body_json().get("id").cloned().unwrap();
                // comment / 알 수 없는 이벤트 타입 / notification / 멀티라인
                // data response(CRLF 혼용, id·retry 필드 포함)를 한 스트림에.
                let events = format!(
                    ": stream comment\n\
                     event: telemetry\ndata: not json but ignored\n\n\
                     data: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{{}}}}\n\n\
                     id: evt-9\r\nretry: 1000\r\ndata: {{\"jsonrpc\":\"2.0\",\r\ndata:  \"id\":{id},\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"hi\"}}],\"isError\":false}}}}\r\n\r\n"
                );
                sse_reply(&events, None)
            }
            _ => not_found(),
        });
        let config = http_config(&server, None);

        let result = manager()
            .call_tool_http(&config, "echo_tool", json!({"msg": "x"}))
            .unwrap();
        assert_eq!(
            result.pointer("/content/0/text").and_then(Value::as_str),
            Some("hi")
        );
    }

    #[test]
    fn 세션_만료_400은_재수립_후_정확히_1회_재시도() {
        let server = spawn_mock(|index, request| match index {
            0 => json_reply(request, init_result(), Some("s1")),
            1 => accepted(),
            2 => Reply::Raw(http_response(400, &[], b"session expired")),
            3 => json_reply(request, init_result(), Some("s2")),
            4 => accepted(),
            5 => json_reply(request, tools_result(), None),
            _ => not_found(),
        });
        let config = http_config(&server, None);

        let tools = manager().discover_tools_http(&config).unwrap();
        assert_eq!(tools.len(), 1);

        let requests = server.wait_captured(7);
        // initialize가 정확히 2번 (최초 + 재수립 1회)
        let init_count = requests
            .iter()
            .filter(|request| request.rpc_method() == "initialize")
            .count();
        assert_eq!(init_count, 2);
        // 만료된 요청은 s1, 재시도는 새 세션 s2로
        assert_eq!(requests[2].header("mcp-session-id"), Some("s1"));
        assert_eq!(requests[5].rpc_method(), "tools/list");
        assert_eq!(requests[5].header("mcp-session-id"), Some("s2"));
        // drop DELETE도 새 세션으로
        assert_eq!(requests[6].method, "DELETE");
        assert_eq!(requests[6].header("mcp-session-id"), Some("s2"));
    }

    #[test]
    fn 세션_재수립_후_재실패는_에러() {
        let server = spawn_mock(|index, request| match index {
            0 => json_reply(request, init_result(), Some("s1")),
            1 => accepted(),
            2 => not_found(), // 세션 만료 (404)
            3 => json_reply(request, init_result(), Some("s2")),
            4 => accepted(),
            5 => not_found(), // 재시도도 404 — 더 이상 재시도 없음
            _ => not_found(),
        });
        let config = http_config(&server, None);

        let error = manager().discover_tools_http(&config).unwrap_err();
        assert!(format!("{error:#}").contains("세션 만료 반복"), "{error:#}");
        let requests = server.captured();
        let init_count = requests
            .iter()
            .filter(|request| request.rpc_method() == "initialize")
            .count();
        assert_eq!(init_count, 2, "재수립은 정확히 1회여야 함");
    }

    #[test]
    fn cross_origin_redirect는_자격_헤더_소거() {
        // 서버 B: redirect 대상 (다른 포트 = cross-origin)
        let server_b = spawn_mock(|index, request| match index {
            0 => json_reply(request, tools_result(), None),
            _ => not_found(),
        });
        let b_url = server_b.base_url.clone();
        // 서버 A: tools/list에 307 → B
        let server_a = spawn_mock(move |index, request| match index {
            0 => json_reply(request, init_result(), Some("s1")),
            1 => accepted(),
            2 => Reply::Raw(http_response(307, &[("Location", &b_url)], b"")),
            _ => not_found(),
        });
        let config = http_config(&server_a, Some("sk-cross-origin-7"));

        let tools = manager().discover_tools_http(&config).unwrap();
        assert_eq!(tools.len(), 1);

        // A의 tools/list에는 자격 헤더가 있었다
        let a_requests = server_a.captured();
        assert_eq!(
            a_requests[2].header("authorization"),
            Some("Bearer sk-cross-origin-7")
        );
        assert_eq!(a_requests[2].header("mcp-session-id"), Some("s1"));

        // cross-origin B에는 Authorization/Mcp-Session-Id 소거, 307이라 POST+바디 유지
        let b_requests = server_b.captured();
        let forwarded = &b_requests[0];
        assert_eq!(forwarded.method, "POST");
        assert_eq!(forwarded.rpc_method(), "tools/list");
        assert_eq!(forwarded.header("authorization"), None);
        assert_eq!(forwarded.header("mcp-session-id"), None);
        // 자격이 아닌 헤더는 유지
        assert_eq!(forwarded.header("mcp-protocol-version"), Some("2025-11-25"));
    }

    #[test]
    fn same_origin_redirect는_자격_헤더_유지_상대경로_해석() {
        let server = spawn_mock(|index, request| match index {
            0 => json_reply(request, init_result(), Some("s1")),
            1 => accepted(),
            2 => Reply::Raw(http_response(307, &[("Location", "/mcp2")], b"")),
            3 => json_reply(request, tools_result(), None),
            _ => not_found(),
        });
        let config = http_config(&server, Some("sk-same-origin-8"));

        let tools = manager().discover_tools_http(&config).unwrap();
        assert_eq!(tools.len(), 1);

        let requests = server.captured();
        let redirected = &requests[3];
        assert_eq!(redirected.path, "/mcp2"); // 상대 Location 해석
        assert_eq!(
            redirected.header("authorization"),
            Some("Bearer sk-same-origin-8")
        );
        assert_eq!(redirected.header("mcp-session-id"), Some("s1"));
    }

    #[test]
    fn redirect_303은_get_전환() {
        let server = spawn_mock(|index, request| match index {
            0 => json_reply(request, init_result(), None),
            1 => accepted(),
            2 => Reply::Raw(http_response(303, &[("Location", "/see-other")], b"")),
            3 => {
                // GET 전환 — tools/list id는 결정적으로 2 (initialize=1)
                let body = json!({"jsonrpc": "2.0", "id": 2, "result": tools_result()}).to_string();
                Reply::Raw(http_response(
                    200,
                    &[("Content-Type", "application/json")],
                    body.as_bytes(),
                ))
            }
            _ => not_found(),
        });
        let config = http_config(&server, None);

        let tools = manager().discover_tools_http(&config).unwrap();
        assert_eq!(tools.len(), 1);
        let requests = server.captured();
        assert_eq!(requests[3].method, "GET");
        assert!(requests[3].body.is_empty(), "{:?}", requests[3]);
    }

    #[test]
    fn 비http_스킴과_비루프백_http_redirect_거부() {
        // ftp 스킴 redirect → fail-closed
        let server = spawn_mock(|index, request| match index {
            0 => json_reply(request, init_result(), None),
            1 => accepted(),
            2 => Reply::Raw(http_response(
                307,
                &[("Location", "ftp://127.0.0.1/x")],
                b"",
            )),
            _ => not_found(),
        });
        let error = manager()
            .discover_tools_http(&http_config(&server, None))
            .unwrap_err();
        assert!(format!("{error:#}").contains("scheme 불허"), "{error:#}");

        // 비루프백 http redirect → deppy https 정책으로 거부
        let server = spawn_mock(|index, request| match index {
            0 => json_reply(request, init_result(), None),
            1 => accepted(),
            2 => Reply::Raw(http_response(
                307,
                &[("Location", "http://evil.example.com/mcp")],
                b"",
            )),
            _ => not_found(),
        });
        let error = manager()
            .discover_tools_http(&http_config(&server, None))
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("localhost/루프백만 허용"),
            "{error:#}"
        );
    }

    #[test]
    fn redirect_한도_초과는_에러() {
        let server = spawn_mock(|index, request| match index {
            0 => json_reply(request, init_result(), None),
            1 => accepted(),
            _ => Reply::Raw(http_response(307, &[("Location", "/loop")], b"")),
        });
        let error = manager()
            .discover_tools_http(&http_config(&server, None))
            .unwrap_err();
        assert!(format!("{error:#}").contains("redirect"), "{error:#}");
        // 최초 1회 + 최대 5회 추적 = tools/list 계열 요청 6개 (init 2개 제외)
        assert_eq!(server.captured().len(), 2 + 6);
    }

    #[test]
    fn 미지원_content_type은_거부() {
        let server = spawn_mock(|index, request| match index {
            0 => json_reply(request, init_result(), None),
            1 => accepted(),
            2 => Reply::Raw(http_response(
                200,
                &[("Content-Type", "text/html")],
                b"<html>oops</html>",
            )),
            _ => not_found(),
        });
        let error = manager()
            .discover_tools_http(&http_config(&server, None))
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("content-type 미지원"),
            "{error:#}"
        );
    }

    #[test]
    fn request에_202는_에러() {
        let server = spawn_mock(|index, request| match index {
            0 => json_reply(request, init_result(), None),
            1 => accepted(),
            2 => accepted(), // tools/list에 202 — response가 없다
            _ => not_found(),
        });
        let error = manager()
            .discover_tools_http(&http_config(&server, None))
            .unwrap_err();
        assert!(format!("{error:#}").contains("202"), "{error:#}");
    }

    #[test]
    fn sse_endpoint_이벤트는_legacy_sse_서버_에러() {
        let server = spawn_mock(|index, _| match index {
            0 => sse_reply("event: endpoint\ndata: /messages?sid=1\n\n", None),
            _ => not_found(),
        });
        let error = manager()
            .discover_tools_http(&http_config(&server, None))
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("구 HTTP+SSE transport"),
            "{error:#}"
        );
    }

    #[test]
    fn initialize_4xx는_legacy_sse_힌트() {
        let server = spawn_mock(|_, _| Reply::Raw(http_response(405, &[], b"method not allowed")));
        let error = manager()
            .discover_tools_http(&http_config(&server, None))
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("HTTP 405"), "{text}");
        assert!(
            text.contains("구 HTTP+SSE transport 서버일 수 있음"),
            "{text}"
        );
    }

    #[test]
    fn sse_요청하지_않은_id의_response는_위반() {
        let server = spawn_mock(|index, request| match index {
            0 => json_reply(request, init_result(), None),
            1 => accepted(),
            2 => sse_reply(
                "data: {\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{}}\n\n",
                None,
            ),
            _ => not_found(),
        });
        let error = manager()
            .discover_tools_http(&http_config(&server, None))
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("요청하지 않은 id"),
            "{error:#}"
        );
    }

    #[test]
    fn sse_비json_data는_프로토콜_위반() {
        let server = spawn_mock(|index, request| match index {
            0 => json_reply(request, init_result(), None),
            1 => accepted(),
            2 => sse_reply("data: plain text\n\n", None),
            _ => not_found(),
        });
        let error = manager()
            .discover_tools_http(&http_config(&server, None))
            .unwrap_err();
        assert!(format!("{error:#}").contains("프로토콜 위반"), "{error:#}");
    }

    #[test]
    fn server발_request는_32601_회신() {
        let server = spawn_mock(|index, request| match index {
            0 => json_reply(request, init_result(), Some("s1")),
            1 => accepted(),
            2 => {
                let id = request.body_json().get("id").cloned().unwrap();
                let events = format!(
                    "data: {{\"jsonrpc\":\"2.0\",\"id\":77,\"method\":\"roots/list\",\"params\":{{}}}}\n\n\
                     data: {{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{}}}\n\n",
                    tools_result()
                );
                sse_reply(&events, None)
            }
            3 => accepted(), // -32601 회신 POST
            _ => not_found(),
        });
        let config = http_config(&server, None);

        let tools = manager().discover_tools_http(&config).unwrap();
        assert_eq!(tools.len(), 1);

        let requests = server.wait_captured(4);
        let reply = &requests[3];
        assert_eq!(reply.method, "POST");
        let body = reply.body_json();
        assert_eq!(body.get("id"), Some(&json!(77)));
        assert_eq!(body.pointer("/error/code"), Some(&json!(-32601)));
        assert_eq!(reply.header("mcp-session-id"), Some("s1"));
    }

    #[test]
    fn json_바디_상한_초과는_에러() {
        let server = spawn_mock(|index, request| match index {
            0 => json_reply(request, init_result(), None),
            1 => accepted(),
            2 => {
                let huge = vec![b'a'; MAX_JSON_BODY_BYTES + 1024];
                Reply::Raw(http_response(
                    200,
                    &[("Content-Type", "application/json")],
                    &huge,
                ))
            }
            _ => not_found(),
        });
        let error = manager()
            .discover_tools_http(&http_config(&server, None))
            .unwrap_err();
        assert!(format!("{error:#}").contains("상한"), "{error:#}");
    }

    #[test]
    fn sse_idle_timeout은_read_timeout으로_탈출() {
        let server = spawn_mock(|index, request| match index {
            0 => json_reply(request, init_result(), None),
            1 => accepted(),
            2 => {
                // SSE 헤더 + 미완성 이벤트만 쓰고 2초 유지 — dispatch 불가 상태.
                // Content-Length가 있으면 ureq가 EOF로 끝내므로 close-delimited로.
                let head = http_response_streaming(
                    200,
                    &[("Content-Type", "text/event-stream")],
                    b"data: {\"jsonrpc\"",
                );
                Reply::RawThenHold(head, Duration::from_secs(2))
            }
            _ => not_found(),
        });
        let manager = LocalMcpManager::new(RedactionService::new())
            .with_request_timeout(Duration::from_millis(300));
        let mut connection = manager.connect_http(&http_config(&server, None)).unwrap();

        let started = Instant::now();
        let error = connection.list_tools().unwrap_err();
        assert!(format!("{error:#}").contains("idle timeout"), "{error:#}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "read timeout이 유일한 탈출구 — 무한 대기 금지"
        );
    }

    #[test]
    fn bearer는_에러와_debug에_비노출() {
        let token = "sk-http-secret-token-42";
        let server = spawn_mock(move |_, request| {
            // 서버가 Authorization 값을 에러 바디로 에코하는 악성 케이스
            let echoed = format!(
                "denied: {}",
                request.header("authorization").unwrap_or_default()
            );
            Reply::Raw(http_response(500, &[], echoed.as_bytes()))
        });
        let config = http_config(&server, Some(token));

        assert!(
            !format!("{config:?}").contains(token),
            "config Debug에 bearer 평문 노출"
        );

        let error = manager().discover_tools_http(&config).unwrap_err();
        let text = format!("{error:#}");
        assert!(!text.contains(token), "에러에 bearer 평문 노출: {text}");
        assert!(text.contains("[REDACTED]"), "{text}");
    }

    #[test]
    fn 협상_버전이_이후_요청_헤더에_실린다() {
        let server = spawn_mock(|index, request| match index {
            0 => {
                let result = json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "serverInfo": {"name": "mock", "version": "0"},
                });
                json_reply(request, result, Some("s1"))
            }
            1 => accepted(),
            2 => json_reply(request, tools_result(), None),
            _ => not_found(),
        });
        let config = http_config(&server, None);

        let manager = manager();
        let mut connection = manager.connect_http(&config).unwrap();
        assert_eq!(connection.negotiated_version, "2025-06-18");
        connection.list_tools().unwrap();
        drop(connection);

        let requests = server.wait_captured(4);
        assert_eq!(
            requests[1].header("mcp-protocol-version"),
            Some("2025-06-18")
        );
        assert_eq!(
            requests[2].header("mcp-protocol-version"),
            Some("2025-06-18")
        );
        assert_eq!(
            requests[3].header("mcp-protocol-version"),
            Some("2025-06-18")
        );
    }

    #[test]
    fn 지원_목록_밖_버전은_http에서도_거부() {
        let server = spawn_mock(|index, request| match index {
            0 => {
                let result = json!({
                    "protocolVersion": "1999-01-01",
                    "capabilities": {},
                    "serverInfo": {"name": "mock", "version": "0"},
                });
                json_reply(request, result, None)
            }
            _ => not_found(),
        });
        let error = manager()
            .connect_http(&http_config(&server, None))
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("1999-01-01"), "{text}");
        assert!(text.contains("미지원"), "{text}");
    }

    #[test]
    fn 검증_실패_url은_네트워크_없이_거부() {
        let config = McpHttpServerConfig {
            name: "bad".to_owned(),
            url: "http://evil.example.com/mcp".to_owned(),
            bearer: None,
        };
        let error = manager().connect_http(&config).unwrap_err();
        assert!(
            format!("{error:#}").contains("localhost/루프백만 허용"),
            "{error:#}"
        );
    }
}
