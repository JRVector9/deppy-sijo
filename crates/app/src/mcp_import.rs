//! MCP 서버 import 파서 (커넥터 센터 — JSON 붙여넣기 · 파일 · Claude Desktop 설정).
//! 생태계 표준 `mcpServers` JSON 블록을 파싱해 stdio 서버 등록 후보로 변환한다.
//! v0는 stdio only(§1.5) — url/type이 HTTP 계열인 항목은 이유와 함께 건너뛴다.
//! secret-like env 값은 저장하지 않고 제외 키 목록으로 보고한다 (credential binding 유도).

use anyhow::Context;
use serde::Deserialize;
use serde_json::Value;

/// 파싱된 stdio 서버 1개 (등록 후보).
#[derive(Debug, PartialEq)]
pub struct ParsedServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// secret-like가 아닌 plain env만 (제외분은 skipped_env)
    pub env_plain: Vec<(String, String)>,
    /// secret-like 값/키 또는 문자열이 아닌 값이라 제외한 env key
    pub skipped_env: Vec<String>,
}

/// 항목을 건너뛴 이유.
#[derive(Debug, PartialEq)]
pub enum SkipReason {
    /// url 또는 type이 HTTP 계열 — Streamable HTTP는 v1 (§1.5)
    HttpTransport,
    /// command가 없거나 비어 있음
    MissingCommand,
    /// 항목 형식이 스펙과 다름 (파싱 에러 메시지)
    Invalid(String),
}

#[derive(Debug, PartialEq)]
pub struct SkippedServer {
    pub name: String,
    pub reason: SkipReason,
}

#[derive(Debug, Default, PartialEq)]
pub struct ImportParse {
    pub servers: Vec<ParsedServer>,
    pub skipped: Vec<SkippedServer>,
}

