//! Runtime Boundary (설계문서 2장 / 9장).
//! UI는 RuntimeCommand를 보내고 RuntimeEvent를 받는다 —
//! SessionManager/PTY/terminal backend를 직접 만지지 않는다.

mod client;
mod command;
mod event;
mod in_process;

pub use client::{RuntimeClient, RuntimeCommandSink, RuntimeEventReceiver, RuntimeEventStream};
pub use command::{RuntimeCommand, SessionId};
pub use event::RuntimeEvent;
pub use in_process::InProcessRuntimeClient;
