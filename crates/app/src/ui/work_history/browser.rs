//! Compact history browser. All reads/activation remain durable intents handled by App.
use super::*;

mod thread;

const ROW_HEIGHT: f32 = 68.0;
const NARROW_WIDTH: f32 = 760.0;
// Sizes are local to history and follow work-history-proposal-2026-10-08.html.
const BODY_FONT: f32 = 14.0;
const META_FONT: f32 = 12.0;
const CONTROL_HEIGHT: f32 = 37.0;
const COMPACT_CONTROL_HEIGHT: f32 = 26.0;
const LIST_PADDING: f32 = 20.0;

fn browser_tokens(visuals: &egui::Visuals) -> crate::ui::designall::Tokens {
    let mut tokens = crate::ui::designall::tokens(visuals);
    if visuals.dark_mode {
        tokens.content_canvas = egui::Color32::from_rgb(0x11, 0x14, 0x18);
        tokens.workspace_background = egui::Color32::from_rgb(0x18, 0x1c, 0x22);
        tokens.input_background = egui::Color32::from_rgb(0x20, 0x26, 0x2e);
        tokens.separator = egui::Color32::from_rgb(0x30, 0x36, 0x40);
        tokens.text = egui::Color32::from_rgb(0xdc, 0xe0, 0xe6);
        tokens.muted_text = egui::Color32::from_rgb(0x92, 0x9c, 0xa9);
        tokens.accent = egui::Color32::from_rgb(0x22, 0xbf, 0xd0);
        tokens.selected_background = egui::Color32::from_rgb(0x1c, 0x30, 0x3a);
        tokens.hover_background = egui::Color32::from_rgb(0x2b, 0x33, 0x3d);
        tokens.success = egui::Color32::from_rgb(0x42, 0xc7, 0xa2);
        tokens.warning = egui::Color32::from_rgb(0xe6, 0xae, 0x56);
    }
    tokens
}

fn browser_style(ui: &mut egui::Ui) {
    let tokens = browser_tokens(ui.visuals());
    let style = ui.style_mut();
    style.override_font_id = None;
    style.override_text_style = None;
    for (name, size) in [
        (egui::TextStyle::Body, BODY_FONT),
        (egui::TextStyle::Button, BODY_FONT),
        (egui::TextStyle::Small, META_FONT),
        (egui::TextStyle::Heading, 21.0),
    ] {
        style
            .text_styles
            .insert(name, egui::FontId::proportional(size));
    }
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    style.spacing.interact_size.y = CONTROL_HEIGHT;
    style.visuals.override_text_color = Some(tokens.text);
    style.visuals.weak_text_color = Some(tokens.muted_text);
    style.visuals.text_edit_bg_color = Some(tokens.input_background);
    style.visuals.selection.bg_fill = tokens.selected_background;
    style.visuals.selection.stroke = egui::Stroke::new(1.0, tokens.accent);
    for widget in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.open,
    ] {
        widget.corner_radius = egui::CornerRadius::same(5);
        widget.bg_stroke = egui::Stroke::new(1.0, tokens.separator);
        widget.fg_stroke = egui::Stroke::new(1.0, tokens.text);
    }
    style.visuals.widgets.inactive.bg_fill = tokens.input_background;
    style.visuals.widgets.inactive.weak_bg_fill = tokens.input_background;
    style.visuals.widgets.hovered.bg_fill = tokens.hover_background;
    style.visuals.widgets.hovered.weak_bg_fill = tokens.hover_background;
}

fn compact_controls(ui: &mut egui::Ui) {
    ui.spacing_mut().button_padding = egui::vec2(8.0, 2.0);
    ui.spacing_mut().interact_size.y = COMPACT_CONTROL_HEIGHT;
    ui.style_mut().text_styles.insert(
        egui::TextStyle::Button,
        egui::FontId::proportional(META_FONT),
    );
}

#[derive(Default)]
pub(super) struct BrowserState {
    pane: Option<String>,
    today_only: bool,
    cards: bool,
    timeline: bool,
    project: Option<String>,
    more_filters: bool,
    conversation: bool,
    mobile_detail: bool,
    requested: Option<WorkTurnIdentity>,
    card_indices: Vec<usize>,
}

