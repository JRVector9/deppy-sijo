//! Responses→Chat 변환 프록시 (PR-L5).
//!
//! codex(0.144.5)는 custom 프로바이더에 `wire_api=responses`만 허용하지만
//! (chat은 폐기 — codex#7782), ollama 계열 원격/로컬 OpenAI 호환 API 대부분은
//! `/v1/chat/completions`만 지원한다(`/v1/responses` 404). 이 프록시가
//! 127.0.0.1 임시 포트에서 codex의 `POST /v1/responses`를 받아 upstream
//! `/chat/completions`(SSE)로 변환·중계한다. 실측 원형: 파이썬 프록시로
//! codex exec·app-server 턴 완주 검증(2026-07-18).
//!
//! tokio 금지 관례 — std TcpListener + 연결당 스레드 + sync ureq. 요청은
//! codex가 보내는 POST /v1/responses(+GET /v1/models)뿐이라 최소 HTTP/1.1
//! 파싱으로 충분하다. 변환은 순수 함수로 분리해 단위 테스트한다.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::Context;
use serde_json::{Value, json};

/// upstream WAF가 기본/파이썬 UA를 403으로 차단한다(실측) — 명시 UA 필수.
const PROXY_USER_AGENT: &str = "deppy-sijo-llm-proxy/0.1";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 스트리밍 중 per-read 유휴 상한 — 전체 시간 제한이 아니라 생성이 길어도 안전.
const UPSTREAM_READ_TIMEOUT: Duration = Duration::from_secs(180);
const MODELS_TIMEOUT: Duration = Duration::from_secs(30);
/// 클라이언트(codex) 요청 수신 상한 — 컨텍스트 전체가 와도 수십 KB 수준(실측).
const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;
const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// 떠 있는 변환 프록시. Drop 시 accept 루프를 깨워 종료한다 — 소유자는
/// CodexAppServerClient(client 수명 = 프록시 수명).
pub struct LlmProxyHandle {
    pub port: u16,
    shutdown: Arc<AtomicBool>,
    accept_worker: Option<thread::JoinHandle<()>>,
}

impl Drop for LlmProxyHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // accept 블로킹 해제 — 자기 포트로 한 번 접속해 루프가 플래그를 보게 한다.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(worker) = self.accept_worker.take() {
            let _ = worker.join();
        }
    }
}

struct ProxyState {
    /// OpenAI 호환 base URL (`…/v1`, 끝 `/` 제거) — `/chat/completions`·`/models`를 붙인다.
    upstream_base: String,
    /// upstream Bearer 키 — 프록시가 보유하고 요청에만 붙인다(자식 프로세스 env 미노출).
    api_key: Option<secret::SecretString>,
}

/// 127.0.0.1 임시 포트에 변환 프록시를 띄운다. `upstream_base`는 OpenAI 호환
/// base URL(`https://host/v1` 형태). 반환 핸들의 `port`로 codex base_url을
/// `http://127.0.0.1:{port}/v1`로 조립한다.
pub fn spawn(
    upstream_base: String,
    api_key: Option<secret::SecretString>,
) -> anyhow::Result<LlmProxyHandle> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("LLM 프록시 포트 바인드 실패")?;
    let port = listener
        .local_addr()
        .context("LLM 프록시 포트 확인 실패")?
        .port();
    let shutdown = Arc::new(AtomicBool::new(false));
    let state = Arc::new(ProxyState {
        upstream_base: upstream_base.trim_end_matches('/').to_owned(),
        api_key,
    });
    let accept_shutdown = Arc::clone(&shutdown);
    let accept_worker = thread::Builder::new()
        .name("llm-proxy-accept".to_owned())
        .spawn(move || {
            for connection in listener.incoming() {
                if accept_shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = connection else { continue };
                let state = Arc::clone(&state);
                let spawned = thread::Builder::new()
                    .name("llm-proxy-conn".to_owned())
                    .spawn(move || {
                        if let Err(error) = handle_connection(stream, &state) {
                            tracing::debug!("llm-proxy 연결 처리 실패: {error:#}");
                        }
                    });
                if let Err(error) = spawned {
                    tracing::warn!("llm-proxy 연결 스레드 spawn 실패: {error}");
                }
            }
        })
        .context("LLM 프록시 accept 스레드 생성 실패")?;
    Ok(LlmProxyHandle {
        port,
        shutdown,
        accept_worker: Some(accept_worker),
    })
}

