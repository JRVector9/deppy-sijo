//! Agent-proxy: deppy를 MCP **서버**로 세우는 재사용 프리미티브 (§1.5 후속).
//! 에이전트(예: 패널에서 도는 Claude Code)가 이 서버에 붙으면, 각 tool 호출은
//! 여기서 permission/audit 판단(hook)을 거친 뒤 실제 백엔드 MCP 서버로 포워딩된다.
//!
//! 이 파일은 순수 프리미티브만 제공한다 — 앱/에이전트 배선은 하지 않는다:
//!   - `run_authorized_proxy`: bounded/zeroizing stdio JSON-RPC server loop
//!   - `AuthorizedToolExecutor`: permission/audit/backend call을 한 소유권 경로로 결합
//!
//! transport.rs와 같은 방어적 스타일을 따른다 — 라인 길이 상한, 악의적/깨진
//! 입력에 대해 panic 없이 graceful 처리, 한 줄 나쁜 입력에 무한 블록 금지.

use std::borrow::Cow;
#[cfg(test)]
use std::io::{BufRead, BufReader};
use std::io::{Read, Write};

use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Value, json};

use crate::PROTOCOL_VERSION;
use crate::limits::MAX_TOOL_INPUT_BYTES;
#[cfg(test)]
use crate::sensitive::zeroize_json_value;
use crate::sensitive::{SensitiveBytes, volatile_zeroize};

/// Proxy request ceiling is deliberately independent of the 8 MiB backend-response ceiling.
/// A valid call needs at most 32 KiB of tool arguments plus a small JSON-RPC envelope; 64 KiB
/// leaves headroom for identifiers and tool names without letting request RAM scale to responses.
const MAX_PROXY_REQUEST_BYTES: usize = 64 * 1024;

/// tools/list·tools/call을 실제 백엔드 MCP 서버로 위임하는 훅.
/// proxy는 백엔드 구현을 알 필요가 없다 — 호출측이 LocalMcpManager 등으로 구현한다.
#[cfg(test)]
trait ToolForwarder {
    /// tools/list 결과 (MCP `{"tools":[...]}` 형태의 result Value).
    fn list_tools(&self) -> anyhow::Result<Value>;
    /// tools/call 결과 (MCP `{"content":[...],"isError":bool}` 형태의 result Value).
    fn call_tool(&self, name: &str, arguments: Value) -> anyhow::Result<Value>;
}

/// tools/call 한 건에 대한 permission/audit 판단.
/// proxy는 audit crate에 의존하지 않는다 — 호출측이 정책+기록으로 구현한다.
#[cfg(test)]
trait PermissionHook {
    fn check(&self, tool_name: &str, arguments: &Value) -> ProxyDecision;
}

/// PermissionHook의 판단 결과.
#[cfg(test)]
enum ProxyDecision {
    /// 백엔드로 포워딩 허용
    Allow,
    /// 거부 — reason은 에이전트에게 돌려줄 사유 텍스트
    Deny(String),
}

/// Bounded, single-owner tool arguments for production authorization executors.
///
/// The type deliberately implements neither `Clone` nor `Serialize`. Debug never exposes bytes,
/// and Drop overwrites the allocation before release. The proxy constructs it only after parsing
/// and validating a JSON object within the existing 32 KiB tool-input ceiling.
pub struct SensitiveToolInput {
    bytes: SensitiveBytes,
}

impl SensitiveToolInput {
    fn from_object_bytes(value: &[u8]) -> anyhow::Result<Self> {
        let raw: &RawValue = serde_json::from_slice(value)?;
        anyhow::ensure!(raw_is_object(raw), "tool arguments must be a JSON object");
        anyhow::ensure!(
            value.len() <= MAX_TOOL_INPUT_BYTES,
            "tool arguments exceed {MAX_TOOL_INPUT_BYTES} bytes"
        );
        Ok(Self {
            bytes: SensitiveBytes::copy_from_slice(value),
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    #[cfg(test)]
    fn observe_zeroized_drop(&mut self, observer: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        self.bytes.observe_zeroized_drop(observer);
    }
}

impl std::fmt::Debug for SensitiveToolInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SensitiveToolInput(REDACTED)")
    }
}

/// Combined production proxy executor. It owns permission, approval, audit, and backend call as
/// one operation so raw arguments cannot be retained in a split hook/forwarder handoff.
pub trait AuthorizedToolExecutor {
    fn list_tools(&self) -> anyhow::Result<Value>;

