//! 승인된 단일 화면: 연결 정보·세션 권한은 같은 본문에서 관리한다.
use super::{
    Action, CloudAgent, connections, layout,
    settings::{self, SettingsState, Tab},
};
use agent_mcp::{AuthKind, ConnectionState};

impl CloudAgent {
    /// 평문은 직후 표시용 소유자에만 두며, 설정을 떠나면 버린다.
    pub(crate) fn clear_connection_token(&mut self) {
        self.displayed_connection_token = None;
    }

    pub(super) fn issue_display_token(
        &mut self,
        name: &str,
        input_scope: bool,
        replace: Option<&str>,
        state: &mut SettingsState,
    ) -> Result<(), String> {
        let (id, token) = self.issue_manual_connection(name, input_scope)?;
        // 발급 실패 때 기존 연결을 잃지 않도록 새 발급 성공 뒤 선택한 연결만 폐기한다.
        if let Some(old) = replace {
            self.revoke_connection(old);
        }
        self.displayed_connection_token = Some((id.clone(), token));
        state.selected = Some(id);
        state.token_hidden = false;
        state.adding = false;
        state.renew = None;
        // 발급 안내는 토큰 박스와 함께 사라지며, 닫은 뒤 복사를 안내하지 않는다.
        state.notice = None;
        Ok(())
    }