// ---------------------------------------------------------------------------
// HTTP 서비스 (연결당 스레드, keep-alive 미지원 — 응답마다 Connection: close)
// ---------------------------------------------------------------------------

struct HttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn handle_connection(mut stream: TcpStream, state: &ProxyState) -> anyhow::Result<()> {
    stream.set_read_timeout(Some(CLIENT_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone().context("클라이언트 소켓 복제 실패")?);
    let request = read_http_request(&mut reader)?;
    let path = request.path.trim_end_matches('/');
    match (request.method.as_str(), path) {
        ("POST", path) if path.ends_with("/responses") => {
            handle_responses(&mut stream, state, &request.body)
        }
        ("GET", path) if path.ends_with("/models") => handle_models(&mut stream, state),
        _ => write_empty_response(&mut stream, "404 Not Found"),
    }
}

/// 최소 HTTP/1.1 요청 파싱 — 요청 줄 + 헤더(Content-Length만 사용) + 본문.
fn read_http_request(reader: &mut impl BufRead) -> anyhow::Result<HttpRequest> {
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .context("요청 줄 수신 실패")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().context("HTTP 메서드 없음")?.to_owned();
    let path = parts.next().context("HTTP 경로 없음")?.to_owned();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).context("헤더 수신 실패")?;
        anyhow::ensure!(read > 0, "헤더 도중 연결 종료");
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().context("Content-Length 파싱 실패")?;
        }
    }
    anyhow::ensure!(
        content_length <= MAX_REQUEST_BODY,
        "요청 본문이 너무 큼 ({content_length} bytes)"
    );
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).context("본문 수신 실패")?;
    Ok(HttpRequest { method, path, body })
}

/// POST /v1/responses — 변환 후 upstream chat SSE를 Responses SSE로 중계.
fn handle_responses(stream: &mut TcpStream, state: &ProxyState, body: &[u8]) -> anyhow::Result<()> {
    let Ok(request) = serde_json::from_slice::<Value>(body) else {
        return write_empty_response(stream, "400 Bad Request");
    };
    let chat_request = responses_to_chat_request(&request);
    // SSE 헤더부터 보낸다 — 이후 오류는 HTTP 상태가 아니라 response.failed 이벤트로.
    stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
    )?;
    let response_id = format!("resp_{}", deppy_core::time::unix_ms());
    let mut machine = ChatStreamState::new(response_id.clone());
    stream.write_all(sse_frame(&machine.created_event()).as_bytes())?;
    match stream_upstream(stream, state, &chat_request, &mut machine) {
        Ok(()) => {
            for event in machine.finish() {
                stream.write_all(sse_frame(&event).as_bytes())?;
            }
        }
        Err(error) => {
            tracing::warn!("llm-proxy upstream 오류: {error:#}");
            let failed = failed_event(&response_id, &format!("{error:#}"));
            stream.write_all(sse_frame(&failed).as_bytes())?;
        }
    }
    stream.flush()?;
    Ok(())
}

/// upstream /chat/completions 스트림을 읽으며 텍스트 델타를 즉시 중계한다.
fn stream_upstream(
    stream: &mut TcpStream,
    state: &ProxyState,
    chat_request: &Value,
    machine: &mut ChatStreamState,
) -> anyhow::Result<()> {
    let agent = ureq::builder()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(UPSTREAM_READ_TIMEOUT)
        .user_agent(PROXY_USER_AGENT)
        .build();
    let mut request = agent
        .post(&format!("{}/chat/completions", state.upstream_base))
        .set("Content-Type", "application/json");
    if let Some(key) = &state.api_key {
        request = request.set("Authorization", &format!("Bearer {}", key.expose()));
    }
    let response = request
        .send_string(&chat_request.to_string())
        .map_err(describe_upstream_error)?;
    let mut reader = BufReader::new(response.into_reader());
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .context("upstream 스트림 수신 실패")?;
        if read == 0 {
            break;
        }
        let Some(payload) = sse_data_payload(&line) else {
            continue;
        };
        if payload == "[DONE]" {
            break;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        for event in machine.on_chunk(&chunk) {
            stream.write_all(sse_frame(&event).as_bytes())?;
        }
    }
    Ok(())
}

