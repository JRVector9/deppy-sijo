use std::path::Path;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension};

const RESOLVED_APPROVAL_RETENTION_SECS: i64 = 30 * 24 * 60 * 60;

/// env 값. secret은 평문 대신 credentials.id만 참조한다 (설계문서 6.3).
/// 평문 해석은 spawn 직전(PR-09)에만 일어난다.
#[derive(Clone, PartialEq)]
pub enum EnvValue {
    Plain(String),
    Secret { credential_id: String },
}

impl std::fmt::Debug for EnvValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvValue::Plain(_) => f.debug_tuple("Plain").field(&"[REDACTED_PLAIN]").finish(),
            EnvValue::Secret { .. } => f
                .debug_struct("Secret")
                .field("credential_id", &"[REDACTED_CREDENTIAL_ID]")
                .finish(),
        }
    }
}

/// SQLite metadata DB (설계문서 11장). secret 평문은 절대 저장하지 않는다 —
/// credentials 행은 keyring 좌표와 masked_hint만 가진다 (6.3).
pub struct Db {
    conn: Connection,
}

/// user_version 기반 forward-only 마이그레이션 (설계문서 11.9).
/// 1: credentials (PR-02), 2: workspaces + env_profiles/env_vars (PR-03, 11.0/11.6),
/// 3: agent_configs (PR-09, 11.0 — *_regex 컬럼은 PR-12 status detector가 소비),
/// 4: sessions + mux_* (PR-14, persist crate DDL),
/// 5: mcp_servers + mcp_tools (PR-15, mcp crate DDL),
/// 6: tool_audit_logs (PR-16, audit crate DDL),
/// 7: agent_configs.deleted_at (soft-delete — 세션 영속 FK와 공존),
/// 8: tool_permission_rules (PR-16 권한 규칙 영속),
/// 9: pending_approvals (agent-proxy 1.5 — proxy↔GUI 라이브 승인 IPC 채널),
/// 10: agent_configs.mcp_proxy_* (agent-proxy 배선 — spawn 시 .mcp.json 생성 +
///     --mcp-config로 에이전트를 deppy-mcp-proxy 권한계층에 태운다).
/// 11: agent_configs.mcp_config_flag (에이전트별 주입 플래그 커스텀 — NULL=기본 --mcp-config).
/// 12: mcp_servers scoped env metadata (plain-safe env + credential ids, PR-U10b).
/// 4~6은 각 crate가 소유한 DDL 상수를 그대로 붙인다 (스키마 정의는 한 곳에서만).
pub(crate) const MIGRATIONS: &[&str] = &[
    "
CREATE TABLE credentials (
    id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    label TEXT NOT NULL,
    credential_kind TEXT NOT NULL,
    keyring_service TEXT NOT NULL,
    keyring_username TEXT NOT NULL,
    masked_hint TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    last_used_at TEXT
);
",
    "
CREATE TABLE workspaces (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    path TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE env_profiles (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    name TEXT NOT NULL,
    kind TEXT NOT NULL DEFAULT 'custom',
    is_production INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY(workspace_id) REFERENCES workspaces(id)
);

CREATE TABLE env_vars (
    id TEXT PRIMARY KEY,
    profile_id TEXT NOT NULL,
    key TEXT NOT NULL,
    kind TEXT NOT NULL,
    plain_value TEXT,
    credential_id TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(profile_id, key),
    CHECK (kind IN ('plain', 'secret')),
    CHECK (kind != 'secret' OR (plain_value IS NULL AND credential_id IS NOT NULL)),
    CHECK (kind != 'plain' OR credential_id IS NULL),
    FOREIGN KEY(profile_id) REFERENCES env_profiles(id),
    FOREIGN KEY(credential_id) REFERENCES credentials(id)
);

CREATE INDEX idx_env_vars_profile_key ON env_vars(profile_id, key);
",
    "
CREATE TABLE agent_configs (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    command TEXT NOT NULL,
    args_json TEXT NOT NULL,
    env_json TEXT,
    env_credentials_json TEXT,
    waiting_regex TEXT,
    approval_regex TEXT,
    error_regex TEXT,
    done_regex TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
",
    persist::MIGRATION_SQL,
    mcp_store::MIGRATION_SQL,
    audit::MIGRATION_SQL,
    // 7: agent_configs soft-delete — sessions.agent_id FK(§11.1)가 실행 이력이
    //    있는 config의 물리 삭제를 막으므로, 삭제는 표시로 대체한다
    "ALTER TABLE agent_configs ADD COLUMN deleted_at TEXT;",
    // 8: tool 권한 규칙 영속 (PR-16 — 재시작해도 Allow/Deny always가 유지되도록).
    //    (server_id, tool_name)별 rule + 마지막 승인 schema hash. FK는 두지 않는다
    //    (규칙은 문자열 키로 느슨히 연결 — orphan은 무해, 감사/로그와 동일한 관례).
    mcp_store::MIGRATION_TOOL_PERMISSION_RULES,
    // 9: pending_approvals (agent-proxy option 1.5). deppy-mcp-proxy가 Ask 규칙 tool을
    //    만나면 이 표에 pending 행을 넣고 status를 폴링한다; GUI는 pending 행을 감시해
    //    팝업을 띄우고 결정을 되쓴다. 두 프로세스가 같은 DB(WAL+busy_timeout)를 공유하는
    //    IPC 채널. arguments_preview는 proxy가 redact를 끝낸 표시용 문자열만 담는다(원문 secret 금지).
    //    권한 규칙(tool_permission_rules)과 마찬가지로 FK는 두지 않는다(문자열 키 느슨 연결).
    mcp_store::MIGRATION_PENDING_APPROVALS,
    // 10: agent_configs에 MCP proxy 배선 컬럼 추가. mcp_proxy_enabled=1이면 spawn 시
    //     deppy-mcp-proxy를 프론트하는 .mcp.json을 생성해 --mcp-config로 붙인다.
    //     mcp_proxy_server_id는 프론트할 backend mcp_server id (FK 없이 문자열 느슨 연결 —
    //     tool_permission_rules/pending_approvals와 동일 관례; 서버가 지워지면 spawn 시 검증).
    "
ALTER TABLE agent_configs ADD COLUMN mcp_proxy_enabled INTEGER NOT NULL DEFAULT 0;
ALTER TABLE agent_configs ADD COLUMN mcp_proxy_server_id TEXT;
",
    // 11: agent_configs에 MCP config 주입 플래그 커스텀 컬럼 추가. NULL이면 기본 --mcp-config를
    //     쓰고, 값이 있으면 그 플래그 이름으로 붙인다 (에이전트마다 다른 규약 대응). nullable —
    //     기존 행/enabled 여부와 무관하게 NULL 허용(빈 문자열은 API 레벨에서 None으로 정규화).
    "ALTER TABLE agent_configs ADD COLUMN mcp_config_flag TEXT;",
    // 12: mcp_servers scoped env. secret 값은 저장하지 않고 credential id만 저장한다.
    mcp_store::MIGRATION_SERVER_ENV,
    // 13: 옵션2 에이전트 세션 — 재시작 복원 시 native resume(claude --resume / codex
    // resume)에 쓸 (pane, kind, agent session-id). pane_id는 복원 시 verbatim 유지되는
    // durable id라 바인딩 키로 쓴다.
    "
CREATE TABLE agent_sessions (
    workspace_id TEXT NOT NULL,
    pane_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    session_id TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, pane_id)
);
",
    // 14: 에이전트 needsInput(승인/입력 대기) — claude/codex hook이 세션 키(DEPPY_SESSION_ID
    // = pane_id)로 set/clear한다. 앱이 레일 상태(주황)에 반영. hook 수신은 deppy-mcp-proxy.
    "
CREATE TABLE agent_needs_input (
    session_key TEXT PRIMARY KEY,
    waiting INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
",
    // v15: hook이 보고한 에이전트 세션 바인딩 (SessionStart 등 — session_key는
    // {workspace_id}:{session_id}, 옵션2 hook 배선의 결정적 바인딩 소스)
    "
CREATE TABLE agent_hook_sessions (
    session_key TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    agent_session_id TEXT NOT NULL,
    transcript_path TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
",
];

/// 옵션2: 저장된 에이전트 세션 한 행 — 재시작 복원 시 native resume에 쓴다.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentSessionRow {
    pub pane_id: String,
    /// "claude" | "codex".
    pub kind: String,
    /// 에이전트 자신의 세션 ID (`claude --resume <id>` / `codex resume <id>`).
    pub session_id: String,
}

/// hook이 보고한 세션 바인딩 행 (v15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSessionRow {
    pub session_key: String,
    pub kind: String,
    pub agent_session_id: String,
    pub transcript_path: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CredentialMeta {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub credential_kind: String,
    pub masked_hint: Option<String>,
}

/// workspace 한 행 (WorkspaceSidebar 표시용).
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceRow {
    pub id: String,
    pub name: String,
    /// 프로젝트 루트 경로 (파일 트리 FT-0). 빈 문자열 = 미설정 (기본/구 workspace).
    pub path: String,
    pub created_at: String,
}

/// 권한 규칙/승인 IPC 타입은 mcp-store 소유(v2.8) — 기존 storage:: 경로 호환을 위해 재수출.
pub use mcp_store::{ApprovalOutcome, ApprovalStatus, PendingApprovalRow, PermissionRuleRow};

#[derive(Debug, Clone, PartialEq)]
pub struct EnvProfileRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub is_production: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnvVarRow {
    pub key: String,
    pub value: EnvValue,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentConfigRow {
    pub id: String,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// status detector regex (PR-12). 빈 문자열은 None으로 정규화.
    pub waiting_regex: Option<String>,
    pub approval_regex: Option<String>,
    pub error_regex: Option<String>,
    pub done_regex: Option<String>,
    /// deppy-mcp-proxy 권한계층 경유 여부 (migration 10). true면 spawn 시 .mcp.json 생성.
    pub mcp_proxy_enabled: bool,
    /// 프론트할 backend mcp_server id (enabled일 때만 의미). None이면 미선택 — spawn은 미주입.
    pub mcp_proxy_server_id: Option<String>,
    /// MCP config 주입 플래그 이름 커스텀 (migration 11). None/빈값이면 기본 `--mcp-config`.
    /// 플래그 이름만 — 경로는 항상 다음 arg로 붙는다 (`--flag=path` 규약은 후속 과제).
    pub mcp_config_flag: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecretLikeReason {
    EnvKey,
    EnvValue,
    ArgFlag,
    BearerToken,
    DatabaseUrl,
    TokenLiteral,
}

impl SecretLikeReason {
    fn label(self) -> &'static str {
        match self {
            SecretLikeReason::EnvKey => "secret-like env key",
            SecretLikeReason::EnvValue => "secret-like env value",
            SecretLikeReason::ArgFlag => "secret-like command argument flag",
            SecretLikeReason::BearerToken => "bearer token payload",
            SecretLikeReason::DatabaseUrl => "database URL payload",
            SecretLikeReason::TokenLiteral => "token-like payload",
        }
    }
}

fn secret_like_env_plain_reason(key: &str, value: &str) -> Option<SecretLikeReason> {
    if secret_like_env_key(key) {
        return Some(SecretLikeReason::EnvKey);
    }
    secret_like_value(value).map(|reason| match reason {
        SecretLikeReason::DatabaseUrl => SecretLikeReason::DatabaseUrl,
        SecretLikeReason::BearerToken => SecretLikeReason::BearerToken,
        SecretLikeReason::TokenLiteral => SecretLikeReason::TokenLiteral,
        _ => SecretLikeReason::EnvValue,
    })
}

fn secret_like_env_key(key: &str) -> bool {
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

fn validate_args_for_persistence(args: &[String], label: &str) -> anyhow::Result<()> {
    if let Some(reason) = secret_like_args_reason(args) {
        anyhow::bail!(
            "{label}에 {}가 포함되어 저장을 거부합니다. secret은 credential/env binding으로 저장하세요",
            reason.label()
        );
    }
    Ok(())
}

fn secret_like_args_reason(args: &[String]) -> Option<SecretLikeReason> {
    let joined = args.join(" ");
    if contains_bearer_payload(&joined) {
        return Some(SecretLikeReason::BearerToken);
    }
    if contains_database_url_payload(&joined) {
        return Some(SecretLikeReason::DatabaseUrl);
    }
    for arg in args {
        let trimmed = arg.trim();
        if secret_like_arg_flag(trimmed) {
            return Some(SecretLikeReason::ArgFlag);
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

fn secret_like_assignment(arg: &str) -> Option<SecretLikeReason> {
    let (key, value) = arg.split_once('=')?;
    let key = key.trim().trim_start_matches('-');
    let value = value.trim();
    if key.is_empty() || value.is_empty() {
        return None;
    }
    if secret_like_env_key(key) {
        let key = normalize_identifier(key);
        if key == "DATABASE_URL"
            || key == "DB_URL"
            || key.ends_with("_DATABASE_URL")
            || key.ends_with("_DB_URL")
        {
            return Some(SecretLikeReason::DatabaseUrl);
        }
        return Some(SecretLikeReason::ArgFlag);
    }
    secret_like_value(value)
}

fn secret_like_value(value: &str) -> Option<SecretLikeReason> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if contains_bearer_payload(trimmed) {
        return Some(SecretLikeReason::BearerToken);
    }
    if contains_database_url_payload(trimmed) || looks_like_database_url_with_password(trimmed) {
        return Some(SecretLikeReason::DatabaseUrl);
    }
    if looks_like_token_literal(trimmed) {
        return Some(SecretLikeReason::TokenLiteral);
    }
    for token in secret_like_tokens(trimmed) {
        if looks_like_database_url_with_password(token) {
            return Some(SecretLikeReason::DatabaseUrl);
        }
        if looks_like_token_literal(token) {
            return Some(SecretLikeReason::TokenLiteral);
        }
    }
    None
}

fn secret_like_tokens(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ';'))
        .filter(|token| !token.is_empty())
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

impl Db {
    /// DB 열기 + 마이그레이션. infra(PRAGMA/백업/IMMEDIATE 러너)는 storage-core가 담당하고
    /// (v2.8 §6.1), 이 crate는 **마이그레이션 원장(MIGRATIONS, v1..vN 순서 불변)** 조립과
    /// 앱 수준 store/facade만 소유한다.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = storage_core::open_with_migrations(path, MIGRATIONS)?;
        Ok(Self { conn })
    }

    #[cfg(test)]
    fn open_in_memory() -> anyhow::Result<Self> {
        let conn = storage_core::open_in_memory_with_migrations(MIGRATIONS)?;
        Ok(Self { conn })
    }

    /// 현재 user_version (테스트에서 마이그레이션 가드로 사용).
    #[cfg(test)]
    fn read_user_version(conn: &Connection) -> anyhow::Result<usize> {
        storage_core::read_user_version(conn)
    }

    /// credential metadata 추가. created_at/updated_at은 SQLite가 UTC로 기록한다.
    pub fn insert_credential(&self, meta: &CredentialMeta) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT INTO credentials
                   (id, provider, label, credential_kind,
                    keyring_service, keyring_username, masked_hint, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7,
                    strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
                (
                    &meta.id,
                    &meta.provider,
                    &meta.label,
                    &meta.credential_kind,
                    secret::KEYRING_SERVICE,
                    &meta.id, // keyring username = credential id
                    &meta.masked_hint,
                ),
            )
            .with_context(|| format!("credential 저장 실패: {}", meta.id))?;
        Ok(())
    }

    pub fn list_credentials(&self) -> anyhow::Result<Vec<CredentialMeta>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, provider, label, credential_kind, masked_hint
             FROM credentials ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(CredentialMeta {
                id: row.get(0)?,
                provider: row.get(1)?,
                label: row.get(2)?,
                credential_kind: row.get(3)?,
                masked_hint: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// 참조가 없을 때만 metadata 행을 지운다 — 확인과 삭제를 한 문장으로 묶어
    /// "확인 후 삭제 사이에 참조가 생기는" TOCTOU를 없앤다 (codex 리뷰).
    /// 지웠으면 true, 참조 중이거나 없는 id면 false.
    pub fn delete_credential_if_unused(&self, id: &str) -> anyhow::Result<bool> {
        let affected = self
            .conn
            .execute(
                "DELETE FROM credentials WHERE id = ?1
                   AND NOT EXISTS (SELECT 1 FROM env_vars WHERE credential_id = ?1)
                   AND NOT EXISTS (
                       SELECT 1
                       FROM mcp_servers, json_each(COALESCE(mcp_servers.env_credentials_json, '{}'))
                       WHERE json_each.value = ?1
                   )",
                [id],
            )
            .with_context(|| format!("credential 삭제 실패: {id}"))?;
        Ok(affected == 1)
    }

    /// env var 또는 MCP scoped env가 이 credential을 참조 중인지 확인 (UI 에러 메시지 구분용).
    pub fn credential_in_use(&self, id: &str) -> anyhow::Result<bool> {
        let exists: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM env_vars WHERE credential_id = ?1
                 UNION ALL
                 SELECT 1
                 FROM mcp_servers, json_each(COALESCE(mcp_servers.env_credentials_json, '{}'))
                 WHERE json_each.value = ?1
                 LIMIT 1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(exists.is_some())
    }

    /// 기본 workspace를 보장하고 id를 돌려준다. 실제 workspace 관리는 PR-14.
    pub fn ensure_default_workspace(&self) -> anyhow::Result<String> {
        if let Some(id) = self
            .conn
            .query_row(
                "SELECT id FROM workspaces ORDER BY created_at LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            return Ok(id);
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO workspaces (id, name, path, created_at, updated_at)
             VALUES (?1, 'default', '',
                strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
            [&id],
        )?;
        Ok(id)
    }

    /// 모든 workspace 목록 (WorkspaceSidebar, 생성순).
    pub fn list_workspaces(&self) -> anyhow::Result<Vec<WorkspaceRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, path, created_at FROM workspaces ORDER BY created_at")?;
        let rows = stmt.query_map([], |row| {
            Ok(WorkspaceRow {
                id: row.get(0)?,
                name: row.get(1)?,
                path: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// workspace의 프로젝트 경로를 설정한다 (FT-0 — 컬럼은 v2부터 존재, 값 채움만).
    /// 감지된 에이전트 세션 하나를 upsert한다(옵션2). replace-all이 아니라 차등 upsert라
    /// 시작 직후(감지 전) 저장된 복원 데이터를 지우지 않는다.
    pub fn upsert_agent_session(
        &self,
        workspace_id: &str,
        pane_id: &str,
        kind: &str,
        session_id: &str,
    ) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO agent_sessions
                   (workspace_id, pane_id, kind, session_id, updated_at)
                 VALUES (?1, ?2, ?3, ?4, CAST(strftime('%s','now') AS INTEGER))",
                (workspace_id, pane_id, kind, session_id),
            )
            .with_context(|| format!("agent session 저장 실패: {pane_id}"))?;
        Ok(())
    }

    /// 에이전트가 종료돼 더는 감지되지 않는 pane의 행을 지운다 — 복원 시 이미 닫은
    /// 에이전트를 되살리지 않도록.
    pub fn delete_agent_session(&self, workspace_id: &str, pane_id: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "DELETE FROM agent_sessions WHERE workspace_id = ?1 AND pane_id = ?2",
            (workspace_id, pane_id),
        )?;
        Ok(())
    }

    /// 에이전트 needsInput 상태를 세션 키(pane_id)로 set/clear한다 (hook 수신부가 호출).
    /// hook 상태 테이블의 오래된 행 정리(7일) — 읽기는 최근만 보지만 행 자체가 무한
    /// 누적되는 것을 막는다(시작 시 1회 호출).
    pub fn prune_agent_hook_state(&self) -> anyhow::Result<()> {
        self.conn.execute(
            "DELETE FROM agent_hook_sessions WHERE updated_at < strftime('%s','now') - 604800",
            [],
        )?;
        self.conn.execute(
            "DELETE FROM agent_needs_input WHERE updated_at < strftime('%s','now') - 604800",
            [],
        )?;
        Ok(())
    }

    /// hook(SessionStart 등)이 보고한 에이전트 바인딩 upsert (v15).
    pub fn upsert_hook_session(
        &self,
        session_key: &str,
        kind: &str,
        agent_session_id: &str,
        transcript_path: &str,
    ) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO agent_hook_sessions
                 (session_key, kind, agent_session_id, transcript_path, updated_at)
                 VALUES (?1, ?2, ?3, ?4, strftime('%s','now'))",
                rusqlite::params![session_key, kind, agent_session_id, transcript_path],
            )
            .map(|_| ())
            .map_err(Into::into)
    }

    /// hook이 보고한 바인딩 목록 (최근 24h — 죽은 세션 행이 영원히 남지 않게).
    pub fn list_hook_sessions(&self) -> anyhow::Result<Vec<HookSessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_key, kind, agent_session_id, transcript_path FROM agent_hook_sessions
             WHERE updated_at > strftime('%s','now') - 86400",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(HookSessionRow {
                    session_key: r.get(0)?,
                    kind: r.get(1)?,
                    agent_session_id: r.get(2)?,
                    transcript_path: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn set_agent_needs_input(&self, session_key: &str, waiting: bool) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO agent_needs_input (session_key, waiting, updated_at)
                 VALUES (?1, ?2, CAST(strftime('%s','now') AS INTEGER))",
                (session_key, waiting as i64),
            )
            .with_context(|| format!("needsInput 저장 실패: {session_key}"))?;
        Ok(())
    }

    /// 현재 입력 대기(waiting) 중인 세션 키 목록. stale(1시간 초과)은 제외해 죽은 hook의
    /// 잔여가 영원히 주황으로 남지 않게 한다.
    pub fn list_waiting_sessions(&self) -> anyhow::Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_key FROM agent_needs_input
             WHERE waiting = 1
               AND updated_at > CAST(strftime('%s','now') AS INTEGER) - 3600",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// 워크스페이스의 저장된 에이전트 세션 (복원 시 resume 대상).
    pub fn list_agent_sessions(&self, workspace_id: &str) -> anyhow::Result<Vec<AgentSessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT pane_id, kind, session_id FROM agent_sessions WHERE workspace_id = ?1",
        )?;
        let rows = stmt.query_map([workspace_id], |row| {
            Ok(AgentSessionRow {
                pane_id: row.get(0)?,
                kind: row.get(1)?,
                session_id: row.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// workspace 이름을 변경한다 (#3 — 사용자 지정 이름).
    pub fn rename_workspace(&self, id: &str, name: &str) -> anyhow::Result<()> {
        let affected = self
            .conn
            .execute(
                "UPDATE workspaces
                 SET name = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE id = ?1",
                (id, name),
            )
            .with_context(|| format!("workspace 이름 저장 실패: {id}"))?;
        anyhow::ensure!(affected == 1, "workspace 없음: {id}");
        Ok(())
    }

    pub fn set_workspace_path(&self, id: &str, path: &str) -> anyhow::Result<()> {
        let affected = self
            .conn
            .execute(
                "UPDATE workspaces
                 SET path = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE id = ?1",
                (id, path),
            )
            .with_context(|| format!("workspace 경로 저장 실패: {id}"))?;
        anyhow::ensure!(affected == 1, "workspace 없음: {id}");
        Ok(())
    }

    /// workspace의 프로젝트 경로. 없는 id는 None, 미설정은 Some("").
    pub fn workspace_path(&self, id: &str) -> anyhow::Result<Option<String>> {
        self.conn
            .query_row("SELECT path FROM workspaces WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()
            .map_err(Into::into)
    }

    /// workspace + 그 자식 데이터(세션/mux/env)를 한 트랜잭션으로 삭제한다 (destructive).
    /// mcp_servers(전역)·감사 로그(FK 없음)는 남긴다. 활성/마지막 workspace 삭제 방지는
    /// 호출측(UI) 책임.
    pub fn delete_workspace(&mut self, workspace_id: &str) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        // persist 소유 테이블 (sessions, mux_*) — FK 순서
        persist::delete_workspace_data(&tx, workspace_id)?;
        // env (storage 소유): env_vars → env_profiles
        tx.execute(
            "DELETE FROM env_vars WHERE profile_id IN
               (SELECT id FROM env_profiles WHERE workspace_id = ?1)",
            [workspace_id],
        )?;
        tx.execute(
            "DELETE FROM env_profiles WHERE workspace_id = ?1",
            [workspace_id],
        )?;
        // 옵션2 에이전트 세션 (storage 소유) — 워크스페이스와 함께 정리(orphan 방지).
        tx.execute(
            "DELETE FROM agent_sessions WHERE workspace_id = ?1",
            [workspace_id],
        )?;
        tx.execute("DELETE FROM workspaces WHERE id = ?1", [workspace_id])?;
        tx.commit()?;
        Ok(())
    }

    /// 새 workspace 생성 — 생성된 id 반환.
    pub fn create_workspace(&self, name: &str) -> anyhow::Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO workspaces (id, name, path, created_at, updated_at)
             VALUES (?1, ?2, '',
                strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
            (&id, name),
        )?;
        Ok(id)
    }

    /// MCP 서버 목록 (Connector Center, PR-17). repo 로직은 mcp crate 소유.
    pub fn list_mcp_servers(&self) -> anyhow::Result<Vec<mcp_store::McpServerRow>> {
        mcp_store::list_servers(&self.conn)
    }

    pub fn insert_mcp_server(&self, row: &mcp_store::McpServerRow) -> anyhow::Result<()> {
        mcp_store::insert_server(&self.conn, row)
    }

    /// 연결 테스트로 발견한 tools를 교체 저장 (PR-17).
    pub fn replace_mcp_tools(
        &mut self,
        server_id: &str,
        rows: &[mcp_store::McpToolRow],
    ) -> anyhow::Result<()> {
        mcp_store::replace_tools_for_server(&mut self.conn, server_id, rows)
    }

    /// 저장된 tool 목록 (도구 실행 UI용).
    pub fn list_mcp_tools(&self, server_id: &str) -> anyhow::Result<Vec<mcp_store::McpToolRow>> {
        mcp_store::list_tools_for_server(&self.conn, server_id)
    }

    /// 저장된 tool 권한 규칙 전체 (앱 시작 시 PermissionPolicy로 로드).
    pub fn list_permission_rules(&self) -> anyhow::Result<Vec<PermissionRuleRow>> {
        mcp_store::list_permission_rules(&self.conn)
    }

    /// 권한 규칙 저장/갱신 (AllowAlways/DenyAlways 결정 시).
    pub fn upsert_permission_rule(
        &self,
        server_id: &str,
        tool_name: &str,
        rule: &str,
        approved_schema_hash: Option<&str>,
    ) -> anyhow::Result<()> {
        mcp_store::upsert_permission_rule(
            &self.conn,
            server_id,
            tool_name,
            rule,
            approved_schema_hash,
        )
    }

    /// 권한 규칙 삭제 (Ask로 재설정 — 행이 없으면 기본값 Ask).
    pub fn delete_permission_rule(&self, server_id: &str, tool_name: &str) -> anyhow::Result<()> {
        mcp_store::delete_permission_rule(&self.conn, server_id, tool_name)
    }

    /// 라이브 승인 요청 등록 (deppy-mcp-proxy → GUI). 세부 계약은 mcp_store 문서 참조.
    pub fn insert_pending_approval(
        &self,
        id: &str,
        server_id: &str,
        tool_name: &str,
        arguments_preview: &str,
        schema_hash: Option<&str>,
        created_at: i64,
    ) -> anyhow::Result<()> {
        mcp_store::insert_pending_approval(
            &self.conn,
            id,
            server_id,
            tool_name,
            arguments_preview,
            schema_hash,
            created_at,
        )
    }

    /// 현재 상태 폴링 (proxy). 행 없음/기형 status는 Err — fail-closed.
    pub fn poll_approval(&self, id: &str) -> anyhow::Result<ApprovalOutcome> {
        mcp_store::poll_approval(&self.conn, id)
    }

    /// pending 상태 요청만, 오래된 순으로 (GUI 목록).
    pub fn list_pending_approvals(&self) -> anyhow::Result<Vec<PendingApprovalRow>> {
        mcp_store::list_pending_approvals(&self.conn)
    }

    /// GUI가 결정을 되쓴다 — first-writer-wins, 이미 해소된 id는 조용한 no-op.
    pub fn resolve_approval(
        &self,
        id: &str,
        allowed: bool,
        remember: bool,
        resolved_at: i64,
    ) -> anyhow::Result<()> {
        mcp_store::resolve_approval(&self.conn, id, allowed, remember, resolved_at)?;
        let cutoff = resolved_at.saturating_sub(RESOLVED_APPROVAL_RETENTION_SECS);
        let _ = mcp_store::prune_resolved_approvals(&self.conn, cutoff)?;
        Ok(())
    }

    /// 크래시 orphan pending 정리 — cutoff보다 오래된 pending을 denied로. 반영 행 수 반환.
    pub fn expire_pending_approvals(
        &self,
        older_than_epoch_secs: i64,
        resolved_at: i64,
    ) -> anyhow::Result<usize> {
        mcp_store::expire_pending_approvals(&self.conn, older_than_epoch_secs, resolved_at)
    }

    pub fn prune_resolved_approvals(
        &self,
        resolved_before_epoch_secs: i64,
    ) -> anyhow::Result<usize> {
        mcp_store::prune_resolved_approvals(&self.conn, resolved_before_epoch_secs)
    }

    /// tool 실행 감사 기록 (PR-16). encryptor를 넘기면 전체 입력이 암호화 저장된다 (§7).
    pub fn record_tool_audit(
        &self,
        record: &audit::AuditRecord<'_>,
        redaction: &secret::RedactionService,
        encryptor: Option<&dyn secret::SecretStore>,
    ) -> anyhow::Result<String> {
        audit::record_audit(&self.conn, redaction, record, encryptor)
    }

    /// 앱 시작 시 crash recovery (설계문서 PR-14): 이전 실행이 남긴 세션 중
    /// exited가 아닌 것을 모두 Exited로 마킹한다 — 재시작 후엔 그 프로세스가
    /// 반드시 orphan(죽음)이기 때문. 반영된 행 수를 돌려준다.
    pub fn reconcile_orphan_sessions(&self) -> anyhow::Result<usize> {
        persist::reconcile_orphan_sessions(&self.conn)
    }

    /// env profile 생성. is_production은 kind에서 파생한다 (설계문서 6.4).
    pub fn insert_env_profile(
        &self,
        workspace_id: &str,
        name: &str,
        kind: &str,
    ) -> anyhow::Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.conn
            .execute(
                "INSERT INTO env_profiles (id, workspace_id, name, kind, is_production, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5,
                    strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
                (&id, workspace_id, name, kind, kind == "production"),
            )
            .with_context(|| format!("env profile 저장 실패: {name}"))?;
        Ok(id)
    }

    pub fn list_env_profiles(&self, workspace_id: &str) -> anyhow::Result<Vec<EnvProfileRow>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, name, kind, is_production FROM env_profiles
             WHERE workspace_id = ?1 ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([workspace_id], |row| {
            Ok(EnvProfileRow {
                id: row.get(0)?,
                name: row.get(1)?,
                kind: row.get(2)?,
                is_production: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// profile과 소속 env var를 한 트랜잭션으로 삭제한다.
    pub fn delete_env_profile(&mut self, id: &str) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM env_vars WHERE profile_id = ?1", [id])?;
        tx.execute("DELETE FROM env_profiles WHERE id = ?1", [id])?;
        tx.commit()
            .with_context(|| format!("env profile 삭제 실패: {id}"))
    }

    /// env var 추가/갱신. secret은 credential_id만 저장한다 (설계문서 6.3) —
    /// EnvValue 타입 + DDL CHECK로 이중 강제.
    pub fn upsert_env_var(
        &self,
        profile_id: &str,
        key: &str,
        value: &EnvValue,
    ) -> anyhow::Result<()> {
        Self::validate_env_var_for_persistence(key, value)?;
        let (kind, plain_value, credential_id) = match value {
            EnvValue::Plain(v) => ("plain", Some(v.as_str()), None),
            EnvValue::Secret { credential_id } => ("secret", None, Some(credential_id.as_str())),
        };
        self.conn
            .execute(
                "INSERT INTO env_vars (id, profile_id, key, kind, plain_value, credential_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6,
                    strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))
                 ON CONFLICT(profile_id, key) DO UPDATE SET
                    kind = excluded.kind,
                    plain_value = excluded.plain_value,
                    credential_id = excluded.credential_id,
                    updated_at = excluded.updated_at",
                (
                    uuid::Uuid::new_v4().to_string(),
                    profile_id,
                    key,
                    kind,
                    plain_value,
                    credential_id,
                ),
            )
            .with_context(|| format!("env var 저장 실패: {key}"))?;
        Ok(())
    }

    pub fn list_env_vars(&self, profile_id: &str) -> anyhow::Result<Vec<EnvVarRow>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT key, kind, plain_value, credential_id FROM env_vars
             WHERE profile_id = ?1 ORDER BY key",
        )?;
        let rows = stmt.query_map([profile_id], |row| {
            let key: String = row.get(0)?;
            let kind: String = row.get(1)?;
            let value = if kind == "secret" {
                EnvValue::Secret {
                    credential_id: row.get(3)?,
                }
            } else {
                EnvValue::Plain(row.get::<_, Option<String>>(2)?.unwrap_or_default())
            };
            Ok(EnvVarRow { key, value })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn delete_env_var(&self, profile_id: &str, key: &str) -> anyhow::Result<()> {
        self.conn
            .execute(
                "DELETE FROM env_vars WHERE profile_id = ?1 AND key = ?2",
                [profile_id, key],
            )
            .with_context(|| format!("env var 삭제 실패: {key}"))?;
        Ok(())
    }

    pub fn validate_env_var_for_persistence(key: &str, value: &EnvValue) -> anyhow::Result<()> {
        if let EnvValue::Plain(plain) = value
            && let Some(reason) = secret_like_env_plain_reason(key, plain)
        {
            anyhow::bail!(
                "{} '{}'는 EnvValue::Plain으로 저장할 수 없습니다. credential secret으로 저장하세요",
                reason.label(),
                key
            );
        }
        Ok(())
    }

    /// agent config 등록 (설계문서 PR-09/12). args는 array로만 저장한다 (완료 기준).
    #[allow(clippy::too_many_arguments)]
    pub fn insert_agent_config(
        &self,
        name: &str,
        command: &str,
        args: &[String],
        waiting_regex: Option<&str>,
        approval_regex: Option<&str>,
        error_regex: Option<&str>,
        done_regex: Option<&str>,
        mcp_proxy_enabled: bool,
        mcp_proxy_server_id: Option<&str>,
        mcp_config_flag: Option<&str>,
    ) -> anyhow::Result<String> {
        Self::validate_agent_args_for_persistence(args)?;
        let id = uuid::Uuid::new_v4().to_string();
        let args_json = serde_json::to_string(args)?;
        // 빈 문자열은 None으로 정규화 (mcp_proxy_server_id와 동일 관례 — stale/빈값 저장 방지)
        let mcp_config_flag = mcp_config_flag.filter(|s| !s.is_empty());
        self.conn
            .execute(
                "INSERT INTO agent_configs
                   (id, name, command, args_json,
                    waiting_regex, approval_regex, error_regex, done_regex,
                    mcp_proxy_enabled, mcp_proxy_server_id, mcp_config_flag,
                    created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                    strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
                (
                    &id,
                    name,
                    command,
                    &args_json,
                    waiting_regex,
                    approval_regex,
                    error_regex,
                    done_regex,
                    mcp_proxy_enabled as i64,
                    mcp_proxy_server_id,
                    mcp_config_flag,
                ),
            )
            .with_context(|| format!("agent config 저장 실패: {name}"))?;
        Ok(id)
    }

    pub fn validate_agent_args_for_persistence(args: &[String]) -> anyhow::Result<()> {
        validate_args_for_persistence(args, "agent args")
    }

    pub fn list_agent_configs(&self) -> anyhow::Result<Vec<AgentConfigRow>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, name, command, args_json,
                    waiting_regex, approval_regex, error_regex, done_regex,
                    mcp_proxy_enabled, mcp_proxy_server_id, mcp_config_flag
             FROM agent_configs WHERE deleted_at IS NULL ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
            ))
        })?;
        // 손상 행 하나가 전체 목록을 죽이지 않게 skip + 경고 (원문은 로그에 남기지 않음)
        let mut out = Vec::new();
        for row in rows {
            let (
                id,
                name,
                command,
                args_json,
                waiting,
                approval,
                error,
                done,
                proxy_on,
                proxy_srv,
                config_flag,
            ) = row?;
            match serde_json::from_str(&args_json) {
                Ok(args) => out.push(AgentConfigRow {
                    id,
                    name,
                    command,
                    args,
                    waiting_regex: waiting.filter(|s| !s.is_empty()),
                    approval_regex: approval.filter(|s| !s.is_empty()),
                    error_regex: error.filter(|s| !s.is_empty()),
                    done_regex: done.filter(|s| !s.is_empty()),
                    mcp_proxy_enabled: proxy_on != 0,
                    mcp_proxy_server_id: proxy_srv.filter(|s| !s.is_empty()),
                    mcp_config_flag: config_flag.filter(|s| !s.is_empty()),
                }),
                Err(e) => tracing::warn!(agent_id = %id, "args_json 파싱 실패 — 행 무시: {e}"),
            }
        }
        Ok(out)
    }

    /// soft-delete — 실행 이력(sessions.agent_id FK)이 있어도 항상 성공한다.
    pub fn delete_agent_config(&self, id: &str) -> anyhow::Result<()> {
        self.conn
            .execute(
                "UPDATE agent_configs
                 SET deleted_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE id = ?1 AND deleted_at IS NULL",
                [id],
            )
            .with_context(|| format!("agent config 삭제 실패: {id}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_workspace는_자식env와_workspace만_지운다() {
        let mut db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("삭제대상").unwrap();
        let other = db.create_workspace("유지").unwrap();
        let profile = db.insert_env_profile(&ws, "dev", "custom").unwrap();
        db.upsert_env_var(&profile, "K", &EnvValue::Plain("v".into()))
            .unwrap();
        db.insert_env_profile(&other, "dev", "custom").unwrap();

        db.delete_workspace(&ws).unwrap();
        // 삭제 대상: workspace·env 모두 사라짐
        assert!(db.list_workspaces().unwrap().iter().all(|w| w.id != ws));
        assert!(db.list_env_profiles(&ws).unwrap().is_empty());
        assert!(db.list_env_vars(&profile).unwrap().is_empty());
        // 다른 workspace는 그대로
        assert!(db.list_workspaces().unwrap().iter().any(|w| w.id == other));
        assert_eq!(db.list_env_profiles(&other).unwrap().len(), 1);
    }

    #[test]
    fn agent_sessions_upsert_list_delete_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("ws").unwrap();
        db.upsert_agent_session(&ws, "pane-1", "claude", "sid-a")
            .unwrap();
        db.upsert_agent_session(&ws, "pane-2", "codex", "sid-b")
            .unwrap();
        let rows = db.list_agent_sessions(&ws).unwrap();
        assert_eq!(rows.len(), 2);
        // upsert는 같은 pane을 교체 (중복 아님)
        db.upsert_agent_session(&ws, "pane-1", "claude", "sid-a2")
            .unwrap();
        let rows = db.list_agent_sessions(&ws).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .any(|r| r.pane_id == "pane-1" && r.session_id == "sid-a2")
        );
        // 개별 삭제
        db.delete_agent_session(&ws, "pane-1").unwrap();
        assert_eq!(db.list_agent_sessions(&ws).unwrap().len(), 1);
    }

    #[test]
    fn agent_needs_input_set_clear_list() {
        let db = Db::open_in_memory().unwrap();
        db.set_agent_needs_input("pane-1", true).unwrap();
        db.set_agent_needs_input("pane-2", true).unwrap();
        db.set_agent_needs_input("pane-3", false).unwrap();
        let mut waiting = db.list_waiting_sessions().unwrap();
        waiting.sort();
        assert_eq!(waiting, vec!["pane-1".to_string(), "pane-2".to_string()]);
        // clear → 목록에서 빠짐
        db.set_agent_needs_input("pane-1", false).unwrap();
        assert_eq!(
            db.list_waiting_sessions().unwrap(),
            vec!["pane-2".to_string()]
        );
    }

    #[test]
    fn delete_workspace가_agent_sessions도_정리() {
        let mut db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("삭제대상").unwrap();
        let other = db.create_workspace("유지").unwrap();
        db.upsert_agent_session(&ws, "p1", "claude", "s1").unwrap();
        db.upsert_agent_session(&other, "p2", "codex", "s2")
            .unwrap();
        db.delete_workspace(&ws).unwrap();
        assert!(db.list_agent_sessions(&ws).unwrap().is_empty());
        assert_eq!(db.list_agent_sessions(&other).unwrap().len(), 1);
    }

    #[test]
    fn 권한규칙_upsert_list_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_permission_rule("srv-1", "read_file", "allow", Some("a".repeat(64).as_str()))
            .unwrap();
        db.upsert_permission_rule("srv-1", "delete_file", "deny", None)
            .unwrap();
        // 같은 키 재저장은 갱신 (중복 아님)
        db.upsert_permission_rule("srv-1", "read_file", "deny", None)
            .unwrap();
        let mut rows = db.list_permission_rules().unwrap();
        rows.sort_by(|a, b| a.tool_name.cmp(&b.tool_name));
        assert_eq!(rows.len(), 2);
        let read = rows.iter().find(|r| r.tool_name == "read_file").unwrap();
        assert_eq!(read.rule, "deny");
        assert_eq!(read.approved_schema_hash, None);
        let del = rows.iter().find(|r| r.tool_name == "delete_file").unwrap();
        assert_eq!(del.rule, "deny");
        // 삭제(Ask 재설정) → 행이 사라진다
        db.delete_permission_rule("srv-1", "read_file").unwrap();
        let after = db.list_permission_rules().unwrap();
        assert_eq!(after.len(), 1);
        assert!(after.iter().all(|r| r.tool_name != "read_file"));
    }

    #[test]
    fn pending_approval_insert_poll_resolve_라이프사이클() {
        let db = Db::open_in_memory().unwrap();
        db.insert_pending_approval("req-1", "srv-1", "read_file", "path=/tmp/x", Some("h"), 100)
            .unwrap();
        // insert 직후엔 pending
        assert_eq!(
            db.poll_approval("req-1").unwrap(),
            ApprovalOutcome {
                status: ApprovalStatus::Pending,
                remember: false,
            }
        );
        // allow + remember로 해소 → poll이 반영
        db.resolve_approval("req-1", true, true, 200).unwrap();
        assert_eq!(
            db.poll_approval("req-1").unwrap(),
            ApprovalOutcome {
                status: ApprovalStatus::Allowed,
                remember: true,
            }
        );

        // deny 경로도 확인
        db.insert_pending_approval("req-2", "srv-1", "delete_file", "path=/tmp/y", None, 101)
            .unwrap();
        db.resolve_approval("req-2", false, false, 201).unwrap();
        assert_eq!(
            db.poll_approval("req-2").unwrap(),
            ApprovalOutcome {
                status: ApprovalStatus::Denied,
                remember: false,
            }
        );
    }

    #[test]
    fn list_pending은_pending만_오래된순으로() {
        let db = Db::open_in_memory().unwrap();
        // 일부러 뒤섞인 created_at으로 넣어 정렬을 검증
        db.insert_pending_approval("b", "srv", "t", "prev", None, 300)
            .unwrap();
        db.insert_pending_approval("a", "srv", "t", "prev", Some("hh"), 100)
            .unwrap();
        db.insert_pending_approval("c", "srv", "t", "prev", None, 200)
            .unwrap();
        // c를 해소하면 목록에서 빠진다
        db.resolve_approval("c", true, false, 400).unwrap();

        let rows = db.list_pending_approvals().unwrap();
        let ids: Vec<_> = rows.iter().map(|r| r.id.as_str()).collect();
        // pending인 a(100), b(300)만, 오래된 순 → a, b
        assert_eq!(ids, vec!["a", "b"]);
        assert_eq!(rows[0].schema_hash.as_deref(), Some("hh"));
        assert_eq!(rows[1].schema_hash, None);
        assert_eq!(rows[0].created_at, 100);
    }

    #[test]
    fn resolve_두번은_먼저_쓴_결정을_안_덮는다() {
        let db = Db::open_in_memory().unwrap();
        db.insert_pending_approval("req", "srv", "t", "prev", None, 100)
            .unwrap();
        db.resolve_approval("req", true, true, 200).unwrap();
        // 두 번째 해소(deny)는 no-op — 첫 결정(allow/remember) 유지
        db.resolve_approval("req", false, false, 300).unwrap();
        assert_eq!(
            db.poll_approval("req").unwrap(),
            ApprovalOutcome {
                status: ApprovalStatus::Allowed,
                remember: true,
            }
        );
        // 이미 해소된 행은 목록에 없음
        assert!(db.list_pending_approvals().unwrap().is_empty());
    }

    #[test]
    fn expire_pending은_오래된_행만_denied로_하고_멱등() {
        let db = Db::open_in_memory().unwrap();
        db.insert_pending_approval("old", "srv", "t", "prev", None, 100)
            .unwrap();
        db.insert_pending_approval("recent", "srv", "t", "prev", None, 1000)
            .unwrap();
        // cutoff=500 → old(100)만 만료, recent(1000)은 유지
        assert_eq!(db.expire_pending_approvals(500, 600).unwrap(), 1);
        let ids: Vec<_> = db
            .list_pending_approvals()
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, vec!["recent"]);
        // old는 denied(resolved)로 전이
        assert_eq!(
            db.poll_approval("old").unwrap().status,
            ApprovalStatus::Denied
        );
        // 멱등: 재호출은 더 이상 pending이 없어 0
        assert_eq!(db.expire_pending_approvals(500, 700).unwrap(), 0);
    }

    #[test]
    fn resolve_approval은_오래된_resolved_rows를_prune한다() {
        let db = Db::open_in_memory().unwrap();
        db.insert_pending_approval("old", "srv", "t", "prev", None, 10)
            .unwrap();
        db.insert_pending_approval("keep", "srv", "t", "prev", None, 20)
            .unwrap();
        db.resolve_approval("old", true, false, 100).unwrap();
        db.resolve_approval(
            "keep",
            true,
            false,
            100 + RESOLVED_APPROVAL_RETENTION_SECS + 1,
        )
        .unwrap();

        assert!(db.poll_approval("old").is_err());
        assert_eq!(
            db.poll_approval("keep").unwrap().status,
            ApprovalStatus::Allowed
        );
    }

    #[test]
    fn poll_없는_id는_에러() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.poll_approval("nope").is_err());
        // 없는 id resolve는 조용한 no-op (Ok) — 여전히 행이 없으니 poll은 에러
        db.resolve_approval("nope", true, false, 100).unwrap();
        assert!(db.poll_approval("nope").is_err());
    }

    #[test]
    fn workspace_생성_목록_기본포함() {
        let db = Db::open_in_memory().unwrap();
        let default_id = db.ensure_default_workspace().unwrap();
        let a = db.create_workspace("프로젝트 A").unwrap();
        let list = db.list_workspaces().unwrap();
        // 기본 + 새 workspace 모두 목록에 (생성순)
        let ids: Vec<_> = list.iter().map(|w| w.id.as_str()).collect();
        assert!(ids.contains(&default_id.as_str()));
        assert!(ids.contains(&a.as_str()));
        let created = list.iter().find(|w| w.id == a).unwrap();
        assert_eq!(created.name, "프로젝트 A");
        // 생성 직후 path는 미설정(빈 문자열)
        assert_eq!(created.path, "");
    }

    #[test]
    fn workspace_path_설정_조회_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("트리 대상").unwrap();
        // 미설정: Some("")
        assert_eq!(db.workspace_path(&ws).unwrap().as_deref(), Some(""));
        db.set_workspace_path(&ws, "/Users/me/projects/demo")
            .unwrap();
        assert_eq!(
            db.workspace_path(&ws).unwrap().as_deref(),
            Some("/Users/me/projects/demo")
        );
        // 목록에도 반영된다
        let list = db.list_workspaces().unwrap();
        let row = list.iter().find(|w| w.id == ws).unwrap();
        assert_eq!(row.path, "/Users/me/projects/demo");
        // 없는 id: set은 에러, 조회는 None
        assert!(db.set_workspace_path("nope", "/x").is_err());
        assert_eq!(db.workspace_path("nope").unwrap(), None);
    }

    fn sample(id: &str) -> CredentialMeta {
        CredentialMeta {
            id: id.into(),
            provider: "anthropic".into(),
            label: "개인 키".into(),
            credential_kind: "api_key".into(),
            masked_hint: Some("****3456".into()),
        }
    }

    /// PR-22 완료 기준 "secret이 DB에 없음" — 파일 바이트 레벨 스캔.
    /// credential 저장/env secret 참조/agent 등록의 전 경로를 지난 뒤
    /// SQLite 파일 원문에 secret 평문이 없어야 한다 (metadata는 keyring 좌표만 — §6.3).
    #[test]
    fn db_파일에_secret_평문이_없다() {
        const SECRET: &str = "sk-live-plaintext-must-not-touch-disk-9x8y7z";
        let dir = std::env::temp_dir().join(format!("deppy-secscan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        {
            let db = Db::open(&path).unwrap();
            let ws = db.ensure_default_workspace().unwrap();
            // credential: 값은 keyring으로 가고 DB에는 좌표/hint만 — 여기서는
            // 실제 UI 경로가 그러듯 metadata만 넣는다 (hint는 마스킹된 문자열)
            db.insert_credential(&CredentialMeta {
                id: "cred-scan".into(),
                provider: "test".into(),
                label: "scan".into(),
                credential_kind: "api_key".into(),
                masked_hint: Some(secret::masked_hint(SECRET)),
            })
            .unwrap();
            // env secret은 credential_id 참조만 저장된다
            let profile = db.insert_env_profile(&ws, "prod", "production").unwrap();
            db.upsert_env_var(
                &profile,
                "API_KEY",
                &EnvValue::Secret {
                    credential_id: "cred-scan".into(),
                },
            )
            .unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        let haystack = String::from_utf8_lossy(&bytes);
        assert!(!haystack.contains(SECRET), "DB 파일에 secret 평문이 있다");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn credential_crud_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        db.insert_credential(&sample("cred-1")).unwrap();
        db.insert_credential(&sample("cred-2")).unwrap();
        let listed = db.list_credentials().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0], sample("cred-1"));

        assert!(db.delete_credential_if_unused("cred-1").unwrap());
        let listed = db.list_credentials().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "cred-2");
        // 없는 id는 false (참조 중과 동일하게 "안 지움")
        assert!(!db.delete_credential_if_unused("cred-1").unwrap());
    }

    #[test]
    fn env_value_debug는_plain과_credential_id를_숨긴다() {
        let plain = EnvValue::Plain("safe-but-still-runtime-env-value".to_owned());
        let secret = EnvValue::Secret {
            credential_id: "cred-debug-never-log".to_owned(),
        };

        let plain_debug = format!("{plain:?}");
        let secret_debug = format!("{secret:?}");

        assert!(!plain_debug.contains("safe-but-still-runtime-env-value"));
        assert!(plain_debug.contains("[REDACTED_PLAIN]"));
        assert!(!secret_debug.contains("cred-debug-never-log"));
        assert!(secret_debug.contains("[REDACTED_CREDENTIAL_ID]"));
    }

    fn 모든_버전_prefix에서_최신까지_마이그레이션되고_fk_정합(k: usize) {
        // v2.8 §11.2 smoke-db-migrations: 임의 구버전(user_version=k) DB가 최신으로
        // 올라가고 foreign_key_check가 깨끗해야 한다. 마이그레이션 재배열/번호 변경을
        // 회귀로 잡는 가드 — registry 전환 시 legacy 슬롯 고정의 안전망.
        let dir = std::env::temp_dir().join(format!("deppy-mig-prefix-{}-{k}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..k] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", k as i64).unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(Db::read_user_version(&db.conn).unwrap(), MIGRATIONS.len());
        // FK 정합: 위반 행이 하나도 없어야 한다
        let violations: i64 = db
            .conn
            .prepare("SELECT count(*) FROM pragma_foreign_key_check")
            .unwrap()
            .query_row([], |row| row.get(0))
            .unwrap();
        assert_eq!(violations, 0, "v{k}→최신 마이그레이션 후 FK 위반");
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn 전_버전_prefix_마이그레이션_스모크() {
        for k in 0..=MIGRATIONS.len() {
            모든_버전_prefix에서_최신까지_마이그레이션되고_fk_정합(k);
        }
    }

    #[test]
    fn 마이그레이션은_멱등() {
        let dir = std::env::temp_dir().join(format!("deppy-sijo-db-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        {
            let db = Db::open(&path).unwrap();
            db.insert_credential(&sample("cred-1")).unwrap();
        }
        // 재오픈 시 기존 데이터 유지 + 마이그레이션 재실행 없음
        let db = Db::open(&path).unwrap();
        assert_eq!(db.list_credentials().unwrap().len(), 1);
        drop(db); // Windows: 파일 핸들을 닫아야 삭제 가능
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn 최신_db_재오픈은_마이그레이션_noop() {
        let dir = std::env::temp_dir().join(format!("deppy-mig-noop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        {
            // 최초 오픈이 최신까지 마이그레이션
            let db = Db::open(&path).unwrap();
            assert_eq!(Db::read_user_version(&db.conn).unwrap(), MIGRATIONS.len());
        }
        // 재오픈: fast-path no-op, user_version 유지
        let db = Db::open(&path).unwrap();
        assert_eq!(Db::read_user_version(&db.conn).unwrap(), MIGRATIONS.len());
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn v8에서_v9로_업그레이드된다() {
        let dir = std::env::temp_dir().join(format!("deppy-mig-8to9-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        // user_version=8 (pending_approvals 이전) 구버전 DB 구성
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..8] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 8).unwrap();
        }
        // 오픈 → IMMEDIATE 트랜잭션으로 migration 9 적용
        let db = Db::open(&path).unwrap();
        assert_eq!(Db::read_user_version(&db.conn).unwrap(), MIGRATIONS.len());
        // pending_approvals 테이블이 실제로 사용 가능
        db.insert_pending_approval("r", "s", "t", "prev", None, 1)
            .unwrap();
        assert_eq!(
            db.poll_approval("r").unwrap().status,
            ApprovalStatus::Pending
        );
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn env_profile과_var_roundtrip() {
        let mut db = Db::open_in_memory().unwrap();
        let ws = db.ensure_default_workspace().unwrap();
        // 멱등: 재호출 시 같은 workspace
        assert_eq!(db.ensure_default_workspace().unwrap(), ws);

        let local = db.insert_env_profile(&ws, "로컬", "local").unwrap();
        let prod = db.insert_env_profile(&ws, "운영", "production").unwrap();
        let profiles = db.list_env_profiles(&ws).unwrap();
        assert_eq!(profiles.len(), 2);
        let by_id = |id: &str| profiles.iter().find(|p| p.id == id).unwrap();
        assert!(!by_id(&local).is_production);
        assert!(by_id(&prod).is_production); // kind에서 파생

        db.insert_credential(&sample("cred-1")).unwrap();
        db.upsert_env_var(&local, "PORT", &EnvValue::Plain("3000".into()))
            .unwrap();
        db.upsert_env_var(
            &local,
            "API_KEY",
            &EnvValue::Secret {
                credential_id: "cred-1".into(),
            },
        )
        .unwrap();
        let vars = db.list_env_vars(&local).unwrap();
        assert_eq!(vars.len(), 2);
        assert_eq!(
            vars[0],
            EnvVarRow {
                key: "API_KEY".into(),
                value: EnvValue::Secret {
                    credential_id: "cred-1".into()
                }
            }
        );

        // upsert: 같은 key 갱신
        db.upsert_env_var(&local, "PORT", &EnvValue::Plain("8080".into()))
            .unwrap();
        let vars = db.list_env_vars(&local).unwrap();
        assert_eq!(vars[1].value, EnvValue::Plain("8080".into()));

        db.delete_env_var(&local, "PORT").unwrap();
        assert_eq!(db.list_env_vars(&local).unwrap().len(), 1);

        db.delete_env_profile(&local).unwrap();
        assert_eq!(db.list_env_profiles(&ws).unwrap().len(), 1);
        assert_eq!(db.list_env_profiles(&ws).unwrap()[0].id, prod);
    }

    #[test]
    fn secret_var는_존재하는_credential만_참조한다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.ensure_default_workspace().unwrap();
        let profile = db.insert_env_profile(&ws, "로컬", "local").unwrap();
        // FK 강제(설계문서 11.9): 없는 credential_id는 거부
        assert!(
            db.upsert_env_var(
                &profile,
                "API_KEY",
                &EnvValue::Secret {
                    credential_id: "cred-none".into()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn secret_like_env_key는_plain_저장을_거부하고_secret은_허용한다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.ensure_default_workspace().unwrap();
        let profile = db.insert_env_profile(&ws, "로컬", "local").unwrap();
        db.insert_credential(&sample("cred-api")).unwrap();

        for key in ["API_KEY", "DATABASE_URL", "AUTHORIZATION", "AUTH_TOKEN"] {
            let err = db
                .upsert_env_var(
                    &profile,
                    key,
                    &EnvValue::Plain(format!("sk-live-env-secret-never-persisted-{key}")),
                )
                .unwrap_err()
                .to_string();
            assert!(err.contains("EnvValue::Plain"));
            let count: i64 = db
                .conn
                .query_row(
                    "SELECT count(*) FROM env_vars WHERE profile_id = ?1 AND key = ?2",
                    (&profile, key),
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 0, "{key} should not be persisted as plain");
        }

        db.upsert_env_var(
            &profile,
            "API_KEY",
            &EnvValue::Secret {
                credential_id: "cred-api".into(),
            },
        )
        .unwrap();
        let (kind, plain_value, credential_id): (String, Option<String>, Option<String>) = db
            .conn
            .query_row(
                "SELECT kind, plain_value, credential_id FROM env_vars
                 WHERE profile_id = ?1 AND key = 'API_KEY'",
                [&profile],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(kind, "secret");
        assert_eq!(plain_value, None);
        assert_eq!(credential_id.as_deref(), Some("cred-api"));
    }

    #[test]
    fn secret_like_env_value는_plain_value에_남지_않는다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.ensure_default_workspace().unwrap();
        let profile = db.insert_env_profile(&ws, "로컬", "local").unwrap();
        let secret = "Authorization: Bearer sk-env-value-never-persisted";

        assert!(
            db.upsert_env_var(&profile, "SAFE_NAME", &EnvValue::Plain(secret.into()))
                .is_err()
        );
        let plain_values: Vec<String> = db
            .conn
            .prepare("SELECT plain_value FROM env_vars")
            .unwrap()
            .query_map([], |row| row.get::<_, Option<String>>(0))
            .unwrap()
            .map(|v| v.unwrap().unwrap_or_default())
            .collect();
        assert!(plain_values.iter().all(|v| !v.contains("sk-env-value")));
    }

    #[test]
    fn env_var가_참조하는_credential은_삭제_거부() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.ensure_default_workspace().unwrap();
        let profile = db.insert_env_profile(&ws, "로컬", "local").unwrap();
        db.insert_credential(&sample("cred-1")).unwrap();
        db.upsert_env_var(
            &profile,
            "API_KEY",
            &EnvValue::Secret {
                credential_id: "cred-1".into(),
            },
        )
        .unwrap();
        // 원자 삭제: 참조 중이면 지우지 않는다 (확인+삭제 한 문장 — TOCTOU 없음)
        assert!(db.credential_in_use("cred-1").unwrap());
        assert!(!db.delete_credential_if_unused("cred-1").unwrap());
        assert_eq!(db.list_credentials().unwrap().len(), 1); // 그대로
        db.delete_env_var(&profile, "API_KEY").unwrap();
        assert!(!db.credential_in_use("cred-1").unwrap());
        assert!(db.delete_credential_if_unused("cred-1").unwrap());
    }

    #[test]
    fn mcp_scoped_env가_참조하는_credential은_삭제_거부() {
        let db = Db::open_in_memory().unwrap();
        db.insert_credential(&sample("cred-1")).unwrap();
        db.insert_mcp_server(&mcp_store::McpServerRow {
            id: "srv-1".to_owned(),
            name: "mock".to_owned(),
            kind: "stdio".to_owned(),
            command: Some("/bin/sh".to_owned()),
            args: Vec::new(),
            env_plain: Vec::new(),
            env_secrets: vec![("MCP_TOKEN".to_owned(), "cred-1".to_owned())],
            inherit_env: true,
            url: None,
            enabled: true,
        })
        .unwrap();

        assert!(db.credential_in_use("cred-1").unwrap());
        assert!(!db.delete_credential_if_unused("cred-1").unwrap());
        assert_eq!(db.list_credentials().unwrap().len(), 1);
    }

    #[test]
    fn ddl_check가_secret_평문을_거부한다() {
        // repository 타입(EnvValue)으로는 표현 불가한 조합을 raw SQL로 검증 (이중 강제)
        let db = Db::open_in_memory().unwrap();
        let ws = db.ensure_default_workspace().unwrap();
        let profile = db.insert_env_profile(&ws, "로컬", "local").unwrap();
        let result = db.conn.execute(
            "INSERT INTO env_vars (id, profile_id, key, kind, plain_value, credential_id, created_at, updated_at)
             VALUES ('v1', ?1, 'API_KEY', 'secret', 'leaked-plaintext', NULL, '', '')",
            [&profile],
        );
        assert!(result.is_err());
    }

    #[test]
    fn 마이그레이션_전_백업이_생성된다() {
        let dir = std::env::temp_dir().join(format!("deppy-sijo-bak-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        // 버전 1(credentials만)인 구버전 DB를 만든다
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert!(path.with_extension("sqlite3.bak").exists());
        // 마이그레이션 완료 후 env 테이블 사용 가능
        db.ensure_default_workspace().unwrap();
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn agent_config_crud와_args_array() {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .insert_agent_config(
                "빌드 에이전트",
                "cargo",
                &["build".into(), "--release".into()],
                Some("입력 대기"),
                None,
                Some("(?i)error"),
                None,
                false,
                None,
                None,
            )
            .unwrap();
        let listed = db.list_agent_configs().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].command, "cargo");
        // args는 array로 저장·복원된다 (완료 기준: 셸 문자열 금지)
        assert_eq!(
            listed[0].args,
            vec!["build".to_owned(), "--release".to_owned()]
        );
        // 기본(구식) insert는 proxy 미사용 (0/NULL)으로 복원된다
        assert!(!listed[0].mcp_proxy_enabled);
        assert_eq!(listed[0].mcp_proxy_server_id, None);
        // 플래그 미지정은 None (기본 --mcp-config 사용을 의미)
        assert_eq!(listed[0].mcp_config_flag, None);
        db.delete_agent_config(&id).unwrap();
        assert!(db.list_agent_configs().unwrap().is_empty());
    }

    #[test]
    fn agent_args_secret_like_payload는_args_json에_저장되지_않는다() {
        let db = Db::open_in_memory().unwrap();
        let secret = "sk-agent-plaintext-never-persisted";
        let args = vec!["--api-key".to_owned(), secret.to_owned()];

        assert!(
            db.insert_agent_config(
                "위험 에이전트",
                "agent",
                &args,
                None,
                None,
                None,
                None,
                false,
                None,
                None,
            )
            .is_err()
        );

        let rows: Vec<String> = db
            .conn
            .prepare("SELECT args_json FROM agent_configs")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(rows.is_empty());
        assert!(rows.iter().all(|json| !json.contains(secret)));
    }

    #[test]
    fn agent_args_한줄_secret_flag와_database_url_flag를_거부한다() {
        let db = Db::open_in_memory().unwrap();
        let cases = [
            "--api-key sk-agent-inline-never-persisted",
            "--database-url=postgres://user:pass@localhost/app",
            "OPENAI_API_KEY=sk-agent-assignment-never-persisted",
        ];

        for (idx, arg) in cases.iter().enumerate() {
            assert!(
                db.insert_agent_config(
                    &format!("위험 에이전트 {idx}"),
                    "agent",
                    &[arg.to_string()],
                    None,
                    None,
                    None,
                    None,
                    false,
                    None,
                    None,
                )
                .is_err(),
                "{arg}"
            );
        }

        let count: i64 = db
            .conn
            .query_row("SELECT count(*) FROM agent_configs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn agent_config_mcp_proxy_필드_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        db.insert_agent_config(
            "프록시 에이전트",
            "claude",
            &["--dangerously".into()],
            None,
            None,
            None,
            None,
            true,
            Some("srv-backend"),
            None,
        )
        .unwrap();
        let listed = db.list_agent_configs().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].mcp_proxy_enabled);
        assert_eq!(
            listed[0].mcp_proxy_server_id.as_deref(),
            Some("srv-backend")
        );
    }

    #[test]
    fn agent_config_mcp_config_flag_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        // Some(플래그): 저장·복원된다
        db.insert_agent_config(
            "커스텀 플래그",
            "gemini",
            &[],
            None,
            None,
            None,
            None,
            true,
            Some("srv-backend"),
            Some("--mcp-config-file"),
        )
        .unwrap();
        // 빈 문자열은 None으로 정규화된다 (mcp_proxy_server_id와 동일 관례)
        db.insert_agent_config(
            "빈 플래그",
            "claude",
            &[],
            None,
            None,
            None,
            None,
            false,
            None,
            Some(""),
        )
        .unwrap();
        let listed = db.list_agent_configs().unwrap();
        assert_eq!(listed.len(), 2);
        let custom = listed.iter().find(|r| r.name == "커스텀 플래그").unwrap();
        assert_eq!(custom.mcp_config_flag.as_deref(), Some("--mcp-config-file"));
        let empty = listed.iter().find(|r| r.name == "빈 플래그").unwrap();
        assert_eq!(empty.mcp_config_flag, None);
    }

    #[test]
    fn v9에서_v10으로_업그레이드된다() {
        let dir = std::env::temp_dir().join(format!("deppy-mig-9to10-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        // user_version=9 (mcp_proxy_* 이전) 구버전 DB 구성
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..9] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 9).unwrap();
        }
        // 오픈 → IMMEDIATE 트랜잭션으로 migration 10 적용
        let db = Db::open(&path).unwrap();
        assert_eq!(Db::read_user_version(&db.conn).unwrap(), MIGRATIONS.len());
        // 새 컬럼이 실제로 사용 가능 (insert/list round-trip)
        db.insert_agent_config("a", "c", &[], None, None, None, None, true, Some("s"), None)
            .unwrap();
        let listed = db.list_agent_configs().unwrap();
        assert!(listed[0].mcp_proxy_enabled);
        assert_eq!(listed[0].mcp_proxy_server_id.as_deref(), Some("s"));
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn v10에서_v11로_업그레이드된다() {
        let dir = std::env::temp_dir().join(format!("deppy-mig-10to11-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        // user_version=10 (mcp_config_flag 이전) 구버전 DB 구성
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..10] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 10).unwrap();
        }
        // 오픈 → IMMEDIATE 트랜잭션으로 migration 11 적용
        let db = Db::open(&path).unwrap();
        assert_eq!(Db::read_user_version(&db.conn).unwrap(), MIGRATIONS.len());
        // 새 컬럼이 실제로 사용 가능 (insert/list round-trip)
        db.insert_agent_config(
            "a",
            "c",
            &[],
            None,
            None,
            None,
            None,
            true,
            Some("s"),
            Some("--cfg"),
        )
        .unwrap();
        let listed = db.list_agent_configs().unwrap();
        assert_eq!(listed[0].mcp_config_flag.as_deref(), Some("--cfg"));
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn v11에서_v12로_업그레이드된다() {
        let dir = std::env::temp_dir().join(format!("deppy-mig-11to12-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..11] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 11).unwrap();
        }

        let db = Db::open(&path).unwrap();
        assert_eq!(Db::read_user_version(&db.conn).unwrap(), MIGRATIONS.len());
        db.insert_mcp_server(&mcp_store::McpServerRow {
            id: "srv-env".to_owned(),
            name: "env server".to_owned(),
            kind: "stdio".to_owned(),
            command: Some("/bin/sh".to_owned()),
            args: vec!["-lc".to_owned(), "true".to_owned()],
            env_plain: vec![("MCP_SAFE_MODE".to_owned(), "1".to_owned())],
            env_secrets: vec![("MCP_TOKEN".to_owned(), "cred-token".to_owned())],
            inherit_env: false,
            url: None,
            enabled: true,
        })
        .unwrap();
        let listed = db.list_mcp_servers().unwrap();
        assert_eq!(
            listed[0].env_plain,
            vec![("MCP_SAFE_MODE".to_owned(), "1".to_owned())]
        );
        assert_eq!(
            listed[0].env_secrets,
            vec![("MCP_TOKEN".to_owned(), "cred-token".to_owned())]
        );
        assert!(!listed[0].inherit_env);
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keyring_좌표가_기록된다() {
        let db = Db::open_in_memory().unwrap();
        db.insert_credential(&sample("cred-1")).unwrap();
        let (service, username): (String, String) = db
            .conn
            .query_row(
                "SELECT keyring_service, keyring_username FROM credentials WHERE id = 'cred-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(service, secret::KEYRING_SERVICE);
        assert_eq!(username, "cred-1");
    }
}
