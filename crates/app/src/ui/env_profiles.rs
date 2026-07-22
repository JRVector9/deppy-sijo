use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

pub const ENV_SNAPSHOT_MAX_PROFILES: usize = 256;
pub const ENV_SNAPSHOT_MAX_VARS: usize = 4_096;
pub const ENV_SNAPSHOT_MAX_BYTES: usize = 4 * 1024 * 1024;
pub const ENV_REVEALED_MAX_ITEMS: usize = 64;
pub const ENV_REVEALED_MAX_BYTES: usize = 1024 * 1024;
pub const ENV_REVEALED_VALUE_MAX_BYTES: usize = 64 * 1024;

const ENV_KEY_INPUT_MAX_BYTES: usize = 1024;
const ENV_VALUE_INPUT_MAX_BYTES: usize = 64 * 1024;
const ENV_ROW_HEIGHT: f32 = 30.0;
const ENV_LIST_MAX_HEIGHT: f32 = 360.0;
const LEGACY_LIST_MAX_HEIGHT: f32 = 240.0;
const MASKED_VALUE: &str = "••••••••••••••••";

/// UI-only value classification. It intentionally has no Clone/Debug/Serialize implementation.
/// Plain values must already have passed the canonical storage classification in the adapter.
pub enum EnvValueView {
    Plain {
        value: Arc<str>,
        has_os_override: bool,
    },
    Secret {
        reveal_handle: Arc<str>,
        credential_available: bool,
        has_os_override: bool,
    },
}

impl EnvValueView {
    pub fn plain(value: impl Into<Arc<str>>, has_os_override: bool) -> Self {
        Self::Plain {
            value: value.into(),
            has_os_override,
        }
    }

    pub fn secret(
        reveal_handle: impl Into<Arc<str>>,
        credential_available: bool,
        has_os_override: bool,
    ) -> Self {
        Self::Secret {
            reveal_handle: reveal_handle.into(),
            credential_available,
            has_os_override,
        }
    }

    fn is_secret(&self) -> bool {
        matches!(self, Self::Secret { .. })
    }

    fn has_os_override(&self) -> bool {
        match self {
            Self::Plain {
                has_os_override, ..
            }
            | Self::Secret {
                has_os_override, ..
            } => *has_os_override,
        }
    }

    fn retained_bytes(&self) -> usize {
        match self {
            Self::Plain { value, .. } => value.len(),
            Self::Secret { reveal_handle, .. } => reveal_handle.len(),
        }
    }
}

/// Immutable snapshot row without any storage row or secret-crate type.
pub struct EnvVarItem {
    profile_id: Arc<str>,
    key: Arc<str>,
    value: EnvValueView,
}

impl EnvVarItem {
    pub fn new(
        profile_id: impl Into<Arc<str>>,
        key: impl Into<Arc<str>>,
        value: EnvValueView,
    ) -> Self {
        Self {
            profile_id: profile_id.into(),
            key: key.into(),
            value,
        }
    }

    pub fn profile_id(&self) -> &str {
        &self.profile_id
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn value(&self) -> &EnvValueView {
        &self.value
    }

    fn retained_bytes(&self) -> usize {
        self.profile_id.len() + self.key.len() + self.value.retained_bytes()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvSnapshotError {
    TooManyProfiles,
    TooManyVariables,
    ByteBudgetExceeded,
}

impl std::fmt::Display for EnvSnapshotError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::TooManyProfiles => "env_snapshot_profile_limit",
            Self::TooManyVariables => "env_snapshot_variable_limit",
            Self::ByteBudgetExceeded => "env_snapshot_byte_limit",
        })
    }
}

impl std::error::Error for EnvSnapshotError {}

/// Bounded immutable render input. Secret plaintext is never part of this snapshot.
pub struct EnvProfilesSnapshot {
    revision: u64,
    available: bool,
    workspace_id: Arc<str>,
    project_root_configured: bool,
    dotenv_profile_id: Option<Arc<str>>,
    dotenv_vars: Arc<[EnvVarItem]>,
    legacy_vars: Arc<[EnvVarItem]>,
}