/// GET /v1/models 패스스루 — codex가 목록 조회를 할 때 그대로 넘겨준다.
fn handle_models(stream: &mut TcpStream, state: &ProxyState) -> anyhow::Result<()> {
    let agent = ureq::builder()
        .timeout(MODELS_TIMEOUT)
        .user_agent(PROXY_USER_AGENT)
        .build();
    let mut request = agent.get(&format!("{}/models", state.upstream_base));
    if let Some(key) = &state.api_key {
        request = request.set("Authorization", &format!("Bearer {}", key.expose()));
    }
    match request.call() {
        Ok(response) => {
            let body = response.into_string().unwrap_or_default();
            write_json_response(stream, "200 OK", body.as_bytes())
        }
        Err(error) => {
            tracing::debug!("llm-proxy models 패스스루 실패: {error:#}");
            write_empty_response(stream, "502 Bad Gateway")
        }
    }
}

/// upstream 오류를 짧은 메시지로 — 4xx/5xx는 본문 발췌 포함 (키는 포함되지 않음).
fn describe_upstream_error(error: ureq::Error) -> anyhow::Error {
    match error {
        ureq::Error::Status(code, response) => {
            let body = response.into_string().unwrap_or_default();
            let excerpt: String = body.chars().take(300).collect();
            anyhow::anyhow!("upstream HTTP {code}: {excerpt}")
        }
        other => anyhow::anyhow!("upstream 연결 실패: {other}"),
    }
}

fn write_empty_response(stream: &mut TcpStream, status: &str) -> anyhow::Result<()> {
    stream.write_all(
        format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
    )?;
    Ok(())
}

fn write_json_response(stream: &mut TcpStream, status: &str, body: &[u8]) -> anyhow::Result<()> {
    stream.write_all(
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    )?;
    stream.write_all(body)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 순수 변환 — 요청 (Responses → chat/completions)
// ---------------------------------------------------------------------------

/// Responses content(문자열 | `{text}` 파트 배열) → 평문 결합.
fn content_to_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|part| match part {
                Value::String(text) => text.as_str(),
                Value::Object(map) => map.get("text").and_then(Value::as_str).unwrap_or(""),
                _ => "",
            })
            .collect(),
        _ => String::new(),
    }
}

/// function_call_output의 output(문자열 | `{content}` 객체) → 평문.
fn function_output_text(output: Option<&Value>) -> String {
    match output {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(value @ Value::Object(map)) => {
            let text = map.get("content").map(content_to_text).unwrap_or_default();
            if text.is_empty() {
                value.to_string()
            } else {
                text
            }
        }
        Some(other) => other.to_string(),
    }
}

/// Responses input 아이템 → chat message. 미지원/`reasoning` 아이템은 None(스킵).
fn input_item_to_message(item: &Value) -> Option<Value> {
    match item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message")
    {
        "message" => Some(json!({
            "role": item.get("role").and_then(Value::as_str).unwrap_or("user"),
            "content": content_to_text(item.get("content").unwrap_or(&Value::Null)),
        })),
        // 이전 턴의 어시스턴트 툴콜 컨텍스트 유지.
        "function_call" => Some(json!({
            "role": "assistant",
            "content": Value::Null,
            "tool_calls": [{
                "id": item.get("call_id").and_then(Value::as_str).unwrap_or("call_0"),
                "type": "function",
                "function": {
                    "name": item.get("name").and_then(Value::as_str).unwrap_or(""),
                    "arguments": item.get("arguments").and_then(Value::as_str).unwrap_or("{}"),
                },
            }],
        })),
        "function_call_output" => Some(json!({
            "role": "tool",
            "tool_call_id": item.get("call_id").and_then(Value::as_str).unwrap_or("call_0"),
            "content": function_output_text(item.get("output")),
        })),
        _ => None,
    }
}

/// Responses 평면형 tools(`{type, name, description, strict, parameters}`) →
/// chat 중첩형(`{type, function: {name, description, parameters}}`). strict는
/// chat 스키마에 없어 버린다. function 외 타입은 스킵.
fn convert_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("function"))
        .map(|tool| {
            let mut function = serde_json::Map::new();
            for key in ["name", "description", "parameters"] {
                if let Some(value) = tool.get(key)
                    && !value.is_null()
                {
                    function.insert(key.to_owned(), value.clone());
                }
            }
            json!({"type": "function", "function": Value::Object(function)})
        })
        .collect()
}

