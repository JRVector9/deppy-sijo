//! Non-secret, opt-in transport recovery. No terminal operations are retained.
use std::time::{Duration, Instant};

#[derive(Default)]
pub(super) struct Recovery {
    enabled: bool,
    attempts: u8,
    due: Option<Instant>,
    last_frame: Option<Instant>,
}

impl Recovery {
    pub fn start(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.attempts = 0;
        self.due = None;
    }
    pub fn stop(&mut self) {
        self.start(false);
    }
    pub fn success(&mut self) {
        self.attempts = 0;
        self.due = None;
    }
    pub fn failed(&mut self, at: Instant) {
        if self.enabled && self.attempts < 6 && self.due.is_none() {
            let seconds = 2_u64.pow(u32::from(self.attempts)).min(30);
            self.attempts += 1;
            self.due = Some(at + Duration::from_secs(seconds));
        }
    }
    pub fn retry(&mut self, at: Instant) -> bool {
        if self.due.is_some_and(|due| at >= due) {
            self.due = None;
            true
        } else {
            false
        }
    }
    pub fn due(&self) -> Option<Instant> {
        self.due
    }
    pub fn frame(&mut self, at: Instant) -> bool {
        self.last_frame
            .replace(at)
            .is_some_and(|last| at.saturating_duration_since(last) > Duration::from_secs(30))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retry_is_opt_in_capped_and_explicit_stop_cancels_it() {
        let at = Instant::now();
        let mut recovery = Recovery::default();
        recovery.failed(at);
        assert!(recovery.due().is_none());
        recovery.start(true);
        for (index, seconds) in [1, 2, 4, 8, 16, 30].into_iter().enumerate() {
            recovery.failed(at);
            assert_eq!(recovery.attempts, index as u8 + 1);
            assert!(!recovery.retry(at));
            assert!(recovery.retry(at + Duration::from_secs(seconds)));
        }
        recovery.failed(at);
        assert!(recovery.due().is_none());
        recovery.success();
        recovery.failed(at);
        recovery.stop();
        assert!(!recovery.retry(at + Duration::from_secs(100)));
    }
    #[test]
    fn sleep_gap_requires_revalidation_without_creating_input_replays() {
        let at = Instant::now();
        let mut recovery = Recovery::default();
        assert!(!recovery.frame(at));
        assert!(!recovery.frame(at + Duration::from_secs(10)));
        assert!(recovery.frame(at + Duration::from_secs(50)));
        assert!(recovery.due().is_none());
    }
}
