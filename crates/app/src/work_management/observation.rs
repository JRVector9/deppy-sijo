//! Exact Codex turn observation. Provider completion is not task completion or verification.
//! Existing turns use the cloud completion tracker. Fresh dedicated executions bind their first
//! genuine native turn after both input receipts. Only a lazy worker opens the bounded rollout.

use crate::cloud_agent::completion::{
    self, Decision, LocalCompletionState, Tracker, WatchKey, WatchTarget,
};
use crate::lazy_worker::{LazyBoundedWorker, LazyWorkerSubmitError};
use deppy_core::WorkspaceId;
use deppy_core::work_management::{AttemptId, OperationId, RuntimeBinding};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use storage::NativeTurnProof;

pub(crate) const FINAL_TEXT_MAX: usize = 128 * 1024;
const WATCH_MAX: usize = 64;
const OPERATION_MAX: usize = 1024;
const START_TIMEOUT: i64 = 30_000_000;
const WATCH_TIMEOUT: i64 = 3_600_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObservationIdentity {
    pub workspace_id: WorkspaceId,
    pub operation_id: OperationId,
    pub attempt_id: Option<AttemptId>,
    pub binding: RuntimeBinding,
}

struct Watch {
    identity: ObservationIdentity,
    target: WatchTarget,
    key: WatchKey,
    sequence: u64,
    baseline_revision: i64,
    transcript_path: String,
    fresh: Option<FreshWatch>,
}

struct FreshWatch {
    created_at: i64,
    prompt_digest: String,
    accepted_at: Option<i64>,
    admitted: bool,
    bound: Option<NativeTurnProof>,
}

#[derive(Default)]
pub(crate) struct CodexTracker {
    tracker: Tracker,
    watches: HashMap<OperationId, Watch>,
    // Unknown delivery is never retried through a new watch in this controller lifetime.
    seen: HashSet<OperationId>,
}

pub(crate) struct ObservedTurn {
    pub identity: ObservationIdentity,
    /// Completed means this provider turn ended. It grants no result verification or user approval.
    pub state: LocalCompletionState,
    pub reason: &'static str,
    pub turn_id: Option<String>,
    pub sequence: Option<u64>,
    pub text: Option<String>,
    pub proof: Option<NativeTurnProof>,
}

pub(crate) struct ObservationPoll {
    pub state: LocalCompletionState,
    /// Native blocked flag only; never an approval ticket or permission to answer one.
    pub waiting: bool,
    pub terminal: Option<ObservedTurn>,
    pub job: Option<ReadCodexFinal>,
}

#[derive(Clone)]
pub(crate) struct ReadCodexFinal {
    pub identity: ObservationIdentity,
    pub target: WatchTarget,
    pub proof: NativeTurnProof,
    pub sequence: u64,
    // Only the tracker can create an admitted, exact-turn request.
    _seal: (),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObservationError {
    Unproven,
    Unavailable,
    Limit,
}

pub(crate) struct FinalResponseResult {
    pub job: ReadCodexFinal,
    pub text: Result<String, ObservationError>,
}

impl CodexTracker {
    /// Only for an exact, newly launched dedicated Codex execution. The host supplies the real
    /// SessionStart native UUID; absence of a previous turn is never replaced by a synthetic proof.
    pub fn begin_fresh(
        &mut self,
        identity: ObservationIdentity,
        target: WatchTarget,
        baseline_input_sequence: u64,
        prompt_digest: String,
        now_micros: i64,
    ) -> Result<(), &'static str> {
        self.validate_start(
            &identity,
            &target,
            baseline_input_sequence,
            &prompt_digest,
            now_micros,
        )?;
        let key = Self::key(&identity);
        self.seen.insert(identity.operation_id.clone());
        self.watches.insert(
            identity.operation_id.clone(),
            Watch {
                identity,
                target,
                key,
                sequence: baseline_input_sequence + 1,
                baseline_revision: 0,
                transcript_path: String::new(),
                fresh: Some(FreshWatch {
                    created_at: now_micros,
                    prompt_digest,
                    accepted_at: None,
                    admitted: false,
                    bound: None,
                }),
            },
        );
        Ok(())
    }

    fn key(identity: &ObservationIdentity) -> WatchKey {
        WatchKey {
            connection_id: "command-center".into(),
            operation_id: identity.operation_id.0.clone(),
            run_id: identity.attempt_id.as_ref().map(|id| id.0.clone()),
        }
    }

