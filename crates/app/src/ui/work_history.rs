//! 현재 워크스페이스의 durable agent work turns를 카드 목록으로 보여주는 순수 UI leaf.
//!
//! 저장소 조회, Git 수집, 세션 이동 같은 권한은 갖지 않는다. App이 넘긴 bounded
//! immutable snapshot을 그리며, 밖으로는 durable identity 기반 의도만 내보낸다.

/// 카드가 실제로 구분·정렬·검색·렌더에 쓰는 상태만 남긴 leaf 전용 상태값.
/// 저장소 쪽 durable 상태값과 값 집합은 같지만, leaf가 그 크레이트를 직접
/// 참조하지 않도록 App이 변환한다(`crates/app/src/app.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkHistoryState {
    Working,
    Waiting,
    Completed,
}

/// 카드 한 장이 그리는 데이터의 **빌린 뷰**. 워크스페이스당 최대 256행이 이력
/// 탭이 활성인 동안 매 프레임 렌더되므로, `String`을 복제하지 않고 App이 들고
/// 있는 durable work-turn 행의 필드를 참조로만 넘긴다.
///
/// durable 행이 갖는 `pane_id`·`cwd`·`occurred_at`은 이 파일 어디에서도 읽지
/// 않아 뺐다 — pane 매칭·git 조회·활성화 판단은 모두 App
/// 쪽(`resolve_work_history_activation` 등)의 책임이라 leaf 뷰에 들어올 이유가
/// 없다.
#[derive(Clone, Copy, Debug)]
pub struct WorkHistoryRow<'a> {
    pub workspace_id: &'a str,
    pub kind: &'a str,
    pub agent_session_id: &'a str,
    pub turn_key: &'a str,
    pub source_offset: u64,
    pub instruction: &'a str,
    pub agent_summary: Option<&'a str>,
    pub messages_json: Option<&'a str>,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub git_change_count: Option<u32>,
    pub state: WorkHistoryState,
    pub updated_at: i64,
}

pub struct WorkHistorySnapshot<'a> {
    pub workspace_name: &'a str,
    pub current_branch: Option<&'a str>,
    pub rows: &'a [WorkHistoryRow<'a>],
    /// App이 `self.work_history_rows`를 실제로 수정할 때마다(교체·부분 갱신·비움)
    /// 올리는 리비전. `rows`의 내용이 바뀔 수 있는 지점(App: `poll_work_history_git`
    /// in-place 갱신, `apply_agent_state_projection_result`의 WorkHistory 스냅샷
    /// 교체, 워크스페이스 이탈 시 clear)마다 반드시 함께 올라간다는 게 계약이다 —
    /// 이 값이 그대로면 `rows`가 그대로라고 믿고 필터·정렬·그룹핑 결과를 재사용한다
    /// (`WorkHistoryUi::cached_grouped_rows`). 이 계약이 깨지면(리비전 갱신을
    /// 빠뜨리면) 화면이 옛 데이터를 계속 보여주는 사고가 난다.
    pub rows_revision: u64,
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

impl From<&WorkHistoryRow<'_>> for WorkTurnIdentity {
    fn from(row: &WorkHistoryRow<'_>) -> Self {
        Self {
            workspace_id: row.workspace_id.to_owned(),
            kind: row.kind.to_owned(),
            agent_session_id: row.agent_session_id.to_owned(),
            turn_key: row.turn_key.to_owned(),
        }
    }
}

impl WorkTurnIdentity {
    pub(crate) fn matches(&self, row: &WorkHistoryRow<'_>) -> bool {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkHistoryAction {
    Refresh,
    Activate(WorkTurnIdentity),
    ShowDiff(WorkTurnIdentity),
    /// 카드 「원문 보기」— App이 identity로 행을 찾아 transcript 경로를 해석하고
    /// off-thread로 읽는다(2026-08-15 Task 10, 스펙 §2-1).
    ShowTranscript(WorkTurnIdentity),
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

/// 카드 하나가 그릴 프레젠테이션. **`identity`를 담지 않는다** — 예전에는 담았지만
/// 그러면 App이 256행 전부에 대해 매 프레임 `WorkTurnIdentity::from(row)`(String
/// 4개 할당)를 불러야 했다(2026-08-19 계측: 프레임당 1,024개). `primary`/`show_diff`
/// 버튼은 카드가 펼쳐졌을 때만 읽히고(`render_card`의 `if expanded` 블록 안), 눌렸을
/// 때 올리는 액션의 identity는 그 자리에서 들고 있는 `row`로 즉석에서 만든다
/// (`WorkTurnIdentity::from(row)`, 「원문 보기」가 이미 하던 방식과 같다) — 클릭은
/// 프레임당 최대 한 번이라 이 할당은 문제가 되지 않는다.
///
/// **행-프레젠테이션 대응은 App이 순서로 보장한다.** `App::render_work_history_tab_body`가
/// `self.work_history_rows`를 한 번만 순회하며 `WorkHistoryRow`와 이 값을 같은 자리에서
/// 함께 만들어, `show()`에 넘기는 `rows`와 `presentations` 두 슬라이스가 항상 같은
/// 길이·같은 순서를 이룬다. 이 leaf는 필터·정렬·그룹핑 과정에서도 원본 인덱스
/// (`WorkHistoryGroup::row_indices`)를 그대로 들고 다녀 그 인덱스로 `presentations`를
/// 직접 찾는다 — 문자열 4개를 비교하는 선형 탐색이 없다(2026-08-19, 스펙 이슈 #2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkHistoryActionPresentation {
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

/// 같은 `agent_session_id`(+`kind`) 턴을 묶은 한 그룹. `rows`는 `visible_rows`가
/// 만든 전역 정렬 순서에서 이 그룹에 속한 항목만 뽑아 원래 상대 순서를 그대로
/// 보존한 것이다 — 그룹 내부 정렬을 다시 계산하지 않는다(아래 `grouped_rows` 문서
/// 참고). `model`/`effort`/`branch`/`git_change_count`는 그룹 내에서 **값이 있는
/// 가장 최신 턴**의 값이고, 아무도 값을 갖지 않으면 `None`으로 비워 둔다 — 없는
/// 값을 지어내지 않는다(app.rs의 `stage_detected_work_history` cwd 캡처 규칙 때문에
/// 대부분 한 턴에만 값이 실린다).
struct WorkHistoryGroup<'a> {
    kind: &'a str,
    agent_session_id: &'a str,
    rows: Vec<WorkHistoryRow<'a>>,
    /// `rows[k]`가 원본 `rows`(이 그룹을 만든 `show()`의 `snapshot.rows`) 슬라이스
    /// 에서 몇 번째였는지 — `rows`와 항상 같은 길이·같은 순서로 나란히 간다.
    /// presentation 조회(App이 같은 순서로 만들어 넘긴 `presentations`)와 캐시
    /// 재조립(`WorkHistoryUi::cached_grouped_rows`)이 이 인덱스로 원본을 O(1)
    /// 역참조한다 — `WorkTurnIdentity::matches` 문자열 비교 선형 탐색을 없앤다.
    row_indices: Vec<usize>,
    model: Option<&'a str>,
    effort: Option<&'a str>,
    branch: Option<&'a str>,
    git_change_count: Option<u32>,
    /// 그룹 내 턴들의 `updated_at` 최댓값 — 헤더의 "최신 시각"이며, branch/변경
    /// 수와 달리 값 유무와 무관하게 그룹의 모든 턴에서 구한다.
    latest_updated_at: i64,
}

/// `rows`에서 값이 있는 항목 중 `updated_at`이 가장 큰 것의 값을 돌려준다. 동률이면
/// `source_offset` 뒤 `turn_key`로 결정적으로 끊는다(파일 전역의 다른 정렬 tie-break와
/// 같은 규칙). 값이 있는 항목이 하나도 없으면 `None`.
fn latest_with_value<'a, T: Copy>(
    rows: &[WorkHistoryRow<'a>],
    extract: impl Fn(&WorkHistoryRow<'a>) -> Option<T>,
) -> Option<T> {
    rows.iter()
        .filter_map(|row| extract(row).map(|value| (*row, value)))
        .max_by(|(a, _), (b, _)| {
            a.updated_at
                .cmp(&b.updated_at)
                .then_with(|| a.source_offset.cmp(&b.source_offset))
                .then_with(|| a.turn_key.cmp(b.turn_key))
        })
        .map(|(_, value)| value)
}

/// `WorkHistoryUi::visible_row_indices`가 쓰는 정렬 비교자. `visible_rows`(캐시 없는
/// 경로)와 캐시 계획을 만드는 인덱스 경로가 이 함수 하나를 공유해 정렬 규칙이 두
/// 곳에서 갈라지지 않는다.
fn compare_rows(
    left: &WorkHistoryRow<'_>,
    right: &WorkHistoryRow<'_>,
    sort_mode: WorkHistorySortMode,
) -> std::cmp::Ordering {
    match sort_mode {
        WorkHistorySortMode::StateFirst => state_rank(left.state)
            .cmp(&state_rank(right.state))
            .then_with(|| right.updated_at.cmp(&left.updated_at))
            .then_with(|| right.source_offset.cmp(&left.source_offset))
            .then_with(|| left.kind.cmp(right.kind))
            .then_with(|| left.agent_session_id.cmp(right.agent_session_id))
            .then_with(|| left.turn_key.cmp(right.turn_key)),
        WorkHistorySortMode::RecentFirst => right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| right.source_offset.cmp(&left.source_offset))
            .then_with(|| left.kind.cmp(right.kind))
            .then_with(|| left.agent_session_id.cmp(right.agent_session_id))
            .then_with(|| left.turn_key.cmp(right.turn_key)),
    }
}

/// 그룹 하나를 원본 `rows`와 인덱스 목록에서 조립한다. `grouped_rows`(캐시 없이
/// 매번)와 `cached_grouped_rows`(캐시된 인덱스 재사용) 둘 다 이 함수 하나로
/// model/effort/branch/git_change_count/latest_updated_at을 계산해, 두 경로의 결과가
/// 갈라질 수 없다.
fn materialize_group<'a>(
    rows: &'a [WorkHistoryRow<'a>],
    indices: &[usize],
) -> WorkHistoryGroup<'a> {
    let group_rows: Vec<WorkHistoryRow<'a>> = indices.iter().map(|&index| rows[index]).collect();
    let kind = group_rows[0].kind;
    let agent_session_id = group_rows[0].agent_session_id;
    let model = latest_with_value(&group_rows, |row| row.model);
    let effort = latest_with_value(&group_rows, |row| row.effort);
    let branch = latest_with_value(&group_rows, |row| row.branch);
    let git_change_count = latest_with_value(&group_rows, |row| row.git_change_count);
    let latest_updated_at = group_rows
        .iter()
        .map(|row| row.updated_at)
        .max()
        .unwrap_or(0);
    WorkHistoryGroup {
        kind,
        agent_session_id,
        row_indices: indices.to_vec(),
        rows: group_rows,
        model,
        effort,
        branch,
        git_change_count,
        latest_updated_at,
    }
}

/// `WorkHistoryUi::cached_grouped_rows`의 캐시 키. 이 값이 이전 프레임과 같으면
/// `rows`가 가리키는 실제 데이터도 그대로라고 App이 보장한다(`rows_revision` 계약,
/// `WorkHistorySnapshot::rows_revision` 문서 참고) — 그러면 필터·정렬·그룹핑(인덱스
/// 계획)을 다시 계산하지 않는다.
#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkHistoryGroupedCacheKey {
    rows_revision: u64,
    query: String,
    filter: WorkHistoryFilter,
    providers: Vec<WorkHistoryProvider>,
    sort_mode: WorkHistorySortMode,
}

/// 목록 가상화 높이 캐시(`WorkHistoryUi::list_heights`)가 유효한 슬롯 구성인지
/// 판단하는 서명. `WorkHistoryGroupedCacheKey`가 커버하는 것(리비전·질의·필터·
/// provider·정렬 모드) 외에도 보조 검색 필터(`apply_aux_filter`, 캐시 범위 밖)와
/// 그룹 접기 상태(슬롯 개수를 바꾼다)까지 더해야 "화면에 그려질 슬롯의 순서·개수"를
/// 완전히 결정한다. 카드 선택(펼침)은 슬롯 개수를 바꾸지 않고 한 슬롯의 높이만
/// 바꾸므로 여기 넣지 않는다 — 가상화 렌더 루프의 자기보정(실측값이 캐시와 다르면
/// 그 자리에서 갱신)이 알아서 따라잡는다.
#[derive(Clone, PartialEq, Eq)]
struct WorkHistoryListShape {
    grouped_key: WorkHistoryGroupedCacheKey,
    aux_filter: String,
    collapsed_generation: u64,
}

