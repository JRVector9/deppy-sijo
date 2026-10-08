//! Per-connection correlation; authority remains in the worker. Every enqueue is
//! nonblocking and retried with the same operation/owner/epoch until an ACK or deadline.
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::dashboard::CommandSink;
use runtime::{
    RuntimeCommand, RuntimeEvent, SessionId, TerminalControlRequest as Request,
    TerminalControlStatus as Status,
};

const SLOT_CAP: usize = 16;
const TTL_MS: u32 = 15_000;
const RENEW_INTERVAL: Duration = Duration::from_secs(5);
const OP_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_secs(1);
const CANCEL_TIMEOUT: Duration =
    Duration::from_millis(runtime::TERMINAL_CONTROL_TTL_MAX_MS as u64 + 5_000);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ControlAction {
    Acquire,
    Release,
}

#[derive(Clone)]
pub(crate) struct ControlTarget {
    pub uuid: String,
    pub generation: u64,
    pub session: SessionId,
    pub sink: CommandSink,
}

#[derive(Clone, Debug)]
pub(crate) struct ControlReply {
    pub uuid: String,
    pub request: u32,
    pub owned: bool,
    pub reason: &'static str,
    pub keyframe: bool,
}

#[derive(Default)]
struct Connection {
    request: u32,
    action: Option<ControlAction>,
    target: Option<ControlTarget>,
    started_at: Option<Instant>,
    begin_acquire: bool,
    version: u64,
    keyframe_version: u64,
    reply: Option<ControlReply>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Query,
    Acquiring,
    Owned,
    Closing,
}

struct Pending {
    operation: u64,
    request: Request,
    last_sent: Instant,
    deadline: Instant,
}

struct Slot {
    connection: Option<u64>,
    client_request: u32,
    target: ControlTarget,
    owner: [u8; 16],
    epoch: Option<u64>,
    phase: Phase,
    pending: Option<Pending>,
    deadline: Instant,
    renew_at: Instant,
    revision: u64,
    geometry: Option<(u16, u16)>,
}

#[derive(Default)]
pub(crate) struct ResizeControls {
    next_connection: u64,
    next_operation: u64,
    next_slot: u64,
    connections: BTreeMap<u64, Connection>,
    slots: BTreeMap<u64, Slot>,
    last_tick: Option<Instant>,
}

impl ResizeControls {
    pub fn register(&mut self) -> u64 {
        self.next_connection = self
            .next_connection
            .checked_add(1)
            .expect("connection identity");
        self.connections
            .insert(self.next_connection, Connection::default());
        self.next_connection
    }

    fn publish(
        &mut self,
        connection: u64,
        uuid: &str,
        request: u32,
        owned: bool,
        reason: &'static str,
        keyframe: bool,
    ) {
        let Some(state) = self
            .connections
            .get_mut(&connection)
            .filter(|state| state.request == request)
        else {
            return;
        };
        state.version = state.version.saturating_add(1);
        if keyframe {
            state.keyframe_version = state.version;
        }
        state.reply = Some(ControlReply {
            uuid: uuid.into(),
            request,
            owned,
            reason,
            keyframe: false,
        });
    }

    fn publish_slot(&mut self, slot: &Slot, owned: bool, reason: &'static str, keyframe: bool) {
        if let Some(connection) = slot.connection {
            self.publish(
                connection,
                &slot.target.uuid,
                slot.client_request,
                owned,
                reason,
                keyframe,
            );
        }
    }

    pub fn reply_after(&self, connection: u64, last: u64) -> Option<(u64, ControlReply)> {
        let state = self.connections.get(&connection)?;
        if state.version <= last {
            return None;
        }
        let mut reply = state.reply.clone()?;
        reply.keyframe = state.keyframe_version > last;
        Some((state.version, reply))
    }

    fn enqueue(
        &mut self,
        slot: &mut Slot,
        request: Request,
        deadline: Instant,
        now: Instant,
    ) -> bool {
        let Some(operation) = self.next_operation.checked_add(1) else {
            return false;
        };
        self.next_operation = operation;
        (slot.target.sink)(RuntimeCommand::TerminalControl {
            session: slot.target.session,
            operation_id: operation,
            request,
        });
        slot.pending = Some(Pending {
            operation,
            request,
            last_sent: now,
            deadline,
        });
        true
    }

