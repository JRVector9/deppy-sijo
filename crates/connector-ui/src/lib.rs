//! Pure Connector snapshot renderer.
//!
//! The renderer consumes immutable [`contract::ConnectorSnapshot`] values and returns at most one
//! [`contract::ConnectorIntent`] per frame. It performs no file, URL, service, thread, channel, or
//! runtime I/O. Only user-authored drafts and a revision-keyed display cache live here.

use std::mem;

use connector_contract::{
    ApprovalDecision, ApprovalPrompt, ConnectionState, ConnectorIntent, ConnectorSnapshot,
    ErrorCode, ExternalLinkKind, ImportOutcome, ImportReport, ImportSource, ImportSourceRequest,
    OAuthRecoveryAction, OAuthUiPhase, OAuthUiState, OperationKind, OperationPhase,
    OperationSummary, PermissionRule, RemoteTrustPrompt, RemoteTrustPurpose, ResourceLimits,
    Revision, SensitiveInput, ServerDraft, ServerId, ServerSummary, SlackRecoveryKind, SlackStatus,
    ToolListItem, ToolPage, TransportDraft, TransportKind,
};
use egui::{Button, ComboBox, Label, ScrollArea, TextEdit, Ui};
use i18n::Catalog;

pub use connector_contract as contract;

const SERVER_ROW_HEIGHT: f32 = 30.0;
const SERVER_LIST_HEIGHT: f32 = 180.0;
const TOOL_ROW_HEIGHT: f32 = 58.0;
const TOOL_LIST_HEIGHT: f32 = TOOL_ROW_HEIGHT * 6.0;
const IMPORT_ROW_HEIGHT: f32 = 30.0;
const IMPORT_LIST_HEIGHT: f32 = IMPORT_ROW_HEIGHT * 6.0;
const OPERATION_ROW_HEIGHT: f32 = 30.0;
const OPERATION_LIST_HEIGHT: f32 = OPERATION_ROW_HEIGHT * 5.0;
const OAUTH_SCOPE_ROW_HEIGHT: f32 = 24.0;
const OAUTH_SCOPE_LIST_HEIGHT: f32 = OAUTH_SCOPE_ROW_HEIGHT * 5.0;

/// Stateful UI shell. State is limited to unfinished user drafts and derived display strings.
pub struct ConnectorUi {
    labels: Labels,
    prepared: PreparedDisplayCache,
    add_server: Option<ServerFormDraft>,
    invoke: Option<InvokeDraft>,
    oauth_client: Option<OAuthClientDraft>,
    delete_server: Option<DeleteServerDraft>,
    paste_input: String,
}

impl std::fmt::Debug for ConnectorUi {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ConnectorUi(REDACTED)")
    }
}

impl Drop for ConnectorUi {
    fn drop(&mut self) {
        zero_string(&mut self.paste_input);
    }
}

impl ConnectorUi {
    pub fn new(catalog: &Catalog) -> Self {
        Self {
            labels: Labels::new(catalog),
            prepared: PreparedDisplayCache::default(),
            add_server: None,
            invoke: None,
            oauth_client: None,
            delete_server: None,
            paste_input: String::new(),
        }
    }

    /// Refreshes cached translations after an application locale change.
    /// Existing user drafts stay intact; derived strings rebuild once on the next snapshot render.
    pub fn set_catalog(&mut self, catalog: &Catalog) {
        self.labels = Labels::new(catalog);
        self.prepared.invalidate();
    }

    /// Renders one frame from an immutable snapshot and returns at most one intent.
    #[must_use]
    pub fn render(&mut self, ui: &mut Ui, snapshot: &ConnectorSnapshot) -> Option<ConnectorIntent> {
        sync_oauth_client_draft(&mut self.oauth_client, snapshot.oauth.as_ref());
        self.prepared.prepare(snapshot);

        let mut intent = None;
        let has_service_modal = snapshot.remote_trust.is_some()
            || snapshot.oauth.is_some()
            || snapshot.approval.is_some();
        let has_local_modal = self.add_server.is_some()
            || self.invoke.is_some()
            || self.oauth_client.is_some()
            || self.delete_server.is_some();

        ui.add_enabled_ui(!(has_service_modal || has_local_modal), |ui| {
            render_main(
                ui,
                snapshot,
                &self.prepared,
                &self.labels,
                &mut self.add_server,
                &mut self.invoke,
                &mut self.delete_server,
                &mut self.paste_input,
                &mut intent,
            );
        });

        render_add_server_modal(ui, &self.labels, &mut self.add_server, &mut intent);
        render_invoke_modal(ui, &self.labels, &mut self.invoke, &mut intent);
        render_delete_modal(ui, &self.labels, &mut self.delete_server, &mut intent);

        if let Some(approval) = snapshot.approval.as_ref() {
            render_approval_modal(ui, &self.labels, approval, &mut intent);
        } else if let Some(prompt) = snapshot.remote_trust.as_ref() {
            render_remote_trust_modal(ui, &self.labels, prompt, &mut intent);
        } else if let Some(oauth) = snapshot.oauth.as_ref() {
            render_oauth_modal(ui, &self.labels, oauth, &mut self.oauth_client, &mut intent);
        }

        intent
    }
}

#[allow(clippy::too_many_arguments)]
fn render_main(
    ui: &mut Ui,
    snapshot: &ConnectorSnapshot,
    prepared: &PreparedDisplayCache,
    labels: &Labels,
    add_server: &mut Option<ServerFormDraft>,
    invoke: &mut Option<InvokeDraft>,
    delete_server: &mut Option<DeleteServerDraft>,
    paste_input: &mut String,
    intent: &mut Option<ConnectorIntent>,
) {
    ui.heading(&labels.title);
    ui.horizontal_wrapped(|ui| {
        if ui.button(&labels.add_server).clicked() {
            *add_server = Some(ServerFormDraft::default());
        }
        if ui.button(&labels.import_file).clicked() {
            offer_intent(
                intent,
                ConnectorIntent::RequestImportSource(ImportSourceRequest::FilePicker),
            );
        }
        if ui.button(&labels.import_claude).clicked() {
            offer_intent(
                intent,
                ConnectorIntent::RequestImportSource(ImportSourceRequest::ClaudeDesktop),
            );
        }
        if ui.button(&labels.open_slack_settings).clicked() {
            offer_intent(
                intent,
                ConnectorIntent::OpenExternalLink(ExternalLinkKind::SlackAppSettings),
            );
        }
        if ui.button(&labels.refresh).clicked() {
            offer_intent(intent, ConnectorIntent::Activate);
        }
    });

    render_slack_summary(ui, snapshot, prepared, labels, intent);
    ui.separator();
    render_server_overview(ui, snapshot, prepared, labels, intent);

    let selected = snapshot
        .selected_server
        .as_ref()
        .and_then(|id| snapshot.servers.iter().find(|server| &server.id == id));
    if let Some(server) = selected {
        let selected_config = snapshot
            .selected_server_config
            .as_ref()
            .filter(|config| config.id.as_ref() == Some(&server.id));
        ui.separator();
        render_selected_server(
            ui,
            snapshot,
            prepared,
            server,
            selected_config,
            labels,
            add_server,
            invoke,
            delete_server,
            intent,
        );
    }

    ui.separator();
    render_import_controls(ui, labels, paste_input, intent);

    if let Some(report) = snapshot.import_report.as_ref() {
        ui.separator();
        render_import_report(ui, report, prepared, labels, intent);
    }

    if !snapshot.operations.is_empty() {
        ui.separator();
        render_operations(ui, snapshot, prepared, labels, intent);
    }

    if let Some(result) = snapshot.result.as_ref() {
        ui.separator();
        ui.strong(&labels.result);
        ScrollArea::vertical()
            .id_salt("connector_result")
            .max_height(160.0)
            .show(ui, |ui| {
                ui.add(Label::new(&result.text).wrap());
            });
        if result.truncated {
            ui.weak(&labels.truncated);
        }
        if ui.button(&labels.close).clicked() {
            offer_intent(
                intent,
                ConnectorIntent::DismissResult(result.operation_id.clone()),
            );
        }
    }
}

fn render_slack_summary(
    ui: &mut Ui,
    snapshot: &ConnectorSnapshot,
    prepared: &PreparedDisplayCache,
    labels: &Labels,
    intent: &mut Option<ConnectorIntent>,
) {
    let slack = &snapshot.slack;
    ui.group(|ui| {
        ui.horizontal_wrapped(|ui| {
            ui.strong("Slack");
            ui.label(slack_status_label(slack.status, labels));
            if slack.tool_count > 0 {
                ui.label(&prepared.slack_tool_count);
                ui.label(&labels.tools);
            }
            if let Some(workspace) = slack.workspace_label.as_deref() {
                ui.label(&labels.workspace);
                ui.strong(workspace);
            }

            if slack_connect_enabled(slack.status) && ui.button(&labels.connect_slack).clicked() {
                offer_intent(intent, ConnectorIntent::ConnectSlack);
            }

            if slack.can_choose_workspace
                && let Some(server_id) = slack.server_id.as_ref()
                && ui.button(&labels.choose_workspace).clicked()
            {
                offer_intent(
                    intent,
                    ConnectorIntent::ChooseSlackWorkspace(server_id.clone()),
                );
            }

            if let Some(kind) = slack.recovery {
                if let Some(server_id) = slack.server_id.as_ref() {
                    if ui.button(slack_recovery_label(kind, labels)).clicked() {
                        offer_intent(
                            intent,
                            ConnectorIntent::OpenSlackRecovery {
                                server_id: server_id.clone(),
                                kind,
                            },
                        );
                    }
                } else if kind == SlackRecoveryKind::ConfigureApp
                    && ui.button(&labels.open_slack_settings).clicked()
                {
                    offer_intent(
                        intent,
                        ConnectorIntent::OpenExternalLink(ExternalLinkKind::SlackAppSettings),
                    );
                }
            }
        });
    });
}

