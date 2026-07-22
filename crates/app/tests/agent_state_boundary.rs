const APP_SOURCE: &str = include_str!("../src/app.rs");

fn production_source() -> &'static str {
    APP_SOURCE
        .split_once("\n#[cfg(test)]\nmod tests")
        .expect("app.rs must keep one trailing cfg(test) module")
        .0
}

fn compact(source: &str) -> String {
    source
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

#[test]
fn legacy_agent_state_orchestration_is_absent_from_production() {
    let production = production_source();
    let surviving = [
        "apply_agent_persistence_batch",
        "agent_persistence_retry_at",
        "TranscriptFinder::new",
        "crate::agent_detect::transcript_cwd",
        "cached_project_name",
        "project_name_cache",
        "import_persisted_threads",
    ]
    .into_iter()
    .filter_map(|symbol| {
        let count = production.matches(symbol).count();
        (count > 0).then(|| format!("{symbol}={count}"))
    })
    .collect::<Vec<_>>();
    assert!(
        surviving.is_empty(),
        "legacy AgentState production symbols survived the worker cutover: {}",
        surviving.join(", ")
    );
}

#[test]
fn legacy_app_owned_agent_storage_calls_are_absent_from_production() {
    let production = compact(production_source());
    let surviving = [
        "self.db.list_hook_sessions_for_prefix_bounded(",
        "self.db.list_statuslines_for_prefix_bounded(",
        "self.db.list_waiting_sessions_bounded(",
        "self.db.list_turn_done_sessions_for_prefix_bounded(",
        "self.db.list_agent_sessions_bounded(",
        "self.db.list_structured_threads_bounded(",
        "self.db.list_persisted_activity_panes_bounded(",
        "self.db.upsert_agent_session(",
        "self.db.delete_agent_session(",
        "self.db.clear_agent_turn_done(",
        "db.upsert_structured_thread(",
        "db.set_structured_thread_archived(",
        "db.delete_structured_thread(",
    ]
    .into_iter()
    .filter_map(|call| {
        let count = production.matches(call).count();
        (count > 0).then(|| format!("{call}={count}"))
    })
    .collect::<Vec<_>>();
    assert!(
        surviving.is_empty(),
        "legacy direct AgentState storage calls survived the worker cutover: {}",
        surviving.join(", ")
    );
}

#[test]
fn legacy_transcript_probe_receivers_are_absent_from_production() {
    let production = compact(production_source());
    let surviving = ["finder.find(", "transcript_finder.find("]
        .into_iter()
        .filter_map(|call| {
            let count = production.matches(call).count();
            (count > 0).then(|| format!("{call}={count}"))
        })
        .collect::<Vec<_>>();
    assert!(
        surviving.is_empty(),
        "legacy synchronous transcript probes survived the worker cutover: {}",
        surviving.join(", ")
    );
}

#[test]
fn production_uses_agent_state_worker_snapshots_and_final_reconcile() {
    let production = production_source();
    let missing = [
        "AgentStateWorker",
        "replace_persisted_threads",
        "set_session_project_names",
        "shutdown_with_final_binding_reconcile",
    ]
    .into_iter()
    .filter(|symbol| !production.contains(symbol))
    .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "AgentState worker cutover is incomplete; production callsites are missing: {}",
        missing.join(", ")
    );
}
