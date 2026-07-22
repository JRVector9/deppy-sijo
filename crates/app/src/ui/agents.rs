use std::sync::Arc;

pub const AGENT_SNAPSHOT_MAX_ITEMS: usize = 1_024;
pub const AGENT_PROFILE_SNAPSHOT_MAX_ITEMS: usize = 256;
pub const AGENT_BACKEND_SNAPSHOT_MAX_ITEMS: usize = 256;
pub const AGENT_SNAPSHOT_MAX_BYTES: usize = 4 * 1024 * 1024;
pub const AGENT_ARGS_MAX_ITEMS: usize = 256;
pub const AGENT_ARGS_MAX_BYTES: usize = 64 * 1024;

const SHORT_INPUT_MAX_BYTES: usize = 4 * 1024;
const FLAG_INPUT_MAX_BYTES: usize = 256;
const AGENT_ROW_HEIGHT: f32 = 30.0;
const AGENT_LIST_MAX_HEIGHT: f32 = 360.0;
const REDACTED_ARGS: &str = "[REDACTED_ARGS]";

/// Adapter-produced argument summary. `Visible` must only be constructed after the composition
/// root applies the canonical persistence validator. Neither variant implements Clone or Debug.
pub enum AgentArgsSummary {
    Visible(Arc<str>),
    Redacted,
}

impl AgentArgsSummary {
    pub fn visible(value: impl Into<Arc<str>>) -> Self {
        Self::Visible(value.into())
    }

    pub const fn redacted() -> Self {
        Self::Redacted
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Visible(value) => value,
            Self::Redacted => REDACTED_ARGS,
        }
    }

    fn retained_bytes(&self) -> usize {
        self.as_str().len()
    }
}

/// Immutable UI-only agent row. It intentionally has no Clone/Debug/Serialize implementation.
pub struct AgentListItem {
    id: Arc<str>,
    name: Arc<str>,
    command: Arc<str>,
    args_summary: AgentArgsSummary,
}

impl AgentListItem {
    pub fn new(
        id: impl Into<Arc<str>>,
        name: impl Into<Arc<str>>,
        command: impl Into<Arc<str>>,
        args_summary: AgentArgsSummary,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            command: command.into(),
            args_summary,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    pub fn args_summary(&self) -> &str {
        self.args_summary.as_str()
    }

    fn retained_bytes(&self) -> usize {
        self.id.len() + self.name.len() + self.command.len() + self.args_summary.retained_bytes()
    }
}

pub struct AgentProfileItem {
    id: Arc<str>,
    name: Arc<str>,
    is_production: bool,
}

impl AgentProfileItem {
    pub fn new(id: impl Into<Arc<str>>, name: impl Into<Arc<str>>, is_production: bool) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            is_production,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn is_production(&self) -> bool {
        self.is_production
    }

    fn retained_bytes(&self) -> usize {
        self.id.len() + self.name.len()
    }
}

pub struct AgentBackendItem {
    id: Arc<str>,
    name: Arc<str>,
}

impl AgentBackendItem {
    pub fn new(id: impl Into<Arc<str>>, name: impl Into<Arc<str>>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn retained_bytes(&self) -> usize {
        self.id.len() + self.name.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentsSnapshotError {
    TooManyAgents,
    TooManyProfiles,
    TooManyBackends,
    ByteBudgetExceeded,
}

impl std::fmt::Display for AgentsSnapshotError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::TooManyAgents => "agents_snapshot_agent_limit",
            Self::TooManyProfiles => "agents_snapshot_profile_limit",
            Self::TooManyBackends => "agents_snapshot_backend_limit",
            Self::ByteBudgetExceeded => "agents_snapshot_byte_limit",
        })
    }
}

impl std::error::Error for AgentsSnapshotError {}

/// Bounded immutable render input. The adapter swaps this value only when its revision changes.
pub struct AgentsSnapshot {
    revision: u64,
    available: bool,
    agents: Arc<[AgentListItem]>,
    profiles: Arc<[AgentProfileItem]>,
    backends: Arc<[AgentBackendItem]>,
}

