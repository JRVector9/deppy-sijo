//! Pure Connector snapshot renderer.
//!
//! The renderer consumes immutable [`contract::ConnectorSnapshot`] values and returns at most one
//! [`contract::ConnectorIntent`] per frame. It never opens files, talks to a service, starts work,
//! or retains snapshot history. Only unfinished form/modal drafts live in this crate.

use std::mem;

use connector_contract::{
    ApprovalDecision, ApprovalPrompt, ConnectionState, ConnectorIntent, ConnectorSnapshot,
    PermissionRule, SensitiveInput, ServerDraft, ServerId, ServerSummary, SlackStatus,
    ToolListItem, ToolPage, TransportDraft, TransportKind,
};
use egui::{Button, ComboBox, Label, ScrollArea, TextEdit, Ui};
use i18n::Catalog;

pub use connector_contract as contract;

const SERVER_ROW_HEIGHT: f32 = 30.0;
const SERVER_LIST_HEIGHT: f32 = 180.0;
const TOOL_ROW_HEIGHT: f32 = 58.0;
const TOOL_LIST_HEIGHT: f32 = TOOL_ROW_HEIGHT * 6.0;
const SLACK_APP_SETTINGS_URL: &str = "https://api.slack.com/apps";

/// Stateful UI shell. State is limited to user-authored drafts and confirmation modals.
pub struct ConnectorUi {
    labels: Labels,
    add_server: Option<ServerFormDraft>,
    invoke: Option<InvokeDraft>,
    oauth_client: Option<OAuthClientDraft>,
    delete_server: Option<DeleteServerDraft>,
}

impl ConnectorUi {
    pub fn new(catalog: &Catalog) -> Self {
        Self {
            labels: Labels::new(catalog),
            add_server: None,
            invoke: None,
            oauth_client: None,
            delete_server: None,
        }
    }

    /// Refreshes cached translations after an application locale change.
    ///
    /// Translations are intentionally cached outside the render path. Existing user drafts stay
    /// intact when the locale changes.
    pub fn set_catalog(&mut self, catalog: &Catalog) {
        self.labels = Labels::new(catalog);
    }

    /// Renders one frame from an immutable snapshot and returns at most one intent.
    #[must_use]
    pub fn render(&mut self, ui: &mut Ui, snapshot: &ConnectorSnapshot) -> Option<ConnectorIntent> {
        let mut intent = None;
        let has_modal = self.add_server.is_some()
            || self.invoke.is_some()
            || self.oauth_client.is_some()
            || self.delete_server.is_some()
            || snapshot.approval.is_some();

        ui.add_enabled_ui(!has_modal, |ui| {
            render_main(
                ui,
                snapshot,
                &self.labels,
                &mut self.add_server,
                &mut self.invoke,
                &mut self.oauth_client,
                &mut self.delete_server,
                &mut intent,
            );
        });

        render_add_server_modal(ui, &self.labels, &mut self.add_server, &mut intent);
        render_invoke_modal(ui, &self.labels, &mut self.invoke, &mut intent);
        render_oauth_modal(ui, &self.labels, &mut self.oauth_client, &mut intent);
        render_delete_modal(ui, &self.labels, &mut self.delete_server, &mut intent);
        if let Some(approval) = snapshot.approval.as_ref() {
            render_approval_modal(ui, &self.labels, approval, &mut intent);
        }

        intent
    }
}

