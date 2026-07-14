use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use deppy_core::SessionId;

use crate::command::RuntimeCommand;
use crate::event::RuntimeEvent;

/// 구독자별 durable lifecycle 이벤트 큐 상한. Viewport/pressure/resource는 별도 최신값
/// slot이라 이 큐를 쓰지 않는다. 포화 시 느린 구독자를 끊어 메모리 상한을 지킨다.
pub(crate) const LOCAL_EVENT_QUEUE_CAP: usize = 1024;
/// UI 한 프레임에서 durable 이벤트를 처리할 최대 수. backlog가 남으면 최신값 slot은
/// 다음 프레임으로 미뤄 Spawn/Mux보다 Viewport가 먼저 보이는 순서 역전을 막는다.
const DURABLE_DRAIN_BUDGET: usize = 256;

/// 이벤트 수신측. 상태 이벤트(저빈도 제어)와 Viewport(고빈도 출력)를 분리한다 (설계문서 8.2):
/// Viewport는 세션별 최신본 하나만 유지하는 slot이라 소비가 늦어도 누적되지 않는다 (14.5).
pub struct RuntimeEventReceiver {
    pub(crate) events: std::sync::mpsc::Receiver<RuntimeEvent>,
    /// drain budget을 확인하려고 한 건 미리 꺼낸 이벤트를 다음 drain까지 보관한다.
    pub(crate) pending_durable: Mutex<Option<RuntimeEvent>>,
    /// 송신 큐 포화로 이 구독자가 disconnect됐는지 소비자에게 한 번 surface한다.
    pub(crate) overflowed: Arc<AtomicBool>,
    pub(crate) viewports: Arc<Mutex<HashMap<SessionId, RuntimeEvent>>>,
    pub(crate) input_pressures: Arc<Mutex<HashMap<SessionId, RuntimeEvent>>>,
    /// ResourceUsage 최신본 slot — 주기 샘플(유일한 무한 반복 이벤트 소스)이라 느린
    /// 소비자에게도 채널에 누적되지 않게 latest-value로 덮어쓴다(안정성 감사 High #1).
    pub(crate) resource_usage: Arc<Mutex<Option<RuntimeEvent>>>,
}

impl RuntimeEventReceiver {
    pub(crate) fn try_recv_durable(&self) -> Result<RuntimeEvent, std::sync::mpsc::TryRecvError> {
        if let Some(event) = self
            .pending_durable
            .lock()
            .expect("pending durable lock")
            .take()
        {
            return Ok(event);
        }
        self.events.try_recv()
    }

    /// durable 큐 포화가 있었으면 true를 한 번 반환한다.
    pub fn take_overflowed(&self) -> bool {
        self.overflowed.swap(false, Ordering::AcqRel)
    }

    /// overflow로 송신측이 이 구독자를 끊은 뒤, 이미 큐에 들어온 durable 이벤트를 모두
    /// 소비했는지 확인한다. 한 건을 미리 읽으면 pending에 돌려놔 순서를 보존한다.
    pub fn durable_backlog_exhausted(&self) -> bool {
        if self
            .pending_durable
            .lock()
            .expect("pending durable lock")
            .is_some()
        {
            return false;
        }
        match self.events.try_recv() {
            Ok(event) => {
                *self.pending_durable.lock().expect("pending durable lock") = Some(event);
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty)
            | Err(std::sync::mpsc::TryRecvError::Disconnected) => true,
        }
    }

