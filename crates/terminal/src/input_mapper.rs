//! egui 입력 이벤트 → PTY 입력 바이트 매핑 (설계문서 9장 input_mapper).
//!
//! 백로그 (PR-21까지): APP_CURSOR/APP_KEYPAD 모드, mouse reporting, kitty protocol.
//! 현재 방향키는 CSI 고정이다.

/// 하나의 egui 이벤트를 PTY 입력 바이트로 바꾼다. 무관한 이벤트는 None.
/// IME Preedit은 입력이 아니라 표시 상태 — 호출측(UI)이 별도로 다룬다.
/// `modifiers`는 이벤트 시점의 modifier 상태 — 클립보드 이벤트 구분에 쓴다.
pub fn map_event(
    event: &egui::Event,
    bracketed_paste: bool,
    modifiers: &egui::Modifiers,
) -> Option<Vec<u8>> {
    match event {
        // 일반 타이핑 (IME 미사용 시). 제어키 조합은 Text로 오지 않는다.
        egui::Event::Text(text) => Some(text.as_bytes().to_vec()),
        egui::Event::Ime(egui::ImeEvent::Commit(text)) => Some(text.as_bytes().to_vec()),
        // egui-winit은 클립보드 단축키를 고수준 이벤트로 바꾼다.
        // 터미널 관례 (Windows/Linux): Ctrl+C/X/V는 제어 바이트,
        // 붙여넣기는 Ctrl+Shift+V. macOS는 Cmd 계열이 클립보드 (Ctrl은 Key 경로).
        // selection copy 도입(백로그, PR-21) 시 선택 영역 있으면 copy 우선으로 재검토.
        egui::Event::Paste(text) => {
            if cfg!(not(target_os = "macos")) && modifiers.ctrl && !modifiers.shift {
                Some(vec![0x16]) // Ctrl+V — readline quoted-insert 등
            } else {
                Some(paste_bytes(text.as_bytes(), bracketed_paste))
            }
        }
        // shift 조합(Ctrl+Shift+C/X)은 클립보드 의도 — selection 미지원이라 무시
        #[cfg(not(target_os = "macos"))]
        egui::Event::Copy if !modifiers.shift => Some(vec![0x03]),
        #[cfg(not(target_os = "macos"))]
        egui::Event::Cut if !modifiers.shift => Some(vec![0x18]),
        egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => map_key(*key, modifiers),
        _ => None,
    }
}

/// macOS IME 조합을 끝내는 printable 키의 마지막 안전망이다.
///
/// 일반적인 키 입력은 [`egui::Event::Text`]도 함께 오므로 [`map_event`]가 Text만 보내
/// 중복을 피한다. 하지만 한글 조합 직후의 문장부호는 macOS가 `Key`와 `Ime::Commit`만
/// 전달하고 Text를 생략할 수 있다. 이 함수는 그 경우에만 UI가 대체 바이트를 보낼 수
/// 있도록, 현재 키보드 배열에서 확정적인 ASCII 문자만 돌려준다. Ctrl/Command/Option
/// 조합은 단축키 또는 레이아웃 의존 문자일 수 있으므로 절대 대체하지 않는다.
pub fn ime_terminator_key_char(event: &egui::Event) -> Option<char> {
    ime_key_char_with_state(event, true)
}

/// macOS/winit은 IME가 소비한 printable key의 key-down은 숨기지만, 조합이 끝난 뒤
/// key-up은 전달한다. UI가 그 key-up에서 실제 문자를 복구할 때 쓴다.
pub fn ime_terminator_key_release_char(event: &egui::Event) -> Option<char> {
    ime_key_char_with_state(event, false)
}