impl WorkHistoryUi {
    fn browser_indices(
        &self,
        snapshot: &WorkHistorySnapshot<'_>,
        filter: &str,
        now: i64,
    ) -> Vec<usize> {
        let query = self.query.trim().to_lowercase();
        let today = local_date(now);
        let mut indices: Vec<_> = (0..snapshot.rows.len())
            .filter(|&i| {
                let row = &snapshot.rows[i];
                (self.filter == WorkHistoryFilter::All
                    || row.current_state.is_some_and(|s| self.filter.matches(s)))
                    && (self.providers.is_empty()
                        || self.providers.iter().any(|p| p.matches(row.kind)))
                    && (query.is_empty() || row_matches_query(row, &query))
                    && (filter.is_empty() || row_matches_aux_filter(row, filter))
                    && self
                        .browser
                        .pane
                        .as_deref()
                        .is_none_or(|pane| pane == row.pane_id)
                    && self.browser.project.as_deref().is_none_or(|cwd| {
                        row.cwd.or_else(|| thread::context(snapshot.rows, row).cwd) == Some(cwd)
                    })
                    && (!self.browser.today_only || local_date(started_at(row)) == today)
            })
            .collect();
        indices.sort_by(|&a, &b| {
            let left = &snapshot.rows[a];
            let right = &snapshot.rows[b];
            let priority = |row: &WorkHistoryRow<'_>| match row.current_state {
                Some(WorkHistoryState::Waiting) => 0,
                Some(WorkHistoryState::Working) => 1,
                Some(WorkHistoryState::Completed) => 2,
                None => 3,
            };
            let status = if self.sort_mode == WorkHistorySortMode::StateFirst {
                priority(left).cmp(&priority(right))
            } else {
                std::cmp::Ordering::Equal
            };
            status.then_with(|| compare_rows(left, right, WorkHistorySortMode::RecentFirst))
        });
        indices
    }

    fn browser_scope_controls(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &WorkHistorySnapshot<'_>,
        catalog: &i18n::Catalog,
    ) {
        let scope_label = self
            .browser
            .pane
            .as_deref()
            .map(|pane| session_text(pane, catalog))
            .unwrap_or_else(|| catalog.t("history.scope.all", &[]));
        egui::ComboBox::from_id_salt("history-scope")
            .selected_text(scope_label)
            .truncate()
            .width(172.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut self.browser.pane,
                    None,
                    catalog.t("history.scope.all", &[]),
                );
                if let Some(pane) = snapshot.current_pane {
                    ui.selectable_value(
                        &mut self.browser.pane,
                        Some(pane.to_owned()),
                        catalog.t("history.scope.current", &[]),
                    );
                }
                let mut seen = std::collections::HashSet::new();
                for row in snapshot.rows {
                    if Some(row.pane_id) != snapshot.current_pane && seen.insert(row.pane_id) {
                        ui.selectable_value(
                            &mut self.browser.pane,
                            Some(row.pane_id.to_owned()),
                            session_label(row, catalog),
                        );
                    }
                }
            });
        egui::ComboBox::from_id_salt("history-period")
            .selected_text(catalog.t(
                if self.browser.today_only {
                    "history.period.today"
                } else {
                    "history.period.all"
                },
                &[],
            ))
            .width(96.0)
            .truncate()
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut self.browser.today_only,
                    false,
                    catalog.t("history.period.all", &[]),
                );
                ui.selectable_value(
                    &mut self.browser.today_only,
                    true,
                    catalog.t("history.period.today", &[]),
                );
            });
    }
    fn browser_provider_control(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        egui::ComboBox::from_id_salt("history-provider")
            .selected_text(catalog.t("history.provider", &[]))
            .width(100.0)
            .truncate()
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(
                        self.providers.is_empty(),
                        catalog.t("history.filter.all", &[]),
                    )
                    .clicked()
                {
                    self.providers.clear();
                }
                for provider in WorkHistoryProvider::ALL {
                    provider_chip(
                        ui,
                        &mut self.providers,
                        provider,
                        &catalog.t(provider.key(), &[]),
                    );
                }
            });
    }
    fn browser_status_controls(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        counts: [usize; 4],
    ) {
        let tokens = browser_tokens(ui.visuals());
        for (index, status) in [
            WorkHistoryFilter::All,
            WorkHistoryFilter::Waiting,
            WorkHistoryFilter::Working,
            WorkHistoryFilter::Completed,
        ]
        .into_iter()
        .enumerate()
        {
            let label = if status == WorkHistoryFilter::All {
                catalog.t(status.key(), &[])
            } else {
                format!("{} {}", catalog.t(status.key(), &[]), counts[index])
            };
            let selected = self.filter == status;
            let response = ui.add(
                egui::Button::new(label)
                    .selected(selected)
                    .corner_radius(16),
            );
            if response.clicked() {
                self.filter = status;
            }
            if selected {
                ui.painter().rect_stroke(
                    response.rect,
                    16,
                    egui::Stroke::new(1.0, tokens.accent),
                    egui::StrokeKind::Inside,
                );
            }
        }
    }

    fn browser_view_controls(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        for (timeline, key) in [
            (false, "history.view.threads"),
            (true, "history.view.timeline"),
        ] {
            if ui
                .selectable_label(
                    !self.browser.cards && self.browser.timeline == timeline,
                    catalog.t(key, &[]),
                )
                .clicked()
            {
                self.browser.cards = false;
                self.browser.timeline = timeline;
            }
        }
        if ui
            .selectable_label(self.browser.cards, catalog.t("history.view.cards", &[]))
            .clicked()
        {
            self.browser.cards = true;
        }
    }

    fn browser_project_control(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &WorkHistorySnapshot<'_>,
        catalog: &i18n::Catalog,
    ) {
        let label = self
            .browser
            .project
            .as_deref()
            .map(|cwd| {
                catalog.t(
                    "history.project.selected",
                    &[(
                        "project",
                        cwd.trim_end_matches('/').rsplit('/').next().unwrap_or(cwd),
                    )],
                )
            })
            .unwrap_or_else(|| catalog.t("history.project.all", &[]));
        // ComboBox creates its own child layout and does not wrap like a
        // Button. Reserve a full readable field on the next toolbar row.
        if ui.available_size_before_wrap().x < 148.0 {
            ui.end_row();
        }
        egui::ComboBox::from_id_salt("history-project")
            .selected_text(label)
            .width(148.0)
            .truncate()
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut self.browser.project,
                    None,
                    catalog.t("history.project.all", &[]),
                );
                let mut seen = std::collections::HashSet::new();
                for row in snapshot.rows {
                    if let Some(cwd) = row.cwd
                        && seen.insert(cwd)
                    {
                        ui.selectable_value(&mut self.browser.project, Some(cwd.to_owned()), cwd);
                    }
                }
            });
    }

    fn browser_sort_control(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        egui::ComboBox::from_id_salt("history-sort")
            .selected_text(catalog.t(self.sort_mode.key(), &[]))
            .width(148.0)
            .truncate()
            .show_ui(ui, |ui| {
                for sort in [
                    WorkHistorySortMode::RecentFirst,
                    WorkHistorySortMode::StateFirst,
                ] {
                    ui.selectable_value(&mut self.sort_mode, sort, catalog.t(sort.key(), &[]));
                }
            });
    }

    /// Full-width controls followed by independently bounded list/detail panes.
    /// The raw transcript renderer is borrowed from the session-scoped host view.
    #[allow(clippy::too_many_arguments)]
    pub fn show_browser(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: WorkHistorySnapshot<'_>,
        presentations: &[WorkHistoryActionPresentation],
        catalog: &i18n::Catalog,
        filter: &str,
        split_width: &mut Option<f32>,
        render_transcript: impl FnMut(&mut egui::Ui),
    ) -> Option<WorkHistoryAction> {
        ui.scope(|ui| {
            browser_style(ui);
            self.show_browser_contents(
                ui,
                snapshot,
                presentations,
                catalog,
                filter,
                split_width,
                render_transcript,
            )
        })
        .inner
    }

    #[allow(clippy::too_many_arguments)]
    fn show_browser_contents(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: WorkHistorySnapshot<'_>,
        presentations: &[WorkHistoryActionPresentation],
        catalog: &i18n::Catalog,
        filter: &str,
        split_width: &mut Option<f32>,
        mut render_transcript: impl FnMut(&mut egui::Ui),
    ) -> Option<WorkHistoryAction> {
        let tokens = browser_tokens(ui.visuals());
        let outer = ui.available_rect_before_wrap();
        ui.painter().rect_filled(outer, 0.0, tokens.content_canvas);
        let mut action = None;
        let narrow = outer.width() < NARROW_WIDTH;
        let horizontal_padding = if narrow { 16 } else { 24 };
        if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.browser.more_filters = false;
        }
        let now = unix_now();
        let active_filter = self.filter;
        self.filter = WorkHistoryFilter::All;
        let candidates = self.browser_indices(&snapshot, filter, now);
        self.filter = active_filter;
        let count_state = |state| {
            candidates
                .iter()
                .filter(|&&i| snapshot.rows[i].current_state == Some(state))
                .count()
        };
        let counts = [
            candidates.len(),
            count_state(WorkHistoryState::Waiting),
            count_state(WorkHistoryState::Working),
            count_state(WorkHistoryState::Completed),
        ];
        let mut header = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(outer)
                .id_salt("history-browser-header"),
        );
        header.set_clip_rect(outer.intersect(ui.clip_rect()));
        egui::Frame::NONE
            .inner_margin(egui::Margin::symmetric(horizontal_padding, 12))
            .show(&mut header, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(catalog.t("history.refresh", &[])).clicked() {
                            action = Some(WorkHistoryAction::Refresh);
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new(catalog.t("history.browser.title", &[]))
                                    .size(21.0)
                                    .strong(),
                            );
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(snapshot.workspace_name)
                                        .size(META_FONT)
                                        .weak(),
                                )
                                .truncate(),
                            );
                        });
                    });
                });
                ui.horizontal_wrapped(|ui| {
                    self.browser_view_controls(ui, catalog);
                    if snapshot.loading {
                        ui.spinner();
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    let width = if narrow {
                        ui.available_width()
                    } else {
                        (ui.available_width() - 635.0).max(180.0)
                    };
                    ui.add_sized(
                        [width, CONTROL_HEIGHT],
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text(catalog.t("history.search", &[])),
                    );
                    self.browser_status_controls(ui, catalog, counts);
                    self.browser_project_control(ui, &snapshot, catalog);
                    if ui
                        .add(
                            egui::Button::new(catalog.t("history.filters.more", &[]))
                                .selected(self.browser.more_filters),
                        )
                        .clicked()
                    {
                        self.browser.more_filters = !self.browser.more_filters;
                    }
                });
                // ComboBox owns a root popup. Inline expansion avoids nesting that
                // popup inside a menu, which would close before a value is chosen.
                if self.browser.more_filters {
                    let mut controls = |ui: &mut egui::Ui| {
                        self.browser_scope_controls(ui, &snapshot, catalog);
                        self.browser_provider_control(ui, catalog);
                        ui.scope(|ui| {
                            compact_controls(ui);
                            self.browser_sort_control(ui, catalog);
                        });
                    };
                    if narrow {
                        ui.vertical(&mut controls);
                    } else {
                        ui.horizontal(&mut controls);
                    }
                }
                if let Some(error) = snapshot.error {
                    render_error(ui, error, catalog);
                }
                if snapshot.collection_blocked > 0 {
                    egui::Frame::NONE
                        .fill(crate::ui::designall::mix(
                            tokens.content_canvas,
                            tokens.warning,
                            0.1,
                        ))
                        .stroke(egui::Stroke::new(1.0, tokens.warning))
                        .corner_radius(6)
                        .inner_margin(10)
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(catalog.t(
                                        "history.collection.blocked",
                                        &[("count", &snapshot.collection_blocked.to_string())],
                                    ))
                                    .size(META_FONT)
                                    .color(tokens.warning),
                                )
                                .wrap(),
                            );
                        });
                }
            });
        let indices = self.browser_indices(&snapshot, filter, now);
        let groups = thread::groups(snapshot.rows, &indices);
        let selected_visible = self
            .selected
            .as_ref()
            .is_some_and(|identity| indices.iter().any(|&i| identity.matches(&snapshot.rows[i])));
        if !selected_visible {
            self.selected = indices
                .first()
                .map(|&i| WorkTurnIdentity::from(&snapshot.rows[i]));
            self.browser.mobile_detail = false;
            self.browser.conversation = false;
            self.browser.requested = None;
        }
        let body_top = header.cursor().top().min(outer.bottom());
        let body = egui::Rect::from_min_max(
            egui::pos2(outer.left(), body_top),
            egui::pos2(outer.right(), (outer.bottom() - 31.0).max(body_top)),
        );
        let footer = egui::Rect::from_min_max(egui::pos2(outer.left(), body.bottom()), outer.max);
        let list_width = crate::app::aux_split_width(
            *split_width,
            crate::app::history_tab_list_width(body.width()),
            body.width(),
            crate::app::HISTORY_TAB_LIST_MIN_WIDTH,
        );
        let (list_rect, detail_rect) = if narrow {
            (body, body)
        } else {
            body.split_left_right_at_x(body.left() + list_width)
        };
        ui.painter().hline(
            body.x_range(),
            body.top(),
            egui::Stroke::new(1.0, tokens.separator),
        );
        if !narrow || !self.browser.mobile_detail {
            let mut list = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(list_rect)
                    .id_salt("work_history_pane_tab"),
            );
            list.set_clip_rect(list_rect.intersect(ui.clip_rect()));
            list.spacing_mut().item_spacing.y = 0.0;
            egui::Frame::NONE
                .fill(tokens.workspace_background)
                .inner_margin(egui::Margin::symmetric(20, 8))
                .show(&mut list, |ui| {
                    ui.label(
                        egui::RichText::new(catalog.t(
                            "history.thread.count",
                            &[
                                ("threads", &groups.len().to_string()),
                                ("turns", &indices.len().to_string()),
                            ],
                        ))
                        .size(META_FONT)
                        .weak(),
                    );
                });
            if snapshot.rows.is_empty() {
                if snapshot.error.is_none() {
                    render_empty(&mut list, snapshot.loading, catalog);
                }
            } else if indices.is_empty() {
                render_centered_message(&mut list, catalog.t("history.no_results", &[]));
                if list
                    .button(catalog.t("history.filters.reset", &[]))
                    .clicked()
                {
                    self.query.clear();
                    self.filter = WorkHistoryFilter::All;
                    self.providers.clear();
                    self.browser.pane = None;
                    self.browser.project = None;
                    self.browser.today_only = false;
                }
            } else if self.browser.cards {
                // The legacy cache indexes this borrowed subset, not the original host rows.
                // Scope/period/live-state changes can change the subset without a DB revision.
                if self.browser.card_indices != indices {
                    self.grouped_cache = None;
                    self.list_shape = None;
                    self.browser.card_indices.clone_from(&indices);
                }
                // Keep the existing virtualized cards/action contracts. Scope the rows before
                // borrowing them, and preserve original presentation order for each row.
                let rows: Vec<_> = indices.iter().map(|&i| snapshot.rows[i]).collect();
                let actions: Vec<_> = indices
                    .iter()
                    .map(|&i| {
                        presentations
                            .get(i)
                            .copied()
                            .unwrap_or(WorkHistoryActionPresentation {
                                primary: WorkHistoryPrimaryAction::Disabled(
                                    WorkHistoryDisabledReason::Stale,
                                ),
                                show_diff: false,
                            })
                    })
                    .collect();
                self.browser_embedded = true;
                let card_action = self.show(
                    &mut list,
                    WorkHistorySnapshot {
                        rows: &rows,
                        ..snapshot
                    },
                    &actions,
                    catalog,
                    filter,
                );
                self.browser_embedded = false;
                if let Some(WorkHistoryAction::ShowTranscript(identity)) = &card_action {
                    self.selected = Some(identity.clone());
                    self.browser.conversation = true;
                    self.browser.mobile_detail = true;
                    self.browser.requested = Some(identity.clone());
                }
                if card_action.is_some() {
                    action = card_action;
                }
            } else {
                let mut selected = None;
                egui::ScrollArea::vertical()
                    .id_salt("history-browser-list")
                    .auto_shrink([false, false])
                    .show(&mut list, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        if self.browser.timeline {
                            let mut day = String::new();
                            for &i in &indices {
                                let row = &snapshot.rows[i];
                                let date = local_date(started_at(row));
                                if day != date {
                                    day = date;
                                    egui::Frame::NONE
                                        .inner_margin(egui::Margin::symmetric(20, 8))
                                        .show(ui, |ui| {
                                            ui.label(
                                                egui::RichText::new(&day).size(META_FONT).weak(),
                                            );
                                        });
                                }
                                if compact_row(
                                    ui,
                                    row,
                                    self.selected.as_ref().is_some_and(|id| id.matches(row)),
                                    catalog,
                                )
                                .clicked()
                                {
                                    selected = Some(i);
                                }
                            }
                        } else {
                            for group in &groups {
                                let anchor = group
                                    .iter()
                                    .map(|&i| &snapshot.rows[i])
                                    .min_by(|a, b| {
                                        compare_rows(a, b, WorkHistorySortMode::RecentFirst)
                                    })
                                    .expect("nonempty group");
                                thread_heading(ui, snapshot.rows, anchor, group, catalog);
                                for &i in group {
                                    let row = &snapshot.rows[i];
                                    if compact_row(
                                        ui,
                                        row,
                                        self.selected.as_ref().is_some_and(|id| id.matches(row)),
                                        catalog,
                                    )
                                    .clicked()
                                    {
                                        selected = Some(i);
                                    }
                                }
                            }
                        }
                    });
                if let Some(i) = selected {
                    self.select_browser_turn(&snapshot.rows[i]);
                }
            }
        }
        let selected_index = self.selected.as_ref().and_then(|identity| {
            indices
                .iter()
                .copied()
                .find(|&i| identity.matches(&snapshot.rows[i]))
        });
        if !narrow || self.browser.mobile_detail {
            ui.painter()
                .rect_filled(detail_rect, 0.0, tokens.workspace_background);
            if let Some(i) = selected_index {
                let contextual = thread::context(snapshot.rows, &snapshot.rows[i]);
                let row = &contextual;
                let action_height = if detail_rect.width() < 550.0 {
                    96.0
                } else {
                    66.0
                };
                let action_top = (detail_rect.bottom() - action_height).max(detail_rect.top());
                let pad = if narrow { 18.0 } else { 26.0 };
                let content = egui::Rect::from_min_max(
                    egui::pos2(detail_rect.left() + pad, detail_rect.top() + 18.0),
                    egui::pos2(
                        (detail_rect.right() - pad).max(detail_rect.left() + pad),
                        action_top.max(detail_rect.top() + 18.0),
                    ),
                );
                let mut detail = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(content)
                        .id_salt("history-browser-detail"),
                );
                detail.set_clip_rect(content.intersect(ui.clip_rect()));
                egui::ScrollArea::vertical()
                    .id_salt((
                        "history-browser-detail-scroll",
                        row.workspace_id,
                        row.kind,
                        row.agent_session_id,
                        row.turn_key,
                    ))
                    .auto_shrink([false, false])
                    .show(&mut detail, |ui| {
                        ui.spacing_mut().item_spacing.y = 8.0;
                        if narrow && ui.button(catalog.t("history.back", &[])).clicked() {
                            self.browser.mobile_detail = false;
                        }
                        egui::Frame::NONE
                            .fill(tokens.input_background)
                            .stroke(egui::Stroke::new(1.0, tokens.separator))
                            .corner_radius(8)
                            .inner_margin(12)
                            .show(ui, |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(
                                        egui::RichText::new(
                                            thread::project(row).unwrap_or(snapshot.workspace_name),
                                        )
                                        .size(16.0)
                                        .strong(),
                                    );
                                    if let Some(cwd) = row.cwd {
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(cwd).size(META_FONT).weak(),
                                            )
                                            .wrap(),
                                        );
                                    }
                                    if let Some(branch) = row.branch {
                                        ui.label(
                                            egui::RichText::new(branch).size(META_FONT).monospace(),
                                        );
                                    }
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(format!(
                                                "{} · {} · {} {}",
                                                session_label(row, catalog),
                                                row.kind,
                                                row.model.unwrap_or_default(),
                                                row.effort.unwrap_or_default()
                                            ))
                                            .size(META_FONT)
                                            .weak(),
                                        )
                                        .wrap(),
                                    );
                                });
                            });
                        ui.add_space(8.0);
                        ui.colored_label(
                            display_color(row, ui.visuals()),
                            format!(
                                "{} · {}",
                                display_state(row, catalog),
                                local_datetime(started_at(row))
                            ),
                        );
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(thread::title(snapshot.rows, row))
                                    .size(20.0)
                                    .strong(),
                            )
                            .wrap(),
                        );
                        browser_detail_tabs(ui, &mut self.browser.conversation, catalog);
                        crate::ui::hairline(ui);
                        if self.browser.conversation {
                            let identity = self.selected.as_ref().expect("selected row identity");
                            if self.browser.requested.as_ref() != Some(identity) {
                                ui.weak(catalog.t("history.transcript.loading", &[]));
                                if action.is_none() {
                                    self.browser.requested = Some(identity.clone());
                                    action =
                                        Some(WorkHistoryAction::ShowTranscript(identity.clone()));
                                }
                            } else if matches!(action, Some(WorkHistoryAction::ShowTranscript(_))) {
                                ui.weak(catalog.t("history.transcript.loading", &[]));
                            } else {
                                render_transcript(ui);
                            }
                        } else {
                            let previous = thread::previous(snapshot.rows, row);
                            if ui.available_width() >= 600.0 {
                                ui.columns(2, |columns| {
                                    instruction_section(&mut columns[0], row, catalog);
                                    if let Some(prior) = context_section(
                                        &mut columns[1],
                                        snapshot.rows,
                                        &previous,
                                        catalog,
                                        filter,
                                    ) {
                                        self.select_browser_context(&snapshot.rows[prior]);
                                    }
                                });
                            } else {
                                instruction_section(ui, row, catalog);
                                if let Some(prior) =
                                    context_section(ui, snapshot.rows, &previous, catalog, filter)
                                {
                                    self.select_browser_context(&snapshot.rows[prior]);
                                }
                            }
                            browser_text_section(
                                ui,
                                row,
                                &catalog.t(
                                    if row.state == WorkHistoryState::Completed {
                                        "history.detail.result"
                                    } else {
                                        "history.detail.saved_response"
                                    },
                                    &[],
                                ),
                                row.agent_summary
                                    .unwrap_or(&catalog.t("history.card.no_summary", &[])),
                                None,
                                true,
                                catalog,
                            );
                            if row.occurred_at.is_none() {
                                ui.weak(catalog.t("history.detail.start_unknown", &[]));
                            }
                        }
                    });
                let action_rect = egui::Rect::from_min_max(
                    egui::pos2(detail_rect.left() + pad, action_top + 10.0),
                    egui::pos2(detail_rect.right() - pad, detail_rect.bottom() - 8.0),
                );
                ui.painter().hline(
                    detail_rect.x_range(),
                    action_top,
                    egui::Stroke::new(1.0, tokens.separator),
                );
                let mut buttons = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(action_rect)
                        .id_salt("history-browser-actions"),
                );
                buttons.set_clip_rect(action_rect.intersect(ui.clip_rect()));
                if action_height > 66.0 {
                    if let Some(presentation) = presentations.get(i)
                        && let Some(intent) =
                            detail_actions(&mut buttons, row, presentation, catalog)
                    {
                        action = Some(intent);
                    }
                    browser_copy_actions(&mut buttons, row, catalog);
                } else {
                    buttons.horizontal(|ui| {
                        if let Some(presentation) = presentations.get(i)
                            && let Some(intent) = detail_actions(ui, row, presentation, catalog)
                        {
                            action = Some(intent);
                        }
                        browser_copy_actions(ui, row, catalog);
                    });
                }
            } else {
                let mut detail = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(detail_rect.shrink(24.0))
                        .id_salt("history-browser-empty"),
                );
                render_centered_message(&mut detail, catalog.t("history.detail.empty", &[]));
            }
        }
        if !narrow {
            let resize_id = ui.id().with("work_history_tab_split_resize");
            let hit = egui::Rect::from_min_max(
                egui::pos2(list_rect.right() - 3.0, body.top()),
                egui::pos2(list_rect.right() + 3.0, body.bottom()),
            );
            let response = ui
                .interact(hit, resize_id, egui::Sense::drag())
                .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
            let start_id = resize_id.with("start");
            if response.drag_started() {
                ui.ctx().data_mut(|d| d.insert_temp(start_id, list_width));
            }
            if let Some(delta) = response.total_drag_delta() {
                let start = ui
                    .ctx()
                    .data(|d| d.get_temp::<f32>(start_id))
                    .unwrap_or(list_width);
                *split_width = crate::app::aux_divider_requested_width(start, delta.x);
            }
            if response.drag_stopped() {
                ui.ctx().data_mut(|d| d.remove::<f32>(start_id));
            }
            ui.painter().vline(
                list_rect.right(),
                body.y_range(),
                crate::ui::designall::separator_stroke(ui.visuals()),
            );
        }
        let mut footer_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(footer.shrink2(egui::vec2(horizontal_padding as f32, 7.0)))
                .id_salt("history-browser-footer"),
        );
        footer_ui.set_clip_rect(footer.intersect(ui.clip_rect()));
        ui.painter().hline(
            footer.x_range(),
            footer.top(),
            egui::Stroke::new(1.0, tokens.separator),
        );
        let saved = snapshot
            .rows
            .iter()
            .map(|r| r.updated_at)
            .max()
            .map(local_datetime)
            .unwrap_or_else(|| "—".to_owned());
        footer_ui.add(
            egui::Label::new(
                egui::RichText::new(catalog.t(
                    "history.browser.footer",
                    &[
                        ("shown", &indices.len().to_string()),
                        ("total", &snapshot.rows.len().to_string()),
                        ("saved", &saved),
                    ],
                ))
                .size(11.0)
                .weak(),
            )
            .truncate(),
        );
        ui.allocate_rect(outer, egui::Sense::hover());
        action
    }
    fn select_browser_context(&mut self, row: &WorkHistoryRow<'_>) {
        // A context link can point outside local search/status/date/project
        // filters. Reveal that exact saved turn rather than reset next frame.
        self.query.clear();
        self.filter = WorkHistoryFilter::All;
        self.providers.clear();
        self.browser.pane = None;
        self.browser.project = None;
        self.browser.today_only = false;
        self.select_browser_turn(row);
    }

    fn select_browser_turn(&mut self, row: &WorkHistoryRow<'_>) {
        self.selected = Some(WorkTurnIdentity::from(row));
        self.browser.mobile_detail = true;
        self.browser.conversation = false;
        self.browser.requested = None;
    }
}

