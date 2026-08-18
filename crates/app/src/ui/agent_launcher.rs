use crate::agent_launcher::{
    AgentKind, DetectedAgent, DetectionSnapshot, LaunchOptions, ModelChoice, ReasoningEffort,
};

const LAUNCHER_WIDTH: f32 = 620.0;
const AGENT_PANE_WIDTH: f32 = LAUNCHER_WIDTH / 2.0;
const AGENT_ROW_HEIGHT: f32 = 47.0;
const AGENT_ROW_GAP: f32 = 5.0;
const VISIBLE_AGENT_ROWS: usize = 3;
const CONTROL_HEIGHT: f32 = 34.0;
const ACTION_HEIGHT: f32 = 32.0;

#[derive(Clone, Copy)]
struct LauncherPalette {
    app: egui::Color32,
    options: egui::Color32,
    surface: egui::Color32,
    selected: egui::Color32,
    input: egui::Color32,
    line: egui::Color32,
    control_border: egui::Color32,
    text: egui::Color32,
    muted: egui::Color32,
    accent: egui::Color32,
    warning: egui::Color32,
    error: egui::Color32,
    button: egui::Color32,
    toggle_off: egui::Color32,
}

fn launcher_palette(dark_mode: bool) -> LauncherPalette {
    if dark_mode {
        LauncherPalette {
            app: egui::Color32::from_rgb(0x18, 0x1b, 0x20),
            options: egui::Color32::from_rgb(0x17, 0x1a, 0x1f),
            surface: egui::Color32::from_rgb(0x1d, 0x20, 0x26),
            selected: egui::Color32::from_rgb(0x21, 0x24, 0x2c),
            input: egui::Color32::from_rgb(0x0f, 0x11, 0x15),
            line: egui::Color32::from_rgb(0x32, 0x36, 0x3e),
            control_border: egui::Color32::from_rgb(0x42, 0x47, 0x51),
            text: egui::Color32::from_rgb(0xdc, 0xde, 0xe2),
            muted: egui::Color32::from_rgb(0x92, 0x98, 0xa3),
            accent: egui::Color32::from_rgb(0x39, 0xb8, 0xe8),
            warning: egui::Color32::from_rgb(0xe0, 0xa4, 0x3a),
            error: egui::Color32::from_rgb(0xef, 0x66, 0x71),
            button: egui::Color32::from_rgb(0x20, 0x24, 0x2a),
            toggle_off: egui::Color32::from_rgb(0x3a, 0x3f, 0x48),
        }
    } else {
        LauncherPalette {
            app: egui::Color32::from_rgb(0xf1, 0xf2, 0xf5),
            options: egui::Color32::from_rgb(0xe9, 0xeb, 0xef),
            surface: egui::Color32::from_rgb(0xeb, 0xed, 0xf0),
            selected: egui::Color32::from_rgb(0xe5, 0xe8, 0xec),
            input: egui::Color32::from_rgb(0xfd, 0xfd, 0xfd),
            line: egui::Color32::from_rgb(0xc9, 0xcd, 0xd6),
            control_border: egui::Color32::from_rgb(0xb8, 0xbe, 0xc8),
            text: egui::Color32::from_rgb(0x23, 0x26, 0x2c),
            muted: egui::Color32::from_rgb(0x65, 0x6a, 0x74),
            accent: egui::Color32::from_rgb(0x1c, 0x93, 0xaa),
            warning: egui::Color32::from_rgb(0xb2, 0x70, 0x16),
            error: egui::Color32::from_rgb(0xc8, 0x3d, 0x49),
            button: egui::Color32::from_rgb(0xe5, 0xe8, 0xec),
            toggle_off: egui::Color32::from_rgb(0xb8, 0xbe, 0xc8),
        }
    }
}

fn apply_launcher_style(ui: &mut egui::Ui, palette: LauncherPalette) {
    ui.style_mut()
        .text_styles
        .insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
    ui.style_mut()
        .text_styles
        .insert(egui::TextStyle::Button, egui::FontId::proportional(13.0));
    let visuals = ui.visuals_mut();
    visuals.override_text_color = Some(palette.text);
    visuals.weak_text_color = Some(palette.muted);
    visuals.panel_fill = palette.app;
    visuals.window_fill = palette.app;
    visuals.extreme_bg_color = palette.input;
    visuals.faint_bg_color = palette.surface;
    visuals.selection.bg_fill = palette.accent;
    visuals.selection.stroke = egui::Stroke::new(
        1.0,
        crate::ui::designall::selection_text_color(palette.accent),
    );
    visuals.warn_fg_color = palette.warning;
    visuals.error_fg_color = palette.error;
    visuals.window_stroke = egui::Stroke::new(1.0, palette.control_border);
    visuals.widgets.inactive.bg_fill = palette.input;
    visuals.widgets.inactive.weak_bg_fill = palette.input;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, palette.control_border);
    visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(4);
    visuals.widgets.hovered.bg_fill = palette.surface;
    visuals.widgets.hovered.weak_bg_fill = palette.surface;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, palette.control_border);
    visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(4);
    visuals.widgets.active.bg_fill = palette.selected;
    visuals.widgets.active.weak_bg_fill = palette.selected;
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, palette.accent);
    visuals.widgets.active.corner_radius = egui::CornerRadius::same(4);
    visuals.widgets.open.bg_fill = palette.surface;
    visuals.widgets.open.weak_bg_fill = palette.surface;
    visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0, palette.control_border);
    visuals.widgets.open.corner_radius = egui::CornerRadius::same(4);
}

