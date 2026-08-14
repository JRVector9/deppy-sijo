//! 현재 워크스페이스의 durable agent work turns를 카드 목록으로 보여주는 순수 UI leaf.
//!
//! 저장소 조회, Git 수집, 세션 이동 같은 권한은 갖지 않는다. App이 넘긴 bounded
//! immutable snapshot을 그리며, 밖으로는 durable identity 기반 의도만 내보낸다.

pub struct WorkHistorySnapshot<'a> {
    pub workspace_name: &'a str,
    pub current_branch: Option<&'a str>,
    pub rows: &'a [storage::AgentWorkTurnRow],
    pub loading: bool,
    pub error: Option<WorkHistoryErrorCode>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorkTurnIdentity {
    pub workspace_id: String,
    pub kind: String,
    pub agent_session_id: String,
    pub turn_key: String,
}

impl From<&storage::AgentWorkTurnRow> for WorkTurnIdentity {
    fn from(row: &storage::AgentWorkTurnRow) -> Self {
        Self {
            workspace_id: row.workspace_id.clone(),
            kind: row.kind.clone(),
            agent_session_id: row.agent_session_id.clone(),
            turn_key: row.turn_key.clone(),
        }
    }
}

impl WorkTurnIdentity {
    pub(crate) fn matches(&self, row: &storage::AgentWorkTurnRow) -> bool {
        self.workspace_id == row.workspace_id
            && self.kind == row.kind
            && self.agent_session_id == row.agent_session_id
            && self.turn_key == row.turn_key
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkHistoryErrorCode {
    Busy,
    InvalidData,
    ResourceLimit,
    ReadFailed,
}

/// 현재 세션 pane 헤더 옆에 붙는 **보조 UI 탭**의 상태.
///
/// runtime의 `MuxTabId`/pane과 무관하다 — 이 상태가 바뀌어도 PTY·세션·mux 탭은
/// 생성되거나 종료되지 않는다. 세션 X와 이력 X가 서로 다른 동작인 이유다.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkHistoryTabState {
    #[default]
    Closed,
    OpenInactive,
    OpenActive,
}

impl WorkHistoryTabState {
    /// 탭 chrome이 헤더에 존재하는지. `Closed`면 세션 헤더는 예전 그대로다.
    pub fn is_open(self) -> bool {
        self != Self::Closed
    }

    pub fn is_active(self) -> bool {
        self == Self::OpenActive
    }

    /// 레일 「이력」 클릭 — 닫혀 있으면 열고 활성화, 이미 활성이면 세션으로 돌아가되
    /// 탭은 남긴다.
    pub fn on_rail_click(self) -> Self {
        match self {
            Self::Closed | Self::OpenInactive => Self::OpenActive,
            Self::OpenActive => Self::OpenInactive,
        }
    }

    /// 이력 탭 클릭 — 열려 있을 때만 활성화한다.
    pub fn on_tab_click(self) -> Self {
        match self {
            Self::Closed => Self::Closed,
            Self::OpenInactive | Self::OpenActive => Self::OpenActive,
        }
    }

    /// 세션 탭 클릭 — 터미널을 보여주되 이력 탭은 유지한다.
    pub fn on_session_tab_click(self) -> Self {
        match self {
            Self::Closed => Self::Closed,
            Self::OpenInactive | Self::OpenActive => Self::OpenInactive,
        }
    }

