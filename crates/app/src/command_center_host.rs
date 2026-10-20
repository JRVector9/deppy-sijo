//! App-owned Command Center adapter. Rendering never opens the sidecar or sends input.
use super::*;
use crate::fleet::{FleetPromptTarget, FleetTarget};
use crate::ui::command_center as cc;
use crate::work_management::{self as wm, ProjectWorkState};
use crate::workspace_add::WorkspaceAddPurpose;
use deppy_core::WorkspaceId;
use deppy_core::work_management as work;
use std::collections::{BTreeMap, VecDeque};
use storage::work_management as stored;
use unicode_normalization::UnicodeNormalization as _;
use wm::repository_worker::{RepositoryJob, RepositoryResult, RepositoryWorker};
#[path = "command_center_approval.rs"]
mod approval;
#[path = "command_center_assignment.rs"]
mod assignment;
#[path = "command_center_devices.rs"]
mod devices;
#[path = "command_center_launch.rs"]
pub(super) mod launch;
#[path = "command_center_switch.rs"]
pub(super) mod layout;
#[path = "command_center_local_close.rs"]
mod local_close;
#[path = "command_center_observe.rs"]
mod observe;
#[path = "command_center_remote.rs"]
mod remote;
#[path = "command_center_routes.rs"]
mod routes;
#[path = "command_center_server.rs"]
pub(super) mod server;

