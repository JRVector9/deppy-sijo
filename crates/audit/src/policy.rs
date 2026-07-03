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

/// approval dialog가 표시할 요청 model (dialog UI 자체는 crates/app 소관).
#[derive(Clone, PartialEq, Eq)]
pub struct ToolApprovalRequest {
    pub server_id: String,
    pub tool_name: String,
    /// tool input JSON 평문 — dialog 표시용. 영속은 record_audit이 redact 후에만 한다.
    pub input_json: String,
    /// 호출 시점 tool input schema의 해시 (crate::schema_hash)
    pub schema_hash: String,
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

    /// 요청 평가 — 즉시 결정(Decided) 또는 approval dialog 필요(NeedsApproval).
    pub fn evaluate(&self, request: &ToolApprovalRequest) -> PolicyEvaluation {
        let key = key(&request.server_id, &request.tool_name);
        match self.rules.get(&key).copied().unwrap_or_default() {
            PermissionRule::Deny => PolicyEvaluation::Decided(ToolDecision::PolicyDeny),
            PermissionRule::Ask => PolicyEvaluation::NeedsApproval(ApprovalReason::AskRule),
            PermissionRule::Allow => match self.approved_schema.get(&key) {
                None => PolicyEvaluation::NeedsApproval(ApprovalReason::FirstUse),
                Some(hash) if *hash != request.schema_hash => {
                    PolicyEvaluation::NeedsApproval(ApprovalReason::SchemaChanged)
                }
                Some(_) => PolicyEvaluation::Decided(ToolDecision::PolicyAllow),
            },
        }
    }

    /// dialog 결과 반영. Always 계열만 규칙·승인 이력을 바꾸고 Once 계열은 상태 불변.
    pub fn apply_decision(&mut self, request: &ToolApprovalRequest, decision: ToolDecision) {
        let key = key(&request.server_id, &request.tool_name);
        match decision {
            ToolDecision::AllowAlways => {
                self.rules.insert(key.clone(), PermissionRule::Allow);
                self.approved_schema
                    .insert(key, request.schema_hash.clone());
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

    fn request(server_id: &str, tool_name: &str, schema_hash: &str) -> ToolApprovalRequest {
        ToolApprovalRequest {
            server_id: server_id.into(),
            tool_name: tool_name.into(),
            input_json: r#"{"path":"/tmp/x"}"#.into(),
            schema_hash: schema_hash.into(),
        }
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
}
