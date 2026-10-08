//! Compact history browser. All reads/activation remain durable intents handled by App.
use super::*;

const ROW_HEIGHT: f32 = 68.0;
const NARROW_WIDTH: f32 = 760.0;

#[derive(Default)]
pub(super) struct BrowserState {
    pane: Option<String>,
    today_only: bool,
    cards: bool,
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
            .width(140.0)
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
            .width(85.0)
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
            .width(85.0)
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
    fn browser_status_controls(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        for status in [
            WorkHistoryFilter::All,
            WorkHistoryFilter::Working,
            WorkHistoryFilter::Waiting,
            WorkHistoryFilter::Completed,
        ] {
            filter_chip(ui, &mut self.filter, status, &catalog.t(status.key(), &[]));
        }
    }
    fn browser_view_controls(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        ui.selectable_value(
            &mut self.browser.cards,
            false,
            catalog.t("history.view.list", &[]),
        );
        ui.selectable_value(
            &mut self.browser.cards,
            true,
            catalog.t("history.view.cards", &[]),
        );
    }
    fn browser_sort_control(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        egui::ComboBox::from_id_salt("history-sort")
            .selected_text(catalog.t(self.sort_mode.key(), &[]))
            .width(130.0)
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
        mut render_transcript: impl FnMut(&mut egui::Ui),
    ) -> Option<WorkHistoryAction> {
        let tokens = crate::ui::designall::tokens(ui.visuals());
        let outer = ui.available_rect_before_wrap();
        let mut action = None;
        let narrow = outer.width() < NARROW_WIDTH;
        let mut header = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(outer)
                .id_salt("history-browser-header"),
        );
        header.set_clip_rect(outer.intersect(ui.clip_rect()));
        egui::Frame::NONE
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(&mut header, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(catalog.t("history.refresh", &[])).clicked() {
                            action = Some(WorkHistoryAction::Refresh);
                        }
                        if snapshot.loading {
                            ui.spinner();
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new(catalog.t("history.browser.title", &[]))
                                    .strong()
                                    .size(20.0),
                            );
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(snapshot.workspace_name).weak(),
                                )
                                .truncate(),
                            );
                        });
                    });
                });
                if let Some(pane) = snapshot.current_pane {
                    let label = snapshot
                        .rows
                        .iter()
                        .find(|row| row.pane_id == pane)
                        .map(|row| session_label(row, catalog))
                        .unwrap_or_else(|| session_text(pane, catalog));
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(
                                catalog.t("history.scope.context", &[("session", &label)]),
                            )
                            .small()
                            .weak(),
                        )
                        .truncate(),
                    );
                }
                ui.add_space(7.0);
                if narrow {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text(catalog.t("history.search", &[]))
                            .desired_width(ui.available_width() - 4.0),
                    );
                    ui.horizontal(|ui| self.browser_scope_controls(ui, &snapshot, catalog));
                    ui.horizontal(|ui| self.browser_status_controls(ui, catalog));
                    ui.horizontal(|ui| {
                        self.browser_provider_control(ui, catalog);
                        self.browser_sort_control(ui, catalog);
                    });
                    ui.horizontal(|ui| self.browser_view_controls(ui, catalog));
                } else {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.query)
                                .hint_text(catalog.t("history.search", &[]))
                                .desired_width(240.0),
                        );
                        self.browser_scope_controls(ui, &snapshot, catalog);
                        self.browser_provider_control(ui, catalog);
                    });
                    ui.horizontal(|ui| {
                        self.browser_status_controls(ui, catalog);
                        ui.separator();
                        self.browser_view_controls(ui, catalog);
                        self.browser_sort_control(ui, catalog);
                    });
                }
                if let Some(error) = snapshot.error {
                    render_error(ui, error, catalog);
                }
                if snapshot.collection_blocked > 0 {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(catalog.t(
                                "history.collection.blocked",
                                &[("count", &snapshot.collection_blocked.to_string())],
                            ))
                            .color(tokens.warning),
                        )
                        .wrap(),
                    );
                }
            });
        let now = unix_now();
        let indices = self.browser_indices(&snapshot, filter, now);
        let selected_visible = self
            .selected
            .as_ref()
            .is_some_and(|identity| indices.iter().any(|&i| identity.matches(&snapshot.rows[i])));
        if !selected_visible {
            self.selected = indices
                .first()
                .map(|&i| WorkTurnIdentity::from(&snapshot.rows[i]));
            self.browser.mobile_detail = false;
        }
        let footer_height = 24.0;
        let body_top = header.cursor().top().min(outer.bottom());
        let body = egui::Rect::from_min_max(
            egui::pos2(outer.left(), body_top),
            egui::pos2(
                outer.right(),
                (outer.bottom() - footer_height).max(body_top),
            ),
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
        if !narrow || !self.browser.mobile_detail {
            let mut list = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(list_rect)
                    .id_salt("work_history_pane_tab"),
            );
            list.set_clip_rect(list_rect.intersect(ui.clip_rect()));
            if snapshot.rows.is_empty() {
                if snapshot.error.is_none() {
                    render_empty(&mut list, snapshot.loading, catalog);
                }
            } else if indices.is_empty() {
                render_centered_message(&mut list, catalog.t("history.no_results", &[]));
                list.vertical_centered(|ui| {
                    if ui.button(catalog.t("history.filters.reset", &[])).clicked() {
                        self.query.clear();
                        self.filter = WorkHistoryFilter::All;
                        self.providers.clear();
                        self.browser.pane = None;
                        self.browser.today_only = false;
                    }
                });
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
                        let mut day = String::new();
                        for &i in &indices {
                            let row = &snapshot.rows[i];
                            let date = local_date(started_at(row));
                            if date != day {
                                day = date;
                                egui::Frame::NONE
                                    .fill(tokens.workspace_background)
                                    .inner_margin(egui::Margin::symmetric(14, 5))
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        ui.weak(&day);
                                    });
                            }
                            if compact_row(
                                ui,
                                row,
                                self.selected.as_ref().is_some_and(|s| s.matches(row)),
                                catalog,
                            )
                            .clicked()
                            {
                                selected = Some(WorkTurnIdentity::from(row));
                            }
                        }
                    });
                if let Some(selected) = selected {
                    self.selected = Some(selected);
                    self.browser.mobile_detail = true;
                    self.browser.conversation = false;
                    self.browser.requested = None;
                }
            }
        }
        // Resolve selection again after a row click; detail updates in the same frame.
        let selected_index = self.selected.as_ref().and_then(|identity| {
            indices
                .iter()
                .copied()
                .find(|&i| identity.matches(&snapshot.rows[i]))
        });
        if !narrow || self.browser.mobile_detail {
            let mut detail = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(detail_rect.shrink2(egui::vec2(14.0, 8.0)))
                    .id_salt("history-browser-detail"),
            );
            detail.set_clip_rect(detail_rect.intersect(ui.clip_rect()));
            if narrow && detail.button(catalog.t("history.back", &[])).clicked() {
                self.browser.mobile_detail = false;
            }
            if let Some(i) = selected_index {
                let row = &snapshot.rows[i];
                detail.colored_label(
                    display_color(row, detail.visuals()),
                    display_state(row, catalog),
                );
                detail.add(
                    egui::Label::new(
                        egui::RichText::new(collapsed_summary_line(row.instruction))
                            .strong()
                            .size(17.0),
                    )
                    .truncate(),
                );
                detail.weak(format!(
                    "{} · {} · {}",
                    session_label(row, catalog),
                    row.kind,
                    local_datetime(started_at(row))
                ));
                if let Some(model) = row.model {
                    detail.weak(format!("{} {}", model, row.effort.unwrap_or_default()));
                }
                detail.horizontal(|ui| {
                    ui.selectable_value(
                        &mut self.browser.conversation,
                        false,
                        catalog.t("history.detail.summary", &[]),
                    );
                    ui.selectable_value(
                        &mut self.browser.conversation,
                        true,
                        catalog.t("history.detail.conversation", &[]),
                    );
                });
                crate::ui::hairline(&mut detail);
                if self.browser.conversation {
                    let identity = self.selected.as_ref().expect("selected row identity");
                    if self.browser.requested.as_ref() != Some(identity) {
                        // Do not paint a previous row's transcript while this intent is pending.
                        detail.weak(catalog.t("history.transcript.loading", &[]));
                        if action.is_none() {
                            self.browser.requested = Some(identity.clone());
                            action = Some(WorkHistoryAction::ShowTranscript(identity.clone()));
                        }
                    } else if matches!(action, Some(WorkHistoryAction::ShowTranscript(_))) {
                        detail.weak(catalog.t("history.transcript.loading", &[]));
                    } else {
                        render_transcript(&mut detail);
                    }
                } else {
                    egui::ScrollArea::vertical()
                        .id_salt((
                            "history-browser-summary",
                            row.turn_key,
                            row.agent_session_id,
                        ))
                        .auto_shrink([false, false])
                        .show(&mut detail, |ui| {
                            ui.add_space(8.0);
                            expanded_text(
                                ui,
                                &catalog.t("history.detail.instruction", &[]),
                                row.instruction,
                            );
                            copy_button(
                                ui,
                                catalog,
                                copy_feedback_id(row, "instruction"),
                                "history.action.copy_instruction",
                                row.instruction,
                            );
                            ui.add_space(14.0);
                            let label = catalog.t(
                                if row.state == WorkHistoryState::Completed {
                                    "history.detail.result"
                                } else {
                                    "history.detail.saved_response"
                                },
                                &[],
                            );
                            expanded_text(
                                ui,
                                &label,
                                row.agent_summary
                                    .unwrap_or(&catalog.t("history.card.no_summary", &[])),
                            );
                            if let Some(summary) = row.agent_summary {
                                copy_button(
                                    ui,
                                    catalog,
                                    copy_feedback_id(row, "summary"),
                                    "history.action.copy_summary",
                                    summary,
                                );
                            }
                            ui.add_space(14.0);
                            if let Some(presentation) = presentations.get(i)
                                && let Some(intent) = detail_actions(ui, row, presentation, catalog)
                            {
                                action = Some(intent);
                            }
                            egui::CollapsingHeader::new(catalog.t("history.detail.info", &[]))
                                .id_salt((row.agent_session_id, row.turn_key))
                                .show(ui, |ui| {
                                    if let Some(cwd) = row.cwd {
                                        ui.label(cwd);
                                    }
                                    if let Some(branch) = row.branch {
                                        metadata_chip(ui, branch);
                                    }
                                    if row.occurred_at.is_none() {
                                        ui.weak(catalog.t("history.detail.start_unknown", &[]));
                                    }
                                    ui.weak(catalog.t("history.detail.git_note", &[]));
                                    ui.weak(format!(
                                        "{}: {}",
                                        catalog.t("history.detail.saved_state", &[]),
                                        state_label(row.state, catalog)
                                    ));
                                    ui.weak(format!(
                                        "{}: {}",
                                        catalog.t("history.detail.updated", &[]),
                                        local_datetime(row.updated_at)
                                    ));
                                });
                        });
                }
            } else {
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
                .max_rect(footer.shrink2(egui::vec2(14.0, 3.0)))
                .id_salt("history-browser-footer"),
        );
        footer_ui.set_clip_rect(footer.intersect(ui.clip_rect()));
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
                .small()
                .weak(),
            )
            .truncate(),
        );
        ui.allocate_rect(outer, egui::Sense::hover());
        action
    }
}