/// 목록 가상화의 슬롯 하나 — 그룹 헤더 또는 그 그룹의 카드 한 장.
enum WorkHistorySlotKind {
    Header { collapsed: bool },
    Card { row_in_group: usize },
}

struct WorkHistorySlot {
    group_index: usize,
    kind: WorkHistorySlotKind,
    /// 이 슬롯이 자기 그룹의 마지막 슬롯이면 true. 그룹 사이 여백(`GROUP_GAP`,
    /// 예전엔 `if index > 0 { ui.add_space(6.0) }`로 다음 그룹 헤더 앞에 뒀다)을
    /// 이 슬롯 자신의 렌더에 트레일링 `ui.add_space`로 포함시켜 측정 높이에 자연히
    /// 녹인다 — 그래야 가상화 오프셋 계산이 슬롯 종류(헤더/카드)나 그룹 경계 여부를
    /// 따로 안 가리는 단일 공식(`extra_per_slot = item_spacing.y`)으로 성립한다.
    /// 맨 마지막 그룹 뒤에도 여백이 남지만 스크롤 영역 바닥의 빈 공간일 뿐이라
    /// 무해하다(transcript_viewer.rs `MESSAGE_GAP`이 마지막 메시지 뒤에도 붙는 것과
    /// 같은 관례).
    is_group_end: bool,
}