    /// 이력 X — UI 탭만 제거한다. 세션에는 어떤 종료 명령도 보내지 않는다.
    pub fn on_close(self) -> Self {
        Self::Closed
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkHistoryAction {
    Refresh,
    Activate(WorkTurnIdentity),
    ShowDiff(WorkTurnIdentity),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkHistoryPrimaryAction {
    Focus,
    Resume,
    NewRun,
    Disabled(WorkHistoryDisabledReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkHistoryDisabledReason {
    Checking,
    AgentUnavailable,
    Stale,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkHistoryActionPresentation {
    pub identity: WorkTurnIdentity,
    pub primary: WorkHistoryPrimaryAction,
    pub show_diff: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum WorkHistoryFilter {
    #[default]
    All,
    Working,
    Waiting,
    Completed,
}

/// provider 다중 선택 칩 값. 상태 필터(`WorkHistoryFilter`)와 달리 배타적 단일
/// 선택이 아니라 `WorkHistoryUi::providers`에 여러 개가 동시에 담길 수 있다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkHistoryProvider {
    Claude,
    Codex,
    Kimi,
}

impl WorkHistoryProvider {
    const ALL: [Self; 3] = [Self::Claude, Self::Codex, Self::Kimi];

    fn key(self) -> &'static str {
        match self {
            Self::Claude => "history.filter.provider.claude",
            Self::Codex => "history.filter.provider.codex",
            Self::Kimi => "history.filter.provider.kimi",
        }
    }

    /// `row.kind`는 storage 쪽 `agent_work_provider_is_valid`가 소문자 ascii·숫자·
    /// `-`·`_`만 허용하도록 이미 검증해 두므로 대소문자 비교만으로 충분하다.
    /// 목록에 없는 provider(예: grok)는 어떤 칩과도 매치되지 않는다 — provider
    /// 칩이 하나라도 선택된 상태라면 그런 row는 걸러진다.
    fn matches(self, kind: &str) -> bool {
        let expected = match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Kimi => "kimi",
        };
        kind.eq_ignore_ascii_case(expected)
    }
}

/// 카드 정렬 기준. 기본값 `StateFirst`는 기존 동작(state_rank → updated_at desc →
/// source_offset desc)을 그대로 유지한다.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum WorkHistorySortMode {
    #[default]
    StateFirst,
    RecentFirst,
}

impl WorkHistorySortMode {
    fn key(self) -> &'static str {
        match self {
            Self::StateFirst => "history.sort.state_first",
            Self::RecentFirst => "history.sort.recent_first",
        }
    }
}

/// 이력 본문 프레임의 내부 여백. 전체 페이지(22/18)가 아니라 pane body에 얹히는
/// 값이라 좁은 split에서도 카드가 숨 쉴 만큼만 남긴다. `show`가 남은 높이를
/// 계산할 때 같은 상수를 쓴다 — 마법값을 다시 만들지 않기 위한 단일 원천.
const BODY_MARGIN_X: i8 = 14;
const BODY_MARGIN_Y: i8 = 10;

struct MetadataParts<'a> {
    primary: Vec<&'a str>,
    branch: Option<&'a str>,
    git_change_count: Option<u32>,
}

pub struct WorkHistoryUi {
    query: String,
    filter: WorkHistoryFilter,
    providers: Vec<WorkHistoryProvider>,
    sort_mode: WorkHistorySortMode,
    selected: Option<WorkTurnIdentity>,
}

impl WorkHistoryUi {
    pub fn new() -> Self {
        Self {
            query: String::new(),
            filter: WorkHistoryFilter::All,
            providers: Vec::new(),
            sort_mode: WorkHistorySortMode::StateFirst,
            selected: None,
        }
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: WorkHistorySnapshot<'_>,
        presentations: &[WorkHistoryActionPresentation],
        catalog: &i18n::Catalog,
    ) -> Option<WorkHistoryAction> {
        self.reconcile_selection(snapshot.rows);
        let mut action = None;
        let tokens = crate::ui::designall::tokens(ui.visuals());
        let content = egui::Frame::NONE
            .fill(tokens.content_canvas)
            .inner_margin(egui::Margin::symmetric(BODY_MARGIN_X, BODY_MARGIN_Y));
        // 넘겨받은 rect(=pane body)를 그대로 채운다. 상단 탭 스트립은 호출부가 이미
        // 잘라내고 남긴 높이라 여기서 다시 빼지 않는다 — 프레임 자기 여백만 제한다.
        let available_height = ui.available_height();
        let visible = self.visible_rows(snapshot.rows);
        content.show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height((available_height - f32::from(BODY_MARGIN_Y) * 2.0).max(0.0));
            if self.render_context_row(ui, &snapshot, visible.len(), catalog) {
                action = Some(WorkHistoryAction::Refresh);
            }
            ui.add_space(8.0);
            self.render_controls(ui, catalog);
            ui.add_space(8.0);

            if let Some(error) = snapshot.error {
                render_error(ui, error, catalog);
                ui.add_space(8.0);
            }

            if snapshot.rows.is_empty() {
                render_empty(ui, snapshot.loading, catalog);
                return;
            }
            if visible.is_empty() {
                render_centered_message(ui, catalog.t("history.no_results", &[]));
                return;
            }

            let now = unix_now();
            egui::ScrollArea::vertical()
                .id_salt("work-history-cards")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 9.0;
                    for row in visible {
                        let expanded = self
                            .selected
                            .as_ref()
                            .is_some_and(|selected| selected.matches(row));
                        let presentation = presentations
                            .iter()
                            .find(|candidate| candidate.identity.matches(row));
                        let card_action =
                            render_card(ui, row, expanded, now, presentation, catalog);
                        if card_action.toggle {
                            self.toggle_selected(WorkTurnIdentity::from(row));
                        }
                        if action.is_none() {
                            action = card_action.action;
                        }
                    }
                });
        });
        action
    }
    /// 탭 아래 한 줄짜리 컨텍스트 — 워크스페이스·branch·건수는 왼쪽에서 잘리고,
    /// 새로고침·로딩은 오른쪽에서 폭을 먼저 확보한다. pane 폭이 좁아져도 둘이
    /// 겹치지 않고 왼쪽 텍스트만 생략된다(예전 전체 페이지의 2단 heading 대체).
    fn render_context_row(
        &self,
        ui: &mut egui::Ui,
        snapshot: &WorkHistorySnapshot<'_>,
        shown: usize,
        catalog: &i18n::Catalog,
    ) -> bool {
        let mut refresh = false;
        let count = catalog.t(
            "history.count",
            &[
                ("shown", &shown.to_string()),
                ("total", &snapshot.rows.len().to_string()),
            ],
        );
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(catalog.t("history.refresh", &[]))
                    .on_hover_text(catalog.t("history.refresh_hint", &[]))
                    .clicked()
                {
                    refresh = true;
                }
                if snapshot.loading {
                    ui.add(egui::Spinner::new().size(13.0))
                        .on_hover_text(catalog.t("history.loading", &[]));
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    ui.label(egui::RichText::new(snapshot.workspace_name).strong());
                    if let Some(branch) = snapshot.current_branch {
                        metadata_chip(ui, branch);
                    }
                    ui.label(egui::RichText::new(count).small().weak());
                });
            });
        });
        refresh
    }

    fn render_controls(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        ui.add(
            egui::TextEdit::singleline(&mut self.query)
                .hint_text(catalog.t("history.search", &[]))
                .desired_width(f32::INFINITY),
        );
        ui.add_space(7.0);
        ui.horizontal_wrapped(|ui| {
            for filter in [
                WorkHistoryFilter::All,
                WorkHistoryFilter::Working,
                WorkHistoryFilter::Waiting,
                WorkHistoryFilter::Completed,
            ] {
                filter_chip(ui, &mut self.filter, filter, &catalog.t(filter.key(), &[]));
            }
        });
        ui.add_space(7.0);
        ui.horizontal_wrapped(|ui| {
            for provider in WorkHistoryProvider::ALL {
                provider_chip(
                    ui,
                    &mut self.providers,
                    provider,
                    &catalog.t(provider.key(), &[]),
                );
            }
        });
        ui.add_space(7.0);
        ui.horizontal_wrapped(|ui| {
            for sort_mode in [
                WorkHistorySortMode::StateFirst,
                WorkHistorySortMode::RecentFirst,
            ] {
                filter_chip(
                    ui,
                    &mut self.sort_mode,
                    sort_mode,
                    &catalog.t(sort_mode.key(), &[]),
                );
            }
        });
    }

    fn visible_rows<'a>(
        &self,
        rows: &'a [storage::AgentWorkTurnRow],
    ) -> Vec<&'a storage::AgentWorkTurnRow> {
        let query = self.query.trim().to_lowercase();
        let mut visible: Vec<_> = rows
            .iter()
            .filter(|row| self.filter.matches(row.state))
            // provider 칩을 전부 해제한 상태는 "빈 목록"이 아니라 "전체 provider
            // 표시"로 취급한다. 세션 시작 시 아무 칩도 선택돼 있지 않은 기본값이
            // 기존 동작(필터 없음)과 동일해야 하고, 사용자가 마지막 칩을 끄는
            // 순간 카드가 통째로 사라지면 "무필터"가 아니라 "빈 화면"이라는
            // 오해를 준다. 하나라도 선택되면 그때부터 선택된 provider와 매치하는
            // row만 남기며, 상태 필터와는 AND로 결합된다(별도 `.filter()` 체인).
            .filter(|row| {
                self.providers.is_empty()
                    || self
                        .providers
                        .iter()
                        .any(|provider| provider.matches(&row.kind))
            })
            .filter(|row| query.is_empty() || row_matches_query(row, &query))
            .collect();
        visible.sort_by(|left, right| match self.sort_mode {
            WorkHistorySortMode::StateFirst => state_rank(left.state)
                .cmp(&state_rank(right.state))
                .then_with(|| right.updated_at.cmp(&left.updated_at))
                .then_with(|| right.source_offset.cmp(&left.source_offset))
                .then_with(|| left.kind.cmp(&right.kind))
                .then_with(|| left.agent_session_id.cmp(&right.agent_session_id))
                .then_with(|| left.turn_key.cmp(&right.turn_key)),
            WorkHistorySortMode::RecentFirst => right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| right.source_offset.cmp(&left.source_offset))
                .then_with(|| left.kind.cmp(&right.kind))
                .then_with(|| left.agent_session_id.cmp(&right.agent_session_id))
                .then_with(|| left.turn_key.cmp(&right.turn_key)),
        });
        visible
    }

    fn toggle_selected(&mut self, identity: WorkTurnIdentity) {
        if self.selected.as_ref() == Some(&identity) {
            self.selected = None;
        } else {
            self.selected = Some(identity);
        }
    }

    fn reconcile_selection(&mut self, rows: &[storage::AgentWorkTurnRow]) {
        if self
            .selected
            .as_ref()
            .is_some_and(|selected| !rows.iter().any(|row| selected.matches(row)))
        {
            self.selected = None;
        }
    }
}

