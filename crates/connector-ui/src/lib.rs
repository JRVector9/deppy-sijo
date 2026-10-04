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
pub mod popup;

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

/// Pure UI presets: selecting one only copies static text into the unfinished add form.
/// The filesystem root remains an explicit placeholder so this leaf never reads HOME or the
/// filesystem while rendering.
struct StdioPreset {
    name: &'static str,
    command: &'static str,
    arguments: &'static str,
}

const STDIO_PRESETS: &[StdioPreset] = &[
    StdioPreset {
        name: "filesystem",
        command: "npx",
        arguments: "-y\n@modelcontextprotocol/server-filesystem\n/absolute/path/to/allowed/directory",
    },
    StdioPreset {
        name: "memory",
        command: "npx",
        arguments: "-y\n@modelcontextprotocol/server-memory",
    },
    StdioPreset {
        name: "fetch",
        command: "uvx",
        arguments: "mcp-server-fetch",
    },
    StdioPreset {
        name: "everything",
        command: "npx",
        arguments: "-y\n@modelcontextprotocol/server-everything",
    },
];

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

    /// Removes every secret-bearing unfinished UI draft without disturbing non-sensitive add,
    /// delete, selection, translation, or prepared-display state.
    ///
    /// Call this when the Connector surface becomes hidden or its owning workspace changes.
    /// Service-owned operations are cancelled separately through [`ConnectorIntent::Cancel`].
    pub fn clear_sensitive_drafts(&mut self) {
        self.invoke = None;
        self.oauth_client = None;
        let mut paste_input = mem::take(&mut self.paste_input);
        zero_string(&mut paste_input);
    }

    /// Renders one frame from an immutable snapshot and returns at most one intent.
    #[must_use]
    pub fn render(&mut self, ui: &mut Ui, snapshot: &ConnectorSnapshot) -> Option<ConnectorIntent> {
        sync_oauth_client_draft(&mut self.oauth_client, snapshot.oauth.as_ref());
        self.prepared.prepare(snapshot, &self.labels);

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

        // Service consent owns interaction until resolved; retain local drafts for retry.
        if !has_service_modal {
            render_add_server_modal(ui, &self.labels, &mut self.add_server, &mut intent);
            render_invoke_modal(ui, &self.labels, &mut self.invoke, &mut intent);
            render_delete_modal(ui, &self.labels, &mut self.delete_server, &mut intent);
        }

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
            ui.strong(&labels.slack_name);
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
                        ui.label(error_code_label(code, labels));
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
    let mut open = true;
    popup::window(
        ui.ctx(),
        popup::WindowSpec {
            id: egui::Id::new("connector_add_server"),
            title: &labels.add_server,
            subtitle: "",
            close_label: &labels.cancel,
            close_enabled: true,
            default_size: egui::vec2(560.0, 500.0),
            min_size: egui::vec2(360.0, 300.0),
        },
        &mut open,
        |ui| {
            popup::window_body(ui, |ui| {
                ui.label(&labels.name);
                popup::text_input(ui, &mut current.name, &labels.name);
                popup::choice_input(
                    ui,
                    "connector_transport",
                    current.transport.label(labels),
                    |ui| {
                        ui.selectable_value(
                            &mut current.transport,
                            FormTransport::Http,
                            &labels.http,
                        );
                        ui.selectable_value(
                            &mut current.transport,
                            FormTransport::Stdio,
                            &labels.stdio,
                        );
                    },
                );
                match current.transport {
                    FormTransport::Http => {
                        ui.label(&labels.url);
                        popup::text_input(ui, &mut current.url, "https://");
                    }
                    FormTransport::Stdio => {
                        if current.id.is_none() {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(&labels.presets);
                                for preset in STDIO_PRESETS {
                                    if ui
                                        .small_button(preset.name)
                                        .on_hover_text(&labels.preset_hint)
                                        .clicked()
                                    {
                                        current.apply_preset(preset);
                                    }
                                }
                            });
                        }
                        ui.label(&labels.command);
                        popup::text_input(ui, &mut current.command, "");
                        ui.label(&labels.arguments);
                        ui.add_sized(
                            [ui.available_width(), 120.0],
                            TextEdit::multiline(&mut current.arguments)
                                .font(egui::FontId::proportional(13.0))
                                .margin(egui::Margin::symmetric(10, 8)),
                        );
                    }
                }
                ui.checkbox(&mut current.enabled, &labels.enabled);
            });
            popup::footer(ui, None, |ui| {
                save = popup::action_button(
                    ui,
                    &labels.save,
                    popup::ActionTone::Primary,
                    current.is_valid(),
                )
                .clicked();
                close = popup::action_button(ui, &labels.cancel, popup::ActionTone::Ghost, true)
                    .clicked();
            });
        },
    );

    if close || !open {
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
    let mut open = true;
    popup::window(
        ui.ctx(),
        popup::WindowSpec {
            id: egui::Id::new("connector_invoke"),
            title: &current.tool_name,
            subtitle: "",
            close_label: &labels.cancel,
            close_enabled: true,
            default_size: egui::vec2(620.0, 490.0),
            min_size: egui::vec2(360.0, 300.0),
        },
        &mut open,
        |ui| {
            popup::window_body(ui, |ui| {
                ui.label(&labels.arguments_json);
                ui.add_sized(
                    [ui.available_width(), ui.available_height().max(136.0)],
                    TextEdit::multiline(&mut current.arguments_json)
                        .code_editor()
                        .char_limit(ResourceLimits::PRODUCTION_CEILING.tool_input_bytes)
                        .desired_rows(1)
                        .margin(egui::Margin::symmetric(10, 8)),
                );
                truncate_utf8(
                    &mut current.arguments_json,
                    ResourceLimits::PRODUCTION_CEILING.tool_input_bytes,
                );
            });
            popup::footer(ui, None, |ui| {
                invoke = popup::action_button(ui, &labels.invoke, popup::ActionTone::Primary, true)
                    .clicked();
                close = popup::action_button(ui, &labels.cancel, popup::ActionTone::Ghost, true)
                    .clicked();
            });
        },
    );
    if close || !open {
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

#[derive(Clone, Copy)]
struct ApprovalFocusTarget {
    target: egui::Id,
    slot: bool,
}

fn render_approval_modal(
    ui: &mut Ui,
    labels: &Labels,
    approval: &ApprovalPrompt,
    intent: &mut Option<ConnectorIntent>,
) {
    // Keep one window Area, but require a fresh keyboard choice for each operation.
    let window_id = egui::Id::new("connector_approval");
    let target = window_id.with(approval.operation_id.as_str());
    let target_key = window_id.with(("target", ui.ctx().viewport_id()));
    let (changed, slot) = ui.ctx().data_mut(|data| {
        let previous = data.get_temp::<ApprovalFocusTarget>(target_key);
        let changed = previous.is_none_or(|previous| previous.target != target);
        let slot = previous.is_some_and(|previous| previous.slot ^ changed);
        data.insert_temp(target_key, ApprovalFocusTarget { target, slot });
        (changed, slot)
    });
    if changed {
        ui.memory_mut(|memory| {
            if let Some(focused) = memory.focused() {
                memory.surrender_focus(focused);
            }
        });
    }
    egui::Window::new(&labels.approval_needed)
        .id(window_id)
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
            // egui retains focus IDs until directional navigation. Two alternating
            // scopes isolate consecutive requests without retaining four IDs per request.
            ui.push_id(window_id.with(("actions", slot)), |ui| {
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
                })
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
                    ui.label(error_code_label(*reason, labels));
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
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        error_code_label(*error_code, labels),
                    );
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
    let open_slack_settings = ui.button(&labels.open_slack_settings).clicked();
    ui.horizontal(|ui| {
        submit = ui
            .add_enabled(can_submit, Button::new(&labels.save_client))
            .clicked();
        cancel = ui.button(&labels.cancel).clicked();
    });
    if open_slack_settings {
        offer_intent(
            intent,
            ConnectorIntent::OpenExternalLink(ExternalLinkKind::SlackAppSettings),
        );
    } else if submit {
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

fn operation_kind_label(kind: OperationKind, labels: &Labels) -> &str {
    match kind {
        OperationKind::Trust => &labels.operation_trust,
        OperationKind::Discover => &labels.operation_discover,
        OperationKind::Invoke => &labels.operation_invoke,
        OperationKind::OAuth => &labels.operation_oauth,
        OperationKind::Import => &labels.operation_import,
        OperationKind::SaveServer => &labels.operation_save_server,
        OperationKind::DeleteServer => &labels.operation_delete_server,
        OperationKind::UpdatePermission => &labels.operation_update_permission,
    }
}

fn operation_phase_label(phase: OperationPhase, labels: &Labels) -> &str {
    match phase {
        OperationPhase::Queued => &labels.phase_queued,
        OperationPhase::Validating => &labels.phase_validating,
        OperationPhase::AwaitingTrust => &labels.phase_awaiting_trust,
        OperationPhase::DiscoveringSchema => &labels.phase_discovering_schema,
        OperationPhase::DiscoveringAuth => &labels.phase_discovering_auth,
        OperationPhase::AwaitingConsent => &labels.phase_awaiting_consent,
        OperationPhase::AwaitingClient => &labels.phase_awaiting_client,
        OperationPhase::PreparingCallback => &labels.phase_preparing_callback,
        OperationPhase::BrowserReady => &labels.phase_browser_ready,
        OperationPhase::AwaitingCallback => &labels.phase_awaiting_callback,
        OperationPhase::Authorizing => &labels.phase_authorizing,
        OperationPhase::AuditPreflight => &labels.phase_audit_preflight,
        OperationPhase::Calling => &labels.phase_calling,
        OperationPhase::Persisting => &labels.phase_persisting,
        OperationPhase::Succeeded => &labels.phase_succeeded,
        OperationPhase::Failed => &labels.phase_failed,
        OperationPhase::Unknown => &labels.phase_unknown,
        OperationPhase::Denied => &labels.phase_denied,
        OperationPhase::Cancelled => &labels.phase_cancelled,
    }
}

fn error_code_label(code: ErrorCode, labels: &Labels) -> &str {
    match code {
        ErrorCode::InvalidInput => &labels.error_invalid_input,
        ErrorCode::InvalidUrl => &labels.error_invalid_url,
        ErrorCode::LimitExceeded => &labels.error_limit_exceeded,
        ErrorCode::Backpressure => &labels.error_backpressure,
        ErrorCode::StorageUnavailable => &labels.error_storage_unavailable,
        ErrorCode::SecretUnavailable => &labels.error_secret_unavailable,
        ErrorCode::TrustDenied => &labels.error_trust_denied,
        ErrorCode::HostUnavailable => &labels.error_host_unavailable,
        ErrorCode::PermissionDenied => &labels.error_permission_denied,
        ErrorCode::AuditUnavailable => &labels.error_audit_unavailable,
        ErrorCode::AuthenticationRequired => &labels.error_authentication_required,
        ErrorCode::AuthenticationFailed => &labels.error_authentication_failed,
        ErrorCode::OAuthCallbackFailed => &labels.error_oauth_callback_failed,
        ErrorCode::NetworkTimeout => &labels.error_network_timeout,
        ErrorCode::TransportFailed => &labels.error_transport_failed,
        ErrorCode::ProtocolViolation => &labels.error_protocol_violation,
        ErrorCode::StaleResult => &labels.error_stale_result,
        ErrorCode::Cancelled => &labels.error_cancelled,
        ErrorCode::UnknownDelivery => &labels.error_unknown_delivery,
        ErrorCode::Internal => &labels.error_internal,
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

    fn prepare(&mut self, snapshot: &ConnectorSnapshot, labels: &Labels) {
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
            let added = report.added.to_string();
            let skipped = report.skipped.to_string();
            let failed = report.failed.to_string();
            self.import_summary = interpolate_label(
                &labels.import_summary_template,
                &[
                    ("added", &added),
                    ("skipped", &skipped),
                    ("failed", &failed),
                ],
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
                    operation_kind_label(operation.kind, labels),
                    operation_phase_label(operation.phase, labels)
                );
                if let Some(code) = operation.error_code {
                    label.push_str(" · ");
                    label.push_str(error_code_label(code, labels));
                }
                label
            }));
    }
}

fn interpolate_label(template: &str, args: &[(&str, &str)]) -> String {
    let mut rendered = template.to_owned();
    for (name, value) in args {
        rendered = rendered.replace(&format!("{{{name}}}"), value);
    }
    rendered
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
    fn apply_preset(&mut self, preset: &StdioPreset) {
        self.transport = FormTransport::Stdio;
        self.name.clear();
        self.name.push_str(preset.name);
        self.command.clear();
        self.command.push_str(preset.command);
        self.arguments.clear();
        self.arguments.push_str(preset.arguments);
        self.url.clear();
    }

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
    import_summary_template: String,
    open_slack_settings: String,
    slack_name: String,
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
    presets: String,
    preset_hint: String,
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
    operation_trust: String,
    operation_discover: String,
    operation_invoke: String,
    operation_oauth: String,
    operation_import: String,
    operation_save_server: String,
    operation_delete_server: String,
    operation_update_permission: String,
    phase_queued: String,
    phase_validating: String,
    phase_awaiting_trust: String,
    phase_discovering_schema: String,
    phase_discovering_auth: String,
    phase_awaiting_consent: String,
    phase_awaiting_client: String,
    phase_preparing_callback: String,
    phase_browser_ready: String,
    phase_awaiting_callback: String,
    phase_authorizing: String,
    phase_audit_preflight: String,
    phase_calling: String,
    phase_persisting: String,
    phase_succeeded: String,
    phase_failed: String,
    phase_unknown: String,
    phase_denied: String,
    phase_cancelled: String,
    error_invalid_input: String,
    error_invalid_url: String,
    error_limit_exceeded: String,
    error_backpressure: String,
    error_storage_unavailable: String,
    error_secret_unavailable: String,
    error_trust_denied: String,
    error_host_unavailable: String,
    error_permission_denied: String,
    error_audit_unavailable: String,
    error_authentication_required: String,
    error_authentication_failed: String,
    error_oauth_callback_failed: String,
    error_network_timeout: String,
    error_transport_failed: String,
    error_protocol_violation: String,
    error_stale_result: String,
    error_cancelled: String,
    error_unknown_delivery: String,
    error_internal: String,
}

impl Labels {
    fn new(catalog: &Catalog) -> Self {
        Self {
            title: catalog.t("connectors.local_mcp", &[]),
            add_server: catalog.t("connectors.add_mcp", &[]),
            import_title: catalog.t("connectors.import_title", &[]),
            import_paste: catalog.t("connector.import.paste", &[]),
            import_file: catalog.t("connectors.import_file", &[]),
            import_claude: catalog.t("connectors.import_claude_desktop", &[]),
            import_report: catalog.t("connector.import.report_title", &[]),
            close_import_report: catalog.t("connector.import.close_report", &[]),
            import_added: catalog.t("connector.import.outcome.added", &[]),
            import_duplicate: catalog.t("connector.import.outcome.skipped_duplicate", &[]),
            import_unsupported: catalog.t("connector.import.outcome.skipped_unsupported", &[]),
            import_failed: catalog.t("connector.import.outcome.failed", &[]),
            secret_env_omitted: catalog.t("connector.import.secret_env_omitted", &[]),
            import_summary_template: catalog.t("connector.import.summary", &[]),
            open_slack_settings: catalog.t("connectors.slack.open_app_settings", &[]),
            slack_name: catalog.t("connector.slack.name", &[]),
            connect_slack: catalog.t("connector.slack.connect", &[]),
            choose_workspace: catalog.t("connector.slack.choose_workspace", &[]),
            configure_slack_app: catalog.t("connector.slack.configure_app", &[]),
            enable_slack_mcp: catalog.t("connector.slack.enable_mcp", &[]),
            retry_authorization: catalog.t("connector.slack.retry_authorization", &[]),
            workspace: catalog.t("connector.slack.workspace_label", &[]),
            refresh: catalog.t("connector.action.refresh", &[]),
            empty: catalog.t("connectors.empty_mcp", &[]),
            ready: catalog.t("connectors.slack.ready", &[]),
            not_configured: catalog.t("connectors.empty_mcp", &[]),
            checking: catalog.t("connectors.checking", &[]),
            needs_authorization: catalog.t("connectors.needs_auth", &[]),
            connected: catalog.t("connectors.connected", &[]),
            failed: catalog.t("connector.status.failed", &[]),
            disabled: catalog.t("connector.status.disabled", &[]),
            unchecked: catalog.t("connectors.unchecked", &[]),
            tools: catalog.t("connector.tools.label", &[]),
            discover: catalog.t("connectors.test", &[]),
            edit: catalog.t("connector.action.edit", &[]),
            authorize: catalog.t("connectors.approve_browser", &[]),
            delete: catalog.t("action.delete", &[]),
            tools_not_loaded: catalog.t("connectors.unchecked", &[]),
            load_tools: catalog.t("connector.action.load_tools", &[]),
            load_more: catalog.t("inbox.view_all", &[]),
            invoke: catalog.t("connectors.invoke", &[]),
            rule_ask: catalog.t("connectors.approval.ask_rule", &[]),
            rule_allow: catalog.t("connectors.rule_allow", &[]),
            rule_deny: catalog.t("connectors.rule_deny", &[]),
            result: catalog.t("connectors.result", &[]),
            truncated: catalog.t("connector.status.truncated", &[]),
            close: catalog.t("action.close", &[]),
            name: catalog.t("common.name", &[]),
            http: "HTTP".to_owned(),
            stdio: "stdio".to_owned(),
            url: "URL".to_owned(),
            command: catalog.t("common.command", &[]),
            arguments: catalog.t("connectors.args_note", &[]),
            presets: catalog.t("connectors.presets", &[]),
            preset_hint: catalog.t("connectors.preset_hint", &[]),
            enabled: catalog.t("connector.status.enabled", &[]),
            cancel: catalog.t("action.cancel", &[]),
            save: catalog.t("action.save", &[]),
            arguments_json: catalog.t("connectors.arguments_json", &[]),
            client_id: catalog.t("connectors.client_id", &[]),
            client_secret: catalog.t("connectors.client_secret", &[]),
            workspace_hint: catalog.t("connectors.slack.workspace_address", &[]),
            save_client: catalog.t("connector.action.save_client_continue", &[]),
            approval_needed: catalog.t("connectors.approval_needed", &[("reason", "")]),
            approval_ask: catalog.t("connectors.approval.ask_rule", &[]),
            approval_first_use: catalog.t("connectors.approval.first_use", &[]),
            approval_schema_changed: catalog.t("connectors.approval.schema_changed", &[]),
            allow_once: catalog.t("connectors.allow_once", &[]),
            allow_always: catalog.t("connectors.allow_always", &[]),
            deny_once: catalog.t("connectors.deny_once", &[]),
            deny_always: catalog.t("connectors.deny_always", &[]),
            remote_trust: catalog.t("connector.trust.title", &[]),
            trust_continue: catalog.t("connector.trust.continue", &[]),
            deny_trust: catalog.t("connector.trust.deny", &[]),
            trust_discover: catalog.t("connector.trust.purpose.discover", &[]),
            trust_invoke: catalog.t("connector.trust.purpose.invoke", &[]),
            trust_oauth: catalog.t("connector.trust.purpose.oauth", &[]),
            oauth_title: catalog.t("connectors.oauth_flow_title", &[]),
            oauth_discovering: catalog.t("connector.oauth.discovering", &[]),
            oauth_consent: catalog.t("connector.oauth.consent", &[]),
            oauth_continue: catalog.t("connector.oauth.continue", &[]),
            oauth_deny: catalog.t("connector.oauth.deny", &[]),
            oauth_preparing_callback: catalog.t("connector.oauth.preparing_callback", &[]),
            oauth_browser_ready: catalog.t("connector.oauth.browser_ready", &[]),
            oauth_browser_host_action: catalog.t("connector.oauth.browser_host_action", &[]),
            oauth_waiting_callback: catalog.t("connector.oauth.waiting_callback", &[]),
            oauth_client_unavailable: catalog.t("connector.oauth.client_unavailable", &[]),
            authority: catalog.t("connector.oauth.authority", &[]),
            resource: catalog.t("connector.oauth.resource", &[]),
            scopes: catalog.t("connector.oauth.scopes", &[]),
            retry_oauth: catalog.t("connector.oauth.retry", &[]),
            operations: catalog.t("connector.operations.title", &[]),
            cancel_operation: catalog.t("connector.action.cancel_operation", &[]),
            operation_trust: catalog.t("connector.operation.kind.trust", &[]),
            operation_discover: catalog.t("connector.operation.kind.discover", &[]),
            operation_invoke: catalog.t("connector.operation.kind.invoke", &[]),
            operation_oauth: catalog.t("connector.operation.kind.oauth", &[]),
            operation_import: catalog.t("connector.operation.kind.import", &[]),
            operation_save_server: catalog.t("connector.operation.kind.save_server", &[]),
            operation_delete_server: catalog.t("connector.operation.kind.delete_server", &[]),
            operation_update_permission: catalog
                .t("connector.operation.kind.update_permission", &[]),
            phase_queued: catalog.t("connector.operation.phase.queued", &[]),
            phase_validating: catalog.t("connector.operation.phase.validating", &[]),
            phase_awaiting_trust: catalog.t("connector.operation.phase.awaiting_trust", &[]),
            phase_discovering_schema: catalog
                .t("connector.operation.phase.discovering_schema", &[]),
            phase_discovering_auth: catalog.t("connector.operation.phase.discovering_auth", &[]),
            phase_awaiting_consent: catalog.t("connector.operation.phase.awaiting_consent", &[]),
            phase_awaiting_client: catalog.t("connector.operation.phase.awaiting_client", &[]),
            phase_preparing_callback: catalog
                .t("connector.operation.phase.preparing_callback", &[]),
            phase_browser_ready: catalog.t("connector.operation.phase.browser_ready", &[]),
            phase_awaiting_callback: catalog.t("connector.operation.phase.awaiting_callback", &[]),
            phase_authorizing: catalog.t("connector.operation.phase.authorizing", &[]),
            phase_audit_preflight: catalog.t("connector.operation.phase.audit_preflight", &[]),
            phase_calling: catalog.t("connector.operation.phase.calling", &[]),
            phase_persisting: catalog.t("connector.operation.phase.persisting", &[]),
            phase_succeeded: catalog.t("connector.operation.phase.succeeded", &[]),
            phase_failed: catalog.t("connector.operation.phase.failed", &[]),
            phase_unknown: catalog.t("connector.operation.phase.unknown", &[]),
            phase_denied: catalog.t("connector.operation.phase.denied", &[]),
            phase_cancelled: catalog.t("connector.operation.phase.cancelled", &[]),
            error_invalid_input: catalog.t("connector.error.invalid_input", &[]),
            error_invalid_url: catalog.t("connector.error.invalid_url", &[]),
            error_limit_exceeded: catalog.t("connector.error.limit_exceeded", &[]),
            error_backpressure: catalog.t("connector.error.backpressure", &[]),
            error_storage_unavailable: catalog.t("connector.error.storage_unavailable", &[]),
            error_secret_unavailable: catalog.t("connector.error.secret_unavailable", &[]),
            error_trust_denied: catalog.t("connector.error.trust_denied", &[]),
            error_host_unavailable: catalog.t("connector.error.host_unavailable", &[]),
            error_permission_denied: catalog.t("connector.error.permission_denied", &[]),
            error_audit_unavailable: catalog.t("connector.error.audit_unavailable", &[]),
            error_authentication_required: catalog
                .t("connector.error.authentication_required", &[]),
            error_authentication_failed: catalog.t("connector.error.authentication_failed", &[]),
            error_oauth_callback_failed: catalog.t("connector.error.oauth_callback_failed", &[]),
            error_network_timeout: catalog.t("connector.error.network_timeout", &[]),
            error_transport_failed: catalog.t("connector.error.transport_failed", &[]),
            error_protocol_violation: catalog.t("connector.error.protocol_violation", &[]),
            error_stale_result: catalog.t("connector.error.stale_result", &[]),
            error_cancelled: catalog.t("connector.error.cancelled", &[]),
            error_unknown_delivery: catalog.t("connector.error.unknown_delivery", &[]),
            error_internal: catalog.t("connector.error.internal", &[]),
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

    #[test]
    fn popup_audit_service_prompt_pauses_local_form_and_preserves_draft() {
        let ctx = egui::Context::default();
        let catalog = Catalog::load("en-US").unwrap();
        let mut connector = ConnectorUi::new(&catalog);
        connector.add_server = Some(ServerFormDraft {
            name: "Keep my server draft".into(),
            ..Default::default()
        });
        let snapshot = ConnectorSnapshot {
            oauth: Some(oauth_state(OAuthUiPhase::DiscoveringAuth)),
            ..Default::default()
        };
        ctx.run_ui(raw_input(), |ui| {
            assert!(connector.render(ui, &snapshot).is_none());
        })
        .drop_without_applying_deltas();
        let local_layer =
            egui::LayerId::new(egui::Order::Middle, egui::Id::new("connector_add_server"));
        assert!(
            !ctx.memory(|memory| memory.areas().is_visible(&local_layer)),
            "local form must pause while OAuth owns interaction"
        );
        assert_eq!(
            connector.add_server.as_ref().unwrap().name,
            "Keep my server draft"
        );
        ctx.run_ui(raw_input(), |ui| {
            assert!(
                connector
                    .render(ui, &ConnectorSnapshot::default())
                    .is_none()
            );
        })
        .drop_without_applying_deltas();
        assert!(ctx.memory(|memory| memory.areas().is_visible(&local_layer)));
        assert_eq!(
            connector.add_server.as_ref().unwrap().name,
            "Keep my server draft"
        );
    }

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
        let mut first = context.run_ui(raw_input(), |ui| {
            assert!(connector_ui.render(ui, snapshot).is_none());
        });
        first.textures_delta.clear();
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
        context
            .run_ui(input, |ui| {
                emitted = connector_ui.render(ui, snapshot);
            })
            .drop_without_applying_deltas();
        emitted
    }

    fn accessible_labels(snapshot: &ConnectorSnapshot) -> Vec<String> {
        let catalog = Catalog::load("en-US").unwrap();
        accessible_labels_with_catalog(snapshot, &catalog)
    }

    fn accessible_labels_with_catalog(
        snapshot: &ConnectorSnapshot,
        catalog: &Catalog,
    ) -> Vec<String> {
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut connector_ui = ConnectorUi::new(catalog);
        let mut output = context.run_ui(raw_input(), |ui| {
            assert!(connector_ui.render(ui, snapshot).is_none());
        });
        output.textures_delta.clear();
        output
            .platform_output
            .accesskit_update
            .expect("AccessKit output")
            .nodes
            .iter()
            .filter_map(|(_, node)| node.label().or_else(|| node.value()).map(str::to_owned))
            .collect()
    }

    const CONNECTOR_CATALOG_KEYS: &[&str] = &[
        "connector.import.paste",
        "connector.import.report_title",
        "connector.import.close_report",
        "connector.import.outcome.added",
        "connector.import.outcome.skipped_duplicate",
        "connector.import.outcome.skipped_unsupported",
        "connector.import.outcome.failed",
        "connector.import.secret_env_omitted",
        "connector.import.summary",
        "connector.slack.name",
        "connector.slack.connect",
        "connector.slack.choose_workspace",
        "connector.slack.configure_app",
        "connector.slack.enable_mcp",
        "connector.slack.retry_authorization",
        "connector.slack.workspace_label",
        "connector.action.refresh",
        "connector.action.edit",
        "connector.action.load_tools",
        "connector.action.save_client_continue",
        "connector.action.cancel_operation",
        "connectors.presets",
        "connectors.preset_hint",
        "connector.status.failed",
        "connector.status.disabled",
        "connector.status.truncated",
        "connector.status.enabled",
        "connector.tools.label",
        "connector.trust.title",
        "connector.trust.continue",
        "connector.trust.deny",
        "connector.trust.purpose.discover",
        "connector.trust.purpose.invoke",
        "connector.trust.purpose.oauth",
        "connector.oauth.discovering",
        "connector.oauth.consent",
        "connector.oauth.continue",
        "connector.oauth.deny",
        "connector.oauth.preparing_callback",
        "connector.oauth.browser_ready",
        "connector.oauth.browser_host_action",
        "connector.oauth.waiting_callback",
        "connector.oauth.client_unavailable",
        "connector.oauth.authority",
        "connector.oauth.resource",
        "connector.oauth.scopes",
        "connector.oauth.retry",
        "connector.operations.title",
        "connector.operation.kind.trust",
        "connector.operation.kind.discover",
        "connector.operation.kind.invoke",
        "connector.operation.kind.oauth",
        "connector.operation.kind.import",
        "connector.operation.kind.save_server",
        "connector.operation.kind.delete_server",
        "connector.operation.kind.update_permission",
        "connector.operation.phase.queued",
        "connector.operation.phase.validating",
        "connector.operation.phase.awaiting_trust",
        "connector.operation.phase.discovering_schema",
        "connector.operation.phase.discovering_auth",
        "connector.operation.phase.awaiting_consent",
        "connector.operation.phase.awaiting_client",
        "connector.operation.phase.preparing_callback",
        "connector.operation.phase.browser_ready",
        "connector.operation.phase.awaiting_callback",
        "connector.operation.phase.authorizing",
        "connector.operation.phase.audit_preflight",
        "connector.operation.phase.calling",
        "connector.operation.phase.persisting",
        "connector.operation.phase.succeeded",
        "connector.operation.phase.failed",
        "connector.operation.phase.unknown",
        "connector.operation.phase.denied",
        "connector.operation.phase.cancelled",
        "connector.error.invalid_input",
        "connector.error.invalid_url",
        "connector.error.limit_exceeded",
        "connector.error.backpressure",
        "connector.error.storage_unavailable",
        "connector.error.secret_unavailable",
        "connector.error.trust_denied",
        "connector.error.host_unavailable",
        "connector.error.permission_denied",
        "connector.error.audit_unavailable",
        "connector.error.authentication_required",
        "connector.error.authentication_failed",
        "connector.error.oauth_callback_failed",
        "connector.error.network_timeout",
        "connector.error.transport_failed",
        "connector.error.protocol_violation",
        "connector.error.stale_result",
        "connector.error.cancelled",
        "connector.error.unknown_delivery",
        "connector.error.internal",
    ];

    #[test]
    fn connector_catalog_keys_resolve_in_every_bundled_locale() {
        for locale in ["en-US", "ja-JP", "zh-Hans", "zh-Hant", "ko-KR"] {
            let catalog = Catalog::load(locale).unwrap();
            for key in CONNECTOR_CATALOG_KEYS {
                let rendered = catalog.t(key, &[]);
                assert_ne!(rendered, *key, "{locale} is missing {key}");
                assert!(
                    !rendered.is_empty(),
                    "{locale} has an empty value for {key}"
                );
            }
        }
    }

    #[test]
    fn non_english_catalog_values_render_without_legacy_english_prose() {
        let catalog = Catalog::load("ko-KR").unwrap();
        let fingerprint = EndpointFingerprint::parse("b".repeat(64)).unwrap();
        let trust_snapshot = ConnectorSnapshot {
            revision: Revision(20),
            remote_trust: Some(RemoteTrustPrompt {
                operation_id: OperationId::new("localized-trust"),
                server_id: ServerId::new("localized-server"),
                server_name: "Localized server".to_owned(),
                purpose: RemoteTrustPurpose::Invoke,
                display_endpoint: EndpointDisplay::new("https://example.test/mcp"),
                endpoint_fingerprint: fingerprint,
                config_revision: Revision(4),
            }),
            ..ConnectorSnapshot::default()
        };
        let trust_labels = accessible_labels_with_catalog(&trust_snapshot, &catalog);
        let expected_trust = catalog.t("connector.trust.continue", &[]);
        assert!(trust_labels.iter().any(|label| label == &expected_trust));
        assert!(
            !trust_labels
                .iter()
                .any(|label| label == "Trust and continue")
        );

        let report_snapshot = ConnectorSnapshot {
            revision: Revision(21),
            import_report: Some(ImportReport {
                operation_id: OperationId::new("localized-import"),
                source: ImportSource::Paste,
                added: 2,
                skipped: 3,
                failed: 1,
                items: Arc::from([]),
                truncated: false,
            }),
            operations: Arc::from([OperationSummary {
                id: OperationId::new("localized-operation"),
                server_id: ServerId::new("localized-server"),
                kind: OperationKind::Invoke,
                phase: OperationPhase::Unknown,
                error_code: Some(ErrorCode::UnknownDelivery),
            }]),
            ..ConnectorSnapshot::default()
        };
        let labels = accessible_labels_with_catalog(&report_snapshot, &catalog);
        let expected_summary = catalog.t(
            "connector.import.summary",
            &[("added", "2"), ("skipped", "3"), ("failed", "1")],
        );
        let expected_operation = format!(
            "{} · {} · {}",
            catalog.t("connector.operation.kind.invoke", &[]),
            catalog.t("connector.operation.phase.unknown", &[]),
            catalog.t("connector.error.unknown_delivery", &[]),
        );
        assert!(labels.iter().any(|label| label == &expected_summary));
        assert!(labels.iter().any(|label| label == &expected_operation));
        for legacy in [
            "2 added · 3 skipped · 1 failed",
            "Tool call · Delivery unknown",
            "Delivery status unknown; not retried",
        ] {
            assert!(!labels.iter().any(|label| label.contains(legacy)));
        }
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
        let trust_continue = catalog.t("connector.trust.continue", &[]);
        let accepted = activate_accessible_label(&mut ui, &snapshot, &trust_continue);
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
        let trust_deny = catalog.t("connector.trust.deny", &[]);
        assert!(matches!(
            activate_accessible_label(&mut ui, &snapshot, &trust_deny),
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
        let oauth_continue = catalog.t("connector.oauth.continue", &[]);
        assert!(matches!(
            activate_accessible_label(&mut ui, &consent, &oauth_continue),
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
        egui::Context::default()
            .run_ui(raw_input(), |egui_ui| {
                assert!(ui.render(egui_ui, &awaiting_client).is_none());
            })
            .drop_without_applying_deltas();
        let draft = ui.oauth_client.as_mut().expect("client draft");
        draft.client_id = "client-id".to_owned();
        draft.client_secret = "client-secret".to_owned();
        draft.workspace_hint = "example.slack.com".to_owned();
        let save_client = catalog.t("connector.action.save_client_continue", &[]);
        let submitted = activate_accessible_label(&mut ui, &awaiting_client, &save_client);
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

        let mut link_ui = ConnectorUi::new(&catalog);
        egui::Context::default()
            .run_ui(raw_input(), |egui_ui| {
                assert!(link_ui.render(egui_ui, &awaiting_client).is_none());
            })
            .drop_without_applying_deltas();
        let link_draft = link_ui.oauth_client.as_mut().expect("client draft");
        link_draft.client_id = "kept-client-id".to_owned();
        link_draft.client_secret = "kept-client-secret".to_owned();
        let open_slack_settings = catalog.t("connectors.slack.open_app_settings", &[]);
        assert!(matches!(
            activate_accessible_label(&mut link_ui, &awaiting_client, &open_slack_settings),
            Some(ConnectorIntent::OpenExternalLink(
                ExternalLinkKind::SlackAppSettings
            ))
        ));
        let link_draft = link_ui
            .oauth_client
            .as_ref()
            .expect("preserved client draft");
        assert_eq!(link_draft.client_id, "kept-client-id");
        assert_eq!(link_draft.client_secret, "kept-client-secret");
    }

    #[test]
    fn oauth_browser_callback_failure_and_recovery_are_rendered_without_raw_url_intents() {
        let catalog = Catalog::load("en-US").unwrap();
        for (phase, key) in [
            (OAuthUiPhase::DiscoveringAuth, "connector.oauth.discovering"),
            (
                OAuthUiPhase::PreparingCallback,
                "connector.oauth.preparing_callback",
            ),
            (OAuthUiPhase::BrowserReady, "connector.oauth.browser_ready"),
            (
                OAuthUiPhase::AwaitingCallback,
                "connector.oauth.waiting_callback",
            ),
        ] {
            let snapshot = ConnectorSnapshot {
                revision: Revision(5),
                oauth: Some(oauth_state(phase)),
                ..ConnectorSnapshot::default()
            };
            let expected = catalog.t(key, &[]);
            assert!(
                accessible_labels(&snapshot)
                    .iter()
                    .any(|label| label == &expected)
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
        let mut ui = ConnectorUi::new(&catalog);
        let retry_oauth = catalog.t("connector.oauth.retry", &[]);
        assert!(matches!(
            activate_accessible_label(&mut ui, &failed, &retry_oauth),
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
        let connect_slack = catalog.t("connector.slack.connect", &[]);
        assert!(matches!(
            activate_accessible_label(&mut ui, &ConnectorSnapshot::default(), &connect_slack),
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
        let choose_workspace = catalog.t("connector.slack.choose_workspace", &[]);
        assert!(matches!(
            activate_accessible_label(&mut ui, &connected, &choose_workspace),
            Some(ConnectorIntent::ChooseSlackWorkspace(server_id))
                if server_id.as_str() == "slack"
        ));
        let mut ui = ConnectorUi::new(&catalog);
        let enable_mcp = catalog.t("connector.slack.enable_mcp", &[]);
        assert!(matches!(
            activate_accessible_label(&mut ui, &connected, &enable_mcp),
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
        let import_paste = catalog.t("connector.import.paste", &[]);
        match activate_accessible_label(&mut ui, &snapshot, &import_paste) {
            Some(ConnectorIntent::ImportConfiguration {
                source: ImportSource::Paste,
                display_name: None,
                contents,
            }) => assert_eq!(contents.expose_bytes(), b"{\"mcpServers\":{}}"),
            other => panic!("unexpected intent: {other:?}"),
        }
        assert!(ui.paste_input.is_empty());

        let mut ui = ConnectorUi::new(&catalog);
        let import_file = catalog.t("connectors.import_file", &[]);
        assert!(matches!(
            activate_accessible_label(&mut ui, &snapshot, &import_file),
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
        let open_slack_settings = catalog.t("connectors.slack.open_app_settings", &[]);
        assert!(matches!(
            activate_accessible_label(&mut ui, &snapshot, &open_slack_settings),
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

        context
            .run_ui(raw_input(), |egui_ui| {
                assert!(ui.render(egui_ui, &snapshot).is_none());
            })
            .drop_without_applying_deltas();

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
        context
            .run_ui(raw_input(), |egui_ui| {
                assert!(ui.render(egui_ui, &snapshot).is_none());
            })
            .drop_without_applying_deltas();
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
        let catalog = Catalog::load("en-US").unwrap();
        let unknown = catalog.t("connector.operation.phase.unknown", &[]);
        assert!(labels.iter().any(|label| label.contains(&unknown)));
        assert!(!labels.iter().any(|label| label.contains("Retry")));

        let mut ui = ConnectorUi::new(&catalog);
        let cancel_operation = catalog.t("connector.action.cancel_operation", &[]);
        assert!(matches!(
            activate_accessible_label(&mut ui, &snapshot, &cancel_operation),
            Some(ConnectorIntent::Cancel(operation_id)) if operation_id.as_str() == "running"
        ));
    }

    #[test]
    fn identical_revision_three_hundred_frames_do_not_rebuild_display_cache() {
        let snapshot = ConnectorSnapshot::default();
        let context = egui::Context::default();
        let catalog = Catalog::load("en-US").unwrap();
        let mut ui = ConnectorUi::new(&catalog);
        context
            .run_ui(raw_input(), |egui_ui| {
                assert!(ui.render(egui_ui, &snapshot).is_none());
            })
            .drop_without_applying_deltas();
        assert_eq!(ui.prepared.rebuild_count, 1);

        for _ in 0..300 {
            context
                .run_ui(raw_input(), |egui_ui| {
                    assert!(ui.render(egui_ui, &snapshot).is_none());
                })
                .drop_without_applying_deltas();
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
    fn clear_sensitive_drafts_preserves_non_sensitive_ui_state() {
        let catalog = Catalog::load("en-US").unwrap();
        let mut ui = ConnectorUi::new(&catalog);
        ui.prepared.revision = Some(Revision(31));
        ui.add_server = Some(ServerFormDraft {
            name: "preserved-add-draft".to_owned(),
            ..ServerFormDraft::default()
        });
        ui.delete_server = Some(DeleteServerDraft {
            server_id: ServerId::new("preserved-server"),
            server_name: "Preserved server".to_owned(),
        });
        ui.invoke = Some(InvokeDraft {
            server_id: ServerId::new("server"),
            tool_id: ToolId::new("tool"),
            tool_name: "tool".to_owned(),
            arguments_json: "{\"token\":\"invoke-secret\"}".to_owned(),
        });
        let oauth = oauth_state(OAuthUiPhase::AwaitingClient {
            reason: ErrorCode::AuthenticationRequired,
            workspace_hint: None,
        });
        let mut oauth_client = OAuthClientDraft::from_state(&oauth);
        oauth_client.client_secret = "oauth-secret".to_owned();
        ui.oauth_client = Some(oauth_client);
        ui.paste_input = "client_secret=paste-secret".to_owned();

        ui.clear_sensitive_drafts();

        assert!(ui.invoke.is_none());
        assert!(ui.oauth_client.is_none());
        assert!(ui.paste_input.is_empty());
        assert_eq!(ui.paste_input.capacity(), 0);
        assert_eq!(ui.prepared.revision, Some(Revision(31)));
        assert_eq!(
            ui.add_server.as_ref().map(|draft| draft.name.as_str()),
            Some("preserved-add-draft")
        );
        assert_eq!(
            ui.delete_server
                .as_ref()
                .map(|draft| draft.server_id.as_str()),
            Some("preserved-server")
        );
    }

    #[test]
    fn stdio_presets_fill_only_the_local_add_draft() {
        let catalog = Catalog::load("en-US").unwrap();
        let mut ui = ConnectorUi::new(&catalog);
        ui.add_server = Some(ServerFormDraft {
            transport: FormTransport::Stdio,
            ..ServerFormDraft::default()
        });

        assert!(
            activate_accessible_label(&mut ui, &ConnectorSnapshot::default(), "filesystem")
                .is_none()
        );

        let draft = ui.add_server.as_ref().expect("preserved add draft");
        assert_eq!(draft.name, "filesystem");
        assert_eq!(draft.command, "npx");
        assert_eq!(
            draft.arguments,
            "-y\n@modelcontextprotocol/server-filesystem\n/absolute/path/to/allowed/directory"
        );
        assert_eq!(draft.transport, FormTransport::Stdio);
        assert!(draft.id.is_none());
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
        egui::Context::default()
            .run_ui(raw_input(), |egui_ui| {
                assert!(ui.render(egui_ui, &awaiting_client).is_none());
            })
            .drop_without_applying_deltas();
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
