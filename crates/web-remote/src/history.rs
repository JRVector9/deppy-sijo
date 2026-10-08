//! Bounded connection-local source queries. The worker owns the terminal; these
//! cursors never change its native display offset or render cache.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::dashboard::CommandSink;
use runtime::{
    RuntimeCommand, RuntimeEvent, SessionId, TerminalHistoryAnchor, TerminalHistoryQuery,
    TerminalViewportSnapshot,
};

const SLOT_CAP: usize = 32;
const TIMEOUT: Duration = Duration::from_secs(5);
const RETRY: Duration = Duration::from_secs(1);

pub(crate) struct HistoryRead {
    pub delta: i32,
    pub request: Option<u32>,
    pub reset: bool,
    pub anchor: Option<crate::protocol::HistoryAnchorView>,
}

#[derive(Clone)]
pub(crate) struct HistoryTarget {
    pub uuid: String,
    pub binding: u64,
    pub watch: u64,
    pub session: SessionId,
    pub namespace: String,
    pub sink: CommandSink,
}

pub(crate) struct HistoryReply {
    pub uuid: String,
    pub request: Option<u32>,
    pub namespace: String,
    pub result: Result<(Arc<TerminalViewportSnapshot>, bool), &'static str>,
    target: Option<HistoryTarget>,
}

#[derive(Default)]
struct Connection {
    last_request: u32,
    pending: Option<u64>,
    cursor: Option<TerminalHistoryAnchor>,
    reply: Option<HistoryReply>,
}

struct Pending {
    target: HistoryTarget,
    connection: Option<u64>,
    request: Option<u32>,
    query: TerminalHistoryQuery,
    last_sent: Instant,
    deadline: Instant,
    dirty: bool,
    source_generation: Option<u64>,
}

pub(crate) struct HistoryQueries {
    next_operation: u64,
    connections: BTreeMap<u64, Connection>,
    pending: BTreeMap<u64, Pending>,
    passive: BTreeMap<String, u64>,
}

impl Default for HistoryQueries {
    fn default() -> Self {
        // Query replies are broadcast by a worker which may outlive this Web
        // instance. Start in a fresh namespace so old same-session replies
        // cannot resolve a new instance's operation. Keep half the range for
        // checked monotonic allocation; retries reuse their existing ID.
        let nonce = uuid::Uuid::new_v4().as_u128();
        let start = ((nonce as u64) ^ ((nonce >> 64) as u64)) & (u64::MAX >> 1);
        Self {
            next_operation: start.max(1),
            connections: BTreeMap::new(),
            pending: BTreeMap::new(),
            passive: BTreeMap::new(),
        }
    }
}

impl HistoryQueries {
    pub fn register(&mut self, connection: u64) {
        if self.connections.len() < SLOT_CAP {
            self.connections.insert(connection, Connection::default());
        }
    }

    pub fn clear_binding(&mut self) {
        self.invalidate(|_| false);
        for state in self.connections.values_mut() {
            state.cursor = None;
        }
    }

    pub fn disconnect(&mut self, connection: u64) {
        self.unwatch(connection);
        self.connections.remove(&connection);
    }

    pub fn unwatch(&mut self, connection: u64) {
        if let Some(state) = self.connections.get_mut(&connection) {
            if let Some(operation) = state.pending.take() {
                self.pending.remove(&operation);
            }
            state.cursor = None;
            state.reply = None;
        }
    }

    fn start(&mut self, pending: Pending) -> Option<u64> {
        if self.pending.len() >= SLOT_CAP {
            return None;
        }
        self.next_operation = self.next_operation.checked_add(1)?;
        let operation = self.next_operation;
        (pending.target.sink)(RuntimeCommand::QueryTerminalHistory {
            session: pending.target.session,
            operation_id: operation,
            query: pending.query,
        });
        self.pending.insert(operation, pending);
        Some(operation)
    }

