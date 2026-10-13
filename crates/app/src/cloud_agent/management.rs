//! App-side cloud work/receipt coordination. All persistence traverses HistoryWorker.
use super::{CloudAgent, Request, Target, field, truncate_utf8};
use crate::cloud_history_worker::{Job, ResultKind};
use agent_mcp::{Auth, CloudRun, RunQuery, RunState, RunUpdate, now};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};

const MAX_PENDING: usize = 16;
const MAX_PENDING_BYTES: usize = 512 * 1024;
const MAX_CACHED_RUNS: usize = 100;
const MAX_CACHE_BYTES: usize = 1024 * 1024;
const HEARTBEAT_TIMEOUT: i64 = 60;

struct Fence {
    request: Request,
    target: Option<Target>,
    auth: Arc<Auth>,
}
enum Stage {
    Start(CloudRun),
    Update { id: String, patch: Value },
    Get,
    Lookup { id: String },
    List,
    Bootstrap,
    Cancel { connection: String, id: String },
    Stale { connection: String, id: String },
    Save { run: CloudRun, status: &'static str },
}
struct Pending {
    fence: Option<Fence>,
    stage: Stage,
    queued: Option<Job>,
}
impl Pending {
    fn mutation_key(&self) -> Option<(&str, &str)> {
        match &self.stage {
            Stage::Start(run) | Stage::Save { run, .. } => Some((&run.connection_id, &run.id)),
            Stage::Update { id, .. } => Some((&self.fence.as_ref()?.request.principal.id, id)),
            Stage::Cancel { connection, id } | Stage::Stale { connection, id } => {
                Some((connection, id))
            }
            _ => None,
        }
    }
    fn bytes(&self) -> usize {
        let payload = match &self.stage {
            Stage::Start(run) | Stage::Save { run, .. } => {
                serde_json::to_vec(run).map_or(agent_mcp::MAX_RUN_BYTES, |v| v.len())
            }
            Stage::Update { patch, .. } => {
                serde_json::to_vec(patch).map_or(agent_mcp::MAX_RUN_BYTES, |v| v.len())
            }
            _ => 1024,
        };
        payload
            + 2048
            + if self.queued.is_some() {
                agent_mcp::MAX_RUN_BYTES + 2048
            } else {
                0
            }
    }
}

#[derive(Default)]
pub(super) struct Management {
    runs: Vec<CloudRun>,
    pending: HashMap<u64, Pending>,
    bootstrap_started: bool,
}
impl CloudAgent {
    pub(super) fn prepare_management(
        &mut self,
        req: &Request,
        target: Option<&Target>,
    ) -> Result<Option<Value>, String> {
        if !self.history_ready {
            return Err("history_unavailable_no_effect".into());
        }
        let server = self
            .server
            .as_ref()
            .ok_or("expired_or_revoked_request_no_effect")?;
        if !req.live(&server.auth)
            || target.is_some_and(|target| !self.management_request_current(req, target))
        {
            return Err("session_or_permission_changed_no_effect".into());
        }
        let args = &req.args;
        let (stage, job) = match req.tool.as_str() {
            "start_run" => {
                let target = target.ok_or("invalid_session_id")?;
                let id = field(args, "run_id", 128)?;
                let title = self.management_text(field(args, "title", 256)?, 256)?;
                let instruction = self.management_text(
                    args.get("instruction")
                        .map(|v| v.as_str().ok_or("invalid_instruction"))
                        .transpose()?
                        .unwrap_or(""),
                    8192,
                )?;
                let run = CloudRun::new(
                    id,
                    &req.principal.id,
                    &self.redact(&req.principal.name),
                    &target.workspace,
                    &target.id,
                    &target.generation,
                    &title,
                    &instruction,
                    now() as i64,
                )
                .map_err(|_| "invalid_run")?;
                let job = Job::GetRun {
                    id: run.id.clone(),
                    connection_id: run.connection_id.clone(),
                };
                (Stage::Start(run), job)
            }
            "update_run" => {
                let id = field(args, "run_id", 128)?.to_owned();
                let mut patch = args.as_object().ok_or("invalid_arguments")?.clone();
                patch.remove("session_id");
                patch.remove("generation");
                patch.remove("run_id");
                let mut patch = Value::Object(patch);
                let clean = self.management_update(&patch, None)?;
                for (key, value) in [
                    ("message", json!(clean.message)),
                    ("result", json!(clean.result)),
                    ("artifacts", json!(clean.artifacts)),
                ] {
                    if patch.get(key).is_some() {
                        patch[key] = value;
                    }
                }
                (
                    Stage::Update {
                        id: id.clone(),
                        patch,
                    },
                    Job::GetRun {
                        id,
                        connection_id: req.principal.id.clone(),
                    },
                )
            }
            "get_run" => (
                Stage::Get,
                Job::GetRun {
                    id: field(args, "run_id", 128)?.into(),
                    connection_id: req.principal.id.clone(),
                },
            ),
            "get_operation" => {
                let target = target.ok_or("invalid_session_id")?;
                let id = field(args, "operation_id", 128)?.to_owned();
                agent_mcp::operation_key(&req.principal.id, &id)
                    .map_err(|_| "invalid_operation_id")?;
                (
                    Stage::Lookup { id: id.clone() },
                    Job::LookupTarget {
                        id,
                        connection_id: req.principal.id.clone(),
                        workspace: target.workspace.clone(),
                        session: target.id.clone(),
                        generation: target.generation.clone(),
                    },
                )
            }
            "list_runs" => {
                let state = args
                    .get("state")
                    .map(|value| {
                        serde_json::from_value::<RunState>(value.clone())
                            .map_err(|_| "invalid_run_state")
                    })
                    .transpose()?;
                let session = args
                    .get("session_id")
                    .map(|_| field(args, "session_id", 128).map(str::to_owned))
                    .transpose()?;
                let generation = args
                    .get("generation")
                    .map(|_| field(args, "generation", 128).map(str::to_owned))
                    .transpose()?;
                if session.is_some() != generation.is_some() {
                    return Err("invalid_arguments".into());
                }
                if let (Some(session), Some(generation)) = (&session, &generation) {
                    let target = self.authorize(session, generation, false)?;
                    self.authorize_connection(&req.principal, &target, false)?;
                }
                let query = RunQuery {
                    connection_id: Some(req.principal.id.clone()),
                    session,
                    generation,
                    state,
                    before: optional_positive(args, "before")?
                        .map(|v| i64::try_from(v).map_err(|_| "invalid_before"))
                        .transpose()?,
                    limit: optional_positive(args, "limit")?
                        .unwrap_or(100)
                        .try_into()
                        .map_err(|_| "invalid_limit")?,
                    max_bytes: optional_positive(args, "max_bytes")?
                        .unwrap_or(agent_mcp::MAX_RUN_PAGE_BYTES as u64)
                        .try_into()
                        .map_err(|_| "invalid_max_bytes")?,
                    ..RunQuery::default()
                };
                query.validate().map_err(|_| "invalid_run_query")?;
                (Stage::List, Job::ListRuns { query })
            }
            _ => return Err("unknown_tool".into()),
        };
        let mut retained_target = target.cloned();
        if let Some(target) = &mut retained_target {
            target.screen = None;
            target.agent_line = None;
            target.workspace_name.clear();
            target.title.clear();
        }
        let retained = Request {
            epoch: req.epoch,
            principal: req.principal.clone(),
            access_key: req.access_key.clone(),
            input_scope: req.input_scope,
            deadline: req.deadline,
            tool: req.tool.clone(),
            args: json!({}),
            reply: req.reply.clone(),
        };
        let pending = Pending {
            fence: Some(Fence {
                request: retained,
                target: retained_target,
                auth: Arc::clone(&server.auth),
            }),
            stage,
            queued: None,
        };
        self.enqueue_management(pending, job, false)?;
        Ok(None)
    }
    fn enqueue_management(
        &mut self,
        mut pending: Pending,
        job: Job,
        retry_local: bool,
    ) -> Result<(), String> {
        if self.management.pending.len() >= MAX_PENDING
            || self
                .management
                .pending
                .values()
                .map(Pending::bytes)
                .sum::<usize>()
                .saturating_add(pending.bytes())
                > MAX_PENDING_BYTES
        {
            return Err("busy_no_effect".into());
        }
        if pending.mutation_key().is_some_and(|(actor, id)| {
            self.management
                .pending
                .values()
                .any(|other| other.mutation_key() == Some((actor, id)))
        }) {
            return Err("run_update_busy_no_effect".into());
        }
        let token = self.operation_token();
        if let Err(job) = self.history.request(token, job) {
            if !retry_local {
                return Err("busy_no_effect".into());
            }
            pending.queued = Some(*job);
            if self
                .management
                .pending
                .values()
                .map(Pending::bytes)
                .sum::<usize>()
                .saturating_add(pending.bytes())
                > MAX_PENDING_BYTES
            {
                return Err("busy_no_effect".into());
            }
        }
        self.management.pending.insert(token, pending);
        Ok(())
    }
    fn management_fence_current(&self, fence: &Fence) -> bool {
        self.server.as_ref().is_some_and(|server| {
            Arc::ptr_eq(&server.auth, &fence.auth) && fence.request.live(&server.auth)
        }) && fence
            .target
            .as_ref()
            .is_none_or(|target| self.management_request_current(&fence.request, target))
    }
    fn management_loaded_run(&self, pending: &Pending, run: &CloudRun) -> Result<(), String> {
        run.validate().map_err(|_| "invalid_stored_run")?;
        if let Some(fence) = &pending.fence {
            let target = fence.target.as_ref().ok_or("invalid_session_id")?;
            if run.connection_id != fence.request.principal.id
                || run.workspace != target.workspace
                || run.session != target.id
                || run.generation != target.generation
            {
                return Err("run_not_owned_or_session_changed".into());
            }
        }
        Ok(())
    }
    fn management_reply(&self, pending: &Pending, result: Result<Value, String>) {
        if let Some(fence) = &pending.fence {
            let _ = fence.request.reply.try_send(result);
        }
    }
    fn management_save(&mut self, mut pending: Pending, run: CloudRun, status: &'static str) {
        let job = Job::SaveRun { run: run.clone() };
        let retry = pending.fence.is_none();
        let reply = pending
            .fence
            .as_ref()
            .map(|fence| fence.request.reply.clone());
        pending.stage = Stage::Save { run, status };
        if let Err(error) = self.enqueue_management(pending, job, retry) {
            if let Some(reply) = reply {
                let _ = reply.try_send(Err(error.clone()));
            }
            self.error = Some(error);
        }
    }
    pub(super) fn complete_management(&mut self, token: u64, result: ResultKind) {
        let Some(pending) = self.management.pending.remove(&token) else {
            return;
        };
        // The operator cache reflects durable truth independently of whether
        // the original caller still has permission to receive a response.
        if matches!(&result, ResultKind::SaveRun(Ok(())))
            && let Stage::Save { run, .. } = &pending.stage
            && run.validate().is_ok()
        {
            self.retire_run_lease(run);
            self.install_cloud_run(run.clone());
        }
        if pending
            .fence
            .as_ref()
            .is_some_and(|fence| !self.management_fence_current(fence))
        {
            let code = if matches!(&pending.stage, Stage::Save { .. }) {
                "run_save_unknown_query_before_retry"
            } else {
                "session_or_permission_changed_no_effect"
            };
            self.management_reply(&pending, Err(code.into()));
            return;
        }
        match result {
            ResultKind::GetRun(result) => {
                let run = match result {
                    Ok(run) => run.map(|run| *run),
                    Err(()) => {
                        self.management_reply(&pending, Err("history_read_failed".into()));
                        return;
                    }
                };
                if let Some(run) = &run
                    && let Err(error) = self.management_loaded_run(&pending, run)
                {
                    self.management_reply(&pending, Err(error));
                    return;
                }
                match &pending.stage {
                    Stage::Start(proposed) => {
                        if let Some(run) = run {
                            if run.title != proposed.title
                                || run.instruction != proposed.instruction
                            {
                                self.management_reply(&pending, Err("run_id_conflict".into()));
                                return;
                            }
                            self.install_cloud_run(run.clone());
                            self.management_reply(
                                &pending,
                                Ok(json!({"status":"existing","run":self.scrub_cloud_run(run)})),
                            );
                        } else {
                            let proposed = proposed.clone();
                            self.management_save(pending, proposed, "registered");
                        }
                    }
                    Stage::Update { patch, .. } => {
                        let Some(mut run) = run else {
                            self.management_reply(&pending, Err("run_not_found".into()));
                            return;
                        };
                        let report = match self.management_update(patch, Some(&run)) {
                            Ok(report) => report,
                            Err(error) => {
                                self.management_reply(&pending, Err(error));
                                return;
                            }
                        };
                        if run
                            .apply_report(&run.actor(), report, now() as i64)
                            .is_err()
                        {
                            self.management_reply(
                                &pending,
                                Err("invalid_run_transition_or_report".into()),
                            );
                            return;
                        }
                        self.management_save(pending, run, "reported");
                    }
                    Stage::Get => {
                        if let Some(run) = &run {
                            self.install_cloud_run(run.clone());
                        }
                        self.management_reply(&pending, Ok(json!({"run":run.map(|run| self.scrub_cloud_run(run)),"source":"agent_reported","retry":false})));
                    }
                    Stage::Cancel { .. } => {
                        let Some(mut run) = run else {
                            self.error = Some("run_not_found".into());
                            return;
                        };
                        if run.request_cancel(now() as i64).is_err() {
                            self.error = Some("run_terminal_or_cancel_invalid".into());
                            return;
                        }
                        self.management_save(pending, run, "cancel_requested");
                    }
                    Stage::Stale { .. } => {
                        if let Some(mut run) = run {
                            let now = now() as i64;
                            if !run.state.is_terminal()
                                && run.state != RunState::Unknown
                                && now >= run.updated
                                && now.saturating_sub(run.heartbeat) >= HEARTBEAT_TIMEOUT
                            {
                                if run.restore(now).is_ok() {
                                    self.management_save(pending, run, "stale");
                                } else {
                                    self.history_ready = false;
                                    self.error = Some("run_recovery_failed".into());
                                    let revoked = self.control.take_control();
                                    self.revoke_leases(revoked);
                                }
                            } else {
                                self.install_cloud_run(run);
                            }
                        }
                    }
                    _ => self.management_reply(&pending, Err("history_reply_mismatch".into())),
                }
            }
            ResultKind::SaveRun(result) => {
                let Stage::Save { run, status } = &pending.stage else {
                    self.management_reply(&pending, Err("history_reply_mismatch".into()));
                    return;
                };
                match result {
                    Ok(()) => {
                        self.management_reply(&pending, Ok(json!({"status":status,"run":self.scrub_cloud_run(run.clone()),"source":"agent_reported","retry":false})));
                    }
                    Err(()) => {
                        self.management_reply(
                            &pending,
                            Err("run_save_unknown_query_before_retry".into()),
                        );
                        self.error = Some("run_save_failed".into());
                    }
                }
            }
            ResultKind::ListRuns(result) => {
                if !matches!(pending.stage, Stage::List | Stage::Bootstrap) {
                    self.management_reply(&pending, Err("history_reply_mismatch".into()));
                    return;
                }
                match result {
                    Ok(mut page) => {
                        if let Some(fence) = &pending.fence {
                            page.runs.retain(|run| {
                                run.connection_id == fence.request.principal.id
                                    && self
                                        .targets
                                        .iter()
                                        .find(|target| {
                                            target.id == run.session
                                                && target.generation == run.generation
                                                && target.workspace == run.workspace
                                        })
                                        .is_some_and(|target| {
                                            self.management_request_current(&fence.request, target)
                                        })
                            });
                        }
                        for run in &page.runs {
                            self.install_cloud_run(run.clone());
                        }
                        for run in &mut page.runs {
                            *run = self.scrub_cloud_run(run.clone());
                        }
                        self.management_reply(&pending, Ok(json!({"runs":page.runs,"next_cursor":page.next_cursor,"truncated":page.truncated,"source":"agent_reported"})));
                    }
                    Err(()) => {
                        self.management_reply(&pending, Err("history_read_failed".into()));
                        self.error = Some("run_history_read_failed".into());
                    }
                }
            }
            ResultKind::LookupTarget(result) | ResultKind::Lookup(result) => {
                let Stage::Lookup { id } = &pending.stage else {
                    self.management_reply(&pending, Err("history_reply_mismatch".into()));
                    return;
                };
                match result {
                    Ok(receipt) => {
                        let receipt = receipt.map(|value| self.scrub_management_value(value));
                        self.management_reply(&pending, Ok(json!({"operation_id":id,"receipt":receipt,"status":if receipt.is_some() {"found"} else {"unknown"},"retry":false})));
                    }
                    Err(()) => self.management_reply(&pending, Err("history_read_failed".into())),
                }
            }
            ResultKind::Query(result) => {
                let _ = result;
                self.management_reply(&pending, Err("history_reply_mismatch".into()));
            }
            _ => self.management_reply(&pending, Err("history_reply_mismatch".into())),
        }
    }
    pub(super) fn poll_management(&mut self) {
        if !self.history_ready {
            return;
        }
        let queued: Vec<_> = self
            .management
            .pending
            .iter()
            .filter_map(|(token, pending)| pending.queued.as_ref().map(|_| *token))
            .collect();
        for token in queued {
            let job = self
                .management
                .pending
                .get_mut(&token)
                .and_then(|pending| pending.queued.take())
                .unwrap();
            if let Err(job) = self.history.request(token, job) {
                self.management.pending.get_mut(&token).unwrap().queued = Some(*job);
            }
        }
        if self.history_ready && !self.management.bootstrap_started {
            let pending = Pending {
                fence: None,
                stage: Stage::Bootstrap,
                queued: None,
            };
            if self
                .enqueue_management(
                    pending,
                    Job::ListRuns {
                        query: RunQuery::default(),
                    },
                    false,
                )
                .is_ok()
            {
                self.management.bootstrap_started = true;
            }
        }
        let stale: Vec<_> = self
            .management
            .runs
            .iter()
            .filter(|run| {
                !run.state.is_terminal()
                    && run.state != RunState::Unknown
                    && (now() as i64).saturating_sub(run.heartbeat) >= HEARTBEAT_TIMEOUT
            })
            .map(|run| (run.connection_id.clone(), run.id.clone()))
            .take(MAX_PENDING)
            .collect();
        for (connection, id) in stale {
            let pending = Pending {
                fence: None,
                stage: Stage::Stale {
                    connection: connection.clone(),
                    id: id.clone(),
                },
                queued: None,
            };
            let _ = self.enqueue_management(
                pending,
                Job::GetRun {
                    id,
                    connection_id: connection,
                },
                false,
            );
        }
    }
    pub(crate) fn management_poll_after(&self) -> Option<Duration> {
        if !self.history_ready {
            return None;
        }
        if self
            .management
            .pending
            .values()
            .any(|pending| pending.queued.is_some())
        {
            return Some(Duration::from_millis(250));
        }
        self.management
            .runs
            .iter()
            .filter(|run| !run.state.is_terminal() && run.state != RunState::Unknown)
            .map(|run| {
                Duration::from_secs(
                    run.heartbeat
                        .saturating_add(HEARTBEAT_TIMEOUT)
                        .saturating_sub(now() as i64)
                        .max(1) as u64,
                )
            })
            .min()
    }
    pub fn cloud_runs(&self) -> &[CloudRun] {
        &self.management.runs
    }
    pub fn request_run_cancel(&mut self, connection: &str, id: &str) -> Result<(), String> {
        let run = self
            .management
            .runs
            .iter()
            .find(|run| run.connection_id == connection && run.id == id)
            .ok_or("run_not_found")?;
        if run.state.is_terminal() {
            return Err("run_terminal".into());
        }
        if !self.history_ready {
            return Err("history_unavailable_no_effect".into());
        }
        self.enqueue_management(
            Pending {
                fence: None,
                stage: Stage::Cancel {
                    connection: connection.into(),
                    id: id.into(),
                },
                queued: None,
            },
            Job::GetRun {
                id: id.into(),
                connection_id: connection.into(),
            },
            false,
        )
    }
    pub(super) fn management_run_owned(&self, connection: &str, id: &str, target: &Target) -> bool {
        self.management.runs.iter().any(|run| {
            run.connection_id == connection
                && run.id == id
                && run.session == target.id
                && run.generation == target.generation
                && run.workspace == target.workspace
        })
    }
    fn install_cloud_run(&mut self, run: CloudRun) {
        let run = self.scrub_cloud_run(run);
        if run.validate().is_err() {
            return;
        }
        let retained_key = (run.connection_id.clone(), run.id.clone());
        if let Some(index) = self
            .management
            .runs
            .iter()
            .position(|old| old.connection_id == run.connection_id && old.id == run.id)
        {
            if self.management.runs[index].revision > run.revision {
                return;
            }
            self.retire_run_lease(&run);
            self.management.runs[index] = run;
        } else {
            self.retire_run_lease(&run);
            self.management.runs.push(run);
        }
        self.management.runs.sort_by(|a, b| {
            b.updated
                .cmp(&a.updated)
                .then_with(|| a.connection_id.cmp(&b.connection_id))
                .then_with(|| a.id.cmp(&b.id))
        });
        while self.management.runs.len() > MAX_CACHED_RUNS
            || self
                .management
                .runs
                .iter()
                .map(|run| serde_json::to_vec(run).map_or(agent_mcp::MAX_RUN_BYTES, |v| v.len()))
                .sum::<usize>()
                > MAX_CACHE_BYTES
        {
            // An exact read must prime ownership even for an older run. Keep
            // that record and discard another oldest cache entry instead.
            let index = self
                .management
                .runs
                .iter()
                .rposition(|run| {
                    (&run.connection_id, &run.id) != (&retained_key.0, &retained_key.1)
                })
                .expect("one bounded run always fits the cache");
            self.management.runs.remove(index);
        }
    }
    fn scrub_cloud_run(&self, mut run: CloudRun) -> CloudRun {
        for (text, limit) in [
            (&mut run.connection_name, 128),
            (&mut run.title, 256),
            (&mut run.instruction, 8192),
            (&mut run.message, 4096),
        ] {
            *text = self.redact(text);
            truncate_utf8(text, limit);
        }
        if let Some(result) = &mut run.result {
            *result = self.redact(result);
            truncate_utf8(result, agent_mcp::MAX_ANSWER);
        }
        run.artifacts.retain_mut(|artifact| {
            artifact.label = self.redact(&artifact.label);
            // A redacted URL would name a different clickable destination.
            // Preserve the exact safe URI or omit the reference entirely.
            self.redact(&artifact.uri) == artifact.uri && artifact.validate().is_ok()
        });
        run
    }
    fn scrub_management_value(&self, value: Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.redact(&text)),
            Value::Array(values) => Value::Array(
                values
                    .into_iter()
                    .map(|v| self.scrub_management_value(v))
                    .collect(),
            ),
            Value::Object(values) => Value::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (self.redact(&key), self.scrub_management_value(value)))
                    .collect(),
            ),
            other => other,
        }
    }
    fn management_text(&self, text: &str, max: usize) -> Result<String, String> {
        if text.len() > max || text.contains('\0') {
            return Err("run_text_too_large_or_invalid".into());
        }
        let clean: String = text
            .chars()
            .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
            .collect();
        let clean = self.redact(&clean);
        if clean.len() > max {
            return Err("run_text_too_large_or_invalid".into());
        }
        Ok(clean)
    }
    fn management_update(
        &self,
        patch: &Value,
        existing: Option<&CloudRun>,
    ) -> Result<RunUpdate, String> {
        let state: RunState =
            serde_json::from_value(patch.get("state").ok_or("invalid_run_state")?.clone())
                .map_err(|_| "invalid_run_state")?;
        if state == RunState::Unknown {
            return Err("invalid_run_state".into());
        }
        let progress = match patch.get("progress") {
            Some(value) => Some(
                u8::try_from(
                    value
                        .as_u64()
                        .filter(|v| *v <= 100)
                        .ok_or("invalid_run_progress")?,
                )
                .map_err(|_| "invalid_run_progress")?,
            ),
            None => existing.and_then(|run| run.progress),
        };
        let message = self.management_text(
            patch
                .get("message")
                .map(|v| v.as_str().ok_or("invalid_run_message"))
                .transpose()?
                .unwrap_or(existing.map_or("", |run| run.message.as_str())),
            4096,
        )?;
        let result = match patch.get("result") {
            Some(value) => Some(self.management_text(
                value.as_str().ok_or("invalid_run_result")?,
                agent_mcp::MAX_ANSWER,
            )?),
            None => existing
                .and_then(|run| run.result.as_deref())
                .map(|value| self.management_text(value, agent_mcp::MAX_ANSWER))
                .transpose()?,
        };
        let mut artifacts = if let Some(value) = patch.get("artifacts") {
            let values = value
                .as_array()
                .filter(|values| values.len() <= 16)
                .ok_or("invalid_run_artifacts")?;
            if values.iter().any(|value| {
                value.as_object().is_none_or(|value| {
                    value
                        .keys()
                        .any(|key| !matches!(key.as_str(), "label" | "uri"))
                })
            }) {
                return Err("invalid_run_artifacts".into());
            }
            serde_json::from_value::<Vec<agent_mcp::ArtifactReference>>(value.clone())
                .map_err(|_| "invalid_run_artifacts")?
        } else {
            existing.map_or_else(Vec::new, |run| run.artifacts.clone())
        };
        for artifact in &mut artifacts {
            artifact.label = self.management_text(&artifact.label, 128)?;
            if self.management_text(&artifact.uri, 2048)? != artifact.uri {
                return Err("invalid_run_artifacts".into());
            }
            artifact.validate().map_err(|_| "invalid_run_artifacts")?;
        }
        let report = RunUpdate {
            state,
            progress,
            message,
            result,
            artifacts,
        };
        if serde_json::to_vec(&report)
            .map_err(|_| "invalid_run_report")?
            .len()
            > agent_mcp::MAX_RUN_BYTES
        {
            return Err("run_report_too_large".into());
        }
        Ok(report)
    }
}

