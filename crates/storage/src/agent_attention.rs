//! 질문 요청과 작업 상태를 독립적으로 보관한다. 대화 본문은 저장하지 않는다.
use super::*;
use serde::{Deserialize, Serialize};

const STATE_BYTES_MAX: usize = 32 * 1024;
const REQUESTS_MAX: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttentionEventKind {
    SessionStart,
    TurnStart,
    Working,
    ResponseRequired,
    ApprovalRequired,
    Resolved,
    Completed,
    IdleObserved,
    Cancelled,
    SessionEnd,
}

pub struct AgentAttentionEvent {
    pub native_session_id: String,
    pub turn_id: Option<String>,
    pub request_id: String,
    pub kind: AttentionEventKind,
    pub at_micros: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct AttentionState {
    native_session_id: String,
    generation: i64,
    last_activity: i64,
    turn_started: i64,
    completed_turn: Option<String>,
    #[serde(default)]
    current_turn: Option<String>,
    #[serde(default)]
    turn_cancelled: bool,
    requests: Vec<RequestState>,
    #[serde(default)]
    active_tools: Vec<(String, Option<String>)>,
    #[serde(default)]
    request_floor: i64,
}

#[derive(Serialize, Deserialize)]
struct RequestState {
    id: String,
    turn: Option<String>,
    at: i64,
    // 0 = 응답/취소됨, 1 = 응답 필요, 2 = 승인 필요.
    kind: u8,
    // 요청 ID를 주지 않는 MCP 서버는 아직 답하지 않은 요청 개수를 유지한다.
    #[serde(default = "one_request")]
    pending_count: u8,
}

fn one_request() -> u8 {
    1
}

impl Db {
    /// 요청 ID의 결과만 해제한다. 병렬 도구나 미확인 완료가 질문을 덮지 않는다.
    pub fn record_agent_attention(
        &self,
        key: &str,
        event: &AgentAttentionEvent,
    ) -> anyhow::Result<()> {
        use AttentionEventKind as K;
        let prefix = bounded_session_key_prefix(key)?;
        let valid_id =
            |s: &str| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control);
        anyhow::ensure!(
            valid_id(&event.native_session_id)
                && event.turn_id.as_deref().is_none_or(valid_id)
                && (event.request_id.is_empty() || valid_id(&event.request_id))
                && event.at_micros > 0,
            "agent attention input invalid"
        );
        if matches!(
            event.kind,
            K::ResponseRequired | K::ApprovalRequired | K::Resolved
        ) {
            anyhow::ensure!(
                valid_id(&event.request_id),
                "agent attention request missing"
            );
        }
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        // 길이를 먼저 제한하여 손상된 DB의 큰 JSON을 힙으로 읽지 않는다.
        let saved: Option<(Option<String>,bool,bool,i64,i64)> = tx.query_row(
            "SELECT CASE WHEN attention_json IS NULL THEN NULL
               WHEN typeof(attention_json)='text' AND length(CAST(attention_json AS BLOB)) <= ?2
               THEN attention_json ELSE 'invalid' END, working, turn_done, updated_at, attention_revision
             FROM agent_needs_input WHERE session_key=?1",(key, STATE_BYTES_MAX as i64),
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
        let (json, mut working, mut done, previous_at, revision) =
            saved.unwrap_or((None, false, false, 0, 0));
        let mut state: AttentionState = match json {
            Some(json) => serde_json::from_str(&json)
                .map_err(|_| anyhow::anyhow!("agent attention state invalid"))?,
            None => AttentionState::default(),
        };
        if event.kind == K::SessionStart {
            if event.at_micros < state.last_activity {
                return Ok(());
            }
            state = AttentionState {
                native_session_id: event.native_session_id.clone(),
                generation: event.at_micros,
                ..AttentionState::default()
            };
            working = false;
            done = false;
        } else if state.native_session_id.is_empty() {
            state.native_session_id.clone_from(&event.native_session_id);
        } else if state.native_session_id != event.native_session_id
            || event.at_micros < state.generation
        {
            return Ok(());
        }
        let current = event.kind == K::TurnStart
            || event.kind == K::SessionStart
            || event.kind == K::SessionEnd
            || event.turn_id.is_none()
            || state.current_turn.is_none()
            || event.turn_id == state.current_turn;
        let latest = current && event.at_micros >= state.last_activity;
        let mut new_completion = false;
        match event.kind {
            K::ResponseRequired | K::ApprovalRequired | K::Resolved => {
                let kind = match event.kind {
                    K::ResponseRequired => 1,
                    K::ApprovalRequired => 2,
                    _ => 0,
                };
                if let Some(request) = state.requests.iter_mut().find(|r| r.id == event.request_id)
                {
                    if event.at_micros >= request.at {
                        let anonymous = event.request_id.starts_with("elicitation:");
                        if anonymous && kind != 0 && request.kind != 0 {
                            if event.at_micros > request.at {
                                anyhow::ensure!(
                                    request.pending_count < REQUESTS_MAX as u8,
                                    "agent attention anonymous request limit"
                                );
                                request.pending_count += 1;
                            }
                        } else if anonymous && kind == 0 && request.pending_count > 1 {
                            request.pending_count -= 1;
                        } else {
                            request.kind = kind;
                            request.pending_count = u8::from(kind != 0);
                        }
                        request.turn.clone_from(&event.turn_id);
                        request.at = event.at_micros;
                    }
                } else {
                    if event.at_micros <= state.request_floor {
                        return Ok(());
                    }
                    if state.requests.len() >= REQUESTS_MAX
                        && let Some((i, _)) = state
                            .requests
                            .iter()
                            .enumerate()
                            .filter(|(_, r)| r.kind == 0)
                            .min_by_key(|(_, r)| r.at)
                    {
                        state.request_floor = state.request_floor.max(state.requests.remove(i).at);
                    }
                    anyhow::ensure!(
                        state.requests.len() < REQUESTS_MAX,
                        "agent attention request limit"
                    );
                    state.requests.push(RequestState {
                        id: event.request_id.clone(),
                        turn: event.turn_id.clone(),
                        at: event.at_micros,
                        kind,
                        pending_count: u8::from(kind != 0),
                    });
                }
                if kind == 0 {
                    let finished = state
                        .active_tools
                        .iter()
                        .any(|(id, _)| id == &event.request_id);
                    state.active_tools.retain(|(id, _)| id != &event.request_id);
                    // ID 없는 알림은 관찰된 병렬 도구가 전부 끝난 뒤에만 해제한다.
                    if finished && state.active_tools.is_empty() {
                        for r in &mut state.requests {
                            if r.id == "notification:permission" && r.at <= event.at_micros {
                                r.kind = 0;
                                r.at = event.at_micros;
                            }
                        }
                    }
                }
                if latest && kind != 0 {
                    done = false;
                }
            }
            K::TurnStart if latest => {
                working = true;
                done = false;
                state.turn_started = event.at_micros;
                state.current_turn.clone_from(&event.turn_id);
                state.turn_cancelled = false;
                state.completed_turn = None;
            }
            K::Working if latest => {
                state.completed_turn = None;
                working = true;
                done = false;
                if !event.request_id.is_empty()
                    && !state
                        .active_tools
                        .iter()
                        .any(|(id, _)| id == &event.request_id)
                {
                    anyhow::ensure!(
                        state.active_tools.len() < REQUESTS_MAX,
                        "agent attention active tool limit"
                    );
                    state
                        .active_tools
                        .push((event.request_id.clone(), event.turn_id.clone()));
                }
            }
            K::Completed if latest && !state.turn_cancelled => {
                working = false;
                state
                    .active_tools
                    .retain(|(_, turn)| event.turn_id.is_some() && turn != &event.turn_id);
                // 정상 턴 종료에는 실행 승인이 남지 않는다. 비동기 질문은 별도로 유지한다.
                for r in &mut state.requests {
                    if r.kind == 2 && (event.turn_id.is_none() || r.turn == event.turn_id) {
                        r.kind = 0;
                        r.at = event.at_micros;
                    }
                }
                let turn = event
                    .turn_id
                    .clone()
                    .unwrap_or_else(|| state.turn_started.to_string());
                if state.completed_turn.as_ref() != Some(&turn) {
                    done = true;
                    new_completion = true;
                    state.completed_turn = Some(turn);
                }
            }
            K::IdleObserved if latest => {
                working = false;
            }
            K::Cancelled | K::SessionEnd => {
                state.active_tools.retain(|(_, turn)| {
                    event.kind != K::SessionEnd && event.turn_id.is_some() && turn != &event.turn_id
                });
                for r in &mut state.requests {
                    if r.at <= event.at_micros
                        && (event.kind == K::SessionEnd
                            || event.turn_id.is_none()
                            || r.turn == event.turn_id)
                    {
                        r.kind = 0;
                        r.at = event.at_micros;
                    }
                }
                if latest {
                    working = false;
                    done = false;
                    state.turn_cancelled = true;
                }
            }
            _ => {}
        }
        if current {
            state.last_activity = state.last_activity.max(event.at_micros);
        }
        let response = state.requests.iter().any(|r| r.kind == 1);
        let approval = state.requests.iter().any(|r| r.kind == 2);
        let waiting = response || approval;
        let json = serde_json::to_string(&state)?;
        anyhow::ensure!(json.len() <= STATE_BYTES_MAX, "agent attention state limit");
        // 보관 시각은 초 단위를 유지하고 완료 소비에는 별도 단조 증가 세대를 쓴다.
        let now = deppy_core::time::unix_secs_i64();
        let updated = now.max(previous_at);
        tx.execute("INSERT INTO agent_needs_input
            (session_key,waiting,working,turn_done,updated_at,message,response_required,attention_json,attention_revision)
            VALUES (?1,?2,?3,?4,?5,NULL,?6,?7,?8)
            ON CONFLICT(session_key) DO UPDATE SET waiting=excluded.waiting,working=excluded.working,
            turn_done=excluded.turn_done,updated_at=excluded.updated_at,message=NULL,
            response_required=excluded.response_required,attention_json=excluded.attention_json,attention_revision=excluded.attention_revision",
            rusqlite::params![key,waiting,working,done,updated,response && !approval,json,if new_completion {revision.saturating_add(1).max(event.at_micros)}else{revision}])?;
        evict_hook_state_prefix_overflow(&tx, HookStateTable::NeedsInput, prefix, key)?;
        evict_hook_state_overflow(&tx, HookStateTable::NeedsInput, key)?;
        tx.commit()?;
        Ok(())
    }

