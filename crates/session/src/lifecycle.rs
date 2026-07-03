/// 세션 수명주기 (PR-08 완료 기준: lifecycle 명확화).
///
/// ```text
/// Running ──(EOF/exit)──▶ Exited
/// ```
/// Exited 후에도 terminal 상태(scrollback)는 세션이 drop될 때까지 유지된다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLifecycle {
    Running,
    Exited { exit_code: Option<u32> },
}

impl SessionLifecycle {
    pub fn is_running(&self) -> bool {
        matches!(self, SessionLifecycle::Running)
    }
}
