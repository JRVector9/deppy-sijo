//! 멀티에이전트 fleet 뷰모델 (기능1) — 순수 데이터 + 로직.
//!
//! deppy는 외부 에이전트(Claude Code/Codex)를 여러 워크스페이스에 걸쳐 호스트한다.
//! fleet 뷰는 그 세션들을 한 화면에 모아 상태를 한눈에 보여준다. 이 모듈은 **순수
//! 데이터**다(egui·PTY·App을 모른다): App이 active+warm 런타임을 가로질러 세션을
//! `FleetSession` 행으로 조립하고(PR-5), 이 모듈의 정렬·요약 로직으로 우선순위를 매긴다.
//!
//! 상태는 새 소스를 만들지 않고 기존 정규화 상태 [`AgentVisualState`]를 재사용한다 —
//! 단일 진실원 유지(agent_surface). needs-input 사유·활동·모델 등은 App이 기존
//! 소스(global_waiting, session_entries)에서 뽑아 넣는다.

use crate::agent_surface::AgentVisualState;

/// 작업 메뉴의 상태별 건수. 상태 없는 셸·비활성 저장 행은 알림에서 제외한다.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FleetNavSummary {
    pub blocked: usize,
    pub errored: usize,
    pub running: usize,
    pub idle: usize,
    pub complete: usize,
}

