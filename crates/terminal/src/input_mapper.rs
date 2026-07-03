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
                Some(wrap_paste(text, bracketed_paste))
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

/// bracketed paste 모드(DEC 2004)면 ESC[200~ / ESC[201~로 감싼다.
fn wrap_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut out = b"\x1b[200~".to_vec();
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        text.as_bytes().to_vec()
    }
}

fn map_key(key: egui::Key, modifiers: &egui::Modifiers) -> Option<Vec<u8>> {
    // Ctrl+문자 → C0 제어 코드 (Ctrl+C = 0x03)
    if modifiers.ctrl
        && let Some(byte) = ctrl_byte(key)
    {
        return Some(vec![byte]);
    }
    let bytes: &[u8] = match key {
        egui::Key::Enter => b"\r",
        egui::Key::Tab => b"\t",
        egui::Key::Backspace => b"\x7f",
        egui::Key::Escape => b"\x1b",
        egui::Key::ArrowUp => b"\x1b[A",
        egui::Key::ArrowDown => b"\x1b[B",
        egui::Key::ArrowRight => b"\x1b[C",
        egui::Key::ArrowLeft => b"\x1b[D",
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
