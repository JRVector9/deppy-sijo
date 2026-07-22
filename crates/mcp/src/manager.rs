//! Local MCP Manager (설계문서 §1.5 v0 / PR-15).
//! spawn → initialize 핸드셰이크 → initialized notification → tools/list.
//! transport는 local stdio + Streamable HTTP(H2, crates/mcp/src/http.rs) —
//! OAuth 사다리는 H4/H5.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Context;
use secret::RedactionService;
use serde_json::{Value, json};

use crate::http::{HttpClient, McpHttpServerConfig};
use crate::limits::{
    MAX_TOOL_DESCRIPTOR_BYTES, MAX_TOOLS_PER_SERVER, McpPayloadKind, enforce_json_payload,
    serialized_json_len,
};
use crate::transport::{StdioCancellationHandle, StdioClient};

/// initialize 요청에 싣는 기준 스펙 개정판 (§1.5 — 최신 우선).
/// VS Code도 요청에는 최신 하나만 보낸다(버전별 분기 없음) — 서버가 자기 버전으로
/// 응답하는 것이 스펙 협상 규칙이다.
pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// 수용 가능한 서버 응답 protocolVersion 목록 (최신 우선, H1). 서버가 이 중 하나로
/// 응답하면 협상 성공 — deppy는 tools/list·tools/call만 쓰므로 개정판 간 실질 차이
/// 없다. 목록 밖 응답만 거부한다(엄격 gate 유지 — VS Code의 "무검증 수용"은 미채택).
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// tools/list cursor 페이지네이션 상한 — item/byte/cursor cycle과 별도의 왕복 상한.
const MAX_TOOL_PAGES: usize = 100;

/// local stdio MCP 서버 실행 스펙 (§11.4 kind='stdio' 행에 대응).
/// scoped env secret은 spawn 직전 호출측이 해석해 env에 넣는다. Debug는 env 값을 출력하지 않는다.
#[derive(PartialEq)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub inherit_env: bool,
}

/// Non-Clone production stdio connect target. Secret environment values are zeroizing owners and
/// are moved into this type exactly once; they are never represented by the legacy string-valued
/// `McpServerConfig`.
pub struct McpStdioConnectConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub plain_env: Vec<(String, String)>,
    pub secret_env: Vec<(String, secret::SecretString)>,
    pub inherit_env: bool,
}

impl std::fmt::Debug for McpStdioConnectConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpStdioConnectConfig")
            .field("name", &self.name)
            .field("command", &"REDACTED")
            .field("args", &format_args!("REDACTED({})", self.args.len()))
            .field("plain_env_count", &self.plain_env.len())
            .field("secret_env_count", &self.secret_env.len())
            .field("inherit_env", &self.inherit_env)
            .finish()
    }
}

impl McpServerConfig {
    /// stdio 서버 config 생성자 — struct literal 대신 쓸 수 있는 헬퍼 (H2).
    /// 이후 필드가 추가돼도 이 경로의 호출측은 깨지지 않는다.
    pub fn stdio(
        name: String,
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        inherit_env: bool,
    ) -> Self {
        Self {
            name,
            command,
            args,
            env,
            inherit_env,
        }
    }
}

impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServerConfig")
            .field("name", &self.name)
            .field("command", &"REDACTED")
            .field("args", &format_args!("REDACTED({})", self.args.len()))
            .field("env_count", &self.env.len())
            .field("inherit_env", &self.inherit_env)
            .finish()
    }
}

/// tools/list가 돌려준 tool 하나 (§11.5 mcp_tools 행에 대응).
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    pub name: String,
    pub description: Option<String>,
    /// inputSchema 원본 JSON (서버가 생략하면 "{}")
    pub input_schema_json: String,
}

/// spawn → initialize → tools/list 흐름을 묶은 상위 API.
pub struct LocalMcpManager {
    redaction: RedactionService,
    request_timeout: Duration,
}

