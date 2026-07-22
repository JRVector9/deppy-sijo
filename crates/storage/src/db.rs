use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use sha2::{Digest as _, Sha256};

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
    authorization_db_identity: String,
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

/// hook이 보고한 세션 바인딩 행 (v15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSessionRow {
    pub session_key: String,
    pub kind: String,
    pub agent_session_id: String,
    pub transcript_path: String,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialSecretLocation {
    pub keyring_service: String,
    pub keyring_username: String,
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

fn validate_owned_physical_secret_slot(logical_id: &str, slot: &str) -> anyhow::Result<()> {
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

fn validate_oauth_metadata_json(json: &str) -> anyhow::Result<()> {
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

impl Db {
    /// DB 열기 + 마이그레이션. infra(PRAGMA/백업/IMMEDIATE 러너)는 storage-core가 담당하고
    /// (v2.8 §6.1), 이 crate는 **마이그레이션 원장(MIGRATIONS, v1..vN 순서 불변)** 조립과
    /// 앱 수준 store/facade만 소유한다.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
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

    /// credential metadata 추가. created_at/updated_at은 SQLite가 UTC로 기록한다.
    ///
    /// 이 기존 생성 경로는 IN01 migration/cutover 전까지 logical id 자체를 keyring username에
    /// 보관한다. 새 physical bundle publish와 secret-backed 실행은 이 legacy pointer를 허용하지
    /// 않으며, [`Self::rotate_credential_secret_slot`]을 거쳐야 한다.
    pub fn insert_credential(&self, meta: &CredentialMeta) -> anyhow::Result<()> {
        self.conn
            .execute(
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
        Ok(())
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
        self.conn
            .execute(
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
        Ok(())
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
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, provider, label, credential_kind, masked_hint, workspace_id,
                    keyring_service, keyring_username, oauth_json
             FROM credentials ORDER BY created_at, id LIMIT ?1",
        )?;
        let rows = stmt.query_map([fetch_limit], |row| {
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
        let records = rows.collect::<Result<Vec<_>, _>>()?;
        anyhow::ensure!(
            records.len() <= limit,
            "credential secret record limit exceeded: limit={limit}"
        );
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
    /// 호출자는 commit 성공 후에만 이전 slot을 지우고, 실패 시 새 orphan slot을 정리한다.
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
        let tx = self.conn.unchecked_transaction()?;
        let affected = tx
            .execute(
                "UPDATE credentials
                 SET keyring_username = ?2, oauth_json = ?3, masked_hint = ?4,
                     updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE id = ?1",
                (id, physical_slot, oauth_json, masked_hint),
            )
            .with_context(|| format!("credential secret slot publish 실패: {id}"))?;
        anyhow::ensure!(affected == 1, "credential 없음: {id}");
        tx.commit()
            .context("credential secret slot/metadata commit 실패")
    }

    /// Compare-and-swap publishes an already-staged physical slot together with its optional
    /// OAuth metadata and masked hint. The update occurs only while the stored pointer exactly
    /// matches `expected_previous_pointer`; stale or missing rows return `false` without changing
    /// any column. `None` writes SQL NULL, preserving the non-OAuth representation.
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
        let affected = self
            .conn
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
            affected <= 1,
            "credential secret slot CAS가 여러 행을 변경했습니다"
        );
        Ok(affected == 1)
    }

    /// logical credential id를 keyring physical slot으로 해석한다. secret 본문은 반환하지 않는다.
    pub fn credential_secret_location(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<CredentialSecretLocation>> {
        self.conn
            .query_row(
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
        let (candidate_count, candidate_bytes): (i64, i64) = tx.query_row(
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
            let mut stmt = tx.prepare_cached(
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
        tx.commit()
            .context("OAuth binding point lookup transaction 실패")?;
        Ok(candidates)
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

    /// Delete credential metadata only when no live env/MCP reference exists and the physical
    /// pointer still equals the caller's expected value. The single conditional DELETE closes the
    /// delete-vs-rotation race: stale cleanup is a harmless no-op and cannot remove a newer slot.
    pub fn delete_credential_if_unused_cas(
        &self,
        id: &str,
        expected_pointer: &str,
    ) -> anyhow::Result<bool> {
        let affected = self
            .conn
            .execute(
                "DELETE FROM credentials WHERE id = ?1 AND keyring_username = ?2
                   AND NOT EXISTS (SELECT 1 FROM env_vars WHERE credential_id = ?1)
                   AND NOT EXISTS (
                       SELECT 1
                       FROM mcp_servers, json_each(COALESCE(mcp_servers.env_credentials_json, '{}'))
                       WHERE json_each.value = ?1
                   )",
                (id, expected_pointer),
            )
            .with_context(|| format!("credential expected-pointer 삭제 실패: {id}"))?;
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

    /// 런타임이 없는(또는 warm) 워크스페이스의 활동 화면에 쓸 영속 pane snapshot —
    /// (workspace_id, 제목, 세션 cwd). `sessions` 전체는 닫힌 과거 이력도 남으므로,
    /// 현재 복원 레이아웃에 연결된 `mux_panes`만 읽는다. 한 쿼리로 모든 workspace를
    /// 반환해 UI의 N+1을 피한다. cwd는 기본 제목("셸 N")을 프로젝트명으로 바꿔 표시하는
    /// 데 쓴다(활성 워크스페이스의 resolve_session_title과 같은 규칙) — 세션이 없는
    /// pane이면 빈 문자열.
    pub fn list_persisted_activity_panes(&self) -> anyhow::Result<Vec<(String, String, String)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT p.workspace_id,
                    COALESCE(NULLIF(p.title, ''), NULLIF(s.title, ''), p.id),
                    COALESCE(s.cwd, '')
               FROM mux_panes p
               LEFT JOIN sessions s ON s.id = p.session_id
              ORDER BY p.workspace_id, p.created_at, p.id",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
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

    /// needsInput 기록. `message`는 hook이 보고한 대기 사유 문구(claude Notification의
    /// payload.message) — 없으면 None. clear(waiting=false) 시에는 문구도 함께 지운다
    /// (해소된 질문이 다음 대기에 되살아나면 안 된다).
    pub fn set_agent_needs_input(
        &self,
        session_key: &str,
        waiting: bool,
        message: Option<&str>,
    ) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO agent_needs_input
                     (session_key, waiting, updated_at, message)
                 VALUES (?1, ?2, CAST(strftime('%s','now') AS INTEGER), ?3)",
                (session_key, waiting as i64, message.filter(|_| waiting)),
            )
            .with_context(|| format!("needsInput 저장 실패: {session_key}"))?;
        Ok(())
    }

    /// 현재 입력 대기(waiting) 중인 세션 키 + hook이 보고한 사유 문구. stale(1시간 초과)은
    /// 제외해 죽은 hook의 잔여가 영원히 주황으로 남지 않게 한다.
    pub fn list_waiting_sessions(&self) -> anyhow::Result<Vec<(String, Option<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_key, message FROM agent_needs_input
             WHERE waiting = 1
               AND updated_at > CAST(strftime('%s','now') AS INTEGER) - 3600",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// 턴 완료(Stop hook) 기록 — waiting은 0으로 함께 리셋한다(턴이 끝났으므로).
    pub fn set_agent_turn_done(&self, session_key: &str) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO agent_needs_input
                     (session_key, waiting, turn_done, updated_at)
                 VALUES (?1, 0, 1, CAST(strftime('%s','now') AS INTEGER))",
                (session_key,),
            )
            .with_context(|| format!("turn_done 저장 실패: {session_key}"))?;
        Ok(())
    }

    /// 턴 완료 소비(사용자가 해당 pane을 확인) — turn_done만 내린다(waiting 불변).
    /// `seen_at`(내가 읽은 updated_at) 이후에 도착한 새 완료 이벤트는 지우지 않는다 —
    /// 읽기~clear 사이 새 Stop이 오면 그 알림까지 유실되던 레이스 방지(codex 리뷰).
    pub fn clear_agent_turn_done(&self, session_key: &str, seen_at: i64) -> anyhow::Result<()> {
        self.conn
            .execute(
                "UPDATE agent_needs_input SET turn_done = 0
                 WHERE session_key = ?1 AND updated_at <= ?2",
                (session_key, seen_at),
            )
            .with_context(|| format!("turn_done 해제 실패: {session_key}"))?;
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
        self.conn
            .execute(
                "INSERT OR REPLACE INTO agent_statusline
                     (session_key, effort, model, context_pct, updated_at)
                 VALUES (?1, ?2, ?3, ?4, CAST(strftime('%s','now') AS INTEGER))",
                (session_key, effort, model, context_pct),
            )
            .with_context(|| format!("statusline 저장 실패: {session_key}"))?;
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

    /// 턴 완료(미확인) 세션 (key, updated_at) 목록. waiting과 같은 1시간 stale 컷오프.
    /// updated_at은 소비 시 조건부 clear의 세대 기준으로 쓴다.
    pub fn list_turn_done_sessions(&self) -> anyhow::Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_key, updated_at FROM agent_needs_input
             WHERE turn_done = 1
               AND updated_at > CAST(strftime('%s','now') AS INTEGER) - 3600",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
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
            .with_context(|| format!("structured thread 저장 실패: {thread_id}"))?;
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
        let mut stmt = self.conn.prepare(&format!(
            "SELECT local_session_id, workspace_id, thread_id, title, cwd, model,
                    favorite, archived, created_at, updated_at
               FROM structured_threads
              WHERE workspace_id = ?1 AND (?2 = 1 OR archived = 0)
              ORDER BY favorite DESC, updated_at DESC, local_session_id
              LIMIT {}",
            Self::STRUCTURED_THREADS_LIST_CAP
        ))?;
        let rows = stmt.query_map((workspace_id, include_archived as i64), |row| {
            Ok(StructuredThreadRow {
                local_session_id: row.get(0)?,
                workspace_id: row.get(1)?,
                thread_id: row.get(2)?,
                title: row.get(3)?,
                cwd: row.get(4)?,
                model: row.get(5)?,
                favorite: row.get::<_, i64>(6)? != 0,
                archived: row.get::<_, i64>(7)? != 0,
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
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

    /// 프로젝트 폴더 앵커(dev, ino)를 저장한다 — 폴더 rename 감지·복구용(2026-07-08).
    /// path 미설정/무효면 (None, None)으로 지운다.
    pub fn set_workspace_anchor(
        &self,
        id: &str,
        dev: Option<i64>,
        ino: Option<i64>,
    ) -> anyhow::Result<()> {
        self.conn
            .execute(
                "UPDATE workspaces SET path_dev = ?2, path_ino = ?3 WHERE id = ?1",
                (id, dev, ino),
            )
            .with_context(|| format!("workspace 앵커 저장 실패: {id}"))?;
        Ok(())
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

    /// Connector overview용 complete-or-error bounded inventory. SQL performs a `limit + 1`
    /// count/byte probe first, then materializes at most `limit` lightweight summary rows.
    pub fn mcp_server_inventory(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<mcp_store::McpServerInventoryRow>> {
        mcp_store::server_inventory(&self.conn, limit)
    }

    pub fn mcp_server(&self, server_id: &str) -> anyhow::Result<Option<mcp_store::McpServerRow>> {
        mcp_store::server(&self.conn, server_id)
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

    /// Reject active agent-proxy references and delete the server plus live MCP metadata in one
    /// IMMEDIATE transaction. Durable audit history intentionally remains untouched.
    pub fn delete_mcp_server(&mut self, server_id: &str, resolved_at: i64) -> anyhow::Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let referenced = tx
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
        let deleted = mcp_store::delete_server_in_transaction(&tx, server_id, resolved_at)?;
        tx.commit().context("MCP server 삭제 commit 실패")?;
        Ok(deleted)
    }

    /// canonical URL 기준 멱등 등록. built-in provider(Slack 등)의 중복 행을 막는다.
    pub fn ensure_mcp_server_by_url(
        &mut self,
        row: &mcp_store::McpServerRow,
    ) -> anyhow::Result<mcp_store::McpServerRow> {
        mcp_store::ensure_server_by_url(&mut self.conn, row)
    }

    /// import 대상 전체를 all-or-nothing으로 저장한다.
    pub fn insert_mcp_servers_batch(
        &mut self,
        rows: &[mcp_store::McpServerRow],
    ) -> anyhow::Result<usize> {
        mcp_store::insert_servers_batch(&mut self.conn, rows)
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

    /// 저장된 tool 목록 (도구 실행 UI용).
    pub fn list_mcp_tools(&self, server_id: &str) -> anyhow::Result<Vec<mcp_store::McpToolRow>> {
        mcp_store::list_tools_for_server(&self.conn, server_id)
    }

    /// Exact invoke preparation lookup; no schema, description, or full tool list is loaded.
    pub fn mcp_tool_name(&self, server_id: &str, tool_id: &str) -> anyhow::Result<Option<String>> {
        mcp_store::tool_name(&self.conn, server_id, tool_id)
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

    /// tool 실행 감사 기록 (PR-16). encryptor를 넘기면 전체 입력이 암호화 저장된다 (§7).
    pub fn record_tool_audit(
        &self,
        record: &audit::AuditRecord<'_>,
        redaction: &secret::RedactionService,
        encryptor: Option<&dyn secret::SecretStore>,
    ) -> anyhow::Result<String> {
        audit::record_audit(&self.conn, redaction, record, encryptor)
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
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE tool_audit_logs
             SET lifecycle = 'unknown', outcome_error_code = 'owner_superseded',
                 completed_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE authorization_scope = ?1
               AND authorization_run_id != ?2
               AND lifecycle = 'prepared'",
            (scope, &run_id),
        )?;
        tx.commit()
            .context("authorization owner 시작 transaction 실패")?;
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
        let tx = self.conn.unchecked_transaction()?;
        anyhow::ensure!(
            self.authorization_db_identity == owner.db_identity,
            "authorization owner가 다른 DB에 속합니다"
        );
        let current_permission =
            match mcp_store::permission_rule(&tx, plan.server_id(), plan.tool_name())? {
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
                &tx,
                plan.server_id(),
                plan.tool_name(),
                audit::PermissionRule::Allow.as_str(),
                Some(plan.live_schema_hash()),
            )?,
            audit::ToolDecision::DenyAlways => mcp_store::upsert_permission_rule(
                &tx,
                plan.server_id(),
                plan.tool_name(),
                audit::PermissionRule::Deny.as_str(),
                None,
            )?,
            _ => {}
        }
        let preflight = audit::prepare_owned_authorization_preflight(
            &tx,
            plan,
            input_json,
            redaction,
            owner.scope(),
            &owner.run_id,
        )?;
        tx.commit()
            .context("authorization permission/audit preflight commit 실패")?;
        Ok(preflight)
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
        audit::complete_authorization_operation(
            &self.conn,
            owner.scope(),
            &owner.run_id,
            operation_id,
            outcome,
        )
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
        let affected = self
            .conn
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
        Ok(affected)
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
            vec![(ws, "saved shell".to_owned(), "/".to_owned())]
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
    fn agent_needs_input_set_clear_list() {
        let db = Db::open_in_memory().unwrap();
        db.set_agent_needs_input("pane-1", true, None).unwrap();
        db.set_agent_needs_input("pane-2", true, None).unwrap();
        db.set_agent_needs_input("pane-3", false, None).unwrap();
        let mut waiting: Vec<String> = db
            .list_waiting_sessions()
            .unwrap()
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        waiting.sort();
        assert_eq!(waiting, vec!["pane-1".to_string(), "pane-2".to_string()]);
        // clear → 목록에서 빠짐
        db.set_agent_needs_input("pane-1", false, None).unwrap();
        assert_eq!(
            db.list_waiting_sessions().unwrap(),
            vec![("pane-2".to_string(), None)]
        );
    }

    /// hook이 보고한 대기 사유 문구는 그대로 실려 나오고, clear 시 함께 지워진다 —
    /// 해소된 질문이 다음 대기에 되살아나면 안 된다(2026-07-17).
    #[test]
    fn agent_needs_input_message_저장과_clear시_소거() {
        let db = Db::open_in_memory().unwrap();
        db.set_agent_needs_input(
            "pane-1",
            true,
            Some("Claude needs your permission to use Bash"),
        )
        .unwrap();
        assert_eq!(
            db.list_waiting_sessions().unwrap(),
            vec![(
                "pane-1".to_string(),
                Some("Claude needs your permission to use Bash".to_string())
            )]
        );
        // clear 후 다시 대기 — 이전 문구가 남아 있으면 안 된다.
        db.set_agent_needs_input("pane-1", false, None).unwrap();
        db.set_agent_needs_input("pane-1", true, None).unwrap();
        assert_eq!(
            db.list_waiting_sessions().unwrap(),
            vec![("pane-1".to_string(), None)]
        );
    }

    #[test]
    fn agent_turn_done_set_clear_및_needs_input과_상호리셋() {
        let db = Db::open_in_memory().unwrap();
        // Stop hook → turn_done=1, waiting=0
        db.set_agent_turn_done("pane-1").unwrap();
        let listed = db.list_turn_done_sessions().unwrap();
        assert_eq!(listed.len(), 1);
        let (key, seen_at) = listed[0].clone();
        assert_eq!(key, "pane-1");
        assert!(db.list_waiting_sessions().unwrap().is_empty());
        // 읽은 세대 이전 이벤트만 소비 — 더 새 이벤트(seen_at 미래)는 남는다(레이스 방지)
        db.clear_agent_turn_done("pane-1", seen_at - 1).unwrap();
        assert_eq!(db.list_turn_done_sessions().unwrap().len(), 1);
        // 확인(소비) → turn_done만 내림
        db.clear_agent_turn_done("pane-1", seen_at).unwrap();
        assert!(db.list_turn_done_sessions().unwrap().is_empty());
        // needs-input(REPLACE)이 turn_done을 자연 리셋
        db.set_agent_turn_done("pane-2").unwrap();
        db.set_agent_needs_input("pane-2", true, None).unwrap();
        assert!(db.list_turn_done_sessions().unwrap().is_empty());
        assert_eq!(
            db.list_waiting_sessions().unwrap(),
            vec![("pane-2".to_string(), None)]
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
        db.set_credential_oauth_json("cred-binding-oversized", &metadata)
            .unwrap();

        assert!(
            db.credential_oauth_bindings_for_server("server-oversized")
                .is_err()
        );
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
    fn physical_slot_credential_insert는_owned_pointer와_nullable_metadata만_publish한다() {
        let db = Db::open_in_memory().unwrap();
        let plain = secret::LogicalCredentialId::new("cred-plain").unwrap();
        let plain_slot = secret::PhysicalSecretSlot::with_version(&plain, uuid::Uuid::new_v4());
        db.insert_credential_with_secret_slot(&sample(plain.as_str()), plain_slot.as_str(), None)
            .unwrap();
        let plain_record = credential_secret_record(&db, plain.as_str());
        assert_eq!(plain_record.keyring_service, secret::KEYRING_SERVICE);
        assert_eq!(plain_record.keyring_username, plain_slot.as_str());
        assert_ne!(plain_record.keyring_username, plain.as_str());
        assert_eq!(plain_record.oauth_json, None);

        let oauth = secret::LogicalCredentialId::new("cred-oauth-new").unwrap();
        let oauth_slot = secret::PhysicalSecretSlot::with_version(&oauth, uuid::Uuid::new_v4());
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
    fn physical_slot_cas는_success후_stale_expected를_noop처리한다() {
        let db = Db::open_in_memory().unwrap();
        let logical = secret::LogicalCredentialId::new("cred-cas").unwrap();
        let first = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        let stale_candidate =
            secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        db.insert_credential(&sample(logical.as_str())).unwrap();

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
    }

    #[test]
    fn physical_slot_cas는_invalid_pointer와_storage_failure에서_no_mutation이다() {
        let db = Db::open_in_memory().unwrap();
        let logical = secret::LogicalCredentialId::new("cred-cas-rollback").unwrap();
        let other = secret::LogicalCredentialId::new("cred-cas-other").unwrap();
        let current = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        let next = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        let other_slot = secret::PhysicalSecretSlot::with_version(&other, uuid::Uuid::new_v4());
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
    }

    #[test]
    fn oauth_secret_slot_pointer와_metadata는_원자적으로_publish된다() {
        let db = Db::open_in_memory().unwrap();
        let logical = secret::LogicalCredentialId::new("cred-oauth").unwrap();
        let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        db.insert_credential(&sample(logical.as_str())).unwrap();
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
        fs::create_dir_all(&lock_dir).unwrap();
        let lock_path = lock_dir.join("stripe-000.lock");
        let first = open_lock_file(&lock_path).unwrap();
        let second = open_lock_file(&lock_path).unwrap();
        fs2::FileExt::try_lock_exclusive(&first).unwrap();
        assert!(fs2::FileExt::try_lock_exclusive(&second).is_err());
        drop(second);
        drop(first);
        drop(db_a);
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
        let replacement = dir.join("replacement.sqlite3");
        File::create(&replacement)
            .unwrap()
            .write_all(b"new")
            .unwrap();
        fs::rename(&replacement, &path).unwrap();
        let new_identity = physical_db_identity(&path).unwrap();
        assert_ne!(old_identity, new_identity);
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