fn browser_copy_actions(ui: &mut egui::Ui, row: &WorkHistoryRow<'_>, catalog: &i18n::Catalog) {
    ui.scope(|ui| {
        compact_controls(ui);
        ui.horizontal(|ui| {
            copy_button_control(
                ui,
                catalog,
                copy_feedback_id(row, "instruction"),
                "history.action.copy_instruction",
                row.instruction,
                Some(COMPACT_CONTROL_HEIGHT),
            );
            if let Some(summary) = row.agent_summary {
                copy_button_control(
                    ui,
                    catalog,
                    copy_feedback_id(row, "summary"),
                    "history.action.copy_summary",
                    summary,
                    Some(COMPACT_CONTROL_HEIGHT),
                );
            }
        });
    });
}

fn thread_heading(
    ui: &mut egui::Ui,
    rows: &[WorkHistoryRow<'_>],
    row: &WorkHistoryRow<'_>,
    group: &[usize],
    catalog: &i18n::Catalog,
) {
    let contextual = thread::context(rows, row);
    let row = &contextual;
    let tokens = browser_tokens(ui.visuals());
    egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(20, 12))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 5.0;
            ui.add(
                egui::Label::new(
                    egui::RichText::new(thread::title(rows, row))
                        .size(BODY_FONT)
                        .strong(),
                )
                .truncate(),
            );
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(
                        thread::project(row).unwrap_or(&catalog.t("history.project.unknown", &[])),
                    )
                    .size(META_FONT)
                    .color(tokens.accent),
                );
                if let Some(branch) = row.branch {
                    ui.add(
                        egui::Label::new(egui::RichText::new(branch).size(META_FONT).monospace())
                            .truncate(),
                    );
                }
            });
            let earliest = group
                .iter()
                .map(|&i| started_at(&rows[i]))
                .min()
                .unwrap_or(started_at(row));
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!(
                        "{} · {} · {} · {}–{}",
                        session_label(row, catalog),
                        row.kind,
                        catalog.t(
                            "history.group.turns",
                            &[("count", &group.len().to_string())]
                        ),
                        local_clock(earliest),
                        local_clock(started_at(row))
                    ))
                    .size(META_FONT)
                    .weak(),
                )
                .truncate(),
            );
        });
}