impl LocalMcpManager {
    pub fn new(redaction: RedactionService) -> Self {
        Self {
            redaction,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }

    /// 요청별 응답 대기 상한을 조정한다 (기본 30초).
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// 서버 spawn → initialize 핸드셰이크 → initialized notification.
    /// 반환된 연결이 drop되면 서버 프로세스는 kill + reap된다.
    pub fn connect(&self, config: &McpServerConfig) -> anyhow::Result<McpConnection> {
        self.connect_stdio_parts(
            McpStdioConnectConfig {
                name: config.name.clone(),
                command: config.command.clone(),
                args: config.args.clone(),
                plain_env: config.env.clone(),
                secret_env: Vec::new(),
                inherit_env: config.inherit_env,
            },
            None,
        )
    }

    pub fn connect_scoped(&self, config: McpStdioConnectConfig) -> anyhow::Result<McpConnection> {
        self.connect_stdio_parts(config, None)
    }

    pub fn connect_scoped_cancellable(
        &self,
        config: McpStdioConnectConfig,
        cancellation_ready: impl FnOnce(McpCancellationHandle),
    ) -> anyhow::Result<McpConnection> {
        self.connect_stdio_parts(config, Some(Box::new(cancellation_ready)))
    }

    fn connect_stdio_parts(
        &self,
        config: McpStdioConnectConfig,
        cancellation_ready: Option<Box<dyn FnOnce(McpCancellationHandle) + '_>>,
    ) -> anyhow::Result<McpConnection> {
        let mut client = StdioClient::spawn_with_secret_env(
            &config.command,
            &config.args,
            &config.plain_env,
            &config.secret_env,
            config.inherit_env,
            &self.redaction,
            self.request_timeout,
        )
        .with_context(|| format!("MCP 서버 '{}' spawn 실패", config.name))?;
        let cancellation = McpCancellationHandle {
            inner: CancellationHandleKind::Stdio(client.cancellation_handle()),
        };
        if let Some(cancellation_ready) = cancellation_ready {
            cancellation_ready(cancellation.clone());
        }

        let initialize_result = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "deppy-sijo",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            )
            .with_context(|| format!("MCP 서버 '{}' initialize 실패", config.name))?;

        let negotiated = negotiate_protocol_version(&config.name, &initialize_result)?;

        client
            .notify("notifications/initialized", json!({}))
            .with_context(|| format!("MCP 서버 '{}' initialized notification 실패", config.name))?;

        Ok(McpConnection {
            client: TransportClient::Stdio(client),
            cancellation,
            initialize_result,
            negotiated_version: negotiated,
        })
    }

    /// Streamable HTTP 서버에 연결한다 (H2): URL 검증 → initialize(세션 캡처)
    /// → 버전 협상 → initialized notification. 반환된 연결이 drop되면 세션
    /// DELETE가 베스트에포트로 나간다.
    pub fn connect_http(&self, config: &McpHttpServerConfig) -> anyhow::Result<McpConnection> {
        let (client, initialize_result, negotiated) =
            HttpClient::connect(config, self.request_timeout)
                .with_context(|| format!("MCP 서버 '{}' HTTP 연결 실패", config.name))?;
        Ok(Self::http_connection(client, initialize_result, negotiated))
    }

    pub fn connect_http_owned(&self, config: McpHttpServerConfig) -> anyhow::Result<McpConnection> {
        let name = config.name.clone();
        let (client, initialize_result, negotiated) =
            HttpClient::connect_owned(config, self.request_timeout)
                .with_context(|| format!("MCP 서버 '{name}' HTTP 연결 실패"))?;
        Ok(Self::http_connection(client, initialize_result, negotiated))
    }

    fn http_connection(
        client: HttpClient,
        initialize_result: Value,
        negotiated: String,
    ) -> McpConnection {
        McpConnection {
            client: TransportClient::Http(client),
            cancellation: McpCancellationHandle {
                inner: CancellationHandleKind::Http,
            },
            initialize_result,
            negotiated_version: negotiated,
        }
    }

