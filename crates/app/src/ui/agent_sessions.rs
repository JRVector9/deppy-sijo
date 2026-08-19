//! Native structured Codex Agent Sessions panel.
//!
//! It renders data from `codex app-server`, not a parsed terminal transcript.
//! The existing PTY workspace remains untouched and continues to display raw
//! terminal bytes exactly as before.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::TryRecvError;

use crate::agent_session::{
    AgentApprovalDecision, AgentApprovalKind, AgentSession, AgentSessionEvent, AgentSessionId,
    AgentSessionStatus, AgentSkillSelection,
};
use crate::agent_surface::{
    AgentProvider, AgentSurfaceId, AgentSurfaceSnapshot, AgentTransport, AgentVisualState,
};
use crate::codex_app_server::{
    CodexAppServerClient, CodexAppServerEvent, CodexAppServerReply, CodexLlmOverride,
    CodexModelCatalogReply, CodexModelInfo, CodexSkillCatalogReply, CodexSkillInfo,
    codex_llm_override_from_config, validate_llm_api_key, validate_llm_base_url,
};
use crate::config::AgentsConfig;
pub const AGENT_SESSION_SENSITIVE_ITEM_MAX_BYTES: usize = 32 * 1024;
const AGENT_SESSION_TEXT_INPUT_MAX_BYTES: usize = 1024 * 1024;
const AGENT_SESSION_PATH_INPUT_MAX_BYTES: usize = 32 * 1024;
pub(crate) const AGENT_SESSION_PERSISTED_MAX_ITEMS: usize = 500;
pub(crate) const AGENT_SESSION_PERSISTED_ROW_MAX_BYTES: usize = 32 * 1024;
const AGENT_SESSION_PERSISTED_TOTAL_MAX_BYTES: usize = 4 * 1024 * 1024;
// This panel owns one API-key draft at a time, so it remains below the shared credential corpus
// budget of 64 items / 1 MiB while enforcing the same 32 KiB redaction-safe item ceiling.

/// Root-provided presence metadata for custom-provider authentication. No secret plaintext is
/// retained in this snapshot and render never probes an external store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentSessionsSecretsSnapshot {
    revision: u64,
    available: bool,
    controls_enabled: bool,
    api_key_present: bool,
}

impl AgentSessionsSecretsSnapshot {
    pub const fn new(revision: u64, api_key_present: bool) -> Self {
        Self {
            revision,
            available: true,
            controls_enabled: true,
            api_key_present,
        }
    }

    pub const fn unavailable(revision: u64) -> Self {
        Self {
            revision,
            available: false,
            controls_enabled: true,
            api_key_present: false,
        }
    }

    pub const fn revision(self) -> u64 {
        self.revision
    }

    pub const fn is_available(self) -> bool {
        self.available
    }

    pub const fn controls_enabled(self) -> bool {
        self.controls_enabled
    }

    pub const fn api_key_present(self) -> bool {
        self.api_key_present
    }
}

/// Secret-bearing UI input. It is non-Clone/non-Serialize, Debug is always redacted, and the
/// owned allocation is overwritten on drop.
pub struct SensitiveInput(String);

impl SensitiveInput {
    pub fn try_api_key(mut value: String) -> anyhow::Result<Self> {
        if value.len() > AGENT_SESSION_SENSITIVE_ITEM_MAX_BYTES {
            clear_sensitive_string(&mut value);
            anyhow::bail!("agent_session_sensitive_item_limit");
        }
        match validate_llm_api_key(&value) {
            Ok(validated) => {
                clear_sensitive_string(&mut value);
                Ok(Self(validated))
            }
            Err(error) => {
                clear_sensitive_string(&mut value);
                Err(error)
            }
        }
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

/// Root handles these intents after the render frame. No variant implements Clone or Serialize.
pub enum AgentSessionsSecretIntent {
    SaveApiKey {
        revision: u64,
        input: SensitiveInput,
    },
    DeleteApiKey {
        revision: u64,
    },
}

/// Production host adapter. The root resolves authentication and owns process construction, so
/// this leaf never names or loads a concrete secret type.
pub trait CodexAppServerHost: Send + Sync {
    fn spawn(
        &self,
        llm_override: Option<CodexLlmOverride>,
        ctx: egui::Context,
    ) -> anyhow::Result<CodexAppServerClient>;
}

pub struct AgentSessionsFrameOutput {
    /// Compatibility output for callers that already handle PTY requests. Rendered controller
    /// actions produce requests only after the root executes `deferred_action` on a logic tick.
    pub requests: Vec<AgentSessionsRequest>,
    pub secret_intent: Option<AgentSessionsSecretIntent>,
    pub deferred_action: Option<AgentSessionsDeferredAction>,
}

/// One opaque controller action produced by a render frame. The value is deliberately non-Clone
/// and non-Serialize, so the root can retain only the latest single action instead of building a
/// background backlog. A newer render generation makes an unexecuted action stale.
pub struct AgentSessionsDeferredAction {
    generation: u64,
    action: PanelAction,
}

pub struct AgentSessionsFrameInput<'a> {
    pub workspace_id: &'a str,
    pub workspace_cwd: Option<String>,
    pub pty_surfaces: Vec<AgentSurfaceSnapshot>,
    pub agents_config: &'a mut AgentsConfig,
    pub secrets_snapshot: &'a AgentSessionsSecretsSnapshot,
    pub ollama_models: Option<&'a [String]>,
}

/// Storage-neutral projection of one persisted structured thread. The composition root maps its
/// concrete repository row into this DTO before the UI controller sees it.
#[derive(Clone, PartialEq, Eq)]
pub struct AgentSessionPersistedRow {
    pub local_session_id: String,
    pub workspace_id: String,
    pub thread_id: String,
    pub title: String,
    pub cwd: String,
    pub model: Option<String>,
    pub favorite: bool,
    pub archived: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

impl std::fmt::Debug for AgentSessionPersistedRow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentSessionPersistedRow")
            .field("local_session_id_bytes", &self.local_session_id.len())
            .field("workspace_id_bytes", &self.workspace_id.len())
            .field("thread_id_bytes", &self.thread_id.len())
            .field("title_bytes", &self.title.len())
            .field("cwd", &"[REDACTED]")
            .field("model_present", &self.model.is_some())
            .field("favorite", &self.favorite)
            .field("archived", &self.archived)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// Stable, data-free rejection codes for an authoritative persisted catalog replacement.
/// Invalid input is rejected before any controller state is changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSessionPersistedCatalogError {
    TooManyItems,
    RowTooLarge,
    TotalBytesExceeded,
    DuplicateLocalSession,
    DuplicateThread,
    InvalidLocalSession,
}

struct AgentSecretRender<'a> {
    snapshot: &'a AgentSessionsSecretsSnapshot,
    intent: &'a mut Option<AgentSessionsSecretIntent>,
}

