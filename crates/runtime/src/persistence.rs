//! runtime↔persist 배선 (세션 영속 파이프라인 — 설계문서 §11.1~11.5).
//! worker가 자체 SQLite 연결(WAL — UI의 Db와 다중 연결 안전)로
//! 세션 spawn/exit과 mux 구조 변경을 저장한다. 저장은 best-effort:
//! 실패는 warn으로 남기고 런타임 동작을 막지 않는다.
//!
//! 복원 UX(PR-14, §14): 이전 실행이 저장한 tab/pane 구조는 [`PersistPipe::open`]이
//! 읽어두고, worker가 [`PersistPipe::take_saved_layout`]으로 시작 시 한 번 소비한다
//! (in_process.rs의 `Worker::restore_saved_layout`). agent 세션 재실행은 하지
//! 않는다 — 복원은 항상 fresh 셸만 spawn한다(안전 요구사항).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Context;
use deppy_core::{MuxTabId, MuxWindowId, SessionId};
use persist::{PaneState, SessionRow, TabState, WindowState};
use storage::{DbWriteHandle, DbWriteStatsSnapshot, DbWriteWorker, DbWriteWorkerConfig};

/// InProcessRuntimeClient 생성 시 넘기는 영속 설정. None이면 영속 없음(테스트).
#[derive(Debug, Clone)]
pub struct PersistConfig {
    pub db_path: PathBuf,
    pub workspace_id: String,
}

/// worker 소유의 영속 파이프. 모든 메서드는 worker 스레드에서만 불린다.
pub(crate) struct PersistPipe {
    conn: rusqlite::Connection,
    workspace_id: String,
    /// Optional batched writer for hot metadata paths. Spawn/session rows and
    /// layout saves stay synchronous because later reads depend on them.
    _write_worker: Option<DbWriteWorker>,
    write_handle: Option<DbWriteHandle>,
    /// 이 실행의 mux window가 저장될 행 id — 이전 실행 것을 재사용해
    /// 실행마다 window 행이 누적되지 않게 한다.
    window_id: MuxWindowId,
    /// runtime SessionId(u64, 실행마다 리셋) → 영속 행 (id는 UUID)
    rows: HashMap<SessionId, SessionRow>,
    /// 이전 실행이 저장한 tab 구조 — worker 시작 시 [`Self::take_saved_layout`]으로
    /// 한 번만 소비된다(복원 완료 후에는 빈 Vec).
    restored_tabs: Vec<TabState>,
    restored_active_tab: Option<MuxTabId>,
}

impl PersistPipe {
    pub(crate) fn open(config: &PersistConfig) -> anyhow::Result<Self> {
        let conn = rusqlite::Connection::open(&config.db_path)
            .with_context(|| format!("persist DB 열기 실패: {}", config.db_path.display()))?;
        // 앱의 Db::open과 같은 연결 규약 (§11.9). 스키마는 앱이 이미 마이그레이션했다.
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        // 동시 writer(app Db·다른 workspace 워커 shutdown 정리)와 겹칠 때 SQLITE_BUSY로
        // 쓰기가 유실되지 않게 대기·재시도한다 (codex 리뷰 — 전환 시 두 워커가 같은 파일에 씀).
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // 이전 실행의 window 행을 재사용 (없으면 새로) — tab/pane 구조는 복원 UX(PR-14)가
        // take_saved_layout으로 소비한다.
        let previous = persist::load_window_layouts(&conn, &config.workspace_id)
            .ok()
            .and_then(|windows| windows.into_iter().next());
        let (window_id, restored_tabs, restored_active_tab) = match previous {
            Some(w) => (w.id, w.tabs, w.active_tab),
            None => (MuxWindowId::new(), Vec::new(), None),
        };
        let (write_worker, write_handle) =
            match DbWriteWorker::spawn(&config.db_path, DbWriteWorkerConfig::default()) {
                Ok(worker) => {
                    let handle = worker.handle();
                    (Some(worker), Some(handle))
                }
                Err(e) => {
                    tracing::warn!("DB batch writer 비활성 (worker 시작 실패): {e:#}");
                    (None, None)
                }
            };
        Ok(Self {
            conn,
            workspace_id: config.workspace_id.clone(),
            _write_worker: write_worker,
            write_handle,
            window_id,
            rows: HashMap::new(),
            restored_tabs,
            restored_active_tab,
        })
    }