#[derive(Clone, Debug, PartialEq)]
struct Capture {
    workspace: String,
    target: FleetTarget,
    prompt: Option<FleetPromptTarget>,
}
#[derive(Clone, PartialEq)]
struct LiveRoute {
    capture: Capture,
    title: String,
    agent: Option<work::AgentSelection>,
    state: cc::TaskState,
    cwd: Option<String>,
    watch: Option<String>,
    runtime_binding: Option<work::RuntimeBinding>,
}
#[derive(Clone)]
struct Project {
    snapshot: stored::CommandCenterSnapshot,
    revision: Option<stored::ProjectRevision>,
    generation: u64,
    saved_generation: u64,
    loaded: bool,
    storage_blocked: bool,
    scopes: Vec<String>,
    allowed_agents: Vec<work::AgentSelection>,
    request: Option<wm::plan::PlannerRequest>,
    proposal: Option<wm::plan::ValidatedPlan>,
    confirmed: BTreeMap<String, assignment::Confirmed>,
    ledger: wm::dispatch::DeliveryLedger,
    catalog: Option<launch::Catalog>,
    catalog_requested: bool,
    worktree_root: Option<wm::task_worktree::ProjectWorktreeRootProof>,
    effect_fence: remote::EffectFence,
}
impl Project {
    fn state(&self) -> ProjectWorkState {
        let mut state = ProjectWorkState::new(self.snapshot.config.workspace_id.clone());
        state.goals = self.snapshot.goals.clone();
        state.tasks = self.snapshot.tasks.clone();
        state.attempts = self.snapshot.attempts.clone();
        state
    }
    fn set_state(&mut self, state: ProjectWorkState) {
        self.snapshot.goals = state.goals;
        self.snapshot.tasks = state.tasks;
        self.snapshot.attempts = state.attempts;
        self.dirty();
    }
    fn ack_saved(&mut self, revision: stored::ProjectRevision, generation: u64) {
        self.revision = Some(revision);
        self.saved_generation = generation;
    }
    fn dirty(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }
    fn policy(&self) -> wm::dispatch::ExecutionPolicy {
        wm::dispatch::ExecutionPolicy {
            workspace_id: self.snapshot.config.workspace_id.clone(),
            config_revision: self.snapshot.config.revision,
            project_folder: self.snapshot.config.folder.clone(),
            worktree_root: self
                .worktree_root
                .as_ref()
                .filter(|proof| same_folder(proof.project_folder(), &self.snapshot.config.folder))
                .map(|proof| proof.root().to_owned()),
            allowed_agents: self.allowed_agents.clone(),
            allowed_scope_roots: self.scopes.clone(),
            max_workers: self.snapshot.config.max_workers,
        }
    }
}
struct Input {
    target: cc::RouteTarget,
    draft: String,
    dispatch: Option<wm::dispatch::DispatchAssignment>,
}
struct AfterSave {
    id: String,
    input: Input,
    prompt: String,
    config_revision: work::ConfigRevision,
}
enum Flight {
    Load {
        workspace: String,
        token: u64,
        receipt: Option<String>,
    },
    Save {
        workspace: String,
        generation: u64,
        after: Option<Box<AfterSave>>,
        receipt: Option<String>,
    },
}
pub(super) struct CommandCenterHost {
    pub ui: cc::UiState,
    routes: routes::RouteRegistry<Capture>,
    live: Vec<LiveRoute>,
    projects: BTreeMap<String, Project>,
    repository: RepositoryWorker,
    flight: Option<Flight>,
    next_token: u64,
    requested_loads: BTreeMap<String, String>,
    launches: BTreeMap<String, launch::Pending>,
    remotes: BTreeMap<String, remote::Pending>,
    remote_sessions: remote::sessions::Sessions,
    local_closes: local_close::Closes,
    reconciles: remote::reconcile::Queries,
    worktrees: wm::task_worktree::TaskWorktreeWorker,
    pending_configs: BTreeMap<String, (String, cc::ProjectConfig)>,
    installed: BTreeMap<u64, launch::Installed>,
    execution_bindings: Vec<(Capture, work::RuntimeBinding, work::AgentSelection)>,
    next_grant_epoch: u64,
    observations: observe::Observations,
    devices: devices::Devices,
    pending: VecDeque<(String, cc::Intent)>,
    inputs: BTreeMap<String, Input>,
    /// Local draft edit identity captured when a Submit enters the host queue.
    input_draft_versions: BTreeMap<String, u64>,
    // IDs are never evicted during a host lifetime. Full capacity rejects new operations.
    operations: BTreeMap<String, cc::Intent>,
    remote_permits: BTreeMap<String, Arc<web_remote::cloud::CloudControlPermit>>,
    receipts: VecDeque<cc::RemoteReceipt>,
    status: String,
    published: bool,
    publication_failed: bool,
    publication_dirty: bool,
    approval_revision: (u64, bool),
    generations: Vec<(String, u64, bool)>,
    pending_git: Option<(cc::RouteTarget, ui::git_panel::GitPanelAction)>,
    aux_loading: Option<(cc::RouteTarget, cc::DetailTab)>,
    aux_ready: Option<cc::RouteTarget>,
}
impl CommandCenterHost {
    fn new(data_dir: PathBuf, ctx: &egui::Context) -> Self {
        let wake = ctx.clone();
        Self {
            ui: cc::UiState::default(),
            routes: routes::RouteRegistry::new(uuid::Uuid::new_v4().to_string()),
            live: vec![],
            projects: BTreeMap::new(),
            repository: RepositoryWorker::new(data_dir.clone(), move || wake.request_repaint()),
            flight: None,
            next_token: 0,
            requested_loads: BTreeMap::new(),
            launches: BTreeMap::new(),
            remotes: BTreeMap::new(),
            remote_sessions: Default::default(),
            local_closes: Default::default(),
            reconciles: Default::default(),
            worktrees: {
                let ctx = ctx.clone();
                wm::task_worktree::TaskWorktreeWorker::new(move || ctx.request_repaint())
            },
            pending_configs: BTreeMap::new(),
            installed: BTreeMap::new(),
            execution_bindings: vec![],
            next_grant_epoch: 0,
            observations: observe::Observations::new(ctx),
            devices: devices::Devices::new(data_dir.clone(), ctx),
            pending: VecDeque::new(),
            inputs: BTreeMap::new(),
            input_draft_versions: BTreeMap::new(),
            operations: BTreeMap::new(),
            remote_permits: BTreeMap::new(),
            receipts: VecDeque::new(),
            status: String::new(),
            published: false,
            publication_failed: false,
            publication_dirty: true,
            approval_revision: (0, false),
            generations: vec![],
            pending_git: None,
            aux_loading: None,
            aux_ready: None,
        }
    }
    pub(super) fn retry_publication(&mut self) {
        self.publication_failed = false;
        self.publication_dirty = true;
    }
    fn finish(&mut self, id: &str, status: cc::ReceiptStatus, detail: impl Into<String>) {
        self.remote_permits.remove(id);
        if status != cc::ReceiptStatus::Uncertain {
            self.input_draft_versions.remove(id);
        }
        if let Some(pending) = self.launches.get_mut(id)
            && matches!(pending.stage, launch::Stage::Input)
        {
            pending.record.phase = match status {
                cc::ReceiptStatus::Acknowledged => launch::Phase::Completed,
                cc::ReceiptStatus::Rejected => launch::Phase::Rejected,
                cc::ReceiptStatus::Uncertain => launch::Phase::Unknown,
            };
            pending.stage = launch::Stage::Done;
            if let Some(p) = self.projects.get_mut(&pending.record.workspace.0) {
                let _ = launch::save_record(p, &pending.record);
            }
        }
        let detail = bounded(&detail.into(), 512);
        self.publication_dirty = true;
        self.status = detail.clone();
        if let Some(receipt) = self.receipts.iter_mut().find(|r| r.operation_id == id) {
            receipt.status = status;
            receipt.detail = detail;
        } else {
            if self.receipts.len() == 64 {
                self.receipts.pop_front();
            }
            self.receipts.push_back(cc::RemoteReceipt {
                operation_id: id.into(),
                status,
                detail,
            });
        }
    }
    fn reject(&mut self, id: &str, detail: impl Into<String>) {
        self.finish(id, cc::ReceiptStatus::Rejected, detail);
    }
    fn route(&self, target: &cc::RouteTarget) -> Option<&LiveRoute> {
        let capture = self.routes.get(target.session_id.as_deref()?)?;
        if capture.workspace != target.workspace_id {
            return None;
        }
        self.live.iter().find(|live| &live.capture == capture)
    }
    fn target(&self, live: &LiveRoute) -> cc::RouteTarget {
        cc::RouteTarget {
            workspace_id: live.capture.workspace.clone(),
            session_id: self.routes.key(&live.capture).map(str::to_owned),
            task_id: None,
        }
    }
    fn main_target(&self, ws: &str, project: &Project) -> cc::RouteTarget {
        let session = self.live.iter().find(|live| {
            live.capture.workspace == ws
                && persistent(live).as_ref() == project.snapshot.main_binding.as_ref()
                && project.snapshot.main_binding.is_some()
        });
        session
            .map(|s| self.target(s))
            .unwrap_or_else(|| cc::RouteTarget {
                workspace_id: ws.into(),
                ..Default::default()
            })
    }
    fn request_project_load(&mut self, id: &str, workspace: &str) -> Result<(), String> {
        if remote::reconcile::project_pending(self, workspace) {
            return Err("원래 원격 요청의 상태 조회가 끝난 뒤 저장 상태를 복구해 주세요.".into());
        }
        if self.requested_loads.contains_key(workspace)
            || matches!(&self.flight,Some(Flight::Load {workspace:inflight,..}) if inflight==workspace)
        {
            return Err(
                "이 프로젝트는 이미 불러오는 중입니다. 현재 요청의 응답을 기다려 주세요.".into(),
            );
        }
        if self.requested_loads.len() >= 64 {
            return Err("프로젝트 불러오기 대기 한도입니다.".into());
        }
        self.requested_loads.insert(workspace.into(), id.into());
        Ok(())
    }
    fn enqueue(&mut self, id: String, intent: cc::Intent) {
        if let cc::Intent::DraftChanged { target, .. } = &intent {
            if let Some((_, queued)) = self
                .pending
                .iter_mut()
                .find(|(_, i)| matches!(i,cc::Intent::DraftChanged { target:t,.. } if t == target))
            {
                *queued = intent;
                return;
            }
            if self.pending.len() < 64 {
                self.pending.push_back((id, intent));
            }
            return;
        }
        if matches!(
            intent,
            cc::Intent::SelectProject { .. }
                | cc::Intent::OpenSession(_)
                | cc::Intent::OpenGit(_)
                | cc::Intent::OpenHistory(_)
                | cc::Intent::SetTheme { .. }
                | cc::Intent::UseOriginalLayout
                | cc::Intent::ObserveRemoteSession { .. }
        ) {
            if self.pending.len() < 64 {
                self.pending.push_back((id, intent));
            }
            return;
        }
        if let Some(existing) = self.operations.get(&id) {
            if existing != &intent {
                self.status = "같은 작업 ID에 다른 요청을 사용할 수 없습니다.".into();
            }
            return;
        }
        if self.operations.len() >= 1024 || self.pending.len() >= 64 {
            self.reject(
                &id,
                "요청 기록 한도에 도달했습니다. 미확인 입력을 재전송하지 않았습니다.",
            );
            return;
        }
        if let cc::Intent::Submit { target, .. } = &intent {
            self.input_draft_versions
                .insert(id.clone(), self.ui.draft_version(target));
        }
        self.operations.insert(id.clone(), intent.clone());
        self.pending.push_back((id, intent));
    }
    fn save(
        &mut self,
        ws: &str,
        after: Option<AfterSave>,
        receipt: Option<String>,
    ) -> Result<(), String> {
        if self.flight.is_some() {
            return Err("저장 작업이 진행 중입니다. 잠시 뒤 다시 시도해 주세요.".into());
        }
        let p = self
            .projects
            .get(ws)
            .ok_or("프로젝트를 찾을 수 없습니다.")?;
        if !p.loaded || p.storage_blocked {
            return Err("저장 상태를 먼저 다시 확인해 주세요. 초안은 보존되어 있습니다.".into());
        }
        let generation = p.generation;
        self.repository
            .try_request(RepositoryJob::Save {
                snapshot: Box::new(p.snapshot.clone()),
                expected: p.revision,
                edit_generation: generation,
            })
            .map_err(|_| "저장 작업을 시작하지 못했습니다.".to_owned())?;
        self.flight = Some(Flight::Save {
            workspace: ws.into(),
            generation,
            after: after.map(Box::new),
            receipt,
        });
        Ok(())
    }
}
fn bounded(text: &str, bytes: usize) -> String {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
fn preserve_known_delivery_receipts(
    loaded: &mut stored::CommandCenterSnapshot,
    current: &Project,
) -> bool {
    fn same_origin(a: &work::Attempt, b: &work::Attempt) -> bool {
        let mut a = a.clone();
        let mut b = b.clone();
        for attempt in [&mut a, &mut b] {
            attempt.state = work::WorkState::Queued;
            attempt.last_event_sequence = 0;
        }
        a == b
    }
    let mut merged = false;
    for (assignment, delivery) in &current.ledger.records {
        let terminal = match delivery {
            wm::dispatch::DeliveryState::Admitted => stored::DispatchOutboxState::Admitted,
            wm::dispatch::DeliveryState::Rejected => stored::DispatchOutboxState::Acknowledged,
            _ => continue,
        };
        let identity = |outbox: &stored::DispatchOutbox| {
            outbox.attempt_id == assignment.attempt.id
                && outbox.operation_id == assignment.attempt.operation_id
        };
        if !current
            .snapshot
            .outbox
            .iter()
            .any(|outbox| identity(outbox) && outbox.state == terminal)
        {
            continue;
        }
        let Some(outbox) = loaded.outbox.iter().position(|outbox| {
            identity(outbox)
                && matches!(
                    outbox.state,
                    stored::DispatchOutboxState::Pending | stored::DispatchOutboxState::Uncertain
                )
        }) else {
            continue;
        };
        let Some(task) = loaded
            .tasks
            .iter()
            .position(|task| task.id == assignment.task.id)
        else {
            continue;
        };
        let Some(attempt) = loaded
            .attempts
            .iter()
            .position(|attempt| attempt.id == assignment.attempt.id)
        else {
            continue;
        };
        let Some(current_task) = current
            .snapshot
            .tasks
            .iter()
            .find(|task| task.id == assignment.task.id)
        else {
            continue;
        };
        let Some(current_attempt) = current
            .snapshot
            .attempts
            .iter()
            .find(|attempt| attempt.id == assignment.attempt.id)
        else {
            continue;
        };
        if loaded.tasks[task].current_attempt_id.as_ref() != Some(&assignment.attempt.id)
            || current_task.current_attempt_id.as_ref() != Some(&assignment.attempt.id)
            || !wm::dispatch::same_task_definition(&loaded.tasks[task], &assignment.task)
            || !wm::dispatch::same_task_definition(current_task, &assignment.task)
            || loaded.tasks[task].result != current_task.result
            || !same_origin(&loaded.attempts[attempt], &assignment.attempt)
            || !same_origin(current_attempt, &assignment.attempt)
            || loaded.attempts[attempt].last_event_sequence > current_attempt.last_event_sequence
            || assignment.source_results.iter().any(|source| {
                [&*loaded, &current.snapshot].iter().any(|snapshot| {
                    !snapshot.tasks.iter().any(|task| {
                        task.id == source.task_id && task.result.as_ref() == Some(&source.result)
                    })
                })
            })
        {
            continue;
        }
        // Only a retained terminal receipt for the exact immutable dispatch may
        // survive a failed save followed by an older load. This never arms input.
        loaded.outbox[outbox].state = terminal;
        loaded.tasks[task] = current_task.clone();
        loaded.attempts[attempt] = current_attempt.clone();
        merged = true;
    }
    merged
}
fn finish_project_load(
    host: &mut CommandCenterHost,
    workspace: &str,
    receipt: Option<String>,
    result: Result<Option<Box<stored::StoredProject>>, wm::repository_worker::RepositoryError>,
) {
    let previous_main = host.main_target(workspace, &host.projects[workspace]);
    let result = result.map_err(|_| "저장된 작업 상태를 읽지 못했습니다. 원래 화면은 계속 사용할 수 있습니다.".to_owned())
        .and_then(|mut record| {
            if let Some(record) = &mut record {
                let current = &host.projects[workspace];
                // Validate the entire merge before replacing any project state. In
                // particular, authoring while loading must not silently lose a
                // local main draft when the stored draft collection is full.
                for draft in current.snapshot.drafts.iter().filter(|d| !d.target_key.starts_with("settings:")) {
                    if let Some(existing) = record.snapshot.drafts.iter_mut().find(|d| d.target_key == draft.target_key) {
                        *existing = draft.clone();
                    } else {
                        record.snapshot.drafts.push(draft.clone());
                    }
                }
                if record.snapshot.drafts.len() > work::WORK_DRAFT_MAX_COUNT
                    || record.snapshot.drafts.iter().any(|d| d.text.len() > work::WORK_DRAFT_MAX_BYTES)
                    || record.snapshot.drafts.iter().map(|d| d.text.len()).sum::<usize>() > work::WORK_DRAFT_TOTAL_MAX_BYTES
                {
                    return Err("초안 저장 한도로 불러오기를 완료하지 못했습니다. 현재 초안은 이 화면에 보존됩니다. 저장된 초안의 공간을 확보한 뒤 저장 상태를 다시 확인해 주세요.".into());
                }
                preserve_known_delivery_receipts(&mut record.snapshot, current);
            }
            Ok(record)
        });
    let succeeded = result.is_ok();
    match result {
        Ok(record) => {
            let p = host.projects.get_mut(workspace).unwrap();
            if let Some(record) = record {
                p.revision = Some(record.revision);
                p.snapshot = record.snapshot;
                p.scopes = p
                    .snapshot
                    .drafts
                    .iter()
                    .find(|d| d.target_key == "settings:scopes")
                    .and_then(|d| serde_json::from_str(&d.text).ok())
                    .unwrap_or_default();
                p.allowed_agents = p
                    .snapshot
                    .drafts
                    .iter()
                    .find(|d| d.target_key == "settings:agents")
                    .and_then(|d| serde_json::from_str(&d.text).ok())
                    .unwrap_or_else(|| vec![p.snapshot.config.worker_agent.clone()]);
                recover_observation(&mut p.snapshot);
                p.dirty();
            }
            p.loaded = true;
            p.storage_blocked = false;
            if let Some(goal) = p
                .snapshot
                .goals
                .iter()
                .rev()
                .find(|g| g.state == work::WorkState::PlanReview)
            {
                p.request =
                    wm::plan::PlannerRequest::new(goal, &p.snapshot.config, &p.policy()).ok();
            }
            launch::restore_records(host, workspace);
            remote::restore(host, workspace);
            let target = host.main_target(workspace, host.projects.get(workspace).unwrap());
            if !host.ui.has_draft(&target)
                && !host.ui.has_draft(&previous_main)
                && let Some(draft) = host.projects[workspace]
                    .snapshot
                    .drafts
                    .iter()
                    .find(|d| d.target_key == "main")
            {
                host.ui.set_draft(target, draft.text.clone());
            }
        }
        Err(error) => {
            if let Some(p) = host.projects.get_mut(workspace) {
                p.storage_blocked = true;
            }
            host.status = error;
        }
    }
    if let Some(id) = receipt {
        if succeeded {
            host.finish(
                &id,
                cc::ReceiptStatus::Acknowledged,
                "프로젝트 상태를 불러왔습니다.",
            );
        } else {
            host.reject(&id, host.status.clone());
        }
    }
}

fn yes() -> cc::Capability {
    cc::Capability {
        enabled: true,
        reason: String::new(),
    }
}
fn no(reason: &str) -> cc::Capability {
    cc::Capability {
        enabled: false,
        reason: reason.into(),
    }
}
fn persistent(live: &LiveRoute) -> Option<work::PersistentBinding> {
    match &live.capture.target {
        FleetTarget::Pty { pane, .. } => Some(work::PersistentBinding::Pane {
            pane_id: pane.clone(),
        }),
        FleetTarget::Structured { .. } => None, // a local controller ID is not a persistent provider thread.
    }
}
fn binding(live: &LiveRoute, key: &str) -> Option<work::RuntimeBinding> {
    if let Some(binding) = &live.runtime_binding {
        return Some(binding.clone());
    }
    // Legacy terminal captures use the route incarnation, which is retired for every
    // exact inner execution change. Atomic launches instead carry actual runtime proof.
    let prompt = live.capture.prompt.as_ref()?;
    Some(work::RuntimeBinding {
        incarnation: work::RuntimeIncarnationId::from_string(key.into()).ok()?,
        session_id: prompt.session,
        execution_generation: 1,
    })
}
fn project_snapshot(ws: &storage::WorkspaceRow) -> stored::CommandCenterSnapshot {
    let agent = work::AgentSelection {
        provider: "unbound".into(),
        model: None,
        effort: None,
    };
    stored::CommandCenterSnapshot {
        config: work::ProjectConfig {
            workspace_id: WorkspaceId(ws.id.clone()),
            revision: work::ConfigRevision(1),
            name: App::workspace_display_name(ws),
            folder: ws.path.clone(),
            main_agent: agent.clone(),
            worker_agent: agent,
            max_workers: 3,
            confirm_plan: true,
            use_worktrees: false,
        },
        main_binding: None,
        plan_confirmations: vec![],
        goals: vec![],
        tasks: vec![],
        attempts: vec![],
        events: vec![],
        outbox: vec![],
        drafts: vec![],
    }
}
impl App {
    fn refresh_command_center_routes(&self, host: &mut CommandCenterHost) {
        let old_live = host.live.clone();
        host.live = self
            .build_fleet_sessions(&self.i18n)
            .into_iter()
            .map(|session| {
                let (agent, cwd, watch) = match &session.target {
                    FleetTarget::Pty { session: sid, .. } => {
                        let rt = if session.workspace_id == self.active.id {
                            Some(&self.active)
                        } else {
                            self.warm.get(&session.workspace_id)
                        };
                        let agent = rt
                            .and_then(|rt| self.agent_kinds.get(&(rt.runtime_instance, *sid)))
                            .map(|a| work::AgentSelection {
                                provider: format!("{:?}", a.kind).to_ascii_lowercase(),
                                model: a.model.clone(),
                                effort: a.effort.clone(),
                            });
                        let watch = (session.workspace_id == self.active.id)
                            .then(|| {
                                self.active.workspace_ui.mux().and_then(|mux| {
                                    ui::inbox_waiting::find_persistent_session_id(mux, *sid)
                                })
                            })
                            .flatten();
                        (
                            agent,
                            active_workspace_cwd(
                                &session.workspace_id,
                                &self.active.id,
                                self.cached_session_cwd(*sid),
                            ),
                            watch,
                        )
                    }
                    FleetTarget::Structured { .. } => (None, None, None),
                };
                use crate::agent_surface::AgentVisualState as V;
                let state = match session.state {
                    V::Active => cc::TaskState::Running,
                    V::Waiting => cc::TaskState::WaitingApproval,
                    V::NeedsResponse => cc::TaskState::WaitingInput,
                    V::Error => cc::TaskState::Failed,
                    V::Off => cc::TaskState::ObservationUnknown,
                    V::Idle | V::Complete => cc::TaskState::Queued,
                };
                let capture = Capture {
                    workspace: session.workspace_id,
                    target: session.target,
                    prompt: session.prompt_target,
                };
                let bound = host
                    .execution_bindings
                    .iter()
                    .find(|(owner, _, _)| owner == &capture);
                let agent = bound.map(|(_, _, selection)| selection.clone()).or(agent);
                LiveRoute {
                    runtime_binding: bound.map(|(_, binding, _)| binding.clone()),
                    capture,
                    title: session.title,
                    agent,
                    state,
                    cwd,
                    watch,
                }
            })
            .collect();
        if host.live != old_live {
            host.publication_dirty = true;
        }
        host.routes
            .refresh(host.live.iter().map(|l| l.capture.clone()).collect());
        for ws in self
            .workspaces
            .iter()
            .filter(|ws| workspace_visible_after_close(&self.closed_workspaces, &ws.id))
        {
            host.projects
                .entry(ws.id.clone())
                .or_insert_with(|| Project {
                    snapshot: project_snapshot(ws),
                    revision: None,
                    generation: 0,
                    saved_generation: 0,
                    loaded: false,
                    storage_blocked: false,
                    scopes: vec![],
                    allowed_agents: vec![],
                    request: None,
                    proposal: None,
                    confirmed: BTreeMap::new(),
                    ledger: Default::default(),
                    catalog: None,
                    catalog_requested: false,
                    worktree_root: None,
                    effect_fence: remote::EffectFence::new(),
                });
        }
    }
    pub(crate) fn command_center_snapshot(&self) -> cc::Snapshot {
        let Some(host) = &self.command_center else {
            return cc::Snapshot::default();
        };
        let mut snapshot = cc::Snapshot {
            selected_workspace_id: Some(self.active.id.clone()),
            dark_mode: self.last_theme_dark,
            font_scale: 1.0,
            status: host.status.clone(),
            add_project: yes(),
            manage_devices: yes(),
            receipts: host.receipts.iter().cloned().collect(),
            ..Default::default()
        };
        for (suffix, fallback) in cc::LABELS {
            let key = format!("command_center.{suffix}");
            let translated = self.i18n.t(&key, &[]);
            snapshot.labels.insert(
                key.clone(),
                if translated == key {
                    (*fallback).into()
                } else {
                    translated
                },
            );
        }
        for suffix in ["main", "tasks", "terminal", "devices", "history"] {
            if let Some(label) = snapshot
                .labels
                .get(&format!("command_center.{suffix}"))
                .cloned()
            {
                snapshot
                    .labels
                    .insert(format!("command_center.nav_{suffix}"), label);
            }
        }
        let mut remote_terminal_budget = 128 * 1024;
        let mut approval_budget = 64 * 1024;
        for ws in self
            .workspaces
            .iter()
            .filter(|ws| workspace_visible_after_close(&self.closed_workspaces, &ws.id))
        {
            let Some(p) = host.projects.get(&ws.id) else {
                continue;
            };
            let config = &p.snapshot.config;
            let main_target = host.main_target(&ws.id, p);
            let policy = p.policy();
            let ready = p.loaded && !p.storage_blocked && host.flight.is_none();
            let configured = policy.validate_config(config).is_ok();
            let sessions: Vec<_> = host
                .live
                .iter()
                .filter(|l| l.capture.workspace == ws.id)
                .map(|live| cc::Session {
                    model: live.agent.as_ref().and_then(|a| a.model.clone()),
                    effort: live.agent.as_ref().and_then(|a| a.effort.clone()),
                    terminal_session_id: live.watch.clone(),
                    target: host.target(live),
                    title: live.title.clone(),
                    provider: live
                        .agent
                        .as_ref()
                        .map(|a| a.provider.clone())
                        .unwrap_or_else(|| "App Server".into()),
                    device: "로컬".into(),
                    state: live.state,
                    input: if live.capture.prompt.is_some() {
                        yes()
                    } else {
                        no("현재 실행의 입력 승인 경로가 없습니다.")
                    },
                })
                .collect();
            let main_live = host.route(&main_target);
            let main_input = if !ready {
                no("프로젝트 상태를 저장하거나 불러오는 중입니다.")
            } else if !configured {
                no(
                    "프로젝트 설정에서 실행 공급자와 허용 범위를 지정해 주세요. 워크트리는 준비된 전용 경로가 필요합니다.",
                )
            } else if main_live.is_none_or(|l| {
                l.capture.prompt.is_none() || l.agent.as_ref() != Some(&config.main_agent)
            }) {
                no("프로젝트 설정에서 현재 실행과 일치하는 메인 세션을 명시적으로 연결해 주세요.")
            } else {
                yes()
            };
            let assignment_choices = assignment::choices(p, &host.devices.policies);
            let ready_tasks = p.ready_tasks().unwrap_or_default();
            let mut tasks = vec![];
            for task in &p.snapshot.tasks {
                let attempt = task
                    .current_attempt_id
                    .as_ref()
                    .and_then(|id| p.snapshot.attempts.iter().find(|a| &a.id == id));
                let live = attempt
                    .and_then(|a| a.runtime_binding.as_ref())
                    .and_then(|b| {
                        host.live.iter().find(|l| {
                            host.routes
                                .key(&l.capture)
                                .is_some_and(|key| binding(l, key).as_ref() == Some(b))
                        })
                    });
                let mut target = live
                    .map(|l| host.target(l))
                    .unwrap_or_else(|| cc::RouteTarget {
                        workspace_id: ws.id.clone(),
                        ..Default::default()
                    });
                target.task_id = Some(task.id.0.clone());
                if let Some(remote_target) = remote::target_for(host, task) {
                    target = remote_target;
                }
                let remote_target = remote::sessions::is_target(host, &target);
                let eligible: Vec<_> = host
                    .live
                    .iter()
                    .filter(|l| {
                        task.execution_device.is_none()
                            && l.capture.workspace == ws.id
                            && l.capture.prompt.is_some()
                            && l.agent.as_ref() == Some(&task.assigned_agent)
                            && l.state == cc::TaskState::Queued
                            && l.cwd
                                .as_deref()
                                .is_some_and(|cwd| same_folder(cwd, &config.folder))
                            && persistent(l) != p.snapshot.main_binding
                    })
                    .take(16)
                    .map(|l| cc::Session {
                        target: host.target(l),
                        title: l.title.clone(),
                        provider: task.assigned_agent.provider.clone(),
                        input: yes(),
                        ..Default::default()
                    })
                    .collect();
                let dispatch = ready
                    && p.confirmed_for_task(&task.id).is_some()
                    && !eligible.is_empty()
                    && p.state().can_dispatch(&task.id);
                tasks.push(cc::Task {
                    reconcile_remote:remote::reconcile::capability(host,&target),
                    remote_terminal:remote::sessions::public_terminal(host,&target,&mut remote_terminal_budget),
                    close_worker: if remote_target {remote::sessions::close_cap(host,&target)} else {local_close::capability(host,&target)},
                    selected_assignment: assignment::selected(task, &assignment_choices),
                    create_worker: if task.execution_device.is_some() {
                        if ready && !remote::reconcile::has_idle(host,task) && matches!(p.confirmed_for_task(&task.id),Some(assignment::Confirmed::Mixed(_))) && ready_tasks.contains(&task.id) && !remote::workspace_preparing(host,&ws.id) && !remote::workspace_unknown(host,&ws.id) && task.execution_device.as_ref().is_some_and(|id|host.devices.can_execute(&ws.id,id)) && !config.use_worktrees && host.devices.policies.iter().any(|d|Some(&d.registration().id)==task.execution_device.as_ref() && d.registration().workspace_id.0==ws.id && d.validate_fresh().is_ok()) {yes()} else {no("배정안·기기 권한·실행 용량을 확인해 주세요. 원격 워크트리는 서버 소유 증명이 필요합니다.")}
                    } else if ready
                        && p.confirmed_for_task(&task.id).is_some()
                        && p.state().can_dispatch(&task.id)
                        && (!config.use_worktrees || p.worktree_root.is_some())
                        && !launch::workspace_uncertain(&host.launches, &ws.id)
                        && !host
                            .launches
                            .values()
                            .any(|l| l.record.task_id.as_ref() == Some(&task.id) && !l.finished())
                    {
                        yes()
                    } else {
                        no("배정안 확인과 현재 작업·폴더 상태를 먼저 확인해 주세요.")
                    },
                    attempt_id: attempt.map(|a| a.id.0.clone()).or_else(||remote::reconcile::idle_attempt(host,task)),
                    result_report: if ready && attempt.is_some() && !remote::handoff_pending(host,&target) {
                        yes()
                    } else {
                        no("현재 시도가 없습니다.")
                    },
                    verify_result: if ready && task.result.is_some() && !remote::handoff_pending(host,&target) {
                        yes()
                    } else {
                        no("제출된 결과가 없습니다.")
                    },
                    verification: task
                        .result
                        .as_ref()
                        .map(|r| {
                            match r.verification {
                                work::VerificationState::NotRun => "검증 미실행",
                                work::VerificationState::Passed => "사용자가 수행한 검증 통과",
                                work::VerificationState::Failed => "검증 실패",
                            }
                            .into()
                        })
                        .unwrap_or_default(),
                    terminal_session_id: live.and_then(|l| l.watch.clone()),
                    eligible_sessions: eligible,
                    target: target.clone(),
                    title: bounded(&task.title, 256),
                    role: task.role.clone(),
                    scope: bounded(&task.scope, 1024),
                    acceptance_criteria: bounded(&task.acceptance_criteria, 1024),
                    provider: task.assigned_agent.provider.clone(),
                    device: task
                        .execution_device
                        .clone()
                        .unwrap_or_else(|| "이 컴퓨터".into()),
                    directory: attempt
                        .map(|a| a.folder.clone())
                        .unwrap_or_else(|| task.execution_device.as_ref().map(|id| host.devices.policies.iter().find(|d| d.registration().id==*id && d.registration().workspace_id==task.workspace_id).map(|d| d.registration().folder.clone()).unwrap_or_else(|| "등록한 원격 폴더를 확인해 주세요.".into())).unwrap_or_else(|| config.folder.clone())),
                    state: work_state(task.state),
                    activity: remote::reconcile::activity(host,task).or_else(||remote::sessions::status(host,&target)).unwrap_or_else(||format!("{:?}", task.state)),
                    result: task.result.as_ref().map(|r| bounded(&r.summary, 2048)),
                    result_revision: task.result.as_ref().map(|r| r.revision.0.to_string()),
                    terminal: if live.is_some() || remote_target {
                        yes()
                    } else {
                        no("이 작업에 연결된 현재 실행이 없습니다.")
                    },
                    git: if live.is_some() {
                        yes()
                    } else {
                        no("실행 폴더를 확인할 수 없습니다.")
                    },
                    input: if local_close::target_pending(host,&target){no("현재 워커 종료 응답을 기다리고 있습니다.")}else if remote_target {remote::sessions::input_cap(host,&target)} else if live.is_some_and(|l| l.capture.prompt.is_some()) {
                        yes()
                    } else {
                        no("이 작업에 연결된 입력 대상이 없습니다.")
                    },
                    dispatch: if dispatch {
                        yes()
                    } else {
                        no("계획 확인과 일치하는 유휴 실행 세션이 필요합니다.")
                    },
                    confirm_result: if task.state == work::WorkState::AwaitingUserConfirmation {
                        yes()
                    } else {
                        no("검증을 통과한 결과만 완료 확인할 수 있습니다.")
                    },
                    ..Default::default()
                });
            }
            let mut agents: Vec<_> = host
                .live
                .iter()
                .filter(|l| l.capture.workspace == ws.id)
                .filter_map(|l| l.agent.clone())
                .collect();
            agents.sort_by_key(|a| a.provider.clone());
            agents.dedup();
            if let Some(catalog) = &p.catalog {
                for agent in &catalog.choices {
                    if !agents.contains(agent) {
                        agents.push(agent.clone());
                    }
                }
            }
            for device in
                host.devices.policies.iter().filter(|d| {
                    d.registration().workspace_id.0 == ws.id && d.validate_fresh().is_ok()
                })
            {
                for profile in device.profiles() {
                    if !agents.contains(&profile.agent) {
                        agents.push(profile.agent.clone());
                    }
                }
            }
            let providers = agents.iter().map(|a| a.provider.clone()).collect();
            let models = agents.iter().filter_map(|a| a.model.clone()).collect();
            let efforts = agents.iter().filter_map(|a| a.effort.clone()).collect();
            let main_summary = if !p.loaded {
                "저장된 작업 상태를 불러오는 중입니다.".into()
            } else if let Some(goal) = p.display_goal() {
                let state = p.state();
                let aggregate = wm::orchestrator::aggregate_goal(&state, &goal.id);
                let completion = if state.goal_is_complete(&goal.id) {
                    "모든 결과 확인 완료"
                } else {
                    "진행 중"
                };
                let review = if p.pending_confirmation_goal().is_some() {
                    "배정안 확인 필요 · "
                } else {
                    ""
                };
                format!(
                    "{review}목표: {}\n{completion} · 작업 {} · 결과 제출 {} · 검증 통과 {} · 사용자 확인 {}\n메인 응답은 실제 세션에서 확인합니다. 배정안 JSON을 검증한 뒤 실행을 확인해 주세요.",
                    bounded(&goal.prompt, 4096),
                    aggregate.total,
                    aggregate.results_submitted,
                    aggregate.verification_passed,
                    aggregate.user_confirmed
                )
            } else {
                "메인 응답은 실제 세션에서 확인합니다. 배정안 JSON을 검증한 뒤 실행을 확인해 주세요.".into()
            };
            let planner_mode =
                wm::provider_adapter::planner_mode(wm::provider_adapter::PlannerSupport {
                    structured_final_response: false,
                    tool_execution_restricted: false,
                });
            let history = approval::history(p, &tasks);
            snapshot.projects.push(cc::Project { pending_approvals:self.command_center_project_approvals(host,&ws.id,&mut approval_budget), create_main: if ready && configured && !launch::workspace_uncertain(&host.launches,&ws.id) && !host.launches.values().any(|l| l.record.workspace.0==ws.id && l.record.task_id.is_none() && !l.finished()) {yes()} else {no("프로젝트 실행 설정과 폴더를 먼저 준비해 주세요.")}, loaded: p.loaded, storage_retry: cc::Capability { enabled: p.storage_blocked, reason: "저장 결과를 확인하지 못했습니다. 초안을 보존한 채 저장 상태를 다시 확인합니다.".into() }, manual_plan: if ready && p.request.is_some() && planner_mode == wm::provider_adapter::PlannerMode::ManualJson { yes() } else { no("먼저 목표를 메인에게 전달해 주세요.") }, planner_prompt: p.request.as_ref().and_then(|r| r.to_prompt().ok()), id: ws.id.clone(), name: config.name.clone(), directory: config.folder.clone(), goal: p.display_goal().map(|g| bounded(&g.prompt,4096)).unwrap_or_default(), main_summary, pending_goal_id: p.pending_confirmation_goal().map(|goal| goal.0), plan_confirmation: if ready && p.pending_confirmation_goal().is_some() { yes() } else { no("검증된 배정안이 없습니다.") }, main_target: main_target.clone(), main_provider: config.main_agent.provider.clone(), main_model: config.main_agent.model.clone().unwrap_or_default(), main_effort: config.main_agent.effort.clone().unwrap_or_default(), main_input, sessions, tasks, history, config: cc::ProjectConfig { allowed_scope_roots: p.scopes.clone(), name: config.name.clone(), directory: config.folder.clone(), main_session_id: main_target.session_id, provider: config.main_agent.provider.clone(), model: config.main_agent.model.clone().unwrap_or_default(), effort: config.main_agent.effort.clone().unwrap_or_default(), worker_providers: p.allowed_agents.iter().map(|a| a.provider.clone()).collect(), parallel_limit: config.max_workers, automation: "배정 전 확인".into(), use_worktrees: config.use_worktrees, revision: config.revision.0.to_string() }, save_settings: if ready { yes() } else { no("저장 또는 불러오기가 진행 중입니다.") }, available_agents: agents.iter().map(|a| cc::AgentOption { provider: a.provider.clone(), model: a.model.clone().unwrap_or_default(), effort: a.effort.clone().unwrap_or_default() }).collect(), available_providers: providers, available_models: models, available_efforts: efforts, available_automation: vec!["배정 전 확인".into()] });
        }
        snapshot.devices.push(cc::Device {
            id: "local".into(),
            name: "이 Mac".into(),
            description: "현재 앱의 로컬 실행 세션".into(),
            capabilities: "현재 실행과 입력 승인 경로를 확인한 세션".into(),
            connection: "로컬".into(),
            connected: true,
            can_execute: true,
            revoke: no("로컬 실행 환경은 기기 권한 회수 대상이 아닙니다."),
        });
        host.devices.project(&mut snapshot, &self.active.id);
        snapshot.server_execution = self.command_center_server_snapshot(host);
        if let Some(p) = host.projects.get(&self.active.id) {
            snapshot.execution_setup.assignments = assignment::choices(p, &host.devices.policies);
        }
        snapshot
    }
    pub(super) fn command_center_input_permit(
        &self,
        id: &str,
    ) -> Option<Arc<web_remote::cloud::CloudControlPermit>> {
        self.command_center
            .as_ref()?
            .remote_permits
            .get(id)
            .cloned()
    }
    pub(crate) fn command_center_remote_intent(
        &mut self,
        operation_id: String,
        value: serde_json::Value,
        permit: web_remote::cloud::CloudControlPermit,
    ) {
        if !self.command_center_layout.available || uuid::Uuid::parse_str(&operation_id).is_err() {
            return;
        }
        let Ok(intent) = serde_json::from_value::<cc::Intent>(value) else {
            return;
        };
        if !matches!(
            intent,
            cc::Intent::LoadProject { .. }
                | cc::Intent::ObserveRemoteSession { .. }
                | cc::Intent::Submit { .. }
                | cc::Intent::Approval { .. }
                | cc::Intent::ConfirmResult { .. }
                | cc::Intent::ReportResult { .. }
                | cc::Intent::VerifyResult { .. }
                | cc::Intent::ConfirmPlan { .. }
                | cc::Intent::DispatchTask { .. }
                | cc::Intent::SaveProject { .. }
                | cc::Intent::InstallPlanJson { .. }
        ) {
            return;
        }
        if let Some(host) = self.command_center.as_mut() {
            if permit.is_current()
                && !host.operations.contains_key(&operation_id)
                && host.remote_permits.len() < 64
            {
                host.remote_permits
                    .insert(operation_id.clone(), Arc::new(permit));
                host.enqueue(operation_id, intent);
            }
            self.egui_ctx.request_repaint();
        }
    }
    fn command_center_intent(
        &mut self,
        host: &mut CommandCenterHost,
        id: &str,
        intent: cc::Intent,
    ) -> Result<(), String> {
        // An intent is transactional in memory as well as in the sidecar: a full queue,
        // draft capacity error, or validation failure must not expose partial edits.
        let workspace = intent_workspace(&intent).map(str::to_owned);
        project_transaction(host, workspace.as_deref(), |host| {
            self.apply_command_center_intent(host, id, intent)
        })
    }

    fn apply_command_center_intent(
        &mut self,
        host: &mut CommandCenterHost,
        id: &str,
        intent: cc::Intent,
    ) -> Result<(), String> {
        if host
            .remote_permits
            .get(id)
            .is_some_and(|permit| !permit.is_current())
        {
            return Err(
                "원격 제어 권한이 만료되었거나 취소되었습니다. 요청을 적용하지 않았습니다.".into(),
            );
        }
        match intent {
            cc::Intent::AllowThisComputer {
                workspace_id,
                config_revision,
                exported_workspace_id,
            } => {
                return self.allow_command_center_server(
                    host,
                    id,
                    &workspace_id,
                    &config_revision,
                    &exported_workspace_id,
                );
            }
            cc::Intent::RevokeThisComputer => return self.revoke_command_center_server(host, id),
            cc::Intent::CopyThisComputerConnection => {
                return self.copy_command_center_server_connection(host, id);
            }
            cc::Intent::OpenThisComputerSettings => {
                self.settings_category = ui::settings::Category::RemoteTls;
                self.request_settings_open();
            }
            cc::Intent::SetTaskAssignment {
                target,
                config_revision,
                device_id,
                profile_id,
            } => {
                return self.amend_command_center_assignment(
                    host,
                    id,
                    target,
                    config_revision,
                    device_id,
                    profile_id,
                );
            }
            cc::Intent::CreateMain { workspace_id } => {
                return self.begin_command_center_launch(host, id, &workspace_id, None);
            }
            cc::Intent::CreateWorker { target } => {
                if host
                    .projects
                    .get(&target.workspace_id)
                    .and_then(|p| {
                        p.snapshot
                            .tasks
                            .iter()
                            .find(|t| Some(&t.id.0) == target.task_id.as_ref())
                    })
                    .is_some_and(|t| t.execution_device.is_some())
                {
                    return self.begin_command_center_remote(host, id, &target);
                }
                return self.begin_command_center_launch(
                    host,
                    id,
                    &target.workspace_id,
                    target.task_id.as_deref(),
                );
            }
            cc::Intent::ConnectExecutionDevice {
                workspace_id,
                request_id,
            } => return self.connect_command_center_device(host, id, &workspace_id, request_id),
            cc::Intent::GrantExecutionDevice {
                workspace_id,
                candidate_id,
                profile_ids,
                max_workers,
            } => {
                return self.grant_command_center_device(
                    host,
                    id,
                    &workspace_id,
                    &candidate_id,
                    profile_ids,
                    max_workers,
                );
            }
            cc::Intent::RevokeExecutionDevice {
                workspace_id,
                device_id,
            } => return self.revoke_command_center_device(host, id, &workspace_id, &device_id),
            cc::Intent::LoadProject { workspace_id } => {
                let p = host
                    .projects
                    .get(&workspace_id)
                    .ok_or("프로젝트가 없습니다.")?;
                if !p.loaded {
                    host.request_project_load(id, &workspace_id)?;
                    return Ok(());
                }
            }
            cc::Intent::RetryStorage { workspace_id } => {
                if remote::project_pending(host, &workspace_id) {
                    return Err("원격 실행·입력의 실제 응답을 확인한 뒤 복구해 주세요.".into());
                }
                if host.flight.is_some() {
                    return Err("저장 작업이 진행 중입니다.".into());
                }
                let p = host
                    .projects
                    .get_mut(&workspace_id)
                    .ok_or("프로젝트가 없습니다.")?;
                p.effect_fence.revoke();
                p.effect_fence = remote::EffectFence::new();
                p.storage_blocked = false;
                p.loaded = false;
                p.catalog_requested = false;
                host.request_project_load(id, &workspace_id)?;
                host.publication_dirty = true;
                return Ok(());
            }
            cc::Intent::UseOriginalLayout => self.command_center_layout.use_original(),
            cc::Intent::ObserveRemoteSession { target } => {
                return remote::sessions::request_view(host, id, &target);
            }
            cc::Intent::SetTheme { dark_mode } => {
                self.config.ui.theme = if dark_mode {
                    crate::config::Theme::Dark
                } else {
                    crate::config::Theme::Light
                };
                self.pending_settings_config_apply = true;
            }
            cc::Intent::AddProject => {
                self.command_center_layout.use_original();
                self.open_workspace_add_dialog(WorkspaceAddPurpose::SwitchRuntime);
            }
            cc::Intent::ManageDevices { .. } => {
                host.ui.view = cc::View::Devices;
            }
            cc::Intent::SelectProject { workspace_id } => {
                if !host.projects.contains_key(&workspace_id) {
                    return Err("프로젝트가 더 이상 존재하지 않습니다.".into());
                }
                self.stage_workspace_controller_action(WorkspaceControllerAction::SwitchWorkspace(
                    workspace_id,
                ));
            }
            navigation @ (cc::Intent::OpenSession(_)
            | cc::Intent::OpenGit(_)
            | cc::Intent::OpenHistory(_)) => {
                let (target, tab_kind) = match navigation {
                    cc::Intent::OpenGit(t) => (t, cc::DetailTab::Git),
                    cc::Intent::OpenHistory(t) => (t, cc::DetailTab::History),
                    cc::Intent::OpenSession(t) => (t, cc::DetailTab::Terminal),
                    _ => unreachable!(),
                };
                if remote::sessions::is_target(host, &target) {
                    if tab_kind != cc::DetailTab::Terminal {
                        return Err("원격 Git·이력 조회는 아직 제공되지 않습니다. 실제 터미널 화면을 사용해 주세요.".into());
                    }
                    if target.task_id.is_some() {
                        host.ui.open_task(target);
                        host.ui.detail_tab = cc::DetailTab::Terminal;
                    } else {
                        host.ui.open_terminal(target);
                    }
                    return Ok(());
                }
                let route = host
                    .route(&target)
                    .ok_or("실행 대상이 바뀌었습니다. 현재 세션을 다시 선택해 주세요.")?
                    .clone();
                match &route.capture.target {
                    FleetTarget::Pty { tab, pane, .. } => {
                        self.reveal_terminal_session();
                        self.stage_workspace_controller_action(
                            WorkspaceControllerAction::FocusSession {
                                workspace_id: target.workspace_id.clone(),
                                tab: tab.clone(),
                                pane: pane.clone(),
                            },
                        );
                        if target.task_id.is_some() {
                            host.ui.open_task(target.clone());
                            host.ui.detail_tab = tab_kind;
                        } else {
                            host.ui.open_terminal(target.clone());
                        }
                        if matches!(tab_kind, cc::DetailTab::Git | cc::DetailTab::History) {
                            host.aux_loading = Some((target, tab_kind));
                            host.aux_ready = None;
                        }
                    }
                    FleetTarget::Structured { session_id } => {
                        if self.agent_sessions_ui.open_session(session_id) {
                            self.agent_sessions_ui.open();
                            self.command_center_layout.use_original();
                        }
                    }
                }
            }
            cc::Intent::DraftChanged { target, text } => {
                store_composer_draft(host, target, text)?;
                return Ok(());
            }
            cc::Intent::SaveProject {
                workspace_id,
                config,
            } => {
                if remote::project_pending(host, &workspace_id) {
                    return Err(
                        "원격 실행·입력 승인 대기 중에는 실행 설정을 바꿀 수 없습니다.".into(),
                    );
                }
                if config.allowed_scope_roots.len() > 64
                    || config
                        .allowed_scope_roots
                        .iter()
                        .map(String::len)
                        .sum::<usize>()
                        > 8192
                {
                    return Err("허용 범위는 합계 8KiB, 64개 이하로 지정해 주세요.".into());
                }
                let p = host
                    .projects
                    .get(&workspace_id)
                    .ok_or("프로젝트가 없습니다.")?;
                if !p.loaded || host.flight.is_some() {
                    return Err("저장 중에는 설정을 변경할 수 없습니다.".into());
                }
                if config.use_worktrees && p.worktree_root.is_none() {
                    if host.pending_configs.contains_key(&workspace_id) {
                        return Err("전용 워크트리 폴더를 이미 준비 중입니다.".into());
                    }
                    if !self.queue_global_settings_action(
                        &workspace_id,
                        SettingsJobAction::CommandCenter(launch::SettingsRequest::PrepareRoot {
                            operation_id: id.into(),
                            workspace: workspace_id.clone(),
                            folder: p.snapshot.config.folder.clone(),
                            revision: p.snapshot.config.revision,
                        }),
                    ) {
                        return Err("설정 워커가 사용 중입니다. 잠시 뒤 저장해 주세요.".into());
                    }
                    host.pending_configs
                        .insert(workspace_id, (id.into(), config));
                    host.status = "소유가 검증되는 전용 워크트리 루트를 준비하고 있습니다.".into();
                    return Ok(());
                }
                if config.revision != p.snapshot.config.revision.0.to_string() {
                    return Err("설정이 변경되었습니다. 최신 설정을 다시 열어 주세요.".into());
                }
                // Folder resolution belongs to the existing workspace registration worker.
                let actual_folder = self
                    .workspaces
                    .iter()
                    .find(|ws| ws.id == workspace_id)
                    .map(|ws| ws.path.as_str())
                    .ok_or("프로젝트가 없습니다.")?;
                if !same_folder(&config.directory, actual_folder) {
                    return Err("폴더 변경은 기존 프로젝트 관리에서 검증 후 적용해 주세요.".into());
                }
                let target = cc::RouteTarget {
                    workspace_id: workspace_id.clone(),
                    session_id: config.main_session_id.clone(),
                    task_id: None,
                };
                let live = host.route(&target);
                let selection = work::AgentSelection {
                    provider: config.provider.clone(),
                    model: (!config.model.is_empty()).then_some(config.model.clone()),
                    effort: (!config.effort.is_empty()).then_some(config.effort.clone()),
                };
                if config.main_session_id.is_some()
                    && live.is_none_or(|l| {
                        l.agent.as_ref() != Some(&selection) || persistent(l).is_none()
                    })
                {
                    return Err(
                        "메인 공급자·모델·강도가 현재 선택한 실행과 일치하지 않습니다.".into(),
                    );
                }
                let main_binding = live.and_then(persistent);
                if config.worker_providers.is_empty() {
                    return Err("실제 워커 공급자를 하나 이상 선택해 주세요.".into());
                }
                let mut allowed_agents: Vec<_> = host
                    .live
                    .iter()
                    .filter(|l| l.capture.workspace == workspace_id)
                    .filter_map(|l| l.agent.as_ref())
                    .filter(|a| config.worker_providers.contains(&a.provider))
                    .cloned()
                    .collect();
                if let Some(catalog) = &p.catalog {
                    for agent in &catalog.defaults {
                        if config.worker_providers.contains(&agent.provider)
                            && !allowed_agents.contains(agent)
                        {
                            allowed_agents.push(agent.clone());
                        }
                    }
                    if !catalog.choices.contains(&selection) && live.is_none() {
                        return Err("설치된 공급자의 실제 모델·강도 설정을 선택해 주세요.".into());
                    }
                }
                for device in host.devices.policies.iter().filter(|d| {
                    d.registration().workspace_id.0 == workspace_id && d.validate_fresh().is_ok()
                }) {
                    for profile in device.profiles() {
                        if config.worker_providers.contains(&profile.agent.provider)
                            && !allowed_agents.contains(&profile.agent)
                        {
                            allowed_agents.push(profile.agent.clone());
                        }
                    }
                }
                allowed_agents
                    .sort_by_key(|a| (a.provider.clone(), a.model.clone(), a.effort.clone()));
                allowed_agents.dedup();
                if allowed_agents.len() > 32 {
                    return Err("허용 실행 프로필은 32개까지 저장할 수 있습니다.".into());
                }
                if config
                    .worker_providers
                    .iter()
                    .any(|provider| !allowed_agents.iter().any(|a| &a.provider == provider))
                {
                    return Err(
                        "선택한 워커 공급자의 실제 모델·강도 설정을 확인할 수 없습니다.".into(),
                    );
                }
                let worker = allowed_agents
                    .first()
                    .cloned()
                    .ok_or("현재 실행 중인 워커 공급자를 선택해 주세요.")?;
                let p = host.projects.get_mut(&workspace_id).unwrap();
                let next = work::ProjectConfig {
                    workspace_id: WorkspaceId(workspace_id.clone()),
                    revision: p
                        .snapshot
                        .config
                        .revision
                        .next()
                        .ok_or("설정 리비전 한도입니다.")?,
                    name: config.name.trim().into(),
                    folder: actual_folder.to_owned(),
                    main_agent: selection,
                    worker_agent: worker,
                    max_workers: config.parallel_limit,
                    confirm_plan: true,
                    use_worktrees: config.use_worktrees,
                };
                next.validate().map_err(|e| e.to_string())?;
                let mut policy = p.policy();
                policy.config_revision = next.revision;
                policy.project_folder = next.folder.clone();
                policy.allowed_agents = allowed_agents.clone();
                policy.allowed_scope_roots = config.allowed_scope_roots.clone();
                policy.max_workers = next.max_workers;
                policy.validate_config(&next).map_err(|e| e.to_string())?;
                p.snapshot.config = next;
                p.snapshot.main_binding = main_binding;
                p.scopes = config.allowed_scope_roots;
                p.allowed_agents = allowed_agents;
                let scopes = serde_json::to_string(&p.scopes).map_err(|e| e.to_string())?;
                set_draft(p, "settings:scopes", &scopes, false)?;
                set_draft(
                    p,
                    "settings:agents",
                    &serde_json::to_string(&p.allowed_agents).map_err(|e| e.to_string())?,
                    false,
                )?;
                p.request = None;
                p.proposal = None;
                p.confirmed.clear();
                p.dirty();
                host.save(&workspace_id, None, Some(id.into()))?;
                let p = host.projects.get_mut(&workspace_id).unwrap();
                p.effect_fence.revoke();
                p.effect_fence = remote::EffectFence::new();
                return Ok(());
            }
            cc::Intent::Submit { target, text } => {
                work::validate_input(&text).map_err(|e| e.to_string())?;
                if local_close::target_pending(host, &target) {
                    return Err("이 워커의 실제 종료 응답을 확인한 뒤 입력해 주세요.".into());
                }
                if remote::sessions::is_target(host, &target) {
                    return remote::sessions::begin(host, id, &target, Some(text), None);
                }
                if host.inputs.values().any(|pending| pending.target == target) {
                    return Err(
                        "이 대상의 입력 승인 응답을 기다리고 있습니다. 자동 재전송하지 않습니다."
                            .into(),
                    );
                }
                let live = host
                    .route(&target)
                    .ok_or("입력 대상의 현재 실행을 확인할 수 없습니다.")?
                    .clone();
                if live.capture.prompt.is_none() {
                    return Err(
                        "현재 세션은 상관관계가 확인된 입력 승인 경로를 지원하지 않습니다.".into(),
                    );
                }
                let key = draft_key(host, &target)?;
                let is_main = host
                    .projects
                    .get(&target.workspace_id)
                    .is_some_and(|p| host.main_target(&target.workspace_id, p) == target);
                let p = host
                    .projects
                    .get_mut(&target.workspace_id)
                    .ok_or("프로젝트가 없습니다.")?;
                if !p.loaded || p.storage_blocked || host.flight.is_some() {
                    return Err("저장 또는 불러오기가 완료된 뒤 입력해 주세요.".into());
                }
                if p.snapshot
                    .drafts
                    .iter()
                    .any(|d| d.target_key == format!("operation:{id}"))
                    || p.snapshot.goals.iter().any(|g| g.submission_id.0 == id)
                {
                    return Err("이미 저장된 입력 ID입니다. 승인 상태가 불명확한 입력을 다시 보내지 않았습니다.".into());
                }
                let mut prompt = text.clone();
                if is_main {
                    let policy = p.policy();
                    policy
                        .validate_config(&p.snapshot.config)
                        .map_err(|e| e.to_string())?;
                    if live.agent.as_ref() != Some(&p.snapshot.config.main_agent) {
                        return Err("메인 실행의 공급자 설정이 바뀌었습니다.".into());
                    }
                    let mut state = p.state();
                    let goal_id = state
                        .submit_goal(
                            work::SubmissionId::from_string(id.into())
                                .map_err(|e| e.to_string())?,
                            bounded(&text, 256),
                            text.clone(),
                        )
                        .map_err(|e| e.to_string())?;
                    let goal = state.goals.iter().find(|g| g.id == goal_id).unwrap();
                    let request = wm::plan::PlannerRequest::new(goal, &p.snapshot.config, &policy)
                        .map_err(|e| e.to_string())?
                        .with_runtime_binding(
                            binding(&live, target.session_id.as_deref().unwrap())
                                .ok_or("현재 메인 실행 바인딩이 없습니다.")?,
                        )
                        .map_err(|e| e.to_string())?;
                    prompt = request.to_prompt().map_err(|e| e.to_string())?;
                    p.request = Some(request);
                    p.proposal = None;
                    p.set_state(state);
                } else if let Some(task_id) = &target.task_id {
                    let task = p
                        .snapshot
                        .tasks
                        .iter()
                        .find(|t| &t.id.0 == task_id)
                        .ok_or("작업이 없습니다.")?;
                    let attempt = task
                        .current_attempt_id
                        .as_ref()
                        .and_then(|id| p.snapshot.attempts.iter().find(|a| &a.id == id))
                        .ok_or("작업의 현재 시도가 없습니다.")?;
                    if attempt.runtime_binding.as_ref()
                        != binding(&live, target.session_id.as_deref().unwrap()).as_ref()
                    {
                        return Err("작업의 현재 실행 대상이 바뀌었습니다.".into());
                    }
                }
                set_draft(
                    p,
                    &format!("operation:{id}"),
                    &serde_json::to_string(&target).map_err(|e| e.to_string())?,
                    true,
                )?;
                set_draft(p, &key, &text, true)?;
                let revision = p.snapshot.config.revision;
                let after = AfterSave {
                    id: id.into(),
                    input: Input {
                        target: target.clone(),
                        draft: text.clone(),
                        dispatch: None,
                    },
                    prompt,
                    config_revision: revision,
                };
                host.save(&target.workspace_id, Some(after), None)?;
                return Ok(());
            }
            cc::Intent::CloseWorker { target, attempt_id } => {
                return if remote::sessions::is_target(host, &target) {
                    remote::sessions::begin(host, id, &target, None, Some(&attempt_id))
                } else {
                    local_close::begin(host, id, &target, &attempt_id)
                };
            }
            cc::Intent::ReconcileRemote { target } => {
                return remote::reconcile::begin(host, id, &target);
            }
            cc::Intent::InstallPlanJson { workspace_id, json } => {
                let p = host
                    .projects
                    .get_mut(&workspace_id)
                    .ok_or("프로젝트가 없습니다.")?;
                let request = p
                    .request
                    .as_ref()
                    .ok_or("확인 중인 메인 계획 요청이 없습니다.")?;
                if request.config_revision() != p.snapshot.config.revision
                    || request.goal().workspace_id != p.snapshot.config.workspace_id
                {
                    return Err("메인 계획 요청의 설정이나 프로젝트가 바뀌었습니다.".into());
                }
                let policy = p.policy();
                let proposal = wm::provider_adapter::accept_manual_json(
                    request,
                    &json,
                    &p.snapshot.config,
                    &policy,
                )
                .map_err(|e| e.to_string())?;
                let mut state = p.state();
                wm::orchestrator::install_proposal(
                    &mut state,
                    &proposal,
                    &p.snapshot.config,
                    &policy,
                )
                .map_err(|e| e.to_string())?;
                p.confirmed.remove(&proposal.plan().goal_id.0);
                p.snapshot
                    .plan_confirmations
                    .retain(|review| review.plan.goal_id != proposal.plan().goal_id);
                p.proposal = Some(proposal);
                p.set_state(state);
                host.save(&workspace_id, None, Some(id.into()))?;
                return Ok(());
            }
            cc::Intent::ConfirmPlan {
                workspace_id,
                goal_id,
            } => {
                if remote::project_pending(host, &workspace_id) {
                    return Err("원격 입력 승인 후 다음 배정안을 확인해 주세요.".into());
                }
                let p = host
                    .projects
                    .get_mut(&workspace_id)
                    .ok_or("프로젝트가 없습니다.")?;
                if host.flight.is_some() || p.generation != p.saved_generation {
                    return Err("배정안 저장이 완료된 뒤 확인해 주세요.".into());
                }
                let goal = work::GoalId::from_string(goal_id).map_err(|error| error.to_string())?;
                p.confirm_goal(&goal, &host.devices.policies)?;
                host.save(&workspace_id, None, Some(id.into()))?;
                return Ok(());
            }
            cc::Intent::DispatchTask { target } => {
                if launch::workspace_uncertain(&host.launches, &target.workspace_id) {
                    return Err(
                        "확인되지 않은 이전 실행이 있습니다. 실행 상태를 먼저 복구해 주세요."
                            .into(),
                    );
                }
                let live = host
                    .route(&target)
                    .ok_or("실행 세션을 명시적으로 선택해 주세요.")?
                    .clone();
                let p = host
                    .projects
                    .get_mut(&target.workspace_id)
                    .ok_or("프로젝트가 없습니다.")?;
                if live.state != cc::TaskState::Queued
                    || !live
                        .cwd
                        .as_deref()
                        .is_some_and(|cwd| same_folder(cwd, &p.snapshot.config.folder))
                    || persistent(&live) == p.snapshot.main_binding
                {
                    return Err("선택한 세션은 현재 프로젝트 폴더의 유휴 워커가 아닙니다.".into());
                }
                let task = p
                    .snapshot
                    .tasks
                    .iter()
                    .find(|t| Some(&t.id.0) == target.task_id.as_ref())
                    .ok_or("작업이 없습니다.")?;
                if live.agent.as_ref() != Some(&task.assigned_agent) {
                    return Err("워커 공급자·모델·강도가 배정안과 일치하지 않습니다.".into());
                }
                let confirmed = p
                    .confirmed_for_task(&task.id)
                    .ok_or("먼저 현재 배정안을 확인해 주세요.")?;
                if confirmed.goal_id() != &task.goal_id {
                    return Err("확인한 배정안과 다른 목표의 작업입니다.".into());
                }
                let request = wm::dispatch::DispatchRequest {
                    task_id: task.id.clone(),
                    operation_id: work::OperationId::from_string(id.into())
                        .map_err(|e| e.to_string())?,
                    folder: wm::dispatch::DispatchFolder {
                        path: p.snapshot.config.folder.clone(),
                        worktree_owner: None,
                    },
                    persistent_binding: persistent(&live),
                    runtime_binding: binding(&live, target.session_id.as_deref().unwrap())
                        .ok_or("실행 세대가 확인되지 않습니다.")?,
                };
                let prepared = p.prepare_existing(request)?;
                let prepared = match prepared {
                    wm::dispatch::PrepareOutcome::Ready(prepared) => prepared,
                    wm::dispatch::PrepareOutcome::Existing {
                        attempt_id,
                        delivery,
                    } => {
                        return Err(format!(
                            "이미 처리한 시도 {} ({delivery:?})입니다. 재전송하지 않았습니다.",
                            attempt_id.0
                        ));
                    }
                };
                let assignment = prepared.assignment;
                p.set_state(prepared.next_state);
                p.snapshot.outbox.push(stored::DispatchOutbox {
                    attempt_id: assignment.attempt.id.clone(),
                    operation_id: assignment.attempt.operation_id.clone(),
                    state: stored::DispatchOutboxState::Pending,
                });
                let after = AfterSave {
                    id: id.into(),
                    prompt: assignment.prompt.clone(),
                    input: Input {
                        target: target.clone(),
                        draft: String::new(),
                        dispatch: Some(assignment),
                    },
                    config_revision: p.snapshot.config.revision,
                };
                host.save(&target.workspace_id, Some(after), None)?;
                return Ok(());
            }
            cc::Intent::Approval {
                target,
                approval_id,
                allow,
            } => {
                return self.respond_command_center_project_approval(
                    host,
                    id,
                    &target,
                    &approval_id,
                    allow,
                );
            }
            cc::Intent::ReportResult {
                target,
                attempt_id,
                text,
            } => {
                if remote::handoff_pending(host, &target) {
                    return Err("실제 입력 승인 응답을 확인한 뒤 결과를 보고해 주세요.".into());
                }
                if text.trim().is_empty() || text.len() > work::WORK_EVENT_MAX_BYTES {
                    return Err("실제 작업 결과는 8KiB 이하로 기록해 주세요.".into());
                }
                let p = host
                    .projects
                    .get_mut(&target.workspace_id)
                    .ok_or("프로젝트가 없습니다.")?;
                let task = p
                    .snapshot
                    .tasks
                    .iter()
                    .find(|t| Some(&t.id.0) == target.task_id.as_ref())
                    .ok_or("현재 작업이 없습니다.")?;
                let revision = task
                    .result
                    .as_ref()
                    .map(|r| r.revision.next())
                    .unwrap_or(Some(work::ResultRevision(1)))
                    .ok_or("결과 리비전 한도입니다.")?;
                let result = work::TaskResult {
                    revision,
                    summary: text.clone(),
                    source_revision: None,
                    artifact_revision: None,
                    verification: work::VerificationState::NotRun,
                };
                result.validate().map_err(|e| e.to_string())?;
                apply_user_work_event(
                    p,
                    &target,
                    &attempt_id,
                    work::WorkEventKind::ResultSubmitted { result },
                    format!("수동 결과 보고 · 검증 미실행\n{text}"),
                )?;
                host.save(&target.workspace_id, None, Some(id.into()))?;
                return Ok(());
            }
            cc::Intent::VerifyResult {
                target,
                attempt_id,
                revision,
                passed,
                evidence,
            } => {
                if remote::handoff_pending(host, &target) {
                    return Err("실제 입력 승인 응답을 확인한 뒤 검증해 주세요.".into());
                }
                if evidence.trim().is_empty() || evidence.len() > 4096 {
                    return Err(
                        "직접 수행한 검증의 명령과 결과를 4KiB 이하로 기록해 주세요.".into(),
                    );
                }
                let p = host
                    .projects
                    .get_mut(&target.workspace_id)
                    .ok_or("프로젝트가 없습니다.")?;
                let task = p
                    .snapshot
                    .tasks
                    .iter()
                    .find(|t| Some(&t.id.0) == target.task_id.as_ref())
                    .ok_or("현재 작업이 없습니다.")?;
                let result = task
                    .result
                    .as_ref()
                    .filter(|r| r.revision.0.to_string() == revision)
                    .ok_or("검증할 결과 리비전이 바뀌었습니다.")?;
                apply_user_work_event(
                    p,
                    &target,
                    &attempt_id,
                    work::WorkEventKind::VerificationFinished {
                        result_revision: result.revision,
                        outcome: if passed {
                            work::VerificationState::Passed
                        } else {
                            work::VerificationState::Failed
                        },
                    },
                    format!(
                        "사용자 직접 검증 · {}\n{}",
                        if passed { "통과" } else { "실패" },
                        evidence
                    ),
                )?;
                host.save(&target.workspace_id, None, Some(id.into()))?;
                return Ok(());
            }
            cc::Intent::ConfirmResult { target, revision } => {
                if remote::handoff_pending(host, &target) {
                    return Err("실제 입력 승인 응답을 확인한 뒤 완료를 확인해 주세요.".into());
                }
                let p = host
                    .projects
                    .get_mut(&target.workspace_id)
                    .ok_or("프로젝트가 없습니다.")?;
                let task = p
                    .snapshot
                    .tasks
                    .iter()
                    .find(|t| Some(&t.id.0) == target.task_id.as_ref())
                    .ok_or("작업이 없습니다.")?;
                let attempt = task
                    .current_attempt_id
                    .as_ref()
                    .and_then(|id| p.snapshot.attempts.iter().find(|a| &a.id == id))
                    .ok_or("현재 시도가 없습니다.")?;
                let result = task.result.as_ref().ok_or("검증된 결과가 없습니다.")?;
                if result.revision.0.to_string() != revision {
                    return Err("결과 리비전이 변경되었습니다.".into());
                }
                let attempt_id = attempt.id.0.clone();
                let result_revision = result.revision;
                apply_user_work_event(
                    p,
                    &target,
                    &attempt_id,
                    work::WorkEventKind::UserConfirmed { result_revision },
                    format!("사용자 결과 확인 · 리비전 {revision}"),
                )?;
                host.save(&target.workspace_id, None, Some(id.into()))?;
                return Ok(());
            }
        }
        host.finish(id, cc::ReceiptStatus::Acknowledged, "요청을 적용했습니다.");
        Ok(())
    }
    pub(super) fn pump_command_center(&mut self, ctx: &egui::Context) {
        if !self.command_center_layout.available {
            return;
        }
        let mut host = self.command_center.take().unwrap_or_else(|| {
            Box::new(CommandCenterHost::new(self.data_dir().to_path_buf(), ctx))
        });
        let visible = self.command_center_layout.mode == layout::LayoutMode::CommandCenter;
        if !visible
            && host.flight.is_none()
            && host.pending.is_empty()
            && host.requested_loads.is_empty()
            && host.inputs.is_empty()
            && !host.observations.has_pending()
            && !host.devices.has_pending()
            && host.launches.values().all(|l| l.finished())
            && !host.remotes.values().any(|p| p.pending())
            && !remote::sessions::pending(&host)
            && !remote::sessions::has_watches(&host)
            && !local_close::has_pending(&host)
            && !remote::reconcile::pending(&host)
            && host
                .projects
                .values()
                .all(|p| p.generation == p.saved_generation)
        {
            if host.published
                && !host.publication_failed
                && let Some(core) = &self.session_core
            {
                if core.publish_work_snapshot(serde_json::Value::Null).is_ok() {
                    host.published = false;
                } else {
                    host.publication_failed = true;
                    host.status =
                        "원격 작업 화면 지우기에 실패했습니다. 다시 열어 갱신할 수 있습니다."
                            .into();
                }
            }
            self.command_center = Some(host);
            return;
        }
        self.refresh_command_center_routes(&mut host);
        if let Some((target, action)) = host.pending_git.take()
            && host
                .route(&target)
                .is_some_and(|route| self.command_center_route_focused(route))
        {
            self.apply_command_center_git_action(ctx, action);
        }
        if let Some((target, tab)) = host.aux_loading.clone()
            && let Some(route) = host
                .route(&target)
                .filter(|route| self.command_center_route_focused(route))
                .cloned()
        {
            self.sync_session_aux_scope();
            if tab == cc::DetailTab::Git {
                self.request_git_panel_io_at(
                    ctx,
                    route.cwd.map(PathBuf::from),
                    ui::git_panel::GitPanelIoRequest::Snapshot,
                );
            }
            if tab == cc::DetailTab::History {
                self.enter_work_history_tab();
            }
            host.aux_ready = Some(target);
            host.aux_loading = None;
        }
        let mut send: Option<(FleetPromptTarget, String, String)> = None;
        if let Some(result) = host.repository.try_recv() {
            let flight = host.flight.take();
            match (flight, result) {
                (
                    Some(Flight::Load {
                        workspace,
                        token,
                        receipt,
                    }),
                    Ok(RepositoryResult::Loaded {
                        workspace_id,
                        token: found,
                        result,
                    }),
                ) if workspace_id.0 == workspace && found == token => {
                    finish_project_load(&mut host, &workspace, receipt, result);
                }
                (
                    Some(Flight::Save {
                        workspace,
                        generation,
                        after,
                        receipt,
                    }),
                    Ok(RepositoryResult::Saved {
                        workspace_id,
                        edit_generation,
                        result,
                    }),
                ) if workspace_id.0 == workspace && edit_generation == generation => match result {
                    Ok(stored::SaveOutcome::Saved { revision }) => {
                        if let Some(p) = host.projects.get_mut(&workspace) {
                            p.ack_saved(revision, generation);
                        }
                        if let Some(after) = after {
                            let authorized = host
                                .remote_permits
                                .get(&after.id)
                                .is_none_or(|permit| permit.is_current());
                            let live = host.route(&after.input.target).cloned();
                            let p = host.projects.get_mut(&workspace).unwrap();
                            let valid = authorized
                                && p.snapshot.config.revision == after.config_revision
                                && live.as_ref().is_some_and(|l| l.capture.prompt.is_some())
                                && after
                                    .input
                                    .dispatch
                                    .as_ref()
                                    .is_none_or(|a| assignment_current(p, a));
                            if valid {
                                let mut allowed = true;
                                if let Some(assignment) = &after.input.dispatch {
                                    allowed = p.ledger.record_durable(assignment.clone()).is_ok()
                                        && p.ledger.claim_send(&assignment.attempt.operation_id);
                                }
                                if allowed {
                                    let prompt_target = live.unwrap().capture.prompt.unwrap();
                                    send = Some((prompt_target, after.prompt, after.id.clone()));
                                    host.inputs.insert(after.id, after.input);
                                } else {
                                    host.reject(&after.id,"실행 요청은 이미 처리되었거나 유효하지 않습니다. 다시 보내지 않았습니다.");
                                }
                            } else {
                                if let Some(assignment) = &after.input.dispatch {
                                    apply_delivery_outcome(
                                        p,
                                        assignment,
                                        ui::composer::PromptAdmissionOutcome::Rejected,
                                    );
                                }
                                host.reject(&after.id,"저장 후 권한·실행 대상·설정이 바뀌었습니다. 입력을 보내지 않았습니다.");
                            }
                        }
                        if let Some(id) = receipt {
                            host.finish(
                                &id,
                                cc::ReceiptStatus::Acknowledged,
                                "프로젝트 상태를 저장했습니다.",
                            );
                        }
                    }
                    Ok(stored::SaveOutcome::Conflict { .. }) | Err(_) => {
                        if let Some(p) = host.projects.get_mut(&workspace) {
                            p.storage_blocked = true;
                        }
                        if let Some(after) = after {
                            if let Some(assignment) = &after.input.dispatch
                                && let Some(p) = host.projects.get_mut(&workspace)
                            {
                                apply_delivery_outcome(
                                    p,
                                    assignment,
                                    ui::composer::PromptAdmissionOutcome::Rejected,
                                );
                            }
                            host.reject(
                                &after.id,
                                "상태 저장에 실패하여 입력을 보내지 않았습니다. 초안은 보존됩니다.",
                            );
                        }
                        if let Some(id) = receipt {
                            host.reject(&id,"상태 저장에 실패했습니다. 다른 앱의 변경 또는 저장소 상태를 확인해 주세요.");
                        }
                        host.status =
                            "프로젝트 저장에 실패했습니다. 현재 초안과 실행 상태는 보존됩니다."
                                .into();
                    }
                },
                (flight, _) => {
                    if let Some(Flight::Load {
                        workspace, receipt, ..
                    }) = &flight
                    {
                        if let Some(p) = host.projects.get_mut(workspace) {
                            p.storage_blocked = true;
                        }
                        if let Some(id) = receipt {
                            host.reject(id, "프로젝트 읽기 응답을 확인할 수 없습니다.");
                        }
                    }
                    if let Some(Flight::Save {
                        workspace,
                        after,
                        receipt,
                        ..
                    }) = flight
                    {
                        if let Some(p) = host.projects.get_mut(&workspace) {
                            p.storage_blocked = true;
                        }
                        if let Some(after) = after {
                            if let Some(assignment) = &after.input.dispatch
                                && let Some(p) = host.projects.get_mut(&workspace)
                            {
                                apply_delivery_outcome(
                                    p,
                                    assignment,
                                    ui::composer::PromptAdmissionOutcome::Rejected,
                                );
                            }
                            host.reject(
                                &after.id,
                                "저장 응답을 확인할 수 없어 입력을 보내지 않았습니다.",
                            );
                        }
                        if let Some(id) = receipt {
                            host.reject(&id, "저장 응답을 확인할 수 없습니다.");
                        }
                    }
                    host.status = "작업 저장 worker의 응답을 확인할 수 없습니다.".into();
                }
            }
        }
        self.pump_command_center_devices(&mut host, visible);
        self.pump_command_center_observations(&mut host);
        self.pump_command_center_launches(&mut host);
        self.pump_command_center_remote(&mut host);
        self.pump_command_center_remote_sessions(&mut host, visible);
        self.pump_command_center_local_closes(&mut host);
        self.pump_command_center_reconcile(&mut host);
        // Mutations are staged by render and run here, through the same guarded host pipeline.
        if host.flight.is_none()
            && let Some((id, intent)) = host.pending.pop_front()
            && let Err(error) = self.command_center_intent(&mut host, &id, intent)
        {
            host.reject(&id, error);
        }
        if host.flight.is_none() && send.is_none() {
            let next_load = host.requested_loads.keys().next().cloned().or_else(|| {
                host.projects
                    .get(&self.active.id)
                    .filter(|p| !p.loaded && !p.storage_blocked)
                    .map(|_| self.active.id.clone())
            });
            if let Some(workspace) = next_load {
                let expired = host
                    .requested_loads
                    .get(&workspace)
                    .and_then(|id| host.remote_permits.get(id))
                    .is_some_and(|permit| !permit.is_current());
                if expired {
                    if let Some(id) = host.requested_loads.remove(&workspace) {
                        host.reject(&id, "프로젝트 읽기 권한이 만료되었습니다.");
                    }
                } else if let Err(error) =
                    admit_project_load(&mut host, &workspace, &self.active.id)
                {
                    if let Some(id) = host.requested_loads.remove(&workspace) {
                        host.reject(&id, error.clone());
                    }
                    host.status = error;
                } else {
                    host.next_token = host.next_token.saturating_add(1);
                    let token = host.next_token;
                    if host
                        .repository
                        .try_request(RepositoryJob::Load {
                            workspace_id: WorkspaceId(workspace.clone()),
                            token,
                        })
                        .is_ok()
                    {
                        let receipt = host.requested_loads.remove(&workspace);
                        host.flight = Some(Flight::Load {
                            workspace,
                            token,
                            receipt,
                        });
                    }
                }
            } else if let Some(workspace) = host
                .projects
                .iter()
                .find(|(_, p)| p.loaded && !p.storage_blocked && p.generation != p.saved_generation)
                .map(|(id, _)| id.clone())
            {
                let _ = host.save(&workspace, None, None);
            }
        }
        self.command_center = Some(host);
        if let Some((target, prompt, id)) = send {
            self.stage_command_center_observed_prompt(target, prompt, id);
        }
        let approval_revision = self.agent_sessions_ui.command_center_approvals_revision();
        if let Some(host) = self.command_center.as_mut()
            && host.approval_revision != approval_revision
        {
            host.approval_revision = approval_revision;
            host.publication_dirty = true;
        }
        let generations: Vec<_> = self
            .command_center
            .as_ref()
            .unwrap()
            .projects
            .iter()
            .map(|(id, p)| (id.clone(), p.generation, p.loaded))
            .collect();
        let dirty = self.command_center.as_ref().is_some_and(|h| {
            h.publication_dirty
                || h.generations != generations
                || !h.published && !h.publication_failed
        });
        if dirty && visible {
            let result = if let Some(core) = &self.session_core {
                let mut remote = self.command_center_snapshot();
                remote.font_scale = self.config.ui.ui_scale;
                for project in &mut remote.projects {
                    project.create_main = no("이 컴퓨터에서 새 실행을 시작해 주세요.");
                    for task in &mut project.tasks {
                        task.create_worker = no("이 컴퓨터에서 새 실행을 시작해 주세요.");
                        task.close_worker = no("이 컴퓨터에서 워커 실행을 종료해 주세요.");
                        if task
                            .target
                            .session_id
                            .as_deref()
                            .is_some_and(|s| s.starts_with("remote:"))
                        {
                            task.input = no("이 컴퓨터에서 원격 실행에 입력해 주세요.");
                        }
                    }
                }
                serde_json::to_value(remote)
                    .map_err(|_| ())
                    .and_then(|value| core.publish_work_snapshot(value).map_err(|_| ()))
            } else {
                Ok(())
            };
            if let Some(host) = self.command_center.as_mut() {
                host.generations = generations;
                host.publication_dirty = false;
                match result {
                    Ok(()) => {
                        host.published = self.session_core.is_some();
                        host.publication_failed = false;
                    }
                    Err(()) => {
                        host.publication_failed = true;
                        host.status = "원격 작업 화면 갱신에 실패했습니다. 로컬 작업은 보존되며 다음 변경 때 다시 게시합니다.".into();
                    }
                }
            }
        } else if !visible
            && self.command_center.as_ref().is_some_and(|h| h.published)
            && let Some(core) = &self.session_core
            && core.publish_work_snapshot(serde_json::Value::Null).is_ok()
        {
            self.command_center.as_mut().unwrap().published = false;
        }
    }
    pub(super) fn command_center_input_outcome(
        &mut self,
        id: &str,
        outcome: ui::composer::PromptAdmissionOutcome,
    ) {
        let Some(mut host) = self.command_center.take() else {
            return;
        };
        finish_input_outcome(&mut host, id, outcome);
        self.command_center = Some(host);
        self.egui_ctx.request_repaint();
    }
    fn command_center_route_focused(&self, route: &LiveRoute) -> bool {
        if route.capture.workspace != self.active.id {
            return false;
        }
        match &route.capture.target {
            FleetTarget::Pty { tab, pane, session } => {
                self.active.workspace_ui.mux().is_some_and(|mux| {
                    mux.active_tab.as_ref() == Some(tab)
                        && mux.focused_pane.as_ref() == Some(pane)
                        && mux.tabs.iter().any(|t| {
                            &t.id == tab
                                && t.panes
                                    .iter()
                                    .any(|p| &p.id == pane && p.session_id == Some(*session))
                        })
                })
            }
            FleetTarget::Structured { .. } => false,
        }
    }
    fn apply_command_center_git_action(
        &mut self,
        ctx: &egui::Context,
        action: ui::git_panel::GitPanelAction,
    ) {
        use ui::git_panel::{GitPanelAction as A, GitPanelIoRequest as R};
        match action {
            A::Refresh => self.request_git_panel_io_at(
                ctx,
                self.session_aux.current.git_panel_cwd.clone(),
                R::Snapshot,
            ),
            A::OpenRemoteBranch => self.open_git_panel_remote(ctx),
            A::ShowFileDiff { rel_path, mode } => {
                let view = &mut self.session_aux.current;
                view.diff_viewer_ui.open(rel_path.clone(), mode);
                view.diff_viewer_ui
                    .set_path_target(view.git_panel_ui.path_target(&rel_path, mode));
                self.request_git_panel_io_at(
                    ctx,
                    self.session_aux.current.git_panel_cwd.clone(),
                    R::FileDiff { rel_path, mode },
                );
            }
            A::LoadCommitFiles { sha } => self.request_git_panel_io_at(
                ctx,
                self.session_aux.current.git_panel_cwd.clone(),
                R::CommitFiles { sha },
            ),
            A::OpenWorktreeShell { path } => {
                self.stage_workspace_controller_action(WorkspaceControllerAction::SpawnShellAt {
                    cwd: Some(path),
                });
            }
            A::InsertPath { path } => {
                if let Some(session) = self.active.workspace_ui.focused_session() {
                    let bytes = ui::workspace::path_insert_paste_bytes(
                        &path,
                        self.active.workspace_ui.session_shell_kind(session),
                        self.active.workspace_ui.session_bracketed_paste(session),
                    );
                    self.stage_workspace_controller_action(WorkspaceControllerAction::Runtime(
                        runtime::RuntimeCommand::WriteInput { session, bytes },
                    ));
                }
            }
            A::RevealInFinder { path } => {
                if self.pending_app_host_action.is_none()
                    && let Some(folder) = existing_reveal_folder(&path)
                {
                    self.pending_app_host_action = Some(AppHostIoAction::OpenPath(folder));
                }
            }
        }
    }
    pub(super) fn show_command_center(&mut self, ui: &mut egui::Ui) {
        let snapshot = self.command_center_snapshot();
        let Some(mut host) = self.command_center.take() else {
            return;
        };
        let intents = cc::show(ui, &mut host.ui, &snapshot);
        for intent in intents {
            host.enqueue(uuid::Uuid::new_v4().to_string(), intent);
        }
        let slot = host
            .ui
            .terminal_rect
            .zip(host.ui.terminal_target.clone())
            .and_then(|(rect, target)| host.route(&target).cloned().map(|route| (rect, route)));
        let text = self.i18n.clone();
        let name = self
            .workspaces
            .iter()
            .find(|ws| ws.id == self.active.id)
            .map(Self::workspace_display_name);
        self.active.workspace_ui.set_project_name(name);
        self.active
            .workspace_ui
            .set_ui_scale(self.config.ui.ui_scale);
        self.active
            .workspace_ui
            .set_agent_send_presets(self.config.ui.agent_send_presets.clone());
        let events: Vec<_> = self.active.pending_events.drain(..).collect();
        let mut rendered = false;
        let remote_rendered = host
            .ui
            .terminal_rect
            .zip(host.ui.terminal_target.clone())
            .is_some_and(|(rect, target)| {
                remote::sessions::render(
                    &mut host,
                    ui,
                    rect,
                    &target,
                    &self.config.terminal,
                    self.config.ui.ui_scale,
                )
            });
        if let Some((rect, route)) = slot
            && route.capture.workspace == self.active.id
            && let FleetTarget::Pty { tab, pane, session } = &route.capture.target
        {
            let focused = self.active.workspace_ui.mux().is_some_and(|mux| {
                mux.active_tab.as_ref() == Some(tab)
                    && mux.focused_pane.as_ref() == Some(pane)
                    && mux.tabs.iter().any(|t| {
                        &t.id == tab
                            && t.panes
                                .iter()
                                .any(|p| &p.id == pane && p.session_id == Some(*session))
                    })
            });
            if focused {
                let mut terminal = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(rect)
                        .id_salt("command-center-native-terminal"),
                );
                terminal.set_clip_rect(rect.intersect(ui.clip_rect()));
                let output = self.active.workspace_ui.show_with_input(
                    &mut terminal,
                    &self.config.terminal,
                    &events,
                    &text,
                    !host.ui.composer_focused,
                );
                if let Some((kind, intent)) = output.aux_tab_intent {
                    match kind {
                        ui::workspace::PaneAuxTabKind::History => {
                            self.apply_work_history_tab_intent(intent)
                        }
                        ui::workspace::PaneAuxTabKind::Git => {
                            self.apply_git_tab_intent(ui.ctx(), intent)
                        }
                        ui::workspace::PaneAuxTabKind::Document(id) => {
                            self.apply_document_tab_intent(id, intent)
                        }
                    }
                }
                self.frame_terminal_owner = if host.ui.composer_focused {
                    FrameTerminalOwner::None
                } else {
                    FrameTerminalOwner::Primary
                };
                rendered = true;
            }
        }
        if !rendered {
            if !remote_rendered
                && let Some(target) = host.ui.terminal_target.clone()
                && host.route(&target).is_some()
                && !host
                    .pending
                    .iter()
                    .any(|(_, i)| matches!(i,cc::Intent::OpenSession(t) if t == &target))
            {
                host.enqueue(
                    uuid::Uuid::new_v4().to_string(),
                    cc::Intent::OpenSession(target),
                );
            }
            self.active
                .workspace_ui
                .update_hidden(ui.ctx(), &events, &text);
            self.frame_terminal_owner = FrameTerminalOwner::None;
        }
        if let Some((rect, target)) = host.ui.aux_rect.zip(host.ui.aux_target.clone())
            && host.aux_ready.as_ref() == Some(&target)
            && host
                .route(&target)
                .is_some_and(|route| self.command_center_route_focused(route))
        {
            let mut aux = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(rect)
                    .id_salt("command-center-native-aux"),
            );
            aux.set_clip_rect(rect.intersect(ui.clip_rect()));
            aux.painter()
                .rect_filled(rect, 0.0, aux.visuals().panel_fill);
            match host.ui.aux_tab {
                Some(cc::DetailTab::Git) => {
                    if let Some(action) = self.render_git_tab_body(&mut aux, rect, &text, false) {
                        host.pending_git = Some((target, action));
                    }
                }
                Some(cc::DetailTab::History) => {
                    let name = self
                        .workspaces
                        .iter()
                        .find(|ws| ws.id == self.active.id)
                        .map(Self::workspace_display_name)
                        .unwrap_or_default();
                    if let Some(action) =
                        self.render_work_history_tab_body(&mut aux, rect, &name, None, &text)
                    {
                        self.pending_work_history_action = Some(action);
                    }
                }
                _ => {}
            }
        }
        self.command_center = Some(host);
        self.active.workspace_ui.flush_render_side_effects(ui.ctx());
        for runtime in self.warm.values_mut() {
            runtime.workspace_ui.flush_render_side_effects(ui.ctx());
        }
        crate::native_key_monitor::discard_unclaimed_move_paste();
        self.flush_workspace_terminal_protocol_tail(ui.ctx());
        self.frame_stats.end();
    }
}
fn admit_project_load(
    host: &mut CommandCenterHost,
    workspace: &str,
    active: &str,
) -> Result<(), String> {
    if host.projects.get(workspace).is_some_and(|p| p.loaded) {
        return Ok(());
    }
    if host.projects.values().filter(|p| p.loaded).count() < 4 {
        return Ok(());
    }
    let evict = host
        .projects
        .iter()
        .find(|(id, p)| {
            id.as_str() != active
                && id.as_str() != workspace
                && p.loaded
                && p.generation == p.saved_generation
                && !p.storage_blocked
                && p.snapshot
                    .goals
                    .iter()
                    .all(|goal| goal.state == work::WorkState::Completed)
                && p.snapshot
                    .tasks
                    .iter()
                    .all(|task| task.state == work::WorkState::Completed)
                && p.snapshot.outbox.iter().all(|entry| {
                    !matches!(
                        entry.state,
                        stored::DispatchOutboxState::Pending
                            | stored::DispatchOutboxState::Uncertain
                    )
                })
                && !p
                    .snapshot
                    .drafts
                    .iter()
                    .any(|draft| draft.delivery_uncertain)
                && !remote::project_pending(host, id)
                && !host.observations.workspace_pending(id)
                && !host
                    .launches
                    .values()
                    .any(|launch| launch.record.workspace.0 == **id && !launch.finished())
                && !host.pending_configs.contains_key(*id)
                && !host
                    .pending
                    .iter()
                    .any(|(_, intent)| intent_workspace(intent) == Some(id.as_str()))
                && !host.flight.as_ref().is_some_and(|flight| match flight {
                    Flight::Load { workspace, .. } | Flight::Save { workspace, .. } => {
                        workspace == *id
                    }
                })
        })
        .map(|(id, _)| id.clone());
    let id = evict.ok_or("작업 중이거나 저장되지 않은 프로젝트가 4개 열려 있습니다. 작업을 확인한 뒤 다른 프로젝트를 불러와 주세요.")?;
    let p = host.projects.get_mut(&id).unwrap();
    p.effect_fence.revoke();
    p.effect_fence = remote::EffectFence::new();
    p.request = None;
    p.proposal = None;
    p.confirmed.clear();
    p.snapshot.plan_confirmations.clear();
    p.snapshot.goals.clear();
    p.snapshot.tasks.clear();
    p.snapshot.attempts.clear();
    p.snapshot.events.clear();
    p.snapshot.outbox.clear();
    p.snapshot.drafts.clear();
    p.scopes.clear();
    p.allowed_agents.clear();
    p.ledger = Default::default();
    p.loaded = false;
    p.generation = 0;
    p.saved_generation = 0;
    p.revision = None;
    host.publication_dirty = true;
    Ok(())
}
fn project_transaction<T>(
    host: &mut CommandCenterHost,
    workspace: Option<&str>,
    apply: impl FnOnce(&mut CommandCenterHost) -> Result<T, String>,
) -> Result<T, String> {
    let previous = workspace.and_then(|ws| host.projects.get(ws).cloned());
    let result = apply(host);
    if result.is_err()
        && let (Some(workspace), Some(previous)) = (workspace, previous)
    {
        host.projects.insert(workspace.into(), previous);
    }
    result
}
fn active_workspace_cwd(
    workspace: &str,
    active_workspace: &str,
    active_cwd: Option<String>,
) -> Option<String> {
    (workspace == active_workspace)
        .then_some(active_cwd)
        .flatten()
}
fn intent_workspace(intent: &cc::Intent) -> Option<&str> {
    match intent {
        cc::Intent::CreateMain { workspace_id }
        | cc::Intent::LoadProject { workspace_id }
        | cc::Intent::SaveProject { workspace_id, .. }
        | cc::Intent::InstallPlanJson { workspace_id, .. }
        | cc::Intent::ConfirmPlan { workspace_id, .. }
        | cc::Intent::RetryStorage { workspace_id } => Some(workspace_id),
        cc::Intent::SetTaskAssignment { target, .. }
        | cc::Intent::CreateWorker { target }
        | cc::Intent::CloseWorker { target, .. }
        | cc::Intent::Submit { target, .. }
        | cc::Intent::DraftChanged { target, .. }
        | cc::Intent::DispatchTask { target }
        | cc::Intent::ReportResult { target, .. }
        | cc::Intent::VerifyResult { target, .. }
        | cc::Intent::ConfirmResult { target, .. } => Some(&target.workspace_id),
        _ => None,
    }
}
fn recover_observation(snapshot: &mut stored::CommandCenterSnapshot) {
    fn active(state: work::WorkState) -> bool {
        matches!(
            state,
            work::WorkState::Queued
                | work::WorkState::Running
                | work::WorkState::WaitingApproval
                | work::WorkState::WaitingInput
                | work::WorkState::ObservationUnknown
        )
    }
    for outbox in &mut snapshot.outbox {
        if outbox.state == stored::DispatchOutboxState::Pending {
            outbox.state = stored::DispatchOutboxState::Uncertain;
        }
    }
    for task in &mut snapshot.tasks {
        if task.current_attempt_id.is_some() && active(task.state) {
            task.state = work::WorkState::ObservationUnknown;
        }
    }
    for attempt in &mut snapshot.attempts {
        if active(attempt.state) {
            attempt.state = work::WorkState::ObservationUnknown;
        }
    }
}