fn render_server_overview(
    ui: &mut Ui,
    snapshot: &ConnectorSnapshot,
    prepared: &PreparedDisplayCache,
    labels: &Labels,
    intent: &mut Option<ConnectorIntent>,
) {
    if snapshot.servers.is_empty() {
        ui.weak(&labels.empty);
        return;
    }

    ScrollArea::vertical()
        .id_salt("connector_server_overview")
        .max_height(SERVER_LIST_HEIGHT)
        .show_rows(ui, SERVER_ROW_HEIGHT, snapshot.servers.len(), |ui, rows| {
            for index in rows {
                let server = &snapshot.servers[index];
                let selected = snapshot.selected_server.as_ref() == Some(&server.id);
                ui.horizontal(|ui| {
                    let response = ui.add_sized(
                        [ui.available_width() * 0.55, SERVER_ROW_HEIGHT],
                        Button::selectable(selected, &server.name),
                    );
                    if response.clicked() {
                        offer_intent(
                            intent,
                            ConnectorIntent::SelectServer(Some(server.id.clone())),
                        );
                    }
                    ui.label(connection_label(server.connection, labels));
                    ui.label(&prepared.server_tool_counts[index]);
                    ui.label(&labels.tools);
                });
            }
        });
}

#[allow(clippy::too_many_arguments)]
fn render_selected_server(
    ui: &mut Ui,
    snapshot: &ConnectorSnapshot,
    prepared: &PreparedDisplayCache,
    server: &ServerSummary,
    selected_config: Option<&ServerDraft>,
    labels: &Labels,
    server_form: &mut Option<ServerFormDraft>,
    invoke: &mut Option<InvokeDraft>,
    delete_server: &mut Option<DeleteServerDraft>,
    intent: &mut Option<ConnectorIntent>,
) {
    ui.horizontal_wrapped(|ui| {
        ui.heading(&server.name);
        ui.label(transport_label(server.transport, labels));
        if ui.button(&labels.discover).clicked() {
            offer_intent(intent, ConnectorIntent::Discover(server.id.clone()));
        }
        if let Some(config) = selected_config
            && ui.button(&labels.edit).clicked()
        {
            *server_form = Some(ServerFormDraft::from_contract(config));
        }
        if ui.button(&labels.authorize).clicked() {
            offer_intent(intent, ConnectorIntent::BeginOAuth(server.id.clone()));
        }
        if ui.button(&labels.delete).clicked() {
            *delete_server = Some(DeleteServerDraft {
                server_id: server.id.clone(),
                server_name: server.name.clone(),
            });
        }
    });

    let matching_page = snapshot
        .tool_page
        .as_ref()
        .filter(|page| page.server_id == server.id);
    if let Some(page) = matching_page {
        render_tool_page(ui, page, &prepared.tool_total, labels, invoke, intent);
    } else {
        ui.weak(&labels.tools_not_loaded);
        if ui.button(&labels.load_tools).clicked() {
            offer_intent(
                intent,
                ConnectorIntent::RequestToolPage {
                    server_id: server.id.clone(),
                    offset: 0,
                },
            );
        }
    }
}

fn render_tool_page(
    ui: &mut Ui,
    page: &ToolPage,
    total_label: &str,
    labels: &Labels,
    invoke: &mut Option<InvokeDraft>,
    intent: &mut Option<ConnectorIntent>,
) {
    ui.horizontal(|ui| {
        ui.strong(&labels.tools);
        ui.label(total_label);
    });

    let items: &[ToolListItem] = page.items.as_ref();
    ScrollArea::vertical()
        .id_salt(("connector_tools", page.server_id.as_str(), page.offset))
        .max_height(TOOL_LIST_HEIGHT)
        .show_rows(ui, TOOL_ROW_HEIGHT, items.len(), |ui, rows| {
            for tool in &items[rows] {
                record_rendered_tool_row();
                render_tool_row(ui, &page.server_id, tool, labels, invoke, intent);
            }
        });

    let loaded_end = page.offset.saturating_add(items.len());
    if loaded_end < page.total && ui.button(&labels.load_more).clicked() {
        offer_intent(
            intent,
            ConnectorIntent::RequestToolPage {
                server_id: page.server_id.clone(),
                offset: loaded_end,
            },
        );
    }
}

fn render_tool_row(
    ui: &mut Ui,
    server_id: &ServerId,
    tool: &ToolListItem,
    labels: &Labels,
    invoke: &mut Option<InvokeDraft>,
    intent: &mut Option<ConnectorIntent>,
) {
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), TOOL_ROW_HEIGHT),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            ui.horizontal(|ui| {
                ui.strong(&tool.name);
                if ui.button(&labels.invoke).clicked() {
                    *invoke = Some(InvokeDraft {
                        server_id: server_id.clone(),
                        tool_id: tool.id.clone(),
                        tool_name: tool.name.clone(),
                        arguments_json: "{}".to_owned(),
                    });
                }
                ComboBox::from_id_salt((
                    "connector_permission",
                    server_id.as_str(),
                    tool.id.as_str(),
                ))
                .selected_text(permission_label(tool.permission, labels))
                .show_ui(ui, |ui| {
                    for (rule, label) in [
                        (PermissionRule::Ask, labels.rule_ask.as_str()),
                        (PermissionRule::Allow, labels.rule_allow.as_str()),
                        (PermissionRule::Deny, labels.rule_deny.as_str()),
                    ] {
                        if ui
                            .selectable_label(tool.permission == rule, label)
                            .clicked()
                        {
                            offer_intent(
                                intent,
                                ConnectorIntent::SetPermission {
                                    server_id: server_id.clone(),
                                    tool_id: tool.id.clone(),
                                    rule,
                                },
                            );
                        }
                    }
                });
            });
            if let Some(description) = tool.description.as_deref() {
                ui.add_sized(
                    [
                        ui.available_width(),
                        ui.text_style_height(&egui::TextStyle::Body),
                    ],
                    Label::new(description).truncate(),
                );
            }
        },
    );
}

fn render_import_controls(
    ui: &mut Ui,
    labels: &Labels,
    paste_input: &mut String,
    intent: &mut Option<ConnectorIntent>,
) {
    ui.strong(&labels.import_title);
    ui.add(
        TextEdit::multiline(paste_input)
            .desired_rows(3)
            .char_limit(ResourceLimits::PRODUCTION_CEILING.import_input_bytes)
            .hint_text(r#"{"mcpServers":{"name":{"command":"..."}}}"#),
    );
    truncate_utf8(
        paste_input,
        ResourceLimits::PRODUCTION_CEILING.import_input_bytes,
    );
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(
                !paste_input.trim().is_empty(),
                Button::new(&labels.import_paste),
            )
            .clicked()
        {
            offer_intent(
                intent,
                ConnectorIntent::ImportConfiguration {
                    source: ImportSource::Paste,
                    display_name: None,
                    contents: SensitiveInput::from(mem::take(paste_input)),
                },
            );
        }
        if ui.button(&labels.import_file).clicked() {
            offer_intent(
                intent,
                ConnectorIntent::RequestImportSource(ImportSourceRequest::FilePicker),
            );
        }
        if ui.button(&labels.import_claude).clicked() {
            offer_intent(
                intent,
                ConnectorIntent::RequestImportSource(ImportSourceRequest::ClaudeDesktop),
            );
        }
    });
}

fn render_import_report(
    ui: &mut Ui,
    report: &ImportReport,
    prepared: &PreparedDisplayCache,
    labels: &Labels,
    intent: &mut Option<ConnectorIntent>,
) {
    ui.strong(&labels.import_report);
    ui.label(&prepared.import_summary);
    ScrollArea::vertical()
        .id_salt(("connector_import_report", report.operation_id.as_str()))
        .max_height(IMPORT_LIST_HEIGHT)
        .show_rows(ui, IMPORT_ROW_HEIGHT, report.items.len(), |ui, rows| {
            for index in rows {
                record_rendered_import_row();
                let item = &report.items[index];
                ui.horizontal(|ui| {
                    ui.strong(&item.name);
                    ui.label(import_outcome_label(item.outcome, labels));
                    if let Some(code) = item.error_code {
                        ui.label(error_code_label(code));
                    }
                    if item.omitted_secret_env_count > 0 {
                        ui.label(&prepared.import_omitted_counts[index]);
                        ui.label(&labels.secret_env_omitted);
                    }
                });
            }
        });
    if report.truncated {
        ui.weak(&labels.truncated);
    }
    if ui.button(&labels.close_import_report).clicked() {
        offer_intent(
            intent,
            ConnectorIntent::DismissImportReport(report.operation_id.clone()),
        );
    }
}

fn render_operations(
    ui: &mut Ui,
    snapshot: &ConnectorSnapshot,
    prepared: &PreparedDisplayCache,
    labels: &Labels,
    intent: &mut Option<ConnectorIntent>,
) {
    ui.strong(&labels.operations);
    ScrollArea::vertical()
        .id_salt("connector_operations")
        .max_height(OPERATION_LIST_HEIGHT)
        .show_rows(
            ui,
            OPERATION_ROW_HEIGHT,
            snapshot.operations.len(),
            |ui, rows| {
                for index in rows {
                    let operation = &snapshot.operations[index];
                    ui.horizontal(|ui| {
                        ui.label(&prepared.operation_labels[index]);
                        if operation_is_cancellable(operation)
                            && ui.button(&labels.cancel_operation).clicked()
                        {
                            offer_intent(intent, ConnectorIntent::Cancel(operation.id.clone()));
                        }
                    });
                }
            },
        );
}

