#![allow(dead_code)]
#[path = "/Users/jr/Desktop/projects/deppy-sijo-performance/crates/app/src/cloud_agent/continuity.rs"]
mod continuity;

#[test]
fn known_pre_suspend_baseline_survives_failed_sample() {
    use std::time::Duration;
    let mut recovery = continuity::Recovery::default();
    let sample = |continuous, awake| Some(continuity::ClockSample {
        continuous: Duration::from_secs(continuous),
        awake: Duration::from_secs(awake),
    });
    assert!(!recovery.frame(sample(100, 50)));
    assert!(!recovery.frame(None));
    assert!(
        recovery.frame(sample(140, 51)),
        "a transient failed sample must not erase the last known baseline and hide 39 seconds of suspend"
    );
}