    /// connect → tools/list → 연결 종료(kill/reap)까지 한 번에.
    pub fn discover_tools(&self, config: &McpServerConfig) -> anyhow::Result<Vec<McpTool>> {
        let mut connection = self.connect(config)?;
        connection.list_tools()
        // connection drop → 서버 프로세스 정리
    }

    /// connect → tools/call → 연결 종료(kill/reap)까지 한 번에. `arguments`는 JSON object.
    /// stdio 서버는 매 호출마다 새 subprocess를 띄운다 — MVP 단순화(영속 연결 아님).
    pub fn call_tool(
        &self,
        config: &McpServerConfig,
        name: &str,
        arguments: Value,
    ) -> anyhow::Result<Value> {
        validate_tool_arguments(&arguments)?;
        let mut connection = self.connect(config)?;
        connection.call_tool(name, arguments)
        // connection drop → 서버 프로세스 정리
    }

    /// HTTP connect → tools/list → 연결 종료(세션 DELETE)까지 한 번에 (H2).
    pub fn discover_tools_http(
        &self,
        config: &McpHttpServerConfig,
    ) -> anyhow::Result<Vec<McpTool>> {
        let mut connection = self.connect_http(config)?;
        connection.list_tools()
        // connection drop → 세션 DELETE (베스트에포트)
    }

    /// HTTP connect → tools/call → 연결 종료(세션 DELETE)까지 한 번에 (H2).
    /// connect-per-call 관례는 stdio와 동일 — 호출마다 새 세션을 수립한다.
    pub fn call_tool_http(
        &self,
        config: &McpHttpServerConfig,
        name: &str,
        arguments: Value,
    ) -> anyhow::Result<Value> {
        validate_tool_arguments(&arguments)?;
        let mut connection = self.connect_http(config)?;
        connection.call_tool(name, arguments)
        // connection drop → 세션 DELETE (베스트에포트)
    }
}

/// initialize 응답의 protocolVersion을 지원 목록과 협상한다 (H1).
/// 목록 밖만 거부 — 실서버 상당수가 아직 구 개정판으로 응답하므로 정확 일치
/// gate는 과도했다. stdio(connect)와 HTTP(handshake·세션 재수립)가 공용으로 쓴다.
pub(crate) fn negotiate_protocol_version(
    server_name: &str,
    initialize_result: &Value,
) -> anyhow::Result<String> {
    let server_version = initialize_result
        .get("protocolVersion")
        .and_then(Value::as_str);
    server_version
        .filter(|version| SUPPORTED_PROTOCOL_VERSIONS.contains(version))
        .map(str::to_owned)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "MCP 서버 '{server_name}' protocolVersion 미지원: {server_version:?} (지원: {})",
                SUPPORTED_PROTOCOL_VERSIONS.join(", ")
            )
        })
}

/// transport별 클라이언트 — McpConnection이 요청을 위임한다 (H2).
#[derive(Debug)]
enum TransportClient {
    /// stdio subprocess. drop 시 kill + reap.
    Stdio(StdioClient),
    /// Streamable HTTP. drop 시 세션 DELETE (베스트에포트).
    Http(HttpClient),
}

impl TransportClient {
    fn request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        match self {
            Self::Stdio(client) => client.request(method, params),
            Self::Http(client) => client.request(method, params),
        }
    }

    fn cancel(&mut self) {
        match self {
            Self::Stdio(client) => client.cancel(),
            // sync ureq는 실행 중 요청을 강제 중단할 수 없다. 호출측 deadline이
            // 반환된 뒤에도 sender가 permit을 소유하며 bounded reaper가 회수한다.
            Self::Http(_) => {}
        }
    }
}

#[derive(Clone, Debug)]
enum CancellationHandleKind {
    Stdio(StdioCancellationHandle),
    Http,
}

/// Cloneable cancellation capability for an active MCP request. It deliberately owns no config,
/// bearer, response, or tool payload. Stdio cancellation can therefore kill/reap a hung process
/// while the connection itself is mutably borrowed by another thread. Sync HTTP remains governed
/// by its transport deadline and bounded sender permits.
#[derive(Clone, Debug)]
pub struct McpCancellationHandle {
    inner: CancellationHandleKind,
}

