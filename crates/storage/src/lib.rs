//! storage crate (설계문서 9장). PR-11: tail-bounded redacted 세션 로그.
//! SQLite metadata·rotation·encrypted raw log(기본 비활성 — 7장)는 후속 PR.

mod db;
pub use db::{
    ActiveAuthorizationOwner, AgentConfigRow, AgentSessionRow, ApprovalOutcome, ApprovalStatus,
    CREDENTIAL_OAUTH_BINDING_BYTES_MAX, CREDENTIAL_SECRET_LOCATION_BYTES_MAX,
    CREDENTIAL_SECRET_RECORD_BYTES_MAX, ConnectorConfigCas, ConnectorConfigRead,
    ConnectorConfigRevision, CredentialMeta, CredentialOAuthBindingRecord,
    CredentialSecretLocation, CredentialSecretRecord, Db, EnvApiProjectCount, EnvProfileRow,
    EnvValue, EnvVarRow, HookSessionRow, PENDING_APPROVAL_SESSION_CLEANUP_LIMIT_MAX,
    PENDING_APPROVAL_SESSION_KEY_BYTES_MAX, PHYSICAL_SECRET_SLOT_RECONCILIATION_BYTES_MAX,
    PHYSICAL_SECRET_SLOT_RECONCILIATION_LIMIT_MAX, PendingApprovalRow, PermissionRuleRow,
    PhysicalSecretSlotLedgerRow, PhysicalSecretSlotState, StatuslineRow, StructuredThreadRow,
    WebPushSubscriptionRow, WorkspaceRow,
};
pub use mcp_store::{
    MCP_PERMISSION_POINT_BYTES_MAX, MCP_SERVER_INVENTORY_BYTES_MAX, MCP_SERVER_INVENTORY_LIMIT_MAX,
    MCP_SERVER_POINT_BYTES_MAX, MCP_TOOL_NAME_BYTES_MAX, MCP_TOOL_PAGE_BYTES_MAX,
    MCP_TOOL_PAGE_LIMIT_MAX, McpServerInventoryRow, McpServerSaveOutcome, McpToolPage,
    McpToolPageRow, PendingApprovalInsert,
};

mod logs;

pub use logs::{
    SESSION_LOG_DISK_BUDGET_BYTES, SessionLogWriter, gc_session_logs, seek_ansi_tail_boundary,
};

/// 종료 세션 스크롤백 압축 아카이브 (§14.3 확장 — PR-A1)
pub mod scrollback_archive;

mod write_worker;
pub use write_worker::{
    DbWriteHandle, DbWriteQueueError, DbWriteStatsSnapshot, DbWriteWorker, DbWriteWorkerConfig,
};