/// Categories come from the accepted event variant, never its display prose.
fn history_kind(kind: &work::WorkEventKind) -> &'static str {
    match kind {
        work::WorkEventKind::WaitingApproval | work::WorkEventKind::UserConfirmed { .. } => {
            "approval"
        }
        work::WorkEventKind::InputAdmitted { .. }
        | work::WorkEventKind::Started
        | work::WorkEventKind::WaitingInput
        | work::WorkEventKind::ResultSubmitted { .. }
        | work::WorkEventKind::VerificationFinished { .. }
        | work::WorkEventKind::Failed { .. }
        | work::WorkEventKind::Cancelled
        | work::WorkEventKind::ObservationLost
        | work::WorkEventKind::ProcessExited => "work",
    }
}

fn apply_user_work_event(
    p: &mut Project,
    target: &cc::RouteTarget,
    expected_attempt: &str,
    kind: work::WorkEventKind,
    display: String,
) -> Result<(), String> {
    if p.storage_blocked || !p.loaded {
        return Err("저장 상태를 먼저 확인해 주세요.".into());
    }
    let task = p
        .snapshot
        .tasks
        .iter()
        .find(|t| {
            Some(&t.id.0) == target.task_id.as_ref() && t.workspace_id.0 == target.workspace_id
        })
        .ok_or("현재 프로젝트의 작업이 아닙니다.")?;
    let attempt = task
        .current_attempt_id
        .as_ref()
        .and_then(|id| p.snapshot.attempts.iter().find(|a| &a.id == id))
        .ok_or("작업의 현재 시도가 없습니다.")?;
    if attempt.id.0 != expected_attempt {
        return Err("보고할 작업 시도가 바뀌었습니다. 현재 결과 화면을 다시 확인해 주세요.".into());
    }
    let event = work::WorkEvent {
        id: work::EventId::new(),
        workspace_id: task.workspace_id.clone(),
        task_id: task.id.clone(),
        attempt_id: attempt.id.clone(),
        runtime_binding: attempt
            .runtime_binding
            .clone()
            .ok_or("현재 시도의 실행 바인딩이 없습니다.")?,
        sequence: attempt.last_event_sequence.saturating_add(1),
        kind,
    };
    let stored_event = stored::StoredEvent {
        id: event.id.clone(),
        kind: history_kind(&event.kind).into(),
        task_id: Some(event.task_id.clone()),
        attempt_id: Some(event.attempt_id.clone()),
        display_payload: bounded(&display, work::WORK_EVENT_MAX_BYTES),
    };
    let mut state = p.state();
    if state.apply_event(event).map_err(|e| e.to_string())? != wm::ApplyOutcome::Applied {
        return Err("현재 작업 상태에서는 이 보고를 적용할 수 없습니다.".into());
    }
    p.set_state(state);
    if p.snapshot.events.len() >= work::WORK_PAGE_MAX_ROWS {
        p.snapshot.events.remove(0);
    }
    p.snapshot.events.push(stored_event);
    Ok(())
}