    /// 이전 실행에서 저장된 tab/pane 구조를 반환한다 — worker 시작 시 정확히
    /// 한 번 소비된다(재호출 시 빈 결과). 저장된 window/tab이 없으면 (빈 Vec, None).
    pub(crate) fn take_saved_layout(&mut self) -> (Vec<TabState>, Option<MuxTabId>) {
        (
            std::mem::take(&mut self.restored_tabs),
            self.restored_active_tab.take(),
        )
    }

    /// 세션 spawn 기록. agent kind면 agent_id 필수 (스키마 CHECK).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn session_spawned(
        &mut self,
        session: SessionId,
        kind: &str,
        agent_id: Option<String>,
        title: &str,
        command: &str,
        args: &[String],
    ) {
        let row = SessionRow {
            id: uuid::Uuid::new_v4().to_string(),
            workspace_id: self.workspace_id.clone(),
            session_kind: kind.to_owned(),
            agent_id,
            title: title.to_owned(),
            command: command.to_owned(),
            args: args.to_vec(),
            cwd: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            status: persist::SESSION_STATUS_RUNNING.to_owned(),
            last_log_offset: 0,
        };
        if let Err(e) = persist::upsert_session(&self.conn, &row) {
            tracing::warn!("세션 영속 실패 (spawn): {e:#}");
        }
        self.rows.insert(session, row);
    }

    /// 세션 종료 기록 — spawn 때 저장한 행의 status만 바꿔 다시 쓴다.
    pub(crate) fn session_exited(&mut self, session: SessionId) {
        let Some(row) = self.rows.get_mut(&session) else {
            return;
        };
        row.status = persist::SESSION_STATUS_EXITED.to_owned();
        if let Some(handle) = &self.write_handle {
            match handle.try_update_session_status(row.id.clone(), row.status.clone()) {
                Ok(()) => return,
                Err(e) => tracing::warn!("세션 status batch enqueue 실패 — direct fallback: {e:#}"),
            }
        }
        if let Err(e) = persist::upsert_session(&self.conn, row) {
            tracing::warn!("세션 영속 실패 (exit direct fallback): {e:#}");
        }
    }

    pub(crate) fn session_status(&mut self, session_id: SessionId, status: session::SessionStatus) {
        let Some(row) = self.rows.get_mut(&session_id) else {
            return;
        };
        row.status = session_status_to_persist(status).to_owned();
        if let Some(handle) = &self.write_handle {
            match handle.try_update_session_status(row.id.clone(), row.status.clone()) {
                Ok(()) => return,
                Err(e) => tracing::warn!("세션 status batch enqueue 실패 — direct fallback: {e:#}"),
            }
        }
        if let Err(e) = persist::upsert_session(&self.conn, row) {
            tracing::warn!("세션 status 영속 실패 (direct fallback): {e:#}");
        }
    }

    /// Redacted ANSI log offset progress. Hot output paths use the batched
    /// writer; if enqueue fails, fall back to direct update so crash recovery
    /// does not regress.
    pub(crate) fn session_log_offset(&mut self, session: SessionId, offset: u64) {
        let Some(row) = self.rows.get_mut(&session) else {
            return;
        };
        row.last_log_offset = row.last_log_offset.max(offset);
        if let Some(handle) = &self.write_handle {
            match handle.try_update_session_log_offset(row.id.clone(), row.last_log_offset) {
                Ok(()) => return,
                Err(e) => {
                    tracing::warn!("세션 log offset batch enqueue 실패 — direct fallback: {e:#}")
                }
            }
        }
        if let Err(e) = persist::update_session_log_offset(&self.conn, &row.id, row.last_log_offset)
        {
            tracing::warn!("세션 log offset 영속 실패 (direct fallback): {e:#}");
        }
    }

    pub(crate) fn flush_async_writes(&self) -> anyhow::Result<Option<DbWriteStatsSnapshot>> {
        match &self.write_handle {
            Some(handle) => handle.flush().map(Some),
            None => Ok(None),
        }
    }

    /// mux 구조를 저장한다 — 구조 변경(탭/pane/포커스) 명령 처리 후 호출된다.
    /// 저빈도 이벤트라 매번 전체 저장으로 충분하다.
    pub(crate) fn save_layout(
        &mut self,
        window: &mux::MuxWindow,
        tabs: &std::collections::HashMap<deppy_core::MuxTabId, mux::MuxTab>,
        panes: &std::collections::HashMap<deppy_core::MuxPaneId, mux::MuxPane>,
    ) {
        let state = WindowState {
            id: self.window_id.clone(),
            title: None,
            active_tab: window.active_tab.clone(),
            tabs: window
                .tabs
                .iter()
                .filter_map(|tab_id| tabs.get(tab_id))
                .map(|tab| TabState {
                    id: tab.id.clone(),
                    title: tab.title.clone(),
                    layout: tab.layout.clone(),
                    active_pane: tab.active_pane.clone(),
                    panes: tab
                        .layout
                        .panes()
                        .into_iter()
                        .filter_map(|pane_id| panes.get(&pane_id))
                        .map(|pane| PaneState {
                            id: pane.id.clone(),
                            session_id: pane
                                .session_id
                                .and_then(|s| self.rows.get(&s))
                                .map(|row| row.id.clone()),
                            title: pane.title.clone(),
                            pane_kind: pane.pane_kind,
                        })
                        .collect(),
                })
                .collect(),
        };
        if let Err(e) = persist::save_window_layout(&mut self.conn, &self.workspace_id, &state) {
            tracing::warn!("mux layout 영속 실패: {e:#}");
        }
    }
}

