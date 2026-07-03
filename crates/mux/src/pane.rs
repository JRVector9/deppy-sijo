use deppy_core::{MuxPaneId, SessionId};

/// 설계문서 5.2 MuxPane. pane은 화면 영역, session은 프로세스 —
/// 분리되어 있고 연결은 Option (PR-07 완료 기준).
pub struct MuxPane {
    pub id: MuxPaneId,
    pub session_id: Option<SessionId>,
    pub title: String,
    pub pane_kind: PaneKind,
}

/// v0은 terminal pane만. (agent/mcp 전용 뷰 등은 후속 PR에서 확장)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneKind {
    Terminal,
}

impl MuxPane {
    pub fn new(id: MuxPaneId, title: String) -> Self {
        Self {
            id,
            session_id: None,
            title,
            pane_kind: PaneKind::Terminal,
        }
    }
}