impl AgentsSnapshot {
    pub fn try_new(
        revision: u64,
        agents: Vec<AgentListItem>,
        profiles: Vec<AgentProfileItem>,
        backends: Vec<AgentBackendItem>,
    ) -> Result<Self, AgentsSnapshotError> {
        if agents.len() > AGENT_SNAPSHOT_MAX_ITEMS {
            return Err(AgentsSnapshotError::TooManyAgents);
        }
        if profiles.len() > AGENT_PROFILE_SNAPSHOT_MAX_ITEMS {
            return Err(AgentsSnapshotError::TooManyProfiles);
        }
        if backends.len() > AGENT_BACKEND_SNAPSHOT_MAX_ITEMS {
            return Err(AgentsSnapshotError::TooManyBackends);
        }
        let retained_bytes = agents
            .iter()
            .map(AgentListItem::retained_bytes)
            .chain(profiles.iter().map(AgentProfileItem::retained_bytes))
            .chain(backends.iter().map(AgentBackendItem::retained_bytes))
            .try_fold(0usize, usize::checked_add)
            .ok_or(AgentsSnapshotError::ByteBudgetExceeded)?;
        if retained_bytes > AGENT_SNAPSHOT_MAX_BYTES {
            return Err(AgentsSnapshotError::ByteBudgetExceeded);
        }
        Ok(Self {
            revision,
            available: true,
            agents: agents.into(),
            profiles: profiles.into(),
            backends: backends.into(),
        })
    }

    pub fn unavailable(revision: u64) -> Self {
        Self {
            revision,
            available: false,
            agents: Arc::from([]),
            profiles: Arc::from([]),
            backends: Arc::from([]),
        }
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub const fn is_available(&self) -> bool {
        self.available
    }

    pub fn agents(&self) -> &[AgentListItem] {
        &self.agents
    }

    pub fn profiles(&self) -> &[AgentProfileItem] {
        &self.profiles
    }

    pub fn backends(&self) -> &[AgentBackendItem] {
        &self.backends
    }
}

/// User-entered registration data. It may contain pasted sensitive text, so it deliberately has
/// no Clone, Debug, Display, or Serialize implementation. The adapter must run the canonical
/// persistence validator before writing it.
pub struct AgentRegistration {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub waiting_regex: Option<String>,
    pub approval_regex: Option<String>,
    pub error_regex: Option<String>,
    pub done_regex: Option<String>,
    pub mcp_proxy_enabled: bool,
    pub mcp_proxy_server_id: Option<String>,
    pub mcp_config_flag: Option<String>,
}

/// At most one intent is emitted by one render call. No variant implements Clone/Debug/Serialize.
pub enum AgentsIntent {
    Register {
        revision: u64,
        registration: AgentRegistration,
    },
    Delete {
        revision: u64,
        agent_id: String,
    },
    Run {
        revision: u64,
        agent_id: String,
        profile_id: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentsUiErrorCode {
    SnapshotUnavailable,
    TooManyArguments,
    InvalidRegex,
    InvalidMcpFlag,
    RegistrationFailed,
    DeleteFailed,
    LaunchFailed,
}

impl AgentsUiErrorCode {
    const fn message(self) -> &'static str {
        match self {
            Self::SnapshotUnavailable => "Agent 설정을 불러오지 못했습니다.",
            Self::TooManyArguments => "Agent argument 개수 또는 크기 상한을 초과했습니다.",
            Self::InvalidRegex => "상태 정규식이 올바르지 않습니다.",
            Self::InvalidMcpFlag => "MCP 설정 플래그가 올바르지 않습니다.",
            Self::RegistrationFailed => "Agent 설정 저장에 실패했습니다.",
            Self::DeleteFailed => "Agent 설정 삭제에 실패했습니다.",
            Self::LaunchFailed => "Agent 실행에 실패했습니다.",
        }
    }
}

pub struct AgentsUi {
    name: String,
    command: String,
    args_input: String,
    waiting_regex: String,
    approval_regex: String,
    error_regex: String,
    done_regex: String,
    mcp_proxy_enabled: bool,
    mcp_proxy_server_id: Option<String>,
    mcp_config_flag: String,
    run_profile: Option<String>,
    pending_production_run: Option<PendingProductionRun>,
    pending_launches: u32,
    registration_pending: bool,
    error: Option<AgentsUiErrorCode>,
}

impl AgentsUi {
    pub fn new() -> Self {
        Self {
            name: String::new(),
            command: String::new(),
            args_input: String::new(),
            waiting_regex: String::new(),
            approval_regex: String::new(),
            error_regex: String::new(),
            done_regex: String::new(),
            mcp_proxy_enabled: false,
            mcp_proxy_server_id: None,
            mcp_config_flag: String::new(),
            run_profile: None,
            pending_production_run: None,
            pending_launches: 0,
            registration_pending: false,
            error: None,
        }
    }

