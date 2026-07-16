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
}

impl AgentProvider {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
        }
    }
}

impl From<AgentKind> for AgentProvider {
    fn from(value: AgentKind) -> Self {
        match value {
            AgentKind::Codex => Self::Codex,
            AgentKind::Claude => Self::Claude,
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
