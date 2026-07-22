//! mcp-store — MCP 영속 계층 (v2.8 §6.4).
//!
//! mcp_servers/mcp_tools + tool_permission_rules + pending_approvals의 SQL/Row를 소유한다.
//! 모든 함수는 Connection을 인자로 받는다(연결/트랜잭션 소유는 호출측 — storage facade 또는
//! persist orchestration). **mcp(runtime)를 모른다** — 규칙(rule)은 문자열("allow"/"deny"/
//! "ask")로만 다루고 해석(PermissionRule)은 상위 계층 소관. audit도 모른다(금지 edge).
//!
//! 마이그레이션 원장: 이 crate는 SQL 상수만 제공하고, 전역 순서(v5/v8/v9 슬롯)는
//! storage의 MIGRATIONS가 소유한다 (재배열 금지 — docs/dependency-graph.md).

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

/// §11.4 mcp_servers + §11.5 mcp_tools DDL — 전역 마이그레이션 v5 슬롯.
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

/// tool 권한 규칙 DDL (PR-16) — 전역 마이그레이션 v8 슬롯.
pub const MIGRATION_TOOL_PERMISSION_RULES: &str = "
CREATE TABLE tool_permission_rules (
    server_id TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    rule TEXT NOT NULL,
    approved_schema_hash TEXT,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (server_id, tool_name)
);
";

/// pending_approvals DDL (agent-proxy 1.5 라이브 승인 IPC) — 전역 마이그레이션 v9 슬롯.
pub const MIGRATION_PENDING_APPROVALS: &str = "
CREATE TABLE pending_approvals (
    id TEXT PRIMARY KEY,
    server_id TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    arguments_preview TEXT NOT NULL,
    schema_hash TEXT,
    status TEXT NOT NULL DEFAULT 'pending',
    remember INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    resolved_at INTEGER,
    CHECK (status IN ('pending', 'allowed', 'denied'))
);

CREATE INDEX idx_pending_approvals_status ON pending_approvals(status);
";

/// 승인 ↔ 세션 연결 (I2). 승인이 어느 pane에서 났는지 기록한다. 전역 마이그레이션 원장이
/// 소유하고, 테스트도 이 상수를 적용해 스키마를 일치시킨다.
pub const MIGRATION_APPROVAL_PANE: &str = "ALTER TABLE pending_approvals ADD COLUMN pane_id TEXT;";

/// scoped MCP env metadata — 전역 마이그레이션 v12 슬롯.
/// env_json은 안전한 plain 값만, env_credentials_json은 key→credential_id만 저장한다.
pub const MIGRATION_SERVER_ENV: &str = "
ALTER TABLE mcp_servers ADD COLUMN env_json TEXT;
ALTER TABLE mcp_servers ADD COLUMN env_credentials_json TEXT;
ALTER TABLE mcp_servers ADD COLUMN inherit_env INTEGER NOT NULL DEFAULT 1;
";

/// tool 권한 규칙 한 행. rule은 영속 문자열("allow"|"deny"|"ask") — 해석은 상위 계층.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRuleRow {
    pub server_id: String,
    pub tool_name: String,
    pub rule: String,
    pub approved_schema_hash: Option<String>,
}

/// pending_approvals.status 값. DB 문자열 'pending' | 'allowed' | 'denied'와 1:1 대응.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalStatus {
    Pending,
    Allowed,
    Denied,
}

impl ApprovalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalStatus::Pending => "pending",
            ApprovalStatus::Allowed => "allowed",
            ApprovalStatus::Denied => "denied",
        }
    }

    /// 영속 문자열에서 복원 — 알 수 없는 값은 None (호출측이 fail-closed 처리).
    pub fn from_persisted(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(ApprovalStatus::Pending),
            "allowed" => Some(ApprovalStatus::Allowed),
            "denied" => Some(ApprovalStatus::Denied),
            _ => None,
        }
    }
}

/// poll_approval 결과 — 현재 상태 + "기억하기"(규칙 영속) 플래그.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApprovalOutcome {
    pub status: ApprovalStatus,
    pub remember: bool,
}

/// pending 상태 승인 요청 한 행 (GUI 목록/팝업용).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApprovalRow {
    pub id: String,
    pub server_id: String,
    pub tool_name: String,
    /// 이미 redact된 표시용 미리보기 (원문 secret 아님).
    pub arguments_preview: String,
    pub schema_hash: Option<String>,
    pub created_at: i64,
    /// 승인을 요청한 세션 키 — proxy가 env `DEPPY_SESSION_ID`를 그대로 싣는다(I2).
    /// 형식은 `{workspace_id}:{session_id}`(`deppy_core::parse_session_key`).
    /// NULL이면 세션 불명.
    ///
    /// 컬럼 이름이 `pane_id`지만 **`mux_panes.id`가 아니다** — 이름에 속아 조인하면
    /// 절대 매칭되지 않는다(2026-07-17 실측: 그 조인이 여기 있었고, 그래서 세션
    /// UUID/제목이 프로덕션에서 늘 NULL이었다). 세션 해석은 파싱 후 런타임 상태에서
    /// 한다 — 소비처가 아는 정보이지 DB가 아는 정보가 아니다.
    pub pane_id: Option<String>,
}

/// 새 pending approval insert 요청. 표시 문자열은 이미 redacted된 preview만 허용한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApprovalInsert {
    pub id: String,
    pub server_id: String,
    pub tool_name: String,
    pub arguments_preview: String,
    pub schema_hash: Option<String>,
    pub created_at: i64,
    /// 요청 pane_id (I2 — proxy가 env로 아는 값. 없으면 None → "세션 불명").
    pub pane_id: Option<String>,
}

