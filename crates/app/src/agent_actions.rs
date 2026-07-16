//! Provider-neutral agent action policy.
//!
//! This module only decides whether an action is safe for the selected surface.
//! Transport-specific execution remains in `App`/the structured session controller.

use crate::agent_surface::AgentSurfaceSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAction {
    OpenAgents,
    SelectPrevious,
    SelectNext,
    FocusInput,
    NewStructured,
    Interrupt,
    ApproveOnce,
    Reject,
    EffortUp,
    EffortDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentActionGate {
    Allowed,
    NoTarget,
    Unsupported,
    ApprovalCountMismatch { pending: usize },
}

impl AgentActionGate {
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Allowed)
    }
}

/// Gate an action before any transport command is created.
///
/// Approval shortcuts intentionally require exactly one request. `ApproveOnce`
/// never represents a session-wide approval and PTY surfaces can never pass the
/// structured approval/rejection capability checks.
pub fn gate_action(
    action: AgentAction,
    selected: Option<&AgentSurfaceSnapshot>,
    pending_approval_count: usize,
) -> AgentActionGate {
    if matches!(
        action,
        AgentAction::OpenAgents
            | AgentAction::SelectPrevious
            | AgentAction::SelectNext
            | AgentAction::NewStructured
    ) {
        return AgentActionGate::Allowed;
    }

    let Some(selected) = selected else {
        return AgentActionGate::NoTarget;
    };
    let capabilities = selected.capabilities();

    match action {
        AgentAction::FocusInput => AgentActionGate::Allowed,
        AgentAction::Interrupt if capabilities.interrupt => AgentActionGate::Allowed,
        AgentAction::ApproveOnce if !capabilities.approve => AgentActionGate::Unsupported,
        AgentAction::Reject if !capabilities.reject => AgentActionGate::Unsupported,
        AgentAction::ApproveOnce | AgentAction::Reject if pending_approval_count == 1 => {
            AgentActionGate::Allowed
        }
        AgentAction::ApproveOnce | AgentAction::Reject => AgentActionGate::ApprovalCountMismatch {
            pending: pending_approval_count,
        },
        AgentAction::EffortUp | AgentAction::EffortDown if capabilities.effort_control => {
            AgentActionGate::Allowed
        }
        AgentAction::Interrupt | AgentAction::EffortUp | AgentAction::EffortDown => {
            AgentActionGate::Unsupported
        }
        AgentAction::OpenAgents
        | AgentAction::SelectPrevious
        | AgentAction::SelectNext
        | AgentAction::NewStructured => AgentActionGate::Allowed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_surface::{AgentProvider, AgentSurfaceId, AgentTransport, AgentVisualState};

    fn snapshot(transport: AgentTransport) -> AgentSurfaceSnapshot {
        AgentSurfaceSnapshot {
            id: match transport {
                AgentTransport::AppServer => AgentSurfaceId::Structured {
                    session_id: "structured-1".to_owned(),
                },
                AgentTransport::Pty => AgentSurfaceId::Pty {
                    workspace_id: "workspace-1".to_owned(),
                    pane_id: "pane-1".to_owned(),
                    session_id: runtime::SessionId(1),
                },
            },
            provider: AgentProvider::Codex,
            transport,
            title: "agent".to_owned(),
            model: None,
            effort: None,
            context_pct: None,
            state: AgentVisualState::Idle,
        }
    }

    #[test]
    fn navigation_and_new_structured_do_not_require_a_target() {
        for action in [
            AgentAction::OpenAgents,
            AgentAction::SelectPrevious,
            AgentAction::SelectNext,
            AgentAction::NewStructured,
        ] {
            assert_eq!(gate_action(action, None, 0), AgentActionGate::Allowed);
        }
    }

    #[test]
    fn target_actions_report_a_missing_selection() {
        for action in [
            AgentAction::FocusInput,
            AgentAction::Interrupt,
            AgentAction::ApproveOnce,
            AgentAction::Reject,
            AgentAction::EffortUp,
            AgentAction::EffortDown,
        ] {
            assert_eq!(gate_action(action, None, 0), AgentActionGate::NoTarget);
        }
    }

    #[test]
    fn pty_can_focus_and_interrupt_but_never_approve_or_reject() {
        let pty = snapshot(AgentTransport::Pty);
        assert!(gate_action(AgentAction::FocusInput, Some(&pty), 0).is_allowed());
        assert!(gate_action(AgentAction::Interrupt, Some(&pty), 0).is_allowed());
        assert_eq!(
            gate_action(AgentAction::ApproveOnce, Some(&pty), 1),
            AgentActionGate::Unsupported
        );
        assert_eq!(
            gate_action(AgentAction::Reject, Some(&pty), 1),
            AgentActionGate::Unsupported
        );
        assert_eq!(
            gate_action(AgentAction::EffortUp, Some(&pty), 0),
            AgentActionGate::Unsupported
        );
    }

    #[test]
    fn structured_approval_requires_exactly_one_pending_request() {
        let app = snapshot(AgentTransport::AppServer);
        for action in [AgentAction::ApproveOnce, AgentAction::Reject] {
            assert_eq!(
                gate_action(action, Some(&app), 0),
                AgentActionGate::ApprovalCountMismatch { pending: 0 }
            );
            assert_eq!(gate_action(action, Some(&app), 1), AgentActionGate::Allowed);
            assert_eq!(
                gate_action(action, Some(&app), 2),
                AgentActionGate::ApprovalCountMismatch { pending: 2 }
            );
        }
    }

    #[test]
    fn structured_surface_supports_interrupt_and_effort_controls() {
        let app = snapshot(AgentTransport::AppServer);
        for action in [
            AgentAction::Interrupt,
            AgentAction::EffortUp,
            AgentAction::EffortDown,
        ] {
            assert!(gate_action(action, Some(&app), 0).is_allowed());
        }
    }
}