fn launcher_hairline(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 0.0, color);
}

/// 감지 스냅샷에서 이 종류의 모델 목록을 꺼낸다. 아직 감지 전이면 빈 목록이다.
fn models_for(snapshot: Option<&DetectionSnapshot>, kind: AgentKind) -> &[ModelChoice] {
    snapshot
        .and_then(|snapshot| snapshot.find(kind))
        .map_or(&[], DetectedAgent::models)
}

fn installed_list_height(agent_count: usize) -> f32 {
    let rows = agent_count.min(VISIBLE_AGENT_ROWS);
    if rows == 0 {
        0.0
    } else {
        rows as f32 * AGENT_ROW_HEIGHT + (rows - 1) as f32 * AGENT_ROW_GAP
    }
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
    /// 카드 상시 토글 스위치가 올린다. leaf는 config를 직접 쓰지 않으므로 거부
    /// 목록 갱신·저장은 App(`crates/app/src/app.rs`)이 맡는다.
    SetAgentEnabled {
        kind: AgentKind,
        enabled: bool,
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

    /// 작업 이력의 `새로 실행`이 기존 런처 흐름을 그대로 사용하되,
    /// 클릭한 제공자만 정확히 선택하도록 한다. 설치 스냅샷에 없으면 열지 않는다.
    pub(crate) fn open_for_kind(
        &mut self,
        workspace_id: String,
        workspace_name: String,
        kind: AgentKind,
        snapshot: &DetectionSnapshot,
    ) -> bool {
        let Some(agent) = snapshot.find(kind) else {
            return false;
        };
        self.open_for(workspace_id, workspace_name);
        self.select(agent);
        true
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
        disabled: &[String],
    ) -> Option<AgentLauncherIntent> {
        if !self.open {
            return None;
        }
        self.reconcile_selection(snapshot, disabled);

        let mut intent = None;
        let palette = launcher_palette(ctx.global_style().visuals.dark_mode);
        let modal_frame = egui::Frame::NONE
            .fill(palette.app)
            .stroke(egui::Stroke::new(1.0, palette.control_border))
            .corner_radius(egui::CornerRadius::same(8))
            .shadow(egui::epaint::Shadow {
                offset: [0, 24],
                blur: 60,
                spread: 0,
                color: egui::Color32::from_black_alpha(97),
            });
        // v2 resets the oversized Area geometry cached by the old single-column launcher.
        // The explicit first-pass width also keeps auto-sized modal content from inheriting the
        // viewport width before its content has been measured.
        let modal_id = egui::Id::new("agent-launcher-modal-v2");
        let modal_area = egui::Modal::default_area(modal_id).default_width(LAUNCHER_WIDTH);
        let response = egui::Modal::new(modal_id)
            .area(modal_area)
            .frame(modal_frame)
            .backdrop_color(egui::Color32::from_black_alpha(145))
            .show(ctx, |ui| {
                apply_launcher_style(ui, palette);
                ui.set_width(LAUNCHER_WIDTH);

                egui::Frame::NONE
                    .inner_margin(egui::Margin {
                        left: 18,
                        right: 18,
                        top: 14,
                        bottom: 11,
                    })
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(catalog.t("agent_launcher.title", &[]))
                                    .size(18.0)
                                    .strong(),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let close_label = catalog.t("action.close", &[]);
                                    let close = egui::Button::new(
                                        egui::RichText::new("×").size(18.0).color(palette.muted),
                                    )
                                    .frame(false)
                                    .min_size(egui::vec2(24.0, 24.0));
                                    let close_response = ui
                                        .add_enabled(!self.launch_pending, close)
                                        .on_hover_text(&close_label);
                                    close_response.widget_info(|| {
                                        egui::WidgetInfo::labeled(
                                            egui::WidgetType::Button,
                                            !self.launch_pending,
                                            &close_label,
                                        )
                                    });
                                    if close_response.clicked() {
                                        self.open = false;
                                    }
                                },
                            );
                        });
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            let (dot_rect, _) =
                                ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                            ui.painter().rect_filled(dot_rect, 2.0, palette.accent);
                            ui.label(
                                egui::RichText::new(self.workspace_name.as_str())
                                    .size(13.0)
                                    .strong(),
                            );
                            ui.label(
                                egui::RichText::new(
                                    catalog.t("agent_launcher.subtitle_suffix", &[]),
                                )
                                .size(13.0)
                                .color(palette.muted),
                            );
                        });
                    });

                launcher_hairline(ui, palette.line);

                let list_height = snapshot.map_or(0.0, |snapshot| {
                    installed_list_height(snapshot.agents().len())
                });
                let empty_height = if snapshot.is_some_and(|snapshot| snapshot.agents().is_empty())
                {
                    36.0
                } else {
                    0.0
                };
                let detecting_height = if detecting { 20.0 } else { 0.0 };
                let left_content_height =
                    24.0 + detecting_height + 5.0 + list_height.max(empty_height);

                let (left_rect, right_rect) = ui
                    .scope_builder(
                        egui::UiBuilder::new()
                            .layout(egui::Layout::left_to_right(egui::Align::Min)),
                        |ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            let left = egui::Frame::NONE
                                .inner_margin(egui::Margin {
                                    left: 16,
                                    right: 12,
                                    top: 12,
                                    bottom: 12,
                                })
                                .show(ui, |ui| {
                                    ui.vertical(|ui| {
                                        ui.set_width(AGENT_PANE_WIDTH - 28.0);
                                        if let Some(list_intent) = self.render_agent_list(
                                            ui, snapshot, detecting, catalog, disabled, palette,
                                        ) {
                                            intent = Some(list_intent);
                                        }
                                    });
                                });
                            let right = egui::Frame::NONE
                                .fill(palette.options)
                                .inner_margin(egui::Margin {
                                    left: 16,
                                    right: 16,
                                    top: 12,
                                    bottom: 12,
                                })
                                .show(ui, |ui| {
                                    ui.vertical(|ui| {
                                        ui.set_width(LAUNCHER_WIDTH - AGENT_PANE_WIDTH - 32.0);
                                        ui.set_min_height(left_content_height);
                                        if let Some(kind) = self.selected {
                                            self.render_options(
                                                ui,
                                                kind,
                                                models_for(snapshot, kind),
                                                catalog,
                                                palette,
                                            );
                                        }
                                    });
                                });
                            (left.response.rect, right.response.rect)
                        },
                    )
                    .inner;

                let body_rect = left_rect.union(right_rect);
                ui.painter().vline(
                    crate::ui::snap_line_to_pixel(
                        left_rect.right(),
                        crate::ui::designall::SEPARATOR_WIDTH,
                        ui.ctx().pixels_per_point(),
                    ),
                    body_rect.y_range(),
                    egui::Stroke::new(1.0, palette.line),
                );

                if let Some(error) = self.error {
                    egui::Frame::NONE
                        .inner_margin(egui::Margin::symmetric(16, 6))
                        .show(ui, |ui| {
                            ui.colored_label(palette.error, catalog.t(error.message_key(), &[]));
                        });
                }

                launcher_hairline(ui, palette.line);
                egui::Frame::NONE
                    .fill(palette.options)
                    .inner_margin(egui::Margin::symmetric(16, 8))
                    .show(ui, |ui| {
                        let row_width = ui.available_width();
                        ui.allocate_ui_with_layout(
                            egui::vec2(row_width, 36.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                let blank = egui::Button::new(
                                    egui::RichText::new(
                                        catalog.t("agent_launcher.blank_terminal", &[]),
                                    )
                                    .size(13.0),
                                )
                                .min_size(egui::vec2(96.0, ACTION_HEIGHT))
                                .fill(palette.button)
                                .stroke(egui::Stroke::new(1.0, palette.control_border))
                                .corner_radius(4);
                                if ui
                                    .add_enabled(!self.launch_pending, blank)
                                    .on_hover_text(
                                        catalog.t("agent_launcher.blank_terminal_hint", &[]),
                                    )
                                    .clicked()
                                {
                                    self.open = false;
                                    intent = Some(AgentLauncherIntent::BlankTerminal {
                                        workspace_id: self.workspace_id.clone(),
                                    });
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let launch =
                                            egui::Button::new(
                                                egui::RichText::new(
                                                    catalog.t("agent_launcher.launch", &[]),
                                                )
                                                .size(13.0)
                                                .strong()
                                                .color(crate::ui::designall::selection_text_color(
                                                    palette.accent,
                                                )),
                                            )
                                            .min_size(egui::vec2(116.0, ACTION_HEIGHT))
                                            .fill(palette.accent)
                                            .stroke(egui::Stroke::new(1.0, palette.accent))
                                            .corner_radius(4);
                                        if ui
                                            .add_enabled(
                                                self.selected.is_some() && !self.launch_pending,
                                                launch,
                                            )
                                            .clicked()
                                            && let Some(launch_intent) = self.start_launch()
                                        {
                                            intent = Some(launch_intent);
                                        }
                                        if self.launch_pending {
                                            ui.spinner();
                                            ui.label(
                                                egui::RichText::new(
                                                    catalog.t("agent_launcher.launching", &[]),
                                                )
                                                .size(11.0)
                                                .color(palette.muted),
                                            );
                                        }
                                    },
                                );
                            },
                        );
                    });
            });

        if response.should_close() && !self.launch_pending {
            self.open = false;
        }
        intent
    }

    fn render_agent_list(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: Option<&DetectionSnapshot>,
        detecting: bool,
        catalog: &i18n::Catalog,
        disabled: &[String],
        palette: LauncherPalette,
    ) -> Option<AgentLauncherIntent> {
        let mut intent = None;
        let agent_count = snapshot.map_or(0, |snapshot| snapshot.agents().len());
        let header_width = ui.available_width();
        ui.allocate_ui_with_layout(
            egui::vec2(header_width, 24.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "{} · {agent_count}",
                        catalog.t("agent_launcher.installed", &[])
                    ))
                    .size(13.0)
                    .strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let refresh = egui::Button::new(
                        egui::RichText::new(format!(
                            "↻ {}",
                            catalog.t("agent_launcher.refresh", &[])
                        ))
                        .size(12.0)
                        .color(ui.visuals().selection.bg_fill),
                    )
                    .frame(false);
                    if ui
                        .add_enabled(!detecting && !self.launch_pending, refresh)
                        .clicked()
                    {
                        intent = Some(AgentLauncherIntent::Refresh);
                    }
                });
            },
        );
        if detecting {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    egui::RichText::new(catalog.t("agent_launcher.detecting", &[]))
                        .size(11.0)
                        .weak(),
                );
            });
        }
        ui.add_space(5.0);

        match snapshot {
            Some(snapshot) if !snapshot.agents().is_empty() => {
                egui::ScrollArea::vertical()
                    .id_salt("agent-launcher-installed")
                    .max_height(installed_list_height(snapshot.agents().len()))
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for (index, agent) in snapshot.agents().iter().enumerate() {
                            let kind = agent.kind();
                            let enabled = crate::agent_launcher::agent_is_enabled(disabled, kind);
                            let card = agent_card(
                                ui,
                                kind,
                                self.selected == Some(kind),
                                enabled,
                                catalog,
                                palette,
                            );
                            // 꺼진 카드는 본문 클릭을 무시한다 — 스위치만 반응한다(B안 계약).
                            if enabled
                                && !self.launch_pending
                                && (card.card.clicked() || card.card.double_clicked())
                            {
                                self.select(agent);
                            }
                            // egui는 더블클릭의 두 번째 릴리즈에서 clicked()도 함께
                            // 발생시킨다. 선택을 먼저 반영한 뒤 기존 단일 시작 경로를 쓴다.
                            if enabled
                                && card.card.double_clicked()
                                && let Some(launch_intent) = self.start_launch()
                            {
                                intent = Some(launch_intent);
                            }
                            if card.toggle.clicked() {
                                let now_enabled = !enabled;
                                intent = Some(AgentLauncherIntent::SetAgentEnabled {
                                    kind,
                                    enabled: now_enabled,
                                });
                                // 켜져 있던 현재 선택을 껐으면 선택을 비운다 — 고른 적
                                // 없는 게 골라져 있으면 다음 실행이 엉뚱해진다.
                                if !now_enabled && self.selected == Some(kind) {
                                    self.selected = None;
                                }
                            }
                            if index + 1 < snapshot.agents().len() {
                                ui.add_space(AGENT_ROW_GAP);
                            }
                        }
                    });
            }
            Some(_) if !detecting => {
                ui.label(catalog.t("agent_launcher.none_detected", &[]));
                ui.weak(catalog.t("agent_launcher.none_detected_hint", &[]));
            }
            _ => {}
        }

        intent
    }

    fn render_options(
        &mut self,
        ui: &mut egui::Ui,
        kind: AgentKind,
        models: &[ModelChoice],
        catalog: &i18n::Catalog,
        palette: LauncherPalette,
    ) {
        ui.spacing_mut().interact_size.y = CONTROL_HEIGHT;
        let mut rendered_field = false;
        if !models.is_empty() {
            ui.label(
                egui::RichText::new(catalog.t("agent_launcher.model", &[]))
                    .size(12.0)
                    .strong(),
            );
            ui.add_space(4.0);
            let selected = crate::agent_launcher::find_model(models, &self.model)
                .map(ModelChoice::label)
                .unwrap_or_default();
            ui.add_enabled_ui(!self.launch_pending, |ui| {
                egui::ComboBox::from_id_salt(("agent-launcher-model", kind.id()))
                    .selected_text(selected)
                    .width(ui.available_width())
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
            rendered_field = true;
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
            if rendered_field {
                ui.add_space(10.0);
            }
            ui.label(
                egui::RichText::new(catalog.t(row_label, &[]))
                    .size(12.0)
                    .strong(),
            );
            ui.add_space(4.0);
            let selected = self
                .effort
                .map(|effort| catalog.t(effort_message_key(effort), &[]))
                .unwrap_or_default();
            ui.add_enabled_ui(!self.launch_pending, |ui| {
                egui::ComboBox::from_id_salt("agent-launcher-effort")
                    .selected_text(selected)
                    .width(ui.available_width())
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
            rendered_field = true;
        }

        let supports_yolo = kind.supports_yolo();
        if !supports_yolo {
            self.yolo = false;
        }
        if rendered_field {
            ui.add_space(12.0);
            launcher_hairline(ui, palette.line);
            ui.add_space(12.0);
        }
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            ui.horizontal_top(|ui| {
                let yolo_label = catalog.t("agent_launcher.yolo", &[]);
                let toggle = ui
                    .add_enabled_ui(supports_yolo && !self.launch_pending, |ui| {
                        launcher_toggle(ui, &mut self.yolo, palette)
                    })
                    .inner;
                toggle.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::WidgetType::Checkbox,
                        ui.is_enabled(),
                        self.yolo,
                        &yolo_label,
                    )
                });
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(yolo_label).size(13.0).strong());
                    ui.add_space(3.0);
                    let (message_key, message_color) = if supports_yolo {
                        ("agent_launcher.yolo_warning", palette.warning)
                    } else {
                        ("agent_launcher.yolo_unsupported", palette.muted)
                    };
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(catalog.t(message_key, &[]))
                                .size(11.0)
                                .color(message_color),
                        )
                        .wrap_mode(egui::TextWrapMode::Wrap),
                    );
                });
            });
        });
    }

    fn reconcile_selection(&mut self, snapshot: Option<&DetectionSnapshot>, disabled: &[String]) {
        let Some(snapshot) = snapshot else {
            return;
        };
        let previous_selected = self.selected;
        // 탐지에서 사라졌거나 그새 꺼진 선택은 비운다 — 꺼진 카드는 골라져 있을 수 없다.
        if self.selected.is_some_and(|selected| {
            snapshot.find(selected).is_none()
                || !crate::agent_launcher::agent_is_enabled(disabled, selected)
        }) {
            self.selected = None;
        }
        if self.selected.is_none() {
            self.selected = snapshot
                .agents()
                .iter()
                .find(|agent| crate::agent_launcher::agent_is_enabled(disabled, agent.kind()))
                .map(|agent| agent.kind());
        }
        if self.selected != previous_selected {
            self.model.clear();
        }
        if let Some(agent) = self.selected.and_then(|kind| snapshot.find(kind)) {
            self.reconcile_options(agent);
        }
    }

    /// 「세션 시작」 버튼과 카드 더블클릭이 공유하는 단 하나의 시작 경로. 선택된
    /// 에이전트가 없거나 이미 시작이 진행 중이면 아무 것도 하지 않는다.
    fn start_launch(&mut self) -> Option<AgentLauncherIntent> {
        if self.launch_pending {
            return None;
        }
        let kind = self.selected?;
        self.launch_pending = true;
        self.error = None;
        Some(AgentLauncherIntent::Launch {
            workspace_id: self.workspace_id.clone(),
            kind,
            options: LaunchOptions {
                model: self.model.clone(),
                effort: self.effort,
                yolo: self.yolo,
            },
        })
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

/// 드롭다운 팝업이 스크롤 없이 항목을 다 보여주도록 상한을 창 높이까지 연다.
///
/// 지정하지 않으면 egui가 `Spacing::combo_height`(기본 200px)에서 잘라 항목이 몇 개든
/// 스크롤이 생긴다. 행 높이를 직접 계산해 넘기는 방법은 위젯 패딩·글꼴에 따라 어긋나
/// 오히려 더 작게 잡히므로, 계산하지 않고 상한만 창 높이로 둔다. egui의 `ScrollArea`는
/// 내용 크기만큼만 차지하므로, 목록이 창보다 길 때만 스크롤이 남는다.
fn combo_popup_height(ui: &egui::Ui) -> f32 {
    ui.ctx().viewport_rect().height()
}

fn launcher_toggle(
    ui: &mut egui::Ui,
    value: &mut bool,
    palette: LauncherPalette,
) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(egui::vec2(31.0, 18.0), egui::Sense::click());
    if response.clicked() {
        *value = !*value;
        response.mark_changed();
    }

    let enabled_alpha = if ui.is_enabled() { 1.0 } else { 0.45 };
    let track = if *value {
        palette.accent
    } else {
        palette.toggle_off
    }
    .gamma_multiply(enabled_alpha);
    let knob = if *value {
        egui::Color32::WHITE
    } else {
        palette.muted
    }
    .gamma_multiply(enabled_alpha);
    ui.painter().rect_filled(rect, 9.0, track);
    let center_x = if *value {
        rect.right() - 9.0
    } else {
        rect.left() + 9.0
    };
    ui.painter()
        .circle_filled(egui::pos2(center_x, rect.center().y), 6.0, knob);
    response
}