    fn cancel(&mut self, slot: &mut Slot, now: Instant) -> bool {
        slot.geometry = None;
        let Some(epoch) = slot.epoch else {
            return false;
        };
        slot.phase = Phase::Closing;
        slot.deadline = now + CANCEL_TIMEOUT;
        let owner = slot.owner;
        let deadline = slot.deadline;
        self.enqueue(
            slot,
            Request::Release {
                owner,
                lease_epoch: epoch,
            },
            deadline,
            now,
        )
    }

    pub fn unwatch(&mut self, connection: u64, now: Instant) {
        if let Some(state) = self.connections.get_mut(&connection) {
            state.begin_acquire = false;
            state.target = None;
        }
        let ids = self
            .slots
            .iter()
            .filter(|(_, slot)| slot.connection == Some(connection))
            .map(|(&id, _)| id)
            .collect::<Vec<_>>();
        for id in ids {
            let mut slot = self.slots.remove(&id).unwrap();
            self.publish_slot(&slot, false, "unavailable", false);
            slot.connection = None;
            if self.cancel(&mut slot, now) {
                self.slots.insert(id, slot);
            }
        }
    }

    pub fn disconnect(&mut self, connection: u64, now: Instant) {
        self.unwatch(connection, now);
        self.connections.remove(&connection);
    }

    pub fn invalidate_binding(&mut self, now: Instant) {
        let connections = self.connections.keys().copied().collect::<Vec<_>>();
        for connection in connections {
            self.unwatch(connection, now);
        }
    }

    pub fn reject(&mut self, connection: u64, uuid: &str, action: ControlAction, request: u32) {
        let Some(state) = self.connections.get_mut(&connection) else {
            return;
        };
        if request == 0 || request <= state.request {
            return;
        }
        state.request = request;
        state.action = Some(action);
        state.begin_acquire = false;
        state.target = None;
        self.publish(connection, uuid, request, false, "unavailable", false);
    }

    pub fn request(
        &mut self,
        connection: u64,
        target: ControlTarget,
        action: ControlAction,
        request: u32,
        now: Instant,
    ) {
        let Some(state) = self.connections.get_mut(&connection) else {
            return;
        };
        if request == 0 || request < state.request {
            return;
        }
        if request == state.request {
            // A duplicate cannot be reinterpreted against a later watch/binding.
            if state.action == Some(action)
                && state.target.as_ref().is_some_and(|old| {
                    old.uuid == target.uuid
                        && old.generation == target.generation
                        && old.session == target.session
                })
            {
                state.version = state.version.saturating_add(1);
            }
            return;
        }
        state.request = request;
        state.action = Some(action);
        state.target = Some(target.clone());
        state.started_at = Some(now);
        state.begin_acquire = action == ControlAction::Acquire;
        self.publish(connection, &target.uuid, request, false, "pending", false);
        let ids = self
            .slots
            .iter()
            .filter(|(_, slot)| slot.connection == Some(connection))
            .map(|(&id, _)| id)
            .collect::<Vec<_>>();
        let mut released = false;
        for id in ids {
            let mut slot = self.slots.remove(&id).unwrap();
            if action == ControlAction::Release
                && slot.target.uuid == target.uuid
                && slot.target.generation == target.generation
                && slot.target.session == target.session
            {
                slot.client_request = request;
                released = true;
            } else {
                slot.connection = None;
            }
            if self.cancel(&mut slot, now) {
                self.slots.insert(id, slot);
            } else if slot.connection.is_some() {
                self.publish_slot(&slot, false, "released", false);
            }
        }
        if action == ControlAction::Release && !released {
            self.publish(connection, &target.uuid, request, false, "released", false);
        }
        self.start_acquires(now);
    }

