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
//! 상태 색은 앱 공용 팔레트(`agent_visuals::status_color`)를 재사용한다.

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
    /// 「이 턴 끝나면 이거 해」 — 한 세션에 다음 프롬프트 한 칸을 예약한다. 브로드캐스트와
    /// 달리 **지금 보내지 않는다**: App이 hook의 turn_done을 보고 턴이 끝난 뒤 보낸다.
    /// `prompt`가 비면 예약 해제다.
    ScheduleFollowUp {
        workspace_id: String,
        session: runtime::SessionId,
        prompt: String,
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

/// 다음 단계 예약 패널 상태. 대상 세션은 패널을 여는 순간 고정된다 — 브로드캐스트와
/// 달리 대상이 하나라 고르는 단계가 없다.
struct FollowUpState {
    workspace_id: String,
    session: runtime::SessionId,
    /// 카드 제목 — 어느 세션에 예약하는지 패널에서 다시 보여준다.
    title: String,
    /// 편집 중인 원문. 이미 예약된 세션이면 그 값으로 시작해 고쳐 쓸 수 있다.
    text: String,
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
    /// 승인 대기 중인 구조화(App Server) 세션. MCP 승인과 응답 경로가 달라 따로 받는다.
    pub structured: &'a [StructuredApproval],
}

/// 승인 대기 중인 구조화 세션 한 건 — 히어로 큐에 세우는 데 필요한 최소 정보.
#[derive(Clone, Debug, PartialEq)]
pub struct StructuredApproval {
    pub session_id: String,
    pub title: String,
    pub workspace_name: Option<String>,
    pub blocked_since: i64,
}

/// 나를 막고 있는 항목 하나 — 승인이든 입력 대기든 같은 큐에 선다.
///
/// 사용자에게는 둘 다 "에이전트가 나를 기다린다"는 같은 종류의 일이라 화면에서 나누지
/// 않는다. 정렬 키는 `blocked_since` 하나뿐이고 그 값이 카드에 그대로 보인다.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockedItem {
    /// 안정 키 — 정렬 동률을 가르고 테스트가 항목을 집는 데 쓴다.
    pub key: String,
    pub title: String,
    /// "워크스페이스 · 세션" 맥락 줄.
    pub context: String,
    pub blocked_since: i64,
    pub kind: BlockedKind,
    /// 가리키는 세션 — 세션 카드와 이어 붙여 같은 막힘을 두 번 그리지 않는다.
    /// 승인의 세션 키를 못 읽으면 None.
    pub session: Option<BlockedRef>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BlockedKind {
    /// MCP 승인 — 실행할 도구와 인자를 그대로 보여준다(가서 보지 않고 판단).
    Approval {
        id: String,
        tool_name: String,
        arguments_preview: String,
    },
    /// 구조화(App Server) 세션 승인 — 응답이 id 기반 steer 경로라 따로 둔다.
    StructuredApproval { session_id: String },
    /// PTY 입력 대기 — 기존 대기 카드 위젯에 그대로 위임한다(자유 응답·로그 미리보기를
    /// 잃지 않으려고 y/n 버튼을 새로 만들지 않는다). 값은 넘겨받은 카드 슬라이스의 인덱스.
    NeedsInput { card_index: usize },
}

/// 큐 항목이 가리키는 세션 — `FleetSession`과 같은 식별자로 맞춰 본다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockedRef {
    /// `SessionId`는 워크스페이스마다 재사용될 수 있어 쌍으로만 식별한다.
    Pty {
        workspace_id: String,
        session: runtime::SessionId,
    },
    Structured {
        session_id: String,
    },
}

