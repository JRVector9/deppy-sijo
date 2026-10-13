//! Non-secret, opt-in transport recovery. No terminal operations are retained.
use std::time::{Duration, Instant};

// Native clocks advance at the same rate while awake; only `continuous` includes sleep.
// Their absolute origins need not match because recovery compares elapsed durations.
#[derive(Clone, Copy)]
pub(super) struct ClockSample {
    pub continuous: Duration,
    pub awake: Duration,
}

const RESUME_TOLERANCE: Duration = Duration::from_secs(1);

impl ClockSample {
    fn suspended_since(self, last: Self) -> Option<Duration> {
        let continuous = self.continuous.checked_sub(last.continuous)?;
        let awake = self.awake.checked_sub(last.awake)?;
        Some(continuous.saturating_sub(awake))
    }
}

pub(super) fn input_current(accepted: Option<ClockSample>) -> bool {
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    {
        permits_input(accepted, clock_sample())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = accepted;
        true
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", windows, test))]
fn permits_input(accepted: Option<ClockSample>, current: Option<ClockSample>) -> bool {
    accepted.zip(current).is_some_and(|(accepted, current)| {
        current
            .suspended_since(accepted)
            .is_some_and(|suspended| suspended <= RESUME_TOLERANCE)
    })
}

pub(super) fn clock_sample() -> Option<ClockSample> {
    #[cfg(target_os = "macos")]
    {
        // Apple's clock_gettime(3): both RAW clocks ignore clock/frequency adjustments,
        // but UPTIME_RAW (mach_absolute_time) stops while the system is asleep.
        sample_unix_clocks(libc::CLOCK_UPTIME_RAW, libc::CLOCK_MONOTONIC_RAW)
    }
    #[cfg(target_os = "linux")]
    {
        // BOOTTIME is MONOTONIC plus time spent suspended; both share rate adjustments.
        sample_unix_clocks(libc::CLOCK_MONOTONIC, libc::CLOCK_BOOTTIME)
    }
    #[cfg(windows)]
    {
        // QueryUnbiasedInterruptTime excludes sleep; GetTickCount64 includes it.
        // Both use the system interrupt clock, rather than the adjustable wall clock.
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn QueryUnbiasedInterruptTime(at: *mut u64) -> i32;
        }
        let awake = || {
            let mut at = 0;
            // SAFETY: the API writes one u64 into a live, exclusively borrowed value.
            if unsafe { QueryUnbiasedInterruptTime(&mut at) } == 0 {
                return None;
            }
            Some(Duration::from_nanos(at.checked_mul(100)?))
        };
        let before = awake()?;
        // SAFETY: GetTickCount64 takes no arguments and has no caller requirements.
        let continuous = Duration::from_millis(unsafe {
            windows_sys::Win32::System::SystemInformation::GetTickCount64()
        });
        bounded_sample(before, continuous, awake()?)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        None
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn sample_unix_clocks(awake: libc::clockid_t, continuous: libc::clockid_t) -> Option<ClockSample> {
    fn read(clock: libc::clockid_t) -> Option<Duration> {
        let mut at = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: clock_gettime writes one timespec into a live, exclusively borrowed value.
        if unsafe { libc::clock_gettime(clock, &mut at) } != 0 {
            return None;
        }
        let nanos = u32::try_from(at.tv_nsec).ok()?;
        if nanos >= 1_000_000_000 {
            return None;
        }
        Some(Duration::new(u64::try_from(at.tv_sec).ok()?, nanos))
    }
    bounded_sample(read(awake)?, read(continuous)?, read(awake)?)
}

#[cfg(any(target_os = "macos", target_os = "linux", windows, test))]
fn bounded_sample(before: Duration, continuous: Duration, awake: Duration) -> Option<ClockSample> {
    // Do not mistake a scheduler delay between reads for sleep. Windows clock
    // granularity is at most tens of milliseconds; the resume tolerance is larger.
    (awake.checked_sub(before)? <= Duration::from_millis(100))
        .then_some(ClockSample { continuous, awake })
}

#[derive(Default)]
pub(super) struct Recovery {
    enabled: bool,
    attempts: u8,
    due: Option<Instant>,
    last_sample: Option<ClockSample>,
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
    pub fn frame(&mut self, clocks: Option<ClockSample>) -> bool {
        let Some(clocks) = clocks else {
            // A failed read cannot rule out suspend. Keep the last verified
            // sample so the next valid frame can still detect accumulated sleep.
            return false;
        };
        let Some(last) = self.last_sample.replace(clocks) else {
            return false;
        };
        // Normal egui idle advances both clocks equally. Only newly accumulated
        // suspend time fences approvals; timer granularity/read jitter does not.
        clocks
            .suspended_since(last)
            .is_some_and(|suspended| suspended > RESUME_TOLERANCE)
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
        let mut recovery = Recovery::default();
        assert!(!recovery.frame(clocks(100, 50)));
        assert!(!recovery.frame(clocks(110, 60)));
        assert!(recovery.frame(clocks(150, 61)));
        assert!(!recovery.frame(clocks(160, 71)));
        assert!(recovery.due().is_none());
    }
    #[test]
    fn normal_idle_frame_gap_does_not_require_revalidation() {
        let mut recovery = Recovery::default();
        assert!(!recovery.frame(clocks(100, 50)));
        assert!(
            !recovery.frame(clocks(220, 170)),
            "an idle UI frame interval is not evidence that the OS suspended"
        );
    }

    fn clocks(continuous: u64, awake: u64) -> Option<ClockSample> {
        Some(ClockSample {
            continuous: Duration::from_secs(continuous),
            awake: Duration::from_secs(awake),
        })
    }

    #[test]
    fn unavailable_sample_preserves_known_baseline_for_later_suspend_detection() {
        let mut recovery = Recovery::default();
        assert!(!recovery.frame(clocks(100, 50)));
        assert!(!recovery.frame(None));
        assert!(recovery.frame(clocks(140, 51)));
        assert!(!recovery.frame(clocks(150, 61)));
    }

    #[test]
    fn unavailable_sample_preserves_known_baseline_without_inventing_idle_suspend() {
        let mut recovery = Recovery::default();
        assert!(!recovery.frame(clocks(100, 50)));
        assert!(!recovery.frame(None));
        assert!(!recovery.frame(clocks(220, 170)));
    }

    #[test]
    fn backward_clocks_reset_the_baseline_without_idle_revalidation() {
        let mut recovery = Recovery::default();
        assert!(!recovery.frame(clocks(1000, 60)));
        assert!(!recovery.frame(clocks(900, 30)));
        assert!(!recovery.frame(clocks(910, 40)));
        assert!(!recovery.frame(clocks(920, 35)));
        assert!(!recovery.frame(clocks(930, 45)));
        assert!(recovery.frame(clocks(940, 46)));
    }

    #[test]
    fn sampled_clock_granularity_and_preemption_do_not_create_resume_events() {
        let mut recovery = Recovery::default();
        assert!(!recovery.frame(clocks(100, 50)));
        assert!(!recovery.frame(Some(ClockSample {
            continuous: Duration::from_secs(110),
            awake: Duration::from_millis(59_950),
        })));
        assert!(bounded_sample(Duration::ZERO, Duration::ZERO, Duration::from_secs(2)).is_none());
        assert!(bounded_sample(Duration::from_secs(2), Duration::ZERO, Duration::ZERO).is_none());
    }

    #[test]
    fn queued_input_continuity_rejects_suspend_and_unavailable_or_backward_clocks() {
        let accepted = clocks(100, 50);
        assert!(permits_input(accepted, clocks(220, 170)));
        assert!(!permits_input(accepted, clocks(102, 50)));
        assert!(!permits_input(None, clocks(102, 52)));
        assert!(!permits_input(accepted, None));
        assert!(!permits_input(accepted, clocks(99, 50)));
        assert!(!permits_input(accepted, clocks(101, 49)));
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn native_clock_sample_is_available_without_creating_an_initial_resume() {
        let mut recovery = Recovery::default();
        let sample = clock_sample();
        assert!(sample.is_some());
        assert!(!recovery.frame(sample));
        assert!(!recovery.frame(clock_sample()));
    }
}