fn optional_positive(args: &Value, key: &str) -> Result<Option<u64>, String> {
    args.get(key)
        .map(|value| {
            value
                .as_u64()
                .filter(|v| *v > 0)
                .ok_or_else(|| format!("invalid_{key}"))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_mcp::{Server, now};
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    fn setup() -> (CloudAgent, Target) {
        let mut bridge = CloudAgent::memory();
        bridge.server = Some(Server::start(0, "", || {}).unwrap());
        let target = Target::fixture("session", "generation-1");
        bridge.set_targets(vec![target.clone()]);
        bridge.share(&target, true);
        (bridge, target)
    }
    fn request(
        bridge: &CloudAgent,
        tool: &str,
        args: Value,
    ) -> (Request, mpsc::Receiver<Result<Value, String>>) {
        let auth = &bridge.server.as_ref().unwrap().auth;
        let header = format!("Bearer {}", auth.token_for_user().as_str());
        let (reply, rx) = mpsc::sync_channel(1);
        (
            Request {
                epoch: auth.authenticate(&header, now()).unwrap(),
                principal: auth.authenticate_principal(&header, now()).unwrap(),
                access_key: None,
                input_scope: true,
                deadline: Instant::now() + Duration::from_secs(3),
                tool: tool.into(),
                args,
                reply,
            },
            rx,
        )
    }
    fn call(
        bridge: &mut CloudAgent,
        target: Option<&Target>,
        tool: &str,
        args: Value,
    ) -> Result<Value, String> {
        let (req, rx) = request(bridge, tool, args);
        assert!(bridge.prepare_management(&req, target)?.is_none());
        bridge.drain_history(|_, _| panic!("management tools must not emit terminal effects"));
        rx.recv_timeout(Duration::from_secs(1)).unwrap()
    }
    fn register(bridge: &mut CloudAgent, target: &Target) {
        call(bridge,Some(target),"start_run",json!({"session_id":target.id,"generation":target.generation,"run_id":"run","title":"Analyze"})).unwrap();
    }
    #[test]
    fn start_run_ack_and_cache_wait_for_durable_completion() {
        let (mut bridge, target) = setup();
        let (req, rx) = request(
            &bridge,
            "start_run",
            json!({"session_id":target.id,"generation":target.generation,"run_id":"run","title":"Analyze","instruction":"Find the error"}),
        );
        let result = bridge.prepare_management(&req, Some(&target)).unwrap();
        assert!(
            result.is_none(),
            "queue admission must not optimistically acknowledge durable run registration"
        );
        assert!(rx.try_recv().is_err());
        assert!(bridge.cloud_runs().is_empty());
        bridge.drain_history(|_, _| panic!("run registration must not emit terminal input"));
        let response = rx.recv_timeout(Duration::from_secs(1)).unwrap().unwrap();
        assert_eq!(response["run"]["state"], "queued");
        assert_eq!(bridge.cloud_runs().len(), 1);
    }

    #[test]
    fn start_run_lookup_completion_rechecks_revocation_before_saving() {
        let (mut bridge, target) = setup();
        let (req, rx) = request(
            &bridge,
            "start_run",
            json!({"session_id":target.id,"generation":target.generation,"run_id":"revoked","title":"Analyze"}),
        );
        assert!(
            bridge
                .prepare_management(&req, Some(&target))
                .unwrap()
                .is_none()
        );
        bridge.server.as_ref().unwrap().auth.revoke();
        bridge.drain_history(|_, _| panic!("revoked run registration emitted input"));
        assert!(rx.recv_timeout(Duration::from_secs(1)).unwrap().is_err());
        assert!(bridge.cloud_runs().is_empty());
    }

    #[test]
    fn management_pending_limit_rejects_without_unbounded_admission() {
        let (mut bridge, target) = setup();
        let mut replies = vec![];
        for index in 0..16 {
            let (req, rx) = request(
                &bridge,
                "start_run",
                json!({"session_id":target.id,"generation":target.generation,"run_id":format!("run-{index}"),"title":"Analyze"}),
            );
            assert!(
                bridge
                    .prepare_management(&req, Some(&target))
                    .unwrap()
                    .is_none()
            );
            replies.push(rx);
        }
        let (req, _rx) = request(
            &bridge,
            "start_run",
            json!({"session_id":target.id,"generation":target.generation,"run_id":"over-limit","title":"Analyze"}),
        );
        assert_eq!(
            bridge.prepare_management(&req, Some(&target)).unwrap_err(),
            "busy_no_effect"
        );
        bridge.drain_history(|_, _| panic!("management admission emitted input"));
        assert_eq!(bridge.cloud_runs().len(), 16);
        for reply in replies {
            assert!(reply.recv_timeout(Duration::from_secs(1)).unwrap().is_ok());
        }
    }

    #[test]
    fn operator_cancel_waits_for_durability_and_same_actor_ack() {
        let (mut bridge, target) = setup();
        register(&mut bridge, &target);
        bridge
            .request_run_cancel(agent_mcp::LEGACY_CONNECTION_ID, "run")
            .unwrap();
        assert!(bridge.cloud_runs()[0].cancel_requested_at.is_none());
        bridge.drain_history(|_, _| panic!("cancellation intent is not terminal Ctrl+C"));
        assert!(bridge.cloud_runs()[0].cancel_requested_at.is_some());
        assert_eq!(bridge.cloud_runs()[0].state, RunState::Queued);
        assert!(bridge.cloud_runs()[0].cancel_acknowledged_at.is_none());
        call(&mut bridge,Some(&target),"update_run",json!({"session_id":target.id,"generation":target.generation,"run_id":"run","state":"cancelled","message":"stopped"})).unwrap();
        assert_eq!(bridge.cloud_runs()[0].state, RunState::Cancelled);
        assert!(bridge.cloud_runs()[0].cancel_acknowledged_at.is_some());
        assert!(call(&mut bridge,Some(&target),"update_run",json!({"session_id":target.id,"generation":target.generation,"run_id":"run","state":"completed"})).is_err());
    }
    #[test]
    fn same_actor_cannot_get_or_update_run_using_another_pane() {
        let (mut bridge, target) = setup();
        register(&mut bridge, &target);
        let other = Target::fixture("other-pane", "generation-1");
        bridge.set_targets(vec![target.clone(), other.clone()]);
        bridge.share(&other, true);
        for tool in ["get_run", "update_run"] {
            let mut args =
                json!({"session_id":other.id,"generation":other.generation,"run_id":"run"});
            if tool == "update_run" {
                args["state"] = json!("running");
            }
            assert!(call(&mut bridge, Some(&other), tool, args).is_err());
        }
        assert_eq!(bridge.cloud_runs()[0].session, target.id);
        assert_eq!(bridge.cloud_runs()[0].state, RunState::Queued);
    }
    #[test]
    fn persisted_run_beyond_cache_page_remains_exactly_accessible() {
        let (mut bridge, target) = setup();
        for index in 0..101 {
            let run = CloudRun::new(
                &format!("run-{index}"),
                agent_mcp::LEGACY_CONNECTION_ID,
                "Legacy",
                &target.workspace,
                &target.id,
                &target.generation,
                "title",
                "instruction",
                now() as i64 - if index == 0 { 120 } else { 0 },
            )
            .unwrap();
            let token = bridge.operation_token();
            assert!(bridge.history.request(token, Job::SaveRun { run }).is_ok());
            assert!(matches!(
                bridge.history.wait().unwrap().result,
                ResultKind::SaveRun(Ok(()))
            ));
        }
        bridge.poll_management();
        bridge.drain_history(|_, _| panic!("cache bootstrap must have no terminal effects"));
        assert_eq!(bridge.cloud_runs().len(), 100);
        assert!(!bridge.cloud_runs().iter().any(|run| run.id == "run-0"));
        let result = call(
            &mut bridge,
            Some(&target),
            "get_run",
            json!({"session_id":target.id,"generation":target.generation,"run_id":"run-0"}),
        )
        .unwrap();
        assert_eq!(result["run"]["id"], "run-0");
        assert!(bridge.management_run_owned(agent_mcp::LEGACY_CONNECTION_ID, "run-0", &target));
        assert_eq!(bridge.cloud_runs().len(), 100);
    }
    #[test]
    fn run_fields_are_redacted_before_sqlite_and_publication() {
        let (mut bridge, target) = setup();
        let sensitive = "fixture-secret-access-token-123456789";
        let secret = secret::SecretString::new(sensitive.into());
        let _lease = bridge
            .redaction
            .acquire_execution_lease(&[&secret])
            .unwrap();
        let registered = call(&mut bridge,Some(&target),"start_run",json!({"session_id":target.id,"generation":target.generation,"run_id":"run","title":sensitive,"instruction":sensitive})).unwrap();
        assert!(!registered.to_string().contains(sensitive));
        let updated = call(&mut bridge,Some(&target),"update_run",json!({"session_id":target.id,"generation":target.generation,"run_id":"run","state":"running","message":sensitive,"result":sensitive,"artifacts":[{"label":sensitive,"uri":"https://example.test/result"}]})).unwrap();
        assert!(!updated.to_string().contains(sensitive));
        let token = bridge.operation_token();
        assert!(
            bridge
                .history
                .request(
                    token,
                    Job::GetRun {
                        id: "run".into(),
                        connection_id: agent_mcp::LEGACY_CONNECTION_ID.into()
                    }
                )
                .is_ok()
        );
        let result = bridge.history.wait().unwrap();
        let ResultKind::GetRun(Ok(Some(run))) = &result.result else {
            panic!("persisted run");
        };
        assert!(!serde_json::to_string(run).unwrap().contains(sensitive));
    }
    #[test]
    fn artifact_update_rejects_a_credential_uri_before_durable_mutation() {
        let (mut bridge, target) = setup();
        register(&mut bridge, &target);
        let before = bridge.cloud_runs()[0].clone();
        let sensitive = "fixture-artifact-access-token-987654321";
        let secret = secret::SecretString::new(sensitive.into());
        let _lease = bridge
            .redaction
            .acquire_execution_lease(&[&secret])
            .unwrap();
        let result = call(
            &mut bridge,
            Some(&target),
            "update_run",
            json!({"session_id":target.id,"generation":target.generation,"run_id":"run","state":"running","artifacts":[{"label":"Report","uri":format!("https://example.test/report/{sensitive}")}]}),
        );
        assert_eq!(result.unwrap_err(), "invalid_run_artifacts");
        assert_eq!(bridge.cloud_runs()[0].revision, before.revision);
        let token = bridge.operation_token();
        assert!(
            bridge
                .history
                .request(
                    token,
                    Job::GetRun {
                        id: "run".into(),
                        connection_id: agent_mcp::LEGACY_CONNECTION_ID.into(),
                    },
                )
                .is_ok()
        );
        let completion = bridge.history.wait().unwrap();
        let ResultKind::GetRun(Ok(Some(run))) = &completion.result else {
            panic!("persisted run");
        };
        assert_eq!(run.state, before.state);
        assert_eq!(run.revision, before.revision);
        assert!(run.artifacts.is_empty());
    }
    #[test]
    fn persisted_credential_artifact_is_removed_from_reply_and_operator_cache() {
        let (mut bridge, target) = setup();
        let sensitive = "fixture-persisted-artifact-token-123456789";
        let secret = secret::SecretString::new(sensitive.into());
        let _lease = bridge
            .redaction
            .acquire_execution_lease(&[&secret])
            .unwrap();
        let mut run = CloudRun::new(
            "run",
            agent_mcp::LEGACY_CONNECTION_ID,
            "Legacy",
            &target.workspace,
            &target.id,
            &target.generation,
            "title",
            "instruction",
            now() as i64,
        )
        .unwrap();
        run.artifacts = vec![
            agent_mcp::ArtifactReference {
                label: "Unsafe report".into(),
                uri: format!("https://example.test/report/{sensitive}"),
            },
            agent_mcp::ArtifactReference {
                label: sensitive.into(),
                uri: "https://example.test/safe-report".into(),
            },
        ];
        let token = bridge.operation_token();
        assert!(bridge.history.request(token, Job::SaveRun { run }).is_ok());
        assert!(matches!(
            bridge.history.wait().unwrap().result,
            ResultKind::SaveRun(Ok(()))
        ));
        let response = call(
            &mut bridge,
            Some(&target),
            "get_run",
            json!({"session_id":target.id,"generation":target.generation,"run_id":"run"}),
        )
        .unwrap();
        let artifacts = response["run"]["artifacts"].as_array().unwrap();
        assert_eq!(
            artifacts.len(),
            1,
            "an altered URI must not become a new clickable destination"
        );
        assert_eq!(artifacts[0]["uri"], "https://example.test/safe-report");
        assert!(!response.to_string().contains(sensitive));
        assert_eq!(bridge.cloud_runs()[0].artifacts.len(), 1);
        assert_eq!(
            bridge.cloud_runs()[0].artifacts[0].uri,
            "https://example.test/safe-report"
        );
    }
    #[test]
    fn heartbeat_loss_persists_unknown_and_cannot_be_reported_completed_directly() {
        let (mut bridge, target) = setup();
        let run = CloudRun::new(
            "old",
            agent_mcp::LEGACY_CONNECTION_ID,
            "Legacy",
            &target.workspace,
            &target.id,
            &target.generation,
            "title",
            "instruction",
            now() as i64 - 120,
        )
        .unwrap();
        let token = bridge.operation_token();
        assert!(bridge.history.request(token, Job::SaveRun { run }).is_ok());
        assert!(matches!(
            bridge.history.wait().unwrap().result,
            ResultKind::SaveRun(Ok(()))
        ));
        call(
            &mut bridge,
            Some(&target),
            "get_run",
            json!({"session_id":target.id,"generation":target.generation,"run_id":"old"}),
        )
        .unwrap();
        bridge.poll_management();
        bridge.drain_history(|_, _| panic!("stale persistence must not emit effects"));
        assert_eq!(bridge.cloud_runs()[0].state, RunState::Unknown);
        assert!(call(&mut bridge,Some(&target),"update_run",json!({"session_id":target.id,"generation":target.generation,"run_id":"old","state":"completed"})).is_err());
    }
    #[test]
    fn failed_local_recovery_disables_history_and_reports_bounded_error() {
        let (mut bridge, target) = setup();
        let mut run = CloudRun::new(
            "old",
            agent_mcp::LEGACY_CONNECTION_ID,
            "Legacy",
            &target.workspace,
            &target.id,
            &target.generation,
            "title",
            "instruction",
            now() as i64 - 120,
        )
        .unwrap();
        run.state = RunState::Running;
        run.revision = u64::MAX;
        bridge.install_cloud_run(run.clone());
        let token = bridge.operation_token();
        bridge.management.pending.insert(
            token,
            Pending {
                fence: None,
                stage: Stage::Stale {
                    connection: run.connection_id.clone(),
                    id: run.id.clone(),
                },
                queued: None,
            },
        );
        bridge.complete_management(token, ResultKind::GetRun(Ok(Some(Box::new(run)))));
        assert!(!bridge.history_ready, "failed recovery must fail closed");
        assert_eq!(bridge.error.as_deref(), Some("run_recovery_failed"));
        assert_eq!(bridge.cloud_runs()[0].revision, u64::MAX);
        assert_eq!(bridge.cloud_runs()[0].state, RunState::Running);
    }
    #[test]
    fn durable_terminal_save_retires_lease_even_when_response_deadline_expires() {
        let (mut bridge, target) = setup();
        register(&mut bridge, &target);
        call(&mut bridge,Some(&target),"update_run",json!({"session_id":target.id,"generation":target.generation,"run_id":"run","state":"running"})).unwrap();
        bridge.allow_input(&target.id, true);
        let (req, rx) = request(
            &bridge,
            "update_run",
            json!({"session_id":target.id,"generation":target.generation,"run_id":"run","state":"completed"}),
        );
        let lease = bridge
            .acquire_lease(&req, &target, Some("run"), 120)
            .unwrap();
        bridge.prepare_management(&req, Some(&target)).unwrap();
        let lookup = bridge.history.wait().unwrap();
        bridge.complete_history(
            lookup,
            &mut |_, _| panic!("run completion must not emit terminal effects"),
            &mut vec![],
        );
        let pending = bridge
            .management
            .pending
            .values_mut()
            .find(|pending| matches!(pending.stage, Stage::Save { .. }))
            .unwrap();
        pending.fence.as_mut().unwrap().request.deadline = Instant::now() - Duration::from_secs(1);
        bridge.drain_history(|_, _| panic!("expired response emitted input"));
        assert!(rx.recv_timeout(Duration::from_secs(1)).unwrap().is_err());
        assert!(
            bridge.control.lease(&target.id, now()).is_none(),
            "durable terminal evidence must retire its input lease even without response publication"
        );
        assert!(!bridge.lease_permits.contains_key(&lease.epoch));
        let (next, next_rx) = request(
            &bridge,
            "acquire_control",
            json!({"session_id":target.id,"generation":target.generation,"run_id":"run"}),
        );
        bridge.handle_and_drain(next, |_, _| panic!("control acquisition emitted input"));
        assert_eq!(
            next_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap_err(),
            "run_not_loaded_or_not_owned",
            "a committed terminal run must not acquire a fresh input lease"
        );
    }
    #[test]
    fn committed_run_save_with_lost_permission_reports_unknown_before_retry() {
        let (mut bridge, target) = setup();
        register(&mut bridge, &target);
        call(&mut bridge,Some(&target),"update_run",json!({"session_id":target.id,"generation":target.generation,"run_id":"run","state":"running"})).unwrap();
        let (req, rx) = request(
            &bridge,
            "update_run",
            json!({"session_id":target.id,"generation":target.generation,"run_id":"run","state":"completed"}),
        );
        bridge.prepare_management(&req, Some(&target)).unwrap();
        let lookup = bridge.history.wait().unwrap();
        bridge.complete_history(
            lookup,
            &mut |_, _| panic!("run update emitted input"),
            &mut vec![],
        );
        let saved = bridge.history.wait().unwrap();
        assert!(matches!(&saved.result, ResultKind::SaveRun(Ok(()))));
        assert!(req.live(&bridge.server.as_ref().unwrap().auth));
        bridge.share(&target, false);
        bridge.complete_history(
            saved,
            &mut |_, _| panic!("revoked response emitted input"),
            &mut vec![],
        );
        let token = bridge.operation_token();
        assert!(
            bridge
                .history
                .request(
                    token,
                    Job::GetRun {
                        id: "run".into(),
                        connection_id: agent_mcp::LEGACY_CONNECTION_ID.into(),
                    }
                )
                .is_ok()
        );
        let stored = bridge.history.wait().unwrap();
        let ResultKind::GetRun(Ok(Some(run))) = &stored.result else {
            panic!("committed run must remain durable");
        };
        assert_eq!(run.state, RunState::Completed);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap_err(),
            "run_save_unknown_query_before_retry",
            "the response must not claim no effect after a durable commit"
        );
        assert_eq!(bridge.cloud_runs()[0].state, RunState::Completed);
    }

    #[test]
    fn pwa_preserves_a_run_when_one_artifact_has_legacy_raw_whitespace() {
        let (mut bridge, target) = setup();
        let mut run = agent_mcp::CloudRun::new(
            "safe-work",
            agent_mcp::LEGACY_CONNECTION_ID,
            "Reported work",
            &target.workspace,
            &target.id,
            &target.generation,
            "Retained result",
            "Inspect project",
            1,
        )
        .unwrap();
        run.state = agent_mcp::RunState::Completed;
        run.result = Some("Useful report".into());
        run.artifacts = vec![
            agent_mcp::ArtifactReference {
                label: "Old spaced URI".into(),
                uri: "https://example.com/my report".into(),
            },
            agent_mcp::ArtifactReference {
                label: "Valid link".into(),
                uri: "https://example.com/report".into(),
            },
        ];
        bridge.install_cloud_run(run);
        let snapshot = bridge.cloud_snapshot();
        assert_eq!(
            snapshot.runs.len(),
            1,
            "a bad artifact must not hide its entire work result"
        );
        assert_eq!(snapshot.runs[0].result.as_deref(), Some("Useful report"));
        assert_eq!(snapshot.runs[0].artifacts.len(), 1);
        assert_eq!(
            snapshot.runs[0].artifacts[0].uri,
            "https://example.com/report"
        );
        assert!(
            snapshot.truncated,
            "omitted legacy artifacts must remain explicit"
        );
    }
}