impl FleetNavSummary {
    pub fn collect<'a>(
        sessions: impl IntoIterator<Item = (&'a str, runtime::SessionId, AgentVisualState)>,
        waiting: impl IntoIterator<Item = (&'a str, runtime::SessionId)>,
        approvals: impl IntoIterator<Item = Option<(&'a str, runtime::SessionId)>>,
        structured: impl IntoIterator<Item = AgentVisualState>,
    ) -> Self {
        // 대기가 없으면 힙 할당 없이 끝난다. 문자열·세션 행 전체는 복제하지 않는다.
        let mut blocked: std::collections::HashSet<_> = waiting.into_iter().collect();
        let mut summary = Self::default();
        for approval in approvals {
            if let Some(key) = approval {
                blocked.insert(key);
            } else {
                // 세션이 없거나 연결 키를 모르는 승인도 사용자의 처리가 필요하다.
                summary.blocked += 1;
            }
        }
        summary.blocked += blocked.len();
        for (workspace, session, state) in sessions {
            if !blocked.contains(&(workspace, session)) {
                summary.add(state);
            }
        }
        for state in structured {
            summary.add(state);
        }
        summary
    }

    fn add(&mut self, state: AgentVisualState) {
        match state {
            AgentVisualState::Waiting | AgentVisualState::NeedsResponse => self.blocked += 1,
            AgentVisualState::Error => self.errored += 1,
            AgentVisualState::Active => self.running += 1,
            AgentVisualState::Idle => self.idle += 1,
            AgentVisualState::Complete => self.complete += 1,
            AgentVisualState::Off => {}
        }
    }

    pub fn counts(self) -> [(AgentVisualState, usize); 5] {
        [
            (AgentVisualState::Waiting, self.blocked),
            (AgentVisualState::Error, self.errored),
            (AgentVisualState::Active, self.running),
            (AgentVisualState::Complete, self.complete),
            (AgentVisualState::Idle, self.idle),
        ]
    }

    /// 여러 상태가 섞이면 사용자 조치가 필요한 상태부터 표시한다.
    pub fn primary(self) -> Option<(AgentVisualState, usize)> {
        self.counts().into_iter().find(|(_, count)| *count > 0)
    }
}

/// fleet 그리드의 세션 행 하나. App이 active/warm 런타임의 여러 필드에서 조립한
/// 읽기전용 스냅샷이다(leaf+intent+host I/O 경계: UI는 이 스냅샷만 그린다).
#[derive(Debug, Clone, PartialEq)]
pub struct FleetSession {
    pub workspace_id: String,
    pub workspace_name: String,
    /// 포커스 라우팅 + 종류(PTY vs 구조화). 브로드캐스트는 PTY만 대상이다.
    pub target: FleetTarget,
    pub title: String,
    /// 정규화된 시각 상태(agent_surface). needs-input/승인/오류/완료/작업중/유휴/off.
    pub state: AgentVisualState,
    /// 에이전트 2행 "Codex · gpt-5.5 · xhigh" 또는 "[APP] Codex · …".
    pub agent_line: Option<String>,
    /// 현재 작업 또는 끝난 작업의 마지막 응답. 한 줄·160자 이하로 제한한다.
    pub task_line: Option<String>,
    /// hook이 보고한 대기 사유(needs-input 메시지). Waiting 상태에서만 대개 Some.
    pub waiting_message: Option<String>,
    /// active 워크스페이스의 세션인지(그 외는 warm — 물러났지만 워커는 실행 중).
    pub active_workspace: bool,
    /// **나를 막기 시작한 시각**(unix 초). Waiting에서만 Some이고, 이 값이 곧 정렬 키다.
    /// 화면에 그대로 보여줘 "왜 이게 위에 있나"를 설명할 필요가 없게 한다.
    pub blocked_since: Option<i64>,
    /// 지시 대기가 시작된 시각(unix 초). hook 완료 시각이 있으면 그 값을 쓴다.
    pub idle_since: Option<i64>,
    /// Completion identity; never interpreted as a wall-clock timestamp.
    pub idle_generation: Option<i64>,
    /// 마지막으로 새 출력이 온 시각(unix 초). 구조화(App Server) 세션은 PTY 스냅샷이
    /// 없어 항상 None이다 — 그 묶음에는 「출력 없음」을 표시하지 않는다.
    pub last_output_at: Option<i64>,
    /// 이 턴이 끝나면 이어서 보낼 예약 프롬프트. 카드 칩과 예약 패널의 초기값을 함께
    /// 담당한다 — 원문이 있어야 다시 열었을 때 고쳐 쓸 수 있다. PTY 전용이라 구조화
    /// 세션은 항상 None이다(steer 경로).
    pub followup: Option<String>,
}

/// 「작업 중」인데 이만큼 출력이 없으면 멈춘 것으로 본다.
pub const STUCK_AFTER_SECS: i64 = 300;

/// 조용히 멈춘 세션인가 — 돌고 있다고 표시되는데 한참 아무것도 안 뱉은 경우.
///
/// "작업 중"과 "멈춘 것"은 화면상 둘 다 파란 점인데 실제로는 전혀 다르다. 출력이 원래
/// 뜸한 작업도 있어 오탐이 가능하므로 상태를 바꾸지 않고 **표시만** 덧붙인다.
pub fn stuck_for(session: &FleetSession, now: i64) -> Option<i64> {
    if session.state != AgentVisualState::Active {
        return None;
    }
    let silent = now.saturating_sub(session.last_output_at?);
    (silent >= STUCK_AFTER_SECS).then_some(silent)
}

/// 예약한 다음 단계를 지금 보내도 되는가.
///
/// 근거는 hook의 turn_done(Stop) 하나다. **「작업 중이 아니다」는 턴 끝이 아니다** —
/// working의 stale 창은 2분인데 하트비트는 툴 호출마다라, 툴 하나가 2분을 넘으면 같은
/// 턴이 working에서 빠졌다 다시 들어온다(app.rs `turn_start_transitions`의 근거와 동일).
/// 그걸 턴 끝으로 오인하면 **돌고 있는 턴 한가운데에** 프롬프트를 밀어넣게 된다.
///
/// 입력 대기면 보내지 않는다. 에이전트가 물어본 질문에 예약해둔 딴소리를 답으로
/// 밀어넣는 꼴이 되기 때문이다 — 그 경우엔 사람이 먼저 답해야 한다.
///
/// `queued_turn`은 예약 시점에 관측한 turn_done 세대다. 이미 끝나 있던 턴으로 즉시
/// 발사되지 않게 **그보다 새 turn_done**을 요구한다.
pub fn followup_ready(queued_turn: Option<i64>, turn_done_now: Option<i64>, waiting: bool) -> bool {
    if waiting {
        return false;
    }
    match (queued_turn, turn_done_now) {
        (_, None) => false,
        (None, Some(_)) => true,
        (Some(before), Some(now)) => now > before,
    }
}

/// fleet 카드의 종류별 포커스 대상. PTY는 tab/pane으로 포커스하고 WriteInput
/// 브로드캐스트가 가능하지만, 구조화(App Server) 세션은 세션 id로 열고 브로드캐스트는
/// steer 경로라 대상이 아니다.
#[derive(Debug, Clone, PartialEq)]
pub enum FleetTarget {
    /// PTY 세션. `runtime::SessionId`는 워크스페이스마다 재사용될 수 있어 브로드캐스트
    /// 키는 항상 `(workspace_id, session)` 쌍이어야 한다(단독 사용 금지).
    Pty {
        session: runtime::SessionId,
        tab: runtime::MuxTabId,
        pane: runtime::MuxPaneId,
    },
    /// 구조화(App Server) 세션 — 관찰 + 열기만.
    Structured { session_id: String },
}

impl FleetSession {
    /// 브로드캐스트 대상 키 — PTY만 Some(구조화는 steer라 제외).
    pub fn broadcast_key(&self) -> Option<(String, runtime::SessionId)> {
        match &self.target {
            FleetTarget::Pty { session, .. } => Some((self.workspace_id.clone(), *session)),
            FleetTarget::Structured { .. } => None,
        }
    }
}

/// fleet 상태별 세션 수 총합. 작업 화면의 상단 요약 스트립에 쓴다.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FleetSummary {
    pub total: usize,
    /// 나를 막고 있는 수 — 작업 화면의 헤더 칩에 쓴다.
    pub blocked: usize,
    pub active: usize,
    pub errored: usize,
    pub finished: usize,
}

impl FleetSummary {
    /// 상태들을 세어 요약을 만든다.
    pub fn from_states(states: impl IntoIterator<Item = AgentVisualState>) -> Self {
        let mut s = Self::default();
        for state in states {
            s.total += 1;
            match session_group(state) {
                SessionGroup::Blocked => s.blocked += 1,
                SessionGroup::Active => s.active += 1,
                SessionGroup::Errored => s.errored += 1,
                SessionGroup::Finished => s.finished += 1,
            }
        }
        s
    }
}

/// 세션이 속한 묶음 — 화면 표시 순서와 같다.
///
/// 가장 중요한 구분은 **나를 막고 있느냐**다. 승인·입력 대기는 내가 답할 때까지
/// 에이전트가 놀지만(시간이 곧 비용), 오류·완료는 이미 끝난 결과라 늦게 봐도 손해가
/// 늘지 않는다. 그래서 12분 된 오류가 1분 40초 된 대기보다 **아래**다(2026-08-08).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SessionGroup {
    /// 나를 막고 있다 — 승인·입력 대기. 이 묶음만 FIFO로 정렬한다.
    Blocked,
    /// 돌고 있다.
    Active,
    /// 실패로 끝났다 — 나를 막지는 않는다.
    Errored,
    /// 끝났다.
    Finished,
}