    fn start_acquires(&mut self, now: Instant) {
        let connections = self
            .connections
            .iter()
            .filter(|(_, state)| state.begin_acquire)
            .map(|(&id, _)| id)
            .collect::<Vec<_>>();
        for connection in connections {
            let state = &self.connections[&connection];
            let Some(target) = state.target.clone() else {
                continue;
            };
            let request = state.request;
            let deadline = state.started_at.unwrap() + OP_TIMEOUT;
            if now >= deadline {
                self.connections.get_mut(&connection).unwrap().begin_acquire = false;
                self.publish(connection, &target.uuid, request, false, "timeout", false);
                continue;
            }
            if self.slots.values().any(|slot| {
                slot.target.generation == target.generation
                    && slot.target.session == target.session
                    && slot.phase == Phase::Closing
            }) {
                continue;
            }
            self.connections.get_mut(&connection).unwrap().begin_acquire = false;
            if self.slots.len() >= SLOT_CAP {
                self.publish(connection, &target.uuid, request, false, "capacity", false);
                continue;
            }
            let Some(id) = self.next_slot.checked_add(1) else {
                self.publish(connection, &target.uuid, request, false, "capacity", false);
                continue;
            };
            self.next_slot = id;
            let mut slot = Slot {
                connection: Some(connection),
                client_request: request,
                target,
                owner: *uuid::Uuid::new_v4().as_bytes(),
                epoch: None,
                phase: Phase::Query,
                pending: None,
                deadline,
                renew_at: now,
                revision: 0,
                geometry: None,
            };
            if self.enqueue(&mut slot, Request::Query, deadline, now) {
                self.slots.insert(id, slot);
            } else {
                self.publish_slot(&slot, false, "capacity", false);
            }
        }
    }

    pub fn resize(
        &mut self,
        connection: u64,
        uuid: &str,
        generation: u64,
        request: u32,
        cols: u16,
        rows: u16,
    ) {
        if !self.connections.get(&connection).is_some_and(|state| {
            state.request == request && state.action == Some(ControlAction::Acquire)
        }) {
            return;
        }
        if let Some(slot) = self.slots.values_mut().find(|slot| {
            slot.connection == Some(connection)
                && slot.client_request == request
                && slot.target.uuid == uuid
                && slot.target.generation == generation
                && slot.phase == Phase::Owned
        }) {
            slot.geometry = Some((cols, rows));
        }
    }

    pub fn invalidate_targets(
        &mut self,
        generation: u64,
        valid: impl Fn(&ControlTarget) -> bool,
        now: Instant,
    ) {
        let connections = self
            .connections
            .iter()
            .filter_map(|(&id, state)| {
                state
                    .target
                    .as_ref()
                    .filter(|target| target.generation == generation && !valid(target))
                    .map(|_| id)
            })
            .collect::<Vec<_>>();
        for connection in connections {
            self.unwatch(connection, now);
        }
    }

    pub fn tick_due(&self, now: Instant) -> bool {
        self.needs_timer()
            && self
                .last_tick
                .is_none_or(|last| now.saturating_duration_since(last) >= RETRY_INTERVAL)
    }

    pub fn timer_wait(&self, now: Instant) -> Option<Duration> {
        self.needs_timer().then(|| {
            self.last_tick.map_or(Duration::ZERO, |last| {
                RETRY_INTERVAL.saturating_sub(now.saturating_duration_since(last))
            })
        })
    }

    fn needs_timer(&self) -> bool {
        !self.slots.is_empty() || self.connections.values().any(|state| state.begin_acquire)
    }

    pub fn tick(&mut self, now: Instant) {
        self.last_tick = Some(now);
        let ids = self.slots.keys().copied().collect::<Vec<_>>();
        for id in ids {
            let mut slot = self.slots.remove(&id).unwrap();
            if slot.phase == Phase::Owned && now >= slot.deadline {
                self.publish_slot(&slot, false, "expired", false);
                slot.connection = None;
                if self.cancel(&mut slot, now) {
                    self.slots.insert(id, slot);
                }
                continue;
            }
            if slot
                .pending
                .as_ref()
                .is_some_and(|pending| now >= pending.deadline)
            {
                if slot.phase == Phase::Closing {
                    self.publish_slot(&slot, false, "timeout", false);
                    continue;
                }
                self.publish_slot(&slot, false, "timeout", false);
                slot.connection = None;
                if self.cancel(&mut slot, now) {
                    self.slots.insert(id, slot);
                }
                continue;
            }
            if let Some(pending) = slot.pending.as_mut() {
                if now.saturating_duration_since(pending.last_sent) >= RETRY_INTERVAL {
                    pending.last_sent = now;
                    (slot.target.sink)(RuntimeCommand::TerminalControl {
                        session: slot.target.session,
                        operation_id: pending.operation,
                        request: pending.request,
                    });
                }
            } else if slot.phase == Phase::Owned {
                let request = if now >= slot.renew_at {
                    Some(Request::Renew {
                        owner: slot.owner,
                        lease_epoch: slot.epoch.unwrap(),
                        ttl_ms: TTL_MS,
                    })
                } else if let Some((cols, rows)) = slot.geometry.take() {
                    if let Some(revision) = slot.revision.checked_add(1) {
                        slot.revision = revision;
                        Some(Request::Resize {
                            owner: slot.owner,
                            lease_epoch: slot.epoch.unwrap(),
                            revision,
                            cols,
                            rows,
                        })
                    } else {
                        None
                    }
                } else {
                    self.slots.insert(id, slot);
                    continue;
                };
                let deadline = (now + OP_TIMEOUT).min(slot.deadline);
                if request.is_none_or(|request| !self.enqueue(&mut slot, request, deadline, now)) {
                    self.publish_slot(&slot, false, "capacity", false);
                    slot.connection = None;
                    if !self.cancel(&mut slot, now) {
                        continue;
                    }
                }
            }
            self.slots.insert(id, slot);
        }
        self.start_acquires(now);
    }

