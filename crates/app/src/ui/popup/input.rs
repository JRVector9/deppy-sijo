//! Input ownership for pending and actually rendered modals. Ordinary
//! Foreground areas (search, tooltips, closed context menus) are not modals.

use connector_ui::popup::modal_input_blocked;
#[cfg(test)]
use connector_ui::popup::set_pending_modal;

pub(crate) fn background_input_blocked(ctx: &egui::Context) -> bool {
    modal_input_blocked(ctx)
        || ctx.any_popup_open()
        || ctx.memory(|memory| {
            memory
                .areas()
                .visible_layer_ids()
                .iter()
                .any(crate::ui::workspace::is_blocking_terminal_window)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popup_review_pending_fence_blocks_shortcut_then_expires_next_pass() {
        let ctx = egui::Context::default();
        let config = crate::config::ShortcutsConfig::default();
        let binding = crate::shortcuts::effective_binding(
            &config,
            crate::shortcuts::ShortcutAction::ClosePane,
        )
        .unwrap();
        ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: binding.logical_key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: binding.modifiers,
                }],
                ..Default::default()
            },
            |ui| {
                set_pending_modal(ui.ctx(), true);
                assert!(background_input_blocked(ui.ctx()));
                assert!(crate::shortcuts::take_triggered_action(ui.ctx(), &config).is_none());
                set_pending_modal(ui.ctx(), false);
                assert!(
                    background_input_blocked(ui.ctx()),
                    "closing must not release the same key batch to the background"
                );
            },
        )
        .drop_without_applying_deltas();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            assert!(!background_input_blocked(ui.ctx()))
        })
        .drop_without_applying_deltas();
    }
}
