//! Local MCP Manager (설계문서 §1.5 v0 / PR-15).
//! spawn → initialize 핸드셰이크 → initialized notification → tools/list.
//! v0는 local stdio MCP only — Streamable HTTP/OAuth는 v1+ (§1.5).

use std::time::Duration;

use anyhow::Context;
use secret::RedactionService;
use serde_json::{Value, json};

use crate::transport::StdioClient;

/// 기준 스펙 개정판 (§1.5 — PR-17/18/19 시작 시 최신 개정판 재확인).
pub const PROTOCOL_VERSION: &str = "2025-11-25";

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// tools/list cursor 페이지네이션 상한 — 악의적 서버의 무한 cursor 방어
const MAX_TOOL_PAGES: usize = 100;

/// local stdio MCP 서버 실행 스펙 (§11.4 kind='stdio' 행에 대응).
/// scoped env secret은 spawn 직전 호출측이 해석해 env에 넣는다. Debug는 env 값을 출력하지 않는다.
#[derive(Clone, PartialEq)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub inherit_env: bool,
}

impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let env_keys: Vec<&str> = self.env.iter().map(|(key, _)| key.as_str()).collect();
        f.debug_struct("McpServerConfig")
            .field("name", &self.name)
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env_keys", &env_keys)
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
        let mut client = StdioClient::spawn(
            &config.command,
            &config.args,
            &config.env,
            config.inherit_env,
            &self.redaction,
            self.request_timeout,
        )
        .with_context(|| format!("MCP 서버 '{}' spawn 실패", config.name))?;

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

        // §1.5는 2025-11-25 개정판을 기준으로 명시한다 — 다른 개정판은
        // 프로토콜 gate에서 거부한다 (codex 리뷰 반영: warn만으로는 기준 미달)
        let server_version = initialize_result
            .get("protocolVersion")
            .and_then(Value::as_str);
        if server_version != Some(PROTOCOL_VERSION) {
            anyhow::bail!(
                "MCP 서버 '{}' protocolVersion 불일치: {:?} (기준 {PROTOCOL_VERSION})",
                config.name,
                server_version
            );
        }

        client
            .notify("notifications/initialized", json!({}))
            .with_context(|| format!("MCP 서버 '{}' initialized notification 실패", config.name))?;

        Ok(McpConnection {
            client,
            initialize_result,
        })
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
        let mut connection = self.connect(config)?;
        connection.call_tool(name, arguments)
        // connection drop → 서버 프로세스 정리
    }
}

/// initialize를 마친 stdio MCP 연결. drop 시 서버 프로세스를 kill + reap한다.
#[derive(Debug)]
pub struct McpConnection {
    client: StdioClient,
    /// initialize 응답 원본 (protocolVersion / capabilities / serverInfo)
    pub initialize_result: Value,
}

impl McpConnection {
    /// tools/list 요청 → tool 목록 (cursor 페이지네이션 포함).
    pub fn list_tools(&mut self) -> anyhow::Result<Vec<McpTool>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_TOOL_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({"cursor": cursor}),
                None => json!({}),
            };
            let result = self.client.request("tools/list", params)?;
            let list = result
                .get("tools")
                .and_then(Value::as_array)
                .context("tools/list 응답에 tools 배열 없음")?;
            for item in list {
                tools.push(parse_tool(item)?);
            }
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) => cursor = Some(next.to_owned()),
                None => return Ok(tools),
            }
        }
        anyhow::bail!("tools/list 페이지가 {MAX_TOOL_PAGES}를 초과 — cursor 순환 의심");
    }

    /// tools/call 요청 → 결과 Value (content 배열 + 선택적 isError).
    /// `arguments`는 JSON object여야 한다 (MCP 스펙). isError=true는 프로토콜
    /// 오류가 아니라 tool이 보고한 실패이므로 결과를 그대로 돌려준다 — 판단은 호출측.
    pub fn call_tool(&mut self, name: &str, arguments: Value) -> anyhow::Result<Value> {
        self.client
            .request("tools/call", json!({"name": name, "arguments": arguments}))
    }

    /// 지금까지 캡처된 redacted stderr 로그.
    pub fn stderr_log(&self) -> String {
        self.client.stderr_log()
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
        let script = r#"
read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"mock","version":"0.1"}}}'
read -r _initialized
sleep 30
"#;
        let manager = manager();
        let started = Instant::now();
        let error = manager
            .call_tool(
                &sh_config(script),
                "echo_tool",
                json!({"msg": "x".repeat(crate::transport::MAX_WRITE_LINE_BYTES)}),
            )
            .unwrap_err();

        assert!(
            format!("{error:#}").contains("MCP 요청 크기 초과"),
            "{error:#}"
        );
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
