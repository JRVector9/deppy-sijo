use crate::settings_snapshot::SnapshotLoadState;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub const CREDENTIAL_SNAPSHOT_MAX_ITEMS: usize = 1_024;
pub const CREDENTIAL_SNAPSHOT_MAX_BYTES: usize = 4 * 1024 * 1024;
pub const CREDENTIAL_SENSITIVE_MAX_ITEMS: usize = 64;
pub const CREDENTIAL_SENSITIVE_ITEM_MAX_BYTES: usize = 32 * 1024;
pub const CREDENTIAL_SENSITIVE_MAX_BYTES: usize = 1024 * 1024;

const CREDENTIAL_ORPHAN_MAX_ITEMS: usize = 1_024;
const CREDENTIAL_ORPHAN_MAX_BYTES: usize = 1024 * 1024;
const CREDENTIAL_TEXT_INPUT_MAX_BYTES: usize = 4 * 1024;
const CREDENTIAL_ROW_HEIGHT: f32 = 30.0;
const CREDENTIAL_LIST_MAX_HEIGHT: f32 = 360.0;
const MASKED_SECRET: &str = "••••••••••••••••";

/// Immutable UI-only credential metadata. Secret plaintext is never part of a snapshot row.
/// Deliberately non-Clone so a render cannot accidentally duplicate an entire snapshot.
pub struct CredentialListItem {
    id: Arc<str>,
    provider: Arc<str>,
    label: Arc<str>,
    credential_kind: Arc<str>,
    masked_hint: Option<Arc<str>>,
}

impl CredentialListItem {
    pub fn new(
        id: impl Into<Arc<str>>,
        provider: impl Into<Arc<str>>,
        label: impl Into<Arc<str>>,
        credential_kind: impl Into<Arc<str>>,
        masked_hint: Option<impl Into<Arc<str>>>,
    ) -> Self {
        Self {
            id: id.into(),
            provider: provider.into(),
            label: label.into(),
            credential_kind: credential_kind.into(),
            masked_hint: masked_hint.map(Into::into),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn credential_kind(&self) -> &str {
        &self.credential_kind
    }

    fn retained_bytes(&self) -> usize {
        self.id.len()
            + self.provider.len()
            + self.label.len()
            + self.credential_kind.len()
            + self.masked_hint.as_ref().map_or(0, |hint| hint.len())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSnapshotError {
    TooManyItems,
    ByteBudgetExceeded,
}

impl std::fmt::Display for CredentialSnapshotError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::TooManyItems => "credential_snapshot_item_limit",
            Self::ByteBudgetExceeded => "credential_snapshot_byte_limit",
        })
    }
}

impl std::error::Error for CredentialSnapshotError {}

/// Bounded immutable render input. The composition root replaces it only when revision changes.
pub struct CredentialsSnapshot {
    revision: u64,
    state: SnapshotLoadState,
    items: Arc<[CredentialListItem]>,
}

impl CredentialsSnapshot {
    pub fn try_new(
        revision: u64,
        items: Vec<CredentialListItem>,
    ) -> Result<Self, CredentialSnapshotError> {
        if items.len() > CREDENTIAL_SNAPSHOT_MAX_ITEMS {
            return Err(CredentialSnapshotError::TooManyItems);
        }
        let retained_bytes = items
            .iter()
            .map(CredentialListItem::retained_bytes)
            .try_fold(0usize, usize::checked_add)
            .ok_or(CredentialSnapshotError::ByteBudgetExceeded)?;
        if retained_bytes > CREDENTIAL_SNAPSHOT_MAX_BYTES {
            return Err(CredentialSnapshotError::ByteBudgetExceeded);
        }
        Ok(Self {
            revision,
            state: SnapshotLoadState::Ready,
            items: items.into(),
        })
    }

    pub fn loading(revision: u64) -> Self {
        Self {
            state: SnapshotLoadState::Loading,
            ..Self::unavailable(revision)
        }
    }

    pub fn unavailable(revision: u64) -> Self {
        Self {
            revision,
            state: SnapshotLoadState::Failed,
            items: Arc::from([]),
        }
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub const fn is_available(&self) -> bool {
        matches!(self.state, SnapshotLoadState::Ready)
    }

    pub fn items(&self) -> &[CredentialListItem] {
        &self.items
    }
}

/// Secret-bearing input crossing from UI to the composition root. It cannot be cloned or
/// serialized, Debug is always redacted, and its allocation is overwritten on drop.
pub struct SensitiveInput(String);

impl SensitiveInput {
    pub fn try_new(mut value: String) -> Result<Self, CredentialSensitiveError> {
        if value.len() > CREDENTIAL_SENSITIVE_ITEM_MAX_BYTES {
            clear_sensitive_string(&mut value);
            return Err(CredentialSensitiveError::ItemTooLarge);
        }
        Ok(Self(value))
    }

