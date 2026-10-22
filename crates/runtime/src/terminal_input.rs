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
    /// Append-only navigation; no text or key payload and no implicit submit.
    Wheel {
        cols: u16,
        rows: u16,
        column: u16,
        row: u16,
        delta: i16,
    },
}

pub const DIRECT_INPUT_TEXT_BYTES_MAX: usize = 256 * 1024;

impl TerminalInput {
    pub(crate) fn requires_current_modes(&self) -> bool {
        match self {
            Self::Wheel { .. } => true,
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

    pub(crate) fn payload(&self) -> Option<&String> {
        match self {
            Self::Text { text, .. } => Some(text),
            Self::Key { key, .. } => Some(key),
            Self::Wheel { .. } => None,
        }
    }

    pub(crate) fn payload_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::Text { text, .. } => Some(text),
            Self::Key { key, .. } => Some(key),
            Self::Wheel { .. } => None,
        }
    }

    /// Validates the same bounded command contract used by runtime retention.
    pub fn is_valid(&self) -> bool {
        match self {
            Self::Text { text, .. } => text.len() <= DIRECT_INPUT_TEXT_BYTES_MAX,
            Self::Wheel {
                cols,
                rows,
                column,
                row,
                delta,
            } => {
                crate::command::dimensions_are_valid(*cols, *rows)
                    && (1..=*cols).contains(column)
                    && (1..=*rows).contains(row)
                    && (1..=32).contains(&delta.unsigned_abs())
            }
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
            // The backend must inspect live negotiated mouse modes and grid dimensions.
            Self::Wheel { .. } => None,
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
    fn application_wheel_validates_bounded_grid_coordinates_without_text_payload() {
        let make = |cols, rows, column, row, delta| TerminalInput::Wheel {
            cols,
            rows,
            column,
            row,
            delta,
        };
        for input in [make(80, 24, 1, 1, 1), make(80, 24, 80, 24, -32)] {
            assert!(input.is_valid());
            assert!(input.requires_current_modes());
            assert!(input.payload().is_none());
            assert!(
                input.encode(true, true).is_none(),
                "worker must use live backend mouse negotiation"
            );
        }
        for input in [
            make(0, 24, 1, 1, 1),
            make(80, 0, 1, 1, 1),
            make(501, 24, 1, 1, 1),
            make(500, 500, 1, 1, 1),
            make(80, 24, 0, 1, 1),
            make(80, 24, 81, 1, 1),
            make(80, 24, 1, 0, 1),
            make(80, 24, 1, 25, 1),
            make(80, 24, 1, 1, 0),
            make(80, 24, 1, 1, 33),
            make(80, 24, 1, 1, -33),
            make(80, 24, 1, 1, i16::MIN),
        ] {
            assert!(!input.is_valid());
        }
    }

    #[test]
    fn application_wheel_is_append_only_and_preserves_legacy_text_key_wire() {
        #[derive(serde::Serialize, serde::Deserialize)]
        enum LegacyInput {
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
        for (legacy, current) in [
            (
                LegacyInput::Text {
                    text: "한글 초안".into(),
                    paste: true,
                },
                TerminalInput::Text {
                    text: "한글 초안".into(),
                    paste: true,
                },
            ),
            (
                LegacyInput::Key {
                    key: "up".into(),
                    ctrl: false,
                    alt: true,
                    shift: false,
                    meta: false,
                },
                TerminalInput::Key {
                    key: "up".into(),
                    ctrl: false,
                    alt: true,
                    shift: false,
                    meta: false,
                },
            ),
        ] {
            let bytes = postcard::to_allocvec(&legacy).unwrap();
            assert_eq!(bytes, postcard::to_allocvec(&current).unwrap());
            assert!(postcard::from_bytes::<TerminalInput>(&bytes).unwrap() == current);
        }
        let wheel = TerminalInput::Wheel {
            cols: 80,
            rows: 24,
            column: 7,
            row: 9,
            delta: -2,
        };
        let bytes = postcard::to_allocvec(&wheel).unwrap();
        assert_eq!(bytes[0], 2);
        assert!(postcard::from_bytes::<LegacyInput>(&bytes).is_err());
        assert!(postcard::from_bytes::<TerminalInput>(&bytes).unwrap() == wheel);
    }

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