    fn execute_tool(&self, tool_name: String, input: SensitiveToolInput) -> AuthorizedToolOutcome;
}

/// Closed production result: only successful structured content may carry a Value. Every failure
/// is a low-cardinality code rendered by the runner, so raw backend/audit errors cannot escape.
pub enum AuthorizedToolOutcome {
    Success(Value),
    Error(AuthorizedToolError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizedToolError {
    InvalidInput,
    PermissionDenied,
    AuditUnavailable,
    DeliveryUnknown,
    BackendFailed,
}

enum AuthorizedRequest {
    Initialize {
        id: Value,
    },
    ListTools {
        id: Value,
    },
    CallTool {
        id: Value,
        tool_name: String,
        input: SensitiveToolInput,
    },
    Response(Value),
    Notification,
}

/// Production stdio proxy path with one combined authorization executor and zeroizing raw input.
pub fn run_authorized_proxy<R, W, E>(reader: R, mut writer: W, executor: E) -> anyhow::Result<()>
where
    R: Read,
    W: Write,
    E: AuthorizedToolExecutor,
{
    let mut reader = ZeroizingLineReader::new(reader);
    loop {
        match reader.read_line_capped(MAX_PROXY_REQUEST_BYTES) {
            SensitiveLineRead::Eof | SensitiveLineRead::Io => return Ok(()),
            SensitiveLineRead::TooLong => {
                anyhow::bail!(
                    "proxy: 요청 라인 상한({MAX_PROXY_REQUEST_BYTES} bytes) 초과 — 커넥션 종료"
                );
            }
            SensitiveLineRead::Line(line) => {
                if line.as_slice().iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let request = parse_authorized_request(line.as_slice());
                // `line` is dropped and zeroized before any executor callback. Parsing borrows
                // raw fields directly from this buffer and copies only the bounded arguments
                // into `SensitiveToolInput`; no secret-bearing serde String/Value is allocated.
                // The reader may retain only a fixed <=8 KiB suffix belonging to a later
                // pipelined request; it never retains a duplicate of this request line.
                drop(line);
                let response = match request {
                    AuthorizedRequest::Initialize { id } => Some(initialize_response(id)),
                    AuthorizedRequest::ListTools { id } => Some(match executor.list_tools() {
                        Ok(result) => success_response(id, result),
                        Err(_) => error_response(id, -32603, "tools/list failed"),
                    }),
                    AuthorizedRequest::CallTool {
                        id,
                        tool_name,
                        input,
                    } => Some(success_response(
                        id,
                        sanitized_tool_outcome(executor.execute_tool(tool_name, input)),
                    )),
                    AuthorizedRequest::Response(response) => Some(response),
                    AuthorizedRequest::Notification => None,
                };
                if let Some(response) = response {
                    write_message(&mut writer, &response)?;
                }
            }
        }
    }
}

fn sanitized_tool_outcome(outcome: AuthorizedToolOutcome) -> Value {
    match outcome {
        AuthorizedToolOutcome::Success(value)
            if value.is_object() && value.get("isError").and_then(Value::as_bool) != Some(true) =>
        {
            value
        }
        AuthorizedToolOutcome::Success(_) => fixed_tool_error(AuthorizedToolError::BackendFailed),
        AuthorizedToolOutcome::Error(error) => fixed_tool_error(error),
    }
}

fn fixed_tool_error(error: AuthorizedToolError) -> Value {
    let code = match error {
        AuthorizedToolError::InvalidInput => "invalid_input",
        AuthorizedToolError::PermissionDenied => "permission_denied",
        AuthorizedToolError::AuditUnavailable => "audit_unavailable",
        AuthorizedToolError::DeliveryUnknown => "delivery_unknown_no_retry",
        AuthorizedToolError::BackendFailed => "backend_failed",
    };
    json!({
        "content": [{"type":"text", "text": code}],
        "isError": true,
    })
}

/// Buffered line reader whose consumed bytes are overwritten before it yields a line. Unlike
/// `BufReader`, it does not leave an inaccessible copy of the current request in a read-ahead
/// buffer while an authorization callback blocks.
struct ZeroizingLineReader<R> {
    inner: R,
    pending: Vec<u8>,
}

enum SensitiveLineRead {
    Line(SensitiveBytes),
    Eof,
    TooLong,
    Io,
}

impl<R: Read> ZeroizingLineReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            pending: Vec::with_capacity(8192),
        }
    }

    fn read_line_capped(&mut self, max: usize) -> SensitiveLineRead {
        let mut line = SensitiveBytes::with_capacity(8192.min(max));
        loop {
            if let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
                if line.as_slice().len().saturating_add(newline) > max {
                    volatile_zeroize(&mut self.pending);
                    self.pending.clear();
                    return SensitiveLineRead::TooLong;
                }
                let _ = std::io::Write::write_all(&mut line, &self.pending[..newline]);
                let old_len = self.pending.len();
                let suffix_start = newline + 1;
                let new_len = old_len - suffix_start;
                self.pending.copy_within(suffix_start..old_len, 0);
                volatile_zeroize(&mut self.pending[new_len..old_len]);
                self.pending.truncate(new_len);
                return SensitiveLineRead::Line(line);
            }
            if !self.pending.is_empty() {
                if line.as_slice().len().saturating_add(self.pending.len()) > max {
                    volatile_zeroize(&mut self.pending);
                    self.pending.clear();
                    return SensitiveLineRead::TooLong;
                }
                let _ = std::io::Write::write_all(&mut line, &self.pending);
                volatile_zeroize(&mut self.pending);
                self.pending.clear();
            }

            let mut chunk = [0u8; 8192];
            match self.inner.read(&mut chunk) {
                Ok(0) => {
                    return if line.as_slice().is_empty() {
                        SensitiveLineRead::Eof
                    } else {
                        SensitiveLineRead::Line(line)
                    };
                }
                Ok(read) => {
                    self.pending.extend_from_slice(&chunk[..read]);
                    volatile_zeroize(&mut chunk[..read]);
                }
                Err(_) => {
                    return SensitiveLineRead::Io;
                }
            }
        }
    }
}

