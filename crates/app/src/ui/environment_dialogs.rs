//! Environment dialog presentation. Callers own targets, drafts and operations.
use super::popup::{self, ActionTone, NoticeTone, PopupSpec};

struct ChoiceSpec<'a> {
    popup: PopupSpec<'a>,
    target: egui::Id,
    accept: &'a str,
    tone: ActionTone,
}

fn choice(
    ctx: &egui::Context,
    spec: ChoiceSpec<'_>,
    catalog: &i18n::Catalog,
    contents: impl FnOnce(&mut egui::Ui),
) -> Option<bool> {
    let id = spec.popup.id;
    let actions = popup::prepare_target(ctx, id, spec.target);
    let mut decision = None;
    let closed = popup::show(ctx, spec.popup, |ui| {
        popup::body(ui, contents);
        popup::footer(ui, None, |ui| {
            ui.push_id(actions, |ui| {
                if popup::action_button(ui, spec.accept, spec.tone, true).clicked() {
                    decision = Some(true);
                }
                if popup::action_button(
                    ui,
                    &catalog.t("action.cancel", &[]),
                    ActionTone::Ghost,
                    true,
                )
                .clicked()
                {
                    decision = Some(false);
                }
            });
        });
    });
    if closed || popup::take_modal_escape(ctx, id) {
        Some(false)
    } else {
        decision
    }
}

pub(crate) fn project_close(
    ctx: &egui::Context,
    project_id: &str,
    name: &str,
    catalog: &i18n::Catalog,
) -> Option<bool> {
    choice(
        ctx,
        ChoiceSpec {
            popup: PopupSpec {
                id: egui::Id::new("env_project_close_confirmation"),
                width: 400.0,
                title: &catalog.t("env.project_close_confirm.title", &[]),
                subtitle: name,
                close_label: &catalog.t("popup.dismiss", &[]),
                close_enabled: true,
            },
            target: egui::Id::new(project_id),
            accept: &catalog.t("action.close", &[]),
            tone: ActionTone::Primary,
        },
        catalog,
        |ui| {
            popup::notice(
                ui,
                &catalog.t("env.project_close_confirm.body", &[("name", name)]),
                NoticeTone::Info,
            )
        },
    )
}

pub(crate) fn delete_variable(
    ctx: &egui::Context,
    profile_id: &str,
    key: &str,
    selected: &mut Option<String>,
    files: &[String],
    catalog: &i18n::Catalog,
) -> Option<bool> {
    choice(
        ctx,
        ChoiceSpec {
            popup: PopupSpec {
                id: egui::Id::new("env_delete_confirmation"),
                width: 480.0,
                title: &catalog.t("env.var_delete_confirm.title", &[]),
                subtitle: key,
                close_label: &catalog.t("popup.dismiss", &[]),
                close_enabled: true,
            },
            target: egui::Id::new((profile_id, key)),
            accept: &catalog.t("action.delete", &[]),
            tone: ActionTone::Danger,
        },
        catalog,
        |ui| {
            popup::notice(
                ui,
                &catalog.t("env.var_delete_confirm.body_dotenv", &[("key", key)]),
                NoticeTone::Error,
            );
            popup::field(ui, &catalog.t("env.delete_source", &[]), None, |ui| {
                let label = selected
                    .clone()
                    .unwrap_or_else(|| catalog.t("env.delete_all_sources", &[]));
                popup::choice_input(ui, "dotenv_delete_source", &label, |ui| {
                    ui.selectable_value(selected, None, catalog.t("env.delete_all_sources", &[]));
                    for file in files {
                        ui.selectable_value(selected, Some(file.clone()), file);
                    }
                });
            });
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;

    #[test]
    fn env_ports_popup_project_close_requires_fresh_choice_for_next_project() {
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (String, Option<bool>)| {
                if state.1.is_none() {
                    state.1 = project_close(ui.ctx(), &state.0, "Same name", &catalog);
                }
            },
            ("project-a".into(), None),
        );
        harness.run();
        let close = harness.get_by_label("Close").rect();
        assert!((close.height() - 34.0).abs() < 0.5);
        harness.get_by_label("Close").focus();
        harness.run();
        harness.state_mut().0 = "project-b".into();
        harness.key_press(egui::Key::Enter);
        harness.run();
        assert!(harness.state().1.is_none());
        harness.get_by_label("Cancel").click();
        harness.run();
        assert_eq!(harness.state().1, Some(false));
    }

    #[test]
    #[ignore = "offscreen environment and notice popup PNGs for visual review"]
    fn env_ports_popup_render_environment_and_notices() {
        for case in [9, 15, 33, 34, 35] {
            let catalog = i18n::Catalog::load("ko-KR").unwrap();
            let mut harness = egui_kittest::Harness::builder()
                .with_size(egui::vec2(800.0, 650.0))
                .build_ui_state(
                    move |ui, selected| match case {
                        9 => {
                            let _ = project_close(ui.ctx(), "fixture", "Serenity", &catalog);
                        }
                        15 => {
                            let _ = delete_variable(
                                ui.ctx(),
                                "fixture",
                                "API_URL",
                                selected,
                                &[".env".into(), ".env.local".into()],
                                &catalog,
                            );
                        }
                        _ => {
                            let (title, message) = match case {
                                33 => (
                                    catalog.t("runtime.event_overflow.title", &[]),
                                    catalog.t("runtime.event_overflow.body", &[]),
                                ),
                                34 => (
                                    catalog.t("workspace.warm_limit.title", &[]),
                                    catalog.t(
                                        "workspace.warm_limit.body",
                                        &[("target", "Design"), ("limit", "4")],
                                    ),
                                ),
                                _ => (
                                    catalog.t("workspace.cross_pane.open_failed.title", &[]),
                                    catalog.t("workspace.cross_pane.open_failed.stale_target", &[]),
                                ),
                            };
                            let _ = popup::information(
                                ui.ctx(),
                                popup::InformationSpec {
                                    id: egui::Id::new(("notice-fixture", case)),
                                    title: &title,
                                    message: &message,
                                    accept_label: &catalog.t("action.close", &[]),
                                    close_label: &catalog.t("popup.dismiss", &[]),
                                },
                            );
                        }
                    },
                    None,
                );
            crate::fonts::install_cjk_fallback(&harness.ctx, None, "JetBrainsMono", "Regular");
            harness.ctx.set_visuals(egui::Visuals::dark());
            harness.run();
            let output = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/popup-parity/20261003");
            std::fs::create_dir_all(&output).unwrap();
            harness
                .render()
                .unwrap()
                .save(output.join(format!("{case:02}.png")))
                .unwrap();
        }
    }
}