fn instruction_section(ui: &mut egui::Ui, row: &WorkHistoryRow<'_>, catalog: &i18n::Catalog) {
    ui.add_space(14.0);
    ui.weak(catalog.t("history.detail.this_instruction", &[]));
    egui::Frame::NONE
        .fill(browser_tokens(ui.visuals()).content_canvas)
        .corner_radius(6)
        .inner_margin(12)
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(row.instruction)
                        .size(BODY_FONT)
                        .line_height(Some(25.2)),
                )
                .wrap(),
            );
        });
}

fn context_section(
    ui: &mut egui::Ui,
    rows: &[WorkHistoryRow<'_>],
    indices: &[usize],
    catalog: &i18n::Catalog,
    aux_filter: &str,
) -> Option<usize> {
    let mut selected = None;
    ui.add_space(14.0);
    ui.weak(catalog.t("history.detail.context", &[]));
    if indices.is_empty() {
        ui.label(
            egui::RichText::new(catalog.t("history.detail.context_empty", &[]))
                .size(META_FONT)
                .weak(),
        );
    }
    ui.scope(|ui| {
        compact_controls(ui);
        for &i in indices {
            let row = &rows[i];
            let text = format!(
                "{}  {}",
                local_clock(started_at(row)),
                collapsed_summary_line(row.instruction)
            );
            if ui
                .add_enabled_ui(
                    aux_filter.is_empty() || row_matches_aux_filter(row, aux_filter),
                    |ui| {
                        ui.add_sized(
                            [ui.available_width(), COMPACT_CONTROL_HEIGHT],
                            egui::Button::new(text).truncate(),
                        )
                    },
                )
                .inner
                .clicked()
            {
                selected = Some(i);
            }
        }
    });
    selected
}

fn browser_detail_tabs(ui: &mut egui::Ui, conversation: &mut bool, catalog: &i18n::Catalog) {
    let tokens = browser_tokens(ui.visuals());
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.spacing_mut().button_padding = egui::vec2(11.0, 9.0);
        ui.spacing_mut().interact_size.y = 39.0;
        ui.horizontal(|ui| {
            for (value, key) in [
                (false, "history.detail.summary"),
                (true, "history.detail.conversation"),
            ] {
                let active = *conversation == value;
                let response = ui.add(
                    egui::Button::new(egui::RichText::new(catalog.t(key, &[])).color(if active {
                        tokens.accent
                    } else {
                        tokens.text
                    }))
                    .fill(egui::Color32::TRANSPARENT)
                    .stroke(egui::Stroke::NONE)
                    .corner_radius(0),
                );
                if active {
                    ui.painter().hline(
                        response.rect.x_range(),
                        response.rect.bottom(),
                        egui::Stroke::new(2.0, tokens.accent),
                    );
                }
                if response.clicked() {
                    *conversation = value;
                }
            }
        });
    });
}