impl EnvProfilesSnapshot {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        revision: u64,
        workspace_id: impl Into<Arc<str>>,
        project_root_configured: bool,
        dotenv_profile_id: Option<impl Into<Arc<str>>>,
        legacy_profile_count: usize,
        dotenv_vars: Vec<EnvVarItem>,
        legacy_vars: Vec<EnvVarItem>,
    ) -> Result<Self, EnvSnapshotError> {
        if legacy_profile_count.saturating_add(usize::from(dotenv_profile_id.is_some()))
            > ENV_SNAPSHOT_MAX_PROFILES
        {
            return Err(EnvSnapshotError::TooManyProfiles);
        }
        if dotenv_vars.len().saturating_add(legacy_vars.len()) > ENV_SNAPSHOT_MAX_VARS {
            return Err(EnvSnapshotError::TooManyVariables);
        }
        let workspace_id = workspace_id.into();
        let dotenv_profile_id = dotenv_profile_id.map(Into::into);
        let retained_bytes = std::iter::once(workspace_id.len())
            .chain(dotenv_profile_id.iter().map(|id| id.len()))
            .chain(dotenv_vars.iter().map(EnvVarItem::retained_bytes))
            .chain(legacy_vars.iter().map(EnvVarItem::retained_bytes))
            .try_fold(0usize, usize::checked_add)
            .ok_or(EnvSnapshotError::ByteBudgetExceeded)?;
        if retained_bytes > ENV_SNAPSHOT_MAX_BYTES {
            return Err(EnvSnapshotError::ByteBudgetExceeded);
        }
        Ok(Self {
            revision,
            available: true,
            workspace_id,
            project_root_configured,
            dotenv_profile_id,
            dotenv_vars: dotenv_vars.into(),
            legacy_vars: legacy_vars.into(),
        })
    }

    pub fn unavailable(
        revision: u64,
        workspace_id: impl Into<Arc<str>>,
        project_root_configured: bool,
    ) -> Self {
        Self {
            revision,
            available: false,
            workspace_id: workspace_id.into(),
            project_root_configured,
            dotenv_profile_id: None,
            dotenv_vars: Arc::from([]),
            legacy_vars: Arc::from([]),
        }
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub const fn is_available(&self) -> bool {
        self.available
    }

    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    pub const fn project_root_configured(&self) -> bool {
        self.project_root_configured
    }

    pub fn dotenv_profile_id(&self) -> Option<&str> {
        self.dotenv_profile_id.as_deref()
    }

    pub fn dotenv_vars(&self) -> &[EnvVarItem] {
        &self.dotenv_vars
    }

    pub fn legacy_vars(&self) -> &[EnvVarItem] {
        &self.legacy_vars
    }
}

/// Secret plaintext delivery from the root-owned background reveal worker. It is non-Clone,
/// non-Debug, and non-Serialize; its owned value is zeroized on drop.
pub struct RevealedEnvValue {
    profile_id: String,
    key: String,
    value: SensitiveDisplay,
}

impl RevealedEnvValue {
    pub fn new(
        profile_id: impl Into<String>,
        key: impl Into<String>,
        value: String,
    ) -> Result<Self, EnvRevealError> {
        if value.len() > ENV_REVEALED_VALUE_MAX_BYTES {
            return Err(EnvRevealError::ValueTooLarge);
        }
        Ok(Self {
            profile_id: profile_id.into(),
            key: key.into(),
            value: SensitiveDisplay(value),
        })
    }
}

struct SensitiveDisplay(String);

impl SensitiveDisplay {
    fn expose(&self) -> &str {
        &self.0
    }

    fn len(&self) -> usize {
        self.0.len()
    }
}

impl Drop for SensitiveDisplay {
    fn drop(&mut self) {
        // SAFETY: this value exclusively owns the String and only overwrites existing bytes.
        for byte in unsafe { self.0.as_mut_vec() } {
            // SAFETY: `byte` is exclusively borrowed from the owned allocation.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvRevealError {
    ValueTooLarge,
    CorpusFull,
}

impl std::fmt::Display for EnvRevealError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ValueTooLarge => "env_reveal_value_limit",
            Self::CorpusFull => "env_reveal_corpus_limit",
        })
    }
}

impl std::error::Error for EnvRevealError {}

/// Environment UI intent. User-entered values may be sensitive, therefore this enum deliberately
/// has no Clone/Debug/Display/Serialize implementation.
pub enum EnvAction {
    ChooseProjectFolder,
    SetProjectPath(PathBuf),
    Resync,
    DotenvWrite {
        key: String,
        value: Option<String>,
    },
    DeleteLegacyVar {
        profile_id: String,
        key: String,
    },
    RevealSecret {
        profile_id: String,
        key: String,
        reveal_handle: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvUiErrorCode {
    SnapshotUnavailable,
    RevealFailed,
    RevealCapacityExceeded,
    LegacyDeleteFailed,
}

impl EnvUiErrorCode {
    const fn message(self) -> &'static str {
        match self {
            Self::SnapshotUnavailable => "환경 설정을 불러오지 못했습니다.",
            Self::RevealFailed => "Secret 값을 불러오지 못했습니다.",
            Self::RevealCapacityExceeded => "동시에 표시할 수 있는 secret 상한을 초과했습니다.",
            Self::LegacyDeleteFailed => "레거시 환경 변수 삭제에 실패했습니다.",
        }
    }
}

pub struct EnvProfilesUi {
    var_key: String,
    var_plain_value: String,
    error: Option<EnvUiErrorCode>,
    show_add_form: bool,
    delete_confirm: Option<(String, String)>,
    revealed: HashMap<(String, String), SensitiveDisplay>,
    revealed_bytes: usize,
    reveal_pending: HashSet<(String, String)>,
    masked: HashSet<(String, String)>,
    snapshot_workspace: Option<String>,
    snapshot_revision: Option<u64>,
}

impl EnvProfilesUi {
    pub fn new() -> Self {
        Self {
            var_key: String::new(),
            var_plain_value: String::new(),
            error: None,
            show_add_form: false,
            delete_confirm: None,
            revealed: HashMap::new(),
            revealed_bytes: 0,
            reveal_pending: HashSet::new(),
            masked: HashSet::new(),
            snapshot_workspace: None,
            snapshot_revision: None,
        }
    }