    /// 쌓인 상태 이벤트 + 세션별 최신 Viewport를 순서대로 돌려준다 (프레임마다 호출).
    ///
    /// slot을 먼저 take하고 나서 채널을 비운다 — worker는 상태 이벤트를 채널에
    /// 넣은 뒤에 Viewport slot을 쓰므로(happens-before), slot에 Viewport가
    /// 있다면 그 세션의 ShellSpawned는 반드시 이번 채널 드레인에 포함된다.
    /// (반대 순서면 Viewport가 자신의 ShellSpawned보다 먼저 도착할 수 있다)
    pub fn drain(&self) -> Vec<RuntimeEvent> {
        // 최신값 slot을 먼저 떼어 둔 뒤 durable을 비운다. worker는 Spawn → Viewport 순으로
        // emit하므로, 여기서 관측한 Viewport의 Spawn은 반드시 아래 durable drain 대상이다.
        // durable을 먼저 비우면 그 사이 들어온 Viewport만 이번 프레임에 보여 순서가 뒤집힌다.
        let viewports: Vec<(SessionId, RuntimeEvent)> = self
            .viewports
            .lock()
            .expect("viewport slot lock")
            .drain()
            .collect();
        let input_pressures: Vec<(SessionId, RuntimeEvent)> = self
            .input_pressures
            .lock()
            .expect("input pressure slot lock")
            .drain()
            .collect();
        let resource = self
            .resource_usage
            .lock()
            .expect("resource usage slot lock")
            .take();

        let mut out = Vec::with_capacity(DURABLE_DRAIN_BUDGET);
        while out.len() < DURABLE_DRAIN_BUDGET {
            match self.try_recv_durable() {
                Ok(event) => out.push(event),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }
        // budget을 다 썼으면 한 건만 미리 확인한다. backlog가 있으면 control 이벤트를
        // 다음 프레임에 먼저 처리하고 최신값 slot은 아직 꺼내지 않는다.
        if out.len() == DURABLE_DRAIN_BUDGET {
            match self.events.try_recv() {
                Ok(event) => {
                    *self.pending_durable.lock().expect("pending durable lock") = Some(event);
                    // durable 순서를 지키기 위해 최신값은 다음 drain으로 되돌린다. 그 사이
                    // worker가 더 새 값을 넣었다면 콘텐츠는 새 값을 보존하되, 되돌리는
                    // 이벤트의 dirty 델타는 합쳐 넘긴다 — 버리면 그 행들이 renderer
                    // 재shaping에서 빠져 stale로 남는다 (event.rs 헬퍼 주석).
                    let mut current_viewports = self.viewports.lock().expect("viewport slot lock");
                    for (session, event) in viewports {
                        match current_viewports.entry(session) {
                            std::collections::hash_map::Entry::Occupied(mut occupied) => {
                                crate::event::merge_unconsumed_viewport_dirty(
                                    &event,
                                    occupied.get_mut(),
                                );
                            }
                            std::collections::hash_map::Entry::Vacant(vacant) => {
                                vacant.insert(event);
                            }
                        }
                    }
                    drop(current_viewports);
                    let mut current_pressures = self
                        .input_pressures
                        .lock()
                        .expect("input pressure slot lock");
                    for (session, event) in input_pressures {
                        current_pressures.entry(session).or_insert(event);
                    }
                    drop(current_pressures);
                    if let Some(resource) = resource {
                        let mut current = self
                            .resource_usage
                            .lock()
                            .expect("resource usage slot lock");
                        if current.is_none() {
                            *current = Some(resource);
                        }
                    }
                    return out;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
            }
        }

        out.extend(resource);
        out.extend(input_pressures.into_iter().map(|(_, event)| event));
        out.extend(viewports.into_iter().map(|(_, event)| event));
        out
    }
}

/// 설계문서 2.2 RuntimeClient trait 3형제.
pub trait RuntimeCommandSink {
    fn send_command(&self, command: RuntimeCommand) -> anyhow::Result<()>;
}

pub trait RuntimeEventStream {
    fn subscribe(&self) -> RuntimeEventReceiver;
}

pub trait RuntimeClient: RuntimeCommandSink + RuntimeEventStream {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_drain은_프레임_budget에서_멈추고_나머지를_보존한다() {
        let (tx, rx) = std::sync::mpsc::channel();
        for id in 0..(DURABLE_DRAIN_BUDGET + 17) {
            tx.send(RuntimeEvent::ShellSpawned {
                session: SessionId(id as u64),
            })
            .unwrap();
        }
        let receiver = RuntimeEventReceiver {
            events: rx,
            pending_durable: Mutex::new(None),
            overflowed: Arc::default(),
            viewports: Arc::default(),
            input_pressures: Arc::default(),
            resource_usage: Arc::new(Mutex::new(Some(RuntimeEvent::ResourceUsage {
                snapshot: crate::resource_monitor::ProcessResourceSnapshot {
                    pid: 1,
                    sampled_at_ms: 1,
                    rss_bytes: 1,
                    cpu_percent: None,
                    high_cpu: false,
                    high_rss: false,
                },
                session_usage: Vec::new(),
            }))),
        };
        assert_eq!(receiver.drain().len(), DURABLE_DRAIN_BUDGET);
        assert!(!receiver.durable_backlog_exhausted());
        let tail = receiver.drain();
        assert_eq!(tail.len(), 18);
        assert!(matches!(
            tail.last(),
            Some(RuntimeEvent::ResourceUsage { .. })
        ));
        assert!(receiver.durable_backlog_exhausted());
    }
}