    pub fn event(&mut self, generation: u64, event: &RuntimeEvent, now: Instant) {
        match event {
            RuntimeEvent::TerminalControlResult {
                session,
                operation_id,
                state,
                status,
                stamp,
            } => {
                let id = self
                    .slots
                    .iter()
                    .find(|(_, slot)| {
                        slot.target.generation == generation
                            && slot.target.session == *session
                            && slot
                                .pending
                                .as_ref()
                                .is_some_and(|pending| pending.operation == *operation_id)
                    })
                    .map(|(&id, _)| id);
                let Some(id) = id else {
                    return;
                };
                let mut slot = self.slots.remove(&id).unwrap();
                let pending = slot.pending.take().unwrap();
                if slot.phase == Phase::Closing {
                    if state.owner == Some(slot.owner) && Some(state.epoch) == slot.epoch {
                        slot.pending = Some(pending);
                        self.slots.insert(id, slot);
                    } else {
                        self.publish_slot(
                            &slot,
                            false,
                            if *status == Status::RestoreFailed {
                                "restore_failed"
                            } else {
                                "released"
                            },
                            stamp.is_some(),
                        );
                    }
                    self.start_acquires(now);
                    return;
                }
                if now >= pending.deadline {
                    self.publish_slot(&slot, false, "timeout", false);
                    slot.connection = None;
                    if self.cancel(&mut slot, now) {
                        self.slots.insert(id, slot);
                    }
                    return;
                }
                if slot.phase == Phase::Query {
                    if *status == Status::Unavailable {
                        self.publish_slot(&slot, false, "unavailable", false);
                        return;
                    }
                    if state.owner.is_some() {
                        self.publish_slot(&slot, false, "in_use", false);
                        return;
                    }
                    let Some(epoch) = state.epoch.checked_add(1) else {
                        self.publish_slot(&slot, false, "stale", false);
                        return;
                    };
                    slot.epoch = Some(epoch);
                    slot.phase = Phase::Acquiring;
                    let owner = slot.owner;
                    let deadline = slot.deadline;
                    if self.enqueue(
                        &mut slot,
                        Request::Acquire {
                            owner,
                            expected_epoch: state.epoch,
                            ttl_ms: TTL_MS,
                        },
                        deadline,
                        now,
                    ) {
                        self.slots.insert(id, slot);
                    } else {
                        self.publish_slot(&slot, false, "capacity", false);
                    }
                    return;
                }
                let owns = state.owner == Some(slot.owner) && Some(state.epoch) == slot.epoch;
                if owns
                    && matches!(
                        status,
                        Status::Owned | Status::InvalidSize | Status::ResizeFailed
                    )
                {
                    slot.phase = Phase::Owned;
                    slot.deadline = now + Duration::from_millis(u64::from(state.lease_ms));
                    if matches!(
                        pending.request,
                        Request::Acquire { .. } | Request::Renew { .. }
                    ) {
                        slot.renew_at = now + RENEW_INTERVAL;
                    }
                    self.publish_slot(&slot, true, status_reason(*status), stamp.is_some());
                    self.slots.insert(id, slot);
                } else {
                    self.publish_slot(&slot, false, status_reason(*status), stamp.is_some());
                    // A mismatched/failed response must never strand a possibly enqueued grant.
                    slot.connection = None;
                    if self.cancel(&mut slot, now) {
                        self.slots.insert(id, slot);
                    }
                }
            }
            RuntimeEvent::TerminalControlChanged {
                session,
                state,
                status,
            } => {
                let ids = self
                    .slots
                    .iter()
                    .filter(|(_, slot)| {
                        slot.target.generation == generation
                            && slot.target.session == *session
                            && slot.phase == Phase::Owned
                            && (state.owner != Some(slot.owner) || Some(state.epoch) != slot.epoch)
                    })
                    .map(|(&id, _)| id)
                    .collect::<Vec<_>>();
                for id in ids {
                    let slot = self.slots.remove(&id).unwrap();
                    self.publish_slot(&slot, false, status_reason(*status), false);
                }
            }
            _ => {}
        }
    }
}

