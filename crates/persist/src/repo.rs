//! mux layout / session metadata repository (설계문서 §11.1~11.5).
//! layout의 source of truth는 mux_windows/mux_tabs/mux_layouts/mux_panes —
//! sessions에는 layout_json을 두지 않는다 (§11.1).

use std::collections::{HashMap, HashSet};

use anyhow::{Context, bail};
use deppy_core::{MuxPaneId, MuxTabId, MuxWindowId};
use mux::{LayoutNode, PaneKind};
use rusqlite::{Connection, OptionalExtension};

use crate::layout_json::{layout_from_json, layout_to_json};

/// sessions.status 값. 영속 컬럼은 소문자 문자열 — crash recovery의
/// orphan 판정(running → exited)이 이 두 값을 쓴다.
pub const SESSION_STATUS_RUNNING: &str = "running";
pub const SESSION_STATUS_EXITED: &str = "exited";

/// mux_windows 한 행 + 소속 tab 전체. 복원 시 이대로 MuxWindow/MuxTab을 재구성한다.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowState {
    pub id: MuxWindowId,
    pub title: Option<String>,
    pub active_tab: Option<MuxTabId>,
    /// tab bar 순서 (mux_tabs.tab_index가 source of truth)
    pub tabs: Vec<TabState>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TabState {
    pub id: MuxTabId,
    pub title: String,
    pub layout: LayoutNode,
    pub active_pane: Option<MuxPaneId>,
    /// layout 좌→우 DFS 순서 (load 시 이 순서로 정규화된다)
    pub panes: Vec<PaneState>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PaneState {
    pub id: MuxPaneId,
    /// 영속 session id (sessions.id). runtime SessionId(u64)와의 매핑은 호출측 몫.
    pub session_id: Option<String>,
    pub title: String,
    pub pane_kind: PaneKind,
    /// 이 pane 세션의 마지막 작업 폴더(sessions.cwd JOIN) — 복원 시 그 폴더에서
    /// 셸을 띄우기 위함(A안 2026-07-08). 세션 행이 없거나 미기록이면 None.
    pub cwd: Option<String>,
}

/// sessions 테이블 한 행 (§11.1). created_at/updated_at은 SQLite가 기록한다.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    pub id: String,
    pub workspace_id: String,
    /// agent | shell | mcp — agent면 agent_id 필수 (DDL CHECK)
    pub session_kind: String,
    pub agent_id: Option<String>,
    pub title: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub status: String,
    pub last_log_offset: u64,
}

fn pane_kind_to_str(kind: PaneKind) -> &'static str {
    match kind {
        PaneKind::Terminal => "terminal",
    }
}

fn pane_kind_from_str(s: &str) -> PaneKind {
    match s {
        "terminal" => PaneKind::Terminal,
        // v0은 terminal뿐 — 알 수 없는 kind는 경고 후 terminal로 복원
        other => {
            tracing::warn!(pane_kind = other, "알 수 없는 pane_kind — terminal로 복원");
            PaneKind::Terminal
        }
    }
}