    pub fn into_inner(mut self) -> String {
        std::mem::take(&mut self.0)
    }
}

impl std::fmt::Debug for SensitiveInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SensitiveInput([REDACTED])")
    }
}

impl Drop for SensitiveInput {
    fn drop(&mut self) {
        clear_sensitive_string(&mut self.0);
    }
}

pub struct NewCredential {
    provider: String,
    label: String,
    credential_kind: String,
    secret: SensitiveInput,
}

impl NewCredential {
    pub fn into_parts(self) -> (String, String, String, SensitiveInput) {
        (self.provider, self.label, self.credential_kind, self.secret)
    }
}

/// Plaintext reveal delivery from a root-owned worker. It has the same ownership rules as input.
pub struct RevealedCredential {
    credential_id: String,
    value: SensitiveDisplay,
}

impl RevealedCredential {
    pub fn new(
        credential_id: impl Into<String>,
        mut value: String,
    ) -> Result<Self, CredentialSensitiveError> {
        if value.len() > CREDENTIAL_SENSITIVE_ITEM_MAX_BYTES {
            clear_sensitive_string(&mut value);
            return Err(CredentialSensitiveError::ItemTooLarge);
        }
        Ok(Self {
            credential_id: credential_id.into(),
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
        clear_sensitive_string(&mut self.0);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSensitiveError {
    ItemTooLarge,
    CorpusFull,
    OrphanListTooLarge,
}

impl std::fmt::Display for CredentialSensitiveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ItemTooLarge => "credential_sensitive_item_limit",
            Self::CorpusFull => "credential_sensitive_corpus_limit",
            Self::OrphanListTooLarge => "credential_orphan_list_limit",
        })
    }
}

impl std::error::Error for CredentialSensitiveError {}

/// One render emits at most one intent. Secret variants deliberately have no Clone/Serialize.
pub enum CredentialsIntent {
    Add {
        revision: u64,
        credential: NewCredential,
    },
    Delete {
        revision: u64,
        credential_id: String,
    },
    Reveal {
        revision: u64,
        credential_id: String,
    },
    ScanOrphans {
        revision: u64,
    },
    PurgeOrphans {
        revision: u64,
        credential_ids: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialsUiErrorCode {
    SnapshotUnavailable,
    DraftLimitExceeded,
    AddFailed,
    DeleteFailed,
    RevealFailed,
    RevealCapacityExceeded,
    OrphanScanFailed,
    OrphanPurgeFailed,
}

impl CredentialsUiErrorCode {
    const fn message(self) -> &'static str {
        match self {
            Self::SnapshotUnavailable => "Credential 목록을 불러오지 못했습니다.",
            Self::DraftLimitExceeded => "입력 크기 상한을 초과했습니다.",
            Self::AddFailed => "Credential 저장에 실패했습니다.",
            Self::DeleteFailed => "Credential 삭제에 실패했습니다.",
            Self::RevealFailed => "Secret 값을 불러오지 못했습니다.",
            Self::RevealCapacityExceeded => "동시에 표시할 수 있는 secret 상한을 초과했습니다.",
            Self::OrphanScanFailed => "고아 credential 검색에 실패했습니다.",
            Self::OrphanPurgeFailed => "고아 credential 정리에 실패했습니다.",
        }
    }
}

enum OrphanStatus {
    NoneFound,
    Purged { purged: usize, remaining: usize },
}

/// Credential settings draft and bounded reveal state. All external work belongs to App.
pub struct CredentialsUi {
    provider: String,
    label: String,
    kind: &'static str,
    secret_input: String,
    secret_input_overflowed: bool,
    error: Option<CredentialsUiErrorCode>,
    show_add_form: bool,
    delete_confirm: Option<(String, String)>,
    orphan_candidates: Option<Arc<[Arc<str>]>>,
    orphan_status: Option<OrphanStatus>,
    orphan_scan_pending: bool,
    orphan_purge_pending: bool,
    add_pending: bool,
    delete_pending: HashSet<String>,
    reveal_pending: HashSet<String>,
    revealed: HashMap<String, SensitiveDisplay>,
    revealed_bytes: usize,
    snapshot_revision: Option<u64>,
}

impl CredentialsUi {
    pub fn new() -> Self {
        Self {
            provider: String::new(),
            label: String::new(),
            kind: "api_key",
            secret_input: String::new(),
            secret_input_overflowed: false,
            error: None,
            show_add_form: false,
            delete_confirm: None,
            orphan_candidates: None,
            orphan_status: None,
            orphan_scan_pending: false,
            orphan_purge_pending: false,
            add_pending: false,
            delete_pending: HashSet::new(),
            reveal_pending: HashSet::new(),
            revealed: HashMap::new(),
            revealed_bytes: 0,
            snapshot_revision: None,
        }
    }