impl<R> Drop for ZeroizingLineReader<R> {
    fn drop(&mut self) {
        volatile_zeroize(&mut self.pending);
    }
}

#[derive(Default)]
struct OptionalRaw<'a>(Option<&'a RawValue>);

impl<'de: 'a, 'a> Deserialize<'de> for OptionalRaw<'a> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        <&'de RawValue>::deserialize(deserializer).map(|value| Self(Some(value)))
    }
}

#[derive(Deserialize)]
struct BorrowedEnvelope<'a> {
    #[serde(default, borrow)]
    id: OptionalRaw<'a>,
    #[serde(default, borrow)]
    jsonrpc: OptionalRaw<'a>,
    #[serde(default, borrow)]
    method: OptionalRaw<'a>,
    #[serde(default, borrow)]
    params: OptionalRaw<'a>,
}

#[derive(Deserialize)]
struct BorrowedCallParams<'a> {
    #[serde(default, borrow)]
    name: OptionalRaw<'a>,
    #[serde(default, borrow)]
    arguments: OptionalRaw<'a>,
}

fn parse_authorized_request(line: &[u8]) -> AuthorizedRequest {
    let raw: &RawValue = match serde_json::from_slice(line) {
        Ok(value) => value,
        Err(_) => {
            return AuthorizedRequest::Response(error_response(Value::Null, -32700, "parse error"));
        }
    };
    if !raw_is_object(raw) {
        return AuthorizedRequest::Response(error_response(Value::Null, -32600, "invalid request"));
    }
    let envelope: BorrowedEnvelope<'_> = match serde_json::from_str(raw.get()) {
        Ok(envelope) => envelope,
        Err(_) => {
            return AuthorizedRequest::Response(error_response(
                Value::Null,
                -32600,
                "invalid request",
            ));
        }
    };
    let Some(raw_id) = envelope.id.0 else {
        return AuthorizedRequest::Notification;
    };
    let id: Value = match serde_json::from_str(raw_id.get()) {
        Ok(id) => id,
        Err(_) => Value::Null,
    };
    if !matches!(&id, Value::String(_) | Value::Number(_) | Value::Null) {
        return AuthorizedRequest::Response(error_response(
            Value::Null,
            -32600,
            "invalid request: id 타입 위반",
        ));
    }
    if raw_string(envelope.jsonrpc.0).as_deref() != Some("2.0") {
        return AuthorizedRequest::Response(error_response(
            id,
            -32600,
            "invalid request: jsonrpc != \"2.0\"",
        ));
    }
    let Some(method) = raw_string(envelope.method.0) else {
        return AuthorizedRequest::Response(error_response(
            id,
            -32600,
            "invalid request: method 누락/비문자열",
        ));
    };
    match method.as_ref() {
        "initialize" => AuthorizedRequest::Initialize { id },
        "tools/list" => AuthorizedRequest::ListTools { id },
        "tools/call" => {
            let Some(params) = envelope.params.0.filter(|params| raw_is_object(params)) else {
                return AuthorizedRequest::Response(error_response(
                    id,
                    -32602,
                    "tools/call: name 없음",
                ));
            };
            let params: BorrowedCallParams<'_> = match serde_json::from_str(params.get()) {
                Ok(params) => params,
                Err(_) => {
                    return AuthorizedRequest::Response(error_response(
                        id,
                        -32602,
                        "tools/call: params가 object 아님",
                    ));
                }
            };
            let Some(tool_name) = raw_string(params.name.0).map(Cow::into_owned) else {
                return AuthorizedRequest::Response(error_response(
                    id,
                    -32602,
                    "tools/call: name 없음",
                ));
            };
            let arguments = match params.arguments.0 {
                None => &b"{}"[..],
                Some(value) if raw_is_object(value) => value.get().as_bytes(),
                Some(_) => {
                    return AuthorizedRequest::Response(error_response(
                        id,
                        -32602,
                        "tools/call: arguments가 object 아님",
                    ));
                }
            };
            match SensitiveToolInput::from_object_bytes(arguments) {
                Ok(input) => AuthorizedRequest::CallTool {
                    id,
                    tool_name,
                    input,
                },
                Err(_) => AuthorizedRequest::Response(error_response(
                    id,
                    -32602,
                    "tools/call: arguments 크기 초과",
                )),
            }
        }
        _ => AuthorizedRequest::Response(error_response(id, -32601, "method not found")),
    }
}

fn raw_is_object(raw: &RawValue) -> bool {
    raw.get().trim_start().starts_with('{')
}

fn raw_string(raw: Option<&RawValue>) -> Option<Cow<'_, str>> {
    serde_json::from_str(raw?.get()).ok()
}