fn browser_text_section(
    ui: &mut egui::Ui,
    row: &WorkHistoryRow<'_>,
    label: &str,
    body: &str,
    copy: Option<(&str, &str)>,
    result: bool,
    catalog: &i18n::Catalog,
) {
    ui.add_space(23.0);
    ui.scope(|ui| {
        compact_controls(ui);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(label).size(13.0).weak());
            if let Some((suffix, key)) = copy {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    copy_button_control(
                        ui,
                        catalog,
                        copy_feedback_id(row, suffix),
                        key,
                        body,
                        Some(COMPACT_CONTROL_HEIGHT),
                    );
                });
            }
        });
    });
    ui.add_space(8.0);
    let text = egui::Label::new(
        egui::RichText::new(body)
            .size(BODY_FONT)
            .line_height(Some(25.2)),
    )
    .wrap();
    if result {
        let tokens = browser_tokens(ui.visuals());
        let section = egui::Frame::NONE
            .inner_margin(egui::Margin {
                left: 14,
                ..Default::default()
            })
            .show(ui, |ui| {
                ui.add(text);
            });
        ui.painter().vline(
            section.response.rect.left(),
            section.response.rect.y_range(),
            egui::Stroke::new(2.0, tokens.success),
        );
    } else {
        ui.add(text);
    }
}

fn compact_row(
    ui: &mut egui::Ui,
    row: &WorkHistoryRow<'_>,
    selected: bool,
    catalog: &i18n::Catalog,
) -> egui::Response {
    let tokens = browser_tokens(ui.visuals());
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ROW_HEIGHT),
        egui::Sense::click(),
    );
    // Skip offscreen galleys while keeping every accessible row exactly 68pt.
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let fill = if selected {
        tokens.selected_background
    } else if response.hovered() {
        tokens.hover_background
    } else {
        tokens.content_canvas
    };
    ui.painter().rect_filled(rect, 0.0, fill);
    if selected {
        ui.painter().rect_filled(
            egui::Rect::from_min_max(rect.min, egui::pos2(rect.left() + 3.0, rect.bottom())),
            0.0,
            tokens.accent,
        );
    }
    ui.painter().hline(
        rect.x_range(),
        rect.bottom(),
        egui::Stroke::new(1.0, tokens.separator),
    );
    let top = rect.top() + (ROW_HEIGHT - 44.0) * 0.5;
    ui.painter().circle_filled(
        egui::pos2(rect.left() + LIST_PADDING + 4.0, top + 10.0),
        3.0,
        display_color(row, ui.visuals()),
    );
    let text_left = rect.left() + LIST_PADDING + 20.0;
    let time_right = rect.right() - LIST_PADDING;
    let time_left = time_right - 60.0;
    let title_rect = egui::Rect::from_min_max(
        egui::pos2(text_left, top),
        egui::pos2((time_left - 12.0).max(text_left), top + 21.0),
    );
    let meta_rect = egui::Rect::from_min_max(
        egui::pos2(text_left, top + 26.0),
        egui::pos2((time_left - 12.0).max(text_left), top + 44.0),
    );
    // `Ui::place` centers a label even with Label::halign(Min). Explicit
    // top-down children keep text on the proposal's shared left edge.
    for (suffix, bounds, align, text) in [
        (
            "title",
            title_rect,
            egui::Align::Min,
            egui::RichText::new(if thread::continuation(row.instruction) {
                format!(
                    "{}  {}",
                    catalog.t("history.thread.continuation", &[]),
                    collapsed_summary_line(row.instruction)
                )
            } else {
                collapsed_summary_line(row.instruction).to_owned()
            })
            .size(BODY_FONT)
            .line_height(Some(21.0))
            .strong(),
        ),
        (
            "metadata",
            meta_rect,
            egui::Align::Min,
            egui::RichText::new(format!(
                "→ {}",
                row.agent_summary
                    .map(thread::preview)
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| catalog.t("history.card.no_summary", &[]))
            ))
            .size(META_FONT)
            .line_height(Some(18.0))
            .weak(),
        ),
        (
            "time",
            egui::Rect::from_min_max(
                egui::pos2(time_left, top),
                egui::pos2(time_right, top + 18.0),
            ),
            egui::Align::Max,
            egui::RichText::new(local_clock(started_at(row)))
                .size(META_FONT)
                .line_height(Some(18.0))
                .weak(),
        ),
        (
            "state",
            egui::Rect::from_min_max(
                egui::pos2(time_left, top + 26.0),
                egui::pos2(time_right, top + 44.0),
            ),
            egui::Align::Max,
            egui::RichText::new(display_state(row, catalog))
                .size(11.0)
                .line_height(Some(18.0))
                .color(display_color(row, ui.visuals())),
        ),
    ] {
        let mut text_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(bounds)
                .layout(egui::Layout::top_down(align))
                .id_salt((row.agent_session_id, row.turn_key, suffix)),
        );
        text_ui.set_clip_rect(bounds.intersect(ui.clip_rect()));
        text_ui.add(egui::Label::new(text).truncate().selectable(false));
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            selected,
            collapsed_summary_line(row.instruction),
        )
    });
    response
}

fn detail_actions(
    ui: &mut egui::Ui,
    row: &WorkHistoryRow<'_>,
    presentation: &WorkHistoryActionPresentation,
    catalog: &i18n::Catalog,
) -> Option<WorkHistoryAction> {
    let mut action = None;
    ui.horizontal_wrapped(|ui| {
        let (key, reason) = match presentation.primary {
            WorkHistoryPrimaryAction::Focus => ("history.action.focus", None),
            WorkHistoryPrimaryAction::Resume => ("history.action.resume", None),
            WorkHistoryPrimaryAction::NewRun => ("history.action.new_run", None),
            WorkHistoryPrimaryAction::Disabled(reason) => {
                ("history.action.unavailable", Some(reason))
            }
        };
        let tokens = browser_tokens(ui.visuals());
        let button = ui.add_enabled(
            reason.is_none(),
            egui::Button::new(egui::RichText::new(catalog.t(key, &[])).color(
                if ui.visuals().dark_mode {
                    egui::Color32::from_rgb(0x8c, 0xe4, 0xee)
                } else {
                    tokens.accent
                },
            ))
            .fill(crate::ui::designall::mix(
                tokens.content_canvas,
                tokens.accent,
                0.15,
            ))
            .stroke(egui::Stroke::new(1.0, tokens.accent)),
        );
        let button = if let Some(reason) = reason {
            button.on_disabled_hover_text(catalog.t(
                match reason {
                    WorkHistoryDisabledReason::Checking => "history.action.disabled.checking",
                    WorkHistoryDisabledReason::AgentUnavailable => {
                        "history.action.disabled.unavailable"
                    }
                    WorkHistoryDisabledReason::Stale => "history.action.disabled.stale",
                },
                &[],
            ))
        } else if presentation.primary == WorkHistoryPrimaryAction::NewRun {
            button.on_hover_text(catalog.t("history.action.new_run_hint", &[]))
        } else {
            button
        };
        if button.clicked() {
            action = Some(WorkHistoryAction::Activate(WorkTurnIdentity::from(row)));
        }
        if presentation.show_diff
            && ui
                .button(catalog.t("history.action.show_diff", &[]))
                .on_hover_text(catalog.t("history.action.show_diff_hint", &[]))
                .clicked()
        {
            action = Some(WorkHistoryAction::ShowDiff(WorkTurnIdentity::from(row)));
        }
    });
    action
}