    fn validate_start(
        &self,
        identity: &ObservationIdentity,
        target: &WatchTarget,
        baseline_sequence: u64,
        digest: &str,
        now: i64,
    ) -> Result<(), &'static str> {
        let id = |value: &str, max| {
            !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
        };
        if identity.workspace_id.0 != target.workspace
            || identity.binding.session_id != target.session
            || identity.binding.validate().is_err()
            || !id(identity.operation_id.as_str(), 128)
            || identity
                .attempt_id
                .as_ref()
                .is_some_and(|attempt| !id(attempt.as_str(), 128))
            || !id(&target.workspace, 1024)
            || !id(&target.session_id, 256)
            || !id(&target.generation, 256)
            || !id(&target.runtime_token, 128)
            || uuid::Uuid::parse_str(&target.native_session_id).is_err()
            || target
                .execution
                .input_guard_for(runtime::AgentInputIntent::ExplicitPrompt)
                .provider
                != runtime::AgentPromptKind::Codex
            || baseline_sequence == u64::MAX
            || now <= 0
            || digest.len() != 64
            || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("observation_invalid_identity");
        }
        if self.seen.contains(&identity.operation_id) {
            return Err("observation_operation_already_tracked");
        }
        if self.seen.len() >= OPERATION_MAX || self.watches.len() >= WATCH_MAX {
            return Err("observation_capacity");
        }
        if self.watches.values().any(|watch| {
            watch.target.session_id == target.session_id
                || (watch.target.runtime == target.runtime
                    && watch.target.session == target.session)
        }) {
            return Err("observation_target_busy");
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn begin(
        &mut self,
        identity: ObservationIdentity,
        target: WatchTarget,
        baseline: &NativeTurnProof,
        baseline_input_sequence: u64,
        prompt_digest: String,
        now_micros: i64,
    ) -> Result<(), &'static str> {
        self.validate_start(
            &identity,
            &target,
            baseline_input_sequence,
            &prompt_digest,
            now_micros,
        )?;
        let key = Self::key(&identity);
        self.tracker.begin(
            key.clone(),
            target.clone(),
            baseline,
            baseline_input_sequence,
            identity.operation_id.0.clone(),
            prompt_digest,
            now_micros,
        )?;
        self.seen.insert(identity.operation_id.clone());
        self.watches.insert(
            identity.operation_id.clone(),
            Watch {
                identity,
                target,
                key,
                sequence: baseline_input_sequence + 1,
                baseline_revision: baseline.completion_revision,
                transcript_path: baseline.transcript_path.clone(),
                fresh: None,
            },
        );
        Ok(())
    }

    pub fn native_turn_keys(&self) -> Vec<String> {
        let mut keys: Vec<_> = self
            .watches
            .values()
            .map(|watch| format!("{}:{}", watch.target.workspace, watch.target.session.0))
            .collect();
        keys.sort();
        keys.dedup();
        keys
    }

    pub fn accepted_input(
        &mut self,
        runtime: u64,
        session: runtime::SessionId,
        sequence: u64,
        operation_id: Option<&str>,
        at_micros: i64,
    ) -> Vec<ObservedTurn> {
        let decisions =
            self.tracker
                .accepted_input(runtime, session, sequence, operation_id, at_micros);
        let mut terminal: Vec<_> = decisions
            .into_iter()
            .filter_map(|decision| self.terminal(decision, None, None))
            .collect();
        let mut intervened = Vec::new();
        for watch in self
            .watches
            .values_mut()
            .filter(|watch| watch.target.runtime == runtime && watch.target.session == session)
        {
            let Some(fresh) = watch.fresh.as_mut() else {
                continue;
            };
            if sequence > 0
                && (sequence < watch.sequence
                    || (sequence == watch.sequence && fresh.accepted_at.is_some()))
            {
                continue;
            }
            if sequence == watch.sequence
                && fresh.accepted_at.is_none()
                && operation_id == Some(watch.identity.operation_id.as_str())
                && at_micros >= fresh.created_at
            {
                fresh.accepted_at = Some(at_micros);
            } else {
                intervened.push(watch.identity.operation_id.clone());
            }
        }
        terminal.extend(intervened.into_iter().filter_map(|id| {
            self.finish_fresh(
                &id,
                LocalCompletionState::Unknown,
                "observation_input_intervened",
                None,
                None,
            )
        }));
        terminal
    }

    pub fn admit(
        &mut self,
        identity: &ObservationIdentity,
        result: Result<(), runtime::PtyInputRejectReason>,
    ) -> Option<ObservedTurn> {
        let watch = self.watches.get_mut(&identity.operation_id)?;
        if &watch.identity != identity {
            return None;
        }
        if let Some(fresh) = watch.fresh.as_mut() {
            let (state, reason) = match result {
                Ok(()) => {
                    fresh.admitted = true;
                    return None;
                }
                Err(runtime::PtyInputRejectReason::AdmissionUnknown) => (
                    LocalCompletionState::Unknown,
                    "observation_admission_unknown",
                ),
                Err(_) => (LocalCompletionState::Rejected, "observation_input_rejected"),
            };
            return self.finish_fresh(&identity.operation_id, state, reason, None, None);
        }
        let decision = self.tracker.admit(&watch.key, result)?;
        self.terminal(decision, None, None)
    }

    pub fn observe(
        &mut self,
        identity: &ObservationIdentity,
        target: &WatchTarget,
        proof: &NativeTurnProof,
        now_micros: i64,
    ) -> ObservationPoll {
        let absent = || ObservationPoll {
            state: LocalCompletionState::Unknown,
            waiting: false,
            terminal: None,
            job: None,
        };
        let Some(watch) = self.watches.get(&identity.operation_id) else {
            return absent();
        };
        if watch.fresh.is_some() {
            return self.observe_fresh(identity, target, proof, now_micros);
        }
        if &watch.identity != identity
            || &watch.target != target
            || watch.transcript_path != proof.transcript_path
        {
            let key = watch.key.clone();
            let terminal = self
                .tracker
                .cancel(
                    &key,
                    LocalCompletionState::Unknown,
                    "observation_identity_changed",
                )
                .and_then(|decision| self.terminal(decision, None, None));
            return ObservationPoll {
                terminal,
                ..absent()
            };
        }
        // File I/O is deliberately excluded from this call. A final candidate is not yet complete.
        let decisions = self.tracker.observe(target, proof, false, now_micros);
        if let Some(decision) = decisions.into_iter().next() {
            let terminal = self.terminal(decision, None, None);
            return ObservationPoll {
                terminal,
                ..absent()
            };
        }
        let watch = self
            .watches
            .get(&identity.operation_id)
            .expect("nonterminal watch retained");
        let state = self
            .tracker
            .state(&watch.key)
            .unwrap_or(LocalCompletionState::Unknown);
        let working = state == LocalCompletionState::Working;
        let job = (working
            && !proof.waiting
            && !proof.children_working
            && proof.completion_revision > watch.baseline_revision
            && proof
                .completed
                .as_ref()
                .is_some_and(|end| end.at_micros <= now_micros))
        .then(|| ReadCodexFinal {
            identity: identity.clone(),
            target: target.clone(),
            proof: proof.clone(),
            sequence: watch.sequence,
            _seal: (),
        });
        ObservationPoll {
            state,
            waiting: working && proof.waiting,
            terminal: None,
            job,
        }
    }

    /// Recheck the current immutable host identity AND the most recent native proof after I/O.
    /// Stale responses cannot install plans or submit results for a replacement attempt/runtime.
    pub fn complete(
        &mut self,
        response: FinalResponseResult,
        current: &ObservationIdentity,
        target: &WatchTarget,
        latest: &NativeTurnProof,
        now_micros: i64,
    ) -> Option<ObservedTurn> {
        let watch = self.watches.get(&response.job.identity.operation_id)?;
        if watch.identity != response.job.identity {
            return None;
        }
        if watch.fresh.is_some() {
            let id = response.job.identity.operation_id.clone();
            let exact = current == &watch.identity
                && target == &watch.target
                && target == &response.job.target
                && latest == &response.job.proof
                && response.job.sequence == watch.sequence;
            let mut poll = if exact {
                Some(self.observe_fresh(current, target, latest, now_micros))
            } else {
                None
            };
            if let Some(terminal) = poll.as_mut().and_then(|poll| poll.terminal.take()) {
                return Some(terminal);
            }
            let valid = exact && poll.as_ref().is_some_and(|poll| poll.job.is_some());
            let text = response
                .text
                .ok()
                .filter(|text| !text.trim().is_empty() && text.len() <= FINAL_TEXT_MAX);
            return if valid && text.is_some() {
                self.finish_fresh(
                    &id,
                    LocalCompletionState::Completed,
                    "observation_native_final_confirmed",
                    text,
                    Some(latest.clone()),
                )
            } else {
                self.finish_fresh(
                    &id,
                    LocalCompletionState::Unknown,
                    "observation_stale_or_unproven_response",
                    None,
                    None,
                )
            };
        }
        let key = watch.key.clone();
        if current != &watch.identity
            || target != &watch.target
            || target != &response.job.target
            || latest != &response.job.proof
            || response.job.sequence != watch.sequence
        {
            let decision = self.tracker.cancel(
                &key,
                LocalCompletionState::Unknown,
                "observation_stale_response",
            )?;
            return self.terminal(decision, None, None);
        }
        let text = match response.text {
            Ok(text) if !text.trim().is_empty() && text.len() <= FINAL_TEXT_MAX => text,
            _ => {
                let decision = self.tracker.cancel(
                    &key,
                    LocalCompletionState::Unknown,
                    "observation_final_response_unproven",
                )?;
                return self.terminal(decision, None, None);
            }
        };
        let decision = self
            .tracker
            .observe(target, latest, true, now_micros)
            .into_iter()
            .next()?;
        let completed = decision.state == LocalCompletionState::Completed;
        self.terminal(
            decision,
            completed.then_some(text),
            completed.then(|| latest.clone()),
        )
    }

    pub fn expire(&mut self, now_micros: i64) -> Vec<ObservedTurn> {
        let mut terminal: Vec<_> = self
            .tracker
            .expire(now_micros)
            .into_iter()
            .filter_map(|decision| self.terminal(decision, None, None))
            .collect();
        let expired: Vec<_> = self
            .watches
            .values()
            .filter_map(|watch| {
                let fresh = watch.fresh.as_ref()?;
                (now_micros >= fresh.created_at.saturating_add(WATCH_TIMEOUT)
                    || (fresh.bound.is_none()
                        && now_micros
                            >= fresh
                                .accepted_at
                                .unwrap_or(fresh.created_at)
                                .saturating_add(START_TIMEOUT)))
                .then(|| watch.identity.operation_id.clone())
            })
            .collect();
        terminal.extend(expired.into_iter().filter_map(|id| {
            self.finish_fresh(
                &id,
                LocalCompletionState::Expired,
                "observation_watch_expired",
                None,
                None,
            )
        }));
        terminal
    }

    pub fn revoke(&mut self, identity: &ObservationIdentity) -> Option<ObservedTurn> {
        let watch = self.watches.get(&identity.operation_id)?;
        if &watch.identity != identity {
            return None;
        }
        if watch.fresh.is_some() {
            return self.finish_fresh(
                &identity.operation_id,
                LocalCompletionState::Unknown,
                "observation_route_revoked",
                None,
                None,
            );
        }
        let decision = self.tracker.cancel(
            &watch.key,
            LocalCompletionState::Unknown,
            "observation_route_revoked",
        )?;
        self.terminal(decision, None, None)
    }

    fn terminal(
        &mut self,
        decision: Decision,
        text: Option<String>,
        proof: Option<NativeTurnProof>,
    ) -> Option<ObservedTurn> {
        let id = OperationId(decision.key.operation_id.clone());
        let watch = self.watches.remove(&id)?;
        Some(ObservedTurn {
            identity: watch.identity,
            state: decision.state,
            reason: decision.reason,
            turn_id: proof.as_ref().map(|p| p.turn_id.clone()),
            sequence: text.as_ref().map(|_| watch.sequence),
            text,
            proof,
        })
    }

    fn finish_fresh(
        &mut self,
        id: &OperationId,
        state: LocalCompletionState,
        reason: &'static str,
        text: Option<String>,
        proof: Option<NativeTurnProof>,
    ) -> Option<ObservedTurn> {
        self.watches.get(id)?.fresh.as_ref()?;
        let watch = self.watches.remove(id)?;
        Some(ObservedTurn {
            identity: watch.identity,
            state,
            reason,
            turn_id: proof.as_ref().map(|proof| proof.turn_id.clone()),
            sequence: text.as_ref().map(|_| watch.sequence),
            text,
            proof,
        })
    }

    fn observe_fresh(
        &mut self,
        identity: &ObservationIdentity,
        target: &WatchTarget,
        proof: &NativeTurnProof,
        now: i64,
    ) -> ObservationPoll {
        let mut poll = ObservationPoll {
            state: LocalCompletionState::AwaitingStart,
            waiting: false,
            terminal: None,
            job: None,
        };
        let Some(watch) = self.watches.get_mut(&identity.operation_id) else {
            poll.state = LocalCompletionState::Unknown;
            return poll;
        };
        let fresh = watch.fresh.as_mut().expect("fresh watch selected");
        let process = target.execution.process_identity();
        let identity_matches = &watch.identity == identity
            && &watch.target == target
            && proof.is_valid()
            && proof.session_key == format!("{}:{}", target.workspace, target.session.0)
            && proof.runtime_instance == target.runtime_token
            && proof.native_session_id == target.native_session_id
            && proof.start_execution_stamp.contains(process)
            && proof
                .completed
                .as_ref()
                .is_none_or(|end| end.execution_stamp.contains(process));
        let failure = if now >= fresh.created_at.saturating_add(WATCH_TIMEOUT) {
            Some((LocalCompletionState::Expired, "observation_watch_expired"))
        } else if !identity_matches || proof.cancelled || proof.ended {
            Some((
                LocalCompletionState::Unknown,
                "observation_native_identity_changed",
            ))
        } else if let Some(accepted) = fresh.accepted_at.filter(|_| fresh.admitted) {
            if fresh
                .bound
                .as_ref()
                .is_some_and(|bound| !same_native_turn(bound, proof))
            {
                Some((
                    LocalCompletionState::Unknown,
                    "observation_fresh_turn_changed",
                ))
            } else if proof.started_at_micros < accepted {
                None // A cached earlier turn grants no first-goal evidence.
            } else if proof.started_at_micros > accepted.saturating_add(START_TIMEOUT)
                || proof.prompt_digest.as_deref() != Some(&fresh.prompt_digest)
            {
                Some((
                    LocalCompletionState::Unknown,
                    "observation_fresh_turn_unproven",
                ))
            } else {
                if fresh.bound.is_none() {
                    fresh.bound = Some(proof.clone());
                }
                poll.state = LocalCompletionState::Working;
                poll.waiting = proof.waiting;
                if !proof.waiting
                    && !proof.children_working
                    && proof.completion_revision > 0
                    && proof
                        .completed
                        .as_ref()
                        .is_some_and(|end| end.at_micros <= now)
                {
                    poll.job = Some(ReadCodexFinal {
                        identity: identity.clone(),
                        target: target.clone(),
                        proof: proof.clone(),
                        sequence: watch.sequence,
                        _seal: (),
                    });
                }
                None
            }
        } else {
            None
        };
        if let Some((state, reason)) = failure {
            poll.state = state;
            poll.terminal = self.finish_fresh(&identity.operation_id, state, reason, None, None);
        }
        poll
    }
}

