//! Native approval, asynchronous evidence, and exact-result application are separate gates.
use super::*;
use wm::verification as approval;
use wm::verification_worker::{VerificationJob, VerificationResult, VerificationWorker};

struct Pending {
    operation: String,
    route: cc::RouteTarget,
    evidence: work::CommandVerificationEvidence,
    config: work::ConfigRevision,
    binding: Option<work::RuntimeBinding>,
    cancelled: bool,
    folder: String,
}
struct Manual {
    operation: String,
    route: cc::RouteTarget,
    evidence: work::ManualVerificationEvidence,
    config: work::ConfigRevision,
    binding: Option<work::RuntimeBinding>,
}
#[derive(Default)]
pub(super) struct Verifications {
    worker: Option<VerificationWorker>,
    pending: Option<Pending>,
    manual: Option<Manual>,
    loading: Option<(String, u64)>,
    loaded: BTreeSet<String>,
    failed_loads: BTreeSet<String>,
    evidence: VecDeque<work::CommandVerificationEvidence>,
}
impl Verifications {
    pub(super) fn retry_load(&mut self, workspace: &str) {
        self.loaded.remove(workspace);
        self.failed_loads.remove(workspace);
    }
    pub(super) fn busy(&self) -> bool {
        self.pending.is_some() || self.manual.is_some() || self.loading.is_some()
    }
    pub(super) fn owns(&self, workspace: &str) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|p| p.route.workspace_id == workspace)
            || self
                .manual
                .as_ref()
                .is_some_and(|p| p.route.workspace_id == workspace)
            || self.loading.as_ref().is_some_and(|(ws, _)| ws == workspace)
    }
    fn remember(&mut self, evidence: work::CommandVerificationEvidence) {
        self.evidence.retain(|old| old.id != evidence.id);
        self.evidence.push_back(evidence);
        while self.evidence.len() > 128
            || self
                .evidence
                .iter()
                .map(|e| {
                    e.stdout.len() + e.stderr.len() + e.spec.command.len() + e.spec.cwd.len() + 2048
                })
                .sum::<usize>()
                > 8 * 1024 * 1024
        {
            self.evidence.pop_front();
        }
    }
    pub(super) fn project(
        &self,
        task: &mut cc::Task,
        project: &Project,
        ready: bool,
        redaction: &secret::RedactionService,
    ) {
        let Some(target) = result_target(project, &task.target) else {
            return;
        };
        task.verification_target = Some(target.clone());
        task.command_verification = self
            .pending
            .as_ref()
            .filter(|p| p.evidence.target == target)
            .map(|p| p.evidence.clone())
            .or_else(|| {
                self.evidence
                    .iter()
                    .rev()
                    .find(|e| e.target == target)
                    .cloned()
            });
        if let Some(evidence) = task.command_verification.as_mut() {
            evidence.spec.command = bounded(
                &redaction.redact_wrapped_text(&evidence.spec.command),
                work::VERIFICATION_COMMAND_MAX_BYTES,
            );
            evidence.spec.cwd = bounded(
                &redaction.redact_wrapped_text(&evidence.spec.cwd),
                work::WORK_FOLDER_MAX_BYTES,
            );
            evidence.stdout = bounded(
                &redaction.redact_wrapped_text(&evidence.stdout),
                evidence.spec.output_limit_bytes as usize,
            );
            evidence.stderr = bounded(
                &redaction.redact_wrapped_text(&evidence.stderr),
                evidence.spec.output_limit_bytes as usize,
            );
            evidence.error = evidence.error.as_ref().map(|text| {
                bounded(
                    &redaction.redact_wrapped_text(text),
                    work::WORK_EVENT_MAX_BYTES,
                )
            });
        }
        let local = project
            .snapshot
            .tasks
            .iter()
            .find(|t| t.id == target.task_id)
            .is_some_and(|t| t.execution_device.is_none());
        let verifiable = project
            .snapshot
            .tasks
            .iter()
            .find(|t| t.id == target.task_id)
            .is_some_and(|t| {
                matches!(
                    t.state,
                    work::WorkState::ResultSubmitted
                        | work::WorkState::VerificationFailed
                        | work::WorkState::AwaitingUserConfirmation
                )
            });
        task.execute_verification = if ready && local && verifiable && !self.busy() {
            yes()
        } else {
            no("현재 로컬 결과를 확인하고 진행 중인 검증·저장을 마친 뒤 실행해 주세요.")
        };
        task.cancel_verification = if self
            .pending
            .as_ref()
            .is_some_and(|p| p.evidence.target == target && !p.cancelled)
        {
            yes()
        } else {
            no("취소할 로컬 검증 실행이 없습니다.")
        };
    }
}
fn result_target(project: &Project, route: &cc::RouteTarget) -> Option<work::VerificationTarget> {
    let task = project
        .snapshot
        .tasks
        .iter()
        .find(|t| Some(&t.id.0) == route.task_id.as_ref())?;
    if !matches!(
        task.state,
        work::WorkState::ResultSubmitted
            | work::WorkState::VerificationFailed
            | work::WorkState::AwaitingUserConfirmation
            | work::WorkState::Completed
    ) {
        return None;
    }
    let result = task.result.as_ref()?;
    Some(work::VerificationTarget {
        workspace_id: task.workspace_id.clone(),
        task_id: task.id.clone(),
        attempt_id: task.current_attempt_id.clone()?,
        result_revision: result.revision,
        source_revision: result.source_revision.clone(),
        artifact_revision: result.artifact_revision.clone(),
    })
}
fn exact(project: &Project, pending: &Pending) -> bool {
    project.loaded
        && !project.storage_blocked
        && project.snapshot.config.revision == pending.config
        && approval::matches_current(&project.state(), &pending.evidence)
        && project.snapshot.attempts.iter().any(|a| {
            a.id == pending.evidence.target.attempt_id
                && a.runtime_binding == pending.binding
                && a.folder == pending.folder
        })
}
fn preparation_current(
    project: &Project,
    pending: &Pending,
    requested: &work::CommandVerificationEvidence,
    approved: &approval::ApprovedVerification,
) -> bool {
    pending.evidence == *requested
        && !pending.cancelled
        && exact(project, pending)
        && approved.evidence() == requested
}
fn review_verification(
    project: &Project,
    expected: &work::VerificationTarget,
    spec: work::VerificationSpec,
) -> Result<work::CommandVerificationEvidence, String> {
    let requested = work::CommandVerificationEvidence {
        id: work::VerificationId::new(),
        target: expected.clone(),
        spec,
        started_at_ms: now_ms(),
        ended_at_ms: None,
        outcome: work::CommandVerificationOutcome::Running,
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        error: None,
    };
    // This gate is deliberately pure: the worker resolves and pins the reviewed directory.
    if !approval::matches_current(&project.state(), &requested) {
        return Err("현재 로컬 결과와 검증 명령을 다시 확인해 주세요.".into());
    }
    Ok(requested)
}
fn same_manual_request(
    a: &work::ManualVerificationEvidence,
    b: &work::ManualVerificationEvidence,
) -> bool {
    a.id == b.id
        && a.target == b.target
        && a.recorded_at_ms == b.recorded_at_ms
        && a.passed == b.passed
}
fn take_manual_request(
    host: &mut CommandCenterHost,
    evidence: &work::ManualVerificationEvidence,
) -> Option<Manual> {
    let pending = host.verifications.manual.take()?;
    if !same_manual_request(&pending.evidence, evidence) {
        host.reject(
            &pending.operation,
            "수동 검증 응답의 식별자가 달라 현재 상태에 적용하지 않았습니다.",
        );
        return None;
    }
    Some(pending)
}
fn apply_evidence(
    project: &mut Project,
    pending: &Pending,
    evidence: &work::CommandVerificationEvidence,
    cancelled: bool,
) -> Result<bool, String> {
    if cancelled
        || pending.cancelled
        || pending.evidence.id != evidence.id
        || pending.evidence.target != evidence.target
        || pending.evidence.spec != evidence.spec
        || !exact(project, pending)
    {
        return Ok(false);
    }
    let Some(outcome) = approval::result_state(evidence) else {
        return Ok(false);
    };
    apply_user_work_event(
        project,
        &pending.route,
        &evidence.target.attempt_id.0,
        work::WorkEventKind::VerificationFinished {
            result_revision: evidence.target.result_revision,
            outcome,
        },
        format!(
            "앱 로컬 검증 · {:?} · 증거 {} · 종료 코드 {:?}",
            evidence.outcome, evidence.id.0, evidence.exit_code
        ),
    )?;
    Ok(true)
}
impl App {
    fn ensure_command_center_verification(&self, host: &mut CommandCenterHost) {
        if host.verifications.worker.is_none() {
            let ctx = self.egui_ctx.clone();
            host.verifications.worker = Some(VerificationWorker::new(
                self.data_dir().to_path_buf(),
                self.command_center_redaction.clone(),
                move || ctx.request_repaint(),
            ));
        }
    }
    pub(super) fn begin_command_center_verification(
        &self,
        host: &mut CommandCenterHost,
        id: &str,
        route: cc::RouteTarget,
        expected: work::VerificationTarget,
        spec: work::VerificationSpec,
    ) -> Result<(), String> {
        if host.remote_permits.contains_key(id) {
            return Err("검증 명령은 이 컴퓨터에서 직접 확인한 뒤 실행해 주세요.".into());
        }
        if host.verifications.busy()
            || host.flight.is_some()
            || remote::handoff_pending(host, &route)
        {
            return Err("진행 중인 검증·입력·저장을 마친 뒤 실행해 주세요.".into());
        }
        let p = host
            .projects
            .get(&route.workspace_id)
            .filter(|p| p.loaded && !p.storage_blocked && p.generation == p.saved_generation)
            .ok_or("현재 프로젝트의 저장 상태를 먼저 확인해 주세요.")?;
        if expected.workspace_id.0 != route.workspace_id
            || Some(&expected.task_id.0) != route.task_id.as_ref()
            || result_target(p, &route).as_ref() != Some(&expected)
        {
            return Err("검증할 결과·시도·소스가 바뀌었습니다. 다시 확인해 주세요.".into());
        }
        let requested = review_verification(p, &expected, spec)?;
        if !wm::verification_worker::safe_metadata(
            &self.command_center_redaction,
            &requested.id,
            &requested.target,
            Some(&requested.spec),
        ) {
            return Err(
                "검증 명령·경로·식별자에 등록된 비밀값이 포함되어 실행하지 않았습니다.".into(),
            );
        }
        let pending = Pending {
            operation: id.into(),
            route,
            evidence: requested.clone(),
            config: p.snapshot.config.revision,
            binding: p
                .snapshot
                .attempts
                .iter()
                .find(|a| a.id == expected.attempt_id)
                .and_then(|a| a.runtime_binding.clone()),
            cancelled: false,
            folder: p
                .snapshot
                .attempts
                .iter()
                .find(|a| a.id == expected.attempt_id)
                .map(|a| a.folder.clone())
                .unwrap_or_default(),
        };
        let preparation = VerificationJob::prepare(p.state(), requested);
        self.ensure_command_center_verification(host);
        host.verifications
            .worker
            .as_mut()
            .unwrap()
            .try_request(preparation)
            .map_err(|_| "검증 실행기가 사용 중입니다. 명령을 실행하지 않았습니다.".to_owned())?;
        host.verifications.pending = Some(pending);
        host.publication_dirty = true;
        host.status = "확인한 로컬 명령의 실행·증거 저장을 기다립니다.".into();
        Ok(())
    }
    pub(super) fn cancel_command_center_verification(
        &self,
        host: &mut CommandCenterHost,
        id: &str,
        route: &cc::RouteTarget,
        expected: &work::VerificationTarget,
        verification_id: &work::VerificationId,
    ) -> Result<(), String> {
        if host.remote_permits.contains_key(id) {
            return Err("로컬 검증 실행은 이 컴퓨터에서 취소해 주세요.".into());
        }
        let pending = host
            .verifications
            .pending
            .as_mut()
            .filter(|p| {
                p.route.workspace_id == route.workspace_id
                    && p.route.task_id == route.task_id
                    && p.evidence.target == *expected
                    && p.evidence.id == *verification_id
            })
            .ok_or("해당 검증 실행이 바뀌었거나 이미 종료되었습니다.")?;
        if !host
            .verifications
            .worker
            .as_mut()
            .is_some_and(|w| w.cancel(verification_id))
        {
            return Err("현재 검증 실행의 취소를 확인하지 못했습니다.".into());
        }
        pending.cancelled = true;
        host.finish(
            id,
            cc::ReceiptStatus::Acknowledged,
            "검증 취소를 요청했습니다. 실행 종료와 증거 저장을 기다립니다.",
        );
        Ok(())
    }
    pub(super) fn begin_command_center_manual_verification(
        &self,
        host: &mut CommandCenterHost,
        id: &str,
        route: cc::RouteTarget,
        attempt: &str,
        review: (&str, bool, &str),
    ) -> Result<(), String> {
        let (revision, passed, text) = review;
        if host.verifications.busy()
            || host.flight.is_some()
            || remote::handoff_pending(host, &route)
        {
            return Err("진행 중인 검증·입력·저장을 마친 뒤 기록해 주세요.".into());
        }
        if text.trim().is_empty() || text.len() > 4096 {
            return Err("직접 수행한 검증의 명령과 결과를 4KiB 이하로 기록해 주세요.".into());
        }
        let p = host
            .projects
            .get(&route.workspace_id)
            .filter(|p| p.loaded && !p.storage_blocked)
            .ok_or("현재 프로젝트가 준비되지 않았습니다.")?;
        let target = result_target(p, &route)
            .filter(|t| t.attempt_id.0 == attempt && t.result_revision.0.to_string() == revision)
            .ok_or("현재 결과 리비전과 시도를 다시 확인해 주세요.")?;
        let evidence = work::ManualVerificationEvidence {
            id: work::VerificationId::new(),
            target,
            recorded_at_ms: now_ms(),
            passed,
            evidence: self.command_center_redaction.redact_wrapped_text(text),
        };
        work::VerificationEvidence::UserManual(evidence.clone())
            .validate()
            .map_err(|e| e.to_string())?;
        let pending = Manual {
            operation: id.into(),
            route,
            evidence: evidence.clone(),
            config: p.snapshot.config.revision,
            binding: p
                .snapshot
                .attempts
                .iter()
                .find(|a| a.id == evidence.target.attempt_id)
                .and_then(|a| a.runtime_binding.clone()),
        };
        self.ensure_command_center_verification(host);
        host.verifications
            .worker
            .as_mut()
            .unwrap()
            .try_request(VerificationJob::RecordManual { evidence })
            .map_err(|_| "검증 기록 저장기가 사용 중입니다.".to_owned())?;
        host.verifications.manual = Some(pending);
        host.publication_dirty = true;
        Ok(())
    }
    pub(super) fn pump_command_center_verification(&self, host: &mut CommandCenterHost) {
        if let Some(pending) = host.verifications.pending.as_ref()
            && !host
                .projects
                .get(&pending.route.workspace_id)
                .is_some_and(|p| exact(p, pending))
        {
            if let Some(worker) = host.verifications.worker.as_mut() {
                worker.cancel(&pending.evidence.id);
            }
            host.verifications.pending.as_mut().unwrap().cancelled = true;
        }
        let result = host
            .verifications
            .worker
            .as_mut()
            .and_then(|w| w.try_recv());
        if let Some(result) = result {
            match result {
                Ok(VerificationResult::Prepared {
                    requested,
                    approved,
                }) => {
                    if let Some(pending) = host.verifications.pending.take() {
                        match approved {
                            Ok(approved) if host.projects.get(&pending.route.workspace_id).is_some_and(|p| preparation_current(p, &pending, &requested, &approved)) => {
                                match host.verifications.worker.as_mut().unwrap().try_request(VerificationJob::Run(*approved)) {
                                    Ok(()) => host.verifications.pending = Some(pending),
                                    Err(_) => host.reject(&pending.operation, "검증 실행기를 준비하지 못해 명령을 실행하지 않았습니다."),
                                }
                            }
                            Ok(_) | Err(wm::verification_worker::VerificationError::Cancelled) => {
                                host.finish(&pending.operation, cc::ReceiptStatus::Acknowledged, "검증 준비가 취소되었거나 대상이 바뀌어 명령을 실행하지 않았습니다.");
                            }
                            Err(error) => host.reject(&pending.operation, format!("로컬 검증 준비를 완료하지 못해 명령을 실행하지 않았습니다 ({error:?}).")),
                        }
                    }
                }
                Ok(VerificationResult::Finished {
                    evidence,
                    cancelled,
                }) => {
                    let pending = host.verifications.pending.take();
                    host.verifications.remember((*evidence).clone());
                    if let Some(pending) = pending.filter(|p| {
                        p.evidence.id == evidence.id
                            && p.evidence.target == evidence.target
                            && p.evidence.spec == evidence.spec
                    }) {
                        let applied = host
                            .projects
                            .get_mut(&pending.route.workspace_id)
                            .map(|p| apply_evidence(p, &pending, &evidence, cancelled))
                            .unwrap_or(Ok(false));
                        if applied == Ok(true) {
                            if let Err(error) = host.save(
                                &pending.route.workspace_id,
                                None,
                                Some(pending.operation.clone()),
                            ) {
                                host.finish(
                                    &pending.operation,
                                    cc::ReceiptStatus::Uncertain,
                                    error,
                                );
                            }
                        } else if applied.is_err() {
                            host.finish(
                                &pending.operation,
                                cc::ReceiptStatus::Uncertain,
                                "검증 증거는 저장했지만 현재 작업 상태에 반영하지 못했습니다.",
                            );
                        } else {
                            host.finish(&pending.operation,cc::ReceiptStatus::Acknowledged,"검증 증거를 저장했습니다. 취소·중단·변경된 결과에는 검증 상태를 적용하지 않았습니다.");
                        }
                    }
                }
                Ok(VerificationResult::Loaded {
                    workspace_id,
                    token,
                    result,
                }) => {
                    if host.verifications.loading.as_ref() == Some(&(workspace_id.0.clone(), token))
                    {
                        host.verifications.loading = None;
                        match result {
                            Ok(records) => {
                                host.verifications.loaded.insert(workspace_id.0.clone());
                                for record in records {
                                    if let work::VerificationEvidence::AppLocal(evidence) = record {
                                        host.verifications.remember(evidence);
                                    }
                                }
                            }
                            Err(_) => {
                                host.verifications.failed_loads.insert(workspace_id.0);
                                host.status="검증 이력을 불러오지 못했습니다. 프로젝트를 다시 불러와 복구할 수 있습니다. 명령은 다시 실행하지 않습니다.".into();
                            }
                        }
                    }
                }
                Ok(VerificationResult::ManualRecorded { evidence, result }) => {
                    if let Some(pending) = take_manual_request(host, &evidence) {
                        let permitted = host
                            .remote_permits
                            .get(&pending.operation)
                            .is_none_or(|permit| permit.is_current());
                        let current = permitted
                            && host
                                .projects
                                .get(&pending.route.workspace_id)
                                .is_some_and(|p| {
                                    p.loaded
                                        && !p.storage_blocked
                                        && p.snapshot.config.revision == pending.config
                                        && result_target(p, &pending.route).as_ref()
                                            == Some(&evidence.target)
                                        && p.snapshot.attempts.iter().any(|a| {
                                            a.id == evidence.target.attempt_id
                                                && a.runtime_binding == pending.binding
                                        })
                                });
                        if result.is_ok() && current {
                            let p = host.projects.get_mut(&pending.route.workspace_id).unwrap();
                            let applied = apply_user_work_event(
                                p,
                                &pending.route,
                                &evidence.target.attempt_id.0,
                                work::WorkEventKind::VerificationFinished {
                                    result_revision: evidence.target.result_revision,
                                    outcome: if evidence.passed {
                                        work::VerificationState::Passed
                                    } else {
                                        work::VerificationState::Failed
                                    },
                                },
                                format!("사용자 직접 검증 · 앱 실행 아님\n{}", evidence.evidence),
                            );
                            if applied.is_ok() {
                                if let Err(e) = host.save(
                                    &pending.route.workspace_id,
                                    None,
                                    Some(pending.operation.clone()),
                                ) {
                                    host.finish(
                                        &pending.operation,
                                        cc::ReceiptStatus::Uncertain,
                                        e,
                                    );
                                }
                            } else {
                                host.reject(&pending.operation,"수동 검증 증거는 저장했지만 현재 상태에는 적용하지 않았습니다.");
                            }
                        } else {
                            host.reject(&pending.operation,"수동 검증 저장 실패 또는 대상 변경으로 현재 상태를 바꾸지 않았습니다.");
                        }
                    }
                }
                Ok(VerificationResult::ExecutionFailed { id, target, error }) => {
                    if let Some(p) = host
                        .verifications
                        .pending
                        .take()
                        .filter(|p| p.evidence.id == id && p.evidence.target == target)
                    {
                        host.reject(&p.operation,format!("검증 실행 또는 증거 저장을 완료하지 못했습니다 ({error:?}). 통과 처리하지 않았습니다."));
                    }
                }
                Err(_) => {
                    if let Some(p) = host.verifications.pending.take() {
                        host.finish(
                            &p.operation,
                            cc::ReceiptStatus::Uncertain,
                            "검증 실행기의 응답을 확인할 수 없습니다. 자동 재실행하지 않습니다.",
                        );
                    }
                    if let Some(p) = host.verifications.manual.take() {
                        host.reject(
                            &p.operation,
                            "수동 검증 기록 저장 결과를 확인할 수 없습니다.",
                        );
                    }
                    host.verifications.loading = None;
                }
            }
            host.publication_dirty = true;
        }
        if !host.verifications.busy() {
            let next = host
                .projects
                .iter()
                .find(|(ws, p)| {
                    p.loaded
                        && !host.verifications.loaded.contains(*ws)
                        && !host.verifications.failed_loads.contains(*ws)
                })
                .map(|(ws, _)| ws.clone());
            if let Some(workspace) = next {
                self.ensure_command_center_verification(host);
                host.next_token = host.next_token.saturating_add(1);
                let token = host.next_token;
                if host
                    .verifications
                    .worker
                    .as_mut()
                    .unwrap()
                    .try_request(VerificationJob::Load {
                        workspace_id: WorkspaceId(workspace.clone()),
                        token,
                    })
                    .is_ok()
                {
                    host.verifications.loading = Some((workspace, token));
                }
            }
        }
    }
}
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(folder: &Path) -> (Project, cc::RouteTarget) {
        let (mut project, assignment) = super::super::tests::attempted();
        project.snapshot.config.folder = folder.to_string_lossy().into_owned();
        project.snapshot.tasks[0].state = work::WorkState::ResultSubmitted;
        project.snapshot.tasks[0].result = Some(work::TaskResult {
            revision: work::ResultRevision(1),
            summary: "Actual result".into(),
            source_revision: Some("provider-turn".into()),
            artifact_revision: None,
            verification: work::VerificationState::NotRun,
        });
        project.snapshot.attempts[0].folder = folder.to_string_lossy().into_owned();
        project.snapshot.attempts[0].state = work::WorkState::ResultSubmitted;
        let route = cc::RouteTarget {
            workspace_id: project.snapshot.config.workspace_id.0.clone(),
            session_id: None,
            task_id: Some(assignment.task.id.0),
        };
        (project, route)
    }
    fn pending(
        project: &Project,
        route: cc::RouteTarget,
        approved: &approval::ApprovedVerification,
    ) -> Pending {
        Pending {
            operation: work::OperationId::new().0,
            route,
            evidence: approved.evidence().clone(),
            config: project.snapshot.config.revision,
            binding: project.snapshot.attempts[0].runtime_binding.clone(),
            cancelled: false,
            folder: project.snapshot.attempts[0].folder.clone(),
        }
    }
    #[test]
    fn command_center_verification_review_defers_unavailable_folder_to_the_worker() {
        let folder = std::env::temp_dir().join(format!("cc-not-created-{}", uuid::Uuid::new_v4()));
        let (project, route) = fixture(&folder);
        let expected = result_target(&project, &route).unwrap();
        let result = review_verification(
            &project,
            &expected,
            work::VerificationSpec {
                command: "true".into(),
                cwd: folder.to_string_lossy().into_owned(),
                timeout_ms: 1000,
                output_limit_bytes: 128,
            },
        );
        assert!(
            result.is_ok(),
            "native review must not resolve or open a folder: {:?}",
            result.err()
        );
        assert!(!folder.exists());
    }
    #[test]
    fn command_center_verification_manual_correlation_survives_worker_redaction() {
        let dir =
            std::env::temp_dir().join(format!("cc-manual-redaction-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let (project, route) = fixture(&dir);
        let original = work::ManualVerificationEvidence {
            id: work::VerificationId::new(),
            target: result_target(&project, &route).unwrap(),
            recorded_at_ms: now_ms(),
            passed: true,
            evidence: "inspected newly-registered-manual-secret-value personally".into(),
        };
        let redaction = secret::RedactionService::new();
        // Registration occurs after the native review captured its original text.
        redaction
            .register_permanent(&secret::SecretString::new(
                "newly-registered-manual-secret-value".into(),
            ))
            .unwrap();
        let mut worker = VerificationWorker::new(dir.clone(), redaction, || {});
        worker
            .try_request(VerificationJob::RecordManual {
                evidence: original.clone(),
            })
            .ok()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let result = loop {
            if let Some(result) = worker.try_recv() {
                break result.unwrap();
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let VerificationResult::ManualRecorded { evidence, result } = result else {
            panic!("manual result")
        };
        result.unwrap();
        assert!(evidence.evidence.contains("[REDACTED]"));
        assert_ne!(evidence.evidence, original.evidence);
        assert!(
            same_manual_request(&original, &evidence),
            "worker redaction must not lose the pending receipt"
        );
        let operation = work::OperationId::new().0;
        let mut host = CommandCenterHost::new(dir.clone(), &egui::Context::default());
        host.verifications.manual = Some(Manual {
            operation: operation.clone(),
            route,
            evidence: original,
            config: project.snapshot.config.revision,
            binding: project.snapshot.attempts[0].runtime_binding.clone(),
        });
        let pending = take_manual_request(&mut host, &evidence).unwrap();
        assert_eq!(pending.operation, operation);
        assert!(
            !host
                .receipts
                .iter()
                .any(|receipt| receipt.status == cc::ReceiptStatus::Rejected)
        );
        drop(worker);
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn command_center_verification_mismatched_manual_response_finishes_original_receipt() {
        let (project, route) = fixture(&std::env::temp_dir());
        let original = work::ManualVerificationEvidence {
            id: work::VerificationId::new(),
            target: result_target(&project, &route).unwrap(),
            recorded_at_ms: now_ms(),
            passed: true,
            evidence: "personal inspection".into(),
        };
        let operation = work::OperationId::new().0;
        let mut host = CommandCenterHost::new(std::env::temp_dir(), &egui::Context::default());
        host.verifications.manual = Some(Manual {
            operation: operation.clone(),
            route,
            evidence: original.clone(),
            config: project.snapshot.config.revision,
            binding: project.snapshot.attempts[0].runtime_binding.clone(),
        });
        let mut mismatched = original.clone();
        mismatched.target.result_revision.0 += 1;
        assert!(take_manual_request(&mut host, &mismatched).is_none());
        assert!(!host.verifications.busy());
        assert!(
            host.receipts
                .iter()
                .any(|receipt| receipt.operation_id == operation
                    && receipt.status == cc::ReceiptStatus::Rejected)
        );
        let mut changed = original.clone();
        changed.id = work::VerificationId::new();
        assert!(!same_manual_request(&original, &changed));
        let mut changed = original.clone();
        changed.passed = false;
        assert!(!same_manual_request(&original, &changed));
        changed = original.clone();
        changed.recorded_at_ms += 1;
        assert!(!same_manual_request(&original, &changed));
    }
    #[test]
    fn command_center_verification_real_shell_durable_evidence_requires_separate_final_confirmation()
     {
        let dir =
            std::env::temp_dir().join(format!("cc-host-verification-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let (mut project, route) = fixture(&dir);
        let requested = review_verification(
            &project,
            &result_target(&project, &route).unwrap(),
            work::VerificationSpec {
                command: "printf prepared > command-ran.txt; printf root-owned-evidence".into(),
                cwd: dir.to_string_lossy().into_owned(),
                timeout_ms: 1000,
                output_limit_bytes: 128,
            },
        )
        .unwrap();
        let pending = Pending {
            operation: work::OperationId::new().0,
            route,
            evidence: requested.clone(),
            config: project.snapshot.config.revision,
            binding: project.snapshot.attempts[0].runtime_binding.clone(),
            cancelled: false,
            folder: project.snapshot.attempts[0].folder.clone(),
        };
        let mut worker =
            VerificationWorker::new(dir.clone(), secret::RedactionService::new(), || {});
        worker
            .try_request(VerificationJob::prepare(project.state(), requested.clone()))
            .ok()
            .unwrap();
        let prepare_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let prepared = loop {
            if let Some(result) = worker.try_recv() {
                break result.unwrap();
            }
            assert!(std::time::Instant::now() < prepare_deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let VerificationResult::Prepared {
            requested: captured,
            approved,
        } = prepared
        else {
            panic!("preparation result")
        };
        let approved = approved.ok().unwrap();
        assert!(preparation_current(
            &project, &pending, &captured, &approved
        ));
        assert!(!dir.join("command-ran.txt").exists());
        assert!(!dir.join(stored::COMMAND_CENTER_FILE_NAME).exists());
        project.snapshot.config.revision.0 += 1;
        assert!(!preparation_current(
            &project, &pending, &captured, &approved
        ));
        project.snapshot.config.revision.0 -= 1;
        worker
            .try_request(VerificationJob::Run(*approved))
            .ok()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let result = loop {
            if let Some(result) = worker.try_recv() {
                break result.unwrap();
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let VerificationResult::Finished {
            evidence,
            cancelled,
        } = result
        else {
            panic!("expected durable result")
        };
        assert_eq!(evidence.stdout, "root-owned-evidence");
        let store = stored::CommandCenterStore::open(&dir).unwrap();
        assert!(store.load_verifications(&evidence.target.workspace_id).unwrap().iter().any(|record|matches!(record,work::VerificationEvidence::AppLocal(stored) if stored==evidence.as_ref())));
        assert!(apply_evidence(&mut project, &pending, &evidence, cancelled).unwrap());
        assert_eq!(
            project.snapshot.tasks[0].state,
            work::WorkState::AwaitingUserConfirmation
        );
        assert_ne!(project.snapshot.tasks[0].state, work::WorkState::Completed);
        drop(worker);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn command_center_verification_stale_config_folder_spec_and_cancel_never_mark_passed() {
        let (mut project, route) = fixture(&std::env::temp_dir());
        let approved = approval::approve_local(
            &project.state(),
            &project.snapshot.tasks[0].id,
            work::ResultRevision(1),
            work::VerificationSpec {
                command: "true".into(),
                cwd: std::env::temp_dir().to_string_lossy().into_owned(),
                timeout_ms: 1000,
                output_limit_bytes: 128,
            },
        )
        .unwrap();
        let pending = pending(&project, route, &approved);
        let mut evidence = approved.evidence().clone();
        evidence.outcome = work::CommandVerificationOutcome::Passed;
        evidence.exit_code = Some(0);
        evidence.ended_at_ms = Some(evidence.started_at_ms);
        assert!(exact(&project, &pending));
        assert!(!apply_evidence(&mut project, &pending, &evidence, true).unwrap());
        project.snapshot.config.revision.0 += 1;
        assert!(!apply_evidence(&mut project, &pending, &evidence, false).unwrap());
        project.snapshot.config.revision.0 -= 1;
        project.snapshot.attempts[0].folder.push_str("/changed");
        assert!(!apply_evidence(&mut project, &pending, &evidence, false).unwrap());
        project.snapshot.attempts[0].folder = pending.folder.clone();
        evidence.spec.command = "different".into();
        assert!(!apply_evidence(&mut project, &pending, &evidence, false).unwrap());
        assert_eq!(
            project.snapshot.tasks[0]
                .result
                .as_ref()
                .unwrap()
                .verification,
            work::VerificationState::NotRun
        );
    }
    #[test]
    fn command_center_verification_completed_result_is_read_only_and_native_intents_reject_wire() {
        let (mut project, route) = fixture(&std::env::temp_dir());
        project.snapshot.tasks[0].state = work::WorkState::Completed;
        let mut task = cc::Task {
            target: route,
            ..Default::default()
        };
        Verifications::default().project(
            &mut task,
            &project,
            true,
            &secret::RedactionService::new(),
        );
        assert!(task.verification_target.is_some());
        assert!(!task.execute_verification.enabled);
        let expected = task.verification_target.clone().unwrap();
        let spec = work::VerificationSpec {
            command: "true".into(),
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            timeout_ms: 1000,
            output_limit_bytes: 128,
        };
        for value in [
            serde_json::json!({"type":"execute_verification","target":task.target,"expected":expected,"spec":spec}),
            serde_json::json!({"type":"cancel_verification","target":task.target,"expected":expected,"verification_id":work::VerificationId::new()}),
        ] {
            assert!(serde_json::from_value::<cc::Intent>(value).is_err());
        }
        assert!(
            serde_json::to_value(cc::Intent::ExecuteVerification {
                target: task.target,
                expected,
                spec
            })
            .is_err()
        );
    }
}
