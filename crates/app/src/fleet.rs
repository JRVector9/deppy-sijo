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

// PR-4는 뷰모델 타입·로직만이다. App 프로젝션·그리드 UI(실사용)는 PR-5에서 붙는다 —
// 그전까지 미사용 항목이 있어 경고를 억제한다(prompt_library PR-1 선례와 동일 정책).
#![allow(dead_code)]

use crate::agent_surface::AgentVisualState;

/// fleet 그리드의 세션 행 하나. App이 active/warm 런타임의 여러 필드에서 조립한
/// 읽기전용 스냅샷이다(leaf+intent+host I/O 경계: UI는 이 스냅샷만 그린다).
#[derive(Debug, Clone, PartialEq)]
pub struct FleetSession {
    pub workspace_id: String,
    pub workspace_name: String,
    /// 세션 id. `runtime::SessionId`는 워크스페이스마다 재사용될 수 있어 fleet 맵의
    /// 키는 항상 `(workspace_id, session)` 쌍이어야 한다(단독 사용 금지).
    pub session: runtime::SessionId,
    /// 포커스 라우팅용 tab/pane(그리드 카드 클릭 → 해당 세션으로 전환).
    pub tab: runtime::MuxTabId,
    pub pane: runtime::MuxPaneId,
    pub title: String,
    /// 정규화된 시각 상태(agent_surface). needs-input/승인/오류/완료/작업중/유휴/off.
    pub state: AgentVisualState,
    /// 에이전트 2행 "Codex · gpt-5.5 · xhigh"(에이전트일 때만 Some).
    pub agent_line: Option<String>,
    /// hook이 보고한 대기 사유(needs-input 메시지). Waiting 상태에서만 대개 Some.
    pub waiting_message: Option<String>,
    /// active 워크스페이스의 세션인지(그 외는 warm — 물러났지만 워커는 실행 중).
    pub active_workspace: bool,
}

/// fleet 상태별 세션 수 총합. 상단 요약 스트립·배지에 쓴다.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FleetSummary {
    pub total: usize,
    /// 사용자 입력·승인 대기(가장 주목 필요).
    pub waiting: usize,
    pub error: usize,
    /// 턴 완료 — 검토 대기.
    pub done: usize,
    /// 작업 중.
    pub working: usize,
    pub idle: usize,
    /// 상태 미보고(off).
    pub off: usize,
}

impl FleetSummary {
    /// 상태들을 세어 요약을 만든다.
    pub fn from_states(states: impl IntoIterator<Item = AgentVisualState>) -> Self {
        let mut s = Self::default();
        for state in states {
            s.total += 1;
            match state {
                AgentVisualState::Waiting => s.waiting += 1,
                AgentVisualState::Error => s.error += 1,
                AgentVisualState::Complete => s.done += 1,
                AgentVisualState::Active => s.working += 1,
                AgentVisualState::Idle => s.idle += 1,
                AgentVisualState::Off => s.off += 1,
            }
        }
        s
    }

    /// 주목이 필요한 세션 수(대기 + 오류) — nav 배지용.
    pub fn attention(&self) -> usize {
        self.waiting + self.error
    }
}

/// 정렬 우선순위. 값이 클수록 그리드 앞(주목 필요 순). 대기 > 오류 > 완료 > 작업중 >
/// 유휴 > off. "지금 나를 필요로 하는 에이전트"를 좌상단에 모으는 게 fleet 뷰의 목적.
pub fn fleet_urgency(state: AgentVisualState) -> u8 {
    match state {
        AgentVisualState::Waiting => 5,
        AgentVisualState::Error => 4,
        AgentVisualState::Complete => 3,
        AgentVisualState::Active => 2,
        AgentVisualState::Idle => 1,
        AgentVisualState::Off => 0,
    }
}

/// fleet 세션을 주목도 순으로 정렬한다(우선순위 내림차순, 동순위는 워크스페이스명→제목).
/// 안정 정렬이라 같은 키의 상대 순서는 입력(워크스페이스·pane 순회 순)을 보존한다.
pub fn sort_sessions(sessions: &mut [FleetSession]) {
    sessions.sort_by(|a, b| {
        fleet_urgency(b.state)
            .cmp(&fleet_urgency(a.state))
            .then_with(|| a.workspace_name.cmp(&b.workspace_name))
            .then_with(|| a.title.cmp(&b.title))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_상태별_집계와_주목수() {
        use AgentVisualState::*;
        let s = FleetSummary::from_states([
            Waiting, Waiting, Error, Complete, Active, Idle, Off,
        ]);
        assert_eq!(s.total, 7);
        assert_eq!(s.waiting, 2);
        assert_eq!(s.error, 1);
        assert_eq!(s.done, 1);
        assert_eq!(s.working, 1);
        assert_eq!(s.idle, 1);
        assert_eq!(s.off, 1);
        assert_eq!(s.attention(), 3); // waiting 2 + error 1
    }

    #[test]
    fn summary_빈입력() {
        let s = FleetSummary::from_states([]);
        assert_eq!(s, FleetSummary::default());
        assert_eq!(s.attention(), 0);
    }

    #[test]
    fn urgency_순서_대기가_최상_off가_최하() {
        use AgentVisualState::*;
        assert!(fleet_urgency(Waiting) > fleet_urgency(Error));
        assert!(fleet_urgency(Error) > fleet_urgency(Complete));
        assert!(fleet_urgency(Complete) > fleet_urgency(Active));
        assert!(fleet_urgency(Active) > fleet_urgency(Idle));
        assert!(fleet_urgency(Idle) > fleet_urgency(Off));
    }
}