/// 승인·입력 대기를 하나의 큐로 합치고 **오래 막힌 순**으로 세운다.
///
/// 순수 함수라 UI 없이 순서 계약을 검증할 수 있다. `now`는 쓰지 않는다 — 정렬은
/// 절대 시각으로 하고 표시할 때만 경과를 계산한다.
pub fn blocked_queue(
    approvals: &[crate::ui::approvals::PendingApprovalItem],
    structured: &[StructuredApproval],
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
        let session = parsed
            .as_ref()
            .map(|(workspace_id, session)| BlockedRef::Pty {
                workspace_id: workspace_id.clone(),
                session: *session,
            });
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
            session,
        });
    }
    for row in structured {
        items.push(BlockedItem {
            key: format!("structured:{}", row.session_id),
            title: row.title.clone(),
            context: row
                .workspace_name
                .clone()
                .unwrap_or_else(|| unknown_session.to_owned()),
            blocked_since: row.blocked_since,
            kind: BlockedKind::StructuredApproval {
                session_id: row.session_id.clone(),
            },
            session: Some(BlockedRef::Structured {
                session_id: row.session_id.clone(),
            }),
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
            session: Some(BlockedRef::Pty {
                workspace_id: card.workspace_id.clone(),
                session: card.session,
            }),
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

fn session_ref(session: &FleetSession) -> BlockedRef {
    match &session.target {
        FleetTarget::Pty { session: id, .. } => BlockedRef::Pty {
            workspace_id: session.workspace_id.clone(),
            session: *id,
        },
        FleetTarget::Structured { session_id } => BlockedRef::Structured {
            session_id: session_id.clone(),
        },
    }
}

/// 「막힌 것」 묶음의 한 줄. 값은 큐(`Expanded`·`Compact`) 또는 세션 슬라이스(`Card`)의
/// 인덱스다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockedRow {
    /// 가장 오래 막힌 큐 항목 — 그 자리에서 펼쳐 결정 UI를 그린다.
    Expanded(usize),
    /// 세션 카드 — 막힌 시간이 카드에 이미 있어 큐 항목을 대신한다.
    Card(usize),
    /// 세션 카드가 없는 결정(세션 키를 못 읽은 승인 등) — 제목·맥락·시간 한 줄.
    Compact(usize),
}

/// 큐(결정할 것)와 막힌 세션 카드를 **한 묶음**으로 잇는다.
///
/// 맨 앞 큐 항목은 펼치고 그 항목이 가리키는 세션 카드는 뺀다 — 같은 막힘이 두 번
/// 보이면 화면을 둘로 나누던 때와 다를 게 없다(2026-09-05). 뒤 항목은 세션 카드로
/// 대신하고, 카드가 없는 결정만 요약 줄로 남긴다. 큐에 없는 막힌 세션은 뒤에 붙는다.
pub fn blocked_rows(queue: &[BlockedItem], sessions: &[FleetSession]) -> Vec<BlockedRow> {
    let mut rows = Vec::with_capacity(queue.len() + sessions.len());
    let mut used = vec![false; sessions.len()];
    for (index, item) in queue.iter().enumerate() {
        let matched = item
            .session
            .as_ref()
            .and_then(|wanted| sessions.iter().position(|s| session_ref(s) == *wanted))
            .filter(|&slot| !used[slot]);
        if let Some(slot) = matched {
            used[slot] = true;
        }
        rows.push(match (index, matched) {
            (0, _) => BlockedRow::Expanded(0),
            (_, Some(slot)) => BlockedRow::Card(slot),
            (_, None) => BlockedRow::Compact(index),
        });
    }
    rows.extend(
        used.iter()
            .enumerate()
            .filter(|(_, taken)| !**taken)
            .map(|(slot, _)| BlockedRow::Card(slot)),
    );
    rows
}

/// 페이지가 App에 돌려주는 intent 묶음. 그리드·승인·대기가 각각 독립적으로 발생할 수 있다.
#[derive(Default)]
pub struct FleetPageOutput {
    pub grid: Option<FleetAction>,
    pub approval_decision: Option<crate::ui::approvals::ApprovalDecision>,
    pub waiting_action: Option<crate::ui::inbox_waiting::WaitingAction>,
    pub goto: Option<crate::ui::notifications::AgentNotificationTarget>,
    /// 구조화 세션 승인 — (session_id, 허용 여부). MCP 승인과 응답 경로가 달라 따로 낸다.
    pub structured_decision: Option<(String, bool)>,
}

#[derive(Default)]
pub struct FleetUi {
    /// Some이면 브로드캐스트 패널이 열려 있다.
    broadcast: Option<BroadcastState>,
    /// Some이면 배치 스폰 패널이 열려 있다.
    batch_spawn: Option<BatchSpawnState>,
    /// Some이면 다음 단계 예약 패널이 열려 있다.
    followup: Option<FollowUpState>,
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
                match header(ui, summary, catalog, has_broadcast_target) {
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
                    structured,
                } = attention;
                let queue = blocked_queue(
                    pending,
                    structured,
                    waiting_cards,
                    workspace_names,
                    session_titles,
                    &catalog.t("inbox.approval.unknown_session", &[]),
                );
                let plain_cards: Vec<crate::ui::inbox_waiting::WaitingCard> =
                    waiting_cards.iter().map(|(card, _)| card.clone()).collect();

                // 한 목록 — 막힌 것은 맨 위 묶음이고, 그중 가장 오래 막힌 하나만 그 자리에서
                // 펼쳐 바로 결정한다. 「지금 처리」 컬럼을 따로 두면 같은 막힘이 두 곳에 보여
                // 화면을 둘로 나눌 이유가 없었다(2026-09-05 사용자 지적).
                if queue.is_empty() {
                    // 빈 입력이어도 반드시 호출 — 안에서 stale 입력버퍼를 정리한다(2026-07-17 P2).
                    out.waiting_action = waiting_ui.render(ui, catalog, &[]);
                }
                // 묶음별 섹션 — 막힌 것이 맨 위다. 정렬은 순수 함수가 하고 여기서는 그리기만
                // 한다(순서 계약을 UI 없이 테스트하려고).
                let grouped = crate::fleet::group_sessions(sessions.to_vec());
                egui::ScrollArea::vertical()
                    .id_salt("fleet_sessions")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (group, members) in SessionGroup::ORDER.into_iter().zip(grouped) {
                            // 막힌 묶음만 큐와 잇는다 — 펼칠 항목 하나, 카드 없는 결정은 요약 줄.
                            let mut expanded: Option<&BlockedItem> = None;
                            let mut compact: Vec<&BlockedItem> = Vec::new();
                            let mut cards: Vec<&FleetSession> = Vec::new();
                            if group == SessionGroup::Blocked {
                                for row in blocked_rows(&queue, &members) {
                                    match row {
                                        BlockedRow::Expanded(i) => expanded = Some(&queue[i]),
                                        BlockedRow::Compact(i) => compact.push(&queue[i]),
                                        BlockedRow::Card(slot) => cards.push(&members[slot]),
                                    }
                                }
                            } else {
                                cards.extend(members.iter());
                            }
                            let count =
                                usize::from(expanded.is_some()) + compact.len() + cards.len();
                            if count == 0 {
                                continue;
                            }
                            section_header(ui, group, count, catalog);
                            if let Some(item) = expanded {
                                let decided = render_expanded(
                                    ui,
                                    catalog,
                                    item,
                                    &plain_cards,
                                    waiting_ui,
                                    now,
                                );
                                out.approval_decision = decided.approval_decision;
                                out.waiting_action = decided.waiting_action;
                                out.goto = decided.goto;
                                out.structured_decision = decided.structured_decision;
                                ui.add_space(6.0);
                            }
                            for item in compact {
                                compact_row(ui, item, now);
                            }
                            ui.horizontal_wrapped(|ui| {
                                for session in cards {
                                    match card(ui, session, catalog, now) {
                                        Some(CardClick::Open) => {
                                            *action = Some(match &session.target {
                                                FleetTarget::Pty { tab, pane, .. } => {
                                                    FleetAction::Focus {
                                                        workspace_id: session.workspace_id.clone(),
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
                                        Some(CardClick::ScheduleFollowUp) => {
                                            if let FleetTarget::Pty { session: id, .. } =
                                                &session.target
                                            {
                                                // 이미 예약된 세션이면 그 원문으로
                                                // 열어 고쳐 쓰게 한다.
                                                self.followup = Some(FollowUpState {
                                                    workspace_id: session.workspace_id.clone(),
                                                    session: *id,
                                                    title: session.title.clone(),
                                                    text: session
                                                        .followup
                                                        .clone()
                                                        .unwrap_or_default(),
                                                });
                                            }
                                        }
                                        Some(CardClick::CancelFollowUp) => {
                                            if let FleetTarget::Pty { session: id, .. } =
                                                &session.target
                                            {
                                                // 빈 프롬프트 = 해제(App이 같은 경로로
                                                // 지운다 — 액션을 하나 더 만들지 않는다).
                                                *action = Some(FleetAction::ScheduleFollowUp {
                                                    workspace_id: session.workspace_id.clone(),
                                                    session: *id,
                                                    prompt: String::new(),
                                                });
                                            }
                                        }
                                        None => {}
                                    }
                                }
                            });
                            ui.add_space(10.0);
                        }
                        // 세션 빈 상태 안내는 목록 **끝**에 — 세션을 전부 닫았는데 승인만 남으면
                        // 막힌 묶음과 안내가 함께 보여야 한다(둘 중 하나만 그리면 안 된다,
                        // 2026-08-08 리뷰).
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
                        }
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
        // 다음 단계 예약 창도 동일하게 독립 Window.
        if let Some(scheduled) = self.followup_window(ui.ctx(), catalog, library) {
            out.grid = Some(scheduled);
        }
        out
    }

    /// 다음 단계 예약 창 — 대상 표시 + 프롬프트 입력 + 예약. 닫혀 있으면 아무것도 그리지
    /// 않는다. 브로드캐스트와 달리 **지금 보내지 않으므로** 2단계 확인이 없다: 되돌릴 수
    /// 있는 예약이고(카드에서 해제), 실제 전송 시점엔 턴이 끝나 있다.
    fn followup_window(
        &mut self,
        ctx: &egui::Context,
        catalog: &i18n::Catalog,
        library: &PromptLibrary,
    ) -> Option<FleetAction> {
        self.followup.as_ref()?;
        let mut action = None;
        let mut open = true;
        egui::Window::new(catalog.t("fleet.followup.title", &[]))
            .id(egui::Id::new("fleet_followup"))
            .collapsible(false)
            .resizable(true)
            .default_width(460.0)
            .open(&mut open)
            .show(ctx, |ui| {
                action = self.followup_body(ui, catalog, library);
            });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            open = false;
        }
        if !open || action.is_some() {
            self.followup = None;
        }
        action
    }

    fn followup_body(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        library: &PromptLibrary,
    ) -> Option<FleetAction> {
        let state = self.followup.as_mut()?;
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(catalog.t("fleet.followup.target", &[]))
                    .small()
                    .weak(),
            );
            ui.add(egui::Label::new(egui::RichText::new(&state.title).strong()).truncate());
        });
        ui.weak(catalog.t("fleet.followup.hint", &[]));
        ui.add_space(6.0);
        ui.add(
            egui::TextEdit::multiline(&mut state.text)
                .desired_rows(4)
                .desired_width(f32::INFINITY)
                .hint_text(catalog.t("fleet.followup.placeholder", &[])),
        );
        // 저장된 프롬프트는 **본문에 끼워 넣기만** 한다 — 브로드캐스트처럼 선택 하나로
        // 전송되는 게 아니라 사용자가 이어서 고쳐 쓰는 자리이기 때문이다.
        if !library.prompts.is_empty() {
            ui.add_space(4.0);
            egui::ComboBox::from_id_salt("fleet_followup_prompt")
                .selected_text(catalog.t("fleet.followup.insert", &[]))
                .show_ui(ui, |ui| {
                    for prompt in &library.prompts {
                        if ui.selectable_label(false, &prompt.title).clicked() {
                            if !state.text.is_empty() && !state.text.ends_with('\n') {
                                state.text.push('\n');
                            }
                            state.text.push_str(&prompt.body);
                        }
                    }
                });
        }
        ui.add_space(8.0);
        let prompt = state.text.trim().to_owned();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !prompt.is_empty(),
                    egui::Button::new(catalog.t("fleet.followup.save", &[])),
                )
                .clicked()
            {
                return Some(FleetAction::ScheduleFollowUp {
                    workspace_id: state.workspace_id.clone(),
                    session: state.session,
                    prompt,
                });
            }
            None
        })
        .inner
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
    structured_decision: Option<(String, bool)>,
}