fn started_at(row: &WorkHistoryRow<'_>) -> i64 {
    row.occurred_at.unwrap_or(row.updated_at)
}
fn session_text(pane: &str, catalog: &i18n::Catalog) -> String {
    catalog.t(
        "history.session",
        &[("id", &pane.chars().take(8).collect::<String>())],
    )
}
fn session_label(row: &WorkHistoryRow<'_>, catalog: &i18n::Catalog) -> String {
    let id = session_text(row.pane_id, catalog);
    row.pane_title
        .filter(|title| !title.trim().is_empty())
        .map(|title| {
            format!(
                "{} · {id}",
                crate::ui::workspace::display_pane_title(title, catalog)
            )
        })
        .unwrap_or(id)
}
pub(super) fn display_state(row: &WorkHistoryRow<'_>, catalog: &i18n::Catalog) -> String {
    row.current_state
        .map(|s| state_label(s, catalog))
        .unwrap_or_else(|| catalog.t("history.state.unknown", &[]))
}
pub(super) fn display_color(row: &WorkHistoryRow<'_>, visuals: &egui::Visuals) -> egui::Color32 {
    let tokens = browser_tokens(visuals);
    match row.current_state {
        Some(WorkHistoryState::Working) => tokens.accent,
        Some(WorkHistoryState::Waiting) => tokens.warning,
        Some(WorkHistoryState::Completed) => tokens.success,
        None => tokens.muted_text,
    }
}

