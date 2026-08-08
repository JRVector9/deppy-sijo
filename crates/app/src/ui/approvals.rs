//! 도구 실행 승인 팝업 (agent-proxy option 1.5, GUI 승인 측).
//! 별도 `deppy-mcp-proxy` 프로세스가 Allow/Deny 규칙이 없는 도구 호출을 만나면
//! 공유 DB에 pending 행을 쓴다. GUI 합성 루트는 그 목록을 읽어 bounded UI DTO로
//! 투영해(App::logic) 여기로 넘기고,
//! 이 모듈이 모달식 창을 띄워 사용자의 허용/거부 결정을 돌려준다. 되쓰기(resolve)는
//! 호출측(App controller)이 bounded intent로 처리한다.
//! http 서버의 호출이면 원격 url을 함께 고지한다 (H3 리뷰 P1 — proxy 경유 경로는
//! Connector Center의 신뢰 모달을 거치지 않으므로 첫 Ask 승인이 원격 전송 고지를 겸한다).

use std::sync::Arc;

/// 한 snapshot에 보존할 수 있는 승인 카드 수. Durable inbox의 hard limit와 같되 이
/// leaf가 저장소 상수에 의존하지 않고 자체 계약을 검증한다.
pub const APPROVAL_ITEM_LIMIT: usize = 256;
/// 승인 snapshot 전체의 문자열 보존 예산.
pub const APPROVAL_RETAINED_BYTES_LIMIT: usize = 1024 * 1024;

const APPROVAL_ID_BYTES_LIMIT: usize = 128;
const APPROVAL_SERVER_ID_BYTES_LIMIT: usize = 256;
const APPROVAL_TOOL_NAME_BYTES_LIMIT: usize = 4 * 1024;
const APPROVAL_PREVIEW_BYTES_LIMIT: usize = 2 * 1024;
const APPROVAL_SESSION_KEY_BYTES_LIMIT: usize = 57;
const APPROVAL_REMOTE_URL_BYTES_LIMIT: usize = 8 * 1024;

/// 합성 루트에서 storage row를 투영할 때 반환되는 low-cardinality 오류 코드.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDataErrorCode {
    InvalidId,
    InvalidServerId,
    InvalidToolName,
    InvalidPreview,
    InvalidSessionKey,
    InvalidRemoteUrl,
    ItemLimit,
    RetainedBytesLimit,
}

/// 렌더가 필요로 하는 값만 가진 immutable approval DTO.
///
/// 모든 필드는 private이며 생성자에서 바이트 상한을 검증한다. Preview와 경로성 값이
/// 진단 출력으로 새지 않도록 Debug는 내용 전체를 숨긴다.
#[derive(PartialEq, Eq)]
pub struct PendingApprovalItem {
    id: String,
    server_id: String,
    tool_name: String,
    arguments_preview: String,
    session_key: Option<String>,
    remote_url: Option<Arc<str>>,
    /// 승인이 만들어진 시각(unix 초, DB `pending_approvals.created_at`). 이 값이 곧
    /// "얼마나 나를 막고 있나"라 정렬·표시에 쓴다. 2026-08-08까지는 DB에 있는데도
    /// 투영에서 버려지고 있었다.
    created_at: i64,
}

impl std::fmt::Debug for PendingApprovalItem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PendingApprovalItem { REDACTED }")
    }
}

