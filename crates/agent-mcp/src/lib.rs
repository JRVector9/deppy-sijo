//! Local, explicitly shared terminal MCP bridge. All effects stay on the App thread.
mod history;
mod management;
mod oauth;
mod principal;
mod protocol;
pub use management::{
    ArtifactReference, CloudRun, MAX_RUN_BYTES, MAX_RUN_PAGE_BYTES, MAX_RUNS, OperationContext,
    ProgressProvenance, RunActor, RunPage, RunQuery, RunState, RunUpdate, operation_key,
};
pub use oauth::Approval;
pub use principal::{AuthKind, ConnectionInfo, ConnectionState, LEGACY_CONNECTION_ID, Principal};
mod server;
pub use history::{Claim, History, HistoryPage, HistoryQuery, Record};
use serde_json::{Value, json};
pub use server::{Auth, Request, Server, TOKEN_TTL, now, valid_public_host};

pub const MAX_TEXT: usize = 8 * 1024;
pub const MAX_PASTE: usize = 32 * 1024;
pub const MAX_ANSWER: usize = 16 * 1024;

/// Validate before the shared Composer encoder; it must never silently strip unsafe bytes.
pub fn validate_paste_text(text: &str, submit: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        text.len() <= MAX_PASTE && (submit || !text.is_empty()),
        "invalid_paste_size"
    );
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        anyhow::ensure!(
            !c.is_control()
                || matches!(c, '\n' | '\t')
                || (c == '\r' && chars.peek() == Some(&'\n')),
            "paste_contains_unsafe_control"
        );
    }
    // Empty Enter remains available through legacy send_text, not a paste operation.
    anyhow::ensure!(!text.is_empty(), "invalid_paste_size");
    Ok(())
}

pub fn encode_input(text: &str, submit: bool) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        text.len() <= MAX_TEXT && (submit || !text.is_empty()),
        "invalid_text_size"
    );
    anyhow::ensure!(
        !text.chars().any(char::is_control),
        "text_contains_control_or_newline"
    );
    let mut bytes = text.as_bytes().to_vec();
    if submit {
        bytes.push(b'\r');
    }
    Ok(bytes)
}