impl SessionGroup {
    /// 표시 순서대로 — 막힌 것이 맨 위.
    pub const ORDER: [SessionGroup; 4] = [
        SessionGroup::Blocked,
        SessionGroup::Active,
        SessionGroup::Errored,
        SessionGroup::Finished,
    ];
}

pub fn session_group(state: AgentVisualState) -> SessionGroup {
    match state {
        AgentVisualState::Waiting | AgentVisualState::NeedsResponse => SessionGroup::Blocked,
        AgentVisualState::Active | AgentVisualState::Idle => SessionGroup::Active,
        AgentVisualState::Error => SessionGroup::Errored,
        AgentVisualState::Complete | AgentVisualState::Off => SessionGroup::Finished,
    }
}

/// 카드의 한 줄 작업 설명. 진행 중에는 최신 사용자 지시를 우선하고, 턴이
/// 끝난 뒤에는 마지막 에이전트 응답을 우선해 결과가 사라지지 않게 한다.
pub fn task_preview(
    state: AgentVisualState,
    instruction: Option<&str>,
    agent_summary: Option<&str>,
) -> Option<String> {
    let order = if matches!(
        state,
        AgentVisualState::Active | AgentVisualState::Waiting | AgentVisualState::NeedsResponse
    ) {
        [instruction, agent_summary]
    } else {
        [agent_summary, instruction]
    };
    let source = order
        .into_iter()
        .flatten()
        .find(|text| text.chars().any(|ch| !ch.is_whitespace()))?;
    let mut preview = String::with_capacity(164);
    let mut count = 0;
    for word in source.split_whitespace() {
        if count > 0 {
            if count == 160 {
                preview.push('…');
                return Some(preview);
            }
            preview.push(' ');
            count += 1;
        }
        for ch in word.chars() {
            if count == 160 {
                preview.push('…');
                return Some(preview);
            }
            preview.push(ch);
            count += 1;
        }
    }
    Some(preview)
}

