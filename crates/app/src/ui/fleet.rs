//! 「작업」 페이지 (leaf) — 에이전트 세션을 한 화면에서 전부 다룬다.
//!
//! 2026-08-08까지는 「작업함(Inbox)」과 「플릿」이 별도 페이지였는데, 같은 사실을 다섯
//! 곳에서 보여주고 배지 두 개가 같은 값(`global_waiting`)을 세고 있었다(사용자 지적).
//! 승인·입력 대기(주의 섹션)와 세션 그리드를 이 leaf 하나가 소유해 중복을 없앤다.
//!
//! 주의 카드는 새로 그리지 않고 [`crate::ui::inbox_approvals::render`]와
//! [`crate::ui::inbox_waiting::InboxWaitingUi::render`]를 **그대로 호출**한다 — 두 렌더러는
//! 이미 승인/거절·y/n·이동 액션을 돌려주고 자체 테스트도 갖고 있다. 결과 intent는
//! [`FleetPageOutput`]으로 묶어 App이 기존 apply 경로(`apply_inbox_approval_decision`,
//! `apply_inbox_waiting_action`)로 소비한다 — leaf는 host I/O를 하지 않는다.
//!
//! 상태 색은 앱 공용 팔레트(`agent_visuals::status_color`), 워크스페이스 색은
//! `file_tree::workspace_accent`를 재사용한다.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use crate::agent_surface::AgentVisualState;
use crate::fleet::{FleetSession, FleetSummary, FleetTarget, SessionGroup};
use crate::prompt_library::PromptLibrary;
use crate::ui::agent_visuals::status_color;

/// fleet 그리드가 App에 돌려주는 액션.
pub enum FleetAction {
    /// PTY 세션으로 포커스(그리드 → 터미널 전환). App이 FocusSession 경로로 라우팅한다.
    Focus {
        workspace_id: String,
        tab: runtime::MuxTabId,
        pane: runtime::MuxPaneId,
    },
    /// 구조화(App Server) 세션 열기 — App이 에이전트 패널에서 해당 세션을 연다.
    OpenStructured { session_id: String },
    /// 새 에이전트 시작 — App이 에이전트 패널을 연다(fleet에 세션을 추가하는 진입점).
    LaunchAgent,
    /// 저장된 프롬프트를 여러 실행 중 에이전트에 브로드캐스트. App이 각 대상 세션에
    /// 컴포저 Send와 동일 경로로 주입한다(사용자 검토 후 전송이 아니라 즉시 전송이므로
    /// 대상·프롬프트를 사용자가 명시적으로 고른 뒤에만 발행된다).
    Broadcast {
        prompt: String,
        targets: Vec<(String, runtime::SessionId)>,
    },
    /// 등록된 에이전트 설정으로 N개 세션을 한 번에 시작. `prompt`가 Some이면 렌더된
    /// 프롬프트 텍스트를 각 에이전트의 초기 argv(위치 인자)로 전달해 즉시 작업을
    /// 시작한다(PR-S2) — `claude "<prompt>"` / `codex "<prompt>"`와 동일한 형태라
    /// 주입 타이밍 문제가 없다. None이면 빈 세션(PR-S1과 동일). App이
    /// fleet_batch_spawn_max로 count를, 바이트 상한으로 prompt를 방어적으로 재검증한다.
    BatchSpawn {
        agent_id: String,
        count: u32,
        prompt: Option<String>,
    },
}

/// 브로드캐스트 패널 상태 — 프롬프트 선택 + 파라미터 + 대상 체크.
#[derive(Default)]
struct BroadcastState {
    prompt_id: Option<String>,
    params: BTreeMap<String, String>,
    /// 체크된 대상 (workspace_id, session).
    targets: HashSet<(String, runtime::SessionId)>,
    /// 대상 3개 이상 전송의 2단계 확인 단계(리뷰 Low). 프롬프트·대상이 바뀌면 초기화한다.
    confirm_send: bool,
}

/// 배치 스폰 패널 상태 — 에이전트 선택 + 개수 + (선택) 프롬프트. 패널을 열 때마다
/// 초기화한다. `prompt_id`가 None이면 빈 세션(PR-S1과 동일 — "없음" 선택).
struct BatchSpawnState {
    agent_id: Option<String>,
    count: u32,
    prompt_id: Option<String>,
    params: BTreeMap<String, String>,
}

/// 배치 스폰 패널에 필요한 App 투영 — 슬림 (id, name) 목록 + 설정 상한. `render`의
/// 인자 수를 clippy::too_many_arguments 문턱 아래로 묶어 둔다.
pub struct BatchSpawnInput<'a> {
    pub agents: &'a [(Arc<str>, Arc<str>)],
    pub max: u32,
}

/// 주의 섹션(승인·입력 대기) 입력 — App이 매 프레임 조립해 내려준다. 세션 그리드와 달리
/// 이 데이터는 워크스페이스 런타임이 아니라 승인 DB와 hook에서 온다.
pub struct AttentionInput<'a> {
    pub pending: &'a [crate::ui::approvals::PendingApprovalItem],
    pub workspace_names: &'a std::collections::HashMap<String, String>,
    pub session_titles: &'a std::collections::HashMap<(String, runtime::SessionId), String>,
    /// 대기 카드와 **막히기 시작한 시각**(unix 초) 쌍. 카드 자체에는 시각이 없어
    /// App의 blocked_since 맵에서 채워 넘긴다.
    pub waiting_cards: &'a [(crate::ui::inbox_waiting::WaitingCard, i64)],
    pub waiting_ui: &'a mut crate::ui::inbox_waiting::InboxWaitingUi,
}

/// 나를 막고 있는 항목 하나 — 승인이든 입력 대기든 같은 큐에 선다.
///
/// 사용자에게는 둘 다 "에이전트가 나를 기다린다"는 같은 종류의 일이라 화면에서 나누지
/// 않는다. 정렬 키는 `blocked_since` 하나뿐이고 그 값이 카드에 그대로 보인다.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockedItem {
    /// 건너뛰기 추적용 안정 키.
    pub key: String,
    pub title: String,
    /// "워크스페이스 · 세션" 맥락 줄.
    pub context: String,
    pub blocked_since: i64,
    pub kind: BlockedKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BlockedKind {
    /// MCP 승인 — 실행할 도구와 인자를 그대로 보여준다(가서 보지 않고 판단).
    Approval {
        id: String,
        tool_name: String,
        arguments_preview: String,
    },
    /// PTY 입력 대기 — 기존 대기 카드 위젯에 그대로 위임한다(자유 응답·로그 미리보기를
    /// 잃지 않으려고 y/n 버튼을 새로 만들지 않는다). 값은 넘겨받은 카드 슬라이스의 인덱스.
    NeedsInput { card_index: usize },
}