/// 카드 본문(선택) 응답과 상시 토글 스위치(켜기/끄기) 응답을 함께 돌려준다.
struct AgentCardResponse {
    card: egui::Response,
    toggle: egui::Response,
}

/// `kind`가 여전히 이 에이전트가 아니라 매 프레임 값으로 넘어오므로, 카드·스위치의
/// egui `Id`는 이 함수 안에서 `kind`로 고유하게 파생한다.
///
/// 스위치는 카드 rect 안의 부분 영역이다. 카드 interact를 **먼저**, 스위치 interact를
/// **나중에** 등록한다 — egui는 겹칠 때 나중에 등록된 위젯이 클릭을 가져가므로, 이
/// 순서가 곧 "스위치 > 카드" 우선순위다(`workspace.rs`의 보조 탭 닫기 버튼이 세션
/// 헤더보다 나중에 등록되는 것과 같은 트릭).
fn agent_card(
    ui: &mut egui::Ui,
    kind: AgentKind,
    selected: bool,
    enabled: bool,
    catalog: &i18n::Catalog,
    palette: LauncherPalette,
) -> AgentCardResponse {
    let tokens = crate::ui::designall::tokens(ui.visuals());
    let width = ui.available_width().max(240.0);
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, AGENT_ROW_HEIGHT), egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, enabled, selected, kind.label())
    });

    let visuals = ui.visuals();
    let fill = if selected {
        palette.selected
    } else if response.hovered() && enabled {
        palette.surface
    } else {
        egui::Color32::TRANSPARENT
    };
    let stroke = if selected {
        egui::Stroke::new(1.0, palette.accent)
    } else {
        egui::Stroke::NONE
    };
    ui.painter().rect_filled(rect, 5.0, fill);
    ui.painter()
        .rect_stroke(rect, 5.0, stroke, egui::StrokeKind::Inside);

    // 꺼진 카드는 흐리게 — `launcher_toggle`이 egui 비활성 상태에 쓰는 것과 같은
    // 배수(0.45)로 배지·글자를 낮춘다.
    let alpha = if enabled { 1.0 } else { 0.45 };
    let badge_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 23.0, rect.center().y),
        egui::vec2(30.0, 30.0),
    );
    let (red, green, blue) = kind.badge_color();
    ui.painter().rect_filled(
        badge_rect,
        7.0,
        egui::Color32::from_rgb(red, green, blue).gamma_multiply(alpha),
    );
    ui.painter().text(
        badge_rect.center(),
        egui::Align2::CENTER_CENTER,
        kind.badge(),
        egui::FontId::proportional(10.0),
        egui::Color32::WHITE.gamma_multiply(alpha),
    );
    let label_color = if enabled {
        visuals.text_color()
    } else {
        tokens.muted_text
    };
    ui.painter().text(
        egui::pos2(rect.left() + 47.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        kind.label(),
        egui::FontId::proportional(14.0),
        label_color,
    );

    // 스위치는 오른쪽 끝에 고정한다. 상태 표시(선택 체크·꺼짐 배지)는 스위치
    // 왼쪽에 오른쪽 정렬로 붙여, 로케일마다 길이가 달라도 스위치와 겹치지 않는다.
    let switch_size = egui::vec2(31.0, 18.0);
    let switch_rect = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 12.0 - switch_size.x / 2.0, rect.center().y),
        switch_size,
    );
    let status_anchor = egui::pos2(switch_rect.left() - 8.0, rect.center().y);
    if selected {
        ui.painter().text(
            status_anchor,
            egui::Align2::RIGHT_CENTER,
            "✓",
            egui::FontId::proportional(13.0),
            palette.accent,
        );
    } else if !enabled {
        ui.painter().text(
            status_anchor,
            egui::Align2::RIGHT_CENTER,
            catalog.t("launcher.agent.disabled_badge", &[]),
            egui::FontId::proportional(10.0),
            tokens.muted_text,
        );
    }

    // 카드보다 나중에 등록 — 겹치는 영역의 클릭은 스위치가 가져간다.
    let toggle_id = egui::Id::new(("agent-launcher-card-toggle", kind));
    let toggle_response = ui.interact(switch_rect, toggle_id, egui::Sense::click());
    let hint_key = if enabled {
        "launcher.agent.enabled_hint"
    } else {
        "launcher.agent.disabled_hint"
    };
    let hint = catalog.t(hint_key, &[]);
    let track = if enabled {
        tokens.accent
    } else {
        tokens.separator
    };
    let knob = if enabled {
        egui::Color32::WHITE
    } else {
        tokens.muted_text
    };
    ui.painter().rect_filled(switch_rect, 9.0, track);
    let knob_center_x = if enabled {
        switch_rect.right() - 9.0
    } else {
        switch_rect.left() + 9.0
    };
    ui.painter()
        .circle_filled(egui::pos2(knob_center_x, switch_rect.center().y), 6.0, knob);
    let toggle_response = toggle_response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(&hint);
    toggle_response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, enabled, &hint)
    });

    AgentCardResponse {
        card: response,
        toggle: toggle_response,
    }
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
    fn history_new_run_preselects_only_an_installed_provider() {
        let detected = snapshot(&[AgentKind::Claude, AgentKind::Kimi]);
        let mut ui = AgentLauncherUi::new();

        assert!(ui.open_for_kind(
            "workspace".to_owned(),
            "Project".to_owned(),
            AgentKind::Kimi,
            &detected,
        ));
        assert_eq!(ui.selected, Some(AgentKind::Kimi));
        assert!(!ui.open_for_kind(
            "workspace".to_owned(),
            "Project".to_owned(),
            AgentKind::Codex,
            &detected,
        ));
        assert_eq!(ui.selected, Some(AgentKind::Kimi));
    }

    #[test]
    fn selection_follows_the_detected_bounded_snapshot() {
        let mut ui = AgentLauncherUi::new();
        ui.open_for("workspace".to_owned(), "Project".to_owned());
        let first = snapshot(&[AgentKind::Codex, AgentKind::Claude]);
        ui.reconcile_selection(Some(&first), &[]);
        assert_eq!(ui.selected, Some(AgentKind::Codex));
        ui.yolo = true;
        let second = snapshot(&[AgentKind::OpenCode]);
        ui.reconcile_selection(Some(&second), &[]);
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

    #[test]
    fn start_launch_without_a_selection_produces_no_intent() {
        // 카드가 하나도 선택되지 않았으면 더블클릭이든 버튼이든 시작할 게 없다.
        let mut ui = AgentLauncherUi::new();
        assert!(ui.start_launch().is_none());
        assert!(!ui.launch_pending);
    }

    #[test]
    fn start_launch_carries_the_selected_agent_and_current_options() {
        // 「세션 시작」 버튼과 카드 더블클릭이 공유하는 경로이므로, 선택된 에이전트와
        // 화면에 보이던 모델/강도/YOLO가 그대로 인텐트에 실려야 한다.
        let detected = snapshot(&[AgentKind::Codex]);
        let mut ui = AgentLauncherUi::new();
        ui.open_for("workspace-1".to_owned(), "Project".to_owned());
        ui.select(agent(&detected, AgentKind::Codex));
        ui.yolo = true;

        let intent = ui.start_launch().expect("selected agent should launch");
        assert!(ui.launch_pending);
        match intent {
            AgentLauncherIntent::Launch {
                workspace_id,
                kind,
                options,
            } => {
                assert_eq!(workspace_id, "workspace-1");
                assert_eq!(kind, AgentKind::Codex);
                assert_eq!(options.model, ui.model);
                assert_eq!(options.effort, ui.effort);
                assert!(options.yolo);
            }
            _ => panic!("expected a Launch intent"),
        }
    }

    #[test]
    fn start_launch_is_a_no_op_while_a_launch_is_already_pending() {
        // 중복 실행 금지: 시작이 진행 중이면 버튼이든 더블클릭이든 다시 시작하지 않는다.
        let detected = snapshot(&[AgentKind::Codex]);
        let mut ui = AgentLauncherUi::new();
        ui.select(agent(&detected, AgentKind::Codex));
        assert!(ui.start_launch().is_some());
        assert!(ui.start_launch().is_none());
    }

    #[test]
    fn 선택_재조정은_꺼진_에이전트를_건너뛴다() {
        // 꺼진 에이전트는 자동 선택 후보에서 빠진다 — 아니면 끈 바로 다음 프레임에
        // 다시 골라져 "선택 비우기"가 무의미해진다.
        let detected = snapshot(&[AgentKind::Claude, AgentKind::Kimi]);
        let mut ui = AgentLauncherUi::new();
        ui.reconcile_selection(Some(&detected), &["claude".to_owned()]);
        assert_eq!(ui.selected, Some(AgentKind::Kimi));

        // 선택돼 있던 에이전트가 그새 꺼졌으면 비워지고 남은 켜진 것으로 옮겨간다.
        ui.selected = Some(AgentKind::Kimi);
        ui.reconcile_selection(Some(&detected), &["kimi".to_owned()]);
        assert_eq!(ui.selected, Some(AgentKind::Claude));

        // 전부 꺼졌으면 고를 게 없다 — 빈 채로 둔다(막지 않는다, B안 동작 4).
        ui.selected = None;
        ui.reconcile_selection(Some(&detected), &["claude".to_owned(), "kimi".to_owned()]);
        assert_eq!(ui.selected, None);
    }

    /// `render_agent_list`가 렌더 중 올리는 인텐트와, 그 렌더가 직접 건드린 선택
    /// 상태를 함께 들여다보기 위한 캡처. 카드가 목록 전체를 훑어야 스위치 우선순위를
    /// 실제 클릭으로 검증할 수 있어, `render_card`(`work_history.rs`)처럼 함수
    /// 하나만 감싸지 않고 목록 렌더를 통째로 감싼다.
    struct AgentListCapture {
        ui: AgentLauncherUi,
        intents: Vec<AgentLauncherIntent>,
    }

    fn agent_list_harness<'a>(
        catalog: &'a i18n::Catalog,
        snapshot: &'a DetectionSnapshot,
        disabled: Vec<String>,
        initial_selected: Option<AgentKind>,
    ) -> egui_kittest::Harness<'a, AgentListCapture> {
        let mut initial_ui = AgentLauncherUi::new();
        initial_ui.selected = initial_selected;
        let palette = launcher_palette(false);
        egui_kittest::Harness::new_ui_state(
            move |ui, capture: &mut AgentListCapture| {
                if let Some(intent) = capture.ui.render_agent_list(
                    ui,
                    Some(snapshot),
                    false,
                    catalog,
                    disabled.as_slice(),
                    palette,
                ) {
                    capture.intents.push(intent);
                }
            },
            AgentListCapture {
                ui: initial_ui,
                intents: Vec::new(),
            },
        )
    }

    /// 한 지점에서 좌클릭 press→release를 흘려보내고 프레임을 한 번 더 처리한다.
    /// `kittest_card_background_click_only_toggles_card`(`work_history.rs`)와 같은
    /// 이벤트 시퀀스를 네 테스트가 공유하기 위한 헬퍼.
    fn click_at(harness: &mut egui_kittest::Harness<'_, AgentListCapture>, pos: egui::Pos2) {
        harness.hover_at(pos);
        for pressed in [true, false] {
            harness.event(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
        }
        harness.run();
    }

    #[test]
    fn kittest_꺼진_카드_본문_클릭은_선택을_바꾸지_않는다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let detected = snapshot(&[AgentKind::Kimi]);
        let mut harness = agent_list_harness(&catalog, &detected, vec!["kimi".to_owned()], None);

        let card =
            harness.get_by_role_and_label(egui::accesskit::Role::Button, AgentKind::Kimi.label());
        // 배지 쪽(카드 왼쪽) — 스위치 히트박스 밖이라 본문 클릭 판정을 겨냥한다.
        let click_pos = card.rect().left_top() + egui::vec2(6.0, 6.0);

        click_at(&mut harness, click_pos);

        assert_eq!(harness.state().ui.selected, None);
        assert!(harness.state().intents.is_empty());
    }

    #[test]
    fn kittest_스위치_클릭은_양방향으로_set_agent_enabled를_올린다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let enabled_hint = catalog.t("launcher.agent.enabled_hint", &[]);
        let disabled_hint = catalog.t("launcher.agent.disabled_hint", &[]);
        let detected = snapshot(&[AgentKind::Kimi]);

        // 켜짐 → 꺼짐
        let mut harness = agent_list_harness(&catalog, &detected, Vec::new(), None);
        let switch =
            harness.get_by_role_and_label(egui::accesskit::Role::CheckBox, &enabled_hint);
        let pos = switch.rect().center();
        click_at(&mut harness, pos);
        match harness.state().intents.as_slice() {
            [AgentLauncherIntent::SetAgentEnabled { kind, enabled }] => {
                assert_eq!(*kind, AgentKind::Kimi);
                assert!(!enabled);
            }
            _ => panic!("expected exactly one SetAgentEnabled(false) intent"),
        }

        // 꺼짐 → 켜짐
        let mut harness =
            agent_list_harness(&catalog, &detected, vec!["kimi".to_owned()], None);
        let switch =
            harness.get_by_role_and_label(egui::accesskit::Role::CheckBox, &disabled_hint);
        let pos = switch.rect().center();
        click_at(&mut harness, pos);
        match harness.state().intents.as_slice() {
            [AgentLauncherIntent::SetAgentEnabled { kind, enabled }] => {
                assert_eq!(*kind, AgentKind::Kimi);
                assert!(enabled);
            }
            _ => panic!("expected exactly one SetAgentEnabled(true) intent"),
        }
    }

    #[test]
    fn kittest_현재_선택을_끄면_선택이_비워진다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let enabled_hint = catalog.t("launcher.agent.enabled_hint", &[]);
        let detected = snapshot(&[AgentKind::Kimi]);
        let mut harness =
            agent_list_harness(&catalog, &detected, Vec::new(), Some(AgentKind::Kimi));
        assert_eq!(harness.state().ui.selected, Some(AgentKind::Kimi));

        let switch =
            harness.get_by_role_and_label(egui::accesskit::Role::CheckBox, &enabled_hint);
        let pos = switch.rect().center();
        click_at(&mut harness, pos);

        assert_eq!(harness.state().ui.selected, None);
        match harness.state().intents.as_slice() {
            [AgentLauncherIntent::SetAgentEnabled { kind, enabled }] => {
                assert_eq!(*kind, AgentKind::Kimi);
                assert!(!enabled);
            }
            _ => panic!("expected exactly one SetAgentEnabled(false) intent"),
        }
    }

    #[test]
    fn kittest_다른_카드의_스위치를_꺼도_기존_선택은_유지된다() {
        use egui_kittest::kittest::Queryable;

        // 스위치 클릭이 카드 본문 클릭도 함께 발화시키는 회귀가 생기면, 꺼지는 카드가
        // `select(kind)`로 먼저 선택됐다가 곧바로 토글 분기가 그 선택을 지운다. 그런데
        // 위 두 테스트 — (선택 안 된 카드를 끄기) · (지금 선택된 카드를 끄기) — 는 둘 다
        // 최종 상태가 우연히 일치해 이 회귀를 못 잡는다. 여기서는 A가 선택된 채로
        // **다른, 켜져 있는** B의 스위치를 꺼서 — 우선순위가 깨지면 B가 잠깐 선택됐다가
        // 토글 분기가 그걸 지워 `selected`가 `None`이 되고, 우선순위가 정상이면 A 선택은
        // 전혀 손대지 않는다.
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let detected = snapshot(&[AgentKind::Claude, AgentKind::Codex]);
        let mut harness =
            agent_list_harness(&catalog, &detected, Vec::new(), Some(AgentKind::Claude));

        let card_b = harness
            .get_by_role_and_label(egui::accesskit::Role::Button, AgentKind::Codex.label());
        // 스위치 위치는 `agent_card`의 `switch_rect` 계산과 같다 — 카드 오른쪽 끝에서
        // 12px 안쪽, 폭 31px 스위치의 중심.
        let pos = egui::pos2(
            card_b.rect().right() - 12.0 - 31.0 / 2.0,
            card_b.rect().center().y,
        );

        click_at(&mut harness, pos);

        assert_eq!(
            harness.state().ui.selected,
            Some(AgentKind::Claude),
            "다른 카드의 스위치를 꺼도 기존 선택(A)이 유지돼야 한다"
        );
        match harness.state().intents.as_slice() {
            [AgentLauncherIntent::SetAgentEnabled { kind, enabled }] => {
                assert_eq!(*kind, AgentKind::Codex);
                assert!(!enabled);
            }
            _ => panic!("expected exactly one SetAgentEnabled(false) intent"),
        }
    }

    #[test]
    fn kittest_꺼진_카드는_목록에_남는다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let detected = snapshot(&[AgentKind::Claude, AgentKind::Kimi]);
        let harness = agent_list_harness(&catalog, &detected, vec!["kimi".to_owned()], None);

        // B안 계약 — 꺼진 카드는 숨지 않고 목록에 남는다.
        assert!(
            harness
                .query_by_role_and_label(egui::accesskit::Role::Button, AgentKind::Kimi.label())
                .is_some(),
            "disabled card must still be rendered"
        );
    }
}