fn render_add_server_modal(
    ui: &mut Ui,
    labels: &Labels,
    draft: &mut Option<ServerFormDraft>,
    intent: &mut Option<ConnectorIntent>,
) {
    let Some(current) = draft.as_mut() else {
        return;
    };
    let mut close = false;
    let mut save = false;
    egui::Window::new(&labels.add_server)
        .id(egui::Id::new("connector_add_server"))
        .collapsible(false)
        .resizable(false)
        .show(ui.ctx(), |ui| {
            ui.label(&labels.name);
            ui.add(TextEdit::singleline(&mut current.name).hint_text(&labels.name));
            ComboBox::from_id_salt("connector_transport")
                .selected_text(current.transport.label(labels))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut current.transport, FormTransport::Http, &labels.http);
                    ui.selectable_value(
                        &mut current.transport,
                        FormTransport::Stdio,
                        &labels.stdio,
                    );
                });
            match current.transport {
                FormTransport::Http => {
                    ui.label(&labels.url);
                    ui.add(TextEdit::singleline(&mut current.url).hint_text("https://"));
                }
                FormTransport::Stdio => {
                    ui.label(&labels.command);
                    ui.add(TextEdit::singleline(&mut current.command));
                    ui.label(&labels.arguments);
                    ui.add(TextEdit::multiline(&mut current.arguments).desired_rows(4));
                }
            }
            ui.checkbox(&mut current.enabled, &labels.enabled);
            ui.horizontal(|ui| {
                close = ui.button(&labels.cancel).clicked();
                save = ui
                    .add_enabled(current.is_valid(), Button::new(&labels.save))
                    .clicked();
            });
        });

    if close {
        *draft = None;
    } else if save {
        let current = draft.take().expect("draft exists while saving");
        offer_intent(intent, ConnectorIntent::SaveServer(current.into_contract()));
    }
}

fn render_invoke_modal(
    ui: &mut Ui,
    labels: &Labels,
    draft: &mut Option<InvokeDraft>,
    intent: &mut Option<ConnectorIntent>,
) {
    let Some(current) = draft.as_mut() else {
        return;
    };
    let mut close = false;
    let mut invoke = false;
    egui::Window::new(&current.tool_name)
        .id(egui::Id::new("connector_invoke"))
        .collapsible(false)
        .show(ui.ctx(), |ui| {
            ui.label(&labels.arguments_json);
            ui.add(
                TextEdit::multiline(&mut current.arguments_json)
                    .code_editor()
                    .char_limit(ResourceLimits::PRODUCTION_CEILING.tool_input_bytes)
                    .desired_rows(8),
            );
            truncate_utf8(
                &mut current.arguments_json,
                ResourceLimits::PRODUCTION_CEILING.tool_input_bytes,
            );
            ui.horizontal(|ui| {
                close = ui.button(&labels.cancel).clicked();
                invoke = ui.button(&labels.invoke).clicked();
            });
        });
    if close {
        *draft = None;
    } else if invoke {
        let mut current = draft.take().expect("draft exists while invoking");
        offer_intent(
            intent,
            ConnectorIntent::InvokeTool {
                server_id: current.server_id.clone(),
                tool_id: current.tool_id.clone(),
                arguments_json: SensitiveInput::from(mem::take(&mut current.arguments_json)),
            },
        );
    }
}

fn render_delete_modal(
    ui: &mut Ui,
    labels: &Labels,
    draft: &mut Option<DeleteServerDraft>,
    intent: &mut Option<ConnectorIntent>,
) {
    let Some(current) = draft.as_ref() else {
        return;
    };
    let mut close = false;
    let mut delete = false;
    egui::Window::new(&labels.delete)
        .id(egui::Id::new("connector_delete_server"))
        .collapsible(false)
        .resizable(false)
        .show(ui.ctx(), |ui| {
            ui.label(&current.server_name);
            ui.horizontal(|ui| {
                close = ui.button(&labels.cancel).clicked();
                delete = ui.button(&labels.delete).clicked();
            });
        });
    if close {
        *draft = None;
    } else if delete {
        let current = draft.take().expect("draft exists while deleting");
        offer_intent(intent, ConnectorIntent::DeleteServer(current.server_id));
    }
}

fn render_approval_modal(
    ui: &mut Ui,
    labels: &Labels,
    approval: &ApprovalPrompt,
    intent: &mut Option<ConnectorIntent>,
) {
    egui::Window::new(&labels.approval_needed)
        .id(egui::Id::new("connector_approval"))
        .collapsible(false)
        .show(ui.ctx(), |ui| {
            ui.strong(&approval.server_name);
            ui.label(&approval.tool_name);
            ui.label(approval_reason_label(approval, labels));
            ScrollArea::vertical()
                .id_salt("connector_approval_arguments")
                .max_height(120.0)
                .show(ui, |ui| {
                    ui.monospace(&approval.arguments_preview);
                });
            ui.horizontal_wrapped(|ui| {
                for (decision, label) in [
                    (ApprovalDecision::AllowOnce, labels.allow_once.as_str()),
                    (ApprovalDecision::AllowAlways, labels.allow_always.as_str()),
                    (ApprovalDecision::DenyOnce, labels.deny_once.as_str()),
                    (ApprovalDecision::DenyAlways, labels.deny_always.as_str()),
                ] {
                    if ui.button(label).clicked() {
                        offer_intent(
                            intent,
                            ConnectorIntent::ResolveApproval {
                                operation_id: approval.operation_id.clone(),
                                decision,
                            },
                        );
                    }
                }
            });
        });
}

fn render_remote_trust_modal(
    ui: &mut Ui,
    labels: &Labels,
    prompt: &RemoteTrustPrompt,
    intent: &mut Option<ConnectorIntent>,
) {
    egui::Window::new(&labels.remote_trust)
        .id(egui::Id::new("connector_remote_trust"))
        .collapsible(false)
        .resizable(false)
        .show(ui.ctx(), |ui| {
            ui.strong(&prompt.server_name);
            ui.label(remote_trust_purpose_label(prompt.purpose, labels));
            ui.monospace(prompt.display_endpoint.as_str());
            ui.horizontal(|ui| {
                if ui.button(&labels.trust_continue).clicked() {
                    offer_intent(intent, remote_trust_intent(prompt, true));
                }
                if ui.button(&labels.deny_trust).clicked() {
                    offer_intent(intent, remote_trust_intent(prompt, false));
                }
            });
        });
}

fn remote_trust_intent(prompt: &RemoteTrustPrompt, accepted: bool) -> ConnectorIntent {
    ConnectorIntent::ResolveRemoteTrust {
        operation_id: prompt.operation_id.clone(),
        config_revision: prompt.config_revision,
        endpoint_fingerprint: prompt.endpoint_fingerprint.clone(),
        accepted,
    }
}

fn render_oauth_modal(
    ui: &mut Ui,
    labels: &Labels,
    oauth: &OAuthUiState,
    client_draft: &mut Option<OAuthClientDraft>,
    intent: &mut Option<ConnectorIntent>,
) {
    egui::Window::new(&labels.oauth_title)
        .id(egui::Id::new("connector_oauth"))
        .collapsible(false)
        .resizable(false)
        .show(ui.ctx(), |ui| {
            ui.strong(&oauth.server_name);
            match &oauth.phase {
                OAuthUiPhase::DiscoveringAuth => {
                    render_waiting(ui, &labels.oauth_discovering);
                    render_oauth_cancel(ui, labels, oauth, intent);
                }
                OAuthUiPhase::AwaitingConsent {
                    authority,
                    resource,
                    scopes,
                } => {
                    ui.label(&labels.oauth_consent);
                    ui.label(&labels.authority);
                    ui.monospace(authority.as_str());
                    ui.label(&labels.resource);
                    ui.monospace(resource.as_str());
                    if !scopes.is_empty() {
                        ui.label(&labels.scopes);
                        ScrollArea::vertical()
                            .id_salt(("connector_oauth_scopes", oauth.operation_id.as_str()))
                            .max_height(OAUTH_SCOPE_LIST_HEIGHT)
                            .show_rows(ui, OAUTH_SCOPE_ROW_HEIGHT, scopes.len(), |ui, rows| {
                                for scope in &scopes[rows] {
                                    ui.monospace(scope);
                                }
                            });
                    }
                    ui.horizontal(|ui| {
                        if ui.button(&labels.oauth_continue).clicked() {
                            offer_intent(
                                intent,
                                ConnectorIntent::ResolveOAuthConsent {
                                    operation_id: oauth.operation_id.clone(),
                                    config_revision: oauth.config_revision,
                                    accepted: true,
                                },
                            );
                        }
                        if ui.button(&labels.oauth_deny).clicked() {
                            offer_intent(
                                intent,
                                ConnectorIntent::ResolveOAuthConsent {
                                    operation_id: oauth.operation_id.clone(),
                                    config_revision: oauth.config_revision,
                                    accepted: false,
                                },
                            );
                        }
                    });
                }
                OAuthUiPhase::AwaitingClient {
                    reason,
                    workspace_hint,
                } => {
                    ui.label(error_code_label(*reason));
                    if let Some(hint) = workspace_hint.as_deref() {
                        ui.weak(hint);
                    }
                    render_oauth_client_form(ui, labels, oauth, client_draft, intent);
                }
                OAuthUiPhase::PreparingCallback => {
                    render_waiting(ui, &labels.oauth_preparing_callback);
                    render_oauth_cancel(ui, labels, oauth, intent);
                }
                OAuthUiPhase::BrowserReady => {
                    render_waiting(ui, &labels.oauth_browser_ready);
                    ui.weak(&labels.oauth_browser_host_action);
                    render_oauth_cancel(ui, labels, oauth, intent);
                }
                OAuthUiPhase::AwaitingCallback => {
                    render_waiting(ui, &labels.oauth_waiting_callback);
                    render_oauth_cancel(ui, labels, oauth, intent);
                }
                OAuthUiPhase::Failed {
                    error_code,
                    recovery,
                } => {
                    ui.colored_label(ui.visuals().error_fg_color, error_code_label(*error_code));
                    ui.horizontal_wrapped(|ui| {
                        for action in recovery.iter().copied() {
                            if ui.button(oauth_recovery_label(action, labels)).clicked() {
                                offer_intent(
                                    intent,
                                    ConnectorIntent::ResolveOAuthRecovery {
                                        operation_id: oauth.operation_id.clone(),
                                        action,
                                    },
                                );
                            }
                        }
                        if ui.button(&labels.close).clicked() {
                            offer_intent(
                                intent,
                                ConnectorIntent::Cancel(oauth.operation_id.clone()),
                            );
                        }
                    });
                }
            }
        });
}

