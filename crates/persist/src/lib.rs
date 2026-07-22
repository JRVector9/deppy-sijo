//! PR-14 Workspace/Restore (설계문서 §11.1~11.5, PR-14 crash recovery).
//! mux layout / session metadata의 SQLite 영속 + crash recovery 순수 로직.
//! 마이그레이션 배열 통합과 UI 배선은 crates/app 몫 — 이 crate는
//! DDL 상수([`MIGRATION_SQL`])와 repo 함수만 제공한다.

mod layout_json;
mod recovery;
mod repo;

pub use layout_json::{layout_from_json, layout_to_json};
pub use recovery::{
    LockFile, delete_workspace_data, reconcile_orphan_sessions, validate_log_offset,
};
pub use repo::{
    PaneState, SESSION_STATUS_EXITED, SESSION_STATUS_RUNNING, SessionRow, TabState, WindowState,
    WorkspaceRestore, load_sessions, load_window_layouts, load_workspace_restore_bounded,
    save_window_layout, update_session_log_offset, upsert_session,
};

/// 설계문서 §11.1~11.5 스키마 그대로. 앱 마이그레이션 배열(app/storage.rs MIGRATIONS)에
/// 이 상수를 추가하는 것은 오케스트레이터 몫이다. FK 대상인 workspaces / agent_configs는
/// 선행 마이그레이션이 이미 만들었다는 전제.
///
/// 상호 참조 주의 (§11.5 참고): `mux_windows.active_tab_id` ↔ `mux_tabs.window_id`이므로
/// insert 순서는 window(active_tab_id NULL) → tabs → window update.
pub const MIGRATION_SQL: &str = "
CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    session_kind TEXT NOT NULL DEFAULT 'agent',  -- agent | shell | mcp
    agent_id TEXT,                               -- session_kind = agent일 때만
    title TEXT NOT NULL,
    command TEXT NOT NULL,
    args_json TEXT NOT NULL,
    cwd TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    last_log_offset INTEGER DEFAULT 0,
    CHECK (session_kind != 'agent' OR agent_id IS NOT NULL),
    FOREIGN KEY(workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY(agent_id) REFERENCES agent_configs(id)
);

CREATE TABLE mux_windows (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    title TEXT,
    active_tab_id TEXT REFERENCES mux_tabs(id),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY(workspace_id) REFERENCES workspaces(id)
);

CREATE TABLE mux_tabs (
    id TEXT PRIMARY KEY,
    window_id TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    title TEXT NOT NULL,
    tab_index INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(window_id, tab_index),
    FOREIGN KEY(window_id) REFERENCES mux_windows(id),
    FOREIGN KEY(workspace_id) REFERENCES workspaces(id)
);

CREATE TABLE mux_layouts (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    tab_id TEXT NOT NULL,
    layout_json TEXT NOT NULL,
    active_pane_id TEXT REFERENCES mux_panes(id),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(tab_id),
    FOREIGN KEY(workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY(tab_id) REFERENCES mux_tabs(id)
);

CREATE TABLE mux_panes (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    tab_id TEXT NOT NULL,
    session_id TEXT,
    title TEXT NOT NULL,
    pane_kind TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY(workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY(tab_id) REFERENCES mux_tabs(id),
    FOREIGN KEY(session_id) REFERENCES sessions(id)
);

-- 복원 쿼리용 인덱스 (설계문서 §11.8)
CREATE INDEX idx_sessions_workspace_id ON sessions(workspace_id);
CREATE INDEX idx_mux_windows_workspace_id ON mux_windows(workspace_id);
CREATE INDEX idx_mux_tabs_window_id ON mux_tabs(window_id);
CREATE INDEX idx_mux_panes_session_id ON mux_panes(session_id);
";