/// `groups`(이미 필터·정렬·그룹핑·보조 검색까지 적용된 결과)와 접기 상태로부터
/// 슬롯 목록을 만든다. 접힌 그룹은 헤더 슬롯 하나뿐이다 — "접힌 그룹 헤더도 목록의
/// 일부"라 가상화 대상에서 빠지면 안 된다.
fn build_slots(
    groups: &[WorkHistoryGroup<'_>],
    collapsed_groups: &std::collections::HashSet<(String, String)>,
) -> Vec<WorkHistorySlot> {
    let mut slots = Vec::new();
    for (group_index, group) in groups.iter().enumerate() {
        let key = (group.kind.to_owned(), group.agent_session_id.to_owned());
        let collapsed = collapsed_groups.contains(&key);
        if collapsed {
            slots.push(WorkHistorySlot {
                group_index,
                kind: WorkHistorySlotKind::Header { collapsed: true },
                is_group_end: true,
            });
            continue;
        }
        slots.push(WorkHistorySlot {
            group_index,
            kind: WorkHistorySlotKind::Header { collapsed: false },
            is_group_end: false,
        });
        let last = group.rows.len().saturating_sub(1);
        for row_in_group in 0..group.rows.len() {
            slots.push(WorkHistorySlot {
                group_index,
                kind: WorkHistorySlotKind::Card { row_in_group },
                is_group_end: row_in_group == last,
            });
        }
    }
    slots
}

/// 슬롯 하나를 그리고 실측 rect를 함께 돌려준다. `render_group_header`/`render_card`
/// 내부는 바꾸지 않고 감싸기만 한다 — `Frame::NONE`(여백 없음)이 안에서 그린 모든 것
/// (카드 프레임, 헤더의 hairline, 그룹 끝이면 트레일링 간격까지)을 정확히 감싼
/// rect를 돌려준다(transcript_viewer.rs `render_message`가 프레임 rect를 재는 것과
/// 같은 방식).
fn render_slot(
    ui: &mut egui::Ui,
    slot: &WorkHistorySlot,
    groups: &[WorkHistoryGroup<'_>],
    presentations: &[WorkHistoryActionPresentation],
    selected: Option<&WorkTurnIdentity>,
    now: i64,
    catalog: &i18n::Catalog,
) -> (SlotOutcome, egui::Rect) {
    let mut outcome = SlotOutcome::default();
    let response = egui::Frame::NONE.show(ui, |ui| {
        let group = &groups[slot.group_index];
        match slot.kind {
            WorkHistorySlotKind::Header { collapsed } => {
                if render_group_header(ui, group, collapsed, now, catalog) {
                    outcome.toggled_group =
                        Some((group.kind.to_owned(), group.agent_session_id.to_owned()));
                }
            }
            WorkHistorySlotKind::Card { row_in_group } => {
                let row = &group.rows[row_in_group];
                let presentation = group
                    .row_indices
                    .get(row_in_group)
                    .and_then(|&index| presentations.get(index));
                let expanded = selected.is_some_and(|identity| identity.matches(row));
                let card_action = render_card(ui, row, expanded, now, presentation, catalog);
                if card_action.toggle {
                    outcome.toggled_card = Some(WorkTurnIdentity::from(row));
                }
                outcome.action = card_action.action;
            }
        }
        if slot.is_group_end {
            ui.add_space(GROUP_GAP);
        }
    });
    (outcome, response.response.rect)
}

#[derive(Default)]
struct SlotOutcome {
    toggled_group: Option<(String, String)>,
    toggled_card: Option<WorkTurnIdentity>,
    action: Option<WorkHistoryAction>,
}

/// 그룹과 다음 그룹 사이 여백(예전 `ui.add_space(6.0)`와 같은 값). `WorkHistorySlot`
/// 문서 참고 — 그룹 앞이 아니라 그 그룹 마지막 슬롯 뒤에 붙인다.
const GROUP_GAP: f32 = 6.0;

/// 뷰포트 앞뒤로 이만큼 슬롯을 더 배치해 스크롤 중 빈칸이 보이지 않게 한다.
/// transcript_viewer.rs `OVERSCAN_MESSAGES`와 같은 값·같은 이유.
const OVERSCAN_SLOTS: usize = 4;

/// 아직 실측 못 한 슬롯의 잠정 높이 — 이미 측정된 높이들의 평균, 하나도 없으면
/// `fallback`(한 줄 높이). transcript_viewer.rs `provisional_height`와 같다.
fn provisional_slot_height(heights: &[Option<f32>], fallback: f32) -> f32 {
    let (sum, count) = heights
        .iter()
        .flatten()
        .fold((0.0_f32, 0usize), |(sum, count), height| {
            (sum + height, count + 1)
        });
    if count == 0 {
        fallback
    } else {
        sum / count as f32
    }
}

/// 슬롯별 "슬롯 높이"(프레임 높이 + item_spacing)의 누적합. 길이는 `heights.len() + 1`
/// 이고 `offsets[k]`는 슬롯 0..k를 배치했을 때 소비하는 전체 높이 —
/// transcript_viewer.rs `slot_offsets`와 같은 계약.
fn slot_offsets(heights: &[Option<f32>], provisional: f32, extra_per_slot: f32) -> Vec<f32> {
    let mut offsets = Vec::with_capacity(heights.len() + 1);
    offsets.push(0.0);
    let mut acc = 0.0;
    for height in heights {
        acc += height.unwrap_or(provisional) + extra_per_slot;
        offsets.push(acc);
    }
    offsets
}

/// 뷰포트 y범위(overscan 포함)와 겹치는 슬롯 인덱스 구간을 누적합에서 찾는다 —
/// transcript_viewer.rs `visible_range`와 같다.
fn visible_slot_range(
    offsets: &[f32],
    viewport: std::ops::Range<f32>,
    overscan: usize,
) -> std::ops::Range<usize> {
    let total = offsets.len().saturating_sub(1);
    if total == 0 || viewport.end <= viewport.start {
        return 0..0;
    }
    let tight_start = offsets
        .partition_point(|offset| *offset <= viewport.start)
        .saturating_sub(1)
        .min(total - 1);
    let tight_end = offsets
        .partition_point(|offset| *offset < viewport.end)
        .clamp(tight_start + 1, total);
    let start = tight_start.saturating_sub(overscan);
    let end = (tight_end + overscan).min(total);
    start..end
}

/// 그룹 헤더 토글의 접근성 이름. provider만 쓰면 같은 provider의 다른 세션과 겹칠
/// 수 있어 `agent_session_id`를 더해 유일하게 만든다 — 카드가 `row.instruction`을
/// 그대로 쓰는 것과 같은 이유(번역이 필요 없는 raw 식별자).
fn group_accessible_label(kind: &str, agent_session_id: &str) -> String {
    format!("{} · {agent_session_id}", provider_label(kind))
}

pub struct WorkHistoryUi {
    query: String,
    filter: WorkHistoryFilter,
    providers: Vec<WorkHistoryProvider>,
    sort_mode: WorkHistorySortMode,
    selected: Option<WorkTurnIdentity>,
    /// 접힌 그룹의 `(kind, agent_session_id)` 집합. 세션 내 UI 상태로만 유지하고
    /// 설정·DB에는 저장하지 않는다 — `selected`와 같은 성격의 필드다.
    collapsed_groups: std::collections::HashSet<(String, String)>,
    /// `collapsed_groups`가 바뀔 때마다 올라간다. 집합 자체(String 여러 개를 담은
    /// `HashSet`)를 매 프레임 복제해 이전 값과 비교하는 대신, 값 하나 비교로 "접기
    /// 상태가 바뀌었는가"를 싸게 묻기 위한 세대 번호다(`WorkHistoryListShape`가 쓴다).
    collapsed_generation: u64,
    /// 필터·정렬·그룹핑(“행 인덱스 계획”)을 재사용하기 위한 캐시. 키가 이전 프레임과
    /// 같으면(`WorkHistoryGroupedCacheKey`) `grouped_row_indices`를 다시 부르지 않고
    /// 캐시된 인덱스만 `materialize_group`으로 얕게 재조립한다 — `'a` 대여가 프레임을
    /// 못 넘기므로 캐시엔 참조가 아니라 `usize` 인덱스만 담는다(2026-08-19, 스펙
    /// 이슈 #4).
    grouped_cache: Option<(WorkHistoryGroupedCacheKey, Vec<Vec<usize>>)>,
    /// 카드 목록 가상화(`show_viewport`, transcript_viewer.rs와 같은 계약)가 슬롯별로
    /// 실측한 높이. 슬롯 하나 = 그룹 헤더 한 줄 또는 카드 한 장. `list_shape`가
    /// 바뀌면(슬롯 구성이 달라질 수 있으므로) 통째로 비운다.
    list_heights: Vec<Option<f32>>,
    /// 이번 슬롯 구성을 연 뒤 아직 "전부 배치"를 하지 않았다. transcript_viewer.rs
    /// `initial_pass_done`과 같은 이유 — 착지 정확성을 위해 슬롯 구성이 바뀐 첫
    /// 프레임은 가상화 없이 전부 배치해 모든 높이를 실측한다.
    list_bootstrap_done: bool,
    /// 마지막으로 `list_heights`를 채운 슬롯 구성의 서명. 이게 바뀌면(리비전·질의·
    /// 필터·provider·정렬 모드·보조 검색·접기 상태 중 하나라도) 슬롯 순서·개수가
    /// 달라졌을 수 있어 높이 캐시를 버리고 다시 부트스트랩한다.
    list_shape: Option<WorkHistoryListShape>,
}

impl WorkHistoryUi {
    pub fn new() -> Self {
        Self {
            query: String::new(),
            filter: WorkHistoryFilter::All,
            providers: Vec::new(),
            sort_mode: WorkHistorySortMode::StateFirst,
            selected: None,
            collapsed_groups: std::collections::HashSet::new(),
            collapsed_generation: 0,
            grouped_cache: None,
            list_heights: Vec::new(),
            list_bootstrap_done: false,
            list_shape: None,
        }
    }

    /// `filter`는 보조 검색(App이 소유한 `AuxSearchState`)이 넘기는 질의다 — 빈
    /// 문자열이면 필터 없음. 카드 자체의 정렬·상태 필터·provider 칩·내부 검색창
    /// (`self.query`)과는 별개로 AND 결합되며, 걸리지 않은 카드는 그룹에서 빠진다
    /// (Task 3, 스펙 "좌측 목록 필터").
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: WorkHistorySnapshot<'_>,
        presentations: &[WorkHistoryActionPresentation],
        catalog: &i18n::Catalog,
        filter: &str,
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
        let grouped_key = WorkHistoryGroupedCacheKey {
            rows_revision: snapshot.rows_revision,
            query: self.query.trim().to_lowercase(),
            filter: self.filter,
            providers: self.providers.clone(),
            sort_mode: self.sort_mode,
        };
        let groups = apply_aux_filter(
            self.cached_grouped_rows(snapshot.rows, &grouped_key),
            filter,
        );
        let shown: usize = groups.iter().map(|group| group.rows.len()).sum();

        // 목록 가상화(`show_viewport`) 슬롯 구성 — 그룹 헤더 한 줄 또는 카드 한 장이
        // 슬롯 하나다(접힌 그룹 헤더도 슬롯이다). 이번 프레임의 슬롯 구성 서명이
        // 직전 프레임과 같으면 실측 높이 캐시를 그대로 쓰고, 다르면(리비전·질의·
        // 필터·provider·정렬 모드·보조 검색·접기 상태 중 하나라도 바뀌었으면) 버리고
        // 이번 프레임에 전부 다시 배치해 재측정한다(transcript_viewer.rs
        // `initial_pass_done`과 같은 부트스트랩 계약 — 착지 정확성이 우선이라 여기서도
        // 타협하지 않는다).
        let slots = build_slots(&groups, &self.collapsed_groups);
        let shape = WorkHistoryListShape {
            grouped_key,
            aux_filter: filter.to_owned(),
            collapsed_generation: self.collapsed_generation,
        };
        if self.list_shape.as_ref() != Some(&shape) || self.list_heights.len() != slots.len() {
            self.list_heights = vec![None; slots.len()];
            self.list_bootstrap_done = false;
            self.list_shape = Some(shape);
        }

        content.show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height((available_height - f32::from(BODY_MARGIN_Y) * 2.0).max(0.0));
            if self.render_context_row(ui, &snapshot, shown, catalog) {
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
            if groups.is_empty() {
                // 보조 검색(filter)이 걸려 있으면 "이 검색어로는 없다"는 전용 안내가
                // 기존 "history.no_results"(내부 검색창·상태·provider 필터가 만든
                // 빈 결과)보다 정확하다 — 스펙 "좌측 목록 필터".
                let message = if filter.is_empty() {
                    catalog.t("history.no_results", &[])
                } else {
                    catalog.t("search.no_match", &[])
                };
                render_centered_message(ui, message);
                return;
            }

            let now = unix_now();
            let selected = self.selected.as_ref();
            let list_heights = &mut self.list_heights;
            let list_bootstrap_done = &mut self.list_bootstrap_done;
            let mut toggled_group = None;
            let mut toggled_card = None;
            let mut list_action = None;

            egui::ScrollArea::vertical()
                .id_salt("work-history-cards")
                .auto_shrink([false, false])
                .show_viewport(ui, |ui, viewport| {
                    ui.spacing_mut().item_spacing.y = 9.0;
                    if !*list_bootstrap_done {
                        // 부트스트랩: 슬롯 구성이 바뀐 첫 프레임은 가상화 없이 전부
                        // 배치해 모든 슬롯 높이를 실측한다.
                        for (index, slot) in slots.iter().enumerate() {
                            let (outcome, rect) = render_slot(
                                ui,
                                slot,
                                &groups,
                                presentations,
                                selected,
                                now,
                                catalog,
                            );
                            list_heights[index] = Some(rect.height());
                            if let Some(key) = outcome.toggled_group {
                                toggled_group = Some(key);
                            }
                            if let Some(identity) = outcome.toggled_card {
                                toggled_card = Some(identity);
                            }
                            if list_action.is_none() {
                                list_action = outcome.action;
                            }
                        }
                        *list_bootstrap_done = true;
                        return;
                    }

                    // 두 번째 프레임부터: 캐시된 높이의 누적합으로 뷰포트에 걸치는
                    // 구간만 배치한다(+ overscan). transcript_viewer.rs와 같은 패턴을
                    // 그대로 따른다 — 카드 높이가 균일하지 않고(펼침 여부에 따라
                    // 크게 달라진다) 헤더·카드 높이도 서로 달라, `show_rows`의 "모든
                    // 행이 같은 높이" 계약을 쓸 수 없다(transcript_viewer.rs가 예전에
                    // `show_rows`를 쓰다 겪은 것과 같은 문제).
                    let row_fallback = ui.text_style_height(&egui::TextStyle::Body);
                    let provisional = provisional_slot_height(list_heights, row_fallback);
                    let extra_per_slot = ui.spacing().item_spacing.y;
                    let offsets = slot_offsets(list_heights, provisional, extra_per_slot);
                    let total_height = offsets.last().copied().unwrap_or(0.0);
                    ui.set_height(total_height);

                    let range = visible_slot_range(
                        &offsets,
                        viewport.min.y..viewport.max.y,
                        OVERSCAN_SLOTS,
                    );
                    if range.is_empty() {
                        return;
                    }

                    let y_min = ui.max_rect().top() + offsets[range.start];
                    let y_max = ui.max_rect().top() + offsets[range.end];
                    let rect = egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), y_min..=y_max);

                    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |viewport_ui| {
                        // 배치 안 하는 앞뒤 슬롯도 "widget이 있었다"고 셈해야 스크롤
                        // 도중 같은 슬롯의 auto id가 프레임마다 안 바뀐다
                        // (transcript_viewer.rs와 같은 관례).
                        viewport_ui.skip_ahead_auto_ids(range.start);
                        for index in range.clone() {
                            let (outcome, rect) = render_slot(
                                viewport_ui,
                                &slots[index],
                                &groups,
                                presentations,
                                selected,
                                now,
                                catalog,
                            );
                            let measured = rect.height();
                            if list_heights[index] != Some(measured) {
                                list_heights[index] = Some(measured);
                                // 값이 바뀌었으니(예: 카드를 펼치거나 접었다) 다음
                                // 프레임에 새 누적합을 반영한다.
                                viewport_ui.ctx().request_repaint();
                            }
                            if let Some(key) = outcome.toggled_group {
                                toggled_group = Some(key);
                            }
                            if let Some(identity) = outcome.toggled_card {
                                toggled_card = Some(identity);
                            }
                            if list_action.is_none() {
                                list_action = outcome.action;
                            }
                        }
                    });
                });

            if let Some(key) = toggled_group {
                self.toggle_group_collapsed(key);
            }
            if let Some(identity) = toggled_card {
                self.toggle_selected(identity);
            }
            if action.is_none() {
                action = list_action;
            }
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

    /// 상태 필터·provider 칩·내부 검색창을 모두 통과하는가. `visible_row_indices`와
    /// (그래서 `visible_rows`도) 이 판정 하나를 공유한다 — 판정 로직이 두 곳에서
    /// 갈라지면 캐시가 화면과 다른 결과를 보여주는 사고가 난다.
    fn passes_filters(&self, row: &WorkHistoryRow<'_>, query: &str) -> bool {
        self.filter.matches(row.state)
            // provider 칩을 전부 해제한 상태는 "빈 목록"이 아니라 "전체 provider
            // 표시"로 취급한다. 세션 시작 시 아무 칩도 선택돼 있지 않은 기본값이
            // 기존 동작(필터 없음)과 동일해야 하고, 사용자가 마지막 칩을 끄는
            // 순간 카드가 통째로 사라지면 "무필터"가 아니라 "빈 화면"이라는
            // 오해를 준다. 하나라도 선택되면 그때부터 선택된 provider와 매치하는
            // row만 남기며, 상태 필터와는 AND로 결합된다.
            && (self.providers.is_empty()
                || self.providers.iter().any(|provider| provider.matches(row.kind)))
            && (query.is_empty() || row_matches_query(row, query))
    }

    /// `visible_row_indices`의 값-반환 버전. 실제 렌더 경로(`show`)는 캐시가 필요해
    /// `cached_grouped_rows`(→ `grouped_row_indices` → `visible_row_indices`)를 직접
    /// 쓰므로 이 메서드는 프로덕션에서 호출되지 않는다 — 인덱스를 신경 쓰지 않고
    /// 필터+정렬 결과만 검증하는 테스트 전용 얇은 래퍼로 남겨 뒀다.
    #[cfg(test)]
    fn visible_rows<'a>(&self, rows: &'a [WorkHistoryRow<'a>]) -> Vec<&'a WorkHistoryRow<'a>> {
        self.visible_row_indices(rows)
            .into_iter()
            .map(|index| &rows[index])
            .collect()
    }

    /// `visible_rows`가 실제로 쓰는 인덱스 버전 — 원본 `rows` 슬라이스에서의 위치를
    /// 그대로 들고 다닌다. `grouped_row_indices`(캐시 계획, `cached_grouped_rows`가
    /// 재사용)와 `visible_rows`가 이 함수 하나를 공유해서 필터·정렬 로직이 두 곳에서
    /// 갈라지는 사고를 막는다.
    fn visible_row_indices(&self, rows: &[WorkHistoryRow<'_>]) -> Vec<usize> {
        let query = self.query.trim().to_lowercase();
        let mut visible: Vec<usize> = (0..rows.len())
            .filter(|&index| self.passes_filters(&rows[index], &query))
            .collect();
        visible.sort_by(|&left, &right| compare_rows(&rows[left], &rows[right], self.sort_mode));
        visible
    }

    /// `visible_rows`가 만든(필터+정렬 적용된) 전역 순서를 `agent_session_id`
    /// (+`kind`)로 묶는다.
    ///
    /// 그룹 순서·그룹 내부 턴 순서 둘 다 새로 계산하지 않고 "첫 등장 순서"만
    /// 본다 — `visible_rows`의 전역 정렬 기준(예: `StateFirst`면 state_rank →
    /// updated_at desc → …)은 행 단위로 완전한 전순서라, 어떤 그룹이 플랫
    /// 목록에 처음 나타나는 위치는 반드시 그 그룹에서 "대표값이 가장 앞서는
    /// 행"의 위치와 같다. 그러므로 첫 등장 순서로 그룹을 나열하면 그게 곧
    /// 그룹 대표값(StateFirst면 그룹 내 최상위 state_rank, RecentFirst면 그룹
    /// 내 최신 updated_at) 기준 순서이고, 그룹에 속한 행들을 만나는 순서 그대로
    /// 모으면 그룹 내부 순서도 전역 정렬 규칙을 그대로 물려받는다. 별도 재정렬이
    /// 필요 없다.
    ///
    /// `grouped_row_indices`(→ `materialize_group`)의 캐시 없는 버전. 실제 렌더
    /// 경로는 `cached_grouped_rows`를 쓰므로(리비전이 그대로면 재계산을 건너뛴다,
    /// 스펙 이슈 #4) 이 메서드 자체는 프로덕션에서 호출되지 않는다 — 그룹핑 로직
    /// 자체(캐시와 무관하게 옳은가)를 검증하는 테스트 전용 얇은 래퍼다.
    #[cfg(test)]
    fn grouped_rows<'a>(&self, rows: &'a [WorkHistoryRow<'a>]) -> Vec<WorkHistoryGroup<'a>> {
        self.grouped_row_indices(rows)
            .iter()
            .map(|indices| materialize_group(rows, indices))
            .collect()
    }

    /// `grouped_rows`가 실제로 쓰는 인덱스 버전 — usize만 담겨 있어 `'a` 대여 없이
    /// `self`에 프레임을 넘겨 캐시할 수 있다(`cached_grouped_rows`).
    fn grouped_row_indices(&self, rows: &[WorkHistoryRow<'_>]) -> Vec<Vec<usize>> {
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for index in self.visible_row_indices(rows) {
            let row = &rows[index];
            match groups.iter_mut().find(|group| {
                let first = &rows[group[0]];
                first.kind == row.kind && first.agent_session_id == row.agent_session_id
            }) {
                Some(group) => group.push(index),
                None => groups.push(vec![index]),
            }
        }
        groups
    }

    /// `grouped_rows(rows)`와 같은 결과를 리비전·질의·필터·provider·정렬 모드가
    /// `key`로 넘어온 값과 이전 프레임에서 그대로면 다시 계산하지 않고 돌려준다
    /// (스펙 이슈 #4). 캐시가 담는 건 usize 인덱스뿐이라 `rows`(매 프레임 새로 빌린
    /// 뷰)의 수명과 무관하게 `self`에 보관할 수 있다 — `key`가 다르면(즉 `rows`가
    /// 가리키는 실제 데이터나 필터·정렬 조건이 바뀌었으면) `grouped_row_indices`를
    /// 다시 불러 새 계획으로 덮어쓴다.
    fn cached_grouped_rows<'a>(
        &mut self,
        rows: &'a [WorkHistoryRow<'a>],
        key: &WorkHistoryGroupedCacheKey,
    ) -> Vec<WorkHistoryGroup<'a>> {
        let stale = match &self.grouped_cache {
            Some((cached_key, _)) => cached_key != key,
            None => true,
        };
        if stale {
            let indices = self.grouped_row_indices(rows);
            self.grouped_cache = Some((key.clone(), indices));
        }
        let (_, indices) = self
            .grouped_cache
            .as_ref()
            .expect("stale 분기가 방금 Some으로 채웠다");
        indices
            .iter()
            .map(|group_indices| materialize_group(rows, group_indices))
            .collect()
    }

    fn toggle_selected(&mut self, identity: WorkTurnIdentity) {
        if self.selected.as_ref() == Some(&identity) {
            self.selected = None;
        } else {
            self.selected = Some(identity);
        }
    }

    /// 그룹 헤더 클릭 — 접혀 있으면 펴고, 펴져 있으면 접는다. `selected`(카드 펼침)와
    /// 별개 상태라 서로 간섭하지 않는다.
    fn toggle_group_collapsed(&mut self, key: (String, String)) {
        if !self.collapsed_groups.remove(&key) {
            self.collapsed_groups.insert(key);
        }
        // 슬롯 구성(카드가 몇 장 보이는가)이 바뀔 수 있으니 목록 가상화 높이 캐시가
        // 이번 프레임에 다시 부트스트랩하도록 세대를 올린다(`WorkHistoryListShape`).
        self.collapsed_generation = self.collapsed_generation.wrapping_add(1);
    }

    fn reconcile_selection(&mut self, rows: &[WorkHistoryRow<'_>]) {
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
    fn matches(self, state: WorkHistoryState) -> bool {
        match self {
            Self::All => true,
            Self::Working => state == WorkHistoryState::Working,
            Self::Waiting => state == WorkHistoryState::Waiting,
            Self::Completed => state == WorkHistoryState::Completed,
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

fn state_rank(state: WorkHistoryState) -> u8 {
    match state {
        WorkHistoryState::Working => 0,
        WorkHistoryState::Waiting => 1,
        WorkHistoryState::Completed => 2,
    }
}

fn row_matches_query(row: &WorkHistoryRow<'_>, query: &str) -> bool {
    [
        Some(row.instruction),
        row.agent_summary,
        Some(row.kind),
        row.model,
        row.effort,
        row.branch,
    ]
    .into_iter()
    .flatten()
    .any(|value| value.to_lowercase().contains(query))
}

/// 보조 검색(`WorkHistoryUi::show`의 `filter` 인자, 스펙 "좌측 목록 필터") 카드
/// 판정 — `instruction`·`agent_summary`·`messages_json`을 편 메시지 텍스트 중
/// 하나라도 걸리면 남긴다. `messages_json`은 파싱 비용이 있어 앞의 두 필드에서
/// 이미 걸리면 시도하지 않는다.
fn row_matches_aux_filter(row: &WorkHistoryRow<'_>, filter: &str) -> bool {
    if crate::ui::aux_search::contains_match(row.instruction, filter) {
        return true;
    }
    if row
        .agent_summary
        .is_some_and(|summary| crate::ui::aux_search::contains_match(summary, filter))
    {
        return true;
    }
    row.messages_json.is_some_and(|json| {
        parse_turn_messages(json, row.instruction)
            .iter()
            .any(|message| crate::ui::aux_search::contains_match(&message.text, filter))
    })
}

/// 그룹 목록에 보조 검색 필터를 적용한다. `filter`가 비어 있으면 "검색 안 함"과
/// 같아 **즉시 원본을 그대로 돌려준다** — `row_matches_aux_filter`(→
/// `messages_json` 파싱)를 단 한 번도 호출하지 않으므로, 필터가 꺼져 있는 평상시
/// 프레임 비용은 이 Task 이전과 동일하다. 필터가 걸리면 그룹 안에서 걸리지 않는
/// 카드를 빼고, 카드가 하나도 안 남은 그룹은 통째로 지운다(빈 그룹 헤더만 남는
/// 것을 막는다).
fn apply_aux_filter<'a>(
    mut groups: Vec<WorkHistoryGroup<'a>>,
    filter: &str,
) -> Vec<WorkHistoryGroup<'a>> {
    if filter.is_empty() {
        return groups;
    }
    for group in &mut groups {
        group.rows.retain(|row| row_matches_aux_filter(row, filter));
    }
    groups.retain(|group| !group.rows.is_empty());
    groups
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

/// 그룹 헤더 한 줄 — provider·model·effort·branch·작업 트리 변경 수·그룹 내 최신
/// 시각·턴 수를 담는다. 카드 토글과 같은 트릭(`Sense::click()` 스코프 전체가
/// 논리 버튼, 내부엔 실제 `egui::Button`을 두지 않음)으로 접기/펴기를 구현한다 —
/// 헤더 안에 버튼을 중첩하면 카드에서 이미 고친(0e934c0) AccessKit 문제가 그대로
/// 재발한다. 좁은 폭에서는 `horizontal_wrapped`로 다음 줄로 흘려보내 겹침을
/// 막는다(카드의 `render_metadata`와 같은 패턴).
fn render_group_header(
    ui: &mut egui::Ui,
    group: &WorkHistoryGroup<'_>,
    collapsed: bool,
    now: i64,
    catalog: &i18n::Catalog,
) -> bool {
    let toggle = ui.scope_builder(
        egui::UiBuilder::new()
            .id_salt(("work-history-group", group.kind, group.agent_session_id))
            .sense(egui::Sense::click()),
        |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(if collapsed { "▸" } else { "▾" });
                ui.label(egui::RichText::new(provider_label(group.kind)).strong());
                if let Some(model) = group.model {
                    ui.label(egui::RichText::new("·").small().weak());
                    ui.label(egui::RichText::new(model).small().weak().monospace());
                }
                if let Some(effort) = group.effort {
                    ui.label(egui::RichText::new("·").small().weak());
                    ui.label(egui::RichText::new(effort).small().weak().monospace());
                }
                if let Some(branch) = group.branch {
                    metadata_chip(ui, branch);
                }
                if let Some(count) = group.git_change_count {
                    let count = count.to_string();
                    metadata_chip(ui, &catalog.t("history.card.changes", &[("count", &count)]));
                }
                ui.label(egui::RichText::new("·").small().weak());
                ui.label(
                    egui::RichText::new(relative_age_text(catalog, group.latest_updated_at, now))
                        .small()
                        .weak(),
                );
                ui.label(egui::RichText::new("·").small().weak());
                let turns = group.rows.len().to_string();
                ui.label(
                    egui::RichText::new(catalog.t("history.group.turns", &[("count", &turns)]))
                        .small()
                        .weak(),
                );
            });
        },
    );
    let response = toggle
        .response
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let label = group_accessible_label(group.kind, group.agent_session_id);
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Button,
            ui.is_enabled(),
            !collapsed,
            &label,
        )
    });
    // 헤더가 카드보다 상위임을 색 대신 굵은 글씨 + hairline으로 표시한다(토큰에
    // 새 색을 추가하지 않기 위함). hairline은 click 스코프 밖(형제)이라 클릭
    // 판정에 관여하지 않는다.
    ui.add_space(3.0);
    crate::ui::hairline(ui);
    response.clicked()
}