    pub fn request(
        &mut self,
        connection: u64,
        uuid: &str,
        request: Option<u32>,
        target: Option<HistoryTarget>,
        query: Result<TerminalHistoryQuery, &'static str>,
        now: Instant,
    ) {
        let Some(state) = self.connections.get_mut(&connection) else {
            return;
        };
        if let Some(request) = request {
            if request == 0 || request <= state.last_request {
                return;
            }
            state.last_request = request;
        }
        if let Some(operation) = state.pending.take() {
            self.pending.remove(&operation);
        }
        state.reply = None;
        let Some(target) = target else {
            state.cursor = None;
            state.reply = Some(HistoryReply {
                uuid: uuid.into(),
                request,
                namespace: String::new(),
                result: Err("stale"),
                target: None,
            });
            return;
        };
        let mut query = match query {
            Ok(query) => query,
            Err(reason) => {
                state.reply = Some(HistoryReply {
                    uuid: uuid.into(),
                    request,
                    namespace: target.namespace.clone(),
                    result: Err(reason),
                    target: Some(target),
                });
                return;
            }
        };
        if !query.reset && query.anchor.is_none() {
            query.anchor = state.cursor;
        }
        let namespace = target.namespace.clone();
        let pending = Pending {
            target: target.clone(),
            connection: Some(connection),
            request,
            query,
            last_sent: now,
            deadline: now + TIMEOUT,
            dirty: false,
            source_generation: None,
        };
        let operation = self.start(pending);
        let state = self.connections.get_mut(&connection).unwrap();
        if let Some(operation) = operation {
            state.pending = Some(operation);
        } else {
            state.reply = Some(HistoryReply {
                uuid: uuid.into(),
                request,
                namespace,
                result: Err("capacity"),
                target: Some(target),
            });
        }
    }

    pub fn passive(&mut self, target: HistoryTarget, source_generation: Option<u64>, now: Instant) {
        if let Some(operation) = self.passive.get(&target.uuid).copied()
            && let Some(pending) = self.pending.get_mut(&operation)
        {
            pending.dirty = true;
            pending.source_generation = source_generation;
            return;
        }
        let uuid = target.uuid.clone();
        let pending = Pending {
            target,
            connection: None,
            request: None,
            query: TerminalHistoryQuery::live(),
            last_sent: now,
            deadline: now + TIMEOUT,
            dirty: false,
            source_generation,
        };
        if let Some(operation) = self.start(pending) {
            self.passive.insert(uuid, operation);
        }
    }

    pub fn cancel_all_passive(&mut self) {
        for (_, operation) in std::mem::take(&mut self.passive) {
            self.pending.remove(&operation);
        }
    }

    pub fn cancel_passive(&mut self, uuid: &str) {
        if let Some(operation) = self.passive.remove(uuid) {
            self.pending.remove(&operation);
        }
    }

    fn fail(&mut self, operation: u64, reason: &'static str) {
        let Some(pending) = self.pending.remove(&operation) else {
            return;
        };
        if let Some(connection) = pending.connection {
            if let Some(state) = self
                .connections
                .get_mut(&connection)
                .filter(|state| state.pending == Some(operation))
            {
                state.pending = None;
                state.reply = Some(HistoryReply {
                    uuid: pending.target.uuid.clone(),
                    request: pending.request,
                    namespace: pending.target.namespace.clone(),
                    result: Err(reason),
                    target: Some(pending.target),
                });
            }
        } else {
            self.passive.remove(&pending.target.uuid);
        }
    }