/// 승인·입력 대기를 하나의 큐로 합치고 **오래 막힌 순**으로 세운다.
///
/// 순수 함수라 UI 없이 순서 계약을 검증할 수 있다. `now`는 쓰지 않는다 — 정렬은
/// 절대 시각으로 하고 표시할 때만 경과를 계산한다.
pub fn blocked_queue(
    approvals: &[crate::ui::approvals::PendingApprovalItem],
    waiting: &[(crate::ui::inbox_waiting::WaitingCard, i64)],
    workspace_names: &std::collections::HashMap<String, String>,
    session_titles: &std::collections::HashMap<(String, runtime::SessionId), String>,
    unknown_session: &str,
) -> Vec<BlockedItem> {
    let mut items: Vec<BlockedItem> = Vec::with_capacity(approvals.len() + waiting.len());
    for row in approvals {
        let parsed = row
            .session_key()
            .and_then(crate::ui::inbox_waiting::parse_session_key);
        let context = match parsed {
            Some((workspace_id, session)) => {
                let ws = workspace_names.get(&workspace_id).cloned();
                let title = session_titles.get(&(workspace_id, session)).cloned();
                match (ws, title) {
                    (Some(ws), Some(title)) => format!("{ws} · {title}"),
                    (Some(ws), None) => ws,
                    (None, Some(title)) => title,
                    (None, None) => unknown_session.to_owned(),
                }
            }
            None => unknown_session.to_owned(),
        };
        items.push(BlockedItem {
            key: format!("approval:{}", row.id()),
            title: row.tool_name().to_owned(),
            context,
            blocked_since: row.created_at(),
            kind: BlockedKind::Approval {
                id: row.id().to_owned(),
                tool_name: row.tool_name().to_owned(),
                arguments_preview: row.arguments_preview().to_owned(),
            },
        });
    }
    for (index, (card, since)) in waiting.iter().enumerate() {
        items.push(BlockedItem {
            key: format!("waiting:{}:{}", card.workspace_id, card.session.0),
            title: card
                .headline
                .clone()
                .unwrap_or_else(|| card.session_title.clone()),
            context: format!("{} · {}", card.workspace_name, card.session_title),
            blocked_since: *since,
            kind: BlockedKind::NeedsInput { card_index: index },
        });
    }
    // 오래 막힌 순 — 굶는 항목이 없고, 정렬 키가 화면에 보여 순서가 자명하다.
    items.sort_by(|a, b| {
        a.blocked_since
            .cmp(&b.blocked_since)
            .then(a.key.cmp(&b.key))
    });
    items
}

/// 헤더 사용량 표기 입력 — 하단 상태바와 **같은 값**을 쓰려고 App이 그대로 넘긴다.
pub struct UsageReadout<'a> {
    pub claude: Option<crate::app::ProviderUsage>,
    pub codex: Option<crate::app::ProviderUsage>,
    pub codex_meta: Option<&'a crate::ui::agent_sessions::CodexUsageMeta>,
}

/// 페이지가 App에 돌려주는 intent 묶음. 그리드·승인·대기가 각각 독립적으로 발생할 수 있다.
#[derive(Default)]
pub struct FleetPageOutput {
    pub grid: Option<FleetAction>,
    pub approval_decision: Option<crate::ui::approvals::ApprovalDecision>,
    pub waiting_action: Option<crate::ui::inbox_waiting::WaitingAction>,
    pub goto: Option<crate::ui::notifications::AgentNotificationTarget>,
}

#[derive(Default)]
pub struct FleetUi {
    /// Some이면 브로드캐스트 패널이 열려 있다.
    broadcast: Option<BroadcastState>,
    /// Some이면 배치 스폰 패널이 열려 있다.
    batch_spawn: Option<BatchSpawnState>,
    /// 「다음」으로 넘긴 항목들. 매 프레임 현재 큐와 대조해 사라진 키는 지우고,
    /// 전부 건너뛴 상태면 비워서 앞으로 되돌아간다(막힌 것을 영영 못 보면 안 된다).
    skipped: HashSet<String>,
}