pub fn list_permission_rules(conn: &Connection) -> anyhow::Result<Vec<PermissionRuleRow>> {
    let mut stmt = conn.prepare(
        "SELECT server_id, tool_name, rule, approved_schema_hash FROM tool_permission_rules",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(PermissionRuleRow {
            server_id: row.get(0)?,
            tool_name: row.get(1)?,
            rule: row.get(2)?,
            approved_schema_hash: row.get(3)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

/// Bounded point lookup used on the authorization hot path. Missing rows mean Ask.
pub fn permission_rule(
    conn: &Connection,
    server_id: &str,
    tool_name: &str,
) -> anyhow::Result<Option<PermissionRuleRow>> {
    conn.query_row(
        "SELECT server_id, tool_name, rule, approved_schema_hash
         FROM tool_permission_rules
         WHERE server_id = ?1 AND tool_name = ?2",
        (server_id, tool_name),
        |row| {
            Ok(PermissionRuleRow {
                server_id: row.get(0)?,
                tool_name: row.get(1)?,
                rule: row.get(2)?,
                approved_schema_hash: row.get(3)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

/// 권한 규칙 저장/갱신 (AllowAlways/DenyAlways 결정 시).
pub fn upsert_permission_rule(
    conn: &Connection,
    server_id: &str,
    tool_name: &str,
    rule: &str,
    approved_schema_hash: Option<&str>,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO tool_permission_rules
           (server_id, tool_name, rule, approved_schema_hash, updated_at)
         VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
         ON CONFLICT(server_id, tool_name) DO UPDATE SET
           rule = excluded.rule,
           approved_schema_hash = excluded.approved_schema_hash,
           updated_at = excluded.updated_at",
        (server_id, tool_name, rule, approved_schema_hash),
    )?;
    Ok(())
}

/// 권한 규칙 삭제 (Ask로 재설정 — 행이 없으면 기본값 Ask).
pub fn delete_permission_rule(
    conn: &Connection,
    server_id: &str,
    tool_name: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM tool_permission_rules WHERE server_id = ?1 AND tool_name = ?2",
        (server_id, tool_name),
    )?;
    Ok(())
}

/// 라이브 승인 요청을 등록한다 (deppy-mcp-proxy → GUI). id는 호출측이 만든 UUID,
/// created_at은 호출측이 SystemTime으로 넘긴 unix seconds. arguments_preview는
/// proxy가 이미 redact한 표시용 문자열이어야 한다.
#[allow(clippy::too_many_arguments)]
pub fn insert_pending_approval(
    conn: &Connection,
    id: &str,
    server_id: &str,
    tool_name: &str,
    arguments_preview: &str,
    schema_hash: Option<&str>,
    created_at: i64,
    pane_id: Option<&str>,
) -> anyhow::Result<()> {
    let row = PendingApprovalInsert {
        id: id.to_owned(),
        server_id: server_id.to_owned(),
        tool_name: tool_name.to_owned(),
        arguments_preview: arguments_preview.to_owned(),
        schema_hash: schema_hash.map(str::to_owned),
        created_at,
        pane_id: pane_id.map(str::to_owned),
    };
    insert_pending_approval_batch(conn, &[row])?;
    Ok(())
}

/// pending approval insert를 한 prepared statement로 반복 실행한다.
pub fn insert_pending_approval_batch(
    conn: &Connection,
    rows: &[PendingApprovalInsert],
) -> anyhow::Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let mut stmt = conn.prepare_cached(
        "INSERT INTO pending_approvals
           (id, server_id, tool_name, arguments_preview, schema_hash,
            status, remember, created_at, pane_id)
         VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6, ?7)",
    )?;
    let mut inserted = 0;
    for row in rows {
        stmt.execute((
            &row.id,
            &row.server_id,
            &row.tool_name,
            &row.arguments_preview,
            &row.schema_hash,
            row.created_at,
            &row.pane_id,
        ))
        .with_context(|| format!("pending approval 저장 실패: {}", row.id))?;
        inserted += 1;
    }
    Ok(inserted)
}

/// 현재 상태를 폴링한다 (proxy가 반복 호출). 행이 없으면 Err — fail-closed.
pub fn poll_approval(conn: &Connection, id: &str) -> anyhow::Result<ApprovalOutcome> {
    let row: Option<(String, i64)> = conn
        .query_row(
            "SELECT status, remember FROM pending_approvals WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (status, remember) = row.with_context(|| format!("pending approval 없음: {id}"))?;
    let status = ApprovalStatus::from_persisted(&status)
        .with_context(|| format!("알 수 없는 approval status '{status}': {id}"))?;
    Ok(ApprovalOutcome {
        status,
        remember: remember != 0,
    })
}

/// pending 상태 요청만, 오래된 순으로 (GUI 목록). id는 tie-break(결정적 순서).
pub fn list_pending_approvals(conn: &Connection) -> anyhow::Result<Vec<PendingApprovalRow>> {
    // 승인 행만 읽는다. 예전엔 여기서 `LEFT JOIN mux_panes p ON p.id = a.pane_id`로
    // 세션 UUID/제목을 채우려 했지만, a.pane_id는 런타임 세션 키(`{ws}:{u64}`)이고
    // mux_panes.id는 UUID라 **절대 매칭되지 않았다** — 2026-07-17 실측으로 확인하고
    // 조인을 걷어냈다(500ms 폴링마다 헛돌던 조인 2개도 함께 사라진다).
    // 세션 해석은 pane_id를 파싱해 런타임 상태에서 하는 소비처의 몫이다.
    let mut stmt = conn.prepare(
        "SELECT a.id, a.server_id, a.tool_name, a.arguments_preview, a.schema_hash,
                a.created_at, a.pane_id
         FROM pending_approvals a
         WHERE a.status = 'pending' ORDER BY a.created_at, a.id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(PendingApprovalRow {
            id: row.get(0)?,
            server_id: row.get(1)?,
            tool_name: row.get(2)?,
            arguments_preview: row.get(3)?,
            schema_hash: row.get(4)?,
            created_at: row.get(5)?,
            pane_id: row.get(6)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

/// GUI가 결정을 되쓴다. first-writer-wins — 없거나 이미 해소된 id면 조용한 no-op(Ok).
pub fn resolve_approval(
    conn: &Connection,
    id: &str,
    allowed: bool,
    remember: bool,
    resolved_at: i64,
) -> anyhow::Result<()> {
    let status = if allowed {
        ApprovalStatus::Allowed
    } else {
        ApprovalStatus::Denied
    };
    conn.execute(
        "UPDATE pending_approvals
         SET status = ?2, remember = ?3, resolved_at = ?4
         WHERE id = ?1 AND status = 'pending'",
        (id, status.as_str(), remember as i64, resolved_at),
    )
    .with_context(|| format!("approval 해소 실패: {id}"))?;
    Ok(())
}

/// 크래시로 남은 orphan pending 승인 행 정리 — cutoff보다 오래된 pending을 denied로.
pub fn expire_pending_approvals(
    conn: &Connection,
    older_than_epoch_secs: i64,
    resolved_at: i64,
) -> anyhow::Result<usize> {
    let affected = conn
        .execute(
            "UPDATE pending_approvals
             SET status = 'denied', resolved_at = ?2
             WHERE status = 'pending' AND created_at < ?1",
            (older_than_epoch_secs, resolved_at),
        )
        .context("orphan pending 승인 만료 실패")?;
    Ok(affected)
}

/// 오래된 resolved approval 행을 삭제한다. pending 행은 live IPC 상태라 건드리지 않는다.
pub fn prune_resolved_approvals(
    conn: &Connection,
    resolved_before_epoch_secs: i64,
) -> anyhow::Result<usize> {
    let affected = conn
        .execute(
            "DELETE FROM pending_approvals
             WHERE status != 'pending' AND resolved_at IS NOT NULL AND resolved_at < ?1",
            [resolved_before_epoch_secs],
        )
        .context("resolved approval 정리 실패")?;
    Ok(affected)
}

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
    /// env_json 컬럼에 JSON object로 저장. secret-like key/value는 저장 전 거부한다.
    pub env_plain: Vec<(String, String)>,
    /// env_credentials_json 컬럼에 JSON object로 저장. 값은 credential id만 저장한다.
    pub env_secrets: Vec<(String, String)>,
    /// true면 parent env 상속, false면 scoped env만 주입한다.
    pub inherit_env: bool,
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

/// Lightweight overview row. Transport arguments/environment are loaded only for a selected
/// server, preventing overview snapshots from retaining execution configuration.
#[derive(Clone, PartialEq, Eq)]
pub struct McpServerInventoryRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub url: Option<String>,
    pub enabled: bool,
    pub tool_count: usize,
}

impl std::fmt::Debug for McpServerInventoryRow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpServerInventoryRow")
            .field("identity", &"REDACTED")
            .field("kind", &self.kind)
            .field("url", &self.url.as_ref().map(|_| "REDACTED"))
            .field("enabled", &self.enabled)
            .field("tool_count", &self.tool_count)
            .finish()
    }
}

/// One bounded tool summary with the persisted permission, when present. Schema/trust fields are
/// deliberately absent: live schema is loaded only on invoke.
#[derive(Clone, PartialEq)]
pub struct McpToolPageRow {
    pub id: String,
    pub server_id: String,
    pub name: String,
    pub description: Option<String>,
    pub permission: Option<PermissionRuleRow>,
}

impl std::fmt::Debug for McpToolPageRow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpToolPageRow")
            .field("identity", &"REDACTED")
            .field(
                "description",
                &self.description.as_ref().map(|_| "REDACTED"),
            )
            .field("has_permission", &self.permission.is_some())
            .finish()
    }
}

/// Same-read-transaction tool count and one ordered page. `rows` never exceeds
/// [`MCP_TOOL_PAGE_LIMIT_MAX`].
#[derive(Clone, PartialEq)]
pub struct McpToolPage {
    pub total: usize,
    pub rows: Vec<McpToolPageRow>,
}

impl std::fmt::Debug for McpToolPage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpToolPage")
            .field("total", &self.total)
            .field("row_count", &self.rows.len())
            .finish()
    }
}

/// Repository-level ceiling. Callers may choose a lower page size but cannot raise this at
/// runtime, preventing accidental full-list allocation in UI snapshot preparation.
pub const MCP_TOOL_PAGE_LIMIT_MAX: usize = 256;

/// Fixed storage-side allocation ceilings. Connector runtime limits may lower these values but
/// cannot raise them. SQL byte preflights run before text values are materialized.
pub const MCP_TOOL_PAGE_BYTES_MAX: usize = 8 * 1024 * 1024;

/// Exact invoke-name lookup ceiling. Tool descriptors may collectively use the larger page budget,
/// but a single protocol method name never needs a multi-megabyte allocation.
pub const MCP_TOOL_NAME_BYTES_MAX: usize = 4 * 1024;

/// Connector overview inventory ceiling. The +1 probe detects overflow without materializing the
/// complete legacy server table.
pub const MCP_SERVER_INVENTORY_LIMIT_MAX: usize = 256;
pub const MCP_SERVER_INVENTORY_BYTES_MAX: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpServerSaveOutcome {
    Inserted,
    Updated { transport_changed: bool },
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecretLikeArgsReason {
    ArgFlag,
    BearerToken,
    DatabaseUrl,
    TokenLiteral,
}

impl SecretLikeArgsReason {
    fn label(self) -> &'static str {
        match self {
            SecretLikeArgsReason::ArgFlag => "secret-like command argument flag",
            SecretLikeArgsReason::BearerToken => "bearer token payload",
            SecretLikeArgsReason::DatabaseUrl => "database URL payload",
            SecretLikeArgsReason::TokenLiteral => "token-like payload",
        }
    }
}

pub fn validate_server_args_for_persistence(args: &[String]) -> anyhow::Result<()> {
    if let Some(reason) = secret_like_args_reason(args) {
        anyhow::bail!(
            "MCP server args에 {}가 포함되어 저장을 거부합니다. secret은 credential/env binding으로 저장하세요",
            reason.label()
        );
    }
    Ok(())
}

pub fn validate_server_env_for_persistence(
    env_plain: &[(String, String)],
    env_secrets: &[(String, String)],
) -> anyhow::Result<()> {
    let mut seen = std::collections::HashSet::new();
    for (key, value) in env_plain {
        validate_env_key(key)?;
        if !seen.insert(key.clone()) {
            anyhow::bail!("MCP env key 중복: {key}");
        }
        if secret_like_arg_key(key) || secret_like_value(value).is_some() {
            anyhow::bail!(
                "MCP env '{}'는 secret-like plain value로 저장할 수 없습니다. credential binding을 사용하세요",
                key
            );
        }
    }
    for (key, credential_id) in env_secrets {
        validate_env_key(key)?;
        if !seen.insert(key.clone()) {
            anyhow::bail!("MCP env key 중복: {key}");
        }
        if credential_id.trim().is_empty() || credential_id.contains('\0') {
            anyhow::bail!(
                "MCP env '{}' credential_id가 비어있거나 유효하지 않습니다",
                key
            );
        }
    }
    Ok(())
}

fn validate_env_key(key: &str) -> anyhow::Result<()> {
    if key.trim().is_empty() || key.contains('=') || key.contains('\0') {
        anyhow::bail!("MCP env key가 비어있거나 유효하지 않습니다");
    }
    Ok(())
}

fn secret_like_args_reason(args: &[String]) -> Option<SecretLikeArgsReason> {
    let joined = args.join(" ");
    if contains_bearer_payload(&joined) {
        return Some(SecretLikeArgsReason::BearerToken);
    }
    if contains_database_url_payload(&joined) {
        return Some(SecretLikeArgsReason::DatabaseUrl);
    }
    for arg in args {
        let trimmed = arg.trim();
        if secret_like_arg_flag(trimmed) {
            return Some(SecretLikeArgsReason::ArgFlag);
        }
        if let Some(reason) = secret_like_assignment(trimmed) {
            return Some(reason);
        }
        if let Some(reason) = secret_like_value(trimmed) {
            return Some(reason);
        }
    }
    None
}

fn secret_like_arg_flag(arg: &str) -> bool {
    const FLAGS: &[&str] = &[
        "--api-key",
        "--api_key",
        "--token",
        "--access-token",
        "--access_token",
        "--auth-token",
        "--auth_token",
        "--password",
        "--secret",
        "--client-secret",
        "--client_secret",
    ];
    let lower = arg.to_ascii_lowercase();
    FLAGS.iter().any(|flag| {
        lower == *flag
            || lower.starts_with(&format!("{flag}="))
            || lower.starts_with(&format!("{flag} "))
    })
}

fn secret_like_assignment(arg: &str) -> Option<SecretLikeArgsReason> {
    let (key, value) = arg.split_once('=')?;
    let key = key.trim().trim_start_matches('-');
    let value = value.trim();
    if key.is_empty() || value.is_empty() {
        return None;
    }
    if secret_like_arg_key(key) {
        let key = normalize_identifier(key);
        if key == "DATABASE_URL"
            || key == "DB_URL"
            || key.ends_with("_DATABASE_URL")
            || key.ends_with("_DB_URL")
        {
            return Some(SecretLikeArgsReason::DatabaseUrl);
        }
        return Some(SecretLikeArgsReason::ArgFlag);
    }
    secret_like_value(value)
}

fn secret_like_value(value: &str) -> Option<SecretLikeArgsReason> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if contains_bearer_payload(trimmed) {
        return Some(SecretLikeArgsReason::BearerToken);
    }
    if contains_database_url_payload(trimmed) || looks_like_database_url_with_password(trimmed) {
        return Some(SecretLikeArgsReason::DatabaseUrl);
    }
    if looks_like_token_literal(trimmed) {
        return Some(SecretLikeArgsReason::TokenLiteral);
    }
    for token in secret_like_tokens(trimmed) {
        if looks_like_database_url_with_password(token) {
            return Some(SecretLikeArgsReason::DatabaseUrl);
        }
        if looks_like_token_literal(token) {
            return Some(SecretLikeArgsReason::TokenLiteral);
        }
    }
    None
}

fn secret_like_tokens(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ';'))
        .filter(|token| !token.is_empty())
}