    pub fn invalidate(&mut self, valid: impl Fn(&HistoryTarget) -> bool) {
        let invalid: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, pending)| !valid(&pending.target))
            .map(|(operation, _)| *operation)
            .collect();
        for operation in invalid {
            self.fail(operation, "stale");
        }
        for state in self.connections.values_mut() {
            if state
                .reply
                .as_ref()
                .and_then(|reply| reply.target.as_ref())
                .is_some_and(|target| !valid(target))
            {
                if let Some(reply) = &mut state.reply {
                    reply.result = Err("stale");
                    reply.target = None;
                }
                state.cursor = None;
            }
        }
    }

    pub fn event(
        &mut self,
        event: &RuntimeEvent,
        now: Instant,
    ) -> Option<(String, Arc<TerminalViewportSnapshot>)> {
        let RuntimeEvent::TerminalHistoryResult {
            session,
            operation_id,
            snapshot,
            expired,
        } = event
        else {
            return None;
        };
        let pending = self.pending.get(operation_id)?;
        if pending.target.session != *session {
            return None;
        }
        if now >= pending.deadline {
            self.fail(*operation_id, "timeout");
            return None;
        }
        let pending = self.pending.remove(operation_id).unwrap();
        if let Some(connection) = pending.connection {
            let state = self
                .connections
                .get_mut(&connection)
                .filter(|state| state.pending == Some(*operation_id))?;
            state.pending = None;
            state.reply = Some(HistoryReply {
                uuid: pending.target.uuid.clone(),
                request: pending.request,
                namespace: pending.target.namespace.clone(),
                result: snapshot
                    .clone()
                    .map(|snapshot| (snapshot, *expired))
                    .ok_or("unavailable"),
                target: Some(pending.target),
            });
            None
        } else {
            let uuid = pending.target.uuid.clone();
            self.passive.remove(&uuid);
            let generation_changed = snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.history)
                .is_some_and(|history| {
                    pending
                        .source_generation
                        .is_some_and(|source| source != history.generation)
                });
            if pending.dirty || generation_changed {
                self.passive(pending.target, pending.source_generation, now);
            }
            if generation_changed {
                return None;
            }
            snapshot
                .as_ref()
                .filter(|snapshot| snapshot.scroll_offset == 0)
                .map(|snapshot| (uuid, Arc::clone(snapshot)))
        }
    }

    pub fn take_reply(
        &mut self,
        connection: u64,
        valid: impl Fn(&HistoryTarget) -> bool,
    ) -> Option<HistoryReply> {
        let state = self.connections.get_mut(&connection)?;
        let mut reply = state.reply.take()?;
        if reply.target.as_ref().is_some_and(|target| !valid(target)) {
            reply.result = Err("stale");
        }
        if let Ok((snapshot, expired)) = &reply.result {
            state.cursor = if *expired {
                None
            } else {
                snapshot.history.map(|history| TerminalHistoryAnchor {
                    generation: history.generation,
                    first_line: history.first_line,
                })
            };
        }
        Some(reply)
    }

    pub fn tick(&mut self, now: Instant) {
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, pending)| now >= pending.deadline)
            .map(|(operation, _)| *operation)
            .collect();
        for operation in expired {
            self.fail(operation, "timeout");
        }
        for (operation, pending) in &mut self.pending {
            if now.duration_since(pending.last_sent) >= RETRY {
                pending.last_sent = now;
                (pending.target.sink)(RuntimeCommand::QueryTerminalHistory {
                    session: pending.target.session,
                    operation_id: *operation,
                    query: pending.query,
                });
            }
        }
    }

    pub fn timer_wait(&self, now: Instant) -> Option<Duration> {
        self.pending
            .values()
            .map(|pending| {
                pending
                    .deadline
                    .min(pending.last_sent + RETRY)
                    .saturating_duration_since(now)
            })
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn target(
        binding: u64,
        watch: u64,
        captured: &Arc<Mutex<Vec<RuntimeCommand>>>,
    ) -> HistoryTarget {
        let captured = Arc::clone(captured);
        HistoryTarget {
            uuid: "same-uuid".into(),
            binding,
            watch,
            session: SessionId(1),
            namespace: format!("test:{binding}"),
            sink: Arc::new(move |command| captured.lock().unwrap().push(command)),
        }
    }
    fn snapshot(first_line: u64, offset: i32, generation: u64) -> Arc<TerminalViewportSnapshot> {
        Arc::new(TerminalViewportSnapshot {
            cols: 2,
            rows: 2,
            cursor: runtime::CursorSnapshot {
                col: 0,
                row: 0,
                shape: runtime::CursorShape::Block,
                visible: false,
            },
            visible_cells: vec![
                runtime::TerminalCell::new(
                    'x',
                    [255; 3],
                    [0; 3],
                    false,
                    false,
                    Default::default()
                );
                4
            ]
            .into(),
            graphemes: Default::default(),
            dirty_ranges: Vec::new(),
            title: None,
            scroll_offset: offset,
            is_alt_screen: false,
            history: Some(runtime::TerminalHistoryMetadata {
                generation,
                first_line,
                total_lines: 10,
            }),
        })
    }
    fn answer(operation_id: u64, first_line: u64, offset: i32, generation: u64) -> RuntimeEvent {
        RuntimeEvent::TerminalHistoryResult {
            session: SessionId(1),
            operation_id,
            snapshot: Some(snapshot(first_line, offset, generation)),
            expired: false,
        }
    }
    fn last_operation(captured: &Arc<Mutex<Vec<RuntimeCommand>>>) -> u64 {
        captured
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|command| match command {
                RuntimeCommand::QueryTerminalHistory { operation_id, .. } => Some(*operation_id),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn history_web_fresh_controllers_reject_late_same_session_old_answers() {
        let now = Instant::now();
        let old_commands = Arc::default();
        let fresh_commands = Arc::default();
        let mut old = HistoryQueries::default();
        let mut fresh = HistoryQueries::default();
        old.register(1);
        fresh.register(1);
        old.request(
            1,
            "same-uuid",
            Some(1),
            Some(target(1, 3, &old_commands)),
            Ok(TerminalHistoryQuery::live()),
            now,
        );
        let old_operation = last_operation(&old_commands);
        fresh.request(
            1,
            "same-uuid",
            Some(1),
            Some(target(1, 3, &fresh_commands)),
            Ok(TerminalHistoryQuery::live()),
            now,
        );
        let fresh_operation = last_operation(&fresh_commands);
        // A retained worker broadcasts an old Web instance's answer to the
        // fresh subscriber. Same session/watch/request must not resolve it.
        fresh.event(&answer(old_operation, 97, 3, 7), now);
        assert!(
            fresh.take_reply(1, |_| true).is_none(),
            "late old Dashboard answer poisoned a fresh socket query"
        );
        assert_ne!(old_operation, fresh_operation);
        fresh.event(&answer(fresh_operation, 100, 0, 7), now);
        let reply = fresh.take_reply(1, |_| true).unwrap();
        assert_eq!(reply.request, Some(1));
        assert_eq!(reply.result.unwrap().0.scroll_offset, 0);
    }

    #[test]
    fn history_web_operation_exhaustion_fails_without_wrapping_or_reviving_canceled_reply() {
        let now = Instant::now();
        let captured = Arc::default();
        let mut queries = HistoryQueries {
            next_operation: u64::MAX - 1,
            ..Default::default()
        };
        queries.register(1);
        let source = target(1, 3, &captured);
        queries.request(
            1,
            "same-uuid",
            Some(1),
            Some(source.clone()),
            Ok(TerminalHistoryQuery::live()),
            now,
        );
        assert_eq!(last_operation(&captured), u64::MAX);
        queries.request(
            1,
            "same-uuid",
            Some(2),
            Some(source),
            Ok(TerminalHistoryQuery::live()),
            now,
        );
        let reply = queries.take_reply(1, |_| true).unwrap();
        assert_eq!(reply.request, Some(2));
        assert_eq!(reply.result.unwrap_err(), "capacity");
        assert_eq!(captured.lock().unwrap().len(), 1);
        assert!(queries.pending.is_empty());
        queries.event(&answer(u64::MAX, 100, 0, 7), now);
        assert!(queries.take_reply(1, |_| true).is_none());
    }

    #[test]
    fn history_web_two_connections_and_latest_reset_discard_stale_answers_before_publication() {
        let now = Instant::now();
        let captured = Arc::default();
        let mut queries = HistoryQueries::default();
        queries.register(1);
        queries.register(2);
        let source = target(1, 3, &captured);
        let older = runtime::TerminalHistoryQuery {
            anchor: None,
            delta: 3,
            reset: false,
        };
        queries.request(
            1,
            "same-uuid",
            Some(1),
            Some(source.clone()),
            Ok(older),
            now,
        );
        let stale = last_operation(&captured);
        queries.request(
            2,
            "same-uuid",
            Some(1),
            Some(source.clone()),
            Ok(older),
            now,
        );
        let other = last_operation(&captured);
        queries.request(
            1,
            "same-uuid",
            Some(2),
            Some(source),
            Ok(TerminalHistoryQuery::live()),
            now,
        );
        let reset = last_operation(&captured);
        assert!(queries.event(&answer(stale, 97, 3, 7), now).is_none());
        assert!(
            queries.take_reply(1, |_| true).is_none(),
            "old request cannot poison socket baseline"
        );
        queries.event(&answer(other, 97, 3, 7), now);
        let reply = queries.take_reply(2, |_| true).unwrap();
        assert_eq!(reply.request, Some(1));
        assert_eq!(reply.result.unwrap().0.scroll_offset, 3);
        queries.event(&answer(reset, 100, 0, 7), now);
        let reply = queries.take_reply(1, |_| true).unwrap();
        assert_eq!(reply.request, Some(2));
        assert_eq!(reply.result.unwrap().0.scroll_offset, 0);
        queries.event(&answer(stale, 97, 3, 7), now);
        assert!(queries.take_reply(1, |_| true).is_none());
    }

    #[test]
    fn history_web_retry_timeout_correlation_and_unwatch_keep_whole_socket_counter() {
        let now = Instant::now();
        let captured = Arc::default();
        let mut queries = HistoryQueries::default();
        queries.register(1);
        let source = target(1, 3, &captured);
        queries.request(
            1,
            "same-uuid",
            Some(7),
            Some(source.clone()),
            Ok(TerminalHistoryQuery::live()),
            now,
        );
        let operation = last_operation(&captured);
        queries.tick(now + RETRY);
        assert_eq!(last_operation(&captured), operation);
        assert_eq!(
            captured.lock().unwrap().len(),
            2,
            "dropped enqueue/reply retries same operation once/sec"
        );
        queries.tick(now + TIMEOUT);
        let reply = queries.take_reply(1, |_| true).unwrap();
        assert_eq!(reply.request, Some(7));
        assert_eq!(reply.result.unwrap_err(), "timeout");
        assert!(queries.timer_wait(now + TIMEOUT).is_none());
        queries.event(&answer(operation, 100, 0, 7), now + TIMEOUT);
        assert!(
            queries.take_reply(1, |_| true).is_none(),
            "late ACK cannot revive timed-out request"
        );
        queries.unwatch(1);
        queries.request(
            1,
            "same-uuid",
            Some(7),
            Some(source.clone()),
            Ok(TerminalHistoryQuery::live()),
            now,
        );
        assert!(
            queries.pending.is_empty(),
            "watch reset cannot reset request sequence"
        );
        queries.request(
            1,
            "same-uuid",
            Some(8),
            Some(source),
            Ok(TerminalHistoryQuery::live()),
            now,
        );
        assert_eq!(queries.pending.len(), 1);
        queries.disconnect(1);
        assert!(queries.pending.is_empty());
        assert!(queries.timer_wait(now).is_none());
    }

    #[test]
    fn history_web_binding_invalidation_cancels_old_sink_and_rejects_old_result() {
        let now = Instant::now();
        let old_commands = Arc::default();
        let new_commands = Arc::default();
        let mut queries = HistoryQueries::default();
        queries.register(1);
        queries.request(
            1,
            "same-uuid",
            Some(1),
            Some(target(1, 3, &old_commands)),
            Ok(TerminalHistoryQuery::live()),
            now,
        );
        let old_operation = last_operation(&old_commands);
        queries.invalidate(|target| target.binding == 2);
        queries.tick(now + RETRY);
        assert_eq!(
            old_commands.lock().unwrap().len(),
            1,
            "invalid old binding never receives retries"
        );
        let stale = queries.take_reply(1, |_| false).unwrap();
        assert_eq!(stale.result.unwrap_err(), "stale");
        queries.request(
            1,
            "same-uuid",
            Some(2),
            Some(target(2, 4, &new_commands)),
            Ok(TerminalHistoryQuery::live()),
            now,
        );
        queries.event(&answer(old_operation, 100, 0, 7), now);
        assert!(queries.take_reply(1, |_| true).is_none());
        queries.event(&answer(last_operation(&new_commands), 200, 0, 7), now);
        assert_eq!(queries.take_reply(1, |_| true).unwrap().namespace, "test:2");
    }

    #[test]
    fn history_web_new_native_live_frame_cancels_old_passive_and_reset_generation_requeries() {
        let now = Instant::now();
        let captured = Arc::default();
        let mut queries = HistoryQueries::default();
        let source = target(1, 3, &captured);
        queries.passive(source.clone(), Some(7), now);
        let obsolete = last_operation(&captured);
        queries.cancel_passive("same-uuid");
        assert!(
            queries.event(&answer(obsolete, 100, 0, 7), now).is_none(),
            "older live0 query cannot overwrite newer native offset0 frame"
        );
        queries.passive(source.clone(), Some(7), now);
        let before_reset = last_operation(&captured);
        queries.passive(source, Some(8), now);
        assert!(
            queries
                .event(&answer(before_reset, 100, 0, 7), now)
                .is_none(),
            "old engine generation cannot publish after reset"
        );
        let fresh = last_operation(&captured);
        assert_ne!(fresh, before_reset);
        let (uuid, live) = queries.event(&answer(fresh, 0, 0, 8), now).unwrap();
        assert_eq!(uuid, "same-uuid");
        assert_eq!(live.history.unwrap().generation, 8);
        assert!(queries.pending.is_empty());
    }

    #[test]
    fn history_web_query_capacity_and_invalid_anchor_fail_without_retaining_extra_frames() {
        let now = Instant::now();
        let captured = Arc::default();
        let mut queries = HistoryQueries::default();
        queries.register(1);
        for number in 0..SLOT_CAP {
            let mut source = target(1, 3, &captured);
            source.uuid = format!("uuid-{number}");
            queries.passive(source, Some(7), now);
        }
        queries.request(
            1,
            "same-uuid",
            Some(1),
            Some(target(1, 3, &captured)),
            Ok(TerminalHistoryQuery::live()),
            now,
        );
        assert_eq!(
            queries.take_reply(1, |_| true).unwrap().result.unwrap_err(),
            "capacity"
        );
        assert_eq!(queries.pending.len(), SLOT_CAP);
        queries.request(
            1,
            "same-uuid",
            Some(2),
            Some(target(1, 3, &captured)),
            Err("invalid_anchor"),
            now,
        );
        assert_eq!(
            queries.take_reply(1, |_| true).unwrap().result.unwrap_err(),
            "invalid_anchor"
        );
        assert_eq!(captured.lock().unwrap().len(), SLOT_CAP);
        queries.clear_binding();
        assert!(queries.pending.is_empty());
        assert!(queries.passive.is_empty());
    }
}
