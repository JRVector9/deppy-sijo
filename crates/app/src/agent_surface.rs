//! Provider-neutral projection shared by agent UI, actions, and notifications.
//!
//! PTY agents and structured App Server threads intentionally keep their own
//! lifecycle implementations.  This module is the small common contract above
//! those implementations; it must not become another source of truth.

// PR-01 establishes the cross-track contract before the UI/action/notification
// consumers land. Remove this allowance once those parallel PRs are integrated.
#![allow(dead_code)]

use crate::agent_detect::AgentKind;
use crate::agent_session::AgentSessionStatus;

/// AI provider shown to the user independently from the transport used to run it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentProvider {
    Codex,
    Claude,
    Kimi,
}

impl AgentProvider {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::Kimi => "Kimi",
        }
    }
}

impl From<AgentKind> for AgentProvider {
    fn from(value: AgentKind) -> Self {
        match value {
            AgentKind::Codex => Self::Codex,
            AgentKind::Claude => Self::Claude,
            AgentKind::Kimi => Self::Kimi,
        }
    }
}

/// How Deppy communicates with one agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentTransport {
    /// Typed JSON-RPC thread/turn/item stream from `codex app-server`.
    AppServer,
    /// Interactive CLI running inside a terminal pane.
    Pty,
}

impl AgentTransport {
    pub const fn badge(self) -> &'static str {
        match self {
            Self::AppServer => "APP",
            Self::Pty => "PTY",
        }
    }
}

/// Stable visual states used by every agent status surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentVisualState {
    Off,
    Idle,
    Active,
    Waiting,
    Complete,
    Error,
}

impl AgentVisualState {
    /// Project a PTY detector result without changing the detector's source or
    /// confidence semantics. `None` means that no agent has reported a state.
    pub const fn from_pty(status: Option<runtime::SessionStatus>) -> Self {
        use runtime::SessionStatus as Status;
        match status {
            None => Self::Off,
            Some(Status::Idle) => Self::Idle,
            Some(Status::Running) => Self::Active,
            Some(Status::Waiting | Status::NeedsApproval) => Self::Waiting,
            Some(Status::Done) => Self::Complete,
            Some(Status::Error) => Self::Error,
        }
    }

    /// 에이전트 감지 여부까지 반영한 PTY 상태 — `agent_present`(감지 워커의 ps 스캔이
    /// 그 pane에서 claude/codex를 찾았는지)가 거짓이면 그 pane에서 도는 에이전트가 없다.
    /// 이때 남은 Idle은 셸의 idle heuristic이 남긴 유령이다: 에이전트 자식 프로세스만
    /// 죽으면 셸(PTY)은 살아 있어 SessionExited가 오지 않고, status는 래치라 None으로
    /// 돌아가지 않아 "에이전트 없는 유휴" 표시가 남는다. status=None과 같은 off(회색)로
    /// 낮춘다 — 항목 자체는 계속 보여야 pane으로 들어갈 수 있고, off 표시는 의도된
    /// 동작이다(2026-07-25 판정).
    ///
    /// 진행형/결과 상태(Running/Waiting/Done/Error)는 그대로 둔다 — 감지가 한 tick
    /// 흔들렸을 때 살아있는 에이전트의 상태까지 지우지 않기 위해서다.
    ///
    /// 한계: 감지는 활성 워크스페이스에서만 돌고 warm은 마지막 감지값을 유지하므로
    /// (`WorkspaceUi::agent_line_for` 주석), warm으로 내려간 뒤 죽은 에이전트는 여기서
    /// 걸러지지 않는다.
    pub const fn from_pty_with_agent(
        status: Option<runtime::SessionStatus>,
        agent_present: bool,
    ) -> Self {
        match (agent_present, status) {
            (false, Some(runtime::SessionStatus::Idle)) => Self::Off,
            _ => Self::from_pty(status),
        }
    }

    /// Project the local structured-session lifecycle. Authoritative App Server
    /// `thread/status/changed` values are normalized into this lifecycle by the
    /// transport before reaching UI code.
    pub const fn from_structured(status: AgentSessionStatus) -> Self {
        match status {
            AgentSessionStatus::Starting | AgentSessionStatus::Running => Self::Active,
            AgentSessionStatus::Ready => Self::Idle,
            AgentSessionStatus::AwaitingApproval => Self::Waiting,
            AgentSessionStatus::Completed => Self::Complete,
            AgentSessionStatus::Failed => Self::Error,
            AgentSessionStatus::Interrupted | AgentSessionStatus::Stopped => Self::Off,
        }
    }
}

/// Actions must be gated by these capabilities before they reach a transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentCapabilities {
    pub exact_status: bool,
    pub structured_results: bool,
    pub approve: bool,
    pub reject: bool,
    pub interrupt: bool,
    pub resume: bool,
    pub model_control: bool,
    pub effort_control: bool,
    pub skill_control: bool,
    pub steer: bool,
    pub terminal_input: bool,
}