fn compact_row(
    ui: &mut egui::Ui,
    row: &WorkHistoryRow<'_>,
    selected: bool,
    catalog: &i18n::Catalog,
) -> egui::Response {
    let tokens = crate::ui::designall::tokens(ui.visuals());
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ROW_HEIGHT),
        egui::Sense::click(),
    );
    // Skip text/galleys for offscreen rows while retaining stable row geometry.
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let fill = if selected {
        tokens.selected_background
    } else if response.hovered() {
        ui.visuals().widgets.hovered.weak_bg_fill
    } else {
        tokens.content_canvas
    };
    ui.painter().rect_filled(rect, 0.0, fill);
    ui.painter().circle_filled(
        egui::pos2(rect.left() + 14.0, rect.top() + 23.0),
        3.5,
        display_color(row, ui.visuals()),
    );
    let text_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 27.0, rect.top() + 10.0),
        egui::pos2(rect.right() - 12.0, rect.bottom() - 8.0),
    );
    let mut text = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(text_rect)
            .id_salt((row.agent_session_id, row.turn_key)),
    );
    text.set_clip_rect(text_rect.intersect(ui.clip_rect()));
    text.add(
        egui::Label::new(egui::RichText::new(collapsed_summary_line(row.instruction)).strong())
            .truncate(),
    );
    text.add(
        egui::Label::new(
            egui::RichText::new(format!(
                "{} · {} · {} · {}",
                session_label(row, catalog),
                row.kind,
                display_state(row, catalog),
                local_clock(started_at(row))
            ))
            .small()
            .weak(),
        )
        .truncate(),
    );
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
        let button = ui.add_enabled(reason.is_none(), egui::Button::new(catalog.t(key, &[])));
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
        .map(|title| format!("{title} · {id}"))
        .unwrap_or(id)
}
pub(super) fn display_state(row: &WorkHistoryRow<'_>, catalog: &i18n::Catalog) -> String {
    row.current_state
        .map(|s| state_label(s, catalog))
        .unwrap_or_else(|| catalog.t("history.state.unknown", &[]))
}
pub(super) fn display_color(row: &WorkHistoryRow<'_>, visuals: &egui::Visuals) -> egui::Color32 {
    let tokens = crate::ui::designall::tokens(visuals);
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
        h.get_by_value("워크스페이스 전체").click();
        h.run();
        h.get_by_label("현재 세션").click();
        h.run();
        assert_eq!(h.state().view.browser.pane.as_deref(), Some("pane-a"));
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
        h.get_by_value("워크스페이스 전체").click();
        h.run();
        h.get_by_label("현재 세션").click();
        h.run();
        assert_eq!(h.state().view.browser.pane.as_deref(), Some("pane-a"));
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
        let footer = h.get_by_label(&footer_label);
        assert!(
            footer.rect().bottom() <= 720.0,
            "long detail must not grow the outer view"
        );
    }

    #[test]
    #[ignore = "offscreen native evidence; does not launch the app"]
    fn approved_history_render_evidence() {
        let out = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/mockups/work-history-applied-assets");
        std::fs::create_dir_all(&out).unwrap();
        for (name, size) in [
            ("desktop", egui::vec2(1100.0, 720.0)),
            ("narrow-list", egui::vec2(390.0, 720.0)),
            ("narrow-detail", egui::vec2(390.0, 720.0)),
        ] {
            let mut h = harness(
                size,
                vec![
                    fixture("입력 전송 경로 점검", "pane-a", 1_791_441_900),
                    fixture("모바일 화면 정리", "pane-b", 1_791_441_000),
                ],
                1,
            );
            h.run();
            if name == "narrow-detail" {
                click_row(&mut h, "입력 전송 경로 점검");
            }
            h.remove_cursor();
            h.run();
            h.render()
                .unwrap()
                .save(out.join(format!("{name}.png")))
                .unwrap();
        }
    }
}
