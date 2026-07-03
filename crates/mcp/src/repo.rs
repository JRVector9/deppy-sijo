//! mcp_servers / mcp_tools repository (설계문서 §11.4–11.5).
//! created_at/updated_at은 SQLite가 UTC로 기록한다 (app storage와 동일 관례).
//! schema_hash 기록·검증은 PR-16 audit 소관 — 여기서는 컬럼만 유지 (NULL 허용).

use anyhow::Context;
use rusqlite::Connection;

/// §11.4 mcp_servers 한 행.
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerRow {
    pub id: String,
    pub name: String,
    /// v0는 'stdio'만 (§1.5) — 'http'는 v1+
    pub kind: String,
    pub command: Option<String>,
    /// args_json 컬럼에 JSON 배열로 저장
    pub args: Vec<String>,
    pub url: Option<String>,
    pub enabled: bool,
}

/// §11.5 mcp_tools 한 행.
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolRow {
    pub id: String,
    pub server_id: String,
    pub name: String,
    pub description: Option<String>,
    pub input_schema_json: Option<String>,
    /// 기본 'unknown' — 신뢰 승격/정책은 PR-16
    pub trust_level: String,
    /// PR-16 audit이 기록 — 여기서는 NULL 허용 통과만
    pub schema_hash: Option<String>,
}

pub fn insert_server(conn: &Connection, row: &McpServerRow) -> anyhow::Result<()> {
    let args_json = serde_json::to_string(&row.args)?;
    conn.execute(
        "INSERT INTO mcp_servers (id, name, kind, command, args_json, url, enabled, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7,
            strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
        (
            &row.id,
            &row.name,
            &row.kind,
            &row.command,
            &args_json,
            &row.url,
            row.enabled,
        ),
    )
    .with_context(|| format!("mcp_server 저장 실패: {}", row.name))?;
    Ok(())
}

pub fn list_servers(conn: &Connection) -> anyhow::Result<Vec<McpServerRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, command, args_json, url, enabled
         FROM mcp_servers ORDER BY created_at, id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, bool>(6)?,
        ))
    })?;
    let mut servers = Vec::new();
    for row in rows {
        let (id, name, kind, command, args_json, url, enabled) = row?;
        let args = match args_json {
            Some(json) => {
                serde_json::from_str(&json).with_context(|| format!("args_json 파싱 실패: {id}"))?
            }
            None => Vec::new(),
        };
        servers.push(McpServerRow {
            id,
            name,
            kind,
            command,
            args,
            url,
            enabled,
        });
    }
    Ok(servers)
}

pub fn insert_tool(conn: &Connection, row: &McpToolRow) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO mcp_tools
           (id, server_id, name, description, input_schema_json, trust_level, schema_hash,
            created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7,
            strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
        (
            &row.id,
            &row.server_id,
            &row.name,
            &row.description,
            &row.input_schema_json,
            &row.trust_level,
            &row.schema_hash,
        ),
    )
    .with_context(|| format!("mcp_tool 저장 실패: {}", row.name))?;
    Ok(())
}

pub fn list_tools_for_server(
    conn: &Connection,
    server_id: &str,
) -> anyhow::Result<Vec<McpToolRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, server_id, name, description, input_schema_json, trust_level, schema_hash
         FROM mcp_tools WHERE server_id = ?1 ORDER BY name, id",
    )?;
    let rows = stmt.query_map([server_id], |row| {
        Ok(McpToolRow {
            id: row.get(0)?,
            server_id: row.get(1)?,
            name: row.get(2)?,
            description: row.get(3)?,
            input_schema_json: row.get(4)?,
            trust_level: row.get(5)?,
            schema_hash: row.get(6)?,
        })
    })?;
    let mut tools = Vec::new();
    for row in rows {
        tools.push(row?);
    }
    Ok(tools)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MIGRATION_SQL;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // 앱은 모든 연결에 foreign_keys=ON을 강제한다 (§11.9) — 테스트도 동일 조건
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(MIGRATION_SQL).unwrap();
        conn
    }

    fn sample_server() -> McpServerRow {
        McpServerRow {
            id: "srv-1".to_owned(),
            name: "filesystem".to_owned(),
            kind: "stdio".to_owned(),
            command: Some("npx".to_owned()),
            args: vec!["-y".to_owned(), "server-filesystem".to_owned()],
            url: None,
            enabled: true,
        }
    }

    #[test]
    fn server_insert_list_roundtrip() {
        let conn = test_conn();
        let server = sample_server();
        insert_server(&conn, &server).unwrap();
        assert_eq!(list_servers(&conn).unwrap(), vec![server]);
    }

    #[test]
    fn tool_insert_list_roundtrip() {
        let conn = test_conn();
        insert_server(&conn, &sample_server()).unwrap();
        let tool = McpToolRow {
            id: "tool-1".to_owned(),
            server_id: "srv-1".to_owned(),
            name: "read_file".to_owned(),
            description: Some("파일 읽기".to_owned()),
            input_schema_json: Some(r#"{"type":"object"}"#.to_owned()),
            trust_level: "unknown".to_owned(),
            schema_hash: None, // PR-16 소관 — NULL 허용
        };
        insert_tool(&conn, &tool).unwrap();
        assert_eq!(list_tools_for_server(&conn, "srv-1").unwrap(), vec![tool]);
        assert!(list_tools_for_server(&conn, "srv-2").unwrap().is_empty());
    }

    #[test]
    fn 없는_서버로_tool_insert는_fk_위반() {
        let conn = test_conn();
        let tool = McpToolRow {
            id: "tool-x".to_owned(),
            server_id: "no-such-server".to_owned(),
            name: "x".to_owned(),
            description: None,
            input_schema_json: None,
            trust_level: "unknown".to_owned(),
            schema_hash: None,
        };
        assert!(insert_tool(&conn, &tool).is_err());
    }
}