impl FleetUi {
    /// 「작업」 페이지를 그린다 — 주의 섹션(승인·입력 대기) + 세션 그리드.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        ui: &mut egui::Ui,
        sessions: &[FleetSession],
        summary: FleetSummary,
        catalog: &i18n::Catalog,
        library: &PromptLibrary,
        batch_spawn_input: BatchSpawnInput<'_>,
        attention: AttentionInput<'_>,
        usage: UsageReadout<'_>,
        workspaces: &[crate::ui::file_tree::SidebarWorkspaceEntry],
    ) -> FleetPageOutput {
        let BatchSpawnInput {
            agents,
            max: batch_spawn_max,
        } = batch_spawn_input;
        let mut out = FleetPageOutput::default();
        let action = &mut out.grid;
        let now = deppy_core::time::unix_secs_i64();
        egui::Frame::central_panel(ui.style())
            .inner_margin(egui::Margin::symmetric(16, 14))
            .show(ui, |ui| {
                // 브로드캐스트 버튼은 브로드캐스트 가능한 세션(PTY)이 있을 때만 — 구조화만
                // 있는 fleet에서 눌러도 대상이 비는 막다른 버튼이 되지 않게(리뷰 Medium).
                let has_broadcast_target = sessions.iter().any(|s| s.broadcast_key().is_some());
                match header(ui, summary, catalog, has_broadcast_target, usage) {
                    Some(HeaderClick::Launch) => *action = Some(FleetAction::LaunchAgent),
                    Some(HeaderClick::Broadcast) => {
                        // 대상 기본값 = 작업 중이 아닌 세션(진행 중 에이전트는 방해하지 않음).
                        self.broadcast = Some(BroadcastState {
                            prompt_id: library.prompts.first().map(|p| p.id.clone()),
                            params: BTreeMap::new(),
                            targets: sessions
                                .iter()
                                .filter(|s| s.state != AgentVisualState::Active)
                                .filter_map(|s| s.broadcast_key())
                                .collect(),
                            confirm_send: false,
                        });
                    }
                    Some(HeaderClick::BatchSpawn) => {
                        // 패널을 열 때마다 초기화 — 기본 선택 = 첫 에이전트, 개수 1,
                        // 프롬프트 없음(빈 세션, PR-S1 동작 유지).
                        self.batch_spawn = Some(BatchSpawnState {
                            agent_id: agents.first().map(|(id, _)| id.to_string()),
                            count: 1,
                            prompt_id: None,
                            params: BTreeMap::new(),
                        });
                    }
                    None => {}
                }
                ui.add_space(12.0);
                let AttentionInput {
                    pending,
                    workspace_names,
                    session_titles,
                    waiting_cards,
                    waiting_ui,
                } = attention;
                let queue = blocked_queue(
                    pending,
                    waiting_cards,
                    workspace_names,
                    session_titles,
                    &catalog.t("inbox.approval.unknown_session", &[]),
                );
                let plain_cards: Vec<crate::ui::inbox_waiting::WaitingCard> =
                    waiting_cards.iter().map(|(card, _)| card.clone()).collect();

                // 2컬럼 — 좌측은 "지금 뭘 할까", 우측은 "다 뭐하고 있나". 두 질문이 달라
                // 화면을 나눈다(2026-08-08 목업).
                let full = ui.available_width();
                let hero_width = (full * 0.42).clamp(300.0, 460.0);
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(hero_width, ui.available_height()),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_width(hero_width);
                            egui::ScrollArea::vertical()
                                .id_salt("fleet_hero")
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    let hero = render_hero(
                                        ui,
                                        catalog,
                                        &queue,
                                        &plain_cards,
                                        waiting_ui,
                                        &mut self.skipped,
                                        now,
                                    );
                                    out.approval_decision = hero.approval_decision;
                                    out.waiting_action = hero.waiting_action;
                                    out.goto = hero.goto;
                                });
                        },
                    );
                    ui.separator();
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new(catalog.t("fleet.hero.sessions", &[]))
                                .small()
                                .weak(),
                        );
                        ui.add_space(4.0);
                        if sessions.is_empty() {
                            ui.add_space(24.0);
                            ui.vertical_centered(|ui| {
                                ui.label(egui::RichText::new(catalog.t("fleet.empty", &[])).weak());
                                ui.add_space(4.0);
                                ui.label(
                                    egui::RichText::new(catalog.t("fleet.empty.hint", &[]))
                                        .weak()
                                        .small(),
                                );
                                ui.add_space(12.0);
                                if ui.button(catalog.t("fleet.new_agent", &[])).clicked() {
                                    *action = Some(FleetAction::LaunchAgent);
                                }
                            });
                            return;
                        }
                        // 묶음별 섹션 — 막힌 것이 맨 위다. 정렬은 순수 함수가 하고
                        // 여기서는 그리기만 한다(순서 계약을 UI 없이 테스트하려고).
                        let grouped = crate::fleet::group_sessions(sessions.to_vec());
                        egui::ScrollArea::vertical()
                            .id_salt("fleet_sessions")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                for (group, members) in SessionGroup::ORDER.into_iter().zip(grouped)
                                {
                                    if members.is_empty() {
                                        continue;
                                    }
                                    section_header(ui, group, members.len(), catalog);
                                    ui.horizontal_wrapped(|ui| {
                                        for session in &members {
                                            if card(
                                                ui,
                                                session,
                                                catalog,
                                                card_accent(workspaces, session),
                                                now,
                                            ) {
                                                *action = Some(match &session.target {
                                                    FleetTarget::Pty { tab, pane, .. } => {
                                                        FleetAction::Focus {
                                                            workspace_id: session
                                                                .workspace_id
                                                                .clone(),
                                                            tab: tab.clone(),
                                                            pane: pane.clone(),
                                                        }
                                                    }
                                                    FleetTarget::Structured { session_id } => {
                                                        FleetAction::OpenStructured {
                                                            session_id: session_id.clone(),
                                                        }
                                                    }
                                                });
                                            }
                                        }
                                    });
                                    ui.add_space(10.0);
                                }
                            });
                    });
                });
            });
        // 브로드캐스트 창은 떠 있는 Window라 중앙 패널과 독립적으로 그린다.
        if let Some(sent) = self.broadcast_window(ui.ctx(), sessions, catalog, library) {
            out.grid = Some(sent);
        }
        // 배치 스폰 창도 동일하게 독립 Window.
        if let Some(sent) =
            self.batch_spawn_window(ui.ctx(), agents, batch_spawn_max, catalog, library)
        {
            out.grid = Some(sent);
        }
        out
    }

    /// 브로드캐스트 창 — 프롬프트 선택 + 파라미터 + 대상 체크 + 전송. 닫혀 있으면 아무것도
    /// 그리지 않는다. 전송/닫기 시 상태를 제거한다.
    fn broadcast_window(
        &mut self,
        ctx: &egui::Context,
        sessions: &[FleetSession],
        catalog: &i18n::Catalog,
        library: &PromptLibrary,
    ) -> Option<FleetAction> {
        // 닫혀 있으면(Some 아님) 창을 그리지 않는다.
        self.broadcast.as_ref()?;
        let mut action = None;
        let mut open = true;
        egui::Window::new(catalog.t("fleet.broadcast.title", &[]))
            .id(egui::Id::new("fleet_broadcast"))
            .collapsible(false)
            .resizable(true)
            .default_width(520.0)
            .open(&mut open)
            .show(ctx, |ui| {
                action = self.broadcast_body(ui, sessions, catalog, library);
            });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            open = false;
        }
        // 전송했거나(action Some) 닫으면 패널 상태를 버린다.
        if !open || action.is_some() {
            self.broadcast = None;
        }
        action
    }

    fn broadcast_body(
        &mut self,
        ui: &mut egui::Ui,
        sessions: &[FleetSession],
        catalog: &i18n::Catalog,
        library: &PromptLibrary,
    ) -> Option<FleetAction> {
        let state = self.broadcast.as_mut()?;
        if library.prompts.is_empty() {
            ui.weak(catalog.t("fleet.broadcast.no_prompts", &[]));
            return None;
        }
        // ① 프롬프트 선택.
        let selected_title = state
            .prompt_id
            .as_ref()
            .and_then(|id| library.get(id))
            .map(|p| p.title.clone())
            .unwrap_or_else(|| catalog.t("fleet.broadcast.pick_prompt", &[]));
        egui::ComboBox::from_id_salt("fleet_bc_prompt")
            .selected_text(selected_title)
            .width(300.0)
            .show_ui(ui, |ui| {
                for prompt in &library.prompts {
                    let picked = state.prompt_id.as_deref() == Some(prompt.id.as_str());
                    if ui.selectable_label(picked, &prompt.title).clicked() {
                        state.prompt_id = Some(prompt.id.clone());
                        state.params.clear();
                        state.confirm_send = false;
                    }
                }
            });
        // ② 파라미터 + 미리보기.
        let prompt = state.prompt_id.as_ref().and_then(|id| library.get(id));
        let ready_prompt = if let Some(prompt) = prompt {
            let names = prompt.params();
            if !names.is_empty() {
                egui::Grid::new("fleet_bc_params")
                    .num_columns(2)
                    .show(ui, |ui| {
                        for name in &names {
                            ui.monospace(format!("{{{{{name}}}}}"));
                            ui.text_edit_singleline(state.params.entry(name.clone()).or_default());
                            ui.end_row();
                        }
                    });
            }
            let rendered = crate::prompt_library::render(&prompt.body, &state.params);
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.add(egui::Label::new(egui::RichText::new(&rendered).monospace()).wrap());
            });
            let filled = names
                .iter()
                .all(|n| state.params.get(n).is_some_and(|v| !v.trim().is_empty()));
            filled.then_some(rendered)
        } else {
            None
        };
        ui.add_space(6.0);
        ui.separator();
        // ③ 대상 체크박스 — PTY 세션만(구조화는 steer 경로라 브로드캐스트 대상 아님).
        ui.label(catalog.t("fleet.broadcast.targets", &[]));
        egui::ScrollArea::vertical()
            .max_height(180.0)
            .show(ui, |ui| {
                for session in sessions {
                    let Some(key) = session.broadcast_key() else {
                        continue;
                    };
                    let mut checked = state.targets.contains(&key);
                    let label = format!(
                        "{} · {}  ({})",
                        session.title,
                        session.workspace_name,
                        state_label(session.state, catalog)
                    );
                    if ui.checkbox(&mut checked, label).changed() {
                        if checked {
                            state.targets.insert(key);
                        } else {
                            state.targets.remove(&key);
                        }
                        state.confirm_send = false;
                    }
                }
            });
        // ④ 전송 — 현재 PTY 세션과 교집합만 보낸다. 패널 연 뒤 종료된 stale 대상을 제외해
        // 카운트가 실제 전송 수와 일치하게 한다(세션 순회 순서라 결정적).
        ui.add_space(6.0);
        let effective_targets: Vec<(String, runtime::SessionId)> = sessions
            .iter()
            .filter_map(|s| s.broadcast_key())
            .filter(|key| state.targets.contains(key))
            .collect();
        let count = effective_targets.len();
        let can_send = ready_prompt.is_some() && count > 0;
        // 대상 3개 이상은 한 번의 실수로 다수 에이전트를 건드릴 수 있어(리뷰 Low) 2단계
        // 확인을 거친다. 1~2개는 되돌리기 부담이 작아 기존처럼 클릭 한 번으로 보낸다.
        const CONFIRM_THRESHOLD: usize = 3;
        let mut out = None;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    can_send,
                    egui::Button::new(
                        catalog.t("fleet.broadcast.send", &[("count", &count.to_string())]),
                    ),
                )
                .clicked()
            {
                if count >= CONFIRM_THRESHOLD {
                    state.confirm_send = true;
                } else if let Some(prompt_text) = ready_prompt.clone() {
                    out = Some(FleetAction::Broadcast {
                        prompt: prompt_text,
                        targets: effective_targets.clone(),
                    });
                }
            }
            if !can_send {
                ui.weak(catalog.t("fleet.broadcast.fill", &[]));
            }
        });
        if state.confirm_send {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    catalog.t("fleet.broadcast.confirm", &[("count", &count.to_string())]),
                );
                if ui
                    .button(catalog.t("fleet.broadcast.confirm_yes", &[]))
                    .clicked()
                    && let Some(prompt_text) = ready_prompt
                {
                    out = Some(FleetAction::Broadcast {
                        prompt: prompt_text,
                        targets: effective_targets,
                    });
                    state.confirm_send = false;
                }
                if ui
                    .button(catalog.t("fleet.broadcast.confirm_no", &[]))
                    .clicked()
                {
                    state.confirm_send = false;
                }
            });
        }
        out
    }

    /// 배치 스폰 창 — 에이전트 선택 + 개수 + 시작. 닫혀 있으면 아무것도 그리지 않는다.
    /// 시작/닫기 시 상태를 제거한다(broadcast_window와 동일 idiom).
    fn batch_spawn_window(
        &mut self,
        ctx: &egui::Context,
        agents: &[(Arc<str>, Arc<str>)],
        max: u32,
        catalog: &i18n::Catalog,
        library: &PromptLibrary,
    ) -> Option<FleetAction> {
        // 닫혀 있으면(Some 아님) 창을 그리지 않는다.
        self.batch_spawn.as_ref()?;
        let mut action = None;
        let mut open = true;
        egui::Window::new(catalog.t("fleet.batch.title", &[]))
            .id(egui::Id::new("fleet_batch_spawn"))
            .collapsible(false)
            .resizable(false)
            .default_width(360.0)
            .open(&mut open)
            .show(ctx, |ui| {
                action = self.batch_spawn_body(ui, agents, max, catalog, library);
            });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            open = false;
        }
        // 시작했거나(action Some) 닫으면 패널 상태를 버린다.
        if !open || action.is_some() {
            self.batch_spawn = None;
        }
        action
    }

    fn batch_spawn_body(
        &mut self,
        ui: &mut egui::Ui,
        agents: &[(Arc<str>, Arc<str>)],
        max: u32,
        catalog: &i18n::Catalog,
        library: &PromptLibrary,
    ) -> Option<FleetAction> {
        let state = self.batch_spawn.as_mut()?;
        // 등록된 에이전트가 없으면 콤보/개수/시작 버튼 없이 안내만(broadcast의 no_prompts와
        // 동일 idiom) — 사용자는 설정 > 에이전트에서 먼저 등록해야 한다.
        if agents.is_empty() {
            ui.weak(catalog.t("fleet.batch.no_agents", &[]));
            return None;
        }
        // ① 에이전트 선택.
        let selected_name = state
            .agent_id
            .as_deref()
            .and_then(|id| agents.iter().find(|(aid, _)| aid.as_ref() == id))
            .map(|(_, name)| name.to_string())
            .unwrap_or_else(|| catalog.t("fleet.batch.pick_agent", &[]));
        egui::ComboBox::from_id_salt("fleet_batch_agent")
            .selected_text(selected_name)
            .width(260.0)
            .show_ui(ui, |ui| {
                for (id, name) in agents {
                    let picked = state.agent_id.as_deref() == Some(id.as_ref());
                    if ui.selectable_label(picked, name.as_ref()).clicked() {
                        state.agent_id = Some(id.to_string());
                    }
                }
            });
        ui.add_space(8.0);
        // ② 개수 — 1..=max(설정 「fleet_batch_spawn_max」에서 온 상한).
        ui.horizontal(|ui| {
            ui.label(catalog.t("fleet.batch.count", &[]));
            ui.add(egui::DragValue::new(&mut state.count).range(1..=max.max(1)));
        });
        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);
        // ③ 프롬프트(선택) — broadcast_body와 동일 idiom. "없음"이면 빈 세션(PR-S1과
        // 동일), 프롬프트를 고르면 파라미터를 채운 뒤 렌더된 텍스트가 각 에이전트의
        // 초기 argv 프롬프트로 전달된다(PR-S2). prompt_library_enabled 토글과 무관하게
        // 항상 노출한다(broadcast와 동일 결정, A안).
        ui.label(catalog.t("fleet.batch.prompt", &[]));
        let selected_title = state
            .prompt_id
            .as_ref()
            .and_then(|id| library.get(id))
            .map(|p| p.title.clone())
            .unwrap_or_else(|| catalog.t("fleet.batch.prompt_none", &[]));
        egui::ComboBox::from_id_salt("fleet_batch_prompt")
            .selected_text(selected_title)
            .width(260.0)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(
                        state.prompt_id.is_none(),
                        catalog.t("fleet.batch.prompt_none", &[]),
                    )
                    .clicked()
                {
                    state.prompt_id = None;
                    state.params.clear();
                }
                for prompt in &library.prompts {
                    let picked = state.prompt_id.as_deref() == Some(prompt.id.as_str());
                    if ui.selectable_label(picked, &prompt.title).clicked() {
                        state.prompt_id = Some(prompt.id.clone());
                        state.params.clear();
                    }
                }
            });
        let selected_prompt = state.prompt_id.as_ref().and_then(|id| library.get(id));
        // ready: 바깥 None이면 시작 불가(파라미터 미입력), Some(None)이면 빈 세션,
        // Some(Some(text))면 렌더된 프롬프트로 시작.
        let ready: Option<Option<String>> = if let Some(prompt) = selected_prompt {
            let names = prompt.params();
            if !names.is_empty() {
                egui::Grid::new("fleet_batch_params")
                    .num_columns(2)
                    .show(ui, |ui| {
                        for name in &names {
                            ui.monospace(format!("{{{{{name}}}}}"));
                            ui.text_edit_singleline(state.params.entry(name.clone()).or_default());
                            ui.end_row();
                        }
                    });
            }
            let rendered = crate::prompt_library::render(&prompt.body, &state.params);
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.add(egui::Label::new(egui::RichText::new(&rendered).monospace()).wrap());
            });
            let filled = names
                .iter()
                .all(|n| state.params.get(n).is_some_and(|v| !v.trim().is_empty()));
            if filled { Some(Some(rendered)) } else { None }
        } else {
            Some(None)
        };
        ui.add_space(10.0);
        // ④ 시작.
        let mut out = None;
        let can_start = state.agent_id.is_some() && ready.is_some();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    can_start,
                    egui::Button::new(
                        catalog.t("fleet.batch.start", &[("count", &state.count.to_string())]),
                    ),
                )
                .clicked()
                && let Some(agent_id) = state.agent_id.clone()
                && let Some(prompt) = ready.clone()
            {
                out = Some(FleetAction::BatchSpawn {
                    agent_id,
                    count: state.count,
                    prompt,
                });
            }
            // 프롬프트를 골랐지만 파라미터가 안 채워졌을 때만 힌트(broadcast의 fill과
            // 동일 idiom) — 프롬프트 없음은 항상 시작 가능이라 힌트가 필요 없다.
            if selected_prompt.is_some() && ready.is_none() {
                ui.weak(catalog.t("fleet.batch.fill_params", &[]));
            }
        });
        out
    }
}

