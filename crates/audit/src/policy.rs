//! PermissionPolicy / ToolApprovalDialog model (설계문서 3장 MCP 모듈).
//! 규칙은 (server_id, tool_name) 단위, 기본값은 Ask.
//! Allow 규칙이라도 마지막 승인 때의 schema hash와 다르면 재승인을 요구한다
//! (PR-22 완료 기준 "MCP tool 변경 시 재승인").

use std::collections::HashMap;

/// tool 하나에 대한 설정 규칙. 미설정 tool은 Ask로 취급한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PermissionRule {
    Allow,
    Deny,
    #[default]
    Ask,
}

/// Exact durable permission row observed before authorization evaluation. `Absent` is distinct
/// from an explicitly persisted Ask row so storage can reject any concurrent policy mutation
/// before it commits audit preflight or mints a call grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionFingerprint {
    Absent,
    Persisted {
        rule: PermissionRule,
        approved_schema_hash: Option<String>,
    },
}

impl PermissionFingerprint {
    pub fn evaluation(&self) -> (PermissionRule, Option<&str>) {
        match self {
            Self::Absent => (PermissionRule::Ask, None),
            Self::Persisted {
                rule,
                approved_schema_hash,
            } => (*rule, approved_schema_hash.as_deref()),
        }
    }
}

impl PermissionRule {
    /// 영속 문자열 (DB 저장/조회 양쪽에서 이것만 쓴다).
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionRule::Allow => "allow",
            PermissionRule::Deny => "deny",
            PermissionRule::Ask => "ask",
        }
    }

    /// 영속 문자열에서 복원 — 알 수 없는 값은 None (호출측이 기본 Ask로 처리).
    pub fn from_persisted(s: &str) -> Option<Self> {
        match s {
            "allow" => Some(PermissionRule::Allow),
            "deny" => Some(PermissionRule::Deny),
            "ask" => Some(PermissionRule::Ask),
            _ => None,
        }
    }
}

/// approval dialog가 표시할 요청 model (dialog UI 자체는 crates/app 소관).
#[derive(PartialEq, Eq)]
pub struct ToolApprovalRequest {
    pub server_id: String,
    pub tool_name: String,
    /// tool input JSON 평문 — dialog 표시용. 영속은 record_audit이 redact 후에만 한다.
    pub input_json: String,
    /// 호출 시점 tool input schema의 해시 (crate::schema_hash)
    pub schema_hash: String,
}

impl Drop for ToolApprovalRequest {
    fn drop(&mut self) {
        // SAFETY: this request exclusively owns the raw input allocation.
        for byte in unsafe { self.input_json.as_mut_vec() } {
            // SAFETY: byte is exclusively borrowed from the owned allocation.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

impl std::fmt::Debug for ToolApprovalRequest {
    /// input_json에 secret이 실릴 수 있으므로 Debug에서는 내용을 숨긴다 (7장 유출 방지)
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolApprovalRequest")
            .field("server_id", &self.server_id)
            .field("tool_name", &self.tool_name)
            .field("input_json", &"<elided>")
            .field("schema_hash", &self.schema_hash)
            .finish()
    }
}

/// 최종 결정. 감사 로그 decision 컬럼 문자열과 1:1 대응한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolDecision {
    /// Allow 규칙에 의한 자동 허용 (dialog 없이)
    PolicyAllow,
    /// Deny 규칙에 의한 자동 거부 (dialog 없이)
    PolicyDeny,
    /// dialog: 이번만 허용
    AllowOnce,
    /// dialog: 항상 허용 — 규칙을 Allow로 바꾸고 현재 schema hash를 승인 이력으로 고정
    AllowAlways,
    /// dialog: 이번만 거부
    DenyOnce,
    /// dialog: 항상 거부 — 규칙을 Deny로 바꾼다
    DenyAlways,
}

impl ToolDecision {
    /// 감사 로그 decision 컬럼 문자열 (snake_case 고정 — 저장/조회 양쪽에서 이것만 쓴다)
    pub fn as_str(self) -> &'static str {
        match self {
            ToolDecision::PolicyAllow => "policy_allow",
            ToolDecision::PolicyDeny => "policy_deny",
            ToolDecision::AllowOnce => "allow_once",
            ToolDecision::AllowAlways => "allow_always",
            ToolDecision::DenyOnce => "deny_once",
            ToolDecision::DenyAlways => "deny_always",
        }
    }

    /// 이 결정으로 tool 호출을 실행해도 되는가
    pub fn is_allowed(self) -> bool {
        matches!(
            self,
            ToolDecision::PolicyAllow | ToolDecision::AllowOnce | ToolDecision::AllowAlways
        )
    }
}

pub const AUTHORIZATION_WORKSPACE_ID_BYTES_MAX: usize = 128;
pub const AUTHORIZATION_SESSION_ID_BYTES_MAX: usize = 128;
const AUTHORIZATION_SUBJECT_INVALID: &str = "authorization_subject_invalid";
const AUTHORIZATION_SUBJECT_ALREADY_BOUND: &str = "authorization_subject_already_bound";

/// Owned execution subject carried through permission evaluation, durable audit preflight, grant,
/// and external-call binding. Global calls use `(None, None)`. A session is meaningful only inside
/// a workspace, so session-without-workspace is rejected before any persistence.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthorizationSubject {
    workspace_id: Option<String>,
    session_id: Option<String>,
}

impl AuthorizationSubject {
    pub fn global() -> Self {
        Self {
            workspace_id: None,
            session_id: None,
        }
    }