#[allow(clippy::too_many_arguments)]
fn render_main(
    ui: &mut Ui,
    snapshot: &ConnectorSnapshot,
    labels: &Labels,
    add_server: &mut Option<ServerFormDraft>,
    invoke: &mut Option<InvokeDraft>,
    oauth_client: &mut Option<OAuthClientDraft>,
    delete_server: &mut Option<DeleteServerDraft>,
    intent: &mut Option<ConnectorIntent>,
) {
    ui.heading(&labels.title);
    ui.horizontal(|ui| {
        if ui.button(&labels.add_server).clicked() {
            *add_server = Some(ServerFormDraft::default());
        }
        if ui.button(&labels.import_file).clicked() {
            offer_intent(intent, ConnectorIntent::RequestImportPicker);
        }
        if ui.button(&labels.open_slack_settings).clicked() {
            offer_intent(
                intent,
                ConnectorIntent::OpenExternalUrl {
                    url: SLACK_APP_SETTINGS_URL.to_owned(),
                },
            );
        }
        if ui.button(&labels.refresh).clicked() {
            offer_intent(intent, ConnectorIntent::Activate);
        }
    });

    render_slack_summary(ui, snapshot, labels);
    ui.separator();
    render_server_overview(ui, snapshot, labels, intent);

    let selected = snapshot
        .selected_server
        .as_ref()
        .and_then(|id| snapshot.servers.iter().find(|server| &server.id == id));
    if let Some(server) = selected {
        ui.separator();
        render_selected_server(
            ui,
            snapshot,
            server,
            labels,
            invoke,
            oauth_client,
            delete_server,
            intent,
        );
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

fn render_slack_summary(ui: &mut Ui, snapshot: &ConnectorSnapshot, labels: &Labels) {
    let status = match snapshot.slack_status {
        SlackStatus::NotConfigured => &labels.not_configured,
        SlackStatus::Ready => &labels.ready,
        SlackStatus::Checking => &labels.checking,
        SlackStatus::NeedsAuthorization => &labels.needs_authorization,
        SlackStatus::Connected => &labels.connected,
        SlackStatus::Failed => &labels.failed,
    };
    ui.horizontal(|ui| {
        ui.strong("Slack");
        ui.label(status);
        if snapshot.slack_tool_count > 0 {
            ui.label(snapshot.slack_tool_count.to_string());
            ui.label(&labels.tools);
        }
    });
}

fn render_server_overview(
    ui: &mut Ui,
    snapshot: &ConnectorSnapshot,
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
            for server in &snapshot.servers[rows] {
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
                    ui.label(server.tool_count.to_string());
                    ui.label(&labels.tools);
                });
            }
        });
}