fn session_status_to_persist(status: session::SessionStatus) -> &'static str {
    match status {
        session::SessionStatus::Running => persist::SESSION_STATUS_RUNNING,
        session::SessionStatus::Waiting => "waiting",
        session::SessionStatus::NeedsApproval => "needs_approval",
        session::SessionStatus::Error => "error",
        session::SessionStatus::Done => "done",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "deppy-runtime-persist-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        (dir, path)
    }

    #[test]
    fn persist_pipe는_status와_log_offset을_batch_writer로_보낸다() {
        let (dir, db_path) = temp_db("batch");
        let db = storage::Db::open(&db_path).unwrap();
        let workspace_id = db.create_workspace("runtime").unwrap();
        drop(db);

        let mut pipe = PersistPipe::open(&PersistConfig {
            db_path: db_path.clone(),
            workspace_id,
        })
        .unwrap();
        let session = SessionId(1);
        pipe.session_spawned(session, "shell", None, "shell", "/bin/sh", &[]);
        pipe.session_status(session, session::SessionStatus::Waiting);
        pipe.session_status(session, session::SessionStatus::Done);
        pipe.session_log_offset(session, 12);
        pipe.session_log_offset(session, 7);
        pipe.session_log_offset(session, 128);

        let stats = pipe.flush_async_writes().unwrap().unwrap();
        assert_eq!(stats.status_enqueued, 2);
        assert_eq!(stats.status_coalesced, 1);
        assert_eq!(stats.status_flushed, 1);
        assert_eq!(stats.log_offset_enqueued, 3);
        assert_eq!(stats.log_offset_coalesced, 2);
        assert_eq!(stats.log_offset_flushed, 1);

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let (status, offset): (String, i64) = conn
            .query_row("SELECT status, last_log_offset FROM sessions", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(status, "done");
        assert_eq!(offset, 128);
        drop(conn);
        drop(pipe);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