    pub fn invalidate_cache(&mut self) {
        self.snapshot_revision = None;
        self.delete_confirm = None;
        self.clear_revealed();
        self.reveal_pending.clear();
    }

    pub fn accept_revealed(&mut self, revealed: RevealedEnvValue) -> Result<(), EnvRevealError> {
        let RevealedEnvValue {
            profile_id,
            key,
            value,
        } = revealed;
        let id = (profile_id, key);
        let previous_bytes = self.revealed.get(&id).map_or(0, SensitiveDisplay::len);
        let next_bytes = self
            .revealed_bytes
            .saturating_sub(previous_bytes)
            .saturating_add(value.len());
        if (self.revealed.len() >= ENV_REVEALED_MAX_ITEMS && !self.revealed.contains_key(&id))
            || next_bytes > ENV_REVEALED_MAX_BYTES
        {
            self.reveal_pending.remove(&id);
            self.masked.insert(id);
            self.error = Some(EnvUiErrorCode::RevealCapacityExceeded);
            return Err(EnvRevealError::CorpusFull);
        }
        self.revealed_bytes = next_bytes;
        self.revealed.insert(id.clone(), value);
        self.reveal_pending.remove(&id);
        self.masked.remove(&id);
        self.error = None;
        Ok(())
    }

    pub fn reject_reveal(&mut self, profile_id: &str, key: &str) {
        let id = (profile_id.to_owned(), key.to_owned());
        self.reveal_pending.remove(&id);
        self.masked.insert(id);
        self.error = Some(EnvUiErrorCode::RevealFailed);
    }

    pub fn report_error(&mut self, code: EnvUiErrorCode) {
        self.error = Some(code);
    }

    /// Pure render path: consumes no service and returns at most one intent.
    pub fn contents_compact(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &EnvProfilesSnapshot,
        catalog: &i18n::Catalog,
    ) -> Option<EnvAction> {
        self.sync_snapshot_state(snapshot);
        truncate_utf8(&mut self.var_key, ENV_KEY_INPUT_MAX_BYTES);
        truncate_utf8(&mut self.var_plain_value, ENV_VALUE_INPUT_MAX_BYTES);
        let mut intent = None;

        if !snapshot.is_available() {
            self.error = Some(EnvUiErrorCode::SnapshotUnavailable);
        }
        if !snapshot.project_root_configured() {
            super::section_header(ui, &catalog.t("env.env_vars", &[]), None, None);
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(catalog.t("env.no_project_path_note", &[]))
                    .size(13.0)
                    .color(ui.visuals().weak_text_color()),
            );
            ui.add_space(8.0);
            if ui
                .button(catalog.t("env.project_folder.choose", &[]))
                .clicked()
            {
                intent = Some(EnvAction::ChooseProjectFolder);
            }
            self.render_legacy_vars(ui, snapshot, catalog, &mut intent);
            self.render_error(ui);
            return intent;
        }

        let profile_id = snapshot.dotenv_profile_id().unwrap_or_default();
        let add_label = format!("+ {}", catalog.t("action.add", &[]));
        if super::section_header(
            ui,
            &catalog.t("env.env_vars", &[]),
            Some(snapshot.dotenv_vars().len()),
            Some(&add_label),
        ) {
            self.show_add_form = !self.show_add_form;
            if self.show_add_form {
                ui.memory_mut(|memory| memory.request_focus(env_var_key_input_id()));
            }
        }
        env_table_header(
            ui,
            &catalog.t("common.key", &[]),
            &catalog.t("common.value", &[]),
        );