fn same_folder(a: &str, b: &str) -> bool {
    fn normalized(path: &str) -> Option<Vec<String>> {
        let path = Path::new(path);
        if !path.is_absolute() {
            return None;
        }
        path.components()
            .map(|c| match c {
                std::path::Component::RootDir => Some("/".into()),
                std::path::Component::Normal(s) => s.to_str().map(|s| s.nfc().collect()),
                std::path::Component::CurDir => Some(String::new()),
                _ => None,
            })
            .collect()
    }
    normalized(a)
        .zip(normalized(b))
        .is_some_and(|(a, b)| a == b)
}

fn assignment_current(project: &Project, assignment: &wm::dispatch::DispatchAssignment) -> bool {
    project.snapshot.tasks.iter().any(|task| {
        task.id == assignment.task.id
            && task.current_attempt_id.as_ref() == Some(&assignment.attempt.id)
            && matches!(
                task.state,
                work::WorkState::Queued
                    | work::WorkState::Running
                    | work::WorkState::WaitingApproval
                    | work::WorkState::WaitingInput
            )
    }) && project.snapshot.attempts.iter().any(|attempt| {
        attempt.id == assignment.attempt.id
            && attempt.runtime_binding == assignment.attempt.runtime_binding
            && attempt.operation_id == assignment.attempt.operation_id
    })
}
fn apply_delivery_outcome(
    p: &mut Project,
    assignment: &wm::dispatch::DispatchAssignment,
    outcome: ui::composer::PromptAdmissionOutcome,
) {
    if matches!(
        p.ledger.stage(&assignment.attempt.operation_id),
        Some(wm::dispatch::DeliveryState::Admitted | wm::dispatch::DeliveryState::Rejected)
    ) {
        return;
    }
    let Some(binding) = assignment.attempt.runtime_binding.as_ref() else {
        return;
    };
    let Some(current) = p
        .snapshot
        .attempts
        .iter()
        .find(|a| a.id == assignment.attempt.id && a.runtime_binding.as_ref() == Some(binding))
    else {
        return;
    };
    let accepted = outcome == ui::composer::PromptAdmissionOutcome::Accepted;
    let unknown = outcome == ui::composer::PromptAdmissionOutcome::Unknown;
    let kind = if accepted {
        p.ledger.note_admitted(
            &assignment.attempt.operation_id,
            &assignment.attempt.id,
            binding,
        );
        work::WorkEventKind::InputAdmitted {
            operation_id: assignment.attempt.operation_id.clone(),
        }
    } else if unknown {
        p.ledger.mark_uncertain(&assignment.attempt.operation_id);
        work::WorkEventKind::ObservationLost
    } else {
        p.ledger.note_rejected(
            &assignment.attempt.operation_id,
            &assignment.attempt.id,
            binding,
        );
        work::WorkEventKind::Failed {
            message: "입력이 승인되지 않아 작업을 실행하지 않았습니다.".into(),
        }
    };
    let event = work::WorkEvent {
        id: work::EventId::new(),
        workspace_id: assignment.task.workspace_id.clone(),
        task_id: assignment.task.id.clone(),
        attempt_id: assignment.attempt.id.clone(),
        runtime_binding: binding.clone(),
        sequence: current.last_event_sequence.saturating_add(1),
        kind,
    };
    let mut state = p.state();
    if matches!(state.apply_event(event), Ok(wm::ApplyOutcome::Applied)) {
        p.set_state(state);
    }
    if let Some(outbox) = p
        .snapshot
        .outbox
        .iter_mut()
        .find(|o| o.operation_id == assignment.attempt.operation_id)
    {
        outbox.state = if accepted {
            stored::DispatchOutboxState::Admitted
        } else if unknown {
            stored::DispatchOutboxState::Uncertain
        } else {
            stored::DispatchOutboxState::Acknowledged
        };
        p.dirty();
    }
}