fn local_seconds(seconds: i64) -> i64 {
    #[cfg(unix)]
    {
        let time = seconds as libc::time_t;
        // SAFETY: a stack-owned tm is filled by reentrant localtime_r; null falls back to UTC.
        let mut parts: libc::tm = unsafe { std::mem::zeroed() };
        if !unsafe { libc::localtime_r(&time, &mut parts) }.is_null() {
            return seconds.saturating_add(parts.tm_gmtoff as i64);
        }
    }
    seconds
}
fn local_date(seconds: i64) -> String {
    deppy_core::time::civil_date(local_seconds(seconds))
}
fn local_clock(seconds: i64) -> String {
    let day = local_seconds(seconds).rem_euclid(86_400);
    format!("{:02}:{:02}", day / 3600, day / 60 % 60)
}
fn local_datetime(seconds: i64) -> String {
    format!("{} {}", local_date(seconds), local_clock(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;

    struct State {
        view: WorkHistoryUi,
        width: Option<f32>,
        actions: Vec<WorkHistoryAction>,
        transcripts: usize,
    }

    fn fixture(key: &str, pane: &str, at: i64) -> storage::AgentWorkTurnRow {
        storage::AgentWorkTurnRow {
            workspace_id: "fixture-workspace".into(),
            pane_id: pane.into(),
            kind: "claude".into(),
            agent_session_id: format!("native-{pane}"),
            turn_key: key.into(),
            source_offset: at as u64,
            instruction: format!("사용자 지시 {key}"),
            agent_summary: Some(format!("검증한 작업 결과 {key}")),
            messages_json: None,
            model: Some("Opus 5.5".into()),
            effort: Some("xhigh".into()),
            cwd: Some("/fixture/project".into()),
            branch: Some("feature/history".into()),
            git_change_count: Some(10),
            state: storage::AgentWorkTurnState::Completed,
            occurred_at: Some(at),
            updated_at: at,
        }
    }

    fn harness(
        size: egui::Vec2,
        rows: Vec<storage::AgentWorkTurnRow>,
        blocked: usize,
    ) -> egui_kittest::Harness<'static, State> {
        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        egui_kittest::Harness::builder()
            .with_size(size)
            .build_ui_state(
                move |ui, state: &mut State| {
                    crate::ui::context_menu_audit::prepare(ui.ctx());
                    let views: Vec<_> = rows.iter().map(WorkHistoryRow::from).collect();
                    let presentations = vec![
                        WorkHistoryActionPresentation {
                            primary: WorkHistoryPrimaryAction::Focus,
                            show_diff: true
                        };
                        rows.len()
                    ];
                    let before_style = ui.style().clone();
                    let action = state.view.show_browser(
                        ui,
                        WorkHistorySnapshot {
                            workspace_name: "이력 검증 워크스페이스",
                            current_branch: Some("feature/history"),
                            rows: &views,
                            rows_revision: 1,
                            loading: false,
                            error: None,
                            current_pane: Some("pane-a"),
                            collection_blocked: blocked,
                        },
                        &presentations,
                        &catalog,
                        "",
                        &mut state.width,
                        |ui| {
                            state.transcripts += 1;
                            ui.label("선택한 작업의 실제 원문 영역");
                        },
                    );
                    assert_eq!(
                        ui.style().text_styles,
                        before_style.text_styles,
                        "history typography must not change other panes"
                    );
                    assert_eq!(
                        ui.spacing().button_padding,
                        before_style.spacing.button_padding,
                        "history component sizing must remain scoped"
                    );
                    if let Some(action) = action {
                        state.actions.push(action);
                    }
                },
                State {
                    view: WorkHistoryUi::new(),
                    width: None,
                    actions: vec![],
                    transcripts: 0,
                },
            )
    }

    fn click_row(harness: &mut egui_kittest::Harness<'_, State>, key: &str) {
        let label = format!("사용자 지시 {key}");
        harness
            .query_all_by_label(&label)
            .find(|node| (node.rect().height() - ROW_HEIGHT).abs() < 0.1)
            .expect("compact row has a full-height accessible hit area")
            .click();
        harness.run();
    }

    #[test]
    fn approved_history_desktop_selects_detail_and_routes_exact_actions() {
        let mut h = harness(
            egui::vec2(1100.0, 720.0),
            vec![
                fixture("first", "pane-a", 200),
                fixture("second", "pane-b", 100),
            ],
            1,
        );
        h.run();
        assert_eq!(h.state().view.selected.as_ref().unwrap().turn_key, "first");
        assert!(
            h.get_by_label("지시 복사").rect().left() > 360.0,
            "default selected detail is on the right"
        );
        h.get_by_label("현재 세션으로 이동").click();
        h.run();
        assert!(
            matches!(h.state().actions.last(), Some(WorkHistoryAction::Activate(id)) if id.turn_key == "first")
        );
        h.get_by_label("현재 Git 보기").click();
        h.run();
        assert!(
            matches!(h.state().actions.last(), Some(WorkHistoryAction::ShowDiff(id)) if id.turn_key == "first")
        );
        click_row(&mut h, "second");
        assert_eq!(h.state().view.selected.as_ref().unwrap().turn_key, "second");
        h.get_by_label("원문 대화").click();
        h.run();
        assert!(
            matches!(h.state().actions.last(), Some(WorkHistoryAction::ShowTranscript(id)) if id.turn_key == "second")
        );
        let count = h.state().actions.len();
        h.run();
        assert_eq!(
            h.state().actions.len(),
            count,
            "conversation intent is sent once per selected identity"
        );
        assert!(h.state().transcripts > 0);
    }

    #[test]
    fn approved_history_narrow_navigates_list_detail_back_without_overflow() {
        let mut h = harness(
            egui::vec2(390.0, 720.0),
            vec![fixture("first", "pane-a", 200)],
            0,
        );
        h.run();
        h.get_by_label("추가 필터").click();
        h.run();
        for label in [
            "워크스페이스 전체",
            "전체 기간",
            "에이전트",
            "작업 시작 최신순",
            "카드",
        ] {
            let control = if label == "카드" {
                h.get_by_label(label)
            } else {
                h.get_by_value(label)
            };
            assert!(
                control.rect().right() <= 390.0,
                "clipped narrow control: {label}"
            );
        }
        h.key_press(egui::Key::Escape);
        h.run();
        assert!(h.query_by_label("지시 복사").is_none());
        click_row(&mut h, "first");
        assert!(h.query_by_label("지시 복사").is_some());
        assert!(h.get_by_label("← 이력 목록").rect().right() <= 390.0);
        h.get_by_label("← 이력 목록").click();
        h.run();
        assert!(h.query_by_label("지시 복사").is_none());
        assert!(!h.state().view.browser.mobile_detail);
    }

    #[test]
    fn approved_history_card_reclick_preserves_selected_conversation() {
        let mut h = harness(
            egui::vec2(1100.0, 720.0),
            vec![fixture("first", "pane-a", 200)],
            0,
        );
        h.run();
        h.get_by_label("원문 대화").click();
        h.run();
        h.get_by_label("카드").click();
        h.run();
        h.get_by_role_and_label(egui::accesskit::Role::Button, "사용자 지시 first")
            .click();
        h.run();
        assert_eq!(h.state().view.selected.as_ref().unwrap().turn_key, "first");
        assert!(h.state().view.browser.conversation);
    }

    #[test]
    fn approved_history_cards_rebuild_when_scope_changes_without_row_revision() {
        let mut h = harness(
            egui::vec2(1100.0, 720.0),
            vec![
                fixture("first", "pane-a", 200),
                fixture("second", "pane-b", 100),
            ],
            0,
        );
        h.run();
        h.get_by_label("카드").click();
        h.run();
        h.get_by_label("추가 필터").click();
        h.run();
        h.get_by_value("워크스페이스 전체").click();
        h.run();
        h.get_by_label("현재 세션").click();
        h.run();
        assert_eq!(h.state().view.browser.pane.as_deref(), Some("pane-a"));
        h.key_press(egui::Key::Escape);
        h.run();
        assert_eq!(h.state().view.selected.as_ref().unwrap().turn_key, "first");
        assert!(
            h.query_by_role_and_label(egui::accesskit::Role::Button, "사용자 지시 second")
                .is_none()
        );
    }

    #[test]
    fn approved_history_search_and_scope_controls_reconcile_selection() {
        let mut h = harness(
            egui::vec2(1100.0, 720.0),
            vec![
                fixture("first", "pane-a", 200),
                fixture("second", "pane-b", 100),
            ],
            0,
        );
        h.run();
        h.get_by_label("추가 필터").click();
        h.run();
        h.get_by_value("워크스페이스 전체").click();
        h.run();
        h.get_by_label("현재 세션").click();
        h.run();
        assert_eq!(h.state().view.browser.pane.as_deref(), Some("pane-a"));
        h.key_press(egui::Key::Escape);
        h.run();
        h.get_by_role(egui::accesskit::Role::TextInput).click();
        h.run();
        h.get_by_role(egui::accesskit::Role::TextInput)
            .type_text("no matches");
        h.run();
        assert_eq!(
            h.state().view.query,
            "no matches",
            "typing requires the search field to own focus"
        );
        assert!(h.state().view.selected.is_none());
        h.get_by_label("필터 초기화").click();
        h.run();
        assert!(h.state().view.query.is_empty());
        assert!(h.state().view.browser.pane.is_none());
        assert_eq!(h.state().view.selected.as_ref().unwrap().turn_key, "first");
    }

    #[test]
    fn approved_history_unknown_active_states_do_not_pass_working_filter() {
        let rows = [
            fixture("first", "pane-a", 200),
            fixture("second", "pane-b", 100),
        ];
        let mut views: Vec<_> = rows.iter().map(WorkHistoryRow::from).collect();
        views[0].state = WorkHistoryState::Working;
        views[0].current_state = None;
        views[1].state = WorkHistoryState::Working;
        views[1].current_state = Some(WorkHistoryState::Working);
        let snapshot = WorkHistorySnapshot {
            rows: &views,
            workspace_name: "fixture",
            current_branch: None,
            rows_revision: 1,
            loading: false,
            error: None,
            current_pane: Some("pane-b"),
            collection_blocked: 0,
        };
        let mut view = WorkHistoryUi::new();
        view.filter = WorkHistoryFilter::Working;
        assert_eq!(view.browser_indices(&snapshot, "", 300), [1]);
        view.filter = WorkHistoryFilter::All;
        view.browser.pane = Some("pane-a".into());
        assert_eq!(view.browser_indices(&snapshot, "", 300), [0]);
        view.browser.pane = None;
        view.query = "native-pane-b".into();
        assert_eq!(view.browser_indices(&snapshot, "", 300), [1]);
    }

    #[test]
    fn approved_history_rows_and_footer_keep_fixed_bounds_over_repaints() {
        let mut first = fixture("first", "pane-a", 200);
        first.agent_summary = Some("긴 결과를 내부 스크롤로 읽습니다.\n".repeat(300));
        let mut h = harness(
            egui::vec2(1100.0, 720.0),
            vec![first, fixture("second", "pane-b", 100)],
            0,
        );
        h.run();
        for _ in 0..30 {
            h.step();
        }
        h.key_press(egui::Key::Escape);
        h.run();
        let row = h
            .query_all_by_label("사용자 지시 first")
            .find(|n| (n.rect().height() - ROW_HEIGHT).abs() < 0.1)
            .unwrap();
        assert_eq!(row.rect().height(), ROW_HEIGHT);
        assert!(row.rect().bottom() < 720.0);
        let footer_label = format!(
            "2/2개 · 최근 최대 256개 보관 · 마지막 저장 {}",
            local_datetime(200)
        );
        for label in [
            "현재 세션으로 이동",
            "현재 Git 보기",
            "지시 복사",
            "요약 복사",
        ] {
            let rect = h.get_by_label(label).rect();
            assert!(
                rect.top() > 600.0 && rect.bottom() < 690.0,
                "pinned action must stay visible over a long result: {label}: {rect:?}"
            );
        }
        let footer = h.get_by_label(&footer_label);
        assert!(
            footer.rect().bottom() <= 720.0,
            "long detail must not grow the outer view"
        );
    }

    #[test]
    fn approved_history_component_geometry_matches_proposal() {
        let mut h = harness(
            egui::vec2(1100.0, 720.0),
            vec![
                fixture("first", "pane-a", 200),
                fixture("second", "pane-b", 100),
            ],
            0,
        );
        h.run();
        for (name, height) in [
            ("새로고침", 37.0),
            ("현재 세션으로 이동", 37.0),
            ("현재 Git 보기", 37.0),
            ("지시 복사", 26.0),
            ("요약 복사", 26.0),
            ("지시와 결과", 39.0),
            ("원문 대화", 39.0),
        ] {
            let rect = h.get_by_label(name).rect();
            assert!((rect.height() - height).abs() < 0.1, "{name}: {rect:?}");
        }
        assert_eq!(
            h.get_by_role(egui::accesskit::Role::TextInput)
                .rect()
                .height(),
            37.0
        );
        h.get_by_label("추가 필터").click();
        h.run();
        assert_eq!(h.get_by_value("워크스페이스 전체").rect().height(), 37.0);
        assert_eq!(h.get_by_value("작업 시작 최신순").rect().height(), 26.0);
        h.key_press(egui::Key::Escape);
        h.run();
        let row = h
            .query_all_by_label("사용자 지시 first")
            .find(|node| (node.rect().height() - 68.0).abs() < 0.1)
            .unwrap()
            .rect();
        let title = h
            .query_all_by_label("사용자 지시 first")
            .find(|node| (node.rect().height() - 21.0).abs() < 0.1)
            .unwrap()
            .rect();
        let meta = h.get_by_label("→ 검증한 작업 결과 first").rect();
        assert_eq!(
            title.left(),
            row.left() + 40.0,
            "row titles must be left aligned"
        );
        assert_eq!(title.top(), row.top() + 12.0);
        assert_eq!(meta.left(), title.left());
        assert_eq!(meta.top(), row.top() + 38.0);
        assert_eq!(meta.height(), 18.0);
        assert_eq!(
            row.bottom() - meta.bottom(),
            12.0,
            "no excess blank area below metadata"
        );
        let clock = h.get_by_label(&local_clock(200)).rect();
        assert!(clock.left() > title.right());
        assert_eq!(clock.right(), row.right() - 20.0);
    }

    #[test]
    fn approved_history_smallest_layout_keeps_every_control_inside_view() {
        for (width, height) in [
            (320.0, 568.0),
            (320.0, 720.0),
            (390.0, 720.0),
            (760.0, 720.0),
            (900.0, 720.0),
            (1100.0, 720.0),
        ] {
            let mut h = harness(
                egui::vec2(width, height),
                vec![fixture("first", "pane-a", 200)],
                0,
            );
            h.run();
            for label in ["새로고침", "전체", "시간순", "스레드로 묶기", "카드"] {
                let controls: Vec<_> = h.query_all_by_label(label).collect();
                assert!(!controls.is_empty(), "missing control: {width}: {label}");
                for control in controls {
                    let rect = control.rect();
                    assert!(
                        rect.left() >= 0.0 && rect.right() <= width,
                        "{width}: {label}: {rect:?}"
                    );
                }
            }
            let project = h.get_by_value("프로젝트: 전체").rect();
            assert!(
                project.left() >= 0.0 && project.right() <= width,
                "project must wrap instead of clipping: {width}: {project:?}"
            );
            h.get_by_label("추가 필터").click();
            h.run();
            for value in [
                "워크스페이스 전체",
                "전체 기간",
                "에이전트",
                "작업 시작 최신순",
            ] {
                let rect = h.get_by_value(value).rect();
                assert!(
                    rect.left() >= 0.0 && rect.right() <= width,
                    "{width}: {value}: {rect:?}"
                );
            }
            h.key_press(egui::Key::Escape);
            h.run();
            // Harness size includes the parent panel's margins; use the
            // browser's rendered mode rather than the outer window width.
            if h.query_by_label("지시 복사").is_none() {
                click_row(&mut h, "first");
            }
            for label in [
                "지시 복사",
                "요약 복사",
                "현재 세션으로 이동",
                "현재 Git 보기",
            ] {
                let rect = h.get_by_label(label).rect();
                assert!(
                    rect.left() >= 0.0
                        && rect.right() <= width
                        && rect.top() >= 0.0
                        && rect.bottom() <= height,
                    "{width}x{height}: {label}: {rect:?}"
                );
            }
        }
    }

    #[test]
    fn claude_history_context_and_project_filter_keep_exact_turn() {
        let mut first = fixture("parent", "pane-a", 200);
        first.instruction =
            "수집 토큰은 같은 계정에서 만들면 안돼? 구현과 사용 방법을 확인해".into();
        let mut short = fixture("short", "pane-a", 300);
        short.instruction = "한글로".into();
        short.cwd = None;
        short.branch = None;
        let mut other = fixture("other", "pane-b", 100);
        other.cwd = Some("/fixture/other-project".into());
        let parent = first.instruction.clone();
        let mut h = harness(egui::vec2(1200.0, 800.0), vec![short, first, other], 0);
        h.run();
        assert!(h.query_all_by_label(&parent).count() >= 1);
        assert!(h.query_by_label("스레드 2 · 작업 3").is_some());
        h.get_by_value("프로젝트: 전체").click();
        h.run();
        h.get_by_role_and_label(egui::accesskit::Role::Button, "/fixture/project")
            .click();
        h.run();
        assert_eq!(h.state().view.selected.as_ref().unwrap().turn_key, "short");
        assert!(h.query_by_label("스레드 1 · 작업 2").is_some());
        h.get_by_value("프로젝트: project").click();
        h.run();
        h.get_by_label("프로젝트: 전체").click();
        h.run();
        h.state_mut().view.query = "한글로".into();
        h.run();
        let prior = format!("{}  {}", local_clock(200), parent);
        h.get_by_label(&prior).click();
        h.run();
        assert_eq!(h.state().view.selected.as_ref().unwrap().turn_key, "parent");
        assert!(h.state().view.query.is_empty());
        h.get_by_label("현재 세션으로 이동").click();
        h.run();
        assert!(
            matches!(h.state().actions.last(),Some(WorkHistoryAction::Activate(id)) if id.turn_key=="parent")
        );
        h.get_by_label("시간순").click();
        h.run();
        assert!(h.state().view.browser.timeline);
        h.get_by_label("스레드로 묶기").click();
        h.run();
        assert!(!h.state().view.browser.timeline);
        h.get_by_value("프로젝트: 전체").click();
        h.run();
        h.get_by_label("/fixture/other-project").click();
        h.run();
        assert_eq!(
            h.state().view.browser.project.as_deref(),
            Some("/fixture/other-project")
        );
        assert_eq!(h.state().view.selected.as_ref().unwrap().turn_key, "other");
    }

    #[test]
    fn claude_history_uses_localized_pane_titles() {
        let stored = fixture("first", "pane-a", 100);
        let mut row = WorkHistoryRow::from(&stored);
        row.pane_title = Some("workspace.spawn.agent 11");
        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        let title = session_label(&row, &catalog);
        assert!(title.starts_with("에이전트 11"));
        assert!(!title.contains("workspace.spawn"));
    }

    fn claude_render_rows() -> Vec<storage::AgentWorkTurnRow> {
        [
            (
                "남은 것 진행하자.",
                "수집 토큰의 계정별 생성 규칙을 확인했습니다. 설정과 검증을 마무리했습니다.",
                "pane-a",
                1_791_441_900,
            ),
            (
                "1번 구현 제대로 되었는지 체크해",
                "토큰 생성과 저장 경로를 점검했습니다. 회귀 검사를 완료했습니다.",
                "pane-a",
                1_791_441_000,
            ),
            (
                "수집 토큰은 같은 계정에서 만들 수 있는지 확인해",
                "같은 계정의 추가 토큰 생성 절차와 제약을 정리했습니다.",
                "pane-a",
                1_791_440_100,
            ),
            (
                "토큰 수집 서버를 추가했을 때 활용 방법을 조사해",
                "서버를 추가하는 구성과 계정 연결 방식을 정리했습니다.",
                "pane-a",
                1_791_439_200,
            ),
            (
                "한글로",
                "모바일 터미널의 높이와 입력 흐름을 한글로 정리했습니다.",
                "pane-b",
                1_791_438_300,
            ),
            (
                "모바일 터미널 화면을 전체 높이로 표시하고 입력을 통합해",
                "터미널과 입력을 한 화면에 배치했습니다. 작은 화면에서도 스크롤을 유지합니다.",
                "pane-b",
                1_791_437_400,
            ),
            (
                "관리자에서 마지막 확인 기간을 설정하는 화면을 구현해",
                "관리자 설정에 기간 필드를 추가하고 저장된 값을 표시했습니다.",
                "pane-c",
                1_791_436_500,
            ),
            (
                "설정 검색에 워크스페이스의 모든 항목을 포함해",
                "워크스페이스 이름과 경로를 검색 대상으로 추가했습니다.",
                "pane-c",
                1_791_435_600,
            ),
        ]
        .into_iter()
        .map(|(prompt, summary, pane, at)| {
            let mut row = fixture(prompt, pane, at);
            row.instruction = prompt.into();
            row.agent_summary = Some(summary.into());
            row.cwd = Some(format!(
                "/Users/demo/projects/{}",
                if pane == "pane-a" {
                    "nomorevibe"
                } else if pane == "pane-b" {
                    "deppy-sijo"
                } else {
                    "Design"
                }
            ));
            if pane == "pane-a" {
                row.branch = Some("feat/collector-token".into());
            } else if pane == "pane-b" {
                row.kind = "codex".into();
                row.model = Some("GPT-6.1-Sol".into());
                row.branch = Some("feat/mobile-terminal".into());
            }
            if thread::continuation(prompt) {
                row.cwd = None;
                row.branch = None;
            }
            row
        })
        .collect()
    }

    #[test]
    #[ignore = "offscreen native evidence; does not launch the app"]
    fn approved_history_render_evidence() {
        let out = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/mockups/work-history-claude-2026-10-11-assets");
        std::fs::create_dir_all(&out).unwrap();
        let parity = out.clone();
        std::fs::create_dir_all(&parity).unwrap();
        for (name, size) in [
            ("desktop", egui::vec2(1100.0, 720.0)),
            ("narrow-list", egui::vec2(390.0, 720.0)),
            ("narrow-detail", egui::vec2(390.0, 720.0)),
        ] {
            let mut h = harness(size, claude_render_rows(), 0);
            h.run();
            if name == "narrow-detail" {
                h.query_all_by_label("남은 것 진행하자.")
                    .find(|node| (node.rect().height() - ROW_HEIGHT).abs() < 0.1)
                    .unwrap()
                    .click();
                h.run();
            }
            h.remove_cursor();
            h.run();
            let rendered = h.render().unwrap();
            rendered.save(out.join(format!("{name}.png"))).unwrap();
            rendered
                .save(parity.join(format!("after-{name}.png")))
                .unwrap();
        }
        let mut normal = harness(egui::vec2(1440.0, 900.0), claude_render_rows(), 0);
        normal.run();
        normal.remove_cursor();
        normal.run();
        normal
            .render()
            .unwrap()
            .save(parity.join("after-normal.png"))
            .unwrap();
        normal.get_by_label("시간순").click();
        normal.run();
        normal
            .render()
            .unwrap()
            .save(out.join("timeline.png"))
            .unwrap();
        normal.get_by_label("스레드로 묶기").click();
        normal.run();
        let mut measurements = serde_json::Map::new();
        for name in [
            "새로고침",
            "지시 복사",
            "요약 복사",
            "현재 세션으로 이동",
            "현재 Git 보기",
            "지시와 결과",
            "원문 대화",
        ] {
            let rect = normal.get_by_label(name).rect();
            measurements.insert(
                name.into(),
                serde_json::json!([rect.left(), rect.top(), rect.width(), rect.height()]),
            );
        }
        normal.get_by_label("추가 필터").click();
        normal.run();
        for name in [
            "워크스페이스 전체",
            "전체 기간",
            "에이전트",
            "작업 시작 최신순",
        ] {
            let rect = normal.get_by_value(name).rect();
            measurements.insert(
                name.into(),
                serde_json::json!([rect.left(), rect.top(), rect.width(), rect.height()]),
            );
        }
        let search = normal.get_by_role(egui::accesskit::Role::TextInput).rect();
        measurements.insert(
            "search".into(),
            serde_json::json!([search.left(), search.top(), search.width(), search.height()]),
        );
        std::fs::write(parity.join("measurements.json"), serde_json::to_vec_pretty(&serde_json::json!({
            "source": "production show_browser through egui_kittest; isolated fixtures, no app launch",
            "image": "after-normal.png",
            "pixels_per_point": normal.ctx.pixels_per_point(),
            "image_size": [1440, 900],
            "rect_order": ["x", "y", "width", "height"],
            "controls": measurements
        })).unwrap()).unwrap();
    }
}