fn same_native_turn(left: &NativeTurnProof, right: &NativeTurnProof) -> bool {
    left.session_generation == right.session_generation
        && left.turn_id == right.turn_id
        && left.started_at_micros == right.started_at_micros
        && left.start_execution_stamp == right.start_execution_stamp
        && left.transcript_path == right.transcript_path
}

pub(crate) struct FinalResponseWorker {
    worker: LazyBoundedWorker<Box<ReadCodexFinal>, FinalResponseResult>,
    pending: Option<ReadCodexFinal>,
}
pub(crate) enum FinalReadSubmitError {
    Full(Box<ReadCodexFinal>),
    Unavailable(Box<ReadCodexFinal>),
}
impl FinalReadSubmitError {
    pub fn into_job(self) -> ReadCodexFinal {
        match self {
            Self::Full(job) | Self::Unavailable(job) => *job,
        }
    }
}
impl FinalResponseWorker {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            worker: LazyBoundedWorker::new(
                "command-center-observation",
                Duration::from_secs(20),
                || {
                    |job: Box<ReadCodexFinal>| FinalResponseResult {
                        text: read_final_text(&job.proof),
                        job: *job,
                    }
                },
                wake,
            ),
            pending: None,
        }
    }
    pub fn try_request(&mut self, job: ReadCodexFinal) -> Result<(), FinalReadSubmitError> {
        let pending = job.clone();
        self.worker
            .try_request(Box::new(job))
            .map_err(|error| match error {
                LazyWorkerSubmitError::Full(job) => FinalReadSubmitError::Full(job),
                LazyWorkerSubmitError::Unavailable { job, .. } => {
                    FinalReadSubmitError::Unavailable(job)
                }
            })?;
        self.pending = Some(pending);
        Ok(())
    }
    pub fn try_recv(&mut self) -> Option<FinalResponseResult> {
        let outcome = self.worker.try_recv()?;
        let job = self.pending.take()?;
        Some(outcome.into_result().unwrap_or(FinalResponseResult {
            job,
            text: Err(ObservationError::Unavailable),
        }))
    }
}

