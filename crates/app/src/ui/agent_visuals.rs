//! Shared colors for every agent status surface.
//!
//! Lifecycle projection stays in `agent_surface`; this module owns only the
//! egui palette so PTY rows and structured sessions cannot drift apart.

use crate::agent_surface::AgentVisualState;

pub(crate) const fn status_color(state: AgentVisualState) -> egui::Color32 {
    match state {
        AgentVisualState::Off => egui::Color32::from_rgb(0x8b, 0x94, 0x9e),
        AgentVisualState::Idle => egui::Color32::from_rgb(0xf2, 0xf4, 0xf7),
        AgentVisualState::Active => egui::Color32::from_rgb(0x58, 0xa6, 0xff),
        AgentVisualState::Waiting => egui::Color32::from_rgb(0xff, 0xbf, 0x69),
        AgentVisualState::Complete => egui::Color32::from_rgb(0x56, 0xd3, 0x64),
        AgentVisualState::Error => egui::Color32::from_rgb(0xff, 0x7b, 0x72),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_palette_matches_the_workspace_contract() {
        let cases = [
            (
                AgentVisualState::Off,
                egui::Color32::from_rgb(0x8b, 0x94, 0x9e),
            ),
            (
                AgentVisualState::Idle,
                egui::Color32::from_rgb(0xf2, 0xf4, 0xf7),
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
