//! Runtime Boundary (설계문서 2장 / 9장).
//! UI는 RuntimeCommand를 보내고 RuntimeEvent를 받는다 —
//! SessionManager/PTY/terminal backend를 직접 만지지 않는다.

mod client;
mod command;
mod event;
mod in_process;
mod remote;

pub use client::{RuntimeClient, RuntimeCommandSink, RuntimeEventReceiver, RuntimeEventStream};
pub use command::{MuxPaneId, MuxTabId, RuntimeCommand, SessionId, SplitDirection};
pub use event::{RuntimeEvent, SpawnKind};
pub use in_process::InProcessRuntimeClient;
pub use mux::{LayoutNode, MuxSnapshot, PaneSnapshot, TabSnapshot};
pub use remote::{RemoteRuntimeClient, RemoteRuntimeServer};
pub use session::SessionStatus;