impl AgentSessionsFrameOutput {
    fn empty() -> Self {
        Self {
            requests: Vec::new(),
            secret_intent: None,
            deferred_action: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSessionsSecretErrorCode {
    SnapshotUnavailable,
    InputLimitExceeded,
    InvalidInput,
    SaveFailed,
    DeleteFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApiKeyMutationKind {
    Save,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSessionStatusNotice {
    pub workspace_id: String,
    pub session_id: AgentSessionId,
    pub title: String,
    pub status: AgentSessionStatus,
}

/// fleet 뷰가 구조화(App Server) 세션을 카드로 그리기 위한 읽기전용 요약.
/// App이 fleet_rows()로 받아 FleetSession으로 투영한다(관찰 + 열기 전용).
#[derive(Debug, Clone, PartialEq)]
pub struct FleetStructuredRow {
    pub session_id: AgentSessionId,
    pub workspace_id: Option<String>,
    pub title: String,
    pub state: crate::agent_surface::AgentVisualState,
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSessionsRequest {
    RevealWorkspace(String),
    FocusPty(AgentSurfaceId),
    InterruptPty(AgentSurfaceId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CatalogMessage {
    Raw(String),
    Key(&'static str),
    Error { key: &'static str, detail: String },
}

impl CatalogMessage {
    fn raw(message: impl Into<String>) -> Self {
        Self::Raw(message.into())
    }

    fn error(key: &'static str, error: &anyhow::Error) -> Self {
        Self::Error {
            key,
            detail: format!("{error:#}"),
        }
    }

    fn render(&self, catalog: &i18n::Catalog) -> String {
        match self {
            Self::Raw(message) => message.clone(),
            Self::Key(key) => catalog.t(key, &[]),
            Self::Error { key, detail } => catalog.t(key, &[("error", detail)]),
        }
    }
}

/// Stable ID for the embedded Agents window.
///
/// Do not infer this from the visible title. Since egui 0.35, `Window::new`
/// accepts `IntoAtoms` and derives its default ID from the resulting optional
/// text value, which is not equivalent to `Id::new("Agents")`. The workspace
/// uses this exact ID to classify this window as non-modal for PTY focus.
pub(crate) fn agents_window_id() -> egui::Id {
    egui::Id::new("deppy_agents_window")
}

/// Storage mutations emitted by the structured-session controller. `App` owns
/// the `Db`, so it drains and executes these after the frame without exposing a
/// database connection to UI code.
#[derive(Clone, PartialEq, Eq)]
pub enum AgentSessionPersistenceMutation {
    Upsert {
        local_session_id: AgentSessionId,
        workspace_id: String,
        thread_id: String,
        title: String,
        cwd: String,
        model: Option<String>,
        favorite: bool,
        archived: bool,
    },
    SetArchived {
        local_session_id: AgentSessionId,
        archived: bool,
    },
    Delete {
        local_session_id: AgentSessionId,
    },
}

impl AgentSessionPersistenceMutation {
    fn local_session_id(&self) -> &str {
        match self {
            Self::Upsert {
                local_session_id, ..
            }
            | Self::SetArchived {
                local_session_id, ..
            }
            | Self::Delete { local_session_id } => local_session_id,
        }
    }

    fn retained_bytes(&self) -> usize {
        match self {
            Self::Upsert {
                local_session_id,
                workspace_id,
                thread_id,
                title,
                cwd,
                model,
                ..
            } => local_session_id
                .len()
                .saturating_add(workspace_id.len())
                .saturating_add(thread_id.len())
                .saturating_add(title.len())
                .saturating_add(cwd.len())
                .saturating_add(model.as_ref().map_or(0, String::len)),
            Self::SetArchived {
                local_session_id, ..
            }
            | Self::Delete { local_session_id } => local_session_id.len(),
        }
    }

    fn canonicalize(mut self) -> Result<Self, AgentSessionPersistenceBacklogError> {
        let valid_local_session =
            AgentSession::try_new(self.local_session_id().to_owned(), String::new(), None)
                .is_some();
        let valid_fields = match &self {
            Self::Upsert {
                workspace_id,
                thread_id,
                title,
                cwd,
                model,
                ..
            } => {
                !workspace_id.is_empty()
                    && !thread_id.is_empty()
                    && [workspace_id, thread_id, title, cwd]
                        .into_iter()
                        .all(|value| !value.as_bytes().contains(&0))
                    && model
                        .as_ref()
                        .is_none_or(|value| !value.as_bytes().contains(&0))
            }
            Self::SetArchived { .. } | Self::Delete { .. } => true,
        };
        if !valid_local_session || !valid_fields {
            return Err(AgentSessionPersistenceBacklogError::InvalidInput);
        }
        if self.retained_bytes() > AGENT_SESSION_PERSISTED_ROW_MAX_BYTES {
            return Err(AgentSessionPersistenceBacklogError::RowTooLarge);
        }
        match &mut self {
            Self::Upsert {
                local_session_id,
                workspace_id,
                thread_id,
                title,
                cwd,
                model,
                ..
            } => {
                canonicalize_persistence_string(local_session_id);
                canonicalize_persistence_string(workspace_id);
                canonicalize_persistence_string(thread_id);
                canonicalize_persistence_string(title);
                canonicalize_persistence_string(cwd);
                if let Some(model) = model {
                    canonicalize_persistence_string(model);
                }
            }
            Self::SetArchived {
                local_session_id, ..
            }
            | Self::Delete { local_session_id } => {
                canonicalize_persistence_string(local_session_id);
            }
        }
        Ok(self)
    }
}

impl std::fmt::Debug for AgentSessionPersistenceMutation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (kind, archived) = match self {
            Self::Upsert { archived, .. } => ("upsert", Some(*archived)),
            Self::SetArchived { archived, .. } => ("set_archived", Some(*archived)),
            Self::Delete { .. } => ("delete", None),
        };
        formatter
            .debug_struct("AgentSessionPersistenceMutation")
            .field("kind", &kind)
            .field("archived", &archived)
            .field("local_session_id", &"REDACTED")
            .finish()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AgentSessionPersistenceBacklogError {
    InvalidInput,
    TooManyItems,
    RowTooLarge,
    TotalBytesExceeded,
}

impl AgentSessionPersistenceBacklogError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::TooManyItems => "too_many_items",
            Self::RowTooLarge => "row_too_large",
            Self::TotalBytesExceeded => "total_bytes_exceeded",
        }
    }
}

impl std::fmt::Debug for AgentSessionPersistenceBacklogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::fmt::Display for AgentSessionPersistenceBacklogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::error::Error for AgentSessionPersistenceBacklogError {}

#[derive(Default)]
struct AgentSessionPersistenceBacklog {
    entries: Vec<AgentSessionPersistenceMutation>,
    retained_bytes: usize,
}

impl AgentSessionPersistenceBacklog {
    fn try_push(
        &mut self,
        incoming: AgentSessionPersistenceMutation,
    ) -> Result<(), AgentSessionPersistenceBacklogError> {
        let incoming = incoming.canonicalize()?;
        if let Some(index) = self
            .entries
            .iter()
            .position(|existing| existing.local_session_id() == incoming.local_session_id())
        {
            let candidate =
                coalesce_persistence_mutation(&self.entries[index], incoming).canonicalize()?;
            let previous_bytes = self.entries[index].retained_bytes();
            let candidate_bytes = candidate.retained_bytes();
            let next_bytes = self
                .retained_bytes
                .checked_sub(previous_bytes)
                .and_then(|bytes| bytes.checked_add(candidate_bytes))
                .ok_or(AgentSessionPersistenceBacklogError::TotalBytesExceeded)?;
            if next_bytes > AGENT_SESSION_PERSISTED_TOTAL_MAX_BYTES {
                return Err(AgentSessionPersistenceBacklogError::TotalBytesExceeded);
            }
            self.entries[index] = candidate;
            self.retained_bytes = next_bytes;
            return Ok(());
        }
        if self.entries.len() >= AGENT_SESSION_PERSISTED_MAX_ITEMS {
            return Err(AgentSessionPersistenceBacklogError::TooManyItems);
        }
        let next_bytes = self
            .retained_bytes
            .checked_add(incoming.retained_bytes())
            .ok_or(AgentSessionPersistenceBacklogError::TotalBytesExceeded)?;
        if next_bytes > AGENT_SESSION_PERSISTED_TOTAL_MAX_BYTES {
            return Err(AgentSessionPersistenceBacklogError::TotalBytesExceeded);
        }
        self.entries.push(incoming);
        self.retained_bytes = next_bytes;
        Ok(())
    }

    fn drain_bounded(&mut self, limit: usize) -> Vec<AgentSessionPersistenceMutation> {
        let take = self.entries.len().min(limit);
        if take == 0 {
            return Vec::new();
        }
        let drained = self.entries.drain(..take).collect::<Vec<_>>();
        self.retained_bytes = self
            .entries
            .iter()
            .map(AgentSessionPersistenceMutation::retained_bytes)
            .sum();
        canonicalize_persistence_vec(&mut self.entries);
        drained.into_boxed_slice().into_vec()
    }

    fn retain(&mut self, mut keep: impl FnMut(&AgentSessionPersistenceMutation) -> bool) {
        self.entries.retain(|mutation| keep(mutation));
        self.retained_bytes = self
            .entries
            .iter()
            .map(AgentSessionPersistenceMutation::retained_bytes)
            .sum();
        canonicalize_persistence_vec(&mut self.entries);
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

fn canonicalize_persistence_string(value: &mut String) {
    *value = std::mem::take(value).into_boxed_str().into_string();
}

fn canonicalize_persistence_vec(values: &mut Vec<AgentSessionPersistenceMutation>) {
    *values = std::mem::take(values).into_boxed_slice().into_vec();
}

fn coalesce_persistence_mutation(
    existing: &AgentSessionPersistenceMutation,
    incoming: AgentSessionPersistenceMutation,
) -> AgentSessionPersistenceMutation {
    match (existing, incoming) {
        (
            AgentSessionPersistenceMutation::Upsert { .. },
            AgentSessionPersistenceMutation::SetArchived { archived, .. },
        ) => {
            let mut candidate = existing.clone();
            if let AgentSessionPersistenceMutation::Upsert {
                archived: current, ..
            } = &mut candidate
            {
                *current = archived;
            }
            candidate
        }
        (
            AgentSessionPersistenceMutation::Delete { .. },
            AgentSessionPersistenceMutation::SetArchived { .. },
        ) => existing.clone(),
        (_, incoming) => incoming,
    }
}

enum PendingThreadRequest {
    Read {
        session_id: AgentSessionId,
        reply: CodexAppServerReply,
    },
    Resume {
        session_id: AgentSessionId,
        reply: CodexAppServerReply,
    },
    Archive {
        session_id: AgentSessionId,
        reply: CodexAppServerReply,
    },
}

impl PendingThreadRequest {
    fn session_id(&self) -> &str {
        match self {
            Self::Read { session_id, .. }
            | Self::Resume { session_id, .. }
            | Self::Archive { session_id, .. } => session_id,
        }
    }

    fn reply(&self) -> &CodexAppServerReply {
        match self {
            Self::Read { reply, .. } | Self::Resume { reply, .. } | Self::Archive { reply, .. } => {
                reply
            }
        }
    }
}

/// App-owned UI/controller for one shared local Codex App Server connection.
pub struct AgentSessionsUi {
    open: bool,
    client: Option<CodexAppServerClient>,
    sessions: Vec<AgentSession>,
    pty_surfaces: Vec<AgentSurfaceSnapshot>,
    selected_surface: Option<AgentSurfaceId>,
    selected_session: Option<AgentSessionId>,
    selected_item: Option<String>,
    new_prompt: String,
    new_model: String,
    new_effort: String,
    /// OSS 섹션 ⟳ 클릭 — App이 take해 ollama 모델 재감지를 돌린다(2026-07-18).
    ollama_redetect_requested: bool,
    follow_up: String,
    steer_input: String,
    focus_new_prompt: bool,
    focus_follow_up: bool,
    status_notices: Vec<AgentSessionStatusNotice>,
    transport_error: Option<CatalogMessage>,
    persisted_threads: HashMap<AgentSessionId, AgentSessionPersistedRow>,
    persisted_thread_bytes: usize,
    /// Exact sessions synthesized only as DB catalog placeholders. Runtime admission or the first
    /// runtime-owned event removes the ID, so a later authoritative DB snapshot can never erase a
    /// live session merely because both happen to be in `Stopped` state.
    persisted_placeholders: HashSet<AgentSessionId>,
    attached_threads: HashSet<AgentSessionId>,
    pending_thread_requests: Vec<PendingThreadRequest>,
    persistence_backlog: AgentSessionPersistenceBacklog,
    model_catalog: Vec<CodexModelInfo>,
    skill_catalog: Vec<CodexSkillInfo>,
    selected_skill_paths: HashSet<String>,
    pending_model_catalog: Option<CodexModelCatalogReply>,
    pending_skill_catalog: Option<CodexSkillCatalogReply>,
    pending_rate_limits: Option<CodexAppServerReply>,
    codex_usage: Option<crate::app::ProviderUsage>,
    codex_usage_meta: Option<CodexUsageMeta>,
    last_rate_limits_request: Option<std::time::Instant>,
    catalog_error: Option<CatalogMessage>,
    text_input_ids: Vec<egui::Id>,
    /// config에서 동기화한 LLM 프로바이더 오버라이드 (PR-L2) — ensure_client가 spawn 시
    /// 사용한다. Err = 잘못된 설정(예: custom인데 base URL 없음) — spawn을 명확히 중단.
    llm_override: Result<Option<CodexLlmOverride>, String>,
    /// 현재 client가 spawn될 때 적용한 오버라이드 — 설정 변경 시 유휴 재시작 판단용.
    client_llm_override: Option<CodexLlmOverride>,
    /// Production app-server/process host. The composition root resolves authentication.
    app_server_host: Option<Arc<dyn CodexAppServerHost>>,
    /// API 키 입력 버퍼 (password 렌더). 저장 성공 시 즉시 비운다.
    api_key_input: String,
    api_key_input_overflowed: bool,
    api_key_pending: Option<ApiKeyMutationKind>,
    api_key_error: Option<AgentSessionsSecretErrorCode>,
    api_key_snapshot_revision: Option<u64>,
    /// 키 저장/삭제 세대. client가 spawn 시 기록한 세대와 다르고 유휴면
    /// sync_llm_config가 재시작해 다음 spawn부터 새 키를 반영한다.
    api_key_generation: u64,
    /// 현재 client가 spawn될 때의 키 세대 (프로바이더 오버라이드 비교와 동일 관례).
    client_api_key_generation: u64,
    /// 비동기 poll·단축키 경로에서도 현재 UI 언어로 오류를 만들기 위한 catalog snapshot.
    /// App 생성 시 주입하고 locale 변경 시 즉시 갱신한다.
    /// Arc인 이유: show()는 이 패널이 보이는 매 프레임 도는데, 내부에서 &mut self를
    /// 쓰는 렌더 헬퍼들에 catalog를 넘기려면 self.catalog를 그대로 들고 있을 수 없어
    /// 매 프레임 clone해야 한다. 값 타입이면 로케일당 ~1,000개 항목짜리 BTreeMap
    /// 2개를 매 프레임 딥카피하게 되므로, clone 비용을 참조 카운트 증가로 낮춘다
    /// (2026-08-14).
    catalog: Arc<i18n::Catalog>,
    /// UI가 만든 session 오류만 원문 detail+catalog key로 보존해 locale 변경 시 재렌더한다.
    localized_session_errors: HashMap<AgentSessionId, CatalogMessage>,
    /// Latest render generation for exact stale-action rejection.
    frame_generation: u64,
}

impl AgentSessionsUi {
    pub fn new() -> Self {
        Self {
            open: false,
            client: None,
            sessions: Vec::new(),
            pty_surfaces: Vec::new(),
            selected_surface: None,
            selected_session: None,
            selected_item: None,
            new_prompt: String::new(),
            new_model: String::new(),
            new_effort: String::new(),
            ollama_redetect_requested: false,
            follow_up: String::new(),
            steer_input: String::new(),
            focus_new_prompt: false,
            focus_follow_up: false,
            status_notices: Vec::new(),
            transport_error: None,
            persisted_threads: HashMap::new(),
            persisted_thread_bytes: 0,
            persisted_placeholders: HashSet::new(),
            attached_threads: HashSet::new(),
            pending_thread_requests: Vec::new(),
            persistence_backlog: AgentSessionPersistenceBacklog::default(),
            model_catalog: Vec::new(),
            skill_catalog: Vec::new(),
            selected_skill_paths: HashSet::new(),
            pending_model_catalog: None,
            pending_skill_catalog: None,
            pending_rate_limits: None,
            codex_usage: None,
            codex_usage_meta: None,
            last_rate_limits_request: None,
            catalog_error: None,
            text_input_ids: Vec::new(),
            llm_override: Ok(None),
            client_llm_override: None,
            app_server_host: None,
            api_key_input: String::new(),
            api_key_input_overflowed: false,
            api_key_pending: None,
            api_key_error: None,
            api_key_snapshot_revision: None,
            api_key_generation: 0,
            client_api_key_generation: 0,
            catalog: Arc::new(
                i18n::Catalog::load(i18n::FALLBACK_LOCALE)
                    .expect("fallback locale catalog must load"),
            ),
            localized_session_errors: HashMap::new(),
            frame_generation: 0,
        }
    }

    /// App wiring: inject the root-owned process/authentication host.
    pub fn with_app_server_host(mut self, host: Arc<dyn CodexAppServerHost>) -> Self {
        self.app_server_host = Some(host);
        self
    }

    pub fn api_key_save_succeeded(&mut self) {
        self.api_key_pending = None;
        self.api_key_error = None;
        self.api_key_generation = self.api_key_generation.saturating_add(1);
    }

    pub fn api_key_delete_succeeded(&mut self) {
        self.api_key_pending = None;
        self.api_key_error = None;
        self.api_key_generation = self.api_key_generation.saturating_add(1);
    }

    pub fn report_api_key_error(&mut self, code: AgentSessionsSecretErrorCode) {
        self.api_key_pending = None;
        self.api_key_error = Some(code);
    }

    /// App 생성·locale hot reload 시 비동기 poll보다 먼저 현재 catalog를 주입한다.
    pub fn with_catalog(mut self, catalog: &i18n::Catalog) -> Self {
        self.set_catalog(catalog);
        self
    }

    pub fn set_catalog(&mut self, catalog: &i18n::Catalog) {
        if self.catalog.locale() != catalog.locale() {
            self.catalog = Arc::new(catalog.clone());
            let synthesized_titles = self
                .persisted_threads
                .iter()
                .filter(|(_, row)| row.title.trim().is_empty())
                .map(|(session_id, row)| (session_id.clone(), persisted_title(row, &self.catalog)))
                .collect::<HashMap<_, _>>();
            for session in &mut self.sessions {
                if let Some(title) = synthesized_titles.get(&session.id) {
                    session.prompt = title.clone();
                }
                if let Some(error) = self.localized_session_errors.get(&session.id) {
                    session.error = Some(error.render(&self.catalog));
                }
            }
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// OSS 섹션 ⟳(모델 재감지) 클릭을 소비한다 — App이 프레임마다 확인.
    pub fn take_ollama_redetect(&mut self) -> bool {
        std::mem::take(&mut self.ollama_redetect_requested)
    }

    /// A terminal click is an explicit keyboard-ownership transfer. Cancel any
    /// deferred Agents autofocus and release the exact Agents TextEdit if it
    /// still owns egui focus; never clear an unrelated widget's focus.
    pub fn surrender_text_focus(&mut self, ctx: &egui::Context) -> bool {
        self.focus_new_prompt = false;
        self.focus_follow_up = false;
        let focused = ctx.memory(|memory| memory.focused());
        let Some(focused) = focused.filter(|id| self.text_input_ids.contains(id)) else {
            return false;
        };
        ctx.memory_mut(|memory| memory.surrender_focus(focused));
        true
    }

    pub fn open(&mut self) {
        self.open = true;
    }

    pub fn session_ids(&self) -> Vec<String> {
        self.sessions
            .iter()
            .map(|session| session.id.clone())
            .collect()
    }

    /// fleet 뷰용 라이브 구조화(App Server) 세션 요약. Off(중단/종료)는 제외해 "지금
    /// 살아있는" 에이전트만 담는다. 관찰 + 열기 전용(브로드캐스트 대상 아님).
    ///
    /// 주의 — PTY 카드와 "off" 의미가 다르다(의도된 비대칭, 2026-07-25 판정): 여기의
    /// Off(Stopped)는 **DB 복원 placeholder 전부**(import/replace_persisted_threads가
    /// Stopped로 하드코딩)를 포함하는 "확실히 죽음/보관"이라, 필터하지 않으면 재시작마다
    /// 죽은 스레드가 fleet를 뒤덮는다. PTY의 Off(status=None)는 스폰 직후 미분류(살아있음)
    /// 라 반대로 표시가 맞다 — 대칭으로 "고치지" 말 것(app.rs build_fleet_sessions 참고).
    /// 승인 대기 중인 구조화 세션 id — App이 「막힌 시각」을 추적하는 데 쓴다.
    /// 선택 상태와 무관하게 전부 돌려준다(히어로는 가장 오래 막힌 것을 고른다).
    pub fn awaiting_approval_ids(&self) -> impl Iterator<Item = &str> {
        self.sessions
            .iter()
            .filter(|session| session.status == AgentSessionStatus::AwaitingApproval)
            .map(|session| session.id.as_str())
    }

    pub fn fleet_rows(&self) -> Vec<FleetStructuredRow> {
        self.sessions
            .iter()
            .filter_map(|session| {
                let state = crate::agent_surface::AgentVisualState::from_structured(session.status);
                if state == crate::agent_surface::AgentVisualState::Off {
                    return None;
                }
                Some(FleetStructuredRow {
                    session_id: session.id.clone(),
                    workspace_id: session.workspace_id.clone(),
                    // 기존 catalog-aware 헬퍼 재사용(빈 프롬프트 fallback도 i18n 처리 —
                    // agent_sessions.default_task). 병렬 리뷰 Medium 반영.
                    title: one_line_title(&session.prompt, &self.catalog),
                    state,
                    model: session.model.clone(),
                })
            })
            .collect()
    }

    pub fn drain_status_notices(&mut self) -> Vec<AgentSessionStatusNotice> {
        std::mem::take(&mut self.status_notices)
    }

    /// Project persisted metadata into lightweight, selectable APP rows. This
    /// does not spawn Codex: the one shared process is started only when the
    /// user explicitly reads or resumes a row.
    #[allow(dead_code)] // App wiring drains DB rows after this ownership phase.
    pub fn import_persisted_threads(&mut self, rows: Vec<AgentSessionPersistedRow>) {
        for row in rows {
            let local_session_id = row.local_session_id.clone();
            let duplicate_thread = self.persisted_threads.iter().any(|(id, existing)| {
                id != &local_session_id && existing.thread_id == row.thread_id
            });
            if duplicate_thread {
                continue;
            }
            // Reject a corrupt/hostile storage row before title/cwd cloning or
            // constructing a placeholder session.
            let row_bytes = persisted_row_retained_bytes(&row);
            let previous_row_bytes = self
                .persisted_threads
                .get(&local_session_id)
                .map_or(0, persisted_row_retained_bytes);
            let next_retained_bytes = self
                .persisted_thread_bytes
                .saturating_sub(previous_row_bytes)
                .saturating_add(row_bytes);
            if row_bytes > AGENT_SESSION_PERSISTED_ROW_MAX_BYTES
                || next_retained_bytes > AGENT_SESSION_PERSISTED_TOTAL_MAX_BYTES
                || (!self.persisted_threads.contains_key(&local_session_id)
                    && self.persisted_threads.len() >= AGENT_SESSION_PERSISTED_MAX_ITEMS)
            {
                continue;
            }

            let was_persisted = self.persisted_threads.contains_key(&local_session_id);
            let existing_session_index = self
                .sessions
                .iter()
                .position(|session| session.id == local_session_id);
            let pending_session = if existing_session_index.is_none() {
                let Some(session) = AgentSession::try_new(
                    local_session_id.clone(),
                    persisted_title(&row, &self.catalog),
                    non_empty(row.cwd.clone()),
                ) else {
                    continue;
                };
                Some(session)
            } else {
                None
            };
            if !self.store_persisted_row(row.clone()) {
                continue;
            }
            if let Some(session_index) = existing_session_index {
                let session = &mut self.sessions[session_index];
                // A repeated DB projection may refresh a placeholder, but it
                // must never regress a live session that already owns events.
                if was_persisted && self.persisted_placeholders.contains(&local_session_id) {
                    apply_persisted_metadata(session, &row, &self.catalog);
                }
                continue;
            }

            let mut session = pending_session.expect("validated missing persisted session");
            apply_persisted_metadata(&mut session, &row, &self.catalog);
            session.status = AgentSessionStatus::Stopped;
            self.sessions.push(session);
            self.persisted_placeholders.insert(local_session_id);
        }
    }

    /// Atomically replaces the complete, multi-workspace persisted-thread catalog.
    ///
    /// The caller must provide an authoritative snapshot, not a page. Every limit and duplicate
    /// invariant is validated before mutation. Rows absent from the snapshot lose their persisted
    /// projection; only UI-synthesized placeholders are removed from `sessions`. Runtime-owned,
    /// attached, in-flight, or event-bearing sessions are retained unchanged.
    pub fn replace_persisted_threads(
        &mut self,
        rows: Vec<AgentSessionPersistedRow>,
    ) -> Result<(), AgentSessionPersistedCatalogError> {
        if rows.len() > AGENT_SESSION_PERSISTED_MAX_ITEMS {
            return Err(AgentSessionPersistedCatalogError::TooManyItems);
        }

        let mut local_ids = HashSet::with_capacity(rows.len());
        let mut thread_ids = HashSet::with_capacity(rows.len());
        let mut retained_bytes = 0usize;
        for row in &rows {
            let row_bytes = persisted_row_retained_bytes(row);
            if row_bytes > AGENT_SESSION_PERSISTED_ROW_MAX_BYTES {
                return Err(AgentSessionPersistedCatalogError::RowTooLarge);
            }
            retained_bytes = retained_bytes
                .checked_add(row_bytes)
                .ok_or(AgentSessionPersistedCatalogError::TotalBytesExceeded)?;
            if retained_bytes > AGENT_SESSION_PERSISTED_TOTAL_MAX_BYTES {
                return Err(AgentSessionPersistedCatalogError::TotalBytesExceeded);
            }
            if !local_ids.insert(row.local_session_id.as_str()) {
                return Err(AgentSessionPersistedCatalogError::DuplicateLocalSession);
            }
            if !thread_ids.insert(row.thread_id.as_str()) {
                return Err(AgentSessionPersistedCatalogError::DuplicateThread);
            }
            if AgentSession::try_new(row.local_session_id.clone(), String::new(), None).is_none() {
                return Err(AgentSessionPersistedCatalogError::InvalidLocalSession);
            }
        }

        let next_rows = rows
            .into_iter()
            .map(|row| (row.local_session_id.clone(), row))
            .collect::<HashMap<_, _>>();
        let existing_ids = self
            .sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect::<HashSet<_>>();
        let mut new_placeholders = Vec::new();
        for row in next_rows
            .values()
            .filter(|row| !existing_ids.contains(row.local_session_id.as_str()))
        {
            let mut session = AgentSession::try_new(
                row.local_session_id.clone(),
                persisted_title(row, &self.catalog),
                non_empty(row.cwd.clone()),
            )
            .ok_or(AgentSessionPersistedCatalogError::InvalidLocalSession)?;
            apply_persisted_metadata(&mut session, row, &self.catalog);
            session.status = AgentSessionStatus::Stopped;
            new_placeholders.push(session);
        }

        // Defensive ownership promotion: even if a caller staged a runtime request directly, a
        // placeholder with live state must become runtime-owned before stale catalog rows prune.
        let pending_ids = self
            .pending_thread_requests
            .iter()
            .map(PendingThreadRequest::session_id)
            .collect::<HashSet<_>>();
        let promoted = self
            .sessions
            .iter()
            .filter(|session| {
                self.persisted_placeholders.contains(&session.id)
                    && (self.attached_threads.contains(&session.id)
                        || pending_ids.contains(session.id.as_str())
                        || session_has_runtime_state(session))
            })
            .map(|session| session.id.clone())
            .collect::<HashSet<_>>();
        self.persisted_placeholders
            .retain(|session_id| !promoted.contains(session_id));

        let removed_placeholders = self
            .persisted_placeholders
            .iter()
            .filter(|session_id| !next_rows.contains_key(session_id.as_str()))
            .cloned()
            .collect::<HashSet<_>>();
        self.sessions
            .retain(|session| !removed_placeholders.contains(&session.id));
        self.persisted_placeholders
            .retain(|session_id| !removed_placeholders.contains(session_id));
        self.localized_session_errors
            .retain(|session_id, _| !removed_placeholders.contains(session_id));

        for session in &mut self.sessions {
            if self.persisted_placeholders.contains(&session.id)
                && let Some(row) = next_rows.get(&session.id)
            {
                apply_persisted_metadata(session, row, &self.catalog);
                session.status = AgentSessionStatus::Stopped;
            }
        }
        for session in new_placeholders {
            self.persisted_placeholders.insert(session.id.clone());
            self.sessions.push(session);
        }

        self.persisted_threads = next_rows;
        self.persisted_thread_bytes = retained_bytes;
        if self
            .selected_session
            .as_ref()
            .is_some_and(|selected| removed_placeholders.contains(selected))
        {
            self.selected_session = None;
            self.selected_surface = None;
            self.selected_item = None;
            self.follow_up.clear();
            self.steer_input.clear();
        }
        Ok(())
    }

    /// Drain the oldest independent final-state mutations. Each local session occupies at most one
    /// bounded slot; repeated changes are coalesced before App observes them.
    pub fn drain_persistence_mutations_bounded(
        &mut self,
        limit: usize,
    ) -> Vec<AgentSessionPersistenceMutation> {
        self.persistence_backlog.drain_bounded(limit)
    }

    pub fn pending_persistence_mutation_count(&self) -> usize {
        self.persistence_backlog.len()
    }

    /// Surface App-owned database failures in the existing Agents error area.
    /// Mutation execution and any retry policy remain with `App`.
    #[allow(dead_code)] // Called by the App-level Db mutation executor.
    pub fn report_persistence_error(&mut self, message: String) {
        self.transport_error = Some(CatalogMessage::raw(message));
    }

    fn queue_persistence_mutation(
        &mut self,
        mutation: AgentSessionPersistenceMutation,
    ) -> Result<(), AgentSessionPersistenceBacklogError> {
        self.persistence_backlog
            .try_push(mutation)
            .inspect_err(|error| {
                self.transport_error = Some(CatalogMessage::raw(format!(
                    "structured_persistence_{}",
                    error.as_str()
                )));
            })
    }

    pub fn read_selected_persisted(&mut self, ctx: &egui::Context) -> anyhow::Result<()> {
        let (session_id, row) = self.selected_persisted_thread()?;
        self.ensure_client(ctx)?;
        let reply = self
            .client
            .as_ref()
            .expect("ensure_client 성공 후 client 존재")
            .read_thread(row.thread_id, true)?;
        self.mark_history_request_started(&session_id);
        self.pending_thread_requests
            .push(PendingThreadRequest::Read { session_id, reply });
        Ok(())
    }

    pub fn resume_selected_persisted(&mut self, ctx: &egui::Context) -> anyhow::Result<()> {
        let (session_id, row) = self.selected_persisted_thread()?;
        self.ensure_client(ctx)?;
        let reply = self
            .client
            .as_ref()
            .expect("ensure_client 성공 후 client 존재")
            .resume_thread(
                session_id.clone(),
                row.thread_id,
                non_empty(row.cwd),
                row.model,
            )?;
        self.mark_history_request_started(&session_id);
        self.pending_thread_requests
            .push(PendingThreadRequest::Resume { session_id, reply });
        Ok(())
    }

    pub fn archive_selected_persisted(&mut self, ctx: &egui::Context) -> anyhow::Result<()> {
        let (session_id, row) = self.selected_persisted_thread()?;
        anyhow::ensure!(
            !self.attached_threads.contains(&session_id),
            self.catalog
                .t("agent_sessions.error.archive_attached_thread", &[])
        );
        self.ensure_client(ctx)?;
        let reply = self
            .client
            .as_ref()
            .expect("ensure_client 성공 후 client 존재")
            .archive_thread(row.thread_id)?;
        if let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
        {
            session.error = None;
        }
        self.pending_thread_requests
            .push(PendingThreadRequest::Archive { session_id, reply });
        Ok(())
    }

    /// Delete only Deppy's local recovery projection. Remote Codex history is
    /// retained unless the user separately archives it first.
    pub fn delete_selected_persisted(&mut self) -> anyhow::Result<()> {
        let (session_id, _) = self.selected_persisted_thread()?;
        anyhow::ensure!(
            !self.attached_threads.contains(&session_id),
            self.catalog
                .t("agent_sessions.error.delete_attached_thread", &[])
        );
        self.queue_persistence_mutation(AgentSessionPersistenceMutation::Delete {
            local_session_id: session_id.clone(),
        })?;
        self.remove_persisted_row(&session_id);
        self.sessions.retain(|session| session.id != session_id);
        self.localized_session_errors.remove(&session_id);
        self.pending_thread_requests
            .retain(|pending| pending.session_id() != session_id);
        if self.selected_session.as_deref() == Some(session_id.as_str()) {
            self.selected_session = None;
            self.selected_surface = None;
            self.selected_item = None;
            self.follow_up.clear();
        }
        Ok(())
    }

    /// Fail-closed guard called before App deletes a workspace and cascades its
    /// storage rows. An attached or in-flight structured thread keeps the
    /// workspace alive; detached projections can be discarded safely.
    #[allow(dead_code)] // Called by the App-level workspace deletion path.
    pub fn prepare_workspace_delete(&mut self, workspace_id: &str) -> anyhow::Result<()> {
        let target_ids = self
            .sessions
            .iter()
            .filter(|session| session.workspace_id.as_deref() == Some(workspace_id))
            .map(|session| session.id.clone())
            .collect::<HashSet<_>>();
        anyhow::ensure!(
            !target_ids
                .iter()
                .any(|session_id| self.attached_threads.contains(session_id)),
            self.catalog
                .t("agent_sessions.error.workspace_has_attached_thread", &[])
        );
        anyhow::ensure!(
            !self
                .pending_thread_requests
                .iter()
                .any(|pending| target_ids.contains(pending.session_id())),
            self.catalog
                .t("agent_sessions.error.workspace_has_pending_thread", &[])
        );

        self.sessions
            .retain(|session| !target_ids.contains(&session.id));
        self.persisted_threads
            .retain(|session_id, _| !target_ids.contains(session_id));
        self.persisted_placeholders
            .retain(|session_id| !target_ids.contains(session_id));
        self.persisted_thread_bytes = self
            .persisted_threads
            .values()
            .map(persisted_row_retained_bytes)
            .sum();
        self.localized_session_errors
            .retain(|session_id, _| !target_ids.contains(session_id));
        self.status_notices
            .retain(|notice| notice.workspace_id != workspace_id);
        self.persistence_backlog.retain(|mutation| match mutation {
            AgentSessionPersistenceMutation::Upsert {
                workspace_id: id, ..
            } => id != workspace_id,
            AgentSessionPersistenceMutation::SetArchived {
                local_session_id, ..
            }
            | AgentSessionPersistenceMutation::Delete { local_session_id } => {
                !target_ids.contains(local_session_id)
            }
        });
        if self
            .selected_session
            .as_ref()
            .is_some_and(|selected| target_ids.contains(selected))
        {
            self.selected_session = None;
            self.selected_surface = None;
            self.selected_item = None;
            self.follow_up.clear();
            self.steer_input.clear();
        }
        Ok(())
    }

    pub fn open_session(&mut self, session_id: &str) -> bool {
        if !self.sessions.iter().any(|session| session.id == session_id) {
            return false;
        }
        self.open = true;
        self.selected_session = Some(session_id.to_owned());
        self.selected_surface = Some(AgentSurfaceId::Structured {
            session_id: session_id.to_owned(),
        });
        self.selected_item = None;
        self.follow_up.clear();
        true
    }

    /// App Server가 지금 writer로 붙잡고 있는(attach) codex thread 중 `thread_id`와
    /// 일치하는 것의 local(App 소유) 세션 id를 찾는다. PTY 쪽 「이어서 하기」가 같은
    /// thread를 또 열어 codex writer 충돌(`already has an active writer`, JSON-RPC
    /// -32600)을 만들지 않도록 app.rs가 PTY resume을 만들기 전에 조회한다.
    pub fn attached_local_session_for_thread(&self, thread_id: &str) -> Option<&str> {
        self.attached_threads.iter().find_map(|session_id| {
            self.persisted_threads
                .get(session_id)
                .filter(|row| row.thread_id == thread_id)
                .map(|_| session_id.as_str())
        })
    }

    /// 위 조회로 충돌은 찾았지만(`attached_local_session_for_thread`가 Some을 돌려줬지만)
    /// 그 local 세션을 열 수 없는 드문 레이스용 — 원문 codex 에러 대신 이해할 수 있는
    /// 안내를 보여준다. 패널도 함께 연다(닫혀 있으면 안내가 보이지 않는다).
    pub fn report_thread_attached_elsewhere(&mut self) {
        self.open = true;
        self.transport_error = Some(CatalogMessage::Key(
            "agent_sessions.error.thread_attached_elsewhere",
        ));
    }

    pub fn selected_surface_snapshot(&self) -> Option<AgentSurfaceSnapshot> {
        match self.selected_surface.as_ref()? {
            AgentSurfaceId::Pty { .. } => self
                .pty_surfaces
                .iter()
                .find(|surface| surface.id == *self.selected_surface.as_ref().expect("selected"))
                .cloned(),
            AgentSurfaceId::Structured { session_id } => {
                let session = self
                    .sessions
                    .iter()
                    .find(|session| &session.id == session_id)?;
                Some(AgentSurfaceSnapshot {
                    id: AgentSurfaceId::Structured {
                        session_id: session.id.clone(),
                    },
                    provider: AgentProvider::Codex,
                    transport: AgentTransport::AppServer,
                    title: one_line_title(&session.prompt, &self.catalog),
                    model: session.model.clone(),
                    effort: session.effort.clone(),
                    context_pct: None,
                    state: AgentVisualState::from_structured(session.status),
                    pty_status: None,
                })
            }
        }
    }

    pub fn selected_pending_approval_count(&self) -> usize {
        let Some(AgentSurfaceId::Structured { session_id }) = &self.selected_surface else {
            return 0;
        };
        self.sessions
            .iter()
            .find(|session| &session.id == session_id)
            .map_or(0, |session| session.approvals.len())
    }

    pub fn select_relative(&mut self, delta: isize) -> Option<AgentSurfaceId> {
        let ids = self.surface_ids();
        if ids.is_empty() {
            self.selected_surface = None;
            self.selected_session = None;
            return None;
        }
        let next = self
            .selected_surface
            .as_ref()
            .and_then(|selected| ids.iter().position(|id| id == selected))
            .map_or_else(
                || if delta < 0 { ids.len() - 1 } else { 0 },
                |current| (current as isize + delta).rem_euclid(ids.len() as isize) as usize,
            );
        let selected = ids[next].clone();
        self.select_surface(selected.clone());
        self.open = true;
        Some(selected)
    }

    pub fn open_new_prompt(&mut self) {
        self.open = true;
        self.focus_new_prompt = true;
    }

    pub fn focus_selected_input(&mut self) -> Option<AgentSessionsRequest> {
        self.open = true;
        match self.selected_surface.clone()? {
            id @ AgentSurfaceId::Pty { .. } => Some(AgentSessionsRequest::FocusPty(id)),
            AgentSurfaceId::Structured { session_id } => {
                let has_thread = self
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .is_some_and(|session| session.thread_id.is_some())
                    && (!self.persisted_threads.contains_key(&session_id)
                        || self.attached_threads.contains(&session_id));
                if has_thread {
                    self.focus_follow_up = true;
                } else {
                    self.focus_new_prompt = true;
                }
                None
            }
        }
    }

    pub fn interrupt_selected(
        &mut self,
        ctx: &egui::Context,
    ) -> anyhow::Result<Option<AgentSessionsRequest>> {
        let Some(selected) = self.selected_surface.clone() else {
            anyhow::bail!(
                self.catalog
                    .t("agent_sessions.error.no_selected_agent", &[])
            );
        };
        match selected {
            id @ AgentSurfaceId::Pty { .. } => Ok(Some(AgentSessionsRequest::InterruptPty(id))),
            AgentSurfaceId::Structured { session_id } => {
                self.client
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!(self.catalog.t("agent_sessions.error.no_app_server", &[]))
                    })?
                    .interrupt(session_id)?;
                ctx.request_repaint();
                Ok(None)
            }
        }
    }

    pub fn approve_selected_once(&mut self, ctx: &egui::Context) -> anyhow::Result<()> {
        self.respond_selected_approval(AgentApprovalDecision::Accept, ctx)
    }

    pub fn reject_selected(&mut self, ctx: &egui::Context) -> anyhow::Result<()> {
        self.respond_selected_approval(AgentApprovalDecision::Decline, ctx)
    }

    /// Cycle the selected APP session's next-turn effort using the authoritative
    /// model catalog. This is local state only; it is sent on the next
    /// `turn/start` and never uses the experimental thread settings API.
    pub fn adjust_selected_effort(&mut self, delta: isize) -> anyhow::Result<()> {
        anyhow::ensure!(
            delta != 0,
            self.catalog
                .t("agent_sessions.error.zero_effort_delta", &[])
        );
        let Some(AgentSurfaceId::Structured { session_id }) = self.selected_surface.as_ref() else {
            anyhow::bail!(
                self.catalog
                    .t("agent_sessions.error.no_selected_app_session", &[])
            );
        };
        let session = self
            .sessions
            .iter()
            .find(|session| &session.id == session_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    self.catalog
                        .t("agent_sessions.error.selected_app_session_missing", &[])
                )
            })?;
        let model = session
            .model
            .as_deref()
            .and_then(|selected| {
                self.model_catalog
                    .iter()
                    .find(|model| model.model == selected || model.id == selected)
            })
            .or_else(|| self.model_catalog.iter().find(|model| model.is_default))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    self.catalog
                        .t("agent_sessions.error.load_model_catalog_first", &[])
                )
            })?;
        anyhow::ensure!(
            !model.supported_reasoning_efforts.is_empty(),
            self.catalog
                .t("agent_sessions.error.model_has_no_efforts", &[])
        );
        let current = session
            .effort
            .as_deref()
            .unwrap_or(&model.default_reasoning_effort);
        let current_index = model
            .supported_reasoning_efforts
            .iter()
            .position(|effort| effort.reasoning_effort == current)
            .or_else(|| {
                model
                    .supported_reasoning_efforts
                    .iter()
                    .position(|effort| effort.reasoning_effort == model.default_reasoning_effort)
            })
            .unwrap_or(0);
        let next = (current_index as isize + delta)
            .rem_euclid(model.supported_reasoning_efforts.len() as isize)
            as usize;
        let next_effort = model.supported_reasoning_efforts[next]
            .reasoning_effort
            .clone();
        self.sessions
            .iter_mut()
            .find(|session| &session.id == session_id)
            .expect("위에서 검증된 APP 세션")
            .effort = Some(next_effort);
        Ok(())
    }

    fn respond_selected_approval(
        &mut self,
        decision: AgentApprovalDecision,
        ctx: &egui::Context,
    ) -> anyhow::Result<()> {
        let Some(AgentSurfaceId::Structured { session_id }) = self.selected_surface.clone() else {
            anyhow::bail!(
                self.catalog
                    .t("agent_sessions.error.no_selected_app_agent", &[])
            );
        };
        self.respond_approval_for_session(&session_id, decision, ctx)
    }

    /// 세션 id로 승인에 응답한다 — **선택 상태와 무관하게**.
    ///
    /// 「작업」 페이지의 히어로 카드는 지금 선택된 세션이 아니라 **가장 오래 막힌** 세션을
    /// 처리한다. 예전에는 선택 기반 경로뿐이라 히어로에서 구조화 세션 승인을 못 했다
    /// (2026-08-08). 선택을 몰래 바꾸는 대신 id 경로를 열어, Agents 패널의 선택은
    /// 건드리지 않는다.
    pub fn respond_approval_for_session(
        &mut self,
        session_id: &str,
        decision: AgentApprovalDecision,
        ctx: &egui::Context,
    ) -> anyhow::Result<()> {
        let session_id = session_id.to_owned();
        let session = self
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    self.catalog
                        .t("agent_sessions.error.selected_app_session_missing", &[])
                )
            })?;
        if session.approvals.len() != 1 {
            anyhow::bail!(self.catalog.t(
                "agent_sessions.error.approval_count",
                &[("count", &session.approvals.len().to_string())],
            ));
        }
        let request_key = session.approvals[0].request_key.clone();
        self.client
            .as_ref()
            .ok_or_else(|| {
                anyhow::anyhow!(self.catalog.t("agent_sessions.error.no_app_server", &[]))
            })?
            .respond_approval(session_id, request_key, decision)?;
        ctx.request_repaint();
        Ok(())
    }

    pub fn shutdown(&mut self) {
        if let Some(client) = self.client.as_mut() {
            client.shutdown();
        }
        // The joined worker may have produced final turn/thread events and
        // one-shot replies immediately before acknowledging Shutdown. Drain
        // them while the receivers are still owned so App can flush any newly
        // emitted persistence mutations once more after this call.
        self.poll();
        self.client = None;
        self.attached_threads.clear();
    }

    fn ensure_client(&mut self, ctx: &egui::Context) -> anyhow::Result<()> {
        if self.client.is_some() {
            return Ok(());
        }
        // 잘못된 프로바이더 설정으로는 spawn하지 않는다 — 기본 프로바이더로 조용히
        // 폴백하면 사용자가 명시한 로컬 LLM 선택이 무력화된다 (PR-L2).
        let llm_override = match &self.llm_override {
            Ok(value) => value.clone(),
            Err(message) => anyhow::bail!(self.catalog.t(
                "agent_sessions.error.provider_config",
                &[("error", message)],
            )),
        };
        let api_key_generation = self.api_key_generation;
        let host = self.app_server_host.as_ref().ok_or_else(|| {
            anyhow::anyhow!(self.catalog.t("agent_sessions.error.no_app_server", &[]))
        })?;
        let client = host
            .spawn(llm_override.clone(), ctx.clone())
            .map_err(|error| {
                if matches!(llm_override, Some(CodexLlmOverride::Custom { .. })) {
                    anyhow::anyhow!(localized_error(
                        &self.catalog,
                        "agent_sessions.error.api_key_load",
                        &error,
                    ))
                } else {
                    error
                }
            })?;
        self.transport_error = None;
        self.client = Some(client);
        self.client_llm_override = llm_override;
        self.client_api_key_generation = api_key_generation;
        Ok(())
    }

    pub fn refresh_rate_limits(&mut self, ctx: &egui::Context) {
        if self.pending_rate_limits.is_some()
            || self
                .last_rate_limits_request
                .is_some_and(|last| last.elapsed() < std::time::Duration::from_secs(60))
        {
            return;
        }
        self.last_rate_limits_request = Some(std::time::Instant::now());
        if self.ensure_client(ctx).is_err() {
            return;
        }
        self.pending_rate_limits = self
            .client
            .as_ref()
            .and_then(|client| client.read_rate_limits().ok());
    }

    pub fn codex_usage(&self) -> Option<crate::app::ProviderUsage> {
        self.codex_usage
    }

    pub fn codex_usage_meta(&self) -> Option<CodexUsageMeta> {
        self.codex_usage_meta.clone()
    }

    fn poll_rate_limits_reply(&mut self) {
        let Some(reply) = self.pending_rate_limits.as_ref() else {
            return;
        };
        let result = match reply.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                Some(Err(anyhow::anyhow!("Codex rate-limit reply disconnected")))
            }
        };
        let Some(result) = result else { return };
        self.pending_rate_limits = None;
        let Ok(snapshot) = result else { return };
        // Codex wraps the windows in `result.rateLimits`. Keep the direct-object
        // fallback for older app-server builds that returned the windows at the root.
        let limits = snapshot.get("rateLimits").unwrap_or(&snapshot);
        let (five_hour, weekly) = classify_codex_rate_limit_windows(limits);
        // 창을 하나도 못 읽은 응답에서만 직전 값을 유지한다. 한쪽 창만 보고하는
        // 계정(예: 주간 창만 있는 플랜)은 읽어낸 쪽을 그대로 반영해야 한다.
        if five_hour.is_some() || weekly.is_some() {
            self.codex_usage = Some((five_hour, weekly));
        }
        // 메타(플랜·리셋 시각·리셋 크레딧)는 장식 정보라 부분 유지 없이 매 응답
        // 그대로 반영한다 — 다음 폴(60초)에서 금방 복구된다.
        let meta = codex_usage_meta_from_reply(&snapshot);
        if meta != CodexUsageMeta::default() {
            self.codex_usage_meta = Some(meta);
        }
    }

    /// config → LLM 프로바이더 오버라이드 동기화 (매 프레임, PR-L2). 프로바이더는
    /// 프로세스 argv라 살아 있는 app-server에는 적용되지 않는다 — 진행 중 작업이
    /// 전혀 없으면 기존 shutdown 경로로 client를 내려 다음 실행부터 새 설정을 쓴다.
    /// Logic-tick synchronization for process-level provider configuration. This may shut down a
    /// stale idle client, so the composition root must call it outside `show`.
    pub fn sync_controller_config(&mut self, agents_config: &AgentsConfig) {
        self.llm_override = codex_llm_override_from_config(
            agents_config.codex_llm_provider.as_deref(),
            agents_config.codex_llm_base_url.as_deref(),
            agents_config.codex_llm_wire.as_deref(),
        )
        .map_err(|error| format!("{error:#}"));
        let Ok(target) = &self.llm_override else {
            return;
        };
        let idle = self.sessions.iter().all(|s| s.status.is_terminal())
            && self.pending_thread_requests.is_empty()
            && self.pending_model_catalog.is_none()
            && self.pending_skill_catalog.is_none();
        // 키 저장/삭제(세대 증가)도 프로바이더 변경과 같은 프로세스 레벨 설정이다 —
        // 유휴면 내렸다가 다음 spawn에 새 키를 반영한다 (PR-L4).
        let stale = &self.client_llm_override != target
            || self.client_api_key_generation != self.api_key_generation;
        if self.client.is_some() && stale && idle {
            self.shutdown();
        }
    }

    /// Executes one action returned by the latest render frame. Host/process/protocol work starts
    /// only here, never in `show`. An action superseded by another render is rejected exactly.
    pub fn execute_deferred(
        &mut self,
        deferred: AgentSessionsDeferredAction,
        ctx: &egui::Context,
    ) -> Option<AgentSessionsRequest> {
        if deferred.generation != self.frame_generation {
            return None;
        }
        self.apply_action(deferred.action, ctx)
    }

    fn request_catalogs(
        &mut self,
        ctx: &egui::Context,
        cwd: Option<String>,
        force_reload: bool,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.pending_model_catalog.is_none() && self.pending_skill_catalog.is_none(),
            self.catalog
                .t("agent_sessions.error.catalog_already_loading", &[])
        );
        self.ensure_client(ctx)?;
        let client = self
            .client
            .as_ref()
            .expect("ensure_client 성공 후 client 존재");
        self.pending_model_catalog = Some(client.list_models(None, Some(100), false)?);
        self.pending_skill_catalog = Some(
            client.list_skills(
                cwd.filter(|cwd| !cwd.trim().is_empty())
                    .into_iter()
                    .collect(),
                force_reload,
            )?,
        );
        self.catalog_error = None;
        Ok(())
    }

    fn poll_catalog_replies(&mut self) {
        if let Some(reply) = self.pending_model_catalog.take() {
            match reply.try_recv() {
                Ok(Ok(page)) => {
                    self.model_catalog = page.data;
                    if self.new_model.trim().is_empty()
                        && let Some(default) = self
                            .model_catalog
                            .iter()
                            .find(|model| model.is_default)
                            .or_else(|| self.model_catalog.first())
                    {
                        self.new_model = default.model.clone();
                        self.new_effort = default.default_reasoning_effort.clone();
                    }
                }
                Ok(Err(error)) => {
                    self.catalog_error = Some(CatalogMessage::error(
                        "agent_sessions.error.model_catalog_failed",
                        &error,
                    ));
                }
                Err(TryRecvError::Empty) => self.pending_model_catalog = Some(reply),
                Err(TryRecvError::Disconnected) => {
                    self.catalog_error = Some(CatalogMessage::Key(
                        "agent_sessions.error.model_catalog_disconnected",
                    ));
                }
            }
        }
        if let Some(reply) = self.pending_skill_catalog.take() {
            match reply.try_recv() {
                Ok(Ok(skills)) => {
                    self.skill_catalog = skills;
                    self.selected_skill_paths.retain(|path| {
                        self.skill_catalog
                            .iter()
                            .any(|skill| skill.enabled && skill.path == *path)
                    });
                }
                Ok(Err(error)) => {
                    self.catalog_error = Some(CatalogMessage::error(
                        "agent_sessions.error.skill_catalog_failed",
                        &error,
                    ));
                }
                Err(TryRecvError::Empty) => self.pending_skill_catalog = Some(reply),
                Err(TryRecvError::Disconnected) => {
                    self.catalog_error = Some(CatalogMessage::Key(
                        "agent_sessions.error.skill_catalog_disconnected",
                    ));
                }
            }
        }
    }

    fn selected_skills(&self) -> Vec<AgentSkillSelection> {
        self.skill_catalog
            .iter()
            .filter(|skill| skill.enabled && self.selected_skill_paths.contains(&skill.path))
            .map(|skill| AgentSkillSelection {
                name: skill.name.clone(),
                path: skill.path.clone(),
            })
            .collect()
    }

    fn session_turn_settings(
        &self,
        session_id: &str,
    ) -> anyhow::Result<(Option<String>, Option<String>, Vec<AgentSkillSelection>)> {
        self.sessions
            .iter()
            .find(|session| session.id == session_id)
            .map(|session| {
                (
                    session.model.clone(),
                    session.effort.clone(),
                    session.skills.clone(),
                )
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    self.catalog
                        .t("agent_sessions.error.follow_up_session_missing", &[])
                )
            })
    }

    fn selected_persisted_thread(
        &self,
    ) -> anyhow::Result<(AgentSessionId, AgentSessionPersistedRow)> {
        let Some(AgentSurfaceId::Structured { session_id }) = self.selected_surface.as_ref() else {
            anyhow::bail!(
                self.catalog
                    .t("agent_sessions.error.no_selected_saved_thread", &[])
            );
        };
        let row = self
            .persisted_threads
            .get(session_id)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    self.catalog
                        .t("agent_sessions.error.not_persisted_thread", &[])
                )
            })?;
        anyhow::ensure!(
            !row.archived,
            self.catalog
                .t("agent_sessions.error.thread_already_archived", &[])
        );
        anyhow::ensure!(
            !self
                .pending_thread_requests
                .iter()
                .any(|pending| pending.session_id() == session_id),
            self.catalog
                .t("agent_sessions.error.thread_request_pending", &[])
        );
        Ok((session_id.clone(), row))
    }

    fn mark_history_request_started(&mut self, session_id: &str) {
        self.persisted_placeholders.remove(session_id);
        self.localized_session_errors.remove(session_id);
        if let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
        {
            session.error = None;
            session.status = AgentSessionStatus::Starting;
        }
    }

    fn poll_thread_replies(&mut self) {
        let pending = std::mem::take(&mut self.pending_thread_requests);
        let mut still_pending = Vec::with_capacity(pending.len());
        for request in pending {
            match request.reply().try_recv() {
                Ok(result) => self.finish_thread_request(request, result),
                Err(TryRecvError::Empty) => still_pending.push(request),
                Err(TryRecvError::Disconnected) => self.finish_thread_request(
                    request,
                    Err(anyhow::anyhow!(
                        self.catalog
                            .t("agent_sessions.error.app_server_disconnected", &[])
                    )),
                ),
            }
        }
        self.pending_thread_requests = still_pending;
    }

    fn finish_thread_request(
        &mut self,
        request: PendingThreadRequest,
        result: anyhow::Result<serde_json::Value>,
    ) {
        match request {
            PendingThreadRequest::Read { session_id, .. } => match result {
                Ok(result) => self.load_history_result(&session_id, &result, false),
                Err(error) => {
                    self.fail_session_localized(
                        &session_id,
                        "agent_sessions.error.read_history_failed",
                        &error,
                    );
                }
            },
            PendingThreadRequest::Resume { session_id, .. } => match result {
                Ok(result) => {
                    self.attached_threads.insert(session_id.clone());
                    self.load_history_result(&session_id, &result, true);
                }
                Err(error) => {
                    self.fail_session_localized(
                        &session_id,
                        "agent_sessions.error.resume_failed",
                        &error,
                    );
                }
            },
            PendingThreadRequest::Archive { session_id, .. } => match result {
                Ok(_) => {
                    if self
                        .queue_persistence_mutation(AgentSessionPersistenceMutation::SetArchived {
                            local_session_id: session_id.clone(),
                            archived: true,
                        })
                        .is_err()
                    {
                        return;
                    }
                    self.attached_threads.remove(&session_id);
                    self.remove_persisted_row(&session_id);
                    self.sessions.retain(|session| session.id != session_id);
                    self.localized_session_errors.remove(&session_id);
                    if self.selected_session.as_deref() == Some(session_id.as_str()) {
                        self.selected_session = None;
                        self.selected_surface = None;
                        self.selected_item = None;
                        self.follow_up.clear();
                    }
                }
                Err(error) => {
                    self.fail_session_localized(
                        &session_id,
                        "agent_sessions.error.archive_failed",
                        &error,
                    );
                }
            },
        }
    }

    fn load_history_result(
        &mut self,
        session_id: &str,
        result: &serde_json::Value,
        persist_resume: bool,
    ) {
        let loaded = self
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    self.catalog
                        .t("agent_sessions.error.local_session_missing", &[])
                )
            })
            .and_then(|session| session.load_thread_snapshot(result));
        match loaded {
            Ok(()) => {
                if persist_resume {
                    self.queue_thread_upsert(session_id, true);
                }
                self.queue_current_status_notice(session_id);
            }
            Err(error) => {
                self.fail_session_localized(
                    session_id,
                    "agent_sessions.error.apply_thread_response_failed",
                    &error,
                );
            }
        }
    }

    fn queue_thread_upsert(&mut self, session_id: &str, force: bool) {
        let Some((workspace_id, thread_id, title, cwd, model)) = self
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .and_then(|session| {
                Some((
                    session.workspace_id.clone()?,
                    session.thread_id.clone()?,
                    one_line_title(&session.prompt, &self.catalog),
                    session.cwd.clone().unwrap_or_default(),
                    session.model.clone(),
                ))
            })
        else {
            return;
        };
        let existing = self.persisted_threads.get(session_id);
        let favorite = existing.is_some_and(|row| row.favorite);
        let archived = existing.is_some_and(|row| row.archived);
        let unchanged = existing.is_some_and(|row| {
            row.workspace_id == workspace_id
                && row.thread_id == thread_id
                && row.title == title
                && row.cwd == cwd
                && row.model == model
                && row.favorite == favorite
                && row.archived == archived
        });
        if unchanged && !force {
            return;
        }

        let (created_at, updated_at) = existing
            .map(|row| (row.created_at, row.updated_at))
            .unwrap_or((0, 0));
        let row = AgentSessionPersistedRow {
            local_session_id: session_id.to_owned(),
            workspace_id: workspace_id.clone(),
            thread_id: thread_id.clone(),
            title: title.clone(),
            cwd: cwd.clone(),
            model: model.clone(),
            favorite,
            archived,
            created_at,
            updated_at,
        };
        if !self.can_store_persisted_row(&row)
            || self
                .queue_persistence_mutation(AgentSessionPersistenceMutation::Upsert {
                    local_session_id: session_id.to_owned(),
                    workspace_id,
                    thread_id,
                    title,
                    cwd,
                    model,
                    favorite,
                    archived,
                })
                .is_err()
        {
            return;
        }
        debug_assert!(self.store_persisted_row(row));
    }

    fn can_store_persisted_row(&self, row: &AgentSessionPersistedRow) -> bool {
        let row_bytes = persisted_row_retained_bytes(row);
        if row_bytes > AGENT_SESSION_PERSISTED_ROW_MAX_BYTES {
            return false;
        }
        let previous_bytes = self
            .persisted_threads
            .get(&row.local_session_id)
            .map_or(0, persisted_row_retained_bytes);
        let is_replacement = self.persisted_threads.contains_key(&row.local_session_id);
        if !is_replacement && self.persisted_threads.len() >= AGENT_SESSION_PERSISTED_MAX_ITEMS {
            return false;
        }
        let next_bytes = self
            .persisted_thread_bytes
            .saturating_sub(previous_bytes)
            .saturating_add(row_bytes);
        next_bytes <= AGENT_SESSION_PERSISTED_TOTAL_MAX_BYTES
    }

    fn store_persisted_row(&mut self, row: AgentSessionPersistedRow) -> bool {
        if !self.can_store_persisted_row(&row) {
            return false;
        }
        let row_bytes = persisted_row_retained_bytes(&row);
        let previous_bytes = self
            .persisted_threads
            .get(&row.local_session_id)
            .map_or(0, persisted_row_retained_bytes);
        let next_bytes = self
            .persisted_thread_bytes
            .saturating_sub(previous_bytes)
            .saturating_add(row_bytes);
        self.persisted_thread_bytes = next_bytes;
        self.persisted_threads
            .insert(row.local_session_id.clone(), row);
        true
    }

    fn remove_persisted_row(&mut self, session_id: &str) {
        self.persisted_placeholders.remove(session_id);
        if let Some(row) = self.persisted_threads.remove(session_id) {
            self.persisted_thread_bytes = self
                .persisted_thread_bytes
                .saturating_sub(persisted_row_retained_bytes(&row));
        }
    }

    /// Poll continuously even when the window is closed so an active structured
    /// session reaches a consistent terminal state in the background.
    pub fn poll(&mut self) {
        // App Server sends a one-shot resume/read reply before later stream
        // deltas can be observed by this controller. Apply the snapshot first
        // so a stale snapshot can never erase a newer streamed item update.
        self.poll_thread_replies();
        self.poll_catalog_replies();
        self.poll_rate_limits_reply();
        let mut connection_stopped = false;
        let events = self
            .client
            .as_ref()
            .map(CodexAppServerClient::drain_events)
            .unwrap_or_default();
        for event in events {
            match event {
                CodexAppServerEvent::Session { session_id, event } => {
                    self.apply_session_event(&session_id, event);
                }
                CodexAppServerEvent::TransportError { message } => {
                    self.transport_error = Some(CatalogMessage::raw(message));
                }
                CodexAppServerEvent::ConnectionStopped => connection_stopped = true,
            }
        }
        if connection_stopped {
            self.attached_threads.clear();
            let active = self
                .sessions
                .iter()
                .filter(|session| !session.status.is_terminal())
                .map(|session| session.id.clone())
                .collect::<Vec<_>>();
            for session_id in active {
                self.apply_session_event(&session_id, AgentSessionEvent::Stopped);
            }
            self.client = None;
        }
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        input: AgentSessionsFrameInput<'_>,
    ) -> AgentSessionsFrameOutput {
        let AgentSessionsFrameInput {
            workspace_id,
            mut workspace_cwd,
            pty_surfaces,
            agents_config,
            secrets_snapshot,
            ollama_models,
        } = input;
        workspace_cwd = workspace_cwd.filter(|path| {
            path.len() <= AGENT_SESSION_PATH_INPUT_MAX_BYTES && !path.contains('\0')
        });
        truncate_utf8(&mut self.new_prompt, AGENT_SESSION_TEXT_INPUT_MAX_BYTES);
        truncate_utf8(&mut self.follow_up, AGENT_SESSION_TEXT_INPUT_MAX_BYTES);
        truncate_utf8(&mut self.steer_input, AGENT_SESSION_TEXT_INPUT_MAX_BYTES);
        self.frame_generation = self.frame_generation.wrapping_add(1);
        let frame_generation = self.frame_generation;
        // 렌더 도중 self를 변경하면서도 동일 frame의 locale snapshot을 유지한다.
        // self.catalog는 Arc<i18n::Catalog>라 여기서의 clone은 참조 카운트 증가일
        // 뿐이다(로케일당 ~1,000개 항목짜리 BTreeMap 2개를 매 프레임 딥카피하던
        // 문제를 필드 타입에서 없앴다, 2026-08-14). show() 아래 렌더 헬퍼들이
        // catalog를 인자로 받으며 동시에 &mut self를 쓰기 때문에, self.catalog를
        // 직접 들고 있으면 borrow 충돌이 난다 — 그래서 여전히 clone한 값을 쓴다.
        let catalog = self.catalog.clone();
        let catalog = &catalog;
        self.pty_surfaces = pty_surfaces;
        if matches!(self.selected_surface, Some(AgentSurfaceId::Pty { .. }))
            && !self
                .pty_surfaces
                .iter()
                .any(|surface| Some(&surface.id) == self.selected_surface.as_ref())
        {
            self.selected_surface = None;
        }
        self.poll();
        if !self.open {
            self.surrender_text_focus(ctx);
            self.text_input_ids.clear();
            return AgentSessionsFrameOutput::empty();
        }

        let focused_before = ctx.memory(|memory| memory.focused());
        let pointer_pressed = ctx.input(|input| input.pointer.primary_pressed());
        let pointer_position = ctx.input(|input| input.pointer.interact_pos());
        let previous_input_ids = std::mem::take(&mut self.text_input_ids);
        let mut text_input_ids = Vec::new();
        let mut window_open = self.open;
        let mut actions = Vec::new();
        let mut secret_intent = None;
        let window_response = egui::Window::new(catalog.t("agent_sessions.title", &[]))
            .id(agents_window_id())
            .open(&mut window_open)
            .default_width(960.0)
            .default_height(650.0)
            .min_width(700.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.heading(catalog.t("agent_sessions.title", &[]));
                ui.horizontal(|ui| {
                    ui.weak(catalog.t("agent_sessions.working_directory", &[]));
                    ui.monospace(
                        workspace_cwd
                            .as_deref()
                            .filter(|path| !path.is_empty())
                            .map(str::to_owned)
                            .unwrap_or_else(|| {
                                catalog.t("agent_sessions.app_working_directory", &[])
                            }),
                    );
                });
                if let Some(error) = &self.transport_error {
                    // #ff7b72는 agent_visuals가 소유한 Error 색을 복사해 둔 것이었다.
                    // 그 모듈의 존재 이유가 "표면마다 상태색이 갈리지 않게"이므로 우회하지
                    // 않는다(2026-08-06). 이 파일도 이미 다른 3곳에서 그렇게 쓰고 있다.
                    ui.colored_label(
                        crate::ui::agent_visuals::status_color(
                            crate::agent_surface::AgentVisualState::Error,
                        ),
                        error.render(catalog),
                    );
                }
                if let Some(error) = &self.catalog_error {
                    // 여기만 위와 달리 그대로 둔다. 색(#ffbf69)은 agent_visuals의 Waiting과
                    // 같지만 의미가 다르다 — 이건 "카탈로그 실패, 폴백 사용"이라는 저하
                    // 경고이지 대기 상태가 아니다. 색이 같다는 이유로 lifecycle 상태에
                    // 묶으면 에이전트 UX 때문에 Waiting을 바꿀 때 여기까지 끌려간다.
                    ui.colored_label(
                        egui::Color32::from_rgb(0xff, 0xbf, 0x69),
                        error.render(catalog),
                    );
                    ui.weak(catalog.t("agent_sessions.catalog_fallback_hint", &[]));
                }
                crate::ui::hairline(ui);

                ui.label(catalog.t("agent_sessions.new_task", &[]));
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            self.pending_model_catalog.is_none()
                                && self.pending_skill_catalog.is_none(),
                            egui::Button::new(catalog.t("agent_sessions.load_catalogs", &[])),
                        )
                        .clicked()
                    {
                        queue_frame_action(
                            &mut actions,
                            PanelAction::RefreshCatalog {
                                cwd: workspace_cwd.clone(),
                                force_reload: !self.skill_catalog.is_empty(),
                            },
                        );
                    }
                    if self.pending_model_catalog.is_some() || self.pending_skill_catalog.is_some()
                    {
                        ui.weak(catalog.t("agent_sessions.catalog_waiting", &[]));
                    }
                });
                self.render_agent_controls(
                    ui,
                    &mut text_input_ids,
                    agents_config,
                    AgentSecretRender {
                        snapshot: secrets_snapshot,
                        intent: &mut secret_intent,
                    },
                    ollama_models,
                    catalog,
                );
                let prompt_response = ui.add_sized(
                    [ui.available_width(), 72.0],
                    egui::TextEdit::multiline(&mut self.new_prompt)
                        .id_salt("agent-new-prompt")
                        .hint_text(catalog.t("agent_sessions.prompt_hint", &[]))
                        .desired_rows(3),
                );
                text_input_ids.push(prompt_response.id);
                truncate_utf8(&mut self.new_prompt, AGENT_SESSION_TEXT_INPUT_MAX_BYTES);
                if self.focus_new_prompt {
                    prompt_response.request_focus();
                    self.focus_new_prompt = false;
                }
                ui.horizontal(|ui| {
                    let can_start = !self.new_prompt.trim().is_empty();
                    if ui
                        .add_enabled(
                            can_start,
                            egui::Button::new(catalog.t("agent_sessions.run_codex", &[])),
                        )
                        .clicked()
                    {
                        queue_frame_action(
                            &mut actions,
                            PanelAction::Start {
                                workspace_id: workspace_id.to_owned(),
                                prompt: std::mem::take(&mut self.new_prompt),
                                model: self.new_model.clone(),
                                effort: self.new_effort.clone(),
                                skills: self.selected_skills(),
                                cwd: workspace_cwd.clone(),
                            },
                        );
                    }
                });

                crate::ui::hairline(ui);
                self.render_surface_tabs(ui, &mut actions, catalog);
                crate::ui::hairline(ui);

                match self.selected_surface_snapshot() {
                    Some(surface) if surface.transport == AgentTransport::Pty => {
                        render_selected_surface_header(ui, &surface, catalog);
                        self.render_pty_surface(ui, surface, &mut actions, catalog);
                    }
                    Some(surface) => {
                        render_selected_surface_header(ui, &surface, catalog);
                        if let Some((session_index, session)) = self.take_selected_session() {
                            self.render_session(
                                ui,
                                &session,
                                workspace_cwd.clone(),
                                &mut actions,
                                &mut text_input_ids,
                                catalog,
                            );
                            self.restore_session(session_index, session);
                        }
                    }
                    None => {
                        ui.weak(catalog.t("agent_sessions.empty_selection", &[]));
                    }
                }
            });
        self.open = window_open;
        let content_visible = window_open
            && window_response
                .as_ref()
                .is_some_and(|window| window.inner.is_some());
        let focus_to_surrender = agent_focus_to_surrender(
            focused_before,
            &previous_input_ids,
            pointer_pressed,
            pointer_position,
            window_response.as_ref().map(|window| window.response.rect),
            content_visible,
        );
        if let Some(focused) = focus_to_surrender {
            // `surrender_focus` is conditional on the same id still owning
            // focus, so a terminal widget that already reclaimed focus in
            // this frame is never cleared accidentally.
            ctx.memory_mut(|memory| memory.surrender_focus(focused));
            ctx.request_repaint();
        }
        self.text_input_ids = if self.open && content_visible {
            text_input_ids
        } else {
            Vec::new()
        };

        let deferred_action =
            actions
                .into_iter()
                .next()
                .map(|action| AgentSessionsDeferredAction {
                    generation: frame_generation,
                    action,
                });
        AgentSessionsFrameOutput {
            requests: Vec::new(),
            secret_intent,
            deferred_action,
        }
    }

    fn render_agent_controls(
        &mut self,
        ui: &mut egui::Ui,
        text_input_ids: &mut Vec<egui::Id>,
        agents_config: &mut AgentsConfig,
        secrets: AgentSecretRender<'_>,
        ollama_models: Option<&[String]>,
        catalog: &i18n::Catalog,
    ) {
        let models = self.model_catalog.clone();
        let selected_model = models
            .iter()
            .find(|model| model.model == self.new_model)
            .cloned();
        // 커스텀/OSS 프로바이더는 codex 카탈로그(OpenAI 모델 목록)와 무관한 백엔드
        // 모델명(qwen3-coder:30b 등)을 쓴다 — 카탈로그 드롭다운에 가두면 입력이
        // 불가능해 실작업이 막힌다(2026-07-18 사용자 스크린샷). 자유 입력 유지.
        let free_model_input = agents_config.codex_llm_provider.is_some();
        ui.horizontal_wrapped(|ui| {
            ui.label(catalog.t("agent_sessions.model", &[]));
            if models.is_empty() || free_model_input {
                let hint = if free_model_input {
                    catalog.t("agent_sessions.backend_model_hint", &[])
                } else {
                    catalog.t("agent_sessions.default_codex_model", &[])
                };
                let response = ui.add_sized(
                    [210.0, 24.0],
                    egui::TextEdit::singleline(&mut self.new_model).hint_text(hint),
                );
                text_input_ids.push(response.id);
            } else {
                egui::ComboBox::from_id_salt("agent-model-catalog")
                    .selected_text(
                        selected_model
                            .as_ref()
                            .map_or(self.new_model.as_str(), |model| model.display_name.as_str()),
                    )
                    .show_ui(ui, |ui| {
                        for model in &models {
                            if ui
                                .selectable_label(
                                    self.new_model == model.model,
                                    &model.display_name,
                                )
                                .on_hover_text(&model.description)
                                .clicked()
                            {
                                self.new_model = model.model.clone();
                                if !model
                                    .supported_reasoning_efforts
                                    .iter()
                                    .any(|effort| effort.reasoning_effort == self.new_effort)
                                {
                                    self.new_effort = model.default_reasoning_effort.clone();
                                }
                            }
                        }
                    });
            }

            ui.label(catalog.t("agent_sessions.effort", &[]));
            // 커스텀/OSS는 카탈로그 모델이 아니므로 effort도 자유 입력(백엔드가 무시할 수 있음).
            if let Some(model) = selected_model.filter(|_| !free_model_input) {
                egui::ComboBox::from_id_salt("agent-effort-catalog")
                    .selected_text(&self.new_effort)
                    .show_ui(ui, |ui| {
                        for effort in &model.supported_reasoning_efforts {
                            ui.selectable_value(
                                &mut self.new_effort,
                                effort.reasoning_effort.clone(),
                                &effort.reasoning_effort,
                            )
                            .on_hover_text(&effort.description);
                        }
                    });
            } else {
                let response = ui.add_sized(
                    [110.0, 24.0],
                    egui::TextEdit::singleline(&mut self.new_effort)
                        .hint_text(catalog.t("agent_sessions.default_effort", &[])),
                );
                text_input_ids.push(response.id);
            }
        });

        self.render_llm_provider_controls(
            ui,
            text_input_ids,
            agents_config,
            secrets,
            ollama_models,
            catalog,
        );

        if !self.skill_catalog.is_empty() {
            let skills = self.skill_catalog.clone();
            egui::CollapsingHeader::new(catalog.t(
                "agent_sessions.skills_selected",
                &[("count", &self.selected_skill_paths.len().to_string())],
            ))
            .id_salt("agent-skills-catalog")
            .show(ui, |ui| {
                for skill in skills {
                    let selected = self.selected_skill_paths.contains(&skill.path);
                    let mut checked = selected;
                    let response = ui.add_enabled(
                        skill.enabled,
                        egui::Checkbox::new(
                            &mut checked,
                            format!("{} · {}", skill.name, skill.scope),
                        ),
                    );
                    response.on_hover_text(format!("{}\n{}", skill.description, skill.path));
                    if checked != selected {
                        if checked {
                            self.selected_skill_paths.insert(skill.path);
                        } else {
                            self.selected_skill_paths.remove(&skill.path);
                        }
                    }
                }
            });
        }
    }

    /// LLM 프로바이더 선택 (PR-L2): 기본(구독/기존 codex 설정) / 로컬 OSS (ollama) /
    /// 커스텀 OpenAI 호환. 값은 config에 저장되고 다음 app-server spawn부터 적용된다.
    fn render_llm_provider_controls(
        &mut self,
        ui: &mut egui::Ui,
        text_input_ids: &mut Vec<egui::Id>,
        agents_config: &mut AgentsConfig,
        secrets: AgentSecretRender<'_>,
        ollama_models: Option<&[String]>,
        catalog: &i18n::Catalog,
    ) {
        ui.horizontal_wrapped(|ui| {
            ui.label(catalog.t("agent_sessions.llm_provider", &[]));
            // 콤보 닫힌 상태 표시용 복제 — 닫힌 뒤 클릭 반영은 아래에서 config에 쓴다.
            let selected = agents_config.codex_llm_provider.clone();
            let selected = selected.as_deref();
            egui::ComboBox::from_id_salt("agent-llm-provider")
                .selected_text(llm_provider_label(selected, catalog))
                .show_ui(ui, |ui| {
                    for value in [None, Some("oss"), Some("custom")] {
                        if ui
                            .selectable_label(selected == value, llm_provider_label(value, catalog))
                            .clicked()
                        {
                            agents_config.codex_llm_provider = value.map(str::to_owned);
                        }
                    }
                });
            if agents_config.codex_llm_provider.as_deref() == Some("custom") {
                ui.label(catalog.t("agent_sessions.base_url", &[]));
                let mut base_url = agents_config.codex_llm_base_url.clone().unwrap_or_default();
                let response = ui.add_sized(
                    [240.0, 24.0],
                    egui::TextEdit::singleline(&mut base_url)
                        .hint_text("http://localhost:11434/v1")
                        .font(egui::TextStyle::Monospace),
                );
                text_input_ids.push(response.id);
                if response.changed() {
                    // 공백/제어문자는 argv `-c` 오설정이 되므로 입력 단계에서 거부한다
                    // (validate_mcp_config_flag 내부 공백 거부와 동일 관례). 빈값은 None.
                    if base_url.trim().is_empty() {
                        agents_config.codex_llm_base_url = None;
                    } else if let Ok(valid) = validate_llm_base_url(&base_url) {
                        agents_config.codex_llm_base_url = Some(valid);
                    }
                }
            }
        });
        if agents_config.codex_llm_provider.as_deref() == Some("custom")
            && agents_config.codex_llm_base_url.is_none()
        {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                catalog.t("agent_sessions.custom_base_url_required", &[]),
            );
        }
        if agents_config.codex_llm_provider.as_deref() == Some("custom") {
            // upstream이 실제로 말하는 API (PR-L5). Chat이면 내장 변환 프록시 경유 —
            // 대부분의 ollama 계열 원격/로컬이 /v1/chat/completions만 지원한다.
            ui.horizontal_wrapped(|ui| {
                ui.label(catalog.t("agent_sessions.api_format", &[]));
                // 콤보 닫힌 상태 표시용 복제 — 클릭 반영은 아래에서 config에 쓴다.
                let selected = agents_config.codex_llm_wire.clone();
                let selected = selected.as_deref();
                egui::ComboBox::from_id_salt("agent-llm-wire")
                    .selected_text(llm_wire_label(selected, catalog))
                    .show_ui(ui, |ui| {
                        for value in [None, Some("responses")] {
                            let current =
                                llm_wire_label(selected, catalog) == llm_wire_label(value, catalog);
                            if ui
                                .selectable_label(current, llm_wire_label(value, catalog))
                                .clicked()
                            {
                                agents_config.codex_llm_wire = value.map(str::to_owned);
                            }
                        }
                    });
            });
            self.render_llm_api_key_controls(
                ui,
                text_input_ids,
                secrets.snapshot,
                secrets.intent,
                catalog,
            );
        }
        // OSS 선택 시 감지된 ollama 모델을 클릭 후보로 (PR-L3 — local_llm 감지 배선.
        // 커스텀은 사용자 지시로 검색 없이 입력값 그대로 쓴다, 2026-07-18).
        if agents_config.codex_llm_provider.as_deref() == Some("oss") {
            ui.horizontal_wrapped(|ui| {
                ui.weak(catalog.t("agent_sessions.ollama_models", &[]));
                match ollama_models {
                    Some([]) => {
                        ui.weak(catalog.t("agent_sessions.ollama_empty", &[]));
                    }
                    Some(models) => {
                        for model in models.iter().take(8) {
                            if ui.small_button(model).clicked() {
                                self.new_model = model.clone();
                            }
                        }
                    }
                    None => {
                        ui.weak(catalog.t("agent_sessions.ollama_not_detected", &[]));
                    }
                }
                // 수동 재감지 (2026-07-18 사용자) — 앱 실행 중 ollama를 켰거나
                // 모델을 받은 뒤 목록을 다시 가져온다. App이 take해 감지 워커 재가동.
                if ui
                    .small_button("⟳")
                    .on_hover_text(catalog.t("agent_sessions.ollama_redetect", &[]))
                    .clicked()
                {
                    self.ollama_redetect_requested = true;
                }
            });
        }
        // 프로바이더는 프로세스 레벨이라 살아 있는 app-server에는 적용되지 않는다.
        // 유휴 상태면 sync_llm_config가 자동으로 내렸다가 다음 실행에 반영한다.
        if agents_config.codex_llm_provider.is_some() {
            ui.weak(catalog.t("agent_sessions.provider_next_run", &[]));
        }
    }

    /// Custom-provider authentication controls. The immutable snapshot is the only render input;
    /// save/delete are returned as intents for the composition root.
    fn render_llm_api_key_controls(
        &mut self,
        ui: &mut egui::Ui,
        text_input_ids: &mut Vec<egui::Id>,
        snapshot: &AgentSessionsSecretsSnapshot,
        intent: &mut Option<AgentSessionsSecretIntent>,
        catalog: &i18n::Catalog,
    ) {
        if !snapshot.controls_enabled() {
            return;
        }
        if self.api_key_snapshot_revision != Some(snapshot.revision()) {
            self.api_key_snapshot_revision = Some(snapshot.revision());
            if snapshot.is_available()
                && self.api_key_error == Some(AgentSessionsSecretErrorCode::SnapshotUnavailable)
            {
                self.api_key_error = None;
            }
        }
        if !snapshot.is_available() {
            self.api_key_error = Some(AgentSessionsSecretErrorCode::SnapshotUnavailable);
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(catalog.t("agent_sessions.api_key", &[]));
            let hint = if snapshot.api_key_present() {
                catalog.t("agent_sessions.api_key_replace_hint", &[])
            } else {
                catalog.t("agent_sessions.api_key_optional_hint", &[])
            };
            let response = ui.add_sized(
                [240.0, 24.0],
                egui::TextEdit::singleline(&mut self.api_key_input)
                    .password(true)
                    .hint_text(hint),
            );
            text_input_ids.push(response.id);
            if self.api_key_input.len() > AGENT_SESSION_SENSITIVE_ITEM_MAX_BYTES {
                clear_sensitive_string(&mut self.api_key_input);
                self.api_key_input_overflowed = true;
                self.api_key_error = Some(AgentSessionsSecretErrorCode::InputLimitExceeded);
            } else if response.changed() {
                self.api_key_input_overflowed = false;
            }
            let has_input = !self.api_key_input.trim().is_empty()
                && !self.api_key_input_overflowed
                && self.api_key_pending.is_none()
                && snapshot.is_available();
            if ui
                .add_enabled(has_input, egui::Button::new(catalog.t("action.save", &[])))
                .clicked()
                && intent.is_none()
            {
                let input = std::mem::take(&mut self.api_key_input);
                match SensitiveInput::try_api_key(input) {
                    Ok(input) => {
                        self.api_key_pending = Some(ApiKeyMutationKind::Save);
                        self.api_key_error = None;
                        *intent = Some(AgentSessionsSecretIntent::SaveApiKey {
                            revision: snapshot.revision(),
                            input,
                        });
                    }
                    Err(_) => {
                        self.api_key_error = Some(AgentSessionsSecretErrorCode::InvalidInput);
                    }
                }
            }
            if snapshot.api_key_present()
                && ui
                    .add_enabled(
                        self.api_key_pending.is_none() && snapshot.is_available(),
                        egui::Button::new(catalog.t("action.delete", &[])),
                    )
                    .clicked()
                && intent.is_none()
            {
                self.api_key_pending = Some(ApiKeyMutationKind::Delete);
                self.api_key_error = None;
                *intent = Some(AgentSessionsSecretIntent::DeleteApiKey {
                    revision: snapshot.revision(),
                });
            }
        });
        if let Some(error) = &self.api_key_error {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                api_key_error_message(*error, catalog),
            );
        } else if snapshot.api_key_present() {
            ui.weak(catalog.t("agent_sessions.api_key_stored", &[]));
        }
        if self.api_key_pending.is_some() {
            ui.weak(catalog.t("agent_sessions.catalog_waiting", &[]));
        }
    }

    fn render_surface_tabs(
        &mut self,
        ui: &mut egui::Ui,
        actions: &mut Vec<PanelAction>,
        catalog: &i18n::Catalog,
    ) {
        ui.strong(catalog.t("agent_sessions.agents_heading", &[]));
        let pty_tabs = self
            .pty_surfaces
            .iter()
            .map(|surface| {
                let label = format!(
                    "[{}] {} · {}",
                    surface.transport.badge(),
                    surface.provider.label(),
                    surface.title
                );
                (surface.id.clone(), label, surface.state)
            })
            .collect::<Vec<_>>();
        let app_tabs = self
            .sessions
            .iter()
            .map(|session| {
                (
                    AgentSurfaceId::Structured {
                        session_id: session.id.clone(),
                    },
                    format!(
                        "[APP] Codex · {} · {}",
                        one_line_title(&session.prompt, catalog),
                        structured_status_label(session.status, catalog)
                    ),
                    AgentVisualState::from_structured(session.status),
                )
            })
            .collect::<Vec<_>>();
        ui.horizontal_wrapped(|ui| {
            for (id, label, state) in pty_tabs.into_iter().chain(app_tabs) {
                ui.colored_label(crate::ui::agent_visuals::status_color(state), "●");
                let selected = self.selected_surface.as_ref() == Some(&id);
                if ui.selectable_label(selected, label).clicked() {
                    self.activate_surface(id, actions);
                }
            }
        });
    }

    fn render_pty_surface(
        &mut self,
        ui: &mut egui::Ui,
        surface: AgentSurfaceSnapshot,
        actions: &mut Vec<PanelAction>,
        catalog: &i18n::Catalog,
    ) {
        ui.horizontal_wrapped(|ui| {
            if let Some(model) = &surface.model {
                ui.weak(catalog.t("agent_sessions.model", &[]));
                ui.monospace(model);
            }
            if let Some(effort) = &surface.effort {
                ui.weak(catalog.t("agent_sessions.effort", &[]));
                ui.monospace(effort);
            }
            if let Some(context_pct) = surface.context_pct {
                ui.weak(catalog.t(
                    "agent_sessions.context_percent",
                    &[("percent", &context_pct.to_string())],
                ));
            }
        });
        ui.label(catalog.t("agent_sessions.pty_description", &[]));
        ui.horizontal(|ui| {
            if ui
                .button(catalog.t("agent_sessions.focus_terminal", &[]))
                .clicked()
            {
                queue_frame_action(actions, PanelAction::FocusPty(surface.id.clone()));
            }
            if matches!(
                surface.state,
                AgentVisualState::Active | AgentVisualState::Waiting
            ) && ui
                .button(catalog.t("agent_sessions.interrupt_ctrl_c", &[]))
                .clicked()
            {
                queue_frame_action(actions, PanelAction::InterruptPty(surface.id));
            }
        });
    }

    fn render_session_turn_controls(
        &mut self,
        ui: &mut egui::Ui,
        session: &AgentSession,
        actions: &mut Vec<PanelAction>,
        text_input_ids: &mut Vec<egui::Id>,
        catalog: &i18n::Catalog,
    ) {
        let mut model = session.model.clone().unwrap_or_default();
        let mut effort = session.effort.clone().unwrap_or_default();
        let mut skill_paths = session
            .skills
            .iter()
            .map(|skill| skill.path.clone())
            .collect::<HashSet<_>>();
        let before = (model.clone(), effort.clone(), skill_paths.clone());
        ui.collapsing(catalog.t("agent_sessions.next_turn_settings", &[]), |ui| {
            render_turn_control_fields(
                ui,
                &session.id,
                &self.model_catalog,
                &self.skill_catalog,
                &mut model,
                &mut effort,
                &mut skill_paths,
                text_input_ids,
                catalog,
            );
            ui.weak(catalog.t("agent_sessions.next_turn_hint", &[]));
        });
        if before != (model.clone(), effort.clone(), skill_paths.clone()) {
            let skills = if self.skill_catalog.is_empty() {
                session.skills.clone()
            } else {
                self.skill_catalog
                    .iter()
                    .filter(|skill| skill.enabled && skill_paths.contains(&skill.path))
                    .map(|skill| AgentSkillSelection {
                        name: skill.name.clone(),
                        path: skill.path.clone(),
                    })
                    .collect()
            };
            queue_frame_action(
                actions,
                PanelAction::UpdateTurnControls {
                    session_id: session.id.clone(),
                    model: non_empty(model),
                    effort: non_empty(effort),
                    skills,
                },
            );
        }
    }

    fn take_selected_session(&mut self) -> Option<(usize, AgentSession)> {
        let selected = self.selected_session.as_ref()?;
        let index = self
            .sessions
            .iter()
            .position(|session| &session.id == selected)?;
        Some((index, self.sessions.swap_remove(index)))
    }

    fn restore_session(&mut self, index: usize, session: AgentSession) {
        self.sessions.push(session);
        let last = self.sessions.len() - 1;
        self.sessions.swap(index, last);
    }

    fn render_session(
        &mut self,
        ui: &mut egui::Ui,
        session: &AgentSession,
        workspace_cwd: Option<String>,
        actions: &mut Vec<PanelAction>,
        text_input_ids: &mut Vec<egui::Id>,
        catalog: &i18n::Catalog,
    ) {
        let is_persisted = self.persisted_threads.contains_key(&session.id);
        let is_attached = self.attached_threads.contains(&session.id);
        let request_pending = self
            .pending_thread_requests
            .iter()
            .any(|pending| pending.session_id() == session.id);
        ui.horizontal(|ui| {
            let color = status_color(session.status);
            ui.colored_label(
                color,
                format!("● {}", structured_status_label(session.status, catalog)),
            );
            if let Some(thread_id) = &session.thread_id {
                ui.weak(catalog.t("agent_sessions.thread", &[]));
                ui.monospace(short_id(thread_id));
            }
            if matches!(
                session.status,
                AgentSessionStatus::Running | AgentSessionStatus::AwaitingApproval
            ) && (!is_persisted || is_attached)
                && ui
                    .button(catalog.t("agent_sessions.interrupt", &[]))
                    .clicked()
            {
                queue_frame_action(actions, PanelAction::Interrupt(session.id.clone()));
            }
            if session.status == AgentSessionStatus::Completed
                && ui
                    .button(catalog.t("agent_sessions.acknowledge_completion", &[]))
                    .clicked()
            {
                queue_frame_action(actions, PanelAction::Acknowledge(session.id.clone()));
            }
        });
        ui.horizontal(|ui| {
            ui.weak(catalog.t("agent_sessions.request", &[]));
            ui.add(egui::Label::new(&session.prompt).truncate())
                .on_hover_text(&session.prompt);
        });
        if let Some(cwd) = &session.cwd {
            ui.horizontal(|ui| {
                ui.weak(catalog.t("agent_sessions.working_directory", &[]));
                ui.monospace(cwd);
            });
        }
        if let Some(model) = &session.model {
            ui.horizontal(|ui| {
                ui.weak(catalog.t("agent_sessions.model", &[]));
                ui.monospace(model);
            });
        }
        if let Some(effort) = &session.effort {
            ui.horizontal(|ui| {
                ui.weak(catalog.t("agent_sessions.effort", &[]));
                ui.monospace(effort);
            });
        }
        if !session.skills.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.weak(catalog.t("agent_sessions.skills", &[]));
                for skill in &session.skills {
                    ui.monospace(&skill.name).on_hover_text(&skill.path);
                }
            });
        }
        if session.thread_id.is_some() {
            self.render_session_turn_controls(ui, session, actions, text_input_ids, catalog);
        }
        if is_persisted {
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !request_pending && !is_attached,
                        egui::Button::new(catalog.t("agent_sessions.resume_thread", &[])),
                    )
                    .clicked()
                {
                    queue_frame_action(actions, PanelAction::ResumePersisted(session.id.clone()));
                }
                if ui
                    .add_enabled(
                        !request_pending,
                        egui::Button::new(catalog.t("agent_sessions.read_history", &[])),
                    )
                    .clicked()
                {
                    queue_frame_action(actions, PanelAction::ReadPersisted(session.id.clone()));
                }
                if ui
                    .add_enabled(
                        !request_pending && !is_attached,
                        egui::Button::new(catalog.t("agent_sessions.archive", &[])),
                    )
                    .clicked()
                {
                    queue_frame_action(actions, PanelAction::ArchivePersisted(session.id.clone()));
                }
                if ui
                    .add_enabled(
                        !request_pending && !is_attached,
                        egui::Button::new(catalog.t("agent_sessions.delete_local", &[])),
                    )
                    .clicked()
                {
                    queue_frame_action(actions, PanelAction::DeletePersisted(session.id.clone()));
                }
                if request_pending {
                    ui.weak(catalog.t("agent_sessions.server_waiting", &[]));
                }
            });
        }
        if let Some(error) = &session.error {
            // 위 transport_error와 같은 이유 — agent_visuals가 소유한 Error 색을 쓴다.
            ui.colored_label(
                crate::ui::agent_visuals::status_color(
                    crate::agent_surface::AgentVisualState::Error,
                ),
                error,
            );
        }

        if session.status == AgentSessionStatus::Running
            && (!is_persisted || is_attached)
            && session.turn_id.is_some()
        {
            ui.add_space(4.0);
            ui.label(catalog.t("agent_sessions.steer_label", &[]));
            ui.horizontal(|ui| {
                let response = ui.add_sized(
                    [ui.available_width() - 110.0, 38.0],
                    egui::TextEdit::multiline(&mut self.steer_input)
                        .hint_text(catalog.t("agent_sessions.steer_hint", &[])),
                );
                text_input_ids.push(response.id);
                truncate_utf8(&mut self.steer_input, AGENT_SESSION_TEXT_INPUT_MAX_BYTES);
                if ui
                    .add_enabled(
                        !self.steer_input.trim().is_empty(),
                        egui::Button::new(catalog.t("agent_sessions.send_steer", &[])),
                    )
                    .clicked()
                {
                    queue_frame_action(
                        actions,
                        PanelAction::Steer {
                            session_id: session.id.clone(),
                            prompt: std::mem::take(&mut self.steer_input),
                            skills: session.skills.clone(),
                        },
                    );
                }
            });
        }

        for approval in &session.approvals {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    // 승인 대기는 agent_visuals의 Waiting과 같은 의미다 — 그 색을 복사해
                    // 두는 대신 모듈을 그대로 쓴다(2026-08-06).
                    ui.colored_label(
                        crate::ui::agent_visuals::status_color(
                            crate::agent_surface::AgentVisualState::Waiting,
                        ),
                        approval_kind_label(approval.kind, catalog),
                    );
                    ui.strong(
                        approval
                            .command
                            .clone()
                            .unwrap_or_else(|| catalog.t("agent_sessions.approval_required", &[])),
                    );
                });
                if let Some(reason) = &approval.reason {
                    ui.label(reason);
                }
                if let Some(cwd) = &approval.cwd {
                    ui.monospace(cwd);
                }
                ui.horizontal(|ui| {
                    if ui
                        .button(catalog.t("agent_sessions.allow_once", &[]))
                        .clicked()
                    {
                        queue_frame_action(
                            actions,
                            PanelAction::Approval {
                                session_id: session.id.clone(),
                                request_key: approval.request_key.clone(),
                                decision: AgentApprovalDecision::Accept,
                            },
                        );
                    }
                    if ui
                        .button(catalog.t("agent_sessions.allow_for_session", &[]))
                        .clicked()
                    {
                        queue_frame_action(
                            actions,
                            PanelAction::Approval {
                                session_id: session.id.clone(),
                                request_key: approval.request_key.clone(),
                                decision: AgentApprovalDecision::AcceptForSession,
                            },
                        );
                    }
                    if ui
                        .button(catalog.t("agent_sessions.decline", &[]))
                        .clicked()
                    {
                        queue_frame_action(
                            actions,
                            PanelAction::Approval {
                                session_id: session.id.clone(),
                                request_key: approval.request_key.clone(),
                                decision: AgentApprovalDecision::Decline,
                            },
                        );
                    }
                    if ui
                        .button(catalog.t("agent_sessions.cancel_task", &[]))
                        .clicked()
                    {
                        queue_frame_action(
                            actions,
                            PanelAction::Approval {
                                session_id: session.id.clone(),
                                request_key: approval.request_key.clone(),
                                decision: AgentApprovalDecision::Cancel,
                            },
                        );
                    }
                });
            });
            ui.add_space(4.0);
        }

        ui.strong(catalog.t("agent_sessions.structured_results", &[]));
        let rows = session.table_rows();
        egui::ScrollArea::vertical()
            .id_salt(("agent-session-table", &session.id))
            .max_height(230.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new(("agent-session-grid", &session.id))
                    .striped(true)
                    .min_col_width(64.0)
                    .show(ui, |ui| {
                        ui.strong(catalog.t("agent_sessions.column.status", &[]));
                        ui.strong(catalog.t("agent_sessions.column.type", &[]));
                        ui.strong(catalog.t("agent_sessions.column.task", &[]));
                        ui.strong(catalog.t("agent_sessions.column.location", &[]));
                        ui.strong(catalog.t("agent_sessions.column.result", &[]));
                        ui.end_row();
                        for row in rows {
                            let item_id = row.item_id.clone();
                            let selected = self.selected_item.as_deref() == Some(item_id.as_str());
                            let state_response = ui.add_sized(
                                [90.0, 20.0],
                                egui::Label::new(localized_item_state(&row.state, catalog))
                                    .sense(egui::Sense::click())
                                    .truncate(),
                            );
                            let kind_response = ui.add_sized(
                                [86.0, 20.0],
                                egui::Label::new(localized_item_kind(&row.kind, catalog))
                                    .sense(egui::Sense::click())
                                    .truncate(),
                            );
                            let subject_response = ui.add_sized(
                                [230.0, 20.0],
                                egui::Label::new(&row.subject)
                                    .sense(egui::Sense::click())
                                    .truncate(),
                            );
                            let location_response = ui.add_sized(
                                [160.0, 20.0],
                                egui::Label::new(&row.location)
                                    .sense(egui::Sense::click())
                                    .truncate(),
                            );
                            let outcome_response = ui.add_sized(
                                [220.0, 20.0],
                                egui::Label::new(&row.outcome)
                                    .sense(egui::Sense::click())
                                    .truncate(),
                            );
                            if state_response.clicked()
                                || kind_response.clicked()
                                || subject_response.clicked()
                                || location_response.clicked()
                                || outcome_response.clicked()
                                || selected
                            {
                                self.selected_item = Some(item_id);
                            }
                            ui.end_row();
                        }
                    });
            });

        if let Some(item_id) = &self.selected_item
            && let Some(item) = session.items.iter().find(|item| &item.id == item_id)
        {
            ui.add_space(6.0);
            ui.strong(catalog.t(
                "agent_sessions.details",
                &[("kind", &localized_item_kind(item.kind.label(), catalog))],
            ));
            if let Some(detail) = &item.detail {
                ui.label(detail);
            }
            if !item.output.is_empty() {
                egui::ScrollArea::vertical()
                    .id_salt(("agent-session-detail", &session.id, &item.id))
                    .max_height(120.0)
                    .show(ui, |ui| {
                        ui.monospace(&item.output);
                    });
            }
        }

        if session.thread_id.is_some()
            && (!is_persisted || is_attached)
            && !matches!(
                session.status,
                AgentSessionStatus::Starting
                    | AgentSessionStatus::Running
                    | AgentSessionStatus::AwaitingApproval
            )
        {
            ui.add_space(6.0);
            crate::ui::hairline(ui);
            ui.label(catalog.t("agent_sessions.follow_up", &[]));
            let follow_up_response = ui.add_sized(
                [ui.available_width(), 48.0],
                egui::TextEdit::multiline(&mut self.follow_up)
                    .hint_text(catalog.t("agent_sessions.follow_up_hint", &[]))
                    .desired_rows(2),
            );
            text_input_ids.push(follow_up_response.id);
            truncate_utf8(&mut self.follow_up, AGENT_SESSION_TEXT_INPUT_MAX_BYTES);
            if self.focus_follow_up {
                follow_up_response.request_focus();
                self.focus_follow_up = false;
            }
            if ui
                .add_enabled(
                    !self.follow_up.trim().is_empty(),
                    egui::Button::new(catalog.t("agent_sessions.send_follow_up", &[])),
                )
                .clicked()
            {
                queue_frame_action(
                    actions,
                    PanelAction::Submit {
                        session_id: session.id.clone(),
                        prompt: std::mem::take(&mut self.follow_up),
                        cwd: workspace_cwd,
                    },
                );
            }
        }
    }

    fn apply_action(
        &mut self,
        action: PanelAction,
        ctx: &egui::Context,
    ) -> Option<AgentSessionsRequest> {
        let mut external_request = None;
        match action {
            PanelAction::Start {
                workspace_id,
                prompt,
                model,
                effort,
                skills,
                cwd,
            } => {
                self.start(
                    workspace_id.clone(),
                    prompt,
                    model,
                    effort,
                    skills,
                    cwd,
                    ctx,
                );
                external_request = Some(AgentSessionsRequest::RevealWorkspace(workspace_id));
            }
            PanelAction::Submit {
                session_id,
                prompt,
                cwd,
            } => {
                let settings = self.session_turn_settings(&session_id);
                let result = self
                    .client
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            self.catalog
                                .t("agent_sessions.error.no_app_server_start", &[])
                        )
                    })
                    .and_then(|client| settings.map(|settings| (client, settings)))
                    .and_then(|(client, (model, effort, skills))| {
                        client.submit_turn(
                            session_id.clone(),
                            prompt,
                            cwd,
                            model.clone(),
                            effort.clone(),
                            skills.clone(),
                        )
                    });
                if let Err(error) = result {
                    self.fail_session_localized(
                        &session_id,
                        "agent_sessions.error.follow_up_send_failed",
                        &error,
                    );
                }
            }
            PanelAction::Steer {
                session_id,
                prompt,
                skills,
            } => {
                let result = self
                    .client
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!(self.catalog.t("agent_sessions.error.no_app_server", &[]))
                    })
                    .and_then(|client| client.steer_turn(session_id.clone(), prompt, skills));
                if let Err(error) = result {
                    self.control_error_localized(
                        &session_id,
                        "agent_sessions.error.steer_failed",
                        &error,
                    );
                }
            }
            PanelAction::RefreshCatalog { cwd, force_reload } => {
                if let Err(error) = self.request_catalogs(ctx, cwd, force_reload) {
                    self.catalog_error = Some(CatalogMessage::error(
                        "agent_sessions.error.catalog_request_failed",
                        &error,
                    ));
                }
            }
            PanelAction::UpdateTurnControls {
                session_id,
                model,
                effort,
                skills,
            } => {
                if let Some(session) = self
                    .sessions
                    .iter_mut()
                    .find(|session| session.id == session_id)
                {
                    session.model = model;
                    session.effort = effort;
                    session.skills = skills;
                }
                self.queue_thread_upsert(&session_id, false);
            }
            PanelAction::Interrupt(session_id) => {
                let result = self
                    .client
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!(self.catalog.t("agent_sessions.error.no_app_server", &[]))
                    })
                    .and_then(|client| client.interrupt(session_id.clone()));
                if let Err(error) = result {
                    self.fail_session_localized(
                        &session_id,
                        "agent_sessions.error.interrupt_failed",
                        &error,
                    );
                }
            }
            PanelAction::Acknowledge(session_id) => {
                if let Some(session) = self
                    .sessions
                    .iter_mut()
                    .find(|session| session.id == session_id)
                {
                    session.acknowledge_completion();
                }
            }
            PanelAction::Approval {
                session_id,
                request_key,
                decision,
            } => {
                let result = self
                    .client
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!(self.catalog.t("agent_sessions.error.no_app_server", &[]))
                    })
                    .and_then(|client| {
                        client.respond_approval(session_id.clone(), request_key, decision)
                    });
                if let Err(error) = result {
                    self.fail_session_localized(
                        &session_id,
                        "agent_sessions.error.approval_response_failed",
                        &error,
                    );
                }
            }
            PanelAction::ReadPersisted(session_id) => {
                if let Err(error) = self.read_selected_persisted(ctx) {
                    self.fail_session_localized(
                        &session_id,
                        "agent_sessions.error.read_history_start_failed",
                        &error,
                    );
                }
            }
            PanelAction::ResumePersisted(session_id) => {
                if let Err(error) = self.resume_selected_persisted(ctx) {
                    self.fail_session_localized(
                        &session_id,
                        "agent_sessions.error.resume_start_failed",
                        &error,
                    );
                }
            }
            PanelAction::ArchivePersisted(session_id) => {
                if let Err(error) = self.archive_selected_persisted(ctx) {
                    self.fail_session_localized(
                        &session_id,
                        "agent_sessions.error.archive_start_failed",
                        &error,
                    );
                }
            }
            PanelAction::DeletePersisted(session_id) => {
                if let Err(error) = self.delete_selected_persisted() {
                    self.fail_session_localized(
                        &session_id,
                        "agent_sessions.error.delete_local_failed",
                        &error,
                    );
                }
            }
            PanelAction::FocusPty(id) => {
                external_request = Some(AgentSessionsRequest::FocusPty(id));
            }
            PanelAction::InterruptPty(id) => {
                external_request = Some(AgentSessionsRequest::InterruptPty(id));
            }
        }
        ctx.request_repaint();
        external_request
    }

    #[allow(clippy::too_many_arguments)] // UI start action mirrors stable turn controls.
    fn start(
        &mut self,
        workspace_id: String,
        prompt: String,
        model: String,
        effort: String,
        skills: Vec<AgentSkillSelection>,
        cwd: Option<String>,
        ctx: &egui::Context,
    ) {
        let session_id = uuid::Uuid::new_v4().to_string();
        let mut session = AgentSession::new(session_id.clone(), prompt.clone(), cwd.clone());
        session.workspace_id = Some(workspace_id);
        session.model = non_empty(model.clone());
        session.effort = non_empty(effort.clone());
        session.skills = skills.clone();
        let mut localized_failure = None;
        let result = self.ensure_client(ctx).and_then(|()| {
            self.client
                .as_ref()
                .expect("성공한 App Server client가 존재")
                .start_session(
                    session_id.clone(),
                    prompt,
                    cwd,
                    non_empty(model),
                    non_empty(effort),
                    skills,
                )
        });
        if let Err(error) = result {
            let failure = CatalogMessage::error("agent_sessions.error.codex_run_failed", &error);
            session.apply(AgentSessionEvent::Failed {
                message: failure.render(&self.catalog),
            });
            localized_failure = Some(failure);
            self.transport_error = Some(CatalogMessage::error(
                "agent_sessions.error.app_server_connect_failed",
                &error,
            ));
        }
        let failed = session.status == AgentSessionStatus::Failed;
        if !failed {
            self.attached_threads.insert(session_id.clone());
        }
        self.sessions.push(session);
        if let Some(error) = localized_failure {
            self.localized_session_errors
                .insert(session_id.clone(), error);
        }
        self.selected_session = Some(session_id.clone());
        self.selected_surface = Some(AgentSurfaceId::Structured { session_id });
        self.selected_item = None;
        if failed {
            let selected = self.selected_session.clone().expect("방금 선택한 세션");
            self.queue_current_status_notice(&selected);
        }
    }

    fn fail_session_localized(
        &mut self,
        session_id: &str,
        key: &'static str,
        error: &anyhow::Error,
    ) {
        self.apply_localized_session_error(session_id, key, error, true);
    }

    fn control_error_localized(
        &mut self,
        session_id: &str,
        key: &'static str,
        error: &anyhow::Error,
    ) {
        self.apply_localized_session_error(session_id, key, error, false);
    }

    fn apply_localized_session_error(
        &mut self,
        session_id: &str,
        key: &'static str,
        error: &anyhow::Error,
        fatal: bool,
    ) {
        let message = CatalogMessage::error(key, error);
        let rendered = message.render(&self.catalog);
        let event = if fatal {
            AgentSessionEvent::Failed { message: rendered }
        } else {
            AgentSessionEvent::ControlError { message: rendered }
        };
        self.apply_session_event(session_id, event);
        self.localized_session_errors
            .insert(session_id.to_owned(), message);
    }

    fn apply_session_event(&mut self, session_id: &str, event: AgentSessionEvent) {
        // Any event delivered by the runtime promotes a DB-synthesized row into controller-owned
        // state. It must survive an authoritative catalog refresh even when the event itself leaves
        // the visible lifecycle in `Stopped`.
        self.persisted_placeholders.remove(session_id);
        self.localized_session_errors.remove(session_id);
        let gained_thread = matches!(&event, AgentSessionEvent::ThreadStarted { .. });
        let changed = self
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
            .is_some_and(|session| {
                let before = session.status;
                session.apply(event);
                session.status != before
            });
        if changed {
            self.queue_current_status_notice(session_id);
        }
        let pending_resume = self.pending_thread_requests.iter().any(|pending| {
            matches!(pending, PendingThreadRequest::Resume { session_id: pending_id, .. } if pending_id == session_id)
        });
        if gained_thread && !pending_resume {
            self.queue_thread_upsert(session_id, false);
        }
    }

    fn queue_current_status_notice(&mut self, session_id: &str) {
        let Some(session) = self
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            return;
        };
        let Some(workspace_id) = session.workspace_id.clone() else {
            return;
        };
        self.status_notices.push(AgentSessionStatusNotice {
            workspace_id,
            session_id: session.id.clone(),
            title: one_line_title(&session.prompt, &self.catalog),
            status: session.status,
        });
    }

    fn surface_ids(&self) -> Vec<AgentSurfaceId> {
        self.pty_surfaces
            .iter()
            .map(|surface| surface.id.clone())
            .chain(
                self.sessions
                    .iter()
                    .map(|session| AgentSurfaceId::Structured {
                        session_id: session.id.clone(),
                    }),
            )
            .collect()
    }

    fn select_surface(&mut self, id: AgentSurfaceId) {
        self.selected_session = match &id {
            AgentSurfaceId::Structured { session_id } => Some(session_id.clone()),
            AgentSurfaceId::Pty { .. } => None,
        };
        self.selected_surface = Some(id);
        self.selected_item = None;
        self.follow_up.clear();
    }

    fn activate_surface(&mut self, id: AgentSurfaceId, actions: &mut Vec<PanelAction>) {
        self.select_surface(id.clone());
        if matches!(id, AgentSurfaceId::Pty { .. }) {
            queue_frame_action(actions, PanelAction::FocusPty(id));
        }
    }
}