fn render_card(
    ui: &mut egui::Ui,
    row: &WorkHistoryRow<'_>,
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
                        row.workspace_id,
                        row.kind,
                        row.agent_session_id,
                        row.turn_key,
                    ))
                    .sense(egui::Sense::click()),
                |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        provider_badge(ui, row.kind);
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
                                            egui::RichText::new(collapsed_summary_line(
                                                row.instruction,
                                            ))
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
                                    .map(collapsed_summary_line)
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
                    row.instruction,
                );
                copy_button(
                    ui,
                    catalog,
                    copy_feedback_id(row, "instruction"),
                    "history.action.copy_instruction",
                    row.instruction,
                );
                ui.add_space(8.0);
                let messages = row
                    .messages_json
                    .map(|json| parse_turn_messages(json, row.instruction))
                    .unwrap_or_default();
                if messages.is_empty() {
                    // messages_json이 없거나(구버전 행) 신뢰할 수 없으면 기존 경로 —
                    // agent_summary 한 덩이.
                    let summary = row
                        .agent_summary
                        .map(str::to_owned)
                        .unwrap_or_else(|| catalog.t("history.card.no_summary", &[]));
                    expanded_text(ui, &catalog.t("history.card.latest_work", &[]), &summary);
                } else {
                    for message in &messages {
                        let label = match message.role {
                            crate::agent_transcript::TurnRole::User => {
                                catalog.t("history.role.user", &[])
                            }
                            crate::agent_transcript::TurnRole::Assistant => {
                                catalog.t("history.role.agent", &[])
                            }
                        };
                        expanded_text(ui, &label, &message.text);
                        ui.add_space(4.0);
                    }
                }
                if let Some(summary_text) = row.agent_summary {
                    copy_button(
                        ui,
                        catalog,
                        copy_feedback_id(row, "summary"),
                        "history.action.copy_summary",
                        summary_text,
                    );
                }
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
                        let primary_button =
                            ui.add_enabled(enabled, egui::Button::new(catalog.t(label_key, &[])));
                        // PR-resume-without-pane(2026-08-19): pane 없이도 이력만으로
                        // 정확히 이어갈 수 있으면 App이 Resume으로 올린다 — 그래서
                        // 이 카드가 NewRun에 도달했다는 것 자체가 "정확히 이어갈 근거가
                        // 없다"는 뜻이 됐다. 왜 「이어서 하기」 대신 「새로 실행」인지
                        // 말없이 남기지 않고 짧게 알려준다.
                        let primary_button =
                            if presentation.primary == WorkHistoryPrimaryAction::NewRun {
                                primary_button
                                    .on_hover_text(catalog.t("history.action.new_run_hint", &[]))
                            } else {
                                primary_button
                            };
                        if primary_button.clicked() {
                            // `presentation`은 identity를 담지 않는다(카드당 String 4개
                            // 할당을 없애려고 App이 더 이상 만들지 않는다) — 지금 그리고
                            // 있는 `row`에서 즉석으로 만든다. 이 카드가 가리키는 행과
                            // `presentation`은 App이 같은 인덱스로 대응시켜 넘긴 값이라
                            // 항상 같은 턴을 가리킨다(`WorkHistoryActionPresentation` 문서
                            // 참고). 클릭은 프레임당 최대 한 번이라 이 할당은 무해하다 —
                            // 「원문 보기」가 이미 같은 방식을 쓴다(아래).
                            action = Some(WorkHistoryAction::Activate(WorkTurnIdentity::from(row)));
                        }
                        if presentation.show_diff
                            && ui
                                .button(catalog.t("history.action.show_diff", &[]))
                                .on_hover_text(catalog.t("history.action.show_diff_hint", &[]))
                                .clicked()
                        {
                            action = Some(WorkHistoryAction::ShowDiff(WorkTurnIdentity::from(row)));
                        }
                    }
                    // presentation과 무관하게 항상 켜져 있다 — 원문은 런처 가용성이나
                    // 활성 워크스페이스와 상관없이 세션이 끝난 뒤에도 열 수 있어야 한다.
                    if ui
                        .button(catalog.t("history.action.show_transcript", &[]))
                        .clicked()
                    {
                        action = Some(WorkHistoryAction::ShowTranscript(WorkTurnIdentity::from(
                            row,
                        )));
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
            row.instruction,
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

/// 접힌 카드에 쓸 한 줄 — 요약이 여러 줄이어도 첫 줄만 보여준다(2026-08-15).
/// 목록의 스캔성이 우선이라, 접힘 상태에서 카드 높이가 요약 줄 수마다
/// 달라지면 목록이 들쭉날쭉해진다. 한 줄 안에서 폭이 모자랄 때 쓰는 기존
/// `.truncate()`는 이 함수와 별개로 그대로 남는다.
fn collapsed_summary_line(summary: &str) -> &str {
    summary.lines().next().unwrap_or("")
}

/// 카드가 그릴 턴 메시지 하나. `role`은 `agent_transcript`의 것을 재사용해 라벨
/// 매핑이 한 곳(`TurnRole`)에서만 정의되게 한다.
pub struct CardMessage {
    pub role: crate::agent_transcript::TurnRole,
    pub text: String,
}

/// `messages_json`을 카드용으로 푼다. 어떤 이유로든 신뢰할 수 없으면 **빈 벡터**를
/// 돌려주고, 호출부는 기존 `agent_summary` 경로로 물러난다(fail-soft) — 이력 카드
/// 하나가 이상해도 패널 전체가 죽지 않는다.
///
/// - 파싱 실패, 배열 길이가 `TURN_MESSAGES_MAX` 초과, `r`이 `"u"`/`"a"`가 아닌 항목이
///   하나라도 있으면 통째로 거부한다(저장 측이 5개·두 역할을 보장하므로, 그걸 벗어나면
///   신뢰할 수 없는 입력이다 — 잘라 쓰지 않는다).
/// - `instruction`과 정규화 비교(트림 + 연속 공백 접기 + 소문자)해 같은 항목은 지운다
///   (제목이 본문에 두 번 나오는 것 방지, orca의 `turnTextMatchesSessionTitle` 차용).
/// - 인접한 같은 역할·같은 정규화 텍스트는 하나로 줄인다(orca의
///   `dedupeAdjacentConversationTurns` 차용).
fn parse_turn_messages(json: &str, instruction: &str) -> Vec<CardMessage> {
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(json)
    else {
        return Vec::new();
    };
    if items.len() > crate::agent_transcript::TURN_MESSAGES_MAX {
        return Vec::new();
    }

    let mut parsed = Vec::with_capacity(items.len());
    for item in &items {
        let role = match item.get("r").and_then(serde_json::Value::as_str) {
            Some("u") => crate::agent_transcript::TurnRole::User,
            Some("a") => crate::agent_transcript::TurnRole::Assistant,
            _ => return Vec::new(),
        };
        let Some(text) = item.get("t").and_then(serde_json::Value::as_str) else {
            return Vec::new();
        };
        parsed.push(CardMessage {
            role,
            text: text.to_owned(),
        });
    }

    let normalized_instruction = normalize_for_dedupe(instruction);
    parsed.retain(|message| normalize_for_dedupe(&message.text) != normalized_instruction);

    let mut deduped: Vec<CardMessage> = Vec::with_capacity(parsed.len());
    for message in parsed {
        let is_adjacent_duplicate = deduped.last().is_some_and(|previous: &CardMessage| {
            previous.role == message.role
                && normalize_for_dedupe(&previous.text) == normalize_for_dedupe(&message.text)
        });
        if !is_adjacent_duplicate {
            deduped.push(message);
        }
    }
    deduped
}

/// 트림 + 연속 공백 접기 + 소문자 — orca가 제목·본문 중복 판정에 쓰는 비교 규칙과
/// 같다. 원문을 바꾸지 않고 비교용으로만 쓴다.
fn normalize_for_dedupe(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// storage가 `agent_work_turn.instruction`/`agent_summary`에 적용하는 저장 상한
/// (`AGENT_WORK_TURN_INSTRUCTION_BYTES_MAX`/`AGENT_WORK_TURN_SUMMARY_BYTES_MAX`, 각
/// 32KB, storage 크레이트의 db 모듈)과 같은 값이다. 두 상수는 그 크레이트에서 `pub`이
/// 아니라 여기서 재사용할 수 없어 값만 복제해 로컬 상한으로 둔다. 저장 시점에 이미
/// 이 크기로 잘리므로 정상 경로에서는 항상 이 안쪽이지만, 클립보드로 나가는 텍스트도
/// 방어적으로 다시 한 번 상한을 건다.
const WORK_HISTORY_CLIPBOARD_BYTES_MAX: usize = 32 * 1024;

/// 복사 직후 버튼 라벨을 "복사됨"으로 잠깐 바꿔 보여주는 시간.
const COPY_FEEDBACK_SECONDS: f64 = 1.5;

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

fn bounded_clipboard_text(value: &str) -> String {
    let mut bounded = value.to_owned();
    truncate_utf8(&mut bounded, WORK_HISTORY_CLIPBOARD_BYTES_MAX);
    bounded
}

/// 카드별·버튼별로 고유한 id. egui 위젯 id가 아니라 "마지막으로 복사한 시각"을
/// `ctx().data_mut`에 넣어두는 열쇠로만 쓴다 — `WorkHistoryUi`에 새 필드를 추가하지
/// 않고 복사 피드백을 주기 위한 선택.
fn copy_feedback_id(row: &WorkHistoryRow<'_>, suffix: &str) -> egui::Id {
    egui::Id::new((
        "work-history-card-copy",
        row.workspace_id,
        row.kind,
        row.agent_session_id,
        row.turn_key,
        suffix,
    ))
}

/// 카드 토글의 접근성 자손이 아니라, `if expanded` 블록 안에서 토글과 형제로만
/// 호출해야 한다 — 토글 안에 버튼을 중첩하면 0e934c0에서 고친 AccessKit 문제가
/// 재발한다.
fn copy_button(
    ui: &mut egui::Ui,
    catalog: &i18n::Catalog,
    id: egui::Id,
    label_key: &str,
    text: &str,
) {
    let now = ui.input(|input| input.time);
    let copied_at = ui.ctx().data(|data| data.get_temp::<f64>(id));
    let feedback_remaining = copied_at
        .map(|at| COPY_FEEDBACK_SECONDS - (now - at))
        .filter(|remaining| *remaining > 0.0);

    let label = if feedback_remaining.is_some() {
        catalog.t("history.action.copied", &[])
    } else {
        catalog.t(label_key, &[])
    };
    if ui.small_button(label).clicked() {
        ui.ctx().copy_text(bounded_clipboard_text(text));
        ui.ctx().data_mut(|data| data.insert_temp(id, now));
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs_f64(COPY_FEEDBACK_SECONDS));
    } else if let Some(remaining) = feedback_remaining {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs_f64(remaining));
    }
}

