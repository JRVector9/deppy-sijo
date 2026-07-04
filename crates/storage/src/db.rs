use std::path::Path;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension};

/// env 값. secret은 평문 대신 credentials.id만 참조한다 (설계문서 6.3).
/// 평문 해석은 spawn 직전(PR-09)에만 일어난다.
#[derive(Debug, Clone, PartialEq)]
pub enum EnvValue {
    Plain(String),
    Secret { credential_id: String },
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
/// 8: tool_permission_rules (PR-16 권한 규칙 영속).
/// 4~6은 각 crate가 소유한 DDL 상수를 그대로 붙인다 (스키마 정의는 한 곳에서만).
const MIGRATIONS: &[&str] = &[
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
    mcp::MIGRATION_SQL,
    audit::MIGRATION_SQL,
    // 7: agent_configs soft-delete — sessions.agent_id FK(§11.1)가 실행 이력이
    //    있는 config의 물리 삭제를 막으므로, 삭제는 표시로 대체한다
    "ALTER TABLE agent_configs ADD COLUMN deleted_at TEXT;",
    // 8: tool 권한 규칙 영속 (PR-16 — 재시작해도 Allow/Deny always가 유지되도록).
    //    (server_id, tool_name)별 rule + 마지막 승인 schema hash. FK는 두지 않는다
    //    (규칙은 문자열 키로 느슨히 연결 — orphan은 무해, 감사/로그와 동일한 관례).
    "
CREATE TABLE tool_permission_rules (
    server_id TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    rule TEXT NOT NULL,
    approved_schema_hash TEXT,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (server_id, tool_name)
);
",
];

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
    pub created_at: String,
}

/// tool 권한 규칙 한 행 (PermissionPolicy 영속 — PR-16).
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRuleRow {
    pub server_id: String,
    pub tool_name: String,
    /// "allow" | "deny" | "ask" (audit::PermissionRule::as_str)
    pub rule: String,
    pub approved_schema_hash: Option<String>,
}

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
}

