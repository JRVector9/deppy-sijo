//! PR-15 Local MCP Manager (설계문서 §1.5, §11.4–11.5).
//! local stdio MCP: subprocess spawn → initialize → initialized → tools/list.
//! 완료 기준: stdout에는 valid MCP message만 허용 / stderr log capture+redaction.
//! 앱 마이그레이션 통합·UI 배선은 crates/app 소관 — 이 crate는
//! 프로토콜/매니저 로직 + DDL 상수 + repository 함수만 제공한다.

mod manager;
mod repo;
mod transport;

pub use manager::{LocalMcpManager, McpConnection, McpServerConfig, McpTool, PROTOCOL_VERSION};
pub use repo::{
    McpServerRow, McpToolRow, insert_server, insert_tool, list_servers, list_tools_for_server,
    replace_tools_for_server,
};

/// §11.4 mcp_servers + §11.5 mcp_tools DDL (index는 §11.8).
/// 오케스트레이터가 앱 마이그레이션 배열에 그대로 붙인다.
pub const MIGRATION_SQL: &str = "
CREATE TABLE mcp_servers (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    command TEXT,
    args_json TEXT,
    url TEXT,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE mcp_tools (
    id TEXT PRIMARY KEY,
    server_id TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT,
    input_schema_json TEXT,
    trust_level TEXT NOT NULL DEFAULT 'unknown',
    schema_hash TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY(server_id) REFERENCES mcp_servers(id)
);

CREATE INDEX idx_mcp_tools_server_name ON mcp_tools(server_id, name);
";
