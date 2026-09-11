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
    pub tool_group: Option<String>,
    pub tool_input_hash: Option<String>,
    pub child_id: Option<String>,
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
    tool_groups: Vec<(String, String, Option<String>)>,
    #[serde(default)]
    request_floor: i64,
    #[serde(default)]
    terminated_turns: Vec<TurnTermination>,
    #[serde(default)]
    termination_floor: i64,
    #[serde(default)]
    ended: bool,
}

impl AttentionState {
    fn prune_anonymous_history(&mut self) {
        // 정리한 경계까지의 양수 잔여만 이월한다. 짝이 없는 과거 결과는 이월하지 않는다.
        while self
            .requests
            .iter()
            .filter_map(|r| r.anonymous.as_ref())
            .map(|counts| counts.seen.len())
            .sum::<usize>()
            > REQUESTS_MAX * 4
        {
            let oldest = self
                .requests
                .iter()
                .enumerate()
                .flat_map(|(r, request)| {
                    request.anonymous.iter().flat_map(move |counts| {
                        counts
                            .seen
                            .iter()
                            .enumerate()
                            .map(move |(i, (at, _))| (r, i, *at))
                    })
                })
                .min_by_key(|(_, _, at)| *at);
            let Some((r, i, at)) = oldest else { break };
            if let Some(counts) = self.requests[r].anonymous.as_mut() {
                counts.fold_oldest(i, at);
            }
            self.requests[r].refresh_anonymous();
        }
    }
}

#[derive(Serialize, Deserialize)]
struct TurnTermination {
    owner: Option<String>,
    turn: Option<String>,
    at: i64,
}

// 부모와 자식의 요청 ID 공간을 나눈다. 같은 이름의 도구도 서로 해제하지 않는다.
fn request_owner(id: &str) -> Option<&str> {
    let (owner, _) = id.strip_prefix("child:")?.split_once(':')?;
    Some(&id[..6 + owner.len()])
}

fn anonymous_request(id: &str) -> bool {
    let base = request_owner(id).map_or(id, |owner| &id[owner.len() + 1..]);
    base.starts_with("elicitation:")
}

#[derive(Serialize, Deserialize)]
struct AnonymousRequests {
    balance: i16,
    seen: Vec<(i64, u8)>,
    floor: i64,
    #[serde(default)]
    baseline: Option<u8>,
}

impl AnonymousRequests {
    fn baseline(&mut self) -> u8 {
        *self.baseline.get_or_insert_with(|| {
            // 이전 버전의 창 밖 양수 잔여는 보존하고 음수 부채는 버린다.
            let delta: i16 = self
                .seen
                .iter()
                .map(|(_, k)| if *k == 0 { -1 } else { 1 })
                .sum();
            (self.balance - delta).clamp(0, REQUESTS_MAX as i16) as u8
        })
    }

    fn recount(&mut self) {
        let mut pending = self.baseline() as i16;
        self.seen.sort_unstable();
        for (_, kind) in &self.seen {
            pending = if *kind == 0 {
                (pending - 1).max(0)
            } else {
                pending + 1
            };
        }
        self.balance = pending;
    }

    fn fold_oldest(&mut self, index: usize, at: i64) {
        let baseline = self.baseline();
        let (_, kind) = self.seen.remove(index);
        self.baseline = Some(if kind == 0 {
            baseline.saturating_sub(1)
        } else {
            baseline.saturating_add(1)
        });
        self.floor = self.floor.max(at);
        self.recount();
    }
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
    #[serde(default)]
    anonymous: Option<AnonymousRequests>,
    #[serde(default)]
    tool_group: Option<String>,
    #[serde(default)]
    tool_input_hash: Option<String>,
    #[serde(default)]
    child_id: Option<String>,
}

impl RequestState {
    fn refresh_anonymous(&mut self) {
        if let Some(counts) = &mut self.anonymous {
            counts.recount();
            self.kind = u8::from(counts.balance > 0);
            self.pending_count = counts.balance.max(0) as u8;
        }
    }

