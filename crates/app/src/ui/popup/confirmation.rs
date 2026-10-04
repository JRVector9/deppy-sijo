use super::{
    ActionTone, NoticeTone, PopupSpec, action_button, body, footer, notice, palette, show,
};

pub struct ConfirmationSpec<'a> {
    pub id: egui::Id,
    pub title: &'a str,
    pub subtitle: &'a str,
    pub target: Option<&'a str>,
    pub message: &'a str,
    pub confirm_label: &'a str,
    pub cancel_label: &'a str,
    pub close_label: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmationChoice {
    Confirm,
    Cancel,
}

/// Presentation only. The caller owns the captured target and accepts the action.
pub fn confirmation(ctx: &egui::Context, spec: ConfirmationSpec<'_>) -> Option<ConfirmationChoice> {
    confirmation_for_target(ctx, spec.id, spec)
}

/// Stable modal case ID, but target-scoped action IDs and no inherited focus.
pub fn confirmation_for_target(
    ctx: &egui::Context,
    target: egui::Id,
    spec: ConfirmationSpec<'_>,
) -> Option<ConfirmationChoice> {
    super::prepare_target(ctx, spec.id, target);
    let mut choice = None;
    let close = show(
        ctx,
        PopupSpec {
            id: spec.id,
            width: 400.0,
            title: spec.title,
            subtitle: spec.subtitle,
            close_label: spec.close_label,
            close_enabled: true,
        },
        |ui| {
            body(ui, |ui| {
                if let Some(target) = spec.target {
                    let colors = palette(ui);
                    let width = (ui.available_width() - 24.0).max(1.0);
                    egui::Frame::NONE
                        .fill(colors.input)
                        .stroke(egui::Stroke::new(1.0, colors.line))
                        .corner_radius(3)
                        .inner_margin(egui::Margin::symmetric(11, 10))
                        .show(ui, |ui| {
                            ui.set_width(width);
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(target).size(11.0).color(colors.muted),
                                )
                                .wrap(),
                            );
                        });
                }
                notice(ui, spec.message, NoticeTone::Error);
            });
            footer(ui, None, |ui| {
                ui.push_id(target, |ui| {
                    if action_button(ui, spec.confirm_label, ActionTone::Danger, true).clicked() {
                        choice = Some(ConfirmationChoice::Confirm);
                    }
                    if action_button(ui, spec.cancel_label, ActionTone::Ghost, true).clicked() {
                        choice = Some(ConfirmationChoice::Cancel);
                    }
                });
            });
        },
    );
    // A source popover behind this modal can still be registered as open. It must
    // not swallow Esc, and a different modal on top must retain its own keyboard.
    let escape = super::take_modal_escape(ctx, spec.id);
    if close || escape {
        Some(ConfirmationChoice::Cancel)
    } else {
        choice
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;

    #[test]
    #[ignore = "offscreen popup PNGs for manual visual review"]
    fn popup_parity_render_workspace_close() {
        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 650.0))
            .build_ui(|ui| {
                assert!(
                    crate::ui::session_close_dialogs::workspace(
                        ui.ctx(),
                        "fixture-workspace",
                        "Serenity",
                        3,
                        2,
                        &catalog,
                    )
                    .is_none()
                );
            });
        crate::fonts::install_cjk_fallback(&harness.ctx, None, "JetBrainsMono", "Regular");
        harness.ctx.set_visuals(egui::Visuals::dark());
        harness.run();
        let output =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/popup-parity");
        std::fs::create_dir_all(&output).unwrap();
        harness
            .render()
            .unwrap()
            .save(output.join("08-workspace.png"))
            .unwrap();
    }

    #[test]
    fn confirmation_requires_explicit_action_and_escape_cancels() {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 650.0))
            .build_ui_state(
                |ui, choice| {
                    if choice.is_none() {
                        *choice = confirmation(
                            ui.ctx(),
                            ConfirmationSpec {
                                id: egui::Id::new("test_confirmation"),
                                title: "End session",
                                subtitle: "Workspace / Agent",
                                target: None,
                                message: "The running session will end.",
                                confirm_label: "End",
                                cancel_label: "Cancel",
                                close_label: "Close dialog",
                            },
                        );
                    }
                },
                None,
            );
        harness.run();
        let button = harness.get_by_label("End").rect();
        assert!((button.height() - 34.0).abs() < 0.5);
        harness.key_press(egui::Key::Enter);
        harness.run();
        assert_eq!(*harness.state(), None);
        harness.key_press(egui::Key::Escape);
        harness.run();
        assert_eq!(*harness.state(), Some(ConfirmationChoice::Cancel));
    }

    #[test]
    fn close_popup_new_workspace_target_does_not_inherit_close_focus() {
        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        let close_label = catalog.t("workspace.close_ws_confirm.confirm", &[]);
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut (String, Option<ConfirmationChoice>)| {
                if state.1.is_some() {
                    return;
                }
                state.1 = crate::ui::session_close_dialogs::workspace(
                    ui.ctx(),
                    &state.0,
                    "Same workspace name",
                    3,
                    2,
                    &catalog,
                );
            },
            ("first-workspace".to_owned(), None),
        );
        harness.run();
        harness.get_by_label(&close_label).focus();
        harness.run();
        harness.state_mut().0 = "second-workspace".to_owned();
        harness.key_press(egui::Key::Enter);
        harness.run();
        assert_eq!(harness.state().1, None);
        harness.get_by_label(&close_label).click();
        harness.run();
        assert_eq!(harness.state().1, Some(ConfirmationChoice::Confirm));
    }
}
