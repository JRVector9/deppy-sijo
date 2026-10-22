//! Session grants and actual single-writer leases; consent never acquires a lease.
use super::{
    CloudAgent, layout,
    settings::{SettingsState, result_notice},
};
use agent_mcp::{ConnectionState, now};

pub(super) fn completion_status_facts(
    ui: &mut egui::Ui,
    catalog: &i18n::Catalog,
    status: &crate::cloud_agent::CompletionStatusView,
) {
    use crate::cloud_agent::completion::LocalCompletionState;
    use agent_mcp::CompletionDeliveryState;
    let local = match status.local {
        LocalCompletionState::AwaitingStart => "cloud.completion_awaiting_start",
        LocalCompletionState::Working => "cloud.completion_working",
        LocalCompletionState::Completed => "cloud.completion_completed",
        LocalCompletionState::Unknown => "cloud.completion_unknown",
        LocalCompletionState::Unsupported => "cloud.completion_unsupported",
        LocalCompletionState::Expired => "cloud.completion_expired",
        LocalCompletionState::Revoked => "cloud.completion_revoked",
        LocalCompletionState::Rejected => "cloud.completion_rejected",
    };
    let delivery = match status.delivery {
        CompletionDeliveryState::Waiting => "cloud.completion_delivery_waiting",
        CompletionDeliveryState::Ready => "cloud.completion_delivery_ready",
        CompletionDeliveryState::ReturnedUnacknowledged => "cloud.completion_delivery_returned",
        CompletionDeliveryState::Disconnected => "cloud.completion_delivery_disconnected",
        CompletionDeliveryState::Closed => "cloud.completion_delivery_closed",
    };
    layout::fact(
        ui,
        &catalog.t("cloud.completion_local", &[]),
        &catalog.t(local, &[]),
    );
    layout::fact(
        ui,
        &catalog.t("cloud.completion_delivery", &[]),
        &catalog.t(delivery, &[]),
    );
}

