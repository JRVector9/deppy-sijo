use crate::agent_launcher::{AgentKind, DetectionSnapshot, LaunchOptions, ReasoningEffort};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LauncherErrorCode {
    DetectionFailed,
    LaunchBusy,
    LaunchFailed,
    AgentUnavailable,
}

impl LauncherErrorCode {
    const fn message_key(self) -> &'static str {
        match self {
            Self::DetectionFailed => "agent_launcher.error.detection_failed",
            Self::LaunchBusy => "agent_launcher.error.busy",
            Self::LaunchFailed => "agent_launcher.error.launch_failed",
            Self::AgentUnavailable => "agent_launcher.error.unavailable",
        }
    }
}

pub(crate) enum AgentLauncherIntent {
    Refresh,
    Launch {
        workspace_id: String,
        kind: AgentKind,
        options: LaunchOptions,
    },
    BlankTerminal {
        workspace_id: String,
    },
}

pub(crate) struct AgentLauncherUi {
    open: bool,
    workspace_id: String,
    workspace_name: String,
    selected: Option<AgentKind>,
    model: String,
    effort: Option<ReasoningEffort>,
    yolo: bool,
    launch_pending: bool,
    error: Option<LauncherErrorCode>,
}

impl AgentLauncherUi {
    pub(crate) fn new() -> Self {
        Self {
            open: false,
            workspace_id: String::new(),
            workspace_name: String::new(),
            selected: None,
            model: String::new(),
            effort: None,
            yolo: false,
            launch_pending: false,
            error: None,
        }
    }

    pub(crate) fn open_for(&mut self, workspace_id: String, workspace_name: String) {
        self.workspace_id = workspace_id;
        self.workspace_name = workspace_name;
        self.model.clear();
        self.effort = None;
        self.yolo = false;
        self.launch_pending = false;
        self.error = None;
        self.open = true;
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) fn launch_succeeded(&mut self) {
        self.launch_pending = false;
        self.error = None;
        self.open = false;
    }

    pub(crate) fn detection_succeeded(&mut self) {
        if self.error == Some(LauncherErrorCode::DetectionFailed) {
            self.error = None;
        }
    }

    pub(crate) fn report_error(&mut self, error: LauncherErrorCode) {
        self.launch_pending = false;
        self.error = Some(error);
    }

    pub(crate) fn show(
        &mut self,
        ctx: &egui::Context,
        snapshot: Option<&DetectionSnapshot>,
        detecting: bool,
        catalog: &i18n::Catalog,
    ) -> Option<AgentLauncherIntent> {
        if !self.open {
            return None;
        }
        self.reconcile_selection(snapshot);

        let mut intent = None;
        let response = egui::Modal::new(egui::Id::new("agent-launcher-modal")).show(ctx, |ui| {
            ui.set_min_width(560.0);
            ui.set_max_width(680.0);
            ui.heading(catalog.t("agent_launcher.title", &[]));
            ui.weak(catalog.t(
                "agent_launcher.subtitle",
                &[("project", self.workspace_name.as_str())],
            ));
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                ui.strong(catalog.t("agent_launcher.installed", &[]));
                if detecting {
                    ui.spinner();
                    ui.weak(catalog.t("agent_launcher.detecting", &[]));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(
                            !detecting && !self.launch_pending,
                            egui::Button::new(catalog.t("agent_launcher.refresh", &[])),
                        )
                        .clicked()
                    {
                        intent = Some(AgentLauncherIntent::Refresh);
                    }
                });
            });

            match snapshot {
                Some(snapshot) if !snapshot.agents().is_empty() => {
                    egui::ScrollArea::vertical()
                        .id_salt("agent-launcher-installed")
                        .max_height(260.0)
                        .show(ui, |ui| {
                            for agent in snapshot.agents() {
                                let kind = agent.kind();
                                if agent_card(ui, kind, self.selected == Some(kind)).clicked()
                                    && !self.launch_pending
                                {
                                    self.select(kind);
                                }
                                ui.add_space(5.0);
                            }
                        });
                }
                Some(_) if !detecting => {
                    ui.group(|ui| {
                        ui.label(catalog.t("agent_launcher.none_detected", &[]));
                        ui.weak(catalog.t("agent_launcher.none_detected_hint", &[]));
                    });
                }
                _ => {
                    ui.add_space(24.0);
                }
            }