fn finish_input_outcome(
    host: &mut CommandCenterHost,
    id: &str,
    outcome: ui::composer::PromptAdmissionOutcome,
) {
    let Some(input) = host.inputs.remove(id) else {
        return;
    };
    let accepted = outcome == ui::composer::PromptAdmissionOutcome::Accepted;
    let unknown = outcome == ui::composer::PromptAdmissionOutcome::Unknown;
    if let Some(p) = host.projects.get_mut(&input.target.workspace_id) {
        if let Some(marker) = p
            .snapshot
            .drafts
            .iter_mut()
            .find(|d| d.target_key == format!("operation:{id}"))
        {
            marker.delivery_uncertain = unknown;
            p.dirty();
        }
        if let Some(assignment) = &input.dispatch {
            apply_delivery_outcome(p, assignment, outcome);
        }
    }
    let key = draft_key(host, &input.target).ok();
    if accepted
        && host.input_draft_versions.get(id) == Some(&host.ui.draft_version(&input.target))
        && clear_admitted_draft(&mut host.ui, &input.target, &input.draft)
        && let (Some(key), Some(p)) = (key, host.projects.get_mut(&input.target.workspace_id))
    {
        let _ = set_draft(p, &key, "", false);
    }
    host.finish(
        id,
        if accepted {
            cc::ReceiptStatus::Acknowledged
        } else if unknown {
            cc::ReceiptStatus::Uncertain
        } else {
            cc::ReceiptStatus::Rejected
        },
        if accepted {
            "현재 실행의 입력 승인을 확인했습니다."
        } else if unknown {
            "입력 승인 여부를 확인할 수 없습니다. 자동 재전송하지 않습니다."
        } else {
            "현재 실행이 입력을 거부했습니다. 초안은 보존됩니다."
        },
    );
    // Keep unknown operations live for a correlated late ACK and block implicit retry.
    if unknown {
        host.inputs.insert(id.into(), input);
    }
}

