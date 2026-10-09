//! Replay sanitized provider contracts through the real reader, reducer and projection.
use super::{
    confirm_grok_completion_in, normalize, reconcile_claude_results, reconcile_codex_results,
};
use serde::Deserialize;
use serde_json::Value;
use storage::{AgentStateJob, Db};

const KEY: &str = "00000000-0000-0000-0000-000000000001:1";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    provider: String,
    name: String,
    steps: Vec<Step>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    at: i64,
    payload: Value,
    #[serde(default)]
    raw_payload: Option<String>,
    expected: Facts,
    #[serde(default)]
    reopen: bool,
    #[serde(default)]
    grok_journal: Option<Vec<Value>>,
    #[serde(default = "accepted_default")]
    accepted: bool,
}

fn accepted_default() -> bool {
    true
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Facts {
    working: bool,
    waiting: bool,
    response: bool,
    done: bool,
    pending: usize,
    idle: Option<i64>,
}

fn scenarios() -> Vec<Scenario> {
    serde_json::from_str(include_str!("../tests/fixtures/agent-status.json")).unwrap()
}

fn replay(provider: &str) {
    let cases = scenarios();
    assert!(cases.iter().any(|case| case.provider == provider));
    for case in cases.into_iter().filter(|case| case.provider == provider) {
        let dir =
            std::env::temp_dir().join(format!("deppy-status-harness-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        let mut db = Db::open(&path).unwrap();
        for (index, step) in case.steps.iter().enumerate() {
            if step.reopen {
                drop(db);
                db = Db::open(&path).unwrap();
            }
            if let Some(rows) = &step.grok_journal {
                let folder = dir.join(".grok/sessions/%2Ffixture/native");
                std::fs::create_dir_all(&folder).unwrap();
                let text = rows
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n");
                std::fs::write(folder.join("events.jsonl"), text).unwrap();
            }
            let raw = step
                .raw_payload
                .clone()
                .unwrap_or_else(|| step.payload.to_string());
            let metadata = crate::hook_payload::read(raw.as_bytes())
                .map(|v| crate::tag_hook_provider(v, Some(provider)));
            let event = metadata
                .as_ref()
                .and_then(|v| normalize(v, step.at * 1_000_000));
            assert_eq!(
                event.is_some(),
                step.accepted,
                "{provider}/{} step{index}: normalization",
                case.name
            );
            if let Some(mut event) = event {
                confirm_grok_completion_in(metadata.as_ref().unwrap(), &mut event, &dir);
                db.record_agent_attention(KEY, &event).unwrap();
            }
            if let Some(metadata) = &metadata {
                reconcile_codex_results(&db, KEY, metadata, step.at * 1_000_000);
                reconcile_claude_results(&db, KEY, metadata, step.at * 1_000_000);
            }
            let snapshot = db
                .apply_agent_state_job(&AgentStateJob::projection("other"))
                .unwrap();
            let facts = Facts {
                working: snapshot.working_sessions.iter().any(|key| key == KEY),
                waiting: snapshot.waiting_sessions.iter().any(|(key, _)| key == KEY),
                response: snapshot.response_sessions.iter().any(|key| key == KEY),
                done: snapshot
                    .turn_done_sessions
                    .iter()
                    .any(|(key, _)| key == KEY),
                pending: db.pending_agent_request_ids(KEY, "native").unwrap().len(),
                idle: snapshot
                    .idle_sessions
                    .iter()
                    .find(|(key, _, _)| key == KEY)
                    .map(|(_, at, _)| *at),
            };
            assert_eq!(facts, step.expected, "{provider}/{} step{index}", case.name);
        }
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
        println!("agent-status-harness: {provider}/{} PASS", case.name);
    }
}

#[test]
fn agent_status_harness_claude() {
    replay("claude");
}
#[test]
fn agent_status_harness_codex() {
    replay("codex");
}
#[test]
fn agent_status_harness_kimi() {
    replay("kimi");
}
#[test]
fn agent_status_harness_grok() {
    replay("grok");
}
#[test]
fn agent_status_harness_cursor() {
    replay("cursor");
}

#[test]
fn agent_status_harness_inventory_has_unique_nonempty_named_cases() {
    let cases = scenarios();
    let mut names = std::collections::HashSet::new();
    for case in cases {
        assert!(["claude", "codex", "kimi", "grok", "cursor"].contains(&case.provider.as_str()));
        assert!(!case.name.is_empty() && !case.steps.is_empty());
        assert!(names.insert((case.provider, case.name)));
    }
}