/// 헤더 우측 버튼 클릭.
enum HeaderClick {
    Launch,
    Broadcast,
    BatchSpawn,
}

/// 주의 섹션이 돌려주는 intent — 승인 결정·대기 응답·이동.
#[derive(Default)]
struct AttentionOutput {
    approval_decision: Option<crate::ui::approvals::ApprovalDecision>,
    waiting_action: Option<crate::ui::inbox_waiting::WaitingAction>,
    goto: Option<crate::ui::notifications::AgentNotificationTarget>,
}

/// 「지금 처리」 — 가장 오래 막힌 항목 하나를 크게, 나머지는 요약 줄로.
///
/// 목록을 훑고 고르는 대신 분류(triage)하듯 처리한다. 승인이면 실행할 도구와 인자를
/// **여기서 바로** 보여줘 터미널로 이동하지 않고 판단할 수 있게 한다 — 지금까지 승인이
/// 밀린 실제 원인이 그 왕복이었다(2026-08-08).
#[allow(clippy::too_many_arguments)]
fn render_hero(
    ui: &mut egui::Ui,
    catalog: &i18n::Catalog,
    queue: &[BlockedItem],
    waiting_cards: &[crate::ui::inbox_waiting::WaitingCard],
    waiting_ui: &mut crate::ui::inbox_waiting::InboxWaitingUi,
    skipped: &mut HashSet<String>,
    now: i64,
) -> AttentionOutput {
    let mut out = AttentionOutput::default();
    ui.label(
        egui::RichText::new(catalog.t("fleet.hero.now", &[]))
            .small()
            .weak(),
    );
    ui.add_space(4.0);
    if queue.is_empty() {
        skipped.clear();
        // 빈 입력이어도 반드시 호출 — 안에서 stale 입력버퍼를 정리한다(2026-07-17 P2).
        out.waiting_action = waiting_ui.render(ui, catalog, &[]);
        ui.label(
            egui::RichText::new(catalog.t("fleet.hero.clear", &[]))
                .weak()
                .small(),
        );
        return out;
    }
    // 사라진 항목의 건너뛰기 기록은 버린다. 전부 건너뛴 상태면 앞으로 되돌아간다.
    let live: HashSet<&str> = queue.iter().map(|item| item.key.as_str()).collect();
    skipped.retain(|key| live.contains(key.as_str()));
    if skipped.len() >= queue.len() {
        skipped.clear();
    }
    let Some(hero) = queue.iter().find(|item| !skipped.contains(&item.key)) else {
        out.waiting_action = waiting_ui.render(ui, catalog, &[]);
        return out;
    };
    // 히어로가 승인이면 대기 위젯은 빈 입력으로 호출해 정리만 시킨다.
    let hero_card: &[crate::ui::inbox_waiting::WaitingCard] = match &hero.kind {
        BlockedKind::NeedsInput { card_index } => &waiting_cards[*card_index..=*card_index],
        _ => &[],
    };

    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let badge = match &hero.kind {
                BlockedKind::NeedsInput { .. } => catalog.t("fleet.hero.needs_input", &[]),
                BlockedKind::Approval { .. } => catalog.t("fleet.hero.approval", &[]),
            };
            ui.label(
                egui::RichText::new(badge)
                    .small()
                    .color(status_color(AgentVisualState::Waiting)),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(catalog.t(
                        "fleet.blocked_for",
                        &[(
                            "value",
                            &crate::fleet::format_blocked_duration(now, hero.blocked_since),
                        )],
                    ))
                    .small()
                    .strong()
                    .color(status_color(AgentVisualState::Error)),
                );
            });
        });
        ui.add(egui::Label::new(egui::RichText::new(&hero.title).strong().size(15.0)).truncate());
        ui.add(egui::Label::new(egui::RichText::new(&hero.context).small().weak()).truncate());
        ui.add_space(6.0);
        // 승인이면 실행할 인자를 그대로 — 가서 보지 않고 판단하는 게 핵심이다.
        if let BlockedKind::Approval {
            arguments_preview, ..
        } = &hero.kind
            && !arguments_preview.is_empty()
        {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(arguments_preview)
                        .monospace()
                        .small()
                        .weak(),
                )
                .truncate(),
            );
            ui.add_space(6.0);
        }
        // 입력 대기는 기존 카드 위젯에 위임 — 자유 응답·로그 미리보기를 잃지 않는다.
        // 히어로 한 장만 넘기므로 나머지 카드의 입력 버퍼는 정리 대상이 된다(보이지도
        // 않는 카드의 초안이라 잃어도 무해하다).
        out.waiting_action = waiting_ui.render(ui, catalog, hero_card);
        ui.horizontal(|ui| {
            match &hero.kind {
                BlockedKind::Approval { id, .. } => {
                    if ui
                        .button(catalog.t("inbox.approval.approve", &[]))
                        .clicked()
                    {
                        out.approval_decision = Some(crate::ui::approvals::ApprovalDecision {
                            id: id.clone(),
                            allowed: true,
                            remember: false,
                        });
                    }
                    if ui.button(catalog.t("inbox.approval.deny", &[])).clicked() {
                        out.approval_decision = Some(crate::ui::approvals::ApprovalDecision {
                            id: id.clone(),
                            allowed: false,
                            remember: false,
                        });
                    }
                }
                // 입력 대기는 기존 카드 위젯이 통째로 그린다(자유 응답·로그 미리보기 포함).
                BlockedKind::NeedsInput { .. } => {}
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // 큐가 하나뿐이면 건너뛸 곳이 없다 — 막다른 버튼을 만들지 않는다.
                if queue.len() > 1
                    && ui
                        .button(catalog.t("fleet.hero.skip", &[]))
                        .on_hover_text(catalog.t("fleet.hero.next", &[]))
                        .clicked()
                {
                    skipped.insert(hero.key.clone());
                }
            });
        });
    });

    // 나머지 대기 — 제목과 막힌 시간만.
    let rest: Vec<&BlockedItem> = queue.iter().filter(|item| item.key != hero.key).collect();
    if !rest.is_empty() {
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(catalog.t("fleet.hero.next", &[]))
                .small()
                .weak(),
        );
        for item in rest {
            ui.horizontal(|ui| {
                ui.add(egui::Label::new(egui::RichText::new(&item.title).small()).truncate());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(crate::fleet::format_blocked_duration(
                            now,
                            item.blocked_since,
                        ))
                        .small()
                        .color(status_color(AgentVisualState::Waiting)),
                    );
                });
            });
        }
    }
    out
}

