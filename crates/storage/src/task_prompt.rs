//! Shared task-title projection for transcript, hook and persisted pane metadata.
//! Only known leading transport wrappers are consumed; coding HTML stays literal.

use std::borrow::Cow;

const MAX_WRAPPER_BYTES: usize = 512;
const MAX_WRAPPER_DEPTH: usize = 8;

fn leading_wrapper(text: &str) -> Option<&'static str> {
    ["pasted_content", "image"].into_iter().find(|name| {
        text.strip_prefix('<')
            .and_then(|rest| rest.strip_prefix(name))
            .is_some_and(|rest| rest.starts_with('>') || rest.starts_with(char::is_whitespace))
    })
}

fn after_opening_tag(text: &str) -> Option<&str> {
    let mut quote = None;
    for (offset, ch) in text.char_indices() {
        if offset >= MAX_WRAPPER_BYTES {
            return None;
        }
        if let Some(open) = quote {
            if ch == open {
                quote = None;
            }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
        } else if ch == '>' {
            return Some(&text[offset + 1..]);
        }
    }
    None
}

/// Project the actual task text before any summary truncation or persisted title display.
/// Claude's pasted_content marker can be open-ended; a closing tag is optional.
/// Limits keep malformed/nested transport metadata from consuming unbounded work.
pub fn task_prompt_text(prompt: &str) -> Option<Cow<'_, str>> {
    let mut visible = prompt.trim();
    let mut pending_closings = 0;
    for _ in 0..MAX_WRAPPER_DEPTH {
        let Some(name) = leading_wrapper(visible) else {
            break;
        };
        visible = after_opening_tag(visible)?.trim();
        if name == "pasted_content" {
            pending_closings += 1;
            if let Some(body) = visible.strip_suffix("</pasted_content>") {
                visible = body.trim();
                pending_closings -= 1;
            }
        }
    }
    if leading_wrapper(visible).is_some() {
        return None;
    }
    // A closed block can be followed by more user instructions. Preserve both
    // sides instead of dropping the follow-up or leaking its closing marker.
    let visible = if pending_closings > 0 && visible.contains("</pasted_content>") {
        Cow::Owned(
            visible
                .replacen("</pasted_content>", "", pending_closings)
                .trim()
                .to_owned(),
        )
    } else {
        Cow::Borrowed(visible)
    };
    task_body_is_displayable(&visible).then_some(visible)
}

/// Internal events cannot replace user task titles, including inside a transport wrapper.
pub fn task_prompt_is_displayable(prompt: &str) -> bool {
    task_prompt_text(prompt).is_some()
}

/// Whether a Claude message describes user work rather than an internal event.
/// Shared with transcript parsing and used when reading already-polluted hook rows.
fn task_body_is_displayable(prompt: &str) -> bool {
    let prompt = prompt.trim_start();
    let agent_envelope = |text: &str| {
        text.strip_prefix("<agent-message").is_some_and(|rest| {
            rest.chars()
                .next()
                .is_some_and(|ch| ch == '>' || ch.is_ascii_whitespace())
        })
    };
    let relayed_agent_envelope = prompt
        .strip_prefix("Another Claude session sent a message:")
        .is_some_and(|rest| agent_envelope(rest.trim_start()));
    !prompt.is_empty()
        && !agent_envelope(prompt)
        && !relayed_agent_envelope
        && !prompt.starts_with("[Subagent hand-back]")
        && ![
            "<task-notification",
            "<system-reminder",
            "<local-command",
            "<command-name",
            "<environment_context",
            "<permissions",
            "<INSTRUCTIONS",
            "<heartbeat",
        ]
        .iter()
        .any(|prefix| prompt.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_transport_unwraps_open_closed_nested_and_quoted_markers() {
        for text in [
            "<pasted_content id=\"e89a\">\n한글 경로 작업을 고쳐",
            "<pasted_content id='x'>한글 경로 작업을 고쳐</pasted_content>",
            "<image path='a>b'><pasted_content id='x'>한글 경로 작업을 고쳐",
            "<pasted_content id='a'><pasted_content id='b'>한글 경로 작업을 고쳐</pasted_content></pasted_content>",
        ] {
            assert_eq!(
                task_prompt_text(text).as_deref(),
                Some("한글 경로 작업을 고쳐")
            );
        }
    }

    #[test]
    fn pasted_transport_keeps_followup_after_closed_block() {
        assert_eq!(
            task_prompt_text("<pasted_content id='x'>첫 지시</pasted_content>\n추가 지시도 실행해")
                .as_deref(),
            Some("첫 지시\n추가 지시도 실행해")
        );
    }

    #[test]
    fn pasted_transport_preserves_literal_code_and_rejects_empty_internal_or_malformed_metadata() {
        for text in [
            "<div> 태그 렌더링을 고쳐",
            "<pasted_content-example> 코드 예시",
            "```xml\n<pasted_content id='x'>\n```",
        ] {
            assert_eq!(task_prompt_text(text).as_deref(), Some(text));
        }
        for text in [
            "<pasted_content id='x'></pasted_content>",
            "<pasted_content id='x'",
            "<pasted_content id='x'><task-notification>internal",
            "<image path='x'><agent-message from='subagent'>internal",
        ] {
            assert_eq!(task_prompt_text(text), None);
        }
        assert_eq!(
            task_prompt_text(&format!(
                "<pasted_content id='{}'>work",
                "x".repeat(MAX_WRAPPER_BYTES)
            )),
            None
        );
        assert_eq!(
            task_prompt_text(&format!(
                "{}work",
                "<pasted_content>".repeat(MAX_WRAPPER_DEPTH + 1)
            )),
            None
        );
    }
}
