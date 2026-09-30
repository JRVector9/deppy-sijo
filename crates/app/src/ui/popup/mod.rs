//! Shared layout primitives for form and confirmation popups.
//!
//! Each caller keeps its own draft state and actions. This module owns only
//! presentation: the shell, fields, notices, and action footer.

mod actions;
mod fields;
mod notice;
mod shell;

pub use actions::{ActionTone, action_button, footer};
pub use fields::{field, segmented_choice};
pub use notice::{NoticeTone, notice};
pub use shell::{PopupSpec, body, show};

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
    error: egui::Color32,
    error_low: egui::Color32,
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
            error: egui::Color32::from_rgb(0xff, 0x73, 0x73),
            error_low: egui::Color32::from_rgb(0x3c, 0x25, 0x29),
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
            error: egui::Color32::from_rgb(0xc8, 0x3d, 0x49),
            error_low: egui::Color32::from_rgb(0xfa, 0xe0, 0xe3),
        }
    }
}