fn clear_admitted_draft(ui: &mut cc::UiState, target: &cc::RouteTarget, submitted: &str) -> bool {
    if submitted.is_empty() || !ui.draft(target).nfc().eq(submitted.nfc()) {
        return false;
    }
    ui.clear_draft(target);
    true
}

fn store_composer_draft(
    host: &mut CommandCenterHost,
    target: cc::RouteTarget,
    text: String,
) -> Result<(), String> {
    if text.len() > work::WORK_INPUT_MAX_BYTES {
        return Err("입력은 16 KiB 이하여야 합니다. 초안은 보존됩니다.".into());
    }
    let key = draft_key(host, &target)?;
    let mut target = target;
    let mut text = text;
    if key == "main" && target.session_id.is_none() {
        let current = host.main_target(&target.workspace_id, &host.projects[&target.workspace_id]);
        if current != target && host.ui.has_draft(&current) {
            if !host.ui.draft(&current).nfc().eq(text.nfc()) {
                return Err("연결 후 편집한 메인 초안을 보존했습니다. 이전 대상의 지연된 편집을 적용하지 않았습니다.".into());
            }
            text = host.ui.draft(&current).to_owned();
            target = current;
        }
    }
    let p = host
        .projects
        .get_mut(&target.workspace_id)
        .ok_or("프로젝트가 없습니다.")?;
    // Authoring survives loading and storage recovery; the load merge preserves
    // these local edits, while host.save still waits for valid storage state.
    set_draft(p, &key, &text, false)?;
    host.ui.set_draft(target, text);
    Ok(())
}

