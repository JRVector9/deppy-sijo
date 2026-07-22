//! One combined durable authorization executor for production proxy tools/call.
//!
//! The executor owns validate -> live schema -> permission/approval -> atomic audit preflight ->
//! exact-bound call -> durable outcome as one callback. Raw arguments remain in mcp's bounded,
//! zeroizing `SensitiveToolInput` only; no split hook/forwarder handoff or schema cache exists.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use audit::{
    ApprovalDecision, AuthorizationEvaluation, AuthorizationOutcome, AuthorizationPreflight,
    PermissionFingerprint, PermissionRule,
};
use deppy_core::time::unix_secs_i64;
use mcp::{
    AuthorizedToolError, AuthorizedToolExecutor, AuthorizedToolOutcome, McpDeliveryUnknown,
    McpServerResponseError, SensitiveToolInput,
};
use secret::RedactionService;
use serde_json::Value;
use storage::{ActiveAuthorizationOwner, ApprovalStatus, Db};

use crate::approval_notify::ApprovalWakeNotifier;
#[cfg(test)]
use crate::session::ConfigRevision;
use crate::session::{BackendClient, BackendVersion};

const APPROVAL_PREVIEW_CHARS: usize = 500;
const OUTCOME_WRITE_ATTEMPTS: usize = 2;