/// 서버 항목 하나의 느슨한 스펙. 알 수 없는 필드(disabled 등)는 무시한다.
#[derive(Deserialize)]
struct RawServer {
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: serde_json::Map<String, Value>,
    url: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

/// `{"mcpServers": {...}}` / `{"servers": {...}}` wrapper 또는
/// name→spec 맨 객체를 받아들인다 (README·설정 파일에서 그대로 복사 가능하게).
pub fn parse_mcp_servers_json(text: &str) -> anyhow::Result<ImportParse> {
    let root: Value = serde_json::from_str(text.trim()).context("JSON 파싱 실패")?;
    let map = server_map(&root)?;

    let mut parse = ImportParse::default();
    for (name, value) in map {
        let raw: RawServer = match serde_json::from_value(value.clone()) {
            Ok(raw) => raw,
            Err(e) => {
                parse.skipped.push(SkippedServer {
                    name: name.clone(),
                    reason: SkipReason::Invalid(e.to_string()),
                });
                continue;
            }
        };
        let kind = raw.kind.as_deref().unwrap_or("").to_ascii_lowercase();
        if raw.url.is_some()
            || matches!(
                kind.as_str(),
                "http" | "sse" | "streamable-http" | "streamable_http"
            )
        {
            parse.skipped.push(SkippedServer {
                name: name.clone(),
                reason: SkipReason::HttpTransport,
            });
            continue;
        }
        let Some(command) = raw.command.filter(|c| !c.trim().is_empty()) else {
            parse.skipped.push(SkippedServer {
                name: name.clone(),
                reason: SkipReason::MissingCommand,
            });
            continue;
        };

        // env 분류: 저장 가능한 plain 값만 통과 — secret-like/비문자열/잘못된 key는
        // 제외 목록으로 보고해 credential binding으로 넣도록 안내한다.
        let mut env_plain = Vec::new();
        let mut skipped_env = Vec::new();
        for (key, value) in &raw.env {
            let ok = value.as_str().is_some_and(|value| {
                let pair = [(key.clone(), value.to_owned())];
                mcp_store::validate_server_env_for_persistence(&pair, &[]).is_ok()
            });
            if ok {
                env_plain.push((key.clone(), value.as_str().unwrap_or_default().to_owned()));
            } else {
                skipped_env.push(key.clone());
            }
        }
        parse.servers.push(ParsedServer {
            name: name.clone(),
            command,
            args: raw.args,
            env_plain,
            skipped_env,
        });
    }
    Ok(parse)
}

fn server_map(root: &Value) -> anyhow::Result<&serde_json::Map<String, Value>> {
    let obj = root
        .as_object()
        .context("JSON 최상위가 object가 아닙니다")?;
    for key in ["mcpServers", "servers"] {
        if let Some(Value::Object(map)) = obj.get(key) {
            return Ok(map);
        }
    }
    // wrapper 없이 name→spec 맨 객체로 붙여넣은 경우 (모든 값이 object일 때만)
    if !obj.is_empty() && obj.values().all(Value::is_object) {
        return Ok(obj);
    }
    anyhow::bail!("mcpServers 항목을 찾을 수 없습니다")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_servers_wrapper_파싱() {
        let parse = parse_mcp_servers_json(
            r#"{"mcpServers": {"fs": {
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
                "env": {"LOG_LEVEL": "debug"}
            }}}"#,
        )
        .unwrap();
        assert!(parse.skipped.is_empty());
        assert_eq!(parse.servers.len(), 1);
        let server = &parse.servers[0];
        assert_eq!(server.name, "fs");
        assert_eq!(server.command, "npx");
        assert_eq!(
            server.args,
            ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
        );
        assert_eq!(
            server.env_plain,
            [("LOG_LEVEL".to_owned(), "debug".to_owned())]
        );
        assert!(server.skipped_env.is_empty());
    }

    #[test]
    fn servers_wrapper와_맨_객체_허용() {
        for text in [
            r#"{"servers": {"a": {"command": "uvx", "args": ["mcp-server-fetch"]}}}"#,
            r#"{"a": {"command": "uvx", "args": ["mcp-server-fetch"]}}"#,
        ] {
            let parse = parse_mcp_servers_json(text).unwrap();
            assert_eq!(parse.servers.len(), 1, "{text}");
            assert_eq!(parse.servers[0].command, "uvx");
        }
    }

    #[test]
    fn http_항목은_v1_사유로_건너뜀() {
        let parse = parse_mcp_servers_json(
            r#"{"mcpServers": {
                "remote": {"url": "https://mcp.example.com"},
                "typed": {"type": "http", "command": "ignored"},
                "local": {"command": "npx"}
            }}"#,
        )
        .unwrap();
        assert_eq!(parse.servers.len(), 1);
        assert_eq!(parse.servers[0].name, "local");
        assert_eq!(parse.skipped.len(), 2);
        assert!(
            parse
                .skipped
                .iter()
                .all(|s| s.reason == SkipReason::HttpTransport)
        );
    }

    #[test]
    fn command_없으면_건너뜀() {
        let parse = parse_mcp_servers_json(
            r#"{"mcpServers": {"empty": {"args": ["-y"]}, "blank": {"command": "  "}}}"#,
        )
        .unwrap();
        assert!(parse.servers.is_empty());
        assert_eq!(parse.skipped.len(), 2);
        assert!(
            parse
                .skipped
                .iter()
                .all(|s| s.reason == SkipReason::MissingCommand)
        );
    }

    #[test]
    fn secret_like_env는_제외되고_보고된다() {
        let parse = parse_mcp_servers_json(
            r#"{"mcpServers": {"gh": {
                "command": "npx",
                "env": {
                    "GITHUB_PERSONAL_ACCESS_TOKEN": "ghp_secret1234567890",
                    "PORT": "8080",
                    "COUNT": 3
                }
            }}}"#,
        )
        .unwrap();
        let server = &parse.servers[0];
        assert_eq!(server.env_plain, [("PORT".to_owned(), "8080".to_owned())]);
        let mut skipped = server.skipped_env.clone();
        skipped.sort();
        assert_eq!(skipped, ["COUNT", "GITHUB_PERSONAL_ACCESS_TOKEN"]);
    }

    #[test]
    fn 형식_오류_항목은_이유와_함께_건너뜀() {
        let parse = parse_mcp_servers_json(
            r#"{"mcpServers": {"bad": {"command": "npx", "args": [1, 2]}}}"#,
        )
        .unwrap();
        assert!(parse.servers.is_empty());
        assert_eq!(parse.skipped.len(), 1);
        assert!(matches!(parse.skipped[0].reason, SkipReason::Invalid(_)));
    }

    #[test]
    fn 잘못된_json과_wrapper_부재는_에러() {
        assert!(parse_mcp_servers_json("{not json").is_err());
        assert!(parse_mcp_servers_json(r#"{"foo": "bar"}"#).is_err());
        assert!(parse_mcp_servers_json("[]").is_err());
    }
}