/// POST /v1/responses 본문 → upstream /chat/completions 요청 본문 (순수 변환).
/// instructions → 선두 system, input[] → messages, tools 평면형 → 중첩형,
/// max_output_tokens → max_tokens. 항상 stream + stream_options.include_usage.
fn responses_to_chat_request(body: &Value) -> Value {
    let mut messages = Vec::new();
    if let Some(instructions) = body.get("instructions").and_then(Value::as_str)
        && !instructions.is_empty()
    {
        messages.push(json!({"role": "system", "content": instructions}));
    }
    let input = body
        .get("input")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    messages.extend(input.iter().filter_map(input_item_to_message));

    let mut chat = serde_json::Map::new();
    if let Some(model) = body.get("model")
        && !model.is_null()
    {
        chat.insert("model".to_owned(), model.clone());
    }
    chat.insert("messages".to_owned(), Value::Array(messages));
    let tools = body
        .get("tools")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let tools = convert_tools(tools);
    if !tools.is_empty() {
        chat.insert("tools".to_owned(), Value::Array(tools));
    }
    if let Some(max) = body.get("max_output_tokens")
        && !max.is_null()
    {
        chat.insert("max_tokens".to_owned(), max.clone());
    }
    chat.insert("stream".to_owned(), Value::Bool(true));
    // 일부 upstream은 미지원이지만 무시해도 동작한다(실측) — usage를 주면 반영.
    chat.insert("stream_options".to_owned(), json!({"include_usage": true}));
    Value::Object(chat)
}

// ---------------------------------------------------------------------------
// 순수 변환 — 응답 (chat SSE 청크 → Responses SSE 이벤트)
// ---------------------------------------------------------------------------

/// upstream `data:` 줄의 페이로드. SSE 규격상 `data:` 뒤 공백은 선택이다.
fn sse_data_payload(line: &str) -> Option<&str> {
    line.trim_end().strip_prefix("data:").map(str::trim)
}

/// Responses SSE 프레임 — `event:` 줄 + `data:` JSON(내부 `type` 필드) 병기(실측 관례).
fn sse_frame(event: &Value) -> String {
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
    format!("event: {event_type}\ndata: {event}\n\n")
}

fn failed_event(response_id: &str, message: &str) -> Value {
    json!({
        "type": "response.failed",
        "response": {
            "id": response_id,
            "error": {"code": "upstream_error", "message": message},
        },
    })
}

/// tool_calls 델타 조립 버퍼 — index별로 id/name은 최신값, arguments는 이어붙인다.
#[derive(Default)]
struct ToolCallDraft {
    id: String,
    name: String,
    arguments: String,
}

/// chat 청크 스트림 → Responses SSE 이벤트 조립 상태기계 (순수 — I/O 없음).
/// 순서: created → (첫 텍스트 델타 전) output_item.added → output_text.delta* →
/// finish에서 output_item.done(message + function_call들) → response.completed.
struct ChatStreamState {
    response_id: String,
    message_added: bool,
    text: String,
    tool_calls: Vec<ToolCallDraft>,
    usage: Option<Value>,
}

impl ChatStreamState {
    fn new(response_id: String) -> Self {
        Self {
            response_id,
            message_added: false,
            text: String::new(),
            tool_calls: Vec::new(),
            usage: None,
        }
    }

    fn created_event(&self) -> Value {
        json!({
            "type": "response.created",
            "response": {"id": self.response_id, "status": "in_progress", "output": []},
        })
    }

    /// chat 청크 하나 소비 → 즉시 내보낼 이벤트들(텍스트 델타). tool_calls
    /// 델타는 조립만 하고 finish에서 아이템으로 흘린다.
    fn on_chunk(&mut self, chunk: &Value) -> Vec<Value> {
        let mut events = Vec::new();
        if let Some(usage) = chunk.get("usage")
            && !usage.is_null()
        {
            self.usage = Some(usage.clone());
        }
        let choices = chunk
            .get("choices")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for choice in choices {
            let Some(delta) = choice.get("delta") else {
                continue;
            };
            if let Some(piece) = delta.get("content").and_then(Value::as_str)
                && !piece.is_empty()
            {
                if !self.message_added {
                    // 첫 델타 전 output_item.added 필수 — 없으면 codex가 ERROR 로그(실측).
                    self.message_added = true;
                    events.push(json!({
                        "type": "response.output_item.added",
                        "output_index": 0,
                        "item": {
                            "type": "message", "id": "msg_0", "role": "assistant",
                            "status": "in_progress", "content": [],
                        },
                    }));
                }
                self.text.push_str(piece);
                events.push(json!({
                    "type": "response.output_text.delta",
                    "item_id": "msg_0",
                    "output_index": 0,
                    "content_index": 0,
                    "delta": piece,
                }));
            }
            let tool_calls = delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            for tool_call in tool_calls {
                self.merge_tool_call_delta(tool_call);
            }
        }
        events
    }