/// window 하나의 mux 상태 전체를 한 트랜잭션으로 저장한다.
///
/// 상호 참조(§11.5 참고: mux_windows.active_tab_id ↔ mux_tabs.window_id) 순서:
/// window upsert(active_tab_id NULL) → 기존 하위 행 삭제(layouts → panes → tabs)
/// → tabs/panes/layouts insert → window active_tab_id update.
pub fn save_window_layout(
    conn: &mut Connection,
    workspace_id: &str,
    window: &WindowState,
) -> anyhow::Result<()> {
    // 저장 전 정합성 — 어긋난 상태를 쓰면 load가 tab을 통째로 skip하므로 여기서 거부한다.
    // active_tab은 이 window의 tab이어야 한다 (FK는 mux_tabs 존재만 보장, 소속은 미보장)
    if let Some(active) = &window.active_tab
        && !window.tabs.iter().any(|t| &t.id == active)
    {
        bail!("active_tab이 window의 tab 목록에 없음: {}", active.0);
    }
    for tab in &window.tabs {
        // layout의 pane 집합 == pane 목록 집합, 중복 참조 금지
        let layout_panes = tab.layout.panes();
        let layout_set: HashSet<&str> = layout_panes.iter().map(|p| p.0.as_str()).collect();
        if layout_set.len() != layout_panes.len() {
            bail!("layout이 같은 pane을 중복 참조: tab {}", tab.id.0);
        }
        let pane_ids: HashSet<&str> = tab.panes.iter().map(|p| p.id.0.as_str()).collect();
        if layout_set != pane_ids {
            bail!("layout과 pane 목록 불일치: tab {}", tab.id.0);
        }
        // active_pane도 이 tab의 layout 안이어야 한다 (FK는 mux_panes 존재만 보장)
        if let Some(active) = &tab.active_pane
            && !tab.layout.contains(active)
        {
            bail!("active_pane이 tab layout에 없음: {}", active.0);
        }
    }

    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO mux_windows (id, workspace_id, title, active_tab_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, NULL,
            strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))
         ON CONFLICT(id) DO UPDATE SET
            workspace_id = excluded.workspace_id,
            title = excluded.title,
            active_tab_id = NULL,
            updated_at = excluded.updated_at",
        (&window.id.0, workspace_id, &window.title),
    )
    .with_context(|| format!("mux window 저장 실패: {}", window.id.0))?;

    // 기존 하위 행은 FK 역순으로 지우고 새로 쓴다 (tab/pane 삭제·재배치 반영)
    tx.execute(
        "DELETE FROM mux_layouts
         WHERE tab_id IN (SELECT id FROM mux_tabs WHERE window_id = ?1)",
        [&window.id.0],
    )?;
    tx.execute(
        "DELETE FROM mux_panes
         WHERE tab_id IN (SELECT id FROM mux_tabs WHERE window_id = ?1)",
        [&window.id.0],
    )?;
    tx.execute("DELETE FROM mux_tabs WHERE window_id = ?1", [&window.id.0])?;

    for (index, tab) in window.tabs.iter().enumerate() {
        tx.execute(
            "INSERT INTO mux_tabs (id, window_id, workspace_id, title, tab_index, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5,
                strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
            (&tab.id.0, &window.id.0, workspace_id, &tab.title, index as i64),
        )
        .with_context(|| format!("mux tab 저장 실패: {}", tab.id.0))?;

        for pane in &tab.panes {
            // pane이 참조하는 session은 같은 workspace 것이어야 한다 — FK는
            // 존재만 보장하므로 여기서 소속을 확인한다 (codex 리뷰 반영:
            // window를 다른 workspace로 저장하면 복원이 세션을 못 찾는다)
            if let Some(session_id) = &pane.session_id {
                let session_ws: Option<String> = tx
                    .query_row(
                        "SELECT workspace_id FROM sessions WHERE id = ?1",
                        [session_id],
                        |row| row.get(0),
                    )
                    .optional()
                    .with_context(|| format!("pane session 조회 실패: {session_id}"))?;
                if session_ws.as_deref() != Some(workspace_id) {
                    bail!(
                        "pane {}의 session {}이 workspace {}에 속하지 않음",
                        pane.id.0,
                        session_id,
                        workspace_id
                    );
                }
            }
            tx.execute(
                "INSERT INTO mux_panes (id, workspace_id, tab_id, session_id, title, pane_kind, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6,
                    strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
                (
                    &pane.id.0,
                    workspace_id,
                    &tab.id.0,
                    &pane.session_id,
                    &pane.title,
                    pane_kind_to_str(pane.pane_kind),
                ),
            )
            .with_context(|| format!("mux pane 저장 실패: {}", pane.id.0))?;
        }

        // active_pane_id가 mux_panes를 참조하므로 pane insert 뒤에
        tx.execute(
            "INSERT INTO mux_layouts (id, workspace_id, tab_id, layout_json, active_pane_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5,
                strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
            (
                uuid::Uuid::new_v4().to_string(),
                workspace_id,
                &tab.id.0,
                layout_to_json(&tab.layout)?,
                tab.active_pane.as_ref().map(|p| p.0.as_str()),
            ),
        )
        .with_context(|| format!("mux layout 저장 실패: tab {}", tab.id.0))?;
    }

    if let Some(active) = &window.active_tab {
        tx.execute(
            "UPDATE mux_windows SET active_tab_id = ?2 WHERE id = ?1",
            (&window.id.0, &active.0),
        )
        .context("active_tab_id 갱신 실패")?;
    }

    tx.commit()
        .with_context(|| format!("mux window 저장 커밋 실패: {}", window.id.0))
}