    pub fn invalidate_cache(&mut self) {
        self.snapshot_revision = None;
        self.clear_revealed_secrets();
    }

    pub fn clear_revealed_secrets(&mut self) {
        self.reveal_pending.clear();
        self.revealed.clear();
        self.revealed_bytes = 0;
    }

    pub fn add_succeeded(&mut self) {
        self.provider.clear();
        self.label.clear();
        self.add_pending = false;
        self.error = None;
    }

    pub fn delete_succeeded(&mut self, credential_id: &str) {
        self.delete_pending.remove(credential_id);
        self.remove_revealed(credential_id);
        self.error = None;
    }

    pub fn report_error(&mut self, code: CredentialsUiErrorCode) {
        match code {
            CredentialsUiErrorCode::AddFailed => self.add_pending = false,
            CredentialsUiErrorCode::DeleteFailed => self.delete_pending.clear(),
            CredentialsUiErrorCode::OrphanScanFailed => self.orphan_scan_pending = false,
            CredentialsUiErrorCode::OrphanPurgeFailed => self.orphan_purge_pending = false,
            _ => {}
        }
        self.error = Some(code);
    }

    pub fn accept_revealed(
        &mut self,
        revealed: RevealedCredential,
    ) -> Result<(), CredentialSensitiveError> {
        let RevealedCredential {
            credential_id,
            value,
        } = revealed;
        let previous = self
            .revealed
            .get(&credential_id)
            .map_or(0, SensitiveDisplay::len);
        let next_bytes = self
            .revealed_bytes
            .saturating_sub(previous)
            .saturating_add(value.len());
        if (self.revealed.len() >= CREDENTIAL_SENSITIVE_MAX_ITEMS
            && !self.revealed.contains_key(&credential_id))
            || next_bytes > CREDENTIAL_SENSITIVE_MAX_BYTES
        {
            self.reveal_pending.remove(&credential_id);
            self.error = Some(CredentialsUiErrorCode::RevealCapacityExceeded);
            return Err(CredentialSensitiveError::CorpusFull);
        }
        self.revealed_bytes = next_bytes;
        self.revealed.insert(credential_id.clone(), value);
        self.reveal_pending.remove(&credential_id);
        self.error = None;
        Ok(())
    }

    pub fn reject_reveal(&mut self, credential_id: &str) {
        self.reveal_pending.remove(credential_id);
        self.error = Some(CredentialsUiErrorCode::RevealFailed);
    }

    pub fn accept_orphan_scan(
        &mut self,
        credential_ids: Vec<String>,
    ) -> Result<(), CredentialSensitiveError> {
        let bytes = credential_ids
            .iter()
            .map(String::len)
            .try_fold(0usize, usize::checked_add)
            .ok_or(CredentialSensitiveError::OrphanListTooLarge)?;
        if credential_ids.len() > CREDENTIAL_ORPHAN_MAX_ITEMS || bytes > CREDENTIAL_ORPHAN_MAX_BYTES
        {
            self.orphan_scan_pending = false;
            self.error = Some(CredentialsUiErrorCode::OrphanScanFailed);
            return Err(CredentialSensitiveError::OrphanListTooLarge);
        }
        self.orphan_scan_pending = false;
        self.orphan_status = credential_ids.is_empty().then_some(OrphanStatus::NoneFound);
        self.orphan_candidates = (!credential_ids.is_empty()).then(|| {
            credential_ids
                .into_iter()
                .map(Arc::<str>::from)
                .collect::<Vec<_>>()
                .into()
        });
        self.error = None;
        Ok(())
    }

    pub fn orphan_purge_succeeded(&mut self, purged: usize, remaining: usize) {
        self.orphan_candidates = None;
        self.orphan_purge_pending = false;
        self.orphan_status = Some(OrphanStatus::Purged { purged, remaining });
        self.error = None;
    }

    /// Pure render path: immutable snapshot in, at most one intent out.
    pub fn contents_compact(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &CredentialsSnapshot,
        catalog: &i18n::Catalog,
    ) -> Option<CredentialsIntent> {
        self.sync_snapshot(snapshot);
        let mut intent = None;
        if !snapshot.is_available() {
            if snapshot.state == SnapshotLoadState::Loading {
                ui.spinner();
            }
            if let Some(error) = self.error {
                ui.colored_label(ui.visuals().error_fg_color, error.message());
            }
            return None;
        }

        ui.add_space(2.0);
        let add_label = format!("+ {}", catalog.t("action.add", &[]));
        if super::section_header(
            ui,
            &catalog.t("credentials.api_keys", &[]),
            Some(snapshot.items().len()),
            Some(&add_label),
        ) {
            self.show_add_form = !self.show_add_form;
            if self.show_add_form {
                ui.memory_mut(|memory| memory.request_focus(credential_provider_input_id()));
            }
        }
        credentials_table_header(
            ui,
            &[
                catalog.t("credentials.provider", &[]),
                catalog.t("credentials.label", &[]),
                catalog.t("credentials.kind", &[]),
                catalog.t("credentials.secret", &[]),
            ],
        );
        self.render_rows(ui, snapshot, catalog, &mut intent);
        self.render_delete_confirmation(ui.ctx(), snapshot.revision(), catalog, &mut intent);
        if self.show_add_form {
            self.render_add_form(ui, snapshot, catalog, &mut intent);
        }
        self.render_orphan_controls(ui, snapshot, catalog, &mut intent);
        if let Some(error) = self.error {
            ui.colored_label(ui.visuals().error_fg_color, error.message());
        }
        intent
    }