fn secret_like_arg_key(key: &str) -> bool {
    let key = normalize_identifier(key);
    matches!(
        key.as_str(),
        "API_KEY"
            | "TOKEN"
            | "ACCESS_TOKEN"
            | "REFRESH_TOKEN"
            | "AUTH_TOKEN"
            | "AUTHORIZATION"
            | "DATABASE_URL"
            | "DB_URL"
            | "PASSWORD"
            | "PASSWD"
            | "SECRET"
            | "SECRET_KEY"
            | "CLIENT_SECRET"
            | "PRIVATE_KEY"
    ) || key.ends_with("_API_KEY")
        || key.ends_with("_TOKEN")
        || key.ends_with("_AUTHORIZATION")
        || key.ends_with("_DATABASE_URL")
        || key.ends_with("_DB_URL")
        || key.ends_with("_PASSWORD")
        || key.ends_with("_PASSWD")
        || key.ends_with("_SECRET")
        || key.ends_with("_SECRET_KEY")
        || key.ends_with("_CLIENT_SECRET")
        || key.ends_with("_PRIVATE_KEY")
}

fn normalize_identifier(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|c| {
            if c == '-' {
                '_'
            } else {
                c.to_ascii_uppercase()
            }
        })
        .collect()
}

fn contains_bearer_payload(value: &str) -> bool {
    value.to_ascii_lowercase().contains("bearer ")
}

fn contains_database_url_payload(value: &str) -> bool {
    value.to_ascii_uppercase().contains("DATABASE_URL=")
}

fn looks_like_database_url_with_password(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    ["postgres://", "postgresql://", "mysql://", "mariadb://"]
        .iter()
        .any(|scheme| {
            lower.starts_with(scheme)
                && value.contains('@')
                && value
                    .split_once("://")
                    .and_then(|(_, rest)| rest.split('@').next())
                    .is_some_and(|userinfo| userinfo.contains(':'))
        })
}

fn looks_like_token_literal(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let len = value.chars().count();
    len >= 12
        && (lower.starts_with("sk-")
            || lower.starts_with("ghp_")
            || lower.starts_with("github_pat_")
            || lower.starts_with("xoxb-")
            || lower.starts_with("xoxp-"))
}

pub fn insert_server(conn: &Connection, row: &McpServerRow) -> anyhow::Result<()> {
    validate_server_args_for_persistence(&row.args)
        .with_context(|| format!("mcp_server args validation 실패: {}", row.name))?;
    validate_server_env_for_persistence(&row.env_plain, &row.env_secrets)
        .with_context(|| format!("mcp_server env validation 실패: {}", row.name))?;
    let args_json = serde_json::to_string(&row.args)?;
    let env_json = env_pairs_json(&row.env_plain)?;
    let env_credentials_json = env_pairs_json(&row.env_secrets)?;
    conn.execute(
        "INSERT INTO mcp_servers
           (id, name, kind, command, args_json, env_json, env_credentials_json,
            inherit_env, url, enabled, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
            strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
        (
            &row.id,
            &row.name,
            &row.kind,
            &row.command,
            &args_json,
            &env_json,
            &env_credentials_json,
            row.inherit_env,
            &row.url,
            row.enabled,
        ),
    )
    .with_context(|| format!("mcp_server 저장 실패: {}", row.name))?;
    Ok(())
}

/// Insert or fully update one server in an IMMEDIATE transaction. A transport change invalidates
/// cached tools and Allow grants atomically, while Deny grants remain a safe restriction. Display
/// name/enabled-only edits preserve transport-derived state, and semantic no-ops perform no write.
pub fn save_server(
    conn: &mut Connection,
    row: &McpServerRow,
) -> anyhow::Result<McpServerSaveOutcome> {
    validate_server_args_for_persistence(&row.args)
        .with_context(|| format!("mcp_server args validation 실패: {}", row.name))?;
    validate_server_env_for_persistence(&row.env_plain, &row.env_secrets)
        .with_context(|| format!("mcp_server env validation 실패: {}", row.name))?;
    let args_json = serde_json::to_string(&row.args)?;
    let env_json = env_pairs_json(&row.env_plain)?;
    let env_credentials_json = env_pairs_json(&row.env_secrets)?;

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let Some(current) = server(&tx, &row.id)? else {
        insert_server(&tx, row)?;
        tx.commit().context("MCP server insert commit 실패")?;
        return Ok(McpServerSaveOutcome::Inserted);
    };

    let current_env_json = env_pairs_json(&current.env_plain)?;
    let current_env_credentials_json = env_pairs_json(&current.env_secrets)?;
    let transport_changed = current.kind != row.kind
        || current.command != row.command
        || current.args != row.args
        || current_env_json != env_json
        || current_env_credentials_json != env_credentials_json
        || current.inherit_env != row.inherit_env
        || current.url != row.url;
    let unchanged =
        !transport_changed && current.name == row.name && current.enabled == row.enabled;
    if unchanged {
        tx.commit().context("MCP server no-op transaction 실패")?;
        return Ok(McpServerSaveOutcome::Unchanged);
    }

    let affected = tx
        .execute(
            "UPDATE mcp_servers
             SET name = ?2, kind = ?3, command = ?4, args_json = ?5,
                 env_json = ?6, env_credentials_json = ?7, inherit_env = ?8,
                 url = ?9, enabled = ?10,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE id = ?1",
            (
                &row.id,
                &row.name,
                &row.kind,
                &row.command,
                &args_json,
                &env_json,
                &env_credentials_json,
                row.inherit_env,
                &row.url,
                row.enabled,
            ),
        )
        .with_context(|| format!("mcp_server 전체 갱신 실패: {}", row.id))?;
    anyhow::ensure!(affected == 1, "mcp_server 전체 갱신 대상 없음: {}", row.id);
    if transport_changed {
        tx.execute(
            "DELETE FROM tool_permission_rules WHERE server_id = ?1 AND rule = 'allow'",
            [&row.id],
        )
        .with_context(|| format!("mcp_server Allow 권한 초기화 실패: {}", row.id))?;
        tx.execute("DELETE FROM mcp_tools WHERE server_id = ?1", [&row.id])
            .with_context(|| format!("mcp_server tool cache 초기화 실패: {}", row.id))?;
    }
    tx.commit().context("MCP server 전체 갱신 commit 실패")?;
    Ok(McpServerSaveOutcome::Updated { transport_changed })
}

/// http 서버의 URL과 그 URL에 묶인 신뢰 상태를 한 transaction으로 갱신한다.
/// 허용 규칙은 Ask(행 없음)로 되돌리고 tool cache를 비운다. Deny 규칙은 새 endpoint에도
/// 권한을 넓히지 않는 안전한 제약이므로 보존한다. 같은 URL이면 아무 것도 지우지 않는다.
pub fn update_server_url(conn: &Connection, server_id: &str, url: &str) -> anyhow::Result<()> {
    let tx = conn.unchecked_transaction()?;
    let old_url: Option<Option<String>> = tx
        .query_row(
            "SELECT url FROM mcp_servers WHERE id = ?1",
            [server_id],
            |row| row.get(0),
        )
        .optional()?;
    let old_url = old_url.with_context(|| format!("mcp_server url 갱신 대상 없음: {server_id}"))?;
    if old_url.as_deref() == Some(url) {
        tx.commit()?;
        return Ok(());
    }
    let affected = tx
        .execute(
            "UPDATE mcp_servers
             SET url = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE id = ?1",
            (server_id, url),
        )
        .with_context(|| format!("mcp_server url 갱신 실패: {server_id}"))?;
    anyhow::ensure!(affected == 1, "mcp_server url 갱신 대상 없음: {server_id}");
    tx.execute(
        "DELETE FROM tool_permission_rules WHERE server_id = ?1 AND rule = 'allow'",
        [server_id],
    )
    .with_context(|| format!("mcp_server Allow 권한 초기화 실패: {server_id}"))?;
    tx.execute("DELETE FROM mcp_tools WHERE server_id = ?1", [server_id])
        .with_context(|| format!("mcp_server tool cache 초기화 실패: {server_id}"))?;
    tx.commit().context("mcp_server URL/신뢰상태 commit 실패")
}

/// 같은 canonical URL(`trim` + trailing slash 제거)의 서버를 멱등 등록한다. Slack 같은
/// built-in provider가 render/UI cache 경쟁과 무관하게 하나의 durable row를 얻는 경로다.
/// IMMEDIATE transaction으로 read-then-insert를 직렬화한다.
pub fn ensure_server_by_url(
    conn: &mut Connection,
    row: &McpServerRow,
) -> anyhow::Result<McpServerRow> {
    let url = row
        .url
        .as_deref()
        .context("멱등 URL 등록에는 URL이 필요합니다")?;
    validate_server_args_for_persistence(&row.args)?;
    validate_server_env_for_persistence(&row.env_plain, &row.env_secrets)?;
    let canonical = canonical_url(url);
    anyhow::ensure!(!canonical.is_empty(), "멱등 등록 URL이 비어 있습니다");

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let existing = tx
        .query_row(
            "SELECT id, name, kind, command, args_json, env_json, env_credentials_json,
                    inherit_env, url, enabled
             FROM mcp_servers
             WHERE rtrim(trim(url), '/') = ?1
             ORDER BY created_at, id
             LIMIT 1",
            [canonical],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, bool>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, bool>(9)?,
                ))
            },
        )
        .optional()?
        .map(parse_server_row)
        .transpose()?;
    if let Some(existing) = existing {
        tx.commit()?;
        return Ok(existing);
    }
    insert_server(&tx, row)?;
    tx.commit().context("MCP 서버 멱등 등록 commit 실패")?;
    Ok(row.clone())
}