fn ime_key_char_with_state(event: &egui::Event, expected_pressed: bool) -> Option<char> {
    let egui::Event::Key {
        key,
        pressed,
        modifiers,
        ..
    } = event
    else {
        return None;
    };
    if *pressed != expected_pressed {
        return None;
    }
    if modifiers.ctrl || modifiers.command || modifiers.mac_cmd || modifiers.alt {
        return None;
    }

    let shifted = modifiers.shift;
    Some(match key {
        egui::Key::Space => ' ',
        egui::Key::Comma => {
            if shifted {
                '<'
            } else {
                ','
            }
        }
        egui::Key::Minus => {
            if shifted {
                '_'
            } else {
                '-'
            }
        }
        egui::Key::Period => {
            if shifted {
                '>'
            } else {
                '.'
            }
        }
        egui::Key::Slash => {
            if shifted {
                '?'
            } else {
                '/'
            }
        }
        egui::Key::Semicolon => {
            if shifted {
                ':'
            } else {
                ';'
            }
        }
        egui::Key::Backslash | egui::Key::IntlBackslash => {
            if shifted {
                '|'
            } else {
                '\\'
            }
        }
        egui::Key::OpenBracket => {
            if shifted {
                '{'
            } else {
                '['
            }
        }
        egui::Key::CloseBracket => {
            if shifted {
                '}'
            } else {
                ']'
            }
        }
        egui::Key::Backtick => {
            if shifted {
                '~'
            } else {
                '`'
            }
        }
        egui::Key::Quote => {
            if shifted {
                '"'
            } else {
                '\''
            }
        }
        egui::Key::Equals => {
            if shifted {
                '+'
            } else {
                '='
            }
        }
        egui::Key::Colon => ':',
        egui::Key::Pipe => '|',
        egui::Key::Questionmark => '?',
        egui::Key::Exclamationmark => '!',
        egui::Key::OpenCurlyBracket => '{',
        egui::Key::CloseCurlyBracket => '}',
        egui::Key::Plus => '+',
        egui::Key::Num0 => {
            if shifted {
                ')'
            } else {
                '0'
            }
        }
        egui::Key::Num1 => {
            if shifted {
                '!'
            } else {
                '1'
            }
        }
        egui::Key::Num2 => {
            if shifted {
                '@'
            } else {
                '2'
            }
        }
        egui::Key::Num3 => {
            if shifted {
                '#'
            } else {
                '3'
            }
        }
        egui::Key::Num4 => {
            if shifted {
                '$'
            } else {
                '4'
            }
        }
        egui::Key::Num5 => {
            if shifted {
                '%'
            } else {
                '5'
            }
        }
        egui::Key::Num6 => {
            if shifted {
                '^'
            } else {
                '6'
            }
        }
        egui::Key::Num7 => {
            if shifted {
                '&'
            } else {
                '7'
            }
        }
        egui::Key::Num8 => {
            if shifted {
                '*'
            } else {
                '8'
            }
        }
        egui::Key::Num9 => {
            if shifted {
                '('
            } else {
                '9'
            }
        }
        _ => return None,
    })
}

/// bracketed paste 모드(DEC 2004)면 ESC[200~ / ESC[201~로 감싼 paste bytes를 만든다.
pub fn paste_bytes(payload: &[u8], bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut out = b"\x1b[200~".to_vec();
        out.extend_from_slice(payload);
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        payload.to_vec()
    }
}

fn map_key(key: egui::Key, modifiers: &egui::Modifiers) -> Option<Vec<u8>> {
    // Ctrl+문자 → C0 제어 코드 (Ctrl+C = 0x03)
    if modifiers.ctrl
        && let Some(byte) = ctrl_byte(key)
    {
        return Some(vec![byte]);
    }
    if key == egui::Key::Tab && modifiers.shift {
        return Some(b"\x1b[Z".to_vec());
    }
    if let Some(bytes) = arrow_key_bytes(key, modifiers) {
        return Some(bytes);
    }
    if let Some(bytes) = function_key_bytes(key) {
        return Some(bytes.to_vec());
    }
    let bytes: &[u8] = match key {
        egui::Key::Enter => b"\r",
        egui::Key::Tab => b"\t",
        egui::Key::Backspace => b"\x7f",
        egui::Key::Escape => b"\x1b",
        egui::Key::Home => b"\x1b[H",
        egui::Key::End => b"\x1b[F",
        egui::Key::PageUp => b"\x1b[5~",
        egui::Key::PageDown => b"\x1b[6~",
        egui::Key::Delete => b"\x1b[3~",
        egui::Key::Insert => b"\x1b[2~",
        _ => return None,
    };
    Some(bytes.to_vec())
}

