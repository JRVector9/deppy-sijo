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
    /// 이 세션의 **영속 UUID**(`sessions.id`) — 실행 사이에 안정적이다.
    ///
    /// `session_id`(u64)는 worker-로컬 카운터라 워커마다 1부터 재배정된다. 경계를 넘는
    /// 식별자(웹 프로토콜·알림 딥링크)는 이 UUID를 써야 앨리어싱이 생기지 않는다
    /// (v3.7 I1). 영속(persist)이 없는 워커(테스트 등)나 레거시 행은 None —
    /// 그 세션은 원격에서 표시 전용으로 강등된다.
    ///
    /// **wire**: 이 필드 추가로 MuxUpdated의 postcard 바이트가 바뀐다(구조체는 태그 없는
    /// 순차 인코딩). 원격은 PROTO_VERSION 정확 일치일 때만 접속을 수립하므로(remote.rs)
    /// 버전을 올려 구버전 클라를 fail-fast로 거부한다.
    pub persistent_session_id: Option<String>,
}
