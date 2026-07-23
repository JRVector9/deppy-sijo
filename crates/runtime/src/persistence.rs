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
    /// 시작 시 읽은 이전 세션 행. layout pane의 영속 session id와 결합해 fresh PTY를
    /// 같은 영속 세션/ANSI 로그에 다시 연결할 때 소비한다.
    restored_rows: HashMap<String, SessionRow>,
    /// 이전 실행이 저장한 tab 구조 — worker 시작 시 [`Self::take_saved_layout`]으로
    /// 한 번만 소비된다(복원 완료 후에는 빈 Vec).
    restored_tabs: Vec<TabState>,
    restored_active_tab: Option<MuxTabId>,
}

impl PersistPipe {
    pub(crate) fn open(config: &PersistConfig) -> anyhow::Result<Self> {
        Self::open_with_worker_config(config, DbWriteWorkerConfig::default())
    }

    /// batch writer 설정을 명시로 받는 open — 테스트에서 긴 flush_interval로 타이머 flush를
    /// 배제해 "drop이 flush했다"를 결정적으로 검증하는 데 쓴다.
    pub(crate) fn open_with_worker_config(
        config: &PersistConfig,
        worker_config: DbWriteWorkerConfig,
    ) -> anyhow::Result<Self> {
        let conn = rusqlite::Connection::open(&config.db_path)
            .with_context(|| format!("persist DB 열기 실패: {}", config.db_path.display()))?;
        // 앱의 Db::open과 같은 연결 규약 (§11.9). 스키마는 앱이 이미 마이그레이션했다.
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        // 동시 writer(app Db·다른 workspace 워커 shutdown 정리)와 겹칠 때 SQLITE_BUSY로
        // 쓰기가 유실되지 않게 대기·재시도한다 (codex 리뷰 — 전환 시 두 워커가 같은 파일에 씀).
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // 하나의 bounded read snapshot에서 canonical window와 그 pane이 실제 참조하는
        // session만 복원한다. 과거 window/session 전체를 startup RAM에 보유하지 않는다.
        let restore = persist::load_workspace_restore_bounded(&conn, &config.workspace_id)?;
        let (window_id, restored_tabs, restored_active_tab) = match restore.window {
            Some(w) => (w.id, w.tabs, w.active_tab),
            None => (MuxWindowId::new(), Vec::new(), None),
        };
        let restored_rows = restore
            .sessions
            .into_iter()
            .map(|row| (row.id.clone(), row))
            .collect();
        let (write_worker, write_handle) =
            match DbWriteWorker::spawn(&config.db_path, worker_config) {
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
            restored_rows,
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
        cwd: &str,
    ) {
        let row = SessionRow {
            id: uuid::Uuid::new_v4().to_string(),
            workspace_id: self.workspace_id.clone(),
            session_kind: kind.to_owned(),
            agent_id,
            title: title.to_owned(),
            command: command.to_owned(),
            args: args.to_vec(),
            // 실제 spawn 폴더 — 이전엔 앱 프로세스 current_dir("/")가 저장돼 복원이
            // 원래 폴더를 못 찾았다(A안 2026-07-08). live cd는 update_session_cwd가 따라간다.
            cwd: cwd.to_owned(),
            status: persist::SESSION_STATUS_RUNNING.to_owned(),
            last_log_offset: 0,
        };
        if let Err(e) = persist::upsert_session(&self.conn, &row) {
            tracing::warn!("세션 영속 실패 (spawn): {e:#}");
        }
        self.rows.insert(session, row);
    }

    /// 이전 pane이 가리키던 영속 세션 행을 fresh PTY에 재연결한다. UUID를 재사용하므로
    /// `logs/<workspace>/<uuid>/redacted.ansi.log`도 실행 사이에 끊기지 않는다.
    /// 손상/삭제로 이전 행이 없으면 안전하게 새 영속 세션으로 폴백한다.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn session_restored(
        &mut self,
        session: SessionId,
        persistent_id: &str,
        kind: &str,
        agent_id: Option<String>,
        title: &str,
        command: &str,
        args: &[String],
        cwd: &str,
    ) {
        let Some(mut row) = self.restored_rows.remove(persistent_id) else {
            tracing::warn!(persistent_id, "복원 세션 행 없음 — 새 영속 세션으로 폴백");
            self.session_spawned(session, kind, agent_id, title, command, args, cwd);
            return;
        };
        row.session_kind = kind.to_owned();
        row.agent_id = agent_id;
        row.title = title.to_owned();
        row.command = command.to_owned();
        row.args = args.to_vec();
        row.cwd = cwd.to_owned();
        row.status = persist::SESSION_STATUS_RUNNING.to_owned();
        if let Err(e) = persist::upsert_session(&self.conn, &row) {
            tracing::warn!(persistent_id, "세션 영속 실패 (restore): {e:#}");
        }
        self.rows.insert(session, row);
    }