    pub(super) fn one_screen(
        &mut self,
        ui: &mut egui::Ui,
        c: &i18n::Catalog,
        state: &mut SettingsState,
        narrow: bool,
    ) {
        // 섹션 여백은 승인 시안의 값으로 한 번만 적용한다.
        ui.spacing_mut().item_spacing.y = 0.0;
        let reveal = state.tab;
        state.tab = Tab::Connections;
        let connections = self.connection_infos();
        if !connections
            .iter()
            .any(|info| state.selected.as_deref() == Some(&info.principal.id))
        {
            state.selected = connections
                .iter()
                .find(|info| info.principal.id != agent_mcp::LEGACY_CONNECTION_ID)
                .or_else(|| connections.first())
                .map(|info| info.principal.id.clone());
        }
        let before = state.selected.clone();
        let (service_key, service_color) = self.service_fact(ui);
        let service_text = c.t(service_key, &[]);
        let service_width = ui
            .painter()
            .layout_no_wrap(
                service_text.clone(),
                egui::FontId::proportional(12.0),
                service_color,
            )
            .size()
            .x
            .min(ui.available_width() * 0.4);
        let heading_width = (ui.available_width() - service_width - 14.0).max(0.0);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            ui.allocate_ui_with_layout(
                egui::vec2(heading_width, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    // allocate_ui는 내용 폭만 소비하므로 제목 열의 남은 공간도 유지한다.
                    ui.set_width(heading_width);
                    ui.spacing_mut().item_spacing.y = 5.0;
                    ui.label(
                        egui::RichText::new(c.t("cloud.one.title", &[]))
                            .size(15.0)
                            .strong(),
                    );
                    layout::hint(ui, c.t("cloud.one.intro", &[]));
                },
            );
            ui.allocate_ui_with_layout(
                egui::vec2(service_width, 0.0),
                egui::Layout::right_to_left(egui::Align::Min),
                |ui| {
                    let status = ui.add(
                        egui::Label::new(
                            egui::RichText::new(service_text)
                                .size(12.0)
                                .color(service_color),
                        )
                        .wrap(),
                    );
                    ui.painter().circle_filled(
                        egui::pos2(status.rect.left() - 10.0, status.rect.center().y),
                        3.0,
                        service_color,
                    );
                },
            );
        });
        ui.add_space(12.0);
        ui.horizontal_wrapped(|ui| {
            if !narrow {
                layout::hint(ui, c.t("cloud.one.connection", &[]));
            }
            let selected_name = connections
                .iter()
                .find(|info| Some(&info.principal.id) == state.selected.as_ref())
                .map_or_else(
                    || c.t("cloud.ui.select_connection", &[]),
                    |info| self.redact(&info.principal.name),
                );
            let selector_width = if narrow { 148.0 } else { 185.0 };
            // ComboBox::width는 최소폭이므로 별도 가용 폭과 말줄임을 함께 제한한다.
            ui.allocate_ui_with_layout(
                egui::vec2(selector_width, layout::CONTROL_HEIGHT),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    egui::ComboBox::from_id_salt("cloud-selected-connection")
                        .height(240.0)
                        .width(selector_width)
                        .truncate()
                        .selected_text(&selected_name)
                        .show_ui(ui, |ui| {
                            for info in &connections {
                                ui.selectable_value(
                                    &mut state.selected,
                                    Some(info.principal.id.clone()),
                                    self.redact(&info.principal.name),
                                );
                            }
                        })
                        .response
                        .on_hover_text(selected_name);
                },
            );
            if layout::button_named(
                ui,
                &format!("＋ {}", c.t("cloud.ui.add", &[])),
                &c.t("cloud.ui.add", &[]),
            )
            .clicked()
            {
                state.adding = true;
                state.manual_name.clear();
                state.notice = None;
            }
            if let Some(info) = connections
                .iter()
                .find(|info| Some(&info.principal.id) == state.selected.as_ref())
            {
                let text = c.t(connections::short_key(info), &[]);
                let color = connections::color(ui, info);
                let text_width = ui
                    .painter()
                    .layout_no_wrap(text.clone(), egui::FontId::proportional(13.0), color)
                    .size()
                    .x;
                if ui.available_size_before_wrap().x < text_width {
                    ui.end_row();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(text).size(13.0).color(color));
                });
            }
        });
        if before != state.selected {
            state.renew = None;
            state.notice = None;
            state.checking = false;
            state.editing = false;
        }
        if self
            .displayed_connection_token
            .as_ref()
            .is_some_and(|(id, _)| {
                Some(id) != state.selected.as_ref()
                    || !connections.iter().any(|info| {
                        &info.principal.id == id && info.state == ConnectionState::Active
                    })
            })
        {
            self.clear_connection_token();
        }
        ui.add_space(12.0);
        layout::line(ui);
        if state.adding {
            ui.add_space(12.0);
            ui.horizontal_wrapped(|ui| {
                let label = ui.label(c.t("cloud.connection_name", &[]));
                layout::text_input(
                    ui,
                    220.0_f32.min(ui.available_width()),
                    egui::TextEdit::singleline(&mut state.manual_name).char_limit(128),
                )
                .labelled_by(label.id);
                ui.add_enabled_ui(
                    self.connector_endpoint().is_some() && !state.manual_name.trim().is_empty(),
                    |ui| {
                        if layout::primary(ui, &c.t("cloud.one.issue", &[])).clicked() {
                            let name = state.manual_name.clone();
                            // 토큰 scope는 입력 기능을 제공하되 모든 세션 동의는 기본 OFF다.
                            let result = self.issue_display_token(&name, true, None, state);
                            if let Err(code) = result {
                                state.notice = Some(("cloud.management_failed", Some(code)));
                            }
                        }
                    },
                );
                if layout::button(ui, &c.t("action.cancel", &[])).clicked() {
                    state.adding = false;
                }
            });
        }
        if self.connector_endpoint().is_none() {
            ui.add_space(14.0);
            layout::notice(ui, layout::warn(ui), |ui| {
                layout::hint(
                    ui,
                    c.t(
                        if self.error.is_some() {
                            "cloud.auto_failed"
                        } else {
                            "cloud.one.prepare"
                        },
                        &[],
                    ),
                );
                ui.add_enabled_ui(!self.busy(), |ui| {
                    if layout::button_named(
                        ui,
                        &c.t("cloud.ui.retry", &[]),
                        &c.t("cloud.start", &[]),
                    )
                    .clicked()
                    {
                        self.action = Some(Action::Start);
                    }
                });
            });
        }
        ui.add_space(14.0);
        self.connection_facts(ui, c, state, narrow);
        ui.add_space(14.0);
        layout::line(ui);
        ui.add_space(14.0);
        let permissions = ui.scope(|ui| self.permission_settings(ui, c, state));
        if reveal == Tab::Permissions {
            permissions.response.scroll_to_me(Some(egui::Align::Min));
        }
        ui.add_space(14.0);
        layout::line(ui);
        ui.add_space(10.0);
        // 기존 고급 관리도 별도 창으로 이동하지 않고 같은 본문에 유지한다.
        egui::CollapsingHeader::new(c.t("cloud.one.more", &[]))
            .id_salt("cloud-one-more")
            .open(matches!(reveal, Tab::Advanced | Tab::Work).then_some(true))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                layout::hint(ui, c.t("cloud.ui.setup_steps", &[]));
                layout::hint(ui, c.t("cloud.client_setup_hint", &[]));
                if layout::button(ui, &c.t("cloud.copy_instruction", &[])).clicked() {
                    ui.ctx().copy_text(super::CLOUD_AGENT_INSTRUCTIONS.into());
                }
                egui::CollapsingHeader::new(c.t("cloud.ui.endpoint_settings", &[]))
                    .id_salt("cloud-one-endpoint")
                    .open((reveal == Tab::Advanced).then_some(true))
                    .show(ui, |ui| self.advanced_settings(ui, c, state));
                if let Some(info) = self
                    .connection_infos()
                    .iter()
                    .find(|info| Some(&info.principal.id) == state.selected.as_ref())
                    .cloned()
                {
                    egui::CollapsingHeader::new(c.t("cloud.ui.manage_details", &[]))
                        .id_salt("cloud-one-management")
                        .show(ui, |ui| self.connection_detail(ui, c, state, &info));
                }
                if layout::button(ui, &c.t("cloud.take_control", &[])).clicked() {
                    self.take_control();
                    state.notice = Some(("cloud.control_taken", None));
                }
                let work = egui::CollapsingHeader::new(c.t("cloud.ui.work", &[]))
                    .id_salt("cloud-one-work")
                    .open((reveal == Tab::Work).then_some(true))
                    .show(ui, |ui| self.work_settings(ui, c, state));
                if reveal == Tab::Work {
                    work.header_response.scroll_to_me(Some(egui::Align::Min));
                }
            });
        ui.add_space(if narrow { 13.0 } else { 12.0 });
        let selected = self
            .connection_infos()
            .into_iter()
            .find(|info| Some(&info.principal.id) == state.selected.as_ref());
        let evidence = |ui: &mut egui::Ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            if let Some(info) = &selected {
                ui.label(egui::RichText::new(c.t(connections::activity_key(info), &[])).size(12.0));
            } else {
                layout::hint(ui, c.t("cloud.one.no_auth", &[]));
            }
            ui.add(
                egui::Label::new(
                    egui::RichText::new(c.t("cloud.one.check_hint", &[]))
                        .size(12.0)
                        .color(layout::secondary(ui)),
                )
                .wrap(),
            );
        };
        if narrow {
            ui.vertical(evidence);
            ui.add_space(9.0);
        }
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if layout::button(ui, &c.t("cloud.one.check", &[])).clicked() {
                    state.checking = true;
                }
                if !narrow {
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), 48.0),
                        egui::Layout::top_down(egui::Align::Min),
                        evidence,
                    );
                }
            });
        });
        if narrow {
            ui.add_space(13.0);
        }
        if state.checking {
            layout::hint(
                ui,
                c.t(
                    selected
                        .as_ref()
                        .map_or("cloud.one.no_auth", connections::activity_key),
                    &[],
                ),
            );
        }
        if state.notice.is_some() {
            settings::notice(ui, c, state);
        }
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs(1));
    }

    fn service_fact(&self, ui: &egui::Ui) -> (&'static str, egui::Color32) {
        use crate::cloud_agent::Connection;
        match self.connection {
            Connection::Ready if self.automatic && self.connector_endpoint().is_some() => {
                ("cloud.one.ready", layout::good(ui))
            }
            Connection::Ready if self.connector_endpoint().is_some() => {
                ("cloud.manual_unverified", layout::warn(ui))
            }
            Connection::Preparing => ("cloud.preparing", layout::warn(ui)),
            Connection::Verifying => ("cloud.verifying", layout::warn(ui)),
            Connection::Stopping => ("cloud.stopping", layout::secondary(ui)),
            _ => ("cloud.ui.service_stopped", layout::secondary(ui)),
        }
    }

    fn connection_facts(
        &mut self,
        ui: &mut egui::Ui,
        c: &i18n::Catalog,
        state: &mut SettingsState,
        narrow: bool,
    ) {
        let endpoint = self.connector_endpoint();
        let info = self
            .connection_infos()
            .into_iter()
            .find(|info| Some(&info.principal.id) == state.selected.as_ref());
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_enabled_ui(
                    endpoint.is_some() && self.displayed_connection_token.is_some(),
                    |ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new(c.t("cloud.one.copy_all", &[])).size(13.0),
                                )
                                .min_size(egui::vec2(0.0, 34.0)),
                            )
                            .clicked()
                            && let (Some(endpoint), Some((_, token))) =
                                (&endpoint, &self.displayed_connection_token)
                        {
                            ui.ctx().copy_text(format!(
                                "MCP URL: {endpoint}\nAuthorization: Bearer {}",
                                token.expose()
                            ));
                            state.notice = Some(("cloud.one.copied", None));
                        }
                    },
                );
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), layout::CONTROL_HEIGHT),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(c.t("cloud.one.details", &[]))
                                    .size(14.0)
                                    .strong(),
                            )
                            .wrap(),
                        );
                    },
                );
            });
        });
        ui.add_space(10.0);
        egui::Frame::NONE
            .fill(ui.visuals().faint_bg_color)
            .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
            .corner_radius(layout::RADIUS)
            .inner_margin(egui::Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                fact_row(
                    ui,
                    narrow,
                    &c.t("cloud.one.address", &[]),
                    Some(&c.t("cloud.one.generated", &[])),
                    |ui| {
                        ui.horizontal(|ui| {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.add_enabled_ui(endpoint.is_some(), |ui| {
                                        if layout::button_named(
                                            ui,
                                            &c.t("action.copy", &[]),
                                            &c.t("cloud.copy_url", &[]),
                                        )
                                        .clicked()
                                            && let Some(endpoint) = &endpoint
                                        {
                                            ui.ctx().copy_text(endpoint.clone());
                                            state.notice = Some(("cloud.one.copied", None));
                                        }
                                    });
                                    readout_value(
                                        ui,
                                        endpoint
                                            .as_deref()
                                            .unwrap_or(&c.t("cloud.one.preparing", &[])),
                                    );
                                },
                            );
                        });
                    },
                );
                ui.add_space(8.0);
                fact_row(ui, narrow, &c.t("cloud.one.token", &[]), None, |ui| {
                    let narrow = ui.available_width() < 480.0;
                    let token = self
                        .displayed_connection_token
                        .as_ref()
                        .map(|(_, token)| token.expose());
                    let oauth = info
                        .as_ref()
                        .is_some_and(|info| info.principal.auth_kind == AuthKind::OAuth);
                    if narrow {
                        token_readout(ui, c, token, state.token_hidden, oauth);
                    }
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if !oauth {
                                ui.add_enabled_ui(endpoint.is_some() && info.is_some(), |ui| {
                                    if layout::button(ui, &c.t("cloud.one.new_token", &[]))
                                        .clicked()
                                    {
                                        state.renew = state.selected.clone();
                                    }
                                });
                            }
                            if let Some(token) = token {
                                ui.add_enabled_ui(endpoint.is_some(), |ui| {
                                    if layout::button_named(
                                        ui,
                                        &c.t("action.copy", &[]),
                                        &c.t("cloud.copy_token", &[]),
                                    )
                                    .clicked()
                                    {
                                        ui.ctx().copy_text(token.to_owned());
                                        state.notice = Some(("cloud.one.copied", None));
                                    }
                                });
                                if layout::button(
                                    ui,
                                    &c.t(
                                        if state.token_hidden {
                                            "cloud.one.show"
                                        } else {
                                            "cloud.one.hide"
                                        },
                                        &[],
                                    ),
                                )
                                .clicked()
                                {
                                    state.token_hidden = !state.token_hidden;
                                }
                            }
                            if !narrow {
                                token_readout(ui, c, token, state.token_hidden, oauth);
                            }
                        });
                    });
                });
                if self.displayed_connection_token.is_some() {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().interact_size.y = 20.0;
                        if !narrow {
                            ui.add_space(104.0);
                        }
                        let mut note = egui::text::LayoutJob::default();
                        for (key, color) in [
                            ("cloud.one.token_note_status", layout::good(ui)),
                            ("cloud.one.token_note_hint", layout::secondary(ui)),
                        ] {
                            note.append(
                                &format!(
                                    "{}{}",
                                    if note.text.is_empty() { "" } else { " " },
                                    c.t(key, &[])
                                ),
                                0.0,
                                egui::TextFormat {
                                    font_id: egui::FontId::proportional(12.0),
                                    color,
                                    ..Default::default()
                                },
                            );
                        }
                        ui.add(egui::Label::new(note).wrap());
                    });
                }
            });
        if let Some(info) = info.filter(|info| state.renew.as_ref() == Some(&info.principal.id)) {
            ui.add_space(10.0);
            let message = c.t(
                "cloud.one.renew_confirm",
                &[("name", &self.redact(&info.principal.name))],
            );
            let mut confirmed = false;
            let mut cancelled = false;
            layout::notice(ui, layout::warn(ui), |ui| {
                if narrow {
                    layout::hint(ui, &message);
                    ui.add_space(8.0);
                    ui.horizontal_wrapped(|ui| {
                        confirmed =
                            layout::primary(ui, &c.t("cloud.one.issue_confirm", &[])).clicked();
                        cancelled = layout::button(ui, &c.t("action.cancel", &[])).clicked();
                    });
                } else {
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            cancelled = layout::button(ui, &c.t("action.cancel", &[])).clicked();
                            confirmed =
                                layout::primary(ui, &c.t("cloud.one.issue_confirm", &[])).clicked();
                            ui.allocate_ui_with_layout(
                                egui::vec2(ui.available_width(), layout::CONTROL_HEIGHT),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    layout::hint(ui, &message);
                                },
                            );
                        });
                    });
                }
            });
            if confirmed {
                let result = self.issue_display_token(
                    &info.principal.name,
                    info.principal.input_scope,
                    Some(&info.principal.id),
                    state,
                );
                if let Err(code) = result {
                    state.notice = Some(("cloud.management_failed", Some(code)));
                }
            }
            if cancelled {
                state.renew = None;
            }
        }
    }
}