impl Drop for AgentSessionsUi {
    fn drop(&mut self) {
        clear_sensitive_string(&mut self.api_key_input);
    }
}

fn agent_focus_to_surrender(
    focused: Option<egui::Id>,
    agent_input_ids: &[egui::Id],
    pointer_pressed: bool,
    pointer_position: Option<egui::Pos2>,
    window_rect: Option<egui::Rect>,
    content_visible: bool,
) -> Option<egui::Id> {
    let focused = focused?;
    if !agent_input_ids.contains(&focused) {
        return None;
    }
    if !content_visible {
        return Some(focused);
    }
    let pointer = pointer_position?;
    let window_rect = window_rect?;
    (pointer_pressed && !window_rect.contains(pointer)).then_some(focused)
}

enum PanelAction {
    Start {
        workspace_id: String,
        prompt: String,
        model: String,
        effort: String,
        skills: Vec<AgentSkillSelection>,
        cwd: Option<String>,
    },
    Submit {
        session_id: AgentSessionId,
        prompt: String,
        cwd: Option<String>,
    },
    Steer {
        session_id: AgentSessionId,
        prompt: String,
        skills: Vec<AgentSkillSelection>,
    },
    RefreshCatalog {
        cwd: Option<String>,
        force_reload: bool,
    },
    UpdateTurnControls {
        session_id: AgentSessionId,
        model: Option<String>,
        effort: Option<String>,
        skills: Vec<AgentSkillSelection>,
    },
    Interrupt(AgentSessionId),
    Acknowledge(AgentSessionId),
    Approval {
        session_id: AgentSessionId,
        request_key: String,
        decision: AgentApprovalDecision,
    },
    ReadPersisted(AgentSessionId),
    ResumePersisted(AgentSessionId),
    ArchivePersisted(AgentSessionId),
    DeletePersisted(AgentSessionId),
    FocusPty(AgentSurfaceId),
    InterruptPty(AgentSurfaceId),
}