fn render_oauth_client_form(
    ui: &mut Ui,
    labels: &Labels,
    oauth: &OAuthUiState,
    draft: &mut Option<OAuthClientDraft>,
    intent: &mut Option<ConnectorIntent>,
) {
    let Some(current) = draft.as_mut() else {
        ui.label(&labels.oauth_client_unavailable);
        return;
    };
    ui.label(&labels.client_id);
    ui.add(
        TextEdit::singleline(&mut current.client_id)
            .char_limit(ResourceLimits::PRODUCTION_CEILING.import_input_bytes / 2),
    );
    ui.label(&labels.client_secret);
    ui.add(
        TextEdit::singleline(&mut current.client_secret)
            .password(true)
            .char_limit(ResourceLimits::PRODUCTION_CEILING.tool_input_bytes),
    );
    ui.label(&labels.workspace_hint);
    ui.add(
        TextEdit::singleline(&mut current.workspace_hint)
            .char_limit(ResourceLimits::PRODUCTION_CEILING.import_input_bytes / 2),
    );
    truncate_utf8(
        &mut current.client_id,
        ResourceLimits::PRODUCTION_CEILING.import_input_bytes / 2,
    );
    truncate_utf8(
        &mut current.client_secret,
        ResourceLimits::PRODUCTION_CEILING.tool_input_bytes,
    );
    truncate_utf8(
        &mut current.workspace_hint,
        ResourceLimits::PRODUCTION_CEILING.import_input_bytes / 2,
    );
    let can_submit = !current.client_id.trim().is_empty();
    let mut submit = false;
    let mut cancel = false;
    ui.horizontal(|ui| {
        submit = ui
            .add_enabled(can_submit, Button::new(&labels.save_client))
            .clicked();
        cancel = ui.button(&labels.cancel).clicked();
    });
    if submit {
        let mut submitted = draft.take().expect("OAuth client draft exists");
        let workspace_hint = if submitted.workspace_hint.trim().is_empty() {
            zero_string(&mut submitted.workspace_hint);
            None
        } else {
            Some(mem::take(&mut submitted.workspace_hint))
        };
        offer_intent(
            intent,
            ConnectorIntent::SubmitOAuthClient {
                operation_id: submitted.operation_id.clone(),
                config_revision: submitted.config_revision,
                server_id: submitted.server_id.clone(),
                client_id: mem::take(&mut submitted.client_id),
                client_secret: SensitiveInput::from(mem::take(&mut submitted.client_secret)),
                workspace_hint,
            },
        );
    } else if cancel {
        offer_intent(intent, ConnectorIntent::Cancel(oauth.operation_id.clone()));
        *draft = None;
    }
}

fn render_waiting(ui: &mut Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(text);
    });
}

fn render_oauth_cancel(
    ui: &mut Ui,
    labels: &Labels,
    oauth: &OAuthUiState,
    intent: &mut Option<ConnectorIntent>,
) {
    if ui.button(&labels.cancel).clicked() {
        offer_intent(intent, ConnectorIntent::Cancel(oauth.operation_id.clone()));
    }
}

fn sync_oauth_client_draft(draft: &mut Option<OAuthClientDraft>, oauth: Option<&OAuthUiState>) {
    let Some(oauth) = oauth else {
        *draft = None;
        return;
    };
    let OAuthUiPhase::AwaitingClient { .. } = &oauth.phase else {
        *draft = None;
        return;
    };
    let matches = draft.as_ref().is_some_and(|current| {
        current.operation_id == oauth.operation_id
            && current.config_revision == oauth.config_revision
            && current.server_id == oauth.server_id
    });
    if !matches {
        *draft = Some(OAuthClientDraft::from_state(oauth));
    }
}

fn offer_intent(slot: &mut Option<ConnectorIntent>, intent: ConnectorIntent) {
    if slot.is_none() {
        *slot = Some(intent);
    }
}

fn truncate_utf8(value: &mut String, maximum_bytes: usize) {
    if value.len() <= maximum_bytes {
        return;
    }
    let mut end = maximum_bytes;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value.truncate(end);
}

fn slack_connect_enabled(status: SlackStatus) -> bool {
    matches!(
        status,
        SlackStatus::NotConfigured
            | SlackStatus::Ready
            | SlackStatus::NeedsAuthorization
            | SlackStatus::Failed
    )
}

fn operation_is_cancellable(operation: &OperationSummary) -> bool {
    !matches!(
        operation.phase,
        OperationPhase::Succeeded
            | OperationPhase::Failed
            | OperationPhase::Unknown
            | OperationPhase::Denied
            | OperationPhase::Cancelled
    )
}

fn connection_label(state: ConnectionState, labels: &Labels) -> &str {
    match state {
        ConnectionState::Disabled => &labels.disabled,
        ConnectionState::Idle => &labels.unchecked,
        ConnectionState::Checking => &labels.checking,
        ConnectionState::NeedsAuthorization => &labels.needs_authorization,
        ConnectionState::Connected => &labels.connected,
        ConnectionState::Failed => &labels.failed,
    }
}

fn slack_status_label(status: SlackStatus, labels: &Labels) -> &str {
    match status {
        SlackStatus::NotConfigured => &labels.not_configured,
        SlackStatus::Ready => &labels.ready,
        SlackStatus::Checking => &labels.checking,
        SlackStatus::NeedsAuthorization => &labels.needs_authorization,
        SlackStatus::Connected => &labels.connected,
        SlackStatus::Failed => &labels.failed,
    }
}

fn slack_recovery_label(kind: SlackRecoveryKind, labels: &Labels) -> &str {
    match kind {
        SlackRecoveryKind::ConfigureApp => &labels.configure_slack_app,
        SlackRecoveryKind::EnableMcpAccess => &labels.enable_slack_mcp,
        SlackRecoveryKind::RetryAuthorization => &labels.retry_authorization,
    }
}

fn transport_label(kind: TransportKind, labels: &Labels) -> &str {
    match kind {
        TransportKind::Stdio => &labels.stdio,
        TransportKind::Http => &labels.http,
    }
}

fn permission_label(rule: PermissionRule, labels: &Labels) -> &str {
    match rule {
        PermissionRule::Ask => &labels.rule_ask,
        PermissionRule::Allow => &labels.rule_allow,
        PermissionRule::Deny => &labels.rule_deny,
    }
}

fn approval_reason_label<'a>(approval: &ApprovalPrompt, labels: &'a Labels) -> &'a str {
    match approval.reason {
        connector_contract::ApprovalReason::AskRule => &labels.approval_ask,
        connector_contract::ApprovalReason::FirstUse => &labels.approval_first_use,
        connector_contract::ApprovalReason::SchemaChanged => &labels.approval_schema_changed,
    }
}

fn remote_trust_purpose_label(purpose: RemoteTrustPurpose, labels: &Labels) -> &str {
    match purpose {
        RemoteTrustPurpose::Discover => &labels.trust_discover,
        RemoteTrustPurpose::Invoke => &labels.trust_invoke,
        RemoteTrustPurpose::OAuth => &labels.trust_oauth,
    }
}

fn import_outcome_label(outcome: ImportOutcome, labels: &Labels) -> &str {
    match outcome {
        ImportOutcome::Added => &labels.import_added,
        ImportOutcome::SkippedDuplicate => &labels.import_duplicate,
        ImportOutcome::SkippedUnsupported => &labels.import_unsupported,
        ImportOutcome::Failed => &labels.import_failed,
    }
}

fn oauth_recovery_label(action: OAuthRecoveryAction, labels: &Labels) -> &str {
    match action {
        OAuthRecoveryAction::Retry => &labels.retry_oauth,
        OAuthRecoveryAction::ChooseWorkspace => &labels.choose_workspace,
        OAuthRecoveryAction::OpenSlackMcpSettings => &labels.enable_slack_mcp,
    }
}

