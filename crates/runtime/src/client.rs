use crate::command::RuntimeCommand;
use crate::event::RuntimeEvent;

pub type RuntimeEventReceiver = std::sync::mpsc::Receiver<RuntimeEvent>;

/// 설계문서 2.2 RuntimeClient trait 3형제.
pub trait RuntimeCommandSink {
    fn send_command(&self, command: RuntimeCommand) -> anyhow::Result<()>;
}

pub trait RuntimeEventStream {
    fn subscribe(&self) -> RuntimeEventReceiver;
}

pub trait RuntimeClient: RuntimeCommandSink + RuntimeEventStream {}
