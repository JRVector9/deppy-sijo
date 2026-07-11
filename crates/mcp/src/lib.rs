//! PR-15 Local MCP Manager (설계문서 §1.5, §11.4–11.5) + H2 Streamable HTTP.
//! local stdio MCP: subprocess spawn → initialize → initialized → tools/list.
//! Streamable HTTP MCP(H2): POST JSON-RPC + SSE 응답, Mcp-Session-Id 세션.
//! 완료 기준: stdout에는 valid MCP message만 허용 / stderr log capture+redaction.
//! 앱 마이그레이션 통합·UI 배선은 crates/app 소관.
//! SQL/Row/DDL은 mcp-store 소유(v2.8) — 이 crate는 프로토콜/매니저(runtime)만 제공한다.

mod http;
mod manager;
mod proxy;
mod transport;

pub use http::{McpAuthRequired, McpHttpServerConfig, validate_mcp_url};
pub use manager::{LocalMcpManager, McpConnection, McpServerConfig, McpTool, PROTOCOL_VERSION};
pub use proxy::{PermissionHook, ProxyDecision, ToolForwarder, run_proxy};
