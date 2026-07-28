use std::time::{Duration, Instant};

pub(super) const CLIENT_LIVENESS_TIMEOUT: Duration = Duration::from_secs(45);

pub(super) struct LivenessTracker {
    last_received: Instant,
    timeout: Duration,
}

impl LivenessTracker {
    pub(super) fn new(now: Instant, timeout: Duration) -> Self {
        Self {
            last_received: now,
            timeout,
        }
    }

    pub(super) fn observe_frame(&mut self, now: Instant) {
        self.last_received = now;
    }

    pub(super) fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_received) >= self.timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liveness_tracker_expires_at_exact_timeout_boundary() {
        let start = Instant::now();
        let tracker = LivenessTracker::new(start, Duration::from_secs(45));

        assert!(tracker.expired(start + Duration::from_secs(45)));
    }

    #[test]
    fn liveness_tracker_stays_alive_just_before_timeout_boundary() {
        let start = Instant::now();
        let tracker = LivenessTracker::new(start, Duration::from_secs(45));

        assert!(!tracker.expired(start + Duration::from_secs(45) - Duration::from_nanos(1)));
    }

    #[test]
    fn liveness_tracker_observed_frame_resets_timeout_window() {
        let start = Instant::now();
        let mut tracker = LivenessTracker::new(start, Duration::from_secs(45));
        let observed = start + Duration::from_secs(30);

        tracker.observe_frame(observed);

        assert!(!tracker.expired(observed + Duration::from_secs(45) - Duration::from_nanos(1)));
        assert!(tracker.expired(observed + Duration::from_secs(45)));
    }

    #[test]
    fn liveness_tracker_client_timeout_is_three_heartbeat_intervals() {
        assert_eq!(
            CLIENT_LIVENESS_TIMEOUT,
            super::super::HEARTBEAT_INTERVAL * 3
        );
    }
}