fn read_final_text(proof: &NativeTurnProof) -> Result<String, ObservationError> {
    use std::io::{Read, Seek, SeekFrom};
    let unproven = ObservationError::Unproven;
    // Retain the existing final-boundary verifier, including native session/turn/error barriers.
    if !completion::confirm_codex_rollout(proof) {
        return Err(unproven);
    }
    let deadline = std::time::Instant::now() + Duration::from_millis(20);
    let path = std::path::Path::new(&proof.transcript_path);
    let before = std::fs::symlink_metadata(path).map_err(|_| ObservationError::Unavailable)?;
    if !before.is_file() {
        return Err(unproven);
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options
        .open(path)
        .map_err(|_| ObservationError::Unavailable)?;
    let opened = file.metadata().map_err(|_| ObservationError::Unavailable)?;
    if !same_file(&before, &opened) {
        return Err(unproven);
    }
    const HEAD: u64 = 64 * 1024;
    const TAIL: u64 = 256 * 1024;
    const ROW: usize = 64 * 1024;
    let mut header = Vec::new();
    (&mut file)
        .take(HEAD)
        .read_to_end(&mut header)
        .map_err(|_| ObservationError::Unavailable)?;
    let end = header
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or(unproven)?;
    let meta: serde_json::Value = serde_json::from_slice(&header[..end]).map_err(|_| unproven)?;
    if meta["type"] != "session_meta"
        || meta
            .pointer("/payload/id")
            .and_then(serde_json::Value::as_str)
            != Some(proof.native_session_id.as_str())
    {
        return Err(unproven);
    }
    let start = opened.len().saturating_sub(TAIL);
    file.seek(SeekFrom::Start(start))
        .map_err(|_| ObservationError::Unavailable)?;
    let mut tail = vec![0; (opened.len() - start) as usize];
    file.read_exact(&mut tail)
        .map_err(|_| ObservationError::Unavailable)?;
    if start > 0 {
        let first = tail
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or(unproven)?;
        tail.drain(..=first);
    }
    if !tail.ends_with(b"\n") {
        return Err(unproven);
    }
    let mut text = None;
    for (index, line) in tail
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        if index >= 2048 || line.len() > ROW || std::time::Instant::now() >= deadline {
            return Err(ObservationError::Limit);
        }
        let value: serde_json::Value = serde_json::from_slice(line).map_err(|_| unproven)?;
        if value["type"] != "event_msg" {
            continue;
        }
        match value
            .pointer("/payload/type")
            .and_then(serde_json::Value::as_str)
        {
            Some("task_started" | "turn_started" | "turn_aborted") => text = None,
            Some("task_complete" | "turn_complete") => {
                text = None;
                if value
                    .pointer("/payload/turn_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(proof.turn_id.as_str())
                    && value
                        .pointer("/payload/error")
                        .is_none_or(serde_json::Value::is_null)
                    && let Some(final_text) = value
                        .pointer("/payload/last_agent_message")
                        .and_then(serde_json::Value::as_str)
                {
                    if final_text.len() > FINAL_TEXT_MAX {
                        return Err(ObservationError::Limit);
                    }
                    if !final_text.trim().is_empty() {
                        text = Some(final_text.to_owned());
                    }
                }
            }
            _ => {}
        }
    }
    let after = file.metadata().map_err(|_| ObservationError::Unavailable)?;
    let path_after = std::fs::symlink_metadata(path).map_err(|_| ObservationError::Unavailable)?;
    if std::time::Instant::now() >= deadline
        || !same_file(&opened, &after)
        || !same_file(&opened, &path_after)
        || opened.len() != after.len()
        || opened.modified().ok() != after.modified().ok()
    {
        return Err(unproven);
    }
    text.ok_or(unproven)
}

fn same_file(first: &std::fs::Metadata, second: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        first.is_file()
            && second.is_file()
            && first.dev() == second.dev()
            && first.ino() == second.ino()
    }
    #[cfg(not(unix))]
    {
        first.is_file()
            && second.is_file()
            && first.len() == second.len()
            && first.modified().ok() == second.modified().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_detect::{AgentExecutionIdentity, AgentKind};
    use deppy_core::process_identity::HookExecutionStamp;
    use deppy_core::work_management::RuntimeIncarnationId;
    use storage::NativeTurnCompletionProof;
    const NOW: i64 = 10_000_000;
    struct Fixture {
        directory: std::path::PathBuf,
        identity: ObservationIdentity,
        target: WatchTarget,
        baseline: NativeTurnProof,
        proof: NativeTurnProof,
    }
    impl Fixture {
        fn new() -> Self {
            let execution =
                AgentExecutionIdentity::fixture_current(AgentKind::Codex, std::process::id())
                    .unwrap();
            let native = uuid::Uuid::new_v4().to_string();
            let directory =
                std::env::temp_dir().join(format!("deppy-cc-observation-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&directory).unwrap();
            let target = WatchTarget {
                workspace: "workspace".into(),
                session_id: "opaque-route".into(),
                generation: "route-generation".into(),
                runtime: 7,
                session: runtime::SessionId(9),
                execution,
                runtime_token: "runtime-token".into(),
                native_session_id: native.clone(),
            };
            let stamp = HookExecutionStamp {
                runtime_instance: target.runtime_token.clone(),
                ancestors: vec![execution.process_identity()],
            };
            let baseline = NativeTurnProof {
                session_key: "workspace:9".into(),
                runtime_instance: target.runtime_token.clone(),
                native_session_id: native.clone(),
                session_generation: 1,
                turn_id: "previous".into(),
                started_at_micros: 2,
                start_execution_stamp: stamp.clone(),
                prompt_digest: Some(storage::native_prompt_digest("previous")),
                transcript_path: directory
                    .join(format!("rollout-fixture-{native}.jsonl"))
                    .to_str()
                    .unwrap()
                    .into(),
                completed: None,
                completion_revision: 1,
                waiting: false,
                children_working: false,
                cancelled: false,
                ended: false,
            };
            let mut proof = baseline.clone();
            proof.turn_id = "own-turn".into();
            proof.started_at_micros = NOW + 2;
            proof.prompt_digest = Some(storage::native_prompt_digest("actual submitted prompt"));
            proof.completed = Some(NativeTurnCompletionProof {
                at_micros: NOW + 3,
                execution_stamp: stamp,
            });
            proof.completion_revision = 2;
            let identity = ObservationIdentity {
                workspace_id: WorkspaceId("workspace".into()),
                operation_id: OperationId::new(),
                attempt_id: Some(AttemptId::new()),
                binding: RuntimeBinding {
                    incarnation: RuntimeIncarnationId::new(),
                    session_id: target.session,
                    execution_generation: 1,
                },
            };
            let fixture = Self {
                directory,
                identity,
                target,
                baseline,
                proof,
            };
            fixture.write("own-turn", "exact final response", None);
            fixture
        }
        fn write(&self, turn: &str, text: &str, suffix: Option<serde_json::Value>) {
            let mut rows = vec![
                serde_json::json!({"type":"session_meta","payload":{"id":self.proof.native_session_id}}),
                serde_json::json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":turn,"last_agent_message":text}}),
            ];
            if let Some(suffix) = suffix {
                rows.push(suffix);
            }
            std::fs::write(
                &self.proof.transcript_path,
                rows.iter().map(|v| format!("{v}\n")).collect::<String>(),
            )
            .unwrap();
        }
        fn tracker(&self) -> CodexTracker {
            let mut tracker = CodexTracker::default();
            tracker
                .begin(
                    self.identity.clone(),
                    self.target.clone(),
                    &self.baseline,
                    10,
                    storage::native_prompt_digest("actual submitted prompt"),
                    NOW,
                )
                .unwrap();
            assert!(
                tracker
                    .accepted_input(
                        7,
                        self.target.session,
                        11,
                        Some(self.identity.operation_id.as_str()),
                        NOW + 1
                    )
                    .is_empty()
            );
            assert!(tracker.admit(&self.identity, Ok(())).is_none());
            tracker
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn exact_admitted_turn_final_text_is_provider_evidence_only() {
        let fixture = Fixture::new();
        let mut tracker = fixture.tracker();
        let poll = tracker.observe(&fixture.identity, &fixture.target, &fixture.proof, NOW + 4);
        assert_eq!(poll.state, LocalCompletionState::Working);
        let job = poll.job.unwrap();
        let response = FinalResponseResult {
            text: read_final_text(&job.proof),
            job,
        };
        let observed = tracker
            .complete(
                response,
                &fixture.identity,
                &fixture.target,
                &fixture.proof,
                NOW + 4,
            )
            .unwrap();
        assert_eq!(observed.state, LocalCompletionState::Completed);
        assert_eq!(observed.text.as_deref(), Some("exact final response"));
        assert_eq!(observed.sequence, Some(11));
        assert!(tracker.native_turn_keys().is_empty());
    }
    #[test]
    fn rollout_turn_mismatch_new_start_missing_final_and_symlink_are_unproven() {
        let fixture = Fixture::new();
        fixture.write("foreign", "foreign result", None);
        assert_eq!(
            read_final_text(&fixture.proof),
            Err(ObservationError::Unproven)
        );
        fixture.write("own-turn","old result",Some(serde_json::json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"next"}})));
        assert_eq!(
            read_final_text(&fixture.proof),
            Err(ObservationError::Unproven)
        );
        fixture.write("own-turn", "", None);
        assert_eq!(
            read_final_text(&fixture.proof),
            Err(ObservationError::Unproven)
        );
        fixture.write("own-turn", "x".repeat(FINAL_TEXT_MAX + 1).as_str(), None);
        assert!(read_final_text(&fixture.proof).is_err());
        #[cfg(unix)]
        {
            let original = fixture.directory.join("original");
            std::fs::rename(&fixture.proof.transcript_path, &original).unwrap();
            std::os::unix::fs::symlink(original, &fixture.proof.transcript_path).unwrap();
            assert!(read_final_text(&fixture.proof).is_err());
        }
    }
    #[test]
    fn stale_attempt_execution_generation_and_native_proof_cannot_publish_text() {
        for change in 0..3 {
            let fixture = Fixture::new();
            let mut tracker = fixture.tracker();
            let job = tracker
                .observe(&fixture.identity, &fixture.target, &fixture.proof, NOW + 4)
                .job
                .unwrap();
            let mut current = fixture.identity.clone();
            let mut latest = fixture.proof.clone();
            match change {
                0 => current.attempt_id = Some(AttemptId::new()),
                1 => current.binding.execution_generation += 1,
                _ => latest.session_generation += 1,
            };
            let observed = tracker
                .complete(
                    FinalResponseResult {
                        job,
                        text: Ok("untrusted stale text".into()),
                    },
                    &current,
                    &fixture.target,
                    &latest,
                    NOW + 4,
                )
                .unwrap();
            assert_eq!(observed.state, LocalCompletionState::Unknown);
            assert!(observed.text.is_none());
        }
    }
    #[test]
    fn unadmitted_intervening_input_and_native_waiting_never_yield_final_read() {
        let fixture = Fixture::new();
        let mut tracker = CodexTracker::default();
        tracker
            .begin(
                fixture.identity.clone(),
                fixture.target.clone(),
                &fixture.baseline,
                10,
                storage::native_prompt_digest("actual submitted prompt"),
                NOW,
            )
            .unwrap();
        assert!(
            tracker
                .observe(&fixture.identity, &fixture.target, &fixture.proof, NOW + 4)
                .job
                .is_none()
        );
        let changes = tracker.accepted_input(
            7,
            fixture.target.session,
            11,
            Some("foreign-operation"),
            NOW + 1,
        );
        assert_eq!(changes[0].state, LocalCompletionState::Unknown);
        let mut tracker = fixture.tracker();
        let mut waiting = fixture.proof.clone();
        waiting.waiting = true;
        let poll = tracker.observe(&fixture.identity, &fixture.target, &waiting, NOW + 4);
        assert!(poll.waiting);
        assert!(poll.job.is_none());
    }

    #[test]
    fn lazy_worker_has_one_exact_request_and_reads_only_after_admission() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let fixture = Fixture::new();
        let mut tracker = fixture.tracker();
        let job = tracker
            .observe(&fixture.identity, &fixture.target, &fixture.proof, NOW + 4)
            .job
            .unwrap();
        let wakes = Arc::new(AtomicUsize::new(0));
        let signal = wakes.clone();
        let mut worker = FinalResponseWorker::new(move || {
            signal.fetch_add(1, Ordering::SeqCst);
        });
        assert!(!worker.worker.has_live_worker());
        assert_eq!(wakes.load(Ordering::SeqCst), 0);
        assert!(worker.try_request(job.clone()).is_ok());
        let error = worker.try_request(job).err().unwrap();
        assert!(matches!(&error, FinalReadSubmitError::Full(_)));
        assert_eq!(error.into_job().identity, fixture.identity);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let result = loop {
            if let Some(result) = worker.try_recv() {
                break result;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(result.job.identity, fixture.identity);
        assert_eq!(result.job.sequence, 11);
        let observed = tracker
            .complete(
                result,
                &fixture.identity,
                &fixture.target,
                &fixture.proof,
                NOW + 4,
            )
            .unwrap();
        assert_eq!(observed.text.as_deref(), Some("exact final response"));
        while wakes.load(Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
    }

    #[test]
    fn native_rollout_path_change_is_not_an_owned_turn() {
        let fixture = Fixture::new();
        let mut tracker = fixture.tracker();
        let mut changed = fixture.proof.clone();
        changed.transcript_path = fixture
            .directory
            .join(format!(
                "rollout-replacement-{}.jsonl",
                changed.native_session_id
            ))
            .to_str()
            .unwrap()
            .into();
        let poll = tracker.observe(&fixture.identity, &fixture.target, &changed, NOW + 4);
        assert!(poll.job.is_none());
        assert_eq!(poll.terminal.unwrap().state, LocalCompletionState::Unknown);
    }

    #[test]
    fn a_different_cli_kind_cannot_claim_codex_native_completion() {
        let fixture = Fixture::new();
        let mut target = fixture.target.clone();
        target.execution =
            AgentExecutionIdentity::fixture_current(AgentKind::Claude, std::process::id()).unwrap();
        let mut tracker = CodexTracker::default();
        assert!(
            tracker
                .begin(
                    fixture.identity.clone(),
                    target,
                    &fixture.baseline,
                    10,
                    storage::native_prompt_digest("actual submitted prompt"),
                    NOW
                )
                .is_err()
        );
    }

    fn fresh(fixture: &Fixture) -> CodexTracker {
        let mut tracker = CodexTracker::default();
        tracker
            .begin_fresh(
                fixture.identity.clone(),
                fixture.target.clone(),
                10,
                storage::native_prompt_digest("actual submitted prompt"),
                NOW,
            )
            .unwrap();
        tracker
    }
    fn accept_fresh(tracker: &mut CodexTracker, fixture: &Fixture) {
        assert!(
            tracker
                .accepted_input(
                    7,
                    fixture.target.session,
                    11,
                    Some(fixture.identity.operation_id.as_str()),
                    NOW + 1
                )
                .is_empty()
        );
        assert!(tracker.admit(&fixture.identity, Ok(())).is_none());
    }

    #[test]
    fn fresh_first_goal_needs_both_real_receipts_and_real_native_turn() {
        let fixture = Fixture::new();
        for admit_first in [false, true] {
            let mut tracker = fresh(&fixture);
            assert_eq!(tracker.native_turn_keys(), vec!["workspace:9"]);
            assert!(
                tracker
                    .observe(&fixture.identity, &fixture.target, &fixture.proof, NOW + 4)
                    .job
                    .is_none()
            );
            if admit_first {
                assert!(tracker.admit(&fixture.identity, Ok(())).is_none());
            }
            assert!(
                tracker
                    .accepted_input(
                        7,
                        fixture.target.session,
                        11,
                        Some(fixture.identity.operation_id.as_str()),
                        NOW + 1
                    )
                    .is_empty()
            );
            if !admit_first {
                assert!(
                    tracker
                        .observe(&fixture.identity, &fixture.target, &fixture.proof, NOW + 4)
                        .job
                        .is_none()
                );
                assert!(tracker.admit(&fixture.identity, Ok(())).is_none());
            }
            let job = tracker
                .observe(&fixture.identity, &fixture.target, &fixture.proof, NOW + 4)
                .job
                .unwrap();
            let response = FinalResponseResult {
                text: read_final_text(&job.proof),
                job,
            };
            let observed = tracker
                .complete(
                    response,
                    &fixture.identity,
                    &fixture.target,
                    &fixture.proof,
                    NOW + 4,
                )
                .unwrap();
            assert_eq!(observed.state, LocalCompletionState::Completed);
            assert_eq!(observed.text.as_deref(), Some("exact final response"));
            assert_eq!(observed.sequence, Some(11));
            assert!(tracker.native_turn_keys().is_empty());
            assert!(
                tracker
                    .begin_fresh(
                        fixture.identity.clone(),
                        fixture.target.clone(),
                        11,
                        storage::native_prompt_digest("actual submitted prompt"),
                        NOW + 5
                    )
                    .is_err()
            );
        }
    }

    #[test]
    fn fresh_first_goal_rejects_foreign_digest_native_runtime_and_process() {
        let fixture = Fixture::new();
        for change in 0..5 {
            let mut tracker = fresh(&fixture);
            accept_fresh(&mut tracker, &fixture);
            let mut proof = fixture.proof.clone();
            match change {
                0 => proof.prompt_digest = Some(storage::native_prompt_digest("foreign prompt")),
                1 => proof.native_session_id = uuid::Uuid::new_v4().to_string(),
                2 => proof.runtime_instance = "replacement-runtime".into(),
                3 => proof.start_execution_stamp.ancestors.clear(),
                _ => proof.started_at_micros = NOW + 30_000_002,
            }
            let poll =
                tracker.observe(&fixture.identity, &fixture.target, &proof, NOW + 31_000_000);
            assert!(poll.job.is_none());
            assert_eq!(poll.terminal.unwrap().state, LocalCompletionState::Unknown);
        }
    }

    #[test]
    fn fresh_first_goal_pins_native_generation_turn_and_start_after_receipts() {
        let fixture = Fixture::new();
        for change in 0..5 {
            let mut tracker = fresh(&fixture);
            accept_fresh(&mut tracker, &fixture);
            let mut active = fixture.proof.clone();
            active.completed = None;
            assert_eq!(
                tracker
                    .observe(&fixture.identity, &fixture.target, &active, NOW + 4)
                    .state,
                LocalCompletionState::Working
            );
            let mut changed = fixture.proof.clone();
            match change {
                0 => changed.session_generation += 1,
                1 => changed.turn_id = "replacement-turn".into(),
                2 => changed.started_at_micros += 1,
                3 => {
                    changed.transcript_path = fixture
                        .directory
                        .join(format!(
                            "rollout-replacement-{}.jsonl",
                            changed.native_session_id
                        ))
                        .to_str()
                        .unwrap()
                        .into()
                }
                _ => changed.started_at_micros = NOW,
            }
            let poll = tracker.observe(&fixture.identity, &fixture.target, &changed, NOW + 5);
            assert!(poll.job.is_none());
            assert_eq!(poll.terminal.unwrap().state, LocalCompletionState::Unknown);
        }
    }

    #[test]
    fn fresh_intervening_input_and_stale_async_response_do_not_publish() {
        let fixture = Fixture::new();
        let mut tracker = fresh(&fixture);
        accept_fresh(&mut tracker, &fixture);
        let job = tracker
            .observe(&fixture.identity, &fixture.target, &fixture.proof, NOW + 4)
            .job
            .unwrap();
        let changed = tracker.accepted_input(
            7,
            fixture.target.session,
            12,
            Some("another-input"),
            NOW + 5,
        );
        assert_eq!(changed[0].state, LocalCompletionState::Unknown);
        assert!(
            tracker
                .complete(
                    FinalResponseResult {
                        job,
                        text: Ok("stale".into())
                    },
                    &fixture.identity,
                    &fixture.target,
                    &fixture.proof,
                    NOW + 6
                )
                .is_none()
        );
        let mut tracker = fresh(&fixture);
        accept_fresh(&mut tracker, &fixture);
        let job = tracker
            .observe(&fixture.identity, &fixture.target, &fixture.proof, NOW + 4)
            .job
            .unwrap();
        let mut replacement = fixture.identity.clone();
        replacement.binding.execution_generation += 1;
        let outcome = tracker
            .complete(
                FinalResponseResult {
                    job,
                    text: Ok("stale".into()),
                },
                &replacement,
                &fixture.target,
                &fixture.proof,
                NOW + 6,
            )
            .unwrap();
        assert_eq!(outcome.state, LocalCompletionState::Unknown);
        assert!(outcome.text.is_none());
    }

    #[test]
    fn fresh_unknown_delivery_waiting_expiry_and_cross_mode_dedup_are_truthful() {
        let fixture = Fixture::new();
        let mut tracker = fresh(&fixture);
        let terminal = tracker
            .admit(
                &fixture.identity,
                Err(runtime::PtyInputRejectReason::AdmissionUnknown),
            )
            .unwrap();
        assert_eq!(terminal.state, LocalCompletionState::Unknown);
        assert!(terminal.text.is_none());
        assert!(
            tracker
                .begin(
                    fixture.identity.clone(),
                    fixture.target.clone(),
                    &fixture.baseline,
                    10,
                    storage::native_prompt_digest("actual submitted prompt"),
                    NOW
                )
                .is_err()
        );
        let mut tracker = fresh(&fixture);
        accept_fresh(&mut tracker, &fixture);
        let mut waiting = fixture.proof.clone();
        waiting.waiting = true;
        let poll = tracker.observe(&fixture.identity, &fixture.target, &waiting, NOW + 4);
        assert!(poll.waiting && poll.job.is_none());
        assert_eq!(
            tracker.expire(NOW + WATCH_TIMEOUT)[0].state,
            LocalCompletionState::Expired
        );
        let mut tracker = fresh(&fixture);
        assert_eq!(
            tracker.expire(NOW + START_TIMEOUT)[0].state,
            LocalCompletionState::Expired
        );
        let mut other = fixture.identity.clone();
        other.operation_id = OperationId::new();
        let mut tracker = fixture.tracker();
        assert!(
            tracker
                .begin_fresh(
                    other,
                    fixture.target.clone(),
                    10,
                    storage::native_prompt_digest("actual submitted prompt"),
                    NOW
                )
                .is_err()
        );
    }
}
