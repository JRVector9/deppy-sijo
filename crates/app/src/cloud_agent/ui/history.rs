//! Independently scoped answers/receipts; notification navigation is one-shot.
use super::{
    CloudAgent, layout,
    settings::{SettingsState, WorkKind},
};

#[derive(Clone, Default)]
struct HistoryState {
    selected: Option<String>,
    navigation: Option<u64>,
    error: Option<String>,
}
fn record_key(record: &agent_mcp::Record) -> String {
    agent_mcp::operation_key(
        record
            .connection_id
            .as_deref()
            .unwrap_or(agent_mcp::LEGACY_CONNECTION_ID),
        &record.id,
    )
    .unwrap_or_else(|_| record.id.clone())
}
impl CloudAgent {
    pub(super) fn history_settings(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        settings: &SettingsState,
        search: &str,
    ) {
        let id = egui::Id::new(("cloud-history-selection", settings.work_kind));
        let mut state: HistoryState = ui
            .ctx()
            .data_mut(|data| data.get_temp(id).unwrap_or_default());
        let reveal = self.selected_record.take();
        if let Some(record) = &reveal {
            state.selected = Some(record.clone());
            state.navigation = None;
            state.error = None;
        }
        let connections = self.connection_infos();
        let connection_name = |record: &agent_mcp::Record| {
            connections
                .iter()
                .find(|info| {
                    info.principal.id
                        == record
                            .connection_id
                            .as_deref()
                            .unwrap_or(agent_mcp::LEGACY_CONNECTION_ID)
                })
                .map(|info| self.redact(&info.principal.name))
                .unwrap_or_else(|| catalog.t("cloud.ui.previous_connection", &[]))
        };
        let query = search.trim().to_lowercase();
        let records = self
            .records
            .iter()
            .filter(|record| (record.tool == "notify") == (settings.work_kind == WorkKind::Answers))
            .filter(|record| {
                query.is_empty()
                    || self
                        .redact(&format!(
                            "{} {} {} {}",
                            record.message,
                            record.workspace,
                            record.session,
                            connection_name(record)
                        ))
                        .to_lowercase()
                        .contains(&query)
                    || reveal.as_deref() == Some(&record_key(record))
            })
            .take(256)
            .cloned()
            .collect::<Vec<_>>();
        if !records
            .iter()
            .any(|record| state.selected.as_deref() == Some(&record_key(record)))
        {
            state.selected = records.first().map(record_key);
        }
        if records.is_empty() {
            layout::hint(ui, catalog.t("cloud.no_history", &[]));
            return;
        }
        let selected = state.selected.clone();
        let mut clicked = None;
        let rows = records
            .iter()
            .map(|record| {
                let kind = catalog.t(
                    match record.tool.as_str() {
                        "notify" => "cloud.history_answer",
                        "send_ctrl_c" => "cloud.history_interrupt",
                        _ => "cloud.history_input",
                    },
                    &[],
                );
                let age = crate::ui::notifications::relative_time_label(
                    catalog,
                    agent_mcp::now() as i64 - record.created,
                );
                let outcome: serde_json::Value =
                    serde_json::from_str(&record.outcome).unwrap_or_default();
                let status = catalog.t(
                    match outcome["status"].as_str() {
                        Some("queued") => "cloud.outcome_queued",
                        Some("stored") => "cloud.outcome_stored",
                        Some("rejected") => "cloud.outcome_rejected",
                        _ => "cloud.outcome_unknown",
                    },
                    &[],
                );
                let message = self.redaction.redact_wrapped_text(&record.message);
                let title = message
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .map(|line| line.chars().take(100).collect::<String>())
                    .unwrap_or_else(|| kind.clone());
                let heading = format!(
                    "{} · {} · {}",
                    kind,
                    age,
                    self.redact(&record.session)
                        .chars()
                        .take(8)
                        .collect::<String>()
                );
                (
                    record_key(record),
                    heading,
                    title,
                    format!(
                        "{} · {} · {}",
                        connection_name(record),
                        self.redact(&record.workspace),
                        age
                    ),
                    status,
                )
            })
            .collect::<Vec<_>>();
        layout::list_detail(
            ui,
            "cloud-history",
            1.15 / 2.15,
            |ui| {
                for (key, heading, title, subtitle, status) in &rows {
                    ui.push_id(key, |ui| {
                        let response = layout::list_row(
                            ui,
                            selected.as_deref() == Some(key),
                            heading,
                            layout::RowContent {
                                title,
                                subtitle,
                                status,
                                icon: None,
                            },
                            layout::secondary(ui),
                        );
                        if reveal.as_deref() == Some(key) {
                            response.scroll_to_me(Some(egui::Align::TOP));
                        }
                        if response.clicked() {
                            clicked = Some(key.clone());
                        }
                    });
                }
            },
            |ui| {
                if let Some(record) = records
                    .iter()
                    .find(|record| selected.as_deref() == Some(&record_key(record)))
                {
                    let index = records
                        .iter()
                        .position(|row| record_key(row) == record_key(record))
                        .unwrap_or(0);
                    layout::detail_title(
                        ui,
                        catalog.t(
                            if settings.work_kind == WorkKind::Answers {
                                "cloud.history_answer"
                            } else {
                                "cloud.ui.receipts"
                            },
                            &[],
                        ),
                    );
                    layout::fact(
                        ui,
                        &catalog.t("cloud.ui.origin", &[]),
                        &self.redact(&format!("{} / {}", record.workspace, record.session)),
                    );
                    ui.label(&rows[index].4);
                    if !record.message.is_empty() {
                        ui.add_space(12.0);
                        let message = self.redaction.redact_wrapped_text(&record.message);
                        ui.label(&message);
                        if layout::button(ui, &catalog.t("cloud.copy_answer", &[])).clicked() {
                            ui.ctx()
                                .copy_text(self.redaction.redact_wrapped_text(&message));
                        }
                    }
                    if layout::button(ui, &catalog.t("cloud.run_open_original", &[])).clicked() {
                        match self.request_run_navigation(&record.workspace, &record.session) {
                            Ok(request) => {
                                state.navigation = Some(request);
                                state.error = None;
                            }
                            Err(error) => {
                                state.navigation = None;
                                state.error = Some(error);
                            }
                        }
                    }
                    if let Some(error) = &state.error {
                        ui.label(catalog.t("cloud.run_navigation_failed", &[]))
                            .on_hover_text(error);
                    }
                    if let Some(request) = state.navigation {
                        let (key, error) = match self.run_navigation_result(request) {
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
            },
        );
        if let Some(clicked) = clicked {
            state.selected = Some(clicked);
            state.navigation = None;
            state.error = None;
            ui.ctx().request_repaint();
        }
        ui.ctx().data_mut(|data| data.insert_temp(id, state));
    }
}
