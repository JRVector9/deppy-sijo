//! Shared colors for every agent status surface.
//!
//! Lifecycle projection stays in `agent_surface`; this module owns only the
//! egui palette so PTY rows and structured sessions cannot drift apart.

use crate::agent_surface::AgentVisualState;

/// 다음 지시를 기다리는 문구는 점/레일보다 선명하게 표시한다.
pub(crate) const fn status_text_color(state: AgentVisualState) -> egui::Color32 {
    match state {
        AgentVisualState::Idle => egui::Color32::from_rgb(0x70, 0xd7, 0xc7),
        _ => status_color(state),
    }
}

pub(crate) const fn status_color(state: AgentVisualState) -> egui::Color32 {
    match state {
        AgentVisualState::Off => egui::Color32::from_rgb(0x8b, 0x94, 0x9e),
        // 지시 대기는 살아 있는 세션이다. Off와 비슷한 회색이면 끝난 작업으로
        // 오인되므로, 작업 중 파랑보다 절제된 청록색으로 구분한다.
        AgentVisualState::Idle => egui::Color32::from_rgb(0x54, 0xb3, 0xa8),
        AgentVisualState::Active => egui::Color32::from_rgb(0x58, 0xa6, 0xff),
        AgentVisualState::Waiting | AgentVisualState::NeedsResponse => {
            egui::Color32::from_rgb(0xff, 0xbf, 0x69)
        }
        AgentVisualState::Complete => egui::Color32::from_rgb(0x56, 0xd3, 0x64),
        AgentVisualState::Error => egui::Color32::from_rgb(0xff, 0x7b, 0x72),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn awaiting_instruction_uses_a_live_teal_rail_distinct_from_off() {
        let idle = status_color(AgentVisualState::Idle);
        let off = status_color(AgentVisualState::Off);
        assert_ne!(idle, off);
        assert!(idle.g() > idle.r() && idle.g() > idle.b());
    }

    #[test]
    fn agent_palette_matches_the_workspace_contract() {
        let cases = [
            (
                AgentVisualState::Off,
                egui::Color32::from_rgb(0x8b, 0x94, 0x9e),
            ),
            (
                AgentVisualState::Idle,
                egui::Color32::from_rgb(0x54, 0xb3, 0xa8),
            ),
            (
                AgentVisualState::Active,
                egui::Color32::from_rgb(0x58, 0xa6, 0xff),
            ),
            (
                AgentVisualState::Waiting,
                egui::Color32::from_rgb(0xff, 0xbf, 0x69),
            ),
            (
                AgentVisualState::Complete,
                egui::Color32::from_rgb(0x56, 0xd3, 0x64),
            ),
            (
                AgentVisualState::Error,
                egui::Color32::from_rgb(0xff, 0x7b, 0x72),
            ),
        ];

        for (state, expected) in cases {
            assert_eq!(status_color(state), expected);
        }
    }
}
