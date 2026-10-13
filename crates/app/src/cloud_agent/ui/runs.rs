//! Bounded work reports. Reported progress and local terminal status stay separate.
use super::CloudAgent;
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
}

impl CloudAgent {
    pub(super) fn run_settings(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        ui.separator();
        ui.heading(catalog.t("cloud.runs", &[]));
        ui.weak(catalog.t("cloud.runs_hint", &[]));
        let id = egui::Id::new("cloud-run-settings");
        let mut state: RunsState = ui
            .ctx()
            .data_mut(|data| data.get_temp(id).unwrap_or_default());
        ui.horizontal_wrapped(|ui| {
            let label = ui.label(catalog.t("cloud.run_search", &[]));
            ui.add(
                egui::TextEdit::singleline(&mut state.search)
                    .char_limit(128)
                    .desired_width(180.0),
            )
            .labelled_by(label.id);
            for (filter, key) in [
                (Filter::All, "cloud.run_filter_all"),
                (Filter::Active, "cloud.run_filter_active"),
                (Filter::Finished, "cloud.run_filter_finished"),
            ] {
                ui.selectable_value(&mut state.filter, filter, catalog.t(key, &[]));
            }
        });
        let query = state.search.trim().to_lowercase();
        let runs = self
            .cloud_runs()
            .iter()
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
            .cloned()
            .collect::<Vec<_>>();
        if runs.is_empty() {
            ui.weak(catalog.t(
                if self.cloud_runs().is_empty() {
                    "cloud.runs_empty"
                } else {
                    "cloud.runs_no_match"
                },
                &[],
            ));
        }
        egui::ScrollArea::vertical()
            .id_salt("cloud-run-list")
            .max_height(420.0)
            .show(ui, |ui| {
                for run in &runs {
                    let heading = format!(
                        "{} · {} · {}",
                        run.title,
                        catalog.t(run_state_key(run.state), &[]),
                        run.connection_name
                    );
                    egui::CollapsingHeader::new(heading)
                        .id_salt((&run.connection_id, &run.id))
                        .show(ui, |ui| {
                            let age = crate::ui::notifications::relative_time_label(
                                catalog,
                                (now() as i64).saturating_sub(run.heartbeat).max(0),
                            );
                            ui.weak(catalog.t("cloud.run_reported_at", &[("age", &age)]));
                            if let Some(progress) = run.progress {
                                ui.label(catalog.t(
                                    "cloud.run_progress",
                                    &[("progress", &progress.to_string())],
                                ));
                            } else {
                                ui.weak(catalog.t("cloud.run_progress_none", &[]));
                            }
                            if run.state == RunState::Unknown {
                                ui.label(catalog.t("cloud.run_unknown_hint", &[]));
                            }
                            if !run.message.is_empty() {
                                ui.label(&run.message);
                            }
                            ui.weak(
                                catalog.t("cloud.run_workspace", &[("workspace", &run.workspace)]),
                            );
                            ui.weak(catalog.t(
                                "cloud.run_origin",
                                &[("session", &run.session), ("generation", &run.generation)],
                            ));
                            if ui
                                .button(catalog.t("cloud.run_open_original", &[]))
                                .clicked()
                            {
                                state.notice = None;
                                match self.request_run_navigation(&run.workspace, &run.session) {
                                    Ok(request) => {
                                        state.navigation_notice = Some((
                                            run.connection_id.clone(),
                                            run.id.clone(),
                                            request,
                                        ))
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
                            ui.weak(catalog.t("cloud.run_navigation_hint", &[]));
                            ui.add(
                                egui::Label::new(format!("{} · {}", run.connection_id, run.id))
                                    .truncate(),
                            )
                            .on_hover_text(format!("{}\n{}", run.connection_id, run.id));
                            if let Some(target) = self.targets.iter().find(|target| {
                                target.id == run.session
                                    && target.generation == run.generation
                                    && target.workspace == run.workspace
                            }) {
                                ui.weak(catalog.t(
                                    "cloud.run_local_status",
                                    &[("status", &target.local_status_label(catalog))],
                                ));
                                ui.weak(target.local_observation_label(catalog));
                                if let Some(agent) = &target.agent_line {
                                    ui.weak(catalog.t("cloud.local_agent", &[("agent", agent)]));
                                }
                                if let Some(observed) = target.screen_observed_at {
                                    let age = crate::ui::notifications::relative_time_label(
                                        catalog,
                                        i64::try_from(now().saturating_sub(observed))
                                            .unwrap_or(i64::MAX),
                                    );
                                    ui.weak(catalog.t("cloud.run_screen_at", &[("age", &age)]));
                                }
                                if !target.live {
                                    ui.weak(catalog.t("cloud.exited", &[]));
                                }
                            } else {
                                ui.weak(catalog.t("cloud.run_original_unavailable", &[]));
                            }
                            if !run.instruction.is_empty() {
                                egui::CollapsingHeader::new(
                                    catalog.t("cloud.run_instruction", &[]),
                                )
                                .id_salt(("instruction", &run.connection_id, &run.id))
                                .show(ui, |ui| {
                                    ui.label(&run.instruction);
                                });
                            }
                            if let Some(result) = &run.result {
                                ui.label(result);
                                if ui.button(catalog.t("cloud.run_copy_result", &[])).clicked() {
                                    ui.ctx().copy_text(result.clone());
                                }
                            }
                            for artifact in run
                                .artifacts
                                .iter()
                                .take(16)
                                .filter(|artifact| artifact.validate().is_ok())
                            {
                                ui.hyperlink_to(&artifact.label, &artifact.uri);
                            }
                            if run.cancel_acknowledged_at.is_some() {
                                ui.label(catalog.t("cloud.run_cancel_ack", &[]));
                            } else if run.cancel_requested_at.is_some() {
                                ui.label(catalog.t("cloud.run_cancel_pending", &[]));
                            }
                            if ui
                                .add_enabled(
                                    !run.state.is_terminal() && run.cancel_requested_at.is_none(),
                                    egui::Button::new(catalog.t("cloud.run_cancel", &[])),
                                )
                                .clicked()
                            {
                                let error =
                                    self.request_run_cancel(&run.connection_id, &run.id).err();
                                let key = if error.is_some() {
                                    "cloud.management_failed"
                                } else {
                                    "cloud.run_cancel_queued"
                                };
                                state.notice =
                                    Some((run.connection_id.clone(), run.id.clone(), key, error));
                                ui.ctx().request_repaint();
                            }
                            if let Some((connection, id, key, error)) = &state.notice
                                && connection == &run.connection_id
                                && id == &run.id
                                && (*key != "cloud.run_cancel_queued"
                                    || run.cancel_requested_at.is_none())
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
                                    Some(Err(error)) => {
                                        ("cloud.run_navigation_failed", Some(error))
                                    }
                                    None => ("cloud.run_navigation_queued", None),
                                };
                                let response = ui.label(catalog.t(key, &[]));
                                if let Some(error) = error {
                                    response.on_hover_text(error);
                                }
                            }
                            ui.weak(catalog.t("cloud.run_cancel_hint", &[]));
                        });
                }
            });
        ui.ctx().data_mut(|data| data.insert_temp(id, state));
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