fn queue_frame_action(slot: &mut Vec<PanelAction>, action: PanelAction) {
    if slot.is_empty() {
        slot.push(action);
    }
}

#[allow(dead_code)] // Reachable from the pending App-level history import.
fn persisted_title(row: &AgentSessionPersistedRow, catalog: &i18n::Catalog) -> String {
    let title = row.title.trim();
    if title.is_empty() {
        catalog.t(
            "agent_sessions.persisted_thread_title",
            &[("id", &short_id(&row.thread_id))],
        )
    } else {
        title.to_owned()
    }
}

fn persisted_row_retained_bytes(row: &AgentSessionPersistedRow) -> usize {
    row.local_session_id
        .len()
        .saturating_add(row.workspace_id.len())
        .saturating_add(row.thread_id.len())
        .saturating_add(row.title.len())
        .saturating_add(row.cwd.len())
        .saturating_add(row.model.as_ref().map_or(0, String::len))
}

fn session_has_runtime_state(session: &AgentSession) -> bool {
    session.status != AgentSessionStatus::Stopped
        || session.turn_id.is_some()
        || session.thread_status.is_some()
        || !session.items.is_empty()
        || !session.approvals.is_empty()
        || session.error.is_some()
        || session.effort.is_some()
        || !session.skills.is_empty()
}