fn status_reason(status: Status) -> &'static str {
    match status {
        Status::Owned => "owned",
        Status::InUse => "in_use",
        Status::Released => "released",
        Status::Expired => "expired",
        Status::Unavailable => "unavailable",
        Status::Stale => "stale",
        Status::InvalidSize => "invalid_size",
        Status::ResizeFailed => "resize_failed",
        Status::RestoreFailed => "restore_failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type Commands = Arc<Mutex<Vec<RuntimeCommand>>>;

    fn target(generation: u64, session: u64) -> (ControlTarget, Commands) {
        let commands = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&commands);
        (
            ControlTarget {
                uuid: format!("uuid-{session}"),
                generation,
                session: SessionId(session),
                sink: Arc::new(move |command| captured.lock().unwrap().push(command)),
            },
            commands,
        )
    }

    fn drain(commands: &Commands) -> Vec<(u64, Request)> {
        commands
            .lock()
            .unwrap()
            .drain(..)
            .map(|command| match command {
                RuntimeCommand::TerminalControl {
                    operation_id,
                    request,
                    ..
                } => (operation_id, request),
                _ => panic!("control command"),
            })
            .collect()
    }

    fn result(
        controls: &mut ResizeControls,
        target: &ControlTarget,
        operation_id: u64,
        state: runtime::TerminalControlState,
        status: Status,
        now: Instant,
    ) {
        controls.event(
            target.generation,
            &RuntimeEvent::TerminalControlResult {
                session: target.session,
                operation_id,
                state,
                status,
                stamp: None,
            },
            now,
        );
    }

    fn grant(
        controls: &mut ResizeControls,
        connection: u64,
        target: &ControlTarget,
        commands: &Commands,
        client_request: u32,
        epoch: u64,
        now: Instant,
    ) -> [u8; 16] {
        controls.request(
            connection,
            target.clone(),
            ControlAction::Acquire,
            client_request,
            now,
        );
        let query = drain(commands);
        assert_eq!(query.len(), 1);
        assert_eq!(query[0].1, Request::Query);
        result(
            controls,
            target,
            query[0].0,
            runtime::TerminalControlState {
                epoch,
                owner: None,
                lease_ms: 0,
            },
            Status::Released,
            now,
        );
        let acquire = drain(commands);
        let Request::Acquire {
            owner,
            expected_epoch,
            ..
        } = acquire[0].1
        else {
            panic!("query before acquire");
        };
        assert_eq!(expected_epoch, epoch);
        let state = runtime::TerminalControlState {
            epoch: epoch + 1,
            owner: Some(owner),
            lease_ms: TTL_MS,
        };
        controls.event(
            target.generation,
            &RuntimeEvent::TerminalControlChanged {
                session: target.session,
                state,
                status: Status::Owned,
            },
            now,
        );
        assert!(
            !controls.reply_after(connection, 0).unwrap().1.owned,
            "broadcast grant is not a correlated operation acknowledgement"
        );
        result(controls, target, acquire[0].0, state, Status::Owned, now);
        assert!(controls.reply_after(connection, 0).unwrap().1.owned);
        owner
    }

    #[test]
    fn resize_control_retries_identical_query_acquire_resize_and_release_at_zero_connections() {
        let now = Instant::now();
        let (target, commands) = target(1, 1);
        let mut controls = ResizeControls::default();
        let connection = controls.register();
        controls.request(connection, target.clone(), ControlAction::Acquire, 1, now);
        let first = drain(&commands)[0];
        controls.tick(now + RETRY_INTERVAL);
        assert_eq!(drain(&commands), [first]);
        result(
            &mut controls,
            &target,
            first.0,
            runtime::TerminalControlState {
                epoch: 0,
                owner: None,
                lease_ms: 0,
            },
            Status::Released,
            now + RETRY_INTERVAL,
        );
        let acquire = drain(&commands)[0];
        controls.tick(now + RETRY_INTERVAL * 2);
        assert_eq!(drain(&commands), [acquire]);
        let Request::Acquire { owner, .. } = acquire.1 else {
            panic!("acquire");
        };
        let state = runtime::TerminalControlState {
            epoch: 1,
            owner: Some(owner),
            lease_ms: TTL_MS,
        };
        result(
            &mut controls,
            &target,
            acquire.0,
            state,
            Status::Owned,
            now + RETRY_INTERVAL * 2,
        );
        controls.resize(connection, &target.uuid, 1, 1, 40, 6);
        controls.tick(now + RETRY_INTERVAL * 2);
        let resize = drain(&commands)[0];
        controls.tick(now + RETRY_INTERVAL * 3);
        assert_eq!(drain(&commands), [resize]);
        result(
            &mut controls,
            &target,
            resize.0,
            state,
            Status::Owned,
            now + RETRY_INTERVAL * 3,
        );
        controls.request(
            connection,
            target.clone(),
            ControlAction::Release,
            2,
            now + RETRY_INTERVAL * 3,
        );
        let release = drain(&commands)[0];
        assert!(!controls.reply_after(connection, 0).unwrap().1.owned);
        controls.disconnect(connection, now + RETRY_INTERVAL * 3);
        let detached = drain(&commands)[0];
        assert_eq!(detached.1, release.1);
        controls.tick(now + RETRY_INTERVAL * 4);
        assert_eq!(drain(&commands), [detached]);
        assert!(controls.timer_wait(now + RETRY_INTERVAL * 4).is_some());
        result(
            &mut controls,
            &target,
            detached.0,
            runtime::TerminalControlState {
                epoch: 2,
                owner: None,
                lease_ms: 0,
            },
            Status::Released,
            now + RETRY_INTERVAL * 4,
        );
        assert!(controls.slots.is_empty());
        assert!(controls.timer_wait(now + RETRY_INTERVAL * 4).is_none());
    }

    #[test]
    fn resize_control_timeout_cancels_predicted_grant_and_ignores_late_ack() {
        let now = Instant::now();
        let (target, commands) = target(1, 1);
        let mut controls = ResizeControls::default();
        let connection = controls.register();
        controls.request(connection, target.clone(), ControlAction::Acquire, 1, now);
        let query = drain(&commands)[0];
        result(
            &mut controls,
            &target,
            query.0,
            runtime::TerminalControlState {
                epoch: 6,
                owner: None,
                lease_ms: 0,
            },
            Status::Released,
            now,
        );
        let acquire = drain(&commands)[0];
        let Request::Acquire {
            owner,
            expected_epoch: 6,
            ..
        } = acquire.1
        else {
            panic!("expected acquired epoch");
        };
        controls.tick(now + OP_TIMEOUT);
        let release = drain(&commands)[0];
        assert_eq!(
            release.1,
            Request::Release {
                owner,
                lease_epoch: 7
            }
        );
        let reply = controls.reply_after(connection, 0).unwrap().1;
        assert!(!reply.owned);
        assert_eq!(reply.reason, "timeout");
        result(
            &mut controls,
            &target,
            acquire.0,
            runtime::TerminalControlState {
                epoch: 7,
                owner: Some(owner),
                lease_ms: TTL_MS,
            },
            Status::Owned,
            now + OP_TIMEOUT,
        );
        assert_eq!(
            controls.reply_after(connection, 0).unwrap().1.reason,
            "timeout"
        );
        controls.tick(now + OP_TIMEOUT + CANCEL_TIMEOUT);
        assert!(controls.slots.is_empty());
        assert!(
            controls
                .timer_wait(now + OP_TIMEOUT + CANCEL_TIMEOUT)
                .is_none()
        );
    }

    #[test]
    fn resize_control_release_reacquire_correlates_latest_action_and_rejects_old_geometry() {
        let now = Instant::now();
        let (target, commands) = target(1, 1);
        let mut controls = ResizeControls::default();
        let connection = controls.register();
        let owner = grant(&mut controls, connection, &target, &commands, 1, 0, now);
        controls.request(connection, target.clone(), ControlAction::Release, 2, now);
        let release = drain(&commands)[0];
        controls.request(connection, target.clone(), ControlAction::Acquire, 3, now);
        let cancel = drain(&commands)[0];
        assert_eq!(release.1, cancel.1);
        assert_eq!(controls.reply_after(connection, 0).unwrap().1.request, 3);
        controls.resize(connection, &target.uuid, 1, 1, 40, 6);
        result(
            &mut controls,
            &target,
            cancel.0,
            runtime::TerminalControlState {
                epoch: 2,
                owner: None,
                lease_ms: 0,
            },
            Status::Released,
            now,
        );
        let query = drain(&commands)[0];
        assert_eq!(query.1, Request::Query);
        result(
            &mut controls,
            &target,
            query.0,
            runtime::TerminalControlState {
                epoch: 2,
                owner: None,
                lease_ms: 0,
            },
            Status::Released,
            now,
        );
        let acquire = drain(&commands)[0];
        let Request::Acquire {
            owner: next_owner, ..
        } = acquire.1
        else {
            panic!("fresh owner");
        };
        assert_ne!(owner, next_owner);
        result(
            &mut controls,
            &target,
            acquire.0,
            runtime::TerminalControlState {
                epoch: 3,
                owner: Some(next_owner),
                lease_ms: TTL_MS,
            },
            Status::Owned,
            now,
        );
        let reply = controls.reply_after(connection, 0).unwrap().1;
        assert_eq!(reply.request, 3);
        assert!(reply.owned);
        controls.request(connection, target.clone(), ControlAction::Release, 1, now);
        controls.request(connection, target.clone(), ControlAction::Release, 3, now);
        controls.resize(connection, &target.uuid, 1, 1, 55, 7);
        controls.tick(now);
        assert!(
            drain(&commands).is_empty(),
            "older or same-number different action cannot change current owner"
        );
    }

    #[test]
    fn resize_control_worker_busy_is_view_only_and_invalid_size_retains_confirmed_owner() {
        let now = Instant::now();
        let (target, commands) = target(1, 1);
        let mut controls = ResizeControls::default();
        let first = controls.register();
        let owner = grant(&mut controls, first, &target, &commands, 1, 0, now);
        let second = controls.register();
        controls.request(second, target.clone(), ControlAction::Acquire, 1, now);
        let query = drain(&commands)[0];
        let state = runtime::TerminalControlState {
            epoch: 1,
            owner: Some(owner),
            lease_ms: TTL_MS,
        };
        result(&mut controls, &target, query.0, state, Status::Owned, now);
        assert!(!controls.reply_after(second, 0).unwrap().1.owned);
        assert_eq!(controls.reply_after(second, 0).unwrap().1.reason, "in_use");
        assert!(drain(&commands).is_empty());
        controls.resize(first, &target.uuid, 1, 1, 400, 200);
        controls.tick(now);
        let resize = drain(&commands)[0];
        result(
            &mut controls,
            &target,
            resize.0,
            state,
            Status::InvalidSize,
            now,
        );
        let reply = controls.reply_after(first, 0).unwrap().1;
        assert!(reply.owned);
        assert_eq!(reply.reason, "invalid_size");
        controls.tick(now + RETRY_INTERVAL);
        assert!(
            drain(&commands).is_empty(),
            "denied geometry is not replayed automatically"
        );
    }

    #[test]
    fn resize_control_binding_cleanup_uses_captured_sink_and_bounded_cancel_capacity() {
        let now = Instant::now();
        let mut controls = ResizeControls::default();
        let mut old_commands = Vec::new();
        for generation in 1..=SLOT_CAP as u64 {
            let connection = controls.register();
            let (target, commands) = target(generation, 1);
            grant(&mut controls, connection, &target, &commands, 1, 0, now);
            controls.disconnect(connection, now);
            assert!(matches!(
                drain(&commands)[0].1,
                Request::Release { lease_epoch: 1, .. }
            ));
            old_commands.push(commands);
        }
        assert_eq!(controls.slots.len(), SLOT_CAP);
        let connection = controls.register();
        let (new_target, new_commands) = target(100, 1);
        controls.request(connection, new_target, ControlAction::Acquire, 1, now);
        assert_eq!(
            controls.reply_after(connection, 0).unwrap().1.reason,
            "capacity"
        );
        controls.tick(now + RETRY_INTERVAL);
        assert!(
            drain(&new_commands).is_empty(),
            "old numeric id cleanup must never use new worker sink"
        );
        for commands in old_commands {
            assert!(matches!(
                drain(&commands).as_slice(),
                [(_, Request::Release { lease_epoch: 1, .. })]
            ));
        }
        controls.tick(now + CANCEL_TIMEOUT);
        assert!(controls.slots.is_empty());
        assert!(controls.timer_wait(now + CANCEL_TIMEOUT).is_none());
    }

    #[test]
    fn resize_control_review_query_release_finishes_without_worker_ack() {
        let now = Instant::now();
        let (target, commands) = target(1, 1);
        let mut controls = ResizeControls::default();
        let connection = controls.register();
        controls.request(connection, target.clone(), ControlAction::Acquire, 1, now);
        let query = drain(&commands)[0];
        controls.request(connection, target.clone(), ControlAction::Release, 2, now);
        let reply = controls.reply_after(connection, 0).unwrap().1;
        assert_eq!(reply.request, 2);
        assert!(!reply.owned);
        assert_eq!(reply.reason, "released");
        assert!(controls.slots.is_empty());
        assert!(controls.timer_wait(now).is_none());
        result(
            &mut controls,
            &target,
            query.0,
            runtime::TerminalControlState {
                epoch: 0,
                owner: None,
                lease_ms: 0,
            },
            Status::Released,
            now,
        );
        assert!(
            drain(&commands).is_empty(),
            "late query must not acquire after release"
        );
        assert_eq!(
            controls.reply_after(connection, 0).unwrap().1.reason,
            "released"
        );
    }

    #[test]
    fn resize_control_review_attached_closing_deadline_publishes_timeout() {
        let now = Instant::now();
        let (target, commands) = target(1, 1);
        let mut controls = ResizeControls::default();
        let connection = controls.register();
        grant(&mut controls, connection, &target, &commands, 1, 0, now);
        controls.request(connection, target.clone(), ControlAction::Release, 2, now);
        let release = drain(&commands)[0];
        assert_eq!(
            controls.reply_after(connection, 0).unwrap().1.reason,
            "pending"
        );
        controls.tick(now + CANCEL_TIMEOUT);
        let reply = controls.reply_after(connection, 0).unwrap().1;
        assert_eq!(reply.request, 2);
        assert!(!reply.owned);
        assert_eq!(reply.reason, "timeout");
        assert!(controls.slots.is_empty());
        assert!(controls.timer_wait(now + CANCEL_TIMEOUT).is_none());
        result(
            &mut controls,
            &target,
            release.0,
            runtime::TerminalControlState {
                epoch: 2,
                owner: None,
                lease_ms: 0,
            },
            Status::Released,
            now + CANCEL_TIMEOUT,
        );
        assert_eq!(
            controls.reply_after(connection, 0).unwrap().1.reason,
            "timeout"
        );
    }

    #[test]
    fn resize_control_acquire_remains_view_only_until_worker_query_and_grant() {
        let commands = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&commands);
        let target = ControlTarget {
            uuid: "uuid".into(),
            generation: 1,
            session: runtime::SessionId(1),
            sink: Arc::new(move |command| captured.lock().unwrap().push(command)),
        };
        let mut controls = ResizeControls::default();
        let connection = controls.register();
        controls.request(
            connection,
            target,
            ControlAction::Acquire,
            1,
            Instant::now(),
        );
        let (_, reply) = controls
            .reply_after(connection, 0)
            .expect("pending correlated response");
        assert!(!reply.owned);
        assert_eq!(reply.reason, "pending");
        assert_eq!(reply.request, 1);
        assert!(matches!(
            commands.lock().unwrap().as_slice(),
            [runtime::RuntimeCommand::TerminalControl {
                request: runtime::TerminalControlRequest::Query,
                ..
            }]
        ));
    }
}