    /// runtime SessionId에 결속된 영속 UUID. 로그 디렉터리 키로 사용한다.
    pub(crate) fn session_log_key(&self, session: SessionId) -> Option<&str> {
        self.rows.get(&session).map(|row| row.id.as_str())
    }

    /// 복원 대기 중인 이전 세션 행의 kind (행 소비 전 peek) — restore_pane이
    /// 셸 respawn / 열람 전용(archived) 복원을 분기하는 게이트 (PR-A2).
    pub(crate) fn restored_session_kind(&self, persistent_id: &str) -> Option<String> {
        self.restored_rows
            .get(persistent_id)
            .map(|row| row.session_kind.clone())
    }

    /// 열람 전용(archived) 복원 세션을 이전 UUID 행에 재결속한다 — kind/exited
    /// status를 **보존**한다 (`session_restored`는 kind를 shell·status를 running으로
    /// 덮어써 재사용 불가). 결속을 누락하면 다음 save_layout이 pane.session_id를
    /// None으로 저장해 이후 재시작부터 내용을 영구히 잃는다 (PR-A2 함정).
    pub(crate) fn session_rebound_archived(
        &mut self,
        session: SessionId,
        persistent_id: &str,
    ) -> bool {
        let Some(row) = self.restored_rows.remove(persistent_id) else {
            tracing::warn!(persistent_id, "archived 재결속 대상 행 없음");
            return false;
        };
        // DB 행은 이미 올바른 상태(kind/exited) — 쓰기 없이 메모리 결속만
        self.rows.insert(session, row);
        true
    }