impl McpCancellationHandle {
    pub fn cancel(&self) {
        match &self.inner {
            CancellationHandleKind::Stdio(handle) => {
                handle.cancel();
            }
            CancellationHandleKind::Http => {}
        }
    }
}

/// initialize를 마친 MCP 연결 (stdio 또는 Streamable HTTP).
/// drop 시 stdio는 서버 프로세스를 kill + reap하고, HTTP는 세션을 DELETE한다.
#[derive(Debug)]
pub struct McpConnection {
    client: TransportClient,
    cancellation: McpCancellationHandle,
    /// initialize 응답 원본 (protocolVersion / capabilities / serverInfo)
    pub initialize_result: Value,
    /// 협상된 프로토콜 버전 (H1) — HTTP transport(H2)가 이후 요청의
    /// `MCP-Protocol-Version` 헤더 값으로 쓴다. stdio는 헤더가 없어 미사용.
    /// HTTP 세션 재수립(400/404 재시도) 시 내부적으로 재협상될 수 있다 —
    /// 이 필드는 최초 connect 시점의 값이다.
    pub negotiated_version: String,
}

impl McpConnection {
    pub fn cancellation_handle(&self) -> McpCancellationHandle {
        self.cancellation.clone()
    }

    /// tools/list 요청 → tool 목록 (cursor 페이지네이션 포함).
    pub fn list_tools(&mut self) -> anyhow::Result<Vec<McpTool>> {
        let mut discovery = ToolDiscovery::production();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_TOOL_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({"cursor": cursor}),
                None => json!({}),
            };
            let result = self.client.request("tools/list", params)?;
            match discovery.push_page(&result)? {
                Some(next) => cursor = Some(next),
                None => return Ok(discovery.finish()),
            }
        }
        anyhow::bail!("tools/list 페이지가 {MAX_TOOL_PAGES}를 초과 — cursor 순환 의심");
    }

    /// tools/call 요청 → 결과 Value (content 배열 + 선택적 isError).
    /// `arguments`는 JSON object여야 한다 (MCP 스펙). isError=true는 프로토콜
    /// 오류가 아니라 tool이 보고한 실패이므로 결과를 그대로 돌려준다 — 판단은 호출측.
    pub fn call_tool(&mut self, name: &str, arguments: Value) -> anyhow::Result<Value> {
        validate_tool_arguments(&arguments)?;
        self.client
            .request("tools/call", json!({"name": name, "arguments": arguments}))
    }

    /// Service-facing byte API keeps serde and MCP wire values inside this crate. Input must be a
    /// bounded JSON object; the returned string is capped by the transport response ceiling.
    pub fn call_tool_json(&mut self, name: &str, arguments_json: &[u8]) -> anyhow::Result<String> {
        crate::limits::enforce_payload_bytes(McpPayloadKind::ToolInput, arguments_json.len())?;
        let arguments: Value = serde_json::from_slice(arguments_json)
            .context("tools/call arguments JSON parsing failed")?;
        let result = self.call_tool(name, arguments)?;
        let output = serde_json::to_string(&result)?;
        crate::limits::enforce_payload_bytes(McpPayloadKind::RawResponse, output.len())?;
        Ok(output)
    }

    /// Cancel and release transport-owned resources. Stdio cancellation closes stdin,
    /// kills the entire process group, reaps the direct child, and joins pipe threads.
    /// Sync ureq cannot be force-cancelled; its deadline returns to the caller while the
    /// bounded HTTP sender retains its permit until the socket operation exits.
    pub fn cancel(&mut self) {
        self.cancellation.cancel();
        self.client.cancel();
    }

    /// 지금까지 캡처된 redacted stderr 로그 (stdio 전용 — HTTP는 stderr가
    /// 없으므로 빈 문자열).
    pub fn stderr_log(&self) -> String {
        match &self.client {
            TransportClient::Stdio(client) => client.stderr_log(),
            TransportClient::Http(_) => String::new(),
        }
    }
}