    fn observe_anonymous(&mut self, kind: u8, at: i64) -> anyhow::Result<()> {
        let counts = self.anonymous.get_or_insert_with(|| AnonymousRequests {
            balance: if self.kind == 0 {
                0
            } else {
                self.pending_count as i16
            },
            seen: Vec::new(),
            floor: self.at,
            baseline: Some(if self.kind == 0 {
                0
            } else {
                self.pending_count
            }),
        });
        if at <= counts.floor || counts.seen.contains(&(at, kind)) {
            return Ok(());
        }
        counts.baseline();
        counts.seen.push((at, kind));
        counts.recount();
        anyhow::ensure!(
            counts.balance <= REQUESTS_MAX as i16,
            "agent attention anonymous request limit"
        );
        if counts.seen.len() > REQUESTS_MAX * 2 {
            counts.fold_oldest(0, counts.seen[0].0);
        }
        self.refresh_anonymous();
        self.at = self.at.max(at);
        Ok(())
    }
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
                && event.tool_group.as_deref().is_none_or(valid_id)
                && event
                    .tool_input_hash
                    .as_deref()
                    .is_none_or(|s| s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                && event.child_id.as_deref().is_none_or(|id| {
                    id.len() <= 128
                        && !id.is_empty()
                        && id
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                })
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
        if state.ended && event.kind != K::SessionStart {
            return Ok(());
        }
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
        let owner = request_owner(&event.request_id);
        let current = owner.is_none()
            && (event.kind == K::TurnStart
                || event.kind == K::SessionStart
                || event.kind == K::SessionEnd
                || event.turn_id.is_none()
                || state.current_turn.is_none()
                || event.turn_id == state.current_turn);
        let latest = current && event.at_micros >= state.last_activity;
        let mut new_completion = false;
        if matches!(
            event.kind,
            K::Working | K::ResponseRequired | K::ApprovalRequired | K::Resolved
        ) && (event.at_micros <= state.termination_floor
            || state.terminated_turns.iter().any(|end| {
                end.owner.as_deref() == owner
                    && (end.turn.is_none() || end.turn == event.turn_id)
                    && (event.at_micros <= end.at
                        || (end.turn.is_some()
                            && (end.turn != state.current_turn || state.turn_cancelled)))
            }))
        {
            return Ok(());
        }
        match event.kind {
            K::ResponseRequired | K::ApprovalRequired | K::Resolved => {
                let kind = match event.kind {
                    K::ResponseRequired => 1,
                    K::ApprovalRequired => 2,
                    _ => 0,
                };
                if kind == 2
                    && let Some(group) = &event.tool_group
                    && !state.tool_groups.iter().any(|(_, active, input)| {
                        active == group
                            && (event.tool_input_hash.is_none() || input == &event.tool_input_hash)
                    })
                    && state.requests.iter().any(|r| {
                        r.kind == 0
                            && r.tool_group.as_ref() == Some(group)
                            && r.at >= event.at_micros
                    })
                {
                    // 결과보다 늦게 저장된 과거 승인 훅은 이미 끝난 실행을 다시 열지 않는다.
                    return Ok(());
                }
                let mut finished_approval = kind == 0
                    && state.requests.iter().any(|r| {
                        r.id == event.request_id && r.kind == 2 && r.at <= event.at_micros
                    });
                let finished_request = kind == 0
                    && state.requests.iter().any(|r| {
                        r.id == event.request_id && r.kind != 0 && r.at <= event.at_micros
                    });
                if let Some(request) = state.requests.iter_mut().find(|r| r.id == event.request_id)
                {
                    if anonymous_request(&event.request_id) {
                        if kind != 0 && event.at_micros >= request.at {
                            request.turn.clone_from(&event.turn_id);
                        }
                        request.observe_anonymous(kind, event.at_micros)?;
                    } else if event.at_micros >= request.at {
                        request.kind = kind;
                        request.pending_count = u8::from(kind != 0);
                        request.turn.clone_from(&event.turn_id);
                        request.at = event.at_micros;
                        if kind != 0 || event.tool_group.is_some() {
                            request.tool_group.clone_from(&event.tool_group);
                            request.tool_input_hash.clone_from(&event.tool_input_hash);
                        }
                        if kind != 0 || event.child_id.is_some() {
                            request.child_id.clone_from(&event.child_id);
                        }
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
                        tool_group: event.tool_group.clone(),
                        tool_input_hash: event.tool_input_hash.clone(),
                        child_id: event.child_id.clone(),
                        anonymous: anonymous_request(&event.request_id).then(|| {
                            AnonymousRequests {
                                balance: i16::from(kind != 0),
                                seen: vec![(event.at_micros, kind)],
                                floor: 0,
                                baseline: Some(0),
                            }
                        }),
                    });
                }
                if kind == 0 {
                    let finished = finished_request
                        || state
                            .active_tools
                            .iter()
                            .any(|(id, _)| id == &event.request_id);
                    let finished_group = state
                        .tool_groups
                        .iter()
                        .find(|(id, _, _)| id == &event.request_id)
                        .map(|(_, group, input)| (group.clone(), input.clone()));
                    state.active_tools.retain(|(id, _)| id != &event.request_id);
                    state
                        .tool_groups
                        .retain(|(id, _, _)| id != &event.request_id);
                    if let Some((group, input)) = &finished_group
                        && let Some(result) =
                            state.requests.iter_mut().find(|r| r.id == event.request_id)
                    {
                        result.tool_group = Some(group.clone());
                        result.tool_input_hash = input.clone();
                    }
                    // 동일 입력의 병렬 호출은 모두 결과가 있어야 닫는다. 다른 입력의
                    // 자동 승인 호출은 붙잡지 않으며, 입력이 바뀐 훅은 도구 그룹으로 보완한다.
                    if let Some((group, input)) = finished_group {
                        for request in &mut state.requests {
                            let base = request_owner(&request.id)
                                .map_or(request.id.as_str(), |o| &request.id[o.len() + 1..]);
                            let matching_input = request.tool_input_hash.is_some()
                                && (request.tool_input_hash == input
                                    || state.tool_groups.iter().any(|(_, g, h)| {
                                        *g == group && *h == request.tool_input_hash
                                    }));
                            let remaining = state.tool_groups.iter().any(|(_, g, h)| {
                                *g == group && (!matching_input || *h == request.tool_input_hash)
                            });
                            if base.starts_with("tool:")
                                && request.tool_group.as_ref() == Some(&group)
                                && !remaining
                                && request.at <= event.at_micros
                            {
                                finished_approval |= request.kind == 2;
                                request.kind = 0;
                                request.pending_count = 0;
                                request.at = event.at_micros;
                            }
                        }
                    }
                    let notification = owner.map_or_else(
                        || "notification:permission".to_owned(),
                        |owner| format!("{owner}:notification:permission"),
                    );
                    // 구체적인 승인을 해제했다면 무관한 자동 실행이 알림을 붙잡지 않는다.
                    // 알림만 있던 경로는 기존처럼 해당 소유자의 모든 실행 결과를 기다린다.
                    if finished
                        && (finished_approval
                            || !state
                                .active_tools
                                .iter()
                                .any(|(id, _)| request_owner(id) == owner))
                        && !state.requests.iter().any(|r| {
                            r.kind == 2 && r.id != notification && request_owner(&r.id) == owner
                        })
                    {
                        for r in &mut state.requests {
                            if r.id == notification && r.at <= event.at_micros {
                                r.kind = 0;
                                r.at = event.at_micros;
                            }
                        }
                    }
                }
                if latest && kind != 0 {
                    done = false;
                    state.completed_turn = None;
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
            K::Working if latest || owner.is_some() => {
                if latest {
                    state.completed_turn = None;
                    working = true;
                    done = false;
                }
            }
            K::Completed if latest && !state.turn_cancelled => {
                working = false;
                state.active_tools.retain(|(id, turn)| {
                    request_owner(id).is_some()
                        || (event.turn_id.is_some() && turn != &event.turn_id)
                });
                // 정상 턴 종료에는 실행 승인이 남지 않는다. 비동기 질문은 별도로 유지한다.
                for r in &mut state.requests {
                    if r.kind == 2
                        && request_owner(&r.id).is_none()
                        && (event.turn_id.is_none() || r.turn == event.turn_id)
                    {
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
                if event.kind == K::SessionEnd {
                    state.ended = true;
                }
                if let Some(end) = state
                    .terminated_turns
                    .iter_mut()
                    .find(|end| end.owner.as_deref() == owner && end.turn == event.turn_id)
                {
                    end.at = end.at.max(event.at_micros);
                } else {
                    if state.terminated_turns.len() >= REQUESTS_MAX {
                        let oldest = state
                            .terminated_turns
                            .iter()
                            .enumerate()
                            .min_by_key(|(_, end)| end.at)
                            .map(|(i, _)| i)
                            .unwrap();
                        state.termination_floor = state
                            .termination_floor
                            .max(state.terminated_turns.remove(oldest).at);
                    }
                    state.terminated_turns.push(TurnTermination {
                        owner: owner.map(str::to_owned),
                        turn: event.turn_id.clone(),
                        at: event.at_micros,
                    });
                }
                state.active_tools.retain(|(id, turn)| {
                    event.kind != K::SessionEnd
                        && (request_owner(id) != owner
                            || (event.turn_id.is_some() && turn != &event.turn_id))
                });
                for r in &mut state.requests {
                    if r.at <= event.at_micros
                        && (event.kind == K::SessionEnd
                            || (request_owner(&r.id) == owner
                                && (event.turn_id.is_none() || r.turn == event.turn_id)))
                    {
                        r.kind = 0;
                        r.at = event.at_micros;
                        r.pending_count = 0;
                        r.anonymous = None;
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
        if ((event.kind == K::ResponseRequired && event.tool_group.is_some())
            || (event.kind == K::Working && (latest || owner.is_some())))
            && !event.request_id.is_empty()
            && !state
                .requests
                .iter()
                .any(|r| r.id == event.request_id && r.kind == 0 && r.at >= event.at_micros)
        {
            if !state
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
            if let Some(group) = &event.tool_group
                && !state
                    .tool_groups
                    .iter()
                    .any(|(id, _, _)| id == &event.request_id)
            {
                state.tool_groups.push((
                    event.request_id.clone(),
                    group.clone(),
                    event.tool_input_hash.clone(),
                ));
            }
        }
        state
            .tool_groups
            .retain(|(id, _, _)| state.active_tools.iter().any(|(active, _)| active == id));
        if current {
            state.last_activity = state.last_activity.max(event.at_micros);
        }
        state.prune_anonymous_history();
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
        self.agent_request_ids(key, native, false)
    }

    /// 승인이 남아 있을 때는 결과 훅을 생략한 일반 도구도 해제 근거에 포함한다.
    pub fn agent_result_candidate_ids(
        &self,
        key: &str,
        native: &str,
    ) -> anyhow::Result<Vec<String>> {
        self.agent_request_ids(key, native, true)
    }

    fn agent_request_ids(
        &self,
        key: &str,
        native: &str,
        include_tools: bool,
    ) -> anyhow::Result<Vec<String>> {
        let Some(state) = self.agent_attention_state(key, native)? else {
            return Ok(Vec::new());
        };
        let mut ids: Vec<String> = state
            .requests
            .into_iter()
            .filter(|r| r.kind != 0)
            .map(|r| r.id)
            .collect();
        if include_tools && !ids.is_empty() {
            for (id, _) in state.active_tools {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        Ok(ids)
    }

    fn agent_attention_state(
        &self,
        key: &str,
        native: &str,
    ) -> anyhow::Result<Option<AttentionState>> {
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
            return Ok(None);
        };
        let state: AttentionState = serde_json::from_str(&json)
            .map_err(|_| anyhow::anyhow!("agent attention state invalid"))?;
        anyhow::ensure!(
            state.requests.len() <= REQUESTS_MAX && state.active_tools.len() <= REQUESTS_MAX,
            "agent attention request limit"
        );
        if state.native_session_id != native {
            return Ok(None);
        }
        Ok(Some(state))
    }

    /// 부모 훅에서 결과를 확인할 미응답 자식만 반환한다. 경로와 대화 본문은 저장하지 않는다.
    pub fn pending_agent_children(&self, key: &str, native: &str) -> anyhow::Result<Vec<String>> {
        let Some(state) = self.agent_attention_state(key, native)? else {
            return Ok(Vec::new());
        };
        let mut children = Vec::new();
        for r in state.requests.iter().filter(|r| r.kind != 0) {
            if let Some(id) = &r.child_id
                && !children.contains(id)
            {
                children.push(id.clone());
            }
        }
        Ok(children)
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
    fn fix5_취소_후_늦은_익명_결과가_다음_질문을_삼키지_않는다() {
        let db = Db::open_in_memory().unwrap();
        for (kind, at) in [
            (AttentionEventKind::ResponseRequired, 1),
            (AttentionEventKind::Cancelled, 2),
            (AttentionEventKind::Resolved, 3),
            (AttentionEventKind::TurnStart, 4),
            (AttentionEventKind::ResponseRequired, 5),
        ] {
            let mut e = event(kind, "elicitation:s", at);
            e.turn_id = None;
            db.record_agent_attention(KEY, &e).unwrap();
        }
        assert_eq!(db.list_waiting_sessions().unwrap().len(), 1);
    }

    #[test]
    fn fix5_정리된_익명_결과는_새_질문을_상쇄하지_않는다() {
        let db = Db::open_in_memory().unwrap();
        db.record_agent_attention(
            KEY,
            &event(AttentionEventKind::Resolved, "elicitation:a", 2),
        )
        .unwrap();
        for i in 0..128 {
            let id = format!("elicitation:{}", i / 64);
            db.record_agent_attention(
                KEY,
                &event(AttentionEventKind::ResponseRequired, &id, 10 + i * 2),
            )
            .unwrap();
            db.record_agent_attention(KEY, &event(AttentionEventKind::Resolved, &id, 11 + i * 2))
                .unwrap();
        }
        db.record_agent_attention(
            KEY,
            &event(AttentionEventKind::ResponseRequired, "elicitation:a", 1),
        )
        .unwrap();
        db.record_agent_attention(
            KEY,
            &event(AttentionEventKind::ResponseRequired, "elicitation:a", 1000),
        )
        .unwrap();
        assert_eq!(db.list_waiting_sessions().unwrap().len(), 1);
    }

    #[test]
    fn fix7_자식의_도구와_승인_알림은_부모_상태를_바꾸지_않고_해제한다() {
        let db = Db::open_in_memory().unwrap();
        for (kind, id, at) in [
            (AttentionEventKind::Completed, "", 1),
            (AttentionEventKind::Working, "child:a:tool", 2),
            (
                AttentionEventKind::ApprovalRequired,
                "child:a:notification:permission",
                3,
            ),
            (AttentionEventKind::ApprovalRequired, "child:a:q", 4),
            (AttentionEventKind::Resolved, "child:a:tool", 5),
        ] {
            db.record_agent_attention(KEY, &event(kind, id, at))
                .unwrap();
        }
        assert_eq!(
            db.pending_agent_request_ids(KEY, "native-1").unwrap().len(),
            2
        );
        db.record_agent_attention(KEY, &event(AttentionEventKind::Resolved, "child:a:q", 6))
            .unwrap();
        assert!(db.list_waiting_sessions().unwrap().is_empty());
        assert_eq!(db.list_turn_done_sessions().unwrap().len(), 1);
        assert!(db.list_working_sessions().unwrap().is_empty());
    }

    #[test]
    fn fix7_여러_익명_서버를_오래_써도_기록_한도로_갱신이_막히지_않는다() {
        let db = Db::open_in_memory().unwrap();
        let mut at = 1_789_080_000_000_000;
        for server in 0..16 {
            for _ in 0..70 {
                for kind in [
                    AttentionEventKind::ResponseRequired,
                    AttentionEventKind::Resolved,
                ] {
                    at += 1;
                    db.record_agent_attention(
                        KEY,
                        &event(kind, &format!("elicitation:{server}"), at),
                    )
                    .unwrap();
                }
            }
        }
        assert!(db.list_waiting_sessions().unwrap().is_empty());
    }

    #[test]
    fn fix7_익명_질문을_새_턴에서_재사용한_후에도_취소할_수_있다() {
        let db = Db::open_in_memory().unwrap();
        for (kind, at) in [
            (AttentionEventKind::ResponseRequired, 1),
            (AttentionEventKind::Resolved, 2),
        ] {
            db.record_agent_attention(KEY, &event(kind, "elicitation:s", at))
                .unwrap();
        }
        for (kind, at) in [
            (AttentionEventKind::TurnStart, 3),
            (AttentionEventKind::ResponseRequired, 4),
            (AttentionEventKind::Cancelled, 5),
        ] {
            let mut e = event(kind, "elicitation:s", at);
            e.turn_id = Some("next".into());
            db.record_agent_attention(KEY, &e).unwrap();
        }
        assert!(db.list_waiting_sessions().unwrap().is_empty());
    }

    #[test]
    fn second_review_취소_전_발생한_질문이_늦게_저장되어도_부활하지_않는다() {
        let db = Db::open_in_memory().unwrap();
        for (k, id, at) in [
            (AttentionEventKind::TurnStart, "", 1),
            (AttentionEventKind::Cancelled, "", 3),
            (AttentionEventKind::TurnStart, "", 4),
            (AttentionEventKind::ResponseRequired, "late", 2),
        ] {
            db.record_agent_attention(KEY, &event(k, id, at)).unwrap();
        }
        assert!(db.list_waiting_sessions().unwrap().is_empty());
    }

    #[test]
    fn second_review_익명_질문의_모든_저장_순서와_재전송을_견딘다() {
        let events = [
            event(AttentionEventKind::ResponseRequired, "elicitation:s", 1),
            event(AttentionEventKind::ResponseRequired, "elicitation:s", 2),
            event(AttentionEventKind::Resolved, "elicitation:s", 3),
        ];
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let db = Db::open_in_memory().unwrap();
            for i in order {
                db.record_agent_attention(KEY, &events[i]).unwrap();
            }
            for i in order {
                db.record_agent_attention(KEY, &events[i]).unwrap();
            }
            assert_eq!(db.list_waiting_sessions().unwrap().len(), 1, "{order:?}");
            db.record_agent_attention(
                KEY,
                &event(AttentionEventKind::Resolved, "elicitation:s", 4),
            )
            .unwrap();
            assert!(db.list_waiting_sessions().unwrap().is_empty(), "{order:?}");
        }
    }

    #[test]
    fn second_review_질문과_승인으로_재개한_턴도_최종_완료를_남긴다() {
        for k in [
            AttentionEventKind::ResponseRequired,
            AttentionEventKind::ApprovalRequired,
        ] {
            let db = Db::open_in_memory().unwrap();
            for (kind, id, at) in [
                (AttentionEventKind::TurnStart, "", 1),
                (AttentionEventKind::Completed, "", 2),
                (k, "q", 3),
                (AttentionEventKind::Resolved, "q", 4),
                (AttentionEventKind::Completed, "", 5),
            ] {
                db.record_agent_attention(KEY, &event(kind, id, at))
                    .unwrap();
            }
            assert_eq!(db.list_turn_done_sessions().unwrap().len(), 1);
        }
    }

    #[test]
    fn second_review_자식_질문과_부모_종료를_섞지_않는다() {
        let db = Db::open_in_memory().unwrap();
        db.record_agent_attention(KEY, &event(AttentionEventKind::TurnStart, "", 1))
            .unwrap();
        db.record_agent_attention(
            KEY,
            &event(AttentionEventKind::ApprovalRequired, "child:a:q", 2),
        )
        .unwrap();
        db.record_agent_attention(KEY, &event(AttentionEventKind::Completed, "", 3))
            .unwrap();
        assert_eq!(db.list_waiting_sessions().unwrap().len(), 1);
        let done = db.list_turn_done_sessions().unwrap();
        db.record_agent_attention(KEY, &event(AttentionEventKind::Cancelled, "child:a:", 4))
            .unwrap();
        assert!(db.list_waiting_sessions().unwrap().is_empty());
        assert_eq!(db.list_turn_done_sessions().unwrap(), done);
    }
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
            tool_group: None,
            tool_input_hash: None,
            child_id: None,
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