    fn merge_tool_call_delta(&mut self, tool_call: &Value) {
        let index = tool_call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        while self.tool_calls.len() <= index {
            self.tool_calls.push(ToolCallDraft::default());
        }
        let draft = &mut self.tool_calls[index];
        if let Some(id) = tool_call.get("id").and_then(Value::as_str)
            && !id.is_empty()
        {
            draft.id = id.to_owned();
        }
        if let Some(function) = tool_call.get("function") {
            if let Some(name) = function.get("name").and_then(Value::as_str)
                && !name.is_empty()
            {
                draft.name = name.to_owned();
            }
            if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                draft.arguments.push_str(arguments);
            }
        }
    }

    /// 스트림 정상 종료 → 완료 이벤트들. message가 있으면 output_index 0,
    /// function_call 아이템들이 이어진다.
    fn finish(self) -> Vec<Value> {
        let mut events = Vec::new();
        let mut output = Vec::new();
        if self.message_added {
            let item = json!({
                "type": "message", "id": "msg_0", "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": self.text, "annotations": []}],
            });
            events.push(json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": item,
            }));
            output.push(item);
        }
        let base_index = usize::from(self.message_added);
        for (i, draft) in self.tool_calls.iter().enumerate() {
            let output_index = base_index + i;
            let call_id = if draft.id.is_empty() {
                format!("call_{i}")
            } else {
                draft.id.clone()
            };
            // codex는 arguments를 JSON으로 파싱한다 — 빈 스트림이면 "{}"로.
            let arguments = if draft.arguments.is_empty() {
                "{}".to_owned()
            } else {
                draft.arguments.clone()
            };
            let item = json!({
                "type": "function_call", "id": format!("fc_{i}"), "call_id": call_id,
                "name": draft.name, "arguments": arguments, "status": "completed",
            });
            let mut added_item = item.clone();
            added_item["status"] = Value::String("in_progress".to_owned());
            events.push(json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": added_item,
            }));
            events.push(json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": item,
            }));
            output.push(item);
        }
        events.push(json!({
            "type": "response.completed",
            "response": {
                "id": self.response_id,
                "status": "completed",
                "output": output,
                "usage": usage_value(self.usage.as_ref()),
            },
        }));
        events
    }
}