/// 막힌 묶음 맨 위 — 가장 오래 막힌 항목 하나를 그 자리에서 펼쳐 바로 결정한다.
///
/// 승인은 실행할 인자를 그대로 보여 주고(가서 보지 않고 판단), 입력 대기는 기존 대기
/// 카드 위젯에 통째로 위임한다(자유 응답·로그 미리보기를 잃지 않으려고 y/n 버튼을 새로
/// 만들지 않는다). 「건너뛰기」는 없다 — 다음 항목이 바로 아래 카드로 보인다.
fn render_expanded(
    ui: &mut egui::Ui,
    catalog: &i18n::Catalog,
    item: &BlockedItem,
    waiting_cards: &[crate::ui::inbox_waiting::WaitingCard],
    waiting_ui: &mut crate::ui::inbox_waiting::InboxWaitingUi,
    now: i64,
) -> AttentionOutput {
    let mut out = AttentionOutput::default();
    // 승인이면 대기 위젯은 빈 입력으로 호출해 정리만 시킨다. 입력 대기는 이 한 장만
    // 넘기므로 나머지 카드의 입력 버퍼는 정리 대상이 된다(보이지도 않는 카드의 초안이라
    // 잃어도 무해하다).
    let own_card: &[crate::ui::inbox_waiting::WaitingCard] = match &item.kind {
        BlockedKind::NeedsInput { card_index } => &waiting_cards[*card_index..=*card_index],
        _ => &[],
    };

    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let badge = match &item.kind {
                BlockedKind::NeedsInput { .. } => catalog.t("fleet.hero.needs_input", &[]),
                BlockedKind::Approval { .. } | BlockedKind::StructuredApproval { .. } => {
                    catalog.t("fleet.hero.approval", &[])
                }
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
                            &crate::fleet::format_blocked_duration(now, item.blocked_since),
                        )],
                    ))
                    .small()
                    .strong()
                    .color(status_color(AgentVisualState::Error)),
                );
            });
        });
        ui.add(egui::Label::new(egui::RichText::new(&item.title).strong().size(15.0)).truncate());
        ui.add(egui::Label::new(egui::RichText::new(&item.context).small().weak()).truncate());
        ui.add_space(6.0);
        // 승인이면 실행할 인자를 그대로 — 가서 보지 않고 판단하는 게 핵심이다.
        if let BlockedKind::Approval {
            arguments_preview, ..
        } = &item.kind
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
        out.waiting_action = waiting_ui.render(ui, catalog, own_card);
        ui.horizontal(|ui| match &item.kind {
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
            BlockedKind::StructuredApproval { session_id } => {
                if ui
                    .button(catalog.t("inbox.approval.approve", &[]))
                    .clicked()
                {
                    out.structured_decision = Some((session_id.clone(), true));
                }
                if ui.button(catalog.t("inbox.approval.deny", &[])).clicked() {
                    out.structured_decision = Some((session_id.clone(), false));
                }
            }
            // 입력 대기는 기존 카드 위젯이 통째로 그린다(자유 응답·로그 미리보기 포함).
            BlockedKind::NeedsInput { .. } => {}
        });
    });
    out
}