        let mut row_action = None;
        egui::ScrollArea::vertical()
            .id_salt("dotenv_bounded_rows")
            .max_height(ENV_LIST_MAX_HEIGHT)
            .show_rows(
                ui,
                ENV_ROW_HEIGHT,
                snapshot.dotenv_vars().len(),
                |ui, range| {
                    for var in &snapshot.dotenv_vars()[range] {
                        let id = row_id(var);
                        let response = env_table_row(
                            ui,
                            var,
                            self.revealed.get(&id).map(SensitiveDisplay::expose),
                            self.masked.contains(&id) || self.reveal_pending.contains(&id),
                            catalog,
                        );
                        if response.delete {
                            row_action = Some(EnvRowAction::ConfirmDelete {
                                profile_id: var.profile_id().to_owned(),
                                key: var.key().to_owned(),
                            });
                        } else if response.toggle_reveal {
                            row_action = Some(EnvRowAction::Toggle {
                                profile_id: var.profile_id().to_owned(),
                                key: var.key().to_owned(),
                            });
                        }
                    }
                },
            );
        if let Some(action) = row_action {
            self.handle_row_action(action, snapshot.dotenv_vars(), &mut intent);
        }
        if snapshot.dotenv_vars().is_empty() && env_empty_placeholder_row(ui, catalog) {
            self.show_add_form = true;
        }

        if self.show_add_form
            && let Some(action) = compact_env_var_form(ui, self, profile_id, catalog)
            && intent.is_none()
        {
            intent = Some(action);
        }
        self.render_delete_confirmation(ui.ctx(), profile_id, catalog, &mut intent);
        self.render_legacy_vars(ui, snapshot, catalog, &mut intent);
        self.render_error(ui);
        intent
    }

    fn sync_snapshot_state(&mut self, snapshot: &EnvProfilesSnapshot) {
        let workspace_changed = self.snapshot_workspace.as_deref() != Some(snapshot.workspace_id());
        if workspace_changed {
            self.snapshot_workspace = Some(snapshot.workspace_id().to_owned());
            self.snapshot_revision = None;
            self.reset_var_form();
            self.masked.clear();
            self.reveal_pending.clear();
            self.clear_revealed();
        }
        if self.snapshot_revision == Some(snapshot.revision()) {
            return;
        }
        self.snapshot_revision = Some(snapshot.revision());
        let live = snapshot
            .dotenv_vars()
            .iter()
            .chain(snapshot.legacy_vars())
            .map(row_id)
            .collect::<HashSet<_>>();
        self.masked.retain(|id| live.contains(id));
        self.reveal_pending.retain(|id| live.contains(id));
        let removed = self
            .revealed
            .extract_if(|id, _| !live.contains(id))
            .map(|(_, value)| value.len())
            .sum::<usize>();
        self.revealed_bytes = self.revealed_bytes.saturating_sub(removed);
        for var in snapshot.dotenv_vars().iter().chain(snapshot.legacy_vars()) {
            if var.value().is_secret() {
                let id = row_id(var);
                if !self.revealed.contains_key(&id) {
                    self.masked.insert(id);
                }
            }
        }
        if self
            .delete_confirm
            .as_ref()
            .is_some_and(|id| !live.contains(id))
        {
            self.delete_confirm = None;
        }
    }

    fn handle_row_action(
        &mut self,
        action: EnvRowAction,
        rows: &[EnvVarItem],
        intent: &mut Option<EnvAction>,
    ) {
        match action {
            EnvRowAction::ConfirmDelete { profile_id, key } => {
                self.delete_confirm = Some((profile_id, key));
            }
            EnvRowAction::Toggle { profile_id, key } => {
                let id = (profile_id, key);
                let Some(var) = rows
                    .iter()
                    .find(|var| var.profile_id() == id.0 && var.key() == id.1)
                else {
                    return;
                };
                match var.value() {
                    EnvValueView::Plain { .. } => {
                        if !self.masked.remove(&id) {
                            self.masked.insert(id);
                        }
                    }
                    EnvValueView::Secret {
                        reveal_handle,
                        credential_available,
                        ..
                    } => {
                        if self.revealed.contains_key(&id) {
                            if let Some(value) = self.revealed.remove(&id) {
                                self.revealed_bytes =
                                    self.revealed_bytes.saturating_sub(value.len());
                            }
                            self.masked.insert(id);
                        } else if !credential_available {
                            if !self.masked.remove(&id) {
                                self.masked.insert(id);
                            }
                        } else if self.reveal_pending.contains(&id) {
                            // One in-flight request per row. The root may still complete a request
                            // after the user navigates away; snapshot reconciliation discards it.
                        } else if intent.is_none() {
                            self.reveal_pending.insert(id.clone());
                            self.masked.insert(id.clone());
                            *intent = Some(EnvAction::RevealSecret {
                                profile_id: id.0,
                                key: id.1,
                                reveal_handle: reveal_handle.to_string(),
                            });
                        }
                    }
                }
            }
        }
    }