impl Default for WorkHistoryUi {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkHistoryFilter {
    fn matches(self, state: storage::AgentWorkTurnState) -> bool {
        match self {
            Self::All => true,
            Self::Working => state == storage::AgentWorkTurnState::Working,
            Self::Waiting => state == storage::AgentWorkTurnState::Waiting,
            Self::Completed => state == storage::AgentWorkTurnState::Completed,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::All => "history.filter.all",
            Self::Working => "history.filter.working",
            Self::Waiting => "history.filter.waiting",
            Self::Completed => "history.filter.completed",
        }
    }
}

fn state_rank(state: storage::AgentWorkTurnState) -> u8 {
    match state {
        storage::AgentWorkTurnState::Working => 0,
        storage::AgentWorkTurnState::Waiting => 1,
        storage::AgentWorkTurnState::Completed => 2,
    }
}

fn row_matches_query(row: &storage::AgentWorkTurnRow, query: &str) -> bool {
    [
        Some(row.instruction.as_str()),
        row.agent_summary.as_deref(),
        Some(row.kind.as_str()),
        row.model.as_deref(),
        row.effort.as_deref(),
        row.branch.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|value| value.to_lowercase().contains(query))
}

/// 배타적 단일 선택 칩. 상태 필터와 정렬 모드 둘 다 "값 하나만 켜져 있다"는
/// 같은 모양이라 제네릭 하나로 공유한다.
fn filter_chip<T: Copy + PartialEq>(ui: &mut egui::Ui, selected: &mut T, value: T, label: &str) {
    if ui.selectable_label(*selected == value, label).clicked() {
        *selected = value;
    }
}

/// provider 칩은 배타적 선택이 아니라 토글이다 — 이미 켜져 있으면 끄고, 꺼져
/// 있으면 켠다. 여러 개를 동시에 켤 수 있어 대입만 하는 `filter_chip`과는
/// 다른 헬퍼가 필요하다.
fn provider_chip(
    ui: &mut egui::Ui,
    selected: &mut Vec<WorkHistoryProvider>,
    value: WorkHistoryProvider,
    label: &str,
) {
    let active = selected.contains(&value);
    if ui.selectable_label(active, label).clicked() {
        if active {
            selected.retain(|provider| *provider != value);
        } else {
            selected.push(value);
        }
    }
}

fn render_card(
    ui: &mut egui::Ui,
    row: &storage::AgentWorkTurnRow,
    expanded: bool,
    now: i64,
    presentation: Option<&WorkHistoryActionPresentation>,
    catalog: &i18n::Catalog,
) -> CardAction {
    let tokens = crate::ui::designall::tokens(ui.visuals());
    let fill = if expanded {
        tokens.selected_background
    } else {
        tokens.app_background
    };
    let stroke = if expanded {
        egui::Stroke::new(1.0, tokens.accent)
    } else {
        egui::Stroke::new(1.0, tokens.separator)
    };
    let shown = egui::Frame::NONE
        .fill(fill)
        .stroke(stroke)
        .corner_radius(egui::CornerRadius::same(5))
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            let mut action = None;
            let toggle = ui.scope_builder(
                egui::UiBuilder::new()
                    .id_salt((
                        "work-history-card",
                        &row.workspace_id,
                        &row.kind,
                        &row.agent_session_id,
                        &row.turn_key,
                    ))
                    .sense(egui::Sense::click()),
                |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        provider_badge(ui, &row.kind);
                        ui.vertical(|ui| {
                            ui.set_width(ui.available_width());
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(relative_age_text(
                                            catalog,
                                            row.updated_at,
                                            now,
                                        ))
                                        .small()
                                        .weak(),
                                    );
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(&row.instruction)
                                                .strong()
                                                .size(15.0),
                                        )
                                        .truncate(),
                                    );
                                },
                            );
                            ui.add_space(5.0);
                            ui.horizontal(|ui| {
                                status_dot(ui, row.state);
                                let summary = row
                                    .agent_summary
                                    .as_deref()
                                    .unwrap_or_else(|| catalog_key_for_summary_fallback(row.state));
                                let summary = if row.agent_summary.is_some() {
                                    summary.to_owned()
                                } else {
                                    catalog.t(summary, &[])
                                };
                                ui.add(
                                    egui::Label::new(egui::RichText::new(summary).weak())
                                        .truncate(),
                                );
                            });
                        });
                    });
                    ui.add_space(8.0);
                    render_metadata(ui, row, catalog);
                },
            );

            if expanded {
                ui.add_space(10.0);
                crate::ui::hairline(ui);
                expanded_text(
                    ui,
                    &catalog.t("history.card.instruction", &[]),
                    &row.instruction,
                );
                ui.add_space(8.0);
                let summary = row
                    .agent_summary
                    .as_deref()
                    .map(str::to_owned)
                    .unwrap_or_else(|| catalog.t("history.card.no_summary", &[]));
                expanded_text(ui, &catalog.t("history.card.latest_work", &[]), &summary);
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    if let Some(presentation) = presentation {
                        let (label_key, enabled) = match presentation.primary {
                            WorkHistoryPrimaryAction::Focus => ("history.action.focus", true),
                            WorkHistoryPrimaryAction::Resume => ("history.action.resume", true),
                            WorkHistoryPrimaryAction::NewRun => ("history.action.new_run", true),
                            WorkHistoryPrimaryAction::Disabled(_) => {
                                ("history.action.unavailable", false)
                            }
                        };
                        if ui
                            .add_enabled(enabled, egui::Button::new(catalog.t(label_key, &[])))
                            .clicked()
                        {
                            action =
                                Some(WorkHistoryAction::Activate(presentation.identity.clone()));
                        }
                        if presentation.show_diff
                            && ui
                                .button(catalog.t("history.action.show_diff", &[]))
                                .on_hover_text(catalog.t("history.action.show_diff_hint", &[]))
                                .clicked()
                        {
                            action =
                                Some(WorkHistoryAction::ShowDiff(presentation.identity.clone()));
                        }
                    }
                });
                if let Some(WorkHistoryActionPresentation {
                    primary: WorkHistoryPrimaryAction::Disabled(reason),
                    ..
                }) = presentation
                {
                    let key = match reason {
                        WorkHistoryDisabledReason::Checking => "history.action.disabled.checking",
                        WorkHistoryDisabledReason::AgentUnavailable => {
                            "history.action.disabled.unavailable"
                        }
                        WorkHistoryDisabledReason::Stale => "history.action.disabled.stale",
                    };
                    ui.weak(catalog.t(key, &[]));
                }
            }
            (action, toggle.response)
        });
    let response = shown
        .inner
        .1
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Button,
            ui.is_enabled(),
            expanded,
            &row.instruction,
        )
    });
    CardAction {
        toggle: response.clicked(),
        action: shown.inner.0,
    }
}

