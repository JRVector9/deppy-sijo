//! Settings leaf: captures owner intents; durable operations use the App controller.
use super::{Action, CloudAgent};
mod advanced;
mod connections;
mod history;
mod layout;
mod permissions;
mod runs;
mod settings;

const CLOUD_AGENT_INSTRUCTIONS: &str = concat!(
    "Use Deppy's MCP connector. First call list_sessions and keep the exact original session_id/generation for every call, even when Deppy's active tab changes. ",
    "Read the selected session with read_output; its cached screen may be stale, and a quiet screen or screen:null does not prove completion. Respect retry_after_ms and cursor/reset. ",
    "Do not send terminal input unless input_allowed is true. Use send_text for a single line without controls (maximum 8192 UTF-8 bytes); submit=false only types and submit=true sends Enter. ",
    "Use paste_text for multiline text or tabs only when paste_bracketed and paste_ai_confirmed are true. Its maximum is 32768 UTF-8 bytes; serialized arguments and the complete HTTP JSON body each have a 65536-byte limit, so JSON escaping can reject earlier. LF/CRLF and tabs are allowed with verified bracketed paste; standalone CR, ESC and other controls are rejected. submit defaults false; true admits the body and one separate Enter together. Respect existing AI drafts/dialogs and no-effect rejections; never bypass them. ",
    "Use send_ctrl_c only for an explicitly authorized interrupt. Use a unique operation_id per action; retry the exact same tool and arguments only with the same operation_id. A queued receipt confirms PTY queue admission, not command execution or task completion. If delivery is unknown, do not automatically replay input or create a new ID. ",
    "After every completed analysis/task, call notify with YOUR OWN complete final answer (maximum 16384 UTF-8 bytes), including when no terminal input was sent. A stored receipt confirms the answer was saved. Keep the original session_id/generation for this answer. Limits count UTF-8 bytes, not characters. ",
    "New connection identities require explicit owner consent for reading and separately for input; authentication alone grants neither. Legacy tools remain available. For tracked work, call start_run with a unique run_id, title and optional instruction (8192 UTF-8 bytes); registration is not completion. Keep the exact original session_id/generation and run_id. Before input from a new connection, call acquire_control for the approved session with run_id when applicable and ttl_seconds of 1..120; check the lease and renew before it expires. Never invent a connection identity or borrow another connection's lease. Use release_control when finished. ",
    "Report work through update_run: queued/running/waiting/completed/failed/cancelled, optional progress 0..100, message (4096 UTF-8 bytes), result (16384 UTF-8 bytes), and up to 16 actually produced HTTP(S) artifact links. Progress is your report, not independent verification. Send heartbeat updates while working; missing updates become unknown, never automatically completed. Use get_run/list_runs to check cancellation requests, stop your own work and acknowledge with update_run state=cancelled. A cancellation request is not Ctrl+C or input permission revocation. Include run_id on input and notify when tracking work; still send your own final answer to the original session. ",
    "After a lost receipt, use get_operation with the original session_id/generation and operation_id to inspect the existing result without repeating input. Unknown delivery must not be replayed. Use read_history with the original session_id/generation, cursor and max_bytes at most 65536; handle reset/truncation and observation time, respect polling hints, and keep reads bounded."
);

#[cfg(test)]
mod management_tests;
#[cfg(test)]
mod redesign_tests;

