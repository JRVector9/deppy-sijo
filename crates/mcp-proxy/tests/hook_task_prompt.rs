use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::json;

struct HookFixture {
    root: PathBuf,
    db_path: PathBuf,
    workspace: String,
}

impl HookFixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("deppy-hook-titles-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let db_path = root.join("metadata.sqlite3");
        let workspace = storage::Db::open(&db_path)
            .unwrap()
            .create_workspace("hook-titles")
            .unwrap();
        Self {
            root,
            db_path,
            workspace,
        }
    }

    fn hook(&self, pane: u64, provider: &str, payload: &serde_json::Value) {
        self.hook_with_provider(pane, Some(provider), payload);
    }

    fn hook_with_provider(&self, pane: u64, provider: Option<&str>, payload: &serde_json::Value) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_deppy-mcp-proxy"));
        command
            .args(["hooks", "--event", "observe", "--db"])
            .arg(&self.db_path)
            .env("HOME", &self.root)
            .env("DEPPY_SESSION_ID", format!("{}:{pane}", self.workspace))
            .env("DEPPY_RUNTIME_ID", "isolated-hook-title-runtime")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(provider) = provider {
            command.args(["--provider", provider]);
        }
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "{}");
    }

    fn snapshot(&self) -> Vec<storage::HookSessionRow> {
        let db = storage::Db::open(&self.db_path).unwrap();
        let mut job = storage::AgentStateJob::projection(&self.workspace);
        job.include_attention = false;
        job.include_agent_sessions = false;
        job.include_structured_threads = false;
        job.include_archived_threads = false;
        db.apply_agent_state_job(&job).unwrap().hook_sessions
    }

    fn transcript(&self, provider: &str, native: &str, body: &str) -> PathBuf {
        let directory = self.root.join(format!(".{provider}"));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("{native}.jsonl"));
        std::fs::write(&path, body).unwrap();
        path
    }
}

impl Drop for HookFixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

fn prompt_payload(native: &str, transcript: &Path, prompt: &str) -> serde_json::Value {
    json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": native,
        "transcript_path": transcript,
        "prompt": prompt,
    })
}

#[test]
fn independent_hook_titles_run_proxy_to_storage_projection_for_all_four_providers() {
    let fixture = HookFixture::new();
    for (pane, provider) in [(1, "claude"), (2, "codex"), (3, "grok"), (4, "kimi-code")] {
        let native = format!("{provider}-native");
        let prompt = format!("{provider} pane 작업을 고쳐");
        let transcript = fixture.transcript(provider, &native, &prompt);
        fixture.hook(
            pane,
            provider.trim_end_matches("-code"),
            &prompt_payload(&native, &transcript, &prompt),
        );
        let row = fixture
            .snapshot()
            .into_iter()
            .find(|row| row.session_key == format!("{}:{pane}", fixture.workspace))
            .unwrap();
        assert_eq!(row.kind, provider.trim_end_matches("-code"));
        assert_eq!(row.agent_session_id, native);
        assert_eq!(
            row.task_prompt.as_deref(),
            Some(prompt.as_str()),
            "{provider}"
        );
        assert_eq!(std::fs::read_to_string(transcript).unwrap(), prompt);
    }
}

#[test]
fn independent_hook_titles_normalize_long_wrapped_korean_before_preview_budget() {
    let fixture = HookFixture::new();
    let transcript = fixture.transcript("claude", "native", "original transcript");
    fixture.hook(
        1,
        "claude",
        &prompt_payload("native", &transcript, "old task"),
    );
    let human = "가".repeat(160);
    let wrapped = format!(
        "<pasted_content id='{}'>{human}</pasted_content>",
        "x".repeat(240)
    );
    let original = prompt_payload("native", &transcript, &wrapped);
    fixture.hook(1, "claude", &original);
    let row = fixture.snapshot().pop().unwrap();
    assert_eq!(row.task_prompt, Some("가".repeat(85)));
    assert_eq!(original["prompt"], wrapped);
    assert_eq!(
        std::fs::read_to_string(transcript).unwrap(),
        "original transcript"
    );
    for internal in [
        format!("<task-notification>{}", "가".repeat(160)),
        format!(
            "<pasted_content id='{}'><agent-message from='child'>{}</agent-message></pasted_content>",
            "x".repeat(240),
            "가".repeat(160)
        ),
    ] {
        fixture.hook(
            1,
            "claude",
            &prompt_payload(
                "native",
                &fixture.root.join(".claude/native.jsonl"),
                &internal,
            ),
        );
        assert_eq!(
            fixture.snapshot().pop().unwrap().task_prompt,
            Some("가".repeat(85))
        );
    }
}