            if let Some(kind) = self.selected {
                ui.add_space(8.0);
                crate::ui::hairline(ui);
                ui.add_space(8.0);
                self.render_options(ui, kind, catalog);
            }

            if let Some(error) = self.error {
                ui.add_space(6.0);
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    catalog.t(error.message_key(), &[]),
                );
            }

            ui.add_space(10.0);
            crate::ui::hairline(ui);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !self.launch_pending,
                        egui::Button::new(catalog.t("agent_launcher.blank_terminal", &[])),
                    )
                    .on_hover_text(catalog.t("agent_launcher.blank_terminal_hint", &[]))
                    .clicked()
                {
                    self.open = false;
                    intent = Some(AgentLauncherIntent::BlankTerminal {
                        workspace_id: self.workspace_id.clone(),
                    });
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(
                            self.selected.is_some() && !self.launch_pending,
                            egui::Button::new(catalog.t("agent_launcher.launch", &[])),
                        )
                        .clicked()
                        && let Some(kind) = self.selected
                    {
                        self.launch_pending = true;
                        self.error = None;
                        intent = Some(AgentLauncherIntent::Launch {
                            workspace_id: self.workspace_id.clone(),
                            kind,
                            options: LaunchOptions {
                                model: self.model.clone(),
                                effort: self.effort,
                                yolo: self.yolo,
                            },
                        });
                    }
                    if self.launch_pending {
                        ui.spinner();
                        ui.weak(catalog.t("agent_launcher.launching", &[]));
                    }
                });
            });
        });

        if response.should_close() && !self.launch_pending {
            self.open = false;
        }
        intent
    }

    fn render_options(&mut self, ui: &mut egui::Ui, kind: AgentKind, catalog: &i18n::Catalog) {
        let models = kind.supported_models();
        if !models.is_empty() {
            ui.horizontal(|ui| {
                ui.label(catalog.t("agent_launcher.model", &[]));
                let selected = models
                    .iter()
                    .find(|choice| choice.value() == self.model)
                    .map_or_else(
                        || catalog.t("agent_launcher.model_hint", &[]),
                        |choice| choice.label().to_owned(),
                    );
                ui.add_enabled_ui(!self.launch_pending, |ui| {
                    egui::ComboBox::from_id_salt(("agent-launcher-model", kind.id()))
                        .selected_text(selected)
                        .width(360.0)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(
                                    self.model.is_empty(),
                                    catalog.t("agent_launcher.model_hint", &[]),
                                )
                                .clicked()
                            {
                                self.model.clear();
                                self.reconcile_effort(kind);
                            }
                            for choice in models {
                                if ui
                                    .selectable_label(self.model == choice.value(), choice.label())
                                    .clicked()
                                {
                                    self.model = choice.value().to_owned();
                                    self.reconcile_effort(kind);
                                }
                            }
                        });
                });
            });
        }

        let efforts = kind.supported_efforts(&self.model);
        if !efforts.is_empty() {
            ui.horizontal(|ui| {
                ui.label(catalog.t("agent_launcher.effort", &[]));
                let default_label = kind.default_effort(&self.model).map_or_else(
                    || catalog.t("agent_launcher.effort.default", &[]),
                    |effort| {
                        let effort = catalog.t(effort_message_key(effort), &[]);
                        catalog.t(
                            "agent_launcher.effort.default_value",
                            &[("effort", effort.as_str())],
                        )
                    },
                );
                let selected = self.effort.map_or_else(
                    || default_label.clone(),
                    |effort| catalog.t(effort_message_key(effort), &[]),
                );
                ui.add_enabled_ui(!self.launch_pending, |ui| {
                    egui::ComboBox::from_id_salt("agent-launcher-effort")
                        .selected_text(selected)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(self.effort.is_none(), default_label)
                                .clicked()
                            {
                                self.effort = None;
                            }
                            for effort in efforts {
                                if ui
                                    .selectable_label(
                                        self.effort == Some(*effort),
                                        catalog.t(effort_message_key(*effort), &[]),
                                    )
                                    .clicked()
                                {
                                    self.effort = Some(*effort);
                                }
                            }
                        });
                });
            });
        }

        let supports_yolo = kind.supports_yolo();
        if !supports_yolo {
            self.yolo = false;
        }
        ui.add_enabled(
            supports_yolo && !self.launch_pending,
            egui::Checkbox::new(&mut self.yolo, catalog.t("agent_launcher.yolo", &[])),
        );
        if supports_yolo {
            ui.weak(catalog.t("agent_launcher.yolo_hint", &[]));
        } else {
            ui.weak(catalog.t("agent_launcher.yolo_unsupported", &[]));
        }
        if self.yolo {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                catalog.t("agent_launcher.yolo_warning", &[]),
            );
        }
    }

    fn reconcile_selection(&mut self, snapshot: Option<&DetectionSnapshot>) {
        let Some(snapshot) = snapshot else {
            return;
        };
        let previous_selected = self.selected;
        if self
            .selected
            .is_some_and(|selected| snapshot.find(selected).is_none())
        {
            self.selected = None;
        }
        if self.selected.is_none() {
            self.selected = snapshot.agents().first().map(|agent| agent.kind());
        }
        if self.selected != previous_selected {
            self.model.clear();
        }
        if let Some(kind) = self.selected {
            self.reconcile_model(kind);
            self.reconcile_effort(kind);
            if !kind.supports_yolo() {
                self.yolo = false;
            }
        }
    }

    fn select(&mut self, kind: AgentKind) {
        if self.selected != Some(kind) {
            self.model.clear();
        }
        self.selected = Some(kind);
        self.error = None;
        self.reconcile_model(kind);
        self.reconcile_effort(kind);
        if !kind.supports_yolo() {
            self.yolo = false;
        }
    }

    fn reconcile_model(&mut self, kind: AgentKind) {
        if !self.model.is_empty()
            && !kind
                .supported_models()
                .iter()
                .any(|choice| choice.value() == self.model)
        {
            self.model.clear();
        }
    }

    fn reconcile_effort(&mut self, kind: AgentKind) {
        if self
            .effort
            .is_some_and(|effort| !kind.supported_efforts(&self.model).contains(&effort))
        {
            self.effort = None;
        }
    }
}

