//! Bounded retry scheduling for a previously authenticated SSH attachment.
//! No commands or input are retained here; the saved resume proof owns session recovery.
use crate::ssh_workspace_add::SshWorkspaceError;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub(crate) struct SshReconnect {
    established: bool,
    blocked: bool,
    attempts: u8,
    retry_at: Option<Instant>,
}

impl SshReconnect {
    pub(crate) fn connected(&mut self) {
        self.established = true;
        self.blocked = false;
        self.attempts = 0;
        self.retry_at = None;
    }

    pub(crate) fn automatic(&self) -> bool {
        self.established && !self.blocked
    }

    pub(crate) fn failed(&mut self, error: SshWorkspaceError) {
        if !matches!(
            error,
            SshWorkspaceError::ConnectionFailed | SshWorkspaceError::WorkerUnavailable
        ) {
            self.block();
        }
    }

    pub(crate) fn block(&mut self) {
        self.blocked = true;
        self.retry_at = None;
    }

    pub(crate) fn disconnected(&mut self, now: Instant) {
        if self.automatic() && self.retry_at.is_none() {
            let seconds = (1u64 << self.attempts.min(5)).min(30);
            self.retry_at = now.checked_add(Duration::from_secs(seconds));
        }
    }

    pub(crate) fn delay(&self, now: Instant) -> Option<Duration> {
        self.retry_at.map(|at| at.saturating_duration_since(now))
    }

    pub(crate) fn started(&mut self) {
        self.retry_at = None;
        self.attempts = self.attempts.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_reconnect_backoff_is_bounded_and_success_resets_it() {
        let now = Instant::now();
        let mut state = SshReconnect::default();
        state.disconnected(now);
        assert_eq!(
            state.delay(now),
            None,
            "first authentication requires explicit action"
        );
        state.connected();
        for seconds in [1, 2, 4, 8, 16, 30, 30, 30] {
            state.disconnected(now);
            assert_eq!(state.delay(now), Some(Duration::from_secs(seconds)));
            state.started();
        }
        state.connected();
        state.disconnected(now);
        assert_eq!(state.delay(now), Some(Duration::from_secs(1)));
    }

    #[test]
    fn ssh_reconnect_authentication_identity_and_expiry_errors_require_user_action() {
        for error in [
            SshWorkspaceError::AuthenticationFailed,
            SshWorkspaceError::HostKeyChanged,
            SshWorkspaceError::SessionExpired,
            SshWorkspaceError::HelperFailed,
            SshWorkspaceError::PermissionDenied,
            SshWorkspaceError::RequestFailed,
        ] {
            let now = Instant::now();
            let mut state = SshReconnect::default();
            state.connected();
            state.disconnected(now);
            state.failed(error);
            state.disconnected(now);
            assert!(!state.automatic(), "{error:?}");
            assert_eq!(state.delay(now), None, "{error:?}");
        }
    }
}
