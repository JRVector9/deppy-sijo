//! Shared layout primitives for form and confirmation popups.
//!
//! Each caller keeps its own draft state and actions. This module owns only
//! presentation: the shell, fields, notices, and action footer.

mod confirmation;
mod information;
mod input;

pub use confirmation::{
    ConfirmationChoice, ConfirmationSpec, confirmation, confirmation_for_target,
};
pub use connector_ui::popup::{
    ActionTone, NoticeTone, PopupSpec, WindowSpec, action_button, body, body_with_max_height,
    choice_input, field, footer, list_actions, list_row, notice, path_input, popover,
    popover_frame, segmented_choice, show, text_input, window, window_body,
};
pub(crate) use connector_ui::popup::{modal_input_blocked, prepare_target, set_pending_modal};
pub use information::{InformationSpec, information};
pub(crate) use input::background_input_blocked;

/// A menu behind a modal must not swallow its Esc; a different front modal owns it.
pub(crate) fn take_modal_escape(ctx: &egui::Context, id: egui::Id) -> bool {
    let top = ctx.memory(|memory| memory.top_modal_layer())
        == Some(egui::LayerId::new(egui::Order::Foreground, id));
    top && ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
}

/// Only the front floating window owns Esc. A modal or an open menu owns it first.
pub fn take_window_escape(ctx: &egui::Context, id: egui::Id) -> bool {
    let owns_escape = !ctx.any_popup_open()
        && !modal_input_blocked(ctx)
        && ctx.memory(|memory| {
            memory.areas().top_layer_id(egui::Order::Middle)
                == Some(egui::LayerId::new(egui::Order::Middle, id))
        });
    owns_escape
        && ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
}

pub(super) use connector_ui::popup::palette;