fn arrow_key_bytes(key: egui::Key, modifiers: &egui::Modifiers) -> Option<Vec<u8>> {
    let final_byte = match key {
        egui::Key::ArrowUp => b'A',
        egui::Key::ArrowDown => b'B',
        egui::Key::ArrowRight => b'C',
        egui::Key::ArrowLeft => b'D',
        _ => return None,
    };
    let Some(modifier) = csi_modifier(modifiers) else {
        return Some(vec![b'\x1b', b'[', final_byte]);
    };
    Some(format!("\x1b[1;{modifier}{}", final_byte as char).into_bytes())
}

fn csi_modifier(modifiers: &egui::Modifiers) -> Option<u8> {
    let mut value = 1;
    if modifiers.shift {
        value += 1;
    }
    if modifiers.alt {
        value += 2;
    }
    if modifiers.ctrl {
        value += 4;
    }
    (value != 1).then_some(value)
}

fn function_key_bytes(key: egui::Key) -> Option<&'static [u8]> {
    match key {
        egui::Key::F1 => Some(b"\x1bOP"),
        egui::Key::F2 => Some(b"\x1bOQ"),
        egui::Key::F3 => Some(b"\x1bOR"),
        egui::Key::F4 => Some(b"\x1bOS"),
        egui::Key::F5 => Some(b"\x1b[15~"),
        egui::Key::F6 => Some(b"\x1b[17~"),
        egui::Key::F7 => Some(b"\x1b[18~"),
        egui::Key::F8 => Some(b"\x1b[19~"),
        egui::Key::F9 => Some(b"\x1b[20~"),
        egui::Key::F10 => Some(b"\x1b[21~"),
        egui::Key::F11 => Some(b"\x1b[23~"),
        egui::Key::F12 => Some(b"\x1b[24~"),
        _ => None,
    }
}

