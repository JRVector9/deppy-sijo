//! Runtime Boundary (설계문서 2장 / 9장).
//! UI는 RuntimeCommand를 보내고 RuntimeEvent를 받는다 —
//! SessionManager/PTY/terminal backend를 직접 만지지 않는다.

mod client;
mod command;
pub mod dotenv;
mod event;
mod host;
mod in_process;
mod input_admission;
pub use input_admission::{InputAdmission, InputPermit};
pub mod known_hosts;
mod persistence;
mod protocol;
mod remote;
mod resource_monitor;
pub mod tls_identity;

// deppy-sijo: 메모리 해제 훅 — hidden/exited 전환이 스크롤백을 압축·트림해 셀 배열을
// 해제하는 순간 호출된다. 앱이 여기에 mimalloc purge(해제 페이지를 OS로 반환)를 등록한다.
// mimalloc은 명시적 purge 없이는 해제 페이지를 free-list에 붙잡아 phys_footprint가 안
// 떨어지므로(실측 확인: 500ms 후에도 반환 0), 해제 시점에 이 훅으로 강제 반환한다.
static MEMORY_RELEASE_HOOK: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// 프로세스 전역 메모리 해제 훅을 등록한다(최초 1회). 앱 시작 시 mimalloc purge를 건다.
/// runtime은 mimalloc을 직접 알지 못하고, 이 훅으로만 신호한다(디커플링).
pub fn set_memory_release_hook(hook: fn()) {
    let _ = MEMORY_RELEASE_HOOK.set(hook);
}

/// 등록된 메모리 해제 훅을 호출한다(미등록이면 no-op). runtime worker 스레드에서
/// 스크롤백을 해제한 직후 부른다 — 훅(mimalloc `mi_collect`)은 스레드 안전하다.
pub(crate) fn signal_memory_released() {
    if let Some(hook) = MEMORY_RELEASE_HOOK.get() {
        hook();
    }
}

pub use client::{
    RuntimeClient, RuntimeCommandSendError, RuntimeCommandSink, RuntimeEventReceiver,
    RuntimeEventStream,
};
pub use command::{
    MuxPaneId, MuxTabId, RuntimeCommand, RuntimeCommandPreparationErrorCode,
    RuntimeCommandRetention, SessionId, SplitDirection, TERMINAL_CELL_COUNT_MAX,
    WorkspaceRuntimeState, checked_runtime_command_retention_total,
    prepare_runtime_command_for_retention,
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
    AGENT_EXIT_SENTINEL_PREFIX, SessionStatus, SessionStatusView, StatusConfidence, StatusSource,
    UserStatusOverride, agent_exit_sentinel_path,
};

#[cfg(test)]
mod memory_release_hook_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CALLS: AtomicUsize = AtomicUsize::new(0);
    fn hook() {
        CALLS.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn 등록된_훅이_signal마다_호출된다() {
        super::set_memory_release_hook(hook);
        let before = CALLS.load(Ordering::SeqCst);
        super::signal_memory_released();
        super::signal_memory_released();
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            before + 2,
            "훅이 signal마다 불려야 함"
        );
    }
}

mod resize;
pub use resize::{ResizeFailure, ResizeStamp, ResizeToken};