    pub fn try_new(
        workspace_id: Option<String>,
        session_id: Option<String>,
    ) -> anyhow::Result<Self> {
        if let Some(workspace_id) = workspace_id.as_deref() {
            validate_subject_component(workspace_id, AUTHORIZATION_WORKSPACE_ID_BYTES_MAX)?;
        }
        if let Some(session_id) = session_id.as_deref() {
            validate_subject_component(session_id, AUTHORIZATION_SESSION_ID_BYTES_MAX)?;
        }
        anyhow::ensure!(
            session_id.is_none() || workspace_id.is_some(),
            AUTHORIZATION_SUBJECT_INVALID
        );
        Ok(Self {
            workspace_id,
            session_id,
        })
    }

    pub fn workspace_id(&self) -> Option<&str> {
        self.workspace_id.as_deref()
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn is_global(&self) -> bool {
        self.workspace_id.is_none() && self.session_id.is_none()
    }
}

impl Default for AuthorizationSubject {
    fn default() -> Self {
        Self::global()
    }
}

impl std::fmt::Debug for AuthorizationSubject {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthorizationSubject")
            .field("has_workspace", &self.workspace_id.is_some())
            .field("has_session", &self.session_id.is_some())
            .finish()
    }
}

fn validate_subject_component(value: &str, max_bytes: usize) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value == value.trim()
            && value.len() <= max_bytes
            && !value.as_bytes().contains(&0),
        AUTHORIZATION_SUBJECT_INVALID
    );
    Ok(())
}

/// Pure permission/approval 결과를 durable preflight에 제출하는 계획.
/// Raw tool arguments와 secret은 의도적으로 보유하지 않는다.
pub struct AuthorizationPlan {
    operation_id: String,
    server_id: String,
    tool_name: String,
    decision: ToolDecision,
    live_schema_hash: String,
    expected_permission: PermissionFingerprint,
    subject: AuthorizationSubject,
    subject_bound: bool,
}

#[derive(PartialEq, Eq)]
pub(crate) struct AuthorizationBinding {
    pub(crate) server_id: String,
    pub(crate) tool_name: String,
    pub(crate) decision: ToolDecision,
    pub(crate) live_schema_hash: String,
    pub(crate) subject: AuthorizationSubject,
    input_digest: [u8; 32],
}

impl AuthorizationPlan {
    fn build(
        operation_id: String,
        server_id: String,
        tool_name: String,
        decision: ToolDecision,
        live_schema_hash: String,
        expected_permission: PermissionFingerprint,
    ) -> anyhow::Result<Self> {
        crate::log::validate_operation_id(&operation_id)?;
        anyhow::ensure!(!server_id.trim().is_empty(), "server id가 비어 있습니다");
        anyhow::ensure!(!tool_name.trim().is_empty(), "tool name이 비어 있습니다");
        anyhow::ensure!(
            valid_schema_hash(&live_schema_hash),
            "live schema hash는 64자리 sha256 hex여야 합니다"
        );
        Ok(Self {
            operation_id,
            server_id,
            tool_name,
            decision,
            live_schema_hash,
            expected_permission,
            subject: AuthorizationSubject::global(),
            subject_bound: false,
        })
    }

    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub fn decision(&self) -> ToolDecision {
        self.decision
    }

    pub fn live_schema_hash(&self) -> &str {
        &self.live_schema_hash
    }

    pub fn expected_permission(&self) -> &PermissionFingerprint {
        &self.expected_permission
    }

    pub fn subject(&self) -> &AuthorizationSubject {
        &self.subject
    }

    /// Rebinds the default global subject exactly once before durable preflight. Consuming `self`
    /// prevents aliases; the private bit prevents a returned plan from being rebound again.
    pub fn bind_subject(mut self, subject: AuthorizationSubject) -> anyhow::Result<Self> {
        anyhow::ensure!(!self.subject_bound, AUTHORIZATION_SUBJECT_ALREADY_BOUND);
        self.subject = subject;
        self.subject_bound = true;
        Ok(self)
    }

    pub fn is_allowed(&self) -> bool {
        self.decision.is_allowed()
    }

    pub(crate) fn binding(&self, input_json: &[u8]) -> AuthorizationBinding {
        use sha2::{Digest, Sha256};

        AuthorizationBinding {
            server_id: self.server_id.clone(),
            tool_name: self.tool_name.clone(),
            decision: self.decision,
            live_schema_hash: self.live_schema_hash.clone(),
            subject: self.subject.clone(),
            input_digest: Sha256::digest(input_json).into(),
        }
    }
}

impl std::fmt::Debug for AuthorizationPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorizationPlan")
            .field("decision", &self.decision)
            .field("state", &"planned")
            .finish()
    }
}

/// Approval UI/port가 opaque pending token을 resolve할 때 선택할 수 있는 결정만 표현한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    AllowOnce,
    AllowAlways,
    DenyOnce,
    DenyAlways,
}

impl ApprovalDecision {
    fn tool_decision(self) -> ToolDecision {
        match self {
            Self::AllowOnce => ToolDecision::AllowOnce,
            Self::AllowAlways => ToolDecision::AllowAlways,
            Self::DenyOnce => ToolDecision::DenyOnce,
            Self::DenyAlways => ToolDecision::DenyAlways,
        }
    }
}

/// Shared evaluation이 approval 필요 시에만 만드는 opaque, non-Clone pending token.
/// Raw input/preview/secret은 보유하지 않는다.
pub struct PendingAuthorization {
    operation_id: String,
    server_id: String,
    tool_name: String,
    live_schema_hash: String,
    reason: ApprovalReason,
    expected_permission: PermissionFingerprint,
    subject: AuthorizationSubject,
    subject_bound: bool,
}

