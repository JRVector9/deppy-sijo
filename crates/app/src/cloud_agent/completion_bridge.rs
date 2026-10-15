//! 원 요청의 완료 감시와 전달 상태. 파일·SQLite 작업은 기존 worker에 맡긴다.
use super::{
    CloudAgent, Target,
    completion::{self, Decision, LocalCompletionState, WatchKey, WatchTarget},
};
use crate::cloud_history_worker::{Job, ResultKind};
use agent_mcp::{
    CompletionDeliveryState, CompletionIdentity, CompletionPermit, CompletionReceipt,
    CompletionScope, Request, now,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::mpsc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CompletionStatusView {
    pub local: LocalCompletionState,
    pub delivery: CompletionDeliveryState,
}
struct Pending {
    identity: CompletionIdentity,
    permit: CompletionPermit,
    target: Target,
    watch_target: WatchTarget,
    reply: mpsc::SyncSender<Result<Value, String>>,
    created_at: u64,
    claimed: bool,
    admission: Option<Value>,
    decision: Option<Decision>,
    finishing: bool,
}
struct Status {
    identity: CompletionIdentity,
    permit: CompletionPermit,
    view: CompletionStatusView,
    at: u64,
}
#[derive(Default)]
pub(super) struct CompletionBridge {
    tracker: completion::Tracker,
    native: HashMap<String, storage::NativeTurnProof>,
    sequences: HashMap<(u64, runtime::SessionId), u64>,
    pending: HashMap<String, Pending>,
    finishes: HashMap<u64, (String, CompletionReceipt)>,
    statuses: VecDeque<Status>,
    last_projection: Option<std::time::Instant>,
}
fn micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|v| i64::try_from(v.as_micros()).ok())
        .unwrap_or(0)
}
fn key(identity: &CompletionIdentity) -> WatchKey {
    WatchKey {
        connection_id: identity.connection_id.clone(),
        operation_id: identity.scope.operation_id.clone(),
        run_id: identity.scope.run_id.clone(),
    }
}
fn same_target(a: &Target, b: &Target) -> bool {
    b.live
        && a.id == b.id
        && a.generation == b.generation
        && a.workspace == b.workspace
        && a.runtime == b.runtime
        && a.runtime_token == b.runtime_token
        && a.session == b.session
        && a.pane == b.pane
        && a.execution == b.execution
        && a.provider == b.provider
}
impl CloudAgent {
    pub(crate) fn invalidate_completion_runtime(&mut self, _runtime: u64) {}
    pub(crate) fn set_completion_observation_ready(&mut self, _ready: bool) {}
    pub(crate) fn completion_projection_due(&mut self) -> bool {
        if !self.has_completion_watches()
            || self
                .completion_bridge
                .last_projection
                .is_some_and(|last| last.elapsed() < std::time::Duration::from_millis(250))
        {
            return false;
        }
        self.completion_bridge.last_projection = Some(std::time::Instant::now());
        true
    }
    pub(super) fn completion_input_permit(&self, internal: &str) -> Option<CompletionPermit> {
        self.completion_bridge
            .pending
            .get(internal)
            .map(|p| p.permit.clone())
    }
    pub(crate) fn native_turn_keys(&self) -> Vec<String> {
        self.targets
            .iter()
            .filter(|t| t.live)
            .take(256)
            .map(|t| format!("{}:{}", t.workspace, t.session.0))
            .collect()
    }
    pub(crate) fn completion_watch_keys(&self) -> Vec<String> {
        self.completion_bridge
            .pending
            .values()
            .take(64)
            .map(|p| format!("{}:{}", p.target.workspace, p.target.session.0))
            .collect()
    }
    pub(crate) fn install_native_turns(&mut self, proofs: Vec<storage::NativeTurnProof>) {
        self.completion_bridge.native = proofs
            .into_iter()
            .take(256)
            .map(|p| (p.session_key.clone(), p))
            .collect();
        self.poll_completions();
    }
    pub(crate) fn has_completion_watches(&self) -> bool {
        !self.completion_bridge.pending.is_empty()
    }
    pub(super) fn reserve_completion(
        &mut self,
        req: &Request,
        target: &Target,
        context: &agent_mcp::OperationContext,
        text: &str,
    ) -> Result<CompletionIdentity, String> {
        let transport = req
            .deferred
            .as_ref()
            .ok_or("completion_transport_required_no_effect")?;
        if text.is_empty() || target.provider != Some(crate::agent_surface::AgentProvider::Codex) {
            return Err("completion_native_proof_unsupported".into());
        }
        let execution = target
            .execution
            .filter(|e| e.is_current())
            .ok_or("completion_native_proof_unsupported")?;
        let baseline = self
            .completion_bridge
            .native
            .get(&format!("{}:{}", target.workspace, target.session.0))
            .ok_or("completion_native_proof_unsupported")?;
        let sequence = *self
            .completion_bridge
            .sequences
            .get(&(target.runtime, target.session))
            .ok_or("completion_native_proof_unsupported")?;
        let watch_target = WatchTarget {
            workspace: target.workspace.clone(),
            session_id: target.id.clone(),
            generation: target.generation.clone(),
            runtime: target.runtime,
            session: target.session,
            execution,
            runtime_token: target.runtime_token.clone(),
            native_session_id: baseline.native_session_id.clone(),
        };
        let watch_key = WatchKey {
            connection_id: context.connection_id.clone(),
            operation_id: context.operation_id.clone(),
            run_id: context.run_id.clone(),
        };
        let internal = agent_mcp::operation_key(&context.connection_id, &context.operation_id)
            .map_err(|_| "completion_invalid_scope")?;
        self.completion_bridge
            .tracker
            .begin(
                watch_key.clone(),
                watch_target.clone(),
                baseline,
                sequence,
                internal.clone(),
                storage::native_prompt_digest(text),
                micros(),
            )
            .map_err(str::to_owned)?;
        let permit = CompletionPermit::new();
        let scope = CompletionScope {
            workspace: target.workspace.clone(),
            session_id: target.id.clone(),
            generation: target.generation.clone(),
            operation_id: context.operation_id.clone(),
            run_id: context.run_id.clone(),
        };
        let identity = match transport.reserve(scope, permit.clone()) {
            Ok(identity) => identity,
            Err(error) => {
                self.completion_bridge.tracker.discard_unclaimed(&watch_key);
                return Err(error);
            }
        };
        self.completion_bridge.statuses.push_back(Status {
            identity: identity.clone(),
            permit: permit.clone(),
            view: CompletionStatusView {
                local: LocalCompletionState::AwaitingStart,
                delivery: CompletionDeliveryState::Waiting,
            },
            at: now(),
        });
        while self.completion_bridge.statuses.len() > 1000 {
            if let Some(old) = self.completion_bridge.statuses.pop_front() {
                old.permit.revoke();
            }
        }
        self.completion_bridge.pending.insert(
            internal,
            Pending {
                identity: identity.clone(),
                permit,
                target: target.clone(),
                watch_target,
                reply: req.reply.clone(),
                created_at: now(),
                claimed: false,
                admission: None,
                decision: None,
                finishing: false,
            },
        );
        Ok(identity)
    }
    pub(super) fn discard_completion(&mut self, internal: &str) {
        if let Some(p) = self.completion_bridge.pending.remove(internal) {
            self.completion_bridge
                .tracker
                .discard_unclaimed(&key(&p.identity));
            self.completion_bridge
                .statuses
                .retain(|s| s.identity != p.identity);
            // 원 오류 응답이 전송되도록 permit은 반환 전에 취소하지 않는다.
        }
    }
    pub(super) fn completion_claimed(&mut self, internal: &str) {
        if let Some(p) = self.completion_bridge.pending.get_mut(internal) {
            p.claimed = true;
        }
    }
    pub(super) fn completion_can_dispatch(&self, internal: &str) -> bool {
        self.completion_bridge
            .pending
            .get(internal)
            .is_some_and(|p| p.permit.is_live() && p.decision.is_none())
    }
    pub(super) fn record_completion_admission(
        &mut self,
        internal: &str,
        admission: Value,
        result: Result<(), runtime::PtyInputRejectReason>,
    ) {
        let Some(p) = self.completion_bridge.pending.get_mut(internal) else {
            return;
        };
        if p.admission.is_some() {
            return;
        }
        p.admission = Some(admission);
        let watch_key = key(&p.identity);
        if let Some(decision) = self.completion_bridge.tracker.admit(&watch_key, result) {
            self.record_completion_decisions(vec![decision]);
        }
        self.poll_completions();
    }
    fn record_completion_decisions(&mut self, decisions: Vec<Decision>) {
        for decision in decisions {
            let Ok(internal) =
                agent_mcp::operation_key(&decision.key.connection_id, &decision.key.operation_id)
            else {
                continue;
            };
            if let Some(p) = self.completion_bridge.pending.get_mut(&internal) {
                p.decision = Some(decision);
            }
        }
    }
    pub(super) fn observe_completion_event(&mut self, runtime: u64, event: &runtime::RuntimeEvent) {
        if let runtime::RuntimeEvent::SessionInputAccepted {
            session,
            sequence,
            operation_id,
            at_micros,
        } = event
        {
            let bridge = &mut self.completion_bridge;
            if !bridge.sequences.contains_key(&(runtime, *session)) && bridge.sequences.len() >= 256
            {
                bridge.sequences.clear();
            }
            bridge
                .sequences
                .entry((runtime, *session))
                .and_modify(|old| *old = (*old).max(*sequence))
                .or_insert(*sequence);
            let decisions = bridge.tracker.accepted_input(
                runtime,
                *session,
                *sequence,
                operation_id.as_deref(),
                *at_micros,
            );
            self.record_completion_decisions(decisions);
        }
    }
    pub(super) fn revoke_completions(&mut self, connection: Option<&str>, session: Option<&str>) {
        let identities: Vec<_> = self
            .completion_bridge
            .statuses
            .iter()
            .filter(|s| {
                connection.is_none_or(|c| s.identity.connection_id == c)
                    && session.is_none_or(|id| s.identity.scope.session_id == id)
            })
            .map(|s| {
                s.permit.revoke();
                s.identity.clone()
            })
            .collect();
        for identity in identities {
            if let Some(decision) = self.completion_bridge.tracker.cancel(
                &key(&identity),
                LocalCompletionState::Revoked,
                "completion_read_revoked",
            ) {
                self.record_completion_decisions(vec![decision]);
            }
        }
    }
    pub(crate) fn poll_completions(&mut self) {
        if let Some(server) = &self.server {
            for _ in 0..128 {
                let Ok(event) = server.delivery_events.try_recv() else {
                    break;
                };
                if let Some(status) = self
                    .completion_bridge
                    .statuses
                    .iter_mut()
                    .find(|s| s.identity == event.identity)
                {
                    status.view.delivery = event.state;
                }
            }
        }
        let invalid: Vec<_> = self
            .completion_bridge
            .pending
            .iter()
            .filter(|(_, p)| {
                !p.permit.is_live()
                    || !self.targets.iter().any(|t| same_target(&p.target, t))
                    || !self
                        .connection_permission(&p.identity.connection_id, &p.target)
                        .local_completion
                    || !self.connection_infos().iter().any(|c| {
                        c.principal.id == p.identity.connection_id
                            && c.state == agent_mcp::ConnectionState::Active
                    })
                    || !p.watch_target.execution.is_current()
            })
            .map(|(_, p)| (key(&p.identity), p.permit.clone()))
            .collect();
        for (key, permit) in invalid {
            permit.revoke();
            if let Some(d) = self.completion_bridge.tracker.cancel(
                &key,
                LocalCompletionState::Revoked,
                "completion_identity_revoked",
            ) {
                self.record_completion_decisions(vec![d]);
            }
        }
        let observed: Vec<_> = self
            .completion_bridge
            .pending
            .values()
            .filter_map(|p| {
                self.completion_bridge
                    .native
                    .get(&format!("{}:{}", p.target.workspace, p.target.session.0))
                    .map(|proof| (p.watch_target.clone(), proof.clone()))
            })
            .collect();
        for (target, proof) in observed {
            let decisions = self.completion_bridge.tracker.observe(
                &target,
                &proof,
                proof.completed.is_some(),
                micros(),
            );
            self.record_completion_decisions(decisions);
        }
        let expired = self.completion_bridge.tracker.expire(micros());
        self.record_completion_decisions(expired);
        for p in self.completion_bridge.pending.values() {
            if let Some(status) = self
                .completion_bridge
                .statuses
                .iter_mut()
                .find(|s| s.identity == p.identity)
            {
                status.view.local = p
                    .decision
                    .as_ref()
                    .map(|d| d.state)
                    .or_else(|| self.completion_bridge.tracker.state(&key(&p.identity)))
                    .unwrap_or(LocalCompletionState::Unknown);
                if !p.permit.is_live() {
                    status.view.delivery = CompletionDeliveryState::Closed;
                }
            }
        }
        let ready: Vec<_> = self
            .completion_bridge
            .pending
            .iter()
            .filter(|(_, p)| {
                p.claimed && p.admission.is_some() && p.decision.is_some() && !p.finishing
            })
            .map(|(id, _)| id.clone())
            .collect();
        for internal in ready {
            let p = &self.completion_bridge.pending[&internal];
            let decision = p.decision.as_ref().expect("ready decision");
            let completed = decision.state == LocalCompletionState::Completed;
            let outcome = if completed {
                json!({"status":"completed","local_turn_completed":true,"source":"verified_native_turn"})
            } else {
                json!({"status":if decision.state == LocalCompletionState::Rejected {"rejected"} else {"unknown"}, "reason":decision.reason})
            };
            let receipt = CompletionReceipt {
                identity: p.identity.clone(),
                event_id: p.identity.event_id(),
                admission: p.admission.clone().expect("ready admission"),
                outcome,
                created_at: p.created_at,
                completed_at: completed.then_some(now()),
                expires_at: now() + agent_mcp::COMPLETION_RECEIPT_TTL,
            };
            let token = self.operation_token();
            self.completion_bridge
                .pending
                .get_mut(&internal)
                .expect("ready pending")
                .finishing = true;
            self.completion_bridge
                .finishes
                .insert(token, (internal, receipt.clone()));
            self.finish_queue.push_back((
                token,
                Job::FinishCompletion {
                    identity: receipt.identity.clone(),
                    receipt,
                },
            ));
        }
        self.completion_bridge.statuses.retain(|s| {
            if now().saturating_sub(s.at) >= agent_mcp::COMPLETION_RECEIPT_TTL {
                s.permit.revoke();
                false
            } else {
                true
            }
        });
    }
    pub(super) fn finish_completion_result(&mut self, token: u64, result: ResultKind) {
        let Some((internal, receipt)) = self.completion_bridge.finishes.remove(&token) else {
            return;
        };
        let Some(p) = self.completion_bridge.pending.remove(&internal) else {
            return;
        };
        let ok = matches!(result, ResultKind::FinishCompletion(Ok(())));
        let response = if ok {
            Ok(receipt.tool_result())
        } else {
            Err("outcome_unknown_do_not_retry_input".into())
        };
        if let Some(status) = self
            .completion_bridge
            .statuses
            .iter_mut()
            .find(|s| s.identity == p.identity)
        {
            status.view.delivery = if p.permit.is_live() {
                CompletionDeliveryState::Ready
            } else {
                CompletionDeliveryState::Closed
            };
        }
        let _ = p.reply.try_send(response);
    }
    pub(super) fn completion_status_for_target(
        &self,
        connection: &str,
        target: &Target,
    ) -> Option<CompletionStatusView> {
        self.completion_bridge
            .statuses
            .iter()
            .rev()
            .find(|s| {
                s.identity.connection_id == connection
                    && s.identity.scope.session_id == target.id
                    && s.identity.scope.generation == target.generation
            })
            .map(|s| s.view)
    }
    pub(super) fn completion_status_for_operation(
        &self,
        connection: &str,
        session: &str,
        generation: &str,
        operation: &str,
    ) -> Option<CompletionStatusView> {
        self.completion_bridge
            .statuses
            .iter()
            .rev()
            .find(|s| {
                s.identity.connection_id == connection
                    && s.identity.scope.session_id == session
                    && s.identity.scope.generation == generation
                    && s.identity.scope.operation_id == operation
            })
            .map(|s| s.view)
    }
    pub(super) fn completion_status_for_run(
        &self,
        connection: &str,
        session: &str,
        generation: &str,
        run: &str,
    ) -> Option<CompletionStatusView> {
        self.completion_bridge
            .statuses
            .iter()
            .rev()
            .find(|s| {
                s.identity.connection_id == connection
                    && s.identity.scope.session_id == session
                    && s.identity.scope.generation == generation
                    && s.identity.scope.run_id.as_deref() == Some(run)
            })
            .map(|s| s.view)
    }
}
