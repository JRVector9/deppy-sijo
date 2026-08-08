//! 벨 팝오버 「대기 중」 섹션의 MCP 승인 카드 (v3.9 PR-N2,
//! `ai_agent_workspace_v3_9_waiting_inbox_pr_plan.md`).
//!
//! 데이터는 합성 루트가 이미 만든 `ApprovalsUi::pending()` immutable snapshot을 그대로
//! 읽는다 — 팝오버는 매 프레임 렌더되므로 이 모듈은 새 host 조회를 하지 않는다.
//! 승인/거부 intent와 [이동→] 네비게이션은 호출측(`App::inbox_popup`)이 기존 경로
//! (`plan_agent_notification_navigation` 등)로 처리한다 — 이 모듈은
//! 렌더 + 사용자 의도([`ApprovalCardsAction`]) 산출만 한다(leaf UI가 host를 직접 만지지
//! 않는다, xtask check-boundary).
//!
//! ## 워크스페이스/세션 해석 (① — "가지 않고 판단"이 이 PR의 핵심 요구)
//! `PendingApprovalItem::session_key`는 런타임 세션 키
//! (`{workspace_id}:{session_id}`)다 — `deppy_core::parse_session_key`로 파싱한다.
//! 워크스페이스명·세션명은 persistence가 모르는 정보라 호출측(App)이 메모리에서 해석해
//! 넘겨준다(에이전트가 감지되면 그 이름, 아니면 셀 제목).
//!
//! 예전엔 mcp-store가 `pane_id = mux_panes.id` 조인으로 세션 UUID/제목을 채우려 했지만
//! 두 값은 다른 식별자 공간이라 매칭된 적이 없다 — 2026-07-17에 조인을 걷어냈다.

use std::collections::HashMap;

use deppy_core::parse_session_key;
use runtime::SessionId;

use super::approvals::{ApprovalDecision, PendingApprovalItem};
use super::notifications::{AgentNotificationTarget, section_label};

/// 팝오버에 한 번에 그리는 카드 최대 수 — 초과분은 "+N건 더"로 뭉친다(②).
/// 전체 「작업함」 페이지는 상한 없이 그린다 — 팝오버 전용 값이라 페이지에 쓰면
/// 6번째 이후 요청을 조작할 수 없다(codex P2). 호출측이 max_cards로 구분한다.
pub const POPUP_MAX_CARDS: usize = 5;
/// 인자 미리보기를 카드 폭(팝오버 260~300px)에서 2줄 안팎으로 자르는 문자 수 예산.
const PREVIEW_CLIP_CHARS: usize = 110;

/// 카드 섹션이 만든 사용자 액션 — 호출측(App)이 DB 되쓰기/네비게이션을 수행한다.
#[derive(Default)]
pub struct ApprovalCardsAction {
    /// [승인]/[거부] 클릭 — 호출측 controller가 bounded command로 처리한다.
    pub decision: Option<ApprovalDecision>,
    /// [이동→] 클릭 — 호출측이 기존 알림 네비게이션 경로로 처리한다.
    pub goto: Option<AgentNotificationTarget>,
}

/// 「대기 중」 섹션의 승인 카드들을 그린다. pending이 비어 있으면 아무것도 그리지
/// 않는다 — N3의 PTY 대기 카드가 있으면 그쪽 섹션 헤더만 보인다.
///
/// `workspace_names`는 workspace_id → 표시 이름(`App::workspace_display_name` 결과) 맵.
/// 이 모듈은 App을 모르므로 호출측이 `self.workspaces`에서 미리 만들어 넘긴다.
pub fn render(
    ui: &mut egui::Ui,
    catalog: &i18n::Catalog,
    pending: &[PendingApprovalItem],
    workspace_names: &HashMap<String, String>,
    session_titles: &HashMap<(String, SessionId), String>,
    max_cards: usize,
    // 막힌 시간 계산 기준(unix 초). 호출측이 넘겨 테스트가 시계에 흔들리지 않게 한다.
    now: i64,
) -> ApprovalCardsAction {
    let mut action = ApprovalCardsAction::default();
    if pending.is_empty() {
        return action;
    }
    section_label(ui, &catalog.t("inbox.approval.section", &[]));
    ui.add_space(2.0);
    for row in pending.iter().take(max_cards) {
        render_card(
            ui,
            catalog,
            row,
            workspace_names,
            session_titles,
            &mut action,
            now,
        );
    }
    let hidden = pending.len().saturating_sub(max_cards);
    if hidden > 0 {
        ui.label(
            egui::RichText::new(
                catalog.t("inbox.approval.more", &[("count", &hidden.to_string())]),
            )
            .size(11.0)
            .weak(),
        );
    }
    ui.add_space(4.0);
    action
}