impl Default for AgentLauncherUi {
    fn default() -> Self {
        Self::new()
    }
}

fn agent_card(ui: &mut egui::Ui, kind: AgentKind, selected: bool) -> egui::Response {
    let width = ui.available_width().max(240.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 58.0), egui::Sense::click());
    let visuals = ui.visuals();
    let fill = if selected {
        visuals.selection.bg_fill.gamma_multiply(0.16)
    } else if response.hovered() {
        visuals.widgets.hovered.weak_bg_fill
    } else {
        visuals.widgets.inactive.weak_bg_fill
    };
    let stroke = if selected {
        egui::Stroke::new(1.25, visuals.selection.bg_fill)
    } else {
        visuals.widgets.inactive.bg_stroke
    };
    ui.painter().rect_filled(rect, 5.0, fill);
    ui.painter()
        .rect_stroke(rect, 5.0, stroke, egui::StrokeKind::Inside);

    let badge_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 29.0, rect.center().y),
        egui::vec2(36.0, 36.0),
    );
    let (red, green, blue) = kind.badge_color();
    ui.painter()
        .rect_filled(badge_rect, 8.0, egui::Color32::from_rgb(red, green, blue));
    ui.painter().text(
        badge_rect.center(),
        egui::Align2::CENTER_CENTER,
        kind.badge(),
        egui::FontId::proportional(11.0),
        egui::Color32::WHITE,
    );
    ui.painter().text(
        egui::pos2(rect.left() + 57.0, rect.center().y - 8.0),
        egui::Align2::LEFT_CENTER,
        kind.label(),
        egui::FontId::proportional(15.0),
        visuals.text_color(),
    );
    ui.painter().text(
        egui::pos2(rect.left() + 57.0, rect.center().y + 11.0),
        egui::Align2::LEFT_CENTER,
        kind.id(),
        egui::FontId::monospace(11.0),
        visuals.weak_text_color(),
    );
    response
}