fn validate_tool_arguments(arguments: &Value) -> anyhow::Result<()> {
    if !arguments.is_object() {
        anyhow::bail!("tools/call arguments가 JSON object가 아님");
    }
    enforce_json_payload(McpPayloadKind::ToolInput, arguments)?;
    Ok(())
}

struct ToolDiscovery {
    tools: Vec<McpTool>,
    descriptor_bytes: usize,
    seen_cursors: HashSet<String>,
    max_tools: usize,
    max_descriptor_bytes: usize,
}

impl ToolDiscovery {
    fn production() -> Self {
        Self::with_limits(MAX_TOOLS_PER_SERVER, MAX_TOOL_DESCRIPTOR_BYTES)
    }

    fn with_limits(max_tools: usize, max_descriptor_bytes: usize) -> Self {
        Self {
            tools: Vec::new(),
            descriptor_bytes: 0,
            seen_cursors: HashSet::new(),
            max_tools,
            max_descriptor_bytes,
        }
    }

    fn push_page(&mut self, result: &Value) -> anyhow::Result<Option<String>> {
        let list = result
            .get("tools")
            .and_then(Value::as_array)
            .context("tools/list 응답에 tools 배열 없음")?;
        let new_len = self
            .tools
            .len()
            .checked_add(list.len())
            .context("tools/list tool 수 overflow")?;
        if new_len > self.max_tools {
            anyhow::bail!(
                "tools/list 누적 tool 수가 상한({})을 초과: {new_len}",
                self.max_tools
            );
        }
        for item in list {
            let item_bytes = serialized_json_len(item)?;
            self.descriptor_bytes = self
                .descriptor_bytes
                .checked_add(item_bytes)
                .context("tools/list descriptor byte 수 overflow")?;
            if self.descriptor_bytes > self.max_descriptor_bytes {
                anyhow::bail!(
                    "tools/list 누적 descriptor가 상한({} bytes)을 초과: {} bytes",
                    self.max_descriptor_bytes,
                    self.descriptor_bytes
                );
            }
            self.tools.push(parse_tool(item)?);
        }
        let Some(next) = result.get("nextCursor") else {
            return Ok(None);
        };
        let next = next
            .as_str()
            .context("tools/list nextCursor가 string이 아님")?
            .to_owned();
        self.descriptor_bytes = self
            .descriptor_bytes
            .checked_add(next.len())
            .context("tools/list cursor byte 수 overflow")?;
        if self.descriptor_bytes > self.max_descriptor_bytes {
            anyhow::bail!(
                "tools/list 누적 descriptor/cursor metadata가 상한({} bytes)을 초과: {} bytes",
                self.max_descriptor_bytes,
                self.descriptor_bytes
            );
        }
        if !self.seen_cursors.insert(next.clone()) {
            anyhow::bail!("tools/list cursor cycle 감지");
        }
        Ok(Some(next))
    }

    fn finish(self) -> Vec<McpTool> {
        self.tools
    }
}

fn parse_tool(item: &Value) -> anyhow::Result<McpTool> {
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .context("tool에 name 없음")?
        .to_owned();
    let description = item
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let input_schema_json = match item.get("inputSchema") {
        Some(schema) => serde_json::to_string(schema)?,
        None => "{}".to_owned(),
    };
    Ok(McpTool {
        name,
        description,
        input_schema_json,
    })
}

