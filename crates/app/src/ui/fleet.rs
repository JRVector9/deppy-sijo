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

use std::collections::{BTreeMap, HashMap, HashSet};
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
        targets: Vec<crate::fleet::FleetPromptTarget>,
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
        target: Option<crate::fleet::FleetPromptTarget>,
        workspace_id: String,
        session: runtime::SessionId,
        prompt: String,
        effort: Option<crate::followup_settings::EffortRequest>,
    },
}

/// 브로드캐스트 패널 상태 — 프롬프트 선택 + 파라미터 + 대상 체크.
#[derive(Default)]
struct BroadcastState {
    prompt_id: Option<String>,
    params: BTreeMap<String, String>,
    preview: FleetPromptPreview,
    /// 체크된 대상 (workspace_id, session).
    targets: HashSet<crate::fleet::FleetPromptTarget>,
    /// 대상 3개 이상 전송의 2단계 확인 단계(리뷰 Low). 프롬프트·대상이 바뀌면 초기화한다.
    confirm_send: bool,
}

/// 다음 단계 예약 패널 상태. 대상 세션은 패널을 여는 순간 고정된다 — 브로드캐스트와
/// 달리 대상이 하나라 고르는 단계가 없다.
struct FollowUpState {
    target: Option<crate::fleet::FleetPromptTarget>,
    workspace_id: String,
    session: runtime::SessionId,
    /// 카드 제목 — 어느 세션에 예약하는지 패널에서 다시 보여준다.
    title: String,
    /// 편집 중인 원문. 이미 예약된 세션이면 그 값으로 시작해 고쳐 쓸 수 있다.
    text: String,
    effort: Option<crate::agent_launcher::ReasoningEffort>,
    context: crate::followup_settings::EffortContext,
}

fn append_followup_template(text: &mut String, body: &str) -> bool {
    let separator = usize::from(!text.is_empty() && !text.ends_with('\n'));
    if separator.saturating_add(body.len())
        > crate::fleet::FLEET_PROMPT_MAX_BYTES.saturating_sub(text.len())
    {
        return false;
    }
    if separator == 1 {
        text.push('\n');
    }
    text.push_str(body);
    true
}

/// 배치 스폰 패널 상태 — 에이전트 선택 + 개수 + (선택) 프롬프트. 패널을 열 때마다
/// 초기화한다. `prompt_id`가 None이면 빈 세션(PR-S1과 동일 — "없음" 선택).
#[derive(Default)]
struct BatchSpawnState {
    agent_id: Option<String>,
    count: u32,
    prompt_id: Option<String>,
    params: BTreeMap<String, String>,
    preview: FleetPromptPreview,
}

/// One bounded expansion per open form, invalidated by content revision or accepted edits.
#[derive(Default)]
struct FleetPromptPreview {
    key: Option<(u64, String)>,
    names: Vec<String>,
    rendered: Option<String>,
    parse_error: bool,
    input_error: bool,
    dirty: bool,
}

fn fleet_prompt_input<'a>(
    ui: &mut egui::Ui,
    prompt: &crate::prompt_library::Prompt,
    params: &mut BTreeMap<String, String>,
    preview: &'a mut FleetPromptPreview,
    revision: u64,
    limits: (&str, usize),
    catalog: &i18n::Catalog,
) -> (Option<&'a str>, bool) {
    use crate::prompt_library::{
        PROMPT_PARAM_MAX_NAMES, PROMPT_PARAM_VALUE_MAX_BYTES, PROMPT_PARAM_VALUES_MAX_BYTES,
        param_names_bounded, render_bounded,
    };
    let (salt, max_output_bytes) = limits;
    let new_content = !preview
        .key
        .as_ref()
        .is_some_and(|(rev, id)| *rev == revision && id == &prompt.id);
    if new_content {
        preview.key = Some((revision, prompt.id.clone()));
        preview.rendered = None;
        preview.input_error = false;
        preview.dirty = true;
        match param_names_bounded(&prompt.body) {
            Ok(names) => {
                params.retain(|name, _| names.contains(name));
                preview.names = names;
                preview.parse_error = false;
            }
            Err(_) => {
                preview.names.clear();
                params.clear();
                preview.parse_error = true;
            }
        }
        for index in 0..PROMPT_PARAM_MAX_NAMES {
            let id = egui::Id::new((salt, index));
            super::text_input::forget_bounded_text_state(ui.ctx(), id);
            ui.memory_mut(|memory| memory.surrender_focus(id));
        }
    }
    let mut changed = new_content;
    if !preview.parse_error {
        let mut total: usize = params.values().map(String::len).sum();
        egui::ScrollArea::vertical()
            .id_salt((salt, "fields"))
            .max_height(180.0)
            .show(ui, |ui| {
                egui::Grid::new((salt, "grid"))
                    .num_columns(2)
                    .show(ui, |ui| {
                        for (index, name) in preview.names.iter().enumerate() {
                            ui.monospace(format!("{{{{{name}}}}}"));
                            let value = params.entry(name.clone()).or_default();
                            let previous = value.len();
                            let budget = PROMPT_PARAM_VALUE_MAX_BYTES.min(
                                PROMPT_PARAM_VALUES_MAX_BYTES
                                    .saturating_sub(total.saturating_sub(previous)),
                            );
                            let (response, rejected) = super::text_input::bounded_edit(
                                ui,
                                value,
                                budget,
                                egui::Id::new((salt, index)),
                                "",
                                false,
                            );
                            total = total.saturating_sub(previous).saturating_add(value.len());
                            if response.changed() {
                                preview.dirty = true;
                                preview.input_error = rejected;
                                changed = true;
                            }
                            ui.end_row();
                        }
                    });
            });
        if preview.dirty {
            preview.dirty = false;
            preview.rendered = render_bounded(&prompt.body, params).ok();
        }
    }
    let output_error = preview
        .rendered
        .as_ref()
        .is_some_and(|text| text.len() > max_output_bytes);
    if preview.parse_error || preview.input_error || preview.rendered.is_none() || output_error {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            catalog.t("prompt.input_limit", &[]),
        );
    }
    if let Some(rendered) = &preview.rendered {
        const MAX_PREVIEW: usize = 16 * 1024;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(
                        &rendered[..rendered.floor_char_boundary(MAX_PREVIEW.min(rendered.len()))],
                    )
                    .monospace(),
                )
                .wrap(),
            );
            if rendered.len() > MAX_PREVIEW {
                ui.weak(catalog.t("prompt.preview_truncated", &[]));
            }
        });
    }
    let filled = preview.names.iter().all(|name| {
        params
            .get(name)
            .is_some_and(|value| !value.trim().is_empty())
    });
    let ready = (!preview.parse_error && !preview.input_error && !output_error && filled)
        .then_some(preview.rendered.as_deref())
        .flatten();
    (ready, changed)
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

fn session_matches_ref(session: &FleetSession, wanted: &BlockedRef) -> bool {
    match (&session.target, wanted) {
        (
            FleetTarget::Pty { session: id, .. },
            BlockedRef::Pty {
                workspace_id,
                session: wanted_id,
            },
        ) => session.workspace_id == *workspace_id && id == wanted_id,
        (
            FleetTarget::Structured { session_id },
            BlockedRef::Structured {
                session_id: wanted_id,
            },
        ) => session_id == wanted_id,
        _ => false,
    }
}