const fn effort_message_key(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Low => "agent_launcher.effort.low",
        ReasoningEffort::Medium => "agent_launcher.effort.medium",
        ReasoningEffort::High => "agent_launcher.effort.high",
        ReasoningEffort::XHigh => "agent_launcher.effort.xhigh",
        ReasoningEffort::Max => "agent_launcher.effort.max",
        ReasoningEffort::Ultra => "agent_launcher.effort.ultra",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn snapshot(kinds: &[AgentKind]) -> DetectionSnapshot {
        DetectionSnapshot::from_test_agents(
            kinds
                .iter()
                .copied()
                .map(|kind| (kind, PathBuf::from(format!("/tmp/{}", kind.id())))),
        )
    }

    #[test]
    fn opening_a_launcher_resets_dangerous_and_per_launch_options() {
        let mut ui = AgentLauncherUi::new();
        ui.yolo = true;
        ui.model = "old-model".to_owned();
        ui.effort = Some(ReasoningEffort::XHigh);
        ui.open_for("workspace".to_owned(), "Project".to_owned());
        assert!(ui.open);
        assert!(!ui.yolo);
        assert!(ui.model.is_empty());
        assert!(ui.effort.is_none());
    }

    #[test]
    fn selection_follows_the_detected_bounded_snapshot() {
        let mut ui = AgentLauncherUi::new();
        ui.open_for("workspace".to_owned(), "Project".to_owned());
        let first = snapshot(&[AgentKind::Codex, AgentKind::Claude]);
        ui.reconcile_selection(Some(&first));
        assert_eq!(ui.selected, Some(AgentKind::Codex));
        ui.yolo = true;
        let second = snapshot(&[AgentKind::OpenCode]);
        ui.reconcile_selection(Some(&second));
        assert_eq!(ui.selected, Some(AgentKind::OpenCode));
        assert!(!ui.yolo);
    }

    #[test]
    fn changing_provider_clears_an_incompatible_model_choice() {
        let mut ui = AgentLauncherUi::new();
        ui.select(AgentKind::Claude);
        ui.model = "sonnet".to_owned();
        ui.select(AgentKind::Codex);
        assert!(ui.model.is_empty());

        ui.model = "gpt-5.6-sol".to_owned();
        ui.select(AgentKind::Codex);
        assert_eq!(ui.model, "gpt-5.6-sol");
    }

    #[test]
    fn changing_codex_model_clears_an_unsupported_reasoning_effort() {
        let mut ui = AgentLauncherUi::new();
        ui.select(AgentKind::Codex);
        ui.model = "gpt-5.6-sol".to_owned();
        ui.effort = Some(ReasoningEffort::Ultra);
        ui.reconcile_effort(AgentKind::Codex);
        assert_eq!(ui.effort, Some(ReasoningEffort::Ultra));

        ui.model = "gpt-5.6-luna".to_owned();
        ui.reconcile_effort(AgentKind::Codex);
        assert!(ui.effort.is_none());

        ui.model = "gpt-5.6-luna".to_owned();
        ui.effort = Some(ReasoningEffort::Max);
        ui.reconcile_effort(AgentKind::Codex);
        assert_eq!(ui.effort, Some(ReasoningEffort::Max));
    }
}