// 프로세스 spawn 테스트 — pty crate 관행과 동일하게 unix 한정.
// mock MCP 서버는 /bin/sh -c 스크립트로 만든다 (별도 파일 불필요).
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use secret::SecretString;
    use std::time::Instant;

    fn sh_config(script: &str) -> McpServerConfig {
        McpServerConfig {
            name: "mock".to_owned(),
            command: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), script.to_owned()],
            env: Vec::new(),
            inherit_env: true,
        }
    }

    fn manager() -> LocalMcpManager {
        LocalMcpManager::new(RedactionService::new()).with_request_timeout(Duration::from_secs(5))
    }

    #[test]
    fn discovery_limits_are_cumulative_across_pages() {
        let item = json!({"name":"a","description":"1234","inputSchema":{}});
        let item_bytes = serialized_json_len(&item).unwrap();
        let mut discovery = ToolDiscovery::with_limits(2, item_bytes * 2);
        assert_eq!(
            discovery
                .push_page(&json!({"tools":[item.clone()],"nextCursor":"p2"}))
                .unwrap()
                .as_deref(),
            Some("p2")
        );
        assert!(
            discovery
                .push_page(&json!({"tools":[item.clone(), item]}))
                .unwrap_err()
                .to_string()
                .contains("tool 수")
        );
    }

    #[test]
    fn discovery_descriptor_bytes_and_cursor_cycles_fail_closed() {
        let item = json!({"name":"a","description":"1234","inputSchema":{}});
        let item_bytes = serialized_json_len(&item).unwrap();
        let mut bytes = ToolDiscovery::with_limits(10, item_bytes - 1);
        assert!(
            bytes
                .push_page(&json!({"tools":[item.clone()]}))
                .unwrap_err()
                .to_string()
                .contains("descriptor")
        );

        let mut cursor = ToolDiscovery::with_limits(10, usize::MAX);
        cursor
            .push_page(&json!({"tools":[],"nextCursor":"same"}))
            .unwrap();
        assert!(
            cursor
                .push_page(&json!({"tools":[],"nextCursor":"same"}))
                .unwrap_err()
                .to_string()
                .contains("cycle")
        );
    }

    // 요청 id는 결정적이다: initialize=1, tools/list=2, ...
    const HAPPY_SCRIPT: &str = r#"
read -r _init
echo 'boot: token sk-mock-secret-123456 loaded' >&2
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"mock","version":"0.1"}}}'
read -r _initialized
read -r _list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo_tool","description":"에코","inputSchema":{"type":"object","properties":{}}}]}}'
"#;

    // initialize(id=1) 후 tools/call(id=2)에 결과를 심는 목 서버
    const CALL_SCRIPT: &str = r#"
read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"mock","version":"0.1"}}}'
read -r _initialized
read -r _call
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"hello world"}],"isError":false}}'
"#;

    const ENV_SCRIPT: &str = r#"
