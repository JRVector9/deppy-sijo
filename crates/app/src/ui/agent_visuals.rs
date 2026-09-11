//! Shared colors for every agent status surface.
//!
//! Lifecycle projection stays in `agent_surface`; this module owns only the
//! egui palette so PTY rows and structured sessions cannot drift apart.

use crate::agent_surface::AgentVisualState;

pub(crate) const fn status_color(state: AgentVisualState) -> egui::Color32 {
    match state {
        AgentVisualState::Off => egui::Color32::from_rgb(0x8b, 0x94, 0x9e),
        // Idle은 원래 #f2f4f7(L 96%)로 6개 중 **가장 밝았다** — 아무 일도 안 하는 세션의
        // 레일이 화면에서 가장 강한 신호가 돼, 세션이 많을수록 유휴 레일이 화면을
        // 지배했다(2026-08-07 사용자). Off(L 58%)보다는 밝아 "살아있음"이 보이되
        // Active(L 67%) 아래로 내려 신호를 가리지 않게 한다.
        AgentVisualState::Idle => egui::Color32::from_rgb(0xa1, 0xa8, 0xb0),
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
    fn agent_palette_matches_the_workspace_contract() {
        let cases = [
            (
                AgentVisualState::Off,
                egui::Color32::from_rgb(0x8b, 0x94, 0x9e),
            ),
            (
                AgentVisualState::Idle,
                egui::Color32::from_rgb(0xa1, 0xa8, 0xb0),
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
