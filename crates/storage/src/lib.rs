//! storage crate (설계문서 9장). PR-11: tail-bounded redacted 세션 로그.
//! SQLite metadata·rotation·encrypted raw log(기본 비활성 — 7장)는 후속 PR.

mod db;
pub use db::{
    AgentConfigRow, AgentSessionRow, ApprovalOutcome, ApprovalStatus, CredentialMeta, Db,
    EnvApiProjectCount, EnvProfileRow, EnvValue, EnvVarRow, HookSessionRow, PendingApprovalRow,
    PermissionRuleRow, StatuslineRow, StructuredThreadRow, WebPushSubscriptionRow, WorkspaceRow,
};
pub use mcp_store::PendingApprovalInsert;

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