#[allow(dead_code)] // Reachable from the pending App-level history import.
fn apply_persisted_metadata(
    session: &mut AgentSession,
    row: &AgentSessionPersistedRow,
    catalog: &i18n::Catalog,
) {
    session.workspace_id = Some(row.workspace_id.clone());
    session.prompt = persisted_title(row, catalog);
    session.cwd = non_empty(row.cwd.clone());
    session.model = row.model.clone();
    session.thread_id = Some(row.thread_id.clone());
}

fn render_selected_surface_header(
    ui: &mut egui::Ui,
    surface: &AgentSurfaceSnapshot,
    catalog: &i18n::Catalog,
) {
    ui.horizontal(|ui| {
        ui.colored_label(
            crate::ui::agent_visuals::status_color(surface.state),
            format!(
                "● [{}] {}",
                surface.transport.badge(),
                surface.provider.label()
            ),
        );
        ui.strong(&surface.title);
    });
    let key = match surface.transport {
        AgentTransport::AppServer => "agent_sessions.surface.app_description",
        AgentTransport::Pty => "agent_sessions.surface.pty_description",
    };
    ui.weak(catalog.t(key, &[]));
}

#[allow(clippy::too_many_arguments)]
fn render_turn_control_fields(
    ui: &mut egui::Ui,
    id: &str,
    models: &[CodexModelInfo],
    skills: &[CodexSkillInfo],
    model_value: &mut String,
    effort_value: &mut String,
    selected_skill_paths: &mut HashSet<String>,
    text_input_ids: &mut Vec<egui::Id>,
    catalog: &i18n::Catalog,
) {
    let selected_model = models
        .iter()
        .find(|model| model.model == *model_value)
        .cloned();
    ui.horizontal_wrapped(|ui| {
        ui.label(catalog.t("agent_sessions.model", &[]));
        if models.is_empty() {
            let response = ui.add_sized(
                [210.0, 24.0],
                egui::TextEdit::singleline(model_value)
                    .hint_text(catalog.t("agent_sessions.default_codex_model", &[])),
            );
            text_input_ids.push(response.id);
        } else {
            egui::ComboBox::from_id_salt(("agent-turn-model", id))
                .selected_text(
                    selected_model
                        .as_ref()
                        .map_or(model_value.as_str(), |model| model.display_name.as_str()),
                )
                .show_ui(ui, |ui| {
                    for model in models {
                        if ui
                            .selectable_label(*model_value == model.model, &model.display_name)
                            .on_hover_text(&model.description)
                            .clicked()
                        {
                            *model_value = model.model.clone();
                            if !model
                                .supported_reasoning_efforts
                                .iter()
                                .any(|effort| effort.reasoning_effort == *effort_value)
                            {
                                *effort_value = model.default_reasoning_effort.clone();
                            }
                        }
                    }
                });
        }
        ui.label(catalog.t("agent_sessions.effort", &[]));
        if let Some(model) = selected_model {
            egui::ComboBox::from_id_salt(("agent-turn-effort", id))
                .selected_text(effort_value.as_str())
                .show_ui(ui, |ui| {
                    for effort in &model.supported_reasoning_efforts {
                        ui.selectable_value(
                            effort_value,
                            effort.reasoning_effort.clone(),
                            &effort.reasoning_effort,
                        )
                        .on_hover_text(&effort.description);
                    }
                });
        } else {
            let response = ui.add_sized(
                [110.0, 24.0],
                egui::TextEdit::singleline(effort_value)
                    .hint_text(catalog.t("agent_sessions.default_effort", &[])),
            );
            text_input_ids.push(response.id);
        }
    });
    if !skills.is_empty() {
        egui::CollapsingHeader::new(catalog.t(
            "agent_sessions.skills_selected",
            &[("count", &selected_skill_paths.len().to_string())],
        ))
        .id_salt(("agent-turn-skills", id))
        .show(ui, |ui| {
            for skill in skills {
                let selected = selected_skill_paths.contains(&skill.path);
                let mut checked = selected;
                ui.add_enabled(
                    skill.enabled,
                    egui::Checkbox::new(&mut checked, format!("{} · {}", skill.name, skill.scope)),
                )
                .on_hover_text(format!("{}\n{}", skill.description, skill.path));
                if checked != selected {
                    if checked {
                        selected_skill_paths.insert(skill.path.clone());
                    } else {
                        selected_skill_paths.remove(&skill.path);
                    }
                }
            }
        });
    }
}