impl PendingAuthorization {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub fn live_schema_hash(&self) -> &str {
        &self.live_schema_hash
    }

    pub fn reason(&self) -> ApprovalReason {
        self.reason
    }

    pub fn subject(&self) -> &AuthorizationSubject {
        &self.subject
    }

    pub fn bind_subject(mut self, subject: AuthorizationSubject) -> anyhow::Result<Self> {
        anyhow::ensure!(!self.subject_bound, AUTHORIZATION_SUBJECT_ALREADY_BOUND);
        self.subject = subject;
        self.subject_bound = true;
        Ok(self)
    }

    pub fn resolve(self, decision: ApprovalDecision) -> AuthorizationPlan {
        // Fields were validated by evaluate_authorization, so this cannot fail.
        AuthorizationPlan {
            operation_id: self.operation_id,
            server_id: self.server_id,
            tool_name: self.tool_name,
            decision: decision.tool_decision(),
            live_schema_hash: self.live_schema_hash,
            expected_permission: self.expected_permission,
            subject: self.subject,
            subject_bound: self.subject_bound,
        }
    }
}

impl std::fmt::Debug for PendingAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingAuthorization")
            .field("reason", &self.reason)
            .field("state", &"awaiting_approval")
            .finish()
    }
}

/// Structural shared authorization result. Plan/Pending 모두 public raw constructor가 없다.
pub enum AuthorizationEvaluation {
    Plan(AuthorizationPlan),
    NeedsApproval(PendingAuthorization),
}

impl AuthorizationEvaluation {
    /// Binds both immediate and approval-required outcomes before callers branch. This prevents an
    /// approval token created for one subject from being resolved and then reassigned to another.
    pub fn bind_subject(self, subject: AuthorizationSubject) -> anyhow::Result<Self> {
        match self {
            Self::Plan(plan) => plan.bind_subject(subject).map(Self::Plan),
            Self::NeedsApproval(pending) => pending.bind_subject(subject).map(Self::NeedsApproval),
        }
    }
}

impl std::fmt::Debug for AuthorizationEvaluation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plan(plan) => plan.fmt(f),
            Self::NeedsApproval(pending) => pending.fmt(f),
        }
    }
}

/// Durable audit preflight가 commit됐음을 증명하는 opaque single-use authorization token.
///
/// Public constructor가 없으며 private/non-Clone AuditOperation proof와 plan을 함께
/// consume해야만 생성된다. Raw tool arguments와 secret은 보유하지 않는다.
pub struct AuthorizationGrant {
    operation_id: String,
    server_id: String,
    tool_name: String,
    decision: ToolDecision,
    live_schema_hash: String,
    subject: AuthorizationSubject,
    input_digest: [u8; 32],
}

impl AuthorizationGrant {
    #[cfg(test)]
    fn from_preflight(
        plan: AuthorizationPlan,
        operation: crate::AuditOperation,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(plan.is_allowed(), "거부 plan은 grant를 만들 수 없습니다");
        anyhow::ensure!(
            operation.operation_id() == plan.operation_id,
            "preflight operation id와 authorization plan이 일치하지 않습니다"
        );
        anyhow::ensure!(
            operation.lifecycle() == crate::AuditLifecycle::Prepared,
            "preflight lifecycle과 authorization decision이 일치하지 않습니다"
        );
        let binding = operation
            .authorization_binding()
            .ok_or_else(|| anyhow::anyhow!("authorization binding이 없는 preflight proof"))?;
        anyhow::ensure!(
            binding.server_id == plan.server_id
                && binding.tool_name == plan.tool_name
                && binding.decision == plan.decision
                && binding.live_schema_hash == plan.live_schema_hash
                && binding.subject == plan.subject,
            "preflight proof와 authorization plan binding이 일치하지 않습니다"
        );
        Ok(Self {
            operation_id: plan.operation_id,
            server_id: plan.server_id,
            tool_name: plan.tool_name,
            decision: plan.decision,
            live_schema_hash: plan.live_schema_hash,
            subject: plan.subject,
            input_digest: binding.input_digest,
        })
    }

    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub fn decision(&self) -> ToolDecision {
        self.decision
    }

    pub fn live_schema_hash(&self) -> &str {
        &self.live_schema_hash
    }

    pub fn is_allowed(&self) -> bool {
        self.decision.is_allowed()
    }

    pub fn subject(&self) -> &AuthorizationSubject {
        &self.subject
    }

    /// Backward-compatible global call binding. A subject-bound grant cannot silently fall back to
    /// the legacy global path.
    pub fn bind_call(
        self,
        server_id: &str,
        tool_name: &str,
        input_json: &[u8],
    ) -> anyhow::Result<AuthorizedCall> {
        self.bind_call_for_subject(
            &AuthorizationSubject::global(),
            server_id,
            tool_name,
            input_json,
        )
    }

    /// Exact subject/server/tool/validated-input binding. The grant is consumed on success or
    /// mismatch, so a capability cannot be retried against a different workspace/session.
    pub fn bind_call_for_subject(
        self,
        subject: &AuthorizationSubject,
        server_id: &str,
        tool_name: &str,
        input_json: &[u8],
    ) -> anyhow::Result<AuthorizedCall> {
        use sha2::{Digest, Sha256};

        anyhow::ensure!(
            self.subject == *subject
                && self.server_id == server_id
                && self.tool_name == tool_name
                && self.input_digest == <[u8; 32]>::from(Sha256::digest(input_json)),
            "authorized call binding mismatch"
        );
        Ok(AuthorizedCall {
            operation_id: self.operation_id,
        })
    }
}