impl CloudAgent {
    pub fn contents(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        use settings::{SettingsState, Tab};
        ui.scope(|ui| {
            crate::ui::settings::apply_component_style(ui);
            ui.style_mut().override_font_id = Some(egui::FontId::proportional(14.0));
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
            layout::apply_palette(ui);
            let mut state: SettingsState = settings::state(ui);
            if self.selected_record.is_some() {
                state.tab = Tab::Work;
                state.work_kind = settings::WorkKind::Answers;
            }
            let rect = ui.available_rect_before_wrap();
            ui.painter()
                .rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);
            ui.allocate_rect(rect, egui::Sense::hover());
            let head = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), 80.0));
            let mut header = layout::child(ui, head, "cloud-page-header");
            egui::Frame::NONE
                .inner_margin(egui::Margin {
                    left: 26,
                    right: 26,
                    top: 18,
                    bottom: 15,
                })
                .show(&mut header, |ui| {
                    ui.scope(|ui| {
                        ui.spacing_mut().interact_size.y = 18.0;
                        ui.horizontal(|ui| {
                            layout::hint(ui, catalog.t("cloud.ui.breadcrumb", &[]));
                            ui.label(
                                egui::RichText::new(catalog.t("cloud.ui.title", &[]))
                                    .size(15.0)
                                    .strong(),
                            );
                        });
                    });
                    layout::hint(ui, catalog.t("cloud.ui.intro", &[]));
                });
            ui.painter().hline(
                head.x_range(),
                head.bottom(),
                egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
            );
            let tabs_rect = egui::Rect::from_min_size(
                egui::pos2(rect.left(), head.bottom()),
                egui::vec2(rect.width(), layout::TAB_HEIGHT),
            );
            let mut tabs = layout::child(ui, tabs_rect, "cloud-settings-tabs");
            egui::Frame::NONE
                .inner_margin(egui::Margin::symmetric(26, 0))
                .show(&mut tabs, |ui| {
                    ui.spacing_mut().item_spacing.x = 22.0;
                    ui.horizontal(|ui| {
                        let mut ids = Vec::new();
                        let mut focused = None;
                        for tab in Tab::ALL {
                            let text = catalog.t(tab.key(), &[]);
                            let display = if tab == Tab::Connections {
                                format!("{}  {}", text, self.connection_infos().len())
                            } else {
                                text.clone()
                            };
                            let selected = tab == state.tab;
                            let response = ui.add(
                                egui::Button::new(egui::RichText::new(&display).size(14.0).color(
                                    if selected {
                                        layout::accent(ui)
                                    } else {
                                        layout::secondary(ui)
                                    },
                                ))
                                .frame(false)
                                .min_size(egui::vec2(0.0, layout::TAB_HEIGHT - 1.0)),
                            );
                            ui.ctx().accesskit_node_builder(response.id, |node| {
                                node.set_role(egui::accesskit::Role::Tab);
                                node.set_label(text.clone());
                                node.set_selected(selected);
                            });
                            if selected {
                                ui.painter().hline(
                                    response.rect.x_range(),
                                    tabs_rect.bottom() - 1.0,
                                    egui::Stroke::new(2.0, layout::accent(ui)),
                                );
                            }
                            ids.push(response.id);
                            if response.has_focus() {
                                focused = Some(tab);
                            }
                            if response.clicked() {
                                state.tab = tab;
                                response.request_focus();
                            }
                        }
                        if let Some(focused) = focused {
                            let index =
                                Tab::ALL.iter().position(|tab| *tab == focused).unwrap_or(0);
                            let next = ui.input_mut(|input| {
                                if input.consume_key(egui::Modifiers::NONE, egui::Key::Home) {
                                    Some(0)
                                } else if input.consume_key(egui::Modifiers::NONE, egui::Key::End) {
                                    Some(3)
                                } else if input
                                    .consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight)
                                {
                                    Some((index + 1) % 4)
                                } else if input
                                    .consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft)
                                {
                                    Some((index + 3) % 4)
                                } else {
                                    None
                                }
                            });
                            if let Some(next) = next {
                                state.tab = Tab::ALL[next];
                                ui.memory_mut(|memory| {
                                    // egui also queues geometric arrow navigation at begin_pass.
                                    // This tab strip owns that navigation; avoid a second move.
                                    memory.move_focus(egui::FocusDirection::None);
                                    memory.request_focus(ids[next]);
                                });
                                ui.ctx().request_repaint();
                            }
                        }
                    });
                });
            ui.painter().hline(
                tabs_rect.x_range(),
                tabs_rect.bottom(),
                egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
            );
            let footer = egui::Rect::from_min_max(
                egui::pos2(
                    rect.left(),
                    (rect.bottom() - layout::FOOTER_HEIGHT).max(tabs_rect.bottom()),
                ),
                rect.max,
            );
            let body = egui::Rect::from_min_max(
                egui::pos2(rect.left(), tabs_rect.bottom()),
                footer.min + egui::vec2(rect.width(), 0.0),
            );
            let mut content = layout::child(ui, body, ("cloud-settings-body", state.tab));
            egui::ScrollArea::vertical()
                .id_salt(("cloud-settings-scroll", state.tab))
                .auto_shrink([false, false])
                .show(&mut content, |ui| {
                    layout::padded(ui, |ui| match state.tab {
                        Tab::Connections => self.connection_settings(ui, catalog, &mut state),
                        Tab::Permissions => self.permission_settings(ui, catalog, &mut state),
                        Tab::Work => self.work_settings(ui, catalog, &mut state),
                        Tab::Advanced => self.advanced_settings(ui, catalog, &mut state),
                    });
                });
            ui.painter()
                .rect_filled(footer, 0.0, ui.visuals().faint_bg_color);
            ui.painter().hline(
                footer.x_range(),
                footer.top(),
                egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
            );
            let mut feedback = layout::child(
                ui,
                footer.shrink2(egui::vec2(26.0, 8.0)),
                "cloud-settings-feedback",
            );
            settings::notice(&mut feedback, catalog, &state);
            settings::store(ui, state);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable as _;
    fn harness(bridge: CloudAgent) -> egui_kittest::Harness<'static, CloudAgent> {
        let catalog = i18n::Catalog::load("en-US").unwrap();
        egui_kittest::Harness::builder()
            .with_size(egui::vec2(1090.0, 1200.0))
            .build_ui_state(
                move |ui, bridge: &mut CloudAgent| {
                    bridge.contents(ui, &catalog);
                    let copy = ui.ctx().output(|output| {
                        output.commands.iter().find_map(|command| {
                            if let egui::OutputCommand::CopyText(text) = command {
                                Some(text.clone())
                            } else {
                                None
                            }
                        })
                    });
                    if let Some(copy) = copy {
                        ui.ctx().data_mut(|data| {
                            data.insert_temp(egui::Id::new("test-cloud-instruction-copy"), copy)
                        });
                    }
                },
                bridge,
            )
    }
    #[test]
    fn setup_state_local_listener_does_not_claim_public_or_client_connection() {
        let mut bridge = CloudAgent::memory();
        bridge.server =
            Some(agent_mcp::Server::start(0, "starting.trycloudflare.com", || {}).unwrap());
        bridge.connection = super::super::Connection::Verifying;
        let mut h = harness(bridge);
        h.get_by_label("Verifying public URL…");
        assert!(h.query_by_label("Public MCP URL verified").is_none());
        h.get_by_label("Connection information").click();
        h.run();
        assert!(h.query_by_label("Copy URL").is_none());
    }
    #[test]
    fn setup_state_public_readiness_keeps_client_registration_separate() {
        let mut bridge = CloudAgent::memory();
        bridge.server =
            Some(agent_mcp::Server::start(0, "ready.trycloudflare.com", || {}).unwrap());
        bridge.generated_hostname = Some("ready.trycloudflare.com".into());
        bridge.connection = super::super::Connection::Ready;
        let mut h = harness(bridge);
        h.get_by_label("Public MCP URL verified");
        h.get_by_label("Connection information").click();
        h.run();
        h.get_by_label("https://ready.trycloudflare.com/mcp");
        h.get_by_label("Copy URL");
        assert!(h.query_by_label("MCP connected").is_none());
    }
    #[test]
    fn setup_state_manual_endpoint_does_not_claim_public_verification() {
        let mut bridge = CloudAgent::memory();
        bridge.automatic = false;
        bridge.hostname = "deppy.example.com".into();
        bridge.server = Some(agent_mcp::Server::start(0, &bridge.hostname, || {}).unwrap());
        bridge.connection = super::super::Connection::Ready;
        let mut h = harness(bridge);
        h.get_by_label("Fixed public URL configured · check your tunnel and connector");
        h.get_by_label("Connection information").click();
        h.run();
        h.get_by_label("https://deppy.example.com/mcp");
        assert!(h.query_by_label("Public MCP URL verified").is_none());
    }
    #[test]
    fn setup_state_manual_local_only_preserves_local_url_and_token_controls() {
        let mut bridge = CloudAgent::memory();
        bridge.automatic = false;
        bridge.server = Some(agent_mcp::Server::start(0, "", || {}).unwrap());
        bridge.connection = super::super::Connection::Ready;
        let url = format!("http://{}/mcp", bridge.server.as_ref().unwrap().addr);
        let mut h = harness(bridge);
        h.get_by_label("Connection information").click();
        h.run();
        h.get_by_label(&url);
        h.get_by_label("Copy URL");
        h.get_by_label("Advanced settings").click();
        h.run();
        h.get_by_label("Shared token · ended sessions · agent instructions")
            .click();
        h.run();
        h.get_by_label("Copy token");
    }
    #[test]
    fn setup_state_copy_instruction_delivers_multiline_bounds_and_receipt_contract() {
        let mut h = harness(CloudAgent::memory());
        h.get_by_label("Add first connection").click();
        h.run();
        h.get_by_label("Instructions for the agent").click();
        h.run();
        h.get_by_label("Copy agent instructions").click();
        h.run();
        // Controller-free UI copy command contains the original complete contract.
        for text in [
            "paste_text",
            "UTF-8 bytes",
            "8192",
            "32768",
            "16384",
            "65536",
            "same operation_id",
            "queued",
            "stored",
            "unknown",
            "notify",
            "original session_id/generation",
            "start_run",
            "acquire_control",
            "release_control",
            "update_run",
            "get_operation",
            "read_history",
            "explicit owner consent",
        ] {
            assert!(
                h.ctx
                    .data_mut(|data| data
                        .get_temp::<String>(egui::Id::new("test-cloud-instruction-copy")))
                    .expect("actual copy command")
                    .contains(text)
            );
        }
    }
    #[test]
    fn answer_navigation_is_one_shot_and_keeps_the_user_selection() {
        let mut bridge = CloudAgent::memory();
        bridge.records = (0..40)
            .map(|i| agent_mcp::Record {
                id: format!("answer-{i}"),
                connection_id: None,
                run_id: None,
                generation: None,
                sequence: 0,
                tool: "notify".into(),
                workspace: "workspace".into(),
                session: format!("{i:08}"),
                created: agent_mcp::now() as i64,
                outcome: r#"{"status":"stored"}"#.into(),
                message: format!("Answer body {i}\nFull result"),
            })
            .collect();
        let mut h = harness(bridge);
        h.state_mut().selected_record = Some("answer-39".into());
        h.run();
        h.get_by_label("Answer body 39\nFull result");
        assert!(h.state().selected_record.is_none());
        let first = format!(
            "Received answer · {} · 00000000",
            crate::ui::notifications::relative_time_label(
                &i18n::Catalog::load("en-US").unwrap(),
                0
            )
        );
        // Move to a second notification then leave and return to the task view.
        h.state_mut().selected_record = Some("answer-0".into());
        h.run();
        h.get_by_label("Answer body 0\nFull result");
        h.get_by_label("Connections").click();
        h.run();
        h.get_by_label("Work · answers").click();
        h.run();
        h.get_by_label("Answer body 0\nFull result");
        let _ = first;
    }
}