impl AgentCapabilities {
    pub const fn for_transport(transport: AgentTransport) -> Self {
        match transport {
            AgentTransport::AppServer => Self {
                exact_status: true,
                structured_results: true,
                approve: true,
                reject: true,
                interrupt: true,
                resume: true,
                model_control: true,
                effort_control: true,
                skill_control: true,
                steer: true,
                terminal_input: false,
            },
            AgentTransport::Pty => Self {
                exact_status: false,
                structured_results: false,
                approve: false,
                reject: false,
                interrupt: true,
                resume: true,
                model_control: false,
                effort_control: false,
                skill_control: false,
                steer: false,
                terminal_input: true,
            },
        }
    }
}

/// Runtime target used for focus and action routing. This is deliberately not
/// serialized: PTY runtime session IDs are process-local, while durable resume
/// metadata remains in the existing storage layer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AgentSurfaceId {
    Pty {
        workspace_id: String,
        pane_id: String,
        session_id: runtime::SessionId,
    },
    Structured {
        session_id: String,
    },
}

/// Read-only row model shared by the Agents panel and notification routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSurfaceSnapshot {
    pub id: AgentSurfaceId,
    pub provider: AgentProvider,
    pub transport: AgentTransport,
    pub title: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub context_pct: Option<u8>,
    pub state: AgentVisualState,
    /// PTY 원본 상태. `state`는 `Waiting`(프롬프트에서 사용자 입력 대기)과
    /// `NeedsApproval`(승인 질문 중)을 하나로 뭉개는데, 슬래시 명령을 보내도 되는지는
    /// 이 둘이 정반대다 — 전자는 보내기 딱 좋은 순간이고, 후자에 보내면 그 텍스트가
    /// **승인 질문의 답으로** 들어간다. 구조화 세션은 `None`이다.
    pub pty_status: Option<runtime::SessionStatus>,
}

impl AgentSurfaceSnapshot {
    pub const fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities::for_transport(self.transport)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_states_project_to_shared_visual_states() {
        use runtime::SessionStatus as Status;
        let cases = [
            (None, AgentVisualState::Off),
            (Some(Status::Idle), AgentVisualState::Idle),
            (Some(Status::Running), AgentVisualState::Active),
            (Some(Status::Waiting), AgentVisualState::Waiting),
            (Some(Status::NeedsApproval), AgentVisualState::Waiting),
            (Some(Status::Done), AgentVisualState::Complete),
            (Some(Status::Error), AgentVisualState::Error),
        ];
        for (input, expected) in cases {
            assert_eq!(AgentVisualState::from_pty(input), expected);
        }
    }

    /// 에이전트 프로세스가 죽으면 agent_line은 사라지지만(ps 스캔) status는 래치라
    /// Idle로 정착한다 — fleet 카드와 사이드바 점에 "에이전트 없는 유휴"가 남던
    /// 유령(백로그 3). 두 표면이 같은 규칙을 쓴다.
    #[test]
    fn 에이전트_없는_idle은_off로_낮춘다() {
        use runtime::SessionStatus as Status;
        let card = AgentVisualState::from_pty_with_agent;
        assert_eq!(card(Some(Status::Idle), false), AgentVisualState::Off);
        assert_eq!(card(Some(Status::Idle), true), AgentVisualState::Idle);
        // 미분류(스폰 직후)는 원래도 off — 그대로.
        assert_eq!(card(None, false), AgentVisualState::Off);
        // 진행형/결과 상태는 감지가 흔들려도 유지한다.
        for status in [
            Status::Running,
            Status::Waiting,
            Status::NeedsApproval,
            Status::Done,
            Status::Error,
        ] {
            assert_eq!(
                card(Some(status), false),
                AgentVisualState::from_pty(Some(status))
            );
        }
    }

    #[test]
    fn structured_states_project_to_shared_visual_states() {
        use AgentSessionStatus as Status;
        let cases = [
            (Status::Starting, AgentVisualState::Active),
            (Status::Ready, AgentVisualState::Idle),
            (Status::Running, AgentVisualState::Active),
            (Status::AwaitingApproval, AgentVisualState::Waiting),
            (Status::Completed, AgentVisualState::Complete),
            (Status::Failed, AgentVisualState::Error),
            (Status::Interrupted, AgentVisualState::Off),
            (Status::Stopped, AgentVisualState::Off),
        ];
        for (input, expected) in cases {
            assert_eq!(AgentVisualState::from_structured(input), expected);
        }
    }

    #[test]
    fn pty_never_exposes_structured_approval_actions() {
        let pty = AgentCapabilities::for_transport(AgentTransport::Pty);
        assert!(!pty.exact_status);
        assert!(!pty.approve);
        assert!(!pty.reject);
        assert!(pty.interrupt);
        assert!(pty.terminal_input);

        let app = AgentCapabilities::for_transport(AgentTransport::AppServer);
        assert!(app.exact_status);
        assert!(app.approve);
        assert!(app.reject);
        assert!(!app.terminal_input);
    }
}