/// chat usage → Responses usage. upstream이 usage를 안 주면 전부 0(실측 관례).
fn usage_value(usage: Option<&Value>) -> Value {
    let map = usage.and_then(Value::as_object);
    let field = |key: &str| {
        map.and_then(|m| m.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    json!({
        "input_tokens": field("prompt_tokens"),
        "input_tokens_details": {"cached_tokens": 0},
        "output_tokens": field("completion_tokens"),
        "output_tokens_details": {"reasoning_tokens": 0},
        "total_tokens": field("total_tokens"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    // ------------------------------------------------------------------
    // 요청 변환
    // ------------------------------------------------------------------

    #[test]
    fn content_to_text는_문자열과_파트_배열을_결합한다() {
        assert_eq!(content_to_text(&json!("plain")), "plain");
        assert_eq!(
            content_to_text(&json!([
                {"type": "input_text", "text": "안녕"},
                {"type": "input_text", "text": " 세계"},
            ])),
            "안녕 세계"
        );
        // text 없는 파트/비객체는 건너뛴다.
        assert_eq!(
            content_to_text(&json!([{"type": "input_image"}, "raw", 3])),
            "raw"
        );
        assert_eq!(content_to_text(&Value::Null), "");
    }

    #[test]
    fn 요청_변환은_instructions를_system_선두로_배치한다() {
        // proxy_log.jsonl의 실요청 형태(축약) 기반 fixture.
        let body = json!({
            "model": "qwen3:8b",
            "instructions": "You are a coding agent.",
            "input": [
                {"type": "message", "role": "developer",
                 "content": [{"type": "input_text", "text": "<permissions>read-only</permissions>"}]},
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "Reply with HELLO"}]},
            ],
            "tool_choice": "auto",
            "store": false,
            "stream": true,
        });
        let chat = responses_to_chat_request(&body);
        assert_eq!(chat["model"], "qwen3:8b");
        assert_eq!(
            chat["messages"],
            json!([
                {"role": "system", "content": "You are a coding agent."},
                {"role": "developer", "content": "<permissions>read-only</permissions>"},
                {"role": "user", "content": "Reply with HELLO"},
            ])
        );
        assert_eq!(chat["stream"], json!(true));
        assert_eq!(chat["stream_options"], json!({"include_usage": true}));
        // tools 없음 → 키 자체가 없어야 한다.
        assert!(chat.get("tools").is_none());
        assert!(chat.get("max_tokens").is_none());
    }

    #[test]
    fn 요청_변환은_함수콜_왕복_아이템을_chat_형태로_바꾼다() {
        let body = json!({
            "input": [
                {"type": "function_call", "call_id": "call_1",
                 "name": "exec_command", "arguments": "{\"cmd\":\"ls\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "file.txt"},
                {"type": "reasoning", "summary": []},
                {"type": "unknown_future_item"},
            ],
        });
        let chat = responses_to_chat_request(&body);
        assert_eq!(
            chat["messages"],
            json!([
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_1", "type": "function",
                    "function": {"name": "exec_command", "arguments": "{\"cmd\":\"ls\"}"},
                }]},
                {"role": "tool", "tool_call_id": "call_1", "content": "file.txt"},
            ])
        );
    }

    #[test]
    fn 함수콜_출력은_객체_형태도_평문으로_바꾼다() {
        assert_eq!(
            function_output_text(Some(&json!({"content": [{"text": "ok"}]}))),
            "ok"
        );
        // content가 없으면 JSON 원문으로 남긴다(정보 손실 방지, 원형 관례).
        assert_eq!(
            function_output_text(Some(&json!({"success": true}))),
            "{\"success\":true}"
        );
        assert_eq!(function_output_text(None), "");
    }

    #[test]
    fn tools_변환은_평면형을_중첩형으로_바꾸고_strict를_버린다() {
        // proxy_log.jsonl의 exec_command 도구 형태(축약).
        let tools = [json!({
            "type": "function", "name": "exec_command",
            "description": "Runs a command in a PTY.",
            "strict": false,
            "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}},
        })];
        assert_eq!(
            convert_tools(&tools),
            vec![json!({
                "type": "function",
                "function": {
                    "name": "exec_command",
                    "description": "Runs a command in a PTY.",
                    "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}},
                },
            })]
        );
        // function 외 타입은 스킵.
        assert!(convert_tools(&[json!({"type": "web_search"})]).is_empty());
    }

    #[test]
    fn 요청_변환은_max_output_tokens를_max_tokens로_옮긴다() {
        let chat = responses_to_chat_request(&json!({"max_output_tokens": 1024, "input": []}));
        assert_eq!(chat["max_tokens"], json!(1024));
    }

    // ------------------------------------------------------------------
    // 응답 SSE 조립
    // ------------------------------------------------------------------

    #[test]
    fn 텍스트_델타는_첫_델타_전_output_item_added를_보장한다() {
        let mut machine = ChatStreamState::new("resp_1".to_owned());
        assert_eq!(machine.created_event()["type"], "response.created");
        let first = machine.on_chunk(&json!({
            "choices": [{"delta": {"role": "assistant", "content": "Hel"}}],
        }));
        assert_eq!(first.len(), 2);
        assert_eq!(first[0]["type"], "response.output_item.added");
        assert_eq!(first[0]["item"]["type"], "message");
        assert_eq!(first[1]["type"], "response.output_text.delta");
        assert_eq!(first[1]["delta"], "Hel");
        // 두 번째 델타부터는 delta만.
        let second = machine.on_chunk(&json!({
            "choices": [{"delta": {"content": "lo"}}],
        }));
        assert_eq!(second.len(), 1);
        assert_eq!(second[0]["delta"], "lo");

        let done = machine.finish();
        assert_eq!(done.len(), 2);
        assert_eq!(done[0]["type"], "response.output_item.done");
        assert_eq!(done[0]["item"]["content"][0]["text"], "Hello");
        assert_eq!(done[0]["item"]["content"][0]["annotations"], json!([]));
        assert_eq!(done[1]["type"], "response.completed");
        assert_eq!(done[1]["response"]["output"].as_array().unwrap().len(), 1);
        // usage 미제공 → 0.
        assert_eq!(done[1]["response"]["usage"]["input_tokens"], 0);
        assert_eq!(done[1]["response"]["usage"]["total_tokens"], 0);
    }

    #[test]
    fn usage_청크는_completed의_responses_usage로_매핑된다() {
        let mut machine = ChatStreamState::new("resp_u".to_owned());
        machine.on_chunk(&json!({"choices": [{"delta": {"content": "x"}}]}));
        machine.on_chunk(&json!({
            "choices": [],
            "usage": {"prompt_tokens": 12, "completion_tokens": 34, "total_tokens": 46},
        }));
        let done = machine.finish();
        let usage = &done.last().unwrap()["response"]["usage"];
        assert_eq!(usage["input_tokens"], 12);
        assert_eq!(usage["input_tokens_details"]["cached_tokens"], 0);
        assert_eq!(usage["output_tokens"], 34);
        assert_eq!(usage["output_tokens_details"]["reasoning_tokens"], 0);
        assert_eq!(usage["total_tokens"], 46);
    }

    #[test]
    fn tool_calls_델타는_function_call_아이템으로_조립된다() {
        let mut machine = ChatStreamState::new("resp_t".to_owned());
        // OpenAI 스트리밍 관례: 첫 청크에 id/name, 이후 arguments 조각.
        let events = machine.on_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": "call_abc", "type": "function",
                "function": {"name": "exec_command", "arguments": "{\"cm"},
            }]}}],
        }));
        assert!(
            events.is_empty(),
            "tool_calls 델타는 즉시 이벤트를 내지 않는다"
        );
        machine.on_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0, "function": {"arguments": "d\":\"ls\"}"},
            }]}}],
        }));
        let done = machine.finish();
        // added(in_progress) → done(completed) → completed.
        assert_eq!(done.len(), 3);
        assert_eq!(done[0]["type"], "response.output_item.added");
        assert_eq!(done[0]["output_index"], 0);
        assert_eq!(done[0]["item"]["status"], "in_progress");
        assert_eq!(done[1]["type"], "response.output_item.done");
        let item = &done[1]["item"];
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["call_id"], "call_abc");
        assert_eq!(item["name"], "exec_command");
        assert_eq!(item["arguments"], "{\"cmd\":\"ls\"}");
        assert_eq!(item["status"], "completed");
        assert_eq!(done[2]["response"]["output"], json!([item]));
    }

    #[test]
    fn 텍스트와_tool_call이_함께_오면_output_index가_이어진다() {
        let mut machine = ChatStreamState::new("resp_m".to_owned());
        machine.on_chunk(&json!({"choices": [{"delta": {"content": "생각 중"}}]}));
        machine.on_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": "call_1", "function": {"name": "f", "arguments": "{}"},
            }]}}],
        }));
        let done = machine.finish();
        // message done(0) → tool added/done(1) → completed.
        assert_eq!(done[0]["output_index"], 0);
        assert_eq!(done[1]["output_index"], 1);
        assert_eq!(done[2]["output_index"], 1);
        assert_eq!(done[3]["response"]["output"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn sse_프레임은_event_줄과_data_json을_병기한다() {
        let frame = sse_frame(&json!({"type": "response.output_text.delta", "delta": "x"}));
        assert!(frame.starts_with("event: response.output_text.delta\ndata: {"));
        assert!(frame.ends_with("\n\n"));
        // data 줄의 JSON에도 내부 type 필드가 남아 있다.
        assert!(frame.contains("\"type\":\"response.output_text.delta\""));
    }

    #[test]
    fn sse_data_payload는_공백_유무와_done_마커를_처리한다() {
        assert_eq!(sse_data_payload("data: {\"a\":1}\n"), Some("{\"a\":1}"));
        assert_eq!(sse_data_payload("data:[DONE]\n"), Some("[DONE]"));
        assert_eq!(sse_data_payload(": keepalive\n"), None);
        assert_eq!(sse_data_payload("\n"), None);
    }

    #[test]
    fn failed_이벤트는_오류_코드와_메시지를_담는다() {
        let event = failed_event("resp_9", "upstream HTTP 403: blocked");
        assert_eq!(event["type"], "response.failed");
        assert_eq!(event["response"]["id"], "resp_9");
        assert_eq!(event["response"]["error"]["code"], "upstream_error");
        assert_eq!(
            event["response"]["error"]["message"],
            "upstream HTTP 403: blocked"
        );
    }

    // ------------------------------------------------------------------
    // HTTP 파싱/서비스
    // ------------------------------------------------------------------

    #[test]
    fn http_요청_파싱은_content_length_본문을_읽는다() {
        let raw = b"POST /v1/responses HTTP/1.1\r\nHost: x\r\ncontent-length: 4\r\n\r\nbody";
        let request = read_http_request(&mut &raw[..]).unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1/responses");
        assert_eq!(request.body, b"body");
    }

    #[test]
    fn http_요청_파싱은_본문_없는_get을_처리한다() {
        let raw = b"GET /v1/models HTTP/1.1\r\nHost: x\r\n\r\n";
        let request = read_http_request(&mut &raw[..]).unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/v1/models");
        assert!(request.body.is_empty());
    }

    #[test]
    fn 프록시는_미지_경로에_404를_주고_drop으로_종료된다() {
        // 실 네트워크 불필요 — 루프백에서 라우팅과 수명만 확인한다.
        let handle = spawn("http://127.0.0.1:9/v1".to_owned(), None).unwrap();
        let port = handle.port;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .write_all(b"POST /nope HTTP/1.1\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
        drop(handle);
        // 종료 후 새 연결은 거부되어야 한다 (accept 루프 종료 + 리스너 drop).
        assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    }

    // ------------------------------------------------------------------
    // 실 E2E (수동): ABC_KEY(필수)/ABC_UPSTREAM/ABC_MODEL env로 실행.
    //   cargo test -p deppy-sijo llm_proxy -- --ignored --nocapture
    // ------------------------------------------------------------------

    fn e2e_env() -> (String, String, String) {
        let key = std::env::var("ABC_KEY").expect("ABC_KEY env 필요 (실 원격 LLM 키)");
        let upstream = std::env::var("ABC_UPSTREAM")
            .unwrap_or_else(|_| "https://abcllm-api.brut.bot".to_owned());
        let model = std::env::var("ABC_MODEL").unwrap_or_else(|_| "qwen3:8b".to_owned());
        (key, upstream, model)
    }

    #[test]
    #[ignore = "실 원격 LLM 필요 — ABC_KEY env로 수동 실행"]
    fn e2e_responses_스모크_턴_완주() {
        let (key, upstream, model) = e2e_env();
        let handle = spawn(
            format!("{upstream}/v1"),
            Some(secret::SecretString::new(key)),
        )
        .unwrap();
        let body = json!({
            "model": model,
            "instructions": "You are a terse assistant.",
            "input": [{"type": "message", "role": "user", "content": [
                {"type": "input_text",
                 "text": "Reply with exactly the text HELLO_RUST_PROXY and nothing else."},
            ]}],
            "stream": true,
        })
        .to_string();
        let mut stream = TcpStream::connect(("127.0.0.1", handle.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(200)))
            .unwrap();
        stream
            .write_all(
                format!(
                    "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        println!("--- SSE 응답 (뒤 1200자) ---");
        let tail: String = response
            .chars()
            .rev()
            .take(1200)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        println!("{tail}");
        assert!(response.contains("event: response.created"), "{response}");
        assert!(
            response.contains("event: response.output_text.delta"),
            "{response}"
        );
        assert!(response.contains("event: response.completed"), "{response}");
        assert!(response.contains("HELLO_RUST_PROXY"), "{response}");
    }

    #[test]
    #[ignore = "실 원격 LLM + codex 바이너리 필요 — ABC_KEY env로 수동 실행"]
    fn e2e_codex_exec_턴_완주() {
        let (key, upstream, model) = e2e_env();
        let handle = spawn(
            format!("{upstream}/v1"),
            Some(secret::SecretString::new(key)),
        )
        .unwrap();
        let home = std::env::temp_dir().join(format!("deppy-llm-proxy-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config.toml"),
            format!(
                "model = \"{model}\"\nmodel_provider = \"deppy_local\"\n\n\
                 [model_providers.deppy_local]\nname = \"deppy_local\"\n\
                 base_url = \"http://127.0.0.1:{}/v1\"\nwire_api = \"responses\"\n",
                handle.port
            ),
        )
        .unwrap();
        let output = std::process::Command::new("codex")
            .args([
                "exec",
                "--skip-git-repo-check",
                "Reply with exactly the text HELLO_CODEX_EXEC and nothing else.",
            ])
            .env("CODEX_HOME", &home)
            .current_dir(&home)
            .output()
            .expect("codex exec 실행 실패");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        println!("--- codex exec stdout (뒤 1200자) ---");
        let tail: String = stdout
            .chars()
            .rev()
            .take(1200)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        println!("{tail}");
        assert!(
            stdout.contains("HELLO_CODEX_EXEC"),
            "stdout: {stdout}\nstderr: {stderr}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