    pub fn invalidate_snapshot_selection(&mut self) {
        self.run_profile = None;
        self.mcp_proxy_server_id = None;
        self.pending_production_run = None;
    }

    pub fn take_pending(&mut self) -> u32 {
        std::mem::take(&mut self.pending_launches)
    }

    pub fn restore_pending(&mut self, pending: u32) {
        self.pending_launches = pending;
    }

    pub fn mark_launch_accepted(&mut self) {
        self.pending_launches = self.pending_launches.saturating_add(1);
        self.error = None;
    }

    pub fn observe_launch_succeeded(&mut self) {
        self.pending_launches = self.pending_launches.saturating_sub(1);
    }

    pub fn observe_launch_failed(&mut self) {
        self.pending_launches = self.pending_launches.saturating_sub(1);
        self.error = Some(AgentsUiErrorCode::LaunchFailed);
    }

    pub fn report_error(&mut self, code: AgentsUiErrorCode) {
        self.error = Some(code);
        if code == AgentsUiErrorCode::RegistrationFailed {
            self.registration_pending = false;
        }
    }

    pub fn registration_succeeded(&mut self) {
        self.name.clear();
        self.command.clear();
        self.args_input.clear();
        self.waiting_regex.clear();
        self.approval_regex.clear();
        self.error_regex.clear();
        self.done_regex.clear();
        self.mcp_proxy_enabled = false;
        self.mcp_proxy_server_id = None;
        self.mcp_config_flag.clear();
        self.registration_pending = false;
        self.error = None;
    }

    /// Pure render path: borrows an immutable snapshot and returns at most one intent.
    pub fn contents(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &AgentsSnapshot,
        catalog: &i18n::Catalog,
    ) -> Option<AgentsIntent> {
        self.enforce_draft_limits();
        self.reconcile_selection(snapshot);
        let mut intent = None;

        if !snapshot.is_available() {
            self.error = Some(AgentsUiErrorCode::SnapshotUnavailable);
        }
        if snapshot.agents().is_empty() {
            ui.label(catalog.t("agents.empty", &[]));
        }

        self.render_profile_picker(ui, snapshot, catalog);
        self.render_agent_rows(ui, snapshot, catalog, &mut intent);
        self.render_registration(ui, snapshot, catalog, &mut intent);
        self.render_production_confirmation(ui.ctx(), snapshot.revision(), catalog, &mut intent);

        if let Some(error) = self.error {
            ui.colored_label(ui.visuals().error_fg_color, error.message());
        }
        intent
    }

    fn reconcile_selection(&mut self, snapshot: &AgentsSnapshot) {
        if self
            .run_profile
            .as_ref()
            .is_some_and(|id| !snapshot.profiles().iter().any(|profile| profile.id() == id))
        {
            self.run_profile = None;
            self.pending_production_run = None;
        }
        if !self.mcp_proxy_enabled {
            self.mcp_proxy_server_id = None;
            self.mcp_config_flag.clear();
        } else if self
            .mcp_proxy_server_id
            .as_ref()
            .is_some_and(|id| !snapshot.backends().iter().any(|backend| backend.id() == id))
        {
            self.mcp_proxy_server_id = None;
        }
    }