/// 카드 좌측 상태색과 별개인 **워크스페이스 고유색** — 어느 프로젝트 일인지 읽지 않고
/// 구분하게 한다. 사이드바 아바타·pane 상단선과 같은 색 체계다.
fn card_accent(
    workspaces: &[crate::ui::file_tree::SidebarWorkspaceEntry],
    session: &FleetSession,
) -> egui::Color32 {
    crate::ui::file_tree::workspace_accent(workspaces, &session.workspace_id)
}

/// 상단 헤더: 제목 + 총계 + (우측) 사용량 + 배치 스폰·브로드캐스트·새 에이전트 버튼 + 상태별 칩.
fn header(
    ui: &mut egui::Ui,
    summary: FleetSummary,
    catalog: &i18n::Catalog,
    has_broadcast_target: bool,
    usage: UsageReadout<'_>,
) -> Option<HeaderClick> {
    let mut click = None;
    ui.horizontal(|ui| {
        ui.heading(catalog.t("fleet.title", &[]));
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(catalog.t(
                "fleet.session_count",
                &[("count", &summary.total.to_string())],
            ))
            .weak(),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button(catalog.t("fleet.new_agent", &[])).clicked() {
                click = Some(HeaderClick::Launch);
            }
            // 브로드캐스트는 실행 중 에이전트가 있을 때만.
            if has_broadcast_target && ui.button(catalog.t("fleet.broadcast", &[])).clicked() {
                click = Some(HeaderClick::Broadcast);
            }
            // 배치 스폰은 등록된 에이전트가 없어도 열 수 있다(패널 안에서 안내, PR-S1).
            if ui.button(catalog.t("fleet.batch", &[])).clicked() {
                click = Some(HeaderClick::BatchSpawn);
            }
            // 사용량은 일괄 실행·브로드캐스트 **바로 옆**에 둔다 — 여러 에이전트를 한꺼번에
            // 돌리기 전에 남은 한도가 보여야 한다. 하단 상태바와 같은 렌더러라 숫자도 같다.
            ui.add_space(10.0);
            crate::app::top_provider_usage(ui, usage.claude, usage.codex, usage.codex_meta);
        });
    });
    ui.add_space(8.0);
    ui.horizontal_wrapped(|ui| {
        for group in SessionGroup::ORDER {
            chip(ui, group, group_count(summary, group), catalog);
        }
    });
    click
}

