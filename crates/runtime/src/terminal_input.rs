/// Direct terminal input is mapped by the worker against the live terminal modes.
/// Text cannot carry arbitrary control sequences; keys use an explicit whitelist.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum TerminalInput {
    Text {
        text: String,
        paste: bool,
    },
    Key {
        key: String,
        ctrl: bool,
        alt: bool,
        shift: bool,
        meta: bool,
    },
}

pub const DIRECT_INPUT_TEXT_BYTES_MAX: usize = 256 * 1024;

impl TerminalInput {
    pub(crate) fn requires_current_modes(&self) -> bool {
        match self {
            Self::Text { paste, .. } => *paste,
            Self::Key {
                key,
                ctrl,
                alt,
                shift,
                meta,
            } => {
                !(*ctrl || *alt || *shift || *meta)
                    && matches!(
                        key.as_str(),
                        "up" | "down" | "left" | "right" | "home" | "end"
                    )
            }
        }
    }

    pub(crate) fn payload(&self) -> &String {
        match self {
            Self::Text { text, .. } => text,
            Self::Key { key, .. } => key,
        }
    }

    pub(crate) fn payload_mut(&mut self) -> &mut String {
        match self {
            Self::Text { text, .. } => text,
            Self::Key { key, .. } => key,
        }
    }

    pub(crate) fn is_valid(&self) -> bool {
        match self {
            Self::Text { text, .. } => text.len() <= DIRECT_INPUT_TEXT_BYTES_MAX,
            Self::Key {
                key,
                ctrl,
                alt,
                shift,
                meta,
            } => key.len() <= 32 && encode_key(key, *ctrl, *alt, *shift, *meta, false).is_some(),
        }
    }

    pub(crate) fn encode(
        &self,
        bracketed_paste: bool,
        application_cursor: bool,
    ) -> Option<Vec<u8>> {
        if !self.is_valid() {
            return None;
        }
        match self {
            Self::Text { text, paste } => {
                let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
                let cleaned: String = normalized
                    .chars()
                    .filter(|c| !c.is_control() || (*paste && matches!(*c, '\n' | '\t')))
                    .collect();
                if cleaned.is_empty() {
                    return None;
                }
                let body = cleaned.replace('\n', "\r");
                Some(terminal::input_mapper::paste_bytes(
                    body.as_bytes(),
                    *paste && bracketed_paste,
                ))
            }
            Self::Key {
                key,
                ctrl,
                alt,
                shift,
                meta,
            } => encode_key(key, *ctrl, *alt, *shift, *meta, application_cursor),
        }
    }
}

fn encode_key(
    key: &str,
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
    application_cursor: bool,
) -> Option<Vec<u8>> {
    if meta {
        return None;
    }
    let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let final_byte = match key {
        "up" => Some('A'),
        "down" => Some('B'),
        "right" => Some('C'),
        "left" => Some('D'),
        "home" => Some('H'),
        "end" => Some('F'),
        _ => None,
    };
    if let Some(final_byte) = final_byte {
        return Some(if modifier != 1 {
            format!("\x1b[1;{modifier}{final_byte}").into_bytes()
        } else if application_cursor {
            format!("\x1bO{final_byte}").into_bytes()
        } else {
            format!("\x1b[{final_byte}").into_bytes()
        });
    }
    let tilde = match key {
        "insert" => Some(2),
        "delete" => Some(3),
        "page_up" => Some(5),
        "page_down" => Some(6),
        _ => None,
    };
    if let Some(tilde) = tilde {
        return Some(if modifier == 1 {
            format!("\x1b[{tilde}~").into_bytes()
        } else {
            format!("\x1b[{tilde};{modifier}~").into_bytes()
        });
    }
    let mut bytes = match key {
        "enter" if !ctrl => vec![b'\r'],
        "backspace" => vec![if ctrl { 0x08 } else { 0x7f }],
        "tab" if !ctrl && shift => b"\x1b[Z".to_vec(),
        "tab" if !ctrl => vec![b'\t'],
        "esc" if !ctrl => vec![0x1b],
        _ if key.len() == 1 && (ctrl || alt) => {
            let byte = key.as_bytes()[0];
            if !(0x20..=0x7e).contains(&byte) {
                return None;
            }
            vec![if ctrl {
                match byte {
                    b'a'..=b'z' => byte - b'a' + 1,
                    b'A'..=b'Z' => byte - b'A' + 1,
                    b' ' | b'@' | b'2' => 0,
                    b'[' | b'3' => 0x1b,
                    b'\\' | b'4' => 0x1c,
                    b']' | b'5' => 0x1d,
                    b'^' | b'6' => 0x1e,
                    b'_' | b'7' => 0x1f,
                    b'?' | b'8' => 0x7f,
                    _ => return None,
                }
            } else {
                byte
            }]
        }
        _ => return None,
    };
    if alt {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_text_preserves_unicode_spaces_and_only_paste_controls() {
        let input = |text: &str, paste| TerminalInput::Text {
            text: text.into(),
            paste,
        };
        assert_eq!(
            input(" 한글🙂 \x1b\x03\x7f\u{009b}\n\t", false).encode(true, false),
            Some(" 한글🙂 ".as_bytes().to_vec())
        );
        assert_eq!(input("x", false).encode(true, false), Some(b"x".to_vec()));
        assert_eq!(
            input("x", true).encode(true, false),
            Some(b"\x1b[200~x\x1b[201~".to_vec())
        );
        assert_eq!(
            input("a\r\nb\rc\n\t", true).encode(false, false),
            Some(b"a\rb\rc\r\t".to_vec())
        );
        assert!(input("\x1b\x03", false).encode(false, false).is_none());
        assert!(
            input(&"x".repeat(DIRECT_INPUT_TEXT_BYTES_MAX + 1), true)
                .encode(true, false)
                .is_none()
        );
    }

    #[test]
    fn direct_keys_follow_live_cursor_modes_and_modifier_whitelist() {
        assert_eq!(
            encode_key("up", false, false, false, false, false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_key("up", false, false, false, false, true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            encode_key("home", false, false, false, false, true),
            Some(b"\x1bOH".to_vec())
        );
        assert_eq!(
            encode_key("left", true, false, false, false, true),
            Some(b"\x1b[1;5D".to_vec())
        );
        assert_eq!(
            encode_key("delete", false, true, true, false, false),
            Some(b"\x1b[3;4~".to_vec())
        );
        assert_eq!(
            encode_key("tab", false, false, true, false, false),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            encode_key("backspace", false, false, false, false, false),
            Some(vec![0x7f])
        );
        assert_eq!(
            encode_key("c", true, false, false, false, false),
            Some(vec![0x03])
        );
        assert_eq!(
            encode_key("b", false, true, false, false, false),
            Some(b"\x1bb".to_vec())
        );
        assert_eq!(
            encode_key(" ", true, false, false, false, false),
            Some(vec![0])
        );
        for key in ["bad", "한", "\x1b[A", "1"] {
            assert!(encode_key(key, true, false, false, false, false).is_none());
        }
        assert!(encode_key("c", false, false, false, false, false).is_none());
        assert!(encode_key("c", true, false, false, true, false).is_none());
    }
}