fn ctrl_byte(key: egui::Key) -> Option<u8> {
    let name = key.name();
    // A~Z 단일 문자 키만 (Ctrl+A=0x01 ... Ctrl+Z=0x1a)
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c @ 'A'..='Z'), None) => Some(c as u8 & 0x1f),
        _ => match key {
            egui::Key::OpenBracket => Some(0x1b),  // Ctrl+[
            egui::Key::Backslash => Some(0x1c),    // Ctrl+\
            egui::Key::CloseBracket => Some(0x1d), // Ctrl+]
            egui::Key::Space => Some(0x00),        // Ctrl+Space
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: egui::Modifiers = egui::Modifiers::NONE;

    fn key_event(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn 특수키_매핑() {
        assert_eq!(
            map_event(&key_event(egui::Key::Enter, NONE), false, &NONE),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            map_event(&key_event(egui::Key::Backspace, NONE), false, &NONE),
            Some(b"\x7f".to_vec())
        );
        assert_eq!(
            map_event(&key_event(egui::Key::ArrowUp, NONE), false, &NONE),
            Some(b"\x1b[A".to_vec())
        );
    }

    #[test]
    fn tui_number_selection은_text_event로_전달하고_key_event는_중복하지_않는다() {
        assert_eq!(
            map_event(&egui::Event::Text("1".into()), false, &NONE),
            Some(b"1".to_vec())
        );
        assert_eq!(
            map_event(&egui::Event::Text("95".into()), false, &NONE),
            Some(b"95".to_vec())
        );
        assert_eq!(
            map_event(&key_event(egui::Key::Num1, NONE), false, &NONE),
            None
        );
    }

    #[test]
    fn tui_navigation_확장키_매핑() {
        let shift = egui::Modifiers::SHIFT;
        let ctrl = egui::Modifiers::CTRL;
        let alt = egui::Modifiers::ALT;
        let ctrl_shift = egui::Modifiers::CTRL | egui::Modifiers::SHIFT;

        assert_eq!(
            map_event(&key_event(egui::Key::Tab, shift), false, &shift),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            map_event(&key_event(egui::Key::ArrowUp, shift), false, &shift),
            Some(b"\x1b[1;2A".to_vec())
        );
        assert_eq!(
            map_event(&key_event(egui::Key::ArrowRight, alt), false, &alt),
            Some(b"\x1b[1;3C".to_vec())
        );
        assert_eq!(
            map_event(
                &key_event(egui::Key::ArrowDown, ctrl_shift),
                false,
                &ctrl_shift
            ),
            Some(b"\x1b[1;6B".to_vec())
        );
        assert_eq!(
            map_event(&key_event(egui::Key::ArrowLeft, ctrl), false, &ctrl),
            Some(b"\x1b[1;5D".to_vec())
        );
    }

    #[test]
    fn function_keys_f1_to_f12_mapping() {
        assert_eq!(
            map_event(&key_event(egui::Key::F1, NONE), false, &NONE),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            map_event(&key_event(egui::Key::F4, NONE), false, &NONE),
            Some(b"\x1bOS".to_vec())
        );
        assert_eq!(
            map_event(&key_event(egui::Key::F5, NONE), false, &NONE),
            Some(b"\x1b[15~".to_vec())
        );
        assert_eq!(
            map_event(&key_event(egui::Key::F12, NONE), false, &NONE),
            Some(b"\x1b[24~".to_vec())
        );
    }

    #[test]
    fn ctrl_c는_0x03() {
        let ctrl = egui::Modifiers::CTRL;
        assert_eq!(
            map_event(&key_event(egui::Key::C, ctrl), false, &ctrl),
            Some(vec![0x03])
        );
        assert_eq!(
            map_event(&key_event(egui::Key::D, ctrl), false, &ctrl),
            Some(vec![0x04])
        );
    }

    #[test]
    fn 텍스트와_ime_commit() {
        assert_eq!(
            map_event(&egui::Event::Text("한a".into()), false, &NONE),
            Some("한a".as_bytes().to_vec())
        );
        assert_eq!(
            map_event(
                &egui::Event::Ime(egui::ImeEvent::Commit("글".into())),
                false,
                &NONE
            ),
            Some("글".as_bytes().to_vec())
        );
    }

    #[test]
    fn 문장부호와_공백은_key와_text_쌍에서도_한번만_전달된다() {
        // egui-winit은 printable key에 대해 Event::Key와 Event::Text를 모두 보낸다.
        // 터미널에는 Text만 전달해야 `.`, 공백, `!` 등이 두 번 입력되지 않는다.
        let cases = [
            (egui::Key::Period, "."),
            (egui::Key::Comma, ","),
            (egui::Key::Slash, "/"),
            (egui::Key::Questionmark, "?"),
            (egui::Key::Exclamationmark, "!"),
            (egui::Key::Space, " "),
        ];
        for (key, text) in cases {
            let events = [key_event(key, NONE), egui::Event::Text(text.to_owned())];
            let mut bytes = Vec::new();
            for event in &events {
                if let Some(mapped) = map_event(event, false, &NONE) {
                    bytes.extend(mapped);
                }
            }
            assert_eq!(bytes, text.as_bytes(), "{text:?} must be sent once");
        }
    }

    #[test]
    fn ime_조합종료_대체키는_특수문자와_shift_기호를_되살린다() {
        assert_eq!(
            ime_terminator_key_char(&key_event(egui::Key::Period, NONE)),
            Some('.')
        );
        assert_eq!(
            ime_terminator_key_char(&key_event(egui::Key::Period, egui::Modifiers::SHIFT)),
            Some('>')
        );
        assert_eq!(
            ime_terminator_key_char(&key_event(egui::Key::Num1, egui::Modifiers::SHIFT)),
            Some('!')
        );
        assert_eq!(
            ime_terminator_key_char(&key_event(egui::Key::Quote, NONE)),
            Some('\'')
        );
        assert_eq!(
            ime_terminator_key_char(&key_event(egui::Key::Backtick, egui::Modifiers::SHIFT)),
            Some('~')
        );
    }

    #[test]
    fn ime_조합종료_대체키는_단축키를_가로채지_않는다() {
        assert_eq!(
            ime_terminator_key_char(&key_event(egui::Key::Period, egui::Modifiers::CTRL)),
            None
        );
        assert_eq!(
            ime_terminator_key_char(&key_event(egui::Key::Period, egui::Modifiers::MAC_CMD)),
            None
        );
    }

    #[test]
    fn ime가_keydown을_삼킨_특수문자는_keyup에서_복구할_수_있다() {
        let release = |key, modifiers| egui::Event::Key {
            key,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers,
        };
        assert_eq!(
            ime_terminator_key_release_char(&release(egui::Key::Period, NONE)),
            Some('.')
        );
        assert_eq!(
            ime_terminator_key_release_char(&release(egui::Key::Num1, egui::Modifiers::SHIFT)),
            Some('!')
        );
        assert_eq!(
            ime_terminator_key_release_char(&release(egui::Key::Period, egui::Modifiers::CTRL)),
            None
        );
    }

    #[test]
    fn bracketed_paste_감싸기() {
        // macOS Cmd+V / Win·Linux Ctrl+Shift+V에 해당하는 modifier 상태
        let paste_mods = if cfg!(target_os = "macos") {
            egui::Modifiers::MAC_CMD
        } else {
            egui::Modifiers::CTRL | egui::Modifiers::SHIFT
        };
        assert_eq!(
            map_event(&egui::Event::Paste("hi".into()), true, &paste_mods),
            Some(b"\x1b[200~hi\x1b[201~".to_vec())
        );
        assert_eq!(
            map_event(&egui::Event::Paste("hi".into()), false, &paste_mods),
            Some(b"hi".to_vec())
        );
    }

    #[test]
    fn paste_bytes_required_fixtures는_bracketed_상태를_따른다() {
        let fixtures = [
            "src/main.rs",
            "プロジェクト/設定ファイル.rs",
            "项目/配置文件.rs",
            "專案/設定檔.rs",
            "프로젝트/설정파일.rs",
            "project/🚀-deploy/config.json",
        ];

        for fixture in fixtures {
            assert_eq!(
                paste_bytes(fixture.as_bytes(), false),
                fixture.as_bytes().to_vec()
            );

            let mut expected = b"\x1b[200~".to_vec();
            expected.extend_from_slice(fixture.as_bytes());
            expected.extend_from_slice(b"\x1b[201~");
            assert_eq!(paste_bytes(fixture.as_bytes(), true), expected, "{fixture}");
        }
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn ctrl_v_단독은_quoted_insert() {
        // Windows/Linux: Ctrl+V(shift 없음) → 0x16, 붙여넣기는 Ctrl+Shift+V
        let ctrl = egui::Modifiers::CTRL;
        assert_eq!(
            map_event(&egui::Event::Paste("클립보드".into()), true, &ctrl),
            Some(vec![0x16])
        );
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn copy_cut_이벤트는_제어바이트() {
        // Windows/Linux: Ctrl+C → Copy, Ctrl+X → Cut 경로
        let ctrl = egui::Modifiers::CTRL;
        assert_eq!(
            map_event(&egui::Event::Copy, false, &ctrl),
            Some(vec![0x03])
        );
        assert_eq!(map_event(&egui::Event::Cut, false, &ctrl), Some(vec![0x18]));
        // Ctrl+Shift+C는 클립보드 복사 의도 — SIGINT 금지
        let ctrl_shift = egui::Modifiers::CTRL | egui::Modifiers::SHIFT;
        assert_eq!(map_event(&egui::Event::Copy, false, &ctrl_shift), None);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_copy는_무시() {
        // Cmd+C는 복사 의도 — SIGINT로 보내면 안 된다
        assert_eq!(
            map_event(&egui::Event::Copy, false, &egui::Modifiers::MAC_CMD),
            None
        );
    }

    #[test]
    fn 키_뗌은_무시() {
        let event = egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: NONE,
        };
        assert_eq!(map_event(&event, false, &NONE), None);
    }
}
