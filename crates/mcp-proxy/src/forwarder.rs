//! 실제 백엔드 MCP 서버로 tools/list·tools/call을 위임하는 ToolForwarder 구현.
//!
//! 연결 수명: **호출당 connect**. LocalMcpManager의 discover_tools/call_tool은
//! 이미 매 호출마다 백엔드 subprocess를 spawn→initialize→요청→종료(kill/reap)한다
//! (manager.rs "stdio 서버는 매 호출마다 새 subprocess" MVP 단순화). 여기서도 그 관행을
//! 그대로 따른다 — 영속 연결 상태를 들고 다니지 않아 단순하고, 백엔드 프로세스 누수가 없다.
//! (프록시 자체가 짧게 사는 per-agent 프로세스라 재spawn 비용은 MVP에서 허용된다.)
//! HTTP 백엔드(H3)도 같은 관례 — 호출마다 connect(initialize+세션)→요청→drop(DELETE).

use anyhow::Context;
use mcp::{LocalMcpManager, McpHttpServerConfig, McpServerConfig, McpTool, ToolForwarder};
use serde_json::{Value, json};

/// kind별 백엔드 config (H3) — mcp_servers.kind('stdio'|'http')에 대응한다.
/// H2가 stdio/http config를 분리 타입으로 만들어 여기서 enum으로 감싼다.
#[derive(Clone, Debug)]
pub enum BackendConfig {
    Stdio(McpServerConfig),
    Http(McpHttpServerConfig),
}

impl BackendConfig {
    pub fn name(&self) -> &str {
        match self {
            Self::Stdio(config) => &config.name,
            Self::Http(config) => &config.name,
        }
    }

    /// connect → tools/list — transport별 manager 경로로 위임.
    pub fn discover_tools(&self, manager: &LocalMcpManager) -> anyhow::Result<Vec<McpTool>> {
        match self {
            Self::Stdio(config) => manager.discover_tools(config),
            Self::Http(config) => manager.discover_tools_http(config),
        }
    }

    /// connect → tools/call — transport별 manager 경로로 위임.
    pub fn call_tool(
        &self,
        manager: &LocalMcpManager,
        name: &str,
        arguments: Value,
    ) -> anyhow::Result<Value> {
        match self {
            Self::Stdio(config) => manager.call_tool(config, name, arguments),
            Self::Http(config) => manager.call_tool_http(config, name, arguments),
        }
    }
}

/// 백엔드 서버 spec + manager를 소유하고 매 호출마다 새로 연결해 포워딩한다.
pub struct ManagerToolForwarder {
    manager: LocalMcpManager,
    config: BackendConfig,
}

impl ManagerToolForwarder {
    pub fn new(manager: LocalMcpManager, config: BackendConfig) -> Self {
        Self { manager, config }
    }
}

impl ToolForwarder for ManagerToolForwarder {
    /// tools/list → 백엔드에서 발견한 tool을 MCP `{"tools":[...]}` result로 재구성한다.
    fn list_tools(&self) -> anyhow::Result<Value> {
        let discovered = self
            .config
            .discover_tools(&self.manager)
            .with_context(|| format!("백엔드 '{}' tools/list 실패", self.config.name()))?;
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
        self.config
            .call_tool(&self.manager, name, arguments)
            .with_context(|| format!("백엔드 '{}' tools/call({name}) 실패", self.config.name()))
    }
}

// http 백엔드 중계 테스트 — H2의 목 서버 관례(std TcpListener, 포트 0)를 축약 이식.
#[cfg(test)]
mod tests {
    use super::*;
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
