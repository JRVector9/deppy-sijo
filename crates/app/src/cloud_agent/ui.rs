//! Settings leaf: captures owner intents; durable operations use the App controller.
use super::{Action, CloudAgent};
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

impl CloudAgent {
    pub fn contents(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        ui.heading(catalog.t("cloud.title", &[]));
        ui.label(catalog.t("cloud.intro", &[]));
        ui.add_space(10.0);
        let running = self.busy();
        ui.add_enabled_ui(!running, |ui| {
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.automatic, true, catalog.t("cloud.mode_auto", &[]));
                ui.radio_value(
                    &mut self.automatic,
                    false,
                    catalog.t("cloud.mode_manual", &[]),
                );
            });
            ui.horizontal(|ui| {
                ui.label(catalog.t("cloud.port", &[]));
                ui.add(egui::DragValue::new(&mut self.port).range(1024..=65535));
                if !self.automatic {
                    ui.label(catalog.t("cloud.hostname", &[]));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.hostname)
                            .char_limit(259)
                            .hint_text("deppy.example.com"),
                    );
                }
            });
        });
        ui.label(
            egui::RichText::new(catalog.t(
                if self.automatic {
                    "cloud.auto_hint"
                } else {
                    "cloud.tunnel_hint"
                },
                &[],
            ))
            .weak(),
        );
        self.profile_settings(ui, catalog);
        if let Some(server) = &self.server {
            ui.label(catalog.t(
                "cloud.local_ready",
                &[("port", &server.addr.port().to_string())],
            ));
        }
        if running {
            let key = match self.connection {
                super::Connection::Preparing => "cloud.preparing",
                super::Connection::Verifying => "cloud.verifying",
                super::Connection::Stopping => "cloud.stopping",
                super::Connection::Ready if self.automatic && self.generated_hostname.is_some() => {
                    "cloud.ready"
                }
                _ if !self.automatic && !self.hostname.trim().is_empty() => {
                    "cloud.manual_unverified"
                }
                _ => "cloud.local_only",
            };
            ui.horizontal(|ui| {
                if matches!(
                    self.connection,
                    super::Connection::Preparing
                        | super::Connection::Verifying
                        | super::Connection::Stopping
                ) {
                    ui.spinner();
                }
                ui.label(catalog.t(key, &[]));
            });
            ui.weak(catalog.t("cloud.client_setup_hint", &[]));
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    self.connection != super::Connection::Stopping,
                    egui::Button::new(
                        catalog.t(if running { "cloud.stop" } else { "cloud.start" }, &[]),
                    ),
                )
                .clicked()
            {
                self.action = Some(if running { Action::Stop } else { Action::Start });
                ui.ctx().request_repaint();
            }
            if self.ready() && ui.button(catalog.t("cloud.rotate", &[])).clicked() {
                self.action = Some(Action::Rotate);
                ui.ctx().request_repaint();
            }
            if ui.button(catalog.t("cloud.take_control", &[])).clicked() {
                // Take control is synchronous consent revocation; no request can be admitted after this click.
                self.take_control();
            }
        });
        let connector_endpoint = self.endpoint().filter(|endpoint| {
            !self.automatic
                || (endpoint.starts_with("https://")
                    && self.connection == super::Connection::Ready
                    && self.generated_hostname.is_some())
        });
        if let (Some(s), Some(endpoint)) = (&self.server, connector_endpoint) {
            let public = endpoint.starts_with("https://");
            ui.horizontal(|ui| {
                ui.label(catalog.t(
                    if public {
                        "cloud.public_url"
                    } else {
                        "cloud.local_url"
                    },
                    &[],
                ));
                ui.monospace(&endpoint);
                if ui.button(catalog.t("cloud.copy_url", &[])).clicked() {
                    ui.ctx().copy_text(endpoint);
                }
            });
            if !self.automatic && public {
                ui.horizontal(|ui| {
                    ui.weak(catalog.t("cloud.local_url", &[]));
                    ui.weak(format!("http://{}/mcp", s.addr));
                });
            }
            ui.label(
                catalog.t(
                    "cloud.expiry",
                    &[(
                        "seconds",
                        &s.auth
                            .expires()
                            .saturating_sub(agent_mcp::now())
                            .to_string(),
                    )],
                ),
            );
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.reveal, catalog.t("cloud.reveal_token", &[]));
                let token = s.auth.token_for_user();
                if ui.button(catalog.t("cloud.copy_token", &[])).clicked() {
                    ui.ctx().copy_text(token.to_string());
                }
                ui.monospace(if self.reveal {
                    token.as_str()
                } else {
                    "••••••••••••••••"
                });
            });
        }
        if let Some(error) = &self.error {
            let message = if error == "tunnel_companion_missing" {
                catalog.t("cloud.helper_missing", &[])
            } else if error.starts_with("tunnel_") {
                catalog.t("cloud.auto_failed", &[])
            } else {
                catalog.t("cloud.error", &[("code", error)])
            };
            ui.colored_label(ui.visuals().error_fg_color, message);
        }
        if let Some(s) = &self.server {
            for approval in s.auth.approvals() {
                ui.group(|ui| {
                    ui.label(
                        catalog.t("cloud.oauth_request", &[("client", &approval.client_name)]),
                    );
                    ui.weak(&approval.redirect_uri);
                    ui.label(catalog.t(
                        if approval.input {
                            "cloud.oauth_input"
                        } else {
                            "cloud.oauth_read"
                        },
                        &[],
                    ));
                    ui.horizontal(|ui| {
                        if ui.button(catalog.t("cloud.oauth_approve", &[])).clicked() {
                            s.auth.approve(&approval.id, true);
                        }
                        if ui.button(catalog.t("cloud.oauth_deny", &[])).clicked() {
                            s.auth.approve(&approval.id, false);
                        }
                    });
                });
            }
        }
        self.connection_settings(ui, catalog);
        self.run_settings(ui, catalog);
        ui.separator();
        ui.heading(catalog.t("cloud.sessions", &[]));
        ui.label(catalog.t("cloud.permission_hint", &[]));
        if self.targets.is_empty() {
            ui.label(catalog.t("cloud.no_sessions", &[]));
        }
        for t in self.targets.clone() {
            ui.push_id((&t.id, &t.generation), |ui| {
                ui.horizontal_wrapped(|ui| {
                    let mut shared = self.grants.contains_key(&t.id);
                    if ui
                        .checkbox(&mut shared, catalog.t("cloud.share", &[]))
                        .changed()
                    {
                        self.share(&t, shared);
                    }
                    let mut input = self.grants.get(&t.id).is_some_and(|g| g.input);
                    if ui
                        .add_enabled(
                            shared && t.live,
                            egui::Checkbox::new(&mut input, catalog.t("cloud.allow_input", &[])),
                        )
                        .changed()
                    {
                        self.allow_input(&t.id, input);
                    }
                    ui.label(format!(
                        "{} · {} / {}",
                        t.workspace_name,
                        t.title,
                        t.agent_line
                            .as_deref()
                            .filter(|line| !line.trim().is_empty())
                            .map(str::to_owned)
                            .unwrap_or_else(|| catalog.t("workspace.spawn.shell", &[]))
                    ))
                    .on_hover_text(&t.id);
                    if !t.live {
                        ui.weak(catalog.t("cloud.exited", &[]));
                    }
                });
            });
        }
        ui.add_space(8.0);
        ui.heading(catalog.t("cloud.ended_history", &[]));
        ui.label(catalog.t("cloud.ended_history_hint", &[]));
        if self.ended_load_failed {
            ui.colored_label(
                ui.visuals().error_fg_color,
                catalog.t("cloud.ended_history_error", &[]),
            );
        }
        let historical = self.historical_rows();
        if historical.is_empty() && self.ended_rx.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(catalog.t("cloud.ended_history_loading", &[]));
            });
        } else if historical.is_empty() && !self.ended_load_failed {
            ui.weak(catalog.t("cloud.ended_history_empty", &[]));
        }
        egui::ScrollArea::vertical()
            .id_salt("cloud-ended-sessions")
            .max_height(240.0)
            .show_rows(ui, 24.0, historical.len(), |ui, range| {
                for row in &historical[range] {
                    ui.add(
                        egui::Label::new(format!(
                            "{} · {} · {}",
                            row.workspace_name,
                            row.title,
                            catalog.t("cloud.ended_history_status", &[])
                        ))
                        .truncate(),
                    )
                    .on_hover_text(&row.id);
                }
            });
        ui.separator();
        if ui
            .button(catalog.t("cloud.copy_instruction", &[]))
            .clicked()
        {
            ui.ctx().copy_text(CLOUD_AGENT_INSTRUCTIONS.into());
        }
        ui.label(catalog.t("cloud.answer_hint", &[]));
        ui.heading(catalog.t("cloud.history", &[]));
        if self.records.is_empty() {
            ui.label(catalog.t("cloud.no_history", &[]));
        }
        // A notification opens and reveals its answer once. Subsequent frames
        // leave scrolling and collapsing under the user's control.
        let selected_record = self.selected_record.take();
        for record in &self.records {
            let record_key = agent_mcp::operation_key(
                record
                    .connection_id
                    .as_deref()
                    .unwrap_or(agent_mcp::LEGACY_CONNECTION_ID),
                &record.id,
            )
            .unwrap_or_else(|_| record.id.clone());
            let selected = selected_record.as_deref() == Some(record_key.as_str());
            let kind = match record.tool.as_str() {
                "notify" => "cloud.history_answer",
                "send_ctrl_c" => "cloud.history_interrupt",
                _ => "cloud.history_input",
            };
            let title = format!(
                "{} · {} · {}",
                catalog.t(kind, &[]),
                crate::ui::notifications::relative_time_label(
                    catalog,
                    agent_mcp::now() as i64 - record.created
                ),
                record.session.chars().take(8).collect::<String>()
            );
            let response = egui::CollapsingHeader::new(title)
                .id_salt(&record_key)
                .open(if selected { Some(true) } else { None })
                .show(ui, |ui| {
                    ui.weak(format!("{} / {}", record.workspace, record.session));
                    let outcome: serde_json::Value =
                        serde_json::from_str(&record.outcome).unwrap_or_default();
                    let status = match outcome["status"].as_str() {
                        Some("queued") => "cloud.outcome_queued",
                        Some("stored") => "cloud.outcome_stored",
                        Some("rejected") => "cloud.outcome_rejected",
                        _ => "cloud.outcome_unknown",
                    };
                    ui.label(catalog.t(status, &[]));
                    if !record.message.is_empty() {
                        ui.label(&record.message);
                        if ui.button(catalog.t("cloud.copy_answer", &[])).clicked() {
                            ui.ctx().copy_text(record.message.clone());
                        }
                    }
                });
            if selected {
                response
                    .header_response
                    .scroll_to_me(Some(egui::Align::TOP));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable as _;
    #[test]
    fn setup_state_local_listener_does_not_claim_public_or_client_connection() {
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut bridge = CloudAgent::memory();
        bridge.server =
            Some(agent_mcp::Server::start(0, "starting.trycloudflare.com", || {}).unwrap());
        let local = format!(
            "Local MCP server ready · 127.0.0.1:{}",
            bridge.server.as_ref().unwrap().addr.port()
        );
        bridge.connection = super::super::Connection::Verifying;
        let harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1400.0, 1000.0))
            .build_ui_state(
                move |ui, state: &mut CloudAgent| state.contents(ui, &catalog),
                bridge,
            );
        assert!(harness.query_by_label(&local).is_some());
        assert!(harness.query_by_label("Verifying public URL…").is_some());
        assert!(harness.query_by_label("Public MCP URL verified").is_none());
        assert!(harness.query_by_label("MCP connected").is_none());
        assert!(harness.query_by_label("Copy URL").is_none());
    }
    #[test]
    fn setup_state_public_readiness_keeps_client_registration_separate() {
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut bridge = CloudAgent::memory();
        bridge.server =
            Some(agent_mcp::Server::start(0, "ready.trycloudflare.com", || {}).unwrap());
        bridge.generated_hostname = Some("ready.trycloudflare.com".into());
        bridge.connection = super::super::Connection::Ready;
        let harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1400.0, 1000.0))
            .build_ui_state(
                move |ui, state: &mut CloudAgent| state.contents(ui, &catalog),
                bridge,
            );
        assert!(harness.query_by_label("Public MCP URL verified").is_some());
        assert!(
            harness
                .query_by_label("Server readiness does not confirm a bot connection. Register the URL in your connector, authenticate, then call list_sessions.")
                .is_some()
        );
        assert!(
            harness
                .query_by_label("https://ready.trycloudflare.com/mcp")
                .is_some()
        );
        assert!(harness.query_by_label("Copy URL").is_some());
        assert!(harness.query_by_label("MCP connected").is_none());
    }
    #[test]
    fn setup_state_manual_endpoint_does_not_claim_public_verification() {
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut bridge = CloudAgent::memory();
        bridge.automatic = false;
        bridge.hostname = "deppy.example.com".into();
        bridge.server = Some(agent_mcp::Server::start(0, &bridge.hostname, || {}).unwrap());
        bridge.connection = super::super::Connection::Ready;
        let harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1400.0, 1000.0))
            .build_ui_state(
                move |ui, state: &mut CloudAgent| state.contents(ui, &catalog),
                bridge,
            );
        assert!(
            harness
                .query_by_label("Fixed public URL configured · check your tunnel and connector")
                .is_some()
        );
        assert!(harness.query_by_label("Public MCP URL verified").is_none());
        assert!(
            harness
                .query_by_label("https://deppy.example.com/mcp")
                .is_some()
        );
    }
    #[test]
    fn setup_state_manual_local_only_preserves_local_url_and_token_controls() {
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut bridge = CloudAgent::memory();
        bridge.automatic = false;
        bridge.server = Some(agent_mcp::Server::start(0, "", || {}).unwrap());
        bridge.connection = super::super::Connection::Ready;
        let local = format!("http://{}/mcp", bridge.server.as_ref().unwrap().addr);
        let harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1400.0, 1000.0))
            .build_ui_state(
                move |ui, state: &mut CloudAgent| state.contents(ui, &catalog),
                bridge,
            );
        assert!(harness.query_by_label("Local URL").is_some());
        assert!(harness.query_by_label(&local).is_some());
        assert!(harness.query_by_label("Copy URL").is_some());
        assert!(harness.query_by_label("Copy token").is_some());
        assert!(harness.query_by_label("Connector URL").is_none());
        assert!(harness.query_by_label("Public MCP URL verified").is_none());
    }
    #[test]
    fn setup_state_copy_instruction_delivers_multiline_bounds_and_receipt_contract() {
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let button = catalog.t("cloud.copy_instruction", &[]);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1400.0, 1000.0))
            .build_ui_state(
                move |ui, state: &mut (CloudAgent, Option<String>)| {
                    state.0.contents(ui, &catalog);
                    ui.ctx().output(|output| {
                        for command in &output.commands {
                            if let egui::OutputCommand::CopyText(text) = command {
                                state.1 = Some(text.clone());
                            }
                        }
                    });
                },
                (CloudAgent::memory(), None),
            );
        harness.get_by_label(&button).click();
        harness.run();
        let copied = harness.state().1.as_ref().expect("instruction copied");
        for contract in [
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
                copied.contains(contract),
                "missing copied contract: {contract}"
            );
        }
    }
    #[test]
    fn shared_rows_show_session_agent_model_and_effort_with_shell_fallback() {
        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        let mut bridge = CloudAgent::memory();
        let mut agent = crate::cloud_agent::Target::fixture("agent", "generation-1");
        agent.title = "리뷰 작업".into();
        let mut workspace = crate::ui::workspace::WorkspaceUi::new();
        workspace.set_agent_info(std::collections::HashMap::from([(
            agent.session,
            crate::agent_detect::AgentDisplay {
                kind: crate::agent_detect::AgentKind::Codex,
                model: Some("gpt-6-astra".into()),
                effort: Some("xhigh".into()),
                context_pct: None,
                last_agent_summary: None,
                user_instruction: None,
            },
        )]));
        agent.agent_line = workspace.agent_line_for(agent.session);
        let mut shell = crate::cloud_agent::Target::fixture("shell", "generation-2");
        shell.title = "배포 셸".into();
        bridge.set_targets(vec![agent, shell]);
        let harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1400.0, 900.0))
            .build_ui_state(
                move |ui, state: &mut CloudAgent| state.contents(ui, &catalog),
                bridge,
            );
        assert!(
            harness
                .query_by_label("workspace · 리뷰 작업 / Codex · gpt-6-astra · xhigh")
                .is_some()
        );
        assert!(harness.query_by_label("workspace · 배포 셸 / 셸").is_some());
    }
    #[test]
    fn closed_session_history_is_visible_without_share_controls() {
        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        let mut bridge = CloudAgent::memory();
        bridge.ended_sessions.push(storage::CloudEndedSession {
            id: "closed-session".into(),
            workspace_id: "workspace-id".into(),
            workspace_name: "디자인".into(),
            title: "이전 작업".into(),
        });
        let harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1400.0, 900.0))
            .build_ui_state(
                move |ui, state: &mut CloudAgent| state.contents(ui, &catalog),
                bridge,
            );
        assert!(
            harness
                .query_by_label("디자인 · 이전 작업 · 종료됨 · 공유 불가")
                .is_some()
        );
        assert!(harness.query_by_label("읽기 · 답변 수신").is_none());
    }
    #[test]
    fn automatic_connection_is_default_and_manual_host_is_advanced() {
        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        let bridge = CloudAgent::memory();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 900.0))
            .build_ui_state(
                move |ui, state: &mut CloudAgent| state.contents(ui, &catalog),
                bridge,
            );
        assert!(harness.query_by_label("자동 주소 생성").is_some());
        assert!(harness.query_by_label("공개 HTTPS 호스트").is_none());
        harness.get_by_label("고정 주소 직접 연결").click();
        harness.run();
        assert!(harness.query_by_label("공개 HTTPS 호스트").is_some());
    }
    #[test]
    fn automatic_connection_shows_public_url_without_local_loopback_url() {
        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        let mut bridge = CloudAgent::memory();
        bridge.server =
            Some(agent_mcp::Server::start(0, "ready.trycloudflare.com", || {}).unwrap());
        let local = format!("http://{}/mcp", bridge.server.as_ref().unwrap().addr);
        bridge.generated_hostname = Some("ready.trycloudflare.com".into());
        bridge.connection = super::super::Connection::Ready;
        let harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1400.0, 900.0))
            .build_ui_state(
                move |ui, state: &mut CloudAgent| state.contents(ui, &catalog),
                bridge,
            );
        assert!(
            harness
                .query_by_label("https://ready.trycloudflare.com/mcp")
                .is_some()
        );
        assert!(harness.query_by_label(&local).is_none());
    }
    #[test]
    fn answer_navigation_scrolls_once_and_allows_collapsing_and_reopening() {
        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        let mut bridge = CloudAgent::memory();
        bridge.records = (0..40)
            .map(|index| agent_mcp::Record {
                id: format!("answer-{index}"),
                connection_id: None,
                run_id: None,
                generation: None,
                sequence: 0,
                tool: "notify".into(),
                workspace: "workspace".into(),
                session: format!("{index:08}"),
                created: agent_mcp::now() as i64,
                outcome: r#"{"status":"stored"}"#.into(),
                message: format!("Cloud answer {index}"),
            })
            .collect();
        let header = format!(
            "{} · {} · 00000039",
            catalog.t("cloud.history_answer", &[]),
            crate::ui::notifications::relative_time_label(&catalog, 0)
        );
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(900.0, 500.0))
            .build_ui_state(
                move |ui, state: &mut (CloudAgent, f32)| {
                    ui.style_mut().animation_time = 0.0;
                    let scroll = egui::ScrollArea::vertical().show(ui, |ui| {
                        state.0.contents(ui, &catalog);
                    });
                    state.1 = scroll.state.offset.y;
                },
                (bridge, 0.0),
            );
        harness.run();
        assert!(harness.query_by_label("Cloud answer 39").is_none());
        // This is the same one-shot intent delivered by a cloud-answer notification.
        harness.state_mut().0.selected_record = Some("answer-39".into());
        harness.run();
        assert!(harness.state().1 > 200.0, "navigate to the older answer");
        let answer = harness.get_by_label("Cloud answer 39").rect();
        assert!(answer.top() >= 0.0 && answer.bottom() <= 500.0);
        harness.get_by_label(&header).click();
        harness.run();
        assert!(
            harness.query_by_label("Cloud answer 39").is_none(),
            "the user can collapse the answer after notification navigation"
        );
        harness.state_mut().0.selected_record = Some("answer-39".into());
        harness.run();
        harness.get_by_label("Cloud answer 39");
    }
    #[test]
    fn session_checkboxes_and_take_control_change_real_consent() {
        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        let mut bridge = CloudAgent::memory();
        let target = crate::cloud_agent::Target::fixture("a", "generation-1");
        bridge.set_targets(vec![target]);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 900.0))
            .build_ui_state(
                move |ui, state: &mut CloudAgent| state.contents(ui, &catalog),
                bridge,
            );
        harness
            .get_by_role_and_label(egui::accesskit::Role::CheckBox, "읽기 · 답변 수신")
            .click();
        harness.run();
        assert!(harness.state().grants.contains_key("a"));
        harness
            .get_by_role_and_label(egui::accesskit::Role::CheckBox, "입력 허용")
            .click();
        harness.run();
        assert!(harness.state().grants["a"].input);
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "모든 입력 제어 회수")
            .click();
        harness.run();
        assert!(!harness.state().grants["a"].input);
        assert!(harness.state().grants.contains_key("a"));
    }
}