    fn render_profile_picker(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &AgentsSnapshot,
        catalog: &i18n::Catalog,
    ) {
        ui.horizontal(|ui| {
            ui.label(catalog.t("agents.run_profile", &[]));
            let selected = self.run_profile.as_deref().and_then(|id| {
                snapshot
                    .profiles()
                    .iter()
                    .find(|profile| profile.id() == id)
            });
            let fallback = catalog.t("common.none", &[]);
            let selected_text = selected
                .map(AgentProfileItem::name)
                .unwrap_or(fallback.as_str());
            egui::ComboBox::from_id_salt("agent_run_profile")
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(self.run_profile.is_none(), catalog.t("common.none", &[]))
                        .clicked()
                    {
                        self.run_profile = None;
                    }
                    for profile in snapshot.profiles() {
                        let selected = self.run_profile.as_deref() == Some(profile.id());
                        let response = if profile.is_production() {
                            ui.selectable_label(selected, format!("⚠ {}", profile.name()))
                        } else {
                            ui.selectable_label(selected, profile.name())
                        };
                        if response.clicked() {
                            self.run_profile = Some(profile.id().to_owned());
                        }
                    }
                });
            if selected.is_some_and(AgentProfileItem::is_production) {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    catalog.t("agents.production_profile", &[]),
                );
            }
        });
    }

    fn render_agent_rows(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &AgentsSnapshot,
        catalog: &i18n::Catalog,
        intent: &mut Option<AgentsIntent>,
    ) {
        egui::ScrollArea::vertical()
            .id_salt("agents_bounded_rows")
            .max_height(AGENT_LIST_MAX_HEIGHT)
            .show_rows(
                ui,
                AGENT_ROW_HEIGHT,
                snapshot.agents().len(),
                |ui, range| {
                    for agent in &snapshot.agents()[range] {
                        ui.horizontal(|ui| {
                            ui.label(agent.name());
                            ui.weak("—");
                            ui.monospace(agent.command());
                            ui.monospace(agent.args_summary());
                            if ui.button(catalog.t("action.run", &[])).clicked() && intent.is_none()
                            {
                                if let Some(profile) = production_profile_to_confirm(
                                    self.run_profile.as_deref(),
                                    snapshot.profiles(),
                                ) {
                                    self.pending_production_run = Some(PendingProductionRun {
                                        agent_id: agent.id().to_owned(),
                                        agent_name: agent.name().to_owned(),
                                        profile_id: profile.id().to_owned(),
                                        profile_name: profile.name().to_owned(),
                                    });
                                } else {
                                    *intent = Some(AgentsIntent::Run {
                                        revision: snapshot.revision(),
                                        agent_id: agent.id().to_owned(),
                                        profile_id: self.run_profile.clone(),
                                    });
                                }
                            }
                            if ui.button(catalog.t("action.delete", &[])).clicked()
                                && intent.is_none()
                            {
                                *intent = Some(AgentsIntent::Delete {
                                    revision: snapshot.revision(),
                                    agent_id: agent.id().to_owned(),
                                });
                            }
                        });
                    }
                },
            );
    }

    fn render_registration(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &AgentsSnapshot,
        catalog: &i18n::Catalog,
        intent: &mut Option<AgentsIntent>,
    ) {
        ui.separator();
        ui.heading(catalog.t("agents.register", &[]));
        ui.horizontal(|ui| {
            ui.label(catalog.t("common.name", &[]));
            ui.text_edit_singleline(&mut self.name);
        });
        ui.horizontal(|ui| {
            ui.label(catalog.t("common.command", &[]));
            ui.text_edit_singleline(&mut self.command);
        });
        ui.label(catalog.t("agents.args_one_per_line", &[]));
        ui.add(
            egui::TextEdit::multiline(&mut self.args_input)
                .desired_rows(3)
                .font(egui::TextStyle::Monospace),
        );
        ui.collapsing(catalog.t("agents.status_regex", &[]), |ui| {
            for (label, field) in [
                ("waiting", &mut self.waiting_regex),
                ("approval", &mut self.approval_regex),
                ("error", &mut self.error_regex),
                ("done", &mut self.done_regex),
            ] {
                ui.horizontal(|ui| {
                    ui.label(label);
                    ui.add(egui::TextEdit::singleline(field).font(egui::TextStyle::Monospace));
                });
            }
        });
        ui.checkbox(
            &mut self.mcp_proxy_enabled,
            catalog.t("agents.mcp_proxy", &[]),
        );
        if self.mcp_proxy_enabled {
            self.render_backend_picker(ui, snapshot, catalog);
            ui.horizontal(|ui| {
                ui.label(catalog.t("agents.config_flag", &[]));
                ui.add(
                    egui::TextEdit::singleline(&mut self.mcp_config_flag)
                        .font(egui::TextStyle::Monospace),
                );
            });
        }

        // Text widgets may receive a large paste in this frame. Bound the retained draft before
        // validating it or moving it into an intent.
        self.enforce_draft_limits();

        let proxy_ok = !self.mcp_proxy_enabled || self.mcp_proxy_server_id.is_some();
        let flag_ok =
            !self.mcp_proxy_enabled || validate_mcp_config_flag(&self.mcp_config_flag).is_ok();
        let filled = !self.name.trim().is_empty()
            && !self.command.trim().is_empty()
            && proxy_ok
            && flag_ok
            && !self.registration_pending
            && snapshot.is_available();
        if self.mcp_proxy_enabled && self.mcp_proxy_server_id.is_none() {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                catalog.t("agents.select_backend", &[]),
            );
        }
        if self.mcp_proxy_enabled && !flag_ok {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                AgentsUiErrorCode::InvalidMcpFlag.message(),
            );
        }
        if ui
            .add_enabled(filled, egui::Button::new(catalog.t("agents.register", &[])))
            .clicked()
            && intent.is_none()
        {
            match self.registration() {
                Ok(registration) => {
                    self.registration_pending = true;
                    self.error = None;
                    *intent = Some(AgentsIntent::Register {
                        revision: snapshot.revision(),
                        registration,
                    });
                }
                Err(error) => self.error = Some(error),
            }
        }
    }

    fn render_backend_picker(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &AgentsSnapshot,
        catalog: &i18n::Catalog,
    ) {
        ui.horizontal(|ui| {
            ui.label(catalog.t("common.backend", &[]));
            let selected = self.mcp_proxy_server_id.as_deref().and_then(|id| {
                snapshot
                    .backends()
                    .iter()
                    .find(|backend| backend.id() == id)
            });
            let fallback = catalog.t("common.select", &[]);
            egui::ComboBox::from_id_salt("agent_mcp_proxy_backend")
                .selected_text(selected.map(AgentBackendItem::name).unwrap_or(&fallback))
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(
                            self.mcp_proxy_server_id.is_none(),
                            catalog.t("common.select", &[]),
                        )
                        .clicked()
                    {
                        self.mcp_proxy_server_id = None;
                    }
                    for backend in snapshot.backends() {
                        if ui
                            .selectable_label(
                                self.mcp_proxy_server_id.as_deref() == Some(backend.id()),
                                backend.name(),
                            )
                            .clicked()
                        {
                            self.mcp_proxy_server_id = Some(backend.id().to_owned());
                        }
                    }
                });
        });
    }

    fn render_production_confirmation(
        &mut self,
        ctx: &egui::Context,
        revision: u64,
        catalog: &i18n::Catalog,
        intent: &mut Option<AgentsIntent>,
    ) {
        let Some(pending) = self.pending_production_run.as_ref() else {
            return;
        };
        let mut action = ProductionConfirmAction::None;
        egui::Window::new(catalog.t("agents.production_confirm_title", &[]))
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(catalog.t(
                    "agents.production_confirm_message",
                    &[
                        ("agent", pending.agent_name.as_str()),
                        ("profile", pending.profile_name.as_str()),
                    ],
                ));
                ui.weak(catalog.t("agents.production_confirm_secret_hint", &[]));
                ui.horizontal(|ui| {
                    if ui.button(catalog.t("action.cancel", &[])).clicked() {
                        action = ProductionConfirmAction::Cancel;
                    }
                    if ui.button(catalog.t("action.run", &[])).clicked() {
                        action = ProductionConfirmAction::Run;
                    }
                });
            });
        match action {
            ProductionConfirmAction::None => {}
            ProductionConfirmAction::Cancel => self.pending_production_run = None,
            ProductionConfirmAction::Run if intent.is_none() => {
                let pending = self.pending_production_run.take().expect("pending checked");
                *intent = Some(AgentsIntent::Run {
                    revision,
                    agent_id: pending.agent_id,
                    profile_id: Some(pending.profile_id),
                });
            }
            ProductionConfirmAction::Run => {}
        }
    }

    fn registration(&self) -> Result<AgentRegistration, AgentsUiErrorCode> {
        let args = self
            .args_input
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let args_bytes = args
            .iter()
            .map(String::len)
            .try_fold(0usize, usize::checked_add)
            .ok_or(AgentsUiErrorCode::TooManyArguments)?;
        if args.len() > AGENT_ARGS_MAX_ITEMS || args_bytes > AGENT_ARGS_MAX_BYTES {
            return Err(AgentsUiErrorCode::TooManyArguments);
        }
        for pattern in [
            &self.waiting_regex,
            &self.approval_regex,
            &self.error_regex,
            &self.done_regex,
        ] {
            let pattern = pattern.trim();
            if !pattern.is_empty() && regex::Regex::new(pattern).is_err() {
                return Err(AgentsUiErrorCode::InvalidRegex);
            }
        }
        let mcp_config_flag = if self.mcp_proxy_enabled {
            validate_mcp_config_flag(&self.mcp_config_flag)
                .map_err(|_| AgentsUiErrorCode::InvalidMcpFlag)?
        } else {
            None
        };
        Ok(AgentRegistration {
            name: self.name.trim().to_owned(),
            command: self.command.trim().to_owned(),
            args,
            waiting_regex: optional_trimmed(&self.waiting_regex),
            approval_regex: optional_trimmed(&self.approval_regex),
            error_regex: optional_trimmed(&self.error_regex),
            done_regex: optional_trimmed(&self.done_regex),
            mcp_proxy_enabled: self.mcp_proxy_enabled,
            mcp_proxy_server_id: self.mcp_proxy_server_id.clone(),
            mcp_config_flag,
        })
    }

    fn enforce_draft_limits(&mut self) {
        for field in [
            &mut self.name,
            &mut self.command,
            &mut self.waiting_regex,
            &mut self.approval_regex,
            &mut self.error_regex,
            &mut self.done_regex,
        ] {
            truncate_utf8(field, SHORT_INPUT_MAX_BYTES);
        }
        truncate_utf8(&mut self.args_input, AGENT_ARGS_MAX_BYTES);
        truncate_utf8(&mut self.mcp_config_flag, FLAG_INPUT_MAX_BYTES);
    }
}