    fn render_delete_confirmation(
        &mut self,
        ctx: &egui::Context,
        profile_id: &str,
        catalog: &i18n::Catalog,
        intent: &mut Option<EnvAction>,
    ) {
        if self
            .delete_confirm
            .as_ref()
            .is_some_and(|(pending_profile, _)| pending_profile != profile_id)
        {
            self.delete_confirm = None;
        }
        let Some((_, pending_key)) = self.delete_confirm.as_ref() else {
            return;
        };
        let mut decision = None;
        egui::Window::new(catalog.t("env.var_delete_confirm.title", &[]))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(catalog.t(
                    "env.var_delete_confirm.body_dotenv",
                    &[("key", pending_key.as_str())],
                ));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button(catalog.t("action.delete", &[])).clicked() {
                        decision = Some(true);
                    }
                    if ui.button(catalog.t("action.cancel", &[])).clicked() {
                        decision = Some(false);
                    }
                });
            });
        match decision {
            Some(true) if intent.is_none() => {
                let (_, key) = self.delete_confirm.take().expect("pending checked");
                self.remove_local_value(profile_id, &key);
                *intent = Some(EnvAction::DotenvWrite { key, value: None });
            }
            Some(false) => self.delete_confirm = None,
            _ => {}
        }
    }

    fn render_legacy_vars(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &EnvProfilesSnapshot,
        catalog: &i18n::Catalog,
        intent: &mut Option<EnvAction>,
    ) {
        if snapshot.legacy_vars().is_empty() {
            return;
        }
        ui.add_space(14.0);
        ui.label(
            egui::RichText::new(catalog.t("env.legacy_pending_note", &[]))
                .size(12.0)
                .color(ui.visuals().warn_fg_color),
        );
        env_table_header(
            ui,
            &catalog.t("common.key", &[]),
            &catalog.t("common.value", &[]),
        );
        let mut action = None;
        egui::ScrollArea::vertical()
            .id_salt("legacy_env_bounded_rows")
            .max_height(LEGACY_LIST_MAX_HEIGHT)
            .show_rows(
                ui,
                ENV_ROW_HEIGHT,
                snapshot.legacy_vars().len(),
                |ui, range| {
                    for var in &snapshot.legacy_vars()[range] {
                        let id = row_id(var);
                        let response = env_table_row(
                            ui,
                            var,
                            self.revealed.get(&id).map(SensitiveDisplay::expose),
                            self.masked.contains(&id) || self.reveal_pending.contains(&id),
                            catalog,
                        );
                        if response.delete {
                            action = Some(EnvRowAction::ConfirmDelete {
                                profile_id: var.profile_id().to_owned(),
                                key: var.key().to_owned(),
                            });
                        } else if response.toggle_reveal {
                            action = Some(EnvRowAction::Toggle {
                                profile_id: var.profile_id().to_owned(),
                                key: var.key().to_owned(),
                            });
                        }
                    }
                },
            );
        match action {
            Some(EnvRowAction::ConfirmDelete { profile_id, key }) if intent.is_none() => {
                self.remove_local_value(&profile_id, &key);
                *intent = Some(EnvAction::DeleteLegacyVar { profile_id, key });
            }
            Some(toggle @ EnvRowAction::Toggle { .. }) => {
                self.handle_row_action(toggle, snapshot.legacy_vars(), intent);
            }
            _ => {}
        }
    }

    fn remove_local_value(&mut self, profile_id: &str, key: &str) {
        let id = (profile_id.to_owned(), key.to_owned());
        self.masked.remove(&id);
        self.reveal_pending.remove(&id);
        if let Some(value) = self.revealed.remove(&id) {
            self.revealed_bytes = self.revealed_bytes.saturating_sub(value.len());
        }
        self.error = None;
    }

    fn render_error(&self, ui: &mut egui::Ui) {
        if let Some(error) = self.error {
            ui.colored_label(ui.visuals().error_fg_color, error.message());
        }
    }

    fn reset_var_form(&mut self) {
        self.show_add_form = false;
        self.delete_confirm = None;
        self.var_key.clear();
        clear_sensitive_string(&mut self.var_plain_value);
    }

    fn clear_revealed(&mut self) {
        self.revealed.clear();
        self.revealed_bytes = 0;
    }
}

