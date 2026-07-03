use deppy_core::{MuxPaneId, MuxTabId, SessionId};

use crate::layout_tree::LayoutNode;

/// UI가 렌더에 쓰는 mux 상태의 읽기 전용 스냅샷 (설계문서 2.1 —
/// UI는 상태를 보여주고 명령을 보낸다). runtime worker가 조립해 push한다.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MuxSnapshot {
    /// tab bar 순서
    pub tabs: Vec<TabSnapshot>,
    pub active_tab: Option<MuxTabId>,
    pub focused_pane: Option<MuxPaneId>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TabSnapshot {
    pub id: MuxTabId,
    pub title: String,
    pub layout: LayoutNode,
    pub panes: Vec<PaneSnapshot>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PaneSnapshot {
    pub id: MuxPaneId,
    pub session_id: Option<SessionId>,
    pub title: String,
}