/// stdio JSON-RPC MCP **서버** 루프.
///
/// `reader`에서 JSON-RPC 요청을 라인 단위로 읽어 처리하고 응답을 `writer`에 쓴다.
/// `reader`/`writer`는 Read/Write 제네릭이므로 in-memory pipe로 테스트할 수 있다
/// (std::io::stdin/stdout을 하드코딩하지 않는다).
///
/// 처리 메서드:
///   - `initialize` → 최소 서버 capabilities/serverInfo로 응답
///   - `notifications/initialized` → 응답 없음
///   - `tools/list` → `forwarder`에 위임
///   - `tools/call` → `hook`에 문의 후 허용이면 `forwarder`로 포워딩, 거부면 MCP 오류 result
///   - 그 외 method → JSON-RPC method-not-found(-32601)
///
/// reader가 EOF(또는 IO 종료)에 도달하면 Ok(())로 정상 종료한다.
#[cfg(test)]
fn run_proxy<R, W, F, H>(reader: R, mut writer: W, forwarder: F, hook: H) -> anyhow::Result<()>
where
    R: Read,
    W: Write,
    F: ToolForwarder,
    H: PermissionHook,
{
    let mut reader = BufReader::new(reader);
    loop {
        match read_line_capped(&mut reader, MAX_PROXY_REQUEST_BYTES) {
            // EOF/IO 종료 → 서버 루프 정상 종료 (에이전트가 연결을 닫음)
            LineRead::Eof | LineRead::Io => return Ok(()),
            LineRead::TooLong => {
                // 한 줄이 상한을 넘으면 transport.rs와 동일하게 스트림 신뢰 불가로 보고
                // 커넥션을 종료한다. read_line_capped가 over-cap에서 drain하지 않으므로
                // 남은 거대 라인 잔여를 다시 읽어 busy-loop에 빠지지 않도록 continue가 아니라
                // bail — 단일 거대/나쁜 라인이 서버를 영구 정지시키지 못하게 한다.
                anyhow::bail!(
                    "proxy: 요청 라인 상한({MAX_PROXY_REQUEST_BYTES} bytes) 초과 — 커넥션 종료"
                );
            }
            LineRead::Line(line) => {
                // 빈 라인(공백만)은 keep-alive/노이즈로 보고 무시
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                if let Some(response) = handle_line(&line, &forwarder, &hook) {
                    write_message(&mut writer, &response)?;
                }
            }
        }
    }
}

/// 한 줄(JSON-RPC 메시지)을 처리해 돌려줄 응답 Value를 만든다.
///
/// request/notification 구분 규칙(JSON-RPC 2.0): `id` 멤버가 있으면 request(반드시 응답),
/// 없으면 notification(무응답 — method가 무엇이든, envelope가 깨졌어도 응답하지 않는다).
/// request는 envelope(`jsonrpc=="2.0"`, id 타입, method)를 검증해 malformed가 dispatch까지
/// 도달하지 못하게 하고, 위반이면 -32600으로 응답해 클라이언트가 hang하지 않게 한다.
#[cfg(test)]
fn handle_line<F: ToolForwarder, H: PermissionHook>(
    line: &[u8],
    forwarder: &F,
    hook: &H,
) -> Option<Value> {
    // 깨진 JSON → JSON-RPC parse error (id null). panic 금지.
    let value: Value = match serde_json::from_slice(line) {
        Ok(value) => value,
        Err(_) => return Some(error_response(Value::Null, -32700, "parse error")),
    };
    let Some(obj) = value.as_object() else {
        return Some(error_response(Value::Null, -32600, "invalid request"));
    };

    // id 멤버 유무로 request/notification을 가른다 (null도 '있음' — request).
    // notification이면 어떤 위반이든 응답하지 않는다.
    if !obj.contains_key("id") {
        return None;
    }

    // 여기부터 request — 반드시 응답한다. id 타입은 string|number|null만 echo 허용;
    // object/bool/array id는 Invalid Request이고 안전히 echo할 수 없으므로 null로 응답.
    let id = obj.get("id").cloned().unwrap_or(Value::Null);
    if !matches!(&id, Value::String(_) | Value::Number(_) | Value::Null) {
        return Some(error_response(
            Value::Null,
            -32600,
            "invalid request: id 타입 위반",
        ));
    }

    // envelope: jsonrpc == "2.0"
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(error_response(
            id,
            -32600,
            "invalid request: jsonrpc != \"2.0\"",
        ));
    }

    // method는 반드시 존재하는 string이어야 한다 (없거나 비-string이면 -32600).
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        return Some(error_response(
            id,
            -32600,
            "invalid request: method 누락/비문자열",
        ));
    };
    let params = obj.get("params").cloned().unwrap_or_else(|| json!({}));

    Some(match method {
        "initialize" => initialize_response(id),
        "tools/list" => tools_list_response(id, forwarder),
        "tools/call" => tools_call_response(id, &params, forwarder, hook),
        _ => error_response(id, -32601, "method not found"),
    })
}

/// initialize 응답 — 최소 서버 capabilities/serverInfo.
fn initialize_response(id: Value) -> Value {
    success_response(
        id,
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "deppy-sijo-proxy",
                "version": env!("CARGO_PKG_VERSION"),
            },
        }),
    )
}

/// tools/list → forwarder 위임. 실패 시 JSON-RPC internal error.
#[cfg(test)]
fn tools_list_response<F: ToolForwarder>(id: Value, forwarder: &F) -> Value {
    match forwarder.list_tools() {
        Ok(result) => success_response(id, result),
        Err(error) => error_response(id, -32603, &format!("tools/list 실패: {error}")),
    }
}

