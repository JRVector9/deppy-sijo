//! Runtime Boundary (설계문서 2장 / 9장).
//! UI는 RuntimeCommand를 보내고 RuntimeEvent를 받는다 —
//! SessionManager/PTY/terminal backend를 직접 만지지 않는다.

mod client;
mod command;
pub mod dotenv;
mod event;
mod host;
mod in_process;
pub mod known_hosts;
mod persistence;
mod protocol;
mod remote;
mod resource_monitor;
pub mod tls_identity;

pub use client::{RuntimeClient, RuntimeCommandSink, RuntimeEventReceiver, RuntimeEventStream};
pub use command::{
    MuxPaneId, MuxTabId, RuntimeCommand, SessionId, SplitDirection, WorkspaceRuntimeState,
};
pub use event::{AgentConfigCorrelationId, MessageArg, MessagePayload, RuntimeEvent, SpawnKind};
pub use host::{
    InProcessRuntimeHostFactory, RuntimeCommandDispatcher, RuntimeHost, RuntimeHostConfig,
    RuntimeHostFactory, RuntimeSecret, RuntimeSecretResolver, RuntimeWake,
};
pub use in_process::InProcessRuntimeClient;
pub use mux::{LayoutNode, MuxSnapshot, PaneSnapshot, TabSnapshot};
pub use persistence::PersistConfig;
pub use pty::{
    ProcessIdentitySource, PtyInputEnqueueResult, PtyInputPressure, PtyInputQueuePolicy,
    PtyInputRejectReason,
};
pub use remote::{RemoteRuntimeClient, RemoteRuntimeServer, TofuOutcome};
pub use resource_monitor::{
    ProcessResourceMonitor, ProcessResourceMonitorConfig, ProcessResourceSnapshot,
    SessionResourceTarget, SessionResourceUsage,
};
// 웹 계층(P5c)이 Viewport 이벤트의 스냅샷을 인코딩할 때 쓴다 — terminal 크레이트에
// 직접 의존하는 대신 runtime 경유로 노출해 의존 표면을 한 곳으로 유지한다.
pub use terminal::{
    CellRange, CursorShape, CursorSnapshot, TerminalCell, TerminalViewportSnapshot,
};

pub use session::{
    SessionStatus, SessionStatusView, StatusConfidence, StatusSource, UserStatusOverride,
};