    pub fn pending_agent_request_ids(
        &self,
        key: &str,
        native: &str,
    ) -> anyhow::Result<Vec<String>> {
        bounded_session_key_prefix(key)?;
        anyhow::ensure!(
            !native.is_empty() && native.len() <= 256,
            "agent attention native id invalid"
        );
        let json: Option<Option<String>>=self.conn.query_row(
            "SELECT CASE WHEN typeof(attention_json)='text' AND length(CAST(attention_json AS BLOB))<=?2
                THEN attention_json ELSE NULL END FROM agent_needs_input WHERE session_key=?1",
            (key,STATE_BYTES_MAX as i64),|r|r.get(0)).optional()?;
        let Some(json) = json.flatten() else {
            return Ok(Vec::new());
        };
        let state: AttentionState = serde_json::from_str(&json)
            .map_err(|_| anyhow::anyhow!("agent attention state invalid"))?;
        anyhow::ensure!(
            state.requests.len() <= REQUESTS_MAX,
            "agent attention request limit"
        );
        if state.native_session_id != native {
            return Ok(Vec::new());
        }
        Ok(state
            .requests
            .into_iter()
            .filter(|r| r.kind != 0)
            .map(|r| r.id)
            .collect())
    }

    #[cfg(test)]
    fn list_response_sessions(&self) -> anyhow::Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_key FROM agent_needs_input
            WHERE waiting=1 AND response_required=1 ORDER BY session_key LIMIT ?1",
        )?;
        let rows = stmt.query_map([WAITING_SESSION_ROWS_MAX as i64], |r| r.get(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn review_늦은_취소가_다시_열린_요청을_해제하지_않는다() {
        let db = Db::open_in_memory().unwrap();
        db.record_agent_attention(KEY, &event(AttentionEventKind::ResponseRequired, "q", 10))
            .unwrap();
        db.record_agent_attention(KEY, &event(AttentionEventKind::ResponseRequired, "q", 30))
            .unwrap();
        db.record_agent_attention(KEY, &event(AttentionEventKind::Cancelled, "q", 20))
            .unwrap();
        assert_eq!(db.list_waiting_sessions().unwrap().len(), 1);
    }
    use super::*;
    const KEY: &str = "00000000-0000-0000-0000-000000000001:1";

    #[test]
    fn review_아이디_없는_동일_서버_질문도_남은_개수를_보존한다() {
        let db = Db::open_in_memory().unwrap();
        for at in [1, 2] {
            db.record_agent_attention(
                KEY,
                &event(
                    AttentionEventKind::ResponseRequired,
                    "elicitation:server",
                    at,
                ),
            )
            .unwrap();
        }
        db.record_agent_attention(
            KEY,
            &event(AttentionEventKind::Resolved, "elicitation:server", 3),
        )
        .unwrap();
        assert_eq!(db.list_waiting_sessions().unwrap().len(), 1);
        db.record_agent_attention(
            KEY,
            &event(AttentionEventKind::Resolved, "elicitation:server", 4),
        )
        .unwrap();
        assert!(db.list_waiting_sessions().unwrap().is_empty());
    }

    #[test]
    fn review_재사용된_승인_별칭은_새_턴에서_취소할_수_있다() {
        let db = Db::open_in_memory().unwrap();
        db.record_agent_attention(
            KEY,
            &event(AttentionEventKind::ApprovalRequired, "alias", 1),
        )
        .unwrap();
        db.record_agent_attention(KEY, &event(AttentionEventKind::Resolved, "alias", 2))
            .unwrap();
        let mut next = event(AttentionEventKind::ApprovalRequired, "alias", 3);
        next.turn_id = Some("next".into());
        db.record_agent_attention(KEY, &next).unwrap();
        next.kind = AttentionEventKind::Cancelled;
        next.at_micros = 4;
        db.record_agent_attention(KEY, &next).unwrap();
        assert!(db.list_waiting_sessions().unwrap().is_empty());
    }

    #[test]
    fn review_stop_뒤_재개된_작업의_최종_완료를_보존한다() {
        let db = Db::open_in_memory().unwrap();
        for (kind, id, at) in [
            (AttentionEventKind::TurnStart, "", 1),
            (AttentionEventKind::Completed, "", 2),
            (AttentionEventKind::Working, "bash", 3),
            (AttentionEventKind::Resolved, "bash", 4),
            (AttentionEventKind::Completed, "", 5),
        ] {
            db.record_agent_attention(KEY, &event(kind, id, at))
                .unwrap();
        }
        assert_eq!(db.list_turn_done_sessions().unwrap().len(), 1);
    }

    #[test]
    fn review_이전_턴의_늦은_취소와_완료는_현재_작업을_바꾸지_않는다() {
        for kind in [AttentionEventKind::Cancelled, AttentionEventKind::Completed] {
            let db = Db::open_in_memory().unwrap();
            db.record_agent_attention(KEY, &event(AttentionEventKind::TurnStart, "", 1))
                .unwrap();
            let mut next = event(AttentionEventKind::TurnStart, "", 2);
            next.turn_id = Some("next".into());
            db.record_agent_attention(KEY, &next).unwrap();
            db.record_agent_attention(KEY, &event(kind, "", 3)).unwrap();
            assert_eq!(db.list_working_sessions().unwrap(), vec![KEY]);
            assert!(db.list_turn_done_sessions().unwrap().is_empty());
        }
    }

    #[test]
    fn review_유휴_알림은_동일_완료_세대를_갱신하지_않는다() {
        let db = Db::open_in_memory().unwrap();
        db.record_agent_attention(KEY, &event(AttentionEventKind::Completed, "", 1))
            .unwrap();
        let before = db.list_turn_done_sessions().unwrap();
        db.record_agent_attention(KEY, &event(AttentionEventKind::IdleObserved, "", 2))
            .unwrap();
        assert_eq!(db.list_turn_done_sessions().unwrap(), before);
    }

    #[test]
    fn review_정착_알림은_정상_턴만_완료하고_취소된_턴은_완료하지_않는다() {
        for cancelled in [false, true] {
            let db = Db::open_in_memory().unwrap();
            db.record_agent_attention(KEY, &event(AttentionEventKind::TurnStart, "", 1))
                .unwrap();
            if cancelled {
                db.record_agent_attention(KEY, &event(AttentionEventKind::Cancelled, "", 2))
                    .unwrap();
            }
            db.record_agent_attention(KEY, &event(AttentionEventKind::IdleObserved, "", 3))
                .unwrap();
            assert!(db.list_turn_done_sessions().unwrap().is_empty());
        }
    }

    fn event(kind: AttentionEventKind, id: &str, at: i64) -> AgentAttentionEvent {
        AgentAttentionEvent {
            native_session_id: "native-1".into(),
            request_id: id.into(),
            turn_id: Some("turn-1".into()),
            kind,
            at_micros: at,
        }
    }

    #[test]
    fn agent_attention_질문은_다른_도구와_완료에도_남고_같은_응답만_해제한다() {
        let db = Db::open_in_memory().unwrap();
        for (kind, id, at) in [
            (AttentionEventKind::SessionStart, "", 1),
            (AttentionEventKind::ResponseRequired, "q1", 2),
            (AttentionEventKind::Working, "bash", 3),
            (AttentionEventKind::Resolved, "bash", 4),
            (AttentionEventKind::Completed, "", 5),
        ] {
            db.record_agent_attention(KEY, &event(kind, id, at))
                .unwrap();
        }
        assert_eq!(db.list_waiting_sessions().unwrap().len(), 1);
        assert_eq!(db.list_response_sessions().unwrap(), vec![KEY]);
        db.record_agent_attention(KEY, &event(AttentionEventKind::Resolved, "q1", 6))
            .unwrap();
        assert!(db.list_waiting_sessions().unwrap().is_empty());
        assert_eq!(db.list_turn_done_sessions().unwrap().len(), 1);
    }

    #[test]
    fn agent_attention_오래된_질문시작과_이전_세션_결과는_무시한다() {
        let db = Db::open_in_memory().unwrap();
        db.record_agent_attention(KEY, &event(AttentionEventKind::SessionStart, "", 1))
            .unwrap();
        db.record_agent_attention(KEY, &event(AttentionEventKind::Resolved, "q1", 4))
            .unwrap();
        db.record_agent_attention(KEY, &event(AttentionEventKind::ResponseRequired, "q1", 2))
            .unwrap();
        assert!(db.list_waiting_sessions().unwrap().is_empty());
        let mut next = event(AttentionEventKind::SessionStart, "", 5);
        next.native_session_id = "native-2".into();
        db.record_agent_attention(KEY, &next).unwrap();
        db.record_agent_attention(KEY, &event(AttentionEventKind::ResponseRequired, "q2", 6))
            .unwrap();
        assert!(db.list_waiting_sessions().unwrap().is_empty());
    }

    #[test]
    fn agent_attention_질문과_승인은_독립적이고_취소는_해당_턴만_해제한다() {
        let db = Db::open_in_memory().unwrap();
        db.record_agent_attention(KEY, &event(AttentionEventKind::ResponseRequired, "q1", 1))
            .unwrap();
        let mut approval = event(AttentionEventKind::ApprovalRequired, "a1", 2);
        approval.turn_id = Some("turn-2".into());
        db.record_agent_attention(KEY, &approval).unwrap();
        db.record_agent_attention(KEY, &event(AttentionEventKind::Cancelled, "", 3))
            .unwrap();
        assert!(db.list_response_sessions().unwrap().is_empty());
        assert_eq!(db.list_waiting_sessions().unwrap().len(), 1);
    }

    #[test]
    fn agent_attention_이전_완료_확인은_같은_초의_새_완료를_지우지_않는다() {
        let db = Db::open_in_memory().unwrap();
        db.record_agent_attention(KEY, &event(AttentionEventKind::Completed, "", 1))
            .unwrap();
        let old = db.list_turn_done_sessions().unwrap()[0].1;
        let mut next = event(AttentionEventKind::TurnStart, "", 2);
        next.turn_id = Some("turn-2".into());
        db.record_agent_attention(KEY, &next).unwrap();
        next.kind = AttentionEventKind::Completed;
        next.at_micros = 3;
        db.record_agent_attention(KEY, &next).unwrap();
        db.clear_agent_turn_done(KEY, old).unwrap();
        assert_eq!(db.list_turn_done_sessions().unwrap().len(), 1);
    }

    #[test]
    fn agent_attention_도구_아이디가_없는_승인은_관련_도구들이_끝난_뒤_해제한다() {
        let db = Db::open_in_memory().unwrap();
        for (kind, id, at) in [
            (AttentionEventKind::Working, "a", 1),
            (AttentionEventKind::Working, "b", 2),
            (
                AttentionEventKind::ApprovalRequired,
                "notification:permission",
                3,
            ),
            (AttentionEventKind::Resolved, "a", 4),
        ] {
            db.record_agent_attention(KEY, &event(kind, id, at))
                .unwrap();
        }
        assert_eq!(db.list_waiting_sessions().unwrap().len(), 1);
        db.record_agent_attention(KEY, &event(AttentionEventKind::Resolved, "b", 5))
            .unwrap();
        assert!(db.list_waiting_sessions().unwrap().is_empty());
    }
}