pub(crate) fn authorization_subject_from_runtime_session_key(
    session_key: Option<&str>,
) -> anyhow::Result<audit::AuthorizationSubject> {
    let Some(session_key) = session_key else {
        return Ok(audit::AuthorizationSubject::global());
    };
    let (workspace_id, session_id) = deppy_core::parse_session_key(session_key)
        .ok_or_else(|| anyhow::anyhow!("invalid_runtime_session_key"))?;
    audit::AuthorizationSubject::try_new(
        Some(workspace_id.to_owned()),
        Some(session_id.0.to_string()),
    )
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AuthorizationStageCounters {
    pub validated: u64,
    pub live_schema: u64,
    pub permission: u64,
    pub approval: u64,
    pub preflight: u64,
    pub backend_calls: u64,
    pub outcomes: u64,
}

#[derive(Default)]
struct StageCounters {
    validated: Cell<u64>,
    live_schema: Cell<u64>,
    permission: Cell<u64>,
    approval: Cell<u64>,
    preflight: Cell<u64>,
    backend_calls: Cell<u64>,
    outcomes: Cell<u64>,
}

impl StageCounters {
    #[cfg(test)]
    fn snapshot(&self) -> AuthorizationStageCounters {
        AuthorizationStageCounters {
            validated: self.validated.get(),
            live_schema: self.live_schema.get(),
            permission: self.permission.get(),
            approval: self.approval.get(),
            preflight: self.preflight.get(),
            backend_calls: self.backend_calls.get(),
            outcomes: self.outcomes.get(),
        }
    }
}

trait ToolBackend: Send + Sync {
    fn list_tools(&self) -> anyhow::Result<(BackendVersion, Vec<mcp::McpTool>)>;
    fn call_tool(
        &self,
        expected_version: BackendVersion,
        name: &str,
        arguments: Value,
    ) -> anyhow::Result<Value>;
}

impl ToolBackend for BackendClient {
    fn list_tools(&self) -> anyhow::Result<(BackendVersion, Vec<mcp::McpTool>)> {
        self.list_tools_versioned()
    }

    fn call_tool(
        &self,
        expected_version: BackendVersion,
        name: &str,
        arguments: Value,
    ) -> anyhow::Result<Value> {
        self.call_tool_versioned(expected_version, name, arguments)
    }
}

#[derive(Clone, Copy)]
struct PendingOutcome {
    operation_id: [u8; 36],
    outcome: AuthorizationOutcome,
}

impl PendingOutcome {
    fn new(operation_id: &str, outcome: AuthorizationOutcome) -> anyhow::Result<Self> {
        let bytes: [u8; 36] = operation_id
            .as_bytes()
            .try_into()
            .map_err(|_| anyhow::anyhow!("operation id shape is not UUID"))?;
        Ok(Self {
            operation_id: bytes,
            outcome,
        })
    }

    fn operation_id(&self) -> &str {
        // UUID operation IDs are guaranteed ASCII at construction.
        std::str::from_utf8(&self.operation_id).expect("UUID operation id is UTF-8")
    }
}

/// Production combined executor. It is intentionally single-threaded at the callback surface;
/// the shared DB mutex exists only so the lazy backend credential resolver reuses this one handle.
pub struct ProxyAuthorizationExecutor {
    db: Arc<Mutex<Db>>,
    owner: RefCell<Option<ActiveAuthorizationOwner>>,
    server_id: String,
    redaction: RedactionService,
    poll_interval: Duration,
    approval_timeout: Duration,
    backend: Arc<dyn ToolBackend>,
    pane_id: Option<String>,
    subject: audit::AuthorizationSubject,
    approval_notifier: Option<Arc<dyn ApprovalWakeNotifier>>,
    pending_outcome: RefCell<Option<PendingOutcome>>,
    counters: Rc<StageCounters>,
    #[cfg(test)]
    fail_post_preflight_parse: Cell<bool>,
    #[cfg(test)]
    before_preflight: RefCell<Option<Box<dyn FnOnce()>>>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn authorized_proxy_executor(
    db: Arc<Mutex<Db>>,
    owner: ActiveAuthorizationOwner,
    server_id: String,
    redaction: RedactionService,
    poll_interval: Duration,
    approval_timeout: Duration,
    backend: Arc<BackendClient>,
    pane_id: Option<String>,
    approval_notifier: Option<Arc<dyn ApprovalWakeNotifier>>,
) -> anyhow::Result<ProxyAuthorizationExecutor> {
    let subject = authorization_subject_from_runtime_session_key(pane_id.as_deref())?;
    Ok(ProxyAuthorizationExecutor {
        db,
        owner: RefCell::new(Some(owner)),
        server_id,
        redaction,
        poll_interval,
        approval_timeout,
        backend,
        pane_id,
        subject,
        approval_notifier,
        pending_outcome: RefCell::new(None),
        counters: Rc::new(StageCounters::default()),
        #[cfg(test)]
        fail_post_preflight_parse: Cell::new(false),
        #[cfg(test)]
        before_preflight: RefCell::new(None),
    })
}

impl ProxyAuthorizationExecutor {
    fn owner_preflight(
        &self,
        plan: audit::AuthorizationPlan,
        input_json: &str,
    ) -> anyhow::Result<AuthorizationPreflight> {
        let db = self
            .db
            .lock()
            .map_err(|_| anyhow::anyhow!("authorization DB unavailable"))?;
        let owner = self.owner.borrow();
        let owner = owner
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("authorization owner closed"))?;
        db.commit_authorization_preflight(owner, plan, input_json, &self.redaction)
    }

    fn live_schema_hash(&self, tool_name: &str) -> anyhow::Result<(String, BackendVersion)> {
        let (revision, tools) = self
            .backend
            .list_tools()
            .map_err(|_| anyhow::anyhow!("live_schema_unavailable"))?;
        self.counters
            .live_schema
            .set(self.counters.live_schema.get().saturating_add(1));
        let mut found = None;
        for tool in tools {
            if tool.name != tool_name {
                continue;
            }
            anyhow::ensure!(found.is_none(), "duplicate_live_tool_name");
            found = Some(audit::schema_hash(&tool.input_schema_json));
        }
        found
            .map(|hash| (hash, revision))
            .ok_or_else(|| anyhow::anyhow!("tool_not_in_live_schema"))
    }

    fn current_rule(&self, tool_name: &str) -> anyhow::Result<PermissionFingerprint> {
        let row = self
            .db
            .lock()
            .map_err(|_| anyhow::anyhow!("authorization DB unavailable"))?
            .permission_rule(&self.server_id, tool_name)?;
        self.counters
            .permission
            .set(self.counters.permission.get().saturating_add(1));
        row.map(|row| {
            let rule = PermissionRule::from_persisted(&row.rule)
                .ok_or_else(|| anyhow::anyhow!("invalid persisted permission rule"))?;
            Ok(PermissionFingerprint::Persisted {
                rule,
                approved_schema_hash: row.approved_schema_hash,
            })
        })
        .unwrap_or(Ok(PermissionFingerprint::Absent))
    }

    fn resolve_approval(
        &self,
        pending: audit::PendingAuthorization,
        input_json: &[u8],
    ) -> audit::AuthorizationPlan {
        let preview = match audit::sanitized_input_preview(
            input_json,
            &self.redaction,
            APPROVAL_PREVIEW_CHARS,
        ) {
            Ok(preview) => preview,
            Err(_) => return pending.resolve(ApprovalDecision::DenyOnce),
        };
        let now = unix_secs_i64();
        let inserted = self
            .db
            .lock()
            .map_err(|_| ())
            .and_then(|db| {
                db.insert_pending_approval(
                    pending.operation_id(),
                    pending.server_id(),
                    pending.tool_name(),
                    &preview,
                    Some(pending.live_schema_hash()),
                    now,
                    self.pane_id.as_deref(),
                )
                .map_err(|_| ())
            })
            .is_ok();
        if !inserted {
            return pending.resolve(ApprovalDecision::DenyOnce);
        }
        self.counters
            .approval
            .set(self.counters.approval.get().saturating_add(1));
        if self
            .approval_notifier
            .as_ref()
            .is_some_and(|notifier| notifier.notify().is_err())
        {
            if let Ok(db) = self.db.lock() {
                let _ = db.resolve_approval(pending.operation_id(), false, false, unix_secs_i64());
            }
            return pending.resolve(ApprovalDecision::DenyOnce);
        }

        let deadline = Instant::now() + self.approval_timeout;
        loop {
            let outcome = self
                .db
                .lock()
                .map_err(|_| ())
                .and_then(|db| db.poll_approval(pending.operation_id()).map_err(|_| ()));
            match outcome {
                Err(()) => return pending.resolve(ApprovalDecision::DenyOnce),
                Ok(outcome) => match outcome.status {
                    ApprovalStatus::Allowed => {
                        return pending.resolve(if outcome.remember {
                            ApprovalDecision::AllowAlways
                        } else {
                            ApprovalDecision::AllowOnce
                        });
                    }
                    ApprovalStatus::Denied => {
                        return pending.resolve(if outcome.remember {
                            ApprovalDecision::DenyAlways
                        } else {
                            ApprovalDecision::DenyOnce
                        });
                    }
                    ApprovalStatus::Pending => {
                        let now = Instant::now();
                        if now >= deadline {
                            if let Ok(db) = self.db.lock() {
                                let _ = db.resolve_approval(
                                    pending.operation_id(),
                                    false,
                                    false,
                                    unix_secs_i64(),
                                );
                            }
                            return pending.resolve(ApprovalDecision::DenyOnce);
                        }
                        std::thread::sleep(
                            self.poll_interval
                                .min(deadline.saturating_duration_since(now)),
                        );
                    }
                },
            }
        }
    }

    fn try_complete(&self, pending: PendingOutcome) -> bool {
        for _ in 0..OUTCOME_WRITE_ATTEMPTS {
            let result = self
                .db
                .lock()
                .map_err(|_| anyhow::anyhow!("authorization DB unavailable"))
                .and_then(|db| {
                    let owner = self.owner.borrow();
                    let owner = owner
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("authorization owner closed"))?;
                    db.complete_authorization_outcome(
                        owner,
                        pending.operation_id(),
                        pending.outcome,
                    )
                });
            if result.is_ok() {
                self.counters
                    .outcomes
                    .set(self.counters.outcomes.get().saturating_add(1));
                return true;
            }
        }
        false
    }

    fn persist_or_retain(&self, operation_id: &str, outcome: AuthorizationOutcome) -> bool {
        let Ok(pending) = PendingOutcome::new(operation_id, outcome) else {
            return false;
        };
        if self.try_complete(pending) {
            return true;
        }
        let mut slot = self.pending_outcome.borrow_mut();
        debug_assert!(
            slot.is_none(),
            "only one outcome obligation may be retained"
        );
        *slot = Some(pending);
        false
    }

    fn resolve_pending_outcome(&self) -> bool {
        let Some(pending) = self.pending_outcome.borrow_mut().take() else {
            return true;
        };
        if self.try_complete(pending) {
            true
        } else {
            *self.pending_outcome.borrow_mut() = Some(pending);
            false
        }
    }

    fn parse_call_arguments(&self, input: &[u8]) -> anyhow::Result<Value> {
        #[cfg(test)]
        if self.fail_post_preflight_parse.get() {
            anyhow::bail!("injected post-preflight parse failure");
        }
        serde_json::from_slice(input).context("validated tool input reparse failed")
    }

    fn execute(&self, tool_name: String, input: SensitiveToolInput) -> AuthorizedToolOutcome {
        if audit::validate_tool_input(input.as_bytes()).is_err() {
            return AuthorizedToolOutcome::Error(AuthorizedToolError::InvalidInput);
        }
        self.counters
            .validated
            .set(self.counters.validated.get().saturating_add(1));
        if !self.resolve_pending_outcome() {
            return AuthorizedToolOutcome::Error(AuthorizedToolError::AuditUnavailable);
        }

        let (live_schema_hash, backend_version) = match self.live_schema_hash(&tool_name) {
            Ok(versioned_hash) => versioned_hash,
            Err(_) => {
                return AuthorizedToolOutcome::Error(AuthorizedToolError::BackendFailed);
            }
        };
        let permission = match self.current_rule(&tool_name) {
            Ok(permission) => permission,
            Err(_) => {
                return AuthorizedToolOutcome::Error(AuthorizedToolError::AuditUnavailable);
            }
        };
        let operation_id = uuid::Uuid::new_v4().to_string();
        let evaluation = match audit::evaluate_authorization_with_fingerprint(
            operation_id,
            self.server_id.clone(),
            tool_name.clone(),
            permission,
            live_schema_hash,
        )
        .and_then(|evaluation| evaluation.bind_subject(self.subject.clone()))
        {
            Ok(evaluation) => evaluation,
            Err(_) => {
                return AuthorizedToolOutcome::Error(AuthorizedToolError::PermissionDenied);
            }
        };
        let plan = match evaluation {
            AuthorizationEvaluation::Plan(plan) => plan,
            AuthorizationEvaluation::NeedsApproval(pending) => {
                self.resolve_approval(pending, input.as_bytes())
            }
        };
        let input_json = match std::str::from_utf8(input.as_bytes()) {
            Ok(input) => input,
            Err(_) => return AuthorizedToolOutcome::Error(AuthorizedToolError::InvalidInput),
        };
        #[cfg(test)]
        if let Some(before_preflight) = self.before_preflight.borrow_mut().take() {
            before_preflight();
        }
        let preflight = match self.owner_preflight(plan, input_json) {
            Ok(preflight) => preflight,
            Err(_) => {
                return AuthorizedToolOutcome::Error(AuthorizedToolError::AuditUnavailable);
            }
        };
        self.counters
            .preflight
            .set(self.counters.preflight.get().saturating_add(1));
        let grant = match preflight {
            AuthorizationPreflight::Prepared(grant) => grant,
            AuthorizationPreflight::Denied(receipt) => {
                let _ = receipt.operation_id();
                return AuthorizedToolOutcome::Error(AuthorizedToolError::PermissionDenied);
            }
        };
        let operation_id = grant.operation_id().to_owned();
        let authorization = match grant.bind_call_for_subject(
            &self.subject,
            &self.server_id,
            &tool_name,
            input.as_bytes(),
        ) {
            Ok(authorization) => authorization,
            Err(_) => {
                let persisted = self.persist_or_retain(
                    &operation_id,
                    AuthorizationOutcome::Failed {
                        error_code: "protocol_mismatch",
                    },
                );
                return AuthorizedToolOutcome::Error(if persisted {
                    AuthorizedToolError::InvalidInput
                } else {
                    AuthorizedToolError::AuditUnavailable
                });
            }
        };
        let arguments: Value = match self.parse_call_arguments(input.as_bytes()) {
            Ok(arguments) => arguments,
            Err(_) => {
                let persisted = self.persist_or_retain(
                    &operation_id,
                    AuthorizationOutcome::Failed {
                        error_code: "input_reparse_failed",
                    },
                );
                return AuthorizedToolOutcome::Error(if persisted {
                    AuthorizedToolError::InvalidInput
                } else {
                    AuthorizedToolError::AuditUnavailable
                });
            }
        };
        drop(input);
        let operation_id = authorization.operation_id().to_owned();
        self.counters
            .backend_calls
            .set(self.counters.backend_calls.get().saturating_add(1));
        match self
            .backend
            .call_tool(backend_version, &tool_name, arguments)
        {
            Ok(value) => {
                let outcome = if value.get("isError").and_then(Value::as_bool) == Some(true) {
                    AuthorizationOutcome::Failed {
                        error_code: "tool_error",
                    }
                } else {
                    AuthorizationOutcome::Succeeded
                };
                if self.persist_or_retain(&operation_id, outcome) {
                    if value.get("isError").and_then(Value::as_bool) == Some(true) {
                        AuthorizedToolOutcome::Error(AuthorizedToolError::BackendFailed)
                    } else {
                        AuthorizedToolOutcome::Success(value)
                    }
                } else {
                    AuthorizedToolOutcome::Error(AuthorizedToolError::AuditUnavailable)
                }
            }
            Err(error) => {
                let (outcome, result_error) =
                    if error.downcast_ref::<McpDeliveryUnknown>().is_some() {
                        (
                            AuthorizationOutcome::Unknown {
                                error_code: "delivery_unknown",
                            },
                            AuthorizedToolError::DeliveryUnknown,
                        )
                    } else if error.downcast_ref::<McpServerResponseError>().is_some() {
                        (
                            AuthorizationOutcome::Failed {
                                error_code: "server_error",
                            },
                            AuthorizedToolError::BackendFailed,
                        )
                    } else {
                        (
                            AuthorizationOutcome::Failed {
                                error_code: "backend_error",
                            },
                            AuthorizedToolError::BackendFailed,
                        )
                    };
                let persisted = self.persist_or_retain(&operation_id, outcome);
                AuthorizedToolOutcome::Error(if persisted {
                    result_error
                } else {
                    AuthorizedToolError::AuditUnavailable
                })
            }
        }
    }

    fn shutdown(&self) {
        let _ = self.resolve_pending_outcome();
        let Some(owner) = self.owner.borrow_mut().take() else {
            return;
        };
        let result = self
            .db
            .lock()
            .map_err(|_| anyhow::anyhow!("authorization DB unavailable"))
            .and_then(|db| db.close_authorization_owner(owner));
        if result.is_err() {
            tracing::warn!("authorization owner shutdown persistence failed");
        }
    }
}

