//! Session/workspace termination presentation; callers own targets and runtime actions.
use super::popup::{self, ConfirmationChoice, ConfirmationSpec};

pub(crate) fn session(
    ctx: &egui::Context,
    pane_id: &str,
    title: &str,
    catalog: &i18n::Catalog,
) -> Option<ConfirmationChoice> {
    let case = egui::Id::new("session_close_confirmation");
    popup::confirmation_for_target(
        ctx,
        case.with(pane_id),
        ConfirmationSpec {
            id: case,
            title: &catalog.t("workspace.close_confirm.title", &[]),
            subtitle: title,
            target: None,
            message: &catalog.t("workspace.close_confirm.body", &[]),
            confirm_label: &catalog.t("action.close", &[]),
            cancel_label: &catalog.t("action.cancel", &[]),
            close_label: &catalog.t("popup.dismiss", &[]),
        },
    )
}

pub(crate) fn workspace(
    ctx: &egui::Context,
    workspace_id: &str,
    name: &str,
    total: usize,
    running: usize,
    catalog: &i18n::Catalog,
) -> Option<ConfirmationChoice> {
    let case = egui::Id::new("workspace_close_confirmation");
    popup::confirmation_for_target(
        ctx,
        case.with(workspace_id),
        ConfirmationSpec {
            id: case,
            title: &catalog.t("workspace.close_ws_confirm.title", &[]),
            subtitle: name,
            target: None,
            message: &catalog.t(
                "workspace.close_ws_confirm.body",
                &[
                    ("name", name),
                    ("count", &total.to_string()),
                    ("running", &running.to_string()),
                ],
            ),
            confirm_label: &catalog.t("workspace.close_ws_confirm.confirm", &[]),
            cancel_label: &catalog.t("action.cancel", &[]),
            close_label: &catalog.t("popup.dismiss", &[]),
        },
    )
}
