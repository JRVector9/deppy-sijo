//! runtime↔persist 배선 (세션 영속 파이프라인 — 설계문서 §11.1~11.5).
//! worker가 자체 SQLite 연결(WAL — UI의 Db와 다중 연결 안전)로
//! 세션 spawn/exit과 mux 구조 변경을 저장한다. 저장은 best-effort:
//! 실패는 warn으로 남기고 런타임 동작을 막지 않는다.
//!
//! 복원 UX(이전 layout 재구성 + respawn 의미론)는 이번 스코프 밖 —
//! 여기서 저장한 상태를 앱 시작 시 reconcile(PR-14)이 exited로 정리하고,
//! layout은 복원 기능이 소비할 수 있게 남는다.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Context;
use deppy_core::{MuxWindowId, SessionId};
use persist::{PaneState, SessionRow, TabState, WindowState};

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
    /// 이 실행의 mux window가 저장될 행 id — 이전 실행 것을 재사용해
    /// 실행마다 window 행이 누적되지 않게 한다.
    window_id: MuxWindowId,
    /// runtime SessionId(u64, 실행마다 리셋) → 영속 행 (id는 UUID)
    rows: HashMap<SessionId, SessionRow>,
}

impl PersistPipe {
    pub(crate) fn open(config: &PersistConfig) -> anyhow::Result<Self> {
        let conn = rusqlite::Connection::open(&config.db_path)
            .with_context(|| format!("persist DB 열기 실패: {}", config.db_path.display()))?;
        // 앱의 Db::open과 같은 연결 규약 (§11.9). 스키마는 앱이 이미 마이그레이션했다.
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        // 이전 실행의 window 행을 재사용 (없으면 새로)
        let window_id = persist::load_window_layouts(&conn, &config.workspace_id)
            .ok()
            .and_then(|windows| windows.into_iter().next().map(|w| w.id))
            .unwrap_or_else(MuxWindowId::new);
        Ok(Self {
            conn,
            workspace_id: config.workspace_id.clone(),
            window_id,
            rows: HashMap::new(),
        })
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
        if let Err(e) = persist::upsert_session(&self.conn, row) {
            tracing::warn!("세션 영속 실패 (exit): {e:#}");
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
