//! Local, explicitly shared terminal MCP bridge. All effects stay on the App thread.
mod history;
mod server;
pub use history::{Claim, History, Record};
use serde_json::{Value, json};
pub use server::{Auth, Request, Server, TOKEN_TTL, now};

pub const MAX_TEXT: usize = 8 * 1024;
pub const MAX_ANSWER: usize = 16 * 1024;

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
    let identity = json!({"session_id":{"type":"string"},"generation":{"type":"string"},"operation_id":{"type":"string","description":"Unique ID for this action. Reuse only for the exact same action; never retry unknown input with a new ID."}});
    let mut result = vec![
        json!({"name":"list_sessions","description":"List explicitly shared Deppy terminal sessions. Keep the UUID and generation for subsequent calls.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true}}),
        json!({"name":"read_output","description":"Read the latest visible terminal screen when it changes. Not a lossless stdout log. Pass returned cursor; reset=true means resynchronize.","inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"generation":{"type":"string"},"cursor":{"type":"integer","minimum":0}},"required":["session_id","generation"],"additionalProperties":false},"annotations":{"readOnlyHint":true}}),
    ];
    for (name, description, extra, required) in [
        (
            "send_text",
            "Type into the exact shared session only if input is allowed. submit defaults false; true appends Enter. Control characters/newlines are rejected. queued means runtime admission, not execution/completion. Never automatically retry an unknown outcome.",
            json!({"text":{"type":"string","description":"Maximum 8192 UTF-8 bytes; no control characters or newlines"},"submit":{"type":"boolean","default":false}}),
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
    json!({"tools":result})
}

#[cfg(test)]
mod tests;