/// import에서 선택된 서버 전체를 all-or-nothing으로 저장한다. 모든 행의 secret-like
/// validation을 transaction 전에 끝내며, ID 충돌/trigger/commit 실패 시 일부 서버가
/// 남지 않는다.
pub fn insert_servers_batch(conn: &mut Connection, rows: &[McpServerRow]) -> anyhow::Result<usize> {
    for row in rows {
        validate_server_args_for_persistence(&row.args)
            .with_context(|| format!("mcp_server args validation 실패: {}", row.name))?;
        validate_server_env_for_persistence(&row.env_plain, &row.env_secrets)
            .with_context(|| format!("mcp_server env validation 실패: {}", row.name))?;
    }
    let tx = conn.transaction()?;
    for row in rows {
        insert_server(&tx, row)?;
    }
    tx.commit().context("MCP server import batch commit 실패")?;
    Ok(rows.len())
}

fn canonical_url(url: &str) -> &str {
    url.trim().trim_end_matches('/')
}

pub fn list_servers(conn: &Connection) -> anyhow::Result<Vec<McpServerRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, command, args_json, env_json, env_credentials_json,
                inherit_env, url, enabled
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
            row.get::<_, Option<String>>(6)?,
            row.get::<_, bool>(7)?,
            row.get::<_, Option<String>>(8)?,
            row.get::<_, bool>(9)?,
        ))
    })?;
    let mut servers = Vec::new();
    for row in rows {
        servers.push(parse_server_row(row?)?);
    }
    Ok(servers)
}

/// Connector-facing bounded inventory. Reads at most `limit + 1` rows and fails closed when the
/// requested snapshot cannot represent the complete inventory. The legacy unbounded list remains
/// available only for callers that have not yet cut over.
pub fn server_inventory(
    conn: &Connection,
    limit: usize,
) -> anyhow::Result<Vec<McpServerInventoryRow>> {
    anyhow::ensure!(limit > 0, "MCP server inventory limit은 0보다 커야 합니다");
    anyhow::ensure!(
        limit <= MCP_SERVER_INVENTORY_LIMIT_MAX,
        "MCP server inventory limit이 상한을 초과했습니다"
    );
    let probe = limit
        .checked_add(1)
        .context("MCP server inventory +1 overflow")?;
    let sql_limit = i64::try_from(probe).context("MCP server inventory LIMIT 변환 실패")?;
    let tx = conn.unchecked_transaction()?;
    let (probe_count, inventory_bytes): (i64, i64) = tx.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(
                    length(CAST(id AS BLOB)) + length(CAST(name AS BLOB)) +
                    length(CAST(kind AS BLOB)) + length(CAST(COALESCE(url, '') AS BLOB))
                ), 0)
         FROM (
             SELECT id, name, kind, url
             FROM mcp_servers ORDER BY created_at, id LIMIT ?1
         )",
        [sql_limit],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let probe_count = usize::try_from(probe_count).context("MCP server count 변환 실패")?;
    let inventory_bytes =
        usize::try_from(inventory_bytes).context("MCP server inventory bytes 변환 실패")?;
    anyhow::ensure!(
        probe_count <= limit,
        "MCP server inventory가 상한 {limit}개를 초과했습니다"
    );
    anyhow::ensure!(
        inventory_bytes <= MCP_SERVER_INVENTORY_BYTES_MAX,
        "MCP server inventory byte 상한을 초과했습니다"
    );
    let servers = {
        let mut stmt = tx.prepare(
            "SELECT s.id, s.name, s.kind, s.url, s.enabled, COUNT(t.id)
             FROM mcp_servers s
             LEFT JOIN mcp_tools t ON t.server_id = s.id
             GROUP BY s.id
             ORDER BY s.created_at, s.id
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([sql_limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, bool>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;
        let mut servers = Vec::with_capacity(probe_count);
        for row in rows {
            let (id, name, kind, url, enabled, tool_count) = row?;
            servers.push(McpServerInventoryRow {
                id,
                name,
                kind,
                url,
                enabled,
                tool_count: usize::try_from(tool_count)
                    .context("MCP server tool count 변환 실패")?,
            });
        }
        servers
    };
    tx.commit()
        .context("MCP server inventory read transaction 실패")?;
    Ok(servers)
}

/// Bounded point lookup for live proxy target refresh. Missing rows are fail-closed upstream.
pub fn server(conn: &Connection, server_id: &str) -> anyhow::Result<Option<McpServerRow>> {
    let row = conn
        .query_row(
            "SELECT id, name, kind, command, args_json, env_json, env_credentials_json,
                    inherit_env, url, enabled
             FROM mcp_servers WHERE id = ?1",
            [server_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, bool>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, bool>(9)?,
                ))
            },
        )
        .optional()?;
    row.map(parse_server_row).transpose()
}

type RawServerRow = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    bool,
    Option<String>,
    bool,
);

fn parse_server_row(row: RawServerRow) -> anyhow::Result<McpServerRow> {
    let (
        id,
        name,
        kind,
        command,
        args_json,
        env_json,
        env_credentials_json,
        inherit_env,
        url,
        enabled,
    ) = row;
    let args = match args_json {
        Some(json) => {
            serde_json::from_str(&json).with_context(|| format!("args_json 파싱 실패: {id}"))?
        }
        None => Vec::new(),
    };
    let env_plain = parse_env_pairs(&id, "env_json", env_json)?;
    let env_secrets = parse_env_pairs(&id, "env_credentials_json", env_credentials_json)?;
    Ok(McpServerRow {
        id,
        name,
        kind,
        command,
        args,
        env_plain,
        env_secrets,
        inherit_env,
        url,
        enabled,
    })
}

fn env_pairs_json(pairs: &[(String, String)]) -> anyhow::Result<Option<String>> {
    if pairs.is_empty() {
        return Ok(None);
    }
    let map: serde_json::Map<String, serde_json::Value> = pairs
        .iter()
        .map(|(key, value)| (key.clone(), serde_json::Value::String(value.clone())))
        .collect();
    Ok(Some(serde_json::to_string(&map)?))
}