    fn sync_snapshot(&mut self, snapshot: &CredentialsSnapshot) {
        snapshot
            .state
            .reconcile_error(&mut self.error, CredentialsUiErrorCode::SnapshotUnavailable);
        if self.snapshot_revision == Some(snapshot.revision()) {
            return;
        }
        self.snapshot_revision = Some(snapshot.revision());
        let live = snapshot
            .items()
            .iter()
            .map(CredentialListItem::id)
            .collect::<HashSet<_>>();
        self.reveal_pending.retain(|id| live.contains(id.as_str()));
        self.delete_pending.retain(|id| live.contains(id.as_str()));
        let removed = self
            .revealed
            .extract_if(|id, _| !live.contains(id.as_str()))
            .map(|(_, value)| value.len())
            .sum::<usize>();
        self.revealed_bytes = self.revealed_bytes.saturating_sub(removed);
        if self
            .delete_confirm
            .as_ref()
            .is_some_and(|(id, _)| !live.contains(id.as_str()))
        {
            self.delete_confirm = None;
        }
    }

    fn render_rows(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &CredentialsSnapshot,
        catalog: &i18n::Catalog,
        intent: &mut Option<CredentialsIntent>,
    ) {
        egui::ScrollArea::vertical()
            .id_salt("credentials_bounded_rows")
            .max_height(CREDENTIAL_LIST_MAX_HEIGHT)
            .show_rows(
                ui,
                CREDENTIAL_ROW_HEIGHT,
                snapshot.items().len(),
                |ui, range| {
                    for meta in &snapshot.items()[range] {
                        let row = credential_table_row(
                            ui,
                            meta,
                            self.revealed.get(meta.id()).map(SensitiveDisplay::expose),
                            self.reveal_pending.contains(meta.id()),
                            catalog,
                        );
                        if row.delete {
                            let name = if meta.label().is_empty() {
                                meta.provider()
                            } else {
                                meta.label()
                            };
                            self.delete_confirm = Some((meta.id().to_owned(), name.to_owned()));
                        } else if row.toggle_reveal {
                            if self.revealed.contains_key(meta.id()) {
                                self.remove_revealed(meta.id());
                            } else if !self.reveal_pending.contains(meta.id()) && intent.is_none() {
                                if self.revealed.len() >= CREDENTIAL_SENSITIVE_MAX_ITEMS {
                                    self.error =
                                        Some(CredentialsUiErrorCode::RevealCapacityExceeded);
                                } else {
                                    self.reveal_pending.insert(meta.id().to_owned());
                                    *intent = Some(CredentialsIntent::Reveal {
                                        revision: snapshot.revision(),
                                        credential_id: meta.id().to_owned(),
                                    });
                                }
                            }
                        }
                    }
                },
            );
        if snapshot.items().is_empty() {
            ui.label(
                egui::RichText::new(catalog.t("credentials.empty", &[]))
                    .size(13.0)
                    .weak(),
            );
        }
    }