/// workspace의 window 전체를 복원한다. 손상 행(layout_json 파싱 실패,
/// layout↔pane 불일치)은 tab 단위로 skip + 경고 — 나머지 복원을 막지 않는다.
pub fn load_window_layouts(
    conn: &Connection,
    workspace_id: &str,
) -> anyhow::Result<Vec<WindowState>> {
    let mut stmt = conn.prepare(
        "SELECT id, title, active_tab_id FROM mux_windows
         WHERE workspace_id = ?1 ORDER BY created_at, id",
    )?;
    let windows = stmt
        .query_map([workspace_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut out = Vec::new();
    for (window_id, title, active_tab_id) in windows {
        let tabs = load_tabs(conn, &window_id)?;
        // 손상 tab이 skip됐을 수 있다 — 없는 active tab은 첫 tab으로 보정
        let active_tab = active_tab_id
            .map(MuxTabId)
            .filter(|id| tabs.iter().any(|t| &t.id == id))
            .or_else(|| tabs.first().map(|t| t.id.clone()));
        out.push(WindowState {
            id: MuxWindowId(window_id),
            title,
            active_tab,
            tabs,
        });
    }
    Ok(out)
}

fn load_tabs(conn: &Connection, window_id: &str) -> anyhow::Result<Vec<TabState>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.title, l.layout_json, l.active_pane_id
         FROM mux_tabs t LEFT JOIN mux_layouts l ON l.tab_id = t.id
         WHERE t.window_id = ?1 ORDER BY t.tab_index",
    )?;
    let rows = stmt
        .query_map([window_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut out = Vec::new();
    for (tab_id, title, layout_json, active_pane_id) in rows {
        let Some(json) = layout_json else {
            tracing::warn!(tab_id, "layout 행 없음 — tab 복원 skip");
            continue;
        };
        let layout = match layout_from_json(&json) {
            Ok(layout) => layout,
            Err(e) => {
                tracing::warn!(tab_id, "layout_json 손상 — tab 복원 skip: {e}");
                continue;
            }
        };
        let Some(panes) = load_panes(conn, &tab_id, &layout)? else {
            continue; // layout↔pane 불일치 (load_panes에서 경고)
        };
        let active_pane = active_pane_id
            .map(MuxPaneId)
            .filter(|id| layout.contains(id))
            .or_else(|| layout.panes().into_iter().next());
        out.push(TabState {
            id: MuxTabId(tab_id),
            title,
            layout,
            active_pane,
            panes,
        });
    }
    Ok(out)
}

/// tab의 pane 행을 layout DFS 순서로 정렬해 돌려준다.
/// layout이 참조하는 pane 행이 없으면 복원 불가 — None (호출측이 tab skip).
fn load_panes(
    conn: &Connection,
    tab_id: &str,
    layout: &LayoutNode,
) -> anyhow::Result<Option<Vec<PaneState>>> {
    // sessions.cwd를 JOIN — 복원 시 pane별 마지막 작업 폴더로 셸을 띄운다(A안).
    let mut stmt = conn.prepare(
        "SELECT p.id, p.session_id, p.title, p.pane_kind, s.cwd
         FROM mux_panes p LEFT JOIN sessions s ON s.id = p.session_id
         WHERE p.tab_id = ?1",
    )?;
    let mut by_id: HashMap<String, PaneState> = stmt
        .query_map([tab_id], |row| {
            let id: String = row.get(0)?;
            Ok((
                id.clone(),
                PaneState {
                    id: MuxPaneId(id),
                    session_id: row.get(1)?,
                    title: row.get(2)?,
                    pane_kind: pane_kind_from_str(&row.get::<_, String>(3)?),
                    cwd: row.get::<_, Option<String>>(4)?.filter(|c| !c.is_empty()),
                },
            ))
        })?
        .collect::<Result<_, _>>()?;

    let mut panes = Vec::new();
    for pane_id in layout.panes() {
        match by_id.remove(&pane_id.0) {
            Some(pane) => panes.push(pane),
            None => {
                tracing::warn!(tab_id, pane_id = %pane_id.0, "layout의 pane 행 없음 — tab 복원 skip");
                return Ok(None);
            }
        }
    }
    for orphan in by_id.keys() {
        tracing::warn!(tab_id, pane_id = %orphan, "layout에 없는 pane 행 — 버림");
    }
    Ok(Some(panes))
}

/// session metadata 저장 (§11.1). 같은 id면 갱신 — status/offset 업데이트에도 쓴다.
pub fn upsert_session(conn: &Connection, row: &SessionRow) -> anyhow::Result<()> {
    let args_json = serde_json::to_string(&row.args)?;
    conn.execute(
        "INSERT INTO sessions
           (id, workspace_id, session_kind, agent_id, title, command, args_json,
            cwd, status, last_log_offset, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
            strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))
         ON CONFLICT(id) DO UPDATE SET
            workspace_id = excluded.workspace_id,
            session_kind = excluded.session_kind,
            agent_id = excluded.agent_id,
            title = excluded.title,
            command = excluded.command,
            args_json = excluded.args_json,
            cwd = excluded.cwd,
            status = excluded.status,
            last_log_offset = excluded.last_log_offset,
            updated_at = excluded.updated_at",
        (
            &row.id,
            &row.workspace_id,
            &row.session_kind,
            &row.agent_id,
            &row.title,
            &row.command,
            &args_json,
            &row.cwd,
            &row.status,
            i64::try_from(row.last_log_offset).unwrap_or(i64::MAX),
        ),
    )
    .with_context(|| format!("session 저장 실패: {}", row.id))?;
    Ok(())
}

pub fn load_sessions(conn: &Connection, workspace_id: &str) -> anyhow::Result<Vec<SessionRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, workspace_id, session_kind, agent_id, title, command, args_json,
                cwd, status, last_log_offset
         FROM sessions WHERE workspace_id = ?1 ORDER BY created_at, id",
    )?;
    let rows = stmt
        .query_map([workspace_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, Option<i64>>(9)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // 손상 행 하나가 전체 복원을 죽이지 않게 skip + 경고 (app/storage.rs 스타일)
    let mut out = Vec::new();
    for (
        id,
        workspace_id,
        session_kind,
        agent_id,
        title,
        command,
        args_json,
        cwd,
        status,
        offset,
    ) in rows
    {
        match serde_json::from_str(&args_json) {
            Ok(args) => out.push(SessionRow {
                id,
                workspace_id,
                session_kind,
                agent_id,
                title,
                command,
                args,
                cwd,
                status,
                last_log_offset: offset.and_then(|v| u64::try_from(v).ok()).unwrap_or(0),
            }),
            Err(e) => tracing::warn!(session_id = %id, "args_json 파싱 실패 — 행 무시: {e}"),
        }
    }
    Ok(out)
}

/// log offset만 갱신한다 (validate_log_offset 보정치 반영 / 주기적 index 진행 기록용).
pub fn update_session_log_offset(
    conn: &Connection,
    session_id: &str,
    offset: u64,
) -> anyhow::Result<()> {
    let affected = conn
        .execute(
            "UPDATE sessions SET last_log_offset = ?2,
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
         WHERE id = ?1",
            (session_id, i64::try_from(offset).unwrap_or(i64::MAX)),
        )
        .with_context(|| format!("log offset 갱신 실패: {session_id}"))?;
    // 없는 세션에 조용히 성공하면 offset 보정 실패가 숨는다 (codex 리뷰 반영)
    anyhow::ensure!(affected == 1, "log offset 갱신 대상 없음: {session_id}");
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use deppy_core::{MuxPaneId, MuxTabId, MuxWindowId};
    use mux::SplitDirection;

    use super::*;

    /// 앱 선행 마이그레이션이 만드는 FK 대상 테이블의 최소 스텁 + MIGRATION_SQL 적용.
    pub(crate) fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        setup_schema(&conn);
        conn
    }

    pub(crate) fn setup_schema(conn: &Connection) {
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS workspaces (id TEXT PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS agent_configs (id TEXT PRIMARY KEY);",
        )
        .unwrap();
        if !migrated(conn) {
            conn.execute_batch(crate::MIGRATION_SQL).unwrap();
        }
        conn.execute("INSERT OR IGNORE INTO workspaces (id) VALUES ('ws-1')", [])
            .unwrap();
    }

    fn migrated(conn: &Connection) -> bool {
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='sessions'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
            > 0
    }

    pub(crate) fn sample_session(id: &str, status: &str) -> SessionRow {
        SessionRow {
            id: id.into(),
            workspace_id: "ws-1".into(),
            session_kind: "shell".into(),
            agent_id: None,
            title: "셸".into(),
            command: "zsh".into(),
            args: vec!["-l".into()],
            cwd: "/tmp".into(),
            status: status.into(),
            last_log_offset: 42,
        }
    }

    /// 2 tab / 중첩 split / session 연결이 있는 window 상태.
    pub(crate) fn sample_window() -> WindowState {
        let (a, b, c) = (MuxPaneId::new(), MuxPaneId::new(), MuxPaneId::new());
        let mut layout = LayoutNode::Pane(a.clone());
        layout.split_pane(&a, SplitDirection::Horizontal, b.clone());
        layout.split_pane(&b, SplitDirection::Vertical, c.clone());

        let tab1 = TabState {
            id: MuxTabId::new(),
            title: "빌드".into(),
            layout,
            active_pane: Some(b.clone()),
            panes: [&a, &b, &c] // layout DFS 순서
                .iter()
                .map(|id| PaneState {
                    id: (*id).clone(),
                    session_id: (*id == &b).then(|| "sess-1".to_owned()),
                    title: "pane".into(),
                    pane_kind: PaneKind::Terminal,
                    // sess-1 pane은 sessions.cwd("/tmp") JOIN 결과와 일치해야 round-trip
                    cwd: (*id == &b).then(|| "/tmp".to_owned()),
                })
                .collect(),
        };
        let d = MuxPaneId::new();
        let tab2 = TabState {
            id: MuxTabId::new(),
            title: "로그".into(),
            layout: LayoutNode::Pane(d.clone()),
            active_pane: Some(d.clone()),
            panes: vec![PaneState {
                id: d,
                session_id: None,
                title: "pane".into(),
                pane_kind: PaneKind::Terminal,
                cwd: None,
            }],
        };
        WindowState {
            id: MuxWindowId::new(),
            title: Some("메인".into()),
            active_tab: Some(tab2.id.clone()),
            tabs: vec![tab1, tab2],
        }
    }

    #[test]
    fn window_layout_저장_복원_round_trip() {
        let mut conn = test_conn();
        upsert_session(&conn, &sample_session("sess-1", SESSION_STATUS_RUNNING)).unwrap();
        let window = sample_window();
        save_window_layout(&mut conn, "ws-1", &window).unwrap();

        let loaded = load_window_layouts(&conn, "ws-1").unwrap();
        assert_eq!(loaded, vec![window]);
        // 다른 workspace에서는 안 보인다
        conn.execute("INSERT INTO workspaces (id) VALUES ('ws-2')", [])
            .unwrap();
        assert!(load_window_layouts(&conn, "ws-2").unwrap().is_empty());
    }

    #[test]
    fn 재저장은_기존_상태를_대체한다() {
        let mut conn = test_conn();
        upsert_session(&conn, &sample_session("sess-1", SESSION_STATUS_RUNNING)).unwrap();
        let mut window = sample_window();
        save_window_layout(&mut conn, "ws-1", &window).unwrap();

        // tab 하나 닫고 활성 tab 변경 후 재저장 (upsert)
        window.tabs.remove(1);
        window.active_tab = Some(window.tabs[0].id.clone());
        save_window_layout(&mut conn, "ws-1", &window).unwrap();

        let loaded = load_window_layouts(&conn, "ws-1").unwrap();
        assert_eq!(loaded, vec![window.clone()]);
        // stale 행이 남지 않는다
        let tabs: i64 = conn
            .query_row("SELECT count(*) FROM mux_tabs", [], |r| r.get(0))
            .unwrap();
        let layouts: i64 = conn
            .query_row("SELECT count(*) FROM mux_layouts", [], |r| r.get(0))
            .unwrap();
        let panes: i64 = conn
            .query_row("SELECT count(*) FROM mux_panes", [], |r| r.get(0))
            .unwrap();
        assert_eq!((tabs, layouts, panes), (1, 1, 3));

        // 다른 workspace로의 재저장은 pane이 참조하는 session의 소속과 어긋나므로
        // 거부된다 (codex 리뷰 반영 — 복원 시 load_sessions와 불일치 방지).
        // session까지 그 workspace로 옮긴 뒤에만 이동이 가능하다.
        conn.execute("INSERT INTO workspaces (id) VALUES ('ws-2')", [])
            .unwrap();
        assert!(save_window_layout(&mut conn, "ws-2", &window).is_err());
        let mut moved = sample_session("sess-1", SESSION_STATUS_RUNNING);
        moved.workspace_id = "ws-2".to_owned();
        upsert_session(&conn, &moved).unwrap();
        save_window_layout(&mut conn, "ws-2", &window).unwrap();
        assert!(load_window_layouts(&conn, "ws-1").unwrap().is_empty());
        assert_eq!(load_window_layouts(&conn, "ws-2").unwrap(), vec![window]);
    }

    #[test]
    fn 어긋난_상태는_저장_거부() {
        let mut conn = test_conn();

        // layout이 참조하는 pane이 목록에 없음
        let mut window = sample_window();
        window.tabs[0].panes.pop();
        assert!(save_window_layout(&mut conn, "ws-1", &window).is_err());

        // layout이 같은 pane을 중복 참조
        let mut window = sample_window();
        let dup = window.tabs[1].panes[0].id.clone();
        window.tabs[1].layout = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutNode::Pane(dup.clone())),
            second: Box::new(LayoutNode::Pane(dup)),
        };
        assert!(save_window_layout(&mut conn, "ws-1", &window).is_err());

        // active_tab이 이 window의 tab이 아님
        let mut window = sample_window();
        window.active_tab = Some(MuxTabId::new());
        assert!(save_window_layout(&mut conn, "ws-1", &window).is_err());

        // active_pane이 다른 tab의 pane (FK 존재성만으로는 못 거르는 cross-tab 참조)
        let mut window = sample_window();
        window.tabs[1].active_pane = Some(window.tabs[0].panes[0].id.clone());
        assert!(save_window_layout(&mut conn, "ws-1", &window).is_err());
    }

    #[test]
    fn 손상_layout_json은_tab_단위_skip() {
        let mut conn = test_conn();
        upsert_session(&conn, &sample_session("sess-1", SESSION_STATUS_RUNNING)).unwrap();
        let window = sample_window();
        save_window_layout(&mut conn, "ws-1", &window).unwrap();
        // tab1의 layout_json을 손상시킨다
        conn.execute(
            "UPDATE mux_layouts SET layout_json = 'broken' WHERE tab_id = ?1",
            [&window.tabs[0].id.0],
        )
        .unwrap();

        let loaded = load_window_layouts(&conn, "ws-1").unwrap();
        assert_eq!(loaded.len(), 1);
        // tab1은 skip, tab2만 복원 — active tab은 살아남은 tab으로 보정
        assert_eq!(loaded[0].tabs, vec![window.tabs[1].clone()]);
        assert_eq!(loaded[0].active_tab, Some(window.tabs[1].id.clone()));
    }

    #[test]
    fn session_metadata_round_trip과_upsert() {
        let conn = test_conn();
        let mut row = sample_session("sess-1", SESSION_STATUS_RUNNING);
        upsert_session(&conn, &row).unwrap();
        assert_eq!(load_sessions(&conn, "ws-1").unwrap(), vec![row.clone()]);

        // 같은 id 갱신
        row.status = SESSION_STATUS_EXITED.into();
        row.title = "종료된 셸".into();
        upsert_session(&conn, &row).unwrap();
        assert_eq!(load_sessions(&conn, "ws-1").unwrap(), vec![row.clone()]);

        // offset만 갱신
        update_session_log_offset(&conn, "sess-1", 128).unwrap();
        assert_eq!(
            load_sessions(&conn, "ws-1").unwrap()[0].last_log_offset,
            128
        );
    }

    #[test]
    fn agent_session은_agent_id가_필수() {
        let conn = test_conn();
        let mut row = sample_session("sess-1", SESSION_STATUS_RUNNING);
        row.session_kind = "agent".into();
        // DDL CHECK: agent인데 agent_id 없음 → 거부
        assert!(upsert_session(&conn, &row).is_err());
    }
}
