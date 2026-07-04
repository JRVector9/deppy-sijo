//! 실제 백엔드 MCP 서버로 tools/list·tools/call을 위임하는 ToolForwarder 구현.
//!
//! 연결 수명: **호출당 connect**. LocalMcpManager의 discover_tools/call_tool은
//! 이미 매 호출마다 백엔드 subprocess를 spawn→initialize→요청→종료(kill/reap)한다
//! (manager.rs "stdio 서버는 매 호출마다 새 subprocess" MVP 단순화). 여기서도 그 관행을
//! 그대로 따른다 — 영속 연결 상태를 들고 다니지 않아 단순하고, 백엔드 프로세스 누수가 없다.
//! (프록시 자체가 짧게 사는 per-agent 프로세스라 재spawn 비용은 MVP에서 허용된다.)

use anyhow::Context;
use mcp::{LocalMcpManager, McpServerConfig, ToolForwarder};
use serde_json::{Value, json};

/// 백엔드 서버 spec + manager를 소유하고 매 호출마다 새로 연결해 포워딩한다.
pub struct ManagerToolForwarder {
    manager: LocalMcpManager,
    config: McpServerConfig,
}

impl ManagerToolForwarder {
    pub fn new(manager: LocalMcpManager, config: McpServerConfig) -> Self {
        Self { manager, config }
    }
}

impl ToolForwarder for ManagerToolForwarder {
    /// tools/list → 백엔드에서 발견한 tool을 MCP `{"tools":[...]}` result로 재구성한다.
    fn list_tools(&self) -> anyhow::Result<Value> {
        let discovered = self
            .manager
            .discover_tools(&self.config)
            .with_context(|| format!("백엔드 '{}' tools/list 실패", self.config.name))?;
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
        self.manager
            .call_tool(&self.config, name, arguments)
            .with_context(|| format!("백엔드 '{}' tools/call({name}) 실패", self.config.name))
    }
}