    fn render_delete_confirmation(
        &mut self,
        ctx: &egui::Context,
        revision: u64,
        catalog: &i18n::Catalog,
        intent: &mut Option<CredentialsIntent>,
    ) {
        let Some((_, name)) = self.delete_confirm.as_ref() else {
            return;
        };
        let mut decision = None;
        egui::Window::new(catalog.t("credentials.delete_confirm.title", &[]))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(catalog.t("credentials.delete_confirm.body", &[("name", name)]));
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
                let (credential_id, _) = self.delete_confirm.take().expect("pending checked");
                self.delete_pending.insert(credential_id.clone());
                *intent = Some(CredentialsIntent::Delete {
                    revision,
                    credential_id,
                });
            }
            Some(false) => self.delete_confirm = None,
            _ => {}
        }
    }

    fn render_add_form(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &CredentialsSnapshot,
        catalog: &i18n::Catalog,
        intent: &mut Option<CredentialsIntent>,
    ) {
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.provider)
                    .id_source(credential_provider_input_id())
                    .hint_text(catalog.t("credentials.provider", &[]))
                    .desired_width(120.0),
            );
            ui.add(
                egui::TextEdit::singleline(&mut self.label)
                    .hint_text(catalog.t("credentials.label", &[]))
                    .desired_width(160.0),
            );
            for kind in ["api_key", "token"] {
                ui.selectable_value(&mut self.kind, kind, kind);
            }
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.secret_input)
                    .password(true)
                    .hint_text(catalog.t("credentials.secret", &[]))
                    .desired_width(220.0),
            );
            truncate_utf8(&mut self.provider, CREDENTIAL_TEXT_INPUT_MAX_BYTES);
            truncate_utf8(&mut self.label, CREDENTIAL_TEXT_INPUT_MAX_BYTES);
            if self.secret_input.len() > CREDENTIAL_SENSITIVE_ITEM_MAX_BYTES {
                clear_sensitive_string(&mut self.secret_input);
                self.secret_input_overflowed = true;
                self.error = Some(CredentialsUiErrorCode::DraftLimitExceeded);
            } else if response.changed() {
                self.secret_input_overflowed = false;
            }
            let filled = !self.provider.trim().is_empty()
                && !self.label.trim().is_empty()
                && !self.secret_input.is_empty()
                && !self.secret_input_overflowed
                && !self.add_pending
                && snapshot.is_available();
            if ui
                .add_enabled(filled, egui::Button::new(catalog.t("action.add", &[])))
                .clicked()
                && intent.is_none()
            {
                let secret = std::mem::take(&mut self.secret_input);
                match SensitiveInput::try_new(secret) {
                    Ok(secret) => {
                        self.add_pending = true;
                        self.error = None;
                        *intent = Some(CredentialsIntent::Add {
                            revision: snapshot.revision(),
                            credential: NewCredential {
                                provider: self.provider.trim().to_owned(),
                                label: self.label.trim().to_owned(),
                                credential_kind: self.kind.to_owned(),
                                secret,
                            },
                        });
                    }
                    Err(_) => {
                        self.error = Some(CredentialsUiErrorCode::DraftLimitExceeded);
                    }
                }
            }
        });
    }

    fn render_orphan_controls(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &CredentialsSnapshot,
        catalog: &i18n::Catalog,
        intent: &mut Option<CredentialsIntent>,
    ) {
        if !self.show_add_form && self.orphan_candidates.is_none() && self.orphan_status.is_none() {
            return;
        }
        ui.add_space(12.0);
        if let Some(candidates) = self.orphan_candidates.as_ref() {
            let mut purge = false;
            let mut cancel = false;
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(catalog.t(
                        "credentials.purge_found",
                        &[("count", &candidates.len().to_string())],
                    ))
                    .size(12.0)
                    .color(ui.visuals().warn_fg_color),
                );
                purge = ui
                    .add_enabled(
                        !self.orphan_purge_pending,
                        egui::Button::new(catalog.t("credentials.purge_go", &[])).small(),
                    )
                    .clicked();
                cancel = ui.small_button(catalog.t("action.cancel", &[])).clicked();
            });
            if purge && intent.is_none() {
                let credential_ids = candidates.iter().map(ToString::to_string).collect();
                self.orphan_purge_pending = true;
                *intent = Some(CredentialsIntent::PurgeOrphans {
                    revision: snapshot.revision(),
                    credential_ids,
                });
            } else if cancel {
                self.orphan_candidates = None;
            }
        } else {
            ui.horizontal(|ui| {
                let link = ui.add_enabled(
                    !self.orphan_scan_pending,
                    egui::Label::new(
                        egui::RichText::new(catalog.t("credentials.purge_orphans", &[]))
                            .size(12.0)
                            .color(ui.visuals().weak_text_color()),
                    )
                    .sense(egui::Sense::click()),
                );
                if link.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if link.clicked() && intent.is_none() {
                    self.orphan_scan_pending = true;
                    *intent = Some(CredentialsIntent::ScanOrphans {
                        revision: snapshot.revision(),
                    });
                }
                if let Some(status) = &self.orphan_status {
                    match status {
                        OrphanStatus::NoneFound => {
                            ui.label(
                                egui::RichText::new(catalog.t("credentials.purge_none", &[]))
                                    .size(12.0)
                                    .weak(),
                            );
                        }
                        OrphanStatus::Purged { purged, remaining } => {
                            let done = catalog
                                .t("credentials.purge_done", &[("count", &purged.to_string())]);
                            let tail = catalog.t(
                                "credentials.purge_remaining",
                                &[("count", &remaining.to_string())],
                            );
                            ui.label(
                                egui::RichText::new(format!("{done} · {tail}"))
                                    .size(12.0)
                                    .weak(),
                            );
                        }
                    }
                }
            });
        }
    }

    fn remove_revealed(&mut self, credential_id: &str) {
        self.reveal_pending.remove(credential_id);
        if let Some(value) = self.revealed.remove(credential_id) {
            self.revealed_bytes = self.revealed_bytes.saturating_sub(value.len());
        }
    }
}