    /// 세션의 현재 작업 폴더 갱신 — 감지 워커(lsof)가 관측한 live cd를 따라간다(A안).
    /// 복원 시 이 값으로 그 폴더에서 셸을 다시 띄운다.
    pub(crate) fn update_session_cwd(&mut self, session: SessionId, cwd: &str) {
        let Some(row) = self.rows.get_mut(&session) else {
            return;
        };
        if row.cwd == cwd {
            return;
        }
        row.cwd = cwd.to_owned();
        if let Err(e) = persist::upsert_session(&self.conn, row) {
            tracing::warn!("세션 cwd 영속 실패: {e:#}");
        }
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
                            cwd: None, // 저장 경로는 sessions 테이블이 원천 — 여기선 불필요
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
        session::SessionStatus::Idle => "idle",
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
        pipe.session_spawned(session, "shell", None, "shell", "/bin/sh", &[], "/tmp");
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

    /// 로드맵 C4: 메모리 압박 비상 플러시(EmergencyPersistFlush →
    /// flush_async_writes)가 **drop 없이** 배치를 커밋한다. OOM-kill은 Drop을
    /// 실행하지 않으므로, 이 경로가 커밋을 보장해야 재시작 시 무손실이다. drop-flush
    /// 테스트가 증명하지 못하는 "Drop 없는 내구성"을 정확히 메운다.
    #[test]
    fn 비상_플러시는_drop_없이_배치를_커밋한다() {
        let (dir, db_path) = temp_db("emergency-flush");
        let db = storage::Db::open(&db_path).unwrap();
        let workspace_id = db.create_workspace("runtime").unwrap();
        drop(db);

        let config = PersistConfig {
            db_path: db_path.clone(),
            workspace_id,
        };
        // 타이머 flush(50ms)를 배제 — status/offset이 커밋됐다면 그건 명시 flush의 효과다.
        let mut pipe = PersistPipe::open_with_worker_config(
            &config,
            DbWriteWorkerConfig {
                flush_interval: std::time::Duration::from_secs(3600),
                ..DbWriteWorkerConfig::default()
            },
        )
        .unwrap();
        let session = SessionId(1);
        // spawn은 동기 커밋(후속 read 의존), status/log_offset은 debounce 배치.
        pipe.session_spawned(session, "shell", None, "shell", "/bin/sh", &[], "/tmp");
        pipe.session_status(session, session::SessionStatus::Done);
        pipe.session_log_offset(session, 128);

        let read = || -> (String, i64) {
            rusqlite::Connection::open(&db_path)
                .unwrap()
                .query_row("SELECT status, last_log_offset FROM sessions", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .unwrap()
        };

        // 대조군: flush 전에는 배치가 아직 반영 안 됨 — 이 순간 SIGKILL이면 유실될 상태.
        let (status_before, offset_before) = read();
        assert_eq!(status_before, "running", "배치 status는 flush 전 미반영");
        assert_eq!(offset_before, 0, "배치 offset은 flush 전 미반영");

        // 실험군: 비상 플러시 핸들러가 부르는 flush_async_writes를 호출하면, drop 없이도
        // 값이 커밋되어 별도 커넥션에서 읽힌다.
        pipe.flush_async_writes().unwrap();
        let (status_after, offset_after) = read();
        assert_eq!(status_after, "done");
        assert_eq!(
            offset_after, 128,
            "비상 플러시가 drop 없이 최신 offset을 커밋한다"
        );

        // SIGKILL 모사 — Drop을 실행하지 않아도 위 값은 이미 내구적이다.
        std::mem::forget(pipe);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// crash-recovery 불변식: 명시적 flush 없이 pipe가 drop돼도(앱 종료 경로)
    /// 배치 버퍼에 남은 write가 유실되면 안 된다.
    #[test]
    fn persist_pipe는_drop시_pending_batch를_flush한다() {
        let (dir, db_path) = temp_db("drop-flush");
        let db = storage::Db::open(&db_path).unwrap();
        let workspace_id = db.create_workspace("runtime").unwrap();
        drop(db);

        // 타이머 flush(기본 50ms)를 사실상 무한대로 늘려 배제한다 — 그래야 drop 시점까지
        // batch가 pending이고, 값이 남아있다면 그건 **drop이 flush했다**는 결정적 증거다
        // (타이머/direct fallback으로 통과하는 false positive 제거, codex 리뷰).
        let mut pipe = PersistPipe::open_with_worker_config(
            &PersistConfig {
                db_path: db_path.clone(),
                workspace_id,
            },
            DbWriteWorkerConfig {
                flush_interval: std::time::Duration::from_secs(3600),
                ..DbWriteWorkerConfig::default()
            },
        )
        .unwrap();
        let session = SessionId(1);
        pipe.session_spawned(session, "shell", None, "shell", "/bin/sh", &[], "/tmp");
        pipe.session_status(session, session::SessionStatus::Done);
        pipe.session_log_offset(session, 64);
        // flush_async_writes 호출 없이 즉시 drop — 타이머가 안 도니 오직 drop만 flush 가능.
        drop(pipe);

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let (status, offset): (String, i64) = conn
            .query_row("SELECT status, last_log_offset FROM sessions", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(status, "done");
        assert_eq!(offset, 64);
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
