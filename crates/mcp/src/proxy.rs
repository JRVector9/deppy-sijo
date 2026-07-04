//! Agent-proxy: deppy를 MCP **서버**로 세우는 재사용 프리미티브 (§1.5 후속).
//! 에이전트(예: 패널에서 도는 Claude Code)가 이 서버에 붙으면, 각 tool 호출은
//! 여기서 permission/audit 판단(hook)을 거친 뒤 실제 백엔드 MCP 서버로 포워딩된다.
//!
//! 이 파일은 순수 프리미티브만 제공한다 — 앱/에이전트 배선은 하지 않는다:
//!   - `run_proxy`: stdio JSON-RPC 서버 루프 (Read/Write 제네릭 → in-memory 테스트 가능)
//!   - `ToolForwarder`: tools/list·tools/call을 실제 백엔드로 위임하는 훅(호출측 제공)
//!   - `PermissionHook` + `ProxyDecision`: tools/call 허용/거부 판단(호출측 제공)
//!
//! transport.rs와 같은 방어적 스타일을 따른다 — 라인 길이 상한, 악의적/깨진
//! 입력에 대해 panic 없이 graceful 처리, 한 줄 나쁜 입력에 무한 블록 금지.

use std::io::{BufRead, BufReader, Read, Write};

use serde_json::{Value, json};

use crate::PROTOCOL_VERSION;

/// 요청 한 줄 최대 길이 — newline 없는 무한 스트림/거대 payload로 인한 메모리 폭주 방지
/// (transport.rs의 MAX_LINE_BYTES와 동일 규모).
const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;

/// tools/list·tools/call을 실제 백엔드 MCP 서버로 위임하는 훅.
/// proxy는 백엔드 구현을 알 필요가 없다 — 호출측이 LocalMcpManager 등으로 구현한다.
pub trait ToolForwarder {
    /// tools/list 결과 (MCP `{"tools":[...]}` 형태의 result Value).
    fn list_tools(&self) -> anyhow::Result<Value>;
    /// tools/call 결과 (MCP `{"content":[...],"isError":bool}` 형태의 result Value).
    fn call_tool(&self, name: &str, arguments: Value) -> anyhow::Result<Value>;
}

/// tools/call 한 건에 대한 permission/audit 판단.
/// proxy는 audit crate에 의존하지 않는다 — 호출측이 정책+기록으로 구현한다.
pub trait PermissionHook {
    fn check(&self, tool_name: &str, arguments: &Value) -> ProxyDecision;
}

/// PermissionHook의 판단 결과.
pub enum ProxyDecision {
    /// 백엔드로 포워딩 허용
    Allow,
    /// 거부 — reason은 에이전트에게 돌려줄 사유 텍스트
    Deny(String),
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
pub fn run_proxy<R, W, F, H>(reader: R, mut writer: W, forwarder: F, hook: H) -> anyhow::Result<()>
where
    R: Read,
    W: Write,
    F: ToolForwarder,
    H: PermissionHook,
{
    let mut reader = BufReader::new(reader);
    loop {
        match read_line_capped(&mut reader, MAX_REQUEST_BYTES) {
            // EOF/IO 종료 → 서버 루프 정상 종료 (에이전트가 연결을 닫음)
            LineRead::Eof | LineRead::Io => return Ok(()),
            LineRead::TooLong => {
                // 한 줄이 상한을 넘으면 transport.rs와 동일하게 스트림 신뢰 불가로 보고
                // 커넥션을 종료한다. read_line_capped가 over-cap에서 drain하지 않으므로
                // 남은 거대 라인 잔여를 다시 읽어 busy-loop에 빠지지 않도록 continue가 아니라
                // bail — 단일 거대/나쁜 라인이 서버를 영구 정지시키지 못하게 한다.
                anyhow::bail!(
                    "proxy: 요청 라인 상한({MAX_REQUEST_BYTES} bytes) 초과 — 커넥션 종료"
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
fn tools_list_response<F: ToolForwarder>(id: Value, forwarder: &F) -> Value {
    match forwarder.list_tools() {
        Ok(result) => success_response(id, result),
        Err(error) => error_response(id, -32603, &format!("tools/list 실패: {error}")),
    }
}

/// tools/call → hook 판단 후 허용이면 forwarder로 포워딩, 거부면 MCP 오류 result.
fn tools_call_response<F: ToolForwarder, H: PermissionHook>(
    id: Value,
    params: &Value,
    forwarder: &F,
    hook: &H,
) -> Value {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return error_response(id, -32602, "tools/call: name 없음");
    };
    // MCP CallToolRequest의 arguments는 optional object다. 누락/null은 no-arg tool 호출로
    // 보고 빈 object로 정규화한다. 값이 있으면서 object가 아닌 경우(array/string 등)만,
    // field-based permission hook 우회를 막기 위해 -32602로 거부한다.
    let arguments = match params.get("arguments") {
        None | Some(Value::Null) => json!({}),
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

enum LineRead {
    Line(Vec<u8>),
    Eof,
    TooLong,
    Io,
}

/// newline까지 한 줄을 읽되 누적 길이가 max를 넘으면 즉시 TooLong으로 중단한다.
/// (transport.rs의 read_line_capped와 동일: over-cap을 만나면 더 읽지/drain하지 않아
/// newline 없는 무한 스트림에서도 blocking하지 않는다. 커넥션 종료는 호출측 판단.)
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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
    fn arguments_누락은_no_arg_tool로_정상_포워딩() {
        // MCP arguments는 optional — 누락/null이면 빈 object로 정규화되어 포워딩돼야 한다.
        for params in [
            r#"{"name":"x"}"#,
            r#"{"name":"x","arguments":null}"#,
            r#"{"name":"x","arguments":{}}"#,
        ] {
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
    fn 거대_라인은_loop를_멈추지_않고_종료() {
        // 8MiB 상한을 넘는 newline 없는 라인 → run loop가 매달리지 않고 Err로 종료
        let mut input = vec![b'a'; MAX_REQUEST_BYTES + 16];
        input.push(b'\n');
        let mut output = Vec::new();
        let result = run_proxy(&input[..], &mut output, MockForwarder::new(), AllowHook);
        assert!(result.is_err(), "거대 라인은 커넥션을 종료해야 함");
    }
}