impl Default for CredentialsUi {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CredentialsUi {
    fn drop(&mut self) {
        clear_sensitive_string(&mut self.secret_input);
    }
}

fn credential_provider_input_id() -> egui::Id {
    egui::Id::new("credentials_provider_input")
}

fn credentials_table_header(ui: &mut egui::Ui, columns: &[String]) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 24.0), egui::Sense::hover());
    let column_rects = credential_columns(rect);
    let painter = ui.painter();
    let color = ui.visuals().weak_text_color();
    let font = egui::FontId::monospace(12.0);
    for (index, column) in columns.iter().enumerate() {
        painter.text(
            egui::pos2(column_rects[index].left(), rect.center().y),
            egui::Align2::LEFT_CENTER,
            column,
            font.clone(),
            color,
        );
    }
    super::hairline_row(ui, rect.bottom());
}

struct CredentialRowResponse {
    delete: bool,
    toggle_reveal: bool,
}

fn credential_table_row(
    ui: &mut egui::Ui,
    meta: &CredentialListItem,
    revealed_secret: Option<&str>,
    reveal_pending: bool,
    catalog: &i18n::Catalog,
) -> CredentialRowResponse {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), CREDENTIAL_ROW_HEIGHT),
        egui::Sense::hover(),
    );
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let columns = credential_columns(rect);
    let painter = ui.painter();
    let y = rect.center().y;
    for (column, text, color) in [
        (0, meta.provider(), credential_secondary_text(ui)),
        (1, meta.label(), ui.visuals().text_color()),
    ] {
        painter.with_clip_rect(columns[column]).text(
            egui::pos2(columns[column].left() + 2.0, y),
            egui::Align2::LEFT_CENTER,
            text,
            egui::FontId::monospace(14.0),
            color,
        );
    }
    if let Some(width) = credential_kind_badge_width(meta.credential_kind(), columns[2].width()) {
        let badge = egui::Rect::from_min_size(
            egui::pos2(columns[2].left() + 4.0, y - 12.0),
            egui::vec2(width, 24.0),
        );
        let filled = meta.credential_kind() == "token";
        if filled {
            painter.rect_filled(badge, 0.0, ui.visuals().selection.bg_fill);
        } else {
            painter.rect_stroke(
                badge,
                0.0,
                egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
                egui::StrokeKind::Inside,
            );
        }
        painter.with_clip_rect(columns[2]).text(
            egui::pos2(badge.left() + 5.0, badge.center().y),
            egui::Align2::LEFT_CENTER,
            meta.credential_kind(),
            egui::FontId::monospace(12.0),
            if filled {
                egui::Color32::WHITE
            } else {
                ui.visuals().weak_text_color()
            },
        );
    }

    let reveal_center = egui::pos2(columns[3].right() - 9.0, y);
    let reveal_rect = egui::Rect::from_center_size(reveal_center, egui::vec2(18.0, 16.0));
    let secret_clip = egui::Rect::from_min_max(
        columns[3].min,
        egui::pos2(
            (reveal_rect.left() - 2.0).max(columns[3].left()),
            columns[3].bottom(),
        ),
    );
    painter.with_clip_rect(secret_clip).text(
        egui::pos2(columns[3].left() + 2.0, y),
        egui::Align2::LEFT_CENTER,
        revealed_secret.unwrap_or(MASKED_SECRET),
        egui::FontId::monospace(14.0),
        ui.visuals().text_color(),
    );
    let reveal = ui
        .interact(
            reveal_rect,
            ui.id().with(("credential_reveal", meta.id())),
            egui::Sense::click(),
        )
        .on_hover_text(if revealed_secret.is_some() {
            catalog.t("action.hide_secret", &[])
        } else {
            catalog.t("action.show_secret", &[])
        });
    if response.hovered() {
        painter.rect_stroke(
            reveal_rect,
            0.0,
            egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
            egui::StrokeKind::Inside,
        );
    }
    painter.text(
        reveal_center,
        egui::Align2::CENTER_CENTER,
        if reveal_pending {
            "…"
        } else if revealed_secret.is_some() {
            "●"
        } else {
            "○"
        },
        egui::FontId::monospace(12.0),
        if revealed_secret.is_some() {
            ui.visuals().hyperlink_color
        } else {
            ui.visuals().weak_text_color()
        },
    );

    let delete_rect = egui::Rect::from_center_size(columns[4].center(), egui::vec2(22.0, 18.0));
    let delete = ui
        .interact(
            delete_rect,
            ui.id().with(("credential_delete", meta.id())),
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
    painter.rect_filled(
        delete_rect,
        0.0,
        if hovered {
            ui.visuals().error_fg_color
        } else {
            egui::Color32::TRANSPARENT
        },
    );
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
        egui::FontId::monospace(12.0),
        if hovered {
            ui.visuals().window_fill
        } else {
            ui.visuals().weak_text_color()
        },
    );
    super::hairline_row(ui, rect.top());
    super::hairline_row(ui, rect.bottom());
    CredentialRowResponse {
        delete: delete.clicked(),
        toggle_reveal: reveal.clicked(),
    }
}