struct CardAction {
    toggle: bool,
    action: Option<WorkHistoryAction>,
}

fn expanded_text(ui: &mut egui::Ui, label: &str, body: &str) {
    ui.label(egui::RichText::new(label).small().weak());
    ui.add(egui::Label::new(body).wrap());
}

fn metadata_parts(row: &storage::AgentWorkTurnRow) -> MetadataParts<'_> {
    let mut primary = vec![provider_label(&row.kind)];
    if let Some(model) = &row.model {
        primary.push(model.as_str());
    }
    if let Some(effort) = &row.effort {
        primary.push(effort.as_str());
    }
    MetadataParts {
        primary,
        branch: row.branch.as_deref(),
        git_change_count: row.git_change_count,
    }
}

fn render_metadata(ui: &mut egui::Ui, row: &storage::AgentWorkTurnRow, catalog: &i18n::Catalog) {
    let metadata = metadata_parts(row);
    ui.horizontal_wrapped(|ui| {
        for (index, part) in metadata.primary.iter().enumerate() {
            if index > 0 {
                ui.label(egui::RichText::new("·").small().weak());
            }
            ui.label(egui::RichText::new(*part).small().weak().monospace());
        }
        if let Some(branch) = metadata.branch {
            metadata_chip(ui, branch);
        }
        if let Some(count) = metadata.git_change_count {
            let count = count.to_string();
            metadata_chip(ui, &catalog.t("history.card.changes", &[("count", &count)]));
        }
        ui.label(egui::RichText::new("·").small().weak());
        ui.label(
            egui::RichText::new(state_label(row.state, catalog))
                .small()
                .color(state_color(row.state, ui.visuals())),
        );
    });
}