fn operation_kind_label(kind: OperationKind) -> &'static str {
    match kind {
        OperationKind::Trust => "Trust",
        OperationKind::Discover => "Discover",
        OperationKind::Invoke => "Tool call",
        OperationKind::OAuth => "OAuth",
        OperationKind::Import => "Import",
        OperationKind::SaveServer => "Save server",
        OperationKind::DeleteServer => "Delete server",
        OperationKind::UpdatePermission => "Permission",
    }
}

fn operation_phase_label(phase: OperationPhase) -> &'static str {
    match phase {
        OperationPhase::Queued => "Queued",
        OperationPhase::Validating => "Validating",
        OperationPhase::AwaitingTrust => "Awaiting trust",
        OperationPhase::DiscoveringSchema => "Discovering schema",
        OperationPhase::DiscoveringAuth => "Discovering authorization",
        OperationPhase::AwaitingConsent => "Awaiting consent",
        OperationPhase::AwaitingClient => "Awaiting client",
        OperationPhase::PreparingCallback => "Preparing callback",
        OperationPhase::BrowserReady => "Browser ready",
        OperationPhase::AwaitingCallback => "Awaiting callback",
        OperationPhase::Authorizing => "Authorizing",
        OperationPhase::AuditPreflight => "Audit preflight",
        OperationPhase::Calling => "Calling",
        OperationPhase::Persisting => "Persisting",
        OperationPhase::Succeeded => "Succeeded",
        OperationPhase::Failed => "Failed",
        OperationPhase::Unknown => "Delivery unknown",
        OperationPhase::Denied => "Denied",
        OperationPhase::Cancelled => "Cancelled",
    }
}

fn error_code_label(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::InvalidInput => "Invalid input",
        ErrorCode::InvalidUrl => "Invalid URL",
        ErrorCode::LimitExceeded => "Resource limit exceeded",
        ErrorCode::Backpressure => "Too many pending operations",
        ErrorCode::StorageUnavailable => "Storage unavailable",
        ErrorCode::SecretUnavailable => "Credential unavailable",
        ErrorCode::TrustDenied => "Remote trust denied",
        ErrorCode::HostUnavailable => "Host action unavailable",
        ErrorCode::PermissionDenied => "Permission denied",
        ErrorCode::AuditUnavailable => "Audit unavailable",
        ErrorCode::AuthenticationRequired => "Authentication required",
        ErrorCode::AuthenticationFailed => "Authentication failed",
        ErrorCode::OAuthCallbackFailed => "OAuth callback failed",
        ErrorCode::NetworkTimeout => "Network timeout",
        ErrorCode::TransportFailed => "Transport failed",
        ErrorCode::ProtocolViolation => "Protocol violation",
        ErrorCode::StaleResult => "Stale result discarded",
        ErrorCode::Cancelled => "Cancelled",
        ErrorCode::UnknownDelivery => "Delivery status unknown; not retried",
        ErrorCode::Internal => "Internal error",
    }
}

#[derive(Default)]
struct PreparedDisplayCache {
    revision: Option<Revision>,
    slack_tool_count: String,
    server_tool_counts: Vec<String>,
    tool_total: String,
    import_summary: String,
    import_omitted_counts: Vec<String>,
    operation_labels: Vec<String>,
    rebuild_count: usize,
}

impl PreparedDisplayCache {
    fn invalidate(&mut self) {
        self.revision = None;
    }

    fn prepare(&mut self, snapshot: &ConnectorSnapshot) {
        if self.revision == Some(snapshot.revision) {
            return;
        }
        self.revision = Some(snapshot.revision);
        self.rebuild_count = self.rebuild_count.saturating_add(1);

        self.slack_tool_count = snapshot.slack.tool_count.to_string();
        self.server_tool_counts.clear();
        self.server_tool_counts.extend(
            snapshot
                .servers
                .iter()
                .map(|server| server.tool_count.to_string()),
        );
        self.tool_total = snapshot
            .tool_page
            .as_ref()
            .map_or_else(String::new, |page| page.total.to_string());

        self.import_summary.clear();
        self.import_omitted_counts.clear();
        if let Some(report) = snapshot.import_report.as_ref() {
            self.import_summary = format!(
                "{} added · {} skipped · {} failed",
                report.added, report.skipped, report.failed
            );
            self.import_omitted_counts.extend(
                report
                    .items
                    .iter()
                    .map(|item| item.omitted_secret_env_count.to_string()),
            );
        }

        self.operation_labels.clear();
        self.operation_labels
            .extend(snapshot.operations.iter().map(|operation| {
                let mut label = format!(
                    "{} · {}",
                    operation_kind_label(operation.kind),
                    operation_phase_label(operation.phase)
                );
                if let Some(code) = operation.error_code {
                    label.push_str(" · ");
                    label.push_str(error_code_label(code));
                }
                label
            }));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormTransport {
    Http,
    Stdio,
}

impl FormTransport {
    fn label(self, labels: &Labels) -> &str {
        match self {
            Self::Http => &labels.http,
            Self::Stdio => &labels.stdio,
        }
    }
}

struct ServerFormDraft {
    id: Option<ServerId>,
    name: String,
    transport: FormTransport,
    url: String,
    command: String,
    arguments: String,
    plain_env: Vec<(String, String)>,
    secret_env: Vec<(String, connector_contract::CredentialId)>,
    inherit_env: bool,
    enabled: bool,
}

impl Default for ServerFormDraft {
    fn default() -> Self {
        Self {
            id: None,
            name: String::new(),
            transport: FormTransport::Http,
            url: String::new(),
            command: String::new(),
            arguments: String::new(),
            plain_env: Vec::new(),
            secret_env: Vec::new(),
            inherit_env: true,
            enabled: true,
        }
    }
}

impl ServerFormDraft {
    fn from_contract(config: &ServerDraft) -> Self {
        let (transport, url, command, arguments, plain_env, secret_env, inherit_env) =
            match &config.transport {
                TransportDraft::Http { url } => (
                    FormTransport::Http,
                    url.clone(),
                    String::new(),
                    String::new(),
                    Vec::new(),
                    Vec::new(),
                    true,
                ),
                TransportDraft::Stdio {
                    command,
                    args,
                    plain_env,
                    secret_env,
                    inherit_env,
                } => (
                    FormTransport::Stdio,
                    String::new(),
                    command.clone(),
                    args.join("\n"),
                    plain_env.clone(),
                    secret_env.clone(),
                    *inherit_env,
                ),
            };
        Self {
            id: config.id.clone(),
            name: config.name.clone(),
            transport,
            url,
            command,
            arguments,
            plain_env,
            secret_env,
            inherit_env,
            enabled: config.enabled,
        }
    }

    fn is_valid(&self) -> bool {
        !self.name.trim().is_empty()
            && match self.transport {
                FormTransport::Http => !self.url.trim().is_empty(),
                FormTransport::Stdio => !self.command.trim().is_empty(),
            }
    }

    fn into_contract(self) -> ServerDraft {
        let transport = match self.transport {
            FormTransport::Http => TransportDraft::Http { url: self.url },
            FormTransport::Stdio => TransportDraft::Stdio {
                command: self.command,
                args: self
                    .arguments
                    .lines()
                    .map(str::trim)
                    .filter(|arg| !arg.is_empty())
                    .map(str::to_owned)
                    .collect(),
                plain_env: self.plain_env,
                secret_env: self.secret_env,
                inherit_env: self.inherit_env,
            },
        };
        ServerDraft {
            id: self.id,
            name: self.name,
            transport,
            enabled: self.enabled,
        }
    }
}

struct InvokeDraft {
    server_id: ServerId,
    tool_id: connector_contract::ToolId,
    tool_name: String,
    arguments_json: String,
}

impl std::fmt::Debug for InvokeDraft {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("InvokeDraft(REDACTED)")
    }
}

impl Drop for InvokeDraft {
    fn drop(&mut self) {
        zero_string(&mut self.arguments_json);
    }
}

struct OAuthClientDraft {
    operation_id: connector_contract::OperationId,
    config_revision: Revision,
    server_id: ServerId,
    client_id: String,
    client_secret: String,
    workspace_hint: String,
}

impl std::fmt::Debug for OAuthClientDraft {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OAuthClientDraft(REDACTED)")
    }
}

impl OAuthClientDraft {
    fn from_state(oauth: &OAuthUiState) -> Self {
        Self {
            operation_id: oauth.operation_id.clone(),
            config_revision: oauth.config_revision,
            server_id: oauth.server_id.clone(),
            client_id: String::new(),
            client_secret: String::new(),
            workspace_hint: String::new(),
        }
    }
}

impl Drop for OAuthClientDraft {
    fn drop(&mut self) {
        zero_string(&mut self.client_id);
        zero_string(&mut self.client_secret);
        zero_string(&mut self.workspace_hint);
    }
}

struct DeleteServerDraft {
    server_id: ServerId,
    server_name: String,
}