#[test]
fn independent_hook_titles_grok_native_camel_case_prompt_event() {
    let fixture = HookFixture::new();
    let transcript = fixture.transcript("grok", "native", "original transcript");
    fixture.hook(
        1,
        "grok",
        &json!({
            "hookEventName": "user_prompt_submit",
            "sessionId": "native",
            "transcriptPath": transcript,
            "prompt": "Grok 현재 작업",
        }),
    );
    assert_eq!(
        fixture.snapshot().pop().unwrap().task_prompt.as_deref(),
        Some("Grok 현재 작업")
    );
}

#[test]
fn independent_hook_titles_path_free_prompts_require_exact_provider_and_native() {
    let fixture = HookFixture::new();
    let transcript = fixture.transcript("codex", "native", "original transcript");
    fixture.hook(
        1,
        "codex",
        &prompt_payload("native", &transcript, "old task"),
    );
    for (provider, native) in [
        (Some("claude"), "native"),
        (Some("grok"), "native"),
        (Some("kimi"), "native"),
        (Some("codex"), "other-native"),
        (None, "native"),
    ] {
        fixture.hook_with_provider(1, provider, &json!({
            "hook_event_name": "UserPromptSubmit", "session_id": native, "prompt": "untrusted task",
        }));
        assert_eq!(
            fixture.snapshot().pop().unwrap().task_prompt.as_deref(),
            Some("old task")
        );
    }
    fixture.hook(
        1,
        "codex",
        &json!({
            "hook_event_name": "UserPromptSubmit", "session_id": "native", "prompt": "current task",
        }),
    );
    let row = fixture.snapshot().pop().unwrap();
    assert_eq!(row.task_prompt.as_deref(), Some("current task"));
    assert_eq!(row.execution_stamp, row.task_prompt_execution_stamp);
    let owner = deppy_core::process_identity::ProcessIdentity::capture(std::process::id()).unwrap();
    assert!(row.execution_stamp.unwrap().contains(owner));
}

#[test]
fn independent_hook_titles_provider_path_mismatch_cannot_rebind_or_overwrite() {
    let fixture = HookFixture::new();
    let transcript = fixture.transcript("claude", "native", "original transcript");
    fixture.hook(
        1,
        "claude",
        &prompt_payload("native", &transcript, "current task"),
    );
    fixture.hook(
        1,
        "codex",
        &prompt_payload("wrong-native", &transcript, "untrusted task"),
    );
    let row = fixture.snapshot().pop().unwrap();
    assert_eq!(row.kind, "claude");
    assert_eq!(row.agent_session_id, "native");
    assert_eq!(row.task_prompt.as_deref(), Some("current task"));
}