read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"mock","version":"0.1"}}}'
read -r _initialized
read -r _list
printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"%s","inputSchema":{"type":"object"}}]}}\n' "$DEPPY_MCP_SCOPED_TOOL"
"#;

    #[test]
    fn call_tool_왕복() {
        let manager = manager();
        let result = manager
            .call_tool(&sh_config(CALL_SCRIPT), "echo_tool", json!({"msg": "hi"}))
            .unwrap();
        assert_eq!(
            result.pointer("/content/0/text").and_then(Value::as_str),
            Some("hello world")
        );
        assert_eq!(result.get("isError").and_then(Value::as_bool), Some(false));
    }

    #[test]
    fn call_tool_큰_payload는_stdio_write전에_거부() {
        let manager = manager();
        let started = Instant::now();
        let error = manager
            .call_tool(
                &McpServerConfig::stdio(
                    "must-not-spawn".to_owned(),
                    "/definitely/missing/mcp-server".to_owned(),
                    Vec::new(),
                    Vec::new(),
                    true,
                ),
                "echo_tool",
                json!({"msg": "x".repeat(crate::transport::MAX_WRITE_LINE_BYTES)}),
            )
            .unwrap_err();

        assert!(
            format!("{error:#}").contains("MCP tool input 크기 초과"),
            "{error:#}"
        );
        assert!(!format!("{error:#}").contains("spawn"), "{error:#}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "큰 payload 거부가 timeout에 의존하면 안 됨"
        );
    }

    #[test]
    fn scoped_env는_stdio_server에만_주입된다() {
        let manager = manager();
        let mut config = sh_config(ENV_SCRIPT);
        config.inherit_env = false;
        config.env = vec![(
            "DEPPY_MCP_SCOPED_TOOL".to_owned(),
            "scoped_env_tool".to_owned(),
        )];

        let tools = manager.discover_tools(&config).unwrap();

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "scoped_env_tool");
        assert!(
            !format!("{config:?}").contains("scoped_env_tool"),
            "Debug must not expose env values"
        );
        assert!(
            !format!("{config:?}").contains(&config.command),
            "Debug must not expose the command"
        );
    }

    #[test]
    fn initialize_tools_list_왕복과_stderr_redaction() {
        let redaction = RedactionService::new();
        redaction.register(&SecretString::new("sk-mock-secret-123456".to_owned()));
        let manager = LocalMcpManager::new(redaction).with_request_timeout(Duration::from_secs(5));

        let mut connection = manager.connect(&sh_config(HAPPY_SCRIPT)).unwrap();
        assert_eq!(
            connection
                .initialize_result
                .pointer("/serverInfo/name")
                .and_then(Value::as_str),
            Some("mock")
        );

        let tools = connection.list_tools().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo_tool");
        assert_eq!(tools[0].description.as_deref(), Some("에코"));
        assert!(tools[0].input_schema_json.contains("\"type\":\"object\""));

        // stderr는 별도 thread가 채운다 — 도착까지 폴링
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let log = connection.stderr_log();
            if log.contains("[REDACTED]") {
                assert!(!log.contains("sk-mock-secret-123456"), "{log}");
                assert!(log.contains("boot:"), "{log}");
                break;
            }
            assert!(Instant::now() < deadline, "stderr 미도착: {log:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn cloneable_cancel_interrupts_hung_stdio_request_and_reaps_resources() {
        let script = r#"
read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"mock","version":"0"}}}'
read -r _initialized
read -r _list
sleep 30
"#;
        let manager = LocalMcpManager::new(RedactionService::new())
            .with_request_timeout(Duration::from_secs(10));
        let mut connection = manager.connect(&sh_config(script)).unwrap();
        let cancellation = connection.cancellation_handle();
        let ready = std::sync::Arc::new(std::sync::Barrier::new(2));
        let request_ready = std::sync::Arc::clone(&ready);
        let request = std::thread::spawn(move || {
            request_ready.wait();
            connection.list_tools()
        });

        ready.wait();
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        cancellation.cancel();
        let error = request.join().unwrap().unwrap_err();
        assert!(!format!("{error:#}").is_empty());
        assert!(started.elapsed() < Duration::from_secs(2));

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let metrics = crate::transport_metrics();
            if metrics.stdio_stdout_threads == 0
                && metrics.stdio_stderr_threads == 0
                && metrics.stdio_writer_threads == 0
            {
                break;
            }
            assert!(Instant::now() < deadline, "stdio resources were not reaped");
            std::thread::yield_now();
        }
    }

    #[test]
    fn 지원_목록의_모든_버전으로_협상_성공() {
        // H1: 서버가 어느 지원 버전으로 응답하든 connect 성공 + negotiated_version 일치
        for version in SUPPORTED_PROTOCOL_VERSIONS {
            let script = format!(
                r#"
read -r _init
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"{version}","capabilities":{{}},"serverInfo":{{"name":"mock","version":"0"}}}}}}'
read -r _initialized
sleep 1
"#
            );
            let connection = manager().connect(&sh_config(&script)).unwrap();
            assert_eq!(&connection.negotiated_version, version);
        }
    }

    #[test]
    fn 지원_목록_밖_버전은_서버버전과_목록을_담아_거부() {
        let script = r#"
read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"1999-01-01","capabilities":{},"serverInfo":{"name":"mock","version":"0"}}}'
sleep 1
"#;
        let error = format!("{:#}", manager().connect(&sh_config(script)).unwrap_err());
        assert!(error.contains("1999-01-01"), "{error}");
        assert!(error.contains("2025-11-25"), "{error}"); // 지원 목록 포함
        assert!(error.contains("미지원"), "{error}");
    }

    #[test]
    fn protocolversion_누락과_빈문자열도_거부() {
        // 필드 자체가 없는 응답 → None, 빈 문자열 → 목록 밖. 둘 다 미지원 거부.
        let missing = r#"
read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"capabilities":{},"serverInfo":{"name":"mock","version":"0"}}}'
sleep 1
"#;
        let error = format!("{:#}", manager().connect(&sh_config(missing)).unwrap_err());
        assert!(error.contains("None"), "{error}");
        assert!(error.contains("미지원"), "{error}");

        let empty = r#"