fn parse_env_pairs(
    row_id: &str,
    column: &str,
    json: Option<String>,
) -> anyhow::Result<Vec<(String, String)>> {
    let Some(json) = json else {
        return Ok(Vec::new());
    };
    let map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&json).with_context(|| format!("{column} 파싱 실패: {row_id}"))?;
    let mut pairs = Vec::with_capacity(map.len());
    for (key, value) in map {
        let Some(value) = value.as_str() else {
            anyhow::bail!("{column} value는 문자열이어야 합니다: {row_id}:{key}");
        };
        pairs.push((key, value.to_owned()));
    }
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(pairs)
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

/// 서버의 tool 목록을 새 발견 결과로 원자적으로 교체한다 (재연결 시 중복 방지).
/// rows의 server_id는 호출측이 일치시켜 넘긴다.
pub fn replace_tools_for_server(
    conn: &mut Connection,
    server_id: &str,
    rows: &[McpToolRow],
) -> anyhow::Result<()> {
    for row in rows {
        anyhow::ensure!(
            row.server_id == server_id,
            "mcp_tool server_id 불일치: {} != {server_id}",
            row.server_id
        );
    }
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM mcp_tools WHERE server_id = ?1", [server_id])
        .with_context(|| format!("mcp_tools 삭제 실패: {server_id}"))?;
    for row in rows {
        insert_tool(&tx, row)?;
    }
    tx.commit().context("mcp_tools 교체 commit 실패")
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

/// Exact `(server_id, tool_id)` lookup used immediately before live-schema authorization. The
/// name byte length is proven in SQL before the String is materialized, and both reads share one
/// snapshot so replacement cannot race the preflight. Missing or wrong-server IDs return None;
/// empty and oversized persisted names fail closed.
pub fn tool_name(
    conn: &Connection,
    server_id: &str,
    tool_id: &str,
) -> anyhow::Result<Option<String>> {
    let tx = conn.unchecked_transaction()?;
    let name_bytes = tx
        .query_row(
            "SELECT length(CAST(name AS BLOB))
             FROM mcp_tools WHERE server_id = ?1 AND id = ?2",
            (server_id, tool_id),
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    let Some(name_bytes) = name_bytes else {
        tx.commit()
            .context("MCP tool name missing lookup transaction 실패")?;
        return Ok(None);
    };
    let name_bytes = usize::try_from(name_bytes).context("MCP tool name bytes 변환 실패")?;
    anyhow::ensure!(name_bytes > 0, "MCP tool name이 비어 있습니다");
    anyhow::ensure!(
        name_bytes <= MCP_TOOL_NAME_BYTES_MAX,
        "MCP tool name byte 상한을 초과했습니다"
    );
    let name: String = tx.query_row(
        "SELECT name FROM mcp_tools WHERE server_id = ?1 AND id = ?2",
        (server_id, tool_id),
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        name.len() == name_bytes,
        "MCP tool name same-snapshot byte 길이 불일치"
    );
    tx.commit()
        .context("MCP tool name lookup transaction 실패")?;
    Ok(Some(name))
}

/// Count and read one ordered tool page inside the same read transaction. Permission is joined in
/// the page query, avoiding both an all-tools allocation and per-row permission lookups.
pub fn tool_page(
    conn: &Connection,
    server_id: &str,
    offset: usize,
    limit: usize,
) -> anyhow::Result<McpToolPage> {
    anyhow::ensure!(limit > 0, "MCP tool page limit은 0보다 커야 합니다");
    anyhow::ensure!(
        limit <= MCP_TOOL_PAGE_LIMIT_MAX,
        "MCP tool page limit이 상한을 초과했습니다"
    );
    let sql_offset = i64::try_from(offset).context("MCP tool page OFFSET 변환 실패")?;
    let sql_limit = i64::try_from(limit).context("MCP tool page LIMIT 변환 실패")?;
    let tx = conn.unchecked_transaction()?;
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM mcp_tools WHERE server_id = ?1",
        [server_id],
        |row| row.get(0),
    )?;
    let total = usize::try_from(count).context("MCP tool count 변환 실패")?;
    let page_bytes: i64 = tx.query_row(
        "SELECT COALESCE(SUM(
                    length(CAST(t.id AS BLOB)) + length(CAST(t.server_id AS BLOB)) +
                    length(CAST(t.name AS BLOB)) +
                    length(CAST(COALESCE(t.description, '') AS BLOB)) +
                    length(CAST(COALESCE(p.server_id, '') AS BLOB)) +
                    length(CAST(COALESCE(p.tool_name, '') AS BLOB)) +
                    length(CAST(COALESCE(p.rule, '') AS BLOB)) +
                    length(CAST(COALESCE(p.approved_schema_hash, '') AS BLOB))
                ), 0)
         FROM (
             SELECT id, server_id, name, description
             FROM mcp_tools
             WHERE server_id = ?1
             ORDER BY name, id
             LIMIT ?2 OFFSET ?3
         ) t
         LEFT JOIN tool_permission_rules p
           ON p.server_id = t.server_id AND p.tool_name = t.name",
        (server_id, sql_limit, sql_offset),
        |row| row.get(0),
    )?;
    let page_bytes = usize::try_from(page_bytes).context("MCP tool page bytes 변환 실패")?;
    anyhow::ensure!(
        page_bytes <= MCP_TOOL_PAGE_BYTES_MAX,
        "MCP tool page byte 상한을 초과했습니다"
    );
    let rows = {
        let mut stmt = tx.prepare(
            "SELECT t.id, t.server_id, t.name, t.description,
                    p.server_id, p.tool_name, p.rule, p.approved_schema_hash
             FROM mcp_tools t
             LEFT JOIN tool_permission_rules p
               ON p.server_id = t.server_id AND p.tool_name = t.name
             WHERE t.server_id = ?1
             ORDER BY t.name, t.id
             LIMIT ?2 OFFSET ?3",
        )?;
        let mapped = stmt.query_map((server_id, sql_limit, sql_offset), |row| {
            let permission_server_id = row.get::<_, Option<String>>(4)?;
            let permission = if let Some(server_id) = permission_server_id {
                Some(PermissionRuleRow {
                    server_id,
                    tool_name: row.get(5)?,
                    rule: row.get(6)?,
                    approved_schema_hash: row.get(7)?,
                })
            } else {
                None
            };
            Ok(McpToolPageRow {
                id: row.get(0)?,
                server_id: row.get(1)?,
                name: row.get(2)?,
                description: row.get(3)?,
                permission,
            })
        })?;
        mapped.collect::<Result<Vec<_>, _>>()?
    };
    tx.commit().context("MCP tool page read transaction 실패")?;
    Ok(McpToolPage { total, rows })
}