fn localized_error(catalog: &i18n::Catalog, key: &str, error: &anyhow::Error) -> String {
    let detail = format!("{error:#}");
    catalog.t(key, &[("error", &detail)])
}

fn non_empty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

/// LLM 프로바이더 콤보 표시 문자열 (PR-L2). 미지값은 config 로드 정규화가 막지만
/// 방어적으로 원문을 그대로 보여준다.
fn llm_provider_label(provider: Option<&str>, catalog: &i18n::Catalog) -> String {
    match provider {
        None => catalog.t("agent_sessions.provider.default", &[]),
        Some("oss") => catalog.t("agent_sessions.provider.oss", &[]),
        Some("custom") => catalog.t("agent_sessions.provider.custom", &[]),
        Some(other) => other.to_owned(),
    }
}

/// custom wire API 콤보 표시 (PR-L5). None/chat = 기본(내장 변환 프록시 경유).
fn llm_wire_label(wire: Option<&str>, catalog: &i18n::Catalog) -> String {
    match wire {
        None | Some("chat") => catalog.t("agent_sessions.wire.chat_default", &[]),
        Some("responses") => catalog.t("agent_sessions.wire.responses", &[]),
        Some(other) => other.to_owned(),
    }
}

fn structured_status_label(status: AgentSessionStatus, catalog: &i18n::Catalog) -> String {
    let key = match status {
        AgentSessionStatus::Starting => "agent_sessions.status.starting",
        AgentSessionStatus::Ready => "agent_sessions.status.ready",
        AgentSessionStatus::Running => "agent_sessions.status.running",
        AgentSessionStatus::AwaitingApproval => "agent_sessions.status.awaiting_approval",
        AgentSessionStatus::Completed => "agent_sessions.status.completed",
        AgentSessionStatus::Interrupted => "agent_sessions.status.interrupted",
        AgentSessionStatus::Failed => "agent_sessions.status.failed",
        AgentSessionStatus::Stopped => "agent_sessions.status.stopped",
    };
    catalog.t(key, &[])
}

fn approval_kind_label(kind: AgentApprovalKind, catalog: &i18n::Catalog) -> String {
    let key = match kind {
        AgentApprovalKind::CommandExecution => "agent_sessions.approval.command",
        AgentApprovalKind::FileChange => "agent_sessions.approval.file_change",
    };
    catalog.t(key, &[])
}

fn localized_item_state(state: &str, catalog: &i18n::Catalog) -> String {
    let key = match state {
        "starting" => Some("agent_sessions.status.starting"),
        "ready" | "idle" => Some("agent_sessions.status.ready"),
        "running" | "inProgress" => Some("agent_sessions.status.running"),
        "approval" => Some("agent_sessions.status.awaiting_approval"),
        "completed" => Some("agent_sessions.status.completed"),
        "interrupted" => Some("agent_sessions.status.interrupted"),
        "failed" => Some("agent_sessions.status.failed"),
        "stopped" => Some("agent_sessions.status.stopped"),
        _ => None,
    };
    key.map_or_else(|| state.to_owned(), |key| catalog.t(key, &[]))
}

fn localized_item_kind(kind: &str, catalog: &i18n::Catalog) -> String {
    let key = match kind {
        "input" => Some("agent_sessions.item.input"),
        "answer" => Some("agent_sessions.item.answer"),
        "plan" => Some("agent_sessions.item.plan"),
        "reasoning" => Some("agent_sessions.item.reasoning"),
        "command" => Some("agent_sessions.item.command"),
        "file change" => Some("agent_sessions.item.file_change"),
        "connector" => Some("agent_sessions.item.connector"),
        "web search" => Some("agent_sessions.item.web_search"),
        "image" => Some("agent_sessions.item.image"),
        "review" => Some("agent_sessions.item.review"),
        "context" => Some("agent_sessions.item.context"),
        "event" => Some("agent_sessions.item.event"),
        _ => None,
    };
    key.map_or_else(|| kind.to_owned(), |key| catalog.t(key, &[]))
}

fn short_id(value: &str) -> String {
    value.chars().take(8).collect()
}

fn one_line_title(value: &str, catalog: &i18n::Catalog) -> String {
    let title = value.lines().next().unwrap_or_default().trim();
    if title.is_empty() {
        catalog.t("agent_sessions.default_task", &[])
    } else {
        title.chars().take(80).collect()
    }
}

fn status_color(status: AgentSessionStatus) -> egui::Color32 {
    crate::ui::agent_visuals::status_color(crate::agent_surface::AgentVisualState::from_structured(
        status,
    ))
}

fn api_key_error_message(code: AgentSessionsSecretErrorCode, catalog: &i18n::Catalog) -> String {
    let (key, detail) = match code {
        AgentSessionsSecretErrorCode::SnapshotUnavailable => (
            "agent_sessions.api_key_check_failed",
            "snapshot_unavailable",
        ),
        AgentSessionsSecretErrorCode::InputLimitExceeded => {
            ("agent_sessions.api_key_save_failed", "input_limit")
        }
        AgentSessionsSecretErrorCode::InvalidInput => {
            ("agent_sessions.api_key_save_failed", "invalid_input")
        }
        AgentSessionsSecretErrorCode::SaveFailed => {
            ("agent_sessions.api_key_save_failed", "save_failed")
        }
        AgentSessionsSecretErrorCode::DeleteFailed => {
            ("agent_sessions.api_key_delete_failed", "delete_failed")
        }
    };
    catalog.t(key, &[("error", detail)])
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

const CODEX_SESSION_WINDOW_MINUTES: f64 = 300.0;
const CODEX_WEEKLY_WINDOW_MINUTES: f64 = 10_080.0;
/// 구버전 app-server가 보고하던 1분 오차만 흡수하고 다른 창 길이는 받지 않는다.
const CODEX_WINDOW_DURATION_TOLERANCE_MINUTES: f64 = 1.0;

/// Codex 사용량 창을 (5시간, 주간)으로 분류한다 — **길이 우선, 위치는 최후**.
///
/// app-server는 창을 `primary`/`secondary`로 주지만 그 자리는 창 길이를 뜻하지
/// 않는다. 주간 창만 있는 플랜은 `primary.windowDurationMins = 10080`에
/// `secondary = null`로 오는데, 위치만 보고 primary를 5시간 칸에 넣으면 주간
/// 수치가 5시간 수치로 둔갑한다. 그래서 위치 기반 옛 매핑(primary=세션,
/// secondary=주간)은 그 창의 길이를 **아예 판별할 수 없을 때만** 남긴다.
/// Codex 사용량의 장식 메타 — 상태바 옆에 곁들이는 정보라 없어도 표시는 성립한다.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodexUsageMeta {
    /// 구독 플랜 (`planType`, 예: "pro" / "plus").
    pub plan_type: Option<String>,
    /// 5시간 창 리셋 시각 (unix 초). 길이로 판별된 창에서만 읽는다.
    pub five_hour_resets_at: Option<i64>,
    /// 주간 창 리셋 시각 (unix 초).
    pub weekly_resets_at: Option<i64>,
    /// 사용 가능한 사용량 리셋 크레딧 수 (`rateLimitResetCredits.availableCount`).
    pub reset_credits: Option<u32>,
}

/// app-server 응답 전체에서 메타를 뽑는다. 리셋 시각은 길이로 판별된 창에서만
/// 읽는다 — 위치 폴백까지 태우면 어떤 창의 리셋인지 라벨을 보증할 수 없다.
fn codex_usage_meta_from_reply(snapshot: &serde_json::Value) -> CodexUsageMeta {
    let limits = snapshot.get("rateLimits").unwrap_or(snapshot);
    let resets_at = |expected_minutes: f64| {
        ["primary", "secondary"].into_iter().find_map(|name| {
            let window = limits.get(name)?;
            let minutes = window.get("windowDurationMins")?.as_f64()?;
            if (minutes - expected_minutes).abs() > CODEX_WINDOW_DURATION_TOLERANCE_MINUTES {
                return None;
            }
            window.get("resetsAt")?.as_i64()
        })
    };
    CodexUsageMeta {
        plan_type: limits
            .get("planType")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        five_hour_resets_at: resets_at(CODEX_SESSION_WINDOW_MINUTES),
        weekly_resets_at: resets_at(CODEX_WEEKLY_WINDOW_MINUTES),
        reset_credits: snapshot
            .get("rateLimitResetCredits")
            .and_then(|credits| credits.get("availableCount"))
            .and_then(serde_json::Value::as_u64)
            .and_then(|count| u32::try_from(count).ok()),
    }
}

