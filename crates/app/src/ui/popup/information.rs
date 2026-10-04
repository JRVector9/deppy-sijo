use super::{ActionTone, NoticeTone, PopupSpec, action_button, body, footer, notice, show};

pub struct InformationSpec<'a> {
    pub id: egui::Id,
    pub title: &'a str,
    pub message: &'a str,
    pub accept_label: &'a str,
    pub close_label: &'a str,
}

/// Returns dismissal only. The caller owns the notice and its lifecycle.
pub fn information(ctx: &egui::Context, spec: InformationSpec<'_>) -> bool {
    let mut accepted = false;
    let closed = show(
        ctx,
        PopupSpec {
            id: spec.id,
            width: 420.0,
            title: spec.title,
            subtitle: "",
            close_label: spec.close_label,
            close_enabled: true,
        },
        |ui| {
            body(ui, |ui| notice(ui, spec.message, NoticeTone::Info));
            footer(ui, None, |ui| {
                accepted =
                    action_button(ui, spec.accept_label, ActionTone::Primary, true).clicked();
            });
        },
    );
    closed || super::take_modal_escape(ctx, spec.id) || accepted
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;

    #[test]
    fn env_ports_popup_information_wraps_and_keeps_close_inside_narrow_viewport() {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(280.0, 340.0))
            .build_ui_state(
                |ui, closed| {
                    if !*closed {
                        *closed = information(ui.ctx(), InformationSpec {
                            id: egui::Id::new("notice-fixture"),
                            title: "Workspace capacity reached",
                            message: "The workspace could not be opened. Close an existing running session and then try again. This notice does not stop any process.",
                            accept_label: "Close",
                            close_label: "Dismiss dialog",
                        });
                    }
                },
                false,
            );
        harness.run();
        let close = harness.get_by_label("Close").rect();
        assert!((close.height() - 34.0).abs() < 0.5, "{close:?}");
        let rect = harness
            .ctx
            .memory(|m| m.area_rect(egui::Id::new("notice-fixture")))
            .unwrap();
        assert!(
            rect.left() >= 0.0
                && rect.right() <= 280.0
                && rect.top() >= 0.0
                && rect.bottom() <= 340.0,
            "{rect:?}"
        );
        harness.key_press(egui::Key::Escape);
        harness.run();
        assert!(*harness.state());
    }
}