fn fact_row(
    ui: &mut egui::Ui,
    narrow: bool,
    label: &str,
    note: Option<&str>,
    contents: impl FnOnce(&mut egui::Ui),
) {
    let width = ui.available_width();
    if narrow {
        ui.horizontal(|ui| {
            ui.spacing_mut().interact_size.y = 0.0;
            layout::hint(ui, label);
            if let Some(note) = note {
                ui.label(
                    egui::RichText::new(note)
                        .size(11.0)
                        .color(layout::secondary(ui)),
                );
            }
        });
        ui.add_space(5.0);
        contents(ui);
    } else {
        let rect = ui
            .allocate_exact_size(egui::vec2(width, 34.0), egui::Sense::hover())
            .0;
        let mut label_ui = layout::child(
            ui,
            egui::Rect::from_min_size(rect.min, egui::vec2(90.0, 34.0)),
            ("cloud-fact-label", label),
        );
        label_ui.spacing_mut().item_spacing.y = 0.0;
        label_ui.spacing_mut().interact_size.y = 0.0;
        layout::hint(&mut label_ui, label);
        if let Some(note) = note {
            label_ui.label(
                egui::RichText::new(note)
                    .size(11.0)
                    .color(layout::secondary(ui)),
            );
        }
        let value_rect = egui::Rect::from_min_max(rect.min + egui::vec2(104.0, 0.0), rect.max);
        let mut value_ui = layout::child(ui, value_rect, ("cloud-fact-value", label));
        value_ui.set_clip_rect(
            egui::Rect::from_x_y_ranges(value_rect.x_range(), ui.clip_rect().y_range())
                .intersect(ui.clip_rect()),
        );
        contents(&mut value_ui);
        if value_ui.min_rect().bottom() > rect.bottom() {
            ui.allocate_rect(
                egui::Rect::from_min_max(
                    rect.min,
                    egui::pos2(rect.right(), value_ui.min_rect().bottom()),
                ),
                egui::Sense::hover(),
            );
        }
    }
}

fn token_readout(
    ui: &mut egui::Ui,
    c: &i18n::Catalog,
    token: Option<&str>,
    hidden: bool,
    oauth: bool,
) {
    if let Some(token) = token {
        readout_value(
            ui,
            if hidden {
                "••••••••••••••••"
            } else {
                token
            },
        );
    } else {
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), 34.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                layout::hint(
                    ui,
                    c.t(
                        if oauth {
                            "cloud.one.oauth"
                        } else {
                            "cloud.one.unavailable"
                        },
                        &[],
                    ),
                );
            },
        );
    }
}

fn readout_value(ui: &mut egui::Ui, text: &str) {
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), 34.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(text)
                        .monospace()
                        .size(13.0)
                        .color(layout::secondary(ui)),
                )
                .selectable(true)
                .truncate(),
            );
        },
    );
}
