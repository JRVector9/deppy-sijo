//! Bounded work reports. Reported progress and local terminal status stay separate.
use super::{
    CloudAgent, layout,
    settings::{SettingsState, WorkKind},
};
use agent_mcp::{RunState, now};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Filter {
    #[default]
    All,
    Active,
    Finished,
}
#[derive(Clone, Default)]
struct RunsState {
    search: String,
    filter: Filter,
    notice: Option<(String, String, &'static str, Option<String>)>,
    navigation_notice: Option<(String, String, u64)>,
    selected: Option<(String, String)>,
}

impl CloudAgent {
    pub(super) fn work_settings(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        settings: &mut SettingsState,
    ) {
        let id = egui::Id::new("cloud-run-settings");
        let mut state: RunsState = ui
            .ctx()
            .data_mut(|data| data.get_temp(id).unwrap_or_default());
        if self.selected_record.is_some() {
            // Notification navigation is one-shot; remove the old filter so
            // the revealed answer remains selected on subsequent frames.
            state.search.clear();
        }
        ui.horizontal_wrapped(|ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                for (kind, key) in [
                    (WorkKind::Runs, "cloud.ui.runs"),
                    (WorkKind::Answers, "cloud.ui.answers"),
                    (WorkKind::Receipts, "cloud.ui.receipts"),
                ] {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(catalog.t(key, &[])).size(13.0).color(
                                    if settings.work_kind == kind {
                                        layout::accent(ui)
                                    } else {
                                        ui.visuals().text_color()
                                    },
                                ),
                            )
                            .selected(settings.work_kind == kind)
                            .min_size(egui::vec2(0.0, 32.0)),
                        )
                        .clicked()
                    {
                        settings.work_kind = kind;
                    }
                }
            });
            let search = layout::text_input(
                ui,
                ui.available_width().min(320.0),
                egui::TextEdit::singleline(&mut state.search)
                    .char_limit(128)
                    .hint_text(catalog.t("cloud.ui.search_hint", &[]))
                    .desired_width(ui.available_width().min(320.0)),
            );
            search.widget_info(|| {
                egui::WidgetInfo::text_edit(
                    ui.is_enabled(),
                    &state.search,
                    &state.search,
                    catalog.t("cloud.run_search", &[]),
                )
            });
            ui.ctx().accesskit_node_builder(search.id, |node| {
                node.set_label(catalog.t("cloud.run_search", &[]))
            });
            if settings.work_kind == WorkKind::Runs {
                ui.menu_button("⋯", |ui| {
                    for (filter, key) in [
                        (Filter::All, "cloud.run_filter_all"),
                        (Filter::Active, "cloud.run_filter_active"),
                        (Filter::Finished, "cloud.run_filter_finished"),
                    ] {
                        ui.selectable_value(&mut state.filter, filter, catalog.t(key, &[]));
                    }
                });
            }
        });
        ui.add_space(13.0);
        layout::hint(ui, catalog.t("cloud.ui.work_hint", &[]));
        ui.add_space(12.0);
        layout::line(ui);
        if settings.work_kind != WorkKind::Runs {
            self.history_settings(ui, catalog, settings, &state.search);
        } else {
            self.run_settings(ui, catalog, &mut state);
        }
        ui.ctx().data_mut(|data| data.insert_temp(id, state));
    }
    fn run_settings(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog, state: &mut RunsState) {
        let query = state.search.trim().to_lowercase();
        let runs = self
            .cloud_runs()
            .into_iter()
            .take(100)
            .filter(|run| {
                matches!(state.filter, Filter::All)
                    || matches!(state.filter, Filter::Finished) == run.state.is_terminal()
            })
            .filter(|run| {
                query.is_empty()
                    || [
                        run.title.as_str(),
                        run.instruction.as_str(),
                        run.connection_name.as_str(),
                        run.id.as_str(),
                        run.connection_id.as_str(),
                        run.workspace.as_str(),
                        run.session.as_str(),
                        run.generation.as_str(),
                        run.message.as_str(),
                        run.result.as_deref().unwrap_or(""),
                    ]
                    .iter()
                    .any(|text| text.to_lowercase().contains(&query))
            })
            .collect::<Vec<_>>();
        if !runs.iter().any(|run| {
            state.selected.as_ref() == Some(&(run.connection_id.clone(), run.id.clone()))
        }) {
            state.selected = runs
                .first()
                .map(|run| (run.connection_id.clone(), run.id.clone()));
        }
        if runs.is_empty() {
            layout::hint(
                ui,
                catalog.t(
                    if self.has_cloud_runs() {
                        "cloud.runs_no_match"
                    } else {
                        "cloud.runs_empty"
                    },
                    &[],
                ),
            );
            return;
        }
        let selected = state.selected.clone();
        let mut clicked = None;
        layout::list_detail(
            ui,
            "cloud-work",
            1.15 / 2.15,
            |ui| {
                for run in &runs {
                    ui.push_id((&run.connection_id, &run.id), |ui| {
                        let status = catalog.t(run_state_key(run.state), &[]);
                        let heading =
                            format!("{} · {} · {}", run.title, status, run.connection_name);
                        let subtitle = format!(
                            "{} · {} · {}",
                            run.connection_name,
                            run.workspace,
                            crate::ui::notifications::relative_time_label(
                                catalog,
                                (now() as i64).saturating_sub(run.heartbeat).max(0)
                            )
                        );
                        if layout::list_row(
                            ui,
                            selected.as_ref() == Some(&(run.connection_id.clone(), run.id.clone())),
                            &heading,
                            layout::RowContent {
                                title: &run.title,
                                subtitle: &subtitle,
                                status: &status,
                                icon: None,
                            },
                            if run.state == RunState::Unknown {
                                layout::warn(ui)
                            } else {
                                layout::good(ui)
                            },
                        )
                        .clicked()
                        {
                            clicked = Some((run.connection_id.clone(), run.id.clone()));
                        }
                    });
                }
            },
            |ui| {
                if let Some(run) = runs.iter().find(|run| {
                    selected.as_ref() == Some(&(run.connection_id.clone(), run.id.clone()))
                }) {
                    layout::detail_title(ui, &run.title);
                    self.run_detail(ui, catalog, state, run);
                }
            },
        );
        if let Some(clicked) = clicked {
            state.selected = Some(clicked);
            ui.ctx().request_repaint();
        }
    }
    fn run_detail(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        state: &mut RunsState,
        run: &agent_mcp::CloudRun,
    ) {
        let age = crate::ui::notifications::relative_time_label(
            catalog,
            (now() as i64).saturating_sub(run.heartbeat).max(0),
        );
        let target = self
            .targets
            .iter()
            .find(|target| {
                target.id == run.session
                    && target.generation == run.generation
                    && target.workspace == run.workspace
            })
            .cloned();
        ui.add_space(14.0);
        layout::fact(
            ui,
            &catalog.t("cloud.ui.agent_report", &[]),
            &format!("{} · {}", catalog.t(run_state_key(run.state), &[]), age),
        );
        let local = target
            .as_ref()
            .map(|target| target.local_status_label(catalog))
            .unwrap_or_else(|| catalog.t("cloud.run_original_unavailable", &[]));
        layout::fact(ui, &catalog.t("cloud.ui.local_terminal", &[]), &local);
        let origin = target
            .as_ref()
            .map(|target| self.redact(&format!("{} · {}", target.workspace_name, target.title)))
            .unwrap_or_else(|| catalog.t("cloud.run_original_unavailable", &[]));
        layout::fact(ui, &catalog.t("cloud.ui.origin", &[]), &origin);
        ui.add_space(12.0);
        if let Some(progress) = run.progress {
            layout::hint(
                ui,
                catalog.t("cloud.run_progress", &[("progress", &progress.to_string())]),
            );
            ui.add_space(13.0);
            let (rect, response) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 3.0), egui::Sense::hover());
            ui.painter().rect_filled(
                rect,
                0.0,
                ui.visuals().widgets.noninteractive.bg_stroke.color,
            );
            ui.painter().rect_filled(
                egui::Rect::from_min_size(
                    rect.min,
                    egui::vec2(rect.width() * f32::from(progress.min(100)) / 100.0, 3.0),
                ),
                0.0,
                layout::accent(ui),
            );
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Other,
                    ui.is_enabled(),
                    catalog.t("cloud.ui.agent_report", &[]),
                )
            });
            ui.ctx().accesskit_node_builder(response.id, |node| {
                node.set_role(egui::accesskit::Role::ProgressIndicator);
                node.set_label(catalog.t("cloud.ui.agent_report", &[]));
                node.set_numeric_value(f64::from(progress));
                node.set_min_numeric_value(0.0);
                node.set_max_numeric_value(100.0);
            });
            ui.add_space(13.0);
        } else {
            layout::hint(ui, catalog.t("cloud.run_progress_none", &[]));
        }
        if run.state == RunState::Unknown {
            layout::hint(ui, catalog.t("cloud.run_unknown_hint", &[]));
        }
        if !run.message.is_empty() {
            ui.label(egui::RichText::new(&run.message).size(13.0));
        }
        ui.add_space(12.0);
        ui.horizontal_wrapped(|ui| {
            if layout::button(ui, &catalog.t("cloud.run_open_original", &[])).clicked() {
                state.notice = None;
                match self.request_run_navigation(&run.workspace, &run.session) {
                    Ok(request) => {
                        state.navigation_notice =
                            Some((run.connection_id.clone(), run.id.clone(), request))
                    }
                    Err(error) => {
                        state.navigation_notice = None;
                        state.notice = Some((
                            run.connection_id.clone(),
                            run.id.clone(),
                            "cloud.run_navigation_failed",
                            Some(error),
                        ));
                    }
                }
                ui.ctx().request_repaint();
            }
            ui.add_enabled_ui(
                !run.state.is_terminal() && run.cancel_requested_at.is_none(),
                |ui| {
                    if layout::button(ui, &catalog.t("cloud.run_cancel", &[])).clicked() {
                        let error = self.request_run_cancel(&run.connection_id, &run.id).err();
                        let key = if error.is_some() {
                            "cloud.management_failed"
                        } else {
                            "cloud.run_cancel_queued"
                        };
                        state.notice =
                            Some((run.connection_id.clone(), run.id.clone(), key, error));
                        ui.ctx().request_repaint();
                    }
                },
            );
        });
        ui.add_space(12.0);
        layout::hint(ui, catalog.t("cloud.run_cancel_hint", &[]));
        ui.add_space(14.0);
        egui::CollapsingHeader::new(catalog.t("cloud.ui.run_details", &[]))
            .id_salt(("run-details", &run.connection_id, &run.id))
            .show(ui, |ui| {
                layout::hint(
                    ui,
                    catalog.t("cloud.run_workspace", &[("workspace", &run.workspace)]),
                );
                layout::hint(
                    ui,
                    catalog.t(
                        "cloud.run_origin",
                        &[("session", &run.session), ("generation", &run.generation)],
                    ),
                );
                layout::hint(ui, catalog.t("cloud.run_navigation_hint", &[]));
                ui.add(
                    egui::Label::new(
                        self.redaction
                            .redact_wrapped_text(&format!("{} · {}", run.connection_id, run.id)),
                    )
                    .truncate(),
                )
                .on_hover_text(
                    self.redaction
                        .redact_wrapped_text(&format!("{}\n{}", run.connection_id, run.id)),
                );
                if let Some(target) = &target {
                    layout::hint(ui, target.local_observation_label(catalog));
                    if let Some(agent) = &target.agent_line {
                        layout::hint(
                            ui,
                            self.redact(&catalog.t("cloud.local_agent", &[("agent", agent)])),
                        );
                    }
                    if let Some(observed) = target.screen_observed_at {
                        let age = crate::ui::notifications::relative_time_label(
                            catalog,
                            i64::try_from(now().saturating_sub(observed)).unwrap_or(i64::MAX),
                        );
                        layout::hint(ui, catalog.t("cloud.run_screen_at", &[("age", &age)]));
                    }
                    if !target.live {
                        layout::hint(ui, catalog.t("cloud.exited", &[]));
                    }
                }
                if !run.instruction.is_empty() {
                    egui::CollapsingHeader::new(catalog.t("cloud.run_instruction", &[]))
                        .id_salt(("instruction", &run.connection_id, &run.id))
                        .show(ui, |ui| {
                            ui.label(&run.instruction);
                        });
                }
            });
        if let Some(result) = &run.result {
            ui.label(result);
            if ui.button(catalog.t("cloud.run_copy_result", &[])).clicked() {
                ui.ctx()
                    .copy_text(self.redaction.redact_wrapped_text(result));
            }
        }
        for artifact in run.artifacts.iter().take(16).filter(|artifact| {
            artifact.validate().is_ok()
                && self.redaction.redact_wrapped_text(&artifact.uri) == artifact.uri
        }) {
            ui.hyperlink_to(&artifact.label, &artifact.uri);
        }
        if run.cancel_acknowledged_at.is_some() {
            ui.label(catalog.t("cloud.run_cancel_ack", &[]));
        } else if run.cancel_requested_at.is_some() {
            ui.label(catalog.t("cloud.run_cancel_pending", &[]));
        }
        if let Some((connection, id, key, error)) = &state.notice
            && connection == &run.connection_id
            && id == &run.id
            && (*key != "cloud.run_cancel_queued" || run.cancel_requested_at.is_none())
        {
            let response = ui.label(catalog.t(key, &[]));
            if let Some(code) = error {
                response.on_hover_text(code);
            }
        }
        if let Some((connection, id, request)) = &state.navigation_notice
            && connection == &run.connection_id
            && id == &run.id
        {
            let (key, error) = match self.run_navigation_result(*request) {
                Some(Ok(())) => ("cloud.run_navigation_opened", None),
                Some(Err(error)) => ("cloud.run_navigation_failed", Some(error)),
                None => ("cloud.run_navigation_queued", None),
            };
            let response = ui.label(catalog.t(key, &[]));
            if let Some(error) = error {
                response.on_hover_text(error);
            }
        }
    }
}

fn run_state_key(state: RunState) -> &'static str {
    match state {
        RunState::Queued => "cloud.run_queued",
        RunState::Running => "cloud.run_running",
        RunState::Waiting => "cloud.run_waiting",
        RunState::Completed => "cloud.run_completed",
        RunState::Failed => "cloud.run_failed",
        RunState::Cancelled => "cloud.run_cancelled",
        RunState::Unknown => "cloud.run_unknown",
    }
}
