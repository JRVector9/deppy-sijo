#[path = "workspace_env.rs"]
mod workspace_env;
pub use workspace_env::CredentialEnvBinding;
#[path = "agent_attention.rs"]
mod agent_attention;
#[path = "workspace_identity.rs"]
mod workspace_identity;
pub use agent_attention::{AgentAttentionEvent, AttentionEventKind};

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::Context;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use sha2::{Digest as _, Sha256};

const RESOLVED_APPROVAL_RETENTION_SECS: i64 = 30 * 24 * 60 * 60;

/// Runtime-generated approval session keys are `{workspace UUID}:{u64}`: at most 36 + 1 + 20
/// bytes. Validate this borrowed input before opening a transaction or binding it to SQLite.
pub const PENDING_APPROVAL_SESSION_KEY_BYTES_MAX: usize =
    mcp_store::PENDING_APPROVAL_SESSION_KEY_BYTES_MAX;
/// One session exit may resolve only this many pending approvals in one bounded transaction.
pub const PENDING_APPROVAL_SESSION_CLEANUP_LIMIT_MAX: usize =
    mcp_store::PENDING_APPROVAL_SESSION_LIMIT_MAX;

const SESSION_CLOSED_APPROVAL_ERROR_CODE: &str = "session_closed";
const PENDING_APPROVAL_OWNER_UNAVAILABLE: &str = "pending_approval_owner_unavailable";
const PENDING_APPROVAL_OWNER_DB_MISMATCH: &str = "pending_approval_owner_db_mismatch";
const PENDING_APPROVAL_OWNER_FILE_BACKED_REQUIRED: &str =
    "pending_approval_owner_file_backed_db_required";

fn validate_pending_approval_session_key(session_key: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !session_key.is_empty() && session_key.len() <= PENDING_APPROVAL_SESSION_KEY_BYTES_MAX,
        "pending approval session key byte length invalid"
    );
    anyhow::ensure!(
        deppy_core::parse_session_key(session_key).is_some(),
        "pending approval session key invalid"
    );
    Ok(())
}

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
    authorization_db_identity: String,
}

pub use crate::task_prompt::{task_prompt_is_displayable, task_prompt_text};

/// Durable, process-independent Connector configuration identity. SQLite stores revisions as a
/// positive signed INTEGER; the public type prevents accidental arithmetic outside storage.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectorConfigRevision(u64);

impl ConnectorConfigRevision {
    pub const INITIAL: Self = Self(1);

    pub const fn get(self) -> u64 {
        self.0
    }

    pub fn try_from_u64(value: u64) -> anyhow::Result<Self> {
        anyhow::ensure!(
            value >= Self::INITIAL.0,
            "Connector config revision은 1 이상이어야 합니다"
        );
        i64::try_from(value).context("Connector config revision이 SQLite 범위를 초과했습니다")?;
        Ok(Self(value))
    }

    fn from_sql(value: i64) -> anyhow::Result<Self> {
        anyhow::ensure!(
            value >= 1,
            "Connector config revision 저장값이 유효하지 않습니다"
        );
        Ok(Self(
            u64::try_from(value).context("Connector config revision 변환 실패")?,
        ))
    }
}

impl std::fmt::Debug for ConnectorConfigRevision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("ConnectorConfigRevision")
            .field(&self.0)
            .finish()
    }
}

/// Payload observed in the same SQLite read transaction as `revision`.
#[derive(Clone, PartialEq, Eq)]
pub struct ConnectorConfigRead<T> {
    pub revision: ConnectorConfigRevision,
    pub value: T,
}

impl<T> std::fmt::Debug for ConnectorConfigRead<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectorConfigRead")
            .field("revision", &self.revision)
            .field("value", &"ELIDED")
            .finish()
    }
}

/// Result of an IMMEDIATE expected-revision write. Stale writers never invoke the mutation and
/// receive the current committed revision to refresh from.
#[derive(Clone, PartialEq, Eq)]
pub enum ConnectorConfigCas<T> {
    Committed {
        revision: ConnectorConfigRevision,
        value: T,
    },
    Stale {
        current_revision: ConnectorConfigRevision,
    },
}

impl<T> std::fmt::Debug for ConnectorConfigCas<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Committed { revision, .. } => formatter
                .debug_struct("ConnectorConfigCas::Committed")
                .field("revision", revision)
                .field("value", &"ELIDED")
                .finish(),
            Self::Stale { current_revision } => formatter
                .debug_struct("ConnectorConfigCas::Stale")
                .field("current_revision", current_revision)
                .finish(),
        }
    }
}

/// Active owner for one DB-bound authorization executor scope. This non-Clone token owns the OS
/// lock for its full lifetime, so preflight/outcome APIs cannot outlive the ownership proof.
pub struct ActiveAuthorizationOwner {
    _scope_lock: File,
    scope: String,
    run_id: String,
    db_identity: String,
}

impl ActiveAuthorizationOwner {
    pub fn scope(&self) -> &str {
        &self.scope
    }
}

impl std::fmt::Debug for ActiveAuthorizationOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActiveAuthorizationOwner")
            .field("state", &"exclusive")
            .finish()
    }
}

/// Lifetime-held, process-independent ownership proof for app-startup pending-approval
/// reconciliation. The token is deliberately non-Clone and owns a dedicated OS lock that is
/// separate from the 256 authorization-executor stripes.
pub struct ActivePendingApprovalOwner {
    _owner_lock: File,
    db_identity: String,
}

impl std::fmt::Debug for ActivePendingApprovalOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivePendingApprovalOwner")
            .field("state", &"exclusive")
            .finish()
    }
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
/// 25: tool audit operation lifecycle (PR-ST01).
/// 26: authorization scope/run ownership columns (PR-AU01).
/// 27: durable Connector config revision + mutation triggers (PR-IN01 prerequisite).
/// 28: exact physical secret-slot lifecycle ledger (PR-SC01 prerequisite).
/// 29: exact pending-approval session cleanup index (event-driven session exit).
/// 30: durable exact cleanup obligations for legacy logical keyring sources (PR-SC01).
/// 31: bounded finalized-audit retention ordering index (PR-AU02 hardening).
/// 32: agent_needs_input.working — hook 기반 "작업 중" 신호(cmux식 턴 경계).
/// 33: workspace_notes — 워크스페이스당 스크래치패드 한 장(사이드바 「메모」 탭).
/// 34: sessions.*_regex — RespawnArchivedAgent가 열람 전용 세션을 재실행할 때
///     status detector regex를 agent_configs 재조회 없이 spawn 시점 값 그대로
///     복원하도록 세션 행에 함께 저장(persist crate 소유 DDL, runtime PR-2 후속).
/// 35: bounded agent work-turn history, keyed by durable provider turn identity.
/// 36: agent_work_turns.messages_json — 턴 안 최근 메시지 배열(유계 JSON). additive라
///     기존 행은 NULL이고 NULL이면 instruction+agent_summary만 보여주는 기존 렌더로 떨어진다.
/// 37: Relay public device metadata and verified pending approvals. Private identities, pairing
///     secrets, transport credentials, and terminal payloads are intentionally absent.
/// 38: Relay reconnect verifier, 39: Relay authorization epoch.
/// 40: physical secret recovery generation for startup cleanup ABA protection.
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
    // v16: 턴 완료(Stop hook) 신호 — 상태 레일 '완료(바이올렛)' 트랜지언트의 소스.
    // needs-input/clear(INSERT OR REPLACE)가 자연히 0으로 리셋한다.
    "
ALTER TABLE agent_needs_input ADD COLUMN turn_done INTEGER NOT NULL DEFAULT 0;
",
    // v17: claude statusLine이 보고한 표시 정보(effort/model/남은 context%). session_key는
    // {workspace_id}:{session_id}(=pane). 3줄 세션 행 2/3행에 병합. codex는 rollout에서
    // 직접 얻으므로 이 테이블은 claude 전용.
    "
CREATE TABLE agent_statusline (
    session_key TEXT PRIMARY KEY,
    effort TEXT,
    model TEXT,
    context_pct INTEGER,
    updated_at INTEGER NOT NULL
);
",
    // v18: 프로젝트 폴더 앵커(dev, ino) — 폴더 rename/이동 시 path가 stale돼도 세션 cwd의
    // inode와 대조해 새 경로를 찾아 복구 제안하는 데 쓴다(2026-07-08).
    "
ALTER TABLE workspaces ADD COLUMN path_dev INTEGER;
ALTER TABLE workspaces ADD COLUMN path_ino INTEGER;
",
    // v19: credential 프로젝트(워크스페이스) 소속 — NULL이면 전역 공유(커넥터/OAuth,
    // 기존 데이터 호환). 환경 UI 추가·.env 동기화 credential은 해당 workspace 소속(#2).
    "
ALTER TABLE credentials ADD COLUMN workspace_id TEXT;
",
    // v20: OAuth 연계 메타데이터 (PR-H5). http MCP 서버 바인딩(동의 시점 서버 URL),
    // 발견된 AS endpoint, client_id, scope, 만료 시각 등 **비밀 아닌** JSON만 저장한다
    // — 비밀(access/refresh/DCR secret)은 전부 keyring (§2.1). JSON 해석은 앱(connectors) 몫.
    "
ALTER TABLE credentials ADD COLUMN oauth_json TEXT;
",
    // v21: 웹푸시(VAPID) 구독 — PR-P4. 폰이 앱(브라우저)을 닫아도 승인 요청을 푸시로 알린다.
    //      endpoint가 PK(푸시 서비스가 준 고유 URL). p256dh/auth는 브라우저 pushManager가
    //      건넨 RFC 8291 암호화 파라미터(base64url) — 비밀 아닌 이 기기 전용 공개값이라
    //      credential/keyring이 아니라 여기 평문으로 둔다. last_ok_at은 마지막 발송 성공 시각
    //      (진단·정리용). 발송 시 410/404를 받으면 web-remote push.rs가 이 행을 즉시 지운다.
    "
CREATE TABLE web_push_subscriptions (
    endpoint TEXT PRIMARY KEY,
    p256dh TEXT NOT NULL,
    auth TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_ok_at INTEGER
);
",
    // 승인 ↔ 세션 연결 (v3.7 I2). 승인이 어느 pane에서 났는지 기록해 UI가 세션명을
    // 보이고 폰이 그 세션 화면을 열 수 있게 한다. proxy가 이미 env로 받는 pane_id를
    // 그대로 싣는다(조회 없음 — 등록 경로는 fail-closed라 새 실패 지점을 안 만든다).
    // NULL 허용: env 미주입 경로·레거시 행은 "세션 불명"으로 표시.
    mcp_store::MIGRATION_APPROVAL_PANE,
    // v23: Codex App Server 구조화 thread 복구 메타데이터. 기존 agent_sessions는
    // Claude/Codex PTY native resume 전용이므로 별도 테이블에서 관리한다. item/turn
    // 원문은 Codex rollout이 source of truth이며 이 테이블에는 중복 저장하지 않는다.
    "
CREATE TABLE structured_threads (
    local_session_id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    thread_id TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL DEFAULT '',
    cwd TEXT NOT NULL DEFAULT '',
    model TEXT,
    favorite INTEGER NOT NULL DEFAULT 0 CHECK (favorite IN (0, 1)),
    archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY(workspace_id) REFERENCES workspaces(id)
);

CREATE INDEX idx_structured_threads_workspace_recency
    ON structured_threads(workspace_id, favorite DESC, updated_at DESC);
",
    // v24: hook이 보고한 대기 사유 문구 — claude Notification hook payload의 `message`
    // ("Claude needs your permission to use Bash"). 벨 인박스 대기 카드의 헤드라인으로
    // 쓴다: 로그 tail은 TUI가 화면을 다시 그린 흔적이라 상태줄이 섞이는데, 이 문구는
    // 에이전트가 직접 말한 "무엇을 묻는지"다(2026-07-17).
    // NULL 허용 — 기존 행과 message를 안 보내는 에이전트(codex 등)는 그대로 tail로 폴백한다.
    "
ALTER TABLE agent_needs_input ADD COLUMN message TEXT;
",
    // v25: 외부 tool call durable lifecycle. Prepared에서 crash한 operation은 시작 시
    // Unknown으로 종결하며 operation_id unique guard로 같은 호출의 자동 retry를 막는다.
    audit::MIGRATION_AUDIT_LIFECYCLE,
    audit::MIGRATION_AUTHORIZATION_OWNERS,
    // v27: all Connector-visible durable mutations advance one shared monotonic revision in the
    // same SQLite transaction, including writes performed by another process or legacy API.
    // Trigger increments may jump by more than one for batch/replace operations; consumers rely
    // only on monotonic identity, never contiguity.
    "
CREATE TABLE connector_config_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    revision INTEGER NOT NULL CHECK (revision >= 1)
);
INSERT INTO connector_config_state (singleton, revision) VALUES (1, 1);

CREATE TRIGGER connector_credentials_insert_revision
AFTER INSERT ON credentials BEGIN
    UPDATE connector_config_state SET revision = CASE
        WHEN revision < 9223372036854775807 THEN revision + 1
        ELSE RAISE(ABORT, 'connector config revision overflow') END
    WHERE singleton = 1;
    SELECT CASE WHEN changes() != 1 THEN RAISE(ABORT, 'connector config revision missing') END;
END;
CREATE TRIGGER connector_credentials_delete_revision
AFTER DELETE ON credentials BEGIN
    UPDATE connector_config_state SET revision = CASE
        WHEN revision < 9223372036854775807 THEN revision + 1
        ELSE RAISE(ABORT, 'connector config revision overflow') END
    WHERE singleton = 1;
    SELECT CASE WHEN changes() != 1 THEN RAISE(ABORT, 'connector config revision missing') END;
END;
CREATE TRIGGER connector_credentials_update_revision
AFTER UPDATE OF id, provider, label, credential_kind, keyring_service, keyring_username,
                masked_hint, workspace_id, oauth_json ON credentials
WHEN OLD.id IS NOT NEW.id
  OR OLD.provider IS NOT NEW.provider
  OR OLD.label IS NOT NEW.label
  OR OLD.credential_kind IS NOT NEW.credential_kind
  OR OLD.keyring_service IS NOT NEW.keyring_service
  OR OLD.keyring_username IS NOT NEW.keyring_username
  OR OLD.masked_hint IS NOT NEW.masked_hint
  OR OLD.workspace_id IS NOT NEW.workspace_id
  OR OLD.oauth_json IS NOT NEW.oauth_json
BEGIN
    UPDATE connector_config_state SET revision = CASE
        WHEN revision < 9223372036854775807 THEN revision + 1
        ELSE RAISE(ABORT, 'connector config revision overflow') END
    WHERE singleton = 1;
    SELECT CASE WHEN changes() != 1 THEN RAISE(ABORT, 'connector config revision missing') END;
END;

CREATE TRIGGER connector_mcp_servers_insert_revision
AFTER INSERT ON mcp_servers BEGIN
    UPDATE connector_config_state SET revision = CASE
        WHEN revision < 9223372036854775807 THEN revision + 1
        ELSE RAISE(ABORT, 'connector config revision overflow') END
    WHERE singleton = 1;
    SELECT CASE WHEN changes() != 1 THEN RAISE(ABORT, 'connector config revision missing') END;
END;
CREATE TRIGGER connector_mcp_servers_delete_revision
AFTER DELETE ON mcp_servers BEGIN
    UPDATE connector_config_state SET revision = CASE
        WHEN revision < 9223372036854775807 THEN revision + 1
        ELSE RAISE(ABORT, 'connector config revision overflow') END
    WHERE singleton = 1;
    SELECT CASE WHEN changes() != 1 THEN RAISE(ABORT, 'connector config revision missing') END;
END;
CREATE TRIGGER connector_mcp_servers_update_revision
AFTER UPDATE OF id, name, kind, command, args_json, env_json, env_credentials_json,
                inherit_env, url, enabled ON mcp_servers
WHEN OLD.id IS NOT NEW.id
  OR OLD.name IS NOT NEW.name
  OR OLD.kind IS NOT NEW.kind
  OR OLD.command IS NOT NEW.command
  OR OLD.args_json IS NOT NEW.args_json
  OR OLD.env_json IS NOT NEW.env_json
  OR OLD.env_credentials_json IS NOT NEW.env_credentials_json
  OR OLD.inherit_env IS NOT NEW.inherit_env
  OR OLD.url IS NOT NEW.url
  OR OLD.enabled IS NOT NEW.enabled
BEGIN
    UPDATE connector_config_state SET revision = CASE
        WHEN revision < 9223372036854775807 THEN revision + 1
        ELSE RAISE(ABORT, 'connector config revision overflow') END
    WHERE singleton = 1;
    SELECT CASE WHEN changes() != 1 THEN RAISE(ABORT, 'connector config revision missing') END;
END;

CREATE TRIGGER connector_mcp_tools_insert_revision
AFTER INSERT ON mcp_tools BEGIN
    UPDATE connector_config_state SET revision = CASE
        WHEN revision < 9223372036854775807 THEN revision + 1
        ELSE RAISE(ABORT, 'connector config revision overflow') END
    WHERE singleton = 1;
    SELECT CASE WHEN changes() != 1 THEN RAISE(ABORT, 'connector config revision missing') END;
END;
CREATE TRIGGER connector_mcp_tools_delete_revision
AFTER DELETE ON mcp_tools BEGIN
    UPDATE connector_config_state SET revision = CASE
        WHEN revision < 9223372036854775807 THEN revision + 1
        ELSE RAISE(ABORT, 'connector config revision overflow') END
    WHERE singleton = 1;
    SELECT CASE WHEN changes() != 1 THEN RAISE(ABORT, 'connector config revision missing') END;
END;
CREATE TRIGGER connector_mcp_tools_update_revision
AFTER UPDATE OF id, server_id, name, description, input_schema_json, trust_level, schema_hash
ON mcp_tools
WHEN OLD.id IS NOT NEW.id
  OR OLD.server_id IS NOT NEW.server_id
  OR OLD.name IS NOT NEW.name
  OR OLD.description IS NOT NEW.description
  OR OLD.input_schema_json IS NOT NEW.input_schema_json
  OR OLD.trust_level IS NOT NEW.trust_level
  OR OLD.schema_hash IS NOT NEW.schema_hash
BEGIN
    UPDATE connector_config_state SET revision = CASE
        WHEN revision < 9223372036854775807 THEN revision + 1
        ELSE RAISE(ABORT, 'connector config revision overflow') END
    WHERE singleton = 1;
    SELECT CASE WHEN changes() != 1 THEN RAISE(ABORT, 'connector config revision missing') END;
END;

CREATE TRIGGER connector_permission_insert_revision
AFTER INSERT ON tool_permission_rules BEGIN
    UPDATE connector_config_state SET revision = CASE
        WHEN revision < 9223372036854775807 THEN revision + 1
        ELSE RAISE(ABORT, 'connector config revision overflow') END
    WHERE singleton = 1;
    SELECT CASE WHEN changes() != 1 THEN RAISE(ABORT, 'connector config revision missing') END;
END;
CREATE TRIGGER connector_permission_delete_revision
AFTER DELETE ON tool_permission_rules BEGIN
    UPDATE connector_config_state SET revision = CASE
        WHEN revision < 9223372036854775807 THEN revision + 1
        ELSE RAISE(ABORT, 'connector config revision overflow') END
    WHERE singleton = 1;
    SELECT CASE WHEN changes() != 1 THEN RAISE(ABORT, 'connector config revision missing') END;
END;
CREATE TRIGGER connector_permission_update_revision
AFTER UPDATE OF server_id, tool_name, rule, approved_schema_hash ON tool_permission_rules
WHEN OLD.server_id IS NOT NEW.server_id
  OR OLD.tool_name IS NOT NEW.tool_name
  OR OLD.rule IS NOT NEW.rule
  OR OLD.approved_schema_hash IS NOT NEW.approved_schema_hash
BEGIN
    UPDATE connector_config_state SET revision = CASE
        WHEN revision < 9223372036854775807 THEN revision + 1
        ELSE RAISE(ABORT, 'connector config revision overflow') END
    WHERE singleton = 1;
    SELECT CASE WHEN changes() != 1 THEN RAISE(ABORT, 'connector config revision missing') END;
END;
",
    // v28: durable exact-slot ownership replaces platform-wide keyring enumeration. This table
    // stores only logical/physical identifiers and lifecycle state, never a secret value. It has
    // deliberately no Connector revision trigger: staging and deletion acknowledgement are
    // housekeeping; credential pointer publication remains the config-visible trigger source.
    "
CREATE TABLE physical_secret_slot_ledger (
    physical_slot TEXT PRIMARY KEY,
    logical_credential_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('staging', 'published', 'orphan')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK (length(CAST(logical_credential_id AS BLOB)) BETWEEN 1 AND 96),
    CHECK (length(CAST(physical_slot AS BLOB)) BETWEEN 1 AND 255),
    CHECK (instr(logical_credential_id, char(0)) = 0),
    CHECK (instr(physical_slot, char(0)) = 0)
);

CREATE UNIQUE INDEX idx_physical_secret_slot_one_published
    ON physical_secret_slot_ledger(logical_credential_id)
    WHERE state = 'published';
CREATE INDEX idx_physical_secret_slot_reconcile
    ON physical_secret_slot_ledger(state, created_at, physical_slot);
CREATE INDEX idx_credentials_keyring_username ON credentials(keyring_username);

CREATE TRIGGER physical_secret_slot_identity_immutable
BEFORE UPDATE OF physical_slot, logical_credential_id ON physical_secret_slot_ledger
BEGIN
    SELECT RAISE(ABORT, 'physical secret slot identity is immutable');
END;
CREATE TRIGGER physical_secret_slot_valid_transition
BEFORE UPDATE OF state ON physical_secret_slot_ledger
WHEN NOT (
    (OLD.state = 'staging' AND NEW.state IN ('published', 'orphan'))
    OR (OLD.state = 'published' AND NEW.state = 'orphan')
)
BEGIN
    SELECT RAISE(ABORT, 'invalid physical secret slot lifecycle transition');
END;

INSERT INTO physical_secret_slot_ledger
    (physical_slot, logical_credential_id, state, created_at, updated_at)
SELECT keyring_username, id, 'published',
       CAST(strftime('%s','now') AS INTEGER), CAST(strftime('%s','now') AS INTEGER)
FROM credentials
WHERE keyring_username LIKE 'deppy.oauth.v1.%'
  AND keyring_username != id
  AND keyring_service = 'app.vector9.deppy-sijo'
  AND length(CAST(id AS BLOB)) BETWEEN 1 AND 96
  AND length(CAST(keyring_username AS BLOB)) BETWEEN 1 AND 255
  AND instr(id, char(0)) = 0
  AND instr(keyring_username, char(0)) = 0;
",
    // v29: session exit cleanup addresses the runtime session key stored in pane_id. A partial
    // index keeps the exact lookup independent of the retained 30-day resolved corpus, and rows
    // leave the index automatically when their lifecycle becomes terminal.
    "
CREATE INDEX idx_pending_approvals_session_pending
    ON pending_approvals(pane_id)
    WHERE status = 'pending';
",
    // v30: publishing a legacy logical keyring username to a versioned physical slot must not
    // lose the post-commit obligation to delete the exact legacy base/.refresh/.dcr entries. The
    // nullable source coordinate remains on the same bounded ledger row, so no second unbounded
    // corpus is introduced. It stores an identifier only, never a keyring value.
    "
ALTER TABLE physical_secret_slot_ledger
    ADD COLUMN legacy_cleanup_username TEXT
    CHECK (legacy_cleanup_username IS NULL OR (
        state IN ('published', 'orphan')
        AND
        legacy_cleanup_username = logical_credential_id
        AND length(CAST(legacy_cleanup_username AS BLOB)) BETWEEN 1 AND 255
        AND instr(legacy_cleanup_username, char(0)) = 0
    ));

CREATE UNIQUE INDEX idx_physical_secret_slot_one_legacy_cleanup
    ON physical_secret_slot_ledger(legacy_cleanup_username)
    WHERE legacy_cleanup_username IS NOT NULL;

CREATE TRIGGER physical_secret_slot_legacy_cleanup_transition
BEFORE UPDATE OF legacy_cleanup_username ON physical_secret_slot_ledger
WHEN NOT (
    OLD.legacy_cleanup_username IS NEW.legacy_cleanup_username
    OR (OLD.legacy_cleanup_username IS NULL
        AND NEW.legacy_cleanup_username = OLD.logical_credential_id
        AND OLD.state = 'published')
    OR (OLD.legacy_cleanup_username IS NOT NULL
        AND NEW.legacy_cleanup_username IS NULL)
)
BEGIN
    SELECT RAISE(ABORT, 'invalid legacy cleanup marker transition');
END;
",
    // v31: steady-state retention must satisfy newest-first item/byte/age ceilings without sorting
    // an unbounded legacy table inside a lifecycle transaction. The partial expression index makes
    // the fixed policy-window + batch + sentinel scan an indexed walk.
    audit::MIGRATION_AUDIT_RETENTION,
    // v32: hook 기반 "작업 중" 신호(cmux식). UserPromptSubmit/PreToolUse의 clear 이벤트가
    // working=1을 기록하고 Stop(turn-done)/Notification(needs-input)이 0으로 되돌린다 —
    // transcript 폴링 지연·warm 미추적을 훅 신호로 대체한다.
    "
ALTER TABLE agent_needs_input ADD COLUMN working INTEGER NOT NULL DEFAULT 0;
",
    // v33: 워크스페이스당 메모 한 장. workspace_id가 PK라 행이 늘 수 없고,
    // ON DELETE CASCADE라 `delete_workspace`가 따로 지우지 않아도 함께 사라진다
    // (고아 메모가 남으면 같은 id 재사용 시 남의 메모가 되살아난다).
    // 빈 본문은 행을 남기지 않는 규약이라 CHECK로 못박는다 — 저장 경로가 지우고
    // 들어오지만, 다른 경로가 생겨도 빈 행이 쌓이지 않는다.
    "
CREATE TABLE workspace_notes (
    workspace_id TEXT PRIMARY KEY
        REFERENCES workspaces(id) ON DELETE CASCADE,
    body TEXT NOT NULL CHECK (length(body) > 0),
    updated_at TEXT NOT NULL
);
",
    // v34: sessions.*_regex — 세션 spawn 시점의 status detector regex를 세션 행에
    // 함께 저장한다. 기존 행은 컬럼이 없던 시절 것이라 NULL(=미지정, 기존과 동일하게
    // idle heuristic만 동작) — 재실행이 아니라 재확인(re-run) 없이는 소급 채움이
    // 불가능하므로 이는 정상 동작이다.
    persist::MIGRATION_SESSION_REGEX,
    // v35: transcript에서 복원한 사용자 지시 단위 작업 이력. 원문 transcript나 tool
    // payload는 저장하지 않고 bounded 표시 필드만 보존한다. workspace 삭제와 함께
    // 제거되며, 저장 API가 workspace당 최신 256행으로 같은 transaction에서 정리한다.
    "
CREATE TABLE agent_work_turns (
    workspace_id TEXT NOT NULL,
    pane_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    agent_session_id TEXT NOT NULL,
    turn_key TEXT NOT NULL,
    source_offset INTEGER NOT NULL CHECK (source_offset >= 0),
    instruction TEXT NOT NULL,
    agent_summary TEXT,
    model TEXT,
    effort TEXT,
    cwd TEXT,
    branch TEXT,
    git_change_count INTEGER CHECK (git_change_count IS NULL OR git_change_count >= 0),
    state TEXT NOT NULL CHECK (state IN ('working', 'waiting', 'completed')),
    occurred_at INTEGER,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, kind, agent_session_id, turn_key),
    FOREIGN KEY(workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE INDEX idx_agent_work_turns_workspace_recency
    ON agent_work_turns(workspace_id, updated_at DESC, source_offset DESC);
",
    // v36: 턴 하나가 남기는 마지막 요약 하나로는 에이전트가 무엇을 했는지 읽히지
    // 않았다. 턴 안 최신 메시지 5개를 유계 JSON으로 함께 보존한다(2026-08-15).
    // 기존 행은 NULL이고, NULL이면 예전대로 instruction+agent_summary만 보여준다.
    "ALTER TABLE agent_work_turns ADD COLUMN messages_json TEXT;",
    // v37: opt-in Relay public metadata. Pending rows exist only after in-memory one-shot secret
    // verification. Approval consumes one row and publishes exactly one device atomically.
    "
CREATE TABLE relay_pending_devices (
    pairing_id BLOB PRIMARY KEY
        CHECK (typeof(pairing_id) = 'blob' AND length(pairing_id) = 16),
    device_id BLOB NOT NULL UNIQUE
        CHECK (typeof(device_id) = 'blob' AND length(device_id) = 16),
    identity_public_sec1 BLOB NOT NULL UNIQUE
        CHECK (typeof(identity_public_sec1) = 'blob'
            AND length(identity_public_sec1) = 65
            AND hex(substr(identity_public_sec1, 1, 1)) = '04'),
    display_name TEXT NOT NULL
        CHECK (typeof(display_name) = 'text'
            AND length(CAST(display_name AS BLOB)) BETWEEN 1 AND 128
            AND instr(display_name, char(0)) = 0),
    permission_view INTEGER NOT NULL CHECK (permission_view IN (0, 1)),
    permission_input INTEGER NOT NULL CHECK (permission_input IN (0, 1)),
    permission_upload INTEGER NOT NULL CHECK (permission_upload IN (0, 1)),
    permission_approval INTEGER NOT NULL CHECK (permission_approval IN (0, 1)),
    issued_at INTEGER NOT NULL CHECK (issued_at >= 0),
    -- 5분 페어링 의식 마감. 이 시각 이후의 승인은 실패한다.
    pairing_expires_at INTEGER NOT NULL
        CHECK (pairing_expires_at > issued_at
            AND pairing_expires_at - issued_at <= 300),
    -- 승인이 커밋된 뒤 발급될 기기 인가 만료. 페어링 마감과 별개 수명이다.
    device_expires_at INTEGER NOT NULL CHECK (device_expires_at >= pairing_expires_at)
);

CREATE INDEX idx_relay_pending_expiry
    ON relay_pending_devices(pairing_expires_at, pairing_id);

CREATE TABLE relay_devices (
    device_id BLOB PRIMARY KEY
        CHECK (typeof(device_id) = 'blob' AND length(device_id) = 16),
    identity_public_sec1 BLOB NOT NULL UNIQUE
        CHECK (typeof(identity_public_sec1) = 'blob'
            AND length(identity_public_sec1) = 65
            AND hex(substr(identity_public_sec1, 1, 1)) = '04'),
    display_name TEXT NOT NULL
        CHECK (typeof(display_name) = 'text'
            AND length(CAST(display_name AS BLOB)) BETWEEN 1 AND 128
            AND instr(display_name, char(0)) = 0),
    permission_view INTEGER NOT NULL CHECK (permission_view IN (0, 1)),
    permission_input INTEGER NOT NULL CHECK (permission_input IN (0, 1)),
    permission_upload INTEGER NOT NULL CHECK (permission_upload IN (0, 1)),
    permission_approval INTEGER NOT NULL CHECK (permission_approval IN (0, 1)),
    issued_at INTEGER NOT NULL CHECK (issued_at >= 0),
    device_expires_at INTEGER NOT NULL CHECK (device_expires_at > issued_at),
    last_seen_at INTEGER CHECK (last_seen_at IS NULL OR last_seen_at >= issued_at),
    revoked_at INTEGER CHECK (revoked_at IS NULL OR revoked_at >= issued_at)
);

CREATE INDEX idx_relay_devices_recency
    ON relay_devices(revoked_at, last_seen_at DESC, issued_at DESC, device_id);
",
    // v38: 재접속 raw grant는 브라우저에만 있고 DB는 검증자만 보존한다.
    "ALTER TABLE relay_devices ADD COLUMN reconnect_verifier BLOB
        CHECK (reconnect_verifier IS NULL OR
            (typeof(reconnect_verifier) = 'blob' AND length(reconnect_verifier) = 32));",
    // v39: 승인마다 바뀌는 공개 세대 표식. 기기 키·권한·만료·검증자는 그대로 보존한다.
    "ALTER TABLE relay_devices ADD COLUMN authorization_epoch BLOB NOT NULL
        DEFAULT X'00000000000000000000000000000000'
        CHECK (typeof(authorization_epoch) = 'blob' AND length(authorization_epoch) = 16);
     UPDATE relay_devices SET authorization_epoch = randomblob(16);",
    // v40: 지연 복구 후보를 행 재생성과 구분한다. 비밀이나 시각 기반 cutoff가 아니다.
    "ALTER TABLE physical_secret_slot_ledger ADD COLUMN recovery_generation BLOB NOT NULL
        DEFAULT X'00000000000000000000000000000000'
        CHECK(typeof(recovery_generation) = 'blob' AND length(recovery_generation) = 16);
     UPDATE physical_secret_slot_ledger SET recovery_generation = randomblob(16);
     CREATE TRIGGER physical_secret_slot_fresh_recovery_generation
     AFTER INSERT ON physical_secret_slot_ledger
     BEGIN
       UPDATE physical_secret_slot_ledger SET recovery_generation = randomblob(16)
       WHERE physical_slot = NEW.physical_slot;
     END;",
    // v41: 비밀값은 Keychain에 유지하고 workspace별 실행 환경 연결만 저장한다.
    "CREATE TABLE workspace_credential_env (
       workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
       env_name TEXT NOT NULL CHECK(typeof(env_name)='text' AND length(CAST(env_name AS BLOB)) BETWEEN 1 AND 256
         AND env_name NOT GLOB '*[^A-Za-z0-9_]*' AND substr(env_name,1,1) GLOB '[A-Za-z_]'),
       credential_id TEXT NOT NULL REFERENCES credentials(id),
       PRIMARY KEY(workspace_id, env_name), UNIQUE(workspace_id, credential_id));
     CREATE TRIGGER workspace_credential_env_insert_guard BEFORE INSERT ON workspace_credential_env
     BEGIN
       SELECT CASE WHEN NOT EXISTS(SELECT 1 FROM credentials WHERE id=NEW.credential_id
         AND (workspace_id IS NULL OR workspace_id=NEW.workspace_id))
         THEN RAISE(ABORT, 'credential_env_owner_invalid') END;
       SELECT CASE WHEN (SELECT COUNT(*) FROM workspace_credential_env WHERE workspace_id=NEW.workspace_id) >= 256
         THEN RAISE(ABORT, 'credential_env_limit') END;
     END;
     CREATE TRIGGER workspace_credential_env_update_guard BEFORE UPDATE ON workspace_credential_env
     BEGIN SELECT RAISE(ABORT, 'credential_env_use_replace'); END;
     CREATE TRIGGER credential_env_owner_guard BEFORE UPDATE OF workspace_id ON credentials
     WHEN NEW.workspace_id IS NOT NULL AND EXISTS(SELECT 1 FROM workspace_credential_env
       WHERE credential_id=OLD.id AND workspace_id<>NEW.workspace_id)
     BEGIN SELECT RAISE(ABORT, 'credential_env_owner_invalid'); END;",
    // v42: 프로젝트가 명시적으로 선택한 dotenv 파일과 적용 순서다. 빈 배열은 사용 중지다.
    "CREATE TABLE workspace_env_sources (
       workspace_id TEXT PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
       files_json TEXT NOT NULL CHECK(typeof(files_json)='text' AND length(CAST(files_json AS BLOB)) <= 8192));",

    // v42: 실행 상태와 별개인 질문/승인 요청 및 종료된 요청의 세대를 보관한다.
    "ALTER TABLE agent_needs_input ADD COLUMN response_required INTEGER NOT NULL DEFAULT 0;
     ALTER TABLE agent_needs_input ADD COLUMN attention_json TEXT;
     ALTER TABLE agent_needs_input ADD COLUMN attention_revision INTEGER NOT NULL DEFAULT 0;",
    // Volume UUID proofs are optional for legacy/unsupported filesystems, and
    // cannot survive a path/inode rebind without fresh filesystem verification.
    "CREATE TABLE workspace_volume_identities (
       workspace_id TEXT PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
       path TEXT NOT NULL, path_dev INTEGER NOT NULL, path_ino INTEGER NOT NULL,
       volume_uuid TEXT NOT NULL CHECK(typeof(volume_uuid)='text' AND length(CAST(volume_uuid AS BLOB))=36));
     CREATE TRIGGER workspace_volume_identity_invalidate AFTER UPDATE OF path,path_dev,path_ino ON workspaces
     WHEN OLD.path IS NOT NEW.path OR OLD.path_dev IS NOT NEW.path_dev OR OLD.path_ino IS NOT NEW.path_ino
     BEGIN DELETE FROM workspace_volume_identities WHERE workspace_id=NEW.id; END;",
    // Actual idle start is Unix seconds, independent of notification CAS generations.
    "ALTER TABLE agent_needs_input ADD COLUMN idle_since INTEGER;
     UPDATE agent_needs_input SET idle_since=updated_at
     WHERE attention_json IS NULL AND turn_done=1 AND working=0 AND waiting=0;
     UPDATE agent_needs_input SET idle_since=attention_revision/1000000
     WHERE working=0 AND waiting=0 AND typeof(attention_revision)='integer'
       AND attention_revision BETWEEN 1000000 AND CAST(strftime('%s','now') AS INTEGER)*1000000+999999
       AND CASE WHEN typeof(attention_json)='text' AND length(CAST(attention_json AS BLOB))<=32768
         AND json_valid(attention_json) THEN
           json_type(attention_json,'$.completed_turn')='text'
           AND json_type(attention_json,'$.last_activity')='integer'
           AND json_extract(attention_json,'$.last_activity')=attention_revision
           AND COALESCE(json_extract(attention_json,'$.turn_cancelled'),0)=0
           AND COALESCE(json_extract(attention_json,'$.ended'),0)=0
           AND json_type(attention_json,'$.requests')='array'
           AND NOT EXISTS(SELECT 1 FROM json_each(attention_json,'$.requests')
             WHERE CASE WHEN type='object' THEN
               COALESCE(json_type(value,'$.kind'),'missing')!='integer'
               OR COALESCE(json_extract(value,'$.kind'),1)!=0 ELSE 1 END)
         ELSE 0 END;
     CREATE INDEX idx_agent_idle_clock ON agent_needs_input(updated_at DESC) WHERE idle_since IS NOT NULL;",
    // Idle episodes may start at a resolved question, after the completion CAS token.
    "ALTER TABLE agent_needs_input ADD COLUMN idle_generation INTEGER;
     UPDATE agent_needs_input SET idle_generation=CASE
       WHEN attention_json IS NOT NULL AND attention_revision/1000000=idle_since
         THEN attention_revision ELSE idle_since*1000000 END
     WHERE idle_since IS NOT NULL;",
    // A Claude hook runs inside one pane even when two processes resume the same native ID.
    // Keep only a bounded task prompt per pane; transcript summaries cannot distinguish them.
    "ALTER TABLE agent_hook_sessions ADD COLUMN task_prompt TEXT
       CHECK(task_prompt IS NULL OR (typeof(task_prompt)='text'
         AND length(CAST(task_prompt AS BLOB)) <= 256));",
    // A pane ID survives app restart; runtime SessionId and hook session_key do not.
    "ALTER TABLE agent_sessions ADD COLUMN task_prompt TEXT
       CHECK(task_prompt IS NULL OR (typeof(task_prompt)='text'
         AND length(CAST(task_prompt AS BLOB)) <= 256));",
];

/// 옵션2: 저장된 에이전트 세션 한 행 — 재시작 복원 시 native resume에 쓴다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSessionRow {
    pub pane_id: String,
    /// "claude" | "codex".
    pub kind: String,
    /// 에이전트 자신의 세션 ID (`claude --resume <id>` / `codex resume <id>`).
    pub session_id: String,
    /// Pane-scoped last real Claude user task; never inferred from a shared transcript.
    pub task_prompt: Option<String>,
}

/// Restored read-only agent pane metadata joined by the durable `sessions.id` carried in mux
/// snapshots. The optional pair is present only when a CLI-native conversation binding exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchivedAgentResumeRow {
    pub persistent_session_id: String,
    pub agent_id: String,
    pub kind: Option<String>,
    pub session_id: Option<String>,
}

/// Codex App Server thread의 앱 소유 복구 메타데이터. 구조화 item/turn 원문은
/// App Server의 `thread/read`/`thread/resume`에서 다시 읽는다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredThreadRow {
    pub local_session_id: String,
    pub workspace_id: String,
    pub thread_id: String,
    pub title: String,
    pub cwd: String,
    pub model: Option<String>,
    pub favorite: bool,
    pub archived: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One persisted PTY-agent binding that may be removed only while all identity fields still
/// match. This prevents a delayed worker job from deleting a newer binding for the same pane.
#[derive(Clone, PartialEq, Eq)]
pub struct AgentSessionIdentity {
    pub pane_id: String,
    pub kind: String,
    pub session_id: String,
}

impl std::fmt::Debug for AgentSessionIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentSessionIdentity")
            .field("state", &"elided")
            .finish()
    }
}

/// Full desired-state input for one authoritative live-pane observation. Rows for panes that are
/// no longer live are removed, desired bindings are upserted, and existing bindings for live but
/// currently undetected panes are preserved for restart recovery.
#[derive(Clone, PartialEq, Eq)]
pub struct AgentSessionBindingReconcile {
    pub live_pane_ids: Vec<String>,
    pub desired_bindings: Vec<AgentSessionRow>,
}

impl std::fmt::Debug for AgentSessionBindingReconcile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentSessionBindingReconcile")
            .field("live_pane_count", &self.live_pane_ids.len())
            .field("desired_binding_count", &self.desired_bindings.len())
            .finish()
    }
}

/// Generation-aware acknowledgement for one hook-reported completed turn.
#[derive(Clone, PartialEq, Eq)]
pub struct AgentTurnDoneClear {
    pub session_key: String,
    pub seen_at: i64,
}

impl std::fmt::Debug for AgentTurnDoneClear {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentTurnDoneClear")
            .field("has_generation", &true)
            .finish()
    }
}

pub const AGENT_WORK_TURNS_PER_WORKSPACE_MAX: usize = 256;
pub const AGENT_WORK_TURN_BATCH_MAX: usize = 24;
pub const AGENT_WORK_TURN_BATCH_BYTES_MAX: usize = 256 * 1024;
pub const AGENT_WORK_HISTORY_SNAPSHOT_BYTES_MAX: usize = 4 * 1024 * 1024;

const AGENT_WORK_TURN_PROVIDER_BYTES_MAX: usize = 64;
const AGENT_WORK_TURN_ID_BYTES_MAX: usize = 1024;
const AGENT_WORK_TURN_INSTRUCTION_BYTES_MAX: usize = 32 * 1024;
const AGENT_WORK_TURN_SUMMARY_BYTES_MAX: usize = 32 * 1024;
/// 턴 메시지 배열 컬럼 상한. 행 전체 상한(32KB) 안에서 나머지 필드에 자리를 남긴다.
const AGENT_WORK_TURN_MESSAGES_BYTES_MAX: usize = 8 * 1024;
const AGENT_WORK_TURN_CWD_BYTES_MAX: usize = 4 * 1024;
const AGENT_WORK_TURN_METADATA_BYTES_MAX: usize = 1024;
const AGENT_WORK_TURN_ROW_BYTES_MAX: usize = 32 * 1024;
const AGENT_WORK_HISTORY_INPUT_INVALID: &str = "agent work history input invalid";
const AGENT_WORK_HISTORY_ROW_INVALID: &str = "agent work history row invalid";
const AGENT_WORK_HISTORY_QUERY_FAILED: &str = "agent work history query failed";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentWorkTurnState {
    Working,
    Waiting,
    Completed,
}

impl AgentWorkTurnState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Completed => "completed",
        }
    }

    fn from_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "working" => Ok(Self::Working),
            "waiting" => Ok(Self::Waiting),
            "completed" => Ok(Self::Completed),
            _ => anyhow::bail!(AGENT_WORK_HISTORY_ROW_INVALID),
        }
    }
}

/// One durable user-instruction turn. Debug deliberately reports only safe cardinality/state.
#[derive(Clone, PartialEq, Eq)]
pub struct AgentWorkTurnRow {
    pub workspace_id: String,
    pub pane_id: String,
    pub kind: String,
    pub agent_session_id: String,
    pub turn_key: String,
    pub source_offset: u64,
    pub instruction: String,
    pub agent_summary: Option<String>,
    /// 턴 안 최신 메시지 5개를 담은 유계 JSON. 컬럼이 없던 시절 행이거나, 상한(8KB) 초과나
    /// NUL로 이 컬럼만 fail-soft로 떨어진 경우 NULL이다(행 자체는 거부되지 않는다) — 그때는
    /// 기존 instruction+agent_summary만 그린다.
    pub messages_json: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub git_change_count: Option<u32>,
    pub state: AgentWorkTurnState,
    pub occurred_at: Option<i64>,
    pub updated_at: i64,
}

impl std::fmt::Debug for AgentWorkTurnRow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentWorkTurnRow")
            .field("state", &self.state)
            .field("has_summary", &self.agent_summary.is_some())
            .field("has_messages", &self.messages_json.is_some())
            .field("has_git_facts", &self.cwd.is_some())
            .finish_non_exhaustive()
    }
}

/// Validated replacement for one durable work-turn identity.
#[derive(Clone, PartialEq, Eq)]
pub struct AgentWorkTurnUpsert {
    pub workspace_id: String,
    pub pane_id: String,
    pub kind: String,
    pub agent_session_id: String,
    pub turn_key: String,
    pub source_offset: u64,
    pub instruction: String,
    pub agent_summary: Option<String>,
    pub messages_json: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub git_change_count: Option<u32>,
    pub state: AgentWorkTurnState,
    pub occurred_at: Option<i64>,
    pub updated_at: i64,
}

impl std::fmt::Debug for AgentWorkTurnUpsert {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentWorkTurnUpsert")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum AgentWorkHistoryMutation {
    Upsert(AgentWorkTurnUpsert),
}

impl std::fmt::Debug for AgentWorkHistoryMutation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentWorkHistoryMutation")
            .field("kind", &"upsert")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct AgentWorkHistoryQuery {
    pub workspace_id: String,
    pub limit: usize,
    pub snapshot_bytes_max: usize,
}

impl AgentWorkHistoryQuery {
    pub fn for_workspace(workspace_id: impl Into<String>) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            limit: AGENT_WORK_TURNS_PER_WORKSPACE_MAX,
            snapshot_bytes_max: AGENT_WORK_HISTORY_SNAPSHOT_BYTES_MAX,
        }
    }
}

impl std::fmt::Debug for AgentWorkHistoryQuery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentWorkHistoryQuery")
            .field("limit", &self.limit)
            .field("snapshot_bytes_max", &self.snapshot_bytes_max)
            .finish()
    }
}

/// Storage-owned structured-thread mutations accepted by [`Db::apply_agent_state_job`].
///
/// The batch is validated in full before SQLite is touched and is committed atomically. Debug
/// output deliberately exposes only the variant so titles, paths, and provider identifiers do not
/// leak into diagnostics.
#[derive(Clone, PartialEq, Eq)]
pub enum StructuredThreadMutation {
    Upsert(StructuredThreadRow),
    SetArchived {
        local_session_id: String,
        archived: bool,
    },
    Delete {
        local_session_id: String,
    },
}

impl std::fmt::Debug for StructuredThreadMutation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            Self::Upsert(_) => "upsert",
            Self::SetArchived { .. } => "set_archived",
            Self::Delete { .. } => "delete",
        };
        formatter
            .debug_struct("StructuredThreadMutation")
            .field("kind", &kind)
            .finish_non_exhaustive()
    }
}

/// One capacity-one AgentStateWorker storage job. Empty mutation vectors make this a projection-
/// only refresh; otherwise every exact mutation and the returned projection share one IMMEDIATE
/// SQLite transaction. Callers must not retry an indeterminate delivery merely because the
/// returned static error code is retryable at a later user/event boundary.
#[derive(Clone, PartialEq, Eq)]
pub struct AgentStateJob {
    /// Active workspace scope for hook, turn, and PTY binding state.
    pub workspace_id: String,
    /// Authoritative structured-thread catalog and mutation scope. This may intentionally differ
    /// from `workspace_id`, but every existing and replacement workspace touched by a structured
    /// mutation must be a member of this bounded set.
    pub structured_workspace_ids: Vec<String>,
    /// Maximum actual heap bytes the caller can accept in the returned snapshot. This output
    /// budget is independent from the separately bounded retained input payload.
    pub snapshot_bytes_max: usize,
    pub binding_reconcile: Option<AgentSessionBindingReconcile>,
    pub stale_binding_deletes: Vec<AgentSessionIdentity>,
    pub turn_done_clears: Vec<AgentTurnDoneClear>,
    pub structured_mutations: Vec<StructuredThreadMutation>,
    pub work_turn_mutations: Vec<AgentWorkHistoryMutation>,
    /// Projects hook sessions and statuslines for the active workspace. False performs no query
    /// or output allocation for either section.
    pub include_hook_status: bool,
    /// Projects waiting and completed-turn attention state. False performs no query or output
    /// allocation for either section.
    pub include_attention: bool,
    /// Projects persisted PTY-to-agent bindings for the active workspace. False performs no query
    /// or output allocation for this section. Binding mutations remain atomic regardless.
    pub include_agent_sessions: bool,
    /// Opt-in bounded cross-workspace pane_id catalog for `agent_sessions` (warm/비활성
    /// 워크스페이스의 「이어가기」 노출 판정용 — pane_id 존재 여부만 필요). False performs no
    /// query or output allocation, keeping non-Restore AgentState jobs free of this global
    /// projection cost.
    pub include_global_agent_sessions: bool,
    /// Projects the bounded structured-thread catalog. False performs no projection query or
    /// output allocation for this section. Structured mutations remain atomic regardless.
    pub include_structured_threads: bool,
    pub include_archived_threads: bool,
    /// Projects the active workspace's bounded work-turn catalog. False performs no query or
    /// output allocation; mutations remain atomic regardless.
    pub include_work_history: bool,
    /// Opt-in complete persisted activity-pane catalog. False performs no activity-pane query or
    /// allocation, keeping non-Catalog AgentState jobs free of this global projection cost.
    pub include_activity_panes: bool,
}

/// Stable payload-free failure returned while preparing an [`AgentStateJob`] for bounded
/// retention outside storage. Raw workspace, session, and thread values never cross this API.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AgentStatePreparationErrorCode {
    InvalidInput,
    ResourceLimit,
}

impl AgentStatePreparationErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::ResourceLimit => "resource_limit",
        }
    }
}

impl std::fmt::Debug for AgentStatePreparationErrorCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::fmt::Display for AgentStatePreparationErrorCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::error::Error for AgentStatePreparationErrorCode {}

/// Actual heap bytes retained by a validated, capacity-canonicalized [`AgentStateJob`]. Debug
/// deliberately omits the value so diagnostics remain low-cardinality.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AgentStateJobRetention {
    retained_bytes: usize,
}

impl AgentStateJobRetention {
    pub const fn retained_bytes(self) -> usize {
        self.retained_bytes
    }
}

impl std::fmt::Debug for AgentStateJobRetention {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AgentStateJobRetention")
    }
}

impl AgentStateJob {
    pub fn projection(workspace_id: impl Into<String>) -> Self {
        let workspace_id = workspace_id.into();
        Self {
            structured_workspace_ids: vec![workspace_id.clone()],
            workspace_id,
            snapshot_bytes_max: AGENT_STATE_SNAPSHOT_BYTES_MAX,
            binding_reconcile: None,
            stale_binding_deletes: Vec::new(),
            turn_done_clears: Vec::new(),
            structured_mutations: Vec::new(),
            work_turn_mutations: Vec::new(),
            include_hook_status: true,
            include_attention: true,
            include_agent_sessions: true,
            include_global_agent_sessions: false,
            include_structured_threads: true,
            include_archived_threads: true,
            include_work_history: false,
            include_activity_panes: false,
        }
    }
}

impl std::fmt::Debug for AgentStateJob {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentStateJob")
            .field(
                "structured_workspace_count",
                &self.structured_workspace_ids.len(),
            )
            .field("snapshot_bytes_max", &self.snapshot_bytes_max)
            .field("has_binding_reconcile", &self.binding_reconcile.is_some())
            .field(
                "stale_binding_delete_count",
                &self.stale_binding_deletes.len(),
            )
            .field("turn_done_clear_count", &self.turn_done_clears.len())
            .field(
                "structured_mutation_count",
                &self.structured_mutations.len(),
            )
            .field("work_turn_mutation_count", &self.work_turn_mutations.len())
            .field("include_hook_status", &self.include_hook_status)
            .field("include_attention", &self.include_attention)
            .field("include_agent_sessions", &self.include_agent_sessions)
            .field(
                "include_structured_threads",
                &self.include_structured_threads,
            )
            .field("include_archived_threads", &self.include_archived_threads)
            .field("include_work_history", &self.include_work_history)
            .field("include_activity_panes", &self.include_activity_panes)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedActivityPane {
    pub workspace_id: String,
    pub pane_id: String,
    pub title: String,
    pub cwd: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudEndedSession {
    pub id: String,
    pub workspace_id: String,
    pub workspace_name: String,
    pub title: String,
}

/// Requested complete-or-error bounded projection returned after an [`AgentStateJob`] commits.
/// Every included row is read from the same transaction snapshot, omitted sections are empty, and
/// total retained heap bytes are capped globally.
#[derive(Clone, PartialEq)]
pub struct AgentStateSnapshot {
    pub hook_sessions: Vec<HookSessionRow>,
    pub statuslines: Vec<StatuslineRow>,
    pub waiting_sessions: Vec<(String, Option<String>)>,
    /// 질문 응답이 필요한 전역 세션. 승인만 남은 세션과 구분한다.
    pub response_sessions: Vec<String>,
    pub turn_done_sessions: Vec<(String, i64)>,
    /// Confirmed idle episodes: key, Unix seconds, and independent microsecond boundary generation.
    pub idle_sessions: Vec<(String, i64, i64)>,
    /// hook 기반 "작업 중" 세션 키(v32) — clear 이벤트(UserPromptSubmit/PreToolUse)가
    /// 기록. waiting처럼 전역(모든 워크스페이스)이며 2분 stale 창으로 자기치유된다.
    pub working_sessions: Vec<String>,
    pub agent_sessions: Vec<AgentSessionRow>,
    /// 전 워크스페이스 스코프에서 실제 resume 명령이 있는 provider의
    /// `(workspace_id, pane_id)` 존재 여부 — warm(비활성) 워크스페이스 사이드바 행의
    /// 「이어가기」 노출 판정용. `include_global_agent_sessions`가 false면 비어 있다.
    pub global_agent_sessions: Vec<(String, String)>,
    pub archived_agent_resume: Vec<ArchivedAgentResumeRow>,
    pub structured_threads: Vec<StructuredThreadRow>,
    pub work_turns: Vec<AgentWorkTurnRow>,
    /// Complete bounded activity catalog when requested; otherwise empty without querying
    /// activity storage.
    pub activity_panes: Vec<PersistedActivityPane>,
}

impl std::fmt::Debug for AgentStateSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentStateSnapshot")
            .field("hook_session_count", &self.hook_sessions.len())
            .field("statusline_count", &self.statuslines.len())
            .field("waiting_session_count", &self.waiting_sessions.len())
            .field("response_session_count", &self.response_sessions.len())
            .field("turn_done_session_count", &self.turn_done_sessions.len())
            .field("idle_session_count", &self.idle_sessions.len())
            .field("working_session_count", &self.working_sessions.len())
            .field("agent_session_count", &self.agent_sessions.len())
            .field(
                "global_agent_session_count",
                &self.global_agent_sessions.len(),
            )
            .field(
                "archived_agent_resume_count",
                &self.archived_agent_resume.len(),
            )
            .field("structured_thread_count", &self.structured_threads.len())
            .field("work_turn_count", &self.work_turns.len())
            .field("activity_pane_count", &self.activity_panes.len())
            .finish()
    }
}

impl AgentStateSnapshot {
    /// Actual heap bytes retained by this storage snapshot, including every Vec backing buffer and
    /// String capacity. `apply_agent_state_job` checks this value before commit, so composition-
    /// root adapters may combine it with separately reserved filesystem output without repeating
    /// storage row formulas.
    pub fn retained_bytes(&self) -> usize {
        agent_state_snapshot_retained_bytes(self).unwrap_or(usize::MAX)
    }
}

const STRUCTURED_THREAD_ID_BYTES_MAX: usize = 1024;
const STRUCTURED_THREAD_CWD_BYTES_MAX: usize = 4 * 1024;
const STRUCTURED_THREAD_MODEL_BYTES_MAX: usize = 1024;
const STRUCTURED_THREAD_ROW_BYTES_MAX: usize = 32 * 1024;
const STRUCTURED_THREADS_RETAINED_BYTES_MAX: usize = 4 * 1024 * 1024;
const STRUCTURED_THREAD_INPUT_INVALID: &str = "structured thread input invalid";
const STRUCTURED_THREAD_ROW_INVALID: &str = "structured thread row invalid";
const STRUCTURED_THREAD_QUERY_FAILED: &str = "structured thread query failed";
const STRUCTURED_THREAD_PERSIST_FAILED: &str = "structured thread persistence failed";

const STRUCTURED_THREADS_BOUNDED_QUERY: &str = "WITH selected AS MATERIALIZED (
        SELECT rowid
          FROM structured_threads
         WHERE workspace_id = ?1 AND (?2 = 1 OR archived = 0)
         ORDER BY favorite DESC, updated_at DESC,
                  substr(CAST(local_session_id AS BLOB), 1, ?4), rowid
         LIMIT ?3
     ), validation AS MATERIALIZED (
        SELECT COALESCE(SUM(CASE WHEN
                   typeof(thread.local_session_id) != 'text'
                OR length(CAST(thread.local_session_id AS BLOB)) NOT BETWEEN 1 AND ?4
                OR typeof(thread.workspace_id) != 'text'
                OR length(CAST(thread.workspace_id AS BLOB)) NOT BETWEEN 1 AND ?4
                OR typeof(thread.thread_id) != 'text'
                OR length(CAST(thread.thread_id AS BLOB)) NOT BETWEEN 1 AND ?4
                OR typeof(thread.title) != 'text'
                OR typeof(thread.cwd) != 'text'
                OR length(CAST(thread.cwd AS BLOB)) > ?5
                OR (typeof(thread.model) NOT IN ('null', 'text'))
                OR (typeof(thread.model) = 'text'
                    AND length(CAST(thread.model AS BLOB)) > ?6)
                OR typeof(thread.favorite) != 'integer'
                OR thread.favorite NOT IN (0, 1)
                OR typeof(thread.archived) != 'integer'
                OR thread.archived NOT IN (0, 1)
                OR typeof(thread.created_at) != 'integer'
                OR typeof(thread.updated_at) != 'integer'
                OR length(CAST(thread.local_session_id AS BLOB))
                   + length(CAST(thread.workspace_id AS BLOB))
                   + length(CAST(thread.thread_id AS BLOB))
                   + length(CAST(thread.title AS BLOB))
                   + length(CAST(thread.cwd AS BLOB))
                   + COALESCE(length(CAST(thread.model AS BLOB)), 0) > ?7
                THEN 1 ELSE 0 END), 0) AS invalid_rows,
               COALESCE(SUM(
                   length(CAST(thread.local_session_id AS BLOB))
                   + length(CAST(thread.workspace_id AS BLOB))
                   + length(CAST(thread.thread_id AS BLOB))
                   + length(CAST(thread.title AS BLOB))
                   + length(CAST(thread.cwd AS BLOB))
                   + COALESCE(length(CAST(thread.model AS BLOB)), 0)
               ), 0) AS retained_bytes
          FROM selected
         CROSS JOIN structured_threads AS thread
         WHERE thread.rowid = selected.rowid
     )
     SELECT 0 AS row_kind,
            NULL AS local_session_id, NULL AS workspace_id, NULL AS thread_id,
            NULL AS title, NULL AS cwd, NULL AS model,
            NULL AS favorite, NULL AS archived,
            NULL AS created_at, NULL AS updated_at,
            validation.invalid_rows, validation.retained_bytes,
            NULL AS selected_rowid
       FROM validation
     UNION ALL
     SELECT 1 AS row_kind,
            thread.local_session_id, thread.workspace_id, thread.thread_id,
            thread.title, thread.cwd, thread.model,
            thread.favorite, thread.archived, thread.created_at, thread.updated_at,
            validation.invalid_rows, validation.retained_bytes,
            selected.rowid AS selected_rowid
       FROM validation
      CROSS JOIN selected
      CROSS JOIN structured_threads AS thread
      WHERE validation.invalid_rows = 0
        AND validation.retained_bytes BETWEEN 0 AND ?8
        AND thread.rowid = selected.rowid
      ORDER BY row_kind, favorite DESC, updated_at DESC, local_session_id";

fn agent_state_structured_scope_sql(workspace_count: usize, select: bool) -> String {
    let requested = std::iter::repeat_n("(?)", workspace_count)
        .collect::<Vec<_>>()
        .join(",");
    if select {
        return format!(
            "WITH requested(workspace_id) AS (VALUES {requested})
             SELECT thread.local_session_id, thread.workspace_id, thread.thread_id,
                    thread.title, thread.cwd, thread.model, thread.favorite, thread.archived,
                    thread.created_at, thread.updated_at
               FROM structured_threads thread
               JOIN requested ON requested.workspace_id = thread.workspace_id
              WHERE (? = 1 OR thread.archived = 0)
              ORDER BY thread.favorite DESC, thread.updated_at DESC,
                       substr(CAST(thread.workspace_id AS BLOB), 1, ?),
                       substr(CAST(thread.local_session_id AS BLOB), 1, ?), thread.rowid
              LIMIT ?"
        );
    }
    format!(
        "WITH requested(workspace_id) AS (VALUES {requested}),
         selected AS MATERIALIZED (
            SELECT thread.rowid
              FROM structured_threads thread
              JOIN requested ON requested.workspace_id = thread.workspace_id
             WHERE (? = 1 OR thread.archived = 0)
             ORDER BY thread.favorite DESC, thread.updated_at DESC,
                      substr(CAST(thread.workspace_id AS BLOB), 1, ?),
                      substr(CAST(thread.local_session_id AS BLOB), 1, ?), thread.rowid
             LIMIT ?
         ), sized AS MATERIALIZED (
            SELECT thread.*,
                   length(CAST(thread.local_session_id AS BLOB))
                   + length(CAST(thread.workspace_id AS BLOB))
                   + length(CAST(thread.thread_id AS BLOB))
                   + length(CAST(thread.title AS BLOB))
                   + length(CAST(thread.cwd AS BLOB))
                   + COALESCE(length(CAST(thread.model AS BLOB)), 0) AS row_bytes
              FROM selected
              JOIN structured_threads AS thread ON thread.rowid = selected.rowid
         )
         SELECT COUNT(*), COALESCE(SUM(CASE WHEN
                    typeof(local_session_id) != 'text'
                 OR length(CAST(local_session_id AS BLOB)) NOT BETWEEN 1 AND ?
                 OR typeof(workspace_id) != 'text'
                 OR length(CAST(workspace_id AS BLOB)) NOT BETWEEN 1 AND ?
                 OR typeof(thread_id) != 'text'
                 OR length(CAST(thread_id AS BLOB)) NOT BETWEEN 1 AND ?
                 OR typeof(title) != 'text'
                 OR typeof(cwd) != 'text' OR length(CAST(cwd AS BLOB)) > ?
                 OR typeof(model) NOT IN ('null', 'text')
                 OR (typeof(model) = 'text' AND length(CAST(model AS BLOB)) > ?)
                 OR typeof(favorite) != 'integer' OR favorite NOT IN (0, 1)
                 OR typeof(archived) != 'integer' OR archived NOT IN (0, 1)
                 OR typeof(created_at) != 'integer' OR typeof(updated_at) != 'integer'
                 OR row_bytes > ? THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
           FROM sized"
    )
}

fn agent_state_structured_mutation_scope_sql(
    workspace_count: usize,
    target_count: usize,
) -> String {
    let workspaces = std::iter::repeat_n("(?)", workspace_count)
        .collect::<Vec<_>>()
        .join(",");
    let targets = std::iter::repeat_n("(?)", target_count)
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "WITH requested_workspace(workspace_id) AS (VALUES {workspaces}),
              target(local_session_id) AS (VALUES {targets})
         SELECT COUNT(*)
           FROM target
           JOIN structured_threads AS thread
             ON thread.local_session_id = target.local_session_id
          WHERE typeof(thread.workspace_id) != 'text'
             OR length(CAST(thread.workspace_id AS BLOB)) NOT BETWEEN 1 AND ?
             OR NOT EXISTS (
                    SELECT 1
                      FROM requested_workspace
                     WHERE requested_workspace.workspace_id = thread.workspace_id
                )"
    )
}

fn structured_thread_id_is_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= STRUCTURED_THREAD_ID_BYTES_MAX
        && !value.bytes().any(|byte| byte.is_ascii_control())
}

fn structured_thread_input_bytes(
    local_session_id: &str,
    workspace_id: &str,
    thread_id: &str,
    title: &str,
    cwd: &str,
    model: Option<&str>,
) -> anyhow::Result<usize> {
    anyhow::ensure!(
        structured_thread_id_is_valid(local_session_id)
            && structured_thread_id_is_valid(workspace_id)
            && structured_thread_id_is_valid(thread_id)
            && cwd.len() <= STRUCTURED_THREAD_CWD_BYTES_MAX
            && !cwd.as_bytes().contains(&0)
            && model.is_none_or(|value| value.len() <= STRUCTURED_THREAD_MODEL_BYTES_MAX),
        STRUCTURED_THREAD_INPUT_INVALID
    );
    let retained_bytes = [
        local_session_id.len(),
        workspace_id.len(),
        thread_id.len(),
        title.len(),
        cwd.len(),
        model.map_or(0, str::len),
    ]
    .into_iter()
    .try_fold(0usize, usize::checked_add)
    .ok_or_else(|| anyhow::anyhow!(STRUCTURED_THREAD_INPUT_INVALID))?;
    anyhow::ensure!(
        retained_bytes <= STRUCTURED_THREAD_ROW_BYTES_MAX,
        STRUCTURED_THREAD_INPUT_INVALID
    );
    Ok(retained_bytes)
}

fn structured_thread_required_text<'row>(
    row: &'row rusqlite::Row<'_>,
    index: usize,
    max_bytes: usize,
    require_nonempty: bool,
    reject_ascii_control: bool,
    reject_nul: bool,
) -> anyhow::Result<&'row str> {
    let rusqlite::types::ValueRef::Text(bytes) = row
        .get_ref(index)
        .map_err(|_| anyhow::anyhow!(STRUCTURED_THREAD_ROW_INVALID))?
    else {
        anyhow::bail!(STRUCTURED_THREAD_ROW_INVALID);
    };
    anyhow::ensure!(
        bytes.len() <= max_bytes
            && (!require_nonempty || !bytes.is_empty())
            && (!reject_ascii_control || !bytes.iter().any(|byte| byte.is_ascii_control()))
            && (!reject_nul || !bytes.contains(&0)),
        STRUCTURED_THREAD_ROW_INVALID
    );
    std::str::from_utf8(bytes).map_err(|_| anyhow::anyhow!(STRUCTURED_THREAD_ROW_INVALID))
}

fn structured_thread_optional_text<'row>(
    row: &'row rusqlite::Row<'_>,
    index: usize,
    max_bytes: usize,
) -> anyhow::Result<Option<&'row str>> {
    match row
        .get_ref(index)
        .map_err(|_| anyhow::anyhow!(STRUCTURED_THREAD_ROW_INVALID))?
    {
        rusqlite::types::ValueRef::Null => Ok(None),
        rusqlite::types::ValueRef::Text(bytes) if bytes.len() <= max_bytes => {
            std::str::from_utf8(bytes)
                .map(Some)
                .map_err(|_| anyhow::anyhow!(STRUCTURED_THREAD_ROW_INVALID))
        }
        _ => anyhow::bail!(STRUCTURED_THREAD_ROW_INVALID),
    }
}

fn structured_thread_integer(row: &rusqlite::Row<'_>, index: usize) -> anyhow::Result<i64> {
    match row
        .get_ref(index)
        .map_err(|_| anyhow::anyhow!(STRUCTURED_THREAD_ROW_INVALID))?
    {
        rusqlite::types::ValueRef::Integer(value) => Ok(value),
        _ => anyhow::bail!(STRUCTURED_THREAD_ROW_INVALID),
    }
}

const BOUNDED_ID_BYTES_MAX: usize = 1024;
const BOUNDED_TEXT_BYTES_MAX: usize = 4 * 1024;
const BOUNDED_MESSAGE_BYTES_MAX: usize = 32 * 1024;
const BOUNDED_ROW_BYTES_MAX: usize = 32 * 1024;
const BOUNDED_RETAINED_BYTES_MAX: usize = 4 * 1024 * 1024;
const HOOK_STATE_ROWS_MAX: usize = 4_096;
const ENV_PROFILE_ROWS_MAX: usize = 256;
const ENV_VAR_ROWS_MAX: usize = 4_096;
const DOTENV_CREDENTIAL_ROWS_MAX: usize = 4_096;
const ENV_API_PROJECT_ROWS_MAX: usize = 256;
const HOOK_PREFIX_ROWS_MAX: usize = 256;
const WAITING_SESSION_ROWS_MAX: usize = 4_096;
const AGENT_SESSION_ROWS_MAX: usize = 256;
/// Maximum authoritative live panes and desired bindings accepted by one worker job.
pub const AGENT_STATE_BINDING_ROWS_MAX: usize = AGENT_SESSION_ROWS_MAX;
/// Exact acknowledgement/CAS continuations accepted in each category per worker job.
pub const AGENT_STATE_EXACT_MUTATIONS_MAX: usize = 8;
/// Maximum structured-thread mutations committed as one all-or-nothing batch.
pub const AGENT_STATE_STRUCTURED_MUTATIONS_MAX: usize = 16;
/// Aggregate structured-thread mutation payload retained by one job.
pub const AGENT_STATE_STRUCTURED_MUTATION_BYTES_MAX: usize = 512 * 1024;
/// Maximum deduplicated workspace IDs in one structured catalog projection.
pub const AGENT_STATE_STRUCTURED_WORKSPACE_MAX: usize = 256;
/// Maximum structured rows returned across the requested workspace catalog.
pub const AGENT_STATE_STRUCTURED_PROJECTION_MAX: usize = 500;
/// Aggregate heap bytes retained by one prepared storage job.
pub const AGENT_STATE_JOB_BYTES_MAX: usize = BOUNDED_RETAINED_BYTES_MAX;
/// Aggregate retained heap bytes across every section of one worker snapshot.
pub const AGENT_STATE_SNAPSHOT_BYTES_MAX: usize = 4 * 1024 * 1024;
const ACTIVITY_PANE_ROWS_MAX: usize = 256 * 256;
/// 전 워크스페이스 스코프 agent_sessions 읽기의 상한. 워크스페이스당 상한
/// (`AGENT_SESSION_ROWS_MAX`)을 워크스페이스 총량 상한(`SETTINGS_WORKSPACE_LIMIT_MAX`)만큼
/// 스케일한다 — `ACTIVITY_PANE_ROWS_MAX`와 같은 관례. 워크스페이스당 상한을 그대로 쓰면
/// 합법적으로 쓴 상태를 읽기에서 거부하게 된다(2026-08-20).
///
/// **이 상한은 실질 경계가 아니다.** 행당 UUID 2개(36 + 36 = 72바이트)라
/// `AGENT_STATE_SNAPSHOT_BYTES_MAX`(4MiB)에 담기는 최대치는 58,254행이고, 이 상한
/// (65,536행)까지 차려면 4,718,592바이트가 필요하다. 즉 최대 합법 상태에서는 행
/// 가드보다 **바이트 가드가 먼저** 터지고, 그 에러는 `apply_agent_state_job` 전체를
/// 빠져나가 활성 워크스페이스의 복원까지 같이 죽인다.
///
/// 알면서 남겨둔 한계다(2026-08-21 코드 리뷰). 발생하려면 워크스페이스 228개에
/// 각각 pane 256개가 필요해 실사용 거리가 아득하고, 기존 `ACTIVITY_PANE_ROWS_MAX`도
/// 같은 상한에 같은 예산이면서 행이 더 커 같은 긴장을 이미 안고 있다 — 도달 불가능한
/// 시나리오 때문에 전역 메모리 가드를 키우는 쪽이 더 나쁜 거래라고 판단했다.
/// 이 경로가 실제로 문제가 되면 예산을 키우기보다 **전역 조회 실패를 국소화**하는
/// 편이 옳다(이 집합은 「이어가기」 버튼 노출 판정용 보조 데이터라, 넘치면 그 기능만
/// 꺼지고 복원은 살아남아야 한다).
const AGENT_SESSIONS_GLOBAL_ROWS_MAX: usize = AGENT_SESSION_ROWS_MAX * SETTINGS_WORKSPACE_LIMIT_MAX;
const WEB_PUSH_SUBSCRIPTION_ROWS_MAX: usize = 8;
const WEB_PUSH_RETAINED_BYTES_MAX: usize = 64 * 1024;
const BOUNDED_READ_INPUT_INVALID: &str = "bounded read input invalid";
const BOUNDED_READ_LIMIT_EXCEEDED: &str = "bounded read limit exceeded";
const BOUNDED_READ_ROW_INVALID: &str = "bounded read row invalid";
const BOUNDED_READ_QUERY_FAILED: &str = "bounded read query failed";
const BOUNDED_WRITE_INPUT_INVALID: &str = "bounded write input invalid";
const BOUNDED_WRITE_FAILED: &str = "bounded write failed";
const AGENT_SESSION_CAPACITY_EXCEEDED: &str = "agent session capacity exceeded";
const AGENT_STATE_INPUT_INVALID: &str = "agent state input invalid";
const AGENT_STATE_PERSIST_FAILED: &str = "agent state persistence failed";
const AGENT_STATE_SNAPSHOT_INVALID: &str = "agent state snapshot invalid";

const ENV_PROFILES_BOUNDED_PREFLIGHT: &str = "WITH selected AS MATERIALIZED (
    SELECT rowid FROM env_profiles WHERE workspace_id = ?1
     ORDER BY created_at, substr(CAST(id AS BLOB), 1, ?3), rowid LIMIT ?2
), sized AS MATERIALIZED (
    SELECT profile.*,
           length(CAST(profile.id AS BLOB)) + length(CAST(profile.name AS BLOB))
           + length(CAST(profile.kind AS BLOB)) AS row_bytes
      FROM selected JOIN env_profiles profile ON profile.rowid = selected.rowid
)
SELECT COUNT(*), COALESCE(SUM(CASE WHEN
       typeof(id) != 'text' OR length(CAST(id AS BLOB)) NOT BETWEEN 1 AND ?3
    OR typeof(name) != 'text' OR length(CAST(name AS BLOB)) > ?4
    OR typeof(kind) != 'text' OR length(CAST(kind AS BLOB)) > ?4
    OR typeof(is_production) != 'integer' OR is_production NOT IN (0, 1)
    OR row_bytes > ?5 THEN 1 ELSE 0 END), 0),
    COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0) FROM sized";
const ENV_PROFILES_BOUNDED_SELECT: &str = "SELECT id, name, kind, is_production
    FROM env_profiles WHERE workspace_id = ?1
    ORDER BY created_at, substr(CAST(id AS BLOB), 1, ?3), rowid LIMIT ?2";

const ENV_VARS_BOUNDED_PREFLIGHT: &str = "WITH selected AS MATERIALIZED (
    SELECT rowid FROM env_vars WHERE profile_id = ?1
     ORDER BY substr(CAST(key AS BLOB), 1, ?4), rowid LIMIT ?2
), sized AS MATERIALIZED (
    SELECT env.*, length(CAST(env.key AS BLOB)) + length(CAST(env.kind AS BLOB))
         + COALESCE(length(CAST(env.plain_value AS BLOB)), 0)
         + COALESCE(length(CAST(env.credential_id AS BLOB)), 0) AS row_bytes
      FROM selected JOIN env_vars env ON env.rowid = selected.rowid
)
SELECT COUNT(*), COALESCE(SUM(CASE WHEN
       typeof(key) != 'text' OR length(CAST(key AS BLOB)) NOT BETWEEN 1 AND ?4
    OR typeof(kind) != 'text' OR kind NOT IN ('plain', 'secret')
    OR typeof(plain_value) NOT IN ('null', 'text')
    OR (typeof(plain_value) = 'text' AND length(CAST(plain_value AS BLOB)) > ?5)
    OR typeof(credential_id) NOT IN ('null', 'text')
    OR (typeof(credential_id) = 'text'
        AND length(CAST(credential_id AS BLOB)) NOT BETWEEN 1 AND ?3)
    OR (kind = 'plain' AND credential_id IS NOT NULL)
    OR (kind = 'secret' AND (plain_value IS NOT NULL OR credential_id IS NULL))
    OR row_bytes > ?5 THEN 1 ELSE 0 END), 0),
    COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0) FROM sized";
const ENV_VARS_BOUNDED_SELECT: &str = "SELECT key, kind, plain_value, credential_id
    FROM env_vars WHERE profile_id = ?1
    ORDER BY substr(CAST(key AS BLOB), 1, ?4), rowid LIMIT ?2";

const DOTENV_CREDENTIALS_BOUNDED_PREFLIGHT: &str = "WITH selected AS MATERIALIZED (
    SELECT credential.rowid
      FROM credentials credential
     WHERE credential.provider = 'env'
     ORDER BY substr(CAST(credential.id AS BLOB), 1, ?2), credential.rowid LIMIT ?1
), sized AS MATERIALIZED (
    SELECT credential.id, length(CAST(credential.id AS BLOB)) AS row_bytes
      FROM selected JOIN credentials credential ON credential.rowid = selected.rowid
)
SELECT COUNT(*), COALESCE(SUM(CASE WHEN
       typeof(id) != 'text' OR length(CAST(id AS BLOB)) NOT BETWEEN 1 AND ?2
    OR row_bytes > ?3 THEN 1 ELSE 0 END), 0),
    COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0) FROM sized";
const DOTENV_CREDENTIALS_BOUNDED_SELECT: &str = "SELECT credential.id
      FROM credentials credential
     WHERE credential.provider = 'env'
     ORDER BY substr(CAST(credential.id AS BLOB), 1, ?2), credential.rowid LIMIT ?1";

const ENV_API_COUNTS_BOUNDED_PREFLIGHT: &str = "WITH selected AS MATERIALIZED (
    SELECT rowid FROM workspaces
     ORDER BY created_at, substr(CAST(id AS BLOB), 1, ?2), rowid LIMIT ?1
), counted AS MATERIALIZED (
    SELECT workspace.id,
           (SELECT COUNT(*) FROM env_profiles profile
             JOIN env_vars env ON env.profile_id = profile.id
            WHERE profile.workspace_id = workspace.id) AS env_count,
           (SELECT COUNT(*) FROM credentials credential
             WHERE (credential.workspace_id IS NULL OR credential.workspace_id = workspace.id)
               AND NOT EXISTS (SELECT 1 FROM env_vars hidden_env
                 JOIN env_profiles hidden_profile ON hidden_profile.id = hidden_env.profile_id
                WHERE hidden_profile.kind = 'dotenv'
                  AND hidden_env.credential_id = credential.id)) AS key_count,
           length(CAST(workspace.id AS BLOB)) AS row_bytes
      FROM selected JOIN workspaces workspace ON workspace.rowid = selected.rowid
)
SELECT COUNT(*), COALESCE(SUM(CASE WHEN
       typeof(id) != 'text' OR length(CAST(id AS BLOB)) NOT BETWEEN 1 AND ?2
    OR typeof(env_count) != 'integer' OR env_count < 0
    OR typeof(key_count) != 'integer' OR key_count < 0
    OR row_bytes > ?3 THEN 1 ELSE 0 END), 0),
    COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0) FROM counted";
const ENV_API_COUNTS_BOUNDED_SELECT: &str = "SELECT workspace.id,
       (SELECT COUNT(*) FROM env_profiles profile JOIN env_vars env ON env.profile_id = profile.id
         WHERE profile.workspace_id = workspace.id),
       (SELECT COUNT(*) FROM credentials credential
         WHERE (credential.workspace_id IS NULL OR credential.workspace_id = workspace.id)
           AND NOT EXISTS (SELECT 1 FROM env_vars hidden_env
             JOIN env_profiles hidden_profile ON hidden_profile.id = hidden_env.profile_id
            WHERE hidden_profile.kind = 'dotenv'
              AND hidden_env.credential_id = credential.id))
    FROM workspaces workspace
    ORDER BY workspace.created_at, substr(CAST(workspace.id AS BLOB), 1, ?2), workspace.rowid
    LIMIT ?1";

const HOOK_SESSIONS_PREFIX_PREFLIGHT: &str = "WITH selected AS MATERIALIZED (
    SELECT rowid FROM agent_hook_sessions
     WHERE updated_at > ?6 - 86400
       AND substr(CAST(session_key AS BLOB), 1, length(CAST(?1 AS BLOB))) = CAST(?1 AS BLOB)
     ORDER BY updated_at DESC, substr(CAST(session_key AS BLOB), 1, ?3), rowid LIMIT ?2
), sized AS MATERIALIZED (
    SELECT hook.*, length(CAST(hook.session_key AS BLOB)) + length(CAST(hook.kind AS BLOB))
         + length(CAST(hook.agent_session_id AS BLOB))
         + length(CAST(hook.transcript_path AS BLOB))
         + COALESCE(length(CAST(hook.task_prompt AS BLOB)), 0) AS row_bytes
      FROM selected JOIN agent_hook_sessions hook ON hook.rowid = selected.rowid
)
SELECT COUNT(*), COALESCE(SUM(CASE WHEN
       typeof(session_key) != 'text' OR length(CAST(session_key AS BLOB)) NOT BETWEEN 1 AND ?3
    OR typeof(kind) != 'text' OR length(CAST(kind AS BLOB)) > ?4
    OR typeof(agent_session_id) != 'text'
       OR length(CAST(agent_session_id AS BLOB)) NOT BETWEEN 1 AND ?3
    OR typeof(transcript_path) != 'text' OR length(CAST(transcript_path AS BLOB)) > ?4
    OR (task_prompt IS NOT NULL AND (typeof(task_prompt) != 'text'
       OR length(CAST(task_prompt AS BLOB)) > 256))
    OR typeof(updated_at) != 'integer' OR row_bytes > ?5 THEN 1 ELSE 0 END), 0),
    COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0) FROM sized";
const HOOK_SESSIONS_PREFIX_SELECT: &str =
    "SELECT session_key, kind, agent_session_id, transcript_path, task_prompt
    FROM agent_hook_sessions
    WHERE updated_at > ?4 - 86400
      AND substr(CAST(session_key AS BLOB), 1, length(CAST(?1 AS BLOB))) = CAST(?1 AS BLOB)
    ORDER BY updated_at DESC, substr(CAST(session_key AS BLOB), 1, ?3), rowid LIMIT ?2";

const STATUSLINES_PREFIX_PREFLIGHT: &str = "WITH selected AS MATERIALIZED (
    SELECT rowid FROM agent_statusline
     WHERE updated_at > ?6 - 3600
       AND substr(CAST(session_key AS BLOB), 1, length(CAST(?1 AS BLOB))) = CAST(?1 AS BLOB)
     ORDER BY updated_at DESC, substr(CAST(session_key AS BLOB), 1, ?3), rowid LIMIT ?2
), sized AS MATERIALIZED (
    SELECT status.*, length(CAST(status.session_key AS BLOB))
         + COALESCE(length(CAST(status.effort AS BLOB)), 0)
         + COALESCE(length(CAST(status.model AS BLOB)), 0) AS row_bytes
      FROM selected JOIN agent_statusline status ON status.rowid = selected.rowid
)
SELECT COUNT(*), COALESCE(SUM(CASE WHEN
       typeof(session_key) != 'text' OR length(CAST(session_key AS BLOB)) NOT BETWEEN 1 AND ?3
    OR typeof(effort) NOT IN ('null', 'text')
       OR (typeof(effort) = 'text' AND length(CAST(effort AS BLOB)) > ?4)
    OR typeof(model) NOT IN ('null', 'text')
       OR (typeof(model) = 'text' AND length(CAST(model AS BLOB)) > ?4)
    OR typeof(context_pct) NOT IN ('null', 'integer')
    OR typeof(updated_at) != 'integer' OR row_bytes > ?5 THEN 1 ELSE 0 END), 0),
    COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0) FROM sized";
const STATUSLINES_PREFIX_SELECT: &str = "SELECT session_key, effort, model, context_pct
    FROM agent_statusline
    WHERE updated_at > ?4 - 3600
      AND substr(CAST(session_key AS BLOB), 1, length(CAST(?1 AS BLOB))) = CAST(?1 AS BLOB)
    ORDER BY updated_at DESC, substr(CAST(session_key AS BLOB), 1, ?3), rowid LIMIT ?2";

// waiting/turn_done의 stale 창은 24시간 — working(120초)과 달리 하트비트가 없다.
// Notification(needs-input)과 Stop(turn-done)은 상태가 바뀌는 순간에만 한 번 발화하므로
// updated_at은 그 시각에 멈춘다. 창이 1시간이면 밤새 승인을 기다린 에이전트나 오래 전
// 끝난 턴이 실제로는 그 상태 그대로인데 배지만 조용히 사라졌다(false negative).
// 죽은 hook의 잔여는 창이 아니라 App의 liveness 필터(attention_session_alive — 세션이
// 아직 살아있는 워크스페이스에 있는지)가 걷어내고, 행 자체는 prune_agent_hook_state(7일)와
// HOOK_STATE_ROWS_MAX eviction이 정리한다. 창은 그 뒤의 마지막 상한일 뿐이다.
const TURN_DONE_PREFIX_PREFLIGHT: &str = "WITH selected AS MATERIALIZED (
    SELECT rowid FROM agent_needs_input
     WHERE turn_done = 1 AND (attention_json IS NOT NULL OR updated_at > ?5 - 86400)
       AND substr(CAST(session_key AS BLOB), 1, length(CAST(?1 AS BLOB))) = CAST(?1 AS BLOB)
     ORDER BY updated_at DESC, substr(CAST(session_key AS BLOB), 1, ?3), rowid LIMIT ?2
), sized AS MATERIALIZED (
    SELECT state.*, length(CAST(state.session_key AS BLOB)) AS row_bytes
      FROM selected JOIN agent_needs_input state ON state.rowid = selected.rowid
)
SELECT COUNT(*), COALESCE(SUM(CASE WHEN
       typeof(session_key) != 'text' OR length(CAST(session_key AS BLOB)) NOT BETWEEN 1 AND ?3
    OR typeof(updated_at) != 'integer' OR typeof(turn_done) != 'integer' OR turn_done != 1
    OR typeof(attention_revision) != 'integer' OR attention_revision < 0
    OR row_bytes > ?4 THEN 1 ELSE 0 END), 0),
    COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0) FROM sized";
const TURN_DONE_PREFIX_SELECT: &str = "SELECT session_key, CASE WHEN attention_json IS NULL THEN updated_at ELSE attention_revision END FROM agent_needs_input
    WHERE turn_done = 1 AND (attention_json IS NOT NULL OR updated_at > ?4 - 86400)
      AND substr(CAST(session_key AS BLOB), 1, length(CAST(?1 AS BLOB))) = CAST(?1 AS BLOB)
    ORDER BY updated_at DESC, substr(CAST(session_key AS BLOB), 1, ?3), rowid LIMIT ?2";

const WAITING_SESSIONS_PREFLIGHT: &str = "WITH selected AS MATERIALIZED (
    SELECT rowid FROM agent_needs_input
     WHERE waiting = 1 AND (attention_json IS NOT NULL OR updated_at > ?5 - 86400)
     ORDER BY updated_at DESC, substr(CAST(session_key AS BLOB), 1, ?2), rowid LIMIT ?1
), sized AS MATERIALIZED (
    SELECT state.*, length(CAST(state.session_key AS BLOB))
         + COALESCE(length(CAST(state.message AS BLOB)), 0) AS row_bytes
      FROM selected JOIN agent_needs_input state ON state.rowid = selected.rowid
)
SELECT COUNT(*), COALESCE(SUM(CASE WHEN
       typeof(session_key) != 'text' OR length(CAST(session_key AS BLOB)) NOT BETWEEN 1 AND ?2
    OR typeof(message) NOT IN ('null', 'text')
       OR (typeof(message) = 'text' AND length(CAST(message AS BLOB)) > ?3)
    OR typeof(updated_at) != 'integer' OR typeof(waiting) != 'integer' OR waiting != 1
    OR typeof(response_required) != 'integer' OR response_required NOT IN (0,1)
    OR row_bytes > ?4 THEN 1 ELSE 0 END), 0),
    COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0) FROM sized";
const WAITING_SESSIONS_SELECT: &str =
    "SELECT session_key, message, response_required FROM agent_needs_input
    WHERE waiting = 1 AND (attention_json IS NOT NULL OR updated_at > ?3 - 86400)
    ORDER BY updated_at DESC, substr(CAST(session_key AS BLOB), 1, ?2), rowid LIMIT ?1";

// 턴 완료(Stop) 세션 — 전역 스코프(warm turn_done 격차, 감사 발견). 기존
// TURN_DONE_PREFIX_*는 워크스페이스 prefix로 좁혀 warm(비활성) 워크스페이스의 완료를
// 놓쳤다 — waiting/working처럼 prefix 없이 전체에서 뽑아야 fleet·사이드바가 warm
// 에이전트의 "완료"도 보인다. stale 창은 waiting과 동일하게 24시간(위 주석).
const TURN_DONE_SESSIONS_PREFLIGHT: &str = "WITH selected AS MATERIALIZED (
    SELECT rowid FROM agent_needs_input
     WHERE turn_done = 1 AND (attention_json IS NOT NULL OR updated_at > ?4 - 86400)
     ORDER BY updated_at DESC, substr(CAST(session_key AS BLOB), 1, ?2), rowid LIMIT ?1
), sized AS MATERIALIZED (
    SELECT state.*, length(CAST(state.session_key AS BLOB)) AS row_bytes
      FROM selected JOIN agent_needs_input state ON state.rowid = selected.rowid
)
SELECT COUNT(*), COALESCE(SUM(CASE WHEN
       typeof(session_key) != 'text' OR length(CAST(session_key AS BLOB)) NOT BETWEEN 1 AND ?2
    OR typeof(updated_at) != 'integer' OR typeof(turn_done) != 'integer' OR turn_done != 1
    OR typeof(attention_revision) != 'integer' OR attention_revision < 0
    OR row_bytes > ?3 THEN 1 ELSE 0 END), 0),
    COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0) FROM sized";
const TURN_DONE_SESSIONS_SELECT: &str = "SELECT session_key, CASE WHEN attention_json IS NULL THEN updated_at ELSE attention_revision END FROM agent_needs_input
    WHERE turn_done = 1 AND (attention_json IS NOT NULL OR updated_at > ?3 - 86400)
    ORDER BY updated_at DESC, substr(CAST(session_key AS BLOB), 1, ?2), rowid LIMIT ?1";

const IDLE_SESSIONS_PREFLIGHT: &str = "WITH selected AS MATERIALIZED (
    SELECT rowid FROM agent_needs_input WHERE idle_since IS NOT NULL
    ORDER BY updated_at DESC, substr(CAST(session_key AS BLOB), 1, ?2), rowid LIMIT ?1
), sized AS MATERIALIZED (
    SELECT state.session_key,state.idle_since,state.working,state.waiting,
        state.idle_generation AS idle_generation,
        length(CAST(state.session_key AS BLOB)) AS row_bytes
    FROM selected JOIN agent_needs_input state ON state.rowid=selected.rowid
)
SELECT COUNT(*), COALESCE(SUM(CASE WHEN
    typeof(session_key)!='text' OR length(CAST(session_key AS BLOB)) NOT BETWEEN 1 AND ?2
    OR instr(session_key,char(0))>0 OR typeof(idle_since)!='integer'
    OR idle_since<0 OR typeof(working)!='integer' OR working!=0
    OR typeof(waiting)!='integer' OR waiting!=0 OR typeof(idle_generation)!='integer'
    OR idle_generation<=0 OR row_bytes>?3 THEN 1 ELSE 0 END),0),
    COALESCE(SUM(row_bytes),0), COALESCE(MAX(row_bytes),0) FROM sized";
const IDLE_SESSIONS_SELECT: &str =
    "SELECT session_key,idle_since,idle_generation FROM agent_needs_input
    WHERE idle_since IS NOT NULL AND idle_since<=?3
    ORDER BY updated_at DESC, substr(CAST(session_key AS BLOB),1,?2),rowid LIMIT ?1";

// hook 기반 "작업 중"(v32). stale 창은 2분 — Stop이 유실되면(Ctrl-C 등 훅 미발화)
// hook_working이 PTY IdleHeuristic까지 눌러 고착 표시가 되므로(병렬 리뷰 H1) 창으로
// 상한을 짧게 건다. 실제 작업 중엔 PreToolUse가 툴 호출마다 updated_at을 갱신해
// (하트비트) 창이 계속 연장되고, 활성 워크스페이스는 만료 후에도 transcript activity가
// 폴백이라 유실 영향이 없다. waiting과 달리 전역(prefix 없음) — warm 워크스페이스
// 세션도 fleet에서 작업 중을 보여야 한다.
const WORKING_SESSIONS_PREFLIGHT: &str = "WITH selected AS MATERIALIZED (
    SELECT rowid FROM agent_needs_input
     WHERE working = 1 AND updated_at > ?4 - 120
     ORDER BY updated_at DESC, substr(CAST(session_key AS BLOB), 1, ?2), rowid LIMIT ?1
), sized AS MATERIALIZED (
    SELECT state.*, length(CAST(state.session_key AS BLOB)) AS row_bytes
      FROM selected JOIN agent_needs_input state ON state.rowid = selected.rowid
)
SELECT COUNT(*), COALESCE(SUM(CASE WHEN
       typeof(session_key) != 'text' OR length(CAST(session_key AS BLOB)) NOT BETWEEN 1 AND ?2
    OR typeof(updated_at) != 'integer' OR typeof(working) != 'integer' OR working != 1
    OR row_bytes > ?3 THEN 1 ELSE 0 END), 0),
    COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0) FROM sized";
const WORKING_SESSIONS_SELECT: &str = "SELECT session_key FROM agent_needs_input
    WHERE working = 1 AND updated_at > ?3 - 120
    ORDER BY updated_at DESC, substr(CAST(session_key AS BLOB), 1, ?2), rowid LIMIT ?1";

const AGENT_SESSIONS_BOUNDED_PREFLIGHT: &str = "WITH selected AS MATERIALIZED (
    SELECT rowid FROM agent_sessions WHERE workspace_id = ?1
     ORDER BY updated_at DESC, substr(CAST(pane_id AS BLOB), 1, ?3), rowid LIMIT ?2
), sized AS MATERIALIZED (
    SELECT session.*, length(CAST(session.pane_id AS BLOB))
         + length(CAST(session.kind AS BLOB)) + length(CAST(session.session_id AS BLOB))
         + COALESCE(length(CAST(session.task_prompt AS BLOB)), 0)
         AS row_bytes
      FROM selected JOIN agent_sessions session ON session.rowid = selected.rowid
)
SELECT COUNT(*), COALESCE(SUM(CASE WHEN
       typeof(pane_id) != 'text' OR length(CAST(pane_id AS BLOB)) NOT BETWEEN 1 AND ?3
    OR typeof(kind) != 'text' OR length(CAST(kind AS BLOB)) > ?4
    OR typeof(session_id) != 'text' OR length(CAST(session_id AS BLOB)) NOT BETWEEN 1 AND ?3
    OR typeof(task_prompt) NOT IN ('null', 'text')
    OR length(CAST(task_prompt AS BLOB)) > 256
    OR typeof(updated_at) != 'integer' OR row_bytes > ?5 THEN 1 ELSE 0 END), 0),
    COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0) FROM sized";
const AGENT_SESSIONS_BOUNDED_SELECT: &str = "SELECT pane_id, kind, session_id, task_prompt
    FROM agent_sessions WHERE workspace_id = ?1
    ORDER BY updated_at DESC, substr(CAST(pane_id AS BLOB), 1, ?3), rowid LIMIT ?2";

// 전 워크스페이스 스코프 — warm(비활성) 사이드바 행의 「이어가기」 노출 판정에는
// 실제 live-pane resume 명령이 있는 Claude/Codex/Grok의 pane_id만 있으면 된다. kind/session_id는
// 전환 후 stage_agent_resume가 새 활성 workspace의 restore_agents에서 다시 읽는다.
// ACTIVITY_PANES_BOUNDED_*
// (전 워크스페이스, LIMIT+tie-breaker)와 동일한 패턴 — 전 워크스페이스로 넓힐수록
// 상한이 더 중요해진다는 원칙을 그대로 따른다.
const AGENT_SESSIONS_GLOBAL_BOUNDED_PREFLIGHT: &str = "WITH selected AS MATERIALIZED (
    SELECT rowid FROM agent_sessions WHERE kind IN ('claude', 'codex', 'grok')
     ORDER BY updated_at DESC,
              substr(CAST(workspace_id AS BLOB), 1, ?2),
              substr(CAST(pane_id AS BLOB), 1, ?2), rowid LIMIT ?1
), sized AS MATERIALIZED (
    SELECT session.workspace_id, session.pane_id,
           length(CAST(session.workspace_id AS BLOB))
             + length(CAST(session.pane_id AS BLOB)) AS row_bytes
      FROM selected JOIN agent_sessions session ON session.rowid = selected.rowid
)
SELECT COUNT(*), COALESCE(SUM(CASE WHEN
       typeof(workspace_id) != 'text' OR length(CAST(workspace_id AS BLOB)) NOT BETWEEN 1 AND ?2
    OR typeof(pane_id) != 'text' OR length(CAST(pane_id AS BLOB)) NOT BETWEEN 1 AND ?2
    OR row_bytes > ?3 THEN 1 ELSE 0 END), 0),
    COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0) FROM sized";
const AGENT_SESSIONS_GLOBAL_BOUNDED_SELECT: &str = "SELECT workspace_id, pane_id
    FROM agent_sessions WHERE kind IN ('claude', 'codex', 'grok')
    ORDER BY updated_at DESC,
             substr(CAST(workspace_id AS BLOB), 1, ?2),
             substr(CAST(pane_id AS BLOB), 1, ?2), rowid LIMIT ?1";

const AGENT_WORK_HISTORY_PREFLIGHT: &str = "WITH selected AS MATERIALIZED (
    SELECT rowid FROM agent_work_turns WHERE workspace_id = ?1
     ORDER BY updated_at DESC, source_offset DESC,
              substr(CAST(kind AS BLOB), 1, ?3),
              substr(CAST(agent_session_id AS BLOB), 1, ?4),
              substr(CAST(turn_key AS BLOB), 1, ?4), rowid LIMIT ?2
), sized AS MATERIALIZED (
    SELECT turn.*,
           length(CAST(turn.workspace_id AS BLOB))
         + length(CAST(turn.pane_id AS BLOB))
         + length(CAST(turn.kind AS BLOB))
         + length(CAST(turn.agent_session_id AS BLOB))
         + length(CAST(turn.turn_key AS BLOB))
         + length(CAST(turn.instruction AS BLOB))
         + COALESCE(length(CAST(turn.agent_summary AS BLOB)), 0)
         + (CASE WHEN typeof(turn.messages_json) = 'text'
                  AND length(CAST(turn.messages_json AS BLOB)) <= ?10
                  AND instr(turn.messages_json, char(0)) = 0
                 THEN length(CAST(turn.messages_json AS BLOB)) ELSE 0 END)
         + COALESCE(length(CAST(turn.model AS BLOB)), 0)
         + COALESCE(length(CAST(turn.effort AS BLOB)), 0)
         + COALESCE(length(CAST(turn.cwd AS BLOB)), 0)
         + COALESCE(length(CAST(turn.branch AS BLOB)), 0) AS row_bytes
      FROM selected JOIN agent_work_turns turn ON turn.rowid = selected.rowid
)
SELECT COUNT(*), COALESCE(SUM(CASE WHEN
       typeof(workspace_id) != 'text'
       OR length(CAST(workspace_id AS BLOB)) NOT BETWEEN 1 AND ?4
       OR instr(workspace_id, char(0)) != 0
    OR typeof(pane_id) != 'text' OR length(CAST(pane_id AS BLOB)) NOT BETWEEN 1 AND ?4
       OR instr(pane_id, char(0)) != 0
    OR typeof(kind) != 'text' OR length(CAST(kind AS BLOB)) NOT BETWEEN 1 AND ?3
       OR kind GLOB '*[^a-z0-9_-]*'
    OR typeof(agent_session_id) != 'text'
       OR length(CAST(agent_session_id AS BLOB)) NOT BETWEEN 1 AND ?4
       OR instr(agent_session_id, char(0)) != 0
    OR typeof(turn_key) != 'text' OR length(CAST(turn_key AS BLOB)) NOT BETWEEN 1 AND ?4
       OR instr(turn_key, char(0)) != 0
    OR typeof(source_offset) != 'integer' OR source_offset < 0
    OR typeof(instruction) != 'text'
       OR length(CAST(instruction AS BLOB)) NOT BETWEEN 1 AND ?5
       OR instr(instruction, char(0)) != 0
    OR typeof(agent_summary) NOT IN ('null', 'text')
       OR (typeof(agent_summary) = 'text'
           AND (length(CAST(agent_summary AS BLOB)) > ?6 OR instr(agent_summary, char(0)) != 0))
    -- messages_json은 여기서 행을 무효화하지 않는다 — 상한 초과·NUL·미지 타입은 컬럼만
    -- None으로 떨어뜨린다(아래 read_agent_work_history의 관대한 읽기, 스펙 §3-2 fail-soft).
    OR typeof(model) NOT IN ('null', 'text')
       OR (typeof(model) = 'text'
           AND (length(CAST(model AS BLOB)) > ?7 OR instr(model, char(0)) != 0))
    OR typeof(effort) NOT IN ('null', 'text')
       OR (typeof(effort) = 'text'
           AND (length(CAST(effort AS BLOB)) > ?7 OR instr(effort, char(0)) != 0))
    OR typeof(cwd) NOT IN ('null', 'text')
       OR (typeof(cwd) = 'text'
           AND (length(CAST(cwd AS BLOB)) > ?8 OR instr(cwd, char(0)) != 0))
    OR typeof(branch) NOT IN ('null', 'text')
       OR (typeof(branch) = 'text'
           AND (length(CAST(branch AS BLOB)) > ?7 OR instr(branch, char(0)) != 0))
    OR typeof(git_change_count) NOT IN ('null', 'integer')
       OR (typeof(git_change_count) = 'integer'
           AND git_change_count NOT BETWEEN 0 AND 4294967295)
    OR typeof(state) != 'text' OR state NOT IN ('working', 'waiting', 'completed')
    OR typeof(occurred_at) NOT IN ('null', 'integer')
    OR typeof(updated_at) != 'integer' OR updated_at < 0
    OR row_bytes > ?9 THEN 1 ELSE 0 END), 0),
    COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0) FROM sized";

const AGENT_WORK_HISTORY_SELECT: &str = "SELECT workspace_id, pane_id, kind,
           agent_session_id, turn_key, source_offset, instruction, agent_summary,
           model, effort, cwd, branch, git_change_count, state, occurred_at, updated_at,
           messages_json
      FROM agent_work_turns WHERE workspace_id = ?1
     ORDER BY updated_at DESC, source_offset DESC,
              substr(CAST(kind AS BLOB), 1, ?3),
              substr(CAST(agent_session_id AS BLOB), 1, ?4),
              substr(CAST(turn_key AS BLOB), 1, ?4), rowid LIMIT ?2";

const ARCHIVED_AGENT_RESUME_PREFLIGHT: &str = "WITH selected AS MATERIALIZED (
    SELECT session.rowid AS session_rowid, pane.id AS pane_id
      FROM sessions session
      JOIN mux_panes pane
        ON pane.workspace_id = session.workspace_id AND pane.session_id = session.id
     WHERE session.workspace_id = ?1 AND session.session_kind = 'agent'
     ORDER BY session.updated_at DESC,
              substr(CAST(session.id AS BLOB), 1, ?3), session.rowid
     LIMIT ?2
), sized AS MATERIALIZED (
    SELECT session.id AS persistent_session_id, session.agent_id,
           binding.kind, binding.session_id,
           length(CAST(session.id AS BLOB)) + length(CAST(session.agent_id AS BLOB))
             + COALESCE(length(CAST(binding.kind AS BLOB)), 0)
             + COALESCE(length(CAST(binding.session_id AS BLOB)), 0) AS row_bytes
      FROM selected
      JOIN sessions session ON session.rowid = selected.session_rowid
      LEFT JOIN agent_sessions binding
        ON binding.workspace_id = session.workspace_id AND binding.pane_id = selected.pane_id
)
SELECT COUNT(*), COALESCE(SUM(CASE WHEN
       typeof(persistent_session_id) != 'text'
       OR length(CAST(persistent_session_id AS BLOB)) NOT BETWEEN 1 AND ?3
    OR typeof(agent_id) != 'text' OR length(CAST(agent_id AS BLOB)) NOT BETWEEN 1 AND ?3
    OR typeof(kind) NOT IN ('null', 'text')
    OR (typeof(kind) = 'text' AND length(CAST(kind AS BLOB)) NOT BETWEEN 1 AND ?4)
    OR typeof(session_id) NOT IN ('null', 'text')
    OR (typeof(session_id) = 'text'
        AND length(CAST(session_id AS BLOB)) NOT BETWEEN 1 AND ?3)
    OR (kind IS NULL) != (session_id IS NULL)
    OR row_bytes > ?5 THEN 1 ELSE 0 END), 0),
    COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0) FROM sized";
const ARCHIVED_AGENT_RESUME_SELECT: &str = "SELECT session.id, session.agent_id,
           binding.kind, binding.session_id
      FROM sessions session
      JOIN mux_panes pane
        ON pane.workspace_id = session.workspace_id AND pane.session_id = session.id
      LEFT JOIN agent_sessions binding
        ON binding.workspace_id = session.workspace_id AND binding.pane_id = pane.id
     WHERE session.workspace_id = ?1 AND session.session_kind = 'agent'
     ORDER BY session.updated_at DESC,
              substr(CAST(session.id AS BLOB), 1, ?3), session.rowid
     LIMIT ?2";

const ACTIVITY_PANES_BOUNDED_PREFLIGHT: &str = "WITH selected AS MATERIALIZED (
    SELECT pane.rowid FROM mux_panes pane
     ORDER BY substr(CAST(pane.workspace_id AS BLOB), 1, ?2),
              substr(CAST(pane.created_at AS BLOB), 1, ?3),
              substr(CAST(pane.id AS BLOB), 1, ?2), pane.rowid LIMIT ?1
), sized AS MATERIALIZED (
    SELECT pane.workspace_id, pane.id AS pane_id, pane.created_at,
           COALESCE(NULLIF(pane.title, ''), NULLIF(session.title, ''), pane.id) AS title,
           COALESCE(session.cwd, '') AS cwd,
           length(CAST(pane.workspace_id AS BLOB))
             + length(CAST(pane.id AS BLOB))
             + length(CAST(COALESCE(NULLIF(pane.title, ''),
                                    NULLIF(session.title, ''), pane.id) AS BLOB))
             + length(CAST(COALESCE(session.cwd, '') AS BLOB)) AS row_bytes
      FROM selected JOIN mux_panes pane ON pane.rowid = selected.rowid
      LEFT JOIN sessions session ON session.id = pane.session_id
)
SELECT COUNT(*), COALESCE(SUM(CASE WHEN
       typeof(workspace_id) != 'text'
       OR length(CAST(workspace_id AS BLOB)) NOT BETWEEN 1 AND ?2
    OR typeof(pane_id) != 'text' OR length(CAST(pane_id AS BLOB)) NOT BETWEEN 1 AND ?2
    OR typeof(created_at) != 'text' OR length(CAST(created_at AS BLOB)) > ?3
    OR typeof(title) != 'text' OR length(CAST(title AS BLOB)) NOT BETWEEN 1 AND ?3
    OR typeof(cwd) != 'text' OR length(CAST(cwd AS BLOB)) > ?3
    OR row_bytes > ?4 THEN 1 ELSE 0 END), 0),
    COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0) FROM sized";
const ACTIVITY_PANES_BOUNDED_SELECT: &str = "SELECT pane.workspace_id, pane.id,
            COALESCE(NULLIF(pane.title, ''), NULLIF(session.title, ''), pane.id),
            COALESCE(session.cwd, '')
       FROM mux_panes pane
       LEFT JOIN sessions session ON session.id = pane.session_id
      ORDER BY substr(CAST(pane.workspace_id AS BLOB), 1, ?2),
               substr(CAST(pane.created_at AS BLOB), 1, ?3),
               substr(CAST(pane.id AS BLOB), 1, ?2), pane.rowid LIMIT ?1";

const WEB_PUSH_BOUNDED_PREFLIGHT: &str = "WITH selected AS MATERIALIZED (
    SELECT rowid FROM web_push_subscriptions
     ORDER BY created_at, substr(CAST(endpoint AS BLOB), 1, ?2), rowid LIMIT ?1
), sized AS MATERIALIZED (
    SELECT subscription.*,
           length(CAST(subscription.endpoint AS BLOB))
             + length(CAST(subscription.p256dh AS BLOB))
             + length(CAST(subscription.auth AS BLOB)) AS row_bytes
      FROM selected JOIN web_push_subscriptions subscription
        ON subscription.rowid = selected.rowid
)
SELECT COUNT(*), COALESCE(SUM(CASE WHEN
       typeof(endpoint) != 'text' OR length(CAST(endpoint AS BLOB)) NOT BETWEEN 1 AND ?2
    OR typeof(p256dh) != 'text' OR length(CAST(p256dh AS BLOB)) NOT BETWEEN 1 AND ?3
    OR typeof(auth) != 'text' OR length(CAST(auth AS BLOB)) NOT BETWEEN 1 AND ?3
    OR typeof(created_at) != 'integer'
    OR row_bytes > ?4 THEN 1 ELSE 0 END), 0),
    COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0) FROM sized";
const WEB_PUSH_BOUNDED_SELECT: &str = "SELECT endpoint, p256dh, auth
    FROM web_push_subscriptions
    ORDER BY created_at, substr(CAST(endpoint AS BLOB), 1, ?2), rowid LIMIT ?1";

#[derive(Debug, Clone, Copy)]
struct BoundedReadProbe {
    count: usize,
    retained_bytes: usize,
}

fn bounded_id_is_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= BOUNDED_ID_BYTES_MAX
        && !value.bytes().any(|byte| byte.is_ascii_control())
}

fn bounded_session_key_prefix(session_key: &str) -> anyhow::Result<&str> {
    anyhow::ensure!(
        bounded_id_is_valid(session_key),
        BOUNDED_WRITE_INPUT_INVALID
    );
    let (workspace_id, session_id) = session_key
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!(BOUNDED_WRITE_INPUT_INVALID))?;
    anyhow::ensure!(
        bounded_id_is_valid(workspace_id)
            && bounded_id_is_valid(session_id)
            && workspace_id.len() < BOUNDED_ID_BYTES_MAX,
        BOUNDED_WRITE_INPUT_INVALID
    );
    Ok(&session_key[..workspace_id.len() + 1])
}

fn bounded_limit_plus_one(limit: usize, max: usize) -> anyhow::Result<i64> {
    anyhow::ensure!(limit <= max, BOUNDED_READ_INPUT_INVALID);
    let plus_one = limit
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!(BOUNDED_READ_INPUT_INVALID))?;
    i64::try_from(plus_one).map_err(|_| anyhow::anyhow!(BOUNDED_READ_INPUT_INVALID))
}

fn bounded_text_is_valid(value: &str, max_bytes: usize) -> bool {
    value.len() <= max_bytes && !value.as_bytes().contains(&0)
}

fn bounded_input_row_bytes(fields: &[&str]) -> anyhow::Result<usize> {
    let bytes = fields
        .iter()
        .map(|field| field.len())
        .try_fold(0usize, usize::checked_add)
        .ok_or_else(|| anyhow::anyhow!(BOUNDED_WRITE_INPUT_INVALID))?;
    anyhow::ensure!(bytes <= BOUNDED_ROW_BYTES_MAX, BOUNDED_WRITE_INPUT_INVALID);
    Ok(bytes)
}

fn bounded_read_preflight<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    limit: usize,
) -> anyhow::Result<BoundedReadProbe> {
    bounded_read_preflight_with_budget(conn, sql, params, limit, BOUNDED_RETAINED_BYTES_MAX)
}

fn bounded_read_preflight_with_budget<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    limit: usize,
    retained_bytes_max: usize,
) -> anyhow::Result<BoundedReadProbe> {
    let raw = conn
        .query_row(sql, params, |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
    let count = usize::try_from(raw.0).map_err(|_| anyhow::anyhow!(BOUNDED_READ_ROW_INVALID))?;
    let invalid_rows =
        usize::try_from(raw.1).map_err(|_| anyhow::anyhow!(BOUNDED_READ_ROW_INVALID))?;
    let retained_bytes =
        usize::try_from(raw.2).map_err(|_| anyhow::anyhow!(BOUNDED_READ_ROW_INVALID))?;
    let max_row_bytes =
        usize::try_from(raw.3).map_err(|_| anyhow::anyhow!(BOUNDED_READ_ROW_INVALID))?;
    anyhow::ensure!(count <= limit, BOUNDED_READ_LIMIT_EXCEEDED);
    anyhow::ensure!(
        invalid_rows == 0
            && retained_bytes <= retained_bytes_max
            && max_row_bytes <= BOUNDED_ROW_BYTES_MAX,
        BOUNDED_READ_ROW_INVALID
    );
    Ok(BoundedReadProbe {
        count,
        retained_bytes,
    })
}

fn bounded_snapshot_epoch(conn: &Connection) -> anyhow::Result<i64> {
    let epoch = conn
        .query_row("SELECT CAST(strftime('%s','now') AS INTEGER)", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
    anyhow::ensure!(epoch >= 0, BOUNDED_READ_ROW_INVALID);
    Ok(epoch)
}

fn bounded_required_text<'row>(
    row: &'row rusqlite::Row<'_>,
    index: usize,
    max_bytes: usize,
    require_nonempty: bool,
    reject_ascii_control: bool,
) -> anyhow::Result<&'row str> {
    let rusqlite::types::ValueRef::Text(bytes) = row
        .get_ref(index)
        .map_err(|_| anyhow::anyhow!(BOUNDED_READ_ROW_INVALID))?
    else {
        anyhow::bail!(BOUNDED_READ_ROW_INVALID);
    };
    anyhow::ensure!(
        bytes.len() <= max_bytes
            && (!require_nonempty || !bytes.is_empty())
            && !bytes.contains(&0)
            && (!reject_ascii_control || !bytes.iter().any(|byte| byte.is_ascii_control())),
        BOUNDED_READ_ROW_INVALID
    );
    std::str::from_utf8(bytes).map_err(|_| anyhow::anyhow!(BOUNDED_READ_ROW_INVALID))
}

fn bounded_optional_text<'row>(
    row: &'row rusqlite::Row<'_>,
    index: usize,
    max_bytes: usize,
) -> anyhow::Result<Option<&'row str>> {
    match row
        .get_ref(index)
        .map_err(|_| anyhow::anyhow!(BOUNDED_READ_ROW_INVALID))?
    {
        rusqlite::types::ValueRef::Null => Ok(None),
        rusqlite::types::ValueRef::Text(bytes)
            if bytes.len() <= max_bytes && !bytes.contains(&0) =>
        {
            std::str::from_utf8(bytes)
                .map(Some)
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_ROW_INVALID))
        }
        _ => anyhow::bail!(BOUNDED_READ_ROW_INVALID),
    }
}

/// `messages_json` 전용 관대한 읽기. 다른 선택 필드가 쓰는 `bounded_optional_text`와 달리
/// 상한 초과·NUL 포함·비UTF-8·예상 밖 타입을 만나도 행을 버리지 않고 이 컬럼만 None으로
/// 낮춘다 — 이력 하나가 패널을 죽이지 않는다는 스펙 §3-2 fail-soft 계약을 지킨다.
fn agent_work_turn_messages_json_lenient(row: &rusqlite::Row<'_>, index: usize) -> Option<String> {
    match row.get_ref(index).ok()? {
        rusqlite::types::ValueRef::Text(bytes)
            if bytes.len() <= AGENT_WORK_TURN_MESSAGES_BYTES_MAX && !bytes.contains(&0) =>
        {
            std::str::from_utf8(bytes).ok().map(str::to_owned)
        }
        _ => None,
    }
}

fn bounded_integer(row: &rusqlite::Row<'_>, index: usize) -> anyhow::Result<i64> {
    match row
        .get_ref(index)
        .map_err(|_| anyhow::anyhow!(BOUNDED_READ_ROW_INVALID))?
    {
        rusqlite::types::ValueRef::Integer(value) => Ok(value),
        _ => anyhow::bail!(BOUNDED_READ_ROW_INVALID),
    }
}

fn bounded_optional_integer(row: &rusqlite::Row<'_>, index: usize) -> anyhow::Result<Option<i64>> {
    match row
        .get_ref(index)
        .map_err(|_| anyhow::anyhow!(BOUNDED_READ_ROW_INVALID))?
    {
        rusqlite::types::ValueRef::Null => Ok(None),
        rusqlite::types::ValueRef::Integer(value) => Ok(Some(value)),
        _ => anyhow::bail!(BOUNDED_READ_ROW_INVALID),
    }
}

fn agent_work_provider_is_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= AGENT_WORK_TURN_PROVIDER_BYTES_MAX
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_".contains(&byte))
}

fn agent_work_optional_text_is_valid(value: Option<&str>, max_bytes: usize) -> bool {
    value.is_none_or(|value| value.len() <= max_bytes && !value.as_bytes().contains(&0))
}

/// `messages_json`은 스펙 §3-2가 fail-soft를 못박은 유일한 선택 필드다 — 상한(8KB) 초과나 NUL은
/// 행 전체를 거부하지 않고 이 컬럼만 저장 시점에 None으로 낮춘다("이력 하나가 패널을 죽이지
/// 않는다"). 다른 선택 필드(agent_summary 등)는 여전히 `agent_work_optional_text_is_valid`로
/// 행 전체를 거부하는 기존 동작을 유지한다.
fn agent_work_turn_messages_json_effective(value: Option<&str>) -> Option<&str> {
    value.filter(|value| {
        value.len() <= AGENT_WORK_TURN_MESSAGES_BYTES_MAX && !value.as_bytes().contains(&0)
    })
}

fn agent_work_turn_input_bytes(row: &AgentWorkTurnUpsert) -> anyhow::Result<usize> {
    anyhow::ensure!(
        bounded_id_is_valid(&row.workspace_id)
            && bounded_id_is_valid(&row.pane_id)
            && agent_work_provider_is_valid(&row.kind)
            && bounded_id_is_valid(&row.agent_session_id)
            && bounded_id_is_valid(&row.turn_key)
            && row.source_offset <= i64::MAX as u64
            && !row.instruction.is_empty()
            && row.instruction.len() <= AGENT_WORK_TURN_INSTRUCTION_BYTES_MAX
            && !row.instruction.as_bytes().contains(&0)
            && agent_work_optional_text_is_valid(
                row.agent_summary.as_deref(),
                AGENT_WORK_TURN_SUMMARY_BYTES_MAX,
            )
            // messages_json은 여기서 검증하지 않는다 — 상한 초과·NUL은 행을 거부하는 대신
            // agent_work_turn_messages_json_effective가 쓰기 시점에 컬럼만 None으로 낮춘다.
            && agent_work_optional_text_is_valid(
                row.model.as_deref(),
                AGENT_WORK_TURN_METADATA_BYTES_MAX,
            )
            && agent_work_optional_text_is_valid(
                row.effort.as_deref(),
                AGENT_WORK_TURN_METADATA_BYTES_MAX,
            )
            && agent_work_optional_text_is_valid(row.cwd.as_deref(), AGENT_WORK_TURN_CWD_BYTES_MAX,)
            && agent_work_optional_text_is_valid(
                row.branch.as_deref(),
                AGENT_WORK_TURN_METADATA_BYTES_MAX,
            )
            && row.updated_at >= 0,
        AGENT_WORK_HISTORY_INPUT_INVALID
    );
    let bytes = [
        row.workspace_id.len(),
        row.pane_id.len(),
        row.kind.len(),
        row.agent_session_id.len(),
        row.turn_key.len(),
        row.instruction.len(),
        row.agent_summary.as_deref().map_or(0, str::len),
        agent_work_turn_messages_json_effective(row.messages_json.as_deref()).map_or(0, str::len),
        row.model.as_deref().map_or(0, str::len),
        row.effort.as_deref().map_or(0, str::len),
        row.cwd.as_deref().map_or(0, str::len),
        row.branch.as_deref().map_or(0, str::len),
    ]
    .into_iter()
    .try_fold(0usize, usize::checked_add)
    .ok_or_else(|| anyhow::anyhow!(AGENT_WORK_HISTORY_INPUT_INVALID))?;
    anyhow::ensure!(
        bytes <= AGENT_WORK_TURN_ROW_BYTES_MAX,
        AGENT_WORK_HISTORY_INPUT_INVALID
    );
    Ok(bytes)
}

fn validate_agent_work_history_query(query: &AgentWorkHistoryQuery) -> anyhow::Result<()> {
    anyhow::ensure!(
        bounded_id_is_valid(&query.workspace_id)
            && query.limit <= AGENT_WORK_TURNS_PER_WORKSPACE_MAX
            && (1..=AGENT_WORK_HISTORY_SNAPSHOT_BYTES_MAX).contains(&query.snapshot_bytes_max),
        AGENT_WORK_HISTORY_INPUT_INVALID
    );
    Ok(())
}

fn agent_work_history_probe(
    conn: &Connection,
    query: &AgentWorkHistoryQuery,
) -> anyhow::Result<(BoundedReadProbe, i64)> {
    validate_agent_work_history_query(query)?;
    let sql_limit = bounded_limit_plus_one(query.limit, AGENT_WORK_TURNS_PER_WORKSPACE_MAX)
        .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_INPUT_INVALID))?;
    let probe = bounded_read_preflight_with_budget(
        conn,
        AGENT_WORK_HISTORY_PREFLIGHT,
        rusqlite::params![
            query.workspace_id,
            sql_limit,
            AGENT_WORK_TURN_PROVIDER_BYTES_MAX as i64,
            AGENT_WORK_TURN_ID_BYTES_MAX as i64,
            AGENT_WORK_TURN_INSTRUCTION_BYTES_MAX as i64,
            AGENT_WORK_TURN_SUMMARY_BYTES_MAX as i64,
            AGENT_WORK_TURN_METADATA_BYTES_MAX as i64,
            AGENT_WORK_TURN_CWD_BYTES_MAX as i64,
            AGENT_WORK_TURN_ROW_BYTES_MAX as i64,
            AGENT_WORK_TURN_MESSAGES_BYTES_MAX as i64,
        ],
        query.limit,
        query.snapshot_bytes_max,
    )
    .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_ROW_INVALID))?;
    Ok((probe, sql_limit))
}

fn read_agent_work_history(
    conn: &Connection,
    query: &AgentWorkHistoryQuery,
    probe: BoundedReadProbe,
    sql_limit: i64,
) -> anyhow::Result<Vec<AgentWorkTurnRow>> {
    let mut result = Vec::with_capacity(probe.count);
    let mut stmt = conn
        .prepare(AGENT_WORK_HISTORY_SELECT)
        .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_QUERY_FAILED))?;
    let mut rows = stmt
        .query(rusqlite::params![
            query.workspace_id,
            sql_limit,
            AGENT_WORK_TURN_PROVIDER_BYTES_MAX as i64,
            AGENT_WORK_TURN_ID_BYTES_MAX as i64,
        ])
        .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_QUERY_FAILED))?;
    while let Some(row) = rows
        .next()
        .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_QUERY_FAILED))?
    {
        let source_offset = u64::try_from(
            bounded_integer(row, 5).map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_ROW_INVALID))?,
        )
        .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_ROW_INVALID))?;
        let git_change_count = bounded_optional_integer(row, 12)
            .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_ROW_INVALID))?
            .map(u32::try_from)
            .transpose()
            .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_ROW_INVALID))?;
        let state = AgentWorkTurnState::from_str(
            bounded_required_text(row, 13, 9, true, true)
                .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_ROW_INVALID))?,
        )?;
        result.push(AgentWorkTurnRow {
            workspace_id: bounded_required_text(row, 0, AGENT_WORK_TURN_ID_BYTES_MAX, true, true)
                .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_ROW_INVALID))?
                .to_owned(),
            pane_id: bounded_required_text(row, 1, AGENT_WORK_TURN_ID_BYTES_MAX, true, true)
                .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_ROW_INVALID))?
                .to_owned(),
            kind: bounded_required_text(row, 2, AGENT_WORK_TURN_PROVIDER_BYTES_MAX, true, true)
                .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_ROW_INVALID))?
                .to_owned(),
            agent_session_id: bounded_required_text(
                row,
                3,
                AGENT_WORK_TURN_ID_BYTES_MAX,
                true,
                true,
            )
            .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_ROW_INVALID))?
            .to_owned(),
            turn_key: bounded_required_text(row, 4, AGENT_WORK_TURN_ID_BYTES_MAX, true, true)
                .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_ROW_INVALID))?
                .to_owned(),
            source_offset,
            instruction: bounded_required_text(
                row,
                6,
                AGENT_WORK_TURN_INSTRUCTION_BYTES_MAX,
                true,
                false,
            )
            .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_ROW_INVALID))?
            .to_owned(),
            agent_summary: bounded_optional_text(row, 7, AGENT_WORK_TURN_SUMMARY_BYTES_MAX)
                .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_ROW_INVALID))?
                .map(str::to_owned),
            messages_json: agent_work_turn_messages_json_lenient(row, 16),
            model: bounded_optional_text(row, 8, AGENT_WORK_TURN_METADATA_BYTES_MAX)
                .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_ROW_INVALID))?
                .map(str::to_owned),
            effort: bounded_optional_text(row, 9, AGENT_WORK_TURN_METADATA_BYTES_MAX)
                .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_ROW_INVALID))?
                .map(str::to_owned),
            cwd: bounded_optional_text(row, 10, AGENT_WORK_TURN_CWD_BYTES_MAX)
                .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_ROW_INVALID))?
                .map(str::to_owned),
            branch: bounded_optional_text(row, 11, AGENT_WORK_TURN_METADATA_BYTES_MAX)
                .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_ROW_INVALID))?
                .map(str::to_owned),
            git_change_count,
            state,
            occurred_at: bounded_optional_integer(row, 14)
                .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_ROW_INVALID))?,
            updated_at: bounded_integer(row, 15)
                .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_ROW_INVALID))?,
        });
    }
    Ok(result)
}

fn agent_session_identity_input_bytes(
    pane_id: &str,
    kind: &str,
    session_id: &str,
) -> anyhow::Result<usize> {
    anyhow::ensure!(
        bounded_id_is_valid(pane_id)
            && bounded_id_is_valid(session_id)
            && !kind.is_empty()
            && bounded_text_is_valid(kind, BOUNDED_TEXT_BYTES_MAX),
        AGENT_STATE_INPUT_INVALID
    );
    bounded_input_row_bytes(&[pane_id, kind, session_id])
        .map_err(|_| anyhow::anyhow!(AGENT_STATE_INPUT_INVALID))
}

fn agent_session_row_input_bytes(row: &AgentSessionRow) -> anyhow::Result<usize> {
    let base = agent_session_identity_input_bytes(&row.pane_id, &row.kind, &row.session_id)?;
    let prompt = row.task_prompt.as_deref().unwrap_or_default();
    anyhow::ensure!(
        prompt.is_empty() || (prompt.len() <= 256 && task_prompt_is_displayable(prompt)),
        AGENT_STATE_INPUT_INVALID
    );
    base.checked_add(prompt.len())
        .filter(|bytes| *bytes <= BOUNDED_ROW_BYTES_MAX)
        .ok_or_else(|| anyhow::anyhow!(AGENT_STATE_INPUT_INVALID))
}

fn canonicalize_agent_state_string(value: &mut String) {
    *value = std::mem::take(value).into_boxed_str().into_string();
}

fn canonicalize_agent_state_optional_string(value: &mut Option<String>) {
    if let Some(value) = value {
        canonicalize_agent_state_string(value);
    }
}

fn canonicalize_agent_state_vec<T>(values: &mut Vec<T>) {
    *values = std::mem::take(values).into_boxed_slice().into_vec();
}

fn canonicalize_agent_session_row(row: &mut AgentSessionRow) {
    canonicalize_agent_state_string(&mut row.pane_id);
    canonicalize_agent_state_string(&mut row.kind);
    canonicalize_agent_state_string(&mut row.session_id);
    canonicalize_agent_state_optional_string(&mut row.task_prompt);
}

fn canonicalize_structured_thread_row(row: &mut StructuredThreadRow) {
    canonicalize_agent_state_string(&mut row.local_session_id);
    canonicalize_agent_state_string(&mut row.workspace_id);
    canonicalize_agent_state_string(&mut row.thread_id);
    canonicalize_agent_state_string(&mut row.title);
    canonicalize_agent_state_string(&mut row.cwd);
    canonicalize_agent_state_optional_string(&mut row.model);
}

fn canonicalize_agent_work_turn_upsert(row: &mut AgentWorkTurnUpsert) {
    canonicalize_agent_state_string(&mut row.workspace_id);
    canonicalize_agent_state_string(&mut row.pane_id);
    canonicalize_agent_state_string(&mut row.kind);
    canonicalize_agent_state_string(&mut row.agent_session_id);
    canonicalize_agent_state_string(&mut row.turn_key);
    canonicalize_agent_state_string(&mut row.instruction);
    canonicalize_agent_state_optional_string(&mut row.agent_summary);
    canonicalize_agent_state_optional_string(&mut row.model);
    canonicalize_agent_state_optional_string(&mut row.effort);
    canonicalize_agent_state_optional_string(&mut row.cwd);
    canonicalize_agent_state_optional_string(&mut row.branch);
}

fn canonicalize_agent_state_job(job: &mut AgentStateJob) {
    canonicalize_agent_state_string(&mut job.workspace_id);
    for workspace_id in &mut job.structured_workspace_ids {
        canonicalize_agent_state_string(workspace_id);
    }
    canonicalize_agent_state_vec(&mut job.structured_workspace_ids);

    if let Some(reconcile) = &mut job.binding_reconcile {
        for pane_id in &mut reconcile.live_pane_ids {
            canonicalize_agent_state_string(pane_id);
        }
        canonicalize_agent_state_vec(&mut reconcile.live_pane_ids);
        for row in &mut reconcile.desired_bindings {
            canonicalize_agent_session_row(row);
        }
        canonicalize_agent_state_vec(&mut reconcile.desired_bindings);
    }
    for identity in &mut job.stale_binding_deletes {
        canonicalize_agent_state_string(&mut identity.pane_id);
        canonicalize_agent_state_string(&mut identity.kind);
        canonicalize_agent_state_string(&mut identity.session_id);
    }
    canonicalize_agent_state_vec(&mut job.stale_binding_deletes);
    for clear in &mut job.turn_done_clears {
        canonicalize_agent_state_string(&mut clear.session_key);
    }
    canonicalize_agent_state_vec(&mut job.turn_done_clears);
    for mutation in &mut job.structured_mutations {
        match mutation {
            StructuredThreadMutation::Upsert(row) => canonicalize_structured_thread_row(row),
            StructuredThreadMutation::SetArchived {
                local_session_id, ..
            }
            | StructuredThreadMutation::Delete { local_session_id } => {
                canonicalize_agent_state_string(local_session_id);
            }
        }
    }
    canonicalize_agent_state_vec(&mut job.structured_mutations);
    for mutation in &mut job.work_turn_mutations {
        let AgentWorkHistoryMutation::Upsert(row) = mutation;
        canonicalize_agent_work_turn_upsert(row);
    }
    canonicalize_agent_state_vec(&mut job.work_turn_mutations);
}

fn checked_agent_state_retained_add(
    total: &mut usize,
    additional: usize,
) -> Result<(), AgentStatePreparationErrorCode> {
    *total = total
        .checked_add(additional)
        .ok_or(AgentStatePreparationErrorCode::ResourceLimit)?;
    Ok(())
}

fn checked_agent_state_vec_allocation<T>(
    total: &mut usize,
    values: &Vec<T>,
) -> Result<(), AgentStatePreparationErrorCode> {
    let bytes = values
        .capacity()
        .checked_mul(std::mem::size_of::<T>())
        .ok_or(AgentStatePreparationErrorCode::ResourceLimit)?;
    checked_agent_state_retained_add(total, bytes)
}

fn checked_agent_state_string_capacity(
    total: &mut usize,
    value: &String,
) -> Result<(), AgentStatePreparationErrorCode> {
    checked_agent_state_retained_add(total, value.capacity())
}

fn checked_agent_state_optional_string_capacity(
    total: &mut usize,
    value: &Option<String>,
) -> Result<(), AgentStatePreparationErrorCode> {
    if let Some(value) = value {
        checked_agent_state_string_capacity(total, value)?;
    }
    Ok(())
}

fn agent_work_history_retained_bytes(
    rows: &Vec<AgentWorkTurnRow>,
) -> Result<usize, AgentStatePreparationErrorCode> {
    let mut total = 0usize;
    checked_agent_state_vec_allocation(&mut total, rows)?;
    for row in rows {
        for value in [
            &row.workspace_id,
            &row.pane_id,
            &row.kind,
            &row.agent_session_id,
            &row.turn_key,
            &row.instruction,
        ] {
            checked_agent_state_string_capacity(&mut total, value)?;
        }
        for value in [
            &row.agent_summary,
            &row.model,
            &row.effort,
            &row.cwd,
            &row.branch,
        ] {
            checked_agent_state_optional_string_capacity(&mut total, value)?;
        }
    }
    Ok(total)
}

fn agent_state_job_retained_bytes(
    job: &AgentStateJob,
) -> Result<usize, AgentStatePreparationErrorCode> {
    let mut total = std::mem::size_of::<AgentStateJob>();
    checked_agent_state_string_capacity(&mut total, &job.workspace_id)?;
    checked_agent_state_vec_allocation(&mut total, &job.structured_workspace_ids)?;
    for workspace_id in &job.structured_workspace_ids {
        checked_agent_state_string_capacity(&mut total, workspace_id)?;
    }
    if let Some(reconcile) = &job.binding_reconcile {
        checked_agent_state_vec_allocation(&mut total, &reconcile.live_pane_ids)?;
        for pane_id in &reconcile.live_pane_ids {
            checked_agent_state_string_capacity(&mut total, pane_id)?;
        }
        checked_agent_state_vec_allocation(&mut total, &reconcile.desired_bindings)?;
        for row in &reconcile.desired_bindings {
            checked_agent_state_string_capacity(&mut total, &row.pane_id)?;
            checked_agent_state_string_capacity(&mut total, &row.kind)?;
            checked_agent_state_string_capacity(&mut total, &row.session_id)?;
            checked_agent_state_optional_string_capacity(&mut total, &row.task_prompt)?;
        }
    }
    checked_agent_state_vec_allocation(&mut total, &job.stale_binding_deletes)?;
    for identity in &job.stale_binding_deletes {
        checked_agent_state_string_capacity(&mut total, &identity.pane_id)?;
        checked_agent_state_string_capacity(&mut total, &identity.kind)?;
        checked_agent_state_string_capacity(&mut total, &identity.session_id)?;
    }
    checked_agent_state_vec_allocation(&mut total, &job.turn_done_clears)?;
    for clear in &job.turn_done_clears {
        checked_agent_state_string_capacity(&mut total, &clear.session_key)?;
    }
    checked_agent_state_vec_allocation(&mut total, &job.structured_mutations)?;
    for mutation in &job.structured_mutations {
        match mutation {
            StructuredThreadMutation::Upsert(row) => {
                checked_agent_state_string_capacity(&mut total, &row.local_session_id)?;
                checked_agent_state_string_capacity(&mut total, &row.workspace_id)?;
                checked_agent_state_string_capacity(&mut total, &row.thread_id)?;
                checked_agent_state_string_capacity(&mut total, &row.title)?;
                checked_agent_state_string_capacity(&mut total, &row.cwd)?;
                checked_agent_state_optional_string_capacity(&mut total, &row.model)?;
            }
            StructuredThreadMutation::SetArchived {
                local_session_id, ..
            }
            | StructuredThreadMutation::Delete { local_session_id } => {
                checked_agent_state_string_capacity(&mut total, local_session_id)?;
            }
        }
    }
    checked_agent_state_vec_allocation(&mut total, &job.work_turn_mutations)?;
    for mutation in &job.work_turn_mutations {
        let AgentWorkHistoryMutation::Upsert(row) = mutation;
        for value in [
            &row.workspace_id,
            &row.pane_id,
            &row.kind,
            &row.agent_session_id,
            &row.turn_key,
            &row.instruction,
        ] {
            checked_agent_state_string_capacity(&mut total, value)?;
        }
        for value in [
            &row.agent_summary,
            &row.model,
            &row.effort,
            &row.cwd,
            &row.branch,
        ] {
            checked_agent_state_optional_string_capacity(&mut total, value)?;
        }
    }
    Ok(total)
}

fn agent_state_snapshot_retained_bytes(
    snapshot: &AgentStateSnapshot,
) -> Result<usize, AgentStatePreparationErrorCode> {
    let mut total = std::mem::size_of::<AgentStateSnapshot>();
    checked_agent_state_vec_allocation(&mut total, &snapshot.hook_sessions)?;
    for row in &snapshot.hook_sessions {
        checked_agent_state_string_capacity(&mut total, &row.session_key)?;
        checked_agent_state_string_capacity(&mut total, &row.kind)?;
        checked_agent_state_string_capacity(&mut total, &row.agent_session_id)?;
        checked_agent_state_string_capacity(&mut total, &row.transcript_path)?;
    }
    checked_agent_state_vec_allocation(&mut total, &snapshot.statuslines)?;
    for row in &snapshot.statuslines {
        checked_agent_state_string_capacity(&mut total, &row.session_key)?;
        checked_agent_state_optional_string_capacity(&mut total, &row.effort)?;
        checked_agent_state_optional_string_capacity(&mut total, &row.model)?;
    }
    checked_agent_state_vec_allocation(&mut total, &snapshot.waiting_sessions)?;
    for (session_key, message) in &snapshot.waiting_sessions {
        checked_agent_state_string_capacity(&mut total, session_key)?;
        checked_agent_state_optional_string_capacity(&mut total, message)?;
    }
    checked_agent_state_vec_allocation(&mut total, &snapshot.response_sessions)?;
    for key in &snapshot.response_sessions {
        checked_agent_state_string_capacity(&mut total, key)?;
    }
    checked_agent_state_vec_allocation(&mut total, &snapshot.turn_done_sessions)?;
    for (session_key, _) in &snapshot.turn_done_sessions {
        checked_agent_state_string_capacity(&mut total, session_key)?;
    }
    checked_agent_state_vec_allocation(&mut total, &snapshot.idle_sessions)?;
    for (key, _, _) in &snapshot.idle_sessions {
        checked_agent_state_string_capacity(&mut total, key)?;
    }
    checked_agent_state_vec_allocation(&mut total, &snapshot.working_sessions)?;
    for session_key in &snapshot.working_sessions {
        checked_agent_state_string_capacity(&mut total, session_key)?;
    }
    checked_agent_state_vec_allocation(&mut total, &snapshot.agent_sessions)?;
    for row in &snapshot.agent_sessions {
        checked_agent_state_string_capacity(&mut total, &row.pane_id)?;
        checked_agent_state_string_capacity(&mut total, &row.kind)?;
        checked_agent_state_string_capacity(&mut total, &row.session_id)?;
        checked_agent_state_optional_string_capacity(&mut total, &row.task_prompt)?;
    }
    checked_agent_state_vec_allocation(&mut total, &snapshot.global_agent_sessions)?;
    for (workspace_id, pane_id) in &snapshot.global_agent_sessions {
        checked_agent_state_string_capacity(&mut total, workspace_id)?;
        checked_agent_state_string_capacity(&mut total, pane_id)?;
    }
    checked_agent_state_vec_allocation(&mut total, &snapshot.archived_agent_resume)?;
    for row in &snapshot.archived_agent_resume {
        checked_agent_state_string_capacity(&mut total, &row.persistent_session_id)?;
        checked_agent_state_string_capacity(&mut total, &row.agent_id)?;
        checked_agent_state_optional_string_capacity(&mut total, &row.kind)?;
        checked_agent_state_optional_string_capacity(&mut total, &row.session_id)?;
    }
    checked_agent_state_vec_allocation(&mut total, &snapshot.structured_threads)?;
    for row in &snapshot.structured_threads {
        checked_agent_state_string_capacity(&mut total, &row.local_session_id)?;
        checked_agent_state_string_capacity(&mut total, &row.workspace_id)?;
        checked_agent_state_string_capacity(&mut total, &row.thread_id)?;
        checked_agent_state_string_capacity(&mut total, &row.title)?;
        checked_agent_state_string_capacity(&mut total, &row.cwd)?;
        checked_agent_state_optional_string_capacity(&mut total, &row.model)?;
    }
    checked_agent_state_retained_add(
        &mut total,
        agent_work_history_retained_bytes(&snapshot.work_turns)?,
    )?;
    checked_agent_state_vec_allocation(&mut total, &snapshot.activity_panes)?;
    for row in &snapshot.activity_panes {
        checked_agent_state_string_capacity(&mut total, &row.workspace_id)?;
        checked_agent_state_string_capacity(&mut total, &row.pane_id)?;
        checked_agent_state_string_capacity(&mut total, &row.title)?;
        checked_agent_state_string_capacity(&mut total, &row.cwd)?;
    }
    Ok(total)
}

/// Validates an AgentState job before mutation, canonicalizes every owned backing allocation to
/// its bounded logical length, and returns the actual heap bytes retained by the prepared value.
/// Invalid input is left untouched; success is ready for bounded worker retention and one
/// `Db::apply_agent_state_job` call.
pub fn prepare_agent_state_job_for_retention(
    job: &mut AgentStateJob,
) -> Result<AgentStateJobRetention, AgentStatePreparationErrorCode> {
    validate_agent_state_job(job).map_err(|_| AgentStatePreparationErrorCode::InvalidInput)?;
    canonicalize_agent_state_job(job);
    let retained_bytes = agent_state_job_retained_bytes(job)?;
    if retained_bytes > AGENT_STATE_JOB_BYTES_MAX {
        return Err(AgentStatePreparationErrorCode::ResourceLimit);
    }
    Ok(AgentStateJobRetention { retained_bytes })
}

fn validate_agent_state_job(job: &AgentStateJob) -> anyhow::Result<String> {
    anyhow::ensure!(
        bounded_id_is_valid(&job.workspace_id)
            && (1..=AGENT_STATE_SNAPSHOT_BYTES_MAX).contains(&job.snapshot_bytes_max)
            && job.stale_binding_deletes.len() <= AGENT_STATE_EXACT_MUTATIONS_MAX
            && job.turn_done_clears.len() <= AGENT_STATE_EXACT_MUTATIONS_MAX
            && job.structured_mutations.len() <= AGENT_STATE_STRUCTURED_MUTATIONS_MAX
            && job.work_turn_mutations.len() <= AGENT_WORK_TURN_BATCH_MAX,
        AGENT_STATE_INPUT_INVALID
    );
    let workspace_prefix = format!("{}:", job.workspace_id);

    let mut retained_input_bytes = job.workspace_id.len();
    anyhow::ensure!(
        !job.structured_workspace_ids.is_empty()
            && job.structured_workspace_ids.len() <= AGENT_STATE_STRUCTURED_WORKSPACE_MAX,
        AGENT_STATE_INPUT_INVALID
    );
    let mut structured_workspaces =
        std::collections::HashSet::with_capacity(job.structured_workspace_ids.len());
    for workspace_id in &job.structured_workspace_ids {
        anyhow::ensure!(
            bounded_id_is_valid(workspace_id)
                && structured_workspaces.insert(workspace_id.as_str()),
            AGENT_STATE_INPUT_INVALID
        );
        retained_input_bytes = retained_input_bytes
            .checked_add(workspace_id.len())
            .ok_or_else(|| anyhow::anyhow!(AGENT_STATE_INPUT_INVALID))?;
    }
    if let Some(reconcile) = &job.binding_reconcile {
        anyhow::ensure!(
            reconcile.live_pane_ids.len() <= AGENT_STATE_BINDING_ROWS_MAX
                && reconcile.desired_bindings.len() <= AGENT_STATE_BINDING_ROWS_MAX,
            AGENT_STATE_INPUT_INVALID
        );
        let mut live = std::collections::HashSet::with_capacity(reconcile.live_pane_ids.len());
        for pane_id in &reconcile.live_pane_ids {
            anyhow::ensure!(
                bounded_id_is_valid(pane_id) && live.insert(pane_id.as_str()),
                AGENT_STATE_INPUT_INVALID
            );
            retained_input_bytes = retained_input_bytes
                .checked_add(pane_id.len())
                .ok_or_else(|| anyhow::anyhow!(AGENT_STATE_INPUT_INVALID))?;
        }
        let mut desired =
            std::collections::HashSet::with_capacity(reconcile.desired_bindings.len());
        for row in &reconcile.desired_bindings {
            anyhow::ensure!(
                live.contains(row.pane_id.as_str()) && desired.insert(row.pane_id.as_str()),
                AGENT_STATE_INPUT_INVALID
            );
            retained_input_bytes = retained_input_bytes
                .checked_add(agent_session_row_input_bytes(row)?)
                .ok_or_else(|| anyhow::anyhow!(AGENT_STATE_INPUT_INVALID))?;
        }
    }

    for identity in &job.stale_binding_deletes {
        retained_input_bytes = retained_input_bytes
            .checked_add(agent_session_identity_input_bytes(
                &identity.pane_id,
                &identity.kind,
                &identity.session_id,
            )?)
            .ok_or_else(|| anyhow::anyhow!(AGENT_STATE_INPUT_INVALID))?;
    }
    for clear in &job.turn_done_clears {
        anyhow::ensure!(
            clear.seen_at >= 0
                && bounded_session_key_prefix(&clear.session_key)
                    .is_ok_and(|prefix| prefix == workspace_prefix),
            AGENT_STATE_INPUT_INVALID
        );
        retained_input_bytes = retained_input_bytes
            .checked_add(clear.session_key.len())
            .ok_or_else(|| anyhow::anyhow!(AGENT_STATE_INPUT_INVALID))?;
    }

    let mut structured_bytes = 0usize;
    for mutation in &job.structured_mutations {
        let mutation_bytes = match mutation {
            StructuredThreadMutation::Upsert(row) => {
                let input_bytes = structured_thread_input_bytes(
                    &row.local_session_id,
                    &row.workspace_id,
                    &row.thread_id,
                    &row.title,
                    &row.cwd,
                    row.model.as_deref(),
                )
                .map_err(|_| anyhow::anyhow!(AGENT_STATE_INPUT_INVALID))?;
                anyhow::ensure!(
                    structured_workspaces.contains(row.workspace_id.as_str()),
                    AGENT_STATE_INPUT_INVALID
                );
                input_bytes
            }
            StructuredThreadMutation::SetArchived {
                local_session_id, ..
            }
            | StructuredThreadMutation::Delete { local_session_id } => {
                anyhow::ensure!(
                    structured_thread_id_is_valid(local_session_id),
                    AGENT_STATE_INPUT_INVALID
                );
                local_session_id.len()
            }
        };
        structured_bytes = structured_bytes
            .checked_add(mutation_bytes)
            .ok_or_else(|| anyhow::anyhow!(AGENT_STATE_INPUT_INVALID))?;
    }
    anyhow::ensure!(
        structured_bytes <= AGENT_STATE_STRUCTURED_MUTATION_BYTES_MAX,
        AGENT_STATE_INPUT_INVALID
    );
    retained_input_bytes = retained_input_bytes
        .checked_add(structured_bytes)
        .ok_or_else(|| anyhow::anyhow!(AGENT_STATE_INPUT_INVALID))?;

    let mut work_turn_bytes = 0usize;
    for mutation in &job.work_turn_mutations {
        let AgentWorkHistoryMutation::Upsert(row) = mutation;
        anyhow::ensure!(
            row.workspace_id == job.workspace_id,
            AGENT_STATE_INPUT_INVALID
        );
        work_turn_bytes = work_turn_bytes
            .checked_add(
                agent_work_turn_input_bytes(row)
                    .map_err(|_| anyhow::anyhow!(AGENT_STATE_INPUT_INVALID))?,
            )
            .ok_or_else(|| anyhow::anyhow!(AGENT_STATE_INPUT_INVALID))?;
    }
    anyhow::ensure!(
        work_turn_bytes <= AGENT_WORK_TURN_BATCH_BYTES_MAX,
        AGENT_STATE_INPUT_INVALID
    );
    retained_input_bytes = retained_input_bytes
        .checked_add(work_turn_bytes)
        .ok_or_else(|| anyhow::anyhow!(AGENT_STATE_INPUT_INVALID))?;
    anyhow::ensure!(
        retained_input_bytes <= AGENT_STATE_JOB_BYTES_MAX,
        AGENT_STATE_INPUT_INVALID
    );
    Ok(workspace_prefix)
}

fn validate_agent_state_structured_existing_scope(
    tx: &rusqlite::Transaction<'_>,
    job: &AgentStateJob,
) -> anyhow::Result<()> {
    let mut seen = std::collections::HashSet::with_capacity(job.structured_mutations.len());
    let mut targets = Vec::with_capacity(job.structured_mutations.len());
    for mutation in &job.structured_mutations {
        let local_session_id = match mutation {
            StructuredThreadMutation::Upsert(row) => row.local_session_id.as_str(),
            StructuredThreadMutation::SetArchived {
                local_session_id, ..
            }
            | StructuredThreadMutation::Delete { local_session_id } => local_session_id.as_str(),
        };
        if seen.insert(local_session_id) {
            targets.push(local_session_id);
        }
    }
    if targets.is_empty() {
        return Ok(());
    }

    let sql = agent_state_structured_mutation_scope_sql(
        job.structured_workspace_ids.len(),
        targets.len(),
    );
    let mut params = Vec::with_capacity(job.structured_workspace_ids.len() + targets.len() + 1);
    params.extend(
        job.structured_workspace_ids
            .iter()
            .cloned()
            .map(rusqlite::types::Value::Text),
    );
    params.extend(
        targets
            .into_iter()
            .map(str::to_owned)
            .map(rusqlite::types::Value::Text),
    );
    params.push(rusqlite::types::Value::Integer(
        STRUCTURED_THREAD_ID_BYTES_MAX as i64,
    ));
    let invalid_count = tx
        .query_row(&sql, rusqlite::params_from_iter(&params), |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|_| anyhow::anyhow!(AGENT_STATE_PERSIST_FAILED))?;
    anyhow::ensure!(invalid_count == 0, AGENT_STATE_INPUT_INVALID);
    Ok(())
}

#[derive(Clone, Copy)]
enum HookStateTable {
    HookSessions,
    NeedsInput,
    Statusline,
}

fn evict_hook_state_overflow(
    tx: &rusqlite::Transaction<'_>,
    table: HookStateTable,
    protected_session_key: &str,
) -> anyhow::Result<()> {
    let sql = match table {
        HookStateTable::HookSessions => {
            "DELETE FROM agent_hook_sessions
              WHERE rowid IN (
                    SELECT rowid FROM agent_hook_sessions
                     WHERE session_key != ?1
                     ORDER BY updated_at ASC,
                              substr(CAST(session_key AS BLOB), 1, 1024), rowid
                     LIMIT MAX((SELECT COUNT(*) FROM agent_hook_sessions) - ?2, 0)
              )"
        }
        HookStateTable::NeedsInput => {
            "DELETE FROM agent_needs_input
              WHERE rowid IN (
                    SELECT rowid FROM agent_needs_input
                     WHERE session_key != ?1
                     ORDER BY updated_at ASC,
                              substr(CAST(session_key AS BLOB), 1, 1024), rowid
                     LIMIT MAX((SELECT COUNT(*) FROM agent_needs_input) - ?2, 0)
              )"
        }
        HookStateTable::Statusline => {
            "DELETE FROM agent_statusline
              WHERE rowid IN (
                    SELECT rowid FROM agent_statusline
                     WHERE session_key != ?1
                     ORDER BY updated_at ASC,
                              substr(CAST(session_key AS BLOB), 1, 1024), rowid
                     LIMIT MAX((SELECT COUNT(*) FROM agent_statusline) - ?2, 0)
              )"
        }
    };
    tx.execute(
        sql,
        rusqlite::params![protected_session_key, HOOK_STATE_ROWS_MAX as i64],
    )
    .map_err(|_| anyhow::anyhow!(BOUNDED_WRITE_FAILED))?;
    Ok(())
}

fn evict_hook_state_prefix_overflow(
    tx: &rusqlite::Transaction<'_>,
    table: HookStateTable,
    workspace_prefix: &str,
    protected_session_key: &str,
) -> anyhow::Result<()> {
    let sql = match table {
        HookStateTable::HookSessions => {
            "DELETE FROM agent_hook_sessions
              WHERE rowid IN (
                    SELECT rowid FROM agent_hook_sessions
                     WHERE substr(CAST(session_key AS BLOB), 1,
                                  length(CAST(?1 AS BLOB))) = CAST(?1 AS BLOB)
                       AND session_key != ?2
                     ORDER BY updated_at DESC,
                              substr(CAST(session_key AS BLOB), 1, 1024) DESC,
                              rowid DESC
                     LIMIT -1 OFFSET CASE WHEN EXISTS (
                         SELECT 1 FROM agent_hook_sessions
                          WHERE substr(CAST(session_key AS BLOB), 1,
                                       length(CAST(?1 AS BLOB))) = CAST(?1 AS BLOB)
                            AND session_key = ?2
                     ) THEN ?3 - 1 ELSE ?3 END
              )"
        }
        HookStateTable::NeedsInput => {
            "DELETE FROM agent_needs_input
              WHERE rowid IN (
                    SELECT rowid FROM agent_needs_input
                     WHERE substr(CAST(session_key AS BLOB), 1,
                                  length(CAST(?1 AS BLOB))) = CAST(?1 AS BLOB)
                       AND session_key != ?2
                     ORDER BY updated_at DESC,
                              substr(CAST(session_key AS BLOB), 1, 1024) DESC,
                              rowid DESC
                     LIMIT -1 OFFSET CASE WHEN EXISTS (
                         SELECT 1 FROM agent_needs_input
                          WHERE substr(CAST(session_key AS BLOB), 1,
                                       length(CAST(?1 AS BLOB))) = CAST(?1 AS BLOB)
                            AND session_key = ?2
                     ) THEN ?3 - 1 ELSE ?3 END
              )"
        }
        HookStateTable::Statusline => {
            "DELETE FROM agent_statusline
              WHERE rowid IN (
                    SELECT rowid FROM agent_statusline
                     WHERE substr(CAST(session_key AS BLOB), 1,
                                  length(CAST(?1 AS BLOB))) = CAST(?1 AS BLOB)
                       AND session_key != ?2
                     ORDER BY updated_at DESC,
                              substr(CAST(session_key AS BLOB), 1, 1024) DESC,
                              rowid DESC
                     LIMIT -1 OFFSET CASE WHEN EXISTS (
                         SELECT 1 FROM agent_statusline
                          WHERE substr(CAST(session_key AS BLOB), 1,
                                       length(CAST(?1 AS BLOB))) = CAST(?1 AS BLOB)
                            AND session_key = ?2
                     ) THEN ?3 - 1 ELSE ?3 END
              )"
        }
    };
    tx.execute(
        sql,
        rusqlite::params![
            workspace_prefix,
            protected_session_key,
            HOOK_PREFIX_ROWS_MAX as i64,
        ],
    )
    .map_err(|_| anyhow::anyhow!(BOUNDED_WRITE_FAILED))?;
    Ok(())
}

/// hook이 보고한 세션 바인딩 행 (v15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSessionRow {
    pub session_key: String,
    pub kind: String,
    pub agent_session_id: String,
    pub transcript_path: String,
    pub task_prompt: Option<String>,
}

/// claude statusLine 표시 정보 한 행 (v17).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatuslineRow {
    pub session_key: String,
    pub effort: Option<String>,
    pub model: Option<String>,
    pub context_pct: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CredentialMeta {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub credential_kind: String,
    pub masked_hint: Option<String>,
    /// 소속 workspace — None이면 전역 공유(커넥터/OAuth·레거시). (#2, v19)
    pub workspace_id: Option<String>,
}

/// logical credential id가 가리키는 keyring physical slot. 실제 secret 값은 포함하지 않는다.
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialSecretLocation {
    pub keyring_service: String,
    pub keyring_username: String,
}

impl std::fmt::Debug for CredentialSecretLocation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialSecretLocation")
            .field("coordinate", &"REDACTED")
            .finish()
    }
}

/// Durable lifecycle of one exact physical keyring bundle base slot. Secret values and derived
/// refresh/DCR entry contents never enter storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhysicalSecretSlotState {
    Staging,
    Published,
    Orphan,
}

impl PhysicalSecretSlotState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Published => "published",
            Self::Orphan => "orphan",
        }
    }

    fn from_persisted(value: &str) -> anyhow::Result<Self> {
        match value {
            "staging" => Ok(Self::Staging),
            "published" => Ok(Self::Published),
            "orphan" => Ok(Self::Orphan),
            _ => anyhow::bail!("알 수 없는 physical secret slot state"),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PhysicalSecretSlotLedgerRow {
    /// 행 재생성마다 바뀌는 공개 복구 세대. startup snapshot의 ABA 방지용이다.
    pub recovery_generation: [u8; 16],
    pub logical_credential_id: String,
    pub physical_slot: String,
    pub state: PhysicalSecretSlotState,
    /// Exact legacy keyring base username. Callers derive only the fixed `.refresh` and `.dcr`
    /// suffixes after this bounded durable coordinate has been read. Secret values are absent.
    pub legacy_cleanup_username: Option<String>,
}

impl std::fmt::Debug for PhysicalSecretSlotLedgerRow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PhysicalSecretSlotLedgerRow")
            .field("logical_credential_id", &"REDACTED")
            .field("physical_slot", &"REDACTED")
            .field("state", &self.state)
            .field(
                "legacy_cleanup_username",
                &self.legacy_cleanup_username.as_ref().map(|_| "REDACTED"),
            )
            .finish()
    }
}

/// Bootstrap/migration view of one credential and its logical-to-physical keyring pointer.
/// Secret values are never loaded into this DTO. The pointer remains a string because pre-IN01
/// databases can still contain the legacy logical credential id in `keyring_username`.
#[derive(Debug, Clone, PartialEq)]
pub struct CredentialSecretRecord {
    pub meta: CredentialMeta,
    pub keyring_service: String,
    pub keyring_username: String,
    pub oauth_json: Option<String>,
}

/// One candidate OAuth binding selected by `oauth_json.server_id`. Values are nonsecret durable
/// metadata and a keyring coordinate only; the secret bundle is never loaded. The raw metadata is
/// preserved so the app adapter can validate endpoint/auth-method binding fail-closed.
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialOAuthBindingRecord {
    pub logical_id: String,
    pub keyring_service: String,
    pub physical_pointer: String,
    pub oauth_metadata_json: String,
}

impl std::fmt::Debug for CredentialOAuthBindingRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialOAuthBindingRecord")
            .field("logical_id", &"REDACTED")
            .field("keyring_service", &self.keyring_service)
            .field("physical_pointer", &"REDACTED")
            .field("oauth_metadata_json", &"REDACTED")
            .finish()
    }
}

pub const CREDENTIAL_OAUTH_BINDING_BYTES_MAX: usize = 64 * 1024;
pub const CREDENTIAL_SECRET_LOCATION_BYTES_MAX: usize = 1024;
pub const CREDENTIAL_SECRET_RECORD_BYTES_MAX: usize = 1024 * 1024;
/// Maximum credential bindings retained by one stdio MCP server. This matches the frozen
/// Connector tools/item ceiling and cannot be raised by runtime configuration.
pub const MCP_REQUEST_TARGET_CREDENTIAL_LIMIT_MAX: usize = 4_096;
/// Aggregate keyring-coordinate bytes materialized for one stdio request target. Server config
/// bytes are bounded independently by `mcp_store::MCP_SERVER_POINT_BYTES_MAX`.
pub const MCP_REQUEST_TARGET_CREDENTIAL_BYTES_MAX: usize = 1024 * 1024;
pub const PHYSICAL_SECRET_SLOT_RECONCILIATION_LIMIT_MAX: usize = 4_096;
pub const PHYSICAL_SECRET_SLOT_RECONCILIATION_BYTES_MAX: usize = 1024 * 1024;

/// Storage-neutral, same-snapshot preparation record for one MCP request target. The vectors are
/// deliberately cross-shaped: stdio records contain credential locations in `env_secrets` order,
/// while HTTP records contain zero to two OAuth candidates. Missing servers contain neither.
#[derive(Clone, PartialEq)]
pub struct McpRequestTargetRecord {
    pub server: Option<mcp_store::McpServerRow>,
    pub credential_locations: Vec<CredentialSecretLocation>,
    pub oauth_bindings: Vec<CredentialOAuthBindingRecord>,
}

impl std::fmt::Debug for McpRequestTargetRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let transport = match self.server.as_ref().map(|server| server.kind.as_str()) {
            None => "absent",
            Some("stdio") => "stdio",
            Some("http") => "http",
            Some(_) => "invalid",
        };
        formatter
            .debug_struct("McpRequestTargetRecord")
            .field("server_present", &self.server.is_some())
            .field("transport", &transport)
            .field(
                "credential_location_count",
                &self.credential_locations.len(),
            )
            .field("oauth_binding_count", &self.oauth_bindings.len())
            .finish()
    }
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

/// Stable filesystem identity for a workspace folder. The pair is deliberately all-or-nothing:
/// a partial `(dev, ino)` value cannot prove that a moved path still names the same directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceFolderAnchor {
    pub dev: i64,
    pub ino: i64,
}

/// Complete, bounded workspace projection for settings/sidebar snapshots. Unlike the legacy
/// `WorkspaceRow`, this includes the folder identity needed to detect moved paths without a
/// per-workspace follow-up query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsWorkspaceProjectionRow {
    pub id: String,
    pub name: String,
    pub path: String,
    pub created_at: String,
    pub folder_anchor: Option<WorkspaceFolderAnchor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceFindOrCreateResult {
    pub row: SettingsWorkspaceProjectionRow,
    pub created: bool,
}

/// Result of an atomic moved-folder compare-and-swap. `Stale` means either the persisted
/// path/anchor no longer matches the caller's snapshot or the new filesystem identity is not the
/// same folder. In every stale case storage performs no mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceMovedPathUpdate {
    Updated,
    Stale,
}

/// 권한 규칙/승인 IPC 타입은 mcp-store 소유(v2.8) — 기존 storage:: 경로 호환을 위해 재수출.
pub use mcp_store::{ApprovalOutcome, ApprovalStatus, PendingApprovalRow, PermissionRuleRow};

/// 웹푸시(VAPID) 구독 한 행 (v21). web-remote push.rs가 발송 대상으로 읽는다. 세 값 모두
/// 브라우저 pushManager가 건넨 이 기기 전용 공개 파라미터라 비밀이 아니다(원문 secret 없음).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebPushSubscriptionRow {
    /// 푸시 서비스가 준 고유 endpoint URL(PK) — 발송 POST 대상.
    pub endpoint: String,
    /// base64url p256dh(수신자 공개키) — RFC 8291 암호화 입력.
    pub p256dh: String,
    /// base64url auth secret(16바이트) — RFC 8291 암호화 입력.
    pub auth: String,
}

pub const RELAY_DEVICE_ROWS_MAX: usize = 64;
pub const RELAY_PENDING_DEVICE_ROWS_MAX: usize = 256;
pub const RELAY_DISPLAY_NAME_BYTES_MAX: usize = 128;
/// 페어링 의식 창의 상한(초). `web-remote`의 `PAIRING_TTL_SECS`와 같은 5분이며,
/// 스키마 CHECK에도 같은 값이 박혀 있다.
pub const RELAY_PAIRING_WINDOW_SECS_MAX: i64 = 300;
const RELAY_ID_BYTES: usize = 16;
const RELAY_PUBLIC_KEY_BYTES: usize = 65;

#[derive(Clone, PartialEq, Eq)]
pub struct RelayPendingDeviceRow {
    pub pairing_id: [u8; RELAY_ID_BYTES],
    pub device_id: [u8; RELAY_ID_BYTES],
    pub identity_public_sec1: [u8; RELAY_PUBLIC_KEY_BYTES],
    pub display_name: String,
    pub permission_view: bool,
    pub permission_input: bool,
    pub permission_upload: bool,
    pub permission_approval: bool,
    pub issued_at: i64,
    /// 5분 페어링 마감 — 이 시각부터 승인은 실패한다.
    pub pairing_expires_at: i64,
    /// 승인 후 발급할 기기 인가 만료 — 페어링 마감보다 길 수 있다.
    pub device_expires_at: i64,
}

impl std::fmt::Debug for RelayPendingDeviceRow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RelayPendingDeviceRow")
            .field("state", &"public-metadata")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct RelayDeviceRow {
    pub device_id: [u8; RELAY_ID_BYTES],
    pub identity_public_sec1: [u8; RELAY_PUBLIC_KEY_BYTES],
    pub display_name: String,
    pub permission_view: bool,
    pub permission_input: bool,
    pub permission_upload: bool,
    pub permission_approval: bool,
    pub issued_at: i64,
    pub device_expires_at: i64,
    pub last_seen_at: Option<i64>,
    pub revoked_at: Option<i64>,
    pub authorization_epoch: [u8; 16],
}

impl std::fmt::Debug for RelayDeviceRow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RelayDeviceRow")
            .field("state", &"public-metadata")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayPendingInsert {
    Stored,
    LimitReached,
    /// 다른 주체의 미완 의식과 id 또는 키가 겹친다. 같은 기기의 재시도는 대체되므로
    /// 여기 오지 않는다.
    Conflict,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RelayDeviceApproval {
    Approved(RelayDeviceRow),
    NotFound,
    Expired,
    DeviceLimitReached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayDeviceRevocation {
    Revoked,
    NotFound,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvApiProjectCount {
    pub workspace_id: String,
    pub env_count: usize,
    pub key_count: usize,
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

/// Storage-owned hard ceilings for the settings snapshot/launch read path. Callers may project
/// these rows into smaller UI snapshots, but cannot accidentally request an unbounded inventory.
pub const SETTINGS_AGENT_LIMIT_MAX: usize = 1_024;
pub const SETTINGS_ENV_PROFILE_LIMIT_MAX: usize = 256;
pub const SETTINGS_ENV_VAR_LIMIT_MAX: usize = 4_096;
pub const SETTINGS_CREDENTIAL_LIMIT_MAX: usize = 4_096;

/// 워크스페이스 메모 본문 상한(바이트). 설정 스냅샷 경로가 bounded read를 전제로 하므로
/// 무제한 본문을 허용하지 않는다. 상한을 넘으면 **자르지 않고 거부**한다 — 조용히 자르면
/// 사용자가 모르는 사이 글이 사라진다. 64 KiB는 사람이 손으로 쓰는 메모에는 충분하고
/// (A4 약 30장), 붙여넣기 사고로 로그를 통째로 넣는 경우는 막는 크기다.
pub const WORKSPACE_NOTE_MAX_BYTES: usize = 64 * 1024;
pub const SETTINGS_ENABLED_MCP_SERVER_LIMIT_MAX: usize = 256;
pub const SETTINGS_WORKSPACE_LIMIT_MAX: usize = SETTINGS_ENV_PROFILE_LIMIT_MAX;
pub const SETTINGS_SNAPSHOT_BYTES_MAX: usize = 4 * 1024 * 1024;
pub const SETTINGS_ROW_BYTES_MAX: usize = 1024 * 1024;
pub const SETTINGS_AGENT_ARGS_LIMIT_MAX: usize = 256;
pub const SETTINGS_AGENT_ARGS_BYTES_MAX: usize = 64 * 1024;

/// Lightweight MCP backend row for the Agents settings snapshot. Execution configuration and
/// transport secrets are deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsMcpServerRow {
    pub id: String,
    pub name: String,
}

/// One workspace-owned env row. Including the profile id lets the app build both dotenv and
/// legacy projections without issuing an N+1 query per profile.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsWorkspaceEnvVarRow {
    pub profile_id: String,
    pub key: String,
    pub value: EnvValue,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SettingsAgentsSnapshotRows {
    pub agents: Vec<AgentConfigRow>,
    pub profiles: Vec<EnvProfileRow>,
    pub enabled_mcp_servers: Vec<SettingsMcpServerRow>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SettingsEnvironmentSnapshotRows {
    pub credential_env: Vec<CredentialEnvBinding>,
    pub env_source_files: Vec<String>,
    pub credentials: Vec<CredentialMeta>,
    pub profiles: Vec<EnvProfileRow>,
    pub env_vars: Vec<SettingsWorkspaceEnvVarRow>,
}

/// Same-snapshot inputs needed to launch one saved agent. `profile` remains `None` when the
/// requested profile is absent or belongs to another workspace; callers must fail closed.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsAgentLaunchRows {
    pub agent: Option<AgentConfigRow>,
    pub profile: Option<EnvProfileRow>,
    pub env_vars: Vec<EnvVarRow>,
    pub mcp_backend_enabled: bool,
}

#[derive(Debug, Clone, Copy)]
struct SettingsReadProbe {
    count: usize,
    retained_bytes: usize,
}

impl SettingsReadProbe {
    const EMPTY: Self = Self {
        count: 0,
        retained_bytes: 0,
    };
}

#[derive(Clone, Copy)]
enum SettingsWriteInventory {
    Workspace,
    Credential,
    EnvProfile,
    EnvVar,
    Agent,
}

impl SettingsWriteInventory {
    fn probe_code(self) -> &'static str {
        match self {
            Self::Workspace => "settings_workspace_write",
            Self::Credential => "settings_credential_write",
            Self::EnvProfile => "settings_env_profile_write",
            Self::EnvVar => "settings_env_var_write",
            Self::Agent => "settings_agent_write",
        }
    }

    fn item_limit_error(self) -> &'static str {
        match self {
            Self::Workspace => "settings_workspace_write_item_limit",
            Self::Credential => "settings_credential_write_item_limit",
            Self::EnvProfile => "settings_env_profile_write_item_limit",
            Self::EnvVar => "settings_env_var_write_item_limit",
            Self::Agent => "settings_agent_write_item_limit",
        }
    }

    fn row_bytes_error(self) -> &'static str {
        match self {
            Self::Workspace => "settings_workspace_write_row_bytes_limit",
            Self::Credential => "settings_credential_write_row_bytes_limit",
            Self::EnvProfile => "settings_env_profile_write_row_bytes_limit",
            Self::EnvVar => "settings_env_var_write_row_bytes_limit",
            Self::Agent => "settings_agent_write_row_bytes_limit",
        }
    }

    fn retained_bytes_error(self) -> &'static str {
        match self {
            Self::Workspace => "settings_workspace_write_retained_bytes_limit",
            Self::Credential => "settings_credential_write_retained_bytes_limit",
            Self::EnvProfile => "settings_env_profile_write_retained_bytes_limit",
            Self::EnvVar => "settings_env_var_write_retained_bytes_limit",
            Self::Agent => "settings_agent_write_retained_bytes_limit",
        }
    }
}

fn settings_static_error(code: &'static str) -> anyhow::Error {
    anyhow::anyhow!(code)
}

fn settings_checked_row_bytes(parts: &[&str]) -> anyhow::Result<usize> {
    parts.iter().try_fold(0usize, |total, part| {
        total
            .checked_add(part.len())
            .ok_or_else(|| settings_static_error("settings_write_row_bytes_overflow"))
    })
}

fn settings_project_candidate(
    existing: SettingsReadProbe,
    candidate_bytes: usize,
    item_limit: usize,
    inventory: SettingsWriteInventory,
) -> anyhow::Result<SettingsReadProbe> {
    if candidate_bytes > SETTINGS_ROW_BYTES_MAX {
        return Err(settings_static_error(inventory.row_bytes_error()));
    }
    let count = existing
        .count
        .checked_add(1)
        .ok_or_else(|| settings_static_error(inventory.item_limit_error()))?;
    if count > item_limit {
        return Err(settings_static_error(inventory.item_limit_error()));
    }
    let retained_bytes = existing
        .retained_bytes
        .checked_add(candidate_bytes)
        .ok_or_else(|| settings_static_error(inventory.retained_bytes_error()))?;
    if retained_bytes > SETTINGS_SNAPSHOT_BYTES_MAX {
        return Err(settings_static_error(inventory.retained_bytes_error()));
    }
    Ok(SettingsReadProbe {
        count,
        retained_bytes,
    })
}

fn settings_validate_combined_retained_bytes(
    probes: &[SettingsReadProbe],
    overflow_error: &'static str,
    limit_error: &'static str,
) -> anyhow::Result<()> {
    let retained_bytes = probes.iter().try_fold(0usize, |total, probe| {
        total
            .checked_add(probe.retained_bytes)
            .ok_or_else(|| settings_static_error(overflow_error))
    })?;
    if retained_bytes > SETTINGS_SNAPSHOT_BYTES_MAX {
        return Err(settings_static_error(limit_error));
    }
    Ok(())
}

fn settings_sql_probe_limit(limit: usize, error_code: &str) -> anyhow::Result<i64> {
    let probe = limit
        .checked_add(1)
        .with_context(|| format!("{error_code}_limit_overflow"))?;
    i64::try_from(probe).with_context(|| format!("{error_code}_limit_conversion"))
}

fn settings_read_probe<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    item_limit: usize,
    retained_bytes_limit: usize,
    row_bytes_limit: usize,
    error_code: &str,
) -> anyhow::Result<SettingsReadProbe> {
    let (count, retained_bytes, max_row_bytes): (i64, i64, i64) =
        conn.query_row(sql, params, |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    let count = usize::try_from(count).with_context(|| format!("{error_code}_count_invalid"))?;
    let retained_bytes =
        usize::try_from(retained_bytes).with_context(|| format!("{error_code}_bytes_invalid"))?;
    let max_row_bytes = usize::try_from(max_row_bytes)
        .with_context(|| format!("{error_code}_row_bytes_invalid"))?;
    anyhow::ensure!(count <= item_limit, "{error_code}_item_limit");
    anyhow::ensure!(
        max_row_bytes <= row_bytes_limit,
        "{error_code}_row_bytes_limit"
    );
    anyhow::ensure!(
        retained_bytes <= retained_bytes_limit,
        "{error_code}_retained_bytes_limit"
    );
    Ok(SettingsReadProbe {
        count,
        retained_bytes,
    })
}

fn settings_candidate_probe_limit(limit: usize) -> anyhow::Result<i64> {
    i64::try_from(limit).map_err(|_| settings_static_error("settings_write_probe_limit_conversion"))
}

struct SettingsWorkspaceWriteCandidate<'a> {
    id: &'a str,
    name: &'a str,
    path: &'a str,
    created_at: Option<&'a str>,
    path_dev: Option<i64>,
    path_ino: Option<i64>,
}

fn settings_workspace_candidate_bytes(
    conn: &Connection,
    candidate: &SettingsWorkspaceWriteCandidate<'_>,
) -> anyhow::Result<usize> {
    let bytes: i64 = conn.query_row(
        "SELECT length(CAST(?1 AS BLOB)) + length(CAST(?2 AS BLOB)) +
                length(CAST(?3 AS BLOB)) +
                length(CAST(COALESCE(?4,
                    strftime('%Y-%m-%dT%H:%M:%fZ','now')) AS BLOB)) +
                length(CAST(COALESCE(?5, '') AS BLOB)) +
                length(CAST(COALESCE(?6, '') AS BLOB))",
        rusqlite::params![
            candidate.id,
            candidate.name,
            candidate.path,
            candidate.created_at,
            candidate.path_dev,
            candidate.path_ino
        ],
        |row| row.get(0),
    )?;
    usize::try_from(bytes)
        .map_err(|_| settings_static_error("settings_workspace_write_bytes_invalid"))
}

fn settings_workspace_existing_probe(
    conn: &Connection,
    excluded_id: &str,
) -> anyhow::Result<SettingsReadProbe> {
    let sql_limit = settings_candidate_probe_limit(SETTINGS_WORKSPACE_LIMIT_MAX)?;
    settings_read_probe(
        conn,
        "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
         FROM (
             SELECT length(CAST(id AS BLOB)) + length(CAST(name AS BLOB)) +
                    length(CAST(path AS BLOB)) + length(CAST(created_at AS BLOB)) +
                    length(CAST(COALESCE(path_dev, '') AS BLOB)) +
                    length(CAST(COALESCE(path_ino, '') AS BLOB)) AS row_bytes
             FROM workspaces WHERE id != ?1 ORDER BY created_at, id LIMIT ?2
         )",
        rusqlite::params![excluded_id, sql_limit],
        SETTINGS_WORKSPACE_LIMIT_MAX,
        SETTINGS_SNAPSHOT_BYTES_MAX,
        SETTINGS_ROW_BYTES_MAX,
        SettingsWriteInventory::Workspace.probe_code(),
    )
}

fn settings_workspace_row_write_admission(
    conn: &Connection,
    candidate: &SettingsWorkspaceWriteCandidate<'_>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        candidate.path_dev.is_some() == candidate.path_ino.is_some(),
        "settings_workspace_write_anchor_partial"
    );
    let existing = settings_workspace_existing_probe(conn, candidate.id)?;
    let candidate_bytes = settings_workspace_candidate_bytes(conn, candidate)?;
    settings_project_candidate(
        existing,
        candidate_bytes,
        SETTINGS_WORKSPACE_LIMIT_MAX,
        SettingsWriteInventory::Workspace,
    )?;
    Ok(())
}

fn settings_workspace_write_admission(
    conn: &Connection,
    id: &str,
    name: &str,
    path: &str,
    path_dev: Option<i64>,
    path_ino: Option<i64>,
) -> anyhow::Result<()> {
    settings_workspace_row_write_admission(
        conn,
        &SettingsWorkspaceWriteCandidate {
            id,
            name,
            path,
            created_at: None,
            path_dev,
            path_ino,
        },
    )?;
    settings_validate_new_workspace_views(conn, id)
}

struct SettingsWorkspaceStoredRow {
    id: String,
    name: String,
    path: String,
    created_at: String,
    path_dev: Option<i64>,
    path_ino: Option<i64>,
}

impl SettingsWorkspaceStoredRow {
    fn candidate(&self) -> SettingsWorkspaceWriteCandidate<'_> {
        SettingsWorkspaceWriteCandidate {
            id: &self.id,
            name: &self.name,
            path: &self.path,
            created_at: Some(&self.created_at),
            path_dev: self.path_dev,
            path_ino: self.path_ino,
        }
    }
}

fn settings_workspace_row_for_update(
    conn: &Connection,
    workspace_id: &str,
) -> anyhow::Result<Option<SettingsWorkspaceStoredRow>> {
    let probe = settings_read_probe(
        conn,
        "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
         FROM (
             SELECT length(CAST(id AS BLOB)) + length(CAST(name AS BLOB)) +
                    length(CAST(path AS BLOB)) + length(CAST(created_at AS BLOB)) +
                    length(CAST(COALESCE(path_dev, '') AS BLOB)) +
                    length(CAST(COALESCE(path_ino, '') AS BLOB)) AS row_bytes
             FROM workspaces WHERE id = ?1 LIMIT 2
         )",
        [workspace_id],
        1,
        SETTINGS_SNAPSHOT_BYTES_MAX,
        SETTINGS_ROW_BYTES_MAX,
        "settings_workspace_update_target",
    )?;
    if probe.count == 0 {
        return Ok(None);
    }
    conn.query_row(
        "SELECT id, name, path, created_at, path_dev, path_ino
         FROM workspaces WHERE id = ?1",
        [workspace_id],
        |row| {
            Ok(SettingsWorkspaceStoredRow {
                id: row.get(0)?,
                name: row.get(1)?,
                path: row.get(2)?,
                created_at: row.get(3)?,
                path_dev: row.get(4)?,
                path_ino: row.get(5)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

fn settings_workspace_update_admission(
    conn: &Connection,
    row: &SettingsWorkspaceStoredRow,
) -> anyhow::Result<()> {
    settings_workspace_row_write_admission(conn, &row.candidate())
}

fn settings_workspace_scope_probe(conn: &Connection) -> anyhow::Result<SettingsReadProbe> {
    let sql_limit = settings_sql_probe_limit(
        SETTINGS_WORKSPACE_LIMIT_MAX,
        "settings_write_workspace_scope",
    )?;
    settings_read_probe(
        conn,
        "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
         FROM (
             SELECT length(CAST(id AS BLOB)) + length(CAST(name AS BLOB)) +
                    length(CAST(path AS BLOB)) + length(CAST(created_at AS BLOB)) +
                    length(CAST(COALESCE(path_dev, '') AS BLOB)) +
                    length(CAST(COALESCE(path_ino, '') AS BLOB)) AS row_bytes
             FROM workspaces ORDER BY created_at, id LIMIT ?1
         )",
        [sql_limit],
        SETTINGS_WORKSPACE_LIMIT_MAX,
        SETTINGS_SNAPSHOT_BYTES_MAX,
        SETTINGS_ROW_BYTES_MAX,
        "settings_write_workspace_scope",
    )
}

fn settings_all_profile_groups_probe(conn: &Connection) -> anyhow::Result<SettingsReadProbe> {
    settings_workspace_scope_probe(conn)?;
    let total_limit = SETTINGS_WORKSPACE_LIMIT_MAX
        .checked_mul(SETTINGS_ENV_PROFILE_LIMIT_MAX)
        .and_then(|limit| limit.checked_add(1))
        .ok_or_else(|| settings_static_error("settings_env_profile_write_limit_overflow"))?;
    let total_limit = i64::try_from(total_limit)
        .map_err(|_| settings_static_error("settings_env_profile_write_limit_conversion"))?;
    settings_read_probe(
        conn,
        "WITH profile_groups AS MATERIALIZED (
             SELECT workspace_id, COUNT(*) AS item_count,
                    COALESCE(SUM(row_bytes), 0) AS retained_bytes,
                    COALESCE(MAX(row_bytes), 0) AS max_row_bytes
             FROM (
                 SELECT workspace_id,
                        length(CAST(id AS BLOB)) + length(CAST(name AS BLOB)) +
                        length(CAST(kind AS BLOB)) AS row_bytes
                 FROM env_profiles
                 ORDER BY workspace_id, created_at, id LIMIT ?1
             ) AS bounded_profiles GROUP BY workspace_id
         )
         SELECT MAX(COALESCE(MAX(item_count), 0), COUNT(*)),
                COALESCE(MAX(retained_bytes), 0),
                COALESCE(MAX(max_row_bytes), 0)
         FROM profile_groups",
        [total_limit],
        SETTINGS_ENV_PROFILE_LIMIT_MAX,
        SETTINGS_SNAPSHOT_BYTES_MAX,
        SETTINGS_ROW_BYTES_MAX,
        SettingsWriteInventory::EnvProfile.probe_code(),
    )
}

fn settings_agent_existing_probe(
    conn: &Connection,
    excluded_id: Option<&str>,
) -> anyhow::Result<SettingsReadProbe> {
    let sql_limit = match excluded_id {
        Some(_) => settings_candidate_probe_limit(SETTINGS_AGENT_LIMIT_MAX)?,
        None => settings_sql_probe_limit(SETTINGS_AGENT_LIMIT_MAX, "settings_agent_write")?,
    };
    let probe = settings_read_probe(
        conn,
        "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
         FROM (
             SELECT length(CAST(id AS BLOB)) + length(CAST(name AS BLOB)) +
                    length(CAST(command AS BLOB)) + length(CAST(args_json AS BLOB)) +
                    length(CAST(COALESCE(waiting_regex, '') AS BLOB)) +
                    length(CAST(COALESCE(approval_regex, '') AS BLOB)) +
                    length(CAST(COALESCE(error_regex, '') AS BLOB)) +
                    length(CAST(COALESCE(done_regex, '') AS BLOB)) +
                    length(CAST(COALESCE(mcp_proxy_server_id, '') AS BLOB)) +
                    length(CAST(COALESCE(mcp_config_flag, '') AS BLOB)) AS row_bytes
             FROM agent_configs
             WHERE deleted_at IS NULL AND (?1 IS NULL OR id != ?1)
             ORDER BY created_at, id LIMIT ?2
         )",
        rusqlite::params![excluded_id, sql_limit],
        SETTINGS_AGENT_LIMIT_MAX,
        SETTINGS_SNAPSHOT_BYTES_MAX,
        SETTINGS_ROW_BYTES_MAX,
        SettingsWriteInventory::Agent.probe_code(),
    )?;
    Db::settings_agent_args_inventory_preflight(conn, sql_limit)?;
    Ok(probe)
}

fn settings_profile_existing_probe(
    conn: &Connection,
    workspace_id: &str,
    excluded_id: Option<&str>,
) -> anyhow::Result<SettingsReadProbe> {
    let sql_limit = match excluded_id {
        Some(_) => settings_candidate_probe_limit(SETTINGS_ENV_PROFILE_LIMIT_MAX)?,
        None => {
            settings_sql_probe_limit(SETTINGS_ENV_PROFILE_LIMIT_MAX, "settings_env_profile_write")?
        }
    };
    settings_read_probe(
        conn,
        "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
         FROM (
             SELECT length(CAST(id AS BLOB)) + length(CAST(name AS BLOB)) +
                    length(CAST(kind AS BLOB)) AS row_bytes
             FROM env_profiles
             WHERE workspace_id = ?1 AND (?2 IS NULL OR id != ?2)
             ORDER BY created_at, id LIMIT ?3
         )",
        rusqlite::params![workspace_id, excluded_id, sql_limit],
        SETTINGS_ENV_PROFILE_LIMIT_MAX,
        SETTINGS_SNAPSHOT_BYTES_MAX,
        SETTINGS_ROW_BYTES_MAX,
        SettingsWriteInventory::EnvProfile.probe_code(),
    )
}

fn settings_credential_existing_probe(
    conn: &Connection,
    workspace_id: &str,
    excluded_id: Option<&str>,
) -> anyhow::Result<SettingsReadProbe> {
    let sql_limit = match excluded_id {
        Some(_) => settings_candidate_probe_limit(SETTINGS_CREDENTIAL_LIMIT_MAX)?,
        None => {
            settings_sql_probe_limit(SETTINGS_CREDENTIAL_LIMIT_MAX, "settings_credential_write")?
        }
    };
    settings_read_probe(
        conn,
        "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
         FROM (
             SELECT length(CAST(id AS BLOB)) + length(CAST(provider AS BLOB)) +
                    length(CAST(label AS BLOB)) + length(CAST(credential_kind AS BLOB)) +
                    length(CAST(COALESCE(masked_hint, '') AS BLOB)) +
                    length(CAST(COALESCE(workspace_id, '') AS BLOB)) AS row_bytes
             FROM credentials
             WHERE (workspace_id IS NULL OR workspace_id = ?1)
               AND (?2 IS NULL OR id != ?2)
             ORDER BY created_at, id LIMIT ?3
         )",
        rusqlite::params![workspace_id, excluded_id, sql_limit],
        SETTINGS_CREDENTIAL_LIMIT_MAX,
        SETTINGS_SNAPSHOT_BYTES_MAX,
        SETTINGS_ROW_BYTES_MAX,
        SettingsWriteInventory::Credential.probe_code(),
    )
}

fn settings_env_var_existing_probe(
    conn: &Connection,
    workspace_id: &str,
    excluded: Option<(&str, &str)>,
) -> anyhow::Result<SettingsReadProbe> {
    let sql_limit = match excluded {
        Some(_) => settings_candidate_probe_limit(SETTINGS_ENV_VAR_LIMIT_MAX)?,
        None => settings_sql_probe_limit(SETTINGS_ENV_VAR_LIMIT_MAX, "settings_env_var_write")?,
    };
    let (excluded_profile, excluded_key) = excluded
        .map(|(profile, key)| (Some(profile), Some(key)))
        .unwrap_or((None, None));
    settings_read_probe(
        conn,
        "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
         FROM (
             SELECT length(CAST(p.id AS BLOB)) + length(CAST(v.key AS BLOB)) +
                    length(CAST(v.kind AS BLOB)) +
                    length(CAST(COALESCE(v.plain_value, '') AS BLOB)) +
                    length(CAST(COALESCE(v.credential_id, '') AS BLOB)) AS row_bytes
             FROM env_vars v JOIN env_profiles p ON p.id = v.profile_id
             WHERE p.workspace_id = ?1
               AND (?2 IS NULL OR v.profile_id != ?2 OR v.key != ?3)
             ORDER BY p.created_at, p.id, v.key LIMIT ?4
         )",
        rusqlite::params![workspace_id, excluded_profile, excluded_key, sql_limit],
        SETTINGS_ENV_VAR_LIMIT_MAX,
        SETTINGS_SNAPSHOT_BYTES_MAX,
        SETTINGS_ROW_BYTES_MAX,
        SettingsWriteInventory::EnvVar.probe_code(),
    )
}

fn settings_backend_existing_probe(conn: &Connection) -> anyhow::Result<SettingsReadProbe> {
    let sql_limit = settings_sql_probe_limit(
        SETTINGS_ENABLED_MCP_SERVER_LIMIT_MAX,
        "settings_agent_backend_write_dependency",
    )?;
    settings_read_probe(
        conn,
        "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
         FROM (
             SELECT length(CAST(id AS BLOB)) + length(CAST(name AS BLOB)) AS row_bytes
             FROM mcp_servers WHERE enabled != 0 ORDER BY created_at, id LIMIT ?1
         )",
        [sql_limit],
        SETTINGS_ENABLED_MCP_SERVER_LIMIT_MAX,
        SETTINGS_SNAPSHOT_BYTES_MAX,
        SETTINGS_ROW_BYTES_MAX,
        "settings_agent_backend_write_dependency",
    )
}

fn settings_validate_agent_snapshot_bytes(
    agent_probe: SettingsReadProbe,
    profile_probe: SettingsReadProbe,
    backend_probe: SettingsReadProbe,
) -> anyhow::Result<()> {
    settings_validate_combined_retained_bytes(
        &[agent_probe, profile_probe, backend_probe],
        "settings_agents_write_bytes_overflow",
        "settings_agents_write_retained_bytes_limit",
    )
}

fn settings_validate_environment_snapshot_bytes(
    credential_probe: SettingsReadProbe,
    profile_probe: SettingsReadProbe,
    env_var_probe: SettingsReadProbe,
) -> anyhow::Result<()> {
    settings_validate_combined_retained_bytes(
        &[credential_probe, profile_probe, env_var_probe],
        "settings_environment_write_bytes_overflow",
        "settings_environment_write_retained_bytes_limit",
    )
}

fn settings_validate_new_workspace_views(
    conn: &Connection,
    workspace_id: &str,
) -> anyhow::Result<()> {
    let agents = settings_agent_existing_probe(conn, None)?;
    let backends = settings_backend_existing_probe(conn)?;
    settings_validate_agent_snapshot_bytes(agents, SettingsReadProbe::EMPTY, backends)?;
    let credentials = settings_credential_existing_probe(conn, workspace_id, None)?;
    settings_validate_environment_snapshot_bytes(
        credentials,
        SettingsReadProbe::EMPTY,
        SettingsReadProbe::EMPTY,
    )
}

struct SettingsAgentWriteCandidate<'a> {
    id: &'a str,
    name: &'a str,
    command: &'a str,
    args_json: &'a str,
    waiting_regex: Option<&'a str>,
    approval_regex: Option<&'a str>,
    error_regex: Option<&'a str>,
    done_regex: Option<&'a str>,
    mcp_proxy_server_id: Option<&'a str>,
    mcp_config_flag: Option<&'a str>,
}

impl SettingsAgentWriteCandidate<'_> {
    fn retained_bytes(&self) -> anyhow::Result<usize> {
        settings_checked_row_bytes(&[
            self.id,
            self.name,
            self.command,
            self.args_json,
            self.waiting_regex.unwrap_or_default(),
            self.approval_regex.unwrap_or_default(),
            self.error_regex.unwrap_or_default(),
            self.done_regex.unwrap_or_default(),
            self.mcp_proxy_server_id.unwrap_or_default(),
            self.mcp_config_flag.unwrap_or_default(),
        ])
    }
}

fn settings_agent_write_admission(
    conn: &Connection,
    candidate: &SettingsAgentWriteCandidate<'_>,
) -> anyhow::Result<()> {
    let agents = settings_project_candidate(
        settings_agent_existing_probe(conn, Some(candidate.id))?,
        candidate.retained_bytes()?,
        SETTINGS_AGENT_LIMIT_MAX,
        SettingsWriteInventory::Agent,
    )?;
    let backends = settings_backend_existing_probe(conn)?;
    let profiles = settings_all_profile_groups_probe(conn)?;
    settings_validate_agent_snapshot_bytes(agents, profiles, backends)
}

const SETTINGS_GLOBAL_CREDENTIAL_WRITE_PREFLIGHT: &str = "WITH scopes AS MATERIALIZED (
         SELECT id FROM workspaces ORDER BY created_at, id LIMIT ?3
     ), effective_scopes AS MATERIALIZED (
         SELECT id FROM scopes
         UNION ALL SELECT '' WHERE NOT EXISTS (SELECT 1 FROM scopes)
     ), credential_usage AS MATERIALIZED (
         SELECT workspace_id, COUNT(*) AS item_count,
                COALESCE(SUM(row_bytes), 0) AS retained_bytes,
                COALESCE(MAX(row_bytes), 0) AS max_row_bytes
         FROM (
             SELECT workspace_id,
                    length(CAST(id AS BLOB)) + length(CAST(provider AS BLOB)) +
                    length(CAST(label AS BLOB)) + length(CAST(credential_kind AS BLOB)) +
                    length(CAST(COALESCE(masked_hint, '') AS BLOB)) +
                    length(CAST(COALESCE(workspace_id, '') AS BLOB)) AS row_bytes
             FROM credentials WHERE id != ?1 LIMIT ?4
         ) AS bounded_credentials GROUP BY workspace_id
     ), credential_global AS (
         SELECT COALESCE(SUM(item_count), 0) AS item_count,
                COALESCE(SUM(retained_bytes), 0) AS retained_bytes,
                COALESCE(MAX(max_row_bytes), 0) AS max_row_bytes
         FROM credential_usage WHERE workspace_id IS NULL
     ), profile_usage AS MATERIALIZED (
         SELECT workspace_id, COUNT(*) AS item_count,
                COALESCE(SUM(row_bytes), 0) AS retained_bytes,
                COALESCE(MAX(row_bytes), 0) AS max_row_bytes
         FROM (
             SELECT workspace_id,
                    length(CAST(id AS BLOB)) + length(CAST(name AS BLOB)) +
                    length(CAST(kind AS BLOB)) AS row_bytes
             FROM env_profiles LIMIT ?5
         ) AS bounded_profiles GROUP BY workspace_id
     ), env_var_usage AS MATERIALIZED (
         SELECT workspace_id, COUNT(*) AS item_count,
                COALESCE(SUM(row_bytes), 0) AS retained_bytes,
                COALESCE(MAX(row_bytes), 0) AS max_row_bytes
         FROM (
             SELECT p.workspace_id,
                    length(CAST(p.id AS BLOB)) + length(CAST(v.key AS BLOB)) +
                    length(CAST(v.kind AS BLOB)) +
                    length(CAST(COALESCE(v.plain_value, '') AS BLOB)) +
                    length(CAST(COALESCE(v.credential_id, '') AS BLOB)) AS row_bytes
             FROM env_vars v JOIN env_profiles p ON p.id = v.profile_id LIMIT ?6
         ) AS bounded_env_vars GROUP BY workspace_id
     ), inventory_meta AS (
         SELECT (SELECT COALESCE(SUM(item_count), 0) FROM credential_usage)
                    AS credential_total,
                (SELECT COALESCE(SUM(item_count), 0) FROM profile_usage) AS profile_total,
                (SELECT COALESCE(SUM(item_count), 0) FROM env_var_usage) AS env_var_total,
                (SELECT COALESCE(SUM(c.item_count), 0) FROM credential_usage c
                   LEFT JOIN scopes s ON s.id = c.workspace_id
                  WHERE c.workspace_id IS NOT NULL AND s.id IS NULL) AS credential_orphans,
                (SELECT COALESCE(SUM(p.item_count), 0) FROM profile_usage p
                   LEFT JOIN scopes s ON s.id = p.workspace_id
                  WHERE s.id IS NULL) AS profile_orphans
     )
     SELECT g.item_count + COALESCE(c.item_count, 0) + 1,
            g.retained_bytes + COALESCE(c.retained_bytes, 0) + ?2,
            MAX(g.max_row_bytes, COALESCE(c.max_row_bytes, 0), ?2),
            COALESCE(p.item_count, 0), COALESCE(p.retained_bytes, 0),
            COALESCE(p.max_row_bytes, 0),
            COALESCE(v.item_count, 0), COALESCE(v.retained_bytes, 0),
            COALESCE(v.max_row_bytes, 0),
            g.retained_bytes + COALESCE(c.retained_bytes, 0) + ?2 +
                COALESCE(p.retained_bytes, 0) + COALESCE(v.retained_bytes, 0),
            m.credential_total, m.profile_total, m.env_var_total,
            m.credential_orphans, m.profile_orphans
     FROM effective_scopes s CROSS JOIN credential_global g
     LEFT JOIN credential_usage c ON c.workspace_id = s.id
     LEFT JOIN profile_usage p ON p.workspace_id = s.id
     LEFT JOIN env_var_usage v ON v.workspace_id = s.id
     CROSS JOIN inventory_meta m";

struct SettingsEnvironmentScopeProbe {
    credentials: SettingsReadProbe,
    profiles: SettingsReadProbe,
    env_vars: SettingsReadProbe,
    combined_retained_bytes: usize,
    credential_total: usize,
    profile_total: usize,
    env_var_total: usize,
    orphan_count: usize,
}

struct SettingsEnvironmentScopeProbeRaw {
    credential_count: i64,
    credential_bytes: i64,
    credential_max_row_bytes: i64,
    profile_count: i64,
    profile_bytes: i64,
    profile_max_row_bytes: i64,
    env_var_count: i64,
    env_var_bytes: i64,
    env_var_max_row_bytes: i64,
    combined_retained_bytes: i64,
    credential_total: i64,
    profile_total: i64,
    env_var_total: i64,
    credential_orphans: i64,
    profile_orphans: i64,
}

fn settings_write_probe_from_sql(
    count: i64,
    retained_bytes: i64,
    max_row_bytes: i64,
    item_limit: usize,
    inventory: SettingsWriteInventory,
) -> anyhow::Result<SettingsReadProbe> {
    let count =
        usize::try_from(count).map_err(|_| settings_static_error(inventory.item_limit_error()))?;
    let retained_bytes = usize::try_from(retained_bytes)
        .map_err(|_| settings_static_error(inventory.retained_bytes_error()))?;
    let max_row_bytes = usize::try_from(max_row_bytes)
        .map_err(|_| settings_static_error(inventory.row_bytes_error()))?;
    if count > item_limit {
        return Err(settings_static_error(inventory.item_limit_error()));
    }
    if max_row_bytes > SETTINGS_ROW_BYTES_MAX {
        return Err(settings_static_error(inventory.row_bytes_error()));
    }
    if retained_bytes > SETTINGS_SNAPSHOT_BYTES_MAX {
        return Err(settings_static_error(inventory.retained_bytes_error()));
    }
    Ok(SettingsReadProbe {
        count,
        retained_bytes,
    })
}

fn settings_total_inventory_probe_limit(
    per_workspace_limit: usize,
    error_code: &'static str,
) -> anyhow::Result<(usize, i64)> {
    let max = SETTINGS_WORKSPACE_LIMIT_MAX
        .checked_mul(per_workspace_limit)
        .ok_or_else(|| settings_static_error(error_code))?;
    let sql_limit = max
        .checked_add(1)
        .and_then(|limit| i64::try_from(limit).ok())
        .ok_or_else(|| settings_static_error(error_code))?;
    Ok((max, sql_limit))
}

fn settings_nonnegative_sql_usize(value: i64, error_code: &'static str) -> anyhow::Result<usize> {
    usize::try_from(value).map_err(|_| settings_static_error(error_code))
}

fn settings_global_credential_write_admission(
    conn: &Connection,
    meta: &CredentialMeta,
    candidate_bytes: usize,
) -> anyhow::Result<()> {
    if candidate_bytes > SETTINGS_ROW_BYTES_MAX {
        return Err(settings_static_error(
            SettingsWriteInventory::Credential.row_bytes_error(),
        ));
    }
    settings_workspace_scope_probe(conn)?;
    let workspace_sql_limit = settings_sql_probe_limit(
        SETTINGS_WORKSPACE_LIMIT_MAX,
        "settings_global_credential_write_scope",
    )?;
    let (credential_total_max, credential_sql_limit) = settings_total_inventory_probe_limit(
        SETTINGS_CREDENTIAL_LIMIT_MAX,
        "settings_credential_write_limit_overflow",
    )?;
    let (profile_total_max, profile_sql_limit) = settings_total_inventory_probe_limit(
        SETTINGS_ENV_PROFILE_LIMIT_MAX,
        "settings_env_profile_write_limit_overflow",
    )?;
    let (env_var_total_max, env_var_sql_limit) = settings_total_inventory_probe_limit(
        SETTINGS_ENV_VAR_LIMIT_MAX,
        "settings_env_var_write_limit_overflow",
    )?;
    let candidate_bytes = i64::try_from(candidate_bytes)
        .map_err(|_| settings_static_error("settings_credential_write_bytes_invalid"))?;
    let mut stmt = conn.prepare(SETTINGS_GLOBAL_CREDENTIAL_WRITE_PREFLIGHT)?;
    let rows = stmt.query_map(
        rusqlite::params![
            &meta.id,
            candidate_bytes,
            workspace_sql_limit,
            credential_sql_limit,
            profile_sql_limit,
            env_var_sql_limit
        ],
        |row| {
            Ok(SettingsEnvironmentScopeProbeRaw {
                credential_count: row.get(0)?,
                credential_bytes: row.get(1)?,
                credential_max_row_bytes: row.get(2)?,
                profile_count: row.get(3)?,
                profile_bytes: row.get(4)?,
                profile_max_row_bytes: row.get(5)?,
                env_var_count: row.get(6)?,
                env_var_bytes: row.get(7)?,
                env_var_max_row_bytes: row.get(8)?,
                combined_retained_bytes: row.get(9)?,
                credential_total: row.get(10)?,
                profile_total: row.get(11)?,
                env_var_total: row.get(12)?,
                credential_orphans: row.get(13)?,
                profile_orphans: row.get(14)?,
            })
        },
    )?;
    let mut scope_count = 0usize;
    for row in rows {
        let raw = row?;
        let row = SettingsEnvironmentScopeProbe {
            credentials: settings_write_probe_from_sql(
                raw.credential_count,
                raw.credential_bytes,
                raw.credential_max_row_bytes,
                SETTINGS_CREDENTIAL_LIMIT_MAX,
                SettingsWriteInventory::Credential,
            )?,
            profiles: settings_write_probe_from_sql(
                raw.profile_count,
                raw.profile_bytes,
                raw.profile_max_row_bytes,
                SETTINGS_ENV_PROFILE_LIMIT_MAX,
                SettingsWriteInventory::EnvProfile,
            )?,
            env_vars: settings_write_probe_from_sql(
                raw.env_var_count,
                raw.env_var_bytes,
                raw.env_var_max_row_bytes,
                SETTINGS_ENV_VAR_LIMIT_MAX,
                SettingsWriteInventory::EnvVar,
            )?,
            combined_retained_bytes: settings_nonnegative_sql_usize(
                raw.combined_retained_bytes,
                "settings_environment_write_bytes_invalid",
            )?,
            credential_total: settings_nonnegative_sql_usize(
                raw.credential_total,
                "settings_credential_write_count_invalid",
            )?,
            profile_total: settings_nonnegative_sql_usize(
                raw.profile_total,
                "settings_env_profile_write_count_invalid",
            )?,
            env_var_total: settings_nonnegative_sql_usize(
                raw.env_var_total,
                "settings_env_var_write_count_invalid",
            )?,
            orphan_count: settings_nonnegative_sql_usize(
                raw.credential_orphans,
                "settings_environment_write_scope_invalid",
            )?
            .checked_add(settings_nonnegative_sql_usize(
                raw.profile_orphans,
                "settings_environment_write_scope_invalid",
            )?)
            .ok_or_else(|| settings_static_error("settings_environment_write_scope_invalid"))?,
        };
        scope_count = scope_count
            .checked_add(1)
            .ok_or_else(|| settings_static_error("settings_credential_write_scope_limit"))?;
        if scope_count > SETTINGS_WORKSPACE_LIMIT_MAX.max(1) {
            return Err(settings_static_error(
                "settings_credential_write_scope_limit",
            ));
        }
        if row.credential_total > credential_total_max {
            return Err(settings_static_error(
                SettingsWriteInventory::Credential.item_limit_error(),
            ));
        }
        if row.profile_total > profile_total_max {
            return Err(settings_static_error(
                SettingsWriteInventory::EnvProfile.item_limit_error(),
            ));
        }
        if row.env_var_total > env_var_total_max {
            return Err(settings_static_error(
                SettingsWriteInventory::EnvVar.item_limit_error(),
            ));
        }
        if row.orphan_count != 0 {
            return Err(settings_static_error(
                "settings_environment_write_scope_invalid",
            ));
        }
        settings_validate_environment_snapshot_bytes(row.credentials, row.profiles, row.env_vars)?;
        if row.combined_retained_bytes > SETTINGS_SNAPSHOT_BYTES_MAX {
            return Err(settings_static_error(
                "settings_environment_write_retained_bytes_limit",
            ));
        }
    }
    if scope_count == 0 {
        return Err(settings_static_error(
            "settings_credential_write_scope_missing",
        ));
    }
    Ok(())
}

fn settings_credential_write_admission(
    conn: &Connection,
    meta: &CredentialMeta,
) -> anyhow::Result<()> {
    let duplicate: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM credentials WHERE id = ?1)",
        [&meta.id],
        |row| row.get(0),
    )?;
    if duplicate {
        // Preserve the existing INSERT constraint error and avoid changing duplicate semantics.
        return Ok(());
    }
    settings_credential_candidate_write_admission(conn, meta)
}

fn settings_credential_candidate_write_admission(
    conn: &Connection,
    meta: &CredentialMeta,
) -> anyhow::Result<()> {
    let candidate_bytes = settings_checked_row_bytes(&[
        &meta.id,
        &meta.provider,
        &meta.label,
        &meta.credential_kind,
        meta.masked_hint.as_deref().unwrap_or_default(),
        meta.workspace_id.as_deref().unwrap_or_default(),
    ])?;
    let Some(workspace_id) = meta.workspace_id.as_deref() else {
        return settings_global_credential_write_admission(conn, meta, candidate_bytes);
    };
    let credentials = settings_project_candidate(
        settings_credential_existing_probe(conn, workspace_id, Some(&meta.id))?,
        candidate_bytes,
        SETTINGS_CREDENTIAL_LIMIT_MAX,
        SettingsWriteInventory::Credential,
    )?;
    let profiles = settings_profile_existing_probe(conn, workspace_id, None)?;
    let env_vars = settings_env_var_existing_probe(conn, workspace_id, None)?;
    settings_validate_environment_snapshot_bytes(credentials, profiles, env_vars)
}

fn settings_credential_for_publish(
    conn: &Connection,
    logical_id: &str,
    expected_previous_pointer: &str,
) -> anyhow::Result<Option<CredentialMeta>> {
    let probe = settings_read_probe(
        conn,
        "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
         FROM (
             SELECT length(CAST(id AS BLOB)) + length(CAST(provider AS BLOB)) +
                    length(CAST(label AS BLOB)) + length(CAST(credential_kind AS BLOB)) +
                    length(CAST(COALESCE(masked_hint, '') AS BLOB)) +
                    length(CAST(COALESCE(workspace_id, '') AS BLOB)) AS row_bytes
             FROM credentials WHERE id = ?1 AND keyring_username = ?2 LIMIT 2
         )",
        (logical_id, expected_previous_pointer),
        1,
        SETTINGS_SNAPSHOT_BYTES_MAX,
        SETTINGS_ROW_BYTES_MAX,
        "settings_credential_publish_target",
    )?;
    if probe.count == 0 {
        return Ok(None);
    }
    conn.query_row(
        "SELECT id, provider, label, credential_kind, masked_hint, workspace_id
         FROM credentials WHERE id = ?1 AND keyring_username = ?2",
        (logical_id, expected_previous_pointer),
        |row| {
            Ok(CredentialMeta {
                id: row.get(0)?,
                provider: row.get(1)?,
                label: row.get(2)?,
                credential_kind: row.get(3)?,
                masked_hint: row.get(4)?,
                workspace_id: row.get(5)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

fn settings_credential_publish_admission(
    conn: &Connection,
    meta: &mut CredentialMeta,
    masked_hint: Option<&str>,
) -> anyhow::Result<()> {
    let candidate_bytes = settings_checked_row_bytes(&[
        &meta.id,
        &meta.provider,
        &meta.label,
        &meta.credential_kind,
        masked_hint.unwrap_or_default(),
        meta.workspace_id.as_deref().unwrap_or_default(),
    ])?;
    if candidate_bytes > SETTINGS_ROW_BYTES_MAX {
        return Err(settings_static_error(
            SettingsWriteInventory::Credential.row_bytes_error(),
        ));
    }
    meta.masked_hint = masked_hint.map(str::to_owned);
    settings_credential_candidate_write_admission(conn, meta)
}

fn settings_env_profile_write_admission(
    conn: &Connection,
    id: &str,
    workspace_id: &str,
    name: &str,
    kind: &str,
) -> anyhow::Result<()> {
    let candidate_bytes = settings_checked_row_bytes(&[id, name, kind])?;
    let profiles = settings_project_candidate(
        settings_profile_existing_probe(conn, workspace_id, Some(id))?,
        candidate_bytes,
        SETTINGS_ENV_PROFILE_LIMIT_MAX,
        SettingsWriteInventory::EnvProfile,
    )?;
    let agents = settings_agent_existing_probe(conn, None)?;
    let backends = settings_backend_existing_probe(conn)?;
    settings_validate_agent_snapshot_bytes(agents, profiles, backends)?;
    let credentials = settings_credential_existing_probe(conn, workspace_id, None)?;
    let env_vars = settings_env_var_existing_probe(conn, workspace_id, None)?;
    settings_validate_environment_snapshot_bytes(credentials, profiles, env_vars)
}

fn settings_env_var_write_admission(
    conn: &Connection,
    profile_id: &str,
    key: &str,
    kind: &str,
    plain_value: Option<&str>,
    credential_id: Option<&str>,
) -> anyhow::Result<()> {
    let workspace_id = conn
        .query_row(
            "SELECT workspace_id FROM env_profiles WHERE id = ?1",
            [profile_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(workspace_id) = workspace_id else {
        // Preserve the existing foreign-key error for a missing profile.
        return Ok(());
    };
    let candidate_bytes = settings_checked_row_bytes(&[
        profile_id,
        key,
        kind,
        plain_value.unwrap_or_default(),
        credential_id.unwrap_or_default(),
    ])?;
    let env_vars = settings_project_candidate(
        settings_env_var_existing_probe(conn, &workspace_id, Some((profile_id, key)))?,
        candidate_bytes,
        SETTINGS_ENV_VAR_LIMIT_MAX,
        SettingsWriteInventory::EnvVar,
    )?;
    let credentials = settings_credential_existing_probe(conn, &workspace_id, None)?;
    let profiles = settings_profile_existing_probe(conn, &workspace_id, None)?;
    settings_validate_environment_snapshot_bytes(credentials, profiles, env_vars)
}

type PersistedWorkspaceProjection = (String, String, String, String, Option<i64>, Option<i64>);

fn settings_workspace_projection_from_persisted(
    persisted: PersistedWorkspaceProjection,
) -> anyhow::Result<SettingsWorkspaceProjectionRow> {
    let (id, name, path, created_at, path_dev, path_ino) = persisted;
    let folder_anchor = match (path_dev, path_ino) {
        (Some(dev), Some(ino)) => Some(WorkspaceFolderAnchor { dev, ino }),
        (None, None) => None,
        _ => anyhow::bail!("settings_workspace_projection_anchor_invalid"),
    };
    Ok(SettingsWorkspaceProjectionRow {
        id,
        name,
        path,
        created_at,
        folder_anchor,
    })
}

fn validate_workspace_text_input(value: &str, field: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        value.len() <= SETTINGS_ROW_BYTES_MAX,
        "settings_workspace_{field}_bytes_limit"
    );
    Ok(())
}

type PersistedAgentConfigRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    Option<String>,
    Option<String>,
);

fn settings_agent_config_from_persisted(
    row: PersistedAgentConfigRow,
) -> anyhow::Result<AgentConfigRow> {
    let (
        id,
        name,
        command,
        args_json,
        waiting_regex,
        approval_regex,
        error_regex,
        done_regex,
        mcp_proxy_enabled,
        mcp_proxy_server_id,
        mcp_config_flag,
    ) = row;
    let args: Vec<String> =
        serde_json::from_str(&args_json).context("settings_agent_args_json_invalid")?;
    anyhow::ensure!(
        args.len() <= SETTINGS_AGENT_ARGS_LIMIT_MAX,
        "settings_agent_args_item_limit"
    );
    let args_bytes = args
        .iter()
        .map(String::len)
        .try_fold(0usize, usize::checked_add)
        .context("settings_agent_args_bytes_overflow")?;
    anyhow::ensure!(
        args_bytes <= SETTINGS_AGENT_ARGS_BYTES_MAX,
        "settings_agent_args_bytes_limit"
    );
    validate_args_for_persistence(&args, "agent args")?;
    Ok(AgentConfigRow {
        id,
        name,
        command,
        args,
        waiting_regex: waiting_regex.filter(|value| !value.is_empty()),
        approval_regex: approval_regex.filter(|value| !value.is_empty()),
        error_regex: error_regex.filter(|value| !value.is_empty()),
        done_regex: done_regex.filter(|value| !value.is_empty()),
        mcp_proxy_enabled: mcp_proxy_enabled != 0,
        mcp_proxy_server_id: mcp_proxy_server_id.filter(|value| !value.is_empty()),
        mcp_config_flag: mcp_config_flag.filter(|value| !value.is_empty()),
    })
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
    // 빈 값은 자격증명이 아니다 — 지킬 내용이 없다. 키 이름만 보고 거부하면 `.env`의
    // 빈 플레이스홀더(`GITHUB_OAUTH_CLIENT_SECRET=`)를 저장할 방법이 사라진다
    // (2026-08-21 사용자 보고: 그 한 줄이 워크스페이스 전체를 막았다).
    if value.is_empty() {
        return None;
    }
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

fn validate_owned_physical_secret_slot(logical_id: &str, slot: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !logical_id.is_empty() && logical_id.len() <= 96,
        "logical credential id byte 길이가 유효하지 않습니다"
    );
    anyhow::ensure!(
        !slot.is_empty() && slot.len() <= 255,
        "physical secret slot byte 길이가 유효하지 않습니다"
    );
    anyhow::ensure!(
        !logical_id.contains('\0') && !slot.contains('\0'),
        "credential/physical slot identifier에 NUL을 허용하지 않습니다"
    );
    let logical_id = secret::LogicalCredentialId::new(logical_id.to_owned())
        .context("logical credential id 검증 실패")?;
    let slot = secret::PhysicalSecretSlot::parse(slot.to_owned())
        .context("physical secret slot 검증 실패")?;
    anyhow::ensure!(
        slot.belongs_to(&logical_id),
        "physical secret slot이 logical credential에 속하지 않습니다"
    );
    Ok(())
}

fn validate_legacy_cleanup_username(logical_id: &str, legacy_username: &str) -> anyhow::Result<()> {
    secret::LogicalCredentialId::new(logical_id.to_owned())
        .context("legacy cleanup logical credential id 검증 실패")?;
    anyhow::ensure!(
        !legacy_username.is_empty() && legacy_username.len() <= 255,
        "legacy cleanup username byte 길이가 유효하지 않습니다"
    );
    anyhow::ensure!(
        !legacy_username.contains('\0'),
        "legacy cleanup username에 NUL을 허용하지 않습니다"
    );
    anyhow::ensure!(
        legacy_username == logical_id,
        "legacy cleanup username은 logical credential id와 같아야 합니다"
    );
    Ok(())
}

fn validate_oauth_metadata_json(json: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        json.len() <= CREDENTIAL_OAUTH_BINDING_BYTES_MAX,
        "OAuth metadata JSON byte 상한을 초과했습니다"
    );
    let value: serde_json::Value =
        serde_json::from_str(json).context("OAuth metadata JSON 검증 실패")?;
    anyhow::ensure!(
        !oauth_metadata_contains_secret(&value),
        "OAuth metadata에 concrete secret/token 필드 또는 secret-like 값이 있습니다"
    );
    Ok(())
}

fn oauth_metadata_contains_secret(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => map.iter().any(|(key, value)| {
            matches!(
                normalize_identifier(key).as_str(),
                "ACCESS_TOKEN" | "REFRESH_TOKEN" | "CLIENT_SECRET" | "DCR_CLIENT_SECRET"
            ) || oauth_metadata_contains_secret(value)
        }),
        serde_json::Value::Array(values) => values.iter().any(oauth_metadata_contains_secret),
        serde_json::Value::String(value) => secret_like_value(value).is_some(),
        _ => false,
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn open_lock_file(path: &Path) -> anyhow::Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("authorization lock 열기 실패: {}", path.display()))
}

#[cfg(unix)]
fn physical_db_identity(path: &Path) -> anyhow::Result<String> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = fs::metadata(path)
        .with_context(|| format!("authorization DB metadata 조회 실패: {}", path.display()))?;
    Ok(format!("unix:{}:{}", metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn physical_db_identity(path: &Path) -> anyhow::Result<String> {
    use std::os::windows::fs::MetadataExt as _;

    let metadata = fs::metadata(path)
        .with_context(|| format!("authorization DB metadata 조회 실패: {}", path.display()))?;
    if let (Some(volume), Some(index)) = (metadata.volume_serial_number(), metadata.file_index()) {
        return Ok(format!("windows:{volume}:{index}"));
    }
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("authorization DB 경로 정규화 실패: {}", path.display()))?;
    Ok(format!("windows-path:{}", canonical.display()))
}

#[cfg(not(any(unix, windows)))]
fn physical_db_identity(path: &Path) -> anyhow::Result<String> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("authorization DB 경로 정규화 실패: {}", path.display()))?;
    Ok(format!("path:{}", canonical.display()))
}

fn authorization_lock_dir(db_identity: &str) -> PathBuf {
    let digest = Sha256::digest(db_identity.as_bytes());
    std::env::temp_dir()
        .join("deppy-authorization-locks")
        .join(hex_digest(&digest))
}

fn pending_approval_owner_lock_dir(db_identity: &str) -> PathBuf {
    let digest = Sha256::digest(db_identity.as_bytes());
    std::env::temp_dir()
        .join("deppy-pending-approval-owner-locks")
        .join(hex_digest(&digest))
}

fn open_pending_approval_owner_lock(path: &Path) -> anyhow::Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .context(PENDING_APPROVAL_OWNER_UNAVAILABLE)
}

fn acquire_pending_approval_owner_for_identity(
    db_identity: &str,
) -> anyhow::Result<ActivePendingApprovalOwner> {
    let lock_dir = pending_approval_owner_lock_dir(db_identity);
    fs::create_dir_all(&lock_dir).context(PENDING_APPROVAL_OWNER_UNAVAILABLE)?;
    let owner_lock = open_pending_approval_owner_lock(&lock_dir.join("owner.lock"))?;
    if fs2::FileExt::try_lock_exclusive(&owner_lock).is_err() {
        anyhow::bail!(PENDING_APPROVAL_OWNER_UNAVAILABLE);
    }
    Ok(ActivePendingApprovalOwner {
        _owner_lock: owner_lock,
        db_identity: db_identity.to_owned(),
    })
}

fn validate_relay_pending_device(row: &RelayPendingDeviceRow) -> anyhow::Result<()> {
    anyhow::ensure!(
        row.identity_public_sec1[0] == 0x04,
        "relay pending device public key invalid"
    );
    anyhow::ensure!(
        !row.display_name.trim().is_empty()
            && row.display_name.len() <= RELAY_DISPLAY_NAME_BYTES_MAX
            && !row.display_name.contains('\0'),
        "relay pending device display name invalid"
    );
    anyhow::ensure!(
        row.issued_at >= 0
            && row.pairing_expires_at > row.issued_at
            && row.pairing_expires_at - row.issued_at <= RELAY_PAIRING_WINDOW_SECS_MAX
            && row.device_expires_at >= row.pairing_expires_at,
        "relay pending device lifetime invalid"
    );
    Ok(())
}

fn relay_blob<const N: usize>(row: &rusqlite::Row<'_>, index: usize) -> anyhow::Result<[u8; N]> {
    let rusqlite::types::ValueRef::Blob(bytes) = row.get_ref(index)? else {
        anyhow::bail!("relay repository row invalid")
    };
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("relay repository row invalid"))
}

fn relay_text(row: &rusqlite::Row<'_>, index: usize) -> anyhow::Result<String> {
    let rusqlite::types::ValueRef::Text(bytes) = row.get_ref(index)? else {
        anyhow::bail!("relay repository row invalid")
    };
    anyhow::ensure!(
        !bytes.is_empty() && bytes.len() <= RELAY_DISPLAY_NAME_BYTES_MAX && !bytes.contains(&0),
        "relay repository row invalid"
    );
    Ok(std::str::from_utf8(bytes)
        .context("relay repository row invalid")?
        .to_owned())
}

fn relay_pending_from_row(row: &rusqlite::Row<'_>) -> anyhow::Result<RelayPendingDeviceRow> {
    let pending = RelayPendingDeviceRow {
        pairing_id: relay_blob(row, 0)?,
        device_id: relay_blob(row, 1)?,
        identity_public_sec1: relay_blob(row, 2)?,
        display_name: relay_text(row, 3)?,
        permission_view: row.get(4)?,
        permission_input: row.get(5)?,
        permission_upload: row.get(6)?,
        permission_approval: row.get(7)?,
        issued_at: row.get(8)?,
        pairing_expires_at: row.get(9)?,
        device_expires_at: row.get(10)?,
    };
    validate_relay_pending_device(&pending)?;
    Ok(pending)
}

fn relay_device_from_row(row: &rusqlite::Row<'_>) -> anyhow::Result<RelayDeviceRow> {
    let device = RelayDeviceRow {
        device_id: relay_blob(row, 0)?,
        identity_public_sec1: relay_blob(row, 1)?,
        display_name: relay_text(row, 2)?,
        permission_view: row.get(3)?,
        permission_input: row.get(4)?,
        permission_upload: row.get(5)?,
        permission_approval: row.get(6)?,
        issued_at: row.get(7)?,
        device_expires_at: row.get(8)?,
        last_seen_at: row.get(9)?,
        revoked_at: row.get(10)?,
        authorization_epoch: relay_blob(row, 11)?,
    };
    anyhow::ensure!(
        device.identity_public_sec1[0] == 0x04
            && device.issued_at >= 0
            && device.device_expires_at > device.issued_at
            && device
                .last_seen_at
                .is_none_or(|timestamp| timestamp >= device.issued_at)
            && device
                .revoked_at
                .is_none_or(|timestamp| timestamp >= device.issued_at),
        "relay repository row invalid"
    );
    Ok(device)
}

fn sqlite_column_exists(
    conn: &Connection,
    table: &'static str,
    column: &str,
) -> anyhow::Result<bool> {
    let query = match table {
        "relay_devices" => {
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('relay_devices') WHERE name = ?1)"
        }
        "physical_secret_slot_ledger" => {
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('physical_secret_slot_ledger') WHERE name = ?1)"
        }
        _ => anyhow::bail!("지원하지 않는 SQLite 테이블 검사입니다"),
    };
    Ok(conn.query_row(query, [column], |row| row.get(0))?)
}

/// 서로 다른 개발 브랜치가 같은 migration 번호를 사용했던 DB를 정식 원장으로
/// 오인하면 뒤 migration이 잘못된 스키마에 적용된다. v38/v39만 읽기 snapshot에서
/// 지문을 확인하고, 정식 Relay 원장과 다른 형태는 데이터 변경 전에 중단한다.
pub(crate) fn reject_noncanonical_development_schema(path: &Path) -> anyhow::Result<()> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("SQLite metadata 확인 실패: {}", path.display()));
        }
    };
    if !metadata.is_file() {
        return Ok(());
    }

    let mut conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("SQLite schema 확인 실패: {}", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let tx = conn.transaction()?;
    let version = storage_core::read_user_version(&tx)?;
    if !(38..=39).contains(&version) {
        return Ok(());
    }

    let has_reconnect = sqlite_column_exists(&tx, "relay_devices", "reconnect_verifier")?;
    let has_epoch = sqlite_column_exists(&tx, "relay_devices", "authorization_epoch")?;
    let has_recovery =
        sqlite_column_exists(&tx, "physical_secret_slot_ledger", "recovery_generation")?;
    let canonical = match version {
        38 => has_reconnect && !has_epoch && !has_recovery,
        39 => has_reconnect && has_epoch && !has_recovery,
        _ => unreachable!("v38/v39만 검사한다"),
    };
    anyhow::ensure!(
        canonical,
        "지원하지 않는 개발용 v{version} SQLite 스키마입니다 — 정식 migration으로 자동 변환하지 않았습니다"
    );
    Ok(())
}

impl Db {
    /// DB 열기 + 마이그레이션. infra(PRAGMA/백업/IMMEDIATE 러너)는 storage-core가 담당하고
    /// (v2.8 §6.1), 이 crate는 **마이그레이션 원장(MIGRATIONS, v1..vN 순서 불변)** 조립과
    /// 앱 수준 store/facade만 소유한다.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        reject_noncanonical_development_schema(path)?;
        let conn = storage_core::open_with_migrations(path, MIGRATIONS)?;
        let authorization_db_identity = physical_db_identity(path)?;
        Ok(Self {
            conn,
            authorization_db_identity,
        })
    }

    #[cfg(test)]
    fn open_in_memory() -> anyhow::Result<Self> {
        let conn = storage_core::open_in_memory_with_migrations(MIGRATIONS)?;
        Ok(Self {
            conn,
            authorization_db_identity: format!("memory:{}", uuid::Uuid::new_v4()),
        })
    }

    /// 현재 user_version (테스트에서 마이그레이션 가드로 사용).
    #[cfg(test)]
    fn read_user_version(conn: &Connection) -> anyhow::Result<usize> {
        storage_core::read_user_version(conn)
    }

    fn read_connector_config_revision(
        conn: &Connection,
    ) -> anyhow::Result<ConnectorConfigRevision> {
        let revision: i64 = conn.query_row(
            "SELECT revision FROM connector_config_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        ConnectorConfigRevision::from_sql(revision)
    }

    pub fn connector_config_revision(&self) -> anyhow::Result<ConnectorConfigRevision> {
        Self::read_connector_config_revision(&self.conn)
    }

    fn read_connector_config<T>(
        &self,
        read: impl FnOnce(&Connection) -> anyhow::Result<T>,
    ) -> anyhow::Result<ConnectorConfigRead<T>> {
        let tx = self.conn.unchecked_transaction()?;
        let revision = Self::read_connector_config_revision(&tx)?;
        let value = read(&tx)?;
        tx.commit()
            .context("Connector config read transaction 실패")?;
        Ok(ConnectorConfigRead { revision, value })
    }

    fn write_connector_config_cas<T>(
        &mut self,
        expected: ConnectorConfigRevision,
        write: impl FnOnce(&Connection) -> anyhow::Result<T>,
    ) -> anyhow::Result<ConnectorConfigCas<T>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_revision = Self::read_connector_config_revision(&tx)?;
        if current_revision != expected {
            tx.commit()
                .context("stale Connector config CAS transaction 실패")?;
            return Ok(ConnectorConfigCas::Stale { current_revision });
        }
        let value = write(&tx)?;
        let revision = Self::read_connector_config_revision(&tx)?;
        anyhow::ensure!(
            revision >= current_revision,
            "Connector config revision이 감소했습니다"
        );
        tx.commit().context("Connector config CAS commit 실패")?;
        Ok(ConnectorConfigCas::Committed { revision, value })
    }

    fn physical_secret_slot_ledger_state(
        conn: &Connection,
        physical_slot: &str,
    ) -> anyhow::Result<Option<(String, PhysicalSecretSlotState)>> {
        let row = conn
            .query_row(
                "SELECT logical_credential_id, state
                 FROM physical_secret_slot_ledger WHERE physical_slot = ?1",
                [physical_slot],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        row.map(|(logical_id, state)| {
            validate_owned_physical_secret_slot(&logical_id, physical_slot)?;
            Ok((logical_id, PhysicalSecretSlotState::from_persisted(&state)?))
        })
        .transpose()
    }

    fn require_physical_secret_slot_state(
        conn: &Connection,
        logical_id: &str,
        physical_slot: &str,
        expected: PhysicalSecretSlotState,
    ) -> anyhow::Result<()> {
        let (stored_logical_id, state) =
            Self::physical_secret_slot_ledger_state(conn, physical_slot)?
                .with_context(|| format!("physical secret slot ledger row 없음: {logical_id}"))?;
        anyhow::ensure!(
            stored_logical_id == logical_id,
            "physical secret slot ledger owner 불일치"
        );
        anyhow::ensure!(
            state == expected,
            "physical secret slot lifecycle state 불일치"
        );
        Ok(())
    }

    fn transition_physical_secret_slot_state(
        conn: &Connection,
        logical_id: &str,
        physical_slot: &str,
        from: PhysicalSecretSlotState,
        to: PhysicalSecretSlotState,
    ) -> anyhow::Result<()> {
        if let Some(growth) = to.as_str().len().checked_sub(from.as_str().len())
            && growth > 0
        {
            let row_bytes: i64 = conn.query_row(
                "SELECT COALESCE(SUM(
                            length(CAST(physical_slot AS BLOB)) +
                            length(CAST(logical_credential_id AS BLOB)) +
                            length(CAST(state AS BLOB)) +
                            COALESCE(length(CAST(legacy_cleanup_username AS BLOB)), 0)
                        ), 0)
                 FROM physical_secret_slot_ledger",
                [],
                |row| row.get(0),
            )?;
            let row_bytes =
                usize::try_from(row_bytes).context("physical secret slot bytes 변환 실패")?;
            anyhow::ensure!(
                row_bytes
                    .checked_add(growth)
                    .context("physical secret slot state byte overflow")?
                    <= PHYSICAL_SECRET_SLOT_RECONCILIATION_BYTES_MAX,
                "physical secret slot ledger byte capacity 초과"
            );
        }
        let affected = conn.execute(
            "UPDATE physical_secret_slot_ledger
             SET state = ?4, updated_at = CAST(strftime('%s','now') AS INTEGER)
             WHERE physical_slot = ?1 AND logical_credential_id = ?2 AND state = ?3",
            (physical_slot, logical_id, from.as_str(), to.as_str()),
        )?;
        anyhow::ensure!(
            affected == 1,
            "physical secret slot state transition 대상 불일치"
        );
        Ok(())
    }

    fn ensure_physical_secret_slot_ledger_capacity(
        conn: &Connection,
        logical_id: &str,
        physical_slot: &str,
    ) -> anyhow::Result<()> {
        let (row_count, row_bytes): (i64, i64) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(
                        length(CAST(physical_slot AS BLOB)) +
                        length(CAST(logical_credential_id AS BLOB)) +
                        length(CAST(state AS BLOB)) +
                        COALESCE(length(CAST(legacy_cleanup_username AS BLOB)), 0)
                    ), 0)
             FROM physical_secret_slot_ledger",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let row_count =
            usize::try_from(row_count).context("physical secret slot count 변환 실패")?;
        let row_bytes =
            usize::try_from(row_bytes).context("physical secret slot bytes 변환 실패")?;
        anyhow::ensure!(
            row_count < PHYSICAL_SECRET_SLOT_RECONCILIATION_LIMIT_MAX,
            "physical secret slot ledger item capacity 초과"
        );
        let added_bytes = logical_id
            .len()
            .checked_add(physical_slot.len())
            .and_then(|bytes| bytes.checked_add(PhysicalSecretSlotState::Staging.as_str().len()))
            .context("physical secret slot ledger byte 계산 overflow")?;
        let next_bytes = row_bytes
            .checked_add(added_bytes)
            .context("physical secret slot ledger byte capacity overflow")?;
        anyhow::ensure!(
            next_bytes <= PHYSICAL_SECRET_SLOT_RECONCILIATION_BYTES_MAX,
            "physical secret slot ledger byte capacity 초과"
        );
        Ok(())
    }

    fn ensure_legacy_cleanup_marker_capacity(
        conn: &Connection,
        legacy_username: &str,
    ) -> anyhow::Result<()> {
        let row_bytes: i64 = conn.query_row(
            "SELECT COALESCE(SUM(
                        length(CAST(physical_slot AS BLOB)) +
                        length(CAST(logical_credential_id AS BLOB)) +
                        length(CAST(state AS BLOB)) +
                        COALESCE(length(CAST(legacy_cleanup_username AS BLOB)), 0)
                    ), 0)
             FROM physical_secret_slot_ledger",
            [],
            |row| row.get(0),
        )?;
        let row_bytes =
            usize::try_from(row_bytes).context("physical secret slot bytes 변환 실패")?;
        let next_bytes = row_bytes
            .checked_add(legacy_username.len())
            .and_then(|bytes| {
                bytes.checked_add(
                    PhysicalSecretSlotState::Published.as_str().len()
                        - PhysicalSecretSlotState::Staging.as_str().len(),
                )
            })
            .context("legacy cleanup marker byte capacity overflow")?;
        anyhow::ensure!(
            next_bytes <= PHYSICAL_SECRET_SLOT_RECONCILIATION_BYTES_MAX,
            "physical secret slot ledger byte capacity 초과"
        );
        Ok(())
    }

    fn orphan_published_physical_slot_if_versioned(
        conn: &Connection,
        logical_id: &str,
        physical_slot: &str,
    ) -> anyhow::Result<()> {
        if physical_slot == logical_id {
            // Pre-ledger logical-id pointer. There is no exact physical bundle to enumerate.
            return Ok(());
        }
        validate_owned_physical_secret_slot(logical_id, physical_slot)?;
        Self::transition_physical_secret_slot_state(
            conn,
            logical_id,
            physical_slot,
            PhysicalSecretSlotState::Published,
            PhysicalSecretSlotState::Orphan,
        )
    }

    fn insert_credential_with_secret_slot_in_transaction(
        conn: &Connection,
        meta: &CredentialMeta,
        physical_slot: &str,
        oauth_json: Option<&str>,
    ) -> anyhow::Result<()> {
        settings_credential_write_admission(conn, meta)?;
        Self::require_physical_secret_slot_state(
            conn,
            &meta.id,
            physical_slot,
            PhysicalSecretSlotState::Staging,
        )?;
        conn.execute(
            "INSERT INTO credentials
               (id, provider, label, credential_kind,
                keyring_service, keyring_username, masked_hint, workspace_id, oauth_json,
                created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
            (
                &meta.id,
                &meta.provider,
                &meta.label,
                &meta.credential_kind,
                secret::KEYRING_SERVICE,
                physical_slot,
                &meta.masked_hint,
                &meta.workspace_id,
                oauth_json,
            ),
        )
        .with_context(|| format!("physical-slot credential 저장 실패: {}", meta.id))?;
        Self::transition_physical_secret_slot_state(
            conn,
            &meta.id,
            physical_slot,
            PhysicalSecretSlotState::Staging,
            PhysicalSecretSlotState::Published,
        )
    }

    fn publish_credential_secret_slot_in_transaction(
        conn: &Connection,
        logical_id: &str,
        expected_previous_pointer: &str,
        physical_slot: &str,
        oauth_json: Option<&str>,
        masked_hint: Option<&str>,
    ) -> anyhow::Result<bool> {
        Self::require_physical_secret_slot_state(
            conn,
            logical_id,
            physical_slot,
            PhysicalSecretSlotState::Staging,
        )?;
        let Some(mut credential) =
            settings_credential_for_publish(conn, logical_id, expected_previous_pointer)?
        else {
            Self::transition_physical_secret_slot_state(
                conn,
                logical_id,
                physical_slot,
                PhysicalSecretSlotState::Staging,
                PhysicalSecretSlotState::Orphan,
            )?;
            return Ok(false);
        };
        settings_credential_publish_admission(conn, &mut credential, masked_hint)?;
        Self::orphan_published_physical_slot_if_versioned(
            conn,
            logical_id,
            expected_previous_pointer,
        )?;
        let affected = conn
            .execute(
                "UPDATE credentials
                 SET keyring_username = ?3, oauth_json = ?4, masked_hint = ?5,
                     updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE id = ?1 AND keyring_username = ?2",
                (
                    logical_id,
                    expected_previous_pointer,
                    physical_slot,
                    oauth_json,
                    masked_hint,
                ),
            )
            .with_context(|| format!("credential secret slot CAS publish 실패: {logical_id}"))?;
        anyhow::ensure!(
            affected == 1,
            "credential pointer가 transaction 안에서 변경됐습니다"
        );
        Self::transition_physical_secret_slot_state(
            conn,
            logical_id,
            physical_slot,
            PhysicalSecretSlotState::Staging,
            PhysicalSecretSlotState::Published,
        )?;
        Ok(true)
    }

    /// Register the exact bundle base slot before any keyring entry is written. Re-registering
    /// the same still-staging row is idempotent; published/orphan reuse is rejected.
    pub fn register_physical_secret_slot_staging(
        &self,
        logical_id: &str,
        physical_slot: &str,
    ) -> anyhow::Result<()> {
        validate_owned_physical_secret_slot(logical_id, physical_slot)?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some((stored_logical_id, state)) =
            Self::physical_secret_slot_ledger_state(&tx, physical_slot)?
        {
            anyhow::ensure!(
                stored_logical_id == logical_id,
                "physical secret slot owner 불일치"
            );
            anyhow::ensure!(
                state == PhysicalSecretSlotState::Staging,
                "published/orphan physical secret slot은 재사용할 수 없습니다"
            );
        } else {
            Self::ensure_physical_secret_slot_ledger_capacity(&tx, logical_id, physical_slot)?;
            tx.execute(
                "INSERT INTO physical_secret_slot_ledger
                   (physical_slot, logical_credential_id, state, created_at, updated_at)
                 VALUES (?1, ?2, 'staging', CAST(strftime('%s','now') AS INTEGER),
                         CAST(strftime('%s','now') AS INTEGER))",
                (physical_slot, logical_id),
            )?;
        }
        tx.commit()
            .context("physical secret slot staging 등록 commit 실패")
    }

    /// startup에 DB만 읽어 캡처한 후보의 세대·상태를 다시 확인한다.
    /// cleanup은 이 트랜잭션 안에서 실행하여 삭제/재생성·publish와 키 삭제의 ABA를 막는다.
    /// UI에서 호출하지 않으며 callback은 이 Db에 재진입하면 안 된다.
    pub fn recover_physical_secret_slot_cas(
        &self,
        candidate: &PhysicalSecretSlotLedgerRow,
        cleanup: impl FnOnce() -> anyhow::Result<()>,
    ) -> anyhow::Result<bool> {
        validate_owned_physical_secret_slot(
            &candidate.logical_credential_id,
            &candidate.physical_slot,
        )?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = tx
            .query_row(
                "SELECT logical_credential_id, state, legacy_cleanup_username, recovery_generation
             FROM physical_secret_slot_ledger WHERE physical_slot = ?1",
                [&candidate.physical_slot],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((logical, state, legacy, generation)) = current else {
            return Ok(false);
        };
        if logical != candidate.logical_credential_id
            || state != candidate.state.as_str()
            || legacy != candidate.legacy_cleanup_username
            || generation != candidate.recovery_generation
        {
            return Ok(false);
        }
        let references: i64 = tx.query_row(
            "SELECT COUNT(*) FROM credentials WHERE keyring_username = ?1",
            [&candidate.physical_slot],
            |row| row.get(0),
        )?;
        if candidate.state == PhysicalSecretSlotState::Published {
            // 현재 access 슬롯을 읽거나 지우지 않는다. 남은 이전 username만 정리한다.
            if legacy.is_none() {
                return Ok(false);
            }
            anyhow::ensure!(
                references == 1,
                "secret recovery published reference invalid"
            );
        } else {
            anyhow::ensure!(references == 0, "secret recovery candidate referenced");
        }
        cleanup()?;
        if candidate.state == PhysicalSecretSlotState::Published {
            tx.execute("UPDATE physical_secret_slot_ledger SET legacy_cleanup_username = NULL WHERE physical_slot = ?1 AND recovery_generation = ?2", rusqlite::params![candidate.physical_slot, candidate.recovery_generation.as_slice()])?;
        } else {
            tx.execute("DELETE FROM physical_secret_slot_ledger WHERE physical_slot = ?1 AND recovery_generation = ?2", rusqlite::params![candidate.physical_slot, candidate.recovery_generation.as_slice()])?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Acknowledge that an exact staging/orphan keyring bundle is absent or was deleted. Published
    /// slots are never acknowledged away because they are still a live credential capability.
    pub fn acknowledge_physical_secret_slot_deleted(
        &self,
        logical_id: &str,
        physical_slot: &str,
    ) -> anyhow::Result<bool> {
        validate_owned_physical_secret_slot(logical_id, physical_slot)?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let Some((stored_logical_id, state)) =
            Self::physical_secret_slot_ledger_state(&tx, physical_slot)?
        else {
            tx.commit()?;
            return Ok(false);
        };
        anyhow::ensure!(
            stored_logical_id == logical_id,
            "physical secret slot owner 불일치"
        );
        anyhow::ensure!(
            matches!(
                state,
                PhysicalSecretSlotState::Staging | PhysicalSecretSlotState::Orphan
            ),
            "published physical secret slot 삭제 acknowledgement를 거부합니다"
        );
        let legacy_cleanup_username: Option<String> = tx.query_row(
            "SELECT legacy_cleanup_username FROM physical_secret_slot_ledger
             WHERE physical_slot = ?1 AND logical_credential_id = ?2",
            (physical_slot, logical_id),
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            legacy_cleanup_username.is_none(),
            "legacy source cleanup acknowledgement 전 physical slot 삭제를 거부합니다"
        );
        let deleted = tx.execute(
            "DELETE FROM physical_secret_slot_ledger
             WHERE physical_slot = ?1 AND logical_credential_id = ?2 AND state = ?3
               AND legacy_cleanup_username IS NULL",
            (physical_slot, logical_id, state.as_str()),
        )?;
        anyhow::ensure!(
            deleted == 1,
            "physical secret slot acknowledgement 대상 불일치"
        );
        tx.commit()
            .context("physical secret slot acknowledgement commit 실패")?;
        Ok(true)
    }

    /// Complete-or-error bounded startup inventory. The caller performs exact keyring get/delete
    /// operations for these slots; platform-wide keyring enumeration is unnecessary.
    pub fn physical_secret_slots_for_reconciliation(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<PhysicalSecretSlotLedgerRow>> {
        anyhow::ensure!(
            limit <= PHYSICAL_SECRET_SLOT_RECONCILIATION_LIMIT_MAX,
            "physical secret slot reconciliation item limit 초과"
        );
        let probe = limit
            .checked_add(1)
            .context("physical secret slot limit overflow")?;
        let sql_limit = i64::try_from(probe).context("physical secret slot LIMIT 변환 실패")?;
        let tx = self.conn.unchecked_transaction()?;
        let (row_count, row_bytes): (i64, i64) = tx.query_row(
            "SELECT COUNT(*), COALESCE(SUM(
                        length(CAST(physical_slot AS BLOB)) +
                        length(CAST(logical_credential_id AS BLOB)) +
                        length(CAST(state AS BLOB)) +
                        COALESCE(length(CAST(legacy_cleanup_username AS BLOB)), 0)
                    ), 0)
             FROM (
                 SELECT physical_slot, logical_credential_id, state, legacy_cleanup_username
                 FROM physical_secret_slot_ledger
                 ORDER BY created_at, physical_slot LIMIT ?1
             )",
            [sql_limit],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let row_count =
            usize::try_from(row_count).context("physical secret slot count 변환 실패")?;
        let row_bytes =
            usize::try_from(row_bytes).context("physical secret slot bytes 변환 실패")?;
        anyhow::ensure!(
            row_count <= limit,
            "physical secret slot reconciliation limit 초과"
        );
        anyhow::ensure!(
            row_bytes <= PHYSICAL_SECRET_SLOT_RECONCILIATION_BYTES_MAX,
            "physical secret slot reconciliation byte budget 초과"
        );
        let rows = {
            let mut statement = tx.prepare(
                "SELECT ledger.physical_slot, ledger.logical_credential_id, ledger.state,
                        ledger.legacy_cleanup_username,
                        (SELECT COUNT(*) FROM credentials c
                         WHERE c.keyring_username = ledger.physical_slot),
                        (SELECT COUNT(*) FROM credentials c
                         WHERE c.id = ledger.logical_credential_id
                           AND c.keyring_username = ledger.physical_slot),
                        ledger.recovery_generation
                 FROM physical_secret_slot_ledger ledger
                 ORDER BY ledger.created_at, ledger.physical_slot LIMIT ?1",
            )?;
            let mapped = statement.query_map([sql_limit], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Vec<u8>>(6)?,
                ))
            })?;
            let mut rows = Vec::with_capacity(row_count);
            for row in mapped {
                let (
                    physical_slot,
                    logical_credential_id,
                    state,
                    legacy_cleanup_username,
                    any_refs,
                    exact_refs,
                    recovery_generation,
                ) = row?;
                validate_owned_physical_secret_slot(&logical_credential_id, &physical_slot)?;
                if let Some(username) = legacy_cleanup_username.as_deref() {
                    validate_legacy_cleanup_username(&logical_credential_id, username)?;
                }
                let state = PhysicalSecretSlotState::from_persisted(&state)?;
                match state {
                    PhysicalSecretSlotState::Published => anyhow::ensure!(
                        any_refs == 1 && exact_refs == 1,
                        "published physical secret slot credential reference 불일치"
                    ),
                    PhysicalSecretSlotState::Staging | PhysicalSecretSlotState::Orphan => {
                        anyhow::ensure!(
                            any_refs == 0 && exact_refs == 0,
                            "non-published physical secret slot이 credential에 참조됩니다"
                        );
                    }
                }
                rows.push(PhysicalSecretSlotLedgerRow {
                    recovery_generation: recovery_generation
                        .try_into()
                        .map_err(|_| anyhow::anyhow!("secret recovery generation invalid"))?,
                    logical_credential_id,
                    physical_slot,
                    state,
                    legacy_cleanup_username,
                });
            }
            rows
        };
        anyhow::ensure!(
            rows.len() == row_count,
            "physical secret slot same-snapshot count 불일치"
        );
        tx.commit()
            .context("physical secret slot reconciliation read commit 실패")?;
        Ok(rows)
    }

    /// Exact idempotent acknowledgement after the legacy base, `.refresh`, and `.dcr` entries are
    /// all absent. It clears only the marker on the exact published-slot ledger row; physical-slot
    /// reconciliation may delete an orphan row only after this succeeds.
    pub fn acknowledge_legacy_secret_source_deleted(
        &self,
        logical_id: &str,
        published_physical_slot: &str,
        expected_legacy_username: &str,
    ) -> anyhow::Result<bool> {
        validate_owned_physical_secret_slot(logical_id, published_physical_slot)?;
        validate_legacy_cleanup_username(logical_id, expected_legacy_username)?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let updated = tx.execute(
            "UPDATE physical_secret_slot_ledger
             SET legacy_cleanup_username = NULL,
                 updated_at = CAST(strftime('%s','now') AS INTEGER)
             WHERE physical_slot = ?1 AND logical_credential_id = ?2
               AND legacy_cleanup_username = ?3",
            (
                published_physical_slot,
                logical_id,
                expected_legacy_username,
            ),
        )?;
        anyhow::ensure!(
            updated <= 1,
            "legacy cleanup acknowledgement 대상이 유일하지 않습니다"
        );
        tx.commit()
            .context("legacy cleanup acknowledgement commit 실패")?;
        Ok(updated == 1)
    }

    /// credential metadata 추가. created_at/updated_at은 SQLite가 UTC로 기록한다.
    ///
    /// 이 기존 생성 경로는 IN01 migration/cutover 전까지 logical id 자체를 keyring username에
    /// 보관한다. 새 physical bundle publish와 secret-backed 실행은 이 legacy pointer를 허용하지
    /// 않으며, [`Self::rotate_credential_secret_slot`]을 거쳐야 한다.
    pub fn insert_credential(&self, meta: &CredentialMeta) -> anyhow::Result<()> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        settings_credential_write_admission(&tx, meta)?;
        tx.execute(
            "INSERT INTO credentials
                   (id, provider, label, credential_kind,
                    keyring_service, keyring_username, masked_hint, workspace_id,
                    created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                    strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
            (
                &meta.id,
                &meta.provider,
                &meta.label,
                &meta.credential_kind,
                secret::KEYRING_SERVICE,
                &meta.id, // keyring username = credential id
                &meta.masked_hint,
                &meta.workspace_id,
            ),
        )
        .with_context(|| format!("credential 저장 실패: {}", meta.id))?;
        tx.commit().context("credential insert commit failed")
    }

    /// Inserts credential metadata with an already-staged, owned physical keyring slot. This is
    /// the only new-credential path suitable for the post-IN01 physical bundle model: it never
    /// publishes the logical credential id as a keyring pointer. `None` preserves SQL NULL for
    /// credentials without OAuth metadata.
    pub fn insert_credential_with_secret_slot(
        &self,
        meta: &CredentialMeta,
        physical_slot: &str,
        oauth_json: Option<&str>,
    ) -> anyhow::Result<()> {
        validate_owned_physical_secret_slot(&meta.id, physical_slot)?;
        if let Some(json) = oauth_json {
            validate_oauth_metadata_json(json)?;
        }
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        Self::insert_credential_with_secret_slot_in_transaction(
            &tx,
            meta,
            physical_slot,
            oauth_json,
        )?;
        tx.commit()
            .context("physical-slot credential/ledger publish commit 실패")
    }

    pub fn insert_credential_with_secret_slot_revision_cas(
        &mut self,
        expected_revision: ConnectorConfigRevision,
        meta: &CredentialMeta,
        physical_slot: &str,
        oauth_json: Option<&str>,
    ) -> anyhow::Result<ConnectorConfigCas<()>> {
        validate_owned_physical_secret_slot(&meta.id, physical_slot)?;
        if let Some(json) = oauth_json {
            validate_oauth_metadata_json(json)?;
        }
        self.write_connector_config_cas(expected_revision, |conn| {
            Self::insert_credential_with_secret_slot_in_transaction(
                conn,
                meta,
                physical_slot,
                oauth_json,
            )
        })
    }

    pub fn list_credentials(&self) -> anyhow::Result<Vec<CredentialMeta>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, provider, label, credential_kind, masked_hint, workspace_id
             FROM credentials ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(CredentialMeta {
                id: row.get(0)?,
                provider: row.get(1)?,
                label: row.get(2)?,
                credential_kind: row.get(3)?,
                masked_hint: row.get(4)?,
                workspace_id: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Returns one consistent, bounded scan of credential metadata and keyring pointers for
    /// bootstrap migration/reconciliation. Exactly `limit` rows are allowed; one extra row is
    /// fetched only to prove that the caller-provided bound was exceeded.
    pub fn list_credential_secret_records(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<CredentialSecretRecord>> {
        let fetch_limit = limit
            .checked_add(1)
            .context("credential secret record limit overflow")?;
        let fetch_limit = i64::try_from(fetch_limit)
            .context("credential secret record limit exceeds SQLite range")?;
        let tx = self.conn.unchecked_transaction()?;
        let (row_count, row_bytes): (i64, i64) = tx.query_row(
            "SELECT COUNT(*), COALESCE(SUM(
                        length(CAST(id AS BLOB)) +
                        length(CAST(provider AS BLOB)) +
                        length(CAST(label AS BLOB)) +
                        length(CAST(credential_kind AS BLOB)) +
                        COALESCE(length(CAST(masked_hint AS BLOB)), 0) +
                        COALESCE(length(CAST(workspace_id AS BLOB)), 0) +
                        length(CAST(keyring_service AS BLOB)) +
                        length(CAST(keyring_username AS BLOB)) +
                        COALESCE(length(CAST(oauth_json AS BLOB)), 0)
                    ), 0)
             FROM (
                 SELECT id, provider, label, credential_kind, masked_hint, workspace_id,
                        keyring_service, keyring_username, oauth_json
                 FROM credentials ORDER BY created_at, id LIMIT ?1
             )",
            [fetch_limit],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let row_count =
            usize::try_from(row_count).context("credential secret record count 변환 실패")?;
        let row_bytes =
            usize::try_from(row_bytes).context("credential secret record bytes 변환 실패")?;
        anyhow::ensure!(
            row_count <= limit,
            "credential secret record limit exceeded: limit={limit}"
        );
        anyhow::ensure!(
            row_bytes <= CREDENTIAL_SECRET_RECORD_BYTES_MAX,
            "credential secret record byte 상한을 초과했습니다"
        );
        let exact_limit =
            i64::try_from(row_count).context("credential secret record count SQLite 변환 실패")?;
        let records = {
            let mut stmt = tx.prepare_cached(
                "SELECT id, provider, label, credential_kind, masked_hint, workspace_id,
                        keyring_service, keyring_username, oauth_json
                 FROM credentials ORDER BY created_at, id LIMIT ?1",
            )?;
            let rows = stmt.query_map([exact_limit], |row| {
                Ok(CredentialSecretRecord {
                    meta: CredentialMeta {
                        id: row.get(0)?,
                        provider: row.get(1)?,
                        label: row.get(2)?,
                        credential_kind: row.get(3)?,
                        masked_hint: row.get(4)?,
                        workspace_id: row.get(5)?,
                    },
                    keyring_service: row.get(6)?,
                    keyring_username: row.get(7)?,
                    oauth_json: row.get(8)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        anyhow::ensure!(
            records.len() == row_count,
            "credential secret record same-snapshot count 불일치"
        );
        tx.commit()
            .context("credential secret record snapshot commit 실패")?;
        Ok(records)
    }

    /// 이 workspace에서 보이는 credential — 소속(workspace_id=ws) + 전역(NULL). (#2)
    pub fn list_credentials_for_workspace(
        &self,
        workspace_id: &str,
    ) -> anyhow::Result<Vec<CredentialMeta>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, provider, label, credential_kind, masked_hint, workspace_id
             FROM credentials
             WHERE workspace_id IS NULL OR workspace_id = ?1
             ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([workspace_id], |row| {
            Ok(CredentialMeta {
                id: row.get(0)?,
                provider: row.get(1)?,
                label: row.get(2)?,
                credential_kind: row.get(3)?,
                masked_hint: row.get(4)?,
                workspace_id: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// OAuth 연계 메타데이터(JSON, 비밀 아님 — v20)를 갱신한다 (PR-H5).
    /// 값 스키마는 앱(connectors)의 OAuthConnection 직렬화가 소유한다.
    pub fn set_credential_oauth_json(&self, id: &str, json: &str) -> anyhow::Result<()> {
        validate_oauth_metadata_json(json)?;
        let affected = self
            .conn
            .execute(
                "UPDATE credentials
                 SET oauth_json = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE id = ?1",
                (id, json),
            )
            .with_context(|| format!("credential oauth 메타 저장 실패: {id}"))?;
        anyhow::ensure!(affected == 1, "credential 없음: {id}");
        Ok(())
    }

    /// 새 access/refresh/DCR bundle이 이미 기록된 physical slot을 OAuth metadata와 함께
    /// 한 transaction으로 publish한다. 이 함수는 secret 값을 받거나 저장하지 않는다.
    /// 성공 시 이전 published slot은 같은 transaction에서 orphan이 된다. 호출자는 commit
    /// 이후 그 exact bundle만 keyring에서 지우고 acknowledgement API로 ledger를 정리한다.
    pub fn rotate_credential_secret_slot(
        &self,
        id: &str,
        physical_slot: &str,
        oauth_json: &str,
        masked_hint: Option<&str>,
    ) -> anyhow::Result<()> {
        // Ownership and the versioned physical format must be proven before opening the
        // transaction. A staged slot for another logical credential can never be published.
        validate_owned_physical_secret_slot(id, physical_slot)?;
        validate_oauth_metadata_json(oauth_json)?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current_pointer: String = tx
            .query_row(
                "SELECT keyring_username FROM credentials WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .with_context(|| format!("credential 없음: {id}"))?;
        let published = Self::publish_credential_secret_slot_in_transaction(
            &tx,
            id,
            &current_pointer,
            physical_slot,
            Some(oauth_json),
            masked_hint,
        )?;
        anyhow::ensure!(published, "credential pointer publish 대상 없음: {id}");
        tx.commit()
            .context("credential secret slot/metadata/ledger commit 실패")
    }

    /// Dedicated one-time legacy migration publish. Unlike regular creation/rotation, a successful
    /// logical-username CAS records the exact access/refresh/DCR source cleanup obligation in the
    /// same transaction as the physical pointer and ledger publication. Passing any expected
    /// pointer other than the logical credential id is rejected, preventing regular rotations from
    /// accidentally creating legacy cleanup markers.
    pub fn publish_legacy_credential_secret_slot_cas(
        &self,
        logical_id: &str,
        expected_legacy_pointer: &str,
        physical_slot: &str,
        oauth_json: Option<&str>,
        masked_hint: Option<&str>,
    ) -> anyhow::Result<bool> {
        validate_owned_physical_secret_slot(logical_id, physical_slot)?;
        anyhow::ensure!(
            expected_legacy_pointer == logical_id,
            "legacy migration expected pointer는 logical credential id여야 합니다"
        );
        if let Some(json) = oauth_json {
            validate_oauth_metadata_json(json)?;
        }
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;

        validate_legacy_cleanup_username(logical_id, expected_legacy_pointer)?;
        let marker_ready: bool = tx.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM credentials WHERE id = ?1 AND keyring_username = ?2
             )",
            (logical_id, expected_legacy_pointer),
            |row| row.get(0),
        )?;
        if marker_ready {
            Self::ensure_legacy_cleanup_marker_capacity(&tx, expected_legacy_pointer)?;
        }

        let published = Self::publish_credential_secret_slot_in_transaction(
            &tx,
            logical_id,
            expected_legacy_pointer,
            physical_slot,
            oauth_json,
            masked_hint,
        )?;
        if published {
            anyhow::ensure!(
                marker_ready,
                "legacy publish source marker가 준비되지 않았습니다"
            );
            let marked = tx.execute(
                "UPDATE physical_secret_slot_ledger
                 SET legacy_cleanup_username = ?3,
                     updated_at = CAST(strftime('%s','now') AS INTEGER)
                 WHERE physical_slot = ?1 AND logical_credential_id = ?2
                   AND state = 'published' AND legacy_cleanup_username IS NULL",
                (physical_slot, logical_id, expected_legacy_pointer),
            )?;
            anyhow::ensure!(marked == 1, "legacy cleanup marker publish 대상 불일치");
        } else {
            anyhow::ensure!(!marker_ready, "legacy pointer CAS 결과가 일관되지 않습니다");
        }
        tx.commit()
            .context("legacy credential pointer/cleanup marker commit 실패")?;
        Ok(published)
    }

    /// Compare-and-swap publishes an already-staged physical slot together with its optional
    /// OAuth metadata and masked hint. The update occurs only while the stored pointer exactly
    /// matches `expected_previous_pointer`; stale or missing rows return `false` after moving the
    /// new staged slot to orphan, without changing Connector revision or credential columns.
    /// `None` writes SQL NULL, preserving the non-OAuth representation.
    pub fn publish_credential_secret_slot_cas(
        &self,
        logical_id: &str,
        expected_previous_pointer: &str,
        physical_slot: &str,
        oauth_json: Option<&str>,
        masked_hint: Option<&str>,
    ) -> anyhow::Result<bool> {
        validate_owned_physical_secret_slot(logical_id, physical_slot)?;
        if let Some(json) = oauth_json {
            validate_oauth_metadata_json(json)?;
        }
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let published = Self::publish_credential_secret_slot_in_transaction(
            &tx,
            logical_id,
            expected_previous_pointer,
            physical_slot,
            oauth_json,
            masked_hint,
        )?;
        tx.commit()
            .context("credential secret slot CAS/ledger commit 실패")?;
        Ok(published)
    }

    /// Expected-config-revision CAS wrapped around the physical-pointer CAS. A stale config
    /// writer never evaluates the pointer mutation and leaves staging intact. When the global
    /// revision matches but the physical pointer is stale, the new slot becomes orphan at the
    /// unchanged revision and returns `value=false`.
    pub fn publish_credential_secret_slot_revision_cas(
        &mut self,
        expected_revision: ConnectorConfigRevision,
        logical_id: &str,
        expected_previous_pointer: &str,
        physical_slot: &str,
        oauth_json: Option<&str>,
        masked_hint: Option<&str>,
    ) -> anyhow::Result<ConnectorConfigCas<bool>> {
        validate_owned_physical_secret_slot(logical_id, physical_slot)?;
        if let Some(json) = oauth_json {
            validate_oauth_metadata_json(json)?;
        }
        self.write_connector_config_cas(expected_revision, |conn| {
            Self::publish_credential_secret_slot_in_transaction(
                conn,
                logical_id,
                expected_previous_pointer,
                physical_slot,
                oauth_json,
                masked_hint,
            )
        })
    }

    /// logical credential id를 keyring physical slot으로 해석한다. secret 본문은 반환하지 않는다.
    pub fn credential_secret_location(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<CredentialSecretLocation>> {
        let tx = self.conn.unchecked_transaction()?;
        let location = Self::credential_secret_location_in_snapshot(&tx, id)?;
        tx.commit()
            .context("credential secret location read transaction 실패")?;
        Ok(location)
    }

    fn credential_secret_location_in_snapshot(
        conn: &Connection,
        id: &str,
    ) -> anyhow::Result<Option<CredentialSecretLocation>> {
        anyhow::ensure!(
            !conn.is_autocommit(),
            "credential secret location snapshot은 caller-owned transaction이 필요합니다"
        );
        let location_bytes = conn
            .query_row(
                "SELECT length(CAST(keyring_service AS BLOB)) +
                        length(CAST(keyring_username AS BLOB))
                 FROM credentials WHERE id = ?1",
                [id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let Some(location_bytes) = location_bytes else {
            return Ok(None);
        };
        let location_bytes = usize::try_from(location_bytes)
            .context("credential secret location bytes 변환 실패")?;
        anyhow::ensure!(
            location_bytes <= CREDENTIAL_SECRET_LOCATION_BYTES_MAX,
            "credential secret location byte 상한을 초과했습니다"
        );
        conn.query_row(
            "SELECT keyring_service, keyring_username FROM credentials WHERE id = ?1",
            [id],
            |row| {
                Ok(CredentialSecretLocation {
                    keyring_service: row.get(0)?,
                    keyring_username: row.get(1)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn credential_secret_location_versioned(
        &self,
        id: &str,
    ) -> anyhow::Result<ConnectorConfigRead<Option<CredentialSecretLocation>>> {
        self.read_connector_config(|conn| Self::credential_secret_location_in_snapshot(conn, id))
    }

    /// oauth_json이 있는 credential (id, json) 목록 — H5가 서버 바인딩을 찾는 데 쓴다.
    pub fn list_credential_oauth_json(&self) -> anyhow::Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, oauth_json FROM credentials
             WHERE oauth_json IS NOT NULL ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Bounded point lookup for OAuth HTTP auth binding. The JSON expression intentionally fails
    /// on corrupt metadata, and `LIMIT 2` lets the app distinguish exactly-one from duplicate
    /// bindings without ever materializing the full OAuth credential list.
    pub fn credential_oauth_bindings_for_server(
        &self,
        server_id: &str,
    ) -> anyhow::Result<Vec<CredentialOAuthBindingRecord>> {
        let tx = self.conn.unchecked_transaction()?;
        let candidates = Self::credential_oauth_bindings_for_server_in_snapshot(&tx, server_id)?;
        tx.commit()
            .context("OAuth binding point lookup transaction 실패")?;
        Ok(candidates)
    }

    pub fn credential_oauth_bindings_for_server_versioned(
        &self,
        server_id: &str,
    ) -> anyhow::Result<ConnectorConfigRead<Vec<CredentialOAuthBindingRecord>>> {
        self.read_connector_config(|conn| {
            Self::credential_oauth_bindings_for_server_in_snapshot(conn, server_id)
        })
    }

    fn credential_oauth_bindings_for_server_in_snapshot(
        conn: &Connection,
        server_id: &str,
    ) -> anyhow::Result<Vec<CredentialOAuthBindingRecord>> {
        anyhow::ensure!(
            !conn.is_autocommit(),
            "OAuth binding snapshot은 caller-owned transaction이 필요합니다"
        );
        let (candidate_count, candidate_bytes): (i64, i64) = conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(
                        length(CAST(id AS BLOB)) +
                        length(CAST(keyring_service AS BLOB)) +
                        length(CAST(keyring_username AS BLOB)) +
                        length(CAST(oauth_json AS BLOB))
                    ), 0)
             FROM (
                 SELECT id, keyring_service, keyring_username, oauth_json
                 FROM credentials
                 WHERE oauth_json IS NOT NULL
                   AND json_extract(oauth_json, '$.server_id') = ?1
                 ORDER BY created_at, id
                 LIMIT 2
             )",
            [server_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let candidate_count =
            usize::try_from(candidate_count).context("OAuth binding count 변환 실패")?;
        let candidate_bytes =
            usize::try_from(candidate_bytes).context("OAuth binding bytes 변환 실패")?;
        anyhow::ensure!(candidate_count <= 2, "OAuth binding lookup 상한 위반");
        anyhow::ensure!(
            candidate_bytes <= CREDENTIAL_OAUTH_BINDING_BYTES_MAX,
            "OAuth binding metadata byte 상한을 초과했습니다"
        );
        let candidates = {
            let mut stmt = conn.prepare_cached(
                "SELECT id, keyring_service, keyring_username, oauth_json
                 FROM credentials
                 WHERE oauth_json IS NOT NULL
                   AND json_extract(oauth_json, '$.server_id') = ?1
                 ORDER BY created_at, id
                 LIMIT 2",
            )?;
            let rows = stmt.query_map([server_id], |row| {
                Ok(CredentialOAuthBindingRecord {
                    logical_id: row.get(0)?,
                    keyring_service: row.get(1)?,
                    physical_pointer: row.get(2)?,
                    oauth_metadata_json: row.get(3)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        anyhow::ensure!(
            candidates.len() == candidate_count,
            "OAuth binding same-snapshot count 불일치"
        );
        for candidate in &candidates {
            validate_oauth_metadata_json(&candidate.oauth_metadata_json)?;
            validate_owned_physical_secret_slot(
                &candidate.logical_id,
                &candidate.physical_pointer,
            )?;
        }
        Ok(candidates)
    }

    /// 참조가 없을 때만 metadata 행을 지운다 — 확인과 삭제를 한 문장으로 묶어
    /// "확인 후 삭제 사이에 참조가 생기는" TOCTOU를 없앤다 (codex 리뷰).
    /// 지웠으면 true, 참조 중이거나 없는 id면 false.
    pub fn delete_credential_if_unused(&self, id: &str) -> anyhow::Result<bool> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let pointer = tx
            .query_row(
                "SELECT keyring_username FROM credentials WHERE id = ?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let affected = tx
            .execute(
                "DELETE FROM credentials WHERE id = ?1
                   AND NOT EXISTS (SELECT 1 FROM env_vars WHERE credential_id = ?1)
                   AND NOT EXISTS (SELECT 1 FROM workspace_credential_env WHERE credential_id = ?1)
                   AND NOT EXISTS (
                       SELECT 1
                       FROM mcp_servers, json_each(COALESCE(mcp_servers.env_credentials_json, '{}'))
                       WHERE json_each.value = ?1
                   )",
                [id],
            )
            .with_context(|| format!("credential 삭제 실패: {id}"))?;
        if affected == 1 {
            let pointer = pointer.context("삭제된 credential pointer snapshot 없음")?;
            Self::orphan_published_physical_slot_if_versioned(&tx, id, &pointer)?;
        }
        tx.commit()
            .context("credential delete/slot orphan commit 실패")?;
        Ok(affected == 1)
    }

    /// Delete credential metadata only when no live env/MCP reference exists and the physical
    /// pointer still equals the caller's expected value. The single conditional DELETE closes the
    /// delete-vs-rotation race: stale cleanup is a harmless no-op and cannot remove a newer slot.
    pub fn delete_credential_if_unused_cas(
        &self,
        id: &str,
        expected_pointer: &str,
    ) -> anyhow::Result<bool> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let affected = tx
            .execute(
                "DELETE FROM credentials WHERE id = ?1 AND keyring_username = ?2
                   AND NOT EXISTS (SELECT 1 FROM env_vars WHERE credential_id = ?1)
                   AND NOT EXISTS (SELECT 1 FROM workspace_credential_env WHERE credential_id = ?1)
                   AND NOT EXISTS (
                       SELECT 1
                       FROM mcp_servers, json_each(COALESCE(mcp_servers.env_credentials_json, '{}'))
                       WHERE json_each.value = ?1
                   )",
                (id, expected_pointer),
            )
            .with_context(|| format!("credential expected-pointer 삭제 실패: {id}"))?;
        if affected == 1 {
            Self::orphan_published_physical_slot_if_versioned(&tx, id, expected_pointer)?;
        }
        tx.commit()
            .context("credential expected-pointer delete/slot orphan commit 실패")?;
        Ok(affected == 1)
    }

    /// env var 또는 MCP scoped env가 이 credential을 참조 중인지 확인 (UI 에러 메시지 구분용).
    /// env var가 참조 중인 credential id 집합 — 환경 UI의 "API 키" 표에서 .env 자동
    /// 동기화로 생긴 credential을 숨겨 환경 변수 표와의 이중 표시를 막는다(2026-07-09).
    pub fn env_referenced_credential_ids(
        &self,
    ) -> anyhow::Result<std::collections::HashSet<String>> {
        let mut stmt = self.conn.prepare_cached(
            // **dotenv 자동 동기화 profile**의 참조만 — 수동 등록 credential을 다른
            // profile에서 참조해도 API 키 표에 남긴다(과필터 방지, codex Med 2026-07-09).
            "SELECT DISTINCT ev.credential_id
             FROM env_vars ev JOIN env_profiles ep ON ev.profile_id = ep.id
             WHERE ep.kind = 'dotenv' AND ev.credential_id IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = std::collections::HashSet::new();
        for row in rows {
            out.insert(row?);
        }
        Ok(out)
    }

    /// Dotenv가 소유한 env-provider credential id만 SQL에서 제한·필터한다. 호출자는
    /// 필요하면 이 deterministic Vec를 HashSet으로 투영할 수 있다.
    pub fn list_dotenv_owned_credential_ids_bounded(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<String>> {
        let sql_limit = bounded_limit_plus_one(limit, DOTENV_CREDENTIAL_ROWS_MAX)?;
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
        let probe = bounded_read_preflight(
            &tx,
            DOTENV_CREDENTIALS_BOUNDED_PREFLIGHT,
            rusqlite::params![
                sql_limit,
                BOUNDED_ID_BYTES_MAX as i64,
                BOUNDED_ROW_BYTES_MAX as i64,
            ],
            limit,
        )?;
        let mut result = Vec::with_capacity(probe.count);
        {
            let mut stmt = tx
                .prepare(DOTENV_CREDENTIALS_BOUNDED_SELECT)
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            let mut rows = stmt
                .query(rusqlite::params![sql_limit, BOUNDED_ID_BYTES_MAX as i64])
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            while let Some(row) = rows
                .next()
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?
            {
                result.push(
                    bounded_required_text(row, 0, BOUNDED_ID_BYTES_MAX, true, true)?.to_owned(),
                );
            }
        }
        tx.commit()
            .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
        Ok(result)
    }

    /// MCP 서버 env가 참조하는 credential id 집합 — orphan 정리의 live set에 포함해
    /// 실제 사용 중인 MCP secret을 지우지 않게 한다(codex Med 2026-07-09).
    pub fn mcp_referenced_credential_ids(
        &self,
    ) -> anyhow::Result<std::collections::HashSet<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT json_each.value
             FROM mcp_servers, json_each(COALESCE(mcp_servers.env_credentials_json, '{}'))",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = std::collections::HashSet::new();
        for row in rows {
            out.insert(row?);
        }
        Ok(out)
    }

    pub fn credential_in_use(&self, id: &str) -> anyhow::Result<bool> {
        let exists: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM env_vars WHERE credential_id = ?1
                 UNION ALL
                 SELECT 1 FROM workspace_credential_env WHERE credential_id = ?1
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
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(id) = tx
            .query_row(
                "SELECT id FROM workspaces ORDER BY created_at LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            tx.commit()
                .context("default workspace existing transaction commit failed")?;
            return Ok(id);
        }
        let id = uuid::Uuid::new_v4().to_string();
        settings_workspace_write_admission(&tx, &id, "default", "", None, None)?;
        tx.execute(
            "INSERT INTO workspaces (id, name, path, created_at, updated_at)
             VALUES (?1, 'default', '',
                strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
            [&id],
        )?;
        tx.commit()
            .context("default workspace create transaction commit failed")?;
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

    /// Complete-or-error workspace projection from one SQLite snapshot. The `limit + 1` count,
    /// aggregate byte size, and maximum row size are checked in SQL before any workspace TEXT is
    /// materialized in Rust.
    pub fn settings_workspace_projection_rows(
        &self,
    ) -> anyhow::Result<Vec<SettingsWorkspaceProjectionRow>> {
        let tx = self.conn.unchecked_transaction()?;
        let sql_limit = settings_sql_probe_limit(
            SETTINGS_WORKSPACE_LIMIT_MAX,
            "settings_workspace_projection",
        )?;
        let probe = settings_read_probe(
            &tx,
            "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
             FROM (
                 SELECT length(CAST(id AS BLOB)) + length(CAST(name AS BLOB)) +
                        length(CAST(path AS BLOB)) + length(CAST(created_at AS BLOB)) +
                        length(CAST(COALESCE(path_dev, '') AS BLOB)) +
                        length(CAST(COALESCE(path_ino, '') AS BLOB)) AS row_bytes
                 FROM workspaces ORDER BY created_at, id LIMIT ?1
             )",
            [sql_limit],
            SETTINGS_WORKSPACE_LIMIT_MAX,
            SETTINGS_SNAPSHOT_BYTES_MAX,
            SETTINGS_ROW_BYTES_MAX,
            "settings_workspace_projection",
        )?;
        let invalid_anchors: i64 = tx.query_row(
            "SELECT COALESCE(SUM(CASE
                        WHEN (path_dev IS NULL) <> (path_ino IS NULL) THEN 1 ELSE 0 END), 0)
             FROM (
                 SELECT path_dev, path_ino
                 FROM workspaces ORDER BY created_at, id LIMIT ?1
             )",
            [sql_limit],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            invalid_anchors == 0,
            "settings_workspace_projection_anchor_invalid"
        );
        let mut stmt = tx.prepare(
            "SELECT id, name, path, created_at, path_dev, path_ino
             FROM workspaces ORDER BY created_at, id LIMIT ?1",
        )?;
        let rows = stmt.query_map([sql_limit], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })?;
        let mut projection = Vec::with_capacity(probe.count);
        for row in rows {
            projection.push(settings_workspace_projection_from_persisted(row?)?);
        }
        drop(stmt);
        anyhow::ensure!(
            projection.len() == probe.count,
            "settings_workspace_projection_snapshot_changed"
        );
        tx.commit()?;
        Ok(projection)
    }

    /// Finds one workspace by exact path or atomically creates its name, path, and folder anchor.
    /// The transaction reads no more than two matching rows and fails closed before materializing
    /// either row when duplicate paths already exist.
    /// Without a stable volume UUID the entire device/inode anchor must match.
    pub fn find_or_create_workspace_by_exact_path(
        &self,
        name: &str,
        path: &str,
        folder_anchor: WorkspaceFolderAnchor,
    ) -> anyhow::Result<WorkspaceFindOrCreateResult> {
        self.find_or_create_workspace_by_exact_path_with_volume(name, path, folder_anchor, None)
    }

    /// A previously verified volume UUID plus inode can prove identity after
    /// device renumbering. Unknown/different volumes must not inherit sessions.
    pub fn find_or_create_workspace_by_exact_path_with_volume(
        &self,
        name: &str,
        path: &str,
        folder_anchor: WorkspaceFolderAnchor,
        volume: Option<uuid::Uuid>,
    ) -> anyhow::Result<WorkspaceFindOrCreateResult> {
        self.find_or_create_workspace_by_exact_path_with_volume_policy(
            name,
            path,
            folder_anchor,
            volume,
            false,
        )
    }

    /// A folder explicitly chosen by the user may reconnect a legacy exact-path row after
    /// device renumbering when its inode still matches and macOS supplies a current volume UUID.
    /// An existing UUID mismatch and alias-path reuse remain rejected.
    pub fn find_or_create_selected_workspace_by_exact_path_with_volume(
        &self,
        name: &str,
        path: &str,
        folder_anchor: WorkspaceFolderAnchor,
        volume: Option<uuid::Uuid>,
    ) -> anyhow::Result<WorkspaceFindOrCreateResult> {
        self.find_or_create_workspace_by_exact_path_with_volume_policy(
            name,
            path,
            folder_anchor,
            volume,
            true,
        )
    }

    fn find_or_create_workspace_by_exact_path_with_volume_policy(
        &self,
        name: &str,
        path: &str,
        folder_anchor: WorkspaceFolderAnchor,
        volume: Option<uuid::Uuid>,
        allow_selected_legacy_rebind: bool,
    ) -> anyhow::Result<WorkspaceFindOrCreateResult> {
        anyhow::ensure!(
            volume.is_none_or(|id| !id.is_nil()),
            "workspace_volume_identity_invalid"
        );
        validate_workspace_text_input(name, "name")?;
        validate_workspace_text_input(path, "path")?;
        let input_bytes = name
            .len()
            .checked_add(path.len())
            .context("settings_workspace_find_or_create_bytes_overflow")?;
        anyhow::ensure!(
            input_bytes <= SETTINGS_ROW_BYTES_MAX,
            "settings_workspace_find_or_create_row_bytes_limit"
        );

        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let existing_probe = settings_read_probe(
            &tx,
            "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
             FROM (
                 SELECT length(CAST(id AS BLOB)) + length(CAST(name AS BLOB)) +
                        length(CAST(path AS BLOB)) + length(CAST(created_at AS BLOB)) +
                        length(CAST(COALESCE(path_dev, '') AS BLOB)) +
                        length(CAST(COALESCE(path_ino, '') AS BLOB)) AS row_bytes
                 FROM workspaces WHERE path = ?1 ORDER BY created_at, id LIMIT 2
             )",
            [path],
            2,
            SETTINGS_SNAPSHOT_BYTES_MAX,
            SETTINGS_ROW_BYTES_MAX,
            "settings_workspace_exact_path",
        )?;
        anyhow::ensure!(
            existing_probe.count < 2,
            "settings_workspace_exact_path_duplicate"
        );
        if existing_probe.count == 1 {
            let persisted = tx.query_row(
                "SELECT id, name, path, created_at, path_dev, path_ino
                 FROM workspaces WHERE path = ?1 ORDER BY created_at, id LIMIT 1",
                [path],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )?;
            let row = settings_workspace_projection_from_persisted(persisted)?;
            workspace_identity::verify(
                &tx,
                &row.id,
                &row.path,
                row.folder_anchor,
                folder_anchor,
                volume,
                allow_selected_legacy_rebind,
            )?;
            let anchor_claimed: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM workspaces
                 WHERE path_dev = ?1 AND path_ino = ?2 AND id != ?3 LIMIT 1)",
                (&folder_anchor.dev, &folder_anchor.ino, &row.id),
                |row| row.get(0),
            )?;
            anyhow::ensure!(
                !anchor_claimed,
                if row.folder_anchor.is_none() {
                    "workspace_path_anchor_conflict"
                } else {
                    "workspace_folder_anchor_duplicate"
                }
            );
            if row.folder_anchor != Some(folder_anchor) {
                let mut stored = settings_workspace_row_for_update(&tx, &row.id)?
                    .context("settings_workspace_exact_path_missing")?;
                stored.path_dev = Some(folder_anchor.dev);
                stored.path_ino = Some(folder_anchor.ino);
                settings_workspace_update_admission(&tx, &stored)?;
                tx.execute(
                    "UPDATE workspaces SET path_dev = ?2, path_ino = ?3,
                     updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = ?1",
                    (&row.id, folder_anchor.dev, folder_anchor.ino),
                )?;
            }
            workspace_identity::record(&tx, &row.id, path, folder_anchor, volume)?;
            tx.commit()?;
            return Ok(WorkspaceFindOrCreateResult {
                row: SettingsWorkspaceProjectionRow {
                    folder_anchor: Some(folder_anchor),
                    ..row
                },
                created: false,
            });
        }

        let anchor_probe = settings_read_probe(
            &tx,
            "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
             FROM (
                 SELECT length(CAST(id AS BLOB)) + length(CAST(name AS BLOB)) +
                        length(CAST(path AS BLOB)) + length(CAST(created_at AS BLOB)) +
                        length(CAST(COALESCE(path_dev, '') AS BLOB)) +
                        length(CAST(COALESCE(path_ino, '') AS BLOB)) AS row_bytes
                 FROM workspaces WHERE path_dev = ?1 AND path_ino = ?2
                 ORDER BY created_at, id LIMIT 2
             )",
            (folder_anchor.dev, folder_anchor.ino),
            2,
            SETTINGS_SNAPSHOT_BYTES_MAX,
            SETTINGS_ROW_BYTES_MAX,
            "settings_workspace_folder_anchor",
        )?;
        anyhow::ensure!(anchor_probe.count < 2, "workspace_folder_anchor_duplicate");
        if anchor_probe.count == 1 {
            let existing_id: String = tx.query_row(
                "SELECT id FROM workspaces WHERE path_dev = ?1 AND path_ino = ?2
                 ORDER BY created_at, id LIMIT 1",
                (folder_anchor.dev, folder_anchor.ino),
                |row| row.get(0),
            )?;
            let mut stored = settings_workspace_row_for_update(&tx, &existing_id)?
                .context("settings_workspace_folder_anchor_missing")?;
            workspace_identity::verify(
                &tx,
                &existing_id,
                &stored.path,
                Some(folder_anchor),
                folder_anchor,
                volume,
                false,
            )?;
            stored.path = path.to_owned();
            settings_workspace_update_admission(&tx, &stored)?;
            let affected = tx.execute(
                "UPDATE workspaces SET path = ?2,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE id = ?1 AND path_dev = ?3 AND path_ino = ?4",
                (&existing_id, path, folder_anchor.dev, folder_anchor.ino),
            )?;
            anyhow::ensure!(affected == 1, "workspace_folder_anchor_rebind_failed");
            let row = tx.query_row(
                "SELECT id, name, path, created_at, path_dev, path_ino
                 FROM workspaces WHERE id = ?1",
                [&existing_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )?;
            let row = settings_workspace_projection_from_persisted(row)?;
            workspace_identity::record(&tx, &row.id, path, folder_anchor, volume)?;
            tx.commit()?;
            return Ok(WorkspaceFindOrCreateResult {
                row,
                created: false,
            });
        }

        let id = uuid::Uuid::new_v4().to_string();
        settings_workspace_write_admission(
            &tx,
            &id,
            name,
            path,
            Some(folder_anchor.dev),
            Some(folder_anchor.ino),
        )?;
        tx.execute(
            "INSERT INTO workspaces
                (id, name, path, created_at, updated_at, path_dev, path_ino)
             VALUES (?1, ?2, ?3,
                strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                strftime('%Y-%m-%dT%H:%M:%fZ','now'), ?4, ?5)",
            (&id, name, path, folder_anchor.dev, folder_anchor.ino),
        )?;
        workspace_identity::record(&tx, &id, path, folder_anchor, volume)?;
        let inserted_probe = settings_read_probe(
            &tx,
            "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
             FROM (
                 SELECT length(CAST(id AS BLOB)) + length(CAST(name AS BLOB)) +
                        length(CAST(path AS BLOB)) + length(CAST(created_at AS BLOB)) +
                        length(CAST(COALESCE(path_dev, '') AS BLOB)) +
                        length(CAST(COALESCE(path_ino, '') AS BLOB)) AS row_bytes
                 FROM workspaces WHERE id = ?1 LIMIT 2
             )",
            [&id],
            1,
            SETTINGS_SNAPSHOT_BYTES_MAX,
            SETTINGS_ROW_BYTES_MAX,
            "settings_workspace_created",
        )?;
        anyhow::ensure!(
            inserted_probe.count == 1,
            "settings_workspace_created_missing"
        );
        let persisted = tx.query_row(
            "SELECT id, name, path, created_at, path_dev, path_ino
             FROM workspaces WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )?;
        let row = settings_workspace_projection_from_persisted(persisted)?;
        tx.commit()?;
        Ok(WorkspaceFindOrCreateResult { row, created: true })
    }

    /// Atomically accepts a moved path only while the old path and persisted folder identity still
    /// equal the caller's snapshot. The caller-supplied filesystem identity for `new_path` must
    /// prove it is the same folder; otherwise this returns `Stale` without opening a transaction.
    pub fn update_workspace_moved_path_cas(
        &self,
        workspace_id: &str,
        expected_old_path: &str,
        expected_stored_anchor: WorkspaceFolderAnchor,
        new_path: &str,
        new_filesystem_anchor: WorkspaceFolderAnchor,
    ) -> anyhow::Result<WorkspaceMovedPathUpdate> {
        validate_workspace_text_input(workspace_id, "id")?;
        validate_workspace_text_input(expected_old_path, "expected_path")?;
        validate_workspace_text_input(new_path, "new_path")?;
        if new_filesystem_anchor != expected_stored_anchor {
            return Ok(WorkspaceMovedPathUpdate::Stale);
        }

        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let Some(mut row) = settings_workspace_row_for_update(&tx, workspace_id)? else {
            tx.commit()?;
            return Ok(WorkspaceMovedPathUpdate::Stale);
        };
        if row.path != expected_old_path
            || row.path_dev != Some(expected_stored_anchor.dev)
            || row.path_ino != Some(expected_stored_anchor.ino)
        {
            tx.commit()?;
            return Ok(WorkspaceMovedPathUpdate::Stale);
        }
        row.path = new_path.to_owned();
        row.path_dev = Some(new_filesystem_anchor.dev);
        row.path_ino = Some(new_filesystem_anchor.ino);
        settings_workspace_update_admission(&tx, &row)?;
        let affected = tx.execute(
            "UPDATE workspaces
             SET path = ?5, path_dev = ?6, path_ino = ?7,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE id = ?1 AND path = ?2 AND path_dev = ?3 AND path_ino = ?4",
            rusqlite::params![
                workspace_id,
                expected_old_path,
                expected_stored_anchor.dev,
                expected_stored_anchor.ino,
                new_path,
                new_filesystem_anchor.dev,
                new_filesystem_anchor.ino,
            ],
        )?;
        if affected == 0 {
            tx.commit()?;
            return Ok(WorkspaceMovedPathUpdate::Stale);
        }
        anyhow::ensure!(affected == 1, "settings_workspace_moved_path_multiple_rows");
        let probe = settings_read_probe(
            &tx,
            "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
             FROM (
                 SELECT length(CAST(id AS BLOB)) + length(CAST(name AS BLOB)) +
                        length(CAST(path AS BLOB)) + length(CAST(created_at AS BLOB)) +
                        length(CAST(COALESCE(path_dev, '') AS BLOB)) +
                        length(CAST(COALESCE(path_ino, '') AS BLOB)) AS row_bytes
                 FROM workspaces WHERE id = ?1 LIMIT 2
             )",
            [workspace_id],
            1,
            SETTINGS_SNAPSHOT_BYTES_MAX,
            SETTINGS_ROW_BYTES_MAX,
            "settings_workspace_moved_path",
        )?;
        anyhow::ensure!(probe.count == 1, "settings_workspace_moved_path_missing");
        tx.commit()?;
        Ok(WorkspaceMovedPathUpdate::Updated)
    }

    /// 런타임이 없는(또는 warm) 워크스페이스의 활동 화면에 쓸 영속 pane snapshot —
    /// (workspace_id, pane_id, 제목, 세션 cwd). `sessions` 전체는 닫힌 과거 이력도 남으므로,
    /// 현재 복원 레이아웃에 연결된 `mux_panes`만 읽는다. 한 쿼리로 모든 workspace를
    /// 반환해 UI의 N+1을 피한다. cwd는 기본 제목("셸 N")을 프로젝트명으로 바꿔 표시하는
    /// 데 쓴다(활성 워크스페이스의 resolve_session_title과 같은 규칙) — 세션이 없는
    /// pane이면 빈 문자열.
    pub fn list_persisted_activity_panes(&self) -> anyhow::Result<Vec<PersistedActivityPane>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT p.workspace_id, p.id,
                    COALESCE(NULLIF(p.title, ''), NULLIF(s.title, ''), p.id),
                    COALESCE(s.cwd, '')
               FROM mux_panes p
               LEFT JOIN sessions s ON s.id = p.session_id
              ORDER BY p.workspace_id, p.created_at, p.id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(PersistedActivityPane {
                workspace_id: row.get(0)?,
                pane_id: row.get(1)?,
                title: row.get(2)?,
                cwd: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Complete activity-pane snapshot with item, row, and aggregate-byte admission before any
    /// returned String is allocated. The read transaction keeps preflight and materialization on
    /// one SQLite snapshot; `limit + 1` detects truncation instead of returning partial UI state.
    pub fn list_persisted_activity_panes_bounded(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<PersistedActivityPane>> {
        let sql_limit = bounded_limit_plus_one(limit, ACTIVITY_PANE_ROWS_MAX)?;
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
        let probe = bounded_read_preflight(
            &tx,
            ACTIVITY_PANES_BOUNDED_PREFLIGHT,
            rusqlite::params![
                sql_limit,
                BOUNDED_ID_BYTES_MAX as i64,
                BOUNDED_TEXT_BYTES_MAX as i64,
                BOUNDED_ROW_BYTES_MAX as i64,
            ],
            limit,
        )?;
        let mut result = Vec::with_capacity(probe.count);
        {
            let mut stmt = tx
                .prepare(ACTIVITY_PANES_BOUNDED_SELECT)
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            let mut rows = stmt
                .query(rusqlite::params![
                    sql_limit,
                    BOUNDED_ID_BYTES_MAX as i64,
                    BOUNDED_TEXT_BYTES_MAX as i64,
                ])
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            while let Some(row) = rows
                .next()
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?
            {
                let workspace_id = bounded_required_text(row, 0, BOUNDED_ID_BYTES_MAX, true, true)?;
                let pane_id = bounded_required_text(row, 1, BOUNDED_ID_BYTES_MAX, true, true)?;
                let title = bounded_required_text(row, 2, BOUNDED_TEXT_BYTES_MAX, true, false)?;
                let cwd = bounded_required_text(row, 3, BOUNDED_TEXT_BYTES_MAX, false, false)?;
                result.push(PersistedActivityPane {
                    workspace_id: workspace_id.to_owned(),
                    pane_id: pane_id.to_owned(),
                    title: title.to_owned(),
                    cwd: cwd.to_owned(),
                });
            }
        }
        tx.commit()
            .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
        Ok(result)
    }

    /// Read-only history for the Cloud Agents settings screen. A closed pane is absent from
    /// mux_panes, so activity-pane snapshots cannot represent its ended session. This query
    /// never creates an MCP target or restores a session.
    pub fn list_cloud_ended_sessions(&self) -> anyhow::Result<Vec<CloudEndedSession>> {
        Self::read_cloud_ended_sessions(&self.conn)
    }

    /// The settings query runs off the UI thread with a read-only connection; opening this
    /// history view must not rerun migrations or contend for a writer lock.
    pub fn list_cloud_ended_sessions_from_path(
        path: &Path,
    ) -> anyhow::Result<Vec<CloudEndedSession>> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        Self::read_cloud_ended_sessions(&conn)
    }

    fn read_cloud_ended_sessions(conn: &Connection) -> anyhow::Result<Vec<CloudEndedSession>> {
        const MAX_ROWS: usize = 32_768;
        const MAX_BYTES: usize = 16 * 1024 * 1024;
        let mut stmt = conn.prepare_cached(
            "SELECT CASE WHEN length(CAST(sessions.id AS BLOB)) BETWEEN 1 AND 128
                         THEN sessions.id END,
                    CASE WHEN length(CAST(sessions.workspace_id AS BLOB)) BETWEEN 1 AND 128
                         THEN sessions.workspace_id END,
                    substr(workspaces.name, 1, 256), substr(sessions.title, 1, 256)
               FROM sessions JOIN workspaces ON workspaces.id = sessions.workspace_id
              WHERE sessions.status = 'exited'
              ORDER BY sessions.rowid DESC
              LIMIT 32769",
        )?;
        let mut rows = stmt.query([])?;
        let mut result = Vec::new();
        let mut retained = 0usize;
        while let Some(row) = rows.next()? {
            anyhow::ensure!(result.len() < MAX_ROWS, "cloud_ended_sessions_limit");
            let id: String = row.get(0)?;
            let workspace_id: String = row.get(1)?;
            let workspace_name: String = row.get(2)?;
            let title: String = row.get(3)?;
            retained = retained
                .checked_add(id.len() + workspace_id.len() + workspace_name.len() + title.len())
                .ok_or_else(|| anyhow::anyhow!("cloud_ended_sessions_limit"))?;
            anyhow::ensure!(retained <= MAX_BYTES, "cloud_ended_sessions_limit");
            result.push(CloudEndedSession {
                id,
                workspace_id,
                workspace_name,
                title,
            });
        }
        Ok(result)
    }

    /// 환경/API 프로젝트 목록용 집계. workspace별 profile/var와 credential을 UI에서
    /// N+1 조회하지 않도록 한 SQL snapshot으로 반환한다. key_count는 현재 UI 계약대로
    /// 해당 workspace에서 보이는(소속+전역) credential 중 dotenv profile이 참조하는
    /// 자동 생성 credential을 제외한 수다.
    pub fn env_api_project_counts(&self) -> anyhow::Result<Vec<EnvApiProjectCount>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT w.id,
                    (SELECT COUNT(*)
                       FROM env_profiles ep
                       JOIN env_vars ev ON ev.profile_id = ep.id
                      WHERE ep.workspace_id = w.id) AS env_count,
                    (SELECT COUNT(*)
                       FROM credentials c
                      WHERE (c.workspace_id IS NULL OR c.workspace_id = w.id)
                        AND NOT EXISTS (
                            SELECT 1
                              FROM env_vars hidden_ev
                              JOIN env_profiles hidden_ep ON hidden_ep.id = hidden_ev.profile_id
                             WHERE hidden_ep.kind = 'dotenv'
                               AND hidden_ev.credential_id = c.id
                        )) AS key_count
               FROM workspaces w
              ORDER BY w.created_at, w.id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(EnvApiProjectCount {
                workspace_id: row.get(0)?,
                env_count: row.get::<_, i64>(1)?.max(0) as usize,
                key_count: row.get::<_, i64>(2)?.max(0) as usize,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn env_api_project_counts_bounded(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<EnvApiProjectCount>> {
        let sql_limit = bounded_limit_plus_one(limit, ENV_API_PROJECT_ROWS_MAX)?;
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
        let probe = bounded_read_preflight(
            &tx,
            ENV_API_COUNTS_BOUNDED_PREFLIGHT,
            rusqlite::params![
                sql_limit,
                BOUNDED_ID_BYTES_MAX as i64,
                BOUNDED_ROW_BYTES_MAX as i64,
            ],
            limit,
        )?;
        let mut result = Vec::with_capacity(probe.count);
        {
            let mut stmt = tx
                .prepare(ENV_API_COUNTS_BOUNDED_SELECT)
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            let mut rows = stmt
                .query(rusqlite::params![sql_limit, BOUNDED_ID_BYTES_MAX as i64])
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            while let Some(row) = rows
                .next()
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?
            {
                let workspace_id = bounded_required_text(row, 0, BOUNDED_ID_BYTES_MAX, true, true)?;
                let env_count = bounded_integer(row, 1)?;
                let key_count = bounded_integer(row, 2)?;
                result.push(EnvApiProjectCount {
                    workspace_id: workspace_id.to_owned(),
                    env_count: usize::try_from(env_count)
                        .map_err(|_| anyhow::anyhow!(BOUNDED_READ_ROW_INVALID))?,
                    key_count: usize::try_from(key_count)
                        .map_err(|_| anyhow::anyhow!(BOUNDED_READ_ROW_INVALID))?,
                });
            }
        }
        tx.commit()
            .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
        Ok(result)
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
        anyhow::ensure!(
            bounded_id_is_valid(workspace_id)
                && bounded_id_is_valid(pane_id)
                && bounded_id_is_valid(session_id)
                && !kind.is_empty()
                && bounded_text_is_valid(kind, BOUNDED_TEXT_BYTES_MAX),
            BOUNDED_WRITE_INPUT_INVALID
        );
        bounded_input_row_bytes(&[workspace_id, pane_id, kind, session_id])?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(|_| anyhow::anyhow!(BOUNDED_WRITE_FAILED))?;
        let exists = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_sessions
                  WHERE workspace_id = ?1 AND pane_id = ?2)",
                (workspace_id, pane_id),
                |row| row.get::<_, i64>(0),
            )
            .map_err(|_| anyhow::anyhow!(BOUNDED_WRITE_FAILED))?
            != 0;
        let count = tx
            .query_row(
                "SELECT COUNT(*) FROM agent_sessions WHERE workspace_id = ?1",
                [workspace_id],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|_| anyhow::anyhow!(BOUNDED_WRITE_FAILED))?;
        anyhow::ensure!(
            exists || count < AGENT_SESSION_ROWS_MAX as i64,
            AGENT_SESSION_CAPACITY_EXCEEDED
        );
        tx.execute(
            "INSERT INTO agent_sessions
                   (workspace_id, pane_id, kind, session_id, updated_at)
                 VALUES (?1, ?2, ?3, ?4, CAST(strftime('%s','now') AS INTEGER))
                 ON CONFLICT(workspace_id, pane_id) DO UPDATE SET
                   task_prompt = CASE WHEN agent_sessions.kind = excluded.kind
                     AND agent_sessions.session_id = excluded.session_id
                     THEN agent_sessions.task_prompt ELSE NULL END,
                   kind = excluded.kind, session_id = excluded.session_id,
                   updated_at = excluded.updated_at",
            (workspace_id, pane_id, kind, session_id),
        )
        .map_err(|_| anyhow::anyhow!(BOUNDED_WRITE_FAILED))?;
        tx.commit()
            .map_err(|_| anyhow::anyhow!(BOUNDED_WRITE_FAILED))?;
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
            "DELETE FROM agent_needs_input WHERE updated_at < strftime('%s','now') - 604800 AND NOT ((attention_json IS NOT NULL AND (waiting=1 OR turn_done=1)) OR (typeof(idle_since)='integer' AND idle_since>=0 AND working=0 AND waiting=0))",
            [],
        )?;
        self.conn.execute(
            "DELETE FROM agent_statusline WHERE updated_at < strftime('%s','now') - 604800",
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
        let workspace_prefix = bounded_session_key_prefix(session_key)?;
        anyhow::ensure!(
            bounded_id_is_valid(agent_session_id)
                && !kind.is_empty()
                && bounded_text_is_valid(kind, BOUNDED_TEXT_BYTES_MAX)
                && bounded_text_is_valid(transcript_path, BOUNDED_TEXT_BYTES_MAX),
            BOUNDED_WRITE_INPUT_INVALID
        );
        bounded_input_row_bytes(&[session_key, kind, agent_session_id, transcript_path])?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(|_| anyhow::anyhow!(BOUNDED_WRITE_FAILED))?;
        tx.execute(
            "INSERT INTO agent_hook_sessions
                 (session_key, kind, agent_session_id, transcript_path, updated_at)
                 VALUES (?1, ?2, ?3, ?4, CAST(strftime('%s','now') AS INTEGER))
                 ON CONFLICT(session_key) DO UPDATE SET
                   task_prompt = CASE
                     WHEN agent_hook_sessions.kind = excluded.kind
                      AND agent_hook_sessions.agent_session_id = excluded.agent_session_id
                     THEN agent_hook_sessions.task_prompt ELSE NULL END,
                   kind = excluded.kind,
                   agent_session_id = excluded.agent_session_id,
                   transcript_path = excluded.transcript_path,
                   updated_at = excluded.updated_at",
            rusqlite::params![session_key, kind, agent_session_id, transcript_path],
        )
        .map_err(|_| anyhow::anyhow!(BOUNDED_WRITE_FAILED))?;
        evict_hook_state_prefix_overflow(
            &tx,
            HookStateTable::HookSessions,
            workspace_prefix,
            session_key,
        )?;
        evict_hook_state_overflow(&tx, HookStateTable::HookSessions, session_key)?;
        tx.commit()
            .map_err(|_| anyhow::anyhow!(BOUNDED_WRITE_FAILED))?;
        Ok(())
    }

    /// Store only the prompt observed by this pane's Claude hook, never by shared transcript ID.
    pub fn record_hook_task_prompt(
        &self,
        session_key: &str,
        agent_session_id: &str,
        prompt: &str,
    ) -> anyhow::Result<()> {
        bounded_session_key_prefix(session_key)?;
        anyhow::ensure!(
            bounded_id_is_valid(agent_session_id) && !prompt.is_empty(),
            BOUNDED_WRITE_INPUT_INVALID
        );
        // Normalize transport metadata before applying the existing 256-byte title budget.
        // Wrapped internal events must leave the previous pane task intact.
        let Some(prompt) = task_prompt_text(prompt) else {
            return Ok(());
        };
        anyhow::ensure!(
            bounded_text_is_valid(prompt.as_ref(), 256),
            BOUNDED_WRITE_INPUT_INVALID
        );
        self.conn.execute(
            "UPDATE agent_hook_sessions SET task_prompt = ?3
             WHERE session_key = ?1 AND kind = 'claude' AND agent_session_id = ?2",
            rusqlite::params![session_key, agent_session_id, prompt.as_ref()],
        )?;
        Ok(())
    }

    /// hook이 보고한 바인딩 목록 (최근 24h — 죽은 세션 행이 영원히 남지 않게).
    pub fn list_hook_sessions(&self) -> anyhow::Result<Vec<HookSessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_key, kind, agent_session_id, transcript_path, task_prompt FROM agent_hook_sessions
             WHERE updated_at > strftime('%s','now') - 86400",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(HookSessionRow {
                    session_key: r.get(0)?,
                    kind: r.get(1)?,
                    agent_session_id: r.get(2)?,
                    transcript_path: r.get(3)?,
                    task_prompt: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn list_hook_sessions_for_prefix_bounded(
        &self,
        prefix: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<HookSessionRow>> {
        anyhow::ensure!(bounded_id_is_valid(prefix), BOUNDED_READ_INPUT_INVALID);
        let sql_limit = bounded_limit_plus_one(limit, HOOK_PREFIX_ROWS_MAX)?;
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
        let snapshot_epoch = bounded_snapshot_epoch(&tx)?;
        let probe = bounded_read_preflight(
            &tx,
            HOOK_SESSIONS_PREFIX_PREFLIGHT,
            rusqlite::params![
                prefix,
                sql_limit,
                BOUNDED_ID_BYTES_MAX as i64,
                BOUNDED_TEXT_BYTES_MAX as i64,
                BOUNDED_ROW_BYTES_MAX as i64,
                snapshot_epoch,
            ],
            limit,
        )?;
        let mut result = Vec::with_capacity(probe.count);
        {
            let mut stmt = tx
                .prepare(HOOK_SESSIONS_PREFIX_SELECT)
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            let mut rows = stmt
                .query(rusqlite::params![
                    prefix,
                    sql_limit,
                    BOUNDED_ID_BYTES_MAX as i64,
                    snapshot_epoch,
                ])
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            while let Some(row) = rows
                .next()
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?
            {
                result.push(HookSessionRow {
                    session_key: bounded_required_text(row, 0, BOUNDED_ID_BYTES_MAX, true, true)?
                        .to_owned(),
                    kind: bounded_required_text(row, 1, BOUNDED_TEXT_BYTES_MAX, true, false)?
                        .to_owned(),
                    agent_session_id: bounded_required_text(
                        row,
                        2,
                        BOUNDED_ID_BYTES_MAX,
                        true,
                        true,
                    )?
                    .to_owned(),
                    transcript_path: bounded_required_text(
                        row,
                        3,
                        BOUNDED_TEXT_BYTES_MAX,
                        false,
                        false,
                    )?
                    .to_owned(),
                    task_prompt: bounded_optional_text(row, 4, 256)?.map(str::to_owned),
                });
            }
        }
        tx.commit()
            .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
        Ok(result)
    }

    /// needsInput 기록. `message`는 hook이 보고한 대기 사유 문구(claude Notification의
    /// payload.message) — 없으면 None. clear(waiting=false) 시에는 문구도 함께 지운다
    /// (해소된 질문이 다음 대기에 되살아나면 안 된다).
    pub fn set_agent_needs_input(
        &self,
        session_key: &str,
        waiting: bool,
        message: Option<&str>,
    ) -> anyhow::Result<()> {
        let workspace_prefix = bounded_session_key_prefix(session_key)?;
        anyhow::ensure!(
            message.is_none_or(|value| { bounded_text_is_valid(value, BOUNDED_MESSAGE_BYTES_MAX) }),
            BOUNDED_WRITE_INPUT_INVALID
        );
        bounded_input_row_bytes(&[session_key, message.unwrap_or_default()])?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(|_| anyhow::anyhow!(BOUNDED_WRITE_FAILED))?;
        // working = !waiting (v32, cmux식 턴 경계): clear 이벤트(UserPromptSubmit=턴 시작,
        // PreToolUse=툴 호출 직전)는 에이전트가 턴 안에서 실제로 작업 중이라는 뜻이라
        // working=1을 함께 기록한다. needs-input(승인 대기)은 작업이 막힌 상태라 0.
        // Stop(set_agent_turn_done)은 working 컬럼을 나열하지 않아 DEFAULT 0으로 리셋된다.
        tx.execute(
            "INSERT OR REPLACE INTO agent_needs_input
                     (session_key, waiting, working, updated_at, message)
                 VALUES (?1, ?2, ?3, CAST(strftime('%s','now') AS INTEGER), ?4)",
            (
                session_key,
                waiting as i64,
                !waiting as i64,
                message.filter(|_| waiting),
            ),
        )
        .map_err(|_| anyhow::anyhow!(BOUNDED_WRITE_FAILED))?;
        evict_hook_state_prefix_overflow(
            &tx,
            HookStateTable::NeedsInput,
            workspace_prefix,
            session_key,
        )?;
        evict_hook_state_overflow(&tx, HookStateTable::NeedsInput, session_key)?;
        tx.commit()
            .map_err(|_| anyhow::anyhow!(BOUNDED_WRITE_FAILED))?;
        Ok(())
    }

    /// hook 기반 "작업 중"(v32) 세션 키. stale(2분 초과)은 제외 — Stop 유실(Ctrl-C 등)
    /// 시 자기치유(병렬 리뷰 H1: 창이 길면 IdleHeuristic을 눌러 고착 표시). 실제 작업
    /// 중엔 PreToolUse가 툴 호출마다 updated_at을 갱신한다.
    pub fn list_working_sessions(&self) -> anyhow::Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_key FROM agent_needs_input
             WHERE working = 1
               AND updated_at > CAST(strftime('%s','now') AS INTEGER) - 120",
        )?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// 현재 입력 대기(waiting) 중인 세션 키 + hook이 보고한 사유 문구. stale(24시간 초과)은
    /// 제외한다 — 대기는 하트비트가 없어 창을 짧게 잡으면 실제로 기다리는 중인 에이전트의
    /// 배지가 사라진다(WAITING_SESSIONS_PREFLIGHT 위 주석).
    pub fn list_waiting_sessions(&self) -> anyhow::Result<Vec<(String, Option<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_key, message FROM agent_needs_input
             WHERE waiting = 1
               AND (attention_json IS NOT NULL OR updated_at > CAST(strftime('%s','now') AS INTEGER) - 86400)",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn list_waiting_sessions_bounded(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<(String, Option<String>)>> {
        let sql_limit = bounded_limit_plus_one(limit, WAITING_SESSION_ROWS_MAX)?;
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
        let snapshot_epoch = bounded_snapshot_epoch(&tx)?;
        let probe = bounded_read_preflight(
            &tx,
            WAITING_SESSIONS_PREFLIGHT,
            rusqlite::params![
                sql_limit,
                BOUNDED_ID_BYTES_MAX as i64,
                BOUNDED_MESSAGE_BYTES_MAX as i64,
                BOUNDED_ROW_BYTES_MAX as i64,
                snapshot_epoch,
            ],
            limit,
        )?;
        let mut result = Vec::with_capacity(probe.count);
        {
            let mut stmt = tx
                .prepare(WAITING_SESSIONS_SELECT)
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            let mut rows = stmt
                .query(rusqlite::params![
                    sql_limit,
                    BOUNDED_ID_BYTES_MAX as i64,
                    snapshot_epoch,
                ])
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            while let Some(row) = rows
                .next()
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?
            {
                result.push((
                    bounded_required_text(row, 0, BOUNDED_ID_BYTES_MAX, true, true)?.to_owned(),
                    bounded_optional_text(row, 1, BOUNDED_MESSAGE_BYTES_MAX)?.map(str::to_owned),
                ));
            }
        }
        tx.commit()
            .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
        Ok(result)
    }

    /// 턴 완료(Stop hook) 기록 — waiting은 0으로 함께 리셋한다(턴이 끝났으므로).
    pub fn set_agent_turn_done(&self, session_key: &str) -> anyhow::Result<()> {
        let workspace_prefix = bounded_session_key_prefix(session_key)?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(|_| anyhow::anyhow!(BOUNDED_WRITE_FAILED))?;
        let idle_generation = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_micros().min(i64::MAX as u128) as i64)
            .unwrap_or(0);
        tx.execute(
            "INSERT OR REPLACE INTO agent_needs_input
                     (session_key, waiting, turn_done, updated_at, idle_since, idle_generation)
                 VALUES (?1, 0, 1, ?2/1000000, ?2/1000000, ?2)",
            rusqlite::params![session_key, idle_generation],
        )
        .map_err(|_| anyhow::anyhow!(BOUNDED_WRITE_FAILED))?;
        evict_hook_state_prefix_overflow(
            &tx,
            HookStateTable::NeedsInput,
            workspace_prefix,
            session_key,
        )?;
        evict_hook_state_overflow(&tx, HookStateTable::NeedsInput, session_key)?;
        tx.commit()
            .map_err(|_| anyhow::anyhow!(BOUNDED_WRITE_FAILED))?;
        Ok(())
    }

    /// 턴 완료 소비(사용자가 해당 pane을 확인) — turn_done만 내린다(waiting 불변).
    /// `seen_at`(내가 읽은 updated_at) 이후에 도착한 새 완료 이벤트는 지우지 않는다 —
    /// 읽기~clear 사이 새 Stop이 오면 그 알림까지 유실되던 레이스 방지(codex 리뷰).
    pub fn clear_agent_turn_done(&self, session_key: &str, seen_at: i64) -> anyhow::Result<()> {
        let workspace_prefix = bounded_session_key_prefix(session_key)?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(|_| anyhow::anyhow!(BOUNDED_WRITE_FAILED))?;
        tx.execute(
            "UPDATE agent_needs_input SET turn_done = 0
                 WHERE session_key = ?1 AND (CASE WHEN attention_json IS NULL THEN updated_at ELSE attention_revision END) <= ?2",
            (session_key, seen_at),
        )
        .map_err(|_| anyhow::anyhow!(BOUNDED_WRITE_FAILED))?;
        evict_hook_state_prefix_overflow(
            &tx,
            HookStateTable::NeedsInput,
            workspace_prefix,
            session_key,
        )?;
        evict_hook_state_overflow(&tx, HookStateTable::NeedsInput, session_key)?;
        tx.commit()
            .map_err(|_| anyhow::anyhow!(BOUNDED_WRITE_FAILED))?;
        Ok(())
    }

    /// claude statusLine 표시 정보 upsert(effort/model/남은 context%). 값 변경 시에만
    /// 호출되도록 프록시가 스로틀한다.
    pub fn upsert_statusline(
        &self,
        session_key: &str,
        effort: Option<&str>,
        model: Option<&str>,
        context_pct: Option<i64>,
    ) -> anyhow::Result<()> {
        let workspace_prefix = bounded_session_key_prefix(session_key)?;
        anyhow::ensure!(
            effort.is_none_or(|value| { bounded_text_is_valid(value, BOUNDED_TEXT_BYTES_MAX) })
                && model
                    .is_none_or(|value| { bounded_text_is_valid(value, BOUNDED_TEXT_BYTES_MAX) }),
            BOUNDED_WRITE_INPUT_INVALID
        );
        bounded_input_row_bytes(&[
            session_key,
            effort.unwrap_or_default(),
            model.unwrap_or_default(),
        ])?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(|_| anyhow::anyhow!(BOUNDED_WRITE_FAILED))?;
        tx.execute(
            "INSERT OR REPLACE INTO agent_statusline
                     (session_key, effort, model, context_pct, updated_at)
                 VALUES (?1, ?2, ?3, ?4, CAST(strftime('%s','now') AS INTEGER))",
            (session_key, effort, model, context_pct),
        )
        .map_err(|_| anyhow::anyhow!(BOUNDED_WRITE_FAILED))?;
        evict_hook_state_prefix_overflow(
            &tx,
            HookStateTable::Statusline,
            workspace_prefix,
            session_key,
        )?;
        evict_hook_state_overflow(&tx, HookStateTable::Statusline, session_key)?;
        tx.commit()
            .map_err(|_| anyhow::anyhow!(BOUNDED_WRITE_FAILED))?;
        Ok(())
    }

    /// 최근(1시간 이내) statusLine 정보 목록 — (key, effort, model, context_pct).
    pub fn list_statuslines(&self) -> anyhow::Result<Vec<StatuslineRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_key, effort, model, context_pct FROM agent_statusline
             WHERE updated_at > CAST(strftime('%s','now') AS INTEGER) - 3600",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(StatuslineRow {
                session_key: row.get(0)?,
                effort: row.get(1)?,
                model: row.get(2)?,
                context_pct: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn list_statuslines_for_prefix_bounded(
        &self,
        prefix: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<StatuslineRow>> {
        anyhow::ensure!(bounded_id_is_valid(prefix), BOUNDED_READ_INPUT_INVALID);
        let sql_limit = bounded_limit_plus_one(limit, HOOK_PREFIX_ROWS_MAX)?;
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
        let snapshot_epoch = bounded_snapshot_epoch(&tx)?;
        let probe = bounded_read_preflight(
            &tx,
            STATUSLINES_PREFIX_PREFLIGHT,
            rusqlite::params![
                prefix,
                sql_limit,
                BOUNDED_ID_BYTES_MAX as i64,
                BOUNDED_TEXT_BYTES_MAX as i64,
                BOUNDED_ROW_BYTES_MAX as i64,
                snapshot_epoch,
            ],
            limit,
        )?;
        let mut result = Vec::with_capacity(probe.count);
        {
            let mut stmt = tx
                .prepare(STATUSLINES_PREFIX_SELECT)
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            let mut rows = stmt
                .query(rusqlite::params![
                    prefix,
                    sql_limit,
                    BOUNDED_ID_BYTES_MAX as i64,
                    snapshot_epoch,
                ])
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            while let Some(row) = rows
                .next()
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?
            {
                result.push(StatuslineRow {
                    session_key: bounded_required_text(row, 0, BOUNDED_ID_BYTES_MAX, true, true)?
                        .to_owned(),
                    effort: bounded_optional_text(row, 1, BOUNDED_TEXT_BYTES_MAX)?
                        .map(str::to_owned),
                    model: bounded_optional_text(row, 2, BOUNDED_TEXT_BYTES_MAX)?
                        .map(str::to_owned),
                    context_pct: bounded_optional_integer(row, 3)?,
                });
            }
        }
        tx.commit()
            .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
        Ok(result)
    }

    /// 턴 완료(미확인) 세션 (key, updated_at) 목록. waiting과 같은 24시간 stale 컷오프.
    /// 두 번째 값은 조건부 clear의 세대 토큰이며 표시용 시각이 아니다.
    pub fn list_turn_done_sessions(&self) -> anyhow::Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_key, CASE WHEN attention_json IS NULL THEN updated_at ELSE attention_revision END FROM agent_needs_input
             WHERE turn_done = 1
               AND (attention_json IS NOT NULL OR updated_at > CAST(strftime('%s','now') AS INTEGER) - 86400)",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn list_turn_done_sessions_for_prefix_bounded(
        &self,
        prefix: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<(String, i64)>> {
        anyhow::ensure!(bounded_id_is_valid(prefix), BOUNDED_READ_INPUT_INVALID);
        let sql_limit = bounded_limit_plus_one(limit, HOOK_PREFIX_ROWS_MAX)?;
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
        let snapshot_epoch = bounded_snapshot_epoch(&tx)?;
        let probe = bounded_read_preflight(
            &tx,
            TURN_DONE_PREFIX_PREFLIGHT,
            rusqlite::params![
                prefix,
                sql_limit,
                BOUNDED_ID_BYTES_MAX as i64,
                BOUNDED_ROW_BYTES_MAX as i64,
                snapshot_epoch,
            ],
            limit,
        )?;
        let mut result = Vec::with_capacity(probe.count);
        {
            let mut stmt = tx
                .prepare(TURN_DONE_PREFIX_SELECT)
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            let mut rows = stmt
                .query(rusqlite::params![
                    prefix,
                    sql_limit,
                    BOUNDED_ID_BYTES_MAX as i64,
                    snapshot_epoch,
                ])
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            while let Some(row) = rows
                .next()
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?
            {
                result.push((
                    bounded_required_text(row, 0, BOUNDED_ID_BYTES_MAX, true, true)?.to_owned(),
                    bounded_integer(row, 1)?,
                ));
            }
        }
        tx.commit()
            .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
        Ok(result)
    }

    /// 워크스페이스의 저장된 에이전트 세션 (복원 시 resume 대상).
    pub fn list_agent_sessions(&self, workspace_id: &str) -> anyhow::Result<Vec<AgentSessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT pane_id, kind, session_id, task_prompt FROM agent_sessions WHERE workspace_id = ?1",
        )?;
        let rows = stmt.query_map([workspace_id], |row| {
            Ok(AgentSessionRow {
                pane_id: row.get(0)?,
                kind: row.get(1)?,
                session_id: row.get(2)?,
                task_prompt: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn list_agent_sessions_bounded(
        &self,
        workspace_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<AgentSessionRow>> {
        anyhow::ensure!(
            bounded_id_is_valid(workspace_id),
            BOUNDED_READ_INPUT_INVALID
        );
        let sql_limit = bounded_limit_plus_one(limit, AGENT_SESSION_ROWS_MAX)?;
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
        let probe = bounded_read_preflight(
            &tx,
            AGENT_SESSIONS_BOUNDED_PREFLIGHT,
            rusqlite::params![
                workspace_id,
                sql_limit,
                BOUNDED_ID_BYTES_MAX as i64,
                BOUNDED_TEXT_BYTES_MAX as i64,
                BOUNDED_ROW_BYTES_MAX as i64,
            ],
            limit,
        )?;
        let mut result = Vec::with_capacity(probe.count);
        {
            let mut stmt = tx
                .prepare(AGENT_SESSIONS_BOUNDED_SELECT)
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            let mut rows = stmt
                .query(rusqlite::params![
                    workspace_id,
                    sql_limit,
                    BOUNDED_ID_BYTES_MAX as i64,
                ])
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            while let Some(row) = rows
                .next()
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?
            {
                result.push(AgentSessionRow {
                    pane_id: bounded_required_text(row, 0, BOUNDED_ID_BYTES_MAX, true, true)?
                        .to_owned(),
                    kind: bounded_required_text(row, 1, BOUNDED_TEXT_BYTES_MAX, true, false)?
                        .to_owned(),
                    session_id: bounded_required_text(row, 2, BOUNDED_ID_BYTES_MAX, true, true)?
                        .to_owned(),
                    task_prompt: bounded_optional_text(row, 3, 256)?.map(str::to_owned),
                });
            }
        }
        tx.commit()
            .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
        Ok(result)
    }

    /// Reads one workspace's stable newest-first work-turn catalog under explicit row/byte caps.
    pub fn list_agent_work_history(
        &self,
        query: &AgentWorkHistoryQuery,
    ) -> anyhow::Result<Vec<AgentWorkTurnRow>> {
        validate_agent_work_history_query(query)?;
        if query.limit == 0 {
            return Ok(Vec::new());
        }
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_QUERY_FAILED))?;
        let (probe, sql_limit) = agent_work_history_probe(&tx, query)?;
        let result = read_agent_work_history(&tx, query, probe, sql_limit)?;
        let retained_bytes = agent_work_history_retained_bytes(&result)
            .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_ROW_INVALID))?;
        anyhow::ensure!(
            retained_bytes <= query.snapshot_bytes_max,
            AGENT_WORK_HISTORY_ROW_INVALID
        );
        tx.commit()
            .map_err(|_| anyhow::anyhow!(AGENT_WORK_HISTORY_QUERY_FAILED))?;
        Ok(result)
    }

    /// Applies one bounded AgentStateWorker job and returns the requested post-mutation projection
    /// from the same IMMEDIATE SQLite transaction. Row, item, and logical-byte validation happens
    /// before any output String/Vec materialization; omitted sections perform neither step. Actual
    /// backing capacities are measured before commit. If either stage or commit fails, all job
    /// mutations roll back.
    pub fn apply_agent_state_job(&self, job: &AgentStateJob) -> anyhow::Result<AgentStateSnapshot> {
        let workspace_prefix = validate_agent_state_job(job)?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(|_| anyhow::anyhow!(AGENT_STATE_PERSIST_FAILED))?;
        validate_agent_state_structured_existing_scope(&tx, job)?;

        // Exact delayed deletes must never erase a newer identity for the same pane.
        for identity in &job.stale_binding_deletes {
            tx.execute(
                "DELETE FROM agent_sessions
                  WHERE workspace_id = ?1 AND pane_id = ?2 AND kind = ?3 AND session_id = ?4",
                rusqlite::params![
                    job.workspace_id,
                    identity.pane_id,
                    identity.kind,
                    identity.session_id,
                ],
            )
            .map_err(|_| anyhow::anyhow!(AGENT_STATE_PERSIST_FAILED))?;
        }

        // Acknowledgements are generation-aware: a newer hook update remains visible.
        for clear in &job.turn_done_clears {
            tx.execute(
                "UPDATE agent_needs_input SET turn_done = 0
                  WHERE session_key = ?1 AND (CASE WHEN attention_json IS NULL THEN updated_at ELSE attention_revision END) <= ?2",
                rusqlite::params![clear.session_key, clear.seen_at],
            )
            .map_err(|_| anyhow::anyhow!(AGENT_STATE_PERSIST_FAILED))?;
        }

        for mutation in &job.structured_mutations {
            match mutation {
                StructuredThreadMutation::Upsert(row) => {
                    tx.execute(
                        "INSERT INTO structured_threads
                            (local_session_id, workspace_id, thread_id, title, cwd, model,
                             favorite, archived, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                                 CAST(strftime('%s','now') AS INTEGER),
                                 CAST(strftime('%s','now') AS INTEGER))
                         ON CONFLICT(local_session_id) DO UPDATE SET
                            workspace_id = excluded.workspace_id,
                            thread_id = excluded.thread_id,
                            title = excluded.title,
                            cwd = excluded.cwd,
                            model = excluded.model,
                            favorite = excluded.favorite,
                            archived = excluded.archived,
                            updated_at = CAST(strftime('%s','now') AS INTEGER)",
                        rusqlite::params![
                            row.local_session_id,
                            row.workspace_id,
                            row.thread_id,
                            row.title,
                            row.cwd,
                            row.model,
                            row.favorite as i64,
                            row.archived as i64,
                        ],
                    )
                    .map_err(|_| anyhow::anyhow!(AGENT_STATE_PERSIST_FAILED))?;
                }
                StructuredThreadMutation::SetArchived {
                    local_session_id,
                    archived,
                } => {
                    tx.execute(
                        "UPDATE structured_threads
                            SET archived = ?2,
                                updated_at = CAST(strftime('%s','now') AS INTEGER)
                          WHERE local_session_id = ?1",
                        rusqlite::params![local_session_id, *archived as i64],
                    )
                    .map_err(|_| anyhow::anyhow!(AGENT_STATE_PERSIST_FAILED))?;
                }
                StructuredThreadMutation::Delete { local_session_id } => {
                    tx.execute(
                        "DELETE FROM structured_threads WHERE local_session_id = ?1",
                        [local_session_id],
                    )
                    .map_err(|_| anyhow::anyhow!(AGENT_STATE_PERSIST_FAILED))?;
                }
            }
        }

        for mutation in &job.work_turn_mutations {
            let AgentWorkHistoryMutation::Upsert(row) = mutation;
            let source_offset = i64::try_from(row.source_offset)
                .map_err(|_| anyhow::anyhow!(AGENT_STATE_INPUT_INVALID))?;
            // 상한 초과·NUL 포함 messages_json은 행을 거부하지 않고 이 컬럼만 NULL로 쓴다
            // (fail-soft, 스펙 §3-2).
            let messages_json =
                agent_work_turn_messages_json_effective(row.messages_json.as_deref());
            tx.execute(
                "INSERT INTO agent_work_turns
                    (workspace_id, pane_id, kind, agent_session_id, turn_key, source_offset,
                     instruction, agent_summary, model, effort, cwd, branch, git_change_count,
                     state, occurred_at, updated_at, messages_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                         ?15, ?16, ?17)
                 ON CONFLICT(workspace_id, kind, agent_session_id, turn_key) DO UPDATE SET
                    pane_id = excluded.pane_id,
                    source_offset = excluded.source_offset,
                    instruction = excluded.instruction,
                    agent_summary = excluded.agent_summary,
                    model = excluded.model,
                    effort = excluded.effort,
                    cwd = excluded.cwd,
                    branch = excluded.branch,
                    git_change_count = excluded.git_change_count,
                    state = excluded.state,
                    occurred_at = excluded.occurred_at,
                    updated_at = excluded.updated_at,
                    messages_json = COALESCE(excluded.messages_json, agent_work_turns.messages_json)
                 WHERE agent_work_turns.updated_at <= excluded.updated_at",
                rusqlite::params![
                    row.workspace_id,
                    row.pane_id,
                    row.kind,
                    row.agent_session_id,
                    row.turn_key,
                    source_offset,
                    row.instruction,
                    row.agent_summary,
                    row.model,
                    row.effort,
                    row.cwd,
                    row.branch,
                    row.git_change_count.map(i64::from),
                    row.state.as_str(),
                    row.occurred_at,
                    row.updated_at,
                    messages_json,
                ],
            )
            .map_err(|_| anyhow::anyhow!(AGENT_STATE_PERSIST_FAILED))?;
        }
        if !job.work_turn_mutations.is_empty() {
            tx.execute(
                "DELETE FROM agent_work_turns
                  WHERE workspace_id = ?1
                    AND rowid IN (
                        SELECT rowid FROM agent_work_turns
                         WHERE workspace_id = ?1
                         ORDER BY updated_at DESC, source_offset DESC,
                                  substr(CAST(kind AS BLOB), 1, ?3),
                                  substr(CAST(agent_session_id AS BLOB), 1, ?4),
                                  substr(CAST(turn_key AS BLOB), 1, ?4), rowid
                         LIMIT -1 OFFSET ?2
                    )",
                rusqlite::params![
                    job.workspace_id,
                    AGENT_WORK_TURNS_PER_WORKSPACE_MAX as i64,
                    AGENT_WORK_TURN_PROVIDER_BYTES_MAX as i64,
                    AGENT_WORK_TURN_ID_BYTES_MAX as i64,
                ],
            )
            .map_err(|_| anyhow::anyhow!(AGENT_STATE_PERSIST_FAILED))?;
        }

        if let Some(reconcile) = &job.binding_reconcile {
            let live = reconcile
                .live_pane_ids
                .iter()
                .map(String::as_str)
                .collect::<std::collections::HashSet<_>>();
            let sql_limit =
                bounded_limit_plus_one(AGENT_STATE_BINDING_ROWS_MAX, AGENT_SESSION_ROWS_MAX)
                    .map_err(|_| anyhow::anyhow!(AGENT_STATE_INPUT_INVALID))?;
            let probe = bounded_read_preflight(
                &tx,
                AGENT_SESSIONS_BOUNDED_PREFLIGHT,
                rusqlite::params![
                    job.workspace_id,
                    sql_limit,
                    BOUNDED_ID_BYTES_MAX as i64,
                    BOUNDED_TEXT_BYTES_MAX as i64,
                    BOUNDED_ROW_BYTES_MAX as i64,
                ],
                AGENT_STATE_BINDING_ROWS_MAX,
            )?;
            let mut existing = Vec::with_capacity(probe.count);
            {
                let mut stmt = tx
                    .prepare(AGENT_SESSIONS_BOUNDED_SELECT)
                    .map_err(|_| anyhow::anyhow!(AGENT_STATE_PERSIST_FAILED))?;
                let mut rows = stmt
                    .query(rusqlite::params![
                        job.workspace_id,
                        sql_limit,
                        BOUNDED_ID_BYTES_MAX as i64,
                    ])
                    .map_err(|_| anyhow::anyhow!(AGENT_STATE_PERSIST_FAILED))?;
                while let Some(row) = rows
                    .next()
                    .map_err(|_| anyhow::anyhow!(AGENT_STATE_PERSIST_FAILED))?
                {
                    existing.push(AgentSessionRow {
                        pane_id: bounded_required_text(row, 0, BOUNDED_ID_BYTES_MAX, true, true)?
                            .to_owned(),
                        kind: bounded_required_text(row, 1, BOUNDED_TEXT_BYTES_MAX, true, false)?
                            .to_owned(),
                        session_id: bounded_required_text(
                            row,
                            2,
                            BOUNDED_ID_BYTES_MAX,
                            true,
                            true,
                        )?
                        .to_owned(),
                        task_prompt: bounded_optional_text(row, 3, 256)?.map(str::to_owned),
                    });
                }
            }
            for row in existing
                .iter()
                .filter(|row| !live.contains(row.pane_id.as_str()))
            {
                tx.execute(
                    "DELETE FROM agent_sessions
                      WHERE workspace_id = ?1 AND pane_id = ?2 AND kind = ?3 AND session_id = ?4",
                    rusqlite::params![job.workspace_id, row.pane_id, row.kind, row.session_id,],
                )
                .map_err(|_| anyhow::anyhow!(AGENT_STATE_PERSIST_FAILED))?;
            }
            for row in &reconcile.desired_bindings {
                tx.execute(
                    "INSERT INTO agent_sessions
                           (workspace_id, pane_id, kind, session_id, task_prompt, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, CAST(strftime('%s','now') AS INTEGER))
                         ON CONFLICT(workspace_id, pane_id) DO UPDATE SET
                           task_prompt = CASE WHEN agent_sessions.kind = excluded.kind
                             AND agent_sessions.session_id = excluded.session_id
                             THEN COALESCE(excluded.task_prompt, agent_sessions.task_prompt)
                             ELSE excluded.task_prompt END,
                           kind = excluded.kind,
                           session_id = excluded.session_id,
                           updated_at = excluded.updated_at
                         WHERE agent_sessions.kind != excluded.kind
                            OR agent_sessions.session_id != excluded.session_id
                            OR (excluded.task_prompt IS NOT NULL
                                AND agent_sessions.task_prompt IS NOT excluded.task_prompt)",
                    rusqlite::params![
                        job.workspace_id,
                        row.pane_id,
                        row.kind,
                        row.session_id,
                        row.task_prompt
                    ],
                )
                .map_err(|_| anyhow::anyhow!(AGENT_STATE_PERSIST_FAILED))?;
            }
        }

        let snapshot_epoch = if job.include_hook_status || job.include_attention {
            Some(bounded_snapshot_epoch(&tx)?)
        } else {
            None
        };

        // Preflight only requested sections before materializing the first output allocation.
        // Every omitted section remains `None`: no query, SQL parameter Vec, or output buffer.
        let hook_status_probes = if job.include_hook_status {
            let snapshot_epoch =
                snapshot_epoch.ok_or_else(|| anyhow::anyhow!(AGENT_STATE_SNAPSHOT_INVALID))?;
            let sql_limit = bounded_limit_plus_one(HOOK_PREFIX_ROWS_MAX, HOOK_PREFIX_ROWS_MAX)?;
            let hook = bounded_read_preflight(
                &tx,
                HOOK_SESSIONS_PREFIX_PREFLIGHT,
                rusqlite::params![
                    workspace_prefix,
                    sql_limit,
                    BOUNDED_ID_BYTES_MAX as i64,
                    BOUNDED_TEXT_BYTES_MAX as i64,
                    BOUNDED_ROW_BYTES_MAX as i64,
                    snapshot_epoch,
                ],
                HOOK_PREFIX_ROWS_MAX,
            )?;
            let status = bounded_read_preflight(
                &tx,
                STATUSLINES_PREFIX_PREFLIGHT,
                rusqlite::params![
                    workspace_prefix,
                    sql_limit,
                    BOUNDED_ID_BYTES_MAX as i64,
                    BOUNDED_TEXT_BYTES_MAX as i64,
                    BOUNDED_ROW_BYTES_MAX as i64,
                    snapshot_epoch,
                ],
                HOOK_PREFIX_ROWS_MAX,
            )?;
            Some((hook, status, sql_limit, snapshot_epoch))
        } else {
            None
        };
        let attention_probes = if job.include_attention {
            let snapshot_epoch =
                snapshot_epoch.ok_or_else(|| anyhow::anyhow!(AGENT_STATE_SNAPSHOT_INVALID))?;
            let waiting_sql_limit =
                bounded_limit_plus_one(WAITING_SESSION_ROWS_MAX, WAITING_SESSION_ROWS_MAX)?;
            let waiting = bounded_read_preflight(
                &tx,
                WAITING_SESSIONS_PREFLIGHT,
                rusqlite::params![
                    waiting_sql_limit,
                    BOUNDED_ID_BYTES_MAX as i64,
                    BOUNDED_MESSAGE_BYTES_MAX as i64,
                    BOUNDED_ROW_BYTES_MAX as i64,
                    snapshot_epoch,
                ],
                WAITING_SESSION_ROWS_MAX,
            )?;
            // turn_done 전역화(warm turn_done 격차, 감사 발견) — waiting과 동일한 전역
            // 상한/스코프를 쓴다(prefix 없음).
            let turn = bounded_read_preflight(
                &tx,
                TURN_DONE_SESSIONS_PREFLIGHT,
                rusqlite::params![
                    waiting_sql_limit,
                    BOUNDED_ID_BYTES_MAX as i64,
                    BOUNDED_ROW_BYTES_MAX as i64,
                    snapshot_epoch,
                ],
                WAITING_SESSION_ROWS_MAX,
            )?;
            // hook working(v32) — waiting과 동일한 전역 상한/스코프를 쓴다.
            let working = bounded_read_preflight(
                &tx,
                WORKING_SESSIONS_PREFLIGHT,
                rusqlite::params![
                    waiting_sql_limit,
                    BOUNDED_ID_BYTES_MAX as i64,
                    BOUNDED_ROW_BYTES_MAX as i64,
                    snapshot_epoch,
                ],
                WAITING_SESSION_ROWS_MAX,
            )?;
            Some((waiting, turn, working, waiting_sql_limit, snapshot_epoch))
        } else {
            None
        };
        let idle_probe = if let Some((_, _, _, limit, epoch)) = &attention_probes {
            Some((
                bounded_read_preflight(
                    &tx,
                    IDLE_SESSIONS_PREFLIGHT,
                    rusqlite::params![
                        limit,
                        BOUNDED_ID_BYTES_MAX as i64,
                        BOUNDED_ROW_BYTES_MAX as i64
                    ],
                    WAITING_SESSION_ROWS_MAX,
                )?,
                *limit,
                *epoch,
            ))
        } else {
            None
        };
        let agent_probe = if job.include_agent_sessions {
            let sql_limit = bounded_limit_plus_one(AGENT_SESSION_ROWS_MAX, AGENT_SESSION_ROWS_MAX)?;
            Some((
                bounded_read_preflight(
                    &tx,
                    AGENT_SESSIONS_BOUNDED_PREFLIGHT,
                    rusqlite::params![
                        job.workspace_id,
                        sql_limit,
                        BOUNDED_ID_BYTES_MAX as i64,
                        BOUNDED_TEXT_BYTES_MAX as i64,
                        BOUNDED_ROW_BYTES_MAX as i64,
                    ],
                    AGENT_SESSION_ROWS_MAX,
                )?,
                sql_limit,
            ))
        } else {
            None
        };
        let global_agent_probe = if job.include_global_agent_sessions {
            let sql_limit = bounded_limit_plus_one(
                AGENT_SESSIONS_GLOBAL_ROWS_MAX,
                AGENT_SESSIONS_GLOBAL_ROWS_MAX,
            )?;
            Some((
                bounded_read_preflight(
                    &tx,
                    AGENT_SESSIONS_GLOBAL_BOUNDED_PREFLIGHT,
                    rusqlite::params![
                        sql_limit,
                        BOUNDED_ID_BYTES_MAX as i64,
                        BOUNDED_ROW_BYTES_MAX as i64,
                    ],
                    AGENT_SESSIONS_GLOBAL_ROWS_MAX,
                )?,
                sql_limit,
            ))
        } else {
            None
        };
        let archived_agent_resume_probe = if job.include_agent_sessions {
            let sql_limit = bounded_limit_plus_one(AGENT_SESSION_ROWS_MAX, AGENT_SESSION_ROWS_MAX)?;
            Some((
                bounded_read_preflight(
                    &tx,
                    ARCHIVED_AGENT_RESUME_PREFLIGHT,
                    rusqlite::params![
                        job.workspace_id,
                        sql_limit,
                        BOUNDED_ID_BYTES_MAX as i64,
                        BOUNDED_TEXT_BYTES_MAX as i64,
                        BOUNDED_ROW_BYTES_MAX as i64,
                    ],
                    AGENT_SESSION_ROWS_MAX,
                )?,
                sql_limit,
            ))
        } else {
            None
        };
        let structured_probe = if job.include_structured_threads {
            let sql_limit = bounded_limit_plus_one(
                AGENT_STATE_STRUCTURED_PROJECTION_MAX,
                AGENT_STATE_STRUCTURED_PROJECTION_MAX,
            )?;
            let preflight_sql =
                agent_state_structured_scope_sql(job.structured_workspace_ids.len(), false);
            let mut params = Vec::with_capacity(job.structured_workspace_ids.len() + 10);
            params.extend(
                job.structured_workspace_ids
                    .iter()
                    .cloned()
                    .map(rusqlite::types::Value::Text),
            );
            params.extend([
                rusqlite::types::Value::Integer(job.include_archived_threads as i64),
                rusqlite::types::Value::Integer(STRUCTURED_THREAD_ID_BYTES_MAX as i64),
                rusqlite::types::Value::Integer(STRUCTURED_THREAD_ID_BYTES_MAX as i64),
                rusqlite::types::Value::Integer(sql_limit),
                rusqlite::types::Value::Integer(STRUCTURED_THREAD_ID_BYTES_MAX as i64),
                rusqlite::types::Value::Integer(STRUCTURED_THREAD_ID_BYTES_MAX as i64),
                rusqlite::types::Value::Integer(STRUCTURED_THREAD_ID_BYTES_MAX as i64),
                rusqlite::types::Value::Integer(STRUCTURED_THREAD_CWD_BYTES_MAX as i64),
                rusqlite::types::Value::Integer(STRUCTURED_THREAD_MODEL_BYTES_MAX as i64),
                rusqlite::types::Value::Integer(STRUCTURED_THREAD_ROW_BYTES_MAX as i64),
            ]);
            Some((
                bounded_read_preflight(
                    &tx,
                    &preflight_sql,
                    rusqlite::params_from_iter(&params),
                    AGENT_STATE_STRUCTURED_PROJECTION_MAX,
                )?,
                sql_limit,
            ))
        } else {
            None
        };
        let work_history_query = job
            .include_work_history
            .then(|| AgentWorkHistoryQuery::for_workspace(job.workspace_id.clone()));
        let work_history_probe = work_history_query
            .as_ref()
            .map(|query| agent_work_history_probe(&tx, query))
            .transpose()?;
        let activity_sql_limit = job
            .include_activity_panes
            .then(|| bounded_limit_plus_one(ACTIVITY_PANE_ROWS_MAX, ACTIVITY_PANE_ROWS_MAX))
            .transpose()?;
        let activity_probe = if let Some(sql_limit) = activity_sql_limit {
            Some(bounded_read_preflight(
                &tx,
                ACTIVITY_PANES_BOUNDED_PREFLIGHT,
                rusqlite::params![
                    sql_limit,
                    BOUNDED_ID_BYTES_MAX as i64,
                    BOUNDED_TEXT_BYTES_MAX as i64,
                    BOUNDED_ROW_BYTES_MAX as i64,
                ],
                ACTIVITY_PANE_ROWS_MAX,
            )?)
        } else {
            None
        };
        let logical_retained_bytes = [
            idle_probe
                .as_ref()
                .map_or(0, |(probe, _, _)| probe.retained_bytes),
            hook_status_probes
                .as_ref()
                .map_or(0, |(probe, _, _, _)| probe.retained_bytes),
            hook_status_probes
                .as_ref()
                .map_or(0, |(_, probe, _, _)| probe.retained_bytes),
            attention_probes
                .as_ref()
                .map_or(0, |(probe, _, _, _, _)| probe.retained_bytes),
            attention_probes
                .as_ref()
                .map_or(0, |(_, probe, _, _, _)| probe.retained_bytes),
            attention_probes
                .as_ref()
                .map_or(0, |(_, _, probe, _, _)| probe.retained_bytes),
            agent_probe
                .as_ref()
                .map_or(0, |(probe, _)| probe.retained_bytes),
            global_agent_probe
                .as_ref()
                .map_or(0, |(probe, _)| probe.retained_bytes),
            structured_probe
                .as_ref()
                .map_or(0, |(probe, _)| probe.retained_bytes),
            work_history_probe
                .as_ref()
                .map_or(0, |(probe, _)| probe.retained_bytes),
            activity_probe
                .as_ref()
                .map_or(0, |probe| probe.retained_bytes),
        ]
        .into_iter()
        .try_fold(0usize, usize::checked_add)
        .ok_or_else(|| anyhow::anyhow!(AGENT_STATE_SNAPSHOT_INVALID))?;
        anyhow::ensure!(
            logical_retained_bytes <= job.snapshot_bytes_max,
            AGENT_STATE_SNAPSHOT_INVALID
        );

        let hook_sessions = if let Some((hook_probe, _, sql_limit, snapshot_epoch)) =
            &hook_status_probes
        {
            let mut result = Vec::with_capacity(hook_probe.count);
            let mut stmt = tx
                .prepare(HOOK_SESSIONS_PREFIX_SELECT)
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            let mut rows = stmt
                .query(rusqlite::params![
                    workspace_prefix,
                    sql_limit,
                    BOUNDED_ID_BYTES_MAX as i64,
                    snapshot_epoch,
                ])
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            while let Some(row) = rows
                .next()
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?
            {
                result.push(HookSessionRow {
                    session_key: bounded_required_text(row, 0, BOUNDED_ID_BYTES_MAX, true, true)?
                        .to_owned(),
                    kind: bounded_required_text(row, 1, BOUNDED_TEXT_BYTES_MAX, true, false)?
                        .to_owned(),
                    agent_session_id: bounded_required_text(
                        row,
                        2,
                        BOUNDED_ID_BYTES_MAX,
                        true,
                        true,
                    )?
                    .to_owned(),
                    transcript_path: bounded_required_text(
                        row,
                        3,
                        BOUNDED_TEXT_BYTES_MAX,
                        false,
                        false,
                    )?
                    .to_owned(),
                    task_prompt: bounded_optional_text(row, 4, 256)?.map(str::to_owned),
                });
            }
            result
        } else {
            Vec::new()
        };
        let statuslines = if let Some((_, status_probe, sql_limit, snapshot_epoch)) =
            &hook_status_probes
        {
            let mut result = Vec::with_capacity(status_probe.count);
            let mut stmt = tx
                .prepare(STATUSLINES_PREFIX_SELECT)
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            let mut rows = stmt
                .query(rusqlite::params![
                    workspace_prefix,
                    sql_limit,
                    BOUNDED_ID_BYTES_MAX as i64,
                    snapshot_epoch,
                ])
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            while let Some(row) = rows
                .next()
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?
            {
                result.push(StatuslineRow {
                    session_key: bounded_required_text(row, 0, BOUNDED_ID_BYTES_MAX, true, true)?
                        .to_owned(),
                    effort: bounded_optional_text(row, 1, BOUNDED_TEXT_BYTES_MAX)?
                        .map(str::to_owned),
                    model: bounded_optional_text(row, 2, BOUNDED_TEXT_BYTES_MAX)?
                        .map(str::to_owned),
                    context_pct: bounded_optional_integer(row, 3)?,
                });
            }
            result
        } else {
            Vec::new()
        };
        let mut response_sessions = Vec::new();
        let waiting_sessions = if let Some((waiting_probe, _, _, sql_limit, snapshot_epoch)) =
            &attention_probes
        {
            let mut result = Vec::with_capacity(waiting_probe.count);
            let mut stmt = tx
                .prepare(WAITING_SESSIONS_SELECT)
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            let mut rows = stmt
                .query(rusqlite::params![
                    sql_limit,
                    BOUNDED_ID_BYTES_MAX as i64,
                    snapshot_epoch,
                ])
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            while let Some(row) = rows
                .next()
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?
            {
                if bounded_integer(row, 2)? == 1 {
                    response_sessions.push(
                        bounded_required_text(row, 0, BOUNDED_ID_BYTES_MAX, true, true)?.to_owned(),
                    );
                }
                result.push((
                    bounded_required_text(row, 0, BOUNDED_ID_BYTES_MAX, true, true)?.to_owned(),
                    bounded_optional_text(row, 1, BOUNDED_MESSAGE_BYTES_MAX)?.map(str::to_owned),
                ));
            }
            result
        } else {
            Vec::new()
        };
        let turn_done_sessions =
            if let Some((_, turn_probe, _, sql_limit, snapshot_epoch)) = &attention_probes {
                let mut result = Vec::with_capacity(turn_probe.count);
                let mut stmt = tx
                    .prepare(TURN_DONE_SESSIONS_SELECT)
                    .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
                let mut rows = stmt
                    .query(rusqlite::params![
                        sql_limit,
                        BOUNDED_ID_BYTES_MAX as i64,
                        snapshot_epoch,
                    ])
                    .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
                while let Some(row) = rows
                    .next()
                    .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?
                {
                    result.push((
                        bounded_required_text(row, 0, BOUNDED_ID_BYTES_MAX, true, true)?.to_owned(),
                        bounded_integer(row, 1)?,
                    ));
                }
                result
            } else {
                Vec::new()
            };
        let idle_sessions = if let Some((probe, limit, epoch)) = &idle_probe {
            let mut result = Vec::with_capacity(probe.count);
            let mut stmt = tx
                .prepare(IDLE_SESSIONS_SELECT)
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            let mut rows = stmt
                .query(rusqlite::params![limit, BOUNDED_ID_BYTES_MAX as i64, epoch])
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            while let Some(row) = rows
                .next()
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?
            {
                result.push((
                    bounded_required_text(row, 0, BOUNDED_ID_BYTES_MAX, true, true)?.to_owned(),
                    bounded_integer(row, 1)?,
                    bounded_integer(row, 2)?,
                ));
            }
            result
        } else {
            Vec::new()
        };
        let working_sessions =
            if let Some((_, _, working_probe, sql_limit, snapshot_epoch)) = &attention_probes {
                let mut result = Vec::with_capacity(working_probe.count);
                let mut stmt = tx
                    .prepare(WORKING_SESSIONS_SELECT)
                    .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
                let mut rows = stmt
                    .query(rusqlite::params![
                        sql_limit,
                        BOUNDED_ID_BYTES_MAX as i64,
                        snapshot_epoch,
                    ])
                    .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
                while let Some(row) = rows
                    .next()
                    .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?
                {
                    result.push(
                        bounded_required_text(row, 0, BOUNDED_ID_BYTES_MAX, true, true)?.to_owned(),
                    );
                }
                result
            } else {
                Vec::new()
            };
        let agent_sessions = if let Some((agent_probe, sql_limit)) = &agent_probe {
            let mut result = Vec::with_capacity(agent_probe.count);
            let mut stmt = tx
                .prepare(AGENT_SESSIONS_BOUNDED_SELECT)
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            let mut rows = stmt
                .query(rusqlite::params![
                    job.workspace_id,
                    sql_limit,
                    BOUNDED_ID_BYTES_MAX as i64,
                ])
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            while let Some(row) = rows
                .next()
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?
            {
                result.push(AgentSessionRow {
                    pane_id: bounded_required_text(row, 0, BOUNDED_ID_BYTES_MAX, true, true)?
                        .to_owned(),
                    kind: bounded_required_text(row, 1, BOUNDED_TEXT_BYTES_MAX, true, false)?
                        .to_owned(),
                    session_id: bounded_required_text(row, 2, BOUNDED_ID_BYTES_MAX, true, true)?
                        .to_owned(),
                    task_prompt: bounded_optional_text(row, 3, 256)?.map(str::to_owned),
                });
            }
            result
        } else {
            Vec::new()
        };
        let global_agent_sessions =
            if let Some((global_agent_probe, sql_limit)) = &global_agent_probe {
                let mut result = Vec::with_capacity(global_agent_probe.count);
                let mut stmt = tx
                    .prepare(AGENT_SESSIONS_GLOBAL_BOUNDED_SELECT)
                    .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
                let mut rows = stmt
                    .query(rusqlite::params![sql_limit, BOUNDED_ID_BYTES_MAX as i64])
                    .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
                while let Some(row) = rows
                    .next()
                    .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?
                {
                    let workspace_id =
                        bounded_required_text(row, 0, BOUNDED_ID_BYTES_MAX, true, true)?.to_owned();
                    let pane_id =
                        bounded_required_text(row, 1, BOUNDED_ID_BYTES_MAX, true, true)?.to_owned();
                    result.push((workspace_id, pane_id));
                }
                result
            } else {
                Vec::new()
            };
        let archived_agent_resume = if let Some((probe, sql_limit)) = &archived_agent_resume_probe {
            let mut result = Vec::with_capacity(probe.count);
            let mut stmt = tx
                .prepare(ARCHIVED_AGENT_RESUME_SELECT)
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            let mut rows = stmt
                .query(rusqlite::params![
                    job.workspace_id,
                    sql_limit,
                    BOUNDED_ID_BYTES_MAX as i64,
                ])
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            while let Some(row) = rows
                .next()
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?
            {
                result.push(ArchivedAgentResumeRow {
                    persistent_session_id: bounded_required_text(
                        row,
                        0,
                        BOUNDED_ID_BYTES_MAX,
                        true,
                        true,
                    )?
                    .to_owned(),
                    agent_id: bounded_required_text(row, 1, BOUNDED_ID_BYTES_MAX, true, true)?
                        .to_owned(),
                    kind: bounded_optional_text(row, 2, BOUNDED_TEXT_BYTES_MAX)?.map(str::to_owned),
                    session_id: bounded_optional_text(row, 3, BOUNDED_ID_BYTES_MAX)?
                        .map(str::to_owned),
                });
            }
            result
        } else {
            Vec::new()
        };
        let structured_threads = if let Some((structured_probe, sql_limit)) = &structured_probe {
            let mut result = Vec::with_capacity(structured_probe.count);
            let sql = agent_state_structured_scope_sql(job.structured_workspace_ids.len(), true);
            let mut params = Vec::with_capacity(job.structured_workspace_ids.len() + 4);
            params.extend(
                job.structured_workspace_ids
                    .iter()
                    .cloned()
                    .map(rusqlite::types::Value::Text),
            );
            params.extend([
                rusqlite::types::Value::Integer(job.include_archived_threads as i64),
                rusqlite::types::Value::Integer(STRUCTURED_THREAD_ID_BYTES_MAX as i64),
                rusqlite::types::Value::Integer(STRUCTURED_THREAD_ID_BYTES_MAX as i64),
                rusqlite::types::Value::Integer(*sql_limit),
            ]);
            let mut stmt = tx
                .prepare(&sql)
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            let mut rows = stmt
                .query(rusqlite::params_from_iter(&params))
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            while let Some(row) = rows
                .next()
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?
            {
                let local_session_id = structured_thread_required_text(
                    row,
                    0,
                    STRUCTURED_THREAD_ID_BYTES_MAX,
                    true,
                    true,
                    false,
                )?;
                let workspace_id = structured_thread_required_text(
                    row,
                    1,
                    STRUCTURED_THREAD_ID_BYTES_MAX,
                    true,
                    true,
                    false,
                )?;
                let thread_id = structured_thread_required_text(
                    row,
                    2,
                    STRUCTURED_THREAD_ID_BYTES_MAX,
                    true,
                    true,
                    false,
                )?;
                let title = structured_thread_required_text(
                    row,
                    3,
                    STRUCTURED_THREAD_ROW_BYTES_MAX,
                    false,
                    false,
                    false,
                )?;
                let cwd = structured_thread_required_text(
                    row,
                    4,
                    STRUCTURED_THREAD_CWD_BYTES_MAX,
                    false,
                    false,
                    true,
                )?;
                let model =
                    structured_thread_optional_text(row, 5, STRUCTURED_THREAD_MODEL_BYTES_MAX)?;
                let favorite = structured_thread_integer(row, 6)?;
                let archived = structured_thread_integer(row, 7)?;
                anyhow::ensure!(
                    matches!(favorite, 0 | 1) && matches!(archived, 0 | 1),
                    AGENT_STATE_SNAPSHOT_INVALID
                );
                result.push(StructuredThreadRow {
                    local_session_id: local_session_id.to_owned(),
                    workspace_id: workspace_id.to_owned(),
                    thread_id: thread_id.to_owned(),
                    title: title.to_owned(),
                    cwd: cwd.to_owned(),
                    model: model.map(str::to_owned),
                    favorite: favorite != 0,
                    archived: archived != 0,
                    created_at: structured_thread_integer(row, 8)?,
                    updated_at: structured_thread_integer(row, 9)?,
                });
            }
            result
        } else {
            Vec::new()
        };
        let work_turns = if let (Some(query), Some((probe, sql_limit))) =
            (&work_history_query, work_history_probe)
        {
            read_agent_work_history(&tx, query, probe, sql_limit)?
        } else {
            Vec::new()
        };
        let activity_panes = if let (Some(sql_limit), Some(probe)) =
            (activity_sql_limit, activity_probe)
        {
            let mut result = Vec::with_capacity(probe.count);
            let mut stmt = tx
                .prepare(ACTIVITY_PANES_BOUNDED_SELECT)
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            let mut rows = stmt
                .query(rusqlite::params![
                    sql_limit,
                    BOUNDED_ID_BYTES_MAX as i64,
                    BOUNDED_TEXT_BYTES_MAX as i64,
                ])
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            while let Some(row) = rows
                .next()
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?
            {
                let workspace_id = bounded_required_text(row, 0, BOUNDED_ID_BYTES_MAX, true, true)?;
                let pane_id = bounded_required_text(row, 1, BOUNDED_ID_BYTES_MAX, true, true)?;
                let title = bounded_required_text(row, 2, BOUNDED_TEXT_BYTES_MAX, true, false)?;
                let cwd = bounded_required_text(row, 3, BOUNDED_TEXT_BYTES_MAX, false, false)?;
                result.push(PersistedActivityPane {
                    workspace_id: workspace_id.to_owned(),
                    pane_id: pane_id.to_owned(),
                    title: title.to_owned(),
                    cwd: cwd.to_owned(),
                });
            }
            result
        } else {
            Vec::new()
        };

        let snapshot = AgentStateSnapshot {
            hook_sessions,
            statuslines,
            waiting_sessions,
            response_sessions,
            turn_done_sessions,
            idle_sessions,
            working_sessions,
            agent_sessions,
            global_agent_sessions,
            archived_agent_resume,
            structured_threads,
            work_turns,
            activity_panes,
        };
        let actual_retained_bytes = agent_state_snapshot_retained_bytes(&snapshot)
            .map_err(|_| anyhow::anyhow!(AGENT_STATE_SNAPSHOT_INVALID))?;
        anyhow::ensure!(
            actual_retained_bytes <= job.snapshot_bytes_max,
            AGENT_STATE_SNAPSHOT_INVALID
        );
        tx.commit()
            .map_err(|_| anyhow::anyhow!(AGENT_STATE_PERSIST_FAILED))?;
        Ok(snapshot)
    }

    /// 구조화 Codex thread 메타데이터를 저장한다. local/thread ID는 모두 durable하며,
    /// 갱신 시 created_at은 보존하고 updated_at만 현재 시각으로 올린다.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_structured_thread(
        &self,
        local_session_id: &str,
        workspace_id: &str,
        thread_id: &str,
        title: &str,
        cwd: &str,
        model: Option<&str>,
        favorite: bool,
        archived: bool,
    ) -> anyhow::Result<()> {
        structured_thread_input_bytes(
            local_session_id,
            workspace_id,
            thread_id,
            title,
            cwd,
            model,
        )?;
        self.conn
            .execute(
                "INSERT INTO structured_threads
                    (local_session_id, workspace_id, thread_id, title, cwd, model,
                     favorite, archived, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                         CAST(strftime('%s','now') AS INTEGER),
                         CAST(strftime('%s','now') AS INTEGER))
                 ON CONFLICT(local_session_id) DO UPDATE SET
                    workspace_id = excluded.workspace_id,
                    thread_id = excluded.thread_id,
                    title = excluded.title,
                    cwd = excluded.cwd,
                    model = excluded.model,
                    favorite = excluded.favorite,
                    archived = excluded.archived,
                    updated_at = CAST(strftime('%s','now') AS INTEGER)",
                rusqlite::params![
                    local_session_id,
                    workspace_id,
                    thread_id,
                    title,
                    cwd,
                    model,
                    favorite as i64,
                    archived as i64,
                ],
            )
            .map_err(|_| anyhow::anyhow!(STRUCTURED_THREAD_PERSIST_FAILED))?;
        Ok(())
    }

    /// 워크스페이스당 조회 상한 — 유일한 프로덕션 소비자(UI 스레드 목록 import)가
    /// refresh_workspaces마다 전량 로드해 AgentSession placeholder로 상주시키므로,
    /// 수개월치 스레드가 무제한 쌓이지 않게 자른다. 정렬이 즐겨찾기 우선·최신순이라
    /// 잘리는 것은 가장 오래된 비즐겨찾기 스레드다 (행 자체는 DB에 남는다).
    const STRUCTURED_THREADS_LIST_CAP: usize = 500;

    pub fn list_structured_threads(
        &self,
        workspace_id: &str,
        include_archived: bool,
    ) -> anyhow::Result<Vec<StructuredThreadRow>> {
        self.list_structured_threads_bounded(
            workspace_id,
            include_archived,
            Self::STRUCTURED_THREADS_LIST_CAP,
        )
    }

    /// `list_structured_threads`와 같은 정렬/필터를 사용하되 호출자가 현재 남은
    /// projection 용량만 요청할 수 있게 한다. 선택된 SQLite 행 전체의 타입과 byte
    /// budget을 같은 statement snapshot에서 검증한 뒤에만 Rust String을 만든다.
    pub fn list_structured_threads_bounded(
        &self,
        workspace_id: &str,
        include_archived: bool,
        limit: usize,
    ) -> anyhow::Result<Vec<StructuredThreadRow>> {
        anyhow::ensure!(
            structured_thread_id_is_valid(workspace_id)
                && limit <= Self::STRUCTURED_THREADS_LIST_CAP,
            STRUCTURED_THREAD_INPUT_INVALID
        );
        if limit == 0 {
            return Ok(Vec::new());
        }
        let limit =
            i64::try_from(limit).map_err(|_| anyhow::anyhow!(STRUCTURED_THREAD_INPUT_INVALID))?;
        let mut stmt = self
            .conn
            .prepare(STRUCTURED_THREADS_BOUNDED_QUERY)
            .map_err(|_| anyhow::anyhow!(STRUCTURED_THREAD_QUERY_FAILED))?;
        let mut rows = stmt
            .query(rusqlite::params![
                workspace_id,
                include_archived as i64,
                limit,
                STRUCTURED_THREAD_ID_BYTES_MAX as i64,
                STRUCTURED_THREAD_CWD_BYTES_MAX as i64,
                STRUCTURED_THREAD_MODEL_BYTES_MAX as i64,
                STRUCTURED_THREAD_ROW_BYTES_MAX as i64,
                STRUCTURED_THREADS_RETAINED_BYTES_MAX as i64,
            ])
            .map_err(|_| anyhow::anyhow!(STRUCTURED_THREAD_QUERY_FAILED))?;
        let mut result = Vec::with_capacity(limit as usize);
        while let Some(row) = rows
            .next()
            .map_err(|_| anyhow::anyhow!(STRUCTURED_THREAD_QUERY_FAILED))?
        {
            let row_kind = structured_thread_integer(row, 0)?;
            let invalid_rows = structured_thread_integer(row, 11)?;
            let retained_bytes = structured_thread_integer(row, 12)?;
            anyhow::ensure!(
                invalid_rows == 0
                    && retained_bytes >= 0
                    && usize::try_from(retained_bytes)
                        .is_ok_and(|bytes| { bytes <= STRUCTURED_THREADS_RETAINED_BYTES_MAX }),
                STRUCTURED_THREAD_ROW_INVALID
            );
            if row_kind == 0 {
                continue;
            }
            anyhow::ensure!(row_kind == 1, STRUCTURED_THREAD_ROW_INVALID);

            let local_session_id = structured_thread_required_text(
                row,
                1,
                STRUCTURED_THREAD_ID_BYTES_MAX,
                true,
                true,
                false,
            )?;
            let row_workspace_id = structured_thread_required_text(
                row,
                2,
                STRUCTURED_THREAD_ID_BYTES_MAX,
                true,
                true,
                false,
            )?;
            let thread_id = structured_thread_required_text(
                row,
                3,
                STRUCTURED_THREAD_ID_BYTES_MAX,
                true,
                true,
                false,
            )?;
            let title = structured_thread_required_text(
                row,
                4,
                STRUCTURED_THREAD_ROW_BYTES_MAX,
                false,
                false,
                false,
            )?;
            let cwd = structured_thread_required_text(
                row,
                5,
                STRUCTURED_THREAD_CWD_BYTES_MAX,
                false,
                false,
                true,
            )?;
            let model = structured_thread_optional_text(row, 6, STRUCTURED_THREAD_MODEL_BYTES_MAX)?;
            let favorite = structured_thread_integer(row, 7)?;
            let archived = structured_thread_integer(row, 8)?;
            let created_at = structured_thread_integer(row, 9)?;
            let updated_at = structured_thread_integer(row, 10)?;
            anyhow::ensure!(
                matches!(favorite, 0 | 1) && matches!(archived, 0 | 1),
                STRUCTURED_THREAD_ROW_INVALID
            );
            structured_thread_input_bytes(
                local_session_id,
                row_workspace_id,
                thread_id,
                title,
                cwd,
                model,
            )
            .map_err(|_| anyhow::anyhow!(STRUCTURED_THREAD_ROW_INVALID))?;
            result.push(StructuredThreadRow {
                local_session_id: local_session_id.to_owned(),
                workspace_id: row_workspace_id.to_owned(),
                thread_id: thread_id.to_owned(),
                title: title.to_owned(),
                cwd: cwd.to_owned(),
                model: model.map(str::to_owned),
                favorite: favorite != 0,
                archived: archived != 0,
                created_at,
                updated_at,
            });
        }
        Ok(result)
    }

    pub fn set_structured_thread_favorite(
        &self,
        local_session_id: &str,
        favorite: bool,
    ) -> anyhow::Result<bool> {
        let affected = self.conn.execute(
            "UPDATE structured_threads
                SET favorite = ?2, updated_at = CAST(strftime('%s','now') AS INTEGER)
              WHERE local_session_id = ?1",
            (local_session_id, favorite as i64),
        )?;
        Ok(affected == 1)
    }

    pub fn set_structured_thread_archived(
        &self,
        local_session_id: &str,
        archived: bool,
    ) -> anyhow::Result<bool> {
        let affected = self.conn.execute(
            "UPDATE structured_threads
                SET archived = ?2, updated_at = CAST(strftime('%s','now') AS INTEGER)
              WHERE local_session_id = ?1",
            (local_session_id, archived as i64),
        )?;
        Ok(affected == 1)
    }

    pub fn touch_structured_thread(&self, local_session_id: &str) -> anyhow::Result<bool> {
        let affected = self.conn.execute(
            "UPDATE structured_threads
                SET updated_at = CAST(strftime('%s','now') AS INTEGER)
              WHERE local_session_id = ?1",
            [local_session_id],
        )?;
        Ok(affected == 1)
    }

    pub fn delete_structured_thread(&self, local_session_id: &str) -> anyhow::Result<bool> {
        let affected = self.conn.execute(
            "DELETE FROM structured_threads WHERE local_session_id = ?1",
            [local_session_id],
        )?;
        Ok(affected == 1)
    }

    /// workspace 이름을 변경한다 (#3 — 사용자 지정 이름).
    pub fn rename_workspace(&self, id: &str, name: &str) -> anyhow::Result<()> {
        validate_workspace_text_input(id, "id")?;
        validate_workspace_text_input(name, "name")?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let mut row = settings_workspace_row_for_update(&tx, id)?
            .ok_or_else(|| anyhow::anyhow!("workspace 없음: {id}"))?;
        row.name = name.to_owned();
        settings_workspace_update_admission(&tx, &row)?;
        let affected = tx
            .execute(
                "UPDATE workspaces
                 SET name = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE id = ?1",
                (id, name),
            )
            .with_context(|| format!("workspace 이름 저장 실패: {id}"))?;
        anyhow::ensure!(affected == 1, "workspace 없음: {id}");
        tx.commit()
            .context("workspace rename transaction commit failed")
    }

    pub fn set_workspace_path(&self, id: &str, path: &str) -> anyhow::Result<()> {
        validate_workspace_text_input(id, "id")?;
        validate_workspace_text_input(path, "path")?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let mut row = settings_workspace_row_for_update(&tx, id)?
            .ok_or_else(|| anyhow::anyhow!("workspace 없음: {id}"))?;
        row.path = path.to_owned();
        settings_workspace_update_admission(&tx, &row)?;
        let affected = tx
            .execute(
                "UPDATE workspaces
                 SET path = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE id = ?1",
                (id, path),
            )
            .with_context(|| format!("workspace 경로 저장 실패: {id}"))?;
        anyhow::ensure!(affected == 1, "workspace 없음: {id}");
        tx.commit()
            .context("workspace path transaction commit failed")
    }

    /// Atomically replaces one exact workspace's project path and optional filesystem anchor.
    /// The anchor is all-or-nothing: a partial `(dev, ino)` pair is rejected before SQLite is
    /// touched. The complete candidate row is checked under the same IMMEDIATE transaction before
    /// UPDATE, then re-read as a defensive postcondition before path and anchor commit together.
    pub fn set_workspace_path_and_anchor(
        &self,
        workspace_id: &str,
        path: &str,
        path_dev: Option<i64>,
        path_ino: Option<i64>,
    ) -> anyhow::Result<SettingsWorkspaceProjectionRow> {
        self.set_workspace_path_and_anchor_with_volume(workspace_id, path, path_dev, path_ino, None)
    }

    /// Explicit user rebinding replaces any prior volume proof atomically,
    /// including when the path and numerical inode anchor happen to be equal.
    pub fn set_workspace_path_and_anchor_with_volume(
        &self,
        workspace_id: &str,
        path: &str,
        path_dev: Option<i64>,
        path_ino: Option<i64>,
        volume: Option<uuid::Uuid>,
    ) -> anyhow::Result<SettingsWorkspaceProjectionRow> {
        anyhow::ensure!(
            volume.is_none() || (path_dev.is_some() && path_ino.is_some()),
            "workspace_volume_identity_anchor_missing"
        );
        validate_workspace_text_input(workspace_id, "id")?;
        validate_workspace_text_input(path, "path")?;
        anyhow::ensure!(
            path_dev.is_some() == path_ino.is_some(),
            "settings_workspace_path_anchor_partial"
        );
        let input_bytes = workspace_id
            .len()
            .checked_add(path.len())
            .context("settings_workspace_path_input_bytes_overflow")?;
        anyhow::ensure!(
            input_bytes <= SETTINGS_ROW_BYTES_MAX,
            "settings_workspace_path_input_row_bytes_limit"
        );

        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let mut row = settings_workspace_row_for_update(&tx, workspace_id)?
            .ok_or_else(|| anyhow::anyhow!("settings_workspace_path_exact_id_missing"))?;
        row.path = path.to_owned();
        row.path_dev = path_dev;
        row.path_ino = path_ino;
        settings_workspace_update_admission(&tx, &row)?;
        let affected = tx
            .execute(
                "UPDATE workspaces
                 SET path = ?2, path_dev = ?3, path_ino = ?4,
                     updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE id = ?1",
                (workspace_id, path, path_dev, path_ino),
            )
            .with_context(|| format!("workspace project path update failed: {workspace_id}"))?;
        anyhow::ensure!(affected == 1, "settings_workspace_path_exact_id_missing");

        let probe = settings_read_probe(
            &tx,
            "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
             FROM (
                 SELECT length(CAST(id AS BLOB)) + length(CAST(name AS BLOB)) +
                        length(CAST(path AS BLOB)) + length(CAST(created_at AS BLOB)) +
                        length(CAST(COALESCE(path_dev, '') AS BLOB)) +
                        length(CAST(COALESCE(path_ino, '') AS BLOB)) AS row_bytes
                 FROM workspaces WHERE id = ?1 LIMIT 2
             )",
            [workspace_id],
            1,
            SETTINGS_SNAPSHOT_BYTES_MAX,
            SETTINGS_ROW_BYTES_MAX,
            "settings_workspace_path_postcondition",
        )?;
        anyhow::ensure!(
            probe.count == 1,
            "settings_workspace_path_postcondition_missing"
        );
        let persisted = tx.query_row(
            "SELECT id, name, path, created_at, path_dev, path_ino
             FROM workspaces WHERE id = ?1",
            [workspace_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )?;
        let projection = settings_workspace_projection_from_persisted(persisted)?;
        if let Some(anchor) = projection.folder_anchor {
            workspace_identity::record(&tx, workspace_id, path, anchor, volume)?;
        } else {
            tx.execute(
                "DELETE FROM workspace_volume_identities WHERE workspace_id=?1",
                [workspace_id],
            )?;
        }
        tx.commit()?;
        Ok(projection)
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

    /// 프로젝트 폴더 앵커(dev, ino)를 저장한다 — 폴더 rename 감지·복구용(2026-07-08).
    /// path 미설정/무효면 (None, None)으로 지운다.
    pub fn set_workspace_anchor(
        &self,
        id: &str,
        dev: Option<i64>,
        ino: Option<i64>,
    ) -> anyhow::Result<()> {
        validate_workspace_text_input(id, "id")?;
        anyhow::ensure!(
            dev.is_some() == ino.is_some(),
            "settings_workspace_anchor_partial"
        );
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let Some(mut row) = settings_workspace_row_for_update(&tx, id)? else {
            tx.commit()
                .context("missing workspace anchor transaction commit failed")?;
            return Ok(());
        };
        row.path_dev = dev;
        row.path_ino = ino;
        settings_workspace_update_admission(&tx, &row)?;
        tx.execute(
            "UPDATE workspaces SET path_dev = ?2, path_ino = ?3 WHERE id = ?1",
            (id, dev, ino),
        )
        .with_context(|| format!("workspace 앵커 저장 실패: {id}"))?;
        tx.commit()
            .context("workspace anchor transaction commit failed")
    }

    /// 프로젝트 폴더 앵커(dev, ino) — 둘 다 있을 때만 Some.
    pub fn workspace_anchor(&self, id: &str) -> anyhow::Result<Option<(i64, i64)>> {
        let row: Option<(Option<i64>, Option<i64>)> = self
            .conn
            .query_row(
                "SELECT path_dev, path_ino FROM workspaces WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.and_then(|(d, i)| Some((d?, i?))))
    }

    /// workspace + 그 자식 데이터(세션/mux/env)를 한 트랜잭션으로 삭제한다 (destructive).
    /// mcp_servers(전역)·감사 로그(FK 없음)는 남긴다. 활성/마지막 workspace 삭제 방지는
    /// 호출측(UI) 책임.
    /// 워크스페이스 메모를 읽는다. 미작성이면 `None` — 빈 문자열과 구분해야 UI가
    /// placeholder를 고를 수 있다(빈 본문은 애초에 행으로 남지 않는다).
    pub fn load_workspace_note(&self, workspace_id: &str) -> anyhow::Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT body FROM workspace_notes WHERE workspace_id = ?1")?;
        let body = stmt
            .query_row([workspace_id], |row| row.get::<_, String>(0))
            .optional()?;
        Ok(body)
    }

    /// 워크스페이스 메모를 저장한다. 공백만 남은 본문은 **행을 지운다** — 빈 행이
    /// 워크스페이스마다 쌓이는 것을 막고, `load`가 "안 쓴 것"과 "지운 것"을 같게 본다.
    /// 상한 초과는 거부하며 기존 내용을 건드리지 않는다.
    pub fn save_workspace_note(&self, workspace_id: &str, body: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            body.len() <= WORKSPACE_NOTE_MAX_BYTES,
            "workspace_note_bytes_limit"
        );
        if body.trim().is_empty() {
            self.conn.execute(
                "DELETE FROM workspace_notes WHERE workspace_id = ?1",
                [workspace_id],
            )?;
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO workspace_notes (workspace_id, body, updated_at)
             VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             ON CONFLICT(workspace_id) DO UPDATE
               SET body = excluded.body, updated_at = excluded.updated_at",
            (workspace_id, body),
        )?;
        Ok(())
    }

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
        // 구조화 Codex thread 메타데이터도 workspace와 함께 정리한다. 실제 Codex
        // rollout/thread archive 여부는 App Server가 별도로 소유한다.
        tx.execute(
            "DELETE FROM structured_threads WHERE workspace_id = ?1",
            [workspace_id],
        )?;
        tx.execute("DELETE FROM workspaces WHERE id = ?1", [workspace_id])?;
        tx.commit()?;
        Ok(())
    }

    /// 새 workspace 생성 — 생성된 id 반환.
    pub fn create_workspace(&self, name: &str) -> anyhow::Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        settings_workspace_write_admission(&tx, &id, name, "", None, None)?;
        tx.execute(
            "INSERT INTO workspaces (id, name, path, created_at, updated_at)
             VALUES (?1, ?2, '',
                strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
            (&id, name),
        )?;
        tx.commit()
            .context("workspace create transaction commit failed")?;
        Ok(id)
    }

    /// MCP 서버 목록 (Connector Center, PR-17). repo 로직은 mcp crate 소유.
    pub fn list_mcp_servers(&self) -> anyhow::Result<Vec<mcp_store::McpServerRow>> {
        mcp_store::list_servers(&self.conn)
    }

    /// Connector overview용 complete-or-error bounded inventory. SQL performs a `limit + 1`
    /// count/byte probe first, then materializes at most `limit` lightweight summary rows.
    pub fn mcp_server_inventory(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<mcp_store::McpServerInventoryRow>> {
        mcp_store::server_inventory(&self.conn, limit)
    }

    /// Bounded inventory paired with the durable revision read from the same SQLite snapshot.
    pub fn mcp_server_inventory_versioned(
        &self,
        limit: usize,
    ) -> anyhow::Result<ConnectorConfigRead<Vec<mcp_store::McpServerInventoryRow>>> {
        self.read_connector_config(|conn| mcp_store::server_inventory_in_snapshot(conn, limit))
    }

    pub fn mcp_server(&self, server_id: &str) -> anyhow::Result<Option<mcp_store::McpServerRow>> {
        mcp_store::server(&self.conn, server_id)
    }

    /// Bounded selected-server lookup paired with its same-snapshot durable revision.
    pub fn mcp_server_versioned(
        &self,
        server_id: &str,
    ) -> anyhow::Result<ConnectorConfigRead<Option<mcp_store::McpServerRow>>> {
        self.read_connector_config(|conn| mcp_store::server_in_snapshot(conn, server_id))
    }

    /// Loads one execution target from exactly one Connector revision snapshot. Stdio credential
    /// locations are aligned with `server.env_secrets`; HTTP OAuth candidates are bounded to two.
    /// No secret value, service DTO, or protocol runtime type crosses this storage boundary.
    pub fn mcp_request_target_versioned(
        &self,
        server_id: &str,
    ) -> anyhow::Result<ConnectorConfigRead<McpRequestTargetRecord>> {
        self.read_connector_config(|conn| Self::mcp_request_target_in_snapshot(conn, server_id))
    }

    fn mcp_request_target_in_snapshot(
        conn: &Connection,
        server_id: &str,
    ) -> anyhow::Result<McpRequestTargetRecord> {
        anyhow::ensure!(
            !conn.is_autocommit(),
            "MCP request target snapshot requires a caller-owned transaction"
        );

        let row_bytes = conn
            .query_row(
                "SELECT length(CAST(id AS BLOB)) + length(CAST(name AS BLOB)) +
                        length(CAST(kind AS BLOB)) +
                        length(CAST(COALESCE(command, '') AS BLOB)) +
                        length(CAST(COALESCE(args_json, '') AS BLOB)) +
                        length(CAST(COALESCE(env_json, '') AS BLOB)) +
                        length(CAST(COALESCE(env_credentials_json, '') AS BLOB)) +
                        length(CAST(COALESCE(url, '') AS BLOB))
                 FROM mcp_servers WHERE id = ?1",
                [server_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|_| anyhow::anyhow!("MCP request target server preflight failed"))?;
        let Some(row_bytes) = row_bytes else {
            return Ok(McpRequestTargetRecord {
                server: None,
                credential_locations: Vec::new(),
                oauth_bindings: Vec::new(),
            });
        };
        let row_bytes = usize::try_from(row_bytes)
            .map_err(|_| anyhow::anyhow!("MCP request target server byte count is invalid"))?;
        anyhow::ensure!(
            row_bytes <= mcp_store::MCP_SERVER_POINT_BYTES_MAX,
            "MCP request target server byte limit exceeded"
        );

        let (
            kind,
            command_present,
            command_nonempty,
            url_present,
            url_nonempty,
            inherit_env,
            args_valid,
            plain_env_valid,
            credential_env_valid,
        ): (String, bool, bool, bool, bool, bool, bool, bool, bool) = conn
            .query_row(
                "SELECT kind,
                        command IS NOT NULL,
                        length(trim(COALESCE(command, ''))) > 0,
                        url IS NOT NULL,
                        length(trim(COALESCE(url, ''))) > 0,
                        inherit_env,
                        CASE WHEN args_json IS NULL THEN 1
                             WHEN json_valid(args_json) = 0 THEN 0
                             WHEN json_type(args_json) = 'array' THEN 1 ELSE 0 END,
                        CASE WHEN env_json IS NULL THEN 1
                             WHEN json_valid(env_json) = 0 THEN 0
                             WHEN json_type(env_json) = 'object' THEN 1 ELSE 0 END,
                        CASE WHEN env_credentials_json IS NULL THEN 1
                             WHEN json_valid(env_credentials_json) = 0 THEN 0
                             WHEN json_type(env_credentials_json) = 'object' THEN 1 ELSE 0 END
                 FROM mcp_servers WHERE id = ?1",
                [server_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                    ))
                },
            )
            .map_err(|_| anyhow::anyhow!("MCP request target server shape read failed"))?;
        anyhow::ensure!(
            args_valid && plain_env_valid && credential_env_valid,
            "MCP request target server JSON shape is invalid"
        );

        let (
            argument_count,
            argument_text_count,
            plain_count,
            plain_unique_count,
            plain_text_count,
            credential_count,
            credential_unique_count,
            credential_text_count,
        ): (i64, i64, i64, i64, i64, i64, i64, i64) = conn
            .query_row(
                "SELECT
                    (SELECT COUNT(*)
                       FROM json_each(COALESCE(mcp_servers.args_json, '[]'))),
                    (SELECT COUNT(*)
                       FROM json_each(COALESCE(mcp_servers.args_json, '[]'))
                      WHERE type = 'text'),
                    (SELECT COUNT(*)
                       FROM json_each(COALESCE(mcp_servers.env_json, '{}'))),
                    (SELECT COUNT(DISTINCT key)
                       FROM json_each(COALESCE(mcp_servers.env_json, '{}'))),
                    (SELECT COUNT(*)
                       FROM json_each(COALESCE(mcp_servers.env_json, '{}'))
                      WHERE type = 'text'),
                    (SELECT COUNT(*)
                       FROM json_each(COALESCE(mcp_servers.env_credentials_json, '{}'))),
                    (SELECT COUNT(DISTINCT key)
                       FROM json_each(COALESCE(mcp_servers.env_credentials_json, '{}'))),
                    (SELECT COUNT(*)
                       FROM json_each(COALESCE(mcp_servers.env_credentials_json, '{}'))
                      WHERE type = 'text')
                 FROM mcp_servers WHERE id = ?1",
                [server_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .map_err(|_| anyhow::anyhow!("MCP request target server item preflight failed"))?;
        let counts = [
            argument_count,
            argument_text_count,
            plain_count,
            plain_unique_count,
            plain_text_count,
            credential_count,
            credential_unique_count,
            credential_text_count,
        ]
        .map(|count| {
            usize::try_from(count)
                .map_err(|_| anyhow::anyhow!("MCP request target item count is invalid"))
        });
        let [
            argument_count,
            argument_text_count,
            plain_count,
            plain_unique_count,
            plain_text_count,
            credential_count,
            credential_unique_count,
            credential_text_count,
        ] = counts;
        let argument_count = argument_count?;
        let argument_text_count = argument_text_count?;
        let plain_count = plain_count?;
        let plain_unique_count = plain_unique_count?;
        let plain_text_count = plain_text_count?;
        let credential_count = credential_count?;
        let credential_unique_count = credential_unique_count?;
        let credential_text_count = credential_text_count?;
        anyhow::ensure!(
            argument_count == argument_text_count
                && plain_count == plain_unique_count
                && plain_count == plain_text_count
                && credential_count == credential_unique_count
                && credential_count == credential_text_count,
            "MCP request target contains malformed or duplicate persisted values"
        );
        let retained_stdio_items = argument_count
            .checked_add(plain_count)
            .and_then(|count| count.checked_add(credential_count))
            .ok_or_else(|| anyhow::anyhow!("MCP request target item count overflow"))?;

        match kind.as_str() {
            "stdio" => {
                anyhow::ensure!(
                    command_present && command_nonempty && !url_present,
                    "stdio MCP request target transport shape is invalid"
                );
                anyhow::ensure!(
                    credential_count <= MCP_REQUEST_TARGET_CREDENTIAL_LIMIT_MAX
                        && retained_stdio_items <= MCP_REQUEST_TARGET_CREDENTIAL_LIMIT_MAX,
                    "MCP request target credential item limit exceeded"
                );
            }
            "http" => {
                anyhow::ensure!(
                    !command_present
                        && url_present
                        && url_nonempty
                        && inherit_env
                        && argument_count == 0
                        && plain_count == 0
                        && credential_count == 0,
                    "HTTP MCP request target transport shape is invalid"
                );
            }
            _ => anyhow::bail!("MCP request target transport kind is invalid"),
        }

        let server = mcp_store::server_in_snapshot(conn, server_id)
            .map_err(|_| anyhow::anyhow!("MCP request target server data is invalid"))?
            .ok_or_else(|| anyhow::anyhow!("MCP request target server disappeared"))?;
        Self::validate_mcp_request_target_server(&server, server_id)?;

        match kind.as_str() {
            "stdio" => {
                anyhow::ensure!(
                    server.env_secrets.len() == credential_count,
                    "MCP request target credential count changed inside snapshot"
                );
                let credential_ids = server
                    .env_secrets
                    .iter()
                    .map(|(_, credential_id)| credential_id.as_str())
                    .collect::<Vec<_>>();
                let credential_locations =
                    Self::credential_locations_in_snapshot(conn, &credential_ids)?;
                Ok(McpRequestTargetRecord {
                    server: Some(server),
                    credential_locations,
                    oauth_bindings: Vec::new(),
                })
            }
            "http" => {
                let oauth_bindings =
                    Self::credential_oauth_bindings_for_server_in_snapshot(conn, server_id)
                        .map_err(|_| {
                            anyhow::anyhow!("MCP request target OAuth bindings are invalid")
                        })?;
                anyhow::ensure!(
                    oauth_bindings.len() <= 2,
                    "MCP request target OAuth binding item limit exceeded"
                );
                for binding in &oauth_bindings {
                    anyhow::ensure!(
                        binding.keyring_service == secret::KEYRING_SERVICE,
                        "MCP request target OAuth keyring service is invalid"
                    );
                }
                Ok(McpRequestTargetRecord {
                    server: Some(server),
                    credential_locations: Vec::new(),
                    oauth_bindings,
                })
            }
            _ => unreachable!("transport kind was validated before materialization"),
        }
    }

    fn validate_mcp_request_target_server(
        server: &mcp_store::McpServerRow,
        expected_server_id: &str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            server.id == expected_server_id
                && !server.id.is_empty()
                && !server.name.trim().is_empty()
                && !server.id.contains('\0')
                && !server.name.contains('\0'),
            "MCP request target server identity is invalid"
        );
        mcp_store::validate_server_args_for_persistence(&server.args)
            .map_err(|_| anyhow::anyhow!("MCP request target arguments are invalid"))?;
        mcp_store::validate_server_env_for_persistence(&server.env_plain, &server.env_secrets)
            .map_err(|_| anyhow::anyhow!("MCP request target environment is invalid"))?;
        match server.kind.as_str() {
            "stdio" => {
                let command = server.command.as_deref().unwrap_or_default();
                anyhow::ensure!(
                    !command.trim().is_empty() && !command.contains('\0') && server.url.is_none(),
                    "stdio MCP request target data is invalid"
                );
            }
            "http" => {
                let url = server.url.as_deref().unwrap_or_default();
                anyhow::ensure!(
                    server.command.is_none()
                        && server.args.is_empty()
                        && server.env_plain.is_empty()
                        && server.env_secrets.is_empty()
                        && server.inherit_env
                        && !url.trim().is_empty()
                        && !url.contains('\0'),
                    "HTTP MCP request target data is invalid"
                );
            }
            _ => anyhow::bail!("MCP request target transport kind is invalid"),
        }
        Ok(())
    }

    fn credential_locations_in_snapshot(
        conn: &Connection,
        credential_ids: &[&str],
    ) -> anyhow::Result<Vec<CredentialSecretLocation>> {
        anyhow::ensure!(
            credential_ids.len() <= MCP_REQUEST_TARGET_CREDENTIAL_LIMIT_MAX,
            "MCP request target credential item limit exceeded"
        );
        if credential_ids.is_empty() {
            return Ok(Vec::new());
        }
        let requested_json = serde_json::to_string(credential_ids)
            .map_err(|_| anyhow::anyhow!("MCP request target credential index is invalid"))?;
        let (requested, matched, aggregate_bytes, maximum_row_bytes): (i64, i64, i64, i64) = conn
            .query_row(
                "WITH requested AS (
                     SELECT CAST(key AS INTEGER) AS ordinal, value AS credential_id
                       FROM json_each(?1)
                 )
                 SELECT COUNT(*), COUNT(credentials.id),
                        COALESCE(SUM(
                            length(CAST(credentials.keyring_service AS BLOB)) +
                            length(CAST(credentials.keyring_username AS BLOB))
                        ), 0),
                        COALESCE(MAX(
                            length(CAST(credentials.keyring_service AS BLOB)) +
                            length(CAST(credentials.keyring_username AS BLOB))
                        ), 0)
                   FROM requested
                   LEFT JOIN credentials ON credentials.id = requested.credential_id",
                [&requested_json],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(|_| anyhow::anyhow!("MCP request target credential preflight failed"))?;
        let requested = usize::try_from(requested)
            .map_err(|_| anyhow::anyhow!("MCP request target credential count is invalid"))?;
        let matched = usize::try_from(matched)
            .map_err(|_| anyhow::anyhow!("MCP request target credential count is invalid"))?;
        let aggregate_bytes = usize::try_from(aggregate_bytes)
            .map_err(|_| anyhow::anyhow!("MCP request target credential bytes are invalid"))?;
        let maximum_row_bytes = usize::try_from(maximum_row_bytes)
            .map_err(|_| anyhow::anyhow!("MCP request target credential bytes are invalid"))?;
        anyhow::ensure!(
            requested == credential_ids.len() && matched == requested,
            "MCP request target credential reference is missing"
        );
        anyhow::ensure!(
            maximum_row_bytes <= CREDENTIAL_SECRET_LOCATION_BYTES_MAX,
            "MCP request target credential row byte limit exceeded"
        );
        anyhow::ensure!(
            aggregate_bytes <= MCP_REQUEST_TARGET_CREDENTIAL_BYTES_MAX,
            "MCP request target credential aggregate byte limit exceeded"
        );

        let locations = {
            let mut statement = conn
                .prepare_cached(
                    "WITH requested AS (
                         SELECT CAST(key AS INTEGER) AS ordinal, value AS credential_id
                           FROM json_each(?1)
                     )
                     SELECT credentials.keyring_service, credentials.keyring_username
                       FROM requested
                       JOIN credentials ON credentials.id = requested.credential_id
                      ORDER BY requested.ordinal",
                )
                .map_err(|_| {
                    anyhow::anyhow!("MCP request target credential materialization failed")
                })?;
            let rows = statement
                .query_map([&requested_json], |row| {
                    Ok(CredentialSecretLocation {
                        keyring_service: row.get(0)?,
                        keyring_username: row.get(1)?,
                    })
                })
                .map_err(|_| {
                    anyhow::anyhow!("MCP request target credential materialization failed")
                })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(|_| {
                anyhow::anyhow!("MCP request target credential materialization failed")
            })?
        };
        anyhow::ensure!(
            locations.len() == credential_ids.len(),
            "MCP request target credential materialization count mismatch"
        );
        for (credential_id, location) in credential_ids.iter().zip(&locations) {
            anyhow::ensure!(
                location.keyring_service == secret::KEYRING_SERVICE,
                "MCP request target credential keyring service is invalid"
            );
            validate_owned_physical_secret_slot(credential_id, &location.keyring_username)
                .map_err(|_| anyhow::anyhow!("MCP request target credential pointer is invalid"))?;
        }
        Ok(locations)
    }

    pub fn insert_mcp_server(&self, row: &mcp_store::McpServerRow) -> anyhow::Result<()> {
        mcp_store::insert_server(&self.conn, row)
    }

    /// Full transactional server save used by the Connector repository adapter.
    pub fn save_mcp_server(
        &mut self,
        row: &mcp_store::McpServerRow,
    ) -> anyhow::Result<mcp_store::McpServerSaveOutcome> {
        mcp_store::save_server(&mut self.conn, row)
    }

    pub fn save_mcp_server_revision_cas(
        &mut self,
        expected: ConnectorConfigRevision,
        row: &mcp_store::McpServerRow,
    ) -> anyhow::Result<ConnectorConfigCas<mcp_store::McpServerSaveOutcome>> {
        self.write_connector_config_cas(expected, |conn| {
            mcp_store::save_server_in_transaction(conn, row)
        })
    }

    /// Reject active agent-proxy references and delete the server plus live MCP metadata in one
    /// IMMEDIATE transaction. Durable audit history intentionally remains untouched.
    pub fn delete_mcp_server(&mut self, server_id: &str, resolved_at: i64) -> anyhow::Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = Self::delete_mcp_server_in_transaction(&tx, server_id, resolved_at)?;
        tx.commit().context("MCP server 삭제 commit 실패")?;
        Ok(deleted)
    }

    fn delete_mcp_server_in_transaction(
        conn: &Connection,
        server_id: &str,
        resolved_at: i64,
    ) -> anyhow::Result<bool> {
        let referenced = conn
            .query_row(
                "SELECT 1 FROM agent_configs
                 WHERE deleted_at IS NULL AND mcp_proxy_server_id = ?1
                 LIMIT 1",
                [server_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        anyhow::ensure!(
            !referenced,
            "활성 agent proxy가 참조 중인 MCP server는 삭제할 수 없습니다: {server_id}"
        );
        mcp_store::delete_server_in_transaction(conn, server_id, resolved_at)
    }

    pub fn delete_mcp_server_revision_cas(
        &mut self,
        expected: ConnectorConfigRevision,
        server_id: &str,
        resolved_at: i64,
    ) -> anyhow::Result<ConnectorConfigCas<bool>> {
        self.write_connector_config_cas(expected, |conn| {
            Self::delete_mcp_server_in_transaction(conn, server_id, resolved_at)
        })
    }

    /// canonical URL 기준 멱등 등록. built-in provider(Slack 등)의 중복 행을 막는다.
    pub fn ensure_mcp_server_by_url(
        &mut self,
        row: &mcp_store::McpServerRow,
    ) -> anyhow::Result<mcp_store::McpServerRow> {
        mcp_store::ensure_server_by_url(&mut self.conn, row)
    }

    pub fn ensure_mcp_server_by_url_revision_cas(
        &mut self,
        expected: ConnectorConfigRevision,
        row: &mcp_store::McpServerRow,
    ) -> anyhow::Result<ConnectorConfigCas<mcp_store::McpServerRow>> {
        self.write_connector_config_cas(expected, |conn| {
            mcp_store::ensure_server_by_url_in_transaction(conn, row)
        })
    }

    /// Canonical-URL reconnect path. Exactly one existing row is re-enabled in the same expected-
    /// revision transaction; historical duplicate canonical rows fail before any mutation.
    pub fn ensure_enabled_mcp_server_by_url_revision_cas(
        &mut self,
        expected: ConnectorConfigRevision,
        row: &mcp_store::McpServerRow,
    ) -> anyhow::Result<ConnectorConfigCas<mcp_store::McpServerRow>> {
        self.write_connector_config_cas(expected, |conn| {
            mcp_store::ensure_enabled_server_by_url_in_transaction(conn, row)
        })
    }

    /// import 대상 전체를 all-or-nothing으로 저장한다.
    pub fn insert_mcp_servers_batch(
        &mut self,
        rows: &[mcp_store::McpServerRow],
    ) -> anyhow::Result<usize> {
        mcp_store::insert_servers_batch(&mut self.conn, rows)
    }

    pub fn insert_mcp_servers_batch_revision_cas(
        &mut self,
        expected: ConnectorConfigRevision,
        rows: &[mcp_store::McpServerRow],
    ) -> anyhow::Result<ConnectorConfigCas<usize>> {
        self.write_connector_config_cas(expected, |conn| {
            mcp_store::insert_servers_batch_in_transaction(conn, rows)
        })
    }

    /// http MCP 서버의 url 갱신 (H3). 호출측(UI)이 Allow 규칙 초기화 +
    /// mcp_tools 캐시 무효화 + 최초 연결 재확인을 함께 수행한다.
    pub fn update_mcp_server_url(&self, server_id: &str, url: &str) -> anyhow::Result<()> {
        mcp_store::update_server_url(&self.conn, server_id, url)
    }

    /// 연결 테스트로 발견한 tools를 교체 저장 (PR-17).
    pub fn replace_mcp_tools(
        &mut self,
        server_id: &str,
        rows: &[mcp_store::McpToolRow],
    ) -> anyhow::Result<()> {
        mcp_store::replace_tools_for_server(&mut self.conn, server_id, rows)
    }

    pub fn replace_mcp_tools_revision_cas(
        &mut self,
        expected: ConnectorConfigRevision,
        server_id: &str,
        rows: &[mcp_store::McpToolRow],
    ) -> anyhow::Result<ConnectorConfigCas<()>> {
        self.write_connector_config_cas(expected, |conn| {
            mcp_store::replace_tools_for_server_in_transaction(conn, server_id, rows)
        })
    }

    /// 저장된 tool 목록 (도구 실행 UI용).
    pub fn list_mcp_tools(&self, server_id: &str) -> anyhow::Result<Vec<mcp_store::McpToolRow>> {
        mcp_store::list_tools_for_server(&self.conn, server_id)
    }

    /// Exact invoke preparation lookup; no schema, description, or full tool list is loaded.
    pub fn mcp_tool_name(&self, server_id: &str, tool_id: &str) -> anyhow::Result<Option<String>> {
        mcp_store::tool_name(&self.conn, server_id, tool_id)
    }

    pub fn mcp_tool_name_versioned(
        &self,
        server_id: &str,
        tool_id: &str,
    ) -> anyhow::Result<ConnectorConfigRead<Option<String>>> {
        self.read_connector_config(|conn| {
            mcp_store::tool_name_in_snapshot(conn, server_id, tool_id)
        })
    }

    /// Same-snapshot bounded tool page with permission rows joined in one page query.
    pub fn mcp_tool_page(
        &self,
        server_id: &str,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<mcp_store::McpToolPage> {
        mcp_store::tool_page(&self.conn, server_id, offset, limit)
    }

    pub fn mcp_tool_page_versioned(
        &self,
        server_id: &str,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<ConnectorConfigRead<mcp_store::McpToolPage>> {
        self.read_connector_config(|conn| {
            mcp_store::tool_page_in_snapshot(conn, server_id, offset, limit)
        })
    }

    /// 저장된 tool 권한 규칙 전체 (앱 시작 시 PermissionPolicy로 로드).
    pub fn list_permission_rules(&self) -> anyhow::Result<Vec<PermissionRuleRow>> {
        mcp_store::list_permission_rules(&self.conn)
    }

    /// Authorization hot-path point lookup. Missing rows are interpreted as Ask by the service.
    pub fn permission_rule(
        &self,
        server_id: &str,
        tool_name: &str,
    ) -> anyhow::Result<Option<PermissionRuleRow>> {
        mcp_store::permission_rule(&self.conn, server_id, tool_name)
    }

    pub fn permission_rule_versioned(
        &self,
        server_id: &str,
        tool_name: &str,
    ) -> anyhow::Result<ConnectorConfigRead<Option<PermissionRuleRow>>> {
        self.read_connector_config(|conn| {
            mcp_store::permission_rule_in_snapshot(conn, server_id, tool_name)
        })
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

    /// Resolve a tool id to its bounded persisted name and mutate that exact permission row inside
    /// one IMMEDIATE expected-revision transaction. `ask` deletes the row; unknown rules fail.
    pub fn set_permission_by_tool_id_revision_cas(
        &mut self,
        expected: ConnectorConfigRevision,
        server_id: &str,
        tool_id: &str,
        rule: &str,
        approved_schema_hash: Option<&str>,
    ) -> anyhow::Result<ConnectorConfigCas<()>> {
        anyhow::ensure!(
            matches!(rule, "ask" | "allow" | "deny"),
            "알 수 없는 MCP permission rule: {rule}"
        );
        anyhow::ensure!(
            rule == "allow" || approved_schema_hash.is_none(),
            "Allow 이외 permission에는 schema hash를 저장할 수 없습니다"
        );
        self.write_connector_config_cas(expected, |conn| {
            let tool_name = mcp_store::tool_name_in_snapshot(conn, server_id, tool_id)?
                .with_context(|| format!("MCP tool permission 대상 없음: {server_id}/{tool_id}"))?;
            if rule == "ask" {
                mcp_store::delete_permission_rule(conn, server_id, &tool_name)
            } else {
                mcp_store::upsert_permission_rule(
                    conn,
                    server_id,
                    &tool_name,
                    rule,
                    approved_schema_hash,
                )
            }
        })
    }

    /// 라이브 승인 요청 등록 (deppy-mcp-proxy → GUI). 세부 계약은 mcp_store 문서 참조.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_pending_approval(
        &self,
        id: &str,
        server_id: &str,
        tool_name: &str,
        arguments_preview: &str,
        schema_hash: Option<&str>,
        created_at: i64,
        pane_id: Option<&str>,
    ) -> anyhow::Result<()> {
        mcp_store::insert_pending_approval(
            &self.conn,
            id,
            server_id,
            tool_name,
            arguments_preview,
            schema_hash,
            created_at,
            pane_id,
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

    /// Bounded pending page for event-driven approval wake handling. The storage layer performs
    /// its byte preflight before materializing any SQLite text.
    pub fn list_pending_approvals_bounded(
        &self,
        limit: usize,
    ) -> anyhow::Result<mcp_store::PendingApprovalPage> {
        mcp_store::list_pending_approvals_bounded(&self.conn, limit)
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

    /// Legacy compatibility entry point for callers not yet moved to app-held startup ownership.
    /// Production app bootstrap must use [`Self::deny_session_scoped_pending_approvals_owned`]
    /// after atomically acquiring [`Self::acquire_pending_approval_owner`].
    pub fn deny_session_scoped_pending_approvals(&self, resolved_at: i64) -> anyhow::Result<usize> {
        mcp_store::deny_session_scoped_pending_approvals(&self.conn, resolved_at)
    }

    /// Non-blockingly acquires the single startup-reconciliation owner for this physical DB.
    /// The dedicated namespace contains exactly one lock file and never shares authorization
    /// executor stripes. Dropping the returned non-Clone token releases ownership.
    pub fn acquire_pending_approval_owner(&self) -> anyhow::Result<ActivePendingApprovalOwner> {
        anyhow::ensure!(
            !self.authorization_db_identity.starts_with("memory:"),
            PENDING_APPROVAL_OWNER_FILE_BACKED_REQUIRED
        );
        acquire_pending_approval_owner_for_identity(&self.authorization_db_identity)
    }

    /// Denies bounded session-owned pending rows only while the caller holds the exact physical
    /// DB's lifetime owner. Cross-DB tokens fail before opening the reconciliation transaction.
    pub fn deny_session_scoped_pending_approvals_owned(
        &self,
        owner: &ActivePendingApprovalOwner,
        resolved_at: i64,
    ) -> anyhow::Result<usize> {
        anyhow::ensure!(
            self.authorization_db_identity == owner.db_identity,
            PENDING_APPROVAL_OWNER_DB_MISMATCH
        );
        mcp_store::deny_session_scoped_pending_approvals(&self.conn, resolved_at)
    }

    /// Ends pending approvals owned by one exact runtime session. The static
    /// `session_closed` reason is represented durably by the terminal `denied` state and
    /// `resolved_at`; no caller-controlled reason or identifier is persisted or logged.
    ///
    /// The candidate probe and update share one IMMEDIATE transaction. More than 256 candidates
    /// fails closed without changing any row, resolved approvals remain first-writer-wins, and a
    /// repeated cleanup returns zero.
    pub fn deny_pending_approvals_for_session(
        &self,
        session_key: &str,
        resolved_at: i64,
    ) -> anyhow::Result<usize> {
        validate_pending_approval_session_key(session_key)?;

        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .context(SESSION_CLOSED_APPROVAL_ERROR_CODE)?;
        let probe_limit = i64::try_from(PENDING_APPROVAL_SESSION_CLEANUP_LIMIT_MAX + 1)
            .context(SESSION_CLOSED_APPROVAL_ERROR_CODE)?;
        let candidate_count: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM (
                     SELECT 1 FROM pending_approvals
                     WHERE pane_id = ?1 AND status = 'pending'
                     LIMIT ?2
                 )",
                (session_key, probe_limit),
                |row| row.get(0),
            )
            .context(SESSION_CLOSED_APPROVAL_ERROR_CODE)?;
        let candidate_count =
            usize::try_from(candidate_count).context(SESSION_CLOSED_APPROVAL_ERROR_CODE)?;
        anyhow::ensure!(
            candidate_count <= PENDING_APPROVAL_SESSION_CLEANUP_LIMIT_MAX,
            SESSION_CLOSED_APPROVAL_ERROR_CODE
        );

        let affected = tx
            .execute(
                "UPDATE pending_approvals
                 SET status = 'denied', remember = 0, resolved_at = ?2
                 WHERE pane_id = ?1 AND status = 'pending'",
                (session_key, resolved_at),
            )
            .context(SESSION_CLOSED_APPROVAL_ERROR_CODE)?;
        anyhow::ensure!(
            affected == candidate_count,
            SESSION_CLOSED_APPROVAL_ERROR_CODE
        );
        tx.commit().context(SESSION_CLOSED_APPROVAL_ERROR_CODE)?;
        Ok(affected)
    }

    pub fn prune_resolved_approvals(
        &self,
        resolved_before_epoch_secs: i64,
    ) -> anyhow::Result<usize> {
        mcp_store::prune_resolved_approvals(&self.conn, resolved_before_epoch_secs)
    }

    pub fn insert_relay_pending_device(
        &self,
        pending: &RelayPendingDeviceRow,
        trusted_now: i64,
    ) -> anyhow::Result<RelayPendingInsert> {
        validate_relay_pending_device(pending)?;
        anyhow::ensure!(
            pending.issued_at <= trusted_now && trusted_now < pending.pairing_expires_at,
            "relay pending device trusted timestamp invalid"
        );
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .context("relay pending device transaction failed")?;
        tx.execute(
            "DELETE FROM relay_pending_devices WHERE pairing_expires_at <= ?1",
            [trusted_now],
        )
        .context("relay pending device expiry cleanup failed")?;
        // 같은 기기(device_id **그리고** 공개키가 모두 같음)가 미완의 의식을 버리고 다시
        // 페어링하는 경우는 새 검증 승인이 옛것을 대체한다 — 5분을 기다리게 할 이유가 없다.
        // 그 밖의 부분 겹침(id만 같거나 키만 같음)은 다른 주체가 끼어든 것이므로 결정적으로
        // 거절한다. 불투명 오류가 아니라 호출자가 분기할 수 있는 값이다.
        let superseded = tx.execute(
            "DELETE FROM relay_pending_devices
             WHERE device_id = ?1 AND identity_public_sec1 = ?2",
            rusqlite::params![
                pending.device_id.as_slice(),
                pending.identity_public_sec1.as_slice(),
            ],
        )?;
        let conflict: bool = tx.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM relay_pending_devices
                 WHERE pairing_id = ?1 OR device_id = ?2 OR identity_public_sec1 = ?3
             )",
            rusqlite::params![
                pending.pairing_id.as_slice(),
                pending.device_id.as_slice(),
                pending.identity_public_sec1.as_slice(),
            ],
            |row| row.get(0),
        )?;
        if conflict {
            // 대체로 지운 행이 있었더라도 롤백된다 — 트랜잭션이 커밋되지 않는다.
            let _ = superseded;
            return Ok(RelayPendingInsert::Conflict);
        }
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM relay_pending_devices", [], |row| {
            row.get(0)
        })?;
        let count = usize::try_from(count).context("relay pending device count invalid")?;
        if count >= RELAY_PENDING_DEVICE_ROWS_MAX {
            tx.commit()?;
            return Ok(RelayPendingInsert::LimitReached);
        }
        tx.execute(
            "INSERT INTO relay_pending_devices (
                 pairing_id, device_id, identity_public_sec1, display_name,
                 permission_view, permission_input, permission_upload, permission_approval,
                 issued_at, pairing_expires_at, device_expires_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                pending.pairing_id.as_slice(),
                pending.device_id.as_slice(),
                pending.identity_public_sec1.as_slice(),
                pending.display_name,
                pending.permission_view,
                pending.permission_input,
                pending.permission_upload,
                pending.permission_approval,
                pending.issued_at,
                pending.pairing_expires_at,
                pending.device_expires_at,
            ],
        )
        .context("relay pending device insert failed")?;
        tx.commit().context("relay pending device commit failed")?;
        Ok(RelayPendingInsert::Stored)
    }

    /// 승인은 발행 트랜잭션과 같은 스냅샷 안에서 `expected_identity_public_sec1`을 다시
    /// 확인한다. 앱 어댑터가 곡선 검증을 마친 뒤와 이 트랜잭션 사이에 외부에서 행을
    /// 바꿔치기해도(SQLite는 곡선을 못 본다) 아무것도 발행되지 않고 롤백된다.
    pub fn approve_relay_pending_device(
        &self,
        pairing_id: &[u8; RELAY_ID_BYTES],
        expected_identity_public_sec1: &[u8; RELAY_PUBLIC_KEY_BYTES],
        approved_at: i64,
    ) -> anyhow::Result<RelayDeviceApproval> {
        anyhow::ensure!(approved_at >= 0, "relay approval timestamp invalid");
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .context("relay approval transaction failed")?;
        let pending = {
            let mut statement = tx.prepare(
                "SELECT pairing_id, device_id, identity_public_sec1, display_name,
                        permission_view, permission_input, permission_upload, permission_approval,
                        issued_at, pairing_expires_at, device_expires_at
                 FROM relay_pending_devices WHERE pairing_id = ?1",
            )?;
            let mut rows = statement.query([pairing_id.as_slice()])?;
            match rows.next()? {
                Some(row) => Some(relay_pending_from_row(row)?),
                None => None,
            }
        };
        let Some(pending) = pending else {
            tx.commit()?;
            return Ok(RelayDeviceApproval::NotFound);
        };
        anyhow::ensure!(
            pending.identity_public_sec1 == *expected_identity_public_sec1,
            "relay pending device changed between validation and approval"
        );
        anyhow::ensure!(
            approved_at >= pending.issued_at,
            "relay approval clock rollback"
        );
        if approved_at >= pending.pairing_expires_at {
            tx.execute(
                "DELETE FROM relay_pending_devices WHERE pairing_id = ?1",
                [pairing_id.as_slice()],
            )?;
            tx.commit()?;
            return Ok(RelayDeviceApproval::Expired);
        }

        let existing: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM relay_devices WHERE device_id = ?1)",
            [pending.device_id.as_slice()],
            |row| row.get(0),
        )?;
        // 같은 공개키를 가진 옛 행(다른 device_id)은 같은 폰의 이전 페어링이다. 취소된 뒤
        // 새 device_id로 다시 페어링하는 정상 경로이므로 옛 행을 지우고 새로 발행한다 —
        // 그러지 않으면 identity UNIQUE에 걸려 불투명하게 실패하고 pending 행만 남는다.
        tx.execute(
            "DELETE FROM relay_devices
             WHERE identity_public_sec1 = ?1 AND device_id != ?2",
            rusqlite::params![
                pending.identity_public_sec1.as_slice(),
                pending.device_id.as_slice(),
            ],
        )?;
        if !existing {
            // 상한은 **살아 있는** 기기 수다. 취소된 행까지 세면 64대를 페어링하고 전부
            // 취소한 뒤 65번째부터 영영 막힌다. 표 자체의 크기도 상한을 지키도록, 자리가
            // 없으면 가장 오래전에 취소된 행부터 필요한 만큼만 지운다.
            let active: i64 = tx.query_row(
                "SELECT COUNT(*) FROM relay_devices WHERE revoked_at IS NULL",
                [],
                |row| row.get(0),
            )?;
            if usize::try_from(active).context("relay device count invalid")?
                >= RELAY_DEVICE_ROWS_MAX
            {
                return Ok(RelayDeviceApproval::DeviceLimitReached);
            }
            let total: i64 =
                tx.query_row("SELECT COUNT(*) FROM relay_devices", [], |row| row.get(0))?;
            let total = usize::try_from(total).context("relay device count invalid")?;
            if total >= RELAY_DEVICE_ROWS_MAX {
                let reclaim = i64::try_from(total - RELAY_DEVICE_ROWS_MAX + 1)?;
                tx.execute(
                    "DELETE FROM relay_devices WHERE device_id IN (
                         SELECT device_id FROM relay_devices
                         WHERE revoked_at IS NOT NULL
                         ORDER BY revoked_at ASC, device_id ASC
                         LIMIT ?1
                     )",
                    [reclaim],
                )?;
            }
        }

        // 시각과 별개인 세대라 같은 초의 재승인·삭제 후 재생성도 이전 채널과 구분된다.
        let authorization_epoch = *uuid::Uuid::new_v4().as_bytes();
        tx.execute(
            "INSERT INTO relay_devices (
                 device_id, identity_public_sec1, display_name,
                 permission_view, permission_input, permission_upload, permission_approval,
                 issued_at, device_expires_at, last_seen_at, revoked_at, authorization_epoch
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, NULL, ?10)
             ON CONFLICT(device_id) DO UPDATE SET
                 identity_public_sec1 = excluded.identity_public_sec1,
                 display_name = excluded.display_name,
                 permission_view = excluded.permission_view,
                 permission_input = excluded.permission_input,
                 permission_upload = excluded.permission_upload,
                 permission_approval = excluded.permission_approval,
                 issued_at = excluded.issued_at,
                 device_expires_at = excluded.device_expires_at,
                 last_seen_at = NULL,
                 revoked_at = NULL,
                 reconnect_verifier = NULL,
                 authorization_epoch = excluded.authorization_epoch",
            rusqlite::params![
                pending.device_id.as_slice(),
                pending.identity_public_sec1.as_slice(),
                pending.display_name,
                pending.permission_view,
                pending.permission_input,
                pending.permission_upload,
                pending.permission_approval,
                approved_at,
                pending.device_expires_at,
                authorization_epoch.as_slice(),
            ],
        )
        .context("relay device approval publish failed")?;
        let deleted = tx.execute(
            "DELETE FROM relay_pending_devices WHERE pairing_id = ?1",
            [pairing_id.as_slice()],
        )?;
        anyhow::ensure!(deleted == 1, "relay approval consume failed");
        tx.commit().context("relay approval commit failed")?;

        Ok(RelayDeviceApproval::Approved(RelayDeviceRow {
            device_id: pending.device_id,
            identity_public_sec1: pending.identity_public_sec1,
            display_name: pending.display_name,
            permission_view: pending.permission_view,
            permission_input: pending.permission_input,
            permission_upload: pending.permission_upload,
            permission_approval: pending.permission_approval,
            issued_at: approved_at,
            device_expires_at: pending.device_expires_at,
            last_seen_at: None,
            revoked_at: None,
            authorization_epoch,
        }))
    }

    pub fn relay_pending_device_count(&self) -> anyhow::Result<usize> {
        let count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM relay_pending_devices", [], |row| {
                    row.get(0)
                })?;
        usize::try_from(count).context("relay pending device count invalid")
    }

    /// 승인 전 검증용 단건 읽기. 앱 어댑터가 곡선 검증을 통과시킨 뒤에야 승인을
    /// 호출할 수 있도록, 변형(승인) 이전에 원본 행을 그대로 돌려준다.
    pub fn relay_pending_device(
        &self,
        pairing_id: &[u8; RELAY_ID_BYTES],
    ) -> anyhow::Result<Option<RelayPendingDeviceRow>> {
        let mut statement = self.conn.prepare_cached(
            "SELECT pairing_id, device_id, identity_public_sec1, display_name,
                    permission_view, permission_input, permission_upload, permission_approval,
                    issued_at, pairing_expires_at, device_expires_at
             FROM relay_pending_devices WHERE pairing_id = ?1",
        )?;
        let mut rows = statement.query([pairing_id.as_slice()])?;
        match rows.next()? {
            Some(row) => Ok(Some(relay_pending_from_row(row)?)),
            None => Ok(None),
        }
    }

    /// 재시작 정리. 검증된 메모리 승인이 사라진 뒤 남은 pending 행은 페어링 증거가
    /// 될 수 없으므로 앱 시작 시 전부 지운다. 지운 행 수를 돌려준다.
    pub fn delete_all_relay_pending_devices(&self) -> anyhow::Result<usize> {
        let deleted = self
            .conn
            .execute("DELETE FROM relay_pending_devices", [])
            .context("relay pending device restart purge failed")?;
        Ok(deleted)
    }

    pub fn relay_device(
        &self,
        device_id: &[u8; RELAY_ID_BYTES],
    ) -> anyhow::Result<Option<RelayDeviceRow>> {
        let mut statement = self.conn.prepare_cached(
            "SELECT device_id, identity_public_sec1, display_name,
                    permission_view, permission_input, permission_upload, permission_approval,
                    issued_at, device_expires_at, last_seen_at, revoked_at, authorization_epoch
             FROM relay_devices WHERE device_id = ?1",
        )?;
        let mut rows = statement.query([device_id.as_slice()])?;
        match rows.next()? {
            Some(row) => Ok(Some(relay_device_from_row(row)?)),
            None => Ok(None),
        }
    }

    /// 승인된 동일 신원에만 검증자를 처음 저장하거나 같은 값으로 재시도한다.
    pub fn store_relay_reconnect_verifier(
        &self,
        device_id: &[u8; 16],
        identity: &[u8; 65],
        verifier: &[u8; 32],
        now: i64,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(now >= 0, "relay reconnect timestamp invalid");
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE relay_devices SET reconnect_verifier = ?3
             WHERE device_id = ?1 AND identity_public_sec1 = ?2 AND revoked_at IS NULL
               AND issued_at <= ?4 AND device_expires_at > ?4 AND permission_view = 1
               AND (reconnect_verifier IS NULL OR reconnect_verifier = ?3)",
            rusqlite::params![
                device_id.as_slice(),
                identity.as_slice(),
                verifier.as_slice(),
                now
            ],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }

    pub fn relay_reconnect_verifier(
        &self,
        device_id: &[u8; 16],
    ) -> anyhow::Result<Option<[u8; 32]>> {
        let mut statement = self.conn.prepare_cached("SELECT reconnect_verifier FROM relay_devices WHERE device_id = ?1 AND reconnect_verifier IS NOT NULL")?;
        let mut rows = statement.query([device_id.as_slice()])?;
        match rows.next()? {
            Some(row) => Ok(Some(relay_blob(row, 0)?)),
            None => Ok(None),
        }
    }

    pub fn list_relay_devices_bounded(&self, limit: usize) -> anyhow::Result<Vec<RelayDeviceRow>> {
        anyhow::ensure!(limit <= RELAY_DEVICE_ROWS_MAX, "relay device limit invalid");
        let probe_limit = i64::try_from(limit.saturating_add(1))?;
        let tx = self.conn.unchecked_transaction()?;
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM (
                 SELECT 1 FROM relay_devices
                 ORDER BY revoked_at, last_seen_at DESC, issued_at DESC, device_id
                 LIMIT ?1
             )",
            [probe_limit],
            |row| row.get(0),
        )?;
        let count = usize::try_from(count).context("relay device count invalid")?;
        anyhow::ensure!(
            count <= limit,
            "relay device snapshot exceeds requested limit"
        );

        let mut result = Vec::with_capacity(count);
        {
            let mut statement = tx.prepare_cached(
                "SELECT device_id, identity_public_sec1, display_name,
                        permission_view, permission_input, permission_upload, permission_approval,
                        issued_at, device_expires_at, last_seen_at, revoked_at, authorization_epoch
                 FROM relay_devices
                 ORDER BY revoked_at, last_seen_at DESC, issued_at DESC, device_id
                 LIMIT ?1",
            )?;
            let mut rows = statement.query([i64::try_from(limit)?])?;
            while let Some(row) = rows.next()? {
                result.push(relay_device_from_row(row)?);
            }
        }
        tx.commit()?;
        Ok(result)
    }

    pub fn revoke_relay_device(
        &self,
        device_id: &[u8; RELAY_ID_BYTES],
        revoked_at: i64,
    ) -> anyhow::Result<RelayDeviceRevocation> {
        anyhow::ensure!(revoked_at >= 0, "relay revocation timestamp invalid");
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let state = tx
            .query_row(
                "SELECT issued_at, revoked_at FROM relay_devices WHERE device_id = ?1",
                [device_id.as_slice()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .optional()?;
        let Some((issued_at, existing_revocation)) = state else {
            tx.commit()?;
            return Ok(RelayDeviceRevocation::NotFound);
        };
        anyhow::ensure!(
            revoked_at >= issued_at,
            "relay revocation timestamp invalid"
        );
        if existing_revocation.is_none() {
            tx.execute(
                "UPDATE relay_devices SET revoked_at = ?2, reconnect_verifier = NULL
                 WHERE device_id = ?1 AND revoked_at IS NULL",
                rusqlite::params![device_id.as_slice(), revoked_at],
            )?;
        }
        tx.commit()?;
        Ok(RelayDeviceRevocation::Revoked)
    }

    pub fn touch_relay_device(
        &self,
        device_id: &[u8; RELAY_ID_BYTES],
        seen_at: i64,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(seen_at >= 0, "relay last-seen timestamp invalid");
        let changed = self.conn.execute(
            "UPDATE relay_devices SET last_seen_at = ?2
             WHERE device_id = ?1
               AND revoked_at IS NULL
               AND issued_at <= ?2
               AND ?2 < device_expires_at
               AND (last_seen_at IS NULL OR last_seen_at <= ?2)",
            rusqlite::params![device_id.as_slice(), seen_at],
        )?;
        Ok(changed == 1)
    }

    /// 웹푸시 구독 등록/갱신 (v21, PR-P4). 같은 endpoint로 재구독하면 키만 갱신하고
    /// created_at은 보존한다(브라우저가 키를 회전해도 최초 등록 시각 유지). last_ok_at은
    /// 갱신 시 손대지 않는다 — 발송 성공만이 갱신한다.
    pub fn upsert_web_push_subscription(
        &self,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
        created_at: i64,
    ) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT INTO web_push_subscriptions (endpoint, p256dh, auth, created_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(endpoint) DO UPDATE SET
                   p256dh = excluded.p256dh,
                   auth = excluded.auth",
                (endpoint, p256dh, auth, created_at),
            )
            .context("웹푸시 구독 저장 실패")?;
        Ok(())
    }

    /// 전체 웹푸시 구독 목록 (발송 대상). 등록순(created_at, endpoint)으로 결정적 정렬.
    pub fn list_web_push_subscriptions(&self) -> anyhow::Result<Vec<WebPushSubscriptionRow>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT endpoint, p256dh, auth FROM web_push_subscriptions
             ORDER BY created_at, endpoint",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(WebPushSubscriptionRow {
                endpoint: row.get(0)?,
                p256dh: row.get(1)?,
                auth: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Complete web-push target snapshot. The endpoint/key material is checked by SQLite type,
    /// per-field bytes, per-row bytes, total retained bytes, and `limit + 1` before Rust allocates
    /// any returned String or Vec element.
    pub fn list_web_push_subscriptions_bounded(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<WebPushSubscriptionRow>> {
        let sql_limit = bounded_limit_plus_one(limit, WEB_PUSH_SUBSCRIPTION_ROWS_MAX)?;
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
        let probe = bounded_read_preflight_with_budget(
            &tx,
            WEB_PUSH_BOUNDED_PREFLIGHT,
            rusqlite::params![
                sql_limit,
                BOUNDED_TEXT_BYTES_MAX as i64,
                BOUNDED_TEXT_BYTES_MAX as i64,
                BOUNDED_ROW_BYTES_MAX as i64,
            ],
            limit,
            WEB_PUSH_RETAINED_BYTES_MAX,
        )?;
        let mut result = Vec::with_capacity(probe.count);
        {
            let mut stmt = tx
                .prepare(WEB_PUSH_BOUNDED_SELECT)
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            let mut rows = stmt
                .query(rusqlite::params![sql_limit, BOUNDED_TEXT_BYTES_MAX as i64])
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            while let Some(row) = rows
                .next()
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?
            {
                let endpoint = bounded_required_text(row, 0, BOUNDED_TEXT_BYTES_MAX, true, false)?;
                let p256dh = bounded_required_text(row, 1, BOUNDED_TEXT_BYTES_MAX, true, false)?;
                let auth = bounded_required_text(row, 2, BOUNDED_TEXT_BYTES_MAX, true, false)?;
                result.push(WebPushSubscriptionRow {
                    endpoint: endpoint.to_owned(),
                    p256dh: p256dh.to_owned(),
                    auth: auth.to_owned(),
                });
            }
        }
        tx.commit()
            .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
        Ok(result)
    }

    /// 웹푸시 구독 삭제 (발송이 410 Gone/404 Not Found를 받은 죽은 구독 정리).
    pub fn delete_web_push_subscription(&self, endpoint: &str) -> anyhow::Result<()> {
        self.conn
            .execute(
                "DELETE FROM web_push_subscriptions WHERE endpoint = ?1",
                [endpoint],
            )
            .context("웹푸시 구독 삭제 실패")?;
        Ok(())
    }

    /// 발송 성공 시각(last_ok_at) 갱신 — 진단/정리용.
    pub fn touch_web_push_subscription(
        &self,
        endpoint: &str,
        last_ok_at: i64,
    ) -> anyhow::Result<()> {
        self.conn
            .execute(
                "UPDATE web_push_subscriptions SET last_ok_at = ?2 WHERE endpoint = ?1",
                (endpoint, last_ok_at),
            )
            .context("웹푸시 구독 last_ok_at 갱신 실패")?;
        Ok(())
    }

    /// 현재 구독 수 — 발송 스레드의 폴링 게이트(구독 0이면 폴링 정지)에 쓴다.
    pub fn count_web_push_subscriptions(&self) -> anyhow::Result<i64> {
        let count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM web_push_subscriptions", [], |row| {
                    row.get(0)
                })?;
        Ok(count)
    }

    /// Runs one explicit, bounded legacy-retention batch. This is private so no production caller
    /// can accidentally forget the completion loop before admitting a lifecycle mutation.
    fn normalize_audit_retention_batch(&self) -> anyhow::Result<audit::AuditNormalizationReport> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let report = audit::normalize_audit_retention_batch_in_transaction(
            &tx,
            audit::AuditRetentionPolicy::production(),
            deppy_core::time::unix_secs_i64(),
        )?;
        tx.commit()
            .context("audit retention normalization transaction commit failed")?;
        Ok(report)
    }

    fn normalize_audit_retention_to_completion(&self) -> anyhow::Result<()> {
        loop {
            let report = self.normalize_audit_retention_batch()?;
            if report.is_complete() {
                return Ok(());
            }
            anyhow::ensure!(
                report.deleted_finalized_items() > 0,
                "audit_retention_normalization_no_progress"
            );
        }
    }

    /// Runs the normal one-scan lifecycle path first. Only the exact opaque normalization marker
    /// may trigger rollback, bounded legacy batches, and one DB-only retry. No external tool call or
    /// authorization grant exists inside this helper.
    fn with_audit_retention_normalization_retry<T>(
        &self,
        mut lifecycle_transaction: impl FnMut() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        match lifecycle_transaction() {
            Err(error)
                if error
                    .downcast_ref::<audit::AuditRetentionNormalizationRequired>()
                    .is_some() =>
            {
                self.normalize_audit_retention_to_completion()?;
                lifecycle_transaction()
            }
            result => result,
        }
    }

    /// tool 실행 감사 기록 (PR-16). encryptor를 넘기면 전체 입력이 암호화 저장된다 (§7).
    pub fn record_tool_audit(
        &self,
        record: &audit::AuditRecord<'_>,
        redaction: &secret::RedactionService,
        encryptor: Option<&dyn secret::SecretStore>,
    ) -> anyhow::Result<String> {
        self.with_audit_retention_normalization_retry(|| {
            let tx =
                rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let audit_id = audit::record_audit(&tx, redaction, record, encryptor)?;
            audit::prune_audit_logs_in_transaction(
                &tx,
                audit::AuditRetentionPolicy::production(),
                deppy_core::time::unix_secs_i64(),
            )?;
            tx.commit().context("tool audit/retention commit failed")?;
            Ok(audit_id)
        })
    }

    /// Acquires one DB/scope owner for its whole lifetime and begins a fresh random run. Since the
    /// exclusive scope lock is already held, any Prepared row from a different run in this scope
    /// belongs to a dead executor and is atomically reconciled to Unknown. Other scopes are never
    /// touched and there is no high-cardinality generation registry table.
    pub fn acquire_authorization_owner(
        &self,
        scope: &str,
    ) -> anyhow::Result<ActiveAuthorizationOwner> {
        let scope_key = audit::authorization_scope_lock_key(scope)?;
        anyhow::ensure!(
            !self.authorization_db_identity.starts_with("memory:"),
            "file-backed DB만 authorization owner를 획득할 수 있습니다"
        );
        let lock_dir = authorization_lock_dir(&self.authorization_db_identity);
        fs::create_dir_all(&lock_dir).with_context(|| {
            format!(
                "authorization lock directory 생성 실패: {}",
                lock_dir.display()
            )
        })?;
        // A fixed 256-stripe set is a hard filesystem bound per DB. Hash collisions only
        // serialize unrelated scopes conservatively; they can never permit concurrent ownership.
        let stripe = u8::from_str_radix(&scope_key[..2], 16)
            .context("authorization scope stripe 계산 실패")?;
        let lock_path = lock_dir.join(format!("stripe-{stripe:03}.lock"));
        let scope_lock = open_lock_file(&lock_path)?;
        if fs2::FileExt::try_lock_exclusive(&scope_lock).is_err() {
            anyhow::bail!("authorization scope is already owned by a live executor");
        }
        let run_id = uuid::Uuid::new_v4().to_string();
        self.with_audit_retention_normalization_retry(|| {
            let tx =
                rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute(
                "UPDATE tool_audit_logs
                 SET lifecycle = 'unknown', outcome_error_code = 'owner_superseded',
                     completed_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE authorization_scope = ?1
                   AND authorization_run_id != ?2
                   AND lifecycle = 'prepared'",
                (scope, &run_id),
            )?;
            audit::prune_audit_logs_in_transaction(
                &tx,
                audit::AuditRetentionPolicy::production(),
                deppy_core::time::unix_secs_i64(),
            )?;
            tx.commit()
                .context("authorization owner 시작 transaction 실패")
        })?;
        Ok(ActiveAuthorizationOwner {
            _scope_lock: scope_lock,
            scope: scope.to_owned(),
            run_id,
            db_identity: self.authorization_db_identity.clone(),
        })
    }

    /// Shared GUI/proxy AU01 preflight. AuthorizationPlan을 consume하고 exact binding에서
    /// permission mutation + redacted audit row를 한 transaction으로 만든 뒤에만 opaque
    /// AuthorizationGrant를 반환한다. Raw encrypted audit는 이 default path에서 항상 NULL이다.
    pub fn commit_authorization_preflight(
        &self,
        owner: &ActiveAuthorizationOwner,
        plan: audit::AuthorizationPlan,
        input_json: &str,
        redaction: &secret::RedactionService,
    ) -> anyhow::Result<audit::AuthorizationPreflight> {
        audit::validate_tool_input(input_json.as_bytes())?;
        let operation = self.with_audit_retention_normalization_retry(|| {
            let tx =
                rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let operation = Self::commit_authorization_preflight_in_transaction(
                &self.authorization_db_identity,
                &tx,
                owner,
                &plan,
                input_json,
                redaction,
            )?;
            audit::prune_audit_logs_in_transaction(
                &tx,
                audit::AuditRetentionPolicy::production(),
                deppy_core::time::unix_secs_i64(),
            )?;
            tx.commit()
                .context("authorization permission/audit preflight commit 실패")?;
            Ok(operation)
        })?;
        Ok(audit::finish_owned_authorization_preflight(plan, operation))
    }

    /// Global-config CAS variant used after live schema validation. Acquiring IMMEDIATE before
    /// reading the revision prevents another process from changing server/tool/credential state
    /// between the revision proof, exact permission check, remembered decision, and audit row.
    pub fn commit_authorization_preflight_revision_cas(
        &self,
        expected_revision: ConnectorConfigRevision,
        owner: &ActiveAuthorizationOwner,
        plan: audit::AuthorizationPlan,
        input_json: &str,
        redaction: &secret::RedactionService,
    ) -> anyhow::Result<ConnectorConfigCas<audit::AuthorizationPreflight>> {
        audit::validate_tool_input(input_json.as_bytes())?;
        let persisted = self.with_audit_retention_normalization_retry(|| {
            let tx =
                rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let current_revision = Self::read_connector_config_revision(&tx)?;
            if current_revision != expected_revision {
                tx.commit()
                    .context("stale authorization revision CAS transaction 실패")?;
                return Ok(ConnectorConfigCas::Stale { current_revision });
            }
            let operation = Self::commit_authorization_preflight_in_transaction(
                &self.authorization_db_identity,
                &tx,
                owner,
                &plan,
                input_json,
                redaction,
            )?;
            audit::prune_audit_logs_in_transaction(
                &tx,
                audit::AuditRetentionPolicy::production(),
                deppy_core::time::unix_secs_i64(),
            )?;
            let revision = Self::read_connector_config_revision(&tx)?;
            anyhow::ensure!(
                revision >= current_revision,
                "authorization preflight 중 Connector config revision이 감소했습니다"
            );
            tx.commit()
                .context("authorization revision CAS preflight commit 실패")?;
            Ok(ConnectorConfigCas::Committed {
                revision,
                value: operation,
            })
        })?;
        Ok(match persisted {
            ConnectorConfigCas::Stale { current_revision } => {
                ConnectorConfigCas::Stale { current_revision }
            }
            ConnectorConfigCas::Committed { revision, value } => ConnectorConfigCas::Committed {
                revision,
                value: audit::finish_owned_authorization_preflight(plan, value),
            },
        })
    }

    fn commit_authorization_preflight_in_transaction(
        authorization_db_identity: &str,
        conn: &Connection,
        owner: &ActiveAuthorizationOwner,
        plan: &audit::AuthorizationPlan,
        input_json: &str,
        redaction: &secret::RedactionService,
    ) -> anyhow::Result<audit::ValidatedOwnedAuthorizationOperation> {
        anyhow::ensure!(
            !conn.is_autocommit(),
            "authorization preflight는 caller-owned transaction이 필요합니다"
        );
        anyhow::ensure!(
            authorization_db_identity == owner.db_identity,
            "authorization owner가 다른 DB에 속합니다"
        );
        let current_permission =
            match mcp_store::permission_rule_in_snapshot(conn, plan.server_id(), plan.tool_name())?
            {
                Some(row) => audit::PermissionFingerprint::Persisted {
                    rule: audit::PermissionRule::from_persisted(&row.rule)
                        .ok_or_else(|| anyhow::anyhow!("invalid persisted permission rule"))?,
                    approved_schema_hash: row.approved_schema_hash,
                },
                None => audit::PermissionFingerprint::Absent,
            };
        anyhow::ensure!(
            &current_permission == plan.expected_permission(),
            "permission changed after authorization evaluation"
        );
        match plan.decision() {
            audit::ToolDecision::AllowAlways => mcp_store::upsert_permission_rule(
                conn,
                plan.server_id(),
                plan.tool_name(),
                audit::PermissionRule::Allow.as_str(),
                Some(plan.live_schema_hash()),
            )?,
            audit::ToolDecision::DenyAlways => mcp_store::upsert_permission_rule(
                conn,
                plan.server_id(),
                plan.tool_name(),
                audit::PermissionRule::Deny.as_str(),
                None,
            )?,
            _ => {}
        }
        let operation = audit::prepare_owned_authorization_operation(
            conn,
            plan,
            input_json,
            redaction,
            owner.scope(),
            &owner.run_id,
        )?;
        audit::validate_owned_authorization_operation(plan, operation)
    }

    pub fn complete_authorization_outcome(
        &self,
        owner: &ActiveAuthorizationOwner,
        operation_id: &str,
        outcome: audit::AuthorizationOutcome,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.authorization_db_identity == owner.db_identity,
            "authorization owner가 다른 DB에 속합니다"
        );
        self.with_audit_retention_normalization_retry(|| {
            let tx =
                rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            audit::complete_authorization_operation(
                &tx,
                owner.scope(),
                &owner.run_id,
                operation_id,
                outcome,
            )?;
            audit::prune_audit_logs_in_transaction(
                &tx,
                audit::AuditRetentionPolicy::production(),
                deppy_core::time::unix_secs_i64(),
            )?;
            tx.commit()
                .context("authorization outcome/retention commit failed")
        })
    }

    /// Graceful executor shutdown after calls drain. Consuming the owner makes further preflight
    /// or completion compiler-unrepresentable. If persistence fails, Drop still releases the OS
    /// stripe lock and the next same-scope owner performs crash-safe reconciliation.
    pub fn close_authorization_owner(
        &self,
        owner: ActiveAuthorizationOwner,
    ) -> anyhow::Result<usize> {
        anyhow::ensure!(
            self.authorization_db_identity == owner.db_identity,
            "authorization owner가 다른 DB에 속합니다"
        );
        self.with_audit_retention_normalization_retry(|| {
            let tx =
                rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let affected = tx
                .execute(
                    "UPDATE tool_audit_logs
                     SET lifecycle = 'unknown', outcome_error_code = 'owner_shutdown',
                         completed_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                     WHERE authorization_scope = ?1
                       AND authorization_run_id = ?2
                       AND lifecycle = 'prepared'",
                    (owner.scope(), &owner.run_id),
                )
                .context("authorization owner shutdown 저장 실패")?;
            audit::prune_audit_logs_in_transaction(
                &tx,
                audit::AuditRetentionPolicy::production(),
                deppy_core::time::unix_secs_i64(),
            )?;
            tx.commit()
                .context("authorization owner shutdown/retention commit failed")?;
            Ok(affected)
        })
    }

    pub fn tool_audit_lifecycle(
        &self,
        operation_id: &str,
    ) -> anyhow::Result<Option<audit::AuditLifecycle>> {
        audit::audit_lifecycle(&self.conn, operation_id)
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
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        settings_env_profile_write_admission(&tx, &id, workspace_id, name, kind)?;
        tx
            .execute(
                "INSERT INTO env_profiles (id, workspace_id, name, kind, is_production, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5,
                    strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
                (&id, workspace_id, name, kind, kind == "production"),
            )
            .with_context(|| format!("env profile 저장 실패: {name}"))?;
        tx.commit()
            .context("env profile insert transaction commit failed")?;
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

    pub fn list_env_profiles_bounded(
        &self,
        workspace_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<EnvProfileRow>> {
        anyhow::ensure!(
            bounded_id_is_valid(workspace_id),
            BOUNDED_READ_INPUT_INVALID
        );
        let sql_limit = bounded_limit_plus_one(limit, ENV_PROFILE_ROWS_MAX)?;
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
        let probe = bounded_read_preflight(
            &tx,
            ENV_PROFILES_BOUNDED_PREFLIGHT,
            rusqlite::params![
                workspace_id,
                sql_limit,
                BOUNDED_ID_BYTES_MAX as i64,
                BOUNDED_TEXT_BYTES_MAX as i64,
                BOUNDED_ROW_BYTES_MAX as i64,
            ],
            limit,
        )?;
        let mut result = Vec::with_capacity(probe.count);
        {
            let mut stmt = tx
                .prepare(ENV_PROFILES_BOUNDED_SELECT)
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            let mut rows = stmt
                .query(rusqlite::params![
                    workspace_id,
                    sql_limit,
                    BOUNDED_ID_BYTES_MAX as i64,
                ])
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            while let Some(row) = rows
                .next()
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?
            {
                let id = bounded_required_text(row, 0, BOUNDED_ID_BYTES_MAX, true, true)?;
                let name = bounded_required_text(row, 1, BOUNDED_TEXT_BYTES_MAX, false, false)?;
                let kind = bounded_required_text(row, 2, BOUNDED_TEXT_BYTES_MAX, false, false)?;
                let is_production = bounded_integer(row, 3)?;
                anyhow::ensure!(matches!(is_production, 0 | 1), BOUNDED_READ_ROW_INVALID);
                result.push(EnvProfileRow {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    kind: kind.to_owned(),
                    is_production: is_production != 0,
                });
            }
        }
        tx.commit()
            .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
        Ok(result)
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
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        settings_env_var_write_admission(&tx, profile_id, key, kind, plain_value, credential_id)?;
        tx
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
        tx.commit()
            .context("env var upsert transaction commit failed")
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

    pub fn list_env_vars_bounded(
        &self,
        profile_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<EnvVarRow>> {
        anyhow::ensure!(bounded_id_is_valid(profile_id), BOUNDED_READ_INPUT_INVALID);
        let sql_limit = bounded_limit_plus_one(limit, ENV_VAR_ROWS_MAX)?;
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
        let probe = bounded_read_preflight(
            &tx,
            ENV_VARS_BOUNDED_PREFLIGHT,
            rusqlite::params![
                profile_id,
                sql_limit,
                BOUNDED_ID_BYTES_MAX as i64,
                BOUNDED_TEXT_BYTES_MAX as i64,
                BOUNDED_ROW_BYTES_MAX as i64,
            ],
            limit,
        )?;
        let mut result = Vec::with_capacity(probe.count);
        {
            let mut stmt = tx
                .prepare(ENV_VARS_BOUNDED_SELECT)
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            let mut rows = stmt
                .query(rusqlite::params![
                    profile_id,
                    sql_limit,
                    BOUNDED_ID_BYTES_MAX as i64,
                    BOUNDED_TEXT_BYTES_MAX as i64,
                ])
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
            while let Some(row) = rows
                .next()
                .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?
            {
                let key = bounded_required_text(row, 0, BOUNDED_TEXT_BYTES_MAX, true, false)?;
                let kind = bounded_required_text(row, 1, BOUNDED_TEXT_BYTES_MAX, true, false)?;
                let plain = bounded_optional_text(row, 2, BOUNDED_MESSAGE_BYTES_MAX)?;
                let credential_id = bounded_optional_text(row, 3, BOUNDED_ID_BYTES_MAX)?;
                let value = match (kind, plain, credential_id) {
                    ("secret", None, Some(id)) if !id.is_empty() => EnvValue::Secret {
                        credential_id: id.to_owned(),
                    },
                    ("plain", value, None) => EnvValue::Plain(value.unwrap_or_default().to_owned()),
                    _ => anyhow::bail!(BOUNDED_READ_ROW_INVALID),
                };
                result.push(EnvVarRow {
                    key: key.to_owned(),
                    value,
                });
            }
        }
        tx.commit()
            .map_err(|_| anyhow::anyhow!(BOUNDED_READ_QUERY_FAILED))?;
        Ok(result)
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

    fn settings_agent_args_inventory_preflight(
        conn: &Connection,
        sql_limit: i64,
    ) -> anyhow::Result<()> {
        let (max_json_bytes, max_args, invalid_rows): (i64, i64, i64) = conn.query_row(
            "SELECT COALESCE(MAX(length(CAST(args_json AS BLOB))), 0),
                    COALESCE(MAX(CASE
                        WHEN json_valid(args_json) = 1 AND json_type(args_json) = 'array'
                        THEN json_array_length(args_json) ELSE 0 END), 0),
                    COALESCE(SUM(CASE
                        WHEN json_valid(args_json) = 1 AND json_type(args_json) = 'array'
                        THEN 0 ELSE 1 END), 0)
             FROM (
                 SELECT args_json FROM agent_configs WHERE deleted_at IS NULL
                 ORDER BY created_at, id LIMIT ?1
             )",
            [sql_limit],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let max_json_bytes =
            usize::try_from(max_json_bytes).context("settings_agent_args_json_bytes_invalid")?;
        let max_args = usize::try_from(max_args).context("settings_agent_args_count_invalid")?;
        anyhow::ensure!(
            max_json_bytes <= SETTINGS_AGENT_ARGS_BYTES_MAX,
            "settings_agent_args_json_bytes_limit"
        );
        anyhow::ensure!(
            max_args <= SETTINGS_AGENT_ARGS_LIMIT_MAX,
            "settings_agent_args_item_limit"
        );
        anyhow::ensure!(invalid_rows == 0, "settings_agent_args_json_invalid");
        Ok(())
    }

    fn settings_agent_configs_in_snapshot(
        conn: &Connection,
        sql_limit: i64,
        capacity: usize,
    ) -> anyhow::Result<Vec<AgentConfigRow>> {
        let mut stmt = conn.prepare(
            "SELECT id, name, command, args_json,
                    waiting_regex, approval_regex, error_regex, done_regex,
                    mcp_proxy_enabled, mcp_proxy_server_id, mcp_config_flag
             FROM agent_configs WHERE deleted_at IS NULL
             ORDER BY created_at, id LIMIT ?1",
        )?;
        let rows = stmt.query_map([sql_limit], |row| {
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
        let mut agents = Vec::with_capacity(capacity);
        for row in rows {
            agents.push(settings_agent_config_from_persisted(row?)?);
        }
        Ok(agents)
    }

    fn settings_agent_args_point_preflight(
        conn: &Connection,
        agent_id: &str,
    ) -> anyhow::Result<()> {
        let Some((json_bytes, args_count, valid_array)) = conn
            .query_row(
                "SELECT length(CAST(args_json AS BLOB)),
                        CASE
                            WHEN json_valid(args_json) = 1 AND json_type(args_json) = 'array'
                            THEN json_array_length(args_json) ELSE 0 END,
                        CASE
                            WHEN json_valid(args_json) = 1 AND json_type(args_json) = 'array'
                            THEN 1 ELSE 0 END
                 FROM agent_configs WHERE id = ?1 AND deleted_at IS NULL",
                [agent_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?
        else {
            return Ok(());
        };
        let json_bytes =
            usize::try_from(json_bytes).context("settings_agent_launch_args_json_bytes_invalid")?;
        let args_count =
            usize::try_from(args_count).context("settings_agent_launch_args_count_invalid")?;
        anyhow::ensure!(
            json_bytes <= SETTINGS_AGENT_ARGS_BYTES_MAX,
            "settings_agent_launch_args_json_bytes_limit"
        );
        anyhow::ensure!(
            args_count <= SETTINGS_AGENT_ARGS_LIMIT_MAX,
            "settings_agent_launch_args_item_limit"
        );
        anyhow::ensure!(valid_array == 1, "settings_agent_launch_args_json_invalid");
        Ok(())
    }

    /// Complete-or-error Agents settings snapshot from one SQLite read transaction. Every count,
    /// row size, aggregate size, and args JSON shape is checked before any TEXT reaches Rust.
    pub fn settings_agents_snapshot_rows(
        &self,
        workspace_id: &str,
    ) -> anyhow::Result<SettingsAgentsSnapshotRows> {
        let tx = self.conn.unchecked_transaction()?;
        let agent_limit =
            settings_sql_probe_limit(SETTINGS_AGENT_LIMIT_MAX, "settings_agents_snapshot")?;
        let profile_limit = settings_sql_probe_limit(
            SETTINGS_ENV_PROFILE_LIMIT_MAX,
            "settings_agent_profiles_snapshot",
        )?;
        let backend_limit = settings_sql_probe_limit(
            SETTINGS_ENABLED_MCP_SERVER_LIMIT_MAX,
            "settings_agent_backends_snapshot",
        )?;
        let agent_probe = settings_read_probe(
            &tx,
            "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
             FROM (
                 SELECT length(CAST(id AS BLOB)) + length(CAST(name AS BLOB)) +
                        length(CAST(command AS BLOB)) + length(CAST(args_json AS BLOB)) +
                        length(CAST(COALESCE(waiting_regex, '') AS BLOB)) +
                        length(CAST(COALESCE(approval_regex, '') AS BLOB)) +
                        length(CAST(COALESCE(error_regex, '') AS BLOB)) +
                        length(CAST(COALESCE(done_regex, '') AS BLOB)) +
                        length(CAST(COALESCE(mcp_proxy_server_id, '') AS BLOB)) +
                        length(CAST(COALESCE(mcp_config_flag, '') AS BLOB)) AS row_bytes
                 FROM agent_configs WHERE deleted_at IS NULL
                 ORDER BY created_at, id LIMIT ?1
             )",
            [agent_limit],
            SETTINGS_AGENT_LIMIT_MAX,
            SETTINGS_SNAPSHOT_BYTES_MAX,
            SETTINGS_ROW_BYTES_MAX,
            "settings_agents_snapshot",
        )?;
        Self::settings_agent_args_inventory_preflight(&tx, agent_limit)?;
        let profile_probe = settings_read_probe(
            &tx,
            "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
             FROM (
                 SELECT length(CAST(id AS BLOB)) + length(CAST(name AS BLOB)) +
                        length(CAST(kind AS BLOB)) AS row_bytes
                 FROM env_profiles WHERE workspace_id = ?1
                 ORDER BY created_at, id LIMIT ?2
             )",
            rusqlite::params![workspace_id, profile_limit],
            SETTINGS_ENV_PROFILE_LIMIT_MAX,
            SETTINGS_SNAPSHOT_BYTES_MAX,
            SETTINGS_ROW_BYTES_MAX,
            "settings_agent_profiles_snapshot",
        )?;
        let backend_probe = settings_read_probe(
            &tx,
            "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
             FROM (
                 SELECT length(CAST(id AS BLOB)) + length(CAST(name AS BLOB)) AS row_bytes
                 FROM mcp_servers WHERE enabled != 0
                 ORDER BY created_at, id LIMIT ?1
             )",
            [backend_limit],
            SETTINGS_ENABLED_MCP_SERVER_LIMIT_MAX,
            SETTINGS_SNAPSHOT_BYTES_MAX,
            SETTINGS_ROW_BYTES_MAX,
            "settings_agent_backends_snapshot",
        )?;
        let retained_bytes = agent_probe
            .retained_bytes
            .checked_add(profile_probe.retained_bytes)
            .and_then(|bytes| bytes.checked_add(backend_probe.retained_bytes))
            .context("settings_agents_snapshot_bytes_overflow")?;
        anyhow::ensure!(
            retained_bytes <= SETTINGS_SNAPSHOT_BYTES_MAX,
            "settings_agents_snapshot_retained_bytes_limit"
        );

        let agents = Self::settings_agent_configs_in_snapshot(&tx, agent_limit, agent_probe.count)?;
        let profiles = {
            let mut stmt = tx.prepare(
                "SELECT id, name, kind, is_production FROM env_profiles
                 WHERE workspace_id = ?1 ORDER BY created_at, id LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![workspace_id, profile_limit], |row| {
                Ok(EnvProfileRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    kind: row.get(2)?,
                    is_production: row.get(3)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let enabled_mcp_servers = {
            let mut stmt = tx.prepare(
                "SELECT id, name FROM mcp_servers WHERE enabled != 0
                 ORDER BY created_at, id LIMIT ?1",
            )?;
            let rows = stmt.query_map([backend_limit], |row| {
                Ok(SettingsMcpServerRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        tx.commit()
            .context("settings agents snapshot transaction commit failed")?;
        Ok(SettingsAgentsSnapshotRows {
            agents,
            profiles,
            enabled_mcp_servers,
        })
    }

    /// Complete-or-error Environment settings snapshot. The workspace env join removes the old
    /// per-profile N+1 read while keeping credentials, profiles, and vars in one stable snapshot.
    pub fn settings_environment_snapshot_rows(
        &self,
        workspace_id: &str,
    ) -> anyhow::Result<SettingsEnvironmentSnapshotRows> {
        let tx = self.conn.unchecked_transaction()?;
        let credential_limit = settings_sql_probe_limit(
            SETTINGS_CREDENTIAL_LIMIT_MAX,
            "settings_credentials_snapshot",
        )?;
        let profile_limit = settings_sql_probe_limit(
            SETTINGS_ENV_PROFILE_LIMIT_MAX,
            "settings_env_profiles_snapshot",
        )?;
        let env_var_limit =
            settings_sql_probe_limit(SETTINGS_ENV_VAR_LIMIT_MAX, "settings_env_vars_snapshot")?;
        let credential_probe = settings_read_probe(
            &tx,
            "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
             FROM (
                 SELECT length(CAST(id AS BLOB)) + length(CAST(provider AS BLOB)) +
                        length(CAST(label AS BLOB)) + length(CAST(credential_kind AS BLOB)) +
                        length(CAST(COALESCE(masked_hint, '') AS BLOB)) +
                        length(CAST(COALESCE(workspace_id, '') AS BLOB)) AS row_bytes
                 FROM credentials WHERE workspace_id IS NULL OR workspace_id = ?1
                 ORDER BY created_at, id LIMIT ?2
             )",
            rusqlite::params![workspace_id, credential_limit],
            SETTINGS_CREDENTIAL_LIMIT_MAX,
            SETTINGS_SNAPSHOT_BYTES_MAX,
            SETTINGS_ROW_BYTES_MAX,
            "settings_credentials_snapshot",
        )?;
        let profile_probe = settings_read_probe(
            &tx,
            "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
             FROM (
                 SELECT length(CAST(id AS BLOB)) + length(CAST(name AS BLOB)) +
                        length(CAST(kind AS BLOB)) AS row_bytes
                 FROM env_profiles WHERE workspace_id = ?1
                 ORDER BY created_at, id LIMIT ?2
             )",
            rusqlite::params![workspace_id, profile_limit],
            SETTINGS_ENV_PROFILE_LIMIT_MAX,
            SETTINGS_SNAPSHOT_BYTES_MAX,
            SETTINGS_ROW_BYTES_MAX,
            "settings_env_profiles_snapshot",
        )?;
        let env_var_probe = settings_read_probe(
            &tx,
            "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
             FROM (
                 SELECT length(CAST(p.id AS BLOB)) + length(CAST(v.key AS BLOB)) +
                        length(CAST(v.kind AS BLOB)) +
                        length(CAST(COALESCE(v.plain_value, '') AS BLOB)) +
                        length(CAST(COALESCE(v.credential_id, '') AS BLOB)) AS row_bytes
                 FROM env_vars v JOIN env_profiles p ON p.id = v.profile_id
                 WHERE p.workspace_id = ?1 ORDER BY p.created_at, p.id, v.key LIMIT ?2
             )",
            rusqlite::params![workspace_id, env_var_limit],
            SETTINGS_ENV_VAR_LIMIT_MAX,
            SETTINGS_SNAPSHOT_BYTES_MAX,
            SETTINGS_ROW_BYTES_MAX,
            "settings_env_vars_snapshot",
        )?;
        let retained_bytes = credential_probe
            .retained_bytes
            .checked_add(profile_probe.retained_bytes)
            .and_then(|bytes| bytes.checked_add(env_var_probe.retained_bytes))
            .context("settings_environment_snapshot_bytes_overflow")?;
        anyhow::ensure!(
            retained_bytes <= SETTINGS_SNAPSHOT_BYTES_MAX,
            "settings_environment_snapshot_retained_bytes_limit"
        );

        let env_source_files = Self::env_source_files_in_snapshot(&tx, workspace_id)?;
        let credential_env = Self::credential_env_bindings_in_snapshot(&tx, workspace_id)?;
        let binding_bytes: usize = credential_env
            .iter()
            .map(|binding| binding.env_name.len() + binding.credential_id.len())
            .sum();
        anyhow::ensure!(
            retained_bytes
                .saturating_add(binding_bytes)
                .saturating_add(env_source_files.iter().map(String::len).sum::<usize>())
                <= SETTINGS_SNAPSHOT_BYTES_MAX,
            "settings_environment_snapshot_retained_bytes_limit"
        );
        let credentials = {
            let mut stmt = tx.prepare(
                "SELECT id, provider, label, credential_kind, masked_hint, workspace_id
                 FROM credentials WHERE workspace_id IS NULL OR workspace_id = ?1
                 ORDER BY created_at, id LIMIT ?2",
            )?;
            let rows =
                stmt.query_map(rusqlite::params![workspace_id, credential_limit], |row| {
                    Ok(CredentialMeta {
                        id: row.get(0)?,
                        provider: row.get(1)?,
                        label: row.get(2)?,
                        credential_kind: row.get(3)?,
                        masked_hint: row.get(4)?,
                        workspace_id: row.get(5)?,
                    })
                })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let profiles = {
            let mut stmt = tx.prepare(
                "SELECT id, name, kind, is_production FROM env_profiles
                 WHERE workspace_id = ?1 ORDER BY created_at, id LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![workspace_id, profile_limit], |row| {
                Ok(EnvProfileRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    kind: row.get(2)?,
                    is_production: row.get(3)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let env_vars = {
            let mut stmt = tx.prepare(
                "SELECT p.id, v.key, v.kind, v.plain_value, v.credential_id
                 FROM env_vars v JOIN env_profiles p ON p.id = v.profile_id
                 WHERE p.workspace_id = ?1 ORDER BY p.created_at, p.id, v.key LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![workspace_id, env_var_limit], |row| {
                let kind: String = row.get(2)?;
                let value = if kind == "secret" {
                    EnvValue::Secret {
                        credential_id: row.get(4)?,
                    }
                } else {
                    EnvValue::Plain(row.get::<_, Option<String>>(3)?.unwrap_or_default())
                };
                Ok(SettingsWorkspaceEnvVarRow {
                    profile_id: row.get(0)?,
                    key: row.get(1)?,
                    value,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        tx.commit()
            .context("settings environment snapshot transaction commit failed")?;
        Ok(SettingsEnvironmentSnapshotRows {
            credential_env,
            env_source_files,
            credentials,
            profiles,
            env_vars,
        })
    }

    /// Bounded point/child read for one agent launch. Agent JSON, optional workspace profile, and
    /// all profile vars share one read transaction; a stale/cross-workspace profile yields no vars.
    pub fn settings_agent_launch_rows(
        &self,
        workspace_id: &str,
        agent_id: &str,
        profile_id: Option<&str>,
    ) -> anyhow::Result<SettingsAgentLaunchRows> {
        let tx = self.conn.unchecked_transaction()?;
        let agent_sql_limit = settings_sql_probe_limit(1, "settings_agent_launch")?;
        let agent_probe = settings_read_probe(
            &tx,
            "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
             FROM (
                 SELECT length(CAST(id AS BLOB)) + length(CAST(name AS BLOB)) +
                        length(CAST(command AS BLOB)) + length(CAST(args_json AS BLOB)) +
                        length(CAST(COALESCE(waiting_regex, '') AS BLOB)) +
                        length(CAST(COALESCE(approval_regex, '') AS BLOB)) +
                        length(CAST(COALESCE(error_regex, '') AS BLOB)) +
                        length(CAST(COALESCE(done_regex, '') AS BLOB)) +
                        length(CAST(COALESCE(mcp_proxy_server_id, '') AS BLOB)) +
                        length(CAST(COALESCE(mcp_config_flag, '') AS BLOB)) AS row_bytes
                 FROM agent_configs WHERE id = ?1 AND deleted_at IS NULL LIMIT ?2
             )",
            rusqlite::params![agent_id, agent_sql_limit],
            1,
            SETTINGS_SNAPSHOT_BYTES_MAX,
            SETTINGS_ROW_BYTES_MAX,
            "settings_agent_launch",
        )?;
        Self::settings_agent_args_point_preflight(&tx, agent_id)?;

        let profile_sql_limit = settings_sql_probe_limit(1, "settings_agent_launch_profile")?;
        let env_var_sql_limit =
            settings_sql_probe_limit(SETTINGS_ENV_VAR_LIMIT_MAX, "settings_agent_launch_env")?;
        let (profile_probe, env_var_probe) = if let Some(profile_id) = profile_id {
            let profile_probe = settings_read_probe(
                &tx,
                "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
                 FROM (
                     SELECT length(CAST(id AS BLOB)) + length(CAST(name AS BLOB)) +
                            length(CAST(kind AS BLOB)) AS row_bytes
                     FROM env_profiles WHERE id = ?1 AND workspace_id = ?2 LIMIT ?3
                 )",
                rusqlite::params![profile_id, workspace_id, profile_sql_limit],
                1,
                SETTINGS_SNAPSHOT_BYTES_MAX,
                SETTINGS_ROW_BYTES_MAX,
                "settings_agent_launch_profile",
            )?;
            let env_var_probe = settings_read_probe(
                &tx,
                "SELECT COUNT(*), COALESCE(SUM(row_bytes), 0), COALESCE(MAX(row_bytes), 0)
                 FROM (
                     SELECT length(CAST(v.key AS BLOB)) + length(CAST(v.kind AS BLOB)) +
                            length(CAST(COALESCE(v.plain_value, '') AS BLOB)) +
                            length(CAST(COALESCE(v.credential_id, '') AS BLOB)) AS row_bytes
                     FROM env_vars v JOIN env_profiles p ON p.id = v.profile_id
                     WHERE p.id = ?1 AND p.workspace_id = ?2 ORDER BY v.key LIMIT ?3
                 )",
                rusqlite::params![profile_id, workspace_id, env_var_sql_limit],
                SETTINGS_ENV_VAR_LIMIT_MAX,
                SETTINGS_SNAPSHOT_BYTES_MAX,
                SETTINGS_ROW_BYTES_MAX,
                "settings_agent_launch_env",
            )?;
            (profile_probe, env_var_probe)
        } else {
            (
                SettingsReadProbe {
                    count: 0,
                    retained_bytes: 0,
                },
                SettingsReadProbe {
                    count: 0,
                    retained_bytes: 0,
                },
            )
        };
        let retained_bytes = agent_probe
            .retained_bytes
            .checked_add(profile_probe.retained_bytes)
            .and_then(|bytes| bytes.checked_add(env_var_probe.retained_bytes))
            .context("settings_agent_launch_bytes_overflow")?;
        anyhow::ensure!(
            retained_bytes <= SETTINGS_SNAPSHOT_BYTES_MAX,
            "settings_agent_launch_retained_bytes_limit"
        );

        let agent = tx
            .query_row(
                "SELECT id, name, command, args_json,
                        waiting_regex, approval_regex, error_regex, done_regex,
                        mcp_proxy_enabled, mcp_proxy_server_id, mcp_config_flag
                 FROM agent_configs WHERE id = ?1 AND deleted_at IS NULL",
                [agent_id],
                |row| {
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
                },
            )
            .optional()?
            .map(settings_agent_config_from_persisted)
            .transpose()?;
        let profile = if let Some(profile_id) = profile_id {
            tx.query_row(
                "SELECT id, name, kind, is_production FROM env_profiles
                 WHERE id = ?1 AND workspace_id = ?2",
                [profile_id, workspace_id],
                |row| {
                    Ok(EnvProfileRow {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        kind: row.get(2)?,
                        is_production: row.get(3)?,
                    })
                },
            )
            .optional()?
        } else {
            None
        };
        let env_vars = if let Some(profile_id) = profile_id
            && profile.is_some()
        {
            let mut stmt = tx.prepare(
                "SELECT v.key, v.kind, v.plain_value, v.credential_id
                 FROM env_vars v JOIN env_profiles p ON p.id = v.profile_id
                 WHERE p.id = ?1 AND p.workspace_id = ?2 ORDER BY v.key LIMIT ?3",
            )?;
            let rows = stmt.query_map(
                rusqlite::params![profile_id, workspace_id, env_var_sql_limit],
                |row| {
                    let kind: String = row.get(1)?;
                    let value = if kind == "secret" {
                        EnvValue::Secret {
                            credential_id: row.get(3)?,
                        }
                    } else {
                        EnvValue::Plain(row.get::<_, Option<String>>(2)?.unwrap_or_default())
                    };
                    Ok(EnvVarRow {
                        key: row.get(0)?,
                        value,
                    })
                },
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        let mcp_backend_enabled = if let Some(server_id) = agent
            .as_ref()
            .filter(|agent| agent.mcp_proxy_enabled)
            .and_then(|agent| agent.mcp_proxy_server_id.as_deref())
        {
            tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM mcp_servers WHERE id = ?1 AND enabled != 0)",
                [server_id],
                |row| row.get::<_, bool>(0),
            )?
        } else {
            false
        };
        tx.commit()
            .context("settings agent launch transaction commit failed")?;
        Ok(SettingsAgentLaunchRows {
            agent,
            profile,
            env_vars,
            mcp_backend_enabled,
        })
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
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        settings_agent_write_admission(
            &tx,
            &SettingsAgentWriteCandidate {
                id: &id,
                name,
                command,
                args_json: &args_json,
                waiting_regex,
                approval_regex,
                error_regex,
                done_regex,
                mcp_proxy_server_id,
                mcp_config_flag,
            },
        )?;
        tx.execute(
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
        tx.commit()
            .context("agent config insert transaction commit failed")?;
        Ok(id)
    }

    pub fn upsert_builtin_agent_config(
        &self,
        id: &str,
        name: &str,
        command: &str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !id.is_empty() && id.len() <= 128 && !id.bytes().any(|byte| byte.is_ascii_control()),
            "builtin_agent_id_invalid"
        );
        anyhow::ensure!(
            !name.is_empty()
                && name.len() <= 256
                && !name.bytes().any(|byte| byte.is_ascii_control()),
            "builtin_agent_name_invalid"
        );
        anyhow::ensure!(
            !command.is_empty()
                && command.len() <= 4 * 1024
                && !command.bytes().any(|byte| byte.is_ascii_control()),
            "builtin_agent_command_invalid"
        );
        let args_json = "[]";
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        settings_agent_write_admission(
            &tx,
            &SettingsAgentWriteCandidate {
                id,
                name,
                command,
                args_json,
                waiting_regex: None,
                approval_regex: None,
                error_regex: None,
                done_regex: None,
                mcp_proxy_server_id: None,
                mcp_config_flag: None,
            },
        )?;
        tx.execute(
            "INSERT INTO agent_configs
                 (id, name, command, args_json,
                  waiting_regex, approval_regex, error_regex, done_regex,
                  mcp_proxy_enabled, mcp_proxy_server_id, mcp_config_flag,
                  created_at, updated_at, deleted_at)
             VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL, NULL, 0, NULL, NULL,
                     strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                     strftime('%Y-%m-%dT%H:%M:%fZ','now'), NULL)
             ON CONFLICT(id) DO UPDATE SET
                 name = excluded.name,
                 command = excluded.command,
                 args_json = excluded.args_json,
                 waiting_regex = NULL,
                 approval_regex = NULL,
                 error_regex = NULL,
                 done_regex = NULL,
                 mcp_proxy_enabled = 0,
                 mcp_proxy_server_id = NULL,
                 mcp_config_flag = NULL,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                 deleted_at = NULL",
            (id, name, command, args_json),
        )
        .context("builtin agent config upsert failed")?;
        tx.commit()
            .context("builtin agent config upsert transaction commit failed")?;
        Ok(())
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

    #[derive(Default)]
    struct CountingAuditSecretStore {
        value: std::sync::Mutex<Option<String>>,
        set_count: std::sync::atomic::AtomicUsize,
    }

    impl secret::SecretStore for CountingAuditSecretStore {
        fn set_secret(&self, _id: &str, value: &secret::SecretString) -> anyhow::Result<()> {
            self.set_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            *self.value.lock().unwrap() = Some(value.expose().to_owned());
            Ok(())
        }

        fn get_secret(&self, _id: &str) -> anyhow::Result<secret::SecretString> {
            self.value
                .lock()
                .unwrap()
                .as_ref()
                .map(|value| secret::SecretString::new(value.clone()))
                .ok_or_else(|| anyhow::anyhow!("missing audit key"))
        }

        fn delete_secret(&self, _id: &str) -> anyhow::Result<()> {
            *self.value.lock().unwrap() = None;
            Ok(())
        }

        fn has_secret(&self, _id: &str) -> anyhow::Result<bool> {
            Ok(self.value.lock().unwrap().is_some())
        }
    }

    fn file_db(label: &str) -> (PathBuf, PathBuf, Db) {
        let dir = std::env::temp_dir().join(format!(
            "deppy-au01-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        let db = Db::open(&path).unwrap();
        (dir, path, db)
    }

    fn relay_pending_row(pairing: u8, device: u8, identity: u8) -> RelayPendingDeviceRow {
        let mut public_key = [identity; RELAY_PUBLIC_KEY_BYTES];
        public_key[0] = 0x04;
        RelayPendingDeviceRow {
            pairing_id: [pairing; RELAY_ID_BYTES],
            device_id: [device; RELAY_ID_BYTES],
            identity_public_sec1: public_key,
            display_name: format!("relay-device-{device}"),
            permission_view: true,
            permission_input: false,
            permission_upload: false,
            permission_approval: false,
            issued_at: 1_800_000_000,
            pairing_expires_at: 1_800_000_000 + RELAY_PAIRING_WINDOW_SECS_MAX,
            device_expires_at: 1_800_086_400,
        }
    }

    fn assert_relay_authorization_changes_without_clock_advance(recreate: bool) {
        let (dir, _, db) = file_db("relay-authorization-generation");
        let first = relay_pending_row(1, 2, 3);
        let approve = |pending: &RelayPendingDeviceRow| {
            db.insert_relay_pending_device(pending, pending.issued_at)
                .unwrap();
            let RelayDeviceApproval::Approved(row) = db
                .approve_relay_pending_device(
                    &pending.pairing_id,
                    &pending.identity_public_sec1,
                    pending.issued_at + 1,
                )
                .unwrap()
            else {
                panic!("승인 실패")
            };
            row
        };
        let before = approve(&first);
        if recreate {
            db.conn
                .execute(
                    "DELETE FROM relay_devices WHERE device_id = ?1",
                    [first.device_id.as_slice()],
                )
                .unwrap();
        }
        let after = approve(&relay_pending_row(4, 2, 3));
        assert_ne!(
            before, after,
            "같은 시각의 재승인/재생성도 새 인가여야 한다"
        );
        assert_eq!(before.issued_at, after.issued_at);
        assert_eq!(before.identity_public_sec1, after.identity_public_sec1);
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn relay_reapproval_changes_authorization_without_clock_advance() {
        assert_relay_authorization_changes_without_clock_advance(false);
    }

    #[test]
    fn relay_recreated_device_does_not_reuse_its_authorization() {
        assert_relay_authorization_changes_without_clock_advance(true);
    }

    #[test]
    fn relay_authorization_epoch_survives_conflict_and_publish_rollback() {
        let (dir, _, db) = file_db("relay-epoch-rollback");
        let first = relay_pending_row(1, 2, 3);
        db.insert_relay_pending_device(&first, first.issued_at)
            .unwrap();
        let RelayDeviceApproval::Approved(before) = db
            .approve_relay_pending_device(
                &first.pairing_id,
                &first.identity_public_sec1,
                first.issued_at + 1,
            )
            .unwrap()
        else {
            panic!("승인 실패")
        };
        db.store_relay_reconnect_verifier(
            &first.device_id,
            &first.identity_public_sec1,
            &[6; 32],
            first.issued_at + 2,
        )
        .unwrap();
        let pending = relay_pending_row(4, 2, 3);
        db.insert_relay_pending_device(&pending, pending.issued_at + 2)
            .unwrap();
        assert_eq!(
            db.insert_relay_pending_device(&relay_pending_row(5, 2, 9), pending.issued_at + 2)
                .unwrap(),
            RelayPendingInsert::Conflict
        );
        assert_eq!(
            db.relay_device(&first.device_id).unwrap(),
            Some(before.clone())
        );
        // 새 epoch UPDATE 이후 pending 소비가 실패해도 이전 승인과 verifier가 복원돼야 한다.
        db.conn.execute_batch("CREATE TRIGGER reject_pending_consume BEFORE DELETE ON relay_pending_devices BEGIN SELECT RAISE(ABORT, 'test rollback'); END;").unwrap();
        assert!(
            db.approve_relay_pending_device(
                &pending.pairing_id,
                &pending.identity_public_sec1,
                pending.issued_at + 3
            )
            .is_err()
        );
        assert_eq!(db.relay_device(&first.device_id).unwrap(), Some(before));
        assert_eq!(
            db.relay_reconnect_verifier(&first.device_id).unwrap(),
            Some([6; 32])
        );
        assert!(
            db.relay_pending_device(&pending.pairing_id)
                .unwrap()
                .is_some()
        );
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn relay_v38_epoch_backfill_preserves_approved_devices_and_reconnect_verifiers() {
        let (dir, path, db) = file_db("relay-epoch-backfill");
        drop(db);
        // 별도 빈 DB에 v38까지 설치해 기존 승인 행을 직접 만든다.
        let legacy_path = dir.join("v38.sqlite3");
        let legacy = storage_core::open_with_migrations(&legacy_path, &MIGRATIONS[..38]).unwrap();
        let pending = relay_pending_row(1, 2, 3);
        legacy.execute("INSERT INTO relay_devices (device_id, identity_public_sec1, display_name, permission_view, permission_input, permission_upload, permission_approval, issued_at, device_expires_at, last_seen_at, revoked_at, reconnect_verifier) VALUES (?1, ?2, ?3, 1, 0, 0, 0, ?4, ?5, NULL, NULL, ?6)", rusqlite::params![pending.device_id.as_slice(), pending.identity_public_sec1.as_slice(), pending.display_name, pending.issued_at, pending.device_expires_at, [6u8; 32].as_slice()]).unwrap();
        let logical = secret::LogicalCredentialId::new("relay-v38-secret").unwrap();
        let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        legacy.execute("INSERT INTO physical_secret_slot_ledger (physical_slot,logical_credential_id,state,created_at,updated_at) VALUES (?1,?2,'staging',1,1)", (slot.as_str(),logical.as_str())).unwrap();
        drop(legacy);
        let migrated = Db::open(&legacy_path).unwrap();
        assert_eq!(
            Db::read_user_version(&migrated.conn).unwrap(),
            MIGRATIONS.len()
        );
        let row = migrated.relay_device(&pending.device_id).unwrap().unwrap();
        assert_ne!(row.authorization_epoch, [0; 16]);
        assert_eq!(row.identity_public_sec1, pending.identity_public_sec1);
        assert_eq!(
            (row.issued_at, row.device_expires_at),
            (pending.issued_at, pending.device_expires_at)
        );
        assert!(
            row.permission_view
                && !row.permission_input
                && !row.permission_upload
                && !row.permission_approval
        );
        assert_eq!(row.revoked_at, None);
        assert_eq!(
            migrated
                .relay_reconnect_verifier(&pending.device_id)
                .unwrap(),
            Some([6; 32])
        );
        let recovery = migrated
            .physical_secret_slots_for_reconciliation(1)
            .unwrap();
        assert_eq!(recovery[0].physical_slot, slot.as_str());
        assert_ne!(recovery[0].recovery_generation, [0; 16]);
        drop(migrated);
        let reopened = Db::open(&legacy_path).unwrap();
        assert_eq!(
            reopened.relay_device(&pending.device_id).unwrap(),
            Some(row)
        );
        drop(reopened);
        assert!(path.is_file());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn relay_reconnect_verifier_persists_and_revocation_prevents_reregistration() {
        let (dir, path, db) = file_db("relay-reconnect");
        let pending = relay_pending_row(1, 2, 3);
        db.insert_relay_pending_device(&pending, pending.issued_at)
            .unwrap();
        db.approve_relay_pending_device(
            &pending.pairing_id,
            &pending.identity_public_sec1,
            pending.issued_at + 1,
        )
        .unwrap();
        assert!(
            db.store_relay_reconnect_verifier(
                &pending.device_id,
                &pending.identity_public_sec1,
                &[7; 32],
                pending.issued_at + 2
            )
            .unwrap()
        );
        assert!(
            !db.store_relay_reconnect_verifier(
                &pending.device_id,
                &pending.identity_public_sec1,
                &[8; 32],
                pending.issued_at + 2
            )
            .unwrap()
        );
        assert!(
            !db.store_relay_reconnect_verifier(
                &pending.device_id,
                &[9; 65],
                &[7; 32],
                pending.issued_at + 2
            )
            .unwrap()
        );
        assert!(
            !db.store_relay_reconnect_verifier(
                &pending.device_id,
                &pending.identity_public_sec1,
                &[7; 32],
                pending.device_expires_at
            )
            .unwrap()
        );
        drop(db);
        let db = Db::open(&path).unwrap();
        assert_eq!(
            db.relay_reconnect_verifier(&pending.device_id).unwrap(),
            Some([7; 32])
        );
        db.revoke_relay_device(&pending.device_id, pending.issued_at + 3)
            .unwrap();
        assert_eq!(
            db.relay_reconnect_verifier(&pending.device_id).unwrap(),
            None
        );
        assert!(
            !db.store_relay_reconnect_verifier(
                &pending.device_id,
                &pending.identity_public_sec1,
                &[7; 32],
                pending.issued_at + 4
            )
            .unwrap()
        );
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn relay_reconnect_schema_stores_only_a_bounded_verifier() {
        let db = Db::open_in_memory().unwrap();
        assert!(
            db.conn
                .prepare("SELECT reconnect_verifier FROM relay_devices")
                .is_ok(),
            "재접속 verifier migration이 필요하다"
        );
    }

    /// 페어링 마감(5분)과 기기 만료(장기)는 서로 다른 수명이다. 한 컬럼으로 합치면
    /// 5분짜리 승인 창이 24시간으로 늘어나거나 기기가 5분 만에 죽는다.
    #[test]
    fn relay_pairing_window_and_device_expiry_are_independent() {
        let (dir, _path, db) = file_db("relay-split-expiry");
        let mut too_long = relay_pending_row(1, 1, 1);
        too_long.pairing_expires_at = too_long.issued_at + RELAY_PAIRING_WINDOW_SECS_MAX + 1;
        assert!(
            db.insert_relay_pending_device(&too_long, too_long.issued_at)
                .is_err(),
            "pairing window must stay bounded by the five-minute ceremony"
        );

        let mut inverted = relay_pending_row(2, 2, 2);
        inverted.device_expires_at = inverted.pairing_expires_at - 1;
        assert!(
            db.insert_relay_pending_device(&inverted, inverted.issued_at)
                .is_err(),
            "a device must not expire before the pairing deadline"
        );

        let pending = relay_pending_row(3, 3, 3);
        db.insert_relay_pending_device(&pending, pending.issued_at)
            .unwrap();
        let RelayDeviceApproval::Approved(device) = db
            .approve_relay_pending_device(
                &pending.pairing_id,
                &pending.identity_public_sec1,
                pending.pairing_expires_at - 1,
            )
            .unwrap()
        else {
            panic!("approval inside the pairing window must succeed");
        };
        assert_eq!(device.issued_at, pending.pairing_expires_at - 1);
        assert_eq!(device.device_expires_at, pending.device_expires_at);
        assert!(
            db.touch_relay_device(&device.device_id, pending.pairing_expires_at + 1)
                .unwrap(),
            "an admitted device outlives the pairing deadline"
        );
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn relay_approval_fails_closed_at_the_exact_pairing_deadline() {
        let (dir, _path, db) = file_db("relay-pairing-deadline");
        let pending = relay_pending_row(4, 4, 4);
        db.insert_relay_pending_device(&pending, pending.issued_at)
            .unwrap();
        assert_eq!(
            db.approve_relay_pending_device(
                &pending.pairing_id,
                &pending.identity_public_sec1,
                pending.pairing_expires_at
            )
            .unwrap(),
            RelayDeviceApproval::Expired
        );
        assert_eq!(db.relay_pending_device_count().unwrap(), 0);
        assert!(db.relay_device(&pending.device_id).unwrap().is_none());
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    /// SQLite는 곡선 위 점인지 검증하지 못한다 — 길이/접두사만 본다. 그래서 65바이트
    /// `0x04` 접두사를 가진 잘못된 P-256 점은 여기까지 그대로 들어온다. 곡선 검증은
    /// 값 생성자를 통과시키는 앱 어댑터의 책임이라는 것을 이 테스트가 고정한다.
    #[test]
    fn relay_rows_accept_structurally_valid_off_curve_keys_for_adapter_rejection() {
        let (dir, _path, db) = file_db("relay-off-curve");
        let mut pending = relay_pending_row(5, 5, 5);
        pending.identity_public_sec1 = off_curve_sec1_point();
        db.insert_relay_pending_device(&pending, pending.issued_at)
            .unwrap();
        let stored = db
            .relay_pending_device(&pending.pairing_id)
            .unwrap()
            .expect("structurally valid row is readable");
        assert_eq!(stored.identity_public_sec1, pending.identity_public_sec1);
        assert!(matches!(
            db.approve_relay_pending_device(
                &pending.pairing_id,
                &pending.identity_public_sec1,
                pending.issued_at + 1,
            )
            .unwrap(),
            RelayDeviceApproval::Approved(_)
        ));
        let device = db.relay_device(&pending.device_id).unwrap().unwrap();
        assert_eq!(device.identity_public_sec1, pending.identity_public_sec1);
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    /// 재시작하면 검증된 메모리 승인(PairingApproval)이 사라진다. 남은 pending 행이
    /// 페어링 증거로 되살아나지 못하도록 앱이 시작 시 전부 지운다.
    #[test]
    fn relay_pending_rows_are_purgeable_after_restart() {
        let (dir, path, db) = file_db("relay-restart-purge");
        for value in 1..=3u8 {
            let pending = relay_pending_row(value, value, value);
            db.insert_relay_pending_device(&pending, pending.issued_at)
                .unwrap();
        }
        drop(db);

        let reopened = Db::open(&path).unwrap();
        assert_eq!(reopened.relay_pending_device_count().unwrap(), 3);
        assert_eq!(reopened.delete_all_relay_pending_devices().unwrap(), 3);
        assert_eq!(reopened.relay_pending_device_count().unwrap(), 0);
        assert_eq!(reopened.delete_all_relay_pending_devices().unwrap(), 0);
        drop(reopened);
        fs::remove_dir_all(dir).unwrap();
    }

    /// pending/기기 상한은 결정적으로 거절하고 기존 행을 절대 밀어내지 않는다.
    /// (암호 연산이 없는 평행 행으로 경계를 그대로 재현한다.)
    #[test]
    fn relay_row_limits_reject_deterministically_without_eviction() {
        let (dir, _path, db) = file_db("relay-limits");
        for index in 0..RELAY_PENDING_DEVICE_ROWS_MAX {
            let row = relay_filler_row(index);
            assert_eq!(
                db.insert_relay_pending_device(&row, row.issued_at).unwrap(),
                RelayPendingInsert::Stored
            );
        }
        let overflow = relay_filler_row(RELAY_PENDING_DEVICE_ROWS_MAX);
        assert_eq!(
            db.insert_relay_pending_device(&overflow, overflow.issued_at)
                .unwrap(),
            RelayPendingInsert::LimitReached
        );
        assert_eq!(
            db.relay_pending_device_count().unwrap(),
            RELAY_PENDING_DEVICE_ROWS_MAX
        );
        // 같은 기기(device_id·공개키 동일)의 재시도는 옛 pending 행을 대체한다 — 행 수는
        // 그대로다. 상한에 닿아 있어도 대체는 새 행을 늘리지 않으므로 통과한다.
        let retry = relay_filler_row(0);
        assert_eq!(
            db.insert_relay_pending_device(&retry, retry.issued_at)
                .unwrap(),
            RelayPendingInsert::Stored
        );
        assert_eq!(
            db.relay_pending_device_count().unwrap(),
            RELAY_PENDING_DEVICE_ROWS_MAX
        );

        for index in 0..RELAY_DEVICE_ROWS_MAX {
            let row = relay_filler_row(index);
            assert!(matches!(
                db.approve_relay_pending_device(
                    &row.pairing_id,
                    &row.identity_public_sec1,
                    row.issued_at + 1
                )
                .unwrap(),
                RelayDeviceApproval::Approved(_)
            ));
        }
        let extra = relay_filler_row(RELAY_DEVICE_ROWS_MAX);
        assert_eq!(
            db.approve_relay_pending_device(
                &extra.pairing_id,
                &extra.identity_public_sec1,
                extra.issued_at + 1
            )
            .unwrap(),
            RelayDeviceApproval::DeviceLimitReached
        );
        assert_eq!(
            db.list_relay_devices_bounded(RELAY_DEVICE_ROWS_MAX)
                .unwrap()
                .len(),
            RELAY_DEVICE_ROWS_MAX
        );
        assert!(
            db.list_relay_devices_bounded(RELAY_DEVICE_ROWS_MAX - 1)
                .is_err(),
            "상한을 넘는 스냅샷은 잘라서 주지 않고 거부한다"
        );
        assert!(
            db.relay_device(&extra.device_id).unwrap().is_none(),
            "상한 거부는 기기를 발행하지 않는다"
        );
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    /// 앱 어댑터가 곡선 검증을 마친 뒤 승인 트랜잭션 사이에 외부에서 공개키를
    /// 바꿔치기하는 경쟁을 막는다 — 승인은 아무것도 발행하지 않고 롤백한다.
    #[test]
    fn relay_approval_rejects_a_pending_row_that_changed_after_validation() {
        let (dir, _path, db) = file_db("relay-approval-race");
        let pending = relay_pending_row(12, 12, 12);
        db.insert_relay_pending_device(&pending, pending.issued_at)
            .unwrap();

        // 검증 시점에 본 값(원본)과 저장된 값(변조본)이 어긋난 상태를 그대로 재현한다.
        db.conn
            .execute(
                "UPDATE relay_pending_devices SET identity_public_sec1 = ?1 WHERE pairing_id = ?2",
                rusqlite::params![
                    off_curve_sec1_point().as_slice(),
                    pending.pairing_id.as_slice()
                ],
            )
            .unwrap();

        assert!(
            db.approve_relay_pending_device(
                &pending.pairing_id,
                &pending.identity_public_sec1,
                pending.issued_at + 1,
            )
            .is_err()
        );
        assert!(db.relay_device(&pending.device_id).unwrap().is_none());
        assert_eq!(db.relay_pending_device_count().unwrap(), 1);

        // 변조본을 그대로 기대값으로 넘겨도 검증은 앱 어댑터가 이미 막는다. 저장 계층은
        // "검증한 값과 같은가"만 책임진다.
        assert!(matches!(
            db.approve_relay_pending_device(
                &pending.pairing_id,
                &off_curve_sec1_point(),
                pending.issued_at + 1,
            )
            .unwrap(),
            RelayDeviceApproval::Approved(_)
        ));
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    /// 취소된 기기는 상한을 차지하지 않는다. 64대를 페어링하고 전부 취소한 뒤에도 65번째가
    /// 들어오고, 표 크기는 상한을 지킨다(가장 오래전 취소된 행이 회수된다).
    #[test]
    fn relay_revoked_devices_do_not_hold_the_device_limit() {
        let (dir, _path, db) = file_db("relay-revoked-reclaim");
        for index in 0..RELAY_DEVICE_ROWS_MAX {
            let row = relay_filler_row(index);
            db.insert_relay_pending_device(&row, row.issued_at).unwrap();
            db.approve_relay_pending_device(
                &row.pairing_id,
                &row.identity_public_sec1,
                row.issued_at + 1,
            )
            .unwrap();
            db.revoke_relay_device(&row.device_id, row.issued_at + 2 + index as i64)
                .unwrap();
        }
        let next = relay_filler_row(RELAY_DEVICE_ROWS_MAX);
        db.insert_relay_pending_device(&next, next.issued_at)
            .unwrap();
        assert!(matches!(
            db.approve_relay_pending_device(
                &next.pairing_id,
                &next.identity_public_sec1,
                next.issued_at + 1,
            )
            .unwrap(),
            RelayDeviceApproval::Approved(_)
        ));
        let rows = db
            .list_relay_devices_bounded(RELAY_DEVICE_ROWS_MAX)
            .unwrap();
        assert_eq!(rows.len(), RELAY_DEVICE_ROWS_MAX, "표 크기는 상한을 지킨다");
        assert!(
            rows.iter().any(|row| row.device_id == next.device_id),
            "새 기기가 발행됐다"
        );
        assert!(
            !rows
                .iter()
                .any(|row| row.device_id == relay_filler_row(0).device_id),
            "가장 오래전에 취소된 행이 회수된다"
        );

        // 살아 있는 기기가 상한이면 여전히 거절한다.
        let (dir2, _path2, db2) = file_db("relay-active-limit");
        for index in 0..RELAY_DEVICE_ROWS_MAX {
            let row = relay_filler_row(index);
            db2.insert_relay_pending_device(&row, row.issued_at)
                .unwrap();
            db2.approve_relay_pending_device(
                &row.pairing_id,
                &row.identity_public_sec1,
                row.issued_at + 1,
            )
            .unwrap();
        }
        let extra = relay_filler_row(RELAY_DEVICE_ROWS_MAX);
        db2.insert_relay_pending_device(&extra, extra.issued_at)
            .unwrap();
        assert_eq!(
            db2.approve_relay_pending_device(
                &extra.pairing_id,
                &extra.identity_public_sec1,
                extra.issued_at + 1,
            )
            .unwrap(),
            RelayDeviceApproval::DeviceLimitReached
        );
        drop(db);
        drop(db2);
        fs::remove_dir_all(dir).unwrap();
        fs::remove_dir_all(dir2).unwrap();
    }

    /// 같은 폰(같은 공개키)이 새 device_id로 다시 페어링하면 옛 행이 대체된다 —
    /// identity UNIQUE에 걸려 불투명하게 실패하지 않는다.
    #[test]
    fn relay_repairing_the_same_phone_under_a_new_device_id_replaces_the_old_row() {
        let (dir, _path, db) = file_db("relay-repair");
        let first = relay_pending_row(21, 21, 21);
        db.insert_relay_pending_device(&first, first.issued_at)
            .unwrap();
        db.approve_relay_pending_device(
            &first.pairing_id,
            &first.identity_public_sec1,
            first.issued_at + 1,
        )
        .unwrap();
        db.revoke_relay_device(&first.device_id, first.issued_at + 2)
            .unwrap();

        let mut again = relay_pending_row(22, 22, 21); // 같은 키, 새 device_id
        again.display_name = "same phone".to_owned();
        db.insert_relay_pending_device(&again, again.issued_at)
            .unwrap();
        assert!(matches!(
            db.approve_relay_pending_device(
                &again.pairing_id,
                &again.identity_public_sec1,
                again.issued_at + 3,
            )
            .unwrap(),
            RelayDeviceApproval::Approved(_)
        ));
        assert!(db.relay_device(&first.device_id).unwrap().is_none());
        let current = db.relay_device(&again.device_id).unwrap().unwrap();
        assert_eq!(current.display_name, "same phone");
        assert_eq!(current.revoked_at, None);
        assert_eq!(db.relay_pending_device_count().unwrap(), 0);
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    /// 미완의 의식을 버리고 다시 시도하는 같은 기기는 대체되고, 다른 주체의 부분 겹침은
    /// 결정적으로 거절된다.
    #[test]
    fn relay_pending_retry_supersedes_and_partial_overlap_conflicts() {
        let (dir, _path, db) = file_db("relay-pending-retry");
        let abandoned = relay_pending_row(31, 31, 31);
        db.insert_relay_pending_device(&abandoned, abandoned.issued_at)
            .unwrap();

        // 같은 기기의 새 의식(새 pairing_id) → 대체.
        let retry = relay_pending_row(32, 31, 31);
        assert_eq!(
            db.insert_relay_pending_device(&retry, retry.issued_at)
                .unwrap(),
            RelayPendingInsert::Stored
        );
        assert_eq!(db.relay_pending_device_count().unwrap(), 1);
        assert!(
            db.relay_pending_device(&abandoned.pairing_id)
                .unwrap()
                .is_none()
        );
        assert!(
            db.relay_pending_device(&retry.pairing_id)
                .unwrap()
                .is_some()
        );

        // 키만 같은 다른 device_id → 충돌.
        let key_clash = relay_pending_row(33, 34, 31);
        assert_eq!(
            db.insert_relay_pending_device(&key_clash, key_clash.issued_at)
                .unwrap(),
            RelayPendingInsert::Conflict
        );
        // device_id만 같은 다른 키 → 충돌.
        let id_clash = relay_pending_row(35, 31, 36);
        assert_eq!(
            db.insert_relay_pending_device(&id_clash, id_clash.issued_at)
                .unwrap(),
            RelayPendingInsert::Conflict
        );
        assert_eq!(
            db.relay_pending_device_count().unwrap(),
            1,
            "충돌은 아무것도 바꾸지 않는다"
        );
        assert!(
            db.relay_pending_device(&retry.pairing_id)
                .unwrap()
                .is_some()
        );
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    /// 재승인은 같은 device_id 위에서 권한을 갱신하고 이전 취소를 지운다.
    #[test]
    fn relay_reapproval_updates_one_device_and_clears_prior_revocation() {
        let (dir, _path, db) = file_db("relay-reapproval");
        let first = relay_pending_row(6, 6, 6);
        db.insert_relay_pending_device(&first, first.issued_at)
            .unwrap();
        db.approve_relay_pending_device(
            &first.pairing_id,
            &first.identity_public_sec1,
            first.issued_at + 1,
        )
        .unwrap();
        db.revoke_relay_device(&first.device_id, first.issued_at + 2)
            .unwrap();

        let mut again = relay_pending_row(7, 6, 7);
        again.display_name = "renamed".to_owned();
        again.permission_input = true;
        db.insert_relay_pending_device(&again, again.issued_at)
            .unwrap();
        assert!(matches!(
            db.approve_relay_pending_device(
                &again.pairing_id,
                &again.identity_public_sec1,
                again.issued_at + 3
            )
            .unwrap(),
            RelayDeviceApproval::Approved(_)
        ));
        let device = db.relay_device(&first.device_id).unwrap().unwrap();
        assert_eq!(device.display_name, "renamed");
        assert_eq!(device.identity_public_sec1, again.identity_public_sec1);
        assert!(device.permission_input);
        assert_eq!(device.revoked_at, None);
        assert_eq!(
            db.list_relay_devices_bounded(RELAY_DEVICE_ROWS_MAX)
                .unwrap()
                .len(),
            1
        );
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    /// last-seen은 발급 이전/만료 이후/취소 이후에는 절대 갱신되지 않는다.
    #[test]
    fn relay_last_seen_updates_only_inside_the_device_window() {
        let (dir, _path, db) = file_db("relay-last-seen");
        let pending = relay_pending_row(8, 8, 8);
        db.insert_relay_pending_device(&pending, pending.issued_at)
            .unwrap();
        db.approve_relay_pending_device(
            &pending.pairing_id,
            &pending.identity_public_sec1,
            pending.issued_at + 1,
        )
        .unwrap();

        assert!(
            !db.touch_relay_device(&pending.device_id, pending.issued_at)
                .unwrap(),
            "발급 이전 시각은 갱신하지 않는다"
        );
        assert!(
            db.touch_relay_device(&pending.device_id, pending.issued_at + 1)
                .unwrap()
        );
        assert!(
            !db.touch_relay_device(&pending.device_id, pending.device_expires_at)
                .unwrap(),
            "만료 시각의 갱신은 실패한다"
        );
        assert_eq!(
            db.relay_device(&pending.device_id)
                .unwrap()
                .unwrap()
                .last_seen_at,
            Some(pending.issued_at + 1)
        );

        db.revoke_relay_device(&pending.device_id, pending.issued_at + 2)
            .unwrap();
        assert!(
            !db.touch_relay_device(&pending.device_id, pending.issued_at + 3)
                .unwrap()
        );
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    /// 존재하지 않는 기기 취소와 반복 취소는 결정적이며 최초 시각을 보존한다.
    #[test]
    fn relay_revocation_is_idempotent_and_keeps_the_first_timestamp() {
        let (dir, _path, db) = file_db("relay-revocation");
        let pending = relay_pending_row(9, 9, 9);
        assert_eq!(
            db.revoke_relay_device(&pending.device_id, pending.issued_at + 1)
                .unwrap(),
            RelayDeviceRevocation::NotFound
        );
        db.insert_relay_pending_device(&pending, pending.issued_at)
            .unwrap();
        db.approve_relay_pending_device(
            &pending.pairing_id,
            &pending.identity_public_sec1,
            pending.issued_at + 1,
        )
        .unwrap();
        for at in [pending.issued_at + 2, pending.issued_at + 3] {
            assert_eq!(
                db.revoke_relay_device(&pending.device_id, at).unwrap(),
                RelayDeviceRevocation::Revoked
            );
        }
        assert_eq!(
            db.relay_device(&pending.device_id)
                .unwrap()
                .unwrap()
                .revoked_at,
            Some(pending.issued_at + 2)
        );
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    fn relay_filler_row(index: usize) -> RelayPendingDeviceRow {
        let mut id = [0u8; RELAY_ID_BYTES];
        id[..8].copy_from_slice(&(index as u64 + 1).to_be_bytes());
        let mut public_key = [0x07u8; RELAY_PUBLIC_KEY_BYTES];
        public_key[0] = 0x04;
        public_key[1..9].copy_from_slice(&(index as u64 + 1).to_be_bytes());
        RelayPendingDeviceRow {
            pairing_id: id,
            device_id: id,
            identity_public_sec1: public_key,
            display_name: format!("filler-{index}"),
            permission_view: true,
            permission_input: false,
            permission_upload: false,
            permission_approval: false,
            issued_at: 1_800_000_000,
            pairing_expires_at: 1_800_000_000 + RELAY_PAIRING_WINDOW_SECS_MAX,
            device_expires_at: 1_800_086_400,
        }
    }

    /// 65바이트/`0x04` 접두사를 만족하지만 곡선 위에 없는 점.
    fn off_curve_sec1_point() -> [u8; RELAY_PUBLIC_KEY_BYTES] {
        let mut point = [0u8; RELAY_PUBLIC_KEY_BYTES];
        point[0] = 0x04;
        point[1..].fill(0x01);
        point
    }

    #[test]
    fn relay_v36_file_migrates_to_current_and_reopens() {
        assert!(MIGRATIONS.len() >= 40);
        let dir = std::env::temp_dir().join(format!(
            "deppy-relay-v36-migration-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        let legacy = storage_core::open_with_migrations(&path, &MIGRATIONS[..36]).unwrap();
        assert_eq!(storage_core::read_user_version(&legacy).unwrap(), 36);
        drop(legacy);

        let migrated = Db::open(&path).unwrap();
        assert_eq!(
            Db::read_user_version(&migrated.conn).unwrap(),
            MIGRATIONS.len()
        );
        for table in ["relay_pending_devices", "relay_devices"] {
            let present: bool = migrated
                .conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(present, "missing {table}");
        }
        drop(migrated);
        let reopened = Db::open(&path).unwrap();
        assert_eq!(
            Db::read_user_version(&reopened.conn).unwrap(),
            MIGRATIONS.len()
        );
        drop(reopened);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupted_relay_pending_row_fails_closed_without_consumption() {
        let (dir, _path, db) = file_db("relay-corrupt-pending");
        let pending = relay_pending_row(1, 1, 1);
        assert_eq!(
            db.insert_relay_pending_device(&pending, pending.issued_at)
                .unwrap(),
            RelayPendingInsert::Stored
        );
        db.conn
            .execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        db.conn
            .execute(
                "UPDATE relay_pending_devices SET identity_public_sec1 = zeroblob(4096)
                 WHERE pairing_id = ?1",
                [pending.pairing_id.as_slice()],
            )
            .unwrap();
        db.conn
            .execute_batch("PRAGMA ignore_check_constraints = OFF;")
            .unwrap();

        assert!(
            db.approve_relay_pending_device(
                &pending.pairing_id,
                &pending.identity_public_sec1,
                pending.issued_at + 1,
            )
            .is_err()
        );
        assert_eq!(db.relay_pending_device_count().unwrap(), 1);
        assert!(db.relay_device(&pending.device_id).unwrap().is_none());
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn relay_approval_with_a_known_key_replaces_the_previous_device_row() {
        let (dir, _path, db) = file_db("relay-approval-conflict");
        let first = relay_pending_row(2, 2, 2);
        db.insert_relay_pending_device(&first, first.issued_at)
            .unwrap();
        assert!(matches!(
            db.approve_relay_pending_device(
                &first.pairing_id,
                &first.identity_public_sec1,
                first.issued_at + 1
            )
            .unwrap(),
            RelayDeviceApproval::Approved(_)
        ));

        // 같은 공개키의 새 device_id는 **같은 폰의 재페어링**이다 — pending 행은 검증된
        // 승인(키 보유 증명) 뒤에만 존재하므로 옛 행을 대체한다. 결과는 언제나 기기 하나.
        let mut repaired = relay_pending_row(3, 3, 3);
        repaired.identity_public_sec1 = first.identity_public_sec1;
        db.insert_relay_pending_device(&repaired, repaired.issued_at)
            .unwrap();
        assert!(matches!(
            db.approve_relay_pending_device(
                &repaired.pairing_id,
                &repaired.identity_public_sec1,
                repaired.issued_at + 1
            )
            .unwrap(),
            RelayDeviceApproval::Approved(_)
        ));
        assert_eq!(db.relay_pending_device_count().unwrap(), 0);
        assert!(db.relay_device(&first.device_id).unwrap().is_none());
        let current = db.relay_device(&repaired.device_id).unwrap().unwrap();
        assert_eq!(current.identity_public_sec1, first.identity_public_sec1);
        assert_eq!(
            db.list_relay_devices_bounded(RELAY_DEVICE_ROWS_MAX)
                .unwrap()
                .len(),
            1,
            "같은 키는 언제나 기기 하나로만 존재한다"
        );
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn relay_device_list_uses_one_read_transaction_before_allocation() {
        let source = include_str!("db.rs");
        let body = source
            .split_once("pub fn list_relay_devices_bounded")
            .unwrap()
            .1
            .split("\n    pub fn ")
            .next()
            .unwrap();
        let transaction = body.find("unchecked_transaction").unwrap();
        let allocation = body.find("Vec::with_capacity").unwrap();
        assert!(transaction < allocation);
        assert!(body.matches("tx.").count() >= 3, "{body}");
    }

    fn authorization_plan(
        operation_id: &str,
        server_id: &str,
        tool_name: &str,
        decision: audit::ApprovalDecision,
    ) -> audit::AuthorizationPlan {
        let audit::AuthorizationEvaluation::NeedsApproval(pending) =
            audit::evaluate_authorization_with_fingerprint(
                operation_id.to_owned(),
                server_id.to_owned(),
                tool_name.to_owned(),
                audit::PermissionFingerprint::Absent,
                audit::schema_hash(r#"{"type":"object"}"#),
            )
            .unwrap()
        else {
            panic!("Ask rule must require approval")
        };
        pending.resolve(decision)
    }

    fn scope_stripe(scope: &str) -> u8 {
        let key = audit::authorization_scope_lock_key(scope).unwrap();
        u8::from_str_radix(&key[..2], 16).unwrap()
    }

    fn noncolliding_scope(scope: &str) -> String {
        let stripe = scope_stripe(scope);
        (0..1024)
            .map(|index| format!("other-scope-{index}"))
            .find(|candidate| scope_stripe(candidate) != stripe)
            .unwrap()
    }

    fn seed_activity_panes(db: &Db, count: usize, title: &str) -> String {
        let workspace_id = db.create_workspace("activity-bounded").unwrap();
        db.conn
            .execute(
                "INSERT INTO mux_windows
                   (id, workspace_id, title, active_tab_id, created_at, updated_at)
                 VALUES ('activity-window', ?1, NULL, NULL, '2026-01-01', '2026-01-01')",
                [&workspace_id],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO mux_tabs
                   (id, window_id, workspace_id, title, tab_index, created_at, updated_at)
                 VALUES ('activity-tab', 'activity-window', ?1, 'tab', 0,
                         '2026-01-01', '2026-01-01')",
                [&workspace_id],
            )
            .unwrap();
        for index in 0..count {
            db.conn
                .execute(
                    "INSERT INTO mux_panes
                       (id, workspace_id, tab_id, session_id, title, pane_kind,
                        created_at, updated_at)
                     VALUES (?1, ?2, 'activity-tab', NULL, ?3, 'terminal', ?4, ?4)",
                    (
                        format!("activity-pane-{index:05}"),
                        &workspace_id,
                        title,
                        format!("2026-01-01T00:00:{index:05}"),
                    ),
                )
                .unwrap();
        }
        workspace_id
    }

    /// 메모는 워크스페이스당 한 장이다(PR-1). 저장·재읽기가 원문 그대로여야 하며
    /// 개행·유니코드가 보존돼야 한다 — 「작업 기록」이 용도라 여러 줄이 기본이다.
    #[test]
    fn workspace_note는_원문_그대로_라운드트립한다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("메모").unwrap();

        // 미작성 워크스페이스는 None — 빈 문자열과 구분한다(UI가 placeholder를 고른다).
        assert_eq!(db.load_workspace_note(&ws).unwrap(), None);

        let body = "2026-08-10\n- 인증 리팩터 중\n- TODO: 캐시 무효화 🔑";
        db.save_workspace_note(&ws, body).unwrap();
        assert_eq!(db.load_workspace_note(&ws).unwrap().as_deref(), Some(body));

        // 같은 워크스페이스에 다시 쓰면 덮어쓴다(행이 늘지 않는다).
        db.save_workspace_note(&ws, "덮어씀").unwrap();
        assert_eq!(
            db.load_workspace_note(&ws).unwrap().as_deref(),
            Some("덮어씀")
        );
        let rows: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM workspace_notes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1, "워크스페이스당 한 행이어야 한다");
    }

    /// 빈 메모는 행을 남기지 않는다. 남기면 워크스페이스마다 빈 행이 쌓이고,
    /// load가 Some("")를 돌려줘 "안 쓴 것"과 "지운 것"이 구분되지 않는다.
    #[test]
    fn 빈_workspace_note는_행을_지운다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("메모").unwrap();
        db.save_workspace_note(&ws, "적었다").unwrap();
        db.save_workspace_note(&ws, "   \n  ").unwrap();
        assert_eq!(db.load_workspace_note(&ws).unwrap(), None);
        let rows: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM workspace_notes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }

    /// 메모는 무제한이 아니다. 설정 스냅샷 경로가 bounded read를 전제로 하므로
    /// 상한을 넘는 쓰기는 **거부**한다 — 자르면 사용자가 모르게 글이 사라진다.
    #[test]
    fn workspace_note는_상한을_넘으면_거부한다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("메모").unwrap();

        let at_limit = "a".repeat(WORKSPACE_NOTE_MAX_BYTES);
        db.save_workspace_note(&ws, &at_limit).unwrap();
        assert_eq!(
            db.load_workspace_note(&ws).unwrap().map(|s| s.len()),
            Some(WORKSPACE_NOTE_MAX_BYTES)
        );

        let over = "a".repeat(WORKSPACE_NOTE_MAX_BYTES + 1);
        let error = db.save_workspace_note(&ws, &over).unwrap_err().to_string();
        assert!(error.contains("workspace_note_bytes_limit"), "{error}");
        // 거부됐으면 기존 내용이 그대로 남아야 한다.
        assert_eq!(
            db.load_workspace_note(&ws).unwrap().map(|s| s.len()),
            Some(WORKSPACE_NOTE_MAX_BYTES)
        );
    }

    /// FK + ON DELETE CASCADE. `delete_workspace`가 메모를 명시적으로 지우지 않아도
    /// 워크스페이스 행이 사라지면 함께 정리돼야 한다 — 고아 메모가 남으면 같은 id가
    /// 재사용될 때 남의 메모가 되살아난다.
    #[test]
    fn workspace_note는_워크스페이스_삭제와_함께_사라진다() {
        let mut db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("삭제대상").unwrap();
        let other = db.create_workspace("유지").unwrap();
        db.save_workspace_note(&ws, "사라질 메모").unwrap();
        db.save_workspace_note(&other, "남을 메모").unwrap();

        db.delete_workspace(&ws).unwrap();

        assert_eq!(db.load_workspace_note(&ws).unwrap(), None);
        assert_eq!(
            db.load_workspace_note(&other).unwrap().as_deref(),
            Some("남을 메모")
        );
    }

    /// 없는 워크스페이스에는 쓸 수 없다(FK). 이게 막히지 않으면 오타 난 id로
    /// 메모가 새고 CASCADE 대상에서도 빠진다.
    #[test]
    fn 없는_워크스페이스에는_note를_쓸_수_없다() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.save_workspace_note("ghost-ws", "본문").is_err());
    }

    /// v32 기존 DB를 열면 v33으로 올라가고 메모가 바로 쓰인다. 기존 데이터는 보존.
    #[test]
    fn workspace_notes_마이그레이션은_기존_v32_db를_보존한다() {
        let dir = std::env::temp_dir().join(format!("deppy-notes-mig-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..32] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 32).unwrap();
            conn.execute(
                "INSERT INTO workspaces (id, name, path, created_at, updated_at)
                 VALUES ('ws-1', 'existing', '/repo', 't', 't')",
                [],
            )
            .unwrap();
        }

        let db = Db::open(&path).unwrap();
        assert_eq!(Db::read_user_version(&db.conn).unwrap(), MIGRATIONS.len());
        // 기존 워크스페이스 보존
        assert!(db.list_workspaces().unwrap().iter().any(|w| w.id == "ws-1"));
        // 새 테이블 사용 가능
        db.save_workspace_note("ws-1", "업그레이드 후 메모")
            .unwrap();
        assert_eq!(
            db.load_workspace_note("ws-1").unwrap().as_deref(),
            Some("업그레이드 후 메모")
        );
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
    fn activity_panes는_현재_mux_layout의_pane만_반환한다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("idle").unwrap();
        for (id, title) in [
            ("session-live", "saved shell"),
            ("session-old", "old shell"),
        ] {
            db.conn
                .execute(
                    "INSERT INTO sessions
                       (id, workspace_id, session_kind, agent_id, title, command, args_json,
                        cwd, status, created_at, updated_at, last_log_offset)
                     VALUES (?1, ?2, 'shell', NULL, ?3, 'sh', '[]', '/', 'exited',
                        '2026-01-01', '2026-01-01', 0)",
                    (id, &ws, title),
                )
                .unwrap();
        }
        db.conn
            .execute(
                "INSERT INTO mux_windows
                   (id, workspace_id, title, active_tab_id, created_at, updated_at)
                 VALUES ('window-1', ?1, NULL, NULL, '2026-01-01', '2026-01-01')",
                [&ws],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO mux_tabs
                   (id, window_id, workspace_id, title, tab_index, created_at, updated_at)
                 VALUES ('tab-1', 'window-1', ?1, 'tab', 0, '2026-01-01', '2026-01-01')",
                [&ws],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO mux_panes
                   (id, workspace_id, tab_id, session_id, title, pane_kind, created_at, updated_at)
                 VALUES ('pane-1', ?1, 'tab-1', 'session-live', '', 'terminal',
                    '2026-01-01', '2026-01-01')",
                [&ws],
            )
            .unwrap();

        // cwd도 함께 온다 — 기본 제목("셸 N")을 프로젝트명으로 표시하는 데 쓴다.
        assert_eq!(
            db.list_persisted_activity_panes().unwrap(),
            vec![PersistedActivityPane {
                workspace_id: ws.clone(),
                pane_id: "pane-1".to_owned(),
                title: "saved shell".to_owned(),
                cwd: "/".to_owned(),
            }]
        );
        assert_eq!(
            db.list_persisted_activity_panes_bounded(1).unwrap(),
            vec![PersistedActivityPane {
                workspace_id: ws,
                pane_id: "pane-1".to_owned(),
                title: "saved shell".to_owned(),
                cwd: "/".to_owned(),
            }]
        );
    }

    #[test]
    fn cloud_ended_sessions_include_closed_panes_and_exclude_running_sessions() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("workspace").unwrap();
        for (id, status) in [("closed-pane", "exited"), ("live-pane", "running")] {
            db.conn
                .execute(
                    "INSERT INTO sessions
                       (id, workspace_id, session_kind, agent_id, title, command, args_json,
                        cwd, status, created_at, updated_at, last_log_offset)
                     VALUES (?1, ?2, 'shell', NULL, ?1, 'sh', '[]', '/', ?3,
                        '2026-01-01', '2026-01-01', 0)",
                    (id, &ws, status),
                )
                .unwrap();
        }

        let rows = db.list_cloud_ended_sessions().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "closed-pane");
        assert_eq!(rows[0].workspace_id, ws);
    }

    #[test]
    fn cloud_ended_sessions_read_only_connection_reads_persisted_history() {
        let dir =
            std::env::temp_dir().join(format!("deppy-cloud-history-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.sqlite");
        {
            let db = Db::open(&path).unwrap();
            let ws = db.create_workspace("history").unwrap();
            db.conn
                .execute(
                    "INSERT INTO sessions
                       (id, workspace_id, session_kind, agent_id, title, command, args_json,
                        cwd, status, created_at, updated_at, last_log_offset)
                     VALUES ('ended-id', ?1, 'shell', NULL, 'old task', 'sh', '[]', '/', 'exited',
                        '2026-01-01', '2026-01-01', 0)",
                    [&ws],
                )
                .unwrap();
        }
        let rows = Db::list_cloud_ended_sessions_from_path(&path).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "old task");
        assert_eq!(rows[0].workspace_name, "history");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bounded_activity_panes는_exact_limit과_plus_one을_구분한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace_id = seed_activity_panes(&db, 3, "saved shell");
        let rows = db.list_persisted_activity_panes_bounded(3).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| row.workspace_id == workspace_id));
        assert_eq!(
            rows.iter()
                .map(|row| row.pane_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "activity-pane-00000",
                "activity-pane-00001",
                "activity-pane-00002"
            ]
        );
        assert_eq!(
            db.list_persisted_activity_panes_bounded(2)
                .unwrap_err()
                .to_string(),
            BOUNDED_READ_LIMIT_EXCEEDED
        );
    }

    #[test]
    fn bounded_activity_panes는_corrupt_type을_materialize하지_않는다() {
        let db = Db::open_in_memory().unwrap();
        seed_activity_panes(&db, 1, "saved shell");
        db.conn
            .execute(
                "UPDATE mux_panes SET title = CAST(x'7879' AS BLOB)
                  WHERE id = 'activity-pane-00000'",
                [],
            )
            .unwrap();
        assert_eq!(
            db.list_persisted_activity_panes_bounded(1)
                .unwrap_err()
                .to_string(),
            BOUNDED_READ_ROW_INVALID
        );
    }

    #[test]
    fn bounded_activity_panes는_pane_id_byte_limit을_검사한다() {
        let db = Db::open_in_memory().unwrap();
        seed_activity_panes(&db, 1, "saved shell");
        db.conn
            .execute(
                "UPDATE mux_panes SET id = ?1 WHERE id = 'activity-pane-00000'",
                [&"p".repeat(BOUNDED_ID_BYTES_MAX + 1)],
            )
            .unwrap();
        assert_eq!(
            db.list_persisted_activity_panes_bounded(1)
                .unwrap_err()
                .to_string(),
            BOUNDED_READ_ROW_INVALID
        );
    }

    #[test]
    fn bounded_activity_panes는_text_byte_limit을_검사한다() {
        let db = Db::open_in_memory().unwrap();
        seed_activity_panes(&db, 1, "saved shell");
        db.conn
            .execute(
                "UPDATE mux_panes SET title = ?1 WHERE id = 'activity-pane-00000'",
                [&"x".repeat(BOUNDED_TEXT_BYTES_MAX + 1)],
            )
            .unwrap();
        assert_eq!(
            db.list_persisted_activity_panes_bounded(1)
                .unwrap_err()
                .to_string(),
            BOUNDED_READ_ROW_INVALID
        );
    }

    #[test]
    fn bounded_activity_panes는_aggregate_byte_budget을_넘기지_않는다() {
        let db = Db::open_in_memory().unwrap();
        seed_activity_panes(&db, 1_025, &"x".repeat(BOUNDED_TEXT_BYTES_MAX));
        assert_eq!(
            db.list_persisted_activity_panes_bounded(1_025)
                .unwrap_err()
                .to_string(),
            BOUNDED_READ_ROW_INVALID
        );
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
    fn pane_task_prompt_survives_reconcile_and_clears_on_new_native_session() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("titles").unwrap();
        let reconcile = |native: &str, prompt: Option<&str>| {
            let mut job = AgentStateJob::projection(ws.clone());
            job.binding_reconcile = Some(AgentSessionBindingReconcile {
                live_pane_ids: vec!["pane-a".into()],
                desired_bindings: vec![AgentSessionRow {
                    pane_id: "pane-a".into(),
                    kind: "claude".into(),
                    session_id: native.into(),
                    task_prompt: prompt.map(str::to_owned),
                }],
            });
            db.apply_agent_state_job(&job).unwrap();
        };
        reconcile("native-1", Some("한글 경로 수정"));
        reconcile("native-1", None);
        assert_eq!(
            db.list_agent_sessions(&ws).unwrap()[0]
                .task_prompt
                .as_deref(),
            Some("한글 경로 수정")
        );
        reconcile("native-2", None);
        assert_eq!(db.list_agent_sessions(&ws).unwrap()[0].task_prompt, None);
    }

    #[test]
    fn shared_native_panes_restore_distinct_task_prompts_after_db_reopen() {
        let (dir, path, db) = file_db("pane-task-restore");
        let ws = db.create_workspace("shared-native").unwrap();
        let mut job = AgentStateJob::projection(ws.clone());
        job.binding_reconcile = Some(AgentSessionBindingReconcile {
            live_pane_ids: vec!["pane-a".into(), "pane-b".into()],
            desired_bindings: [("pane-a", "첫 작업"), ("pane-b", "둘째 작업")]
                .into_iter()
                .map(|(pane, prompt)| AgentSessionRow {
                    pane_id: pane.into(),
                    kind: "claude".into(),
                    session_id: "shared-native".into(),
                    task_prompt: Some(prompt.into()),
                })
                .collect(),
        });
        db.apply_agent_state_job(&job).unwrap();
        drop(db);
        let reopened = Db::open(&path).unwrap();
        let rows = reopened.list_agent_sessions_bounded(&ws, 2).unwrap();
        let titles = rows
            .into_iter()
            .map(|row| (row.pane_id, row.task_prompt))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(titles["pane-a"].as_deref(), Some("첫 작업"));
        assert_eq!(titles["pane-b"].as_deref(), Some("둘째 작업"));
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn structured_threads_crud와_archive_filter_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("structured").unwrap();
        db.upsert_structured_thread(
            "local-1",
            &ws,
            "thread-1",
            "첫 작업",
            "/repo",
            Some("gpt-test"),
            false,
            false,
        )
        .unwrap();
        db.upsert_structured_thread(
            "local-2",
            &ws,
            "thread-2",
            "두 번째 작업",
            "/repo/sub",
            None,
            true,
            true,
        )
        .unwrap();

        let active = db.list_structured_threads(&ws, false).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].local_session_id, "local-1");
        assert_eq!(active[0].thread_id, "thread-1");
        assert_eq!(active[0].model.as_deref(), Some("gpt-test"));

        let all = db.list_structured_threads(&ws, true).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].local_session_id, "local-2");
        assert!(all[0].favorite);
        assert!(all[0].archived);

        assert!(db.set_structured_thread_favorite("local-1", true).unwrap());
        assert!(db.set_structured_thread_archived("local-1", true).unwrap());
        assert!(db.touch_structured_thread("local-1").unwrap());
        assert!(db.list_structured_threads(&ws, false).unwrap().is_empty());
        assert!(db.delete_structured_thread("local-1").unwrap());
        assert!(!db.delete_structured_thread("local-missing").unwrap());
        assert_eq!(db.list_structured_threads(&ws, true).unwrap().len(), 1);
    }

    #[test]
    fn structured_thread_upsert는_created_at을_보존하고_메타데이터를_갱신한다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("structured").unwrap();
        db.upsert_structured_thread(
            "local-1",
            &ws,
            "thread-1",
            "초기 제목",
            "/repo",
            Some("model-a"),
            false,
            false,
        )
        .unwrap();
        let created_at = db.list_structured_threads(&ws, true).unwrap()[0].created_at;

        db.upsert_structured_thread(
            "local-1",
            &ws,
            "thread-1",
            "갱신 제목",
            "/repo/new",
            Some("model-b"),
            true,
            false,
        )
        .unwrap();
        let rows = db.list_structured_threads(&ws, true).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].created_at, created_at);
        assert_eq!(rows[0].title, "갱신 제목");
        assert_eq!(rows[0].cwd, "/repo/new");
        assert_eq!(rows[0].model.as_deref(), Some("model-b"));
        assert!(rows[0].favorite);
    }

    #[test]
    fn structured_thread_id는_로컬_세션과_일대일이다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("structured").unwrap();
        db.upsert_structured_thread(
            "local-1", &ws, "thread-1", "one", "/repo", None, false, false,
        )
        .unwrap();
        assert!(
            db.upsert_structured_thread(
                "local-2",
                &ws,
                "thread-1",
                "duplicate",
                "/repo",
                None,
                false,
                false,
            )
            .is_err()
        );
        assert_eq!(db.list_structured_threads(&ws, true).unwrap().len(), 1);
    }

    #[test]
    fn structured_thread_admission은_필드와_행_byte_상한의_exact만_허용한다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("structured-bounds").unwrap();
        let exact_id = "i".repeat(STRUCTURED_THREAD_ID_BYTES_MAX);
        let exact_thread_id = "t".repeat(STRUCTURED_THREAD_ID_BYTES_MAX);
        let exact_cwd = "c".repeat(STRUCTURED_THREAD_CWD_BYTES_MAX);
        let exact_model = "m".repeat(STRUCTURED_THREAD_MODEL_BYTES_MAX);
        db.upsert_structured_thread(
            &exact_id,
            &ws,
            &exact_thread_id,
            "",
            &exact_cwd,
            Some(&exact_model),
            false,
            false,
        )
        .unwrap();

        let id_too_large = "i".repeat(STRUCTURED_THREAD_ID_BYTES_MAX + 1);
        assert_eq!(
            db.upsert_structured_thread(
                &id_too_large,
                &ws,
                "thread-id-plus-one",
                "",
                "",
                None,
                false,
                false,
            )
            .unwrap_err()
            .to_string(),
            STRUCTURED_THREAD_INPUT_INVALID
        );
        let cwd_too_large = "c".repeat(STRUCTURED_THREAD_CWD_BYTES_MAX + 1);
        assert_eq!(
            db.upsert_structured_thread(
                "local-cwd-plus-one",
                &ws,
                "thread-cwd-plus-one",
                "",
                &cwd_too_large,
                None,
                false,
                false,
            )
            .unwrap_err()
            .to_string(),
            STRUCTURED_THREAD_INPUT_INVALID
        );
        let model_too_large = "m".repeat(STRUCTURED_THREAD_MODEL_BYTES_MAX + 1);
        assert_eq!(
            db.upsert_structured_thread(
                "local-model-plus-one",
                &ws,
                "thread-model-plus-one",
                "",
                "",
                Some(&model_too_large),
                false,
                false,
            )
            .unwrap_err()
            .to_string(),
            STRUCTURED_THREAD_INPUT_INVALID
        );

        let exact_local = "local-row-exact";
        let exact_thread = "thread-row-exact";
        let exact_title_bytes =
            STRUCTURED_THREAD_ROW_BYTES_MAX - exact_local.len() - ws.len() - exact_thread.len();
        db.upsert_structured_thread(
            exact_local,
            &ws,
            exact_thread,
            &"x".repeat(exact_title_bytes),
            "",
            None,
            false,
            false,
        )
        .unwrap();

        let plus_local = "local-row-plus-one";
        let plus_thread = "thread-row-plus-one";
        let plus_title_bytes =
            STRUCTURED_THREAD_ROW_BYTES_MAX + 1 - plus_local.len() - ws.len() - plus_thread.len();
        assert_eq!(
            db.upsert_structured_thread(
                plus_local,
                &ws,
                plus_thread,
                &"x".repeat(plus_title_bytes),
                "",
                None,
                false,
                false,
            )
            .unwrap_err()
            .to_string(),
            STRUCTURED_THREAD_INPUT_INVALID
        );
        assert_eq!(
            db.upsert_structured_thread(
                "local\ncontrol",
                &ws,
                "thread-control-id",
                "",
                "",
                None,
                false,
                false,
            )
            .unwrap_err()
            .to_string(),
            STRUCTURED_THREAD_INPUT_INVALID
        );
        assert_eq!(
            db.upsert_structured_thread(
                "local-cwd-nul",
                &ws,
                "thread-cwd-nul",
                "",
                "/repo\0hidden",
                None,
                false,
                false,
            )
            .unwrap_err()
            .to_string(),
            STRUCTURED_THREAD_INPUT_INVALID
        );
        assert_eq!(db.list_structured_threads(&ws, true).unwrap().len(), 2);
    }

    #[test]
    fn structured_thread_selected_cte는_rowid만_materialize한다() {
        let selected_projection = STRUCTURED_THREADS_BOUNDED_QUERY
            .split_once("FROM structured_threads")
            .unwrap()
            .0;
        assert!(selected_projection.contains("SELECT rowid"));
        for raw_field in [
            "local_session_id",
            "workspace_id",
            "thread_id",
            "title",
            "cwd",
            "model",
        ] {
            assert!(!selected_projection.contains(raw_field), "{raw_field}");
        }
        assert!(STRUCTURED_THREADS_BOUNDED_QUERY.contains("selected AS MATERIALIZED"));
        assert!(STRUCTURED_THREADS_BOUNDED_QUERY.contains("validation AS MATERIALIZED"));
    }

    #[test]
    fn structured_thread_bounded_list는_기존_tie_break_order를_보존한다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("structured-order").unwrap();
        for id in ["z", "b", "a"] {
            db.upsert_structured_thread(
                &format!("local-{id}"),
                &ws,
                &format!("thread-{id}"),
                "",
                "",
                None,
                false,
                false,
            )
            .unwrap();
        }
        db.conn
            .execute(
                "UPDATE structured_threads SET updated_at = 1 WHERE workspace_id = ?1",
                [&ws],
            )
            .unwrap();

        let ids = db
            .list_structured_threads_bounded(&ws, true, 2)
            .unwrap()
            .into_iter()
            .map(|row| row.local_session_id)
            .collect::<Vec<_>>();
        assert_eq!(ids, ["local-a", "local-b"]);
    }

    #[test]
    fn structured_thread_query는_bounded_control_id를_clone전에_거부한다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("structured-control").unwrap();
        db.upsert_structured_thread(
            "local-control",
            &ws,
            "thread-control",
            "title",
            "/repo",
            None,
            false,
            false,
        )
        .unwrap();
        db.conn
            .execute(
                "UPDATE structured_threads SET thread_id = 'thread' || char(10) || 'control'",
                [],
            )
            .unwrap();

        assert_eq!(
            db.list_structured_threads_bounded(&ws, true, 1)
                .unwrap_err()
                .to_string(),
            STRUCTURED_THREAD_ROW_INVALID
        );
        db.conn
            .execute(
                "UPDATE structured_threads
                    SET thread_id = 'thread-control', cwd = '/repo' || char(0) || 'hidden'",
                [],
            )
            .unwrap();
        assert_eq!(
            db.list_structured_threads_bounded(&ws, true, 1)
                .unwrap_err()
                .to_string(),
            STRUCTURED_THREAD_ROW_INVALID
        );
        assert_eq!(
            db.list_structured_threads_bounded("workspace\u{7f}control", true, 0)
                .unwrap_err()
                .to_string(),
            STRUCTURED_THREAD_INPUT_INVALID
        );
    }

    #[test]
    fn structured_thread_bounded_list는_zero_small_limit과_corrupt_sqlite_type을_처리한다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("structured-limit").unwrap();
        for index in 0..3 {
            db.upsert_structured_thread(
                &format!("local-{index}"),
                &ws,
                &format!("thread-{index}"),
                "title",
                "/repo",
                None,
                index == 0,
                false,
            )
            .unwrap();
        }

        assert!(
            db.list_structured_threads_bounded(&ws, true, 0)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.list_structured_threads_bounded(&ws, true, 1)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.list_structured_threads_bounded(&ws, true, 2)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            db.list_structured_threads_bounded(&ws, true, Db::STRUCTURED_THREADS_LIST_CAP + 1,)
                .unwrap_err()
                .to_string(),
            STRUCTURED_THREAD_INPUT_INVALID
        );

        db.conn
            .execute(
                "UPDATE structured_threads SET title = x'ff' WHERE local_session_id = 'local-0'",
                [],
            )
            .unwrap();
        assert!(
            db.list_structured_threads_bounded(&ws, true, 0)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.list_structured_threads_bounded(&ws, true, 1)
                .unwrap_err()
                .to_string(),
            STRUCTURED_THREAD_ROW_INVALID
        );
    }

    #[test]
    fn structured_thread_bounded_list는_aggregate_retained_budget을_넘지_않는다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("structured-aggregate").unwrap();
        let exact_row_count =
            STRUCTURED_THREADS_RETAINED_BYTES_MAX / STRUCTURED_THREAD_ROW_BYTES_MAX;
        assert_eq!(exact_row_count, 128);
        for index in 0..=exact_row_count {
            let local_session_id = format!("local-{index:03}");
            let thread_id = format!("thread-{index:03}");
            let title_bytes = STRUCTURED_THREAD_ROW_BYTES_MAX
                - local_session_id.len()
                - ws.len()
                - thread_id.len();
            db.upsert_structured_thread(
                &local_session_id,
                &ws,
                &thread_id,
                &"x".repeat(title_bytes),
                "",
                None,
                false,
                false,
            )
            .unwrap();
        }

        assert_eq!(
            db.list_structured_threads_bounded(&ws, true, exact_row_count)
                .unwrap()
                .len(),
            exact_row_count
        );
        assert_eq!(
            db.list_structured_threads_bounded(&ws, true, exact_row_count + 1)
                .unwrap_err()
                .to_string(),
            STRUCTURED_THREAD_ROW_INVALID
        );
    }

    fn agent_state_structured_row(
        workspace_id: &str,
        index: usize,
        title: String,
    ) -> StructuredThreadRow {
        StructuredThreadRow {
            local_session_id: format!("local-{index}"),
            workspace_id: workspace_id.to_owned(),
            thread_id: format!("thread-{index}"),
            title,
            cwd: String::new(),
            model: None,
            favorite: false,
            archived: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn agent_state_mutation_only_job(workspace_id: &str) -> AgentStateJob {
        let mut job = AgentStateJob::projection(workspace_id);
        job.include_hook_status = false;
        job.include_attention = false;
        job.include_agent_sessions = false;
        job.include_structured_threads = false;
        job
    }

    fn agent_work_turn(workspace_id: &str, index: usize, updated_at: i64) -> AgentWorkTurnUpsert {
        AgentWorkTurnUpsert {
            workspace_id: workspace_id.to_owned(),
            pane_id: format!("pane-{index}"),
            kind: "codex".to_owned(),
            agent_session_id: "agent-session".to_owned(),
            turn_key: format!("codex:{index:x}"),
            source_offset: index as u64,
            instruction: format!("instruction-{index}"),
            agent_summary: Some(format!("summary-{index}")),
            messages_json: None,
            model: Some("gpt-5.6".to_owned()),
            effort: Some("high".to_owned()),
            cwd: Some("/repo".to_owned()),
            branch: Some("main".to_owned()),
            git_change_count: Some(index as u32),
            state: AgentWorkTurnState::Completed,
            occurred_at: Some(updated_at),
            updated_at,
        }
    }

    fn apply_agent_work_turns(
        db: &Db,
        workspace_id: &str,
        rows: Vec<AgentWorkTurnUpsert>,
    ) -> anyhow::Result<AgentStateSnapshot> {
        let mut job = agent_state_mutation_only_job(workspace_id);
        job.work_turn_mutations = rows
            .into_iter()
            .map(AgentWorkHistoryMutation::Upsert)
            .collect();
        job.include_work_history = true;
        db.apply_agent_state_job(&job)
    }

    #[test]
    fn agent_work_history_v35_migrates_and_reopens() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-work-history-v35-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..34] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 34).unwrap();
        }

        {
            let db = Db::open(&path).unwrap();
            assert_eq!(Db::read_user_version(&db.conn).unwrap(), MIGRATIONS.len());
            let primary_key = db
                .conn
                .prepare("PRAGMA table_info(agent_work_turns)")
                .unwrap()
                .query_map([], |row| {
                    Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
                .into_iter()
                .filter(|(_, position)| *position > 0)
                .collect::<Vec<_>>();
            assert_eq!(
                primary_key,
                [
                    ("workspace_id".to_owned(), 1),
                    ("kind".to_owned(), 2),
                    ("agent_session_id".to_owned(), 3),
                    ("turn_key".to_owned(), 4),
                ]
            );
            let index_count: i64 = db
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                      WHERE type = 'index'
                        AND name = 'idx_agent_work_turns_workspace_recency'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(index_count, 1);
            let workspace_id = db.create_workspace("history-reopen").unwrap();
            apply_agent_work_turns(
                &db,
                &workspace_id,
                vec![agent_work_turn(&workspace_id, 0, 1)],
            )
            .unwrap();
        }
        let reopened = Db::open(&path).unwrap();
        assert_eq!(
            Db::read_user_version(&reopened.conn).unwrap(),
            MIGRATIONS.len()
        );
        let workspace_id = reopened
            .list_workspaces()
            .unwrap()
            .into_iter()
            .find(|workspace| workspace.name == "history-reopen")
            .unwrap()
            .id;
        assert_eq!(
            reopened
                .list_agent_work_history(&AgentWorkHistoryQuery::for_workspace(workspace_id))
                .unwrap()
                .len(),
            1
        );
        drop(reopened);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn agent_work_history_upsert_is_idempotent_by_turn_key() {
        let db = Db::open_in_memory().unwrap();
        let workspace_id = db.create_workspace("history-idempotent").unwrap();
        let first = agent_work_turn(&workspace_id, 7, 10);
        apply_agent_work_turns(&db, &workspace_id, vec![first]).unwrap();

        let mut replacement = agent_work_turn(&workspace_id, 7, 20);
        replacement.pane_id = "new-pane".to_owned();
        replacement.agent_summary = Some("updated-summary".to_owned());
        replacement.state = AgentWorkTurnState::Waiting;
        let snapshot = apply_agent_work_turns(&db, &workspace_id, vec![replacement]).unwrap();

        assert_eq!(snapshot.work_turns.len(), 1);
        assert_eq!(snapshot.work_turns[0].pane_id, "new-pane");
        assert_eq!(
            snapshot.work_turns[0].agent_summary.as_deref(),
            Some("updated-summary")
        );
        assert_eq!(snapshot.work_turns[0].state, AgentWorkTurnState::Waiting);
        assert_eq!(snapshot.work_turns[0].updated_at, 20);

        let mut stale = agent_work_turn(&workspace_id, 7, 19);
        stale.agent_summary = Some("stale-summary".to_owned());
        let snapshot = apply_agent_work_turns(&db, &workspace_id, vec![stale]).unwrap();
        assert_eq!(
            snapshot.work_turns[0].agent_summary.as_deref(),
            Some("updated-summary")
        );
        assert_eq!(snapshot.work_turns[0].updated_at, 20);
    }

    #[test]
    fn agent_work_turn_messages_json은_왕복한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace_id = db.create_workspace("history-messages").unwrap();
        let mut row = agent_work_turn(&workspace_id, 0, 1);
        row.messages_json = Some(r#"[{"r":"u","t":"물어봤다","at":1}]"#.to_owned());
        let snapshot = apply_agent_work_turns(&db, &workspace_id, vec![row]).unwrap();
        assert_eq!(
            snapshot.work_turns[0].messages_json.as_deref(),
            Some(r#"[{"r":"u","t":"물어봤다","at":1}]"#)
        );
        assert_eq!(
            db.list_agent_work_history(&AgentWorkHistoryQuery::for_workspace(
                workspace_id.as_str()
            ))
            .unwrap()[0]
                .messages_json
                .as_deref(),
            Some(r#"[{"r":"u","t":"물어봤다","at":1}]"#)
        );
    }

    #[test]
    fn agent_work_turn_messages_json은_기존_행에서_null이다() {
        // additive 마이그레이션 — 컬럼이 없던 시절 행은 NULL로 읽히고 카드는 기존 두
        // 필드(instruction+agent_summary)만 쓰는 경로로 떨어진다.
        let db = Db::open_in_memory().unwrap();
        let workspace_id = db.create_workspace("history-messages-null").unwrap();
        let row = agent_work_turn(&workspace_id, 0, 1);
        let snapshot = apply_agent_work_turns(&db, &workspace_id, vec![row]).unwrap();
        assert_eq!(snapshot.work_turns[0].messages_json, None);
    }

    #[test]
    fn agent_work_turn_messages_json은_상한_초과와_nul이면_컬럼만_none으로_떨어진다() {
        // 스펙 §3-2: 8KB 초과·NUL 포함은 행 전체를 거부하지 않고 messages_json 컬럼만
        // fail-soft로 NULL로 낮춘다 — 이력 하나 때문에 패널이 죽지 않는다. instruction 등
        // 나머지 필드는 온전히 저장·복원된다.
        let db = Db::open_in_memory().unwrap();
        let workspace_id = db.create_workspace("history-messages-invalid").unwrap();

        let mut oversized = agent_work_turn(&workspace_id, 0, 0);
        oversized.instruction = "instruction-oversized".to_owned();
        oversized.messages_json = Some("x".repeat(AGENT_WORK_TURN_MESSAGES_BYTES_MAX + 1));
        let snapshot = apply_agent_work_turns(&db, &workspace_id, vec![oversized]).unwrap();
        assert_eq!(snapshot.work_turns.len(), 1);
        assert_eq!(snapshot.work_turns[0].messages_json, None);
        assert_eq!(snapshot.work_turns[0].instruction, "instruction-oversized");

        let mut has_nul = agent_work_turn(&workspace_id, 1, 1);
        has_nul.instruction = "instruction-has-nul".to_owned();
        has_nul.messages_json = Some("no-nul\0".to_owned());
        let snapshot = apply_agent_work_turns(&db, &workspace_id, vec![has_nul]).unwrap();
        let with_nul_row = snapshot
            .work_turns
            .iter()
            .find(|row| row.instruction == "instruction-has-nul")
            .unwrap();
        assert_eq!(with_nul_row.messages_json, None);

        let read_back = db
            .list_agent_work_history(&AgentWorkHistoryQuery::for_workspace(workspace_id.as_str()))
            .unwrap();
        assert_eq!(read_back.len(), 2);
        assert!(read_back.iter().all(|row| row.messages_json.is_none()));
        assert!(
            read_back
                .iter()
                .any(|row| row.instruction == "instruction-oversized")
        );
        assert!(
            read_back
                .iter()
                .any(|row| row.instruction == "instruction-has-nul")
        );
    }

    #[test]
    fn agent_work_turn_messages_json은_같은_초의_빈_값에_덮이지_않는다() {
        // stage_detected_work_history가 방금 쓴 새 messages_json을, updated_at이 바뀌지 않은
        // git-facts 백필(poll_work_history_git)이 같은 초에 뒤따라 덮지 못해야 한다.
        let db = Db::open_in_memory().unwrap();
        let workspace_id = db.create_workspace("history-messages-same-second").unwrap();

        let mut fresh = agent_work_turn(&workspace_id, 0, 5);
        fresh.messages_json = Some(r#"[{"r":"a","t":"방금 답했다","at":5}]"#.to_owned());
        apply_agent_work_turns(&db, &workspace_id, vec![fresh]).unwrap();

        // git-facts 백필: 같은 turn_key, 같은 updated_at(=5)로 캐시 스냅샷을 그대로
        // 재-upsert한다 — 캐시가 메시지 갱신 전에 떴다면 messages_json은 None이다.
        let mut backfill = agent_work_turn(&workspace_id, 0, 5);
        backfill.messages_json = None;
        backfill.branch = Some("feature/x".to_owned());
        let snapshot = apply_agent_work_turns(&db, &workspace_id, vec![backfill]).unwrap();

        assert_eq!(snapshot.work_turns.len(), 1);
        assert_eq!(
            snapshot.work_turns[0].messages_json.as_deref(),
            Some(r#"[{"r":"a","t":"방금 답했다","at":5}]"#)
        );
        // 다른 컬럼은 기존 동작대로 여전히 excluded 값으로 갱신된다.
        assert_eq!(snapshot.work_turns[0].branch.as_deref(), Some("feature/x"));

        // stage_detected_work_history의 정상 경로: 같은 초라도 새 Some(...) 값은 여전히
        // 갱신되어야 한다.
        let mut updated = agent_work_turn(&workspace_id, 0, 5);
        updated.messages_json = Some(r#"[{"r":"a","t":"이어서 답했다","at":6}]"#.to_owned());
        let snapshot = apply_agent_work_turns(&db, &workspace_id, vec![updated]).unwrap();
        assert_eq!(
            snapshot.work_turns[0].messages_json.as_deref(),
            Some(r#"[{"r":"a","t":"이어서 답했다","at":6}]"#)
        );
    }

    #[test]
    fn agent_work_history_accepts_bounded_future_provider_id() {
        let db = Db::open_in_memory().unwrap();
        let workspace_id = db.create_workspace("history-provider").unwrap();
        let mut row = agent_work_turn(&workspace_id, 0, 1);
        row.kind = "future_agent-2".to_owned();
        let snapshot = apply_agent_work_turns(&db, &workspace_id, vec![row]).unwrap();
        assert_eq!(snapshot.work_turns[0].kind, "future_agent-2");

        let mut exact = agent_work_turn(&workspace_id, 1, 2);
        exact.kind = "a".repeat(AGENT_WORK_TURN_PROVIDER_BYTES_MAX);
        exact.source_offset = i64::MAX as u64;
        assert!(apply_agent_work_turns(&db, &workspace_id, vec![exact]).is_ok());
        for invalid in ["Future", "future.agent", "future agent"] {
            let mut row = agent_work_turn(&workspace_id, 2, 3);
            row.kind = invalid.to_owned();
            assert_eq!(
                apply_agent_work_turns(&db, &workspace_id, vec![row])
                    .unwrap_err()
                    .to_string(),
                AGENT_STATE_INPUT_INVALID
            );
        }
    }

    #[test]
    fn agent_work_history_debug_redacts_content() {
        let marker = "work-history-secret-marker";
        let mut row = agent_work_turn(marker, 0, 1);
        row.workspace_id = marker.to_owned();
        row.pane_id = marker.to_owned();
        row.kind = "provider".to_owned();
        row.agent_session_id = marker.to_owned();
        row.turn_key = marker.to_owned();
        row.instruction = marker.to_owned();
        row.agent_summary = Some(marker.to_owned());
        row.messages_json = Some(marker.to_owned());
        row.cwd = Some(marker.to_owned());
        let durable = AgentWorkTurnRow {
            workspace_id: row.workspace_id.clone(),
            pane_id: row.pane_id.clone(),
            kind: row.kind.clone(),
            agent_session_id: row.agent_session_id.clone(),
            turn_key: row.turn_key.clone(),
            source_offset: row.source_offset,
            instruction: row.instruction.clone(),
            agent_summary: row.agent_summary.clone(),
            messages_json: row.messages_json.clone(),
            model: row.model.clone(),
            effort: row.effort.clone(),
            cwd: row.cwd.clone(),
            branch: row.branch.clone(),
            git_change_count: row.git_change_count,
            state: row.state,
            occurred_at: row.occurred_at,
            updated_at: row.updated_at,
        };
        let mutation = AgentWorkHistoryMutation::Upsert(row.clone());
        let query = AgentWorkHistoryQuery::for_workspace(marker);
        for debug in [
            format!("{row:?}"),
            format!("{durable:?}"),
            format!("{mutation:?}"),
            format!("{query:?}"),
        ] {
            assert!(!debug.contains(marker));
        }
    }

    #[test]
    fn agent_work_history_batch_bounds_and_atomic_rejection() {
        let db = Db::open_in_memory().unwrap();
        let workspace_id = db.create_workspace("history-batch").unwrap();
        let exact = (0..AGENT_WORK_TURN_BATCH_MAX)
            .map(|index| agent_work_turn(&workspace_id, index, index as i64))
            .collect();
        assert_eq!(
            apply_agent_work_turns(&db, &workspace_id, exact)
                .unwrap()
                .work_turns
                .len(),
            AGENT_WORK_TURN_BATCH_MAX
        );

        let plus_one = (100..100 + AGENT_WORK_TURN_BATCH_MAX + 1)
            .map(|index| agent_work_turn(&workspace_id, index, index as i64))
            .collect();
        assert_eq!(
            apply_agent_work_turns(&db, &workspace_id, plus_one)
                .unwrap_err()
                .to_string(),
            AGENT_STATE_INPUT_INVALID
        );
        assert_eq!(
            db.list_agent_work_history(&AgentWorkHistoryQuery::for_workspace(
                workspace_id.as_str()
            ))
            .unwrap()
            .len(),
            AGENT_WORK_TURN_BATCH_MAX
        );

        let mut valid = agent_work_turn(&workspace_id, 200, 200);
        valid.instruction = "valid-before-invalid".to_owned();
        let mut invalid = agent_work_turn(&workspace_id, 201, 201);
        invalid.source_offset = i64::MAX as u64 + 1;
        assert_eq!(
            apply_agent_work_turns(&db, &workspace_id, vec![valid, invalid])
                .unwrap_err()
                .to_string(),
            AGENT_STATE_INPUT_INVALID
        );
        assert!(
            db.list_agent_work_history(&AgentWorkHistoryQuery::for_workspace(
                workspace_id.as_str()
            ))
            .unwrap()
            .iter()
            .all(|row| row.instruction != "valid-before-invalid")
        );
    }

    #[test]
    fn agent_work_history_rejects_oversized_row_and_aggregate() {
        let db = Db::open_in_memory().unwrap();
        let workspace_id = db.create_workspace("history-bytes").unwrap();
        let mut oversized = agent_work_turn(&workspace_id, 0, 0);
        oversized.instruction = "x".repeat(AGENT_WORK_TURN_INSTRUCTION_BYTES_MAX + 1);
        assert_eq!(
            apply_agent_work_turns(&db, &workspace_id, vec![oversized])
                .unwrap_err()
                .to_string(),
            AGENT_STATE_INPUT_INVALID
        );

        let aggregate = (0..9)
            .map(|index| {
                let mut row = agent_work_turn(&workspace_id, index, index as i64);
                row.instruction = "x".repeat(30 * 1024);
                row
            })
            .collect();
        assert_eq!(
            apply_agent_work_turns(&db, &workspace_id, aggregate)
                .unwrap_err()
                .to_string(),
            AGENT_STATE_INPUT_INVALID
        );
        assert!(
            db.list_agent_work_history(&AgentWorkHistoryQuery::for_workspace(
                workspace_id.as_str()
            ))
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn agent_work_history_query_enforces_row_and_snapshot_limits() {
        let db = Db::open_in_memory().unwrap();
        let workspace_id = db.create_workspace("history-query-bounds").unwrap();
        apply_agent_work_turns(
            &db,
            &workspace_id,
            vec![agent_work_turn(&workspace_id, 0, 0)],
        )
        .unwrap();

        let mut query = AgentWorkHistoryQuery::for_workspace(workspace_id.as_str());
        query.limit = 0;
        assert!(db.list_agent_work_history(&query).unwrap().is_empty());
        query.limit = AGENT_WORK_TURNS_PER_WORKSPACE_MAX + 1;
        assert_eq!(
            db.list_agent_work_history(&query).unwrap_err().to_string(),
            AGENT_WORK_HISTORY_INPUT_INVALID
        );
        query.limit = AGENT_WORK_TURNS_PER_WORKSPACE_MAX;
        query.snapshot_bytes_max = 0;
        assert_eq!(
            db.list_agent_work_history(&query).unwrap_err().to_string(),
            AGENT_WORK_HISTORY_INPUT_INVALID
        );
        query.snapshot_bytes_max = AGENT_WORK_HISTORY_SNAPSHOT_BYTES_MAX + 1;
        assert_eq!(
            db.list_agent_work_history(&query).unwrap_err().to_string(),
            AGENT_WORK_HISTORY_INPUT_INVALID
        );
        query.snapshot_bytes_max = AGENT_WORK_HISTORY_SNAPSHOT_BYTES_MAX;
        assert_eq!(db.list_agent_work_history(&query).unwrap().len(), 1);
    }

    #[test]
    fn agent_work_history_is_workspace_isolated_stably_ordered_and_pruned() {
        let db = Db::open_in_memory().unwrap();
        let first = db.create_workspace("history-first").unwrap();
        let second = db.create_workspace("history-second").unwrap();
        for batch_start in (0..257).step_by(AGENT_WORK_TURN_BATCH_MAX) {
            let rows = (batch_start..(batch_start + AGENT_WORK_TURN_BATCH_MAX).min(257))
                .map(|index| agent_work_turn(&first, index, index as i64))
                .collect();
            apply_agent_work_turns(&db, &first, rows).unwrap();
        }
        apply_agent_work_turns(&db, &second, vec![agent_work_turn(&second, 999, 999)]).unwrap();

        let first_rows = db
            .list_agent_work_history(&AgentWorkHistoryQuery::for_workspace(first.as_str()))
            .unwrap();
        assert_eq!(first_rows.len(), AGENT_WORK_TURNS_PER_WORKSPACE_MAX);
        assert_eq!(first_rows.first().unwrap().turn_key, "codex:100");
        assert_eq!(first_rows.last().unwrap().turn_key, "codex:1");
        let second_rows = db
            .list_agent_work_history(&AgentWorkHistoryQuery::for_workspace(second.as_str()))
            .unwrap();
        assert_eq!(second_rows.len(), 1);
        assert_eq!(second_rows[0].turn_key, "codex:3e7");

        let mut tied_a = agent_work_turn(&second, 20, 1000);
        tied_a.source_offset = 20;
        let mut tied_b = agent_work_turn(&second, 21, 1000);
        tied_b.source_offset = 21;
        let rows = apply_agent_work_turns(&db, &second, vec![tied_a, tied_b])
            .unwrap()
            .work_turns;
        assert_eq!(rows[0].source_offset, 21);
        assert_eq!(rows[1].source_offset, 20);
    }

    fn seed_archived_agent_resume_row(
        db: &Db,
        workspace_id: &str,
        tab_id: &str,
        pane_id: &str,
        persistent_session_id: &str,
        agent_id: &str,
    ) {
        persist::upsert_session(
            &db.conn,
            &persist::SessionRow {
                id: persistent_session_id.to_owned(),
                workspace_id: workspace_id.to_owned(),
                session_kind: "agent".to_owned(),
                agent_id: Some(agent_id.to_owned()),
                title: "Archived agent".to_owned(),
                command: "/bin/sh".to_owned(),
                args: Vec::new(),
                cwd: "/tmp".to_owned(),
                status: persist::SESSION_STATUS_EXITED.to_owned(),
                last_log_offset: 0,
                waiting_regex: None,
                approval_regex: None,
                error_regex: None,
                done_regex: None,
            },
        )
        .unwrap();
        db.conn
            .execute(
                "INSERT INTO mux_panes
                    (id, workspace_id, tab_id, session_id, title, pane_kind, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 'Archived agent', 'terminal', 't', 't')",
                (pane_id, workspace_id, tab_id, persistent_session_id),
            )
            .unwrap();
    }

    fn seed_archived_agent_resume_scope(db: &Db, workspace_id: &str) -> String {
        let window_id = format!("window-{workspace_id}");
        let tab_id = format!("tab-{workspace_id}");
        db.conn
            .execute(
                "INSERT INTO mux_windows
                    (id, workspace_id, title, active_tab_id, created_at, updated_at)
                 VALUES (?1, ?2, 'window', NULL, 't', 't')",
                (&window_id, workspace_id),
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO mux_tabs
                    (id, window_id, workspace_id, title, tab_index, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'tab', 0, 't', 't')",
                (&tab_id, &window_id, workspace_id),
            )
            .unwrap();
        tab_id
    }

    #[test]
    fn archived_agent_resume_projection은_binding없이_agent_id를_보존하고_exact_token을_조인한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace_id = db.create_workspace("archived-agent-resume").unwrap();
        db.upsert_builtin_agent_config(
            "deppy-builtin-kimi",
            "Kimi CLI",
            "/Users/test/.kimi-code/bin/kimi",
        )
        .unwrap();
        let tab_id = seed_archived_agent_resume_scope(&db, &workspace_id);
        seed_archived_agent_resume_row(
            &db,
            &workspace_id,
            &tab_id,
            "pane-kimi",
            "persistent-kimi",
            "deppy-builtin-kimi",
        );

        let without_binding = db
            .apply_agent_state_job(&AgentStateJob::projection(&workspace_id))
            .unwrap();
        assert_eq!(
            without_binding.archived_agent_resume,
            vec![ArchivedAgentResumeRow {
                persistent_session_id: "persistent-kimi".to_owned(),
                agent_id: "deppy-builtin-kimi".to_owned(),
                kind: None,
                session_id: None,
            }]
        );

        db.upsert_agent_session(&workspace_id, "pane-kimi", "kimi", "kimi-native-session")
            .unwrap();
        let with_binding = db
            .apply_agent_state_job(&AgentStateJob::projection(&workspace_id))
            .unwrap();
        assert_eq!(
            with_binding.archived_agent_resume,
            vec![ArchivedAgentResumeRow {
                persistent_session_id: "persistent-kimi".to_owned(),
                agent_id: "deppy-builtin-kimi".to_owned(),
                kind: Some("kimi".to_owned()),
                session_id: Some("kimi-native-session".to_owned()),
            }]
        );
    }

    #[test]
    fn archived_agent_resume_projection은_256행과_plus_one을_구분한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace_id = db.create_workspace("archived-agent-resume-limit").unwrap();
        db.upsert_builtin_agent_config("deppy-builtin-codex", "Codex", "/opt/codex")
            .unwrap();
        let tab_id = seed_archived_agent_resume_scope(&db, &workspace_id);
        for index in 0..AGENT_SESSION_ROWS_MAX {
            seed_archived_agent_resume_row(
                &db,
                &workspace_id,
                &tab_id,
                &format!("pane-{index}"),
                &format!("persistent-{index}"),
                "deppy-builtin-codex",
            );
        }
        assert_eq!(
            db.apply_agent_state_job(&AgentStateJob::projection(&workspace_id))
                .unwrap()
                .archived_agent_resume
                .len(),
            AGENT_SESSION_ROWS_MAX
        );

        seed_archived_agent_resume_row(
            &db,
            &workspace_id,
            &tab_id,
            "pane-plus-one",
            "persistent-plus-one",
            "deppy-builtin-codex",
        );
        assert_eq!(
            db.apply_agent_state_job(&AgentStateJob::projection(&workspace_id))
                .unwrap_err()
                .to_string(),
            BOUNDED_READ_LIMIT_EXCEEDED
        );
    }

    #[test]
    fn agent_state_job은_mutation뒤_complete_projection을_한_transaction에서_반환한다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("agent-state-aggregate").unwrap();
        db.upsert_agent_session(&ws, "pane-dead", "claude", "dead")
            .unwrap();
        db.upsert_agent_session(&ws, "pane-preserved", "claude", "preserved")
            .unwrap();
        db.upsert_agent_session(&ws, "pane-desired", "codex", "desired")
            .unwrap();
        db.conn
            .execute(
                "UPDATE agent_sessions SET updated_at = 7
                  WHERE workspace_id = ?1 AND pane_id = 'pane-desired'",
                [&ws],
            )
            .unwrap();
        let session_key = format!("{ws}:1");
        db.upsert_hook_session(&session_key, "claude", "hook-id", "/tmp/transcript")
            .unwrap();
        db.upsert_statusline(&session_key, Some("high"), Some("model"), Some(55))
            .unwrap();
        db.set_agent_needs_input(&session_key, true, Some("waiting"))
            .unwrap();
        db.set_agent_turn_done(&session_key).unwrap();
        let seen_at = db.list_turn_done_sessions().unwrap()[0].1;
        db.upsert_structured_thread(
            "local-structured",
            &ws,
            "thread-structured",
            "title",
            "/repo",
            Some("model"),
            false,
            false,
        )
        .unwrap();

        let mut job = AgentStateJob::projection(&ws);
        job.binding_reconcile = Some(AgentSessionBindingReconcile {
            live_pane_ids: vec!["pane-preserved".to_owned(), "pane-desired".to_owned()],
            // Same desired identity must be a true no-op, including updated_at.
            desired_bindings: vec![AgentSessionRow {
                pane_id: "pane-desired".to_owned(),
                kind: "codex".to_owned(),
                session_id: "desired".to_owned(),
                task_prompt: None,
            }],
        });
        job.turn_done_clears.push(AgentTurnDoneClear {
            session_key: session_key.clone(),
            seen_at,
        });
        job.structured_mutations
            .push(StructuredThreadMutation::SetArchived {
                local_session_id: "local-structured".to_owned(),
                archived: true,
            });
        let snapshot = db.apply_agent_state_job(&job).unwrap();

        assert_eq!(snapshot.hook_sessions.len(), 1);
        assert_eq!(snapshot.statuslines.len(), 1);
        assert!(snapshot.waiting_sessions.is_empty());
        assert!(snapshot.turn_done_sessions.is_empty());
        assert_eq!(
            snapshot
                .agent_sessions
                .iter()
                .map(|row| row.pane_id.as_str())
                .collect::<std::collections::HashSet<_>>(),
            std::collections::HashSet::from(["pane-preserved", "pane-desired"])
        );
        assert!(snapshot.structured_threads[0].archived);
        assert!(snapshot.activity_panes.is_empty());
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT updated_at FROM agent_sessions
                      WHERE workspace_id = ?1 AND pane_id = 'pane-desired'",
                    [&ws],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            7
        );
    }

    #[test]
    fn 같은_native_id를_공유해도_hook_작업_입력은_pane별로_분리된다() {
        let db = Db::open_in_memory().unwrap();
        let workspace = db.create_workspace("hook-prompts").unwrap();
        let first = format!("{workspace}:1");
        let second = format!("{workspace}:2");
        for key in [&first, &second] {
            db.upsert_hook_session(key, "claude", "shared", "/tmp/shared.jsonl")
                .unwrap();
        }
        db.record_hook_task_prompt(&first, "shared", "첫 작업")
            .unwrap();
        db.record_hook_task_prompt(&second, "shared", "둘째 작업")
            .unwrap();
        // 일반 hook 갱신은 현재 pane의 미리보기를 지우지 않는다.
        db.upsert_hook_session(&first, "claude", "shared", "/tmp/shared.jsonl")
            .unwrap();
        let rows = db
            .list_hook_sessions_for_prefix_bounded(&format!("{workspace}:"), 2)
            .unwrap();
        let by_key: std::collections::HashMap<_, _> = rows
            .iter()
            .map(|row| (row.session_key.as_str(), row.task_prompt.as_deref()))
            .collect();
        assert_eq!(by_key[first.as_str()], Some("첫 작업"));
        assert_eq!(by_key[second.as_str()], Some("둘째 작업"));
        // 과거 ID가 보낸 지연 hook은 새 바인딩의 작업 제목을 다시 덮을 수 없다.
        db.upsert_hook_session(&first, "claude", "forked", "/tmp/forked.jsonl")
            .unwrap();
        db.record_hook_task_prompt(&first, "shared", "오래된 작업")
            .unwrap();
        let first_row = db
            .list_hook_sessions()
            .unwrap()
            .into_iter()
            .find(|row| row.session_key == first)
            .unwrap();
        assert_eq!(first_row.agent_session_id, "forked");
        assert_eq!(first_row.task_prompt, None);
    }

    #[test]
    fn pasted_transport_hook_stores_human_task_and_rejects_wrapped_internal_events() {
        let db = Db::open_in_memory().unwrap();
        let workspace = db.create_workspace("paste-hook").unwrap();
        let key = format!("{workspace}:1");
        db.upsert_hook_session(&key, "claude", "native", "/tmp/native.jsonl")
            .unwrap();
        db.record_hook_task_prompt(
            &key,
            "native",
            "<pasted_content id=\"e89a\">실제 작업을 고쳐",
        )
        .unwrap();
        assert_eq!(
            db.list_hook_sessions().unwrap()[0].task_prompt.as_deref(),
            Some("실제 작업을 고쳐")
        );
        db.record_hook_task_prompt(
            &key,
            "native",
            "<pasted_content id='x'>첫 지시</pasted_content>\n추가 지시",
        )
        .unwrap();
        assert_eq!(
            db.list_hook_sessions().unwrap()[0].task_prompt.as_deref(),
            Some("첫 지시\n추가 지시")
        );
        // Transport bytes must not consume the human title budget.
        let wrapped = format!("<pasted_content id='{}'>실제 작업을 고쳐", "x".repeat(240));
        assert!(wrapped.len() > 256);
        db.record_hook_task_prompt(&key, "native", &wrapped)
            .unwrap();
        for internal in [
            "<task-notification>internal",
            "<agent-message from='subagent'>internal",
            "<pasted_content id='x'><agent-message from='subagent'>internal</agent-message></pasted_content>",
            "<pasted_content id='x'><task-notification>internal</task-notification></pasted_content>",
        ] {
            assert!(!task_prompt_is_displayable(internal));
            db.record_hook_task_prompt(&key, "native", internal)
                .unwrap();
            assert_eq!(
                db.list_hook_sessions().unwrap()[0].task_prompt.as_deref(),
                Some("실제 작업을 고쳐")
            );
        }
    }

    #[test]
    fn 내부_알림은_현재_pane의_작업_제목을_덮지_않는다() {
        let db = Db::open_in_memory().unwrap();
        let workspace = db.create_workspace("hook-internal-prompt").unwrap();
        let key = format!("{workspace}:4");
        db.upsert_hook_session(&key, "claude", "native", "/tmp/native.jsonl")
            .unwrap();
        db.record_hook_task_prompt(&key, "native", "실제 진행 중인 작업")
            .unwrap();

        for internal in [
            "<task-notification> <task-id>internal</task-id>",
            "<agent-message from=\"subagent\"> [Subagent hand-back] internal",
            "Another Claude session sent a message: <agent-message from=\"subagent\"> internal",
        ] {
            db.record_hook_task_prompt(&key, "native", internal)
                .unwrap();
            assert!(!task_prompt_is_displayable(internal));
        }
        assert!(task_prompt_is_displayable(
            "<agent-message-example> UI 문구를 바꿔"
        ));
        assert!(task_prompt_is_displayable("<div> 태그 렌더링을 고쳐"));

        let row = db
            .list_hook_sessions()
            .unwrap()
            .into_iter()
            .find(|row| row.session_key == key)
            .unwrap();
        assert_eq!(row.task_prompt.as_deref(), Some("실제 진행 중인 작업"));
    }

    #[test]
    fn agent_state_projection_legacy_default는_기존section을모두요청한다() {
        let job = AgentStateJob::projection("legacy-workspace");
        assert!(job.include_hook_status);
        assert!(job.include_attention);
        assert!(job.include_agent_sessions);
        assert!(job.include_structured_threads);
        assert!(job.include_archived_threads);
        assert!(!job.include_activity_panes);
    }

    #[test]
    fn agent_state_hook_status_omission은_corrupt_rows를조회하거나할당하지않는다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("agent-state-omit-hook-status").unwrap();
        let key = format!("{ws}:hook");
        db.conn
            .execute(
                "INSERT INTO agent_hook_sessions
                    (session_key, kind, agent_session_id, transcript_path, updated_at)
                 VALUES (?1, x'ff', 'agent', '', CAST(strftime('%s','now') AS INTEGER))",
                [&key],
            )
            .unwrap();

        let omitted = agent_state_mutation_only_job(&ws);
        let snapshot = db.apply_agent_state_job(&omitted).unwrap();
        assert_eq!(snapshot.hook_sessions.capacity(), 0);
        assert_eq!(snapshot.statuslines.capacity(), 0);
        let mut requested = omitted.clone();
        requested.include_hook_status = true;
        assert_eq!(
            db.apply_agent_state_job(&requested)
                .unwrap_err()
                .to_string(),
            BOUNDED_READ_ROW_INVALID
        );

        db.conn
            .execute("DELETE FROM agent_hook_sessions", [])
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO agent_statusline
                    (session_key, effort, model, context_pct, updated_at)
                 VALUES (?1, NULL, x'ff', NULL, CAST(strftime('%s','now') AS INTEGER))",
                [&key],
            )
            .unwrap();
        assert!(db.apply_agent_state_job(&omitted).is_ok());
        assert_eq!(
            db.apply_agent_state_job(&requested)
                .unwrap_err()
                .to_string(),
            BOUNDED_READ_ROW_INVALID
        );
    }

    #[test]
    fn agent_state_attention_omission은_corrupt_rows를조회하거나할당하지않는다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("agent-state-omit-attention").unwrap();
        let waiting_key = format!("{ws}:waiting");
        db.conn
            .execute(
                "INSERT INTO agent_needs_input
                    (session_key, waiting, updated_at, turn_done, message)
                 VALUES (?1, 1, CAST(strftime('%s','now') AS INTEGER), 0, x'ff')",
                [&waiting_key],
            )
            .unwrap();

        let omitted = agent_state_mutation_only_job(&ws);
        let snapshot = db.apply_agent_state_job(&omitted).unwrap();
        assert_eq!(snapshot.waiting_sessions.capacity(), 0);
        assert_eq!(snapshot.turn_done_sessions.capacity(), 0);
        let mut requested = omitted.clone();
        requested.include_attention = true;
        assert_eq!(
            db.apply_agent_state_job(&requested)
                .unwrap_err()
                .to_string(),
            BOUNDED_READ_ROW_INVALID
        );

        db.conn
            .execute("DELETE FROM agent_needs_input", [])
            .unwrap();
        let turn_key = format!("{ws}:turn");
        db.conn
            .execute(
                "INSERT INTO agent_needs_input
                    (session_key, waiting, updated_at, turn_done, message)
                 VALUES (CAST(?1 AS BLOB), 0, CAST(strftime('%s','now') AS INTEGER), 1, NULL)",
                [&turn_key],
            )
            .unwrap();
        assert!(db.apply_agent_state_job(&omitted).is_ok());
        assert_eq!(
            db.apply_agent_state_job(&requested)
                .unwrap_err()
                .to_string(),
            BOUNDED_READ_ROW_INVALID
        );

        // working=1 코럽트 행(v32)도 waiting/turn과 같은 preflight 검증을 거친다.
        db.conn
            .execute("DELETE FROM agent_needs_input", [])
            .unwrap();
        let working_key = format!("{ws}:working");
        db.conn
            .execute(
                "INSERT INTO agent_needs_input
                    (session_key, waiting, updated_at, turn_done, message, working)
                 VALUES (CAST(?1 AS BLOB), 0, CAST(strftime('%s','now') AS INTEGER), 0, NULL, 1)",
                [&working_key],
            )
            .unwrap();
        assert!(db.apply_agent_state_job(&omitted).is_ok());
        assert_eq!(
            db.apply_agent_state_job(&requested)
                .unwrap_err()
                .to_string(),
            BOUNDED_READ_ROW_INVALID
        );
    }

    #[test]
    fn agent_state_binding_omission은_corrupt_rows를조회하거나할당하지않는다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("agent-state-omit-bindings").unwrap();
        db.conn
            .execute(
                "INSERT INTO agent_sessions
                    (workspace_id, pane_id, kind, session_id, updated_at)
                 VALUES (?1, 'pane', x'ff', 'session',
                         CAST(strftime('%s','now') AS INTEGER))",
                [&ws],
            )
            .unwrap();

        let omitted = agent_state_mutation_only_job(&ws);
        let snapshot = db.apply_agent_state_job(&omitted).unwrap();
        assert_eq!(snapshot.agent_sessions.capacity(), 0);
        let mut requested = omitted;
        requested.include_agent_sessions = true;
        assert_eq!(
            db.apply_agent_state_job(&requested)
                .unwrap_err()
                .to_string(),
            BOUNDED_READ_ROW_INVALID
        );
    }

    #[test]
    fn agent_state_global_binding_omission은_corrupt_rows를조회하거나할당하지않는다() {
        // 워크스페이스별 include_agent_sessions=false여도 전역 include_global_agent_sessions는
        // 별개 플래그 — 둘 다 꺼져 있으면 corrupt pane_id가 있어도 조회조차 하지 않는다.
        let db = Db::open_in_memory().unwrap();
        let ws = db
            .create_workspace("agent-state-omit-global-bindings")
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO agent_sessions
                    (workspace_id, pane_id, kind, session_id, updated_at)
                 VALUES (?1, x'ff', 'claude', 'session',
                         CAST(strftime('%s','now') AS INTEGER))",
                [&ws],
            )
            .unwrap();

        let omitted = agent_state_mutation_only_job(&ws);
        assert!(!omitted.include_global_agent_sessions);
        let snapshot = db.apply_agent_state_job(&omitted).unwrap();
        assert_eq!(snapshot.global_agent_sessions.capacity(), 0);
        let mut requested = omitted;
        requested.include_global_agent_sessions = true;
        assert_eq!(
            db.apply_agent_state_job(&requested)
                .unwrap_err()
                .to_string(),
            BOUNDED_READ_ROW_INVALID
        );
    }

    #[test]
    fn agent_state_global_agent_sessions는_다른_워크스페이스의_pane도_담는다() {
        // 이게 이 기능의 핵심 계약이다: include_agent_sessions(워크스페이스 스코프)는
        // job.workspace_id 하나만 보지만, include_global_agent_sessions는 전 워크스페이스의
        // (workspace_id, pane_id)를 담아 warm(비활성) 워크스페이스 행의 「이어가기」
        // 판정에 쓴다.
        let db = Db::open_in_memory().unwrap();
        let active = db.create_workspace("agent-state-global-active").unwrap();
        let warm = db.create_workspace("agent-state-global-warm").unwrap();
        db.upsert_agent_session(&active, "active-pane", "claude", "active-session")
            .unwrap();
        db.upsert_agent_session(&warm, "warm-pane", "codex", "warm-session")
            .unwrap();

        let mut job = AgentStateJob::projection(&active);
        job.include_global_agent_sessions = true;
        let snapshot = db.apply_agent_state_job(&job).unwrap();

        // 워크스페이스 스코프 agent_sessions는 여전히 active만.
        assert_eq!(snapshot.agent_sessions.len(), 1);
        assert_eq!(snapshot.agent_sessions[0].pane_id, "active-pane");

        // 전역 스코프는 active·warm 둘 다.
        let mut global = snapshot.global_agent_sessions.clone();
        global.sort();
        let mut expected = vec![
            (active.clone(), "active-pane".to_owned()),
            (warm.clone(), "warm-pane".to_owned()),
        ];
        expected.sort();
        assert_eq!(global, expected);
    }

    #[test]
    fn agent_state_global_agent_sessions는_실제_resume명령이_없는_kind를_제외한다() {
        let db = Db::open_in_memory().unwrap();
        let active = db.create_workspace("agent-state-global-resumable").unwrap();
        db.upsert_agent_session(&active, "claude-pane", "claude", "claude-session")
            .unwrap();
        db.upsert_agent_session(&active, "grok-pane", "grok", "grok-session")
            .unwrap();
        db.upsert_agent_session(&active, "kimi-pane", "kimi", "kimi-session")
            .unwrap();

        let mut job = AgentStateJob::projection(&active);
        job.include_global_agent_sessions = true;
        let snapshot = db.apply_agent_state_job(&job).unwrap();

        let mut actual = snapshot.global_agent_sessions;
        actual.sort();
        let mut expected = vec![
            (active.clone(), "claude-pane".to_owned()),
            (active, "grok-pane".to_owned()),
        ];
        expected.sort();
        assert_eq!(actual, expected);
    }

    #[test]
    fn agent_state_global_agent_sessions는_합법적인_쓰기를_거부하지_않는다() {
        // 전역 스코프 읽기의 상한은 "워크스페이스당 상한"이 아니라 "전역 상한"이어야 한다.
        // upsert_agent_session은 워크스페이스당 AGENT_SESSION_ROWS_MAX(256)까지 허용하므로
        // 워크스페이스가 여러 개면 전체 합계는 그보다 훨씬 커질 수 있다 — 전역 읽기가
        // 워크스페이스당 상한을 그대로 쓰면 **전부 합법적으로 쓴 상태**를 읽기에서
        // BOUNDED_READ_LIMIT_EXCEEDED로 거부하게 되고, 그 에러는 스냅샷 함수 전체를
        // 빠져나가 활성 워크스페이스의 restore_agents까지 같이 죽인다(2026-08-20).
        // ACTIVITY_PANE_ROWS_MAX(256 * 256)와 같은 관례로 전역 상한을 스케일한다.
        let db = Db::open_in_memory().unwrap();
        let mut workspaces = Vec::with_capacity(AGENT_SESSION_ROWS_MAX);
        for index in 0..AGENT_SESSION_ROWS_MAX {
            let ws = db
                .create_workspace(&format!("agent-state-global-bound-{index}"))
                .unwrap();
            db.upsert_agent_session(&ws, "pane", "claude", "session")
                .unwrap();
            workspaces.push(ws);
        }
        // 워크스페이스당 상한(256)에는 한참 못 미치는 두 번째 pane — 쓰기는 당연히 성공한다.
        db.upsert_agent_session(&workspaces[0], "pane-2", "claude", "session")
            .unwrap();

        let mut job = AgentStateJob::projection(&workspaces[0]);
        job.include_global_agent_sessions = true;
        let snapshot = db.apply_agent_state_job(&job).unwrap();
        assert_eq!(
            snapshot.global_agent_sessions.len(),
            AGENT_SESSION_ROWS_MAX + 1,
            "합법적으로 쓴 행은 전역 읽기에서도 전부 보여야 한다"
        );
    }

    #[test]
    fn agent_state_structured_omission은_corrupt_rows를조회하거나할당하지않는다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("agent-state-omit-structured").unwrap();
        db.upsert_structured_thread(
            "omit-structured-local",
            &ws,
            "omit-structured-thread",
            "title",
            "",
            None,
            false,
            false,
        )
        .unwrap();
        db.conn
            .execute(
                "UPDATE structured_threads SET title = x'ff'
                  WHERE local_session_id = 'omit-structured-local'",
                [],
            )
            .unwrap();

        let omitted = agent_state_mutation_only_job(&ws);
        let snapshot = db.apply_agent_state_job(&omitted).unwrap();
        assert_eq!(snapshot.structured_threads.capacity(), 0);
        let mut requested = omitted;
        requested.include_structured_threads = true;
        assert_eq!(
            db.apply_agent_state_job(&requested)
                .unwrap_err()
                .to_string(),
            BOUNDED_READ_ROW_INVALID
        );
    }

    #[test]
    fn agent_state_selective_projection은_omitted_section_mutation도원자commit한다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("agent-state-selective-commit").unwrap();
        db.upsert_agent_session(&ws, "old-pane", "codex", "old-session")
            .unwrap();
        db.upsert_structured_thread(
            "selective-local",
            &ws,
            "selective-thread",
            "title",
            "",
            None,
            false,
            false,
        )
        .unwrap();
        let key = format!("{ws}:turn");
        db.set_agent_turn_done(&key).unwrap();
        let seen_at = db.list_turn_done_sessions().unwrap()[0].1;

        let mut job = agent_state_mutation_only_job(&ws);
        job.include_attention = true;
        job.binding_reconcile = Some(AgentSessionBindingReconcile {
            live_pane_ids: vec!["new-pane".to_owned()],
            desired_bindings: vec![AgentSessionRow {
                pane_id: "new-pane".to_owned(),
                kind: "codex".to_owned(),
                session_id: "new-session".to_owned(),
                task_prompt: None,
            }],
        });
        job.turn_done_clears.push(AgentTurnDoneClear {
            session_key: key,
            seen_at,
        });
        job.structured_mutations
            .push(StructuredThreadMutation::SetArchived {
                local_session_id: "selective-local".to_owned(),
                archived: true,
            });

        let snapshot = db.apply_agent_state_job(&job).unwrap();
        assert!(snapshot.turn_done_sessions.is_empty());
        assert_eq!(snapshot.hook_sessions.capacity(), 0);
        assert_eq!(snapshot.agent_sessions.capacity(), 0);
        assert_eq!(snapshot.structured_threads.capacity(), 0);
        assert_eq!(
            db.list_agent_sessions(&ws).unwrap(),
            vec![AgentSessionRow {
                pane_id: "new-pane".to_owned(),
                kind: "codex".to_owned(),
                session_id: "new-session".to_owned(),
                task_prompt: None,
            }]
        );
        assert!(db.list_structured_threads(&ws, true).unwrap()[0].archived);
    }

    #[test]
    fn agent_state_selective_projection실패는_omitted_section_mutation도rollback한다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db
            .create_workspace("agent-state-selective-rollback")
            .unwrap();
        db.upsert_agent_session(&ws, "preserved-pane", "codex", "preserved-session")
            .unwrap();
        db.upsert_structured_thread(
            "selective-rollback-local",
            &ws,
            "selective-rollback-thread",
            "title",
            "",
            None,
            false,
            false,
        )
        .unwrap();
        let corrupt_key = format!("{ws}:corrupt");
        db.conn
            .execute(
                "INSERT INTO agent_needs_input
                    (session_key, waiting, updated_at, turn_done, message)
                 VALUES (?1, 1, CAST(strftime('%s','now') AS INTEGER), 0, x'ff')",
                [&corrupt_key],
            )
            .unwrap();

        let mut job = agent_state_mutation_only_job(&ws);
        job.include_attention = true;
        job.stale_binding_deletes.push(AgentSessionIdentity {
            pane_id: "preserved-pane".to_owned(),
            kind: "codex".to_owned(),
            session_id: "preserved-session".to_owned(),
        });
        job.structured_mutations
            .push(StructuredThreadMutation::SetArchived {
                local_session_id: "selective-rollback-local".to_owned(),
                archived: true,
            });
        assert_eq!(
            db.apply_agent_state_job(&job).unwrap_err().to_string(),
            BOUNDED_READ_ROW_INVALID
        );
        assert_eq!(db.list_agent_sessions(&ws).unwrap().len(), 1);
        assert!(
            !db.list_structured_threads(&ws, true).unwrap()[0].archived,
            "requested-section preflight failure must roll back omitted-section mutations"
        );
    }

    #[test]
    fn agent_state_activity_catalog는_opt_in으로같은_snapshot에포함된다() {
        let db = Db::open_in_memory().unwrap();
        let workspace_id = seed_activity_panes(&db, 2, "saved shell");

        let without_activity = db
            .apply_agent_state_job(&AgentStateJob::projection(&workspace_id))
            .unwrap();
        assert!(without_activity.activity_panes.is_empty());

        let mut job = AgentStateJob::projection(&workspace_id);
        job.include_activity_panes = true;
        let snapshot = db.apply_agent_state_job(&job).unwrap();
        assert_eq!(snapshot.activity_panes.len(), 2);
        assert!(
            snapshot
                .activity_panes
                .iter()
                .all(|row| row.workspace_id == workspace_id
                    && row.pane_id.starts_with("activity-pane-")
                    && row.title == "saved shell"
                    && row.cwd.is_empty())
        );
        assert!(snapshot.retained_bytes() <= AGENT_STATE_SNAPSHOT_BYTES_MAX);
    }

    #[test]
    fn agent_state_activity_actual_byte_failure는_exact_mutation을_rollback한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace_id = seed_activity_panes(&db, 2, "saved shell");
        db.upsert_structured_thread(
            "activity-rollback-local",
            &workspace_id,
            "activity-rollback-thread",
            "title",
            "",
            None,
            false,
            false,
        )
        .unwrap();

        let mut projection = AgentStateJob::projection(&workspace_id);
        projection.include_activity_panes = true;
        let exact_bytes = db
            .apply_agent_state_job(&projection)
            .unwrap()
            .retained_bytes();

        let mut exact = projection.clone();
        exact.snapshot_bytes_max = exact_bytes;
        exact
            .structured_mutations
            .push(StructuredThreadMutation::SetArchived {
                local_session_id: "activity-rollback-local".to_owned(),
                archived: true,
            });
        assert!(db.apply_agent_state_job(&exact).is_ok());
        assert!(db.list_structured_threads(&workspace_id, true).unwrap()[0].archived);
        assert!(
            db.set_structured_thread_archived("activity-rollback-local", false)
                .unwrap()
        );

        exact.snapshot_bytes_max = exact_bytes - 1;
        assert_eq!(
            db.apply_agent_state_job(&exact).unwrap_err().to_string(),
            AGENT_STATE_SNAPSHOT_INVALID
        );
        assert!(
            !db.list_structured_threads(&workspace_id, true).unwrap()[0].archived,
            "actual activity allocation failure must roll back the co-staged mutation"
        );
    }

    #[test]
    fn agent_state_activity_preflight_failure는_exact_mutation을_rollback한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace_id = seed_activity_panes(&db, 1, "saved shell");
        let session_key = format!("{workspace_id}:session");
        db.set_agent_turn_done(&session_key).unwrap();
        let seen_at = db.list_turn_done_sessions().unwrap()[0].1;
        db.conn
            .execute(
                "UPDATE mux_panes SET title = CAST(x'7879' AS BLOB)
                  WHERE id = 'activity-pane-00000'",
                [],
            )
            .unwrap();
        assert!(
            db.apply_agent_state_job(&AgentStateJob::projection(&workspace_id))
                .is_ok(),
            "activity storage must not be queried unless the projection is requested"
        );

        let mut job = AgentStateJob::projection(&workspace_id);
        job.include_activity_panes = true;
        job.turn_done_clears.push(AgentTurnDoneClear {
            session_key: session_key.clone(),
            seen_at,
        });
        assert_eq!(
            db.apply_agent_state_job(&job).unwrap_err().to_string(),
            BOUNDED_READ_ROW_INVALID
        );
        assert_eq!(
            db.list_turn_done_sessions().unwrap(),
            vec![(session_key, seen_at)]
        );
    }

    #[test]
    fn agent_state_catalog는_여러_workspace를_한_snapshot으로_투영한다() {
        let db = Db::open_in_memory().unwrap();
        let first = db.create_workspace("agent-state-catalog-first").unwrap();
        let second = db.create_workspace("agent-state-catalog-second").unwrap();
        let omitted = db.create_workspace("agent-state-catalog-omitted").unwrap();
        db.upsert_structured_thread(
            "catalog-first",
            &first,
            "thread-first",
            "first",
            "",
            None,
            false,
            false,
        )
        .unwrap();
        db.upsert_structured_thread(
            "catalog-second",
            &second,
            "thread-second",
            "second",
            "",
            None,
            true,
            false,
        )
        .unwrap();
        db.upsert_structured_thread(
            "catalog-archived",
            &second,
            "thread-archived",
            "archived",
            "",
            None,
            false,
            true,
        )
        .unwrap();
        db.upsert_structured_thread(
            "catalog-omitted",
            &omitted,
            "thread-omitted",
            "omitted",
            "",
            None,
            true,
            false,
        )
        .unwrap();

        let mut job = AgentStateJob::projection(&first);
        job.structured_workspace_ids = vec![second.clone(), first.clone()];
        job.include_archived_threads = false;
        let snapshot = db.apply_agent_state_job(&job).unwrap();
        assert_eq!(
            snapshot
                .structured_threads
                .iter()
                .map(|row| (row.local_session_id.as_str(), row.workspace_id.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("catalog-second", second.as_str()),
                ("catalog-first", first.as_str()),
            ]
        );

        job.include_archived_threads = true;
        let snapshot = db.apply_agent_state_job(&job).unwrap();
        assert_eq!(snapshot.structured_threads.len(), 3);
        assert!(
            snapshot
                .structured_threads
                .iter()
                .any(|row| row.local_session_id == "catalog-archived")
        );
        assert!(
            snapshot
                .structured_threads
                .iter()
                .all(|row| row.workspace_id != omitted)
        );
    }

    #[test]
    fn agent_state_catalog_workspace_scope는_1에서_256까지로_제한된다() {
        let db = Db::open_in_memory().unwrap();
        let active = db.create_workspace("agent-state-catalog-scope").unwrap();
        let exact = (0..AGENT_STATE_STRUCTURED_WORKSPACE_MAX)
            .map(|index| format!("workspace-{index}"))
            .collect::<Vec<_>>();
        let mut job = AgentStateJob::projection(&active);
        job.structured_workspace_ids = exact.clone();
        assert!(db.apply_agent_state_job(&job).is_ok());

        job.structured_workspace_ids.clear();
        assert_eq!(
            db.apply_agent_state_job(&job).unwrap_err().to_string(),
            AGENT_STATE_INPUT_INVALID
        );

        job.structured_workspace_ids = exact.clone();
        job.structured_workspace_ids
            .push("workspace-plus-one".to_owned());
        assert_eq!(
            db.apply_agent_state_job(&job).unwrap_err().to_string(),
            AGENT_STATE_INPUT_INVALID
        );

        job.structured_workspace_ids = vec![exact[0].clone(), exact[0].clone()];
        assert_eq!(
            db.apply_agent_state_job(&job).unwrap_err().to_string(),
            AGENT_STATE_INPUT_INVALID
        );

        job.structured_workspace_ids = vec!["workspace\ncontrol".to_owned()];
        assert_eq!(
            db.apply_agent_state_job(&job).unwrap_err().to_string(),
            AGENT_STATE_INPUT_INVALID
        );

        job.structured_workspace_ids = vec!["w".repeat(BOUNDED_ID_BYTES_MAX)];
        assert!(db.apply_agent_state_job(&job).is_ok());
        job.structured_workspace_ids = vec!["w".repeat(BOUNDED_ID_BYTES_MAX + 1)];
        assert_eq!(
            db.apply_agent_state_job(&job).unwrap_err().to_string(),
            AGENT_STATE_INPUT_INVALID
        );

        job.structured_workspace_ids = vec!["') UNION SELECT 'hostile' --".to_owned()];
        assert!(
            db.apply_agent_state_job(&job)
                .unwrap()
                .structured_threads
                .is_empty(),
            "workspace IDs must remain bound values rather than SQL fragments"
        );
    }

    #[test]
    fn agent_state_structured_mutation은_requested_catalog_scope안의_multi_workspace를_허용한다() {
        let db = Db::open_in_memory().unwrap();
        let active = db
            .create_workspace("agent-state-structured-scope-active")
            .unwrap();
        let first = db
            .create_workspace("agent-state-structured-scope-first")
            .unwrap();
        let second = db
            .create_workspace("agent-state-structured-scope-second")
            .unwrap();
        db.upsert_structured_thread(
            "scope-first-existing",
            &first,
            "scope-first-thread",
            "first",
            "",
            None,
            false,
            false,
        )
        .unwrap();
        let session_key = format!("{active}:1");
        db.set_agent_turn_done(&session_key).unwrap();
        let seen_at = db.list_turn_done_sessions().unwrap()[0].1;

        let mut job = AgentStateJob::projection(&active);
        job.structured_workspace_ids = vec![first.clone(), second.clone()];
        job.turn_done_clears.push(AgentTurnDoneClear {
            session_key,
            seen_at,
        });
        job.structured_mutations
            .push(StructuredThreadMutation::SetArchived {
                local_session_id: "scope-first-existing".to_owned(),
                archived: true,
            });
        let mut second_row = agent_state_structured_row(&second, 0, "second".to_owned());
        second_row.local_session_id = "scope-second-new".to_owned();
        second_row.thread_id = "scope-second-thread".to_owned();
        job.structured_mutations
            .push(StructuredThreadMutation::Upsert(second_row));

        let snapshot = db.apply_agent_state_job(&job).unwrap();
        assert!(snapshot.turn_done_sessions.is_empty());
        assert_eq!(snapshot.structured_threads.len(), 2);
        assert!(
            snapshot
                .structured_threads
                .iter()
                .any(|row| row.local_session_id == "scope-first-existing" && row.archived)
        );
        assert!(
            snapshot
                .structured_threads
                .iter()
                .any(|row| row.local_session_id == "scope-second-new")
        );
    }

    #[test]
    fn agent_state_structured_mutation의_out_of_scope는_첫_write전에_fail_closed된다() {
        let db = Db::open_in_memory().unwrap();
        let active = db
            .create_workspace("agent-state-out-of-scope-active")
            .unwrap();
        let requested = db
            .create_workspace("agent-state-out-of-scope-requested")
            .unwrap();
        let outside = db
            .create_workspace("agent-state-out-of-scope-outside")
            .unwrap();
        db.upsert_structured_thread(
            "outside-existing",
            &outside,
            "outside-thread",
            "outside",
            "",
            None,
            false,
            false,
        )
        .unwrap();
        let session_key = format!("{active}:1");
        db.set_agent_turn_done(&session_key).unwrap();
        let seen_at = db.list_turn_done_sessions().unwrap()[0].1;

        let mut new_outside = AgentStateJob::projection(&active);
        new_outside.structured_workspace_ids = vec![requested.clone()];
        new_outside.turn_done_clears.push(AgentTurnDoneClear {
            session_key: session_key.clone(),
            seen_at,
        });
        let mut outside_row = agent_state_structured_row(&outside, 0, "outside-new".to_owned());
        outside_row.local_session_id = "outside-new".to_owned();
        new_outside
            .structured_mutations
            .push(StructuredThreadMutation::Upsert(outside_row));
        assert_eq!(
            db.apply_agent_state_job(&new_outside)
                .unwrap_err()
                .to_string(),
            AGENT_STATE_INPUT_INVALID
        );
        assert_eq!(
            db.list_turn_done_sessions().unwrap(),
            [(session_key.clone(), seen_at)]
        );
        assert!(
            db.list_structured_threads(&outside, true)
                .unwrap()
                .iter()
                .all(|row| row.local_session_id != "outside-new")
        );

        let mut existing_outside = AgentStateJob::projection(&active);
        existing_outside.structured_workspace_ids = vec![requested.clone()];
        existing_outside.turn_done_clears.push(AgentTurnDoneClear {
            session_key: session_key.clone(),
            seen_at,
        });
        existing_outside
            .structured_mutations
            .push(StructuredThreadMutation::SetArchived {
                local_session_id: "outside-existing".to_owned(),
                archived: true,
            });
        assert_eq!(
            db.apply_agent_state_job(&existing_outside)
                .unwrap_err()
                .to_string(),
            AGENT_STATE_INPUT_INVALID
        );
        assert_eq!(
            db.list_turn_done_sessions().unwrap(),
            [(session_key.clone(), seen_at)]
        );
        assert!(!db.list_structured_threads(&outside, true).unwrap()[0].archived);

        let mut move_outside = AgentStateJob::projection(&active);
        move_outside.structured_workspace_ids = vec![requested.clone()];
        let mut moved_row = agent_state_structured_row(&requested, 1, "moved".to_owned());
        moved_row.local_session_id = "outside-existing".to_owned();
        move_outside
            .structured_mutations
            .push(StructuredThreadMutation::Upsert(moved_row));
        assert_eq!(
            db.apply_agent_state_job(&move_outside)
                .unwrap_err()
                .to_string(),
            AGENT_STATE_INPUT_INVALID
        );
        let outside_rows = db.list_structured_threads(&outside, true).unwrap();
        assert_eq!(outside_rows.len(), 1);
        assert_eq!(outside_rows[0].local_session_id, "outside-existing");
        assert!(
            db.list_structured_threads(&requested, true)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn agent_state_catalog의_500_row상한은_workspace합계에_적용되고_rollback한다() {
        let db = Db::open_in_memory().unwrap();
        let first = db
            .create_workspace("agent-state-catalog-limit-first")
            .unwrap();
        let second = db
            .create_workspace("agent-state-catalog-limit-second")
            .unwrap();
        for index in 0..251 {
            db.upsert_structured_thread(
                &format!("first-{index}"),
                &first,
                &format!("thread-first-{index}"),
                "title",
                "",
                None,
                false,
                false,
            )
            .unwrap();
        }
        for index in 0..250 {
            db.upsert_structured_thread(
                &format!("second-{index}"),
                &second,
                &format!("thread-second-{index}"),
                "title",
                "",
                None,
                false,
                false,
            )
            .unwrap();
        }

        let mut job = AgentStateJob::projection(&first);
        job.structured_workspace_ids = vec![first.clone(), second.clone()];
        job.include_archived_threads = true;
        job.structured_mutations
            .push(StructuredThreadMutation::SetArchived {
                local_session_id: "first-0".to_owned(),
                archived: true,
            });
        assert_eq!(
            db.apply_agent_state_job(&job).unwrap_err().to_string(),
            BOUNDED_READ_LIMIT_EXCEEDED
        );
        assert!(
            !db.list_structured_threads(&first, true)
                .unwrap()
                .into_iter()
                .find(|row| row.local_session_id == "first-0")
                .unwrap()
                .archived,
            "projection admission failure must roll back earlier exact mutations"
        );

        assert!(db.delete_structured_thread("second-249").unwrap());
        job.structured_mutations.clear();
        assert_eq!(
            db.apply_agent_state_job(&job)
                .unwrap()
                .structured_threads
                .len(),
            AGENT_STATE_STRUCTURED_PROJECTION_MAX
        );
    }

    #[test]
    fn agent_state_catalog의_4mib상한은_workspace합계에_적용되고_rollback한다() {
        let db = Db::open_in_memory().unwrap();
        let first = db
            .create_workspace("agent-state-catalog-bytes-first")
            .unwrap();
        let second = db
            .create_workspace("agent-state-catalog-bytes-second")
            .unwrap();
        let row_retained_bytes =
            (AGENT_STATE_SNAPSHOT_BYTES_MAX - std::mem::size_of::<AgentStateSnapshot>()) / 128
                - std::mem::size_of::<StructuredThreadRow>();
        for index in 0..129 {
            let workspace_id = if index < 65 { &first } else { &second };
            let local_session_id = format!("catalog-byte-local-{index}");
            let thread_id = format!("catalog-byte-thread-{index}");
            let title_bytes =
                row_retained_bytes - local_session_id.len() - workspace_id.len() - thread_id.len();
            db.upsert_structured_thread(
                &local_session_id,
                workspace_id,
                &thread_id,
                &"x".repeat(title_bytes),
                "",
                None,
                false,
                false,
            )
            .unwrap();
        }

        let mut job = AgentStateJob::projection(&first);
        job.structured_workspace_ids = vec![first.clone(), second.clone()];
        job.structured_mutations
            .push(StructuredThreadMutation::SetArchived {
                local_session_id: "catalog-byte-local-0".to_owned(),
                archived: true,
            });
        assert_eq!(
            db.apply_agent_state_job(&job).unwrap_err().to_string(),
            BOUNDED_READ_ROW_INVALID
        );
        assert!(
            !db.list_structured_threads(&first, true)
                .unwrap()
                .into_iter()
                .find(|row| row.local_session_id == "catalog-byte-local-0")
                .unwrap()
                .archived,
            "aggregate byte admission failure must roll back earlier exact mutations"
        );

        assert!(
            db.delete_structured_thread("catalog-byte-local-128")
                .unwrap()
        );
        job.structured_mutations.clear();
        assert_eq!(
            db.apply_agent_state_job(&job)
                .unwrap()
                .structured_threads
                .len(),
            128
        );
        assert_eq!(job.snapshot_bytes_max, AGENT_STATE_SNAPSHOT_BYTES_MAX);
    }

    #[test]
    fn agent_state_snapshot_byte_ceiling은_exact_zero_plus_one과_custom_rollback을_보장한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace_id = db
            .create_workspace("agent-state-snapshot-byte-ceiling")
            .unwrap();
        let local_session_id = "snapshot-byte-local";
        let thread_id = "snapshot-byte-thread";
        let title = "bounded-title";
        let cwd = "/bounded/cwd";
        let model = "bounded-model";
        db.upsert_structured_thread(
            local_session_id,
            &workspace_id,
            thread_id,
            title,
            cwd,
            Some(model),
            false,
            false,
        )
        .unwrap();
        let retained_bytes = db
            .apply_agent_state_job(&AgentStateJob::projection(&workspace_id))
            .unwrap()
            .retained_bytes();

        let mut exact = AgentStateJob::projection(&workspace_id);
        assert_eq!(exact.snapshot_bytes_max, AGENT_STATE_SNAPSHOT_BYTES_MAX);
        exact.snapshot_bytes_max = retained_bytes;
        assert_eq!(
            db.apply_agent_state_job(&exact)
                .unwrap()
                .structured_threads
                .len(),
            1
        );

        for invalid in [0, AGENT_STATE_SNAPSHOT_BYTES_MAX + 1] {
            let mut job = AgentStateJob::projection(&workspace_id);
            job.snapshot_bytes_max = invalid;
            job.structured_mutations
                .push(StructuredThreadMutation::SetArchived {
                    local_session_id: local_session_id.to_owned(),
                    archived: true,
                });
            assert_eq!(
                db.apply_agent_state_job(&job).unwrap_err().to_string(),
                AGENT_STATE_INPUT_INVALID
            );
            assert!(!db.list_structured_threads(&workspace_id, true).unwrap()[0].archived);
        }

        let mut one_byte_short = AgentStateJob::projection(&workspace_id);
        one_byte_short.snapshot_bytes_max = retained_bytes - 1;
        one_byte_short
            .structured_mutations
            .push(StructuredThreadMutation::SetArchived {
                local_session_id: local_session_id.to_owned(),
                archived: true,
            });
        assert_eq!(
            db.apply_agent_state_job(&one_byte_short)
                .unwrap_err()
                .to_string(),
            AGENT_STATE_SNAPSHOT_INVALID
        );
        assert!(
            !db.list_structured_threads(&workspace_id, true).unwrap()[0].archived,
            "custom output cap failure must roll back the exact mutation"
        );
    }

    #[test]
    fn agent_state_retention_preflight는_validate후_capacity를정규화한다() {
        fn spare_string(value: &str, capacity: usize) -> String {
            let mut output = String::with_capacity(capacity);
            output.push_str(value);
            output
        }

        let workspace = spare_string("workspace", AGENT_STATE_JOB_BYTES_MAX * 2);
        let mut job = AgentStateJob::projection(workspace);
        job.structured_workspace_ids = Vec::with_capacity(32);
        job.structured_workspace_ids
            .push(spare_string("workspace", 64 * 1024));
        job.stale_binding_deletes = Vec::with_capacity(AGENT_STATE_EXACT_MUTATIONS_MAX * 4);
        job.stale_binding_deletes.push(AgentSessionIdentity {
            pane_id: spare_string("pane", 64 * 1024),
            kind: spare_string("codex", 64 * 1024),
            session_id: spare_string("session", 64 * 1024),
        });
        assert!(agent_state_job_retained_bytes(&job).unwrap() > AGENT_STATE_JOB_BYTES_MAX);

        let retention = prepare_agent_state_job_for_retention(&mut job).unwrap();
        assert_eq!(
            retention.retained_bytes(),
            agent_state_job_retained_bytes(&job).unwrap()
        );
        assert!(retention.retained_bytes() <= AGENT_STATE_JOB_BYTES_MAX);
        assert_eq!(job.workspace_id.capacity(), job.workspace_id.len());
        assert_eq!(
            job.structured_workspace_ids.capacity(),
            job.structured_workspace_ids.len()
        );
        assert_eq!(
            job.stale_binding_deletes.capacity(),
            job.stale_binding_deletes.len()
        );
        let identity = &job.stale_binding_deletes[0];
        assert_eq!(identity.pane_id.capacity(), identity.pane_id.len());
        assert_eq!(identity.kind.capacity(), identity.kind.len());
        assert_eq!(identity.session_id.capacity(), identity.session_id.len());
        assert_eq!(format!("{retention:?}"), "AgentStateJobRetention");
        assert_eq!(
            format!("{:?}", AgentStatePreparationErrorCode::InvalidInput),
            "invalid_input"
        );
        assert_eq!(
            format!("{:?}", AgentStatePreparationErrorCode::ResourceLimit),
            "resource_limit"
        );
    }

    #[test]
    fn agent_state_retention_preflight는_plus_one을변경전거부하고overflow를닫는다() {
        let workspace = "workspace";
        let mut job = AgentStateJob::projection(workspace);
        job.structured_mutations = Vec::with_capacity(AGENT_STATE_STRUCTURED_MUTATIONS_MAX * 2);
        for index in 0..=AGENT_STATE_STRUCTURED_MUTATIONS_MAX {
            job.structured_mutations
                .push(StructuredThreadMutation::Delete {
                    local_session_id: format!("local-{index}"),
                });
        }
        let original_capacity = job.structured_mutations.capacity();
        assert_eq!(
            prepare_agent_state_job_for_retention(&mut job),
            Err(AgentStatePreparationErrorCode::InvalidInput)
        );
        assert_eq!(job.structured_mutations.capacity(), original_capacity);

        let mut total = usize::MAX;
        assert_eq!(
            checked_agent_state_retained_add(&mut total, 1),
            Err(AgentStatePreparationErrorCode::ResourceLimit)
        );
        assert_eq!(total, usize::MAX);
    }

    #[test]
    fn agent_state_binding_reconcile은_exact_256과_plus_one을_검증한다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("agent-state-bindings").unwrap();
        let live_pane_ids = (0..AGENT_STATE_BINDING_ROWS_MAX)
            .map(|index| format!("pane-{index}"))
            .collect::<Vec<_>>();
        let desired_bindings = live_pane_ids
            .iter()
            .enumerate()
            .map(|(index, pane_id)| AgentSessionRow {
                pane_id: pane_id.clone(),
                kind: "codex".to_owned(),
                session_id: format!("session-{index}"),
                task_prompt: None,
            })
            .collect::<Vec<_>>();
        let mut exact = AgentStateJob::projection(&ws);
        exact.binding_reconcile = Some(AgentSessionBindingReconcile {
            live_pane_ids: live_pane_ids.clone(),
            desired_bindings,
        });
        assert_eq!(
            db.apply_agent_state_job(&exact)
                .unwrap()
                .agent_sessions
                .len(),
            AGENT_STATE_BINDING_ROWS_MAX
        );

        let mut plus_one = AgentStateJob::projection(&ws);
        let mut too_many = live_pane_ids;
        too_many.push("pane-plus-one".to_owned());
        plus_one.binding_reconcile = Some(AgentSessionBindingReconcile {
            live_pane_ids: too_many,
            desired_bindings: Vec::new(),
        });
        assert_eq!(
            db.apply_agent_state_job(&plus_one).unwrap_err().to_string(),
            AGENT_STATE_INPUT_INVALID
        );

        let mut control = AgentStateJob::projection(&ws);
        control.binding_reconcile = Some(AgentSessionBindingReconcile {
            live_pane_ids: vec!["pane\ncontrol".to_owned()],
            desired_bindings: Vec::new(),
        });
        assert_eq!(
            db.apply_agent_state_job(&control).unwrap_err().to_string(),
            AGENT_STATE_INPUT_INVALID
        );

        let exact_workspace_id = "w".repeat(BOUNDED_ID_BYTES_MAX);
        assert!(
            db.apply_agent_state_job(&AgentStateJob::projection(&exact_workspace_id))
                .is_ok()
        );
        assert_eq!(
            db.apply_agent_state_job(&AgentStateJob::projection(format!("{exact_workspace_id}w")))
                .unwrap_err()
                .to_string(),
            AGENT_STATE_INPUT_INVALID
        );
    }

    #[test]
    fn agent_state_public_dto_debug는_hostile_marker를_노출하지_않는다() {
        let marker = "agent-state-hostile-secret-marker";
        let identity = AgentSessionIdentity {
            pane_id: marker.to_owned(),
            kind: marker.to_owned(),
            session_id: marker.to_owned(),
        };
        let reconcile = AgentSessionBindingReconcile {
            live_pane_ids: vec![marker.to_owned()],
            desired_bindings: vec![AgentSessionRow {
                pane_id: marker.to_owned(),
                kind: marker.to_owned(),
                session_id: marker.to_owned(),
                task_prompt: None,
            }],
        };
        let clear = AgentTurnDoneClear {
            session_key: marker.to_owned(),
            seen_at: i64::MAX,
        };
        let structured = agent_state_structured_row(marker, 0, marker.to_owned());
        let mutation = StructuredThreadMutation::Upsert(structured.clone());
        let job = AgentStateJob {
            workspace_id: marker.to_owned(),
            structured_workspace_ids: vec![marker.to_owned()],
            snapshot_bytes_max: AGENT_STATE_SNAPSHOT_BYTES_MAX,
            binding_reconcile: Some(reconcile.clone()),
            stale_binding_deletes: vec![identity.clone()],
            turn_done_clears: vec![clear.clone()],
            structured_mutations: vec![mutation.clone()],
            work_turn_mutations: Vec::new(),
            include_hook_status: true,
            include_attention: true,
            include_agent_sessions: true,
            include_global_agent_sessions: true,
            include_structured_threads: true,
            include_archived_threads: true,
            include_work_history: true,
            include_activity_panes: true,
        };
        let snapshot = AgentStateSnapshot {
            hook_sessions: vec![HookSessionRow {
                session_key: marker.to_owned(),
                kind: marker.to_owned(),
                agent_session_id: marker.to_owned(),
                transcript_path: marker.to_owned(),
                task_prompt: Some(marker.to_owned()),
            }],
            statuslines: vec![StatuslineRow {
                session_key: marker.to_owned(),
                effort: Some(marker.to_owned()),
                model: Some(marker.to_owned()),
                context_pct: Some(99),
            }],
            waiting_sessions: vec![(marker.to_owned(), Some(marker.to_owned()))],
            response_sessions: vec![marker.to_owned()],
            turn_done_sessions: vec![(marker.to_owned(), i64::MAX)],
            idle_sessions: vec![(marker.to_owned(), 0, 1)],
            working_sessions: vec![marker.to_owned()],
            agent_sessions: reconcile.desired_bindings.clone(),
            archived_agent_resume: vec![ArchivedAgentResumeRow {
                persistent_session_id: marker.to_owned(),
                agent_id: marker.to_owned(),
                kind: Some(marker.to_owned()),
                session_id: Some(marker.to_owned()),
            }],
            structured_threads: vec![structured],
            work_turns: Vec::new(),
            activity_panes: vec![PersistedActivityPane {
                workspace_id: marker.to_owned(),
                pane_id: marker.to_owned(),
                title: marker.to_owned(),
                cwd: marker.to_owned(),
            }],
            global_agent_sessions: vec![(marker.to_owned(), marker.to_owned())],
        };

        for debug in [
            format!("{identity:?}"),
            format!("{reconcile:?}"),
            format!("{clear:?}"),
            format!("{mutation:?}"),
            format!("{job:?}"),
            format!("{snapshot:?}"),
        ] {
            assert!(!debug.contains(marker), "{debug}");
        }
    }

    #[test]
    fn fleet_wait_migration_restores_only_proven_modern_completions() {
        let conn = Connection::open_in_memory().unwrap();
        let idle_migration = MIGRATIONS
            .iter()
            .position(|sql| sql.contains("ADD COLUMN idle_since"))
            .unwrap();
        for sql in &MIGRATIONS[..idle_migration] {
            conn.execute_batch(sql).unwrap();
        }
        let complete =
            serde_json::json!({"native_session_id":"native", "generation":1_700_000_000_000_000i64,
            "last_activity":1_700_000_010_000_000i64, "turn_started":1_700_000_000_000_000i64,
            "completed_turn":"turn", "requests":[]})
            .to_string();
        for (key, json, revision) in [
            ("ws:1", complete.clone(), 1_700_000_010_000_000i64),
            ("ws:2", complete.clone(), 1_700_000_001_000_000i64),
            ("ws:3", "invalid".into(), 1_700_000_010_000_000i64),
            (
                "ws:4",
                complete.replace("\"requests\":[]", "\"requests\":[\"bad\"]"),
                1_700_000_010_000_000i64,
            ),
        ] {
            conn.execute("INSERT INTO agent_needs_input(session_key,waiting,working,turn_done,updated_at,attention_json,attention_revision) VALUES(?1,0,0,0,1,?2,?3)",rusqlite::params![key,json,revision]).unwrap();
        }
        for migration in &MIGRATIONS[idle_migration..] {
            conn.execute_batch(migration).unwrap();
        }
        for (key, expected) in [
            ("ws:1", Some(1_700_000_010i64)),
            ("ws:2", None),
            ("ws:3", None),
            ("ws:4", None),
        ] {
            let actual: Option<i64> = conn
                .query_row(
                    "SELECT idle_since FROM agent_needs_input WHERE session_key=?1",
                    [key],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(actual, expected, "{key}");
        }
    }

    #[test]
    fn fleet_wait_startup_prune_retains_acknowledged_idle_under_existing_caps() {
        let db = Db::open_in_memory().unwrap();
        db.set_agent_turn_done("ws:1").unwrap();
        db.conn.execute("UPDATE agent_needs_input SET turn_done=0,attention_json='{}',updated_at=1,idle_since=100",[]).unwrap();
        db.set_agent_needs_input("ws:2", false, None).unwrap();
        db.conn
            .execute(
                "UPDATE agent_needs_input SET updated_at=1 WHERE session_key='ws:2'",
                [],
            )
            .unwrap();
        db.prune_agent_hook_state().unwrap();
        let clocks: Vec<(String, i64)> = db
            .conn
            .prepare("SELECT session_key,idle_since FROM agent_needs_input")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(clocks, vec![("ws:1".into(), 100)]);
    }

    #[test]
    fn fleet_wait_snapshot_keeps_acknowledged_clock_and_checks_bounds() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("fleet-clock").unwrap();
        let key = format!("{ws}:1");
        db.set_agent_turn_done(&key).unwrap();
        let seen = db.list_turn_done_sessions().unwrap()[0].1;
        db.clear_agent_turn_done(&key, seen).unwrap();
        let job = AgentStateJob::projection(&ws);
        let snapshot = db.apply_agent_state_job(&job).unwrap();
        assert!(snapshot.turn_done_sessions.is_empty());
        assert_eq!(snapshot.idle_sessions[0].0, key);
        assert_eq!(snapshot.idle_sessions[0].1, seen);
        assert_eq!(snapshot.idle_sessions[0].2 / 1_000_000, seen);
        let mut omitted = job.clone();
        omitted.include_attention = false;
        assert_eq!(
            db.apply_agent_state_job(&omitted)
                .unwrap()
                .idle_sessions
                .capacity(),
            0
        );
        let mut tiny = job.clone();
        tiny.snapshot_bytes_max = 1;
        assert!(db.apply_agent_state_job(&tiny).is_err());
        for value in ["'bad'", "-1", "1.5"] {
            db.conn
                .execute(
                    &format!(
                        "UPDATE agent_needs_input SET idle_since={value} WHERE session_key=?1"
                    ),
                    [&key],
                )
                .unwrap();
            assert!(db.apply_agent_state_job(&job).is_err());
        }
    }

    #[test]
    fn agent_state_exact_cas는_새_binding과_새_turn을_지우지_않는다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("agent-state-cas").unwrap();
        db.upsert_agent_session(&ws, "pane", "claude", "old")
            .unwrap();
        db.upsert_agent_session(&ws, "pane", "codex", "new")
            .unwrap();
        let key = format!("{ws}:1");
        db.set_agent_turn_done(&key).unwrap();
        let seen_at = db.list_turn_done_sessions().unwrap()[0].1;

        let mut stale = AgentStateJob::projection(&ws);
        stale.stale_binding_deletes.push(AgentSessionIdentity {
            pane_id: "pane".to_owned(),
            kind: "claude".to_owned(),
            session_id: "old".to_owned(),
        });
        stale.turn_done_clears.push(AgentTurnDoneClear {
            session_key: key.clone(),
            seen_at: seen_at - 1,
        });
        let snapshot = db.apply_agent_state_job(&stale).unwrap();
        assert_eq!(snapshot.agent_sessions[0].session_id, "new");
        assert_eq!(snapshot.turn_done_sessions, vec![(key.clone(), seen_at)]);

        let mut exact = AgentStateJob::projection(&ws);
        exact.stale_binding_deletes.push(AgentSessionIdentity {
            pane_id: "pane".to_owned(),
            kind: "codex".to_owned(),
            session_id: "new".to_owned(),
        });
        exact.turn_done_clears.push(AgentTurnDoneClear {
            session_key: key,
            seen_at,
        });
        let snapshot = db.apply_agent_state_job(&exact).unwrap();
        assert!(snapshot.agent_sessions.is_empty());
        assert!(snapshot.turn_done_sessions.is_empty());
    }

    #[test]
    fn agent_state_structured_batch는_item_byte상한과_atomic_rollback을_보장한다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("agent-state-structured").unwrap();
        let mut exact = AgentStateJob::projection(&ws);
        for index in 0..AGENT_STATE_STRUCTURED_MUTATIONS_MAX {
            let local = format!("local-{index}");
            let thread = format!("thread-{index}");
            let title_bytes =
                STRUCTURED_THREAD_ROW_BYTES_MAX - local.len() - ws.len() - thread.len();
            exact
                .structured_mutations
                .push(StructuredThreadMutation::Upsert(
                    agent_state_structured_row(&ws, index, "x".repeat(title_bytes)),
                ));
        }
        let snapshot = db.apply_agent_state_job(&exact).unwrap();
        assert_eq!(
            snapshot.structured_threads.len(),
            AGENT_STATE_STRUCTURED_MUTATIONS_MAX
        );

        let mut item_plus_one = exact.clone();
        item_plus_one
            .structured_mutations
            .push(StructuredThreadMutation::Delete {
                local_session_id: "extra".to_owned(),
            });
        assert_eq!(
            db.apply_agent_state_job(&item_plus_one)
                .unwrap_err()
                .to_string(),
            AGENT_STATE_INPUT_INVALID
        );

        let mut byte_plus_one = exact;
        let StructuredThreadMutation::Upsert(first) = &mut byte_plus_one.structured_mutations[0]
        else {
            unreachable!()
        };
        first.title.push('x');
        assert_eq!(
            db.apply_agent_state_job(&byte_plus_one)
                .unwrap_err()
                .to_string(),
            AGENT_STATE_INPUT_INVALID
        );

        db.upsert_structured_thread(
            "rollback-existing",
            &ws,
            "rollback-thread",
            "title",
            "",
            None,
            false,
            false,
        )
        .unwrap();
        db.conn
            .execute_batch(
                "CREATE TRIGGER fail_agent_state_structured_insert
                 BEFORE INSERT ON structured_threads
                 WHEN NEW.local_session_id = 'explode'
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )
            .unwrap();
        let mut rollback = AgentStateJob::projection(&ws);
        rollback
            .structured_mutations
            .push(StructuredThreadMutation::SetArchived {
                local_session_id: "rollback-existing".to_owned(),
                archived: true,
            });
        rollback
            .structured_mutations
            .push(StructuredThreadMutation::Upsert(
                agent_state_structured_row(&ws, usize::MAX, "explode".to_owned()),
            ));
        let StructuredThreadMutation::Upsert(explode) =
            rollback.structured_mutations.last_mut().unwrap()
        else {
            unreachable!()
        };
        explode.local_session_id = "explode".to_owned();
        assert_eq!(
            db.apply_agent_state_job(&rollback).unwrap_err().to_string(),
            AGENT_STATE_PERSIST_FAILED
        );
        assert!(
            !db.list_structured_threads(&ws, true)
                .unwrap()
                .into_iter()
                .find(|row| row.local_session_id == "rollback-existing")
                .unwrap()
                .archived
        );

        db.conn
            .execute_batch(
                "DROP TRIGGER fail_agent_state_structured_insert;
                 CREATE TRIGGER fail_agent_state_binding_insert
                 BEFORE INSERT ON agent_sessions
                 WHEN NEW.pane_id = 'binding-explode'
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )
            .unwrap();
        let mut reconcile_rollback = AgentStateJob::projection(&ws);
        reconcile_rollback
            .structured_mutations
            .push(StructuredThreadMutation::SetArchived {
                local_session_id: "rollback-existing".to_owned(),
                archived: true,
            });
        reconcile_rollback.binding_reconcile = Some(AgentSessionBindingReconcile {
            live_pane_ids: vec!["binding-explode".to_owned()],
            desired_bindings: vec![AgentSessionRow {
                pane_id: "binding-explode".to_owned(),
                kind: "codex".to_owned(),
                session_id: "session".to_owned(),
                task_prompt: None,
            }],
        });
        assert_eq!(
            db.apply_agent_state_job(&reconcile_rollback)
                .unwrap_err()
                .to_string(),
            AGENT_STATE_PERSIST_FAILED
        );
        assert!(
            !db.list_structured_threads(&ws, true)
                .unwrap()
                .into_iter()
                .find(|row| row.local_session_id == "rollback-existing")
                .unwrap()
                .archived
        );
        assert!(db.list_agent_sessions(&ws).unwrap().is_empty());
    }

    #[test]
    fn agent_state_projection_preflight실패는_앞선_mutation도_rollback한다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db
            .create_workspace("agent-state-projection-rollback")
            .unwrap();
        db.upsert_structured_thread(
            "local-rollback",
            &ws,
            "thread-rollback",
            "title",
            "",
            None,
            false,
            false,
        )
        .unwrap();
        let key = format!("{ws}:1");
        db.conn
            .execute(
                "INSERT INTO agent_hook_sessions
                    (session_key, kind, agent_session_id, transcript_path, updated_at)
                 VALUES (CAST(?1 AS BLOB), 'claude', 'agent', '',
                         CAST(strftime('%s','now') AS INTEGER))",
                [&key],
            )
            .unwrap();
        let mut job = AgentStateJob::projection(&ws);
        job.structured_mutations
            .push(StructuredThreadMutation::SetArchived {
                local_session_id: "local-rollback".to_owned(),
                archived: true,
            });
        assert_eq!(
            db.apply_agent_state_job(&job).unwrap_err().to_string(),
            BOUNDED_READ_ROW_INVALID
        );
        assert!(!db.list_structured_threads(&ws, true).unwrap()[0].archived);
    }

    #[test]
    fn agent_needs_input_set_clear_list() {
        let db = Db::open_in_memory().unwrap();
        db.set_agent_needs_input("workspace:pane-1", true, None)
            .unwrap();
        db.set_agent_needs_input("workspace:pane-2", true, None)
            .unwrap();
        db.set_agent_needs_input("workspace:pane-3", false, None)
            .unwrap();
        let mut waiting: Vec<String> = db
            .list_waiting_sessions()
            .unwrap()
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        waiting.sort();
        assert_eq!(
            waiting,
            vec![
                "workspace:pane-1".to_string(),
                "workspace:pane-2".to_string()
            ]
        );
        // clear → 목록에서 빠짐
        db.set_agent_needs_input("workspace:pane-1", false, None)
            .unwrap();
        assert_eq!(
            db.list_waiting_sessions().unwrap(),
            vec![("workspace:pane-2".to_string(), None)]
        );
    }

    /// hook이 보고한 대기 사유 문구는 그대로 실려 나오고, clear 시 함께 지워진다 —
    /// 해소된 질문이 다음 대기에 되살아나면 안 된다(2026-07-17).
    #[test]
    fn agent_needs_input_message_저장과_clear시_소거() {
        let db = Db::open_in_memory().unwrap();
        db.set_agent_needs_input(
            "workspace:pane-1",
            true,
            Some("Claude needs your permission to use Bash"),
        )
        .unwrap();
        assert_eq!(
            db.list_waiting_sessions().unwrap(),
            vec![(
                "workspace:pane-1".to_string(),
                Some("Claude needs your permission to use Bash".to_string())
            )]
        );
        // clear 후 다시 대기 — 이전 문구가 남아 있으면 안 된다.
        db.set_agent_needs_input("workspace:pane-1", false, None)
            .unwrap();
        db.set_agent_needs_input("workspace:pane-1", true, None)
            .unwrap();
        assert_eq!(
            db.list_waiting_sessions().unwrap(),
            vec![("workspace:pane-1".to_string(), None)]
        );
    }

    /// hook "작업 중"(v32) 라이프사이클: clear(UserPromptSubmit/PreToolUse)=working,
    /// needs-input(승인 대기)=중단, Stop(turn-done)=중단.
    #[test]
    fn agent_working_은_clear가_켜고_needs_input과_turn_done이_끈다() {
        let db = Db::open_in_memory().unwrap();
        // 턴 시작(clear) → working
        db.set_agent_needs_input("workspace:pane-1", false, None)
            .unwrap();
        assert_eq!(
            db.list_working_sessions().unwrap(),
            vec!["workspace:pane-1".to_string()]
        );
        // 승인 대기 → working 중단(작업이 막힘)
        db.set_agent_needs_input("workspace:pane-1", true, None)
            .unwrap();
        assert!(db.list_working_sessions().unwrap().is_empty());
        // 승인 후 PreToolUse(clear) → 다시 working
        db.set_agent_needs_input("workspace:pane-1", false, None)
            .unwrap();
        assert_eq!(db.list_working_sessions().unwrap().len(), 1);
        // Stop(턴 완료) → working 중단 (turn_done SQL이 working 컬럼을 나열하지 않아
        // INSERT OR REPLACE가 DEFAULT 0으로 리셋)
        db.set_agent_turn_done("workspace:pane-1").unwrap();
        assert!(db.list_working_sessions().unwrap().is_empty());
        assert_eq!(db.list_turn_done_sessions().unwrap().len(), 1);
    }

    #[test]
    fn agent_turn_done_set_clear_및_needs_input과_상호리셋() {
        let db = Db::open_in_memory().unwrap();
        // Stop hook → turn_done=1, waiting=0
        db.set_agent_turn_done("workspace:pane-1").unwrap();
        let listed = db.list_turn_done_sessions().unwrap();
        assert_eq!(listed.len(), 1);
        let (key, seen_at) = listed[0].clone();
        assert_eq!(key, "workspace:pane-1");
        assert!(db.list_waiting_sessions().unwrap().is_empty());
        // 읽은 세대 이전 이벤트만 소비 — 더 새 이벤트(seen_at 미래)는 남는다(레이스 방지)
        db.clear_agent_turn_done("workspace:pane-1", seen_at - 1)
            .unwrap();
        assert_eq!(db.list_turn_done_sessions().unwrap().len(), 1);
        // 확인(소비) → turn_done만 내림
        db.clear_agent_turn_done("workspace:pane-1", seen_at)
            .unwrap();
        assert!(db.list_turn_done_sessions().unwrap().is_empty());
        // needs-input(REPLACE)이 turn_done을 자연 리셋
        db.set_agent_turn_done("workspace:pane-2").unwrap();
        db.set_agent_needs_input("workspace:pane-2", true, None)
            .unwrap();
        assert!(db.list_turn_done_sessions().unwrap().is_empty());
        assert_eq!(
            db.list_waiting_sessions().unwrap(),
            vec![("workspace:pane-2".to_string(), None)]
        );
    }

    /// agent_needs_input의 모든 행을 `seconds`초 전으로 되돌린다 — stale 창 검증용.
    fn backdate_agent_needs_input(db: &Db, seconds: i64) {
        db.conn
            .execute(
                "UPDATE agent_needs_input
                    SET updated_at = CAST(strftime('%s','now') AS INTEGER) - ?1",
                [seconds],
            )
            .unwrap();
    }

    /// waiting/turn_done은 working과 달리 하트비트가 없다(상태 전이 때 한 번만 기록) —
    /// 1시간 창이던 시절엔 실제로는 계속 승인을 기다리는 에이전트의 배지가 조용히
    /// 사라졌다(false negative). 24시간 창은 넘기고, 그 뒤에는 여전히 잘린다.
    #[test]
    fn waiting과_turn_done은_1시간이_지나도_24시간_안이면_유지된다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("ws").unwrap();
        db.set_agent_needs_input(&format!("{ws}:1"), true, Some("승인 필요"))
            .unwrap();
        db.set_agent_turn_done(&format!("{ws}:2")).unwrap();

        backdate_agent_needs_input(&db, 2 * 3600);
        assert_eq!(db.list_waiting_sessions().unwrap().len(), 1);
        assert_eq!(db.list_turn_done_sessions().unwrap().len(), 1);
        let snapshot = db
            .apply_agent_state_job(&AgentStateJob::projection(&ws))
            .unwrap();
        assert_eq!(snapshot.waiting_sessions.len(), 1);
        assert_eq!(snapshot.turn_done_sessions.len(), 1);

        backdate_agent_needs_input(&db, 25 * 3600);
        assert!(db.list_waiting_sessions().unwrap().is_empty());
        assert!(db.list_turn_done_sessions().unwrap().is_empty());
        let snapshot = db
            .apply_agent_state_job(&AgentStateJob::projection(&ws))
            .unwrap();
        assert!(snapshot.waiting_sessions.is_empty());
        assert!(snapshot.turn_done_sessions.is_empty());
    }

    /// warm turn_done 격차(감사 발견) — turn_done_sessions는 이제 waiting/working과 같은
    /// 전역 스코프라, job이 요청한(활성) workspace와 다른 warm workspace의 완료 세션도
    /// 스냅샷에 나타나야 한다(prefix 스코프였다면 누락됐을 것).
    #[test]
    fn turn_done_sessions는_요청_workspace가_아닌_warm_세션도_전역으로_반환한다() {
        let db = Db::open_in_memory().unwrap();
        let active = db.create_workspace("active-ws").unwrap();
        let warm = db.create_workspace("warm-ws").unwrap();
        let warm_key = format!("{warm}:1");
        db.set_agent_turn_done(&warm_key).unwrap();

        // job은 active workspace를 요청하지만 turn_done은 warm workspace 세션의 것이다.
        let job = AgentStateJob::projection(&active);
        let snapshot = db.apply_agent_state_job(&job).unwrap();

        assert_eq!(snapshot.turn_done_sessions.len(), 1);
        assert_eq!(snapshot.turn_done_sessions[0].0, warm_key);
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
    fn delete_workspace가_structured_threads도_정리() {
        let mut db = Db::open_in_memory().unwrap();
        let ws = db.create_workspace("삭제대상").unwrap();
        let other = db.create_workspace("유지").unwrap();
        db.upsert_structured_thread(
            "local-1", &ws, "thread-1", "delete", "/repo", None, false, false,
        )
        .unwrap();
        db.upsert_structured_thread(
            "local-2", &other, "thread-2", "keep", "/other", None, false, false,
        )
        .unwrap();

        db.delete_workspace(&ws).unwrap();
        assert!(db.list_structured_threads(&ws, true).unwrap().is_empty());
        assert_eq!(db.list_structured_threads(&other, true).unwrap().len(), 1);
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
        assert_eq!(
            db.permission_rule("srv-1", "read_file").unwrap(),
            Some(read.clone())
        );
        assert!(db.permission_rule("srv-1", "missing").unwrap().is_none());
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
        db.insert_pending_approval(
            "req-1",
            "srv-1",
            "read_file",
            "path=/tmp/x",
            Some("h"),
            100,
            None,
        )
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
        db.insert_pending_approval(
            "req-2",
            "srv-1",
            "delete_file",
            "path=/tmp/y",
            None,
            101,
            None,
        )
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
        db.insert_pending_approval("b", "srv", "t", "prev", None, 300, None)
            .unwrap();
        db.insert_pending_approval("a", "srv", "t", "prev", Some("hh"), 100, None)
            .unwrap();
        db.insert_pending_approval("c", "srv", "t", "prev", None, 200, None)
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
        db.insert_pending_approval("req", "srv", "t", "prev", None, 100, None)
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
        db.insert_pending_approval("old", "srv", "t", "prev", None, 100, None)
            .unwrap();
        db.insert_pending_approval("recent", "srv", "t", "prev", None, 1000, None)
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
    fn session_cleanup은_exact_pending만_denied로_하고_멱등이다() {
        let db = Db::open_in_memory().unwrap();
        let session = "315f68b6-333f-409f-a2c5-922b9eacfd7e:2";
        let other_session = "315f68b6-333f-409f-a2c5-922b9eacfd7e:3";
        for id in ["session-a", "session-b", "already-allowed"] {
            db.insert_pending_approval(id, "srv", "tool", "{}", None, 100, Some(session))
                .unwrap();
        }
        db.insert_pending_approval(
            "other-session",
            "srv",
            "tool",
            "{}",
            None,
            100,
            Some(other_session),
        )
        .unwrap();
        db.resolve_approval("already-allowed", true, true, 150)
            .unwrap();

        assert_eq!(
            db.deny_pending_approvals_for_session(session, 200).unwrap(),
            2
        );
        assert_eq!(
            db.deny_pending_approvals_for_session(session, 300).unwrap(),
            0
        );
        for id in ["session-a", "session-b"] {
            assert_eq!(db.poll_approval(id).unwrap().status, ApprovalStatus::Denied);
            let (remember, resolved_at): (i64, Option<i64>) = db
                .conn
                .query_row(
                    "SELECT remember, resolved_at FROM pending_approvals WHERE id = ?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!((remember, resolved_at), (0, Some(200)));
        }
        assert_eq!(
            db.poll_approval("other-session").unwrap().status,
            ApprovalStatus::Pending
        );
        assert_eq!(
            db.poll_approval("already-allowed").unwrap(),
            ApprovalOutcome {
                status: ApprovalStatus::Allowed,
                remember: true,
            }
        );
        let allowed_resolved_at: Option<i64> = db
            .conn
            .query_row(
                "SELECT resolved_at FROM pending_approvals WHERE id = 'already-allowed'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(allowed_resolved_at, Some(150));
    }

    #[test]
    fn session_cleanup은_없는_exact_session에서_zero다() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(
            db.deny_pending_approvals_for_session("315f68b6-333f-409f-a2c5-922b9eacfd7e:404", 200,)
                .unwrap(),
            0
        );
    }

    #[test]
    fn session_cleanup_failure는_모든_pending을_rollback한다() {
        let db = Db::open_in_memory().unwrap();
        let session = "315f68b6-333f-409f-a2c5-922b9eacfd7e:5";
        for id in ["cleanup-first", "cleanup-second"] {
            db.insert_pending_approval(id, "srv", "tool", "{}", None, 100, Some(session))
                .unwrap();
        }
        db.conn
            .execute_batch(
                "CREATE TRIGGER fail_session_cleanup
                 AFTER UPDATE OF status ON pending_approvals
                 WHEN NEW.id = 'cleanup-second' AND NEW.status = 'denied'
                 BEGIN SELECT RAISE(ABORT, 'injected session cleanup failure'); END;",
            )
            .unwrap();

        assert!(db.deny_pending_approvals_for_session(session, 200).is_err());
        for id in ["cleanup-first", "cleanup-second"] {
            assert_eq!(
                db.poll_approval(id).unwrap().status,
                ApprovalStatus::Pending
            );
            let resolved_at: Option<i64> = db
                .conn
                .query_row(
                    "SELECT resolved_at FROM pending_approvals WHERE id = ?1",
                    [id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(resolved_at, None);
        }
    }

    #[test]
    fn session_cleanup은_restart후에도_terminal_state를_보존한다() {
        let (dir, path, db) = file_db("session-approval-cleanup");
        let session = "315f68b6-333f-409f-a2c5-922b9eacfd7e:6";
        db.insert_pending_approval(
            "restart-cleanup",
            "srv",
            "tool",
            "{}",
            None,
            100,
            Some(session),
        )
        .unwrap();
        assert_eq!(
            db.deny_pending_approvals_for_session(session, 200).unwrap(),
            1
        );
        drop(db);

        let reopened = Db::open(&path).unwrap();
        assert_eq!(
            reopened.poll_approval("restart-cleanup").unwrap().status,
            ApprovalStatus::Denied
        );
        let resolved_at: Option<i64> = reopened
            .conn
            .query_row(
                "SELECT resolved_at FROM pending_approvals WHERE id = 'restart-cleanup'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(resolved_at, Some(200));
        assert_eq!(
            reopened
                .deny_pending_approvals_for_session(session, 300)
                .unwrap(),
            0
        );
        drop(reopened);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn session_cleanup은_key와_insert_candidate_count를_선할당없이_제한한다() {
        let db = Db::open_in_memory().unwrap();
        let exact_max = "00000000-0000-0000-0000-000000000000:18446744073709551615";
        assert_eq!(exact_max.len(), PENDING_APPROVAL_SESSION_KEY_BYTES_MAX);
        assert_eq!(
            db.deny_pending_approvals_for_session(exact_max, 200)
                .unwrap(),
            0
        );

        let oversized = format!("{exact_max}0");
        let error = db
            .deny_pending_approvals_for_session(&oversized, 200)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "pending approval session key byte length invalid"
        );
        assert!(
            db.deny_pending_approvals_for_session("not-a-session-key", 200)
                .is_err()
        );

        for index in 0..PENDING_APPROVAL_SESSION_CLEANUP_LIMIT_MAX {
            db.insert_pending_approval(
                &format!("bounded-cleanup-{index}"),
                "srv",
                "tool",
                "{}",
                None,
                100,
                Some(exact_max),
            )
            .unwrap();
        }
        let error = db
            .insert_pending_approval(
                "bounded-cleanup-overflow",
                "srv",
                "tool",
                "{}",
                None,
                100,
                Some(exact_max),
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            mcp_store::PENDING_APPROVAL_SESSION_LIMIT_ERROR
        );
        let pending_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pending_approvals
                 WHERE pane_id = ?1 AND status = 'pending'",
                [exact_max],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            pending_count,
            i64::try_from(PENDING_APPROVAL_SESSION_CLEANUP_LIMIT_MAX).unwrap()
        );
        assert_eq!(
            db.deny_pending_approvals_for_session(exact_max, 200)
                .unwrap(),
            PENDING_APPROVAL_SESSION_CLEANUP_LIMIT_MAX
        );
    }

    #[test]
    fn pending_global_cap은_concurrent_immediate_writers에서_overshoot하지_않는다() {
        let (dir, path, db) = file_db("pending-concurrent-cap");
        let rows = (0..(mcp_store::PENDING_APPROVAL_GLOBAL_LIMIT_MAX - 1))
            .map(|index| mcp_store::PendingApprovalInsert {
                id: format!("seed-{index}"),
                server_id: "srv".to_owned(),
                tool_name: "tool".to_owned(),
                arguments_preview: "{}".to_owned(),
                schema_hash: None,
                created_at: 1,
                pane_id: None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            mcp_store::insert_pending_approval_batch(&db.conn, &rows).unwrap(),
            rows.len()
        );
        drop(db);

        let db_a = Db::open(&path).unwrap();
        let db_b = Db::open(&path).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let spawn_writer =
            |db: Db, id: &'static str, barrier: std::sync::Arc<std::sync::Barrier>| {
                std::thread::spawn(move || {
                    barrier.wait();
                    db.insert_pending_approval(id, "srv", "tool", "{}", None, 2, None)
                        .map_err(|error| error.to_string())
                })
            };
        let writer_a = spawn_writer(db_a, "winner-a", barrier.clone());
        let writer_b = spawn_writer(db_b, "winner-b", barrier.clone());
        barrier.wait();
        let results = [writer_a.join().unwrap(), writer_b.join().unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter_map(|result| result.as_ref().err())
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec![mcp_store::PENDING_APPROVAL_GLOBAL_LIMIT_ERROR]
        );

        let reopened = Db::open(&path).unwrap();
        let page = reopened
            .list_pending_approvals_bounded(mcp_store::PENDING_APPROVAL_LIST_LIMIT_MAX)
            .unwrap();
        assert_eq!(
            page.rows.len(),
            mcp_store::PENDING_APPROVAL_GLOBAL_LIMIT_MAX
        );
        assert!(!page.has_more);
        drop(reopened);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resolve_approval은_오래된_resolved_rows를_prune한다() {
        let db = Db::open_in_memory().unwrap();
        db.insert_pending_approval("old", "srv", "t", "prev", None, 10, None)
            .unwrap();
        db.insert_pending_approval("keep", "srv", "t", "prev", None, 20, None)
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

    #[test]
    fn workspace_path_anchor_atomic은_exact_id의_pair만_갱신하고_partial을_거부한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace_id = db.create_workspace("atomic path").unwrap();

        let projection = db
            .set_workspace_path_and_anchor(&workspace_id, "/project", Some(11), Some(22))
            .unwrap();
        assert_eq!(projection.id, workspace_id);
        assert_eq!(projection.path, "/project");
        assert_eq!(
            projection.folder_anchor,
            Some(WorkspaceFolderAnchor { dev: 11, ino: 22 })
        );

        for (dev, ino) in [(Some(33), None), (None, Some(44))] {
            let error = db
                .set_workspace_path_and_anchor(&workspace_id, "/partial", dev, ino)
                .unwrap_err();
            assert!(format!("{error:#}").contains("settings_workspace_path_anchor_partial"));
        }
        assert_eq!(
            db.workspace_path(&workspace_id).unwrap().as_deref(),
            Some("/project")
        );
        assert_eq!(db.workspace_anchor(&workspace_id).unwrap(), Some((11, 22)));

        let cleared = db
            .set_workspace_path_and_anchor(&workspace_id, "/without-anchor", None, None)
            .unwrap();
        assert_eq!(cleared.path, "/without-anchor");
        assert_eq!(cleared.folder_anchor, None);
        assert!(
            db.set_workspace_path_and_anchor("missing", "/must-not-exist", None, None)
                .unwrap_err()
                .to_string()
                .contains("settings_workspace_path_exact_id_missing")
        );
    }

    #[test]
    fn workspace_path_anchor_atomic은_injected_failure에서_path와_anchor를_rollback한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace_id = db.create_workspace("rollback path").unwrap();
        db.set_workspace_path_and_anchor(&workspace_id, "/old", Some(101), Some(202))
            .unwrap();
        db.conn
            .execute_batch(
                "CREATE TEMP TRIGGER fail_workspace_path_anchor
                 AFTER UPDATE OF path, path_dev, path_ino ON workspaces
                 WHEN NEW.path = '/injected-failure'
                 BEGIN
                     SELECT RAISE(ABORT, 'injected workspace path/anchor failure');
                 END;",
            )
            .unwrap();

        assert!(
            db.set_workspace_path_and_anchor(
                &workspace_id,
                "/injected-failure",
                Some(303),
                Some(404),
            )
            .is_err()
        );
        assert_eq!(
            db.workspace_path(&workspace_id).unwrap().as_deref(),
            Some("/old")
        );
        assert_eq!(
            db.workspace_anchor(&workspace_id).unwrap(),
            Some((101, 202))
        );
    }

    #[test]
    fn workspace_path_anchor_atomic은_oversized_target을_update전에거부한다() {
        let db = Db::open_in_memory().unwrap();
        let oversized_name = "x".repeat(SETTINGS_ROW_BYTES_MAX);
        db.conn
            .execute(
                "INSERT INTO workspaces
                    (id, name, path, created_at, updated_at, path_dev, path_ino)
                 VALUES ('oversized-workspace', ?1, '/old', '', '', 7, 9)",
                [&oversized_name],
            )
            .unwrap();

        let error = db
            .set_workspace_path_and_anchor("oversized-workspace", "/new", Some(10), Some(12))
            .unwrap_err();
        assert!(format!("{error:#}").contains("settings_workspace_update_target_row_bytes_limit"));
        assert_eq!(
            db.workspace_path("oversized-workspace").unwrap().as_deref(),
            Some("/old")
        );
        assert_eq!(
            db.workspace_anchor("oversized-workspace").unwrap(),
            Some((7, 9))
        );
    }

    #[test]
    fn settings_workspace_projection은_256행_exact를_허용하고_plus_one을_거부한다() {
        let db = Db::open_in_memory().unwrap();
        db.conn
            .execute(
                "WITH RECURSIVE seq(n) AS (
                     SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n < ?1
                 )
                 INSERT INTO workspaces
                    (id, name, path, created_at, updated_at, path_dev, path_ino)
                 SELECT printf('workspace-%03d', n), 'workspace', printf('/workspace/%03d', n),
                        printf('%08d', n), printf('%08d', n), 7, 9 FROM seq",
                [i64::try_from(SETTINGS_WORKSPACE_LIMIT_MAX - 1).unwrap()],
            )
            .unwrap();

        let rows = db.settings_workspace_projection_rows().unwrap();
        assert_eq!(rows.len(), SETTINGS_WORKSPACE_LIMIT_MAX);
        assert_eq!(
            rows[0].folder_anchor,
            Some(WorkspaceFolderAnchor { dev: 7, ino: 9 })
        );

        db.conn
            .execute(
                "INSERT INTO workspaces
                    (id, name, path, created_at, updated_at, path_dev, path_ino)
                 VALUES ('workspace-over-limit', 'workspace', '/workspace/over-limit',
                         '99999999', '99999999', 7, 9)",
                [],
            )
            .unwrap();
        let error = db.settings_workspace_projection_rows().unwrap_err();
        assert!(
            format!("{error:#}").contains("settings_workspace_projection_item_limit"),
            "limit + 1 count must fail before row materialization: {error:#}"
        );
    }

    #[test]
    fn settings_workspace_projection은_row와_aggregate_byte_exact_plus_one을_강제한다() {
        let row_db = Db::open_in_memory().unwrap();
        let exact_row_name = "x".repeat(SETTINGS_ROW_BYTES_MAX - 1);
        row_db
            .conn
            .execute(
                "INSERT INTO workspaces
                    (id, name, path, created_at, updated_at, path_dev, path_ino)
                 VALUES ('r', ?1, '', '', '', NULL, NULL)",
                [&exact_row_name],
            )
            .unwrap();
        assert_eq!(
            row_db.settings_workspace_projection_rows().unwrap().len(),
            1
        );
        row_db
            .conn
            .execute(
                "UPDATE workspaces SET name = name || 'x' WHERE id = 'r'",
                [],
            )
            .unwrap();
        let row_error = row_db.settings_workspace_projection_rows().unwrap_err();
        assert!(format!("{row_error:#}").contains("settings_workspace_projection_row_bytes_limit"));

        let aggregate_db = Db::open_in_memory().unwrap();
        let exact_row_name = "y".repeat(SETTINGS_ROW_BYTES_MAX - 1);
        for id in ["a", "b", "c", "d"] {
            aggregate_db
                .conn
                .execute(
                    "INSERT INTO workspaces
                        (id, name, path, created_at, updated_at, path_dev, path_ino)
                     VALUES (?1, ?2, '', '', '', NULL, NULL)",
                    (id, &exact_row_name),
                )
                .unwrap();
        }
        assert_eq!(
            aggregate_db
                .settings_workspace_projection_rows()
                .unwrap()
                .len(),
            4
        );
        aggregate_db
            .conn
            .execute(
                "INSERT INTO workspaces
                    (id, name, path, created_at, updated_at, path_dev, path_ino)
                 VALUES ('e', '', '', '', '', NULL, NULL)",
                [],
            )
            .unwrap();
        let aggregate_error = aggregate_db
            .settings_workspace_projection_rows()
            .unwrap_err();
        assert!(
            format!("{aggregate_error:#}")
                .contains("settings_workspace_projection_retained_bytes_limit")
        );
    }

    #[test]
    fn workspace_find_or_create_exact_path는_name_path_anchor를_한번에_생성하고_재사용한다() {
        let db = Db::open_in_memory().unwrap();
        let anchor = WorkspaceFolderAnchor { dev: 11, ino: 22 };
        let created = db
            .find_or_create_workspace_by_exact_path("project", "/project", anchor)
            .unwrap();
        assert!(created.created);
        assert_eq!(created.row.name, "project");
        assert_eq!(created.row.path, "/project");
        assert_eq!(created.row.folder_anchor, Some(anchor));

        let reused = db
            .find_or_create_workspace_by_exact_path("ignored-new-name", "/project", anchor)
            .unwrap();
        assert!(!reused.created);
        assert_eq!(reused.row.id, created.row.id);
        assert_eq!(reused.row.name, "project");
        assert_eq!(reused.row.folder_anchor, Some(anchor));
        assert_eq!(db.list_workspaces().unwrap().len(), 1);
    }

    #[test]
    fn workspace_find_or_create_refreshes_remounted_device_at_same_path() {
        let db = Db::open_in_memory().unwrap();
        let original = WorkspaceFolderAnchor { dev: 11, ino: 22 };
        let current = WorkspaceFolderAnchor { dev: 33, ino: 22 };
        let created = db
            .find_or_create_workspace_by_exact_path_with_volume(
                "custom name",
                "/project",
                original,
                Some(uuid::Uuid::from_u128(1)),
            )
            .unwrap();

        let reopened = db
            .find_or_create_workspace_by_exact_path_with_volume(
                "ignored",
                "/project",
                current,
                Some(uuid::Uuid::from_u128(1)),
            )
            .unwrap();
        assert!(!reopened.created);
        assert_eq!(reopened.row.id, created.row.id);
        assert_eq!(reopened.row.name, "custom name");
        assert_eq!(reopened.row.path, "/project");
        assert_eq!(reopened.row.created_at, created.row.created_at);
        assert_eq!(reopened.row.folder_anchor, Some(current));
        assert_eq!(
            db.workspace_anchor(&created.row.id).unwrap(),
            Some((33, 22))
        );
        assert_eq!(db.list_workspaces().unwrap().len(), 1);
    }

    #[test]
    fn user_selected_legacy_exact_path_rebinds_once_with_a_recorded_volume() {
        let db = Db::open_in_memory().unwrap();
        let old = WorkspaceFolderAnchor { dev: 11, ino: 22 };
        let current = WorkspaceFolderAnchor { dev: 33, ino: 22 };
        let volume = uuid::Uuid::from_u128(1);
        let created = db
            .find_or_create_workspace_by_exact_path_with_volume("original", "/project", old, None)
            .unwrap();

        assert!(
            db.find_or_create_workspace_by_exact_path_with_volume(
                "ignored",
                "/project",
                current,
                Some(volume)
            )
            .unwrap_err()
            .to_string()
            .contains("workspace_legacy_rebind_requires_confirmation")
        );
        let selected = db
            .find_or_create_selected_workspace_by_exact_path_with_volume(
                "ignored",
                "/project",
                current,
                Some(volume),
            )
            .unwrap();
        assert!(!selected.created);
        assert_eq!(selected.row.id, created.row.id);
        assert_eq!(selected.row.name, "original");
        assert_eq!(
            db.workspace_anchor(&created.row.id).unwrap(),
            Some((33, 22))
        );
        assert!(
            db.find_or_create_selected_workspace_by_exact_path_with_volume(
                "ignored",
                "/project",
                current,
                Some(uuid::Uuid::from_u128(2))
            )
            .is_err()
        );
        assert_eq!(db.list_workspaces().unwrap().len(), 1);
    }

    #[test]
    fn user_selection_cannot_override_known_volume_or_changed_inode() {
        for (stored_volume, current, selected_volume) in [
            (None, WorkspaceFolderAnchor { dev: 33, ino: 22 }, None),
            (
                Some(uuid::Uuid::from_u128(1)),
                WorkspaceFolderAnchor { dev: 33, ino: 22 },
                Some(uuid::Uuid::from_u128(2)),
            ),
            (
                None,
                WorkspaceFolderAnchor { dev: 33, ino: 23 },
                Some(uuid::Uuid::from_u128(1)),
            ),
        ] {
            let db = Db::open_in_memory().unwrap();
            let old = WorkspaceFolderAnchor { dev: 11, ino: 22 };
            let created = db
                .find_or_create_workspace_by_exact_path_with_volume(
                    "original",
                    "/project",
                    old,
                    stored_volume,
                )
                .unwrap();
            assert!(
                db.find_or_create_selected_workspace_by_exact_path_with_volume(
                    "ignored",
                    "/project",
                    current,
                    selected_volume
                )
                .is_err()
            );
            assert_eq!(
                db.workspace_anchor(&created.row.id).unwrap(),
                Some((11, 22))
            );
        }
    }

    #[test]
    fn popup_review_unknown_volume_device_change_cannot_reuse_workspace() {
        let db = Db::open_in_memory().unwrap();
        let original = WorkspaceFolderAnchor { dev: 11, ino: 22 };
        let created = db
            .find_or_create_workspace_by_exact_path("original", "/project", original)
            .unwrap();
        let result = db.find_or_create_workspace_by_exact_path(
            "other",
            "/project",
            WorkspaceFolderAnchor { dev: 33, ino: 22 },
        );
        assert!(
            result.is_err(),
            "inode alone must not prove identity on a different device"
        );
        assert_eq!(
            db.workspace_anchor(&created.row.id).unwrap(),
            Some((11, 22))
        );
        assert_eq!(db.list_workspaces().unwrap().len(), 1);
    }

    #[test]
    fn popup_review_verified_volume_remount_keeps_workspace_identity() {
        let db = Db::open_in_memory().unwrap();
        let volume = uuid::Uuid::from_u128(1);
        let original = WorkspaceFolderAnchor { dev: 11, ino: 22 };
        let created = db
            .find_or_create_workspace_by_exact_path_with_volume(
                "original",
                "/project",
                original,
                Some(volume),
            )
            .unwrap();
        let current = WorkspaceFolderAnchor { dev: 33, ino: 22 };
        let reused = db
            .find_or_create_workspace_by_exact_path_with_volume(
                "other",
                "/project",
                current,
                Some(volume),
            )
            .unwrap();
        assert!(!reused.created);
        assert_eq!(reused.row.id, created.row.id);
        assert_eq!(reused.row.name, "original");
        assert_eq!(
            db.workspace_anchor(&created.row.id).unwrap(),
            Some((33, 22))
        );
        assert!(
            db.find_or_create_workspace_by_exact_path_with_volume(
                "other",
                "/project",
                WorkspaceFolderAnchor { dev: 44, ino: 23 },
                Some(volume)
            )
            .is_err()
        );
        assert!(
            db.find_or_create_workspace_by_exact_path_with_volume(
                "other",
                "/project",
                WorkspaceFolderAnchor { dev: 44, ino: 22 },
                None
            )
            .is_err()
        );
    }

    #[test]
    fn popup_review_different_volume_cannot_reuse_even_equal_device_and_inode() {
        let db = Db::open_in_memory().unwrap();
        let anchor = WorkspaceFolderAnchor { dev: 11, ino: 22 };
        let volume = uuid::Uuid::from_u128(1);
        let created = db
            .find_or_create_workspace_by_exact_path_with_volume(
                "original",
                "/project",
                anchor,
                Some(volume),
            )
            .unwrap();
        for dev in [11, 33] {
            assert!(
                db.find_or_create_workspace_by_exact_path_with_volume(
                    "other",
                    "/project",
                    WorkspaceFolderAnchor { dev, ino: 22 },
                    Some(uuid::Uuid::from_u128(2))
                )
                .is_err()
            );
            assert_eq!(
                db.workspace_anchor(&created.row.id).unwrap(),
                Some((11, 22))
            );
        }
        assert!(
            db.find_or_create_workspace_by_exact_path_with_volume(
                "other",
                "/alias",
                anchor,
                Some(uuid::Uuid::from_u128(2))
            )
            .is_err()
        );
    }

    #[test]
    fn popup_review_explicit_same_path_rebind_releases_old_volume_proof() {
        let db = Db::open_in_memory().unwrap();
        let anchor = WorkspaceFolderAnchor { dev: 11, ino: 22 };
        let old_volume = uuid::Uuid::new_v4();
        let current_volume = uuid::Uuid::new_v4();
        let created = db
            .find_or_create_workspace_by_exact_path_with_volume(
                "custom alias",
                "/project",
                anchor,
                Some(old_volume),
            )
            .unwrap();
        db.set_workspace_path_and_anchor(&created.row.id, "/project", Some(11), Some(22))
            .unwrap();
        let reopened = db
            .find_or_create_workspace_by_exact_path_with_volume(
                "ignored",
                "/project",
                anchor,
                Some(current_volume),
            )
            .expect("explicit reconnect must clear the old proof even at equal path/device/inode");
        assert_eq!(reopened.row.id, created.row.id);
        assert_eq!(reopened.row.name, "custom alias");
        db.set_workspace_path_and_anchor_with_volume(
            &created.row.id,
            "/project",
            Some(11),
            Some(22),
            Some(old_volume),
        )
        .unwrap();
        assert!(
            db.find_or_create_workspace_by_exact_path_with_volume(
                "ignored",
                "/project",
                anchor,
                Some(current_volume)
            )
            .is_err()
        );
        let remounted = db
            .find_or_create_workspace_by_exact_path_with_volume(
                "ignored",
                "/project",
                WorkspaceFolderAnchor { dev: 33, ino: 22 },
                Some(old_volume),
            )
            .unwrap();
        assert_eq!(remounted.row.id, created.row.id);
    }

    #[test]
    fn popup_review_path_rebind_invalidates_old_volume_proof() {
        let db = Db::open_in_memory().unwrap();
        let volume = uuid::Uuid::from_u128(1);
        let anchor = WorkspaceFolderAnchor { dev: 11, ino: 22 };
        let created = db
            .find_or_create_workspace_by_exact_path_with_volume(
                "original",
                "/project",
                anchor,
                Some(volume),
            )
            .unwrap();
        db.set_workspace_path_and_anchor(&created.row.id, "/other", Some(11), Some(23))
            .unwrap();
        assert!(
            db.find_or_create_workspace_by_exact_path_with_volume(
                "other",
                "/other",
                WorkspaceFolderAnchor { dev: 33, ino: 23 },
                Some(volume)
            )
            .is_err()
        );
    }

    #[test]
    fn popup_review_volume_identity_refresh_rolls_back_with_anchor() {
        let db = Db::open_in_memory().unwrap();
        let volume = uuid::Uuid::from_u128(1);
        let anchor = WorkspaceFolderAnchor { dev: 11, ino: 22 };
        let created = db
            .find_or_create_workspace_by_exact_path_with_volume(
                "original",
                "/project",
                anchor,
                Some(volume),
            )
            .unwrap();
        db.conn.execute_batch("CREATE TEMP TRIGGER fail_volume_cache BEFORE INSERT ON workspace_volume_identities BEGIN SELECT RAISE(ABORT, 'injected volume cache failure'); END;").unwrap();
        assert!(
            db.find_or_create_workspace_by_exact_path_with_volume(
                "other",
                "/project",
                WorkspaceFolderAnchor { dev: 33, ino: 22 },
                Some(volume)
            )
            .is_err()
        );
        assert_eq!(
            db.workspace_anchor(&created.row.id).unwrap(),
            Some((11, 22))
        );
        db.conn
            .execute_batch("DROP TRIGGER fail_volume_cache")
            .unwrap();
        assert!(
            db.find_or_create_workspace_by_exact_path_with_volume(
                "other",
                "/project",
                anchor,
                Some(uuid::Uuid::from_u128(2))
            )
            .is_err()
        );
    }

    #[test]
    fn workspace_find_or_create_remount_does_not_claim_another_workspace_anchor() {
        let db = Db::open_in_memory().unwrap();
        let original = WorkspaceFolderAnchor { dev: 11, ino: 22 };
        let current = WorkspaceFolderAnchor { dev: 33, ino: 22 };
        let created = db
            .find_or_create_workspace_by_exact_path_with_volume(
                "project",
                "/project",
                original,
                Some(uuid::Uuid::from_u128(1)),
            )
            .unwrap();
        db.find_or_create_workspace_by_exact_path_with_volume(
            "other",
            "/other",
            current,
            Some(uuid::Uuid::from_u128(1)),
        )
        .unwrap();
        let error = db
            .find_or_create_workspace_by_exact_path_with_volume(
                "ignored",
                "/project",
                current,
                Some(uuid::Uuid::from_u128(1)),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("workspace_folder_anchor_duplicate"));
        assert_eq!(
            db.workspace_anchor(&created.row.id).unwrap(),
            Some((11, 22))
        );
        assert_eq!(db.list_workspaces().unwrap().len(), 2);
    }

    #[test]
    fn workspace_find_or_create_remount_refresh_rolls_back_on_write_failure() {
        let db = Db::open_in_memory().unwrap();
        let original = WorkspaceFolderAnchor { dev: 11, ino: 22 };
        let created = db
            .find_or_create_workspace_by_exact_path_with_volume(
                "project",
                "/project",
                original,
                Some(uuid::Uuid::from_u128(1)),
            )
            .unwrap();
        db.conn
            .execute_batch(
                "CREATE TEMP TRIGGER fail_anchor_refresh BEFORE UPDATE OF path_dev ON workspaces
             BEGIN SELECT RAISE(ABORT, 'injected anchor refresh failure'); END;",
            )
            .unwrap();
        let error = db
            .find_or_create_workspace_by_exact_path_with_volume(
                "ignored",
                "/project",
                WorkspaceFolderAnchor { dev: 33, ino: 22 },
                Some(uuid::Uuid::from_u128(1)),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("injected anchor refresh failure"));
        assert_eq!(
            db.workspace_anchor(&created.row.id).unwrap(),
            Some((11, 22))
        );
        assert_eq!(db.list_workspaces().unwrap().len(), 1);
    }

    #[test]
    fn workspace_find_or_create_rejects_replaced_folder_at_existing_path() {
        let db = Db::open_in_memory().unwrap();
        let original = WorkspaceFolderAnchor { dev: 11, ino: 22 };
        let created = db
            .find_or_create_workspace_by_exact_path("original", "/project", original)
            .unwrap();
        let error = db
            .find_or_create_workspace_by_exact_path(
                "replacement",
                "/project",
                WorkspaceFolderAnchor { dev: 33, ino: 44 },
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("workspace_path_anchor_conflict"));
        let stored = db.list_workspaces().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].id, created.row.id);
    }

    #[test]
    fn workspace_find_or_create_rebinds_same_folder_from_alias_path() {
        let db = Db::open_in_memory().unwrap();
        let anchor = WorkspaceFolderAnchor { dev: 11, ino: 22 };
        let created = db
            .find_or_create_workspace_by_exact_path("project", "/alias/project", anchor)
            .unwrap();
        let rebound = db
            .find_or_create_workspace_by_exact_path("ignored", "/real/project", anchor)
            .unwrap();
        assert!(!rebound.created);
        assert_eq!(rebound.row.id, created.row.id);
        assert_eq!(rebound.row.path, "/real/project");
        assert_eq!(db.list_workspaces().unwrap().len(), 1);
    }

    #[test]
    fn workspace_find_or_create_does_not_claim_legacy_path_owned_by_another_anchor_row() {
        let db = Db::open_in_memory().unwrap();
        let anchor = WorkspaceFolderAnchor { dev: 11, ino: 22 };
        let known = db
            .find_or_create_workspace_by_exact_path("known", "/alias/project", anchor)
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO workspaces (id, name, path, created_at, updated_at)
             VALUES ('legacy', 'legacy', '/real/project', '', '')",
                [],
            )
            .unwrap();
        let error = db
            .find_or_create_workspace_by_exact_path("ignored", "/real/project", anchor)
            .unwrap_err();
        assert!(format!("{error:#}").contains("workspace_path_anchor_conflict"));
        assert_eq!(
            db.list_workspaces()
                .unwrap()
                .into_iter()
                .find(|row| row.id == known.row.id)
                .unwrap()
                .path,
            "/alias/project"
        );
    }

    #[test]
    fn workspace_find_or_create_rejects_ambiguous_duplicate_folder_anchor() {
        let db = Db::open_in_memory().unwrap();
        let anchor = WorkspaceFolderAnchor { dev: 11, ino: 22 };
        db.find_or_create_workspace_by_exact_path("project", "/project", anchor)
            .unwrap();
        db.conn.execute(
            "INSERT INTO workspaces (id, name, path, created_at, updated_at, path_dev, path_ino)
             VALUES ('alias', 'alias', '/alias/project', '', '', 11, 22)",
            [],
        ).unwrap();
        let error = db
            .find_or_create_workspace_by_exact_path("ignored", "/project", anchor)
            .unwrap_err();
        assert!(format!("{error:#}").contains("workspace_folder_anchor_duplicate"));
    }

    #[test]
    fn workspace_find_or_create_exact_path는_duplicate를_fail_closed한다() {
        let db = Db::open_in_memory().unwrap();
        for id in ["duplicate-a", "duplicate-b", "duplicate-c"] {
            db.conn
                .execute(
                    "INSERT INTO workspaces
                        (id, name, path, created_at, updated_at, path_dev, path_ino)
                     VALUES (?1, ?1, '/duplicate', ?1, ?1, 1, 2)",
                    [id],
                )
                .unwrap();
        }
        let error = db
            .find_or_create_workspace_by_exact_path(
                "must-not-exist",
                "/duplicate",
                WorkspaceFolderAnchor { dev: 1, ino: 2 },
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("settings_workspace_exact_path_duplicate"));
        assert_eq!(db.list_workspaces().unwrap().len(), 3);
        assert!(
            db.list_workspaces()
                .unwrap()
                .iter()
                .all(|row| row.name != "must-not-exist")
        );
    }

    #[test]
    fn workspace_find_or_create_exact_path는_insert_failure를_rollback한다() {
        let db = Db::open_in_memory().unwrap();
        db.conn
            .execute_batch(
                "CREATE TEMP TRIGGER fail_workspace_insert
                 BEFORE INSERT ON workspaces
                 WHEN NEW.path = '/injected-failure'
                 BEGIN
                     SELECT RAISE(ABORT, 'injected workspace insert failure');
                 END;",
            )
            .unwrap();
        assert!(
            db.find_or_create_workspace_by_exact_path(
                "rollback",
                "/injected-failure",
                WorkspaceFolderAnchor { dev: 1, ino: 2 },
            )
            .is_err()
        );
        assert!(db.list_workspaces().unwrap().is_empty());
    }

    #[test]
    fn workspace_moved_path_cas는_path와_anchor_stale에_무변경이고_match만_갱신한다() {
        let db = Db::open_in_memory().unwrap();
        let anchor = WorkspaceFolderAnchor { dev: 101, ino: 202 };
        let workspace = db
            .find_or_create_workspace_by_exact_path("project", "/old", anchor)
            .unwrap()
            .row;

        assert_eq!(
            db.update_workspace_moved_path_cas(
                &workspace.id,
                "/old",
                anchor,
                "/wrong-folder",
                WorkspaceFolderAnchor { dev: 101, ino: 303 },
            )
            .unwrap(),
            WorkspaceMovedPathUpdate::Stale
        );
        assert_eq!(
            db.workspace_path(&workspace.id).unwrap().as_deref(),
            Some("/old")
        );
        assert_eq!(
            db.workspace_anchor(&workspace.id).unwrap(),
            Some((101, 202))
        );

        assert_eq!(
            db.update_workspace_moved_path_cas(&workspace.id, "/stale", anchor, "/new", anchor)
                .unwrap(),
            WorkspaceMovedPathUpdate::Stale
        );
        assert_eq!(
            db.update_workspace_moved_path_cas(
                &workspace.id,
                "/old",
                WorkspaceFolderAnchor { dev: 101, ino: 404 },
                "/new",
                WorkspaceFolderAnchor { dev: 101, ino: 404 },
            )
            .unwrap(),
            WorkspaceMovedPathUpdate::Stale
        );
        assert_eq!(
            db.workspace_path(&workspace.id).unwrap().as_deref(),
            Some("/old")
        );

        assert_eq!(
            db.update_workspace_moved_path_cas(&workspace.id, "/old", anchor, "/new", anchor)
                .unwrap(),
            WorkspaceMovedPathUpdate::Updated
        );
        assert_eq!(
            db.workspace_path(&workspace.id).unwrap().as_deref(),
            Some("/new")
        );
        assert_eq!(
            db.workspace_anchor(&workspace.id).unwrap(),
            Some((101, 202))
        );

        assert_eq!(
            db.update_workspace_moved_path_cas(&workspace.id, "/old", anchor, "/late", anchor)
                .unwrap(),
            WorkspaceMovedPathUpdate::Stale
        );
        assert_eq!(
            db.workspace_path(&workspace.id).unwrap().as_deref(),
            Some("/new")
        );
    }

    fn sample(id: &str) -> CredentialMeta {
        CredentialMeta {
            id: id.into(),
            provider: "anthropic".into(),
            label: "개인 키".into(),
            credential_kind: "api_key".into(),
            masked_hint: Some("****3456".into()),
            workspace_id: None,
        }
    }

    fn sample_mcp_server(id: &str) -> mcp_store::McpServerRow {
        mcp_store::McpServerRow {
            id: id.to_owned(),
            name: format!("server-{id}"),
            kind: "stdio".to_owned(),
            command: Some("safe-command".to_owned()),
            args: vec!["--safe".to_owned()],
            env_plain: Vec::new(),
            env_secrets: Vec::new(),
            inherit_env: true,
            url: None,
            enabled: true,
        }
    }

    fn sample_mcp_tool(server_id: &str, id: &str, name: &str) -> mcp_store::McpToolRow {
        mcp_store::McpToolRow {
            id: id.to_owned(),
            server_id: server_id.to_owned(),
            name: name.to_owned(),
            description: None,
            input_schema_json: Some(r#"{"type":"object"}"#.to_owned()),
            trust_level: "unknown".to_owned(),
            schema_hash: Some(format!("hash-{name}")),
        }
    }

    fn committed<T>(result: ConnectorConfigCas<T>) -> (ConnectorConfigRevision, T) {
        match result {
            ConnectorConfigCas::Committed { revision, value } => (revision, value),
            ConnectorConfigCas::Stale { current_revision } => {
                panic!("unexpected stale revision: {current_revision:?}")
            }
        }
    }

    fn stage_slot(
        db: &Db,
        logical_id: &secret::LogicalCredentialId,
        slot: &secret::PhysicalSecretSlot,
    ) {
        db.register_physical_secret_slot_staging(logical_id.as_str(), slot.as_str())
            .unwrap();
    }

    fn insert_physical_credential(db: &Db, logical_id: &str) -> secret::PhysicalSecretSlot {
        let logical = secret::LogicalCredentialId::new(logical_id).unwrap();
        let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        stage_slot(db, &logical, &slot);
        db.insert_credential_with_secret_slot(&sample(logical.as_str()), slot.as_str(), None)
            .unwrap();
        slot
    }

    fn env_credential_json(entries: &[(String, String)]) -> String {
        let values = entries
            .iter()
            .map(|(key, value)| (key.clone(), serde_json::Value::String(value.clone())))
            .collect::<serde_json::Map<_, _>>();
        serde_json::Value::Object(values).to_string()
    }

    #[test]
    fn connector_config_revision은_모든_가시_mutation과_versioned_read를_추적한다() {
        let mut db = Db::open_in_memory().unwrap();
        let initial = db.connector_config_revision().unwrap();
        assert_eq!(initial, ConnectorConfigRevision::INITIAL);

        // Credentials are a global Connector invalidation source, including workspace/global rows.
        db.insert_credential(&sample("cred-global")).unwrap();
        let after_credential = db.connector_config_revision().unwrap();
        assert!(after_credential > initial);
        db.set_credential_oauth_json("cred-global", r#"{"server_id":"srv"}"#)
            .unwrap();
        let after_oauth = db.connector_config_revision().unwrap();
        assert!(after_oauth > after_credential);
        db.conn
            .execute(
                "UPDATE credentials SET last_used_at = '2026-07-22T00:00:00Z' WHERE id = ?1",
                ["cred-global"],
            )
            .unwrap();
        assert_eq!(db.connector_config_revision().unwrap(), after_oauth);

        let server = sample_mcp_server("srv");
        let (after_server, outcome) = committed(
            db.save_mcp_server_revision_cas(after_oauth, &server)
                .unwrap(),
        );
        assert_eq!(outcome, mcp_store::McpServerSaveOutcome::Inserted);
        assert!(after_server > after_oauth);
        let inventory = db.mcp_server_inventory_versioned(8).unwrap();
        assert_eq!(inventory.revision, after_server);
        assert_eq!(inventory.value.len(), 1);
        let selected = db.mcp_server_versioned("srv").unwrap();
        assert_eq!(selected.revision, after_server);
        assert_eq!(selected.value.unwrap().id, "srv");

        let tool = sample_mcp_tool("srv", "tool-id", "tool-name");
        let (after_tools, ()) = committed(
            db.replace_mcp_tools_revision_cas(after_server, "srv", &[tool])
                .unwrap(),
        );
        assert!(after_tools > after_server);
        let name = db.mcp_tool_name_versioned("srv", "tool-id").unwrap();
        assert_eq!(name.revision, after_tools);
        assert_eq!(name.value.as_deref(), Some("tool-name"));
        let page = db.mcp_tool_page_versioned("srv", 0, 8).unwrap();
        assert_eq!(page.revision, after_tools);
        assert_eq!(page.value.total, 1);

        let (after_permission, ()) = committed(
            db.set_permission_by_tool_id_revision_cas(
                after_tools,
                "srv",
                "tool-id",
                "allow",
                Some("hash-tool-name"),
            )
            .unwrap(),
        );
        assert!(after_permission > after_tools);
        let permission = db.permission_rule_versioned("srv", "tool-name").unwrap();
        assert_eq!(permission.revision, after_permission);
        assert_eq!(permission.value.unwrap().rule, "allow");
    }

    #[test]
    fn slack_reconnect_revision_cas는_stale을_거부하고_disabled_row를_원자적으로_enable한다() {
        let mut db = Db::open_in_memory().unwrap();
        let initial = db.connector_config_revision().unwrap();
        let mut slack = sample_mcp_server("slack-disabled");
        slack.kind = "http".to_owned();
        slack.command = None;
        slack.args.clear();
        slack.url = Some("https://mcp.slack.com/mcp/".to_owned());
        slack.enabled = false;
        let (disabled_revision, _) =
            committed(db.save_mcp_server_revision_cas(initial, &slack).unwrap());

        let mut reconnect = slack.clone();
        reconnect.id = "must-not-be-inserted".to_owned();
        reconnect.url = Some(" https://mcp.slack.com/mcp ".to_owned());
        reconnect.enabled = true;
        assert_eq!(
            db.ensure_enabled_mcp_server_by_url_revision_cas(initial, &reconnect)
                .unwrap(),
            ConnectorConfigCas::Stale {
                current_revision: disabled_revision,
            }
        );
        assert!(!db.mcp_server("slack-disabled").unwrap().unwrap().enabled);

        let (enabled_revision, enabled) = committed(
            db.ensure_enabled_mcp_server_by_url_revision_cas(disabled_revision, &reconnect)
                .unwrap(),
        );
        assert!(enabled_revision > disabled_revision);
        assert_eq!(enabled.id, "slack-disabled");
        assert!(enabled.enabled);
        let rows = db.list_mcp_servers().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "slack-disabled");
        assert!(rows[0].enabled);
    }

    #[test]
    fn slack_reconnect_revision_cas는_duplicate_canonical_rows를_변경_전에_fail_closed한다() {
        let mut db = Db::open_in_memory().unwrap();
        let mut first = sample_mcp_server("slack-duplicate-a");
        first.kind = "http".to_owned();
        first.command = None;
        first.args.clear();
        first.url = Some("https://mcp.slack.com/mcp/".to_owned());
        first.enabled = false;
        db.save_mcp_server(&first).unwrap();
        let mut second = first.clone();
        second.id = "slack-duplicate-b".to_owned();
        second.url = Some(" https://mcp.slack.com/mcp ".to_owned());
        db.save_mcp_server(&second).unwrap();
        let expected = db.connector_config_revision().unwrap();

        let mut reconnect = first.clone();
        reconnect.id = "must-not-be-inserted".to_owned();
        reconnect.enabled = true;
        assert!(
            db.ensure_enabled_mcp_server_by_url_revision_cas(expected, &reconnect)
                .is_err()
        );
        assert_eq!(db.connector_config_revision().unwrap(), expected);
        let rows = db.list_mcp_servers().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| !row.enabled));
        assert!(rows.iter().all(|row| row.id != reconnect.id));
    }

    #[test]
    fn connector_config_revision은_restart와_other_writer를_견디고_stale_writer를_거부한다() {
        let (dir, path, mut db_a) = file_db("connector-revision-stale");
        let mut db_b = Db::open(&path).unwrap();
        let expected = db_a.connector_config_revision().unwrap();
        let (committed_revision, _) = committed(
            db_a.save_mcp_server_revision_cas(expected, &sample_mcp_server("winner"))
                .unwrap(),
        );
        assert!(committed_revision > expected);

        let stale = db_b
            .save_mcp_server_revision_cas(expected, &sample_mcp_server("stale"))
            .unwrap();
        assert_eq!(
            stale,
            ConnectorConfigCas::Stale {
                current_revision: committed_revision
            }
        );
        assert!(db_b.mcp_server("stale").unwrap().is_none());
        drop(db_a);
        drop(db_b);

        let reopened = Db::open(&path).unwrap();
        assert_eq!(
            reopened.connector_config_revision().unwrap(),
            committed_revision
        );
        reopened
            .insert_mcp_server(&sample_mcp_server("other-writer"))
            .unwrap();
        assert!(
            reopened.connector_config_revision().unwrap() > committed_revision,
            "legacy/other-process writer must use the same trigger-backed revision"
        );
        drop(reopened);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn connector_config_revision_overflow와_injected_failure는_writer전체를_rollback한다() {
        let mut db = Db::open_in_memory().unwrap();
        db.conn
            .execute(
                "UPDATE connector_config_state SET revision = ?1 WHERE singleton = 1",
                [i64::MAX],
            )
            .unwrap();
        let maximum = db.connector_config_revision().unwrap();
        assert_eq!(maximum.get(), u64::try_from(i64::MAX).unwrap());
        assert!(
            db.save_mcp_server_revision_cas(maximum, &sample_mcp_server("overflow"))
                .is_err()
        );
        assert!(db.mcp_server("overflow").unwrap().is_none());
        assert_eq!(db.connector_config_revision().unwrap(), maximum);

        db.conn
            .execute(
                "UPDATE connector_config_state SET revision = 1 WHERE singleton = 1",
                [],
            )
            .unwrap();
        let reset = db.connector_config_revision().unwrap();
        db.conn
            .execute_batch(
                "CREATE TRIGGER fail_connector_revision
                 BEFORE UPDATE ON connector_config_state
                 BEGIN SELECT RAISE(ABORT, 'injected connector revision failure'); END;",
            )
            .unwrap();
        assert!(
            db.save_mcp_server_revision_cas(reset, &sample_mcp_server("injected"))
                .is_err()
        );
        assert!(db.mcp_server("injected").unwrap().is_none());
        assert_eq!(db.connector_config_revision().unwrap(), reset);
    }

    #[test]
    fn selected_server는_sql_byte_preflight에서_oversize를_거부한다() {
        let db = Db::open_in_memory().unwrap();
        let oversized = "x".repeat(mcp_store::MCP_SERVER_POINT_BYTES_MAX + 1);
        db.conn
            .execute(
                "INSERT INTO mcp_servers
                   (id, name, kind, command, args_json, env_json, env_credentials_json,
                    inherit_env, url, enabled, created_at, updated_at)
                 VALUES ('oversized', 'name', 'stdio', ?1, NULL, NULL, NULL, 1, NULL, 1,
                         'now', 'now')",
                [&oversized],
            )
            .unwrap();
        assert!(db.mcp_server_versioned("oversized").is_err());
    }

    #[test]
    fn credential_secret_location은_materialize전에_sql_byte_preflight를_강제한다() {
        let db = Db::open_in_memory().unwrap();
        db.insert_credential(&sample("oversized-location")).unwrap();
        let oversized = "x".repeat(CREDENTIAL_SECRET_LOCATION_BYTES_MAX + 1);
        db.conn
            .execute(
                "UPDATE credentials SET keyring_username = ?2 WHERE id = ?1",
                ("oversized-location", oversized),
            )
            .unwrap();

        assert!(
            db.credential_secret_location_versioned("oversized-location")
                .is_err()
        );
    }

    #[test]
    fn mcp_request_target_stdio는_credential_item과_aggregate_byte_exact_limit을_보존한다() {
        let db = Db::open_in_memory().unwrap();
        let logical_padding = "x".repeat(88);
        let mut env_secrets = Vec::with_capacity(MCP_REQUEST_TARGET_CREDENTIAL_LIMIT_MAX);
        let mut first_credential_id = None;
        let tx = db.conn.unchecked_transaction().unwrap();
        {
            let mut insert = tx
                .prepare_cached(
                    "INSERT INTO credentials
                       (id, provider, label, credential_kind, keyring_service, keyring_username,
                        masked_hint, created_at, updated_at)
                     VALUES (?1, 'oauth', 'bounded', 'oauth_token', ?2, ?3, NULL, 'now', 'now')",
                )
                .unwrap();
            for index in 0..MCP_REQUEST_TARGET_CREDENTIAL_LIMIT_MAX {
                let logical_id = format!("{index:04}-{logical_padding}");
                assert_eq!(logical_id.len(), 93);
                let logical = secret::LogicalCredentialId::new(logical_id.clone()).unwrap();
                let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
                assert_eq!(secret::KEYRING_SERVICE.len() + slot.as_str().len(), 256);
                insert
                    .execute((logical.as_str(), secret::KEYRING_SERVICE, slot.as_str()))
                    .unwrap();
                first_credential_id.get_or_insert_with(|| logical_id.clone());
                env_secrets.push((format!("KEY_{index:04}"), logical_id));
            }
        }
        tx.commit().unwrap();
        let mut server = sample_mcp_server("target-exact");
        server.args.clear();
        server.env_secrets = env_secrets;
        db.insert_mcp_server(&server).unwrap();

        let target = db.mcp_request_target_versioned("target-exact").unwrap();
        let returned_server = target.value.server.as_ref().unwrap();
        assert_eq!(
            target.value.credential_locations.len(),
            MCP_REQUEST_TARGET_CREDENTIAL_LIMIT_MAX
        );
        assert_eq!(
            target
                .value
                .credential_locations
                .iter()
                .map(|location| {
                    location.keyring_service.len() + location.keyring_username.len()
                })
                .sum::<usize>(),
            MCP_REQUEST_TARGET_CREDENTIAL_BYTES_MAX,
            "exact aggregate byte ceiling must be accepted"
        );
        for ((_, credential_id), location) in returned_server
            .env_secrets
            .iter()
            .zip(&target.value.credential_locations)
        {
            validate_owned_physical_secret_slot(credential_id, &location.keyring_username).unwrap();
        }

        let marker = "AGGREGATE_PLUS_ONE_MARKER";
        let invalid_service = format!("{}{marker}", secret::KEYRING_SERVICE);
        let first_credential_id = first_credential_id.unwrap();
        db.conn
            .execute(
                "UPDATE credentials SET keyring_service = ?2 WHERE id = ?1",
                (&first_credential_id, &invalid_service),
            )
            .unwrap();
        let error = db.mcp_request_target_versioned("target-exact").unwrap_err();
        let rendered = format!("{error:#}");
        assert!(rendered.contains("aggregate byte limit"));
        assert!(!rendered.contains(marker));

        db.conn
            .execute(
                "UPDATE credentials SET keyring_service = ?2 WHERE id = ?1",
                (&first_credential_id, secret::KEYRING_SERVICE),
            )
            .unwrap();
        let mut plus_one = returned_server.env_secrets.clone();
        plus_one.push(("KEY_PLUS_ONE".to_owned(), "missing-plus-one".to_owned()));
        db.conn
            .execute(
                "UPDATE mcp_servers SET env_credentials_json = ?2 WHERE id = ?1",
                ("target-exact", env_credential_json(&plus_one)),
            )
            .unwrap();
        let error = db.mcp_request_target_versioned("target-exact").unwrap_err();
        assert!(format!("{error:#}").contains("credential item limit"));
    }

    #[test]
    fn mcp_request_target_stdio는_server_env_order와_single_snapshot_revision을_보존한다() {
        let (dir, path, db_a) = file_db("mcp-request-target-snapshot");
        let credential_ids = ["cred-order-z", "cred-order-a", "cred-order-m"];
        let slots = credential_ids
            .iter()
            .map(|id| ((*id).to_owned(), insert_physical_credential(&db_a, id)))
            .collect::<std::collections::HashMap<_, _>>();
        let mut server = sample_mcp_server("snapshot-server");
        server.env_secrets = vec![
            ("Z_KEY".to_owned(), credential_ids[0].to_owned()),
            ("A_KEY".to_owned(), credential_ids[1].to_owned()),
            ("M_KEY".to_owned(), credential_ids[2].to_owned()),
        ];
        db_a.insert_mcp_server(&server).unwrap();

        let tx = db_a.conn.unchecked_transaction().unwrap();
        let stale_revision = Db::read_connector_config_revision(&tx).unwrap();
        let first = Db::mcp_request_target_in_snapshot(&tx, "snapshot-server").unwrap();
        let ordered_ids = first
            .server
            .as_ref()
            .unwrap()
            .env_secrets
            .iter()
            .map(|(_, id)| id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ordered_ids,
            vec!["cred-order-a", "cred-order-m", "cred-order-z"]
        );
        for ((_, credential_id), location) in first
            .server
            .as_ref()
            .unwrap()
            .env_secrets
            .iter()
            .zip(&first.credential_locations)
        {
            assert_eq!(
                location.keyring_username,
                slots.get(credential_id).unwrap().as_str()
            );
        }

        let db_b = Db::open(&path).unwrap();
        let logical = secret::LogicalCredentialId::new("cred-order-a").unwrap();
        let replacement = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        db_b.conn
            .execute(
                "UPDATE credentials SET keyring_username = ?2 WHERE id = ?1",
                (logical.as_str(), replacement.as_str()),
            )
            .unwrap();
        let current_revision = db_b.connector_config_revision().unwrap();
        assert!(current_revision > stale_revision);

        let stale_again = Db::mcp_request_target_in_snapshot(&tx, "snapshot-server").unwrap();
        assert_eq!(first, stale_again);
        assert_eq!(
            Db::read_connector_config_revision(&tx).unwrap(),
            stale_revision
        );
        tx.commit().unwrap();

        let fresh = db_a
            .mcp_request_target_versioned("snapshot-server")
            .unwrap();
        assert_eq!(fresh.revision, current_revision);
        let fresh_server = fresh.value.server.as_ref().unwrap();
        let position = fresh_server
            .env_secrets
            .iter()
            .position(|(_, id)| id == logical.as_str())
            .unwrap();
        assert_eq!(
            fresh.value.credential_locations[position].keyring_username,
            replacement.as_str()
        );
        drop(db_b);
        drop(db_a);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mcp_request_target_http는_zero_one_two_oauth_candidates를_같은_snapshot에서_반환한다() {
        let db = Db::open_in_memory().unwrap();
        let mut server = sample_mcp_server("http-target");
        server.kind = "http".to_owned();
        server.command = None;
        server.args.clear();
        server.url = Some("https://marker.example/mcp".to_owned());
        db.insert_mcp_server(&server).unwrap();

        let zero = db.mcp_request_target_versioned("http-target").unwrap();
        assert!(zero.value.credential_locations.is_empty());
        assert!(zero.value.oauth_bindings.is_empty());

        for logical_id in ["oauth-target-a", "oauth-target-b"] {
            let logical = secret::LogicalCredentialId::new(logical_id).unwrap();
            let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
            stage_slot(&db, &logical, &slot);
            db.insert_credential_with_secret_slot(
                &sample(logical.as_str()),
                slot.as_str(),
                Some(r#"{"server_id":"http-target","provider_marker":"HIDDEN_OAUTH_MARKER"}"#),
            )
            .unwrap();
            let target = db.mcp_request_target_versioned("http-target").unwrap();
            assert_eq!(
                target.value.oauth_bindings.len(),
                if logical_id.ends_with('a') { 1 } else { 2 }
            );
            assert!(target.value.credential_locations.is_empty());
            assert_eq!(target.revision, db.connector_config_revision().unwrap());
        }

        let target = db.mcp_request_target_versioned("http-target").unwrap();
        let debug = format!("{:?}", target.value);
        assert!(debug.contains("transport: \"http\""));
        for marker in [
            "http-target",
            "marker.example",
            "oauth-target-a",
            "oauth-target-b",
            "HIDDEN_OAUTH_MARKER",
            "deppy.oauth.v1",
        ] {
            assert!(!debug.contains(marker));
        }
    }

    #[test]
    fn mcp_request_target는_malformed_duplicate_cross_shape와_coordinate를_fail_closed한다() {
        let db = Db::open_in_memory().unwrap();
        let slot = insert_physical_credential(&db, "coordinate-marker-credential");
        let mut server = sample_mcp_server("error-marker-server");
        server.env_secrets = vec![(
            "TOKEN".to_owned(),
            "coordinate-marker-credential".to_owned(),
        )];
        db.insert_mcp_server(&server).unwrap();

        db.conn
            .execute(
                "UPDATE mcp_servers SET env_credentials_json = ?2 WHERE id = ?1",
                (
                    "error-marker-server",
                    r#"{"TOKEN":"ERROR_VALUE_MARKER","TOKEN":"duplicate"}"#,
                ),
            )
            .unwrap();
        let duplicate = db
            .mcp_request_target_versioned("error-marker-server")
            .unwrap_err();
        assert!(format!("{duplicate:#}").contains("malformed or duplicate"));
        assert!(!format!("{duplicate:#}").contains("ERROR_VALUE_MARKER"));

        db.conn
            .execute(
                "UPDATE mcp_servers SET env_credentials_json = ?2 WHERE id = ?1",
                (
                    "error-marker-server",
                    r#"{"TOKEN":"BROKEN_JSON_ERROR_MARKER""#,
                ),
            )
            .unwrap();
        let malformed = db
            .mcp_request_target_versioned("error-marker-server")
            .unwrap_err();
        assert!(!format!("{malformed:#}").contains("BROKEN_JSON_ERROR_MARKER"));

        db.conn
            .execute(
                "UPDATE mcp_servers
                    SET kind = 'http', command = 'CROSS_SHAPE_COMMAND_MARKER', args_json = NULL,
                        env_json = NULL, env_credentials_json = NULL,
                        url = 'https://example.invalid/mcp'
                  WHERE id = ?1",
                ["error-marker-server"],
            )
            .unwrap();
        let cross_shape = db
            .mcp_request_target_versioned("error-marker-server")
            .unwrap_err();
        assert!(format!("{cross_shape:#}").contains("transport shape"));
        assert!(!format!("{cross_shape:#}").contains("CROSS_SHAPE_COMMAND_MARKER"));

        db.conn
            .execute(
                "UPDATE mcp_servers
                    SET kind = 'stdio', command = 'safe-command', url = NULL,
                        env_credentials_json = ?2
                  WHERE id = ?1",
                (
                    "error-marker-server",
                    env_credential_json(&[(
                        "TOKEN".to_owned(),
                        "coordinate-marker-credential".to_owned(),
                    )]),
                ),
            )
            .unwrap();
        db.conn
            .execute(
                "UPDATE credentials SET keyring_service = 'RAW_SERVICE_MARKER' WHERE id = ?1",
                ["coordinate-marker-credential"],
            )
            .unwrap();
        let coordinate = db
            .mcp_request_target_versioned("error-marker-server")
            .unwrap_err();
        assert!(!format!("{coordinate:#}").contains("RAW_SERVICE_MARKER"));
        db.conn
            .execute(
                "UPDATE credentials SET keyring_service = ?2, keyring_username = ?3 WHERE id = ?1",
                (
                    "coordinate-marker-credential",
                    secret::KEYRING_SERVICE,
                    slot.as_str(),
                ),
            )
            .unwrap();
        assert!(
            db.mcp_request_target_versioned("error-marker-server")
                .is_ok(),
            "failed read transaction must roll back and release the connection"
        );

        let location = CredentialSecretLocation {
            keyring_service: "RAW_SERVICE_DEBUG_MARKER".to_owned(),
            keyring_username: "RAW_USERNAME_DEBUG_MARKER".to_owned(),
        };
        let debug = format!("{location:?}");
        assert!(debug.contains("REDACTED"));
        assert!(!debug.contains("RAW_SERVICE_DEBUG_MARKER"));
        assert!(!debug.contains("RAW_USERNAME_DEBUG_MARKER"));
    }

    #[test]
    fn permission_tool_id_lookup과_write는_한_revision_cas로_직렬화된다() {
        let (dir, path, mut db_a) = file_db("connector-permission-stale-tool");
        db_a.insert_mcp_server(&sample_mcp_server("server"))
            .unwrap();
        db_a.replace_mcp_tools("server", &[sample_mcp_tool("server", "tool", "old-name")])
            .unwrap();
        let stale_revision = db_a.connector_config_revision().unwrap();

        let mut db_b = Db::open(&path).unwrap();
        let (current_revision, ()) = committed(
            db_b.replace_mcp_tools_revision_cas(
                stale_revision,
                "server",
                &[sample_mcp_tool("server", "tool", "new-name")],
            )
            .unwrap(),
        );
        let result = db_a
            .set_permission_by_tool_id_revision_cas(
                stale_revision,
                "server",
                "tool",
                "allow",
                Some("hash-old-name"),
            )
            .unwrap();
        assert_eq!(result, ConnectorConfigCas::Stale { current_revision });
        assert!(
            db_a.permission_rule("server", "old-name")
                .unwrap()
                .is_none()
        );
        assert!(
            db_a.permission_rule("server", "new-name")
                .unwrap()
                .is_none()
        );

        drop(db_a);
        drop(db_b);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn oauth_pointer_publish와_auth_reads는_같은_durable_revision을_공유한다() {
        let mut db = Db::open_in_memory().unwrap();
        let logical = secret::LogicalCredentialId::new("oauth-revision").unwrap();
        db.insert_credential(&sample(logical.as_str())).unwrap();
        let before = db.connector_config_revision().unwrap();
        let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        let oauth_json = r#"{"server_id":"server"}"#;
        stage_slot(&db, &logical, &slot);

        let (published, changed) = committed(
            db.publish_credential_secret_slot_revision_cas(
                before,
                logical.as_str(),
                logical.as_str(),
                slot.as_str(),
                Some(oauth_json),
                Some("masked"),
            )
            .unwrap(),
        );
        assert!(changed);
        assert!(published > before);
        let location = db
            .credential_secret_location_versioned(logical.as_str())
            .unwrap();
        assert_eq!(location.revision, published);
        assert_eq!(location.value.unwrap().keyring_username, slot.as_str());
        let binding = db
            .credential_oauth_bindings_for_server_versioned("server")
            .unwrap();
        assert_eq!(binding.revision, published);
        assert_eq!(binding.value.len(), 1);
        assert_eq!(binding.value[0].physical_pointer, slot.as_str());

        let stale_candidate =
            secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        stage_slot(&db, &logical, &stale_candidate);
        let (unchanged, changed) = committed(
            db.publish_credential_secret_slot_revision_cas(
                published,
                logical.as_str(),
                "stale-pointer",
                stale_candidate.as_str(),
                Some(oauth_json),
                Some("masked"),
            )
            .unwrap(),
        );
        assert!(!changed);
        assert_eq!(unchanged, published);
    }

    fn credential_secret_record(db: &Db, id: &str) -> CredentialSecretRecord {
        db.list_credential_secret_records(32)
            .unwrap()
            .into_iter()
            .find(|record| record.meta.id == id)
            .unwrap()
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
                workspace_id: None,
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
    fn credential_env_연결은_워크스페이스별로_저장되고_사용중인_키는_삭제할수없다() {
        let db = Db::open_in_memory().unwrap();
        let workspace = db.create_workspace("credential-env").unwrap();
        db.insert_credential(&sample("env-key")).unwrap();
        let inserted = db.conn.execute(
            "INSERT INTO workspace_credential_env(workspace_id, env_name, credential_id) VALUES (?1, 'SERVICE_TOKEN', 'env-key')",
            [&workspace],
        );
        assert!(inserted.is_ok(), "환경 연결을 영속할 테이블이 필요하다");
        assert!(db.credential_in_use("env-key").unwrap());
        assert!(!db.delete_credential_if_unused("env-key").unwrap());
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
    fn credential_expected_pointer_delete는_stale과_reference를_noop처리하고_failure를_rollback한다()
     {
        let db = Db::open_in_memory().unwrap();
        let logical = secret::LogicalCredentialId::new("cred-delete-cas").unwrap();
        let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        stage_slot(&db, &logical, &slot);
        db.insert_credential_with_secret_slot(&sample(logical.as_str()), slot.as_str(), None)
            .unwrap();

        assert!(
            !db.delete_credential_if_unused_cas(logical.as_str(), "stale-pointer")
                .unwrap()
        );
        let workspace = db.ensure_default_workspace().unwrap();
        let profile = db.insert_env_profile(&workspace, "local", "local").unwrap();
        db.upsert_env_var(
            &profile,
            "TOKEN",
            &EnvValue::Secret {
                credential_id: logical.as_str().to_owned(),
            },
        )
        .unwrap();
        assert!(
            !db.delete_credential_if_unused_cas(logical.as_str(), slot.as_str())
                .unwrap()
        );
        db.delete_env_var(&profile, "TOKEN").unwrap();
        db.conn
            .execute_batch(
                "CREATE TRIGGER fail_credential_expected_delete BEFORE DELETE ON credentials
                 BEGIN SELECT RAISE(ABORT, 'injected credential delete failure'); END;",
            )
            .unwrap();
        assert!(
            db.delete_credential_if_unused_cas(logical.as_str(), slot.as_str())
                .is_err()
        );
        assert_eq!(
            credential_secret_record(&db, logical.as_str()).keyring_username,
            slot.as_str()
        );
        db.conn
            .execute_batch("DROP TRIGGER fail_credential_expected_delete;")
            .unwrap();
        assert!(
            db.delete_credential_if_unused_cas(logical.as_str(), slot.as_str())
                .unwrap()
        );
        assert!(db.list_credential_secret_records(1).unwrap().is_empty());
    }

    #[test]
    fn credential_expected_pointer_delete는_mcp_secret_reference도_거부한다() {
        let db = Db::open_in_memory().unwrap();
        let logical = secret::LogicalCredentialId::new("cred-delete-mcp").unwrap();
        let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        stage_slot(&db, &logical, &slot);
        db.insert_credential_with_secret_slot(&sample(logical.as_str()), slot.as_str(), None)
            .unwrap();
        let mut server = sample_mcp_server("srv-delete-ref");
        server.env_secrets = vec![("TOKEN".to_owned(), logical.as_str().to_owned())];
        db.insert_mcp_server(&server).unwrap();

        assert!(
            !db.delete_credential_if_unused_cas(logical.as_str(), slot.as_str())
                .unwrap()
        );
        assert_eq!(db.list_credential_secret_records(1).unwrap().len(), 1);
    }

    #[test]
    fn credential_oauth_메타는_json으로_왕복된다() {
        // PR-H5 (v20): oauth_json은 비밀 아닌 연계 메타데이터만 담는다
        let db = Db::open_in_memory().unwrap();
        db.insert_credential(&sample("cred-oauth")).unwrap();
        db.insert_credential(&sample("cred-plain")).unwrap();

        assert!(db.list_credential_oauth_json().unwrap().is_empty());
        db.set_credential_oauth_json("cred-oauth", r#"{"server_id":"srv-1"}"#)
            .unwrap();
        let rows = db.list_credential_oauth_json().unwrap();
        assert_eq!(
            rows,
            vec![(
                "cred-oauth".to_owned(),
                r#"{"server_id":"srv-1"}"#.to_owned()
            )]
        );
        // 갱신은 마지막 값으로 대체
        db.set_credential_oauth_json("cred-oauth", r#"{"server_id":"srv-2"}"#)
            .unwrap();
        assert_eq!(
            db.list_credential_oauth_json().unwrap()[0].1,
            r#"{"server_id":"srv-2"}"#
        );
        // 없는 credential은 에러
        assert!(db.set_credential_oauth_json("cred-missing", "{}").is_err());
    }

    #[test]
    fn oauth_server_binding_lookup은_exact_zero와_one을_구분하고_raw_metadata를_보존한다() {
        let db = Db::open_in_memory().unwrap();
        let other = secret::LogicalCredentialId::new("cred-binding-other").unwrap();
        let other_slot = secret::PhysicalSecretSlot::with_version(&other, uuid::Uuid::new_v4());
        stage_slot(&db, &other, &other_slot);
        db.insert_credential_with_secret_slot(
            &sample(other.as_str()),
            other_slot.as_str(),
            Some(r#"{"server_id":"server-other"}"#),
        )
        .unwrap();
        assert!(
            db.credential_oauth_bindings_for_server("server-target")
                .unwrap()
                .is_empty()
        );

        let logical = secret::LogicalCredentialId::new("cred-binding-target").unwrap();
        let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        stage_slot(&db, &logical, &slot);
        let metadata = r#"{"server_id":"server-target","server_url":"https://stored.example/mcp","token_endpoint_auth_method":"client_secret_post"}"#;
        db.insert_credential_with_secret_slot(
            &sample(logical.as_str()),
            slot.as_str(),
            Some(metadata),
        )
        .unwrap();

        let records = db
            .credential_oauth_bindings_for_server("server-target")
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].logical_id, logical.as_str());
        assert_eq!(records[0].keyring_service, secret::KEYRING_SERVICE);
        assert_eq!(records[0].physical_pointer, slot.as_str());
        assert_eq!(records[0].oauth_metadata_json, metadata);
        let debug = format!("{:?}", records[0]);
        assert!(!debug.contains(logical.as_str()));
        assert!(!debug.contains(slot.as_str()));
        assert!(!debug.contains("stored.example"));
        assert!(debug.contains("REDACTED"));
    }

    #[test]
    fn oauth_server_binding_lookup은_duplicate를_limit_two로_노출한다() {
        let db = Db::open_in_memory().unwrap();
        for id in ["cred-binding-a", "cred-binding-b", "cred-binding-c"] {
            let logical = secret::LogicalCredentialId::new(id).unwrap();
            let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
            stage_slot(&db, &logical, &slot);
            db.insert_credential_with_secret_slot(
                &sample(logical.as_str()),
                slot.as_str(),
                Some(r#"{"server_id":"server-duplicate"}"#),
            )
            .unwrap();
        }

        let records = db
            .credential_oauth_bindings_for_server("server-duplicate")
            .unwrap();
        assert_eq!(
            records.len(),
            2,
            "+1 probe must expose ambiguity without full allocation"
        );
    }

    #[test]
    fn oauth_server_binding_lookup은_corrupt_json을_fail_closed한다() {
        let db = Db::open_in_memory().unwrap();
        db.insert_credential(&sample("cred-binding-corrupt"))
            .unwrap();
        db.conn
            .execute(
                "UPDATE credentials SET oauth_json = '{\"server_id\":' WHERE id = ?1",
                ["cred-binding-corrupt"],
            )
            .unwrap();

        assert!(
            db.credential_oauth_bindings_for_server("server-target")
                .is_err()
        );
    }

    #[test]
    fn oauth_server_binding_lookup은_sql_byte_preflight를_강제한다() {
        let db = Db::open_in_memory().unwrap();
        db.insert_credential(&sample("cred-binding-oversized"))
            .unwrap();
        let padding = "x".repeat(CREDENTIAL_OAUTH_BINDING_BYTES_MAX + 1);
        let metadata = serde_json::json!({
            "server_id": "server-oversized",
            "nonsecret_padding": padding,
        })
        .to_string();
        assert!(
            db.set_credential_oauth_json("cred-binding-oversized", &metadata)
                .is_err(),
            "legacy metadata write must enforce the point-read byte ceiling"
        );
        assert!(db.list_credential_oauth_json().unwrap().is_empty());
        db.conn
            .execute(
                "UPDATE credentials SET oauth_json = ?2 WHERE id = ?1",
                ("cred-binding-oversized", &metadata),
            )
            .unwrap();

        assert!(
            db.credential_oauth_bindings_for_server("server-oversized")
                .is_err()
        );
    }

    #[test]
    fn oauth_metadata_json은_exact_byte_limit을_accept하고_plus_one을_preparse_reject한다() {
        fn metadata_with_len(target: usize) -> String {
            const PREFIX: &str = "{\"padding\":\"";
            const SUFFIX: &str = "\"}";
            format!(
                "{PREFIX}{}{SUFFIX}",
                "x".repeat(target - PREFIX.len() - SUFFIX.len())
            )
        }

        let exact = metadata_with_len(CREDENTIAL_OAUTH_BINDING_BYTES_MAX);
        assert_eq!(exact.len(), CREDENTIAL_OAUTH_BINDING_BYTES_MAX);
        validate_oauth_metadata_json(&exact).unwrap();

        let plus_one = metadata_with_len(CREDENTIAL_OAUTH_BINDING_BYTES_MAX + 1);
        assert_eq!(plus_one.len(), CREDENTIAL_OAUTH_BINDING_BYTES_MAX + 1);
        assert!(validate_oauth_metadata_json(&plus_one).is_err());
    }

    #[test]
    fn credential_secret_record_scan은_inclusive_limit과_full_pointer를_보존한다() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.list_credential_secret_records(0).unwrap().is_empty());
        db.insert_credential(&sample("cred-a")).unwrap();
        db.insert_credential(&sample("cred-b")).unwrap();
        db.set_credential_oauth_json("cred-a", r#"{"server_id":"srv-a"}"#)
            .unwrap();

        let records = db.list_credential_secret_records(2).unwrap();
        assert_eq!(records.len(), 2, "exactly-at-limit must be accepted");
        assert_eq!(records[0].meta, sample("cred-a"));
        assert_eq!(records[0].keyring_service, secret::KEYRING_SERVICE);
        assert_eq!(records[0].keyring_username, "cred-a");
        assert_eq!(
            records[0].oauth_json.as_deref(),
            Some(r#"{"server_id":"srv-a"}"#)
        );
        assert_eq!(records[1].meta, sample("cred-b"));
        assert_eq!(records[1].keyring_username, "cred-b");
        assert_eq!(records[1].oauth_json, None);
        assert!(
            db.list_credential_secret_records(1).is_err(),
            "the +1 probe must reject a truncated bootstrap scan"
        );
        assert!(db.list_credential_secret_records(usize::MAX).is_err());
    }

    #[test]
    fn credential_secret_record_scan은_sql_byte_preflight와_corrupt_text를_fail_closed한다() {
        let db = Db::open_in_memory().unwrap();
        db.insert_credential(&sample("cred-record-oversized"))
            .unwrap();
        db.conn
            .execute(
                "UPDATE credentials SET label = ?2 WHERE id = ?1",
                (
                    "cred-record-oversized",
                    "x".repeat(CREDENTIAL_SECRET_RECORD_BYTES_MAX + 1),
                ),
            )
            .unwrap();
        assert!(
            db.list_credential_secret_records(1).is_err(),
            "oversized legacy row must fail before String materialization"
        );

        let db = Db::open_in_memory().unwrap();
        db.insert_credential(&sample("cred-record-corrupt"))
            .unwrap();
        db.conn
            .execute(
                "UPDATE credentials SET label = ?2 WHERE id = ?1",
                (
                    "cred-record-corrupt",
                    rusqlite::types::Value::Blob(vec![0x80]),
                ),
            )
            .unwrap();
        assert!(
            db.list_credential_secret_records(1).is_err(),
            "non-TEXT legacy metadata must fail closed"
        );
    }

    #[test]
    fn physical_slot_credential_insert는_owned_pointer와_nullable_metadata만_publish한다() {
        let db = Db::open_in_memory().unwrap();
        let plain = secret::LogicalCredentialId::new("cred-plain").unwrap();
        let plain_slot = secret::PhysicalSecretSlot::with_version(&plain, uuid::Uuid::new_v4());
        stage_slot(&db, &plain, &plain_slot);
        db.insert_credential_with_secret_slot(&sample(plain.as_str()), plain_slot.as_str(), None)
            .unwrap();
        let plain_record = credential_secret_record(&db, plain.as_str());
        assert_eq!(plain_record.keyring_service, secret::KEYRING_SERVICE);
        assert_eq!(plain_record.keyring_username, plain_slot.as_str());
        assert_ne!(plain_record.keyring_username, plain.as_str());
        assert_eq!(plain_record.oauth_json, None);

        let oauth = secret::LogicalCredentialId::new("cred-oauth-new").unwrap();
        let oauth_slot = secret::PhysicalSecretSlot::with_version(&oauth, uuid::Uuid::new_v4());
        stage_slot(&db, &oauth, &oauth_slot);
        db.insert_credential_with_secret_slot(
            &sample(oauth.as_str()),
            oauth_slot.as_str(),
            Some(r#"{"server_id":"srv-1","client_id":"cid-1"}"#),
        )
        .unwrap();
        let oauth_record = credential_secret_record(&db, oauth.as_str());
        assert_eq!(oauth_record.keyring_username, oauth_slot.as_str());
        assert_eq!(
            oauth_record.oauth_json.as_deref(),
            Some(r#"{"server_id":"srv-1","client_id":"cid-1"}"#)
        );
    }

    #[test]
    fn secret_recovery_generation_rejects_aba_and_newly_published_candidates() {
        let db = Db::open_in_memory().unwrap();
        let logical = secret::LogicalCredentialId::new("recovery-aba").unwrap();
        let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        stage_slot(&db, &logical, &slot);
        let old = db
            .physical_secret_slots_for_reconciliation(8)
            .unwrap()
            .remove(0);
        db.acknowledge_physical_secret_slot_deleted(logical.as_str(), slot.as_str())
            .unwrap();
        stage_slot(&db, &logical, &slot);
        let current = db
            .physical_secret_slots_for_reconciliation(8)
            .unwrap()
            .remove(0);
        assert_ne!(old.recovery_generation, current.recovery_generation);
        assert!(
            !db.recover_physical_secret_slot_cas(&old, || panic!(
                "재생성된 행의 비밀을 삭제하면 안 된다"
            ))
            .unwrap()
        );
        db.insert_credential_with_secret_slot(&sample(logical.as_str()), slot.as_str(), None)
            .unwrap();
        assert!(
            !db.recover_physical_secret_slot_cas(&current, || panic!(
                "승인된 physical 슬롯을 삭제하면 안 된다"
            ))
            .unwrap()
        );
    }

    #[test]
    fn secret_recovery_callback_failure_rolls_back_and_can_retry() {
        let db = Db::open_in_memory().unwrap();
        let logical = secret::LogicalCredentialId::new("recovery-retry").unwrap();
        let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        stage_slot(&db, &logical, &slot);
        let candidate = db
            .physical_secret_slots_for_reconciliation(8)
            .unwrap()
            .remove(0);
        assert!(
            db.recover_physical_secret_slot_cas(&candidate, || anyhow::bail!("test-denied"))
                .is_err()
        );
        assert_eq!(
            db.physical_secret_slots_for_reconciliation(8).unwrap(),
            vec![candidate.clone()]
        );
        assert!(
            db.recover_physical_secret_slot_cas(&candidate, || Ok(()))
                .unwrap()
        );
        assert!(
            !db.recover_physical_secret_slot_cas(&candidate, || panic!("이미 정리된 후보"))
                .unwrap()
        );
    }

    #[test]
    fn secret_recovery_v39_backfill_preserves_slots_and_generation_on_reopen() {
        let (dir, _, initial) = file_db("secret-recovery-backfill");
        drop(initial);
        let path = dir.join("legacy.sqlite3");
        let legacy = storage_core::open_with_migrations(&path, &MIGRATIONS[..39]).unwrap();
        assert_eq!(storage_core::read_user_version(&legacy).unwrap(), 39);
        let logical = secret::LogicalCredentialId::new("recovery-backfill").unwrap();
        let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        legacy.execute("INSERT INTO physical_secret_slot_ledger (physical_slot,logical_credential_id,state,created_at,updated_at) VALUES (?1,?2,'staging',1,1)", (slot.as_str(),logical.as_str())).unwrap();
        drop(legacy);
        let migrated = Db::open(&path).unwrap();
        assert_eq!(
            Db::read_user_version(&migrated.conn).unwrap(),
            MIGRATIONS.len()
        );
        let rows = migrated
            .physical_secret_slots_for_reconciliation(8)
            .unwrap();
        assert_eq!(rows[0].physical_slot, slot.as_str());
        assert_eq!(rows[0].state, PhysicalSecretSlotState::Staging);
        assert_ne!(rows[0].recovery_generation, [0; 16]);
        drop(migrated);
        let reopened = Db::open(&path).unwrap();
        assert_eq!(
            Db::read_user_version(&reopened.conn).unwrap(),
            MIGRATIONS.len()
        );
        assert_eq!(
            rows,
            reopened
                .physical_secret_slots_for_reconciliation(8)
                .unwrap()
        );
        drop(reopened);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn keychain개발판_v38은_canonical_relay_v38로_오인하지_않는다() {
        let (dir, _, initial) = file_db("forked-secret-v38");
        drop(initial);
        let path = dir.join("forked-v38.sqlite3");
        let forked = storage_core::open_with_migrations(&path, &MIGRATIONS[..37]).unwrap();
        forked
            .execute_batch(
                "ALTER TABLE physical_secret_slot_ledger ADD COLUMN recovery_generation BLOB NOT NULL
                    DEFAULT X'00000000000000000000000000000000'
                    CHECK(typeof(recovery_generation) = 'blob' AND length(recovery_generation) = 16);
                 UPDATE physical_secret_slot_ledger SET recovery_generation = randomblob(16);
                 PRAGMA user_version = 38;",
            )
            .unwrap();
        drop(forked);

        let error = Db::open(&path).err().expect("forked v38은 거부해야 한다");
        assert!(
            error.to_string().contains("지원하지 않는 개발용 v38"),
            "예상하지 못한 오류: {error}"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn physical_slot_ledger는_crash_windows_restart와_exact_ack를_보존한다() {
        let (dir, path, db) = file_db("physical-slot-ledger-restart");
        let logical = secret::LogicalCredentialId::new("ledger-restart").unwrap();
        let missing = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        let initial_revision = db.connector_config_revision().unwrap();

        // Crash before or after the keyring write has the same durable staging identity. An exact
        // missing-keyring acknowledgement removes it without invalidating Connector snapshots.
        stage_slot(&db, &logical, &missing);
        assert_eq!(db.connector_config_revision().unwrap(), initial_revision);
        drop(db);
        let db = Db::open(&path).unwrap();
        assert_eq!(
            db.physical_secret_slots_for_reconciliation(1).unwrap(),
            vec![PhysicalSecretSlotLedgerRow {
                recovery_generation: db.physical_secret_slots_for_reconciliation(1).unwrap()[0]
                    .recovery_generation,
                logical_credential_id: logical.as_str().to_owned(),
                physical_slot: missing.as_str().to_owned(),
                state: PhysicalSecretSlotState::Staging,
                legacy_cleanup_username: None,
            }]
        );
        assert!(
            db.acknowledge_physical_secret_slot_deleted(logical.as_str(), missing.as_str())
                .unwrap()
        );
        assert_eq!(db.connector_config_revision().unwrap(), initial_revision);

        let first = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        stage_slot(&db, &logical, &first);
        db.insert_credential_with_secret_slot(&sample(logical.as_str()), first.as_str(), None)
            .unwrap();
        let after_first_publish = db.connector_config_revision().unwrap();
        assert!(after_first_publish > initial_revision);
        assert!(
            db.acknowledge_physical_secret_slot_deleted(logical.as_str(), first.as_str())
                .is_err(),
            "missing live keyring capability must remain published/fail-closed"
        );

        let second = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        stage_slot(&db, &logical, &second);
        db.rotate_credential_secret_slot(
            logical.as_str(),
            second.as_str(),
            r#"{"server_id":"server"}"#,
            None,
        )
        .unwrap();
        let after_rotation = db.connector_config_revision().unwrap();
        assert!(after_rotation > after_first_publish);
        drop(db);

        let db = Db::open(&path).unwrap();
        let rows = db.physical_secret_slots_for_reconciliation(2).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter()
                .find(|row| row.physical_slot == first.as_str())
                .unwrap()
                .state,
            PhysicalSecretSlotState::Orphan
        );
        assert_eq!(
            rows.iter()
                .find(|row| row.physical_slot == second.as_str())
                .unwrap()
                .state,
            PhysicalSecretSlotState::Published
        );
        assert!(
            db.acknowledge_physical_secret_slot_deleted(logical.as_str(), first.as_str())
                .unwrap()
        );
        assert!(
            !db.acknowledge_physical_secret_slot_deleted(logical.as_str(), first.as_str())
                .unwrap()
        );
        assert_eq!(db.connector_config_revision().unwrap(), after_rotation);
        assert!(
            db.delete_credential_if_unused_cas(logical.as_str(), second.as_str())
                .unwrap()
        );
        let after_delete = db.connector_config_revision().unwrap();
        assert!(after_delete > after_rotation);
        assert_eq!(
            db.physical_secret_slots_for_reconciliation(1).unwrap()[0].state,
            PhysicalSecretSlotState::Orphan
        );
        assert!(
            db.acknowledge_physical_secret_slot_deleted(logical.as_str(), second.as_str())
                .unwrap()
        );
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_cleanup_marker는_publish와_atomic하고_crash후_exact_order로_수렴한다() {
        let (dir, path, db) = file_db("legacy-cleanup-crash");
        let logical = secret::LogicalCredentialId::new("legacy-cleanup-crash").unwrap();
        let first = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        db.insert_credential(&sample(logical.as_str())).unwrap();
        stage_slot(&db, &logical, &first);

        let staged = db.physical_secret_slots_for_reconciliation(1).unwrap();
        assert_eq!(staged[0].state, PhysicalSecretSlotState::Staging);
        assert_eq!(staged[0].legacy_cleanup_username, None);
        assert_eq!(
            credential_secret_record(&db, logical.as_str()).keyring_username,
            logical.as_str()
        );
        drop(db);

        let db = Db::open(&path).unwrap();
        assert!(
            db.publish_legacy_credential_secret_slot_cas(
                logical.as_str(),
                logical.as_str(),
                first.as_str(),
                Some(r#"{"server_id":"legacy"}"#),
                Some("…1234"),
            )
            .unwrap()
        );
        drop(db);

        let db = Db::open(&path).unwrap();
        assert_eq!(
            credential_secret_record(&db, logical.as_str()).keyring_username,
            first.as_str()
        );
        let published = db.physical_secret_slots_for_reconciliation(1).unwrap();
        assert_eq!(published[0].state, PhysicalSecretSlotState::Published);
        assert_eq!(
            published[0].legacy_cleanup_username.as_deref(),
            Some(logical.as_str())
        );
        let debug = format!("{:?}", published[0]);
        assert!(!debug.contains(logical.as_str()));
        assert!(!debug.contains(first.as_str()));

        let second = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        stage_slot(&db, &logical, &second);
        db.rotate_credential_secret_slot(
            logical.as_str(),
            second.as_str(),
            r#"{"server_id":"rotated"}"#,
            None,
        )
        .unwrap();
        let rows = db.physical_secret_slots_for_reconciliation(2).unwrap();
        let old = rows
            .iter()
            .find(|row| row.physical_slot == first.as_str())
            .unwrap();
        let live = rows
            .iter()
            .find(|row| row.physical_slot == second.as_str())
            .unwrap();
        assert_eq!(old.state, PhysicalSecretSlotState::Orphan);
        assert_eq!(
            old.legacy_cleanup_username.as_deref(),
            Some(logical.as_str())
        );
        assert_eq!(live.state, PhysicalSecretSlotState::Published);
        assert_eq!(live.legacy_cleanup_username, None);
        assert!(
            db.acknowledge_physical_secret_slot_deleted(logical.as_str(), first.as_str())
                .is_err(),
            "legacy access/refresh/dcr source cleanup must be acknowledged first"
        );

        let mut legacy_sources = std::collections::BTreeSet::from([
            logical.as_str().to_owned(),
            format!("{}.refresh", logical.as_str()),
            format!("{}.dcr", logical.as_str()),
        ]);
        for exact in [
            logical.as_str().to_owned(),
            format!("{}.refresh", logical.as_str()),
            format!("{}.dcr", logical.as_str()),
        ] {
            assert!(legacy_sources.remove(&exact));
        }
        assert!(legacy_sources.is_empty());
        assert!(
            db.acknowledge_legacy_secret_source_deleted(
                logical.as_str(),
                first.as_str(),
                logical.as_str(),
            )
            .unwrap()
        );
        assert!(
            !db.acknowledge_legacy_secret_source_deleted(
                logical.as_str(),
                first.as_str(),
                logical.as_str(),
            )
            .unwrap()
        );
        assert!(
            db.acknowledge_physical_secret_slot_deleted(logical.as_str(), first.as_str())
                .unwrap()
        );
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_cleanup_marker_failpoint는_pointer_ledger_ack를_rollback한다() {
        let db = Db::open_in_memory().unwrap();
        let logical = secret::LogicalCredentialId::new("legacy-cleanup-failpoint").unwrap();
        let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        db.insert_credential(&sample(logical.as_str())).unwrap();
        stage_slot(&db, &logical, &slot);
        let before = db.connector_config_revision().unwrap();
        db.conn
            .execute_batch(
                "CREATE TRIGGER fail_legacy_cleanup_publish
                 BEFORE UPDATE OF legacy_cleanup_username ON physical_secret_slot_ledger
                 WHEN NEW.legacy_cleanup_username IS NOT NULL
                 BEGIN SELECT RAISE(ABORT, 'injected legacy marker failure'); END;",
            )
            .unwrap();
        assert!(
            db.publish_legacy_credential_secret_slot_cas(
                logical.as_str(),
                logical.as_str(),
                slot.as_str(),
                None,
                None,
            )
            .is_err()
        );
        assert_eq!(db.connector_config_revision().unwrap(), before);
        assert_eq!(
            credential_secret_record(&db, logical.as_str()).keyring_username,
            logical.as_str()
        );
        let staged = db.physical_secret_slots_for_reconciliation(1).unwrap();
        assert_eq!(staged[0].state, PhysicalSecretSlotState::Staging);
        assert_eq!(staged[0].legacy_cleanup_username, None);

        db.conn
            .execute_batch("DROP TRIGGER fail_legacy_cleanup_publish;")
            .unwrap();
        assert!(
            db.publish_legacy_credential_secret_slot_cas(
                logical.as_str(),
                logical.as_str(),
                slot.as_str(),
                None,
                None,
            )
            .unwrap()
        );
        db.conn
            .execute_batch(
                "CREATE TRIGGER fail_legacy_cleanup_ack
                 BEFORE UPDATE OF legacy_cleanup_username ON physical_secret_slot_ledger
                 WHEN OLD.legacy_cleanup_username IS NOT NULL
                      AND NEW.legacy_cleanup_username IS NULL
                 BEGIN SELECT RAISE(ABORT, 'injected legacy ack failure'); END;",
            )
            .unwrap();
        assert!(
            db.acknowledge_legacy_secret_source_deleted(
                logical.as_str(),
                slot.as_str(),
                logical.as_str(),
            )
            .is_err()
        );
        assert_eq!(
            db.physical_secret_slots_for_reconciliation(1).unwrap()[0]
                .legacy_cleanup_username
                .as_deref(),
            Some(logical.as_str())
        );
    }

    #[test]
    fn legacy_publish_stale_cas는_live_pointer와_source를_보존하고_candidate를_orphan한다() {
        let db = Db::open_in_memory().unwrap();
        let logical = secret::LogicalCredentialId::new("legacy-cleanup-stale").unwrap();
        let live = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        let candidate = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        db.insert_credential(&sample(logical.as_str())).unwrap();
        stage_slot(&db, &logical, &live);
        assert!(
            db.publish_credential_secret_slot_cas(
                logical.as_str(),
                logical.as_str(),
                live.as_str(),
                None,
                None,
            )
            .unwrap()
        );
        stage_slot(&db, &logical, &candidate);
        assert!(
            !db.publish_legacy_credential_secret_slot_cas(
                logical.as_str(),
                logical.as_str(),
                candidate.as_str(),
                None,
                None,
            )
            .unwrap()
        );
        assert_eq!(
            credential_secret_record(&db, logical.as_str()).keyring_username,
            live.as_str()
        );
        let rows = db.physical_secret_slots_for_reconciliation(2).unwrap();
        assert_eq!(
            rows.iter()
                .find(|row| row.physical_slot == live.as_str())
                .unwrap()
                .state,
            PhysicalSecretSlotState::Published
        );
        let orphan = rows
            .iter()
            .find(|row| row.physical_slot == candidate.as_str())
            .unwrap();
        assert_eq!(orphan.state, PhysicalSecretSlotState::Orphan);
        assert_eq!(orphan.legacy_cleanup_username, None);
        assert!(
            db.publish_legacy_credential_secret_slot_cas(
                logical.as_str(),
                "validated-alias-is-not-supported",
                candidate.as_str(),
                None,
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn first_publish_revision_cas는_other_writer_stale에서_staging을_보존한다() {
        let (dir, path, mut db_a) = file_db("physical-slot-first-publish-stale");
        let db_b = Db::open(&path).unwrap();
        let logical = secret::LogicalCredentialId::new("ledger-first-stale").unwrap();
        let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        stage_slot(&db_a, &logical, &slot);
        let expected = db_a.connector_config_revision().unwrap();
        db_b.insert_credential(&sample("other-writer-credential"))
            .unwrap();
        let current_revision = db_b.connector_config_revision().unwrap();
        let result = db_a
            .insert_credential_with_secret_slot_revision_cas(
                expected,
                &sample(logical.as_str()),
                slot.as_str(),
                Some(r#"{"server_id":"server"}"#),
            )
            .unwrap();
        match result {
            ConnectorConfigCas::Stale {
                current_revision: actual,
            } => assert_eq!(actual, current_revision),
            ConnectorConfigCas::Committed { .. } => panic!("stale first publish committed"),
        }
        assert!(
            db_a.credential_secret_location(logical.as_str())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db_a.physical_secret_slots_for_reconciliation(1).unwrap()[0].state,
            PhysicalSecretSlotState::Staging
        );

        let (published_revision, ()) = committed(
            db_a.insert_credential_with_secret_slot_revision_cas(
                current_revision,
                &sample(logical.as_str()),
                slot.as_str(),
                Some(r#"{"server_id":"server"}"#),
            )
            .unwrap(),
        );
        assert!(published_revision > current_revision);
        assert_eq!(
            db_a.physical_secret_slots_for_reconciliation(1).unwrap()[0].state,
            PhysicalSecretSlotState::Published
        );

        drop(db_a);
        drop(db_b);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn physical_slot_publish_failure는_ledger_pointer_revision을_전부_rollback한다() {
        let mut db = Db::open_in_memory().unwrap();
        let logical = secret::LogicalCredentialId::new("ledger-publish-rollback").unwrap();
        let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        stage_slot(&db, &logical, &slot);
        let expected = db.connector_config_revision().unwrap();
        db.conn
            .execute_batch(
                "CREATE TRIGGER fail_first_slot_publish
                 BEFORE UPDATE ON physical_secret_slot_ledger
                 WHEN OLD.state = 'staging' AND NEW.state = 'published'
                 BEGIN SELECT RAISE(ABORT, 'injected ledger publish failure'); END;",
            )
            .unwrap();
        assert!(
            db.insert_credential_with_secret_slot_revision_cas(
                expected,
                &sample(logical.as_str()),
                slot.as_str(),
                None,
            )
            .is_err()
        );
        assert_eq!(db.connector_config_revision().unwrap(), expected);
        assert!(
            db.credential_secret_location(logical.as_str())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.physical_secret_slots_for_reconciliation(1).unwrap()[0].state,
            PhysicalSecretSlotState::Staging
        );

        db.conn
            .execute_batch("DROP TRIGGER fail_first_slot_publish;")
            .unwrap();
        db.conn
            .execute(
                "UPDATE connector_config_state SET revision = ?1 WHERE singleton = 1",
                [i64::MAX],
            )
            .unwrap();
        let maximum = db.connector_config_revision().unwrap();
        assert!(
            db.insert_credential_with_secret_slot_revision_cas(
                maximum,
                &sample(logical.as_str()),
                slot.as_str(),
                None,
            )
            .is_err()
        );
        assert_eq!(db.connector_config_revision().unwrap(), maximum);
        assert!(
            db.credential_secret_location(logical.as_str())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.physical_secret_slots_for_reconciliation(1).unwrap()[0].state,
            PhysicalSecretSlotState::Staging
        );
    }

    #[test]
    fn physical_slot_credential_insert는_cross_owner_corrupt_unversioned를_reject한다() {
        let logical = secret::LogicalCredentialId::new("cred-owner-new").unwrap();
        let other = secret::LogicalCredentialId::new("cred-other-new").unwrap();
        let other_slot = secret::PhysicalSecretSlot::with_version(&other, uuid::Uuid::new_v4());
        for invalid in [
            other_slot.as_str(),
            "deppy.oauth.v1.not-hex.not-a-uuid",
            logical.as_str(),
        ] {
            let db = Db::open_in_memory().unwrap();
            assert!(
                db.insert_credential_with_secret_slot(
                    &sample(logical.as_str()),
                    invalid,
                    Some(r#"{"server_id":"must-not-insert"}"#),
                )
                .is_err(),
                "invalid pointer unexpectedly inserted: {invalid}"
            );
            assert!(db.list_credential_secret_records(1).unwrap().is_empty());
        }
    }

    #[test]
    fn physical_slot_validation은_oversized_borrowed_ids를_allocation전에_reject한다() {
        let logical = secret::LogicalCredentialId::new("bounded-owner").unwrap();
        let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        let oversized_logical = "l".repeat(97);
        let oversized_slot = "s".repeat(256);

        assert!(validate_owned_physical_secret_slot(&oversized_logical, slot.as_str()).is_err());
        assert!(validate_owned_physical_secret_slot(logical.as_str(), &oversized_slot).is_err());
    }

    #[test]
    fn physical_slot_ledger는_cross_owner_corrupt_state와_id_limits를_fail_closed한다() {
        let db = Db::open_in_memory().unwrap();
        let logical = secret::LogicalCredentialId::new("ledger-owner").unwrap();
        let other = secret::LogicalCredentialId::new("ledger-other").unwrap();
        let other_slot = secret::PhysicalSecretSlot::with_version(&other, uuid::Uuid::new_v4());
        assert!(
            db.register_physical_secret_slot_staging(logical.as_str(), other_slot.as_str())
                .is_err()
        );
        assert!(
            db.register_physical_secret_slot_staging("ledger\0owner", other_slot.as_str())
                .is_err()
        );
        assert!(
            db.conn
                .execute(
                    "INSERT INTO physical_secret_slot_ledger
                       (physical_slot, logical_credential_id, state, created_at, updated_at)
                     VALUES ('invalid-state-slot', 'ledger-owner', 'broken', 1, 1)",
                    [],
                )
                .is_err()
        );
        assert!(
            db.conn
                .execute(
                    "INSERT INTO physical_secret_slot_ledger
                       (physical_slot, logical_credential_id, state, created_at, updated_at)
                     VALUES (?1, 'ledger-owner', 'staging', 1, 1)",
                    ["x".repeat(256)],
                )
                .is_err()
        );
        db.conn
            .execute(
                "INSERT INTO physical_secret_slot_ledger
                   (physical_slot, logical_credential_id, state, created_at, updated_at)
                 VALUES ('malformed-but-bounded', 'ledger-owner', 'staging', 1, 1)",
                [],
            )
            .unwrap();
        assert!(db.physical_secret_slots_for_reconciliation(1).is_err());
    }

    #[test]
    fn physical_slot_reconciliation은_item_plus_one과_byte_budget을_preflight한다() {
        let db = Db::open_in_memory().unwrap();
        let tx = db.conn.unchecked_transaction().unwrap();
        for index in 0..=PHYSICAL_SECRET_SLOT_RECONCILIATION_LIMIT_MAX {
            let logical =
                secret::LogicalCredentialId::new(format!("ledger-item-{index:04}")).unwrap();
            let slot = secret::PhysicalSecretSlot::with_version(
                &logical,
                uuid::Uuid::from_u128(u128::try_from(index + 1).unwrap()),
            );
            tx.execute(
                "INSERT INTO physical_secret_slot_ledger
                   (physical_slot, logical_credential_id, state, created_at, updated_at)
                 VALUES (?1, ?2, 'staging', ?3, ?3)",
                (
                    slot.as_str(),
                    logical.as_str(),
                    i64::try_from(index).unwrap(),
                ),
            )
            .unwrap();
        }
        tx.commit().unwrap();
        assert!(
            db.physical_secret_slots_for_reconciliation(
                PHYSICAL_SECRET_SLOT_RECONCILIATION_LIMIT_MAX
            )
            .is_err()
        );
        assert!(
            db.physical_secret_slots_for_reconciliation(
                PHYSICAL_SECRET_SLOT_RECONCILIATION_LIMIT_MAX + 1
            )
            .is_err()
        );
        let overflow_logical = secret::LogicalCredentialId::new("ledger-item-overflow").unwrap();
        let overflow_slot =
            secret::PhysicalSecretSlot::with_version(&overflow_logical, uuid::Uuid::new_v4());
        assert!(
            db.register_physical_secret_slot_staging(
                overflow_logical.as_str(),
                overflow_slot.as_str()
            )
            .is_err(),
            "write path must not grow an already-over-limit ledger"
        );

        let db = Db::open_in_memory().unwrap();
        let tx = db.conn.unchecked_transaction().unwrap();
        for index in 0..2_500usize {
            let suffix = format!("{index:04}");
            let logical = secret::LogicalCredentialId::new(format!(
                "{}{}",
                "x".repeat(96 - suffix.len()),
                suffix
            ))
            .unwrap();
            let slot = secret::PhysicalSecretSlot::with_version(
                &logical,
                uuid::Uuid::from_u128(u128::try_from(index + 1).unwrap()),
            );
            tx.execute(
                "INSERT INTO physical_secret_slot_ledger
                   (physical_slot, logical_credential_id, state, created_at, updated_at,
                    legacy_cleanup_username)
                 VALUES (?1, ?2, 'orphan', ?3, ?3, ?2)",
                (
                    slot.as_str(),
                    logical.as_str(),
                    i64::try_from(index).unwrap(),
                ),
            )
            .unwrap();
        }
        tx.commit().unwrap();
        assert!(
            db.physical_secret_slots_for_reconciliation(
                PHYSICAL_SECRET_SLOT_RECONCILIATION_LIMIT_MAX
            )
            .is_err(),
            "rows below the item ceiling must still obey the byte budget"
        );
        let overflow_logical = secret::LogicalCredentialId::new("ledger-byte-overflow").unwrap();
        let overflow_slot =
            secret::PhysicalSecretSlot::with_version(&overflow_logical, uuid::Uuid::new_v4());
        assert!(
            db.register_physical_secret_slot_staging(
                overflow_logical.as_str(),
                overflow_slot.as_str()
            )
            .is_err(),
            "write path must enforce the same byte budget as startup reconciliation"
        );
    }

    #[test]
    fn v27_physical_pointer는_v28_ledger에_published로_backfill되고_restart된다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-ledger-backfill-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        let logical = secret::LogicalCredentialId::new("ledger-backfill").unwrap();
        let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..27] {
                conn.execute_batch(sql).unwrap();
            }
            conn.execute(
                "INSERT INTO credentials
                   (id, provider, label, credential_kind, keyring_service, keyring_username,
                    masked_hint, workspace_id, oauth_json, created_at, updated_at)
                 VALUES (?1, 'test', 'test', 'oauth', ?2, ?3, NULL, NULL, NULL, 'now', 'now')",
                (logical.as_str(), secret::KEYRING_SERVICE, slot.as_str()),
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 27).unwrap();
        }
        for _ in 0..2 {
            let db = Db::open(&path).unwrap();
            assert_eq!(
                db.physical_secret_slots_for_reconciliation(1).unwrap(),
                vec![PhysicalSecretSlotLedgerRow {
                    recovery_generation: db.physical_secret_slots_for_reconciliation(1).unwrap()[0]
                        .recovery_generation,
                    logical_credential_id: logical.as_str().to_owned(),
                    physical_slot: slot.as_str().to_owned(),
                    state: PhysicalSecretSlotState::Published,
                    legacy_cleanup_username: None,
                }]
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn v28_backfill은_duplicate_physical_slot_collision을_fail_closed한다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-ledger-corrupt-backfill-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        let owner = secret::LogicalCredentialId::new("ledger-collision-owner").unwrap();
        let other = secret::LogicalCredentialId::new("ledger-collision-other").unwrap();
        let slot = secret::PhysicalSecretSlot::with_version(&owner, uuid::Uuid::new_v4());
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..27] {
                conn.execute_batch(sql).unwrap();
            }
            for logical_id in [owner.as_str(), other.as_str()] {
                conn.execute(
                    "INSERT INTO credentials
                       (id, provider, label, credential_kind, keyring_service, keyring_username,
                        masked_hint, workspace_id, oauth_json, created_at, updated_at)
                     VALUES (?1, 'test', 'test', 'oauth', ?2, ?3,
                             NULL, NULL, NULL, 'now', 'now')",
                    (logical_id, secret::KEYRING_SERVICE, slot.as_str()),
                )
                .unwrap();
            }
            conn.pragma_update(None, "user_version", 27).unwrap();
        }

        assert!(
            Db::open(&path).is_err(),
            "duplicate exact slot ownership must abort migration instead of skipping a row"
        );
        let conn = Connection::open(&path).unwrap();
        assert_eq!(Db::read_user_version(&conn).unwrap(), 27);
        let ledger_exists: i64 = conn
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM sqlite_master
                     WHERE type = 'table' AND name = 'physical_secret_slot_ledger'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ledger_exists, 0, "failed migration must roll back its DDL");
        drop(conn);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn v29에서_v30_legacy_marker로_upgrade하고_ddl_failure는_version과_alter를_rollback한다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-legacy-marker-migration-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let good_path = dir.join("good.sqlite3");
        {
            let conn = Connection::open(&good_path).unwrap();
            for sql in &MIGRATIONS[..29] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 29).unwrap();
        }
        let db = Db::open(&good_path).unwrap();
        assert_eq!(Db::read_user_version(&db.conn).unwrap(), MIGRATIONS.len());
        let marker_columns: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('physical_secret_slot_ledger')
                 WHERE name = 'legacy_cleanup_username'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marker_columns, 1);
        drop(db);

        let rollback_path = dir.join("rollback.sqlite3");
        {
            let conn = Connection::open(&rollback_path).unwrap();
            for sql in &MIGRATIONS[..29] {
                conn.execute_batch(sql).unwrap();
            }
            conn.execute_batch(
                "CREATE INDEX idx_physical_secret_slot_one_legacy_cleanup
                 ON credentials(id);",
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 29).unwrap();
        }
        assert!(Db::open(&rollback_path).is_err());
        let conn = Connection::open(&rollback_path).unwrap();
        assert_eq!(Db::read_user_version(&conn).unwrap(), 29);
        let marker_columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('physical_secret_slot_ledger')
                 WHERE name = 'legacy_cleanup_username'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marker_columns, 0, "failed v30 must roll back ALTER TABLE");
        drop(conn);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_marker_schema는_duplicate_invalid_transition과_corrupt_data를_fail_closed한다() {
        let db = Db::open_in_memory().unwrap();
        let logical = secret::LogicalCredentialId::new("legacy-marker-constraints").unwrap();
        let first = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        let second = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        db.insert_credential(&sample(logical.as_str())).unwrap();
        stage_slot(&db, &logical, &first);
        assert!(
            db.publish_legacy_credential_secret_slot_cas(
                logical.as_str(),
                logical.as_str(),
                first.as_str(),
                None,
                None,
            )
            .unwrap()
        );
        stage_slot(&db, &logical, &second);
        db.rotate_credential_secret_slot(
            logical.as_str(),
            second.as_str(),
            r#"{"server_id":"constraints"}"#,
            None,
        )
        .unwrap();

        assert!(
            db.conn
                .execute(
                    "UPDATE physical_secret_slot_ledger
                     SET legacy_cleanup_username = ?2 WHERE physical_slot = ?1",
                    (second.as_str(), logical.as_str()),
                )
                .is_err(),
            "one logical legacy source cannot have duplicate cleanup obligations"
        );
        for invalid in ["wrong-logical", "nul\0marker"] {
            assert!(
                db.conn
                    .execute(
                        "UPDATE physical_secret_slot_ledger
                         SET legacy_cleanup_username = ?2 WHERE physical_slot = ?1",
                        (second.as_str(), invalid),
                    )
                    .is_err()
            );
        }
        assert!(
            db.conn
                .execute(
                    "UPDATE physical_secret_slot_ledger
                     SET legacy_cleanup_username = ?2 WHERE physical_slot = ?1",
                    (second.as_str(), "x".repeat(256)),
                )
                .is_err()
        );
        assert_eq!(
            db.physical_secret_slots_for_reconciliation(2)
                .unwrap()
                .iter()
                .filter(|row| row.legacy_cleanup_username.is_some())
                .count(),
            1
        );
    }

    #[test]
    fn physical_slot_cas는_success후_stale_expected를_noop처리한다() {
        let db = Db::open_in_memory().unwrap();
        let logical = secret::LogicalCredentialId::new("cred-cas").unwrap();
        let first = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        let stale_candidate =
            secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        db.insert_credential(&sample(logical.as_str())).unwrap();
        stage_slot(&db, &logical, &first);

        assert!(
            db.publish_credential_secret_slot_cas(
                logical.as_str(),
                logical.as_str(),
                first.as_str(),
                None,
                Some("…1111"),
            )
            .unwrap()
        );
        let published = credential_secret_record(&db, logical.as_str());
        assert_eq!(published.keyring_username, first.as_str());
        assert_eq!(published.oauth_json, None);
        assert_eq!(published.meta.masked_hint.as_deref(), Some("…1111"));

        stage_slot(&db, &logical, &stale_candidate);
        let revision_before_stale = db.connector_config_revision().unwrap();
        assert!(
            !db.publish_credential_secret_slot_cas(
                logical.as_str(),
                logical.as_str(),
                stale_candidate.as_str(),
                Some(r#"{"server_id":"stale"}"#),
                Some("…9999"),
            )
            .unwrap()
        );
        assert_eq!(credential_secret_record(&db, logical.as_str()), published);
        assert_eq!(
            db.connector_config_revision().unwrap(),
            revision_before_stale
        );
        let rows = db.physical_secret_slots_for_reconciliation(2).unwrap();
        assert_eq!(
            rows.iter()
                .find(|row| row.physical_slot == first.as_str())
                .unwrap()
                .state,
            PhysicalSecretSlotState::Published
        );
        assert_eq!(
            rows.iter()
                .find(|row| row.physical_slot == stale_candidate.as_str())
                .unwrap()
                .state,
            PhysicalSecretSlotState::Orphan
        );
    }

    #[test]
    fn physical_slot_cas는_invalid_pointer와_storage_failure에서_no_mutation이다() {
        let db = Db::open_in_memory().unwrap();
        let logical = secret::LogicalCredentialId::new("cred-cas-rollback").unwrap();
        let other = secret::LogicalCredentialId::new("cred-cas-other").unwrap();
        let current = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        let next = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        let other_slot = secret::PhysicalSecretSlot::with_version(&other, uuid::Uuid::new_v4());
        stage_slot(&db, &logical, &current);
        db.insert_credential_with_secret_slot(
            &sample(logical.as_str()),
            current.as_str(),
            Some(r#"{"server_id":"old"}"#),
        )
        .unwrap();
        let before = credential_secret_record(&db, logical.as_str());

        for invalid in [
            other_slot.as_str(),
            "deppy.oauth.v1.not-hex.not-a-uuid",
            logical.as_str(),
        ] {
            assert!(
                db.publish_credential_secret_slot_cas(
                    logical.as_str(),
                    current.as_str(),
                    invalid,
                    None,
                    None,
                )
                .is_err(),
                "invalid CAS pointer unexpectedly accepted: {invalid}"
            );
            assert_eq!(credential_secret_record(&db, logical.as_str()), before);
        }

        db.conn
            .execute_batch(
                "CREATE TRIGGER fail_secret_slot_cas AFTER UPDATE OF keyring_username ON credentials
                 BEGIN SELECT RAISE(ABORT, 'injected CAS failure'); END;",
            )
            .unwrap();
        stage_slot(&db, &logical, &next);
        assert!(
            db.publish_credential_secret_slot_cas(
                logical.as_str(),
                current.as_str(),
                next.as_str(),
                None,
                Some("…2222"),
            )
            .is_err()
        );
        assert_eq!(credential_secret_record(&db, logical.as_str()), before);
        let rows = db.physical_secret_slots_for_reconciliation(2).unwrap();
        assert_eq!(
            rows.iter()
                .find(|row| row.physical_slot == current.as_str())
                .unwrap()
                .state,
            PhysicalSecretSlotState::Published
        );
        assert_eq!(
            rows.iter()
                .find(|row| row.physical_slot == next.as_str())
                .unwrap()
                .state,
            PhysicalSecretSlotState::Staging
        );
    }

    #[test]
    fn oauth_secret_slot_pointer와_metadata는_원자적으로_publish된다() {
        let db = Db::open_in_memory().unwrap();
        let logical = secret::LogicalCredentialId::new("cred-oauth").unwrap();
        let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        db.insert_credential(&sample(logical.as_str())).unwrap();
        stage_slot(&db, &logical, &slot);
        db.rotate_credential_secret_slot(
            logical.as_str(),
            slot.as_str(),
            r#"{"server_id":"srv-1","client_id":"cid-1"}"#,
            Some("…7890"),
        )
        .unwrap();

        let location = db
            .credential_secret_location(logical.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(location.keyring_service, secret::KEYRING_SERVICE);
        assert_eq!(location.keyring_username, slot.as_str());
        assert_eq!(
            db.list_credential_oauth_json().unwrap()[0].1,
            r#"{"server_id":"srv-1","client_id":"cid-1"}"#
        );
    }

    #[test]
    fn physical_slot_publish는_other_corrupt_unversioned_pointer를_fail_closed한다() {
        let db = Db::open_in_memory().unwrap();
        let logical = secret::LogicalCredentialId::new("cred-owner").unwrap();
        let other = secret::LogicalCredentialId::new("cred-other").unwrap();
        let other_slot = secret::PhysicalSecretSlot::with_version(&other, uuid::Uuid::new_v4());
        db.insert_credential(&sample(logical.as_str())).unwrap();

        for invalid in [
            other_slot.as_str(),
            "deppy.oauth.v1.not-hex.not-a-uuid",
            logical.as_str(),
        ] {
            assert!(
                db.rotate_credential_secret_slot(
                    logical.as_str(),
                    invalid,
                    r#"{"server_id":"must-not-publish"}"#,
                    None,
                )
                .is_err(),
                "invalid pointer unexpectedly published: {invalid}"
            );
            let location = db
                .credential_secret_location(logical.as_str())
                .unwrap()
                .unwrap();
            assert_eq!(location.keyring_username, logical.as_str());
            assert!(db.list_credential_oauth_json().unwrap().is_empty());
        }
    }

    #[test]
    fn oauth_secret_slot_publish_실패는_pointer와_metadata를_rollback한다() {
        let db = Db::open_in_memory().unwrap();
        let logical = secret::LogicalCredentialId::new("cred-oauth").unwrap();
        let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        db.insert_credential(&sample(logical.as_str())).unwrap();
        db.set_credential_oauth_json(logical.as_str(), r#"{"server_id":"old"}"#)
            .unwrap();
        stage_slot(&db, &logical, &slot);
        db.conn
            .execute_batch(
                "CREATE TRIGGER fail_slot_publish AFTER UPDATE OF keyring_username ON credentials
                 BEGIN SELECT RAISE(ABORT, 'injected slot publish failure'); END;",
            )
            .unwrap();

        assert!(
            db.rotate_credential_secret_slot(
                logical.as_str(),
                slot.as_str(),
                r#"{"server_id":"new"}"#,
                None,
            )
            .is_err()
        );
        assert_eq!(
            db.credential_secret_location(logical.as_str())
                .unwrap()
                .unwrap()
                .keyring_username,
            logical.as_str()
        );
        assert_eq!(
            db.list_credential_oauth_json().unwrap()[0].1,
            r#"{"server_id":"old"}"#
        );
    }

    #[test]
    fn oauth_metadata에는_concrete_token을_저장하지_않는다() {
        let db = Db::open_in_memory().unwrap();
        let logical = secret::LogicalCredentialId::new("cred-oauth").unwrap();
        let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        db.insert_credential(&sample(logical.as_str())).unwrap();
        stage_slot(&db, &logical, &slot);
        assert!(
            db.rotate_credential_secret_slot(
                logical.as_str(),
                slot.as_str(),
                r#"{"access_token":"plaintext"}"#,
                None,
            )
            .is_err()
        );
        assert!(db.list_credential_oauth_json().unwrap().is_empty());
    }

    #[test]
    fn pending_approval_owner는_physical_db별_배타적이고_drop후_해제된다() {
        let (dir_a, path_a, db_a) = file_db("pending-owner-a");
        let db_a_second = Db::open(&path_a).unwrap();
        let (dir_b, _path_b, db_b) = file_db("pending-owner-b");
        let owner_dir_a = pending_approval_owner_lock_dir(&db_a.authorization_db_identity);
        let owner_dir_b = pending_approval_owner_lock_dir(&db_b.authorization_db_identity);
        assert_ne!(
            owner_dir_a,
            authorization_lock_dir(&db_a.authorization_db_identity)
        );

        let owner_a = db_a.acquire_pending_approval_owner().unwrap();
        let busy = db_a_second.acquire_pending_approval_owner().unwrap_err();
        assert_eq!(busy.to_string(), PENDING_APPROVAL_OWNER_UNAVAILABLE);
        let raw_path = path_a.to_string_lossy();
        assert!(!format!("{busy:?}").contains(raw_path.as_ref()));
        assert!(!format!("{busy:?}").contains(&db_a.authorization_db_identity));
        let owner_b = db_b.acquire_pending_approval_owner().unwrap();
        let debug = format!("{owner_a:?}");
        assert_eq!(debug, "ActivePendingApprovalOwner { state: \"exclusive\" }");
        assert!(!debug.contains(raw_path.as_ref()));
        assert!(!debug.contains(&db_a.authorization_db_identity));

        drop(owner_a);
        drop(db_a_second.acquire_pending_approval_owner().unwrap());
        for _ in 0..1024 {
            drop(db_a.acquire_pending_approval_owner().unwrap());
        }
        let lock_files = fs::read_dir(&owner_dir_a)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(lock_files, [std::ffi::OsString::from("owner.lock")]);

        drop(owner_b);
        drop(db_b);
        drop(db_a_second);
        drop(db_a);
        fs::remove_dir_all(owner_dir_b).unwrap();
        fs::remove_dir_all(owner_dir_a).unwrap();
        fs::remove_dir_all(dir_b).unwrap();
        fs::remove_dir_all(dir_a).unwrap();
    }

    #[test]
    fn pending_approval_owner는_file_backed_db만_허용한다() {
        let db = Db::open_in_memory().unwrap();
        let error = db.acquire_pending_approval_owner().unwrap_err();
        assert_eq!(
            error.to_string(),
            PENDING_APPROVAL_OWNER_FILE_BACKED_REQUIRED
        );
        assert!(!format!("{error:?}").contains(&db.authorization_db_identity));
    }

    #[test]
    fn pending_approval_reconciliation_owner는_db_identity에_묶이고_max에서_멱등이다() {
        let (dir_a, _path_a, db_a) = file_db("pending-owner-max-a");
        let (dir_b, path_b, db_b) = file_db("pending-owner-max-b");
        let owner_dir_a = pending_approval_owner_lock_dir(&db_a.authorization_db_identity);
        let owner_a = db_a.acquire_pending_approval_owner().unwrap();
        let session = "315f68b6-333f-409f-a2c5-922b9eacfd7e:42";
        let rows = (0..mcp_store::PENDING_APPROVAL_GLOBAL_LIMIT_MAX)
            .map(|index| mcp_store::PendingApprovalInsert {
                id: format!("owned-reconciliation-{index}"),
                server_id: "srv".to_owned(),
                tool_name: "tool".to_owned(),
                arguments_preview: "{}".to_owned(),
                schema_hash: None,
                created_at: i64::try_from(index).unwrap(),
                pane_id: Some(session.to_owned()),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            mcp_store::insert_pending_approval_batch(&db_a.conn, &rows).unwrap(),
            rows.len()
        );
        db_b.insert_pending_approval("foreign", "srv", "tool", "{}", None, 1, Some(session))
            .unwrap();

        let mismatch = db_b
            .deny_session_scoped_pending_approvals_owned(&owner_a, 100)
            .unwrap_err();
        assert_eq!(mismatch.to_string(), PENDING_APPROVAL_OWNER_DB_MISMATCH);
        let mismatch_debug = format!("{mismatch:?}");
        assert!(!mismatch_debug.contains(path_b.to_string_lossy().as_ref()));
        assert!(!mismatch_debug.contains(&db_a.authorization_db_identity));
        assert!(!mismatch_debug.contains(&db_b.authorization_db_identity));
        assert_eq!(
            db_b.poll_approval("foreign").unwrap().status,
            ApprovalStatus::Pending
        );

        assert_eq!(
            db_a.deny_session_scoped_pending_approvals_owned(&owner_a, 200)
                .unwrap(),
            mcp_store::PENDING_APPROVAL_GLOBAL_LIMIT_MAX
        );
        assert_eq!(
            db_a.deny_session_scoped_pending_approvals_owned(&owner_a, 300)
                .unwrap(),
            0
        );

        drop(owner_a);
        drop(db_b);
        drop(db_a);
        fs::remove_dir_all(owner_dir_a).unwrap();
        fs::remove_dir_all(dir_b).unwrap();
        fs::remove_dir_all(dir_a).unwrap();
    }

    #[test]
    fn owned_pending_approval_reconciliation_failure는_전체를_rollback한다() {
        let (dir, _path, db) = file_db("pending-owner-rollback");
        let owner_dir = pending_approval_owner_lock_dir(&db.authorization_db_identity);
        let owner = db.acquire_pending_approval_owner().unwrap();
        let session = "315f68b6-333f-409f-a2c5-922b9eacfd7e:43";
        for id in ["owned-first", "owned-second"] {
            db.insert_pending_approval(id, "srv", "tool", "{}", None, 1, Some(session))
                .unwrap();
        }
        db.conn
            .execute_batch(
                "CREATE TRIGGER fail_owned_reconciliation
                 AFTER UPDATE OF status ON pending_approvals
                 WHEN NEW.id = 'owned-second' AND NEW.status = 'denied'
                 BEGIN SELECT RAISE(ABORT, 'injected owned reconciliation failure'); END;",
            )
            .unwrap();

        assert!(
            db.deny_session_scoped_pending_approvals_owned(&owner, 50)
                .is_err()
        );
        for id in ["owned-first", "owned-second"] {
            assert_eq!(
                db.poll_approval(id).unwrap().status,
                ApprovalStatus::Pending
            );
        }

        drop(owner);
        drop(db);
        fs::remove_dir_all(owner_dir).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn authorization_owner는_live_scope를_배타화하고_drop후_same_scope만_unknown처리한다() {
        let (dir, path, db_a) = file_db("owner-recovery");
        let lock_dir = authorization_lock_dir(&db_a.authorization_db_identity);
        let db_b = Db::open(&path).unwrap();
        let scope_a = "proxy:pane-a:server-a";
        let scope_b = noncolliding_scope(scope_a);
        let owner_a = db_a.acquire_authorization_owner(scope_a).unwrap();
        let old_run = owner_a.run_id.clone();

        let audit::AuthorizationPreflight::Prepared(_grant_a) = db_a
            .commit_authorization_preflight(
                &owner_a,
                authorization_plan(
                    "operation-owner-a",
                    "server-a",
                    "tool-a",
                    audit::ApprovalDecision::AllowOnce,
                ),
                "{}",
                &secret::RedactionService::new(),
            )
            .unwrap()
        else {
            panic!("allow must prepare a grant")
        };

        assert!(db_b.acquire_authorization_owner(scope_a).is_err());
        assert_eq!(
            db_a.tool_audit_lifecycle("operation-owner-a").unwrap(),
            Some(audit::AuditLifecycle::Prepared)
        );

        let owner_b = db_b.acquire_authorization_owner(&scope_b).unwrap();
        let audit::AuthorizationPreflight::Prepared(_grant_b) = db_b
            .commit_authorization_preflight(
                &owner_b,
                authorization_plan(
                    "operation-owner-b",
                    "server-b",
                    "tool-b",
                    audit::ApprovalDecision::AllowOnce,
                ),
                "{}",
                &secret::RedactionService::new(),
            )
            .unwrap()
        else {
            panic!("allow must prepare a grant")
        };

        drop(owner_a);
        let owner_a_next = db_b.acquire_authorization_owner(scope_a).unwrap();
        assert_eq!(
            db_b.tool_audit_lifecycle("operation-owner-a").unwrap(),
            Some(audit::AuditLifecycle::Unknown)
        );
        assert_eq!(
            db_b.tool_audit_lifecycle("operation-owner-b").unwrap(),
            Some(audit::AuditLifecycle::Prepared)
        );
        assert!(
            audit::complete_authorization_operation(
                &db_b.conn,
                scope_a,
                &old_run,
                "operation-owner-a",
                audit::AuthorizationOutcome::Succeeded,
            )
            .is_err(),
            "stale run must not complete a superseded operation"
        );

        drop(owner_a_next);
        drop(owner_b);
        drop(db_b);
        drop(db_a);
        fs::remove_dir_all(lock_dir).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn authorization_owner는_db_identity에_묶이고_outcome은_멱등이다() {
        let (dir_a, _path_a, db_a) = file_db("db-a");
        let (dir_b, _path_b, db_b) = file_db("db-b");
        let lock_dir = authorization_lock_dir(&db_a.authorization_db_identity);
        let owner_a = db_a.acquire_authorization_owner("gui").unwrap();
        assert!(
            db_b.commit_authorization_preflight(
                &owner_a,
                authorization_plan(
                    "operation-cross-db",
                    "server",
                    "tool",
                    audit::ApprovalDecision::AllowOnce,
                ),
                "{}",
                &secret::RedactionService::new(),
            )
            .is_err()
        );

        let audit::AuthorizationPreflight::Prepared(grant) = db_a
            .commit_authorization_preflight(
                &owner_a,
                authorization_plan(
                    "operation-idempotent",
                    "server",
                    "tool",
                    audit::ApprovalDecision::AllowOnce,
                ),
                r#"{"token":"never-store-plaintext"}"#,
                &secret::RedactionService::new(),
            )
            .unwrap()
        else {
            panic!("allow must prepare a grant")
        };
        assert_eq!(grant.operation_id(), "operation-idempotent");
        let encrypted: Option<Vec<u8>> = db_a
            .conn
            .query_row(
                "SELECT input_encrypted_blob FROM tool_audit_logs WHERE operation_id = ?1",
                ["operation-idempotent"],
                |row| row.get(0),
            )
            .unwrap();
        assert!(encrypted.is_none());
        let outcome = audit::AuthorizationOutcome::Unknown {
            error_code: "delivery_unknown",
        };
        db_a.complete_authorization_outcome(&owner_a, "operation-idempotent", outcome)
            .unwrap();
        db_a.complete_authorization_outcome(&owner_a, "operation-idempotent", outcome)
            .unwrap();
        assert_eq!(
            db_a.tool_audit_lifecycle("operation-idempotent").unwrap(),
            Some(audit::AuditLifecycle::Unknown)
        );

        drop(owner_a);
        drop(db_a);
        drop(db_b);
        fs::remove_dir_all(lock_dir).unwrap();
        fs::remove_dir_all(dir_a).unwrap();
        fs::remove_dir_all(dir_b).unwrap();
    }

    #[test]
    fn authorization_outcome은_모든종결상태에서_same_idempotent_conflict_rejected다() {
        let (dir, _path, db) = file_db("outcome-idempotency");
        let lock_dir = authorization_lock_dir(&db.authorization_db_identity);
        let owner = db.acquire_authorization_owner("proxy:outcomes").unwrap();
        for (suffix, outcome, lifecycle, conflict) in [
            (
                "success",
                audit::AuthorizationOutcome::Succeeded,
                audit::AuditLifecycle::Succeeded,
                audit::AuthorizationOutcome::Failed {
                    error_code: "backend_error",
                },
            ),
            (
                "failed",
                audit::AuthorizationOutcome::Failed {
                    error_code: "backend_error",
                },
                audit::AuditLifecycle::Failed,
                audit::AuthorizationOutcome::Unknown {
                    error_code: "delivery_unknown",
                },
            ),
            (
                "unknown",
                audit::AuthorizationOutcome::Unknown {
                    error_code: "delivery_unknown",
                },
                audit::AuditLifecycle::Unknown,
                audit::AuthorizationOutcome::Succeeded,
            ),
        ] {
            let operation = format!("operation-outcome-{suffix}");
            let audit::AuthorizationPreflight::Prepared(_grant) = db
                .commit_authorization_preflight(
                    &owner,
                    authorization_plan(
                        &operation,
                        "server",
                        &format!("tool-{suffix}"),
                        audit::ApprovalDecision::AllowOnce,
                    ),
                    "{}",
                    &secret::RedactionService::new(),
                )
                .unwrap()
            else {
                panic!("allow-once must prepare")
            };
            db.complete_authorization_outcome(&owner, &operation, outcome)
                .unwrap();
            db.complete_authorization_outcome(&owner, &operation, outcome)
                .unwrap();
            assert!(
                db.complete_authorization_outcome(&owner, &operation, conflict)
                    .is_err()
            );
            assert_eq!(
                db.tool_audit_lifecycle(&operation).unwrap(),
                Some(lifecycle)
            );
        }
        drop(owner);
        drop(db);
        fs::remove_dir_all(lock_dir).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn authorization_owner_graceful_close와_실패_fallback은_scope_run에_격리된다() {
        let (dir, _path, db) = file_db("owner-close");
        let lock_dir = authorization_lock_dir(&db.authorization_db_identity);
        let scope_a = "proxy:pane-close:server";
        let scope_b = noncolliding_scope(scope_a);
        let owner_a = db.acquire_authorization_owner(scope_a).unwrap();
        let owner_b = db.acquire_authorization_owner(&scope_b).unwrap();
        for (owner, operation, server) in [
            (&owner_a, "operation-close-a", "server-a"),
            (&owner_b, "operation-close-b", "server-b"),
        ] {
            let audit::AuthorizationPreflight::Prepared(_grant) = db
                .commit_authorization_preflight(
                    owner,
                    authorization_plan(
                        operation,
                        server,
                        "tool",
                        audit::ApprovalDecision::AllowOnce,
                    ),
                    "{}",
                    &secret::RedactionService::new(),
                )
                .unwrap()
            else {
                panic!("allow must prepare a grant")
            };
        }
        assert_eq!(db.close_authorization_owner(owner_a).unwrap(), 1);
        assert_eq!(
            db.tool_audit_lifecycle("operation-close-a").unwrap(),
            Some(audit::AuditLifecycle::Unknown)
        );
        assert_eq!(
            db.tool_audit_lifecycle("operation-close-b").unwrap(),
            Some(audit::AuditLifecycle::Prepared)
        );

        db.conn
            .execute_batch(
                "CREATE TRIGGER fail_owner_shutdown BEFORE UPDATE OF lifecycle ON tool_audit_logs
                 WHEN NEW.outcome_error_code = 'owner_shutdown'
                 BEGIN SELECT RAISE(ABORT, 'injected owner shutdown failure'); END;",
            )
            .unwrap();
        assert!(db.close_authorization_owner(owner_b).is_err());
        db.conn
            .execute_batch("DROP TRIGGER fail_owner_shutdown;")
            .unwrap();
        let owner_b_next = db.acquire_authorization_owner(&scope_b).unwrap();
        assert_eq!(
            db.tool_audit_lifecycle("operation-close-b").unwrap(),
            Some(audit::AuditLifecycle::Unknown)
        );
        drop(owner_b_next);
        drop(db);
        fs::remove_dir_all(lock_dir).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn authorization_lock_files는_256_stripe로_상한되고_collision은_직렬화된다() {
        let (dir, path, db_a) = file_db("lock-stripes");
        let db_b = Db::open(&path).unwrap();
        let mut by_stripe: [Option<String>; 256] = std::array::from_fn(|_| None);
        let (scope_a, scope_b) = (0..4096)
            .find_map(|index| {
                let scope = format!("collision-{index}");
                let stripe = usize::from(scope_stripe(&scope));
                if let Some(existing) = by_stripe[stripe].take() {
                    Some((existing, scope))
                } else {
                    by_stripe[stripe] = Some(scope);
                    None
                }
            })
            .unwrap();
        let owner = db_a.acquire_authorization_owner(&scope_a).unwrap();
        assert_ne!(scope_a, scope_b);
        assert!(db_b.acquire_authorization_owner(&scope_b).is_err());
        drop(owner);
        drop(db_b.acquire_authorization_owner(&scope_b).unwrap());

        for index in 0..1024 {
            drop(
                db_a.acquire_authorization_owner(&format!("bounded-{index}"))
                    .unwrap(),
            );
        }
        let lock_dir = authorization_lock_dir(&db_a.authorization_db_identity);
        let lock_files = fs::read_dir(&lock_dir).unwrap().count();
        assert!(lock_files <= 256, "lock file bound exceeded: {lock_files}");

        drop(db_b);
        drop(db_a);
        fs::remove_dir_all(lock_dir).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn hardlink_alias는_같은_physical_db_lock_namespace를_사용한다() {
        let (dir, path, db_a) = file_db("hardlink-a");
        let alias_dir = dir.join("alias");
        fs::create_dir_all(&alias_dir).unwrap();
        let alias = alias_dir.join("metadata-alias.sqlite3");
        fs::hard_link(&path, &alias).unwrap();
        let alias_identity = physical_db_identity(&alias).unwrap();
        assert_eq!(db_a.authorization_db_identity, alias_identity);
        let lock_dir = authorization_lock_dir(&db_a.authorization_db_identity);
        assert_eq!(lock_dir, authorization_lock_dir(&alias_identity));
        let pending_lock_dir = pending_approval_owner_lock_dir(&db_a.authorization_db_identity);
        assert_eq!(
            pending_lock_dir,
            pending_approval_owner_lock_dir(&alias_identity)
        );
        let pending_owner =
            acquire_pending_approval_owner_for_identity(&db_a.authorization_db_identity).unwrap();
        assert_eq!(
            acquire_pending_approval_owner_for_identity(&alias_identity)
                .unwrap_err()
                .to_string(),
            PENDING_APPROVAL_OWNER_UNAVAILABLE
        );
        drop(pending_owner);
        drop(acquire_pending_approval_owner_for_identity(&alias_identity).unwrap());
        fs::create_dir_all(&lock_dir).unwrap();
        let lock_path = lock_dir.join("stripe-000.lock");
        let first = open_lock_file(&lock_path).unwrap();
        let second = open_lock_file(&lock_path).unwrap();
        fs2::FileExt::try_lock_exclusive(&first).unwrap();
        assert!(fs2::FileExt::try_lock_exclusive(&second).is_err());
        drop(second);
        drop(first);
        drop(db_a);
        fs::remove_dir_all(pending_lock_dir).unwrap();
        fs::remove_dir_all(lock_dir).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_path_replacement은_새_physical_identity를_얻는다() {
        use std::io::Write as _;

        let dir = std::env::temp_dir().join(format!(
            "deppy-au01-identity-replace-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        File::create(&path).unwrap().write_all(b"old").unwrap();
        let old_identity = physical_db_identity(&path).unwrap();
        let old_lock_dir = pending_approval_owner_lock_dir(&old_identity);
        let old_owner = acquire_pending_approval_owner_for_identity(&old_identity).unwrap();
        let replacement = dir.join("replacement.sqlite3");
        File::create(&replacement)
            .unwrap()
            .write_all(b"new")
            .unwrap();
        fs::rename(&replacement, &path).unwrap();
        let new_identity = physical_db_identity(&path).unwrap();
        assert_ne!(old_identity, new_identity);
        let new_lock_dir = pending_approval_owner_lock_dir(&new_identity);
        assert_ne!(old_lock_dir, new_lock_dir);
        let new_owner = acquire_pending_approval_owner_for_identity(&new_identity).unwrap();
        drop(new_owner);
        drop(old_owner);
        fs::remove_dir_all(new_lock_dir).unwrap();
        fs::remove_dir_all(old_lock_dir).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn allow_always_permission과_audit은_같이_rollback된다() {
        let (dir, _path, db) = file_db("atomic-preflight");
        let lock_dir = authorization_lock_dir(&db.authorization_db_identity);
        let owner = db.acquire_authorization_owner("gui").unwrap();
        db.conn
            .execute_batch(
                "CREATE TRIGGER fail_shared_preflight BEFORE INSERT ON tool_audit_logs
                 BEGIN SELECT RAISE(ABORT, 'injected shared audit failure'); END;",
            )
            .unwrap();
        assert!(
            db.commit_authorization_preflight(
                &owner,
                authorization_plan(
                    "operation-atomic-fail",
                    "server",
                    "tool",
                    audit::ApprovalDecision::AllowAlways,
                ),
                "{}",
                &secret::RedactionService::new(),
            )
            .is_err()
        );
        assert!(db.permission_rule("server", "tool").unwrap().is_none());
        assert_eq!(
            db.tool_audit_lifecycle("operation-atomic-fail").unwrap(),
            None
        );
        drop(owner);
        drop(db);
        fs::remove_dir_all(lock_dir).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn authorization_preflight는_plan_subject를_same_transaction_audit에_영속한다() {
        let (dir, _path, db) = file_db("authorization-subject");
        let lock_dir = authorization_lock_dir(&db.authorization_db_identity);
        let owner = db.acquire_authorization_owner("gui:subject").unwrap();
        let subject = audit::AuthorizationSubject::try_new(
            Some("workspace-subject".to_owned()),
            Some("session-subject".to_owned()),
        )
        .unwrap();
        let plan = authorization_plan(
            "operation-subject",
            "server",
            "tool",
            audit::ApprovalDecision::AllowOnce,
        )
        .bind_subject(subject.clone())
        .unwrap();
        let audit::AuthorizationPreflight::Prepared(grant) = db
            .commit_authorization_preflight(&owner, plan, "{}", &secret::RedactionService::new())
            .unwrap()
        else {
            panic!("allow must prepare a grant")
        };
        assert_eq!(grant.subject(), &subject);
        let persisted: (Option<String>, Option<String>) = db
            .conn
            .query_row(
                "SELECT workspace_id, session_id FROM tool_audit_logs
                 WHERE operation_id = 'operation-subject'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            persisted,
            (
                Some("workspace-subject".to_owned()),
                Some("session-subject".to_owned())
            )
        );

        drop(owner);
        drop(db);
        fs::remove_dir_all(lock_dir).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn authorization_revision_cas는_other_writer의_server_tool_credential변경을_거부한다() {
        let (dir, path, db_a) = file_db("authorization-config-stale");
        let lock_dir = authorization_lock_dir(&db_a.authorization_db_identity);
        let mut db_b = Db::open(&path).unwrap();
        db_b.insert_mcp_server(&sample_mcp_server("target"))
            .unwrap();
        let owner = db_a
            .acquire_authorization_owner("gui:config-stale")
            .unwrap();

        for (operation, mutate) in [
            ("operation-stale-server", "server"),
            ("operation-stale-tool", "tool"),
            ("operation-stale-credential", "credential"),
        ] {
            let expected = db_a.connector_config_revision().unwrap();
            match mutate {
                "server" => db_b
                    .insert_mcp_server(&sample_mcp_server("other-server"))
                    .unwrap(),
                "tool" => db_b
                    .replace_mcp_tools(
                        "target",
                        &[sample_mcp_tool("target", "tool-id", "tool-name")],
                    )
                    .unwrap(),
                "credential" => db_b.insert_credential(&sample("other-credential")).unwrap(),
                _ => unreachable!(),
            }
            let current_revision = db_b.connector_config_revision().unwrap();
            assert!(current_revision > expected);
            let result = db_a
                .commit_authorization_preflight_revision_cas(
                    expected,
                    &owner,
                    authorization_plan(
                        operation,
                        "target",
                        "tool-name",
                        audit::ApprovalDecision::AllowOnce,
                    ),
                    "{}",
                    &secret::RedactionService::new(),
                )
                .unwrap();
            match result {
                ConnectorConfigCas::Stale {
                    current_revision: actual,
                } => assert_eq!(actual, current_revision),
                ConnectorConfigCas::Committed { .. } => {
                    panic!("stale authorization unexpectedly committed")
                }
            }
            assert_eq!(db_a.tool_audit_lifecycle(operation).unwrap(), None);
            assert!(
                db_a.permission_rule("target", "tool-name")
                    .unwrap()
                    .is_none()
            );
        }

        drop(owner);
        drop(db_a);
        drop(db_b);
        fs::remove_dir_all(lock_dir).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn remembered_authorization은_permission과_audit의_committed_revision을_반환한다() {
        for (label, decision, expected_rule) in [
            ("allow", audit::ApprovalDecision::AllowAlways, "allow"),
            ("deny", audit::ApprovalDecision::DenyAlways, "deny"),
        ] {
            let (dir, _path, db) = file_db(&format!("authorization-revision-{label}"));
            let lock_dir = authorization_lock_dir(&db.authorization_db_identity);
            let owner = db
                .acquire_authorization_owner(&format!("gui:revision:{label}"))
                .unwrap();
            let expected = db.connector_config_revision().unwrap();
            let operation = format!("operation-revision-{label}");
            let (revision, preflight) = committed(
                db.commit_authorization_preflight_revision_cas(
                    expected,
                    &owner,
                    authorization_plan(&operation, "server", "tool", decision),
                    "{}",
                    &secret::RedactionService::new(),
                )
                .unwrap(),
            );
            assert!(revision > expected);
            assert_eq!(db.connector_config_revision().unwrap(), revision);
            assert_eq!(
                db.permission_rule("server", "tool").unwrap().unwrap().rule,
                expected_rule
            );
            match preflight {
                audit::AuthorizationPreflight::Prepared(grant) => db
                    .complete_authorization_outcome(
                        &owner,
                        grant.operation_id(),
                        audit::AuthorizationOutcome::Succeeded,
                    )
                    .unwrap(),
                audit::AuthorizationPreflight::Denied(receipt) => {
                    assert_eq!(receipt.operation_id(), operation)
                }
            }

            drop(owner);
            drop(db);
            fs::remove_dir_all(lock_dir).unwrap();
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn authorization_revision_cas_failure는_permission_audit_revision을_rollback한다() {
        for failure in ["audit", "revision", "overflow"] {
            let (dir, _path, db) = file_db(&format!("authorization-revision-fail-{failure}"));
            let lock_dir = authorization_lock_dir(&db.authorization_db_identity);
            let owner = db
                .acquire_authorization_owner(&format!("gui:revision-fail:{failure}"))
                .unwrap();
            match failure {
                "audit" => db
                    .conn
                    .execute_batch(
                        "CREATE TRIGGER fail_revision_cas_audit
                         BEFORE INSERT ON tool_audit_logs
                         BEGIN SELECT RAISE(ABORT, 'injected audit failure'); END;",
                    )
                    .unwrap(),
                "revision" => db
                    .conn
                    .execute_batch(
                        "CREATE TRIGGER fail_revision_cas_revision
                         BEFORE UPDATE ON connector_config_state
                         BEGIN SELECT RAISE(ABORT, 'injected revision failure'); END;",
                    )
                    .unwrap(),
                "overflow" => {
                    db.conn
                        .execute(
                            "UPDATE connector_config_state SET revision = ?1 WHERE singleton = 1",
                            [i64::MAX],
                        )
                        .unwrap();
                }
                _ => unreachable!(),
            }
            let expected = db.connector_config_revision().unwrap();
            let operation = format!("operation-revision-fail-{failure}");
            assert!(
                db.commit_authorization_preflight_revision_cas(
                    expected,
                    &owner,
                    authorization_plan(
                        &operation,
                        "server",
                        "tool",
                        audit::ApprovalDecision::AllowAlways,
                    ),
                    "{}",
                    &secret::RedactionService::new(),
                )
                .is_err()
            );
            assert_eq!(db.connector_config_revision().unwrap(), expected);
            assert!(db.permission_rule("server", "tool").unwrap().is_none());
            assert_eq!(db.tool_audit_lifecycle(&operation).unwrap(), None);

            drop(owner);
            drop(db);
            fs::remove_dir_all(lock_dir).unwrap();
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn permission_fingerprint변경은_preflight와_grant를_막는다() {
        let (dir, _path, db) = file_db("permission-fingerprint");
        let lock_dir = authorization_lock_dir(&db.authorization_db_identity);
        let owner = db.acquire_authorization_owner("gui:fingerprint").unwrap();
        let plan = authorization_plan(
            "operation-stale-permission",
            "server",
            "tool",
            audit::ApprovalDecision::AllowAlways,
        );
        db.upsert_permission_rule("server", "tool", "deny", None)
            .unwrap();
        assert!(
            db.commit_authorization_preflight(
                &owner,
                plan,
                "{}",
                &secret::RedactionService::new(),
            )
            .is_err()
        );
        assert_eq!(
            db.permission_rule("server", "tool").unwrap().unwrap().rule,
            "deny"
        );
        assert_eq!(
            db.tool_audit_lifecycle("operation-stale-permission")
                .unwrap(),
            None
        );
        drop(owner);
        drop(db);
        fs::remove_dir_all(lock_dir).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn allow_always와_deny_always는_exact_fingerprint에서만_원자commit된다() {
        for (label, decision, expected_rule, expected_hash) in [
            (
                "allow",
                audit::ApprovalDecision::AllowAlways,
                "allow",
                Some(audit::schema_hash(r#"{"type":"object"}"#)),
            ),
            ("deny", audit::ApprovalDecision::DenyAlways, "deny", None),
        ] {
            let (dir, _path, db) = file_db(&format!("remember-{label}"));
            let lock_dir = authorization_lock_dir(&db.authorization_db_identity);
            let owner = db
                .acquire_authorization_owner(&format!("gui:remember:{label}"))
                .unwrap();
            let operation = format!("operation-remember-{label}");
            let preflight = db
                .commit_authorization_preflight(
                    &owner,
                    authorization_plan(&operation, "server", "tool", decision),
                    "{}",
                    &secret::RedactionService::new(),
                )
                .unwrap();
            let row = db.permission_rule("server", "tool").unwrap().unwrap();
            assert_eq!(row.rule, expected_rule);
            assert_eq!(row.approved_schema_hash, expected_hash);
            match (decision, preflight) {
                (
                    audit::ApprovalDecision::AllowAlways,
                    audit::AuthorizationPreflight::Prepared(grant),
                ) => {
                    assert_eq!(grant.operation_id(), operation);
                    db.complete_authorization_outcome(
                        &owner,
                        &operation,
                        audit::AuthorizationOutcome::Succeeded,
                    )
                    .unwrap();
                }
                (
                    audit::ApprovalDecision::DenyAlways,
                    audit::AuthorizationPreflight::Denied(receipt),
                ) => assert_eq!(receipt.operation_id(), operation),
                _ => panic!("remembered decision/preflight mismatch"),
            }
            drop(owner);
            drop(db);
            fs::remove_dir_all(lock_dir).unwrap();
            fs::remove_dir_all(dir).unwrap();
        }
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
    fn v28에서_v29_session_pending_index로_업그레이드된다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-mig-28to29-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..28] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 28).unwrap();
        }

        let db = Db::open(&path).unwrap();
        assert_eq!(Db::read_user_version(&db.conn).unwrap(), MIGRATIONS.len());
        let index_sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master
                 WHERE type = 'index' AND name = 'idx_pending_approvals_session_pending'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(index_sql.contains("ON pending_approvals(pane_id)"));
        assert!(index_sql.contains("WHERE status = 'pending'"));

        let query_plan: String = db
            .conn
            .query_row(
                "EXPLAIN QUERY PLAN
                 SELECT 1 FROM pending_approvals
                 WHERE pane_id = ?1 AND status = 'pending'
                 LIMIT ?2",
                ("315f68b6-333f-409f-a2c5-922b9eacfd7e:2", 257_i64),
                |row| row.get(3),
            )
            .unwrap();
        assert!(query_plan.contains("idx_pending_approvals_session_pending"));
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
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
        db.insert_pending_approval("r", "s", "t", "prev", None, 1, None)
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
    fn env_api_project_counts는_workspace별_집계를_한번에_반환한다() {
        let db = Db::open_in_memory().unwrap();
        let ws1 = db.ensure_default_workspace().unwrap();
        let ws2 = db.create_workspace("second").unwrap();
        let local = db.insert_env_profile(&ws1, "local", "local").unwrap();
        let dotenv = db.insert_env_profile(&ws1, ".env", "dotenv").unwrap();

        let mut local_credential = sample("local-key");
        local_credential.workspace_id = Some(ws1.clone());
        db.insert_credential(&sample("global-key")).unwrap();
        db.insert_credential(&local_credential).unwrap();
        db.insert_credential(&sample("dotenv-key")).unwrap();
        db.upsert_env_var(&local, "PORT", &EnvValue::Plain("3000".into()))
            .unwrap();
        db.upsert_env_var(
            &dotenv,
            "API_KEY",
            &EnvValue::Secret {
                credential_id: "dotenv-key".into(),
            },
        )
        .unwrap();

        let counts = db.env_api_project_counts().unwrap();
        let first = counts
            .iter()
            .find(|count| count.workspace_id == ws1)
            .unwrap();
        assert_eq!((first.env_count, first.key_count), (2, 2));
        let second = counts
            .iter()
            .find(|count| count.workspace_id == ws2)
            .unwrap();
        assert_eq!((second.env_count, second.key_count), (0, 1));
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
    fn settings_bounded_snapshots는_workspace_rows만_한_snapshot으로_반환한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace = db.create_workspace("settings").unwrap();
        let other_workspace = db.create_workspace("other").unwrap();
        let profile = db
            .insert_env_profile(&workspace, "dotenv", "dotenv")
            .unwrap();
        let other_profile = db
            .insert_env_profile(&other_workspace, "other", "custom")
            .unwrap();
        db.upsert_env_var(&profile, "PLAIN", &EnvValue::Plain("value".to_owned()))
            .unwrap();
        db.upsert_env_var(
            &other_profile,
            "OTHER",
            &EnvValue::Plain("not-visible".to_owned()),
        )
        .unwrap();
        db.insert_credential(&CredentialMeta {
            id: "credential-visible".to_owned(),
            provider: "custom".to_owned(),
            label: "visible".to_owned(),
            credential_kind: "api_key".to_owned(),
            masked_hint: Some("••••".to_owned()),
            workspace_id: Some(workspace.clone()),
        })
        .unwrap();
        db.insert_mcp_server(&mcp_store::McpServerRow {
            id: "server-enabled".to_owned(),
            name: "enabled".to_owned(),
            kind: "stdio".to_owned(),
            command: Some("true".to_owned()),
            args: Vec::new(),
            env_plain: Vec::new(),
            env_secrets: Vec::new(),
            inherit_env: true,
            url: None,
            enabled: true,
        })
        .unwrap();
        db.insert_mcp_server(&mcp_store::McpServerRow {
            id: "server-disabled".to_owned(),
            name: "disabled".to_owned(),
            kind: "stdio".to_owned(),
            command: Some("false".to_owned()),
            args: Vec::new(),
            env_plain: Vec::new(),
            env_secrets: Vec::new(),
            inherit_env: true,
            url: None,
            enabled: false,
        })
        .unwrap();
        let agent_id = db
            .insert_agent_config(
                "agent",
                "true",
                &["--version".to_owned()],
                None,
                None,
                None,
                None,
                true,
                Some("server-enabled"),
                None,
            )
            .unwrap();

        let agents = db.settings_agents_snapshot_rows(&workspace).unwrap();
        assert_eq!(agents.agents.len(), 1);
        assert_eq!(agents.profiles.len(), 1);
        assert_eq!(agents.enabled_mcp_servers.len(), 1);
        assert_eq!(agents.enabled_mcp_servers[0].id, "server-enabled");
        let environment = db.settings_environment_snapshot_rows(&workspace).unwrap();
        assert_eq!(environment.credentials.len(), 1);
        assert_eq!(environment.profiles.len(), 1);
        assert_eq!(environment.env_vars.len(), 1);
        assert_eq!(environment.env_vars[0].profile_id, profile);

        let launch = db
            .settings_agent_launch_rows(&workspace, &agent_id, Some(&profile))
            .unwrap();
        assert_eq!(
            launch.agent.as_ref().map(|agent| agent.id.as_str()),
            Some(agent_id.as_str())
        );
        assert_eq!(
            launch.profile.as_ref().map(|row| row.id.as_str()),
            Some(profile.as_str())
        );
        assert_eq!(launch.env_vars.len(), 1);
        assert!(launch.mcp_backend_enabled);
        let cross_workspace = db
            .settings_agent_launch_rows(&workspace, &agent_id, Some(&other_profile))
            .unwrap();
        assert!(cross_workspace.profile.is_none());
        assert!(cross_workspace.env_vars.is_empty());
    }

    #[test]
    fn settings_agent_snapshot는_max_plus_one을_json_materialize전에_거부한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace = db.create_workspace("settings").unwrap();
        let last = SETTINGS_AGENT_LIMIT_MAX;
        db.conn
            .execute(
                "WITH RECURSIVE seq(n) AS (
                     SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n < ?1
                 )
                 INSERT INTO agent_configs
                    (id, name, command, args_json, waiting_regex, approval_regex, error_regex,
                     done_regex, mcp_proxy_enabled, mcp_proxy_server_id, mcp_config_flag,
                     created_at, updated_at)
                 SELECT printf('bounded-agent-%04d', n), 'agent', 'true',
                        CASE WHEN n = 0 THEN '{not-json' ELSE '[]' END,
                        NULL, NULL, NULL, NULL, 0, NULL, NULL,
                        printf('%08d', n), printf('%08d', n)
                 FROM seq",
                [i64::try_from(last).unwrap()],
            )
            .unwrap();

        let error = db.settings_agents_snapshot_rows(&workspace).unwrap_err();
        assert!(
            format!("{error:#}").contains("settings_agents_snapshot_item_limit"),
            "count preflight must win before malformed JSON parsing: {error:#}"
        );
    }

    #[test]
    fn settings_agent_snapshot는_oversized_json을_parse전에_거부한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace = db.create_workspace("settings").unwrap();
        let oversized = "x".repeat(SETTINGS_AGENT_ARGS_BYTES_MAX + 1);
        db.conn
            .execute(
                "INSERT INTO agent_configs
                    (id, name, command, args_json, waiting_regex, approval_regex, error_regex,
                     done_regex, mcp_proxy_enabled, mcp_proxy_server_id, mcp_config_flag,
                     created_at, updated_at)
                 VALUES ('oversized-agent', 'agent', 'true', ?1, NULL, NULL, NULL, NULL,
                         0, NULL, NULL, '', '')",
                [&oversized],
            )
            .unwrap();

        let error = db.settings_agents_snapshot_rows(&workspace).unwrap_err();
        assert!(
            format!("{error:#}").contains("settings_agent_args_json_bytes_limit"),
            "SQL byte preflight must win before malformed JSON parsing: {error:#}"
        );
        let launch_error = db
            .settings_agent_launch_rows(&workspace, "oversized-agent", None)
            .unwrap_err();
        assert!(
            format!("{launch_error:#}").contains("settings_agent_launch_args_json_bytes_limit"),
            "launch point read must apply the same pre-materialization gate: {launch_error:#}"
        );
    }

    #[test]
    fn settings_environment_snapshot는_env_max_plus_one을_materialize전에_거부한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace = db.create_workspace("settings").unwrap();
        let profile = db
            .insert_env_profile(&workspace, "dotenv", "dotenv")
            .unwrap();
        let last = SETTINGS_ENV_VAR_LIMIT_MAX;
        db.conn
            .execute(
                "WITH RECURSIVE seq(n) AS (
                     SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n < ?2
                 )
                 INSERT INTO env_vars
                    (id, profile_id, key, kind, plain_value, credential_id, created_at, updated_at)
                 SELECT printf('bounded-env-%05d', n), ?1, printf('KEY_%05d', n),
                        'plain', 'value', NULL, printf('%08d', n), printf('%08d', n)
                 FROM seq",
                rusqlite::params![profile, i64::try_from(last).unwrap()],
            )
            .unwrap();

        let error = db
            .settings_environment_snapshot_rows(&workspace)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("settings_env_vars_snapshot_item_limit"),
            "env cap+1 must fail before Vec/String materialization: {error:#}"
        );
        let launch_error = db
            .settings_agent_launch_rows(&workspace, "missing-agent", Some(&profile))
            .unwrap_err();
        assert!(
            format!("{launch_error:#}").contains("settings_agent_launch_env_item_limit"),
            "launch env cap+1 must fail before Vec/String materialization: {launch_error:#}"
        );
    }

    #[test]
    fn settings_snapshot는_profile_credential_backend_cap_plus_one을_거부한다() {
        let backend_db = Db::open_in_memory().unwrap();
        let backend_workspace = backend_db.create_workspace("backend").unwrap();
        backend_db
            .conn
            .execute(
                "WITH RECURSIVE seq(n) AS (
                     SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n < ?1
                 )
                 INSERT INTO mcp_servers
                    (id, name, kind, command, args_json, url, enabled, created_at, updated_at)
                 SELECT printf('bounded-server-%04d', n), 'server', 'stdio', 'true', '[]',
                        NULL, 1, printf('%08d', n), printf('%08d', n) FROM seq",
                [i64::try_from(SETTINGS_ENABLED_MCP_SERVER_LIMIT_MAX).unwrap()],
            )
            .unwrap();
        let backend_error = backend_db
            .settings_agents_snapshot_rows(&backend_workspace)
            .unwrap_err();
        assert!(
            format!("{backend_error:#}").contains("settings_agent_backends_snapshot_item_limit")
        );

        let profile_db = Db::open_in_memory().unwrap();
        let profile_workspace = profile_db.create_workspace("profiles").unwrap();
        profile_db
            .conn
            .execute(
                "WITH RECURSIVE seq(n) AS (
                     SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n < ?2
                 )
                 INSERT INTO env_profiles
                    (id, workspace_id, name, kind, is_production, created_at, updated_at)
                 SELECT printf('bounded-profile-%04d', n), ?1, 'profile', 'custom', 0,
                        printf('%08d', n), printf('%08d', n) FROM seq",
                rusqlite::params![
                    profile_workspace,
                    i64::try_from(SETTINGS_ENV_PROFILE_LIMIT_MAX).unwrap()
                ],
            )
            .unwrap();
        let profile_error = profile_db
            .settings_environment_snapshot_rows(&profile_workspace)
            .unwrap_err();
        assert!(format!("{profile_error:#}").contains("settings_env_profiles_snapshot_item_limit"));

        let credential_db = Db::open_in_memory().unwrap();
        let credential_workspace = credential_db.create_workspace("credentials").unwrap();
        credential_db
            .conn
            .execute(
                "WITH RECURSIVE seq(n) AS (
                     SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n < ?2
                 )
                 INSERT INTO credentials
                    (id, provider, label, credential_kind, keyring_service, keyring_username,
                     masked_hint, created_at, updated_at, workspace_id)
                 SELECT printf('bounded-credential-%05d', n), 'custom', 'credential', 'api_key',
                        'app.vector9.deppy-sijo', printf('slot-%05d', n), NULL,
                        printf('%08d', n), printf('%08d', n), ?1 FROM seq",
                rusqlite::params![
                    credential_workspace,
                    i64::try_from(SETTINGS_CREDENTIAL_LIMIT_MAX).unwrap()
                ],
            )
            .unwrap();
        let credential_error = credential_db
            .settings_environment_snapshot_rows(&credential_workspace)
            .unwrap_err();
        assert!(
            format!("{credential_error:#}").contains("settings_credentials_snapshot_item_limit")
        );
    }

    #[test]
    fn settings_agents_snapshot는_category합산_byte_cap을_materialize전에_거부한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace = db.create_workspace("aggregate").unwrap();
        let large = "x".repeat(700 * 1024);
        for index in 0..3 {
            db.conn
                .execute(
                    "INSERT INTO agent_configs
                        (id, name, command, args_json, waiting_regex, approval_regex, error_regex,
                         done_regex, mcp_proxy_enabled, mcp_proxy_server_id, mcp_config_flag,
                         created_at, updated_at)
                     VALUES (?1, ?2, 'true', '[]', NULL, NULL, NULL, NULL, 0, NULL, NULL, ?3, ?3)",
                    (
                        format!("aggregate-agent-{index}"),
                        &large,
                        format!("{index:08}"),
                    ),
                )
                .unwrap();
            db.conn
                .execute(
                    "INSERT INTO env_profiles
                        (id, workspace_id, name, kind, is_production, created_at, updated_at)
                     VALUES (?1, ?2, ?3, 'custom', 0, ?4, ?4)",
                    (
                        format!("aggregate-profile-{index}"),
                        &workspace,
                        &large,
                        format!("{index:08}"),
                    ),
                )
                .unwrap();
        }

        let error = db.settings_agents_snapshot_rows(&workspace).unwrap_err();
        assert!(
            format!("{error:#}").contains("settings_agents_snapshot_retained_bytes_limit"),
            "combined categories must fail before any large Rust String is read: {error:#}"
        );
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
    fn builtin_agent_config_upsert는_고정_id를_갱신하고_soft_delete를_복구한다() {
        let db = Db::open_in_memory().unwrap();
        let id = "deppy-builtin-codex";
        db.upsert_builtin_agent_config(id, "Codex", "/usr/local/bin/codex")
            .unwrap();
        db.upsert_builtin_agent_config(id, "Codex CLI", "/opt/homebrew/bin/codex")
            .unwrap();

        let listed = db.list_agent_configs().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].name, "Codex CLI");
        assert_eq!(listed[0].command, "/opt/homebrew/bin/codex");
        assert!(listed[0].args.is_empty());

        db.delete_agent_config(id).unwrap();
        assert!(db.list_agent_configs().unwrap().is_empty());
        db.upsert_builtin_agent_config(id, "Codex", "/usr/bin/codex")
            .unwrap();
        let revived = db.list_agent_configs().unwrap();
        assert_eq!(revived.len(), 1);
        assert_eq!(revived[0].id, id);
        assert_eq!(revived[0].command, "/usr/bin/codex");
        let total: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM agent_configs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 1);
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
    fn v20에서_v21로_업그레이드된다() {
        let dir = std::env::temp_dir().join(format!("deppy-mig-20to21-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        // user_version=20 (web_push_subscriptions 이전) 구버전 DB 구성
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..20] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 20).unwrap();
        }
        // 오픈 → IMMEDIATE 트랜잭션으로 migration 21 적용. 기존 데이터(무손실) 확인용으로
        // v20까지 존재하던 credentials에 행을 하나 넣어 두고, 마이그레이션 후에도 남는지 본다.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "INSERT INTO credentials
                   (id, provider, label, credential_kind, keyring_service, keyring_username,
                    created_at, updated_at)
                 VALUES ('c1','p','l','k','s','u','t','t')",
                [],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(Db::read_user_version(&db.conn).unwrap(), MIGRATIONS.len());
        // 무손실: v20 데이터가 그대로 남아 있다
        assert_eq!(db.list_credentials().unwrap().len(), 1);
        // 새 테이블이 실제로 사용 가능 (upsert/list/touch/delete round-trip)
        db.upsert_web_push_subscription("https://push.example/a", "p256", "auth", 100)
            .unwrap();
        let subs = db.list_web_push_subscriptions().unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].endpoint, "https://push.example/a");
        assert_eq!(subs[0].p256dh, "p256");
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn v22에서_structured_threads_마이그레이션이_적용된다() {
        let dir =
            std::env::temp_dir().join(format!("deppy-mig-22-structured-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..22] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 22).unwrap();
            conn.execute(
                "INSERT INTO workspaces (id, name, path, created_at, updated_at)
                 VALUES ('ws-1', 'existing', '/repo', 't', 't')",
                [],
            )
            .unwrap();
        }

        let db = Db::open(&path).unwrap();
        assert_eq!(Db::read_user_version(&db.conn).unwrap(), MIGRATIONS.len());
        db.upsert_structured_thread(
            "local-1", "ws-1", "thread-1", "restored", "/repo", None, false, false,
        )
        .unwrap();
        assert_eq!(
            db.list_structured_threads("ws-1", false).unwrap()[0].thread_id,
            "thread-1"
        );
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn 웹푸시_구독_등록_갱신_삭제_왕복() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.count_web_push_subscriptions().unwrap(), 0);
        db.upsert_web_push_subscription("https://push/x", "key1", "auth1", 10)
            .unwrap();
        db.upsert_web_push_subscription("https://push/y", "key2", "auth2", 20)
            .unwrap();
        assert_eq!(db.count_web_push_subscriptions().unwrap(), 2);
        // 같은 endpoint 재구독은 키만 갱신하고 행 수는 그대로(created_at 보존)
        db.upsert_web_push_subscription("https://push/x", "key1b", "auth1b", 99)
            .unwrap();
        assert_eq!(db.count_web_push_subscriptions().unwrap(), 2);
        let list = db.list_web_push_subscriptions().unwrap();
        // created_at 정렬: x(10) 먼저, y(20) 다음 — 재구독이 created_at을 바꾸지 않았다
        assert_eq!(list[0].endpoint, "https://push/x");
        assert_eq!(list[0].p256dh, "key1b");
        assert_eq!(list[0].auth, "auth1b");
        // 발송 성공 시각 갱신은 목록에 영향 없음
        db.touch_web_push_subscription("https://push/x", 12345)
            .unwrap();
        // 410 정리: 죽은 구독 삭제
        db.delete_web_push_subscription("https://push/x").unwrap();
        assert_eq!(db.count_web_push_subscriptions().unwrap(), 1);
        assert_eq!(
            db.list_web_push_subscriptions().unwrap()[0].endpoint,
            "https://push/y"
        );
    }

    #[test]
    fn bounded_web_push는_exact_limit과_plus_one을_구분한다() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_web_push_subscription("https://push/a", "key-a", "auth-a", 10)
            .unwrap();
        db.upsert_web_push_subscription("https://push/b", "key-b", "auth-b", 20)
            .unwrap();
        assert_eq!(db.list_web_push_subscriptions_bounded(2).unwrap().len(), 2);
        assert_eq!(
            db.list_web_push_subscriptions_bounded(1)
                .unwrap_err()
                .to_string(),
            BOUNDED_READ_LIMIT_EXCEEDED
        );
    }

    #[test]
    fn bounded_web_push는_corrupt_type을_materialize하지_않는다() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_web_push_subscription("https://push/a", "key-a", "auth-a", 10)
            .unwrap();
        db.conn
            .execute(
                "UPDATE web_push_subscriptions SET auth = CAST(x'7879' AS BLOB)
                  WHERE endpoint = 'https://push/a'",
                [],
            )
            .unwrap();
        assert_eq!(
            db.list_web_push_subscriptions_bounded(1)
                .unwrap_err()
                .to_string(),
            BOUNDED_READ_ROW_INVALID
        );
    }

    #[test]
    fn bounded_web_push는_aggregate_byte_budget을_넘기지_않는다() {
        let db = Db::open_in_memory().unwrap();
        for index in 0..WEB_PUSH_SUBSCRIPTION_ROWS_MAX {
            let prefix = format!("endpoint-{index}-");
            let endpoint = format!(
                "{prefix}{}",
                "e".repeat(BOUNDED_TEXT_BYTES_MAX - prefix.len())
            );
            db.upsert_web_push_subscription(
                &endpoint,
                &"p".repeat(BOUNDED_TEXT_BYTES_MAX),
                &"a".repeat(BOUNDED_TEXT_BYTES_MAX),
                index as i64,
            )
            .unwrap();
        }
        assert_eq!(
            db.list_web_push_subscriptions_bounded(WEB_PUSH_SUBSCRIPTION_ROWS_MAX)
                .unwrap_err()
                .to_string(),
            BOUNDED_READ_ROW_INVALID
        );
    }

    #[test]
    fn connector_repository_inventory와_tool_page는_bounded_complete_snapshot을_제공한다() {
        let mut db = Db::open_in_memory().unwrap();
        db.save_mcp_server(&sample_mcp_server("srv-page-a"))
            .unwrap();
        db.save_mcp_server(&sample_mcp_server("srv-page-b"))
            .unwrap();
        assert_eq!(db.mcp_server_inventory(2).unwrap().len(), 2);
        assert!(db.mcp_server_inventory(1).is_err());

        db.replace_mcp_tools(
            "srv-page-a",
            &[
                sample_mcp_tool("srv-page-a", "tool-b", "bravo"),
                sample_mcp_tool("srv-page-a", "tool-a", "alpha"),
            ],
        )
        .unwrap();
        db.upsert_permission_rule("srv-page-a", "bravo", "deny", None)
            .unwrap();
        assert_eq!(
            db.mcp_tool_name("srv-page-a", "tool-a").unwrap(),
            Some("alpha".to_owned())
        );
        let page = db.mcp_tool_page("srv-page-a", 0, 1).unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].name, "alpha");
        assert!(page.rows[0].permission.is_none());
        let second = db.mcp_tool_page("srv-page-a", 1, 1).unwrap();
        assert_eq!(second.rows[0].permission.as_ref().unwrap().rule, "deny");
    }

    #[test]
    fn server_delete는_active_agent_reference를_거부하고_audit을_보존한다() {
        let mut db = Db::open_in_memory().unwrap();
        let server_id = "srv-delete";
        db.insert_mcp_server(&sample_mcp_server(server_id)).unwrap();
        db.replace_mcp_tools(
            server_id,
            &[sample_mcp_tool(server_id, "tool-read", "read")],
        )
        .unwrap();
        db.upsert_permission_rule(server_id, "read", "allow", Some("hash-read"))
            .unwrap();
        db.insert_pending_approval("approval-delete", server_id, "read", "{}", None, 1, None)
            .unwrap();
        let audit_id = db
            .record_tool_audit(
                &audit::AuditRecord {
                    workspace_id: None,
                    session_id: None,
                    server_id: Some(server_id),
                    tool_name: "read",
                    input_json: "{}",
                    decision: audit::ToolDecision::AllowOnce,
                },
                &secret::RedactionService::new(),
                None,
            )
            .unwrap();
        let agent_id = db
            .insert_agent_config(
                "proxy-user",
                "safe-command",
                &[],
                None,
                None,
                None,
                None,
                true,
                Some(server_id),
                None,
            )
            .unwrap();

        assert!(db.delete_mcp_server(server_id, 10).is_err());
        assert!(db.mcp_server(server_id).unwrap().is_some());
        assert_eq!(db.list_mcp_tools(server_id).unwrap().len(), 1);
        assert_eq!(
            db.permission_rule(server_id, "read").unwrap().unwrap().rule,
            "allow"
        );
        assert_eq!(
            db.poll_approval("approval-delete").unwrap().status,
            ApprovalStatus::Pending
        );

        db.delete_agent_config(&agent_id).unwrap();
        assert!(db.delete_mcp_server(server_id, 20).unwrap());
        assert!(db.mcp_server(server_id).unwrap().is_none());
        assert!(db.list_mcp_tools(server_id).unwrap().is_empty());
        assert!(db.permission_rule(server_id, "read").unwrap().is_none());
        assert_eq!(
            db.poll_approval("approval-delete").unwrap(),
            ApprovalOutcome {
                status: ApprovalStatus::Denied,
                remember: false,
            }
        );
        let audit_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM tool_audit_logs WHERE id = ?1 AND server_id = ?2",
                (&audit_id, server_id),
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            audit_count, 1,
            "durable audit history must survive server deletion"
        );
    }

    #[test]
    fn server_delete_failure는_pending_permission_tool_server를_모두_rollback한다() {
        let mut db = Db::open_in_memory().unwrap();
        let server_id = "srv-delete-fail";
        db.insert_mcp_server(&sample_mcp_server(server_id)).unwrap();
        let tool = sample_mcp_tool(server_id, "tool-read-fail", "read");
        db.replace_mcp_tools(server_id, std::slice::from_ref(&tool))
            .unwrap();
        db.upsert_permission_rule(server_id, "read", "allow", Some("hash-read"))
            .unwrap();
        db.insert_pending_approval(
            "approval-delete-fail",
            server_id,
            "read",
            "{}",
            None,
            1,
            None,
        )
        .unwrap();
        db.conn
            .execute_batch(
                "CREATE TRIGGER fail_storage_server_delete BEFORE DELETE ON mcp_tools
                 BEGIN SELECT RAISE(ABORT, 'injected storage server delete failure'); END;",
            )
            .unwrap();

        assert!(db.delete_mcp_server(server_id, 10).is_err());
        assert_eq!(
            db.mcp_server(server_id).unwrap(),
            Some(sample_mcp_server(server_id))
        );
        assert_eq!(db.list_mcp_tools(server_id).unwrap(), vec![tool]);
        assert_eq!(
            db.permission_rule(server_id, "read").unwrap().unwrap().rule,
            "allow"
        );
        assert_eq!(
            db.poll_approval("approval-delete-fail").unwrap().status,
            ApprovalStatus::Pending
        );
    }

    #[test]
    fn bounded_env_projection은_sql_filter_order와_limit_plus_one을_강제한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace = db.ensure_default_workspace().unwrap();
        let dotenv = db.insert_env_profile(&workspace, ".env", "dotenv").unwrap();
        let local = db.insert_env_profile(&workspace, "local", "local").unwrap();
        let mut owned = sample("owned-env");
        owned.provider = "env".to_owned();
        let mut foreign = sample("foreign-provider");
        foreign.provider = "github".to_owned();
        let mut unreferenced_owned = sample("orphan-env");
        unreferenced_owned.provider = "env".to_owned();
        db.insert_credential(&owned).unwrap();
        db.insert_credential(&foreign).unwrap();
        db.insert_credential(&unreferenced_owned).unwrap();
        db.upsert_env_var(
            &dotenv,
            "OWNED",
            &EnvValue::Secret {
                credential_id: owned.id.clone(),
            },
        )
        .unwrap();
        db.upsert_env_var(
            &dotenv,
            "FOREIGN",
            &EnvValue::Secret {
                credential_id: foreign.id.clone(),
            },
        )
        .unwrap();
        db.upsert_env_var(&local, "PORT", &EnvValue::Plain("3000".to_owned()))
            .unwrap();

        assert_eq!(
            db.list_env_profiles_bounded(&workspace, 2).unwrap().len(),
            2
        );
        assert_eq!(
            db.list_env_profiles_bounded(&workspace, 1)
                .unwrap_err()
                .to_string(),
            BOUNDED_READ_LIMIT_EXCEEDED
        );
        assert_eq!(db.list_env_vars_bounded(&dotenv, 2).unwrap().len(), 2);
        assert_eq!(
            db.list_dotenv_owned_credential_ids_bounded(2).unwrap(),
            vec![unreferenced_owned.id, owned.id]
        );
        assert_eq!(db.env_api_project_counts_bounded(1).unwrap().len(), 1);
    }

    #[test]
    fn bounded_projection_empty_snapshot은_zero_limit으로_완전하다() {
        let db = Db::open_in_memory().unwrap();
        assert!(
            db.list_env_profiles_bounded("workspace", 0)
                .unwrap()
                .is_empty()
        );
        assert!(db.list_env_vars_bounded("profile", 0).unwrap().is_empty());
        assert!(
            db.list_dotenv_owned_credential_ids_bounded(0)
                .unwrap()
                .is_empty()
        );
        assert!(db.env_api_project_counts_bounded(0).unwrap().is_empty());
        assert!(
            db.list_hook_sessions_for_prefix_bounded("workspace:", 0)
                .unwrap()
                .is_empty()
        );
        assert!(
            db.list_statuslines_for_prefix_bounded("workspace:", 0)
                .unwrap()
                .is_empty()
        );
        assert!(
            db.list_turn_done_sessions_for_prefix_bounded("workspace:", 0)
                .unwrap()
                .is_empty()
        );
        assert!(db.list_waiting_sessions_bounded(0).unwrap().is_empty());
        assert!(
            db.list_agent_sessions_bounded("workspace", 0)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn bounded_env_projection은_4mib_exact와_plus_one을_구분한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace = db.ensure_default_workspace().unwrap();
        let profile = db
            .insert_env_profile(&workspace, "aggregate", "local")
            .unwrap();
        for index in 0..128 {
            let key = format!("K{index:03}");
            let value_bytes = BOUNDED_ROW_BYTES_MAX - key.len() - "plain".len();
            db.conn
                .execute(
                    "INSERT INTO env_vars
                        (id, profile_id, key, kind, plain_value, credential_id, created_at,
                         updated_at)
                     VALUES (?1, ?2, ?3, 'plain', ?4, NULL, ?1, ?1)",
                    (
                        format!("bounded-env-{index:03}"),
                        &profile,
                        &key,
                        "x".repeat(value_bytes),
                    ),
                )
                .unwrap();
        }
        assert_eq!(db.list_env_vars_bounded(&profile, 128).unwrap().len(), 128);

        let key = "K128";
        let value_bytes = BOUNDED_ROW_BYTES_MAX - key.len() - "plain".len();
        db.conn
            .execute(
                "INSERT INTO env_vars
                    (id, profile_id, key, kind, plain_value, credential_id, created_at,
                     updated_at)
                 VALUES ('bounded-env-128', ?1, ?2, 'plain', ?3, NULL,
                         'bounded-env-128', 'bounded-env-128')",
                (&profile, key, "x".repeat(value_bytes)),
            )
            .unwrap();
        assert_eq!(
            db.list_env_vars_bounded(&profile, 129)
                .unwrap_err()
                .to_string(),
            BOUNDED_READ_ROW_INVALID
        );
    }

    #[test]
    fn bounded_projection은_corrupt_sqlite_type과_huge_blob을_preflight에서_거부한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace = db.ensure_default_workspace().unwrap();
        let profile = db.insert_env_profile(&workspace, "valid", "local").unwrap();
        db.conn
            .execute(
                "UPDATE env_profiles SET name = x'ff' WHERE id = ?1",
                [&profile],
            )
            .unwrap();
        assert_eq!(
            db.list_env_profiles_bounded(&workspace, 1)
                .unwrap_err()
                .to_string(),
            BOUNDED_READ_ROW_INVALID
        );

        db.upsert_statusline("prefix:session", Some("effort"), Some("model"), Some(50))
            .unwrap();
        db.conn
            .execute(
                "UPDATE agent_statusline SET model = zeroblob(5000)
                  WHERE session_key = 'prefix:session'",
                [],
            )
            .unwrap();
        assert_eq!(
            db.list_statuslines_for_prefix_bounded("prefix:", 1)
                .unwrap_err()
                .to_string(),
            BOUNDED_READ_ROW_INVALID
        );
    }

    #[test]
    fn hook_prefix_bounded_reads는_percent와_underscore를_literal로_격리한다() {
        let db = Db::open_in_memory().unwrap();
        for key in ["ws%:one", "wsX:two", "ws_:three"] {
            db.upsert_hook_session(key, "claude", &format!("agent-{key}"), "/tmp")
                .unwrap();
            db.upsert_statusline(key, Some("high"), Some("model"), Some(50))
                .unwrap();
            db.set_agent_turn_done(key).unwrap();
        }
        db.set_agent_needs_input("waiting:one", true, Some("first"))
            .unwrap();
        db.set_agent_needs_input("waiting:two", true, Some("second"))
            .unwrap();

        assert_eq!(
            db.list_hook_sessions_for_prefix_bounded("ws%:", 1).unwrap()[0].session_key,
            "ws%:one"
        );
        assert_eq!(
            db.list_statuslines_for_prefix_bounded("ws_:", 1).unwrap()[0].session_key,
            "ws_:three"
        );
        assert_eq!(
            db.list_turn_done_sessions_for_prefix_bounded("wsX:", 1)
                .unwrap()[0]
                .0,
            "wsX:two"
        );
        assert_eq!(
            db.list_hook_sessions_for_prefix_bounded("ws", 2)
                .unwrap_err()
                .to_string(),
            BOUNDED_READ_LIMIT_EXCEEDED
        );
        assert_eq!(db.list_waiting_sessions_bounded(2).unwrap().len(), 2);
        assert_eq!(
            db.list_waiting_sessions_bounded(1).unwrap_err().to_string(),
            BOUNDED_READ_LIMIT_EXCEEDED
        );
    }

    #[test]
    fn hook_state_4097번째_write는_원자적으로_oldest를_evict한다() {
        let db = Db::open_in_memory().unwrap();
        db.conn
            .execute_batch(
                "WITH RECURSIVE seq(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n < 4095)
                 INSERT INTO agent_hook_sessions
                    (session_key, kind, agent_session_id, transcript_path, updated_at)
                 SELECT printf('workspace-%04d:session', n), 'claude', printf('agent-%04d', n), '/tmp', 0
                   FROM seq;
                 WITH RECURSIVE seq(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n < 4095)
                 INSERT INTO agent_needs_input
                    (session_key, waiting, turn_done, updated_at, message)
                 SELECT printf('workspace-%04d:session', n), 1, 0, 0, NULL FROM seq;
                 WITH RECURSIVE seq(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n < 4095)
                 INSERT INTO agent_statusline
                    (session_key, effort, model, context_pct, updated_at)
                 SELECT printf('workspace-%04d:session', n), NULL, NULL, NULL, 0 FROM seq;",
            )
            .unwrap();

        db.upsert_hook_session("new-workspace:new-hook", "claude", "new-agent", "/tmp")
            .unwrap();
        db.set_agent_needs_input("new-workspace:new-needs", true, Some("waiting"))
            .unwrap();
        db.upsert_statusline("new-workspace:new-status", None, None, None)
            .unwrap();
        for (table, new_key) in [
            ("agent_hook_sessions", "new-workspace:new-hook"),
            ("agent_needs_input", "new-workspace:new-needs"),
            ("agent_statusline", "new-workspace:new-status"),
        ] {
            let count: i64 = db
                .conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            let retained: i64 = db
                .conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE session_key = ?1"),
                    [new_key],
                    |row| row.get(0),
                )
                .unwrap();
            let oldest: i64 = db
                .conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {table} \
                         WHERE session_key = 'workspace-0000:session'"
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, HOOK_STATE_ROWS_MAX as i64);
            assert_eq!(retained, 1);
            assert_eq!(oldest, 0);
        }
    }

    #[test]
    fn hook_state_prefix_257번째_write는_같은_workspace의_oldest만_evict한다() {
        let db = Db::open_in_memory().unwrap();
        db.conn
            .execute_batch(
                "WITH RECURSIVE seq(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n < 255)
                 INSERT INTO agent_hook_sessions
                    (session_key, kind, agent_session_id, transcript_path, updated_at)
                 SELECT printf('same_:seed-%03d', n), 'claude', printf('agent-%03d', n), '/tmp',
                        CAST(strftime('%s','now') AS INTEGER)
                   FROM seq;
                 WITH RECURSIVE seq(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n < 255)
                 INSERT INTO agent_needs_input
                    (session_key, waiting, turn_done, updated_at, message)
                 SELECT printf('same_:seed-%03d', n), 1, 1,
                        CAST(strftime('%s','now') AS INTEGER), NULL FROM seq;
                 WITH RECURSIVE seq(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n < 255)
                 INSERT INTO agent_statusline
                    (session_key, effort, model, context_pct, updated_at)
                 SELECT printf('same_:seed-%03d', n), NULL, NULL, NULL,
                        CAST(strftime('%s','now') AS INTEGER) FROM seq;
                 INSERT INTO agent_hook_sessions
                    (session_key, kind, agent_session_id, transcript_path, updated_at)
                 VALUES ('sameX:other', 'claude', 'other-agent', '/other',
                         CAST(strftime('%s','now') AS INTEGER));
                 INSERT INTO agent_needs_input
                    (session_key, waiting, turn_done, updated_at, message)
                 VALUES ('sameX:other', 1, 0, CAST(strftime('%s','now') AS INTEGER), NULL);
                 INSERT INTO agent_statusline
                    (session_key, effort, model, context_pct, updated_at)
                 VALUES ('sameX:other', NULL, NULL, NULL,
                         CAST(strftime('%s','now') AS INTEGER));",
            )
            .unwrap();

        for table in [
            "agent_hook_sessions",
            "agent_needs_input",
            "agent_statusline",
        ] {
            let count: i64 = db
                .conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {table}
                          WHERE substr(CAST(session_key AS BLOB), 1, length(CAST(?1 AS BLOB)))
                                = CAST(?1 AS BLOB)"
                    ),
                    ["same_:"],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, HOOK_PREFIX_ROWS_MAX as i64);
        }
        assert_eq!(
            db.list_hook_sessions_for_prefix_bounded("same_:", HOOK_PREFIX_ROWS_MAX)
                .unwrap()
                .len(),
            HOOK_PREFIX_ROWS_MAX
        );
        assert_eq!(
            db.list_statuslines_for_prefix_bounded("same_:", HOOK_PREFIX_ROWS_MAX)
                .unwrap()
                .len(),
            HOOK_PREFIX_ROWS_MAX
        );
        assert_eq!(
            db.list_turn_done_sessions_for_prefix_bounded("same_:", HOOK_PREFIX_ROWS_MAX)
                .unwrap()
                .len(),
            HOOK_PREFIX_ROWS_MAX
        );

        db.upsert_hook_session("same_:new-hook", "claude", "new-agent", "/tmp")
            .unwrap();
        db.set_agent_needs_input("same_:new-needs", true, Some("waiting"))
            .unwrap();
        db.upsert_statusline("same_:new-status", None, None, None)
            .unwrap();

        for (table, new_key) in [
            ("agent_hook_sessions", "same_:new-hook"),
            ("agent_needs_input", "same_:new-needs"),
            ("agent_statusline", "same_:new-status"),
        ] {
            let prefix_count: i64 = db
                .conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {table}
                          WHERE substr(CAST(session_key AS BLOB), 1, length(CAST(?1 AS BLOB)))
                                = CAST(?1 AS BLOB)"
                    ),
                    ["same_:"],
                    |row| row.get(0),
                )
                .unwrap();
            let retained: i64 = db
                .conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE session_key = ?1"),
                    [new_key],
                    |row| row.get(0),
                )
                .unwrap();
            let oldest: i64 = db
                .conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE session_key = 'same_:seed-000'"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let other_workspace: i64 = db
                .conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE session_key = 'sameX:other'"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(prefix_count, HOOK_PREFIX_ROWS_MAX as i64);
            assert_eq!(retained, 1);
            assert_eq!(oldest, 0);
            assert_eq!(other_workspace, 1);
        }
        assert_eq!(
            db.list_hook_sessions_for_prefix_bounded("same_:", HOOK_PREFIX_ROWS_MAX)
                .unwrap()
                .len(),
            HOOK_PREFIX_ROWS_MAX
        );
        assert_eq!(
            db.list_statuslines_for_prefix_bounded("same_:", HOOK_PREFIX_ROWS_MAX)
                .unwrap()
                .len(),
            HOOK_PREFIX_ROWS_MAX
        );

        db.set_agent_turn_done("same_:new-turn").unwrap();
        let turn_done = db
            .list_turn_done_sessions_for_prefix_bounded("same_:", HOOK_PREFIX_ROWS_MAX)
            .unwrap();
        let seen_at = turn_done
            .iter()
            .find_map(|(session_key, updated_at)| {
                (session_key == "same_:new-turn").then_some(*updated_at)
            })
            .unwrap();
        db.clear_agent_turn_done("same_:new-turn", seen_at).unwrap();
        assert!(
            db.list_turn_done_sessions_for_prefix_bounded("same_:", HOOK_PREFIX_ROWS_MAX)
                .unwrap()
                .iter()
                .all(|(session_key, _)| session_key != "same_:new-turn")
        );
        db.clear_agent_turn_done("same_:missing", i64::MAX).unwrap();
        let needs_prefix_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM agent_needs_input
                  WHERE substr(CAST(session_key AS BLOB), 1, length(CAST(?1 AS BLOB)))
                        = CAST(?1 AS BLOB)",
                ["same_:"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(needs_prefix_count, HOOK_PREFIX_ROWS_MAX as i64);
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM agent_needs_input
                      WHERE session_key = 'sameX:other'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn hook_state_prefix_eviction_실패는_insert까지_rollback한다() {
        let db = Db::open_in_memory().unwrap();
        db.conn
            .execute_batch(
                "WITH RECURSIVE seq(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n < 255)
                 INSERT INTO agent_hook_sessions
                    (session_key, kind, agent_session_id, transcript_path, updated_at)
                 SELECT printf('rollback:seed-%03d', n), 'claude', printf('agent-%03d', n), '/tmp', 0
                   FROM seq;
                 CREATE TRIGGER fail_prefix_evict
                 BEFORE DELETE ON agent_hook_sessions
                 WHEN OLD.session_key = 'rollback:seed-000'
                 BEGIN
                    SELECT RAISE(ABORT, 'injected prefix eviction failure');
                 END;",
            )
            .unwrap();

        assert_eq!(
            db.upsert_hook_session("rollback:new", "claude", "new-agent", "/new")
                .unwrap_err()
                .to_string(),
            BOUNDED_WRITE_FAILED
        );
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM agent_hook_sessions
                      WHERE substr(CAST(session_key AS BLOB), 1,
                                   length(CAST('rollback:' AS BLOB)))
                            = CAST('rollback:' AS BLOB)",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            HOOK_PREFIX_ROWS_MAX as i64
        );
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM agent_hook_sessions
                      WHERE session_key = 'rollback:new'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM agent_hook_sessions
                      WHERE session_key = 'rollback:seed-000'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn hook_state_writer는_bounded_workspace_prefix를_요구한다() {
        let db = Db::open_in_memory().unwrap();
        for result in [
            db.upsert_hook_session("bare-key", "claude", "agent", "/tmp"),
            db.set_agent_needs_input("workspace:", true, None),
            db.set_agent_turn_done(":session"),
            db.upsert_statusline("workspace:\u{7f}session", None, None, None),
        ] {
            assert_eq!(result.unwrap_err().to_string(), BOUNDED_WRITE_INPUT_INVALID);
        }
    }

    #[test]
    fn oversized_hook_payload는_existing_row를_변경하지_않는다() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_hook_session("workspace:stable", "claude", "agent", "/stable")
            .unwrap();
        assert_eq!(
            db.upsert_hook_session(
                "workspace:stable",
                "claude",
                "agent",
                &"x".repeat(BOUNDED_TEXT_BYTES_MAX + 1),
            )
            .unwrap_err()
            .to_string(),
            BOUNDED_WRITE_INPUT_INVALID
        );
        let row = db
            .conn
            .query_row(
                "SELECT transcript_path FROM agent_hook_sessions \
                 WHERE session_key = 'workspace:stable'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert_eq!(row, "/stable");
    }

    #[test]
    fn agent_session_workspace_cap은_256_exact만_admit한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace = db.ensure_default_workspace().unwrap();
        for index in 0..AGENT_SESSION_ROWS_MAX {
            db.upsert_agent_session(
                &workspace,
                &format!("pane-{index:03}"),
                "claude",
                &format!("session-{index:03}"),
            )
            .unwrap();
        }
        assert_eq!(
            db.list_agent_sessions_bounded(&workspace, AGENT_SESSION_ROWS_MAX)
                .unwrap()
                .len(),
            AGENT_SESSION_ROWS_MAX
        );
        assert_eq!(
            db.upsert_agent_session(&workspace, "pane-overflow", "claude", "session-overflow")
                .unwrap_err()
                .to_string(),
            AGENT_SESSION_CAPACITY_EXCEEDED
        );
        db.upsert_agent_session(&workspace, "pane-000", "codex", "session-updated")
            .unwrap();
        assert_eq!(
            db.list_agent_sessions_bounded(&workspace, AGENT_SESSION_ROWS_MAX)
                .unwrap()
                .len(),
            AGENT_SESSION_ROWS_MAX
        );
    }

    #[test]
    fn bounded_projection_sql_source_laws() {
        for preflight in [
            ENV_PROFILES_BOUNDED_PREFLIGHT,
            ENV_VARS_BOUNDED_PREFLIGHT,
            DOTENV_CREDENTIALS_BOUNDED_PREFLIGHT,
            ENV_API_COUNTS_BOUNDED_PREFLIGHT,
            HOOK_SESSIONS_PREFIX_PREFLIGHT,
            STATUSLINES_PREFIX_PREFLIGHT,
            TURN_DONE_PREFIX_PREFLIGHT,
            WAITING_SESSIONS_PREFLIGHT,
            AGENT_SESSIONS_BOUNDED_PREFLIGHT,
            ACTIVITY_PANES_BOUNDED_PREFLIGHT,
            WEB_PUSH_BOUNDED_PREFLIGHT,
        ] {
            assert!(preflight.contains("selected AS MATERIALIZED"));
            assert!(preflight.contains("typeof("));
            assert!(preflight.contains("LIMIT ?"));
            assert!(preflight.contains("row_bytes"));
        }
        for prefix_query in [
            HOOK_SESSIONS_PREFIX_PREFLIGHT,
            HOOK_SESSIONS_PREFIX_SELECT,
            STATUSLINES_PREFIX_PREFLIGHT,
            STATUSLINES_PREFIX_SELECT,
            TURN_DONE_PREFIX_PREFLIGHT,
            TURN_DONE_PREFIX_SELECT,
        ] {
            assert!(prefix_query.contains("substr(CAST(session_key AS BLOB)"));
            assert!(!prefix_query.to_ascii_uppercase().contains(" LIKE "));
        }
        for (preflight, projection, cutoff) in [
            (
                HOOK_SESSIONS_PREFIX_PREFLIGHT,
                HOOK_SESSIONS_PREFIX_SELECT,
                "86400",
            ),
            (
                STATUSLINES_PREFIX_PREFLIGHT,
                STATUSLINES_PREFIX_SELECT,
                "3600",
            ),
            (TURN_DONE_PREFIX_PREFLIGHT, TURN_DONE_PREFIX_SELECT, "86400"),
            (WAITING_SESSIONS_PREFLIGHT, WAITING_SESSIONS_SELECT, "86400"),
            (
                TURN_DONE_SESSIONS_PREFLIGHT,
                TURN_DONE_SESSIONS_SELECT,
                "86400",
            ),
        ] {
            assert!(!preflight.contains("strftime"));
            assert!(!projection.contains("strftime"));
            assert!(preflight.contains(&format!("- {cutoff}")));
            assert!(projection.contains(&format!("- {cutoff}")));
            assert!(preflight.contains("updated_at > ?"));
            assert!(projection.contains("updated_at > ?"));
        }
        let source = include_str!("db.rs");
        for method in [
            "list_persisted_activity_panes_bounded",
            "list_web_push_subscriptions_bounded",
        ] {
            let body = source
                .split_once(&format!("pub fn {method}"))
                .unwrap()
                .1
                .split("\n    pub fn ")
                .next()
                .unwrap();
            let preflight_at = body.find("bounded_read_preflight").unwrap();
            let vector_at = body.find("Vec::with_capacity").unwrap();
            assert!(preflight_at < vector_at);
            assert!(body.contains("unchecked_transaction"));
            assert!(body.contains("sql_limit"));
        }
        let activity_body = source
            .split_once("pub fn list_persisted_activity_panes_bounded")
            .unwrap()
            .1
            .split("\n    pub fn ")
            .next()
            .unwrap();
        let activity_before_push = &activity_body[..activity_body.find("result.push").unwrap()];
        for validation in [
            "let workspace_id = bounded_required_text(row, 0",
            "let pane_id = bounded_required_text(row, 1",
            "let title = bounded_required_text(row, 2",
            "let cwd = bounded_required_text(row, 3",
        ] {
            assert!(activity_before_push.contains(validation));
        }
        assert_eq!(
            activity_before_push
                .matches("bounded_required_text(")
                .count(),
            4
        );
        let web_push_body = source
            .split_once("pub fn list_web_push_subscriptions_bounded")
            .unwrap()
            .1
            .split("\n    pub fn ")
            .next()
            .unwrap();
        let web_push_before_push = &web_push_body[..web_push_body.find("result.push").unwrap()];
        assert_eq!(
            web_push_before_push
                .matches("bounded_required_text(")
                .count(),
            3
        );
        for (preflight, projection) in [
            (
                ACTIVITY_PANES_BOUNDED_PREFLIGHT,
                ACTIVITY_PANES_BOUNDED_SELECT,
            ),
            (WEB_PUSH_BOUNDED_PREFLIGHT, WEB_PUSH_BOUNDED_SELECT),
        ] {
            assert!(preflight.contains("ORDER BY"));
            assert!(projection.contains("ORDER BY"));
            assert!(preflight.contains("rowid LIMIT ?"));
            assert!(projection.contains("rowid LIMIT ?"));
        }
        assert!(ACTIVITY_PANES_BOUNDED_PREFLIGHT.contains("length(CAST(pane.id AS BLOB))"));
        assert!(ACTIVITY_PANES_BOUNDED_SELECT.contains("pane.id"));
        for method in [
            "list_hook_sessions_for_prefix_bounded",
            "list_statuslines_for_prefix_bounded",
            "list_turn_done_sessions_for_prefix_bounded",
            "list_waiting_sessions_bounded",
        ] {
            let body = source
                .split_once(&format!("pub fn {method}"))
                .unwrap()
                .1
                .split("\n    pub fn ")
                .next()
                .unwrap();
            assert_eq!(body.matches("bounded_snapshot_epoch(&tx)?").count(), 1);
        }
        for method in [
            "upsert_hook_session",
            "set_agent_needs_input",
            "set_agent_turn_done",
            "clear_agent_turn_done",
            "upsert_statusline",
        ] {
            let body = source
                .split_once(&format!("pub fn {method}"))
                .unwrap()
                .1
                .split("\n    pub fn ")
                .next()
                .unwrap();
            assert_eq!(
                body.matches("bounded_session_key_prefix(session_key)?")
                    .count(),
                1,
                "{method} must validate exactly one workspace prefix"
            );
            assert_eq!(
                body.matches("evict_hook_state_prefix_overflow(").count(),
                1,
                "{method} must enforce the prefix cap in its write transaction"
            );
            assert_eq!(
                body.matches("evict_hook_state_overflow(").count(),
                1,
                "{method} must retain the global cap"
            );
        }
        let prefix_evict_body = source
            .split_once("fn evict_hook_state_prefix_overflow")
            .unwrap()
            .1
            .split("\n/// hook")
            .next()
            .unwrap();
        assert!(prefix_evict_body.contains("substr(CAST(session_key AS BLOB)"));
        assert!(!prefix_evict_body.to_ascii_uppercase().contains(" LIKE "));
        assert!(prefix_evict_body.contains("ORDER BY updated_at DESC"));
        assert!(prefix_evict_body.contains("THEN ?3 - 1 ELSE ?3 END"));
        assert!(DOTENV_CREDENTIALS_BOUNDED_PREFLIGHT.contains("credential.provider = 'env'"));
        assert!(DOTENV_CREDENTIALS_BOUNDED_SELECT.contains("credential.provider = 'env'"));
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

    fn seed_settings_workspaces(conn: &Connection, count: usize) {
        conn.execute(
            "WITH RECURSIVE seq(n) AS (
                 SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n + 1 < ?1
             )
             INSERT INTO workspaces (id, name, path, created_at, updated_at)
             SELECT printf('write-workspace-%04d', n), 'workspace', '',
                    printf('%08d', n), printf('%08d', n)
             FROM seq",
            [i64::try_from(count).unwrap()],
        )
        .unwrap();
    }

    fn assert_static_settings_error(error: anyhow::Error, expected: &str) {
        assert_eq!(format!("{error:#}"), expected);
    }

    fn query_count(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> usize {
        let count: i64 = conn.query_row(sql, params, |row| row.get(0)).unwrap();
        usize::try_from(count).unwrap()
    }

    fn insert_workspace_with_logical_bytes(
        conn: &Connection,
        id: &str,
        path: &str,
        target_bytes: usize,
    ) {
        let fixed_bytes = id.len() + path.len() + "t".len() + "7".len() + "9".len();
        let name = "x".repeat(target_bytes.checked_sub(fixed_bytes).unwrap());
        conn.execute(
            "INSERT INTO workspaces
                (id, name, path, created_at, updated_at, path_dev, path_ino)
             VALUES (?1, ?2, ?3, 't', 't', 7, 9)",
            (id, name, path),
        )
        .unwrap();
    }

    fn seed_exact_workspace_update_budget(conn: &Connection) -> &'static str {
        let target_id = "settings-update-target";
        let target_name = "n";
        let target_path = "/old";
        let target_bytes = target_id.len()
            + target_name.len()
            + target_path.len()
            + "t".len()
            + "7".len()
            + "9".len();
        conn.execute(
            "INSERT INTO workspaces
                (id, name, path, created_at, updated_at, path_dev, path_ino)
             VALUES (?1, ?2, ?3, 't', 't', 7, 9)",
            (target_id, target_name, target_path),
        )
        .unwrap();
        for index in 0..3 {
            insert_workspace_with_logical_bytes(
                conn,
                &format!("settings-update-full-{index}"),
                "/filler",
                SETTINGS_ROW_BYTES_MAX,
            );
        }
        insert_workspace_with_logical_bytes(
            conn,
            "settings-update-tail",
            "/tail",
            SETTINGS_ROW_BYTES_MAX - target_bytes,
        );
        target_id
    }

    #[test]
    fn settings_global_write_admission은_workspace수에따른_query_loop가없다() {
        let source = include_str!("db.rs");
        let function_body = |name: &str| {
            source
                .split_once(&format!("fn {name}("))
                .unwrap()
                .1
                .split("\nfn ")
                .next()
                .unwrap()
        };
        let agent = function_body("settings_agent_write_admission");
        assert!(agent.contains("settings_all_profile_groups_probe"));
        assert!(!agent.contains("for workspace"));
        let credential = function_body("settings_credential_candidate_write_admission");
        assert!(credential.contains("settings_global_credential_write_admission"));
        assert!(!credential.contains("for workspace"));
        assert_eq!(
            source.matches("settings_workspace_ids_for_write").count(),
            1
        );
        assert!(SETTINGS_GLOBAL_CREDENTIAL_WRITE_PREFLIGHT.contains("effective_scopes"));
        assert_eq!(
            SETTINGS_GLOBAL_CREDENTIAL_WRITE_PREFLIGHT
                .matches("GROUP BY workspace_id")
                .count(),
            3
        );
        assert_eq!(
            SETTINGS_GLOBAL_CREDENTIAL_WRITE_PREFLIGHT
                .matches("AS MATERIALIZED")
                .count(),
            5
        );
        assert_eq!(
            function_body("settings_global_credential_write_admission")
                .matches("query_map(")
                .count(),
            1
        );
    }

    #[test]
    fn settings_workspace_updates는_mutation전에aggregate_plus_one을거부한다() {
        let db = Db::open_in_memory().unwrap();
        let target = seed_exact_workspace_update_budget(&db.conn);
        assert_eq!(db.settings_workspace_projection_rows().unwrap().len(), 5);
        db.conn
            .execute_batch(
                "CREATE TEMP TRIGGER mutation_must_not_run
                 BEFORE UPDATE ON workspaces
                 WHEN OLD.id = 'settings-update-target'
                 BEGIN SELECT RAISE(ABORT, 'workspace mutation reached'); END;",
            )
            .unwrap();
        let anchor = WorkspaceFolderAnchor { dev: 7, ino: 9 };

        let errors = [
            db.rename_workspace(target, "nn").unwrap_err(),
            db.set_workspace_path(target, "/oldx").unwrap_err(),
            db.set_workspace_path_and_anchor(target, "/oldx", Some(7), Some(9))
                .unwrap_err(),
            db.set_workspace_anchor(target, Some(10), Some(9))
                .unwrap_err(),
            db.update_workspace_moved_path_cas(target, "/old", anchor, "/oldx", anchor)
                .unwrap_err(),
        ];
        for error in errors {
            assert_static_settings_error(error, "settings_workspace_write_retained_bytes_limit");
        }
        assert_eq!(db.workspace_path(target).unwrap().as_deref(), Some("/old"));
        assert_eq!(db.workspace_anchor(target).unwrap(), Some((7, 9)));
        let name: String = db
            .conn
            .query_row(
                "SELECT name FROM workspaces WHERE id = ?1",
                [target],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(name, "n");
    }

    #[test]
    fn settings_update_source_laws_require_pre_mutation_admission() {
        let source = include_str!("db.rs");
        let public_body = |name: &str| {
            source
                .split_once(&format!("pub fn {name}("))
                .unwrap()
                .1
                .split("\n    pub fn ")
                .next()
                .unwrap()
        };
        assert!(
            public_body("set_workspace_path_and_anchor")
                .contains("self.set_workspace_path_and_anchor_with_volume(")
        );
        for method in [
            "rename_workspace",
            "set_workspace_path",
            "set_workspace_path_and_anchor_with_volume",
            "set_workspace_anchor",
            "update_workspace_moved_path_cas",
        ] {
            let body = public_body(method);
            assert!(body.contains("TransactionBehavior::Immediate"), "{method}");
            let admission = body.find("settings_workspace_update_admission").unwrap();
            let mutation = body.find(".execute(").unwrap();
            assert!(admission < mutation, "{method}");
        }
        let publish = source
            .split_once("fn publish_credential_secret_slot_in_transaction(")
            .unwrap()
            .1
            .split("\n    fn ")
            .next()
            .unwrap();
        let admission = publish
            .find("settings_credential_publish_admission")
            .unwrap();
        assert!(admission < publish.find("orphan_published_physical_slot").unwrap());
        assert!(admission < publish.find("\"UPDATE credentials").unwrap());
        for method in [
            "rotate_credential_secret_slot",
            "publish_legacy_credential_secret_slot_cas",
            "publish_credential_secret_slot_cas",
            "publish_credential_secret_slot_revision_cas",
        ] {
            assert!(
                public_body(method).contains("publish_credential_secret_slot_in_transaction"),
                "{method}"
            );
        }
        let credential_admission = source
            .split_once("fn settings_credential_publish_admission(")
            .unwrap()
            .1
            .split("\nfn ")
            .next()
            .unwrap();
        let byte_guard = credential_admission
            .find("candidate_bytes > SETTINGS_ROW_BYTES_MAX")
            .unwrap();
        let clone = credential_admission
            .find("masked_hint.map(str::to_owned)")
            .unwrap();
        assert!(
            byte_guard < clone,
            "masked_hint clone must follow its byte guard"
        );
    }

    #[test]
    fn credential_slot_publish는_masked_hint_admission전에아무것도변경하지않는다() {
        let db = Db::open_in_memory().unwrap();
        let logical = secret::LogicalCredentialId::new("publish-admission").unwrap();
        let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        let mut meta = sample(logical.as_str());
        let fixed_bytes =
            meta.id.len() + meta.provider.len() + meta.label.len() + meta.credential_kind.len();
        let old_hint_bytes = SETTINGS_ROW_BYTES_MAX - fixed_bytes;
        meta.masked_hint = Some("x".repeat(old_hint_bytes));
        db.insert_credential(&meta).unwrap();
        stage_slot(&db, &logical, &slot);
        db.conn
            .execute_batch(
                "CREATE TEMP TRIGGER credential_mutation_must_not_run
                 BEFORE UPDATE ON credentials
                 WHEN OLD.id = 'publish-admission'
                 BEGIN SELECT RAISE(ABORT, 'credential mutation reached'); END;",
            )
            .unwrap();

        let oversized_hint = "x".repeat(old_hint_bytes + 1);
        let error = db
            .rotate_credential_secret_slot(
                logical.as_str(),
                slot.as_str(),
                r#"{"server_id":"server"}"#,
                Some(&oversized_hint),
            )
            .unwrap_err();

        assert_static_settings_error(error, "settings_credential_write_row_bytes_limit");
        assert_eq!(
            db.credential_secret_location(logical.as_str())
                .unwrap()
                .unwrap()
                .keyring_username,
            logical.as_str()
        );
        let (hint_bytes, oauth_json): (i64, Option<String>) = db
            .conn
            .query_row(
                "SELECT length(CAST(masked_hint AS BLOB)), oauth_json
                 FROM credentials WHERE id = ?1",
                [logical.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(usize::try_from(hint_bytes).unwrap(), old_hint_bytes);
        assert!(oauth_json.is_none());
        let state: String = db
            .conn
            .query_row(
                "SELECT state FROM physical_secret_slot_ledger WHERE physical_slot = ?1",
                [slot.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "staging");
    }

    #[test]
    fn settings_workspace_write는_max_plus_one을_insert전에_거부한다() {
        let db = Db::open_in_memory().unwrap();
        seed_settings_workspaces(&db.conn, SETTINGS_WORKSPACE_LIMIT_MAX);

        let error = db.create_workspace("must-not-persist").unwrap_err();

        assert_static_settings_error(error, "settings_workspace_write_item_limit");
        let count = query_count(&db.conn, "SELECT COUNT(*) FROM workspaces", []);
        assert_eq!(count, SETTINGS_WORKSPACE_LIMIT_MAX);
    }

    #[test]
    fn settings_workspace_exact_path는_cap에서도_existing을반환하고_new만거부한다() {
        let db = Db::open_in_memory().unwrap();
        seed_settings_workspaces(&db.conn, SETTINGS_WORKSPACE_LIMIT_MAX);
        db.conn
            .execute(
                "UPDATE workspaces
                 SET path = '/existing', path_dev = 7, path_ino = 9
                 WHERE id = 'write-workspace-0000'",
                [],
            )
            .unwrap();
        let anchor = WorkspaceFolderAnchor { dev: 7, ino: 9 };

        let existing = db
            .find_or_create_workspace_by_exact_path("ignored", "/existing", anchor)
            .unwrap();
        assert!(!existing.created);
        assert_eq!(existing.row.id, "write-workspace-0000");
        let error = db
            .find_or_create_workspace_by_exact_path(
                "new",
                "/new",
                WorkspaceFolderAnchor { dev: 7, ino: 10 },
            )
            .unwrap_err();

        assert_static_settings_error(error, "settings_workspace_write_item_limit");
        let count = query_count(&db.conn, "SELECT COUNT(*) FROM workspaces", []);
        assert_eq!(count, SETTINGS_WORKSPACE_LIMIT_MAX);
    }

    #[test]
    fn settings_credential_write는_visible_inventory_max_plus_one을거부한다() {
        let db = Db::open_in_memory().unwrap();
        db.create_workspace("scope").unwrap();
        db.conn
            .execute(
                "WITH RECURSIVE seq(n) AS (
                     SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n + 1 < ?1
                 )
                 INSERT INTO credentials
                    (id, provider, label, credential_kind, keyring_service, keyring_username,
                     masked_hint, workspace_id, created_at, updated_at)
                 SELECT printf('write-credential-%05d', n), 'custom', 'credential', 'api_key',
                        ?2, printf('slot-%05d', n), NULL, NULL,
                        printf('%08d', n), printf('%08d', n)
                 FROM seq",
                rusqlite::params![
                    i64::try_from(SETTINGS_CREDENTIAL_LIMIT_MAX).unwrap(),
                    secret::KEYRING_SERVICE
                ],
            )
            .unwrap();

        let error = db
            .insert_credential(&sample("must-not-persist"))
            .unwrap_err();

        assert_static_settings_error(error, "settings_credential_write_item_limit");
        let count = query_count(&db.conn, "SELECT COUNT(*) FROM credentials", []);
        assert_eq!(count, SETTINGS_CREDENTIAL_LIMIT_MAX);
    }

    #[test]
    fn settings_env_profile_write는_workspace_max_plus_one을거부한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace = db.create_workspace("scope").unwrap();
        db.conn
            .execute(
                "WITH RECURSIVE seq(n) AS (
                     SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n + 1 < ?2
                 )
                 INSERT INTO env_profiles
                    (id, workspace_id, name, kind, is_production, created_at, updated_at)
                 SELECT printf('write-profile-%04d', n), ?1, 'profile', 'custom', 0,
                        printf('%08d', n), printf('%08d', n)
                 FROM seq",
                rusqlite::params![
                    workspace,
                    i64::try_from(SETTINGS_ENV_PROFILE_LIMIT_MAX).unwrap()
                ],
            )
            .unwrap();

        let error = db
            .insert_env_profile(&workspace, "must-not-persist", "custom")
            .unwrap_err();

        assert_static_settings_error(error, "settings_env_profile_write_item_limit");
        let count = query_count(
            &db.conn,
            "SELECT COUNT(*) FROM env_profiles WHERE workspace_id = ?1",
            [&workspace],
        );
        assert_eq!(count, SETTINGS_ENV_PROFILE_LIMIT_MAX);
    }

    #[test]
    fn settings_env_var_upsert는_cap에서update를보존하고new만거부한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace = db.create_workspace("scope").unwrap();
        let profile = db
            .insert_env_profile(&workspace, "profile", "custom")
            .unwrap();
        db.conn
            .execute(
                "WITH RECURSIVE seq(n) AS (
                     SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n + 1 < ?2
                 )
                 INSERT INTO env_vars
                    (id, profile_id, key, kind, plain_value, credential_id, created_at, updated_at)
                 SELECT printf('write-env-%05d', n), ?1, printf('KEY_%05d', n),
                        'plain', 'value', NULL, printf('%08d', n), printf('%08d', n)
                 FROM seq",
                rusqlite::params![profile, i64::try_from(SETTINGS_ENV_VAR_LIMIT_MAX).unwrap()],
            )
            .unwrap();

        db.upsert_env_var(&profile, "KEY_00000", &EnvValue::Plain("updated".into()))
            .unwrap();
        let updated: String = db
            .conn
            .query_row(
                "SELECT plain_value FROM env_vars WHERE profile_id = ?1 AND key = 'KEY_00000'",
                [&profile],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(updated, "updated");
        let error = db
            .upsert_env_var(
                &profile,
                "KEY_MUST_NOT_PERSIST",
                &EnvValue::Plain("value".into()),
            )
            .unwrap_err();

        assert_static_settings_error(error, "settings_env_var_write_item_limit");
        let count = query_count(
            &db.conn,
            "SELECT COUNT(*) FROM env_vars WHERE profile_id = ?1",
            [&profile],
        );
        assert_eq!(count, SETTINGS_ENV_VAR_LIMIT_MAX);
    }

    #[test]
    fn settings_agent_write는_max_plus_one을거부한다() {
        let db = Db::open_in_memory().unwrap();
        db.create_workspace("scope").unwrap();
        db.conn
            .execute(
                "WITH RECURSIVE seq(n) AS (
                     SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n + 1 < ?1
                 )
                 INSERT INTO agent_configs
                    (id, name, command, args_json, waiting_regex, approval_regex, error_regex,
                     done_regex, mcp_proxy_enabled, mcp_proxy_server_id, mcp_config_flag,
                     created_at, updated_at)
                 SELECT printf('write-agent-%04d', n), 'agent', 'true', '[]',
                        NULL, NULL, NULL, NULL, 0, NULL, NULL,
                        printf('%08d', n), printf('%08d', n)
                 FROM seq",
                [i64::try_from(SETTINGS_AGENT_LIMIT_MAX).unwrap()],
            )
            .unwrap();

        let error = db
            .insert_agent_config(
                "must-not-persist",
                "true",
                &[],
                None,
                None,
                None,
                None,
                false,
                None,
                None,
            )
            .unwrap_err();

        assert_static_settings_error(error, "settings_agent_write_item_limit");
        let count = query_count(
            &db.conn,
            "SELECT COUNT(*) FROM agent_configs WHERE deleted_at IS NULL",
            [],
        );
        assert_eq!(count, SETTINGS_AGENT_LIMIT_MAX);
    }

    #[test]
    fn settings_agent_write는_logical_byte_plus_one을insert전에거부한다() {
        let db = Db::open_in_memory().unwrap();
        db.create_workspace("scope").unwrap();
        for index in 0..4 {
            let id = format!("byte-agent-{index}");
            let name_bytes = SETTINGS_ROW_BYTES_MAX - id.len() - "true".len() - "[]".len();
            let name = "x".repeat(name_bytes);
            db.conn
                .execute(
                    "INSERT INTO agent_configs
                        (id, name, command, args_json, waiting_regex, approval_regex, error_regex,
                         done_regex, mcp_proxy_enabled, mcp_proxy_server_id, mcp_config_flag,
                         created_at, updated_at)
                     VALUES (?1, ?2, 'true', '[]', NULL, NULL, NULL, NULL, 0, NULL, NULL,
                             ?1, ?1)",
                    (&id, &name),
                )
                .unwrap();
        }

        let error = db
            .insert_agent_config(
                "must-not-persist",
                "true",
                &[],
                None,
                None,
                None,
                None,
                false,
                None,
                None,
            )
            .unwrap_err();

        assert_static_settings_error(error, "settings_agent_write_retained_bytes_limit");
        let count = query_count(&db.conn, "SELECT COUNT(*) FROM agent_configs", []);
        assert_eq!(count, 4);
    }

    #[test]
    fn settings_workspace_concurrent_writers는하나만마지막slot을획득한다() {
        let (dir, path, db) = file_db("settings-write-concurrent");
        seed_settings_workspaces(&db.conn, SETTINGS_WORKSPACE_LIMIT_MAX - 1);
        drop(db);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|index| {
                let path = path.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let db = Db::open(&path).unwrap();
                    barrier.wait();
                    db.create_workspace(&format!("concurrent-{index}"))
                })
            })
            .collect();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let error = results.into_iter().find_map(Result::err).unwrap();
        assert_static_settings_error(error, "settings_workspace_write_item_limit");
        let db = Db::open(&path).unwrap();
        let count = query_count(&db.conn, "SELECT COUNT(*) FROM workspaces", []);
        assert_eq!(count, SETTINGS_WORKSPACE_LIMIT_MAX);
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    fn seed_finalized_audit_rows(conn: &Connection, count: usize) {
        conn.execute(
            "WITH RECURSIVE seq(n) AS (
                 SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n + 1 < ?1
             )
             INSERT INTO tool_audit_logs
                (id, tool_name, decision, created_at, lifecycle, completed_at)
             SELECT printf('retained-audit-%05d', n), 'tool', 'allow_once',
                    strftime('%Y-%m-%dT%H:%M:%fZ','now','-1 day'), 'succeeded',
                    strftime('%Y-%m-%dT%H:%M:%fZ','now','-1 day')
             FROM seq",
            [i64::try_from(count).unwrap()],
        )
        .unwrap();
    }

    fn seed_expired_finalized_audit_rows(conn: &Connection, count: usize) {
        conn.execute(
            "WITH RECURSIVE seq(n) AS (
                 SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n + 1 < ?1
             )
             INSERT INTO tool_audit_logs
                (id, tool_name, decision, created_at, lifecycle, completed_at)
             SELECT printf('expired-audit-%05d', n), 'tool', 'allow_once',
                    '2000-01-01T00:00:00.000Z', 'succeeded',
                    '2000-01-01T00:00:00.000Z'
             FROM seq",
            [i64::try_from(count).unwrap()],
        )
        .unwrap();
    }

    #[test]
    fn production_audit_write는_finalized_n_plus_one을원자적으로정리한다() {
        let db = Db::open_in_memory().unwrap();
        seed_finalized_audit_rows(&db.conn, audit::AUDIT_RETENTION_MAX_FINALIZED_ITEMS);

        let audit_id = db
            .record_tool_audit(
                &audit::AuditRecord {
                    workspace_id: None,
                    session_id: None,
                    server_id: Some("server"),
                    tool_name: "newest-tool",
                    input_json: "{}",
                    decision: audit::ToolDecision::AllowOnce,
                },
                &secret::RedactionService::new(),
                None,
            )
            .unwrap();

        let finalized = query_count(
            &db.conn,
            "SELECT COUNT(*) FROM tool_audit_logs
             WHERE lifecycle IN ('succeeded', 'failed', 'unknown', 'denied')",
            [],
        );
        assert_eq!(finalized, audit::AUDIT_RETENTION_MAX_FINALIZED_ITEMS);
        let inserted: bool = db
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM tool_audit_logs WHERE id = ?1)",
                [&audit_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(inserted);
    }

    #[test]
    fn legacy_audit_oversize는한번의lazy_retry에서bounded_batch로수렴한다() {
        let db = Db::open_in_memory().unwrap();
        let encryptor = CountingAuditSecretStore::default();
        seed_finalized_audit_rows(
            &db.conn,
            audit::AUDIT_RETENTION_MAX_FINALIZED_ITEMS
                + audit::AUDIT_RETENTION_DELETE_BATCH_ITEMS
                + 1,
        );
        let audit_id = db
            .record_tool_audit(
                &audit::AuditRecord {
                    workspace_id: None,
                    session_id: None,
                    server_id: Some("server"),
                    tool_name: "after-lazy-normalization",
                    input_json: "{}",
                    decision: audit::ToolDecision::AllowOnce,
                },
                &secret::RedactionService::new(),
                Some(&encryptor),
            )
            .unwrap();
        assert_eq!(
            query_count(
                &db.conn,
                "SELECT COUNT(*) FROM tool_audit_logs WHERE lifecycle = 'succeeded'",
                [],
            ),
            audit::AUDIT_RETENTION_MAX_FINALIZED_ITEMS,
            "lazy batches must complete before the retried lifecycle transaction commits"
        );
        assert!(
            db.conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM tool_audit_logs WHERE id = ?1)",
                    [&audit_id],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap()
        );
        assert_eq!(
            encryptor
                .set_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "rollback retry must reuse the exact audit key instead of creating another secret"
        );
        let encrypted: bool = db
            .conn
            .query_row(
                "SELECT input_encrypted_blob IS NOT NULL FROM tool_audit_logs WHERE id = ?1",
                [&audit_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(encrypted);
    }

    #[test]
    fn idle후_64개초과expired_audit도첫lifecycle호출에서수렴한다() {
        let db = Db::open_in_memory().unwrap();
        seed_expired_finalized_audit_rows(
            &db.conn,
            audit::AUDIT_RETENTION_DELETE_BATCH_ITEMS * 2 + 2,
        );

        let audit_id = db
            .record_tool_audit(
                &audit::AuditRecord {
                    workspace_id: None,
                    session_id: None,
                    server_id: Some("server"),
                    tool_name: "after-idle",
                    input_json: "{}",
                    decision: audit::ToolDecision::AllowOnce,
                },
                &secret::RedactionService::new(),
                None,
            )
            .unwrap();

        assert_eq!(
            query_count(
                &db.conn,
                "SELECT COUNT(*) FROM tool_audit_logs WHERE lifecycle = 'succeeded'",
                [],
            ),
            1
        );
        assert!(
            db.conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM tool_audit_logs WHERE id = ?1)",
                    [&audit_id],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap()
        );
    }

    #[test]
    fn audit_lifecycle_source_laws_keep_steady_state_to_one_window_scan() {
        let source = include_str!("db.rs");
        let public_body = |name: &str| {
            source
                .split_once(&format!("pub fn {name}("))
                .unwrap()
                .1
                .split("\n    pub fn ")
                .next()
                .unwrap()
        };
        for method in [
            "record_tool_audit",
            "acquire_authorization_owner",
            "commit_authorization_preflight",
            "commit_authorization_preflight_revision_cas",
            "complete_authorization_outcome",
            "close_authorization_owner",
        ] {
            let body = public_body(method);
            assert!(
                body.contains("with_audit_retention_normalization_retry"),
                "{method}"
            );
            assert_eq!(
                body.matches("prune_audit_logs_in_transaction").count(),
                1,
                "{method} steady path must contain one bounded retention window"
            );
            assert!(
                !body.contains("normalize_audit_retention_to_completion"),
                "{method} must not pre-scan steady state"
            );
        }
        let retry = source
            .split_once("fn with_audit_retention_normalization_retry")
            .unwrap()
            .1
            .split("\n    ///")
            .next()
            .unwrap();
        assert_eq!(retry.matches("lifecycle_transaction()").count(), 2);
        assert!(retry.contains("downcast_ref::<audit::AuditRetentionNormalizationRequired>()"));
        assert!(!retry.contains("cause.to_string()"));
        assert_eq!(
            retry
                .matches("normalize_audit_retention_to_completion")
                .count(),
            1
        );
    }

    #[test]
    fn audit_retention_retry는동일문자열의비retention오류를재시도하지않는다() {
        let db = Db::open_in_memory().unwrap();
        let attempts = std::cell::Cell::new(0_u8);

        let result: anyhow::Result<()> = db.with_audit_retention_normalization_retry(|| {
            attempts.set(attempts.get() + 1);
            anyhow::bail!("audit_retention_normalization_required")
        });

        assert_eq!(attempts.get(), 1);
        assert_eq!(
            result.unwrap_err().to_string(),
            "audit_retention_normalization_required"
        );
    }

    #[test]
    fn authorization_completion_retention_failure는transition과gc를함께rollback한다() {
        let (dir, _path, db) = file_db("audit-completion-retention-rollback");
        let lock_dir = authorization_lock_dir(&db.authorization_db_identity);
        let owner = db
            .acquire_authorization_owner("proxy:retention-completion")
            .unwrap();
        let operation = "operation-retention-completion";
        let audit::AuthorizationPreflight::Prepared(_grant) = db
            .commit_authorization_preflight(
                &owner,
                authorization_plan(
                    operation,
                    "server",
                    "tool",
                    audit::ApprovalDecision::AllowOnce,
                ),
                "{}",
                &secret::RedactionService::new(),
            )
            .unwrap()
        else {
            panic!("allow-once must prepare")
        };
        seed_finalized_audit_rows(&db.conn, audit::AUDIT_RETENTION_MAX_FINALIZED_ITEMS);
        db.conn
            .execute_batch(
                "CREATE TRIGGER fail_storage_audit_retention_completion
                 BEFORE DELETE ON tool_audit_logs
                 BEGIN SELECT RAISE(ABORT, 'injected retention failure'); END;",
            )
            .unwrap();

        assert!(
            db.complete_authorization_outcome(
                &owner,
                operation,
                audit::AuthorizationOutcome::Succeeded,
            )
            .is_err()
        );
        assert_eq!(
            db.tool_audit_lifecycle(operation).unwrap(),
            Some(audit::AuditLifecycle::Prepared)
        );
        db.conn
            .execute_batch("DROP TRIGGER fail_storage_audit_retention_completion")
            .unwrap();
        db.complete_authorization_outcome(
            &owner,
            operation,
            audit::AuthorizationOutcome::Succeeded,
        )
        .unwrap();
        assert_eq!(
            db.tool_audit_lifecycle(operation).unwrap(),
            Some(audit::AuditLifecycle::Succeeded)
        );
        db.close_authorization_owner(owner).unwrap();
        drop(db);
        fs::remove_dir_all(lock_dir).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn authorization_recovery_retention_failure는unknown전이를rollback하고lock을해제한다() {
        let (dir, _path, db) = file_db("audit-recovery-retention-rollback");
        let lock_dir = authorization_lock_dir(&db.authorization_db_identity);
        let scope = "proxy:retention-recovery";
        let owner = db.acquire_authorization_owner(scope).unwrap();
        let operation = "operation-retention-recovery";
        let audit::AuthorizationPreflight::Prepared(_grant) = db
            .commit_authorization_preflight(
                &owner,
                authorization_plan(
                    operation,
                    "server",
                    "tool",
                    audit::ApprovalDecision::AllowOnce,
                ),
                "{}",
                &secret::RedactionService::new(),
            )
            .unwrap()
        else {
            panic!("allow-once must prepare")
        };
        drop(owner);
        seed_finalized_audit_rows(&db.conn, audit::AUDIT_RETENTION_MAX_FINALIZED_ITEMS);
        db.conn
            .execute_batch(
                "CREATE TRIGGER fail_storage_audit_retention_recovery
                 BEFORE DELETE ON tool_audit_logs
                 BEGIN SELECT RAISE(ABORT, 'injected retention failure'); END;",
            )
            .unwrap();

        assert!(db.acquire_authorization_owner(scope).is_err());
        assert_eq!(
            db.tool_audit_lifecycle(operation).unwrap(),
            Some(audit::AuditLifecycle::Prepared)
        );
        db.conn
            .execute_batch("DROP TRIGGER fail_storage_audit_retention_recovery")
            .unwrap();
        let recovered_owner = db.acquire_authorization_owner(scope).unwrap();
        assert_eq!(
            db.tool_audit_lifecycle(operation).unwrap(),
            Some(audit::AuditLifecycle::Unknown)
        );
        db.close_authorization_owner(recovered_owner).unwrap();
        drop(db);
        fs::remove_dir_all(lock_dir).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    /// v33 기존 DB(sessions.*_regex 컬럼 없음)를 열면 v34로 올라가고 기존 세션 행이
    /// 그대로 보존된다. 새 컬럼도 바로 쓸 수 있다 — RespawnArchivedAgent가 재실행 시
    /// 이 컬럼에서 regex를 복원하므로, 마이그레이션이 기존 행을 깨거나 새 컬럼이
    /// 막히면 그 복원 경로 전체가 조용히 무너진다.
    #[test]
    fn session_regex_마이그레이션은_기존_v33_db를_보존한다() {
        let dir =
            std::env::temp_dir().join(format!("deppy-session-regex-mig-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..33] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 33).unwrap();
            conn.execute(
                "INSERT INTO workspaces (id, name, path, created_at, updated_at)
                 VALUES ('ws-1', 'existing', '/repo', 't', 't')",
                [],
            )
            .unwrap();
            // v33 스키마 그대로 — waiting_regex 등 컬럼이 아직 없는 실제 구버전 행을 흉내낸다.
            conn.execute(
                "INSERT INTO sessions
                    (id, workspace_id, session_kind, agent_id, title, command, args_json,
                     cwd, status, created_at, updated_at, last_log_offset)
                 VALUES ('sess-1', 'ws-1', 'shell', NULL, '기존 세션', '/bin/sh', '[]',
                    '/tmp', 'exited', 't', 't', 7)",
                [],
            )
            .unwrap();
        }

        let db = Db::open(&path).unwrap();
        assert_eq!(Db::read_user_version(&db.conn).unwrap(), MIGRATIONS.len());

        // 기존 세션 행 보존 — 새 컬럼은 NULL(=미지정)로 채워져 있어야 한다.
        let preserved = persist::load_sessions(&db.conn, "ws-1").unwrap();
        assert_eq!(preserved.len(), 1);
        assert_eq!(preserved[0].id, "sess-1");
        assert_eq!(preserved[0].command, "/bin/sh");
        assert_eq!(preserved[0].last_log_offset, 7);
        assert_eq!(preserved[0].waiting_regex, None);

        // 새 컬럼 사용 가능 — 업그레이드 후 spawn되는 세션은 regex를 저장/복원할 수 있다.
        let mut fresh = preserved[0].clone();
        fresh.id = "sess-2".to_owned();
        fresh.waiting_regex = Some("Waiting".to_owned());
        fresh.error_regex = Some("FATAL".to_owned());
        persist::upsert_session(&db.conn, &fresh).unwrap();
        let after = persist::load_sessions(&db.conn, "ws-1").unwrap();
        let saved = after.iter().find(|row| row.id == "sess-2").unwrap();
        assert_eq!(saved.waiting_regex.as_deref(), Some("Waiting"));
        assert_eq!(saved.error_regex.as_deref(), Some("FATAL"));
        assert_eq!(saved.approval_regex, None);

        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
