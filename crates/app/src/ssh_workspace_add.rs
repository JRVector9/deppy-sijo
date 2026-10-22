//! Data-driven SSH workspace addition. Rendering performs no filesystem or network I/O.
use deppy_core::workspace_target::{RemotePath, SshConnectionProfile, SshWorkspaceTarget};
use deppy_remote_host::{
    files::{DirectoryCursor, DirectoryPage},
    services::Agent,
};
use ssh_transport::{AuthPrompt, PromptKind, PromptReply};
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
const DIRECTORY_LIMIT: usize = 256;
const DIRECTORY_BYTES_MAX: usize = 128 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshWorkspaceDraft {
    pub profile_id: String,
    pub destination: String,
    pub port: String,
    pub use_identity_file: bool,
    pub identity_file: String,
    pub name: String,
}
impl Default for SshWorkspaceDraft {
    fn default() -> Self {
        Self {
            profile_id: uuid::Uuid::new_v4().to_string(),
            destination: String::new(),
            port: String::new(),
            use_identity_file: false,
            identity_file: String::new(),
            name: String::new(),
        }
    }
}

pub enum SshWorkspaceIntent {
    Connect {
        generation: u64,
        profile: SshConnectionProfile,
    },
    PromptReply {
        generation: u64,
        request_id: u64,
        reply: PromptReply,
    },
    InstallHelper {
        generation: u64,
    },
    Browse {
        generation: u64,
        request_id: u64,
        path: RemotePath,
        cursor: Option<DirectoryCursor>,
    },
    VerifyRoot {
        generation: u64,
        request_id: u64,
        path: RemotePath,
    },
    Add {
        generation: u64,
        name: String,
        profile: SshConnectionProfile,
        target: SshWorkspaceTarget,
    },
    Cancel {
        generation: u64,
    },
    Close {
        generation: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SshWorkspaceError {
    InvalidAddress,
    InvalidPort,
    InvalidIdentityPath,
    ConnectionFailed,
    AuthenticationFailed,
    HostKeyChanged,
    PermissionDenied,
    FolderMissing,
    UnsupportedServer,
    HelperFailed,
    RequestFailed,
    WorkerUnavailable,
    SessionExpired,
}
impl SshWorkspaceError {
    fn message_key(self) -> &'static str {
        match self {
            Self::InvalidAddress => "workspace.ssh.invalid_address",
            Self::InvalidPort => "workspace.ssh.invalid_port",
            Self::InvalidIdentityPath => "workspace.ssh.invalid_identity",
            Self::ConnectionFailed => "workspace.ssh.connection_failed",
            Self::AuthenticationFailed => "workspace.ssh.authentication_failed",
            Self::HostKeyChanged => "workspace.ssh.host_key_changed",
            Self::PermissionDenied => "workspace.ssh.permission_denied",
            Self::FolderMissing => "workspace.ssh.folder_missing",
            Self::UnsupportedServer => "workspace.ssh.unsupported",
            Self::HelperFailed => "workspace.ssh.helper_failed",
            Self::RequestFailed => "workspace.ssh.request_failed",
            Self::WorkerUnavailable => "workspace.ssh.worker_failed",
            Self::SessionExpired => "workspace.ssh.session_expired",
        }
    }
}

pub struct SshWorkspaceSnapshot {
    pub generation: u64,
    pub update: SshWorkspaceUpdate,
}
pub enum SshWorkspaceUpdate {
    Prompt(AuthPrompt),
    InstallRequired {
        install_path: String,
    },
    Connected {
        profile: SshConnectionProfile,
        target: SshWorkspaceTarget,
        home: RemotePath,
        agents: Vec<Agent>,
    },
    Directory {
        request_id: u64,
        path: RemotePath,
        page: DirectoryPage,
    },
    RootVerified {
        request_id: u64,
        target: SshWorkspaceTarget,
    },
    RequestFailed {
        request_id: u64,
        error: SshWorkspaceError,
    },
    Failed(SshWorkspaceError),
    Offline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Entry,
    Connecting,
    Prompt,
    Install,
    Connected,
    Offline,
    Failed,
    Registering,
}
struct SecretDraft(String);
impl SecretDraft {
    fn clear(&mut self) {
        // The owned draft is the sole printable copy. The widget receives masks only.
        unsafe {
            let bytes = self.0.as_mut_vec();
            for index in 0..bytes.capacity() {
                std::ptr::write_volatile(bytes.as_mut_ptr().add(index), 0);
            }
            bytes.set_len(0);
        }
        std::sync::atomic::compiler_fence(Ordering::SeqCst);
    }
}
impl Drop for SecretDraft {
    fn drop(&mut self) {
        self.clear();
    }
}

pub struct SshWorkspaceAddUi {
    draft: SshWorkspaceDraft,
    generation: u64,
    next_request: u64,
    phase: Phase,
    profile: Option<SshConnectionProfile>,
    server: Option<SshWorkspaceTarget>,
    agents: Vec<Agent>,
    directory: Option<(RemotePath, DirectoryPage)>,
    path_input: String,
    selected_path: Option<RemotePath>,
    verified: Option<SshWorkspaceTarget>,
    pending_browse: Option<(u64, RemotePath)>,
    pending_verify: Option<(u64, RemotePath)>,
    prompt: Option<AuthPrompt>,
    install_path: String,
    secret: SecretDraft,
    clear_widget: bool,
    error: Option<SshWorkspaceError>,
    exhausted: bool,
    queued: VecDeque<SshWorkspaceIntent>,
}
impl Default for SshWorkspaceAddUi {
    fn default() -> Self {
        Self::new()
    }
}
impl SshWorkspaceAddUi {
    pub fn new() -> Self {
        let generation = NEXT_GENERATION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .ok();
        Self {
            draft: SshWorkspaceDraft::default(),
            generation: generation.unwrap_or(0),
            next_request: 0,
            phase: Phase::Entry,
            profile: None,
            server: None,
            agents: Vec::new(),
            directory: None,
            path_input: String::new(),
            selected_path: None,
            verified: None,
            pending_browse: None,
            pending_verify: None,
            prompt: None,
            install_path: String::new(),
            secret: SecretDraft(String::new()),
            clear_widget: false,
            error: None,
            exhausted: generation.is_none(),
            queued: VecDeque::new(),
        }
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn draft(&self) -> SshWorkspaceDraft {
        self.draft.clone()
    }
    pub fn restore_draft(&mut self, draft: SshWorkspaceDraft) {
        self.invalidate(false);
        self.draft = draft;
    }
    pub fn draft_profile(&self) -> Result<SshConnectionProfile, SshWorkspaceError> {
        let port = if self.draft.port.is_empty() {
            None
        } else {
            Some(
                self.draft
                    .port
                    .parse::<u16>()
                    .ok()
                    .filter(|port| *port > 0)
                    .ok_or(SshWorkspaceError::InvalidPort)?,
            )
        };
        ssh_transport::SshDestination::parse(&self.draft.destination)
            .map_err(|_| SshWorkspaceError::InvalidAddress)?;
        let profile = SshConnectionProfile {
            id: self.draft.profile_id.clone(),
            destination: self.draft.destination.clone(),
            port,
            identity_file: self
                .draft
                .use_identity_file
                .then(|| self.draft.identity_file.clone()),
        };
        profile.validate().map_err(|_| {
            if self.draft.use_identity_file {
                SshWorkspaceError::InvalidIdentityPath
            } else {
                SshWorkspaceError::InvalidAddress
            }
        })?;
        Ok(profile)
    }
    pub fn can_add(&self) -> bool {
        self.phase == Phase::Connected
            && !self.exhausted
            && self.pending_verify.is_none()
            && self.verified.is_some()
            && self.profile.is_some()
    }
    #[cfg(test)]
    pub fn selected_path(&self) -> Option<&RemotePath> {
        self.selected_path.as_ref()
    }
    pub fn take_pending_intent(&mut self) -> Option<SshWorkspaceIntent> {
        self.queued.pop_front()
    }
    fn clear_secret(&mut self) {
        self.secret.clear();
        self.clear_widget = true;
    }
    fn clear_secret_widget(&mut self, ctx: &egui::Context) {
        if self.clear_widget {
            let id = egui::Id::new("ssh_workspace_auth_secret");
            ctx.data_mut(|data| data.remove::<egui::text_edit::TextEditState>(id));
            ctx.memory_mut(|memory| memory.surrender_focus(id));
            self.clear_widget = false;
        }
    }
    fn invalidate(&mut self, preserve_selection: bool) -> u64 {
        let old = self.generation;
        let next = NEXT_GENERATION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .ok();
        self.generation = next.unwrap_or(0);
        self.exhausted = next.is_none();
        self.phase = Phase::Entry;
        self.verified = None;
        self.prompt = None;
        self.directory = None;
        self.pending_browse = None;
        self.pending_verify = None;
        self.queued.clear();
        self.clear_secret();
        self.error = None;
        if !preserve_selection {
            self.selected_path = None;
            self.server = None;
            self.profile = None;
            self.agents.clear();
            self.path_input.clear();
        }
        old
    }
    fn same_server(left: &SshWorkspaceTarget, right: &SshWorkspaceTarget) -> bool {
        left.connection_id == right.connection_id
            && left.host.eq_ignore_ascii_case(&right.host)
            && left.port == right.port
            && left.account == right.account
            && left.host_key_sha256 == right.host_key_sha256
    }
    pub fn apply_snapshot(&mut self, snapshot: SshWorkspaceSnapshot) -> bool {
        if self.exhausted || snapshot.generation != self.generation {
            return false;
        }
        match snapshot.update {
            SshWorkspaceUpdate::Prompt(prompt) => {
                if prompt.id == 0
                    || prompt.text.len() > 8192
                    || prompt.text.contains('\0')
                    || self.error == Some(SshWorkspaceError::HostKeyChanged)
                {
                    return false;
                }
                self.clear_secret();
                self.prompt = Some(prompt);
                self.phase = Phase::Prompt;
                self.verified = None;
            }
            SshWorkspaceUpdate::InstallRequired { install_path } => {
                if install_path.is_empty()
                    || install_path.len() > 4096
                    || install_path.chars().any(char::is_control)
                {
                    return false;
                }
                self.clear_secret();
                self.prompt = None;
                self.install_path = install_path;
                self.phase = Phase::Install;
                self.verified = None;
            }
            SshWorkspaceUpdate::Connected {
                profile,
                target,
                home,
                agents,
            } => {
                if profile.validate().is_err()
                    || target.validate().is_err()
                    || target.connection_id != profile.id
                    || self.draft_profile().as_ref() != Ok(&profile)
                    || agents.len() > 32
                    || agents.iter().any(|agent| {
                        agent.kind.is_empty()
                            || agent.kind.len() > 64
                            || agent.kind.chars().any(char::is_control)
                            || agent.executable.len() > 4096
                    })
                {
                    return false;
                }
                if self
                    .server
                    .as_ref()
                    .is_some_and(|previous| !Self::same_server(previous, &target))
                {
                    self.error = Some(SshWorkspaceError::ConnectionFailed);
                    self.phase = Phase::Failed;
                    self.verified = None;
                    self.selected_path = None;
                    return false;
                }
                let selected = self.selected_path.clone();
                self.clear_secret();
                self.prompt = None;
                self.error = None;
                self.phase = Phase::Connected;
                self.server = Some(target);
                self.profile = Some(profile);
                self.agents = agents;
                if let Some(intent) = self.browse(home, None) {
                    self.queued.push_back(intent);
                }
                if let Some(path) = selected
                    && let Some(intent) = self.verify_root(path)
                {
                    self.queued.push_back(intent);
                }
            }
            SshWorkspaceUpdate::Directory {
                request_id,
                path,
                page,
            } => {
                if self.phase != Phase::Connected
                    || self.pending_browse.as_ref() != Some(&(request_id, path.clone()))
                {
                    return false;
                }
                if page.entries.len() > DIRECTORY_LIMIT
                    || page
                        .entries
                        .iter()
                        .map(|entry| entry.name.len())
                        .sum::<usize>()
                        > DIRECTORY_BYTES_MAX
                    || page
                        .next
                        .as_ref()
                        .is_some_and(|cursor| cursor.revision.len() > 256)
                {
                    self.error = Some(SshWorkspaceError::RequestFailed);
                    self.pending_browse = None;
                    return false;
                }
                self.pending_browse = None;
                self.path_input = path.as_str().to_owned();
                self.directory = Some((path, page));
            }
            SshWorkspaceUpdate::RootVerified { request_id, target } => {
                if self.phase != Phase::Connected
                    || self
                        .pending_verify
                        .as_ref()
                        .is_none_or(|(id, _)| *id != request_id)
                    || target.validate().is_err()
                    || self
                        .server
                        .as_ref()
                        .is_none_or(|server| !Self::same_server(server, &target))
                {
                    return false;
                }
                self.pending_verify = None;
                self.selected_path = Some(target.canonical_root.clone());
                self.verified = Some(target);
                self.error = None;
            }
            SshWorkspaceUpdate::RequestFailed { request_id, error } => {
                let browse = self
                    .pending_browse
                    .as_ref()
                    .is_some_and(|(id, _)| *id == request_id);
                let verify = self
                    .pending_verify
                    .as_ref()
                    .is_some_and(|(id, _)| *id == request_id);
                if !browse && !verify {
                    return false;
                }
                if browse {
                    self.pending_browse = None;
                }
                if verify {
                    self.pending_verify = None;
                    self.verified = None;
                }
                self.error = Some(error);
            }
            SshWorkspaceUpdate::Failed(error) => {
                self.clear_secret();
                self.prompt = None;
                self.phase = Phase::Failed;
                self.error = Some(error);
                self.verified = None;
                self.pending_browse = None;
                self.pending_verify = None;
                self.queued.clear();
                if error == SshWorkspaceError::HostKeyChanged {
                    self.selected_path = None;
                }
            }
            SshWorkspaceUpdate::Offline => {
                self.clear_secret();
                self.prompt = None;
                self.phase = Phase::Offline;
                self.verified = None;
                self.pending_browse = None;
                self.pending_verify = None;
                self.queued.clear();
            }
        }
        true
    }
    pub fn connect(&mut self) -> Option<SshWorkspaceIntent> {
        let profile = match self.draft_profile() {
            Ok(profile) => profile,
            Err(error) => {
                self.error = Some(error);
                return None;
            }
        };
        let preserve = self.profile.as_ref() == Some(&profile) && self.server.is_some();
        self.invalidate(preserve);
        if self.exhausted {
            self.error = Some(SshWorkspaceError::WorkerUnavailable);
            return None;
        }
        self.phase = Phase::Connecting;
        Some(SshWorkspaceIntent::Connect {
            generation: self.generation,
            profile,
        })
    }
    pub fn cancel(&mut self) -> SshWorkspaceIntent {
        let generation = self.invalidate(false);
        SshWorkspaceIntent::Cancel { generation }
    }
    pub fn close(&mut self) -> SshWorkspaceIntent {
        let generation = self.invalidate(false);
        SshWorkspaceIntent::Close { generation }
    }
    fn request_id(&mut self) -> Option<u64> {
        self.next_request = self.next_request.checked_add(1)?;
        Some(self.next_request)
    }
    pub fn browse(
        &mut self,
        path: RemotePath,
        cursor: Option<DirectoryCursor>,
    ) -> Option<SshWorkspaceIntent> {
        if self.phase != Phase::Connected {
            return None;
        }
        let request_id = self.request_id()?;
        self.pending_browse = Some((request_id, path.clone()));
        self.pending_verify = None;
        self.selected_path = None;
        self.verified = None;
        self.path_input = path.as_str().to_owned();
        self.directory = None;
        self.error = None;
        Some(SshWorkspaceIntent::Browse {
            generation: self.generation,
            request_id,
            path,
            cursor,
        })
    }
    pub fn verify_root(&mut self, path: RemotePath) -> Option<SshWorkspaceIntent> {
        if self.phase != Phase::Connected {
            return None;
        }
        let request_id = self.request_id()?;
        self.pending_verify = Some((request_id, path.clone()));
        self.selected_path = Some(path.clone());
        self.verified = None;
        self.error = None;
        Some(SshWorkspaceIntent::VerifyRoot {
            generation: self.generation,
            request_id,
            path,
        })
    }
    fn folder_path(parent: &RemotePath, name: &[u8]) -> Option<RemotePath> {
        let name = std::str::from_utf8(name).ok()?;
        if name.is_empty() || name.contains('/') {
            return None;
        }
        RemotePath::new(format!("{}/{name}", parent.as_str().trim_end_matches('/'))).ok()
    }
    fn reply(&mut self, reply: PromptReply) -> Option<SshWorkspaceIntent> {
        let prompt = self.prompt.take()?;
        self.clear_secret();
        self.phase = Phase::Connecting;
        Some(SshWorkspaceIntent::PromptReply {
            generation: self.generation,
            request_id: prompt.id,
            reply,
        })
    }
    pub fn show_body(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
    ) -> Option<SshWorkspaceIntent> {
        use crate::ui::popup;
        self.clear_secret_widget(ui.ctx());
        let mut action = self.take_pending_intent();
        if !self.queued.is_empty() {
            ui.ctx().request_repaint();
        }
        popup::field(
            ui,
            &catalog.t("workspace.ssh.address", &[]),
            Some(&catalog.t("workspace.ssh.address_hint", &[])),
            |ui| {
                let key = match self.phase {
                    Phase::Connecting => "workspace.ssh.connecting",
                    Phase::Connected => "workspace.ssh.recheck",
                    Phase::Offline => "workspace.ssh.reconnect",
                    _ => "workspace.ssh.connect",
                };
                let enabled = !matches!(
                    self.phase,
                    Phase::Connecting | Phase::Prompt | Phase::Install | Phase::Registering
                );
                let label = catalog.t(key, &[]);
                let button_width = ui.fonts_mut(|fonts| {
                    fonts
                        .layout_no_wrap(
                            label.clone(),
                            egui::FontId::proportional(13.0),
                            egui::Color32::WHITE,
                        )
                        .size()
                        .x
                }) + 26.0;
                let narrow = ui.available_width() < button_width + 31.0;
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 7.0;
                    let width = if narrow {
                        ui.available_width()
                    } else {
                        (ui.available_width() - button_width - 7.0).max(24.0)
                    };
                    let input = ui
                        .allocate_ui_with_layout(
                            egui::vec2(width, 36.0),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| popup::text_input(ui, &mut self.draft.destination, ""),
                        )
                        .inner;
                    if input.changed() {
                        action = Some(self.cancel());
                    }
                    if popup::action_button(ui, &label, popup::ActionTone::Secondary, enabled)
                        .clicked()
                    {
                        action = self.connect();
                    }
                });
            },
        );
        egui::CollapsingHeader::new(catalog.t("workspace.ssh.options", &[]))
            .id_salt("ssh_workspace_options")
            .show(ui, |ui| {
                popup::field(
                    ui,
                    &catalog.t("workspace.ssh.port", &[]),
                    Some(&catalog.t("workspace.ssh.port_hint", &[])),
                    |ui| {
                        if popup::text_input(ui, &mut self.draft.port, "").changed() {
                            action = Some(self.cancel());
                        }
                    },
                );
                if ui
                    .checkbox(
                        &mut self.draft.use_identity_file,
                        catalog.t("workspace.ssh.specific_key", &[]),
                    )
                    .changed()
                {
                    action = Some(self.cancel());
                }
                if self.draft.use_identity_file {
                    popup::field(
                        ui,
                        &catalog.t("workspace.ssh.key_path", &[]),
                        Some(&catalog.t("workspace.ssh.key_hint", &[])),
                        |ui| {
                            if popup::text_input(ui, &mut self.draft.identity_file, "").changed() {
                                action = Some(self.cancel());
                            }
                        },
                    );
                }
                ui.add(egui::Label::new(catalog.t("workspace.ssh.config_hint", &[])).wrap());
            });
        match self.phase {
            Phase::Connecting => popup::notice(
                ui,
                &catalog.t("workspace.ssh.connecting_hint", &[]),
                popup::NoticeTone::Info,
            ),
            Phase::Prompt => {
                if let Some(prompt) = self.prompt.clone() {
                    let title = if prompt.kind == PromptKind::HostTrust {
                        "workspace.ssh.trust_title"
                    } else {
                        "workspace.ssh.auth_title"
                    };
                    popup::notice(ui, &catalog.t(title, &[]), popup::NoticeTone::Info);
                    ui.add(
                        egui::Label::new(egui::RichText::new(&prompt.text).monospace().size(12.0))
                            .wrap(),
                    );
                    if prompt.kind == PromptKind::Authentication {
                        popup::field(
                            ui,
                            &catalog.t("workspace.ssh.auth_secret", &[]),
                            None,
                            |ui| {
                                let mut buffer =
                                    popup::MaskedSecretBuffer::new(&mut self.secret.0, 4096);
                                popup::secret_input(
                                    ui,
                                    &mut buffer,
                                    egui::Id::new("ssh_workspace_auth_secret"),
                                    &catalog.t("workspace.ssh.auth_hint", &[]),
                                    false,
                                );
                            },
                        );
                    }
                    let key = match prompt.kind {
                        PromptKind::HostTrust => "workspace.ssh.trust_continue",
                        PromptKind::Authentication => "workspace.ssh.auth_continue",
                        PromptKind::AuthenticationConfirmation => "workspace.ssh.auth_approve",
                    };
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        if popup::action_button(
                            ui,
                            &catalog.t(key, &[]),
                            popup::ActionTone::Secondary,
                            prompt.kind != PromptKind::Authentication || !self.secret.0.is_empty(),
                        )
                        .clicked()
                        {
                            let reply = match prompt.kind {
                                PromptKind::HostTrust => Some(PromptReply::ApproveHost),
                                PromptKind::AuthenticationConfirmation => {
                                    Some(PromptReply::ApproveAuthentication)
                                }
                                PromptKind::Authentication => ssh_transport::Secret::new(
                                    std::mem::take(&mut self.secret.0).into_bytes(),
                                )
                                .ok()
                                .map(PromptReply::Secret),
                            };
                            if let Some(reply) = reply {
                                action = self.reply(reply);
                            } else {
                                self.error = Some(SshWorkspaceError::AuthenticationFailed);
                            }
                        }
                        if popup::action_button(
                            ui,
                            &catalog.t("action.cancel", &[]),
                            popup::ActionTone::Ghost,
                            true,
                        )
                        .clicked()
                        {
                            action = self.reply(PromptReply::Reject);
                        }
                    });
                }
            }
            Phase::Install => {
                popup::notice(
                    ui,
                    &catalog.t("workspace.ssh.install_hint", &[]),
                    popup::NoticeTone::Info,
                );
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&self.install_path)
                            .monospace()
                            .size(12.0),
                    )
                    .wrap(),
                );
                if popup::action_button(
                    ui,
                    &catalog.t("workspace.ssh.install_continue", &[]),
                    popup::ActionTone::Secondary,
                    true,
                )
                .clicked()
                {
                    self.phase = Phase::Connecting;
                    action = Some(SshWorkspaceIntent::InstallHelper {
                        generation: self.generation,
                    });
                }
            }
            Phase::Connected => {
                if let Some(server) = &self.server {
                    popup::notice(
                        ui,
                        &catalog.t(
                            "workspace.ssh.connected",
                            &[(
                                "server",
                                &format!("{}@{}:{}", server.account, server.host, server.port),
                            )],
                        ),
                        popup::NoticeTone::Info,
                    );
                }
            }
            Phase::Offline => popup::notice(
                ui,
                &catalog.t("workspace.ssh.offline", &[]),
                popup::NoticeTone::Info,
            ),
            Phase::Registering => popup::notice(
                ui,
                &catalog.t("workspace.add.opening", &[]),
                popup::NoticeTone::Info,
            ),
            Phase::Entry | Phase::Failed => {}
        }
        if let Some(error) = self.error {
            popup::notice(
                ui,
                &catalog.t(error.message_key(), &[]),
                popup::NoticeTone::Error,
            );
        }
        ui.separator();
        ui.label(
            egui::RichText::new(catalog.t("workspace.ssh.folders", &[]))
                .size(12.0)
                .strong(),
        );
        if self.phase == Phase::Connected {
            let parent = self.directory.as_ref().and_then(|(path, _)| {
                (path.as_str() != "/")
                    .then(|| {
                        path.as_str()
                            .rsplit_once('/')
                            .map(|(parent, _)| if parent.is_empty() { "/" } else { parent })
                            .unwrap_or("/")
                    })
                    .and_then(|path| RemotePath::new(path).ok())
            });
            let (row, up_clicked) = ui
                .scope(|ui| {
                    // Both horizontal layouts must begin at the shared input height.
                    // Expanding an initially 18pt nested row shifts its controls down.
                    ui.spacing_mut().interact_size.y = 36.0;
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        let up = popup::action_button(
                            ui,
                            "↑",
                            popup::ActionTone::Ghost,
                            parent.is_some(),
                        );
                        up.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                ui.is_enabled(),
                                catalog.t("workspace.ssh.up", &[]),
                            )
                        });
                        let up = up.on_hover_text(catalog.t("workspace.ssh.up", &[]));
                        let row = popup::path_input(
                            ui,
                            &mut self.path_input,
                            &catalog.t("workspace.ssh.open_path", &[]),
                        );
                        (row, up.clicked())
                    })
                    .inner
                })
                .inner;
            if up_clicked && let Some(parent) = parent {
                action = self.browse(parent, None);
            }
            if row.input.changed() {
                self.selected_path = None;
                self.verified = None;
                self.pending_verify = None;
                self.pending_browse = None;
            }
            if row.browse.clicked()
                || (row.input.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)))
            {
                match RemotePath::new(self.path_input.clone()) {
                    Ok(path) => action = self.browse(path, None),
                    Err(_) => self.error = Some(SshWorkspaceError::FolderMissing),
                }
            }
            if let Some((path, page)) = self.directory.clone() {
                popup::list_row(ui, |ui| {
                    let mut count = 0;
                    for entry in page.entries.iter().filter(|entry| entry.is_directory) {
                        count += 1;
                        let target = Self::folder_path(&path, &entry.name);
                        let name = std::str::from_utf8(&entry.name).unwrap_or("");
                        let label = if name.is_empty() {
                            catalog.t("workspace.ssh.unsupported_name", &[])
                        } else {
                            name.to_owned()
                        };
                        let selected = target
                            .as_ref()
                            .is_some_and(|target| self.selected_path.as_ref() == Some(target));
                        let response = ui.add_enabled(
                            target.is_some(),
                            egui::Button::selectable(selected, label)
                                .min_size(egui::vec2(ui.available_width(), 34.0)),
                        );
                        if let Some(target) = target {
                            if response.double_clicked()
                                || (response.has_focus()
                                    && ui.input(|input| input.key_pressed(egui::Key::ArrowRight)))
                            {
                                action = self.browse(target, None);
                            } else if response.clicked() {
                                action = self.verify_root(target);
                            }
                        }
                    }
                    if count == 0 {
                        ui.add(egui::Label::new(catalog.t("workspace.ssh.empty", &[])).wrap());
                    }
                });
                if popup::action_button(
                    ui,
                    &catalog.t("workspace.ssh.choose_current", &[]),
                    popup::ActionTone::Secondary,
                    true,
                )
                .clicked()
                {
                    action = self.verify_root(path.clone());
                }
                if let Some(cursor) = page.next
                    && popup::action_button(
                        ui,
                        &catalog.t("workspace.ssh.next_page", &[]),
                        popup::ActionTone::Ghost,
                        true,
                    )
                    .clicked()
                {
                    action = self.browse(path, Some(cursor));
                }
            } else {
                popup::notice(
                    ui,
                    &catalog.t("workspace.ssh.loading_folders", &[]),
                    popup::NoticeTone::Info,
                );
            }
            if self.pending_verify.is_some() {
                ui.label(catalog.t("workspace.ssh.checking_root", &[]));
            }
            if !self.agents.is_empty() {
                ui.add(
                    egui::Label::new(
                        catalog.t(
                            "workspace.ssh.installed_agents",
                            &[(
                                "agents",
                                &self
                                    .agents
                                    .iter()
                                    .map(|agent| agent.kind.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", "),
                            )],
                        ),
                    )
                    .wrap(),
                );
            }
        } else {
            popup::notice(
                ui,
                &catalog.t("workspace.ssh.folders_hint", &[]),
                popup::NoticeTone::Info,
            );
        }
        if let Some(path) = &self.selected_path {
            ui.add(
                egui::Label::new(catalog.t("workspace.ssh.selected", &[("path", path.as_str())]))
                    .wrap(),
            );
        }
        popup::field(
            ui,
            &catalog.t("workspace.ssh.name", &[]),
            Some(&catalog.t("workspace.ssh.name_hint", &[])),
            |ui| {
                popup::text_input(ui, &mut self.draft.name, "");
            },
        );
        self.clear_secret_widget(ui.ctx());
        action
    }
    pub fn show_footer(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
    ) -> Option<SshWorkspaceIntent> {
        use crate::ui::popup;
        let mut action = None;
        if popup::action_button(
            ui,
            &catalog.t("workspace.ssh.add", &[]),
            popup::ActionTone::Primary,
            self.can_add(),
        )
        .clicked()
            && let (Some(profile), Some(target)) = (self.profile.clone(), self.verified.clone())
        {
            let name = if self.draft.name.trim().is_empty() {
                target
                    .canonical_root
                    .as_str()
                    .rsplit('/')
                    .find(|part| !part.is_empty())
                    .unwrap_or(&target.host)
                    .to_owned()
            } else {
                self.draft.name.clone()
            };
            self.phase = Phase::Registering;
            action = Some(SshWorkspaceIntent::Add {
                generation: self.generation,
                name,
                profile,
                target,
            });
        }
        if popup::action_button(
            ui,
            &catalog.t("action.cancel", &[]),
            popup::ActionTone::Ghost,
            true,
        )
        .clicked()
        {
            action = Some(self.close());
        }
        self.clear_secret_widget(ui.ctx());
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (SshWorkspaceAddUi, SshConnectionProfile, SshWorkspaceTarget) {
        let mut ui = SshWorkspaceAddUi::new();
        ui.draft.destination = "ubuntu@dev-alias".into();
        let profile = ui.draft_profile().unwrap();
        let target = SshWorkspaceTarget::new(
            profile.id.clone(),
            "server.example".into(),
            22,
            "ubuntu".into(),
            "SHA256:abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG".into(),
            "root-id".into(),
            RemotePath::new("/").unwrap(),
        )
        .unwrap();
        (ui, profile, target)
    }
    fn connected(
        ui: &mut SshWorkspaceAddUi,
        profile: SshConnectionProfile,
        target: SshWorkspaceTarget,
    ) {
        let generation = ui.generation();
        assert!(ui.apply_snapshot(SshWorkspaceSnapshot {
            generation,
            update: SshWorkspaceUpdate::Connected {
                profile,
                target,
                home: RemotePath::new("/home/ubuntu").unwrap(),
                agents: vec![Agent {
                    kind: "codex".into(),
                    executable: "/usr/bin/codex".into()
                }]
            }
        }));
    }
    #[test]
    fn ssh_workspace_generation_rejects_late_results_and_discards_secrets() {
        let (mut ui, profile, target) = setup();
        connected(&mut ui, profile.clone(), target.clone());
        ui.secret.0 = "private-value".into();
        let old = ui.generation();
        let _ = ui.cancel();
        assert_ne!(ui.generation(), old);
        assert!(ui.secret.0.is_empty());
        assert!(!ui.apply_snapshot(SshWorkspaceSnapshot {
            generation: old,
            update: SshWorkspaceUpdate::Connected {
                profile,
                target,
                home: RemotePath::new("/").unwrap(),
                agents: Vec::new()
            }
        }));
        assert!(!ui.can_add());
        assert_eq!(ui.draft.destination, "ubuntu@dev-alias");
    }
    #[test]
    fn ssh_workspace_add_requires_correlated_server_verified_root() {
        let (mut ui, profile, target) = setup();
        connected(&mut ui, profile, target.clone());
        assert!(!ui.can_add());
        let path = RemotePath::new("/home/ubuntu/project").unwrap();
        let Some(SshWorkspaceIntent::VerifyRoot { request_id, .. }) = ui.verify_root(path.clone())
        else {
            panic!("must request server validation")
        };
        let mut verified = target.clone();
        verified.canonical_root = path;
        verified.remote_workspace_id = "project-id".into();
        assert!(!ui.apply_snapshot(SshWorkspaceSnapshot {
            generation: ui.generation(),
            update: SshWorkspaceUpdate::RootVerified {
                request_id: request_id + 1,
                target: verified.clone()
            }
        }));
        assert!(!ui.can_add());
        assert!(ui.apply_snapshot(SshWorkspaceSnapshot {
            generation: ui.generation(),
            update: SshWorkspaceUpdate::RootVerified {
                request_id,
                target: verified
            }
        }));
        assert!(ui.can_add());
        assert!(ui.apply_snapshot(SshWorkspaceSnapshot {
            generation: ui.generation(),
            update: SshWorkspaceUpdate::Offline
        }));
        assert!(!ui.can_add());
        assert!(ui.selected_path().is_some());
    }
    #[test]
    fn ssh_workspace_directory_responses_cannot_overwrite_later_navigation() {
        let (mut ui, profile, target) = setup();
        connected(&mut ui, profile, target);
        let first = RemotePath::new("/first").unwrap();
        let second = RemotePath::new("/second").unwrap();
        let Some(SshWorkspaceIntent::Browse {
            request_id: old, ..
        }) = ui.browse(first.clone(), None)
        else {
            panic!("browse")
        };
        let Some(SshWorkspaceIntent::Browse {
            request_id: current,
            ..
        }) = ui.browse(second.clone(), None)
        else {
            panic!("browse")
        };
        assert!(!ui.apply_snapshot(SshWorkspaceSnapshot {
            generation: ui.generation(),
            update: SshWorkspaceUpdate::Directory {
                request_id: old,
                path: first,
                page: DirectoryPage {
                    entries: Vec::new(),
                    next: None
                }
            }
        }));
        assert!(ui.apply_snapshot(SshWorkspaceSnapshot {
            generation: ui.generation(),
            update: SshWorkspaceUpdate::Directory {
                request_id: current,
                path: second.clone(),
                page: DirectoryPage {
                    entries: Vec::new(),
                    next: None
                }
            }
        }));
        assert_eq!(ui.directory.as_ref().unwrap().0, second);
    }
    /// Test-only server snapshots for comparing the approved eight-state HTML design.
    /// Registered success closes the picker in App; its last picker state is verified-ready.
    #[cfg(feature = "ssh-workspaces")]
    #[test]
    #[ignore = "offscreen eight-state SSH popup PNGs for manual visual review"]
    fn popup_parity_render_ssh_workspace_eight_states() {
        use deppy_remote_host::files::DirectoryEntry;
        let output =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ssh-ui-qa");
        std::fs::create_dir_all(&output).unwrap();
        for (index, label) in [
            "01-entry",
            "02-host-trust",
            "03-authentication",
            "04-install-consent",
            "05-browse",
            "06-failed",
            "07-offline",
            "08-verified-ready",
        ]
        .into_iter()
        .enumerate()
        {
            let (mut state, _, _) = setup();
            state.draft.destination = "ubuntu@gpu3090".into();
            state.draft.name = "xAegis".into();
            let profile = state.draft_profile().unwrap();
            let target = SshWorkspaceTarget::new(
                profile.id.clone(),
                "gpu3090".into(),
                22,
                "ubuntu".into(),
                "SHA256:abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG".into(),
                "render-home".into(),
                RemotePath::new("/").unwrap(),
            )
            .unwrap();
            match index {
                1 | 2 => {
                    state.apply_snapshot(SshWorkspaceSnapshot {
                        generation: state.generation(),
                        update: SshWorkspaceUpdate::Prompt(AuthPrompt {
                            id: 1,
                            kind: if index == 1 {
                                PromptKind::HostTrust
                            } else {
                                PromptKind::Authentication
                            },
                            text: if index == 1 {
                                "ED25519 · SHA256:abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG"
                            } else {
                                "SSH key passphrase:"
                            }
                            .into(),
                        }),
                    });
                }
                3 => {
                    state.apply_snapshot(SshWorkspaceSnapshot {
                        generation: state.generation(),
                        update: SshWorkspaceUpdate::InstallRequired {
                            install_path: format!(
                                "/home/ubuntu/.local/share/deppy/remote-host/{}",
                                env!("CARGO_PKG_VERSION")
                            ),
                        },
                    });
                }
                4 | 6 | 7 => {
                    connected(&mut state, profile, target.clone());
                    let directory = RemotePath::new(if index == 7 {
                        "/home/ubuntu/xAegis"
                    } else {
                        "/home/ubuntu"
                    })
                    .unwrap();
                    let Some(SshWorkspaceIntent::Browse { request_id, .. }) =
                        state.browse(directory.clone(), None)
                    else {
                        panic!("browse fixture")
                    };
                    let entries = if index == 7 {
                        vec![
                            ("src", true),
                            ("docs", true),
                            ("tests", true),
                            ("Cargo.toml", false),
                        ]
                    } else {
                        vec![("xAegis", true), ("data", true), ("archive", true)]
                    };
                    state.apply_snapshot(SshWorkspaceSnapshot {
                        generation: state.generation(),
                        update: SshWorkspaceUpdate::Directory {
                            request_id,
                            path: directory,
                            page: DirectoryPage {
                                entries: entries
                                    .into_iter()
                                    .map(|(name, is_directory)| DirectoryEntry {
                                        name: name.as_bytes().to_vec(),
                                        is_directory,
                                        is_symlink: false,
                                    })
                                    .collect(),
                                next: None,
                            },
                        },
                    });
                    if index == 6 || index == 7 {
                        let path = RemotePath::new("/home/ubuntu/xAegis").unwrap();
                        let Some(SshWorkspaceIntent::VerifyRoot { request_id, .. }) =
                            state.verify_root(path.clone())
                        else {
                            panic!("verify fixture")
                        };
                        let mut verified = target;
                        verified.canonical_root = path;
                        verified.remote_workspace_id = "render-project".into();
                        state.apply_snapshot(SshWorkspaceSnapshot {
                            generation: state.generation(),
                            update: SshWorkspaceUpdate::RootVerified {
                                request_id,
                                target: verified,
                            },
                        });
                    }
                    if index == 6 {
                        state.apply_snapshot(SshWorkspaceSnapshot {
                            generation: state.generation(),
                            update: SshWorkspaceUpdate::Offline,
                        });
                    }
                }
                5 => {
                    state.apply_snapshot(SshWorkspaceSnapshot {
                        generation: state.generation(),
                        update: SshWorkspaceUpdate::Failed(SshWorkspaceError::AuthenticationFailed),
                    });
                }
                _ => {}
            }
            for (suffix, size) in [
                ("", egui::vec2(800.0, 950.0)),
                ("-narrow", egui::vec2(390.0, 700.0)),
            ] {
                // Move the same data-driven picker state into each offscreen viewport and back.
                let mut dialog = crate::workspace_add::WorkspaceAddUi::new(
                    crate::workspace_add::WorkspaceAddPurpose::SwitchRuntime,
                    std::path::PathBuf::new(),
                );
                dialog.open_ssh_draft(state.draft());
                std::mem::swap(dialog.ssh_mut(), &mut state);
                let catalog = i18n::Catalog::load("ko-KR").unwrap();
                let mut harness = egui_kittest::Harness::builder()
                    .with_size(size)
                    .build_ui_state(
                        move |ui, dialog: &mut crate::workspace_add::WorkspaceAddUi| {
                            dialog.show(ui.ctx(), &catalog);
                        },
                        dialog,
                    );
                crate::fonts::install_cjk_fallback(&harness.ctx, None, "JetBrainsMono", "Regular");
                harness.ctx.set_visuals(egui::Visuals::dark());
                harness.run();
                harness
                    .render()
                    .unwrap()
                    .save(output.join(format!("{label}{suffix}.png")))
                    .unwrap();
                std::mem::swap(harness.state_mut().ssh_mut(), &mut state);
            }
        }
    }
    #[test]
    fn ssh_workspace_auth_confirmation_emits_auth_reply_only() {
        use egui_kittest::kittest::Queryable;
        let (mut state, _, _) = setup();
        state.apply_snapshot(SshWorkspaceSnapshot {
            generation: state.generation(),
            update: SshWorkspaceUpdate::Prompt(AuthPrompt {
                id: 41,
                kind: PromptKind::AuthenticationConfirmation,
                text: "Confirm authentication with your security key?".into(),
            }),
        });
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 950.0))
            .build_ui_state(
                move |ui, state: &mut (SshWorkspaceAddUi, Vec<SshWorkspaceIntent>)| {
                    if let Some(intent) = state.0.show_body(ui, &catalog) {
                        state.1.push(intent);
                    }
                },
                (state, Vec::new()),
            );
        harness.run();
        harness.get_by_label("Approve authentication").click();
        harness.run();
        assert!(harness.state().1.iter().any(|intent| matches!(
            intent,
            SshWorkspaceIntent::PromptReply {
                request_id: 41,
                reply: PromptReply::ApproveAuthentication,
                ..
            }
        )));
        assert!(!harness.state().1.iter().any(|intent| matches!(
            intent,
            SshWorkspaceIntent::PromptReply {
                reply: PromptReply::ApproveHost,
                ..
            }
        )));
    }
    #[test]
    fn ssh_workspace_prompt_actions_share_one_row() {
        use egui_kittest::kittest::Queryable as _;
        let (mut state, _, _) = setup();
        state.apply_snapshot(SshWorkspaceSnapshot {
            generation: state.generation(),
            update: SshWorkspaceUpdate::Prompt(AuthPrompt {
                id: 17,
                kind: PromptKind::HostTrust,
                text: "Verified fixture fingerprint".into(),
            }),
        });
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(560.0, 900.0))
            .build_ui(move |ui| {
                state.show_body(ui, &catalog);
            });
        harness.run();
        let approve = harness.get_by_label("Trust and continue").rect();
        let cancel = harness.get_by_label("Cancel").rect();
        assert!(
            (approve.center().y - cancel.center().y).abs() < 1.0,
            "prompt decisions must use the approved inline action row: {approve:?} {cancel:?}"
        );
        assert!((cancel.left() - approve.right() - 8.0).abs() < 1.0);
    }
    #[test]
    fn ssh_workspace_parent_navigation_shares_the_path_row() {
        use egui_kittest::kittest::Queryable as _;
        let (mut state, profile, target) = setup();
        connected(&mut state, profile, target);
        let path = RemotePath::new("/srv/project").unwrap();
        let Some(SshWorkspaceIntent::Browse { request_id, .. }) = state.browse(path.clone(), None)
        else {
            panic!("browse fixture")
        };
        state.apply_snapshot(SshWorkspaceSnapshot {
            generation: state.generation(),
            update: SshWorkspaceUpdate::Directory {
                request_id,
                path,
                page: DirectoryPage {
                    entries: vec![],
                    next: None,
                },
            },
        });
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(560.0, 1000.0))
            .build_ui(move |ui| {
                state.show_body(ui, &catalog);
            });
        harness.run();
        let parent = harness.get_by_label("Parent folder").rect();
        let open = harness.get_by_label("Open path").rect();
        assert!(
            (parent.center().y - open.center().y).abs() < 1.0,
            "parent navigation belongs in the path row: {parent:?} {open:?}"
        );
        assert!(parent.right() < open.left());
    }
    #[test]
    fn ssh_workspace_secret_is_absent_from_shapes_accessibility_and_undo() {
        let (mut state, _, _) = setup();
        state.apply_snapshot(SshWorkspaceSnapshot {
            generation: state.generation(),
            update: SshWorkspaceUpdate::Prompt(AuthPrompt {
                id: 42,
                kind: PromptKind::Authentication,
                text: "Password:".into(),
            }),
        });
        let sentinel = "private-ssh-fixture-value";
        state.secret.0 = sentinel.into();
        let context = egui::Context::default();
        context.enable_accesskit();
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut output = context.run_ui(egui::RawInput::default(), |ui| {
            state.show_body(ui, &catalog);
        });
        output.textures_delta.clear();
        assert!(!format!("{:?}", output.shapes).contains(sentinel));
        let update = output.platform_output.accesskit_update.unwrap();
        assert!(
            update
                .nodes
                .iter()
                .all(|(_, node)| !node.label().unwrap_or("").contains(sentinel)
                    && !node.value().unwrap_or("").contains(sentinel))
        );
        let edit = egui::text_edit::TextEditState::load(
            &context,
            egui::Id::new("ssh_workspace_auth_secret"),
        );
        if let Some(edit) = edit {
            let mut undoer = edit.undoer();
            let mut current = (
                egui::text::CCursorRange::default(),
                "•".repeat(sentinel.chars().count()),
            );
            for _ in 0..1024 {
                let Some(previous) = undoer.undo(&current).cloned() else {
                    break;
                };
                assert!(!previous.1.contains(sentinel));
                current = previous;
            }
        }
        let _ = state.close();
        state.clear_secret_widget(&context);
        assert!(state.secret.0.is_empty());
        assert!(
            egui::text_edit::TextEditState::load(
                &context,
                egui::Id::new("ssh_workspace_auth_secret")
            )
            .is_none()
        );
    }
    #[test]
    fn ssh_workspace_reconnect_preserves_safe_selection_but_rechecks_root() {
        let (mut state, profile, target) = setup();
        connected(&mut state, profile.clone(), target.clone());
        let path = RemotePath::new("/srv/한 글/it's $data").unwrap();
        let _ = state.verify_root(path.clone());
        state.apply_snapshot(SshWorkspaceSnapshot {
            generation: state.generation(),
            update: SshWorkspaceUpdate::Offline,
        });
        let old = state.generation();
        assert!(matches!(
            state.connect(),
            Some(SshWorkspaceIntent::Connect { .. })
        ));
        assert_ne!(state.generation(), old);
        connected(&mut state, profile, target);
        assert!(!state.can_add());
        assert_eq!(state.selected_path(), Some(&path));
        assert!(state.queued.iter().any(|intent| matches!(intent, SshWorkspaceIntent::VerifyRoot{path:actual,..} if actual==&path)));
    }
    #[test]
    fn ssh_workspace_profile_uses_alias_and_validates_port_without_rewriting_paths() {
        let (mut ui, _, _) = setup();
        ui.draft.port = "2222".into();
        ui.draft.use_identity_file = true;
        ui.draft.identity_file = "/Users/me/키 'file'".into();
        let profile = ui.draft_profile().unwrap();
        assert_eq!(profile.destination, "ubuntu@dev-alias");
        assert_eq!(profile.port, Some(2222));
        assert_eq!(profile.identity_file, Some(ui.draft.identity_file.clone()));
        ui.draft.port = "65536".into();
        assert_eq!(ui.draft_profile(), Err(SshWorkspaceError::InvalidPort));
        ui.draft.port.clear();
        ui.draft.destination = "host;touch /tmp/injection".into();
        assert_eq!(ui.draft_profile(), Err(SshWorkspaceError::InvalidAddress));
    }
}