impl Db {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("SQLite 열기 실패: {}", path.display()))?;
        // 설계문서 11.9: 모든 연결에 WAL + foreign_keys 강제
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        // 워커 persist 연결과 동시 쓰기가 겹칠 때 SQLITE_BUSY로 실패하지 않게 대기 (codex 리뷰)
        conn.busy_timeout(std::time::Duration::from_secs(5))?;

        // 설계문서 11.9: pending migration이 있으면 적용 전 파일 백업 (직전 1개 유지)
        let version: usize =
            conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))? as usize;
        if version > 0 && version < MIGRATIONS.len() {
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
            let backup = path.with_extension("sqlite3.bak");
            std::fs::copy(path, &backup)
                .with_context(|| format!("마이그레이션 전 백업 실패: {}", backup.display()))?;
        }
        Self::migrate(conn).with_context(|| {
            format!(
                "DB 마이그레이션 실패 — 백업: {}",
                path.with_extension("sqlite3.bak").display()
            )
        })
    }

    #[cfg(test)]
    fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", true)?;
        Self::migrate(conn)
    }

    fn migrate(mut conn: Connection) -> anyhow::Result<Self> {
        let version: usize =
            conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))? as usize;
        // forward-only (11.9): 이 바이너리보다 앞선 DB는 downgrade가 불가능하므로
        // 조용히 실행하지 않고 기동을 중단한다 (codex 리뷰 반영)
        anyhow::ensure!(
            version <= MIGRATIONS.len(),
            "DB user_version({version})이 이 버전이 아는 마이그레이션({})보다 앞서 있습니다 — \
             더 새 버전의 앱이 만든 DB입니다",
            MIGRATIONS.len()
        );
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version) {
            // 스키마 변경과 user_version 갱신을 한 트랜잭션으로 묶어
            // 중단 시 절반만 적용된 상태를 막는다
            let tx = conn.transaction()?;
            tx.execute_batch(sql)
                .with_context(|| format!("마이그레이션 {} 실패", i + 1))?;
            tx.pragma_update(None, "user_version", i as i64 + 1)?;
            tx.commit()
                .with_context(|| format!("마이그레이션 {} 커밋 실패", i + 1))?;
        }
        Ok(Self { conn })
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
                   AND NOT EXISTS (SELECT 1 FROM env_vars WHERE credential_id = ?1)",
                [id],
            )
            .with_context(|| format!("credential 삭제 실패: {id}"))?;
        Ok(affected == 1)
    }

    /// env var가 이 credential을 참조 중인지 확인 (UI 에러 메시지 구분용).
    pub fn credential_in_use(&self, id: &str) -> anyhow::Result<bool> {
        let exists: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM env_vars WHERE credential_id = ?1 LIMIT 1",
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
            .prepare("SELECT id, name, created_at FROM workspaces ORDER BY created_at")?;
        let rows = stmt.query_map([], |row| {
            Ok(WorkspaceRow {
                id: row.get(0)?,
                name: row.get(1)?,
                created_at: row.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
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
    pub fn list_mcp_servers(&self) -> anyhow::Result<Vec<mcp::McpServerRow>> {
        mcp::list_servers(&self.conn)
    }

    pub fn insert_mcp_server(&self, row: &mcp::McpServerRow) -> anyhow::Result<()> {
        mcp::insert_server(&self.conn, row)
    }

    /// 연결 테스트로 발견한 tools를 교체 저장 (PR-17).
    pub fn replace_mcp_tools(
        &mut self,
        server_id: &str,
        rows: &[mcp::McpToolRow],
    ) -> anyhow::Result<()> {
        mcp::replace_tools_for_server(&mut self.conn, server_id, rows)
    }

    /// 저장된 tool 목록 (도구 실행 UI용).
    pub fn list_mcp_tools(&self, server_id: &str) -> anyhow::Result<Vec<mcp::McpToolRow>> {
        mcp::list_tools_for_server(&self.conn, server_id)
    }

    /// 저장된 tool 권한 규칙 전체 (앱 시작 시 PermissionPolicy로 로드).
    pub fn list_permission_rules(&self) -> anyhow::Result<Vec<PermissionRuleRow>> {
        let mut stmt = self.conn.prepare(
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

    /// 권한 규칙 저장/갱신 (AllowAlways/DenyAlways 결정 시).
    pub fn upsert_permission_rule(
        &self,
        server_id: &str,
        tool_name: &str,
        rule: &str,
        approved_schema_hash: Option<&str>,
    ) -> anyhow::Result<()> {
        self.conn.execute(
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
    pub fn delete_permission_rule(&self, server_id: &str, tool_name: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "DELETE FROM tool_permission_rules WHERE server_id = ?1 AND tool_name = ?2",
            (server_id, tool_name),
        )?;
        Ok(())
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
    ) -> anyhow::Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let args_json = serde_json::to_string(args)?;
        self.conn
            .execute(
                "INSERT INTO agent_configs
                   (id, name, command, args_json,
                    waiting_regex, approval_regex, error_regex, done_regex,
                    created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
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
                ),
            )
            .with_context(|| format!("agent config 저장 실패: {name}"))?;
        Ok(id)
    }

    pub fn list_agent_configs(&self) -> anyhow::Result<Vec<AgentConfigRow>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, name, command, args_json,
                    waiting_regex, approval_regex, error_regex, done_regex
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
            ))
        })?;
        // 손상 행 하나가 전체 목록을 죽이지 않게 skip + 경고 (원문은 로그에 남기지 않음)
        let mut out = Vec::new();
        for row in rows {
            let (id, name, command, args_json, waiting, approval, error, done) = row?;
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
        db.delete_agent_config(&id).unwrap();
        assert!(db.list_agent_configs().unwrap().is_empty());
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
