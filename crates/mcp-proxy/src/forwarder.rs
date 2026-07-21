//! 실제 백엔드 MCP 서버로 tools/list·tools/call을 위임하는 ToolForwarder 구현.
//!
//! Hook의 live schema discovery와 forwarding이 하나의 lazy BackendSession을 공유한다.
//! warm request는 initialize handshake를 반복하지 않으며 idle TTL/revision/poison 때만 연결을
//! 폐기한다.

use anyhow::Context;
#[cfg(test)]
use mcp::LocalMcpManager;
use mcp::ToolForwarder;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::session::BackendClient;
pub use crate::session::BackendConfig;

/// Permission hook과 같은 BackendClient를 공유하는 forwarder.
pub struct ManagerToolForwarder {
    backend: Arc<BackendClient>,
}

impl ManagerToolForwarder {
    /// 독립 사용/테스트용 constructor. Production main은 `from_backend`로 hook과 공유한다.
    #[cfg(test)]
    pub fn new(manager: LocalMcpManager, config: BackendConfig) -> Self {
        Self {
            backend: BackendClient::production(config.name().to_owned(), config, manager),
        }
    }

    pub fn from_backend(backend: Arc<BackendClient>) -> Self {
        Self { backend }
    }
}

impl ToolForwarder for ManagerToolForwarder {
    /// tools/list → 백엔드에서 발견한 tool을 MCP `{"tools":[...]}` result로 재구성한다.
    fn list_tools(&self) -> anyhow::Result<Value> {
        let discovered = self
            .backend
            .list_tools()
            .context("백엔드 tools/list 실패")?;
        let tools: Vec<Value> = discovered
            .into_iter()
            .map(|tool| {
                // input_schema_json은 원본 JSON 문자열 — object Value로 되돌린다.
                // 파싱 불가(비정상)면 빈 object로 대체해 목록 전체를 죽이지 않는다.
                let schema: Value =
                    serde_json::from_str(&tool.input_schema_json).unwrap_or_else(|_| json!({}));
                let mut obj = json!({ "name": tool.name, "inputSchema": schema });
                if let Some(description) = tool.description {
                    obj["description"] = Value::String(description);
                }
                obj
            })
            .collect();
        Ok(json!({ "tools": tools }))
    }

    /// tools/call → 백엔드로 그대로 위임. 결과 Value({content, isError})를 그대로 돌려준다.
    fn call_tool(&self, name: &str, arguments: Value) -> anyhow::Result<Value> {
        self.backend
            .call_tool(name, arguments)
            .with_context(|| format!("백엔드 tools/call({name}) 실패"))
    }
}

// http 백엔드 중계 테스트 — H2의 목 서버 관례(std TcpListener, 포트 0)를 축약 이식.
#[cfg(test)]
mod tests {
    use super::*;
    use mcp::{McpHttpServerConfig, McpServerConfig};
    use secret::RedactionService;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    /// JSON-RPC method별로 응답하는 목 Streamable HTTP MCP 서버.
    /// 커넥션당 요청 1개(Connection: close) — connect-per-call이라 순서 무관 분기.
    fn spawn_http_backend() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // 헤더 끝까지 읽기
                let header_end = loop {
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos;
                    }
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break usize::MAX,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                };
                if header_end == usize::MAX {
                    continue;
                }
                let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
                let content_length: usize = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().ok())?
                    })
                    .unwrap_or(0);
                let mut body = buf[header_end + 4..].to_vec();
                while body.len() < content_length {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => body.extend_from_slice(&chunk[..n]),
                    }
                }
                let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let id = request.get("id").cloned().unwrap_or(Value::Null);
                let is_delete = head.starts_with("DELETE");
                let reply = match request.get("method").and_then(Value::as_str) {
                    Some("initialize") => json_response(
                        &id,
                        json!({
                            "protocolVersion": "2025-11-25",
                            "capabilities": {},
                            "serverInfo": {"name": "mock-http", "version": "0"},
                        }),
                    ),
                    Some("notifications/initialized") => {
                        b"HTTP/1.1 202 OK\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
                            .to_vec()
                    }
                    Some("tools/list") => json_response(
                        &id,
                        json!({"tools": [{
                            "name": "echo_tool",
                            "description": "에코",
                            "inputSchema": {"type": "object"},
                        }]}),
                    ),
                    Some("tools/call") => {
                        let msg = request
                            .pointer("/params/arguments/msg")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        json_response(
                            &id,
                            json!({"content": [{"type": "text", "text": msg}], "isError": false}),
                        )
                    }
                    // drop 시 세션 DELETE (바디 없음)
                    _ if is_delete => {
                        b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
                            .to_vec()
                    }
                    _ => b"HTTP/1.1 404 OK\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
                        .to_vec(),
                };
                let _ = stream.write_all(&reply);
            }
        });
        format!("http://{addr}/mcp")
    }

    fn json_response(id: &Value, result: Value) -> Vec<u8> {
        let body = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
        format!(
            "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn http_forwarder(url: String) -> ManagerToolForwarder {
        let manager = LocalMcpManager::new(RedactionService::new())
            .with_request_timeout(Duration::from_secs(5));
        ManagerToolForwarder::new(
            manager,
            BackendConfig::Http(McpHttpServerConfig {
                name: "mock-http".to_owned(),
                url,
                bearer: None,
            }),
        )
    }

    #[test]
    fn http_백엔드로_tools_list_중계() {
        let forwarder = http_forwarder(spawn_http_backend());
        let result = forwarder.list_tools().unwrap();
        assert_eq!(
            result.pointer("/tools/0/name").and_then(Value::as_str),
            Some("echo_tool")
        );
        assert_eq!(
            result
                .pointer("/tools/0/inputSchema/type")
                .and_then(Value::as_str),
            Some("object")
        );
        assert_eq!(
            result
                .pointer("/tools/0/description")
                .and_then(Value::as_str),
            Some("에코")
        );
    }

    #[test]
    fn http_백엔드로_tools_call_중계() {
        let forwarder = http_forwarder(spawn_http_backend());
        let result = forwarder
            .call_tool("echo_tool", json!({"msg": "hello http"}))
            .unwrap();
        assert_eq!(
            result.pointer("/content/0/text").and_then(Value::as_str),
            Some("hello http")
        );
        assert_eq!(result.get("isError").and_then(Value::as_bool), Some(false));
    }

    // stdio 경로 회귀 — enum 도입 후에도 기존 stdio 중계가 그대로 동작한다.
    #[cfg(unix)]
    #[test]
    fn stdio_백엔드로_tools_list_중계() {
        let script = r#"
read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"mock","version":"0"}}}'
read -r _initialized
read -r _list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"stdio_tool","inputSchema":{"type":"object"}}]}}'
"#;
        let manager = LocalMcpManager::new(RedactionService::new())
            .with_request_timeout(Duration::from_secs(5));
        let forwarder = ManagerToolForwarder::new(
            manager,
            BackendConfig::Stdio(McpServerConfig::stdio(
                "mock".to_owned(),
                "/bin/sh".to_owned(),
                vec!["-c".to_owned(), script.to_owned()],
                Vec::new(),
                true,
            )),
        );
        let result = forwarder.list_tools().unwrap();
        assert_eq!(
            result.pointer("/tools/0/name").and_then(Value::as_str),
            Some("stdio_tool")
        );
    }
}