#[test]
fn independent_hook_titles_kimi_path_free_binding_uses_isolated_native_index() {
    let fixture = HookFixture::new();
    let directory = fixture.root.join(".kimi-code/session-native");
    std::fs::create_dir_all(directory.join("agents/main")).unwrap();
    let transcript = directory.join("agents/main/wire.jsonl");
    std::fs::write(&transcript, "original Kimi transcript").unwrap();
    std::fs::write(
        fixture.root.join(".kimi-code/session_index.jsonl"),
        json!({
            "sessionId": "native", "sessionDir": directory,
        })
        .to_string(),
    )
    .unwrap();
    fixture.hook(1, "kimi", &json!({
        "hook_event_name": "UserPromptSubmit", "session_id": "native", "agent_id": "main", "prompt": "Kimi 当前 작업",
    }));
    let row = fixture.snapshot().pop().unwrap();
    assert_eq!(row.kind, "kimi");
    assert_eq!(row.agent_session_id, "native");
    assert_eq!(row.transcript_path, transcript.to_string_lossy());
    assert_eq!(row.task_prompt.as_deref(), Some("Kimi 当前 작업"));
    assert_eq!(
        std::fs::read_to_string(transcript).unwrap(),
        "original Kimi transcript"
    );
}

#[test]
fn independent_hook_titles_exact_path_free_prompt_revives_binding_ttl() {
    let fixture = HookFixture::new();
    let transcript = fixture.transcript("codex", "native", "original transcript");
    fixture.hook(
        1,
        "codex",
        &prompt_payload("native", &transcript, "old task"),
    );
    let connection = rusqlite::Connection::open(&fixture.db_path).unwrap();
    connection
        .execute(
            "UPDATE agent_hook_sessions SET updated_at=CAST(strftime('%s','now') AS INTEGER)-86401",
            [],
        )
        .unwrap();
    for (provider, native, prompt) in [
        ("claude", "native", "wrong provider"),
        ("codex", "other-native", "wrong native"),
        ("codex", "native", "<task-notification>internal event"),
    ] {
        fixture.hook(
            1,
            provider,
            &json!({
                "hook_event_name": "UserPromptSubmit", "session_id": native, "prompt": prompt,
            }),
        );
        assert!(
            fixture.snapshot().is_empty(),
            "unmatched/internal prompt must not refresh TTL"
        );
    }
    fixture.hook(
        1,
        "codex",
        &json!({
            "hook_event_name": "UserPromptSubmit", "session_id": "native", "prompt": "current task",
        }),
    );
    let rows = fixture.snapshot();
    assert_eq!(
        rows.len(),
        1,
        "exact current prompt must revive the retained binding"
    );
    assert_eq!(rows[0].task_prompt.as_deref(), Some("current task"));
    assert_eq!(rows[0].execution_stamp, rows[0].task_prompt_execution_stamp);
}

#[test]
fn independent_hook_titles_all_providers_update_path_free_native_prompt_events() {
    let fixture = HookFixture::new();
    for (pane, provider, directory) in [
        (1, "claude", "claude"),
        (2, "codex", "codex"),
        (3, "grok", "grok"),
        (4, "kimi", "kimi-code"),
    ] {
        let transcript = fixture.transcript(directory, "shared-native", "original transcript");
        fixture.hook(
            pane,
            provider,
            &prompt_payload("shared-native", &transcript, "old task"),
        );
        let prompt = format!("{provider} current pane task");
        let payload = if provider == "grok" {
            json!({"hookEventName":"user_prompt_submit", "sessionId":"shared-native", "prompt":prompt})
        } else if provider == "kimi" {
            json!({"hook_event_name":"UserPromptSubmit", "session_id":"shared-native", "agent_id":"main", "prompt":prompt})
        } else {
            json!({"hook_event_name":"UserPromptSubmit", "session_id":"shared-native", "prompt":prompt})
        };
        fixture.hook(pane, provider, &payload);
        let row = fixture
            .snapshot()
            .into_iter()
            .find(|row| row.session_key == format!("{}:{pane}", fixture.workspace))
            .unwrap();
        assert_eq!(row.kind, provider);
        assert_eq!(row.task_prompt.as_deref(), Some(prompt.as_str()));
        assert_eq!(row.execution_stamp, row.task_prompt_execution_stamp);
        assert_eq!(row.transcript_path, transcript.to_string_lossy());
        assert_eq!(
            std::fs::read_to_string(transcript).unwrap(),
            "original transcript"
        );
    }
}