fn classify_codex_rate_limit_windows(limits: &serde_json::Value) -> crate::app::ProviderUsage {
    let used_percent = |name: &str| {
        limits
            .get(name)?
            .get("usedPercent")?
            .as_f64()
            .filter(|percent| percent.is_finite())
            .map(|percent| percent.clamp(0.0, 100.0).round() as u8)
    };
    let is_window = |name: &str, expected_minutes: f64| {
        limits
            .get(name)
            .and_then(|window| window.get("windowDurationMins"))
            .and_then(serde_json::Value::as_f64)
            .is_some_and(|minutes| {
                (minutes - expected_minutes).abs() <= CODEX_WINDOW_DURATION_TOLERANCE_MINUTES
            })
    };
    // 길이를 알아본 창 — 두 자리를 모두 훑어 순서가 뒤바뀐 응답도 받는다.
    let by_duration = |expected_minutes: f64| {
        ["primary", "secondary"]
            .into_iter()
            .filter(|name| is_window(name, expected_minutes))
            .find_map(used_percent)
    };
    // 길이를 못 알아본 창에만 옛 위치 매핑(primary=세션, secondary=주간)을 남긴다.
    let positional = |name: &str| {
        (!is_window(name, CODEX_SESSION_WINDOW_MINUTES)
            && !is_window(name, CODEX_WEEKLY_WINDOW_MINUTES))
        .then(|| used_percent(name))
        .flatten()
    };
    (
        by_duration(CODEX_SESSION_WINDOW_MINUTES).or_else(|| positional("primary")),
        by_duration(CODEX_WEEKLY_WINDOW_MINUTES).or_else(|| positional("secondary")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_session::{AgentApproval, AgentApprovalKind};
    use crate::codex_app_server::CodexLlmWire;
    use serde_json::json;
    use std::sync::mpsc;

    fn catalog() -> i18n::Catalog {
        i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap()
    }

    fn persisted_row(local_session_id: &str, thread_id: &str) -> AgentSessionPersistedRow {
        AgentSessionPersistedRow {
            local_session_id: local_session_id.to_owned(),
            workspace_id: "ws-1".to_owned(),
            thread_id: thread_id.to_owned(),
            title: "복구 작업".to_owned(),
            cwd: "/repo".to_owned(),
            model: Some("gpt-test".to_owned()),
            favorite: true,
            archived: false,
            created_at: 10,
            updated_at: 20,
        }
    }

    fn upsert_mutation(
        local_session_id: &str,
        title: impl Into<String>,
        archived: bool,
    ) -> AgentSessionPersistenceMutation {
        AgentSessionPersistenceMutation::Upsert {
            local_session_id: local_session_id.to_owned(),
            workspace_id: "w".to_owned(),
            thread_id: format!("thread-{local_session_id}"),
            title: title.into(),
            cwd: "/".to_owned(),
            model: None,
            favorite: false,
            archived,
        }
    }

    fn archive_mutation(local_session_id: &str, archived: bool) -> AgentSessionPersistenceMutation {
        AgentSessionPersistenceMutation::SetArchived {
            local_session_id: local_session_id.to_owned(),
            archived,
        }
    }

    fn delete_mutation(local_session_id: &str) -> AgentSessionPersistenceMutation {
        AgentSessionPersistenceMutation::Delete {
            local_session_id: local_session_id.to_owned(),
        }
    }

    fn thread_result(thread_id: &str) -> serde_json::Value {
        json!({
            "thread": {
                "id": thread_id,
                "cwd": "/repo",
                "model": "gpt-test",
                "status": {"type": "idle"},
                "turns": [{
                    "id": "turn-1",
                    "items": [{
                        "id": "answer-1",
                        "type": "agentMessage",
                        "text": "restored",
                        "status": "completed"
                    }]
                }]
            }
        })
    }

    fn pty_surface(id: u64) -> AgentSurfaceSnapshot {
        AgentSurfaceSnapshot {
            id: AgentSurfaceId::Pty {
                workspace_id: "ws-1".to_owned(),
                pane_id: format!("pane-{id}"),
                session_id: runtime::SessionId(id),
            },
            provider: AgentProvider::Codex,
            transport: AgentTransport::Pty,
            title: format!("PTY {id}"),
            model: None,
            effort: None,
            context_pct: None,
            state: AgentVisualState::Idle,
            pty_status: Some(runtime::SessionStatus::Idle),
        }
    }

    fn approval(key: &str) -> AgentApproval {
        AgentApproval {
            request_key: key.to_owned(),
            kind: AgentApprovalKind::CommandExecution,
            thread_id: "thread-1".to_owned(),
            turn_id: "turn-1".to_owned(),
            item_id: "item-1".to_owned(),
            reason: None,
            command: Some("cargo test".to_owned()),
            cwd: None,
        }
    }

    #[test]
    fn sync_llm_config는_config를_오버라이드로_반영한다() {
        let mut ui = AgentSessionsUi::new();
        // 기본: 오버라이드 없음.
        ui.sync_controller_config(&AgentsConfig::default());
        assert_eq!(ui.llm_override, Ok(None));
        // oss / custom 반영.
        ui.sync_controller_config(&AgentsConfig {
            codex_llm_provider: Some("oss".to_owned()),
            codex_llm_base_url: None,
            codex_llm_wire: None,
            disabled: Vec::new(),
        });
        assert_eq!(ui.llm_override, Ok(Some(CodexLlmOverride::Oss)));
        ui.sync_controller_config(&AgentsConfig {
            codex_llm_provider: Some("custom".to_owned()),
            codex_llm_base_url: Some("http://localhost:11434/v1".to_owned()),
            codex_llm_wire: None,
            disabled: Vec::new(),
        });
        assert_eq!(
            ui.llm_override,
            Ok(Some(CodexLlmOverride::Custom {
                base_url: "http://localhost:11434/v1".to_owned(),
                wire: CodexLlmWire::Chat,
            }))
        );
        // wire responses는 직결 경로로 반영된다 (PR-L5).
        ui.sync_controller_config(&AgentsConfig {
            codex_llm_provider: Some("custom".to_owned()),
            codex_llm_base_url: Some("http://localhost:11434/v1".to_owned()),
            codex_llm_wire: Some("responses".to_owned()),
            disabled: Vec::new(),
        });
        assert_eq!(
            ui.llm_override,
            Ok(Some(CodexLlmOverride::Custom {
                base_url: "http://localhost:11434/v1".to_owned(),
                wire: CodexLlmWire::Responses,
            }))
        );
        // custom인데 base URL 없음 → Err (spawn 차단 사유 보존).
        ui.sync_controller_config(&AgentsConfig {
            codex_llm_provider: Some("custom".to_owned()),
            codex_llm_base_url: None,
            codex_llm_wire: None,
            disabled: Vec::new(),
        });
        assert!(ui.llm_override.is_err());
    }

    #[test]
    fn 잘못된_llm_설정은_ensure_client가_spawn_전에_거부한다() {
        let mut ui = AgentSessionsUi::new();
        ui.sync_controller_config(&AgentsConfig {
            codex_llm_provider: Some("custom".to_owned()),
            codex_llm_base_url: None,
            codex_llm_wire: None,
            disabled: Vec::new(),
        });
        let ctx = egui::Context::default();
        let error = ui.ensure_client(&ctx).unwrap_err();
        assert!(format!("{error:#}").contains("LLM provider configuration error"));
        assert!(ui.client.is_none());
    }

    #[derive(Default)]
    struct CountingHost {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl CodexAppServerHost for CountingHost {
        fn spawn(
            &self,
            _llm_override: Option<CodexLlmOverride>,
            _ctx: egui::Context,
        ) -> anyhow::Result<CodexAppServerClient> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            anyhow::bail!("host load failed")
        }
    }

    #[test]
    fn sensitive_api_key_is_trimmed_redacted_and_bounded() {
        let input = SensitiveInput::try_api_key("  sk-test-123  ".to_owned()).unwrap();
        assert_eq!(format!("{input:?}"), "SensitiveInput([REDACTED])");
        assert!(!format!("{input:?}").contains("sk-test-123"));
        let mut plain = input.into_inner();
        assert_eq!(plain, "sk-test-123");
        clear_sensitive_string(&mut plain);
        assert!(
            SensitiveInput::try_api_key("x".repeat(AGENT_SESSION_SENSITIVE_ITEM_MAX_BYTES)).is_ok()
        );
        assert!(
            SensitiveInput::try_api_key("x".repeat(AGENT_SESSION_SENSITIVE_ITEM_MAX_BYTES + 1))
                .is_err()
        );
        for bad in ["", "   ", "sk a", "sk\nb"] {
            assert!(SensitiveInput::try_api_key(bad.to_owned()).is_err());
        }
    }

    #[test]
    fn unchanged_secret_snapshot_renders_300_frames_without_host_calls() {
        let host = Arc::new(CountingHost::default());
        let mut state = AgentSessionsUi::new().with_app_server_host(host.clone());
        let snapshot = AgentSessionsSecretsSnapshot::new(7, true);
        let catalog = catalog();
        let context = egui::Context::default();
        for _ in 0..300 {
            let _ = context.run_ui(egui::RawInput::default(), |ui| {
                let mut text_input_ids = Vec::new();
                let mut intent = None;
                state.render_llm_api_key_controls(
                    ui,
                    &mut text_input_ids,
                    &snapshot,
                    &mut intent,
                    &catalog,
                );
                assert!(intent.is_none());
            });
        }
        assert_eq!(host.calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn deferred_action_boundary_keeps_show_host_free_and_executes_once() {
        let host = Arc::new(CountingHost::default());
        let mut state = AgentSessionsUi::new().with_app_server_host(host.clone());
        let snapshot = AgentSessionsSecretsSnapshot::new(7, false);
        let mut config = AgentsConfig::default();
        let context = egui::Context::default();

        for _ in 0..300 {
            let output = state.show(
                &context,
                AgentSessionsFrameInput {
                    workspace_id: "workspace-1",
                    workspace_cwd: Some("/repo".to_owned()),
                    pty_surfaces: Vec::new(),
                    agents_config: &mut config,
                    secrets_snapshot: &snapshot,
                    ollama_models: None,
                },
            );
            assert!(output.deferred_action.is_none());
        }
        assert_eq!(host.calls.load(std::sync::atomic::Ordering::Relaxed), 0);

        let deferred = AgentSessionsDeferredAction {
            generation: state.frame_generation,
            action: PanelAction::Start {
                workspace_id: "workspace-1".to_owned(),
                prompt: "bounded request".to_owned(),
                model: String::new(),
                effort: String::new(),
                skills: Vec::new(),
                cwd: Some("/repo".to_owned()),
            },
        };
        assert_eq!(
            state.execute_deferred(deferred, &context),
            Some(AgentSessionsRequest::RevealWorkspace(
                "workspace-1".to_owned()
            ))
        );
        assert_eq!(host.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn deferred_action_boundary_rejects_superseded_generation() {
        let host = Arc::new(CountingHost::default());
        let mut state = AgentSessionsUi::new().with_app_server_host(host.clone());
        let deferred = AgentSessionsDeferredAction {
            generation: state.frame_generation,
            action: PanelAction::RefreshCatalog {
                cwd: None,
                force_reload: false,
            },
        };
        state.frame_generation = state.frame_generation.wrapping_add(1);
        assert_eq!(
            state.execute_deferred(deferred, &egui::Context::default()),
            None
        );
        assert_eq!(host.calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn deferred_action_boundary_keeps_only_first_and_bounds_text() {
        let mut actions = Vec::new();
        queue_frame_action(
            &mut actions,
            PanelAction::FocusPty(AgentSurfaceId::Pty {
                workspace_id: "workspace-1".to_owned(),
                pane_id: "pane-1".to_owned(),
                session_id: runtime::SessionId(1),
            }),
        );
        queue_frame_action(
            &mut actions,
            PanelAction::InterruptPty(AgentSurfaceId::Pty {
                workspace_id: "workspace-1".to_owned(),
                pane_id: "pane-2".to_owned(),
                session_id: runtime::SessionId(2),
            }),
        );
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], PanelAction::FocusPty(_)));

        let mut text = format!("{}한", "x".repeat(AGENT_SESSION_TEXT_INPUT_MAX_BYTES));
        truncate_utf8(&mut text, AGENT_SESSION_TEXT_INPUT_MAX_BYTES);
        assert_eq!(text.len(), AGENT_SESSION_TEXT_INPUT_MAX_BYTES);
        assert!(text.is_char_boundary(text.len()));
    }

    #[test]
    fn api_key_mutation_callbacks_advance_generation_only_after_success() {
        let mut state = AgentSessionsUi::new();
        state.api_key_pending = Some(ApiKeyMutationKind::Save);
        state.api_key_save_succeeded();
        assert_eq!(state.api_key_generation, 1);
        assert_eq!(state.api_key_pending, None);
        state.api_key_pending = Some(ApiKeyMutationKind::Delete);
        state.api_key_delete_succeeded();
        assert_eq!(state.api_key_generation, 2);
        assert_eq!(state.api_key_pending, None);
    }

    #[test]
    fn api_key_host_failure_stops_custom_client_before_spawn() {
        let host = Arc::new(CountingHost::default());
        let mut ui = AgentSessionsUi::new().with_app_server_host(host.clone());
        ui.sync_controller_config(&AgentsConfig {
            codex_llm_provider: Some("custom".to_owned()),
            codex_llm_base_url: Some("http://localhost:11434/v1".to_owned()),
            codex_llm_wire: None,
            disabled: Vec::new(),
        });
        let ctx = egui::Context::default();
        let error = ui.ensure_client(&ctx).unwrap_err();
        assert!(format!("{error:#}").contains("Failed to load the LLM API key"));
        assert!(ui.client.is_none());
        assert_eq!(host.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn production_source_has_no_concrete_secret_or_store_edge() {
        let source = include_str!("agent_sessions.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for forbidden in [
            ["storage", "::"].concat(),
            ["std::", "fs"].concat(),
            ["std::", "process"].concat(),
            ["request_repaint_", "after"].concat(),
            ["req", "west"].concat(),
            ["Tcp", "Stream"].concat(),
            ["Udp", "Socket"].concat(),
            ["clip", "board"].concat(),
            ["r", "fd::"].concat(),
            ["secret::", "SecretString"].concat(),
            ["CodexLlm", "ApiKeyStore"].concat(),
            ["Keyring", "SecretStore"].concat(),
            ["CodexAppServerClient", "::spawn"].concat(),
        ] {
            assert!(!source.contains(&forbidden), "forbidden edge: {forbidden}");
        }

        let render_source = source
            .split("    pub fn show(")
            .nth(1)
            .and_then(|tail| tail.split("    fn render_agent_controls(").next())
            .expect("show source region");
        for forbidden in [
            ["ensure_", "client("].concat(),
            ["apply_", "action("].concat(),
            ["sync_controller_", "config("].concat(),
            ["request_repaint_", "after("].concat(),
            ["::", "spawn("].concat(),
        ] {
            assert!(
                !render_source.contains(&forbidden),
                "render host edge: {forbidden}"
            );
        }
    }

    #[test]
    fn llm_provider_라벨_매핑() {
        let catalog = catalog();
        assert_eq!(
            llm_provider_label(None, &catalog),
            "Default (subscription/existing settings)"
        );
        assert_eq!(
            llm_provider_label(Some("oss"), &catalog),
            "Local OSS (ollama)"
        );
        assert_eq!(
            llm_provider_label(Some("custom"), &catalog),
            "Custom (OpenAI-compatible)"
        );
        // 미지값은 방어적으로 원문 표시 (config 로드 정규화가 1차 방어선).
        assert_eq!(llm_provider_label(Some("weird"), &catalog), "weird");
    }

    #[test]
    fn llm_wire_라벨_매핑() {
        let catalog = catalog();
        // None과 "chat"은 같은 기본 항목이다 (config에는 None으로 저장).
        assert_eq!(llm_wire_label(None, &catalog), "Chat Completions (default)");
        assert_eq!(
            llm_wire_label(Some("chat"), &catalog),
            "Chat Completions (default)"
        );
        assert_eq!(llm_wire_label(Some("responses"), &catalog), "Responses");
        assert_eq!(llm_wire_label(Some("weird"), &catalog), "weird");
    }

    #[test]
    fn relative_selection_wraps_across_pty_and_app_surfaces() {
        let mut ui = AgentSessionsUi::new();
        ui.pty_surfaces = vec![pty_surface(1), pty_surface(2)];
        ui.sessions.push(AgentSession::new(
            "app-1".to_owned(),
            "review".to_owned(),
            None,
        ));

        assert!(matches!(
            ui.select_relative(1),
            Some(AgentSurfaceId::Pty {
                session_id: runtime::SessionId(1),
                ..
            })
        ));
        assert!(
            matches!(ui.select_relative(-1), Some(AgentSurfaceId::Structured { session_id }) if session_id == "app-1")
        );
        assert!(matches!(
            ui.select_relative(1),
            Some(AgentSurfaceId::Pty {
                session_id: runtime::SessionId(1),
                ..
            })
        ));
    }

    #[test]
    fn structured_status_notice_keeps_exact_workspace_and_click_target() {
        let mut ui = AgentSessionsUi::new();
        let mut session =
            AgentSession::new("app-1".to_owned(), "First line\nmore".to_owned(), None);
        session.workspace_id = Some("ws-1".to_owned());
        ui.sessions.push(session);
        ui.apply_session_event(
            "app-1",
            AgentSessionEvent::TurnCompleted {
                status: "completed".to_owned(),
            },
        );

        assert_eq!(
            ui.drain_status_notices(),
            vec![AgentSessionStatusNotice {
                workspace_id: "ws-1".to_owned(),
                session_id: "app-1".to_owned(),
                title: "First line".to_owned(),
                status: AgentSessionStatus::Completed,
            }]
        );
        assert!(ui.open_session("app-1"));
        assert!(matches!(
            ui.selected_surface,
            Some(AgentSurfaceId::Structured { ref session_id }) if session_id == "app-1"
        ));
    }

    #[test]
    fn shortcut_approval_rechecks_zero_one_and_many_pending_requests() {
        let mut ui = AgentSessionsUi::new();
        ui.sessions.push(AgentSession::new(
            "app-1".to_owned(),
            "review".to_owned(),
            None,
        ));
        ui.open_session("app-1");
        let ctx = egui::Context::default();

        let zero = ui.approve_selected_once(&ctx).unwrap_err().to_string();
        assert!(zero.contains("currently 0"));

        ui.sessions[0].approvals.push(approval("one"));
        let one = ui.approve_selected_once(&ctx).unwrap_err().to_string();
        assert!(one.contains("Codex App Server is not connected"));

        ui.sessions[0].approvals.push(approval("two"));
        let many = ui.reject_selected(&ctx).unwrap_err().to_string();
        assert!(many.contains("currently 2"));
    }

    #[test]
    fn persisted_import_deduplicates_local_and_thread_ids_without_spawning() {
        let mut ui = AgentSessionsUi::new();
        let first = persisted_row("local-1", "thread-1");
        let mut refreshed = first.clone();
        refreshed.title = "갱신된 제목".to_owned();
        refreshed.cwd = "/repo/new".to_owned();
        let duplicate_thread = persisted_row("local-2", "thread-1");

        ui.import_persisted_threads(vec![first, refreshed, duplicate_thread]);

        assert!(ui.client.is_none());
        assert_eq!(ui.sessions.len(), 1);
        assert_eq!(ui.sessions[0].id, "local-1");
        assert_eq!(ui.sessions[0].prompt, "갱신된 제목");
        assert_eq!(ui.sessions[0].cwd.as_deref(), Some("/repo/new"));
        assert_eq!(ui.sessions[0].model.as_deref(), Some("gpt-test"));
        assert_eq!(ui.sessions[0].status, AgentSessionStatus::Stopped);
        assert!(ui.open_session("local-1"));
        assert!(matches!(
            ui.selected_surface,
            Some(AgentSurfaceId::Structured { ref session_id }) if session_id == "local-1"
        ));
    }

    #[test]
    fn persisted_projection_is_item_and_byte_bounded() {
        let mut ui = AgentSessionsUi::new();
        let rows = (0..=AGENT_SESSION_PERSISTED_MAX_ITEMS)
            .map(|index| persisted_row(&format!("local-{index}"), &format!("thread-{index}")))
            .collect();
        ui.import_persisted_threads(rows);
        assert_eq!(
            ui.persisted_threads.len(),
            AGENT_SESSION_PERSISTED_MAX_ITEMS
        );
        assert_eq!(ui.sessions.len(), AGENT_SESSION_PERSISTED_MAX_ITEMS);
        assert!(ui.persisted_thread_bytes <= AGENT_SESSION_PERSISTED_TOTAL_MAX_BYTES);

        let before = ui.persisted_thread_bytes;
        let mut oversized = persisted_row("local-0", "thread-0");
        oversized.title = "x".repeat(AGENT_SESSION_PERSISTED_ROW_MAX_BYTES);
        ui.import_persisted_threads(vec![oversized]);
        assert_eq!(ui.persisted_thread_bytes, before);
        assert_ne!(
            ui.persisted_threads["local-0"].title.len(),
            AGENT_SESSION_PERSISTED_ROW_MAX_BYTES
        );
    }

    #[test]
    fn persisted_import_rejects_oversized_identity_without_panicking_or_retaining() {
        let mut ui = AgentSessionsUi::new();
        let row = persisted_row(&"i".repeat(1025), "thread-1");

        ui.import_persisted_threads(vec![row]);

        assert!(ui.sessions.is_empty());
        assert!(ui.persisted_threads.is_empty());
        assert_eq!(ui.persisted_thread_bytes, 0);
    }

    #[test]
    fn authoritative_persisted_catalog_replaces_exactly_and_removes_stale_placeholders() {
        let mut ui = AgentSessionsUi::new();
        ui.import_persisted_threads(vec![
            persisted_row("local-1", "thread-1"),
            persisted_row("local-stale", "thread-stale"),
        ]);
        assert!(ui.open_session("local-stale"));
        let mut refreshed = persisted_row("local-1", "thread-1");
        refreshed.title = "authoritative title".to_owned();
        refreshed.workspace_id = "ws-2".to_owned();

        ui.replace_persisted_threads(vec![
            refreshed.clone(),
            persisted_row("local-2", "thread-2"),
        ])
        .unwrap();

        assert_eq!(ui.persisted_threads.len(), 2);
        assert_eq!(ui.persisted_threads["local-1"], refreshed);
        assert!(!ui.persisted_threads.contains_key("local-stale"));
        assert!(
            !ui.sessions
                .iter()
                .any(|session| session.id == "local-stale")
        );
        assert_eq!(
            ui.sessions
                .iter()
                .find(|session| session.id == "local-1")
                .unwrap()
                .prompt,
            "authoritative title"
        );
        assert!(ui.sessions.iter().any(|session| session.id == "local-2"));
        assert!(ui.selected_session.is_none());
        assert!(ui.selected_surface.is_none());
        assert_eq!(
            ui.persisted_thread_bytes,
            ui.persisted_threads
                .values()
                .map(persisted_row_retained_bytes)
                .sum::<usize>()
        );
    }

    #[test]
    fn authoritative_catalog_never_removes_running_pending_or_runtime_event_sessions() {
        let mut ui = AgentSessionsUi::new();
        ui.import_persisted_threads(vec![
            persisted_row("event-owned", "thread-event"),
            persisted_row("pending-owned", "thread-pending"),
        ]);
        ui.apply_session_event(
            "event-owned",
            AgentSessionEvent::TurnStarted {
                turn_id: "turn-1".to_owned(),
            },
        );
        ui.apply_session_event("event-owned", AgentSessionEvent::Stopped);
        let (_pending_tx, pending_rx) = mpsc::channel();
        ui.pending_thread_requests.push(PendingThreadRequest::Read {
            session_id: "pending-owned".to_owned(),
            reply: pending_rx,
        });
        let mut running = AgentSession::new("running-owned".to_owned(), "runtime".to_owned(), None);
        running.status = AgentSessionStatus::Running;
        ui.sessions.push(running);

        ui.replace_persisted_threads(Vec::new()).unwrap();

        assert!(ui.persisted_threads.is_empty());
        assert_eq!(ui.persisted_thread_bytes, 0);
        assert!(ui.sessions.iter().any(|session| {
            session.id == "event-owned"
                && session.status == AgentSessionStatus::Stopped
                && session.turn_id.as_deref() == Some("turn-1")
        }));
        assert!(
            ui.sessions
                .iter()
                .any(|session| session.id == "pending-owned")
        );
        assert!(
            ui.sessions
                .iter()
                .any(|session| session.id == "running-owned")
        );
        assert_eq!(ui.pending_thread_requests.len(), 1);
        assert!(ui.persisted_placeholders.is_empty());
    }

    #[test]
    fn authoritative_catalog_rejections_roll_back_without_partial_application() {
        let mut ui = AgentSessionsUi::new();
        ui.import_persisted_threads(vec![persisted_row("kept", "thread-kept")]);
        let expected_rows = ui.persisted_threads.clone();
        let expected_bytes = ui.persisted_thread_bytes;
        let expected_sessions = ui.session_ids();
        let expected_placeholders = ui.persisted_placeholders.clone();

        let assert_unchanged = |ui: &AgentSessionsUi| {
            assert_eq!(ui.persisted_threads, expected_rows);
            assert_eq!(ui.persisted_thread_bytes, expected_bytes);
            assert_eq!(ui.session_ids(), expected_sessions);
            assert_eq!(ui.persisted_placeholders, expected_placeholders);
        };

        let too_many = (0..=AGENT_SESSION_PERSISTED_MAX_ITEMS)
            .map(|index| persisted_row(&format!("local-{index}"), &format!("thread-{index}")))
            .collect();
        assert_eq!(
            ui.replace_persisted_threads(too_many).unwrap_err(),
            AgentSessionPersistedCatalogError::TooManyItems
        );
        assert_unchanged(&ui);

        let mut oversized = persisted_row("valid-first", "thread-valid-first");
        oversized.title = "secret-row".repeat(AGENT_SESSION_PERSISTED_ROW_MAX_BYTES);
        assert_eq!(
            ui.replace_persisted_threads(vec![
                persisted_row("would-partially-apply", "thread-new"),
                oversized,
            ])
            .unwrap_err(),
            AgentSessionPersistedCatalogError::RowTooLarge
        );
        assert_unchanged(&ui);

        let total_overflow = (0..AGENT_SESSION_PERSISTED_MAX_ITEMS)
            .map(|index| {
                let mut row =
                    persisted_row(&format!("total-{index}"), &format!("total-thread-{index}"));
                row.title = "x".repeat(9_000);
                row
            })
            .collect();
        assert_eq!(
            ui.replace_persisted_threads(total_overflow).unwrap_err(),
            AgentSessionPersistedCatalogError::TotalBytesExceeded
        );
        assert_unchanged(&ui);

        assert_eq!(
            ui.replace_persisted_threads(vec![
                persisted_row("duplicate", "thread-a"),
                persisted_row("duplicate", "thread-b"),
            ])
            .unwrap_err(),
            AgentSessionPersistedCatalogError::DuplicateLocalSession
        );
        assert_unchanged(&ui);
        assert_eq!(
            ui.replace_persisted_threads(vec![
                persisted_row("local-a", "duplicate-thread"),
                persisted_row("local-b", "duplicate-thread"),
            ])
            .unwrap_err(),
            AgentSessionPersistedCatalogError::DuplicateThread
        );
        assert_unchanged(&ui);
        assert_eq!(
            ui.replace_persisted_threads(vec![persisted_row(
                &"invalid".repeat(1_024),
                "thread-invalid",
            )])
            .unwrap_err(),
            AgentSessionPersistedCatalogError::InvalidLocalSession
        );
        assert_unchanged(&ui);
    }

    #[test]
    fn persisted_catalog_debug_never_exposes_hostile_row_text() {
        let secret = "super-secret-prompt-and-path";
        let mut row = persisted_row(secret, secret);
        row.workspace_id = secret.to_owned();
        row.title = secret.to_owned();
        row.cwd = secret.to_owned();
        row.model = Some(secret.to_owned());

        let row_debug = format!("{row:?}");
        assert!(!row_debug.contains(secret));
        assert!(!format!("{:?}", AgentSessionPersistedCatalogError::RowTooLarge).contains(secret));
    }

    #[test]
    fn 생성_시_주입한_catalog가_첫_persisted_import에도_적용된다() {
        let en = i18n::Catalog::load("en-US").unwrap();
        let ko = i18n::Catalog::load("ko-KR").unwrap();
        let mut ui = AgentSessionsUi::new().with_catalog(&en);
        let mut row = persisted_row("local-ko", "thread-korean");
        row.title.clear();

        ui.import_persisted_threads(vec![row]);

        assert_eq!(ui.sessions[0].prompt, "Codex thread thread-k");
        ui.set_catalog(&ko);
        assert_eq!(ui.catalog.locale(), "ko-KR");
        assert_eq!(ui.sessions[0].prompt, "Codex 스레드 thread-k");
    }

    #[test]
    fn locale_변경은_보관된_catalog와_session_오류를_다시_렌더한다() {
        let en = i18n::Catalog::load("en-US").unwrap();
        let ko = i18n::Catalog::load("ko-KR").unwrap();
        let mut ui = AgentSessionsUi::new().with_catalog(&en);
        ui.sessions.push(AgentSession::new(
            "localized-error".to_owned(),
            "task".to_owned(),
            None,
        ));
        let error = anyhow::anyhow!("wire detail");
        ui.fail_session_localized(
            "localized-error",
            "agent_sessions.error.interrupt_failed",
            &error,
        );
        ui.catalog_error = Some(CatalogMessage::error(
            "agent_sessions.error.model_catalog_failed",
            &error,
        ));

        let english = ui.sessions[0].error.clone().unwrap();
        ui.set_catalog(&ko);

        assert_ne!(ui.sessions[0].error.as_deref(), Some(english.as_str()));
        assert_eq!(
            ui.sessions[0].error.as_deref(),
            Some(
                ko.t(
                    "agent_sessions.error.interrupt_failed",
                    &[("error", "wire detail")]
                )
                .as_str()
            )
        );
        assert_eq!(
            ui.catalog_error.as_ref().unwrap().render(&ko),
            ko.t(
                "agent_sessions.error.model_catalog_failed",
                &[("error", "wire detail")]
            )
        );
    }

    #[test]
    fn persistence_backlog_coalesces_every_transition_pair() {
        let cases = [
            (
                upsert_mutation("local", "old", false),
                upsert_mutation("local", "new", true),
                upsert_mutation("local", "new", true),
            ),
            (
                upsert_mutation("local", "old", false),
                archive_mutation("local", true),
                upsert_mutation("local", "old", true),
            ),
            (
                upsert_mutation("local", "old", false),
                delete_mutation("local"),
                delete_mutation("local"),
            ),
            (
                archive_mutation("local", false),
                upsert_mutation("local", "new", true),
                upsert_mutation("local", "new", true),
            ),
            (
                archive_mutation("local", false),
                archive_mutation("local", true),
                archive_mutation("local", true),
            ),
            (
                archive_mutation("local", false),
                delete_mutation("local"),
                delete_mutation("local"),
            ),
            (
                delete_mutation("local"),
                upsert_mutation("local", "recreated", false),
                upsert_mutation("local", "recreated", false),
            ),
            (
                delete_mutation("local"),
                archive_mutation("local", true),
                delete_mutation("local"),
            ),
            (
                delete_mutation("local"),
                delete_mutation("local"),
                delete_mutation("local"),
            ),
        ];
        for (first, second, expected) in cases {
            let mut backlog = AgentSessionPersistenceBacklog::default();
            backlog.try_push(first).unwrap();
            backlog.try_push(second).unwrap();
            assert_eq!(backlog.entries, vec![expected]);
            assert_eq!(backlog.retained_bytes, backlog.entries[0].retained_bytes());
        }
    }

    #[test]
    fn persistence_backlog_preserves_first_seen_order_across_independent_ids() {
        let mut backlog = AgentSessionPersistenceBacklog::default();
        backlog
            .try_push(upsert_mutation("local-a", "a", false))
            .unwrap();
        backlog.try_push(delete_mutation("local-b")).unwrap();
        backlog.try_push(archive_mutation("local-a", true)).unwrap();
        backlog
            .try_push(upsert_mutation("local-c", "c", false))
            .unwrap();
        backlog
            .try_push(upsert_mutation("local-b", "recreated", false))
            .unwrap();

        let drained = backlog.drain_bounded(AGENT_SESSION_PERSISTED_MAX_ITEMS);
        assert_eq!(
            drained
                .iter()
                .map(AgentSessionPersistenceMutation::local_session_id)
                .collect::<Vec<_>>(),
            vec!["local-a", "local-b", "local-c"]
        );
        assert!(matches!(
            &drained[0],
            AgentSessionPersistenceMutation::Upsert { archived: true, .. }
        ));
        assert!(matches!(
            &drained[1],
            AgentSessionPersistenceMutation::Upsert { title, .. } if title == "recreated"
        ));
    }

    #[test]
    fn persistence_backlog_accepts_exact_item_and_byte_bounds_then_rolls_back_plus_one() {
        let mut items = AgentSessionPersistenceBacklog::default();
        for index in 0..AGENT_SESSION_PERSISTED_MAX_ITEMS {
            items
                .try_push(delete_mutation(&format!("item-{index}")))
                .unwrap();
        }
        let item_bytes = items.retained_bytes;
        assert_eq!(items.len(), AGENT_SESSION_PERSISTED_MAX_ITEMS);
        assert_eq!(
            items.try_push(delete_mutation("item-overflow")),
            Err(AgentSessionPersistenceBacklogError::TooManyItems)
        );
        assert_eq!(items.len(), AGENT_SESSION_PERSISTED_MAX_ITEMS);
        assert_eq!(items.retained_bytes, item_bytes);

        let mut bytes = AgentSessionPersistenceBacklog::default();
        for index in
            0..(AGENT_SESSION_PERSISTED_TOTAL_MAX_BYTES / AGENT_SESSION_PERSISTED_ROW_MAX_BYTES)
        {
            let local_session_id = format!("byte-{index}");
            let thread_id = format!("thread-{local_session_id}");
            let fixed_bytes = local_session_id.len() + 1 + thread_id.len() + 1;
            let title = "x".repeat(AGENT_SESSION_PERSISTED_ROW_MAX_BYTES - fixed_bytes);
            bytes
                .try_push(AgentSessionPersistenceMutation::Upsert {
                    local_session_id,
                    workspace_id: "w".to_owned(),
                    thread_id,
                    title,
                    cwd: "/".to_owned(),
                    model: None,
                    favorite: false,
                    archived: false,
                })
                .unwrap();
        }
        assert_eq!(
            bytes.retained_bytes,
            AGENT_SESSION_PERSISTED_TOTAL_MAX_BYTES
        );
        let exact_entries = bytes.len();
        assert_eq!(
            bytes.try_push(delete_mutation("byte-overflow")),
            Err(AgentSessionPersistenceBacklogError::TotalBytesExceeded)
        );
        assert_eq!(bytes.len(), exact_entries);
        assert_eq!(
            bytes.retained_bytes,
            AGENT_SESSION_PERSISTED_TOTAL_MAX_BYTES
        );
    }

    #[test]
    fn persistence_backlog_canonicalizes_allocations_and_bounded_drain_releases_capacity() {
        let mut oversized_capacity = String::with_capacity(1024 * 1024);
        oversized_capacity.push_str("canonical");
        let mut backlog = AgentSessionPersistenceBacklog::default();
        backlog
            .try_push(upsert_mutation(
                "local-canonical",
                oversized_capacity,
                false,
            ))
            .unwrap();
        let AgentSessionPersistenceMutation::Upsert { title, .. } = &backlog.entries[0] else {
            panic!("expected upsert")
        };
        assert_eq!(title.capacity(), title.len());

        for index in 0..31 {
            backlog
                .try_push(delete_mutation(&format!("drain-{index}")))
                .unwrap();
        }
        let drained = backlog.drain_bounded(7);
        assert_eq!(drained.len(), 7);
        assert_eq!(drained.capacity(), drained.len());
        assert_eq!(backlog.entries.capacity(), backlog.entries.len());
        let remaining = backlog.len();
        assert_eq!(backlog.drain_bounded(usize::MAX).len(), remaining);
        assert!(backlog.entries.is_empty());
        assert_eq!(backlog.entries.capacity(), 0);
        assert_eq!(backlog.retained_bytes, 0);
    }

    #[test]
    fn persistence_backlog_failure_is_fail_closed_and_debug_is_hostile_safe() {
        let secret = "private-session-marker";
        let mutation = AgentSessionPersistenceMutation::Upsert {
            local_session_id: secret.to_owned(),
            workspace_id: "private-workspace".to_owned(),
            thread_id: "private-thread".to_owned(),
            title: "private-title".to_owned(),
            cwd: "/private/cwd".to_owned(),
            model: Some("private-model".to_owned()),
            favorite: false,
            archived: false,
        };
        let debug = format!("{mutation:?}");
        for raw in [
            secret,
            "private-workspace",
            "private-thread",
            "private-title",
            "/private/cwd",
            "private-model",
        ] {
            assert!(!debug.contains(raw));
        }

        let mut backlog = AgentSessionPersistenceBacklog::default();
        backlog.try_push(delete_mutation("safe")).unwrap();
        let before = backlog.entries.clone();
        let before_bytes = backlog.retained_bytes;
        assert_eq!(
            backlog.try_push(delete_mutation("../invalid\0id")),
            Err(AgentSessionPersistenceBacklogError::InvalidInput)
        );
        assert_eq!(backlog.entries, before);
        assert_eq!(backlog.retained_bytes, before_bytes);
        assert_eq!(
            format!("{:?}", AgentSessionPersistenceBacklogError::InvalidInput),
            "invalid_input"
        );
    }

    #[test]
    fn delete_admission_failure_preserves_ui_projection() {
        let mut ui = AgentSessionsUi::new();
        ui.import_persisted_threads(vec![persisted_row("delete-target", "thread-target")]);
        assert!(ui.open_session("delete-target"));
        for index in 0..AGENT_SESSION_PERSISTED_MAX_ITEMS {
            ui.persistence_backlog
                .try_push(delete_mutation(&format!("pending-{index}")))
                .unwrap();
        }

        let error = ui.delete_selected_persisted().unwrap_err();
        assert_eq!(error.to_string(), "too_many_items");
        assert!(ui.persisted_threads.contains_key("delete-target"));
        assert!(
            ui.sessions
                .iter()
                .any(|session| session.id == "delete-target")
        );
        assert_eq!(ui.selected_session.as_deref(), Some("delete-target"));
        let rendered = ui.transport_error.as_ref().unwrap().render(&ui.catalog);
        assert_eq!(rendered, "structured_persistence_too_many_items");
    }

    #[test]
    fn read_and_resume_replies_poll_nonblocking_and_keep_recovery_selected() {
        let mut read_ui = AgentSessionsUi::new();
        read_ui.import_persisted_threads(vec![persisted_row("local-read", "thread-read")]);
        assert!(read_ui.open_session("local-read"));
        let (read_tx, read_rx) = mpsc::channel();
        read_ui.mark_history_request_started("local-read");
        read_ui
            .pending_thread_requests
            .push(PendingThreadRequest::Read {
                session_id: "local-read".to_owned(),
                reply: read_rx,
            });
        read_ui.poll_thread_replies();
        assert_eq!(read_ui.pending_thread_requests.len(), 1);
        read_tx.send(Ok(thread_result("thread-read"))).unwrap();
        read_ui.poll_thread_replies();
        assert_eq!(read_ui.sessions[0].items[0].summary, "restored");
        assert_eq!(read_ui.sessions[0].status, AgentSessionStatus::Ready);
        assert!(!read_ui.attached_threads.contains("local-read"));
        assert!(
            read_ui
                .drain_persistence_mutations_bounded(AGENT_SESSION_PERSISTED_MAX_ITEMS)
                .is_empty()
        );

        let mut resume_ui = AgentSessionsUi::new();
        resume_ui.import_persisted_threads(vec![persisted_row("local-1", "thread-1")]);
        assert!(resume_ui.open_session("local-1"));
        let (resume_tx, resume_rx) = mpsc::channel();
        resume_ui.mark_history_request_started("local-1");
        resume_ui
            .pending_thread_requests
            .push(PendingThreadRequest::Resume {
                session_id: "local-1".to_owned(),
                reply: resume_rx,
            });
        resume_tx.send(Ok(thread_result("thread-1"))).unwrap();
        resume_ui.poll_thread_replies();

        assert!(resume_ui.attached_threads.contains("local-1"));
        assert_eq!(resume_ui.selected_session.as_deref(), Some("local-1"));
        assert!(matches!(
            resume_ui.selected_surface,
            Some(AgentSurfaceId::Structured { ref session_id }) if session_id == "local-1"
        ));
        assert_eq!(
            resume_ui.drain_persistence_mutations_bounded(AGENT_SESSION_PERSISTED_MAX_ITEMS),
            vec![AgentSessionPersistenceMutation::Upsert {
                local_session_id: "local-1".to_owned(),
                workspace_id: "ws-1".to_owned(),
                thread_id: "thread-1".to_owned(),
                title: "복구 작업".to_owned(),
                cwd: "/repo".to_owned(),
                model: Some("gpt-test".to_owned()),
                favorite: true,
                archived: false,
            }]
        );
    }

    #[test]
    fn archive_emits_db_mutation_only_after_app_server_success() {
        let mut ui = AgentSessionsUi::new();
        ui.import_persisted_threads(vec![persisted_row("local-1", "thread-1")]);
        ui.open_session("local-1");
        let (reply_tx, reply_rx) = mpsc::channel();
        ui.pending_thread_requests
            .push(PendingThreadRequest::Archive {
                session_id: "local-1".to_owned(),
                reply: reply_rx,
            });

        ui.poll_thread_replies();
        assert!(
            ui.drain_persistence_mutations_bounded(AGENT_SESSION_PERSISTED_MAX_ITEMS)
                .is_empty()
        );
        assert!(!ui.persisted_threads["local-1"].archived);
        assert_eq!(ui.selected_session.as_deref(), Some("local-1"));

        reply_tx.send(Ok(json!({}))).unwrap();
        ui.poll_thread_replies();
        assert!(!ui.persisted_threads.contains_key("local-1"));
        assert!(!ui.sessions.iter().any(|session| session.id == "local-1"));
        assert!(ui.selected_session.is_none());
        assert!(ui.selected_surface.is_none());
        assert_eq!(
            ui.drain_persistence_mutations_bounded(AGENT_SESSION_PERSISTED_MAX_ITEMS),
            vec![AgentSessionPersistenceMutation::SetArchived {
                local_session_id: "local-1".to_owned(),
                archived: true,
            }]
        );
    }

    #[test]
    fn deleting_persisted_thread_removes_only_its_projection() {
        let mut ui = AgentSessionsUi::new();
        ui.import_persisted_threads(vec![
            persisted_row("local-1", "thread-1"),
            persisted_row("local-2", "thread-2"),
        ]);
        ui.open_session("local-1");

        ui.delete_selected_persisted().unwrap();

        assert_eq!(ui.session_ids(), vec!["local-2".to_owned()]);
        assert!(!ui.persisted_threads.contains_key("local-1"));
        assert!(ui.persisted_threads.contains_key("local-2"));
        assert!(ui.selected_surface.is_none());
        assert_eq!(
            ui.drain_persistence_mutations_bounded(AGENT_SESSION_PERSISTED_MAX_ITEMS),
            vec![AgentSessionPersistenceMutation::Delete {
                local_session_id: "local-1".to_owned(),
            }]
        );
    }

    #[test]
    fn thread_started_emits_one_deduplicated_upsert() {
        let mut ui = AgentSessionsUi::new();
        let mut session = AgentSession::new(
            "local-new".to_owned(),
            "새 작업\n상세".to_owned(),
            Some("/repo".to_owned()),
        );
        session.workspace_id = Some("ws-1".to_owned());
        session.model = Some("gpt-test".to_owned());
        ui.sessions.push(session);

        for _ in 0..2 {
            ui.apply_session_event(
                "local-new",
                AgentSessionEvent::ThreadStarted {
                    thread_id: "thread-new".to_owned(),
                },
            );
        }

        let mutations = ui.drain_persistence_mutations_bounded(AGENT_SESSION_PERSISTED_MAX_ITEMS);
        assert_eq!(mutations.len(), 1);
        assert!(matches!(
            &mutations[0],
            AgentSessionPersistenceMutation::Upsert {
                local_session_id,
                thread_id,
                title,
                ..
            } if local_session_id == "local-new"
                && thread_id == "thread-new"
                && title == "새 작업"
        ));
    }

    #[test]
    fn pty_row_activation_focuses_exact_pane_immediately() {
        let mut ui = AgentSessionsUi::new();
        let expected = pty_surface(9).id;
        let mut actions = Vec::new();

        ui.activate_surface(expected.clone(), &mut actions);

        assert!(matches!(
            actions.as_slice(),
            [PanelAction::FocusPty(id)] if id == &expected
        ));
        assert_eq!(ui.selected_surface.as_ref(), Some(&expected));
    }

    #[test]
    fn completion_acknowledge_returns_controller_projection_to_idle() {
        let mut ui = AgentSessionsUi::new();
        let mut session = AgentSession::new("app-1".to_owned(), "done".to_owned(), None);
        session.thread_status = Some(crate::agent_session::AgentThreadStatus::Idle);
        session.status = AgentSessionStatus::Completed;
        ui.sessions.push(session);

        ui.apply_action(
            PanelAction::Acknowledge("app-1".to_owned()),
            &egui::Context::default(),
        );

        assert_eq!(ui.sessions[0].status, AgentSessionStatus::Ready);
        assert_eq!(
            AgentVisualState::from_structured(ui.sessions[0].status),
            AgentVisualState::Idle
        );
    }

    #[test]
    fn history_snapshot_is_applied_before_newer_stream_delta() {
        let mut ui = AgentSessionsUi::new();
        ui.import_persisted_threads(vec![persisted_row("local-1", "thread-1")]);
        let (reply_tx, reply_rx) = mpsc::channel();
        ui.pending_thread_requests.push(PendingThreadRequest::Read {
            session_id: "local-1".to_owned(),
            reply: reply_rx,
        });
        reply_tx.send(Ok(thread_result("thread-1"))).unwrap();

        // This is the same seam used by poll(): reply/snapshot first, stream
        // events second. Reversing it would erase " newer" here.
        ui.poll_thread_replies();
        ui.apply_session_event(
            "local-1",
            AgentSessionEvent::ItemDelta {
                item_id: "answer-1".to_owned(),
                delta: " newer".to_owned(),
            },
        );

        assert_eq!(ui.sessions[0].items[0].summary, "restored newer");
    }

    #[test]
    fn workspace_delete_is_fail_closed_for_attached_or_pending_threads_and_prunes_safe_rows() {
        let mut attached = AgentSessionsUi::new();
        attached.import_persisted_threads(vec![persisted_row("local-1", "thread-1")]);
        attached.attached_threads.insert("local-1".to_owned());
        assert!(attached.prepare_workspace_delete("ws-1").is_err());
        assert!(attached.persisted_threads.contains_key("local-1"));

        let mut pending = AgentSessionsUi::new();
        pending.import_persisted_threads(vec![persisted_row("local-1", "thread-1")]);
        let (_reply_tx, reply_rx) = mpsc::channel();
        pending
            .pending_thread_requests
            .push(PendingThreadRequest::Read {
                session_id: "local-1".to_owned(),
                reply: reply_rx,
            });
        assert!(pending.prepare_workspace_delete("ws-1").is_err());
        assert_eq!(pending.pending_thread_requests.len(), 1);

        let mut safe = AgentSessionsUi::new();
        let ws1 = persisted_row("local-1", "thread-1");
        let mut ws2 = persisted_row("local-2", "thread-2");
        ws2.workspace_id = "ws-2".to_owned();
        safe.import_persisted_threads(vec![ws1, ws2]);
        safe.open_session("local-1");
        safe.persistence_backlog
            .try_push(AgentSessionPersistenceMutation::Delete {
                local_session_id: "local-1".to_owned(),
            })
            .unwrap();
        safe.persistence_backlog
            .try_push(AgentSessionPersistenceMutation::Delete {
                local_session_id: "local-2".to_owned(),
            })
            .unwrap();

        safe.prepare_workspace_delete("ws-1").unwrap();
        assert_eq!(safe.session_ids(), vec!["local-2".to_owned()]);
        assert!(!safe.persisted_threads.contains_key("local-1"));
        assert!(safe.persisted_threads.contains_key("local-2"));
        assert!(safe.selected_surface.is_none());
        assert_eq!(safe.persistence_backlog.len(), 1);
        assert!(matches!(
            &safe.persistence_backlog.entries[0],
            AgentSessionPersistenceMutation::Delete { local_session_id }
                if local_session_id == "local-2"
        ));
    }

    #[test]
    fn attached_persisted_thread_cannot_be_archived_or_deleted() {
        let mut ui = AgentSessionsUi::new();
        ui.import_persisted_threads(vec![persisted_row("local-1", "thread-1")]);
        ui.open_session("local-1");
        ui.attached_threads.insert("local-1".to_owned());

        assert!(
            ui.archive_selected_persisted(&egui::Context::default())
                .unwrap_err()
                .to_string()
                .contains("Disconnect the resumed APP thread")
        );
        assert!(ui.delete_selected_persisted().is_err());
        assert!(ui.client.is_none());
        assert!(ui.persisted_threads.contains_key("local-1"));
    }

    /// app.rs의 PTY 「이어서 하기」 충돌 판정이 기대는 조회 — attach 안 된 thread,
    /// 다른 thread에 attach된 경우, 알려지지 않은 thread는 전부 None이어야 한다.
    #[test]
    fn attached_local_session_for_thread_finds_only_the_attached_match() {
        let mut ui = AgentSessionsUi::new();
        ui.import_persisted_threads(vec![
            persisted_row("local-1", "thread-1"),
            persisted_row("local-2", "thread-2"),
        ]);
        assert_eq!(ui.attached_local_session_for_thread("thread-1"), None);

        ui.attached_threads.insert("local-1".to_owned());
        assert_eq!(
            ui.attached_local_session_for_thread("thread-1"),
            Some("local-1")
        );
        assert_eq!(ui.attached_local_session_for_thread("thread-2"), None);
        assert_eq!(ui.attached_local_session_for_thread("unknown-thread"), None);
    }

    #[test]
    fn report_thread_attached_elsewhere_opens_panel_with_localized_notice() {
        let mut ui = AgentSessionsUi::new();
        assert!(!ui.is_open());

        ui.report_thread_attached_elsewhere();

        assert!(ui.is_open());
        let rendered = ui
            .transport_error
            .as_ref()
            .expect("notice set")
            .render(&ui.catalog);
        // catalog.t()는 키가 없으면 키 문자열 자체를 그대로 돌려준다(raw fallback) —
        // 번역이 실제로 등록됐는지는 렌더 결과가 키와 달라야만 보장된다.
        assert_ne!(rendered, "agent_sessions.error.thread_attached_elsewhere");
    }

    #[test]
    fn effort_shortcut_cycles_selected_session_and_submit_reads_that_value() {
        use crate::codex_app_server::{CodexModelInfo, CodexReasoningEffort};

        let mut ui = AgentSessionsUi::new();
        ui.model_catalog = vec![CodexModelInfo {
            id: "model-id".to_owned(),
            model: "gpt-test".to_owned(),
            display_name: "GPT Test".to_owned(),
            description: "test".to_owned(),
            is_default: true,
            default_reasoning_effort: "medium".to_owned(),
            supported_reasoning_efforts: ["low", "medium", "high"]
                .into_iter()
                .map(|effort| CodexReasoningEffort {
                    reasoning_effort: effort.to_owned(),
                    description: effort.to_owned(),
                })
                .collect(),
        }];
        let mut first = AgentSession::new("app-1".to_owned(), "first".to_owned(), None);
        first.model = Some("gpt-test".to_owned());
        first.effort = Some("medium".to_owned());
        first.skills = vec![AgentSkillSelection {
            name: "first-skill".to_owned(),
            path: "/skills/first".to_owned(),
        }];
        let mut second = AgentSession::new("app-2".to_owned(), "second".to_owned(), None);
        second.model = Some("gpt-test".to_owned());
        second.effort = Some("low".to_owned());
        second.skills = vec![AgentSkillSelection {
            name: "second-skill".to_owned(),
            path: "/skills/second".to_owned(),
        }];
        ui.sessions.extend([first, second]);

        ui.open_session("app-1");
        ui.adjust_selected_effort(1).unwrap();
        assert_eq!(
            ui.selected_surface_snapshot().unwrap().effort.as_deref(),
            Some("high")
        );
        assert_eq!(
            ui.session_turn_settings("app-1").unwrap().1.as_deref(),
            Some("high")
        );

        ui.open_session("app-2");
        assert_eq!(
            ui.session_turn_settings("app-2").unwrap().1.as_deref(),
            Some("low")
        );
        assert_eq!(
            ui.session_turn_settings("app-2").unwrap().2[0].name,
            "second-skill"
        );
        ui.open_session("app-1");
        assert_eq!(
            ui.session_turn_settings("app-1").unwrap().1.as_deref(),
            Some("high")
        );
        assert_eq!(
            ui.session_turn_settings("app-1").unwrap().2[0].name,
            "first-skill"
        );
    }

    #[test]
    fn catalog_failure_keeps_manual_controls_as_safe_fallback() {
        let mut ui = AgentSessionsUi::new();
        ui.new_model = "manual-model".to_owned();
        ui.new_effort = "manual-effort".to_owned();
        let (reply_tx, reply_rx) = mpsc::channel();
        ui.pending_model_catalog = Some(reply_rx);
        reply_tx
            .send(Err(anyhow::anyhow!("method unavailable")))
            .unwrap();

        ui.poll_catalog_replies();

        assert_eq!(ui.new_model, "manual-model");
        assert_eq!(ui.new_effort, "manual-effort");
        assert!(
            ui.catalog_error
                .as_ref()
                .unwrap()
                .render(&ui.catalog)
                .contains("method unavailable")
        );
        assert!(ui.pending_model_catalog.is_none());
    }

    #[test]
    fn stale_steer_error_preserves_completed_controller_status() {
        let mut ui = AgentSessionsUi::new();
        let mut session = AgentSession::new("app-1".to_owned(), "done".to_owned(), None);
        session.status = AgentSessionStatus::Completed;
        ui.sessions.push(session);

        ui.apply_session_event(
            "app-1",
            AgentSessionEvent::ControlError {
                message: "stale expectedTurnId".to_owned(),
            },
        );

        assert_eq!(ui.sessions[0].status, AgentSessionStatus::Completed);
        assert_eq!(
            ui.sessions[0].error.as_deref(),
            Some("control_error:stale_turn")
        );
        assert!(ui.drain_status_notices().is_empty());
    }

    #[test]
    fn next_turn_model_change_queues_deduplicated_structured_upsert() {
        let mut ui = AgentSessionsUi::new();
        ui.import_persisted_threads(vec![persisted_row("local-1", "thread-1")]);

        ui.apply_action(
            PanelAction::UpdateTurnControls {
                session_id: "local-1".to_owned(),
                model: Some("gpt-new".to_owned()),
                effort: Some("high".to_owned()),
                skills: Vec::new(),
            },
            &egui::Context::default(),
        );
        ui.apply_action(
            PanelAction::UpdateTurnControls {
                session_id: "local-1".to_owned(),
                model: Some("gpt-new".to_owned()),
                effort: Some("high".to_owned()),
                skills: Vec::new(),
            },
            &egui::Context::default(),
        );

        assert_eq!(ui.sessions[0].model.as_deref(), Some("gpt-new"));
        let mutations = ui.drain_persistence_mutations_bounded(AGENT_SESSION_PERSISTED_MAX_ITEMS);
        assert_eq!(mutations.len(), 1);
        assert!(matches!(
            &mutations[0],
            AgentSessionPersistenceMutation::Upsert { model, .. }
                if model.as_deref() == Some("gpt-new")
        ));
    }

    #[test]
    fn agents_text_focus는_window_밖_primary_click에서만_반납한다() {
        let field = egui::Id::new("agent-field");
        let other = egui::Id::new("other-field");
        let rect = egui::Rect::from_min_max(egui::pos2(10.0, 10.0), egui::pos2(100.0, 100.0));

        assert_eq!(
            agent_focus_to_surrender(
                Some(field),
                &[field],
                true,
                Some(egui::pos2(4.0, 4.0)),
                Some(rect),
                true,
            ),
            Some(field)
        );
        assert_eq!(
            agent_focus_to_surrender(
                Some(field),
                &[field],
                true,
                Some(egui::pos2(50.0, 50.0)),
                Some(rect),
                true,
            ),
            None
        );
        assert_eq!(
            agent_focus_to_surrender(
                Some(other),
                &[field],
                true,
                Some(egui::pos2(4.0, 4.0)),
                Some(rect),
                true,
            ),
            None
        );
    }

    #[test]
    fn agents가_접히거나_닫히면_숨겨진_text_focus를_즉시_반납한다() {
        let field = egui::Id::new("agent-field");
        let rect = egui::Rect::from_min_max(egui::pos2(10.0, 10.0), egui::pos2(100.0, 100.0));

        assert_eq!(
            agent_focus_to_surrender(Some(field), &[field], false, None, Some(rect), false),
            Some(field)
        );
        assert_eq!(
            agent_focus_to_surrender(
                Some(egui::Id::new("terminal")),
                &[field],
                false,
                None,
                Some(rect),
                false,
            ),
            None
        );
    }

    #[test]
    fn terminal_claim은_agents_text_focus와_지연_autofocus를_함께_반납한다() {
        let ctx = egui::Context::default();
        let field = egui::Id::new("agent-field");
        let mut ui = AgentSessionsUi::new();
        ui.text_input_ids.push(field);
        ui.focus_new_prompt = true;
        ui.focus_follow_up = true;
        ctx.memory_mut(|memory| memory.request_focus(field));

        assert!(ui.surrender_text_focus(&ctx));
        assert_eq!(ctx.memory(|memory| memory.focused()), None);
        assert!(!ui.focus_new_prompt);
        assert!(!ui.focus_follow_up);
    }

    #[test]
    fn terminal_claim은_이미_포커스된_다른_widget을_지우지_않는다() {
        let ctx = egui::Context::default();
        let field = egui::Id::new("agent-field");
        let terminal = egui::Id::new("terminal");
        let mut ui = AgentSessionsUi::new();
        ui.text_input_ids.push(field);
        ctx.memory_mut(|memory| memory.request_focus(terminal));

        assert!(!ui.surrender_text_focus(&ctx));
        assert_eq!(ctx.memory(|memory| memory.focused()), Some(terminal));
    }

    /// 창 분류 계약 표. app-server가 창을 `primary`/`secondary` 어느 자리에 담든
    /// **길이(windowDurationMins)**가 5시간/주간을 정하고, 위치 기반 옛 매핑은
    /// 길이를 판별할 수 없을 때만 쓴다.
    #[test]
    fn codex_사용량_창은_길이로_분류되고_위치는_판별_불가일_때만_쓴다() {
        let cases: &[(&str, serde_json::Value, crate::app::ProviderUsage)] = &[
            ("창 객체 자체가 없음", serde_json::json!(null), (None, None)),
            (
                "자리가 뒤바뀐 창 — 길이를 따라간다",
                serde_json::json!({
                    "primary": {"usedPercent": 81, "windowDurationMins": 10080},
                    "secondary": {"usedPercent": 21, "windowDurationMins": 300},
                }),
                (Some(21), Some(81)),
            ),
            (
                "주간 창만 있는 플랜 — 5시간 칸을 주간 값으로 채우지 않는다",
                serde_json::json!({
                    "primary": {"usedPercent": 22, "windowDurationMins": 10080},
                    "secondary": null,
                }),
                (None, Some(22)),
            ),
            (
                "5시간 창만 secondary에 있음",
                serde_json::json!({
                    "primary": null,
                    "secondary": {"usedPercent": 31, "windowDurationMins": 300},
                }),
                (Some(31), None),
            ),
            (
                "5시간 창이 둘 — 먼저 온 쪽을 쓰고 주간은 비운다",
                serde_json::json!({
                    "primary": {"usedPercent": 41, "windowDurationMins": 300},
                    "secondary": {"usedPercent": 42, "windowDurationMins": 300},
                }),
                (Some(41), None),
            ),
            (
                "주간 창이 둘 — 먼저 온 쪽을 쓰고 5시간은 비운다",
                serde_json::json!({
                    "primary": {"usedPercent": 51, "windowDurationMins": 10080},
                    "secondary": {"usedPercent": 52, "windowDurationMins": 10080},
                }),
                (None, Some(51)),
            ),
            (
                "usedPercent가 숫자가 아닌 창은 버린다",
                serde_json::json!({
                    "primary": {"usedPercent": "n/a", "windowDurationMins": 300},
                    "secondary": {"usedPercent": 61, "windowDurationMins": 10080},
                }),
                (None, Some(61)),
            ),
            (
                "길이가 숫자가 아니면 판별 불가 — 위치 폴백",
                serde_json::json!({
                    "primary": {"usedPercent": 71, "windowDurationMins": "300"},
                    "secondary": null,
                }),
                (Some(71), None),
            ),
            (
                "허용 오차 1분 안쪽은 받는다",
                serde_json::json!({
                    "primary": {"usedPercent": 81, "windowDurationMins": 10081},
                    "secondary": {"usedPercent": 82, "windowDurationMins": 299},
                }),
                (Some(82), Some(81)),
            ),
            (
                "허용 오차 반대쪽 경계도 받는다",
                serde_json::json!({
                    "primary": {"usedPercent": 83, "windowDurationMins": 10079},
                    "secondary": {"usedPercent": 84, "windowDurationMins": 301},
                }),
                (Some(84), Some(83)),
            ),
            (
                "허용 오차 밖은 판별 불가 — 위치 폴백",
                serde_json::json!({
                    "primary": {"usedPercent": 91, "windowDurationMins": 302},
                    "secondary": {"usedPercent": 92, "windowDurationMins": 10082},
                }),
                (Some(91), Some(92)),
            ),
            (
                "길이 필드가 아예 없는 구버전 응답 — 위치 폴백",
                serde_json::json!({
                    "primary": {"usedPercent": 61},
                    "secondary": {"usedPercent": 62},
                }),
                (Some(61), Some(62)),
            ),
            (
                "판별된 5시간 창이 위치 폴백을 이긴다",
                serde_json::json!({
                    "primary": {"usedPercent": 71, "windowDurationMins": 60},
                    "secondary": {"usedPercent": 72, "windowDurationMins": 300},
                }),
                (Some(72), None),
            ),
            (
                "판별된 주간 창이 위치 폴백을 이긴다",
                serde_json::json!({
                    "primary": {"usedPercent": 81, "windowDurationMins": 10080},
                    "secondary": {"usedPercent": 82, "windowDurationMins": 60},
                }),
                (None, Some(81)),
            ),
            (
                "범위를 벗어난 수치는 0~100으로 자른다",
                serde_json::json!({
                    "primary": {"usedPercent": 120.4, "windowDurationMins": 300},
                    "secondary": {"usedPercent": -3.0, "windowDurationMins": 10080},
                }),
                (Some(100), Some(0)),
            ),
        ];
        for (name, snapshot, expected) in cases {
            assert_eq!(
                classify_codex_rate_limit_windows(snapshot),
                *expected,
                "{name}"
            );
        }
    }

    /// 실제 app-server 응답 모양(2026-08 pro 계정) — 주간 창 하나만 오고
    /// `secondary`가 null이다. 이 응답에서 5시간 칸에 91이 들어가면 회귀다.
    #[test]
    fn 주간_창만_보고하는_응답은_5시간_칸을_비워둔다() {
        let snapshot = serde_json::json!({
            "rateLimits": {
                "limitId": "codex",
                "primary": {"usedPercent": 91, "windowDurationMins": 10080, "resetsAt": 1786160724},
                "secondary": null,
                "planType": "pro",
            }
        });
        let limits = snapshot.get("rateLimits").expect("rateLimits");
        assert_eq!(
            classify_codex_rate_limit_windows(limits),
            (None, Some(91)),
            "주간 91%가 5시간 칸에 복제되면 안 된다"
        );
    }

    /// 실제 app-server 응답 모양에서 플랜·리셋 시각·리셋 크레딧을 뽑는다.
    /// 리셋 시각은 길이로 판별된 창에서만 — 자리가 뒤바뀌어도 창을 따라간다.
    #[test]
    fn codex_메타는_플랜과_창별_리셋과_크레딧을_읽는다() {
        let snapshot = serde_json::json!({
            "rateLimits": {
                "limitId": "codex",
                "primary": {"usedPercent": 91, "windowDurationMins": 10_080, "resetsAt": 1_786_160_724i64},
                "secondary": null,
                "planType": "pro",
            },
            "rateLimitResetCredits": {"availableCount": 1},
        });
        assert_eq!(
            codex_usage_meta_from_reply(&snapshot),
            CodexUsageMeta {
                plan_type: Some("pro".to_owned()),
                five_hour_resets_at: None,
                weekly_resets_at: Some(1_786_160_724),
                reset_credits: Some(1),
            }
        );

        let plus_swapped = serde_json::json!({
            "rateLimits": {
                "primary": {"usedPercent": 81, "windowDurationMins": 10_080, "resetsAt": 200i64},
                "secondary": {"usedPercent": 21, "windowDurationMins": 300, "resetsAt": 100i64},
                "planType": "plus",
            },
        });
        assert_eq!(
            codex_usage_meta_from_reply(&plus_swapped),
            CodexUsageMeta {
                plan_type: Some("plus".to_owned()),
                five_hour_resets_at: Some(100),
                weekly_resets_at: Some(200),
                reset_credits: None,
            }
        );

        assert_eq!(
            codex_usage_meta_from_reply(&serde_json::json!({})),
            CodexUsageMeta::default(),
            "빈 응답이면 기본값 — poll이 직전 메타를 유지한다"
        );
    }
}