fn metadata_parts<'a>(row: &WorkHistoryRow<'a>) -> MetadataParts<'a> {
    let mut primary = vec![provider_label(row.kind)];
    if let Some(model) = row.model {
        primary.push(model);
    }
    if let Some(effort) = row.effort {
        primary.push(effort);
    }
    MetadataParts {
        primary,
        branch: row.branch,
        git_change_count: row.git_change_count,
    }
}

fn render_metadata(ui: &mut egui::Ui, row: &WorkHistoryRow<'_>, catalog: &i18n::Catalog) {
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
    // 배지 텍스트·색 판정이 각각 `kind.trim().to_ascii_lowercase()`를 새로 할당하던
    // 것을 하나로 합쳤다(카드당 힙 alloc 2회 → 1회, 2026-08-19).
    let normalized = kind.trim().to_ascii_lowercase();
    let badge = match normalized.as_str() {
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
    let color = match normalized.as_str() {
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

fn status_dot(ui: &mut egui::Ui, state: WorkHistoryState) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 14.0), egui::Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), 4.0, state_color(state, ui.visuals()));
}

fn state_color(state: WorkHistoryState, visuals: &egui::Visuals) -> egui::Color32 {
    let tokens = crate::ui::designall::tokens(visuals);
    match state {
        WorkHistoryState::Working => tokens.success,
        WorkHistoryState::Waiting => tokens.warning,
        WorkHistoryState::Completed => tokens.muted_text,
    }
}

fn state_label(state: WorkHistoryState, catalog: &i18n::Catalog) -> String {
    let key = match state {
        WorkHistoryState::Working => "history.state.working",
        WorkHistoryState::Waiting => "history.state.waiting",
        WorkHistoryState::Completed => "history.state.completed",
    };
    catalog.t(key, &[])
}