/// tools/call → hook 판단 후 허용이면 forwarder로 포워딩, 거부면 MCP 오류 result.
#[cfg(test)]
fn tools_call_response<F: ToolForwarder, H: PermissionHook>(
    id: Value,
    params: &Value,
    forwarder: &F,
    hook: &H,
) -> Value {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return error_response(id, -32602, "tools/call: name 없음");
    };
    // MCP CallToolRequest의 arguments는 optional object다. 누락만 no-arg tool 호출로
    // 보고 빈 object로 정규화한다. 명시된 값은 object여야 하며 null/array/string은
    // authorization/audit hook 전에 -32602로 거부한다.
    let arguments = match params.get("arguments") {
        None => json!({}),
        Some(value) if value.is_object() => value.clone(),
        Some(_) => return error_response(id, -32602, "tools/call: arguments가 object 아님"),
    };

    match hook.check(name, &arguments) {
        // 거부: 포워딩하지 않고 isError=true tool result로 사유를 돌려준다.
        // 프로토콜 오류가 아니라 tool이 실패한 것처럼 표현 → 에이전트가 자연스럽게 처리.
        ProxyDecision::Deny(reason) => success_response(
            id,
            json!({
                "content": [{ "type": "text", "text": reason }],
                "isError": true,
            }),
        ),
        // 허용: 백엔드로 포워딩. forwarder 오류는 JSON-RPC error로 변환.
        ProxyDecision::Allow => match forwarder.call_tool(name, arguments) {
            Ok(result) => success_response(id, result),
            Err(error) => error_response(id, -32603, &format!("tools/call 실패: {error}")),
        },
    }
}

/// JSON-RPC 성공 응답.
fn success_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// JSON-RPC 오류 응답.
fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// 응답 한 줄(JSON + newline)을 쓰고 flush한다.
fn write_message<W: Write>(writer: &mut W, msg: &Value) -> anyhow::Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
enum LineRead {
    Line(Vec<u8>),
    Eof,
    TooLong,
    Io,
}