fn credential_columns(rect: egui::Rect) -> [egui::Rect; 5] {
    const GAP: f32 = 4.0;
    const PROVIDER_WIDTH: f32 = 80.0;
    const KIND_WIDTH: f32 = 56.0;
    const ACTION_WIDTH: f32 = 24.0;
    let flexible =
        ((rect.width() - PROVIDER_WIDTH - KIND_WIDTH - ACTION_WIDTH - GAP * 4.0).max(0.0)) / 2.0;
    let provider = egui::Rect::from_min_size(rect.min, egui::vec2(PROVIDER_WIDTH, rect.height()));
    let label = egui::Rect::from_min_size(
        egui::pos2(provider.right() + GAP, rect.top()),
        egui::vec2(flexible, rect.height()),
    );
    let kind = egui::Rect::from_min_size(
        egui::pos2(label.right() + GAP, rect.top()),
        egui::vec2(KIND_WIDTH, rect.height()),
    );
    let secret = egui::Rect::from_min_size(
        egui::pos2(kind.right() + GAP, rect.top()),
        egui::vec2(flexible, rect.height()),
    );
    let delete = egui::Rect::from_min_size(
        egui::pos2(secret.right() + GAP, rect.top()),
        egui::vec2(ACTION_WIDTH, rect.height()),
    );
    [provider, label, kind, secret, delete]
}

fn credential_secondary_text(ui: &egui::Ui) -> egui::Color32 {
    // #aaaaaa/#444444는 settings.rs 보조색의 색상축 통일 이전 값이 그대로 남아있던
    // 사본이었다(2026-08-06) — 같은 설정 창 안에서 렌더되므로 settings.rs 헬퍼로 위임한다.
    super::settings::settings_text_secondary(ui)
}

