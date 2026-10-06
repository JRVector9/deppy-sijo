//! Shared presentation-only popup components used by App and Connector forms.
mod actions;
mod fields;
mod list;
mod notice;
mod shell;
mod window;

pub use actions::{ActionTone, action_button, footer};
pub use fields::{choice_input, field, path_input, segmented_choice, text_input};
pub use list::{list_actions, list_row};
pub use notice::{NoticeTone, notice};
pub use shell::{
    PopupSpec, body, body_with_max_height, popover, popover_frame, prepare_target, show,
};
pub use window::{WindowSpec, fixed_window, window, window_body};

#[derive(Clone, Copy)]
struct ModalFence {
    pass: u64,
}
fn fence_id(ctx: &egui::Context) -> egui::Id {
    egui::Id::new(("popup_input_fence", ctx.viewport_id()))
}
#[doc(hidden)]
pub fn set_pending_modal(ctx: &egui::Context, pending: bool) {
    if pending {
        let pass = ctx.cumulative_pass_nr();
        let id = fence_id(ctx);
        ctx.data_mut(|data| data.insert_temp(id, ModalFence { pass }));
    }
}
#[doc(hidden)]
pub fn modal_input_blocked(ctx: &egui::Context) -> bool {
    // Context accessors share one RwLock. Never reacquire it inside data():
    // a queued background repaint writer would strand this outer read guard.
    let id = fence_id(ctx);
    let pass = ctx.cumulative_pass_nr();
    ctx.data(|data| {
        data.get_temp::<ModalFence>(id)
            .is_some_and(|f| f.pass == pass)
    }) || ctx.memory(|m| m.top_modal_layer().is_some())
}
#[derive(Clone, Copy)]
#[doc(hidden)]
pub struct Palette {
    pub surface: egui::Color32,
    footer: egui::Color32,
    pub line: egui::Color32,
    pub border: egui::Color32,
    pub text: egui::Color32,
    pub muted: egui::Color32,
    accent: egui::Color32,
    accent_low: egui::Color32,
    button: egui::Color32,
    pub input: egui::Color32,
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

#[doc(hidden)]
pub fn palette(ui: &egui::Ui) -> Palette {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popup_fence_polling_does_not_deadlock_background_repaint() {
        const CHILD_ENV: &str = "DEPPY_POPUP_REPAINT_CHILD";
        if std::env::var_os(CHILD_ENV).is_some() {
            use std::sync::{Arc, Barrier};
            let ctx = egui::Context::default();
            ctx.begin_pass(egui::RawInput::default());
            set_pending_modal(&ctx, true);
            let barrier = Arc::new(Barrier::new(3));
            let writers = (0..2)
                .map(|_| {
                    let ctx = ctx.clone();
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        for _ in 0..30_000 {
                            ctx.request_repaint();
                        }
                    })
                })
                .collect::<Vec<_>>();
            barrier.wait();
            for _ in 0..30_000 {
                assert!(modal_input_blocked(&ctx));
            }
            for writer in writers {
                writer.join().unwrap();
            }
            ctx.end_pass().drop_without_applying_deltas();
            return;
        }
        // Bound a real lock regression in an owned subprocess; a broken Context must not
        // strand the test runner or leave blocked background threads alive after failure.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "popup::tests::popup_fence_polling_does_not_deadlock_background_repaint",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "popup/repaint concurrency child failed: {status}"
                );
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("popup fence reader and background repaint stalled on the Context lock");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn popup_pending_fence_expires_after_its_pass() {
        let ctx = egui::Context::default();
        ctx.begin_pass(egui::RawInput::default());
        assert!(!modal_input_blocked(&ctx));
        set_pending_modal(&ctx, true);
        assert!(modal_input_blocked(&ctx));
        set_pending_modal(&ctx, false);
        assert!(modal_input_blocked(&ctx));
        ctx.end_pass().drop_without_applying_deltas();
        ctx.begin_pass(egui::RawInput::default());
        assert!(!modal_input_blocked(&ctx));
        ctx.end_pass().drop_without_applying_deltas();
    }
}
