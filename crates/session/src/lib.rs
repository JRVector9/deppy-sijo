//! Session Runtime (설계문서 3장 / 9장).
//! session = PTY 프로세스 + terminal 상태 + lifecycle.
//! pane(mux)과 분리되어 독립 생존한다 — attach/detach(v1)의 전제.
//! secret 의존 금지 (PR-08 완료 기준): env는 호출측이 resolve를 끝낸
//! 평문 쌍으로만 받는다.

mod agent_session;
mod lifecycle;
mod session;
mod shell_session;
mod status;

pub use agent_session::spawn_agent;
pub use lifecycle::SessionLifecycle;
pub use session::{PumpResult, Session, SessionKind};
pub use shell_session::spawn_shell;
pub use status::{
    SessionStatus, SessionStatusView, StatusConfidence, StatusDetector, StatusPatterns,
    StatusSource, UserStatusOverride,
};