/// Durable Denied audit commit receipt. External-call 권한으로 사용할 수 없다.
pub struct DeniedAuthorization {
    operation_id: String,
    decision: ToolDecision,
}

/// Exact grant binding을 consume한 뒤 external executor만 받는 single-use call capability.
pub struct AuthorizedCall {
    operation_id: String,
}

impl AuthorizedCall {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
}

impl std::fmt::Debug for AuthorizedCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorizedCall")
            .field("state", &"exact_call_bound")
            .finish()
    }
}

impl DeniedAuthorization {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub fn decision(&self) -> ToolDecision {
        self.decision
    }

    #[cfg(test)]
    fn from_preflight(
        plan: AuthorizationPlan,
        operation: crate::AuditOperation,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !plan.is_allowed(),
            "허용 plan은 denied receipt를 만들 수 없습니다"
        );
        anyhow::ensure!(
            operation.operation_id() == plan.operation_id,
            "preflight operation id와 authorization plan이 일치하지 않습니다"
        );
        anyhow::ensure!(
            operation.lifecycle() == crate::AuditLifecycle::Denied,
            "거부 preflight lifecycle이 denied가 아닙니다"
        );
        let binding = operation
            .authorization_binding()
            .ok_or_else(|| anyhow::anyhow!("authorization binding이 없는 preflight proof"))?;
        anyhow::ensure!(
            binding.server_id == plan.server_id
                && binding.tool_name == plan.tool_name
                && binding.decision == plan.decision
                && binding.live_schema_hash == plan.live_schema_hash
                && binding.subject == plan.subject,
            "preflight proof와 authorization plan binding이 일치하지 않습니다"
        );
        Ok(Self {
            operation_id: plan.operation_id,
            decision: plan.decision,
        })
    }
}

impl std::fmt::Debug for DeniedAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeniedAuthorization")
            .field("decision", &self.decision)
            .field("state", &"denied_committed")
            .finish()
    }
}

/// Durable preflight result. Only Prepared carries an external-call grant.
pub enum AuthorizationPreflight {
    Prepared(AuthorizationGrant),
    Denied(DeniedAuthorization),
}

impl AuthorizationPreflight {
    pub(crate) fn from_committed_owned(
        plan: AuthorizationPlan,
        operation: crate::ValidatedOwnedAuthorizationOperation,
    ) -> Self {
        let (operation_id, lifecycle, binding) = operation.into_parts();
        debug_assert_eq!(operation_id, plan.operation_id);
        debug_assert_eq!(binding.server_id, plan.server_id);
        debug_assert_eq!(binding.tool_name, plan.tool_name);
        debug_assert_eq!(binding.decision, plan.decision);
        debug_assert_eq!(binding.live_schema_hash, plan.live_schema_hash);
        debug_assert_eq!(binding.subject, plan.subject);
        if plan.is_allowed() {
            debug_assert_eq!(lifecycle, crate::AuditLifecycle::Prepared);
            Self::Prepared(AuthorizationGrant {
                operation_id: plan.operation_id,
                server_id: plan.server_id,
                tool_name: plan.tool_name,
                decision: plan.decision,
                live_schema_hash: plan.live_schema_hash,
                subject: plan.subject,
                input_digest: binding.input_digest,
            })
        } else {
            debug_assert_eq!(lifecycle, crate::AuditLifecycle::Denied);
            Self::Denied(DeniedAuthorization {
                operation_id: plan.operation_id,
                decision: plan.decision,
            })
        }
    }

    #[cfg(test)]
    pub(crate) fn from_preflight(
        plan: AuthorizationPlan,
        operation: crate::AuditOperation,
    ) -> anyhow::Result<Self> {
        if plan.is_allowed() {
            AuthorizationGrant::from_preflight(plan, operation).map(Self::Prepared)
        } else {
            DeniedAuthorization::from_preflight(plan, operation).map(Self::Denied)
        }
    }
}

impl std::fmt::Debug for AuthorizationPreflight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Prepared(grant) => grant.fmt(f),
            Self::Denied(receipt) => receipt.fmt(f),
        }
    }
}

impl std::fmt::Debug for AuthorizationGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorizationGrant")
            .field("decision", &self.decision)
            .field("state", &"preflight_committed")
            .finish()
    }
}

/// dialog가 필요한 이유 (dialog 문구 분기용)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalReason {
    /// 규칙이 Ask(기본값)
    AskRule,
    /// Allow 규칙이지만 승인 이력이 없다 — 첫 사용은 승인부터
    FirstUse,
    /// 마지막 승인 때와 schema hash가 다르다 — 도구 스키마 변경, 재승인 필요
    SchemaChanged,
}

/// evaluate 결과 — 즉시 결정이거나 dialog가 필요하거나 둘 중 하나
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyEvaluation {
    Decided(ToolDecision),
    NeedsApproval(ApprovalReason),
}

/// 저장된 rule/hash와 live schema만으로 permission을 평가하는 공용 pure function.
/// GUI Connector와 mcp-proxy가 이 함수를 함께 사용해 동일한 fail-closed 규칙을 적용한다.
pub fn evaluate_permission(
    rule: PermissionRule,
    approved_schema_hash: Option<&str>,
    live_schema_hash: &str,
) -> anyhow::Result<PolicyEvaluation> {
    anyhow::ensure!(
        valid_schema_hash(live_schema_hash),
        "live schema hash는 64자리 sha256 hex여야 합니다"
    );
    Ok(match rule {
        PermissionRule::Deny => PolicyEvaluation::Decided(ToolDecision::PolicyDeny),
        PermissionRule::Ask => PolicyEvaluation::NeedsApproval(ApprovalReason::AskRule),
        PermissionRule::Allow => match approved_schema_hash {
            None => PolicyEvaluation::NeedsApproval(ApprovalReason::FirstUse),
            Some(approved)
                if valid_schema_hash(approved)
                    && approved.eq_ignore_ascii_case(live_schema_hash) =>
            {
                PolicyEvaluation::Decided(ToolDecision::PolicyAllow)
            }
            _ => PolicyEvaluation::NeedsApproval(ApprovalReason::SchemaChanged),
        },
    })
}