/// 상태별 칩 — 색 점 + "라벨 n". 0이면 흐리게(회색) 표시.
fn chip(ui: &mut egui::Ui, group: SessionGroup, count: usize, catalog: &i18n::Catalog) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(88.0, 22.0), egui::Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let dim = count == 0;
    let dot_color = if dim {
        ui.visuals().weak_text_color()
    } else {
        status_color(group_state(group))
    };
    let text_color = if dim {
        ui.visuals().weak_text_color()
    } else {
        ui.visuals().text_color()
    };
    let label = group_label(group, catalog);
    let p = ui.painter();
    p.circle_filled(
        egui::pos2(rect.left() + 6.0, rect.center().y),
        4.0,
        dot_color,
    );
    p.text(
        egui::pos2(rect.left() + 16.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        format!("{label} {count}"),
        egui::FontId::proportional(12.5),
        text_color,
    );
}

/// 세션 카드 하나 — 좌측 상태 바 + 우측 워크스페이스 띠 + 제목/상태/워크스페이스/보조 줄.
/// 클릭 시 true.
fn card(
    ui: &mut egui::Ui,
    session: &FleetSession,
    catalog: &i18n::Catalog,
    accent: egui::Color32,
    now: i64,
) -> bool {
    let size = egui::vec2(252.0, 96.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    if !ui.is_rect_visible(rect) {
        return response.clicked();
    }
    let visuals = ui.visuals();
    let bg = if response.hovered() {
        visuals.widgets.hovered.bg_fill
    } else {
        visuals.faint_bg_color
    };
    let border = visuals.widgets.noninteractive.bg_stroke.color;
    let state_color = status_color(session.state);
    {
        let p = ui.painter();
        p.rect_filled(rect, 6.0, bg);
        p.rect_stroke(
            rect,
            6.0,
            egui::Stroke::new(1.0, border),
            egui::StrokeKind::Inside,
        );
        // 좌측 상태 바 — 정렬을 지배하는 신호라 시선이 먼저 닿는 자리에 둔다.
        let bar = egui::Rect::from_min_size(rect.left_top(), egui::vec2(4.0, rect.height()));
        p.rect_filled(bar, 6.0, state_color);
        // 우측 워크스페이스 띠 — 어느 프로젝트인지 읽지 않고 구분하게 한다. 상태색과
        // 겹치지 않게 반대편에 두어 둘 중 뭐가 상태인지 헷갈리지 않는다.
        let accent_bar = egui::Rect::from_min_size(
            rect.right_top() - egui::vec2(3.0, 0.0),
            egui::vec2(3.0, rect.height()),
        );
        p.rect_filled(accent_bar, 6.0, accent);
    }

    // 내용은 child UI(top-down)로 — 라벨 truncate가 카드 폭을 넘지 않게 클립한다.
    let inner = rect.shrink2(egui::vec2(14.0, 10.0));
    let mut content = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    content.set_clip_rect(inner.intersect(ui.clip_rect()));
    content.spacing_mut().item_spacing.y = 3.0;
    // 1행: 제목.
    content.add(egui::Label::new(egui::RichText::new(&session.title).strong()).truncate());
    // 2행: 상태 라벨(색) + [막힌 시간] + 워크스페이스 + active/warm.
    content.horizontal(|ui| {
        ui.label(
            egui::RichText::new(state_label(session.state, catalog))
                .small()
                .color(state_color),
        );
        // 막힌 시간은 정렬 키 그 자체다 — 화면에 보여야 "왜 이게 위에 있나"를
        // 설명할 필요가 없다(2026-08-08 정렬 기준).
        if let Some(since) = session.blocked_since {
            ui.label(
                egui::RichText::new(catalog.t(
                    "fleet.blocked_for",
                    &[("value", &crate::fleet::format_blocked_duration(now, since))],
                ))
                .small()
                .color(state_color),
            );
        }
        ui.label(egui::RichText::new("·").small().weak());
        ui.add(
            egui::Label::new(egui::RichText::new(&session.workspace_name).small().weak())
                .truncate(),
        );
        if !session.active_workspace {
            ui.label(
                egui::RichText::new(catalog.t("fleet.warm", &[]))
                    .small()
                    .weak(),
            );
        }
    });
    // 3행: 대기 사유 우선, 없으면 에이전트 라인("Codex · gpt-5.5 · xhigh").
    if let Some(message) = &session.waiting_message {
        content.add(
            egui::Label::new(egui::RichText::new(message).small().color(state_color)).truncate(),
        );
    } else if let Some(line) = &session.agent_line {
        content
            .add(egui::Label::new(egui::RichText::new(line).small().weak().monospace()).truncate());
    }

    response.clicked()
}

/// 상태별 라벨(i18n).
/// 묶음 구분 헤더 — 라벨 + 개수 + 얇은 선.
fn section_header(ui: &mut egui::Ui, group: SessionGroup, count: usize, catalog: &i18n::Catalog) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(group_label(group, catalog))
                .small()
                .color(status_color(group_state(group))),
        );
        ui.label(egui::RichText::new(count.to_string()).small().weak());
    });
    ui.add_space(4.0);
}