/// 세션 카드가 없는 결정(세션 키를 못 읽은 승인 등) — 제목·맥락·막힌 시간 한 줄.
fn compact_row(ui: &mut egui::Ui, item: &BlockedItem, now: i64) {
    ui.horizontal(|ui| {
        ui.add(egui::Label::new(egui::RichText::new(&item.title).small()).truncate());
        ui.add(egui::Label::new(egui::RichText::new(&item.context).small().weak()).truncate());
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

/// 상단 헤더: 제목 + 총계 + (우측) 배치 스폰·브로드캐스트·새 에이전트 버튼 + 묶음별 칩.
fn header(
    ui: &mut egui::Ui,
    summary: FleetSummary,
    catalog: &i18n::Catalog,
    has_broadcast_target: bool,
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

/// 카드에서 나온 사용자 의도. 좌클릭은 열기, 우클릭 메뉴는 예약/해제다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CardClick {
    Open,
    ScheduleFollowUp,
    CancelFollowUp,
}

/// 세션 카드 하나 — 좌측 상태 바 + 우측 워크스페이스 띠 + 제목/상태/워크스페이스/보조 줄.
fn card(
    ui: &mut egui::Ui,
    session: &FleetSession,
    catalog: &i18n::Catalog,
    now: i64,
) -> Option<CardClick> {
    // 아래쪽 여백이 넓어 카드가 비어 보였다(2026-08-10 사용자 지적). 4행(예약 칩)이
    // 다 찼을 때가 기준이라 그보다 더 줄이면 칩이 잘린다.
    let size = egui::vec2(252.0, 84.0);
    // 자리만 잡는다. **상호작용은 내용을 그린 뒤에** 잡는다 — 여기서 잡으면 나중에
    // 그려진 라벨이 위에 놓여 텍스트 위 클릭을 가로챈다(2026-08-10 실증: 「Kimi」
    // 글자를 눌러도 안 먹혔다).
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    if !ui.is_rect_visible(rect) {
        return None;
    }
    let visuals = ui.visuals();
    // 그리기용 hover는 포인터 위치로 본다 — 상호작용 응답이 아직 없기 때문이다.
    let hovered = ui.rect_contains_pointer(rect);
    let bg = if hovered {
        visuals.widgets.hovered.bg_fill
    } else {
        visuals.faint_bg_color
    };
    // 테두리도 함께 바뀌어야 **어디까지가 이 카드인가**가 분명해진다. 배경만 바꾸면
    // 어두운 테마에서 차이가 거의 안 보여 경계가 흐릿하다(2026-08-10 사용자 지적).
    // 버튼과 같은 팔레트 슬롯을 쓴다 — 카드가 버튼처럼 동작하니 같은 언어여야 한다.
    let stroke = if hovered {
        visuals.widgets.hovered.bg_stroke
    } else {
        visuals.widgets.noninteractive.bg_stroke
    };
    let state_color = status_color(session.state);
    {
        let p = ui.painter();
        p.rect_filled(rect, 6.0, bg);
        p.rect_stroke(rect, 6.0, stroke, egui::StrokeKind::Inside);
        // 좌측 상태 바 하나만 — 정렬을 지배하는 신호라 시선이 먼저 닿는 자리에 둔다.
        // 우측에 워크스페이스 색 띠도 그렸었지만, 카드마다 색이 둘이라 어느 쪽이 상태인지
        // 읽는 데 품이 들었다(2026-08-09). 워크스페이스는 2행에 이름으로 이미 있다.
        let bar = egui::Rect::from_min_size(rect.left_top(), egui::vec2(4.0, rect.height()));
        p.rect_filled(bar, 6.0, state_color);
    }

    // 내용은 child UI(top-down)로 — 라벨 truncate가 카드 폭을 넘지 않게 클립한다.
    let inner = rect.shrink2(egui::vec2(14.0, 8.0));
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
        // 「작업 중」인데 한참 조용한 세션 — 파란 점만으로는 멈춘 걸 알 수 없다.
        if let Some(silent) = crate::fleet::stuck_for(session, now) {
            ui.label(
                egui::RichText::new(catalog.t(
                    "fleet.stuck",
                    &[("value", &crate::fleet::format_blocked_duration(silent, 0))],
                ))
                .small()
                .color(status_color(AgentVisualState::Waiting)),
            );
        }
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
    // 4행: 예약 칩. 「예약해뒀다」는 사실이 카드에 없으면 예약해둔 걸 잊는다 — 그러면
    // 나중에 도착한 프롬프트가 내가 안 시킨 일처럼 보인다.
    if let Some(prompt) = &session.followup {
        content.add(
            egui::Label::new(
                egui::RichText::new(format!(
                    "{} · {prompt}",
                    catalog.t("fleet.followup.chip", &[])
                ))
                .small()
                .color(status_color(AgentVisualState::Complete)),
            )
            .truncate(),
        );
    }

    // 이제 내용 **위에서** 상호작용을 잡는다. 카드 전체가 버튼이므로 커서도 바꾼다.
    let response = ui
        .interact(rect, card_id(ui, session), egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let mut click = response.clicked().then_some(CardClick::Open);
    // 예약은 PTY 전용이다 — 구조화 세션은 steer 경로라 WriteInput 대상이 아니다.
    if matches!(session.target, FleetTarget::Pty { .. }) {
        response.context_menu(|ui| {
            if ui.button(catalog.t("fleet.followup.menu", &[])).clicked() {
                click = Some(CardClick::ScheduleFollowUp);
                ui.close();
            }
            if session.followup.is_some()
                && ui.button(catalog.t("fleet.followup.cancel", &[])).clicked()
            {
                click = Some(CardClick::CancelFollowUp);
                ui.close();
            }
        });
    }
    click
}

/// 카드의 상호작용 id — 세션마다 안정적이어야 한다. `SessionId`는 워크스페이스마다
/// 재사용되므로 워크스페이스까지 포함한다(이 저장소의 다른 키와 같은 관례).
fn card_id(ui: &egui::Ui, session: &FleetSession) -> egui::Id {
    match &session.target {
        FleetTarget::Pty { session: id, .. } => {
            ui.id().with(("fleet_card", &session.workspace_id, id.0))
        }
        FleetTarget::Structured { session_id } => ui.id().with(("fleet_card_app", session_id)),
    }
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
    use crate::ui::inbox_waiting::{InboxWaitingUi, WaitingCard};
    use std::collections::HashMap;

    fn catalog() -> i18n::Catalog {
        i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap()
    }

    fn approval_at(id: &str, tool: &str, created_at: i64) -> PendingApprovalItem {
        PendingApprovalItem::try_new(
            id.to_owned(),
            "srv".to_owned(),
            tool.to_owned(),
            "{}".to_owned(),
            None,
            None,
            created_at,
        )
        .unwrap()
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
            last_output_at: None,
            followup: None,
        }
    }

    /// 페이지를 한 번 그리고 결과를 돌려주는 최소 하네스. 클릭은 하지 않는다.
    fn draw(
        ui_fleet: &mut FleetUi,
        sessions: &[FleetSession],
        pending: &[PendingApprovalItem],
        waiting_cards: &[(WaitingCard, i64)],
        waiting_ui: &mut InboxWaitingUi,
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
                    structured: &[],
                },
            );
        });
        out
    }

    /// 2026-08-08 통합의 핵심 위험: 주의 섹션이 `sessions.is_empty()` 조기반환 **뒤에**
    /// 오면 세션을 전부 닫았는데 승인만 남은 상태에서 승인 카드가 사라진다. 클릭 없이
    /// `is_none()`만 보면 이 회귀가 재발해도 통과하므로(2026-08-08 리뷰) 카드가 실제로
    /// 그려졌는지 라벨로 확인한다.
    #[test]
    fn 세션이_없어도_승인_카드는_그린다() {
        use egui_kittest::kittest::Queryable;

        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1100.0, 600.0))
            .build_ui(|ui| {
                let catalog = catalog();
                let library = crate::prompt_library::PromptLibrary::default();
                let mut fleet = FleetUi::default();
                let mut waiting = InboxWaitingUi::new();
                let pending = [approval("a1", Some("ws-1:7"))];
                let _ = fleet.render(
                    ui,
                    &[],
                    FleetSummary::default(),
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
                        structured: &[],
                    },
                );
            });
        harness.run();
        let catalog = catalog();
        assert!(
            harness.query_by_label("read_file").is_some(),
            "세션이 0인데 승인 카드가 사라졌다 — 주의 섹션이 조기반환 뒤로 밀렸다"
        );
        // 세션 빈 상태 안내도 함께 보여야 한다(둘 중 하나만 그리면 안 된다).
        assert!(
            harness
                .query_by_label(&catalog.t("fleet.empty", &[]))
                .is_some(),
            "세션 빈 상태 안내가 사라졌다"
        );
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
        );
        assert!(first.waiting_action.is_none());
        let second = draw(&mut fleet, &[], &[], &[], &mut waiting);
        assert!(second.waiting_action.is_none());
    }

    /// 막힌 것이 있으면 「막힌 것」 묶음 맨 위에 그 항목이 펼쳐지고, 없으면 묶음 자체가
    /// 없다 — 이 화면은 비어 있는 게 정상이라 빈 안내 카드로 자리를 차지하지 않는다.
    #[test]
    fn 막힌_것이_있을_때만_펼친_항목이_보인다() {
        use egui_kittest::kittest::Queryable;

        let cat = catalog();
        let blocked_label = cat.t("fleet.group.blocked", &[]);
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
                            structured: &[],
                        },
                    );
                });
            harness.run();
            // 승인이 있으면 그 도구 이름이 펼친 항목 제목으로 보이고, 세션은 진행 중
            // 하나뿐이라 「막힌 것」 묶음은 오직 큐 때문에 생긴다.
            assert_eq!(
                harness.query_by_label("read_file").is_some(),
                has_item,
                "승인 {}건일 때 펼친 항목 표시가 기대와 다르다",
                pending.len()
            );
            assert_eq!(
                harness.query_by_label(&blocked_label).is_some(),
                has_item,
                "승인 {}건일 때 「{blocked_label}」 묶음 표시가 기대와 다르다",
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
            &[],
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
                        structured: &[],
                    },
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

    /// 가장 오래 막힌 항목 **하나만** 펼쳐진다 — 승인 버튼은 그 카드에만 있고, 뒤 항목은
    /// 요약 줄이라 버튼이 없다. 「건너뛰기」는 없앴다: 한 목록에서 다음 항목이 바로 아래
    /// 보이니 넘길 이유가 없다(2026-09-05).
    #[test]
    fn 가장_오래_막힌_항목만_펼쳐지고_건너뛰기는_없다() {
        use egui_kittest::kittest::Queryable;

        let outer = catalog();
        let approve = outer.t("inbox.approval.approve", &[]);
        // 삭제된 버튼의 예전 문구를 고정해 번역 키 없이도 재등장을 검사한다.
        let skip_label = "Skip";
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1100.0, 600.0))
            .build_ui(|ui| {
                let catalog = catalog();
                let library = crate::prompt_library::PromptLibrary::default();
                let mut fleet = FleetUi::default();
                let mut waiting = InboxWaitingUi::new();
                // 오래 막힌 순: older(100) → newer(200).
                let pending = [
                    approval_at("a1", "older_tool", 100),
                    approval_at("a2", "newer_tool", 200),
                ];
                let _ = fleet.render(
                    ui,
                    &[],
                    FleetSummary::default(),
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
                        structured: &[],
                    },
                );
            });
        harness.run();
        assert_eq!(
            harness.query_all_by_label(&approve).count(),
            1,
            "승인 버튼은 펼친 항목 하나에만 있어야 한다"
        );
        let older = harness.get_by_label("older_tool").rect();
        let newer = harness.get_by_label("newer_tool").rect();
        assert!(
            older.top() < newer.top(),
            "오래 막힌 쪽이 위(펼침)여야 한다"
        );
        assert!(
            harness.query_by_label(skip_label).is_none(),
            "「{skip_label}」 버튼은 없어야 한다"
        );
    }

    /// 좁은 창에서도 펼친 카드가 화면 안에 있어야 한다. 창 최소 크기 제한이 없고
    /// 사이드바가 680px까지 넓어져 available이 아주 작아질 수 있다(2026-08-08 리뷰).
    #[test]
    fn 좁은_폭에서도_펼친_카드가_화면_안에_있다() {
        use egui_kittest::kittest::Queryable;

        const NARROW: f32 = 300.0;
        let outer = catalog();
        let approve = outer.t("inbox.approval.approve", &[]);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(NARROW, 500.0))
            .build_ui(|ui| {
                let catalog = catalog();
                let library = crate::prompt_library::PromptLibrary::default();
                let mut fleet = FleetUi::default();
                let mut waiting = InboxWaitingUi::new();
                let pending = [approval_at("a1", "older_tool", 100)];
                let _ = fleet.render(
                    ui,
                    &[],
                    FleetSummary::default(),
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
                        structured: &[],
                    },
                );
            });
        harness.run();
        for label in ["older_tool", approve.as_str()] {
            let rect = harness.get_by_label(label).rect();
            assert!(
                rect.right() <= NARROW,
                "「{label}」이 화면 밖({})까지 나간다 — 펼친 카드가 available을 무시했다",
                rect.right()
            );
        }
    }

    fn structured(id: &str, title: &str, since: i64) -> StructuredApproval {
        StructuredApproval {
            session_id: id.to_owned(),
            title: title.to_owned(),
            workspace_name: Some("ws-1".to_owned()),
            blocked_since: since,
        }
    }

    /// 구조화(App Server) 승인도 MCP 승인·입력 대기와 **같은 큐**에 서고 같은 기준으로
    /// 정렬된다 — 사용자에겐 셋 다 "에이전트가 나를 기다린다"는 같은 일이다.
    #[test]
    fn 구조화_승인도_같은_큐에_오래_막힌_순으로_선다() {
        let queue = blocked_queue(
            &[approval_at("a1", "mcp_tool", 300)],
            &[structured("s1", "구조화 승인", 100)],
            &[(waiting_card("ws-1", 7), 200)],
            &HashMap::new(),
            &HashMap::new(),
            "unknown",
        );
        assert_eq!(queue.len(), 3);
        let since: Vec<i64> = queue.iter().map(|item| item.blocked_since).collect();
        assert_eq!(since, vec![100, 200, 300], "오래 막힌 순이어야 한다");
        assert!(
            matches!(queue[0].kind, BlockedKind::StructuredApproval { .. }),
            "가장 오래 막힌 구조화 승인이 맨 앞이어야 한다"
        );
    }

    /// 「막힌 것」 묶음은 큐와 세션 카드를 **한 번씩만** 잇는다 — 맨 앞 항목은 펼치고 그
    /// 세션 카드는 빼며, 뒤 항목은 세션 카드로 대신하고, 카드가 없는 결정만 요약 줄로
    /// 남긴다. 큐에 없는 막힌 세션은 뒤에 카드로 붙는다.
    #[test]
    fn 막힌_묶음은_같은_막힘을_두_번_그리지_않는다() {
        let sessions = [
            pty_session("ws-1", 7, AgentVisualState::Waiting),
            pty_session("ws-1", 9, AgentVisualState::Waiting),
            pty_session("ws-2", 3, AgentVisualState::Waiting),
            pty_session("ws-3", 5, AgentVisualState::Waiting),
        ];
        let queue = blocked_queue(
            &[
                // created_at=0 → 가장 오래 막힘 → 펼침. 세션 키로 ws-2:3 카드를 대신한다.
                approval("a0", Some("ws-2:3")),
                // 세션 키 없음 → 카드가 없어 요약 줄.
                approval_at("a1", "orphan_tool", 300),
            ],
            &[],
            &[
                (waiting_card("ws-1", 7), 100),
                (waiting_card("ws-1", 9), 200),
            ],
            &HashMap::new(),
            &HashMap::new(),
            "unknown",
        );
        let rows = blocked_rows(&queue, &sessions);
        assert_eq!(
            rows,
            vec![
                BlockedRow::Expanded(0),
                BlockedRow::Card(0),
                BlockedRow::Card(1),
                BlockedRow::Compact(3),
                BlockedRow::Card(3),
            ],
            "큐 순서대로 잇고, 펼친 항목의 세션(ws-2:3)은 카드로 다시 나오면 안 된다"
        );
    }

    /// 히어로에서 구조화 승인을 누르면 **id를 실은 결정**이 나온다 — Agents 패널의
    /// 선택 상태와 무관해야 한다(선택 기반 경로만 있던 것을 id 경로로 뺀 이유).
    #[test]
    fn 히어로의_구조화_승인_클릭이_세션_id를_실어_보낸다() {
        use egui_kittest::kittest::Queryable;

        struct State {
            fleet: FleetUi,
            waiting: InboxWaitingUi,
            out: Option<(String, bool)>,
        }
        let outer = catalog();
        let approve = outer.t("inbox.approval.approve", &[]);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1100.0, 600.0))
            .build_ui_state(
                |ui, state: &mut State| {
                    let catalog = catalog();
                    let library = crate::prompt_library::PromptLibrary::default();
                    let page = state.fleet.render(
                        ui,
                        &[],
                        FleetSummary::default(),
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
                            waiting_ui: &mut state.waiting,
                            structured: &[structured("s-42", "위험한 작업", 100)],
                        },
                    );
                    if page.structured_decision.is_some() {
                        state.out = page.structured_decision;
                    }
                },
                State {
                    fleet: FleetUi::default(),
                    waiting: InboxWaitingUi::new(),
                    out: None,
                },
            );
        harness.run();
        assert!(
            harness.query_by_label("위험한 작업").is_some(),
            "구조화 승인이 히어로에 보여야 한다"
        );
        harness.get_by_label(&approve).click();
        harness.run();
        assert_eq!(
            harness.state().out,
            Some(("s-42".to_owned(), true)),
            "승인 클릭이 그 세션 id를 허용으로 실어 보내야 한다"
        );
    }
    /// 예약해뒀다는 사실이 카드에 안 보이면, 나중에 도착한 프롬프트가 내가 안 시킨
    /// 일처럼 보인다. 칩과 **원문**이 함께 보여야 뭘 예약했는지 기억이 난다.
    #[test]
    fn 예약된_세션_카드는_칩과_원문을_보여준다() {
        use egui_kittest::kittest::Queryable;

        let outer = catalog();
        let chip = outer.t("fleet.followup.chip", &[]);
        let mut session = pty_session("ws-1", 7, AgentVisualState::Active);
        session.followup = Some("테스트 돌리고 실패한 것만 고쳐".to_owned());
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1100.0, 600.0))
            .build_ui(|ui| {
                let catalog = catalog();
                let library = crate::prompt_library::PromptLibrary::default();
                let mut fleet = FleetUi::default();
                let mut waiting = InboxWaitingUi::new();
                let sessions = [session.clone()];
                let _ = fleet.render(
                    ui,
                    &sessions,
                    FleetSummary::from_states(sessions.iter().map(|s| s.state)),
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
                        structured: &[],
                    },
                );
            });
        harness.run();
        assert!(
            harness
                .query_by_label(&format!("{chip} · 테스트 돌리고 실패한 것만 고쳐"))
                .is_some(),
            "예약 칩과 원문이 카드에 보여야 한다"
        );
    }

    /// 예약 버튼은 **대상 세션과 원문을 그대로** 실어 보내야 한다. 대상이 어긋나면
    /// 엉뚱한 에이전트가 남의 다음 단계를 받는다.
    #[test]
    fn 예약_버튼은_대상_세션과_원문을_실어_보낸다() {
        use egui_kittest::kittest::Queryable;

        struct State {
            fleet: FleetUi,
            waiting: InboxWaitingUi,
            out: Option<(String, runtime::SessionId, String)>,
        }
        let outer = catalog();
        let save = outer.t("fleet.followup.save", &[]);
        // 패널은 카드 우클릭으로 열린다. 여는 경로가 아니라 **보내는 계약**을 보는
        // 테스트라 열린 상태에서 시작한다.
        let fleet = FleetUi {
            followup: Some(FollowUpState {
                workspace_id: "ws-1".to_owned(),
                session: runtime::SessionId(7),
                title: "session-7".to_owned(),
                text: "테스트 돌리고 실패한 것만 고쳐".to_owned(),
            }),
            ..Default::default()
        };
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1100.0, 600.0))
            .build_ui_state(
                |ui, state: &mut State| {
                    let catalog = catalog();
                    let library = crate::prompt_library::PromptLibrary::default();
                    let page = state.fleet.render(
                        ui,
                        &[],
                        FleetSummary::default(),
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
                            waiting_ui: &mut state.waiting,
                            structured: &[],
                        },
                    );
                    if let Some(FleetAction::ScheduleFollowUp {
                        workspace_id,
                        session,
                        prompt,
                    }) = page.grid
                    {
                        state.out = Some((workspace_id, session, prompt));
                    }
                },
                State {
                    fleet,
                    waiting: InboxWaitingUi::new(),
                    out: None,
                },
            );
        harness.run();
        harness.get_by_label(&save).click();
        harness.run();
        assert_eq!(
            harness.state().out,
            Some((
                "ws-1".to_owned(),
                runtime::SessionId(7),
                "테스트 돌리고 실패한 것만 고쳐".to_owned()
            )),
            "예약이 대상 세션과 원문을 그대로 실어 보내야 한다"
        );
    }
}