fn metadata_chip(ui: &mut egui::Ui, text: &str) {
    let tokens = crate::ui::designall::tokens(ui.visuals());
    egui::Frame::NONE
        .fill(tokens.input_background)
        .stroke(egui::Stroke::new(1.0, tokens.separator))
        .corner_radius(egui::CornerRadius::same(3))
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).small().weak().monospace());
        });
}

fn provider_label(kind: &str) -> &str {
    match kind.trim().to_ascii_lowercase().as_str() {
        "claude" | "claude code" => "Claude Code",
        "codex" => "Codex",
        "kimi" | "kimi cli" => "Kimi CLI",
        "grok" => "Grok",
        _ => kind,
    }
}

fn provider_badge(ui: &mut egui::Ui, kind: &str) {
    let label = provider_label(kind);
    let badge = match kind.trim().to_ascii_lowercase().as_str() {
        "claude" | "claude code" => "CL".to_owned(),
        "codex" => "CX".to_owned(),
        "kimi" | "kimi cli" => "KI".to_owned(),
        "grok" => "GK".to_owned(),
        _ => label
            .chars()
            .filter(|ch| ch.is_alphanumeric())
            .take(2)
            .flat_map(|ch| ch.to_uppercase())
            .collect(),
    };
    let color = match kind.trim().to_ascii_lowercase().as_str() {
        "claude" | "claude code" => egui::Color32::from_rgb(0xd9, 0x70, 0x4e),
        "codex" => egui::Color32::from_rgb(0x10, 0xa3, 0x7f),
        "kimi" | "kimi cli" => egui::Color32::from_rgb(0x42, 0x73, 0xda),
        "grok" => egui::Color32::from_rgb(0x52, 0x56, 0x60),
        _ => crate::ui::designall::tokens(ui.visuals()).accent,
    };
    let (rect, _) = ui.allocate_exact_size(egui::vec2(34.0, 34.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 7.0, color);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        badge,
        egui::FontId::proportional(11.0),
        egui::Color32::WHITE,
    );
}

fn status_dot(ui: &mut egui::Ui, state: storage::AgentWorkTurnState) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 14.0), egui::Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), 4.0, state_color(state, ui.visuals()));
}

fn state_color(state: storage::AgentWorkTurnState, visuals: &egui::Visuals) -> egui::Color32 {
    let tokens = crate::ui::designall::tokens(visuals);
    match state {
        storage::AgentWorkTurnState::Working => tokens.success,
        storage::AgentWorkTurnState::Waiting => tokens.warning,
        storage::AgentWorkTurnState::Completed => tokens.muted_text,
    }
}

fn state_label(state: storage::AgentWorkTurnState, catalog: &i18n::Catalog) -> String {
    let key = match state {
        storage::AgentWorkTurnState::Working => "history.state.working",
        storage::AgentWorkTurnState::Waiting => "history.state.waiting",
        storage::AgentWorkTurnState::Completed => "history.state.completed",
    };
    catalog.t(key, &[])
}

fn catalog_key_for_summary_fallback(state: storage::AgentWorkTurnState) -> &'static str {
    match state {
        storage::AgentWorkTurnState::Working => "history.card.no_summary.working",
        storage::AgentWorkTurnState::Waiting => "history.card.no_summary.waiting",
        storage::AgentWorkTurnState::Completed => "history.card.no_summary.completed",
    }
}

fn render_error(ui: &mut egui::Ui, error: WorkHistoryErrorCode, catalog: &i18n::Catalog) {
    let key = match error {
        WorkHistoryErrorCode::Busy => "history.error.busy",
        WorkHistoryErrorCode::InvalidData => "history.error.invalid_data",
        WorkHistoryErrorCode::ResourceLimit => "history.error.resource_limit",
        WorkHistoryErrorCode::ReadFailed => "history.error.read_failed",
    };
    let tokens = crate::ui::designall::tokens(ui.visuals());
    egui::Frame::NONE
        .fill(tokens.error.gamma_multiply(0.12))
        .stroke(egui::Stroke::new(1.0, tokens.error.gamma_multiply(0.5)))
        .inner_margin(egui::Margin::symmetric(10, 7))
        .show(ui, |ui| {
            ui.colored_label(tokens.error, catalog.t(key, &[]));
        });
}

fn render_empty(ui: &mut egui::Ui, loading: bool, catalog: &i18n::Catalog) {
    if loading {
        render_centered_message(ui, catalog.t("history.loading", &[]));
    } else {
        ui.vertical_centered(|ui| {
            ui.add_space(48.0);
            ui.strong(catalog.t("history.empty", &[]));
            ui.weak(catalog.t("history.empty_hint", &[]));
        });
    }
}

