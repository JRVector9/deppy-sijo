//! Native completion binding. Tracker transitions are in-memory and contain no transport effects.
//! The separate rollout verifier is called only by the existing background state worker.
use crate::agent_detect::AgentExecutionIdentity;
use std::collections::{HashMap, VecDeque};
use storage::NativeTurnProof;

const WATCH_MAX: usize = 64;
const STATUS_MAX: usize = 1000;
const START_TIMEOUT_MICROS: i64 = 30_000_000;
const WATCH_TIMEOUT_MICROS: i64 = 60 * 60 * 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocalCompletionState {
    AwaitingStart,
    Working,
    Completed,
    Unknown,
    Unsupported,
    Expired,
    Revoked,
    Rejected,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct WatchKey {
    pub connection_id: String,
    pub operation_id: String,
    pub run_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WatchTarget {
    pub workspace: String,
    pub session_id: String,
    pub generation: String,
    pub runtime: u64,
    pub session: runtime::SessionId,
    pub execution: AgentExecutionIdentity,
    pub runtime_token: String,
    pub native_session_id: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Decision {
    pub key: WatchKey,
    pub target: WatchTarget,
    pub state: LocalCompletionState,
    pub completion_revision: Option<i64>,
    pub completed_at_micros: Option<i64>,
    pub reason: &'static str,
}

struct Watch {
    key: WatchKey,
    target: WatchTarget,
    input_operation_id: String,
    prompt_digest: String,
    baseline_started: i64,
    baseline_turn: String,
    native_generation: i64,
    baseline_revision: i64,
    sequence: u64,
    accepted_at: Option<i64>,
    admitted: bool,
    created_at: i64,
    bound_turn: Option<String>,
    state: LocalCompletionState,
}

#[derive(Default)]
pub(crate) struct Tracker {
    active: HashMap<WatchKey, Watch>,
    statuses: VecDeque<Decision>,
}

fn bounded_id(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

fn matches_proof(target: &WatchTarget, proof: &NativeTurnProof) -> bool {
    let process = target.execution.process_identity();
    proof.is_valid()
        && proof.session_key == format!("{}:{}", target.workspace, target.session.0)
        && proof.runtime_instance == target.runtime_token
        && proof.native_session_id == target.native_session_id
        && proof.start_execution_stamp.contains(process)
        && proof
            .completed
            .as_ref()
            .is_none_or(|end| end.execution_stamp.contains(process))
}

impl Tracker {
    /// Roll back only a pre-durable-claim reservation known not to have sent input.
    pub(crate) fn discard_unclaimed(&mut self, key: &WatchKey) {
        self.active.remove(key);
        self.statuses.retain(|status| &status.key != key);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn begin(
        &mut self,
        key: WatchKey,
        target: WatchTarget,
        baseline: &NativeTurnProof,
        baseline_input_sequence: u64,
        input_operation_id: String,
        prompt_digest: String,
        now_micros: i64,
    ) -> Result<(), &'static str> {
        if !bounded_id(&key.connection_id, 128)
            || !bounded_id(&key.operation_id, 128)
            || key.run_id.as_deref().is_some_and(|id| !bounded_id(id, 128))
            || !bounded_id(&target.workspace, 1024)
            || !bounded_id(&target.session_id, 256)
            || !bounded_id(&target.generation, 256)
            || !bounded_id(&input_operation_id, 128)
            || prompt_digest.len() != 64
            || !prompt_digest.bytes().all(|b| b.is_ascii_hexdigit())
            || now_micros <= 0
            || baseline_input_sequence == u64::MAX
        {
            return Err("completion_invalid_scope");
        }
        if !matches_proof(&target, baseline) || baseline.cancelled || baseline.ended {
            return Err("completion_native_proof_unsupported");
        }
        if self.active.contains_key(&key) || self.statuses.iter().any(|status| status.key == key) {
            return Err("completion_operation_already_tracked");
        }
        if self.active.values().any(|watch| {
            watch.target.session_id == target.session_id
                || (watch.target.runtime == target.runtime
                    && watch.target.session == target.session)
        }) {
            return Err("completion_target_busy");
        }
        if self.active.len() >= WATCH_MAX {
            return Err("completion_watch_capacity");
        }
        self.active.insert(
            key.clone(),
            Watch {
                key,
                target,
                input_operation_id,
                prompt_digest,
                baseline_started: baseline.started_at_micros,
                baseline_turn: baseline.turn_id.clone(),
                native_generation: baseline.session_generation,
                baseline_revision: baseline.completion_revision,
                sequence: baseline_input_sequence,
                accepted_at: None,
                admitted: false,
                created_at: now_micros,
                bound_turn: None,
                state: LocalCompletionState::AwaitingStart,
            },
        );
        Ok(())
    }

    pub(crate) fn accepted_input(
        &mut self,
        runtime: u64,
        session: runtime::SessionId,
        sequence: u64,
        operation_id: Option<&str>,
        at_micros: i64,
    ) -> Vec<Decision> {
        let mut ended = Vec::new();
        for watch in self
            .active
            .values_mut()
            .filter(|watch| watch.target.runtime == runtime && watch.target.session == session)
        {
            if sequence == watch.sequence && sequence > 0 {
                continue;
            }
            if sequence < watch.sequence {
                continue;
            }
            if sequence == 0
                || sequence != watch.sequence.saturating_add(1)
                || watch.accepted_at.is_some()
                || operation_id != Some(&watch.input_operation_id)
                || at_micros < watch.created_at
            {
                ended.push(watch.key.clone());
            } else {
                watch.sequence = sequence;
                watch.accepted_at = Some(at_micros);
            }
        }
        ended
            .into_iter()
            .filter_map(|key| {
                self.cancel(
                    &key,
                    LocalCompletionState::Unknown,
                    "completion_input_intervened",
                )
            })
            .collect()
    }

    pub(crate) fn admit(
        &mut self,
        key: &WatchKey,
        result: Result<(), runtime::PtyInputRejectReason>,
    ) -> Option<Decision> {
        match result {
            Ok(()) => {
                self.active.get_mut(key)?.admitted = true;
                None
            }
            Err(runtime::PtyInputRejectReason::AdmissionUnknown) => self.cancel(
                key,
                LocalCompletionState::Unknown,
                "completion_admission_unknown",
            ),
            Err(_) => self.cancel(
                key,
                LocalCompletionState::Rejected,
                "completion_input_rejected",
            ),
        }
    }

    pub(crate) fn observe(
        &mut self,
        target: &WatchTarget,
        proof: &NativeTurnProof,
        rollout_confirmed: bool,
        now_micros: i64,
    ) -> Vec<Decision> {
        let mut terminal = Vec::new();
        for watch in self
            .active
            .values_mut()
            .filter(|watch| watch.target.session_id == target.session_id)
        {
            let fail = if watch.target != *target
                || !matches_proof(target, proof)
                || proof.session_generation != watch.native_generation
            {
                Some("completion_identity_changed")
            } else if proof.cancelled || proof.ended {
                Some("completion_native_cancelled")
            } else {
                None
            };
            if let Some(reason) = fail {
                terminal.push((
                    watch.key.clone(),
                    LocalCompletionState::Unknown,
                    reason,
                    None,
                    None,
                ));
                continue;
            }
            let Some(accepted_at) = watch.accepted_at.filter(|_| watch.admitted) else {
                continue;
            };
            if proof.started_at_micros <= watch.baseline_started {
                continue;
            }
            if proof.turn_id == watch.baseline_turn
                || proof.started_at_micros < accepted_at
                || proof.started_at_micros > accepted_at.saturating_add(START_TIMEOUT_MICROS)
                || proof.prompt_digest.as_deref() != Some(watch.prompt_digest.as_str())
                || watch
                    .bound_turn
                    .as_ref()
                    .is_some_and(|turn| turn != &proof.turn_id)
            {
                terminal.push((
                    watch.key.clone(),
                    LocalCompletionState::Unknown,
                    "completion_turn_binding_ambiguous",
                    None,
                    None,
                ));
                continue;
            }
            watch.bound_turn = Some(proof.turn_id.clone());
            watch.state = LocalCompletionState::Working;
            if let Some(end) = &proof.completed
                && rollout_confirmed
                && !proof.waiting
                && !proof.children_working
                && proof.completion_revision > watch.baseline_revision
                && end.at_micros >= proof.started_at_micros
                && now_micros >= end.at_micros
            {
                terminal.push((
                    watch.key.clone(),
                    LocalCompletionState::Completed,
                    "completion_native_verified",
                    Some(proof.completion_revision),
                    Some(end.at_micros),
                ));
            }
        }
        terminal
            .into_iter()
            .filter_map(|(key, state, reason, revision, completed_at)| {
                self.finish(&key, state, reason, revision, completed_at)
            })
            .collect()
    }

    fn finish(
        &mut self,
        key: &WatchKey,
        state: LocalCompletionState,
        reason: &'static str,
        completion_revision: Option<i64>,
        completed_at_micros: Option<i64>,
    ) -> Option<Decision> {
        let watch = self.active.remove(key)?;
        let decision = Decision {
            key: watch.key,
            target: watch.target,
            state,
            completion_revision,
            completed_at_micros,
            reason,
        };
        if self.statuses.len() == STATUS_MAX {
            self.statuses.pop_front();
        }
        self.statuses.push_back(decision.clone());
        Some(decision)
    }

    pub(crate) fn cancel(
        &mut self,
        key: &WatchKey,
        state: LocalCompletionState,
        reason: &'static str,
    ) -> Option<Decision> {
        let state = match state {
            LocalCompletionState::Unknown
            | LocalCompletionState::Unsupported
            | LocalCompletionState::Expired
            | LocalCompletionState::Revoked
            | LocalCompletionState::Rejected => state,
            _ => LocalCompletionState::Unknown,
        };
        self.finish(key, state, reason, None, None)
    }

    pub(crate) fn expire(&mut self, now_micros: i64) -> Vec<Decision> {
        let expired: Vec<_> = self
            .active
            .values()
            .filter_map(|watch| {
                let start_deadline = watch
                    .accepted_at
                    .unwrap_or(watch.created_at)
                    .saturating_add(START_TIMEOUT_MICROS);
                let total_deadline = watch.created_at.saturating_add(WATCH_TIMEOUT_MICROS);
                (now_micros >= total_deadline
                    || (watch.bound_turn.is_none() && now_micros >= start_deadline))
                    .then_some(watch.key.clone())
            })
            .collect();
        expired
            .into_iter()
            .filter_map(|key| {
                self.cancel(
                    &key,
                    LocalCompletionState::Expired,
                    "completion_watch_expired",
                )
            })
            .collect()
    }

    pub(crate) fn state(&self, key: &WatchKey) -> Option<LocalCompletionState> {
        self.active.get(key).map(|watch| watch.state).or_else(|| {
            self.statuses
                .iter()
                .rev()
                .find(|status| &status.key == key)
                .map(|status| status.state)
        })
    }

    pub(crate) fn watch_keys(&self) -> Vec<WatchKey> {
        self.active.keys().cloned().collect()
    }
}

/// Codex Stop observers run before other stop hooks can request continuation.
/// Its rollout's matching task_complete/turn_complete is the final native boundary.
/// Call only on the background state worker and only for active watches.
pub(crate) fn confirm_codex_rollout(proof: &NativeTurnProof) -> bool {
    confirm_codex_rollout_inner(proof).unwrap_or(false)
}

fn confirm_codex_rollout_inner(proof: &NativeTurnProof) -> std::io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "completion_rollout_invalid",
        )
    };
    if !proof.is_valid() || proof.completed.is_none() {
        return Ok(false);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(20);
    let path = std::path::Path::new(&proof.transcript_path);
    let before = std::fs::symlink_metadata(path)?;
    if !before.is_file() {
        return Ok(false);
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || !same_file(&before, &opened) {
        return Ok(false);
    }
    const HEAD: usize = 64 * 1024;
    const TAIL: u64 = 256 * 1024;
    const ROW: usize = 64 * 1024;
    let mut header = Vec::with_capacity(HEAD);
    (&mut file).take(HEAD as u64).read_to_end(&mut header)?;
    let head_end = header
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or_else(invalid)?;
    let meta: serde_json::Value =
        serde_json::from_slice(&header[..head_end]).map_err(|_| invalid())?;
    if meta["type"] != "session_meta"
        || meta
            .pointer("/payload/id")
            .and_then(serde_json::Value::as_str)
            != Some(&proof.native_session_id)
    {
        return Ok(false);
    }
    let len = opened.len();
    let start = len.saturating_sub(TAIL);
    file.seek(SeekFrom::Start(start))?;
    let mut tail = vec![0; (len - start) as usize];
    file.read_exact(&mut tail)?;
    if start > 0 {
        let Some(first) = tail.iter().position(|byte| *byte == b'\n') else {
            return Ok(false);
        };
        tail.drain(..=first);
    }
    if !tail.ends_with(b"\n") {
        return Ok(false);
    }
    let mut confirmed = false;
    let mut rows = 0;
    for line in tail
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        rows += 1;
        if rows > 2048 || line.len() > ROW || std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        let value: serde_json::Value = serde_json::from_slice(line).map_err(|_| invalid())?;
        if value["type"] != "event_msg" {
            continue;
        }
        let Some(kind) = value
            .pointer("/payload/type")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        let turn = value
            .pointer("/payload/turn_id")
            .and_then(serde_json::Value::as_str);
        match kind {
            "task_started" | "turn_started" | "turn_aborted" => {
                confirmed = false;
            }
            "task_complete" | "turn_complete" => {
                confirmed = turn == Some(&proof.turn_id)
                    && value
                        .pointer("/payload/error")
                        .is_none_or(serde_json::Value::is_null);
            }
            _ => {}
        }
    }
    let after = file.metadata()?;
    let path_after = std::fs::symlink_metadata(path)?;
    Ok(confirmed
        && std::time::Instant::now() < deadline
        && same_file(&opened, &after)
        && same_file(&opened, &path_after)
        && opened.len() == after.len()
        && opened.modified().ok() == after.modified().ok())
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
    use crate::agent_detect::AgentKind;
    use deppy_core::process_identity::HookExecutionStamp;
    use storage::NativeTurnCompletionProof;

    const START: i64 = 1_000_000;
    fn fixture() -> (WatchKey, WatchTarget, NativeTurnProof) {
        let execution =
            AgentExecutionIdentity::fixture_current(AgentKind::Codex, std::process::id()).unwrap();
        let native = "00000000-0000-0000-0000-000000000002";
        let target = WatchTarget {
            workspace: "00000000-0000-0000-0000-000000000001".into(),
            session_id: "00000000-0000-0000-0000-000000000003".into(),
            generation: "boot:7:9".into(),
            runtime: 7,
            session: runtime::SessionId(9),
            execution,
            runtime_token: "runtime-token".into(),
            native_session_id: native.into(),
        };
        let stamp = HookExecutionStamp {
            runtime_instance: target.runtime_token.clone(),
            ancestors: vec![execution.process_identity()],
        };
        let proof = NativeTurnProof {
            session_key: format!("{}:{}", target.workspace, target.session.0),
            runtime_instance: target.runtime_token.clone(),
            native_session_id: native.into(),
            session_generation: 1,
            turn_id: "previous-turn".into(),
            started_at_micros: 2,
            start_execution_stamp: stamp,
            prompt_digest: Some(storage::native_prompt_digest("previous prompt")),
            transcript_path: format!("/isolated/rollout-fixture-{native}.jsonl"),
            completed: None,
            completion_revision: 4,
            waiting: false,
            children_working: false,
            cancelled: false,
            ended: false,
        };
        let key = WatchKey {
            connection_id: "actor".into(),
            operation_id: "operation".into(),
            run_id: Some("run".into()),
        };
        (key, target, proof)
    }

    fn begun() -> (Tracker, WatchKey, WatchTarget, NativeTurnProof) {
        let (key, target, baseline) = fixture();
        let mut tracker = Tracker::default();
        tracker
            .begin(
                key.clone(),
                target.clone(),
                &baseline,
                10,
                "@internal:operation".into(),
                storage::native_prompt_digest("own prompt"),
                START,
            )
            .unwrap();
        assert!(
            tracker
                .accepted_input(
                    target.runtime,
                    target.session,
                    11,
                    Some("@internal:operation"),
                    START + 1
                )
                .is_empty()
        );
        assert!(tracker.admit(&key, Ok(())).is_none());
        let mut own = baseline;
        own.turn_id = "own-turn".into();
        own.started_at_micros = START + 2;
        own.prompt_digest = Some(storage::native_prompt_digest("own prompt"));
        own.completed = Some(NativeTurnCompletionProof {
            at_micros: START + 3,
            execution_stamp: own.start_execution_stamp.clone(),
        });
        own.completion_revision += 1;
        (tracker, key, target, own)
    }

    #[test]
    fn completion_scrubbed_baseline_keeps_capability_but_cannot_bind_unhashed_new_turn() {
        let (key, target, baseline) = fixture();
        let mut value = serde_json::to_value(baseline).unwrap();
        value["prompt_digest"] = serde_json::Value::Null;
        let mut scrubbed: NativeTurnProof = serde_json::from_value(value)
            .expect("expired temporary digest must preserve native capability evidence");
        assert!(scrubbed.is_valid());
        let mut tracker = Tracker::default();
        tracker
            .begin(
                key.clone(),
                target.clone(),
                &scrubbed,
                10,
                "@internal:operation".into(),
                storage::native_prompt_digest("own prompt"),
                START,
            )
            .unwrap();
        tracker.accepted_input(
            target.runtime,
            target.session,
            11,
            Some("@internal:operation"),
            START + 1,
        );
        tracker.admit(&key, Ok(()));
        scrubbed.turn_id = "unhashed-turn".into();
        scrubbed.started_at_micros = START + 2;
        scrubbed.completed = Some(NativeTurnCompletionProof {
            at_micros: START + 3,
            execution_stamp: scrubbed.start_execution_stamp.clone(),
        });
        scrubbed.completion_revision += 1;
        assert_eq!(
            tracker.observe(&target, &scrubbed, true, START + 4)[0].state,
            LocalCompletionState::Unknown
        );
        assert_eq!(tracker.state(&key), Some(LocalCompletionState::Unknown));
    }

    #[test]
    fn completion_unclaimed_rollback_releases_capacity_and_allows_same_operation_retry() {
        let (key, target, baseline) = fixture();
        let mut tracker = Tracker::default();
        for _ in 0..(WATCH_MAX + 1) {
            tracker
                .begin(
                    key.clone(),
                    target.clone(),
                    &baseline,
                    10,
                    "@internal:operation".into(),
                    storage::native_prompt_digest("own prompt"),
                    START,
                )
                .unwrap();
            tracker.discard_unclaimed(&key);
            assert!(
                tracker.state(&key).is_none(),
                "unsent reservation has no terminal receipt"
            );
            assert!(tracker.watch_keys().is_empty());
        }
        tracker
            .begin(
                key.clone(),
                target,
                &baseline,
                10,
                "@internal:operation".into(),
                storage::native_prompt_digest("own prompt"),
                START,
            )
            .unwrap();
        tracker.admit(&key, Err(runtime::PtyInputRejectReason::AdmissionDenied));
        tracker.discard_unclaimed(&key);
        assert!(
            tracker.state(&key).is_none(),
            "known-unsent claim rollback also clears its provisional status"
        );
    }

    #[test]
    fn completion_native_end_after_absolute_watch_deadline_is_expired() {
        let (mut tracker, key, target, mut own) = begun();
        own.completed.as_mut().unwrap().at_micros = START + WATCH_TIMEOUT_MICROS + 1;
        let decisions = tracker.observe(&target, &own, true, START + WATCH_TIMEOUT_MICROS + 2);
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].state, LocalCompletionState::Expired);
        assert_eq!(tracker.state(&key), Some(LocalCompletionState::Expired));
        assert!(
            tracker
                .observe(&target, &own, true, START + WATCH_TIMEOUT_MICROS + 3)
                .is_empty()
        );
    }

    #[test]
    fn completion_fast_native_end_requires_own_admission_and_final_rollout_once() {
        let (mut tracker, key, target, own) = begun();
        assert!(tracker.observe(&target, &own, false, START + 4).is_empty());
        assert_eq!(tracker.state(&key), Some(LocalCompletionState::Working));
        let decisions = tracker.observe(&target, &own, true, START + 4);
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].key, key);
        assert_eq!(decisions[0].state, LocalCompletionState::Completed);
        assert_eq!(decisions[0].completion_revision, Some(5));
        assert_eq!(decisions[0].completed_at_micros, Some(START + 3));
        assert_eq!(decisions[0].target, target);
        assert_eq!(decisions[0].reason, "completion_native_verified");
        assert!(tracker.observe(&target, &own, true, START + 5).is_empty());
        assert!(tracker.watch_keys().is_empty());
    }

    #[test]
    fn completion_identity_digest_and_turn_replacement_fail_closed() {
        for change in 0..11 {
            let (mut tracker, key, mut target, mut own) = begun();
            match change {
                0 => target.workspace.push('x'),
                1 => target.generation.push('x'),
                2 => target.runtime += 1,
                3 => target.session.0 += 1,
                4 => target.runtime_token.push('x'),
                5 => target.native_session_id.push('x'),
                6 => target.execution = AgentExecutionIdentity::fixture(AgentKind::Codex, 99),
                7 => {
                    own.prompt_digest = Some(storage::native_prompt_digest(
                        "own prompt with another suffix",
                    ))
                }
                8 => own.session_generation += 1,
                9 => own.started_at_micros = START - 1,
                _ => own.turn_id = "previous-turn".into(),
            }
            let result = tracker.observe(&target, &own, true, START + 4);
            assert_eq!(result.len(), 1, "change {change}");
            assert_eq!(
                tracker.state(&key),
                Some(LocalCompletionState::Unknown),
                "change {change}"
            );
        }
    }

    #[test]
    fn completion_parent_end_waits_for_children_and_questions_without_badge_input() {
        let (mut tracker, key, target, mut own) = begun();
        own.children_working = true;
        assert!(tracker.observe(&target, &own, true, START + 4).is_empty());
        own.children_working = false;
        own.waiting = true;
        assert!(tracker.observe(&target, &own, true, START + 5).is_empty());
        own.waiting = false;
        assert_eq!(
            tracker.observe(&target, &own, true, START + 6)[0].state,
            LocalCompletionState::Completed
        );
        assert_eq!(tracker.state(&key), Some(LocalCompletionState::Completed));
    }

    #[test]
    fn completion_local_other_operation_or_missing_sequence_invalidates_watch() {
        for operation in [None, Some("other:operation"), Some("@internal:operation")] {
            let (mut tracker, key, target, own) = begun();
            let decisions =
                tracker.accepted_input(target.runtime, target.session, 12, operation, START + 3);
            assert_eq!(decisions[0].state, LocalCompletionState::Unknown);
            assert!(tracker.observe(&target, &own, true, START + 4).is_empty());
            assert_eq!(tracker.state(&key), Some(LocalCompletionState::Unknown));
        }
        let (key, target, baseline) = fixture();
        let mut tracker = Tracker::default();
        tracker
            .begin(
                key,
                target.clone(),
                &baseline,
                10,
                "@internal:operation".into(),
                storage::native_prompt_digest("own prompt"),
                START,
            )
            .unwrap();
        assert_eq!(
            tracker.accepted_input(
                target.runtime,
                target.session,
                12,
                Some("@internal:operation"),
                START + 1
            )[0]
            .state,
            LocalCompletionState::Unknown
        );
    }

    #[test]
    fn completion_actor_run_and_target_capacity_remain_separate_from_input_identity() {
        let (mut tracker, key, target, own) = begun();
        for (actor, run) in [
            ("other-actor", Some("run")),
            ("actor", Some("other-run")),
            ("actor", None),
        ] {
            let other = WatchKey {
                connection_id: actor.into(),
                operation_id: key.operation_id.clone(),
                run_id: run.map(str::to_owned),
            };
            assert!(
                tracker
                    .admit(&other, Err(runtime::PtyInputRejectReason::AdmissionDenied))
                    .is_none()
            );
            assert!(tracker.state(&other).is_none());
        }
        assert_eq!(
            tracker.begin(
                WatchKey {
                    operation_id: "overlap".into(),
                    ..key.clone()
                },
                target.clone(),
                &own,
                11,
                "other".into(),
                storage::native_prompt_digest("own prompt"),
                START
            ),
            Err("completion_target_busy")
        );
        assert_eq!(
            tracker.observe(&target, &own, true, START + 4)[0].state,
            LocalCompletionState::Completed
        );
    }

    #[test]
    fn completion_old_proof_unadmitted_input_and_expiry_cannot_complete() {
        let (key, target, baseline) = fixture();
        let mut tracker = Tracker::default();
        tracker
            .begin(
                key.clone(),
                target.clone(),
                &baseline,
                10,
                "@internal:operation".into(),
                storage::native_prompt_digest("own prompt"),
                START,
            )
            .unwrap();
        assert!(
            tracker
                .observe(&target, &baseline, true, START + 1)
                .is_empty()
        );
        assert_eq!(
            tracker.expire(START + START_TIMEOUT_MICROS)[0].state,
            LocalCompletionState::Expired
        );
        assert_eq!(tracker.state(&key), Some(LocalCompletionState::Expired));
        let (mut tracker, key, target, mut own) = begun();
        own.completed = None;
        assert!(tracker.observe(&target, &own, false, START + 4).is_empty());
        assert_eq!(
            tracker.expire(START + WATCH_TIMEOUT_MICROS)[0].state,
            LocalCompletionState::Expired
        );
        assert_eq!(tracker.state(&key), Some(LocalCompletionState::Expired));
    }

    #[test]
    fn completion_denied_unknown_and_revoked_have_single_distinct_terminal_result() {
        for (reason, expected) in [
            (
                runtime::PtyInputRejectReason::AdmissionDenied,
                LocalCompletionState::Rejected,
            ),
            (
                runtime::PtyInputRejectReason::AdmissionUnknown,
                LocalCompletionState::Unknown,
            ),
        ] {
            let (mut tracker, key, _, _) = begun();
            assert_eq!(tracker.admit(&key, Err(reason)).unwrap().state, expected);
            assert!(tracker.admit(&key, Err(reason)).is_none());
        }
        let (mut tracker, key, _, _) = begun();
        assert_eq!(
            tracker
                .cancel(&key, LocalCompletionState::Revoked, "permission_revoked")
                .unwrap()
                .state,
            LocalCompletionState::Revoked
        );
    }

    #[test]
    fn completion_unsupported_baseline_has_no_input_watch() {
        let (key, target, mut baseline) = fixture();
        baseline.start_execution_stamp.ancestors.clear();
        let mut tracker = Tracker::default();
        assert_eq!(
            tracker.begin(
                key,
                target,
                &baseline,
                0,
                "internal".into(),
                storage::native_prompt_digest("own prompt"),
                START
            ),
            Err("completion_native_proof_unsupported")
        );
        assert!(tracker.watch_keys().is_empty());
    }

    fn rollout_fixture() -> (NativeTurnProof, std::path::PathBuf) {
        let (_, _, mut proof) = fixture();
        proof.completed = Some(NativeTurnCompletionProof {
            at_micros: 3,
            execution_stamp: proof.start_execution_stamp.clone(),
        });
        let directory =
            std::env::temp_dir().join(format!("deppy-completion-rollout-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        proof.transcript_path = directory
            .join(format!("rollout-fixture-{}.jsonl", proof.native_session_id))
            .to_str()
            .unwrap()
            .to_owned();
        (proof, directory)
    }

    #[test]
    fn completion_codex_final_rollout_rejects_foreign_turn_errors_aborts_and_new_start() {
        let (proof, directory) = rollout_fixture();
        let header =
            serde_json::json!({"type":"session_meta","payload":{"id":proof.native_session_id}})
                .to_string();
        let complete = serde_json::json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":proof.turn_id}}).to_string();
        for (suffix, expected) in [
            (complete.clone(), true),
            (format!("{complete}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"next\"}}}}"), false),
            (format!("{complete}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"turn_aborted\"}}}}"), false),
            ("{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"foreign\"}}".into(), false),
            (format!("{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"{}\",\"error\":{{\"message\":\"failed\"}}}}}}", proof.turn_id), false),
        ] {
            std::fs::write(&proof.transcript_path, format!("{header}\n{suffix}\n")).unwrap();
            assert_eq!(confirm_codex_rollout(&proof), expected, "{suffix}");
        }
        std::fs::write(
            &proof.transcript_path,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"foreign\"}}}}\n{complete}\n"
            ),
        )
        .unwrap();
        assert!(!confirm_codex_rollout(&proof));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn completion_codex_rollout_refuses_symlink_and_fifo_without_blocking() {
        use std::os::unix::fs::symlink;
        let (proof, directory) = rollout_fixture();
        let target = directory.join("target");
        std::fs::write(&target, b"unused").unwrap();
        symlink(&target, &proof.transcript_path).unwrap();
        assert!(!confirm_codex_rollout(&proof));
        std::fs::remove_file(&proof.transcript_path).unwrap();
        let path = std::ffi::CString::new(proof.transcript_path.as_bytes()).unwrap();
        // SAFETY: path is a valid NUL-terminated path owned by this isolated fixture.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(!confirm_codex_rollout(&proof));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