fn render_card(
    ui: &mut egui::Ui,
    catalog: &i18n::Catalog,
    row: &PendingApprovalItem,
    workspace_names: &HashMap<String, String>,
    session_titles: &HashMap<(String, SessionId), String>,
    action: &mut ApprovalCardsAction,
    now: i64,
) {
    let session_key = row.session_key().and_then(parse_session_key);
    let workspace_label =
        session_key.and_then(|(workspace_id, _)| workspace_names.get(workspace_id).cloned());
    // 세션 라벨은 호출측이 메모리에서 해석해 넘긴다 — DB는 이 정보를 모른다(모듈 상단
    // 문서). "어느 셀의 에이전트인가"가 없으면 워크스페이스명만으로는 무엇을 승인하는지
    // 판단할 수 없다(2026-07-17 사용자).
    let session_label = session_key.and_then(|(workspace_id, session)| {
        session_titles
            .get(&(workspace_id.to_owned(), session))
            .cloned()
    });
    egui::Frame::group(ui.style()).show(ui, |ui| {
        // 워크스페이스/세션 컨텍스트 — ① 핵심 요구(가지 않고 판단).
        let context = match (&workspace_label, &session_label) {
            (Some(ws), Some(title)) => format!("{ws} · {title}"),
            (Some(ws), None) => ws.clone(),
            (None, Some(title)) => title.clone(),
            (None, None) => catalog.t("inbox.approval.unknown_session", &[]),
        };
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(context).size(11.0).weak());
            // 승인이 나를 막고 있는 시간 — DB created_at이 진실이라 별도 추적이 없다.
            // 세션 카드와 같은 표기를 써서 두 자리의 숫자가 같은 뜻으로 읽힌다.
            ui.label(
                egui::RichText::new(catalog.t(
                    "fleet.blocked_for",
                    &[(
                        "value",
                        &crate::fleet::format_blocked_duration(now, row.created_at()),
                    )],
                ))
                .size(11.0)
                .color(crate::ui::agent_visuals::status_color(
                    crate::agent_surface::AgentVisualState::Waiting,
                )),
            );
        });
        ui.strong(row.tool_name());
        // arguments_preview는 proxy가 이미 redact한 표시용 텍스트 — 추가 redaction
        // 불필요, 원문 조회 금지(설계 제약).
        //
        // raw JSON을 그대로 뿌리면 "무엇을 승인하는지"가 안 보인다(2026-07-17 사용자:
        // 긴 경로가 네 줄로 접히며 정작 파일명이 묻혔다). 인자별 한 줄로 펴고, 값은
        // 뒤쪽(파일명·명령 꼬리)을 남기며 줄인다 — 판단에 필요한 건 대개 뒤쪽이다.
        for (key, value) in format_arguments(row.arguments_preview()) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                if !key.is_empty() {
                    ui.label(egui::RichText::new(format!("{key}:")).size(11.0).weak());
                }
                ui.add(
                    egui::Label::new(egui::RichText::new(value).monospace().size(11.0)).truncate(),
                );
            });
        }
        ui.horizontal(|ui| {
            if ui
                .button(catalog.t("inbox.approval.approve", &[]))
                .clicked()
            {
                action.decision = Some(decision_for(row, true));
            }
            if ui.button(catalog.t("inbox.approval.deny", &[])).clicked() {
                action.decision = Some(decision_for(row, false));
            }
            // pane_id가 파싱되는 행만 [이동→]를 보여준다 — 세션 불명 행은 이동할 곳이 없다.
            if let Some((workspace_id, session)) = session_key
                && ui.button(catalog.t("inbox.approval.goto", &[])).clicked()
            {
                action.goto = Some(AgentNotificationTarget::Pty {
                    workspace_id: workspace_id.to_owned(),
                    session,
                });
            }
        });
    });
    ui.add_space(4.0);
}

fn decision_for(row: &PendingApprovalItem, allowed: bool) -> ApprovalDecision {
    ApprovalDecision {
        id: row.id().to_owned(),
        allowed,
        // 인박스 빠른 조치는 "이 도구 항상 허용" 규칙 저장을 다루지 않는다 — 그 옵션은
        // 기존 모달(approvals_ui.show)의 체크박스로 남겨둔다(요구사항 범위 밖).
        remember: false,
    }
}

/// 인자 미리보기를 "키 → 값" 목록으로 편다. JSON object가 아니면(파싱 실패·배열 등)
/// 키 없이 원문 한 줄로 돌려준다 — proxy가 무엇을 싣든 카드는 그려져야 한다.
fn format_arguments(preview: &str) -> Vec<(String, String)> {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(preview)
    else {
        return vec![(String::new(), clip_value(preview))];
    };
    if map.is_empty() {
        return Vec::new();
    }
    map.into_iter()
        .map(|(key, value)| (key, summarize_value(&value)))
        .collect()
}