read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"","capabilities":{},"serverInfo":{"name":"mock","version":"0"}}}'
sleep 1
"#;
        let error = format!("{:#}", manager().connect(&sh_config(empty)).unwrap_err());
        assert!(error.contains("미지원"), "{error}");
    }

    #[test]
    fn stdout_비json_라인은_프로토콜_위반으로_거부() {
        let script = r#"
read -r _init
echo 'starting up...'
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
sleep 1
"#;
        let error = manager().connect(&sh_config(script)).unwrap_err();
        assert!(
            format!("{error:#}").contains("stdout 프로토콜 위반"),
            "{error:#}"
        );
    }

    #[test]
    fn 요청하지_않은_id의_response는_위반() {
        // 유일한 outstanding 요청은 initialize(id=1) — id 99 response는 상관 위반
        let script = r#"
read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":99,"result":{}}'
sleep 1
"#;
        let error = manager().connect(&sh_config(script)).unwrap_err();
        assert!(
            format!("{error:#}").contains("요청하지 않은 id"),
            "{error:#}"
        );
    }

    #[test]
    fn 요청_전에_선점된_미래_id_response는_위반() {
        // initialize 직후 서버가 tools/list(id=2) 응답을 미리 심는다
        let script = r#"
read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"mock","version":"0"}}}'
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"fake","inputSchema":{}}]}}'
sleep 2
"#;
        let mut connection = manager().connect(&sh_config(script)).unwrap();
        // reader thread가 선점 response를 큐에 넣을 시간을 준다
        std::thread::sleep(Duration::from_millis(300));
        let error = connection.list_tools().unwrap_err();
        assert!(
            format!("{error:#}").contains("outstanding 요청이 없는데"),
            "{error:#}"
        );
    }

    #[test]
    fn 유효한_notification_라인은_통과() {
        let script = r#"
read -r _init
printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":"hello"}}'
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"mock","version":"0"}}}'
read -r _initialized
read -r _list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}'
"#;
        let tools = manager().discover_tools(&sh_config(script)).unwrap();
        assert!(tools.is_empty());
    }

    #[test]
    fn tools_list_cursor_페이지네이션() {
        let script = r#"
read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"mock","version":"0"}}}'
read -r _initialized
read -r _list1
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"a","inputSchema":{}}],"nextCursor":"p2"}}'
read -r _list2
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"b","inputSchema":{}}]}}'
"#;
        let tools = manager().discover_tools(&sh_config(script)).unwrap();
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(tools[0].description, None);
        assert_eq!(tools[0].input_schema_json, "{}");
    }

    #[test]
    fn 응답_없으면_timeout() {
        let script = "read -r _init\nsleep 30\n";
        let manager = LocalMcpManager::new(RedactionService::new())
            .with_request_timeout(Duration::from_millis(300));
        let started = Instant::now();
        let error = manager.connect(&sh_config(script)).unwrap_err();
        assert!(format!("{error:#}").contains("timeout"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(5)); // 무한 대기 금지
        // connect 실패 경로에서도 child는 drop에서 kill+reap된다
    }

    #[test]
    fn 서버_조기_종료는_에러() {
        let error = manager().connect(&sh_config("exit 0")).unwrap_err();
        assert!(format!("{error:#}").contains("종료"), "{error:#}");
    }
}