impl CloudAgent {
    pub(super) fn permission_settings(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        state: &mut SettingsState,
    ) {
        let connections = self.connection_infos();
        if !connections
            .iter()
            .any(|info| state.selected.as_deref() == Some(&info.principal.id))
        {
            state.selected = connections.first().map(|info| info.principal.id.clone());
        }
        ui.horizontal(|ui| {
            layout::title(ui, catalog.t("cloud.one.sessions", &[]));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let selected = state.selected.clone();
                let can_reclaim = self.targets.iter().any(|target| {
                    selected.as_deref().is_some_and(|id| {
                        self.control
                            .permission(id, &target.id, &target.generation)
                            .input
                    })
                });
                ui.add_enabled_ui(can_reclaim, |ui| {
                    if layout::button_named(
                        ui,
                        &catalog.t("cloud.one.reclaim_all", &[]),
                        &catalog.t("cloud.ui.reclaim_actor", &[]),
                    )
                    .clicked()
                        && let Some(id) = state.selected.clone()
                    {
                        for target in self.targets.clone() {
                            let permission = self.connection_permission(&id, &target);
                            if self
                                .control
                                .permission(&id, &target.id, &target.generation)
                                .input
                            {
                                result_notice(
                                    state,
                                    self.set_connection_access(
                                        &id,
                                        &target,
                                        permission.read,
                                        false,
                                    ),
                                    "cloud.control_taken",
                                );
                            }
                        }
                    }
                });
                let count = self
                    .targets
                    .iter()
                    .filter(|target| {
                        selected
                            .as_deref()
                            .is_some_and(|id| self.connection_permission(id, target).read)
                    })
                    .count();
                layout::hint(
                    ui,
                    catalog.t(
                        if count == 0 {
                            "cloud.one.no_sessions"
                        } else {
                            "cloud.one.session_count"
                        },
                        &[("count", &count.to_string())],
                    ),
                );
            });
        });
        ui.add_space(10.0);
        self.approval_settings(ui, catalog, state);
        let Some(info) = connections
            .iter()
            .find(|info| Some(&info.principal.id) == state.selected.as_ref())
        else {
            layout::hint(ui, catalog.t("cloud.ui.permission_no_connection", &[]));
            self.permission_guide(ui, catalog);
            return;
        };
        let active = info.state == ConnectionState::Active;
        let external_ready = self.connector_endpoint().is_some();
        if !active {
            layout::hint(ui, catalog.t("cloud.reauth_hint", &[]));
            ui.add_space(12.0);
        }
        egui::ScrollArea::both()
            .id_salt("cloud-permission-table")
            .max_height(200.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.set_min_width(layout::SESSION_TABLE_MIN);
                let width = ui.available_width().max(layout::SESSION_TABLE_MIN);
                let columns = [
                    width - layout::PERMISSION_COLUMN * 3.0 - layout::CONTROL_COLUMN,
                    layout::PERMISSION_COLUMN,
                    layout::PERMISSION_COLUMN,
                    layout::PERMISSION_COLUMN,
                    layout::CONTROL_COLUMN,
                ];
                let header = ui
                    .allocate_exact_size(egui::vec2(width, 34.0), egui::Sense::hover())
                    .0;
                let mut x = header.left();
                for (index, key) in [
                    "cloud.one.session_state",
                    "cloud.one.read",
                    "cloud.one.input",
                    "cloud.one.completion",
                    "cloud.one.control",
                ]
                .iter()
                .enumerate()
                {
                    let rect = egui::Rect::from_min_size(
                        egui::pos2(x, header.top()),
                        egui::vec2(columns[index], 34.0),
                    );
                    let response = layout::cell_label(
                        ui,
                        rect,
                        &catalog.t(key, &[]),
                        12.0,
                        layout::secondary(ui),
                        index != 0,
                    );
                    if index == 3 {
                        response.on_hover_text(catalog.t("cloud.completion_host_unverified", &[]));
                    }
                    x += columns[index];
                }
                layout::line(ui);
                for target in self.targets.clone() {
                    ui.push_id((&info.principal.id, &target.id, &target.generation), |ui| {
                        let rect = ui
                            .allocate_exact_size(
                                egui::vec2(width, layout::SESSION_ROW_HEIGHT),
                                egui::Sense::hover(),
                            )
                            .0;
                        let mut x = rect.left();
                        let mut cells = Vec::new();
                        for (index, w) in columns.iter().enumerate() {
                            let cell_rect = if matches!(index, 1..=3) {
                                egui::Rect::from_min_size(
                                    egui::pos2(
                                        x + (*w - layout::CONTROL_HEIGHT) / 2.0,
                                        rect.top()
                                            + (layout::SESSION_ROW_HEIGHT - layout::CONTROL_HEIGHT)
                                                / 2.0,
                                    ),
                                    egui::vec2(layout::CONTROL_HEIGHT, layout::CONTROL_HEIGHT),
                                )
                            } else {
                                egui::Rect::from_min_size(
                                    egui::pos2(x, rect.top()),
                                    egui::vec2(*w, layout::SESSION_ROW_HEIGHT),
                                )
                            };
                            let mut cell = ui.new_child(
                                egui::UiBuilder::new()
                                    .id_salt(("cloud-permission-cell", index))
                                    .max_rect(cell_rect)
                                    .layout(egui::Layout::top_down(egui::Align::Min)),
                            );
                            cell.spacing_mut().interact_size.y = 0.0;
                            cell.spacing_mut().item_spacing.y = 2.0;
                            cell.set_clip_rect(cell_rect.intersect(ui.clip_rect()));
                            cells.push(cell);
                            x += w;
                        }
                        cells[0].with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                            ui.add_space(5.0);
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(self.redact(&target.title)).size(13.0),
                                )
                                .truncate(),
                            );
                            let local = target.local_status_label(catalog);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                ui.spacing_mut().interact_size.y = 0.0;
                                let status_width = ui
                                    .painter()
                                    .layout_no_wrap(
                                        local.clone(),
                                        egui::FontId::proportional(12.0),
                                        layout::accent(ui),
                                    )
                                    .size()
                                    .x;
                                let metadata_width =
                                    (ui.available_width() - status_width - 4.0).max(0.0);
                                // 긴 워크스페이스·모델 이름도 현재 상태의 자리는 남긴다.
                                let metadata = self.redact(&format!(
                                    "{} · {} ·",
                                    target.workspace_name,
                                    target
                                        .agent_line
                                        .as_deref()
                                        .unwrap_or(&catalog.t("workspace.spawn.shell", &[]))
                                ));
                                let galley = ui.painter().layout_no_wrap(
                                    metadata.clone(),
                                    egui::FontId::proportional(12.0),
                                    layout::secondary(ui),
                                );
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(
                                        metadata_width.min(galley.size().x),
                                        galley.size().y,
                                    ),
                                    egui::Sense::hover(),
                                );
                                let mut prefix = layout::child(ui, rect, "cloud-session-metadata");
                                prefix
                                    .add(
                                        egui::Label::new(
                                            egui::RichText::new(&metadata)
                                                .size(12.0)
                                                .color(layout::secondary(ui)),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(&metadata);
                                ui.label(
                                    egui::RichText::new(&local)
                                        .size(12.0)
                                        .color(layout::accent(ui)),
                                )
                                .on_hover_text(target.local_observation_label(catalog));
                            });
                        });
                        let permission = self.connection_permission(&info.principal.id, &target);
                        let mut read = active && permission.read;
                        let mut input = active && permission.input;
                        // Ended targets retain their last screen and read consent.
                        // Permit revocation without opening a new ended-target grant.
                        let read_enabled =
                            active && (permission.read || (target.live && external_ready));
                        let read_response = cells[1]
                            .add_enabled_ui(read_enabled, |ui| {
                                ui.add_sized(
                                    egui::vec2(layout::CONTROL_HEIGHT, layout::CONTROL_HEIGHT),
                                    egui::Checkbox::without_text(&mut read),
                                )
                            })
                            .inner;
                        read_response.widget_info(|| {
                            egui::WidgetInfo::selected(
                                egui::WidgetType::Checkbox,
                                read_enabled,
                                read,
                                catalog.t("cloud.connection_read", &[]),
                            )
                        });
                        if read_response.changed() {
                            input &= read;
                            result_notice(
                                state,
                                self.set_connection_access(
                                    &info.principal.id,
                                    &target,
                                    read,
                                    input,
                                ),
                                "cloud.permission_saved",
                            );
                        }
                        let input_enabled = active
                            && target.live
                            && read
                            && info.principal.input_scope
                            && (input || external_ready);
                        let input_response = cells[2]
                            .add_enabled_ui(input_enabled, |ui| {
                                ui.add_sized(
                                    egui::vec2(layout::CONTROL_HEIGHT, layout::CONTROL_HEIGHT),
                                    egui::Checkbox::without_text(&mut input),
                                )
                            })
                            .inner;
                        input_response.widget_info(|| {
                            egui::WidgetInfo::selected(
                                egui::WidgetType::Checkbox,
                                input_enabled,
                                input,
                                catalog.t("cloud.connection_input", &[]),
                            )
                        });
                        if input_response.changed() {
                            result_notice(
                                state,
                                self.set_connection_access(
                                    &info.principal.id,
                                    &target,
                                    read,
                                    input,
                                ),
                                "cloud.permission_saved",
                            );
                        }
                        let supported =
                            target.provider == Some(crate::agent_surface::AgentProvider::Codex);
                        if supported {
                            let mut completion = active && read && permission.local_completion;
                            let completion_enabled =
                                active && target.live && read && (completion || external_ready);
                            let completion_response = cells[3]
                                .add_enabled_ui(completion_enabled, |ui| {
                                    ui.add_sized(
                                        egui::vec2(layout::CONTROL_HEIGHT, layout::CONTROL_HEIGHT),
                                        egui::Checkbox::without_text(&mut completion),
                                    )
                                })
                                .inner;
                            completion_response.widget_info(|| {
                                egui::WidgetInfo::selected(
                                    egui::WidgetType::Checkbox,
                                    completion_enabled,
                                    completion,
                                    catalog.t("cloud.connection_local_completion", &[]),
                                )
                            });
                            if completion_response.changed() {
                                result_notice(
                                    state,
                                    self.set_connection_local_completion(
                                        &info.principal.id,
                                        &target,
                                        completion,
                                    ),
                                    "cloud.permission_saved",
                                );
                            }
                        } else {
                            let rect = cells[3].max_rect();
                            layout::cell_label(
                                &mut cells[3],
                                rect,
                                &catalog.t("cloud.one.unsupported", &[]),
                                11.0,
                                layout::secondary(ui),
                                true,
                            );
                        }
                        let lease = self.connection_lease(&target);
                        let owner = lease
                            .as_ref()
                            .map(|lease| {
                                connections
                                    .iter()
                                    .find(|info| info.principal.id == lease.connection_id)
                                    .map_or_else(
                                        || self.redact(&lease.connection_id),
                                        |info| self.redact(&info.principal.name),
                                    )
                            })
                            .unwrap_or_else(|| catalog.t("cloud.lease_you", &[]));
                        let control_rect = cells[4].max_rect();
                        let reclaim = target.live && (input || lease.is_some());
                        let owner_rect = if reclaim {
                            egui::Rect::from_min_size(
                                control_rect.min,
                                egui::vec2(64.0, control_rect.height()),
                            )
                        } else {
                            control_rect
                        };
                        layout::cell_label(
                            &mut cells[4],
                            owner_rect,
                            &owner,
                            12.0,
                            ui.visuals().text_color(),
                            true,
                        );
                        if reclaim {
                            let rect = egui::Rect::from_min_size(
                                egui::pos2(
                                    control_rect.left() + 71.0,
                                    control_rect.center().y - 14.0,
                                ),
                                egui::vec2(61.0, 28.0),
                            );
                            let mut action =
                                layout::child(&mut cells[4], rect, "cloud-input-reclaim");
                            let response = action.add_sized(
                                rect.size(),
                                egui::Button::new(
                                    egui::RichText::new(catalog.t("cloud.one.reclaim", &[]))
                                        .size(12.0),
                                ),
                            );
                            response.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Button,
                                    true,
                                    catalog.t("cloud.session_take_control", &[]),
                                )
                            });
                            if response.clicked() {
                                self.take_session_control(&target);
                                state.notice = Some(("cloud.control_taken", None));
                            }
                        }
                        if let Some(lease) = &lease {
                            cells[4].response().on_hover_text(catalog.t(
                                "cloud.lease_expiry",
                                &[(
                                    "seconds",
                                    &lease.expires_at.saturating_sub(now()).to_string(),
                                )],
                            ));
                        }
                        if !target.live {
                            layout::hint(&mut cells[4], catalog.t("cloud.exited", &[]));
                        }
                        let bottom = if let Some(status) =
                            self.completion_status_for_target(&info.principal.id, &target)
                        {
                            let details = ui
                                .allocate_exact_size(egui::vec2(width, 68.0), egui::Sense::hover())
                                .0;
                            let mut child = layout::child(ui, details, "cloud-completion-status");
                            completion_status_facts(&mut child, catalog, &status);
                            details.bottom()
                        } else {
                            rect.bottom()
                        };
                        ui.painter().hline(
                            rect.x_range(),
                            bottom,
                            egui::Stroke::new(
                                1.0,
                                ui.visuals().widgets.noninteractive.bg_stroke.color,
                            ),
                        );
                    });
                }
            });
        self.permission_guide(ui, catalog);
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs(1));
    }
    fn permission_guide(&mut self, ui: &mut egui::Ui, c: &i18n::Catalog) {
        ui.add_space(14.0);
        egui::Frame::NONE
            .fill(ui.visuals().faint_bg_color)
            .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
            .corner_radius(layout::RADIUS)
            .inner_margin(egui::Margin::symmetric(14, 11))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().interact_size.y = 0.0;
                egui::Grid::new("cloud-permission-guide")
                    .num_columns(2)
                    .spacing(egui::vec2(12.0, 6.0))
                    .show(ui, |ui| {
                        for (label, description) in [
                            (
                                "cloud.one.guide_permissions",
                                "cloud.one.guide_permissions_text",
                            ),
                            ("cloud.one.completion", "cloud.one.guide_completion_text"),
                            ("cloud.one.control", "cloud.one.guide_control_text"),
                        ] {
                            ui.allocate_ui(egui::vec2(66.0, 18.0), |ui| {
                                ui.set_min_width(66.0);
                                ui.label(egui::RichText::new(c.t(label, &[])).size(12.0));
                            });
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(c.t(description, &[]))
                                        .size(12.0)
                                        .color(layout::secondary(ui)),
                                )
                                .wrap(),
                            );
                            ui.end_row();
                        }
                    });
            });
    }
}