fn credential_kind_badge_width(kind: &str, column_width: f32) -> Option<f32> {
    if !column_width.is_finite() || column_width <= 14.0 {
        return None;
    }
    let desired = kind.len() as f32 * 8.0 + 14.0;
    let max = (column_width - 8.0).max(14.0);
    Some(desired.min(max))
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
    // SAFETY: caller exclusively owns this String and only overwrites initialized bytes.
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

    struct FakePort {
        calls: Cell<usize>,
    }

    impl FakePort {
        fn snapshot(&self, count: usize) -> CredentialsSnapshot {
            self.calls.set(self.calls.get() + 1);
            let items = (0..count)
                .map(|index| {
                    CredentialListItem::new(
                        format!("id-{index}"),
                        "provider",
                        format!("credential-{index}"),
                        "api_key",
                        None::<String>,
                    )
                })
                .collect();
            CredentialsSnapshot::try_new(9, items).unwrap()
        }
    }

    #[test]
    fn settings_load_로딩과_빈_정상목록은_오류가_아니다() {
        let mut view = CredentialsUi::new();
        view.report_error(CredentialsUiErrorCode::SnapshotUnavailable);
        let loading = CredentialsSnapshot::loading(8);
        view.sync_snapshot(&loading);
        assert!(!loading.is_available());
        assert_eq!(view.error, None);
        let ready = CredentialsSnapshot::try_new(8, Vec::new()).unwrap();
        view.sync_snapshot(&ready);
        assert!(ready.is_available());
        assert_eq!(view.error, None);
    }

    #[test]
    fn settings_load_실제실패는_오류를_남기고_재조회성공으로_복구한다() {
        let mut view = CredentialsUi::new();
        let failed = CredentialsSnapshot::unavailable(8);
        view.sync_snapshot(&failed);
        assert_eq!(
            view.error,
            Some(CredentialsUiErrorCode::SnapshotUnavailable)
        );
        let ready = CredentialsSnapshot::try_new(8, Vec::new()).unwrap();
        view.sync_snapshot(&ready);
        assert_eq!(view.error, None);
    }

    #[test]
    fn settings_load_조회상태변경은_별도_작업오류를_지우지_않는다() {
        let mut view = CredentialsUi::new();
        view.report_error(CredentialsUiErrorCode::DeleteFailed);
        for snapshot in [
            CredentialsSnapshot::loading(8),
            CredentialsSnapshot::unavailable(8),
            CredentialsSnapshot::try_new(8, Vec::new()).unwrap(),
        ] {
            view.sync_snapshot(&snapshot);
            assert_eq!(view.error, Some(CredentialsUiErrorCode::DeleteFailed));
        }
    }

    #[test]
    fn settings_load_성공하면_이전_조회_오류를_지운다() {
        let mut view = CredentialsUi::new();
        let ready = CredentialsSnapshot::try_new(7, Vec::new()).unwrap();
        view.sync_snapshot(&ready);
        view.report_error(CredentialsUiErrorCode::SnapshotUnavailable);
        // 같은 revision에서도 정상 결과를 반영하면 오래된 조회 오류가 남지 않는다.
        view.sync_snapshot(&ready);
        assert_eq!(view.error, None);
    }

    #[test]
    fn unchanged_snapshot_renders_300_frames_without_port_calls() {
        let port = FakePort {
            calls: Cell::new(0),
        };
        let snapshot = port.snapshot(32);
        let calls = port.calls.get();
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let context = egui::Context::default();
        let mut state = CredentialsUi::new();
        for _ in 0..300 {
            let mut output = context.run_ui(egui::RawInput::default(), |ui| {
                assert!(state.contents_compact(ui, &snapshot, &catalog).is_none());
            });
            output.textures_delta.clear();
            assert!(output.platform_output.commands.is_empty());
        }
        assert_eq!(port.calls.get(), calls);
    }

    #[test]
    fn large_list_is_bounded_and_virtualized() {
        let port = FakePort {
            calls: Cell::new(0),
        };
        let snapshot = port.snapshot(CREDENTIAL_SNAPSHOT_MAX_ITEMS);
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let context = egui::Context::default();
        let mut state = CredentialsUi::new();
        let mut output = context.run_ui(egui::RawInput::default(), |ui| {
            assert!(state.contents_compact(ui, &snapshot, &catalog).is_none());
        });
        output.textures_delta.clear();
        assert!(output.shapes.len() < CREDENTIAL_SNAPSHOT_MAX_ITEMS);

        let over = (0..=CREDENTIAL_SNAPSHOT_MAX_ITEMS)
            .map(|index| {
                CredentialListItem::new(format!("id-{index}"), "p", "l", "api_key", None::<String>)
            })
            .collect();
        assert!(matches!(
            CredentialsSnapshot::try_new(1, over),
            Err(CredentialSnapshotError::TooManyItems)
        ));
    }

    #[test]
    fn sensitive_input_and_reveal_are_redacted_and_bounded() {
        let sensitive = SensitiveInput::try_new("super-secret".to_owned()).unwrap();
        assert_eq!(format!("{sensitive:?}"), "SensitiveInput([REDACTED])");
        assert!(!format!("{sensitive:?}").contains("super-secret"));
        assert!(SensitiveInput::try_new("x".repeat(CREDENTIAL_SENSITIVE_ITEM_MAX_BYTES)).is_ok());
        assert!(matches!(
            SensitiveInput::try_new("x".repeat(CREDENTIAL_SENSITIVE_ITEM_MAX_BYTES + 1)),
            Err(CredentialSensitiveError::ItemTooLarge)
        ));

        let mut state = CredentialsUi::new();
        for index in 0..CREDENTIAL_SENSITIVE_MAX_ITEMS {
            state
                .accept_revealed(
                    RevealedCredential::new(format!("id-{index}"), "x".to_owned()).unwrap(),
                )
                .unwrap();
        }
        assert!(matches!(
            state.accept_revealed(RevealedCredential::new("overflow", "x".to_owned()).unwrap()),
            Err(CredentialSensitiveError::CorpusFull)
        ));
    }

    #[test]
    fn credential_columns_keep_reference_grid_width() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(841.0, 30.0));
        let columns = credential_columns(rect);
        assert_eq!(columns[0].width(), 80.0);
        assert_eq!(columns[1].width(), 332.5);
        assert_eq!(columns[2].width(), 56.0);
        assert_eq!(columns[3].width(), 332.5);
        assert_eq!(columns[4].width(), 24.0);
        assert_eq!(columns[4].right(), rect.right());
    }

    #[test]
    fn production_source_has_no_service_storage_or_secret_edge() {
        let source = include_str!("credentials.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for forbidden in [
            ["Credential", "Service"].concat(),
            ["crate::", "storage"].concat(),
            ["secret::", "SecretString"].concat(),
            ["Keyring", "SecretStore"].concat(),
            ["std::", "fs"].concat(),
            ["r", "fd::"].concat(),
        ] {
            assert!(!source.contains(&forbidden), "forbidden edge: {forbidden}");
        }
        assert!(source.contains("show_rows"));
    }

    #[test]
    fn badge_width_is_safe_for_narrow_columns() {
        assert_eq!(credential_kind_badge_width("api_key", -8.0), None);
        assert_eq!(credential_kind_badge_width("api_key", f32::NAN), None);
        assert_eq!(credential_kind_badge_width("api_key", 22.0), Some(14.0));
    }
}