impl PendingApprovalItem {
    pub fn try_new(
        id: String,
        server_id: String,
        tool_name: String,
        arguments_preview: String,
        session_key: Option<String>,
        remote_url: Option<Arc<str>>,
        created_at: i64,
    ) -> Result<Self, ApprovalDataErrorCode> {
        validate_required(
            &id,
            APPROVAL_ID_BYTES_LIMIT,
            ApprovalDataErrorCode::InvalidId,
        )?;
        validate_required(
            &server_id,
            APPROVAL_SERVER_ID_BYTES_LIMIT,
            ApprovalDataErrorCode::InvalidServerId,
        )?;
        validate_required(
            &tool_name,
            APPROVAL_TOOL_NAME_BYTES_LIMIT,
            ApprovalDataErrorCode::InvalidToolName,
        )?;
        validate_optional_or_empty(
            &arguments_preview,
            APPROVAL_PREVIEW_BYTES_LIMIT,
            ApprovalDataErrorCode::InvalidPreview,
        )?;
        validate_optional(
            session_key.as_deref(),
            APPROVAL_SESSION_KEY_BYTES_LIMIT,
            ApprovalDataErrorCode::InvalidSessionKey,
        )?;
        validate_optional(
            remote_url.as_deref(),
            APPROVAL_REMOTE_URL_BYTES_LIMIT,
            ApprovalDataErrorCode::InvalidRemoteUrl,
        )?;
        Ok(Self {
            id,
            server_id,
            tool_name,
            arguments_preview,
            session_key,
            remote_url,
            created_at,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub fn arguments_preview(&self) -> &str {
        &self.arguments_preview
    }

    pub fn session_key(&self) -> Option<&str> {
        self.session_key.as_deref()
    }

    pub fn remote_url(&self) -> Option<&str> {
        self.remote_url.as_deref()
    }

    /// 승인이 만들어진 시각(unix 초) — 막힌 시간 계산의 기준.
    pub fn created_at(&self) -> i64 {
        self.created_at
    }

    fn retained_bytes(&self) -> usize {
        self.id.len()
            + self.server_id.len()
            + self.tool_name.len()
            + self.arguments_preview.len()
            + self.session_key.as_ref().map_or(0, String::len)
            + self.remote_url.as_ref().map_or(0, |url| url.len())
    }
}

fn validate_required(
    value: &str,
    limit: usize,
    error: ApprovalDataErrorCode,
) -> Result<(), ApprovalDataErrorCode> {
    if value.is_empty() || value.len() > limit || value.contains('\0') {
        return Err(error);
    }
    Ok(())
}

fn validate_optional(
    value: Option<&str>,
    limit: usize,
    error: ApprovalDataErrorCode,
) -> Result<(), ApprovalDataErrorCode> {
    match value {
        Some(value) => validate_required(value, limit, error),
        None => Ok(()),
    }
}

fn validate_optional_or_empty(
    value: &str,
    limit: usize,
    error: ApprovalDataErrorCode,
) -> Result<(), ApprovalDataErrorCode> {
    if value.len() > limit || value.contains('\0') {
        return Err(error);
    }
    Ok(())
}

/// 사용자가 팝업에서 내린 결정. remember는 proxy 측이 permission 규칙으로 영속화한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalDecision {
    pub id: String,
    pub allowed: bool,
    pub remember: bool,
}

pub struct ApprovalsUi {
    /// pending 승인 행들 (오래된 순 — DB가 정렬해 준다). 맨 앞 하나만 표시한다.
    pending: Arc<[PendingApprovalItem]>,
    /// 현재(맨 앞) 항목의 "이 도구 기억" 체크 상태. 항목이 바뀌면 리셋한다.
    remember: bool,
    /// remember 리셋 판단용 — 마지막으로 표시한 항목 id.
    current_id: Option<String>,
}

impl ApprovalsUi {
    pub fn new() -> Self {
        Self {
            pending: Arc::from([]),
            remember: false,
            current_id: None,
        }
    }

    /// 폴링으로 새로 읽은 pending 목록을 반영한다 (App::logic에서 호출).
    /// 맨 앞 항목이 바뀌면 이전 항목의 체크 상태가 새 항목에 새지 않도록 remember를 리셋한다.
    /// 각 DTO의 `remote_url`은 http 서버의 원격 전송 고지용이다. 상한을 벗어난 입력은
    /// stale 승인 버튼을 남기지 않도록 기존 snapshot까지 비우고 fail-closed한다.
    pub fn set_pending(
        &mut self,
        rows: Vec<PendingApprovalItem>,
    ) -> Result<(), ApprovalDataErrorCode> {
        let retained_bytes = rows
            .iter()
            .try_fold(0usize, |total, row| total.checked_add(row.retained_bytes()));
        if rows.len() > APPROVAL_ITEM_LIMIT {
            self.clear_pending();
            return Err(ApprovalDataErrorCode::ItemLimit);
        }
        if retained_bytes.is_none_or(|total| total > APPROVAL_RETAINED_BYTES_LIMIT) {
            self.clear_pending();
            return Err(ApprovalDataErrorCode::RetainedBytesLimit);
        }
        let front = rows.first().map(|row| row.id().to_owned());
        if front != self.current_id {
            self.remember = false;
            self.current_id = front;
        }
        self.pending = Arc::from(rows);
        Ok(())
    }

    fn clear_pending(&mut self) {
        self.pending = Arc::from([]);
        self.remember = false;
        self.current_id = None;
    }

    /// 벨 인박스 카드용 읽기 전용 접근자 (v3.9 N2). 이미 폴링된 목록을 그대로 노출한다 —
    /// 인박스도 새 host 조회를 만들지 않고 이 값을 재사용한다.
    pub fn pending(&self) -> &[PendingApprovalItem] {
        &self.pending
    }

    /// 상태바 승인 팝오버용. App이 `&mut self`로 상태바를 그리는 동안에도 목록을
    /// 들고 있어야 해서 빌림 대신 `Arc`를 복제한다 — 항목 자체는 복사되지 않는다.
    pub fn pending_shared(&self) -> Arc<[PendingApprovalItem]> {
        Arc::clone(&self.pending)
    }

    /// 버튼 클릭 → 결정 매핑 (egui 컨텍스트 없이 테스트할 수 있게 분리).
    /// 맨 앞(가장 오래된) 항목 하나에 대해서만 결정을 만든다.
    #[cfg_attr(not(test), allow(dead_code))]
    fn decide(&self, allowed: bool) -> Option<ApprovalDecision> {
        self.pending.first().map(|row| ApprovalDecision {
            id: row.id().to_owned(),
            allowed,
            remember: self.remember,
        })
    }

    /// pending 항목이 있으면 모달식 창을 그린다. 버튼을 누르면 그 결정을 돌려준다.
    /// 한 번에 하나(가장 오래된 것)만 — 해소 후 목록을 갱신하면 다음 항목이 뜬다.
    ///
    /// **v3.9 N4부터 호출되지 않는다** — 승인은 벨 팝오버 인박스가 처리한다(전역 대기를
    /// 한 곳에서, 워크스페이스 전환 없이). 이 위젯은 되돌릴 수 있게 남겨 둔 것이다:
    /// 강제 팝업이 필요하다고 판단되면 app.rs의 호출 한 줄을 되살리면 된다. `pending`
    /// 목록 자체는 인박스 카드의 소스로 계속 쓰인다.
    ///
    /// 창이 숨겨져 있으면 ui()가 실행되지 않아 이 팝업도 안 뜬다 — 사용자가 앱을
    /// 전면으로 가져와야 승인할 수 있다(그동안 proxy는 타임아웃까지 폴링). 의도된 동작.
    #[allow(dead_code, reason = "N4에서 호출 중단 — 되살릴 수 있게 보존 (위 주석)")]
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        catalog: &i18n::Catalog,
    ) -> Option<ApprovalDecision> {
        let row = self.pending.first()?;
        let row_id = row.id();
        let mut decision = None;
        egui::Window::new(catalog.t("approval.title", &[]))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(catalog.t("approval.server", &[("value", row.server_id())]));
                ui.label(catalog.t("approval.tool", &[("value", row.tool_name())]));
                // 어느 세션의 승인인지는 여기서 안 보인다 — 세션명은 DB가 아니라 런타임
                // 상태에서 와야 하는데(2026-07-17: pane_id↔mux_panes 조인이 매칭되지 않아
                // 늘 비어 있었다), 이 모달은 N4에서 호출을 접었으므로 배선을 되살리지
                // 않았다. 세션 맥락이 필요하면 벨 인박스가 보여준다(에이전트명까지).
                // http 서버면 원격 전송 고지 — 이 승인이 신뢰 확인을 겸한다 (H3 리뷰 P1)
                if let Some(url) = row.remote_url() {
                    // 주의 톤은 agent_visuals가 소유한다 — 일회성 #d08a00을 쓰면 같은
                    // 의미가 화면마다 다른 주황이 된다(2026-08-06).
                    ui.colored_label(
                        crate::ui::agent_visuals::status_color(
                            crate::agent_surface::AgentVisualState::Waiting,
                        ),
                        catalog.t("approval.remote_note", &[("url", url)]),
                    );
                }
                ui.separator();
                ui.label(catalog.t("approval.arguments_preview", &[]));
                // proxy가 이미 redact한 표시용 텍스트지만 신뢰하지 않는다 —
                // 일반 Label로 그대로 표시한다(egui는 마크업을 해석하지 않음).
                egui::ScrollArea::vertical()
                    .max_height(200.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(row.arguments_preview()).monospace(),
                            )
                            .wrap(),
                        );
                    });
                ui.separator();
                ui.checkbox(&mut self.remember, catalog.t("approval.remember_tool", &[]));
                ui.horizontal(|ui| {
                    if ui.button(catalog.t("action.allow", &[])).clicked() {
                        decision = Some(ApprovalDecision {
                            id: row_id.to_owned(),
                            allowed: true,
                            remember: self.remember,
                        });
                    }
                    if ui.button(catalog.t("action.deny", &[])).clicked() {
                        decision = Some(ApprovalDecision {
                            id: row_id.to_owned(),
                            allowed: false,
                            remember: self.remember,
                        });
                    }
                });
            });
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str) -> PendingApprovalItem {
        row_with_remote(id, None)
    }

    fn row_with_remote(id: &str, remote_url: Option<&str>) -> PendingApprovalItem {
        PendingApprovalItem::try_new(
            id.to_owned(),
            "srv".to_owned(),
            "tool".to_owned(),
            "{\"k\":\"v\"}".to_owned(),
            None,
            remote_url.map(Arc::from),
            0,
        )
        .unwrap()
    }

    #[test]
    fn http_서버의_원격_url이_고지용으로_보관된다() {
        // H3 리뷰 P1: proxy 경유 첫 Ask 승인이 원격 전송 고지를 겸한다
        let mut ui = ApprovalsUi::new();
        ui.set_pending(vec![row_with_remote("a", Some("https://mcp.example/mcp"))])
            .unwrap();
        assert_eq!(
            ui.pending()
                .first()
                .and_then(PendingApprovalItem::remote_url),
            Some("https://mcp.example/mcp")
        );
        // stdio만 남으면 고지도 사라진다
        ui.set_pending(vec![row("a")]).unwrap();
        assert!(ui.pending()[0].remote_url().is_none());
    }

    #[test]
    fn decide_uses_front_row_and_remember_flag() {
        let mut ui = ApprovalsUi::new();
        assert_eq!(ui.decide(true), None); // 비어 있으면 결정 없음

        ui.set_pending(vec![row("a"), row("b")]).unwrap();
        // 맨 앞(가장 오래된) 항목 'a'에 대한 결정
        assert_eq!(
            ui.decide(true),
            Some(ApprovalDecision {
                id: "a".to_owned(),
                allowed: true,
                remember: false,
            })
        );
        ui.remember = true;
        assert_eq!(
            ui.decide(false),
            Some(ApprovalDecision {
                id: "a".to_owned(),
                allowed: false,
                remember: true,
            })
        );
    }

    #[test]
    fn set_pending_resets_remember_only_on_front_change() {
        let mut ui = ApprovalsUi::new();
        ui.set_pending(vec![row("a")]).unwrap();
        ui.remember = true;
        // 같은 맨 앞 항목 → 체크 유지 (뒤 항목만 추가돼도)
        ui.set_pending(vec![row("a"), row("b")]).unwrap();
        assert!(ui.remember);
        // 맨 앞이 'b'로 바뀜('a' 해소됨) → 리셋
        ui.set_pending(vec![row("b")]).unwrap();
        assert!(!ui.remember);
        // 빈 목록 → 리셋 상태 유지
        ui.set_pending(vec![]).unwrap();
        assert!(!ui.remember);
        assert_eq!(ui.decide(true), None);
    }

    #[test]
    fn dto_debug는_내용을_완전히_redact한다() {
        let item = PendingApprovalItem::try_new(
            "sensitive-id".to_owned(),
            "private-server".to_owned(),
            "private-tool".to_owned(),
            "{\"token\":\"private-value\"}".to_owned(),
            Some("private-workspace:1".to_owned()),
            Some(Arc::from("https://private.example/mcp")),
            0,
        )
        .unwrap();
        let debug = format!("{item:?}");
        assert_eq!(debug, "PendingApprovalItem { REDACTED }");
        for secret_like in ["sensitive-id", "private-server", "private-tool", "token"] {
            assert!(!debug.contains(secret_like));
        }
    }

    #[test]
    fn dto_필드_상한은_exact_max를_허용하고_plus_one을_거부한다() {
        let exact = PendingApprovalItem::try_new(
            "i".repeat(APPROVAL_ID_BYTES_LIMIT),
            "s".repeat(APPROVAL_SERVER_ID_BYTES_LIMIT),
            "t".repeat(APPROVAL_TOOL_NAME_BYTES_LIMIT),
            "p".repeat(APPROVAL_PREVIEW_BYTES_LIMIT),
            Some("k".repeat(APPROVAL_SESSION_KEY_BYTES_LIMIT)),
            Some(Arc::from("u".repeat(APPROVAL_REMOTE_URL_BYTES_LIMIT))),
            0,
        );
        assert!(exact.is_ok());
        assert_eq!(
            PendingApprovalItem::try_new(
                "i".repeat(APPROVAL_ID_BYTES_LIMIT + 1),
                "s".to_owned(),
                "t".to_owned(),
                String::new(),
                None,
                None,
                0,
            )
            .unwrap_err(),
            ApprovalDataErrorCode::InvalidId
        );
        assert_eq!(
            PendingApprovalItem::try_new(
                "i".to_owned(),
                "s".to_owned(),
                "t".to_owned(),
                "p".repeat(APPROVAL_PREVIEW_BYTES_LIMIT + 1),
                None,
                None,
                0,
            )
            .unwrap_err(),
            ApprovalDataErrorCode::InvalidPreview
        );
        assert_eq!(
            PendingApprovalItem::try_new(
                "i".to_owned(),
                "s".repeat(APPROVAL_SERVER_ID_BYTES_LIMIT + 1),
                "t".to_owned(),
                String::new(),
                None,
                None,
                0,
            )
            .unwrap_err(),
            ApprovalDataErrorCode::InvalidServerId
        );
        assert_eq!(
            PendingApprovalItem::try_new(
                "i".to_owned(),
                "s".to_owned(),
                "t".repeat(APPROVAL_TOOL_NAME_BYTES_LIMIT + 1),
                String::new(),
                None,
                None,
                0,
            )
            .unwrap_err(),
            ApprovalDataErrorCode::InvalidToolName
        );
        assert_eq!(
            PendingApprovalItem::try_new(
                "i".to_owned(),
                "s".to_owned(),
                "t".to_owned(),
                String::new(),
                Some("k".repeat(APPROVAL_SESSION_KEY_BYTES_LIMIT + 1)),
                None,
                0,
            )
            .unwrap_err(),
            ApprovalDataErrorCode::InvalidSessionKey
        );
        assert_eq!(
            PendingApprovalItem::try_new(
                "i".to_owned(),
                "s".to_owned(),
                "t".to_owned(),
                String::new(),
                None,
                Some(Arc::from("u".repeat(APPROVAL_REMOTE_URL_BYTES_LIMIT + 1))),
                0,
            )
            .unwrap_err(),
            ApprovalDataErrorCode::InvalidRemoteUrl
        );
    }

    #[test]
    fn dto_accessors는_root_projection값을_그대로_노출한다() {
        let item = PendingApprovalItem::try_new(
            "approval-id".to_owned(),
            "server-id".to_owned(),
            "read_file".to_owned(),
            "{\"path\":\"/redacted\"}".to_owned(),
            Some("workspace-id:7".to_owned()),
            Some(Arc::from("https://example.invalid/mcp")),
            0,
        )
        .unwrap();
        assert_eq!(item.id(), "approval-id");
        assert_eq!(item.server_id(), "server-id");
        assert_eq!(item.tool_name(), "read_file");
        assert_eq!(item.arguments_preview(), "{\"path\":\"/redacted\"}");
        assert_eq!(item.session_key(), Some("workspace-id:7"));
        assert_eq!(item.remote_url(), Some("https://example.invalid/mcp"));
    }

    #[test]
    fn snapshot_item_limit_초과는_stale_snapshot도_비운다() {
        let mut ui = ApprovalsUi::new();
        let exact = (0..APPROVAL_ITEM_LIMIT)
            .map(|index| row(&format!("id-{index}")))
            .collect();
        ui.set_pending(exact).unwrap();
        assert_eq!(ui.pending().len(), APPROVAL_ITEM_LIMIT);
        let rows = (0..=APPROVAL_ITEM_LIMIT)
            .map(|index| row(&format!("id-{index}")))
            .collect();
        assert_eq!(ui.set_pending(rows), Err(ApprovalDataErrorCode::ItemLimit));
        assert!(ui.pending().is_empty());
        assert!(ui.current_id.is_none());
    }

    fn retained_limit_rows(last_url_bytes: usize) -> Vec<PendingApprovalItem> {
        let mut rows: Vec<_> = (0..70)
            .map(|_| {
                PendingApprovalItem::try_new(
                    "i".repeat(APPROVAL_ID_BYTES_LIMIT),
                    "s".repeat(APPROVAL_SERVER_ID_BYTES_LIMIT),
                    "t".repeat(APPROVAL_TOOL_NAME_BYTES_LIMIT),
                    "p".repeat(APPROVAL_PREVIEW_BYTES_LIMIT),
                    Some("k".repeat(APPROVAL_SESSION_KEY_BYTES_LIMIT)),
                    Some(Arc::from("u".repeat(APPROVAL_REMOTE_URL_BYTES_LIMIT))),
                    0,
                )
                .unwrap()
            })
            .collect();
        // 70 * 14,777 bytes = 1,034,390. The final row is 6,146 fixed bytes plus
        // `last_url_bytes`; 8,040 therefore lands exactly on the 1 MiB budget.
        rows.push(
            PendingApprovalItem::try_new(
                "i".to_owned(),
                "s".to_owned(),
                "t".repeat(APPROVAL_TOOL_NAME_BYTES_LIMIT),
                "p".repeat(APPROVAL_PREVIEW_BYTES_LIMIT),
                None,
                Some(Arc::from("u".repeat(last_url_bytes))),
                0,
            )
            .unwrap(),
        );
        rows
    }

    #[test]
    fn snapshot_retained_byte_limit는_exact_max를_허용하고_plus_one을_거부한다() {
        let mut ui = ApprovalsUi::new();
        ui.set_pending(retained_limit_rows(8_040)).unwrap();
        assert_eq!(
            ui.set_pending(retained_limit_rows(8_041)),
            Err(ApprovalDataErrorCode::RetainedBytesLimit)
        );
        assert!(ui.pending().is_empty());
    }

    #[test]
    fn approval_leaf_production_source에는_concrete_boundary_dependency가_없다() {
        let production = include_str!("approvals.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for forbidden in [
            ["storage", "::"].concat(),
            ["mcp_", "store::"].concat(),
            ["Db", "::"].concat(),
            ["audit", "::"].concat(),
            ["secret", "::"].concat(),
            ["PendingApproval", "Row"].concat(),
        ] {
            assert!(
                !production.contains(&forbidden),
                "approval leaf contains forbidden dependency: {forbidden}"
            );
        }
    }
}
