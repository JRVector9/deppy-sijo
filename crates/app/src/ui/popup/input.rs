//! Input ownership for pending and actually rendered modals. Ordinary
//! Foreground areas (search, tooltips, closed context menus) are not modals.

#[derive(Clone, Copy)]
struct ModalFence {
    pass: u64,
}

fn fence_id(ctx: &egui::Context) -> egui::Id {
    egui::Id::new(("popup_input_fence", ctx.viewport_id()))
}

/// Publish before background keyboard processing. A fence lasts through this
/// entire pass, including the pass in which a popup closes.
pub(crate) fn set_pending_modal(ctx: &egui::Context, pending: bool) {
    if pending {
        let id = fence_id(ctx);
        let pass = ctx.cumulative_pass_nr();
        ctx.data_mut(|data| data.insert_temp(id, ModalFence { pass }));
    }
}

pub(crate) fn modal_input_blocked(ctx: &egui::Context) -> bool {
    let id = fence_id(ctx);
    let pass = ctx.cumulative_pass_nr();
    ctx.data(|data| {
        data.get_temp::<ModalFence>(id)
            .is_some_and(|fence| fence.pass == pass)
    }) || ctx.memory(|memory| memory.top_modal_layer().is_some())
}

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
