//! Shared layout primitives for form and confirmation popups.
//!
//! Each caller keeps its own draft state and actions. This module owns only
//! presentation: the shell, fields, notices, and action footer.

mod actions;
mod confirmation;
mod fields;
mod information;
mod input;
mod list;
mod notice;
mod shell;

pub use actions::{ActionTone, action_button, footer};
pub use confirmation::{
    ConfirmationChoice, ConfirmationSpec, confirmation, confirmation_for_target,
};
pub use fields::{choice_input, field, path_input, segmented_choice, text_input};
pub use information::{InformationSpec, information};
pub(crate) use input::{background_input_blocked, modal_input_blocked, set_pending_modal};
pub use list::{list_actions, list_row};
pub use notice::{NoticeTone, notice};
pub(crate) use shell::prepare_target;
pub use shell::{PopupSpec, body, body_with_max_height, popover, popover_frame, show};

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

#[derive(Clone, Copy)]
struct Palette {
    surface: egui::Color32,
    footer: egui::Color32,
    line: egui::Color32,
    border: egui::Color32,
    text: egui::Color32,
    muted: egui::Color32,
    accent: egui::Color32,
    accent_low: egui::Color32,
    button: egui::Color32,
    input: egui::Color32,
    hover: egui::Color32,
    accent_hover: egui::Color32,
    primary_text: egui::Color32,
    info_line: egui::Color32,
    info_text: egui::Color32,
    error_low: egui::Color32,
    error_line: egui::Color32,
    error_text: egui::Color32,
    danger: egui::Color32,
    danger_hover: egui::Color32,
    danger_text: egui::Color32,
}

const INPUT_HEIGHT: f32 = 36.0;
const BUTTON_HEIGHT: f32 = 34.0;
const BUTTON_PADDING: egui::Vec2 = egui::vec2(13.0, 6.0);
const ACTION_GAP: f32 = 8.0;
const FIELD_GAP: f32 = 16.0;

fn divider(ui: &mut egui::Ui) {
    let colors = palette(ui);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().hline(
        rect.x_range(),
        rect.center().y,
        egui::Stroke::new(1.0, colors.line),
    );
}

fn palette(ui: &egui::Ui) -> Palette {
    palette_for(ui.visuals().dark_mode)
}

fn palette_for(dark_mode: bool) -> Palette {
    if dark_mode {
        Palette {
            surface: egui::Color32::from_rgb(0x18, 0x1b, 0x20),
            footer: egui::Color32::from_rgb(0x1b, 0x1e, 0x24),
            line: egui::Color32::from_rgb(0x32, 0x36, 0x3e),
            border: egui::Color32::from_rgb(0x4a, 0x50, 0x5b),
            text: egui::Color32::from_rgb(0xd5, 0xd8, 0xdd),
            muted: egui::Color32::from_rgb(0x8a, 0x8f, 0x99),
            accent: egui::Color32::from_rgb(0x39, 0xb7, 0xd0),
            accent_low: egui::Color32::from_rgb(0x15, 0x34, 0x40),
            button: egui::Color32::from_rgb(0x21, 0x24, 0x2c),
            input: egui::Color32::from_rgb(0x0f, 0x11, 0x15),
            hover: egui::Color32::from_rgb(0x29, 0x2e, 0x38),
            accent_hover: egui::Color32::from_rgb(0x62, 0xc9, 0xdd),
            primary_text: egui::Color32::from_rgb(0x0d, 0x1c, 0x21),
            info_line: egui::Color32::from_rgb(0x31, 0x56, 0x66),
            info_text: egui::Color32::from_rgb(0xbc, 0xe7, 0xee),
            error_low: egui::Color32::from_rgb(0x3c, 0x25, 0x29),
            error_line: egui::Color32::from_rgb(0x79, 0x46, 0x4b),
            error_text: egui::Color32::from_rgb(0xff, 0xc8, 0xc8),
            danger: egui::Color32::from_rgb(0xff, 0x73, 0x73),
            danger_hover: egui::Color32::from_rgb(0xff, 0x99, 0x99),
            danger_text: egui::Color32::from_rgb(0x24, 0x12, 0x15),
        }
    } else {
        Palette {
            surface: egui::Color32::from_rgb(0xf1, 0xf2, 0xf5),
            footer: egui::Color32::from_rgb(0xe9, 0xeb, 0xef),
            line: egui::Color32::from_rgb(0xc9, 0xcd, 0xd6),
            border: egui::Color32::from_rgb(0xb8, 0xbe, 0xc8),
            text: egui::Color32::from_rgb(0x23, 0x26, 0x2c),
            muted: egui::Color32::from_rgb(0x65, 0x6a, 0x74),
            accent: egui::Color32::from_rgb(0x1c, 0x93, 0xaa),
            accent_low: egui::Color32::from_rgb(0xd5, 0xf0, 0xf3),
            button: egui::Color32::from_rgb(0xe5, 0xe8, 0xec),
            input: egui::Color32::WHITE,
            hover: egui::Color32::from_rgb(0xda, 0xdf, 0xe7),
            accent_hover: egui::Color32::from_rgb(0x27, 0xa7, 0xbf),
            primary_text: egui::Color32::from_rgb(0x0d, 0x1c, 0x21),
            info_line: egui::Color32::from_rgb(0x8e, 0xc5, 0xd1),
            info_text: egui::Color32::from_rgb(0x22, 0x5b, 0x68),
            error_low: egui::Color32::from_rgb(0xfa, 0xe0, 0xe3),
            error_line: egui::Color32::from_rgb(0xda, 0xa3, 0xaa),
            error_text: egui::Color32::from_rgb(0x8e, 0x26, 0x33),
            danger: egui::Color32::from_rgb(0xb7, 0x34, 0x42),
            danger_hover: egui::Color32::from_rgb(0xcd, 0x46, 0x53),
            danger_text: egui::Color32::WHITE,
        }
    }
}
