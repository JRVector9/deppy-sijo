//! Settings leaf: captures owner intents; durable operations use the App controller.
use super::{Action, CloudAgent};
mod advanced;
mod connections;
mod history;
mod layout;
mod one_screen;
mod permissions;
mod runs;
mod settings;

const CLOUD_AGENT_INSTRUCTIONS: &str = concat!(
    "Use Deppy's MCP connector. First call list_sessions and keep the exact original session_id/generation for every call, even when Deppy's active tab changes. ",
    "Read the selected session with read_output; its cached screen may be stale, and a quiet screen or screen:null does not prove completion. Respect retry_after_ms and cursor/reset. ",
    "Do not send terminal input unless input_allowed is true. Use send_text for a single line without controls (maximum 8192 UTF-8 bytes); submit=false only types and submit=true sends Enter. ",
    "Use paste_text for multiline text or tabs only when paste_bracketed and paste_ai_confirmed are true. Its maximum is 32768 UTF-8 bytes; serialized arguments and the complete HTTP JSON body each have a 65536-byte limit, so JSON escaping can reject earlier. LF/CRLF and tabs are allowed with verified bracketed paste; standalone CR, ESC and other controls are rejected. submit defaults false; true admits the body and one separate Enter together. Respect existing AI drafts/dialogs and no-effect rejections; never bypass them. ",
    "Use send_ctrl_c only for an explicitly authorized interrupt. Use a unique operation_id per action; retry the exact same tool and arguments only with the same operation_id. A queued receipt confirms PTY queue admission, not command execution or task completion. If delivery is unknown, do not automatically replay input or create a new ID. ",
    "Local completion return is off by default and requires separate owner consent for this connection and session plus existing read consent; enabling it grants no input permission. When enabled, eligible send_text/paste_text calls with submit=true keep the original request open as SSE until verified local task completion or an explicit terminal outcome. Other calls and opt-out calls retain the ordinary JSON/admission response. Unsupported tracking or exhausted capacity rejects before input. ",
    "Keep the initialized MCP-Session-Id and the original SSE Last-Event-ID. After disconnection, resume only that original stream with GET and Last-Event-ID; never resubmit uncertain input to recover a response. The final tool result belongs to the original operation, session/generation and optional run_id. Returned means response bytes were written with no acknowledgement from the receiving agent. This host's long-request timeout and reconnect support remain unverified; do not promise automatic model receipt or wake-up. ",
    "After every completed analysis/task, call notify with YOUR OWN complete final answer (maximum 16384 UTF-8 bytes), including when no terminal input was sent. A stored receipt confirms the answer was saved. Keep the original session_id/generation for this answer. Limits count UTF-8 bytes, not characters. ",
    "New connection identities require explicit owner consent for reading and separately for input; authentication alone grants neither. Legacy tools remain available. For tracked work, call start_run with a unique run_id, title and optional instruction (8192 UTF-8 bytes); registration is not completion. Keep the exact original session_id/generation and run_id. Before input from a new connection, call acquire_control for the approved session with run_id when applicable and ttl_seconds of 1..120; check the lease and renew before it expires. Never invent a connection identity or borrow another connection's lease. Use release_control when finished. ",
    "Report work through update_run: queued/running/waiting/completed/failed/cancelled, optional progress 0..100, message (4096 UTF-8 bytes), result (16384 UTF-8 bytes), and up to 16 actually produced HTTP(S) artifact links. Progress is your report, not independent verification. Send heartbeat updates while working; missing updates become unknown, never automatically completed. Use get_run/list_runs to check cancellation requests, stop your own work and acknowledge with update_run state=cancelled. A cancellation request is not Ctrl+C or input permission revocation. Include run_id on input and notify when tracking work; still send your own final answer to the original session. ",
    "After a lost receipt, use get_operation with the original session_id/generation and operation_id to inspect the existing result without repeating input. Unknown delivery must not be replayed. Use read_history with the original session_id/generation, cursor and max_bytes at most 65536; handle reset/truncation and observation time, respect polling hints, and keep reads bounded."
);

#[cfg(test)]
mod management_tests;
#[cfg(test)]
mod one_screen_tests;
#[cfg(test)]
mod redesign_tests;

impl CloudAgent {
    pub fn contents(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        ui.scope(|ui| {
            crate::ui::settings::apply_component_style(ui);
            ui.style_mut().override_font_id = None;
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
            layout::apply_palette(ui);
            let mut state = settings::state(ui);
            if self.selected_record.is_some() {
                state.tab = settings::Tab::Work;
                state.work_kind = settings::WorkKind::Answers;
            }
            ui.painter().rect_filled(
                ui.available_rect_before_wrap(),
                0.0,
                ui.visuals().extreme_bg_color,
            );
            egui::ScrollArea::vertical()
                .id_salt("cloud-one-screen-scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let width = ui.available_width();
                    let compact = width < layout::COMPACT_WIDTH;
                    let pad_x = if compact {
                        16
                    } else if width < layout::MEDIUM_WIDTH {
                        20
                    } else {
                        26
                    };
                    egui::Frame::NONE
                        .inner_margin(egui::Margin {
                            left: pad_x,
                            right: pad_x,
                            top: if compact { 18 } else { 22 },
                            bottom: 14,
                        })
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            self.one_screen(ui, catalog, &mut state, compact);
                        });
                });
            settings::store(ui, state);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::{NodeT as _, Queryable as _};
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
        let h = harness(bridge);
        h.get_by_label("Verifying public URL…");
        assert!(h.query_by_label("Public MCP URL verified").is_none());

        assert!(h.get_by_label("Copy URL").accesskit_node().is_disabled());
    }
    #[test]
    fn setup_state_public_readiness_keeps_client_registration_separate() {
        let mut bridge = CloudAgent::memory();
        bridge.server =
            Some(agent_mcp::Server::start(0, "ready.trycloudflare.com", || {}).unwrap());
        bridge.generated_hostname = Some("ready.trycloudflare.com".into());
        bridge.connection = super::super::Connection::Ready;
        let h = harness(bridge);
        h.get_by_label("External access ready");

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
        let h = harness(bridge);
        h.get_by_label("Fixed public URL configured · check your tunnel and connector");

        h.get_by_label("https://deppy.example.com/mcp");
        assert!(h.query_by_label("Public MCP URL verified").is_none());
    }
    #[test]
    fn setup_state_manual_local_only_hides_local_endpoint_and_disables_copy() {
        let mut bridge = CloudAgent::memory();
        bridge.automatic = false;
        bridge.server = Some(agent_mcp::Server::start(0, "", || {}).unwrap());
        bridge.connection = super::super::Connection::Ready;
        let url = format!("http://{}/mcp", bridge.server.as_ref().unwrap().addr);
        let h = harness(bridge);
        assert!(h.query_by_label(&url).is_none());
        assert!(h.get_by_label("Copy URL").accesskit_node().is_disabled());
    }
    #[test]
    fn setup_state_copy_instruction_delivers_multiline_bounds_and_receipt_contract() {
        let mut h = harness(CloudAgent::memory());
        h.get_by_label("Other connection methods · more settings")
            .click();
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
            "Local completion return is off by default",
            "SSE",
            "MCP-Session-Id",
            "Last-Event-ID",
            "no acknowledgement",
            "unverified",
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
        h.get_by_label("Work · answers").click();
        h.run();
        h.get_by_label("Work · answers").click();
        h.run();
        h.get_by_label("Answer body 0\nFull result");
        let _ = first;
    }
}