/// Delete one server and all live execution metadata from a caller-owned transaction. Durable
/// audit history is intentionally outside this crate and is never deleted. The storage facade must
/// first reject active agent-proxy references in the same transaction.
pub fn delete_server_in_transaction(
    conn: &Connection,
    server_id: &str,
    resolved_at: i64,
) -> anyhow::Result<bool> {
    anyhow::ensure!(
        !conn.is_autocommit(),
        "MCP server 삭제는 caller-owned transaction이 필요합니다"
    );
    let exists = conn
        .query_row(
            "SELECT 1 FROM mcp_servers WHERE id = ?1",
            [server_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !exists {
        return Ok(false);
    }
    conn.execute(
        "UPDATE pending_approvals
         SET status = 'denied', remember = 0, resolved_at = ?2
         WHERE server_id = ?1 AND status = 'pending'",
        (server_id, resolved_at),
    )
    .with_context(|| format!("MCP server pending approval 종료 실패: {server_id}"))?;
    conn.execute(
        "DELETE FROM tool_permission_rules WHERE server_id = ?1",
        [server_id],
    )
    .with_context(|| format!("MCP server permission 삭제 실패: {server_id}"))?;
    conn.execute("DELETE FROM mcp_tools WHERE server_id = ?1", [server_id])
        .with_context(|| format!("MCP server tool 삭제 실패: {server_id}"))?;
    let deleted = conn
        .execute("DELETE FROM mcp_servers WHERE id = ?1", [server_id])
        .with_context(|| format!("MCP server 삭제 실패: {server_id}"))?;
    anyhow::ensure!(deleted == 1, "MCP server 삭제 대상 수 불일치: {server_id}");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MIGRATION_SERVER_ENV, MIGRATION_SQL};

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // 앱은 모든 연결에 foreign_keys=ON을 강제한다 (§11.9) — 테스트도 동일 조건
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(MIGRATION_SQL).unwrap();
        conn.execute_batch(MIGRATION_SERVER_ENV).unwrap();
        conn.execute_batch(MIGRATION_TOOL_PERMISSION_RULES).unwrap();
        conn.execute_batch(MIGRATION_PENDING_APPROVALS).unwrap();
        conn.execute_batch(MIGRATION_APPROVAL_PANE).unwrap();
        // list_pending_approvals의 LEFT JOIN 대상 (I2) — persist가 소유하는 테이블이지만
        // 이 crate 테스트는 격리되므로 조인이 성립하도록 최소 컬럼만 만든다.
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT);
             CREATE TABLE mux_panes (id TEXT PRIMARY KEY, session_id TEXT);",
        )
        .unwrap();
        conn
    }

    fn sample_server() -> McpServerRow {
        McpServerRow {
            id: "srv-1".to_owned(),
            name: "filesystem".to_owned(),
            kind: "stdio".to_owned(),
            command: Some("npx".to_owned()),
            args: vec!["-y".to_owned(), "server-filesystem".to_owned()],
            env_plain: Vec::new(),
            env_secrets: Vec::new(),
            inherit_env: true,
            url: None,
            enabled: true,
        }
    }

    fn sample_tool(id: &str, name: &str) -> McpToolRow {
        McpToolRow {
            id: id.to_owned(),
            server_id: "srv-1".to_owned(),
            name: name.to_owned(),
            description: Some(format!("{name} description")),
            input_schema_json: Some(r#"{"type":"object"}"#.to_owned()),
            trust_level: "unknown".to_owned(),
            schema_hash: Some(format!("hash-{name}")),
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
    fn full_server_save는_noop과_display_edit를_구분하고_transport_change만_신뢰를_초기화한다() {
        let mut conn = test_conn();
        let original = sample_server();
        assert_eq!(
            save_server(&mut conn, &original).unwrap(),
            McpServerSaveOutcome::Inserted
        );
        upsert_permission_rule(&conn, "srv-1", "read", "allow", Some("hash-read")).unwrap();
        upsert_permission_rule(&conn, "srv-1", "delete", "deny", None).unwrap();
        insert_tool(&conn, &sample_tool("tool-read", "read")).unwrap();

        assert_eq!(
            save_server(&mut conn, &original).unwrap(),
            McpServerSaveOutcome::Unchanged
        );
        assert_eq!(list_permission_rules(&conn).unwrap().len(), 2);
        assert_eq!(list_tools_for_server(&conn, "srv-1").unwrap().len(), 1);

        let mut display_edit = original.clone();
        display_edit.name = "renamed".to_owned();
        display_edit.enabled = false;
        assert_eq!(
            save_server(&mut conn, &display_edit).unwrap(),
            McpServerSaveOutcome::Updated {
                transport_changed: false
            }
        );
        assert_eq!(list_permission_rules(&conn).unwrap().len(), 2);
        assert_eq!(list_tools_for_server(&conn, "srv-1").unwrap().len(), 1);

        let mut transport_edit = display_edit.clone();
        transport_edit.command = Some("new-command".to_owned());
        transport_edit.args = vec!["--safe".to_owned()];
        transport_edit.env_plain = vec![
            ("Z_SAFE".to_owned(), "2".to_owned()),
            ("A_SAFE".to_owned(), "1".to_owned()),
        ];
        assert_eq!(
            save_server(&mut conn, &transport_edit).unwrap(),
            McpServerSaveOutcome::Updated {
                transport_changed: true
            }
        );
        let mut persisted = transport_edit.clone();
        persisted.env_plain.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(server(&conn, "srv-1").unwrap(), Some(persisted));
        assert_eq!(
            save_server(&mut conn, &transport_edit).unwrap(),
            McpServerSaveOutcome::Unchanged,
            "canonical env ordering must not produce a false transport change"
        );
        assert!(list_tools_for_server(&conn, "srv-1").unwrap().is_empty());
        assert_eq!(
            list_permission_rules(&conn).unwrap(),
            vec![PermissionRuleRow {
                server_id: "srv-1".to_owned(),
                tool_name: "delete".to_owned(),
                rule: "deny".to_owned(),
                approved_schema_hash: None,
            }]
        );
    }

    #[test]
    fn full_server_save_중간실패는_row와_permission과_tools를_모두_rollback한다() {
        let mut conn = test_conn();
        let original = sample_server();
        insert_server(&conn, &original).unwrap();
        upsert_permission_rule(&conn, "srv-1", "read", "allow", Some("hash")).unwrap();
        let tool = sample_tool("tool-read", "read");
        insert_tool(&conn, &tool).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_full_save_tool_clear BEFORE DELETE ON mcp_tools
             BEGIN SELECT RAISE(ABORT, 'injected full save failure'); END;",
        )
        .unwrap();
        let mut changed = original.clone();
        changed.command = Some("changed".to_owned());

        assert!(save_server(&mut conn, &changed).is_err());
        assert_eq!(server(&conn, "srv-1").unwrap(), Some(original));
        assert_eq!(list_permission_rules(&conn).unwrap().len(), 1);
        assert_eq!(list_tools_for_server(&conn, "srv-1").unwrap(), vec![tool]);
    }

    #[test]
    fn full_server_save는_validation_실패시_기존_row를_수정하지_않는다() {
        let mut conn = test_conn();
        let original = sample_server();
        insert_server(&conn, &original).unwrap();
        let mut invalid = original.clone();
        invalid.args = vec!["--token".to_owned(), "sk-never-persist-this".to_owned()];

        assert!(save_server(&mut conn, &invalid).is_err());
        assert_eq!(server(&conn, "srv-1").unwrap(), Some(original));
    }

    #[test]
    fn server_inventory는_exact_limit을_허용하고_plus_one을_감지한다() {
        let mut conn = test_conn();
        let mut first = sample_server();
        first.id = "srv-a".to_owned();
        let mut second = sample_server();
        second.id = "srv-b".to_owned();
        save_server(&mut conn, &first).unwrap();
        save_server(&mut conn, &second).unwrap();
        let mut tool = sample_tool("tool-a", "read");
        tool.server_id = "srv-a".to_owned();
        insert_tool(&conn, &tool).unwrap();

        let inventory = server_inventory(&conn, 2).unwrap();
        assert_eq!(inventory.len(), 2);
        assert_eq!(inventory[0].id, "srv-a");
        assert_eq!(inventory[0].tool_count, 1);
        assert_eq!(inventory[1].tool_count, 0);
        assert!(!format!("{:?}", inventory[0]).contains("srv-a"));
        assert!(server_inventory(&conn, 1).is_err());
        assert!(server_inventory(&conn, 0).is_err());
        assert!(server_inventory(&conn, MCP_SERVER_INVENTORY_LIMIT_MAX + 1).is_err());
    }

    #[test]
    fn server_inventory는_sql_byte_preflight로_oversized_row를_거부한다() {
        let conn = test_conn();
        let mut server = sample_server();
        server.name = "x".repeat(MCP_SERVER_INVENTORY_BYTES_MAX + 1);
        insert_server(&conn, &server).unwrap();

        assert!(server_inventory(&conn, 1).is_err());
    }

    #[test]
    fn server_url_갱신은_allow와_tools를_같이_초기화하고_deny는_보존한다() {
        let conn = test_conn();
        let mut server = sample_server();
        server.kind = "http".to_owned();
        server.command = None;
        server.args = Vec::new();
        server.url = Some("https://old.example.com/mcp".to_owned());
        insert_server(&conn, &server).unwrap();
        upsert_permission_rule(&conn, "srv-1", "read", "allow", Some("hash")).unwrap();
        upsert_permission_rule(&conn, "srv-1", "delete", "deny", None).unwrap();
        insert_tool(
            &conn,
            &McpToolRow {
                id: "tool-old".to_owned(),
                server_id: "srv-1".to_owned(),
                name: "read".to_owned(),
                description: None,
                input_schema_json: None,
                trust_level: "unknown".to_owned(),
                schema_hash: None,
            },
        )
        .unwrap();

        update_server_url(&conn, "srv-1", "https://new.example.com/mcp").unwrap();
        let rows = list_servers(&conn).unwrap();
        assert_eq!(rows[0].url.as_deref(), Some("https://new.example.com/mcp"));
        assert!(list_tools_for_server(&conn, "srv-1").unwrap().is_empty());
        let rules = list_permission_rules(&conn).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].tool_name, "delete");
        assert_eq!(rules[0].rule, "deny");

        // 없는 id는 에러 (조용한 no-op이면 신뢰 리셋 훅이 헛돈다)
        assert!(update_server_url(&conn, "no-such", "https://x.example.com").is_err());
    }

    #[test]
    fn server_url_transaction_중간실패는_url과_신뢰상태를_모두_rollback한다() {
        let conn = test_conn();
        let mut server = sample_server();
        server.kind = "http".to_owned();
        server.command = None;
        server.url = Some("https://old.example.com/mcp".to_owned());
        insert_server(&conn, &server).unwrap();
        upsert_permission_rule(&conn, "srv-1", "read", "allow", Some("hash")).unwrap();
        let tool = McpToolRow {
            id: "tool-old".to_owned(),
            server_id: "srv-1".to_owned(),
            name: "read".to_owned(),
            description: None,
            input_schema_json: None,
            trust_level: "unknown".to_owned(),
            schema_hash: None,
        };
        insert_tool(&conn, &tool).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_tool_clear BEFORE DELETE ON mcp_tools
             BEGIN SELECT RAISE(ABORT, 'injected tool clear failure'); END;",
        )
        .unwrap();

        assert!(update_server_url(&conn, "srv-1", "https://new.example.com/mcp").is_err());
        assert_eq!(
            list_servers(&conn).unwrap()[0].url.as_deref(),
            Some("https://old.example.com/mcp")
        );
        assert_eq!(list_permission_rules(&conn).unwrap().len(), 1);
        assert_eq!(list_tools_for_server(&conn, "srv-1").unwrap(), vec![tool]);
    }

    #[test]
    fn canonical_url_멱등등록은_기존_server를_재사용한다() {
        let mut conn = test_conn();
        let mut slack = sample_server();
        slack.kind = "http".to_owned();
        slack.command = None;
        slack.url = Some("https://mcp.slack.com/mcp/".to_owned());
        let first = ensure_server_by_url(&mut conn, &slack).unwrap();

        let mut duplicate = slack.clone();
        duplicate.id = "different-id".to_owned();
        duplicate.name = "renamed".to_owned();
        duplicate.url = Some(" https://mcp.slack.com/mcp ".to_owned());
        let second = ensure_server_by_url(&mut conn, &duplicate).unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(list_servers(&conn).unwrap().len(), 1);
    }

    #[test]
    fn canonical_url_멱등등록은_큰_inventory를_materialize하지_않고_point_lookup한다() {
        let mut conn = test_conn();
        for index in 0..=MCP_SERVER_INVENTORY_LIMIT_MAX {
            let mut server = sample_server();
            server.id = format!("local-{index:03}");
            server.name = server.id.clone();
            insert_server(&conn, &server).unwrap();
        }
        assert!(server_inventory(&conn, MCP_SERVER_INVENTORY_LIMIT_MAX).is_err());

        let mut slack = sample_server();
        slack.id = "slack".to_owned();
        slack.kind = "http".to_owned();
        slack.command = None;
        slack.args.clear();
        slack.url = Some("https://mcp.slack.com/mcp/".to_owned());
        insert_server(&conn, &slack).unwrap();
        let mut duplicate = slack.clone();
        duplicate.id = "slack-duplicate".to_owned();
        duplicate.url = Some(" https://mcp.slack.com/mcp ".to_owned());

        assert_eq!(
            ensure_server_by_url(&mut conn, &duplicate).unwrap().id,
            "slack"
        );
    }

    #[test]
    fn import_batch_중간실패는_앞선_insert도_rollback한다() {
        let mut conn = test_conn();
        let first = sample_server();
        let mut second = sample_server();
        second.id = "srv-2".to_owned();
        second.name = "trigger-failure".to_owned();
        conn.execute_batch(
            "CREATE TRIGGER fail_import BEFORE INSERT ON mcp_servers
             WHEN NEW.id = 'srv-2'
             BEGIN SELECT RAISE(ABORT, 'injected import failure'); END;",
        )
        .unwrap();

        assert!(insert_servers_batch(&mut conn, &[first, second]).is_err());
        assert!(list_servers(&conn).unwrap().is_empty());
    }

    #[test]
    fn server_env_plain과_credential_id는_roundtrip된다() {
        let conn = test_conn();
        let mut server = sample_server();
        server.env_plain = vec![("MCP_SAFE_MODE".to_owned(), "1".to_owned())];
        server.env_secrets = vec![("MCP_ACCESS_TOKEN".to_owned(), "cred-123".to_owned())];
        server.inherit_env = false;

        insert_server(&conn, &server).unwrap();

        assert_eq!(list_servers(&conn).unwrap(), vec![server]);
        let (env_json, env_credentials_json): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT env_json, env_credentials_json FROM mcp_servers WHERE id = 'srv-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(env_json.as_deref(), Some(r#"{"MCP_SAFE_MODE":"1"}"#));
        assert_eq!(
            env_credentials_json.as_deref(),
            Some(r#"{"MCP_ACCESS_TOKEN":"cred-123"}"#)
        );
    }

    #[test]
    fn server_env_plain_secret_like_value는_저장하지_않는다() {
        let conn = test_conn();
        let mut server = sample_server();
        let secret = "sk-mcp-env-secret-never-persisted";
        server.env_plain = vec![("MCP_TOKEN".to_owned(), secret.to_owned())];

        assert!(insert_server(&conn, &server).is_err());
        let count: i64 = conn
            .query_row("SELECT count(*) FROM mcp_servers", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn server_args_secret_like_payload는_args_json에_저장되지_않는다() {
        let conn = test_conn();
        let secret = "sk-mcp-plaintext-never-persisted";
        let mut server = sample_server();
        server.args = vec!["--token".to_owned(), secret.to_owned()];

        assert!(insert_server(&conn, &server).is_err());
        let rows: Vec<String> = conn
            .prepare("SELECT args_json FROM mcp_servers")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(rows.is_empty());
        assert!(rows.iter().all(|json| !json.contains(secret)));
    }

    #[test]
    fn server_args_database_url_assignment은_거부된다() {
        let conn = test_conn();
        let mut server = sample_server();
        server.args = vec![
            "DATABASE_URL=postgres://user:pass@localhost/app".to_owned(),
            "--safe".to_owned(),
        ];

        assert!(insert_server(&conn, &server).is_err());
        let count: i64 = conn
            .query_row("SELECT count(*) FROM mcp_servers", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn server_args_한줄_secret_flag와_database_url_flag를_거부한다() {
        let conn = test_conn();
        let cases = [
            "--api-key sk-mcp-inline-never-persisted",
            "--database-url=postgres://user:pass@localhost/app",
            "MCP_TOKEN=sk-mcp-assignment-never-persisted",
        ];

        for (idx, arg) in cases.iter().enumerate() {
            let mut server = sample_server();
            server.id = format!("srv-danger-{idx}");
            server.args = vec![arg.to_string()];
            assert!(insert_server(&conn, &server).is_err(), "{arg}");
        }

        let count: i64 = conn
            .query_row("SELECT count(*) FROM mcp_servers", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    /// 승인 행은 pane_id(=런타임 세션 키)를 **그대로** 돌려준다. 세션 해석은 소비처
    /// (app 인박스 / web-remote 대시보드)가 파싱해서 런타임 상태에서 하는 몫이다.
    ///
    /// 2026-07-17까지 여기서 `pane_id = mux_panes.id` 조인으로 세션 UUID/제목을 채웠는데,
    /// **프로덕션에서 매칭된 적이 없다** — pane_id는 `{workspace}:{u64}`이고 mux_panes.id는
    /// UUID다. 옛 테스트는 양쪽에 같은 가짜 문자열('pane-1')을 넣어 통과했을 뿐이라
    /// 버그를 몇 달간 가렸다. 그래서 이 테스트는 **실제 형식**을 쓴다.
    #[test]
    fn 승인은_pane_id를_그대로_반환하고_세션해석은_하지_않는다() {
        let conn = test_conn();
        // 실제 mux_panes 행이 있어도(=옛 조인의 상대) 결과에 영향을 주지 않아야 한다.
        conn.execute(
            "INSERT INTO sessions (id, title) VALUES ('sess-uuid-1', 'deppy-sijo')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mux_panes (id, session_id) VALUES ('130d9017-be25-469e-8f8d-984abacae701', 'sess-uuid-1')",
            [],
        )
        .unwrap();
        // proxy가 싣는 실제 값 형식: DEPPY_SESSION_ID = {workspace_id}:{session_id}
        let real_key = "315f68b6-333f-409f-a2c5-922b9eacfd7e:2";
        insert_pending_approval(
            &conn,
            "a1",
            "srv",
            "read",
            "prev",
            None,
            100,
            Some(real_key),
        )
        .unwrap();
        insert_pending_approval(&conn, "a2", "srv", "write", "prev", None, 200, None).unwrap();

        let rows = list_pending_approvals(&conn).unwrap();
        assert_eq!(rows.len(), 2);
        let by_id = |id: &str| rows.iter().find(|r| r.id == id).unwrap();
        assert_eq!(by_id("a1").pane_id.as_deref(), Some(real_key));
        assert_eq!(by_id("a2").pane_id, None);
        // 세션 키는 core 파서로만 해석된다 — DB는 세션을 모른다.
        let (ws, session) = deppy_core::parse_session_key(by_id("a1").pane_id.as_deref().unwrap())
            .expect("실제 형식은 파싱된다");
        assert_eq!(ws, "315f68b6-333f-409f-a2c5-922b9eacfd7e");
        assert_eq!(session.0, 2);
    }

    #[test]
    fn pending_approval_batch_insert_roundtrip() {
        let conn = test_conn();
        insert_pending_approval_batch(
            &conn,
            &[
                PendingApprovalInsert {
                    id: "b".to_owned(),
                    server_id: "srv".to_owned(),
                    tool_name: "read_file".to_owned(),
                    arguments_preview: "path=/tmp/b".to_owned(),
                    schema_hash: None,
                    created_at: 20,
                    pane_id: None,
                },
                PendingApprovalInsert {
                    id: "a".to_owned(),
                    server_id: "srv".to_owned(),
                    tool_name: "write_file".to_owned(),
                    arguments_preview: "path=/tmp/a".to_owned(),
                    schema_hash: Some("hash".to_owned()),
                    created_at: 10,
                    pane_id: None,
                },
            ],
        )
        .unwrap();

        let rows = list_pending_approvals(&conn).unwrap();
        let ids: Vec<_> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
        assert_eq!(rows[0].schema_hash.as_deref(), Some("hash"));
    }

    #[test]
    fn prune_resolved_approvals는_pending을_보존하고_오래된_resolved만_삭제() {
        let conn = test_conn();
        insert_pending_approval(&conn, "pending", "srv", "tool", "prev", None, 10, None).unwrap();
        insert_pending_approval(&conn, "old", "srv", "tool", "prev", None, 20, None).unwrap();
        insert_pending_approval(&conn, "new", "srv", "tool", "prev", None, 30, None).unwrap();
        resolve_approval(&conn, "old", true, false, 100).unwrap();
        resolve_approval(&conn, "new", false, false, 300).unwrap();

        assert_eq!(prune_resolved_approvals(&conn, 200).unwrap(), 1);
        assert!(poll_approval(&conn, "old").is_err());
        assert_eq!(
            poll_approval(&conn, "new").unwrap().status,
            ApprovalStatus::Denied
        );
        assert_eq!(
            list_pending_approvals(&conn).unwrap()[0].id.as_str(),
            "pending"
        );
    }

    #[test]
    fn replace_tools는_기존을_지우고_교체() {
        let mut conn = test_conn();
        insert_server(&conn, &sample_server()).unwrap();
        let tool = |id: &str, name: &str| McpToolRow {
            id: id.to_owned(),
            server_id: "srv-1".to_owned(),
            name: name.to_owned(),
            description: None,
            input_schema_json: None,
            trust_level: "unknown".to_owned(),
            schema_hash: None,
        };
        replace_tools_for_server(&mut conn, "srv-1", &[tool("t1", "old")]).unwrap();
        replace_tools_for_server(
            &mut conn,
            "srv-1",
            &[tool("t2", "new_a"), tool("t3", "new_b")],
        )
        .unwrap();
        let names: Vec<String> = list_tools_for_server(&conn, "srv-1")
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, vec!["new_a", "new_b"]);
    }

    #[test]
    fn replace_tools의_server_id_불일치는_기존목록을_보존한다() {
        let mut conn = test_conn();
        insert_server(&conn, &sample_server()).unwrap();
        let old = McpToolRow {
            id: "old".to_owned(),
            server_id: "srv-1".to_owned(),
            name: "old".to_owned(),
            description: None,
            input_schema_json: None,
            trust_level: "unknown".to_owned(),
            schema_hash: None,
        };
        insert_tool(&conn, &old).unwrap();
        let mut wrong = old.clone();
        wrong.id = "new".to_owned();
        wrong.server_id = "srv-other".to_owned();

        assert!(replace_tools_for_server(&mut conn, "srv-1", &[wrong]).is_err());
        assert_eq!(list_tools_for_server(&conn, "srv-1").unwrap(), vec![old]);
    }

    #[test]
    fn replace_tools_두번째_insert_실패는_delete와_첫insert를_rollback한다() {
        let mut conn = test_conn();
        insert_server(&conn, &sample_server()).unwrap();
        let tool = |id: &str, name: &str| McpToolRow {
            id: id.to_owned(),
            server_id: "srv-1".to_owned(),
            name: name.to_owned(),
            description: None,
            input_schema_json: None,
            trust_level: "unknown".to_owned(),
            schema_hash: None,
        };
        let old = tool("old", "old");
        insert_tool(&conn, &old).unwrap();
        let duplicate_id = [tool("new", "first"), tool("new", "second")];

        assert!(replace_tools_for_server(&mut conn, "srv-1", &duplicate_id).is_err());
        assert_eq!(list_tools_for_server(&conn, "srv-1").unwrap(), vec![old]);
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
    fn tool_name은_server와_id_exact_match에서_name만_반환한다() {
        let conn = test_conn();
        insert_server(&conn, &sample_server()).unwrap();
        let mut tool = sample_tool("tool-exact", "파일_읽기");
        tool.description = Some("description-must-not-be-returned".to_owned());
        tool.input_schema_json = Some("schema-must-not-be-returned".to_owned());
        insert_tool(&conn, &tool).unwrap();

        assert_eq!(
            tool_name(&conn, "srv-1", "tool-exact").unwrap(),
            Some("파일_읽기".to_owned())
        );
        assert_eq!(
            tool_name(&conn, "wrong-server", "tool-exact").unwrap(),
            None
        );
        assert_eq!(tool_name(&conn, "srv-1", "missing-tool").unwrap(), None);
    }

    #[test]
    fn tool_name은_empty와_oversized_persisted_name을_fail_closed한다() {
        let conn = test_conn();
        insert_server(&conn, &sample_server()).unwrap();
        insert_tool(&conn, &sample_tool("tool-empty", "")).unwrap();
        insert_tool(
            &conn,
            &sample_tool(
                "tool-oversized-name",
                &"x".repeat(MCP_TOOL_NAME_BYTES_MAX + 1),
            ),
        )
        .unwrap();

        assert!(tool_name(&conn, "srv-1", "tool-empty").is_err());
        assert!(tool_name(&conn, "srv-1", "tool-oversized-name").is_err());
    }

    #[test]
    fn tool_page는_same_snapshot_total과_ordered_permission_join을_bounded로_반환한다() {
        let conn = test_conn();
        insert_server(&conn, &sample_server()).unwrap();
        for (id, name) in [("t-c", "charlie"), ("t-a", "alpha"), ("t-b", "bravo")] {
            insert_tool(&conn, &sample_tool(id, name)).unwrap();
        }
        upsert_permission_rule(&conn, "srv-1", "bravo", "allow", Some("hash-bravo")).unwrap();

        let page = tool_page(&conn, "srv-1", 1, 2).unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(page.rows.len(), 2);
        assert_eq!(page.rows[0].name, "bravo");
        assert_eq!(page.rows[0].permission.as_ref().unwrap().rule, "allow");
        assert_eq!(page.rows[1].name, "charlie");
        assert!(page.rows[1].permission.is_none());
        let debug = format!("{page:?}");
        assert!(!debug.contains("bravo"));
        assert!(!debug.contains("description"));

        assert!(tool_page(&conn, "srv-1", 0, 0).is_err());
        assert!(tool_page(&conn, "srv-1", 0, MCP_TOOL_PAGE_LIMIT_MAX + 1).is_err());
        assert!(tool_page(&conn, "srv-1", usize::MAX, 1).is_err());
    }

    #[test]
    fn tool_page_hard_cap은_total과_rows상한을_분리한다() {
        let conn = test_conn();
        insert_server(&conn, &sample_server()).unwrap();
        for index in 0..=MCP_TOOL_PAGE_LIMIT_MAX {
            insert_tool(
                &conn,
                &sample_tool(&format!("tool-{index:03}"), &format!("name-{index:03}")),
            )
            .unwrap();
        }

        let page = tool_page(&conn, "srv-1", 0, MCP_TOOL_PAGE_LIMIT_MAX).unwrap();
        assert_eq!(page.total, MCP_TOOL_PAGE_LIMIT_MAX + 1);
        assert_eq!(page.rows.len(), MCP_TOOL_PAGE_LIMIT_MAX);
    }

    #[test]
    fn tool_page는_schema를_읽지않고_summary_byte_preflight를_강제한다() {
        let conn = test_conn();
        insert_server(&conn, &sample_server()).unwrap();
        let mut oversized = sample_tool("tool-oversized", "read");
        oversized.description = Some("x".repeat(MCP_TOOL_PAGE_BYTES_MAX + 1));
        oversized.input_schema_json = Some("schema-must-not-enter-page".to_owned());
        insert_tool(&conn, &oversized).unwrap();

        assert!(tool_page(&conn, "srv-1", 0, 1).is_err());
    }

    #[test]
    fn transactional_server_delete는_pending을_deny하고_live_metadata를_정리한다() {
        let mut conn = test_conn();
        insert_server(&conn, &sample_server()).unwrap();
        insert_tool(&conn, &sample_tool("tool-read", "read")).unwrap();
        upsert_permission_rule(&conn, "srv-1", "read", "allow", Some("hash")).unwrap();
        insert_pending_approval(&conn, "approval", "srv-1", "read", "{}", None, 1, None).unwrap();

        assert!(delete_server_in_transaction(&conn, "srv-1", 10).is_err());
        {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            assert!(delete_server_in_transaction(&tx, "srv-1", 10).unwrap());
            tx.commit().unwrap();
        }

        assert!(server(&conn, "srv-1").unwrap().is_none());
        assert!(list_tools_for_server(&conn, "srv-1").unwrap().is_empty());
        assert!(list_permission_rules(&conn).unwrap().is_empty());
        assert_eq!(
            poll_approval(&conn, "approval").unwrap(),
            ApprovalOutcome {
                status: ApprovalStatus::Denied,
                remember: false,
            }
        );
    }

    #[test]
    fn transactional_server_delete_중간실패는_pending과모든metadata를_rollback한다() {
        let mut conn = test_conn();
        let server_row = sample_server();
        let tool = sample_tool("tool-read", "read");
        insert_server(&conn, &server_row).unwrap();
        insert_tool(&conn, &tool).unwrap();
        upsert_permission_rule(&conn, "srv-1", "read", "allow", Some("hash")).unwrap();
        insert_pending_approval(&conn, "approval", "srv-1", "read", "{}", None, 1, None).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_server_tool_delete BEFORE DELETE ON mcp_tools
             BEGIN SELECT RAISE(ABORT, 'injected server delete failure'); END;",
        )
        .unwrap();

        {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            assert!(delete_server_in_transaction(&tx, "srv-1", 10).is_err());
        }
        assert_eq!(server(&conn, "srv-1").unwrap(), Some(server_row));
        assert_eq!(list_tools_for_server(&conn, "srv-1").unwrap(), vec![tool]);
        assert_eq!(list_permission_rules(&conn).unwrap().len(), 1);
        assert_eq!(
            poll_approval(&conn, "approval").unwrap().status,
            ApprovalStatus::Pending
        );
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