fn queue_links_session(queue: &[BlockedItem], session: &FleetSession) -> bool {
    queue
        .iter()
        .filter_map(|item| item.session.as_ref())
        .any(|wanted| session_matches_ref(session, wanted))
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
pub fn blocked_rows<T: std::borrow::Borrow<FleetSession>>(
    queue: &[BlockedItem],
    sessions: &[T],
) -> Vec<BlockedRow> {
    let mut rows = Vec::with_capacity(queue.len() + sessions.len());
    let mut used = vec![false; sessions.len()];
    for (index, item) in queue.iter().enumerate() {
        let matched = item
            .session
            .as_ref()
            .and_then(|wanted| {
                sessions
                    .iter()
                    .position(|session| session_matches_ref(session.borrow(), wanted))
            })
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

/// Actual completion clocks projected by the attention worker, including consumed badges.
pub type PtyIdleClocks = HashMap<(String, runtime::SessionId), (i64, i64)>;

#[derive(Default)]
pub struct FleetUi {
    /// App supplies the actual accepted library content revision before rendering Fleet.
    prompt_revision: u64,
    /// Some이면 브로드캐스트 패널이 열려 있다.
    broadcast: Option<BroadcastState>,
    /// Some이면 배치 스폰 패널이 열려 있다.
    batch_spawn: Option<BatchSpawnState>,
    /// Some이면 다음 단계 예약 패널이 열려 있다.
    followup: Option<FollowUpState>,
    followup_reset_pending: bool,
    followup_input_error: bool,
    followup_admission_error: bool,
    followup_pending: bool,
    retained_followups: usize,
    blocked_followups: Vec<(String, runtime::SessionId)>,
    /// hook 완료 시각이 없는 세션도 첫 지시 대기 관측부터 시간을 센다.
    idle_started: HashMap<FleetIdleKey, IdleClock>,
}

#[derive(Clone, Copy)]
struct IdleTime {
    since: i64,
    confirmed: bool,
}

#[derive(Default)]
struct IdleClock {
    since: Option<i64>,
    confirmed: bool,
    generation: Option<i64>,
    last_submission_at_micros: Option<i64>,
}

/// Both modern and legacy idle generations use microseconds in the snapshot.
/// An ambiguous same-boundary completion stays observed, not falsely confirmed.
pub fn completion_follows_submission(generation: Option<i64>, submitted_at: Option<i64>) -> bool {
    submitted_at.is_none_or(|submitted| generation.is_some_and(|completed| completed > submitted))
}

impl IdleClock {
    fn confirm_source(&mut self, since: i64, generation: Option<i64>) {
        if self.confirmed {
            if let (Some(current), Some(incoming)) = (self.generation, generation)
                && incoming < current
            {
                return;
            }
            if self.since == Some(since) && self.generation == generation {
                return;
            }
        }
        self.since = Some(since);
        self.generation = generation;
        self.confirmed = true;
    }
}

#[derive(Clone, Hash, PartialEq, Eq)]
enum FleetIdleKey {
    Pty(String, runtime::SessionId, runtime::MuxPaneId),
    Structured(String),
}

impl FleetIdleKey {
    fn for_session(session: &FleetSession) -> Self {
        match &session.target {
            FleetTarget::Pty {
                session: id, pane, ..
            } => Self::Pty(session.workspace_id.clone(), *id, pane.clone()),
            FleetTarget::Structured { session_id } => Self::Structured(session_id.clone()),
        }
    }
}

impl FleetUi {
    pub fn set_followup_summary(
        &mut self,
        total: usize,
        mut blocked: Vec<(String, runtime::SessionId)>,
    ) {
        blocked.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.0.cmp(&right.1.0)));
        self.retained_followups = total;
        self.blocked_followups = blocked;
    }

    /// A host refusal keeps the exact original form and draft available to correct or cancel.
    pub fn settle_followup(&mut self, target: &crate::fleet::FleetPromptTarget, accepted: bool) {
        if !self.followup.as_ref().is_some_and(|state| {
            state.target.as_ref() == Some(target)
                && state.workspace_id == target.workspace_id
                && state.session == target.session
        }) {
            return;
        }
        self.followup_pending = false;
        if accepted {
            self.followup = None;
        } else {
            self.followup_admission_error = true;
        }
    }

    fn blocked_followup_controls(
        &self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
    ) -> Option<FleetAction> {
        if self.retained_followups == 0 {
            return None;
        }
        ui.weak(catalog.t(
            "fleet.followup.retained",
            &[("count", &self.retained_followups.to_string())],
        ));
        if self.blocked_followups.is_empty() {
            return None;
        }
        ui.colored_label(
            ui.visuals().warn_fg_color,
            catalog.t("fleet.followup.blocked", &[]),
        );
        let mut action = None;
        egui::ScrollArea::vertical()
            .id_salt("fleet_blocked_followups")
            .max_height(120.0)
            .show_rows(
                ui,
                ui.text_style_height(&egui::TextStyle::Body) + 8.0,
                self.blocked_followups.len(),
                |ui, range| {
                    for index in range {
                        let (workspace, session) = &self.blocked_followups[index];
                        ui.push_id(("blocked_followup", workspace, session.0), |ui| {
                            ui.horizontal(|ui| {
                                ui.label(format!("{} / {}", workspace, session.0));
                                if ui.button(catalog.t("fleet.followup.cancel", &[])).clicked() {
                                    action = Some(FleetAction::ScheduleFollowUp {
                                        target: None,
                                        workspace_id: workspace.clone(),
                                        session: *session,
                                        prompt: String::new(),
                                        effort: None,
                                    });
                                }
                            })
                        });
                    }
                },
            );
        action
    }

    pub(crate) fn followup_target(&self) -> Option<&crate::fleet::FleetPromptTarget> {
        self.followup.as_ref()?.target.as_ref()
    }

    pub(crate) fn set_followup_context(
        &mut self,
        context: crate::followup_settings::EffortContext,
        reserved: Option<crate::agent_launcher::ReasoningEffort>,
    ) {
        if let Some(state) = self.followup.as_mut() {
            if state.context.provider.is_none() {
                state.effort = reserved;
            }
            state.context = context;
        }
    }

    pub fn set_prompt_revision(&mut self, revision: u64) {
        self.prompt_revision = revision;
    }

    #[cfg(test)]
    fn idle_since(&self, session: &FleetSession) -> Option<i64> {
        self.idle_started
            .get(&FleetIdleKey::for_session(session))
            .and_then(|clock| clock.since)
    }

    fn note_observed_turn_start(&mut self, workspace: &str, session: runtime::SessionId) {
        for (key, clock) in &mut self.idle_started {
            if matches!(key, FleetIdleKey::Pty(ws, id, _) if ws == workspace && *id == session)
                && !clock.confirmed
            {
                clock.since = None;
            }
        }
    }

    pub fn observe_attention(
        &mut self,
        clocks: &PtyIdleClocks,
        working: &HashSet<(String, runtime::SessionId)>,
        blocked: &HashSet<(String, runtime::SessionId)>,
        now: i64,
    ) {
        for (key, clock) in &mut self.idle_started {
            let FleetIdleKey::Pty(ws, session, _) = key else {
                continue;
            };
            let identity = (ws.clone(), *session);
            if let Some((since, generation)) =
                clocks.get(&identity).filter(|(since, generation)| {
                    *since >= 0
                        && *since <= now
                        && *generation > 0
                        && completion_follows_submission(
                            Some(*generation),
                            clock.last_submission_at_micros,
                        )
                })
            {
                clock.confirm_source(*since, Some(*generation));
            } else if clock.confirmed || working.contains(&identity) || blocked.contains(&identity)
            {
                clock.since = None;
                clock.generation = None;
                clock.confirmed = false;
            }
        }
    }

    pub fn observe_structured_status(&mut self, id: &str, state: AgentVisualState, now: i64) {
        let key = FleetIdleKey::Structured(id.to_owned());
        if state == AgentVisualState::Off {
            self.idle_started.remove(&key);
        } else if let Some(clock) = self.idle_started.get_mut(&key) {
            if state == AgentVisualState::Idle {
                clock.since.get_or_insert(now);
            } else {
                clock.since = None;
                clock.confirmed = false;
                clock.generation = None;
            }
        }
    }

    /// Receives the existing event stream even when Fleet is hidden. No I/O or repaint.
    pub fn observe_runtime_events(
        &mut self,
        workspace: &str,
        events: &[runtime::RuntimeEvent],
        _now: i64,
    ) {
        for event in events {
            match event {
                runtime::RuntimeEvent::SessionStatusChanged {
                    session,
                    status:
                        runtime::SessionStatus::Running
                        | runtime::SessionStatus::Waiting
                        | runtime::SessionStatus::NeedsApproval,
                } => {
                    self.note_observed_turn_start(workspace, *session);
                }
                runtime::RuntimeEvent::SessionInputSubmitted { session, at_micros }
                    if *at_micros > 0 =>
                {
                    for (key, clock) in &mut self.idle_started {
                        if matches!(key, FleetIdleKey::Pty(ws,id,_) if ws==workspace && id==session)
                        {
                            clock.last_submission_at_micros = Some(
                                clock
                                    .last_submission_at_micros
                                    .map_or(*at_micros, |previous| previous.max(*at_micros)),
                            );
                            if !completion_follows_submission(
                                clock.generation,
                                clock.last_submission_at_micros,
                            ) {
                                clock.since = None;
                                clock.generation = None;
                                clock.confirmed = false;
                            }
                        }
                    }
                }
                runtime::RuntimeEvent::SessionExited { session, .. }
                | runtime::RuntimeEvent::SessionRestored { session, .. } => {
                    self.idle_started.retain(|key, _| !matches!(key, FleetIdleKey::Pty(ws, id, _) if ws == workspace && id == session));
                }
                _ => {}
            }
        }
    }

    fn update_idle_clocks(&mut self, sessions: &[FleetSession], now: i64) {
        let live: HashSet<_> = sessions.iter().map(FleetIdleKey::for_session).collect();
        self.idle_started.retain(|key, _| live.contains(key));
        for session in sessions {
            let clock = self
                .idle_started
                .entry(FleetIdleKey::for_session(session))
                .or_default();
            // The persisted reducer is authoritative. A detector event has no episode/time
            // identity and may be delivered after this completion; it only resets estimates.
            let source = session.idle_since.filter(|at| {
                *at >= 0
                    && *at <= now
                    && completion_follows_submission(
                        session.idle_generation,
                        clock.last_submission_at_micros,
                    )
            });
            if let Some(since) = source {
                clock.confirm_source(since, session.idle_generation);
            } else if session.state != AgentVisualState::Idle {
                clock.since = None;
                clock.generation = None;
                clock.confirmed = false;
            } else {
                if clock.confirmed {
                    clock.since = None;
                    clock.generation = None;
                    clock.confirmed = false;
                }
                clock.since.get_or_insert(now);
            }
        }
    }

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
        self.update_idle_clocks(sessions, now);
        if sessions.iter().any(|session| {
            session.state == AgentVisualState::Idle || session.blocked_since.is_some()
        }) {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs(1));
        }
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
                            preview: FleetPromptPreview::default(),
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
                            preview: FleetPromptPreview::default(),
                        });
                    }
                    None => {}
                }
                ui.add_space(12.0);
                if let Some(cancel) = self.blocked_followup_controls(ui, catalog) {
                    *action = Some(cancel);
                }
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
                let mut grouped = crate::fleet::group_session_refs(sessions);
                // 승인과 상태 스냅샷의 도착 순서가 달라도 큐에 연결된 세션은 모두 막힌
                // 묶음으로 모은다. 그래야 뒤 큐 항목도 Compact 행과 다른 묶음의 카드로
                // 갈라지지 않고, `blocked_rows`가 큐 순서대로 카드 하나에 대응시킨다.
                for slot in 1..grouped.len() {
                    let (mut linked, retained): (Vec<_>, Vec<_>) =
                        std::mem::take(&mut grouped[slot])
                            .into_iter()
                            .partition(|session| queue_links_session(&queue, session));
                    grouped[0].append(&mut linked);
                    grouped[slot] = retained;
                }
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
                                        BlockedRow::Card(slot) => cards.push(members[slot]),
                                    }
                                }
                            } else {
                                cards.extend(members.iter().copied());
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
                                    let idle_time = self
                                        .idle_started
                                        .get(&FleetIdleKey::for_session(session))
                                        .and_then(|clock| {
                                            clock.since.map(|since| IdleTime {
                                                since,
                                                confirmed: clock.confirmed,
                                            })
                                        });
                                    match card(ui, session, catalog, now, idle_time) {
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
                                                    target: session.broadcast_key(),
                                                    workspace_id: session.workspace_id.clone(),
                                                    session: *id,
                                                    title: session.title.clone(),
                                                    text: session
                                                        .followup
                                                        .as_deref()
                                                        .unwrap_or_default()
                                                        .to_owned(),
                                                    effort: None,
                                                    context: crate::followup_settings::EffortContext::default(),
                                                });
                                                self.followup_reset_pending = true;
                                                self.followup_input_error = false;
                                                self.followup_admission_error = false;
                                                self.followup_pending = false;
                                            }
                                        }
                                        Some(CardClick::CancelFollowUp) => {
                                            if let FleetTarget::Pty { session: id, .. } =
                                                &session.target
                                            {
                                                // 빈 프롬프트 = 해제(App이 같은 경로로
                                                // 지운다 — 액션을 하나 더 만들지 않는다).
                                                *action = Some(FleetAction::ScheduleFollowUp {
                                                    target: None,
                                                    workspace_id: session.workspace_id.clone(),
                                                    session: *id,
                                                    prompt: String::new(),
                                        effort: None,
                                                });
                                            }
                                        }
                                        None => {}
                                    }
                                }
                            });
                            ui.add_space(6.0);
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
        super::popup::window(
            ctx,
            super::popup::WindowSpec {
                id: egui::Id::new("fleet_followup"),
                title: &catalog.t("fleet.followup.title", &[]),
                subtitle: "",
                close_label: &catalog.t("popup.dismiss", &[]),
                close_enabled: !self.followup_pending,
                default_size: egui::vec2(620.0, 560.0),
                min_size: egui::vec2(360.0, 300.0),
            },
            &mut open,
            |ui| {
                action = self.followup_body(ui, catalog, library);
            },
        );
        if !self.followup_pending
            && super::popup::take_window_escape(ctx, egui::Id::new("fleet_followup"))
        {
            open = false;
        }
        if !open {
            self.followup = None;
            self.followup_pending = false;
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
        super::popup::window_body(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(catalog.t("fleet.followup.target", &[]))
                        .small()
                        .weak(),
                );
                ui.add(egui::Label::new(egui::RichText::new(&state.title).strong()).truncate());
            });
            if !state.context.model.is_empty() {
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&state.context.model).size(12.0).weak(),
                            )
                            .truncate(),
                        );
                    });
                });
            }
            let selected = state
                .effort
                .map(|level| level.value().to_owned())
                .unwrap_or_else(|| {
                    let current = state.context.current.as_deref().unwrap_or("—");
                    catalog.t("fleet.followup.effort_keep", &[("effort", current)])
                });
            super::popup::field(
                ui,
                &catalog.t("fleet.followup.effort", &[]),
                Some(&catalog.t("fleet.followup.effort_hint", &[])),
                |ui| {
                    ui.add_enabled_ui(
                        !state.context.levels.is_empty() && !self.followup_pending,
                        |ui| {
                            super::popup::choice_input(
                                ui,
                                "fleet_followup_effort",
                                &selected,
                                |ui| {
                                    ui.selectable_value(
                                        &mut state.effort,
                                        None,
                                        catalog.t(
                                            "fleet.followup.effort_keep",
                                            &[(
                                                "effort",
                                                state.context.current.as_deref().unwrap_or("—"),
                                            )],
                                        ),
                                    );
                                    for &level in &state.context.levels {
                                        ui.selectable_value(
                                            &mut state.effort,
                                            Some(level),
                                            level.value(),
                                        );
                                    }
                                },
                            );
                        },
                    );
                    if state.context.levels.is_empty() {
                        ui.weak(catalog.t("fleet.followup.effort_unavailable", &[]));
                    }
                },
            );
            ui.add_space(6.0);
            let id = egui::Id::new("fleet_followup_text");
            if self.followup_reset_pending {
                super::text_input::forget_bounded_text_state(ui.ctx(), id);
                ui.memory_mut(|memory| memory.surrender_focus(id));
                self.followup_reset_pending = false;
            }
            let (response, rejected) = super::text_input::bounded_edit_with_style(
                ui,
                &mut state.text,
                crate::fleet::FLEET_PROMPT_MAX_BYTES,
                id,
                &catalog.t("fleet.followup.placeholder", &[]),
                super::text_input::BoundedEditStyle::WindowEditor {
                    height: (ui.available_height()
                        - if library.prompts.is_empty() {
                            24.0
                        } else {
                            100.0
                        })
                    .max(136.0),
                },
            );
            if response.changed() {
                self.followup_input_error = rejected;
                self.followup_admission_error = false;
            }
            // 저장된 프롬프트는 **본문에 끼워 넣기만** 한다 — 브로드캐스트처럼 선택 하나로
            // 전송되는 게 아니라 사용자가 이어서 고쳐 쓰는 자리이기 때문이다.
            if !library.prompts.is_empty() {
                ui.add_space(4.0);
                super::popup::choice_input(
                    ui,
                    "fleet_followup_prompt",
                    &catalog.t("fleet.followup.insert", &[]),
                    |ui| {
                        for prompt in &library.prompts {
                            if ui.selectable_label(false, &prompt.title).clicked() {
                                self.followup_input_error =
                                    !append_followup_template(&mut state.text, &prompt.body);
                                if !self.followup_input_error {
                                    self.followup_admission_error = false;
                                }
                            }
                        }
                    },
                );
            }
            ui.add_space(8.0);
            if self.followup_input_error || state.text.len() > crate::fleet::FLEET_PROMPT_MAX_BYTES
            {
                super::popup::notice(
                    ui,
                    &catalog.t("prompt.input_limit", &[]),
                    super::popup::NoticeTone::Error,
                );
            }
            if state
                .effort
                .is_some_and(|level| state.context.request(level).is_none())
            {
                super::popup::notice(
                    ui,
                    &catalog.t("fleet.followup.effort_invalid", &[]),
                    super::popup::NoticeTone::Error,
                );
            }
            if state.context.provider == Some(crate::agent_surface::AgentProvider::Claude) {
                ui.weak(catalog.t("fleet.followup.claude_default", &[]));
            }
            if self.followup_admission_error {
                super::popup::notice(
                    ui,
                    &catalog.t("fleet.followup.admission_rejected", &[]),
                    super::popup::NoticeTone::Error,
                );
            }
        });
        let prompt = state.text.trim();
        let within_limit = state.text.len() <= crate::fleet::FLEET_PROMPT_MAX_BYTES;
        let settings_valid = state
            .effort
            .is_none_or(|level| state.context.request(level).is_some());
        let mut cancelled = false;
        let mut action = None;
        let action = super::popup::footer(ui, Some(&catalog.t("fleet.followup.hint", &[])), |ui| {
            if super::popup::action_button(
                ui,
                &catalog.t("fleet.followup.save", &[]),
                super::popup::ActionTone::Primary,
                !prompt.is_empty()
                    && within_limit
                    && settings_valid
                    && state.target.is_some()
                    && !self.followup_pending,
            )
            .clicked()
            {
                self.followup_pending = true;
                action = Some(FleetAction::ScheduleFollowUp {
                    target: state.target.clone(),
                    workspace_id: state.workspace_id.clone(),
                    session: state.session,
                    prompt: prompt.to_owned(),
                    effort: state.effort.and_then(|level| state.context.request(level)),
                });
            }
            cancelled = super::popup::action_button(
                ui,
                &catalog.t("fleet.followup.cancel", &[]),
                super::popup::ActionTone::Ghost,
                !self.followup_pending,
            )
            .clicked();
            action
        });
        if cancelled {
            self.followup = None;
        }
        action
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
        super::popup::window(
            ctx,
            super::popup::WindowSpec {
                id: egui::Id::new("fleet_broadcast"),
                title: &catalog.t("fleet.broadcast.title", &[]),
                subtitle: "",
                close_label: &catalog.t("popup.dismiss", &[]),
                close_enabled: true,
                default_size: egui::vec2(680.0, 560.0),
                min_size: egui::vec2(360.0, 300.0),
            },
            &mut open,
            |ui| {
                action = self.broadcast_body(ui, sessions, catalog, library);
            },
        );
        if super::popup::take_window_escape(ctx, egui::Id::new("fleet_broadcast")) {
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
        let ready_prompt = super::popup::window_body(ui, |ui| {
            // ① 프롬프트 선택.
            let selected_title = state
                .prompt_id
                .as_ref()
                .and_then(|id| library.get(id))
                .map(|p| p.title.clone())
                .unwrap_or_else(|| catalog.t("fleet.broadcast.pick_prompt", &[]));
            super::popup::choice_input(ui, "fleet_bc_prompt", &selected_title, |ui| {
                for prompt in &library.prompts {
                    let picked = state.prompt_id.as_deref() == Some(prompt.id.as_str());
                    if ui.selectable_label(picked, &prompt.title).clicked() {
                        state.prompt_id = Some(prompt.id.clone());
                        state.params.clear();
                        state.preview = FleetPromptPreview::default();
                        state.confirm_send = false;
                    }
                }
            });
            // ② 파라미터 + 미리보기.
            let prompt = state.prompt_id.as_ref().and_then(|id| library.get(id));
            let ready_prompt = if let Some(prompt) = prompt {
                let (ready, changed) = fleet_prompt_input(
                    ui,
                    prompt,
                    &mut state.params,
                    &mut state.preview,
                    self.prompt_revision,
                    (
                        "fleet_bc_param",
                        crate::prompt_library::PROMPT_BODY_MAX_BYTES,
                    ),
                    catalog,
                );
                if changed {
                    state.confirm_send = false;
                }
                ready
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
            ready_prompt
        });
        // ④ 전송 — 현재 PTY 세션과 교집합만 보낸다. 패널 연 뒤 종료된 stale 대상을 제외해
        // 카운트가 실제 전송 수와 일치하게 한다(세션 순회 순서라 결정적).
        ui.add_space(6.0);
        let effective_targets: Vec<crate::fleet::FleetPromptTarget> = sessions
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
        super::popup::footer(ui, None, |ui| {
            if super::popup::action_button(
                ui,
                &catalog.t("fleet.broadcast.send", &[("count", &count.to_string())]),
                super::popup::ActionTone::Primary,
                can_send,
            )
            .clicked()
            {
                if count >= CONFIRM_THRESHOLD {
                    state.confirm_send = true;
                } else if let Some(prompt_text) = ready_prompt {
                    out = Some(FleetAction::Broadcast {
                        prompt: prompt_text.to_owned(),
                        targets: effective_targets.clone(),
                    });
                }
            }
            if !can_send {
                ui.weak(catalog.t("fleet.broadcast.fill", &[]));
            }
            if state.confirm_send {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        catalog.t("fleet.broadcast.confirm", &[("count", &count.to_string())]),
                    );
                    if super::popup::action_button(
                        ui,
                        &catalog.t("fleet.broadcast.confirm_yes", &[]),
                        super::popup::ActionTone::Primary,
                        can_send,
                    )
                    .clicked()
                        && let Some(prompt_text) = ready_prompt
                    {
                        out = Some(FleetAction::Broadcast {
                            prompt: prompt_text.to_owned(),
                            targets: effective_targets,
                        });
                        state.confirm_send = false;
                    }
                    if super::popup::action_button(
                        ui,
                        &catalog.t("fleet.broadcast.confirm_no", &[]),
                        super::popup::ActionTone::Ghost,
                        true,
                    )
                    .clicked()
                    {
                        state.confirm_send = false;
                    }
                });
            }
        });
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
        super::popup::window(
            ctx,
            super::popup::WindowSpec {
                id: egui::Id::new("fleet_batch_spawn"),
                title: &catalog.t("fleet.batch.title", &[]),
                subtitle: "",
                close_label: &catalog.t("popup.dismiss", &[]),
                close_enabled: true,
                default_size: egui::vec2(520.0, 440.0),
                min_size: egui::vec2(360.0, 300.0),
            },
            &mut open,
            |ui| {
                action = self.batch_spawn_body(ui, agents, max, catalog, library);
            },
        );
        if super::popup::take_window_escape(ctx, egui::Id::new("fleet_batch_spawn")) {
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
        let ready = super::popup::window_body(ui, |ui| {
            // ① 에이전트 선택.
            let selected_name = state
                .agent_id
                .as_deref()
                .and_then(|id| agents.iter().find(|(aid, _)| aid.as_ref() == id))
                .map(|(_, name)| name.to_string())
                .unwrap_or_else(|| catalog.t("fleet.batch.pick_agent", &[]));
            super::popup::choice_input(ui, "fleet_batch_agent", &selected_name, |ui| {
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
            super::popup::choice_input(ui, "fleet_batch_prompt", &selected_title, |ui| {
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
                        state.preview = FleetPromptPreview::default();
                    }
                }
            });
            let selected_prompt = state.prompt_id.as_ref().and_then(|id| library.get(id));
            // ready: 바깥 None이면 시작 불가(파라미터 미입력), Some(None)이면 빈 세션,
            // Some(Some(text))면 렌더된 프롬프트로 시작.
            let ready: Option<Option<&str>> = if let Some(prompt) = selected_prompt {
                fleet_prompt_input(
                    ui,
                    prompt,
                    &mut state.params,
                    &mut state.preview,
                    self.prompt_revision,
                    ("fleet_batch_param", crate::fleet::FLEET_PROMPT_MAX_BYTES),
                    catalog,
                )
                .0
                .map(Some)
            } else if state.prompt_id.is_none() {
                Some(None)
            } else {
                None
            };
            ready
        });
        ui.add_space(10.0);
        // ④ 시작.
        let mut out = None;
        let can_start = state.agent_id.is_some() && ready.is_some();
        super::popup::footer(ui, None, |ui| {
            if super::popup::action_button(
                ui,
                &catalog.t("fleet.batch.start", &[("count", &state.count.to_string())]),
                super::popup::ActionTone::Primary,
                can_start,
            )
            .clicked()
                && let Some(agent_id) = state.agent_id.clone()
                && let Some(prompt) = ready
            {
                out = Some(FleetAction::BatchSpawn {
                    agent_id,
                    count: state.count,
                    prompt: prompt.map(str::to_owned),
                });
            }
            // 프롬프트를 골랐지만 파라미터가 안 채워졌을 때만 힌트(broadcast의 fill과
            // 동일 idiom) — 프롬프트 없음은 항상 시작 가능이라 힌트가 필요 없다.
            if state.prompt_id.is_some() && ready.is_none() {
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
    idle_time: Option<IdleTime>,
) -> Option<CardClick> {
    // 실제 행 높이만 예약한다. 작업 설명은 같은 galley를 측정과 그리기에 재사용한다.
    const CARD_WIDTH: f32 = 252.0;
    const CONTENT_WIDTH: f32 = CARD_WIDTH - 28.0;
    let task = session.task_line.as_deref().map_or_else(
        || {
            catalog.t(
                if matches!(
                    session.state,
                    AgentVisualState::Active
                        | AgentVisualState::Waiting
                        | AgentVisualState::NeedsResponse
                ) {
                    "fleet.task.pending"
                } else {
                    "fleet.task.unknown"
                },
                &[],
            )
        },
        |line| {
            catalog.t(
                if matches!(
                    session.state,
                    AgentVisualState::Active
                        | AgentVisualState::Waiting
                        | AgentVisualState::NeedsResponse
                ) {
                    "fleet.task.current"
                } else {
                    "fleet.task.last"
                },
                &[("value", line)],
            )
        },
    );
    let mut task_job = egui::text::LayoutJob::simple(
        task,
        egui::FontId::proportional(12.0),
        ui.visuals().text_color(),
        CONTENT_WIDTH,
    );
    task_job.wrap.max_rows = 2;
    task_job.wrap.break_anywhere = true;
    let small_font = egui::TextStyle::Small.resolve(ui.style());
    let (task_galley, small_height) = ui.fonts_mut(|fonts| {
        let small_height = fonts.row_height(&small_font);
        (fonts.layout_job(task_job), small_height)
    });
    // Measure the exact styled text once and reuse it when painting. In particular,
    // the model's monospace font can be taller than the proportional Small font.
    let one_line = |text: egui::RichText| {
        egui::WidgetText::from(text).into_galley(
            ui,
            Some(egui::TextWrapMode::Truncate),
            CONTENT_WIDTH,
            egui::TextStyle::Body,
        )
    };
    let title_galley = one_line(egui::RichText::new(&session.title).strong());
    let state_color = status_color(session.state);
    let waiting_galley = session
        .waiting_message
        .as_ref()
        .map(|message| one_line(egui::RichText::new(message).small().color(state_color)));
    let agent_galley = session
        .agent_line
        .as_ref()
        .map(|line| one_line(egui::RichText::new(line).small().weak().monospace()));
    let followup_heading = one_line(
        egui::RichText::new(catalog.t(
            "fleet.followup.list",
            &[("count", if session.followup.is_some() { "1" } else { "0" })],
        ))
        .small()
        .weak(),
    );
    let followup_galley = {
        // Bound layout cost while preserving the full original for editing and hover.
        let preview = session.followup.as_ref().map_or_else(
            || catalog.t("fleet.followup.empty", &[]),
            |prompt| {
                let mut text: String = prompt.chars().take(512).collect();
                if prompt.chars().nth(512).is_some() {
                    text.push('…');
                }
                format!("1. {text}")
            },
        );
        let mut job = egui::text::LayoutJob::simple(
            preview,
            small_font.clone(),
            if session.followup.is_some() {
                status_color(AgentVisualState::Complete)
            } else {
                ui.visuals().weak_text_color()
            },
            CONTENT_WIDTH,
        );
        job.wrap.max_rows = 3;
        job.wrap.break_anywhere = true;
        Some(ui.fonts_mut(|fonts| fonts.layout_job(job)))
    };
    let optional_galleys = [&waiting_galley, &agent_galley, &followup_galley];
    let optional_rows = optional_galleys.iter().filter(|row| row.is_some()).count();
    let optional_height: f32 = optional_galleys
        .iter()
        .filter_map(|row| row.as_ref())
        .map(|galley| galley.size().y)
        .sum();
    let rows = 4 + optional_rows;
    // ui.horizontal reserves at least interact_size.y even for small text labels.
    let status_height = ui
        .spacing()
        .interact_size
        .y
        .max(small_height + ui.spacing().extra_text_line_spacing);
    let card_height = (16.0
        + title_galley.size().y
        + status_height
        + task_galley.size().y
        + optional_height
        + followup_heading.size().y
        + (rows - 1) as f32 * 3.0)
        .ceil();
    let size = egui::vec2(CARD_WIDTH, card_height);
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
    content.add(egui::Label::new(title_galley));
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
        if session.state == AgentVisualState::Idle
            && let Some(since) = idle_time
                .map(|clock| clock.since)
                .or(session.idle_since.filter(|at| *at >= 0 && *at <= now))
        {
            ui.label(
                egui::RichText::new(catalog.t(
                    if idle_time.map_or_else(
                        || session.idle_since == Some(since),
                        |clock| clock.confirmed,
                    ) {
                        "fleet.idle_for"
                    } else {
                        "fleet.idle_observed_for"
                    },
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
    // 3~4행: 현재/마지막 작업을 최대 두 줄로 보여준다. 근거가 없으면 빈 프로젝트명이나
    // 임의 터미널 출력으로 작업을 꾸미지 않고 명시적으로 기록 없음이라 한다.
    content.add(egui::Label::new(task_galley));
    // 4~5행: 대기 사유와 에이전트 모델. 둘 다 있으면 둘 다 보여준다.
    if let Some(galley) = waiting_galley {
        content.add(egui::Label::new(galley));
    }
    if let Some(galley) = agent_galley {
        content.add(egui::Label::new(galley));
    }
    // 마지막 행: 예약 칩. 「예약해뒀다」는 사실이 카드에 없으면 예약해둔 걸 잊는다 — 그러면
    // 나중에 도착한 프롬프트가 내가 안 시킨 일처럼 보인다.
    content.add(egui::Label::new(followup_heading));
    if let Some(galley) = followup_galley {
        let response = content.add(egui::Label::new(galley));
        if let Some(prompt) = &session.followup {
            response.on_hover_text(prompt.as_ref());
        }
    }

    // 이제 내용 **위에서** 상호작용을 잡는다. 카드 전체가 버튼이므로 커서도 바꾼다.
    let response = ui
        .interact(rect, card_id(ui, session), egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let mut click = response.clicked().then_some(CardClick::Open);
    // 예약은 PTY 전용이다 — 구조화 세션은 steer 경로라 WriteInput 대상이 아니다.
    if matches!(session.target, FleetTarget::Pty { .. }) {
        response.context_menu(|ui| {
            if ui
                .add_enabled(
                    session.broadcast_key().is_some(),
                    egui::Button::new(catalog.t("fleet.followup.menu", &[])),
                )
                .clicked()
            {
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
        AgentVisualState::Waiting => "status.needs_approval",
        AgentVisualState::NeedsResponse => "status.waiting",
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
    #[test]
    fn pr4_fleet_live_forms_reject_parameter_count_and_expansion_overflow() {
        use egui_kittest::kittest::Queryable;
        for batch in [false, true] {
            for scenario in [0, 1, 2] {
                if scenario == 2 && !batch {
                    continue;
                }
                let expansion = scenario == 1;
                let body = if expansion {
                    "{{x}}".repeat(129)
                } else if scenario == 2 {
                    "x".repeat(16 * 1024 + 1)
                } else {
                    (0..129).map(|i| format!("{{{{p{i}}}}}")).collect()
                };
                let library = PromptLibrary {
                    prompts: vec![crate::prompt_library::Prompt {
                        id: "bounded-fixture".into(),
                        title: "Fixture".into(),
                        body,
                        tags: vec![],
                    }],
                };
                let params = if expansion {
                    BTreeMap::from([("x".into(), "a".repeat(8192))])
                } else {
                    BTreeMap::new()
                };
                let mut fleet = FleetUi {
                    broadcast: Some(BroadcastState {
                        prompt_id: Some("bounded-fixture".into()),
                        params: params.clone(),
                        ..Default::default()
                    }),
                    batch_spawn: Some(BatchSpawnState {
                        agent_id: Some("fixture".into()),
                        count: 1,
                        prompt_id: Some("bounded-fixture".into()),
                        params,
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                let mut harness = egui_kittest::Harness::builder()
                    .with_size(egui::vec2(700.0, 500.0))
                    .build_ui(|ui| {
                        if batch {
                            let agents = [(Arc::from("fixture"), Arc::from("Fixture"))];
                            let _ = fleet.batch_spawn_body(ui, &agents, 4, &catalog(), &library);
                        } else {
                            let _ = fleet.broadcast_body(ui, &[], &catalog(), &library);
                        }
                    });
                harness.run();
                assert!(
                    harness
                        .query_by_label(&catalog().t("prompt.input_limit", &[]))
                        .is_some(),
                    "actual Fleet form must show limit before enabling action: batch={batch}, expansion={expansion}"
                );
            }
        }
    }

    #[test]
    fn pr4_fleet_followup_actual_paste_overflow_preserves_original() {
        use egui_kittest::kittest::Queryable;
        let fleet = FleetUi {
            followup: Some(FollowUpState {
                target: None,
                workspace_id: "fixture".into(),
                session: runtime::SessionId(1),
                title: "Fixture".into(),
                text: "KEEP".into(),
                effort: None,
                context: crate::followup_settings::EffortContext::default(),
            }),
            ..Default::default()
        };
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(700.0, 500.0))
            .build_ui_state(
                |ui, fleet: &mut FleetUi| {
                    let _ = fleet.followup_body(ui, &catalog(), &PromptLibrary::default());
                },
                fleet,
            );
        harness.run();
        harness
            .get_by_role(egui::accesskit::Role::MultilineTextInput)
            .click();
        harness.run();
        harness
            .input_mut()
            .events
            .push(egui::Event::Paste("가".repeat(6000)));
        harness.run();
        let draft = &harness.state().followup.as_ref().unwrap().text;
        assert!(
            draft == "KEEP",
            "rejected paste changed original: {} bytes",
            draft.len()
        );
        assert!(
            harness
                .query_by_label(&catalog().t("prompt.input_limit", &[]))
                .is_some()
        );
        harness
            .input_mut()
            .events
            .push(egui::Event::Ime(egui::ImeEvent::Commit("😀".repeat(5000))));
        harness.run();
        assert_eq!(harness.state().followup.as_ref().unwrap().text, "KEEP");
    }

    #[test]
    fn pr4_fleet_batch_host_limit_keeps_actual_form_open_without_action() {
        use egui_kittest::kittest::Queryable;
        struct State {
            fleet: FleetUi,
            acted: bool,
        }
        let state = State {
            fleet: FleetUi {
                batch_spawn: Some(BatchSpawnState {
                    agent_id: Some("fixture".into()),
                    count: 1,
                    prompt_id: Some("large".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            acted: false,
        };
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 900.0))
            .build_ui_state(
                |ui, state: &mut State| {
                    let library = PromptLibrary {
                        prompts: vec![crate::prompt_library::Prompt {
                            id: "large".into(),
                            title: "Large".into(),
                            body: "x".repeat(16 * 1024 + 1),
                            tags: vec![],
                        }],
                    };
                    let agents = [(Arc::from("fixture"), Arc::from("Fixture"))];
                    state.acted |= state
                        .fleet
                        .batch_spawn_window(ui.ctx(), &agents, 4, &catalog(), &library)
                        .is_some();
                },
                state,
            );
        harness.run();
        use egui_kittest::kittest::NodeT;
        assert!(
            harness
                .get_by_label(&catalog().t("fleet.batch.start", &[("count", "1")]))
                .accesskit_node()
                .is_disabled(),
            "batch Start must be disabled at the actual host byte limit"
        );
        harness
            .get_by_label(&catalog().t("fleet.batch.start", &[("count", "1")]))
            .click();
        harness.run();
        assert!(
            !harness.state().acted,
            "oversized prompt must not leave the UI as a launch intent"
        );
        assert!(harness.state().fleet.batch_spawn.is_some());
    }

    use super::*;

    #[test]
    fn pr4_fleet_broadcast_keeps_one_mib_while_batch_uses_host_limit() {
        let ctx = egui::Context::default();
        for size in [
            crate::fleet::FLEET_PROMPT_MAX_BYTES + 1,
            crate::prompt_library::PROMPT_BODY_MAX_BYTES,
            crate::prompt_library::PROMPT_BODY_MAX_BYTES + 1,
        ] {
            let prompt = crate::prompt_library::Prompt {
                id: "limit".into(),
                title: "Limit".into(),
                body: "a".repeat(size),
                tags: vec![],
            };
            for max in [
                crate::fleet::FLEET_PROMPT_MAX_BYTES,
                crate::prompt_library::PROMPT_BODY_MAX_BYTES,
            ] {
                let mut preview = FleetPromptPreview::default();
                let mut params = BTreeMap::new();
                let mut ready = None;
                ctx.run_ui(egui::RawInput::default(), |ui| {
                    ready = fleet_prompt_input(
                        ui,
                        &prompt,
                        &mut params,
                        &mut preview,
                        1,
                        ("limit", max),
                        &catalog(),
                    )
                    .0
                    .map(str::len);
                })
                .drop_without_applying_deltas();
                assert_eq!(
                    ready,
                    (size <= max).then_some(size),
                    "size={size}, form limit={max}"
                );
            }
        }
    }

    fn followup_fixture(text: String) -> FollowUpState {
        FollowUpState {
            target: Some(crate::fleet::FleetPromptTarget {
                workspace_id: "fixture".into(),
                runtime_instance: 41,
                session: runtime::SessionId(7),
                execution: crate::agent_detect::AgentExecutionIdentity::fixture(
                    crate::agent_detect::AgentKind::Claude,
                    1,
                ),
            }),
            workspace_id: "fixture".into(),
            session: runtime::SessionId(7),
            title: "Fixture".into(),
            text,
            effort: None,
            context: crate::followup_settings::EffortContext::default(),
        }
    }

    #[test]
    fn pr4_fleet_followup_host_rejection_retains_exact_form_until_accepted() {
        use egui_kittest::kittest::Queryable;
        struct State {
            fleet: FleetUi,
            action: Option<FleetAction>,
        }
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 650.0))
            .build_ui_state(
                |ui, state: &mut State| {
                    if let Some(action) =
                        state
                            .fleet
                            .followup_window(ui.ctx(), &catalog(), &PromptLibrary::default())
                    {
                        state.action = Some(action);
                    }
                },
                State {
                    fleet: FleetUi {
                        followup: Some(followup_fixture("  한글 😀\n다음 작업\t  ".into())),
                        followup_reset_pending: true,
                        ..Default::default()
                    },
                    action: None,
                },
            );
        harness.run();
        harness
            .get_by_label(&catalog().t("fleet.followup.save", &[]))
            .click();
        harness.run();
        let Some(FleetAction::ScheduleFollowUp {
            target: Some(target),
            prompt,
            ..
        }) = harness.state().action.as_ref()
        else {
            panic!("no exact reservation action")
        };
        assert_eq!(prompt, "한글 😀\n다음 작업");
        let target = target.clone();
        assert!(
            harness.state().fleet.followup.is_some(),
            "host has not accepted yet"
        );
        assert!(harness.state().fleet.followup_pending);
        let mut stale = target.clone();
        stale.runtime_instance += 1;
        harness.state_mut().fleet.settle_followup(&stale, true);
        assert!(harness.state().fleet.followup.is_some());
        harness.state_mut().fleet.settle_followup(&target, false);
        harness.run();
        assert_eq!(
            harness.state().fleet.followup.as_ref().unwrap().text,
            "  한글 😀\n다음 작업\t  "
        );
        assert!(
            harness
                .query_by_label(&catalog().t("fleet.followup.admission_rejected", &[]))
                .is_some()
        );
        assert!(!harness.state().fleet.followup_pending);
        harness.state_mut().fleet.settle_followup(&target, true);
        assert!(harness.state().fleet.followup.is_none());
    }

    #[test]
    fn pr4_fleet_followup_template_separator_is_atomic_at_limit() {
        let limit = crate::fleet::FLEET_PROMPT_MAX_BYTES;
        let mut original = "a".repeat(limit - 3);
        let before = original.clone();
        assert!(!append_followup_template(&mut original, "한"));
        assert_eq!(original, before, "separator must count before any mutation");
        original.pop();
        assert!(append_followup_template(&mut original, "한"));
        assert_eq!(original.len(), limit);
        assert!(original.ends_with("\n한"));
        let before = original.clone();
        assert!(!append_followup_template(&mut original, "😀"));
        assert_eq!(original, before);
    }

    #[test]
    fn pr4_fleet_blocked_reservation_cancel_is_visible_without_session_cards() {
        use egui_kittest::kittest::Queryable;
        struct State {
            fleet: FleetUi,
            action: Option<FleetAction>,
        }
        let mut fleet = FleetUi::default();
        fleet.set_followup_summary(1, vec![("closed-workspace".into(), runtime::SessionId(99))]);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(900.0, 600.0))
            .build_ui_state(
                |ui, state: &mut State| {
                    let mut waiting = InboxWaitingUi::new();
                    let page = state.fleet.render(
                        ui,
                        &[],
                        FleetSummary::default(),
                        &catalog(),
                        &PromptLibrary::default(),
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
                    if let Some(action) = page.grid {
                        state.action = Some(action);
                    }
                },
                State {
                    fleet,
                    action: None,
                },
            );
        harness.run();
        assert!(
            harness
                .query_by_label(&catalog().t("fleet.followup.blocked", &[]))
                .is_some()
        );
        harness
            .get_by_label(&catalog().t("fleet.followup.cancel", &[]))
            .click();
        harness.run();
        assert!(
            matches!(harness.state().action.as_ref(), Some(FleetAction::ScheduleFollowUp {
            target: None, workspace_id, session: runtime::SessionId(99), prompt, ..
        }) if workspace_id == "closed-workspace" && prompt.is_empty())
        );
    }

    #[test]
    fn pr4_fleet_followup_reopen_resets_fixed_id_undo_but_stable_form_can_undo() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("fleet_followup_text");
        let mut fleet = FleetUi {
            followup: Some(followup_fixture("BASE".into())),
            followup_reset_pending: true,
            ..Default::default()
        };
        let mut time = 0.0;
        ctx.run_ui(egui::RawInput::default(), |ui| {
            fleet.followup_body(ui, &catalog(), &PromptLibrary::default());
        })
        .drop_without_applying_deltas();
        ctx.memory_mut(|memory| memory.request_focus(id));
        let mut state = egui::TextEdit::load_state(&ctx, id).unwrap();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(
                egui::text::CCursor::new(4),
            )));
        state.store(&ctx, id);
        for _ in 0..20 {
            time += 2.0;
            let mut input = egui::RawInput {
                time: Some(time),
                ..Default::default()
            };
            input.events.push(egui::Event::Text("a".into()));
            ctx.run_ui(input, |ui| {
                fleet.followup_body(ui, &catalog(), &PromptLibrary::default());
            })
            .drop_without_applying_deltas();
            time += 2.0;
            ctx.run_ui(
                egui::RawInput {
                    time: Some(time),
                    ..Default::default()
                },
                |ui| {
                    fleet.followup_body(ui, &catalog(), &PromptLibrary::default());
                },
            )
            .drop_without_applying_deltas();
        }
        fleet.followup_admission_error = true;
        let mut undos = 0;
        for _ in 0..25 {
            let previous = fleet.followup.as_ref().unwrap().text.clone();
            time += 2.0;
            let mut input = egui::RawInput {
                time: Some(time),
                ..Default::default()
            };
            input.events.push(egui::Event::Key {
                key: egui::Key::Z,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND,
            });
            ctx.run_ui(input, |ui| {
                fleet.followup_body(ui, &catalog(), &PromptLibrary::default());
            })
            .drop_without_applying_deltas();
            if fleet.followup.as_ref().unwrap().text == previous {
                break;
            }
            undos += 1;
            assert!(fleet.followup.as_ref().unwrap().text.starts_with("BASE"));
        }
        assert!(
            undos > 0 && undos <= 8,
            "stable form retains bounded useful undo: {undos}"
        );
        fleet.followup = Some(followup_fixture("OTHER_SESSION".into()));
        fleet.followup_reset_pending = true;
        ctx.run_ui(egui::RawInput::default(), |ui| {
            fleet.followup_body(ui, &catalog(), &PromptLibrary::default());
        })
        .drop_without_applying_deltas();
        ctx.memory_mut(|memory| memory.request_focus(id));
        let mut input = egui::RawInput::default();
        input.events.push(egui::Event::Key {
            key: egui::Key::Z,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        ctx.run_ui(input, |ui| {
            fleet.followup_body(ui, &catalog(), &PromptLibrary::default());
        })
        .drop_without_applying_deltas();
        assert_eq!(fleet.followup.as_ref().unwrap().text, "OTHER_SESSION");
    }

    #[test]
    fn popup_audit_followup_behind_confirmation_keeps_its_draft_on_escape() {
        let ctx = egui::Context::default();
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let library = PromptLibrary::default();
        let mut fleet = FleetUi {
            followup: Some(FollowUpState {
                target: Some(crate::fleet::FleetPromptTarget {
                    workspace_id: "ws-1".into(),
                    runtime_instance: 1,
                    session: runtime::SessionId(7),
                    execution: crate::agent_detect::AgentExecutionIdentity::fixture(
                        crate::agent_detect::AgentKind::Claude,
                        1,
                    ),
                }),
                workspace_id: "project".into(),
                session: runtime::SessionId(7),
                title: "Agent".into(),
                text: "Keep this draft".into(),
                effort: None,
                context: crate::followup_settings::EffortContext::default(),
            }),
            ..Default::default()
        };
        let mut input = egui::RawInput::default();
        input.events.push(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        ctx.run_ui(input, |ui| {
            crate::ui::popup::show(
                ui.ctx(),
                crate::ui::popup::PopupSpec {
                    id: egui::Id::new("popup_audit_front_modal"),
                    width: 400.0,
                    title: "Confirm",
                    subtitle: "",
                    close_label: "Close",
                    close_enabled: true,
                },
                |ui| {
                    ui.label("Front confirmation");
                },
            );
            assert!(
                fleet
                    .followup_window(ui.ctx(), &catalog, &library)
                    .is_none()
            );
        })
        .drop_without_applying_deltas();
        assert_eq!(
            fleet.followup.as_ref().map(|draft| draft.text.as_str()),
            Some("Keep this draft")
        );
    }

    #[test]
    fn popup_audit_escape_closes_only_the_front_fleet_form() {
        let ctx = egui::Context::default();
        let catalog = catalog();
        let library = PromptLibrary::default();
        let mut fleet = FleetUi {
            followup: Some(FollowUpState {
                target: Some(crate::fleet::FleetPromptTarget {
                    workspace_id: "ws-1".into(),
                    runtime_instance: 1,
                    session: runtime::SessionId(7),
                    execution: crate::agent_detect::AgentExecutionIdentity::fixture(
                        crate::agent_detect::AgentKind::Claude,
                        1,
                    ),
                }),
                workspace_id: "project".into(),
                session: runtime::SessionId(7),
                title: "Agent".into(),
                text: "Keep draft".into(),
                effort: None,
                context: crate::followup_settings::EffortContext::default(),
            }),
            broadcast: Some(BroadcastState::default()),
            ..Default::default()
        };
        for _ in 0..2 {
            ctx.run_ui(egui::RawInput::default(), |ui| {
                fleet.followup_window(ui.ctx(), &catalog, &library);
                fleet.broadcast_window(ui.ctx(), &[], &catalog, &library);
            })
            .drop_without_applying_deltas();
        }
        let mut input = egui::RawInput::default();
        input.events.push(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        ctx.run_ui(input, |ui| {
            fleet.followup_window(ui.ctx(), &catalog, &library);
            fleet.broadcast_window(ui.ctx(), &[], &catalog, &library);
        })
        .drop_without_applying_deltas();
        assert!(fleet.broadcast.is_none());
        assert_eq!(
            fleet.followup.as_ref().map(|draft| draft.text.as_str()),
            Some("Keep draft")
        );
    }
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
            prompt_target: Some(crate::fleet::FleetPromptTarget {
                workspace_id: workspace_id.into(),
                runtime_instance: 1,
                session: runtime::SessionId(session),
                execution: crate::agent_detect::AgentExecutionIdentity::fixture(
                    crate::agent_detect::AgentKind::Claude,
                    1,
                ),
            }),
            title: format!("session-{session}"),
            state,
            agent_line: None,
            task_line: None,
            waiting_message: None,
            active_workspace: true,
            blocked_since: None,
            idle_since: None,
            idle_generation: None,
            last_output_at: None,
            followup: None,
        }
    }

    #[test]
    fn idle_card_shows_how_long_it_has_awaited_an_instruction() {
        use egui_kittest::kittest::Queryable;

        let mut session = pty_session("ws", 7, AgentVisualState::Idle);
        session.idle_since = Some(100);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(400.0, 200.0))
            .build_ui(move |ui| {
                let _ = card(ui, &session, &catalog(), 220, None);
            });
        harness.run();
        harness.get_by_label("waiting 2:00");
    }

    #[test]
    fn idle_card_does_not_treat_last_output_as_completion_time() {
        use egui_kittest::kittest::Queryable;

        let mut session = pty_session("ws", 7, AgentVisualState::Idle);
        session.last_output_at = Some(100);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(400.0, 200.0))
            .build_ui(move |ui| {
                let _ = card(ui, &session, &catalog(), 220, None);
            });
        harness.run();
        assert!(harness.query_by_label("waiting 2:00").is_none());
    }

    #[test]
    #[ignore = "offscreen PNG for visual review without launching Deppy"]
    fn fleet_card_render_content_sizing() {
        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        let mut idle = pty_session("Serenity", 11, AgentVisualState::Idle);
        idle.title = "Serenity".into();
        idle.task_line = Some("커밋 완료했습니다. docs/CODEX_HANDOFF.md에 진행한 작업과 최종 검증 내용을 정리했습니다.".into());
        idle.agent_line = Some("Codex · gpt-6-sol · xhigh".into());
        idle.idle_since = Some(100);
        idle.followup = Some("1, 2번 테스트를 실행하고 실패 원인을 확인한 뒤 코드 리뷰 결과를 정리해줘. 작업별 검증 명령과 결과도 보고해줘.".into());
        let mut active = pty_session("Serenity", 12, AgentVisualState::Active);
        active.title = "Serenity".into();
        active.task_line = Some("1, 2 진행해".into());
        active.agent_line = Some("Claude · Opus 5.5".into());
        let mut finished = pty_session("Serenity", 13, AgentVisualState::Off);
        finished.title = "Serenity".into();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(820.0, 480.0))
            .build_ui(|ui| {
                section_header(ui, SessionGroup::Active, 2, &catalog);
                ui.horizontal_wrapped(|ui| {
                    let _ = card(ui, &idle, &catalog, 520, None);
                    let _ = card(ui, &active, &catalog, 520, None);
                });
                ui.add_space(6.0);
                section_header(ui, SessionGroup::Finished, 1, &catalog);
                let _ = card(ui, &finished, &catalog, 520, None);
            });
        crate::fonts::install_cjk_fallback(&harness.ctx, None, "JetBrainsMono", "Regular");
        crate::theme::install_palette(&harness.ctx);
        harness.run();
        let output = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/fleet-waiting-next-tasks-0.5.0.png");
        harness.render().unwrap().save(output).unwrap();
    }

    #[test]
    fn card_keeps_model_and_followup_text_inside_its_clip() {
        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        let mut session = pty_session("Serenity", 11, AgentVisualState::Idle);
        session.task_line = Some("커밋 완료했습니다. docs/CODEX_HANDOFF.md에 진행한 작업과 최종 검증 내용을 정리했습니다.".into());
        session.agent_line = Some("Codex · gpt-6-sol · xhigh".into());
        session.idle_since = Some(100);
        for extra_rows in [false, true] {
            if extra_rows {
                session.waiting_message = Some("다음 작업 지시를 기다립니다".into());
                session.followup = Some("관련 테스트를 실행해줘.\n결과를 확인하고 실패한 원인을 정리한 다음 수정 후 다시 테스트하고 검증 명령과 결과를 보고해줘. ".repeat(100).into());
            }
            let mut harness = egui_kittest::Harness::builder()
                .with_size(egui::vec2(400.0, 300.0))
                .build_ui(|ui| {
                    let _ = card(ui, &session, &catalog, 220, None);
                });
            crate::fonts::install_cjk_fallback(&harness.ctx, None, "JetBrainsMono", "Regular");
            harness.run();
            let mut checked = 0;
            for clipped in &harness.output().shapes {
                if let egui::Shape::Text(text) = &clipped.shape
                    && (text.galley.text() == session.agent_line.as_deref().unwrap()
                        || text.galley.text().starts_with("1. "))
                {
                    let painted = text.visual_bounding_rect();
                    assert!(
                        clipped.clip_rect.contains_rect(painted),
                        "text is clipped: {}, painted={painted:?}, clip={:?}",
                        text.galley.text(),
                        clipped.clip_rect
                    );
                    checked += 1;
                }
            }
            assert_eq!(checked, if extra_rows { 2 } else { 1 });
        }
    }

    #[test]
    fn card_uses_content_height_and_shows_two_task_rows() {
        use egui_kittest::kittest::Queryable;

        let empty = pty_session("ws", 10, AgentVisualState::Error);
        let mut compact = egui_kittest::Harness::builder()
            .with_size(egui::vec2(400.0, 220.0))
            .build_ui_state(
                move |ui, measured: &mut f32| {
                    let top = ui.cursor().top();
                    let _ = card(ui, &empty, &catalog(), 220, None);
                    *measured = ui.cursor().top() - top;
                },
                0.0,
            );
        compact.run();
        assert!(
            *compact.state() < 125.0,
            "empty next-task section should reserve only its two compact rows"
        );

        let mut active = pty_session("ws", 11, AgentVisualState::Active);
        active.task_line = Some("예약 트래픽을 불러와 서버 오류 원인을 추적하고 검증 결과를 정리하는 작업을 진행 중입니다".to_owned());
        active.agent_line = Some("Claude · Opus 5.5 · xhigh".to_owned());
        let mut expanded = egui_kittest::Harness::builder()
            .with_size(egui::vec2(400.0, 240.0))
            .build_ui(move |ui| {
                let _ = card(ui, &active, &catalog(), 220, None);
            });
        expanded.run();
        let task = expanded.get_by_label_contains("Working on ·");
        assert!(
            task.rect().height() > 20.0,
            "task must occupy two text rows"
        );
        assert!(task.rect().height() < 40.0, "task must stop after two rows");
    }

    #[test]
    fn fleet_review_fix_submitted_unhooked_turn_does_not_reuse_prior_clock() {
        let mut fleet = FleetUi::default();
        let mut rows = [pty_session("ws", 7, AgentVisualState::Idle)];
        rows[0].idle_since = Some(100);
        rows[0].idle_generation = Some(100_000_000);
        fleet.update_idle_clocks(&rows, 105);
        fleet.observe_runtime_events(
            "ws",
            &[
                runtime::RuntimeEvent::SessionInputSubmitted {
                    session: runtime::SessionId(7),
                    at_micros: 200_000_000,
                },
                runtime::RuntimeEvent::SessionStatusChanged {
                    session: runtime::SessionId(7),
                    status: runtime::SessionStatus::Running,
                },
            ],
            200,
        );
        rows[0].state = AgentVisualState::Active;
        fleet.update_idle_clocks(&rows, 200);
        rows[0].state = AgentVisualState::Idle;
        fleet.update_idle_clocks(&rows, 230);
        let clock = &fleet.idle_started[&FleetIdleKey::for_session(&rows[0])];
        assert_eq!((clock.since, clock.confirmed), (Some(230), false));
        fleet.observe_attention(
            &PtyIdleClocks::from([(("ws".into(), runtime::SessionId(7)), (100, 100_000_000))]),
            &HashSet::new(),
            &HashSet::new(),
            240,
        );
        fleet.update_idle_clocks(&rows, 240);
        assert_eq!(fleet.idle_since(&rows[0]), Some(230));
    }

    #[test]
    fn fleet_review_fix_resolved_question_boundary_after_input_remains_confirmed() {
        let mut fleet = FleetUi::default();
        let mut rows = [pty_session("ws", 7, AgentVisualState::NeedsResponse)];
        fleet.update_idle_clocks(&rows, 125);
        fleet.observe_runtime_events(
            "ws",
            &[runtime::RuntimeEvent::SessionInputSubmitted {
                session: runtime::SessionId(7),
                at_micros: 130_000_000,
            }],
            135,
        );
        rows[0].state = AgentVisualState::Idle;
        rows[0].idle_since = Some(140);
        rows[0].idle_generation = Some(140_000_001);
        fleet.observe_attention(
            &PtyIdleClocks::from([(("ws".into(), runtime::SessionId(7)), (140, 140_000_001))]),
            &HashSet::new(),
            &HashSet::new(),
            145,
        );
        fleet.update_idle_clocks(&rows, 160);
        let clock = &fleet.idle_started[&FleetIdleKey::for_session(&rows[0])];
        assert_eq!((clock.since, clock.confirmed), (Some(140), true));
    }

    #[test]
    fn fleet_review_fix_delayed_submission_keeps_a_newer_completion() {
        let mut fleet = FleetUi::default();
        let mut rows = [pty_session("ws", 7, AgentVisualState::Idle)];
        rows[0].idle_since = Some(300);
        rows[0].idle_generation = Some(300_000_000);
        fleet.update_idle_clocks(&rows, 305);
        fleet.observe_runtime_events(
            "ws",
            &[runtime::RuntimeEvent::SessionInputSubmitted {
                session: runtime::SessionId(7),
                at_micros: 200_000_000,
            }],
            310,
        );
        fleet.update_idle_clocks(&rows, 320);
        let clock = &fleet.idle_started[&FleetIdleKey::for_session(&rows[0])];
        assert_eq!((clock.since, clock.confirmed), (Some(300), true));
    }

    #[test]
    fn fleet_wait_delayed_exact_source_replaces_first_observation() {
        let mut fleet = FleetUi::default();
        let mut rows = vec![pty_session("ws", 11, AgentVisualState::Idle)];
        fleet.update_idle_clocks(&rows, 1000);
        assert_eq!(fleet.idle_since(&rows[0]), Some(1000));
        rows[0].idle_since = Some(100);
        fleet.update_idle_clocks(&rows, 1060);
        assert_eq!(
            fleet.idle_since(&rows[0]),
            Some(100),
            "completion time must replace an estimate, not max with first paint"
        );
    }

    #[test]
    fn fleet_wait_card_shows_numbered_next_task_and_observed_clock() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut session = pty_session("ws", 11, AgentVisualState::Idle);
        session.followup =
            Some("Run the tests, then review the failed cases and report the results. Fix remaining regressions and summarize all verification commands.".into());
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            let _ = card(
                ui,
                &session,
                &catalog,
                120,
                Some(IdleTime {
                    since: 60,
                    confirmed: false,
                }),
            );
        });
        harness.run();
        harness.get_by_label("Next tasks (1)");
        let row = harness.get_by_label_contains("1. Run the tests");
        assert!(row.rect().height() > 20.0, "long queued task must wrap");
        harness.get_by_label("observed waiting 1:00");
    }

    #[test]
    fn fleet_wait_hidden_runtime_turn_resets_only_its_session_and_prunes_exit() {
        let mut fleet = FleetUi::default();
        let mut rows = vec![
            pty_session("ws", 7, AgentVisualState::Idle),
            pty_session("other", 7, AgentVisualState::Idle),
        ];
        fleet.update_idle_clocks(&rows, 100);
        fleet.observe_runtime_events(
            "ws",
            &[runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(7),
                status: runtime::SessionStatus::Running,
            }],
            110,
        );
        // Fleet is reopened only after the turn finished. An old snapshot cannot revive 90.
        fleet.update_idle_clocks(&rows, 130);
        assert_eq!(fleet.idle_since(&rows[0]), Some(130));
        assert_eq!(fleet.idle_since(&rows[1]), Some(100));
        assert!(!fleet.idle_started[&FleetIdleKey::for_session(&rows[0])].confirmed);
        rows[0].idle_since = Some(120);
        fleet.update_idle_clocks(&rows, 140);
        assert_eq!(fleet.idle_since(&rows[0]), Some(120));
        assert!(fleet.idle_started[&FleetIdleKey::for_session(&rows[0])].confirmed);
        fleet.observe_runtime_events(
            "ws",
            &[runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(7),
                exit_code: Some(0),
            }],
            150,
        );
        assert_eq!(fleet.idle_since(&rows[0]), None);
        fleet.update_idle_clocks(&[], 160);
        assert!(fleet.idle_started.is_empty());
    }

    #[test]
    fn fleet_wait_delayed_runtime_drain_accepts_new_completion_before_receipt() {
        let mut fleet = FleetUi::default();
        let mut rows = [pty_session("ws", 7, AgentVisualState::Idle)];
        rows[0].idle_since = Some(90);
        fleet.update_idle_clocks(&rows, 100);
        fleet.observe_runtime_events(
            "ws",
            &[runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(7),
                status: runtime::SessionStatus::Running,
            }],
            130,
        );
        rows[0].idle_since = Some(120);
        fleet.update_idle_clocks(&rows, 140);
        assert_eq!(fleet.idle_since(&rows[0]), Some(120));
        assert!(fleet.idle_started[&FleetIdleKey::for_session(&rows[0])].confirmed);
    }

    #[test]
    fn fleet_wait_same_second_completion_keeps_accumulated_wait() {
        let mut fleet = FleetUi::default();
        let mut rows = [pty_session("ws", 7, AgentVisualState::Idle)];
        rows[0].idle_since = Some(100);
        fleet.update_idle_clocks(&rows, 100);
        fleet.observe_attention(
            &PtyIdleClocks::new(),
            &HashSet::from([("ws".into(), runtime::SessionId(7))]),
            &HashSet::new(),
            100,
        );
        rows[0].idle_generation = Some(2);
        // Another completion within that same second; reopen Fleet much later.
        fleet.update_idle_clocks(&rows, 3600);
        assert_eq!(fleet.idle_since(&rows[0]), Some(100));
        assert_eq!(
            fleet.idle_started[&FleetIdleKey::for_session(&rows[0])].generation,
            Some(2)
        );
    }

    #[test]
    fn fleet_wait_completion_projection_before_runtime_drain_is_not_invalidated() {
        let mut fleet = FleetUi::default();
        let mut rows = [pty_session("ws", 7, AgentVisualState::Idle)];
        rows[0].idle_since = Some(90);
        fleet.update_idle_clocks(&rows, 100);
        rows[0].idle_since = Some(120);
        rows[0].idle_generation = Some(2);
        fleet.observe_attention(
            &PtyIdleClocks::from([(("ws".into(), runtime::SessionId(7)), (120, 2))]),
            &HashSet::new(),
            &HashSet::new(),
            125,
        );
        fleet.observe_runtime_events(
            "ws",
            &[runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(7),
                status: runtime::SessionStatus::Running,
            }],
            130,
        );
        fleet.update_idle_clocks(&rows, 140);
        assert_eq!(fleet.idle_since(&rows[0]), Some(120));
        assert!(fleet.idle_started[&FleetIdleKey::for_session(&rows[0])].confirmed);
    }

    #[test]
    fn fleet_wait_hidden_hook_work_and_missing_source_clear_the_exact_clock() {
        let mut fleet = FleetUi::default();
        let mut rows = [pty_session("ws", 7, AgentVisualState::Idle)];
        rows[0].idle_since = Some(100);
        rows[0].idle_generation = Some(1);
        fleet.update_idle_clocks(&rows, 110);
        fleet.observe_attention(
            &PtyIdleClocks::new(),
            &HashSet::from([("ws".into(), runtime::SessionId(7))]),
            &HashSet::new(),
            120,
        );
        assert_eq!(fleet.idle_since(&rows[0]), None);
        rows[0].idle_since = None;
        rows[0].idle_generation = None;
        fleet.update_idle_clocks(&rows, 150);
        assert_eq!(fleet.idle_since(&rows[0]), Some(150));
        assert!(!fleet.idle_started[&FleetIdleKey::for_session(&rows[0])].confirmed);
        rows[0].idle_since = Some(160);
        rows[0].idle_generation = Some(2);
        fleet.update_idle_clocks(&rows, 170);
        rows[0].idle_since = None;
        rows[0].idle_generation = None;
        fleet.update_idle_clocks(&rows, 180);
        assert_eq!(fleet.idle_since(&rows[0]), Some(180));
        assert!(!fleet.idle_started[&FleetIdleKey::for_session(&rows[0])].confirmed);
    }

    #[test]
    fn fleet_wait_hidden_structured_turn_resets_observed_time() {
        let mut fleet = FleetUi::default();
        let mut row = pty_session("ws", 7, AgentVisualState::Idle);
        row.target = FleetTarget::Structured {
            session_id: "thread-1".into(),
        };
        fleet.update_idle_clocks(&[row.clone()], 100);
        fleet.observe_structured_status("thread-1", AgentVisualState::Active, 110);
        fleet.observe_structured_status("thread-1", AgentVisualState::Idle, 130);
        fleet.update_idle_clocks(&[row.clone()], 140);
        assert_eq!(fleet.idle_since(&row), Some(130));
        fleet.observe_structured_status("thread-1", AgentVisualState::Off, 150);
        assert_eq!(fleet.idle_since(&row), None);
    }

    #[test]
    fn idle_clock_survives_repaints_and_resets_after_a_new_turn() {
        let mut fleet = FleetUi::default();
        let mut rows = [pty_session("ws", 7, AgentVisualState::Idle)];
        fleet.update_idle_clocks(&rows, 100);
        assert_eq!(fleet.idle_since(&rows[0]), Some(100));

        fleet.update_idle_clocks(&rows, 160);
        assert_eq!(fleet.idle_since(&rows[0]), Some(100));

        rows[0].state = AgentVisualState::Active;
        fleet.update_idle_clocks(&rows, 170);
        rows[0].state = AgentVisualState::Idle;
        fleet.update_idle_clocks(&rows, 200);
        assert_eq!(fleet.idle_since(&rows[0]), Some(200));
    }

    #[test]
    fn idle_without_completion_hook_starts_at_first_idle_observation() {
        let mut fleet = FleetUi::default();
        let mut rows = [pty_session("ws", 7, AgentVisualState::Idle)];
        rows[0].last_output_at = Some(100);
        fleet.update_idle_clocks(&rows, 1000);
        assert_eq!(fleet.idle_since(&rows[0]), Some(1000));
        fleet.update_idle_clocks(&rows, 1060);
        assert_eq!(fleet.idle_since(&rows[0]), Some(1000));
    }

    #[test]
    fn idle_clock_does_not_follow_a_replaced_pty_pane() {
        let mut fleet = FleetUi::default();
        let mut rows = [pty_session("ws", 7, AgentVisualState::Idle)];
        fleet.update_idle_clocks(&rows, 100);
        assert_eq!(fleet.idle_since(&rows[0]), Some(100));

        rows[0].target = FleetTarget::Pty {
            session: runtime::SessionId(7),
            tab: runtime::MuxTabId("t1".into()),
            pane: runtime::MuxPaneId("p2".into()),
        };
        rows[0].idle_since = None;
        fleet.update_idle_clocks(&rows, 200);
        assert_eq!(fleet.idle_since(&rows[0]), Some(200));
    }

    #[test]
    fn followup_dropdown_selection_travels_with_original_reservation() {
        use egui_kittest::kittest::Queryable;
        struct State {
            fleet: FleetUi,
            action: Option<FleetAction>,
        }
        let mut form = followup_fixture("Next original task".into());
        form.context = crate::followup_settings::EffortContext {
            provider: Some(crate::agent_surface::AgentProvider::Codex),
            model: "gpt-6.1-sol".into(),
            current: Some("high".into()),
            levels: vec![
                crate::agent_launcher::ReasoningEffort::Low,
                crate::agent_launcher::ReasoningEffort::High,
            ],
        };
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1000.0, 850.0))
            .build_ui_state(
                |ui, state: &mut State| {
                    if let Some(action) =
                        state
                            .fleet
                            .followup_window(ui.ctx(), &catalog(), &PromptLibrary::default())
                    {
                        state.action = Some(action);
                    }
                },
                State {
                    fleet: FleetUi {
                        followup: Some(form),
                        ..Default::default()
                    },
                    action: None,
                },
            );
        harness.run();
        harness.get_by_role(egui::accesskit::Role::ComboBox).click();
        harness.run();
        harness.get_by_label("low").click();
        harness.run();
        harness
            .get_by_label(&catalog().t("fleet.followup.save", &[]))
            .click();
        harness.run();
        let Some(FleetAction::ScheduleFollowUp {
            target,
            session,
            prompt,
            effort,
            ..
        }) = &harness.state().action
        else {
            panic!("expected reservation");
        };
        assert_eq!(*session, runtime::SessionId(7));
        assert!(target.is_some());
        assert_eq!(prompt, "Next original task");
        assert_eq!(
            effort.as_ref().unwrap().level,
            crate::agent_launcher::ReasoningEffort::Low
        );
        assert_eq!(effort.as_ref().unwrap().model, "gpt-6.1-sol");
    }

    #[test]
    fn invalid_effort_after_model_change_cannot_silently_save_keep_current() {
        use egui_kittest::kittest::{NodeT, Queryable};
        let mut form = followup_fixture("Original task".into());
        form.effort = Some(crate::agent_launcher::ReasoningEffort::Max);
        form.context = crate::followup_settings::EffortContext {
            provider: Some(crate::agent_surface::AgentProvider::Codex),
            model: "new-model".into(),
            current: Some("high".into()),
            levels: vec![crate::agent_launcher::ReasoningEffort::High],
        };
        struct State {
            fleet: FleetUi,
            acted: bool,
        }
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1000.0, 850.0))
            .build_ui_state(
                |ui, state: &mut State| {
                    state.acted |= state
                        .fleet
                        .followup_window(ui.ctx(), &catalog(), &PromptLibrary::default())
                        .is_some();
                },
                State {
                    fleet: FleetUi {
                        followup: Some(form),
                        ..Default::default()
                    },
                    acted: false,
                },
            );
        harness.run();
        assert!(
            harness
                .get_by_label(&catalog().t("fleet.followup.save", &[]))
                .accesskit_node()
                .is_disabled()
        );
        harness
            .get_by_label(&catalog().t("fleet.followup.save", &[]))
            .click();
        harness.run();
        assert!(!harness.state().acted);
        assert_eq!(
            harness.state().fleet.followup.as_ref().unwrap().effort,
            Some(crate::agent_launcher::ReasoningEffort::Max)
        );
    }

    #[test]
    fn followup_window_opens_in_screen_center() {
        let mut fleet = FleetUi {
            followup: Some(followup_fixture("Next task".to_owned())),
            ..Default::default()
        };
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1000.0, 800.0))
            .build_ui(move |ui| {
                fleet.followup_window(ui.ctx(), &catalog(), &PromptLibrary::default());
            });
        harness.run();
        let rect = harness
            .ctx
            .memory(|memory| memory.area_rect(egui::Id::new("fleet_followup")))
            .unwrap();
        assert!(
            (rect.center() - egui::pos2(500.0, 400.0)).length() < 2.0,
            "{rect:?}"
        );
    }

    #[test]
    fn running_card_without_description_does_not_claim_no_task() {
        use egui_kittest::kittest::Queryable;
        let mut session = pty_session("ws", 8, AgentVisualState::Active);
        session.agent_line = Some("Codex".to_owned());
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(400.0, 200.0))
            .build_ui(move |ui| {
                let _ = card(ui, &session, &catalog(), 220, None);
            });
        harness.run();
        harness.get_by_label("Task description not received yet");
    }

    #[test]
    fn finished_card_keeps_its_last_task_on_a_separate_line() {
        use egui_kittest::kittest::Queryable;

        let mut session = pty_session("ws", 8, AgentVisualState::Off);
        session.agent_line = Some("Codex · gpt-6-sol".to_owned());
        session.task_line = Some("Folder tree fix completed".to_owned());
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(400.0, 200.0))
            .build_ui(move |ui| {
                let _ = card(ui, &session, &catalog(), 220, None);
            });
        harness.run();
        harness.get_by_label("Last update · Folder tree fix completed");
        harness.get_by_label("Codex · gpt-6-sol");
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
        let mut frame = ctx.run_ui(egui::RawInput::default(), |ui| {
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
        // 테스트 하네스에는 텍스처 업로더가 없으므로 생성된 델타를 명시적으로 비운다.
        frame.textures_delta.clear();
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

    /// MCP 승인 스냅샷과 에이전트 상태는 서로 다른 경로에서 도착한다. 승인 쪽이 먼저
    /// 도착해 세션이 아직 Active여도 펼친 승인의 세션 카드를 다른 묶음에 다시 그리면 안 된다.
    #[test]
    fn 펼친_승인의_세션은_상태가_아직_active여도_중복하지_않는다() {
        use egui_kittest::kittest::Queryable;

        let sessions = [pty_session("ws-1", 7, AgentVisualState::Active)];
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(900.0, 600.0))
            .build_ui(|ui| {
                let catalog = catalog();
                let library = crate::prompt_library::PromptLibrary::default();
                let mut fleet = FleetUi::default();
                let mut waiting = InboxWaitingUi::new();
                let pending = [approval("a1", Some("ws-1:7"))];
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
        assert!(
            harness.query_by_label("read_file").is_some(),
            "승인 카드는 펼쳐져야 한다"
        );
        assert!(
            harness.query_by_label("session-7").is_none(),
            "펼친 승인이 가리키는 Active 세션 카드를 다시 그렸다"
        );
    }

    /// 뒤 큐 항목도 대상 세션 상태가 늦게 도착하면 Compact 행과 Active 카드로 갈라질 수
    /// 있다. 큐에 연결된 세션은 모두 막힌 묶음으로 모아 한 번만 그린다.
    #[test]
    fn 뒤_승인의_active_세션도_막힌_묶음에서_한번만_그린다() {
        use egui_kittest::kittest::Queryable;

        let sessions = [pty_session("ws-1", 7, AgentVisualState::Active)];
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(900.0, 600.0))
            .build_ui(|ui| {
                let catalog = catalog();
                let library = crate::prompt_library::PromptLibrary::default();
                let mut fleet = FleetUi::default();
                let mut waiting = InboxWaitingUi::new();
                let linked = PendingApprovalItem::try_new(
                    "a1".to_owned(),
                    "srv".to_owned(),
                    "linked_tool".to_owned(),
                    "{}".to_owned(),
                    Some("ws-1:7".to_owned()),
                    None,
                    300,
                )
                .unwrap();
                let pending = [approval_at("a0", "older_tool", 100), linked];
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
        assert!(
            harness.query_by_label("older_tool").is_some(),
            "가장 오래된 승인은 펼쳐져야 한다"
        );
        assert!(
            harness.query_by_label("linked_tool").is_none(),
            "세션 카드가 있는 뒤 승인을 Compact 행으로도 그렸다"
        );
        assert!(
            harness.query_by_label("session-7").is_some(),
            "뒤 승인은 연결된 세션 카드 하나로 보여야 한다"
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
    fn 예약된_세션_카드는_번호_목록과_원문을_보여준다() {
        use egui_kittest::kittest::Queryable;

        let outer = catalog();
        let heading = outer.t("fleet.followup.list", &[("count", "1")]);
        let mut session = pty_session("ws-1", 7, AgentVisualState::Active);
        session.followup = Some("테스트 돌리고 실패한 것만 고쳐".into());
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
                .query_by_label("1. 테스트 돌리고 실패한 것만 고쳐")
                .is_some(),
            "번호와 원문이 카드에 보여야 한다"
        );
        harness.get_by_label(&heading);
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
                target: Some(crate::fleet::FleetPromptTarget {
                    workspace_id: "ws-1".into(),
                    runtime_instance: 1,
                    session: runtime::SessionId(7),
                    execution: crate::agent_detect::AgentExecutionIdentity::fixture(
                        crate::agent_detect::AgentKind::Claude,
                        1,
                    ),
                }),
                workspace_id: "ws-1".to_owned(),
                session: runtime::SessionId(7),
                title: "session-7".to_owned(),
                text: "테스트 돌리고 실패한 것만 고쳐".to_owned(),
                effort: None,
                context: crate::followup_settings::EffortContext::default(),
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
                        target: _,
                        workspace_id,
                        session,
                        prompt,
                        ..
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