/// newline까지 한 줄을 읽되 누적 길이가 max를 넘으면 즉시 TooLong으로 중단한다.
/// (transport.rs의 read_line_capped와 동일: over-cap을 만나면 더 읽지/drain하지 않아
/// newline 없는 무한 스트림에서도 blocking하지 않는다. 커넥션 종료는 호출측 판단.)
#[cfg(test)]
fn read_line_capped(reader: &mut impl BufRead, max: usize) -> LineRead {
    let mut line = Vec::new();
    loop {
        let buf = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(_) => return LineRead::Io,
        };
        if buf.is_empty() {
            // EOF — newline 없이 끝난 마지막 라인도 메시지로 취급
            return if line.is_empty() {
                LineRead::Eof
            } else {
                LineRead::Line(line)
            };
        }
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            line.extend_from_slice(&buf[..pos]);
            reader.consume(pos + 1);
            return if line.len() > max {
                LineRead::TooLong
            } else {
                LineRead::Line(line)
            };
        }
        line.extend_from_slice(buf);
        let n = buf.len();
        reader.consume(n);
        if line.len() > max {
            return LineRead::TooLong;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// forwarder mock — list/call 결과를 심어두고 call 횟수를 센다.
    /// call_count는 Arc 공유 — forwarder를 run_proxy로 move한 뒤에도 검사할 수 있다.
    struct MockForwarder {
        list_result: Value,
        call_result: Value,
        call_count: Arc<AtomicUsize>,
    }

    impl MockForwarder {
        fn new() -> Self {
            Self {
                list_result: json!({"tools": [{"name": "echo", "inputSchema": {}}]}),
                call_result: json!({"content": [{"type": "text", "text": "ok"}], "isError": false}),
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl ToolForwarder for MockForwarder {
        fn list_tools(&self) -> anyhow::Result<Value> {
            Ok(self.list_result.clone())
        }
        fn call_tool(&self, _name: &str, _arguments: Value) -> anyhow::Result<Value> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(self.call_result.clone())
        }
    }

    struct AllowHook;
    impl PermissionHook for AllowHook {
        fn check(&self, _tool_name: &str, _arguments: &Value) -> ProxyDecision {
            ProxyDecision::Allow
        }
    }

    struct DenyHook(&'static str);
    impl PermissionHook for DenyHook {
        fn check(&self, _tool_name: &str, _arguments: &Value) -> ProxyDecision {
            ProxyDecision::Deny(self.0.to_owned())
        }
    }

    #[derive(Clone, Copy)]
    enum AuthorizedMode {
        Allow,
        Deny,
        Error,
        Panic,
    }

    struct AuthorizedFixture {
        mode: AuthorizedMode,
        calls: Arc<AtomicUsize>,
        drop_zeroized: Arc<AtomicBool>,
        input_len: Arc<AtomicUsize>,
    }

    impl AuthorizedFixture {
        fn new(mode: AuthorizedMode) -> Self {
            Self {
                mode,
                calls: Arc::new(AtomicUsize::new(0)),
                drop_zeroized: Arc::new(AtomicBool::new(false)),
                input_len: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl AuthorizedToolExecutor for AuthorizedFixture {
        fn list_tools(&self) -> anyhow::Result<Value> {
            Ok(json!({"tools": []}))
        }

        fn execute_tool(
            &self,
            _tool_name: String,
            mut input: SensitiveToolInput,
        ) -> AuthorizedToolOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.input_len
                .store(input.as_bytes().len(), Ordering::Release);
            input.observe_zeroized_drop(Arc::clone(&self.drop_zeroized));
            match self.mode {
                AuthorizedMode::Allow => AuthorizedToolOutcome::Success(
                    json!({"content":[{"type":"text","text":"ok"}],"isError":false}),
                ),
                AuthorizedMode::Deny => {
                    AuthorizedToolOutcome::Error(AuthorizedToolError::PermissionDenied)
                }
                AuthorizedMode::Error => {
                    AuthorizedToolOutcome::Error(AuthorizedToolError::DeliveryUnknown)
                }
                AuthorizedMode::Panic => panic!("injected executor panic"),
            }
        }
    }

    struct CountingHook(Arc<AtomicUsize>);
    impl PermissionHook for CountingHook {
        fn check(&self, _tool_name: &str, _arguments: &Value) -> ProxyDecision {
            self.0.fetch_add(1, Ordering::SeqCst);
            ProxyDecision::Allow
        }
    }

    /// writer 바이트를 라인별 JSON 응답 Vec으로 파싱한다.
    fn responses(output: &[u8]) -> Vec<Value> {
        output
            .split(|&b| b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).expect("응답이 JSON이어야 함"))
            .collect()
    }

    #[test]
    fn initialize_list_call_허용_왕복() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo","arguments":{"msg":"hi"}}}"#,
            "\n",
        );
        let forwarder = MockForwarder::new();
        let call_count = Arc::clone(&forwarder.call_count);
        let mut output = Vec::new();
        run_proxy(input.as_bytes(), &mut output, forwarder, AllowHook).unwrap();

        let responses = responses(&output);
        // initialized notification은 응답 없음 → 응답은 3개(id 1,2,3)
        assert_eq!(responses.len(), 3);

        // initialize → serverInfo 존재
        assert_eq!(responses[0].pointer("/id").and_then(Value::as_u64), Some(1));
        assert_eq!(
            responses[0]
                .pointer("/result/serverInfo/name")
                .and_then(Value::as_str),
            Some("deppy-sijo-proxy")
        );

        // tools/list → forwarder 결과 그대로
        assert_eq!(
            responses[1]
                .pointer("/result/tools/0/name")
                .and_then(Value::as_str),
            Some("echo")
        );

        // tools/call(allow) → 포워딩된 결과
        assert_eq!(
            responses[2]
                .pointer("/result/content/0/text")
                .and_then(Value::as_str),
            Some("ok")
        );
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn tools_call_거부는_is_error_result이고_포워딩_안함() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"rm","arguments":{}}}"#,
            "\n",
        );
        let forwarder = MockForwarder::new();
        let call_count = Arc::clone(&forwarder.call_count);
        let mut output = Vec::new();
        run_proxy(
            input.as_bytes(),
            &mut output,
            forwarder,
            DenyHook("정책 거부"),
        )
        .unwrap();

        let responses = responses(&output);
        assert_eq!(responses.len(), 1);
        // isError=true + 사유 텍스트
        assert_eq!(
            responses[0]
                .pointer("/result/isError")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            responses[0]
                .pointer("/result/content/0/text")
                .and_then(Value::as_str),
            Some("정책 거부")
        );
        // forwarder는 호출되지 않아야 한다
        assert_eq!(call_count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn unknown_method는_method_not_found() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":9,"method":"resources/list","params":{}}"#,
            "\n",
        );
        let forwarder = MockForwarder::new();
        let mut output = Vec::new();
        run_proxy(input.as_bytes(), &mut output, forwarder, AllowHook).unwrap();

        let responses = responses(&output);
        assert_eq!(responses.len(), 1);
        assert_eq!(
            responses[0].pointer("/error/code").and_then(Value::as_i64),
            Some(-32601)
        );
    }

    /// input 한 줄을 run_proxy에 흘려 응답 Vec을 돌려주는 헬퍼 (forwarder 미사용 케이스).
    fn run_line(input: &str) -> Vec<Value> {
        let mut output = Vec::new();
        run_proxy(
            input.as_bytes(),
            &mut output,
            MockForwarder::new(),
            AllowHook,
        )
        .unwrap();
        responses(&output)
    }

    #[test]
    fn method_누락_또는_비문자열인데_id_있으면_invalid_request() {
        // id는 있는데 method 누락 → -32600 (client hang 방지)
        let responses = run_line("{\"jsonrpc\":\"2.0\",\"id\":1}\n");
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].pointer("/id").and_then(Value::as_u64), Some(1));
        assert_eq!(
            responses[0].pointer("/error/code").and_then(Value::as_i64),
            Some(-32600)
        );

        // method가 string이 아님 → -32600
        let responses = run_line("{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":123}\n");
        assert_eq!(
            responses[0].pointer("/error/code").and_then(Value::as_i64),
            Some(-32600)
        );
    }

    #[test]
    fn 잘못된_id_타입은_invalid_request이고_null로_응답() {
        // object id → -32600, echo는 null
        let responses = run_line(r#"{"jsonrpc":"2.0","id":{},"method":"initialize"}"#);
        assert_eq!(responses.len(), 1);
        assert!(responses[0].pointer("/id").unwrap().is_null());
        assert_eq!(
            responses[0].pointer("/error/code").and_then(Value::as_i64),
            Some(-32600)
        );
    }

    #[test]
    fn jsonrpc_envelope_위반은_invalid_request() {
        // jsonrpc 필드 누락 → -32600 (dispatch 도달 전 차단)
        let responses =
            run_line(r#"{"id":5,"method":"tools/call","params":{"name":"x","arguments":{}}}"#);
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].pointer("/id").and_then(Value::as_u64), Some(5));
        assert_eq!(
            responses[0].pointer("/error/code").and_then(Value::as_i64),
            Some(-32600)
        );
    }

    #[test]
    fn id_없는_notification은_무응답이고_포워딩_안함() {
        // id 없는 tools/call = notification → 무응답, forwarder 미호출
        let forwarder = MockForwarder::new();
        let call_count = Arc::clone(&forwarder.call_count);
        let mut output = Vec::new();
        let input =
            r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"x","arguments":{}}}"#;
        run_proxy(input.as_bytes(), &mut output, forwarder, AllowHook).unwrap();
        assert!(output.is_empty());
        assert_eq!(call_count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn non_object_arguments는_invalid_params이고_포워딩_안함() {
        // arguments가 배열 → -32602, forwarder 미호출 (permission hook 우회 방지)
        let forwarder = MockForwarder::new();
        let call_count = Arc::clone(&forwarder.call_count);
        let mut output = Vec::new();
        let input = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"x","arguments":[1,2]}}"#;
        run_proxy(input.as_bytes(), &mut output, forwarder, AllowHook).unwrap();

        let responses = responses(&output);
        assert_eq!(responses.len(), 1);
        assert_eq!(
            responses[0].pointer("/error/code").and_then(Value::as_i64),
            Some(-32602)
        );
        assert_eq!(call_count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn arguments_누락과_빈_object는_no_arg_tool로_정상_포워딩() {
        // arguments는 optional — 누락 또는 빈 object만 no-arg 호출로 정규화된다.
        for params in [r#"{"name":"x"}"#, r#"{"name":"x","arguments":{}}"#] {
            let line =
                format!(r#"{{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{params}}}"#);
            let responses = run_line(&line);
            assert!(
                responses[0].pointer("/result").is_some(),
                "누락/빈 arguments가 거부됨: {params}"
            );
            assert!(responses[0].pointer("/error").is_none());
        }
    }

    #[test]
    fn explicit_null_arguments는_auth와_포워더_전에_거부() {
        let forwarder = MockForwarder::new();
        let call_count = Arc::clone(&forwarder.call_count);
        let hook_count = Arc::new(AtomicUsize::new(0));
        let mut output = Vec::new();
        let input = r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"x","arguments":null}}"#;

        run_proxy(
            input.as_bytes(),
            &mut output,
            forwarder,
            CountingHook(Arc::clone(&hook_count)),
        )
        .unwrap();

        let responses = responses(&output);
        assert_eq!(responses.len(), 1);
        assert_eq!(
            responses[0].pointer("/error/code").and_then(Value::as_i64),
            Some(-32602)
        );
        assert_eq!(hook_count.load(Ordering::SeqCst), 0);
        assert_eq!(call_count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn authorized_proxy_rejects_explicit_null_before_executor() {
        let executor = AuthorizedFixture::new(AuthorizedMode::Allow);
        let calls = Arc::clone(&executor.calls);
        let mut output = Vec::new();
        let input = r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"x","arguments":null}}"#;

        run_authorized_proxy(input.as_bytes(), &mut output, executor).unwrap();

        let responses = responses(&output);
        assert_eq!(
            responses[0].pointer("/error/code").and_then(Value::as_i64),
            Some(-32602)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn authorized_proxy_zeroizes_owned_input_for_allow_deny_and_error() {
        for mode in [
            AuthorizedMode::Allow,
            AuthorizedMode::Deny,
            AuthorizedMode::Error,
        ] {
            let executor = AuthorizedFixture::new(mode);
            let calls = Arc::clone(&executor.calls);
            let drop_zeroized = Arc::clone(&executor.drop_zeroized);
            let input_len = Arc::clone(&executor.input_len);
            let mut output = Vec::new();
            let input = r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"x","arguments":{"secret":"value"}}}"#;

            run_authorized_proxy(input.as_bytes(), &mut output, executor).unwrap();

            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                input_len.load(Ordering::Acquire),
                br#"{"secret":"value"}"#.len()
            );
            assert!(drop_zeroized.load(Ordering::Acquire));
            let responses = responses(&output);
            assert!(responses[0].pointer("/result").is_some());
            assert!(responses[0].pointer("/error").is_none());
        }
    }

    #[test]
    fn sensitive_tool_input_debug_and_json_value_cleanup_are_redacted() {
        let input = SensitiveToolInput::from_object_bytes(br#"{"secret":"value"}"#).unwrap();
        assert_eq!(format!("{input:?}"), "SensitiveToolInput(REDACTED)");
        drop(input);

        let mut value = json!({"secret":"value","nested":["token"]});
        zeroize_json_value(&mut value);
        assert!(value.is_null());
    }

    #[test]
    fn authorized_input_limit_is_inclusive_and_plus_one_is_callback_zero() {
        let empty_size = serde_json::to_vec(&json!({"payload":""})).unwrap().len();
        for (size, expected_calls) in [
            (MAX_TOOL_INPUT_BYTES, 1usize),
            (MAX_TOOL_INPUT_BYTES + 1, 0usize),
        ] {
            let payload = "x".repeat(size - empty_size);
            let arguments = json!({"payload": payload});
            assert_eq!(serde_json::to_vec(&arguments).unwrap().len(), size);
            let line = json!({
                "jsonrpc":"2.0",
                "id":9,
                "method":"tools/call",
                "params":{"name":"x", "arguments":arguments},
            });
            let input = serde_json::to_vec(&line).unwrap();
            let executor = AuthorizedFixture::new(AuthorizedMode::Allow);
            let calls = Arc::clone(&executor.calls);
            let mut output = Vec::new();
            run_authorized_proxy(input.as_slice(), &mut output, executor).unwrap();
            assert_eq!(calls.load(Ordering::Acquire), expected_calls);
        }
    }

    #[test]
    fn authorized_notification_is_callback_zero() {
        let executor = AuthorizedFixture::new(AuthorizedMode::Allow);
        let calls = Arc::clone(&executor.calls);
        let mut output = Vec::new();
        let input = br#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"x","arguments":{"secret":"value"}}}"#;
        run_authorized_proxy(input.as_slice(), &mut output, executor).unwrap();
        assert_eq!(calls.load(Ordering::Acquire), 0);
        assert!(output.is_empty());
    }

    #[test]
    fn authorized_input_zeroizes_during_executor_panic_unwind() {
        let executor = AuthorizedFixture::new(AuthorizedMode::Panic);
        let observer = Arc::clone(&executor.drop_zeroized);
        let input = br#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"x","arguments":{"secret":"value"}}}"#;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut output = Vec::new();
            let _ = run_authorized_proxy(input.as_slice(), &mut output, executor);
        }));
        assert!(result.is_err());
        assert!(observer.load(Ordering::Acquire));
    }

    #[test]
    fn sensitive_buffer_growth_wipes_old_allocation_before_replacement() {
        let wipes = Arc::new(AtomicUsize::new(0));
        let mut bytes = SensitiveBytes::with_capacity(1);
        bytes.observe_growth_wipes(Arc::clone(&wipes));
        std::io::Write::write_all(&mut bytes, &[b'x'; 4096]).unwrap();
        assert!(wipes.load(Ordering::Acquire) >= 1);
    }

    #[test]
    fn authorized_reader_wipes_shifted_tail_and_prechecks_limit() {
        let mut reader = ZeroizingLineReader::new(&b"first\nSECOND\n"[..]);
        assert!(matches!(
            reader.read_line_capped(64),
            SensitiveLineRead::Line(line) if line.as_slice() == b"first"
        ));
        let logical_len = reader.pending.len();
        let old_len = b"first\nSECOND\n".len();
        assert_eq!(&reader.pending, b"SECOND\n");
        // SAFETY: the shifted tail was initialized and explicitly overwritten with zero before
        // truncate; u8 has no drop glue. This test temporarily exposes only those initialized
        // bytes to prove no duplicate suffix survives beyond logical length.
        unsafe { reader.pending.set_len(old_len) };
        assert!(reader.pending[logical_len..].iter().all(|byte| *byte == 0));
        reader.pending.truncate(logical_len);

        let mut over_limit = ZeroizingLineReader::new(&b"123456789\n"[..]);
        assert!(matches!(
            over_limit.read_line_capped(8),
            SensitiveLineRead::TooLong
        ));
        assert!(over_limit.pending.is_empty());
    }

    #[test]
    fn 거대_라인은_loop를_멈추지_않고_종료() {
        // 64 KiB proxy request 상한을 넘는 newline 없는 라인 → 즉시 종료.
        let mut input = vec![b'a'; MAX_PROXY_REQUEST_BYTES + 16];
        input.push(b'\n');
        let mut output = Vec::new();
        let result = run_proxy(&input[..], &mut output, MockForwarder::new(), AllowHook);
        assert!(result.is_err(), "거대 라인은 커넥션을 종료해야 함");
    }

    #[test]
    fn authorized_proxy_request_cap_is_exact_and_plus_one_fails_closed() {
        fn request_of_len(length: usize) -> Vec<u8> {
            let prefix = br#"{"jsonrpc":"2.0","id":12,"method":"tools/list","padding":""#;
            let suffix = br#""}"#;
            assert!(length >= prefix.len() + suffix.len());
            let mut request = Vec::with_capacity(length);
            request.extend_from_slice(prefix);
            request.resize(length - suffix.len(), b'x');
            request.extend_from_slice(suffix);
            assert_eq!(request.len(), length);
            request
        }

        let mut output = Vec::new();
        run_authorized_proxy(
            request_of_len(MAX_PROXY_REQUEST_BYTES).as_slice(),
            &mut output,
            AuthorizedFixture::new(AuthorizedMode::Allow),
        )
        .unwrap();
        assert_eq!(responses(&output).len(), 1);

        let mut output = Vec::new();
        let result = run_authorized_proxy(
            request_of_len(MAX_PROXY_REQUEST_BYTES + 1).as_slice(),
            &mut output,
            AuthorizedFixture::new(AuthorizedMode::Allow),
        );
        assert!(result.is_err());
        assert!(output.is_empty());
    }
}