impl Default for EnvProfilesUi {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for EnvProfilesUi {
    fn drop(&mut self) {
        clear_sensitive_string(&mut self.var_plain_value);
    }
}

enum EnvRowAction {
    ConfirmDelete { profile_id: String, key: String },
    Toggle { profile_id: String, key: String },
}

struct EnvRowResponse {
    delete: bool,
    toggle_reveal: bool,
}

fn env_table_header(ui: &mut egui::Ui, key_label: &str, value_label: &str) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 24.0), egui::Sense::hover());
    let columns = env_table_columns(rect);
    let painter = ui.painter();
    let color = ui.visuals().weak_text_color();
    let font = egui::FontId::monospace(12.0);
    for (column, label) in columns.iter().take(2).zip([key_label, value_label]) {
        painter.text(
            egui::pos2(column.left(), rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            font.clone(),
            color,
        );
    }
    super::hairline_row(ui, rect.bottom());
}

fn env_table_row(
    ui: &mut egui::Ui,
    var: &EnvVarItem,
    revealed_value: Option<&str>,
    is_masked: bool,
    catalog: &i18n::Catalog,
) -> EnvRowResponse {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ENV_ROW_HEIGHT),
        egui::Sense::hover(),
    );
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let columns = env_table_columns(rect);
    let painter = ui.painter();
    let y = rect.center().y;
    painter.with_clip_rect(columns[0]).text(
        egui::pos2(columns[0].left() + 2.0, y),
        egui::Align2::LEFT_CENTER,
        var.key(),
        egui::FontId::monospace(14.0),
        ui.visuals().hyperlink_color,
    );
    let deleted_label = catalog.t("env.deleted_credential", &[]);
    let value_text = if is_masked {
        MASKED_VALUE
    } else if let Some(value) = revealed_value {
        value
    } else {
        match var.value() {
            EnvValueView::Plain { value, .. } => value.as_ref(),
            EnvValueView::Secret {
                credential_available,
                ..
            } if !credential_available => deleted_label.as_str(),
            EnvValueView::Secret { .. } => MASKED_VALUE,
        }
    };
    painter.with_clip_rect(columns[1]).text(
        egui::pos2(columns[1].left() + 2.0, y),
        egui::Align2::LEFT_CENTER,
        value_text,
        egui::FontId::monospace(14.0),
        ui.visuals().text_color(),
    );

    let dot_center = columns[2].center();
    let (dot_text, dot_color) = if is_masked {
        ("●", ui.visuals().hyperlink_color)
    } else {
        ("○", ui.visuals().weak_text_color())
    };
    painter.text(
        dot_center,
        egui::Align2::CENTER_CENTER,
        dot_text,
        egui::FontId::monospace(12.0),
        dot_color,
    );
    let dot_rect = egui::Rect::from_center_size(dot_center, egui::vec2(22.0, 18.0));
    let dot_response = ui.interact(
        dot_rect,
        ui.id().with(("env_dot", var.profile_id(), var.key())),
        egui::Sense::click(),
    );
    if dot_response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let mut hover = String::new();
    if var.value().is_secret() {
        hover.push_str(&catalog.t("env.secret_stored", &[]));
    }
    if var.value().has_os_override() {
        if !hover.is_empty() {
            hover.push('\n');
        }
        hover.push_str(&catalog.t("env.os_override", &[]));
    }
    if !hover.is_empty() {
        dot_response.clone().on_hover_text(hover);
    }
    if response.hovered() {
        painter.rect_stroke(
            dot_rect,
            0.0,
            egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
            egui::StrokeKind::Inside,
        );
    }

    let delete_rect = egui::Rect::from_center_size(columns[3].center(), egui::vec2(22.0, 18.0));
    let delete = ui
        .interact(
            delete_rect,
            ui.id()
                .with(("env_var_delete", var.profile_id(), var.key())),
            egui::Sense::click(),
        )
        .on_hover_text(catalog.t("action.delete", &[]));
    let hovered = delete.hovered();
    let stroke = if hovered {
        ui.visuals().error_fg_color
    } else if response.hovered() {
        ui.visuals().widgets.noninteractive.bg_stroke.color
    } else {
        egui::Color32::TRANSPARENT
    };
    let fill = if hovered {
        ui.visuals().error_fg_color
    } else {
        egui::Color32::TRANSPARENT
    };
    let text = if hovered {
        ui.visuals().window_fill
    } else {
        ui.visuals().weak_text_color()
    };
    painter.rect_filled(delete_rect, 0.0, fill);
    painter.rect_stroke(
        delete_rect,
        0.0,
        egui::Stroke::new(1.0, stroke),
        egui::StrokeKind::Inside,
    );
    painter.text(
        delete_rect.center(),
        egui::Align2::CENTER_CENTER,
        "×",
        egui::FontId::monospace(11.0),
        text,
    );
    super::hairline_row(ui, rect.top());
    super::hairline_row(ui, rect.bottom());
    EnvRowResponse {
        delete: delete.clicked(),
        toggle_reveal: dot_response.clicked(),
    }
}

