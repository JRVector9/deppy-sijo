//! storage crate (설계문서 9장). PR-11: tail-bounded redacted 세션 로그.
//! SQLite metadata·rotation·encrypted raw log(기본 비활성 — 7장)는 후속 PR.

mod db;
pub use db::{
    AGENT_STATE_BINDING_ROWS_MAX, AGENT_STATE_EXACT_MUTATIONS_MAX, AGENT_STATE_JOB_BYTES_MAX,
    AGENT_STATE_SNAPSHOT_BYTES_MAX, AGENT_STATE_STRUCTURED_MUTATION_BYTES_MAX,
    AGENT_STATE_STRUCTURED_MUTATIONS_MAX, AGENT_STATE_STRUCTURED_PROJECTION_MAX,
    AGENT_STATE_STRUCTURED_WORKSPACE_MAX, ActiveAuthorizationOwner, ActivePendingApprovalOwner,
    AgentConfigRow, AgentSessionBindingReconcile, AgentSessionIdentity, AgentSessionRow,
    AgentStateJob, AgentStateJobRetention, AgentStatePreparationErrorCode, AgentStateSnapshot,
    AgentTurnDoneClear, ApprovalOutcome, ApprovalStatus, CREDENTIAL_OAUTH_BINDING_BYTES_MAX,
    CREDENTIAL_SECRET_LOCATION_BYTES_MAX, CREDENTIAL_SECRET_RECORD_BYTES_MAX, ConnectorConfigCas,
    ConnectorConfigRead, ConnectorConfigRevision, CredentialMeta, CredentialOAuthBindingRecord,
    CredentialSecretLocation, CredentialSecretRecord, Db, EnvApiProjectCount, EnvProfileRow,
    EnvValue, EnvVarRow, HookSessionRow, MCP_REQUEST_TARGET_CREDENTIAL_BYTES_MAX,
    MCP_REQUEST_TARGET_CREDENTIAL_LIMIT_MAX, McpRequestTargetRecord,
    PENDING_APPROVAL_SESSION_CLEANUP_LIMIT_MAX, PENDING_APPROVAL_SESSION_KEY_BYTES_MAX,
    PHYSICAL_SECRET_SLOT_RECONCILIATION_BYTES_MAX, PHYSICAL_SECRET_SLOT_RECONCILIATION_LIMIT_MAX,
    PendingApprovalRow, PermissionRuleRow, PersistedActivityPane, PhysicalSecretSlotLedgerRow,
    PhysicalSecretSlotState, SettingsWorkspaceProjectionRow, StatuslineRow,
    StructuredThreadMutation, StructuredThreadRow, WORKSPACE_NOTE_MAX_BYTES,
    WebPushSubscriptionRow, WorkspaceFindOrCreateResult, WorkspaceFolderAnchor,
    WorkspaceMovedPathUpdate, WorkspaceRow, prepare_agent_state_job_for_retention,
};
pub use mcp_store::{
    MCP_PERMISSION_POINT_BYTES_MAX, MCP_SERVER_INVENTORY_BYTES_MAX, MCP_SERVER_INVENTORY_LIMIT_MAX,
    MCP_SERVER_POINT_BYTES_MAX, MCP_TOOL_NAME_BYTES_MAX, MCP_TOOL_PAGE_BYTES_MAX,
    MCP_TOOL_PAGE_LIMIT_MAX, McpServerInventoryRow, McpServerSaveOutcome, McpToolPage,
    McpToolPageRow, PENDING_APPROVAL_GLOBAL_LIMIT_MAX, PENDING_APPROVAL_ID_BYTES_MAX,
    PENDING_APPROVAL_LIST_LIMIT_MAX, PENDING_APPROVAL_PREVIEW_BYTES_MAX,
    PENDING_APPROVAL_RETAINED_BYTES_MAX, PENDING_APPROVAL_SCHEMA_HASH_BYTES_MAX,
    PENDING_APPROVAL_SERVER_ID_BYTES_MAX, PENDING_APPROVAL_SESSION_LIMIT_MAX,
    PENDING_APPROVAL_TOOL_NAME_BYTES_MAX, PendingApprovalInsert, PendingApprovalPage,
};

mod logs;

pub use logs::{
    SESSION_LOG_DISK_BUDGET_BYTES, SessionLogWriter, gc_session_logs, seek_ansi_tail_boundary,
    seek_ansi_tail_boundary_snapshot,
};

/// 종료 세션 스크롤백 압축 아카이브 (§14.3 확장 — PR-A1)
pub mod scrollback_archive;

mod write_worker;
pub use write_worker::{
    DbWriteHandle, DbWriteQueueError, DbWriteStatsSnapshot, DbWriteWorker, DbWriteWorkerConfig,
};

#[cfg(test)]
mod tests {
    #[test]
    fn persisted_activity_pane_is_reexported_from_crate_root() {
        let row = crate::PersistedActivityPane {
            workspace_id: "workspace".to_owned(),
            pane_id: "pane".to_owned(),
            title: "title".to_owned(),
            cwd: "/tmp".to_owned(),
        };

        assert_eq!(row.pane_id, "pane");
    }
}