/// A borrowed ordering for painting; session titles and task text stay in their source vector.
pub fn group_session_refs(sessions: &[FleetSession]) -> [Vec<&FleetSession>; 4] {
    let mut groups: [Vec<&FleetSession>; 4] = Default::default();
    for session in sessions {
        let slot = SessionGroup::ORDER
            .iter()
            .position(|group| *group == session_group(session.state))
            .expect("ORDER는 모든 묶음을 담는다");
        groups[slot].push(session);
    }
    for group in &mut groups {
        group.sort_by(|a, b| {
            a.workspace_name
                .cmp(&b.workspace_name)
                .then_with(|| a.title.cmp(&b.title))
        });
    }
    groups[0].sort_by_key(|s| s.blocked_since.unwrap_or(i64::MAX));
    groups
}

/// 묶음별로 나누고 각 묶음 안을 정렬한다.
///
/// 막힌 묶음만 **오래 막힌 순(FIFO)** 이다 — 굶는 항목이 없고, 정렬 키(막힌 시각)가
/// 화면에 그대로 보여 순서가 자명하다. 나머지는 기존대로 워크스페이스명→제목.
/// `blocked_since`가 없는 대기 세션(막 감지된 직후)은 맨 뒤로 보낸다.
#[cfg(test)]
pub fn group_sessions(sessions: Vec<FleetSession>) -> [Vec<FleetSession>; 4] {
    let mut groups: [Vec<FleetSession>; 4] = Default::default();
    for session in sessions {
        let slot = SessionGroup::ORDER
            .iter()
            .position(|group| *group == session_group(session.state))
            .expect("ORDER는 모든 묶음을 담는다");
        groups[slot].push(session);
    }
    for group in &mut groups {
        group.sort_by(|a, b| {
            a.workspace_name
                .cmp(&b.workspace_name)
                .then_with(|| a.title.cmp(&b.title))
        });
    }
    groups[0].sort_by_key(|s| s.blocked_since.unwrap_or(i64::MAX));
    groups
}