fn env_empty_placeholder_row(ui: &mut egui::Ui, catalog: &i18n::Catalog) -> bool {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ENV_ROW_HEIGHT),
        egui::Sense::click(),
    );
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    if response.clicked() {
        ui.memory_mut(|memory| memory.request_focus(env_var_key_input_id()));
    }
    let columns = env_table_columns(rect);
    let y = rect.center().y;
    ui.painter().with_clip_rect(columns[0]).text(
        egui::pos2(columns[0].left() + 2.0, y),
        egui::Align2::LEFT_CENTER,
        "NEW_VAR",
        egui::FontId::monospace(14.0),
        ui.visuals().hyperlink_color,
    );
    ui.painter().with_clip_rect(columns[1]).text(
        egui::pos2(columns[1].left() + 2.0, y),
        egui::Align2::LEFT_CENTER,
        catalog.t("env.empty_value_placeholder", &[]),
        egui::FontId::monospace(14.0),
        ui.visuals().weak_text_color(),
    );
    ui.painter().text(
        columns[2].center(),
        egui::Align2::CENTER_CENTER,
        "○",
        egui::FontId::monospace(12.0),
        ui.visuals().weak_text_color(),
    );
    ui.painter().text(
        columns[3].center(),
        egui::Align2::CENTER_CENTER,
        "×",
        egui::FontId::monospace(11.0),
        ui.visuals().weak_text_color(),
    );
    super::hairline_row(ui, rect.top());
    super::hairline_row(ui, rect.bottom());
    response.clicked()
}

fn env_table_columns(rect: egui::Rect) -> [egui::Rect; 4] {
    const GAP: f32 = 4.0;
    const ACTION_WIDTH: f32 = 24.0;
    let flexible = ((rect.width() - ACTION_WIDTH * 2.0 - GAP * 3.0).max(0.0)) / 2.0;
    let key = egui::Rect::from_min_size(rect.min, egui::vec2(flexible, rect.height()));
    let value = egui::Rect::from_min_size(
        egui::pos2(key.right() + GAP, rect.top()),
        egui::vec2(flexible, rect.height()),
    );
    let mask = egui::Rect::from_min_size(
        egui::pos2(value.right() + GAP, rect.top()),
        egui::vec2(ACTION_WIDTH, rect.height()),
    );
    let delete = egui::Rect::from_min_size(
        egui::pos2(mask.right() + GAP, rect.top()),
        egui::vec2(ACTION_WIDTH, rect.height()),
    );
    [key, value, mask, delete]
}

fn compact_env_var_form(
    ui: &mut egui::Ui,
    state: &mut EnvProfilesUi,
    profile_id: &str,
    catalog: &i18n::Catalog,
) -> Option<EnvAction> {
    let mut written = None;
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.var_key)
                .hint_text(catalog.t("common.key", &[]))
                .id_source(env_var_key_input_id())
                .desired_width(180.0),
        );
        ui.add(
            egui::TextEdit::singleline(&mut state.var_plain_value)
                .hint_text(catalog.t("common.value", &[]))
                .desired_width(240.0),
        );
        // Bound same-frame paste input before validation or intent construction.
        truncate_utf8(&mut state.var_key, ENV_KEY_INPUT_MAX_BYTES);
        truncate_utf8(&mut state.var_plain_value, ENV_VALUE_INPUT_MAX_BYTES);
        let key = state.var_key.trim();
        let key_valid = !key.is_empty() && !key.contains('=') && !key.contains('\0');
        if ui
            .add_enabled(key_valid, egui::Button::new(catalog.t("env.add_var", &[])))
            .clicked()
        {
            let key = key.to_owned();
            let value = std::mem::take(&mut state.var_plain_value);
            let id = (profile_id.to_owned(), key.clone());
            state.remove_local_value(&id.0, &id.1);
            state.var_key.clear();
            written = Some(EnvAction::DotenvWrite {
                key,
                value: Some(value),
            });
        }
    });
    written
}

fn row_id(var: &EnvVarItem) -> (String, String) {
    (var.profile_id().to_owned(), var.key().to_owned())
}