pub fn tools() -> Value {
    let identity = json!({"session_id":{"type":"string"},"generation":{"type":"string"},"operation_id":{"type":"string","description":"Unique ID for this action. Reuse only for the exact same action; never retry unknown input with a new ID."},"run_id":{"type":"string","description":"Optional run owned by this authenticated connection and exact session generation."}});
    let mut result = vec![
        json!({"name":"list_sessions","description":"List explicitly shared Deppy terminal sessions. Keep the UUID and generation for subsequent calls.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true}}),
        json!({"name":"read_output","description":"Read the latest observed terminal screen and request a refresh. The cached result may be stale; null means unchanged cache, not proof of no new output. Retry after retry_after_ms for a refreshed screen. Not a lossless stdout log. Pass returned cursor; reset=true means resynchronize.","inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"generation":{"type":"string"},"cursor":{"type":"integer","minimum":0}},"required":["session_id","generation"],"additionalProperties":false},"annotations":{"readOnlyHint":true}}),
    ];
    for (name, description, extra, required) in [
        (
            "send_text",
            "Type into the exact shared session only if input is allowed. submit defaults false; true appends Enter. Control characters/newlines are rejected. queued means PTY queue admission, not execution/completion. Never automatically retry an unknown outcome.",
            json!({"text":{"type":"string","description":"Maximum 8192 UTF-8 bytes; no control characters or newlines"},"submit":{"type":"boolean","default":false}}),
            vec!["text"],
        ),
        (
            "paste_text",
            "Explicitly paste into the exact shared session. Maximum 32768 UTF-8 bytes; JSON/envelope bounds may reject earlier. LF/CRLF and tabs require verified bracketed paste, otherwise rejected. submit defaults false; true sends one separate Enter atomically with the body. Known AI execution/draft/dialog changes reject input. queued means admission, not execution/completion. Never automatically retry unknown input.",
            json!({"text":{"type":"string","description":"Maximum 32768 UTF-8 bytes; LF, CRLF and tab accepted only with verified bracketed paste. Other controls and ESC rejected."},"submit":{"type":"boolean","default":false}}),
            vec!["text"],
        ),
        (
            "send_ctrl_c",
            "Send Ctrl+C once to the exact shared session if input is allowed.",
            json!({}),
            vec![],
        ),
        (
            "notify",
            "Send YOUR OWN answer back to Deppy. REQUIRED after completing an analysis or task: call this with your final answer, even if you did not type any terminal commands. Stores it in the original session's local history and notification center, never terminal stdin.",
            json!({"message":{"type":"string","description":"Maximum 16384 UTF-8 bytes"}}),
            vec!["message"],
        ),
    ] {
        let mut properties = identity.as_object().unwrap().clone();
        properties.extend(extra.as_object().unwrap().clone());
        let mut req = vec!["session_id", "generation", "operation_id"];
        req.extend(required);
        result.push(json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":req,"additionalProperties":false}}));
    }
    let session = json!({"session_id":{"type":"string","maxLength":128},"generation":{"type":"string","maxLength":128}});
    for (name, description, extra, required, read_only) in [
        (
            "start_run",
            "Register bounded cloud work for this authenticated connection and exact shared session. Registration is not proof of terminal execution or completion.",
            json!({"run_id":{"type":"string","maxLength":128},"title":{"type":"string","maxLength":256},"instruction":{"type":"string","description":"Maximum 8192 UTF-8 bytes"}}),
            vec!["session_id", "generation", "run_id", "title"],
            false,
        ),
        (
            "update_run",
            "Report this connection's cloud run status and heartbeat. Agent reports are separate from observed terminal state. A cancelled report acknowledges an existing owner cancellation request.",
            json!({"run_id":{"type":"string","maxLength":128},"state":{"type":"string","enum":["queued","running","waiting","completed","failed","cancelled"]},"progress":{"type":"integer","minimum":0,"maximum":100},"message":{"type":"string","description":"Maximum 4096 UTF-8 bytes"},"result":{"type":"string","description":"Maximum 16384 UTF-8 bytes"},"artifacts":{"type":"array","maxItems":16,"items":{"type":"object","properties":{"label":{"type":"string","maxLength":128},"uri":{"type":"string","maxLength":2048}},"required":["label","uri"],"additionalProperties":false}}}),
            vec!["session_id", "generation", "run_id", "state"],
            false,
        ),
        (
            "get_run",
            "Read this connection's run without effects. An unknown or stale run is not completed.",
            json!({"run_id":{"type":"string","maxLength":128}}),
            vec!["session_id", "generation", "run_id"],
            true,
        ),
        (
            "list_runs",
            "Read a bounded page of this connection's authorized runs. Agent identity is supplied by the verified credential.",
            json!({"state":{"type":"string","enum":["queued","running","waiting","completed","failed","cancelled","unknown"]},"before":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1,"maximum":100},"max_bytes":{"type":"integer","minimum":1,"maximum":1048576}}),
            vec![],
            true,
        ),
        (
            "read_history",
            "Read bounded, redacted terminal history for the exact shared session generation. Observe reset/truncated markers; history is an observation, not proof of cloud task completion.",
            json!({"cursor":{"type":"integer","minimum":0},"max_bytes":{"type":"integer","minimum":1024,"maximum":65536}}),
            vec!["session_id", "generation"],
            true,
        ),
        (
            "get_operation",
            "Read this connection's durable receipt for an operation ID without replaying input. An unknown receipt does not authorize automatically retrying an uncertain input.",
            json!({"operation_id":{"type":"string","maxLength":128}}),
            vec!["session_id", "generation", "operation_id"],
            true,
        ),
        (
            "acquire_control",
            "Request this connection/run's temporary input control for the exact shared session. Read-only credentials cannot acquire input control. Local owner takeover revokes queued admission.",
            json!({"run_id":{"type":"string","maxLength":128},"ttl_seconds":{"type":"integer","minimum":1,"maximum":120}}),
            vec!["session_id", "generation"],
            false,
        ),
        (
            "release_control",
            "Release only this connection/run's input control for the exact shared session without sending terminal input.",
            json!({"run_id":{"type":"string","maxLength":128}}),
            vec!["session_id", "generation"],
            false,
        ),
    ] {
        let mut properties = session.as_object().unwrap().clone();
        properties.extend(extra.as_object().unwrap().clone());
        result.push(json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},"annotations":{"readOnlyHint":read_only}}));
    }
    json!({"tools":result})
}

#[cfg(test)]
mod tests;