/// 막힌 시간을 사람이 읽는 짧은 표기로. `since`가 미래면(시계 되감김) "0초".
pub fn format_blocked_duration(now: i64, since: i64) -> String {
    let secs = now.saturating_sub(since).max(0);
    if secs < 60 {
        format!("{secs}초")
    } else if secs < 3_600 {
        format!("{}:{:02}", secs / 60, secs % 60)
    } else {
        format!("{}시간 {}분", secs / 3_600, (secs % 3_600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_card_names_the_current_user_task() {
        assert_eq!(
            task_preview(
                AgentVisualState::Active,
                Some("  Fix the folder tree\nthen review  "),
                Some("Old answer from a previous turn"),
            ),
            Some("Fix the folder tree then review".to_owned())
        );
    }

    #[test]
    fn finished_card_retains_the_last_agent_update() {
        assert_eq!(
            task_preview(
                AgentVisualState::Off,
                Some("Fix the folder tree"),
                Some("  Folder tree fix completed\nTests passed  "),
            ),
            Some("Folder tree fix completed Tests passed".to_owned())
        );
    }

    #[test]
    fn nav_summary_상태별_집계와_우선순위() {
        use AgentVisualState::*;
        let mut summary = FleetNavSummary::collect(
            [Waiting, NeedsResponse, Error, Active, Idle, Complete, Off]
                .into_iter()
                .enumerate()
                .map(|(id, state)| ("ws", runtime::SessionId(id as u64), state)),
            [],
            [],
            [],
        );
        assert_eq!(
            summary,
            FleetNavSummary {
                blocked: 2,
                errored: 1,
                running: 1,
                idle: 1,
                complete: 1,
            }
        );
        assert_eq!(summary.primary(), Some((Waiting, 2)));
        summary.blocked = 0;
        assert_eq!(summary.primary(), Some((Error, 1)));
        summary.errored = 0;
        assert_eq!(summary.primary(), Some((Active, 1)));
        summary.running = 0;
        assert_eq!(summary.primary(), Some((Complete, 1)));
        summary.complete = 0;
        assert_eq!(summary.primary(), Some((Idle, 1)));
        summary.idle = 0;
        assert_eq!(summary.primary(), None);
    }

    #[test]
    fn nav_summary_승인과_입력대기_중복제거는_워크스페이스별() {
        use AgentVisualState::*;
        let id = runtime::SessionId(1);
        let summary = FleetNavSummary::collect(
            [("a", id, Active), ("b", id, Active)],
            [("a", id)],
            [Some(("a", id)), Some(("a", id))],
            [],
        );
        assert_eq!(summary.blocked, 1);
        assert_eq!(
            summary.running, 1,
            "다른 프로젝트의 같은 세션 번호는 별개다"
        );
        assert_eq!(summary.primary(), Some((Waiting, 1)));
    }

    #[test]
    fn nav_summary_세션_없는_승인과_구조화_상태를_포함한다() {
        use AgentVisualState::*;
        let summary = FleetNavSummary::collect(
            [],
            [],
            [Some(("closed-pty", runtime::SessionId(7))), None],
            [Waiting, Active, Off],
        );
        assert_eq!(summary.blocked, 3);
        assert_eq!(summary.running, 1);
        assert_eq!(summary.complete, 0);
        assert_eq!(
            FleetNavSummary::collect([], [], [None], []).primary(),
            Some((Waiting, 1))
        );
        assert_eq!(FleetNavSummary::collect([], [], [], [Off]).primary(), None);
    }

    fn session(workspace: &str, title: &str, state: AgentVisualState) -> FleetSession {
        FleetSession {
            workspace_id: workspace.into(),
            workspace_name: workspace.into(),
            target: FleetTarget::Structured {
                session_id: format!("{workspace}-{title}"),
            },
            title: title.into(),
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

    fn blocked(workspace: &str, title: &str, since: i64) -> FleetSession {
        FleetSession {
            blocked_since: Some(since),
            ..session(workspace, title, AgentVisualState::Waiting)
        }
    }

    #[test]
    fn summary_묶음별_집계() {
        use AgentVisualState::*;
        let s = FleetSummary::from_states([Waiting, Waiting, Error, Complete, Active, Idle, Off]);
        assert_eq!(s.total, 7);
        assert_eq!(s.blocked, 2, "대기 2건");
        assert_eq!(s.errored, 1);
        assert_eq!(s.active, 2, "작업중 + 유휴");
        assert_eq!(s.finished, 2, "완료 + off");
    }

    #[test]
    fn summary_빈입력() {
        let s = FleetSummary::from_states([]);
        assert_eq!(s, FleetSummary::default());
    }

    /// 묶음의 핵심 계약 — 오류는 "나를 막는" 묶음이 아니다. 이미 끝난 결과라 기다린
    /// 시간이 비용이 아니므로 대기보다 아래여야 한다(2026-08-08 정렬 기준).
    #[test]
    fn 묶음은_나를_막느냐로_먼저_갈린다() {
        use AgentVisualState::*;
        assert_eq!(session_group(Waiting), SessionGroup::Blocked);
        assert_eq!(session_group(Active), SessionGroup::Active);
        assert_eq!(session_group(Idle), SessionGroup::Active);
        assert_eq!(session_group(Error), SessionGroup::Errored);
        assert_eq!(session_group(Complete), SessionGroup::Finished);
        assert_eq!(session_group(Off), SessionGroup::Finished);
        assert!(
            SessionGroup::Blocked < SessionGroup::Errored,
            "막힌 것이 오류보다 위여야 한다"
        );
        assert_eq!(SessionGroup::ORDER[0], SessionGroup::Blocked);
    }

    /// 막힌 묶음만 FIFO — 오래 막힌 것이 위. 나머지 묶음은 워크스페이스명→제목.
    #[test]
    fn 막힌_묶음은_오래된_순_나머지는_이름순() {
        let grouped = group_sessions(vec![
            blocked("beta", "새 대기", 300),
            blocked("alpha", "오래된 대기", 100),
            session("beta", "b", AgentVisualState::Active),
            session("alpha", "a", AgentVisualState::Active),
            session("x", "err", AgentVisualState::Error),
            session("y", "done", AgentVisualState::Complete),
        ]);
        let titles =
            |group: &Vec<FleetSession>| group.iter().map(|s| s.title.clone()).collect::<Vec<_>>();
        assert_eq!(
            titles(&grouped[0]),
            vec!["오래된 대기", "새 대기"],
            "막힌 묶음은 오래 막힌 순이어야 한다 — 이름순이 아니다"
        );
        assert_eq!(titles(&grouped[1]), vec!["a", "b"], "진행 중은 이름순");
        assert_eq!(titles(&grouped[2]), vec!["err"]);
        assert_eq!(titles(&grouped[3]), vec!["done"]);
    }

    /// 막 감지돼 시각이 아직 없는 대기 세션은 맨 뒤로 — 0으로 취급하면 오래 막힌 것을
    /// 제치고 맨 위로 올라간다.
    #[test]
    fn 시각을_모르는_대기는_맨_뒤로_간다() {
        let mut unknown = blocked("ws", "시각 없음", 0);
        unknown.blocked_since = None;
        let grouped = group_sessions(vec![unknown, blocked("ws", "오래됨", 100)]);
        assert_eq!(
            grouped[0]
                .iter()
                .map(|s| s.title.clone())
                .collect::<Vec<_>>(),
            vec!["오래됨", "시각 없음"]
        );
    }

    #[test]
    fn 막힌_시간_표기는_구간별로_바뀐다() {
        assert_eq!(format_blocked_duration(100, 100), "0초");
        assert_eq!(format_blocked_duration(159, 100), "59초");
        assert_eq!(format_blocked_duration(160, 100), "1:00");
        assert_eq!(format_blocked_duration(100 + 252, 100), "4:12");
        assert_eq!(format_blocked_duration(100 + 3_599, 100), "59:59");
        assert_eq!(format_blocked_duration(100 + 3_600, 100), "1시간 0분");
        assert_eq!(format_blocked_duration(100 + 7_500, 100), "2시간 5분");
        assert_eq!(
            format_blocked_duration(100, 500),
            "0초",
            "시계가 되감겨도 음수 표기가 나오면 안 된다"
        );
    }

    /// 멈춤 판정은 **작업 중**일 때만, 그리고 출력 시각을 아는 세션만 — 구조화 세션은
    /// PTY 스냅샷이 없어 항상 None이라 오탐이 없어야 한다.
    #[test]
    fn 멈춤은_작업중이면서_출력이_끊긴_세션만_잡는다() {
        let active_silent = FleetSession {
            state: AgentVisualState::Active,
            last_output_at: Some(1_000),
            ..session("ws", "t", AgentVisualState::Active)
        };
        assert_eq!(
            stuck_for(&active_silent, 1_000 + STUCK_AFTER_SECS),
            Some(STUCK_AFTER_SECS),
            "경계에서 잡혀야 한다"
        );
        assert_eq!(
            stuck_for(&active_silent, 1_000 + STUCK_AFTER_SECS - 1),
            None,
            "경계 직전은 멈춘 게 아니다"
        );

        // 대기·완료는 원래 출력이 없다 — 멈춤으로 부르면 안 된다.
        for state in [
            AgentVisualState::Waiting,
            AgentVisualState::Complete,
            AgentVisualState::Idle,
            AgentVisualState::Error,
        ] {
            let other = FleetSession {
                state,
                last_output_at: Some(0),
                ..session("ws", "t", state)
            };
            assert_eq!(
                stuck_for(&other, 100_000),
                None,
                "{state:?}는 대상이 아니다"
            );
        }

        // 출력 시각을 모르면(구조화 세션·갓 뜬 세션) 판정하지 않는다.
        let unknown = session("ws", "t", AgentVisualState::Active);
        assert_eq!(stuck_for(&unknown, 100_000), None);
    }

    /// 안전 불변식: 구조화 세션은 브로드캐스트 키를 절대 내지 않는다(steer 경로라
    /// WriteInput 대상 불가). FleetTarget/match를 미래에 바꿔도 이 회귀를 잡는다(리뷰 Low).
    #[test]
    fn 구조화_세션은_브로드캐스트_대상이_아니다() {
        let structured = session("ws", "t", AgentVisualState::Idle);
        assert_eq!(structured.broadcast_key(), None);
    }
    /// 발사 조건은 **turn_done 하나**다. 이 표의 두 줄이 각각 실제 사고를 막는다:
    /// turn_done 없이 발사하면 돌고 있는 턴 한가운데에 프롬프트가 들어가고, 입력 대기에
    /// 발사하면 에이전트가 물어본 질문에 딴소리가 답으로 들어간다.
    #[test]
    fn 예약은_턴이_끝났고_질문중이_아닐_때만_발사한다() {
        // (예약 시점 세대, 지금 turn_done, 입력 대기, 기대)
        let cases = [
            (
                None,
                None,
                false,
                false,
                "턴이 끝난 근거가 없으면 보내지 않는다",
            ),
            (
                None,
                Some(500),
                false,
                true,
                "예약 후 처음 끝난 턴에 보낸다",
            ),
            (
                None,
                Some(500),
                true,
                false,
                "질문으로 멈췄으면 사람이 먼저 답해야 한다",
            ),
            (
                Some(500),
                Some(500),
                false,
                false,
                "예약 직전에 이미 끝나 있던 턴으로 즉시 발사되면 안 된다",
            ),
            (
                Some(500),
                Some(501),
                false,
                true,
                "그 뒤에 새 턴이 끝나면 보낸다",
            ),
            (
                Some(500),
                None,
                false,
                false,
                "turn_done이 사라지면 근거가 없다",
            ),
        ];
        for (queued, now, waiting, expected, why) in cases {
            assert_eq!(followup_ready(queued, now, waiting), expected, "{why}");
        }
    }
}
