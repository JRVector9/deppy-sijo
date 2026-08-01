use crate::agent_launcher::{
    AgentKind, DetectedAgent, DetectionSnapshot, LaunchOptions, ModelChoice, ReasoningEffort,
};

/// 감지 스냅샷에서 이 종류의 모델 목록을 꺼낸다. 아직 감지 전이면 빈 목록이다.
fn models_for(snapshot: Option<&DetectionSnapshot>, kind: AgentKind) -> &[ModelChoice] {
    snapshot
        .and_then(|snapshot| snapshot.find(kind))
        .map_or(&[], DetectedAgent::models)
}

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
                    // 설치 목록은 화면이 허용하는 만큼 늘어나고, 그래도 모자랄 때만
                    // 스크롤한다. 고정 상한을 두면 에이전트가 몇 개든 항상 잘린다.
                    egui::ScrollArea::vertical()
                        .id_salt("agent-launcher-installed")
                        .max_height(installed_list_max_height(ctx))
                        .show(ui, |ui| {
                            for agent in snapshot.agents() {
                                let kind = agent.kind();
                                if agent_card(ui, kind, self.selected == Some(kind)).clicked()
                                    && !self.launch_pending
                                {
                                    self.select(agent);
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
                self.render_options(ui, kind, models_for(snapshot, kind), catalog);
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

    fn render_options(
        &mut self,
        ui: &mut egui::Ui,
        kind: AgentKind,
        models: &[ModelChoice],
        catalog: &i18n::Catalog,
    ) {
        if !models.is_empty() {
            ui.horizontal(|ui| {
                ui.label(catalog.t("agent_launcher.model", &[]));
                let selected = crate::agent_launcher::find_model(models, &self.model)
                    .map(ModelChoice::label)
                    .unwrap_or_default();
                ui.add_enabled_ui(!self.launch_pending, |ui| {
                    egui::ComboBox::from_id_salt(("agent-launcher-model", kind.id()))
                        .selected_text(selected)
                        .width(360.0)
                        .height(combo_popup_height(ui))
                        .show_ui(ui, |ui| {
                            for choice in models {
                                if ui
                                    .selectable_label(self.model == choice.value(), choice.label())
                                    .clicked()
                                {
                                    self.model = choice.value().to_owned();
                                    self.reconcile_effort(models);
                                }
                            }
                        });
                });
            });
        }

        // 모델 개념이 없는 에이전트(models가 비어 있음)는 강도도 제시하지 않는다.
        let efforts = crate::agent_launcher::find_model(models, &self.model)
            .map_or(&[][..], ModelChoice::efforts);
        if !efforts.is_empty() {
            let row_label = if ReasoningEffort::is_thinking_toggle(efforts) {
                "agent_launcher.thinking"
            } else {
                "agent_launcher.effort"
            };
            ui.horizontal(|ui| {
                ui.label(catalog.t(row_label, &[]));
                let selected = self
                    .effort
                    .map(|effort| catalog.t(effort_message_key(effort), &[]))
                    .unwrap_or_default();
                ui.add_enabled_ui(!self.launch_pending, |ui| {
                    egui::ComboBox::from_id_salt("agent-launcher-effort")
                        .selected_text(selected)
                        .height(combo_popup_height(ui))
                        .show_ui(ui, |ui| {
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
        if let Some(agent) = self.selected.and_then(|kind| snapshot.find(kind)) {
            self.reconcile_options(agent);
        }
    }

    fn select(&mut self, agent: &DetectedAgent) {
        if self.selected != Some(agent.kind()) {
            self.model.clear();
        }
        self.selected = Some(agent.kind());
        self.error = None;
        self.reconcile_options(agent);
    }

    /// 선택된 모델/강도/YOLO가 이 에이전트에서 여전히 유효한지 맞춘다.
    fn reconcile_options(&mut self, agent: &DetectedAgent) {
        self.reconcile_model(agent);
        self.reconcile_effort(agent.models());
        if !agent.kind().supports_yolo() {
            self.yolo = false;
        }
    }

    /// 모델은 "기본 모델" 항목 없이 항상 하나가 선택돼 있다. 선택이 비었거나 이 에이전트가
    /// 더 이상 제공하지 않는 모델이면, CLI가 자기 설정에 적어 둔 기본 모델로 되돌린다.
    /// 그래야 앱으로 띄운 결과가 CLI를 그냥 실행한 것과 같다.
    fn reconcile_model(&mut self, agent: &DetectedAgent) {
        if crate::agent_launcher::find_model(agent.models(), &self.model).is_none() {
            self.model = agent.initial_model().to_owned();
        }
    }

    /// 강도는 "기본값" 항목 없이 항상 하나가 선택돼 있다. 화면에 보이는 값이 곧
    /// 실행에 전달되는 값이므로, 선택이 비었거나 현재 모델이 지원하지 않는 값이면
    /// 카탈로그가 선언한 기본 강도(없으면 첫 단계)로 되돌린다.
    fn reconcile_effort(&mut self, models: &[ModelChoice]) {
        let Some(model) = crate::agent_launcher::find_model(models, &self.model) else {
            self.effort = None;
            return;
        };
        let efforts = model.efforts();
        if efforts.is_empty() {
            self.effort = None;
        } else if self.effort.is_none_or(|effort| !efforts.contains(&effort)) {
            self.effort = model.default_effort().or_else(|| efforts.first().copied());
        }
    }
}

impl Default for AgentLauncherUi {
    fn default() -> Self {
        Self::new()
    }
}

/// 모달의 나머지 요소(제목·옵션·버튼)가 쓰는 세로 공간을 뺀 나머지를 설치 목록에 준다.
/// 그래서 창이 충분히 크면 감지된 에이전트가 전부 보이고 스크롤이 아예 생기지 않는다.
///
/// 드롭다운 몫(`POPUP_CLEARANCE`)을 따로 남긴다. egui는 팝업이 창 안에 완전히 들어가는
/// 배치를 못 찾으면 뒤집지 않고 그냥 아래로 펼친 뒤 창 밖을 잘라내기 때문에, 모달이 창
/// 높이를 다 쓰면 옵션 콤보박스의 목록이 잘린다.
fn installed_list_max_height(ctx: &egui::Context) -> f32 {
    const MODAL_CHROME_HEIGHT: f32 = 330.0;
    const POPUP_CLEARANCE: f32 = 200.0;
    const MIN_LIST_HEIGHT: f32 = 132.0;
    (ctx.viewport_rect().height() - MODAL_CHROME_HEIGHT - POPUP_CLEARANCE).max(MIN_LIST_HEIGHT)
}

/// 드롭다운 팝업이 스크롤 없이 항목을 다 보여주도록 상한을 창 높이까지 연다.
///
/// 지정하지 않으면 egui가 `Spacing::combo_height`(기본 200px)에서 잘라 항목이 몇 개든
/// 스크롤이 생긴다. 행 높이를 직접 계산해 넘기는 방법은 위젯 패딩·글꼴에 따라 어긋나
/// 오히려 더 작게 잡히므로, 계산하지 않고 상한만 창 높이로 둔다. egui의 `ScrollArea`는
/// 내용 크기만큼만 차지하므로, 목록이 창보다 길 때만 스크롤이 남는다.
fn combo_popup_height(ui: &egui::Ui) -> f32 {
    ui.ctx().viewport_rect().height()
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
        ReasoningEffort::On => "agent_launcher.effort.on",
        ReasoningEffort::Off => "agent_launcher.effort.off",
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

    /// 감지된 에이전트. 테스트 스냅샷에는 내장 모델 목록이 실린다.
    fn agent(snapshot: &DetectionSnapshot, kind: AgentKind) -> &DetectedAgent {
        snapshot.find(kind).expect("detected agent")
    }

    #[test]
    fn changing_provider_replaces_an_incompatible_model_with_a_concrete_one() {
        let detected = snapshot(&[AgentKind::Claude, AgentKind::Codex]);
        let codex = agent(&detected, AgentKind::Codex);
        let mut ui = AgentLauncherUi::new();
        ui.select(agent(&detected, AgentKind::Claude));
        ui.model = "sonnet".to_owned();

        // "기본 모델" 항목이 없으므로 비우지 않고 Codex가 실제로 제공하는 모델로 바꾼다.
        ui.select(codex);
        assert_eq!(ui.model, codex.initial_model());
        assert!(crate::agent_launcher::find_model(codex.models(), &ui.model).is_some());

        // 이미 유효한 선택은 그대로 둔다.
        ui.model = "gpt-5.6-sol".to_owned();
        ui.select(codex);
        assert_eq!(ui.model, "gpt-5.6-sol");
    }

    #[test]
    fn changing_codex_model_falls_back_to_the_declared_default_effort() {
        let detected = snapshot(&[AgentKind::Codex]);
        let codex = agent(&detected, AgentKind::Codex);
        let mut ui = AgentLauncherUi::new();
        ui.select(codex);
        ui.model = "gpt-5.6-sol".to_owned();
        ui.effort = Some(ReasoningEffort::Ultra);
        ui.reconcile_effort(codex.models());
        assert_eq!(ui.effort, Some(ReasoningEffort::Ultra));

        // luna는 ultra를 지원하지 않는다. "기본값" 항목이 없으므로 비우는 대신
        // 그 모델이 선언한 기본 강도로 되돌아간다.
        ui.model = "gpt-5.6-luna".to_owned();
        ui.reconcile_effort(codex.models());
        assert_eq!(ui.effort, Some(ReasoningEffort::Medium));

        ui.effort = Some(ReasoningEffort::Max);
        ui.reconcile_effort(codex.models());
        assert_eq!(ui.effort, Some(ReasoningEffort::Max));
    }

    #[test]
    fn a_selected_agent_always_carries_an_explicit_effort() {
        // 화면에 보이는 값이 곧 실행값이 되도록, 강도가 있는 모델은 빈 선택이 없다.
        let detected = snapshot(&[AgentKind::Claude, AgentKind::Kimi]);
        let mut ui = AgentLauncherUi::new();

        ui.select(agent(&detected, AgentKind::Claude));
        ui.model = "opus".to_owned();
        ui.reconcile_effort(agent(&detected, AgentKind::Claude).models());
        assert_eq!(ui.effort, Some(ReasoningEffort::High));

        // 기본 강도를 선언하지 않은 boolean thinking 모델은 첫 항목(켬)을 쓴다.
        let kimi = agent(&detected, AgentKind::Kimi);
        ui.select(kimi);
        ui.model = "kimi-code/kimi-for-coding".to_owned();
        ui.reconcile_effort(kimi.models());
        assert_eq!(ui.effort, Some(ReasoningEffort::On));

        // 강도 개념이 없는 에이전트는 계속 비어 있다.
        let none = snapshot(&[AgentKind::Gemini]);
        ui.select(agent(&none, AgentKind::Gemini));
        assert!(ui.effort.is_none());
    }

    #[test]
    fn a_model_the_detected_catalog_no_longer_offers_is_replaced() {
        // 카탈로그가 동적이므로 CLI가 모델을 내리면 UI의 이전 선택도 풀려야 하는데,
        // "기본 모델" 항목이 없으니 빈 값이 아니라 실제 제공 모델로 대체돼야 한다.
        let detected = snapshot(&[AgentKind::Codex]);
        let codex = agent(&detected, AgentKind::Codex);
        let mut ui = AgentLauncherUi::new();
        ui.model = "gpt-retired".to_owned();
        ui.reconcile_model(codex);
        assert_eq!(ui.model, codex.initial_model());
        assert!(crate::agent_launcher::find_model(codex.models(), &ui.model).is_some());
    }

    #[test]
    fn a_model_supporting_agent_never_launches_without_a_model() {
        // 모델을 가진 에이전트는 항상 구체 모델이 선택돼 있고, 없는 에이전트는 비어 있다.
        let detected = snapshot(&[AgentKind::Claude, AgentKind::Codex, AgentKind::Kimi]);
        for kind in [AgentKind::Claude, AgentKind::Codex, AgentKind::Kimi] {
            let mut ui = AgentLauncherUi::new();
            ui.select(agent(&detected, kind));
            assert!(!ui.model.is_empty(), "{}", kind.id());
        }

        let none = snapshot(&[AgentKind::Gemini]);
        let mut ui = AgentLauncherUi::new();
        ui.select(agent(&none, AgentKind::Gemini));
        assert!(ui.model.is_empty());
    }

    #[test]
    fn thinking_toggle_is_distinguished_from_graded_effort() {
        assert!(ReasoningEffort::is_thinking_toggle(&[
            ReasoningEffort::On,
            ReasoningEffort::Off
        ]));
        assert!(!ReasoningEffort::is_thinking_toggle(&[
            ReasoningEffort::Low,
            ReasoningEffort::High
        ]));
        assert!(!ReasoningEffort::is_thinking_toggle(&[]));
    }
}
