//! 도구 실행 승인 팝업 (agent-proxy option 1.5, GUI 승인 측).
//! 별도 `deppy-mcp-proxy` 프로세스가 Allow/Deny 규칙이 없는 도구 호출을 만나면
//! 공유 DB에 pending 행을 쓴다. GUI는 그 목록을 폴링해(App::logic) 여기로 넘기고,
//! 이 모듈이 모달식 창을 띄워 사용자의 허용/거부 결정을 돌려준다. 되쓰기(resolve)는
//! 호출측(App::ui)이 Db::resolve_approval로 처리한다.
//! http 서버의 호출이면 원격 url을 함께 고지한다 (H3 리뷰 P1 — proxy 경유 경로는
//! Connector Center의 신뢰 모달을 거치지 않으므로 첫 Ask 승인이 원격 전송 고지를 겸한다).

use std::collections::HashMap;

use storage::PendingApprovalRow;

/// 사용자가 팝업에서 내린 결정. remember는 proxy 측이 permission 규칙으로 영속화한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalDecision {
    pub id: String,
    pub allowed: bool,
    pub remember: bool,
}

pub struct ApprovalsUi {
    /// pending 승인 행들 (오래된 순 — DB가 정렬해 준다). 맨 앞 하나만 표시한다.
    pending: Vec<PendingApprovalRow>,
    /// http 서버의 원격 url (server_id → url) — 승인 팝업의 원격 전송 고지용.
    /// 호출측(App)이 pending 폴링 시 mcp_servers에서 함께 채운다.
    remote_urls: HashMap<String, String>,
    /// 현재(맨 앞) 항목의 "이 도구 기억" 체크 상태. 항목이 바뀌면 리셋한다.
    remember: bool,
    /// remember 리셋 판단용 — 마지막으로 표시한 항목 id.
    current_id: Option<String>,
}

impl ApprovalsUi {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            remote_urls: HashMap::new(),
            remember: false,
            current_id: None,
        }
    }

    /// 폴링으로 새로 읽은 pending 목록을 반영한다 (App::logic에서 호출).
    /// 맨 앞 항목이 바뀌면 이전 항목의 체크 상태가 새 항목에 새지 않도록 remember를 리셋한다.
    /// `remote_urls`는 http 서버의 원격 전송 고지용 (server_id → url).
    pub fn set_pending(
        &mut self,
        rows: Vec<PendingApprovalRow>,
        remote_urls: HashMap<String, String>,
    ) {
        let front = rows.first().map(|r| r.id.clone());
        if front != self.current_id {
            self.remember = false;
            self.current_id = front;
        }
        self.pending = rows;
        self.remote_urls = remote_urls;
    }

    /// 벨 인박스 카드용 읽기 전용 접근자 (v3.9 N2). 이미 폴링된 목록을 그대로 노출한다 —
    /// 인박스도 새 DB 조회를 만들지 않고 이 값을 재사용한다(App::poll_pending_approvals가
    /// 채운다).
    pub fn pending(&self) -> &[PendingApprovalRow] {
        &self.pending
    }

    /// 버튼 클릭 → 결정 매핑 (egui 컨텍스트 없이 테스트할 수 있게 분리).
    /// 맨 앞(가장 오래된) 항목 하나에 대해서만 결정을 만든다.
    #[cfg_attr(not(test), allow(dead_code))]
    fn decide(&self, allowed: bool) -> Option<ApprovalDecision> {
        self.pending.first().map(|row| ApprovalDecision {
            id: row.id.clone(),
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
        let row = self.pending.first()?.clone();
        let mut decision = None;
        egui::Window::new(catalog.t("approval.title", &[]))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(catalog.t("approval.server", &[("value", &row.server_id)]));
                ui.label(catalog.t("approval.tool", &[("value", &row.tool_name)]));
                // 어느 세션의 승인인지는 여기서 안 보인다 — 세션명은 DB가 아니라 런타임
                // 상태에서 와야 하는데(2026-07-17: pane_id↔mux_panes 조인이 매칭되지 않아
                // 늘 비어 있었다), 이 모달은 N4에서 호출을 접었으므로 배선을 되살리지
                // 않았다. 세션 맥락이 필요하면 벨 인박스가 보여준다(에이전트명까지).
                // http 서버면 원격 전송 고지 — 이 승인이 신뢰 확인을 겸한다 (H3 리뷰 P1)
                if let Some(url) = self.remote_urls.get(&row.server_id) {
                    ui.colored_label(
                        egui::Color32::from_rgb(0xd0, 0x8a, 0x00),
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
                                egui::RichText::new(&row.arguments_preview).monospace(),
                            )
                            .wrap(),
                        );
                    });
                ui.separator();
                ui.checkbox(&mut self.remember, catalog.t("approval.remember_tool", &[]));
                ui.horizontal(|ui| {
                    if ui.button(catalog.t("action.allow", &[])).clicked() {
                        decision = self.decide(true);
                    }
                    if ui.button(catalog.t("action.deny", &[])).clicked() {
                        decision = self.decide(false);
                    }
                });
            });
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str) -> PendingApprovalRow {
        PendingApprovalRow {
            id: id.to_owned(),
            server_id: "srv".to_owned(),
            tool_name: "tool".to_owned(),
            arguments_preview: "{\"k\":\"v\"}".to_owned(),
            schema_hash: None,
            created_at: 0,
            pane_id: None,
        }
    }

    #[test]
    fn http_서버의_원격_url이_고지용으로_보관된다() {
        // H3 리뷰 P1: proxy 경유 첫 Ask 승인이 원격 전송 고지를 겸한다
        let mut ui = ApprovalsUi::new();
        ui.set_pending(
            vec![row("a")],
            HashMap::from([("srv".to_owned(), "https://mcp.example/mcp".to_owned())]),
        );
        assert_eq!(
            ui.remote_urls.get("srv").map(String::as_str),
            Some("https://mcp.example/mcp")
        );
        // stdio만 남으면(맵 비움) 고지도 사라진다
        ui.set_pending(vec![row("a")], HashMap::new());
        assert!(ui.remote_urls.is_empty());
    }

    #[test]
    fn decide_uses_front_row_and_remember_flag() {
        let mut ui = ApprovalsUi::new();
        assert_eq!(ui.decide(true), None); // 비어 있으면 결정 없음

        ui.set_pending(vec![row("a"), row("b")], HashMap::new());
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
        ui.set_pending(vec![row("a")], HashMap::new());
        ui.remember = true;
        // 같은 맨 앞 항목 → 체크 유지 (뒤 항목만 추가돼도)
        ui.set_pending(vec![row("a"), row("b")], HashMap::new());
        assert!(ui.remember);
        // 맨 앞이 'b'로 바뀜('a' 해소됨) → 리셋
        ui.set_pending(vec![row("b")], HashMap::new());
        assert!(!ui.remember);
        // 빈 목록 → 리셋 상태 유지
        ui.set_pending(vec![], HashMap::new());
        assert!(!ui.remember);
        assert_eq!(ui.decide(true), None);
    }
}