fn draft_key(host: &CommandCenterHost, target: &cc::RouteTarget) -> Result<String, String> {
    let p = host
        .projects
        .get(&target.workspace_id)
        .ok_or("프로젝트가 없습니다.")?;
    if let Some(task_id) = &target.task_id {
        let task = p
            .snapshot
            .tasks
            .iter()
            .find(|t| &t.id.0 == task_id && t.workspace_id.0 == target.workspace_id)
            .ok_or("현재 프로젝트의 작업이 아닙니다.")?;
        let expected = if let Some(remote) = remote::target_for(host, task) {
            remote
        } else {
            let runtime = task
                .current_attempt_id
                .as_ref()
                .and_then(|id| p.snapshot.attempts.iter().find(|a| &a.id == id))
                .and_then(|a| a.runtime_binding.as_ref());
            let live = runtime.and_then(|runtime| {
                host.live.iter().find(|live| {
                    live.capture.workspace == target.workspace_id
                        && host
                            .routes
                            .key(&live.capture)
                            .is_some_and(|key| binding(live, key).as_ref() == Some(runtime))
                })
            });
            let mut expected =
                live.map(|live| host.target(live))
                    .unwrap_or_else(|| cc::RouteTarget {
                        workspace_id: target.workspace_id.clone(),
                        ..Default::default()
                    });
            expected.task_id = Some(task_id.clone());
            expected
        };
        if expected != *target {
            return Err("초안의 작업 실행 대상이 바뀌었습니다.".into());
        }
        return Ok(format!("task:{task_id}"));
    }
    // The no-session route is the logical main draft before binding. Delayed
    // local edits still address that draft after binding, never an execution.
    if target.session_id.is_none() || host.main_target(&target.workspace_id, p) == *target {
        return Ok("main".into());
    }
    let live = host
        .route(target)
        .ok_or("초안의 실행 대상이 바뀌었습니다.")?;
    persistent(live)
        .and_then(|b| serde_json::to_string(&b).ok())
        .map(|b| format!("session:{b}"))
        .ok_or_else(|| "세션 초안을 영구 바인딩할 수 없습니다.".into())
}
fn set_draft(p: &mut Project, key: &str, text: &str, uncertain: bool) -> Result<(), String> {
    if let Some(draft) = p.snapshot.drafts.iter_mut().find(|d| d.target_key == key) {
        draft.text = text.into();
        draft.delivery_uncertain = uncertain;
    } else {
        if p.snapshot.drafts.len() >= work::WORK_DRAFT_MAX_COUNT {
            return Err("초안 저장 한도입니다.".into());
        }
        p.snapshot.drafts.push(stored::WorkDraft {
            target_key: key.into(),
            text: text.into(),
            delivery_uncertain: uncertain,
        });
    }
    p.dirty();
    Ok(())
}
fn work_state(state: work::WorkState) -> cc::TaskState {
    use work::WorkState as W;
    match state {
        W::PlanReview => cc::TaskState::PlanReview,
        W::Queued => cc::TaskState::Queued,
        W::Running => cc::TaskState::Running,
        W::WaitingApproval => cc::TaskState::WaitingApproval,
        W::WaitingInput => cc::TaskState::WaitingInput,
        W::ResultSubmitted => cc::TaskState::ResultSubmitted,
        W::VerificationFailed => cc::TaskState::VerificationFailed,
        W::AwaitingUserConfirmation => cc::TaskState::AwaitingUserConfirmation,
        W::Completed => cc::TaskState::Completed,
        W::Failed => cc::TaskState::Failed,
        W::Cancelled => cc::TaskState::Cancelled,
        W::ObservationUnknown => cc::TaskState::ObservationUnknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn host() -> CommandCenterHost {
        CommandCenterHost::new(
            std::env::temp_dir().join(format!("cc-inert-{}", uuid::Uuid::new_v4())),
            &egui::Context::default(),
        )
    }
    fn target() -> cc::RouteTarget {
        cc::RouteTarget {
            workspace_id: "workspace-a".into(),
            session_id: Some("opaque-current-route".into()),
            task_id: None,
        }
    }
    fn project() -> Project {
        let ws = storage::WorkspaceRow {
            id: "workspace-a".into(),
            name: "A".into(),
            path: "/real/project".into(),
            created_at: String::new(),
        };
        Project {
            snapshot: project_snapshot(&ws),
            revision: None,
            generation: 0,
            saved_generation: 0,
            loaded: true,
            storage_blocked: false,
            scopes: vec![],
            allowed_agents: vec![],
            request: None,
            proposal: None,
            confirmed: BTreeMap::new(),
            ledger: Default::default(),
            catalog: None,
            catalog_requested: false,
            worktree_root: None,
            effect_fence: remote::EffectFence::new(),
        }
    }
    #[test]
    fn operation_payload_and_unknown_receipt_cannot_be_replayed_after_receipt_eviction() {
        let mut h = host();
        let id = uuid::Uuid::new_v4().to_string();
        let intent = cc::Intent::Submit {
            target: target(),
            text: "actual input".into(),
        };
        h.enqueue(id.clone(), intent.clone());
        assert_eq!(h.pending.len(), 1);
        h.pending.clear();
        h.finish(&id, cc::ReceiptStatus::Uncertain, "writer ACK unknown");
        for _ in 0..70 {
            h.finish(
                &uuid::Uuid::new_v4().to_string(),
                cc::ReceiptStatus::Acknowledged,
                "other",
            );
        }
        assert_eq!(h.receipts.len(), 64);
        h.enqueue(id.clone(), intent);
        assert!(
            h.pending.is_empty(),
            "evicting display receipts cannot reopen an operation ID"
        );
        h.enqueue(
            id,
            cc::Intent::Submit {
                target: target(),
                text: "changed input".into(),
            },
        );
        assert!(h.pending.is_empty());
    }
    #[test]
    fn frequent_draft_changes_coalesce_without_consuming_operation_capacity() {
        let mut h = host();
        for n in 0..2048 {
            h.enqueue(
                uuid::Uuid::new_v4().to_string(),
                cc::Intent::DraftChanged {
                    target: target(),
                    text: n.to_string(),
                },
            );
        }
        assert!(h.operations.is_empty());
        assert_eq!(h.pending.len(), 1);
        assert!(matches!(&h.pending[0].1,cc::Intent::DraftChanged { text,.. } if text == "2047"));
    }
    #[test]
    fn saved_revision_ack_does_not_replace_newer_text_or_dirty_generation() {
        let mut p = project();
        set_draft(&mut p, "main", "saved text", true).unwrap();
        let admitted_generation = p.generation;
        let persisted = p.snapshot.clone();
        set_draft(&mut p, "main", "new text while saving", false).unwrap();
        p.ack_saved(
            stored::ProjectRevision::new(1).unwrap(),
            admitted_generation,
        );
        assert_eq!(p.snapshot.drafts[0].text, "new text while saving");
        assert_eq!(persisted.drafts[0].text, "saved text");
        assert!(p.generation > p.saved_generation);
    }
    #[test]
    fn composer_host_unbound_draft_remains_addressable_after_exact_main_binding() {
        let mut h = host();
        let mut p = project();
        let live = LiveRoute {
            capture: Capture {
                workspace: "workspace-a".into(),
                target: FleetTarget::Pty {
                    session: runtime::SessionId(1),
                    tab: runtime::MuxTabId::new(),
                    pane: runtime::MuxPaneId::new(),
                },
                prompt: None,
            },
            title: "Actual main".into(),
            agent: None,
            state: cc::TaskState::Queued,
            cwd: None,
            watch: None,
            runtime_binding: None,
        };
        let before = cc::RouteTarget {
            workspace_id: "workspace-a".into(),
            ..Default::default()
        };
        set_draft(&mut p, "main", "same project draft", false).unwrap();
        h.routes.refresh(vec![live.capture.clone()]);
        h.live.push(live.clone());
        p.snapshot.main_binding = persistent(&live);
        h.projects.insert("workspace-a".into(), p);
        let after = h.main_target("workspace-a", &h.projects["workspace-a"]);
        assert!(after.session_id.is_some());
        assert_eq!(draft_key(&h, &before).unwrap(), "main");
        assert_eq!(draft_key(&h, &after).unwrap(), "main");
        assert!(
            h.route(&before).is_none(),
            "draft compatibility must not authorize execution"
        );
        assert!(h.inputs.is_empty());
        assert!(h.flight.is_none());
        assert_eq!(
            h.projects["workspace-a"].snapshot.drafts[0].text,
            "same project draft"
        );
        h.ui.set_draft(after.clone(), "new bound edit".into());
        set_draft(
            h.projects.get_mut("workspace-a").unwrap(),
            "main",
            "new bound edit",
            false,
        )
        .unwrap();
        assert!(store_composer_draft(&mut h, before, "late prebind edit".into()).is_err());
        assert_eq!(h.ui.draft(&after), "new bound edit");
        assert_eq!(
            h.projects["workspace-a"].snapshot.drafts[0].text,
            "new bound edit"
        );
    }
    #[test]
    fn composer_host_stale_task_draft_cannot_overwrite_current_task_text() {
        let (mut p, assignment) = attempted();
        let key = format!("task:{}", assignment.task.id.0);
        set_draft(&mut p, &key, "current task draft", false).unwrap();
        let mut h = host();
        h.projects.insert("workspace-a".into(), p);
        let stale = cc::RouteTarget {
            workspace_id: "workspace-a".into(),
            session_id: Some("retired-execution".into()),
            task_id: Some(assignment.task.id.0),
        };
        assert!(store_composer_draft(&mut h, stale, "stale edit".into()).is_err());
        assert_eq!(
            h.projects["workspace-a"]
                .snapshot
                .drafts
                .iter()
                .find(|d| d.target_key == key)
                .unwrap()
                .text,
            "current task draft"
        );
    }

    #[test]
    fn composer_host_preserves_local_draft_during_load_and_storage_recovery() {
        let mut h = host();
        let mut p = project();
        p.loaded = false;
        p.storage_blocked = true;
        h.projects.insert("workspace-a".into(), p);
        let target = cc::RouteTarget {
            workspace_id: "workspace-a".into(),
            ..Default::default()
        };
        store_composer_draft(&mut h, target.clone(), "  \n가".into()).unwrap();
        assert_eq!(h.ui.draft(&target), "  \n가");
        assert_eq!(h.projects["workspace-a"].snapshot.drafts[0].text, "  \n가");
        assert!(h.projects["workspace-a"].generation > 0);
        assert!(h.inputs.is_empty());
        assert!(h.save("workspace-a", None, None).is_err());
        assert!(h.flight.is_none());
        assert!(store_composer_draft(&mut h, target.clone(), "가".repeat(6000)).is_err());
        assert_eq!(h.ui.draft(&target), "  \n가");
    }
    #[test]
    fn composer_load_capacity_conflict_preserves_local_draft_and_blocks_storage_until_retry() {
        let mut h = host();
        let mut current = project();
        current.loaded = false;
        let mut loaded = stored::StoredProject {
            revision: stored::ProjectRevision::new(1).unwrap(),
            snapshot: current.snapshot.clone(),
        };
        loaded.snapshot.drafts = (0..work::WORK_DRAFT_MAX_COUNT)
            .map(|n| stored::WorkDraft {
                target_key: format!("stored-session-{n}"),
                text: format!("independent stored draft {n}"),
                delivery_uncertain: false,
            })
            .collect();
        let loaded_before = loaded.clone();
        h.projects.insert("workspace-a".into(), current);
        let target = cc::RouteTarget {
            workspace_id: "workspace-a".into(),
            ..Default::default()
        };
        store_composer_draft(&mut h, target.clone(), "typed while loading".into()).unwrap();
        finish_project_load(
            &mut h,
            "workspace-a",
            None,
            Ok(Some(Box::new(loaded.clone()))),
        );
        assert!(h.projects["workspace-a"].storage_blocked);
        assert!(!h.projects["workspace-a"].loaded);
        assert_eq!(h.ui.draft(&target), "typed while loading");
        assert_eq!(
            h.projects["workspace-a"].snapshot.drafts[0].text,
            "typed while loading"
        );
        assert!(h.status.contains("초안"));
        assert!(h.save("workspace-a", None, None).is_err());
        assert!(h.flight.is_none());
        assert_eq!(
            loaded, loaded_before,
            "the stored record must remain recoverable"
        );
        // Retry can install a snapshot with room without dropping either retained owner.
        loaded.snapshot.drafts.pop();
        finish_project_load(&mut h, "workspace-a", None, Ok(Some(Box::new(loaded))));
        assert!(!h.projects["workspace-a"].storage_blocked);
        assert!(h.projects["workspace-a"].loaded);
        assert_eq!(
            h.projects["workspace-a"].snapshot.drafts.len(),
            work::WORK_DRAFT_MAX_COUNT
        );
        assert_eq!(h.ui.draft(&target), "typed while loading");
        assert_eq!(
            h.projects["workspace-a"]
                .snapshot
                .drafts
                .iter()
                .find(|d| d.target_key == "main")
                .unwrap()
                .text,
            "typed while loading"
        );
        assert!(h.inputs.is_empty());
    }

    #[test]
    fn composer_load_does_not_overwrite_a_newer_ui_edit_waiting_for_host_processing() {
        let mut h = host();
        let mut p = project();
        p.loaded = false;
        let loaded = stored::StoredProject {
            revision: stored::ProjectRevision::new(1).unwrap(),
            snapshot: p.snapshot.clone(),
        };
        h.projects.insert("workspace-a".into(), p);
        let target = cc::RouteTarget {
            workspace_id: "workspace-a".into(),
            ..Default::default()
        };
        store_composer_draft(&mut h, target.clone(), "processed local text".into()).unwrap();
        h.ui.set_draft(target.clone(), "new edit awaiting DraftChanged".into());
        finish_project_load(&mut h, "workspace-a", None, Ok(Some(Box::new(loaded))));
        assert_eq!(h.ui.draft(&target), "new edit awaiting DraftChanged");
        assert!(h.inputs.is_empty());
    }

    #[test]
    fn composer_late_ack_preserves_same_text_after_new_edits() {
        let mut h = host();
        let mut p = project();
        let live = LiveRoute {
            capture: Capture {
                workspace: "workspace-a".into(),
                target: FleetTarget::Pty {
                    session: runtime::SessionId(1),
                    tab: runtime::MuxTabId::new(),
                    pane: runtime::MuxPaneId::new(),
                },
                prompt: None,
            },
            title: "Actual bound main".into(),
            agent: None,
            state: cc::TaskState::Queued,
            cwd: None,
            watch: None,
            runtime_binding: None,
        };
        p.snapshot.main_binding = persistent(&live);
        h.routes.refresh(vec![live.capture.clone()]);
        h.live.push(live);
        h.projects.insert("workspace-a".into(), p);
        let target = h.main_target("workspace-a", &h.projects["workspace-a"]);
        assert!(target.session_id.is_some() && h.route(&target).is_some());
        store_composer_draft(&mut h, target.clone(), "가 submitted".into()).unwrap();
        h.enqueue(
            "actual-pending-input".into(),
            cc::Intent::Submit {
                target: target.clone(),
                text: "가 submitted".into(),
            },
        );
        assert!(matches!(
            h.pending.pop_front(),
            Some((_, cc::Intent::Submit { .. }))
        ));
        h.inputs.insert(
            "actual-pending-input".into(),
            Input {
                target: target.clone(),
                draft: "가 submitted".into(),
                dispatch: None,
            },
        );
        finish_input_outcome(
            &mut h,
            "actual-pending-input",
            ui::composer::PromptAdmissionOutcome::Unknown,
        );
        assert!(h.inputs.contains_key("actual-pending-input"));
        store_composer_draft(&mut h, target.clone(), "B new edit".into()).unwrap();
        store_composer_draft(&mut h, target.clone(), "가 submitted".into()).unwrap();
        finish_input_outcome(
            &mut h,
            "actual-pending-input",
            ui::composer::PromptAdmissionOutcome::Accepted,
        );
        assert_eq!(h.ui.draft(&target), "가 submitted");
        assert_eq!(
            h.projects["workspace-a"]
                .snapshot
                .drafts
                .iter()
                .find(|d| d.target_key == "main")
                .unwrap()
                .text,
            "가 submitted"
        );
        assert!(!h.inputs.contains_key("actual-pending-input"));
        assert_eq!(
            h.receipts.back().unwrap().status,
            cc::ReceiptStatus::Acknowledged
        );
        assert!(!h.input_draft_versions.contains_key("actual-pending-input"));
        h.enqueue(
            "next-explicit-input".into(),
            cc::Intent::Submit {
                target: target.clone(),
                text: "가 submitted".into(),
            },
        );
        h.inputs.insert(
            "next-explicit-input".into(),
            Input {
                target: target.clone(),
                draft: "가 submitted".into(),
                dispatch: None,
            },
        );
        finish_input_outcome(
            &mut h,
            "next-explicit-input",
            ui::composer::PromptAdmissionOutcome::Accepted,
        );
        assert_eq!(
            h.ui.draft(&target),
            "",
            "an unchanged canonically equivalent draft should clear"
        );
    }

    #[test]
    fn admission_only_clears_the_same_target_and_unchanged_draft() {
        let mut state = cc::UiState::default();
        let target = target();
        state.set_draft(target.clone(), "submitted".into());
        state.set_draft(target.clone(), "edited after submit".into());
        assert!(!clear_admitted_draft(&mut state, &target, "submitted"));
        assert_eq!(state.draft(&target), "edited after submit");
        state.set_draft(target.clone(), "submitted".into());
        assert!(clear_admitted_draft(&mut state, &target, "submitted"));
        assert_eq!(state.draft(&target), "");
        state.set_draft(target.clone(), "가 goal".into());
        assert!(clear_admitted_draft(&mut state, &target, "가 goal"));
        assert_eq!(state.draft(&target), "");
    }
    #[test]
    fn native_dto_encodes_through_the_actual_permission_filtered_web_boundary() {
        let config = cc::ProjectConfig {
            name: "Actual project".into(),
            directory: "/real/project".into(),
            allowed_scope_roots: vec!["src".into()],
            revision: "1".into(),
            ..Default::default()
        };
        let snapshot = cc::Snapshot {
            selected_workspace_id: Some("workspace-a".into()),
            projects: vec![cc::Project {
                id: "workspace-a".into(),
                main_target: target(),
                config: config.clone(),
                tasks: vec![cc::Task {
                    attempt_id: Some("exact-attempt".into()),
                    result_report: yes(),
                    verify_result: yes(),
                    verification: "검증 미실행".into(),
                    ..Default::default()
                }],
                sessions: vec![cc::Session {
                    target: target(),
                    terminal_session_id: Some(uuid::Uuid::new_v4().to_string()),
                    model: Some("observed-model".into()),
                    effort: Some("observed-effort".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let value = serde_json::to_value(snapshot).unwrap();
        for (input, approval) in [(true, true), (true, false), (false, true), (false, false)] {
            assert!(web_remote::work::encode_frame(&value, input, approval).is_ok());
        }
        let intent = cc::Intent::SaveProject {
            workspace_id: "workspace-a".into(),
            config,
        };
        assert!(web_remote::work::validate_intent(
            &serde_json::to_value(intent).unwrap()
        ));
        for intent in [
            cc::Intent::ReportResult {
                target: cc::RouteTarget {
                    task_id: Some("actual-task".into()),
                    ..target()
                },
                attempt_id: "exact-attempt".into(),
                text: "Actual operator report".into(),
            },
            cc::Intent::VerifyResult {
                target: cc::RouteTarget {
                    task_id: Some("actual-task".into()),
                    ..target()
                },
                attempt_id: "exact-attempt".into(),
                revision: "7".into(),
                passed: true,
                evidence: "cargo test: pass".into(),
            },
        ] {
            assert!(web_remote::work::validate_intent(
                &serde_json::to_value(intent).unwrap()
            ));
        }
        assert_eq!(bounded("한글문자", 7), "한글");
    }
    pub(super) fn attempted() -> (Project, wm::dispatch::DispatchAssignment) {
        let mut p = project();
        let mut state = p.state();
        let goal = state
            .submit_goal(
                work::SubmissionId::new(),
                "Goal".into(),
                "actual goal".into(),
            )
            .unwrap();
        let task = work::Task {
            workspace_id: state.workspace_id.clone(),
            id: work::TaskId::new(),
            goal_id: goal.clone(),
            parent_task_id: None,
            role: "actual role".into(),
            scope: "src".into(),
            assigned_agent: p.snapshot.config.worker_agent.clone(),
            execution_device: None,
            title: "Actual task".into(),
            instructions: "actual instructions".into(),
            acceptance_criteria: "actual criteria".into(),
            state: work::WorkState::PlanReview,
            current_attempt_id: None,
            dependencies: vec![],
            result: None,
        };
        let task_id = task.id.clone();
        state
            .install_plan(work::WorkPlan {
                workspace_id: state.workspace_id.clone(),
                goal_id: goal.clone(),
                tasks: vec![task],
            })
            .unwrap();
        state.confirm_plan(&goal).unwrap();
        let binding = work::RuntimeBinding {
            incarnation: work::RuntimeIncarnationId::new(),
            session_id: runtime::SessionId(1),
            execution_generation: 1,
        };
        state
            .begin_attempt(
                &task_id,
                work::OperationId::new(),
                p.snapshot.config.clone(),
                p.snapshot.config.folder.clone(),
                None,
                binding,
            )
            .unwrap();
        let assignment = wm::dispatch::DispatchAssignment {
            task: state.tasks[0].clone(),
            attempt: state.attempts[0].clone(),
            source_results: vec![],
            prompt: "actual prompt".into(),
        };
        p.set_state(state);
        p.ledger.record_durable(assignment.clone()).unwrap();
        assert!(p.ledger.claim_send(&assignment.attempt.operation_id));
        p.snapshot.outbox.push(stored::DispatchOutbox {
            attempt_id: assignment.attempt.id.clone(),
            operation_id: assignment.attempt.operation_id.clone(),
            state: stored::DispatchOutboxState::Pending,
        });
        (p, assignment)
    }
    #[test]
    fn delayed_saved_assignment_cannot_send_after_result_or_attempt_replacement() {
        let (mut project, assignment) = attempted();
        assert!(assignment_current(&project, &assignment));
        project.snapshot.tasks[0].state = work::WorkState::ResultSubmitted;
        assert!(!assignment_current(&project, &assignment));
        project.snapshot.tasks[0].state = work::WorkState::Queued;
        project.snapshot.tasks[0].current_attempt_id = Some(work::AttemptId::new());
        assert!(!assignment_current(&project, &assignment));
    }
    #[test]
    fn terminal_admission_receipt_cannot_be_downgraded_by_a_late_rejection() {
        let (mut project, assignment) = attempted();
        apply_delivery_outcome(
            &mut project,
            &assignment,
            ui::composer::PromptAdmissionOutcome::Accepted,
        );
        apply_delivery_outcome(
            &mut project,
            &assignment,
            ui::composer::PromptAdmissionOutcome::Rejected,
        );
        assert_eq!(project.snapshot.tasks[0].state, work::WorkState::Queued);
        assert_eq!(
            project.snapshot.outbox[0].state,
            stored::DispatchOutboxState::Admitted
        );
        assert_eq!(
            project.ledger.stage(&assignment.attempt.operation_id),
            Some(wm::dispatch::DeliveryState::Admitted)
        );
    }
    #[test]
    fn known_rejection_frees_attempt_capacity_while_unknown_never_retries() {
        let (mut rejected, assignment) = attempted();
        apply_delivery_outcome(
            &mut rejected,
            &assignment,
            ui::composer::PromptAdmissionOutcome::Rejected,
        );
        assert_eq!(rejected.snapshot.tasks[0].state, work::WorkState::Failed);
        assert!(rejected.state().can_dispatch(&assignment.task.id));
        assert_eq!(
            rejected.snapshot.outbox[0].state,
            stored::DispatchOutboxState::Acknowledged
        );
        let (mut unknown, assignment) = attempted();
        apply_delivery_outcome(
            &mut unknown,
            &assignment,
            ui::composer::PromptAdmissionOutcome::Unknown,
        );
        assert_eq!(
            unknown.snapshot.tasks[0].state,
            work::WorkState::ObservationUnknown
        );
        assert!(!unknown.state().can_dispatch(&assignment.task.id));
        assert_eq!(
            unknown.snapshot.outbox[0].state,
            stored::DispatchOutboxState::Uncertain
        );
    }
    #[test]
    fn admission_is_not_a_provider_turn_start_or_success() {
        let (mut p, assignment) = attempted();
        apply_delivery_outcome(
            &mut p,
            &assignment,
            ui::composer::PromptAdmissionOutcome::Accepted,
        );
        assert_eq!(p.snapshot.tasks[0].state, work::WorkState::Queued);
        assert!(p.snapshot.tasks[0].result.is_none());
        assert!(!p.state().goal_is_complete(&assignment.task.goal_id));
    }
    #[test]
    fn same_folder_handles_macos_unicode_and_lexical_components_without_symlink_claims() {
        assert!(same_folder("/tmp/한글/./src/", "/tmp/한글/src"));
        assert!(!same_folder("/tmp/a/../b", "/tmp/b"));
        assert!(!same_folder("relative/path", "relative/path"));
        assert!(!same_folder("/different-volume/한글", "/tmp/한글"));
    }
    #[test]
    fn history_kind_follows_actual_event_variants_through_storage_and_projection() {
        let (mut p, assignment) = attempted();
        let target = cc::RouteTarget {
            workspace_id: p.snapshot.config.workspace_id.0.clone(),
            task_id: Some(assignment.task.id.0.clone()),
            session_id: None,
        };
        let report = work::TaskResult {
            revision: work::ResultRevision(1),
            summary: "actual report".into(),
            source_revision: None,
            artifact_revision: None,
            verification: work::VerificationState::NotRun,
        };
        for (kind, expected) in [
            (work::WorkEventKind::Started, "work"),
            (work::WorkEventKind::WaitingApproval, "approval"),
            (
                work::WorkEventKind::ResultSubmitted { result: report },
                "work",
            ),
            (
                work::WorkEventKind::VerificationFinished {
                    result_revision: work::ResultRevision(1),
                    outcome: work::VerificationState::Passed,
                },
                "work",
            ),
            (
                work::WorkEventKind::UserConfirmed {
                    result_revision: work::ResultRevision(1),
                },
                "approval",
            ),
        ] {
            apply_user_work_event(
                &mut p,
                &target,
                &assignment.attempt.id.0,
                kind,
                "설정 승인 작업이라는 같은 표시 문구".into(),
            )
            .unwrap();
            assert_eq!(p.snapshot.events.last().unwrap().kind, expected);
            let history = approval::history(&p, &[]);
            assert_eq!(history.last().unwrap().kind, expected);
            assert!(
                history.last().unwrap().time.is_empty(),
                "event categories must not invent event times"
            );
        }
    }

    #[test]
    fn manual_report_is_bound_to_the_displayed_attempt_and_requires_separate_verification() {
        let (mut p, assignment) = attempted();
        let target = cc::RouteTarget {
            workspace_id: p.snapshot.config.workspace_id.0.clone(),
            task_id: Some(assignment.task.id.0.clone()),
            session_id: None,
        };
        let result = work::TaskResult {
            revision: work::ResultRevision(1),
            summary: "actual user report".into(),
            source_revision: None,
            artifact_revision: None,
            verification: work::VerificationState::NotRun,
        };
        assert!(
            apply_user_work_event(
                &mut p,
                &target,
                "stale-attempt",
                work::WorkEventKind::ResultSubmitted {
                    result: result.clone()
                },
                "stale".into()
            )
            .is_err()
        );
        assert!(p.snapshot.tasks[0].result.is_none());
        apply_user_work_event(
            &mut p,
            &target,
            &assignment.attempt.id.0,
            work::WorkEventKind::ResultSubmitted { result },
            "manual report".into(),
        )
        .unwrap();
        assert_eq!(p.snapshot.tasks[0].state, work::WorkState::ResultSubmitted);
        assert_eq!(
            p.snapshot.tasks[0].result.as_ref().unwrap().verification,
            work::VerificationState::NotRun
        );
        assert!(
            apply_user_work_event(
                &mut p,
                &target,
                &assignment.attempt.id.0,
                work::WorkEventKind::UserConfirmed {
                    result_revision: work::ResultRevision(1)
                },
                "cannot confirm unverified".into()
            )
            .is_err()
        );
        apply_user_work_event(
            &mut p,
            &target,
            &assignment.attempt.id.0,
            work::WorkEventKind::VerificationFinished {
                result_revision: work::ResultRevision(1),
                outcome: work::VerificationState::Passed,
            },
            "cargo test: passed".into(),
        )
        .unwrap();
        assert_eq!(
            p.snapshot.tasks[0].state,
            work::WorkState::AwaitingUserConfirmation
        );
        assert!(!p.state().goal_is_complete(&assignment.task.goal_id));
        apply_user_work_event(
            &mut p,
            &target,
            &assignment.attempt.id.0,
            work::WorkEventKind::UserConfirmed {
                result_revision: work::ResultRevision(1),
            },
            "confirmed".into(),
        )
        .unwrap();
        assert!(p.state().goal_is_complete(&assignment.task.goal_id));
    }
    #[test]
    fn restore_preserves_durable_result_review_and_invalidates_only_active_observation() {
        for state in [
            work::WorkState::Queued,
            work::WorkState::Running,
            work::WorkState::ResultSubmitted,
            work::WorkState::VerificationFailed,
            work::WorkState::AwaitingUserConfirmation,
            work::WorkState::Completed,
        ] {
            let (mut p, _) = attempted();
            p.snapshot.tasks[0].state = state;
            p.snapshot.attempts[0].state = state;
            recover_observation(&mut p.snapshot);
            let expected = if matches!(state, work::WorkState::Queued | work::WorkState::Running) {
                work::WorkState::ObservationUnknown
            } else {
                state
            };
            assert_eq!(p.snapshot.tasks[0].state, expected);
            assert_eq!(p.snapshot.attempts[0].state, expected);
        }
    }
    #[test]
    fn active_cwd_cannot_be_lent_to_a_warm_workspace_with_the_same_session_number() {
        // SessionId(1) exists in both runtimes; the cache contains only active A.
        assert_eq!(
            active_workspace_cwd("A", "A", Some("/project-A".into())),
            Some("/project-A".into())
        );
        assert_eq!(
            active_workspace_cwd("B", "A", Some("/project-A".into())),
            None
        );
    }
    #[test]
    fn rejected_partial_settings_edit_preserves_original_project_on_full_draft_capacity() {
        let mut host = host();
        let mut p = project();
        let original = p.snapshot.config.clone();
        p.snapshot.drafts = (0..work::WORK_DRAFT_MAX_COUNT)
            .map(|n| stored::WorkDraft {
                target_key: format!("draft-{n}"),
                text: String::new(),
                delivery_uncertain: false,
            })
            .collect();
        host.projects.insert("workspace-a".into(), p);
        let result = project_transaction(&mut host, Some("workspace-a"), |host| {
            let p = host.projects.get_mut("workspace-a").unwrap();
            p.snapshot.config.name = "partial rejected edit".into();
            set_draft(p, "settings:scopes", r#"["src"]"#, false)
        });
        assert!(result.is_err());
        assert_eq!(host.projects["workspace-a"].snapshot.config, original);
        assert_eq!(host.projects["workspace-a"].generation, 0);
    }
    fn completed_project(workspace: &str, store: &mut stored::CommandCenterStore) -> Project {
        let (mut p, _) = attempted();
        let workspace_id = WorkspaceId(workspace.into());
        p.snapshot.config.workspace_id = workspace_id.clone();
        for goal in &mut p.snapshot.goals {
            goal.workspace_id = workspace_id.clone();
        }
        for task in &mut p.snapshot.tasks {
            task.workspace_id = workspace_id.clone();
        }
        for attempt in &mut p.snapshot.attempts {
            attempt.workspace_id = workspace_id.clone();
            attempt.config.workspace_id = workspace_id.clone();
        }
        p.scopes = vec!["src".into()];
        p.allowed_agents = vec![p.snapshot.config.worker_agent.clone()];
        p.ledger = Default::default();
        let assignment = wm::dispatch::DispatchAssignment {
            task: p.snapshot.tasks[0].clone(),
            attempt: p.snapshot.attempts[0].clone(),
            source_results: vec![],
            prompt: "actual dispatched instructions".into(),
        };
        p.ledger.record_durable(assignment.clone()).unwrap();
        assert!(p.ledger.claim_send(&assignment.attempt.operation_id));
        p.request = Some(
            wm::plan::PlannerRequest::new(&p.snapshot.goals[0], &p.snapshot.config, &p.policy())
                .unwrap(),
        );
        let mut reviewed_task = p.snapshot.tasks[0].clone();
        reviewed_task.state = work::WorkState::PlanReview;
        reviewed_task.current_attempt_id = None;
        let reviewed = work::WorkPlan {
            workspace_id,
            goal_id: reviewed_task.goal_id.clone(),
            tasks: vec![reviewed_task],
        };
        let token = wm::dispatch::ConfirmedPlan::confirm_remaining(
            &p.state(),
            &reviewed,
            &p.snapshot.config,
            &p.policy(),
        )
        .unwrap();
        p.confirmed.insert(
            token.goal_id().0.clone(),
            assignment::Confirmed::Local(token),
        );
        p.snapshot
            .plan_confirmations
            .push(stored::PlanConfirmation {
                version: 1,
                config: p.snapshot.config.clone(),
                plan: reviewed,
            });
        apply_delivery_outcome(
            &mut p,
            &assignment,
            ui::composer::PromptAdmissionOutcome::Accepted,
        );
        let target = cc::RouteTarget {
            workspace_id: workspace.into(),
            session_id: None,
            task_id: Some(assignment.task.id.0.clone()),
        };
        for kind in [
            work::WorkEventKind::ResultSubmitted {
                result: work::TaskResult {
                    revision: work::ResultRevision(1),
                    summary: "actual completed work".into(),
                    source_revision: None,
                    artifact_revision: None,
                    verification: work::VerificationState::NotRun,
                },
            },
            work::WorkEventKind::VerificationFinished {
                result_revision: work::ResultRevision(1),
                outcome: work::VerificationState::Passed,
            },
            work::WorkEventKind::UserConfirmed {
                result_revision: work::ResultRevision(1),
            },
        ] {
            apply_user_work_event(
                &mut p,
                &target,
                &assignment.attempt.id.0,
                kind,
                "actual operator verification".into(),
            )
            .unwrap();
        }
        assert!(p.state().goal_is_complete(&p.snapshot.goals[0].id));
        let stored::SaveOutcome::Saved { revision } =
            store.save_workspace(&p.snapshot, None).unwrap()
        else {
            panic!("independent project must save")
        };
        p.ack_saved(revision, p.generation);
        assert_eq!(
            store
                .load_workspace(&p.snapshot.config.workspace_id)
                .unwrap()
                .unwrap()
                .snapshot,
            p.snapshot
        );
        p
    }
    #[test]
    fn four_independently_persisted_completed_projects_can_admit_a_fifth() {
        let dir = std::env::temp_dir().join(format!("cc-eviction-{}", uuid::Uuid::new_v4()));
        let mut store = stored::CommandCenterStore::open(&dir).unwrap();
        let mut h = host();
        for workspace in ["active", "completed-a", "completed-b", "completed-c"] {
            h.projects
                .insert(workspace.into(), completed_project(workspace, &mut store));
        }
        h.projects.insert(
            "fifth".into(),
            Project {
                loaded: false,
                ..project()
            },
        );
        let durable = store
            .load_workspace(&WorkspaceId("completed-a".into()))
            .unwrap()
            .unwrap();
        admit_project_load(&mut h, "fifth", "active").unwrap();
        assert!(!h.projects["completed-a"].loaded);
        assert!(h.projects["active"].loaded);
        assert!(h.projects["completed-a"].request.is_none());
        assert!(h.projects["completed-a"].confirmed.is_empty());
        assert_eq!(
            store
                .load_workspace(&WorkspaceId("completed-a".into()))
                .unwrap()
                .unwrap(),
            durable,
            "cache eviction cannot delete durable work"
        );
    }
    #[test]
    fn cache_eviction_preserves_retryable_work_and_every_pending_effect_owner() {
        for blocker in 0..9 {
            let dir = std::env::temp_dir().join(format!(
                "cc-eviction-block-{blocker}-{}",
                uuid::Uuid::new_v4()
            ));
            let mut store = stored::CommandCenterStore::open(&dir).unwrap();
            let mut h = host();
            for workspace in ["active", "protected", "dirty-a", "dirty-b"] {
                let mut p = completed_project(workspace, &mut store);
                if workspace.starts_with("dirty") {
                    p.dirty();
                }
                h.projects.insert(workspace.into(), p);
            }
            h.projects.insert(
                "fifth".into(),
                Project {
                    loaded: false,
                    ..project()
                },
            );
            let task = h.projects["protected"].snapshot.tasks[0].id.0.clone();
            let target = cc::RouteTarget {
                workspace_id: "protected".into(),
                task_id: Some(task),
                session_id: None,
            };
            match blocker {
                0 => {
                    h.projects.get_mut("protected").unwrap().snapshot.tasks[0].state =
                        work::WorkState::VerificationFailed
                }
                1 => {
                    h.projects.get_mut("protected").unwrap().snapshot.tasks[0].state =
                        work::WorkState::ResultSubmitted
                }
                2 => {
                    h.projects.get_mut("protected").unwrap().snapshot.outbox[0].state =
                        stored::DispatchOutboxState::Uncertain
                }
                3 => {
                    h.inputs.insert(
                        "pending-input".into(),
                        Input {
                            target,
                            draft: "pending actual input".into(),
                            dispatch: None,
                        },
                    );
                }
                4 => {
                    h.pending_configs.insert(
                        "protected".into(),
                        ("prepare-root".into(), cc::ProjectConfig::default()),
                    );
                }
                5 => {
                    h.flight = Some(Flight::Save {
                        workspace: "protected".into(),
                        generation: 1,
                        after: None,
                        receipt: None,
                    })
                }
                6 => h.pending.push_back((
                    "pending-user-action".into(),
                    cc::Intent::ReportResult {
                        target,
                        attempt_id: "pending-attempt".into(),
                        text: "actual result".into(),
                    },
                )),
                7 => h
                    .projects
                    .get_mut("protected")
                    .unwrap()
                    .snapshot
                    .drafts
                    .push(stored::WorkDraft {
                        target_key: "operation:unknown".into(),
                        text: "uncertain operation".into(),
                        delivery_uncertain: true,
                    }),
                _ => {
                    h.projects.get_mut("protected").unwrap().snapshot.tasks[0].state =
                        work::WorkState::Cancelled
                }
            }
            assert!(
                admit_project_load(&mut h, "fifth", "active").is_err(),
                "blocker {blocker} must retain its project"
            );
            assert!(h.projects["protected"].loaded);
        }
    }
    #[test]
    fn bounded_project_load_evicts_only_clean_inactive_and_preserves_pending_work() {
        let mut h = host();
        for name in ["active", "clean", "dirty", "working"] {
            let mut p = project();
            if name == "dirty" {
                p.dirty();
            }
            if name == "working" {
                p = attempted().0;
                p.saved_generation = p.generation;
            }
            h.projects.insert(name.into(), p);
        }
        h.projects.insert(
            "requested".into(),
            Project {
                loaded: false,
                ..project()
            },
        );
        admit_project_load(&mut h, "requested", "active").unwrap();
        assert!(!h.projects["clean"].loaded);
        assert!(h.projects["active"].loaded);
        assert!(h.projects["dirty"].loaded);
        assert!(h.projects["working"].loaded);
        h.projects.get_mut("requested").unwrap().loaded = true;
        h.projects.insert(
            "another".into(),
            Project {
                loaded: false,
                ..project()
            },
        );
        assert!(admit_project_load(&mut h, "another", "requested").is_ok());
    }
    #[test]
    fn restored_launch_before_attempt_commit_is_unknown_and_never_replayed() {
        let mut h = host();
        let mut p = project();
        let record = launch::Record {
            operation_id: work::OperationId::new(),
            attempt_id: work::AttemptId::new(),
            workspace: p.snapshot.config.workspace_id.clone(),
            task_id: Some(work::TaskId::new()),
            config_revision: p.snapshot.config.revision,
            selection: p.snapshot.config.worker_agent.clone(),
            folder: p.snapshot.config.folder.clone(),
            phase: launch::Phase::SpawnRequested,
        };
        launch::save_record(&mut p, &record).unwrap();
        let generation = p.generation;
        launch::save_record(&mut p, &record).unwrap();
        assert_eq!(
            p.generation, generation,
            "unchanged launch marker cannot keep autosave awake"
        );
        h.projects.insert("workspace-a".into(), p);
        launch::restore_records(&mut h, "workspace-a");
        assert!(launch::workspace_uncertain(&h.launches, "workspace-a"));
        let pending = &h.launches[&record.operation_id.0];
        assert!(matches!(pending.stage, launch::Stage::Done));
        assert!(pending.request.is_none());
        launch::restore_records(&mut h, "workspace-a");
        assert_eq!(h.launches.len(), 1);
        assert!(h.inputs.is_empty());
        assert!(h.flight.is_none());
    }
    #[test]
    fn completed_launch_history_does_not_limit_a_valid_sixty_four_task_project_to_thirty_two() {
        let mut h = host();
        let mut p = project();
        for _ in 0..64 {
            let record = launch::Record {
                operation_id: work::OperationId::new(),
                attempt_id: work::AttemptId::new(),
                workspace: p.snapshot.config.workspace_id.clone(),
                task_id: Some(work::TaskId::new()),
                config_revision: p.snapshot.config.revision,
                selection: p.snapshot.config.worker_agent.clone(),
                folder: p.snapshot.config.folder.clone(),
                phase: launch::Phase::Completed,
            };
            launch::save_record(&mut p, &record).unwrap();
        }
        h.projects.insert("workspace-a".into(), p);
        launch::restore_records(&mut h, "workspace-a");
        assert_eq!(h.launches.len(), 64);
        assert!(!h.projects["workspace-a"].storage_blocked);
        assert!(!launch::workspace_uncertain(&h.launches, "workspace-a"));
        assert_eq!(h.launches.values().filter(|p| p.preparing()).count(), 0);
    }
    #[test]
    fn corrupt_launch_marker_blocks_project_instead_of_losing_dedup_history() {
        let mut h = host();
        let mut p = project();
        set_draft(&mut p, "launch:corrupt", "invalid json", true).unwrap();
        h.projects.insert("workspace-a".into(), p);
        launch::restore_records(&mut h, "workspace-a");
        assert!(h.projects["workspace-a"].storage_blocked);
        assert!(h.launches.is_empty());
    }
    #[test]
    fn two_distinct_project_load_operations_never_leave_an_unreceipted_waiter() {
        let mut h = host();
        h.request_project_load("first-op", "workspace-a").unwrap();
        assert!(h.request_project_load("second-op", "workspace-a").is_err());
        assert_eq!(h.requested_loads["workspace-a"], "first-op");
        h.requested_loads.clear();
        h.flight = Some(Flight::Load {
            workspace: "workspace-a".into(),
            token: 1,
            receipt: Some("first-op".into()),
        });
        assert!(h.request_project_load("third-op", "workspace-a").is_err());
        assert!(h.requested_loads.is_empty());
    }
}