fn catalog_key_for_summary_fallback(state: WorkHistoryState) -> &'static str {
    match state {
        WorkHistoryState::Working => "history.card.no_summary.working",
        WorkHistoryState::Waiting => "history.card.no_summary.waiting",
        WorkHistoryState::Completed => "history.card.no_summary.completed",
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
            messages_json: None,
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

    /// production 함수는 leaf 뷰(`WorkHistoryRow`)만 받으므로, 이 파일의 테스트가
    /// 계속 `storage::AgentWorkTurnRow` 픽스처로 쓰기 위한 변환 헬퍼.
    /// `WorkHistoryRow::from`은 App(`crates/app/src/app.rs`)이 정의한다 — 같은
    /// 크레이트라 여기서도 그대로 쓸 수 있다.
    fn views(rows: &[storage::AgentWorkTurnRow]) -> Vec<WorkHistoryRow<'_>> {
        rows.iter().map(WorkHistoryRow::from).collect()
    }

    #[derive(Default)]
    struct CardInteractionCapture {
        toggles: usize,
        actions: Vec<WorkHistoryAction>,
        copied_text: Vec<String>,
    }

    fn card_harness<'a>(
        catalog: &'a i18n::Catalog,
        candidate: &'a storage::AgentWorkTurnRow,
        presentation: &'a WorkHistoryActionPresentation,
    ) -> egui_kittest::Harness<'a, CardInteractionCapture> {
        let view = WorkHistoryRow::from(candidate);
        egui_kittest::Harness::new_ui_state(
            move |ui, capture: &mut CardInteractionCapture| {
                let result = render_card(ui, &view, true, 10, Some(presentation), catalog);
                if result.toggle {
                    capture.toggles += 1;
                }
                if let Some(action) = result.action {
                    capture.actions.push(action);
                }
                ui.ctx().output(|output| {
                    for command in &output.commands {
                        if let egui::OutputCommand::CopyText(text) = command {
                            capture.copied_text.push(text.clone());
                        }
                    }
                });
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
    fn kittest_원문_보기_버튼은_intent를_올린다() {
        // 카드 펼침 토글이 클릭을 삼키지 않는다 — 「변경 보기」와 같은 위치(toggle
        // 스코프 밖)에 둔 버튼이라 같은 방식으로 검증한다.
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let candidate = row(
            "transcript-hit-test",
            storage::AgentWorkTurnState::Completed,
            10,
        );
        let presentation = WorkHistoryActionPresentation {
            primary: WorkHistoryPrimaryAction::NewRun,
            show_diff: true,
        };
        let mut harness = card_harness(&catalog, &candidate, &presentation);

        harness
            .get_by_role_and_label(
                egui::accesskit::Role::Button,
                &catalog.t("history.action.show_transcript", &[]),
            )
            .click();
        harness.run();

        assert!(
            harness
                .state()
                .actions
                .iter()
                .any(|action| matches!(action, WorkHistoryAction::ShowTranscript(_))),
            "actions: {:?}",
            harness.state().actions
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
    fn kittest_copy_instruction_button_is_not_intercepted_by_card_toggle() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let candidate = row(
            "copy-instruction-hit-test",
            storage::AgentWorkTurnState::Completed,
            10,
        );
        let presentation = WorkHistoryActionPresentation {
            primary: WorkHistoryPrimaryAction::NewRun,
            show_diff: true,
        };
        let mut harness = card_harness(&catalog, &candidate, &presentation);
        let label = catalog.t("history.action.copy_instruction", &[]);

        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, &label)
            .click();
        harness.run();

        assert_eq!(harness.state().toggles, 0);
        assert!(harness.state().actions.is_empty());
        assert_eq!(
            harness.state().copied_text,
            vec![candidate.instruction.clone()]
        );
    }

    #[test]
    fn kittest_copy_summary_button_is_not_intercepted_by_card_toggle() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let candidate = row(
            "copy-summary-hit-test",
            storage::AgentWorkTurnState::Completed,
            10,
        );
        let presentation = WorkHistoryActionPresentation {
            primary: WorkHistoryPrimaryAction::NewRun,
            show_diff: true,
        };
        let mut harness = card_harness(&catalog, &candidate, &presentation);
        let label = catalog.t("history.action.copy_summary", &[]);

        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, &label)
            .click();
        harness.run();

        assert_eq!(harness.state().toggles, 0);
        assert!(harness.state().actions.is_empty());
        assert_eq!(
            harness.state().copied_text,
            vec![candidate.agent_summary.clone().unwrap()]
        );
    }

    #[test]
    fn kittest_card_toggle_does_not_contain_copy_buttons() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let candidate = row(
            "copy-accessibility-tree",
            storage::AgentWorkTurnState::Completed,
            10,
        );
        let presentation = WorkHistoryActionPresentation {
            primary: WorkHistoryPrimaryAction::NewRun,
            show_diff: true,
        };
        let harness = card_harness(&catalog, &candidate, &presentation);
        let card_toggle =
            harness.get_by_role_and_label(egui::accesskit::Role::Button, &candidate.instruction);

        for label in [
            catalog.t("history.action.copy_instruction", &[]),
            catalog.t("history.action.copy_summary", &[]),
        ] {
            assert!(
                card_toggle
                    .query_by_role_and_label(egui::accesskit::Role::Button, &label)
                    .is_none(),
                "card toggle must not contain copy button {label}"
            );
        }
    }

    #[test]
    fn kittest_card_without_agent_summary_has_no_copy_summary_button() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut candidate = row(
            "copy-summary-absent",
            storage::AgentWorkTurnState::Working,
            10,
        );
        candidate.agent_summary = None;
        let presentation = WorkHistoryActionPresentation {
            primary: WorkHistoryPrimaryAction::NewRun,
            show_diff: false,
        };
        let harness = card_harness(&catalog, &candidate, &presentation);

        assert!(
            harness
                .query_by_role_and_label(
                    egui::accesskit::Role::Button,
                    &catalog.t("history.action.copy_summary", &[])
                )
                .is_none(),
            "no agent_summary must not render a copy-summary button"
        );
        assert!(
            harness
                .query_by_role_and_label(
                    egui::accesskit::Role::Button,
                    &catalog.t("history.action.copy_instruction", &[])
                )
                .is_some(),
            "instruction copy button must still render regardless of summary presence"
        );
    }

    #[test]
    fn bounded_clipboard_text_stays_within_storage_cap_and_char_boundary() {
        let mut oversized = "x".repeat(WORK_HISTORY_CLIPBOARD_BYTES_MAX);
        oversized.push('한');

        let bounded = bounded_clipboard_text(&oversized);

        assert!(bounded.len() <= WORK_HISTORY_CLIPBOARD_BYTES_MAX);
        assert!(bounded.is_char_boundary(bounded.len()));
        assert_eq!(bounded, "x".repeat(WORK_HISTORY_CLIPBOARD_BYTES_MAX));

        let within_cap = "short instruction";
        assert_eq!(bounded_clipboard_text(within_cap), within_cap);
    }

    #[test]
    fn 접힌_카드_요약은_첫_줄만_쓴다() {
        // 목록의 스캔성이 우선 — 접힘 상태에서 카드 높이가 요약 줄 수마다
        // 달라지면 목록이 들쭉날쭉해진다.
        assert_eq!(collapsed_summary_line("첫 줄\n둘째 줄"), "첫 줄");
        assert_eq!(collapsed_summary_line("한 줄뿐"), "한 줄뿐");
        assert_eq!(collapsed_summary_line(""), "");
    }

    #[test]
    fn 카드_메시지는_인접_중복을_지운다() {
        // orca의 dedupeAdjacentConversationTurns와 같은 규칙 — 같은 역할이 같은 말을
        // 연달아 하면 한 번만 보여준다.
        let parsed = parse_turn_messages(
            r#"[{"r":"a","t":"같은 말"},{"r":"a","t":"같은 말"},{"r":"u","t":"다른 말"}]"#,
            "지시",
        );
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn 카드_메시지는_지시와_같은_턴을_지운다() {
        // 제목(instruction)이 본문에 한 번 더 나오는 것을 막는다.
        let parsed = parse_turn_messages(r#"[{"r":"u","t":"지시"},{"r":"a","t":"답"}]"#, "지시");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].text, "답");
    }

    #[test]
    fn 카드_메시지는_손상된_json을_비워서_돌려준다() {
        assert!(parse_turn_messages("{ 망가짐", "지시").is_empty());
        assert!(parse_turn_messages(r#"[{"r":"x","t":"모를 역할"}]"#, "지시").is_empty());
        assert!(
            parse_turn_messages(&format!("[{}]", r#"{"r":"a","t":"x"},"#.repeat(9)), "지시")
                .is_empty()
        );
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
        let views = views(&rows);

        let keys: Vec<&str> = ui
            .visible_rows(&views)
            .into_iter()
            .map(|row| row.turn_key)
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
        let views = views(&rows);

        for query in ["BILLING", "oauth", "claude", "OPUS", "high", "checkout"] {
            let mut ui = WorkHistoryUi::new();
            ui.query = query.to_owned();
            assert_eq!(ui.visible_rows(&views).len(), 1, "query={query}");
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
        let views = views(&rows);

        let keys: Vec<&str> = ui
            .visible_rows(&views)
            .into_iter()
            .map(|row| row.turn_key)
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
        let views = views(&rows);

        assert_eq!(
            ui.visible_rows(&views).len(),
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
        let views = views(&rows);

        let keys: Vec<&str> = ui
            .visible_rows(&views)
            .into_iter()
            .map(|row| row.turn_key)
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
        let views = views(&rows);
        assert_eq!(ui.visible_rows(&views).len(), 3);

        for (filter, expected) in [
            (WorkHistoryFilter::Working, "working"),
            (WorkHistoryFilter::Waiting, "waiting"),
            (WorkHistoryFilter::Completed, "completed"),
        ] {
            ui.filter = filter;
            let visible = ui.visible_rows(&views);
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

        ui.reconcile_selection(&views(std::slice::from_ref(&kept)));
        assert!(ui.selected.is_some());
        ui.reconcile_selection(&views(std::slice::from_ref(&replacement)));
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

        let view = WorkHistoryRow::from(&candidate);
        let metadata = metadata_parts(&view);
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
        let single = row("one", storage::AgentWorkTurnState::Completed, 1);
        assert!(
            ui.visible_rows(&views(std::slice::from_ref(&single)))
                .is_empty()
        );
    }

    // ---- 보조 검색 목록 필터 (Task 3) ----

    fn row_with_messages(
        turn_key: &str,
        state: storage::AgentWorkTurnState,
        updated_at: i64,
        messages_json: &str,
    ) -> storage::AgentWorkTurnRow {
        let mut candidate = row(turn_key, state, updated_at);
        candidate.messages_json = Some(messages_json.to_owned());
        candidate
    }

    #[test]
    fn 보조_필터는_instruction_agent_summary_messages_중_하나라도_걸리면_남긴다() {
        let mut by_instruction = row("by-instruction", storage::AgentWorkTurnState::Working, 3);
        by_instruction.instruction = "Fix the billing widget".to_owned();
        by_instruction.agent_summary = Some("unrelated".to_owned());

        let mut by_summary = row("by-summary", storage::AgentWorkTurnState::Working, 2);
        by_summary.instruction = "unrelated".to_owned();
        by_summary.agent_summary = Some("Reviewed the billing widget".to_owned());

        let by_message = row_with_messages(
            "by-message",
            storage::AgentWorkTurnState::Working,
            1,
            r#"[{"r":"u","t":"unrelated"},{"r":"a","t":"the billing widget is fixed"}]"#,
        );

        let no_match = row("no-match", storage::AgentWorkTurnState::Working, 0);

        let rows = vec![by_instruction, by_summary, by_message, no_match];
        let views = views(&rows);
        let ui = WorkHistoryUi::new();

        let groups = apply_aux_filter(ui.grouped_rows(&views), "billing widget");
        let keys: Vec<&str> = groups
            .iter()
            .flat_map(|group| group.rows.iter().map(|row| row.turn_key))
            .collect();

        assert_eq!(
            keys.len(),
            3,
            "instruction·agent_summary·messages_json 세 필드 중 하나라도 걸린 카드만 남아야 한다: {keys:?}"
        );
        assert!(keys.contains(&"by-instruction"));
        assert!(keys.contains(&"by-summary"));
        assert!(keys.contains(&"by-message"));
        assert!(
            !keys.contains(&"no-match"),
            "어느 필드에도 안 걸리면 빠져야 한다"
        );
    }

    #[test]
    fn 보조_필터는_대소문자를_구분하지않는다() {
        let mut candidate = row("case-insensitive", storage::AgentWorkTurnState::Working, 1);
        candidate.instruction = "Implement Billing Widget".to_owned();
        let views = views(std::slice::from_ref(&candidate));
        let ui = WorkHistoryUi::new();

        let groups = apply_aux_filter(ui.grouped_rows(&views), "billing WIDGET");

        assert_eq!(
            groups.iter().map(|group| group.rows.len()).sum::<usize>(),
            1
        );
    }

    #[test]
    fn 보조_필터가_비어있으면_모든_카드를_그대로_남긴다() {
        // messages_json이 손상돼 있어도(파싱 불가) 필터가 비어 있으면 아예 건드리지
        // 않는다 — `apply_aux_filter`는 `filter.is_empty()`에서 원본 groups를 그대로
        // 돌려주고 `row_matches_aux_filter`(→ 매 카드 JSON 파싱)를 호출하지 않는다.
        // 평상시(필터 없음) 프레임 비용이 이 Task 이전과 같아야 한다는 요구사항의
        // 회귀 방지 테스트.
        let broken = row_with_messages(
            "broken-json",
            storage::AgentWorkTurnState::Working,
            1,
            "{ 이건 파싱되면 안 된다",
        );
        let views = views(std::slice::from_ref(&broken));
        let ui = WorkHistoryUi::new();

        let groups = apply_aux_filter(ui.grouped_rows(&views), "");

        assert_eq!(
            groups.iter().map(|group| group.rows.len()).sum::<usize>(),
            1
        );
    }

    #[test]
    fn kittest_보조_필터에_걸리는_카드가_없으면_안내문구를_보여준다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let no_match_label = catalog.t("search.no_match", &[]);
        let candidate = row(
            "aux-filter-empty-result",
            storage::AgentWorkTurnState::Working,
            10,
        );
        let presentation = WorkHistoryActionPresentation {
            primary: WorkHistoryPrimaryAction::NewRun,
            show_diff: false,
        };
        let rows = vec![candidate];
        let views = views(&rows);
        let presentations = vec![presentation];
        let mut harness = full_harness(&catalog, &views, &presentations, "no-such-text-anywhere");
        harness.run();

        assert!(
            harness.query_by_label(&no_match_label).is_some(),
            "필터에 걸리는 카드가 없으면 안내 문구가 보여야 한다"
        );
    }

    // ---- 세션 그룹핑 (A2/B1) ----

    #[test]
    fn 빈_rows는_그룹이_없다() {
        let ui = WorkHistoryUi::new();
        assert!(ui.grouped_rows(&[]).is_empty());
    }

    #[test]
    fn 같은_kind_같은_agent_session_id는_한_그룹으로_묶인다() {
        let rows = vec![
            row("turn-1", storage::AgentWorkTurnState::Working, 10),
            row("turn-2", storage::AgentWorkTurnState::Completed, 5),
        ];
        let ui = WorkHistoryUi::new();
        let views = views(&rows);

        let groups = ui.grouped_rows(&views);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].rows.len(), 2);
    }

    #[test]
    fn kind이_다르면_같은_agent_session_id라도_다른_그룹이다() {
        // provider 사이에서 같은 세션 id가 우연히 충돌할 수 있으므로 kind도 그룹
        // 키에 들어가야 한다(작업 지시사항의 명시 요구).
        let mut claude_turn = row("shared-id-claude", storage::AgentWorkTurnState::Working, 10);
        claude_turn.kind = "claude".to_owned();
        claude_turn.agent_session_id = "shared-id".to_owned();
        let mut codex_turn = row("shared-id-codex", storage::AgentWorkTurnState::Working, 9);
        codex_turn.kind = "codex".to_owned();
        codex_turn.agent_session_id = "shared-id".to_owned();
        let rows = vec![claude_turn, codex_turn];
        let ui = WorkHistoryUi::new();
        let views = views(&rows);

        assert_eq!(
            ui.grouped_rows(&views).len(),
            2,
            "kind가 다르면 같은 session id라도 별도 그룹이어야 한다"
        );
    }

    #[test]
    fn 다른_agent_session_id는_다른_그룹이다() {
        let mut first = row("turn-a", storage::AgentWorkTurnState::Working, 10);
        first.agent_session_id = "session-a".to_owned();
        let mut second = row("turn-b", storage::AgentWorkTurnState::Working, 9);
        second.agent_session_id = "session-b".to_owned();
        let rows = vec![first, second];
        let ui = WorkHistoryUi::new();
        let views = views(&rows);

        assert_eq!(ui.grouped_rows(&views).len(), 2);
    }

    /// 그룹 순서는 "그룹 내 최상위 turn"의 순위를 따르고, 그룹 내부 턴 순서도 같은
    /// 전역 정렬 규칙을 그대로 물려받는다. session-a는 working 턴(랭크 최상위) 하나와
    /// completed 턴(최신이지만 랭크가 낮음) 하나를 가져 이 둘을 구분한다.
    #[test]
    fn statefirst_그룹_순서는_그룹_내_최상위_turn_rank를_따른다() {
        let mut a_working = row("a-working", storage::AgentWorkTurnState::Working, 1);
        a_working.agent_session_id = "session-a".to_owned();
        let mut a_completed = row("a-completed", storage::AgentWorkTurnState::Completed, 99);
        a_completed.agent_session_id = "session-a".to_owned();
        let mut b_waiting = row("b-waiting", storage::AgentWorkTurnState::Waiting, 100);
        b_waiting.agent_session_id = "session-b".to_owned();
        let mut c_completed = row("c-completed", storage::AgentWorkTurnState::Completed, 50);
        c_completed.agent_session_id = "session-c".to_owned();
        let rows = vec![a_working, a_completed, b_waiting, c_completed];
        let ui = WorkHistoryUi::new();
        let views = views(&rows);

        let groups = ui.grouped_rows(&views);

        let group_ids: Vec<&str> = groups.iter().map(|group| group.agent_session_id).collect();
        assert_eq!(
            group_ids,
            ["session-a", "session-b", "session-c"],
            "a는 working 턴을 갖고 있어 가장 앞이어야 한다"
        );
        let a_turn_keys: Vec<&str> = groups[0].rows.iter().map(|row| row.turn_key).collect();
        assert_eq!(
            a_turn_keys,
            ["a-working", "a-completed"],
            "그룹 내부에서도 working이 completed보다 앞이어야 한다(전역 규칙 상속)"
        );
    }

    /// RecentFirst는 상태와 무관하게 그룹 내 최신 updated_at으로만 그룹 순서를 정한다.
    #[test]
    fn recentfirst_그룹_순서는_그룹_내_최신_updated_at을_따른다() {
        let mut x = row("x-completed", storage::AgentWorkTurnState::Completed, 30);
        x.agent_session_id = "session-x".to_owned();
        let mut z = row("z-working", storage::AgentWorkTurnState::Working, 20);
        z.agent_session_id = "session-z".to_owned();
        let mut y_working = row("y-working", storage::AgentWorkTurnState::Working, 10);
        y_working.agent_session_id = "session-y".to_owned();
        let mut y_waiting = row("y-waiting", storage::AgentWorkTurnState::Waiting, 5);
        y_waiting.agent_session_id = "session-y".to_owned();
        let rows = vec![x, z, y_working, y_waiting];
        let mut ui = WorkHistoryUi::new();
        ui.sort_mode = WorkHistorySortMode::RecentFirst;
        let views = views(&rows);

        let groups = ui.grouped_rows(&views);

        let group_ids: Vec<&str> = groups.iter().map(|group| group.agent_session_id).collect();
        assert_eq!(group_ids, ["session-x", "session-z", "session-y"]);
        let y_turn_keys: Vec<&str> = groups[2].rows.iter().map(|row| row.turn_key).collect();
        assert_eq!(y_turn_keys, ["y-working", "y-waiting"]);
    }

    #[test]
    fn 상태필터가_그룹의_일부턴만_남기면_그룹은_남은턴만_담는다() {
        let mut working = row("s-working", storage::AgentWorkTurnState::Working, 10);
        working.agent_session_id = "session-s".to_owned();
        let mut waiting = row("s-waiting", storage::AgentWorkTurnState::Waiting, 9);
        waiting.agent_session_id = "session-s".to_owned();
        let rows = vec![working, waiting];
        let mut ui = WorkHistoryUi::new();
        ui.filter = WorkHistoryFilter::Working;
        let views = views(&rows);

        let groups = ui.grouped_rows(&views);

        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].rows.len(),
            1,
            "헤더 턴 수도 남은 턴만 반영해야 한다"
        );
        assert_eq!(groups[0].rows[0].turn_key, "s-working");
    }

    #[test]
    fn 필터로_그룹의_모든턴이_사라지면_그룹자체가_사라진다() {
        let mut only_completed = row("s1-completed", storage::AgentWorkTurnState::Completed, 10);
        only_completed.agent_session_id = "session-s1".to_owned();
        let mut has_working = row("s2-working", storage::AgentWorkTurnState::Working, 9);
        has_working.agent_session_id = "session-s2".to_owned();
        let rows = vec![only_completed, has_working];
        let mut ui = WorkHistoryUi::new();
        ui.filter = WorkHistoryFilter::Working;
        let views = views(&rows);

        let groups = ui.grouped_rows(&views);

        assert_eq!(
            groups.len(),
            1,
            "필터에 남는 턴이 없는 세션은 그룹째로 사라져야 한다"
        );
        assert_eq!(groups[0].agent_session_id, "session-s2");
    }

    /// A2 결함 재현 시나리오 — 최신 턴은 cwd 파생 사실(branch/변경 수)이 없고, 더
    /// 오래된 턴에만 있다. 헤더는 "값이 있는 가장 최신 턴"을 찾아야 하며, 그냥
    /// 가장 최신 턴(newer)의 빈 값을 그대로 쓰면 안 된다.
    #[test]
    fn 헤더_branch와_변경수는_값이_있는_가장_최신턴에서_가져온다() {
        let mut older_with_value = row("s-older", storage::AgentWorkTurnState::Completed, 5);
        older_with_value.agent_session_id = "session-s".to_owned();
        older_with_value.branch = Some("main".to_owned());
        older_with_value.git_change_count = Some(2);
        older_with_value.model = Some("model-old".to_owned());
        older_with_value.effort = Some("low".to_owned());
        let mut newer_without_value = row("s-newer", storage::AgentWorkTurnState::Working, 50);
        newer_without_value.agent_session_id = "session-s".to_owned();
        newer_without_value.branch = None;
        newer_without_value.git_change_count = None;
        newer_without_value.model = None;
        newer_without_value.effort = None;
        let rows = vec![older_with_value, newer_without_value];
        let ui = WorkHistoryUi::new();
        let views = views(&rows);

        let groups = ui.grouped_rows(&views);

        assert_eq!(groups.len(), 1);
        let group = &groups[0];
        assert_eq!(group.branch, Some("main"));
        assert_eq!(group.git_change_count, Some(2));
        assert_eq!(group.model, Some("model-old"));
        assert_eq!(group.effort, Some("low"));
        assert_eq!(
            group.latest_updated_at, 50,
            "헤더의 '최신 시각'은 값 유무와 무관하게 그룹의 진짜 최신 턴을 따른다"
        );
    }

    #[test]
    fn 값이_아예없으면_헤더_필드를_비운다() {
        let mut first = row("s-a", storage::AgentWorkTurnState::Working, 10);
        first.agent_session_id = "session-s".to_owned();
        first.branch = None;
        first.git_change_count = None;
        let mut second = row("s-b", storage::AgentWorkTurnState::Completed, 5);
        second.agent_session_id = "session-s".to_owned();
        second.branch = None;
        second.git_change_count = None;
        let rows = vec![first, second];
        let ui = WorkHistoryUi::new();
        let views = views(&rows);

        let groups = ui.grouped_rows(&views);

        assert!(groups[0].branch.is_none(), "없는 값을 지어내면 안 된다");
        assert!(groups[0].git_change_count.is_none());
    }

    #[test]
    fn 그룹_토글은_접기_펴기를_전환하고_기본값은_펼침이다() {
        let mut ui = WorkHistoryUi::new();
        let key = ("codex".to_owned(), "agent-session-a".to_owned());
        assert!(
            !ui.collapsed_groups.contains(&key),
            "기본값은 펼침이어야 한다"
        );

        ui.toggle_group_collapsed(key.clone());
        assert!(ui.collapsed_groups.contains(&key));

        ui.toggle_group_collapsed(key.clone());
        assert!(!ui.collapsed_groups.contains(&key));
    }

    fn full_harness<'a>(
        catalog: &'a i18n::Catalog,
        rows: &'a [WorkHistoryRow<'a>],
        presentations: &'a [WorkHistoryActionPresentation],
        filter: &'a str,
    ) -> egui_kittest::Harness<'a, WorkHistoryUi> {
        egui_kittest::Harness::builder()
            .with_size(egui::vec2(900.0, 700.0))
            .build_ui_state(
                move |ui, state: &mut WorkHistoryUi| {
                    state.show(
                        ui,
                        WorkHistorySnapshot {
                            workspace_name: "workspace",
                            current_branch: None,
                            rows,
                            // 이 하니스로 여러 프레임(`harness.run()`을 두 번 이상)을
                            // 도는 테스트가 없으므로 고정값 1이면 충분하다 — 캐시
                            // 무효화 자체는 `WorkHistoryUi` 단위 테스트가 별도로 잰다.
                            rows_revision: 1,
                            loading: false,
                            error: None,
                        },
                        presentations,
                        catalog,
                        filter,
                    );
                },
                WorkHistoryUi::new(),
            )
    }

    #[test]
    fn kittest_group_header_click_toggles_card_visibility() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let candidate = row(
            "group-toggle-hit-test",
            storage::AgentWorkTurnState::Working,
            10,
        );
        let presentation = WorkHistoryActionPresentation {
            primary: WorkHistoryPrimaryAction::NewRun,
            show_diff: false,
        };
        let header_label = group_accessible_label(&candidate.kind, &candidate.agent_session_id);
        let instruction = candidate.instruction.clone();
        let rows = vec![candidate];
        let views = views(&rows);
        let presentations = vec![presentation];
        let mut harness = full_harness(&catalog, &views, &presentations, "");
        harness.run();

        assert!(
            harness
                .query_by_role_and_label(egui::accesskit::Role::Button, &instruction)
                .is_some(),
            "접기 전에는 턴 카드가 보여야 한다"
        );

        // `ScrollArea` claims raw pointer press/release for its own drag-to-scroll
        // sensing before nested `Sense::click()` scopes see them, so a simulated
        // `.click()` (raw pointer events) never reaches the header inside the
        // history list's scroll area. `click_accesskit()` drives the same
        // `Response` through AccessKit's `Action::Click` instead and reliably
        // reaches it.
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, &header_label)
            .click_accesskit();
        // 클릭을 처리하는 프레임은 `collapsed` 값을 헤더를 그리기 **전에** 이미
        // 읽어 뒀으므로 그 프레임의 카드 렌더링에는 아직 반영되지 않는다(egui
        // 즉시모드의 흔한 1프레임 지연). 구조 변화를 확인하려면 한 프레임 더
        // 돌려야 한다.
        harness.run();
        harness.run();

        assert!(
            harness
                .query_by_role_and_label(egui::accesskit::Role::Button, &instruction)
                .is_none(),
            "그룹 헤더를 접으면 턴 카드가 숨어야 한다"
        );

        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, &header_label)
            .click_accesskit();
        harness.run();
        harness.run();

        assert!(
            harness
                .query_by_role_and_label(egui::accesskit::Role::Button, &instruction)
                .is_some(),
            "다시 클릭하면 펼쳐져야 한다"
        );
    }

    /// 카드 토글(`kittest_card_toggle_does_not_contain_action_buttons`)과 같은
    /// 회귀 방지 — 그룹 헤더도 접기/펴기 토글이라 같은 함정(토글 스코프 안에
    /// 실제 버튼을 두는 것)이 있다. 헤더는 라벨만 그리므로 지금은 버튼이 없어야
    /// 하고, 나중에 실수로 버튼을 넣으면 이 테스트가 먼저 깨진다.
    #[test]
    fn kittest_group_header_toggle_does_not_contain_buttons() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let candidate = row(
            "group-header-a11y-tree",
            storage::AgentWorkTurnState::Completed,
            10,
        );
        let presentation = WorkHistoryActionPresentation {
            primary: WorkHistoryPrimaryAction::NewRun,
            show_diff: true,
        };
        let header_label = group_accessible_label(&candidate.kind, &candidate.agent_session_id);
        let rows = vec![candidate];
        let views = views(&rows);
        let presentations = vec![presentation];
        let mut harness = full_harness(&catalog, &views, &presentations, "");
        harness.run();

        let header = harness.get_by_role_and_label(egui::accesskit::Role::Button, &header_label);
        assert!(
            header
                .query_by_role(egui::accesskit::Role::Button)
                .is_none(),
            "group header toggle must not contain nested button widgets"
        );
    }

    #[test]
    fn 좁은_폭에서도_그룹_헤더_요소가_화면_안에_머문다() {
        use egui_kittest::kittest::Queryable;

        const NARROW: f32 = 260.0;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut candidate = row(
            "narrow-header-wrap",
            storage::AgentWorkTurnState::Working,
            10,
        );
        candidate.model = Some("gpt-5.6-sol-extended-reasoning".to_owned());
        candidate.branch = Some("feature/very-long-branch-name-for-wrapping".to_owned());
        let presentation = WorkHistoryActionPresentation {
            primary: WorkHistoryPrimaryAction::NewRun,
            show_diff: false,
        };
        // `catalog`가 아래 `move` 클로저로 소유권째 넘어가므로, 클로저 구성 전에
        // 미리 라벨 문자열을 뽑아 둔다(이동 뒤에는 `catalog`를 다시 쓸 수 없다).
        let turn_count_label = catalog.t("history.group.turns", &[("count", "1")]);
        let rows = vec![candidate];
        let views = views(&rows);
        let presentations = vec![presentation];
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(NARROW, 500.0))
            .build_ui_state(
                move |ui, state: &mut WorkHistoryUi| {
                    state.show(
                        ui,
                        WorkHistorySnapshot {
                            workspace_name: "workspace",
                            current_branch: None,
                            rows: &views,
                            rows_revision: 1,
                            loading: false,
                            error: None,
                        },
                        &presentations,
                        &catalog,
                        "",
                    );
                },
                WorkHistoryUi::new(),
            );
        harness.run();

        let element = harness
            .query_by_label(&turn_count_label)
            .expect("좁은 폭에서 턴 수 라벨이 사라졌다");
        assert!(
            element.rect().right() <= NARROW,
            "턴 수 라벨이 캔버스 밖으로 넘친다: {}",
            element.rect().right()
        );
    }

    // ---- 필터·정렬·그룹핑 캐시 (스펙 이슈 #4) ----

    fn base_grouped_key() -> WorkHistoryGroupedCacheKey {
        WorkHistoryGroupedCacheKey {
            rows_revision: 1,
            query: String::new(),
            filter: WorkHistoryFilter::All,
            providers: Vec::new(),
            sort_mode: WorkHistorySortMode::StateFirst,
        }
    }

    #[test]
    fn 캐시된_그룹핑은_캐시_없는_그룹핑과_같은_결과를_돌려준다() {
        let rows = vec![
            row("a", storage::AgentWorkTurnState::Working, 3),
            row("b", storage::AgentWorkTurnState::Completed, 2),
        ];
        let views = views(&rows);
        let mut ui = WorkHistoryUi::new();
        let key = base_grouped_key();

        let cached = ui.cached_grouped_rows(&views, &key);
        let direct = ui.grouped_rows(&views);

        let cached_keys: Vec<&str> = cached
            .iter()
            .flat_map(|group| group.rows.iter().map(|row| row.turn_key))
            .collect();
        let direct_keys: Vec<&str> = direct
            .iter()
            .flat_map(|group| group.rows.iter().map(|row| row.turn_key))
            .collect();
        assert_eq!(cached_keys, direct_keys);
    }

    #[test]
    fn 캐시_키가_같으면_저장된_계획을_다시_계산하지_않고_그대로_쓴다() {
        let rows = vec![row("a", storage::AgentWorkTurnState::Working, 1)];
        let views = views(&rows);
        let mut ui = WorkHistoryUi::new();
        let key = base_grouped_key();

        let _ = ui.cached_grouped_rows(&views, &key); // 캐시를 채운다(진짜 계획: [[0]]).
        // 캐시된 계획을 일부러 틀리게 바꾼다 — 키가 같은데도 진짜로 다시 계산했다면
        // 이 조작은 무시되고 원래 계획(그룹 1개)으로 되돌아올 것이다. 조작한 값이
        // 그대로 나오면 재계산을 건너뛰고 캐시를 재사용했다는 뜻이다.
        ui.grouped_cache.as_mut().unwrap().1 = vec![];

        let reused = ui.cached_grouped_rows(&views, &key);
        assert!(
            reused.is_empty(),
            "리비전·질의·필터·provider·정렬 모드가 그대로면 캐시를 다시 계산하면 안 된다"
        );
    }

    #[test]
    fn 캐시_키가_하나라도_바뀌면_저장된_계획을_버리고_다시_계산한다() {
        let rows = vec![row("a", storage::AgentWorkTurnState::Working, 1)];
        let views = views(&rows);
        let base = base_grouped_key();
        let variants = [
            WorkHistoryGroupedCacheKey {
                rows_revision: 2,
                ..base.clone()
            },
            WorkHistoryGroupedCacheKey {
                query: "billing".to_owned(),
                ..base.clone()
            },
            WorkHistoryGroupedCacheKey {
                filter: WorkHistoryFilter::Waiting,
                ..base.clone()
            },
            WorkHistoryGroupedCacheKey {
                providers: vec![WorkHistoryProvider::Claude],
                ..base.clone()
            },
            WorkHistoryGroupedCacheKey {
                sort_mode: WorkHistorySortMode::RecentFirst,
                ..base.clone()
            },
        ];

        for variant in variants {
            let mut ui = WorkHistoryUi::new();
            let _ = ui.cached_grouped_rows(&views, &base);
            let _ = ui.cached_grouped_rows(&views, &variant);
            assert_eq!(
                ui.grouped_cache.as_ref().map(|(key, _)| key),
                Some(&variant),
                "입력이 바뀌면 캐시 키를 새 값으로 갱신해야 한다(재계산이 실제로 일어났다는 뜻)"
            );
        }
    }

    #[test]
    fn 접힌_그룹은_헤더_슬롯_하나만_만든다() {
        let rows = vec![
            row("a", storage::AgentWorkTurnState::Working, 1),
            row("b", storage::AgentWorkTurnState::Working, 2),
        ];
        let views = views(&rows);
        let ui = WorkHistoryUi::new();
        let groups = ui.grouped_rows(&views);
        assert_eq!(
            groups.len(),
            1,
            "같은 kind/session_id 두 턴은 한 그룹이어야 한다"
        );

        let mut collapsed = std::collections::HashSet::new();
        collapsed.insert((
            groups[0].kind.to_owned(),
            groups[0].agent_session_id.to_owned(),
        ));
        let slots = build_slots(&groups, &collapsed);

        assert_eq!(
            slots.len(),
            1,
            "접힌 그룹은 헤더 슬롯 하나만 남아야 한다 — 카드는 슬롯이 아니다"
        );
        assert!(matches!(
            slots[0].kind,
            WorkHistorySlotKind::Header { collapsed: true }
        ));
    }

    #[test]
    fn 펼친_그룹은_헤더_다음에_카드_슬롯이_행_수만큼_있고_마지막만_그룹_끝이다() {
        let rows = vec![
            row("a", storage::AgentWorkTurnState::Working, 1),
            row("b", storage::AgentWorkTurnState::Working, 2),
            row("c", storage::AgentWorkTurnState::Working, 3),
        ];
        let views = views(&rows);
        let ui = WorkHistoryUi::new();
        let groups = ui.grouped_rows(&views);
        let slots = build_slots(&groups, &std::collections::HashSet::new());

        assert_eq!(slots.len(), 4, "헤더 1 + 카드 3");
        assert!(matches!(
            slots[0].kind,
            WorkHistorySlotKind::Header { collapsed: false }
        ));
        assert!(!slots[0].is_group_end);
        for (offset, slot) in slots[1..].iter().enumerate() {
            assert!(matches!(slot.kind, WorkHistorySlotKind::Card { .. }));
            assert_eq!(
                slot.is_group_end,
                offset == 2,
                "그룹의 마지막 카드만 그룹 끝이어야 한다"
            );
        }
    }

    // ---- 목록 가상화(스펙 이슈 #1) ----

    fn presentations_for<'a>(rows: &[WorkHistoryRow<'a>]) -> Vec<WorkHistoryActionPresentation> {
        rows.iter()
            .map(|_| WorkHistoryActionPresentation {
                primary: WorkHistoryPrimaryAction::NewRun,
                show_diff: false,
            })
            .collect()
    }

    /// transcript_viewer.rs `kittest_뷰포트_밖_메시지는_배치되지_않고_최신_메시지는_보인다`
    /// 와 같은 계약 — 부트스트랩 프레임(1번째 `run()`)은 전부 배치해 높이를 재고,
    /// 가상화 프레임(2번째 `run()`)부터 뷰포트에 걸치는 슬롯만 배치한다.
    #[test]
    fn kittest_뷰포트_밖_카드는_배치되지_않고_최근_카드는_보인다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let rows: Vec<storage::AgentWorkTurnRow> = (0..200)
            .map(|i| {
                row(
                    &format!("turn-{i}"),
                    storage::AgentWorkTurnState::Working,
                    i as i64,
                )
            })
            .collect();
        let views = views(&rows);
        let presentations = presentations_for(&views);
        let mut harness = full_harness(&catalog, &views, &presentations, "");

        harness.run(); // 부트스트랩: 전부 배치해 높이를 잰다.
        harness.run(); // 가상화 프레임: 뷰포트 걸치는 구간만 배치한다.

        // StateFirst 정렬은 같은 상태(Working)에서 updated_at desc라 turn-199(가장
        // 최신)가 맨 위에 온다.
        assert!(
            harness
                .query_by_role_and_label(egui::accesskit::Role::Button, "Instruction turn-199")
                .is_some(),
            "가장 최근 카드는 스크롤 맨 위 뷰포트 안이라 배치돼야 한다"
        );
        assert!(
            harness
                .query_by_role_and_label(egui::accesskit::Role::Button, "Instruction turn-0")
                .is_none(),
            "뷰포트에서 한참 벗어난(overscan 밖) 카드는 배치되지 않아야 한다 — 그래야 \
             스크롤 프레임 비용이 행 수가 아니라 뷰포트 크기로 유계가 된다"
        );
    }

    /// 작업 지시의 "절대 지켜야 할 것" — 화면 밖 카드를 선택한 상태로 스크롤(=가상화
    /// 프레임을 지남)해도 선택이 풀리면 안 된다. `selected`는 슬롯 렌더 여부와
    /// 무관한 별도 상태라 가상화 로직이 건드리지 않는다는 걸 고정한다.
    #[test]
    fn kittest_화면_밖으로_스크롤해도_선택한_카드_상태는_유지된다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let rows: Vec<storage::AgentWorkTurnRow> = (0..200)
            .map(|i| {
                row(
                    &format!("turn-{i}"),
                    storage::AgentWorkTurnState::Working,
                    i as i64,
                )
            })
            .collect();
        // turn-0은 StateFirst 정렬(같은 Working끼리는 updated_at desc)에서 맨 아래로
        // 밀려나 뷰포트 밖이다.
        let identity_turn_0 = WorkTurnIdentity::from(&rows[0]);
        let views = views(&rows);
        let presentations = presentations_for(&views);

        let mut initial = WorkHistoryUi::new();
        initial.selected = Some(identity_turn_0.clone());
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(900.0, 700.0))
            .build_ui_state(
                move |ui, state: &mut WorkHistoryUi| {
                    state.show(
                        ui,
                        WorkHistorySnapshot {
                            workspace_name: "workspace",
                            current_branch: None,
                            rows: &views,
                            rows_revision: 1,
                            loading: false,
                            error: None,
                        },
                        &presentations,
                        &catalog,
                        "",
                    );
                },
                initial,
            );

        harness.run(); // 부트스트랩
        harness.run(); // 가상화 — turn-0 카드는 화면 밖이라 배치되지 않는다

        assert_eq!(
            harness.state().selected,
            Some(identity_turn_0),
            "화면 밖 카드를 선택한 채 가상화 프레임을 지나도 선택 상태가 남아야 한다"
        );
    }

    // ---- presentation 인덱스 대응(스펙 이슈 #2·#3) ----

    /// `App`은 `rows`와 `presentations`를 같은 순회에서 함께 만들어 인덱스로
    /// 대응시킨다(문서 참고) — 이 leaf는 필터·정렬로 순서가 바뀌어도 원본 인덱스
    /// (`WorkHistoryGroup::row_indices`)로 각 카드에 맞는 presentation을 찾는다.
    /// 인덱스 대응이 깨지면 카드가 이웃의 버튼을 보여주는 사고가 나므로, 입력 배열
    /// 순서와 화면 순서가 다르게(정렬이 뒤집게) 일부러 구성해 검증한다.
    #[test]
    fn kittest_정렬로_순서가_바뀌어도_각_카드는_자기_프레젠테이션을_보여준다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        // StateFirst 정렬에서 Working이 Completed보다 앞선다 — 입력 배열 순서
        // (old, new)와 화면 순서(new, old)가 뒤집힌다.
        let row_old = row("row-old", storage::AgentWorkTurnState::Completed, 1);
        let row_new = row("row-new", storage::AgentWorkTurnState::Working, 2);
        let identity_old = WorkTurnIdentity::from(&row_old);
        let rows = vec![row_old, row_new];
        let views = views(&rows);
        // presentations는 rows와 같은 순서(App이 보장하는 인덱스 정렬 계약)로 둔다.
        let presentations = vec![
            WorkHistoryActionPresentation {
                primary: WorkHistoryPrimaryAction::Resume,
                show_diff: false,
            },
            WorkHistoryActionPresentation {
                primary: WorkHistoryPrimaryAction::Focus,
                show_diff: false,
            },
        ];

        let mut initial = WorkHistoryUi::new();
        initial.selected = Some(identity_old);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(900.0, 700.0))
            .build_ui_state(
                move |ui, state: &mut WorkHistoryUi| {
                    state.show(
                        ui,
                        WorkHistorySnapshot {
                            workspace_name: "workspace",
                            current_branch: None,
                            rows: &views,
                            rows_revision: 1,
                            loading: false,
                            error: None,
                        },
                        &presentations,
                        &catalog,
                        "",
                    );
                },
                initial,
            );
        harness.run();

        assert!(
            harness
                .query_by_role_and_label(egui::accesskit::Role::Button, "Resume")
                .is_some(),
            "화면 아래쪽(row-old)이 펼쳐졌을 때 자기 presentation(Resume)을 보여줘야 한다"
        );
        assert!(
            harness
                .query_by_role_and_label(egui::accesskit::Role::Button, "Go to current session")
                .is_none(),
            "row-old가 펼쳐진 동안 이웃 row-new의 Focus 버튼이 보이면 안 된다"
        );
    }
}