impl AuthorizedToolExecutor for ProxyAuthorizationExecutor {
    fn list_tools(&self) -> anyhow::Result<Value> {
        crate::forwarder::list_tools(
            self.backend
                .list_tools()
                .map_err(|_| anyhow::anyhow!("backend tools/list failed"))?
                .1,
        )
    }

    fn execute_tool(&self, tool_name: String, input: SensitiveToolInput) -> AuthorizedToolOutcome {
        self.execute(tool_name, input)
    }
}

impl Drop for ProxyAuthorizationExecutor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use rusqlite::OptionalExtension;
    use serde_json::json;

    use super::*;
    #[cfg(unix)]
    use crate::approval_notify::UnixDatagramApprovalNotifier;

    const SCHEMA_A: &str = r#"{"type":"object","properties":{"value":{"type":"string"}}}"#;
    const SCHEMA_B: &str = r#"{"type":"object","properties":{"value":{"type":"number"}}}"#;

    struct FakeBackendState {
        queued_schemas: VecDeque<Vec<mcp::McpTool>>,
        current_schema: Vec<mcp::McpTool>,
    }

    struct FakeBackend {
        revision: ConfigRevision,
        state: Mutex<FakeBackendState>,
        list_failure: AtomicBool,
        lists: AtomicUsize,
        calls: AtomicUsize,
    }

    impl FakeBackend {
        fn new(schemas: Vec<Vec<mcp::McpTool>>) -> Arc<Self> {
            let current_schema = schemas.first().cloned().unwrap_or_default();
            Self::with_initial_state(schemas.into(), current_schema)
        }

        fn with_initial_state(
            queued_schemas: VecDeque<Vec<mcp::McpTool>>,
            current_schema: Vec<mcp::McpTool>,
        ) -> Arc<Self> {
            Arc::new(Self {
                revision: ConfigRevision::next(),
                state: Mutex::new(FakeBackendState {
                    queued_schemas,
                    current_schema,
                }),
                list_failure: AtomicBool::new(false),
                lists: AtomicUsize::new(0),
                calls: AtomicUsize::new(0),
            })
        }

        fn tool(name: &str, schema: &str) -> mcp::McpTool {
            mcp::McpTool {
                name: name.to_owned(),
                description: Some(format!("{name} description")),
                input_schema_json: schema.to_owned(),
            }
        }
    }

    impl ToolBackend for FakeBackend {
        fn list_tools(&self) -> anyhow::Result<(BackendVersion, Vec<mcp::McpTool>)> {
            self.lists.fetch_add(1, Ordering::SeqCst);
            if self.list_failure.load(Ordering::SeqCst) {
                anyhow::bail!("raw-list-error-must-not-escape");
            }
            let mut state = self.state.lock().unwrap();
            if let Some(next) = state.queued_schemas.pop_front() {
                state.current_schema = next;
            }
            Ok((
                BackendVersion {
                    config_revision: self.revision,
                    auth_revision: Vec::new(),
                },
                state.current_schema.clone(),
            ))
        }

        fn call_tool(
            &self,
            expected_version: BackendVersion,
            _name: &str,
            arguments: Value,
        ) -> anyhow::Result<Value> {
            anyhow::ensure!(
                expected_version.config_revision == self.revision
                    && expected_version.auth_revision.is_empty(),
                "stale fake revision"
            );
            self.calls.fetch_add(1, Ordering::SeqCst);
            match arguments.get("mode").and_then(Value::as_str) {
                Some("tool_error") => Ok(json!({"content": [], "isError": true})),
                Some("server_error") => Err(anyhow::Error::new(McpServerResponseError::from_code(
                    Some(-32001),
                ))),
                Some("delivery_unknown") => {
                    Err(anyhow::Error::new(McpDeliveryUnknown { status: None }))
                }
                Some("backend_error") => anyhow::bail!("raw-backend-error-must-not-escape"),
                _ => Ok(json!({"content": [{"type":"text","text":"ok"}], "isError": false})),
            }
        }
    }

    #[derive(Default)]
    struct CountingNotifier {
        sends: AtomicUsize,
    }

    impl ApprovalWakeNotifier for CountingNotifier {
        fn notify(&self) -> anyhow::Result<()> {
            self.sends.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct ResolvingNotifier {
        db: Arc<Mutex<Db>>,
        sends: AtomicUsize,
        durable_operation_ids: Mutex<Vec<String>>,
    }

    impl ResolvingNotifier {
        fn new(db: Arc<Mutex<Db>>) -> Arc<Self> {
            Arc::new(Self {
                db,
                sends: AtomicUsize::new(0),
                durable_operation_ids: Mutex::new(Vec::new()),
            })
        }
    }

    impl ApprovalWakeNotifier for ResolvingNotifier {
        fn notify(&self) -> anyhow::Result<()> {
            self.sends.fetch_add(1, Ordering::SeqCst);
            let db = self
                .db
                .lock()
                .map_err(|_| anyhow::anyhow!("test approval DB unavailable"))?;
            let pending = db.list_pending_approvals()?;
            anyhow::ensure!(
                pending.len() == 1,
                "wake must observe exactly one durable pending approval"
            );
            self.durable_operation_ids
                .lock()
                .map_err(|_| anyhow::anyhow!("test operation id lock unavailable"))?
                .push(pending[0].id.clone());
            db.resolve_approval(&pending[0].id, true, false, unix_secs_i64())
        }
    }

    fn attach_counting_notifier(context: &mut TestContext) -> Arc<CountingNotifier> {
        let notifier = Arc::new(CountingNotifier::default());
        context.executor.approval_notifier = Some(notifier.clone());
        notifier
    }

    struct TestContext {
        dir: PathBuf,
        path: PathBuf,
        db: Arc<Mutex<Db>>,
        backend: Arc<FakeBackend>,
        executor: ProxyAuthorizationExecutor,
        counters: Rc<StageCounters>,
        _cleanup: TestCleanup,
    }

    struct TestCleanup {
        dir: PathBuf,
        lock_dir: Option<PathBuf>,
    }

    impl Drop for TestCleanup {
        fn drop(&mut self) {
            if let Some(lock_dir) = &self.lock_dir {
                let _ = std::fs::remove_dir_all(lock_dir);
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[cfg(unix)]
    fn test_authorization_lock_dir(path: &Path) -> Option<PathBuf> {
        use std::os::unix::fs::MetadataExt as _;

        let metadata = std::fs::metadata(path).ok()?;
        let identity = format!("unix:{}:{}", metadata.dev(), metadata.ino());
        let key = audit::authorization_scope_lock_key(&identity).ok()?;
        Some(
            std::env::temp_dir()
                .join("deppy-authorization-locks")
                .join(key),
        )
    }

    #[cfg(not(unix))]
    fn test_authorization_lock_dir(_path: &Path) -> Option<PathBuf> {
        None
    }

    impl TestContext {
        fn new(schemas: Vec<Vec<mcp::McpTool>>, rule: Option<(&str, Option<&str>)>) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "deppy-au01-proxy-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("metadata.sqlite3");
            let db = Arc::new(Mutex::new(Db::open(&path).unwrap()));
            if let Some((rule, approved_schema)) = rule {
                db.lock()
                    .unwrap()
                    .upsert_permission_rule("server", "echo", rule, approved_schema)
                    .unwrap();
            }
            let owner = db
                .lock()
                .unwrap()
                .acquire_authorization_owner(&format!("proxy:test:{}", uuid::Uuid::new_v4()))
                .unwrap();
            let backend = FakeBackend::new(schemas);
            let redaction = RedactionService::new();
            let pane_id = "315f68b6-333f-409f-a2c5-922b9eacfd7e:2";
            let executor = ProxyAuthorizationExecutor {
                db: Arc::clone(&db),
                owner: RefCell::new(Some(owner)),
                server_id: "server".to_owned(),
                redaction,
                poll_interval: Duration::from_millis(1),
                approval_timeout: Duration::from_secs(2),
                backend: backend.clone(),
                pane_id: Some(pane_id.to_owned()),
                subject: authorization_subject_from_runtime_session_key(Some(pane_id)).unwrap(),
                approval_notifier: None,
                pending_outcome: RefCell::new(None),
                counters: Rc::new(StageCounters::default()),
                fail_post_preflight_parse: Cell::new(false),
                before_preflight: RefCell::new(None),
            };
            let counters = Rc::clone(&executor.counters);
            let cleanup = TestCleanup {
                dir: dir.clone(),
                lock_dir: test_authorization_lock_dir(&path),
            };
            Self {
                dir,
                path,
                db,
                backend,
                executor,
                counters,
                _cleanup: cleanup,
            }
        }

        fn register_secret(&self, value: &str) {
            self.executor
                .redaction
                .register_permanent(&secret::SecretString::new(value.to_owned()))
                .unwrap();
        }

        fn cleanup(self) {
            drop(self.executor);
            drop(self.db);
            std::fs::remove_dir_all(self.dir).unwrap();
        }
    }

    #[derive(Debug)]
    struct AuditRow {
        lifecycle: String,
        error_code: Option<String>,
        decision: String,
        redacted: Option<String>,
        encrypted: Option<Vec<u8>>,
        workspace_id: Option<String>,
        session_id: Option<String>,
    }

    fn audit_rows(path: &Path) -> Vec<AuditRow> {
        let conn = rusqlite::Connection::open(path).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT lifecycle, outcome_error_code, decision, input_redacted_json,
                        input_encrypted_blob, workspace_id, session_id
                 FROM tool_audit_logs ORDER BY rowid",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok(AuditRow {
                lifecycle: row.get(0)?,
                error_code: row.get(1)?,
                decision: row.get(2)?,
                redacted: row.get(3)?,
                encrypted: row.get(4)?,
                workspace_id: row.get(5)?,
                session_id: row.get(6)?,
            })
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    }

    fn call_line(id: usize, arguments: Value) -> String {
        format!(
            "{}\n",
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {"name": "echo", "arguments": arguments},
            })
        )
    }

    fn run_lines(executor: ProxyAuthorizationExecutor, input: String) -> Vec<Value> {
        let mut output = Vec::new();
        mcp::run_authorized_proxy(input.as_bytes(), &mut output, executor).unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn response_error_code(response: &Value) -> Option<&str> {
        response
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
    }

    #[test]
    fn runtime_session_key는_validated_workspace_session_subject로만_분리된다() {
        let key = "315f68b6-333f-409f-a2c5-922b9eacfd7e:18446744073709551615";
        let subject = authorization_subject_from_runtime_session_key(Some(key)).unwrap();
        assert_eq!(
            subject.workspace_id(),
            Some("315f68b6-333f-409f-a2c5-922b9eacfd7e")
        );
        assert_eq!(subject.session_id(), Some("18446744073709551615"));
        assert!(
            authorization_subject_from_runtime_session_key(None)
                .unwrap()
                .is_global()
        );
        for invalid in [
            "workspace:not-a-number",
            "missing-separator",
            ":7",
            "workspace:7\0tail",
        ] {
            assert!(authorization_subject_from_runtime_session_key(Some(invalid)).is_err());
        }
        assert!(!format!("{subject:?}").contains("315f68b6"));
    }

    fn spawn_approval(
        path: PathBuf,
        allowed: bool,
        remember: bool,
    ) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let db = Db::open(&path).unwrap();
            for _ in 0..2_000 {
                if let Some(row) = db.list_pending_approvals().unwrap().into_iter().next() {
                    let preview = row.arguments_preview;
                    db.resolve_approval(
                        &row.id,
                        allowed,
                        remember,
                        deppy_core::time::unix_secs_i64(),
                    )
                    .unwrap();
                    return preview;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            panic!("pending approval was not observed")
        })
    }

    #[test]
    fn invalid_arguments는_authorization_audit_backend를_전부_건너뛴다() {
        let schema_hash = audit::schema_hash(SCHEMA_A);
        let mut context = TestContext::new(
            vec![vec![FakeBackend::tool("echo", SCHEMA_A)]],
            Some(("allow", Some(&schema_hash))),
        );
        let notifier = attach_counting_notifier(&mut context);
        let mut input = String::new();
        for (id, arguments) in [Value::Null, json!([]), json!("string")]
            .into_iter()
            .enumerate()
        {
            input.push_str(&call_line(id, arguments));
        }
        input.push_str(&call_line(4, json!({"value": "x".repeat(33 * 1024)})));
        let counters = Rc::clone(&context.counters);
        let executor = context.executor;
        let responses = run_lines(executor, input);

        assert_eq!(responses.len(), 4);
        assert_eq!(counters.snapshot(), AuthorizationStageCounters::default());
        assert_eq!(context.backend.lists.load(Ordering::SeqCst), 0);
        assert_eq!(context.backend.calls.load(Ordering::SeqCst), 0);
        assert_eq!(notifier.sends.load(Ordering::SeqCst), 0);
        assert!(audit_rows(&context.path).is_empty());
        drop(context.db);
        std::fs::remove_dir_all(context.dir).unwrap();
    }

    #[test]
    fn malformed_json_request는_approval_wake를_발생시키지_않는다() {
        let mut context = TestContext::new(vec![vec![FakeBackend::tool("echo", SCHEMA_A)]], None);
        let notifier = attach_counting_notifier(&mut context);
        let backend = Arc::clone(&context.backend);
        let db = Arc::clone(&context.db);
        let mut output = Vec::new();

        mcp::run_authorized_proxy(b"{not-json}\n".as_slice(), &mut output, context.executor)
            .unwrap();

        assert_eq!(notifier.sends.load(Ordering::SeqCst), 0);
        assert_eq!(backend.lists.load(Ordering::SeqCst), 0);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        assert!(
            db.lock()
                .unwrap()
                .list_pending_approvals()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn allow는_live_schema_permission_preflight_call_outcome을_정확히_한번씩_수행한다() {
        let schema_hash = audit::schema_hash(SCHEMA_A);
        let mut context = TestContext::new(
            vec![vec![FakeBackend::tool("echo", SCHEMA_A)]],
            Some(("allow", Some(&schema_hash))),
        );
        let notifier = attach_counting_notifier(&mut context);
        let counters = Rc::clone(&context.counters);
        let backend = Arc::clone(&context.backend);
        let path = context.path.clone();
        let dir = context.dir.clone();
        let db = Arc::clone(&context.db);
        let responses = run_lines(
            context.executor,
            call_line(1, json!({"token": "never-persist-this-secret"})),
        );

        assert_eq!(
            responses[0].pointer("/result/isError"),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            counters.snapshot(),
            AuthorizationStageCounters {
                validated: 1,
                live_schema: 1,
                permission: 1,
                approval: 0,
                preflight: 1,
                backend_calls: 1,
                outcomes: 1,
            }
        );
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        assert_eq!(notifier.sends.load(Ordering::SeqCst), 0);
        let rows = audit_rows(&path);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].lifecycle, "succeeded");
        assert_eq!(rows[0].decision, "policy_allow");
        assert_eq!(
            rows[0].workspace_id.as_deref(),
            Some("315f68b6-333f-409f-a2c5-922b9eacfd7e")
        );
        assert_eq!(rows[0].session_id.as_deref(), Some("2"));
        assert!(rows[0].encrypted.is_none());
        assert!(
            !rows[0]
                .redacted
                .as_deref()
                .unwrap()
                .contains("never-persist")
        );
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn deny는_durable_denied를_남기고_external_call을_하지_않는다() {
        let mut context = TestContext::new(
            vec![vec![FakeBackend::tool("echo", SCHEMA_A)]],
            Some(("deny", None)),
        );
        let notifier = attach_counting_notifier(&mut context);
        let backend = Arc::clone(&context.backend);
        let path = context.path.clone();
        let dir = context.dir.clone();
        let db = Arc::clone(&context.db);
        let responses = run_lines(context.executor, call_line(1, json!({})));
        assert_eq!(
            response_error_code(&responses[0]),
            Some("permission_denied")
        );
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        assert_eq!(notifier.sends.load(Ordering::SeqCst), 0);
        let rows = audit_rows(&path);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].lifecycle, "denied");
        assert_eq!(rows[0].decision, "policy_deny");
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ask_allow_always는_preview를_redact하고_permission_audit_call을_원자완료한다() {
        let context = TestContext::new(vec![vec![FakeBackend::tool("echo", SCHEMA_A)]], None);
        context.register_secret("rotating-secret-material");
        let approval = spawn_approval(context.path.clone(), true, true);
        let backend = Arc::clone(&context.backend);
        let path = context.path.clone();
        let dir = context.dir.clone();
        let db = Arc::clone(&context.db);
        let responses = run_lines(
            context.executor,
            call_line(1, json!({"value": "rotating-secret-material"})),
        );
        let preview = approval.join().unwrap();

        assert_eq!(
            responses[0].pointer("/result/isError"),
            Some(&Value::Bool(false))
        );
        assert!(!preview.contains("rotating-secret-material"));
        assert!(preview.contains("[REDACTED]"));
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        let persisted = db
            .lock()
            .unwrap()
            .permission_rule("server", "echo")
            .unwrap()
            .unwrap();
        assert_eq!(persisted.rule, "allow");
        assert_eq!(
            persisted.approved_schema_hash,
            Some(audit::schema_hash(SCHEMA_A))
        );
        let rows = audit_rows(&path);
        assert_eq!(rows[0].lifecycle, "succeeded");
        assert_eq!(rows[0].decision, "allow_always");
        assert!(rows[0].encrypted.is_none());
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn approval_timeout은_denied로_종결하고_external_call을_하지_않는다() {
        let mut context = TestContext::new(vec![vec![FakeBackend::tool("echo", SCHEMA_A)]], None);
        context.executor.approval_timeout = Duration::ZERO;
        let backend = Arc::clone(&context.backend);
        let path = context.path.clone();
        let dir = context.dir.clone();
        let db = Arc::clone(&context.db);
        let responses = run_lines(context.executor, call_line(1, json!({})));
        assert_eq!(
            response_error_code(&responses[0]),
            Some("permission_denied")
        );
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        assert_eq!(audit_rows(&path)[0].lifecycle, "denied");
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ask는_durable_insert후_payload_free_wake를_정확히_한번씩_보낸다() {
        let mut context = TestContext::new(vec![vec![FakeBackend::tool("echo", SCHEMA_A)]], None);
        let notifier = ResolvingNotifier::new(Arc::clone(&context.db));
        context.executor.approval_notifier = Some(notifier.clone());

        let backend = Arc::clone(&context.backend);
        let input = format!("{}{}", call_line(1, json!({})), call_line(2, json!({})));
        let responses = run_lines(context.executor, input);
        let operation_ids = notifier.durable_operation_ids.lock().unwrap();

        assert_eq!(responses.len(), 2);
        assert!(
            responses
                .iter()
                .all(|response| response.pointer("/result/isError") == Some(&Value::Bool(false)))
        );
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        assert_eq!(notifier.sends.load(Ordering::SeqCst), 2);
        assert_eq!(operation_ids.len(), 2);
        assert_ne!(operation_ids[0], operation_ids[1]);
    }

    #[cfg(unix)]
    #[test]
    fn missing_notify_socket은_pending을_deny하고_external_call을_막는다() {
        let mut context = TestContext::new(vec![vec![FakeBackend::tool("echo", SCHEMA_A)]], None);
        let missing = PathBuf::from(format!(
            "/tmp/daw-missing-{}.sock",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        context.executor.approval_notifier = Some(Arc::new(
            UnixDatagramApprovalNotifier::new(missing).unwrap(),
        ));
        let backend = Arc::clone(&context.backend);
        let db = Arc::clone(&context.db);
        let path = context.path.clone();
        let responses = run_lines(context.executor, call_line(1, json!({})));

        assert_eq!(
            response_error_code(&responses[0]),
            Some("permission_denied")
        );
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        assert!(
            db.lock()
                .unwrap()
                .list_pending_approvals()
                .unwrap()
                .is_empty()
        );
        let denied: i64 = rusqlite::Connection::open(path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM pending_approvals WHERE status = 'denied'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(denied, 1, "failed delivery must terminate the durable row");
    }

    #[test]
    fn pending_insert_failure는_wake와_external_call을_모두_막는다() {
        let mut context = TestContext::new(vec![vec![FakeBackend::tool("echo", SCHEMA_A)]], None);
        let notifier = attach_counting_notifier(&mut context);
        let conn = rusqlite::Connection::open(&context.path).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_pending_insert BEFORE INSERT ON pending_approvals
             BEGIN SELECT RAISE(ABORT, 'injected pending insert failure'); END;",
        )
        .unwrap();
        drop(conn);
        let backend = Arc::clone(&context.backend);
        let responses = run_lines(context.executor, call_line(1, json!({})));

        assert_eq!(
            response_error_code(&responses[0]),
            Some("permission_denied")
        );
        assert_eq!(notifier.sends.load(Ordering::SeqCst), 0);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn policy_allow_preflight_failure는_wake와_external_call을_모두_막는다() {
        let schema_hash = audit::schema_hash(SCHEMA_A);
        let mut context = TestContext::new(
            vec![vec![FakeBackend::tool("echo", SCHEMA_A)]],
            Some(("allow", Some(&schema_hash))),
        );
        let notifier = attach_counting_notifier(&mut context);
        let conn = rusqlite::Connection::open(&context.path).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_policy_preflight BEFORE INSERT ON tool_audit_logs
             BEGIN SELECT RAISE(ABORT, 'injected policy preflight failure'); END;",
        )
        .unwrap();
        drop(conn);
        let backend = Arc::clone(&context.backend);
        let responses = run_lines(context.executor, call_line(1, json!({})));

        assert_eq!(
            response_error_code(&responses[0]),
            Some("audit_unavailable")
        );
        assert_eq!(notifier.sends.load(Ordering::SeqCst), 0);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dynamic_schema는_매_call_live조회하고_schema변경을_재승인한다() {
        let schema_hash = audit::schema_hash(SCHEMA_A);
        let context = TestContext::new(
            vec![
                vec![FakeBackend::tool("echo", SCHEMA_A)],
                vec![FakeBackend::tool("echo", SCHEMA_B)],
            ],
            Some(("allow", Some(&schema_hash))),
        );
        let approval = spawn_approval(context.path.clone(), true, false);
        let backend = Arc::clone(&context.backend);
        let path = context.path.clone();
        let dir = context.dir.clone();
        let db = Arc::clone(&context.db);
        let input = format!("{}{}", call_line(1, json!({})), call_line(2, json!({})));
        let responses = run_lines(context.executor, input);
        approval.join().unwrap();

        assert_eq!(responses.len(), 2);
        assert_eq!(backend.lists.load(Ordering::SeqCst), 2);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        let rows = audit_rows(&path);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].decision, "policy_allow");
        assert_eq!(rows[1].decision, "allow_once");
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn backend_outcomes는_known_failed와_unknown을_구분하고_자동재시도하지_않는다() {
        let schema_hash = audit::schema_hash(SCHEMA_A);
        let context = TestContext::new(
            vec![vec![FakeBackend::tool("echo", SCHEMA_A)]],
            Some(("allow", Some(&schema_hash))),
        );
        let backend = Arc::clone(&context.backend);
        let path = context.path.clone();
        let dir = context.dir.clone();
        let db = Arc::clone(&context.db);
        let modes = [
            "success",
            "tool_error",
            "server_error",
            "backend_error",
            "delivery_unknown",
        ];
        let input = modes
            .iter()
            .enumerate()
            .map(|(index, mode)| call_line(index + 1, json!({"mode": mode})))
            .collect::<String>();
        let responses = run_lines(context.executor, input);

        assert_eq!(backend.calls.load(Ordering::SeqCst), modes.len());
        assert_eq!(response_error_code(&responses[1]), Some("backend_failed"));
        assert_eq!(response_error_code(&responses[2]), Some("backend_failed"));
        assert_eq!(response_error_code(&responses[3]), Some("backend_failed"));
        assert_eq!(
            response_error_code(&responses[4]),
            Some("delivery_unknown_no_retry")
        );
        let rows = audit_rows(&path);
        assert_eq!(
            rows.iter()
                .filter(|row| row.lifecycle == "succeeded")
                .count(),
            1
        );
        assert_eq!(
            rows.iter().filter(|row| row.lifecycle == "failed").count(),
            3
        );
        assert_eq!(
            rows.iter().filter(|row| row.lifecycle == "unknown").count(),
            1
        );
        assert!(
            rows.iter()
                .any(|row| row.error_code.as_deref() == Some("tool_error"))
        );
        assert!(
            rows.iter()
                .any(|row| row.error_code.as_deref() == Some("server_error"))
        );
        assert!(
            rows.iter()
                .any(|row| row.error_code.as_deref() == Some("backend_error"))
        );
        assert!(
            rows.iter()
                .any(|row| row.error_code.as_deref() == Some("delivery_unknown"))
        );
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn preflight_commit실패는_allow_always_permission과_call을_모두_rollback한다() {
        let context = TestContext::new(vec![vec![FakeBackend::tool("echo", SCHEMA_A)]], None);
        let conn = rusqlite::Connection::open(&context.path).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_proxy_preflight BEFORE INSERT ON tool_audit_logs
             BEGIN SELECT RAISE(ABORT, 'injected preflight failure'); END;",
        )
        .unwrap();
        drop(conn);
        let approval = spawn_approval(context.path.clone(), true, true);
        let backend = Arc::clone(&context.backend);
        let path = context.path.clone();
        let dir = context.dir.clone();
        let db = Arc::clone(&context.db);
        let responses = run_lines(context.executor, call_line(1, json!({})));
        approval.join().unwrap();

        assert_eq!(
            response_error_code(&responses[0]),
            Some("audit_unavailable")
        );
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        assert!(
            db.lock()
                .unwrap()
                .permission_rule("server", "echo")
                .unwrap()
                .is_none()
        );
        assert!(audit_rows(&path).is_empty());
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn permission이_evaluation후_deny로바뀌면_preflight와_external_call을_막는다() {
        let schema_hash = audit::schema_hash(SCHEMA_A);
        let context = TestContext::new(
            vec![vec![FakeBackend::tool("echo", SCHEMA_A)]],
            Some(("allow", Some(&schema_hash))),
        );
        let mutation_db = Arc::clone(&context.db);
        *context.executor.before_preflight.borrow_mut() = Some(Box::new(move || {
            mutation_db
                .lock()
                .unwrap()
                .upsert_permission_rule("server", "echo", "deny", None)
                .unwrap();
        }));
        let backend = Arc::clone(&context.backend);
        let path = context.path.clone();
        let dir = context.dir.clone();
        let db = Arc::clone(&context.db);
        let responses = run_lines(context.executor, call_line(1, json!({})));

        assert_eq!(
            response_error_code(&responses[0]),
            Some("audit_unavailable")
        );
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        assert!(audit_rows(&path).is_empty());
        assert_eq!(
            db.lock()
                .unwrap()
                .permission_rule("server", "echo")
                .unwrap()
                .unwrap()
                .rule,
            "deny"
        );
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn post_preflight_reparse실패는_failed로_닫고_external_call을_하지_않는다() {
        let schema_hash = audit::schema_hash(SCHEMA_A);
        let context = TestContext::new(
            vec![vec![FakeBackend::tool("echo", SCHEMA_A)]],
            Some(("allow", Some(&schema_hash))),
        );
        context.executor.fail_post_preflight_parse.set(true);
        let backend = Arc::clone(&context.backend);
        let path = context.path.clone();
        let dir = context.dir.clone();
        let db = Arc::clone(&context.db);
        let responses = run_lines(context.executor, call_line(1, json!({})));

        assert_eq!(response_error_code(&responses[0]), Some("invalid_input"));
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        let rows = audit_rows(&path);
        assert_eq!(rows[0].lifecycle, "failed");
        assert_eq!(rows[0].error_code.as_deref(), Some("input_reparse_failed"));
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn list_failure_duplicate_or_missing_tool은_preflight와_call전에_fail_closed한다() {
        let cases = [
            ("missing", Vec::new(), false),
            (
                "duplicate",
                vec![
                    FakeBackend::tool("echo", SCHEMA_A),
                    FakeBackend::tool("echo", SCHEMA_B),
                ],
                false,
            ),
            ("failure", vec![FakeBackend::tool("echo", SCHEMA_A)], true),
        ];
        for (label, tools, list_failure) in cases {
            let context = TestContext::new(vec![tools], Some(("deny", None)));
            context
                .backend
                .list_failure
                .store(list_failure, Ordering::SeqCst);
            let backend = Arc::clone(&context.backend);
            let path = context.path.clone();
            let dir = context.dir.clone();
            let db = Arc::clone(&context.db);
            let responses = run_lines(context.executor, call_line(1, json!({})));
            assert_eq!(
                response_error_code(&responses[0]),
                Some("backend_failed"),
                "{label}"
            );
            assert_eq!(backend.calls.load(Ordering::SeqCst), 0, "{label}");
            assert!(audit_rows(&path).is_empty(), "{label}");
            drop(db);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    struct TriggerDroppingWriter {
        path: PathBuf,
        bytes: Vec<u8>,
        lines: usize,
    }

    impl Write for TriggerDroppingWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(buffer);
            let lines = buffer.iter().filter(|byte| **byte == b'\n').count();
            let previous = self.lines;
            self.lines += lines;
            if previous < 2 && self.lines >= 2 {
                rusqlite::Connection::open(&self.path)
                    .and_then(|conn| conn.execute_batch("DROP TRIGGER fail_proxy_outcome;"))
                    .map_err(std::io::Error::other)?;
            }
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn outcome_persistence_obligation은_한개만_보관하고_후속_call을_막은뒤_복구한다() {
        let schema_hash = audit::schema_hash(SCHEMA_A);
        let context = TestContext::new(
            vec![vec![FakeBackend::tool("echo", SCHEMA_A)]],
            Some(("allow", Some(&schema_hash))),
        );
        let conn = rusqlite::Connection::open(&context.path).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_proxy_outcome BEFORE UPDATE OF lifecycle ON tool_audit_logs
             WHEN OLD.lifecycle = 'prepared' AND NEW.outcome_error_code IS NOT 'owner_shutdown'
             BEGIN SELECT RAISE(ABORT, 'injected outcome failure'); END;",
        )
        .unwrap();
        drop(conn);
        let backend = Arc::clone(&context.backend);
        let path = context.path.clone();
        let dir = context.dir.clone();
        let db = Arc::clone(&context.db);
        let input = format!(
            "{}{}{}",
            call_line(1, json!({})),
            call_line(2, json!({})),
            call_line(3, json!({}))
        );
        let mut writer = TriggerDroppingWriter {
            path: path.clone(),
            bytes: Vec::new(),
            lines: 0,
        };
        mcp::run_authorized_proxy(input.as_bytes(), &mut writer, context.executor).unwrap();
        let responses: Vec<Value> = String::from_utf8(writer.bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        assert_eq!(
            response_error_code(&responses[0]),
            Some("audit_unavailable")
        );
        assert_eq!(
            response_error_code(&responses[1]),
            Some("audit_unavailable")
        );
        assert_eq!(
            responses[2].pointer("/result/isError"),
            Some(&Value::Bool(false))
        );
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        let rows = audit_rows(&path);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.lifecycle == "succeeded"));
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn executor_drop은_unresolved_prepared를_unknown으로_닫고_call을_재시도하지_않는다() {
        let schema_hash = audit::schema_hash(SCHEMA_A);
        let context = TestContext::new(
            vec![vec![FakeBackend::tool("echo", SCHEMA_A)]],
            Some(("allow", Some(&schema_hash))),
        );
        let conn = rusqlite::Connection::open(&context.path).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_proxy_outcome BEFORE UPDATE OF lifecycle ON tool_audit_logs
             WHEN OLD.lifecycle = 'prepared' AND NEW.outcome_error_code IS NOT 'owner_shutdown'
             BEGIN SELECT RAISE(ABORT, 'injected outcome failure'); END;",
        )
        .unwrap();
        drop(conn);
        let backend = Arc::clone(&context.backend);
        let path = context.path.clone();
        let dir = context.dir.clone();
        let db = Arc::clone(&context.db);
        let responses = run_lines(context.executor, call_line(1, json!({})));

        assert_eq!(
            response_error_code(&responses[0]),
            Some("audit_unavailable")
        );
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        let rows = audit_rows(&path);
        assert_eq!(rows[0].lifecycle, "unknown");
        assert_eq!(rows[0].error_code.as_deref(), Some("owner_shutdown"));
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn audit_row_lookup_helper는_absent를_none으로_읽는다() {
        let context = TestContext::new(Vec::new(), None);
        let conn = rusqlite::Connection::open(&context.path).unwrap();
        let value: Option<String> = conn
            .query_row(
                "SELECT lifecycle FROM tool_audit_logs WHERE operation_id = 'missing'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert!(value.is_none());
        drop(conn);
        context.cleanup();
    }
}