/// GUI Connector와 proxy가 공통으로 쓰는 유일한 AuthorizationPlan mint.
/// live schema를 먼저 검증한 뒤 pure permission 결과를 opaque plan/pending으로 봉인한다.
pub fn evaluate_authorization(
    operation_id: String,
    server_id: String,
    tool_name: String,
    rule: PermissionRule,
    approved_schema_hash: Option<&str>,
    live_schema_hash: String,
) -> anyhow::Result<AuthorizationEvaluation> {
    evaluate_authorization_with_fingerprint(
        operation_id,
        server_id,
        tool_name,
        PermissionFingerprint::Persisted {
            rule,
            approved_schema_hash: approved_schema_hash.map(str::to_owned),
        },
        live_schema_hash,
    )
}

pub fn evaluate_authorization_with_fingerprint(
    operation_id: String,
    server_id: String,
    tool_name: String,
    permission: PermissionFingerprint,
    live_schema_hash: String,
) -> anyhow::Result<AuthorizationEvaluation> {
    crate::log::validate_operation_id(&operation_id)?;
    anyhow::ensure!(!server_id.trim().is_empty(), "server id가 비어 있습니다");
    anyhow::ensure!(!tool_name.trim().is_empty(), "tool name이 비어 있습니다");
    let (rule, approved_schema_hash) = permission.evaluation();
    match evaluate_permission(rule, approved_schema_hash, &live_schema_hash)? {
        PolicyEvaluation::Decided(decision) => AuthorizationPlan::build(
            operation_id,
            server_id,
            tool_name,
            decision,
            live_schema_hash,
            permission,
        )
        .map(AuthorizationEvaluation::Plan),
        PolicyEvaluation::NeedsApproval(reason) => Ok(AuthorizationEvaluation::NeedsApproval(
            PendingAuthorization {
                operation_id,
                server_id,
                tool_name,
                live_schema_hash,
                reason,
                expected_permission: permission,
                subject: AuthorizationSubject::global(),
                subject_bound: false,
            },
        )),
    }
}

fn valid_schema_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// tool_name → Allow/Deny/Ask 규칙 집합. 기본값은 Ask.
/// Allow는 승인 이력(schema hash)이 있어야만 자동 통과한다 —
/// set_rule로 직접 Allow를 지정해도 첫 호출은 dialog를 거치고,
/// AllowAlways 승인이 hash를 고정한 뒤부터 자동 허용된다.
#[derive(Debug, Clone, Default)]
pub struct PermissionPolicy {
    rules: HashMap<(String, String), PermissionRule>,
    /// (server_id, tool_name) → 마지막 승인 때의 schema hash
    approved_schema: HashMap<(String, String), String>,
}

impl PermissionPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    /// 설정 화면 등에서 규칙을 직접 지정한다. 승인 이력은 만들지 않으며,
    /// 규칙이 실제로 바뀌면 과거 승인 이력(schema hash)도 무효화한다 —
    /// Deny/Ask를 거쳐 Allow로 돌아와도 옛 승인이 되살아나지 않고
    /// 재승인(FirstUse)부터 시작한다 (codex 리뷰 반영).
    pub fn set_rule(&mut self, server_id: &str, tool_name: &str, rule: PermissionRule) {
        let key = key(server_id, tool_name);
        if self.rules.insert(key.clone(), rule) != Some(rule) {
            self.approved_schema.remove(&key);
        }
    }

    pub fn rule(&self, server_id: &str, tool_name: &str) -> PermissionRule {
        self.rules
            .get(&key(server_id, tool_name))
            .copied()
            .unwrap_or_default()
    }

    /// 마지막 승인 때의 schema hash (영속 저장용).
    pub fn approved_hash(&self, server_id: &str, tool_name: &str) -> Option<&str> {
        self.approved_schema
            .get(&key(server_id, tool_name))
            .map(String::as_str)
    }

    /// DB에서 로드한 규칙을 복원한다. set_rule과 달리 change-시-clear를 하지 않고
    /// 승인 이력(hash)도 함께 설정한다. hash는 유효한 sha256 hex(64자)만 신뢰한다
    /// (fail-closed — 기형 hash가 영구 승인이 되면 재승인 트리거 무력화).
    pub fn load_rule(
        &mut self,
        server_id: &str,
        tool_name: &str,
        rule: PermissionRule,
        approved_hash: Option<String>,
    ) {
        let key = key(server_id, tool_name);
        self.rules.insert(key.clone(), rule);
        match approved_hash {
            Some(hash) if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) => {
                self.approved_schema.insert(key, hash);
            }
            _ => {
                self.approved_schema.remove(&key);
            }
        }
    }

    /// 요청 평가 — 즉시 결정(Decided) 또는 approval dialog 필요(NeedsApproval).
    pub fn evaluate(&self, request: &ToolApprovalRequest) -> PolicyEvaluation {
        let key = key(&request.server_id, &request.tool_name);
        evaluate_permission(
            self.rules.get(&key).copied().unwrap_or_default(),
            self.approved_schema.get(&key).map(String::as_str),
            &request.schema_hash,
        )
        .unwrap_or(PolicyEvaluation::NeedsApproval(
            ApprovalReason::SchemaChanged,
        ))
    }

    /// dialog 결과 반영. Always 계열만 규칙·승인 이력을 바꾸고 Once 계열은 상태 불변.
    pub fn apply_decision(&mut self, request: &ToolApprovalRequest, decision: ToolDecision) {
        let key = key(&request.server_id, &request.tool_name);
        match decision {
            ToolDecision::AllowAlways => {
                self.rules.insert(key.clone(), PermissionRule::Allow);
                // schema_hash는 이 crate의 schema_hash()가 만든 sha256 hex(64자)만
                // 승인 토큰으로 신뢰한다 — 빈/기형 문자열이 영구 승인 키가 되면
                // 재승인 트리거가 무력화된다 (codex 리뷰, fail-closed)
                let hash = &request.schema_hash;
                if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
                    self.approved_schema.insert(key, hash.clone());
                } else {
                    self.approved_schema.remove(&key);
                    tracing::warn!(
                        tool = %request.tool_name,
                        "기형 schema_hash — 승인 이력을 남기지 않음 (매번 재승인)"
                    );
                }
            }
            ToolDecision::DenyAlways => {
                self.rules.insert(key.clone(), PermissionRule::Deny);
                // 항상 거부로 바꾸면 과거 승인 이력도 무효 — 이후 다시 Allow로
                // 돌려도 재승인(FirstUse)부터 시작한다
                self.approved_schema.remove(&key);
            }
            // Once 계열과 자동 결정(PolicyAllow/PolicyDeny)은 상태를 바꾸지 않는다
            _ => {}
        }
    }
}

