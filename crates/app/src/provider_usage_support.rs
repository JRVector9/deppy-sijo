//! Bounded terminal text projection for account-usage probes, never a PTY input path.

/// Strip complete OSC/CSI sequences independently. A partial final sequence stays hidden until
/// the collector's next pass over its bounded accumulated buffer supplies its terminator.
pub(crate) fn strip_terminal_controls(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut visible = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b']') {
            index += 2;
            while index < bytes.len() {
                if bytes[index] == 0x07 {
                    index += 1;
                    break;
                }
                if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'\\') {
                    index += 2;
                    break;
                }
                index += 1;
            }
        } else if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'[') {
            index += 2;
            while index < bytes.len() {
                let byte = bytes[index];
                index += 1;
                if (0x40..=0x7e).contains(&byte) {
                    break;
                }
            }
        } else {
            let character = text[index..].chars().next().expect("valid UTF-8 boundary");
            visible.push(character);
            index += character.len_utf8();
        }
    }
    visible
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_usage_harness_controls_preserve_mixed_terminators_and_unicode() {
        assert_eq!(
            strip_terminal_controls(
                "\u{1b}]0;x\u{1b}\\한글\u{1b}[31m 23%\u{1b}[0m\u{1b}]8;;url\u{7} 값\u{1b}]8;;\u{1b}\\"
            ),
            "한글 23% 값"
        );
    }

    #[test]
    fn provider_usage_harness_controls_hide_incomplete_sequences_until_complete() {
        assert_eq!(strip_terminal_controls("23%\u{1b}]private"), "23%");
        assert_eq!(strip_terminal_controls("23%\u{1b}[31"), "23%");
        assert_eq!(
            strip_terminal_controls("23%\u{1b}]private\u{1b}\\ 84%"),
            "23% 84%"
        );
    }
}
