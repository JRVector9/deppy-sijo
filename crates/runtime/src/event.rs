use std::sync::Arc;

use terminal::TerminalViewportSnapshot;

use crate::command::SessionId;

/// SpawnFailed의 출처 구분 — 셸/에이전트 UI가 서로의 실패를 오귀속하지 않게 한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnKind {
    Shell,
    Agent,
}

/// Runtime → UI 이벤트 (설계문서 2.1).
/// Viewport는 output batch 주기(설계문서 10.1)마다 push된다 —
/// remote 전환 시 terminal delta 스트림으로 대체되는 자리 (8.2).
#[derive(Clone)]
pub enum RuntimeEvent {
    ShellSpawned {
        session: SessionId,
    },
    /// SpawnAgent 성공 — 셸과 소유 UI가 다르므로 이벤트를 구분한다
    AgentSpawned {
        session: SessionId,
    },
    SpawnFailed {
        kind: SpawnKind,
        message: String,
    },
    Viewport {
        session: SessionId,
        snapshot: Arc<TerminalViewportSnapshot>,
        /// 입력 매핑(bracketed paste wrap)에 필요한 터미널 모드
        bracketed_paste: bool,
    },
    SessionExited {
        session: SessionId,
        exit_code: Option<u32>,
    },
}
