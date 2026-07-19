//! SQLite metadata DB는 crates/storage가 소유한다 (설계문서 §10).
//! 이 모듈은 기존 호출부(`crate::storage::Db` 등)를 위한 re-export shim이다.
pub use storage::{
    AgentConfigRow, AgentSessionRow, CredentialMeta, Db, EnvProfileRow, EnvVarRow,
    SESSION_LOG_DISK_BUDGET_BYTES, StatuslineRow, WorkspaceRow, gc_session_logs,
};