#[allow(clippy::too_many_arguments)]
fn render_selected_server(
    ui: &mut Ui,
    snapshot: &ConnectorSnapshot,
    server: &ServerSummary,
    labels: &Labels,
    invoke: &mut Option<InvokeDraft>,
    oauth_client: &mut Option<OAuthClientDraft>,
    delete_server: &mut Option<DeleteServerDraft>,
    intent: &mut Option<ConnectorIntent>,
) {
    ui.horizontal(|ui| {
        ui.heading(&server.name);
        ui.label(transport_label(server.transport, labels));
        if ui.button(&labels.discover).clicked() {
            offer_intent(intent, ConnectorIntent::Discover(server.id.clone()));
        }
        if ui.button(&labels.authorize).clicked() {
            offer_intent(intent, ConnectorIntent::BeginOAuth(server.id.clone()));
        }
        if ui.button(&labels.oauth_client).clicked() {
            *oauth_client = Some(OAuthClientDraft::new(server));
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
        render_tool_page(ui, page, labels, invoke, intent);
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
    labels: &Labels,
    invoke: &mut Option<InvokeDraft>,
    intent: &mut Option<ConnectorIntent>,
) {
    ui.horizontal(|ui| {
        ui.strong(&labels.tools);
        ui.label(page.total.to_string());
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
                    .desired_rows(8),
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

fn render_oauth_modal(
    ui: &mut Ui,
    labels: &Labels,
    draft: &mut Option<OAuthClientDraft>,
    intent: &mut Option<ConnectorIntent>,
) {
    let Some(current) = draft.as_mut() else {
        return;
    };
    let mut close = false;
    let mut submit = false;
    egui::Window::new(&labels.oauth_client)
        .id(egui::Id::new("connector_oauth_client"))
        .collapsible(false)
        .show(ui.ctx(), |ui| {
            ui.strong(&current.server_name);
            ui.label(&labels.client_id);
            ui.add(TextEdit::singleline(&mut current.client_id));
            ui.label(&labels.client_secret);
            ui.add(TextEdit::singleline(&mut current.client_secret).password(true));
            ui.label(&labels.workspace_hint);
            ui.add(TextEdit::singleline(&mut current.workspace_hint));
            ui.horizontal(|ui| {
                close = ui.button(&labels.cancel).clicked();
                submit = ui
                    .add_enabled(
                        !current.client_id.trim().is_empty(),
                        Button::new(&labels.save),
                    )
                    .clicked();
            });
        });
    if close {
        *draft = None;
    } else if submit {
        let mut current = draft.take().expect("draft exists while submitting OAuth");
        let workspace_hint = trimmed_owned(mem::take(&mut current.workspace_hint));
        offer_intent(
            intent,
            ConnectorIntent::SubmitOAuthClient {
                server_id: current.server_id.clone(),
                client_id: mem::take(&mut current.client_id),
                client_secret: SensitiveInput::from(mem::take(&mut current.client_secret)),
                workspace_hint,
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
            ui.horizontal(|ui| {
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

fn offer_intent(slot: &mut Option<ConnectorIntent>, intent: ConnectorIntent) {
    if slot.is_none() {
        *slot = Some(intent);
    }
}

fn trimmed_owned(value: String) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
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
    name: String,
    transport: FormTransport,
    url: String,
    command: String,
    arguments: String,
    enabled: bool,
}

impl Default for ServerFormDraft {
    fn default() -> Self {
        Self {
            name: String::new(),
            transport: FormTransport::Http,
            url: String::new(),
            command: String::new(),
            arguments: String::new(),
            enabled: true,
        }
    }
}

impl ServerFormDraft {
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
                plain_env: Vec::new(),
                secret_env: Vec::new(),
                inherit_env: true,
            },
        };
        ServerDraft {
            id: None,
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

impl Drop for InvokeDraft {
    fn drop(&mut self) {
        zero_string(&mut self.arguments_json);
    }
}

struct OAuthClientDraft {
    server_id: ServerId,
    server_name: String,
    client_id: String,
    client_secret: String,
    workspace_hint: String,
}

impl OAuthClientDraft {
    fn new(server: &ServerSummary) -> Self {
        Self {
            server_id: server.id.clone(),
            server_name: server.name.clone(),
            client_id: String::new(),
            client_secret: String::new(),
            workspace_hint: String::new(),
        }
    }
}

impl Drop for OAuthClientDraft {
    fn drop(&mut self) {
        zero_string(&mut self.client_secret);
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
    import_file: String,
    open_slack_settings: String,
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
    authorize: String,
    oauth_client: String,
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
    approval_needed: String,
    approval_ask: String,
    approval_first_use: String,
    approval_schema_changed: String,
    allow_once: String,
    allow_always: String,
    deny_once: String,
    deny_always: String,
}

impl Labels {
    fn new(catalog: &Catalog) -> Self {
        Self {
            title: catalog.t("connectors.local_mcp", &[]),
            add_server: catalog.t("connectors.add_mcp", &[]),
            import_file: catalog.t("connectors.import_file", &[]),
            open_slack_settings: catalog.t("connectors.slack.open_app_settings", &[]),
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
            authorize: catalog.t("connectors.approve_browser", &[]),
            oauth_client: catalog.t("connectors.oauth_flow_title", &[]),
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
            approval_needed: catalog.t("connectors.approval_needed", &[("reason", "")]),
            approval_ask: catalog.t("connectors.approval.ask_rule", &[]),
            approval_first_use: catalog.t("connectors.approval.first_use", &[]),
            approval_schema_changed: catalog.t("connectors.approval.schema_changed", &[]),
            allow_once: catalog.t("connectors.allow_once", &[]),
            allow_always: catalog.t("connectors.allow_always", &[]),
            deny_once: catalog.t("connectors.deny_once", &[]),
            deny_always: catalog.t("connectors.deny_always", &[]),
        }
    }
}

#[cfg(test)]
thread_local! {
    static RENDERED_TOOL_ROWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[inline]
fn record_rendered_tool_row() {
    #[cfg(test)]
    RENDERED_TOOL_ROWS.with(|count| count.set(count.get().saturating_add(1)));
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use connector_contract::{
        ConnectorSnapshot, PermissionRule, Revision, ServerId, ServerSummary, SlackStatus, ToolId,
        ToolListItem, ToolPage, TransportKind,
    };

    use super::*;

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
            slack_status: SlackStatus::Connected,
            slack_tool_count: 4,
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

    fn raw_input() -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 720.0),
            )),
            ..Default::default()
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

    #[test]
    fn headless_snapshot_exposes_accessible_server_and_tool_labels() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let catalog = Catalog::load("en-US").unwrap();
        let mut connector_ui = ConnectorUi::new(&catalog);
        let snapshot = snapshot_with_tools(16);

        let output = context.run_ui(raw_input(), |ui| {
            assert!(connector_ui.render(ui, &snapshot).is_none());
        });
        let update = output
            .platform_output
            .accesskit_update
            .expect("AccessKit output");
        let accessible_text: Vec<&str> = update
            .nodes
            .iter()
            .filter_map(|(_, node)| node.label().or_else(|| node.value()))
            .collect();

        assert!(accessible_text.contains(&"Fake server"));
        assert!(accessible_text.contains(&"Accessible tool 0"));
        assert!(accessible_text.contains(&"Call"));
        assert!(accessible_text.contains(&"From JSON file..."));
        assert!(accessible_text.contains(&"Open Slack app settings"));
    }

    #[test]
    fn four_thousand_ninety_six_tools_render_only_viewport_rows() {
        RENDERED_TOOL_ROWS.with(|count| count.set(0));
        let context = egui::Context::default();
        let catalog = Catalog::load("en-US").unwrap();
        let mut connector_ui = ConnectorUi::new(&catalog);
        let snapshot = snapshot_with_tools(4_096);
        let page_items = snapshot.tool_page.as_ref().unwrap().items.clone();

        let _ = context.run_ui(raw_input(), |ui| {
            assert!(connector_ui.render(ui, &snapshot).is_none());
        });

        let rendered = RENDERED_TOOL_ROWS.with(std::cell::Cell::get);
        let maximum_visible_with_overscan = (TOOL_LIST_HEIGHT / TOOL_ROW_HEIGHT) as usize + 2;
        assert!(rendered > 0);
        assert!(
            rendered <= maximum_visible_with_overscan,
            "rendered {rendered} rows from a 4096-row fixture"
        );
        assert!(Arc::ptr_eq(
            &page_items,
            &snapshot.tool_page.as_ref().unwrap().items
        ));
    }

    #[test]
    fn unmatched_tool_page_is_not_rendered_for_selected_server() {
        RENDERED_TOOL_ROWS.with(|count| count.set(0));
        let context = egui::Context::default();
        let catalog = Catalog::load("en-US").unwrap();
        let mut connector_ui = ConnectorUi::new(&catalog);
        let mut snapshot = snapshot_with_tools(32);
        snapshot.tool_page.as_mut().unwrap().server_id = ServerId::new("different-server");

        let _ = context.run_ui(raw_input(), |ui| {
            assert!(connector_ui.render(ui, &snapshot).is_none());
        });
        assert_eq!(RENDERED_TOOL_ROWS.with(std::cell::Cell::get), 0);
    }

    #[test]
    fn file_picker_and_external_url_are_intents_not_platform_commands() {
        let catalog = Catalog::load("en-US").unwrap();
        let snapshot = ConnectorSnapshot::default();
        let mut connector_ui = ConnectorUi::new(&catalog);
        assert!(matches!(
            activate_accessible_label(&mut connector_ui, &snapshot, "From JSON file..."),
            Some(ConnectorIntent::RequestImportPicker)
        ));

        let mut connector_ui = ConnectorUi::new(&catalog);
        let intent =
            activate_accessible_label(&mut connector_ui, &snapshot, "Open Slack app settings");
        assert!(matches!(
            intent,
            Some(ConnectorIntent::OpenExternalUrl { url })
                if url == SLACK_APP_SETTINGS_URL
        ));
    }

    #[test]
    fn selected_server_requests_its_tool_page_only_after_explicit_action() {
        let catalog = Catalog::load("en-US").unwrap();
        let mut connector_ui = ConnectorUi::new(&catalog);
        let mut snapshot = snapshot_with_tools(16);
        snapshot.tool_page = None;

        let intent = activate_accessible_label(&mut connector_ui, &snapshot, "Load tools");
        assert!(matches!(
            intent,
            Some(ConnectorIntent::RequestToolPage { server_id, offset: 0 })
                if server_id.as_str() == "server-1"
        ));
    }

    #[test]
    fn renderer_returns_only_one_intent_slot() {
        let mut intent = None;
        offer_intent(&mut intent, ConnectorIntent::Activate);
        offer_intent(&mut intent, ConnectorIntent::RequestImportPicker);
        assert!(matches!(intent, Some(ConnectorIntent::Activate)));
    }

    #[test]
    fn stdio_form_allocates_arguments_only_when_saved() {
        let draft = ServerFormDraft {
            name: "local".to_owned(),
            transport: FormTransport::Stdio,
            url: String::new(),
            command: "example".to_owned(),
            arguments: "--one\n\n --two ".to_owned(),
            enabled: true,
        };
        let saved = draft.into_contract();
        let TransportDraft::Stdio { args, .. } = saved.transport else {
            panic!("expected stdio draft");
        };
        assert_eq!(args, ["--one", "--two"]);
    }
}