fn key(server_id: &str, tool_name: &str) -> (String, String) {
    (server_id.to_owned(), tool_name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn automatic_allow_plan(operation_id: &str) -> AuthorizationPlan {
        let hash = crate::schema_hash("subject-schema");
        let AuthorizationEvaluation::Plan(plan) = evaluate_authorization(
            operation_id.to_owned(),
            "server".to_owned(),
            "tool".to_owned(),
            PermissionRule::Allow,
            Some(&hash),
            hash.clone(),
        )
        .unwrap() else {
            panic!("matching Allow fingerprint must create a plan")
        };
        plan
    }

    #[test]
    fn authorization_subject는_owned_bounded_nul_free이고_debug가_id를_숨긴다() {
        const MARKER: &str = "workspace-marker-never-debug";
        let subject = AuthorizationSubject::try_new(
            Some(MARKER.to_owned()),
            Some("session-marker-never-debug".to_owned()),
        )
        .unwrap();
        assert_eq!(subject.workspace_id(), Some(MARKER));
        assert_eq!(subject.session_id(), Some("session-marker-never-debug"));
        assert_eq!(
            format!("{subject:?}"),
            "AuthorizationSubject { has_workspace: true, has_session: true }"
        );
        assert!(!format!("{subject:?}").contains(MARKER));

        assert!(
            AuthorizationSubject::try_new(
                Some("w".repeat(AUTHORIZATION_WORKSPACE_ID_BYTES_MAX)),
                Some("s".repeat(AUTHORIZATION_SESSION_ID_BYTES_MAX)),
            )
            .is_ok()
        );
        for invalid in [
            AuthorizationSubject::try_new(Some(String::new()), None),
            AuthorizationSubject::try_new(Some(" leading".to_owned()), None),
            AuthorizationSubject::try_new(Some("nul\0workspace".to_owned()), None),
            AuthorizationSubject::try_new(
                Some("w".repeat(AUTHORIZATION_WORKSPACE_ID_BYTES_MAX + 1)),
                None,
            ),
            AuthorizationSubject::try_new(None, Some("orphan-session".to_owned())),
        ] {
            assert_eq!(
                invalid.unwrap_err().to_string(),
                AUTHORIZATION_SUBJECT_INVALID
            );
        }
    }

    #[test]
    fn authorization_plan은_global_호환을_유지하고_subject를_정확히_한번만_bind한다() {
        let global = automatic_allow_plan("subject-global");
        assert!(global.subject().is_global());

        let subject =
            AuthorizationSubject::try_new(Some("workspace".to_owned()), Some("7".to_owned()))
                .unwrap();
        let bound = automatic_allow_plan("subject-bound")
            .bind_subject(subject.clone())
            .unwrap();
        assert_eq!(bound.subject(), &subject);
        assert_eq!(
            bound
                .bind_subject(AuthorizationSubject::global())
                .unwrap_err()
                .to_string(),
            AUTHORIZATION_SUBJECT_ALREADY_BOUND
        );

        let AuthorizationEvaluation::NeedsApproval(pending) = evaluate_authorization(
            "subject-pending".to_owned(),
            "server".to_owned(),
            "tool".to_owned(),
            PermissionRule::Ask,
            None,
            crate::schema_hash("subject-schema"),
        )
        .unwrap() else {
            panic!("Ask must create pending authorization")
        };
        assert!(pending.subject().is_global());

        let AuthorizationEvaluation::NeedsApproval(pending) = evaluate_authorization(
            "subject-pending-bound".to_owned(),
            "server".to_owned(),
            "tool".to_owned(),
            PermissionRule::Ask,
            None,
            crate::schema_hash("subject-schema"),
        )
        .unwrap()
        .bind_subject(subject.clone())
        .unwrap() else {
            panic!("Ask must remain pending after subject bind")
        };
        assert_eq!(pending.subject(), &subject);
        let resolved = pending.resolve(ApprovalDecision::AllowOnce);
        assert_eq!(resolved.subject(), &subject);
        assert_eq!(
            resolved
                .bind_subject(AuthorizationSubject::global())
                .unwrap_err()
                .to_string(),
            AUTHORIZATION_SUBJECT_ALREADY_BOUND
        );
    }

    /// 라벨("hash-a")을 실제 형식(sha256 hex 64자)으로 변환 — fail-closed 검증 통과용.
    fn request(server_id: &str, tool_name: &str, schema_hash: &str) -> ToolApprovalRequest {
        let schema_hash = &crate::schema_hash(schema_hash);
        ToolApprovalRequest {
            server_id: server_id.into(),
            tool_name: tool_name.into(),
            input_json: r#"{"path":"/tmp/x"}"#.into(),
            schema_hash: schema_hash.into(),
        }
    }

    #[test]
    fn load_rule은_규칙과_승인이력을_복원한다() {
        let mut policy = PermissionPolicy::new();
        let hash = crate::schema_hash("schema-x");
        policy.load_rule("srv", "tool", PermissionRule::Allow, Some(hash.clone()));
        // 로드된 Allow + 일치 hash → 자동 허용 (재승인 불필요)
        let req = ToolApprovalRequest {
            server_id: "srv".into(),
            tool_name: "tool".into(),
            input_json: "{}".into(),
            schema_hash: hash,
        };
        assert_eq!(
            policy.evaluate(&req),
            PolicyEvaluation::Decided(ToolDecision::PolicyAllow)
        );
        assert_eq!(policy.rule("srv", "tool"), PermissionRule::Allow);
        // 기형 hash는 승인 이력으로 저장 안 됨 (fail-closed)
        policy.load_rule("srv", "t2", PermissionRule::Allow, Some("short".into()));
        assert_eq!(policy.approved_hash("srv", "t2"), None);
    }

    #[test]
    fn 기형_schema_hash는_영구_승인이_되지_않는다() {
        // fail-closed (codex 리뷰): 빈/짧은 hash로 AllowAlways 해도 이력이 안 남아
        // 다음 요청은 다시 승인을 요구한다
        let mut policy = PermissionPolicy::new();
        let mut req = request("srv-1", "read_file", "hash-a");
        req.schema_hash = String::new(); // 기형
        policy.apply_decision(&req, ToolDecision::AllowAlways);
        assert!(matches!(
            policy.evaluate(&req),
            PolicyEvaluation::NeedsApproval(_)
        ));
    }

    #[test]
    fn 기본값은_ask() {
        let policy = PermissionPolicy::new();
        let req = request("srv-1", "read_file", "hash-a");
        assert_eq!(policy.rule("srv-1", "read_file"), PermissionRule::Ask);
        assert_eq!(
            policy.evaluate(&req),
            PolicyEvaluation::NeedsApproval(ApprovalReason::AskRule)
        );
    }

    #[test]
    fn deny_규칙은_즉시_거부() {
        let mut policy = PermissionPolicy::new();
        policy.set_rule("srv-1", "delete_file", PermissionRule::Deny);
        let req = request("srv-1", "delete_file", "hash-a");
        assert_eq!(
            policy.evaluate(&req),
            PolicyEvaluation::Decided(ToolDecision::PolicyDeny)
        );
    }

    #[test]
    fn allow_always_이후_같은_스키마는_자동_허용() {
        let mut policy = PermissionPolicy::new();
        let req = request("srv-1", "read_file", "hash-a");
        policy.apply_decision(&req, ToolDecision::AllowAlways);
        assert_eq!(policy.rule("srv-1", "read_file"), PermissionRule::Allow);
        assert_eq!(
            policy.evaluate(&req),
            PolicyEvaluation::Decided(ToolDecision::PolicyAllow)
        );
    }

    #[test]
    fn 스키마_변경_시_재승인() {
        let mut policy = PermissionPolicy::new();
        let req = request("srv-1", "read_file", "hash-a");
        policy.apply_decision(&req, ToolDecision::AllowAlways);
        // 같은 tool, 다른 schema hash → 재승인 (PR-22 "MCP tool 변경 시 재승인")
        let changed = request("srv-1", "read_file", "hash-b");
        assert_eq!(
            policy.evaluate(&changed),
            PolicyEvaluation::NeedsApproval(ApprovalReason::SchemaChanged)
        );
        // 재승인하면 새 hash로 갱신되어 다시 자동 허용
        policy.apply_decision(&changed, ToolDecision::AllowAlways);
        assert_eq!(
            policy.evaluate(&changed),
            PolicyEvaluation::Decided(ToolDecision::PolicyAllow)
        );
    }

    #[test]
    fn 직접_지정한_allow는_첫_사용에_승인_필요() {
        let mut policy = PermissionPolicy::new();
        policy.set_rule("srv-1", "read_file", PermissionRule::Allow);
        let req = request("srv-1", "read_file", "hash-a");
        assert_eq!(
            policy.evaluate(&req),
            PolicyEvaluation::NeedsApproval(ApprovalReason::FirstUse)
        );
    }

    #[test]
    fn once_계열은_상태를_바꾸지_않는다() {
        let mut policy = PermissionPolicy::new();
        let req = request("srv-1", "read_file", "hash-a");
        policy.apply_decision(&req, ToolDecision::AllowOnce);
        policy.apply_decision(&req, ToolDecision::DenyOnce);
        assert_eq!(policy.rule("srv-1", "read_file"), PermissionRule::Ask);
        assert_eq!(
            policy.evaluate(&req),
            PolicyEvaluation::NeedsApproval(ApprovalReason::AskRule)
        );
    }

    #[test]
    fn deny_always는_승인_이력을_무효화한다() {
        let mut policy = PermissionPolicy::new();
        let req = request("srv-1", "read_file", "hash-a");
        policy.apply_decision(&req, ToolDecision::AllowAlways);
        policy.apply_decision(&req, ToolDecision::DenyAlways);
        assert_eq!(
            policy.evaluate(&req),
            PolicyEvaluation::Decided(ToolDecision::PolicyDeny)
        );
        // Deny → Allow로 되돌려도 과거 이력이 아닌 재승인부터
        policy.set_rule("srv-1", "read_file", PermissionRule::Allow);
        assert_eq!(
            policy.evaluate(&req),
            PolicyEvaluation::NeedsApproval(ApprovalReason::FirstUse)
        );
    }

    #[test]
    fn 규칙_변경은_승인_이력을_무효화하고_같은_규칙_재지정은_유지한다() {
        let mut policy = PermissionPolicy::new();
        let req = request("srv-1", "read_file", "hash-a");
        policy.apply_decision(&req, ToolDecision::AllowAlways);
        // 같은 규칙(Allow) 재지정은 승인 이력 유지 — 설정 재저장에 안전
        policy.set_rule("srv-1", "read_file", PermissionRule::Allow);
        assert_eq!(
            policy.evaluate(&req),
            PolicyEvaluation::Decided(ToolDecision::PolicyAllow)
        );
        // Ask를 거쳐 Allow로 돌아오면 옛 승인이 되살아나지 않는다
        policy.set_rule("srv-1", "read_file", PermissionRule::Ask);
        policy.set_rule("srv-1", "read_file", PermissionRule::Allow);
        assert_eq!(
            policy.evaluate(&req),
            PolicyEvaluation::NeedsApproval(ApprovalReason::FirstUse)
        );
    }

    #[test]
    fn 규칙은_서버와_tool_이름_쌍_단위() {
        let mut policy = PermissionPolicy::new();
        let req = request("srv-1", "read_file", "hash-a");
        policy.apply_decision(&req, ToolDecision::AllowAlways);
        // 다른 서버의 동명 tool은 별개 — 기본값 Ask
        let other = request("srv-2", "read_file", "hash-a");
        assert_eq!(
            policy.evaluate(&other),
            PolicyEvaluation::NeedsApproval(ApprovalReason::AskRule)
        );
    }

    #[test]
    fn decision_문자열과_허용_여부() {
        assert_eq!(ToolDecision::PolicyAllow.as_str(), "policy_allow");
        assert_eq!(ToolDecision::AllowAlways.as_str(), "allow_always");
        assert_eq!(ToolDecision::DenyOnce.as_str(), "deny_once");
        assert!(ToolDecision::AllowOnce.is_allowed());
        assert!(!ToolDecision::PolicyDeny.is_allowed());
        assert!(!ToolDecision::DenyAlways.is_allowed());
    }

    #[test]
    fn debug_출력에_input_평문이_없다() {
        let req = request("srv-1", "read_file", "hash-a");
        let debug = format!("{req:?}");
        assert!(!debug.contains("/tmp/x"), "{debug}");
    }

    #[test]
    fn shared_evaluation은_모든_rule에서_유효한_live_hash를_요구한다() {
        for rule in [
            PermissionRule::Allow,
            PermissionRule::Deny,
            PermissionRule::Ask,
        ] {
            assert!(evaluate_permission(rule, None, "").is_err());
            assert!(evaluate_permission(rule, None, "malformed").is_err());
        }
    }

    #[test]
    fn plan은_shared_evaluation또는_opaque_pending_resolve로만_생성된다() {
        let hash = crate::schema_hash("schema");
        let AuthorizationEvaluation::NeedsApproval(pending) = evaluate_authorization(
            "op-needs-approval".to_owned(),
            "srv".to_owned(),
            "tool".to_owned(),
            PermissionRule::Ask,
            None,
            hash,
        )
        .unwrap() else {
            panic!("Ask must create opaque pending")
        };
        assert_eq!(pending.reason(), ApprovalReason::AskRule);
        let plan = pending.resolve(ApprovalDecision::AllowOnce);
        assert_eq!(plan.decision(), ToolDecision::AllowOnce);

        let source = include_str!("policy.rs");
        let plan_impl = source
            .split("impl AuthorizationPlan")
            .nth(1)
            .unwrap()
            .split("impl std::fmt::Debug for AuthorizationPlan")
            .next()
            .unwrap();
        assert!(!plan_impl.contains("pub fn new("), "{plan_impl}");
        assert!(!plan_impl.contains("pub fn from_"), "{plan_impl}");
    }

    #[test]
    fn plan_debug는_식별자와_schema를_숨긴다() {
        let hash = crate::schema_hash("schema-secret-marker");
        let AuthorizationEvaluation::NeedsApproval(pending) = evaluate_authorization(
            "op-secret-marker".to_owned(),
            "server-secret-marker".to_owned(),
            "tool-secret-marker".to_owned(),
            PermissionRule::Ask,
            None,
            hash.clone(),
        )
        .unwrap() else {
            panic!("Ask must create pending")
        };
        let plan = pending.resolve(ApprovalDecision::AllowOnce);
        let debug = format!("{plan:?}");
        for hidden in [
            "op-secret-marker",
            "server-secret-marker",
            "tool-secret-marker",
            hash.as_str(),
        ] {
            assert!(!debug.contains(hidden), "{debug}");
        }
    }
}