fn env_var_key_input_id() -> egui::Id {
    egui::Id::new("env_var_key_input_compact")
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

fn clear_sensitive_string(value: &mut String) {
    // SAFETY: the caller holds an exclusive mutable borrow of this owned String.
    for byte in unsafe { value.as_mut_vec() } {
        // SAFETY: `byte` is exclusively borrowed from the owned allocation.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    value.clear();
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    struct FakeAdapter {
        calls: Cell<usize>,
    }

    impl FakeAdapter {
        fn snapshot(&self, count: usize) -> EnvProfilesSnapshot {
            self.calls.set(self.calls.get() + 1);
            let vars = (0..count)
                .map(|index| {
                    EnvVarItem::new(
                        "dotenv-profile",
                        format!("KEY_{index}"),
                        EnvValueView::plain(format!("value-{index}"), false),
                    )
                })
                .collect();
            EnvProfilesSnapshot::try_new(
                11,
                "workspace-1",
                true,
                Some("dotenv-profile"),
                0,
                vars,
                Vec::new(),
            )
            .unwrap()
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
        let mut state = EnvProfilesUi::new();
        for _ in 0..300 {
            let output = context.run_ui(egui::RawInput::default(), |ui| {
                assert!(state.contents_compact(ui, &snapshot, &catalog).is_none());
            });
            assert!(output.platform_output.commands.is_empty());
        }
        assert_eq!(adapter.calls.get(), calls_after_snapshot);
    }

    #[test]
    fn large_variable_list_is_bounded_and_virtualized() {
        let adapter = FakeAdapter {
            calls: Cell::new(0),
        };
        let snapshot = adapter.snapshot(ENV_SNAPSHOT_MAX_VARS);
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let context = egui::Context::default();
        let mut state = EnvProfilesUi::new();
        let output = context.run_ui(egui::RawInput::default(), |ui| {
            assert!(state.contents_compact(ui, &snapshot, &catalog).is_none());
        });
        assert!(output.shapes.len() < ENV_SNAPSHOT_MAX_VARS);

        let over = (0..=ENV_SNAPSHOT_MAX_VARS)
            .map(|index| EnvVarItem::new("p", format!("K{index}"), EnvValueView::plain("v", false)))
            .collect();
        assert!(matches!(
            EnvProfilesSnapshot::try_new(1, "w", true, Some("p"), 0, over, Vec::new()),
            Err(EnvSnapshotError::TooManyVariables)
        ));
    }

    #[test]
    fn revealed_values_are_bounded_and_not_in_snapshot() {
        let mut state = EnvProfilesUi::new();
        let oversized = "x".repeat(ENV_REVEALED_VALUE_MAX_BYTES + 1);
        assert!(matches!(
            RevealedEnvValue::new("p", "k", oversized),
            Err(EnvRevealError::ValueTooLarge)
        ));
        let snapshot = EnvProfilesSnapshot::try_new(
            1,
            "w",
            true,
            Some("p"),
            0,
            vec![EnvVarItem::new(
                "p",
                "SECRET_KEY",
                EnvValueView::secret("opaque-handle", true, false),
            )],
            Vec::new(),
        )
        .unwrap();
        state.sync_snapshot_state(&snapshot);
        state
            .accept_revealed(
                RevealedEnvValue::new("p", "SECRET_KEY", "plaintext".to_owned()).unwrap(),
            )
            .unwrap();
        assert_eq!(
            state
                .revealed
                .get(&("p".to_owned(), "SECRET_KEY".to_owned()))
                .map(SensitiveDisplay::expose),
            Some("plaintext")
        );
    }

    #[test]
    fn env_columns_place_two_flexible_and_two_action_columns() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(841.0, 30.0));
        let columns = env_table_columns(rect);
        assert_eq!(columns[0].width(), 390.5);
        assert_eq!(columns[1].width(), 390.5);
        assert_eq!(columns[2].width(), 24.0);
        assert_eq!(columns[3].width(), 24.0);
        assert_eq!(columns[3].right(), rect.right());
    }

    fn clash_warning_texts(output: &egui::FullOutput) -> Vec<String> {
        output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) => {
                    let text = text.galley.text();
                    text.contains("use of").then(|| text.to_owned())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn equal_keys_in_different_profiles_have_distinct_widget_ids() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let first = EnvVarItem::new(
            "profile-a",
            "OPENAI_API_KEY",
            EnvValueView::plain("x", false),
        );
        let second = EnvVarItem::new(
            "profile-b",
            "OPENAI_API_KEY",
            EnvValueView::plain("x", false),
        );
        let mut harness = egui_kittest::Harness::new_ui(|ui| {
            env_table_row(ui, &first, None, false, &catalog);
            env_table_row(ui, &second, None, false, &catalog);
        });
        harness
            .ctx
            .options_mut(|options| options.warn_on_id_clash = true);
        harness.step();
        assert!(clash_warning_texts(harness.output()).is_empty());
    }

    #[test]
    fn production_source_has_no_storage_keyring_picker_or_render_io_edge() {
        let source = include_str!("env_profiles.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for forbidden in [
            ["crate::", "storage"].concat(),
            ["crate::", "env::"].concat(),
            ["r", "fd::"].concat(),
            ["std::", "env::"].concat(),
            ["Secret", "String"].concat(),
            ["Keyring", "SecretStore"].concat(),
            ["Runtime", "Client"].concat(),
        ] {
            assert!(!source.contains(&forbidden));
        }
    }
}