/// 묶음을 대표하는 상태 — 칩·구분선 색에만 쓴다.
fn group_state(group: SessionGroup) -> AgentVisualState {
    match group {
        SessionGroup::Blocked => AgentVisualState::Waiting,
        SessionGroup::Active => AgentVisualState::Active,
        SessionGroup::Errored => AgentVisualState::Error,
        SessionGroup::Finished => AgentVisualState::Complete,
    }
}

fn group_label(group: SessionGroup, catalog: &i18n::Catalog) -> String {
    let key = match group {
        SessionGroup::Blocked => "fleet.group.blocked",
        SessionGroup::Active => "fleet.group.active",
        SessionGroup::Errored => "fleet.group.errored",
        SessionGroup::Finished => "fleet.group.finished",
    };
    catalog.t(key, &[])
}

fn group_count(summary: FleetSummary, group: SessionGroup) -> usize {
    match group {
        SessionGroup::Blocked => summary.blocked,
        SessionGroup::Active => summary.active,
        SessionGroup::Errored => summary.errored,
        SessionGroup::Finished => summary.finished,
    }
}

fn state_label(state: AgentVisualState, catalog: &i18n::Catalog) -> String {
    let key = match state {
        AgentVisualState::Waiting => "fleet.state.waiting",
        AgentVisualState::Error => "fleet.state.error",
        AgentVisualState::Complete => "fleet.state.done",
        AgentVisualState::Active => "fleet.state.working",
        AgentVisualState::Idle => "fleet.state.idle",
        AgentVisualState::Off => "fleet.state.off",
    };
    catalog.t(key, &[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::approvals::PendingApprovalItem;
    use crate::ui::file_tree::{
        SidebarSessionSummary, SidebarWorkspaceEntry, SidebarWorkspaceState,
    };
    use crate::ui::inbox_waiting::{InboxWaitingUi, WaitingCard};
    use std::collections::HashMap;

    fn catalog() -> i18n::Catalog {
        i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap()
    }

    fn approval(id: &str, session_key: Option<&str>) -> PendingApprovalItem {
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

    fn waiting_card(workspace_id: &str, session: u64) -> WaitingCard {
        WaitingCard {
            workspace_id: workspace_id.to_owned(),
            session: runtime::SessionId(session),
            workspace_name: "my-project".to_owned(),
            session_title: "claude".to_owned(),
            headline: Some("계속할까요?".to_owned()),
            preview_source: None,
        }
    }

    fn workspace(id: &str) -> SidebarWorkspaceEntry {
        SidebarWorkspaceEntry {
            id: id.to_owned(),
            name: id.to_owned(),
            state: SidebarWorkspaceState::Active,
            summary: SidebarSessionSummary::default(),
        }
    }

    fn pty_session(workspace_id: &str, session: u64, state: AgentVisualState) -> FleetSession {
        FleetSession {
            workspace_id: workspace_id.to_owned(),
            workspace_name: workspace_id.to_owned(),
            target: FleetTarget::Pty {
                session: runtime::SessionId(session),
                tab: runtime::MuxTabId("t1".into()),
                pane: runtime::MuxPaneId("p1".into()),
            },
            title: format!("session-{session}"),
            state,
            agent_line: None,
            waiting_message: None,
            active_workspace: true,
            blocked_since: None,
        }
    }

    /// 페이지를 한 번 그리고 결과를 돌려주는 최소 하네스. 클릭은 하지 않는다.
    fn draw(
        ui_fleet: &mut FleetUi,
        sessions: &[FleetSession],
        pending: &[PendingApprovalItem],
        waiting_cards: &[(WaitingCard, i64)],
        waiting_ui: &mut InboxWaitingUi,
        workspaces: &[SidebarWorkspaceEntry],
    ) -> FleetPageOutput {
        let ctx = egui::Context::default();
        let catalog = catalog();
        let library = crate::prompt_library::PromptLibrary::default();
        let summary = FleetSummary::from_states(sessions.iter().map(|s| s.state));
        let mut out = FleetPageOutput::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            out = ui_fleet.render(
                ui,
                sessions,
                summary,
                &catalog,
                &library,
                BatchSpawnInput {
                    agents: &[],
                    max: 4,
                },
                AttentionInput {
                    pending,
                    workspace_names: &HashMap::new(),
                    session_titles: &HashMap::new(),
                    waiting_cards,
                    waiting_ui,
                },
                UsageReadout {
                    claude: None,
                    codex: None,
                    codex_meta: None,
                },
                workspaces,
            );
        });
        out
    }

    /// 2026-08-08 통합의 핵심 위험: 주의 섹션이 `sessions.is_empty()` 조기반환 **뒤에** 오면
    /// 세션을 전부 닫았는데 승인만 남은 상태에서 승인 카드가 사라지고, InboxWaitingUi의
    /// stale 입력버퍼 정리(2026-07-17 P2)도 건너뛴다. 순서를 이 테스트가 고정한다.
    #[test]
    fn 세션이_없어도_승인_카드는_그린다() {
        let mut fleet = FleetUi::default();
        let mut waiting = InboxWaitingUi::new();
        let out = draw(
            &mut fleet,
            &[],
            &[approval("a1", Some("ws-1:7"))],
            &[],
            &mut waiting,
            &[workspace("ws-1")],
        );
        // 클릭이 없으니 액션은 없지만, 패닉 없이 승인과 빈 상태가 함께 그려져야 한다.
        assert!(out.approval_decision.is_none());
        assert!(out.grid.is_none());
    }

    /// 대기 카드가 사라진 다음 프레임에도 정리 경로에 도달해야 한다 — 조기반환으로
    /// 건너뛰면 여기서 패닉하거나 버퍼가 남는다.
    #[test]
    fn 대기카드가_사라져도_다음_프레임에_정리_경로에_도달한다() {
        let mut fleet = FleetUi::default();
        let mut waiting = InboxWaitingUi::new();
        let first = draw(
            &mut fleet,
            &[],
            &[],
            &[(waiting_card("ws-1", 7), 0)],
            &mut waiting,
            &[workspace("ws-1")],
        );
        assert!(first.waiting_action.is_none());
        let second = draw(
            &mut fleet,
            &[],
            &[],
            &[],
            &mut waiting,
            &[workspace("ws-1")],
        );
        assert!(second.waiting_action.is_none());
    }

    /// 히어로는 **가장 오래 막힌 항목 하나**를 보여준다. 막힌 게 없으면 안내 문구로
    /// 바뀐다 — 이 화면은 비어 있는 게 정상이다(2026-08-08).
    #[test]
    fn 히어로는_막힌_것이_있을_때만_항목을_보여준다() {
        use egui_kittest::kittest::Queryable;

        let cat = catalog();
        let clear = cat.t("fleet.hero.clear", &[]);
        for (pending, has_item) in [(Vec::new(), false), (vec![approval("a1", None)], true)] {
            let mut harness = egui_kittest::Harness::builder()
                .with_size(egui::vec2(1100.0, 600.0))
                .build_ui(|ui| {
                    let catalog = catalog();
                    let library = crate::prompt_library::PromptLibrary::default();
                    let mut fleet = FleetUi::default();
                    let mut waiting = InboxWaitingUi::new();
                    let sessions = [pty_session("ws-1", 7, AgentVisualState::Active)];
                    let summary = FleetSummary::from_states(sessions.iter().map(|s| s.state));
                    let _ = fleet.render(
                        ui,
                        &sessions,
                        summary,
                        &catalog,
                        &library,
                        BatchSpawnInput {
                            agents: &[],
                            max: 4,
                        },
                        AttentionInput {
                            pending: &pending,
                            workspace_names: &HashMap::new(),
                            session_titles: &HashMap::new(),
                            waiting_cards: &[],
                            waiting_ui: &mut waiting,
                        },
                        UsageReadout {
                            claude: None,
                            codex: None,
                            codex_meta: None,
                        },
                        &[workspace("ws-1")],
                    );
                });
            harness.run();
            // 승인이 있으면 그 도구 이름이 히어로 제목으로 보이고, 없으면 안내 문구가 뜬다.
            assert_eq!(
                harness.query_by_label("read_file").is_some(),
                has_item,
                "승인 {}건일 때 히어로 항목 표시가 기대와 다르다",
                pending.len()
            );
            assert_eq!(
                harness.query_by_label(&clear).is_some(),
                !has_item,
                "승인 {}건일 때 「{clear}」 표시가 기대와 다르다",
                pending.len()
            );
        }
    }

    /// 승인과 입력 대기는 **하나의 큐**에 서고 오래 막힌 순으로 정렬된다 — 사용자에겐
    /// 둘 다 "에이전트가 나를 기다린다"는 같은 종류의 일이다.
    #[test]
    fn 큐는_승인과_대기를_섞어_오래_막힌_순으로_세운다() {
        let cards = vec![
            (waiting_card("ws-1", 7), 500_i64),
            (waiting_card("ws-2", 9), 100),
        ];
        let queue = blocked_queue(
            &[approval("a1", None)],
            &cards,
            &HashMap::new(),
            &HashMap::new(),
            "unknown",
        );
        assert_eq!(queue.len(), 3);
        let since: Vec<i64> = queue.iter().map(|item| item.blocked_since).collect();
        assert!(
            since.windows(2).all(|w| w[0] <= w[1]),
            "오래 막힌 순이 아니다: {since:?}"
        );
        assert_eq!(
            queue[0].blocked_since, 0,
            "승인(created_at=0)이 가장 오래됐다"
        );
        assert!(matches!(queue[0].kind, BlockedKind::Approval { .. }));
    }

    /// 묶음 헤더는 **비어 있지 않은 묶음만** 나온다 — 빈 헤더가 화면을 채우면 안 된다.
    /// 그리고 막힌 세션이 있으면 그 묶음 헤더가 반드시 보여야 한다.
    #[test]
    fn 묶음_헤더는_비어있지_않은_묶음만_보인다() {
        use egui_kittest::kittest::Queryable;

        let cat = catalog();
        let blocked_label = cat.t("fleet.group.blocked", &[]);
        let errored_label = cat.t("fleet.group.errored", &[]);
        let mut sessions = vec![pty_session("ws-1", 7, AgentVisualState::Waiting)];
        sessions[0].blocked_since = Some(1);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(900.0, 600.0))
            .build_ui(|ui| {
                let catalog = catalog();
                let library = crate::prompt_library::PromptLibrary::default();
                let mut fleet = FleetUi::default();
                let mut waiting = InboxWaitingUi::new();
                let summary = FleetSummary::from_states(sessions.iter().map(|s| s.state));
                let _ = fleet.render(
                    ui,
                    &sessions,
                    summary,
                    &catalog,
                    &library,
                    BatchSpawnInput {
                        agents: &[],
                        max: 4,
                    },
                    AttentionInput {
                        pending: &[],
                        workspace_names: &HashMap::new(),
                        session_titles: &HashMap::new(),
                        waiting_cards: &[],
                        waiting_ui: &mut waiting,
                    },
                    UsageReadout {
                        claude: None,
                        codex: None,
                        codex_meta: None,
                    },
                    &[workspace("ws-1")],
                );
            });
        harness.run();
        assert!(
            harness.query_by_label(&blocked_label).is_some(),
            "막힌 세션이 있는데 「{blocked_label}」 헤더가 없다"
        );
        assert!(
            harness.query_by_label(&errored_label).is_none(),
            "오류가 없는데 「{errored_label}」 헤더를 그렸다"
        );
    }

    /// 워크스페이스 띠는 프로젝트마다 다르고 같은 프로젝트에서는 안정적이어야 한다 —
    /// 픽셀을 보지 않고 색 계산만 검증한다.
    #[test]
    fn 카드_워크스페이스색은_프로젝트마다_다르고_같은_프로젝트에서_안정적이다() {
        let workspaces = [workspace("ws-1"), workspace("ws-2")];
        let a = pty_session("ws-1", 1, AgentVisualState::Active);
        let b = pty_session("ws-2", 2, AgentVisualState::Active);
        let a_again = pty_session("ws-1", 3, AgentVisualState::Waiting);
        assert_ne!(
            card_accent(&workspaces, &a),
            card_accent(&workspaces, &b),
            "다른 워크스페이스가 같은 색이면 구분이 안 된다"
        );
        assert_eq!(
            card_accent(&workspaces, &a),
            card_accent(&workspaces, &a_again),
            "같은 워크스페이스는 세션·상태가 달라도 같은 색이어야 한다"
        );
    }
}
