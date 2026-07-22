#[path = "../src/logging.rs"]
mod logging;

#[test]
fn module_is_ready_for_root_wiring_without_background_construction() {
    assert_eq!(logging::APP_LOG_LINE_MAX_BYTES, 64 * 1024);
    assert_eq!(logging::APP_LOG_QUEUE_LINES, 1_024);
    assert_eq!(logging::APP_LOG_ACTIVE_FILE_MAX_BYTES, 8 * 1024 * 1024);
    assert_eq!(logging::APP_LOG_TOTAL_MAX_BYTES, 32 * 1024 * 1024);
    assert_eq!(logging::APP_LOG_RETENTION_DAYS, 7);
    assert_eq!(logging::APP_LOG_MANAGED_FILE_PROBE_LIMIT, 32);

    let root = std::env::temp_dir().join(format!(
        "deppy-logging-production-constructor-{}",
        std::process::id()
    ));
    let (sink, _) = logging::open_app_log_sink(&root).unwrap();
    drop(sink);
    std::fs::remove_dir_all(root).unwrap();
}