impl Default for AgentsUi {
    fn default() -> Self {
        Self::new()
    }
}

struct PendingProductionRun {
    agent_id: String,
    agent_name: String,
    profile_id: String,
    profile_name: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProductionConfirmAction {
    None,
    Cancel,
    Run,
}

fn production_profile_to_confirm<'a>(
    run_profile: Option<&str>,
    profiles: &'a [AgentProfileItem],
) -> Option<&'a AgentProfileItem> {
    let id = run_profile?;
    profiles
        .iter()
        .find(|profile| profile.id() == id && profile.is_production())
}

fn validate_mcp_config_flag(raw: &str) -> Result<Option<String>, ()> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if !trimmed.starts_with('-') || trimmed.contains(char::is_whitespace) {
        return Err(());
    }
    Ok(Some(trimmed.to_owned()))
}

fn optional_trimmed(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    struct FakeAdapter {
        calls: Cell<usize>,
    }

    impl FakeAdapter {
        fn snapshot(&self, item_count: usize) -> AgentsSnapshot {
            self.calls.set(self.calls.get() + 1);
            let agents = (0..item_count)
                .map(|index| {
                    AgentListItem::new(
                        format!("agent-{index}"),
                        format!("Agent {index}"),
                        "codex",
                        AgentArgsSummary::visible("--safe"),
                    )
                })
                .collect();
            AgentsSnapshot::try_new(7, agents, Vec::new(), Vec::new()).unwrap()
        }
    }

    #[test]
    fn unchanged_snapshot_renders_300_frames_without_adapter_calls() {
        let adapter = FakeAdapter {
            calls: Cell::new(0),
        };
        let snapshot = adapter.snapshot(32);
        let calls_after_snapshot = adapter.calls.get();
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let context = egui::Context::default();
        let mut state = AgentsUi::new();
        for _ in 0..300 {
            let output = context.run_ui(egui::RawInput::default(), |ui| {
                assert!(state.contents(ui, &snapshot, &catalog).is_none());
            });
            assert!(output.platform_output.commands.is_empty());
        }
        assert_eq!(adapter.calls.get(), calls_after_snapshot);
    }

    #[test]
    fn large_agent_list_is_bounded_and_virtualized() {
        let adapter = FakeAdapter {
            calls: Cell::new(0),
        };
        let snapshot = adapter.snapshot(AGENT_SNAPSHOT_MAX_ITEMS);
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let context = egui::Context::default();
        let mut state = AgentsUi::new();
        let output = context.run_ui(egui::RawInput::default(), |ui| {
            assert!(state.contents(ui, &snapshot, &catalog).is_none());
        });
        assert!(output.shapes.len() < AGENT_SNAPSHOT_MAX_ITEMS);
        let over = (0..=AGENT_SNAPSHOT_MAX_ITEMS)
            .map(|index| {
                AgentListItem::new(
                    format!("agent-{index}"),
                    "name",
                    "command",
                    AgentArgsSummary::redacted(),
                )
            })
            .collect();
        assert!(matches!(
            AgentsSnapshot::try_new(8, over, Vec::new(), Vec::new()),
            Err(AgentsSnapshotError::TooManyAgents)
        ));
    }

    #[test]
    fn flag_and_registration_bounds_are_pure() {
        assert!(validate_mcp_config_flag("--a --b").is_err());
        assert_eq!(validate_mcp_config_flag(""), Ok(None));
        assert_eq!(
            validate_mcp_config_flag("  --mcp-config-file  "),
            Ok(Some("--mcp-config-file".to_owned()))
        );
        assert!(validate_mcp_config_flag("mcp-config").is_err());

        let mut state = AgentsUi::new();
        state.name = "name".to_owned();
        state.command = "command".to_owned();
        state.args_input = (0..=AGENT_ARGS_MAX_ITEMS)
            .map(|index| format!("arg-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(matches!(
            state.registration(),
            Err(AgentsUiErrorCode::TooManyArguments)
        ));
    }

    #[test]
    fn production_profile_requires_confirmation() {
        let profiles = [
            AgentProfileItem::new("dev", "Development", false),
            AgentProfileItem::new("prod", "Production", true),
        ];
        assert_eq!(
            production_profile_to_confirm(Some("prod"), &profiles).map(AgentProfileItem::id),
            Some("prod")
        );
        assert!(production_profile_to_confirm(Some("dev"), &profiles).is_none());
    }

    #[test]
    fn production_source_has_no_service_storage_or_io_edge() {
        let source = include_str!("agents.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for forbidden in [
            ["crate::", "storage"].concat(),
            ["Runtime", "Client"].concat(),
            ["Runtime", "Command"].concat(),
            ["std::", "fs"].concat(),
            ["std::", "process"].concat(),
            ["request_repaint_", "after"].concat(),
            ["req", "west"].concat(),
            ["Tcp", "Stream"].concat(),
            ["Udp", "Socket"].concat(),
            ["clip", "board"].concat(),
            ["r", "fd::"].concat(),
            ["Secret", "String"].concat(),
            ["Keyring", "SecretStore"].concat(),
        ] {
            assert!(!source.contains(&forbidden));
        }
    }
}
