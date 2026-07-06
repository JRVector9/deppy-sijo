use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use deppy_core::SessionId;

use crate::command::RuntimeCommand;
use crate::event::RuntimeEvent;

/// 이벤트 수신측. 상태 이벤트(저빈도 제어)와 Viewport(고빈도 출력)를 분리한다 (설계문서 8.2):
/// Viewport는 세션별 최신본 하나만 유지하는 slot이라 소비가 늦어도 누적되지 않는다 (14.5).
pub struct RuntimeEventReceiver {
    pub(crate) events: std::sync::mpsc::Receiver<RuntimeEvent>,
    pub(crate) viewports: Arc<Mutex<HashMap<SessionId, RuntimeEvent>>>,
    pub(crate) input_pressures: Arc<Mutex<HashMap<SessionId, RuntimeEvent>>>,
}

impl RuntimeEventReceiver {
    /// 쌓인 상태 이벤트 + 세션별 최신 Viewport를 순서대로 돌려준다 (프레임마다 호출).
    ///
    /// slot을 먼저 take하고 나서 채널을 비운다 — worker는 상태 이벤트를 채널에
    /// 넣은 뒤에 Viewport slot을 쓰므로(happens-before), slot에 Viewport가
    /// 있다면 그 세션의 ShellSpawned는 반드시 이번 채널 드레인에 포함된다.
    /// (반대 순서면 Viewport가 자신의 ShellSpawned보다 먼저 도착할 수 있다)
    pub fn drain(&self) -> Vec<RuntimeEvent> {
        let viewports: Vec<RuntimeEvent> = self
            .viewports
            .lock()
            .expect("viewport slot lock")
            .drain()
            .map(|(_, event)| event)
            .collect();
        let input_pressures: Vec<RuntimeEvent> = self
            .input_pressures
            .lock()
            .expect("input pressure slot lock")
            .drain()
            .map(|(_, event)| event)
            .collect();
        let mut out: Vec<RuntimeEvent> = self.events.try_iter().collect();
        out.extend(input_pressures);
        out.extend(viewports);
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
