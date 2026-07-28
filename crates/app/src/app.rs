use std::path::{Path, PathBuf};

use anyhow::Context as _;
use runtime::{InProcessRuntimeClient, RuntimeCommandSink, RuntimeEventReceiver};

use crate::config::Config;
use std::sync::Arc;

use crate::ui;
use secret::KeyringSecretStore;
use storage::Db;

/// 커스텀 상단 타이틀바 높이 — macOS 신호등(닫기/최소화/전체화면) 수직 중앙 정렬에도
/// 쓰인다(main.rs의 `set_traffic_light_titlebar_height`). 값이 바뀌면 신호등도 다시
/// 어긋나므로 두 곳이 이 상수 하나만 본다.
pub(crate) const TOP_BAR_HEIGHT: f32 = 38.0;
const APPROVAL_WAKE_MARKER: u8 = 1;
const APPROVAL_CONTROL_MARKER: u8 = 2;
const APPROVAL_COMMAND_CAP: usize = 8;
const APPROVAL_LAUNCH_CAP: usize = 8;
const APPROVAL_SPAWN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
const APPROVAL_DENIAL_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(60);
const APP_NOTICE_TEXT_MAX_BYTES: usize = 4 * 1024;
const ENV_PROFILE_PROJECTION_MAX: usize = 256;
const ENV_VARIABLE_PROJECTION_MAX: usize = 4_096;
const ENV_PROJECT_PROJECTION_MAX: usize = 256;
const ENV_AUX_WORKER_IDLE_TTL: std::time::Duration = std::time::Duration::from_secs(30);
const PENDING_SHUTDOWN_LIMIT: usize = 2;
#[derive(Clone, PartialEq, Eq)]
struct AppAgentStateScope {
    epoch: u64,
    workspace_id: String,
    structured_workspace_ids: Arc<[String]>,
    retained_bytes: usize,
}

impl AppAgentStateScope {
    fn new(
        epoch: u64,
        workspace_id: String,
        mut structured_workspace_ids: Vec<String>,
    ) -> Option<Self> {
        structured_workspace_ids.sort_unstable();
        structured_workspace_ids.dedup();
        if epoch == 0
            || structured_workspace_ids.is_empty()
            || structured_workspace_ids.len() > storage::AGENT_STATE_STRUCTURED_WORKSPACE_MAX
        {
            return None;
        }
        for value in std::iter::once(&workspace_id).chain(structured_workspace_ids.iter()) {
            if value.is_empty() || value.len() > 1_024 || value.as_bytes().contains(&0) {
                return None;
            }
        }
        let retained_bytes = std::mem::size_of::<Self>()
            .checked_add(workspace_id.capacity())?
            .checked_add(
                std::mem::size_of::<String>().checked_mul(structured_workspace_ids.capacity())?,
            )?
            .checked_add(
                structured_workspace_ids
                    .iter()
                    .try_fold(0usize, |total, value| total.checked_add(value.capacity()))?,
            )?;
        Some(Self {
            epoch,
            workspace_id,
            structured_workspace_ids: structured_workspace_ids.into(),
            retained_bytes,
        })
    }
}

impl std::fmt::Debug for AppAgentStateScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppAgentStateScope")
            .field("workspace_count", &self.structured_workspace_ids.len())
            .finish_non_exhaustive()
    }
}

struct AppResumeCandidate {
    pane_id: String,
    pane_title: String,
    session: runtime::SessionId,
    identity: storage::AgentSessionIdentity,
    manual: bool,
}

impl std::fmt::Debug for AppResumeCandidate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppResumeCandidate")
            .field("manual", &self.manual)
            .field("payload", &"[REDACTED]")
            .finish()
    }
}

struct AppProjectNameRequest {
    session: Option<runtime::SessionId>,
    cwd: String,
}

enum AppAgentStateProjectionKind {
    Hooks,
    Attention,
    Restore,
    BindingSync(storage::AgentSessionBindingReconcile),
    ResumeProbe {
        probes: Vec<crate::agent_detect::ResumeTranscriptProbe>,
        candidates: Vec<AppResumeCandidate>,
    },
    Catalog,
    ProjectNames {
        style: crate::config::SessionNameStyle,
        rows: Vec<AppProjectNameRequest>,
    },
}

struct AppAgentStateProjection {
    scope: Arc<AppAgentStateScope>,
    kind: AppAgentStateProjectionKind,
    retained_bytes: usize,
}

impl crate::agent_state_worker::RetainedBytes for AppAgentStateProjection {
    fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
}

impl std::fmt::Debug for AppAgentStateProjection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match &self.kind {
            AppAgentStateProjectionKind::Hooks => "hooks",
            AppAgentStateProjectionKind::Attention => "attention",
            AppAgentStateProjectionKind::Restore => "restore",
            AppAgentStateProjectionKind::BindingSync(_) => "binding_sync",
            AppAgentStateProjectionKind::ResumeProbe { .. } => "resume_probe",
            AppAgentStateProjectionKind::Catalog => "catalog",
            AppAgentStateProjectionKind::ProjectNames { .. } => "project_names",
        };
        formatter
            .debug_struct("AppAgentStateProjection")
            .field("kind", &kind)
            .field("payload", &"[REDACTED]")
            .finish()
    }
}

enum AppAgentStateExactKind {
    TurnDoneClear(storage::AgentTurnDoneClear),
    BindingDelete(storage::AgentSessionIdentity),
    StructuredBatch(Vec<storage::StructuredThreadMutation>),
    FinalBindingReconcile {
        binding: storage::AgentSessionBindingReconcile,
        turn_done_clears: Vec<storage::AgentTurnDoneClear>,
        structured_mutations: Vec<storage::StructuredThreadMutation>,
    },
}

struct AppAgentStateExactRequest {
    scope: Arc<AppAgentStateScope>,
    kind: AppAgentStateExactKind,
    retained_bytes: usize,
}

impl crate::agent_state_worker::RetainedBytes for AppAgentStateExactRequest {
    fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
}

impl std::fmt::Debug for AppAgentStateExactRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match &self.kind {
            AppAgentStateExactKind::TurnDoneClear(_) => "turn_done_clear",
            AppAgentStateExactKind::BindingDelete(_) => "binding_delete",
            AppAgentStateExactKind::StructuredBatch(_) => "structured_batch",
            AppAgentStateExactKind::FinalBindingReconcile { .. } => "binding_reconcile",
        };
        formatter
            .debug_struct("AppAgentStateExactRequest")
            .field("kind", &kind)
            .field("payload", &"[REDACTED]")
            .finish()
    }
}

struct AppResumeProbeResult {
    pane_id: String,
    pane_title: String,
    session: runtime::SessionId,
    identity: storage::AgentSessionIdentity,
    found: bool,
    cwd: Option<String>,
    manual: bool,
}

struct AppProjectNameResult {
    session: Option<runtime::SessionId>,
    cwd: String,
    name: Option<String>,
}

struct AppAgentStateSnapshot {
    scope: Arc<AppAgentStateScope>,
    storage: Option<storage::AgentStateSnapshot>,
    resume: Option<Vec<AppResumeProbeResult>>,
    project_names: Option<Vec<AppProjectNameResult>>,
    project_name_style: Option<crate::config::SessionNameStyle>,
    retained_bytes: usize,
}

impl crate::agent_state_worker::RetainedBytes for AppAgentStateSnapshot {
    fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
}

impl std::fmt::Debug for AppAgentStateSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppAgentStateSnapshot")
            .field("has_storage", &self.storage.is_some())
            .field("resume_count", &self.resume.as_ref().map_or(0, Vec::len))
            .field(
                "project_name_count",
                &self.project_names.as_ref().map_or(0, Vec::len),
            )
            .finish()
    }
}

struct AppAgentStateBackend {
    db_path: PathBuf,
    db: Option<Db>,
}

#[cfg(test)]
fn replace_complete_projection<T, E>(target: &mut T, projection: Result<T, E>) -> Result<(), E> {
    match projection {
        Ok(value) => {
            *target = value;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn retained_string_bytes(value: &String) -> usize {
    value.capacity()
}

fn retained_string_vec_bytes(values: &[String], capacity: usize) -> Option<usize> {
    std::mem::size_of::<String>()
        .checked_mul(capacity)?
        .checked_add(
            values
                .iter()
                .try_fold(0usize, |total, value| total.checked_add(value.capacity()))?,
        )
}

fn retained_agent_session_row_bytes(row: &storage::AgentSessionRow) -> Option<usize> {
    std::mem::size_of::<storage::AgentSessionRow>()
        .checked_add(row.pane_id.capacity())?
        .checked_add(row.kind.capacity())?
        .checked_add(row.session_id.capacity())
}

fn retained_binding_reconcile_bytes(
    value: &storage::AgentSessionBindingReconcile,
) -> Option<usize> {
    std::mem::size_of::<storage::AgentSessionBindingReconcile>()
        .checked_add(retained_string_vec_bytes(
            &value.live_pane_ids,
            value.live_pane_ids.capacity(),
        )?)?
        .checked_add(
            std::mem::size_of::<storage::AgentSessionRow>()
                .checked_mul(value.desired_bindings.capacity())?,
        )?
        .checked_add(
            value
                .desired_bindings
                .iter()
                .try_fold(0usize, |total, row| {
                    total.checked_add(
                        retained_agent_session_row_bytes(row)?
                            .saturating_sub(std::mem::size_of::<storage::AgentSessionRow>()),
                    )
                })?,
        )
}

fn retained_structured_mutations_bytes(
    values: &Vec<storage::StructuredThreadMutation>,
) -> Option<usize> {
    let mut total =
        std::mem::size_of::<storage::StructuredThreadMutation>().checked_mul(values.capacity())?;
    for value in values {
        let bytes = match value {
            storage::StructuredThreadMutation::Upsert(row) => [
                &row.local_session_id,
                &row.workspace_id,
                &row.thread_id,
                &row.title,
                &row.cwd,
            ]
            .into_iter()
            .try_fold(0usize, |sum, value| {
                sum.checked_add(retained_string_bytes(value))
            })?
            .checked_add(row.model.as_ref().map_or(0, String::capacity))?,
            storage::StructuredThreadMutation::SetArchived {
                local_session_id, ..
            }
            | storage::StructuredThreadMutation::Delete { local_session_id } => {
                retained_string_bytes(local_session_id)
            }
        };
        total = total.checked_add(bytes)?;
    }
    Some(total)
}

fn retained_turn_done_clears_bytes(values: &Vec<storage::AgentTurnDoneClear>) -> Option<usize> {
    std::mem::size_of::<storage::AgentTurnDoneClear>()
        .checked_mul(values.capacity())?
        .checked_add(values.iter().try_fold(0usize, |total, value| {
            total.checked_add(value.session_key.capacity())
        })?)
}

fn turn_done_clear_matches_workspace(
    clear: &storage::AgentTurnDoneClear,
    workspace_id: &str,
) -> bool {
    clear
        .session_key
        .rsplit_once(':')
        .is_some_and(|(workspace, session)| workspace == workspace_id && !session.is_empty())
}

impl AppAgentStateProjection {
    fn try_new(scope: Arc<AppAgentStateScope>, kind: AppAgentStateProjectionKind) -> Option<Self> {
        let payload_bytes = match &kind {
            AppAgentStateProjectionKind::Hooks
            | AppAgentStateProjectionKind::Attention
            | AppAgentStateProjectionKind::Restore
            | AppAgentStateProjectionKind::Catalog => 0,
            AppAgentStateProjectionKind::BindingSync(value) => {
                retained_binding_reconcile_bytes(value)?
            }
            AppAgentStateProjectionKind::ResumeProbe { probes, candidates } => {
                if probes.len() != candidates.len() {
                    return None;
                }
                std::mem::size_of::<AppResumeCandidate>()
                    .checked_mul(candidates.capacity())?
                    .checked_add(
                        std::mem::size_of::<crate::agent_detect::ResumeTranscriptProbe>()
                            .checked_mul(probes.capacity().checked_sub(probes.len())?)?,
                    )?
                    .checked_add(candidates.iter().try_fold(0usize, |total, value| {
                        total
                            .checked_add(value.pane_id.capacity())?
                            .checked_add(value.pane_title.capacity())?
                            .checked_add(value.identity.pane_id.capacity())?
                            .checked_add(value.identity.kind.capacity())?
                            .checked_add(value.identity.session_id.capacity())
                    })?)?
                    .checked_add(probes.iter().try_fold(0usize, |total, value| {
                        total.checked_add(value.retained_bytes())
                    })?)?
            }
            AppAgentStateProjectionKind::ProjectNames { rows, .. } => {
                std::mem::size_of::<AppProjectNameRequest>()
                    .checked_mul(rows.capacity())?
                    .checked_add(
                        rows.iter()
                            .try_fold(0usize, |total, row| total.checked_add(row.cwd.capacity()))?,
                    )?
            }
        };
        let retained_bytes = std::mem::size_of::<Self>().checked_add(payload_bytes)?;
        Some(Self {
            scope,
            kind,
            retained_bytes,
        })
    }
}

impl AppAgentStateExactRequest {
    fn try_new(scope: Arc<AppAgentStateScope>, kind: AppAgentStateExactKind) -> Option<Self> {
        let payload_bytes = match &kind {
            AppAgentStateExactKind::TurnDoneClear(value) => {
                std::mem::size_of::<storage::AgentTurnDoneClear>()
                    .checked_add(value.session_key.capacity())?
            }
            AppAgentStateExactKind::BindingDelete(value) => {
                std::mem::size_of::<storage::AgentSessionIdentity>()
                    .checked_add(value.pane_id.capacity())?
                    .checked_add(value.kind.capacity())?
                    .checked_add(value.session_id.capacity())?
            }
            AppAgentStateExactKind::StructuredBatch(values) => {
                retained_structured_mutations_bytes(values)?
            }
            AppAgentStateExactKind::FinalBindingReconcile {
                binding,
                turn_done_clears,
                structured_mutations,
            } => retained_binding_reconcile_bytes(binding)?
                .checked_add(retained_turn_done_clears_bytes(turn_done_clears)?)?
                .checked_add(retained_structured_mutations_bytes(structured_mutations)?)?,
        };
        let retained_bytes = std::mem::size_of::<Self>().checked_add(payload_bytes)?;
        Some(Self {
            scope,
            kind,
            retained_bytes,
        })
    }
}

fn prepare_structured_agent_state_prefix(
    scope: &Arc<AppAgentStateScope>,
    pending: &[storage::StructuredThreadMutation],
) -> Option<(usize, AppAgentStateExactRequest)> {
    let max_items = pending
        .len()
        .min(crate::agent_state_worker::AGENT_STATE_STRUCTURED_BATCH_MAX);
    for items in (1..=max_items).rev() {
        let mutations = pending[..items].to_vec().into_boxed_slice().into_vec();
        let Some(request) = AppAgentStateExactRequest::try_new(
            Arc::clone(scope),
            AppAgentStateExactKind::StructuredBatch(mutations),
        ) else {
            continue;
        };
        if request.retained_bytes
            <= crate::agent_state_worker::AGENT_STATE_STRUCTURED_BATCH_BYTES_MAX
        {
            return Some((items, request));
        }
    }
    None
}

fn app_agent_state_result_bytes(
    resume: Option<&Vec<AppResumeProbeResult>>,
    project_names: Option<&Vec<AppProjectNameResult>>,
) -> Option<usize> {
    let resume_bytes = resume.map_or(Some(0), |values| {
        std::mem::size_of::<AppResumeProbeResult>()
            .checked_mul(values.capacity())?
            .checked_add(values.iter().try_fold(0usize, |total, value| {
                total
                    .checked_add(value.pane_id.capacity())?
                    .checked_add(value.pane_title.capacity())?
                    .checked_add(value.identity.pane_id.capacity())?
                    .checked_add(value.identity.kind.capacity())?
                    .checked_add(value.identity.session_id.capacity())?
                    .checked_add(value.cwd.as_ref().map_or(0, String::capacity))
            })?)
    })?;
    let project_bytes = project_names.map_or(Some(0), |values| {
        std::mem::size_of::<AppProjectNameResult>()
            .checked_mul(values.capacity())?
            .checked_add(values.iter().try_fold(0usize, |total, value| {
                total
                    .checked_add(value.cwd.capacity())?
                    .checked_add(value.name.as_ref().map_or(0, String::capacity))
            })?)
    })?;
    resume_bytes.checked_add(project_bytes)
}

fn app_agent_state_reserved_bytes(
    resume: Option<&Vec<AppResumeProbeResult>>,
    project_names: Option<&Vec<AppProjectNameResult>>,
) -> Option<usize> {
    std::mem::size_of::<AppAgentStateSnapshot>()
        .checked_add(app_agent_state_result_bytes(resume, project_names)?)
}

impl crate::agent_state_worker::AgentStateBackend for AppAgentStateBackend {
    type ProjectionRequest = AppAgentStateProjection;
    type Snapshot = AppAgentStateSnapshot;
    type ExactRequest = AppAgentStateExactRequest;

    fn execute_job(
        &mut self,
        exact: Option<crate::agent_state_worker::AgentStateExactInput<'_, Self::ExactRequest>>,
        projections: &[crate::agent_state_worker::AgentStateProjectionInput<
            '_,
            Self::ProjectionRequest,
        >],
    ) -> Result<Self::Snapshot, crate::agent_state_worker::AgentStateErrorCode> {
        let scope = exact
            .as_ref()
            .map(|request| Arc::clone(&request.payload().scope))
            .or_else(|| {
                projections
                    .first()
                    .map(|request| Arc::clone(&request.payload().scope))
            })
            .ok_or(crate::agent_state_worker::AgentStateErrorCode::InvalidData)?;
        if exact
            .as_ref()
            .is_some_and(|request| request.payload().scope.as_ref() != scope.as_ref())
            || projections
                .iter()
                .any(|request| request.payload().scope.as_ref() != scope.as_ref())
        {
            return Err(crate::agent_state_worker::AgentStateErrorCode::Stale);
        }

        let mut storage_job = storage::AgentStateJob::projection(scope.workspace_id.clone());
        storage_job.structured_workspace_ids = scope.structured_workspace_ids.to_vec();
        storage_job.include_hook_status = false;
        storage_job.include_attention = false;
        storage_job.include_agent_sessions = false;
        storage_job.include_structured_threads = false;
        storage_job.include_archived_threads = false;
        let mut storage_needed = exact.is_some();
        if let Some(exact) = exact {
            match &exact.payload().kind {
                AppAgentStateExactKind::TurnDoneClear(value) => {
                    storage_job.turn_done_clears.push(value.clone());
                }
                AppAgentStateExactKind::BindingDelete(value) => {
                    storage_job.stale_binding_deletes.push(value.clone());
                }
                AppAgentStateExactKind::StructuredBatch(values) => {
                    storage_job.structured_mutations.clone_from(values);
                }
                AppAgentStateExactKind::FinalBindingReconcile {
                    binding,
                    turn_done_clears,
                    structured_mutations,
                } => {
                    storage_job.binding_reconcile = Some(binding.clone());
                    storage_job.turn_done_clears.clone_from(turn_done_clears);
                    storage_job
                        .structured_mutations
                        .clone_from(structured_mutations);
                }
            }
        }

        let mut resume = None;
        let mut project_names = None;
        let mut project_name_style = None;
        for projection in projections {
            match (projection.section(), &projection.payload().kind) {
                (
                    crate::agent_state_worker::AgentStateSection::Hooks,
                    AppAgentStateProjectionKind::Hooks,
                ) => {
                    storage_job.include_hook_status = true;
                    storage_needed = true;
                }
                (
                    crate::agent_state_worker::AgentStateSection::Attention,
                    AppAgentStateProjectionKind::Attention,
                ) => {
                    storage_job.include_attention = true;
                    storage_needed = true;
                }
                (
                    crate::agent_state_worker::AgentStateSection::Restore,
                    AppAgentStateProjectionKind::Restore,
                ) => {
                    storage_job.include_agent_sessions = true;
                    storage_needed = true;
                }
                (
                    crate::agent_state_worker::AgentStateSection::Catalog,
                    AppAgentStateProjectionKind::Catalog,
                ) => {
                    storage_job.include_structured_threads = true;
                    storage_job.include_activity_panes = true;
                    storage_needed = true;
                }
                (
                    crate::agent_state_worker::AgentStateSection::BindingSync,
                    AppAgentStateProjectionKind::BindingSync(value),
                ) => {
                    if storage_job.binding_reconcile.is_some() {
                        return Err(crate::agent_state_worker::AgentStateErrorCode::InvalidData);
                    }
                    storage_job.binding_reconcile = Some(value.clone());
                    storage_job.include_agent_sessions = true;
                    storage_needed = true;
                }
                (
                    crate::agent_state_worker::AgentStateSection::ResumeProbe,
                    AppAgentStateProjectionKind::ResumeProbe { probes, candidates },
                ) => {
                    if resume.is_some() {
                        return Err(crate::agent_state_worker::AgentStateErrorCode::InvalidData);
                    }
                    if probes.len() != candidates.len() {
                        return Err(crate::agent_state_worker::AgentStateErrorCode::InvalidData);
                    }
                    let results = crate::agent_detect::probe_resume_transcripts(probes);
                    let results = results.map_err(|error| match error {
                        crate::agent_detect::ResumeTranscriptProbeError::InvalidRequest => {
                            crate::agent_state_worker::AgentStateErrorCode::InvalidData
                        }
                        crate::agent_detect::ResumeTranscriptProbeError::ResourceLimit => {
                            crate::agent_state_worker::AgentStateErrorCode::ResourceLimit
                        }
                    })?;
                    let mut output = Vec::with_capacity(candidates.len());
                    for (candidate, result) in candidates.iter().zip(results) {
                        output.push(AppResumeProbeResult {
                            pane_id: candidate.pane_id.clone(),
                            pane_title: candidate.pane_title.clone(),
                            session: candidate.session,
                            identity: candidate.identity.clone(),
                            found: result.found(),
                            cwd: result.cwd().map(str::to_owned),
                            manual: candidate.manual,
                        });
                    }
                    resume = Some(output);
                }
                (
                    crate::agent_state_worker::AgentStateSection::ProjectNames,
                    AppAgentStateProjectionKind::ProjectNames { style, rows },
                ) => {
                    if project_names.is_some() {
                        return Err(crate::agent_state_worker::AgentStateErrorCode::InvalidData);
                    }
                    let output = rows
                        .iter()
                        .map(|row| AppProjectNameResult {
                            session: row.session,
                            cwd: row.cwd.clone(),
                            name: crate::agent_detect::project_display_name(&row.cwd, *style)
                                .filter(|name| !name.trim().is_empty()),
                        })
                        .collect();
                    project_names = Some(output);
                    project_name_style = Some(*style);
                }
                _ => return Err(crate::agent_state_worker::AgentStateErrorCode::InvalidData),
            }
        }

        let reserved_bytes =
            app_agent_state_reserved_bytes(resume.as_ref(), project_names.as_ref())
                .ok_or(crate::agent_state_worker::AgentStateErrorCode::ResourceLimit)?;
        let storage =
            if storage_needed {
                storage_job.snapshot_bytes_max =
                    crate::agent_state_worker::checked_projection_result_remaining(reserved_bytes)?;
                let _retention = storage::prepare_agent_state_job_for_retention(&mut storage_job)
                    .map_err(|error| match error {
                    storage::AgentStatePreparationErrorCode::InvalidInput => {
                        crate::agent_state_worker::AgentStateErrorCode::InvalidData
                    }
                    storage::AgentStatePreparationErrorCode::ResourceLimit => {
                        crate::agent_state_worker::AgentStateErrorCode::ResourceLimit
                    }
                })?;
                let db = match &mut self.db {
                    Some(db) => db,
                    slot @ None => slot.insert(Db::open(&self.db_path).map_err(|_| {
                        crate::agent_state_worker::AgentStateErrorCode::StorageUnavailable
                    })?),
                };
                Some(db.apply_agent_state_job(&storage_job).map_err(|_| {
                    crate::agent_state_worker::AgentStateErrorCode::StorageUnavailable
                })?)
            } else {
                crate::agent_state_worker::check_projection_result_total(reserved_bytes)?;
                None
            };
        // `apply_agent_state_job` checks actual output capacity against the exact remaining
        // budget before commit. Therefore this sum is proven bounded on every successful return;
        // do not introduce a fallible post-commit branch that could misreport a durable exact.
        let retained_bytes = reserved_bytes
            + storage
                .as_ref()
                .map_or(0, storage::AgentStateSnapshot::retained_bytes);
        debug_assert!(
            crate::agent_state_worker::check_projection_result_total(retained_bytes).is_ok()
        );
        Ok(AppAgentStateSnapshot {
            scope,
            storage,
            resume,
            project_names,
            project_name_style,
            retained_bytes,
        })
    }
}

struct ApprovalSnapshot {
    rows: Vec<ui::approvals::PendingApprovalItem>,
}

/// Composition-root adapter for dotenv persistence. The orchestration module receives only its
/// bounded storage-neutral port; concrete rows and the production `Db` stay in app.rs.
struct AppDotenvRepository<'a>(&'a mut Db);

impl crate::dotenv_sync::DotenvRepository for AppDotenvRepository<'_> {
    fn credential_secret_location(
        &mut self,
        credential_id: &str,
    ) -> anyhow::Result<Option<crate::dotenv_sync::DotenvSecretLocation>> {
        Db::credential_secret_location(self.0, credential_id).map(|value| {
            value.map(|location| {
                crate::dotenv_sync::DotenvSecretLocation::new(
                    location.keyring_service,
                    location.keyring_username,
                )
            })
        })
    }

    fn acknowledge_physical_secret_slot_deleted(
        &mut self,
        logical_id: &str,
        physical_slot: &str,
    ) -> anyhow::Result<()> {
        Db::acknowledge_physical_secret_slot_deleted(self.0, logical_id, physical_slot).map(|_| ())
    }

    fn register_physical_secret_slot_staging(
        &mut self,
        logical_id: &str,
        physical_slot: &str,
    ) -> anyhow::Result<()> {
        Db::register_physical_secret_slot_staging(self.0, logical_id, physical_slot)
    }

    fn insert_credential_with_secret_slot(
        &mut self,
        draft: &crate::dotenv_sync::DotenvCredentialDraft,
        physical_slot: &str,
    ) -> anyhow::Result<()> {
        Db::insert_credential_with_secret_slot(
            self.0,
            &storage::CredentialMeta {
                id: draft.id().to_owned(),
                provider: draft.provider().to_owned(),
                label: draft.label().to_owned(),
                credential_kind: draft.credential_kind().to_owned(),
                masked_hint: draft.masked_hint().map(str::to_owned),
                workspace_id: draft.workspace_id().map(str::to_owned),
            },
            physical_slot,
            None,
        )
    }

    fn publish_credential_secret_slot_cas(
        &mut self,
        logical_id: &str,
        expected_previous_pointer: &str,
        physical_slot: &str,
        masked_hint: Option<&str>,
    ) -> anyhow::Result<bool> {
        Db::publish_credential_secret_slot_cas(
            self.0,
            logical_id,
            expected_previous_pointer,
            physical_slot,
            None,
            masked_hint,
        )
    }

    fn delete_credential_if_unused_cas(
        &mut self,
        logical_id: &str,
        expected_pointer: &str,
    ) -> anyhow::Result<bool> {
        Db::delete_credential_if_unused_cas(self.0, logical_id, expected_pointer)
    }

    fn list_env_profiles(
        &mut self,
        workspace_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::dotenv_sync::DotenvProfile>> {
        let rows = Db::list_env_profiles_bounded(self.0, workspace_id, limit)?;
        Ok(rows
            .into_iter()
            .map(|row| crate::dotenv_sync::DotenvProfile {
                id: row.id,
                kind: row.kind,
            })
            .collect())
    }

    fn insert_env_profile(
        &mut self,
        workspace_id: &str,
        name: &str,
        kind: &str,
    ) -> anyhow::Result<String> {
        Db::insert_env_profile(self.0, workspace_id, name, kind)
    }

    fn list_env_vars(
        &mut self,
        profile_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::dotenv_sync::DotenvVariable>> {
        let rows = Db::list_env_vars_bounded(self.0, profile_id, limit)?;
        Ok(rows
            .into_iter()
            .map(|row| crate::dotenv_sync::DotenvVariable {
                key: row.key,
                value: row.value,
            })
            .collect())
    }

    fn list_dotenv_owned_credential_ids(
        &mut self,
        limit: usize,
    ) -> anyhow::Result<std::collections::HashSet<String>> {
        Ok(Db::list_dotenv_owned_credential_ids_bounded(self.0, limit)?
            .into_iter()
            .collect())
    }

    fn plain_env_value_allowed(&mut self, key: &str, value: &str) -> bool {
        Db::validate_env_var_for_persistence(key, &crate::env::EnvValue::Plain(value.to_owned()))
            .is_ok()
    }

    fn upsert_env_var(
        &mut self,
        profile_id: &str,
        key: &str,
        value: &crate::env::EnvValue,
    ) -> anyhow::Result<()> {
        Db::upsert_env_var(self.0, profile_id, key, value)
    }

    fn delete_env_var(&mut self, profile_id: &str, key: &str) -> anyhow::Result<()> {
        Db::delete_env_var(self.0, profile_id, key)
    }

    fn delete_env_profile(&mut self, profile_id: &str) -> anyhow::Result<()> {
        Db::delete_env_profile(self.0, profile_id)
    }
}

fn sync_workspace_dotenv_at_root(
    db: &mut Db,
    secret_store: &dyn secret::SecretStore,
    redaction: &secret::RedactionService,
    workspace_id: &str,
    root: &Path,
) -> anyhow::Result<Option<crate::dotenv_sync::DotenvSyncReport>> {
    let Some(plan) = crate::dotenv_sync::load_workspace_dotenv_plan(root)? else {
        return Ok(None);
    };
    crate::dotenv_sync::apply_workspace_dotenv_plan(
        &mut AppDotenvRepository(db),
        secret_store,
        redaction,
        workspace_id,
        plan,
    )
    .map(Some)
}

enum ApprovalWorkerCommand {
    Resolve {
        id: String,
        allowed: bool,
        remember: bool,
        resolved_at: i64,
    },
    DenySession {
        workspace_id: String,
        session: runtime::SessionId,
        resolved_at: i64,
    },
    DenyAllOwned {
        resolved_at: i64,
    },
}

enum ApprovalWorkerResult {
    Resolved,
    SessionDenied {
        workspace_id: String,
        session: runtime::SessionId,
        succeeded: bool,
    },
    AllDenied {
        succeeded: bool,
    },
    Failed,
}

struct ApprovalListenerSlot {
    command_tx: std::sync::mpsc::SyncSender<ApprovalWorkerCommand>,
    ready_rx: std::sync::mpsc::Receiver<Result<ApprovalListenerReady, ApprovalWakeErrorCode>>,
    result_rx: std::sync::mpsc::Receiver<ApprovalWorkerResult>,
    handle: std::thread::JoinHandle<()>,
    socket_path: Option<PathBuf>,
    #[cfg(unix)]
    control_socket: Option<std::os::unix::net::UnixDatagram>,
    stop_requested: Arc<std::sync::atomic::AtomicBool>,
}

struct ApprovalListenerReady {
    socket_path: PathBuf,
    #[cfg(unix)]
    control_socket: std::os::unix::net::UnixDatagram,
}

#[cfg(unix)]
struct ApprovalListenerContext {
    db_path: PathBuf,
    socket_path: PathBuf,
    commands: std::sync::mpsc::Receiver<ApprovalWorkerCommand>,
    ready: std::sync::mpsc::SyncSender<Result<ApprovalListenerReady, ApprovalWakeErrorCode>>,
    results: std::sync::mpsc::SyncSender<ApprovalWorkerResult>,
    snapshot: Arc<std::sync::Mutex<Option<ApprovalSnapshot>>>,
    ctx: egui::Context,
    stop_requested: Arc<std::sync::atomic::AtomicBool>,
    pending_owner: Arc<storage::ActivePendingApprovalOwner>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalWakeErrorCode {
    #[cfg(not(unix))]
    Unsupported,
    Start,
    Storage,
    Bind,
    Backpressure,
    Delivery,
}

struct ApprovalWakeHub {
    db_path: PathBuf,
    ctx: egui::Context,
    snapshot: Arc<std::sync::Mutex<Option<ApprovalSnapshot>>>,
    slot: Option<ApprovalListenerSlot>,
    deferred: std::collections::VecDeque<ApprovalWorkerCommand>,
    inflight: usize,
    pending_owner: Arc<storage::ActivePendingApprovalOwner>,
}

struct PendingProxyLaunch {
    generation: u64,
    workspace_id: String,
    prepared: PreparedAgentLaunch,
}

struct ApprovalLaunchTicket {
    id: u64,
    workspace_id: String,
    agent_config_id: String,
    state: ApprovalLaunchTicketState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ApprovalLaunchTicketState {
    Preparing { deadline: std::time::Instant },
    SpawnSent,
}

#[derive(Default)]
struct ApprovalLaunchTracker {
    next_id: u64,
    pending: std::collections::VecDeque<ApprovalLaunchTicket>,
    live: std::collections::HashMap<(String, runtime::SessionId), u64>,
    exited: std::collections::HashSet<(String, runtime::SessionId)>,
    denying: std::collections::HashSet<(String, runtime::SessionId)>,
    denial_retries: std::collections::HashMap<(String, runtime::SessionId), ApprovalDenialRetry>,
}

struct ApprovalDenialRetry {
    failures: u8,
    retry_at: std::time::Instant,
}

#[derive(Default)]
struct ApprovalGlobalReconcile {
    required: bool,
    queued: bool,
    failures: u8,
    retry_at: Option<std::time::Instant>,
}

impl ApprovalGlobalReconcile {
    fn request(&mut self) {
        self.required = true;
    }

    fn is_due(&self, now: std::time::Instant) -> bool {
        self.required && !self.queued && self.retry_at.is_none_or(|retry_at| retry_at <= now)
    }

    fn mark_queued(&mut self) {
        self.queued = true;
        self.retry_at = None;
    }

    fn finish(&mut self, succeeded: bool, now: std::time::Instant) -> Option<std::time::Duration> {
        self.queued = false;
        if succeeded {
            self.required = false;
            self.failures = 0;
            self.retry_at = None;
            None
        } else {
            self.required = true;
            self.failures = self.failures.saturating_add(1);
            let shift = u32::from(self.failures.saturating_sub(1).min(6));
            let delay =
                std::time::Duration::from_secs(1u64 << shift).min(APPROVAL_DENIAL_RETRY_MAX);
            self.retry_at = Some(now + delay);
            Some(delay)
        }
    }
}

struct EnvProjectRowsJob {
    generation: u64,
    workspaces: Vec<storage::WorkspaceRow>,
}

struct EnvProjectRowsOutcome {
    generation: u64,
    rows: anyhow::Result<Vec<ui::env_project_list::EnvProjectRow>>,
}

type EnvProjectRowsWorker =
    crate::lazy_worker::LazyBoundedWorker<EnvProjectRowsJob, EnvProjectRowsOutcome>;

#[derive(Default)]
struct PendingShutdownRegistry {
    entries: Vec<PendingShutdown>,
}

struct PendingShutdown {
    workspace_id: String,
    completed: Arc<std::sync::atomic::AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

struct PendingShutdownCompletion {
    completed: Arc<std::sync::atomic::AtomicBool>,
    wake: egui::Context,
}

impl Drop for PendingShutdownCompletion {
    fn drop(&mut self) {
        // Publish completion before the only wake. The UI can therefore consume the wake and
        // deterministically reap a slot even during the tiny pre-JoinHandle::is_finished gap.
        self.completed
            .store(true, std::sync::atomic::Ordering::Release);
        self.wake.request_repaint();
    }
}

impl PendingShutdownRegistry {
    fn reap_finished(&mut self) -> bool {
        let mut reaped = false;
        let mut index = 0;
        while index < self.entries.len() {
            if self.entries[index]
                .completed
                .load(std::sync::atomic::Ordering::Acquire)
                || self.entries[index].handle.is_finished()
            {
                let pending = self.entries.remove(index);
                let _ = pending.handle.join();
                reaped = true;
            } else {
                index += 1;
            }
        }
        reaped
    }

    fn can_start(&mut self) -> bool {
        self.reap_finished();
        self.entries.len() < PENDING_SHUTDOWN_LIMIT
    }

    fn register(
        &mut self,
        workspace_id: String,
        completed: Arc<std::sync::atomic::AtomicBool>,
        handle: std::thread::JoinHandle<()>,
    ) {
        debug_assert!(self.entries.len() < PENDING_SHUTDOWN_LIMIT);
        self.entries.push(PendingShutdown {
            workspace_id,
            completed,
            handle,
        });
    }

    fn join_workspace(&mut self, workspace_id: &str) {
        let mut index = 0;
        while index < self.entries.len() {
            if self.entries[index].workspace_id == workspace_id {
                let pending = self.entries.remove(index);
                let _ = pending.handle.join();
            } else {
                index += 1;
            }
        }
        self.reap_finished();
    }

    fn join_all(&mut self) {
        for pending in self.entries.drain(..) {
            let _ = pending.handle.join();
        }
    }
}

impl Drop for PendingShutdownRegistry {
    fn drop(&mut self) {
        self.join_all();
    }
}

struct EnvSecretRevealJob {
    generation: u64,
    target: EnvSecretRevealTarget,
}

struct EnvSecretRevealOutcome {
    generation: u64,
    target: EnvSecretRevealTarget,
    value: anyhow::Result<secret::SecretString>,
}

enum EnvSecretRevealTarget {
    EnvRow {
        profile_id: String,
        key: String,
        credential_id: String,
    },
}

impl EnvSecretRevealTarget {
    fn credential_id(&self) -> &str {
        match self {
            Self::EnvRow { credential_id, .. } => credential_id,
        }
    }
}

type EnvSecretRevealWorker =
    crate::lazy_worker::LazyBoundedWorker<EnvSecretRevealJob, EnvSecretRevealOutcome>;

type DotenvState = (bool, Option<std::time::SystemTime>);

struct DotenvSyncJob {
    workspace_id: String,
    root: Option<PathBuf>,
    runtime_instance: u64,
    previous_state: Option<DotenvState>,
    force: bool,
    migrate_legacy: bool,
}

struct DotenvSyncPayload {
    report: Option<crate::dotenv_sync::DotenvSyncReport>,
    env_plain: Vec<(String, String)>,
    env_secrets: Vec<(String, String)>,
}

struct DotenvSyncOutcome {
    workspace_id: String,
    root: Option<PathBuf>,
    runtime_instance: u64,
    baseline: DotenvState,
    payload: Option<DotenvSyncPayload>,
}

enum PendingDotenvContinuation {
    WorkspaceProtocol {
        operation: ui::workspace::WorkspaceProtocolOperation,
        generation: u64,
        command: runtime::RuntimeCommand,
    },
    RuntimeCommand(runtime::RuntimeCommand),
    AgentLaunch {
        command: runtime::RuntimeCommand,
        approval_ticket: Option<u64>,
        launcher_request_id: Option<u64>,
    },
}

struct PendingDotenvOperation {
    correlation: crate::dotenv_sync::DotenvWorkerCorrelation,
    workspace_id: String,
    root: Option<PathBuf>,
    runtime_instance: u64,
    retained_bytes: usize,
    continuation: PendingDotenvContinuation,
}

fn prepare_dotenv_continuation_retention(
    continuation: &mut PendingDotenvContinuation,
) -> Result<usize, runtime::RuntimeCommandPreparationErrorCode> {
    match continuation {
        PendingDotenvContinuation::WorkspaceProtocol { command, .. }
        | PendingDotenvContinuation::RuntimeCommand(command)
        | PendingDotenvContinuation::AgentLaunch { command, .. } => {
            runtime::prepare_runtime_command_for_retention(command)
                .map(runtime::RuntimeCommandRetention::retained_bytes)
        }
    }
}

fn runtime_command_requires_dotenv(command: &runtime::RuntimeCommand) -> bool {
    matches!(
        command,
        runtime::RuntimeCommand::SpawnShell { .. }
            | runtime::RuntimeCommand::SpawnAgent { .. }
            | runtime::RuntimeCommand::SplitPane { .. }
            | runtime::RuntimeCommand::RestoreWorkspace
    )
}

struct AppDotenvResource {
    db_path: PathBuf,
    db: Option<Db>,
    redaction: secret::RedactionService,
}

type DotenvSyncWorker =
    crate::dotenv_sync::LazyDotenvWorker<DotenvSyncJob, DotenvSyncOutcome, AppDotenvResource>;

const SETTINGS_WORKER_IDLE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

struct SettingsSnapshotWorker {
    db_path: PathBuf,
    redaction: secret::RedactionService,
    ctx: egui::Context,
    slot: Option<SettingsWorkerSlot>,
    pending_result: Option<SettingsOutcome>,
}

struct SettingsWorkerSlot {
    tx: std::sync::mpsc::SyncSender<SettingsJob>,
    rx: std::sync::mpsc::Receiver<SettingsOutcome>,
    handle: std::thread::JoinHandle<()>,
    lifecycle: Arc<std::sync::Mutex<SettingsWorkerLifecycle>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SettingsWorkerLifecycle {
    Running,
    Exited,
}

enum SettingsTrySendError {
    Full(Box<SettingsJob>),
    Disconnected(Box<SettingsJob>),
}

struct SettingsWorkerExitGuard(Arc<std::sync::Mutex<SettingsWorkerLifecycle>>);

impl Drop for SettingsWorkerExitGuard {
    fn drop(&mut self) {
        *self.0.lock().unwrap_or_else(|poison| poison.into_inner()) =
            SettingsWorkerLifecycle::Exited;
    }
}

struct SettingsJob {
    generation: u64,
    revision: u64,
    workspace_id: String,
    project_root: Option<PathBuf>,
    action: SettingsJobAction,
}

/// fleet 배치 스폰 프롬프트(PR-S2) 바이트 상한 — FleetAction 핸들러에서 방어적으로
/// 재검증한다. prepare_agent_launch의 args-byte 상한(64KiB, 설정 args 포함)과는 별개로,
/// 패널이 비정상적으로 큰 값을 보내는 경우를 조기에 거부해 settings 잡 큐까지 가지
/// 않게 한다.
const FLEET_BATCH_SPAWN_PROMPT_MAX_BYTES: usize = 16 * 1024;

/// fleet 배치 스폰(PR-S1) 대기 상태 — settings 잡 큐가 단일 슬롯이라 프레임에 걸쳐
/// PrepareAgentLaunch를 하나씩 큐잉한다(`pump_batch_spawn`).
struct PendingBatchSpawn {
    agent_id: String,
    /// 남은 스폰 횟수 — 큐잉 성공마다 감소, 0이면 pending 상태를 지운다.
    remaining: u32,
    /// staging(버튼 클릭) 시점의 활성 workspace. 펌프 도중 활성 workspace가 바뀌면
    /// 엉뚱한 workspace로 이어 스폰되는 걸 막기 위해 남은 스폰을 전부 취소한다.
    staged_workspace_id: String,
    /// 선택된 저장 프롬프트의 렌더 결과(PR-S2) — Some이면 각 스폰마다 초기 argv
    /// 프롬프트로 전달된다. None이면 빈 세션(PR-S1과 동일).
    prompt: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkspaceMutationPurpose {
    SelectInSettings,
    SwitchRuntime,
}

enum SettingsJobAction {
    Load,
    AddCredential {
        credential: ui::credentials::NewCredential,
    },
    DeleteCredential {
        credential_id: String,
    },
    RevealCredential {
        credential_id: String,
    },
    ScanOrphanCredentials,
    PurgeOrphanCredentials {
        credential_ids: Vec<String>,
    },
    SaveCodexLlmApiKey {
        value: secret::SecretString,
    },
    DeleteCodexLlmApiKey,
    RegisterAgent(ui::agents::AgentRegistration),
    DeleteAgent {
        agent_id: String,
    },
    PrepareAgentLaunch {
        agent_id: String,
        profile_id: Option<String>,
        runtime_workspace_id: String,
        /// fleet 배치 스폰(PR-S2)의 렌더된 프롬프트 — 설정 args 뒤에 위치 인자로
        /// 덧붙는다. 일반 AgentsIntent::Run 경로는 항상 None.
        extra_arg: Option<String>,
    },
    PrepareQuickAgentLaunch {
        request_id: u64,
        spec: crate::agent_launcher::LaunchSpec,
        runtime_workspace_id: String,
    },
    FinalizeProxyAgentLaunch {
        prepared: Box<PreparedAgentLaunch>,
        approval_notify_socket: PathBuf,
    },
    DeleteLegacyVar {
        profile_id: String,
        key: String,
    },
    WriteDotenv {
        key: String,
        value: Option<String>,
    },
    ResyncDotenv,
    SetProjectPath {
        path: PathBuf,
    },
    RenameWorkspace {
        name: String,
    },
    FindOrCreateWorkspace {
        name: String,
        path: PathBuf,
        purpose: WorkspaceMutationPurpose,
    },
    AcceptMovedWorkspacePath {
        expected_old_path: String,
        expected_anchor: storage::WorkspaceFolderAnchor,
        new_path: PathBuf,
    },
}

struct SettingsOutcome {
    generation: u64,
    revision: u64,
    workspace_id: String,
    kind: SettingsOutcomeKind,
    snapshots: Option<SettingsSnapshots>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SettingsOperationKey {
    generation: u64,
    revision: u64,
    workspace_id: String,
}

impl SettingsOperationKey {
    fn for_job(job: &SettingsJob) -> Self {
        Self {
            generation: job.generation,
            revision: job.revision,
            workspace_id: job.workspace_id.clone(),
        }
    }

    fn matches_outcome(&self, outcome: &SettingsOutcome) -> bool {
        self.generation == outcome.generation
            && self.revision == outcome.revision
            && self.workspace_id == outcome.workspace_id
    }
}

enum SettingsOutcomeKind {
    Loaded,
    CredentialAdded(Result<(), SettingsErrorCode>),
    CredentialDeleted {
        credential_id: String,
        result: Result<(), SettingsErrorCode>,
    },
    CredentialRevealed {
        credential_id: String,
        result: Result<ui::credentials::RevealedCredential, SettingsErrorCode>,
    },
    OrphanCredentialsScanned(Result<Vec<String>, SettingsErrorCode>),
    OrphanCredentialsPurged {
        purged: usize,
        remaining: usize,
        result: Result<(), SettingsErrorCode>,
    },
    CodexLlmApiKeySaved(Result<(), SettingsErrorCode>),
    CodexLlmApiKeyDeleted(Result<(), SettingsErrorCode>),
    AgentRegistered(Result<(), SettingsErrorCode>),
    AgentDeleted(Result<(), SettingsErrorCode>),
    AgentLaunchPrepared(Result<PreparedAgentLaunch, SettingsErrorCode>),
    QuickAgentLaunchPrepared {
        request_id: u64,
        result: Result<PreparedAgentLaunch, SettingsErrorCode>,
    },
    AgentLaunchFinalized {
        ticket_id: Option<u64>,
        result: Result<PreparedAgentLaunch, SettingsErrorCode>,
    },
    LegacyVarDeleted(Result<(), SettingsErrorCode>),
    DotenvWritten(Result<(), SettingsErrorCode>),
    DotenvResynced(Result<(), SettingsErrorCode>),
    ProjectPathSet(Result<storage::SettingsWorkspaceProjectionRow, SettingsErrorCode>),
    WorkspaceRenamed {
        name: String,
        result: Result<(), SettingsErrorCode>,
    },
    WorkspaceFoundOrCreated {
        purpose: WorkspaceMutationPurpose,
        result: Result<storage::WorkspaceFindOrCreateResult, SettingsErrorCode>,
    },
    WorkspaceMovedPathAccepted {
        new_path: PathBuf,
        result: Result<storage::WorkspaceMovedPathUpdate, SettingsErrorCode>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsErrorCode {
    CredentialAdd,
    CredentialDelete,
    CredentialReveal,
    OrphanScan,
    OrphanPurge,
    ApiKeySave,
    ApiKeyDelete,
    Registration,
    Delete,
    Launch,
    LegacyDelete,
    DotenvWrite,
    DotenvSync,
    ProjectPath,
    WorkspaceMutation,
}

struct SettingsSnapshots {
    agents: ui::agents::AgentsSnapshot,
    env: ui::env_profiles::EnvProfilesSnapshot,
    credentials: ui::credentials::CredentialsSnapshot,
}

struct PreparedAgentLaunch {
    runtime_workspace_id: String,
    agent_config_id: String,
    command: String,
    args: Vec<String>,
    env_plain: Vec<(String, String)>,
    env_secrets: Vec<(String, String)>,
    waiting_regex: Option<String>,
    approval_regex: Option<String>,
    error_regex: Option<String>,
    done_regex: Option<String>,
    proxy: Option<PreparedProxyLaunch>,
    approval_ticket: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingAgentLauncherLaunch {
    request_id: u64,
    workspace_id: String,
    agent_config_id: String,
}

fn take_matching_agent_launcher_launch(
    pending: &mut Option<PendingAgentLauncherLaunch>,
    workspace_id: &str,
    agent_config_id: &str,
) -> Option<PendingAgentLauncherLaunch> {
    let matches = pending.as_ref().is_some_and(|launch| {
        launch.workspace_id == workspace_id && launch.agent_config_id == agent_config_id
    });
    matches.then(|| pending.take()).flatten()
}

struct PreparedProxyLaunch {
    server_id: String,
    config_flag: String,
}

fn dotenv_state_for_root(root: Option<&std::path::Path>) -> DotenvState {
    let Some(root) = root else {
        return (false, None);
    };
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    let mut exists = false;
    for name in crate::dotenv_sync::DOTENV_FILE_NAMES {
        match std::fs::metadata(root.join(name)) {
            Ok(meta) => {
                exists = true;
                true.hash(&mut hasher);
                meta.modified()
                    .ok()
                    .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_nanos())
                    .hash(&mut hasher);
            }
            Err(_) => false.hash(&mut hasher),
        }
    }
    let digest = hasher.finish();
    let surrogate = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(digest >> 1);
    (exists, exists.then_some(surrogate))
}

fn load_dotenv_default_env(db: &Db, workspace_id: &str) -> anyhow::Result<(EnvPairs, EnvPairs)> {
    let (mut env_plain, mut env_secrets) = (Vec::new(), Vec::new());
    if let Some(profile) = db
        .list_env_profiles_bounded(workspace_id, ENV_PROFILE_PROJECTION_MAX)?
        .into_iter()
        .find(|profile| profile.kind == crate::dotenv_sync::DOTENV_PROFILE_KIND)
    {
        for var in db.list_env_vars_bounded(&profile.id, ENV_VARIABLE_PROJECTION_MAX)? {
            match var.value {
                crate::env::EnvValue::Plain(value) => env_plain.push((var.key, value)),
                crate::env::EnvValue::Secret { credential_id } => {
                    env_secrets.push((var.key, credential_id));
                }
            }
        }
    }
    Ok((env_plain, env_secrets))
}

fn execute_dotenv_sync_job(
    resource: &mut AppDotenvResource,
    job: DotenvSyncJob,
) -> Result<DotenvSyncOutcome, crate::dotenv_sync::DotenvWorkerErrorCode> {
    // Capture the source immediately before reading. A change during the read differs from the
    // next event-driven request and cannot make an old result current.
    let baseline = dotenv_state_for_root(job.root.as_deref());
    let mut execute = || -> anyhow::Result<Option<DotenvSyncPayload>> {
        if !job.force && job.previous_state == Some(baseline) {
            return Ok(None);
        }
        let Some(root) = job.root.as_deref() else {
            return Ok(Some(DotenvSyncPayload {
                report: None,
                env_plain: Vec::new(),
                env_secrets: Vec::new(),
            }));
        };
        let db = match &mut resource.db {
            Some(db) => db,
            slot @ None => slot.insert(Db::open(&resource.db_path)?),
        };
        if job.migrate_legacy {
            let mut repository = AppDotenvRepository(db);
            crate::dotenv_sync::migrate_legacy_profiles_to_dotenv(
                &mut repository,
                &KeyringSecretStore,
                &job.workspace_id,
                root,
            )?;
        }
        let report = sync_workspace_dotenv_at_root(
            db,
            &KeyringSecretStore,
            &resource.redaction,
            &job.workspace_id,
            root,
        )?;
        let (env_plain, env_secrets) = if report.is_some() {
            load_dotenv_default_env(db, &job.workspace_id)?
        } else {
            (Vec::new(), Vec::new())
        };
        Ok(Some(DotenvSyncPayload {
            report,
            env_plain,
            env_secrets,
        }))
    };
    match execute() {
        Ok(payload) if dotenv_state_for_root(job.root.as_deref()) == baseline => {
            Ok(DotenvSyncOutcome {
                workspace_id: job.workspace_id,
                root: job.root,
                runtime_instance: job.runtime_instance,
                baseline,
                payload,
            })
        }
        Err(_) => {
            tracing::warn!(
                kind = "dotenv",
                phase = "synchronize",
                error_code = "execute_failed",
                "dotenv synchronization failed"
            );
            Err(crate::dotenv_sync::DotenvWorkerErrorCode::ExecuteFailed)
        }
        Ok(_) => {
            tracing::warn!(
                kind = "dotenv",
                phase = "source_verify",
                error_code = "source_changed",
                "dotenv source changed during synchronization"
            );
            Err(crate::dotenv_sync::DotenvWorkerErrorCode::ExecuteFailed)
        }
    }
}

fn new_dotenv_sync_worker(
    db_path: PathBuf,
    redaction: secret::RedactionService,
    ctx: egui::Context,
) -> DotenvSyncWorker {
    crate::dotenv_sync::LazyDotenvWorker::new(
        move || {
            Ok(AppDotenvResource {
                db_path: db_path.clone(),
                db: None,
                redaction: redaction.clone(),
            })
        },
        execute_dotenv_sync_job,
        move || ctx.request_repaint(),
    )
}

fn mcp_proxy_bin() -> anyhow::Result<String> {
    let exe = std::env::current_exe().context("현재 실행 파일 경로 조회 실패")?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("실행 파일 디렉터리를 알 수 없습니다"))?;
    let candidate = dir.join(format!("deppy-mcp-proxy{}", std::env::consts::EXE_SUFFIX));
    anyhow::ensure!(candidate.is_file(), "mcp_proxy_binary_missing");
    Ok(candidate.to_string_lossy().into_owned())
}

fn write_mcp_proxy_config(
    proxy_bin: &str,
    db_path: &std::path::Path,
    agent_id: &str,
    server_id: &str,
    approval_notify_socket: &std::path::Path,
) -> anyhow::Result<PathBuf> {
    let data_dir = db_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("mcp_proxy_config_parent_missing"))?;
    let dir = data_dir.join("mcp-proxy-configs");
    std::fs::create_dir_all(&dir).context("mcp_proxy_config_directory_create_failed")?;
    let path = dir.join(format!("{agent_id}.mcp.json"));
    let args = vec![
        "--db".to_owned(),
        db_path.to_string_lossy().into_owned(),
        "--server".to_owned(),
        server_id.to_owned(),
        "--approval-notify-socket".to_owned(),
        approval_notify_socket.to_string_lossy().into_owned(),
    ];
    let content = serde_json::to_vec_pretty(&serde_json::json!({
        "mcpServers": {
            "deppy-proxy": {
                "command": proxy_bin,
                "args": args,
            }
        }
    }))
    .context("mcp_proxy_config_serialize_failed")?;
    deppy_core::fs::atomic_write(&path, &content)
        .context("mcp_proxy_config_atomic_write_failed")?;
    Ok(path)
}

fn settings_agent_args_summary(args: &[String]) -> ui::agents::AgentArgsSummary {
    let bytes = args
        .iter()
        .map(String::len)
        .try_fold(0usize, usize::checked_add);
    if args.len() > ui::agents::AGENT_ARGS_MAX_ITEMS
        || bytes.is_none_or(|bytes| bytes > ui::agents::AGENT_ARGS_MAX_BYTES)
        || Db::validate_agent_args_for_persistence(args).is_err()
    {
        ui::agents::AgentArgsSummary::redacted()
    } else {
        ui::agents::AgentArgsSummary::visible(args.join(" "))
    }
}

fn load_agents_snapshot(db: &Db, revision: u64, workspace_id: &str) -> ui::agents::AgentsSnapshot {
    let loaded = (|| -> anyhow::Result<_> {
        let rows = db.settings_agents_snapshot_rows(workspace_id)?;
        let agents = rows
            .agents
            .into_iter()
            .filter(|row| !crate::agent_launcher::is_builtin_config_id(&row.id))
            .map(|row| {
                let summary = settings_agent_args_summary(&row.args);
                ui::agents::AgentListItem::new(row.id, row.name, row.command, summary)
            })
            .collect();
        let profiles = rows
            .profiles
            .into_iter()
            .map(|row| ui::agents::AgentProfileItem::new(row.id, row.name, row.is_production))
            .collect();
        let backends = rows
            .enabled_mcp_servers
            .into_iter()
            .map(|row| ui::agents::AgentBackendItem::new(row.id, row.name))
            .collect();
        ui::agents::AgentsSnapshot::try_new(revision, agents, profiles, backends)
            .map_err(Into::into)
    })();
    loaded.unwrap_or_else(|_| ui::agents::AgentsSnapshot::unavailable(revision))
}

fn load_environment_snapshots(
    db: &Db,
    revision: u64,
    workspace_id: &str,
    project_root_configured: bool,
) -> (
    ui::env_profiles::EnvProfilesSnapshot,
    ui::credentials::CredentialsSnapshot,
) {
    let loaded = (|| -> anyhow::Result<_> {
        let rows = db.settings_environment_snapshot_rows(workspace_id)?;
        let credential_ids = rows
            .credentials
            .iter()
            .map(|credential| credential.id.clone())
            .collect::<std::collections::HashSet<_>>();
        let profiles = rows.profiles;
        let legacy_profile_count = profiles
            .iter()
            .filter(|profile| profile.kind != crate::dotenv_sync::DOTENV_PROFILE_KIND)
            .count();
        let dotenv_profile_id = profiles
            .iter()
            .find(|profile| profile.kind == crate::dotenv_sync::DOTENV_PROFILE_KIND)
            .map(|profile| profile.id.clone());
        let mut dotenv_vars = Vec::new();
        let mut legacy_vars = Vec::new();
        let dotenv_profile_ids = profiles
            .iter()
            .filter(|profile| profile.kind == crate::dotenv_sync::DOTENV_PROFILE_KIND)
            .map(|profile| profile.id.clone())
            .collect::<std::collections::HashSet<_>>();
        let dotenv_referenced_credentials = rows
            .env_vars
            .iter()
            .filter(|row| dotenv_profile_ids.contains(&row.profile_id))
            .filter_map(|row| match &row.value {
                crate::env::EnvValue::Secret { credential_id } => Some(credential_id.clone()),
                crate::env::EnvValue::Plain(_) => None,
            })
            .collect::<std::collections::HashSet<_>>();
        let credentials = rows
            .credentials
            .into_iter()
            .filter(|credential| !dotenv_referenced_credentials.contains(&credential.id))
            .map(|credential| {
                ui::credentials::CredentialListItem::new(
                    credential.id,
                    credential.provider,
                    credential.label,
                    credential.credential_kind,
                    credential.masked_hint,
                )
            })
            .collect();
        for row in rows.env_vars {
            let has_os_override = std::env::var_os(&row.key).is_some();
            let view = match row.value {
                crate::env::EnvValue::Plain(value) => {
                    ui::env_profiles::EnvValueView::plain(value, has_os_override)
                }
                crate::env::EnvValue::Secret { credential_id } => {
                    let available = credential_ids.contains(&credential_id);
                    ui::env_profiles::EnvValueView::secret(
                        credential_id,
                        available,
                        has_os_override,
                    )
                }
            };
            let target = if dotenv_profile_ids.contains(&row.profile_id) {
                &mut dotenv_vars
            } else {
                &mut legacy_vars
            };
            target.push(ui::env_profiles::EnvVarItem::new(
                row.profile_id,
                row.key,
                view,
            ));
        }
        let env = ui::env_profiles::EnvProfilesSnapshot::try_new(
            revision,
            workspace_id,
            project_root_configured,
            dotenv_profile_id,
            legacy_profile_count,
            dotenv_vars,
            legacy_vars,
        )
        .map_err(anyhow::Error::from)?;
        let credentials = ui::credentials::CredentialsSnapshot::try_new(revision, credentials)
            .map_err(anyhow::Error::from)?;
        Ok((env, credentials))
    })();
    loaded.unwrap_or_else(|_| {
        (
            ui::env_profiles::EnvProfilesSnapshot::unavailable(
                revision,
                workspace_id,
                project_root_configured,
            ),
            ui::credentials::CredentialsSnapshot::unavailable(revision),
        )
    })
}

fn load_settings_snapshots(
    db: &Db,
    revision: u64,
    workspace_id: &str,
    project_root_configured: bool,
) -> SettingsSnapshots {
    let (env, credentials) =
        load_environment_snapshots(db, revision, workspace_id, project_root_configured);
    SettingsSnapshots {
        agents: load_agents_snapshot(db, revision, workspace_id),
        env,
        credentials,
    }
}

fn validate_settings_registration(
    registration: &ui::agents::AgentRegistration,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !registration.name.trim().is_empty() && registration.name.len() <= 4 * 1024,
        "settings_agent_name_invalid"
    );
    anyhow::ensure!(
        !registration.command.trim().is_empty() && registration.command.len() <= 4 * 1024,
        "settings_agent_command_invalid"
    );
    anyhow::ensure!(
        registration.args.len() <= ui::agents::AGENT_ARGS_MAX_ITEMS,
        "settings_agent_args_item_limit"
    );
    let arg_bytes = registration
        .args
        .iter()
        .map(String::len)
        .try_fold(0usize, usize::checked_add)
        .context("settings_agent_args_byte_overflow")?;
    anyhow::ensure!(
        arg_bytes <= ui::agents::AGENT_ARGS_MAX_BYTES,
        "settings_agent_args_byte_limit"
    );
    Db::validate_agent_args_for_persistence(&registration.args)?;
    for pattern in [
        &registration.waiting_regex,
        &registration.approval_regex,
        &registration.error_regex,
        &registration.done_regex,
    ]
    .into_iter()
    .flatten()
    {
        anyhow::ensure!(pattern.len() <= 4 * 1024, "settings_agent_regex_limit");
        regex::Regex::new(pattern).context("settings_agent_regex_invalid")?;
    }
    if let Some(flag) = registration.mcp_config_flag.as_deref() {
        anyhow::ensure!(
            flag.starts_with('-') && !flag.contains(char::is_whitespace) && flag.len() <= 256,
            "settings_agent_mcp_flag_invalid"
        );
    }
    anyhow::ensure!(
        !registration.mcp_proxy_enabled || registration.mcp_proxy_server_id.is_some(),
        "settings_agent_mcp_backend_missing"
    );
    Ok(())
}

fn prepare_agent_launch(
    db: &Db,
    workspace_id: &str,
    agent_id: &str,
    profile_id: Option<&str>,
    runtime_workspace_id: String,
    extra_arg: Option<String>,
) -> anyhow::Result<PreparedAgentLaunch> {
    let rows = db.settings_agent_launch_rows(workspace_id, agent_id, profile_id)?;
    let agent = rows.agent.context("settings_agent_missing")?;
    // extra_arg(배치 스폰 렌더된 프롬프트, PR-S2)도 같은 바이트 상한에 포함시켜, 설정
    // args만으로 초과할 때와 동일하게 실패하게 한다.
    let extra_bytes = extra_arg.as_deref().map_or(0usize, str::len);
    let arg_bytes = agent
        .args
        .iter()
        .map(String::len)
        .try_fold(extra_bytes, usize::checked_add)
        .context("settings_agent_args_byte_overflow")?;
    anyhow::ensure!(
        agent.args.len() <= ui::agents::AGENT_ARGS_MAX_ITEMS
            && arg_bytes <= ui::agents::AGENT_ARGS_MAX_BYTES,
        "settings_agent_args_limit"
    );
    Db::validate_agent_args_for_persistence(&agent.args)?;

    let mut env_plain = Vec::new();
    let mut env_secrets = Vec::new();
    if profile_id.is_some() {
        anyhow::ensure!(rows.profile.is_some(), "settings_profile_not_owned");
        for var in rows.env_vars {
            match var.value {
                crate::env::EnvValue::Plain(value) => env_plain.push((var.key, value)),
                crate::env::EnvValue::Secret { credential_id } => {
                    env_secrets.push((var.key, credential_id));
                }
            }
        }
    }

    let proxy = if agent.mcp_proxy_enabled {
        let server_id = agent
            .mcp_proxy_server_id
            .as_deref()
            .context("settings_agent_mcp_backend_missing")?;
        anyhow::ensure!(
            rows.mcp_backend_enabled,
            "settings_agent_mcp_backend_unavailable"
        );
        let flag = agent.mcp_config_flag.as_deref().unwrap_or("--mcp-config");
        anyhow::ensure!(
            flag.starts_with('-') && !flag.contains(char::is_whitespace) && flag.len() <= 256,
            "settings_agent_mcp_flag_invalid"
        );
        Some(PreparedProxyLaunch {
            server_id: server_id.to_owned(),
            config_flag: flag.to_owned(),
        })
    } else {
        None
    };

    // extra_arg(있으면)는 설정 args 뒤에 위치 인자로 덧붙인다 — `claude "<prompt>"` /
    // `codex "<prompt>"`와 동일한 형태로 각 CLI가 초기 프롬프트로 즉시 받는다(PR-S2).
    let mut args = agent.args;
    if let Some(extra) = extra_arg {
        args.push(extra);
    }

    Ok(PreparedAgentLaunch {
        runtime_workspace_id,
        agent_config_id: agent.id,
        command: agent.command,
        args,
        env_plain,
        env_secrets,
        waiting_regex: agent.waiting_regex,
        approval_regex: agent.approval_regex,
        error_regex: agent.error_regex,
        done_regex: agent.done_regex,
        proxy,
        approval_ticket: None,
    })
}

fn prepare_quick_agent_launch(
    db: &Db,
    workspace_id: &str,
    spec: crate::agent_launcher::LaunchSpec,
    runtime_workspace_id: String,
) -> anyhow::Result<PreparedAgentLaunch> {
    let (kind, agent_command, agent_args, env_plain) = spec.into_parts();
    Db::validate_agent_args_for_persistence(&agent_args)?;
    db.upsert_builtin_agent_config(kind.stable_config_id(), kind.label(), &agent_command)?;
    let mut prepared = prepare_agent_launch(
        db,
        workspace_id,
        kind.stable_config_id(),
        None,
        runtime_workspace_id,
        None,
    )?;
    let (command, args) = crate::agent_launcher::wrap_agent_then_shell(agent_command, agent_args);
    Db::validate_agent_args_for_persistence(&args)?;
    prepared.command = command;
    prepared.args = args;
    prepared.env_plain = env_plain;
    Ok(prepared)
}

fn finalize_proxy_agent_launch(
    mut prepared: PreparedAgentLaunch,
    db_path: &std::path::Path,
    approval_notify_socket: &std::path::Path,
) -> anyhow::Result<PreparedAgentLaunch> {
    let proxy = prepared
        .proxy
        .take()
        .context("settings_proxy_finalize_without_plan")?;
    let proxy_bin = mcp_proxy_bin()?;
    let path = write_mcp_proxy_config(
        &proxy_bin,
        db_path,
        &prepared.agent_config_id,
        &proxy.server_id,
        approval_notify_socket,
    )?;
    prepared.args.push(proxy.config_flag);
    prepared.args.push(path.to_string_lossy().into_owned());
    Ok(prepared)
}

fn add_settings_credential(
    db: &Db,
    store: &dyn secret::SecretStore,
    workspace_id: &str,
    credential: ui::credentials::NewCredential,
) -> anyhow::Result<()> {
    let (provider, label, credential_kind, input) = credential.into_parts();
    let access = secret::SecretString::new(input.into_inner());
    let logical = secret::LogicalCredentialId::new(uuid::Uuid::new_v4().to_string())?;
    let plan = secret::SecretBundleStagePlan::allocate(logical.clone(), None)?;
    db.register_physical_secret_slot_staging(logical.as_str(), plan.new_slot().as_str())?;
    if let Err(error) = secret::stage_secret_bundle(
        store,
        &plan,
        secret::SecretBundleRef::new(&access, None, None),
    ) {
        if secret::inspect_secret_bundle(store, plan.new_slot()).is_ok_and(|state| state.is_empty())
        {
            let _ = db.acknowledge_physical_secret_slot_deleted(
                logical.as_str(),
                plan.new_slot().as_str(),
            );
        }
        return Err(error);
    }
    let meta = storage::CredentialMeta {
        id: logical.as_str().to_owned(),
        provider,
        label,
        credential_kind,
        masked_hint: Some(secret::masked_hint(access.expose())),
        workspace_id: Some(workspace_id.to_owned()),
    };
    // A commit error has an indeterminate outcome. Keep the exact staged bundle and ledger row;
    // startup reconciliation decides from durable state and never guesses by deleting it here.
    db.insert_credential_with_secret_slot(&meta, plan.new_slot().as_str(), None)
}

fn settings_credential_slot(
    db: &Db,
    credential_id: &str,
) -> anyhow::Result<(secret::LogicalCredentialId, secret::PhysicalSecretSlot)> {
    let logical = secret::LogicalCredentialId::new(credential_id.to_owned())?;
    let location = db
        .credential_secret_location(credential_id)?
        .context("settings_credential_missing")?;
    anyhow::ensure!(
        location.keyring_service == secret::KEYRING_SERVICE,
        "settings_credential_service_invalid"
    );
    let slot = secret::PhysicalSecretSlot::parse(location.keyring_username)?;
    anyhow::ensure!(
        slot.belongs_to(&logical),
        "settings_credential_slot_owner_invalid"
    );
    Ok((logical, slot))
}

fn delete_settings_credential(
    db: &Db,
    store: &dyn secret::SecretStore,
    credential_id: &str,
) -> anyhow::Result<()> {
    let (logical, slot) = settings_credential_slot(db, credential_id)?;
    anyhow::ensure!(
        db.delete_credential_if_unused_cas(logical.as_str(), slot.as_str())?,
        "settings_credential_delete_stale_or_in_use"
    );
    // Metadata deletion atomically marks the exact slot Orphan. Cleanup failure is recoverable and
    // must not turn the already-committed delete into a stale UI row or an unsafe automatic retry.
    if secret::delete_secret_bundle(store, &slot).is_ok() {
        let _ = db.acknowledge_physical_secret_slot_deleted(logical.as_str(), slot.as_str())?;
    } else {
        tracing::warn!(
            kind = "credential",
            phase = "cleanup",
            error_code = "deferred",
            "credential physical slot cleanup deferred"
        );
    }
    Ok(())
}

fn reveal_settings_credential(
    db: &Db,
    store: &dyn secret::SecretStore,
    credential_id: &str,
) -> anyhow::Result<ui::credentials::RevealedCredential> {
    let (_, slot) = settings_credential_slot(db, credential_id)?;
    let (access, _, _) = secret::read_secret_bundle(store, &slot)?.into_parts();
    ui::credentials::RevealedCredential::new(credential_id, access.expose().to_owned())
        .map_err(Into::into)
}

fn scan_orphan_settings_credentials(
    db: &Db,
    store: &dyn secret::SecretStore,
) -> anyhow::Result<Vec<String>> {
    let known = db
        .list_credential_secret_records(secret::VERSIONED_SECRET_BUNDLE_SLOT_CEILING)?
        .into_iter()
        .map(|record| record.meta.id)
        .collect::<std::collections::HashSet<_>>();
    let mut orphans = store
        .list_secret_ids_bounded("")?
        .into_iter()
        .filter(|account| uuid_base(account).is_some_and(|base| !known.contains(base)))
        .collect::<Vec<_>>();
    orphans.sort();
    orphans.dedup();
    Ok(orphans)
}

fn purge_orphan_settings_credentials(
    store: &dyn secret::SecretStore,
    credential_ids: &[String],
) -> anyhow::Result<(usize, usize)> {
    let bytes = credential_ids
        .iter()
        .map(String::len)
        .try_fold(0usize, usize::checked_add)
        .context("settings_orphan_credential_bytes_overflow")?;
    anyhow::ensure!(
        credential_ids.len() <= 1_024 && bytes <= 1024 * 1024,
        "settings_orphan_credential_limit"
    );
    let mut purged = 0usize;
    for credential_id in credential_ids {
        anyhow::ensure!(
            uuid_base(credential_id).is_some(),
            "settings_orphan_credential_invalid"
        );
        if store.delete_secret(credential_id).is_ok() {
            purged = purged.saturating_add(1);
        }
    }
    Ok((purged, credential_ids.len().saturating_sub(purged)))
}

fn delete_legacy_secret_bundle(
    store: &dyn secret::SecretStore,
    logical_id: &str,
) -> anyhow::Result<()> {
    store.delete_secret(logical_id)?;
    store.delete_secret(&auth::refresh_entry_id(logical_id))?;
    store.delete_secret(&auth::dcr_secret_entry_id(logical_id))?;
    Ok(())
}

fn reconcile_physical_secret_ledger(
    db: &Db,
    store: &dyn secret::SecretStore,
) -> anyhow::Result<()> {
    let rows =
        db.physical_secret_slots_for_reconciliation(secret::VERSIONED_SECRET_BUNDLE_SLOT_CEILING)?;
    for row in rows {
        let logical = secret::LogicalCredentialId::new(row.logical_credential_id)?;
        let slot = secret::PhysicalSecretSlot::parse(row.physical_slot)?;
        anyhow::ensure!(
            slot.belongs_to(&logical),
            "startup_secret_slot_owner_invalid"
        );
        match row.state {
            storage::PhysicalSecretSlotState::Staging
            | storage::PhysicalSecretSlotState::Orphan => {
                secret::delete_secret_bundle(store, &slot)?;
                let _ =
                    db.acknowledge_physical_secret_slot_deleted(logical.as_str(), slot.as_str())?;
            }
            storage::PhysicalSecretSlotState::Published => {
                if let Some(legacy) = row.legacy_cleanup_username {
                    anyhow::ensure!(
                        legacy == logical.as_str(),
                        "startup_legacy_cleanup_owner_invalid"
                    );
                    delete_legacy_secret_bundle(store, &legacy)?;
                    let _ = db.acknowledge_legacy_secret_source_deleted(
                        logical.as_str(),
                        slot.as_str(),
                        &legacy,
                    )?;
                }
                anyhow::ensure!(
                    secret::inspect_secret_bundle(store, &slot)?.access,
                    "startup_published_secret_missing"
                );
            }
        }
    }
    Ok(())
}

fn migrate_legacy_secret_pointers(db: &Db, store: &dyn secret::SecretStore) -> anyhow::Result<()> {
    let records =
        db.list_credential_secret_records(secret::VERSIONED_SECRET_BUNDLE_SLOT_CEILING)?;
    for record in records {
        anyhow::ensure!(
            record.keyring_service == secret::KEYRING_SERVICE,
            "startup_credential_service_invalid"
        );
        let logical = secret::LogicalCredentialId::new(record.meta.id.clone())?;
        if record.keyring_username != logical.as_str() {
            let physical = secret::PhysicalSecretSlot::parse(record.keyring_username)?;
            anyhow::ensure!(
                physical.belongs_to(&logical),
                "startup_credential_slot_owner_invalid"
            );
            continue;
        }

        let access = store
            .get_secret(logical.as_str())
            .map_err(|_| anyhow::anyhow!("startup_legacy_access_read_failed"))?;
        let refresh_id = auth::refresh_entry_id(logical.as_str());
        let refresh = store
            .has_secret(&refresh_id)
            .map_err(|_| anyhow::anyhow!("startup_legacy_refresh_probe_failed"))?
            .then(|| store.get_secret(&refresh_id))
            .transpose()
            .map_err(|_| anyhow::anyhow!("startup_legacy_refresh_read_failed"))?;
        let dcr_id = auth::dcr_secret_entry_id(logical.as_str());
        let dcr = store
            .has_secret(&dcr_id)
            .map_err(|_| anyhow::anyhow!("startup_legacy_dcr_probe_failed"))?
            .then(|| store.get_secret(&dcr_id))
            .transpose()
            .map_err(|_| anyhow::anyhow!("startup_legacy_dcr_read_failed"))?;
        let plan = secret::SecretBundleStagePlan::allocate(logical.clone(), None)?;
        db.register_physical_secret_slot_staging(logical.as_str(), plan.new_slot().as_str())?;
        if let Err(error) = secret::stage_secret_bundle(
            store,
            &plan,
            secret::SecretBundleRef::new(&access, refresh.as_ref(), dcr.as_ref()),
        ) {
            if secret::inspect_secret_bundle(store, plan.new_slot())
                .is_ok_and(|state| state.is_empty())
            {
                let _ = db.acknowledge_physical_secret_slot_deleted(
                    logical.as_str(),
                    plan.new_slot().as_str(),
                );
            }
            return Err(error);
        }
        let published = db.publish_legacy_credential_secret_slot_cas(
            logical.as_str(),
            logical.as_str(),
            plan.new_slot().as_str(),
            record.oauth_json.as_deref(),
            record.meta.masked_hint.as_deref(),
        )?;
        if published {
            delete_legacy_secret_bundle(store, logical.as_str())?;
            let _ = db.acknowledge_legacy_secret_source_deleted(
                logical.as_str(),
                plan.new_slot().as_str(),
                logical.as_str(),
            )?;
        } else {
            secret::delete_secret_bundle(store, plan.new_slot())?;
            let _ = db.acknowledge_physical_secret_slot_deleted(
                logical.as_str(),
                plan.new_slot().as_str(),
            )?;
        }
    }
    Ok(())
}

fn reconcile_and_migrate_startup_secrets(
    db: &Db,
    store: &dyn secret::SecretStore,
) -> anyhow::Result<()> {
    reconcile_physical_secret_ledger(db, store)?;
    migrate_legacy_secret_pointers(db, store)?;
    reconcile_physical_secret_ledger(db, store)?;
    let published = db
        .physical_secret_slots_for_reconciliation(secret::VERSIONED_SECRET_BUNDLE_SLOT_CEILING)?
        .into_iter()
        .map(|row| {
            anyhow::ensure!(
                row.state == storage::PhysicalSecretSlotState::Published
                    && row.legacy_cleanup_username.is_none(),
                "startup_secret_ledger_not_converged"
            );
            Ok((row.logical_credential_id, row.physical_slot))
        })
        .collect::<anyhow::Result<std::collections::HashSet<_>>>()?;
    for record in db.list_credential_secret_records(secret::VERSIONED_SECRET_BUNDLE_SLOT_CEILING)? {
        anyhow::ensure!(
            record.keyring_service == secret::KEYRING_SERVICE
                && record.keyring_username != record.meta.id,
            "startup_logical_secret_pointer_remaining"
        );
        let logical = secret::LogicalCredentialId::new(record.meta.id)?;
        let slot = secret::PhysicalSecretSlot::parse(record.keyring_username)?;
        anyhow::ensure!(
            slot.belongs_to(&logical),
            "startup_final_secret_slot_owner_invalid"
        );
        anyhow::ensure!(
            published.contains(&(logical.as_str().to_owned(), slot.as_str().to_owned())),
            "startup_published_secret_ledger_missing"
        );
    }
    Ok(())
}

fn execute_settings_job(
    db: &mut Db,
    db_path: &std::path::Path,
    redaction: &secret::RedactionService,
    job: SettingsJob,
) -> SettingsOutcome {
    let SettingsJob {
        generation,
        revision,
        workspace_id,
        mut project_root,
        action,
    } = job;
    let mut refresh = false;
    let kind = match action {
        SettingsJobAction::Load => SettingsOutcomeKind::Loaded,
        SettingsJobAction::AddCredential { credential } => {
            let result =
                add_settings_credential(db, &KeyringSecretStore, &workspace_id, credential)
                    .map_err(|_| SettingsErrorCode::CredentialAdd);
            refresh = result.is_ok();
            SettingsOutcomeKind::CredentialAdded(result)
        }
        SettingsJobAction::DeleteCredential { credential_id } => {
            let result = delete_settings_credential(db, &KeyringSecretStore, &credential_id)
                .map_err(|_| SettingsErrorCode::CredentialDelete);
            refresh = result.is_ok();
            SettingsOutcomeKind::CredentialDeleted {
                credential_id,
                result,
            }
        }
        SettingsJobAction::RevealCredential { credential_id } => {
            let result = reveal_settings_credential(db, &KeyringSecretStore, &credential_id)
                .map_err(|_| SettingsErrorCode::CredentialReveal);
            SettingsOutcomeKind::CredentialRevealed {
                credential_id,
                result,
            }
        }
        SettingsJobAction::ScanOrphanCredentials => SettingsOutcomeKind::OrphanCredentialsScanned(
            scan_orphan_settings_credentials(db, &KeyringSecretStore)
                .map_err(|_| SettingsErrorCode::OrphanScan),
        ),
        SettingsJobAction::PurgeOrphanCredentials { credential_ids } => {
            let result = purge_orphan_settings_credentials(&KeyringSecretStore, &credential_ids)
                .map_err(|_| SettingsErrorCode::OrphanPurge);
            let (purged, remaining) = result
                .as_ref()
                .copied()
                .unwrap_or((0, credential_ids.len()));
            SettingsOutcomeKind::OrphanCredentialsPurged {
                purged,
                remaining,
                result: result.map(|_| ()),
            }
        }
        SettingsJobAction::SaveCodexLlmApiKey { value } => {
            let result = crate::codex_app_server::validate_llm_api_key(value.expose())
                .and_then(|_| {
                    secret::SecretStore::set_secret(
                        &KeyringSecretStore,
                        CODEX_LLM_API_KEY_ENTRY_ID,
                        &value,
                    )
                })
                .map_err(|_| SettingsErrorCode::ApiKeySave);
            SettingsOutcomeKind::CodexLlmApiKeySaved(result)
        }
        SettingsJobAction::DeleteCodexLlmApiKey => SettingsOutcomeKind::CodexLlmApiKeyDeleted(
            secret::SecretStore::delete_secret(&KeyringSecretStore, CODEX_LLM_API_KEY_ENTRY_ID)
                .map_err(|_| SettingsErrorCode::ApiKeyDelete),
        ),
        SettingsJobAction::RegisterAgent(registration) => {
            let result = validate_settings_registration(&registration)
                .and_then(|()| {
                    db.insert_agent_config(
                        registration.name.trim(),
                        registration.command.trim(),
                        &registration.args,
                        registration.waiting_regex.as_deref(),
                        registration.approval_regex.as_deref(),
                        registration.error_regex.as_deref(),
                        registration.done_regex.as_deref(),
                        registration.mcp_proxy_enabled,
                        registration.mcp_proxy_server_id.as_deref(),
                        registration.mcp_config_flag.as_deref(),
                    )
                    .map(|_| ())
                })
                .map_err(|_| SettingsErrorCode::Registration);
            refresh = result.is_ok();
            SettingsOutcomeKind::AgentRegistered(result)
        }
        SettingsJobAction::DeleteAgent { agent_id } => {
            let result = db
                .delete_agent_config(&agent_id)
                .map_err(|_| SettingsErrorCode::Delete);
            refresh = result.is_ok();
            SettingsOutcomeKind::AgentDeleted(result)
        }
        SettingsJobAction::PrepareAgentLaunch {
            agent_id,
            profile_id,
            runtime_workspace_id,
            extra_arg,
        } => SettingsOutcomeKind::AgentLaunchPrepared(
            prepare_agent_launch(
                db,
                &workspace_id,
                &agent_id,
                profile_id.as_deref(),
                runtime_workspace_id,
                extra_arg,
            )
            .map_err(|_| SettingsErrorCode::Launch),
        ),
        SettingsJobAction::PrepareQuickAgentLaunch {
            request_id,
            spec,
            runtime_workspace_id,
        } => {
            let result = prepare_quick_agent_launch(db, &workspace_id, spec, runtime_workspace_id)
                .map_err(|_| SettingsErrorCode::Launch);
            refresh = result.is_ok();
            SettingsOutcomeKind::QuickAgentLaunchPrepared { request_id, result }
        }
        SettingsJobAction::FinalizeProxyAgentLaunch {
            prepared,
            approval_notify_socket,
        } => {
            let ticket_id = prepared.approval_ticket;
            SettingsOutcomeKind::AgentLaunchFinalized {
                ticket_id,
                result: finalize_proxy_agent_launch(*prepared, db_path, &approval_notify_socket)
                    .map_err(|_| SettingsErrorCode::Launch),
            }
        }
        SettingsJobAction::DeleteLegacyVar { profile_id, key } => {
            let result = (|| -> anyhow::Result<()> {
                let profiles =
                    db.list_env_profiles_bounded(&workspace_id, ENV_PROFILE_PROJECTION_MAX)?;
                let profile = profiles
                    .iter()
                    .find(|profile| profile.id == profile_id)
                    .context("settings_legacy_profile_not_owned")?;
                anyhow::ensure!(
                    profile.kind != crate::dotenv_sync::DOTENV_PROFILE_KIND,
                    "settings_dotenv_delete_wrong_path"
                );
                db.delete_env_var(&profile_id, &key)?;
                if db
                    .list_env_vars_bounded(&profile_id, ENV_VARIABLE_PROJECTION_MAX)?
                    .is_empty()
                {
                    db.delete_env_profile(&profile_id)?;
                }
                Ok(())
            })()
            .map_err(|_| SettingsErrorCode::LegacyDelete);
            refresh = result.is_ok();
            SettingsOutcomeKind::LegacyVarDeleted(result)
        }
        SettingsJobAction::WriteDotenv { key, value } => {
            let result = project_root
                .as_deref()
                .context("settings_dotenv_root_missing")
                .and_then(|root| crate::dotenv_sync::write_env_var(root, &key, value.as_deref()))
                .map_err(|_| SettingsErrorCode::DotenvWrite);
            refresh = result.is_ok();
            SettingsOutcomeKind::DotenvWritten(result)
        }
        SettingsJobAction::ResyncDotenv => {
            let result = project_root
                .as_deref()
                .context("settings_dotenv_root_missing")
                .and_then(|root| {
                    {
                        let mut repository = AppDotenvRepository(db);
                        crate::dotenv_sync::migrate_legacy_profiles_to_dotenv(
                            &mut repository,
                            &KeyringSecretStore,
                            &workspace_id,
                            root,
                        )?;
                    }
                    sync_workspace_dotenv_at_root(
                        db,
                        &KeyringSecretStore,
                        redaction,
                        &workspace_id,
                        root,
                    )?;
                    Ok(())
                })
                .map_err(|_| SettingsErrorCode::DotenvSync);
            refresh = result.is_ok();
            SettingsOutcomeKind::DotenvResynced(result)
        }
        SettingsJobAction::SetProjectPath { path } => {
            let path_string = path.to_string_lossy().into_owned();
            let anchor = (!path_string.trim().is_empty())
                .then(|| App::folder_anchor(&path_string))
                .flatten();
            let result = db
                .set_workspace_path_and_anchor(
                    &workspace_id,
                    &path_string,
                    anchor.map(|value| value.0),
                    anchor.map(|value| value.1),
                )
                .map_err(|_| SettingsErrorCode::ProjectPath);
            if result.is_ok() && path_string.trim().is_empty() && {
                let mut repository = AppDotenvRepository(db);
                crate::dotenv_sync::remove_workspace_dotenv(
                    &mut repository,
                    &KeyringSecretStore,
                    &workspace_id,
                )
                .is_err()
            } {
                tracing::warn!(
                    kind = "settings",
                    phase = "dotenv_cleanup",
                    error_code = "dotenv_cleanup_failed",
                    "workspace dotenv cleanup failed after path detach"
                );
            }
            if result.is_ok() {
                project_root = path.is_dir().then_some(path.clone());
                refresh = true;
            }
            SettingsOutcomeKind::ProjectPathSet(result)
        }
        SettingsJobAction::RenameWorkspace { name } => {
            let result = db
                .rename_workspace(&workspace_id, name.trim())
                .map_err(|_| SettingsErrorCode::WorkspaceMutation);
            SettingsOutcomeKind::WorkspaceRenamed { name, result }
        }
        SettingsJobAction::FindOrCreateWorkspace {
            name,
            path,
            purpose,
        } => {
            let result = (|| {
                let path_string = path.to_string_lossy().into_owned();
                let (dev, ino) = App::folder_anchor(&path_string)
                    .context("settings_workspace_folder_anchor_missing")?;
                db.find_or_create_workspace_by_exact_path(
                    &name,
                    &path_string,
                    storage::WorkspaceFolderAnchor { dev, ino },
                )
            })()
            .map_err(|_| SettingsErrorCode::WorkspaceMutation);
            SettingsOutcomeKind::WorkspaceFoundOrCreated { purpose, result }
        }
        SettingsJobAction::AcceptMovedWorkspacePath {
            expected_old_path,
            expected_anchor,
            new_path,
        } => {
            let result = (|| {
                let new_path_string = new_path.to_string_lossy().into_owned();
                let (dev, ino) = App::folder_anchor(&new_path_string)
                    .context("settings_workspace_moved_anchor_missing")?;
                db.update_workspace_moved_path_cas(
                    &workspace_id,
                    &expected_old_path,
                    expected_anchor,
                    &new_path_string,
                    storage::WorkspaceFolderAnchor { dev, ino },
                )
            })()
            .map_err(|_| SettingsErrorCode::WorkspaceMutation);
            SettingsOutcomeKind::WorkspaceMovedPathAccepted { new_path, result }
        }
    };
    let snapshots = (refresh || matches!(kind, SettingsOutcomeKind::Loaded))
        .then(|| load_settings_snapshots(db, revision, &workspace_id, project_root.is_some()));
    SettingsOutcome {
        generation,
        revision,
        workspace_id,
        kind,
        snapshots,
    }
}

fn settings_open_failed_outcome(job: SettingsJob) -> SettingsOutcome {
    let kind = match job.action {
        SettingsJobAction::Load => SettingsOutcomeKind::Loaded,
        SettingsJobAction::AddCredential { .. } => {
            SettingsOutcomeKind::CredentialAdded(Err(SettingsErrorCode::CredentialAdd))
        }
        SettingsJobAction::DeleteCredential { credential_id } => {
            SettingsOutcomeKind::CredentialDeleted {
                credential_id,
                result: Err(SettingsErrorCode::CredentialDelete),
            }
        }
        SettingsJobAction::RevealCredential { credential_id } => {
            SettingsOutcomeKind::CredentialRevealed {
                credential_id,
                result: Err(SettingsErrorCode::CredentialReveal),
            }
        }
        SettingsJobAction::ScanOrphanCredentials => {
            SettingsOutcomeKind::OrphanCredentialsScanned(Err(SettingsErrorCode::OrphanScan))
        }
        SettingsJobAction::PurgeOrphanCredentials { credential_ids } => {
            SettingsOutcomeKind::OrphanCredentialsPurged {
                purged: 0,
                remaining: credential_ids.len(),
                result: Err(SettingsErrorCode::OrphanPurge),
            }
        }
        SettingsJobAction::SaveCodexLlmApiKey { .. } => {
            SettingsOutcomeKind::CodexLlmApiKeySaved(Err(SettingsErrorCode::ApiKeySave))
        }
        SettingsJobAction::DeleteCodexLlmApiKey => {
            SettingsOutcomeKind::CodexLlmApiKeyDeleted(Err(SettingsErrorCode::ApiKeyDelete))
        }
        SettingsJobAction::RegisterAgent(_) => {
            SettingsOutcomeKind::AgentRegistered(Err(SettingsErrorCode::Registration))
        }
        SettingsJobAction::DeleteAgent { .. } => {
            SettingsOutcomeKind::AgentDeleted(Err(SettingsErrorCode::Delete))
        }
        SettingsJobAction::PrepareAgentLaunch { .. } => {
            SettingsOutcomeKind::AgentLaunchPrepared(Err(SettingsErrorCode::Launch))
        }
        SettingsJobAction::PrepareQuickAgentLaunch { request_id, .. } => {
            SettingsOutcomeKind::QuickAgentLaunchPrepared {
                request_id,
                result: Err(SettingsErrorCode::Launch),
            }
        }
        SettingsJobAction::FinalizeProxyAgentLaunch { prepared, .. } => {
            SettingsOutcomeKind::AgentLaunchFinalized {
                ticket_id: prepared.approval_ticket,
                result: Err(SettingsErrorCode::Launch),
            }
        }
        SettingsJobAction::DeleteLegacyVar { .. } => {
            SettingsOutcomeKind::LegacyVarDeleted(Err(SettingsErrorCode::LegacyDelete))
        }
        SettingsJobAction::WriteDotenv { .. } => {
            SettingsOutcomeKind::DotenvWritten(Err(SettingsErrorCode::DotenvWrite))
        }
        SettingsJobAction::ResyncDotenv => {
            SettingsOutcomeKind::DotenvResynced(Err(SettingsErrorCode::DotenvSync))
        }
        SettingsJobAction::SetProjectPath { .. } => {
            SettingsOutcomeKind::ProjectPathSet(Err(SettingsErrorCode::ProjectPath))
        }
        SettingsJobAction::RenameWorkspace { name } => SettingsOutcomeKind::WorkspaceRenamed {
            name,
            result: Err(SettingsErrorCode::WorkspaceMutation),
        },
        SettingsJobAction::FindOrCreateWorkspace { purpose, .. } => {
            SettingsOutcomeKind::WorkspaceFoundOrCreated {
                purpose,
                result: Err(SettingsErrorCode::WorkspaceMutation),
            }
        }
        SettingsJobAction::AcceptMovedWorkspacePath { new_path, .. } => {
            SettingsOutcomeKind::WorkspaceMovedPathAccepted {
                new_path,
                result: Err(SettingsErrorCode::WorkspaceMutation),
            }
        }
    };
    SettingsOutcome {
        generation: job.generation,
        revision: job.revision,
        workspace_id: job.workspace_id,
        kind,
        snapshots: None,
    }
}

impl SettingsSnapshotWorker {
    fn new(db_path: PathBuf, redaction: secret::RedactionService, ctx: egui::Context) -> Self {
        Self {
            db_path,
            redaction,
            ctx,
            slot: None,
            pending_result: None,
        }
    }

    fn spawn_slot(&self) -> SettingsWorkerSlot {
        self.spawn_slot_with_idle_ttl(SETTINGS_WORKER_IDLE_TTL)
    }

    fn spawn_slot_with_idle_ttl(&self, idle_ttl: std::time::Duration) -> SettingsWorkerSlot {
        let (tx, jobs) = std::sync::mpsc::sync_channel::<SettingsJob>(1);
        let (results, rx) = std::sync::mpsc::sync_channel::<SettingsOutcome>(1);
        let db_path = self.db_path.clone();
        let redaction = self.redaction.clone();
        let ctx = self.ctx.clone();
        let lifecycle = Arc::new(std::sync::Mutex::new(SettingsWorkerLifecycle::Running));
        let thread_lifecycle = Arc::clone(&lifecycle);
        let handle = std::thread::Builder::new()
            .name("settings-snapshot".to_owned())
            .spawn(move || {
                let _exit_guard = SettingsWorkerExitGuard(Arc::clone(&thread_lifecycle));
                let mut db = None;
                loop {
                    let job = match jobs.recv_timeout(idle_ttl) {
                        Ok(job) => job,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            // Coordinate the final empty check with `try_send_to_slot`. If a
                            // sender won the lifecycle lock after recv_timeout fired, its job is
                            // observed here; otherwise Exited is published before any later send
                            // can report success.
                            let mut lifecycle = thread_lifecycle
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner());
                            match jobs.try_recv() {
                                Ok(job) => {
                                    drop(lifecycle);
                                    job
                                }
                                Err(std::sync::mpsc::TryRecvError::Empty)
                                | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                    *lifecycle = SettingsWorkerLifecycle::Exited;
                                    return;
                                }
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                    };
                    let db = match &mut db {
                        Some(db) => db,
                        slot @ None => match Db::open(&db_path) {
                            Ok(opened) => slot.insert(opened),
                            Err(_) => {
                                let outcome = settings_open_failed_outcome(job);
                                if results.send(outcome).is_err() {
                                    return;
                                }
                                ctx.request_repaint();
                                continue;
                            }
                        },
                    };
                    let outcome = execute_settings_job(db, &db_path, &redaction, job);
                    if results.send(outcome).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("settings snapshot worker thread spawn");
        SettingsWorkerSlot {
            tx,
            rx,
            handle,
            lifecycle,
        }
    }

    fn try_send_to_slot(
        slot: &SettingsWorkerSlot,
        job: SettingsJob,
    ) -> Result<(), SettingsTrySendError> {
        let lifecycle = slot
            .lifecycle
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if *lifecycle == SettingsWorkerLifecycle::Exited {
            return Err(SettingsTrySendError::Disconnected(Box::new(job)));
        }
        slot.tx.try_send(job).map_err(|error| match error {
            std::sync::mpsc::TrySendError::Full(job) => SettingsTrySendError::Full(Box::new(job)),
            std::sync::mpsc::TrySendError::Disconnected(job) => {
                SettingsTrySendError::Disconnected(Box::new(job))
            }
        })
    }

    fn reap_finished(&mut self) {
        if self
            .slot
            .as_ref()
            .is_some_and(|slot| slot.handle.is_finished())
            && let Some(slot) = self.slot.take()
        {
            if self.pending_result.is_none() {
                self.pending_result = slot.rx.try_recv().ok();
            }
            let _ = slot.handle.join();
        }
    }

    fn try_request_recover(&mut self, job: SettingsJob) -> Result<(), Box<SettingsJob>> {
        self.reap_finished();
        if self.pending_result.is_some() {
            return Err(Box::new(job));
        }
        if self.slot.is_none() {
            self.slot = Some(self.spawn_slot());
        }
        let Some(slot) = self.slot.as_ref() else {
            return Err(Box::new(job));
        };
        match Self::try_send_to_slot(slot, job) {
            Ok(()) => Ok(()),
            Err(SettingsTrySendError::Full(job)) => Err(job),
            Err(SettingsTrySendError::Disconnected(job)) => {
                if let Some(slot) = self.slot.take() {
                    let _ = slot.handle.join();
                }
                let slot = self.spawn_slot();
                let sent = Self::try_send_to_slot(&slot, *job);
                self.slot = Some(slot);
                match sent {
                    Ok(()) => Ok(()),
                    Err(SettingsTrySendError::Full(job))
                    | Err(SettingsTrySendError::Disconnected(job)) => Err(job),
                }
            }
        }
    }

    fn try_recv(&mut self) -> Option<SettingsOutcome> {
        if let Some(outcome) = self.pending_result.take() {
            return Some(outcome);
        }
        if let Ok(outcome) = self.slot.as_ref()?.rx.try_recv() {
            return Some(outcome);
        }
        self.reap_finished();
        self.pending_result.take()
    }
}

impl Drop for SettingsSnapshotWorker {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            drop(slot.tx);
            drop(slot.rx);
            let _ = slot.handle.join();
        }
    }
}

fn new_env_secret_reveal_worker(db_path: PathBuf, ctx: egui::Context) -> EnvSecretRevealWorker {
    EnvSecretRevealWorker::new(
        "env-secret-reveal",
        ENV_AUX_WORKER_IDLE_TTL,
        move || {
            let db_path = db_path.clone();
            let mut db = None;
            move |job: EnvSecretRevealJob| {
                let value = (|| -> anyhow::Result<secret::SecretString> {
                    let db = match &mut db {
                        Some(db) => db,
                        slot @ None => slot.insert(Db::open(&db_path)?),
                    };
                    let location = db
                        .credential_secret_location(job.target.credential_id())?
                        .context("credential_secret_location_missing")?;
                    anyhow::ensure!(
                        location.keyring_service == secret::KEYRING_SERVICE,
                        "credential_secret_service_mismatch"
                    );
                    let secret = secret::SecretStore::get_secret(
                        &KeyringSecretStore,
                        &location.keyring_username,
                    )?;
                    anyhow::ensure!(
                        secret.expose().len() <= ui::env_profiles::ENV_REVEALED_VALUE_MAX_BYTES,
                        "env_secret_value_limit"
                    );
                    Ok(secret)
                })();
                EnvSecretRevealOutcome {
                    generation: job.generation,
                    target: job.target,
                    value,
                }
            }
        },
        move || ctx.request_repaint(),
    )
}

fn new_env_project_rows_worker(db_path: PathBuf, ctx: egui::Context) -> EnvProjectRowsWorker {
    EnvProjectRowsWorker::new(
        "env-project-rows",
        ENV_AUX_WORKER_IDLE_TTL,
        move || {
            let db_path = db_path.clone();
            let mut db = None;
            move |job: EnvProjectRowsJob| {
                let rows = (|| -> anyhow::Result<_> {
                    if db.is_none() {
                        db = Some(Db::open(&db_path)?);
                    }
                    let db = db.as_ref().expect("DB initialized above");
                    db.env_api_project_counts_bounded(ENV_PROJECT_PROJECTION_MAX)
                        .map(|counts| {
                            let counts: std::collections::HashMap<_, _> = counts
                                .into_iter()
                                .map(|count| (count.workspace_id.clone(), count))
                                .collect();
                            job.workspaces
                                .iter()
                                .map(|workspace| {
                                    let count = counts.get(&workspace.id);
                                    let path = workspace.path.clone();
                                    ui::env_project_list::EnvProjectRow {
                                        id: workspace.id.clone(),
                                        name: App::workspace_display_name(workspace),
                                        alias: workspace.name.clone(),
                                        path_missing: !path.trim().is_empty()
                                            && !std::path::Path::new(&path).is_dir(),
                                        path,
                                        env_count: count.map_or(0, |count| count.env_count),
                                        key_count: count.map_or(0, |count| count.key_count),
                                    }
                                })
                                .collect()
                        })
                })();
                EnvProjectRowsOutcome {
                    generation: job.generation,
                    rows,
                }
            }
        },
        move || ctx.request_repaint(),
    )
}

fn load_approval_snapshot(db: &Db) -> anyhow::Result<ApprovalSnapshot> {
    let page = db.list_pending_approvals_bounded(mcp_store::PENDING_APPROVAL_LIST_LIMIT_MAX)?;
    anyhow::ensure!(!page.has_more, "approval_snapshot_item_limit");
    let inventory = if page.rows.is_empty() {
        Vec::new()
    } else {
        db.mcp_server_inventory(mcp_store::MCP_SERVER_INVENTORY_LIMIT_MAX)?
    };
    let remote_urls: std::collections::HashMap<String, Arc<str>> = inventory
        .into_iter()
        .filter(|server| server.kind == "http")
        .filter(|server| {
            page.rows
                .iter()
                .any(|approval| approval.server_id == server.id)
        })
        .filter_map(|server| Some((server.id, Arc::from(server.url?))))
        .collect();
    let rows = page
        .rows
        .into_iter()
        .map(|row| {
            let remote_url = remote_urls.get(&row.server_id).cloned();
            ui::approvals::PendingApprovalItem::try_new(
                row.id,
                row.server_id,
                row.tool_name,
                row.arguments_preview,
                row.pane_id,
                remote_url,
            )
            .map_err(|_| anyhow::anyhow!("approval_snapshot_invalid_projection"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(ApprovalSnapshot { rows })
}

#[cfg(unix)]
fn signal_approval_listener(
    socket: &std::os::unix::net::UnixDatagram,
) -> Result<(), ApprovalWakeErrorCode> {
    let sent = socket
        .send(&[APPROVAL_CONTROL_MARKER])
        .map_err(|_| ApprovalWakeErrorCode::Delivery)?;
    (sent == 1)
        .then_some(())
        .ok_or(ApprovalWakeErrorCode::Delivery)
}

#[cfg(not(unix))]
fn signal_approval_listener(_ready: &ApprovalListenerReady) -> Result<(), ApprovalWakeErrorCode> {
    Err(ApprovalWakeErrorCode::Unsupported)
}

#[cfg(unix)]
fn approval_listener_main(listener: ApprovalListenerContext) {
    use std::sync::atomic::Ordering;

    let ApprovalListenerContext {
        db_path,
        socket_path,
        commands,
        ready,
        results,
        snapshot,
        ctx,
        stop_requested,
        pending_owner,
    } = listener;

    if stop_requested.load(Ordering::Acquire) {
        let _ = ready.send(Err(ApprovalWakeErrorCode::Start));
        return;
    }
    let db = match Db::open(&db_path) {
        Ok(db) => db,
        Err(_) => {
            let _ = ready.send(Err(ApprovalWakeErrorCode::Storage));
            return;
        }
    };
    let socket = match std::os::unix::net::UnixDatagram::bind(&socket_path) {
        Ok(socket) => socket,
        Err(_) => {
            let _ = ready.send(Err(ApprovalWakeErrorCode::Bind));
            return;
        }
    };
    if stop_requested.load(Ordering::Acquire) {
        drop(socket);
        let _ = std::fs::remove_file(&socket_path);
        let _ = ready.send(Err(ApprovalWakeErrorCode::Start));
        return;
    }
    let control_socket = match std::os::unix::net::UnixDatagram::unbound().and_then(|control| {
        control.connect(&socket_path)?;
        Ok(control)
    }) {
        Ok(control) => control,
        Err(_) => {
            drop(socket);
            let _ = std::fs::remove_file(&socket_path);
            let _ = ready.send(Err(ApprovalWakeErrorCode::Bind));
            return;
        }
    };
    if ready
        .send(Ok(ApprovalListenerReady {
            socket_path: socket_path.clone(),
            control_socket,
        }))
        .is_err()
    {
        let _ = std::fs::remove_file(&socket_path);
        return;
    }
    ctx.request_repaint();

    let publish = |db: &Db| -> bool {
        let next = match load_approval_snapshot(db) {
            Ok(next) => next,
            Err(_) => return false,
        };
        let mut guard = snapshot.lock().unwrap_or_else(|poison| poison.into_inner());
        *guard = Some(next);
        drop(guard);
        ctx.request_repaint();
        true
    };
    let _ = publish(&db);

    let mut byte = [0u8; 1];
    'listen: loop {
        let received = match socket.recv(&mut byte) {
            Ok(1) => byte[0],
            Ok(_) => continue,
            Err(_) => break,
        };
        if stop_requested.load(Ordering::Acquire) {
            break;
        }
        let mut proxy_wake = received == APPROVAL_WAKE_MARKER;
        let mut control_wake = received == APPROVAL_CONTROL_MARKER;
        if socket.set_nonblocking(true).is_ok() {
            loop {
                match socket.recv(&mut byte) {
                    Ok(1) => {
                        proxy_wake |= byte[0] == APPROVAL_WAKE_MARKER;
                        control_wake |= byte[0] == APPROVAL_CONTROL_MARKER;
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break 'listen,
                }
            }
            let _ = socket.set_nonblocking(false);
        }

        let mut refresh = proxy_wake;
        if control_wake {
            loop {
                if stop_requested.load(Ordering::Acquire) {
                    break 'listen;
                }
                match commands.try_recv() {
                    Ok(ApprovalWorkerCommand::Resolve {
                        id,
                        allowed,
                        remember,
                        resolved_at,
                    }) => {
                        let succeeded = db
                            .resolve_approval(&id, allowed, remember, resolved_at)
                            .is_ok();
                        let _ = results.send(if succeeded {
                            ApprovalWorkerResult::Resolved
                        } else {
                            ApprovalWorkerResult::Failed
                        });
                        refresh = true;
                    }
                    Ok(ApprovalWorkerCommand::DenySession {
                        workspace_id,
                        session,
                        resolved_at,
                    }) => {
                        let key = format!("{workspace_id}:{}", session.0);
                        let succeeded = db
                            .deny_pending_approvals_for_session(&key, resolved_at)
                            .is_ok();
                        let _ = results.send(ApprovalWorkerResult::SessionDenied {
                            workspace_id,
                            session,
                            succeeded,
                        });
                        refresh = true;
                    }
                    Ok(ApprovalWorkerCommand::DenyAllOwned { resolved_at }) => {
                        let succeeded = db
                            .deny_session_scoped_pending_approvals_owned(
                                pending_owner.as_ref(),
                                resolved_at,
                            )
                            .is_ok();
                        let _ = results.send(ApprovalWorkerResult::AllDenied { succeeded });
                        refresh = true;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break 'listen,
                }
            }
        }
        if refresh {
            let _ = publish(&db);
        }
    }
    drop(socket);
    let _ = std::fs::remove_file(&socket_path);
}

impl ApprovalWakeHub {
    fn new(
        db_path: PathBuf,
        ctx: egui::Context,
        pending_owner: Arc<storage::ActivePendingApprovalOwner>,
    ) -> Self {
        Self {
            db_path,
            ctx,
            snapshot: Arc::new(std::sync::Mutex::new(None)),
            slot: None,
            deferred: std::collections::VecDeque::new(),
            inflight: 0,
            pending_owner,
        }
    }

    fn ensure_started(&mut self) -> Result<(), ApprovalWakeErrorCode> {
        if self.slot.is_some() {
            return Ok(());
        }
        #[cfg(not(unix))]
        return Err(ApprovalWakeErrorCode::Unsupported);
        #[cfg(unix)]
        {
            let socket_name = format!(
                "deppy-appr-{}-{}.sock",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            );
            let mut socket_path = std::env::temp_dir().join(&socket_name);
            if socket_path.to_string_lossy().len() > 100 {
                // macOS TMPDIR is commonly a long /var/folders path and sockaddr_un is only
                // 104 bytes. The literal /tmp alias keeps the kernel-visible address bounded.
                socket_path = PathBuf::from("/tmp").join(socket_name);
            }
            let bytes = socket_path
                .to_str()
                .ok_or(ApprovalWakeErrorCode::Start)?
                .len();
            if bytes == 0 || bytes > 100 || socket_path.exists() {
                return Err(ApprovalWakeErrorCode::Start);
            }
            let (command_tx, commands) =
                std::sync::mpsc::sync_channel::<ApprovalWorkerCommand>(APPROVAL_COMMAND_CAP);
            let (ready, ready_rx) = std::sync::mpsc::sync_channel(1);
            let (results, result_rx) =
                std::sync::mpsc::sync_channel::<ApprovalWorkerResult>(APPROVAL_COMMAND_CAP);
            let db_path = self.db_path.clone();
            let snapshot = Arc::clone(&self.snapshot);
            let ctx = self.ctx.clone();
            let thread_path = socket_path.clone();
            let stop_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let thread_stop_requested = Arc::clone(&stop_requested);
            let pending_owner = Arc::clone(&self.pending_owner);
            let handle = std::thread::Builder::new()
                .name("approval-wake".to_owned())
                .spawn(move || {
                    approval_listener_main(ApprovalListenerContext {
                        db_path,
                        socket_path: thread_path,
                        commands,
                        ready,
                        results,
                        snapshot,
                        ctx,
                        stop_requested: thread_stop_requested,
                        pending_owner,
                    });
                })
                .map_err(|_| ApprovalWakeErrorCode::Start)?;
            self.slot = Some(ApprovalListenerSlot {
                command_tx,
                ready_rx,
                result_rx,
                handle,
                socket_path: None,
                control_socket: None,
                stop_requested,
            });
            Ok(())
        }
    }

    fn poll_ready(&mut self) -> Result<Option<PathBuf>, ApprovalWakeErrorCode> {
        let Some(slot) = self.slot.as_mut() else {
            return Ok(None);
        };
        if slot.socket_path.is_none() {
            match slot.ready_rx.try_recv() {
                Ok(Ok(ready)) => {
                    slot.socket_path = Some(ready.socket_path.clone());
                    #[cfg(unix)]
                    {
                        slot.control_socket = Some(ready.control_socket);
                    }
                }
                Ok(Err(error)) => return Err(error),
                Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(None),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err(ApprovalWakeErrorCode::Start);
                }
            }
        }
        let path = slot
            .socket_path
            .clone()
            .ok_or(ApprovalWakeErrorCode::Start)?;
        let mut sent = false;
        while let Some(command) = self.deferred.pop_front() {
            match slot.command_tx.try_send(command) {
                Ok(()) => sent = true,
                Err(std::sync::mpsc::TrySendError::Full(command)) => {
                    self.deferred.push_front(command);
                    break;
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(command)) => {
                    self.deferred.push_front(command);
                    return Err(ApprovalWakeErrorCode::Start);
                }
            }
        }
        if sent {
            #[cfg(unix)]
            signal_approval_listener(
                slot.control_socket
                    .as_ref()
                    .ok_or(ApprovalWakeErrorCode::Start)?,
            )?;
            #[cfg(not(unix))]
            return Err(ApprovalWakeErrorCode::Unsupported);
        }
        Ok(Some(path))
    }

    fn enqueue(&mut self, command: ApprovalWorkerCommand) -> Result<(), ApprovalWakeErrorCode> {
        self.ensure_started()?;
        let expects_result = matches!(
            &command,
            ApprovalWorkerCommand::Resolve { .. }
                | ApprovalWorkerCommand::DenySession { .. }
                | ApprovalWorkerCommand::DenyAllOwned { .. }
        );
        if expects_result && self.inflight >= APPROVAL_COMMAND_CAP {
            return Err(ApprovalWakeErrorCode::Backpressure);
        }
        let Some(slot) = self.slot.as_ref() else {
            return Err(ApprovalWakeErrorCode::Start);
        };
        if let Some(path) = slot.socket_path.as_deref() {
            slot.command_tx
                .try_send(command)
                .map_err(|_| ApprovalWakeErrorCode::Backpressure)?;
            if expects_result {
                self.inflight += 1;
            }
            let delivered = {
                #[cfg(unix)]
                {
                    let _ = path;
                    slot.control_socket
                        .as_ref()
                        .ok_or(ApprovalWakeErrorCode::Start)
                        .and_then(signal_approval_listener)
                }
                #[cfg(not(unix))]
                {
                    let _ = path;
                    Err(ApprovalWakeErrorCode::Unsupported)
                }
            };
            if delivered.is_err() {
                // Ownership has already crossed the command channel. Stop the dead listener so
                // the queued idempotent approval mutation is either completed once or dropped
                // before the caller may retry it through a fresh listener.
                self.stop();
                return Err(ApprovalWakeErrorCode::Delivery);
            }
        } else if self.deferred.len() < APPROVAL_COMMAND_CAP {
            self.deferred.push_back(command);
            if expects_result {
                self.inflight += 1;
            }
        } else {
            return Err(ApprovalWakeErrorCode::Backpressure);
        }
        Ok(())
    }

    fn take_snapshot(&self) -> Option<ApprovalSnapshot> {
        self.snapshot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
    }

    fn drain_results(&mut self) -> Vec<ApprovalWorkerResult> {
        let Some(slot) = self.slot.as_ref() else {
            return Vec::new();
        };
        let results = slot.result_rx.try_iter().collect::<Vec<_>>();
        self.inflight = self.inflight.saturating_sub(results.len());
        results
    }

    fn is_idle(&self) -> bool {
        self.inflight == 0 && self.deferred.is_empty()
    }

    fn stop(&mut self) {
        let Some(mut slot) = self.slot.take() else {
            self.deferred.clear();
            self.inflight = 0;
            return;
        };
        // Publish stop before opening result capacity. The listener checks this flag before every
        // queued command, so after the drain it can emit at most its one currently executing
        // result and cannot refill the bounded queue behind the join.
        slot.stop_requested
            .store(true, std::sync::atomic::Ordering::Release);
        for _ in slot.result_rx.try_iter() {}
        if slot.socket_path.is_none()
            && let Ok(Ok(ready)) = slot
                .ready_rx
                .recv_timeout(std::time::Duration::from_secs(2))
        {
            slot.socket_path = Some(ready.socket_path);
            #[cfg(unix)]
            {
                slot.control_socket = Some(ready.control_socket);
            }
        }
        #[cfg(unix)]
        if let Some(control) = slot.control_socket.as_ref() {
            let _ = signal_approval_listener(control);
        }
        drop(slot.command_tx);
        let _ = slot.handle.join();
        self.deferred.clear();
        self.inflight = 0;
    }
}

impl Drop for ApprovalWakeHub {
    fn drop(&mut self) {
        self.stop();
    }
}

impl ApprovalLaunchTracker {
    fn reserve(
        &mut self,
        workspace_id: String,
        agent_config_id: String,
        now: std::time::Instant,
    ) -> Result<u64, ApprovalWakeErrorCode> {
        if self.pending.len().saturating_add(self.live.len()) >= APPROVAL_LAUNCH_CAP {
            return Err(ApprovalWakeErrorCode::Backpressure);
        }
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let id = self.next_id;
        self.pending.push_back(ApprovalLaunchTicket {
            id,
            workspace_id,
            agent_config_id,
            state: ApprovalLaunchTicketState::Preparing {
                deadline: now + APPROVAL_SPAWN_DEADLINE,
            },
        });
        Ok(id)
    }

    fn cancel(&mut self, ticket_id: u64) {
        self.pending.retain(|ticket| ticket.id != ticket_id);
    }

    fn mark_spawn_sent(&mut self, ticket_id: u64, now: std::time::Instant) -> bool {
        let Some(ticket) = self
            .pending
            .iter_mut()
            .find(|ticket| ticket.id == ticket_id)
        else {
            return false;
        };
        match ticket.state {
            ApprovalLaunchTicketState::Preparing { deadline } if now < deadline => {
                ticket.state = ApprovalLaunchTicketState::SpawnSent;
                true
            }
            ApprovalLaunchTicketState::Preparing { .. } | ApprovalLaunchTicketState::SpawnSent => {
                false
            }
        }
    }

    fn correlate(
        &mut self,
        workspace_id: &str,
        agent_config_id: &str,
        session: Option<runtime::SessionId>,
    ) -> bool {
        let Some(position) = self.pending.iter().position(|ticket| {
            ticket.workspace_id == workspace_id
                && ticket.agent_config_id == agent_config_id
                && ticket.state == ApprovalLaunchTicketState::SpawnSent
        }) else {
            return false;
        };
        let ticket = self
            .pending
            .remove(position)
            .expect("approval ticket position checked");
        if let Some(session) = session {
            self.live
                .insert((workspace_id.to_owned(), session), ticket.id);
        }
        true
    }

    fn expire(&mut self, now: std::time::Instant) -> Vec<u64> {
        let mut expired = Vec::new();
        self.pending.retain(|ticket| {
            if matches!(
                ticket.state,
                ApprovalLaunchTicketState::Preparing { deadline } if deadline <= now
            ) {
                expired.push(ticket.id);
                false
            } else {
                true
            }
        });
        expired
    }

    fn observe_session_exit(&mut self, workspace_id: &str, session: runtime::SessionId) -> bool {
        let key = (workspace_id.to_owned(), session);
        if self.live.contains_key(&key) {
            self.exited.insert(key)
        } else {
            false
        }
    }

    fn pending_denials(&self, now: std::time::Instant) -> Vec<(String, runtime::SessionId)> {
        self.exited
            .iter()
            .filter(|key| {
                !self.denying.contains(*key)
                    && self
                        .denial_retries
                        .get(*key)
                        .is_none_or(|retry| retry.retry_at <= now)
            })
            .cloned()
            .collect()
    }

    fn mark_deny_queued(&mut self, workspace_id: &str, session: runtime::SessionId) {
        self.denying.insert((workspace_id.to_owned(), session));
    }

    fn finish_session(
        &mut self,
        workspace_id: &str,
        session: runtime::SessionId,
        succeeded: bool,
        now: std::time::Instant,
    ) -> Option<std::time::Duration> {
        let key = (workspace_id.to_owned(), session);
        self.denying.remove(&key);
        if succeeded {
            self.denial_retries.remove(&key);
            self.exited.remove(&key);
            self.live.remove(&key);
            None
        } else if self.live.contains_key(&key) && self.exited.contains(&key) {
            let retry = self
                .denial_retries
                .entry(key)
                .or_insert(ApprovalDenialRetry {
                    failures: 0,
                    retry_at: now,
                });
            retry.failures = retry.failures.saturating_add(1);
            let shift = u32::from(retry.failures.saturating_sub(1).min(6));
            let delay =
                std::time::Duration::from_secs(1u64 << shift).min(APPROVAL_DENIAL_RETRY_MAX);
            retry.retry_at = now + delay;
            Some(delay)
        } else {
            self.denial_retries.remove(&key);
            None
        }
    }

    fn close_workspace(&mut self, workspace_id: &str) -> usize {
        let mut canceled = 0usize;
        self.pending.retain(|ticket| {
            if ticket.workspace_id == workspace_id {
                canceled = canceled.saturating_add(1);
                false
            } else {
                true
            }
        });
        let live = self
            .live
            .keys()
            .filter(|(candidate, _)| candidate == workspace_id)
            .cloned()
            .collect::<Vec<_>>();
        for key in live {
            self.exited.insert(key);
        }
        canceled
    }

    fn clear_fail_closed(&mut self) {
        self.pending.clear();
        self.live.clear();
        self.exited.clear();
        self.denying.clear();
        self.denial_retries.clear();
    }

    fn is_empty(&self) -> bool {
        self.pending.is_empty() && self.live.is_empty()
    }
}

/// Agents(APP) custom LLM 프로바이더 API 키의 고정 keyring entry id (PR-L4).
/// config에는 키도 존재 플래그도 저장하지 않는다 — keyring 존재 여부가 단일 진실.
const CODEX_LLM_API_KEY_ENTRY_ID: &str = "codex-llm-custom-api-key";

/// Composition-root host for authenticated Codex App Server construction. The leaf owns neither
/// a concrete keyring type nor the secret-bearing process options.
struct AppCodexAppServerHost {
    secret_store: KeyringSecretStore,
}

impl ui::agent_sessions::CodexAppServerHost for AppCodexAppServerHost {
    fn spawn(
        &self,
        llm_override: Option<crate::codex_app_server::CodexLlmOverride>,
        ctx: egui::Context,
    ) -> anyhow::Result<crate::codex_app_server::CodexAppServerClient> {
        let llm_api_key =
            if matches!(
                llm_override,
                Some(crate::codex_app_server::CodexLlmOverride::Custom { .. })
            ) && secret::SecretStore::has_secret(&self.secret_store, CODEX_LLM_API_KEY_ENTRY_ID)?
            {
                Some(secret::SecretStore::get_secret(
                    &self.secret_store,
                    CODEX_LLM_API_KEY_ENTRY_ID,
                )?)
            } else {
                None
            };
        crate::codex_app_server::CodexAppServerClient::spawn(
            crate::codex_app_server::CodexAppServerOptions {
                llm_override,
                llm_api_key,
                ..crate::codex_app_server::CodexAppServerOptions::default()
            },
            ctx,
        )
    }
}

/// Production runtime port: logical metadata IDs are resolved through the current SQLite pointer
/// to one validated physical keyring slot. There is deliberately no logical-ID fallback.
struct AppRuntimeSecretResolver {
    db_path: PathBuf,
    db: std::sync::Mutex<Option<Db>>,
    secret_store: KeyringSecretStore,
}

impl AppRuntimeSecretResolver {
    fn new(db_path: PathBuf) -> Self {
        Self {
            db_path,
            db: std::sync::Mutex::new(None),
            secret_store: KeyringSecretStore,
        }
    }
}

impl runtime::RuntimeSecretResolver for AppRuntimeSecretResolver {
    fn resolve(&self, logical_credential_id: &str) -> anyhow::Result<runtime::RuntimeSecret> {
        let logical = secret::LogicalCredentialId::new(logical_credential_id.to_owned())
            .map_err(|_| anyhow::anyhow!("runtime_secret_logical_id_invalid"))?;
        let mut db = self
            .db
            .lock()
            .map_err(|_| anyhow::anyhow!("runtime_secret_repository_lock_failed"))?;
        if db.is_none() {
            *db = Some(
                Db::open(&self.db_path)
                    .map_err(|_| anyhow::anyhow!("runtime_secret_repository_open_failed"))?,
            );
        }
        let location = db
            .as_ref()
            .expect("runtime secret repository initialized")
            .credential_secret_location(logical.as_str())
            .map_err(|_| anyhow::anyhow!("runtime_secret_pointer_read_failed"))?
            .ok_or_else(|| anyhow::anyhow!("runtime_secret_pointer_missing"))?;
        anyhow::ensure!(
            location.keyring_service == secret::KEYRING_SERVICE,
            "runtime_secret_service_invalid"
        );
        let slot = secret::PhysicalSecretSlot::parse(location.keyring_username)
            .map_err(|_| anyhow::anyhow!("runtime_secret_slot_invalid"))?;
        anyhow::ensure!(
            slot.belongs_to(&logical),
            "runtime_secret_slot_owner_invalid"
        );
        let value = secret::SecretStore::get_secret(&self.secret_store, slot.as_str())
            .map_err(|_| anyhow::anyhow!("runtime_secret_read_failed"))?;
        Ok(runtime::RuntimeSecret::new(value.into_string()))
    }
}

#[derive(Default)]
struct EnvApiProjectEditState {
    name_workspace_id: Option<String>,
    name_buffer: String,
}

/// 환경/API 상세 상단 헤더 — 참조 화면의 68px 고정 헤더와 14px 좌우 inset.
/// 이름/경로는 클릭해 인라인 편집하며, 화면에 없는 폴더 관리 동작은 경로 우클릭 메뉴에
/// 보존한다. 따라서 표준 상태의 픽셀 배치는 목업과 같고 기존 기능도 잃지 않는다.
fn render_env_api_project_header(
    ui: &mut egui::Ui,
    project: Option<&ui::env_project_list::EnvProjectRow>,
    env_action: &mut Option<ui::env_profiles::EnvAction>,
    workspace_rename: &mut Option<String>,
    edit: &mut EnvApiProjectEditState,
    catalog: &i18n::Catalog,
) {
    let project_id = project.map(|project| project.id.as_str()).unwrap_or("");
    let name = project
        .map(|project| project.name.as_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("~");
    // 이름 편집 초기값은 표시명("폴더명 (별칭)")이 아니라 별칭 원본이다 (E3).
    let alias = project.map(|project| project.alias.trim()).unwrap_or("");
    let path = project
        .map(|project| project.path.as_str())
        .filter(|path| !path.trim().is_empty())
        .unwrap_or("");
    // 프로젝트 목록을 만들 때 계산한 값을 재사용한다. Path::is_dir()는 네트워크/외장
    // 볼륨에서 블록될 수 있으므로 설정 UI의 매 프레임 렌더 경로에서 다시 호출하지 않는다.
    let path_missing = project.is_some_and(|project| project.path_missing);
    let path_text = if path.is_empty() {
        catalog.t("workspace.manager.path_unset", &[])
    } else {
        ui::env_project_list::display_project_path(path)
    };

    const HEADER_H: f32 = 68.0;
    const PAD_X: f32 = 14.0;
    const LABEL_W: f32 = 34.0;
    const LABEL_GAP: f32 = 6.0;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), HEADER_H),
        egui::Sense::hover(),
    );
    let painter = ui.painter().clone();
    let value_x = rect.left() + PAD_X + LABEL_W + LABEL_GAP;
    let name_y = rect.top() + 21.0;
    let path_y = rect.top() + 49.0;
    let value_right = rect.right() - PAD_X;

    painter.text(
        egui::pos2(rect.left() + PAD_X, name_y),
        egui::Align2::LEFT_CENTER,
        catalog.t("common.name", &[]),
        egui::FontId::monospace(13.0),
        ui.visuals().weak_text_color(),
    );
    painter.text(
        egui::pos2(rect.left() + PAD_X, path_y),
        egui::Align2::LEFT_CENTER,
        catalog.t("workspace.manager.path", &[]),
        egui::FontId::monospace(13.0),
        ui.visuals().weak_text_color(),
    );

    let name_rect = egui::Rect::from_min_max(
        egui::pos2(value_x, rect.top() + 8.0),
        egui::pos2(value_right, rect.top() + 34.0),
    );
    if edit.name_workspace_id.as_deref() == Some(project_id) {
        let response = ui.put(
            name_rect,
            egui::TextEdit::singleline(&mut edit.name_buffer)
                .font(egui::TextStyle::Monospace)
                .id_source(("env_api_project_name", project_id)),
        );
        let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
        let commit = response.lost_focus() || ui.input(|i| i.key_pressed(egui::Key::Enter));
        if escape && response.has_focus() {
            edit.name_workspace_id = None;
            edit.name_buffer.clear();
        } else if commit {
            // E3: 편집 대상은 표시명이 아니라 **별칭**이다. 빈 값 = 별칭 해제(폴더명만 표시).
            let next = edit.name_buffer.trim();
            if next != alias {
                *workspace_rename = Some(next.to_owned());
            }
            edit.name_workspace_id = None;
            edit.name_buffer.clear();
        }
    } else {
        painter.with_clip_rect(name_rect).text(
            egui::pos2(value_x, name_y),
            egui::Align2::LEFT_CENTER,
            name,
            egui::FontId::monospace(15.0),
            ui.visuals().text_color(),
        );
        let response = ui
            .interact(
                name_rect,
                ui.id().with(("env_api_project_name_label", project_id)),
                egui::Sense::click(),
            )
            .on_hover_text(catalog.t("workspace.alias_hint", &[]));
        if response.clicked() && !project_id.is_empty() {
            edit.name_workspace_id = Some(project_id.to_owned());
            edit.name_buffer = alias.to_owned();
        }
    }

    let path_rect = egui::Rect::from_min_max(
        egui::pos2(value_x, rect.top() + 36.0),
        egui::pos2(value_right, rect.top() + 62.0),
    );
    {
        let path_color = if path_missing {
            ui.visuals().error_fg_color
        } else {
            ui.visuals().weak_text_color()
        };
        painter.with_clip_rect(path_rect).text(
            egui::pos2(value_x, path_y),
            egui::Align2::LEFT_CENTER,
            &path_text,
            egui::FontId::monospace(14.0),
            path_color,
        );
        let hover_text = if path_missing {
            format!(
                "{}\n{}",
                path_text,
                catalog.t("env.project_path_missing", &[])
            )
        } else {
            path_text.clone()
        };
        let response = ui
            .interact(
                path_rect,
                ui.id().with(("env_api_project_path_label", project_id)),
                egui::Sense::click(),
            )
            .on_hover_text(hover_text);
        // E3 ④: 경로는 타이핑이 아니라 Finder로만 지정한다 — 오타/존재하지 않는 경로로
        // 워크스페이스가 유령이 되는 입력 경로 제거. 클릭 = 폴더 선택 다이얼로그.
        if response.clicked() && !project_id.is_empty() {
            *env_action = Some(ui::env_profiles::EnvAction::ChooseProjectFolder);
        }
        response.context_menu(|ui| {
            if ui
                .button(catalog.t("env.project_folder.choose", &[]))
                .clicked()
            {
                *env_action = Some(ui::env_profiles::EnvAction::ChooseProjectFolder);
                ui.close();
            }
            if !path.is_empty()
                && ui
                    .button(catalog.t("env.project_folder.clear", &[]))
                    .clicked()
            {
                *env_action = Some(ui::env_profiles::EnvAction::SetProjectPath(
                    std::path::PathBuf::new(),
                ));
                ui.close();
            }
            if !path.is_empty() && ui.button(catalog.t("env.resync_hint", &[])).clicked() {
                *env_action = Some(ui::env_profiles::EnvAction::Resync);
                ui.close();
            }
        });
    }

    let y = painter.round_to_pixel_center(rect.bottom());
    painter.hline(
        rect.x_range(),
        y,
        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
    );
}

/// UUID v4 형태(8-4-4-4-12 hex)인가 — credential id 규약. `.refresh`/`.dcr`
/// 접미(OAuth refresh token / DCR client_secret entry — H4 규약)는 벗겨 판정.
fn uuid_base(account: &str) -> Option<&str> {
    let base = account
        .strip_suffix(".refresh")
        .or_else(|| account.strip_suffix(".dcr"))
        .unwrap_or(account);
    let bytes = base.as_bytes();
    if bytes.len() != 36 {
        return None;
    }
    for (i, b) in bytes.iter().enumerate() {
        let ok = match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        };
        if !ok {
            return None;
        }
    }
    Some(base)
}

const CONNECTOR_IDLE_TTL: std::time::Duration = std::time::Duration::from_secs(30);
const CONNECTOR_OAUTH_HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const CONNECTOR_OAUTH_CALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
const SLACK_MCP_URL: &str = "https://mcp.slack.com/mcp";

fn connector_service_error(
    code: connector_contract::ErrorCode,
    message: &'static str,
) -> connector_service::ServiceError {
    connector_service::ServiceError::new(code, message)
}

fn connector_storage_revision(
    revision: connector_contract::Revision,
) -> Result<storage::ConnectorConfigRevision, connector_service::ServiceError> {
    storage::ConnectorConfigRevision::try_from_u64(revision.0).map_err(|_| {
        connector_service_error(
            connector_contract::ErrorCode::StorageUnavailable,
            "connector revision is invalid",
        )
    })
}

fn connector_revision(revision: storage::ConnectorConfigRevision) -> connector_contract::Revision {
    connector_contract::Revision(revision.get())
}

fn connector_repository_cas<T, U>(
    value: storage::ConnectorConfigCas<T>,
    map: impl FnOnce(T) -> U,
) -> connector_service::RepositoryCas<U> {
    match value {
        storage::ConnectorConfigCas::Committed { revision, value } => {
            connector_service::RepositoryCas::Committed {
                revision: connector_revision(revision),
                value: map(value),
            }
        }
        storage::ConnectorConfigCas::Stale { current_revision } => {
            connector_service::RepositoryCas::Stale {
                current_revision: connector_revision(current_revision),
            }
        }
    }
}

fn connector_transport_kind(
    kind: &str,
) -> Result<connector_contract::TransportKind, connector_service::ServiceError> {
    match kind {
        "stdio" => Ok(connector_contract::TransportKind::Stdio),
        "http" => Ok(connector_contract::TransportKind::Http),
        _ => Err(connector_service_error(
            connector_contract::ErrorCode::StorageUnavailable,
            "stored connector transport is invalid",
        )),
    }
}

fn connector_server_draft(
    row: mcp_store::McpServerRow,
) -> Result<connector_contract::ServerDraft, connector_service::ServiceError> {
    let id = connector_contract::ServerId::new(row.id);
    let transport = match row.kind.as_str() {
        "stdio" => connector_contract::TransportDraft::Stdio {
            command: row.command.ok_or_else(|| {
                connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "stored stdio connector command is missing",
                )
            })?,
            args: row.args,
            plain_env: row.env_plain,
            secret_env: row
                .env_secrets
                .into_iter()
                .map(|(key, credential)| (key, connector_contract::CredentialId::new(credential)))
                .collect(),
            inherit_env: row.inherit_env,
        },
        "http" => connector_contract::TransportDraft::Http {
            url: row.url.ok_or_else(|| {
                connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "stored HTTP connector URL is missing",
                )
            })?,
        },
        _ => {
            return Err(connector_service_error(
                connector_contract::ErrorCode::StorageUnavailable,
                "stored connector transport is invalid",
            ));
        }
    };
    Ok(connector_contract::ServerDraft {
        id: Some(id),
        name: row.name,
        transport,
        enabled: row.enabled,
    })
}

fn connector_server_row(
    draft: connector_contract::ServerDraft,
) -> Result<mcp_store::McpServerRow, connector_service::ServiceError> {
    let id = draft
        .id
        .map(String::from)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let (kind, command, args, env_plain, env_secrets, inherit_env, url) = match draft.transport {
        connector_contract::TransportDraft::Stdio {
            command,
            args,
            plain_env,
            secret_env,
            inherit_env,
        } => (
            "stdio".to_owned(),
            Some(command),
            args,
            plain_env,
            secret_env
                .into_iter()
                .map(|(key, credential)| (key, String::from(credential)))
                .collect(),
            inherit_env,
            None,
        ),
        connector_contract::TransportDraft::Http { url } => (
            "http".to_owned(),
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            Some(url),
        ),
    };
    let row = mcp_store::McpServerRow {
        id,
        name: draft.name,
        kind,
        command,
        args,
        env_plain,
        env_secrets,
        inherit_env,
        url,
        enabled: draft.enabled,
    };
    mcp_store::validate_server_env_for_persistence(&row.env_plain, &row.env_secrets).map_err(
        |_| {
            connector_service_error(
                connector_contract::ErrorCode::InvalidInput,
                "connector environment is invalid",
            )
        },
    )?;
    Ok(row)
}

fn connector_permission(
    rule: Option<&mcp_store::PermissionRuleRow>,
) -> Result<connector_contract::PermissionRule, connector_service::ServiceError> {
    match rule.map(|rule| rule.rule.as_str()) {
        None | Some("ask") => Ok(connector_contract::PermissionRule::Ask),
        Some("allow") => Ok(connector_contract::PermissionRule::Allow),
        Some("deny") => Ok(connector_contract::PermissionRule::Deny),
        Some(_) => Err(connector_service_error(
            connector_contract::ErrorCode::StorageUnavailable,
            "stored connector permission is invalid",
        )),
    }
}

fn connector_slack_projection(
    rows: &[mcp_store::McpServerInventoryRow],
) -> connector_contract::SlackProjection {
    let mut matching = rows.iter().filter(|row| {
        row.kind == "http"
            && row
                .url
                .as_deref()
                .is_some_and(|url| url.trim().trim_end_matches('/') == SLACK_MCP_URL)
    });
    let Some(row) = matching.next() else {
        return connector_contract::SlackProjection::default();
    };
    if matching.next().is_some() {
        return connector_contract::SlackProjection {
            server_id: None,
            status: connector_contract::SlackStatus::Failed,
            tool_count: 0,
            workspace_label: None,
            can_choose_workspace: false,
            recovery: None,
        };
    }
    connector_contract::SlackProjection {
        server_id: Some(connector_contract::ServerId::new(row.id.clone())),
        status: if !row.enabled {
            connector_contract::SlackStatus::NotConfigured
        } else if row.tool_count == 0 {
            connector_contract::SlackStatus::Ready
        } else {
            connector_contract::SlackStatus::Connected
        },
        tool_count: row.tool_count,
        workspace_label: None,
        can_choose_workspace: row.enabled,
        recovery: None,
    }
}

struct AppConnectorRepositoryFactory {
    db_path: PathBuf,
    redaction: secret::RedactionService,
}

impl connector_service::ConnectorRepositoryFactory for AppConnectorRepositoryFactory {
    fn open(
        &self,
    ) -> Result<Box<dyn connector_service::ConnectorRepository>, connector_service::ServiceError>
    {
        let db = Db::open(&self.db_path).map_err(|_| {
            connector_service_error(
                connector_contract::ErrorCode::StorageUnavailable,
                "connector repository open failed",
            )
        })?;
        let authorization_owner = db.acquire_authorization_owner("gui").map_err(|_| {
            connector_service_error(
                connector_contract::ErrorCode::AuditUnavailable,
                "connector authorization owner is unavailable",
            )
        })?;
        Ok(Box::new(AppConnectorRepository {
            db,
            redaction: self.redaction.clone(),
            authorization_owner: Some(authorization_owner),
        }))
    }
}

struct AppConnectorRepository {
    db: Db,
    redaction: secret::RedactionService,
    authorization_owner: Option<storage::ActiveAuthorizationOwner>,
}

impl AppConnectorRepository {
    fn overview_from(
        read: storage::ConnectorConfigRead<Vec<mcp_store::McpServerInventoryRow>>,
    ) -> Result<connector_service::OverviewData, connector_service::ServiceError> {
        let slack = connector_slack_projection(&read.value);
        let servers = read
            .value
            .into_iter()
            .map(|row| {
                Ok(connector_contract::ServerSummary {
                    id: connector_contract::ServerId::new(row.id),
                    name: row.name,
                    transport: connector_transport_kind(&row.kind)?,
                    enabled: row.enabled,
                    connection: if !row.enabled {
                        connector_contract::ConnectionState::Disabled
                    } else if row.tool_count == 0 {
                        connector_contract::ConnectionState::Idle
                    } else {
                        connector_contract::ConnectionState::Connected
                    },
                    tool_count: row.tool_count,
                    error_code: None,
                })
            })
            .collect::<Result<Vec<_>, connector_service::ServiceError>>()?;
        Ok(connector_service::OverviewData {
            config_revision: connector_revision(read.revision),
            slack,
            servers,
        })
    }

    fn oauth_binding(
        record: storage::CredentialOAuthBindingRecord,
        server_id: &connector_contract::ServerId,
        exact_url: &str,
    ) -> Result<connector_service::HttpAuthBinding, connector_service::ServiceError> {
        if record.keyring_service != secret::KEYRING_SERVICE {
            return Err(connector_service_error(
                connector_contract::ErrorCode::SecretUnavailable,
                "connector OAuth keyring service is invalid",
            ));
        }
        let logical =
            secret::LogicalCredentialId::new(record.logical_id.clone()).map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::SecretUnavailable,
                    "connector OAuth credential identifier is invalid",
                )
            })?;
        let physical =
            secret::PhysicalSecretSlot::parse(record.physical_pointer).map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::SecretUnavailable,
                    "connector OAuth physical slot is invalid",
                )
            })?;
        if !physical.belongs_to(&logical) {
            return Err(connector_service_error(
                connector_contract::ErrorCode::SecretUnavailable,
                "connector OAuth physical slot owner is invalid",
            ));
        }
        let metadata = auth::StoredOAuthMetadata::from_json_bounded(
            record.oauth_metadata_json.as_bytes(),
            server_id.as_str(),
            exact_url,
            auth::StoredOAuthMetadataLimits::PRODUCTION,
        )
        .map_err(|_| {
            connector_service_error(
                connector_contract::ErrorCode::StorageUnavailable,
                "connector OAuth metadata is invalid",
            )
        })?;
        Ok(connector_service::HttpAuthBinding {
            credential_id: connector_contract::CredentialId::new(record.logical_id),
            physical_slot: physical,
            oauth_metadata: Some(metadata),
        })
    }
}

impl connector_service::ConnectorRepository for AppConnectorRepository {
    fn load_overview(
        &mut self,
    ) -> Result<connector_service::OverviewData, connector_service::ServiceError> {
        let read = self
            .db
            .mcp_server_inventory_versioned(
                connector_contract::ResourceLimits::PRODUCTION_CEILING.import_servers,
            )
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "connector overview load failed",
                )
            })?;
        Self::overview_from(read)
    }

    fn load_server(
        &mut self,
        server_id: &connector_contract::ServerId,
    ) -> Result<
        connector_service::Observed<connector_contract::ServerDraft>,
        connector_service::ServiceError,
    > {
        let read = self
            .db
            .mcp_server_versioned(server_id.as_str())
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "connector server load failed",
                )
            })?;
        let row = read.value.ok_or_else(|| {
            connector_service_error(
                connector_contract::ErrorCode::StorageUnavailable,
                "connector server is unavailable",
            )
        })?;
        Ok(connector_service::Observed {
            revision: connector_revision(read.revision),
            value: connector_server_draft(row)?,
        })
    }

    fn load_mcp_target(
        &mut self,
        server_id: &connector_contract::ServerId,
    ) -> Result<
        connector_service::Observed<connector_service::RepositoryMcpTarget>,
        connector_service::ServiceError,
    > {
        let read = self
            .db
            .mcp_request_target_versioned(server_id.as_str())
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "connector execution target load failed",
                )
            })?;
        let row = read.value.server.ok_or_else(|| {
            connector_service_error(
                connector_contract::ErrorCode::StorageUnavailable,
                "connector execution target is unavailable",
            )
        })?;
        let exact_url = row.url.clone();
        let server = connector_server_draft(row)?;
        let credential_ids = match &server.transport {
            connector_contract::TransportDraft::Stdio { secret_env, .. } => secret_env
                .iter()
                .map(|(_, credential_id)| credential_id.clone())
                .collect::<Vec<_>>(),
            connector_contract::TransportDraft::Http { .. } => Vec::new(),
        };
        if credential_ids.len() != read.value.credential_locations.len() {
            return Err(connector_service_error(
                connector_contract::ErrorCode::StorageUnavailable,
                "connector credential binding count is invalid",
            ));
        }
        let credential_revisions = credential_ids
            .into_iter()
            .zip(read.value.credential_locations)
            .map(|(credential_id, location)| {
                if location.keyring_service != secret::KEYRING_SERVICE {
                    return Err(connector_service_error(
                        connector_contract::ErrorCode::SecretUnavailable,
                        "connector credential keyring service is invalid",
                    ));
                }
                let logical =
                    secret::LogicalCredentialId::new(credential_id.as_str()).map_err(|_| {
                        connector_service_error(
                            connector_contract::ErrorCode::SecretUnavailable,
                            "connector credential identifier is invalid",
                        )
                    })?;
                let physical = secret::PhysicalSecretSlot::parse(location.keyring_username)
                    .map_err(|_| {
                        connector_service_error(
                            connector_contract::ErrorCode::SecretUnavailable,
                            "connector credential physical slot is invalid",
                        )
                    })?;
                if !physical.belongs_to(&logical) {
                    return Err(connector_service_error(
                        connector_contract::ErrorCode::SecretUnavailable,
                        "connector credential physical slot owner is invalid",
                    ));
                }
                Ok(connector_service::CredentialResolutionRequest {
                    credential_id,
                    expected_physical_slot: Some(physical),
                })
            })
            .collect::<Result<Vec<_>, connector_service::ServiceError>>()?;
        let http_auth = match read.value.oauth_bindings.as_slice() {
            [] => None,
            [binding] => {
                let exact_url = exact_url.as_deref().ok_or_else(|| {
                    connector_service_error(
                        connector_contract::ErrorCode::StorageUnavailable,
                        "connector OAuth binding has no exact URL",
                    )
                })?;
                Some(Self::oauth_binding(binding.clone(), server_id, exact_url)?)
            }
            _ => {
                return Err(connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "connector OAuth binding is ambiguous",
                ));
            }
        };
        Ok(connector_service::Observed {
            revision: connector_revision(read.revision),
            value: connector_service::RepositoryMcpTarget {
                server,
                credential_revisions,
                http_auth,
            },
        })
    }

    fn load_tool_page(
        &mut self,
        server_id: &connector_contract::ServerId,
        offset: usize,
        limit: usize,
    ) -> Result<
        connector_service::Observed<connector_service::RepositoryToolPage>,
        connector_service::ServiceError,
    > {
        let read = self
            .db
            .mcp_tool_page_versioned(server_id.as_str(), offset, limit)
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "connector tool page load failed",
                )
            })?;
        let items = read
            .value
            .rows
            .into_iter()
            .map(|row| {
                Ok(connector_contract::ToolListItem {
                    id: connector_contract::ToolId::new(row.id),
                    name: row.name,
                    description: row.description,
                    permission: connector_permission(row.permission.as_ref())?,
                })
            })
            .collect::<Result<Vec<_>, connector_service::ServiceError>>()?;
        Ok(connector_service::Observed {
            revision: connector_revision(read.revision),
            value: connector_service::RepositoryToolPage {
                total: read.value.total,
                items,
            },
        })
    }

    fn load_tool_name(
        &mut self,
        server_id: &connector_contract::ServerId,
        tool_id: &connector_contract::ToolId,
    ) -> Result<connector_service::Observed<String>, connector_service::ServiceError> {
        let read = self
            .db
            .mcp_tool_name_versioned(server_id.as_str(), tool_id.as_str())
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "connector tool name load failed",
                )
            })?;
        Ok(connector_service::Observed {
            revision: connector_revision(read.revision),
            value: read.value.ok_or_else(|| {
                connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "connector tool is unavailable",
                )
            })?,
        })
    }

    fn save_server(
        &mut self,
        expected_revision: connector_contract::Revision,
        draft: connector_contract::ServerDraft,
    ) -> Result<connector_service::RepositoryCas<()>, connector_service::ServiceError> {
        let expected = connector_storage_revision(expected_revision)?;
        let row = connector_server_row(draft)?;
        self.db
            .save_mcp_server_revision_cas(expected, &row)
            .map(|value| connector_repository_cas(value, |_| ()))
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "connector server save failed",
                )
            })
    }

    fn delete_server(
        &mut self,
        expected_revision: connector_contract::Revision,
        server_id: &connector_contract::ServerId,
    ) -> Result<connector_service::RepositoryCas<()>, connector_service::ServiceError> {
        let expected = connector_storage_revision(expected_revision)?;
        let resolved_at = unix_now_secs_i64();
        self.db
            .delete_mcp_server_revision_cas(expected, server_id.as_str(), resolved_at)
            .map(|value| connector_repository_cas(value, |_| ()))
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "connector server delete failed",
                )
            })
    }

    fn replace_tools(
        &mut self,
        expected_revision: connector_contract::Revision,
        server_id: &connector_contract::ServerId,
        tools: &[connector_service::DiscoveredTool],
    ) -> Result<connector_service::RepositoryCas<()>, connector_service::ServiceError> {
        let expected = connector_storage_revision(expected_revision)?;
        let rows = tools
            .iter()
            .map(|tool| mcp_store::McpToolRow {
                id: tool.id.as_str().to_owned(),
                server_id: server_id.as_str().to_owned(),
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema_json: None,
                trust_level: "unknown".to_owned(),
                schema_hash: None,
            })
            .collect::<Vec<_>>();
        self.db
            .replace_mcp_tools_revision_cas(expected, server_id.as_str(), &rows)
            .map(|value| connector_repository_cas(value, |()| ()))
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "connector tools replacement failed",
                )
            })
    }

    fn set_permission(
        &mut self,
        expected_revision: connector_contract::Revision,
        server_id: &connector_contract::ServerId,
        tool_id: &connector_contract::ToolId,
        rule: connector_contract::PermissionRule,
    ) -> Result<connector_service::RepositoryCas<()>, connector_service::ServiceError> {
        let expected = connector_storage_revision(expected_revision)?;
        let persisted = match rule {
            connector_contract::PermissionRule::Ask => "ask",
            connector_contract::PermissionRule::Allow => "allow",
            connector_contract::PermissionRule::Deny => "deny",
        };
        self.db
            .set_permission_by_tool_id_revision_cas(
                expected,
                server_id.as_str(),
                tool_id.as_str(),
                persisted,
                None,
            )
            .map(|value| connector_repository_cas(value, |()| ()))
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "connector permission update failed",
                )
            })
    }

    fn ensure_slack_server(
        &mut self,
        expected_revision: connector_contract::Revision,
    ) -> Result<connector_service::RepositoryCas<()>, connector_service::ServiceError> {
        let expected = connector_storage_revision(expected_revision)?;
        let row = mcp_store::McpServerRow {
            id: uuid::Uuid::new_v4().to_string(),
            name: "Slack".to_owned(),
            kind: "http".to_owned(),
            command: None,
            args: Vec::new(),
            env_plain: Vec::new(),
            env_secrets: Vec::new(),
            inherit_env: true,
            url: Some(SLACK_MCP_URL.to_owned()),
            enabled: true,
        };
        self.db
            .ensure_enabled_mcp_server_by_url_revision_cas(expected, &row)
            .map(|value| connector_repository_cas(value, |_| ()))
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "Slack connector registration failed",
                )
            })
    }

    fn parse_import(
        &mut self,
        _source_name: &str,
        bytes: &[u8],
    ) -> Result<connector_service::ImportPlan, connector_service::ServiceError> {
        let text = std::str::from_utf8(bytes).map_err(|_| {
            connector_service_error(
                connector_contract::ErrorCode::InvalidInput,
                "connector import is not UTF-8",
            )
        })?;
        let parsed = crate::mcp_import::parse_mcp_servers_json_bounded(
            text,
            connector_contract::ResourceLimits::PRODUCTION_CEILING.import_servers,
        )
        .map_err(|_| {
            connector_service_error(
                connector_contract::ErrorCode::InvalidInput,
                "connector import JSON is invalid or exceeds the server limit",
            )
        })?;
        let existing = self
            .db
            .mcp_server_inventory(
                connector_contract::ResourceLimits::PRODUCTION_CEILING.import_servers,
            )
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "connector import inventory failed",
                )
            })?
            .into_iter()
            .map(|row| row.name)
            .collect::<std::collections::HashSet<_>>();
        let mut names = existing;
        let mut candidates = Vec::new();
        let mut report = Vec::new();
        for server in parsed.servers {
            if !names.insert(server.name.clone()) {
                report.push(connector_contract::ImportReportItem {
                    name: server.name,
                    outcome: connector_contract::ImportOutcome::SkippedDuplicate,
                    error_code: None,
                    omitted_secret_env_count: server.skipped_env.len(),
                });
                continue;
            }
            candidates.push(connector_service::ImportCandidate {
                draft: connector_contract::ServerDraft {
                    id: Some(connector_contract::ServerId::new(
                        uuid::Uuid::new_v4().to_string(),
                    )),
                    name: server.name,
                    transport: connector_contract::TransportDraft::Stdio {
                        command: server.command,
                        args: server.args,
                        plain_env: server.env_plain,
                        secret_env: Vec::new(),
                        inherit_env: true,
                    },
                    enabled: true,
                },
                omitted_secret_env_count: server.skipped_env.len(),
            });
        }
        for server in parsed.http_servers {
            if !names.insert(server.name.clone()) {
                report.push(connector_contract::ImportReportItem {
                    name: server.name,
                    outcome: connector_contract::ImportOutcome::SkippedDuplicate,
                    error_code: None,
                    omitted_secret_env_count: 0,
                });
                continue;
            }
            if mcp::validate_mcp_url(&server.url).is_err() {
                report.push(connector_contract::ImportReportItem {
                    name: server.name,
                    outcome: connector_contract::ImportOutcome::Failed,
                    error_code: Some(connector_contract::ErrorCode::InvalidInput),
                    omitted_secret_env_count: 0,
                });
                continue;
            }
            candidates.push(connector_service::ImportCandidate {
                draft: connector_contract::ServerDraft {
                    id: Some(connector_contract::ServerId::new(
                        uuid::Uuid::new_v4().to_string(),
                    )),
                    name: server.name,
                    transport: connector_contract::TransportDraft::Http { url: server.url },
                    enabled: true,
                },
                omitted_secret_env_count: 0,
            });
        }
        for skipped in parsed.skipped {
            report.push(connector_contract::ImportReportItem {
                name: skipped.name,
                outcome: connector_contract::ImportOutcome::SkippedUnsupported,
                error_code: match skipped.reason {
                    crate::mcp_import::SkipReason::Invalid(_) => {
                        Some(connector_contract::ErrorCode::InvalidInput)
                    }
                    crate::mcp_import::SkipReason::LegacySse
                    | crate::mcp_import::SkipReason::MissingCommand
                    | crate::mcp_import::SkipReason::MissingUrl => None,
                },
                omitted_secret_env_count: 0,
            });
        }
        Ok(connector_service::ImportPlan { candidates, report })
    }

    fn import_servers(
        &mut self,
        expected_revision: connector_contract::Revision,
        servers: Vec<connector_contract::ServerDraft>,
    ) -> Result<connector_service::RepositoryCas<()>, connector_service::ServiceError> {
        let expected = connector_storage_revision(expected_revision)?;
        let rows = servers
            .into_iter()
            .map(connector_server_row)
            .collect::<Result<Vec<_>, _>>()?;
        self.db
            .insert_mcp_servers_batch_revision_cas(expected, &rows)
            .map(|value| connector_repository_cas(value, |_| ()))
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "connector import commit failed",
                )
            })
    }

    fn load_http_auth_binding(
        &mut self,
        server_id: &connector_contract::ServerId,
        exact_url: &str,
    ) -> Result<
        connector_service::Observed<Option<connector_service::HttpAuthBinding>>,
        connector_service::ServiceError,
    > {
        let read = self
            .db
            .credential_oauth_bindings_for_server_versioned(server_id.as_str())
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "connector OAuth binding load failed",
                )
            })?;
        let value = match read.value.as_slice() {
            [] => None,
            [binding] => Some(Self::oauth_binding(binding.clone(), server_id, exact_url)?),
            _ => {
                return Err(connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "connector OAuth binding is ambiguous",
                ));
            }
        };
        Ok(connector_service::Observed {
            revision: connector_revision(read.revision),
            value,
        })
    }

    fn load_oauth_secret_slot(
        &mut self,
        logical_id: &secret::LogicalCredentialId,
    ) -> Result<
        connector_service::Observed<Option<secret::PhysicalSecretSlot>>,
        connector_service::ServiceError,
    > {
        let read = self
            .db
            .credential_secret_location_versioned(logical_id.as_str())
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "connector OAuth slot load failed",
                )
            })?;
        let value = read
            .value
            .map(|location| {
                if location.keyring_service != secret::KEYRING_SERVICE {
                    return Err(connector_service_error(
                        connector_contract::ErrorCode::SecretUnavailable,
                        "connector OAuth slot service is invalid",
                    ));
                }
                let slot =
                    secret::PhysicalSecretSlot::parse(location.keyring_username).map_err(|_| {
                        connector_service_error(
                            connector_contract::ErrorCode::SecretUnavailable,
                            "connector OAuth slot is invalid",
                        )
                    })?;
                if !slot.belongs_to(logical_id) {
                    return Err(connector_service_error(
                        connector_contract::ErrorCode::SecretUnavailable,
                        "connector OAuth slot owner is invalid",
                    ));
                }
                Ok(slot)
            })
            .transpose()?;
        Ok(connector_service::Observed {
            revision: connector_revision(read.revision),
            value,
        })
    }

    fn register_oauth_secret_staging(
        &mut self,
        plan: &secret::SecretBundleStagePlan,
    ) -> Result<(), connector_service::ServiceError> {
        self.db
            .register_physical_secret_slot_staging(
                plan.logical_id().as_str(),
                plan.new_slot().as_str(),
            )
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "connector OAuth staging registration failed",
                )
            })
    }

    fn acknowledge_oauth_secret_deleted(
        &mut self,
        logical_id: &secret::LogicalCredentialId,
        slot: &secret::PhysicalSecretSlot,
    ) -> Result<(), connector_service::ServiceError> {
        self.db
            .acknowledge_physical_secret_slot_deleted(logical_id.as_str(), slot.as_str())
            .map(|_| ())
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "connector OAuth cleanup acknowledgement failed",
                )
            })
    }

    fn publish_oauth_secret_slot(
        &mut self,
        expected_revision: connector_contract::Revision,
        descriptor: connector_service::OAuthPublishDescriptor<'_>,
    ) -> Result<connector_service::OAuthPublishResult, connector_service::ServiceError> {
        let expected = connector_storage_revision(expected_revision)?;
        let logical_id = descriptor.staged.logical_id.as_str();
        let exact_url = descriptor.metadata.server_url();
        let metadata = descriptor
            .metadata
            .to_json_bounded(
                descriptor.metadata.server_id(),
                exact_url,
                auth::StoredOAuthMetadataLimits::PRODUCTION,
            )
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::InvalidInput,
                    "connector OAuth metadata serialization failed",
                )
            })?;
        if matches!(
            descriptor.mode,
            connector_service::OAuthPublishMode::FirstInsert
        ) {
            let meta = storage::CredentialMeta {
                id: logical_id.to_owned(),
                provider: "oauth".to_owned(),
                label: descriptor.label.to_owned(),
                credential_kind: "oauth".to_owned(),
                masked_hint: descriptor.masked_hint.map(str::to_owned),
                workspace_id: None,
            };
            let result = self
                .db
                .insert_credential_with_secret_slot_revision_cas(
                    expected,
                    &meta,
                    descriptor.staged.new_slot.as_str(),
                    Some(&metadata),
                )
                .map_err(|_| {
                    connector_service_error(
                        connector_contract::ErrorCode::StorageUnavailable,
                        "connector OAuth publish failed",
                    )
                })?;
            return Ok(match result {
                storage::ConnectorConfigCas::Stale { current_revision } => {
                    connector_service::OAuthPublishResult::RevisionStale {
                        current_revision: connector_revision(current_revision),
                    }
                }
                storage::ConnectorConfigCas::Committed {
                    revision,
                    value: (),
                } => connector_service::OAuthPublishResult::Committed {
                    revision: connector_revision(revision),
                    previous_slot: None,
                },
            });
        }
        let connector_service::OAuthPublishMode::Rotation { expected_previous } = descriptor.mode
        else {
            unreachable!("first insert returned above")
        };
        let result = self
            .db
            .publish_credential_secret_slot_revision_cas(
                expected,
                logical_id,
                expected_previous.as_str(),
                descriptor.staged.new_slot.as_str(),
                Some(&metadata),
                descriptor.masked_hint,
            )
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "connector OAuth publish failed",
                )
            })?;
        match result {
            storage::ConnectorConfigCas::Stale { current_revision } => {
                Ok(connector_service::OAuthPublishResult::RevisionStale {
                    current_revision: connector_revision(current_revision),
                })
            }
            storage::ConnectorConfigCas::Committed {
                revision,
                value: false,
            } => Ok(connector_service::OAuthPublishResult::PointerStale {
                revision: connector_revision(revision),
            }),
            storage::ConnectorConfigCas::Committed {
                revision,
                value: true,
            } => Ok(connector_service::OAuthPublishResult::Committed {
                revision: connector_revision(revision),
                previous_slot: Some(expected_previous),
            }),
        }
    }

    fn load_authorization_state(
        &mut self,
        server_id: &connector_contract::ServerId,
        tool_name: &str,
    ) -> Result<
        connector_service::Observed<connector_service::AuthorizationState>,
        connector_service::ServiceError,
    > {
        let read = self
            .db
            .permission_rule_versioned(server_id.as_str(), tool_name)
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::StorageUnavailable,
                    "connector authorization state load failed",
                )
            })?;
        let permission = match read.value {
            None => audit::PermissionFingerprint::Absent,
            Some(row) => audit::PermissionFingerprint::Persisted {
                rule: audit::PermissionRule::from_persisted(&row.rule).ok_or_else(|| {
                    connector_service_error(
                        connector_contract::ErrorCode::StorageUnavailable,
                        "stored connector authorization rule is invalid",
                    )
                })?,
                approved_schema_hash: row.approved_schema_hash,
            },
        };
        Ok(connector_service::Observed {
            revision: connector_revision(read.revision),
            value: connector_service::AuthorizationState { permission },
        })
    }

    fn commit_authorization_preflight(
        &mut self,
        expected_revision: connector_contract::Revision,
        plan: audit::AuthorizationPlan,
        arguments_json: &connector_contract::SensitiveInput,
    ) -> Result<
        connector_service::RepositoryCas<audit::AuthorizationPreflight>,
        connector_service::ServiceError,
    > {
        let expected = connector_storage_revision(expected_revision)?;
        let input = std::str::from_utf8(arguments_json.expose_bytes()).map_err(|_| {
            connector_service_error(
                connector_contract::ErrorCode::InvalidInput,
                "connector tool input is not UTF-8 JSON",
            )
        })?;
        let owner = self.authorization_owner.as_ref().ok_or_else(|| {
            connector_service_error(
                connector_contract::ErrorCode::AuditUnavailable,
                "connector authorization owner is unavailable",
            )
        })?;
        self.db
            .commit_authorization_preflight_revision_cas(
                expected,
                owner,
                plan,
                input,
                &self.redaction,
            )
            .map(|value| connector_repository_cas(value, |value| value))
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::AuditUnavailable,
                    "connector authorization preflight failed",
                )
            })
    }

    fn complete_authorization(
        &mut self,
        operation_id: &connector_contract::OperationId,
        outcome: audit::AuthorizationOutcome,
    ) -> Result<(), connector_service::ServiceError> {
        let owner = self.authorization_owner.as_ref().ok_or_else(|| {
            connector_service_error(
                connector_contract::ErrorCode::AuditUnavailable,
                "connector authorization owner is unavailable",
            )
        })?;
        self.db
            .complete_authorization_outcome(owner, operation_id.as_str(), outcome)
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::AuditUnavailable,
                    "connector authorization outcome failed",
                )
            })
    }

    fn shutdown(&mut self) -> Result<(), connector_service::ServiceError> {
        let Some(owner) = self.authorization_owner.take() else {
            return Ok(());
        };
        self.db
            .close_authorization_owner(owner)
            .map(|_| ())
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::AuditUnavailable,
                    "connector authorization shutdown failed",
                )
            })
    }
}

fn unix_now_secs_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or(i64::MAX)
}

fn unix_now_secs_u64() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

struct AppConnectorSecrets {
    secret_store: KeyringSecretStore,
    redaction: secret::RedactionService,
}

impl AppConnectorSecrets {
    fn metadata_after_refresh(
        metadata: &auth::StoredOAuthMetadata,
        expires_in_secs: Option<u64>,
    ) -> Result<auth::StoredOAuthMetadata, connector_service::ServiceError> {
        auth::StoredOAuthMetadata::new(
            auth::StoredOAuthMetadataDraft {
                server_id: metadata.server_id().to_owned(),
                server_url: metadata.server_url().to_owned(),
                issuer: metadata.issuer().to_owned(),
                authorization_endpoint: metadata.authorization_endpoint().to_owned(),
                token_endpoint: metadata.token_endpoint().to_owned(),
                oauth_resource: metadata.oauth_resource().to_owned(),
                client_id: metadata.client_id().to_owned(),
                token_endpoint_auth_method: metadata.token_endpoint_auth_method(),
                manual_client: metadata.manual_client(),
                provider_workspace_id: metadata.provider_workspace_id().map(str::to_owned),
                workspace_domain: metadata.workspace_domain().map(str::to_owned),
                scopes: metadata.scopes().to_vec(),
                expires_at_secs: expires_in_secs
                    .map(|seconds| unix_now_secs_u64().saturating_add(seconds)),
            },
            auth::StoredOAuthMetadataLimits::PRODUCTION,
        )
        .map_err(|_| {
            connector_service_error(
                connector_contract::ErrorCode::AuthenticationFailed,
                "refreshed connector OAuth metadata is invalid",
            )
        })
    }
}

impl connector_service::ConnectorSecrets for AppConnectorSecrets {
    fn load_stored_oauth_client(
        &self,
        binding: &connector_service::HttpAuthBinding,
    ) -> Result<Option<connector_service::StoredOAuthClient>, connector_service::ServiceError> {
        let Some(metadata) = binding.oauth_metadata.as_ref() else {
            return Ok(None);
        };
        let logical =
            secret::LogicalCredentialId::new(binding.credential_id.as_str()).map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::SecretUnavailable,
                    "connector OAuth credential identifier is invalid",
                )
            })?;
        if !binding.physical_slot.belongs_to(&logical) {
            return Err(connector_service_error(
                connector_contract::ErrorCode::SecretUnavailable,
                "connector OAuth slot owner is invalid",
            ));
        }
        let bundle = secret::read_secret_bundle(&self.secret_store, &binding.physical_slot)
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::SecretUnavailable,
                    "connector OAuth bundle load failed",
                )
            })?;
        let (_, _, dcr) = bundle.into_parts();
        Ok(Some(connector_service::StoredOAuthClient {
            server_id: connector_contract::ServerId::new(metadata.server_id().to_owned()),
            logical_id: logical,
            client_id: metadata.client_id().to_owned(),
            client_secret: dcr
                .map(|value| connector_contract::SensitiveInput::from(value.into_string())),
            workspace_hint: metadata.workspace_domain().map(str::to_owned),
            metadata: Some(metadata.clone()),
            manual_client: metadata.manual_client(),
        }))
    }

    fn exchange_oauth_refresh(
        &self,
        request: connector_service::OAuthRefreshRequest,
        cancellation: connector_service::CancellationToken,
    ) -> Result<connector_service::OAuthRefreshOutcome, connector_service::ServiceError> {
        if cancellation.is_cancelled() {
            return Err(connector_service_error(
                connector_contract::ErrorCode::Cancelled,
                "connector OAuth refresh was cancelled",
            ));
        }
        let bundle = secret::read_secret_bundle(&self.secret_store, &request.current_slot)
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::SecretUnavailable,
                    "connector OAuth refresh bundle load failed",
                )
            })?;
        let (_, refresh, dcr) = bundle.into_parts();
        let Some(refresh) = refresh else {
            return Ok(connector_service::OAuthRefreshOutcome::ReauthorizationRequired);
        };
        let params = auth::RefreshParams {
            token_url: request.metadata.token_endpoint().to_owned(),
            client_id: request.metadata.client_id().to_owned(),
            client_secret: dcr,
            client_secret_post: request
                .metadata
                .token_endpoint_auth_method()
                .uses_client_secret(),
            resource: Some(request.metadata.oauth_resource().to_owned()),
        };
        let outcome =
            auth::exchange_refresh_token_once(CONNECTOR_OAUTH_HTTP_TIMEOUT, refresh, &params)
                .map_err(|_| {
                    connector_service_error(
                        connector_contract::ErrorCode::AuthenticationFailed,
                        "connector OAuth refresh failed",
                    )
                })?;
        if cancellation.is_cancelled() {
            return Err(connector_service_error(
                connector_contract::ErrorCode::Cancelled,
                "connector OAuth refresh was cancelled",
            ));
        }
        match outcome {
            auth::RefreshOutcome::Refreshed(token) => {
                let metadata =
                    Self::metadata_after_refresh(&request.metadata, token.expires_in_secs)?;
                let dcr = params.client_secret;
                Ok(connector_service::OAuthRefreshOutcome::Refreshed(Box::new(
                    connector_service::OAuthCredentialUpdate {
                        logical_id: request.logical_id,
                        label: request.label,
                        masked_hint: Some(secret::masked_hint(token.access_token.expose())),
                        bundle: secret::SecretBundle::new(
                            token.access_token,
                            token.refresh_token,
                            dcr,
                        ),
                        metadata,
                    },
                )))
            }
            auth::RefreshOutcome::ReauthorizationRequired { .. }
            | auth::RefreshOutcome::AlreadyRefreshed => {
                Ok(connector_service::OAuthRefreshOutcome::ReauthorizationRequired)
            }
        }
    }

    fn resolve_credentials(
        &self,
        requests: Vec<connector_service::CredentialResolutionRequest>,
    ) -> Result<connector_service::ResolvedCredentials, connector_service::ServiceError> {
        if requests.is_empty() {
            return Ok(connector_service::ResolvedCredentials::empty());
        }
        let mut values = Vec::with_capacity(requests.len());
        for request in &requests {
            let logical = secret::LogicalCredentialId::new(request.credential_id.as_str())
                .map_err(|_| {
                    connector_service_error(
                        connector_contract::ErrorCode::SecretUnavailable,
                        "connector credential identifier is invalid",
                    )
                })?;
            let slot = request.expected_physical_slot.as_ref().ok_or_else(|| {
                connector_service_error(
                    connector_contract::ErrorCode::SecretUnavailable,
                    "connector credential physical slot is missing",
                )
            })?;
            if !slot.belongs_to(&logical) {
                return Err(connector_service_error(
                    connector_contract::ErrorCode::SecretUnavailable,
                    "connector credential physical slot owner is invalid",
                ));
            }
            values.push(
                secret::SecretStore::get_secret(&self.secret_store, slot.as_str()).map_err(
                    |_| {
                        connector_service_error(
                            connector_contract::ErrorCode::SecretUnavailable,
                            "connector credential load failed",
                        )
                    },
                )?,
            );
        }
        let refs = values.iter().collect::<Vec<_>>();
        let redaction = self
            .redaction
            .acquire_json_execution_lease(&refs)
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::SecretUnavailable,
                    "connector redaction lease is unavailable",
                )
            })?;
        let entries = requests
            .into_iter()
            .zip(values)
            .map(|(request, value)| {
                connector_service::ResolvedCredential::new(
                    request.credential_id,
                    request
                        .expected_physical_slot
                        .expect("validated physical slot"),
                    value,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        connector_service::ResolvedCredentials::new(entries, redaction)
    }

    fn stage_oauth_bundle(
        &self,
        plan: &secret::SecretBundleStagePlan,
        bundle: secret::SecretBundle,
    ) -> Result<secret::StagedSecretBundle, connector_service::ServiceError> {
        secret::stage_secret_bundle(&self.secret_store, plan, bundle.as_ref()).map_err(|_| {
            connector_service_error(
                connector_contract::ErrorCode::SecretUnavailable,
                "connector OAuth bundle staging failed",
            )
        })
    }

    fn delete_oauth_bundle(
        &self,
        slot: &secret::PhysicalSecretSlot,
    ) -> Result<(), connector_service::ServiceError> {
        secret::delete_secret_bundle(&self.secret_store, slot)
            .map(|_| ())
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::SecretUnavailable,
                    "connector OAuth bundle cleanup failed",
                )
            })
    }

    fn sanitized_input_preview(
        &self,
        arguments_json: &connector_contract::SensitiveInput,
        max_chars: usize,
    ) -> Result<String, connector_service::ServiceError> {
        audit::sanitized_input_preview(arguments_json.expose_bytes(), &self.redaction, max_chars)
            .map_err(|_| {
                connector_service_error(
                    connector_contract::ErrorCode::InvalidInput,
                    "connector input preview failed",
                )
            })
    }
}

struct AppConnectorHost {
    ctx: egui::Context,
}

impl connector_service::ConnectorHost for AppConnectorHost {
    fn wake(&self) {
        self.ctx.request_repaint();
    }
}

/// 한 workspace의 런타임 상태 묶음 (워커-per-workspace §14.1 준비 — Stage A).
/// 활성 workspace는 렌더되고, (후속) warm workspace는 이벤트만 드레인된다.
struct WorkspaceRuntime {
    id: String,
    /// Monotonic identity for one concrete worker lifetime. Workspace IDs can be reused after a
    /// suspend/recreate, so async freshness checks must never key only by workspace ID.
    runtime_instance: u64,
    /// Source stamp whose default env was accepted by this exact runtime lifetime.
    dotenv_state: Option<DotenvState>,
    runtime: InProcessRuntimeClient,
    events: RuntimeEventReceiver,
    workspace_ui: ui::workspace::WorkspaceUi,
    /// worker에 마지막으로 보낸 render 활성 상태 (§14.1 Active↔Warm) — 전이 시에만 전송
    render_active: bool,
    /// logic()에서 drain했지만 아직 ui()가 렌더에 소비하지 않은 이벤트 (§14.1 Warm:
    /// 알림은 logic()에서 처리하고 렌더는 Active 복귀 시 ui()가 몰아서 소비).
    pending_events: Vec<runtime::RuntimeEvent>,
    /// 세션→제목 캐시 (MuxUpdated에서 누적) — Warm 동안 mux가 안 갱신돼도 알림 제목을
    /// 해석하기 위함. exit 처리 후 제거해 live 세션으로 유계.
    /// 세션별 **raw** pane 제목("workspace.spawn.shell 3"). 표시 시점에 해석한다 —
    /// 활동 패널/폰은 프로젝트명 규칙(activity_session_name), 알림은 i18n 렌더.
    session_titles: std::collections::HashMap<runtime::SessionId, String>,
    /// 마지막 worker resource sample. PR-U25 activity view 표시용.
    resource_usage: Option<runtime::ProcessResourceSnapshot>,
    /// 마지막 worker child-process resource samples. Runtime이 집계한 값만 보관한다.
    session_resource_usage: Vec<runtime::SessionResourceUsage>,
    /// 세션별 자식 프로세스 폭주 판정 상태 (로드맵 B1). ResourceUsage 샘플로만
    /// 갱신되고 세션 exit 시 제거 — live 세션 수로 유계.
    storm_episodes:
        std::collections::HashMap<runtime::SessionId, crate::process_storm::StormEpisode>,
    /// 폭주 에피소드 id 발급 카운터 — 해소 후 재발생 구분(B2 재알림 근거).
    storm_next_episode_id: u64,
    /// 폭주 확정 전이 시 (세션, peak 프로세스 수)를 쌓는 알림 큐 (B2). logic()이
    /// drain해 OS 알림을 1회 발화한다 — 에피소드당 확정 1회만 쌓인다.
    storm_notify_pending: Vec<(runtime::SessionId, usize)>,
    /// 사용자가 동결(SIGSTOP)한 세션 (로드맵 B3). SessionFreezeChanged로 갱신,
    /// exit 시 제거 — live 세션 수로 유계.
    frozen_sessions: std::collections::HashSet<runtime::SessionId>,
    /// 마지막 PTY input pressure signal(+관측 시각). 회복 이벤트가 없어(QueueFull은
    /// writer drain으로 조용히 해소) 표시 시 TTL로 stale 뱃지를 걸러낸다(codex 2026-07-08).
    input_pressure: Option<(runtime::PtyInputPressure, std::time::Instant)>,
    /// 세션별 마지막 input pressure(+관측 시각) — 활동 뷰 pane 서브행용. exit 시 제거.
    session_input_pressure: std::collections::HashMap<
        runtime::SessionId,
        (runtime::PtyInputPressure, std::time::Instant),
    >,
    /// Source stamp inherited by each concrete shell process at creation. Updating runtime
    /// defaults does not retroactively mutate an existing shell environment, so programmatic
    /// resume may use WriteInput only while this exact session stamp is still current.
    session_dotenv_states: std::collections::HashMap<runtime::SessionId, DotenvState>,
    /// Warm으로 내려간 시각. 일정 시간 이후 자동 Suspended(워커 shutdown)로 내린다.
    backgrounded_at: Option<std::time::Instant>,
    /// live 세션 추적 (suspend 보호 — 이벤트 스트림에서 갱신).
    live: LiveSessionTracker,
    /// 워커 생성 시각 — 첫 MuxUpdated 관측 전 suspend 유예(RESTORE 관측 창) 판정용.
    created: std::time::Instant,
    /// 응답(AgentSpawned/SpawnFailed) 대기 중인 agent spawn 수 — 전환 시 전역
    /// AgentsUi에서 이관받는다 (agent spawn 직후 전환 race의 live 판정).
    pending_agent_spawns: u32,
    /// durable 구독 overflow 뒤 이미 큐에 들어온 이벤트를 budget 단위로 끝까지 적용한 다음
    /// fresh receiver로 재구독하기 위한 상태.
    event_overflow_pending: bool,
    /// active receiver 재구독 뒤 전체 mux/viewport snapshot 재전송 명령이 아직 남아 있다.
    event_resync_pending: bool,
    /// replay cap overflow 뒤 다음 Warm→Active full snapshot 전환으로 보정해야 한다.
    pending_replay_resync: bool,
}

#[derive(Clone)]
struct WorkspaceGitLabelCacheEntry {
    checked_at: std::time::Instant,
    last_accessed: std::time::Instant,
    label: Option<String>,
}

const WORKSPACE_GIT_LABEL_CACHE_CAP: usize = 256;
const WORKSPACE_GIT_LABEL_CACHE_FRESHNESS: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Default)]
struct WorkspaceGitLabelCache {
    entries: std::collections::HashMap<String, WorkspaceGitLabelCacheEntry>,
}

impl WorkspaceGitLabelCache {
    fn get_fresh(&mut self, path: &str, now: std::time::Instant) -> Option<Option<String>> {
        let entry = self.entries.get_mut(path)?;
        if now.duration_since(entry.checked_at) >= WORKSPACE_GIT_LABEL_CACHE_FRESHNESS {
            return None;
        }
        entry.last_accessed = now;
        Some(entry.label.clone())
    }

    fn insert(&mut self, path: String, label: Option<String>, now: std::time::Instant) {
        if let Some(entry) = self.entries.get_mut(&path) {
            entry.checked_at = now;
            entry.last_accessed = now;
            entry.label = label;
            return;
        }
        while self.entries.len() >= WORKSPACE_GIT_LABEL_CACHE_CAP {
            let Some(oldest_path) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_accessed)
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            self.entries.remove(&oldest_path);
        }
        self.entries.insert(
            path,
            WorkspaceGitLabelCacheEntry {
                checked_at: now,
                last_accessed: now,
                label,
            },
        );
    }

    #[cfg(test)]
    fn entry_count(&self) -> usize {
        self.entries.len()
    }
}

fn workspace_git_label(path: &str) -> Option<String> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<WorkspaceGitLabelCache>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(WorkspaceGitLabelCache::default()));
    let now = std::time::Instant::now();
    if let Ok(mut entries) = cache.lock()
        && let Some(label) = entries.get_fresh(path, now)
    {
        return label;
    }

    let root = std::path::Path::new(path);
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        Some(dot_git)
    } else {
        std::fs::read_to_string(&dot_git).ok().and_then(|contents| {
            let relative = contents.trim().strip_prefix("gitdir:")?.trim();
            let candidate = std::path::PathBuf::from(relative);
            Some(if candidate.is_absolute() {
                candidate
            } else {
                root.join(candidate)
            })
        })
    };
    let label = git_dir.and_then(|git_dir| {
        let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
        let head = head.trim();
        let branch = head
            .strip_prefix("ref: refs/heads/")
            .map(str::to_owned)
            .unwrap_or_else(|| head.chars().take(7).collect());
        Some(branch)
    });
    if let Ok(mut entries) = cache.lock() {
        entries.insert(path.to_owned(), label.clone(), std::time::Instant::now());
    }
    label
}

fn claude_usage_snapshot() -> Option<(u8, u8)> {
    type Cache = Option<(std::time::Instant, Option<(u8, u8)>)>;
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Cache>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(None));
    let mut cache = cache.lock().ok()?;
    if let Some((checked_at, usage)) = *cache
        && checked_at.elapsed() < std::time::Duration::from_secs(2)
    {
        return usage;
    }
    let usage = (|| {
        let path = crate::paths::home_dir()?
            .join(".deppy-sijo")
            .join("claude-usage.json");
        if std::fs::metadata(&path).ok()?.len() > 4 * 1024 {
            return None;
        }
        let snapshot: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
        let percent = |name: &str| {
            snapshot
                .get(name)
                .and_then(serde_json::Value::as_f64)
                .filter(|value| value.is_finite())
                .map(|value| value.clamp(0.0, 100.0).round() as u8)
        };
        Some((percent("five_hour")?, percent("seven_day")?))
    })();
    *cache = Some((std::time::Instant::now(), usage));
    usage
}

pub(crate) fn top_provider_usage(
    ui: &mut egui::Ui,
    claude_usage: Option<(u8, u8)>,
    codex_usage: Option<(u8, u8)>,
) {
    for text_style in [
        egui::TextStyle::Body,
        egui::TextStyle::Button,
        egui::TextStyle::Small,
    ] {
        ui.style_mut()
            .text_styles
            .insert(text_style, crate::fonts::sidebar_font(13.0));
    }

    fn separator(ui: &mut egui::Ui, height: f32) {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(1.0, height), egui::Sense::hover());
        ui.painter().vline(
            rect.center().x,
            rect.y_range(),
            egui::Stroke::new(1.0, ui.visuals().weak_text_color().gamma_multiply(0.55)),
        );
    }

    fn provider(ui: &mut egui::Ui, name: &str, accent: egui::Color32, usage: Option<(u8, u8)>) {
        let (logo, _) = ui.allocate_exact_size(egui::vec2(14.5, 14.5), egui::Sense::hover());
        crate::ui::agent_terminal::paint_announcement_provider_logo(ui, logo, name);

        let five_hour = usage.map(|value| value.0);
        let weekly = usage.map(|value| value.1);
        let five_hour_label = five_hour.map_or_else(|| "—".to_owned(), |value| format!("{value}%"));
        let weekly_label = weekly.map_or_else(|| "—".to_owned(), |value| format!("{value}%"));
        ui.label(
            egui::RichText::new(five_hour_label)
                .size(13.0)
                .color(accent)
                .strong(),
        );

        let (bar, _) = ui.allocate_exact_size(egui::vec2(42.0, 6.0), egui::Sense::hover());
        ui.painter()
            .rect_filled(bar, 3.0, egui::Color32::from_gray(42));
        if let Some(five_hour) = five_hour {
            let filled = egui::Rect::from_min_max(
                bar.min,
                egui::pos2(
                    bar.left() + bar.width() * f32::from(five_hour) / 100.0,
                    bar.bottom(),
                ),
            );
            ui.painter().rect_filled(filled, 3.0, accent);
        }
        ui.label(egui::RichText::new("5h").size(13.0).weak());
        separator(ui, 14.0);
        ui.label(egui::RichText::new("이번 주").size(13.0).weak());
        ui.label(
            egui::RichText::new(weekly_label)
                .size(13.0)
                .color(accent)
                .strong(),
        );
    }

    ui.allocate_ui_with_layout(
        egui::vec2(430.0, 20.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            provider(
                ui,
                "Claude",
                egui::Color32::from_rgb(0xe7, 0x9a, 0x3b),
                claude_usage,
            );
            ui.add_space(4.0);
            separator(ui, 18.0);
            ui.add_space(4.0);
            provider(ui, "Codex", ui.visuals().hyperlink_color, codex_usage);
        },
    );
}

impl WorkspaceRuntime {
    /// 아직 종료(Exited)되지 않은 세션이 pane에 하나라도 있으면 true — 셸이든
    /// 에이전트든 떠 있는 것 자체가 실행 중이다. 이런 workspace는 Suspended(워커
    /// shutdown = PTY kill)로 내리면 안 된다 (2026-07-05 사용자 요구: 진행 중인
    /// 에이전트 작업이 경고 없이 죽는 문제).
    ///
    /// tracker 외 두 가지를 추가로 live 취급한다 (codex High — spawn/restore race):
    /// - 응답 대기 중인 셸 spawn (명령이 큐/워커에 있고 MuxUpdated가 아직 안 옴)
    /// - 워커 생성 직후 첫 MuxUpdated 관측 전의 유예 창 (RestoreWorkspace 복원 세션이
    ///   아직 이벤트로 안 왔을 수 있다 — 빈 workspace는 restore가 emit하지 않으므로
    ///   유예가 끝나면 정상적으로 suspend 가능해진다)
    fn has_live_sessions(&self) -> bool {
        workspace_is_live(
            self.live.has_live(),
            self.live.seen_mux,
            self.workspace_ui.pending_spawns() + self.pending_agent_spawns,
            self.created.elapsed(),
        )
    }

    /// 30분 warm timeout 뒤 안전하게 재생성 가능한 "프롬프트 대기 셸만" 남았는지.
    /// 에이전트/미분류 세션, 자식 프로세스, resource 샘플 부재는 모두 작업 중으로 보고
    /// 보호한다. 셸 자체는 layout/cwd에서 다시 spawn되므로 이 조건에서만 suspend 가능하다.
    fn can_auto_suspend_idle_shells(&self) -> bool {
        if self.workspace_ui.pending_spawns() + self.pending_agent_spawns > 0 || !self.live.seen_mux
        {
            return false;
        }
        let Some(sessions) = self.live.live_shell_sessions() else {
            return false;
        };
        shell_sessions_are_idle(&sessions, &self.session_resource_usage, |session| {
            self.workspace_ui.agent_line_for(session).is_some()
        })
    }
}

/// suspend 보호의 live 판정 (순수 함수 — 테스트 용이).
fn workspace_is_live(
    tracker_live: bool,
    seen_mux: bool,
    pending_spawns: u32,
    age: std::time::Duration,
) -> bool {
    /// 첫 MuxUpdated 관측 전 suspend를 미루는 유예 — restore 이벤트 전파(수 ms)보다
    /// 넉넉히. 빈 workspace는 이 유예만 지나면 suspend 대상이 된다.
    const RESTORE_GRACE: std::time::Duration = std::time::Duration::from_secs(10);
    tracker_live || pending_spawns > 0 || (!seen_mux && age < RESTORE_GRACE)
}

fn projected_live_warm_count(
    current_live_warm: usize,
    target_is_live_warm: bool,
    active_will_be_live: bool,
) -> usize {
    current_live_warm.saturating_sub(usize::from(target_is_live_warm))
        + usize::from(active_will_be_live)
}

/// 이벤트 스트림에서 "pane에 붙어 있고 아직 Exited 안 된 세션"을 추적한다.
/// MuxUpdated가 세션 집합의 근거, SessionExited가 종료 마킹 — 이벤트 순서대로
/// 갱신해 한 drain 안의 Exited → pane 제거 MuxUpdated 시퀀스도 정확히 반영된다.
#[derive(Default)]
struct LiveSessionTracker {
    /// 최신 MuxUpdated 기준 pane에 붙은 세션 집합.
    mux_sessions: std::collections::HashSet<runtime::SessionId>,
    /// SessionExited를 관측한 세션 (mux_sessions에 남은 것만 유지해 유계).
    exited_sessions: std::collections::HashSet<runtime::SessionId>,
    /// Spawn 이벤트로 확인한 세션 종류. MuxUpdated가 먼저 오므로 종류 미확인 창은
    /// unknown으로 남겨 suspend를 보수적으로 막는다.
    session_kinds: std::collections::HashMap<runtime::SessionId, runtime::SpawnKind>,
    /// MuxUpdated를 한 번이라도 관측했다 — 관측 전에는 restore 유예가 적용된다.
    seen_mux: bool,
}

impl LiveSessionTracker {
    fn observe(&mut self, event: &runtime::RuntimeEvent) {
        match event {
            runtime::RuntimeEvent::MuxUpdated { snapshot } => {
                self.seen_mux = true;
                self.mux_sessions = snapshot
                    .tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .filter_map(|pane| pane.session_id)
                    .collect();
                self.exited_sessions
                    .retain(|s| self.mux_sessions.contains(s));
                self.session_kinds
                    .retain(|s, _| self.mux_sessions.contains(s));
            }
            runtime::RuntimeEvent::ShellSpawned { session } => {
                self.session_kinds
                    .insert(*session, runtime::SpawnKind::Shell);
            }
            runtime::RuntimeEvent::AgentSpawned { session } => {
                self.session_kinds
                    .insert(*session, runtime::SpawnKind::Agent);
            }
            runtime::RuntimeEvent::SessionExited { session, .. }
            // 재시작 시 archived 복원된 세션도 이미 종료됨 — 생존 추적에서 제외해야
            // auto-suspend/warm 축출이 정상 동작한다 (PR-A2 codex 리뷰 P1).
            | runtime::RuntimeEvent::SessionRestored { session, .. } => {
                self.exited_sessions.insert(*session);
            }
            _ => {}
        }
    }

    fn has_live(&self) -> bool {
        self.mux_sessions
            .iter()
            .any(|s| !self.exited_sessions.contains(s))
    }

    /// live 세션이 하나 이상이고 전부 명시적으로 Shell일 때만 목록을 반환한다.
    /// MuxUpdated→Spawned 사이 unknown 또는 Agent가 하나라도 있으면 None(작업 보호).
    fn live_shell_sessions(&self) -> Option<Vec<runtime::SessionId>> {
        let live = self
            .mux_sessions
            .iter()
            .copied()
            .filter(|session| !self.exited_sessions.contains(session))
            .collect::<Vec<_>>();
        if live.is_empty()
            || live
                .iter()
                .any(|session| self.session_kinds.get(session) != Some(&runtime::SpawnKind::Shell))
        {
            return None;
        }
        Some(live)
    }
}

/// 실행 중인 remote TLS 서버 + 그 신원 지문(attach 클라이언트 대조용).
/// 원격 worker는 server가 소유(move)한다 — 활성 workspace worker와 별개의 전용 worker라
/// 수명이 서로 얽히지 않는다. Drop/shutdown이 accept 루프·접속·worker를 모두 정리한다.
struct RemoteTlsState {
    server: runtime::RemoteRuntimeServer,
    fingerprint: String,
}

/// 실행 중인 모바일 웹(PWA) 서버 + 페어링 토큰(접속 URL/QR 표시용) — mobile-pwa v3.3 P1.
/// Drop/shutdown이 accept 루프·접속 스레드를 모두 정리한다 (RemoteTlsState 관례).
struct WebRemoteState {
    server: web_remote::WebRemoteServer,
    /// keyring에서 로드한 페어링 토큰 — 서버가 `/?token=` 게이트로 검증하는 값과 동일.
    token: String,
}

/// Optional web-remote persistence adapter. The concrete SQLite handle is constructed only from
/// the composition root and shared by dashboard/push through the storage-neutral port. Each
/// method releases the mutex before the caller performs network I/O.
struct AppWebRemoteRepository {
    db: std::sync::Mutex<Db>,
}

impl AppWebRemoteRepository {
    fn open(path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            db: std::sync::Mutex::new(Db::open(path)?),
        })
    }

    fn lock(&self) -> anyhow::Result<std::sync::MutexGuard<'_, Db>> {
        self.db
            .lock()
            .map_err(|_| anyhow::anyhow!("web_remote_repository_lock_failed"))
    }
}

impl web_remote::repository::WebRemoteRepository for AppWebRemoteRepository {
    fn list_pending_approvals(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<web_remote::repository::PendingApprovalRecord>> {
        anyhow::ensure!(
            limit <= web_remote::repository::PENDING_APPROVAL_LIMIT,
            "web_remote_pending_limit_exceeded"
        );
        Ok(self
            .lock()?
            .list_pending_approvals_bounded(limit)?
            .rows
            .into_iter()
            .map(|row| web_remote::repository::PendingApprovalRecord {
                id: row.id,
                server_id: row.server_id,
                tool_name: row.tool_name,
                arguments_preview: row.arguments_preview,
                created_at: row.created_at,
                pane_id: row.pane_id,
            })
            .collect())
    }

    fn resolve_approval(
        &self,
        id: &str,
        allowed: bool,
        remember: bool,
        resolved_at: i64,
    ) -> anyhow::Result<()> {
        self.lock()?
            .resolve_approval(id, allowed, remember, resolved_at)
    }

    fn web_push_subscription_count(&self) -> anyhow::Result<usize> {
        usize::try_from(self.lock()?.count_web_push_subscriptions()?)
            .map_err(|_| anyhow::anyhow!("web_push_subscription_count_invalid"))
    }

    fn upsert_web_push_subscription(
        &self,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
        created_at: i64,
        limit: usize,
    ) -> anyhow::Result<web_remote::repository::SubscriptionUpsert> {
        anyhow::ensure!(
            limit <= web_remote::repository::PUSH_SUBSCRIPTION_LIMIT,
            "web_push_subscription_limit_invalid"
        );
        let db = self.lock()?;
        let existing = db
            .list_web_push_subscriptions_bounded(web_remote::repository::PUSH_SUBSCRIPTION_LIMIT)?;
        anyhow::ensure!(
            existing.len() <= web_remote::repository::PUSH_SUBSCRIPTION_LIMIT,
            "web_push_subscription_inventory_oversized"
        );
        if existing.len() >= limit && !existing.iter().any(|row| row.endpoint == endpoint) {
            return Ok(web_remote::repository::SubscriptionUpsert::LimitReached);
        }
        db.upsert_web_push_subscription(endpoint, p256dh, auth, created_at)?;
        let total = usize::try_from(db.count_web_push_subscriptions()?)
            .map_err(|_| anyhow::anyhow!("web_push_subscription_count_invalid"))?;
        anyhow::ensure!(total <= limit, "web_push_subscription_limit_exceeded");
        Ok(web_remote::repository::SubscriptionUpsert::Stored { total })
    }

    fn list_web_push_subscriptions(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<web_remote::repository::PushSubscriptionRecord>> {
        anyhow::ensure!(
            limit <= web_remote::repository::PUSH_SUBSCRIPTION_LIMIT,
            "web_push_subscription_limit_invalid"
        );
        let rows = self.lock()?.list_web_push_subscriptions_bounded(limit)?;
        Ok(rows
            .into_iter()
            .map(|row| {
                web_remote::repository::PushSubscriptionRecord::new(
                    row.endpoint,
                    row.p256dh,
                    row.auth,
                )
            })
            .collect())
    }

    fn touch_web_push_subscription(&self, endpoint: &str, last_ok_at: i64) -> anyhow::Result<()> {
        self.lock()?
            .touch_web_push_subscription(endpoint, last_ok_at)
    }

    fn delete_web_push_subscription(&self, endpoint: &str) -> anyhow::Result<usize> {
        let db = self.lock()?;
        db.delete_web_push_subscription(endpoint)?;
        usize::try_from(db.count_web_push_subscriptions()?)
            .map_err(|_| anyhow::anyhow!("web_push_subscription_count_invalid"))
    }
}

/// 세션 알림(완료/입력대기) 주목 상태 — 레일 폭(6px)·1회 펄스 추적 (2026-07-07).
struct SessionAlert {
    status: runtime::SessionStatus,
    /// 사용자가 확인(포커스)했는가 — false면 레일 6px 유지.
    seen: bool,
    /// 알림 도착 시 이미 포커스 중이던 pane의 1회 펄스 시작 시각.
    pulse_started: Option<std::time::Instant>,
}

/// (env key, 값 또는 credential_id) 쌍 목록 — SetSessionDefaultEnv용.
type EnvPairs = Vec<(String, String)>;

fn font_settings_changed(
    config: &Config,
    last_ui_font: &Option<String>,
    last_mono_font: &str,
    last_mono_weight: &str,
) -> bool {
    &config.ui.ui_font != last_ui_font
        || config.terminal.mono_font.as_str() != last_mono_font
        || config.terminal.mono_weight.as_str() != last_mono_weight
}

/// 워크트리 삭제 백그라운드 작업의 성공 결과 — (지운 워크트리 루트, 브랜치 처리
/// 결과, 그 루트 하위 cwd였던 (workspace id, 세션) 목록). worktree_remove_rx 참조.
type WorktreeRemoveOutcome = (
    std::path::PathBuf,
    crate::worktree::BranchCleanup,
    Vec<(String, runtime::SessionId)>,
);

pub struct App {
    config: Config,
    config_path: PathBuf,
    /// 직전 프레임의 실효 테마(다크 여부) — 바뀌면 터미널 렌더 캐시를 비운다.
    /// System 테마의 OS 레벨 전환은 config_changed를 안 거치므로 매 프레임 감지한다(#7 codex).
    last_theme_dark: bool,
    /// 직전 프레임의 UI 폰트 설정 — 바뀌면 폰트 재등록(hot reload).
    last_ui_font: Option<String>,
    /// 터미널 모노 굵기 변경 감지용(hot reload 트리거).
    last_mono_font: String,
    last_mono_weight: String,
    /// UI 배율 변경 감지용(zoom_factor 재적용 트리거). 첫 프레임 적용을 위해 sentinel로 시작.
    last_ui_scale: f32,
    last_dotenv_state: Option<DotenvState>,
    dotenv_sync_worker: DotenvSyncWorker,
    /// 활성 workspace/root가 바뀔 때 증가한다. 옛 worker 결과가 새 workspace runtime에
    /// 주입되는 것을 막는 epoch이다.
    dotenv_sync_generation: u64,
    /// 같은 workspace/root에서도 watcher/manual 변경이 들어오면 증가해 실행 중이던 옛
    /// 파일 snapshot 결과를 폐기한다. 주기적 unchanged poll은 증가시키지 않는다.
    dotenv_sync_revision: u64,
    /// Worker correlation only. Never reused while the process is alive.
    dotenv_next_operation_id: u64,
    dotenv_sync_context: Option<(String, Option<PathBuf>, u64)>,
    /// Runtime commands are retained exactly once here; worker jobs contain only freshness scope.
    /// The worker independently caps accepted continuations at the same fixed eight operations.
    dotenv_pending_operations: std::collections::HashMap<u64, PendingDotenvOperation>,
    /// Checked retained heap bytes across the same exact launch commands.
    dotenv_pending_bytes: usize,
    /// 프로젝트 폴더 rename 복구 확인 모달 — Some((old, new))이면 표시(2026-07-08).
    workspace_rename_prompt: Option<(String, String)>,
    /// Environment & API 프로젝트 닫기 확인 대기 — sidebar/DB 삭제와 무관하다.
    env_project_close_confirm: Option<(String, String)>,
    /// 사이드바 「워크스페이스 종료」 확인 대기 — Some((id, 표시명, 세션 수, 실행 중 수)).
    /// 확정 시 세션(pane)만 일괄 닫고 워크스페이스(경로·설정·DB 기록)는 보존한다.
    ws_close_confirm: Option<(String, String, usize, usize)>,
    /// 사이드바 「이름 바꾸기」 모달 — Some((id, 편집 버퍼)). 별칭(name 컬럼)만
    /// 바꾸고 실제 폴더/경로는 불변. 빈 값 확정 = 별칭 해제(폴더명 복귀).
    ws_rename_edit: Option<(String, String)>,
    /// runtime durable 이벤트 큐가 포화돼 느린 구독자가 끊긴 경우 사용자 경고 모달.
    runtime_stream_warning: bool,
    /// live warm hard cap을 넘기는 workspace 전환을 거부했을 때 대상 표시명.
    warm_limit_warning: Option<String>,
    /// 폰(미러 진입 — I1b-2)이 보낸 워크스페이스 전환 요청 큐. 웹 스레드가 push하고 egui
    /// 스레드가 ui() 시작에서 drain해 switch_workspace로 넘긴다(App은 egui 스레드 소유).
    web_switch_queue: Arc<std::sync::Mutex<Vec<String>>>,
    /// 폰에 띄울 일시 안내(전환 상한 초과 등 — I1b-2)와 세팅 시각. TTL이 지나면 프레임
    /// push에서 None으로 돌려 배너를 내린다(데스크탑 warm_limit_warning 모달과 독립).
    web_notice: Option<(String, std::time::Instant)>,
    /// rename 제안을 '무시'한 워크스페이스 — 이번 실행 동안 재확인 안 함(경로 변경 시 해제).
    dismissed_renames: std::collections::HashSet<String>,
    settings_open: bool,
    /// 직전 프레임의 설정창 열림 상태 — 닫힘 전이에서 env 평문 캐시를 비운다(보안).
    settings_was_open: bool,
    /// 통합 설정 창의 선택된 카테고리.
    settings_category: ui::settings::Category,
    /// 설정 창 안에서만 선택된 workspace. 사이드바 표시/활성 runtime/terminal focus와
    /// 독립이며 Environment/Workspaces 관리 화면의 대상만 바꾼다.
    settings_workspace_id: Option<String>,
    settings_search: String,
    env_api_project_edit: EnvApiProjectEditState,
    /// Event-invalidated environment project snapshot. Every production mutation path calls
    /// `invalidate_env_api_projects`; render only clones this Arc and no periodic TTL read exists.
    env_api_projects_cache: Option<Arc<[ui::env_project_list::EnvProjectRow]>>,
    /// T1: pane 우클릭 → 환경설정 진입 시 감지한 focused 세션 폴더 배너.
    /// 우클릭 진입 시점에만 계산하고, 버튼 클릭 또는 설정 창 닫힘에 버린다.
    env_session_banner: Option<EnvSessionCwdBanner>,
    /// 폭주 경고 배너를 닫음 (로드맵 B2). 현재 폭주가 모두 해소되면 리셋돼 다음
    /// 폭주에 다시 뜬다 — 에피소드별 상태를 안 들고도 유계.
    storm_banner_dismissed: bool,
    /// 배너 버튼이 요청한 폭주 대응 (로드맵 B3). ui()는 렌더 경로라 명령을 못 보내므로
    /// 여기 담고 logic()에서 소비한다.
    pending_storm_action: Option<StormAction>,
    env_project_rows_worker: EnvProjectRowsWorker,
    env_project_rows_generation: u64,
    /// Exact generation currently owned by the capacity-one worker. Invalidation advances the
    /// desired generation but never clears this token; the stale completion releases it and the
    /// same logic tick submits the newest projection.
    env_project_rows_in_flight: Option<u64>,
    env_project_rows_failed: bool,
    env_secret_reveal_worker: EnvSecretRevealWorker,
    pending_env_secret_reveal: Option<EnvSecretRevealJob>,
    env_secret_generation: u64,
    /// Agents/Environment leaf가 읽는 immutable snapshot과 mutation intent를 전담한다.
    /// 첫 설정 요청에서만 thread/SQLite connection을 만들고 30초 idle이면 둘 다 회수한다.
    settings_snapshot_worker: SettingsSnapshotWorker,
    /// Render/logic producers stage at most one settings job; worker spawn/send/join happens only
    /// from logic on the next tick.
    pending_settings_job: Option<SettingsJob>,
    settings_snapshot_generation: u64,
    settings_snapshot_revision: u64,
    settings_snapshot_pending: bool,
    settings_pending_operation: Option<SettingsOperationKey>,
    settings_snapshot_retry_at: Option<std::time::Instant>,
    settings_snapshot_workspace_id: Option<String>,
    agents_snapshot: ui::agents::AgentsSnapshot,
    env_profiles_snapshot: ui::env_profiles::EnvProfilesSnapshot,
    credentials_snapshot: ui::credentials::CredentialsSnapshot,
    agent_sessions_secrets_snapshot: ui::agent_sessions::AgentSessionsSecretsSnapshot,
    db: Db,
    secret_store: KeyringSecretStore,
    agents_ui: ui::agents::AgentsUi,
    agent_launcher_ui: ui::agent_launcher::AgentLauncherUi,
    agent_launcher_worker:
        crate::lazy_worker::LazyBoundedWorker<(), crate::agent_launcher::DetectionSnapshot>,
    agent_launcher_snapshot: Option<crate::agent_launcher::DetectionSnapshot>,
    agent_launcher_detection_requested: bool,
    agent_launcher_detection_in_flight: bool,
    pending_agent_launcher_intent: Option<ui::agent_launcher::AgentLauncherIntent>,
    next_agent_launcher_request_id: u64,
    pending_agent_launcher_launch: Option<PendingAgentLauncherLaunch>,
    agent_launcher_seen_workspaces: std::collections::HashSet<String>,
    /// PTY와 분리된 Codex App Server structured session controller.
    agent_sessions_ui: ui::agent_sessions::AgentSessionsUi,
    /// Render가 반환한 controller action 한 건. 다음 logic tick에서만 실행해 process와
    /// protocol I/O가 render call graph에 들어오지 않게 한다.
    pending_agent_sessions_action: Option<ui::agent_sessions::AgentSessionsDeferredAction>,
    /// 세션 cwd 레포의 git 변경분 리뷰 패널 (사이드바 「변경 보기」).
    diff_panel_ui: ui::diff_panel::DiffPanelUi,
    /// Lazy aggregate boundary for hook/attention/restore/binding/resume/catalog/project-name
    /// persistence and filesystem projections. Construction opens no DB and starts no thread.
    agent_state_worker: crate::agent_state_worker::AgentStateWorker<AppAgentStateBackend>,
    agent_state_scope: Arc<AppAgentStateScope>,
    pending_agent_state_scope: Option<Arc<AppAgentStateScope>>,
    agent_state_next_revision: u64,
    agent_state_next_operation_id: u64,
    /// Set only after two immediate known-unsent worker admissions fail. It is cleared by a
    /// relevant new exact/scope/manual action, never by a frame timer or scheduled repaint.
    agent_state_admission_blocked: bool,
    /// Filesystem-derived names applied only from a current bounded ProjectNames completion.
    activity_project_names: std::collections::HashMap<String, Option<String>>,
    activity_project_name_style: crate::config::SessionNameStyle,
    project_name_projection_dirty: bool,
    project_name_projection_pending: bool,
    /// One bounded structured mutation batch retained until worker admission succeeds.
    pending_agent_state_structured: Vec<storage::StructuredThreadMutation>,
    /// Lazy Connector service + latest-only render snapshot. Construction performs no DB open,
    /// thread, process, network request, polling, or scheduled repaint.
    connector_coordinator: connector_service::ConnectorCoordinator,
    connector_snapshot_reader: connector_service::SnapshotReader,
    connector_ui: connector_ui::ConnectorUi,
    /// Connector render가 반환한 intent 한 건. 최초 service start/DB open은 다음 logic tick.
    pending_connector_dispatch: Option<(
        connector_contract::ConnectorIntent,
        Option<connector_service::InvocationContext>,
    )>,
    /// App-owned single-flight file/dialog/browser task. `None` until a host action is emitted;
    /// there is no host thread/channel/poll/timer while unused. Connector와 Composer native
    /// picker가 이 한 슬롯을 공유해 동시에 둘 이상의 dialog/process를 만들지 않는다.
    app_host_io: Option<AppHostIoTask>,
    /// Render가 반환한 native-host intent. 다음 logic tick에서만 host task로 넘기며
    /// latest-only 한 건만 보존한다.
    pending_app_host_action: Option<AppHostIoAction>,
    pending_file_tree_maintenance: Option<ui::file_tree::FileTreeMaintenanceIntent>,
    file_tree_watcher: Option<AppFileTreeWatcher>,
    /// Settings가 반환한 lifecycle action 한 건. 다음 logic tick에서만 실행한다.
    pending_app_controller_action: Option<AppControllerAction>,
    /// Render가 반환한 workspace/runtime action 한 건. 다음 logic tick에서만 실행한다.
    pending_workspace_controller_action: Option<WorkspaceControllerAction>,
    /// Focused completion acknowledgement. Render removes the in-memory generation and stages at
    /// most one durable clear; SQLite is touched only by the following logic tick.
    pending_turn_done_clear: Option<(String, i64)>,
    /// 설정 전체 변경과 단순 config 저장은 각각 latest-only bit로 합쳐 backlog를 막는다.
    pending_settings_config_apply: bool,
    pending_config_save: bool,
    /// Send events publish one Arc-backed bounded history snapshot. Only the latest snapshot needs
    /// persistence, so a busy host task retains one replacement rather than a write queue.
    pending_composer_history: Option<Arc<[Arc<str>]>>,
    /// 폴더 선택 결과를 settings queue가 빌 때까지 한 건만 보존한다. 선택 결과를 적용하기
    /// 전에는 다음 host action을 시작하지 않아 raw path/backlog가 늘지 않는다.
    pending_folder_picker_completion: Option<(FolderPickerPurpose, PathBuf)>,
    credentials_ui: ui::credentials::CredentialsUi,
    env_profiles_ui: ui::env_profiles::EnvProfilesUi,
    activity_ui: ui::activity::ActivityUi,
    /// 목업 기반 홈/터미널 전환과 전체 워크스페이스 대시보드 상태.
    agent_terminal_ui: ui::agent_terminal::AgentTerminalUi,
    /// 멀티에이전트 fleet 그리드 (기능1) — Fleet 뷰에서 그린다.
    fleet_ui: ui::fleet::FleetUi,
    /// fleet 배치 스폰(PR-S1) 대기 — Some이면 매 logic tick `pump_batch_spawn`이 settings
    /// 잡 큐 1슬롯이 빌 때마다 하나씩 launch를 큐잉한다.
    pending_batch_spawn: Option<PendingBatchSpawn>,
    /// 브로드캐스트 직후 대상 세션을 잠깐 "작업 중"으로 낙관적 표시하기 위한 타임스탬프
    /// ((workspace_id, session) → 전송 시각). 전송했으니 지금 작업을 시작했다는 걸 아는데
    /// transcript 감지에는 지연이 있어(warm은 아예 활동 추적 안 됨) 그 공백을 메운다.
    /// BROADCAST_WORKING_WINDOW 안에서 감지 상태가 Idle/Off일 때만 Active로 덮는다.
    broadcast_working: std::collections::HashMap<(String, runtime::SessionId), std::time::Instant>,
    /// 상태바·홈이 쓰는 activity_rows 500ms 캐시 — 매 프레임(타이핑 중 60~120fps)
    /// 전 워크스페이스 × 세션의 String/Vec 재조립을 피한다. 리소스 샘플 주기(2s)보다
    /// 짧아 표시 신선도는 유지된다.
    activity_rows_cache: Option<(std::time::Instant, ui::activity::ActivitySnapshot)>,
    /// 홈 업데이트 피드 수신(Claude/OpenAI 상태 5분, 공지/HF/Grok 4시간) + 최신
    /// 스냅샷. provider별 조회 실패(None)면 마지막 성공값을 유지한다.
    status_feed_rx: crate::status_feed::StatusFeedReceiver,
    /// 수동 갱신(홈 「AI 공지」 ⟳ 버튼) — 워커를 즉시 깨워 상태+공지 재조회.
    status_feed_refresh: crate::status_feed::StatusFeedRefresh,
    /// 앱 시작 시 상태 점등을 즉시 채우도록 1회 폴링을 보냈는가 — 첫 logic tick에서
    /// 소비한다(홈을 열지 않아도 하단 서비스 점등이 바로 켜지게, 2026-07-23 사용자).
    status_feed_startup_polled: bool,
    status_feed: crate::status_feed::StatusFeedSnapshot,
    /// Home을 마지막으로 본 시점의 provider별 공지 ID와 현재 신규 공지 배지 수.
    /// 별도 작은 JSON으로 영속해 앱 재시작 때 기존 공지가 다시 새 알림이 되지 않는다.
    notice_read_state: crate::status_feed::NoticeReadState,
    notice_read_state_path: PathBuf,
    home_notice_unread: usize,
    /// 공지 제목 번역 영속 캐시(제공자+로케일+원문 → 번역) + 진행 중 번역 수신.
    /// 앱 재시작 뒤에도 같은 제목은 재번역하지 않으며, 저장 실패는 원문 표시로 완화한다.
    notice_translation_cache: crate::notice_translate::TranslationCache,
    notice_translation_cache_path: PathBuf,
    notice_translate_rx: Option<
        std::sync::mpsc::Receiver<Vec<(crate::notice_translate::TranslationCacheKey, String)>>,
    >,
    /// 실제 번역 수요가 처음 생길 때 한 번만 CLI 경로를 해석한다. 바깥 Option은
    /// 해석 여부, 안쪽 Option은 설치 여부라 공지 미사용 idle stat은 0이다.
    notice_translate_bin: Option<Option<PathBuf>>,
    /// ollama 모델 감지 (PR-L3) — Agents 창에서 OSS 프로바이더 선택 시 1회 감지해
    /// 새 작업 폼의 모델 후보로 보여준다. None=미감지/실패(표시 안 함).
    ollama_models: Option<Vec<String>>,
    ollama_detect_done: bool,
    ollama_detect_rx: Option<std::sync::mpsc::Receiver<crate::local_llm::LocalLlmSnapshot>>,
    /// Agents 창 열림 전환 감지용 직전 프레임 상태 — 재오픈마다 ollama 자동 재감지.
    agent_sessions_was_open: bool,
    notifications_ui: ui::notifications::NotificationsUi,
    /// 벨 팝오버 「대기 중」 섹션의 PTY 입력 대기 카드 렌더 상태 (v3.9 N3) — 자유 입력칸
    /// 버퍼 + 로그 tail 미리보기 캐시. 팝오버가 열려 있을 때만 조회한다(idle 비용 0).
    inbox_waiting_ui: ui::inbox_waiting::InboxWaitingUi,
    /// 하단 도크 프롬프트 컴포저 (2026-07-17) — 워크스페이스별 드래프트 + 영속 히스토리.
    composer: ui::composer::ComposerUi,
    composer_history_path: PathBuf,
    /// 프롬프트 라이브러리 (기능2) — 저장된 에이전트 프롬프트 팔레트. 파레트에서 고른
    /// 프롬프트는 파라미터를 채워 활성 세션의 컴포저 버퍼에 삽입된다.
    prompt_palette: ui::prompt_palette::PromptPaletteUi,
    prompt_library: crate::prompt_library::PromptLibrary,
    /// 저장/삭제 영속화 경로 (persist_prompt_library).
    prompt_library_path: PathBuf,
    /// agent-proxy 승인 팝업 (option 1.5). proxy가 DB에 쓴 pending 행을 폴링해 표시한다.
    approvals_ui: ui::approvals::ApprovalsUi,
    /// 이미 알림을 발화한 pending 승인 id — 폴링마다 재발화하지 않기 위한 기억.
    /// 매 폴링에서 현재 pending 집합으로 통째 교체되므로 성장하지 않는다.
    approval_notified: std::collections::HashSet<String>,
    /// 같은 physical DB의 startup pending reconciliation을 다른 app instance와 직렬화한다.
    _pending_approval_owner: Arc<storage::ActivePendingApprovalOwner>,
    /// proxy launch/approval decision이 있을 때만 존재하는 event-driven DB/socket worker.
    approval_wake_hub: ApprovalWakeHub,
    approval_launch_tracker: ApprovalLaunchTracker,
    approval_global_reconcile: ApprovalGlobalReconcile,
    pending_proxy_launches: std::collections::VecDeque<PendingProxyLaunch>,
    /// 마지막 오프스크린 창 위치 보정 시각 (쿨다운용)
    last_offscreen_fix: std::time::Instant,
    /// 시작 시 창을 주 화면으로 1회 이동했다 (centered의 macOS 좌표 문제 우회)
    startup_positioned: bool,
    frame_stats: crate::perf::FrameStats,
    /// 렌더러 A/B 실측 드라이버 (B1) — env 미설정이면 None이고 모든 훅이 no-op이다.
    bench: Option<crate::bench::Bench>,
    /// PR-21 hidden load harness. Retains only the next bounded fixture index; each concrete
    /// command goes through the same exact dotenv continuation as production launches.
    perf_harness_next: Option<usize>,
    i18n: i18n::Catalog,
    /// 현재 활성(렌더되는) workspace의 런타임 상태.
    active: WorkspaceRuntime,
    /// Next concrete runtime identity; zero is never issued.
    next_runtime_instance: u64,
    /// warm workspace들 (전환으로 물러났지만 워커는 계속 실행 — §14.1 Warm). 이벤트는
    /// drain만 하고(채널 backup 방지) 렌더/알림은 안 한다. 재활성 시 즉시 복귀.
    warm: std::collections::HashMap<String, WorkspaceRuntime>,
    /// warm LRU 순서 (앞이 가장 오래됨) — max_warm 초과 시 앞에서부터 Suspended(shutdown).
    warm_order: Vec<String>,
    egui_ctx: egui::Context,
    db_path: PathBuf,
    logs_base: PathBuf,
    runtime_host_factory: Arc<runtime::InProcessRuntimeHostFactory>,
    workspaces: Vec<storage::WorkspaceRow>,
    /// Bounded workspace projection의 all-or-nothing filesystem identity. Render/rename
    /// detection은 path/anchor를 per-workspace로 다시 조회하지 않는다.
    workspace_anchors: std::collections::HashMap<String, storage::WorkspaceFolderAnchor>,
    /// 런타임이 없는 workspace도 활동 화면에 복원 대상 pane을 표시하기 위한 DB snapshot.
    /// refresh_workspaces에서 한 쿼리로 갱신한다.
    /// 워크스페이스별 영속 pane snapshot — (raw 제목, 세션 cwd). cwd는 기본 제목
    /// ("셸 N")을 프로젝트명으로 표시하는 데 쓴다(활성 워크스페이스와 같은 규칙).
    persisted_activity_panes: std::collections::HashMap<String, Vec<(String, String)>>,
    /// 옵션2: 활성 세션별 에이전트 transcript 활동(working/idle) — 레일 상태에 반영.
    agent_activity:
        std::collections::HashMap<runtime::SessionId, crate::agent_transcript::AgentActivity>,
    /// 세션 → 바인딩된 에이전트(transcript 경로 포함). 바인딩 폴(느림)에서 갱신,
    /// 활동 폴(빠름)이 이걸 재파싱한다.
    agent_bindings:
        std::collections::HashMap<runtime::SessionId, crate::agent_detect::AgentBinding>,
    /// agent 감지 백그라운드 워커(ps/lsof/transcript 스캔을 UI 스레드 밖에서, codex #3).
    /// 필드로 보유만 한다 — App drop 시 이 필드의 Drop이 스레드를 stop+join한다(직접 read X).
    #[allow(dead_code)]
    agent_detect_worker: crate::agent_detect_worker::AgentDetectWorker,
    /// 워커 입력(활성 세션 pid 목록 + epoch) — 매 프레임 최신값 write-through.
    agent_detect_input: crate::agent_detect_worker::DetectInput,
    /// 워커의 capacity-one 최신 결과 수신기.
    agent_detect_rx: crate::agent_detect_worker::DetectOutcomeReceiver,
    /// 워크스페이스 전환마다 증가 — 스레드가 실어 보낸 stale 결과를 폐기하는 데 쓴다.
    agent_detect_epoch: u64,
    /// hook 바인딩 DB 조회 스로틀(1s) — poll_agent_detect는 매 프레임 돌아 매번 쿼리하면
    /// 렌더 중 초당 수십 회가 된다. 캐시를 워커 입력에 재사용.
    last_hook_query: std::time::Instant,
    hook_overrides:
        std::collections::HashMap<runtime::SessionId, crate::agent_detect::AgentBinding>,
    /// 마지막으로 DB에 저장한 pane_id → row — 차등 upsert/delete 및 churn 방지용.
    persisted_agents: std::collections::HashMap<String, storage::AgentSessionRow>,
    /// hook이 보고한 입력 대기(needsInput) 세션들 — DB에서 주기적으로 읽어 레일 주황 반영.
    agent_needs_input: std::collections::HashSet<runtime::SessionId>,
    /// v3.9 N3: 전역(모든 워크스페이스) 입력 대기 — 벨 팝오버 PTY 카드의 소스.
    /// (workspace_id, SessionId, hook이 보고한 대기 사유 문구). 제목/미리보기 등 나머지
    /// 표시 데이터는 팝오버가 열렸을 때만 지연 해석한다(idle 비용 0).
    /// agent_needs_input(활성 전용, 사이드바/상태 레일이 쓴다)과는 별개 필드다.
    global_waiting: Vec<(String, runtime::SessionId, Option<String>)>,
    /// hook이 보고한 턴 완료(Stop) 세션 → updated_at — 레일 '완료(바이올렛)' 트랜지언트
    /// 소스. 값(updated_at)은 소비 시 조건부 clear의 세대 기준(레이스 방지, codex 리뷰).
    agent_turn_done: std::collections::HashMap<runtime::SessionId, i64>,
    /// hook이 보고한 "작업 중"(v32, cmux식 턴 경계) — UserPromptSubmit/PreToolUse가 기록.
    /// transcript 폴링(지연·활성 전용)과 달리 즉시·전 워크스페이스. 활성 전용 set은
    /// session_entries 경로용, global은 warm 워크스페이스(fleet/사이드바)용 —
    /// SessionId가 워크스페이스마다 재사용될 수 있어 global은 (workspace, session) 쌍.
    agent_working: std::collections::HashSet<runtime::SessionId>,
    global_working: std::collections::HashSet<(String, runtime::SessionId)>,
    /// v3.9 N3의 global_waiting/global_working과 같은 패턴 — turn_done이 storage에서
    /// 전역(prefix 없음) 스코프가 되어(warm turn_done 격차, 감사 발견) warm 워크스페이스도
    /// "완료" 표시가 가능해졌다. agent_turn_done(활성 전용)과 달리 워크스페이스별로 갈라
    /// fleet/사이드바 warm 경로에 넘긴다.
    global_turn_done: std::collections::HashMap<(String, runtime::SessionId), i64>,
    /// 완료/입력대기 주목(attention) 추적 — 미확인이면 레일 6px, 포커스 확인 시 해제.
    session_alerts: std::collections::HashMap<runtime::SessionId, SessionAlert>,
    /// 세션별 현재 작업 폴더(감지 워커 lsof) — 행 1행 폴더명 + 워크스페이스명.
    session_cwds: std::collections::HashMap<runtime::SessionId, String>,
    /// 세션별 에이전트 표시 정보(model/effort/context) — 워커 raw(transcript). claude는
    /// effort/context를 statusLine DB(아래)에서 병합해 최종본을 WorkspaceUi로 넘긴다.
    agent_info: std::collections::HashMap<runtime::SessionId, crate::agent_detect::AgentDisplay>,
    /// claude statusLine이 보고한 effort/model/context% — 1s 스로틀로 DB에서 읽어 병합.
    statuslines: std::collections::HashMap<runtime::SessionId, storage::StatuslineRow>,
    /// 복원용으로 로드한 (pane_id → 저장된 에이전트 세션). 워크스페이스 활성 시 로드.
    restore_agents: std::collections::HashMap<String, storage::AgentSessionRow>,
    /// restore_agents를 로드한 워크스페이스 id (전환 시 재로드 판정).
    restore_loaded_for: Option<String>,
    /// 이번 workspace 활성화에서 자동 resume 판단을 끝낸 pane. 명령을 보낸 경우뿐 아니라
    /// 이미 에이전트/ssh 등 다른 작업이 있어 건너뛴 경우도 포함한다. 그래야 사용자가
    /// 작업을 종료한 뒤 뒤늦게 resume 명령이 주입되지 않는다.
    resumed_panes: std::collections::HashSet<String>,
    resume_probe_pending_panes: std::collections::HashSet<String>,
    /// 알림 클릭으로 다른 workspace 전환 후, mux 재구성되면 이동할 (workspace, session).
    pending_focus: Option<(String, runtime::SessionId)>,
    /// 전환으로 background 정리 중인 옛 워커 shutdown 스레드들 (workspace_id, handle).
    /// 앱 종료 시 join(자식 reap 보장) + 같은 workspace 재오픈 전 직렬화(layout 경합 방지).
    pending_shutdowns: PendingShutdownRegistry,
    /// Earliest eligible warm-runtime suspension deadline. Recomputed only when warm lifecycle
    /// state changes, so `logic()` does not scan the warm set on every frame.
    next_warm_idle_eviction_at: Option<std::time::Instant>,
    /// A capacity-blocked eviction is retried only after a shutdown completion wakes the app.
    warm_eviction_deferred: bool,
    /// 사이드바 「워크스페이스 종료」로 숨긴 워크스페이스의 실행 중 상태.
    /// 종료 ≠ 삭제(DB·경로·별칭 보존) — 사이드바 목록에서만 감춘다(2026-07-18 사용자 요구).
    /// 영속 복원된 숨김은 stale layout pane만으로 자동 해제하면 안 된다. 현재 프로세스에서
    /// 막 닫는 활성 workspace만 closing pane 집합을 보유하고, 그 집합에 없는 새 pane이
    /// 생겼을 때만 자동 복귀한다. 명시적 전환/같은 폴더 재선택은 양쪽 상태를 모두 해제한다.
    closed_workspaces: std::collections::HashMap<String, ClosedWorkspaceState>,
    /// remote TLS 서버 (켜져 있을 때만 Some). 활성 workspace worker와 별개의 전용 worker를 노출.
    remote: Option<RemoteTlsState>,
    /// remote 시작 실패 시 settings에 표시할 에러 (best-effort — 앱은 계속, 크래시 금지).
    remote_error: Option<String>,
    /// settings의 토큰 표시(reveal) 토글. 토큰은 민감이라 기본 마스킹.
    remote_reveal_token: bool,
    /// 모바일 웹(PWA) 서버 (켜져 있을 때만 Some). OFF면 리스너 스레드 자체가 없다 — 리소스 0.
    web: Option<WebRemoteState>,
    /// 웹서버 시작/토큰 재발급 실패 시 settings에 표시할 에러.
    web_error: Option<String>,
    /// settings의 접속 URL 표시(reveal) 토글 — URL에 페어링 토큰이 실리므로 기본 마스킹.
    web_reveal_url: bool,
    /// 접속 URL QR 텍스처 캐시 — URL이 바뀔 때만 재생성, 설정창 닫으면 반환.
    web_qr: ui::settings::WebQrCache,
    /// ts.net 호스트명 자동 감지 1회성 스레드의 결과 수신 (진행 중일 때만 Some).
    /// [PR-W] 진행 중인 워크트리 생성 — (요청 시점 workspace id, 결과 채널). 백그라운드
    /// git 작업 완료를 프레임 폴링으로 수령해, 그 워크스페이스가 여전히 활성일 때만
    /// 그 폴더에서 셸을 스폰한다(전환됐으면 오배치 대신 알림). None = 진행 중 아님.
    worktree_rx: Option<(
        String,
        std::sync::mpsc::Receiver<anyhow::Result<std::path::PathBuf>>,
    )>,
    /// 진행 중인 워크트리 삭제 — (요청 시점 workspace id, 결과 채널). 결과는 (지운
    /// 워크트리 루트, 브랜치 처리 결과, 그 루트 하위 cwd였던 (workspace id, 세션)
    /// 목록). 목록은 요청 시점 활성+warm 전체의 (세션, 셸 pid) 스냅샷을 백그라운드
    /// 스레드가 삭제 성공 후 lsof(process_cwd)로 실측해 만든다 — warm 세션은 cwd
    /// 캐시가 없고(감지 워커 입력이 활성 pid뿐), UI 스레드에서의 lsof 다건 호출은
    /// 스톨 위험이라 스레드에서 한다. 성공하면 그 pane들을 활성/warm 가리지 않고
    /// 닫는다(같은 폴더 형제 pane·하위 폴더로 cd한 pane 포함, 2026-07-18).
    worktree_remove_rx: Option<(
        String,
        std::sync::mpsc::Receiver<anyhow::Result<WorktreeRemoveOutcome>>,
    )>,
    ts_detect_rx: Option<std::sync::mpsc::Receiver<crate::tailscale::Detected>>,
    /// 마지막 감지 결과 — 설정 UI 표시용. None = 이 세션에서 아직 시도 안 함.
    ts_detected: Option<crate::tailscale::Detected>,
    /// 이번 감지가 수동 버튼 유래인가 — true면 기존 설정값도 감지값으로 덮어쓴다.
    ts_detect_overwrite: bool,
    /// 웹 스냅샷 마지막 동기화 시각 — 프레임마다 구축하지 않도록 스로틀(리뷰 P2-2).
    last_web_sync: Option<std::time::Instant>,
    /// serve 진단/설정 1회성 스레드의 결과 수신 (진행 중일 때만 Some) — O1.
    serve_rx: Option<std::sync::mpsc::Receiver<crate::tailscale::ServeState>>,
    /// 마지막 serve 진단 결과. None = 이 세션에서 아직 진단 안 함.
    serve_state: Option<crate::tailscale::ServeState>,
    /// known_hosts 표시 캐시 (settings 열 때 lazily 로드, 닫으면 None으로 리셋해 재로드).
    known_hosts_cache: Option<Vec<(String, String)>>,
    /// Agents render가 읽는 검증 완료 cwd. key가 바뀔 때만 logic에서 metadata를 확인한다.
    agent_workspace_cwd_key: Option<(String, String)>,
    agent_workspace_cwd: Option<String>,
    /// 폴더 트리 사이드바 (file-tree-design §6). OFF면 None — Panel 미생성 + 상태 drop(리소스 0).
    file_tree: Option<ui::file_tree::FileTreeUi>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClosedWorkspaceState {
    /// config에서 복원했거나 비활성/warm 상태에서 종료해 자동 재노출하면 안 되는 숨김.
    Persisted,
    /// 현재 프로세스에서 활성 pane을 닫는 중. 이 집합에 없는 pane만 명시적인 새 세션이다.
    ClosingPanes(std::collections::HashSet<String>),
}

impl ClosedWorkspaceState {
    fn should_auto_reveal<'a>(&self, pane_ids: impl IntoIterator<Item = &'a str>) -> bool {
        match self {
            Self::Persisted => false,
            Self::ClosingPanes(closing) => pane_ids.into_iter().any(|pane| !closing.contains(pane)),
        }
    }
}

fn workspace_visible_after_close(
    closed: &std::collections::HashMap<String, ClosedWorkspaceState>,
    workspace_id: &str,
) -> bool {
    !closed.contains_key(workspace_id)
}

/// hook 상태에서 "새 턴 시작"으로 볼 전이만 고른다 — 직전에 막혀 있던(대기 또는 완료)
/// 세션이 작업 중으로 바뀐 것.
///
/// working 재진입 자체는 턴 경계가 아니다: working의 stale 창은 2분인데 하트비트는 툴
/// 호출(PreToolUse)마다라, 한 턴 안에서 툴 사이가 2분을 넘으면 같은 턴이 working에서
/// 빠졌다 다시 들어온다. 그걸 턴 시작으로 오인하면 진행 중인 턴의 진짜 Error latch를
/// 지우게 된다(병렬 리뷰 medium).
fn turn_start_transitions(
    working_now: &std::collections::HashSet<(String, runtime::SessionId)>,
    working_before: &std::collections::HashSet<(String, runtime::SessionId)>,
    blocked_before: &std::collections::HashSet<(String, runtime::SessionId)>,
) -> Vec<(String, runtime::SessionId)> {
    working_now
        .difference(working_before)
        .filter(|key| blocked_before.contains(*key))
        .cloned()
        .collect()
}

/// 설정 창의 workspace context만 결정한다. DB 목록 존재 여부만 보며 sidebar의
/// `closed_workspaces`나 active runtime은 입력조차 받지 않아 선택만으로 재노출/전환할 수 없다.
fn resolve_settings_workspace_id(
    workspaces: &[storage::WorkspaceRow],
    requested: Option<&str>,
    current: Option<&str>,
    active_id: &str,
) -> Option<String> {
    [requested, current, Some(active_id)]
        .into_iter()
        .flatten()
        .find(|candidate| {
            workspaces
                .iter()
                .any(|workspace| workspace.id == *candidate)
        })
        .map(str::to_owned)
        .or_else(|| workspaces.first().map(|workspace| workspace.id.clone()))
}

fn initial_workspace_id(
    db: &Db,
    last_workspace_id: Option<&str>,
    closed_workspace_ids: &std::collections::BTreeSet<String>,
) -> anyhow::Result<String> {
    let fallback = db.ensure_default_workspace()?;
    // Startup uses the same SQL-preflighted 256-row/byte-bounded projection as settings. The
    // legacy full-row list remains test/compatibility surface only and must not materialize an
    // attacker-sized workspace table before choosing one id.
    let workspaces = db.settings_workspace_projection_rows()?;
    if let Some(last) = last_workspace_id
        && !closed_workspace_ids.contains(last)
        && workspaces.iter().any(|workspace| workspace.id == last)
    {
        return Ok(last.to_owned());
    }
    Ok(workspaces
        .iter()
        .find(|workspace| !closed_workspace_ids.contains(&workspace.id))
        .map(|workspace| workspace.id.clone())
        .unwrap_or(fallback))
}

/// Environment & API 목록 안에서만 설정 context를 결정한다. sidebar visibility와
/// runtime active는 후보 우선순위에만 쓰고 수정하지 않는다.
fn resolve_settings_env_project_id(
    projects: &[ui::env_project_list::EnvProjectRow],
    requested: Option<&str>,
    current: Option<&str>,
    active_id: &str,
) -> Option<String> {
    [requested, current, Some(active_id)]
        .into_iter()
        .flatten()
        .find(|candidate| projects.iter().any(|project| project.id == *candidate))
        .map(str::to_owned)
        .or_else(|| projects.first().map(|project| project.id.clone()))
}

/// Environment & API에서 프로젝트를 닫는다. 설정 목록의 숨김 집합과 다음 설정 선택만
/// 바꾸며 workspace DB/sidebar/runtime/session 상태는 입력으로 받지 않는다.
fn close_settings_env_project(
    hidden: &mut std::collections::BTreeSet<String>,
    visible_projects: &[ui::env_project_list::EnvProjectRow],
    workspace_id: &str,
    current: Option<&str>,
) -> Option<String> {
    if !visible_projects
        .iter()
        .any(|project| project.id == workspace_id)
    {
        return current.map(str::to_owned);
    }
    hidden.insert(workspace_id.to_owned());
    if current != Some(workspace_id) {
        return current
            .filter(|current| {
                visible_projects
                    .iter()
                    .any(|project| project.id == *current)
            })
            .map(str::to_owned);
    }
    visible_projects
        .iter()
        .find(|project| project.id != workspace_id)
        .map(|project| project.id.clone())
}

fn clear_closed_workspace_state(
    closed: &mut std::collections::HashMap<String, ClosedWorkspaceState>,
    persisted: &mut std::collections::BTreeSet<String>,
    workspace_id: &str,
) -> bool {
    let runtime_removed = closed.remove(workspace_id).is_some();
    let persisted_removed = persisted.remove(workspace_id);
    runtime_removed || persisted_removed
}

#[cfg(test)]
fn stale_agent_session_panes<'a>(
    persisted_panes: impl IntoIterator<Item = &'a str>,
    live_panes: &std::collections::HashSet<String>,
) -> Vec<String> {
    let mut stale: Vec<String> = persisted_panes
        .into_iter()
        .filter(|pane_id| !live_panes.contains(*pane_id))
        .map(str::to_owned)
        .collect();
    stale.sort_unstable();
    stale
}

/// 저장된 에이전트 세션을 자동으로 이어갈지 결정한다. 자동 주입은 workspace restore 때
/// 비어 있는 로컬 셸에만 허용한다. 이미 에이전트가 실행 중이었거나 ssh/tmux/editor 같은
/// 다른 프로세스가 붙은 pane은 이번 활성화에서 처리 완료로 표시해, 그 작업/에이전트가
/// 나중에 끝나도 resume 명령을 뒤늦게 주입하지 않는다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoResumeDecision {
    /// 이미 판단을 끝낸 pane.
    Skip,
    /// 자원 스냅샷이 아직 없어 안전 여부를 판단할 수 없음 — fail closed 후 다음 poll 대기.
    Wait,
    /// 셸만 살아 있어 자동 resume 가능.
    Resume,
    /// 에이전트나 다른 foreground 작업이 있으므로 자동 주입 없이 처리 완료.
    MarkHandled,
}

fn auto_resume_decision(
    already_handled: bool,
    agent_running: bool,
    live_process_count: Option<usize>,
) -> AutoResumeDecision {
    if already_handled {
        return AutoResumeDecision::Skip;
    }
    if agent_running {
        return AutoResumeDecision::MarkHandled;
    }
    match live_process_count {
        Some(1) => AutoResumeDecision::Resume,
        Some(2..) => AutoResumeDecision::MarkHandled,
        Some(0) | None => AutoResumeDecision::Wait,
    }
}

fn resume_probe_completion_allowed(
    manual: bool,
    already_handled: bool,
    agent_running: bool,
    live_process_count: Option<usize>,
) -> bool {
    if manual {
        !agent_running && live_process_count == Some(1)
    } else {
        auto_resume_decision(already_handled, agent_running, live_process_count)
            == AutoResumeDecision::Resume
    }
}

fn read_connector_import_file(
    path: &std::path::Path,
) -> Result<Vec<u8>, connector_contract::ErrorCode> {
    let limit = connector_contract::ResourceLimits::PRODUCTION_CEILING.import_input_bytes;
    let file =
        std::fs::File::open(path).map_err(|_| connector_contract::ErrorCode::StorageUnavailable)?;
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut bounded = std::io::Read::take(
        file,
        u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1),
    );
    std::io::Read::read_to_end(&mut bounded, &mut bytes)
        .map_err(|_| connector_contract::ErrorCode::StorageUnavailable)?;
    if bytes.len() > limit {
        return Err(connector_contract::ErrorCode::LimitExceeded);
    }
    Ok(bytes)
}

fn open_connector_sensitive_url(url: &connector_contract::SensitiveInput) -> bool {
    let Ok(url) = std::str::from_utf8(url.expose_bytes()) else {
        return false;
    };
    if !is_bounded_https_url(url) {
        return false;
    }
    auth::open_in_browser_reaped(url).is_ok()
}

const APP_HOST_PATH_MAX_BYTES: usize = 32 * 1024;
const APP_HOST_URL_MAX_BYTES: usize = 32 * 1024;
const APP_HOST_FILE_OPERATION_MAX_ITEMS: usize = 50_000;
const APP_HOST_FILE_OPERATION_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const APP_HOST_FILE_OPERATION_MAX_DEPTH: usize = 128;
const APP_HOST_FILE_COPY_BUFFER_BYTES: usize = 64 * 1024;

fn is_bounded_https_url(url: &str) -> bool {
    url.len() <= APP_HOST_URL_MAX_BYTES
        && !url.contains(['\0', '\r', '\n'])
        && url.starts_with("https://")
}

enum AppHostIoAction {
    Connector(connector_service::HostAction),
    Workspace {
        workspace_id: String,
        intent: ui::workspace::WorkspaceIoIntent,
    },
    FileTree(ui::file_tree::FileTreeIoIntent),
    FileTreeMaintenance(ui::file_tree::FileTreeMaintenanceIntent),
    InboxPreview(ui::inbox_waiting::LogPreviewIntent),
    Diff(ui::diff_panel::DiffIoIntent),
    ComposerContextFile(ui::composer::ContextFileRequest),
    ComposerClipboard(ui::composer::ClipboardAttachmentRequest),
    PersistComposerHistory {
        path: PathBuf,
        history: Arc<[Arc<str>]>,
    },
    FolderPicker(FolderPickerPurpose),
    OpenPath(PathBuf),
    ExternalHttpsUrl(String),
}

/// Root-owned lifecycle mutations emitted by Settings. The UI keeps at most one action and
/// `logic()` executes it on the next tick, so render never opens files/keyring/listeners/processes.
enum AppControllerAction {
    OpenFileAccessSettings,
    DetectEnvSessionBanner { cwd: String },
    CreateWorktree { workspace_id: String, cwd: String },
    RemoveWorktree { workspace_id: String, cwd: String },
    RemoteStart,
    RemoteStop,
    ForgetKnownHost(String),
    WebStart,
    WebStop,
    RotateWebToken,
    DetectHostname,
    CheckServe,
    ConfigureServe,
}

/// Capacity-one high-level terminal/workspace action emitted by the render pass. Runtime
/// protocol, persistence, process/session lifecycle, and transcript filesystem work execute only
/// when `logic` drains this slot on the next tick.
enum WorkspaceControllerAction {
    OpenAgentLauncher,
    SwitchWorkspace(String),
    FocusSession {
        workspace_id: String,
        tab: runtime::MuxTabId,
        pane: runtime::MuxPaneId,
    },
    Runtime(runtime::RuntimeCommand),
    SpawnShellAt {
        cwd: Option<String>,
    },
    ResumeAgent {
        pane_key: String,
        title: String,
        session: runtime::SessionId,
    },
    ClosePane(runtime::MuxPaneId),
    CloseWorkspace(String),
    FocusPty {
        switch_workspace: Option<String>,
        session: runtime::SessionId,
    },
    OpenStructured {
        switch_workspace: Option<String>,
        session_id: String,
    },
    Notify {
        summary: String,
        body: String,
    },
    ComposerPrompt(Arc<str>),
    SyncDotenv,
}

#[derive(Clone)]
enum FolderPickerPurpose {
    SwitchWorkspace,
    SelectWorkspaceInSettings,
    SetProjectPath { workspace_id: String },
}

enum AppHostIoCompletion {
    Dispatch(connector_contract::ConnectorIntent),
    Workspace {
        workspace_id: String,
        completion: ui::workspace::WorkspaceIoCompletion,
    },
    FileTree(ui::file_tree::FileTreeIoCompletion),
    FileTreeMaintenance(ui::file_tree::FileTreeMaintenanceCompletion),
    InboxPreview(ui::inbox_waiting::LogPreviewCompletion),
    Diff(ui::diff_panel::DiffIoCompletion),
    ComposerContextFile {
        request: ui::composer::ContextFileRequest,
        selected_path: Option<PathBuf>,
    },
    ComposerClipboard {
        request: ui::composer::ClipboardAttachmentRequest,
        payload: Option<ui::composer::ClipboardAttachmentPayload>,
    },
    ComposerHistoryWriteFailed,
    FolderPicker {
        purpose: FolderPickerPurpose,
        selected_path: Option<PathBuf>,
    },
    Complete,
    ExternalLinkFailed,
}

#[derive(Clone)]
enum AppHostIoFallback {
    Import {
        operation_id: connector_contract::OperationId,
        source: connector_contract::ImportSource,
    },
    Cancel(connector_contract::OperationId),
    WorkspacePath {
        workspace_id: String,
        operation: ui::workspace::WorkspaceIoOperation,
        generation: u64,
    },
    WorkspaceClipboard {
        workspace_id: String,
        operation: ui::workspace::WorkspaceIoOperation,
        generation: u64,
    },
    FileTree {
        operation: ui::file_tree::FileTreeIoOperation,
        generation: u64,
    },
    FileTreeMaintenance {
        operation: ui::file_tree::FileTreeMaintenanceOperation,
        generation: u64,
    },
    InboxPreview {
        operation: ui::inbox_waiting::LogPreviewOperation,
        generation: u64,
    },
    Diff {
        operation: ui::diff_panel::DiffIoOperation,
        generation: u64,
    },
    ComposerContextFile(ui::composer::ContextFileRequest),
    ComposerClipboard(ui::composer::ClipboardAttachmentRequest),
    ComposerHistory,
    FolderPicker(FolderPickerPurpose),
    None,
}

impl AppHostIoFallback {
    fn for_action(action: &AppHostIoAction) -> Self {
        match action {
            AppHostIoAction::Connector(connector_service::HostAction::RequestImportSource {
                operation_id,
                source,
            }) => Self::Import {
                operation_id: operation_id.clone(),
                source: connector_import_source(*source),
            },
            AppHostIoAction::Connector(
                connector_service::HostAction::OpenOAuthBrowser { operation_id, .. }
                | connector_service::HostAction::OpenSlackRecovery { operation_id, .. },
            ) => Self::Cancel(operation_id.clone()),
            AppHostIoAction::Connector(connector_service::HostAction::OpenExternalLink {
                ..
            }) => Self::None,
            AppHostIoAction::Workspace {
                workspace_id,
                intent:
                    ui::workspace::WorkspaceIoIntent::ResolvePath {
                        operation,
                        generation,
                        ..
                    },
            } => Self::WorkspacePath {
                workspace_id: workspace_id.clone(),
                operation: *operation,
                generation: *generation,
            },
            AppHostIoAction::Workspace {
                workspace_id,
                intent:
                    ui::workspace::WorkspaceIoIntent::ReadTerminalClipboard {
                        operation,
                        generation,
                    },
            } => Self::WorkspaceClipboard {
                workspace_id: workspace_id.clone(),
                operation: *operation,
                generation: *generation,
            },
            AppHostIoAction::Workspace { .. } => Self::None,
            AppHostIoAction::FileTree(intent) => Self::FileTree {
                operation: intent.operation,
                generation: intent.generation,
            },
            AppHostIoAction::FileTreeMaintenance(intent) => Self::FileTreeMaintenance {
                operation: intent.operation,
                generation: intent.generation,
            },
            AppHostIoAction::InboxPreview(intent) => Self::InboxPreview {
                operation: intent.operation,
                generation: intent.generation,
            },
            AppHostIoAction::Diff(intent) => Self::Diff {
                operation: intent.operation,
                generation: intent.generation,
            },
            AppHostIoAction::ComposerContextFile(request) => {
                Self::ComposerContextFile(request.clone())
            }
            AppHostIoAction::ComposerClipboard(request) => Self::ComposerClipboard(request.clone()),
            AppHostIoAction::PersistComposerHistory { .. } => Self::ComposerHistory,
            AppHostIoAction::FolderPicker(purpose) => Self::FolderPicker(purpose.clone()),
            AppHostIoAction::OpenPath(_) => Self::None,
            AppHostIoAction::ExternalHttpsUrl(_) => Self::None,
        }
    }

    fn into_completion(self) -> AppHostIoCompletion {
        match self {
            Self::Import {
                operation_id,
                source,
            } => AppHostIoCompletion::Dispatch(
                connector_contract::ConnectorIntent::FailImportSource {
                    operation_id,
                    source,
                    error_code: connector_contract::ErrorCode::HostUnavailable,
                },
            ),
            Self::Cancel(operation_id) => AppHostIoCompletion::Dispatch(
                connector_contract::ConnectorIntent::Cancel(operation_id),
            ),
            Self::WorkspacePath {
                workspace_id,
                operation,
                generation,
            } => AppHostIoCompletion::Workspace {
                workspace_id,
                completion: ui::workspace::WorkspaceIoCompletion::PathResolved {
                    operation,
                    generation,
                    result: None,
                },
            },
            Self::WorkspaceClipboard {
                workspace_id,
                operation,
                generation,
            } => AppHostIoCompletion::Workspace {
                workspace_id,
                completion: ui::workspace::WorkspaceIoCompletion::TerminalClipboardRead {
                    operation,
                    generation,
                    result: Err(ui::workspace::WorkspaceIoErrorCode::NativeFailure),
                },
            },
            Self::FileTree {
                operation,
                generation,
            } => AppHostIoCompletion::FileTree(ui::file_tree::FileTreeIoCompletion {
                operation,
                generation,
                result: Err(ui::file_tree::FileTreeIoErrorCode::NativeFailure),
            }),
            Self::FileTreeMaintenance {
                operation,
                generation,
            } => AppHostIoCompletion::FileTreeMaintenance(
                ui::file_tree::FileTreeMaintenanceCompletion {
                    operation,
                    generation,
                    result: Err(ui::file_tree::FileTreeMaintenanceErrorCode::NativeFailure),
                },
            ),
            Self::InboxPreview {
                operation,
                generation,
            } => AppHostIoCompletion::InboxPreview(ui::inbox_waiting::LogPreviewCompletion {
                operation,
                generation,
                result: Err(ui::inbox_waiting::LogPreviewErrorCode::NativeFailure),
            }),
            Self::Diff {
                operation,
                generation,
            } => AppHostIoCompletion::Diff(ui::diff_panel::DiffIoCompletion {
                operation,
                generation,
                result: Err(ui::diff_panel::DiffIoErrorCode::CollectionFailed),
            }),
            Self::ComposerContextFile(request) => AppHostIoCompletion::ComposerContextFile {
                request,
                selected_path: None,
            },
            Self::ComposerClipboard(request) => AppHostIoCompletion::ComposerClipboard {
                request,
                payload: None,
            },
            Self::ComposerHistory => AppHostIoCompletion::ComposerHistoryWriteFailed,
            Self::FolderPicker(purpose) => AppHostIoCompletion::FolderPicker {
                purpose,
                selected_path: None,
            },
            Self::None => AppHostIoCompletion::ExternalLinkFailed,
        }
    }
}

#[derive(Default)]
struct PendingFileTreeWatchEvents {
    paths: Vec<PathBuf>,
    overflowed: bool,
}

struct AppFileTreeWatcher {
    watcher: notify::RecommendedWatcher,
    watched: std::collections::HashSet<PathBuf>,
    pending: Arc<std::sync::Mutex<PendingFileTreeWatchEvents>>,
    generation: u64,
    revision: u64,
    root: PathBuf,
    ignored_prefixes: Vec<PathBuf>,
    show_hidden: bool,
}

impl AppFileTreeWatcher {
    fn new(ctx: egui::Context) -> Result<Self, ui::file_tree::FileTreeMaintenanceErrorCode> {
        let pending = Arc::new(std::sync::Mutex::new(PendingFileTreeWatchEvents::default()));
        let callback_pending = Arc::clone(&pending);
        let watcher =
            notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                let mut pending = callback_pending
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                match event {
                    Ok(event) if !matches!(event.kind, notify::EventKind::Access(_)) => {
                        let mut changed = false;
                        for path in event.paths {
                            let bytes = path.as_os_str().as_encoded_bytes().len();
                            if bytes == 0 || bytes > APP_HOST_PATH_MAX_BYTES {
                                pending.overflowed = true;
                                changed = true;
                                continue;
                            }
                            if pending.paths.iter().any(|existing| existing == &path) {
                                continue;
                            }
                            if pending.paths.len() >= ui::file_tree::FILE_TREE_WATCH_MAX_EVENTS {
                                pending.overflowed = true;
                                changed = true;
                                break;
                            }
                            pending.paths.push(path);
                            changed = true;
                        }
                        drop(pending);
                        if changed {
                            ctx.request_repaint();
                        }
                    }
                    Ok(_) => {}
                    Err(_) => {
                        pending.overflowed = true;
                        drop(pending);
                        ctx.request_repaint();
                    }
                }
            })
            .map_err(|_| ui::file_tree::FileTreeMaintenanceErrorCode::WatchUnavailable)?;
        Ok(Self {
            watcher,
            watched: std::collections::HashSet::new(),
            pending,
            generation: 0,
            revision: 0,
            root: PathBuf::new(),
            ignored_prefixes: Vec::new(),
            show_hidden: false,
        })
    }

    fn replace(
        &mut self,
        generation: u64,
        directories: Vec<PathBuf>,
        ignored_prefixes: Vec<PathBuf>,
        show_hidden: bool,
    ) -> Result<(), ui::file_tree::FileTreeMaintenanceErrorCode> {
        use notify::Watcher as _;

        let Some(root) = directories.first().cloned() else {
            return Err(ui::file_tree::FileTreeMaintenanceErrorCode::InvalidSnapshot);
        };
        if directories.len() > ui::file_tree::FILE_TREE_WATCH_MAX_DIRECTORIES
            || directories
                .iter()
                .any(|directory| !directory.starts_with(&root))
        {
            return Err(ui::file_tree::FileTreeMaintenanceErrorCode::WatchPlanTooLarge);
        }
        let desired = directories
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        for directory in desired.difference(&self.watched) {
            self.watcher
                .watch(directory, notify::RecursiveMode::NonRecursive)
                .map_err(|_| ui::file_tree::FileTreeMaintenanceErrorCode::WatchUnavailable)?;
        }
        for directory in self.watched.difference(&desired) {
            let _ = self.watcher.unwatch(directory);
        }
        self.watched = desired;
        self.generation = generation;
        self.root = root;
        self.ignored_prefixes = ignored_prefixes;
        self.show_hidden = show_hidden;
        *self
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = PendingFileTreeWatchEvents::default();
        Ok(())
    }

    fn take_snapshot(
        &mut self,
    ) -> Result<
        Option<ui::file_tree::FileTreeWatchSnapshot>,
        ui::file_tree::FileTreeMaintenanceErrorCode,
    > {
        let PendingFileTreeWatchEvents {
            paths,
            mut overflowed,
        } = std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        );
        if paths.is_empty() && !overflowed {
            return Ok(None);
        }
        let mut events = Vec::with_capacity(ui::file_tree::FILE_TREE_WATCH_MAX_EVENTS);
        for path in paths {
            if !path.starts_with(&self.root)
                || self
                    .ignored_prefixes
                    .iter()
                    .any(|prefix| path.starts_with(prefix))
            {
                continue;
            }
            let env_file = app_file_tree_env_candidate(&path);
            if !self.show_hidden && app_file_tree_hidden_component(&self.root, &path) && !env_file {
                continue;
            }
            if env_file {
                if events.len() >= ui::file_tree::FILE_TREE_WATCH_MAX_EVENTS {
                    overflowed = true;
                    break;
                }
                events.push(ui::file_tree::FileTreeWatchEvent::try_new(
                    ui::file_tree::FileTreeWatchEventKind::EnvFileChanged,
                    path.clone(),
                )?);
            }
            if let Some(parent) = path.parent() {
                if events.len() >= ui::file_tree::FILE_TREE_WATCH_MAX_EVENTS {
                    overflowed = true;
                    break;
                }
                events.push(ui::file_tree::FileTreeWatchEvent::try_new(
                    ui::file_tree::FileTreeWatchEventKind::DirtyDirectory,
                    parent.to_path_buf(),
                )?);
            }
        }
        self.revision = self.revision.wrapping_add(1).max(1);
        ui::file_tree::FileTreeWatchSnapshot::try_new(
            self.generation,
            self.revision,
            overflowed,
            events,
        )
        .map(Some)
    }
}

fn app_file_tree_env_candidate(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name == ".env"
                || name == ".envrc"
                || name.starts_with(".env.")
                || name.starts_with(".env-")
        })
}

fn app_file_tree_hidden_component(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|relative| {
        relative.components().any(|component| {
            matches!(component, std::path::Component::Normal(name) if name.to_string_lossy().starts_with('.'))
        })
    })
}

struct AppHostIoTask {
    result_rx: std::sync::mpsc::Receiver<AppHostIoCompletion>,
    handle: std::thread::JoinHandle<()>,
    fallback: AppHostIoFallback,
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

impl AppHostIoTask {
    fn spawn(action: AppHostIoAction, ctx: egui::Context) -> Result<Self, AppHostIoFallback> {
        let fallback = AppHostIoFallback::for_action(&action);
        let fallback_on_spawn = fallback.clone();
        let fallback_on_panic = fallback.clone();
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let handle = std::thread::Builder::new()
            .name("app-host-io".to_owned())
            .spawn(move || {
                let completion = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_app_host_io(action, worker_cancel.as_ref())
                }))
                .unwrap_or_else(|_| fallback_on_panic.into_completion());
                if result_tx.send(completion).is_ok() {
                    ctx.request_repaint();
                }
            })
            .map_err(|_| fallback_on_spawn)?;
        Ok(Self {
            result_rx,
            handle,
            fallback,
            cancel,
        })
    }
}

fn connector_import_source(
    source: connector_contract::ImportSourceRequest,
) -> connector_contract::ImportSource {
    match source {
        connector_contract::ImportSourceRequest::FilePicker => {
            connector_contract::ImportSource::File
        }
        connector_contract::ImportSourceRequest::ClaudeDesktop => {
            connector_contract::ImportSource::ClaudeDesktop
        }
    }
}

fn workspace_name_for_path(path: &Path, style: crate::config::SessionNameStyle) -> String {
    let path_text = path.to_string_lossy();
    crate::agent_detect::project_display_name(&path_text, style)
        .or_else(|| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "workspace".to_owned())
}

fn write_composer_history(path: &Path, history: &[Arc<str>]) -> bool {
    if history.len() > ui::composer::COMPOSER_HISTORY_MAX_ITEMS
        || history.iter().map(|entry| entry.len()).sum::<usize>()
            > ui::composer::COMPOSER_HISTORY_MAX_BYTES
    {
        return false;
    }
    let mut bytes = Vec::with_capacity(ui::composer::COMPOSER_HISTORY_MAX_BYTES.min(64 * 1024));
    for entry in history {
        if serde_json::to_writer(&mut bytes, entry).is_err() {
            return false;
        }
        bytes.push(b'\n');
        if bytes.len() > ui::composer::COMPOSER_HISTORY_FILE_MAX_BYTES {
            return false;
        }
    }
    let mut temp_name = path.as_os_str().to_owned();
    temp_name.push(".tmp");
    let temp_path = PathBuf::from(temp_name);
    if std::fs::write(&temp_path, bytes).is_err() {
        return false;
    }
    if std::fs::rename(&temp_path, path).is_err() {
        let _ = std::fs::remove_file(temp_path);
        return false;
    }
    true
}

fn read_composer_clipboard_paths() -> Option<Vec<PathBuf>> {
    let mut result = ui::clipboard_image::paste_clipboard_paths_or_image_to_paths();
    let mut retries = 0;
    while retries < 4
        && matches!(&result, Ok(None))
        && ui::clipboard_image::read_clipboard_text().is_none()
    {
        std::thread::sleep(std::time::Duration::from_millis(150));
        result = ui::clipboard_image::paste_clipboard_paths_or_image_to_paths();
        retries += 1;
    }
    result.ok().flatten().filter(|paths| {
        paths.len() <= ui::composer::COMPOSER_ATTACHMENT_MAX_ITEMS
            && paths
                .iter()
                .all(|path| path.as_os_str().as_encoded_bytes().len() <= APP_HOST_PATH_MAX_BYTES)
            && paths
                .iter()
                .map(|path| path.as_os_str().as_encoded_bytes().len())
                .sum::<usize>()
                <= ui::composer::COMPOSER_ATTACHMENT_MAX_BYTES
    })
}

const APP_HOST_OPENABLE_EXTS: &[&str] = &[
    "pdf", "html", "htm", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "csv", "hwp", "txt", "md",
    "rtf", "png", "jpg", "jpeg", "gif", "webp", "svg", "heic", "tiff", "mp4", "mov", "mp3", "wav",
    "zip", "numbers", "pages", "key",
];

fn app_host_openable_file(path: &Path) -> bool {
    let allowed_extension = |candidate: &Path| {
        candidate
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                APP_HOST_OPENABLE_EXTS.contains(&extension.to_ascii_lowercase().as_str())
            })
    };
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
        && allowed_extension(path)
        && std::fs::canonicalize(path).is_ok_and(|real| allowed_extension(&real))
}

fn resolve_workspace_host_path(
    word: &str,
    cwd: Option<&Path>,
) -> Option<ui::workspace::WorkspacePathResolution> {
    if word.is_empty() || word.len() > APP_HOST_PATH_MAX_BYTES || word.as_bytes().contains(&0) {
        return None;
    }
    let token = word
        .trim_start_matches(|character: char| "\"'`([{<".contains(character))
        .trim_end_matches(|character: char| "\"'`.,;!?)]}>".contains(character));
    if token.is_empty() {
        return None;
    }
    let mut candidates = vec![token];
    let mut head = token;
    while let Some((rest, tail)) = head.rsplit_once(':')
        && !rest.is_empty()
        && !tail.is_empty()
        && tail.chars().all(|character| character.is_ascii_digit())
    {
        candidates.push(rest);
        head = rest;
    }
    for candidate in candidates {
        let path = if let Some(rest) = candidate.strip_prefix("~/") {
            crate::paths::home_dir().map(|home| home.join(rest))
        } else if candidate.starts_with('/') {
            Some(PathBuf::from(candidate))
        } else {
            cwd.map(|root| root.join(candidate))
        }?;
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        let kind = if metadata.is_dir() {
            ui::workspace::WorkspacePathKind::Directory
        } else if app_host_openable_file(&path) {
            ui::workspace::WorkspacePathKind::OpenableFile
        } else {
            continue;
        };
        let path = ui::workspace::WorkspacePathPayload::try_new(path).ok()?;
        return Some(ui::workspace::WorkspacePathResolution { kind, path });
    }
    None
}

fn app_host_open_path_reaped(path: &Path) -> bool {
    #[cfg(target_os = "macos")]
    const OPENER: &str = "open";
    #[cfg(not(target_os = "macos"))]
    const OPENER: &str = "xdg-open";
    std::process::Command::new(OPENER)
        .arg(path)
        .status()
        .is_ok_and(|status| status.success())
}

fn app_host_valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && name == name.trim()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', '\0'])
        && name.len() <= APP_HOST_PATH_MAX_BYTES
}

fn app_host_rename_no_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let to_c_string = |path: &Path| {
            std::ffi::CString::new(path.as_os_str().as_bytes())
                .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
        };
        let source = to_c_string(source)?;
        let destination = to_c_string(destination)?;
        let result =
            unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ENOTSUP) {
            return Err(error);
        }
    }
    if std::fs::symlink_metadata(destination).is_ok() {
        return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
    }
    std::fs::rename(source, destination)
}

fn app_host_remove_all(path: &Path) -> std::io::Result<()> {
    let file_type = std::fs::symlink_metadata(path)?.file_type();
    if file_type.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

#[derive(Default)]
struct AppHostFileOperationBudget {
    items: usize,
    bytes: u64,
}

impl AppHostFileOperationBudget {
    fn consume_item(&mut self, depth: usize) -> std::io::Result<()> {
        if depth > APP_HOST_FILE_OPERATION_MAX_DEPTH {
            return Err(std::io::Error::other("file_operation_depth_limit"));
        }
        self.items = self
            .items
            .checked_add(1)
            .filter(|items| *items <= APP_HOST_FILE_OPERATION_MAX_ITEMS)
            .ok_or_else(|| std::io::Error::other("file_operation_item_limit"))?;
        Ok(())
    }

    fn consume_bytes(&mut self, bytes: u64) -> std::io::Result<()> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .filter(|bytes| *bytes <= APP_HOST_FILE_OPERATION_MAX_BYTES)
            .ok_or_else(|| std::io::Error::other("file_operation_byte_limit"))?;
        Ok(())
    }
}

fn app_host_file_operation_cancelled(
    cancel: &std::sync::atomic::AtomicBool,
) -> std::io::Result<()> {
    if cancel.load(std::sync::atomic::Ordering::Acquire) {
        Err(std::io::Error::from(std::io::ErrorKind::Interrupted))
    } else {
        Ok(())
    }
}

fn app_host_validate_tree(
    source: &Path,
    budget: &mut AppHostFileOperationBudget,
    cancel: &std::sync::atomic::AtomicBool,
    depth: usize,
) -> std::io::Result<()> {
    app_host_file_operation_cancelled(cancel)?;
    budget.consume_item(depth)?;
    let file_type = std::fs::symlink_metadata(source)?.file_type();
    if file_type.is_symlink() {
        let target = std::fs::read_link(source)?;
        return budget.consume_bytes(target.as_os_str().as_encoded_bytes().len() as u64);
    }
    if file_type.is_dir() {
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            app_host_validate_tree(&entry.path(), budget, cancel, depth.saturating_add(1))?;
        }
        return Ok(());
    }
    if !file_type.is_file() {
        return Err(std::io::Error::other("unsupported_file_type"));
    }
    budget.consume_bytes(std::fs::symlink_metadata(source)?.len())
}

fn app_host_copy_recursive(
    source: &Path,
    destination: &Path,
    budget: &mut AppHostFileOperationBudget,
    cancel: &std::sync::atomic::AtomicBool,
    depth: usize,
) -> std::io::Result<()> {
    use std::io::{Read as _, Write as _};

    app_host_file_operation_cancelled(cancel)?;
    budget.consume_item(depth)?;
    let metadata = std::fs::symlink_metadata(source)?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        let target = std::fs::read_link(source)?;
        budget.consume_bytes(target.as_os_str().as_encoded_bytes().len() as u64)?;
        #[cfg(unix)]
        return std::os::unix::fs::symlink(target, destination);
        #[cfg(not(unix))]
        {
            let _ = target;
            return Err(std::io::Error::other("symlink_copy_unsupported"));
        }
    }
    if file_type.is_dir() {
        std::fs::create_dir(destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            app_host_copy_recursive(
                &entry.path(),
                &destination.join(entry.file_name()),
                budget,
                cancel,
                depth.saturating_add(1),
            )?;
        }
        std::fs::set_permissions(destination, metadata.permissions())?;
        return Ok(());
    }
    if !file_type.is_file() {
        return Err(std::io::Error::other("unsupported_file_type"));
    }
    let mut input = std::fs::File::open(source)?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let mut buffer = vec![0_u8; APP_HOST_FILE_COPY_BUFFER_BYTES];
    loop {
        app_host_file_operation_cancelled(cancel)?;
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        budget.consume_bytes(read as u64)?;
        output.write_all(&buffer[..read])?;
    }
    output.flush()?;
    std::fs::set_permissions(destination, metadata.permissions())
}

fn app_host_copy_into(
    source: &Path,
    destination_dir: &Path,
    budget: &mut AppHostFileOperationBudget,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<(), ()> {
    let name = source.file_name().ok_or(())?;
    if let Ok(source_real) = std::fs::canonicalize(source)
        && destination_dir.starts_with(source_real)
    {
        return Err(());
    }
    let destination = destination_dir.join(name);
    let temporary = destination_dir.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
    if app_host_copy_recursive(source, &temporary, budget, cancel, 0).is_err() {
        let _ = app_host_remove_all(&temporary);
        return Err(());
    }
    if app_host_file_operation_cancelled(cancel).is_err() {
        let _ = app_host_remove_all(&temporary);
        return Err(());
    }
    if app_host_rename_no_replace(&temporary, &destination).is_err() {
        let _ = app_host_remove_all(&temporary);
        return Err(());
    }
    Ok(())
}

fn app_host_move(
    root: &Path,
    source: &Path,
    destination_dir: &Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<(), ui::file_tree::FileTreeIoErrorCode> {
    let root =
        std::fs::canonicalize(root).map_err(|_| ui::file_tree::FileTreeIoErrorCode::OutsideRoot)?;
    let destination_dir = std::fs::canonicalize(destination_dir)
        .map_err(|_| ui::file_tree::FileTreeIoErrorCode::OutsideRoot)?;
    let name = source
        .file_name()
        .ok_or(ui::file_tree::FileTreeIoErrorCode::InvalidPath)?;
    let source_parent = source
        .parent()
        .and_then(|parent| std::fs::canonicalize(parent).ok())
        .ok_or(ui::file_tree::FileTreeIoErrorCode::InvalidPath)?;
    let source = source_parent.join(name);
    if !source.starts_with(&root)
        || !destination_dir.starts_with(&root)
        || destination_dir.starts_with(&source)
    {
        return Err(ui::file_tree::FileTreeIoErrorCode::OutsideRoot);
    }
    if source_parent == destination_dir {
        return Ok(());
    }
    let destination = destination_dir.join(name);
    app_host_file_operation_cancelled(cancel)
        .map_err(|_| ui::file_tree::FileTreeIoErrorCode::NativeFailure)?;
    match app_host_rename_no_replace(&source, &destination) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {
            let mut validation = AppHostFileOperationBudget::default();
            app_host_validate_tree(&source, &mut validation, cancel, 0)
                .map_err(|_| ui::file_tree::FileTreeIoErrorCode::NativeFailure)?;
            let temporary = destination_dir.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
            let mut copy_budget = AppHostFileOperationBudget::default();
            if app_host_copy_recursive(&source, &temporary, &mut copy_budget, cancel, 0).is_err() {
                let _ = app_host_remove_all(&temporary);
                return Err(ui::file_tree::FileTreeIoErrorCode::NativeFailure);
            }
            if app_host_file_operation_cancelled(cancel).is_err() {
                let _ = app_host_remove_all(&temporary);
                return Err(ui::file_tree::FileTreeIoErrorCode::NativeFailure);
            }
            if let Err(error) = app_host_rename_no_replace(&temporary, &destination) {
                let _ = app_host_remove_all(&temporary);
                return Err(if error.kind() == std::io::ErrorKind::AlreadyExists {
                    ui::file_tree::FileTreeIoErrorCode::Conflict
                } else {
                    ui::file_tree::FileTreeIoErrorCode::NativeFailure
                });
            }
            app_host_remove_all(&source)
                .map_err(|_| ui::file_tree::FileTreeIoErrorCode::NativeFailure)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(ui::file_tree::FileTreeIoErrorCode::Conflict)
        }
        Err(_) => Err(ui::file_tree::FileTreeIoErrorCode::NativeFailure),
    }
}

fn run_file_tree_host_io(
    request: ui::file_tree::FileTreeIoRequest,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<(), ui::file_tree::FileTreeIoErrorCode> {
    use ui::file_tree::{FileTreeIoErrorCode as Error, FileTreeIoRequest as Request};
    match request {
        Request::Rename { source, name } => {
            if !app_host_valid_file_name(&name) {
                return Err(Error::InvalidName);
            }
            let source = source.into_path();
            let destination = source.parent().ok_or(Error::InvalidPath)?.join(name);
            if destination == source {
                return Ok(());
            }
            app_host_rename_no_replace(&source, &destination).map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    Error::Conflict
                } else {
                    Error::NativeFailure
                }
            })
        }
        Request::CreateDirectory { parent, name } => {
            if !app_host_valid_file_name(&name) {
                return Err(Error::InvalidName);
            }
            std::fs::create_dir(parent.into_path().join(name)).map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    Error::Conflict
                } else {
                    Error::NativeFailure
                }
            })
        }
        Request::CreateFile { parent, name } => {
            if !app_host_valid_file_name(&name) {
                return Err(Error::InvalidName);
            }
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(parent.into_path().join(name))
                .map(|_| ())
                .map_err(|error| {
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        Error::Conflict
                    } else {
                        Error::NativeFailure
                    }
                })
        }
        Request::Move {
            root,
            source,
            destination,
        } => app_host_move(
            root.as_path(),
            source.as_path(),
            destination.as_path(),
            cancel,
        ),
        Request::CopyInto {
            sources,
            destination,
        } => {
            let destination =
                std::fs::canonicalize(destination.into_path()).map_err(|_| Error::InvalidPath)?;
            let sources = sources.into_paths();
            let mut validation = AppHostFileOperationBudget::default();
            for source in &sources {
                app_host_validate_tree(source, &mut validation, cancel, 0)
                    .map_err(|_| Error::NativeFailure)?;
            }
            let mut copy_budget = AppHostFileOperationBudget::default();
            for source in sources {
                app_host_copy_into(&source, &destination, &mut copy_budget, cancel)
                    .map_err(|_| Error::NativeFailure)?;
            }
            Ok(())
        }
        Request::PasteFromClipboard { destination } => {
            let Some(sources) = ui::clipboard_image::read_clipboard_file_list() else {
                return Ok(());
            };
            let destination =
                std::fs::canonicalize(destination.into_path()).map_err(|_| Error::InvalidPath)?;
            let mut validation = AppHostFileOperationBudget::default();
            for source in &sources {
                app_host_validate_tree(source, &mut validation, cancel, 0)
                    .map_err(|_| Error::NativeFailure)?;
            }
            let mut copy_budget = AppHostFileOperationBudget::default();
            for source in sources {
                app_host_copy_into(&source, &destination, &mut copy_budget, cancel)
                    .map_err(|_| Error::NativeFailure)?;
            }
            Ok(())
        }
        Request::Trash { target } => {
            let mut validation = AppHostFileOperationBudget::default();
            app_host_validate_tree(target.as_path(), &mut validation, cancel, 0)
                .map_err(|_| Error::TrashUnavailable)?;
            app_host_file_operation_cancelled(cancel).map_err(|_| Error::TrashUnavailable)?;
            trash::delete(target.as_path()).map_err(|_| Error::TrashUnavailable)
        }
        Request::DeletePermanently { target } => {
            let mut validation = AppHostFileOperationBudget::default();
            app_host_validate_tree(target.as_path(), &mut validation, cancel, 0)
                .map_err(|_| Error::NativeFailure)?;
            app_host_file_operation_cancelled(cancel).map_err(|_| Error::NativeFailure)?;
            app_host_remove_all(target.as_path()).map_err(|_| Error::NativeFailure)
        }
        Request::CopyFileUrls { paths } => {
            ui::clipboard_image::copy_file_urls_to_clipboard(&paths.into_paths())
                .map_err(|_| Error::NativeFailure)
        }
        Request::OpenPath {
            target,
            require_openable_file,
        } => {
            if require_openable_file && !app_host_openable_file(target.as_path()) {
                return Err(Error::InvalidPath);
            }
            app_host_open_path_reaped(target.as_path())
                .then_some(())
                .ok_or(Error::NativeFailure)
        }
    }
}

fn read_inbox_log_preview(
    source: &ui::inbox_waiting::LogPreviewSource,
) -> Result<Option<ui::inbox_waiting::LogPreviewSnapshot>, ui::inbox_waiting::LogPreviewErrorCode> {
    use std::io::{Read as _, Seek as _};

    let mut file = std::fs::File::open(source.as_path())
        .map_err(|_| ui::inbox_waiting::LogPreviewErrorCode::NativeFailure)?;
    let length = file
        .metadata()
        .map_err(|_| ui::inbox_waiting::LogPreviewErrorCode::NativeFailure)?
        .len();
    let start = length.saturating_sub(ui::inbox_waiting::LOG_PREVIEW_TAIL_BYTES);
    file.seek(std::io::SeekFrom::Start(start))
        .map_err(|_| ui::inbox_waiting::LogPreviewErrorCode::NativeFailure)?;
    let mut bytes = Vec::with_capacity(
        usize::try_from(length.saturating_sub(start))
            .unwrap_or(ui::inbox_waiting::LOG_PREVIEW_TAIL_BYTES as usize),
    );
    file.take(ui::inbox_waiting::LOG_PREVIEW_TAIL_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|_| ui::inbox_waiting::LogPreviewErrorCode::NativeFailure)?;
    ui::inbox_waiting::LogPreviewSnapshot::try_from_tail_bytes(&bytes, start > 0)
}

fn run_file_tree_listing(
    root: &Path,
    directory: &Path,
    max_items: usize,
    max_bytes: usize,
) -> Result<ui::file_tree::FileTreeListingSnapshot, ui::file_tree::FileTreeMaintenanceErrorCode> {
    use ui::file_tree::FileTreeMaintenanceErrorCode as Error;

    if max_items == 0
        || max_items > ui::file_tree::FILE_TREE_LISTING_MAX_ITEMS
        || max_bytes == 0
        || max_bytes > ui::file_tree::FILE_TREE_LISTING_MAX_BYTES
    {
        return Err(Error::ListingTooLarge);
    }
    let canonical_root = std::fs::canonicalize(root).map_err(|error| {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            Error::PermissionDenied
        } else {
            Error::NativeFailure
        }
    })?;
    let canonical_directory = std::fs::canonicalize(directory).map_err(|error| {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            Error::PermissionDenied
        } else {
            Error::NativeFailure
        }
    })?;
    if !canonical_directory.starts_with(&canonical_root) || !canonical_directory.is_dir() {
        return Err(Error::InvalidSnapshot);
    }
    let entries = std::fs::read_dir(&canonical_directory).map_err(|error| {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            Error::PermissionDenied
        } else {
            Error::NativeFailure
        }
    })?;
    let mut items = Vec::with_capacity(max_items.min(256));
    let mut bytes = 0usize;
    for entry in entries {
        let entry = entry.map_err(|error| {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                Error::PermissionDenied
            } else {
                Error::NativeFailure
            }
        })?;
        if items.len() >= max_items {
            return Err(Error::ListingTooLarge);
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        bytes = bytes
            .checked_add(name.len())
            .filter(|bytes| *bytes <= max_bytes)
            .ok_or(Error::ListingTooLarge)?;
        let is_dir = entry
            .file_type()
            .map_err(|_| Error::NativeFailure)?
            .is_dir();
        items.push(ui::file_tree::FileTreeListingItem::try_new(name, is_dir)?);
    }
    ui::file_tree::FileTreeListingSnapshot::try_new(items)
}

fn run_app_host_io(
    action: AppHostIoAction,
    cancel: &std::sync::atomic::AtomicBool,
) -> AppHostIoCompletion {
    match action {
        AppHostIoAction::Workspace {
            workspace_id,
            intent:
                ui::workspace::WorkspaceIoIntent::ResolvePath {
                    operation,
                    generation,
                    pid,
                    cwd,
                    word,
                    ..
                },
        } => {
            let cwd = cwd
                .map(ui::workspace::WorkspacePathPayload::into_path)
                .or_else(|| pid.and_then(platform::process_cwd));
            AppHostIoCompletion::Workspace {
                workspace_id,
                completion: ui::workspace::WorkspaceIoCompletion::PathResolved {
                    operation,
                    generation,
                    result: resolve_workspace_host_path(&word, cwd.as_deref()),
                },
            }
        }
        AppHostIoAction::Workspace {
            workspace_id,
            intent:
                ui::workspace::WorkspaceIoIntent::ReadTerminalClipboard {
                    operation,
                    generation,
                },
        } => {
            let paths = ui::clipboard_image::paste_clipboard_paths_or_image_to_paths()
                .ok()
                .flatten()
                .unwrap_or_default();
            let text = ui::clipboard_image::read_clipboard_text();
            AppHostIoCompletion::Workspace {
                workspace_id,
                completion: ui::workspace::WorkspaceIoCompletion::TerminalClipboardRead {
                    operation,
                    generation,
                    result: ui::workspace::TerminalClipboardPayload::try_new(paths, text),
                },
            }
        }
        AppHostIoAction::Workspace {
            intent: ui::workspace::WorkspaceIoIntent::OpenPath(path),
            ..
        } => {
            if app_host_open_path_reaped(path.as_path()) {
                AppHostIoCompletion::Complete
            } else {
                AppHostIoCompletion::ExternalLinkFailed
            }
        }
        AppHostIoAction::Workspace {
            intent: ui::workspace::WorkspaceIoIntent::OpenUrl(url),
            ..
        } => {
            if auth::open_in_browser_reaped(url.as_str()).is_ok() {
                AppHostIoCompletion::Complete
            } else {
                AppHostIoCompletion::ExternalLinkFailed
            }
        }
        AppHostIoAction::FileTree(intent) => {
            let operation = intent.operation;
            let generation = intent.generation;
            AppHostIoCompletion::FileTree(ui::file_tree::FileTreeIoCompletion {
                operation,
                generation,
                result: run_file_tree_host_io(intent.request, cancel),
            })
        }
        AppHostIoAction::FileTreeMaintenance(intent) => {
            let operation = intent.operation;
            let generation = intent.generation;
            let result = match intent.request {
                ui::file_tree::FileTreeMaintenanceRequest::ListDirectory {
                    root,
                    directory,
                    max_items,
                    max_bytes,
                } => {
                    run_file_tree_listing(root.as_path(), directory.as_path(), max_items, max_bytes)
                        .map(ui::file_tree::FileTreeMaintenanceResult::Listing)
                }
                ui::file_tree::FileTreeMaintenanceRequest::ReplaceWatchSet(_) => {
                    Err(ui::file_tree::FileTreeMaintenanceErrorCode::NativeFailure)
                }
            };
            AppHostIoCompletion::FileTreeMaintenance(ui::file_tree::FileTreeMaintenanceCompletion {
                operation,
                generation,
                result,
            })
        }
        AppHostIoAction::InboxPreview(intent) => {
            let operation = intent.operation;
            let generation = intent.generation;
            AppHostIoCompletion::InboxPreview(ui::inbox_waiting::LogPreviewCompletion {
                operation,
                generation,
                result: read_inbox_log_preview(&intent.source),
            })
        }
        AppHostIoAction::Diff(intent) => {
            AppHostIoCompletion::Diff(ui::diff_panel::execute_io(intent))
        }
        AppHostIoAction::PersistComposerHistory { path, history } => {
            if write_composer_history(&path, &history) {
                AppHostIoCompletion::Complete
            } else {
                AppHostIoCompletion::ComposerHistoryWriteFailed
            }
        }
        AppHostIoAction::ComposerClipboard(request) => AppHostIoCompletion::ComposerClipboard {
            request,
            payload: read_composer_clipboard_paths()
                .and_then(|paths| ui::composer::ClipboardAttachmentPayload::try_new(paths).ok()),
        },
        AppHostIoAction::OpenPath(path) => {
            if path.as_os_str().as_encoded_bytes().len() <= APP_HOST_PATH_MAX_BYTES
                && app_host_open_path_reaped(&path)
            {
                AppHostIoCompletion::Complete
            } else {
                AppHostIoCompletion::ExternalLinkFailed
            }
        }
        AppHostIoAction::ExternalHttpsUrl(url) => {
            if is_bounded_https_url(&url) && auth::open_in_browser_reaped(&url).is_ok() {
                AppHostIoCompletion::Complete
            } else {
                AppHostIoCompletion::ExternalLinkFailed
            }
        }
        AppHostIoAction::FolderPicker(purpose) => AppHostIoCompletion::FolderPicker {
            purpose,
            selected_path: rfd::FileDialog::new().pick_folder(),
        },
        AppHostIoAction::ComposerContextFile(request) => {
            let mut dialog = rfd::FileDialog::new();
            if let Some(root) = request.workspace_root() {
                dialog = dialog.set_directory(root);
            }
            AppHostIoCompletion::ComposerContextFile {
                request,
                selected_path: dialog.pick_file(),
            }
        }
        AppHostIoAction::Connector(connector_service::HostAction::RequestImportSource {
            operation_id,
            source,
        }) => {
            let path = match source {
                connector_contract::ImportSourceRequest::FilePicker => {
                    rfd::FileDialog::new().pick_file()
                }
                connector_contract::ImportSourceRequest::ClaudeDesktop => {
                    directories::BaseDirs::new().map(|dirs| {
                        dirs.config_dir()
                            .join("Claude")
                            .join("claude_desktop_config.json")
                    })
                }
            };
            let Some(path) = path else {
                return AppHostIoCompletion::Dispatch(connector_contract::ConnectorIntent::Cancel(
                    operation_id,
                ));
            };
            let source = connector_import_source(source);
            let display_name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned());
            let intent = match read_connector_import_file(&path) {
                Ok(contents) => connector_contract::ConnectorIntent::CompleteImportSource {
                    operation_id,
                    source,
                    display_name,
                    contents: connector_contract::SensitiveInput::new(contents),
                },
                Err(error_code) => connector_contract::ConnectorIntent::FailImportSource {
                    operation_id,
                    source,
                    error_code,
                },
            };
            AppHostIoCompletion::Dispatch(intent)
        }
        AppHostIoAction::Connector(connector_service::HostAction::OpenExternalLink {
            kind,
            ..
        }) => {
            let url = match kind {
                connector_contract::ExternalLinkKind::SlackAppSettings => {
                    "https://api.slack.com/apps"
                }
            };
            if auth::open_in_browser_reaped(url).is_ok() {
                AppHostIoCompletion::Complete
            } else {
                AppHostIoCompletion::ExternalLinkFailed
            }
        }
        AppHostIoAction::Connector(connector_service::HostAction::OpenOAuthBrowser {
            operation_id,
            url,
            ..
        }) => {
            if open_connector_sensitive_url(&url) {
                AppHostIoCompletion::Complete
            } else {
                AppHostIoCompletion::Dispatch(connector_contract::ConnectorIntent::Cancel(
                    operation_id,
                ))
            }
        }
        AppHostIoAction::Connector(connector_service::HostAction::OpenSlackRecovery {
            operation_id,
            kind,
            url,
            ..
        }) => {
            let opened = match url.as_ref() {
                Some(url) => open_connector_sensitive_url(url),
                None if matches!(kind, connector_contract::SlackRecoveryKind::ConfigureApp) => {
                    auth::open_in_browser_reaped("https://api.slack.com/apps").is_ok()
                }
                None => false,
            };
            if opened {
                AppHostIoCompletion::Complete
            } else {
                AppHostIoCompletion::Dispatch(connector_contract::ConnectorIntent::Cancel(
                    operation_id,
                ))
            }
        }
    }
}

const AGENT_HOOK_QUERY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

fn agent_hook_query_due(session_count: usize, elapsed: std::time::Duration) -> bool {
    session_count > 0 && elapsed >= AGENT_HOOK_QUERY_INTERVAL
}

fn should_process_agent_bindings(
    has_new_bindings: bool,
    restore_loaded_for: Option<&str>,
    active_workspace_id: &str,
    bounded_refresh_due: bool,
) -> bool {
    has_new_bindings || (bounded_refresh_due && restore_loaded_for != Some(active_workspace_id))
}

fn storage_structured_mutation(
    mutation: ui::agent_sessions::AgentSessionPersistenceMutation,
) -> storage::StructuredThreadMutation {
    use ui::agent_sessions::AgentSessionPersistenceMutation as Mutation;
    match mutation {
        Mutation::Upsert {
            local_session_id,
            workspace_id,
            thread_id,
            title,
            cwd,
            model,
            favorite,
            archived,
        } => storage::StructuredThreadMutation::Upsert(storage::StructuredThreadRow {
            local_session_id,
            workspace_id,
            thread_id,
            title,
            cwd,
            model,
            favorite,
            archived,
            created_at: 0,
            updated_at: 0,
        }),
        Mutation::SetArchived {
            local_session_id,
            archived,
        } => storage::StructuredThreadMutation::SetArchived {
            local_session_id,
            archived,
        },
        Mutation::Delete { local_session_id } => {
            storage::StructuredThreadMutation::Delete { local_session_id }
        }
    }
}

impl App {
    fn apply_app_host_completion(&mut self, completion: AppHostIoCompletion) {
        match completion {
            AppHostIoCompletion::Dispatch(intent) => {
                if self.connector_coordinator.dispatch(intent).is_err() {
                    tracing::warn!("Connector host completion dispatch failed");
                }
            }
            AppHostIoCompletion::Workspace {
                workspace_id,
                completion,
            } => {
                let runtime = if self.active.id == workspace_id {
                    Some(&mut self.active)
                } else {
                    self.warm.get_mut(&workspace_id)
                };
                if let Some(runtime) = runtime {
                    runtime.workspace_ui.complete_io(completion);
                }
            }
            AppHostIoCompletion::FileTree(completion) => {
                if let Some(tree) = self.file_tree.as_mut() {
                    tree.complete_io(completion);
                }
            }
            AppHostIoCompletion::FileTreeMaintenance(completion) => {
                if let Some(tree) = self.file_tree.as_mut() {
                    tree.complete_maintenance(completion);
                    self.egui_ctx.request_repaint();
                }
            }
            AppHostIoCompletion::InboxPreview(completion) => {
                if self.inbox_waiting_ui.complete_preview(completion) {
                    self.egui_ctx.request_repaint();
                }
            }
            AppHostIoCompletion::Diff(completion) => {
                self.diff_panel_ui.complete_io(completion);
                self.egui_ctx.request_repaint();
            }
            AppHostIoCompletion::ComposerContextFile {
                request,
                selected_path,
            } => {
                let active_workspace = self.active.id.clone();
                let _ = self.composer.complete_context_file(
                    &self.egui_ctx,
                    request,
                    selected_path,
                    &active_workspace,
                );
            }
            AppHostIoCompletion::ComposerClipboard { request, payload } => {
                let active_workspace = self.active.id.clone();
                let _ = self.composer.complete_clipboard_attachment(
                    &self.egui_ctx,
                    request,
                    payload,
                    &active_workspace,
                );
            }
            AppHostIoCompletion::ComposerHistoryWriteFailed => {
                tracing::warn!(
                    kind = "composer_history",
                    phase = "write",
                    error_code = "history_write_failed",
                    "composer history persistence failed"
                );
            }
            AppHostIoCompletion::FolderPicker {
                purpose,
                selected_path,
            } => {
                if let Some(path) = selected_path.filter(|path| {
                    path.as_os_str().as_encoded_bytes().len() <= APP_HOST_PATH_MAX_BYTES
                }) {
                    self.pending_folder_picker_completion = Some((purpose, path));
                }
            }
            AppHostIoCompletion::Complete => {}
            AppHostIoCompletion::ExternalLinkFailed => {
                tracing::warn!("Connector external link open failed");
            }
        }
    }

    fn poll_file_tree_maintenance(&mut self, ctx: &egui::Context) {
        let Some(tree) = self.file_tree.as_mut() else {
            self.pending_file_tree_maintenance = None;
            self.file_tree_watcher = None;
            return;
        };
        if let Some(watcher) = self.file_tree_watcher.as_mut() {
            match watcher.take_snapshot() {
                Ok(Some(snapshot)) => {
                    tree.apply_watch_snapshot(snapshot);
                    ctx.request_repaint();
                }
                Ok(None) => {}
                Err(_) => {
                    self.file_tree_watcher = None;
                    tracing::warn!(
                        kind = "file_tree",
                        phase = "watch",
                        error_code = "snapshot_invalid",
                        "file tree watch snapshot invalid"
                    );
                }
            }
        }
        if self.pending_file_tree_maintenance.is_none() {
            self.pending_file_tree_maintenance = tree.take_maintenance_intent();
        }
        let is_watch_plan = self
            .pending_file_tree_maintenance
            .as_ref()
            .is_some_and(|intent| {
                matches!(
                    intent.request,
                    ui::file_tree::FileTreeMaintenanceRequest::ReplaceWatchSet(_)
                )
            });
        if !is_watch_plan {
            return;
        }
        let intent = self
            .pending_file_tree_maintenance
            .take()
            .expect("watch plan checked");
        let operation = intent.operation;
        let generation = intent.generation;
        let ui::file_tree::FileTreeMaintenanceRequest::ReplaceWatchSet(plan) = intent.request
        else {
            unreachable!("watch plan checked")
        };
        let (directories, ignored_prefixes, show_hidden) = plan.into_parts();
        let result = if directories.is_empty() {
            self.file_tree_watcher = None;
            Ok(ui::file_tree::FileTreeMaintenanceResult::WatchSetApplied)
        } else {
            if self.file_tree_watcher.is_none() {
                self.file_tree_watcher = AppFileTreeWatcher::new(ctx.clone()).ok();
            }
            match self.file_tree_watcher.as_mut() {
                Some(watcher) => watcher
                    .replace(generation, directories, ignored_prefixes, show_hidden)
                    .map(|()| ui::file_tree::FileTreeMaintenanceResult::WatchSetApplied),
                None => Err(ui::file_tree::FileTreeMaintenanceErrorCode::WatchUnavailable),
            }
        };
        if result.is_err() {
            self.file_tree_watcher = None;
        }
        if let Some(tree) = self.file_tree.as_mut() {
            tree.complete_maintenance(ui::file_tree::FileTreeMaintenanceCompletion {
                operation,
                generation,
                result,
            });
        }
        ctx.request_repaint();
    }

    fn poll_app_host_io(&mut self, ctx: &egui::Context) {
        let completed =
            self.app_host_io
                .as_ref()
                .and_then(|task| match task.result_rx.try_recv() {
                    Ok(completion) => Some(Some(completion)),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(None),
                    Err(std::sync::mpsc::TryRecvError::Empty) => None,
                });
        if let Some(completion) = completed {
            let task = self.app_host_io.take().expect("host task exists");
            let fallback = task.fallback;
            let _ = task.handle.join();
            self.apply_app_host_completion(
                completion.unwrap_or_else(|| fallback.into_completion()),
            );
        }
        if self.app_host_io.is_some() {
            return;
        }
        if !self.try_apply_pending_folder_picker_completion() {
            return;
        }
        let action =
            self.connector_coordinator
                .try_take_host_action()
                .map(AppHostIoAction::Connector)
                .or_else(|| self.pending_app_host_action.take())
                .or_else(|| {
                    self.active.workspace_ui.take_io_intent().map(|intent| {
                        AppHostIoAction::Workspace {
                            workspace_id: self.active.id.clone(),
                            intent,
                        }
                    })
                })
                .or_else(|| {
                    self.file_tree
                        .as_mut()
                        .and_then(ui::file_tree::FileTreeUi::take_io_intent)
                        .map(AppHostIoAction::FileTree)
                })
                .or_else(|| {
                    self.pending_file_tree_maintenance
                        .take()
                        .map(AppHostIoAction::FileTreeMaintenance)
                })
                .or_else(|| {
                    self.inbox_waiting_ui
                        .take_preview_intent()
                        .map(AppHostIoAction::InboxPreview)
                })
                .or_else(|| {
                    self.diff_panel_ui
                        .take_io_intent()
                        .map(AppHostIoAction::Diff)
                })
                .or_else(|| {
                    self.pending_composer_history.take().map(|history| {
                        AppHostIoAction::PersistComposerHistory {
                            path: self.composer_history_path.clone(),
                            history,
                        }
                    })
                });
        let Some(action) = action else {
            return;
        };
        match AppHostIoTask::spawn(action, ctx.clone()) {
            Ok(task) => self.app_host_io = Some(task),
            Err(fallback) => {
                self.apply_app_host_completion(fallback.into_completion());
                ctx.request_repaint();
            }
        }
    }

    fn try_apply_pending_folder_picker_completion(&mut self) -> bool {
        let Some((purpose, path)) = self.pending_folder_picker_completion.take() else {
            return true;
        };
        let retry_purpose = purpose.clone();
        let request_workspace = match &purpose {
            FolderPickerPurpose::SetProjectPath { workspace_id } => workspace_id.clone(),
            FolderPickerPurpose::SwitchWorkspace
            | FolderPickerPurpose::SelectWorkspaceInSettings => self.active.id.clone(),
        };
        let action = match purpose {
            FolderPickerPurpose::SetProjectPath { .. } => {
                SettingsJobAction::SetProjectPath { path: path.clone() }
            }
            FolderPickerPurpose::SwitchWorkspace => SettingsJobAction::FindOrCreateWorkspace {
                name: workspace_name_for_path(&path, self.config.ui.session_name_style),
                path: path.clone(),
                purpose: WorkspaceMutationPurpose::SwitchRuntime,
            },
            FolderPickerPurpose::SelectWorkspaceInSettings => {
                SettingsJobAction::FindOrCreateWorkspace {
                    name: workspace_name_for_path(&path, self.config.ui.session_name_style),
                    path: path.clone(),
                    purpose: WorkspaceMutationPurpose::SelectInSettings,
                }
            }
        };
        if self.queue_global_settings_action(&request_workspace, action) {
            true
        } else {
            self.pending_folder_picker_completion = Some((retry_purpose, path));
            false
        }
    }

    /// Production composition root. Paths/config are prepared by `main`; concrete storage,
    /// keyring-backed services, crash recovery, and all adapters are constructed only here.
    pub fn bootstrap(
        config: Config,
        config_path: PathBuf,
        data_dir: PathBuf,
        egui_ctx: egui::Context,
        bench: Option<crate::bench::Bench>,
    ) -> anyhow::Result<Self> {
        if secret::init_platform_store().is_err() {
            tracing::warn!(
                kind = "secret_store",
                phase = "initialize",
                error_code = "platform_store_unavailable",
                "keyring store unavailable"
            );
        }
        let db_path = data_dir.join("metadata.sqlite3");
        let db = Db::open(&db_path)?;
        let workspace_id = initial_workspace_id(
            &db,
            config.ui.last_workspace_id.as_deref(),
            &config.ui.closed_workspace_ids,
        )?;
        let reconciled = db
            .reconcile_orphan_sessions()
            .map_err(|error| anyhow::anyhow!("session_crash_reconciliation_failed: {error:#}"))?;
        if reconciled > 0 {
            tracing::info!(count = reconciled, "orphan sessions reconciled");
        }
        let logs_base = data_dir.join("logs");
        match storage::gc_session_logs(&logs_base, storage::SESSION_LOG_DISK_BUDGET_BYTES) {
            Ok(bytes) => tracing::info!(bytes, "session log budget applied"),
            Err(_) => tracing::warn!(
                kind = "session_log",
                phase = "gc",
                error_code = "session_log_gc_failed",
                "session log GC failed"
            ),
        }
        Ok(Self::new(
            config,
            config_path,
            db,
            workspace_id,
            logs_base,
            db_path,
            egui_ctx,
            bench,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        mut config: Config,
        config_path: PathBuf,
        db: Db,
        workspace_id: String,
        logs_base: PathBuf,
        db_path: PathBuf,
        egui_ctx: egui::Context,
        bench: Option<crate::bench::Bench>,
    ) -> Self {
        // output_batch_ms는 시작 시 고정, scrollback_lines는 세션 spawn 시점에 전달
        config.ui.last_workspace_id = Some(workspace_id.clone());
        let persisted_closed_workspaces = config.ui.closed_workspace_ids.clone();
        // 벤치(B1): DEPPY_BENCH_WORKSPACES=N개가 실제로 상주해야 RSS 비교가 성립한다.
        // warm 상한은 **설정값**이므로(코드 경로 변경 아님) 벤치 임시 config에서만 올린다.
        // clamp(max_warm ≤ 8) 때문에 실효 상한은 active 1 + warm 8 = 9개다.
        if let Some(bench) = &bench {
            let wanted = bench.opts.workspaces.saturating_sub(1).min(8) as u32;
            config.performance.max_warm = config.performance.max_warm.max(wanted);
            config.performance.max_live_warm = config.performance.max_live_warm.max(wanted + 1);
        }
        let redaction = secret::RedactionService::new();
        let i18n = load_catalog(&config.i18n.locale);
        // Complete crash reconciliation and one-time logical→physical migration before any
        // runtime, dotenv, Connector, or settings worker can resolve a credential.
        reconcile_and_migrate_startup_secrets(&db, &KeyringSecretStore)
            .expect("physical secret startup reconciliation failed");
        let pending_approval_owner = Arc::new(
            db.acquire_pending_approval_owner()
                .expect("pending approval owner acquire failed"),
        );
        db.deny_session_scoped_pending_approvals_owned(
            pending_approval_owner.as_ref(),
            deppy_core::time::unix_secs_i64(),
        )
        .expect("pending approval startup reconciliation failed");
        let initial_approval_snapshot =
            load_approval_snapshot(&db).expect("initial approval snapshot load failed");
        // shim을 make_runtime 전에 설치한다 — 첫 셸부터 PATH에 shim이 얹히도록.
        if config.ui.agent_status_hooks
            && let Ok(bin) = mcp_proxy_bin()
            && let Err(e) = crate::agent_shim::install(&db_path, &bin)
        {
            tracing::warn!("agent shim 설치 실패: {e:#}");
        }
        let runtime_host_factory = Arc::new(runtime::InProcessRuntimeHostFactory::new(
            Arc::new(AppRuntimeSecretResolver::new(db_path.clone())),
            redaction.clone(),
        ));
        let active = Self::make_runtime(
            &config,
            &logs_base,
            &workspace_id,
            1,
            &db_path,
            runtime_host_factory.as_ref(),
            &db,
            &egui_ctx,
        );

        let connector_initial_overview = AppConnectorRepository::overview_from(
            db.mcp_server_inventory_versioned(
                connector_contract::ResourceLimits::PRODUCTION_CEILING.import_servers,
            )
            .expect("initial Connector overview load failed"),
        )
        .expect("initial Connector overview validation failed");
        let connector_host: Arc<dyn connector_service::ConnectorHost> =
            Arc::new(AppConnectorHost {
                ctx: egui_ctx.clone(),
            });
        let connector_secrets: Arc<dyn connector_service::ConnectorSecrets> =
            Arc::new(AppConnectorSecrets {
                secret_store: KeyringSecretStore,
                redaction: redaction.clone(),
            });
        let connector_mcp: Arc<dyn connector_service::ConnectorMcp> = Arc::new(
            connector_service::ProductionConnectorMcp::new(
                mcp::LocalMcpManager::new(redaction.clone()),
                Arc::clone(&connector_secrets),
                CONNECTOR_IDLE_TTL,
                connector_contract::ResourceLimits::PRODUCTION_CEILING.backend_leases,
            )
            .expect("Connector MCP adapter configuration is valid"),
        );
        let connector_oauth: Arc<dyn connector_service::ConnectorOAuth> = Arc::new(
            connector_service::ProductionConnectorOAuth::new(
                CONNECTOR_OAUTH_HTTP_TIMEOUT,
                CONNECTOR_OAUTH_CALLBACK_TIMEOUT,
                mcp::PROTOCOL_VERSION,
            )
            .expect("Connector OAuth adapter configuration is valid"),
        );
        let connector_coordinator = connector_service::ConnectorCoordinator::new(
            connector_service::ConnectorCoordinatorConfig {
                limits: connector_contract::ResourceLimits::PRODUCTION_CEILING,
                idle_ttl: CONNECTOR_IDLE_TTL,
                initial_overview: Some(connector_initial_overview),
                repository_factory: Arc::new(AppConnectorRepositoryFactory {
                    db_path: db_path.clone(),
                    redaction: redaction.clone(),
                }),
                secrets: connector_secrets,
                mcp: connector_mcp,
                oauth: connector_oauth,
                host: connector_host,
                clock: Arc::new(connector_service::SystemCoordinatorClock::default()),
                operation_ids: Arc::new(connector_service::SystemOperationIdFactory::default()),
            },
        )
        .expect("Connector coordinator limits are valid");
        let connector_snapshot_reader = connector_coordinator.snapshot_reader();

        let approval_wake_hub = ApprovalWakeHub::new(
            db_path.clone(),
            egui_ctx.clone(),
            Arc::clone(&pending_approval_owner),
        );
        // agent 감지 백그라운드 워커 (ps/lsof/transcript 스캔을 UI 스레드 밖에서, codex #3).
        let (agent_detect_worker, agent_detect_input, agent_detect_rx) =
            crate::agent_detect_worker::AgentDetectWorker::spawn(egui_ctx.clone());
        let env_project_rows_worker =
            new_env_project_rows_worker(db_path.clone(), egui_ctx.clone());
        let env_secret_reveal_worker =
            new_env_secret_reveal_worker(db_path.clone(), egui_ctx.clone());
        let settings_snapshot_worker =
            SettingsSnapshotWorker::new(db_path.clone(), redaction.clone(), egui_ctx.clone());
        let launcher_ctx = egui_ctx.clone();
        let launcher_excluded_directory = crate::agent_shim::shim_path();
        let agent_launcher_worker = crate::lazy_worker::LazyBoundedWorker::new(
            "agent-launch-detect",
            std::time::Duration::from_secs(30),
            move || {
                let excluded_directory = launcher_excluded_directory.clone();
                move |_| {
                    crate::agent_launcher::detect_installed_agents(excluded_directory.as_deref())
                }
            },
            move || launcher_ctx.request_repaint(),
        );
        let dotenv_sync_worker =
            new_dotenv_sync_worker(db_path.clone(), redaction.clone(), egui_ctx.clone());
        let initial_agent_state_scope = Arc::new(
            AppAgentStateScope::new(1, workspace_id.clone(), vec![workspace_id.clone()])
                .expect("initial agent state scope is bounded"),
        );
        let agent_state_worker = {
            let worker_db_path = db_path.clone();
            let wake_ctx = egui_ctx.clone();
            crate::agent_state_worker::AgentStateWorker::new(
                Arc::new(move || {
                    Ok(AppAgentStateBackend {
                        db_path: worker_db_path.clone(),
                        db: None,
                    })
                }),
                Arc::new(move || wake_ctx.request_repaint()),
            )
        };
        let status_feed_rx_channel = crate::status_feed::spawn(egui_ctx.clone());
        let notice_translation_cache_path = db_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("notice_translations.json");
        let composer_history_path = db_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("composer_history.jsonl");
        // 프롬프트 라이브러리 (기능2) — 없으면 예시 프롬프트로 씨드해 팔레트가 비지 않게 한다.
        let prompt_library_path = db_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("prompt_library.json");
        let mut prompt_library = crate::prompt_library::PromptLibrary::load(&prompt_library_path);
        if prompt_library.prompts.is_empty() {
            prompt_library = crate::prompt_library::PromptLibrary::default_seed();
            if let Err(error) = prompt_library.save(&prompt_library_path) {
                tracing::warn!(
                    path = %prompt_library_path.display(),
                    "프롬프트 라이브러리 씨드 저장 실패: {error:#}"
                );
            }
        }
        let notice_translation_cache =
            match crate::notice_translate::TranslationCache::load(&notice_translation_cache_path) {
                Ok(cache) => cache,
                Err(error) => {
                    tracing::warn!(
                        path = %notice_translation_cache_path.display(),
                        "공지 번역 캐시를 읽지 못해 빈 캐시로 시작: {error:#}"
                    );
                    crate::notice_translate::TranslationCache::default()
                }
            };
        let notice_read_state_path = db_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("notice_read_state.json");
        let notice_read_state =
            match crate::status_feed::NoticeReadState::load(&notice_read_state_path) {
                Ok(state) => state,
                Err(error) => {
                    tracing::warn!(
                        path = %notice_read_state_path.display(),
                        "공지 읽음 상태를 읽지 못해 새 기준으로 시작: {error:#}"
                    );
                    crate::status_feed::NoticeReadState::default()
                }
            };

        // main에서 CreationContext를 받자마자 이 설정으로 폰트를 이미 설치했다. sentinel로
        // 시작하면 첫 프레임에 15MB AppleGothic을 포함한 FontDefinitions를 다시 만들고
        // 전체 TTF equality 비교까지 하므로, 실제 설치 상태를 초기 snapshot으로 쓴다.
        let last_ui_font = config.ui.ui_font.clone();
        let last_mono_font = config.terminal.mono_font.clone();
        let last_mono_weight = config.terminal.mono_weight.clone();
        let initial_project_name_style = config.ui.session_name_style;
        let agent_sessions_secrets_snapshot = match secret::SecretStore::has_secret(
            &KeyringSecretStore,
            CODEX_LLM_API_KEY_ENTRY_ID,
        ) {
            Ok(present) => ui::agent_sessions::AgentSessionsSecretsSnapshot::new(0, present),
            Err(_) => ui::agent_sessions::AgentSessionsSecretsSnapshot::unavailable(0),
        };
        let mut app = Self {
            config,
            config_path,
            last_theme_dark: true,
            last_ui_font,
            last_mono_font,
            last_mono_weight,
            last_ui_scale: -1.0,
            last_dotenv_state: None,
            dotenv_sync_worker,
            dotenv_sync_generation: 0,
            dotenv_sync_revision: 0,
            dotenv_next_operation_id: 0,
            dotenv_sync_context: None,
            dotenv_pending_operations: std::collections::HashMap::with_capacity(
                crate::dotenv_sync::DOTENV_WORKER_CONTINUATION_MAX,
            ),
            dotenv_pending_bytes: 0,
            workspace_rename_prompt: None,
            env_project_close_confirm: None,
            ws_close_confirm: None,
            ws_rename_edit: None,
            runtime_stream_warning: false,
            warm_limit_warning: None,
            web_switch_queue: Arc::new(std::sync::Mutex::new(Vec::new())),
            web_notice: None,
            dismissed_renames: std::collections::HashSet::new(),
            settings_open: false,
            settings_was_open: false,
            settings_category: ui::settings::Category::default(),
            settings_workspace_id: None,
            settings_search: String::new(),
            env_api_project_edit: EnvApiProjectEditState::default(),
            env_api_projects_cache: None,
            env_session_banner: None,
            storm_banner_dismissed: false,
            pending_storm_action: None,
            env_project_rows_worker,
            env_project_rows_generation: 0,
            env_project_rows_in_flight: None,
            env_project_rows_failed: false,
            env_secret_reveal_worker,
            pending_env_secret_reveal: None,
            env_secret_generation: 0,
            settings_snapshot_worker,
            pending_settings_job: None,
            settings_snapshot_generation: 0,
            settings_snapshot_revision: 0,
            settings_snapshot_pending: false,
            settings_pending_operation: None,
            settings_snapshot_retry_at: None,
            settings_snapshot_workspace_id: None,
            agents_snapshot: ui::agents::AgentsSnapshot::unavailable(0),
            env_profiles_snapshot: ui::env_profiles::EnvProfilesSnapshot::unavailable(
                0,
                workspace_id.clone(),
                false,
            ),
            credentials_snapshot: ui::credentials::CredentialsSnapshot::unavailable(0),
            agent_sessions_secrets_snapshot,
            db,
            secret_store: KeyringSecretStore,
            agents_ui: ui::agents::AgentsUi::new(),
            agent_launcher_ui: ui::agent_launcher::AgentLauncherUi::new(),
            agent_launcher_worker,
            agent_launcher_snapshot: None,
            agent_launcher_detection_requested: false,
            agent_launcher_detection_in_flight: false,
            pending_agent_launcher_intent: None,
            next_agent_launcher_request_id: 0,
            pending_agent_launcher_launch: None,
            agent_launcher_seen_workspaces: std::collections::HashSet::new(),
            agent_sessions_ui: ui::agent_sessions::AgentSessionsUi::new()
                .with_catalog(&i18n)
                .with_app_server_host(Arc::new(AppCodexAppServerHost {
                    secret_store: KeyringSecretStore,
                })),
            pending_agent_sessions_action: None,
            diff_panel_ui: ui::diff_panel::DiffPanelUi::new(),
            agent_state_worker,
            agent_state_scope: initial_agent_state_scope,
            pending_agent_state_scope: None,
            agent_state_next_revision: 0,
            agent_state_next_operation_id: 0,
            agent_state_admission_blocked: false,
            activity_project_names: std::collections::HashMap::new(),
            activity_project_name_style: initial_project_name_style,
            project_name_projection_dirty: true,
            project_name_projection_pending: false,
            pending_agent_state_structured: Vec::new(),
            connector_coordinator,
            connector_snapshot_reader,
            connector_ui: connector_ui::ConnectorUi::new(&i18n),
            pending_connector_dispatch: None,
            app_host_io: None,
            pending_app_host_action: None,
            pending_file_tree_maintenance: None,
            file_tree_watcher: None,
            pending_app_controller_action: None,
            pending_workspace_controller_action: None,
            pending_turn_done_clear: None,
            pending_settings_config_apply: false,
            pending_config_save: false,
            pending_composer_history: None,
            pending_folder_picker_completion: None,
            credentials_ui: ui::credentials::CredentialsUi::new(),
            env_profiles_ui: ui::env_profiles::EnvProfilesUi::new(),
            activity_ui: ui::activity::ActivityUi::new(),
            agent_terminal_ui: ui::agent_terminal::AgentTerminalUi::new(),
            fleet_ui: ui::fleet::FleetUi::default(),
            pending_batch_spawn: None,
            broadcast_working: std::collections::HashMap::new(),
            activity_rows_cache: None,
            status_feed_rx: status_feed_rx_channel.0,
            status_feed_refresh: status_feed_rx_channel.1,
            status_feed_startup_polled: false,
            status_feed: crate::status_feed::StatusFeedSnapshot::default(),
            notice_read_state,
            notice_read_state_path,
            home_notice_unread: 0,
            notice_translation_cache,
            notice_translation_cache_path,
            notice_translate_rx: None,
            notice_translate_bin: None,
            ollama_models: None,
            ollama_detect_done: false,
            ollama_detect_rx: None,
            agent_sessions_was_open: false,
            notifications_ui: ui::notifications::NotificationsUi::new(),
            inbox_waiting_ui: ui::inbox_waiting::InboxWaitingUi::new(),
            // 히스토리 파일은 앱 데이터 디렉터리(= 메타데이터 파일과 같은 폴더) 아래.
            composer: ui::composer::ComposerUi::new(composer_history_path.clone()),
            composer_history_path,
            prompt_palette: ui::prompt_palette::PromptPaletteUi::default(),
            prompt_library,
            prompt_library_path,
            approvals_ui: ui::approvals::ApprovalsUi::new(),
            approval_notified: std::collections::HashSet::new(),
            _pending_approval_owner: pending_approval_owner,
            approval_wake_hub,
            approval_launch_tracker: ApprovalLaunchTracker::default(),
            approval_global_reconcile: ApprovalGlobalReconcile::default(),
            pending_proxy_launches: std::collections::VecDeque::new(),
            last_offscreen_fix: std::time::Instant::now(),
            startup_positioned: false,
            active,
            next_runtime_instance: 2,
            warm: std::collections::HashMap::new(),
            warm_order: Vec::new(),
            frame_stats: crate::perf::FrameStats::new(),
            bench,
            perf_harness_next: crate::perf::harness_enabled().then_some(0),
            i18n,
            egui_ctx,
            db_path,
            logs_base,
            runtime_host_factory,
            workspaces: Vec::new(),
            workspace_anchors: std::collections::HashMap::new(),
            persisted_activity_panes: std::collections::HashMap::new(),
            agent_activity: std::collections::HashMap::new(),
            agent_bindings: std::collections::HashMap::new(),
            agent_detect_worker,
            agent_detect_input,
            agent_detect_rx,
            agent_detect_epoch: 0,
            last_hook_query: std::time::Instant::now(),
            hook_overrides: std::collections::HashMap::new(),
            persisted_agents: std::collections::HashMap::new(),
            agent_needs_input: std::collections::HashSet::new(),
            global_waiting: Vec::new(),
            agent_turn_done: std::collections::HashMap::new(),
            agent_working: std::collections::HashSet::new(),
            global_working: std::collections::HashSet::new(),
            global_turn_done: std::collections::HashMap::new(),
            session_alerts: std::collections::HashMap::new(),
            session_cwds: std::collections::HashMap::new(),
            agent_info: std::collections::HashMap::new(),
            statuslines: std::collections::HashMap::new(),
            restore_agents: std::collections::HashMap::new(),
            restore_loaded_for: None,
            resumed_panes: std::collections::HashSet::new(),
            resume_probe_pending_panes: std::collections::HashSet::new(),
            pending_focus: None,
            pending_shutdowns: PendingShutdownRegistry::default(),
            next_warm_idle_eviction_at: None,
            warm_eviction_deferred: false,
            closed_workspaces: persisted_closed_workspaces
                .into_iter()
                .map(|workspace_id| (workspace_id, ClosedWorkspaceState::Persisted))
                .collect(),
            remote: None,
            remote_error: None,
            remote_reveal_token: false,
            web: None,
            web_error: None,
            web_reveal_url: false,
            web_qr: None,
            worktree_rx: None,
            worktree_remove_rx: None,
            ts_detect_rx: None,
            ts_detected: None,
            ts_detect_overwrite: false,
            last_web_sync: None,
            serve_rx: None,
            serve_state: None,
            known_hosts_cache: None,
            agent_workspace_cwd_key: None,
            agent_workspace_cwd: None,
            file_tree: None,
        };
        app.prune_resolved_approvals();
        // hook 상태 테이블 오래된 행 정리(무한 누적 방지).
        if let Err(e) = app.db.prune_agent_hook_state() {
            tracing::warn!("hook 상태 정리 실패: {e:#}");
        }
        app.apply_approval_snapshot(initial_approval_snapshot);
        // 파일 트리 헤더(workspace 이름) 표시용 — 시작 시 1회 로드
        app.refresh_workspaces();
        if app.config.ui.file_tree_enabled {
            app.file_tree = Some(app.make_file_tree());
        }
        // 에이전트 상태 hook 전역 설치/해제 (설정 토글에 따라, best-effort).
        app.sync_agent_hooks();
        // The first process-capable restore is an exact dotenv continuation. Construction alone
        // does not bypass source/keyring verification or fall back to an empty environment.
        let persisted_restore_exists = app
            .persisted_activity_panes
            .get(&app.active.id)
            .is_some_and(|panes| !panes.is_empty());
        if persisted_restore_exists {
            let initial_runtime_instance = app.active.runtime_instance;
            app.stage_runtime_restore(initial_runtime_instance);
        } else if app.bench.is_none() && app.perf_harness_next.is_none() {
            app.offer_agent_launcher_for_active();
        }
        // 시작 시 config가 remote를 켜 뒀으면 best-effort로 기동한다 (실패는 log + settings 표시,
        // config는 그대로 두어 다음 실행에 재시도). 자동 시작은 config 저장을 유발하지 않는다.
        if app.config.remote.tls_enabled {
            match app.start_remote() {
                Ok(state) => app.remote = Some(state),
                Err(e) => {
                    tracing::warn!("remote TLS 자동 시작 실패: {e:#}");
                    app.remote_error = Some(format!("{e:#}"));
                }
            }
        }
        // 모바일 웹(PWA) 서버 자동 시작 — remote와 동일한 best-effort 규칙 (v3.3 P1).
        if app.config.web.enabled {
            match app.start_web() {
                Ok(state) => app.web = Some(state),
                Err(e) => {
                    tracing::warn!("모바일 웹 서버 자동 시작 실패: {e:#}");
                    app.web_error = Some(format!("{e:#}"));
                }
            }
        }
        app
    }

    /// warm 상한(빈 warm 유지 수 `max_warm`, live warm hard cap `max_live_warm`)은 설정
    /// (성능)으로 조정한다 — `self.config.performance`. 기본값은 RAM 유도(config.rs).
    /// 폰 미러 진입(I1b-2) 안내 배너 표시 시간 — 이 뒤 앱이 notice를 None으로 돌린다.
    const WEB_NOTICE_TTL: std::time::Duration = std::time::Duration::from_secs(6);
    /// Warm workspace가 이 시간 동안 재활성화되지 않으면 Suspended로 내린다. 세션/PTY는
    /// 종료되고 layout/session metadata만 DB에 남는다 (§14.1). 에이전트·자식 작업은
    /// 계속 보호하고, 단일 저CPU 셸 리더만 남은 경우에만 fresh 셸 복원 전제로 내린다.
    const WARM_AUTO_SUSPEND_AFTER: std::time::Duration = std::time::Duration::from_secs(30 * 60);

    const RESOLVED_APPROVAL_RETENTION_SECS: i64 = 30 * 24 * 60 * 60;

    /// 한 workspace의 런타임 워커를 만든다: 생성 → wake 구독 → 저장 layout 복원 →
    /// credential redaction 시드. (perf 하네스는 제외 — new()에서 기본 workspace만.)
    #[allow(clippy::too_many_arguments)]
    fn make_runtime(
        config: &Config,
        logs_base: &std::path::Path,
        workspace_id: &str,
        runtime_instance: u64,
        db_path: &std::path::Path,
        runtime_host_factory: &runtime::InProcessRuntimeHostFactory,
        db: &Db,
        egui_ctx: &egui::Context,
    ) -> WorkspaceRuntime {
        // 세션 로그 루트: logs/<workspace_id>/ (설계문서 7장)
        let logs_root = logs_base.join(workspace_id);
        // 셸 cwd = workspace 폴더(존재하는 디렉터리일 때만) — 재시작 시 루트가 아닌 이 폴더에서
        // 셸이 떠 claude/codex를 이어갈 수 있다(#2). 미설정/무효면 None(앱 cwd 상속).
        let shell_cwd = db
            .workspace_path(workspace_id)
            .ok()
            .flatten()
            .map(PathBuf::from)
            .filter(|p| p.is_dir());
        let runtime = runtime_host_factory
            .create_client(runtime::RuntimeHostConfig {
                output_batch_ms: config.performance.output_batch_ms,
                logs_root,
                persist: Some(runtime::PersistConfig {
                    db_path: db_path.to_path_buf(),
                    workspace_id: workspace_id.to_owned(),
                }),
                cwd: shell_cwd.clone(),
                extra_env: Self::shim_shell_env(config),
            })
            .expect("runtime worker thread 생성");
        // 상태 이벤트 도착 시 UI를 깨운다 (§14.1 Warm 알림 유지). subscribe→restore 순서
        // 를 코드로 보장하려 subscribe 직후 복원 명령을 보낸다.
        let runtime_events = Self::subscribe_runtime_events(&runtime, egui_ctx);
        // RestoreWorkspace는 background dotenv 결과를 적용한 뒤 보낸다. `.env`/keychain I/O를
        // UI thread에서 수행하지 않으면서도 복원된 첫 셸부터 올바른 기본 env를 받게 한다.
        WorkspaceRuntime {
            id: workspace_id.to_owned(),
            runtime_instance,
            dotenv_state: None,
            runtime,
            events: runtime_events,
            workspace_ui: ui::workspace::WorkspaceUi::new(),
            render_active: true,
            pending_events: Vec::new(),
            session_titles: std::collections::HashMap::new(),
            resource_usage: None,
            session_resource_usage: Vec::new(),
            storm_episodes: std::collections::HashMap::new(),
            storm_next_episode_id: 0,
            storm_notify_pending: Vec::new(),
            frozen_sessions: std::collections::HashSet::new(),
            input_pressure: None,
            session_input_pressure: std::collections::HashMap::new(),
            session_dotenv_states: std::collections::HashMap::new(),
            backgrounded_at: None,
            live: LiveSessionTracker::default(),
            created: std::time::Instant::now(),
            pending_agent_spawns: 0,
            event_overflow_pending: false,
            event_resync_pending: false,
            pending_replay_resync: false,
        }
    }

    // --- 렌더러 A/B 실측 드라이버 (B1) ---------------------------------------
    // 전부 `self.bench`(env 게이트) 뒤. 워크스페이스 생성/전환/삭제는 **실제 앱 경로**
    // (DB workspace + runtime worker)를 그대로 탄다 — 그래야 실측이 의미가 있다.

    fn bench_step(&mut self, ctx: &egui::Context) {
        let Some(mut bench) = self.bench.take() else {
            return;
        };
        bench.set_workspaces(1 + self.warm.len());
        // 종료 중이면 새 작업을 시작하지 않는다 (on_exit이 깨끗이 정리되도록).
        if !bench.closing() {
            if bench.needs_setup() {
                self.bench_setup(&mut bench);
            }
            let now = std::time::Instant::now();
            if bench.switch_due(now) {
                self.cycle_workspace(1);
            }
            match bench.createdelete_step(now) {
                Some(crate::bench::CreateDeleteStep::Create(iter)) => {
                    self.bench_create_and_run(&mut bench, &format!("bench-cd-{iter}"));
                }
                Some(crate::bench::CreateDeleteStep::Delete(id)) => {
                    self.bench_delete_workspace(&mut bench, &id);
                }
                None => {}
            }
            // 드라이버가 워크스페이스를 조작하는 시나리오만 프레임을 요구한다.
            if bench.needs_frames() {
                ctx.request_repaint();
            }
        }
        self.bench = Some(bench);
    }

    fn pump_perf_harness(&mut self) {
        let Some(index) = self.perf_harness_next else {
            return;
        };
        if index >= crate::perf::HARNESS_SESSIONS {
            self.perf_harness_next = None;
            return;
        }
        if !self.dotenv_pending_operations.is_empty() {
            return;
        }
        let (command, args) = crate::perf::harness_command(index);
        let command = runtime::RuntimeCommand::SpawnAgent {
            agent_config_id: None,
            cols: 120,
            rows: 40,
            scrollback_lines: self.config.terminal.scrollback_lines as usize,
            command,
            args,
            env_plain: Vec::new(),
            env_secrets: Vec::new(),
            waiting_regex: None,
            approval_regex: None,
            error_regex: None,
            done_regex: None,
        };
        if self
            .stage_dotenv_continuation(
                self.active.runtime_instance,
                PendingDotenvContinuation::RuntimeCommand(command),
            )
            .is_ok()
        {
            self.perf_harness_next = Some(index + 1);
        } else {
            self.perf_harness_next = None;
            tracing::warn!(
                kind = "perf_harness",
                phase = "launch_admission",
                error_code = "backpressure",
                "performance harness launch failed closed"
            );
        }
    }

    /// fleet 배치 스폰(PR-S1) 펌프 — settings 잡 큐는 동시 1개만 허용해(단일 슬롯) 매
    /// logic tick 빈 슬롯이면 PrepareAgentLaunch를 하나씩 큐잉한다. AgentsIntent::Run
    /// 핸들러와 같은 launch 파이프라인을 재사용하되 N개를 여러 프레임에 걸쳐 순차 발사한다.
    fn pump_batch_spawn(&mut self, ctx: &egui::Context) {
        let Some(pending) = &self.pending_batch_spawn else {
            return;
        };
        // staging(버튼 클릭) 이후 활성 workspace가 바뀌면 남은 스폰을 전부 취소한다 —
        // 그러지 않으면 사용자가 다른 workspace로 전환한 사이에도 스폰이 이어져 엉뚱한
        // workspace에 세션이 쌓인다(배치 스폰은 클릭 시점의 workspace에만 적용).
        if pending.staged_workspace_id != self.active.id {
            self.pending_batch_spawn = None;
            return;
        }
        // 아래 request_settings_snapshot_if_needed/queue_settings_action이 &mut self를
        // 요구하므로 pending의 값은 먼저 복제해 빌림을 끝낸다.
        let agent_id = pending.agent_id.clone();
        let prompt = pending.prompt.clone();
        let workspace_id = self.active.id.clone();
        // 설정/에이전트 창을 한 번도 안 열었으면 agents_snapshot이 비어 있을 수 있다 —
        // Run 핸들러와 동일하게 project_root는 workspace_tree_root에서 유도한다.
        let project_root = self.workspace_tree_root(&workspace_id);
        self.request_settings_snapshot_if_needed(&workspace_id, project_root);
        let queued = self.queue_settings_action(
            &workspace_id,
            None,
            SettingsJobAction::PrepareAgentLaunch {
                agent_id,
                profile_id: None,
                runtime_workspace_id: workspace_id.clone(),
                extra_arg: prompt,
            },
        );
        if queued && let Some(pending) = &mut self.pending_batch_spawn {
            pending.remaining -= 1;
            if pending.remaining == 0 {
                self.pending_batch_spawn = None;
            }
        }
        // 남은 스폰이 있으면 다음 프레임에 즉시 재시도 — 사용자 입력 없이도 큐가 드레인된다.
        if self.pending_batch_spawn.is_some() {
            ctx.request_repaint();
        }
    }

    fn bench_setup(&mut self, bench: &mut crate::bench::Bench) {
        let base = self.active.id.clone();
        bench.base = Some(base.clone());
        let extra = bench.opts.workspaces.saturating_sub(1);
        if extra > 0 {
            bench.emit_rss_stage("ws_create_begin");
            for i in 0..extra {
                self.bench_create_and_run(bench, &format!("bench-ws-{i}"));
            }
            // 활성은 항상 1개 — 기준 워크스페이스로 복귀(나머지는 warm으로 상주).
            let started = std::time::Instant::now();
            self.switch_workspace(&base);
            bench.emit_ws_step("switch_back", elapsed_ms(started));
            // 상주 수를 먼저 갱신한 뒤 스테이지를 찍는다 — 안 그러면 ws_create_done이
            // 직전 프레임의 값(1)을 달고 나간다.
            bench.set_workspaces(1 + self.warm.len());
            bench.emit_rss_stage("ws_create_done");
        }
        // createdelete는 반복마다 자기 셸을 띄운다 — 기준 워크스페이스는 비워 둔다.
        if bench.scenario() != crate::bench::Scenario::CreateDelete {
            bench.begin_burst();
            self.bench_spawn_scenario(bench);
        }
    }

    /// 워크스페이스를 만들고(DB) 전환한 뒤(runtime worker) 시나리오 셸을 띄운다.
    fn bench_create_and_run(&mut self, bench: &mut crate::bench::Bench, name: &str) {
        let started = std::time::Instant::now();
        let id = match self.db.create_workspace(name) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!("벤치 워크스페이스 생성 실패: {e:#}");
                return;
            }
        };
        bench.emit_ws_step("db_create", elapsed_ms(started));
        self.refresh_workspaces();

        let started = std::time::Instant::now();
        self.switch_workspace(&id);
        bench.emit_ws_step("runtime_alloc", elapsed_ms(started));
        if self.active.id != id {
            // warm hard cap이 전환을 거부했다 — 지어내지 말고 사실대로 남긴다.
            tracing::warn!(workspace = %id, "벤치 전환 거부(live warm 상한) — 이 워크스페이스는 미상주");
            bench.emit_ws_step("switch_rejected", 0.0);
            return;
        }
        bench.begin_burst();
        self.bench_spawn_scenario(bench);
        bench.set_createdelete_current(id);
    }

    fn bench_spawn_scenario(&mut self, bench: &mut crate::bench::Bench) {
        let Some((command, args)) = bench.scenario().command() else {
            return;
        };
        let started = std::time::Instant::now();
        let command = runtime::RuntimeCommand::SpawnAgent {
            agent_config_id: None,
            cols: 120,
            rows: 40,
            scrollback_lines: self.config.terminal.scrollback_lines as usize,
            command,
            args,
            env_plain: Vec::new(),
            env_secrets: Vec::new(),
            waiting_regex: None,
            approval_regex: None,
            error_regex: None,
            done_regex: None,
        };
        if self
            .stage_dotenv_continuation(
                self.active.runtime_instance,
                PendingDotenvContinuation::RuntimeCommand(command),
            )
            .is_err()
        {
            tracing::warn!(
                kind = "benchmark",
                phase = "launch_admission",
                error_code = "backpressure",
                "benchmark launch failed closed"
            );
        }
        bench.emit_ws_step("spawn_send", elapsed_ms(started));
    }

    /// UI의 삭제 경로와 같은 순서: 기준 워크스페이스로 물러난 뒤 warm shutdown + DB 삭제.
    fn bench_delete_workspace(&mut self, bench: &mut crate::bench::Bench, delete_id: &str) {
        let started = std::time::Instant::now();
        if self.active.id == delete_id
            && let Some(base) = bench.base.clone()
        {
            self.switch_workspace(&base);
        }
        if self.active.id == delete_id {
            tracing::warn!(workspace = %delete_id, "벤치: 활성 워크스페이스라 삭제 불가");
            return;
        }
        self.join_pending_shutdown(delete_id);
        if let Some(mut runtime) = self.warm.remove(delete_id) {
            self.close_approval_workspace(delete_id);
            runtime.runtime.shutdown();
        }
        self.warm_order.retain(|id| id != delete_id);
        self.refresh_warm_idle_deadline();
        self.broadcast_terminal_cache_policy();
        self.notifications_ui.prune_workspace(delete_id);
        if let Err(e) = self.db.delete_workspace(delete_id) {
            tracing::warn!("벤치 워크스페이스 삭제 실패: {e:#}");
        }
        self.refresh_workspaces();
        bench.emit_ws_step("delete", elapsed_ms(started));
    }

    fn subscribe_runtime_events(
        runtime: &InProcessRuntimeClient,
        ctx: &egui::Context,
    ) -> RuntimeEventReceiver {
        runtime.subscribe_with_wake(std::sync::Arc::new({
            let ctx = ctx.clone();
            // request_repaint()가 아니라 request_repaint_after(1ms) — egui는 delay==0인
            // 요청마다 "settle" 프레임을 한 장 더 붙인다(egui 0.35 context.rs:137, outstanding=1).
            // 0이 아닌 delay는 그 경로를 타지 않고, 이어서 delay -= predicted_dt로 0이 되어
            // 결국 즉시 리페인트된다 — 지연 없이 헛 프레임만 뺀다. 터미널 내용은 같은 프레임의
            // handle_events()에서 스냅샷이 반영된 뒤 그려지므로 settle 프레임이 필요 없다.
            move || ctx.request_repaint_after(std::time::Duration::from_millis(1))
        }))
    }

    fn request_agent_state_scope(&mut self) {
        let structured_workspace_ids = self
            .workspaces
            .iter()
            .map(|workspace| workspace.id.clone())
            .collect::<Vec<_>>();
        if structured_workspace_ids.is_empty() {
            return;
        }
        // refresh_workspaces is event-driven (startup or an explicit workspace mutation), so it
        // may re-arm one bounded admission attempt after an earlier thread-spawn failure.
        self.agent_state_admission_blocked = false;
        let next_epoch = self
            .pending_agent_state_scope
            .as_ref()
            .map_or(self.agent_state_scope.epoch, |scope| scope.epoch)
            .wrapping_add(1)
            .max(1);
        let Some(scope) =
            AppAgentStateScope::new(next_epoch, self.active.id.clone(), structured_workspace_ids)
        else {
            tracing::warn!(
                kind = "agent_state",
                phase = "scope",
                error_code = "invalid_data",
                "agent state scope update rejected"
            );
            return;
        };
        if self
            .pending_agent_state_scope
            .as_ref()
            .is_some_and(|pending| {
                scope.workspace_id == pending.workspace_id
                    && scope.structured_workspace_ids == pending.structured_workspace_ids
            })
        {
            return;
        }
        if scope.workspace_id == self.agent_state_scope.workspace_id
            && scope.structured_workspace_ids == self.agent_state_scope.structured_workspace_ids
        {
            // A rapid A -> B -> A switch can cancel a metadata-only pending transition. No
            // new-scope payload is ever constructed before the drain barrier, so continuing on
            // the still-current A scope is safe and avoids installing the obsolete B scope.
            self.pending_agent_state_scope = None;
            return;
        }
        self.pending_agent_state_scope = Some(Arc::new(scope));
    }

    fn agent_state_scope_ready(&self) -> bool {
        self.pending_agent_state_scope.is_none()
            && self.agent_state_scope.workspace_id == self.active.id
    }

    fn next_agent_state_revision(&mut self) -> crate::agent_state_worker::AgentStateRevision {
        self.agent_state_next_revision = self.agent_state_next_revision.wrapping_add(1).max(1);
        crate::agent_state_worker::AgentStateRevision::new(
            self.agent_state_scope.epoch,
            self.agent_state_next_revision,
        )
    }

    fn next_agent_state_operation_id(&mut self) -> u64 {
        self.agent_state_next_operation_id =
            self.agent_state_next_operation_id.wrapping_add(1).max(1);
        self.agent_state_next_operation_id
    }

    fn stage_agent_state_projection(
        &mut self,
        section: crate::agent_state_worker::AgentStateSection,
        kind: AppAgentStateProjectionKind,
    ) -> bool {
        if !self.agent_state_scope_ready() {
            return false;
        }
        let Some(payload) =
            AppAgentStateProjection::try_new(Arc::clone(&self.agent_state_scope), kind)
        else {
            tracing::warn!(
                kind = "agent_state",
                phase = "projection_stage",
                error_code = "resource_limit",
                "agent state projection rejected"
            );
            return false;
        };
        let key = self.next_agent_state_revision();
        match self
            .agent_state_worker
            .stage_projection(section, key, Arc::new(payload))
        {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(
                    kind = "agent_state",
                    phase = "projection_stage",
                    error_code = error.error_code().as_str(),
                    "agent state projection rejected"
                );
                false
            }
        }
    }

    fn stage_prepared_agent_state_exact(
        &mut self,
        worker_kind: crate::agent_state_worker::ExactKind,
        payload: AppAgentStateExactRequest,
    ) -> bool {
        let operation_id = self.next_agent_state_operation_id();
        match self
            .agent_state_worker
            .stage_exact(operation_id, worker_kind, Arc::new(payload))
        {
            Ok(()) => {
                // A new exact mutation is a relevant event, not a render/frame retry.
                self.agent_state_admission_blocked = false;
                true
            }
            Err(error) => {
                tracing::warn!(
                    kind = "agent_state",
                    phase = "exact_stage",
                    error_code = error.error_code().as_str(),
                    "agent state exact request rejected"
                );
                false
            }
        }
    }

    fn stage_agent_state_exact(&mut self, kind: AppAgentStateExactKind) -> bool {
        if !self.agent_state_scope_ready() {
            return false;
        }
        let worker_kind = match &kind {
            AppAgentStateExactKind::TurnDoneClear(_) => {
                crate::agent_state_worker::ExactKind::TurnDoneClear
            }
            AppAgentStateExactKind::BindingDelete(_) => {
                crate::agent_state_worker::ExactKind::BindingDelete
            }
            AppAgentStateExactKind::StructuredBatch(values) => {
                crate::agent_state_worker::ExactKind::StructuredBatch {
                    items: values.len(),
                }
            }
            AppAgentStateExactKind::FinalBindingReconcile { binding: value, .. } => {
                crate::agent_state_worker::ExactKind::BindingReconcile {
                    items: value.live_pane_ids.len().max(value.desired_bindings.len()),
                }
            }
        };
        let Some(payload) =
            AppAgentStateExactRequest::try_new(Arc::clone(&self.agent_state_scope), kind)
        else {
            tracing::warn!(
                kind = "agent_state",
                phase = "exact_stage",
                error_code = "resource_limit",
                "agent state exact request rejected"
            );
            return false;
        };
        self.stage_prepared_agent_state_exact(worker_kind, payload)
    }

    fn request_project_name_projection(&mut self) {
        self.project_name_projection_dirty = true;
    }

    fn admit_agent_state_once(&mut self) {
        use crate::agent_state_worker::AdmissionError;

        if self.agent_state_admission_blocked
            || self.agent_state_worker.has_in_flight()
            || (self.agent_state_worker.pending_exact_count() == 0
                && self.agent_state_worker.pending_projection_count() == 0)
        {
            return;
        }
        match self.agent_state_worker.admit() {
            Ok(()) | Err(AdmissionError::Busy | AdmissionError::Empty) => {}
            Err(AdmissionError::WorkerUnavailable) => {
                // The first failure is proven known-unsent and restores the complete job. Retry
                // exactly once now; a second failure retains the bounded queue fail-closed until
                // a relevant event explicitly re-arms admission.
                if let Err(error) = self.agent_state_worker.admit()
                    && matches!(
                        error,
                        AdmissionError::WorkerUnavailable | AdmissionError::Closed
                    )
                {
                    self.agent_state_admission_blocked = true;
                    tracing::warn!(
                        kind = "agent_state",
                        phase = "admission",
                        error_code = "worker_unavailable",
                        "agent state worker admission failed closed"
                    );
                }
            }
            Err(AdmissionError::Closed) => {
                self.agent_state_admission_blocked = true;
            }
        }
    }

    fn stage_project_name_projection(&mut self) -> bool {
        const PROJECT_NAME_ITEMS_MAX: usize = 256;
        if !self.agent_state_scope_ready() {
            return false;
        }
        let mut rows = Vec::with_capacity(self.session_cwds.len().min(PROJECT_NAME_ITEMS_MAX));
        let mut seen_cwds = std::collections::HashSet::new();
        for (session, cwd) in &self.session_cwds {
            if rows.len() >= PROJECT_NAME_ITEMS_MAX {
                break;
            }
            if cwd.is_empty() || cwd.len() > 4 * 1024 || cwd.as_bytes().contains(&0) {
                continue;
            }
            seen_cwds.insert(cwd.clone());
            rows.push(AppProjectNameRequest {
                session: Some(*session),
                cwd: cwd.clone(),
            });
        }
        for (_, cwd) in self
            .persisted_activity_panes
            .values()
            .flat_map(|panes| panes.iter())
        {
            if rows.len() >= PROJECT_NAME_ITEMS_MAX {
                break;
            }
            if cwd.is_empty()
                || cwd.len() > 4 * 1024
                || cwd.as_bytes().contains(&0)
                || !seen_cwds.insert(cwd.clone())
            {
                continue;
            }
            rows.push(AppProjectNameRequest {
                session: None,
                cwd: cwd.clone(),
            });
        }
        self.stage_agent_state_projection(
            crate::agent_state_worker::AgentStateSection::ProjectNames,
            AppAgentStateProjectionKind::ProjectNames {
                style: self.config.ui.session_name_style,
                rows,
            },
        )
    }

    /// 전역 attention 세션(global_waiting/global_working/global_turn_done)의 liveness 필터
    /// — 활성 워크스페이스면 active.session_titles, 아니면 warm runtime의
    /// session_titles로 살아있는 세션인지 확인한다(세 곳에 복붙되던 판정을 한 곳으로).
    fn attention_session_alive(&self, workspace_id: &str, session: runtime::SessionId) -> bool {
        if workspace_id == self.active.id {
            self.active.session_titles.contains_key(&session)
        } else {
            self.warm
                .get(workspace_id)
                .is_some_and(|runtime| runtime.session_titles.contains_key(&session))
        }
    }

    /// hook이 보고한 턴 시작을 해당 워크스페이스 런타임의 status detector에 전달한다.
    /// regex 결과 상태(Error/Done)는 latch라 해제 경로가 on_input(=그 pane에 직접 타이핑)
    /// 하나뿐이었고, 그래서 error regex 오탐 한 번이 무기한 남았다. 턴 경계는 입력과
    /// 동등한 리셋 신호다. warm 워크스페이스도 자기 런타임 핸들로 그대로 전달한다.
    ///
    /// 호출부는 "대기(waiting)/완료(turn_done)였다가 작업 중(working)이 된" 전이만
    /// 넘긴다 — working 재진입 자체는 턴 경계가 아니다(2분 stale 창 만료 후의 하트비트
    /// 재개일 수 있다). Stop hook이 유실된 턴(Ctrl-C 등)은 이 전이를 못 만들어 latch가
    /// 남지만, 그건 종전과 같은 상태라 회귀는 아니다.
    fn note_turn_starts(&self, sessions: &[(String, runtime::SessionId)]) {
        for (workspace_id, session) in sessions {
            let runtime = if workspace_id == &self.active.id {
                Some(&self.active.runtime)
            } else {
                self.warm.get(workspace_id).map(|warm| &warm.runtime)
            };
            if let Some(runtime) = runtime {
                let _ = runtime
                    .send_command(runtime::RuntimeCommand::NoteTurnStart { session: *session });
            }
        }
    }

    fn apply_agent_state_storage_projection(
        &mut self,
        section: crate::agent_state_worker::AgentStateSection,
        snapshot: &storage::AgentStateSnapshot,
    ) {
        let workspace_prefix = format!("{}:", self.agent_state_scope.workspace_id);
        let session_id = |key: &str| {
            key.strip_prefix(&workspace_prefix)
                .and_then(|value| value.parse::<u64>().ok())
                .map(runtime::SessionId)
        };
        match section {
            crate::agent_state_worker::AgentStateSection::Hooks => {
                self.hook_overrides = snapshot
                    .hook_sessions
                    .iter()
                    .filter_map(|row| {
                        Some((
                            session_id(&row.session_key)?,
                            crate::agent_detect::AgentBinding {
                                kind: crate::agent_detect::kind_from_str(&row.kind)?,
                                session_id: row.agent_session_id.clone(),
                                transcript: PathBuf::from(&row.transcript_path),
                            },
                        ))
                    })
                    .collect();
                self.statuslines = snapshot
                    .statuslines
                    .iter()
                    .filter_map(|row| Some((session_id(&row.session_key)?, row.clone())))
                    .collect();
                self.push_agent_display();
            }
            crate::agent_state_worker::AgentStateSection::Attention => {
                // 턴 경계 판정용 직전 스냅샷 — "대기/완료였다가 작업 중"만 새 턴으로 본다
                // (note_turn_starts 주석 참조). 아래에서 덮어쓰기 전에 떠 둔다.
                let was_blocked: std::collections::HashSet<(String, runtime::SessionId)> = self
                    .global_waiting
                    .iter()
                    .map(|(workspace_id, session, _)| (workspace_id.clone(), *session))
                    .chain(self.global_turn_done.keys().cloned())
                    .collect();
                self.agent_needs_input = snapshot
                    .waiting_sessions
                    .iter()
                    .filter_map(|(key, _)| session_id(key))
                    .collect();
                self.global_waiting = snapshot
                    .waiting_sessions
                    .iter()
                    .filter_map(|(key, message)| {
                        let (workspace_id, session) = ui::inbox_waiting::parse_session_key(key)?;
                        self.attention_session_alive(&workspace_id, session)
                            .then(|| (workspace_id, session, message.clone()))
                    })
                    .collect();
                self.agent_turn_done = snapshot
                    .turn_done_sessions
                    .iter()
                    .filter_map(|(key, at)| Some((session_id(key)?, *at)))
                    .collect();
                // turn_done 전역화(warm turn_done 격차, 감사 발견) — global_waiting/
                // global_working과 동일 규칙(liveness 필터, 살아있는 세션만). 값(updated_at)은
                // 워크스페이스별 subset 소비 시 조건부 clear의 세대 기준으로 그대로 쓴다.
                self.global_turn_done = snapshot
                    .turn_done_sessions
                    .iter()
                    .filter_map(|(key, at)| {
                        let (workspace_id, session) = ui::inbox_waiting::parse_session_key(key)?;
                        self.attention_session_alive(&workspace_id, session)
                            .then_some(((workspace_id, session), *at))
                    })
                    .collect();
                // hook 기반 "작업 중"(v32) — 활성 전용 set + 전 워크스페이스(liveness 필터,
                // global_waiting과 동일 규칙: 살아있는 세션만).
                self.agent_working = snapshot
                    .working_sessions
                    .iter()
                    .filter_map(|key| session_id(key))
                    .collect();
                let working_now: std::collections::HashSet<(String, runtime::SessionId)> = snapshot
                    .working_sessions
                    .iter()
                    .filter_map(|key| {
                        let (workspace_id, session) = ui::inbox_waiting::parse_session_key(key)?;
                        self.attention_session_alive(&workspace_id, session)
                            .then_some((workspace_id, session))
                    })
                    .collect();
                let turn_started =
                    turn_start_transitions(&working_now, &self.global_working, &was_blocked);
                self.global_working = working_now;
                self.note_turn_starts(&turn_started);
            }
            crate::agent_state_worker::AgentStateSection::Restore => {
                let rows = snapshot
                    .agent_sessions
                    .iter()
                    .cloned()
                    .map(|row| (row.pane_id.clone(), row))
                    .collect::<std::collections::HashMap<_, _>>();
                self.restore_agents = rows.clone();
                self.persisted_agents = rows;
                self.restore_loaded_for = Some(self.active.id.clone());
                self.resumed_panes.clear();
            }
            crate::agent_state_worker::AgentStateSection::BindingSync => {
                let rows = snapshot
                    .agent_sessions
                    .iter()
                    .cloned()
                    .map(|row| (row.pane_id.clone(), row))
                    .collect::<std::collections::HashMap<_, _>>();
                self.persisted_agents = rows.clone();
                self.restore_agents = rows;
            }
            crate::agent_state_worker::AgentStateSection::Catalog => {
                let rows = snapshot
                    .structured_threads
                    .iter()
                    .map(|row| ui::agent_sessions::AgentSessionPersistedRow {
                        local_session_id: row.local_session_id.clone(),
                        workspace_id: row.workspace_id.clone(),
                        thread_id: row.thread_id.clone(),
                        title: row.title.clone(),
                        cwd: row.cwd.clone(),
                        model: row.model.clone(),
                        favorite: row.favorite,
                        archived: row.archived,
                        created_at: row.created_at,
                        updated_at: row.updated_at,
                    })
                    .collect();
                if self
                    .agent_sessions_ui
                    .replace_persisted_threads(rows)
                    .is_err()
                {
                    tracing::warn!(
                        kind = "agent_state",
                        phase = "catalog_apply",
                        error_code = "invalid_data",
                        "agent state catalog projection rejected"
                    );
                }
                let mut by_workspace: std::collections::HashMap<String, Vec<(String, String)>> =
                    std::collections::HashMap::new();
                for (workspace_id, title, cwd) in &snapshot.activity_panes {
                    by_workspace
                        .entry(workspace_id.clone())
                        .or_default()
                        .push((title.clone(), cwd.clone()));
                }
                self.persisted_activity_panes = by_workspace;
                self.request_project_name_projection();
            }
            crate::agent_state_worker::AgentStateSection::ResumeProbe
            | crate::agent_state_worker::AgentStateSection::ProjectNames => {}
        }
    }

    fn apply_project_name_results(
        &mut self,
        revision: u64,
        style: crate::config::SessionNameStyle,
        results: &[AppProjectNameResult],
    ) {
        if style != self.config.ui.session_name_style {
            return;
        }
        let mut activity_names = std::collections::HashMap::with_capacity(results.len());
        let mut session_names = Vec::new();
        for result in results {
            activity_names.insert(result.cwd.clone(), result.name.clone());
            if let (Some(session), Some(name)) = (result.session, result.name.clone()) {
                session_names.push((session, result.cwd.clone(), name));
            }
        }
        if let Ok(snapshot) =
            ui::workspace::SessionProjectNameSnapshot::try_new(revision, session_names)
        {
            self.activity_project_names = activity_names;
            self.activity_project_name_style = style;
            self.active.workspace_ui.set_session_project_names(snapshot);
            if let Some(cwd) = self
                .active
                .workspace_ui
                .focused_session()
                .and_then(|session| self.session_cwds.get(&session))
                .cloned()
            {
                self.update_workspace_folder_name(&cwd);
            }
        }
    }

    fn apply_resume_probe_results(&mut self, results: &[AppResumeProbeResult]) {
        for result in results {
            self.resume_probe_pending_panes.remove(&result.pane_id);
            let identity_is_current =
                self.restore_agents
                    .get(&result.pane_id)
                    .is_some_and(|saved| {
                        saved.kind == result.identity.kind
                            && saved.session_id == result.identity.session_id
                    });
            let pane_is_current = self
                .active
                .workspace_ui
                .mux()
                .and_then(|mux| pane_of_session(mux, result.session))
                .is_some_and(|pane| pane.0 == result.pane_id);
            if !identity_is_current || !pane_is_current {
                continue;
            }
            if !result.found {
                if self.stage_agent_state_exact(AppAgentStateExactKind::BindingDelete(
                    result.identity.clone(),
                )) {
                    let title = self.activity_session_name(&self.active.id, &result.pane_title);
                    let message = self
                        .i18n
                        .t("workspace.wake.resume_missing", &[("title", &title)]);
                    self.set_web_notice(Some(message));
                    self.egui_ctx.request_repaint_after(Self::WEB_NOTICE_TTL);
                }
                continue;
            }
            let source_state = dotenv_state_for_root(self.active_tree_root().as_deref());
            if self.active.session_dotenv_states.get(&result.session) != Some(&source_state) {
                continue;
            }
            let live_process_count = self
                .active
                .session_resource_usage
                .iter()
                .find(|usage| usage.session == result.session)
                .and_then(|usage| usage.pid.map(|_| usage.process_count));
            if !resume_probe_completion_allowed(
                result.manual,
                self.resumed_panes.contains(&result.pane_id),
                self.agent_bindings.contains_key(&result.session),
                live_process_count,
            ) {
                continue;
            }
            let cd_prefix = result
                .cwd
                .as_deref()
                .map(|cwd| format!("cd {} && ", crate::agent_hooks::sh_quote(cwd)))
                .unwrap_or_default();
            let command = match result.identity.kind.as_str() {
                "claude" => format!(
                    "{cd_prefix}claude --resume {}\n",
                    result.identity.session_id
                ),
                "codex" => format!("{cd_prefix}codex resume {}\n", result.identity.session_id),
                _ => continue,
            };
            self.active.workspace_ui.clear_selection(result.session);
            if self
                .active
                .runtime
                .send_command(runtime::RuntimeCommand::WriteInput {
                    session: result.session,
                    bytes: command.into_bytes(),
                })
                .is_ok()
            {
                self.resumed_panes.insert(result.pane_id.clone());
            }
        }
    }

    fn poll_agent_state_worker(&mut self) {
        while let Ok(mut outcome) = self.agent_state_worker.try_recv() {
            if let Some(exact) = outcome.take_exact() {
                let (continuation, result) = exact.into_parts();
                if let Err(error) = result {
                    tracing::warn!(
                        kind = "agent_state",
                        phase = "exact_complete",
                        error_code = error.as_str(),
                        "agent state exact request failed"
                    );
                } else {
                    if let AppAgentStateExactKind::BindingDelete(identity) =
                        &continuation.payload().kind
                        && self
                            .restore_agents
                            .get(&identity.pane_id)
                            .is_some_and(|saved| {
                                saved.kind == identity.kind
                                    && saved.session_id == identity.session_id
                            })
                    {
                        self.restore_agents.remove(&identity.pane_id);
                        self.persisted_agents.remove(&identity.pane_id);
                        self.resumed_panes.remove(&identity.pane_id);
                    }
                }
            }
            for projection in outcome.into_projections() {
                let section = projection.section();
                let key = projection.key();
                if section == crate::agent_state_worker::AgentStateSection::ProjectNames {
                    self.project_name_projection_pending = false;
                }
                if section == crate::agent_state_worker::AgentStateSection::ResumeProbe {
                    self.resume_probe_pending_panes.clear();
                }
                // Scope transitions retain metadata only until every old-scope payload has
                // settled. Coalescible old projections are deliberately discarded: applying a
                // resume completion could derive a new exact delete while staging is barred,
                // and all state projections are requested again immediately after cutover.
                if self.pending_agent_state_scope.is_some() {
                    continue;
                }
                if key.workspace_epoch() != self.agent_state_scope.epoch {
                    continue;
                }
                match projection.into_result() {
                    Ok(snapshot)
                        if snapshot.scope.as_ref() == self.agent_state_scope.as_ref()
                            && snapshot.scope.workspace_id == self.active.id =>
                    {
                        if let Some(storage) = snapshot.storage.as_ref() {
                            self.apply_agent_state_storage_projection(section, storage);
                        }
                        if section == crate::agent_state_worker::AgentStateSection::ResumeProbe
                            && let Some(results) = snapshot.resume.as_deref()
                        {
                            self.apply_resume_probe_results(results);
                        }
                        if section == crate::agent_state_worker::AgentStateSection::ProjectNames
                            && let Some(results) = snapshot.project_names.as_deref()
                            && let Some(style) = snapshot.project_name_style
                        {
                            self.apply_project_name_results(key.revision(), style, results);
                        }
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!(
                        kind = "agent_state",
                        phase = "projection_complete",
                        error_code = error.as_str(),
                        "agent state projection failed"
                    ),
                }
            }
        }

        if self.pending_agent_state_scope.is_some() {
            if !self.agent_state_worker.has_in_flight()
                && (self.agent_state_worker.pending_exact_count() > 0
                    || self.agent_state_worker.pending_projection_count() > 0)
            {
                self.admit_agent_state_once();
                return;
            }
            if self.agent_state_worker.pending_exact_count() == 0
                && self.agent_state_worker.pending_projection_count() == 0
                && !self.agent_state_worker.has_in_flight()
            {
                self.agent_state_scope = self
                    .pending_agent_state_scope
                    .take()
                    .expect("pending scope exists");
                self.agent_state_next_revision = 0;
                self.restore_loaded_for = None;
                self.resume_probe_pending_panes.clear();
                self.project_name_projection_pending = false;
                self.project_name_projection_dirty = true;
                self.stage_agent_state_projection(
                    crate::agent_state_worker::AgentStateSection::Hooks,
                    AppAgentStateProjectionKind::Hooks,
                );
                self.stage_agent_state_projection(
                    crate::agent_state_worker::AgentStateSection::Attention,
                    AppAgentStateProjectionKind::Attention,
                );
                self.stage_agent_state_projection(
                    crate::agent_state_worker::AgentStateSection::Restore,
                    AppAgentStateProjectionKind::Restore,
                );
                self.stage_agent_state_projection(
                    crate::agent_state_worker::AgentStateSection::Catalog,
                    AppAgentStateProjectionKind::Catalog,
                );
            }
        }
        if self.project_name_projection_dirty
            && !self.project_name_projection_pending
            && self.stage_project_name_projection()
        {
            self.project_name_projection_dirty = false;
            self.project_name_projection_pending = true;
        }
        if !self.agent_state_worker.has_in_flight()
            && (self.agent_state_worker.pending_exact_count() > 0
                || self.agent_state_worker.pending_projection_count() > 0)
        {
            self.admit_agent_state_once();
        }
    }

    /// 에이전트 감지 워커의 입력 갱신 + 결과 드레인 — ui()가 아닌 logic()에서 돈다.
    /// hidden/minimized로 ui()가 스킵돼도 결과를 소비해 unbounded 채널 누적을 막는다
    /// (§14.1 Warm: 창이 안 보이면 logic()만 호출됨, codex 리뷰). UI(egui)에 의존하지 않는
    /// 순수 상태 갱신이라 logic()이 올바른 위치다.
    fn poll_agent_detect(&mut self) {
        // 워커 입력(활성 세션 pid 목록 + epoch)을 최신값으로 갱신 — 워커가 다음 tick에 읽어
        // ps/lsof/transcript 스캔을 UI 스레드 밖에서 수행한다.
        let sessions: Vec<(runtime::SessionId, u32)> = self
            .active
            .session_resource_usage
            .iter()
            .filter_map(|r| r.pid.map(|pid| (r.session, pid)))
            .collect();
        // 터미널 경로 더블클릭의 상대경로 해석용 — 같은 목록을 workspace UI에도 나른다.
        self.active.workspace_ui.set_session_pids(&sessions);
        // 감지할 세션이 없으면 hook/statusline DB에도 접근하지 않는다. 캐시를 비워 두면
        // empty input의 latest-only worker가 thread/backend/repaint 모두 유휴 상태로 남는다.
        let bounded_refresh_due =
            agent_hook_query_due(sessions.len(), self.last_hook_query.elapsed());
        if sessions.is_empty() {
            self.hook_overrides.clear();
            self.statuslines.clear();
        } else if bounded_refresh_due {
            self.last_hook_query = std::time::Instant::now();
            self.stage_agent_state_projection(
                crate::agent_state_worker::AgentStateSection::Hooks,
                AppAgentStateProjectionKind::Hooks,
            );
        }
        let _ = self.agent_detect_input.publish(
            self.agent_detect_epoch,
            sessions,
            &self.hook_overrides,
            // 창 숨김(가림/최소화) — detect 스레드가 ps/lsof/transcript 폴링을 완화한다.
            !self.active.render_active,
        );
        // capacity-one 결과를 논블로킹 소비한다(epoch 불일치=전환 잔여는 폐기).
        let mut latest_bindings = None;
        let mut latest_activity = None;
        let mut latest_cwds = None;
        let mut latest_info = None;
        if let Ok(outcome) = self.agent_detect_rx.try_recv()
            && outcome.epoch == self.agent_detect_epoch
        {
            latest_activity = Some(outcome.activity.clone());
            if outcome.bindings.is_some() {
                latest_bindings = outcome.bindings.clone();
            }
            if outcome.session_cwds.is_some() {
                latest_cwds = outcome.session_cwds.clone();
            }
            if outcome.agent_info.is_some() {
                latest_info = outcome.agent_info.clone();
            }
        }
        if let Some(info) = latest_info {
            self.agent_info = info;
        }
        // 에이전트 표시정보 최종본(claude는 statusLine으로 effort/model/context 병합) →
        // WorkspaceUi. statuslines가 매 1s 갱신되므로 매 poll에서 병합해 최신을 반영한다.
        self.push_agent_display();
        if let Some(cwds) = latest_cwds {
            // 변경된 세션 cwd만 워커 persist로 — 재시작 복원이 pane별 원래 폴더에서
            // 셸을 띄우게 한다(A안 2026-07-08). 같은 값은 워커 쪽에서도 no-op이지만
            // 여기서 걸러 wire 트래픽을 줄인다.
            for (session, cwd) in &cwds {
                if self.session_cwds.get(session) != Some(cwd) {
                    let _ = self.active.runtime.send_command(
                        runtime::RuntimeCommand::UpdateSessionCwd {
                            session: *session,
                            cwd: cwd.clone(),
                        },
                    );
                }
            }
            self.session_cwds = cwds;
            // 세션 행/pane 헤더 1행 폴더명 원천 — WorkspaceUi에 전달.
            self.active
                .workspace_ui
                .set_session_cwds(self.session_cwds.clone(), self.config.ui.session_name_style);
            self.request_project_name_projection();
            // Rename recovery is driven by a fresh detector projection. There is no periodic
            // filesystem stat or repaint when session cwd state is idle.
            self.detect_workspace_folder_rename();
            // 포커스 세션 cwd → 워크스페이스 이름(현재 작업 폴더/프로젝트명).
            if let Some(cwd) = self
                .active
                .workspace_ui
                .focused_session()
                .and_then(|sid| self.session_cwds.get(&sid))
                .cloned()
            {
                self.update_workspace_folder_name(&cwd);
            }
        }
        if let Some(activity) = latest_activity {
            self.agent_activity = activity;
            self.stage_agent_state_projection(
                crate::agent_state_worker::AgentStateSection::Attention,
                AppAgentStateProjectionKind::Attention,
            );
        }
        let has_new_bindings = latest_bindings.is_some();
        if should_process_agent_bindings(
            has_new_bindings,
            self.restore_loaded_for.as_deref(),
            &self.active.id,
            bounded_refresh_due,
        ) {
            let bindings = latest_bindings.unwrap_or_else(|| self.agent_bindings.clone());
            self.agent_bindings = bindings.clone();
            self.process_agent_bindings(&bindings);
        }
    }

    /// 워커 raw(agent_info) + claude statusLine(statuslines)을 병합해 최종 표시정보를
    /// WorkspaceUi에 넘긴다. claude는 statusLine의 effort/model/context%를 우선(정확),
    /// 없으면 transcript 값. codex는 raw 그대로.
    fn push_agent_display(&mut self) {
        use crate::agent_detect::{AgentDisplay, AgentKind};
        let mut merged: std::collections::HashMap<runtime::SessionId, AgentDisplay> =
            self.agent_info.clone();
        for (sid, d) in merged.iter_mut() {
            if d.kind == AgentKind::Claude
                && let Some(sl) = self.statuslines.get(sid)
            {
                if sl.effort.is_some() {
                    d.effort = sl.effort.clone();
                }
                if sl.model.is_some() {
                    d.model = sl.model.clone(); // "Opus 4.8 (1M context)" — transcript보다 나음
                }
                if let Some(pct) = sl.context_pct {
                    d.context_pct = Some(pct.clamp(0, 100) as u8);
                }
            }
        }
        self.active.workspace_ui.set_agent_info(merged);
    }

    fn pty_agent_surfaces(
        &self,
        entries: &[ui::file_tree::SessionEntry],
    ) -> Vec<crate::agent_surface::AgentSurfaceSnapshot> {
        entries
            .iter()
            .filter_map(|entry| {
                let session_id = entry.session?;
                let info = self.agent_info.get(&session_id)?;
                Some(crate::agent_surface::AgentSurfaceSnapshot {
                    id: crate::agent_surface::AgentSurfaceId::Pty {
                        workspace_id: self.active.id.clone(),
                        pane_id: entry.pane.0.clone(),
                        session_id,
                    },
                    provider: crate::agent_surface::AgentProvider::from(info.kind),
                    transport: crate::agent_surface::AgentTransport::Pty,
                    title: entry.title.clone(),
                    model: info.model.clone(),
                    effort: info.effort.clone(),
                    context_pct: info.context_pct,
                    state: crate::agent_surface::AgentVisualState::from_pty(entry.status),
                })
            })
            .collect()
    }

    /// 완료/입력대기 주목(attention) 추적 — 세션 엔트리에 attention/pulse를 채운다.
    /// 규칙(2026-07-07): 알림 발생 시 그 pane이 비포커스면 확인할 때까지 레일 6px 유지,
    /// 이미 포커스 중이면 6px 대신 1회 펄스. 완료는 확인 시 소비(DB clear → 유휴로 복귀).
    fn update_session_alerts(&mut self, entries: &mut [ui::file_tree::SessionEntry]) {
        use runtime::SessionStatus as S;
        const PULSE_SECS: f32 = 0.9;
        let mut any_pulse = false;
        for entry in entries.iter_mut() {
            let Some(sid) = entry.session else { continue };
            let alert_status = match entry.status {
                Some(s @ (S::Done | S::NeedsApproval)) => Some(s),
                _ => None,
            };
            match alert_status {
                Some(status) => {
                    let is_new = self
                        .session_alerts
                        .get(&sid)
                        .is_none_or(|a| a.status != status);
                    if is_new {
                        // 새 알림: 보고 있으면 펄스 1회, 아니면 미확인(6px)으로 시작.
                        self.session_alerts.insert(
                            sid,
                            SessionAlert {
                                status,
                                seen: entry.focused,
                                pulse_started: entry.focused.then(std::time::Instant::now),
                            },
                        );
                    }
                    let alert = self.session_alerts.get_mut(&sid).expect("방금 삽입/존재");
                    // 확인: 포커스가 오면 seen 처리. 완료는 소비해 유휴로 되돌린다.
                    if entry.focused && !alert.seen {
                        alert.seen = true;
                    }
                    if alert.seen
                        && status == S::Done
                        && let Some(&seen_at) = self.agent_turn_done.get(&sid)
                        && self.pending_turn_done_clear.is_none()
                    {
                        // 내가 읽은 세대(seen_at)까지만 소비 — 그 뒤 도착한 새 완료는 남긴다.
                        let key = format!("{}:{}", self.active.id, sid.0);
                        self.pending_turn_done_clear = Some((key, seen_at));
                        self.agent_turn_done.remove(&sid);
                    }
                    entry.attention = !alert.seen;
                    if let Some(started) = alert.pulse_started {
                        let t = started.elapsed().as_secs_f32() / PULSE_SECS;
                        if t < 1.0 {
                            entry.pulse = Some((
                                t,
                                ui::file_tree::session_status_color(
                                    Some(status),
                                    &egui::Visuals::dark(),
                                ),
                            ));
                            any_pulse = true;
                        } else {
                            alert.pulse_started = None;
                        }
                    }
                }
                None => {
                    self.session_alerts.remove(&sid);
                }
            }
        }
        if any_pulse {
            // 펄스 애니메이션 프레임 지속 — 끝나면 자연히 유휴 리페인트로 복귀.
            self.egui_ctx.request_repaint();
        }
    }

    /// 바인딩 감지 결과를 소비한다 — 저장(차등 upsert/delete) + 복원 resume 주입. 워커
    /// 스레드에서 계산된 bindings를 받아 UI 스레드(여기)에서 부수효과만 처리한다(codex #3).
    fn process_agent_bindings(
        &mut self,
        bindings: &std::collections::HashMap<runtime::SessionId, crate::agent_detect::AgentBinding>,
    ) {
        let mux = self.active.workspace_ui.mux().cloned();
        let current: std::collections::HashMap<String, storage::AgentSessionRow> = bindings
            .iter()
            .filter_map(|(sid, b)| {
                let pane = mux.as_ref().and_then(|m| pane_of_session(m, *sid))?;
                let kind = match b.kind {
                    crate::agent_detect::AgentKind::Claude => "claude",
                    crate::agent_detect::AgentKind::Codex => "codex",
                };
                Some((
                    pane.0.clone(),
                    storage::AgentSessionRow {
                        pane_id: pane.0,
                        kind: kind.to_owned(),
                        session_id: b.session_id.clone(),
                    },
                ))
            })
            .collect();
        if let Some(mux) = &mux {
            let live_pane_ids = mux
                .tabs
                .iter()
                .flat_map(|tab| tab.panes.iter().map(|pane| pane.id.0.clone()))
                .collect::<Vec<_>>();
            if !live_pane_ids.is_empty() {
                self.stage_agent_state_projection(
                    crate::agent_state_worker::AgentStateSection::BindingSync,
                    AppAgentStateProjectionKind::BindingSync(
                        storage::AgentSessionBindingReconcile {
                            live_pane_ids,
                            desired_bindings: current.values().cloned().collect(),
                        },
                    ),
                );
            }
        }
        if self.restore_loaded_for.as_deref() != Some(self.active.id.as_str()) {
            self.stage_agent_state_projection(
                crate::agent_state_worker::AgentStateSection::Restore,
                AppAgentStateProjectionKind::Restore,
            );
            return;
        }

        // 복원 resume 주입: workspace 활성화 시 저장된 에이전트가 있고 **셸만** 살아 있는
        // pane에 한해 native resume 명령을 한 번 보낸다(설정으로 끌 수 있다, 기본 ON).
        // 이미 실행 중인 agent나 ssh/tmux/editor 등 다른 child process가 있으면 처리 완료로
        // 표시한다. 이후 그 프로세스가 끝나도 자동 resume이 뒤늦게 끼어들면 안 된다.
        if self.config.ui.auto_resume_agents
            && self.resume_probe_pending_panes.is_empty()
            && let Some(mux) = &mux
        {
            let mut probes = Vec::new();
            let mut candidates = Vec::new();
            for pane in mux.tabs.iter().flat_map(|t| &t.panes) {
                let pane_key = pane.id.0.clone();
                let Some(saved) = self.restore_agents.get(&pane_key).cloned() else {
                    continue;
                };
                let Some(session) = pane.session_id else {
                    continue;
                };
                if self.resume_probe_pending_panes.contains(&pane_key) {
                    continue;
                }
                // ResourceUsage는 background sampler가 셸 pid/process tree를 이미 계산한 값.
                // UI 스레드에서 ps를 새로 spawn하지 않고, snapshot이 없거나 0이면 fail closed.
                let live_process_count = self
                    .active
                    .session_resource_usage
                    .iter()
                    .find(|usage| usage.session == session)
                    .and_then(|usage| usage.pid.map(|_| usage.process_count));
                match auto_resume_decision(
                    self.resumed_panes.contains(&pane_key),
                    bindings.contains_key(&session),
                    live_process_count,
                ) {
                    AutoResumeDecision::Skip | AutoResumeDecision::Wait => continue,
                    AutoResumeDecision::MarkHandled => {
                        self.resumed_panes.insert(pane_key);
                        continue;
                    }
                    AutoResumeDecision::Resume => {}
                }
                let identity = storage::AgentSessionIdentity {
                    pane_id: saved.pane_id.clone(),
                    kind: saved.kind.clone(),
                    session_id: saved.session_id.clone(),
                };
                let Some(kind) = crate::agent_detect::kind_from_str(&saved.kind) else {
                    if self.stage_agent_state_exact(AppAgentStateExactKind::BindingDelete(identity))
                    {
                        self.resumed_panes.insert(pane_key);
                    }
                    continue;
                };
                let Ok(probe) = crate::agent_detect::ResumeTranscriptProbe::try_new(
                    kind,
                    saved.session_id.clone(),
                ) else {
                    if self.stage_agent_state_exact(AppAgentStateExactKind::BindingDelete(identity))
                    {
                        self.resumed_panes.insert(pane_key);
                    }
                    continue;
                };
                probes.push(probe);
                candidates.push(AppResumeCandidate {
                    pane_id: pane_key,
                    pane_title: pane.title.clone(),
                    session,
                    identity,
                    manual: false,
                });
            }
            if !candidates.is_empty() {
                let pending_panes = candidates
                    .iter()
                    .map(|candidate| candidate.pane_id.clone())
                    .collect::<Vec<_>>();
                if self.stage_agent_state_projection(
                    crate::agent_state_worker::AgentStateSection::ResumeProbe,
                    AppAgentStateProjectionKind::ResumeProbe { probes, candidates },
                ) {
                    self.resume_probe_pending_panes.extend(pending_panes);
                }
            }
        }
    }

    /// A manual resume follows the same bounded off-thread transcript probe as auto-resume.
    fn stage_agent_resume(
        &mut self,
        pane_key: &str,
        pane_title: &str,
        session: runtime::SessionId,
    ) -> bool {
        if !self.resume_probe_pending_panes.is_empty() {
            return false;
        }
        // Manual resume is an explicit user event and may re-arm one bounded worker admission.
        self.agent_state_admission_blocked = false;
        let Some(saved) = self.restore_agents.get(pane_key).cloned() else {
            return false;
        };
        let source_state = dotenv_state_for_root(self.active_tree_root().as_deref());
        if self.active.session_dotenv_states.get(&session) != Some(&source_state) {
            tracing::warn!(
                kind = "agent_resume",
                phase = "source_verify",
                error_code = "stale_session_env",
                "agent resume into a stale shell environment was rejected"
            );
            return false;
        }
        let identity = storage::AgentSessionIdentity {
            pane_id: saved.pane_id.clone(),
            kind: saved.kind.clone(),
            session_id: saved.session_id.clone(),
        };
        let Some(kind) = crate::agent_detect::kind_from_str(&saved.kind) else {
            let _ = self.stage_agent_state_exact(AppAgentStateExactKind::BindingDelete(identity));
            return false;
        };
        let Ok(probe) =
            crate::agent_detect::ResumeTranscriptProbe::try_new(kind, saved.session_id.clone())
        else {
            let _ = self.stage_agent_state_exact(AppAgentStateExactKind::BindingDelete(identity));
            return false;
        };
        let staged = self.stage_agent_state_projection(
            crate::agent_state_worker::AgentStateSection::ResumeProbe,
            AppAgentStateProjectionKind::ResumeProbe {
                probes: vec![probe],
                candidates: vec![AppResumeCandidate {
                    pane_id: pane_key.to_owned(),
                    pane_title: pane_title.to_owned(),
                    session,
                    identity,
                    manual: true,
                }],
            },
        );
        if staged {
            self.resume_probe_pending_panes.insert(pane_key.to_owned());
        }
        staged
    }

    /// 세션의 현재 작업 폴더 — 감지 워커 캐시 우선, 없으면 pid로 일회성 lsof 조회
    /// (사용자 클릭 시점의 1회 조회라 스폰 비용 감수 — platform::process_cwd 관례).
    fn cached_session_cwd(&self, session: runtime::SessionId) -> Option<String> {
        self.session_cwds.get(&session).cloned()
    }

    /// shim PATH env — hook 토글 ON이고 shim이 설치돼 있으면 셸 PATH 앞에 주입한다.
    fn shim_shell_env(config: &Config) -> Vec<(String, String)> {
        // .env 라이브 반영(E5 ⑨): zsh ZDOTDIR 훅 — 래퍼는 항상 주입(passthrough,
        // 기능 OFF면 no-op)하고 활성 조건은 세션 기본 env가 동적으로 나른다.
        let mut env = crate::env_reload::shell_env();
        if !config.ui.agent_status_hooks {
            return env;
        }
        let Some(dir) = crate::agent_shim::shim_dir() else {
            return env;
        };
        let path = std::env::var("PATH").unwrap_or_default();
        env.push(("PATH".to_owned(), format!("{}:{path}", dir.display())));
        env
    }

    /// 에이전트 상태 hook을 설정 토글에 맞춰 전역 설치/해제한다(옵션2 needsInput).
    /// best-effort — 실패해도 앱은 정상 동작(regex fallback). claude + codex.
    fn sync_agent_hooks(&self) {
        let result = (|| -> anyhow::Result<()> {
            // 전역 config 방식(구)은 항상 정리한다 — shim 방식으로 전환(cmux식, 2026-07-07).
            crate::agent_hooks::uninstall_claude()?;
            crate::agent_hooks::uninstall_codex()?;
            if self.config.ui.agent_status_hooks {
                let bin = mcp_proxy_bin()?;
                crate::agent_shim::install(&self.db_path, &bin)?;
            } else {
                crate::agent_shim::remove()?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            tracing::warn!("에이전트 상태 hook 동기화 실패: {e:#}");
        }
    }

    /// 앱 데이터 디렉터리 (db_path = `<data>/metadata.sqlite3` → parent). remote cert/known_hosts의 기준.
    fn data_dir(&self) -> &std::path::Path {
        self.db_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
    }

    /// remote TLS 서버 신원 인증서 경로 (`<data>/remote-tls.crt` — tls_identity 관례, 키는 keyring).
    fn cert_path(&self) -> PathBuf {
        self.data_dir().join("remote-tls.crt")
    }

    /// 클라이언트 측 known_hosts 파일 경로 (`<data>/known_hosts`).
    fn known_hosts_path(&self) -> PathBuf {
        self.data_dir().join("known_hosts")
    }

    /// remote TLS 서버를 기동한다: 신원 로드/생성 → 전용 원격 worker(비영속) → loopback bind.
    /// **원격 worker는 fresh empty 런타임**(원격 클라가 스스로 세션을 만든다) + PersistConfig=None
    /// (원격 세션은 영속하지 않는다). 실패는 Err — 호출측이 표시하고 앱은 계속(크래시 금지).
    fn start_remote(&self) -> anyhow::Result<RemoteTlsState> {
        let identity =
            runtime::tls_identity::get_or_create_identity(&self.secret_store, &self.cert_path())?;
        let fingerprint = identity.fingerprint();
        // 전용 원격 worker — logs는 logs_base/remote/ 하위(활성 workspace 로그와 분리).
        let worker = self
            .runtime_host_factory
            .create_client(runtime::RuntimeHostConfig {
                output_batch_ms: self.config.performance.output_batch_ms,
                logs_root: self.logs_base.join("remote"),
                persist: None,
                cwd: None,
                extra_env: Vec::new(),
            })?;
        let addr =
            std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, self.config.remote.port));
        // loopback 전용(allow_non_loopback=false) — 비-loopback 개방은 후속 UI(C-4 가드 유지).
        let server = runtime::RemoteRuntimeServer::serve_tls(worker, addr, identity, false)?;
        Ok(RemoteTlsState {
            server,
            fingerprint,
        })
    }

    /// settings 체크 on: 서버를 켜고 성공 시 config에 의도를 영속한다(다음 실행 자동 시작).
    fn remote_enable(&mut self) {
        match self.start_remote() {
            Ok(state) => {
                self.remote = Some(state);
                self.remote_error = None;
                self.config.remote.tls_enabled = true;
                if let Err(e) = self.config.save(&self.config_path) {
                    tracing::warn!("config 저장 실패: {e:#}");
                    // 서버는 켜졌지만 자동시작이 영속되지 않음 — 사용자에게 알린다.
                    self.remote_error = Some(format!(
                        "설정 저장 실패 — 다음 실행엔 자동시작 안 됨: {e:#}"
                    ));
                }
            }
            Err(e) => {
                tracing::warn!("remote TLS 시작 실패: {e:#}");
                self.remote_error = Some(format!("{e:#}"));
            }
        }
    }

    /// settings 체크 off: 서버를 정지(Drop이 accept/접속/worker 정리)하고 config에 영속한다.
    fn remote_disable(&mut self) {
        if let Some(state) = self.remote.take() {
            state.server.shutdown();
        }
        self.remote_error = None;
        self.config.remote.tls_enabled = false;
        if let Err(e) = self.config.save(&self.config_path) {
            tracing::warn!("config 저장 실패: {e:#}");
            // 저장 실패를 조용히 넘기면 config.toml에 tls_enabled=true가 남아, 사용자가 껐다고
            // 생각한 원격 서버(셸 접근 동등)가 다음 실행에 다시 자동시작된다 — 표면화 (codex P2).
            self.remote_error = Some(format!(
                "서버는 껐지만 설정 저장 실패 — 다음 실행에 다시 켜질 수 있습니다: {e:#}"
            ));
        }
    }

    /// 모바일 웹(PWA) 서버 기동 (mobile-pwa v3.3 P1): keyring 페어링 토큰 로드/생성 →
    /// 127.0.0.1 평문 bind(serve 모드 — HTTPS 종단은 tailscale serve 몫).
    /// cert 모드(자체 TLS + 비-loopback)는 후속 — config에 자리만 있다.
    fn start_web(&self) -> anyhow::Result<WebRemoteState> {
        let token = web_remote::pairing::get_or_create_token(&self.secret_store)?;
        // 웹푸시(P4) VAPID 키 — keyring에서 get_or_create(SecretStore 접근이 app 소유). 개인키는
        // keyring에만, 공개키만 서버가 JS에 노출한다. 실패하면 푸시만 비활성(대시보드는 유지).
        let vapid = match web_remote::push::get_or_create_vapid_key(&self.secret_store) {
            Ok(key) => Some(key),
            Err(e) => {
                tracing::warn!("웹푸시 VAPID 키 준비 실패 — 푸시 비활성: {e:#}");
                None
            }
        };
        let addr =
            std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, self.config.web.port));
        let hostname = self.config.web.ts_hostname.trim();
        let repository: Arc<dyn web_remote::repository::WebRemoteRepository> =
            Arc::new(AppWebRemoteRepository::open(&self.db_path)?);
        let server = web_remote::WebRemoteServer::serve(
            addr,
            web_remote::ServeOptions {
                token: token.clone(),
                allowed_host: (!hostname.is_empty()).then(|| hostname.to_owned()),
                // app-owned adapter 한 개를 Dashboard와 Push가 공유한다. web-remote는
                // concrete Db를 생성하거나 storage row를 contract에 노출하지 않는다.
                repository: Some(repository),
                vapid,
                // 모바일 파일 첨부(P6d) — 세션에 묶이지 않는 평면 디렉터리라 workspace별
                // logs_root가 아닌 logs_base 바로 아래에 둔다(remote/ 분리와 같은 이유).
                uploads_dir: Some(self.logs_base.join("uploads")),
            },
        )?;
        // 활성 workspace worker 이벤트를 대시보드에 붙인다(P2). wake 클로저는 egui 프레임과
        // 무관하게 브리지 스레드를 깨운다(§14.1 Warm 알림 유지) — 창이 숨겨져도 상태가 흐른다.
        // 워크스페이스 전환 시엔 rebind_web_dashboard가 새 worker로 재구독한다.
        let receiver = self
            .active
            .runtime
            // background 구독 — 웹 브리지는 렌더와 무관하므로 원격 전용 Viewport에도
            // 깨어나되(시청 프레임 라우팅), GUI repaint는 유발하지 않는다 (P5 리뷰 P1).
            .subscribe_with_wake_background(server.dashboard_wake());
        // 터미널 뷰어(P5)의 시청 lease를 runtime으로 보낼 명령 싱크 — receiver보다 먼저
        // (set_runtime_source의 lease 재선언이 이 싱크로 나간다, rebind와 동일 순서).
        if let Some(sink) = self.active.runtime.command_sink() {
            server.set_runtime_command_sink(sink);
        }
        server.set_runtime_source(receiver);
        // 구독 등록 직후 전체 워크스페이스 스냅샷을 시드한다 — 이벤트 스트림은 edge-trigger라,
        // 재구독한 대시보드는 과거 이력을 모른다. 시드가 없으면 이미 needs_approval로 정착한
        // 세션이 다음 상태 변화까지 "실행 중"으로 오표시된다(계획 P2 리뷰: 킬러 기능 훼손).
        let seeds = self.web_workspace_seed();
        // 재구독 시점에만 활성 세션의 라이브 상태를 시드한다(매 프레임 push와 분리 —
        // 리뷰 P1-1: 프레임 push가 상태를 덮으면 숨김 창에서 stale 값으로 되돌아간다).
        Self::reseed_web_sessions(&server, &seeds);
        server.set_workspaces(seeds);
        // 폰 미러 진입(I1b-2) — 전환 요청을 egui 스레드 큐로 넘기는 싱크. app 레벨이라 한 번만
        // 주입한다(command_sink처럼 워커별 교체 불필요). 웹 스레드에서 불리므로 큐 push +
        // repaint만 하고, 실제 switch_workspace는 ui()가 큐를 drain해 egui 스레드에서 실행한다.
        let switch_queue = Arc::clone(&self.web_switch_queue);
        let switch_ctx = self.egui_ctx.clone();
        server.set_switch_sink(Arc::new(move |workspace: String| {
            if let Ok(mut queue) = switch_queue.lock() {
                queue.push(workspace);
            }
            switch_ctx.request_repaint();
        }));
        Ok(WebRemoteState { server, token })
    }

    /// 워크스페이스 전환 시 웹 대시보드를 새 활성 worker에 재구독시킨다 — 옛 receiver는
    /// 교체와 함께 drop되어 옛 worker가 자기 subscriber를 정리한다(계획 P2 "wake 클로저 수명").
    fn rebind_web_dashboard(&self) {
        if let Some(web) = &self.web {
            let receiver = self
                .active
                .runtime
                .subscribe_with_wake_background(web.server.dashboard_wake());
            // 명령 싱크를 receiver보다 먼저 교체한다 — set_runtime_source의 lease 재선언이
            // 새 worker의 싱크로 나가게 (P5b, 워크스페이스 전환 시 시청 연속성).
            if let Some(sink) = self.active.runtime.command_sink() {
                web.server.set_runtime_command_sink(sink);
            }
            web.server.set_runtime_source(receiver);
            // 재구독 직후 새 워크스페이스 스냅샷을 시드한다(start_web과 동일 이유 + 세션 맵
            // 통째 교체로 옛 워크스페이스 세션 정체/전환 레이스까지 해소 — P2 리뷰).
            let seeds = self.web_workspace_seed();
            Self::reseed_web_sessions(&web.server, &seeds);
            web.server.set_workspaces(seeds);
        }
    }

    /// 재구독 시점 시딩 — 스냅샷에서 활성 워크스페이스의 세션만 골라 넘긴다.
    fn reseed_web_sessions(
        server: &web_remote::WebRemoteServer,
        seeds: &[web_remote::dashboard::WorkspaceSeed],
    ) {
        if let Some(active) = seeds
            .iter()
            .find(|ws| ws.state == web_remote::dashboard::WorkspaceState::Active)
        {
            server.reseed_active_sessions(&active.sessions);
        }
    }

    /// 웹 대시보드용 **전체 워크스페이스** 스냅샷 — 활성 1개 + warm/유휴 N개.
    /// 데스크톱 활동 패널(activity_rows)과 같은 원천·같은 이름 규칙을 쓴다: 세션 표시명은
    /// 기본 제목이면 프로젝트명으로 해석되고(activity_session_name / resolve_session_title),
    /// 사용자가 rename했으면 그대로다 — 폰에서도 "workspace.spawn.shell 140"이 아니라
    /// 사람이 읽는 이름이 보인다.
    ///
    /// 활성 세션만 id를 싣는다(시청/입력 대상). warm/유휴는 표시 전용 — 세션 id는
    /// worker-로컬이라 다른 워크스페이스 id로 시청하면 엉뚱한 세션이 잡힌다(P5 리뷰 P2).
    ///
    /// 활성 워크스페이스 상태는 런타임 감지 이벤트에서만 오므로(앱의 transcript/hook 병합은
    /// 브리지에 안 보임) 병합 맵은 비워 넘겨 순수 감지 상태를 시드한다 — 브리지의 이벤트
    /// 갱신과 일관된다. 브라우저는 innerHTML 금지라 제목 문자열은 그대로 안전하다.
    fn web_workspace_seed(&self) -> Vec<web_remote::dashboard::WorkspaceSeed> {
        use web_remote::dashboard::{SessionSeed, WorkspaceSeed, WorkspaceState};
        let empty_activity = std::collections::HashMap::new();
        let empty_needs_input = std::collections::HashSet::new();
        let empty_turn_done = std::collections::HashMap::new();
        let empty_working = std::collections::HashSet::new();
        self.workspaces
            .iter()
            .filter(|ws| workspace_visible_after_close(&self.closed_workspaces, &ws.id))
            .map(|ws| {
                if ws.id == self.active.id {
                    let sessions = self
                        .active
                        .workspace_ui
                        .session_entries(
                            &self.i18n,
                            &empty_activity,
                            &empty_needs_input,
                            &empty_turn_done,
                            &empty_working,
                        )
                        .into_iter()
                        .filter_map(|entry| {
                            let session = entry.session?;
                            Some(SessionSeed {
                                id: Some(session.0),
                                title: entry.title,
                                status: Some(
                                    entry.status.unwrap_or(runtime::SessionStatus::Running),
                                ),
                                // 돌고 있는 에이전트("Claude · sonnet · high") — 사이드바
                                // 2행과 같은 원천(agent_detect). 셸이면 None.
                                agent: entry.agent_line,
                                exited: self.active.live.exited_sessions.contains(&session),
                            })
                        })
                        .collect();
                    return WorkspaceSeed {
                        id: ws.id.clone(),
                        name: Self::workspace_display_name(ws),
                        state: WorkspaceState::Active,
                        sessions,
                    };
                }
                // 대기(warm)/절전 — 감지 워커가 안 돌아 상태는 없다. 이름만 활동 패널과 동일 규칙.
                // 대기는 에이전트가 살아있어 마지막 감지 정보를 유지·표시하고(방안①), 절전은 죽어
                // 표시하지 않는다.
                let (state, sessions): (WorkspaceState, Vec<SessionSeed>) =
                    match self.warm.get(&ws.id) {
                        Some(rt) => {
                            let mut ids: Vec<_> = rt.session_titles.keys().copied().collect();
                            ids.sort_by_key(|s| s.0);
                            let sessions = ids
                                .iter()
                                .filter_map(|s| {
                                    let raw = rt.session_titles.get(s)?;
                                    Some(SessionSeed {
                                        id: None, // 표시 전용
                                        title: self.activity_session_name(&ws.id, raw),
                                        status: None,
                                        agent: rt.workspace_ui.agent_line_for(*s),
                                        exited: false,
                                    })
                                })
                                .collect();
                            (WorkspaceState::Warm, sessions)
                        }
                        None => {
                            let sessions = self
                                .persisted_activity_panes
                                .get(&ws.id)
                                .into_iter()
                                .flatten()
                                .map(|(title, _cwd)| SessionSeed {
                                    id: None, // 표시 전용
                                    title: self.activity_session_name(&ws.id, title),
                                    status: None,
                                    agent: None, // 절전 — 에이전트 죽음
                                    exited: false,
                                })
                                .collect();
                            (WorkspaceState::Suspended, sessions)
                        }
                    };
                WorkspaceSeed {
                    id: ws.id.clone(),
                    name: Self::workspace_display_name(ws),
                    state,
                    sessions,
                }
            })
            .collect()
    }

    /// 워크스페이스 표시 스냅샷을 웹 대시보드에 반영한다(변화가 없으면 브리지가 무시).
    ///
    /// **상태는 시드하지 않는다** — 활성 세션의 상태는 런타임 이벤트가 소유하는 프레임
    /// 독립 데이터다(리뷰 P1-1). 여기서는 구성·표시명만 보낸다.
    ///
    /// 스냅샷 **구축 비용**(제목 해석의 .git 상향 stat 등)이 프레임마다 들지 않도록
    /// 스로틀한다 — 폰의 ≤1s 반영 요건에 여유가 큰 250ms (리뷰 P2-2).
    fn sync_web_workspaces(&mut self, now: std::time::Instant) {
        const WEB_SYNC_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
        if self.web.is_none() {
            return;
        }
        if let Some(last) = self.last_web_sync
            && now.duration_since(last) < WEB_SYNC_INTERVAL
        {
            return;
        }
        self.last_web_sync = Some(now);
        let seeds = self.web_workspace_seed();
        if let Some(web) = &self.web {
            web.server.set_workspaces(seeds);
        }
    }

    /// settings 토글 on: 웹서버를 켜고 성공 시 config에 의도를 영속한다 (remote_enable 관례).
    fn web_enable(&mut self) {
        match self.start_web() {
            Ok(state) => {
                self.web = Some(state);
                self.web_error = None;
                // 포트가 바뀌었을 수 있다 — serve 진단을 무효화해 다음 프레임에 재진단한다.
                self.serve_state = None;
                self.config.web.enabled = true;
                if let Err(e) = self.config.save(&self.config_path) {
                    tracing::warn!("config 저장 실패: {e:#}");
                    self.web_error = Some(format!(
                        "설정 저장 실패 — 다음 실행엔 자동시작 안 됨: {e:#}"
                    ));
                }
            }
            Err(e) => {
                tracing::warn!("모바일 웹 서버 시작 실패: {e:#}");
                self.web_error = Some(format!("{e:#}"));
            }
        }
    }

    /// settings 토글 off: 웹서버 정지(Drop이 accept/접속 스레드 정리) + config 영속.
    fn web_disable(&mut self) {
        if let Some(state) = self.web.take() {
            state.server.shutdown();
        }
        self.web_error = None;
        self.serve_state = None; // 서버가 없으면 진단은 의미 없다
        self.config.web.enabled = false;
        if let Err(e) = self.config.save(&self.config_path) {
            tracing::warn!("config 저장 실패: {e:#}");
            // 저장 실패를 조용히 넘기면 껐다고 생각한 서버가 다음 실행에 자동시작된다 — 표면화.
            self.web_error = Some(format!(
                "서버는 껐지만 설정 저장 실패 — 다음 실행에 다시 켜질 수 있습니다: {e:#}"
            ));
        }
    }

    /// 페어링 토큰 재발급 — 기존 페어링(QR/브라우저 저장분) 무효. 서버는 시작 시 토큰을
    /// 고정하므로 실행 중이면 새 토큰으로 재시작해 반영한다.
    fn web_rotate_token(&mut self) {
        if let Err(e) = web_remote::pairing::rotate_token(&self.secret_store) {
            tracing::warn!("페어링 토큰 재발급 실패: {e:#}");
            self.web_error = Some(format!("{e:#}"));
            return;
        }
        self.web_error = None;
        if let Some(state) = self.web.take() {
            state.server.shutdown();
            match self.start_web() {
                Ok(state) => self.web = Some(state),
                Err(e) => {
                    tracing::warn!("토큰 재발급 후 웹서버 재시작 실패: {e:#}");
                    self.web_error = Some(format!("{e:#}"));
                }
            }
        }
    }

    /// known_hosts 파일을 (host, 지문) 목록으로 로드한다 (표시 전용 — 파일 없으면 빈 목록).
    fn load_known_hosts(&self) -> Vec<(String, String)> {
        let path = self.known_hosts_path();
        match runtime::known_hosts::KnownHosts::load(&path) {
            Ok(known_hosts) => known_hosts.into_entries().collect(),
            Err(_) => {
                tracing::warn!(
                    kind = "known_hosts",
                    phase = "load",
                    error_code = "known_hosts_read_failed",
                    "known_hosts read failed"
                );
                Vec::new()
            }
        }
    }

    fn apply_settings_config(&mut self, ctx: &egui::Context) {
        self.config.i18n.locale = i18n::normalize_locale(&self.config.i18n.locale);
        if self.i18n.locale() != self.config.i18n.locale {
            self.i18n = load_catalog(&self.config.i18n.locale);
            self.agent_sessions_ui.set_catalog(&self.i18n);
            self.connector_ui.set_catalog(&self.i18n);
            // 홈 공지는 로케일별 캐시 키라 언어 변경 시 새 언어로 재번역돼야 한다.
            // 번역 펌프가 다음 프레임에 새 로케일의 캐시 미스를 잡도록 리페인트를
            // 확실히 깨우고, 대기 중이던 이전 로케일 번역이 새 로케일 시작을 막지
            // 않도록 in-flight 핸들을 버린다(결과는 이전 로케일용이라 무의미).
            self.notice_translate_rx = None;
            ctx.request_repaint();
        }
        ctx.set_theme(self.config.ui.theme.to_egui());
        self.sync_agent_hooks();
        self.broadcast_terminal_cache_policy();
        if self.config.ui.file_tree_enabled != self.file_tree.is_some() {
            self.file_tree = self
                .config
                .ui
                .file_tree_enabled
                .then(|| self.make_file_tree());
        }
        if self.activity_project_name_style != self.config.ui.session_name_style {
            self.request_project_name_projection();
        }
        if self.config.save(&self.config_path).is_err() {
            tracing::warn!(
                kind = "config",
                phase = "save",
                error_code = "config_save_failed",
                "config save failed"
            );
            self.remote_error = Some("설정 저장 실패".to_owned());
        }
    }

    fn stage_workspace_controller_action(&mut self, action: WorkspaceControllerAction) -> bool {
        if self.pending_workspace_controller_action.is_some() {
            return false;
        }
        self.pending_workspace_controller_action = Some(action);
        self.egui_ctx.request_repaint();
        true
    }

    fn poll_workspace_controller(&mut self) {
        let Some(action) = self.pending_workspace_controller_action.take() else {
            return;
        };
        match action {
            WorkspaceControllerAction::OpenAgentLauncher => {
                self.open_agent_launcher_for_active();
            }
            WorkspaceControllerAction::SwitchWorkspace(workspace_id) => {
                self.switch_workspace(&workspace_id);
            }
            WorkspaceControllerAction::FocusSession {
                workspace_id,
                tab,
                pane,
            } => {
                if workspace_id != self.active.id {
                    self.switch_workspace(&workspace_id);
                }
                if workspace_id == self.active.id {
                    let is_active_tab = self
                        .active
                        .workspace_ui
                        .mux()
                        .and_then(|mux| mux.active_tab.clone())
                        == Some(tab.clone());
                    if !is_active_tab {
                        let _ = self
                            .active
                            .runtime
                            .send_command(runtime::RuntimeCommand::SelectTab { tab });
                    }
                    let _ = self
                        .active
                        .runtime
                        .send_command(runtime::RuntimeCommand::FocusPane { pane });
                }
            }
            WorkspaceControllerAction::Runtime(command) => {
                if let runtime::RuntimeCommand::WriteInput { session, .. } = &command {
                    self.active.workspace_ui.clear_selection(*session);
                }
                if runtime_command_requires_dotenv(&command) {
                    let runtime_instance = self.active.runtime_instance;
                    if self
                        .stage_dotenv_continuation(
                            runtime_instance,
                            PendingDotenvContinuation::RuntimeCommand(command),
                        )
                        .is_err()
                    {
                        tracing::warn!(
                            kind = "dotenv",
                            phase = "runtime_admission",
                            error_code = "backpressure",
                            "dotenv-gated runtime command was rejected"
                        );
                    }
                } else if self.active.runtime.send_command(command).is_err() {
                    tracing::warn!(
                        kind = "workspace",
                        phase = "runtime_command",
                        error_code = "delivery_failed",
                        "workspace runtime command failed"
                    );
                }
            }
            WorkspaceControllerAction::SpawnShellAt { cwd } => {
                self.reveal_active_workspace_for_new_session();
                self.active
                    .workspace_ui
                    .spawn_shell_at(self.config.terminal.scrollback_lines as usize, cwd);
            }
            WorkspaceControllerAction::ResumeAgent {
                pane_key,
                title,
                session,
            } => {
                self.stage_agent_resume(&pane_key, &title, session);
                self.resumed_panes.insert(pane_key);
            }
            WorkspaceControllerAction::ClosePane(pane) => {
                self.active.workspace_ui.request_close_pane(pane);
            }
            WorkspaceControllerAction::CloseWorkspace(workspace_id) => {
                let was_active = workspace_id == self.active.id;
                self.close_workspace_sessions(&workspace_id);
                if was_active
                    && let Some(fallback) = self
                        .workspaces
                        .iter()
                        .map(|workspace| workspace.id.clone())
                        .find(|id| *id != workspace_id && !self.closed_workspaces.contains_key(id))
                {
                    self.switch_workspace(&fallback);
                }
            }
            WorkspaceControllerAction::FocusPty {
                switch_workspace,
                session,
            } => {
                if let Some(workspace_id) = switch_workspace {
                    self.switch_workspace(&workspace_id);
                    self.pending_focus = Some((workspace_id, session));
                } else if let Some(pane) = self
                    .active
                    .workspace_ui
                    .mux()
                    .and_then(|mux| pane_of_session(mux, session))
                {
                    let _ = self
                        .active
                        .runtime
                        .send_command(runtime::RuntimeCommand::FocusPane { pane });
                }
            }
            WorkspaceControllerAction::OpenStructured {
                switch_workspace,
                session_id,
            } => {
                if let Some(workspace_id) = switch_workspace {
                    self.switch_workspace(&workspace_id);
                }
                self.agent_sessions_ui.open_session(&session_id);
            }
            WorkspaceControllerAction::Notify { summary, body } => {
                platform::notify(&summary, &body);
            }
            WorkspaceControllerAction::ComposerPrompt(prompt) => {
                self.send_composer_prompt(&prompt);
            }
            WorkspaceControllerAction::SyncDotenv => self.sync_dotenv_env(),
        }
    }

    fn drain_workspace_protocol_intents(&mut self, runtime_instance: u64) {
        while let Some(intent) = self
            .runtime_by_instance_mut(runtime_instance)
            .and_then(|runtime| runtime.workspace_ui.take_protocol_intent())
        {
            let operation = intent.operation();
            let generation = intent.generation();
            let command = intent.into_command();
            if runtime_command_requires_dotenv(&command) {
                let continuation = PendingDotenvContinuation::WorkspaceProtocol {
                    operation,
                    generation,
                    command,
                };
                if let Err(continuation) =
                    self.stage_dotenv_continuation(runtime_instance, continuation)
                    && let PendingDotenvContinuation::WorkspaceProtocol {
                        operation,
                        generation,
                        ..
                    } = *continuation
                    && let Some(runtime) = self.runtime_by_instance_mut(runtime_instance)
                {
                    runtime.workspace_ui.complete_protocol(
                        ui::workspace::WorkspaceProtocolCompletion {
                            operation,
                            generation,
                            result: Err(ui::workspace::WorkspaceProtocolErrorCode::Busy),
                        },
                    );
                }
                continue;
            }
            let Some(runtime) = self.runtime_by_instance_mut(runtime_instance) else {
                continue;
            };
            let result = runtime
                .runtime
                .send_command(command)
                .map_err(|_| ui::workspace::WorkspaceProtocolErrorCode::DeliveryFailed);
            runtime
                .workspace_ui
                .complete_protocol(ui::workspace::WorkspaceProtocolCompletion {
                    operation,
                    generation,
                    result,
                });
        }
    }

    fn poll_workspace_protocol_intents(&mut self) {
        let mut runtime_instances = Vec::with_capacity(1 + self.warm.len());
        runtime_instances.push(self.active.runtime_instance);
        runtime_instances.extend(self.warm.values().map(|runtime| runtime.runtime_instance));
        for runtime_instance in runtime_instances {
            self.drain_workspace_protocol_intents(runtime_instance);
        }
    }

    fn drain_closing_workspace_protocol_intents(runtime: &mut WorkspaceRuntime) {
        while let Some(intent) = runtime.workspace_ui.take_protocol_intent() {
            let operation = intent.operation();
            let generation = intent.generation();
            let command = intent.into_command();
            let result = if runtime_command_requires_dotenv(&command) {
                Err(ui::workspace::WorkspaceProtocolErrorCode::DeliveryFailed)
            } else {
                runtime
                    .runtime
                    .send_command(command)
                    .map_err(|_| ui::workspace::WorkspaceProtocolErrorCode::DeliveryFailed)
            };
            runtime
                .workspace_ui
                .complete_protocol(ui::workspace::WorkspaceProtocolCompletion {
                    operation,
                    generation,
                    result,
                });
        }
    }

    fn poll_pending_workspace_focus(&mut self) {
        let Some((workspace_id, session)) = self.pending_focus.clone() else {
            return;
        };
        if workspace_id != self.active.id {
            self.pending_focus = None;
            return;
        }
        let Some(pane) = self
            .active
            .workspace_ui
            .mux()
            .and_then(|mux| pane_of_session(mux, session))
        else {
            return;
        };
        let _ = self
            .active
            .runtime
            .send_command(runtime::RuntimeCommand::FocusPane { pane });
        self.pending_focus = None;
    }

    fn poll_turn_done_clear(&mut self) {
        let Some((session_key, seen_at)) = self.pending_turn_done_clear.take() else {
            return;
        };
        let clear = storage::AgentTurnDoneClear {
            session_key,
            seen_at,
        };
        if !turn_done_clear_matches_workspace(&clear, &self.agent_state_scope.workspace_id) {
            return;
        }
        if !self.stage_agent_state_exact(AppAgentStateExactKind::TurnDoneClear(clear.clone())) {
            self.pending_turn_done_clear = Some((clear.session_key, clear.seen_at));
        }
    }

    /// Settings render가 만든 bounded lifecycle intent와 worker 결과를 non-render 단계에서
    /// 처리한다. 설정을 열지 않았고 intent/result가 없으면 파일/키링/프로세스 작업은 0이다.
    fn poll_app_controller(&mut self, ctx: &egui::Context) {
        if self.pending_settings_config_apply {
            self.pending_settings_config_apply = false;
            self.pending_config_save = false;
            self.apply_settings_config(ctx);
        } else if self.pending_config_save {
            self.pending_config_save = false;
            if self.config.save(&self.config_path).is_err() {
                tracing::warn!(
                    kind = "config",
                    phase = "save",
                    error_code = "config_save_failed",
                    "config save failed"
                );
            }
        }

        if let Some(action) = self.pending_app_controller_action.take() {
            match action {
                AppControllerAction::OpenFileAccessSettings => {
                    platform::open_file_access_settings();
                }
                AppControllerAction::DetectEnvSessionBanner { cwd } => {
                    self.env_session_banner = self.detect_session_cwd_banner(&cwd);
                }
                AppControllerAction::CreateWorktree { workspace_id, cwd } => {
                    if self.worktree_rx.is_none() {
                        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
                        self.worktree_rx = Some((workspace_id, result_rx));
                        let wake = ctx.clone();
                        let spawned = std::thread::Builder::new()
                            .name("worktree-create".to_owned())
                            .spawn(move || {
                                let result = crate::worktree::create_worktree(Path::new(&cwd));
                                let _ = result_tx.send(result);
                                wake.request_repaint();
                            });
                        if spawned.is_err() {
                            self.worktree_rx = None;
                            tracing::warn!(
                                kind = "worktree",
                                phase = "create",
                                error_code = "worker_spawn_failed",
                                "worktree worker spawn failed"
                            );
                        }
                    }
                }
                AppControllerAction::RemoveWorktree { workspace_id, cwd } => {
                    if self.worktree_remove_rx.is_none() {
                        const SESSION_PID_MAX: usize = 256;
                        let session_pids = std::iter::once(&self.active)
                            .chain(self.warm.values())
                            .flat_map(|runtime| {
                                runtime.session_resource_usage.iter().filter_map(|usage| {
                                    usage
                                        .pid
                                        .map(|pid| (runtime.id.clone(), usage.session, pid))
                                })
                            })
                            .take(SESSION_PID_MAX + 1)
                            .collect::<Vec<_>>();
                        if session_pids.len() > SESSION_PID_MAX {
                            tracing::warn!(
                                kind = "worktree",
                                phase = "remove",
                                error_code = "session_limit",
                                "worktree session limit exceeded"
                            );
                            return;
                        }
                        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
                        self.worktree_remove_rx = Some((workspace_id, result_rx));
                        let wake = ctx.clone();
                        let spawned = std::thread::Builder::new()
                            .name("worktree-remove".to_owned())
                            .spawn(move || {
                                let result = crate::worktree::remove_worktree(Path::new(&cwd)).map(
                                    |(root, branch)| {
                                        let hits = session_pids
                                            .into_iter()
                                            .filter(|(_, _, pid)| {
                                                platform::process_cwd(*pid)
                                                    .is_some_and(|path| path.starts_with(&root))
                                            })
                                            .map(|(workspace, session, _)| (workspace, session))
                                            .collect();
                                        (root, branch, hits)
                                    },
                                );
                                let _ = result_tx.send(result);
                                wake.request_repaint();
                            });
                        if spawned.is_err() {
                            self.worktree_remove_rx = None;
                            tracing::warn!(
                                kind = "worktree",
                                phase = "remove",
                                error_code = "worker_spawn_failed",
                                "worktree worker spawn failed"
                            );
                        }
                    }
                }
                AppControllerAction::RemoteStart => self.remote_enable(),
                AppControllerAction::RemoteStop => self.remote_disable(),
                AppControllerAction::ForgetKnownHost(host) => {
                    let path = self.known_hosts_path();
                    if runtime::known_hosts::KnownHosts::load(&path)
                        .and_then(|mut known_hosts| known_hosts.forget(&host))
                        .is_err()
                    {
                        tracing::warn!(
                            kind = "known_hosts",
                            phase = "forget",
                            error_code = "known_hosts_update_failed",
                            "known_hosts update failed"
                        );
                    }
                    self.known_hosts_cache = Some(self.load_known_hosts());
                }
                AppControllerAction::WebStart => self.web_enable(),
                AppControllerAction::WebStop => self.web_disable(),
                AppControllerAction::RotateWebToken => self.web_rotate_token(),
                AppControllerAction::DetectHostname => {
                    if self.ts_detect_rx.is_none() {
                        self.ts_detect_overwrite = true;
                        self.ts_detect_rx = Some(crate::tailscale::spawn_detect(ctx.clone()));
                    }
                }
                AppControllerAction::CheckServe => {
                    if self.serve_rx.is_none()
                        && let Some(port) =
                            self.web.as_ref().map(|web| web.server.local_addr().port())
                    {
                        self.serve_rx =
                            Some(crate::tailscale::spawn_serve_check(ctx.clone(), port));
                    }
                }
                AppControllerAction::ConfigureServe => {
                    if self.serve_rx.is_none()
                        && let Some(port) =
                            self.web.as_ref().map(|web| web.server.local_addr().port())
                    {
                        self.serve_rx =
                            Some(crate::tailscale::spawn_serve_configure(ctx.clone(), port));
                    }
                }
            }
        }

        if self.settings_open && self.known_hosts_cache.is_none() {
            self.known_hosts_cache = Some(self.load_known_hosts());
        }
        if let Some(rx) = &self.ts_detect_rx {
            match rx.try_recv() {
                Ok(result) => {
                    if let crate::tailscale::Detected::Hostname(host) = &result
                        && (self.ts_detect_overwrite
                            || self.config.web.ts_hostname.trim().is_empty())
                        && self.config.web.ts_hostname.trim() != host
                    {
                        self.config.web.ts_hostname = host.clone();
                        self.pending_config_save = true;
                    }
                    self.ts_detected = Some(result);
                    self.ts_detect_rx = None;
                    self.ts_detect_overwrite = false;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.ts_detected = Some(crate::tailscale::Detected::CliNotFound);
                    self.ts_detect_rx = None;
                    self.ts_detect_overwrite = false;
                }
            }
        } else if self.settings_open
            && self.settings_category == ui::settings::Category::MobileWeb
            && self.config.web.ts_hostname.trim().is_empty()
            && self.ts_detected.is_none()
        {
            self.ts_detect_rx = Some(crate::tailscale::spawn_detect(ctx.clone()));
        }
        if let Some(rx) = &self.serve_rx {
            match rx.try_recv() {
                Ok(state) => {
                    self.serve_state = Some(state);
                    self.serve_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.serve_state = Some(crate::tailscale::ServeState::Unknown);
                    self.serve_rx = None;
                }
            }
        } else if self.settings_open
            && self.settings_category == ui::settings::Category::MobileWeb
            && self.serve_state.is_none()
            && let Some(port) = self.web.as_ref().map(|web| web.server.local_addr().port())
        {
            self.serve_rx = Some(crate::tailscale::spawn_serve_check(ctx.clone(), port));
        }
        if !self.settings_open {
            self.known_hosts_cache = None;
        }
    }

    fn poll_worktree_jobs(&mut self) {
        let text = self.i18n.clone();
        if let Some((requested_workspace, receiver)) = &self.worktree_rx {
            let same_workspace = *requested_workspace == self.active.id;
            let spawn_busy = self.active.workspace_ui.pending_spawns() > 0;
            match crate::worktree::spawn_decision(same_workspace, spawn_busy) {
                crate::worktree::SpawnDecision::Defer => {}
                decision => match receiver.try_recv() {
                    Ok(Ok(path)) => {
                        self.worktree_rx = None;
                        if decision == crate::worktree::SpawnDecision::Spawn {
                            self.reveal_active_workspace_for_new_session();
                            self.active.workspace_ui.spawn_shell_at(
                                self.config.terminal.scrollback_lines as usize,
                                Some(path.to_string_lossy().into_owned()),
                            );
                        } else {
                            platform::notify(&text.t("worktree.created_elsewhere", &[]), "");
                        }
                    }
                    Ok(Err(_)) => {
                        self.worktree_rx = None;
                        tracing::warn!(
                            kind = "worktree",
                            phase = "create",
                            error_code = "create_failed",
                            "worktree create failed"
                        );
                        platform::notify(&text.t("worktree.create_failed", &[]), "");
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        self.worktree_rx = None;
                        tracing::warn!(
                            kind = "worktree",
                            phase = "create",
                            error_code = "worker_disconnected",
                            "worktree worker disconnected"
                        );
                    }
                },
            }
        }

        let worktree_remove_result = self
            .worktree_remove_rx
            .as_ref()
            .map(|(workspace, receiver)| (workspace.clone(), receiver.try_recv()));
        if let Some((requested_workspace, result)) = worktree_remove_result {
            match result {
                Ok(Ok((root, branch, hits))) => {
                    let mut targets: Vec<(String, runtime::MuxPaneId)> = Vec::new();
                    for (workspace_id, session) in &hits {
                        let workspace_ui = if *workspace_id == self.active.id {
                            Some(&self.active.workspace_ui)
                        } else {
                            self.warm
                                .get(workspace_id)
                                .map(|runtime| &runtime.workspace_ui)
                        };
                        let Some(pane) =
                            workspace_ui.and_then(|workspace| workspace.pane_for_session(*session))
                        else {
                            continue;
                        };
                        if !targets
                            .iter()
                            .any(|(workspace, target)| workspace == workspace_id && *target == pane)
                        {
                            targets.push((workspace_id.clone(), pane));
                        }
                    }
                    for (session, cwd) in &self.session_cwds {
                        if !Path::new(cwd).starts_with(&root) {
                            continue;
                        }
                        let Some(pane) = self.active.workspace_ui.pane_for_session(*session) else {
                            continue;
                        };
                        if !targets.iter().any(|(workspace, target)| {
                            *workspace == self.active.id && *target == pane
                        }) {
                            targets.push((self.active.id.clone(), pane));
                        }
                    }
                    for (workspace_id, pane) in targets {
                        if workspace_id == self.active.id {
                            let runtime_instance = self.active.runtime_instance;
                            self.drain_workspace_protocol_intents(runtime_instance);
                            self.active.workspace_ui.close_pane_now(pane);
                            self.drain_workspace_protocol_intents(runtime_instance);
                        } else if let Some(runtime_instance) = self
                            .warm
                            .get(&workspace_id)
                            .map(|runtime| runtime.runtime_instance)
                        {
                            self.drain_workspace_protocol_intents(runtime_instance);
                            if let Some(runtime) = self.warm.get_mut(&workspace_id) {
                                runtime.workspace_ui.close_pane_now(pane);
                            }
                            self.drain_workspace_protocol_intents(runtime_instance);
                        }
                    }
                    let branch_note = (branch == crate::worktree::BranchCleanup::PreservedUnmerged)
                        .then(|| text.t("worktree.branch_preserved", &[]));
                    if requested_workspace == self.active.id {
                        platform::notify(
                            &text.t("worktree.removed", &[]),
                            branch_note.as_deref().unwrap_or(""),
                        );
                    } else {
                        platform::notify(
                            &text.t("worktree.removed_elsewhere", &[]),
                            branch_note.as_deref().unwrap_or(""),
                        );
                    }
                    self.worktree_remove_rx = None;
                }
                Ok(Err(_)) => {
                    self.worktree_remove_rx = None;
                    tracing::warn!(
                        kind = "worktree",
                        phase = "remove",
                        error_code = "remove_failed",
                        "worktree remove failed"
                    );
                    platform::notify(&text.t("worktree.remove_failed", &[]), "");
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.worktree_remove_rx = None;
                    tracing::warn!(
                        kind = "worktree",
                        phase = "remove",
                        error_code = "worker_disconnected",
                        "worktree worker disconnected"
                    );
                }
            }
        }
    }

    /// workspace 전환 (워커-per-workspace §14.1 Warm): 현재 활성 workspace는 Warm으로
    /// 내려 워커를 계속 살려 둔다(에이전트 유지). 대상이 warm 풀에 있으면 재사용(즉시 복귀),
    /// 없으면 새로 만든다. warm 풀이 max_warm을 넘으면 가장 오래된 것을 Suspended(shutdown).
    /// 폰(미러 진입 — I1b-2)이 보낸 전환 요청 큐를 비운다. 웹 스레드가 push한 워크스페이스
    /// id를 egui 스레드에서 switch_workspace로 넘긴다 — 대기=재사용/절전=재생성/상한초과=거부를
    /// switch_workspace가 처리하고, 성공 시 rebind_web_dashboard가 폰·데스크탑을 미러시킨다.
    fn drain_web_switch_requests(&mut self) {
        // 하드 미러라 최종 목적지만 의미 있다 — 큐에 쌓인 중간 요청은 버리고 마지막 하나만
        // 처리한다. 안 그러면 A,B,A,B 연타가 한 프레임에 N번의 워커 spawn+config 저장을
        // egui 스레드에서 유발한다(리뷰 P3). Drain은 next_back으로 마지막만 꺼내도 drop 시
        // 범위 전체를 vec에서 제거하므로 큐는 그대로 비워진다(last()의 전체순회 회피).
        let target: Option<String> = match self.web_switch_queue.lock() {
            Ok(mut queue) => queue.drain(..).next_back(),
            Err(_) => return,
        };
        if let Some(ws_id) = target {
            self.handle_web_switch(&ws_id);
        }
    }

    /// 폰이 요청한 워크스페이스로 전환한다(미러 진입). 알 수 없는 id는 무시(방어), 이미
    /// 활성이면 no-op. 상한 초과로 switch_workspace가 거부하면(active 그대로) 폰에 안내를
    /// 띄운다 — 조용한 실패를 막는다.
    fn handle_web_switch(&mut self, ws_id: &str) {
        if ws_id == self.active.id {
            return; // 이미 활성 — 폰은 이미 미러 중.
        }
        let Some(name) = self
            .workspaces
            .iter()
            .find(|ws| ws.id == ws_id)
            .map(Self::workspace_display_name)
        else {
            tracing::warn!(ws = %ws_id, "폰 전환 요청 — 알 수 없는 워크스페이스 무시");
            return;
        };
        // 절전 깨우기(워커 없음 + 복원할 pane 있음) 판정은 전환 전에 — switch가 워커를
        // 만들고 나면 구분이 사라진다 (I1b-3 "복원 중" 안내).
        let wake_from_suspend = !self.warm.contains_key(ws_id)
            && self
                .persisted_activity_panes
                .get(ws_id)
                .is_some_and(|panes| !panes.is_empty());
        self.switch_workspace(ws_id);
        if self.active.id == ws_id {
            if wake_from_suspend {
                // 절전 해제는 워커 생성+RestoreWorkspace+resume까지 몇 초 걸린다 — 그동안
                // 폰이 빈 세션 목록을 보므로 "복원 중" 안내(TTL 자동 해제, 복원이 끝나면
                // 대시보드 프레임이 세션을 채운다).
                let msg = self.i18n.t("workspace.wake.restoring", &[("name", &name)]);
                self.set_web_notice(Some(msg));
                self.egui_ctx.request_repaint_after(Self::WEB_NOTICE_TTL);
            } else {
                // 대기 재사용 — 즉시 미러되므로 직전 안내만 해제.
                self.set_web_notice(None);
            }
        } else if let Some(target) = self.warm_limit_warning.clone() {
            // 상한 초과로 거부됨 — 폰에 안내(데스크탑 모달과 독립, TTL로 자동 해제).
            let msg = self.i18n.t(
                "workspace.warm_limit.body",
                &[
                    ("target", &target),
                    ("limit", &self.config.performance.max_live_warm.to_string()),
                ],
            );
            self.set_web_notice(Some(msg));
            self.egui_ctx.request_repaint_after(Self::WEB_NOTICE_TTL);
        }
    }

    /// 폰 안내 배너를 세팅/해제한다 (I1b-2). 앱 상태와 브리지 프레임을 함께 갱신한다.
    /// **같은 내용을 다시 세팅해도 TTL(set_at)은 유지한다** — 안 그러면 cap-full 버튼 연타가
    /// 매번 타이머를 리셋해 expire_web_notice가 영영 안 돌고, 서버 notice는 Some에 고정되며
    /// 클라는 내용 dedup으로 재표시를 안 해 "재탭했는데 아무 반응 없음"이 된다(리뷰 P3:
    /// 없애려던 silent failure의 재발). now는 내용이 바뀔 때만 새로 찍는다.
    fn set_web_notice(&mut self, notice: Option<String>) {
        self.web_notice = notice.clone().map(|msg| {
            let set_at = match &self.web_notice {
                Some((prev, at)) if *prev == msg => *at,
                _ => std::time::Instant::now(),
            };
            (msg, set_at)
        });
        if let Some(web) = &self.web {
            web.server.set_dashboard_notice(notice);
        }
    }

    /// notice TTL이 지나면 배너를 내린다 (I1b-2 — logic()에서 매 프레임 확인).
    fn expire_web_notice(&mut self) {
        if let Some((_, set_at)) = &self.web_notice
            && set_at.elapsed() >= Self::WEB_NOTICE_TTL
        {
            self.set_web_notice(None);
        }
    }

    fn switch_workspace(&mut self, target_id: &str) {
        // 명시적 전환은 종료 숨김 해제 — 사용자가 다시 연 것이다(사이드바 행 클릭·
        // 워크스페이스 순환·알림/에이전트 이동·같은 폴더 재선택 모두 이 경로).
        self.reveal_closed_workspace(target_id);
        if target_id == self.active.id {
            return;
        }
        let live_warm = self
            .warm
            .values()
            .filter(|runtime| runtime.has_live_sessions())
            .count();
        let target_is_live_warm = self
            .warm
            .get(target_id)
            .is_some_and(WorkspaceRuntime::has_live_sessions);
        let projected = projected_live_warm_count(
            live_warm,
            target_is_live_warm,
            self.active.has_live_sessions(),
        );
        if projected > self.config.performance.max_live_warm as usize {
            self.warm_limit_warning = Some(
                self.workspaces
                    .iter()
                    .find(|workspace| workspace.id == target_id)
                    .map(Self::workspace_display_name)
                    .unwrap_or_else(|| target_id.to_owned()),
            );
            self.egui_ctx.request_repaint();
            return;
        }
        // 대상이 background 정리 중이면 먼저 끝낸다 (같은 window 행 경합 방지 — codex 리뷰).
        self.join_pending_shutdown(target_id);

        // 대상 준비: warm 풀에 있으면 재사용, 없으면 새 워커.
        let persisted_restore_exists = self
            .persisted_activity_panes
            .get(target_id)
            .is_some_and(|panes| !panes.is_empty());
        let (mut new_active, needs_restore) = match self.warm.remove(target_id) {
            Some(rt) => {
                self.warm_order.retain(|id| id != target_id);
                (rt, false)
            }
            None => {
                // 새 워커는 SessionId를 1부터 다시 시작한다 — 이 workspace의 옛 워커
                // lifetime에서 남은 알림을 지운다. 안 그러면 재사용된 SessionId의 완료
                // 알림이 옛 항목과 dup으로 취급돼 안 뜬다 (codex 리뷰).
                self.notifications_ui.prune_workspace(target_id);
                let runtime_instance = self.next_runtime_instance;
                self.next_runtime_instance = self.next_runtime_instance.wrapping_add(1).max(1);
                (
                    Self::make_runtime(
                        &self.config,
                        &self.logs_base,
                        target_id,
                        runtime_instance,
                        &self.db_path,
                        self.runtime_host_factory.as_ref(),
                        &self.db,
                        &self.egui_ctx,
                    ),
                    persisted_restore_exists,
                )
            }
        };
        let resumed_pending_agents = std::mem::take(&mut new_active.pending_agent_spawns);
        // UI 상태는 리셋하지 않는다 — warm 재사용이면 그동안 누적된 pending_events(=lifecycle
        // 이벤트 포함)를 그대로 ui()가 처리해 exit/status 상태를 재구성해야 하고, workspace_ui는
        // 마지막 active 상태 + 아래 Active 재emit(전체 mux 스냅샷)으로 최신화된다. (새 워커는
        // 이미 fresh + RestoreWorkspace라 리셋 불필요.)
        new_active.render_active = true;
        new_active.backgrounded_at = None;
        let activated = new_active
            .runtime
            .send_command(runtime::RuntimeCommand::SetWorkspaceState(
                runtime::WorkspaceRuntimeState::Active,
            ))
            .is_ok();
        clear_pending_replay_resync_after_activation(
            &mut new_active.pending_replay_resync,
            true,
            activated,
        );

        // 현재 활성을 Warm으로 내리고 warm 풀에 보관 (워커·세션 계속 실행).
        let mut old = std::mem::replace(&mut self.active, new_active);
        // Retain only the pending scope metadata before any old-scope completion can be applied
        // against the new active runtime. The worker drain barrier installs the new epoch later.
        self.request_agent_state_scope();
        // 웹 대시보드가 켜져 있으면 새 활성 worker로 재구독한다(전환 후 상태 스트림 유지).
        self.rebind_web_dashboard();
        // agent 감지 워커: 전환 시 epoch을 올려 이전 워크스페이스의 잔여 결과를 폐기하고,
        // 즉시 감지가 새 워크스페이스 기준으로 재시작되게 한다(codex #3).
        self.agent_detect_epoch += 1;
        self.agent_bindings.clear();
        self.agent_activity.clear();
        self.agent_needs_input.clear();
        self.agent_turn_done.clear();
        self.pending_turn_done_clear = None;
        self.session_alerts.clear();
        self.session_cwds.clear();
        self.workspace_rename_prompt = None; // 워크스페이스 전환 시 옛 rename 제안 폐기
        self.agent_info.clear();
        self.statuslines.clear();
        let _ = old
            .runtime
            .send_command(runtime::RuntimeCommand::SetWorkspaceState(
                runtime::WorkspaceRuntimeState::Warm,
            ));
        old.render_active = false;
        old.backgrounded_at = Some(std::time::Instant::now());
        let old_id = old.id.clone();
        self.warm.insert(old_id.clone(), old);
        self.warm_order.push(old_id.clone());
        self.refresh_warm_idle_deadline();

        // pending 상태 정리 (이전 워커 응답 못 받음, 교차-ws 감사 방지).
        // notifications는 리셋하지 않는다 — (ws, session)로 namespacing돼 전역 센터가
        // 모든 workspace 알림을 유지한다 (background 완료 통지·클릭 이동, codex 리뷰).
        // agent spawn 대기는 버리지 않고 물러난 workspace로 이관 — 응답이 오기 전까지
        // 그 workspace를 live로 취급해 suspend가 새 PTY를 죽이는 창을 막는다 (codex).
        let pending_agents = self.agents_ui.take_pending();
        if let Some(old_rt) = self.warm.get_mut(&old_id) {
            old_rt.pending_agent_spawns += pending_agents;
        }
        self.agents_ui.restore_pending(resumed_pending_agents);
        self.connector_ui.clear_sensitive_drafts();
        // 파일 트리 루트를 새 workspace path로 갱신 (FT-1)
        self.config.ui.last_workspace_id = Some(target_id.to_owned());
        if let Err(e) = self.config.save(&self.config_path) {
            tracing::warn!("마지막 workspace 저장 실패: {e:#}");
        }
        self.refresh_file_tree_root();
        if needs_restore {
            self.stage_runtime_restore(self.active.runtime_instance);
        }
        self.egui_ctx.request_repaint();

        if !persisted_restore_exists && self.bench.is_none() && self.perf_harness_next.is_none() {
            self.offer_agent_launcher_for_active();
        }

        self.evict_warm();
        // 새 runtime 생성 또는 warm 축출로 resident 수가 바뀌었을 수 있다. 설정의
        // 전역 예산을 현재 active+warm 전체에 다시 나눠 각 워커에 반영한다.
        self.broadcast_terminal_cache_policy();
    }

    fn cycle_workspace(&mut self, delta: isize) {
        if self.workspaces.len() < 2 {
            return;
        }
        let current = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == self.active.id)
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(self.workspaces.len() as isize) as usize;
        let target = self.workspaces[next].id.clone();
        self.switch_workspace(&target);
        self.refresh_workspaces();
    }

    fn handle_agent_sessions_request(&mut self, request: ui::agent_sessions::AgentSessionsRequest) {
        use crate::agent_surface::AgentSurfaceId;
        match request {
            ui::agent_sessions::AgentSessionsRequest::RevealWorkspace(workspace_id) => {
                self.reveal_closed_workspace(&workspace_id);
            }
            ui::agent_sessions::AgentSessionsRequest::FocusPty(AgentSurfaceId::Pty {
                workspace_id,
                pane_id,
                session_id,
            }) => {
                if workspace_id != self.active.id {
                    self.switch_workspace(&workspace_id);
                    self.refresh_workspaces();
                    self.pending_focus = Some((workspace_id, session_id));
                    return;
                }
                let target = runtime::MuxPaneId(pane_id);
                let tab = self
                    .active
                    .workspace_ui
                    .mux()
                    .and_then(|mux| tab_of_agent_target(mux, &target, session_id));
                if let Some(tab) = tab {
                    if self
                        .active
                        .workspace_ui
                        .mux()
                        .and_then(|mux| mux.active_tab.clone())
                        != Some(tab.clone())
                    {
                        let _ = self
                            .active
                            .runtime
                            .send_command(runtime::RuntimeCommand::SelectTab { tab });
                    }
                    let _ = self
                        .active
                        .runtime
                        .send_command(runtime::RuntimeCommand::FocusPane { pane: target });
                }
            }
            ui::agent_sessions::AgentSessionsRequest::InterruptPty(AgentSurfaceId::Pty {
                workspace_id,
                pane_id,
                session_id,
            }) => {
                let still_matches = workspace_id == self.active.id
                    && self.active.workspace_ui.mux().is_some_and(|mux| {
                        mux.tabs
                            .iter()
                            .flat_map(|tab| &tab.panes)
                            .any(|pane| pane.id.0 == pane_id && pane.session_id == Some(session_id))
                    });
                if still_matches {
                    let _ = self
                        .active
                        .runtime
                        .send_command(runtime::RuntimeCommand::WriteInput {
                            session: session_id,
                            bytes: vec![0x03],
                        });
                }
            }
            ui::agent_sessions::AgentSessionsRequest::FocusPty(AgentSurfaceId::Structured {
                ..
            })
            | ui::agent_sessions::AgentSessionsRequest::InterruptPty(
                AgentSurfaceId::Structured { .. },
            ) => unreachable!("PTY 요청은 PTY target만 생성한다"),
        }
    }

    /// 설정에 저장된 전역 단축키 한 건을 실행한다. 설정 창에서는 키 녹화와 검색 입력이
    /// 우선이고, 일반 TextEdit 포커스 중에도 문자 편집 단축키를 가로채지 않는다.
    fn handle_configured_shortcut(&mut self, ctx: &egui::Context) {
        if self.settings_open || ctx.text_edit_focused() {
            return;
        }
        let Some(action) = crate::shortcuts::take_triggered_action(ctx, &self.config.shortcuts)
        else {
            return;
        };

        use crate::shortcuts::ShortcutAction as A;
        match action {
            A::ToggleSidebar => {
                self.config.ui.file_tree_enabled = !self.config.ui.file_tree_enabled;
                self.file_tree = self
                    .config
                    .ui
                    .file_tree_enabled
                    .then(|| self.make_file_tree());
                if let Err(error) = self.config.save(&self.config_path) {
                    tracing::warn!("단축키 설정 저장 실패: {error:#}");
                }
            }
            // 알림은 설정 창이 아니라 벨 팝오버를 토글한다 (v3.9 N1) — 승인/응답을
            // 빠르게 처리하는 경로라 통합 설정 창 전체를 열지 않는다. 전체 기록은
            // 팝오버의 「전체 보기」가 설정→알림으로 연결한다.
            A::OpenNotifications => egui::Popup::toggle_id(ctx, Self::inbox_popup_id()),
            A::OpenEnvironment | A::OpenActivity => {
                self.settings_category = match action {
                    A::OpenEnvironment => ui::settings::Category::Environment,
                    A::OpenActivity => ui::settings::Category::Activity,
                    _ => unreachable!(),
                };
                self.settings_open = true;
                self.refresh_workspaces();
            }
            A::OpenAgents => self.handle_agent_shortcut(action, ctx),
            A::NewShell => self.open_agent_launcher_for_active(),
            A::ClosePane => self.active.workspace_ui.close_focused_pane(),
            A::ScrollToBottom => self.active.workspace_ui.scroll_focused_to_bottom(),
            A::PromptJumpPrev => self.active.workspace_ui.scroll_focused_to_prompt(-1),
            A::PromptJumpNext => self.active.workspace_ui.scroll_focused_to_prompt(1),
            A::SplitVertical | A::SplitHorizontal => {
                self.reveal_active_workspace_for_new_session();
                let direction = if action == A::SplitVertical {
                    runtime::SplitDirection::Vertical
                } else {
                    runtime::SplitDirection::Horizontal
                };
                self.active
                    .workspace_ui
                    .split_focused_pane(direction, self.config.terminal.scrollback_lines as usize);
            }
            A::FocusNextPane => self.active.workspace_ui.focus_relative_pane(1),
            A::FocusPreviousPane => self.active.workspace_ui.focus_relative_pane(-1),
            A::NextWorkspace => self.cycle_workspace(1),
            A::PreviousWorkspace => self.cycle_workspace(-1),
            A::IncreaseTerminalFont | A::DecreaseTerminalFont => {
                let delta = if action == A::IncreaseTerminalFont {
                    0.5
                } else {
                    -0.5
                };
                self.config.terminal.font_size =
                    (self.config.terminal.font_size + delta).clamp(8.0, 32.0);
                self.active.workspace_ui.clear_render_caches();
                for runtime in self.warm.values_mut() {
                    runtime.workspace_ui.clear_render_caches();
                }
                if let Err(error) = self.config.save(&self.config_path) {
                    tracing::warn!("터미널 글꼴 크기 저장 실패: {error:#}");
                }
            }
            A::TerminalSearch => self.active.workspace_ui.open_search(),
            // 컴포저 포커스+펼침. 이미 포커스면 이 경로는 오지 않는다(text_edit_focused
            // 조기 반환) — 접기는 컴포저가 ⌘J를 직접 소비해 처리한다.
            // 설정 OFF면 무시 — 숨겨진(미생성) 도크에 포커스를 줄 수 없다.
            A::FocusComposer => {
                if self.config.ui.composer_enabled {
                    self.composer.request_focus();
                }
            }
            A::ClearRenderCaches => {
                self.active.workspace_ui.clear_render_caches();
                for runtime in self.warm.values_mut() {
                    runtime.workspace_ui.clear_render_caches();
                }
            }
            A::PreviousAgent
            | A::NextAgent
            | A::FocusAgentInput
            | A::NewStructuredAgent
            | A::InterruptAgent
            | A::ApproveAgent
            | A::RejectAgent
            | A::IncreaseAgentEffort
            | A::DecreaseAgentEffort => {
                self.handle_agent_shortcut(action, ctx);
            }
        }
        ctx.request_repaint();
    }

    fn handle_agent_shortcut(
        &mut self,
        shortcut: crate::shortcuts::ShortcutAction,
        ctx: &egui::Context,
    ) {
        use crate::agent_actions::{AgentAction, AgentActionGate, gate_action};
        use crate::shortcuts::ShortcutAction as A;

        let action = match shortcut {
            A::OpenAgents => AgentAction::OpenAgents,
            A::PreviousAgent => AgentAction::SelectPrevious,
            A::NextAgent => AgentAction::SelectNext,
            A::FocusAgentInput => AgentAction::FocusInput,
            A::NewStructuredAgent => AgentAction::NewStructured,
            A::InterruptAgent => AgentAction::Interrupt,
            A::ApproveAgent => AgentAction::ApproveOnce,
            A::RejectAgent => AgentAction::Reject,
            A::IncreaseAgentEffort => AgentAction::EffortUp,
            A::DecreaseAgentEffort => AgentAction::EffortDown,
            _ => return,
        };
        let selected = self.agent_sessions_ui.selected_surface_snapshot();
        let pending = self.agent_sessions_ui.selected_pending_approval_count();
        let gate = gate_action(action, selected.as_ref(), pending);
        if !gate.is_allowed() {
            self.agent_sessions_ui.open();
            match gate {
                AgentActionGate::NoTarget => tracing::info!("에이전트 단축키: 선택 없음"),
                AgentActionGate::Unsupported => {
                    tracing::info!("에이전트 단축키: 선택 transport에서 지원하지 않음")
                }
                AgentActionGate::ApprovalCountMismatch { pending } => {
                    tracing::info!(pending, "에이전트 승인 단축키 안전 조건 불충족")
                }
                AgentActionGate::Allowed => unreachable!(),
            }
            return;
        }

        let request = match action {
            AgentAction::SelectPrevious => {
                self.agent_sessions_ui.select_relative(-1);
                None
            }
            AgentAction::SelectNext => {
                self.agent_sessions_ui.select_relative(1);
                None
            }
            AgentAction::FocusInput => self.agent_sessions_ui.focus_selected_input(),
            AgentAction::NewStructured => {
                self.agent_sessions_ui.open_new_prompt();
                None
            }
            AgentAction::Interrupt => match self.agent_sessions_ui.interrupt_selected(ctx) {
                Ok(request) => request,
                Err(error) => {
                    tracing::warn!("에이전트 중단 단축키 실패: {error:#}");
                    None
                }
            },
            AgentAction::ApproveOnce => {
                if let Err(error) = self.agent_sessions_ui.approve_selected_once(ctx) {
                    tracing::warn!("에이전트 승인 단축키 실패: {error:#}");
                }
                None
            }
            AgentAction::Reject => {
                if let Err(error) = self.agent_sessions_ui.reject_selected(ctx) {
                    tracing::warn!("에이전트 거절 단축키 실패: {error:#}");
                }
                None
            }
            AgentAction::EffortUp | AgentAction::EffortDown => {
                self.agent_sessions_ui.open();
                let delta = if action == AgentAction::EffortUp {
                    1
                } else {
                    -1
                };
                if let Err(error) = self.agent_sessions_ui.adjust_selected_effort(delta) {
                    tracing::warn!("에이전트 effort 단축키 실패: {error:#}");
                }
                None
            }
            AgentAction::OpenAgents => {
                self.agent_sessions_ui.open();
                None
            }
        };
        if let Some(request) = request {
            self.handle_agent_sessions_request(request);
        }
    }

    /// warm 풀이 max_warm(설정)을 넘으면 가장 오래된 것부터 Suspended로 내린다 (워커
    /// shutdown, 세션 종료 — §14.1 Suspended). background 스레드에서 정리하고 on_exit에서 join.
    /// **live 세션(미종료 셸/에이전트)이 있는 workspace는 축출하지 않는다** — 진행 중
    /// 작업을 경고 없이 kill하지 않기 위해 상한 초과를 허용한다 (메모리 < 작업 보호).
    fn evict_warm(&mut self) {
        self.warm_eviction_deferred = false;
        let max_warm = self.config.performance.max_warm as usize;
        let evictable = warm_eviction_candidates(&self.warm_order, max_warm, |id| {
            self.warm.get(id).is_some_and(|rt| rt.has_live_sessions())
        });
        for evict_id in evictable {
            self.suspend_warm_workspace(&evict_id, false);
            if self.warm_eviction_deferred {
                break;
            }
        }
        self.refresh_warm_idle_deadline();
    }

    fn refresh_warm_idle_deadline(&mut self) {
        if self.warm_eviction_deferred {
            self.next_warm_idle_eviction_at = None;
            return;
        }
        let previous = self.next_warm_idle_eviction_at;
        let next = self
            .warm
            .values()
            .filter(|runtime| {
                !runtime.has_live_sessions() || runtime.can_auto_suspend_idle_shells()
            })
            .filter_map(|runtime| {
                runtime
                    .backgrounded_at
                    .and_then(|at| at.checked_add(Self::WARM_AUTO_SUSPEND_AFTER))
            })
            .min();
        self.next_warm_idle_eviction_at = next;
        if let Some(delay) =
            changed_deadline_repaint_delay(previous, next, std::time::Instant::now())
        {
            self.egui_ctx.request_repaint_after(delay);
        }
    }

    fn maintain_warm_evictions(&mut self, now: std::time::Instant) {
        let shutdown_finished = self.pending_shutdowns.reap_finished();
        if shutdown_finished && self.warm_eviction_deferred {
            self.evict_warm();
        }
        if !self.warm_eviction_deferred
            && self
                .next_warm_idle_eviction_at
                .is_some_and(|deadline| deadline <= now)
        {
            self.evict_idle_warm(now);
        }
    }

    fn evict_idle_warm(&mut self, now: std::time::Instant) {
        self.next_warm_idle_eviction_at = None;
        let resident_before = 1 + self.warm.len();
        let expired = expired_warm_workspace_ids(
            &self.warm_order,
            |id| self.warm.get(id).and_then(|rt| rt.backgrounded_at),
            now,
            Self::WARM_AUTO_SUSPEND_AFTER,
        );
        for id in expired {
            // 에이전트/자식 작업은 계속 보호한다. 30분 동안 background였고 resource
            // 샘플로 프롬프트 대기 셸 리더만 확인된 경우에만 셸을 재생성 가능한 상태로 내린다.
            if self
                .warm
                .get(&id)
                .is_some_and(|rt| rt.has_live_sessions() && !rt.can_auto_suspend_idle_shells())
            {
                continue;
            }
            self.suspend_warm_workspace(&id, true);
        }
        if 1 + self.warm.len() != resident_before {
            self.broadcast_terminal_cache_policy();
        }
        self.refresh_warm_idle_deadline();
    }

    fn suspend_warm_workspace(&mut self, workspace_id: &str, allow_idle_shells: bool) {
        if !self.warm.contains_key(workspace_id) {
            self.warm_order.retain(|id| id != workspace_id);
            self.refresh_warm_idle_deadline();
            return;
        }
        // A slow PTY/process reap must not let repeated warm evictions create an unbounded set of
        // shutdown threads. Keep the runtime warm until a fixed slot is available.
        if !self.pending_shutdowns.can_start() {
            self.warm_eviction_deferred = true;
            self.next_warm_idle_eviction_at = None;
            return;
        }
        if let Some(mut rt) = self.warm.remove(workspace_id) {
            self.warm_order.retain(|id| id != workspace_id);
            // 마지막으로 큐에 남은 이벤트를 처리해 방금 끝난 background 작업의 완료/오류
            // 알림을 놓치지 않는다 (codex 리뷰 — 축출 시 receiver drop으로 유실되던 것).
            let events = rt.events.drain();
            let approval_events_overflowed = rt.events.take_overflowed();
            if approval_events_overflowed {
                self.runtime_stream_warning = true;
            }
            Self::record_activity_events(&mut rt, &events);
            self.observe_approval_runtime_events(workspace_id, &events);
            if approval_events_overflowed {
                self.fail_closed_approval_event_overflow();
            }
            let agent_providers = rt.workspace_ui.agent_providers();
            Self::process_ws_notifications(
                &mut self.notifications_ui,
                workspace_id,
                &events,
                &mut rt.session_titles,
                &agent_providers,
                &self.i18n,
            );
            // 최종 방어: 마지막 drain에서 새 spawn/자식 작업이 관측됐을 수 있다.
            // 일반 축출은 live를 모두 보호하고, timeout 축출도 안전한 idle 셸 조건을
            // 다시 만족할 때만 진행한다.
            let has_live = rt.has_live_sessions();
            let idle_shells = allow_idle_shells && rt.can_auto_suspend_idle_shells();
            if has_live && !idle_shells {
                tracing::info!(
                    workspace_id,
                    "suspend 취소 — 실행 중 세션이 있어 warm 유지 (작업 보호)"
                );
                // drain한 lifecycle 이벤트를 replay 큐에 보존 — 버리면 재활성 시
                // exit/status 상태가 UI에 재구성되지 않는다 (codex Medium).
                rt.pending_events.extend(events.into_iter().filter(|event| {
                    !matches!(event, runtime::RuntimeEvent::AgentSpawnResolved { .. })
                }));
                self.warm.insert(workspace_id.to_owned(), rt);
                self.warm_order.push(workspace_id.to_owned());
                self.refresh_warm_idle_deadline();
                return;
            }
            if idle_shells {
                tracing::info!(workspace_id, "30분 유휴 셸 workspace를 suspend");
            }
            // 축출 = Suspended(워커 종료) — 그 workspace의 진행형 알림은 더는 조치
            // 불가하므로 정리한다 (결과 알림은 기록이라 유지, codex 리뷰).
            self.notifications_ui.prune_transient(workspace_id);
            let evict_id = workspace_id.to_owned();
            let wake = self.egui_ctx.clone();
            let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let completion = PendingShutdownCompletion {
                completed: Arc::clone(&completed),
                wake,
            };
            match std::thread::Builder::new()
                .name("workspace-shutdown".to_owned())
                .spawn(move || {
                    let _completion = completion;
                    let mut runtime = rt.runtime;
                    let _ = runtime.send_command(runtime::RuntimeCommand::SetWorkspaceState(
                        runtime::WorkspaceRuntimeState::Suspended,
                    ));
                    runtime.shutdown();
                }) {
                Ok(handle) => self.pending_shutdowns.register(evict_id, completed, handle),
                Err(_) => tracing::warn!(
                    kind = "runtime",
                    phase = "shutdown_start",
                    error_code = "thread_spawn_failed",
                    "workspace shutdown could not start"
                ),
            }
            self.refresh_warm_idle_deadline();
        }
    }

    /// 주어진 workspace의 대기 중 background shutdown들을 join한다 (같은 workspace 워커가
    /// 동시에 두 개 살아 layout 행을 경합하지 않도록). 다른 workspace 것은 남겨 둔다.
    fn join_pending_shutdown(&mut self, workspace_id: &str) {
        self.pending_shutdowns.join_workspace(workspace_id);
    }

    /// 워크스페이스의 세션(pane)을 전부 닫는다 — 사이드바 「워크스페이스 종료」 확정 경로.
    /// 워크스페이스 자체(경로·설정·DB 기록)는 보존한다(설정의 「프로젝트 삭제」와 구분).
    fn close_workspace_sessions(&mut self, workspace_id: &str) {
        if workspace_id == self.active.id {
            // 활성: 전 pane을 확인 없이 즉시 닫는다(확인은 모달이 이미 했다). 워크스
            // 페이스는 활성인 채 빈 상태로 남는다 — 바로 새 셸을 열 수 있다.
            let panes: Vec<runtime::MuxPaneId> = self
                .active
                .workspace_ui
                .mux()
                .map(|mux| {
                    mux.tabs
                        .iter()
                        .flat_map(|tab| tab.panes.iter().map(|p| p.id.clone()))
                        .collect()
                })
                .unwrap_or_default();
            tracing::info!(
                workspace = %workspace_id,
                panes = panes.len(),
                "워크스페이스 종료 — 활성 pane 일괄 닫기"
            );
            // 종료 숨김 표식 — 닫히는 pane 집합을 기록한다. ClosePane은 비동기라 이
            // pane들은 exit 이벤트가 돌아올 때까지 몇 프레임 mux에 남는데, 이 기록으로
            // "죽어가는 pane"과 이후의 진짜 새 세션(숨김 해제 조건)을 구분한다.
            self.closed_workspaces.insert(
                workspace_id.to_owned(),
                ClosedWorkspaceState::ClosingPanes(
                    panes.iter().map(|pane| pane.0.clone()).collect(),
                ),
            );
            for pane in panes {
                let runtime_instance = self.active.runtime_instance;
                self.drain_workspace_protocol_intents(runtime_instance);
                self.active.workspace_ui.close_pane_now(pane);
                self.drain_workspace_protocol_intents(runtime_instance);
            }
        } else if self.warm.contains_key(workspace_id) {
            // warm 종료는 runtime shutdown까지 동기로 끝난다 — 죽어가는 pane 추적 불필요.
            self.closed_workspaces
                .insert(workspace_id.to_owned(), ClosedWorkspaceState::Persisted);
            // warm: 살아 있는 워커에 ClosePane을 모두 보낸 뒤 runtime을 내린다(idle 전환,
            // 프로젝트 삭제의 warm 종료 경로와 같은 순서). worker 루프는 shutdown 판정
            // 전에 큐 명령을 전부 소화하므로 pane 정리(persist layout 갱신)가 종료 전에
            // 반영된다 — 재활성 시 세션이 부활하지 않는다. 한계: warm 동안 새로 생긴
            // pane은 mux 스냅샷이 얼어 못 찾는다(워크트리 삭제 경로와 동일) — layout에
            // 남아 재활성 시 fresh 셸로만 뜬다.
            self.join_pending_shutdown(workspace_id);
            if let Some(mut rt) = self.warm.remove(workspace_id) {
                let panes: Vec<runtime::MuxPaneId> = rt
                    .workspace_ui
                    .mux()
                    .map(|mux| {
                        mux.tabs
                            .iter()
                            .flat_map(|tab| tab.panes.iter().map(|p| p.id.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                tracing::info!(
                    workspace = %workspace_id,
                    panes = panes.len(),
                    "워크스페이스 종료 — warm pane 정리 후 runtime shutdown"
                );
                for pane in panes {
                    Self::drain_closing_workspace_protocol_intents(&mut rt);
                    rt.workspace_ui.close_pane_now(pane);
                    Self::drain_closing_workspace_protocol_intents(&mut rt);
                }
                self.close_approval_workspace(workspace_id);
                rt.runtime.shutdown();
            }
            self.warm_order.retain(|id| id != workspace_id);
            self.refresh_warm_idle_deadline();
            self.broadcast_terminal_cache_policy();
            // 내려간 워크스페이스의 진행형 알림은 더는 조치 불가 — suspend와 같은 정리
            // (결과 알림은 기록이라 유지).
            self.notifications_ui.prune_transient(workspace_id);
            // persisted pane 스냅샷 재적재 — shutdown()이 worker join까지 하므로 빈
            // 레이아웃 저장이 끝난 뒤다. 안 하면 이제 비활성이 된 이 워크스페이스가
            // 사이드바/홈/웹 대시보드에 옛 세션 수로 계속 표시된다(codex P2).
            self.refresh_workspaces();
        } else {
            // 확인 모달이 떠 있는 사이 auto-suspend 등으로 이미 내려간 경우 — 조용히
            // 지나가지 않고 로그로 남긴다(닫을 세션이 없으니 실행할 것도 없다).
            // 사용자가 종료를 확정했으므로 숨김 표식은 동일하게 남긴다.
            self.closed_workspaces
                .insert(workspace_id.to_owned(), ClosedWorkspaceState::Persisted);
            tracing::info!(
                workspace = %workspace_id,
                "워크스페이스 종료 생략 — 이미 비활성(세션 없음)"
            );
        }
        // 재빌드·앱 재시작 뒤에도 종료한 워크스페이스가 되살아나지 않게 ID를 config에
        // 영속화한다. pane 집합은 위의 현재 프로세스 종료 판정에만 필요하다.
        self.config
            .ui
            .closed_workspace_ids
            .insert(workspace_id.to_owned());
        if let Err(error) = self.config.save(&self.config_path) {
            tracing::warn!(workspace = %workspace_id, "종료 워크스페이스 저장 실패: {error:#}");
        }
        // 홈/상태바가 최대 500ms 전 activity_rows를 재사용하므로 종료 직후 즉시 폐기한다.
        self.activity_rows_cache = None;
    }

    /// 종료 숨김을 해제하고 config에도 즉시 반영한다. 같은 활성 워크스페이스 재선택처럼
    /// switch의 나머지 저장 경로를 타지 않는 경우도 있어 이 함수가 직접 저장한다.
    fn reveal_closed_workspace(&mut self, workspace_id: &str) -> bool {
        let changed = clear_closed_workspace_state(
            &mut self.closed_workspaces,
            &mut self.config.ui.closed_workspace_ids,
            workspace_id,
        );
        if changed {
            self.activity_rows_cache = None;
            if let Err(error) = self.config.save(&self.config_path) {
                tracing::warn!(workspace = %workspace_id, "종료 워크스페이스 재열기 저장 실패: {error:#}");
            }
        }
        changed
    }

    fn reveal_active_workspace_for_new_session(&mut self) {
        let workspace_id = self.active.id.clone();
        self.reveal_closed_workspace(&workspace_id);
    }

    /// 활성 workspace의 `.env`를 환경 profile로 동기화하고, 그 env를 워커 기본 env로
    /// 전송한다(SetSessionDefaultEnv). 파일/SQLite/keyring 작업은 전용 worker에서 수행하고
    /// 이 메서드는 bounded/coalesced 요청만 넣으므로 UI thread를 막지 않는다.
    fn sync_dotenv_env(&mut self) {
        self.request_dotenv_sync(true);
    }

    fn stage_runtime_restore(&mut self, runtime_instance: u64) {
        if self
            .stage_dotenv_continuation(
                runtime_instance,
                PendingDotenvContinuation::RuntimeCommand(
                    runtime::RuntimeCommand::RestoreWorkspace,
                ),
            )
            .is_err()
        {
            tracing::warn!(
                kind = "workspace",
                phase = "restore_admission",
                error_code = "backpressure",
                "workspace restore failed closed"
            );
        }
    }

    /// 설정 창에서 선택한 workspace의 `.env` 동기화를 bounded worker에 제출한다.
    /// 활성 workspace는 runtime 기본 env 갱신까지 수행하는 기존 dotenv worker를 쓰고,
    /// 비활성 workspace는 Settings worker가 DB/keyring/file I/O를 전담한다.
    fn sync_settings_workspace_dotenv(&mut self, workspace_id: &str) -> bool {
        if workspace_id == self.active.id {
            self.sync_dotenv_env();
            return true;
        }
        let Some(root) = self.workspace_tree_root(workspace_id) else {
            return false;
        };
        self.queue_settings_action(workspace_id, Some(root), SettingsJobAction::ResyncDotenv)
    }

    fn request_dotenv_sync(&mut self, force: bool) {
        let workspace_id = self.active.id.clone();
        let root = self.active_tree_root();
        let runtime_instance = self.active.runtime_instance;
        let context = (workspace_id.clone(), root.clone(), runtime_instance);
        let context_changed = self.dotenv_sync_context.as_ref() != Some(&context);
        if context_changed {
            self.dotenv_sync_generation = self.dotenv_sync_generation.wrapping_add(1);
            self.dotenv_sync_context = Some(context);
            self.last_dotenv_state = None;
        }
        if force || context_changed {
            self.dotenv_sync_revision = self.dotenv_sync_revision.wrapping_add(1);
        }
        self.dotenv_next_operation_id = self.dotenv_next_operation_id.wrapping_add(1);
        let correlation = crate::dotenv_sync::DotenvWorkerCorrelation::new(
            self.dotenv_sync_generation,
            self.dotenv_sync_revision,
            self.dotenv_next_operation_id,
        );
        let job = DotenvSyncJob {
            workspace_id,
            root,
            runtime_instance,
            previous_state: self.last_dotenv_state,
            force,
            migrate_legacy: force,
        };
        match self.dotenv_sync_worker.request_state(correlation, job) {
            Ok(replaced) => {
                if let Some(replaced) = replaced {
                    // The latest-only state slot intentionally displaced this bounded job.
                    let _ = replaced.into_parts();
                }
            }
            Err(error) => {
                let (code, rejected_correlation, _rejected_job) = error.into_parts();
                debug_assert_eq!(rejected_correlation, correlation);
                tracing::warn!(
                    kind = "dotenv",
                    phase = "admission",
                    error_code = %code,
                    "dotenv synchronization admission failed"
                );
                self.last_dotenv_state = None;
            }
        }
    }

    fn stage_dotenv_continuation(
        &mut self,
        runtime_instance: u64,
        mut continuation: PendingDotenvContinuation,
    ) -> Result<(), Box<PendingDotenvContinuation>> {
        if self.dotenv_pending_operations.len()
            >= crate::dotenv_sync::DOTENV_WORKER_CONTINUATION_MAX
        {
            return Err(Box::new(continuation));
        }
        let Ok(retained_bytes) = prepare_dotenv_continuation_retention(&mut continuation) else {
            return Err(Box::new(continuation));
        };
        let Ok(pending_bytes) = runtime::checked_runtime_command_retention_total(
            self.dotenv_pending_bytes,
            retained_bytes,
        ) else {
            return Err(Box::new(continuation));
        };
        let Some((workspace_id, previous_state)) = self
            .runtime_by_instance(runtime_instance)
            .map(|runtime| (runtime.id.clone(), runtime.dotenv_state))
        else {
            return Err(Box::new(continuation));
        };
        let migrate_legacy = matches!(
            &continuation,
            PendingDotenvContinuation::RuntimeCommand(runtime::RuntimeCommand::RestoreWorkspace)
        );
        let root = self.workspace_tree_root(&workspace_id);
        self.dotenv_next_operation_id = self.dotenv_next_operation_id.wrapping_add(1).max(1);
        let operation_id = self.dotenv_next_operation_id;
        if self.dotenv_pending_operations.contains_key(&operation_id) {
            return Err(Box::new(continuation));
        }
        let correlation = crate::dotenv_sync::DotenvWorkerCorrelation::new(
            runtime_instance,
            operation_id,
            operation_id,
        );
        self.dotenv_pending_operations.insert(
            operation_id,
            PendingDotenvOperation {
                correlation,
                workspace_id: workspace_id.clone(),
                root: root.clone(),
                runtime_instance,
                retained_bytes,
                continuation,
            },
        );
        self.dotenv_pending_bytes = pending_bytes;
        let job = DotenvSyncJob {
            workspace_id,
            root,
            runtime_instance,
            previous_state,
            force: false,
            migrate_legacy,
        };
        if let Err(error) = self
            .dotenv_sync_worker
            .request_continuation(correlation, job)
        {
            let (code, rejected_correlation, _rejected_job) = error.into_parts();
            debug_assert_eq!(rejected_correlation, correlation);
            tracing::warn!(
                kind = "dotenv",
                phase = "continuation_admission",
                error_code = %code,
                "dotenv launch admission failed"
            );
            let pending = self
                .dotenv_pending_operations
                .remove(&operation_id)
                .expect("pending dotenv operation inserted above");
            self.dotenv_pending_bytes = self
                .dotenv_pending_bytes
                .checked_sub(pending.retained_bytes)
                .expect("dotenv pending byte ledger is balanced");
            return Err(Box::new(pending.continuation));
        }
        Ok(())
    }

    fn runtime_by_instance(&self, runtime_instance: u64) -> Option<&WorkspaceRuntime> {
        if self.active.runtime_instance == runtime_instance {
            return Some(&self.active);
        }
        self.warm
            .values()
            .find(|runtime| runtime.runtime_instance == runtime_instance)
    }

    fn runtime_by_instance_mut(&mut self, runtime_instance: u64) -> Option<&mut WorkspaceRuntime> {
        if self.active.runtime_instance == runtime_instance {
            return Some(&mut self.active);
        }
        self.warm
            .values_mut()
            .find(|runtime| runtime.runtime_instance == runtime_instance)
    }

    fn finish_dotenv_continuation(
        &mut self,
        pending: PendingDotenvOperation,
        outcome: Result<DotenvSyncOutcome, crate::dotenv_sync::DotenvWorkerErrorCode>,
    ) {
        let current_root = self.workspace_tree_root(&pending.workspace_id);
        let outcome = outcome.ok().filter(|outcome| {
            outcome.workspace_id == pending.workspace_id
                && outcome.root == pending.root
                && outcome.runtime_instance == pending.runtime_instance
                && current_root == pending.root
                && dotenv_state_for_root(pending.root.as_deref()) == outcome.baseline
        });
        let is_agent_launch = matches!(
            pending.continuation,
            PendingDotenvContinuation::AgentLaunch { .. }
        );
        let agent_ticket = match &pending.continuation {
            PendingDotenvContinuation::AgentLaunch {
                approval_ticket, ..
            } => *approval_ticket,
            _ => None,
        };
        let launcher_request_id = match &pending.continuation {
            PendingDotenvContinuation::AgentLaunch {
                launcher_request_id,
                ..
            } => *launcher_request_id,
            _ => None,
        };
        let Some(outcome) = outcome else {
            if let PendingDotenvContinuation::WorkspaceProtocol {
                operation,
                generation,
                ..
            } = pending.continuation
                && let Some(runtime) = self.runtime_by_instance_mut(pending.runtime_instance)
            {
                runtime.workspace_ui.complete_protocol(
                    ui::workspace::WorkspaceProtocolCompletion {
                        operation,
                        generation,
                        result: Err(ui::workspace::WorkspaceProtocolErrorCode::DeliveryFailed),
                    },
                );
            }
            if let Some(ticket_id) = agent_ticket {
                self.approval_launch_tracker.cancel(ticket_id);
            }
            if is_agent_launch {
                self.agents_ui
                    .report_error(ui::agents::AgentsUiErrorCode::LaunchFailed);
            }
            if let Some(request_id) = launcher_request_id {
                self.fail_agent_launcher_request(request_id);
            }
            tracing::warn!(
                kind = "dotenv",
                phase = "continuation",
                error_code = "source_unavailable",
                "dotenv-gated launch failed closed"
            );
            return;
        };
        let baseline = outcome.baseline;
        let mut payload = outcome.payload;
        if let Some(ticket_id) = agent_ticket
            && !self
                .approval_launch_tracker
                .mark_spawn_sent(ticket_id, std::time::Instant::now())
        {
            self.agents_ui
                .report_error(ui::agents::AgentsUiErrorCode::LaunchFailed);
            if let Some(request_id) = launcher_request_id {
                self.fail_agent_launcher_request(request_id);
            }
            tracing::warn!(
                kind = "agent",
                phase = "spawn_admission",
                error_code = "stale_ticket",
                "agent launch ticket expired before delivery"
            );
            return;
        }
        let live_reload = self.config.ui.env_live_reload;
        let cache_policy = self.terminal_cache_policy_command();
        if let Some(payload) = &mut payload {
            if let Some(report) = payload.report.take()
                && report.upserted + report.removed > 0
            {
                tracing::info!(
                    upserted = report.upserted,
                    removed = report.removed,
                    "dotenv launch synchronization"
                );
            }
            if live_reload && let Some(root) = pending.root.as_deref() {
                payload
                    .env_plain
                    .push(("DEPPY_ENV_LIVE_RELOAD".to_owned(), "1".to_owned()));
                payload
                    .env_plain
                    .push(("DEPPY_PROJECT_ROOT".to_owned(), root.display().to_string()));
            }
        }
        let Some(runtime) = self.runtime_by_instance_mut(pending.runtime_instance) else {
            if let Some(ticket_id) = agent_ticket {
                self.approval_launch_tracker.cancel(ticket_id);
            }
            if is_agent_launch {
                self.agents_ui
                    .report_error(ui::agents::AgentsUiErrorCode::LaunchFailed);
            }
            if let Some(request_id) = launcher_request_id {
                self.fail_agent_launcher_request(request_id);
            }
            tracing::warn!(
                kind = "dotenv",
                phase = "continuation",
                error_code = "stale_runtime",
                "dotenv launch target became stale"
            );
            return;
        };
        let env_delivered = match payload {
            Some(payload) => runtime
                .runtime
                .send_command(runtime::RuntimeCommand::SetSessionDefaultEnv {
                    env_plain: payload.env_plain,
                    env_secrets: payload.env_secrets,
                })
                .is_ok(),
            None => true,
        };
        if env_delivered {
            runtime.dotenv_state = Some(baseline);
        }
        let policy_delivered = env_delivered && runtime.runtime.send_command(cache_policy).is_ok();
        let delivered = match pending.continuation {
            PendingDotenvContinuation::WorkspaceProtocol {
                operation,
                generation,
                command,
            } => {
                let delivered = policy_delivered && runtime.runtime.send_command(command).is_ok();
                runtime.workspace_ui.complete_protocol(
                    ui::workspace::WorkspaceProtocolCompletion {
                        operation,
                        generation,
                        result: delivered
                            .then_some(())
                            .ok_or(ui::workspace::WorkspaceProtocolErrorCode::DeliveryFailed),
                    },
                );
                delivered
            }
            PendingDotenvContinuation::RuntimeCommand(command)
            | PendingDotenvContinuation::AgentLaunch { command, .. } => {
                policy_delivered && runtime.runtime.send_command(command).is_ok()
            }
        };
        if delivered {
            self.invalidate_env_profile_ui();
            self.credentials_ui.invalidate_cache();
            self.invalidate_env_api_projects();
            if is_agent_launch {
                self.agents_ui.mark_launch_accepted();
                self.reveal_active_workspace_for_new_session();
            }
        } else {
            if let Some(ticket_id) = agent_ticket {
                self.approval_launch_tracker.cancel(ticket_id);
            }
            if is_agent_launch {
                self.agents_ui
                    .report_error(ui::agents::AgentsUiErrorCode::LaunchFailed);
            }
            if let Some(request_id) = launcher_request_id {
                self.fail_agent_launcher_request(request_id);
            }
            tracing::warn!(
                kind = "dotenv",
                phase = "continuation",
                error_code = "delivery_failed",
                "dotenv-gated launch failed closed"
            );
        }
    }

    fn poll_dotenv_sync(&mut self) {
        while let Some(worker_outcome) = self.dotenv_sync_worker.try_recv() {
            if worker_outcome.is_continuation() {
                let operation_id = worker_outcome.operation_id();
                let Some(pending) = self.dotenv_pending_operations.remove(&operation_id) else {
                    continue;
                };
                self.dotenv_pending_bytes = self
                    .dotenv_pending_bytes
                    .checked_sub(pending.retained_bytes)
                    .expect("dotenv pending byte ledger is balanced");
                let result = worker_outcome
                    .into_current(
                        pending.correlation.generation(),
                        pending.correlation.revision(),
                    )
                    .and_then(|outcome| outcome.into_result());
                self.finish_dotenv_continuation(pending, result);
                continue;
            }
            let Ok(worker_outcome) =
                worker_outcome.into_current(self.dotenv_sync_generation, self.dotenv_sync_revision)
            else {
                continue;
            };
            let outcome = match worker_outcome.into_result() {
                Ok(outcome) => outcome,
                Err(error) => {
                    self.last_dotenv_state = None;
                    tracing::warn!(
                        kind = "dotenv",
                        phase = "completion",
                        error_code = %error,
                        "dotenv synchronization failed closed"
                    );
                    continue;
                }
            };
            let current = self.dotenv_sync_context.as_ref().is_some_and(|context| {
                context.0 == outcome.workspace_id
                    && context.1 == outcome.root
                    && context.2 == outcome.runtime_instance
                    && self.active.id == outcome.workspace_id
                    && self.active.runtime_instance == outcome.runtime_instance
            });
            if !current {
                continue;
            }
            if dotenv_state_for_root(outcome.root.as_deref()) != outcome.baseline {
                self.last_dotenv_state = None;
                tracing::warn!(
                    kind = "dotenv",
                    phase = "completion_verify",
                    error_code = "source_changed",
                    "stale dotenv synchronization result discarded"
                );
                continue;
            }
            self.last_dotenv_state = Some(outcome.baseline);
            match outcome.payload {
                None => {}
                Some(payload) => {
                    if let Some(report) = payload.report
                        && report.upserted + report.removed > 0
                    {
                        tracing::info!(
                            upserted = report.upserted,
                            removed = report.removed,
                            ".env → 환경 profile 동기화"
                        );
                    }
                    // .env 라이브 반영(E5 ⑨) 활성 조건 — 새 셸의 precmd 훅이 이
                    // 두 값으로 깨어난다. 토글/경로 변경이 다음 동기화에 반영된다.
                    let mut env_plain = payload.env_plain;
                    if self.config.ui.env_live_reload
                        && let Some(root) = outcome.root.as_deref()
                    {
                        env_plain.push(("DEPPY_ENV_LIVE_RELOAD".to_owned(), "1".to_owned()));
                        env_plain
                            .push(("DEPPY_PROJECT_ROOT".to_owned(), root.display().to_string()));
                    }
                    let env_ready = self
                        .active
                        .runtime
                        .send_command(runtime::RuntimeCommand::SetSessionDefaultEnv {
                            env_plain,
                            env_secrets: payload.env_secrets,
                        })
                        .is_ok();
                    self.invalidate_env_profile_ui();
                    self.credentials_ui.invalidate_cache();
                    self.invalidate_env_api_projects();
                    if env_ready {
                        self.active.dotenv_state = Some(outcome.baseline);
                    } else {
                        // Never acknowledge the source stamp until the exact runtime instance has
                        // accepted its default environment.
                        self.last_dotenv_state = None;
                    }
                }
            }
        }
    }

    /// 설정의 exited cap / **프로세스 전역** 캐시 예산을 워커 정책 명령으로 만든다.
    /// 각 runtime은 자기 세션만 볼 수 있으므로 active+warm resident 수로 균등 분배해
    /// 합산 허용량이 설정값을 넘지 않게 한다(§14.3 확장).
    fn terminal_cache_policy_command(&self) -> runtime::RuntimeCommand {
        runtime::RuntimeCommand::SetTerminalCachePolicy {
            max_exited_backends: self.config.terminal.exited_backend_cap as usize,
            cache_budget_bytes: per_runtime_cache_budget_bytes(
                self.config.terminal.cache_budget_mb,
                1 + self.warm.len(),
            ),
        }
    }

    /// 캐시 정책을 활성 + warm 워커 전체에 반영한다 (설정 또는 resident 수 변경 시).
    fn broadcast_terminal_cache_policy(&mut self) {
        let command = self.terminal_cache_policy_command();
        let _ = self.active.runtime.send_command(command.clone());
        for rt in self.warm.values() {
            let _ = rt.runtime.send_command(command.clone());
        }
    }

    /// 폴더의 (dev, ino)를 읽는다(inode 앵커용). 유효 디렉터리가 아니면 None.
    fn folder_anchor(path: &str) -> Option<(i64, i64)> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::metadata(path).ok()?;
            if !meta.is_dir() {
                return None;
            }
            Some((meta.dev() as i64, meta.ino() as i64))
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            None
        }
    }

    /// 지정 workspace의 프로젝트 폴더 앵커(dev,ino)를 현재 경로 기준으로 저장한다.
    fn save_workspace_anchor_for(&mut self, workspace_id: &str) {
        let anchor = self
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .map(|workspace| workspace.path.as_str())
            .filter(|path| !path.trim().is_empty())
            .and_then(Self::folder_anchor);
        if self
            .db
            .set_workspace_anchor(workspace_id, anchor.map(|a| a.0), anchor.map(|a| a.1))
            .is_ok()
        {
            match anchor {
                Some((dev, ino)) => {
                    self.workspace_anchors.insert(
                        workspace_id.to_owned(),
                        storage::WorkspaceFolderAnchor { dev, ino },
                    );
                }
                None => {
                    self.workspace_anchors.remove(workspace_id);
                }
            }
        }
    }

    /// 활성 workspace의 프로젝트 폴더 앵커(dev,ino)를 저장한다.
    fn save_workspace_anchor(&mut self) {
        let workspace_id = self.active.id.clone();
        self.save_workspace_anchor_for(&workspace_id);
    }

    /// 프로젝트 폴더 rename/이동 감지. 새 session-cwd projection이 도착했을 때만 실행한다.
    /// 저장된 경로가 stale(사라짐)이고, 세션 cwd 중
    /// 저장된 앵커(dev,ino)와 일치하는 폴더가 있으면 → 그 폴더가 이동된 새 경로다. 확인 모달로
    /// 제안한다(사용자 요청 2026-07-08). 앵커 없으면(구 워크스페이스) 유효 경로일 때 backfill.
    fn detect_workspace_folder_rename(&mut self) {
        if self.workspace_rename_prompt.is_some() {
            return; // 이미 확인 대기 중
        }
        let ws = self.active.id.clone();
        let Some(path) = self
            .workspaces
            .iter()
            .find(|workspace| workspace.id == ws)
            .map(|workspace| workspace.path.clone())
            .filter(|path| !path.trim().is_empty())
        else {
            return; // 폴더 미설정 — 감지 대상 아님
        };
        let anchor = self
            .workspace_anchors
            .get(&ws)
            .map(|anchor| (anchor.dev, anchor.ino));
        let current = Self::folder_anchor(&path); // path가 유효 디렉터리면 그 (dev,ino)
        // 정상 상태 판정: path가 유효하고 앵커가 없거나(backfill) 앵커와 inode가 일치.
        if let Some(cur) = current {
            match anchor {
                None => self.save_workspace_anchor(), // 구 워크스페이스 backfill
                Some(a) if a == cur => {}             // 정상 — 같은 폴더
                Some(_) => {
                    // path는 유효하지만 inode가 다르다 = 원래 폴더가 이동되고 그 자리에 다른
                    // 폴더가 들어섰다(mv proj proj.old && mkdir proj). stale로 보고 매칭 진행.
                    self.propose_rename_by_anchor(&ws, &path, anchor);
                    return;
                }
            }
            self.dismissed_renames.remove(&ws); // 경로 정상화 → 무시 상태 해제
            return;
        }
        // path가 사라짐(stale) → 앵커로 이동 위치 탐색.
        self.propose_rename_by_anchor(&ws, &path, anchor);
    }

    /// 저장 앵커(dev,ino)와 일치하는 세션 cwd를 찾아 rename 복구 모달을 제안한다.
    fn propose_rename_by_anchor(&mut self, ws: &str, old_path: &str, anchor: Option<(i64, i64)>) {
        if self.dismissed_renames.contains(ws) {
            return;
        }
        let Some(anchor) = anchor else {
            return; // 앵커 없음 → 자동 복구 불가(재선택 안내는 파일트리/환경메뉴가 담당)
        };
        for cwd in self.session_cwds.values() {
            // 후보는 현재 저장 경로와 달라야 한다(같으면 이동 아님).
            if cwd != old_path && Self::folder_anchor(cwd) == Some(anchor) {
                self.workspace_rename_prompt = Some((old_path.to_owned(), cwd.clone()));
                return;
            }
        }
    }

    /// 지정 workspace의 프로젝트 루트. App이 이미 소유한 immutable workspace projection을
    /// 사용해 render/action 경로에서 SQLite를 다시 조회하지 않는다.
    fn workspace_tree_root(&self, workspace_id: &str) -> Option<PathBuf> {
        self.workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .and_then(|workspace| Self::workspace_path_to_tree_root(Some(workspace.path.clone())))
    }

    /// 활성 workspace의 트리 루트 (path 미설정/조회 실패 → None → 안내 표시 §9-2).
    fn active_tree_root(&self) -> Option<PathBuf> {
        self.workspace_tree_root(&self.active.id)
    }

    /// T1: focused pane 세션의 현재 작업 폴더 — agent_detect 워커(lsof)가 채운
    /// `session_cwds`를 재사용한다 (새 감지 메커니즘 없음).
    fn focused_session_cwd(&self) -> Option<String> {
        self.active
            .workspace_ui
            .focused_session()
            .and_then(|sid| self.session_cwds.get(&sid))
            .cloned()
    }

    /// T1: pane 우클릭 → 환경설정 진입 시점에 focused 세션 cwd를 감지해 배너 상태를
    /// 만든다. cwd가 없거나 폴더가 아니면 None. 비교는 canonicalize 기준
    /// (macOS `/var`↔`/private/var`, 심링크 등)으로 하되 실패 시 원경로로 폴백.
    fn detect_session_cwd_banner(&self, cwd: &str) -> Option<EnvSessionCwdBanner> {
        let cwd = PathBuf::from(cwd);
        if !cwd.is_dir() {
            return None;
        }
        let cwd_canon = std::fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());
        let roots: Vec<std::path::PathBuf> = self
            .workspaces
            .iter()
            .filter(|ws| !ws.path.trim().is_empty())
            .map(|ws| {
                let p = std::path::PathBuf::from(ws.path.trim());
                std::fs::canonicalize(&p).unwrap_or(p)
            })
            .collect();
        Some(EnvSessionCwdBanner {
            registered: cwd_belongs_to_any(&cwd_canon, &roots),
            cwd,
        })
    }

    /// 워크스페이스 표시 이름 (2026-07-18): **별칭 우선** — 사용자가 지정한
    /// 별칭(name 컬럼, 비어있지 않고 "default" 아님)이 있으면 폴더명 병기 없이
    /// 별칭만 보여준다(사이드바 「이름 바꾸기」 요구). 별칭이 없으면 E3(2026-07-13)
    /// 규칙대로 프로젝트 폴더명에서 파생, 그것도 없으면 "~". 실제 폴더/경로는
    /// 이 함수가 절대 건드리지 않는다 — 표시 전용. 자동 폴더명 추종
    /// (update_workspace_folder_name)은 경로·별칭이 모두 없는 부트스트랩에만
    /// 남아 있어 사용자 별칭을 덮어쓰지 않는다.
    fn workspace_display_name(row: &storage::WorkspaceRow) -> String {
        let alias = row.name.trim();
        let alias = (!alias.is_empty() && alias != "default").then_some(alias);
        let folder = {
            let path = row.path.trim();
            (!path.is_empty())
                .then(|| std::path::Path::new(path).file_name())
                .flatten()
                .map(|base| base.to_string_lossy().into_owned())
        };
        match (folder, alias) {
            (_, Some(alias)) => alias.to_owned(),
            (Some(folder), None) => folder,
            (None, None) => "~".to_owned(),
        }
    }

    fn active_workspace_display_name(&self) -> String {
        self.workspaces
            .iter()
            .find(|workspace| workspace.id == self.active.id)
            .map(Self::workspace_display_name)
            .unwrap_or_else(|| self.active.id.clone())
    }

    fn open_agent_launcher_for_active(&mut self) {
        if self.pending_agent_launcher_launch.is_some() {
            return;
        }
        self.agent_launcher_seen_workspaces
            .insert(self.active.id.clone());
        self.agent_launcher_ui
            .open_for(self.active.id.clone(), self.active_workspace_display_name());
        if self.agent_launcher_snapshot.is_none() {
            self.agent_launcher_detection_requested = true;
        }
        self.egui_ctx.request_repaint();
    }

    fn offer_agent_launcher_for_active(&mut self) {
        if self.pending_agent_launcher_launch.is_some() {
            return;
        }
        if self
            .agent_launcher_seen_workspaces
            .insert(self.active.id.clone())
        {
            self.agent_launcher_ui
                .open_for(self.active.id.clone(), self.active_workspace_display_name());
            if self.agent_launcher_snapshot.is_none() {
                self.agent_launcher_detection_requested = true;
            }
            self.egui_ctx.request_repaint();
        }
    }

    fn poll_agent_launcher_detection(&mut self) {
        while let Some(outcome) = self.agent_launcher_worker.try_recv() {
            self.agent_launcher_detection_in_flight = false;
            match outcome.into_result() {
                Ok(snapshot) => {
                    self.agent_launcher_snapshot = Some(snapshot);
                    self.agent_launcher_ui.detection_succeeded();
                }
                Err(_) if self.agent_launcher_ui.is_open() => self
                    .agent_launcher_ui
                    .report_error(ui::agent_launcher::LauncherErrorCode::DetectionFailed),
                Err(_) => {}
            }
        }
        if !self.agent_launcher_detection_requested || self.agent_launcher_detection_in_flight {
            return;
        }
        match self.agent_launcher_worker.try_request(()) {
            Ok(()) => {
                self.agent_launcher_detection_requested = false;
                self.agent_launcher_detection_in_flight = true;
            }
            Err(crate::lazy_worker::LazyWorkerSubmitError::Full(())) => {}
            Err(crate::lazy_worker::LazyWorkerSubmitError::Unavailable { .. }) => {
                self.agent_launcher_detection_requested = false;
                if self.agent_launcher_ui.is_open() {
                    self.agent_launcher_ui
                        .report_error(ui::agent_launcher::LauncherErrorCode::DetectionFailed);
                }
            }
        }
    }

    fn handle_agent_launcher_intent(&mut self, intent: ui::agent_launcher::AgentLauncherIntent) {
        match intent {
            ui::agent_launcher::AgentLauncherIntent::Refresh => {
                self.agent_launcher_detection_requested = true;
            }
            ui::agent_launcher::AgentLauncherIntent::BlankTerminal { workspace_id } => {
                if workspace_id == self.active.id {
                    self.reveal_active_workspace_for_new_session();
                    self.active
                        .workspace_ui
                        .spawn_shell(self.config.terminal.scrollback_lines as usize);
                }
            }
            ui::agent_launcher::AgentLauncherIntent::Launch {
                workspace_id,
                kind,
                options,
            } => {
                if self.pending_agent_launcher_launch.is_some() {
                    self.agent_launcher_ui
                        .report_error(ui::agent_launcher::LauncherErrorCode::LaunchBusy);
                    return;
                }
                if workspace_id != self.active.id {
                    self.agent_launcher_ui
                        .report_error(ui::agent_launcher::LauncherErrorCode::AgentUnavailable);
                    return;
                }
                let shim = (self.config.ui.agent_status_hooks && kind.supports_deppy_shim())
                    .then(crate::agent_shim::shim_dir)
                    .flatten()
                    .map(|directory| directory.join(kind.id()));
                let spec = self
                    .agent_launcher_snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.find(kind))
                    .ok_or(())
                    .and_then(|agent| {
                        crate::agent_launcher::build_launch_spec(agent, options, shim.as_deref())
                            .map_err(|_| ())
                    });
                let Ok(spec) = spec else {
                    self.agent_launcher_ui
                        .report_error(ui::agent_launcher::LauncherErrorCode::AgentUnavailable);
                    return;
                };
                self.next_agent_launcher_request_id =
                    self.next_agent_launcher_request_id.wrapping_add(1).max(1);
                let request_id = self.next_agent_launcher_request_id;
                let queued = self.queue_global_settings_action(
                    &workspace_id,
                    SettingsJobAction::PrepareQuickAgentLaunch {
                        request_id,
                        spec,
                        runtime_workspace_id: workspace_id.clone(),
                    },
                );
                if queued {
                    self.pending_agent_launcher_launch = Some(PendingAgentLauncherLaunch {
                        request_id,
                        workspace_id,
                        agent_config_id: kind.stable_config_id().to_owned(),
                    });
                } else {
                    self.agent_launcher_ui
                        .report_error(ui::agent_launcher::LauncherErrorCode::LaunchBusy);
                }
            }
        }
    }

    fn fail_agent_launcher_request(&mut self, request_id: u64) {
        if self
            .pending_agent_launcher_launch
            .as_ref()
            .is_some_and(|pending| pending.request_id == request_id)
        {
            self.pending_agent_launcher_launch = None;
            self.agent_launcher_ui
                .report_error(ui::agent_launcher::LauncherErrorCode::LaunchFailed);
        }
    }

    fn observe_agent_launcher_runtime_events(
        &mut self,
        workspace_id: &str,
        events: &[runtime::RuntimeEvent],
    ) {
        for event in events {
            let runtime::RuntimeEvent::AgentSpawnResolved {
                agent_config_id,
                session,
            } = event
            else {
                continue;
            };
            let Some(_) = take_matching_agent_launcher_launch(
                &mut self.pending_agent_launcher_launch,
                workspace_id,
                agent_config_id.as_str(),
            ) else {
                continue;
            };
            if session.is_some() {
                self.agent_launcher_ui.launch_succeeded();
            } else {
                self.agent_launcher_ui
                    .report_error(ui::agent_launcher::LauncherErrorCode::LaunchFailed);
            }
        }
    }

    fn upsert_workspace_projection(&mut self, row: storage::SettingsWorkspaceProjectionRow) {
        let anchor = row.folder_anchor;
        let workspace = storage::WorkspaceRow {
            id: row.id,
            name: row.name,
            path: row.path,
            created_at: row.created_at,
        };
        match self
            .workspaces
            .iter_mut()
            .find(|existing| existing.id == workspace.id)
        {
            Some(existing) => *existing = workspace.clone(),
            None => self.workspaces.push(workspace.clone()),
        }
        self.workspaces.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then(left.id.cmp(&right.id))
        });
        match anchor {
            Some(anchor) => {
                self.workspace_anchors.insert(workspace.id, anchor);
            }
            None => {
                self.workspace_anchors.remove(&workspace.id);
            }
        }
        self.invalidate_env_api_projects();
    }

    fn update_workspace_projection_name(&mut self, workspace_id: &str, name: String) {
        if let Some(workspace) = self
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
        {
            workspace.name = name;
        }
        self.invalidate_env_api_projects();
    }

    fn update_workspace_projection_path(&mut self, workspace_id: &str, path: &Path) {
        if let Some(workspace) = self
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
        {
            workspace.path = path.to_string_lossy().into_owned();
        }
        match Self::folder_anchor(&path.to_string_lossy()) {
            Some((dev, ino)) => {
                self.workspace_anchors.insert(
                    workspace_id.to_owned(),
                    storage::WorkspaceFolderAnchor { dev, ino },
                );
            }
            None => {
                self.workspace_anchors.remove(workspace_id);
            }
        }
        self.invalidate_env_api_projects();
    }

    fn invalidate_env_api_projects(&mut self) {
        self.env_api_projects_cache = None;
        self.env_project_rows_generation = self.env_project_rows_generation.wrapping_add(1);
        self.env_project_rows_failed = false;
    }

    fn invalidate_env_profile_ui(&mut self) {
        self.pending_settings_job = None;
        self.pending_env_secret_reveal = None;
        self.env_profiles_ui.invalidate_cache();
        self.agents_ui.invalidate_snapshot_selection();
        self.settings_snapshot_generation = self.settings_snapshot_generation.wrapping_add(1);
        self.settings_snapshot_pending = false;
        self.settings_pending_operation = None;
        self.settings_snapshot_retry_at = None;
        self.settings_snapshot_workspace_id = None;
        self.agents_snapshot =
            ui::agents::AgentsSnapshot::unavailable(self.settings_snapshot_revision);
        let workspace_id = self
            .settings_workspace_id
            .as_deref()
            .unwrap_or(&self.active.id)
            .to_owned();
        self.env_profiles_snapshot = ui::env_profiles::EnvProfilesSnapshot::unavailable(
            self.settings_snapshot_revision,
            workspace_id,
            false,
        );
        self.credentials_snapshot =
            ui::credentials::CredentialsSnapshot::unavailable(self.settings_snapshot_revision);
        self.credentials_ui.clear_revealed_secrets();
        self.env_secret_generation = self.env_secret_generation.wrapping_add(1);
    }

    fn request_settings_snapshot_if_needed(
        &mut self,
        workspace_id: &str,
        project_root: Option<PathBuf>,
    ) {
        let now = std::time::Instant::now();
        if self.settings_snapshot_workspace_id.as_deref() == Some(workspace_id) {
            match self.settings_snapshot_retry_at {
                Some(retry_at) if retry_at <= now && !self.settings_snapshot_pending => {
                    self.settings_snapshot_retry_at = None;
                }
                _ => return,
            }
        }
        if self.settings_snapshot_workspace_id.is_some() {
            self.settings_snapshot_generation = self.settings_snapshot_generation.wrapping_add(1);
            self.settings_snapshot_pending = false;
            self.settings_pending_operation = None;
            self.settings_snapshot_retry_at = None;
            self.settings_snapshot_workspace_id = None;
            self.agents_ui.invalidate_snapshot_selection();
            self.env_profiles_ui.invalidate_cache();
            self.agents_snapshot =
                ui::agents::AgentsSnapshot::unavailable(self.settings_snapshot_revision);
            self.env_profiles_snapshot = ui::env_profiles::EnvProfilesSnapshot::unavailable(
                self.settings_snapshot_revision,
                workspace_id,
                project_root.is_some(),
            );
            self.credentials_snapshot =
                ui::credentials::CredentialsSnapshot::unavailable(self.settings_snapshot_revision);
        }
        if self.settings_snapshot_pending {
            return;
        }
        if self.pending_settings_job.is_some() {
            return;
        }
        if self.settings_snapshot_generation == 0 {
            self.settings_snapshot_generation = 1;
        }
        self.settings_snapshot_revision = self.settings_snapshot_revision.wrapping_add(1);
        let job = SettingsJob {
            generation: self.settings_snapshot_generation,
            revision: self.settings_snapshot_revision,
            workspace_id: workspace_id.to_owned(),
            project_root,
            action: SettingsJobAction::Load,
        };
        let operation = SettingsOperationKey::for_job(&job);
        self.pending_settings_job = Some(job);
        self.settings_snapshot_workspace_id = Some(workspace_id.to_owned());
        self.settings_snapshot_pending = true;
        self.settings_pending_operation = Some(operation);
    }

    fn queue_settings_action(
        &mut self,
        workspace_id: &str,
        project_root: Option<PathBuf>,
        action: SettingsJobAction,
    ) -> bool {
        if self.settings_snapshot_pending
            || self.pending_settings_job.is_some()
            || self.settings_snapshot_workspace_id.as_deref() != Some(workspace_id)
        {
            return false;
        }
        self.settings_snapshot_revision = self.settings_snapshot_revision.wrapping_add(1);
        let job = SettingsJob {
            generation: self.settings_snapshot_generation,
            revision: self.settings_snapshot_revision,
            workspace_id: workspace_id.to_owned(),
            project_root,
            action,
        };
        let operation = SettingsOperationKey::for_job(&job);
        self.pending_settings_job = Some(job);
        self.settings_snapshot_pending = true;
        self.settings_pending_operation = Some(operation);
        true
    }

    fn queue_global_settings_action(
        &mut self,
        workspace_id: &str,
        action: SettingsJobAction,
    ) -> bool {
        if self.settings_snapshot_pending || self.pending_settings_job.is_some() {
            return false;
        }
        if self.settings_snapshot_generation == 0 {
            self.settings_snapshot_generation = 1;
        }
        self.settings_snapshot_revision = self.settings_snapshot_revision.wrapping_add(1);
        let job = SettingsJob {
            generation: self.settings_snapshot_generation,
            revision: self.settings_snapshot_revision,
            workspace_id: workspace_id.to_owned(),
            project_root: None,
            action,
        };
        let operation = SettingsOperationKey::for_job(&job);
        self.pending_settings_job = Some(job);
        self.settings_snapshot_pending = true;
        self.settings_pending_operation = Some(operation);
        true
    }

    fn poll_settings_job_admission(&mut self) {
        let Some(job) = self.pending_settings_job.take() else {
            return;
        };
        if let Err(job) = self.settings_snapshot_worker.try_request_recover(job) {
            self.pending_settings_job = Some(*job);
        }
    }

    fn send_prepared_agent_launch(
        &mut self,
        prepared: PreparedAgentLaunch,
        launcher_request_id: Option<u64>,
    ) -> bool {
        let ticket_id = prepared.approval_ticket;
        let launcher_request_is_current = launcher_request_id.is_none_or(|request_id| {
            self.pending_agent_launcher_launch
                .as_ref()
                .is_some_and(|pending| {
                    pending.request_id == request_id
                        && pending.workspace_id == prepared.runtime_workspace_id
                        && pending.agent_config_id == prepared.agent_config_id
                })
        });
        if !launcher_request_is_current {
            return false;
        }
        if prepared.runtime_workspace_id != self.active.id || prepared.proxy.is_some() {
            if let Some(ticket_id) = ticket_id {
                self.approval_launch_tracker.cancel(ticket_id);
            }
            self.agents_ui
                .report_error(ui::agents::AgentsUiErrorCode::LaunchFailed);
            if let Some(request_id) = launcher_request_id {
                self.fail_agent_launcher_request(request_id);
            }
            return false;
        }
        let command = runtime::RuntimeCommand::SpawnAgent {
            agent_config_id: Some(prepared.agent_config_id),
            cols: 80,
            rows: 24,
            scrollback_lines: self.config.terminal.scrollback_lines as usize,
            command: prepared.command,
            args: prepared.args,
            env_plain: prepared.env_plain,
            env_secrets: prepared.env_secrets,
            waiting_regex: prepared.waiting_regex,
            approval_regex: prepared.approval_regex,
            error_regex: prepared.error_regex,
            done_regex: prepared.done_regex,
        };
        let runtime_instance = self.active.runtime_instance;
        if self
            .stage_dotenv_continuation(
                runtime_instance,
                PendingDotenvContinuation::AgentLaunch {
                    command,
                    approval_ticket: ticket_id,
                    launcher_request_id,
                },
            )
            .is_err()
        {
            if let Some(ticket_id) = ticket_id {
                self.approval_launch_tracker.cancel(ticket_id);
            }
            self.agents_ui
                .report_error(ui::agents::AgentsUiErrorCode::LaunchFailed);
            if let Some(request_id) = launcher_request_id {
                self.fail_agent_launcher_request(request_id);
            }
            return false;
        }
        true
    }

    fn stage_prepared_proxy_launch(
        &mut self,
        generation: u64,
        settings_workspace_id: String,
        mut prepared: PreparedAgentLaunch,
    ) {
        let now = std::time::Instant::now();
        let ticket_id = match self.approval_launch_tracker.reserve(
            prepared.runtime_workspace_id.clone(),
            prepared.agent_config_id.clone(),
            now,
        ) {
            Ok(ticket_id) => ticket_id,
            Err(_) => {
                self.agents_ui
                    .report_error(ui::agents::AgentsUiErrorCode::LaunchFailed);
                return;
            }
        };
        prepared.approval_ticket = Some(ticket_id);
        if self.approval_wake_hub.ensure_started().is_err() {
            self.approval_launch_tracker.cancel(ticket_id);
            self.agents_ui
                .report_error(ui::agents::AgentsUiErrorCode::LaunchFailed);
            return;
        }
        self.pending_proxy_launches.push_back(PendingProxyLaunch {
            generation,
            workspace_id: settings_workspace_id,
            prepared,
        });
        self.egui_ctx.request_repaint_after(APPROVAL_SPAWN_DEADLINE);
    }

    fn observe_approval_runtime_events(
        &mut self,
        workspace_id: &str,
        events: &[runtime::RuntimeEvent],
    ) {
        let mut changed = false;
        for event in events {
            match event {
                runtime::RuntimeEvent::AgentSpawnResolved {
                    agent_config_id,
                    session,
                } => {
                    changed |= self.approval_launch_tracker.correlate(
                        workspace_id,
                        agent_config_id.as_str(),
                        *session,
                    );
                }
                runtime::RuntimeEvent::SessionExited { session, .. } => {
                    changed |= self
                        .approval_launch_tracker
                        .observe_session_exit(workspace_id, *session);
                }
                _ => {}
            }
        }
        if changed {
            self.egui_ctx.request_repaint();
        }
    }

    fn fail_pending_proxy_launches(&mut self) {
        let mut failed = false;
        while let Some(launch) = self.pending_proxy_launches.pop_front() {
            if let Some(ticket_id) = launch.prepared.approval_ticket {
                self.approval_launch_tracker.cancel(ticket_id);
            }
            failed = true;
        }
        if failed {
            self.agents_ui
                .report_error(ui::agents::AgentsUiErrorCode::LaunchFailed);
        }
    }

    fn fail_closed_approval_event_overflow(&mut self) {
        if self.approval_launch_tracker.is_empty() && self.pending_proxy_launches.is_empty() {
            return;
        }
        self.fail_pending_proxy_launches();
        self.approval_launch_tracker.clear_fail_closed();
        let _ = self.agents_ui.take_pending();
        self.active.pending_agent_spawns = 0;
        for runtime in self.warm.values_mut() {
            runtime.pending_agent_spawns = 0;
        }
        self.agents_ui
            .report_error(ui::agents::AgentsUiErrorCode::LaunchFailed);
        self.approval_global_reconcile.request();
        self.queue_global_approval_reconcile_if_due();
    }

    fn queue_global_approval_reconcile_if_due(&mut self) {
        let now = std::time::Instant::now();
        if !self.approval_global_reconcile.is_due(now) {
            return;
        }
        match self
            .approval_wake_hub
            .enqueue(ApprovalWorkerCommand::DenyAllOwned {
                resolved_at: deppy_core::time::unix_secs_i64(),
            }) {
            Ok(()) => self.approval_global_reconcile.mark_queued(),
            Err(_) => {
                if let Some(delay) = self.approval_global_reconcile.finish(false, now) {
                    self.egui_ctx.request_repaint_after(delay);
                }
            }
        }
    }

    fn close_approval_workspace(&mut self, workspace_id: &str) {
        let canceled = self.approval_launch_tracker.close_workspace(workspace_id);
        self.pending_proxy_launches
            .retain(|launch| launch.prepared.runtime_workspace_id != workspace_id);
        if canceled > 0 {
            self.agents_ui
                .report_error(ui::agents::AgentsUiErrorCode::LaunchFailed);
        }
        if !self
            .approval_launch_tracker
            .pending_denials(std::time::Instant::now())
            .is_empty()
        {
            self.egui_ctx.request_repaint();
        }
    }

    fn poll_approval_wake(&mut self) {
        let ready_path = match self.approval_wake_hub.poll_ready() {
            Ok(path) => path,
            Err(_) => {
                self.fail_pending_proxy_launches();
                self.approval_wake_hub.stop();
                if self.approval_global_reconcile.required
                    && let Some(delay) = self
                        .approval_global_reconcile
                        .finish(false, std::time::Instant::now())
                {
                    self.egui_ctx.request_repaint_after(delay);
                }
                tracing::warn!(
                    kind = "approval",
                    phase = "listener",
                    error_code = "unavailable",
                    "승인 listener를 사용할 수 없어 proxy launch를 중단"
                );
                return;
            }
        };

        if let Some(snapshot) = self.approval_wake_hub.take_snapshot() {
            self.apply_approval_snapshot(snapshot);
        }
        for result in self.approval_wake_hub.drain_results() {
            match result {
                ApprovalWorkerResult::Resolved => {}
                ApprovalWorkerResult::SessionDenied {
                    workspace_id,
                    session,
                    succeeded,
                } => {
                    if let Some(delay) = self.approval_launch_tracker.finish_session(
                        &workspace_id,
                        session,
                        succeeded,
                        std::time::Instant::now(),
                    ) {
                        self.egui_ctx.request_repaint_after(delay);
                    }
                }
                ApprovalWorkerResult::AllDenied { succeeded } => {
                    if let Some(delay) = self
                        .approval_global_reconcile
                        .finish(succeeded, std::time::Instant::now())
                    {
                        self.egui_ctx.request_repaint_after(delay);
                    }
                }
                ApprovalWorkerResult::Failed => tracing::warn!(
                    kind = "approval",
                    phase = "worker",
                    error_code = "operation_failed",
                    "승인 worker 작업 실패"
                ),
            }
        }

        self.queue_global_approval_reconcile_if_due();

        let expired = self
            .approval_launch_tracker
            .expire(std::time::Instant::now());
        if !expired.is_empty() {
            self.pending_proxy_launches.retain(|launch| {
                launch
                    .prepared
                    .approval_ticket
                    .is_none_or(|ticket_id| !expired.contains(&ticket_id))
            });
            self.agents_ui
                .report_error(ui::agents::AgentsUiErrorCode::LaunchFailed);
        }

        for (workspace_id, session) in self
            .approval_launch_tracker
            .pending_denials(std::time::Instant::now())
        {
            if self
                .approval_wake_hub
                .enqueue(ApprovalWorkerCommand::DenySession {
                    workspace_id: workspace_id.clone(),
                    session,
                    resolved_at: deppy_core::time::unix_secs_i64(),
                })
                .is_ok()
            {
                self.approval_launch_tracker
                    .mark_deny_queued(&workspace_id, session);
            }
        }

        if let Some(socket_path) = ready_path
            && !self.settings_snapshot_pending
            && let Some(launch) = self.pending_proxy_launches.pop_front()
        {
            let stale = launch.generation != self.settings_snapshot_generation
                || self.settings_snapshot_workspace_id.as_deref()
                    != Some(launch.workspace_id.as_str())
                || launch.prepared.runtime_workspace_id != self.active.id;
            if stale {
                if let Some(ticket_id) = launch.prepared.approval_ticket {
                    self.approval_launch_tracker.cancel(ticket_id);
                }
                self.agents_ui
                    .report_error(ui::agents::AgentsUiErrorCode::LaunchFailed);
            } else {
                self.settings_snapshot_revision = self.settings_snapshot_revision.wrapping_add(1);
                let job = SettingsJob {
                    generation: launch.generation,
                    revision: self.settings_snapshot_revision,
                    workspace_id: launch.workspace_id.clone(),
                    project_root: None,
                    action: SettingsJobAction::FinalizeProxyAgentLaunch {
                        prepared: Box::new(launch.prepared),
                        approval_notify_socket: socket_path,
                    },
                };
                let operation = SettingsOperationKey::for_job(&job);
                match self.settings_snapshot_worker.try_request_recover(job) {
                    Ok(()) => {
                        self.settings_snapshot_pending = true;
                        self.settings_pending_operation = Some(operation);
                    }
                    Err(job) => {
                        let job = *job;
                        if let SettingsJobAction::FinalizeProxyAgentLaunch { prepared, .. } =
                            job.action
                        {
                            self.pending_proxy_launches.push_front(PendingProxyLaunch {
                                generation: job.generation,
                                workspace_id: job.workspace_id,
                                prepared: *prepared,
                            });
                        }
                    }
                }
            }
        }

        if self.pending_proxy_launches.is_empty()
            && self.approval_launch_tracker.is_empty()
            && !self.approval_global_reconcile.required
            && self.approval_wake_hub.is_idle()
        {
            self.approval_wake_hub.stop();
        }
    }

    fn poll_settings_outcomes(&mut self) {
        while let Some(outcome) = self.settings_snapshot_worker.try_recv() {
            let projection_current = outcome.generation == self.settings_snapshot_generation
                && self.settings_snapshot_workspace_id.as_deref()
                    == Some(outcome.workspace_id.as_str());
            if self
                .settings_pending_operation
                .as_ref()
                .is_some_and(|operation| operation.matches_outcome(&outcome))
            {
                self.settings_snapshot_pending = false;
                self.settings_pending_operation = None;
            }
            let is_load = matches!(&outcome.kind, SettingsOutcomeKind::Loaded);
            if projection_current {
                if let Some(snapshots) = outcome.snapshots {
                    self.agents_snapshot = snapshots.agents;
                    self.env_profiles_snapshot = snapshots.env;
                    self.credentials_snapshot = snapshots.credentials;
                    if is_load {
                        self.settings_snapshot_retry_at = None;
                    }
                } else if is_load {
                    self.agents_snapshot =
                        ui::agents::AgentsSnapshot::unavailable(outcome.revision);
                    self.env_profiles_snapshot = ui::env_profiles::EnvProfilesSnapshot::unavailable(
                        outcome.revision,
                        outcome.workspace_id.as_str(),
                        false,
                    );
                    self.credentials_snapshot =
                        ui::credentials::CredentialsSnapshot::unavailable(outcome.revision);
                    let delay = std::time::Duration::from_secs(1);
                    self.settings_snapshot_retry_at = Some(std::time::Instant::now() + delay);
                    self.egui_ctx.request_repaint_after(delay);
                }
            }
            match outcome.kind {
                SettingsOutcomeKind::Loaded => {}
                SettingsOutcomeKind::CredentialAdded(result) => match result {
                    Ok(()) => {
                        self.credentials_ui.add_succeeded();
                        self.invalidate_env_api_projects();
                    }
                    Err(_) => self
                        .credentials_ui
                        .report_error(ui::credentials::CredentialsUiErrorCode::AddFailed),
                },
                SettingsOutcomeKind::CredentialDeleted {
                    credential_id,
                    result,
                } => match result {
                    Ok(()) => {
                        self.credentials_ui.delete_succeeded(&credential_id);
                        self.invalidate_env_api_projects();
                    }
                    Err(_) => self
                        .credentials_ui
                        .report_error(ui::credentials::CredentialsUiErrorCode::DeleteFailed),
                },
                SettingsOutcomeKind::CredentialRevealed {
                    credential_id,
                    result,
                } => match result {
                    Ok(revealed) => {
                        let _ = self.credentials_ui.accept_revealed(revealed);
                    }
                    Err(_) => self.credentials_ui.reject_reveal(&credential_id),
                },
                SettingsOutcomeKind::OrphanCredentialsScanned(result) => match result {
                    Ok(ids) => {
                        let _ = self.credentials_ui.accept_orphan_scan(ids);
                    }
                    Err(_) => self
                        .credentials_ui
                        .report_error(ui::credentials::CredentialsUiErrorCode::OrphanScanFailed),
                },
                SettingsOutcomeKind::OrphanCredentialsPurged {
                    purged,
                    remaining,
                    result,
                } => {
                    if result.is_ok() {
                        self.credentials_ui
                            .orphan_purge_succeeded(purged, remaining);
                    } else {
                        self.credentials_ui.report_error(
                            ui::credentials::CredentialsUiErrorCode::OrphanPurgeFailed,
                        );
                    }
                }
                SettingsOutcomeKind::CodexLlmApiKeySaved(result) => {
                    if result.is_ok() {
                        let revision = self
                            .agent_sessions_secrets_snapshot
                            .revision()
                            .wrapping_add(1);
                        self.agent_sessions_secrets_snapshot =
                            ui::agent_sessions::AgentSessionsSecretsSnapshot::new(revision, true);
                        self.agent_sessions_ui.api_key_save_succeeded();
                    } else {
                        self.agent_sessions_ui.report_api_key_error(
                            ui::agent_sessions::AgentSessionsSecretErrorCode::SaveFailed,
                        );
                    }
                }
                SettingsOutcomeKind::CodexLlmApiKeyDeleted(result) => {
                    if result.is_ok() {
                        let revision = self
                            .agent_sessions_secrets_snapshot
                            .revision()
                            .wrapping_add(1);
                        self.agent_sessions_secrets_snapshot =
                            ui::agent_sessions::AgentSessionsSecretsSnapshot::new(revision, false);
                        self.agent_sessions_ui.api_key_delete_succeeded();
                    } else {
                        self.agent_sessions_ui.report_api_key_error(
                            ui::agent_sessions::AgentSessionsSecretErrorCode::DeleteFailed,
                        );
                    }
                }
                SettingsOutcomeKind::AgentRegistered(result) => match result {
                    Ok(()) => self.agents_ui.registration_succeeded(),
                    Err(_) => self
                        .agents_ui
                        .report_error(ui::agents::AgentsUiErrorCode::RegistrationFailed),
                },
                SettingsOutcomeKind::AgentDeleted(result) => {
                    if result.is_err() {
                        self.agents_ui
                            .report_error(ui::agents::AgentsUiErrorCode::DeleteFailed);
                    }
                }
                SettingsOutcomeKind::AgentLaunchPrepared(result) => match result {
                    Ok(prepared) => {
                        if prepared.proxy.is_some() {
                            self.stage_prepared_proxy_launch(
                                outcome.generation,
                                outcome.workspace_id,
                                prepared,
                            );
                        } else {
                            self.send_prepared_agent_launch(prepared, None);
                        }
                    }
                    Err(_) => self
                        .agents_ui
                        .report_error(ui::agents::AgentsUiErrorCode::LaunchFailed),
                },
                SettingsOutcomeKind::QuickAgentLaunchPrepared { request_id, result } => {
                    match result {
                        Ok(prepared) => {
                            self.send_prepared_agent_launch(prepared, Some(request_id));
                        }
                        Err(_) => self.fail_agent_launcher_request(request_id),
                    }
                }
                SettingsOutcomeKind::AgentLaunchFinalized { ticket_id, result } => match result {
                    Ok(prepared) => {
                        self.send_prepared_agent_launch(prepared, None);
                    }
                    Err(_) => {
                        if let Some(ticket_id) = ticket_id {
                            self.approval_launch_tracker.cancel(ticket_id);
                        }
                        self.agents_ui
                            .report_error(ui::agents::AgentsUiErrorCode::LaunchFailed);
                    }
                },
                SettingsOutcomeKind::LegacyVarDeleted(result) => {
                    if result.is_err() {
                        self.env_profiles_ui
                            .report_error(ui::env_profiles::EnvUiErrorCode::LegacyDeleteFailed);
                    } else {
                        self.invalidate_env_api_projects();
                    }
                }
                SettingsOutcomeKind::DotenvWritten(result) => {
                    if result.is_err() {
                        self.env_profiles_ui
                            .report_error(ui::env_profiles::EnvUiErrorCode::SnapshotUnavailable);
                    } else {
                        self.sync_settings_workspace_dotenv(&outcome.workspace_id);
                        self.invalidate_env_api_projects();
                    }
                }
                SettingsOutcomeKind::DotenvResynced(result) => {
                    if result.is_err() {
                        self.env_profiles_ui
                            .report_error(ui::env_profiles::EnvUiErrorCode::SnapshotUnavailable);
                    } else {
                        self.credentials_ui.invalidate_cache();
                        self.invalidate_env_api_projects();
                    }
                }
                SettingsOutcomeKind::ProjectPathSet(result) => {
                    let Ok(row) = result else {
                        self.env_profiles_ui
                            .report_error(ui::env_profiles::EnvUiErrorCode::SnapshotUnavailable);
                        continue;
                    };
                    let path = PathBuf::from(&row.path);
                    self.upsert_workspace_projection(row);
                    self.dismissed_renames.remove(&outcome.workspace_id);
                    self.sync_settings_workspace_dotenv(&outcome.workspace_id);
                    if outcome.workspace_id == self.active.id {
                        let cwd = path.is_dir().then_some(path.clone());
                        let _ = self
                            .active
                            .runtime
                            .send_command(runtime::RuntimeCommand::SetShellCwd(cwd));
                        self.refresh_file_tree_root();
                    }
                    self.credentials_ui.invalidate_cache();
                    self.invalidate_env_api_projects();
                }
                SettingsOutcomeKind::WorkspaceRenamed { name, result } => {
                    if result.is_ok() {
                        self.update_workspace_projection_name(&outcome.workspace_id, name);
                    } else {
                        tracing::warn!(
                            workspace = %outcome.workspace_id,
                            "workspace rename worker failed"
                        );
                    }
                }
                SettingsOutcomeKind::WorkspaceFoundOrCreated { purpose, result } => {
                    let Ok(result) = result else {
                        tracing::warn!("workspace find-or-create worker failed");
                        continue;
                    };
                    let workspace_id = result.row.id.clone();
                    let created = result.created;
                    self.upsert_workspace_projection(result.row);
                    match purpose {
                        WorkspaceMutationPurpose::SelectInSettings => {
                            self.config.ui.hidden_env_project_ids.remove(&workspace_id);
                            if created {
                                self.closed_workspaces
                                    .insert(workspace_id.clone(), ClosedWorkspaceState::Persisted);
                                self.config
                                    .ui
                                    .closed_workspace_ids
                                    .insert(workspace_id.clone());
                            }
                            self.settings_workspace_id = Some(workspace_id);
                            if let Err(error) = self.config.save(&self.config_path) {
                                tracing::warn!("settings workspace state save failed: {error:#}");
                            }
                            self.invalidate_env_profile_ui();
                        }
                        WorkspaceMutationPurpose::SwitchRuntime => {
                            self.reveal_closed_workspace(&workspace_id);
                            if workspace_id != self.active.id {
                                self.switch_workspace(&workspace_id);
                            }
                            if created {
                                self.active
                                    .workspace_ui
                                    .spawn_shell(self.config.terminal.scrollback_lines as usize);
                            }
                        }
                    }
                }
                SettingsOutcomeKind::WorkspaceMovedPathAccepted { new_path, result } => {
                    if !matches!(result, Ok(storage::WorkspaceMovedPathUpdate::Updated)) {
                        tracing::info!("workspace moved-path confirmation became stale");
                        self.workspace_rename_prompt = None;
                        continue;
                    }
                    self.update_workspace_projection_path(&outcome.workspace_id, &new_path);
                    self.dismissed_renames.remove(&outcome.workspace_id);
                    self.sync_settings_workspace_dotenv(&outcome.workspace_id);
                    if outcome.workspace_id == self.active.id {
                        let cwd = new_path.is_dir().then_some(new_path);
                        let _ = self
                            .active
                            .runtime
                            .send_command(runtime::RuntimeCommand::SetShellCwd(cwd));
                        self.refresh_file_tree_root();
                    }
                    self.workspace_rename_prompt = None;
                }
            }
        }
    }

    fn poll_env_secret_reveals(&mut self) {
        while let Some(result) = self.env_secret_reveal_worker.try_recv() {
            let outcome = match result.into_result() {
                Ok(outcome) => outcome,
                Err(code) => {
                    tracing::warn!(
                        kind = "settings",
                        phase = "secret_reveal",
                        error_code = code.as_str(),
                        "settings auxiliary worker failed"
                    );
                    self.env_secret_generation = self.env_secret_generation.wrapping_add(1);
                    self.env_profiles_ui.invalidate_cache();
                    continue;
                }
            };
            if outcome.generation != self.env_secret_generation {
                continue;
            }
            match outcome.target {
                EnvSecretRevealTarget::EnvRow {
                    profile_id,
                    key,
                    credential_id: _,
                } => match outcome.value {
                    Ok(value) => {
                        match ui::env_profiles::RevealedEnvValue::new(
                            profile_id.clone(),
                            key.clone(),
                            value.expose().to_owned(),
                        ) {
                            Ok(revealed) => {
                                let _ = self.env_profiles_ui.accept_revealed(revealed);
                            }
                            Err(_) => self.env_profiles_ui.reject_reveal(&profile_id, &key),
                        }
                    }
                    Err(_) => self.env_profiles_ui.reject_reveal(&profile_id, &key),
                },
            }
        }
    }

    fn poll_env_secret_reveal_admission(&mut self) {
        let Some(job) = self.pending_env_secret_reveal.take() else {
            return;
        };
        if let Err(error) = self.env_secret_reveal_worker.try_request(job) {
            let error_code = error
                .error_code()
                .map_or("backpressure", |code| code.as_str());
            tracing::warn!(
                kind = "settings",
                phase = "secret_reveal_admission",
                error_code,
                "settings auxiliary worker rejected work"
            );
            match error.into_job().target {
                EnvSecretRevealTarget::EnvRow {
                    profile_id, key, ..
                } => self.env_profiles_ui.reject_reveal(&profile_id, &key),
            }
        }
    }

    /// Drains and requests the environment project projection from `logic()` only. Render reads
    /// the immutable Arc cache and never starts a worker or clones the workspace collection.
    fn poll_env_api_project_rows(&mut self) {
        while let Some(result) = self.env_project_rows_worker.try_recv() {
            let submitted_generation = self.env_project_rows_in_flight.take();
            let outcome = match result.into_result() {
                Ok(outcome) => outcome,
                Err(code) => {
                    if submitted_generation != Some(self.env_project_rows_generation) {
                        continue;
                    }
                    tracing::warn!(
                        kind = "settings",
                        phase = "environment_projection",
                        error_code = code.as_str(),
                        "settings auxiliary worker failed"
                    );
                    self.env_project_rows_failed = true;
                    self.env_api_projects_cache
                        .get_or_insert_with(|| Arc::from([]));
                    continue;
                }
            };
            if outcome.generation != self.env_project_rows_generation {
                continue;
            }
            match outcome.rows {
                Ok(rows) => {
                    self.env_project_rows_failed = false;
                    let rows = rows
                        .into_iter()
                        .filter(|project| {
                            !self.config.ui.hidden_env_project_ids.contains(&project.id)
                        })
                        .collect::<Vec<_>>();
                    self.env_api_projects_cache = Some(rows.into());
                }
                Err(_) => {
                    tracing::warn!(
                        kind = "settings",
                        phase = "environment_projection",
                        error_code = "bounded_read_failed",
                        "settings projection failed"
                    );
                    self.env_project_rows_failed = true;
                    self.env_api_projects_cache
                        .get_or_insert_with(|| Arc::from([]));
                }
            }
        }
        if self.settings_open
            && self.settings_category == ui::settings::Category::Environment
            && self.env_api_projects_cache.is_none()
            && self.env_project_rows_in_flight.is_none()
        {
            let generation = self.env_project_rows_generation;
            match self.env_project_rows_worker.try_request(EnvProjectRowsJob {
                generation,
                workspaces: self.workspaces.clone(),
            }) {
                Ok(()) => {
                    self.env_project_rows_in_flight = Some(generation);
                    self.env_project_rows_failed = false;
                }
                Err(error) => {
                    let error_code = error
                        .error_code()
                        .map_or("backpressure", |code| code.as_str());
                    tracing::warn!(
                        kind = "settings",
                        phase = "environment_projection_admission",
                        error_code,
                        "settings auxiliary worker rejected work"
                    );
                    self.env_project_rows_failed = true;
                    self.env_api_projects_cache
                        .get_or_insert_with(|| Arc::from([]));
                }
            }
        }
    }

    /// 포커스 세션 cwd → 워크스페이스 이름(현재 작업 폴더/프로젝트명)을 갱신·영속한다.
    /// 변경 시에만 DB에 쓴다(churn 방지). 감지 실패(빈 이름)면 이전 값을 유지한다 —
    /// 포커스가 다른 pane으로 옮겨가도 폴더명이 "~"로 리셋되지 않게(사용자 요청).
    fn update_workspace_folder_name(&mut self, cwd: &str) {
        // E3: 표시 이름은 프로젝트 폴더명에서 파생되고 name 컬럼은 사용자 별칭이다.
        // cwd 자동 추적은 **경로 미지정 + 별칭 없음** 워크스페이스의 부트스트랩에만
        // 남긴다 — 경로가 지정된 뒤 자동 추적이 별칭을 덮어쓰지 않게(별칭 보호).
        let keep = self
            .workspaces
            .iter()
            .find(|w| w.id == self.active.id)
            .is_none_or(|w| {
                !w.path.trim().is_empty()
                    || (!w.name.trim().is_empty() && w.name.trim() != "default")
            });
        if keep {
            return;
        }
        let Some(name) = self.activity_project_names.get(cwd).cloned().flatten() else {
            self.request_project_name_projection();
            return;
        };
        if name.is_empty() || name == "default" {
            return;
        }
        if let Err(e) = self.db.rename_workspace(&self.active.id, &name) {
            tracing::warn!("워크스페이스 폴더명 저장 실패: {e:#}");
            return;
        }
        self.refresh_workspaces();
    }

    fn workspace_path_to_tree_root(path: Option<String>) -> Option<PathBuf> {
        path.and_then(|path| (!path.trim().is_empty()).then(|| PathBuf::from(path)))
    }

    /// 활성 workspace 기준으로 파일 트리 상태를 새로 만든다 (ON 전환/루트 변경 시).
    fn make_file_tree(&self) -> ui::file_tree::FileTreeUi {
        let mut tree = ui::file_tree::FileTreeUi::new(self.egui_ctx.clone());
        // 앱 자신의 data dir(로그·DB·cert 등) 이벤트는 무시 — 로그 쓰기가 워처로 돌아와
        // 리페인트를 유발하는 자기-루프 차단 (workspace 루트가 홈 등 넓은 경로일 때).
        if let Some(data_dir) = self.db_path.parent() {
            tree.set_watch_ignore(vec![data_dir.to_path_buf()]);
        }
        // 워크스페이스 폴더 미설정이어도 파일트리는 **항상** 뜨게 — HOME으로 폴백(사용자
        // 2026-07-08). .env 동기화는 active_tree_root(폴더 미설정=None)를 따로 쓰므로
        // ~/.env를 자동 로드하진 않는다(트리 표시 루트와 .env 원천 분리).
        // 저장 경로가 존재하지 않으면(폴더 이동/삭제/최초 실행 stale) 에러 대신 HOME으로
        // 폴백 — "폴더 못 찾음" 에러가 뜨지 않게(사용자 2026-07-08). rename 감지는 별도.
        tree.set_root(
            self.active_tree_root()
                .filter(|p| p.is_dir())
                .or_else(crate::paths::home_dir),
        );
        tree
    }

    /// 활성 workspace의 트리 루트가 바뀌었을 수 있을 때 (전환/경로 저장) 파일 루트만
    /// 교체한다. FileTreeUi 전체를 재생성하면 workspace별 세션 펼침 상태까지 사라져,
    /// 다른 workspace를 선택하는 순간 이전 세션 트리가 자동으로 닫힌다.
    fn refresh_file_tree_root(&mut self) {
        let root = self
            .active_tree_root()
            .filter(|path| path.is_dir())
            .or_else(crate::paths::home_dir);
        if let Some(tree) = self.file_tree.as_mut() {
            tree.set_root(root);
        }
    }

    fn refresh_workspaces(&mut self) {
        match self.db.settings_workspace_projection_rows() {
            Ok(list) => {
                let mut anchors = std::collections::HashMap::with_capacity(list.len());
                self.workspaces = list
                    .into_iter()
                    .map(|row| {
                        if let Some(anchor) = row.folder_anchor {
                            anchors.insert(row.id.clone(), anchor);
                        }
                        storage::WorkspaceRow {
                            id: row.id,
                            name: row.name,
                            path: row.path,
                            created_at: row.created_at,
                        }
                    })
                    .collect();
                self.workspace_anchors = anchors;
            }
            Err(_) => tracing::warn!(
                kind = "settings",
                phase = "workspace_projection",
                error_code = "bounded_read_failed",
                "settings projection failed"
            ),
        }
        if self.settings_workspace_id.as_ref().is_some_and(|selected| {
            !self
                .workspaces
                .iter()
                .any(|workspace| workspace.id == *selected)
        }) {
            self.settings_workspace_id = None;
        }
        // 종료 숨김 표식 정리 — 프로젝트 삭제 등으로 목록에서 사라진 id의 표식을
        // 메모리와 config 양쪽에서 지워 유계로 유지한다.
        let workspaces = &self.workspaces;
        self.closed_workspaces
            .retain(|id, _| workspaces.iter().any(|workspace| workspace.id == *id));
        self.dismissed_renames
            .retain(|id| workspaces.iter().any(|workspace| workspace.id == *id));
        let persisted_before = self.config.ui.closed_workspace_ids.len();
        self.config
            .ui
            .closed_workspace_ids
            .retain(|id| workspaces.iter().any(|workspace| workspace.id == *id));
        let hidden_env_before = self.config.ui.hidden_env_project_ids.clone();
        self.config
            .ui
            .hidden_env_project_ids
            .retain(|id| workspaces.iter().any(|workspace| workspace.id == *id));
        if (self.config.ui.closed_workspace_ids.len() != persisted_before
            || self.config.ui.hidden_env_project_ids != hidden_env_before)
            && let Err(error) = self.config.save(&self.config_path)
        {
            tracing::warn!("삭제 워크스페이스 UI 숨김 표식 정리 저장 실패: {error:#}");
        }
        self.request_agent_state_scope();
        if self.agent_state_scope_ready() {
            self.stage_agent_state_projection(
                crate::agent_state_worker::AgentStateSection::Catalog,
                AppAgentStateProjectionKind::Catalog,
            );
        }
        // 워크스페이스 목록/이름/경로가 바뀌었을 수 있다 — env/API 프로젝트 행 캐시 무효화.
        self.invalidate_env_api_projects();
    }

    /// pressure 뱃지 표시 TTL — 회복 이벤트가 없어(큐가 빠져도 신호 없음) 마지막 관측이
    /// 이 시간보다 오래되면 해소된 것으로 보고 숨긴다. 종료성 사유는 SessionExited가 정리.
    const PRESSURE_TTL: std::time::Duration = std::time::Duration::from_secs(10);

    fn fresh_pressure(
        entry: Option<&(runtime::PtyInputPressure, std::time::Instant)>,
        now: std::time::Instant,
    ) -> Option<runtime::PtyInputPressure> {
        entry
            .filter(|(_, at)| now.saturating_duration_since(*at) < Self::PRESSURE_TTL)
            .map(|(p, _)| p.clone())
    }

    /// 비활성(warm/유휴) 워크스페이스의 pane 표시명 — 활성 워크스페이스의
    /// `resolve_session_title`과 같은 규칙: 사용자가 rename했으면 그대로, 기본 제목
    /// ("셸 N")이면 세션 cwd의 프로젝트명으로 대체한다. 감지 워커는 활성 워크스페이스만
    /// 돌지만 cwd는 worker가 DB에 영속하므로(UpdateSessionCwd) 여기서 재사용한다.
    /// cwd를 못 찾으면 기본 제목을 i18n 렌더한 값("셸 1")으로 폴백.
    fn activity_session_name(&self, workspace_id: &str, raw_title: &str) -> String {
        let cwd = self
            .persisted_activity_panes
            .get(workspace_id)
            .and_then(|panes| pane_cwd(panes, raw_title));
        activity_session_name(raw_title, cwd, &self.i18n, |cwd| {
            self.activity_project_names.get(cwd).cloned().flatten()
        })
    }

    /// 폭주 확정 알림 큐(active+warm)를 비워 OS 알림을 1회씩 발화한다 (로드맵 B2).
    /// notify-rust가 아닌 platform::notify(osascript)를 쓴다 — notify-rust는 번들
    /// 미해석 시 Finder 다이얼로그 취소가 FFI panic→abort로 앱을 죽인 전례가 있다
    /// (2026-07-05). logic()에서만 호출 — 렌더 경로 바깥.
    fn dispatch_storm_notifications(&mut self) {
        // 먼저 (워크스페이스, raw 제목, peak)을 모아 borrow를 풀고, 그다음 제목
        // 해석 + 알림을 한다(activity_session_name이 &self를 빌리므로).
        let mut drained: Vec<(String, Option<String>, usize)> = Vec::new();
        for workspace in std::iter::once(&mut self.active).chain(self.warm.values_mut()) {
            if workspace.storm_notify_pending.is_empty() {
                continue;
            }
            for (session, peak) in workspace.storm_notify_pending.drain(..) {
                let raw_title = workspace.session_titles.get(&session).cloned();
                drained.push((workspace.id.clone(), raw_title, peak));
            }
        }
        for (workspace_id, raw_title, peak) in drained {
            let session_name = raw_title
                .map(|raw| self.activity_session_name(&workspace_id, &raw))
                .unwrap_or_else(|| self.i18n.t("process_storm.unknown_session", &[]));
            let summary = self.i18n.t("process_storm.notification.title", &[]);
            let body = self.i18n.t(
                "process_storm.notification.body",
                &[("session", &session_name), ("count", &peak.to_string())],
            );
            platform::notify(&summary, &body);
        }
    }

    /// 메모리 압박 격상 시 최대 사용 세션을 담아 OS 알림을 1회 발화한다 (로드맵 C3).
    /// 표시 수치는 A1 이후 정확해진 세션별 phys_footprint 합. logic()에서만 호출.
    fn notify_memory_pressure(&mut self) {
        // active+warm의 세션 자원 샘플에서 rss 최댓값 세션을 찾는다(새 샘플링 없음).
        let mut top: Option<(String, runtime::SessionId, u64)> = None;
        for workspace in std::iter::once(&self.active).chain(self.warm.values()) {
            for usage in &workspace.session_resource_usage {
                if top
                    .as_ref()
                    .is_none_or(|(_, _, rss)| usage.rss_bytes > *rss)
                {
                    top = Some((workspace.id.clone(), usage.session, usage.rss_bytes));
                }
            }
        }
        let session_name = top.map(|(workspace_id, session, _rss)| {
            let raw = std::iter::once(&self.active)
                .chain(self.warm.values())
                .find(|w| w.id == workspace_id)
                .and_then(|w| w.session_titles.get(&session).cloned());
            raw.map(|raw| self.activity_session_name(&workspace_id, &raw))
                .unwrap_or_else(|| self.i18n.t("process_storm.unknown_session", &[]))
        });
        let (summary, body) = memory_pressure_notification(&self.i18n, session_name.as_deref());
        platform::notify(&summary, &body);
    }

    /// 확정된 폭주 세션 수와 최대 peak 프로세스 수 (active+warm 전체). 배너 표시용
    /// 순수 읽기 — 렌더 경로에서 호출해도 안전하다.
    fn confirmed_storm_summary(&self) -> Option<(usize, usize)> {
        let mut count = 0usize;
        let mut peak = 0usize;
        for workspace in std::iter::once(&self.active).chain(self.warm.values()) {
            for episode in workspace.storm_episodes.values() {
                if episode.confirmed {
                    count += 1;
                    peak = peak.max(episode.peak_process_count);
                }
            }
        }
        (count > 0).then_some((count, peak))
    }

    /// 확정 폭주 세션 중 하나라도 동결 상태인가 (배너 [재개]/[동결] 라벨 선택용).
    fn any_storm_session_frozen(&self) -> bool {
        std::iter::once(&self.active)
            .chain(self.warm.values())
            .any(|workspace| {
                workspace.storm_episodes.iter().any(|(session, episode)| {
                    episode.confirmed && workspace.frozen_sessions.contains(session)
                })
            })
    }

    /// 배너 버튼이 요청한 폭주 대응을 실행한다 (로드맵 B3). logic()에서만 호출 —
    /// 확정 폭주 세션(재개는 동결된 세션) 전체에 명령을 보낸다. 종료는 기존
    /// KillSession(SIGHUP→SIGTERM→SIGKILL 에스컬레이션) 재사용.
    fn dispatch_storm_action(&mut self, action: StormAction) {
        for workspace in std::iter::once(&mut self.active).chain(self.warm.values_mut()) {
            let targets: Vec<runtime::SessionId> = workspace
                .storm_episodes
                .iter()
                .filter(|(session, episode)| {
                    if !episode.confirmed {
                        return false;
                    }
                    match action {
                        StormAction::Resume => workspace.frozen_sessions.contains(session),
                        StormAction::Freeze | StormAction::Kill => true,
                    }
                })
                .map(|(session, _)| *session)
                .collect();
            for session in targets {
                let command = match action {
                    StormAction::Freeze => runtime::RuntimeCommand::FreezeSession { session },
                    StormAction::Resume => runtime::RuntimeCommand::ResumeSession { session },
                    StormAction::Kill => runtime::RuntimeCommand::KillSession { session },
                };
                if let Err(error) = workspace.runtime.send_command(command) {
                    tracing::warn!(
                        kind = "resource",
                        phase = "storm_action_send_failed",
                        action = ?action,
                        workspace = %workspace.id,
                        error = %error,
                    );
                }
            }
        }
    }

    /// 최신 feed를 읽음 기준과 대조해 Home 배지 수를 갱신한다. Home이 선택된 동안에는
    /// 현재 목록을 곧바로 읽음 처리한다. 디스크 쓰기는 기준이 실제로 달라질 때만 한다.
    fn sync_home_notice_badge(&mut self, mark_read: bool, ctx: &egui::Context) {
        let state_changed = self
            .notice_read_state
            .reconcile(&self.status_feed, mark_read);
        let unread = self.notice_read_state.unread_count(&self.status_feed);
        let count_changed = unread != self.home_notice_unread;
        self.home_notice_unread = unread;
        if state_changed
            && let Err(error) = self.notice_read_state.save(&self.notice_read_state_path)
        {
            tracing::warn!(
                path = %self.notice_read_state_path.display(),
                "공지 읽음 상태 저장 실패: {error:#}"
            );
        }
        if state_changed || count_changed {
            ctx.request_repaint();
        }
    }

    /// 홈 공지 제목 번역 펌프 — 진행 중 결과를 메모리+디스크 캐시에 합치고, 캐시에
    /// 없는 새 제공자·로케일·제목만 번역한다(rx 보유가 동시 실행 게이트).
    /// 영어 로케일이거나 claude CLI가 없으면 아무것도 하지 않는다(원문 표시).
    fn pump_notice_translations(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.notice_translate_rx {
            match rx.try_recv() {
                Ok(pairs) => {
                    self.notice_translation_cache.extend(pairs);
                    if let Err(error) = self
                        .notice_translation_cache
                        .save(&self.notice_translation_cache_path)
                    {
                        tracing::warn!(
                            path = %self.notice_translation_cache_path.display(),
                            "공지 번역 캐시 저장 실패: {error:#}"
                        );
                    }
                    self.notice_translate_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // CLI 없음/실패로 결과 없이 종료 — 원문 유지로 종결.
                    self.notice_translate_rx = None;
                }
            }
        }
        if self.notice_translate_rx.is_some() {
            return;
        }
        let Some(language) = crate::notice_translate::language_for_locale(&self.config.i18n.locale)
        else {
            return;
        };
        let locale = self.config.i18n.locale.as_str();
        let translation_cache = &self.notice_translation_cache;
        let pending: Vec<crate::notice_translate::TranslationCacheKey> = [
            ("Claude", &self.status_feed.claude),
            ("OpenAI", &self.status_feed.openai),
            ("Grok", &self.status_feed.grok),
        ]
        .into_iter()
        .filter_map(|(provider, status)| status.as_ref().map(|status| (provider, status)))
        .flat_map(|(provider, status)| {
            status.incidents.iter().map(move |incident| {
                crate::notice_translate::TranslationCacheKey::new(provider, locale, &incident.title)
            })
        })
        .filter(|key| !translation_cache.contains_key(key))
        .collect();
        if pending.is_empty() {
            return;
        }
        let bin = match &self.notice_translate_bin {
            Some(Some(bin)) => bin.clone(),
            Some(None) => return,
            None => {
                let resolved = crate::notice_translate::claude_bin();
                self.notice_translate_bin = Some(resolved.clone());
                let Some(bin) = resolved else {
                    return;
                };
                bin
            }
        };
        self.notice_translate_rx = Some(crate::notice_translate::spawn_translate(
            bin,
            pending,
            language.to_owned(),
            ctx.clone(),
        ));
    }

    /// Agents에 넘길 cwd는 active workspace/path가 바뀔 때만 파일시스템에서 검증한다.
    /// render는 이 immutable projection만 읽는다.
    fn refresh_agent_workspace_cwd(&mut self) {
        let candidate = self
            .workspaces
            .iter()
            .find(|workspace| workspace.id == self.active.id)
            .filter(|workspace| !workspace.path.trim().is_empty());
        let unchanged = match (&self.agent_workspace_cwd_key, candidate) {
            (Some((workspace_id, path)), Some(workspace)) => {
                workspace_id == &workspace.id && path == &workspace.path
            }
            (None, None) => true,
            _ => false,
        };
        if unchanged {
            return;
        }
        self.agent_workspace_cwd = candidate.and_then(|workspace| {
            std::path::Path::new(&workspace.path)
                .is_dir()
                .then(|| workspace.path.clone())
        });
        self.agent_workspace_cwd_key =
            candidate.map(|workspace| (workspace.id.clone(), workspace.path.clone()));
    }

    /// ollama 모델 감지 펌프 (PR-L3) — Agents 창이 열려 있고 OSS 프로바이더가 선택된
    /// 첫 시점에 1회 감지(local_llm 일회성 워커). 앱 실행 중 재감지는 하지 않는다 —
    /// ollama를 나중에 켠 경우는 모델명을 직접 입력하면 된다(입력 즉시 적용 원칙).
    fn pump_ollama_detect(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.ollama_detect_rx {
            match rx.try_recv() {
                Ok(snapshot) => {
                    self.ollama_models = snapshot.ollama;
                    self.ollama_detect_done = true;
                    self.ollama_detect_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.ollama_detect_done = true;
                    self.ollama_detect_rx = None;
                }
            }
        }
        // 자동 재감지 (2026-07-18 사용자): Agents 창이 닫혔다 다시 열리는 전환마다
        // done을 리셋 — 앱 실행 중 ollama를 켜거나 모델을 받은 경우를 창 재오픈이
        // 자연스럽게 반영한다. 수동 ⟳(OSS 섹션)도 같은 리셋 경로.
        let agents_open = self.agent_sessions_ui.is_open();
        if agents_open && !self.agent_sessions_was_open {
            self.ollama_detect_done = false;
        }
        self.agent_sessions_was_open = agents_open;
        if self.agent_sessions_ui.take_ollama_redetect() {
            self.ollama_detect_done = false;
        }
        if !self.ollama_detect_done
            && self.ollama_detect_rx.is_none()
            && agents_open
            && self.config.agents.codex_llm_provider.as_deref() == Some("oss")
        {
            self.ollama_detect_rx = Some(crate::local_llm::spawn_detect(
                ctx.clone(),
                crate::local_llm::DEFAULT_OLLAMA_BASE.to_owned(),
                None,
            ));
        }
    }

    fn activity_rows(&self) -> ui::activity::ActivitySnapshot {
        let now = std::time::Instant::now();
        let rows: Vec<ui::activity::ActivityWorkspaceRow> = self
            .workspaces
            .iter()
            // 사이드바 「워크스페이스 종료」는 DB 프로젝트 삭제가 아니라 이번 실행의
            // 명시적 숨김이다. 홈도 사이드바와 같은 visible set을 써야 종료한 프로젝트가
            // 유휴 카드로 되살아나지 않는다(2026-07-19 사용자).
            .filter(|ws| workspace_visible_after_close(&self.closed_workspaces, &ws.id))
            .take(ui::activity::MAX_ACTIVITY_WORKSPACES + 1)
            .map(|ws| {
                if ws.id == self.active.id {
                    // pane별 서브행 — 사이드바 3줄 행과 같은 원천(session_entries)에
                    // 세션별 자원/입력압력을 붙인다(2026-07-08).
                    let entries = self.active.workspace_ui.session_entries(
                        &self.i18n,
                        &self.agent_activity,
                        &self.agent_needs_input,
                        &self.agent_turn_done,
                        &self.agent_working,
                    );
                    let sessions = entries
                        .iter()
                        .take(ui::activity::MAX_ACTIVITY_ITEMS_PER_WORKSPACE + 1)
                        .map(|e| ui::activity::ActivitySessionRow {
                            name: Arc::from(e.title.as_str()),
                            agent_line: e.agent_line.as_deref().map(Arc::from),
                            status_line: e.status_line.as_deref().map(Arc::from),
                            resource: e.session.and_then(|s| {
                                self.active
                                    .session_resource_usage
                                    .iter()
                                    .find(|u| u.session == s)
                                    .cloned()
                            }),
                            pressure: Self::fresh_pressure(
                                e.session
                                    .and_then(|s| self.active.session_input_pressure.get(&s)),
                                now,
                            ),
                            storm: e.session.is_some_and(|s| {
                                self.active
                                    .storm_episodes
                                    .get(&s)
                                    .is_some_and(|ep| ep.confirmed)
                            }),
                        })
                        .collect::<Vec<_>>();
                    return ui::activity::ActivityWorkspaceRow {
                        name: Self::workspace_display_name(ws).into(),
                        state: ui::activity::ActivityWorkspaceState::Active,
                        session_count: entries.len(),
                        pending_events: self.active.pending_events.len(),
                        input_pressure: Self::fresh_pressure(
                            self.active.input_pressure.as_ref(),
                            now,
                        ),
                        backgrounded_for_secs: None,
                        auto_suspend_remaining_secs: None,
                        resource: self.active.resource_usage,
                        session_resources: self
                            .active
                            .session_resource_usage
                            .iter()
                            .take(ui::activity::MAX_ACTIVITY_ITEMS_PER_WORKSPACE + 1)
                            .cloned()
                            .collect::<Vec<_>>()
                            .into(),
                        sessions: sessions.into(),
                    };
                }
                if let Some(rt) = self.warm.get(&ws.id) {
                    let elapsed = rt
                        .backgrounded_at
                        .map(|at| now.saturating_duration_since(at));
                    let auto_suspend_eligible =
                        !rt.has_live_sessions() || rt.can_auto_suspend_idle_shells();
                    let remaining = elapsed.filter(|_| auto_suspend_eligible).map(|duration| {
                        Self::WARM_AUTO_SUSPEND_AFTER
                            .as_secs()
                            .saturating_sub(duration.as_secs())
                    });
                    // warm은 감지 워커가 안 돌아 제목·자원·압력만 채운다 (id 순 정렬).
                    let mut ids: Vec<_> = rt
                        .session_titles
                        .keys()
                        .copied()
                        .take(ui::activity::MAX_ACTIVITY_ITEMS_PER_WORKSPACE + 1)
                        .collect();
                    ids.sort_by_key(|s| s.0);
                    let sessions = ids
                        .iter()
                        .map(|s| ui::activity::ActivitySessionRow {
                            // 기본 제목이면 프로젝트명으로 표시 (활성 워크스페이스와 동일 규칙).
                            name: rt
                                .session_titles
                                .get(s)
                                .map(|raw| self.activity_session_name(&ws.id, raw))
                                .unwrap_or_default()
                                .into(),
                            // 대기(warm)는 에이전트가 살아있음 — 활성일 때 감지한 마지막 에이전트
                            // 줄을 유지해 보여준다(방안①). 셸이면 None.
                            agent_line: rt.workspace_ui.agent_line_for(*s).map(Into::into),
                            status_line: None,
                            resource: rt
                                .session_resource_usage
                                .iter()
                                .find(|u| u.session == *s)
                                .cloned(),
                            pressure: Self::fresh_pressure(rt.session_input_pressure.get(s), now),
                            storm: rt.storm_episodes.get(s).is_some_and(|ep| ep.confirmed),
                        })
                        .collect::<Vec<_>>();
                    return ui::activity::ActivityWorkspaceRow {
                        name: Self::workspace_display_name(ws).into(),
                        state: ui::activity::ActivityWorkspaceState::Warm,
                        session_count: rt.session_titles.len(),
                        pending_events: rt.pending_events.len(),
                        input_pressure: Self::fresh_pressure(rt.input_pressure.as_ref(), now),
                        backgrounded_for_secs: elapsed.map(|duration| duration.as_secs()),
                        auto_suspend_remaining_secs: remaining,
                        resource: rt.resource_usage,
                        session_resources: rt
                            .session_resource_usage
                            .iter()
                            .take(ui::activity::MAX_ACTIVITY_ITEMS_PER_WORKSPACE + 1)
                            .cloned()
                            .collect::<Vec<_>>()
                            .into(),
                        sessions: sessions.into(),
                    };
                }
                let sessions = self
                    .persisted_activity_panes
                    .get(&ws.id)
                    .into_iter()
                    .flatten()
                    .take(ui::activity::MAX_ACTIVITY_ITEMS_PER_WORKSPACE + 1)
                    .map(|(title, _cwd)| ui::activity::ActivitySessionRow {
                        // 유휴 워크스페이스도 프로젝트명으로 표시 (활성/warm과 동일 규칙).
                        name: self.activity_session_name(&ws.id, title).into(),
                        agent_line: None,
                        status_line: None,
                        resource: None,
                        pressure: None,
                        // 유휴(런타임 없음)는 감지 대상이 아니다.
                        storm: false,
                    })
                    .collect::<Vec<_>>();
                ui::activity::ActivityWorkspaceRow {
                    name: Self::workspace_display_name(ws).into(),
                    // DB에는 있으나 active/warm runtime이 없는 워크스페이스도 숨기지 않고
                    // 유휴 카드로 표시한다. 현재 복원 레이아웃의 pane은 위 snapshot에서
                    // 하위 세션 행으로 복구한다.
                    state: ui::activity::ActivityWorkspaceState::Idle,
                    session_count: sessions.len(),
                    pending_events: 0,
                    input_pressure: None,
                    backgrounded_for_secs: None,
                    auto_suspend_remaining_secs: None,
                    resource: None,
                    session_resources: Arc::from([]),
                    sessions: sessions.into(),
                }
            })
            .collect();
        ui::activity::ActivitySnapshot::try_new(rows).unwrap_or_else(|_| {
            tracing::warn!(
                kind = "activity",
                phase = "snapshot",
                error_code = "invalid_projection",
                "activity snapshot rejected"
            );
            ui::activity::ActivitySnapshot::empty()
        })
    }

    fn refresh_activity_snapshot_if_needed(&mut self) {
        const TTL: std::time::Duration = std::time::Duration::from_millis(500);
        if self
            .activity_rows_cache
            .as_ref()
            .is_some_and(|(created_at, _)| created_at.elapsed() <= TTL)
        {
            return;
        }
        let snapshot = self.activity_rows();
        self.activity_rows_cache = Some((std::time::Instant::now(), snapshot));
    }

    /// 한 workspace의 이벤트에서 제목을 누적(session_titles)하고 상태/exit을 알림으로
    /// 만든다. 알림은 (workspace_id, SessionId)로 식별 — 워커마다 SessionId가 리셋돼
    /// 충돌하므로. 활성/warm 워커 모두 이걸 거쳐 background workspace 알림도 뜬다.
    fn process_ws_notifications(
        notifications: &mut ui::notifications::NotificationsUi,
        workspace_id: &str,
        events: &[runtime::RuntimeEvent],
        session_titles: &mut std::collections::HashMap<runtime::SessionId, String>,
        agent_providers: &std::collections::HashMap<
            runtime::SessionId,
            crate::agent_surface::AgentProvider,
        >,
        catalog: &i18n::Catalog,
    ) {
        for event in events {
            match event {
                runtime::RuntimeEvent::MuxUpdated { snapshot } => {
                    let present: std::collections::HashSet<runtime::SessionId> = snapshot
                        .tabs
                        .iter()
                        .flat_map(|tab| &tab.panes)
                        .filter_map(|pane| pane.session_id)
                        .collect();
                    session_titles.retain(|session, _| present.contains(session));
                    for pane in snapshot.tabs.iter().flat_map(|tab| &tab.panes) {
                        if let Some(session) = pane.session_id {
                            // **raw** 제목을 저장한다 — 표시 시점에 해석한다(활동 패널/폰은
                            // 프로젝트명 규칙, 알림은 i18n 렌더). 렌더된 값을 넣으면
                            // DB의 raw 제목과 매칭되지 않아 프로젝트명 해석이 조용히
                            // 실패한다 (리뷰 P2-1).
                            session_titles.insert(session, pane.title.clone());
                        }
                    }
                }
                runtime::RuntimeEvent::SessionStatusChanged { session, status } => {
                    if let Some(raw) = session_titles.get(session).cloned() {
                        let title = ui::workspace::display_pane_title(&raw, catalog);
                        notifications.on_pty_status(
                            workspace_id,
                            *session,
                            *status,
                            &title,
                            agent_providers.get(session).copied(),
                            catalog,
                        );
                    }
                }
                // regex 없는 agent는 결과가 SessionExited로만 온다 (완료 기준: done/error)
                runtime::RuntimeEvent::SessionExited { session, exit_code } => {
                    if let Some(raw) = session_titles.get(session).cloned() {
                        let title = ui::workspace::display_pane_title(&raw, catalog);
                        notifications.on_pty_exit(
                            workspace_id,
                            *session,
                            *exit_code,
                            &title,
                            agent_providers.get(session).copied(),
                            catalog,
                        );
                    }
                    session_titles.remove(session);
                }
                _ => {}
            }
        }
    }

    fn stage_structured_agent_state(&mut self, refresh_catalog: bool) {
        if self.pending_agent_state_structured.is_empty() {
            let mutations = self.agent_sessions_ui.drain_persistence_mutations_bounded(
                crate::agent_state_worker::AGENT_STATE_STRUCTURED_BATCH_MAX,
            );
            self.pending_agent_state_structured = mutations
                .into_iter()
                .map(storage_structured_mutation)
                .collect();
        }
        if self.pending_agent_state_structured.is_empty() || !self.agent_state_scope_ready() {
            return;
        }
        if refresh_catalog {
            self.stage_agent_state_projection(
                crate::agent_state_worker::AgentStateSection::Catalog,
                AppAgentStateProjectionKind::Catalog,
            );
        }
        let Some((items, payload)) = prepare_structured_agent_state_prefix(
            &self.agent_state_scope,
            &self.pending_agent_state_structured,
        ) else {
            self.agent_sessions_ui.report_persistence_error(
                "구조화 세션 저장 요청이 현재 처리 한도를 초과했습니다".to_owned(),
            );
            return;
        };
        if self.stage_prepared_agent_state_exact(
            crate::agent_state_worker::ExactKind::StructuredBatch { items },
            payload,
        ) {
            self.pending_agent_state_structured.drain(..items);
            self.pending_agent_state_structured =
                std::mem::take(&mut self.pending_agent_state_structured)
                    .into_boxed_slice()
                    .into_vec();
        } else {
            self.agent_sessions_ui.report_persistence_error(
                "구조화 세션 저장 요청이 현재 처리 한도를 초과했습니다".to_owned(),
            );
        }
    }

    fn record_activity_events(rt: &mut WorkspaceRuntime, events: &[runtime::RuntimeEvent]) {
        for event in events {
            if let runtime::RuntimeEvent::MuxUpdated { snapshot } = event {
                let present: std::collections::HashSet<runtime::SessionId> = snapshot
                    .tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .filter_map(|pane| pane.session_id)
                    .collect();
                rt.session_dotenv_states
                    .retain(|session, _| present.contains(session));
                if let Some(state) = rt.dotenv_state {
                    for session in present {
                        rt.session_dotenv_states.entry(session).or_insert(state);
                    }
                }
            }
            if let runtime::RuntimeEvent::ResourceUsage {
                snapshot,
                session_usage,
            } = event
            {
                rt.resource_usage = Some(*snapshot);
                rt.session_resource_usage = session_usage.clone();
                update_storm_episodes(
                    &mut rt.storm_episodes,
                    &mut rt.storm_next_episode_id,
                    &mut rt.storm_notify_pending,
                    session_usage,
                );
            }
            if let runtime::RuntimeEvent::PtyInputPressure { session, pressure } = event {
                if pressure.queued_messages == 0 && pressure.queued_bytes == 0 {
                    // 해소 신호(워커가 큐 비움 관측, 2026-07-09) — 뱃지 즉시 내림.
                    rt.session_input_pressure.remove(session);
                    rt.input_pressure = rt
                        .session_input_pressure
                        .values()
                        .max_by_key(|(_, at)| *at)
                        .cloned();
                } else {
                    let now = std::time::Instant::now();
                    rt.input_pressure = Some((pressure.clone(), now));
                    rt.session_input_pressure
                        .insert(*session, (pressure.clone(), now));
                }
            }
            // 세션 종료 시 pane별 압력 신호 정리 (stale 뱃지 방지). 워크스페이스 뱃지도
            // 남은 세션들 중 최신으로 재계산 — exit한 세션의 신호가 TTL까지 남지 않게(codex).
            if let runtime::RuntimeEvent::SessionExited { session, .. } = event {
                rt.session_input_pressure.remove(session);
                rt.session_dotenv_states.remove(session);
                rt.storm_episodes.remove(session);
                rt.frozen_sessions.remove(session);
                rt.input_pressure = rt
                    .session_input_pressure
                    .values()
                    .max_by_key(|(_, at)| *at)
                    .cloned();
            }
            // 동결/재개 결과 반영 (B3) — 실제 프로세스 상태를 낙관적 상태 대신 추적.
            if let runtime::RuntimeEvent::SessionFreezeChanged { session, frozen } = event {
                if *frozen {
                    rt.frozen_sessions.insert(*session);
                } else {
                    rt.frozen_sessions.remove(session);
                }
            }
            // live 세션 추적 (suspend 보호)
            rt.live.observe(event);
            // 이관받은 agent spawn 대기 해소. Legacy AgentSpawned/SpawnFailed와 정확히
            // 한 쌍인 correlation event만 세어 두 이벤트를 중복 소비하지 않는다.
            if matches!(event, runtime::RuntimeEvent::AgentSpawnResolved { .. }) {
                rt.pending_agent_spawns = rt.pending_agent_spawns.saturating_sub(1);
            }
        }
    }

    fn apply_approval_snapshot(&mut self, snapshot: ApprovalSnapshot) {
        // SQLite/MCP inventory reads are completed by the listener worker. The frame path only
        // swaps this bounded, sanitized snapshot and derives notifications from it.
        for row in &snapshot.rows {
            if !self.approval_notified.contains(row.id())
                && let Some((workspace_id, session)) = row
                    .session_key()
                    .and_then(ui::inbox_waiting::parse_session_key)
            {
                self.notifications_ui.on_mcp_approval(
                    &workspace_id,
                    session,
                    row.tool_name(),
                    &self.i18n,
                );
            }
        }
        self.approval_notified = snapshot
            .rows
            .iter()
            .map(|row| row.id().to_owned())
            .collect();
        if self.approvals_ui.set_pending(snapshot.rows).is_err() {
            tracing::warn!(
                kind = "approval",
                phase = "snapshot_apply",
                error_code = "invalid_projection",
                "approval snapshot rejected"
            );
        }
    }

    /// 벨 팝오버(대기 인박스 + 최근 알림)의 고정 Id — 단축키(⌘⇧U) 토글이 같은 팝오버를
    /// 가리켜야 하므로 상수 Id를 쓴다.
    fn inbox_popup_id() -> egui::Id {
        egui::Id::new("inbox_popup")
    }

    /// 인박스 카드의 세션 표시 라벨 — **감지된 에이전트 이름**(Claude/Codex)을 우선하고,
    /// 에이전트가 없는 셸이면 셀 제목으로 폴백한다. "셀 2"보다 "Codex"가 무엇을
    /// 승인/응답하는지 판단하는 데 직접적이다(2026-07-17 사용자).
    /// 활성/warm 모두 워크스페이스별 agent_info를 쓴다(warm도 마지막 감지값 유지).
    fn inbox_session_label(&self, ws_id: &str, session: runtime::SessionId) -> Option<String> {
        let rt = if ws_id == self.active.id {
            &self.active
        } else {
            self.warm.get(ws_id)?
        };
        if let Some(provider) = rt.workspace_ui.agent_provider_for(session) {
            return Some(provider.label().to_owned());
        }
        let raw = rt.session_titles.get(&session)?;
        Some(ui::workspace::display_pane_title(raw, &self.i18n))
    }

    /// [N3] 전역 PTY 대기 카드 데이터 조립 — 팝오버가 열렸을 때만 호출된다(호출부 게이트,
    /// idle 비용 0). suspended/사라진 워크스페이스는 카드에서 제외한다(I1 "모르는/사라진
    /// 세션이면 명령 미생성" 원칙 — 이동 외엔 아무것도 할 수 없는 죽은 카드를 안 보인다).
    /// fleet 뷰모델 행을 active+warm 런타임을 가로질러 조립한다(읽기전용, 기능1 PR-5).
    /// idle 워크스페이스(런타임 없음)는 라이브 세션이 없어 제외하고, 에이전트 세션만
    /// 담는다(셸은 status·agent_line 모두 없음). 정렬은 fleet 모듈이 주목도 순으로 한다.
    ///
    /// build_waiting_cards와 같은 active+warm 유니온 패턴. active 전용 map
    /// (agent_activity/needs_input/turn_done)은 active에만 쓰고, warm은 workspace
    /// namespace가 있는 global_waiting에서 needs_input을 뽑아 넣는다(SessionId 재사용 안전).
    fn build_fleet_sessions(&self, text: &i18n::Catalog) -> Vec<crate::fleet::FleetSession> {
        // 브로드캐스트 직후 낙관적 "작업 중" 윈도우. 감지가 따라잡거나 지나면 실제 상태로.
        const BROADCAST_WORKING_WINDOW: std::time::Duration = std::time::Duration::from_secs(8);
        let now = std::time::Instant::now();
        let empty_activity: std::collections::HashMap<
            runtime::SessionId,
            crate::agent_transcript::AgentActivity,
        > = std::collections::HashMap::new();
        let mut out = Vec::new();
        for workspace in &self.workspaces {
            if !workspace_visible_after_close(&self.closed_workspaces, &workspace.id) {
                continue;
            }
            let active = workspace.id == self.active.id;
            let runtime = if active {
                Some(&self.active)
            } else {
                self.warm.get(&workspace.id)
            };
            let Some(runtime) = runtime else {
                continue; // idle 워크스페이스 — 라이브 세션 없음.
            };
            let needs_input: std::collections::HashSet<runtime::SessionId> = self
                .global_waiting
                .iter()
                .filter(|(ws, _, _)| ws == &workspace.id)
                .map(|(_, session, _)| *session)
                .collect();
            // hook "작업 중"(v32) — global_working에서 이 워크스페이스 몫만. warm도
            // 훅 신호로 작업 중을 정확히 보인다(transcript는 활성 전용이라 불가능했음).
            let working: std::collections::HashSet<runtime::SessionId> = self
                .global_working
                .iter()
                .filter(|(ws, _)| ws == &workspace.id)
                .map(|(_, session)| *session)
                .collect();
            // turn_done 전역화(warm turn_done 격차, 감사 발견) — global_turn_done에서
            // 이 워크스페이스 몫만. warm도 "완료(바이올렛)" 표시가 가능해진다.
            let turn_done: std::collections::HashMap<runtime::SessionId, i64> = self
                .global_turn_done
                .iter()
                .filter(|((ws, _), _)| ws == &workspace.id)
                .map(|((_, session), at)| (*session, *at))
                .collect();
            let entries = if active {
                runtime.workspace_ui.session_entries(
                    text,
                    &self.agent_activity,
                    &self.agent_needs_input,
                    &self.agent_turn_done,
                    &self.agent_working,
                )
            } else {
                runtime.workspace_ui.session_entries(
                    text,
                    &empty_activity,
                    &needs_input,
                    &turn_done,
                    &working,
                )
            };
            let workspace_name = Self::workspace_display_name(workspace);
            for entry in entries {
                // agent_line도 status도 없는 pane만 제외한다(셸도 idle heuristic으로
                // status를 받으므로 "셸 = 둘 다 None"은 성립하지 않는다 — 그런 pane은
                // 아래 from_pty_with_agent가 off로 낮춘다).
                // 주의(의도된 비대칭, 2026-07-25 판정): PTY의 status=None(회색 off 카드)은
                // 스폰 직후 첫 상태 감지 전(~10초)의 "미분류지만 살아있는" 상태라 일부러
                // 표시한다 — 구조화 쪽 Off 필터(agent_sessions.rs fleet_rows: DB placeholder
                // 홍수 방지)와 대칭으로 숨기지 말 것.
                if entry.agent_line.is_none() && entry.status.is_none() {
                    continue;
                }
                let Some(session) = entry.session else {
                    continue;
                };
                let waiting_message = self
                    .global_waiting
                    .iter()
                    .find(|(ws, s, _)| ws == &workspace.id && *s == session)
                    .and_then(|(_, _, message)| message.clone());
                // 감지 상태. 브로드캐스트 직후 아직 Idle/Off로 보이면(감지 지연/​warm 미추적)
                // 윈도우 안에서 Active로 덮는다 — Waiting/Done/Error 등 확정 상태는 그대로 둔다.
                use crate::agent_surface::AgentVisualState;
                let detected =
                    AgentVisualState::from_pty_with_agent(entry.status, entry.agent_line.is_some());
                let optimistic = matches!(detected, AgentVisualState::Idle | AgentVisualState::Off)
                    && self
                        .broadcast_working
                        .get(&(workspace.id.clone(), session))
                        .is_some_and(|sent| now.duration_since(*sent) < BROADCAST_WORKING_WINDOW);
                let state = if optimistic {
                    AgentVisualState::Active
                } else {
                    detected
                };
                out.push(crate::fleet::FleetSession {
                    workspace_id: workspace.id.clone(),
                    workspace_name: workspace_name.clone(),
                    target: crate::fleet::FleetTarget::Pty {
                        session,
                        tab: entry.tab,
                        pane: entry.pane,
                    },
                    title: entry.title,
                    state,
                    agent_line: entry.agent_line,
                    waiting_message,
                    active_workspace: active,
                });
            }
        }
        // 구조화(App Server) 에이전트 — 워크스페이스 런타임과 독립이라 agent_sessions_ui에서
        // 직접 나열한다(관찰 + 열기 전용, 브로드캐스트 대상 아님). 상태는 from_structured.
        for row in self.agent_sessions_ui.fleet_rows() {
            // 닫은 워크스페이스의 구조화 세션은 fleet에서도 숨긴다 — PTY 카드와 일관되게
            // (병렬 리뷰 Low). workspace_id가 없으면 필터할 수 없어 그대로 표시한다.
            if let Some(ws) = &row.workspace_id
                && !workspace_visible_after_close(&self.closed_workspaces, ws)
            {
                continue;
            }
            // workspace_id 없으면 빈 문자열(표시용 fallback). 구조화 카드의 라우팅은
            // OpenStructured가 session_id로만 하므로 이 값이 실제 id가 아니어도 안전하다.
            let workspace_id = row.workspace_id.clone().unwrap_or_default();
            let workspace_name = self
                .workspaces
                .iter()
                .find(|w| Some(&w.id) == row.workspace_id.as_ref())
                .map(Self::workspace_display_name)
                .unwrap_or_else(|| "—".to_owned());
            // badge는 AgentTransport 상수 재사용(드리프트 방지 — 병렬 리뷰 Low).
            let badge = crate::agent_surface::AgentTransport::AppServer.badge();
            let agent_line = Some(match &row.model {
                Some(model) => format!("[{badge}] Codex · {model}"),
                None => format!("[{badge}] Codex"),
            });
            out.push(crate::fleet::FleetSession {
                active_workspace: row.workspace_id.as_deref() == Some(self.active.id.as_str()),
                workspace_id,
                workspace_name,
                target: crate::fleet::FleetTarget::Structured {
                    session_id: row.session_id,
                },
                title: row.title,
                state: row.state,
                agent_line,
                waiting_message: None,
            });
        }
        crate::fleet::sort_sessions(&mut out);
        out
    }

    fn build_waiting_cards(&mut self) -> Vec<ui::inbox_waiting::WaitingCard> {
        let global_waiting = self.global_waiting.clone();
        let mut cards = Vec::with_capacity(global_waiting.len());
        for (ws_id, session, headline) in global_waiting {
            let Some(ws_row) = self.workspaces.iter().find(|w| w.id == ws_id) else {
                continue;
            };
            let workspace_name = Self::workspace_display_name(ws_row);
            if let Some(card) = self.build_waiting_card(&ws_id, session, workspace_name, headline) {
                cards.push(card);
            }
        }
        cards
    }

    /// [N3] 대기 카드 하나 — 활성/warm 워크스페이스를 같은 방식으로 만든다.
    ///
    /// 미리보기는 **양쪽 다 로그 tail**에서 읽는다. 활성 워크스페이스만 사이드바용
    /// `session_entries` summary를 쓰던 때는 마지막 한 줄(claude 상태줄이나 셸 프롬프트)만
    /// 나와서, 정작 지금 보고 있는 워크스페이스의 카드가 "무엇을 묻는지"를 못 보여줬다
    /// (2026-07-17 실측 — 카드에 `▶▶ auto mode on …`만 뜨고 질문은 안 보임). tail은
    /// 백그라운드 조회 + TTL 캐시라 활성이라고 더 싸지도 않다.
    ///
    /// §14.1: warm은 렌더 경로가 아니라 mux 스냅샷이 최신이 아닐 수 있다 — UUID를 못
    /// 찾으면 미리보기만 비고 카드(버튼)는 정상 동작한다(폴백).
    fn build_waiting_card(
        &mut self,
        ws_id: &str,
        session: runtime::SessionId,
        workspace_name: String,
        headline: Option<String>,
    ) -> Option<ui::inbox_waiting::WaitingCard> {
        let session_title = self.inbox_session_label(ws_id, session)?;
        let active = ws_id == self.active.id;
        let rt = if active {
            &self.active
        } else {
            self.warm.get(ws_id)?
        };
        let uuid = rt
            .workspace_ui
            .mux()
            .and_then(|mux| ui::inbox_waiting::find_persistent_session_id(mux, session));
        let preview_source = uuid.and_then(|uuid| {
            let logs_root = self.logs_base.join(ws_id);
            ::storage::SessionLogWriter::session_dir_key(&logs_root, &uuid)
                .ok()
                .map(|directory| directory.join("redacted.plain.txt"))
                .and_then(|path| ui::inbox_waiting::LogPreviewSource::try_new(path).ok())
                .map(Arc::new)
        });
        Some(ui::inbox_waiting::WaitingCard {
            workspace_id: ws_id.to_owned(),
            session,
            workspace_name,
            session_title,
            headline,
            preview_source,
        })
    }

    /// [N3] 응답 주입 — 워크스페이스 전환 없이 활성/warm 워크스페이스에 직접 WriteInput을
    /// 보낸다. 안전장치: 주입 직전 그 세션이 여전히 대기 중인지 재확인한다(stale 카드가
    /// 엉뚱한 입력을 넣지 않게 — I1 "모르는/사라진 세션이면 명령 미생성" 원칙).
    fn inject_waiting_answer(
        &mut self,
        workspace_id: &str,
        session: runtime::SessionId,
        reply: &str,
    ) {
        let still_waiting = self
            .global_waiting
            .iter()
            .any(|(ws, waiting_session, _)| ws == workspace_id && *waiting_session == session);
        if !still_waiting {
            // 대기가 이미 해소됨 — 조용히 무시한다. 다음 refresh_needs_input이 카드
            // 목록을 갱신한다.
            return;
        }
        let bytes = waiting_answer_bytes(reply);
        if workspace_id == self.active.id {
            // 선택 중 freeze 해제 — 이 경로도 WorkspaceUi::send를 우회한다
            // (resume 주입 경로와 동일 관례, app.rs의 다른 WriteInput 직접 전송 참고).
            self.active.workspace_ui.clear_selection(session);
            let _ = self
                .active
                .runtime
                .send_command(runtime::RuntimeCommand::WriteInput { session, bytes });
        } else if let Some(rt) = self.warm.get_mut(workspace_id) {
            rt.workspace_ui.clear_selection(session);
            let _ = rt
                .runtime
                .send_command(runtime::RuntimeCommand::WriteInput { session, bytes });
        }
        // suspended/사라진 워크스페이스면 runtime이 없어 자연히 no-op(I1) — build_waiting_cards가
        // 애초에 그런 세션의 카드를 만들지 않으므로 정상 경로에서는 도달하지 않는다.
        //
        // 주입 후 낙관적으로 카드를 즉시 내린다 — hook의 waiting=0 기록 + 다음 detect
        // tick(~2.5s)까지 카드가 남아 있으면 재클릭이 DB 재확인(아직 waiting=1)을 통과해
        // 이중 주입된다(리뷰 P2). 진실은 다음 refresh_needs_input이 복원한다.
        self.global_waiting
            .retain(|(ws, s, _)| !(ws == workspace_id && *s == session));
        self.egui_ctx.request_repaint();
    }

    /// 하단 도크 컴포저 렌더 (2026-07-17) — CentralPanel보다 먼저 호출해야 터미널
    /// 영역이 자동으로 줄어든다(통합 도크 — 팝업/오버레이 금지 사양). 설정 OFF면
    /// 호출측이 아예 부르지 않는다(Panel 미생성 — 리소스 0).
    /// MCP 목록은 Connector의 bounded immutable overview/단일 tool page만 읽는다.
    /// 프롬프트 라이브러리를 파일에 저장한다(저장/편집/삭제 후). 실패는 경고만 남기고
    /// 앱을 막지 않는다 — 사용자 데이터라 다음 저장에서 복구된다.
    fn persist_prompt_library(&self) {
        if let Err(error) = self.prompt_library.save(&self.prompt_library_path) {
            tracing::warn!(
                path = %self.prompt_library_path.display(),
                "프롬프트 라이브러리 저장 실패: {error:#}"
            );
        }
    }

    fn render_composer_dock(&mut self, ui: &mut egui::Ui, text: &i18n::Catalog) {
        let composer_session = self.active.workspace_ui.focused_session();
        let composer_agent = composer_session
            .and_then(|session| self.active.workspace_ui.agent_provider_for(session));
        let composer_root = self.agent_workspace_cwd.as_deref().map(PathBuf::from);
        let composer_workspace_id = self.active.id.clone();
        // 프롬프트 라이브러리(기능2) 열기 요청 — 아래 도크 클로저에서 self.composer를
        // 이미 빌린 상태라 로컬 플래그로 모았다가 블록 뒤에서 연다(빌림 충돌 회피).
        // enabled 여부도 미리 복사한다(클로저 안에서 self.config를 못 읽음).
        let mut open_palette = false;
        let prompt_library_enabled = self.config.ui.prompt_library_enabled;
        let composer_action = {
            // 도크 배경은 패널색(테마 파생) — 카드가 살짝 떠 보이도록 여백을 준다.
            let dock_frame =
                egui::Frame::side_top_panel(&ui.ctx().global_style()).inner_margin(egui::Margin {
                    left: 10,
                    right: 10,
                    top: 8,
                    bottom: 10,
                });
            let composer = &mut self.composer;
            let composer_ctx = ui::composer::ComposerContext {
                workspace_id: &composer_workspace_id,
                send_key: self.config.ui.composer_send_key,
                can_send: composer_session.is_some(),
                agent: composer_agent,
                workspace_root: composer_root.as_deref(),
                // 접힘 단축키 = FocusComposer의 유효 바인딩 + dispatcher와 같은 충돌
                // 억제 — 리바인드/비활성/충돌을 열기와 동일 규칙으로 반영한다(codex P2).
                collapse_shortcut: composer_collapse_shortcut(&self.config.shortcuts),
                connector_snapshot: self.connector_snapshot_reader.snapshot(),
            };
            egui::Panel::bottom("composer_dock")
                .resizable(false)
                .show_separator_line(false)
                .frame(dock_frame)
                .show(ui, |ui| {
                    // 프롬프트 라이브러리 열기 — Ctrl+K가 컴포저 접기로 리바인드되어
                    // 단축키 대신 버튼으로 연다. 툴바 스타일(small_button "/model")과 맞춘다.
                    // 설정에서 끄면(PR-7) 버튼을 숨긴다.
                    if prompt_library_enabled {
                        ui.horizontal(|ui| {
                            if ui
                                .small_button("/prompt")
                                .on_hover_text(text.t("composer.prompt_hint", &[]))
                                .clicked()
                            {
                                open_palette = true;
                            }
                        });
                    }
                    composer.render(ui, text, &composer_ctx)
                })
                .inner
        };
        // 설정에서 꺼져 있으면(PR-7) 팔레트를 그리지 않는다. 켜져 있던 중 끄면 닫는다.
        if prompt_library_enabled {
            // 이미 열려 있으면 재초기화하지 않는다 — 파라미터 입력 중 재클릭으로 작업이
            // 날아가지 않게(PR-2 리뷰 Low).
            if open_palette && !self.prompt_palette.is_open() {
                self.prompt_palette.open();
            }
            // 팔레트는 떠 있는 Window라 도크와 독립적으로 그린다. intent를 받으면 App이
            // 실제 부수효과를 수행한다(leaf+intent+host I/O 경계): 삽입=컴포저 버퍼 쓰기,
            // 저장/삭제=라이브러리 변경 + 파일 영속화. composer_draft는 "현재 내용 저장" 프리필용.
            let composer_draft = self
                .composer
                .current_text(&composer_workspace_id)
                .to_owned();
            match self
                .prompt_palette
                .render(ui.ctx(), &self.prompt_library, &composer_draft, text)
            {
                Some(ui::prompt_palette::PromptPaletteAction::Insert(prompt_text)) => {
                    self.composer
                        .insert_text(&composer_workspace_id, &prompt_text);
                }
                Some(ui::prompt_palette::PromptPaletteAction::Upsert(prompt)) => {
                    self.prompt_library.upsert(prompt);
                    self.persist_prompt_library();
                }
                Some(ui::prompt_palette::PromptPaletteAction::Delete(id)) => {
                    self.prompt_library.delete(&id);
                    self.persist_prompt_library();
                }
                None => {}
            }
        } else if self.prompt_palette.is_open() {
            self.prompt_palette.close();
        }
        match composer_action {
            Some(ui::composer::ComposerAction::Send(submission)) => {
                let (prompt, history) = submission.into_parts();
                if self.stage_workspace_controller_action(
                    WorkspaceControllerAction::ComposerPrompt(prompt),
                ) {
                    self.pending_composer_history = Some(history);
                }
            }
            Some(ui::composer::ComposerAction::RequestMcpToolPage { server_id, offset }) => {
                if self.pending_connector_dispatch.is_none() {
                    self.pending_connector_dispatch = Some((
                        connector_contract::ConnectorIntent::RequestToolPage { server_id, offset },
                        None,
                    ));
                    ui.ctx().request_repaint();
                } else {
                    tracing::warn!(
                        kind = "connector",
                        phase = "dispatch",
                        error_code = "backpressure",
                        "Composer Connector tool-page request rejected"
                    );
                }
            }
            Some(ui::composer::ComposerAction::RequestContextFile(request)) => {
                if self.pending_app_host_action.is_none() {
                    self.pending_app_host_action =
                        Some(AppHostIoAction::ComposerContextFile(request));
                    ui.ctx().request_repaint();
                } else {
                    let active_workspace = self.active.id.clone();
                    let _ = self.composer.complete_context_file(
                        &self.egui_ctx,
                        request,
                        None,
                        &active_workspace,
                    );
                }
            }
            Some(ui::composer::ComposerAction::RequestClipboardAttachment(request)) => {
                if self.pending_app_host_action.is_none() {
                    self.pending_app_host_action =
                        Some(AppHostIoAction::ComposerClipboard(request));
                    ui.ctx().request_repaint();
                } else {
                    let active_workspace = self.active.id.clone();
                    let _ = self.composer.complete_clipboard_attachment(
                        &self.egui_ctx,
                        request,
                        None,
                        &active_workspace,
                    );
                }
            }
            None => {}
        }
    }

    /// 컴포저 프롬프트를 활성 워크스페이스의 포커스 세션에 주입한다 (2026-07-17).
    /// bracketed-paste TUI에는 짧은 한 줄도 명시적 paste 본문과 별도 submit CR로
    /// 보낸다. Codex 감지 워커가 아직 결과를 내기 전에는 일반 키 입력 경로를 타던
    /// 타이밍 의존을 없애고, PTY writer의 FIFO 순서로 두 입력을 전달한다.
    /// 주입 전 clear_selection은 WriteInput 직접 전송 관례(inject_waiting_answer와 동일).
    fn send_composer_prompt(&mut self, prompt: &str) {
        let Some(session) = self.active.workspace_ui.focused_session() else {
            tracing::info!("컴포저 전송: 포커스된 터미널 세션 없음 — 무시");
            return;
        };
        let bracketed = self.active.workspace_ui.session_bracketed_paste(session);
        let provider = self.active.workspace_ui.agent_provider_for(session);
        self.active.workspace_ui.clear_selection(session);
        let Some(plan) = ui::composer::plan_composer_input(prompt, true, bracketed, provider)
        else {
            return;
        };
        let writes = match plan {
            ui::composer::ComposerInputPlan::Single(bytes) => vec![bytes],
            ui::composer::ComposerInputPlan::BracketedPaste { body, submit } => vec![body, submit],
        };
        for bytes in writes {
            if let Err(e) = self
                .active
                .runtime
                .send_command(runtime::RuntimeCommand::WriteInput { session, bytes })
            {
                tracing::warn!("컴포저 전송 실패: {e:#}");
                return;
            }
        }
    }

    /// 브로드캐스트(기능2×1): 특정 (workspace, session)에 프롬프트를 컴포저 Send와 동일한
    /// 경로로 주입한다. active/warm 런타임을 라우팅하고, suspended/사라진 워크스페이스는
    /// runtime이 없어 자연히 no-op(inject_waiting_answer와 같은 관례).
    fn broadcast_prompt_to(
        &mut self,
        workspace_id: &str,
        session: runtime::SessionId,
        prompt: &str,
    ) {
        if workspace_id == self.active.id {
            Self::write_prompt_to_session(
                &mut self.active.workspace_ui,
                &self.active.runtime,
                session,
                prompt,
            );
        } else if let Some(rt) = self.warm.get_mut(workspace_id) {
            Self::write_prompt_to_session(&mut rt.workspace_ui, &rt.runtime, session, prompt);
        }
    }

    /// 한 세션에 프롬프트를 주입한다 — send_composer_prompt와 동일한 bracketed-paste + CR
    /// 계획을 쓰되 대상 세션을 인자로 받아 active/warm 어디든 보낸다.
    fn write_prompt_to_session(
        workspace_ui: &mut ui::workspace::WorkspaceUi,
        runtime: &InProcessRuntimeClient,
        session: runtime::SessionId,
        prompt: &str,
    ) {
        let bracketed = workspace_ui.session_bracketed_paste(session);
        let provider = workspace_ui.agent_provider_for(session);
        workspace_ui.clear_selection(session);
        let Some(plan) = ui::composer::plan_composer_input(prompt, true, bracketed, provider)
        else {
            return;
        };
        let writes = match plan {
            ui::composer::ComposerInputPlan::Single(bytes) => vec![bytes],
            ui::composer::ComposerInputPlan::BracketedPaste { body, submit } => vec![body, submit],
        };
        for bytes in writes {
            if let Err(e) =
                runtime.send_command(runtime::RuntimeCommand::WriteInput { session, bytes })
            {
                tracing::warn!("브로드캐스트 전송 실패: {e:#}");
                return;
            }
        }
    }

    /// 인박스 표면(벨 팝오버·작업함 페이지)이 공유하는 워크스페이스 표시명 맵 —
    /// 승인 카드·알림 행의 "어느 워크스페이스인가" 컨텍스트.
    fn inbox_workspace_names(&self) -> std::collections::HashMap<String, String> {
        self.workspaces
            .iter()
            .map(|w| (w.id.clone(), Self::workspace_display_name(w)))
            .collect()
    }

    /// 승인 카드의 세션 라벨 맵 — DB 조인은 프로덕션에서 늘 None이라(pane_id는 런타임
    /// 세션 키, mux_panes.id는 UUID) 메모리에서 해석해 넘긴다. 활성 + warm 전부:
    /// 인박스의 존재 이유가 "다른 워크스페이스 것도 여기서 판단"이다.
    /// 라벨 규칙은 PTY 카드와 같다(inbox_session_label — 에이전트명 우선, 셸이면 셀 제목).
    fn inbox_approval_session_titles(
        &self,
    ) -> std::collections::HashMap<(String, runtime::SessionId), String> {
        let keys: Vec<(String, runtime::SessionId)> = self
            .active
            .session_titles
            .keys()
            .map(|session| (self.active.id.clone(), *session))
            .chain(self.warm.iter().flat_map(|(ws, rt)| {
                rt.session_titles
                    .keys()
                    .map(move |session| (ws.clone(), *session))
            }))
            .collect();
        keys.into_iter()
            .filter_map(|(ws, session)| {
                let label = self.inbox_session_label(&ws, session)?;
                Some(((ws, session), label))
            })
            .collect()
    }

    /// [N3] PTY 대기 카드 액션 적용 — 응답은 바로 주입하고, [이동→]은 대상을 반환한다
    /// (호출측이 기존 알림 네비게이션 파이프라인에 태운다). 팝오버·작업함 페이지 공용.
    fn apply_inbox_waiting_action(
        &mut self,
        action: Option<ui::inbox_waiting::WaitingAction>,
    ) -> Option<ui::notifications::AgentNotificationTarget> {
        match action {
            Some(ui::inbox_waiting::WaitingAction::Answer {
                workspace_id,
                session,
                reply,
            }) => {
                self.inject_waiting_answer(&workspace_id, session, &reply);
                None
            }
            Some(ui::inbox_waiting::WaitingAction::Goto(target)) => Some(target),
            None => None,
        }
    }

    /// 승인/거부 결정은 listener의 bounded command queue로만 전달한다. DB write와 후속
    /// snapshot materialization은 같은 worker connection에서 순서대로 실행된다.
    fn apply_inbox_approval_decision(&mut self, decision: Option<ui::approvals::ApprovalDecision>) {
        let Some(decision) = decision else {
            return;
        };
        if self
            .approval_wake_hub
            .enqueue(ApprovalWorkerCommand::Resolve {
                id: decision.id,
                allowed: decision.allowed,
                remember: decision.remember,
                resolved_at: deppy_core::time::unix_secs_i64(),
            })
            .is_err()
        {
            tracing::warn!(
                kind = "approval",
                phase = "resolve_enqueue",
                error_code = "unavailable",
                "승인 해소 요청 전달 실패"
            );
        }
    }

    /// 벨 팝오버 본문 (v3.9 N1). 설정 창과 독립 — 밖을 클릭하면 닫힌다.
    /// 반환: 최근 알림 / 승인 카드 / PTY 대기 카드의 [이동→]에서 클릭한 대상
    /// (있으면 호출측이 기존 네비게이션 경로로 처리).
    ///
    /// 「대기 중」 섹션 = MCP 승인 카드(N2) + PTY 입력 대기 카드(N3) — 둘 다 그 워크스페이스로
    /// 이동하지 않고 처리하는 것이 목적이다(v3.9). 그 아래가 「최근 알림」(지나간 기록).
    fn inbox_popup(
        &mut self,
        bell: &egui::Response,
        text: &i18n::Catalog,
    ) -> Option<ui::notifications::AgentNotificationTarget> {
        const RECENT_IN_POPOVER: usize = 5;
        let mut clicked = None;
        let mut open_full = false;
        // 벨 클릭의 Toggle은 아래 Popup::show() **내부**에서 적용된다 — is_id_open만 보면
        // 클릭으로 여는 프레임에 카드가 1프레임 늦는다(리뷰 P3). XOR로 이번 프레임의
        // 실제 표시 여부를 미리 계산한다.
        let popup_open =
            egui::Popup::is_id_open(&self.egui_ctx, Self::inbox_popup_id()) ^ bell.clicked();
        // 승인 카드마다 워크스페이스명이 필요하다(핵심 요구 — 가지 않고 판단). 표시
        // 이름은 .show() 진입 전에 소유 데이터로 미리 계산해 둔다 — closure 안에서
        // self.workspaces를 빌리면 아래 self.notifications_ui(&mut) 차용과 얽힌다.
        // 팝오버가 닫혀 있으면 만들지 않는다(idle 비용 0 원칙, 리뷰 P3).
        let approval_workspace_names: std::collections::HashMap<String, String> = if popup_open {
            self.inbox_workspace_names()
        } else {
            Default::default()
        };
        let approval_session_titles: std::collections::HashMap<
            (String, runtime::SessionId),
            String,
        > = if popup_open {
            self.inbox_approval_session_titles()
        } else {
            Default::default()
        };
        let mut approval_decision = None;
        // PTY 입력 대기 카드 — 팝오버가 열려 있을 때만 조립한다(idle 비용 0:
        // 닫혀 있으면 tail 조회·캐시 갱신을 전혀 하지 않는다).
        let waiting_cards = popup_open.then(|| self.build_waiting_cards());
        let mut waiting_action = None;
        egui::Popup::from_response(bell)
            .id(Self::inbox_popup_id())
            .open_memory(bell.clicked().then_some(egui::SetOpenCommand::Toggle))
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .align(egui::RectAlign::BOTTOM_START)
            .show(|ui| {
                // 카드는 "가지 않고 판단"이 목적이라 폭이 정보량을 좌우한다 — 좁으면
                // 경로·명령·미리보기가 죄다 잘린다(2026-07-17 사용자). 화면 폭의 절반까지
                // 허용하되 상한을 둔다(작은 화면에서 팝오버가 창을 덮지 않게).
                let content = ui.ctx().content_rect();
                ui.set_min_width(420.0_f32.min(content.width() - 40.0));
                ui.set_max_width(560.0_f32.min(content.width() * 0.5).max(420.0));
                // 미리보기가 12줄까지 늘어 카드가 길어졌다 — 대기가 여러 건이면 팝오버가
                // 창을 덮으므로 스크롤에 담는다(높이는 창의 70%까지).
                egui::ScrollArea::vertical()
                    .max_height(content.height() * 0.7)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        // ── 대기 중 섹션 (처리하면 사라지는 액션 큐) ──
                        // MCP 승인 카드 — listener worker가 만든 immutable snapshot만 읽는다.
                        // render 경로에는 DB 조회가 없다.
                        let approval_action = ui::inbox_approvals::render(
                            ui,
                            text,
                            self.approvals_ui.pending(),
                            &approval_workspace_names,
                            &approval_session_titles,
                            ui::inbox_approvals::POPUP_MAX_CARDS,
                        );
                        approval_decision = approval_action.decision;
                        clicked = approval_action.goto;
                        // PTY 입력 대기 카드 — 카드가 없으면 아무것도 그리지 않는다.
                        if let Some(cards) = &waiting_cards {
                            waiting_action = self.inbox_waiting_ui.render(ui, text, cards);
                            if !cards.is_empty() {
                                ui.add_space(6.0);
                            }
                        }
                        // ── 최근 알림 섹션 (지나간 기록) ──
                        if let Some(target) =
                            self.notifications_ui
                                .recent_section(ui, text, RECENT_IN_POPOVER)
                        {
                            clicked = Some(target);
                        }
                        ui.add_space(6.0);
                        // 전체 기록·비우기는 설정→알림이 계속 담당한다 (팝오버는 빠른 확인만).
                        if ui
                            .button(text.t("inbox.view_all", &[]))
                            .on_hover_text(text.t("inbox.view_all.hint", &[]))
                            .clicked()
                        {
                            open_full = true;
                        }
                    });
            });
        if open_full {
            self.settings_category = ui::settings::Category::Notifications;
            self.settings_open = true;
            egui::Popup::close_id(&self.egui_ctx, Self::inbox_popup_id());
        }
        // [N3] 카드 액션 처리 — Popup::show 클로저 밖에서 한다(클로저 내부 빌림 단순화).
        // 응답 주입·승인 되쓰기는 작업함 페이지와 공용 헬퍼로 처리한다(동작 동일 보장).
        let goto = self.apply_inbox_waiting_action(waiting_action);
        // 팝오버를 연 동안은 읽음 처리 — 설정→알림 카테고리와 같은 규약.
        if popup_open && self.notifications_ui.mark_all_read() {
            self.egui_ctx.request_repaint();
        }
        self.apply_inbox_approval_decision(approval_decision);
        let target = clicked.or(goto);
        // [이동→]/최근 알림 클릭으로 다른 화면으로 가면 팝오버를 닫는다 — 열린 채 두면
        // 전환된 화면 위에 계속 떠 있다(리뷰 P3).
        if target.is_some() {
            egui::Popup::close_id(&self.egui_ctx, Self::inbox_popup_id());
        }
        target
    }

    /// 「작업함」 전체 페이지 (2026-07-18 사용자 확정 디자인) — 사이드바 하단 nav로
    /// 진입하는 중앙 뷰. 벨 팝오버(빠른 훑어보기용 — 유지)와 **같은 카드 컴포넌트**를
    /// 재사용한다: 대기 중 = inbox_approvals::render + InboxWaitingUi::render(승인/거부·
    /// 자유 입력·미리보기 동작 동일), 액션 처리도 공용 헬퍼(apply_inbox_*)를 거친다.
    /// 최근 알림은 시간·워크스페이스를 포함한 전체 목록(history_section).
    /// 반환: 세션 점프 대상 — 호출측이 기존 알림 네비게이션 경로로 처리한다.
    fn render_inbox_page(
        &mut self,
        ui: &mut egui::Ui,
        text: &i18n::Catalog,
    ) -> Option<ui::notifications::AgentNotificationTarget> {
        let workspace_names = self.inbox_workspace_names();
        let session_titles = self.inbox_approval_session_titles();
        let waiting_cards = self.build_waiting_cards();
        let mut approval_decision = None;
        let mut waiting_action = None;
        let mut clicked = None;
        egui::ScrollArea::vertical()
            .id_salt("inbox_page")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Frame::NONE
                    .inner_margin(egui::Margin::same(22))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        // 홈 대시보드 섹션과 같은 프레임 톤.
                        let panel = egui::Frame::NONE
                            .fill(ui.visuals().panel_fill)
                            .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
                            .corner_radius(egui::CornerRadius::same(2))
                            .inner_margin(egui::Margin::same(16));
                        // ── 대기 중 (처리하면 사라지는 액션 큐) ──
                        panel.show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(
                                egui::RichText::new(text.t("inbox.page.waiting", &[]))
                                    .strong()
                                    .size(16.0),
                            );
                            ui.add_space(4.0);
                            if self.approvals_ui.pending().is_empty() && waiting_cards.is_empty() {
                                ui.weak(text.t("inbox.page.no_waiting", &[]));
                            } else {
                                // 전체 페이지는 팝오버 카드 상한(5) 없이 전부 그린다 —
                                // 상한이 있으면 6번째 이후 요청을 조작할 수 없다(codex P2).
                                let approval_action = ui::inbox_approvals::render(
                                    ui,
                                    text,
                                    self.approvals_ui.pending(),
                                    &workspace_names,
                                    &session_titles,
                                    usize::MAX,
                                );
                                approval_decision = approval_action.decision;
                                clicked = approval_action.goto;
                                waiting_action =
                                    self.inbox_waiting_ui.render(ui, text, &waiting_cards);
                            }
                            // 카드가 비어도 render의 정리 경로는 돌아야 한다 — SessionId가
                            // 워커마다 재배정되므로 마지막 카드 해소 시 드래프트를 안 지우면
                            // 다른 논리 세션이 과거 입력을 물려받는다(codex P2, 팝오버와
                            // 같은 규칙). 위 else에서 이미 그렸으면 중복 호출하지 않는다.
                            if self.approvals_ui.pending().is_empty() && waiting_cards.is_empty() {
                                waiting_action =
                                    self.inbox_waiting_ui.render(ui, text, &waiting_cards);
                            }
                        });
                        ui.add_space(14.0);
                        // ── 최근 알림 (지나간 기록 — 시간·워크스페이스 포함 전체) ──
                        panel.show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(
                                egui::RichText::new(text.t("inbox.recent", &[]))
                                    .strong()
                                    .size(16.0),
                            );
                            ui.add_space(4.0);
                            if let Some(target) =
                                self.notifications_ui
                                    .history_section(ui, text, &workspace_names)
                            {
                                clicked = Some(target);
                            }
                        });
                    });
            });
        self.apply_inbox_approval_decision(approval_decision);
        let goto = self.apply_inbox_waiting_action(waiting_action);
        // 페이지가 보이는 동안 읽음 처리 — 팝오버·설정→알림과 같은 규약.
        if self.notifications_ui.mark_all_read() {
            self.egui_ctx.request_repaint();
        }
        clicked.or(goto)
    }

    fn prune_resolved_approvals(&self) {
        let now = deppy_core::time::unix_secs_i64();
        match self
            .db
            .prune_resolved_approvals(now.saturating_sub(Self::RESOLVED_APPROVAL_RETENTION_SECS))
        {
            Ok(n) if n > 0 => tracing::info!("resolved MCP approval {n}건 정리"),
            Ok(_) => {}
            Err(e) => tracing::warn!("resolved MCP approval 정리 실패: {e:#}"),
        }
    }

    /// Applies visual configuration outside the render pass. Font installation can read font
    /// files, so keeping the change detector in `logic` is part of the leaf render-I/O boundary.
    fn apply_pending_visual_settings(&mut self, ctx: &egui::Context) {
        let theme_dark = ctx.global_style().visuals.dark_mode;
        if theme_dark != self.last_theme_dark {
            self.last_theme_dark = theme_dark;
            self.active.workspace_ui.clear_render_caches();
            for runtime in self.warm.values_mut() {
                runtime.workspace_ui.clear_render_caches();
            }
            ctx.request_repaint();
        }

        if font_settings_changed(
            &self.config,
            &self.last_ui_font,
            &self.last_mono_font,
            &self.last_mono_weight,
        ) {
            self.last_ui_font = self.config.ui.ui_font.clone();
            self.last_mono_font = self.config.terminal.mono_font.clone();
            self.last_mono_weight = self.config.terminal.mono_weight.clone();
            crate::fonts::install_cjk_fallback(
                ctx,
                self.config.ui.ui_font.as_deref(),
                &self.config.terminal.mono_font,
                &self.config.terminal.mono_weight,
            );
            self.active.workspace_ui.clear_render_caches();
            for runtime in self.warm.values_mut() {
                runtime.workspace_ui.clear_render_caches();
            }
            ctx.request_repaint();
        }

        if (self.config.ui.ui_scale - self.last_ui_scale).abs() > f32::EPSILON {
            self.last_ui_scale = self.config.ui.ui_scale;
            ctx.set_zoom_factor(self.config.ui.ui_scale);
            self.active.workspace_ui.clear_render_caches();
            for runtime in self.warm.values_mut() {
                runtime.workspace_ui.clear_render_caches();
            }
            ctx.request_repaint();
        }
    }

    /// eframe renderer feature와 무관한 공통 종료 경로. `App::on_exit` 시그니처만
    /// `glow` feature에 따라 달라지므로 실제 정리는 여기 한 번만 유지한다.
    fn shutdown_on_exit(&mut self) {
        // B1: 링버퍼에 모은 frame 이벤트 flush + 요약/gpu 이벤트. shutdown보다 **먼저** —
        // egui 텍스처 상태가 살아 있어야 gpu 이벤트가 실제 값을 낸다.
        if let Some(bench) = self.bench.as_mut() {
            bench.finish();
        }
        self.status_feed_rx.shutdown();
        if let Some(task) = self.app_host_io.take() {
            task.cancel
                .store(true, std::sync::atomic::Ordering::Release);
            let _ = task.handle.join();
        }
        // 마지막 App Server 이벤트가 만든 thread metadata를 종료 전에 한 번 더 반영한다.
        self.agent_sessions_ui.poll();
        // App Server는 PTY runtime과 독립된 child process라 여기서 명시적으로 종료·reap한다.
        self.agent_sessions_ui.shutdown();

        // First settle every pre-existing exact under its original scope and release all
        // projection payloads. The blocking seam keeps admission open for bounded structured
        // waves and never retries an in-flight unknown delivery.
        let initial_report = self.agent_state_worker.drain_exact_wave_for_shutdown();
        let mut shutdown_agent_state_failed = initial_report.overflowed()
            || initial_report
                .receipts()
                .iter()
                .any(|receipt| receipt.result().is_err());

        // No worker payload remains, so installing the final active/catalog scope cannot mix
        // epochs. This also resolves a metadata-only transition that was pending at shutdown.
        let final_epoch = self
            .pending_agent_state_scope
            .as_ref()
            .map_or(self.agent_state_scope.epoch, |scope| scope.epoch)
            .wrapping_add(1)
            .max(1);
        let final_scope = AppAgentStateScope::new(
            final_epoch,
            self.active.id.clone(),
            self.workspaces
                .iter()
                .map(|workspace| workspace.id.clone())
                .collect(),
        )
        .map(Arc::new);
        if let Some(scope) = final_scope.as_ref() {
            self.agent_state_scope = Arc::clone(scope);
            self.pending_agent_state_scope = None;
            self.agent_state_next_revision = 0;
        } else {
            shutdown_agent_state_failed = true;
        }

        // The UI backlog has at most one coalesced mutation per bounded persisted session. Drain
        // it in finite <=16-item exact batches, settling each <=8-item worker FIFO wave before
        // admitting more. No timer, repaint, or unbounded retry participates in shutdown.
        const SHUTDOWN_STRUCTURED_WAVES_MAX: usize =
            ui::agent_sessions::AGENT_SESSION_PERSISTED_MAX_ITEMS
                .div_ceil(crate::agent_state_worker::AGENT_STATE_CONTINUATION_MAX)
                + 2;
        for _ in 0..SHUTDOWN_STRUCTURED_WAVES_MAX {
            while self.agent_state_worker.pending_exact_count()
                < crate::agent_state_worker::AGENT_STATE_CONTINUATION_MAX
                && (!self.pending_agent_state_structured.is_empty()
                    || self.agent_sessions_ui.pending_persistence_mutation_count() > 0)
            {
                let before = (
                    self.agent_state_worker.pending_exact_count(),
                    self.pending_agent_state_structured.len(),
                    self.agent_sessions_ui.pending_persistence_mutation_count(),
                );
                self.stage_structured_agent_state(false);
                let after = (
                    self.agent_state_worker.pending_exact_count(),
                    self.pending_agent_state_structured.len(),
                    self.agent_sessions_ui.pending_persistence_mutation_count(),
                );
                if after == before {
                    break;
                }
            }
            if self.agent_state_worker.pending_exact_count() == 0 {
                break;
            }
            let report = self.agent_state_worker.drain_exact_wave_for_shutdown();
            shutdown_agent_state_failed |= report.overflowed()
                || report
                    .receipts()
                    .iter()
                    .any(|receipt| receipt.result().is_err());
        }
        if !self.pending_agent_state_structured.is_empty()
            || self.agent_sessions_ui.pending_persistence_mutation_count() > 0
        {
            shutdown_agent_state_failed = true;
            tracing::warn!(
                kind = "agent_state",
                phase = "shutdown",
                error_code = "backpressure",
                "agent state shutdown mutation backlog did not settle"
            );
        }
        let final_structured = Vec::new();
        let operation_id = self.next_agent_state_operation_id();
        let mux = self.active.workspace_ui.mux();
        let live_items = mux.map_or(0, |mux| {
            mux.tabs.iter().map(|tab| tab.panes.len()).sum::<usize>()
        });
        let desired_items = mux.map_or(0, |mux| {
            self.agent_bindings
                .keys()
                .filter(|session| pane_of_session(mux, **session).is_some())
                .count()
        });
        let final_items = live_items.max(desired_items);
        let bindings = &self.agent_bindings;
        let pending_turn_done_clear = &mut self.pending_turn_done_clear;
        let report = self
            .agent_state_worker
            .shutdown_with_final_binding_reconcile(operation_id, final_items, || {
                let scope = final_scope
                    .ok_or(crate::agent_state_worker::AgentStateErrorCode::InvalidData)?;
                let live_pane_ids = mux
                    .into_iter()
                    .flat_map(|mux| &mux.tabs)
                    .flat_map(|tab| &tab.panes)
                    .map(|pane| pane.id.0.clone())
                    .collect::<Vec<_>>();
                let desired_bindings = bindings
                    .iter()
                    .filter_map(|(session, binding)| {
                        let pane = mux.and_then(|mux| pane_of_session(mux, *session))?;
                        Some(storage::AgentSessionRow {
                            pane_id: pane.0,
                            kind: match binding.kind {
                                crate::agent_detect::AgentKind::Claude => "claude".to_owned(),
                                crate::agent_detect::AgentKind::Codex => "codex".to_owned(),
                            },
                            session_id: binding.session_id.clone(),
                        })
                    })
                    .collect();
                let turn_done_clears = pending_turn_done_clear
                    .take()
                    .map(|(session_key, seen_at)| storage::AgentTurnDoneClear {
                        session_key,
                        seen_at,
                    })
                    .filter(|clear| turn_done_clear_matches_workspace(clear, &scope.workspace_id))
                    .into_iter()
                    .collect();
                AppAgentStateExactRequest::try_new(
                    scope,
                    AppAgentStateExactKind::FinalBindingReconcile {
                        binding: storage::AgentSessionBindingReconcile {
                            live_pane_ids,
                            desired_bindings,
                        },
                        turn_done_clears,
                        structured_mutations: final_structured,
                    },
                )
                .map(Arc::new)
                .ok_or(crate::agent_state_worker::AgentStateErrorCode::ResourceLimit)
            });
        if shutdown_agent_state_failed
            || report.overflowed()
            || report
                .receipts()
                .iter()
                .any(|receipt| receipt.result().is_err())
        {
            tracing::warn!(
                kind = "agent_state",
                phase = "shutdown",
                error_code = "persistence_failed",
                "agent state shutdown did not settle cleanly"
            );
        }
        self.fail_pending_proxy_launches();
        if self
            .db
            .deny_session_scoped_pending_approvals_owned(
                self._pending_approval_owner.as_ref(),
                deppy_core::time::unix_secs_i64(),
            )
            .is_err()
        {
            tracing::warn!(
                kind = "approval",
                phase = "shutdown_reconcile",
                error_code = "storage_failed",
                "종료 중 pending approval 정리 실패"
            );
        }
        self.approval_wake_hub.stop();
        // 웹서버(모바일 PWA)를 runtime보다 먼저 정지 — 브리지가 쥔 command_sink가
        // worker 채널을 살려둔 채 join을 기다리는 순환을 끊는다 (P5 리뷰 P1 종료 데드락;
        // runtime shutdown 플래그가 근본 방어이고 이 순서는 이중 방어 + 접속 정리).
        if let Some(state) = self.web.take() {
            state.server.shutdown();
        }
        // remote TLS 서버를 먼저 정지 — accept 루프·접속·전용 worker(그 세션들 reap)를 정리한다.
        if let Some(state) = self.remote.take() {
            state.server.shutdown();
        }
        // worker join까지 동기 대기 — 셸 자식 프로세스 정리(reap) 보장.
        self.active.runtime.shutdown();
        // warm 워커들도 종료 (계속 실행 중이던 세션들 reap).
        for (_, rt) in self.warm.drain() {
            let mut runtime = rt.runtime;
            runtime.shutdown();
        }
        // 전환으로 background 정리 중이던 옛 워커들도 끝까지 join한다 — detached
        // 스레드는 프로세스 종료 시 join되지 않아 PTY reap이 중단될 수 있다 (codex 리뷰).
        self.pending_shutdowns.join_all();
    }
}

impl eframe::App for App {
    #[cfg(feature = "render-glow")]
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.shutdown_on_exit();
    }

    #[cfg(not(feature = "render-glow"))]
    fn on_exit(&mut self) {
        self.shutdown_on_exit();
    }

    // §14.1 Active↔Warm: 창이 안 보이면(최소화/완전 가림) worker가 snapshot 생성을
    // 멈추게 한다(세션은 유지). logic()은 창이 안 보여 ui()가 스킵될 때도 호출되므로
    // 여기서 감지해야 전이를 놓치지 않는다 (eframe 0.35). `visible()`은 eframe이 ui()
    // 스킵 판단에 쓰는 바로 그 신호(minimized OR occluded — macOS는 occluded로 갱신되어
    // minimized 미갱신 문제를 피한다). None(미보고)이면 안전하게 Active 유지.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 시스템 메모리 압박 레벨 전이 (로드맵 C1 관측 + C2 비상 플러시).
        if let Some((previous, current)) = crate::mem_pressure_monitor::take_level_transition() {
            tracing::warn!(
                kind = "resource",
                phase = "memory_pressure_transition",
                previous = ?previous,
                current = ?current,
            );
            // 격상 시에만 1회 플러시 — OOM-kill은 Drop을 실행하지 않으므로 pending
            // debounce 배치를 미리 커밋한다. send는 try_send 기반 non-blocking이라
            // 큐가 가득해도 재시도 없이 넘어간다(압박 상황에 부하를 더하지 않는다).
            if current > previous {
                for workspace in std::iter::once(&self.active).chain(self.warm.values()) {
                    if let Err(error) = workspace
                        .runtime
                        .send_command(runtime::RuntimeCommand::EmergencyPersistFlush)
                    {
                        tracing::warn!(
                            kind = "resource",
                            phase = "emergency_flush_send_failed",
                            workspace = %workspace.id,
                            error = %error,
                        );
                    }
                }
                // 압박 알림 (로드맵 C3) — 격상 에피소드당 1회. 자리를 비운 사용자에게
                // "곧 앱이 죽을 수 있다 + 어느 세션이 원인인지"를 알린다.
                self.notify_memory_pressure();
                // 해제됐지만 mimalloc이 보유 중인 페이지를 즉시 OS로 반환한다 —
                // 압박 격상 순간에 phys_footprint를 낮춰 OOM-kill 여지를 줄인다.
                // 격상 전이(드문 이벤트)에서만 호출하므로 렌더 핫패스 비용은 없다.
                crate::alloc::purge();
            }
        }
        // Shortcut handling may persist config, switch runtimes, or start protocol/process work.
        // Consume egui input here so none of those effects are reachable from the render pass.
        self.handle_configured_shortcut(ctx);
        if let Some(intent) = self.pending_agent_launcher_intent.take() {
            self.handle_agent_launcher_intent(intent);
        }
        self.poll_agent_launcher_detection();
        // 파일/SQLite/keyring은 worker에서 끝났고, 여기서는 최신 epoch 결과만 짧게 적용한다.
        self.poll_dotenv_sync();
        self.poll_agent_state_worker();
        self.pump_perf_harness();
        self.pump_batch_spawn(ctx);
        if let Some((intent, subject)) = self.pending_connector_dispatch.take() {
            let result = match subject {
                Some(subject) => self
                    .connector_coordinator
                    .dispatch_for_subject(intent, subject),
                None => self.connector_coordinator.dispatch(intent),
            };
            if result.is_err() {
                tracing::warn!(
                    kind = "connector",
                    phase = "dispatch",
                    error_code = "worker_unavailable",
                    "Connector intent dispatch failed"
                );
            }
        }
        // Render가 만든 controller action은 다음 logic tick에서 한 건만 실행한다. Client
        // process 생성과 JSON-RPC 요청은 이 경계 안에서만 시작된다.
        if let Some(action) = self.pending_agent_sessions_action.take() {
            self.agent_sessions_ui
                .sync_controller_config(&self.config.agents);
            if let Some(request) = self.agent_sessions_ui.execute_deferred(action, ctx) {
                self.handle_agent_sessions_request(request);
            }
        }
        // 창이 숨겨져도 App Server JSON-RPC 이벤트를 드레인해 structured session 상태를 최신화한다.
        self.agent_sessions_ui
            .sync_controller_config(&self.config.agents);
        self.agent_sessions_ui.refresh_rate_limits(ctx);
        self.agent_sessions_ui.poll();
        self.stage_structured_agent_state(true);
        for notice in self.agent_sessions_ui.drain_status_notices() {
            self.notifications_ui.on_structured_status(
                &notice.workspace_id,
                &notice.session_id,
                notice.status,
                &notice.title,
                &self.i18n,
            );
        }
        let structured_alive = self.agent_sessions_ui.session_ids();
        self.notifications_ui
            .retain_structured_sessions(&structured_alive);
        // 설정이 닫혀도 stale generation 결과를 계속 버려 worker의 bounded 결과 큐가
        // 평문 secret을 붙잡은 채 막히지 않게 한다.
        self.poll_env_secret_reveals();
        self.poll_env_secret_reveal_admission();
        // Settings mutation completion can invalidate the Environment projection. Drain it before
        // project admission so the same event-driven logic pass submits the newest generation.
        self.poll_settings_outcomes();
        self.poll_settings_job_admission();
        self.poll_env_api_project_rows();
        self.poll_file_tree_maintenance(ctx);
        // Settings completion을 먼저 drain해야 folder-result backpressure가 같은 wake에서
        // 해제된다. Host task는 그 뒤 한 건만 적용/시작한다.
        self.poll_app_host_io(ctx);
        self.poll_app_controller(ctx);
        // Drain the notice from the runtime that rendered it before a queued workspace switch can
        // move that runtime into the warm pool.
        if let Some(intent) = self.active.workspace_ui.take_notice_intent() {
            platform::notify(intent.summary(), intent.body());
        }
        self.poll_workspace_controller();
        self.poll_pending_workspace_focus();
        self.poll_turn_done_clear();
        self.apply_pending_visual_settings(ctx);
        self.poll_worktree_jobs();
        self.refresh_agent_workspace_cwd();
        let want_active = ctx.input(|input| input.viewport().visible()) != Some(false);
        let home_active = want_active
            && self.agent_terminal_ui.view() == ui::agent_terminal::AgentTerminalView::Home;
        self.status_feed_rx.set_active(home_active);
        // 앱 시작 직후 1회 폴링 — 홈을 열지 않아도 하단 서비스 점등이 바로 켜지게 한다.
        // 이후엔 idle TTL로 워커가 회수되고, 홈 활성/수동 갱신 시 다시 조회한다.
        if !self.status_feed_startup_polled {
            self.status_feed_startup_polled = true;
            let _ = self.status_feed_refresh.send(());
        }
        // 외부 feed/번역/로컬 모델 worker 채널과 그에 따른 캐시 파일/CLI 작업은 render
        // 밖에서만 처리한다. 데이터가 없으면 try_recv와 조건 확인 외 추가 자원은 없다.
        let mut status_feed_received = false;
        while let Ok(snapshot) = self.status_feed_rx.try_recv() {
            status_feed_received = true;
            if snapshot.claude.is_some() {
                self.status_feed.claude = snapshot.claude;
            }
            if snapshot.openai.is_some() {
                self.status_feed.openai = snapshot.openai;
            }
            if snapshot.github.is_some() {
                self.status_feed.github = snapshot.github;
            }
            if snapshot.hugging_face.is_some() {
                self.status_feed.hugging_face = snapshot.hugging_face;
            }
            if snapshot.grok.is_some() {
                self.status_feed.grok = snapshot.grok;
            }
        }
        if status_feed_received {
            self.sync_home_notice_badge(home_active, ctx);
        }
        self.pump_notice_translations(ctx);
        self.pump_ollama_detect(ctx);
        // Listener readiness, latest-only snapshots, launch correlation, and session-scoped
        // cleanup are all event-driven. No timer/repaint is scheduled while the hub is absent.
        self.poll_approval_wake();

        // macOS 네이티브 메뉴 이벤트 (main.rs install_macos_menu)
        #[cfg(target_os = "macos")]
        while let Ok(event) = muda::MenuEvent::receiver().try_recv() {
            if event.id() == "settings" {
                self.settings_open = true;
            }
        }

        // 오프스크린 방어: 실행 중 외부 모니터가 분리되면 창이 존재하지 않는 좌표에
        // 남아 "죽은 것처럼" 보인다 (2026-07-05 실증). 창이 어느 모니터에도 속하지
        // 않으면(monitor_size None — macOS는 완전 오프스크린 창의 screen이 nil)
        // 주 화면 안으로 옮긴다. 쿨다운 2s — 이동 반영 전 재발사 방지.
        let offscreen =
            ctx.input(|i| i.viewport().outer_rect.is_some() && i.viewport().monitor_size.is_none());
        if offscreen && self.last_offscreen_fix.elapsed() >= std::time::Duration::from_secs(2) {
            self.last_offscreen_fix = std::time::Instant::now();
            tracing::warn!("창이 화면 밖 — 주 화면으로 이동");
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(80.0, 80.0)));
        }
        // 시작 위치 강제: 항상 주 화면에 뜬다. NativeOptions.centered는 주 화면 크기로
        // 계산한 좌표를 macOS winit이 보조 모니터 로컬 좌표로 적용하는 문제가 있어
        // (2026-07-05 실증: (255,137) 지정 → 왼쪽 모니터 -2303) 창 생성 후 런타임
        // 명령으로 1회 이동한다 — 이 경로는 글로벌 좌표로 동작한다. 이후 사용자가
        // 옮기는 위치는 존중(1회뿐, persist_window=false라 다음 시작도 여기부터).
        if !self.startup_positioned && ctx.input(|i| i.viewport().outer_rect.is_some()) {
            self.startup_positioned = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
                120.0, 60.0,
            )));
            // 이동 직후 key window 상태가 흔들려 키 입력이 일시적으로 안 먹는 사례
            // (2026-07-05 사용자 보고) — 창 포커스를 명시 재요청한다.
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }

        if want_active != self.active.render_active {
            let state = if want_active {
                runtime::WorkspaceRuntimeState::Active
            } else {
                runtime::WorkspaceRuntimeState::Warm
            };
            let delivered = self
                .active
                .runtime
                .send_command(runtime::RuntimeCommand::SetWorkspaceState(state))
                .is_ok();
            self.active.render_active = want_active;
            clear_pending_replay_resync_after_activation(
                &mut self.active.pending_replay_resync,
                want_active,
                delivered,
            );
            if want_active {
                // 재개된 Viewport push는 비동기 — 다음 프레임을 예약해 드레인한다.
                // (안 그러면 hidden 중 종료된 pane이 stale/"연결 중…"에 갇힐 수 있다)
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
        }

        // warm 워커의 이벤트는 drain해서 그 워커의 pending_events에 '누적'한다 (버리지
        // 않는다 — SessionExited/StatusChanged 같은 일회성 lifecycle 이벤트를 버리면
        // 재활성 시 종료된 pane이 실행 중으로 보인다, codex 리뷰). 재활성 시 fresh가 아닌
        // 이 누적분을 그대로 ui()가 처리해 상태를 재구성한다. 렌더/알림은 활성만.
        let mut runtime_stream_overflowed = false;
        let mut approval_events_overflowed = false;
        let mut approval_runtime_events = Vec::new();
        let mut warm_lifecycle_changed = false;
        for rt in self.warm.values_mut() {
            let events = rt.events.drain();
            // active와 동일 — 예산 초과 backlog는 wake가 이미 소진돼 직접 예약해야 한다.
            if rt.events.has_backlog() {
                ctx.request_repaint();
            }
            if rt.events.take_overflowed() {
                runtime_stream_overflowed = true;
                approval_events_overflowed = true;
                rt.event_overflow_pending = true;
            }
            if !events.is_empty() {
                warm_lifecycle_changed = true;
                Self::record_activity_events(rt, &events);
                approval_runtime_events.extend(
                    events
                        .iter()
                        .filter(|event| {
                            matches!(
                                event,
                                runtime::RuntimeEvent::AgentSpawnResolved { .. }
                                    | runtime::RuntimeEvent::SessionExited { .. }
                            )
                        })
                        .cloned()
                        .map(|event| (rt.id.clone(), event)),
                );
                let agent_providers = rt.workspace_ui.agent_providers();
                // warm workspace도 알림은 만든다 (background 완료/승인 통지) — (ws, session)로
                // 식별해 워커 간 SessionId 충돌을 피한다. 렌더용으로는 pending에 누적.
                Self::process_ws_notifications(
                    &mut self.notifications_ui,
                    &rt.id,
                    &events,
                    &mut rt.session_titles,
                    &agent_providers,
                    &self.i18n,
                );
                // 표시 상태(mux 구조·세션 status·종료 결과)는 warm에서도 지금 반영한다 —
                // 안 하면 fleet/사이드바가 warm 진입 시점 스냅샷에 얼어붙어 종료된 pane이
                // 계속 실행 중으로, 새 pane은 없는 것으로 보인다. 렌더 상태는 그대로
                // pending에 남겨 재활성 replay가 처리한다(둘 다 last-write-wins라 중복
                // 적용이 안전하다).
                rt.workspace_ui.apply_warm_events(&events);
                rt.pending_events.extend(events.into_iter().filter(|event| {
                    !matches!(event, runtime::RuntimeEvent::AgentSpawnResolved { .. })
                }));
                // MuxUpdated는 매번 전체 스냅샷이라 오래된 건 최신에 완전히 대체된다.
                // chatty한 warm 워커가 pending_events를 무한 누적하지 않도록 최신 하나만
                // 남기고 합친다 (lifecycle은 순서 보존, Viewport는 세션별 최신본 — replay 정확성).
                // 새 이벤트가 들어온 이 분기에서만 호출돼 프레임마다 도는 걸 피한다.
                let compacted = coalesce_mux_updated(&mut rt.pending_events);
                rt.pending_replay_resync |= compacted.overflowed;
            }
            if rt.event_overflow_pending && rt.events.durable_backlog_exhausted() {
                rt.events = Self::subscribe_runtime_events(&rt.runtime, ctx);
                rt.event_overflow_pending = false;
                // warm→active 전환 자체가 전체 mux/viewport snapshot을 보내므로 지금은
                // worker를 깨우지 않는다.
            }
        }
        for (workspace_id, event) in approval_runtime_events {
            self.observe_agent_launcher_runtime_events(&workspace_id, std::slice::from_ref(&event));
            self.observe_approval_runtime_events(&workspace_id, std::slice::from_ref(&event));
        }
        self.runtime_stream_warning |= runtime_stream_overflowed;
        if warm_lifecycle_changed {
            self.refresh_warm_idle_deadline();
        }
        self.maintain_warm_evictions(std::time::Instant::now());

        // 이벤트 drain + 알림 생성은 non-render 경로인 여기서 한다 (§14.1 Warm:
        // ui()가 스킵돼도 승인/완료/실패 알림은 유지). worker의 wake가 숨겨진 UI를
        // 깨워 이 logic()을 돌린다. 렌더용으로는 pending_events에 쌓아 ui()가 소비한다.
        let new_events = self.active.events.drain();
        if !new_events.is_empty() {
            Self::record_activity_events(&mut self.active, &new_events);
            for event in &new_events {
                if let runtime::RuntimeEvent::AgentSpawnResolved { session, .. } = event {
                    if session.is_some() {
                        self.agents_ui.observe_launch_succeeded();
                    } else {
                        self.agents_ui.observe_launch_failed();
                    }
                }
            }
            let active_id = self.active.id.clone();
            self.observe_agent_launcher_runtime_events(&active_id, &new_events);
            self.observe_approval_runtime_events(&active_id, &new_events);
            let agent_providers = self.active.workspace_ui.agent_providers();
            Self::process_ws_notifications(
                &mut self.notifications_ui,
                &self.active.id,
                &new_events,
                &mut self.active.session_titles,
                &agent_providers,
                &self.i18n,
            );
            // 창이 숨겨져(render_active=false) ui()가 스킵되면 active의 pending도 warm처럼
            // 무한 누적된다. 이때 표시 상태도 warm과 같은 경로로 먼저 반영한 뒤 replay를
            // 유계화한다. 보일 때는 ui()가 매 프레임 take()로 소비해 자라지 않으므로
            // coalesce가 불필요하다.
            if !self.active.render_active {
                let compacted = admit_hidden_active_replay_events(
                    &mut self.active.workspace_ui,
                    &mut self.active.pending_events,
                    new_events,
                );
                self.active.pending_replay_resync |= compacted.overflowed;
            } else {
                self.active
                    .pending_events
                    .extend(new_events.into_iter().filter(|event| {
                        !matches!(event, runtime::RuntimeEvent::AgentSpawnResolved { .. })
                    }));
            }
            // 여기서 리페인트를 재요청하지 않는다 — 이벤트를 여기까지 실어나른 모든 경로
            // (emit_gated의 Viewport/InputPressure/ResourceUsage slot + enqueue_durable_event)가
            // 이미 subscribe_runtime_events의 wake로 리페인트를 요청했다. 재요청하면 이번
            // 프레임이 그리는 내용을 위해 프레임을 한 장 더 잡고, egui가 거기에 settle 프레임을
            // 하나 더 붙여 갱신 1회당 3프레임이 된다 (agenttui 실측: 페인트의 70%가 헛 프레임).
        }
        // 예외: drain이 durable 예산(256/프레임)을 다 써 backlog를 남겼으면 다음
        // 프레임을 직접 예약한다 — 남은 이벤트의 wake는 이미 coalesce돼 사라졌으므로
        // 예약하지 않으면 lifecycle 이벤트가 무관한 리페인트까지 굶는다 (codex 리뷰 HIGH).
        if self.active.events.has_backlog() {
            ctx.request_repaint();
        }
        if self.active.events.take_overflowed() {
            self.runtime_stream_warning = true;
            approval_events_overflowed = true;
            self.active.event_overflow_pending = true;
        }
        if approval_events_overflowed {
            self.fail_closed_approval_event_overflow();
        }
        if self.active.event_overflow_pending && self.active.events.durable_backlog_exhausted() {
            self.active.events = Self::subscribe_runtime_events(&self.active.runtime, ctx);
            self.active.event_overflow_pending = false;
            self.active.event_resync_pending = self.active.render_active;
        }
        if self.active.event_resync_pending
            && self
                .active
                .runtime
                .send_command(runtime::RuntimeCommand::SetWorkspaceState(
                    runtime::WorkspaceRuntimeState::Warm,
                ))
                .is_ok()
            && self
                .active
                .runtime
                .send_command(runtime::RuntimeCommand::SetWorkspaceState(
                    runtime::WorkspaceRuntimeState::Active,
                ))
                .is_ok()
        {
            self.active.event_resync_pending = false;
            ctx.request_repaint();
        }

        // 폰 미러 진입(I1b-2) — 웹 스레드가 큐에 넣은 워크스페이스 전환 요청을 처리한다.
        // ui()가 아닌 여기(logic)에서 — 데스크탑 창이 숨겨져도(폰 전용 사용) 전환돼야 한다.
        self.drain_web_switch_requests();
        self.expire_web_notice();

        // 에이전트 감지 워커 입력 갱신 + 결과 드레인 — ui()가 아닌 여기(logic)에서 해야
        // hidden/minimized로 ui()가 스킵돼도 결과 채널이 누적되지 않는다(codex 리뷰).
        self.poll_agent_detect();

        // Connector worker가 wake한 latest-only immutable snapshot은 non-render logic에서만
        // 교체한다. UI는 아래 ui()에서 Arc projection을 읽을 뿐 DB/MCP를 호출하지 않는다.
        let previous_connector_config_revision =
            self.connector_snapshot_reader.snapshot().config_revision;
        if self.connector_snapshot_reader.refresh()
            && self.connector_snapshot_reader.snapshot().config_revision
                != previous_connector_config_revision
        {
            self.invalidate_env_profile_ui();
        }

        self.refresh_activity_snapshot_if_needed();
        // Render and non-render WorkspaceUi producers share one capacity-eight protocol contract.
        // Drain each resident runtime only in logic, returning exact operation/generation
        // completions so queue and in-flight slots cannot accumulate across frames.
        self.poll_workspace_protocol_intents();
        while let Some(intent) = self.notifications_ui.pop_native_intent() {
            platform::notify(intent.summary(), intent.body());
        }
        self.dispatch_storm_notifications();
        if let Some(action) = self.pending_storm_action.take() {
            self.dispatch_storm_action(action);
        }

        // 폰 대시보드가 볼 워크스페이스 스냅샷(전체 + 해석된 세션 이름) 동기화.
        // ui()가 아닌 logic()에서 — 창이 숨겨져도 폰에는 최신 구성이 보여야 한다.
        // 브리지가 변화 없으면 무시하므로(값 비교) 유휴 프레임 비용은 사실상 0이다.
        self.sync_web_workspaces(std::time::Instant::now());

        // 렌더러 A/B 실측 드라이버 (B1) — env 미설정이면 즉시 반환한다.
        self.bench_step(ctx);
    }

    // egui 0.35부터 update(&Context) 대신 ui(&mut Ui) 시그니처를 쓴다.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frame_stats.begin();
        if let Some(bench) = self.bench.as_mut() {
            bench.frame_begin(ui.ctx());
        }
        let text = self.i18n.clone();
        let mut unread_before = 0;
        // 벨 팝오버의 최근 알림 클릭 — 설정→알림(notif_click)과 같은 네비게이션 경로로
        // 아래에서 함께 처리한다.
        let mut inbox_click = None;
        // 타이틀바 통합 바: 패널 기본 inner_margin(8)을 없애 상단 경계에 붙이고 좌측
        // 여백을 제거한다(#67 사용자). 항목은 신호등과 38pt 브랜드 바 안에서
        // 맞춰 세로 중앙 정렬.
        let bar_h = TOP_BAR_HEIGHT;
        let top_frame =
            egui::Frame::side_top_panel(&ui.ctx().global_style()).inner_margin(egui::Margin::ZERO);
        egui::Panel::top("top_bar")
            .resizable(false)
            .frame(top_frame)
            .show(ui, |ui| {
                // 빈 곳을 잡으면 창을 드래그로 옮긴다. auto-sized Panel의 max_rect는
                // content 측정 전 매우 커질 수 있으므로 실제 titlebar 높이만 hit-test한다.
                let bar_rect = egui::Rect::from_min_size(
                    ui.cursor().min,
                    egui::vec2(ui.available_width(), bar_h),
                );
                let drag = ui.interact(
                    bar_rect,
                    egui::Id::new("titlebar_drag"),
                    egui::Sense::click_and_drag(),
                );
                if drag.drag_started_by(egui::PointerButton::Primary) {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), bar_h),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        // 신호등(닫기/최소화/전체화면) 폭만큼 왼쪽 여백 — macOS. 브랜드
                        // 텍스트("Deppy Sijo"/"AI Agent Workspace")는 제거(2026-07-25
                        // 사용자) — 신호등 3개만 이 자리에서 수직 중앙 정렬로 보인다
                        // (실제 재배치는 main.rs의 set_traffic_light_titlebar_height).
                        #[cfg(target_os = "macos")]
                        ui.add_space(76.0);
                        // 신호등 옆 "+" — 워크스페이스 추가(폴더 선택), 사이드바의
                        // CreateWorkspaceFromPicker와 동일 경로(2026-07-25 사용자).
                        if tbtn_response(ui, "+".to_owned(), false)
                            .on_hover_text(text.t("sidebar.empty.start_workspace", &[]))
                            .clicked()
                            && self.pending_app_host_action.is_none()
                        {
                            self.pending_app_host_action = Some(AppHostIoAction::FolderPicker(
                                FolderPickerPurpose::SwitchWorkspace,
                            ));
                            ui.ctx().request_repaint();
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_space(10.0);
                            // 우측: 로케일. 중앙에는 검색/워크스페이스 선택기를 두지
                            // 않아 목업처럼 작업 표면이 비어 있게 한다. 메모리 표시는
                            // 하단 상태바로 일원화(2026-07-18 사용자 — 상/하단 수치가
                            // 샘플 시점 차이로 어긋나 보였음).
                            let locale_short = self
                                .config
                                .i18n
                                .locale
                                .split('-')
                                .next()
                                .unwrap_or(&self.config.i18n.locale);
                            ui.weak(locale_short.to_owned());
                            let tbtn = |ui: &mut egui::Ui, label: String, selected: bool| -> bool {
                                tbtn_response(ui, label, selected).clicked()
                            };
                            let sel = self.settings_open;
                            if tbtn(ui, text.t("top.settings", &[]), sel) {
                                self.settings_open = !sel;
                            }
                            let unread = self.notifications_ui.unread();
                            unread_before = unread;
                            let waiting =
                                self.approvals_ui.pending().len() + self.global_waiting.len();
                            let bell_label = if waiting > 0 {
                                format!("🔔 {waiting}")
                            } else if unread > 0 {
                                format!("🔔 {unread}")
                            } else {
                                "🔔".to_owned()
                            };
                            let bell_open =
                                egui::Popup::is_id_open(ui.ctx(), Self::inbox_popup_id());
                            let bell = tbtn_response(ui, bell_label, bell_open)
                                .on_hover_text(text.t("top.notifications", &[]));
                            inbox_click = self.inbox_popup(&bell, &text);
                            // Agents 진입은 사이드바 하단 nav가 담당한다 — 상단바 버튼은
                            // 삭제(2026-07-18 사용자 확정). 단축키·기타 진입점은 유지.
                        });
                    },
                );
                // 툴바-본문 경계선은 egui Panel::top이 자체로 그린다 — 커스텀 hairline을
                // 추가하면 패널 여백 탓에 끝까지 안 닿는 짧은 선이 겹쳤다(#65 사용자).
            });

        // 폭주 경고 배너 (로드맵 B2) — 타이틀바 바로 아래, 어떤 탭을 보든 보이게
        // top 패널로 얹는다. 정책상 여기엔 조치 버튼이 없고 [닫기]뿐이다(B3에서 추가).
        match self.confirmed_storm_summary() {
            Some(_) if self.storm_banner_dismissed => {}
            Some((count, peak)) => {
                let banner_frame = egui::Frame::side_top_panel(&ui.ctx().global_style())
                    .fill(egui::Color32::from_rgb(120, 40, 40))
                    .inner_margin(egui::Margin::symmetric(14, 8));
                egui::Panel::top("process_storm_banner")
                    .resizable(false)
                    .frame(banner_frame)
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                egui::RichText::new(text.t("process_storm.banner.title", &[]))
                                    .strong()
                                    .color(egui::Color32::WHITE),
                            );
                            ui.label(
                                egui::RichText::new(text.t(
                                    "process_storm.banner.detail",
                                    &[
                                        ("sessions", &count.to_string()),
                                        ("count", &peak.to_string()),
                                    ],
                                ))
                                .color(egui::Color32::from_rgb(240, 220, 220)),
                            );
                            let any_frozen = self.any_storm_session_frozen();
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.button(text.t("process_storm.dismiss", &[])).clicked() {
                                        self.storm_banner_dismissed = true;
                                    }
                                    // 종료(되돌릴 수 없음) → 재개/동결 순. 정책상 자동 개입
                                    // 없음 — 모두 사용자 클릭이다.
                                    if ui.button(text.t("process_storm.kill", &[])).clicked() {
                                        self.pending_storm_action = Some(StormAction::Kill);
                                    }
                                    if any_frozen {
                                        if ui.button(text.t("process_storm.resume", &[])).clicked()
                                        {
                                            self.pending_storm_action = Some(StormAction::Resume);
                                        }
                                    } else if ui
                                        .button(text.t("process_storm.freeze", &[]))
                                        .clicked()
                                    {
                                        self.pending_storm_action = Some(StormAction::Freeze);
                                    }
                                },
                            );
                        });
                    });
            }
            None => {
                // 모든 폭주 해소 — 다음 폭주에 배너가 다시 뜨도록 dismiss 리셋.
                self.storm_banner_dismissed = false;
            }
        }

        // 세션 기본 제목을 "셀 N" 대신 프로젝트명(폴더명 ≈ 깃 레포명, 없으면 "~")으로
        // 표시하도록 활성 workspace 이름을 WorkspaceUi에 넘긴다(사용자 요청).
        let project_name = self
            .workspaces
            .iter()
            .find(|w| w.id == self.active.id)
            .map(Self::workspace_display_name);
        self.active.workspace_ui.set_project_name(project_name);
        // 터미널 폰트 역보정용 UI 배율 전달(zoom_factor로 커진 만큼 font_size를 되돌린다).
        self.active
            .workspace_ui
            .set_ui_scale(self.config.ui.ui_scale);
        // 「에이전트로 보내기」 프리셋(설정) 미러 — pane 우클릭 메뉴가 쓴다.
        self.active
            .workspace_ui
            .set_agent_send_presets(self.config.ui.agent_send_presets.clone());

        // 폴더 트리 사이드바 (FT-1) — CentralPanel보다 먼저 배치해야 한다 (§9-1).
        // OFF(None)면 Panel 자체를 만들지 않는다 (§6 리소스 0).
        let mut terminal_sessions = self.active.workspace_ui.session_entries(
            &text,
            &self.agent_activity,
            &self.agent_needs_input,
            &self.agent_turn_done,
            &self.agent_working,
        );
        // 저장된 에이전트가 있고 지금 실행 중이 아닌 pane — 컨텍스트 메뉴 '이어가기' 노출.
        for entry in &mut terminal_sessions {
            entry.resumable =
                entry.agent_line.is_none() && self.restore_agents.contains_key(&entry.pane.0);
            // 워크트리 메뉴 노출 조건 — 프레임마다 도는 경로라 lsof fallback 없이
            // 감지 캐시만 본다 (실제 조회는 dispatch의 session_cwd_lookup, PR-W).
            entry.has_cwd = entry
                .session
                .is_some_and(|s| self.session_cwds.contains_key(&s));
            // 워크트리 삭제 메뉴 노출 조건 — cwd가 이 앱이 만든 워크트리 하위인가.
            entry.in_worktree = entry.session.is_some_and(|s| {
                self.session_cwds
                    .get(&s)
                    .is_some_and(|cwd| cwd.contains("/.deppy/worktrees/"))
            });
        }
        // 완료/입력대기 주목(6px 레일·펄스) 갱신 + 확인 시 완료 소비. Agents 패널도
        // 같은 상태 원천을 사용하므로 사이드바가 꺼져 있어도 계산한다.
        self.update_session_alerts(&mut terminal_sessions);
        let pty_agent_surfaces = self.pty_agent_surfaces(&terminal_sessions);
        let active_workspace_id = self.active.id.clone();
        // 종료 숨김 해제 — 종료 때 닫히던 pane이 아닌 **새** 세션이 활성에 나타나면
        // (새 셸/에이전트) 목록에 복귀시킨다. 종료 직후 exit 이벤트를 기다리는 옛
        // pane은 기록된 집합에 있어 해제 조건에 걸리지 않는다.
        if self
            .closed_workspaces
            .get(&active_workspace_id)
            .is_some_and(|state| {
                state
                    .should_auto_reveal(terminal_sessions.iter().map(|entry| entry.pane.0.as_str()))
            })
        {
            self.reveal_closed_workspace(&active_workspace_id);
        }
        // 명시적으로 종료한 워크스페이스는 목록에서 숨긴다(DB는 보존 — 종료 ≠ 삭제).
        let sidebar_workspaces: Vec<_> = self
            .workspaces
            .iter()
            .filter(|workspace| {
                workspace_visible_after_close(&self.closed_workspaces, &workspace.id)
            })
            .map(|workspace| {
                let waiting_sessions: std::collections::HashSet<_> = self
                    .global_waiting
                    .iter()
                    .filter(|(workspace_id, _, _)| workspace_id == &workspace.id)
                    .map(|(_, session, _)| *session)
                    .collect();
                let (state, summary) = if workspace.id == active_workspace_id {
                    let mut summary = ui::file_tree::SidebarSessionSummary::default();
                    for entry in &terminal_sessions {
                        summary.add(
                            entry.status,
                            entry
                                .session
                                .is_some_and(|session| waiting_sessions.contains(&session)),
                        );
                    }
                    (ui::file_tree::SidebarWorkspaceState::Active, summary)
                } else if let Some(runtime) = self.warm.get(&workspace.id) {
                    let mut summary = ui::file_tree::SidebarSessionSummary::default();
                    for session in runtime.session_titles.keys().copied() {
                        summary.add(
                            runtime.workspace_ui.last_session_status(session),
                            waiting_sessions.contains(&session),
                        );
                    }
                    (ui::file_tree::SidebarWorkspaceState::Warm, summary)
                } else {
                    (
                        ui::file_tree::SidebarWorkspaceState::Idle,
                        ui::file_tree::SidebarSessionSummary::inactive(
                            self.persisted_activity_panes
                                .get(&workspace.id)
                                .map_or(0, Vec::len),
                        ),
                    )
                };
                ui::file_tree::SidebarWorkspaceEntry {
                    id: workspace.id.clone(),
                    name: Self::workspace_display_name(workspace),
                    repo: workspace_git_label(&workspace.path),
                    state,
                    summary,
                }
            })
            .collect();
        // 사이드바 펼침은 활성 선택과 독립적이다. 활성 세션뿐 아니라 warm runtime의
        // 마지막 mux 스냅샷도 workspace별로 넘겨 이전 트리를 전환 후에도 유지한다.
        // active 전용 transcript map은 SessionId가 workspace마다 재사용될 수 있어 warm에
        // 섞지 않고, workspace namespace가 있는 global_waiting만 해당 id로 필터한다.
        let no_activity: std::collections::HashMap<
            runtime::SessionId,
            crate::agent_transcript::AgentActivity,
        > = std::collections::HashMap::new();
        let mut sidebar_sessions: std::collections::HashMap<
            String,
            Vec<ui::file_tree::SessionEntry>,
        > = std::collections::HashMap::new();
        for workspace in &sidebar_workspaces {
            if workspace.id == active_workspace_id {
                continue;
            }
            let Some(warm_runtime) = self.warm.get(&workspace.id) else {
                continue;
            };
            let needs_input: std::collections::HashSet<_> = self
                .global_waiting
                .iter()
                .filter(|(workspace_id, _, _)| workspace_id == &workspace.id)
                .map(|(_, session, _)| *session)
                .collect();
            // hook "작업 중"(v32) — warm 워크스페이스도 사이드바에서 작업 중을 보인다.
            let working: std::collections::HashSet<_> = self
                .global_working
                .iter()
                .filter(|(workspace_id, _)| workspace_id == &workspace.id)
                .map(|(_, session)| *session)
                .collect();
            // turn_done 전역화(warm turn_done 격차, 감사 발견) — warm 워크스페이스도
            // 사이드바에서 "완료"를 보인다.
            let turn_done: std::collections::HashMap<_, _> = self
                .global_turn_done
                .iter()
                .filter(|((workspace_id, _), _)| workspace_id == &workspace.id)
                .map(|((_, session), at)| (*session, *at))
                .collect();
            let mut entries = warm_runtime.workspace_ui.session_entries(
                &text,
                &no_activity,
                &needs_input,
                &turn_done,
                &working,
            );
            for entry in &mut entries {
                // workspace가 warm이면 그 안의 과거 focused_pane은 전역 포커스가 아니다.
                entry.focused = false;
                entry.attention = entry
                    .session
                    .is_some_and(|session| needs_input.contains(&session));
                entry.resumable =
                    entry.agent_line.is_none() && self.restore_agents.contains_key(&entry.pane.0);
            }
            sidebar_sessions.insert(workspace.id.clone(), entries);
        }
        sidebar_sessions.insert(active_workspace_id.clone(), terminal_sessions);
        // Home을 보고 있는 동안 도착했거나 이미 표시 중인 공지는 읽음이다. sidebar
        // snapshot을 만들기 전에 반영해 같은 프레임에 Home 배지가 사라지게 한다.
        if self.agent_terminal_ui.view() == ui::agent_terminal::AgentTerminalView::Home {
            self.sync_home_notice_badge(true, ui.ctx());
        }
        let inbox_count = self.approvals_ui.pending().len()
            + self.global_waiting.len()
            + self.notifications_ui.unread();
        let sidebar_snapshot = ui::file_tree::SidebarSnapshot {
            active_workspace_id: &active_workspace_id,
            workspaces: &sidebar_workspaces,
            view: self.agent_terminal_ui.view(),
            home_notice_count: self.home_notice_unread,
            inbox_count,
            // fleet 배지 = 주목 필요한 에이전트 수. global_waiting(needs-input, 전 워크스페이스)
            // 을 싼 프록시로 쓴다 — 매 프레임 build_fleet_sessions를 돌리지 않는다.
            fleet_count: self.global_waiting.len(),
            agents_open: self.agent_sessions_ui.is_open(),
        };

        // 500ms 캐시에서 꺼내 쓰고 프레임 끝에 되돌린다(take/put-back) — 참조로 들면
        // 아래 render_composer_dock(&mut self)와 빌림이 충돌한다. 상태 스트립은
        // 사이드바보다 먼저 선언해야 좌측 도크 아래까지 창 전체 폭을 차지한다.
        let (activity_rows_stamp, activity_rows) = match self.activity_rows_cache.take() {
            Some(snapshot) => snapshot,
            None => (
                std::time::Instant::now(),
                ui::activity::ActivitySnapshot::empty(),
            ),
        };
        let waiting_count = self.approvals_ui.pending().len() + self.global_waiting.len();
        // 상태바도 Home/Connector와 동일한 immutable overview snapshot을 사용한다.
        let mcp_count = self
            .connector_snapshot_reader
            .snapshot()
            .servers
            .iter()
            .filter(|server| server.enabled)
            .count();
        egui::Panel::bottom("agent_terminal_status_bar")
            .resizable(false)
            .exact_size(26.0)
            .frame(
                egui::Frame::side_top_panel(&ui.ctx().global_style())
                    .inner_margin(egui::Margin::ZERO),
            )
            .show(ui, |ui| {
                self.agent_terminal_ui.status_bar(
                    ui,
                    claude_usage_snapshot().or_else(|| crate::claude_usage::current(ui.ctx())),
                    self.agent_sessions_ui.codex_usage(),
                    activity_rows.rows(),
                    waiting_count,
                    mcp_count,
                    &self.status_feed,
                    &text,
                );
            });

        // 새 워크스페이스로 만들 폴더 — 사이드바 빈 상태 CTA와 설정 화면 양쪽이 채우고,
        // 프레임 끝의 공통 생성/전환 흐름이 소비한다(선언을 사이드바 dispatch보다 앞에).
        if self.file_tree.is_some() {
            let sidebar_action = self
                .file_tree
                .as_mut()
                .and_then(|tree| tree.panel(ui, &sidebar_sessions, &sidebar_snapshot, &text));
            // 워처의 .env* 변경 신호 → 활성 워크스페이스에서 .env가 바뀌거나 사라져도
            // 즉시 재동기화 + 기본 env 재전송 — 시작/전환 시에만 동기화하면 삭제된
            // .env의 secret이 새 셸에 계속 주입된다(codex High).
            let env_changed = self
                .file_tree
                .as_mut()
                .is_some_and(|tree| !tree.take_env_warning_candidates().is_empty());
            if env_changed {
                self.stage_workspace_controller_action(WorkspaceControllerAction::SyncDotenv);
            }
            // 파일 트리가 이번 프레임 ⌘V/⌘C를 소비했으면 같은 제스처가 터미널로도 흘러
            // 이중 처리(경로 삽입 붙여넣기/선택 복사 pasteboard 덮어쓰기)되는 것을
            // 누른다 — 사이드바(좌측 패널)가 workspace show()보다 먼저 도는 순서 전제.
            if let Some((paste_consumed, copy_consumed)) = self
                .file_tree
                .as_mut()
                .map(|tree| tree.take_clipboard_shortcut_consumption())
                && (paste_consumed || copy_consumed)
            {
                self.active
                    .workspace_ui
                    .suppress_clipboard_shortcuts_this_frame(paste_consumed, copy_consumed);
            }
            match sidebar_action {
                Some(ui::file_tree::SidebarAction::SwitchWorkspace(workspace_id)) => {
                    self.agent_terminal_ui
                        .set_view(ui::agent_terminal::AgentTerminalView::Terminal);
                    self.stage_workspace_controller_action(
                        WorkspaceControllerAction::SwitchWorkspace(workspace_id),
                    );
                }
                Some(ui::file_tree::SidebarAction::ShowHome) => {
                    // 재클릭 토글 — 이미 홈이면 터미널로 복귀 (2026-07-18 확정 디자인).
                    self.agent_terminal_ui.set_view(
                        if self.agent_terminal_ui.view()
                            == ui::agent_terminal::AgentTerminalView::Home
                        {
                            ui::agent_terminal::AgentTerminalView::Terminal
                        } else {
                            ui::agent_terminal::AgentTerminalView::Home
                        },
                    );
                }
                Some(ui::file_tree::SidebarAction::ShowInbox) => {
                    // 작업함 전체 페이지 — 재클릭 토글 규칙은 홈과 동일.
                    self.agent_terminal_ui.set_view(
                        if self.agent_terminal_ui.view()
                            == ui::agent_terminal::AgentTerminalView::Inbox
                        {
                            ui::agent_terminal::AgentTerminalView::Terminal
                        } else {
                            ui::agent_terminal::AgentTerminalView::Inbox
                        },
                    );
                }
                Some(ui::file_tree::SidebarAction::ShowFleet) => {
                    // fleet 그리드 — 재클릭 토글 규칙은 홈/작업함과 동일.
                    self.agent_terminal_ui.set_view(
                        if self.agent_terminal_ui.view()
                            == ui::agent_terminal::AgentTerminalView::Fleet
                        {
                            ui::agent_terminal::AgentTerminalView::Terminal
                        } else {
                            ui::agent_terminal::AgentTerminalView::Fleet
                        },
                    );
                }
                Some(ui::file_tree::SidebarAction::OpenAgents) => {
                    self.agent_sessions_ui.open();
                }
                Some(ui::file_tree::SidebarAction::OpenMacosFileAccessSettings) => {
                    if self.pending_app_controller_action.is_none() {
                        self.pending_app_controller_action =
                            Some(AppControllerAction::OpenFileAccessSettings);
                        ui.ctx().request_repaint();
                    }
                }
                // "터미널에 경로 삽입" (FT-3): 포커스된 pane의 세션에 WriteInput —
                // 파일 트리의 유일한 runtime 접점 (§6).
                Some(ui::file_tree::SidebarAction::InsertPath(path)) => {
                    let session = self.active.workspace_ui.mux().and_then(|mux| {
                        mux.focused_pane.as_ref().and_then(|focused| {
                            mux.tabs
                                .iter()
                                .flat_map(|tab| &tab.panes)
                                .find(|pane| &pane.id == focused)
                                .and_then(|pane| pane.session_id)
                        })
                    });
                    match session {
                        Some(session) => {
                            let bracketed =
                                self.active.workspace_ui.session_bracketed_paste(session);
                            let shell_kind = self.active.workspace_ui.session_shell_kind(session);
                            let bytes = ui::workspace::path_insert_paste_bytes(
                                &path, shell_kind, bracketed,
                            );
                            self.stage_workspace_controller_action(
                                WorkspaceControllerAction::Runtime(
                                    runtime::RuntimeCommand::WriteInput { session, bytes },
                                ),
                            );
                        }
                        None => tracing::info!("경로 삽입: 활성 터미널 세션 없음 — 무시"),
                    }
                }
                Some(ui::file_tree::SidebarAction::CdPath(path)) => {
                    // 포커스된 터미널에서 이 폴더로 cd 실행 (InsertPath와 같은 세션 해석).
                    let session = self.active.workspace_ui.mux().and_then(|mux| {
                        mux.focused_pane.as_ref().and_then(|focused| {
                            mux.tabs
                                .iter()
                                .flat_map(|tab| &tab.panes)
                                .find(|pane| &pane.id == focused)
                                .and_then(|pane| pane.session_id)
                        })
                    });
                    match session {
                        Some(session) => {
                            let bracketed =
                                self.active.workspace_ui.session_bracketed_paste(session);
                            let shell_kind = self.active.workspace_ui.session_shell_kind(session);
                            let bytes = ui::workspace::cd_paste_bytes(&path, shell_kind, bracketed);
                            self.stage_workspace_controller_action(
                                WorkspaceControllerAction::Runtime(
                                    runtime::RuntimeCommand::WriteInput { session, bytes },
                                ),
                            );
                        }
                        None => tracing::info!("cd: 활성 터미널 세션 없음 — 무시"),
                    }
                }
                // 세션 목록 클릭 — 해당 tab/pane으로 전환 (workspace 사이드바)
                Some(ui::file_tree::SidebarAction::FocusSession {
                    workspace_id,
                    tab,
                    pane,
                }) => {
                    self.agent_terminal_ui
                        .set_view(ui::agent_terminal::AgentTerminalView::Terminal);
                    self.stage_workspace_controller_action(
                        WorkspaceControllerAction::FocusSession {
                            workspace_id,
                            tab,
                            pane,
                        },
                    );
                }
                // 사이드바 + 버튼 — 새 셸 (탭바 제거 후 대체 진입점)
                // 세션 이름 변경 — pane 제목 갱신(mux 반영 + 영속).
                Some(ui::file_tree::SidebarAction::RenameSession { pane, title }) => {
                    self.stage_workspace_controller_action(WorkspaceControllerAction::Runtime(
                        runtime::RuntimeCommand::RenamePane { pane, title },
                    ));
                }
                // 세션 폴더 열기/경로 복사 — cwd는 감지 캐시 우선, 없으면 일회성 lsof.
                Some(ui::file_tree::SidebarAction::OpenSessionFolder { session }) => {
                    if self.pending_app_host_action.is_none()
                        && let Some(cwd) = self.cached_session_cwd(session)
                        && cwd.len() <= APP_HOST_PATH_MAX_BYTES
                        && !cwd.as_bytes().contains(&0)
                    {
                        self.pending_app_host_action =
                            Some(AppHostIoAction::OpenPath(PathBuf::from(cwd)));
                        ui.ctx().request_repaint();
                    }
                }
                Some(ui::file_tree::SidebarAction::CopySessionPath { session }) => {
                    match self.cached_session_cwd(session) {
                        Some(cwd) => ui.ctx().copy_text(cwd),
                        None => tracing::warn!("세션 cwd 미확인 — 경로 복사 생략"),
                    }
                }
                // 변경 보기 — 세션 cwd 레포의 diff 패널(독립 창)을 연다.
                // cwd 미확인이어도 패널은 열어 안내를 표시한다 (조용한 실패 금지).
                // 제목은 인박스와 같은 관례로 해석 — "세션 #2"보다 "SKRT · Claude"가
                // 무엇의 변경분인지 바로 판단된다(2026-07-18 사용자: 가독성 개선 요청).
                Some(ui::file_tree::SidebarAction::ShowDiff { session }) => {
                    let cwd = self.cached_session_cwd(session);
                    let ws_name = self
                        .workspaces
                        .iter()
                        .find(|w| w.id == self.active.id)
                        .map(Self::workspace_display_name);
                    let session_label = self.inbox_session_label(&self.active.id, session);
                    let title = match (ws_name, session_label) {
                        (Some(ws), Some(s)) => format!("{ws} · {s}"),
                        (Some(ws), None) => ws,
                        (None, Some(s)) => s,
                        (None, None) => String::new(),
                    };
                    self.diff_panel_ui.open_for(
                        ui.ctx(),
                        self.active.id.clone(),
                        session,
                        cwd,
                        title,
                    );
                }
                // 새 워크트리 셸 (PR-W) — 백그라운드에서 repo_root → exclude 보장 →
                // worktree add 후, 아래 worktree_rx 폴링부가 그 폴더에서 셸을 연다.
                Some(ui::file_tree::SidebarAction::NewWorktreeCell { session }) => {
                    if self.worktree_rx.is_some() {
                        // 동시 생성은 채널이 1개라 받지 않는다 — 완료 후 다시.
                        tracing::info!("워크트리 생성이 이미 진행 중 — 요청 무시");
                    } else if self.pending_app_controller_action.is_none()
                        && let Some(cwd) = self.cached_session_cwd(session)
                        && cwd.len() <= APP_HOST_PATH_MAX_BYTES
                        && !cwd.as_bytes().contains(&0)
                    {
                        self.pending_app_controller_action =
                            Some(AppControllerAction::CreateWorktree {
                                workspace_id: self.active.id.clone(),
                                cwd,
                            });
                        ui.ctx().request_repaint();
                    }
                }
                // 워크트리 삭제 — dirty/무시 파일 거부와 서브모듈·브랜치 처리는
                // worktree::remove_worktree가 맡는다(조용한 데이터 손실 금지).
                Some(ui::file_tree::SidebarAction::RemoveWorktree { session }) => {
                    if self.worktree_remove_rx.is_some() {
                        tracing::info!("워크트리 삭제가 이미 진행 중 — 요청 무시");
                    } else if self.pending_app_controller_action.is_none()
                        && let Some(cwd) = self.cached_session_cwd(session)
                        && cwd.len() <= APP_HOST_PATH_MAX_BYTES
                        && !cwd.as_bytes().contains(&0)
                    {
                        self.pending_app_controller_action =
                            Some(AppControllerAction::RemoveWorktree {
                                workspace_id: self.active.id.clone(),
                                cwd,
                            });
                        ui.ctx().request_repaint();
                    }
                }
                // 같은 폴더에서 새 셸 — cwd 미확인이면 일반 새 셸로 폴백.
                Some(ui::file_tree::SidebarAction::NewShellSameFolder { session }) => {
                    let cwd = self.cached_session_cwd(session);
                    self.stage_workspace_controller_action(
                        WorkspaceControllerAction::SpawnShellAt { cwd },
                    );
                }
                // 저장된 에이전트 수동 이어가기 — 자동 이어가기 OFF여도 동작한다.
                Some(ui::file_tree::SidebarAction::ResumeAgent {
                    pane,
                    session,
                    title,
                }) => {
                    self.stage_workspace_controller_action(
                        WorkspaceControllerAction::ResumeAgent {
                            pane_key: pane.0,
                            title,
                            session,
                        },
                    );
                }
                Some(ui::file_tree::SidebarAction::ClosePane { pane }) => {
                    self.stage_workspace_controller_action(WorkspaceControllerAction::ClosePane(
                        pane,
                    ));
                }
                Some(ui::file_tree::SidebarAction::CloseWorkspace(workspace_id)) => {
                    // 세션·실행 중 수는 사이드바 행이 그린 것과 같은 원천(summary) —
                    // 다이얼로그 숫자가 방금 본 행 요약과 어긋나지 않는다.
                    match sidebar_workspaces.iter().find(|w| w.id == workspace_id) {
                        Some(entry)
                            if entry.state != ui::file_tree::SidebarWorkspaceState::Idle =>
                        {
                            let s = entry.summary;
                            let total = s.running + s.waiting + s.done + s.error + s.idle;
                            self.ws_close_confirm =
                                Some((workspace_id, entry.name.clone(), total, s.running));
                        }
                        _ => {
                            // 메뉴는 Idle에 안 붙지만 요청 프레임 사이 상태 변화 방어.
                            tracing::info!(
                                workspace = %workspace_id,
                                "워크스페이스 종료 무시 — 닫을 세션/런타임 없음"
                            );
                        }
                    }
                }
                Some(ui::file_tree::SidebarAction::RenameWorkspace(workspace_id)) => {
                    // 편집 초기값은 표시명이 아니라 **별칭 원본**이다(E3 설정 창 관례) —
                    // 별칭이 없으면 빈 버퍼로 시작하고 placeholder가 폴더명을 보여준다.
                    let alias = self
                        .workspaces
                        .iter()
                        .find(|w| w.id == workspace_id)
                        .map(|w| w.name.trim())
                        .filter(|name| !name.is_empty() && *name != "default")
                        .unwrap_or_default()
                        .to_owned();
                    self.ws_rename_edit = Some((workspace_id, alias));
                }
                Some(ui::file_tree::SidebarAction::CreateWorkspaceFromPicker)
                    if self.pending_app_host_action.is_none() =>
                {
                    self.pending_app_host_action = Some(AppHostIoAction::FolderPicker(
                        FolderPickerPurpose::SwitchWorkspace,
                    ));
                    ui.ctx().request_repaint();
                }
                Some(ui::file_tree::SidebarAction::CreateWorkspaceFromPicker) => {}
                None => {}
            }
        }

        // Worktree job completions are drained by logic(); render only emits bounded actions.
        // ── 관리/모니터 패널 부수효과 (매 프레임 — 통합 설정 창 표시 여부와 무관) ──
        let events = std::mem::take(&mut self.active.pending_events);
        let central_view = self.agent_terminal_ui.view();
        let home_visible = central_view == ui::agent_terminal::AgentTerminalView::Home;
        let inbox_visible = central_view == ui::agent_terminal::AgentTerminalView::Inbox;
        let fleet_visible = central_view == ui::agent_terminal::AgentTerminalView::Fleet;
        // 홈/작업함/fleet이 중앙을 차지해도 활성 워크스페이스 이벤트는 계속 소화한다.
        if home_visible || inbox_visible || fleet_visible {
            self.active
                .workspace_ui
                .update_hidden(ui.ctx(), &events, &text);
        }
        // fleet 뷰모델은 Fleet 뷰일 때만 조립한다(active+warm 순회 비용 회피).
        let (fleet_sessions, fleet_summary) = if fleet_visible {
            let sessions = self.build_fleet_sessions(&text);
            let summary = crate::fleet::FleetSummary::from_states(sessions.iter().map(|s| s.state));
            (sessions, summary)
        } else {
            (Vec::new(), crate::fleet::FleetSummary::default())
        };
        // 배치 스폰 패널용 슬림 (id, name) 목록 — leaf가 AgentsSnapshot 내부를 직접 몰라도
        // 되게 App이 매 프레임(Fleet 뷰일 때만) 투영한다. Arc 클론이라 재할당 없음.
        let fleet_batch_spawn_agents: Vec<(Arc<str>, Arc<str>)> = if fleet_visible {
            self.agents_snapshot
                .agents()
                .iter()
                .map(ui::agents::AgentListItem::id_name)
                .collect()
        } else {
            Vec::new()
        };

        // 컴포저는 터미널 표면에만 붙는다. 홈/작업함/fleet은 전체 폭 페이지가 중앙을 쓴다.
        if !home_visible && !inbox_visible && !fleet_visible && self.config.ui.composer_enabled {
            self.render_composer_dock(ui, &text);
        }

        // 작업창은 여백 없이 경계까지 채운다 — CentralPanel 기본 inner_margin(8) 탓에
        // pane 좌/상/우 여백이 보였다(#69 사용자).
        // 작업창 배경은 Target의 tab strip 바탕(#17171c)으로 고정한다. 실제 PTY는
        // WorkspaceUi가 한 단계 더 어두운 inset surface(#0f1117)에 렌더한다.
        let central_frame = egui::Frame::central_panel(&ui.ctx().global_style())
            .inner_margin(egui::Margin::ZERO)
            .fill(egui::Color32::from_rgb(0x17, 0x17, 0x1c));
        let mut home_action = None;
        let mut inbox_page_click = None;
        let mut fleet_action = None;
        egui::CentralPanel::default()
            .frame(central_frame)
            .show(ui, |ui| {
                if home_visible {
                    home_action = self.agent_terminal_ui.home(
                        ui,
                        &self.status_feed,
                        ui::agent_terminal::NoticeTranslations {
                            cache: &self.notice_translation_cache,
                            locale: &self.config.i18n.locale,
                        },
                        &self.connector_snapshot_reader.snapshot().slack,
                        &text,
                    );
                } else if inbox_visible {
                    inbox_page_click = self.render_inbox_page(ui, &text);
                } else if fleet_visible {
                    fleet_action = self.fleet_ui.render(
                        ui,
                        &fleet_sessions,
                        fleet_summary,
                        &text,
                        &self.prompt_library,
                        ui::fleet::BatchSpawnInput {
                            agents: &fleet_batch_spawn_agents,
                            max: self.config.ui.fleet_batch_spawn_max,
                        },
                    );
                } else {
                    self.active
                        .workspace_ui
                        .show(ui, &self.config.terminal, &events, &text);
                }
            });
        if self.active.workspace_ui.take_new_session_requested() {
            self.stage_workspace_controller_action(WorkspaceControllerAction::OpenAgentLauncher);
        }
        // 작업함 페이지에서 세션 점프 — 터미널로 복귀한 뒤 기존 알림 네비게이션 경로
        // (아래 notif_click 합류 지점)에 태운다(사이드바 FocusSession과 같은 규칙).
        if inbox_page_click.is_some() {
            self.agent_terminal_ui
                .set_view(ui::agent_terminal::AgentTerminalView::Terminal);
        }
        // fleet 액션 처리 — 카드 클릭은 세션 포커스, 새 에이전트는 에이전트 패널 열기.
        match fleet_action {
            Some(ui::fleet::FleetAction::Focus {
                workspace_id,
                tab,
                pane,
            }) => {
                // 터미널로 복귀 후 해당 세션 포커스(사이드바 FocusSession과 동일).
                self.agent_terminal_ui
                    .set_view(ui::agent_terminal::AgentTerminalView::Terminal);
                self.stage_workspace_controller_action(WorkspaceControllerAction::FocusSession {
                    workspace_id,
                    tab,
                    pane,
                });
            }
            Some(ui::fleet::FleetAction::LaunchAgent) => {
                // 에이전트 패널을 연다(fleet에 세션을 추가하는 진입점). 패널은 떠 있는
                // 창이라 fleet 뷰 위에서 바로 쓸 수 있다.
                self.agent_sessions_ui.open();
            }
            Some(ui::fleet::FleetAction::OpenStructured { session_id }) => {
                // 구조화 세션 카드 클릭 → 에이전트 패널에서 해당 세션을 연다. 세션이
                // 사라졌으면(open_session=false) 엉뚱한 이전 선택을 띄우지 않는다(리뷰 Low).
                if self.agent_sessions_ui.open_session(&session_id) {
                    self.agent_sessions_ui.open();
                }
            }
            Some(ui::fleet::FleetAction::Broadcast { prompt, targets }) => {
                // 저장된 프롬프트를 선택된 각 실행 중 에이전트에 컴포저 Send와 동일 경로로
                // 주입한다(사용자가 대상·프롬프트를 명시적으로 고른 뒤에만 발행됨).
                let now = std::time::Instant::now();
                // 만료된 낙관적 마커 정리(윈도우보다 넉넉히 — 맵을 작게 유지).
                self.broadcast_working.retain(|_, sent| {
                    now.duration_since(*sent) < std::time::Duration::from_secs(30)
                });
                for (workspace_id, session) in targets {
                    self.broadcast_prompt_to(&workspace_id, session, &prompt);
                    // 방금 프롬프트를 보냈으니 잠깐 "작업 중"으로 낙관적 표시(감지 지연 메움).
                    self.broadcast_working.insert((workspace_id, session), now);
                }
            }
            Some(ui::fleet::FleetAction::BatchSpawn {
                agent_id,
                count,
                prompt,
            }) => {
                // 실제 launch는 다음 logic tick부터 pump_batch_spawn이 settings 잡 큐
                // 1슬롯을 통해 순차로 큐잉한다(PR-S1 빈 세션 / PR-S2 프롬프트 argv 전달).
                // count는 패널이 이미 1..=cap으로 그렸지만 방어적으로 다시 클램프한다.
                let prompt_bytes = prompt.as_deref().map_or(0, str::len);
                if prompt_bytes > FLEET_BATCH_SPAWN_PROMPT_MAX_BYTES {
                    // prepare_agent_launch의 args-byte 상한(64KiB)과는 별개로, 패널이
                    // 비정상적으로 큰 프롬프트를 보내면 settings 잡 큐까지 보내지 않고
                    // 조기에 거부한다(스폰 자체를 하지 않음).
                    self.agents_ui
                        .report_error(ui::agents::AgentsUiErrorCode::TooManyArguments);
                } else {
                    let cap = self.config.ui.fleet_batch_spawn_max.max(1);
                    self.pending_batch_spawn = Some(PendingBatchSpawn {
                        agent_id,
                        remaining: count.clamp(1, cap),
                        staged_workspace_id: self.active.id.clone(),
                        prompt,
                    });
                    ui.ctx().request_repaint();
                }
            }
            None => {}
        }
        // take/put-back 마무리 — 위 take에서 꺼낸 rows를 타임스탬프 그대로 되돌린다.
        self.activity_rows_cache = Some((activity_rows_stamp, activity_rows));
        match home_action {
            Some(ui::agent_terminal::HomeAction::Connectors) => {
                self.settings_category = ui::settings::Category::Connectors;
                self.settings_open = true;
            }
            Some(ui::agent_terminal::HomeAction::RefreshNotices) => {
                // 워커를 즉시 깨워 상태+공지 강제 재조회 — 결과는 기존 스냅샷
                // 채널로 돌아온다(추가 상태 불필요).
                let _ = self.status_feed_refresh.send(());
            }
            None => {}
        }
        if self.active.workspace_ui.take_terminal_focus_claimed() {
            self.agent_sessions_ui.surrender_text_focus(ui.ctx());
        }
        // logic에서 workspace/path 변경 때만 검증한 immutable cwd projection을 넘긴다.
        let agent_workspace_cwd = self.agent_workspace_cwd.clone();
        let active_workspace_id = self.active.id.clone();
        // Agents 창의 LLM 프로바이더 설정은 config 소유(App) — 창이 바꾸면 저장한다 (PR-L2).
        let agents_config_before = self.config.agents.clone();
        self.agent_sessions_ui.set_catalog(&text);
        let agent_output = self.agent_sessions_ui.show(
            ui.ctx(),
            ui::agent_sessions::AgentSessionsFrameInput {
                workspace_id: &active_workspace_id,
                workspace_cwd: agent_workspace_cwd,
                pty_surfaces: pty_agent_surfaces,
                agents_config: &mut self.config.agents,
                secrets_snapshot: &self.agent_sessions_secrets_snapshot,
                ollama_models: self.ollama_models.as_deref(),
            },
        );
        if self.config.agents != agents_config_before {
            self.pending_config_save = true;
            ui.ctx().request_repaint();
        }
        for request in agent_output.requests {
            self.handle_agent_sessions_request(request);
        }
        if let Some(action) = agent_output.deferred_action {
            // logic()이 매 render 전에 기존 slot을 비운다. 예외적으로 남아 있어도 latest-only
            // 교체해 backlog를 만들지 않는다; generation mismatch가 stale 실행을 막는다.
            self.pending_agent_sessions_action = Some(action);
        }
        if let Some(intent) = agent_output.secret_intent {
            let current_revision = self.agent_sessions_secrets_snapshot.revision();
            let action = match intent {
                ui::agent_sessions::AgentSessionsSecretIntent::SaveApiKey { revision, input }
                    if revision == current_revision =>
                {
                    Some(SettingsJobAction::SaveCodexLlmApiKey {
                        value: secret::SecretString::new(input.into_inner()),
                    })
                }
                ui::agent_sessions::AgentSessionsSecretIntent::DeleteApiKey { revision }
                    if revision == current_revision =>
                {
                    Some(SettingsJobAction::DeleteCodexLlmApiKey)
                }
                _ => {
                    self.agent_sessions_ui.report_api_key_error(
                        ui::agent_sessions::AgentSessionsSecretErrorCode::SnapshotUnavailable,
                    );
                    None
                }
            };
            if let Some(action) = action
                && !self.queue_global_settings_action(&active_workspace_id, action)
            {
                self.agent_sessions_ui.report_api_key_error(
                    ui::agent_sessions::AgentSessionsSecretErrorCode::SnapshotUnavailable,
                );
            }
        }
        // 「변경 보기」 diff 패널 — 매 프레임 렌더 + 백그라운드 수집 결과 poll.
        self.diff_panel_ui.show(ui.ctx(), &text);
        // pane 우클릭 → 환경변수·API 설정 (E4 ⑥) — 프로젝트 화면에서 바로 진입.
        if self.active.workspace_ui.take_open_environment() {
            self.settings_category = ui::settings::Category::Environment;
            self.settings_open = true;
            // T1: 우클릭 진입 시에만 focused 세션 cwd를 감지 — env 페이지 상단에
            // "새 프로젝트로 등록"/"이 폴더를 프로젝트 폴더로 지정" 배너를 띄운다.
            // App-owned workspace projection과 비교하므로 render-time DB refresh가 없다.
            if self.pending_app_controller_action.is_none()
                && let Some(cwd) = self.focused_session_cwd()
                && cwd.len() <= APP_HOST_PATH_MAX_BYTES
                && !cwd.as_bytes().contains(&0)
            {
                self.pending_app_controller_action =
                    Some(AppControllerAction::DetectEnvSessionBanner { cwd });
                ui.ctx().request_repaint();
            }
        }
        // pane 우클릭 → 세션 폴더 동선 (2026-07-18): 파일 트리 이동은 사이드바 트리의
        // set_root(브레드크럼·'..'과 같은 탐색 메커니즘), Finder는 사이드바
        // OpenSessionFolder와 같은 경로. cwd 미확인 시 경고 로그(사이드바 관례).
        match self.active.workspace_ui.take_session_folder_request() {
            Some(ui::workspace::SessionFolderRequest::RevealInTree(session)) => {
                match self.cached_session_cwd(session) {
                    Some(cwd) => match self.file_tree.as_mut() {
                        Some(tree) => tree.set_root(Some(std::path::PathBuf::from(cwd))),
                        None => tracing::info!("파일 트리 OFF — 트리 이동 생략"),
                    },
                    None => tracing::warn!("세션 cwd 미확인 — 트리 이동 생략"),
                }
            }
            Some(ui::workspace::SessionFolderRequest::OpenInFinder(session)) => {
                if self.pending_app_host_action.is_none()
                    && let Some(cwd) = self.cached_session_cwd(session)
                    && cwd.len() <= APP_HOST_PATH_MAX_BYTES
                    && !cwd.as_bytes().contains(&0)
                {
                    self.pending_app_host_action =
                        Some(AppHostIoAction::OpenPath(PathBuf::from(cwd)));
                    ui.ctx().request_repaint();
                }
            }
            None => {}
        }

        // 알림 센터 렌더 (생성은 logic()에서 끝났다). 활성 workspace의 사라진 세션의
        // 진행형 알림 정리 (다른 workspace 건 alive를 알 수 없어 유지).
        let mux = self.active.workspace_ui.mux().cloned();
        if let Some(mux) = &mux {
            let alive: Vec<_> = mux
                .tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .filter_map(|pane| pane.session_id)
                .collect();
            self.notifications_ui
                .retain_sessions(&self.active.id, &alive);
            // retain_sessions가 배지 그리기 이후 unread를 줄였다면 다음 프레임에 재반영
            if self.notifications_ui.unread() != unread_before {
                ui.ctx().request_repaint();
            }
        }
        // 알림 클릭으로 예약된 runtime focus는 다음 logic tick에서만 실행한다.

        // agent-proxy 승인은 벨 팝오버의 「대기 중」 섹션이 처리한다 (v3.9 N4) — 화면 중앙
        // 모달은 **표시하지 않는다**.
        //
        // 모달을 접는 이유: 인박스는 전역(다른 워크스페이스 것 포함) 대기를 한 곳에서
        // 보여주고 그 자리에서 처리하는데, 모달은 큐의 맨 앞 1건만 강제로 띄워 작업을
        // 가로챈다 — 같은 일을 두 곳에서 다르게 하는 셈이다. "무시할 수 없게 알린다"는
        // 모달의 역할은 벨 뱃지 + 기존 OS 알림이 대신한다.
        //
        // `ApprovalsUi`(모달 위젯)와 `show()`는 **의도적으로 남겨 둔다** — 인박스를 써 보고
        // 강제 팝업이 필요하다고 판단되면 이 호출 한 줄을 되살리면 된다. 상태(pending
        // 목록)는 인박스 카드의 소스로 계속 쓰이므로 set_pending 폴링은 그대로다.

        if self.runtime_stream_warning {
            let mut close = false;
            egui::Window::new(text.t("runtime.event_overflow.title", &[]))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.label(text.t("runtime.event_overflow.body", &[]));
                    ui.add_space(8.0);
                    if ui.button(text.t("action.close", &[])).clicked() {
                        close = true;
                    }
                });
            if close {
                self.runtime_stream_warning = false;
            }
        }

        if let Some(target) = self.warm_limit_warning.clone() {
            let mut close = false;
            egui::Window::new(text.t("workspace.warm_limit.title", &[]))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.label(text.t(
                        "workspace.warm_limit.body",
                        &[
                            ("target", &target),
                            ("limit", &self.config.performance.max_live_warm.to_string()),
                        ],
                    ));
                    ui.add_space(8.0);
                    if ui.button(text.t("action.close", &[])).clicked() {
                        close = true;
                    }
                });
            if close {
                self.warm_limit_warning = None;
            }
        }

        // 「워크스페이스 종료」 확인 모달 — 실행 중 에이전트를 죽일 수 있어 반드시
        // 확인을 거친다(pane 닫기 confirm_close와 같은 중앙 egui::Window 관례).
        if let Some((close_id, close_name, total, running)) = self.ws_close_confirm.clone() {
            let mut decision: Option<bool> = None; // Some(true)=모두 종료, Some(false)=취소
            egui::Window::new(text.t("workspace.close_ws_confirm.title", &[]))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.label(text.t(
                        "workspace.close_ws_confirm.body",
                        &[
                            ("name", close_name.as_str()),
                            ("count", &total.to_string()),
                            ("running", &running.to_string()),
                        ],
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui
                            .button(text.t("workspace.close_ws_confirm.confirm", &[]))
                            .clicked()
                        {
                            decision = Some(true);
                        }
                        if ui.button(text.t("action.cancel", &[])).clicked() {
                            decision = Some(false);
                        }
                    });
                });
            match decision {
                Some(true) => {
                    self.ws_close_confirm = None;
                    self.stage_workspace_controller_action(
                        WorkspaceControllerAction::CloseWorkspace(close_id),
                    );
                }
                Some(false) => self.ws_close_confirm = None,
                None => {}
            }
        }

        // 사이드바 「이름 바꾸기」 모달 — 별칭(name 컬럼)만 편집하고 실제 폴더/경로는
        // 불변. Enter/저장 = 확정, Esc/취소 = 폐기. 빈 값 확정 = 별칭 해제(폴더명 복귀).
        if let Some((rename_id, mut buf)) = self.ws_rename_edit.take() {
            // 대상이 사라졌으면(프레임 사이 삭제) 모달을 접는다 — take()가 이미 닫았다.
            if let Some(row) = self.workspaces.iter().find(|w| w.id == rename_id) {
                let folder_hint = {
                    let path = row.path.trim();
                    (!path.is_empty())
                        .then(|| std::path::Path::new(path).file_name())
                        .flatten()
                        .map(|base| base.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "~".to_owned())
                };
                let current_alias = {
                    let name = row.name.trim();
                    if name == "default" { "" } else { name }.to_owned()
                };
                let mut decision: Option<bool> = None; // Some(true)=저장, Some(false)=취소
                egui::Window::new(text.t("workspace.rename_ws.title", &[]))
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ui.ctx(), |ui| {
                        ui.label(text.t("workspace.rename_ws.body", &[("folder", &folder_hint)]));
                        ui.add_space(4.0);
                        let edit = ui.add(
                            egui::TextEdit::singleline(&mut buf)
                                .hint_text(folder_hint.as_str())
                                .desired_width(240.0),
                        );
                        // 세션 인라인 편집과 같은 관례 — 키가 터미널로 새지 않게 포커스 고정.
                        edit.request_focus();
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui
                                .button(text.t("workspace.rename_ws.confirm", &[]))
                                .clicked()
                            {
                                decision = Some(true);
                            }
                            if ui.button(text.t("action.cancel", &[])).clicked() {
                                decision = Some(false);
                            }
                        });
                        let (enter, esc) = ui.input(|i| {
                            (
                                i.key_pressed(egui::Key::Enter),
                                i.key_pressed(egui::Key::Escape),
                            )
                        });
                        if enter {
                            decision = Some(true);
                        } else if esc {
                            decision = Some(false);
                        }
                    });
                match decision {
                    Some(true) => {
                        let next = buf.trim();
                        // 별칭이 그대로면 DB 쓰기 생략(churn 방지). 실패는 로그 + OS 알림
                        // 으로 표면화한다 — 조용한 실패 금지.
                        if next != current_alias
                            && !self.queue_global_settings_action(
                                &rename_id,
                                SettingsJobAction::RenameWorkspace {
                                    name: next.to_owned(),
                                },
                            )
                        {
                            let summary = text.t("workspace.rename_ws.failed", &[]);
                            let body = "workspace settings worker unavailable".to_owned();
                            if summary.len() <= APP_NOTICE_TEXT_MAX_BYTES
                                && body.len() <= APP_NOTICE_TEXT_MAX_BYTES
                                && !summary.contains('\0')
                            {
                                self.stage_workspace_controller_action(
                                    WorkspaceControllerAction::Notify { summary, body },
                                );
                            }
                        }
                    }
                    Some(false) => {}
                    None => self.ws_rename_edit = Some((rename_id, buf)),
                }
            }
        }

        // 프로젝트 폴더 rename/이동 감지 → 복구 확인 모달 (사용자 요청 2026-07-08).
        if let Some((old, new)) = self.workspace_rename_prompt.clone() {
            let mut decision: Option<bool> = None; // Some(true)=갱신, Some(false)=무시
            egui::Window::new(text.t("workspace.folder_moved.title", &[]))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.label(text.t("workspace.folder_moved.body", &[]));
                    ui.add_space(4.0);
                    ui.label(text.t("workspace.folder_moved.from", &[("path", &old)]));
                    ui.label(text.t("workspace.folder_moved.to", &[("path", &new)]));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui
                            .button(text.t("workspace.folder_moved.update", &[]))
                            .clicked()
                        {
                            decision = Some(true);
                        }
                        if ui
                            .button(text.t("workspace.folder_moved.ignore", &[]))
                            .clicked()
                        {
                            decision = Some(false);
                        }
                    });
                });
            match decision {
                Some(true) => {
                    let workspace_id = self.active.id.clone();
                    let queued = self
                        .workspace_anchors
                        .get(&workspace_id)
                        .copied()
                        .is_some_and(|expected_anchor| {
                            self.queue_global_settings_action(
                                &workspace_id,
                                SettingsJobAction::AcceptMovedWorkspacePath {
                                    expected_old_path: old.clone(),
                                    expected_anchor,
                                    new_path: std::path::PathBuf::from(&new),
                                },
                            )
                        });
                    if !queued {
                        tracing::info!("폴더 이동 프롬프트 stale 또는 worker unavailable");
                        self.workspace_rename_prompt = None;
                    }
                }
                Some(false) => {
                    self.dismissed_renames.insert(self.active.id.clone());
                    self.workspace_rename_prompt = None;
                }
                None => {}
            }
        }

        // A prior infrastructure failure retries only on an explicit closed→open Settings edge;
        // there is no TTL or frame retry. This mutation only invalidates bounded memory state.
        if !self.settings_was_open && self.settings_open && self.env_project_rows_failed {
            self.invalidate_env_api_projects();
        }
        // logic() owns worker admission/result application. Render clones only the immutable Arc;
        // the first Environment frame may be empty until its edge-triggered result arrives.
        let env_api_projects_snapshot = if self.settings_open {
            self.env_api_projects_cache.as_ref().map(Arc::clone)
        } else {
            None
        };
        let env_api_projects = env_api_projects_snapshot.as_deref().unwrap_or(&[]);
        // 설정창 닫힘 전이 — env secret 평문 캐시를 메모리에서 정리(codex Med:
        // 기본 노출로 상주하는 평문의 수명을 설정창 열림 동안으로 한정). remote_view가
        // self 일부를 immutable 차용하기 전에 처리한다.
        if self.settings_was_open && !self.settings_open {
            self.invalidate_env_profile_ui();
        }
        self.settings_was_open = self.settings_open;
        // Settings snapshot worker 요청은 아래 RemoteView가 self 일부를 빌리기 전에 끝낸다.
        // render closure는 이 시점에 준비된 immutable snapshot만 읽는다.
        if self.settings_open
            && matches!(
                self.settings_category,
                ui::settings::Category::Environment | ui::settings::Category::Agents
            )
        {
            let active_id = self.active.id.clone();
            let selected = if self.settings_category == ui::settings::Category::Environment {
                resolve_settings_env_project_id(
                    env_api_projects,
                    None,
                    self.settings_workspace_id.as_deref(),
                    &active_id,
                )
                .unwrap_or(active_id)
            } else {
                resolve_settings_workspace_id(
                    &self.workspaces,
                    None,
                    self.settings_workspace_id.as_deref(),
                    &active_id,
                )
                .unwrap_or(active_id)
            };
            let project_root = self.workspace_tree_root(&selected);
            self.request_settings_snapshot_if_needed(&selected, project_root);
        }
        // Remote 뷰모델을 현재 상태에서 구성 (UI는 서버를 직접 만지지 않는다 — disjoint 필드 차용).
        let remote_view = {
            let (running, addr, fp, token) = match &self.remote {
                Some(s) => (
                    true,
                    Some(s.server.local_addr().to_string()),
                    Some(s.fingerprint.as_str()),
                    Some(s.server.auth_token()),
                ),
                None => (false, None, None, None),
            };
            ui::settings::RemoteView {
                running,
                addr,
                fingerprint: fp,
                token,
                error: self.remote_error.as_deref(),
                known_hosts_path: self.known_hosts_path().display().to_string(),
                known_hosts: self.known_hosts_cache.as_deref().unwrap_or(&[]),
            }
        };
        // 모바일 웹(PWA) 뷰모델 — remote_view와 동일 규칙 (v3.3 P1).
        let web_view = {
            let (running, addr, url) = match &self.web {
                Some(state) => {
                    let addr = state.server.local_addr();
                    let hostname = self.config.web.ts_hostname.trim();
                    let url = web_remote::pairing::access_url(
                        (!hostname.is_empty()).then_some(hostname),
                        addr.port(),
                        &state.token,
                    );
                    (true, Some(addr.to_string()), Some(url))
                }
                None => (false, None, None),
            };
            ui::settings::WebRemoteView {
                running,
                addr,
                url,
                error: self.web_error.as_deref(),
                serve: if self.serve_rx.is_some() {
                    ui::settings::ServeView::Running
                } else {
                    match &self.serve_state {
                        None => ui::settings::ServeView::Idle,
                        Some(crate::tailscale::ServeState::Ready) => ui::settings::ServeView::Ready,
                        Some(crate::tailscale::ServeState::WrongPort(p)) => {
                            ui::settings::ServeView::WrongPort(*p)
                        }
                        Some(crate::tailscale::ServeState::NotConfigured) => {
                            ui::settings::ServeView::NotConfigured
                        }
                        Some(crate::tailscale::ServeState::NotEnabledOnTailnet { approve_url }) => {
                            ui::settings::ServeView::NotEnabled {
                                approve_url: approve_url.as_deref(),
                            }
                        }
                        Some(crate::tailscale::ServeState::Unknown) => {
                            ui::settings::ServeView::Unknown
                        }
                    }
                },
                ts_detect: if self.ts_detect_rx.is_some() {
                    ui::settings::TsDetectView::Running
                } else {
                    match &self.ts_detected {
                        None => ui::settings::TsDetectView::Idle,
                        Some(crate::tailscale::Detected::Hostname(host)) => {
                            ui::settings::TsDetectView::Found(host)
                        }
                        Some(crate::tailscale::Detected::NoHostname) => {
                            ui::settings::TsDetectView::NoHostname
                        }
                        Some(crate::tailscale::Detected::CliNotFound) => {
                            ui::settings::TsDetectView::NoCli
                        }
                    }
                },
            }
        };
        // 알림 카테고리를 보고 있으면 읽음 처리 (기존 notifications.show가 하던 것).
        if self.settings_open
            && self.settings_category == ui::settings::Category::Notifications
            && self.notifications_ui.mark_all_read()
        {
            ui.ctx().request_repaint();
        }
        let notif_unread = self.notifications_ui.unread() as u32;
        // 통합 설정 창: 설정 5개는 settings::show가 인라인, 관리/모니터 7개는 아래
        // render_management 클로저가 각 패널 contents()를 렌더한다 (전체 통합, 2026-07-06).
        // config는 &mut로 넘기므로 클로저는 config 대신 미리 클론한 값을 쓴다 (borrow 분리).
        // 설정창이 열려 있을 때만 조립 — 닫힌 평상시 프레임 비용 0(codex Low). 카테고리까지
        // 조건에 넣으면 탭 전환 프레임에 빈 행이 한 프레임 번쩍이므로(category가 show() 안에서
        // 갱신) 창 열림만 본다.
        let activity_rows = if self.settings_open {
            self.activity_rows_cache
                .as_ref()
                .map(|(_, snapshot)| snapshot.clone())
                .unwrap_or_default()
        } else {
            ui::activity::ActivitySnapshot::empty()
        };
        let wsid = self.active.id.clone();
        let is_environment = self.settings_category == ui::settings::Category::Environment;
        let settings_env_wsid = is_environment
            .then(|| {
                resolve_settings_env_project_id(
                    env_api_projects,
                    None,
                    self.settings_workspace_id.as_deref(),
                    &wsid,
                )
            })
            .flatten();
        let settings_wsid = if is_environment {
            settings_env_wsid.clone()
        } else {
            resolve_settings_workspace_id(
                &self.workspaces,
                None,
                self.settings_workspace_id.as_deref(),
                &wsid,
            )
        }
        .unwrap_or_else(|| wsid.clone());
        if self.settings_open {
            let next_settings_id = if is_environment {
                settings_env_wsid.clone()
            } else {
                Some(settings_wsid.clone())
            };
            if self.settings_workspace_id != next_settings_id {
                self.settings_workspace_id = next_settings_id;
            }
        }
        let settings_env_api_project = settings_env_wsid.as_deref().and_then(|settings_id| {
            env_api_projects
                .iter()
                .find(|project| project.id == settings_id)
                .cloned()
        });
        // 환경변수 편집 게이트(E1 ⑤): 프로젝트 폴더가 지정된 워크스페이스만 .env 편집 허용.
        let env_project_root = settings_env_api_project.as_ref().and_then(|project| {
            (!project.path.trim().is_empty() && !project.path_missing)
                .then(|| PathBuf::from(&project.path))
        });
        let env_project_rows_loading =
            self.env_api_projects_cache.is_none() && self.env_project_rows_in_flight.is_some();
        let env_project_rows_failed = self.env_project_rows_failed;
        let mut activity_action = None;
        let mut notif_click = None;
        let mut settings_workspace_select: Option<String> = None;
        let mut settings_ws_create: Option<PathBuf> = None;
        let mut settings_folder_picker_requested = false;
        // Environment 프로젝트 닫기 확인 결정 — 설정 목록 숨김만 클로저 밖에서 처리한다.
        let mut env_project_close_decision: Option<bool> = None;
        let mut workspace_rename: Option<String> = None;
        let mut env_action: Option<ui::env_profiles::EnvAction> = None;
        let mut agents_intent: Option<ui::agents::AgentsIntent> = None;
        let mut credentials_intent: Option<ui::credentials::CredentialsIntent> = None;
        let mut connector_intent: Option<connector_contract::ConnectorIntent> = None;
        // .env 라이브 반영 토글(E5 ⑨) — 클로저 안에서 편집하고 밖에서 저장/적용.
        let mut env_live_reload_toggle = self.config.ui.env_live_reload;
        // #3 워크스페이스 이름 편집 캡처 (클로저 밖에서 db/refresh 처리 — self 전체 &mut).
        let out = ui::settings::show(
            ui.ctx(),
            &mut self.settings_open,
            &mut self.settings_category,
            &mut self.config,
            &remote_view,
            &mut self.remote_reveal_token,
            &web_view,
            &mut self.web_reveal_url,
            &mut self.web_qr,
            notif_unread,
            &mut self.settings_search,
            &text,
            |ui, cat| {
                use ui::settings::Category as C;
                match cat {
                    // C::Credentials는 settings.rs가 Environment로 리다이렉트 — 분기 불필요
                    // (자격증명 UI는 Environment 뷰의 API 키 섹션으로 통합, 2026-07-09).
                    C::Connectors => {
                        connector_intent = self
                            .connector_ui
                            .render(ui, self.connector_snapshot_reader.snapshot());
                    }
                    C::Environment => {
                        // 이 화면은 참조 목업처럼 전용 monospace grid를 사용한다.
                        let style = ui.style_mut();
                        style
                            .text_styles
                            .insert(egui::TextStyle::Body, egui::FontId::monospace(14.0));
                        style
                            .text_styles
                            .insert(egui::TextStyle::Button, egui::FontId::monospace(13.0));
                        style
                            .text_styles
                            .insert(egui::TextStyle::Small, egui::FontId::monospace(12.0));
                        // 상세 surface=#242424, 프로젝트 rail은 renderer가 #1e1e1e로 덮는다.
                        ui.painter()
                            .rect_filled(ui.clip_rect(), 0.0, ui.visuals().panel_fill);
                        // T1: 우클릭 진입 시 감지한 세션 폴더 배너 — cwd가 어떤 워크스페이스에도
                        // 속하지 않으면 새 프로젝트 등록, 활성 워크스페이스가 경로 미설정이면
                        // 이 폴더 지정 CTA. 클릭 시 기존 ws_create/SetProjectPath 흐름 재사용.
                        let mut banner_used = false;
                        if settings_env_wsid.as_deref() == Some(wsid.as_str())
                            && let Some(banner) = &self.env_session_banner
                        {
                            let show_register = !banner.registered;
                            let show_set_path = env_project_root.is_none();
                            if show_register || show_set_path {
                                egui::Frame::NONE
                                    .inner_margin(egui::Margin::symmetric(14, 8))
                                    .show(ui, |ui| {
                                        ui.horizontal_wrapped(|ui| {
                                            let display =
                                                ui::env_project_list::display_project_path(
                                                    &banner.cwd.to_string_lossy(),
                                                );
                                            ui.label(text.t(
                                                "env.session_cwd.detected",
                                                &[("path", &display)],
                                            ));
                                            if show_register
                                                && ui
                                                    .button(text.t("env.session_cwd.register", &[]))
                                                    .clicked()
                                            {
                                                settings_ws_create = Some(banner.cwd.clone());
                                                banner_used = true;
                                            }
                                            if show_set_path
                                                && ui
                                                    .button(
                                                        text.t("env.session_cwd.set_project", &[]),
                                                    )
                                                    .clicked()
                                            {
                                                env_action = Some(
                                                    ui::env_profiles::EnvAction::SetProjectPath(
                                                        banner.cwd.clone(),
                                                    ),
                                                );
                                                banner_used = true;
                                            }
                                        });
                                    });
                                ui.separator();
                            }
                        }
                        if banner_used {
                            self.env_session_banner = None;
                        }
                        // 전체 가용 높이를 **먼저** 캡처해 좌측 리스트/우측 스크롤에 강제한다
                        // — horizontal 안에서 available_height가 줄어 리스트가 수십 px로
                        // 잘리던 회귀 방지(2026-07-09 스크린샷).
                        let full_h = ui.available_height();
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            ui.set_min_height(full_h);
                            let project_list_style =
                                ui::env_project_list::EnvProjectListStyle::for_available_width(
                                    ui.available_width(),
                                );
                            match ui::env_project_list::render_with_style(
                                ui,
                                env_api_projects,
                                settings_env_wsid.as_deref().unwrap_or(""),
                                &text,
                                &project_list_style,
                            ) {
                                ui::env_project_list::EnvProjectListAction::None => {}
                                ui::env_project_list::EnvProjectListAction::Select(id) => {
                                    settings_workspace_select = Some(id);
                                }
                                ui::env_project_list::EnvProjectListAction::AddRequested => {
                                    settings_folder_picker_requested = true;
                                }
                                ui::env_project_list::EnvProjectListAction::CloseRequested(id) => {
                                    // Environment 목록에서만 닫는 확인 모달. workspace DB,
                                    // sidebar 표시 상태, runtime/session은 건드리지 않는다.
                                    let name = env_api_projects
                                        .iter()
                                        .find(|p| p.id == id)
                                        .map(|p| p.name.clone())
                                        .unwrap_or_default();
                                    self.env_project_close_confirm = Some((id, name));
                                }
                            }
                            // 리스트/상세 경계 — separator(6px 스트립)는 우측에 배경
                            // 띠를 남겼다(codex Low) → 1px vline으로 대체.
                            {
                                let h = ui.available_height();
                                let (r, _) = ui
                                    .allocate_exact_size(egui::vec2(1.0, h), egui::Sense::hover());
                                ui.painter().vline(
                                    r.center().x,
                                    r.y_range(),
                                    egui::Stroke::new(
                                        1.0,
                                        ui.visuals().widgets.noninteractive.bg_stroke.color,
                                    ),
                                );
                            }
                            // 우측은 React의 flex column과 동일: 68px 고정 헤더 + body만 scroll.
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                ui.set_min_height(full_h);
                                ui.set_width(ui.available_width());
                                if settings_env_api_project.is_none() {
                                    return;
                                }
                                render_env_api_project_header(
                                    ui,
                                    settings_env_api_project.as_ref(),
                                    &mut env_action,
                                    &mut workspace_rename,
                                    &mut self.env_api_project_edit,
                                    &text,
                                );
                                egui::ScrollArea::vertical()
                                    .id_salt("env_api_detail_scroll")
                                    .auto_shrink([false, false])
                                    .max_height((full_h - 68.0).max(0.0))
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        egui::Frame::NONE
                                            .inner_margin(egui::Margin {
                                                left: 14,
                                                right: 14,
                                                top: 0,
                                                bottom: 12,
                                            })
                                            .show(ui, |ui| {
                                                if env_project_rows_loading {
                                                    ui.horizontal(|ui| {
                                                        ui.add(egui::Spinner::new().size(12.0));
                                                        ui.weak(
                                                            text.t("env.background.loading", &[]),
                                                        );
                                                    });
                                                } else if env_project_rows_failed {
                                                    ui.colored_label(
                                                        ui.visuals().error_fg_color,
                                                        text.t("env.background.load_failed", &[]),
                                                    );
                                                }

                                                if let Some(action) =
                                                    self.env_profiles_ui.contents_compact(
                                                        ui,
                                                        &self.env_profiles_snapshot,
                                                        &text,
                                                    )
                                                {
                                                    env_action = Some(action);
                                                }

                                                credentials_intent =
                                                    self.credentials_ui.contents_compact(
                                                        ui,
                                                        &self.credentials_snapshot,
                                                        &text,
                                                    );

                                                // .env 라이브 반영 토글(E5 ⑨ — 옵트인).
                                                ui.add_space(14.0);
                                                ui.checkbox(
                                                    &mut env_live_reload_toggle,
                                                    text.t("env.live_reload", &[]),
                                                )
                                                .on_hover_text(text.t("env.live_reload_hint", &[]));
                                            });
                                    });
                            });
                        });
                        // Environment 목록 닫기 확인 모달 — 결정만 캡처하고 설정 전용
                        // 숨김 상태 반영은 클로저 밖에서 처리한다.
                        if let Some((_, project_name)) = self.env_project_close_confirm.clone() {
                            egui::Window::new(text.t("env.project_close_confirm.title", &[]))
                                .collapsible(false)
                                .resizable(false)
                                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                                .show(ui.ctx(), |ui| {
                                    ui.label(text.t(
                                        "env.project_close_confirm.body",
                                        &[("name", &project_name)],
                                    ));
                                    ui.add_space(8.0);
                                    ui.horizontal(|ui| {
                                        if ui.button(text.t("action.close", &[])).clicked() {
                                            env_project_close_decision = Some(true);
                                        }
                                        if ui.button(text.t("action.cancel", &[])).clicked() {
                                            env_project_close_decision = Some(false);
                                        }
                                    });
                                });
                        }
                    }
                    C::Agents => {
                        agents_intent = self.agents_ui.contents(ui, &self.agents_snapshot, &text);
                    }
                    C::Workspaces => {
                        // 설정 전용 선택 목록. 여기서 workspace를 눌러도 sidebar 숨김 상태,
                        // active runtime, terminal focus는 바꾸지 않는다.
                        // 이름 지정(이름 변경) 기능은 제거(2026-07-08 사용자) — 워크스페이스
                        // 이름은 항상 프로젝트 폴더명(경로 미설정이면 "~"). 세부 구분은
                        // 세션(pane) 이름 직접 수정으로 한다.
                        if ui
                            .button(text.t("workspace.manager.new", &[]))
                            .on_hover_text(text.t("workspace.manager.new_hint", &[]))
                            .clicked()
                        {
                            settings_folder_picker_requested = true;
                        }
                        ui.add_space(6.0);
                        for ws in &self.workspaces {
                            let display = Self::workspace_display_name(ws);
                            if ui
                                .selectable_label(ws.id == settings_wsid, display)
                                .clicked()
                            {
                                settings_workspace_select = Some(ws.id.clone());
                            }
                        }
                    }
                    C::Activity => {
                        activity_action = self.activity_ui.contents(ui, &text, &activity_rows);
                    }
                    C::Notifications => {
                        notif_click = self.notifications_ui.contents(ui, &text);
                    }
                    _ => {}
                }
            },
        );
        if let Some(intent) = connector_intent {
            let subject = if matches!(
                intent,
                connector_contract::ConnectorIntent::InvokeTool { .. }
            ) {
                connector_service::InvocationContext::for_workspace(wsid.clone())
                    .ok()
                    .map(Some)
            } else {
                Some(None)
            };
            if self.pending_connector_dispatch.is_none()
                && let Some(subject) = subject
            {
                self.pending_connector_dispatch = Some((intent, subject));
                ui.ctx().request_repaint();
            }
        }
        // T1: 설정 창이 닫히면 세션 폴더 배너를 버린다 — 다음 우클릭 진입에서 재감지.
        if !self.settings_open {
            self.env_session_banner = None;
        }
        if settings_folder_picker_requested && self.pending_app_host_action.is_none() {
            self.pending_app_host_action = Some(AppHostIoAction::FolderPicker(
                FolderPickerPurpose::SelectWorkspaceInSettings,
            ));
            ui.ctx().request_repaint();
        }
        if let Some(path) = settings_ws_create
            && path.as_os_str().as_encoded_bytes().len() <= APP_HOST_PATH_MAX_BYTES
        {
            self.pending_folder_picker_completion =
                Some((FolderPickerPurpose::SelectWorkspaceInSettings, path));
            ui.ctx().request_repaint();
        }
        if let Some(workspace_id) = settings_workspace_select
            && let Some(selected) = resolve_settings_workspace_id(
                &self.workspaces,
                Some(&workspace_id),
                self.settings_workspace_id.as_deref(),
                &self.active.id,
            )
        {
            self.settings_workspace_id = Some(selected);
            self.invalidate_env_profile_ui();
        }
        if let Some(intent) = agents_intent {
            let current_revision = self.agents_snapshot.revision();
            let registration_intent = matches!(&intent, ui::agents::AgentsIntent::Register { .. });
            let action = match intent {
                ui::agents::AgentsIntent::Register {
                    revision,
                    registration,
                } if revision == current_revision => {
                    Some(SettingsJobAction::RegisterAgent(registration))
                }
                ui::agents::AgentsIntent::Delete { revision, agent_id }
                    if revision == current_revision =>
                {
                    Some(SettingsJobAction::DeleteAgent { agent_id })
                }
                ui::agents::AgentsIntent::Run {
                    revision,
                    agent_id,
                    profile_id,
                } if revision == current_revision => Some(SettingsJobAction::PrepareAgentLaunch {
                    agent_id,
                    profile_id,
                    runtime_workspace_id: self.active.id.clone(),
                    extra_arg: None,
                }),
                _ => {
                    self.agents_ui.report_error(if registration_intent {
                        ui::agents::AgentsUiErrorCode::RegistrationFailed
                    } else {
                        ui::agents::AgentsUiErrorCode::SnapshotUnavailable
                    });
                    None
                }
            };
            if let Some(action) = action
                && !self.queue_settings_action(&settings_wsid, env_project_root.clone(), action)
            {
                self.agents_ui.report_error(if registration_intent {
                    ui::agents::AgentsUiErrorCode::RegistrationFailed
                } else {
                    ui::agents::AgentsUiErrorCode::SnapshotUnavailable
                });
            }
        }
        if let Some(intent) = credentials_intent {
            let current_revision = self.credentials_snapshot.revision();
            let failure = match &intent {
                ui::credentials::CredentialsIntent::Add { .. } => {
                    ui::credentials::CredentialsUiErrorCode::AddFailed
                }
                ui::credentials::CredentialsIntent::Delete { .. } => {
                    ui::credentials::CredentialsUiErrorCode::DeleteFailed
                }
                ui::credentials::CredentialsIntent::Reveal { .. } => {
                    ui::credentials::CredentialsUiErrorCode::RevealFailed
                }
                ui::credentials::CredentialsIntent::ScanOrphans { .. } => {
                    ui::credentials::CredentialsUiErrorCode::OrphanScanFailed
                }
                ui::credentials::CredentialsIntent::PurgeOrphans { .. } => {
                    ui::credentials::CredentialsUiErrorCode::OrphanPurgeFailed
                }
            };
            let action = match intent {
                ui::credentials::CredentialsIntent::Add {
                    revision,
                    credential,
                } if revision == current_revision => {
                    Some(SettingsJobAction::AddCredential { credential })
                }
                ui::credentials::CredentialsIntent::Delete {
                    revision,
                    credential_id,
                } if revision == current_revision => {
                    Some(SettingsJobAction::DeleteCredential { credential_id })
                }
                ui::credentials::CredentialsIntent::Reveal {
                    revision,
                    credential_id,
                } if revision == current_revision => {
                    Some(SettingsJobAction::RevealCredential { credential_id })
                }
                ui::credentials::CredentialsIntent::ScanOrphans { revision }
                    if revision == current_revision =>
                {
                    Some(SettingsJobAction::ScanOrphanCredentials)
                }
                ui::credentials::CredentialsIntent::PurgeOrphans {
                    revision,
                    credential_ids,
                } if revision == current_revision => {
                    Some(SettingsJobAction::PurgeOrphanCredentials { credential_ids })
                }
                _ => None,
            };
            if action.is_none()
                || !action.is_some_and(|action| {
                    self.queue_settings_action(&settings_wsid, env_project_root.clone(), action)
                })
            {
                self.credentials_ui.report_error(failure);
            }
        }
        // 관리/모니터 액션 처리 (클로저 밖 — self 전체 &mut 필요한 것들)
        if env_live_reload_toggle != self.config.ui.env_live_reload {
            self.config.ui.env_live_reload = env_live_reload_toggle;
            self.pending_config_save = true;
            ui.ctx().request_repaint();
            // 다음 동기화가 세션 기본 env(활성 조건)를 갱신한다 — 새 셸부터 적용.
            self.stage_workspace_controller_action(WorkspaceControllerAction::SyncDotenv);
        }
        if let Some(name) = workspace_rename {
            // E3: name 컬럼은 별칭 — 빈 값 허용(별칭 해제, 폴더명만 표시).
            if !self.queue_global_settings_action(
                &settings_wsid,
                SettingsJobAction::RenameWorkspace {
                    name: name.trim().to_owned(),
                },
            ) {
                tracing::warn!("workspace rename worker unavailable");
            }
        }
        // Leaf는 snapshot만 읽고 intent를 하나 반환한다. 파일 picker·DB·keyring·파일 쓰기는
        // 모두 render closure가 끝난 뒤 bounded worker/action 경계에서 처리한다.
        if let Some(action) = env_action {
            let action = match action {
                ui::env_profiles::EnvAction::ChooseProjectFolder => {
                    if self.pending_app_host_action.is_none() {
                        self.pending_app_host_action = Some(AppHostIoAction::FolderPicker(
                            FolderPickerPurpose::SetProjectPath {
                                workspace_id: settings_wsid.clone(),
                            },
                        ));
                        ui.ctx().request_repaint();
                    }
                    None
                }
                action => Some(action),
            };
            if let Some(action) = action {
                let queued = match action {
                    ui::env_profiles::EnvAction::Resync => {
                        self.sync_settings_workspace_dotenv(&settings_wsid)
                    }
                    ui::env_profiles::EnvAction::RevealSecret {
                        profile_id,
                        key,
                        reveal_handle,
                    } => {
                        let queued = if self.pending_env_secret_reveal.is_none() {
                            self.pending_env_secret_reveal = Some(EnvSecretRevealJob {
                                generation: self.env_secret_generation,
                                target: EnvSecretRevealTarget::EnvRow {
                                    profile_id: profile_id.clone(),
                                    key: key.clone(),
                                    credential_id: reveal_handle,
                                },
                            });
                            ui.ctx().request_repaint();
                            true
                        } else {
                            false
                        };
                        if !queued {
                            self.env_profiles_ui.reject_reveal(&profile_id, &key);
                        }
                        queued
                    }
                    ui::env_profiles::EnvAction::DotenvWrite { key, value } => self
                        .queue_settings_action(
                            &settings_wsid,
                            env_project_root.clone(),
                            SettingsJobAction::WriteDotenv { key, value },
                        ),
                    ui::env_profiles::EnvAction::DeleteLegacyVar { profile_id, key } => self
                        .queue_settings_action(
                            &settings_wsid,
                            env_project_root.clone(),
                            SettingsJobAction::DeleteLegacyVar { profile_id, key },
                        ),
                    ui::env_profiles::EnvAction::SetProjectPath(path) => self
                        .queue_settings_action(
                            &settings_wsid,
                            env_project_root.clone(),
                            SettingsJobAction::SetProjectPath { path },
                        ),
                    ui::env_profiles::EnvAction::ChooseProjectFolder => true,
                };
                if !queued {
                    self.env_profiles_ui
                        .report_error(ui::env_profiles::EnvUiErrorCode::SnapshotUnavailable);
                }
            }
        }
        // Environment 프로젝트 닫기는 설정 목록의 영속 숨김 상태만 바꾼다. workspace
        // DB/side bar closed state/runtime/session/credential/.env는 의도적으로 건드리지 않는다.
        if let Some((close_id, _project_name)) = self.env_project_close_confirm.clone() {
            match env_project_close_decision {
                Some(true) => {
                    let was_hidden = self.config.ui.hidden_env_project_ids.contains(&close_id);
                    self.settings_workspace_id = close_settings_env_project(
                        &mut self.config.ui.hidden_env_project_ids,
                        env_api_projects,
                        &close_id,
                        self.settings_workspace_id.as_deref(),
                    );
                    if !was_hidden && self.config.ui.hidden_env_project_ids.contains(&close_id) {
                        self.pending_config_save = true;
                        self.invalidate_env_api_projects();
                        ui.ctx().request_repaint();
                    }
                    self.invalidate_env_profile_ui();
                    self.env_project_close_confirm = None;
                }
                Some(false) => self.env_project_close_confirm = None,
                None => {}
            }
        }
        match activity_action {
            Some(ui::activity::ActivityAction::ClearRenderCaches) => {
                // 렌더 캐시만 — 작업/프로세스/스크롤백 무해(2026-07-08 검토). 다음 프레임 재구축.
                self.active.workspace_ui.clear_render_caches();
                for rt in self.warm.values_mut() {
                    rt.workspace_ui.clear_render_caches();
                }
                self.egui_ctx.request_repaint();
            }
            None => {}
        }
        // 설정→알림·벨 팝오버·작업함 페이지는 같은 대상 타입을 돌려준다 — 네비게이션 경로 공유.
        if let Some(target) = notif_click.or(inbox_click).or(inbox_page_click) {
            let workspace_ids = self
                .workspaces
                .iter()
                .map(|workspace| workspace.id.clone())
                .collect::<Vec<_>>();
            let navigation =
                plan_agent_notification_navigation(&target, &self.active.id, &workspace_ids);
            if let Some(navigation) = navigation {
                match navigation {
                    AgentNotificationNavigation::FocusCurrentPty { session } => {
                        self.stage_workspace_controller_action(
                            WorkspaceControllerAction::FocusPty {
                                switch_workspace: None,
                                session,
                            },
                        );
                    }
                    AgentNotificationNavigation::SwitchAndFocusPty {
                        workspace_id,
                        session,
                    } => {
                        self.stage_workspace_controller_action(
                            WorkspaceControllerAction::FocusPty {
                                switch_workspace: Some(workspace_id),
                                session,
                            },
                        );
                    }
                    AgentNotificationNavigation::OpenStructured {
                        switch_workspace,
                        session_id,
                    } => {
                        self.stage_workspace_controller_action(
                            WorkspaceControllerAction::OpenStructured {
                                switch_workspace,
                                session_id,
                            },
                        );
                    }
                }
                // Settings는 별도 native viewport다. 대상 전환 후 그대로 앞에 남으면
                // 이동이 실패한 것처럼 보이므로 닫고 root workspace를 key window로 올린다.
                self.settings_open = false;
                ui.ctx()
                    .send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Focus);
                ui.ctx().request_repaint();
            }
        }
        if out.config_changed {
            self.pending_settings_config_apply = true;
            ui.ctx().request_repaint();
        }
        let controller_action = match out.remote_action {
            ui::settings::RemoteAction::Start => Some(AppControllerAction::RemoteStart),
            ui::settings::RemoteAction::Stop => Some(AppControllerAction::RemoteStop),
            ui::settings::RemoteAction::Forget(host)
                if host.len() <= APP_HOST_PATH_MAX_BYTES && !host.contains('\0') =>
            {
                Some(AppControllerAction::ForgetKnownHost(host))
            }
            ui::settings::RemoteAction::Forget(_) | ui::settings::RemoteAction::None => None,
        };
        let controller_action = controller_action.or_else(|| match out.web_action {
            ui::settings::WebRemoteAction::Start => Some(AppControllerAction::WebStart),
            ui::settings::WebRemoteAction::Stop => Some(AppControllerAction::WebStop),
            ui::settings::WebRemoteAction::RotateToken => Some(AppControllerAction::RotateWebToken),
            ui::settings::WebRemoteAction::DetectHostname => {
                Some(AppControllerAction::DetectHostname)
            }
            ui::settings::WebRemoteAction::CheckServe => Some(AppControllerAction::CheckServe),
            ui::settings::WebRemoteAction::ConfigureServe => {
                Some(AppControllerAction::ConfigureServe)
            }
            ui::settings::WebRemoteAction::OpenApproveUrl(url) => {
                // tailnet 관리 콘솔 승인은 app-owned host worker에서만 연다. Render는
                // bounded URL intent 한 건만 보존하고 subprocess를 직접 만들지 않는다.
                if self.pending_app_host_action.is_none() && is_bounded_https_url(&url) {
                    self.pending_app_host_action = Some(AppHostIoAction::ExternalHttpsUrl(url));
                    ui.ctx().request_repaint();
                }
                None
            }
            ui::settings::WebRemoteAction::None => None,
        });
        if self.pending_app_controller_action.is_none()
            && let Some(action) = controller_action
        {
            self.pending_app_controller_action = Some(action);
            ui.ctx().request_repaint();
        }
        // settings가 닫혔으면 표시 상태를 리셋 — 다음에 열 때 known_hosts를 fresh 로드하고
        // 토큰은 다시 마스킹한다. QR 텍스처도 반환한다(다시 열면 재생성).
        if !self.settings_open {
            self.remote_reveal_token = false;
            self.web_reveal_url = false;
            self.web_qr = None;
        }
        if self.pending_agent_launcher_intent.is_none()
            && let Some(intent) = self.agent_launcher_ui.show(
                ui.ctx(),
                self.agent_launcher_snapshot.as_ref(),
                self.agent_launcher_detection_in_flight,
                &text,
            )
        {
            self.pending_agent_launcher_intent = Some(intent);
            ui.ctx().request_repaint();
        }
        self.frame_stats.end();
        // B1: 이번 프레임에 그린 터미널 렌더 카운터를 프레임 이벤트에 실어 보낸다.
        // frame_stats.end() 뒤라 JSONL 기록 비용은 ui_ms에 섞이지 않는다.
        // 스냅샷 관측도 여기서 — logic()에서 보면 다음 프레임까지 밀려 first_snapshot이
        // first_render보다 늦게 찍힌다(첫 실측에서 발견).
        if self.bench.is_some() {
            let counters = self.active.workspace_ui.frame_counters();
            let has_snapshot = self.active.workspace_ui.any_snapshot();
            if let Some(bench) = self.bench.as_mut() {
                if has_snapshot {
                    bench.note_first_snapshot();
                }
                bench.frame_end(counters);
            }
        }
    }
}

/// Instant → 경과 ms (벤치 ws_step용).
fn elapsed_ms(started: std::time::Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// 세션이 붙어 있는 pane id를 mux 스냅샷에서 찾는다 (알림 클릭 → focus용).
/// 인박스 응답을 PTY에 보낼 바이트 — 끝은 **CR(`\r`)이다**.
///
/// 터미널 raw 모드에서 실행을 일으키는 건 CR이고, 실제 Enter 키도 그렇게 매핑된다
/// (terminal::input_mapper `Key::Enter => b"\r"`). LF(`\n`)를 보내면 줄만 바뀌고
/// 명령이 쌓이기만 한다 — 2026-07-17 사용자가 실제로 겪은 증상.
fn waiting_answer_bytes(reply: &str) -> Vec<u8> {
    format!("{reply}\r").into_bytes()
}

/// 컴포저 접힘 단축키 — FocusComposer의 유효 바인딩에 **dispatcher와 같은 충돌 억제**를
/// 적용한다. dispatcher(take_triggered_action)는 충돌 바인딩의 모든 액션을 억제하는데,
/// 여기서 바인딩만 넘기면 "열기는 안 되고 닫기만 되는" 비대칭이 생긴다(codex P2).
fn composer_collapse_shortcut(
    config: &crate::config::ShortcutsConfig,
) -> Option<egui::KeyboardShortcut> {
    if crate::shortcuts::conflicts(config)
        .contains(&crate::shortcuts::ShortcutAction::FocusComposer)
    {
        return None;
    }
    crate::shortcuts::effective_binding(config, crate::shortcuts::ShortcutAction::FocusComposer)
}

/// 상단바 텍스트 버튼 — 프레임 없이 라벨만, 선택 시 accent-soft 박스.
/// Response를 돌려주므로 팝오버 앵커/hover 텍스트에 쓸 수 있다.
fn tbtn_response(ui: &mut egui::Ui, label: String, selected: bool) -> egui::Response {
    let accent = ui.visuals().selection.bg_fill;
    let col = if selected {
        accent
    } else {
        ui.visuals().weak_text_color()
    };
    let font = egui::FontId::proportional(13.0);
    let galley = ui.painter().layout_no_wrap(label, font, col);
    let w = galley.size().x + 20.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 26.0), egui::Sense::click());
    if selected {
        ui.painter()
            .rect_filled(rect, 6.0, accent.gamma_multiply(0.15));
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(rect, 6.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let pos = egui::pos2(
        rect.center().x - galley.size().x / 2.0,
        rect.center().y - galley.size().y / 2.0,
    );
    ui.painter().galley(pos, galley, col);
    resp
}

fn pane_of_session(
    mux: &runtime::MuxSnapshot,
    session: runtime::SessionId,
) -> Option<runtime::MuxPaneId> {
    mux.tabs
        .iter()
        .flat_map(|tab| &tab.panes)
        .find(|pane| pane.session_id == Some(session))
        .map(|pane| pane.id.clone())
}

fn tab_of_agent_target(
    mux: &runtime::MuxSnapshot,
    pane_id: &runtime::MuxPaneId,
    session_id: runtime::SessionId,
) -> Option<runtime::MuxTabId> {
    mux.tabs
        .iter()
        .find(|tab| {
            tab.panes
                .iter()
                .any(|pane| &pane.id == pane_id && pane.session_id == Some(session_id))
        })
        .map(|tab| tab.id.clone())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AgentNotificationNavigation {
    FocusCurrentPty {
        session: runtime::SessionId,
    },
    SwitchAndFocusPty {
        workspace_id: String,
        session: runtime::SessionId,
    },
    OpenStructured {
        switch_workspace: Option<String>,
        session_id: String,
    },
}

/// Resolve notification navigation before mutating runtimes. Deleted/stale
/// workspace targets fail closed; a cross-workspace PTY target always keeps a
/// deferred exact-session focus, including when the runtime must be rebuilt.
fn plan_agent_notification_navigation(
    target: &ui::notifications::AgentNotificationTarget,
    active_workspace_id: &str,
    known_workspace_ids: &[String],
) -> Option<AgentNotificationNavigation> {
    match target {
        ui::notifications::AgentNotificationTarget::Pty {
            workspace_id,
            session,
        } if workspace_id == active_workspace_id => {
            Some(AgentNotificationNavigation::FocusCurrentPty { session: *session })
        }
        ui::notifications::AgentNotificationTarget::Pty {
            workspace_id,
            session,
        } if known_workspace_ids.contains(workspace_id) => {
            Some(AgentNotificationNavigation::SwitchAndFocusPty {
                workspace_id: workspace_id.clone(),
                session: *session,
            })
        }
        ui::notifications::AgentNotificationTarget::Structured {
            workspace_id,
            session_id,
        } if workspace_id == active_workspace_id => {
            Some(AgentNotificationNavigation::OpenStructured {
                switch_workspace: None,
                session_id: session_id.clone(),
            })
        }
        ui::notifications::AgentNotificationTarget::Structured {
            workspace_id,
            session_id,
        } if known_workspace_ids.contains(workspace_id) => {
            Some(AgentNotificationNavigation::OpenStructured {
                switch_workspace: Some(workspace_id.clone()),
                session_id: session_id.clone(),
            })
        }
        _ => None,
    }
}

fn load_catalog(locale: &str) -> i18n::Catalog {
    i18n::Catalog::load(locale).unwrap_or_else(|e| {
        tracing::warn!("locale catalog 로드 실패({locale}): {e:#}");
        i18n::Catalog::load(i18n::FALLBACK_LOCALE).expect("fallback locale catalog must load")
    })
}

/// warm 풀 상한 초과분 중 축출 가능한(live 세션 없는) workspace를 앞(가장 오래됨)에서부터
/// 고른다. live workspace는 건너뛰며, 그만큼 상한 초과가 허용된다 (작업 보호 우선).
fn warm_eviction_candidates(
    warm_order: &[String],
    max_warm: usize,
    has_live: impl Fn(&str) -> bool,
) -> Vec<String> {
    let overflow = warm_order.len().saturating_sub(max_warm);
    warm_order
        .iter()
        .filter(|id| !has_live(id))
        .take(overflow)
        .cloned()
        .collect()
}

/// 설정의 전역 MB 예산을 resident runtime 수로 나눈 워커별 share.
/// 설정 최소값(32MB)과 resident 최대값(active 1 + warm 12)에서는 1MB 아래로 내려가지
/// 않지만, 잘못된 호출에도 0바이트 정책이 생기지 않도록 1MiB를 최종 하한으로 둔다.
fn per_runtime_cache_budget_bytes(global_budget_mb: u32, resident_runtimes: usize) -> usize {
    const MIB: usize = 1024 * 1024;
    let total = global_budget_mb as usize * MIB;
    (total / resident_runtimes.max(1)).max(MIB)
}

/// live shell 세션 모두가 단일 저CPU 셸 리더만 보유하는지 확인한다.
/// 직접 셸에서 실행한 Codex/Claude는 UI 감지 또는 같은 process group의 자식 수로 보호한다.
/// ResourceUsage 샘플로 세션별 폭주 판정 상태를 갱신한다 (로드맵 B1).
/// 캡처 실패 샘플은 runtime이 마지막 발행값을 유지해 여기 오지 않는다.
/// 압박 알림 (summary, body) 문구 — 최대 사용 세션명이 있으면 지목하고, 없으면
/// 세션 없이 압박만 알린다 (순수 — 테스트 대상).
fn memory_pressure_notification(
    i18n: &i18n::Catalog,
    top_session_name: Option<&str>,
) -> (String, String) {
    let summary = i18n.t("memory_pressure.notification.title", &[]);
    let body = match top_session_name {
        Some(name) => i18n.t("memory_pressure.notification.body", &[("session", name)]),
        None => i18n.t("memory_pressure.notification.body_no_session", &[]),
    };
    (summary, body)
}

fn update_storm_episodes(
    episodes: &mut std::collections::HashMap<
        runtime::SessionId,
        crate::process_storm::StormEpisode,
    >,
    next_episode_id: &mut u64,
    notify_pending: &mut Vec<(runtime::SessionId, usize)>,
    session_usage: &[runtime::SessionResourceUsage],
) {
    for usage in session_usage {
        let prev = episodes.get(&usage.session).copied();
        let next = crate::process_storm::observe(
            prev,
            usage.process_count,
            usage.sampled_at_ms,
            next_episode_id,
        );
        if next.is_some_and(|e| e.confirmed) && !prev.is_some_and(|e| e.confirmed) {
            // 확정 전이 1회 — 로그 + OS 알림 큐 적재(에피소드당 1회).
            tracing::warn!(
                kind = "resource",
                phase = "process_storm_confirmed",
                session = usage.session.0,
                process_count = usage.process_count,
            );
            let peak = next.map_or(usage.process_count, |e| e.peak_process_count);
            notify_pending.push((usage.session, peak));
        }
        match next {
            Some(episode) => {
                episodes.insert(usage.session, episode);
            }
            None => {
                episodes.remove(&usage.session);
            }
        }
    }
}

fn shell_sessions_are_idle(
    sessions: &[runtime::SessionId],
    usage: &[runtime::SessionResourceUsage],
    is_detected_agent: impl Fn(runtime::SessionId) -> bool,
) -> bool {
    !sessions.is_empty()
        && sessions.iter().all(|session| {
            if is_detected_agent(*session) {
                return false;
            }
            usage
                .iter()
                .find(|sample| sample.session == *session)
                .is_some_and(|sample| {
                    sample.pid.is_some()
                        && sample.process_count == 1
                        && sample.cpu_percent.is_some_and(|cpu| cpu <= 1.0)
                        && !sample.high_cpu
                        && !sample.high_rss
                })
        })
}

fn changed_deadline_repaint_delay(
    previous: Option<std::time::Instant>,
    next: Option<std::time::Instant>,
    now: std::time::Instant,
) -> Option<std::time::Duration> {
    if next == previous {
        return None;
    }
    next.map(|deadline| deadline.saturating_duration_since(now))
}

fn expired_warm_workspace_ids(
    warm_order: &[String],
    backgrounded_at: impl Fn(&str) -> Option<std::time::Instant>,
    now: std::time::Instant,
    timeout: std::time::Duration,
) -> Vec<String> {
    warm_order
        .iter()
        .filter(|id| {
            backgrounded_at(id).is_some_and(|at| now.saturating_duration_since(at) >= timeout)
        })
        .cloned()
        .collect()
}

/// warm workspace의 pending_events를 합쳐(coalesce) 재활성 replay를 정확+유계로 만든다.
///
/// replay 규칙(중요): pending_events는 재활성 시 workspace_ui.show()로 렌더 상태를
/// 재구성한다. workspace_ui는 SessionExited/SessionStatusChanged/SessionStatusViewChanged를
/// "현재 mux에 그 세션이 있을 때만" 적용하고(session_alive 체크), MuxUpdated는 pane
/// 구조(session_id/title)만 담아 status/exit은 lifecycle 이벤트로만 반영된다.
///
/// 그래서:
/// 1) 최신 MuxUpdated 하나만 남기고 **맨 앞으로 옮긴다**(나머지 MuxUpdated 제거). replay가
///    최신 mux로 현재 세션/pane을 먼저 확립한 뒤 lifecycle 이벤트가 자기 세션을 찾아
///    적용된다 — 최신 mux 뒤에 남은 SessionExited가 session_alive를 통과해 종료 pane이
///    running으로 남는 버그를 막는다. (mux에 없는 detach된 세션의 잔여 이벤트는 무시돼도
///    화면에 안 나오니 무해.)
/// 2) SessionStatusChanged/SessionStatusViewChanged는 세션별 최신 1개만 유지한다(status는
///    last-wins). detector의 Running↔Waiting churn으로 무계 누적되던 것을 O(세션수)로
///    유계화. 유지분 상대 순서는 보존.
/// 3) ResourceUsage는 프로세스/세션 리소스의 현재 상태라 최신 1개만 유지한다.
/// 4) PtyInputPressure는 세션별 현재 입력 큐 상태라 세션별 최신 1개만 유지한다.
/// 5) Viewport는 화면 전체 최신 스냅샷이므로 세션별 최신 1개만 유지한다. warm/숨김 상태의
///    고출력 세션이 pending replay Vec를 출력량만큼 키우지 않게 한다.
/// 6) SessionExited/ShellSpawned/AgentSpawned/SpawnFailed는 전량 순서 보존.
///
/// 알림은 coalesce 전에 process_ws_notifications가 전량 소비하므로(렌더 replay 전용)
/// 공격적으로 줄여도 알림엔 영향이 없다.
const PENDING_REPLAY_EVENT_CAP: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReplayCompaction {
    overflowed: bool,
}

fn clear_pending_replay_resync_after_activation(
    pending_replay_resync: &mut bool,
    want_active: bool,
    delivered: bool,
) {
    if want_active && delivered {
        *pending_replay_resync = false;
    }
}

fn admit_hidden_active_replay_events(
    workspace_ui: &mut ui::workspace::WorkspaceUi,
    pending_events: &mut Vec<runtime::RuntimeEvent>,
    events: Vec<runtime::RuntimeEvent>,
) -> ReplayCompaction {
    workspace_ui.apply_warm_events(&events);
    pending_events.extend(
        events
            .into_iter()
            .filter(|event| !matches!(event, runtime::RuntimeEvent::AgentSpawnResolved { .. })),
    );
    coalesce_mux_updated(pending_events)
}

fn mux_live_sessions(mux: &runtime::MuxSnapshot) -> std::collections::HashSet<runtime::SessionId> {
    mux.tabs
        .iter()
        .flat_map(|tab| &tab.panes)
        .filter_map(|pane| pane.session_id)
        .collect()
}

fn event_session(event: &runtime::RuntimeEvent) -> Option<runtime::SessionId> {
    match event {
        runtime::RuntimeEvent::ShellSpawned { session }
        | runtime::RuntimeEvent::AgentSpawned { session }
        | runtime::RuntimeEvent::Viewport { session, .. }
        | runtime::RuntimeEvent::SessionExited { session, .. }
        | runtime::RuntimeEvent::SessionStatusChanged { session, .. }
        | runtime::RuntimeEvent::PtyInputPressure { session, .. }
        | runtime::RuntimeEvent::SessionStatusViewChanged { session, .. }
        | runtime::RuntimeEvent::SessionRestored { session, .. }
        | runtime::RuntimeEvent::ScrollbackSearchResult { session, .. }
        | runtime::RuntimeEvent::LastOutputExtracted { session, .. }
        | runtime::RuntimeEvent::SessionFreezeChanged { session, .. } => Some(*session),
        runtime::RuntimeEvent::AgentSpawnResolved {
            session: Some(session),
            ..
        } => Some(*session),
        _ => None,
    }
}

fn replay_transient_event(event: &runtime::RuntimeEvent) -> bool {
    matches!(
        event,
        runtime::RuntimeEvent::ShellSpawned { .. }
            | runtime::RuntimeEvent::AgentSpawned { .. }
            | runtime::RuntimeEvent::SpawnFailed { .. }
            | runtime::RuntimeEvent::AgentSpawnResolved { .. }
    )
}

fn coalesce_mux_updated(events: &mut Vec<runtime::RuntimeEvent>) -> ReplayCompaction {
    // 남길 최신 MuxUpdated(있으면) — 뽑아서 나중에 맨 앞에 재삽입.
    let latest_mux = events
        .iter()
        .rposition(|e| matches!(e, runtime::RuntimeEvent::MuxUpdated { .. }))
        .map(|i| events[i].clone());
    let latest_mux_live_sessions = latest_mux.as_ref().and_then(|event| match event {
        runtime::RuntimeEvent::MuxUpdated { snapshot } => Some(mux_live_sessions(snapshot)),
        _ => None,
    });
    let latest_resource_idx = events
        .iter()
        .rposition(|e| matches!(e, runtime::RuntimeEvent::ResourceUsage { .. }));

    // 세션별 마지막 StatusChanged의 원 인덱스 (나중 것이 이김 → 그 인덱스만 유지).
    let mut latest_status_idx: std::collections::HashMap<runtime::SessionId, usize> =
        std::collections::HashMap::new();
    let mut latest_status_view_idx: std::collections::HashMap<runtime::SessionId, usize> =
        std::collections::HashMap::new();
    let mut latest_input_pressure_idx: std::collections::HashMap<runtime::SessionId, usize> =
        std::collections::HashMap::new();
    let mut latest_viewport_idx: std::collections::HashMap<runtime::SessionId, usize> =
        std::collections::HashMap::new();
    for (i, e) in events.iter().enumerate() {
        if let runtime::RuntimeEvent::SessionStatusChanged { session, .. } = e {
            latest_status_idx.insert(*session, i);
        }
        if let runtime::RuntimeEvent::SessionStatusViewChanged { session, .. } = e {
            latest_status_view_idx.insert(*session, i);
        }
        if let runtime::RuntimeEvent::PtyInputPressure { session, .. } = e {
            latest_input_pressure_idx.insert(*session, i);
        }
        if let runtime::RuntimeEvent::Viewport { session, .. } = e {
            latest_viewport_idx.insert(*session, i);
        }
    }

    let mut idx = 0;
    events.retain(|e| {
        // retain은 원소를 원래 순서대로 한 번씩 방문 → idx로 원 위치를 추적한다.
        let keep = match e {
            // 모든 MuxUpdated 제거 (최신 하나는 아래서 맨 앞에 재삽입).
            runtime::RuntimeEvent::MuxUpdated { .. } => false,
            // 세션별 마지막 StatusChanged만 유지.
            runtime::RuntimeEvent::SessionStatusChanged { session, .. } => {
                latest_status_idx.get(session) == Some(&idx)
            }
            runtime::RuntimeEvent::SessionStatusViewChanged { session, .. } => {
                latest_status_view_idx.get(session) == Some(&idx)
            }
            runtime::RuntimeEvent::ResourceUsage { .. } => latest_resource_idx == Some(idx),
            runtime::RuntimeEvent::PtyInputPressure { session, .. } => {
                latest_input_pressure_idx.get(session) == Some(&idx)
            }
            runtime::RuntimeEvent::Viewport { session, .. } => {
                latest_viewport_idx.get(session) == Some(&idx)
            }
            _ => true,
        };
        let keep = keep
            && latest_mux_live_sessions
                .as_ref()
                .is_none_or(|live| event_session(e).is_none_or(|session| live.contains(&session)));
        idx += 1;
        keep
    });

    if let Some(mux) = latest_mux {
        events.insert(0, mux);
    }

    let mut overflowed = false;
    if events.len() > PENDING_REPLAY_EVENT_CAP {
        overflowed = true;
        let mut drop_remaining = events.len() - PENDING_REPLAY_EVENT_CAP;
        events.retain(|event| {
            if drop_remaining > 0 && replay_transient_event(event) {
                drop_remaining -= 1;
                false
            } else {
                true
            }
        });
    }

    if events.len() > PENDING_REPLAY_EVENT_CAP {
        overflowed = true;
        let keep_tail = PENDING_REPLAY_EVENT_CAP.saturating_sub(1);
        let non_mux_len = events.len().saturating_sub(1);
        let drop_non_mux = non_mux_len.saturating_sub(keep_tail);
        if drop_non_mux > 0 {
            events.drain(1..1 + drop_non_mux);
        }
        events.truncate(PENDING_REPLAY_EVENT_CAP);
    }

    ReplayCompaction { overflowed }
}

/// T1: pane 우클릭 → 환경설정 진입 시 감지한 focused 세션 폴더 배너 상태.
struct EnvSessionCwdBanner {
    /// focused 세션의 현재 작업 폴더 (agent_detect 워커 lsof 소스 재사용).
    cwd: std::path::PathBuf,
    /// cwd가 기존 워크스페이스 path와 일치하거나 그 하위 폴더인가.
    registered: bool,
}

/// 폭주 배너 버튼이 요청하는 대응 (로드맵 B3) — 확정된 폭주 세션 전체에 적용된다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StormAction {
    Freeze,
    Resume,
    Kill,
}

/// T1: cwd가 워크스페이스 path 중 하나와 일치하거나 그 하위 폴더인지 판정
/// (순수 — 테스트 대상). `Path::starts_with`는 컴포넌트 단위라 `/a/bc`가
/// `/a/b`에 속하는 것으로 오판하지 않는다.
fn cwd_belongs_to_any(cwd: &std::path::Path, roots: &[std::path::PathBuf]) -> bool {
    roots.iter().any(|root| cwd.starts_with(root))
}

/// 영속 pane snapshot에서 이 제목의 세션 cwd를 찾는다 (순수 — 테스트 대상).
///
/// **키는 DB에 저장된 raw 제목**("workspace.spawn.shell 3")이다. 호출측이 i18n 렌더된
/// 값("셸 3")을 넘기면 항상 miss가 되어 프로젝트명 해석이 조용히 실패한다 —
/// warm 워크스페이스에서 실제로 그랬다(리뷰 P2-1). session_titles는 raw를 보관한다.
fn pane_cwd<'a>(panes: &'a [(String, String)], raw_title: &str) -> Option<&'a str> {
    panes
        .iter()
        .find(|(title, _)| title == raw_title)
        .map(|(_, cwd)| cwd.as_str())
        .filter(|cwd| !cwd.is_empty())
}

/// 비활성(warm/유휴) 워크스페이스 pane의 표시명 (순수 — 테스트 대상).
/// 활성 워크스페이스의 `resolve_session_title`과 같은 규칙: 사용자가 rename했으면
/// 그대로, 기본 제목("셸 N")이면 세션 cwd의 프로젝트명으로 대체, cwd가 없거나 판별
/// 불가면 기본 제목을 i18n 렌더한 값으로 폴백.
fn activity_session_name(
    raw_title: &str,
    cwd: Option<&str>,
    catalog: &i18n::Catalog,
    resolve_project: impl Fn(&str) -> Option<String>,
) -> String {
    if !ui::workspace::is_default_session_title(raw_title) {
        return ui::workspace::display_pane_title(raw_title, catalog);
    }
    cwd.filter(|cwd| !cwd.is_empty())
        .and_then(resolve_project)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| ui::workspace::display_pane_title(raw_title, catalog))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// 턴 시작 판정 — 막혀 있던(대기/완료) 세션이 작업 중으로 바뀐 것만 새 턴이다.
    /// working 재진입(2분 stale 창 만료 뒤 하트비트 재개)을 턴 시작으로 오인하면
    /// 진행 중인 턴의 진짜 Error latch를 지운다(병렬 리뷰 medium).
    #[test]
    fn 턴_시작은_막혀있던_세션의_작업_재개만_센다() {
        let key = |name: &str, id: u64| (name.to_owned(), runtime::SessionId(id));
        let set = |keys: &[(String, runtime::SessionId)]| {
            keys.iter()
                .cloned()
                .collect::<std::collections::HashSet<_>>()
        };
        let resumed = key("ws", 1); // 완료/대기 뒤 재개 = 새 턴
        let heartbeat = key("ws", 2); // stale 만료 뒤 하트비트 재개 = 같은 턴
        let steady = key("ws", 3); // 계속 작업 중

        let started = turn_start_transitions(
            &set(&[resumed.clone(), heartbeat, steady.clone()]),
            &set(&[steady]),
            &set(std::slice::from_ref(&resumed)),
        );
        assert_eq!(started, vec![resumed]);
    }

    #[test]
    fn bounded_projection_failure_keeps_last_complete_snapshot() {
        let mut snapshot = vec!["last-complete".to_owned()];

        let error = replace_complete_projection::<_, &'static str>(
            &mut snapshot,
            Err("bounded_read_failed"),
        );
        assert_eq!(error, Err("bounded_read_failed"));
        assert_eq!(snapshot, ["last-complete"]);

        replace_complete_projection::<_, &'static str>(&mut snapshot, Ok(vec!["fresh".to_owned()]))
            .expect("complete projection replaces the snapshot");
        assert_eq!(snapshot, ["fresh"]);
    }

    fn slack_inventory_row(
        id: &str,
        enabled: bool,
        tool_count: usize,
    ) -> mcp_store::McpServerInventoryRow {
        mcp_store::McpServerInventoryRow {
            id: id.to_owned(),
            name: "Slack".to_owned(),
            kind: "http".to_owned(),
            url: Some(SLACK_MCP_URL.to_owned()),
            enabled,
            tool_count,
        }
    }

    #[test]
    fn slack_projection은_duplicate_canonical_rows를_fail_closed한다() {
        let projection = connector_slack_projection(&[
            slack_inventory_row("slack-a", true, 1),
            slack_inventory_row("slack-b", true, 2),
        ]);

        assert_eq!(projection.status, connector_contract::SlackStatus::Failed);
        assert_eq!(projection.server_id, None);
        assert_eq!(projection.tool_count, 0);
        assert!(!projection.can_choose_workspace);
    }

    #[test]
    fn agent_hook_query는_세션이_없으면_경과시간과_무관하게_idle이다() {
        assert!(!agent_hook_query_due(0, std::time::Duration::from_secs(60)));
        assert!(!agent_hook_query_due(
            1,
            AGENT_HOOK_QUERY_INTERVAL - std::time::Duration::from_nanos(1)
        ));
        assert!(agent_hook_query_due(1, AGENT_HOOK_QUERY_INTERVAL));
    }

    #[test]
    fn agent_restore_failure는_unchanged_binding에서도_다음_bounded_refresh에_재시도한다() {
        assert!(should_process_agent_bindings(
            true,
            None,
            "workspace",
            false
        ));
        assert!(should_process_agent_bindings(
            false,
            None,
            "workspace",
            true
        ));
        assert!(!should_process_agent_bindings(
            false,
            None,
            "workspace",
            false
        ));
        assert!(!should_process_agent_bindings(
            false,
            Some("workspace"),
            "workspace",
            true
        ));
    }

    #[test]
    fn slack_projection은_single_canonical_row만_exposes한다() {
        let projection = connector_slack_projection(&[slack_inventory_row("slack-a", true, 2)]);

        assert_eq!(
            projection.status,
            connector_contract::SlackStatus::Connected
        );
        assert_eq!(
            projection.server_id.as_ref().map(|id| id.as_str()),
            Some("slack-a")
        );
        assert_eq!(projection.tool_count, 2);
        assert!(projection.can_choose_workspace);
    }

    #[test]
    fn connector_import_file은_exact_byte_ceiling만_허용한다() {
        let path = temp_db_path("connector-import-byte-ceiling");
        let limit = connector_contract::ResourceLimits::PRODUCTION_CEILING.import_input_bytes;
        std::fs::write(&path, vec![b'x'; limit]).unwrap();
        assert_eq!(read_connector_import_file(&path).unwrap().len(), limit);

        std::fs::write(&path, vec![b'x'; limit + 1]).unwrap();
        assert_eq!(
            read_connector_import_file(&path).unwrap_err(),
            connector_contract::ErrorCode::LimitExceeded
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn connector_host_worker_failure는_exact_import를_fail_closed한다() {
        let operation_id = connector_contract::OperationId::new("import-op");
        let action = connector_service::HostAction::RequestImportSource {
            operation_id: operation_id.clone(),
            source: connector_contract::ImportSourceRequest::ClaudeDesktop,
        };
        let completion =
            AppHostIoFallback::for_action(&AppHostIoAction::Connector(action)).into_completion();

        assert!(matches!(
            completion,
            AppHostIoCompletion::Dispatch(
                connector_contract::ConnectorIntent::FailImportSource {
                    operation_id: failed_operation,
                    source: connector_contract::ImportSource::ClaudeDesktop,
                    error_code: connector_contract::ErrorCode::HostUnavailable,
                }
            ) if failed_operation == operation_id
        ));
    }

    #[test]
    fn workspace_git_label_cache_evicts_oldest_accessed_entry_at_cap() {
        let now = std::time::Instant::now();
        let mut cache = WorkspaceGitLabelCache::default();
        for idx in 0..WORKSPACE_GIT_LABEL_CACHE_CAP {
            cache.insert(
                format!("path-{idx}"),
                Some(format!("branch-{idx}")),
                now + std::time::Duration::from_nanos(idx as u64),
            );
        }

        assert_eq!(
            cache.get_fresh("path-0", now + std::time::Duration::from_millis(1)),
            Some(Some("branch-0".to_owned()))
        );
        cache.insert(
            format!("path-{WORKSPACE_GIT_LABEL_CACHE_CAP}"),
            Some(format!("branch-{WORKSPACE_GIT_LABEL_CACHE_CAP}")),
            now + std::time::Duration::from_millis(2),
        );

        assert_eq!(cache.entry_count(), WORKSPACE_GIT_LABEL_CACHE_CAP);
        assert_eq!(
            cache.get_fresh("path-0", now + std::time::Duration::from_millis(3)),
            Some(Some("branch-0".to_owned()))
        );
        assert_eq!(
            cache.get_fresh("path-1", now + std::time::Duration::from_millis(3)),
            None
        );
        assert_eq!(
            cache.get_fresh(
                &format!("path-{WORKSPACE_GIT_LABEL_CACHE_CAP}"),
                now + std::time::Duration::from_millis(3)
            ),
            Some(Some(format!("branch-{WORKSPACE_GIT_LABEL_CACHE_CAP}")))
        );
    }

    #[test]
    fn app_host_folder_picker_failure는_빈_completion으로_닫힌다() {
        let completion = AppHostIoFallback::for_action(&AppHostIoAction::FolderPicker(
            FolderPickerPurpose::SelectWorkspaceInSettings,
        ))
        .into_completion();

        assert!(matches!(
            completion,
            AppHostIoCompletion::FolderPicker {
                purpose: FolderPickerPurpose::SelectWorkspaceInSettings,
                selected_path: None,
            }
        ));
    }

    #[test]
    fn app_host_https_url은_exact_bound와_scheme을_강제한다() {
        let mut exact = "https://".to_owned();
        exact.push_str(&"a".repeat(APP_HOST_URL_MAX_BYTES - exact.len()));
        assert!(is_bounded_https_url(&exact));
        exact.push('a');
        assert!(!is_bounded_https_url(&exact));
        assert!(!is_bounded_https_url("http://example.com"));
        assert!(!is_bounded_https_url("https://example.com\nsecret"));
    }

    #[test]
    fn app_host_file_budget은_exact_item_byte_depth만_허용한다() {
        let mut budget = AppHostFileOperationBudget::default();
        for _ in 0..APP_HOST_FILE_OPERATION_MAX_ITEMS {
            budget
                .consume_item(APP_HOST_FILE_OPERATION_MAX_DEPTH)
                .unwrap();
        }
        assert!(
            budget
                .consume_item(APP_HOST_FILE_OPERATION_MAX_DEPTH)
                .is_err()
        );

        let mut budget = AppHostFileOperationBudget::default();
        budget
            .consume_bytes(APP_HOST_FILE_OPERATION_MAX_BYTES)
            .unwrap();
        assert!(budget.consume_bytes(1).is_err());

        let mut budget = AppHostFileOperationBudget::default();
        assert!(
            budget
                .consume_item(APP_HOST_FILE_OPERATION_MAX_DEPTH + 1)
                .is_err()
        );
    }

    #[test]
    fn app_host_file_copy는_cancel이면_destination을_만들지_않는다() {
        let root = std::env::temp_dir().join(format!(
            "deppy-app-host-cancel-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.bin");
        let destination = root.join("destination.bin");
        std::fs::write(&source, vec![7_u8; APP_HOST_FILE_COPY_BUFFER_BYTES * 2]).unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(true);
        let mut budget = AppHostFileOperationBudget::default();
        assert!(app_host_copy_recursive(&source, &destination, &mut budget, &cancel, 0).is_err());
        assert!(!destination.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn app_host_file_move는_cancel이면_source를_보존한다() {
        let root = std::env::temp_dir().join(format!(
            "deppy-app-host-move-cancel-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let destination_dir = root.join("destination");
        std::fs::create_dir_all(&destination_dir).unwrap();
        let source = root.join("source.bin");
        std::fs::write(&source, b"bounded").unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(true);

        assert_eq!(
            app_host_move(&root, &source, &destination_dir, &cancel),
            Err(ui::file_tree::FileTreeIoErrorCode::NativeFailure)
        );
        assert!(source.exists());
        assert!(!destination_dir.join("source.bin").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn app_host_composer_history는_bounded_atomic_jsonl을_쓴다() {
        let path = temp_db_path("composer-history").with_extension("jsonl");
        let history: Arc<[Arc<str>]> =
            vec![Arc::<str>::from("one"), Arc::<str>::from("two")].into();
        assert!(write_composer_history(&path, &history));
        let lines = std::fs::read_to_string(&path).unwrap();
        assert_eq!(lines, "\"one\"\n\"two\"\n");

        let oversized: Arc<[Arc<str>]> = (0..=ui::composer::COMPOSER_HISTORY_MAX_ITEMS)
            .map(|index| Arc::<str>::from(index.to_string()))
            .collect::<Vec<_>>()
            .into();
        assert!(!write_composer_history(&path, &oversized));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), lines);
        std::fs::remove_file(path).unwrap();
    }

    /// codex P2 회귀: FocusComposer 바인딩이 다른 액션과 충돌하면 dispatcher가 열기를
    /// 억제하므로 접힘 단축키도 함께 없어져야 한다(비대칭 방지).
    #[test]
    fn composer_collapse_shortcut_은_충돌_시_none이다() {
        use crate::shortcuts::{self, ShortcutAction};
        let mut config = crate::config::ShortcutsConfig::default();
        assert_eq!(
            composer_collapse_shortcut(&config),
            shortcuts::parse_binding("Command+J"),
            "기본은 ⌘J"
        );
        // 다른 액션을 ⌘J로 리바인드 → 충돌 → 열기/닫기 모두 억제.
        shortcuts::set_binding(
            &mut config,
            ShortcutAction::NewShell,
            shortcuts::parse_binding("Command+J"),
        );
        assert!(
            shortcuts::conflicts(&config).contains(&ShortcutAction::FocusComposer),
            "전제: 충돌 집합에 FocusComposer가 있어야 한다"
        );
        assert_eq!(composer_collapse_shortcut(&config), None);
        // 충돌 해소(FocusComposer 리바인드) → 새 바인딩이 접힘 키.
        shortcuts::set_binding(
            &mut config,
            ShortcutAction::FocusComposer,
            shortcuts::parse_binding("Command+Shift+J"),
        );
        assert_eq!(
            composer_collapse_shortcut(&config),
            shortcuts::parse_binding("Command+Shift+J")
        );
    }

    /// 2026-07-17 사용자 회귀: 인박스에서 답을 보내면 "명령어가 줄에 쌓이고 실행은
    /// 안 되는" 증상. LF는 줄만 바꾼다 — PTY에서 실행을 일으키는 건 CR이고, 실제
    /// Enter 키 매핑(terminal::input_mapper)도 CR이다.
    #[test]
    fn waiting_answer는_실제_enter키와_같은_cr로_끝난다() {
        assert_eq!(waiting_answer_bytes("y"), b"y\r".to_vec());
        assert_eq!(waiting_answer_bytes("2"), b"2\r".to_vec());
        assert!(
            !waiting_answer_bytes("y").contains(&b'\n'),
            "LF를 보내면 실행되지 않는다"
        );
        // 실제 Enter 키가 내는 바이트와 종결이 같아야 한다(같은 경로로 취급되도록).
        let enter = terminal::input_mapper::map_event(
            &egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
            false,
            &egui::Modifiers::NONE,
        )
        .expect("Enter 매핑");
        assert_eq!(
            waiting_answer_bytes("y").last(),
            enter.last(),
            "인박스 응답의 종결 바이트가 실제 Enter와 같아야 한다"
        );
    }

    #[test]
    fn agent_notification_navigation_preserves_transport_workspace_and_session() {
        use ui::notifications::AgentNotificationTarget as Target;

        let known = vec!["ws-a".to_owned(), "ws-b".to_owned()];
        let current_pty = Target::Pty {
            workspace_id: "ws-a".to_owned(),
            session: runtime::SessionId(7),
        };
        assert_eq!(
            plan_agent_notification_navigation(&current_pty, "ws-a", &known),
            Some(AgentNotificationNavigation::FocusCurrentPty {
                session: runtime::SessionId(7),
            })
        );

        let cross_pty = Target::Pty {
            workspace_id: "ws-b".to_owned(),
            session: runtime::SessionId(8),
        };
        assert_eq!(
            plan_agent_notification_navigation(&cross_pty, "ws-a", &known),
            Some(AgentNotificationNavigation::SwitchAndFocusPty {
                workspace_id: "ws-b".to_owned(),
                session: runtime::SessionId(8),
            })
        );

        let current_app = Target::Structured {
            workspace_id: "ws-a".to_owned(),
            session_id: "app-1".to_owned(),
        };
        assert_eq!(
            plan_agent_notification_navigation(&current_app, "ws-a", &known),
            Some(AgentNotificationNavigation::OpenStructured {
                switch_workspace: None,
                session_id: "app-1".to_owned(),
            })
        );

        let cross_app = Target::Structured {
            workspace_id: "ws-b".to_owned(),
            session_id: "app-2".to_owned(),
        };
        assert_eq!(
            plan_agent_notification_navigation(&cross_app, "ws-a", &known),
            Some(AgentNotificationNavigation::OpenStructured {
                switch_workspace: Some("ws-b".to_owned()),
                session_id: "app-2".to_owned(),
            })
        );

        let stale = Target::Structured {
            workspace_id: "deleted".to_owned(),
            session_id: "app-stale".to_owned(),
        };
        assert_eq!(
            plan_agent_notification_navigation(&stale, "ws-a", &known),
            None
        );
    }

    #[test]
    fn agent_pty_focus_requires_matching_pane_and_session_pair() {
        let pane_id = runtime::MuxPaneId("pane-1".to_owned());
        let tab_id = runtime::MuxTabId("tab-1".to_owned());
        let session_id = runtime::SessionId(9);
        let mux = runtime::MuxSnapshot {
            tabs: vec![runtime::TabSnapshot {
                id: tab_id.clone(),
                title: "agents".to_owned(),
                layout: runtime::LayoutNode::Pane(pane_id.clone()),
                panes: vec![runtime::PaneSnapshot {
                    id: pane_id.clone(),
                    session_id: Some(session_id),
                    title: "Codex".to_owned(),
                    persistent_session_id: None,
                }],
            }],
            active_tab: Some(tab_id.clone()),
            focused_pane: Some(pane_id.clone()),
        };

        assert_eq!(
            tab_of_agent_target(&mux, &pane_id, session_id),
            Some(tab_id)
        );
        assert_eq!(
            tab_of_agent_target(&mux, &pane_id, runtime::SessionId(10)),
            None
        );
        assert_eq!(
            tab_of_agent_target(
                &mux,
                &runtime::MuxPaneId("stale-pane".to_owned()),
                session_id
            ),
            None
        );
    }

    #[test]
    fn 시작_font_snapshot은_첫_frame_재설치를_유발하지_않는다() {
        let config = Config::default();
        let last_ui_font = config.ui.ui_font.clone();
        let last_mono_font = config.terminal.mono_font.clone();
        let last_mono_weight = config.terminal.mono_weight.clone();
        assert!(!font_settings_changed(
            &config,
            &last_ui_font,
            &last_mono_font,
            &last_mono_weight,
        ));

        let mut changed = config;
        changed.terminal.mono_weight = "Bold".to_owned();
        assert!(font_settings_changed(
            &changed,
            &last_ui_font,
            &last_mono_font,
            &last_mono_weight,
        ));
    }

    /// ⑦ 검증: .env 외부 수정 감지의 근거인 baseline 상태가 파일 변경/생성/삭제를
    /// 구분한다 — 2초 점검이 이 값의 변화로 재동기화를 트리거한다(B 경로).
    #[test]
    fn dotenv_baseline은_외부_수정과_생성_삭제를_감지한다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-dotenv-state-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // 파일 없음 → (false, None). 경로 없음도 동일.
        assert_eq!(dotenv_state_for_root(Some(&dir)), (false, None));
        assert_eq!(dotenv_state_for_root(None), (false, None));
        // 생성 감지
        std::fs::write(dir.join(".env"), "A=1\n").unwrap();
        let created = dotenv_state_for_root(Some(&dir));
        assert!(created.0 && created.1.is_some());
        // 내용 수정(mtime 변화) 감지 — 에디터/터미널로 직접 고친 경우
        std::thread::sleep(std::time::Duration::from_millis(15));
        std::fs::write(dir.join(".env"), "A=2\n").unwrap();
        let edited = dotenv_state_for_root(Some(&dir));
        assert_ne!(created, edited, "외부 수정이 baseline에 반영되지 않음");
        // .env.local 추가도 감지(병합 대상 전체를 본다)
        std::fs::write(dir.join(".env.local"), "B=1\n").unwrap();
        assert_ne!(edited, dotenv_state_for_root(Some(&dir)));
        // 삭제 감지
        std::fs::remove_file(dir.join(".env")).unwrap();
        std::fs::remove_file(dir.join(".env.local")).unwrap();
        assert_eq!(dotenv_state_for_root(Some(&dir)), (false, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 별칭 우선(2026-07-18): 별칭이 있으면 폴더명 병기 없이 별칭만, 없으면 폴더명.
    #[test]
    fn 워크스페이스_표시명은_별칭이_있으면_별칭만_보여준다() {
        let row = |name: &str, path: &str| storage::WorkspaceRow {
            id: "w".into(),
            name: name.into(),
            path: path.into(),
            created_at: String::new(),
        };
        // 별칭 없음/기본값 → 폴더명
        assert_eq!(
            App::workspace_display_name(&row("", "/p/binjari")),
            "binjari"
        );
        assert_eq!(
            App::workspace_display_name(&row("default", "/p/binjari")),
            "binjari"
        );
        assert_eq!(
            App::workspace_display_name(&row("binjari", "/p/binjari")),
            "binjari"
        );
        // 별칭이 폴더명과 달라도 별칭만 — 「이름 바꾸기」 후 폴더명 병기 없음
        assert_eq!(
            App::workspace_display_name(&row("예매봇", "/p/binjari")),
            "예매봇"
        );
        // 경로 없음 — 별칭만, 그것도 없으면 "~"
        assert_eq!(App::workspace_display_name(&row("예매봇", "")), "예매봇");
        assert_eq!(App::workspace_display_name(&row("", "")), "~");
    }

    /// 리뷰 P2-1 회귀: cwd 조회 키는 **DB의 raw 제목**이다. i18n 렌더된 값("셸 3")을
    /// 넘기면 항상 miss가 되어 프로젝트명 해석이 조용히 실패한다 — warm 워크스페이스가
    /// 실제로 그랬다(session_titles가 렌더 값을 담고 있었다).
    #[test]
    fn cwd_조회는_raw_제목을_키로_쓴다() {
        let catalog = load_catalog("ko-KR");
        let raw = "workspace.spawn.shell 3";
        let panes = vec![(
            raw.to_owned(),
            "/Users/jr/Desktop/Projects/deppy-sijo".to_owned(),
        )];
        // raw로 조회 → cwd 적중 → 프로젝트명 해석 가능
        assert_eq!(
            pane_cwd(&panes, raw),
            Some("/Users/jr/Desktop/Projects/deppy-sijo")
        );
        // 렌더된 값으로 조회 → miss (옛 버그 경로: session_titles가 렌더 값을 담았다)
        let rendered = ui::workspace::display_pane_title(raw, &catalog);
        assert_ne!(
            rendered, raw,
            "ko에서 렌더 값이 raw와 같으면 이 테스트는 무의미"
        );
        assert_eq!(
            pane_cwd(&panes, &rendered),
            None,
            "렌더 값으로 조회가 적중하면 회귀"
        );
        // 빈 cwd는 없는 것으로 취급
        let empty = vec![(raw.to_owned(), String::new())];
        assert_eq!(pane_cwd(&empty, raw), None);
    }

    /// T1: 세션 cwd가 기존 워크스페이스 경로(또는 하위)에 속하는지 판정 — 등록 배너 조건.
    #[test]
    fn 세션_cwd_워크스페이스_소속_판정() {
        let roots = vec![
            PathBuf::from("/Users/jr/Desktop/Projects/deppy-sijo"),
            PathBuf::from("/Users/jr/work"),
        ];
        // 정확히 일치 → 소속
        assert!(cwd_belongs_to_any(Path::new("/Users/jr/work"), &roots));
        // 하위 폴더 → 소속
        assert!(cwd_belongs_to_any(
            Path::new("/Users/jr/Desktop/Projects/deppy-sijo/crates/app"),
            &roots
        ));
        // 무관한 새 폴더 → 미소속 (배너 표시 대상)
        assert!(!cwd_belongs_to_any(
            Path::new("/Users/jr/Desktop/Projects/Crawler"),
            &roots
        ));
        // 접두 문자열만 같은 형제 폴더는 오판하지 않는다 (/a/bc vs /a/b)
        assert!(!cwd_belongs_to_any(
            Path::new("/Users/jr/workbench"),
            &roots
        ));
        // 루트 목록이 비면 항상 미소속
        assert!(!cwd_belongs_to_any(Path::new("/tmp"), &[]));
    }

    /// 활동 패널(warm/유휴)의 pane 이름: 기본 제목은 프로젝트명으로, rename은 그대로.
    #[test]
    fn 활동_pane_이름은_기본제목이면_프로젝트명으로_표시된다() {
        let catalog = load_catalog("ko-KR");
        // 기본 제목 + cwd → 프로젝트(폴더)명
        assert_eq!(
            activity_session_name(
                "workspace.spawn.shell 1",
                Some("/Users/jr/Desktop/Projects/deppy-sijo"),
                &catalog,
                |cwd| crate::agent_detect::project_display_name(
                    cwd,
                    crate::config::SessionNameStyle::Folder
                ),
            ),
            "deppy-sijo"
        );
        // 사용자 rename은 cwd와 무관하게 그대로
        assert_eq!(
            activity_session_name("배포 작업", Some("/tmp/whatever"), &catalog, |cwd| {
                crate::agent_detect::project_display_name(
                    cwd,
                    crate::config::SessionNameStyle::Folder,
                )
            }),
            "배포 작업"
        );
        // cwd 없음/빈 값 → 기본 제목 i18n 렌더로 폴백(기존 동작)
        let fallback = ui::workspace::display_pane_title("workspace.spawn.shell 3", &catalog);
        assert_eq!(
            activity_session_name("workspace.spawn.shell 3", None, &catalog, |cwd| {
                crate::agent_detect::project_display_name(
                    cwd,
                    crate::config::SessionNameStyle::Folder,
                )
            }),
            fallback
        );
        assert_eq!(
            activity_session_name("workspace.spawn.shell 3", Some(""), &catalog, |cwd| {
                crate::agent_detect::project_display_name(
                    cwd,
                    crate::config::SessionNameStyle::Folder,
                )
            }),
            fallback
        );
        // 상대경로(비정상) → 폴백
        assert_eq!(
            activity_session_name(
                "workspace.spawn.shell 3",
                Some("relative/path"),
                &catalog,
                |cwd| crate::agent_detect::project_display_name(
                    cwd,
                    crate::config::SessionNameStyle::Folder
                )
            ),
            fallback
        );
    }

    /// 구분 가능한 최소 MuxUpdated 이벤트 (active_tab 태그로 스냅샷을 식별).
    fn mux_event(tag: &str) -> runtime::RuntimeEvent {
        runtime::RuntimeEvent::MuxUpdated {
            snapshot: std::sync::Arc::new(runtime::MuxSnapshot {
                tabs: Vec::new(),
                active_tab: Some(runtime::MuxTabId(tag.to_owned())),
                focused_pane: None,
            }),
        }
    }

    fn live_mux_event(tag: &str, sessions: &[u64]) -> runtime::RuntimeEvent {
        runtime::RuntimeEvent::MuxUpdated {
            snapshot: std::sync::Arc::new(runtime::MuxSnapshot {
                tabs: vec![runtime::TabSnapshot {
                    id: runtime::MuxTabId(tag.to_owned()),
                    title: tag.to_owned(),
                    layout: runtime::LayoutNode::Pane(runtime::MuxPaneId(format!(
                        "{tag}-pane-{}",
                        sessions.first().copied().unwrap_or(0)
                    ))),
                    panes: sessions
                        .iter()
                        .map(|session| runtime::PaneSnapshot {
                            id: runtime::MuxPaneId(format!("{tag}-pane-{session}")),
                            session_id: Some(runtime::SessionId(*session)),
                            title: format!("session-{session}"),
                            persistent_session_id: None,
                        })
                        .collect(),
                }],
                active_tab: Some(runtime::MuxTabId(tag.to_owned())),
                focused_pane: sessions
                    .first()
                    .map(|session| runtime::MuxPaneId(format!("{tag}-pane-{session}"))),
            }),
        }
    }

    fn mux_tag(e: &runtime::RuntimeEvent) -> Option<&str> {
        match e {
            runtime::RuntimeEvent::MuxUpdated { snapshot } => {
                snapshot.active_tab.as_ref().map(|t| t.0.as_str())
            }
            _ => None,
        }
    }

    fn resource_event(sampled_at_ms: u64) -> runtime::RuntimeEvent {
        runtime::RuntimeEvent::ResourceUsage {
            snapshot: runtime::ProcessResourceSnapshot {
                pid: 42,
                sampled_at_ms,
                rss_bytes: sampled_at_ms * 1024,
                cpu_percent: Some(sampled_at_ms as f32),
                high_cpu: false,
                high_rss: false,
            },
            session_usage: Vec::new(),
        }
    }

    fn input_pressure_event(session: u64, queued_bytes: usize) -> runtime::RuntimeEvent {
        runtime::RuntimeEvent::PtyInputPressure {
            session: runtime::SessionId(session),
            pressure: runtime::PtyInputPressure {
                attempted_bytes: queued_bytes + 1,
                queued_bytes,
                queued_messages: 1,
                max_bytes: 1024,
                max_messages: 8,
                reason: runtime::PtyInputRejectReason::QueueFull,
            },
        }
    }

    fn viewport_event(session: u64, title: &str) -> runtime::RuntimeEvent {
        runtime::RuntimeEvent::Viewport {
            session: runtime::SessionId(session),
            snapshot: std::sync::Arc::new(terminal::TerminalViewportSnapshot {
                cols: 0,
                rows: 0,
                cursor: terminal::CursorSnapshot {
                    col: 0,
                    row: 0,
                    shape: terminal::CursorShape::Block,
                    visible: false,
                },
                visible_cells: Vec::new().into(),
                dirty_ranges: Vec::new(),
                title: Some(title.to_owned()),
                scroll_offset: 0,
                is_alt_screen: false,
            }),
            bracketed_paste: false,
        }
    }

    fn temp_db_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "deppy-sijo-{name}-{}-{nanos}.sqlite3",
            std::process::id()
        ))
    }

    fn sqlite_sidecar(path: &Path, suffix: &str) -> PathBuf {
        let mut file_name = path.as_os_str().to_owned();
        file_name.push(suffix);
        PathBuf::from(file_name)
    }

    fn remove_sqlite_files(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(sqlite_sidecar(path, "-wal"));
        let _ = std::fs::remove_file(sqlite_sidecar(path, "-shm"));
    }

    #[test]
    fn initial_workspace_prefers_existing_last_workspace() {
        let path = temp_db_path("initial-workspace-existing");
        let db = Db::open(&path).unwrap();
        let _default = db.ensure_default_workspace().unwrap();
        let last = db.create_workspace("last").unwrap();
        assert_eq!(
            initial_workspace_id(&db, Some(&last), &Default::default()).unwrap(),
            last
        );
        drop(db);
        remove_sqlite_files(&path);
    }

    #[test]
    fn initial_workspace_falls_back_when_last_workspace_is_missing() {
        let path = temp_db_path("initial-workspace-missing");
        let db = Db::open(&path).unwrap();
        let default = db.ensure_default_workspace().unwrap();
        assert_eq!(
            initial_workspace_id(&db, Some("missing"), &Default::default()).unwrap(),
            default
        );
        drop(db);
        remove_sqlite_files(&path);
    }

    #[test]
    fn initial_workspace_skips_persisted_closed_workspace() {
        let path = temp_db_path("initial-workspace-closed");
        let db = Db::open(&path).unwrap();
        let closed = db.ensure_default_workspace().unwrap();
        let visible = db.create_workspace("visible").unwrap();
        let closed_ids = std::collections::BTreeSet::from([closed.clone()]);
        assert_eq!(
            initial_workspace_id(&db, Some(&closed), &closed_ids).unwrap(),
            visible
        );
        drop(db);
        remove_sqlite_files(&path);
    }

    struct MemSecretStore(Mutex<HashMap<String, String>>);

    impl MemSecretStore {
        fn new() -> Self {
            Self(Mutex::new(HashMap::new()))
        }

        fn contains(&self, id: &str) -> bool {
            self.0.lock().unwrap().contains_key(id)
        }
    }

    impl secret::SecretStore for MemSecretStore {
        fn set_secret(&self, id: &str, secret: &secret::SecretString) -> anyhow::Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert(id.to_owned(), secret.expose().to_owned());
            Ok(())
        }

        fn get_secret(&self, id: &str) -> anyhow::Result<secret::SecretString> {
            let value = self
                .0
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing secret: {id}"))?;
            Ok(secret::SecretString::new(value))
        }

        fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().remove(id);
            Ok(())
        }

        fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
            Ok(self.contains(id))
        }
    }

    #[test]
    fn physical_credential_delete_commits_metadata_before_exact_bundle_cleanup() {
        let path = temp_db_path("physical-credential-delete");
        let db = storage::Db::open(&path).unwrap();
        let store = MemSecretStore::new();
        let logical = secret::LogicalCredentialId::new(uuid::Uuid::new_v4().to_string()).unwrap();
        let plan = secret::SecretBundleStagePlan::allocate(logical.clone(), None).unwrap();
        db.register_physical_secret_slot_staging(logical.as_str(), plan.new_slot().as_str())
            .unwrap();
        let access = secret::SecretString::new("sk-test-boundary-secret".to_owned());
        secret::stage_secret_bundle(
            &store,
            &plan,
            secret::SecretBundleRef::new(&access, None, None),
        )
        .unwrap();
        let meta = storage::CredentialMeta {
            id: logical.as_str().to_owned(),
            provider: "test".to_owned(),
            label: "unit".to_owned(),
            credential_kind: "api_key".to_owned(),
            masked_hint: Some(secret::masked_hint(access.expose())),
            workspace_id: Some("ws-test".to_owned()),
        };
        db.insert_credential_with_secret_slot(&meta, plan.new_slot().as_str(), None)
            .unwrap();

        let rows = db.list_credentials().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(store.contains(plan.new_slot().as_str()));

        delete_settings_credential(&db, &store, logical.as_str()).unwrap();
        assert!(db.list_credentials().unwrap().is_empty());
        assert!(!store.contains(plan.new_slot().as_str()));
        assert!(
            db.physical_secret_slots_for_reconciliation(1)
                .unwrap()
                .is_empty()
        );

        drop(db);
        remove_sqlite_files(&path);
    }

    #[test]
    fn startup_secret_migration_is_physical_complete_and_idempotent() {
        let path = temp_db_path("startup-secret-migration");
        let db = storage::Db::open(&path).unwrap();
        let store = MemSecretStore::new();
        let logical_id = uuid::Uuid::new_v4().to_string();
        let access = secret::SecretString::new("legacy-access-secret".to_owned());
        let refresh = secret::SecretString::new("legacy-refresh-secret".to_owned());
        let dcr = secret::SecretString::new("legacy-dcr-secret".to_owned());
        secret::SecretStore::set_secret(&store, &logical_id, &access).unwrap();
        secret::SecretStore::set_secret(&store, &auth::refresh_entry_id(&logical_id), &refresh)
            .unwrap();
        secret::SecretStore::set_secret(&store, &auth::dcr_secret_entry_id(&logical_id), &dcr)
            .unwrap();
        db.insert_credential(&storage::CredentialMeta {
            id: logical_id.clone(),
            provider: "legacy".to_owned(),
            label: "migration".to_owned(),
            credential_kind: "oauth_token".to_owned(),
            masked_hint: Some(secret::masked_hint(access.expose())),
            workspace_id: None,
        })
        .unwrap();

        reconcile_and_migrate_startup_secrets(&db, &store).unwrap();
        let location = db.credential_secret_location(&logical_id).unwrap().unwrap();
        let logical = secret::LogicalCredentialId::new(logical_id.clone()).unwrap();
        let physical = secret::PhysicalSecretSlot::parse(location.keyring_username).unwrap();
        assert!(physical.belongs_to(&logical));
        assert!(store.contains(physical.as_str()));
        assert!(store.contains(&physical.refresh_entry_id()));
        assert!(store.contains(&physical.dcr_entry_id()));
        assert!(!store.contains(&logical_id));
        assert!(!store.contains(&auth::refresh_entry_id(&logical_id)));
        assert!(!store.contains(&auth::dcr_secret_entry_id(&logical_id)));
        let ledger = db.physical_secret_slots_for_reconciliation(1).unwrap();
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].state, storage::PhysicalSecretSlotState::Published);
        assert!(ledger[0].legacy_cleanup_username.is_none());

        reconcile_and_migrate_startup_secrets(&db, &store).unwrap();
        assert_eq!(
            db.credential_secret_location(&logical_id)
                .unwrap()
                .unwrap()
                .keyring_username,
            physical.as_str()
        );
        assert_eq!(
            db.physical_secret_slots_for_reconciliation(1)
                .unwrap()
                .len(),
            1
        );

        drop(db);
        remove_sqlite_files(&path);
    }

    #[test]
    fn workspace_path_to_tree_root는_빈_경로를_desktop_fallback하지_않는다() {
        assert_eq!(App::workspace_path_to_tree_root(None), None);
        assert_eq!(App::workspace_path_to_tree_root(Some(String::new())), None);
        assert_eq!(
            App::workspace_path_to_tree_root(Some("   \t ".to_owned())),
            None
        );
        assert_eq!(
            App::workspace_path_to_tree_root(Some(" /tmp/project ".to_owned())),
            Some(PathBuf::from(" /tmp/project "))
        );
    }

    #[test]
    fn approval_wake_hub는_생성만으로_thread나_repaint를_만들지_않는다() {
        let path = temp_db_path("approval-lazy");
        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel();
        ctx.set_request_repaint_callback(move |info| {
            let _ = tx.send(info.delay);
        });

        let db = storage::Db::open(&path).unwrap();
        let owner = Arc::new(db.acquire_pending_approval_owner().unwrap());
        let hub = ApprovalWakeHub::new(path, ctx, owner);
        assert!(hub.slot.is_none());
        assert!(hub.is_idle());
        assert!(rx.try_recv().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn approval_wake_hub는_ready_race중_stop도_즉시_join한다() {
        let path = temp_db_path("approval-stop-race");
        let db = storage::Db::open(&path).unwrap();
        let owner = Arc::new(db.acquire_pending_approval_owner().unwrap());
        let mut hub = ApprovalWakeHub::new(path.clone(), egui::Context::default(), owner);
        hub.ensure_started().unwrap();
        let started = std::time::Instant::now();
        hub.stop();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        drop(db);
        remove_sqlite_files(&path);
    }

    #[cfg(unix)]
    #[test]
    fn approval_wake_hub는_published_socket이_unlink돼도_stop한다() {
        let path = temp_db_path("approval-stop-unlinked");
        let db = storage::Db::open(&path).unwrap();
        let owner = Arc::new(db.acquire_pending_approval_owner().unwrap());
        let mut hub = ApprovalWakeHub::new(path.clone(), egui::Context::default(), owner);
        hub.ensure_started().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let socket_path = loop {
            if let Some(path) = hub.poll_ready().unwrap() {
                break path;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        std::fs::remove_file(&socket_path).unwrap();
        let started = std::time::Instant::now();
        hub.stop();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        drop(db);
        remove_sqlite_files(&path);
    }

    #[cfg(unix)]
    #[test]
    fn approval_wake_hub는_datagram_후에만_bounded_snapshot을_발행한다() {
        let path = temp_db_path("approval-wake");
        let db = storage::Db::open(&path).unwrap();
        let owner = Arc::new(db.acquire_pending_approval_owner().unwrap());
        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel();
        ctx.set_request_repaint_callback(move |info| {
            let _ = tx.send(info.delay);
        });
        let mut hub = ApprovalWakeHub::new(path.clone(), ctx, owner);
        hub.ensure_started().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let socket_path = loop {
            if let Some(path) = hub.poll_ready().unwrap() {
                break path;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "listener ready timeout"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        while hub.take_snapshot().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "initial snapshot timeout"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        while rx.try_recv().is_ok() {}

        db.insert_pending_approval("req-1", "srv", "tool", "{}", None, 100, None)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(hub.take_snapshot().is_none());
        assert!(rx.try_recv().is_err());

        let socket = std::os::unix::net::UnixDatagram::unbound().unwrap();
        assert_eq!(
            socket
                .send_to(&[APPROVAL_WAKE_MARKER], &socket_path)
                .unwrap(),
            1
        );
        let snapshot = loop {
            if let Some(snapshot) = hub.take_snapshot() {
                break snapshot;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "wake snapshot timeout"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        };

        assert_eq!(snapshot.rows.len(), 1);
        assert_eq!(snapshot.rows[0].id(), "req-1");
        hub.enqueue(ApprovalWorkerCommand::Resolve {
            id: "req-1".to_owned(),
            allowed: false,
            remember: false,
            resolved_at: 101,
        })
        .unwrap();
        let mut resolved = false;
        loop {
            let _ = hub.poll_ready().unwrap();
            resolved |= hub
                .drain_results()
                .into_iter()
                .any(|result| matches!(result, ApprovalWorkerResult::Resolved));
            if hub
                .take_snapshot()
                .is_some_and(|snapshot| snapshot.rows.is_empty())
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "resolve snapshot timeout"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        if !resolved {
            resolved = hub
                .drain_results()
                .into_iter()
                .any(|result| matches!(result, ApprovalWorkerResult::Resolved));
        }
        assert!(resolved);
        assert!(hub.is_idle());
        hub.stop();
        assert!(!socket_path.exists());
        drop(db);
        remove_sqlite_files(&path);
    }

    #[cfg(unix)]
    #[test]
    fn approval_wake_hub는_outstanding_8개를_넘지않고_full_result에서도_stop한다() {
        let path = temp_db_path("approval-result-cap");
        let db = storage::Db::open(&path).unwrap();
        let owner = Arc::new(db.acquire_pending_approval_owner().unwrap());
        let mut hub = ApprovalWakeHub::new(path.clone(), egui::Context::default(), owner);
        hub.ensure_started().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while hub.poll_ready().unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        for index in 0..APPROVAL_COMMAND_CAP {
            hub.enqueue(ApprovalWorkerCommand::Resolve {
                id: format!("missing-{index}"),
                allowed: false,
                remember: false,
                resolved_at: 100,
            })
            .unwrap();
        }
        assert_eq!(hub.inflight, APPROVAL_COMMAND_CAP);
        assert_eq!(
            hub.enqueue(ApprovalWorkerCommand::Resolve {
                id: "overflow".to_owned(),
                allowed: false,
                remember: false,
                resolved_at: 100,
            }),
            Err(ApprovalWakeErrorCode::Backpressure)
        );
        std::thread::sleep(std::time::Duration::from_millis(30));
        let started = std::time::Instant::now();
        hub.stop();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        drop(db);
        remove_sqlite_files(&path);
    }

    #[test]
    fn approval_launch_tracker는_same_config_fifo와_session_cleanup을_보존한다() {
        let now = std::time::Instant::now();
        let mut tracker = ApprovalLaunchTracker::default();
        let first = tracker
            .reserve("ws".to_owned(), "agent".to_owned(), now)
            .unwrap();
        let second = tracker
            .reserve("ws".to_owned(), "agent".to_owned(), now)
            .unwrap();
        assert!(tracker.mark_spawn_sent(first, now));
        assert!(tracker.correlate("ws", "agent", Some(runtime::SessionId(41))));
        assert_eq!(
            tracker.live.get(&("ws".to_owned(), runtime::SessionId(41))),
            Some(&first)
        );
        assert_eq!(
            tracker.pending.front().map(|ticket| ticket.id),
            Some(second)
        );

        assert!(tracker.observe_session_exit("ws", runtime::SessionId(41)));
        assert_eq!(
            tracker.pending_denials(now),
            vec![("ws".to_owned(), runtime::SessionId(41))]
        );
        tracker.mark_deny_queued("ws", runtime::SessionId(41));
        assert!(tracker.pending_denials(now).is_empty());
        assert_eq!(
            tracker.finish_session("ws", runtime::SessionId(41), true, now),
            None
        );
        assert!(
            !tracker
                .live
                .contains_key(&("ws".to_owned(), runtime::SessionId(41)))
        );
    }

    #[test]
    fn approval_launch_tracker는_8개_cap과_deadline을_강제한다() {
        let now = std::time::Instant::now();
        let mut tracker = ApprovalLaunchTracker::default();
        for index in 0..APPROVAL_LAUNCH_CAP {
            tracker
                .reserve("ws".to_owned(), format!("agent-{index}"), now)
                .unwrap();
        }
        assert_eq!(
            tracker.reserve("ws".to_owned(), "overflow".to_owned(), now),
            Err(ApprovalWakeErrorCode::Backpressure)
        );
        let expired = tracker.expire(now + APPROVAL_SPAWN_DEADLINE);
        assert_eq!(expired.len(), APPROVAL_LAUNCH_CAP);
        assert!(tracker.is_empty());
    }

    #[test]
    fn approval_launch_tracker는_spawn_sent_ticket을_deadline후에도_상관시킨다() {
        let now = std::time::Instant::now();
        let mut tracker = ApprovalLaunchTracker::default();
        let ticket = tracker
            .reserve("ws".to_owned(), "agent".to_owned(), now)
            .unwrap();
        assert!(tracker.mark_spawn_sent(ticket, now));
        assert!(
            tracker
                .expire(now + APPROVAL_SPAWN_DEADLINE + std::time::Duration::from_secs(1))
                .is_empty()
        );
        assert!(tracker.correlate("ws", "agent", Some(runtime::SessionId(42))));
        assert_eq!(
            tracker.live.get(&("ws".to_owned(), runtime::SessionId(42))),
            Some(&ticket)
        );
    }

    #[test]
    fn approval_launch_tracker는_denial실패를_bounded_backoff한다() {
        let now = std::time::Instant::now();
        let mut tracker = ApprovalLaunchTracker::default();
        let ticket = tracker
            .reserve("ws".to_owned(), "agent".to_owned(), now)
            .unwrap();
        assert!(tracker.mark_spawn_sent(ticket, now));
        assert!(tracker.correlate("ws", "agent", Some(runtime::SessionId(43))));
        assert!(tracker.observe_session_exit("ws", runtime::SessionId(43)));
        tracker.mark_deny_queued("ws", runtime::SessionId(43));
        assert_eq!(
            tracker.finish_session("ws", runtime::SessionId(43), false, now),
            Some(std::time::Duration::from_secs(1))
        );
        for _ in 0..300 {
            assert!(tracker.pending_denials(now).is_empty());
        }
        assert_eq!(
            tracker.pending_denials(now + std::time::Duration::from_secs(1)),
            vec![("ws".to_owned(), runtime::SessionId(43))]
        );
        assert_eq!(tracker.denial_retries.len(), 1);
        assert_eq!(tracker.live.len(), 1);
    }

    #[test]
    fn settings_snapshot_worker는_요청전까지_thread를_만들지_않는다() {
        let path = temp_db_path("settings-lazy");
        let ctx = egui::Context::default();
        let worker = SettingsSnapshotWorker::new(path, secret::RedactionService::new(), ctx);
        assert!(worker.slot.is_none());
    }

    #[test]
    fn quick_agent_launch는_고정_config와_선택옵션을_준비한다() {
        let path = temp_db_path("quick-agent-launch");
        let db = Db::open(&path).unwrap();
        let workspace_id = db.create_workspace("quick-launch").unwrap();
        let snapshot = crate::agent_launcher::DetectionSnapshot::from_test_agents([(
            crate::agent_launcher::AgentKind::Codex,
            PathBuf::from("/usr/local/bin/codex"),
        )]);
        let spec = crate::agent_launcher::build_launch_spec(
            snapshot
                .find(crate::agent_launcher::AgentKind::Codex)
                .unwrap(),
            crate::agent_launcher::LaunchOptions {
                model: "gpt-5.4".to_owned(),
                effort: Some(crate::agent_launcher::ReasoningEffort::XHigh),
                yolo: true,
            },
            Some(Path::new("/tmp/deppy/shims/codex")),
        )
        .unwrap();

        let prepared =
            prepare_quick_agent_launch(&db, &workspace_id, spec, workspace_id.clone()).unwrap();
        assert_eq!(prepared.runtime_workspace_id, workspace_id);
        assert_eq!(prepared.agent_config_id, "deppy-builtin-codex");
        #[cfg(unix)]
        {
            assert_eq!(prepared.command, "/bin/sh");
            assert_eq!(prepared.args[0], "-c");
            assert_eq!(prepared.args[2], "deppy-agent-session");
            assert_eq!(prepared.args[3], "/tmp/deppy/shims/codex");
            assert_eq!(
                prepared.args[4..],
                [
                    "--dangerously-bypass-approvals-and-sandbox",
                    "--model",
                    "gpt-5.4",
                    "--config",
                    "model_reasoning_effort=\"xhigh\"",
                ]
            );
        }
        #[cfg(not(unix))]
        {
            assert_eq!(prepared.command, "/tmp/deppy/shims/codex");
            assert_eq!(
                prepared.args,
                [
                    "--dangerously-bypass-approvals-and-sandbox",
                    "--model",
                    "gpt-5.4",
                    "--config",
                    "model_reasoning_effort=\"xhigh\"",
                ]
            );
        }
        assert_eq!(
            prepared.env_plain,
            [(
                "DEPPY_AGENT_EXECUTABLE".to_owned(),
                "/usr/local/bin/codex".to_owned()
            )]
        );
        let configs = db.list_agent_configs().unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].id, "deppy-builtin-codex");
        let agents = load_agents_snapshot(&db, 1, &workspace_id);
        assert!(agents.agents().is_empty());
        drop(db);
        remove_sqlite_files(&path);
    }

    #[test]
    fn launcher_spawn_correlation은_workspace와_config가_모두_맞을때만_소비한다() {
        let mut pending = Some(PendingAgentLauncherLaunch {
            request_id: 7,
            workspace_id: "workspace-a".to_owned(),
            agent_config_id: "deppy-builtin-codex".to_owned(),
        });
        assert!(
            take_matching_agent_launcher_launch(&mut pending, "workspace-b", "deppy-builtin-codex")
                .is_none()
        );
        assert!(pending.is_some());
        assert!(
            take_matching_agent_launcher_launch(
                &mut pending,
                "workspace-a",
                "deppy-builtin-claude"
            )
            .is_none()
        );
        assert_eq!(
            take_matching_agent_launcher_launch(&mut pending, "workspace-a", "deppy-builtin-codex")
                .map(|launch| launch.request_id),
            Some(7)
        );
        assert!(pending.is_none());
    }

    #[test]
    fn workspace_shutdown_registry는_two_slot을넘지않고_join한다() {
        let mut registry = PendingShutdownRegistry::default();
        let mut releases = Vec::new();
        for index in 0..PENDING_SHUTDOWN_LIMIT {
            assert!(registry.can_start());
            let (release, wait) = std::sync::mpsc::sync_channel::<()>(0);
            releases.push(release);
            let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let completed_in_thread = Arc::clone(&completed);
            let handle = std::thread::spawn(move || {
                let _ = wait.recv();
                completed_in_thread.store(true, std::sync::atomic::Ordering::Release);
            });
            registry.register(format!("workspace-{index}"), completed, handle);
        }
        assert!(!registry.can_start());
        assert_eq!(registry.entries.len(), PENDING_SHUTDOWN_LIMIT);

        drop(releases);
        registry.join_all();
        assert!(registry.can_start());
        assert!(registry.entries.is_empty());
    }

    #[test]
    fn workspace_shutdown_registry는_finished_slot을_reap하고_drop에서_join한다() {
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed_in_thread = Arc::clone(&completed);
        let handle = std::thread::spawn(move || {
            completed_in_thread.store(true, std::sync::atomic::Ordering::Release);
        });
        let mut registry = PendingShutdownRegistry::default();
        registry.register("finished".to_owned(), Arc::clone(&completed), handle);
        while !registry.entries[0]
            .completed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            std::thread::yield_now();
        }
        assert!(completed.load(std::sync::atomic::Ordering::Acquire));
        assert!(registry.reap_finished());
        assert!(registry.entries.is_empty());

        let joined = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let joined_in_thread = Arc::clone(&joined);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(5));
            joined_in_thread.store(true, std::sync::atomic::Ordering::Release);
        });
        registry.register("drop-join".to_owned(), Arc::clone(&joined), handle);
        drop(registry);
        assert!(joined.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn workspace_shutdown_completion은_wake전에_flag를게시한다() {
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed_in_callback = Arc::clone(&observed);
        let completed_in_callback = Arc::clone(&completed);
        let ctx = egui::Context::default();
        ctx.set_request_repaint_callback(move |_| {
            observed_in_callback.store(
                completed_in_callback.load(std::sync::atomic::Ordering::Acquire),
                std::sync::atomic::Ordering::Release,
            );
        });

        drop(PendingShutdownCompletion {
            completed,
            wake: ctx,
        });

        assert!(observed.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn env_aux_workers는_constructor에서_thread나_io를_시작하지_않는다() {
        let path = temp_db_path("env-aux-construction");
        let ctx = egui::Context::default();
        let mut projects = new_env_project_rows_worker(path.clone(), ctx.clone());
        let mut secrets = new_env_secret_reveal_worker(path, ctx);
        assert!(!projects.has_live_worker());
        assert!(!secrets.has_live_worker());
    }

    #[test]
    fn env_project_invalidation은_inflight를보존하고_stale완료가해제한다() {
        let source = include_str!("app.rs");
        let invalidate_start = source.find("    fn invalidate_env_api_projects(").unwrap();
        let invalidate_end = source[invalidate_start..]
            .find("\n    fn invalidate_env_profile_ui(")
            .map(|offset| invalidate_start + offset)
            .unwrap();
        let invalidate = &source[invalidate_start..invalidate_end];
        assert!(!invalidate.contains("env_project_rows_in_flight"));

        let poll_start = source.find("    fn poll_env_api_project_rows(").unwrap();
        let poll_end = source[poll_start..]
            .find("\n    fn update_workspace_folder_name(")
            .map(|offset| poll_start + offset)
            .unwrap();
        let poll = &source[poll_start..poll_end];
        assert!(poll.contains("self.env_project_rows_in_flight.take()"));
        assert!(poll.contains("self.env_project_rows_in_flight = Some(generation)"));
    }

    #[test]
    fn app_ui는_aux_worker를_직접_admit하지_않는다() {
        let source = include_str!("app.rs");
        let ui_start = source.find("    fn ui(&mut self, ui:").unwrap();
        let tests_start = source[ui_start..]
            .find("\n#[cfg(test)]\nmod tests")
            .map(|offset| ui_start + offset)
            .unwrap();
        let ui_body = &source[ui_start..tests_start];
        assert!(!ui_body.contains(".try_request("));
        assert!(!ui_body.contains("env_api_project_rows_cached"));
        assert!(!source.contains(&["ENV_API_", "PROJECTS_TTL"].concat()));
        assert!(!source.contains(&["from_millis", "(25)"].concat()));
    }

    #[test]
    fn settings_snapshot_worker는_idle_exit_race와_queued_result를_유실하지_않는다() {
        let path = temp_db_path("settings-idle-race");
        let db = storage::Db::open(&path).unwrap();
        let workspace_id = db.create_workspace("workspace").unwrap();
        drop(db);
        let mut worker = SettingsSnapshotWorker::new(
            path.clone(),
            secret::RedactionService::new(),
            egui::Context::default(),
        );

        for index in 0..24u64 {
            let slot = worker.spawn_slot_with_idle_ttl(std::time::Duration::from_millis(2));
            if index % 2 == 1 {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            let job = SettingsJob {
                generation: 1,
                revision: index + 1,
                workspace_id: workspace_id.clone(),
                project_root: None,
                action: SettingsJobAction::Load,
            };
            let accepted = match SettingsSnapshotWorker::try_send_to_slot(&slot, job) {
                Ok(()) => true,
                Err(SettingsTrySendError::Disconnected(_)) => false,
                Err(SettingsTrySendError::Full(_)) => panic!("fresh slot queue was full"),
            };
            let SettingsWorkerSlot {
                tx,
                rx,
                handle,
                lifecycle,
            } = slot;
            if accepted {
                let outcome = rx
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .expect("accepted settings job must produce one result");
                assert_eq!(outcome.revision, index + 1);
            }
            drop(tx);
            drop(rx);
            handle.join().unwrap();
            assert_eq!(
                *lifecycle
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()),
                SettingsWorkerLifecycle::Exited
            );
        }

        let slot = worker.spawn_slot_with_idle_ttl(std::time::Duration::from_millis(2));
        assert!(
            SettingsSnapshotWorker::try_send_to_slot(
                &slot,
                SettingsJob {
                    generation: 1,
                    revision: 99,
                    workspace_id,
                    project_root: None,
                    action: SettingsJobAction::Load,
                },
            )
            .is_ok()
        );
        worker.slot = Some(slot);
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert_eq!(worker.try_recv().map(|outcome| outcome.revision), Some(99));
        remove_sqlite_files(&path);
    }

    #[test]
    fn settings_workspace_find_or_create는_exact_path를_재사용한다() {
        let db_path = temp_db_path("settings-workspace-find-create");
        let workspace_path = temp_db_path("settings-workspace-directory").with_extension("dir");
        std::fs::create_dir_all(&workspace_path).unwrap();
        let mut db = storage::Db::open(&db_path).unwrap();
        let request_workspace = db.create_workspace("request").unwrap();

        let first = execute_settings_job(
            &mut db,
            &db_path,
            &secret::RedactionService::new(),
            SettingsJob {
                generation: 1,
                revision: 1,
                workspace_id: request_workspace.clone(),
                project_root: None,
                action: SettingsJobAction::FindOrCreateWorkspace {
                    name: "project".to_owned(),
                    path: workspace_path.clone(),
                    purpose: WorkspaceMutationPurpose::SwitchRuntime,
                },
            },
        );
        let first_id = match first.kind {
            SettingsOutcomeKind::WorkspaceFoundOrCreated {
                purpose: WorkspaceMutationPurpose::SwitchRuntime,
                result: Ok(result),
            } => {
                assert!(result.created);
                result.row.id
            }
            _ => panic!("unexpected first workspace outcome"),
        };

        let second = execute_settings_job(
            &mut db,
            &db_path,
            &secret::RedactionService::new(),
            SettingsJob {
                generation: 1,
                revision: 2,
                workspace_id: request_workspace,
                project_root: None,
                action: SettingsJobAction::FindOrCreateWorkspace {
                    name: "ignored".to_owned(),
                    path: workspace_path.clone(),
                    purpose: WorkspaceMutationPurpose::SelectInSettings,
                },
            },
        );
        match second.kind {
            SettingsOutcomeKind::WorkspaceFoundOrCreated {
                purpose: WorkspaceMutationPurpose::SelectInSettings,
                result: Ok(result),
            } => {
                assert!(!result.created);
                assert_eq!(result.row.id, first_id);
            }
            _ => panic!("unexpected second workspace outcome"),
        }

        drop(db);
        remove_sqlite_files(&db_path);
        std::fs::remove_dir_all(workspace_path).unwrap();
    }

    #[test]
    fn settings_project_path는_committed_projection을_반환한다() {
        let db_path = temp_db_path("settings-project-path");
        let project_path = temp_db_path("settings-project-path-directory").with_extension("dir");
        std::fs::create_dir_all(&project_path).unwrap();
        let mut db = storage::Db::open(&db_path).unwrap();
        let workspace_id = db.create_workspace("workspace").unwrap();

        let outcome = execute_settings_job(
            &mut db,
            &db_path,
            &secret::RedactionService::new(),
            SettingsJob {
                generation: 1,
                revision: 1,
                workspace_id: workspace_id.clone(),
                project_root: None,
                action: SettingsJobAction::SetProjectPath {
                    path: project_path.clone(),
                },
            },
        );
        match outcome.kind {
            SettingsOutcomeKind::ProjectPathSet(Ok(row)) => {
                assert_eq!(row.id, workspace_id);
                assert_eq!(row.path, project_path.to_string_lossy());
                assert!(row.folder_anchor.is_some());
            }
            _ => panic!("unexpected project path outcome"),
        }

        drop(db);
        remove_sqlite_files(&db_path);
        std::fs::remove_dir_all(project_path).unwrap();
    }

    #[test]
    fn settings_inactive_dotenv_resync는_worker_job에서_영속한다() {
        let db_path = temp_db_path("settings-inactive-dotenv");
        let project_path = temp_db_path("settings-inactive-dotenv-directory").with_extension("dir");
        std::fs::create_dir_all(&project_path).unwrap();
        std::fs::write(project_path.join(".env"), "PLAIN_VALUE=hello\n").unwrap();
        let mut db = storage::Db::open(&db_path).unwrap();
        let workspace_id = db.create_workspace("workspace").unwrap();

        let outcome = execute_settings_job(
            &mut db,
            &db_path,
            &secret::RedactionService::new(),
            SettingsJob {
                generation: 1,
                revision: 1,
                workspace_id: workspace_id.clone(),
                project_root: Some(project_path.clone()),
                action: SettingsJobAction::ResyncDotenv,
            },
        );
        assert!(matches!(
            outcome.kind,
            SettingsOutcomeKind::DotenvResynced(Ok(()))
        ));
        let profile = db
            .list_env_profiles(&workspace_id)
            .unwrap()
            .into_iter()
            .find(|profile| profile.kind == crate::dotenv_sync::DOTENV_PROFILE_KIND)
            .unwrap();
        assert!(db.list_env_vars(&profile.id).unwrap().iter().any(|row| {
            row.key == "PLAIN_VALUE" && row.value == crate::env::EnvValue::Plain("hello".to_owned())
        }));

        drop(db);
        remove_sqlite_files(&db_path);
        std::fs::remove_dir_all(project_path).unwrap();
    }

    #[test]
    fn proxy_config는_ready_listener_socket을_항상_포함한다() {
        let root = std::env::temp_dir().join(format!(
            "deppy-proxy-config-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let db_path = root.join("data.sqlite3");
        let socket_path = PathBuf::from("/tmp/deppy-approval-test.sock");
        let config_path = write_mcp_proxy_config(
            "/tmp/deppy-mcp-proxy",
            &db_path,
            "agent-1",
            "server-1",
            &socket_path,
        )
        .unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(config_path).unwrap()).unwrap();
        let args = value["mcpServers"]["deppy-proxy"]["args"]
            .as_array()
            .unwrap();
        let notify_position = args
            .iter()
            .position(|value| value == "--approval-notify-socket")
            .unwrap();
        assert_eq!(
            args[notify_position + 1],
            socket_path.to_string_lossy().as_ref()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn coalesce_mux_updated_pending_replay_caps_exact_boundary_and_reports_overflow() {
        let mut exact = vec![mux_event("old")];
        exact.extend((0..PENDING_REPLAY_EVENT_CAP - 1).map(|idx| {
            runtime::RuntimeEvent::SpawnFailed {
                kind: runtime::SpawnKind::Shell,
                message: runtime::MessagePayload::new(format!("spawn.failed.{idx}")),
            }
        }));
        exact.push(mux_event("latest"));

        let exact_result = coalesce_mux_updated(&mut exact);

        assert!(!exact_result.overflowed);
        assert_eq!(exact.len(), PENDING_REPLAY_EVENT_CAP);
        assert_eq!(mux_tag(&exact[0]), Some("latest"));

        let mut events = vec![mux_event("old")];
        events.extend((0..PENDING_REPLAY_EVENT_CAP).map(|idx| {
            runtime::RuntimeEvent::SpawnFailed {
                kind: runtime::SpawnKind::Shell,
                message: runtime::MessagePayload::new(format!("spawn.failed.{idx}")),
            }
        }));
        events.push(mux_event("latest"));

        let result = coalesce_mux_updated(&mut events);

        assert!(result.overflowed);
        assert!(events.len() <= PENDING_REPLAY_EVENT_CAP);
        assert_eq!(mux_tag(&events[0]), Some("latest"));
    }

    #[test]
    fn pending_replay_resync_flag_waits_for_successful_activation() {
        let mut render_active = false;
        let mut pending_replay_resync = true;

        clear_pending_replay_resync_after_activation(&mut pending_replay_resync, false, true);
        assert!(!render_active);
        assert!(pending_replay_resync);

        render_active = true;
        clear_pending_replay_resync_after_activation(&mut pending_replay_resync, true, false);
        assert!(render_active);
        assert!(pending_replay_resync);

        clear_pending_replay_resync_after_activation(&mut pending_replay_resync, true, true);
        assert!(render_active);
        assert!(!pending_replay_resync);
    }

    #[test]
    fn pending_replay_hidden_active_applies_required_state_before_cap_discards_replay() {
        let session_ids: Vec<u64> = (1..=(PENDING_REPLAY_EVENT_CAP + 1) as u64).collect();
        let target = runtime::SessionId(session_ids[0]);
        let mut events = vec![live_mux_event("latest", &session_ids)];
        events.extend(session_ids.iter().copied().map(|session| {
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(session),
                exit_code: Some(if session == target.0 { 1 } else { 0 }),
            }
        }));
        let mut workspace_ui = ui::workspace::WorkspaceUi::new();
        let mut pending_events = Vec::new();

        let result =
            admit_hidden_active_replay_events(&mut workspace_ui, &mut pending_events, events);

        assert!(result.overflowed);
        assert_eq!(pending_events.len(), PENDING_REPLAY_EVENT_CAP);
        assert_eq!(mux_tag(&pending_events[0]), Some("latest"));
        assert_eq!(
            workspace_ui.last_session_status(target),
            Some(runtime::SessionStatus::Error)
        );
        assert!(
            !pending_events.iter().any(|event| matches!(
                event,
                runtime::RuntimeEvent::SessionExited { session, .. } if *session == target
            )),
            "target exit replay should be dropped by the hard cap"
        );
    }

    #[test]
    fn coalesce_mux_updated_pending_replay_preserves_latest_live_session_state_after_churn() {
        let session = runtime::SessionId(7);
        let stale_session = runtime::SessionId(9);
        let mut events = vec![live_mux_event("old", &[session.0, stale_session.0])];
        events.extend((0..PENDING_REPLAY_EVENT_CAP).map(|idx| {
            runtime::RuntimeEvent::SpawnFailed {
                kind: runtime::SpawnKind::Agent,
                message: runtime::MessagePayload::new(format!("agent.failed.{idx}")),
            }
        }));
        events.extend([
            runtime::RuntimeEvent::SessionStatusChanged {
                session,
                status: runtime::SessionStatus::Running,
            },
            runtime::RuntimeEvent::SessionStatusChanged {
                session,
                status: runtime::SessionStatus::Waiting,
            },
            runtime::RuntimeEvent::SessionStatusChanged {
                session: stale_session,
                status: runtime::SessionStatus::Waiting,
            },
            viewport_event(session.0, "final"),
            viewport_event(stale_session.0, "stale"),
            runtime::RuntimeEvent::SessionExited {
                session,
                exit_code: Some(0),
            },
            runtime::RuntimeEvent::SessionExited {
                session: stale_session,
                exit_code: Some(1),
            },
            live_mux_event("latest", &[session.0]),
        ]);

        let result = coalesce_mux_updated(&mut events);

        assert!(result.overflowed);
        assert!(events.len() <= PENDING_REPLAY_EVENT_CAP);
        assert_eq!(mux_tag(&events[0]), Some("latest"));
        assert!(events.iter().any(|event| matches!(
            event,
            runtime::RuntimeEvent::SessionStatusChanged {
                session: changed,
                status: runtime::SessionStatus::Waiting,
            } if *changed == session
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            runtime::RuntimeEvent::Viewport { session: changed, snapshot, .. }
                if *changed == session && snapshot.title.as_deref() == Some("final")
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            runtime::RuntimeEvent::SessionExited {
                session: exited,
                exit_code: Some(0),
            } if *exited == session
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            runtime::RuntimeEvent::SessionStatusChanged { session, .. }
                | runtime::RuntimeEvent::Viewport { session, .. }
                | runtime::RuntimeEvent::SessionExited { session, .. }
                if *session == stale_session
        )));
    }

    #[test]
    fn coalesce_moves_latest_mux_to_front() {
        let mut events = vec![
            live_mux_event("a", &[1]),
            runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(1),
                status: runtime::SessionStatus::Running,
            },
            live_mux_event("b", &[1]),
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(1),
                exit_code: None,
            },
            live_mux_event("c", &[1]),
        ];

        coalesce_mux_updated(&mut events);

        // 최신 mux(c)만 남아 맨 앞으로. 이전 mux(a, b) 제거. lifecycle는 순서 보존.
        assert_eq!(events.len(), 3);
        assert_eq!(mux_tag(&events[0]), Some("c"));
        assert!(matches!(
            events[1],
            runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(1),
                status: runtime::SessionStatus::Running,
            }
        ));
        assert!(matches!(
            events[2],
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(1),
                exit_code: None,
            }
        ));
    }

    #[test]
    fn coalesce_dedups_status_per_session() {
        // 같은 세션의 status churn → 세션별 최신 1개만. 유지분 상대 순서 보존.
        let mut events = vec![
            runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(1),
                status: runtime::SessionStatus::Running,
            },
            runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(1),
                status: runtime::SessionStatus::Waiting,
            },
            runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(2),
                status: runtime::SessionStatus::Running,
            },
        ];

        coalesce_mux_updated(&mut events);

        assert_eq!(events.len(), 2);
        // 세션1은 최신(Waiting)만, 세션2는 그대로. 순서: 세션1 → 세션2.
        assert!(matches!(
            events[0],
            runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(1),
                status: runtime::SessionStatus::Waiting,
            }
        ));
        assert!(matches!(
            events[1],
            runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(2),
                status: runtime::SessionStatus::Running,
            }
        ));
    }

    #[test]
    fn coalesce_dedups_status_view_per_session() {
        let view = |status| {
            runtime::SessionStatusView::detected(
                status,
                runtime::StatusSource::StreamRegex,
                None,
                None,
            )
        };
        let mut events = vec![
            runtime::RuntimeEvent::SessionStatusViewChanged {
                session: runtime::SessionId(1),
                view: view(runtime::SessionStatus::Running),
            },
            runtime::RuntimeEvent::SessionStatusViewChanged {
                session: runtime::SessionId(1),
                view: view(runtime::SessionStatus::Waiting),
            },
        ];

        coalesce_mux_updated(&mut events);

        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            runtime::RuntimeEvent::SessionStatusViewChanged {
                session: runtime::SessionId(1),
                view
            } if view.status == runtime::SessionStatus::Waiting
        ));
    }

    #[test]
    fn coalesce_dedups_resource_usage_to_latest() {
        let mut events = vec![
            resource_event(1),
            runtime::RuntimeEvent::ShellSpawned {
                session: runtime::SessionId(7),
            },
            resource_event(2),
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(7),
                exit_code: Some(0),
            },
        ];

        coalesce_mux_updated(&mut events);

        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[0],
            runtime::RuntimeEvent::ShellSpawned {
                session: runtime::SessionId(7)
            }
        ));
        assert!(matches!(
            &events[1],
            runtime::RuntimeEvent::ResourceUsage { snapshot, .. }
                if snapshot.sampled_at_ms == 2
        ));
        assert!(matches!(
            &events[2],
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(7),
                exit_code: Some(0)
            }
        ));
    }

    #[test]
    fn coalesce_dedups_input_pressure_per_session() {
        let mut events = vec![
            input_pressure_event(1, 10),
            input_pressure_event(2, 20),
            input_pressure_event(1, 30),
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(1),
                exit_code: Some(0),
            },
        ];

        coalesce_mux_updated(&mut events);

        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[0],
            runtime::RuntimeEvent::PtyInputPressure { session, pressure }
                if *session == runtime::SessionId(2) && pressure.queued_bytes == 20
        ));
        assert!(matches!(
            &events[1],
            runtime::RuntimeEvent::PtyInputPressure { session, pressure }
                if *session == runtime::SessionId(1) && pressure.queued_bytes == 30
        ));
        assert!(matches!(
            &events[2],
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(1),
                exit_code: Some(0)
            }
        ));
    }

    #[test]
    fn coalesce_dedups_viewport_per_session() {
        let mut events = vec![
            viewport_event(1, "old"),
            viewport_event(2, "other"),
            viewport_event(1, "latest"),
        ];

        coalesce_mux_updated(&mut events);

        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            runtime::RuntimeEvent::Viewport { session, snapshot, .. }
                if *session == runtime::SessionId(2)
                    && snapshot.title.as_deref() == Some("other")
        ));
        assert!(matches!(
            &events[1],
            runtime::RuntimeEvent::Viewport { session, snapshot, .. }
                if *session == runtime::SessionId(1)
                    && snapshot.title.as_deref() == Some("latest")
        ));
    }

    #[test]
    fn coalesce_exit_stays_after_mux_for_replay() {
        // [MuxUpdated(세션X 도입), SessionExited(X)] → coalesce 후에도 exit이 mux 뒤에.
        // (mux가 맨 앞으로 가므로 replay 시 X를 먼저 확립하고 exit이 적용됨.)
        let mut events = vec![
            live_mux_event("x", &[9]),
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(9),
                exit_code: Some(0),
            },
        ];

        coalesce_mux_updated(&mut events);

        assert_eq!(events.len(), 2);
        assert_eq!(mux_tag(&events[0]), Some("x"));
        assert!(matches!(
            events[1],
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(9),
                exit_code: Some(0),
            }
        ));
    }

    #[test]
    fn coalesce_noop_without_mux() {
        let mut events = vec![
            runtime::RuntimeEvent::ShellSpawned {
                session: runtime::SessionId(7),
            },
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(7),
                exit_code: Some(0),
            },
        ];
        coalesce_mux_updated(&mut events);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn warm_auto_suspend_candidates_respect_timeout_and_order() {
        let now = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(60);
        let ids = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let mut backgrounded = HashMap::new();
        backgrounded.insert("a".to_owned(), now - std::time::Duration::from_secs(61));
        backgrounded.insert("b".to_owned(), now - std::time::Duration::from_secs(59));
        backgrounded.insert("c".to_owned(), now - std::time::Duration::from_secs(120));

        assert_eq!(
            expired_warm_workspace_ids(&ids, |id| backgrounded.get(id).copied(), now, timeout),
            vec!["a".to_owned(), "c".to_owned()]
        );
    }

    #[test]
    fn warm_idle_deadline은_변경될때만_one_shot_repaint를_예약한다() {
        let now = std::time::Instant::now();
        let deadline = now + std::time::Duration::from_secs(60);
        assert_eq!(
            changed_deadline_repaint_delay(None, Some(deadline), now),
            Some(std::time::Duration::from_secs(60))
        );
        assert_eq!(
            changed_deadline_repaint_delay(Some(deadline), Some(deadline), now),
            None
        );
        assert_eq!(
            changed_deadline_repaint_delay(Some(deadline), None, now),
            None
        );
        assert_eq!(
            changed_deadline_repaint_delay(
                None,
                Some(now - std::time::Duration::from_secs(1)),
                now,
            ),
            Some(std::time::Duration::ZERO)
        );
    }

    #[test]
    fn global_terminal_cache_budget_is_divided_across_resident_runtimes() {
        const MIB: usize = 1024 * 1024;
        assert_eq!(per_runtime_cache_budget_bytes(128, 1), 128 * MIB);
        assert_eq!(per_runtime_cache_budget_bytes(128, 2), 64 * MIB);
        assert_eq!(per_runtime_cache_budget_bytes(128, 4), 32 * MIB);

        let share = per_runtime_cache_budget_bytes(128, 3);
        assert!(share * 3 <= 128 * MIB);
        assert!((128 * MIB) - share * 3 < 3, "나눗셈 나머지만 미배정");

        // 방어적 0 count는 active runtime 하나로 취급하고, 비정상 0MB도 1MiB로 제한한다.
        assert_eq!(per_runtime_cache_budget_bytes(32, 0), 32 * MIB);
        assert_eq!(per_runtime_cache_budget_bytes(0, 12), MIB);
    }

    #[test]
    fn 폭주_에피소드는_샘플로_갱신되고_해소되면_제거된다() {
        let session = runtime::SessionId(1);
        let sample = |process_count, sampled_at_ms| runtime::SessionResourceUsage {
            session,
            pid: Some(1),
            process_group: Some(1),
            identity_source: runtime::ProcessIdentitySource::PortablePty,
            sampled_at_ms,
            process_count,
            rss_bytes: 0,
            cpu_percent: None,
            high_cpu: false,
            high_rss: false,
        };
        let mut episodes = std::collections::HashMap::new();
        let mut next_id = 0;
        let mut notify = Vec::new();
        update_storm_episodes(
            &mut episodes,
            &mut next_id,
            &mut notify,
            &[sample(5_417, 0)],
        );
        assert!(episodes.get(&session).is_some_and(|e| !e.confirmed));
        assert!(notify.is_empty(), "확정 전에는 알림 큐가 비어 있다");
        update_storm_episodes(
            &mut episodes,
            &mut next_id,
            &mut notify,
            &[sample(5_417, 6_000)],
        );
        assert!(episodes.get(&session).is_some_and(|e| e.confirmed));
        assert_eq!(notify, vec![(session, 5_417)], "확정 전이 1회 알림 적재");
        // 확정 후 임계 미만 — 히스테리시스 창 안에서는 유지, 지속되면 제거.
        update_storm_episodes(
            &mut episodes,
            &mut next_id,
            &mut notify,
            &[sample(1, 10_000)],
        );
        assert!(episodes.contains_key(&session));
        update_storm_episodes(
            &mut episodes,
            &mut next_id,
            &mut notify,
            &[sample(1, 16_000)],
        );
        assert!(!episodes.contains_key(&session));
        assert_eq!(notify.len(), 1, "해소 구간에서는 추가 알림 없음");
    }

    #[test]
    fn 압박_알림은_세션_유무에_따라_문구가_달라진다() {
        let i18n = load_catalog("en-US");
        let (summary, with) = memory_pressure_notification(&i18n, Some("web-remote"));
        assert!(!summary.is_empty());
        assert!(with.contains("web-remote"));
        let (_, without) = memory_pressure_notification(&i18n, None);
        assert!(!without.contains("web-remote"));
        assert_ne!(with, without);
    }

    #[test]
    fn only_sampled_single_process_low_cpu_shells_are_auto_suspendable() {
        let s1 = runtime::SessionId(1);
        let s2 = runtime::SessionId(2);
        let sample = |session, process_count, cpu_percent| runtime::SessionResourceUsage {
            session,
            pid: Some(session.0 as u32),
            process_group: Some(session.0 as u32),
            identity_source: runtime::ProcessIdentitySource::PortablePty,
            sampled_at_ms: 1,
            process_count,
            rss_bytes: 1024,
            cpu_percent,
            high_cpu: false,
            high_rss: false,
        };
        let sessions = [s1, s2];
        let idle = [sample(s1, 1, Some(0.0)), sample(s2, 1, Some(0.5))];
        assert!(shell_sessions_are_idle(&sessions, &idle, |_| false));
        assert!(!shell_sessions_are_idle(&sessions, &idle, |s| s == s2));
        assert!(!shell_sessions_are_idle(&sessions, &idle[..1], |_| false));

        let child_work = [sample(s1, 2, Some(0.0)), sample(s2, 1, Some(0.0))];
        assert!(!shell_sessions_are_idle(&sessions, &child_work, |_| false));
        let busy = [sample(s1, 1, Some(1.1)), sample(s2, 1, Some(0.0))];
        assert!(!shell_sessions_are_idle(&sessions, &busy, |_| false));
        let unsampled_cpu = [sample(s1, 1, None), sample(s2, 1, Some(0.0))];
        assert!(!shell_sessions_are_idle(&sessions, &unsampled_cpu, |_| {
            false
        }));
    }

    #[test]
    fn warm_eviction은_live_workspace를_건너뛴다() {
        let ids = vec![
            "a".to_owned(),
            "b".to_owned(),
            "c".to_owned(),
            "d".to_owned(),
        ];
        // 상한 2, 초과 2 — 가장 오래된 a부터 고르되 live(a, c)는 건너뛴다
        let live: std::collections::HashSet<&str> = ["a", "c"].into();
        assert_eq!(
            warm_eviction_candidates(&ids, 2, |id| live.contains(id)),
            vec!["b".to_owned(), "d".to_owned()]
        );
        // 전부 live면 아무것도 축출하지 않는다 (상한 초과 허용 — 작업 보호)
        assert_eq!(
            warm_eviction_candidates(&ids, 2, |_| true),
            Vec::<String>::new()
        );
        // 초과 없음 → 빈 결과
        assert_eq!(
            warm_eviction_candidates(&ids, 4, |_| false),
            Vec::<String>::new()
        );
        // live 아닌 것이 초과분보다 많아도 초과분만큼만 축출
        assert_eq!(
            warm_eviction_candidates(&ids, 3, |_| false),
            vec!["a".to_owned()]
        );
    }

    #[test]
    fn workspace_is_live는_spawn대기와_초기유예를_존중한다() {
        let d = std::time::Duration::from_secs;
        // tracker가 live면 무조건 live
        assert!(workspace_is_live(true, true, 0, d(999)));
        // spawn 응답 대기 중이면 live (mux 관측과 무관)
        assert!(workspace_is_live(false, true, 1, d(999)));
        // 첫 MuxUpdated 관측 전 + 유예 내 → live (restore 이벤트 미도착 창)
        assert!(workspace_is_live(false, false, 0, d(1)));
        // 유예가 지나면 빈 workspace로 취급 — suspend 가능
        assert!(!workspace_is_live(false, false, 0, d(11)));
        // mux 관측 후 세션 없음 → suspend 가능
        assert!(!workspace_is_live(false, true, 0, d(1)));
    }

    #[test]
    fn live_warm_예상치는_기존_target_전환과_신규_전환을_구분한다() {
        assert_eq!(projected_live_warm_count(4, true, true), 4);
        assert_eq!(projected_live_warm_count(4, false, true), 5);
        assert_eq!(projected_live_warm_count(4, false, false), 4);
    }

    #[test]
    fn live_세션_추적은_mux와_exited를_반영한다() {
        use std::sync::Arc;
        let mut tracker = LiveSessionTracker::default();
        assert!(!tracker.has_live(), "빈 workspace는 live 아님");

        let s1 = runtime::SessionId(1);
        let mux = |sessions: &[runtime::SessionId]| runtime::RuntimeEvent::MuxUpdated {
            snapshot: Arc::new(runtime::MuxSnapshot {
                tabs: vec![runtime::TabSnapshot {
                    id: runtime::MuxTabId::new(),
                    title: "t".into(),
                    layout: runtime::LayoutNode::Pane(runtime::MuxPaneId::new()),
                    panes: sessions
                        .iter()
                        .map(|s| runtime::PaneSnapshot {
                            id: runtime::MuxPaneId::new(),
                            session_id: Some(*s),
                            title: "p".into(),
                            persistent_session_id: None,
                        })
                        .collect(),
                }],
                active_tab: None,
                focused_pane: None,
            }),
        };

        // 세션 attach → live
        tracker.observe(&mux(&[s1]));
        assert!(tracker.has_live());
        assert!(
            tracker.live_shell_sessions().is_none(),
            "spawn 종류 확인 전은 보호"
        );
        tracker.observe(&runtime::RuntimeEvent::ShellSpawned { session: s1 });
        assert_eq!(tracker.live_shell_sessions(), Some(vec![s1]));

        // Exited → live 아님 (pane은 남아 있어도 프로세스는 죽음 — agent 결과 pane)
        tracker.observe(&runtime::RuntimeEvent::SessionExited {
            session: s1,
            exit_code: Some(0),
        });
        assert!(!tracker.has_live());
        assert!(tracker.live_shell_sessions().is_none());

        // pane 제거 MuxUpdated → exited 집합도 정리(유계)
        tracker.observe(&mux(&[]));
        assert!(tracker.exited_sessions.is_empty());
        assert!(!tracker.has_live());
    }

    #[test]
    fn agent가_섞인_live_session은_idle_shell_suspend에서_제외된다() {
        use std::sync::Arc;
        let shell = runtime::SessionId(1);
        let agent = runtime::SessionId(2);
        let mut tracker = LiveSessionTracker::default();
        tracker.observe(&runtime::RuntimeEvent::MuxUpdated {
            snapshot: Arc::new(runtime::MuxSnapshot {
                tabs: vec![runtime::TabSnapshot {
                    id: runtime::MuxTabId::new(),
                    title: "t".into(),
                    layout: runtime::LayoutNode::Pane(runtime::MuxPaneId::new()),
                    panes: [shell, agent]
                        .into_iter()
                        .map(|session| runtime::PaneSnapshot {
                            id: runtime::MuxPaneId::new(),
                            session_id: Some(session),
                            title: "p".into(),
                            persistent_session_id: None,
                        })
                        .collect(),
                }],
                active_tab: None,
                focused_pane: None,
            }),
        });
        tracker.observe(&runtime::RuntimeEvent::ShellSpawned { session: shell });
        tracker.observe(&runtime::RuntimeEvent::AgentSpawned { session: agent });
        assert!(tracker.has_live());
        assert!(tracker.live_shell_sessions().is_none());
    }

    /// PR-A2 회귀 방지: 재시작 시 archived 복원된 세션은 SessionRestored로 오고,
    /// SessionExited와 동일하게 생존 추적에서 제외돼야 한다 (그러지 않으면 복원된
    /// agent pane 때문에 auto-suspend/warm 축출이 영구 무력화됨 — codex 리뷰 P1).
    #[test]
    fn session_restored도_live_추적에서_제외된다() {
        use std::sync::Arc;
        let mut tracker = LiveSessionTracker::default();
        let s1 = runtime::SessionId(1);
        let mux = |sessions: &[runtime::SessionId]| runtime::RuntimeEvent::MuxUpdated {
            snapshot: Arc::new(runtime::MuxSnapshot {
                tabs: vec![runtime::TabSnapshot {
                    id: runtime::MuxTabId::new(),
                    title: "t".into(),
                    layout: runtime::LayoutNode::Pane(runtime::MuxPaneId::new()),
                    panes: sessions
                        .iter()
                        .map(|s| runtime::PaneSnapshot {
                            id: runtime::MuxPaneId::new(),
                            session_id: Some(*s),
                            title: "p".into(),
                            persistent_session_id: None,
                        })
                        .collect(),
                }],
                active_tab: None,
                focused_pane: None,
            }),
        };
        // 복원된 pane(세션 있음) — SessionExited 없이 SessionRestored만 온다
        tracker.observe(&mux(&[s1]));
        assert!(
            tracker.has_live(),
            "SessionRestored 관측 전에는 live로 보임"
        );
        tracker.observe(&runtime::RuntimeEvent::SessionRestored {
            session: s1,
            exit_code: Some(0),
        });
        assert!(
            !tracker.has_live(),
            "복원된 exited 세션은 live 아님 — auto-suspend 정상 동작"
        );
    }

    #[test]
    fn 종료한_워크스페이스는_사이드바와_홈의_공통_visible_set에서_빠진다() {
        let mut closed = std::collections::HashMap::new();
        closed.insert("closed".to_owned(), ClosedWorkspaceState::Persisted);

        assert!(!workspace_visible_after_close(&closed, "closed"));
        assert!(workspace_visible_after_close(&closed, "open"));
    }

    #[test]
    fn 설정_워크스페이스_선택은_숨김과_active를_변경하지_않는다() {
        let row = |id: &str| storage::WorkspaceRow {
            id: id.to_owned(),
            name: id.to_owned(),
            path: format!("/projects/{id}"),
            created_at: String::new(),
        };
        let workspaces = vec![row("open"), row("closed")];
        let active = "open".to_owned();
        let closed = std::collections::HashMap::from([(
            "closed".to_owned(),
            ClosedWorkspaceState::Persisted,
        )]);

        let selected =
            resolve_settings_workspace_id(&workspaces, Some("closed"), Some("open"), &active);

        assert_eq!(selected.as_deref(), Some("closed"));
        assert_eq!(active, "open", "설정 선택은 active runtime을 바꾸지 않는다");
        assert!(
            closed.contains_key("closed"),
            "설정 선택은 sidebar 숨김 표식을 해제하지 않는다"
        );
    }

    #[test]
    fn 환경_프로젝트_닫기는_설정목록만_숨기고_sidebar와_active를_유지한다() {
        let project = |id: &str| ui::env_project_list::EnvProjectRow {
            id: id.to_owned(),
            name: id.to_owned(),
            alias: String::new(),
            path: format!("/projects/{id}"),
            path_missing: false,
            env_count: 0,
            key_count: 0,
        };
        let visible = vec![project("SKRT"), project("mjm")];
        let mut hidden = std::collections::BTreeSet::new();
        let active = "mjm".to_owned();
        let sidebar_closed: std::collections::HashMap<String, ClosedWorkspaceState> =
            std::collections::HashMap::new();

        let next = close_settings_env_project(&mut hidden, &visible, "mjm", Some("mjm"));

        assert_eq!(next.as_deref(), Some("SKRT"));
        assert_eq!(hidden, std::collections::BTreeSet::from(["mjm".to_owned()]));
        assert_eq!(
            active, "mjm",
            "Environment 닫기는 active runtime을 바꾸지 않는다"
        );
        assert!(
            sidebar_closed.is_empty(),
            "Environment 닫기는 sidebar 종료 표식을 만들지 않는다"
        );
        assert_eq!(visible.len(), 2, "workspace 원본 목록은 삭제되지 않는다");

        let only_remaining = vec![project("SKRT")];
        let kept = close_settings_env_project(&mut hidden, &only_remaining, "SKRT", Some("SKRT"));
        assert_eq!(kept, None);
        assert!(
            hidden.contains("SKRT"),
            "마지막 프로젝트도 설정 목록에서 닫힌다"
        );
        assert_eq!(
            resolve_settings_env_project_id(&[], None, kept.as_deref(), &active),
            None,
            "빈 Environment 목록은 active workspace로 폴백하지 않는다"
        );
        assert_eq!(active, "mjm", "마지막 닫기도 active runtime을 유지한다");
        assert!(
            sidebar_closed.is_empty(),
            "마지막 닫기도 sidebar를 유지한다"
        );
    }

    #[test]
    fn 영속_종료는_stale_pane으로_재노출되지_않고_현재종료만_새_pane을_인식한다() {
        let persisted = ClosedWorkspaceState::Persisted;
        assert!(!persisted.should_auto_reveal(["stale-pane"]));

        let closing =
            ClosedWorkspaceState::ClosingPanes(["closing-pane".to_owned()].into_iter().collect());
        assert!(!closing.should_auto_reveal(["closing-pane"]));
        assert!(closing.should_auto_reveal(["closing-pane", "new-pane"]));
    }

    #[test]
    fn 사용자_세션_시작은_영속_종료_표식을_메모리와_config에서_함께_해제한다() {
        let mut closed = std::collections::HashMap::from([(
            "closed".to_owned(),
            ClosedWorkspaceState::Persisted,
        )]);
        let mut persisted = std::collections::BTreeSet::from(["closed".to_owned()]);

        assert!(clear_closed_workspace_state(
            &mut closed,
            &mut persisted,
            "closed"
        ));
        assert!(!closed.contains_key("closed"));
        assert!(!persisted.contains("closed"));
        assert!(!clear_closed_workspace_state(
            &mut closed,
            &mut persisted,
            "closed"
        ));
    }

    #[test]
    fn agent_resume_mapping은_프로세스가_아니라_pane_생존기준으로_정리한다() {
        let live_panes = std::collections::HashSet::from(["claude-pane".to_owned()]);
        let stale = stale_agent_session_panes(["claude-pane", "deleted-pane"], &live_panes);

        // claude 프로세스가 잠시 없더라도 pane이 남으면 mapping을 유지하고,
        // 실제 layout에서 사라진 pane의 mapping만 삭제한다.
        assert_eq!(stale, vec!["deleted-pane".to_owned()]);
    }

    #[test]
    fn 자동_resume은_비어있는_로컬_셸에만_허용한다() {
        assert_eq!(
            auto_resume_decision(false, false, Some(1)),
            AutoResumeDecision::Resume,
            "셸 프로세스 하나만 있을 때만 자동 이어가기"
        );
        assert_eq!(
            auto_resume_decision(false, false, Some(2)),
            AutoResumeDecision::MarkHandled,
            "shell+ssh 같은 다른 작업이 있으면 주입 금지"
        );
        assert_eq!(
            auto_resume_decision(false, false, None),
            AutoResumeDecision::Wait,
            "프로세스 스냅샷이 없으면 안전하게 대기"
        );
        assert_eq!(
            auto_resume_decision(false, false, Some(0)),
            AutoResumeDecision::Wait,
            "불완전한 프로세스 스냅샷도 자동 주입 금지"
        );
    }

    #[test]
    fn 실행중이던_agent가_끝나도_자동_resume하지_않는다() {
        let initial = auto_resume_decision(false, true, Some(2));
        assert_eq!(initial, AutoResumeDecision::MarkHandled);

        // 최초 관측에서 처리 완료로 표시한 뒤 agent가 종료돼 셸만 남더라도 재주입 금지.
        let after_exit = auto_resume_decision(true, false, Some(1));
        assert_eq!(after_exit, AutoResumeDecision::Skip);
    }

    #[test]
    fn 수동_resume_완료도_현재_shell_only_상태만_허용한다() {
        assert!(resume_probe_completion_allowed(true, true, false, Some(1)));
        for process_count in [None, Some(0), Some(2), Some(3)] {
            assert!(!resume_probe_completion_allowed(
                true,
                false,
                false,
                process_count
            ));
        }
        assert!(!resume_probe_completion_allowed(true, false, true, Some(1)));
    }

    #[test]
    fn agent_state_결과예산은_storage_transaction_전에_wrapper를_예약한다() {
        assert_eq!(
            app_agent_state_reserved_bytes(None, None),
            Some(std::mem::size_of::<AppAgentStateSnapshot>())
        );
        assert!(
            crate::agent_state_worker::checked_projection_result_remaining(
                app_agent_state_reserved_bytes(None, None).unwrap()
            )
            .is_ok()
        );
    }

    fn max_valid_structured_mutation(index: usize) -> storage::StructuredThreadMutation {
        let local_session_id = format!("local-{index:04}");
        let workspace_id = "workspace".to_owned();
        let thread_id = format!("thread-{index:04}");
        let cwd = String::new();
        let model = None;
        let fixed_bytes = local_session_id.len() + workspace_id.len() + thread_id.len() + cwd.len();
        let title = "x"
            .repeat(ui::agent_sessions::AGENT_SESSION_PERSISTED_ROW_MAX_BYTES - fixed_bytes)
            .into_boxed_str()
            .into_string();
        storage::StructuredThreadMutation::Upsert(storage::StructuredThreadRow {
            local_session_id,
            workspace_id,
            thread_id,
            title,
            cwd,
            model,
            favorite: false,
            archived: false,
            created_at: 0,
            updated_at: 0,
        })
    }

    #[test]
    fn structured_agent_state는_ui유효최대행을worker_byte_prefix로분할한다() {
        let scope = Arc::new(
            AppAgentStateScope::new(1, "workspace".to_owned(), vec!["workspace".to_owned()])
                .unwrap(),
        );
        let pending = (0..crate::agent_state_worker::AGENT_STATE_STRUCTURED_BATCH_MAX)
            .map(max_valid_structured_mutation)
            .collect::<Vec<_>>();

        let (items, request) = prepare_structured_agent_state_prefix(&scope, &pending).unwrap();
        assert!(items > 0);
        assert!(items < pending.len(), "wrapper and Vec capacity must count");
        assert!(
            request.retained_bytes
                <= crate::agent_state_worker::AGENT_STATE_STRUCTURED_BATCH_BYTES_MAX
        );
        assert!(prepare_structured_agent_state_prefix(&scope, &pending[items..]).is_some());
    }

    #[test]
    fn structured_agent_state는작은행16개를한batch로유지한다() {
        let scope = Arc::new(
            AppAgentStateScope::new(1, "workspace".to_owned(), vec!["workspace".to_owned()])
                .unwrap(),
        );
        let pending = (0..crate::agent_state_worker::AGENT_STATE_STRUCTURED_BATCH_MAX)
            .map(|index| storage::StructuredThreadMutation::Delete {
                local_session_id: format!("local-{index}"),
            })
            .collect::<Vec<_>>();

        let (items, request) = prepare_structured_agent_state_prefix(&scope, &pending).unwrap();
        assert_eq!(
            items,
            crate::agent_state_worker::AGENT_STATE_STRUCTURED_BATCH_MAX
        );
        assert!(
            request.retained_bytes
                <= crate::agent_state_worker::AGENT_STATE_STRUCTURED_BATCH_BYTES_MAX
        );
    }

    #[test]
    fn turn_done_clear는_현재_workspace_scope에만_포함한다() {
        let current = storage::AgentTurnDoneClear {
            session_key: "workspace-a:42".to_owned(),
            seen_at: 7,
        };
        let stale = storage::AgentTurnDoneClear {
            session_key: "workspace-b:42".to_owned(),
            seen_at: 7,
        };
        assert!(turn_done_clear_matches_workspace(&current, "workspace-a"));
        assert!(!turn_done_clear_matches_workspace(&stale, "workspace-a"));
    }
}