fn zero_string(value: &mut String) {
    // SAFETY: the draft owns the string, and `as_mut_vec` is used only to overwrite existing bytes.
    for byte in unsafe { value.as_mut_vec() } {
        // SAFETY: `byte` is exclusively borrowed from the owned string allocation.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

struct Labels {
    title: String,
    add_server: String,
    import_title: String,
    import_paste: String,
    import_file: String,
    import_claude: String,
    import_report: String,
    close_import_report: String,
    import_added: String,
    import_duplicate: String,
    import_unsupported: String,
    import_failed: String,
    secret_env_omitted: String,
    open_slack_settings: String,
    connect_slack: String,
    choose_workspace: String,
    configure_slack_app: String,
    enable_slack_mcp: String,
    retry_authorization: String,
    workspace: String,
    refresh: String,
    empty: String,
    ready: String,
    not_configured: String,
    checking: String,
    needs_authorization: String,
    connected: String,
    failed: String,
    disabled: String,
    unchecked: String,
    tools: String,
    discover: String,
    edit: String,
    authorize: String,
    delete: String,
    tools_not_loaded: String,
    load_tools: String,
    load_more: String,
    invoke: String,
    rule_ask: String,
    rule_allow: String,
    rule_deny: String,
    result: String,
    truncated: String,
    close: String,
    name: String,
    http: String,
    stdio: String,
    url: String,
    command: String,
    arguments: String,
    enabled: String,
    cancel: String,
    save: String,
    arguments_json: String,
    client_id: String,
    client_secret: String,
    workspace_hint: String,
    save_client: String,
    approval_needed: String,
    approval_ask: String,
    approval_first_use: String,
    approval_schema_changed: String,
    allow_once: String,
    allow_always: String,
    deny_once: String,
    deny_always: String,
    remote_trust: String,
    trust_continue: String,
    deny_trust: String,
    trust_discover: String,
    trust_invoke: String,
    trust_oauth: String,
    oauth_title: String,
    oauth_discovering: String,
    oauth_consent: String,
    oauth_continue: String,
    oauth_deny: String,
    oauth_preparing_callback: String,
    oauth_browser_ready: String,
    oauth_browser_host_action: String,
    oauth_waiting_callback: String,
    oauth_client_unavailable: String,
    authority: String,
    resource: String,
    scopes: String,
    retry_oauth: String,
    operations: String,
    cancel_operation: String,
}

impl Labels {
    fn new(catalog: &Catalog) -> Self {
        Self {
            title: catalog.t("connectors.local_mcp", &[]),
            add_server: catalog.t("connectors.add_mcp", &[]),
            import_title: catalog.t("connectors.import_title", &[]),
            import_paste: "Import pasted JSON".to_owned(),
            import_file: catalog.t("connectors.import_file", &[]),
            import_claude: catalog.t("connectors.import_claude_desktop", &[]),
            import_report: "Import report".to_owned(),
            close_import_report: "Close import report".to_owned(),
            import_added: "Added".to_owned(),
            import_duplicate: "Skipped: duplicate".to_owned(),
            import_unsupported: "Skipped: unsupported".to_owned(),
            import_failed: "Failed".to_owned(),
            secret_env_omitted: "secret environment entries omitted".to_owned(),
            open_slack_settings: catalog.t("connectors.slack.open_app_settings", &[]),
            connect_slack: "Connect Slack".to_owned(),
            choose_workspace: "Choose another workspace".to_owned(),
            configure_slack_app: "Configure Slack app".to_owned(),
            enable_slack_mcp: "Enable Slack MCP access".to_owned(),
            retry_authorization: "Retry Slack authorization".to_owned(),
            workspace: "Workspace:".to_owned(),
            refresh: "Refresh".to_owned(),
            empty: catalog.t("connectors.empty_mcp", &[]),
            ready: catalog.t("connectors.slack.ready", &[]),
            not_configured: catalog.t("connectors.empty_mcp", &[]),
            checking: catalog.t("connectors.checking", &[]),
            needs_authorization: catalog.t("connectors.needs_auth", &[]),
            connected: catalog.t("connectors.connected", &[]),
            failed: catalog.t("connectors.failed", &[("message", "")]),
            disabled: "Disabled".to_owned(),
            unchecked: catalog.t("connectors.unchecked", &[]),
            tools: "Tools".to_owned(),
            discover: catalog.t("connectors.test", &[]),
            edit: "Edit".to_owned(),
            authorize: catalog.t("connectors.approve_browser", &[]),
            delete: catalog.t("action.delete", &[]),
            tools_not_loaded: catalog.t("connectors.unchecked", &[]),
            load_tools: "Load tools".to_owned(),
            load_more: catalog.t("inbox.view_all", &[]),
            invoke: catalog.t("connectors.invoke", &[]),
            rule_ask: catalog.t("connectors.approval.ask_rule", &[]),
            rule_allow: catalog.t("connectors.rule_allow", &[]),
            rule_deny: catalog.t("connectors.rule_deny", &[]),
            result: catalog.t("connectors.result", &[]),
            truncated: "Truncated".to_owned(),
            close: catalog.t("action.close", &[]),
            name: catalog.t("common.name", &[]),
            http: "HTTP".to_owned(),
            stdio: "stdio".to_owned(),
            url: "URL".to_owned(),
            command: catalog.t("common.command", &[]),
            arguments: catalog.t("connectors.args_note", &[]),
            enabled: "Enabled".to_owned(),
            cancel: catalog.t("action.cancel", &[]),
            save: catalog.t("action.save", &[]),
            arguments_json: catalog.t("connectors.arguments_json", &[]),
            client_id: catalog.t("connectors.client_id", &[]),
            client_secret: catalog.t("connectors.client_secret", &[]),
            workspace_hint: catalog.t("connectors.slack.workspace_address", &[]),
            save_client: "Save client and continue".to_owned(),
            approval_needed: catalog.t("connectors.approval_needed", &[("reason", "")]),
            approval_ask: catalog.t("connectors.approval.ask_rule", &[]),
            approval_first_use: catalog.t("connectors.approval.first_use", &[]),
            approval_schema_changed: catalog.t("connectors.approval.schema_changed", &[]),
            allow_once: catalog.t("connectors.allow_once", &[]),
            allow_always: catalog.t("connectors.allow_always", &[]),
            deny_once: catalog.t("connectors.deny_once", &[]),
            deny_always: catalog.t("connectors.deny_always", &[]),
            remote_trust: "Trust remote endpoint".to_owned(),
            trust_continue: "Trust and continue".to_owned(),
            deny_trust: "Deny remote endpoint".to_owned(),
            trust_discover: "This endpoint will receive a schema discovery request.".to_owned(),
            trust_invoke: "This endpoint will receive tool arguments.".to_owned(),
            trust_oauth: "This endpoint will begin OAuth discovery.".to_owned(),
            oauth_title: catalog.t("connectors.oauth_flow_title", &[]),
            oauth_discovering: "Discovering authorization".to_owned(),
            oauth_consent: "Review the authorization authority before continuing.".to_owned(),
            oauth_continue: "Continue to authorization".to_owned(),
            oauth_deny: "Deny authorization".to_owned(),
            oauth_preparing_callback: "Preparing local OAuth callback".to_owned(),
            oauth_browser_ready: "Authorization browser is ready".to_owned(),
            oauth_browser_host_action: "The app host is opening the approved authorization URL."
                .to_owned(),
            oauth_waiting_callback: "Waiting for OAuth callback".to_owned(),
            oauth_client_unavailable: "OAuth client input is unavailable.".to_owned(),
            authority: "Authorization authority".to_owned(),
            resource: "Resource".to_owned(),
            scopes: "Requested scopes".to_owned(),
            retry_oauth: "Retry OAuth".to_owned(),
            operations: "Active operations".to_owned(),
            cancel_operation: "Cancel operation".to_owned(),
        }
    }
}

#[cfg(test)]
thread_local! {
    static RENDERED_TOOL_ROWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static RENDERED_IMPORT_ROWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[inline]
fn record_rendered_tool_row() {
    #[cfg(test)]
    RENDERED_TOOL_ROWS.with(|count| count.set(count.get().saturating_add(1)));
}

#[inline]
fn record_rendered_import_row() {
    #[cfg(test)]
    RENDERED_IMPORT_ROWS.with(|count| count.set(count.get().saturating_add(1)));
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use connector_contract::{
        EndpointDisplay, EndpointFingerprint, ImportReportItem, OperationId, SlackProjection,
        ToolId,
    };

    use super::*;

    fn raw_input() -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 720.0),
            )),
            ..Default::default()
        }
    }

    fn snapshot_with_tools(tool_count: usize) -> ConnectorSnapshot {
        let server_id = ServerId::new("server-1");
        let tools: Arc<[ToolListItem]> = (0..tool_count)
            .map(|index| ToolListItem {
                id: ToolId::new(format!("tool-{index}")),
                name: format!("Accessible tool {index}"),
                description: Some(format!("Description {index}")),
                permission: PermissionRule::Ask,
            })
            .collect::<Vec<_>>()
            .into();
        ConnectorSnapshot {
            revision: Revision(7),
            config_revision: Revision(3),
            slack: SlackProjection {
                status: SlackStatus::Connected,
                tool_count: 4,
                ..SlackProjection::default()
            },
            servers: Arc::from([ServerSummary {
                id: server_id.clone(),
                name: "Fake server".to_owned(),
                transport: TransportKind::Http,
                enabled: true,
                connection: ConnectionState::Connected,
                tool_count,
                error_code: None,
            }]),
            selected_server: Some(server_id.clone()),
            tool_page: Some(ToolPage {
                server_id,
                offset: 0,
                total: tool_count,
                items: tools,
            }),
            ..ConnectorSnapshot::default()
        }
    }

    fn oauth_state(phase: OAuthUiPhase) -> OAuthUiState {
        OAuthUiState {
            operation_id: OperationId::new("oauth-op-1"),
            server_id: ServerId::new("server-1"),
            server_name: "Slack".to_owned(),
            config_revision: Revision(9),
            phase,
        }
    }

    fn activate_accessible_label(
        connector_ui: &mut ConnectorUi,
        snapshot: &ConnectorSnapshot,
        label: &str,
    ) -> Option<ConnectorIntent> {
        let context = egui::Context::default();
        context.enable_accesskit();
        let first = context.run_ui(raw_input(), |ui| {
            assert!(connector_ui.render(ui, snapshot).is_none());
        });
        let update = first
            .platform_output
            .accesskit_update
            .expect("AccessKit output");
        let target_node = update
            .nodes
            .iter()
            .find_map(|(id, node)| (node.label() == Some(label)).then_some(*id))
            .unwrap_or_else(|| panic!("accessible label not found: {label}"));
        let mut input = raw_input();
        input.events.push(egui::Event::AccessKitActionRequest(
            egui::accesskit::ActionRequest {
                action: egui::accesskit::Action::Click,
                target_tree: update.tree_id,
                target_node,
                data: None,
            },
        ));
        let mut emitted = None;
        let _ = context.run_ui(input, |ui| {
            emitted = connector_ui.render(ui, snapshot);
        });
        emitted
    }

    fn accessible_labels(snapshot: &ConnectorSnapshot) -> Vec<String> {
        let context = egui::Context::default();
        context.enable_accesskit();
        let catalog = Catalog::load("en-US").unwrap();
        let mut connector_ui = ConnectorUi::new(&catalog);
        let output = context.run_ui(raw_input(), |ui| {
            assert!(connector_ui.render(ui, snapshot).is_none());
        });
        output
            .platform_output
            .accesskit_update
            .expect("AccessKit output")
            .nodes
            .iter()
            .filter_map(|(_, node)| node.label().or_else(|| node.value()).map(str::to_owned))
            .collect()
    }

    #[test]
    fn remote_trust_accept_and_deny_preserve_all_correlations() {
        let fingerprint = EndpointFingerprint::parse("a".repeat(64)).unwrap();
        let snapshot = ConnectorSnapshot {
            revision: Revision(1),
            remote_trust: Some(RemoteTrustPrompt {
                operation_id: OperationId::new("trust-op"),
                server_id: ServerId::new("remote"),
                server_name: "Remote".to_owned(),
                purpose: RemoteTrustPurpose::Invoke,
                display_endpoint: EndpointDisplay::new("https://example.test/mcp"),
                endpoint_fingerprint: fingerprint.clone(),
                config_revision: Revision(4),
            }),
            ..ConnectorSnapshot::default()
        };
        let catalog = Catalog::load("en-US").unwrap();
        let mut ui = ConnectorUi::new(&catalog);
        let accepted = activate_accessible_label(&mut ui, &snapshot, "Trust and continue");
        assert!(matches!(
            accepted,
            Some(ConnectorIntent::ResolveRemoteTrust {
                operation_id,
                config_revision: Revision(4),
                endpoint_fingerprint,
                accepted: true,
            }) if operation_id.as_str() == "trust-op" && endpoint_fingerprint == fingerprint
        ));

        let mut ui = ConnectorUi::new(&catalog);
        assert!(matches!(
            activate_accessible_label(&mut ui, &snapshot, "Deny remote endpoint"),
            Some(ConnectorIntent::ResolveRemoteTrust {
                operation_id,
                config_revision: Revision(4),
                accepted: false,
                ..
            }) if operation_id.as_str() == "trust-op"
        ));
    }

    #[test]
    fn oauth_consent_and_client_submission_are_operation_and_config_bound() {
        let consent = ConnectorSnapshot {
            revision: Revision(2),
            oauth: Some(oauth_state(OAuthUiPhase::AwaitingConsent {
                authority: EndpointDisplay::new("https://auth.example.test"),
                resource: EndpointDisplay::new("https://mcp.example.test"),
                scopes: Arc::from(["tools.read".to_owned(), "tools.call".to_owned()]),
            })),
            ..ConnectorSnapshot::default()
        };
        let catalog = Catalog::load("en-US").unwrap();
        let mut ui = ConnectorUi::new(&catalog);
        assert!(matches!(
            activate_accessible_label(&mut ui, &consent, "Continue to authorization"),
            Some(ConnectorIntent::ResolveOAuthConsent {
                operation_id,
                config_revision: Revision(9),
                accepted: true,
            }) if operation_id.as_str() == "oauth-op-1"
        ));

        let awaiting_client = ConnectorSnapshot {
            revision: Revision(3),
            oauth: Some(oauth_state(OAuthUiPhase::AwaitingClient {
                reason: ErrorCode::AuthenticationRequired,
                workspace_hint: Some("example.slack.com".to_owned()),
            })),
            ..ConnectorSnapshot::default()
        };
        let mut ui = ConnectorUi::new(&catalog);
        let _ = egui::Context::default().run_ui(raw_input(), |egui_ui| {
            assert!(ui.render(egui_ui, &awaiting_client).is_none());
        });
        let draft = ui.oauth_client.as_mut().expect("client draft");
        draft.client_id = "client-id".to_owned();
        draft.client_secret = "client-secret".to_owned();
        draft.workspace_hint = "example.slack.com".to_owned();
        let submitted =
            activate_accessible_label(&mut ui, &awaiting_client, "Save client and continue");
        match submitted {
            Some(ConnectorIntent::SubmitOAuthClient {
                operation_id,
                config_revision,
                server_id,
                client_id,
                client_secret,
                workspace_hint,
            }) => {
                assert_eq!(operation_id.as_str(), "oauth-op-1");
                assert_eq!(config_revision, Revision(9));
                assert_eq!(server_id.as_str(), "server-1");
                assert_eq!(client_id, "client-id");
                assert_eq!(client_secret.expose_bytes(), b"client-secret");
                assert_eq!(workspace_hint.as_deref(), Some("example.slack.com"));
            }
            other => panic!("unexpected intent: {other:?}"),
        }
    }

    #[test]
    fn oauth_browser_callback_failure_and_recovery_are_rendered_without_raw_url_intents() {
        for (phase, visible) in [
            (OAuthUiPhase::DiscoveringAuth, "Discovering authorization"),
            (
                OAuthUiPhase::PreparingCallback,
                "Preparing local OAuth callback",
            ),
            (OAuthUiPhase::BrowserReady, "Authorization browser is ready"),
            (OAuthUiPhase::AwaitingCallback, "Waiting for OAuth callback"),
        ] {
            let snapshot = ConnectorSnapshot {
                revision: Revision(5),
                oauth: Some(oauth_state(phase)),
                ..ConnectorSnapshot::default()
            };
            assert!(
                accessible_labels(&snapshot)
                    .iter()
                    .any(|label| label == visible)
            );
        }

        let failed = ConnectorSnapshot {
            revision: Revision(6),
            oauth: Some(oauth_state(OAuthUiPhase::Failed {
                error_code: ErrorCode::OAuthCallbackFailed,
                recovery: Arc::from([
                    OAuthRecoveryAction::Retry,
                    OAuthRecoveryAction::ChooseWorkspace,
                    OAuthRecoveryAction::OpenSlackMcpSettings,
                ]),
            })),
            ..ConnectorSnapshot::default()
        };
        let catalog = Catalog::load("en-US").unwrap();
        let mut ui = ConnectorUi::new(&catalog);
        assert!(matches!(
            activate_accessible_label(&mut ui, &failed, "Retry OAuth"),
            Some(ConnectorIntent::ResolveOAuthRecovery {
                operation_id,
                action: OAuthRecoveryAction::Retry,
            }) if operation_id.as_str() == "oauth-op-1"
        ));
    }

    #[test]
    fn slack_projection_emits_connect_workspace_and_typed_recovery_intents() {
        let catalog = Catalog::load("en-US").unwrap();
        let mut ui = ConnectorUi::new(&catalog);
        assert!(matches!(
            activate_accessible_label(&mut ui, &ConnectorSnapshot::default(), "Connect Slack"),
            Some(ConnectorIntent::ConnectSlack)
        ));

        let connected = ConnectorSnapshot {
            revision: Revision(2),
            slack: SlackProjection {
                server_id: Some(ServerId::new("slack")),
                status: SlackStatus::Connected,
                tool_count: 12,
                workspace_label: Some("example.slack.com".to_owned()),
                can_choose_workspace: true,
                recovery: Some(SlackRecoveryKind::EnableMcpAccess),
            },
            ..ConnectorSnapshot::default()
        };
        let mut ui = ConnectorUi::new(&catalog);
        assert!(matches!(
            activate_accessible_label(&mut ui, &connected, "Choose another workspace"),
            Some(ConnectorIntent::ChooseSlackWorkspace(server_id))
                if server_id.as_str() == "slack"
        ));
        let mut ui = ConnectorUi::new(&catalog);
        assert!(matches!(
            activate_accessible_label(&mut ui, &connected, "Enable Slack MCP access"),
            Some(ConnectorIntent::OpenSlackRecovery {
                server_id,
                kind: SlackRecoveryKind::EnableMcpAccess,
            }) if server_id.as_str() == "slack"
        ));
    }

    #[test]
    fn paste_moves_sensitive_input_and_file_claude_and_links_are_typed_intents() {
        let catalog = Catalog::load("en-US").unwrap();
        let snapshot = ConnectorSnapshot::default();
        let mut ui = ConnectorUi::new(&catalog);
        ui.paste_input = "{\"mcpServers\":{}}".to_owned();
        match activate_accessible_label(&mut ui, &snapshot, "Import pasted JSON") {
            Some(ConnectorIntent::ImportConfiguration {
                source: ImportSource::Paste,
                display_name: None,
                contents,
            }) => assert_eq!(contents.expose_bytes(), b"{\"mcpServers\":{}}"),
            other => panic!("unexpected intent: {other:?}"),
        }
        assert!(ui.paste_input.is_empty());

        let mut ui = ConnectorUi::new(&catalog);
        assert!(matches!(
            activate_accessible_label(&mut ui, &snapshot, "From JSON file..."),
            Some(ConnectorIntent::RequestImportSource(
                ImportSourceRequest::FilePicker
            ))
        ));
        let mut ui = ConnectorUi::new(&catalog);
        let import_claude_label = ui.labels.import_claude.clone();
        assert!(matches!(
            activate_accessible_label(&mut ui, &snapshot, &import_claude_label),
            Some(ConnectorIntent::RequestImportSource(
                ImportSourceRequest::ClaudeDesktop
            ))
        ));
        let mut ui = ConnectorUi::new(&catalog);
        assert!(matches!(
            activate_accessible_label(&mut ui, &snapshot, "Open Slack app settings"),
            Some(ConnectorIntent::OpenExternalLink(
                ExternalLinkKind::SlackAppSettings
            ))
        ));
    }

    #[test]
    fn four_thousand_ninety_six_tools_render_only_viewport_rows() {
        RENDERED_TOOL_ROWS.with(|count| count.set(0));
        let context = egui::Context::default();
        let catalog = Catalog::load("en-US").unwrap();
        let mut ui = ConnectorUi::new(&catalog);
        let snapshot = snapshot_with_tools(4_096);
        let page_items = &snapshot.tool_page.as_ref().unwrap().items;
        let pointer = Arc::as_ptr(page_items);

        let _ = context.run_ui(raw_input(), |egui_ui| {
            assert!(ui.render(egui_ui, &snapshot).is_none());
        });

        let rendered = RENDERED_TOOL_ROWS.with(std::cell::Cell::get);
        let maximum_visible_with_overscan = (TOOL_LIST_HEIGHT / TOOL_ROW_HEIGHT) as usize + 2;
        assert!(rendered > 0);
        assert!(rendered <= maximum_visible_with_overscan);
        assert_eq!(
            pointer,
            Arc::as_ptr(&snapshot.tool_page.as_ref().unwrap().items)
        );
    }

    #[test]
    fn two_hundred_fifty_six_import_rows_are_virtualized() {
        RENDERED_IMPORT_ROWS.with(|count| count.set(0));
        let items: Arc<[ImportReportItem]> = (0..256)
            .map(|index| ImportReportItem {
                name: format!("server-{index}"),
                outcome: ImportOutcome::Added,
                error_code: None,
                omitted_secret_env_count: 0,
            })
            .collect::<Vec<_>>()
            .into();
        let snapshot = ConnectorSnapshot {
            revision: Revision(10),
            import_report: Some(ImportReport {
                operation_id: OperationId::new("import-op"),
                source: ImportSource::File,
                added: 256,
                skipped: 0,
                failed: 0,
                items,
                truncated: false,
            }),
            ..ConnectorSnapshot::default()
        };
        let context = egui::Context::default();
        let catalog = Catalog::load("en-US").unwrap();
        let mut ui = ConnectorUi::new(&catalog);
        let _ = context.run_ui(raw_input(), |egui_ui| {
            assert!(ui.render(egui_ui, &snapshot).is_none());
        });
        let rendered = RENDERED_IMPORT_ROWS.with(std::cell::Cell::get);
        let maximum_visible_with_overscan = (IMPORT_LIST_HEIGHT / IMPORT_ROW_HEIGHT) as usize + 2;
        assert!(rendered > 0);
        assert!(rendered <= maximum_visible_with_overscan);
    }

    #[test]
    fn operations_are_visible_and_unknown_delivery_never_offers_retry_or_cancel() {
        let snapshot = ConnectorSnapshot {
            revision: Revision(11),
            operations: Arc::from([
                OperationSummary {
                    id: OperationId::new("running"),
                    server_id: ServerId::new("server"),
                    kind: OperationKind::Invoke,
                    phase: OperationPhase::Calling,
                    error_code: None,
                },
                OperationSummary {
                    id: OperationId::new("unknown"),
                    server_id: ServerId::new("server"),
                    kind: OperationKind::Invoke,
                    phase: OperationPhase::Unknown,
                    error_code: Some(ErrorCode::UnknownDelivery),
                },
            ]),
            ..ConnectorSnapshot::default()
        };
        let labels = accessible_labels(&snapshot);
        assert!(
            labels
                .iter()
                .any(|label| label.contains("Delivery unknown"))
        );
        assert!(!labels.iter().any(|label| label.contains("Retry")));

        let catalog = Catalog::load("en-US").unwrap();
        let mut ui = ConnectorUi::new(&catalog);
        assert!(matches!(
            activate_accessible_label(&mut ui, &snapshot, "Cancel operation"),
            Some(ConnectorIntent::Cancel(operation_id)) if operation_id.as_str() == "running"
        ));
    }

    #[test]
    fn identical_revision_three_hundred_frames_do_not_rebuild_display_cache() {
        let snapshot = ConnectorSnapshot::default();
        let context = egui::Context::default();
        let catalog = Catalog::load("en-US").unwrap();
        let mut ui = ConnectorUi::new(&catalog);
        let _ = context.run_ui(raw_input(), |egui_ui| {
            assert!(ui.render(egui_ui, &snapshot).is_none());
        });
        assert_eq!(ui.prepared.rebuild_count, 1);

        for _ in 0..300 {
            let _ = context.run_ui(raw_input(), |egui_ui| {
                assert!(ui.render(egui_ui, &snapshot).is_none());
            });
        }
        assert_eq!(ui.prepared.rebuild_count, 1);
    }

    #[test]
    fn secret_bearing_drafts_have_redacted_debug_and_owned_buffers_are_wipeable() {
        let invoke = InvokeDraft {
            server_id: ServerId::new("server"),
            tool_id: ToolId::new("tool"),
            tool_name: "tool".to_owned(),
            arguments_json: "{\"token\":\"invoke-secret\"}".to_owned(),
        };
        assert_eq!(format!("{invoke:?}"), "InvokeDraft(REDACTED)");

        let oauth = oauth_state(OAuthUiPhase::AwaitingClient {
            reason: ErrorCode::AuthenticationRequired,
            workspace_hint: None,
        });
        let mut client = OAuthClientDraft::from_state(&oauth);
        client.client_id = "private-client".to_owned();
        client.client_secret = "oauth-secret".to_owned();
        client.workspace_hint = "private-workspace".to_owned();
        assert_eq!(format!("{client:?}"), "OAuthClientDraft(REDACTED)");

        let catalog = Catalog::load("en-US").unwrap();
        let mut ui = ConnectorUi::new(&catalog);
        ui.paste_input = "client_secret=paste-secret".to_owned();
        assert_eq!(format!("{ui:?}"), "ConnectorUi(REDACTED)");

        let mut owned = "wipe-me".to_owned();
        zero_string(&mut owned);
        assert!(owned.as_bytes().iter().all(|byte| *byte == 0));
    }

    #[test]
    fn tool_and_oauth_drafts_move_or_drop_sensitive_buffers_on_user_action() {
        let catalog = Catalog::load("en-US").unwrap();
        let snapshot = ConnectorSnapshot::default();
        let mut ui = ConnectorUi::new(&catalog);
        ui.invoke = Some(InvokeDraft {
            server_id: ServerId::new("server"),
            tool_id: ToolId::new("tool"),
            tool_name: "tool".to_owned(),
            arguments_json: "{\"token\":\"move-once\"}".to_owned(),
        });
        let invoke_label = ui.labels.invoke.clone();
        match activate_accessible_label(&mut ui, &snapshot, &invoke_label) {
            Some(ConnectorIntent::InvokeTool { arguments_json, .. }) => {
                assert_eq!(arguments_json.expose_bytes(), b"{\"token\":\"move-once\"}");
            }
            other => panic!("unexpected intent: {other:?}"),
        }
        assert!(ui.invoke.is_none());

        let awaiting_client = ConnectorSnapshot {
            revision: Revision(15),
            oauth: Some(oauth_state(OAuthUiPhase::AwaitingClient {
                reason: ErrorCode::AuthenticationRequired,
                workspace_hint: None,
            })),
            ..ConnectorSnapshot::default()
        };
        let mut ui = ConnectorUi::new(&catalog);
        let _ = egui::Context::default().run_ui(raw_input(), |egui_ui| {
            assert!(ui.render(egui_ui, &awaiting_client).is_none());
        });
        ui.oauth_client.as_mut().unwrap().client_secret = "drop-on-cancel".to_owned();
        let cancel_label = ui.labels.cancel.clone();
        assert!(matches!(
            activate_accessible_label(&mut ui, &awaiting_client, &cancel_label),
            Some(ConnectorIntent::Cancel(operation_id))
                if operation_id.as_str() == "oauth-op-1"
        ));
        assert!(ui.oauth_client.is_none());
    }

    #[test]
    fn selected_server_edit_roundtrips_hidden_env_bindings() {
        let original = ServerDraft {
            id: Some(ServerId::new("server-1")),
            name: "local".to_owned(),
            transport: TransportDraft::Stdio {
                command: "example".to_owned(),
                args: vec!["--one".to_owned()],
                plain_env: vec![("SAFE".to_owned(), "yes".to_owned())],
                secret_env: vec![(
                    "TOKEN".to_owned(),
                    connector_contract::CredentialId::new("credential-1"),
                )],
                inherit_env: false,
            },
            enabled: false,
        };
        let roundtrip = ServerFormDraft::from_contract(&original).into_contract();
        assert_eq!(roundtrip, original);
    }
}