/// JSON 값 한 개를 카드 한 줄로 요약한다.
fn summarize_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => clip_value(s),
        serde_json::Value::Array(items) => format!("[{}개 항목]", items.len()),
        serde_json::Value::Object(map) => format!("{{{}개 필드}}", map.len()),
        other => other.to_string(),
    }
}

/// 값이 길면 **뒤쪽을 남기고** 앞을 줄인다 — 경로는 파일명이, 명령은 인자가 뒤에 있어
/// 판단에 필요한 정보가 대개 뒤쪽이다(앞을 남기면 홈 경로만 보인다).
/// 홈 디렉터리는 `~`로 접는다.
fn clip_value(text: &str) -> String {
    let text = match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && text.starts_with(&home) => {
            format!("~{}", &text[home.len()..])
        }
        _ => text.to_owned(),
    };
    let count = text.chars().count();
    if count <= PREVIEW_CLIP_CHARS {
        return text;
    }
    let tail: String = text.chars().skip(count - PREVIEW_CLIP_CHARS).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, session_key: Option<&str>) -> PendingApprovalItem {
        PendingApprovalItem::try_new(
            id.to_owned(),
            "srv".to_owned(),
            "read_file".to_owned(),
            "{}".to_owned(),
            session_key.map(str::to_owned),
            None,
            0,
        )
        .unwrap()
    }

    #[test]
    fn inbox_approval_leaf_production_source에는_host_boundary가_없다() {
        let production = include_str!("inbox_approvals.rs")
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
            ["std::", "fs"].concat(),
            ["std::", "process"].concat(),
            ["std::", "thread"].concat(),
            ["request_repaint", "("].concat(),
        ] {
            assert!(
                !production.contains(&forbidden),
                "approval inbox leaf contains forbidden host edge: {forbidden}"
            );
        }
    }

    #[test]
    fn parse_session_key_workspace_id와_세션번호를_나눈다() {
        let (ws, session) = parse_session_key("ws-abc-123:42").unwrap();
        assert_eq!(ws, "ws-abc-123");
        assert_eq!(session, SessionId(42));
    }

    #[test]
    fn parse_session_key_콜론_없으면_none() {
        assert_eq!(parse_session_key("no-colon-here"), None);
    }

    #[test]
    fn parse_session_key_세션번호가_숫자가_아니면_none() {
        assert_eq!(parse_session_key("ws:not-a-number"), None);
    }

    #[test]
    fn parse_session_key_workspace_id가_비어있으면_none() {
        assert_eq!(parse_session_key(":42"), None);
    }

    #[test]
    fn clip_value_짧은_텍스트는_그대로_둔다() {
        assert_eq!(clip_value("short"), "short");
    }

    /// 2026-07-17 사용자 회귀: raw JSON을 그대로 뿌리자 긴 경로가 네 줄로 접히며
    /// 정작 판단에 필요한 파일명이 묻혔다. 값은 **뒤쪽**(파일명)을 남겨야 한다.
    #[test]
    fn clip_value_긴_값은_뒤쪽_파일명을_남긴다() {
        let long = format!("/Users/jr/{}/src/main.rs", "deep/".repeat(40));
        let clipped = clip_value(&long);
        assert!(clipped.starts_with('…'), "앞을 줄였음을 표시해야 한다");
        assert!(
            clipped.ends_with("src/main.rs"),
            "무엇을 승인하는지 = 파일명이 남아야 한다: {clipped}"
        );
        assert_eq!(clipped.chars().count(), PREVIEW_CLIP_CHARS + 1);
    }

    #[test]
    fn format_arguments_json을_키별_한줄로_편다() {
        let args = format_arguments(r#"{"cmd":"npm test","timeout":30}"#);
        assert_eq!(args.len(), 2);
        assert!(args.contains(&("cmd".to_owned(), "npm test".to_owned())));
        assert!(args.contains(&("timeout".to_owned(), "30".to_owned())));
    }

    #[test]
    fn format_arguments_큰_값은_크기만_요약한다() {
        let args = format_arguments(r#"{"files":["a","b","c"],"opts":{"x":1}}"#);
        let map: HashMap<_, _> = args.into_iter().collect();
        assert_eq!(map.get("files").unwrap(), "[3개 항목]");
        assert_eq!(map.get("opts").unwrap(), "{1개 필드}");
    }

    /// proxy가 무엇을 싣든 카드는 그려져야 한다 — JSON이 아니면 원문 한 줄로 폴백.
    #[test]
    fn format_arguments_json이_아니면_원문_한줄로_폴백한다() {
        let args = format_arguments("not json at all");
        assert_eq!(args, vec![(String::new(), "not json at all".to_owned())]);
    }

    #[test]
    fn decision_for_row_id와_allowed를_싣고_remember는_항상_false() {
        let r = row("a1", None);
        let d = decision_for(&r, true);
        assert_eq!(d.id, "a1");
        assert!(d.allowed);
        assert!(!d.remember);
    }

    #[test]
    fn render_pending_비어있으면_아무것도_안그리고_액션도_없다() {
        let ctx = egui::Context::default();
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let workspace_names = HashMap::new();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let action = render(
                ui,
                &catalog,
                &[],
                &workspace_names,
                &HashMap::new(),
                POPUP_MAX_CARDS,
                0,
            );
            assert!(action.decision.is_none());
            assert!(action.goto.is_none());
        });
    }

    #[test]
    fn render_클릭_없으면_카드가_있어도_액션이_없다_스모크() {
        let ctx = egui::Context::default();
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut workspace_names = HashMap::new();
        workspace_names.insert("ws-1".to_owned(), "my-project".to_owned());
        let rows = vec![
            row("a1", Some("ws-1:7")),
            row("a2", None),
            row("a3", Some("ws-unknown:1")),
        ];
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let action = render(
                ui,
                &catalog,
                &rows,
                &workspace_names,
                &HashMap::new(),
                POPUP_MAX_CARDS,
                0,
            );
            assert!(action.decision.is_none());
            assert!(action.goto.is_none());
        });
    }

    #[test]
    fn render_max_cards_초과행도_패닉없이_그려진다_스모크() {
        let ctx = egui::Context::default();
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let workspace_names = HashMap::new();
        let rows: Vec<_> = (0..POPUP_MAX_CARDS + 3)
            .map(|i| row(&format!("a{i}"), None))
            .collect();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let action = render(
                ui,
                &catalog,
                &rows,
                &workspace_names,
                &HashMap::new(),
                POPUP_MAX_CARDS,
                0,
            );
            assert!(action.decision.is_none());
            assert!(action.goto.is_none());
        });
    }

    // ── kittest 상호작용 테스트 (2026-07-17) ──
    // 실제 클릭을 AccessKit 트리로 시뮬레이션한다 — "인박스에서 워크스페이스 전환 없이
    // 승인"의 UI 절반을 자동 검증(나머지 절반 resolve_approval은 storage 테스트가 커버).

    /// 클릭된 decision들을 프레임 너머로 수집하는 하네스.
    fn decision_harness<'a>(
        catalog: &'a i18n::Catalog,
        rows: &'a [PendingApprovalItem],
        names: &'a HashMap<String, String>,
    ) -> egui_kittest::Harness<'a, Vec<ApprovalDecision>> {
        egui_kittest::Harness::new_ui_state(
            move |ui, captured: &mut Vec<ApprovalDecision>| {
                let action = render(
                    ui,
                    catalog,
                    rows,
                    names,
                    &HashMap::new(),
                    POPUP_MAX_CARDS,
                    0,
                );
                if let Some(decision) = action.decision {
                    captured.push(decision);
                }
            },
            Vec::new(),
        )
    }

    #[test]
    fn kittest_승인_클릭이_그_row의_allowed_decision을_만든다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let rows = vec![row("a1", Some("ws-1:7"))];
        let mut names = HashMap::new();
        names.insert("ws-1".to_owned(), "proj".to_owned());
        let mut harness = decision_harness(&catalog, &rows, &names);
        harness.get_by_label("Approve").click();
        harness.run();
        assert_eq!(harness.state().len(), 1);
        let d = &harness.state()[0];
        assert_eq!(d.id, "a1");
        assert!(d.allowed);
    }

    #[test]
    fn kittest_거부_클릭이_denied_decision을_만든다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let rows = vec![row("a1", Some("ws-1:7"))];
        let names = HashMap::new();
        let mut harness = decision_harness(&catalog, &rows, &names);
        harness.get_by_label("Deny").click();
        harness.run();
        assert_eq!(harness.state().len(), 1);
        assert!(!harness.state()[0].allowed);
    }

    #[test]
    fn kittest_카드_여러_장일_때_두번째_카드의_승인이_그_row를_가리킨다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let rows = vec![row("a1", Some("ws-1:7")), row("a2", Some("ws-1:8"))];
        let names = HashMap::new();
        let mut harness = decision_harness(&catalog, &rows, &names);
        let buttons: Vec<_> = harness.get_all_by_label("Approve").collect();
        assert_eq!(buttons.len(), 2, "카드마다 승인 버튼이 있어야 한다");
        buttons[1].click();
        drop(buttons);
        harness.run();
        assert_eq!(harness.state().len(), 1);
        assert_eq!(
            harness.state()[0].id,
            "a2",
            "두번째 카드 클릭이 a2를 승인해야 한다"
        );
    }
}