fn render_centered_message(ui: &mut egui::Ui, message: String) {
    ui.vertical_centered(|ui| {
        ui.add_space(48.0);
        ui.weak(message);
    });
}

fn relative_age_text(catalog: &i18n::Catalog, updated_at: i64, now: i64) -> String {
    let seconds = now.saturating_sub(updated_at).max(0);
    if seconds < 60 {
        catalog.t("history.time.just_now", &[])
    } else if seconds < 3_600 {
        let count = (seconds / 60).to_string();
        catalog.t("history.time.minutes", &[("count", &count)])
    } else if seconds < 86_400 {
        let count = (seconds / 3_600).to_string();
        catalog.t("history.time.hours", &[("count", &count)])
    } else {
        let count = (seconds / 86_400).to_string();
        catalog.t("history.time.days", &[("count", &count)])
    }
}

fn unix_now() -> i64 {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    seconds.min(i64::MAX as u64) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 핸드오프가 고정한 상태 기계 — 레일 재클릭은 탭을 **지우지 않고** 세션으로만
    /// 돌아가고, 탭 제거는 이력 X 전용이다.
    #[test]
    fn 이력탭_상태기계는_레일_탭_세션_닫기_규칙을_지킨다() {
        use WorkHistoryTabState::{Closed, OpenActive, OpenInactive};

        assert_eq!(Closed.on_rail_click(), OpenActive, "레일: 닫힘 → 열고 활성");
        assert_eq!(
            OpenInactive.on_rail_click(),
            OpenActive,
            "레일: 열림 → 활성"
        );
        assert_eq!(
            OpenActive.on_rail_click(),
            OpenInactive,
            "레일 재클릭은 세션으로 돌아가되 탭은 남긴다"
        );

        assert_eq!(OpenInactive.on_tab_click(), OpenActive);
        assert_eq!(OpenActive.on_tab_click(), OpenActive);
        assert_eq!(
            Closed.on_tab_click(),
            Closed,
            "없는 탭은 클릭으로 살아나지 않는다"
        );

        assert_eq!(OpenActive.on_session_tab_click(), OpenInactive);
        assert_eq!(OpenInactive.on_session_tab_click(), OpenInactive);

        for state in [Closed, OpenInactive, OpenActive] {
            assert_eq!(state.on_close(), Closed, "이력 X는 항상 탭만 제거한다");
        }

        assert!(OpenActive.is_active());
        assert!(
            !OpenInactive.is_active(),
            "열려 있어도 비활성은 레일을 켜지 않는다"
        );
        assert!(!Closed.is_active());

        // 탭 chrome 존재 여부 — 한 번도 열지 않았거나 이력 X로 닫으면 헤더에 탭이 없다.
        assert!(
            !Closed.is_open(),
            "열기 전에는 헤더에 이력 탭이 없어야 한다"
        );
        assert!(OpenInactive.is_open());
        assert!(OpenActive.is_open());
        assert!(
            !OpenActive.on_close().is_open(),
            "이력 X 뒤에는 탭 chrome이 사라져야 한다"
        );
    }

    fn row(
        turn_key: &str,
        state: storage::AgentWorkTurnState,
        updated_at: i64,
    ) -> storage::AgentWorkTurnRow {
        storage::AgentWorkTurnRow {
            workspace_id: "workspace-a".to_owned(),
            pane_id: "pane-a".to_owned(),
            kind: "codex".to_owned(),
            agent_session_id: "agent-session-a".to_owned(),
            turn_key: turn_key.to_owned(),
            source_offset: updated_at as u64,
            instruction: format!("Instruction {turn_key}"),
            agent_summary: Some(format!("Summary {turn_key}")),
            model: Some("gpt-5.6-sol".to_owned()),
            effort: Some("xhigh".to_owned()),
            cwd: Some("/private/project".to_owned()),
            branch: Some("feature/history".to_owned()),
            git_change_count: Some(3),
            state,
            occurred_at: Some(updated_at - 1),
            updated_at,
        }
    }

    #[derive(Default)]
    struct CardInteractionCapture {
        toggles: usize,
        actions: Vec<WorkHistoryAction>,
    }

    fn card_harness<'a>(
        catalog: &'a i18n::Catalog,
        candidate: &'a storage::AgentWorkTurnRow,
        presentation: &'a WorkHistoryActionPresentation,
    ) -> egui_kittest::Harness<'a, CardInteractionCapture> {
        egui_kittest::Harness::new_ui_state(
            move |ui, capture: &mut CardInteractionCapture| {
                let result = render_card(ui, candidate, true, 10, Some(presentation), catalog);
                if result.toggle {
                    capture.toggles += 1;
                }
                if let Some(action) = result.action {
                    capture.actions.push(action);
                }
            },
            CardInteractionCapture::default(),
        )
    }

    fn assert_primary_button_wins_card_hit_test(primary: WorkHistoryPrimaryAction, label: &str) {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let candidate = row("button-hit-test", storage::AgentWorkTurnState::Working, 10);
        let identity = WorkTurnIdentity::from(&candidate);
        let presentation = WorkHistoryActionPresentation {
            identity: identity.clone(),
            primary,
            show_diff: true,
        };
        let mut harness = card_harness(&catalog, &candidate, &presentation);

        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, label)
            .click();
        harness.run();

        assert_eq!(
            harness.state().actions,
            vec![WorkHistoryAction::Activate(identity)]
        );
        assert_eq!(harness.state().toggles, 0);
    }

    #[test]
    fn kittest_primary_buttons_are_not_intercepted_by_card_toggle() {
        for (primary, label) in [
            (WorkHistoryPrimaryAction::Focus, "Go to current session"),
            (WorkHistoryPrimaryAction::Resume, "Resume"),
            (WorkHistoryPrimaryAction::NewRun, "New run"),
        ] {
            assert_primary_button_wins_card_hit_test(primary, label);
        }
    }

    #[test]
    fn kittest_diff_button_is_not_intercepted_by_card_toggle() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let candidate = row("diff-hit-test", storage::AgentWorkTurnState::Completed, 10);
        let identity = WorkTurnIdentity::from(&candidate);
        let presentation = WorkHistoryActionPresentation {
            identity: identity.clone(),
            primary: WorkHistoryPrimaryAction::NewRun,
            show_diff: true,
        };
        let mut harness = card_harness(&catalog, &candidate, &presentation);

        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "View Git changes")
            .click();
        harness.run();

        assert_eq!(
            harness.state().actions,
            vec![WorkHistoryAction::ShowDiff(identity)]
        );
        assert_eq!(harness.state().toggles, 0);
    }

    #[test]
    fn kittest_card_toggle_does_not_contain_action_buttons() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let candidate = row(
            "accessibility-tree",
            storage::AgentWorkTurnState::Completed,
            10,
        );
        let presentation = WorkHistoryActionPresentation {
            identity: WorkTurnIdentity::from(&candidate),
            primary: WorkHistoryPrimaryAction::NewRun,
            show_diff: true,
        };
        let harness = card_harness(&catalog, &candidate, &presentation);
        let card_toggle =
            harness.get_by_role_and_label(egui::accesskit::Role::Button, &candidate.instruction);

        for label in ["New run", "View Git changes"] {
            assert!(
                card_toggle
                    .query_by_role_and_label(egui::accesskit::Role::Button, label)
                    .is_none(),
                "card toggle must not contain action button {label}"
            );
        }
    }

    #[test]
    fn kittest_card_background_click_only_toggles_card() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let candidate = row(
            "background-hit-test",
            storage::AgentWorkTurnState::Completed,
            10,
        );
        let presentation = WorkHistoryActionPresentation {
            identity: WorkTurnIdentity::from(&candidate),
            primary: WorkHistoryPrimaryAction::NewRun,
            show_diff: true,
        };
        let mut harness = card_harness(&catalog, &candidate, &presentation);
        let card =
            harness.get_by_role_and_label(egui::accesskit::Role::Button, &candidate.instruction);
        let click_pos = card.rect().left_top() + egui::vec2(3.0, 3.0);

        harness.hover_at(click_pos);
        for pressed in [true, false] {
            harness.event(egui::Event::PointerButton {
                pos: click_pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
        }
        harness.run();

        assert_eq!(harness.state().toggles, 1);
        assert!(harness.state().actions.is_empty());
    }

    #[test]
    fn 정렬은_작업중_확인필요_완료_순이고_같은상태는_최신순이다() {
        let mut same_second_newer = row(
            "same-second-newer",
            storage::AgentWorkTurnState::Working,
            20,
        );
        same_second_newer.source_offset = 21;
        let rows = vec![
            row("completed", storage::AgentWorkTurnState::Completed, 90),
            row("working-old", storage::AgentWorkTurnState::Working, 10),
            row("waiting", storage::AgentWorkTurnState::Waiting, 100),
            row("working-new", storage::AgentWorkTurnState::Working, 20),
            same_second_newer,
        ];
        let ui = WorkHistoryUi::new();

        let keys: Vec<&str> = ui
            .visible_rows(&rows)
            .into_iter()
            .map(|row| row.turn_key.as_str())
            .collect();

        assert_eq!(
            keys,
            [
                "same-second-newer",
                "working-new",
                "working-old",
                "waiting",
                "completed",
            ]
        );
    }

    #[test]
    fn 검색은_대소문자없이_지시_요약_에이전트_모델_effort_branch를_찾는다() {
        let mut candidate = row("turn", storage::AgentWorkTurnState::Completed, 10);
        candidate.instruction = "Implement billing page".to_owned();
        candidate.agent_summary = Some("Reviewed OAuth flow".to_owned());
        candidate.kind = "Claude Code".to_owned();
        candidate.model = Some("Opus [1M]".to_owned());
        candidate.effort = Some("High".to_owned());
        candidate.branch = Some("Feature/Checkout".to_owned());
        let rows = vec![candidate];

        for query in ["BILLING", "oauth", "claude", "OPUS", "high", "checkout"] {
            let mut ui = WorkHistoryUi::new();
            ui.query = query.to_owned();
            assert_eq!(ui.visible_rows(&rows).len(), 1, "query={query}");
        }
    }

    fn row_with_kind(
        turn_key: &str,
        kind: &str,
        state: storage::AgentWorkTurnState,
        updated_at: i64,
    ) -> storage::AgentWorkTurnRow {
        let mut candidate = row(turn_key, state, updated_at);
        candidate.kind = kind.to_owned();
        candidate
    }

    #[test]
    fn provider_필터는_상태_필터와_and로_결합한다() {
        let rows = vec![
            row_with_kind(
                "claude-working",
                "claude",
                storage::AgentWorkTurnState::Working,
                3,
            ),
            row_with_kind(
                "claude-completed",
                "claude",
                storage::AgentWorkTurnState::Completed,
                2,
            ),
            row_with_kind(
                "codex-working",
                "codex",
                storage::AgentWorkTurnState::Working,
                1,
            ),
        ];
        let mut ui = WorkHistoryUi::new();
        ui.providers = vec![WorkHistoryProvider::Claude];
        ui.filter = WorkHistoryFilter::Working;

        let keys: Vec<&str> = ui
            .visible_rows(&rows)
            .into_iter()
            .map(|row| row.turn_key.as_str())
            .collect();

        assert_eq!(
            keys,
            ["claude-working"],
            "claude만 선택 + working 필터는 claude이면서 working인 row만 남겨야 한다"
        );
    }

    #[test]
    fn provider_칩을_전부_해제하면_전체_provider가_보인다() {
        let rows = vec![
            row_with_kind("claude", "claude", storage::AgentWorkTurnState::Working, 3),
            row_with_kind("codex", "codex", storage::AgentWorkTurnState::Working, 2),
            row_with_kind("kimi", "kimi", storage::AgentWorkTurnState::Working, 1),
        ];
        let ui = WorkHistoryUi::new();
        assert!(
            ui.providers.is_empty(),
            "기본값은 provider 칩이 전부 미선택이어야 한다"
        );

        assert_eq!(
            ui.visible_rows(&rows).len(),
            3,
            "전부 해제는 빈 목록이 아니라 전체 provider 표시로 취급한다"
        );
    }

    #[test]
    fn 정렬모드_최신순은_상태와_무관하게_updated_at_desc다() {
        let rows = vec![
            row(
                "completed-newest",
                storage::AgentWorkTurnState::Completed,
                30,
            ),
            row("working-oldest", storage::AgentWorkTurnState::Working, 10),
            row("waiting-mid", storage::AgentWorkTurnState::Waiting, 20),
        ];
        let mut ui = WorkHistoryUi::new();
        ui.sort_mode = WorkHistorySortMode::RecentFirst;

        let keys: Vec<&str> = ui
            .visible_rows(&rows)
            .into_iter()
            .map(|row| row.turn_key.as_str())
            .collect();

        assert_eq!(
            keys,
            ["completed-newest", "waiting-mid", "working-oldest"],
            "최신순은 state_rank를 무시하고 updated_at desc만 본다"
        );
    }

    #[test]
    fn 네가지_필터는_해당상태만_남긴다() {
        let rows = vec![
            row("working", storage::AgentWorkTurnState::Working, 3),
            row("waiting", storage::AgentWorkTurnState::Waiting, 2),
            row("completed", storage::AgentWorkTurnState::Completed, 1),
        ];
        let mut ui = WorkHistoryUi::new();
        assert_eq!(ui.visible_rows(&rows).len(), 3);

        for (filter, expected) in [
            (WorkHistoryFilter::Working, "working"),
            (WorkHistoryFilter::Waiting, "waiting"),
            (WorkHistoryFilter::Completed, "completed"),
        ] {
            ui.filter = filter;
            let visible = ui.visible_rows(&rows);
            assert_eq!(visible.len(), 1);
            assert_eq!(visible[0].turn_key, expected);
        }
    }

    #[test]
    fn 같은카드를_다시고르면_접힌다() {
        let identity =
            WorkTurnIdentity::from(&row("toggle", storage::AgentWorkTurnState::Working, 1));
        let mut ui = WorkHistoryUi::new();

        ui.toggle_selected(identity.clone());
        assert!(ui.selected.as_ref() == Some(&identity));
        ui.toggle_selected(identity);
        assert!(ui.selected.is_none());
    }

    #[test]
    fn snapshot교체는_identity가_남을때만_선택을_보존한다() {
        let kept = row("kept", storage::AgentWorkTurnState::Working, 2);
        let replacement = row("other", storage::AgentWorkTurnState::Completed, 1);
        let mut ui = WorkHistoryUi::new();
        ui.selected = Some(WorkTurnIdentity::from(&kept));

        ui.reconcile_selection(std::slice::from_ref(&kept));
        assert!(ui.selected.is_some());
        ui.reconcile_selection(&[replacement]);
        assert!(ui.selected.is_none());
    }

    #[test]
    fn 선택identity는_같은문구라도_turn_key로_구분한다() {
        let mut first = row("turn-1", storage::AgentWorkTurnState::Completed, 1);
        let mut second = row("turn-2", storage::AgentWorkTurnState::Completed, 2);
        second.instruction.clone_from(&first.instruction);
        first.source_offset = 10;
        second.source_offset = 20;

        assert!(WorkTurnIdentity::from(&first) != WorkTurnIdentity::from(&second));
    }

    #[test]
    fn 선택메타데이터가_없으면_빈토큰과_branch를_만들지않는다() {
        let mut candidate = row("minimal", storage::AgentWorkTurnState::Completed, 1);
        candidate.model = None;
        candidate.effort = None;
        candidate.branch = None;
        candidate.git_change_count = None;

        let metadata = metadata_parts(&candidate);
        assert_eq!(metadata.primary, vec!["Codex"]);
        assert!(metadata.branch.is_none());
        assert!(metadata.git_change_count.is_none());
        assert!(metadata.primary.iter().all(|part| !part.is_empty()));
    }

    #[test]
    fn 빈_snapshot과_검색결과없음은_각각_빈목록이다() {
        let mut ui = WorkHistoryUi::new();
        assert!(ui.visible_rows(&[]).is_empty());

        ui.query = "not-found".to_owned();
        assert!(
            ui.visible_rows(&[row("one", storage::AgentWorkTurnState::Completed, 1,)])
                .is_empty()
        );
    }
}
