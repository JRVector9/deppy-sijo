//! macOS가 IME Commit에 사용한 printable key-down을 winit에 넘기지 않는 경우를 위한
//! 앱 내부 이벤트 관찰기.
//!
//! AppKit local monitor는 이벤트를 winit의 NSView가 처리하기 전에 호출된다. 여기서는
//! 영문자와 단축키를 제외한 ASCII 문장부호/숫자/공백의 원본 key-down만 짧게 보관하고
//! 이벤트 자체는 수정 없이 그대로 돌려준다. 실제 PTY 전송 여부와 Text/Commit 중복
//! 제거는 터미널 키보드 소유권을 아는 `WorkspaceUi`가 결정한다.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NativePrintableKeyDown {
    pub(crate) character: char,
    observed_at: Instant,
}

impl NativePrintableKeyDown {
    #[cfg(test)]
    pub(crate) fn for_test(character: char) -> Self {
        Self {
            character,
            observed_at: Instant::now(),
        }
    }

    fn fresh(self) -> bool {
        self.observed_at.elapsed() <= NATIVE_KEY_MAX_AGE
    }
}

const NATIVE_KEY_MAX_AGE: Duration = Duration::from_millis(500);
const NATIVE_KEY_QUEUE_CAPACITY: usize = 64;

static KEY_DOWNS: OnceLock<Mutex<VecDeque<NativePrintableKeyDown>>> = OnceLock::new();

fn queue() -> &'static Mutex<VecDeque<NativePrintableKeyDown>> {
    KEY_DOWNS.get_or_init(|| Mutex::new(VecDeque::with_capacity(NATIVE_KEY_QUEUE_CAPACITY)))
}

fn record(character: char) {
    let Ok(mut key_downs) = queue().lock() else {
        return;
    };
    if key_downs.len() == NATIVE_KEY_QUEUE_CAPACITY {
        key_downs.pop_front();
    }
    key_downs.push_back(NativePrintableKeyDown {
        character,
        observed_at: Instant::now(),
    });
}

/// 이번 egui 프레임 직전에 AppKit이 본 printable key-down을 모두 꺼낸다. 오래됐거나
/// 터미널 UI가 비활성인 프레임의 레코드는 다음 입력에 섞이지 않도록 재사용하지 않는다.
pub(crate) fn drain() -> Vec<NativePrintableKeyDown> {
    let Ok(mut key_downs) = queue().lock() else {
        return Vec::new();
    };
    key_downs
        .drain(..)
        .filter(|key_down| key_down.fresh())
        .collect()
}

#[cfg(target_os = "macos")]
pub(crate) fn install() {
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicBool, Ordering};

    use block2::RcBlock;
    use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags};

    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }

    let handler = RcBlock::new(|event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: AppKit guarantees the event pointer is valid for the local monitor callback.
        // We only inspect it synchronously and return the exact same pointer unchanged.
        let event_ref = unsafe { event.as_ref() };
        let modifiers = event_ref.modifierFlags();
        if !modifiers.intersects(
            NSEventModifierFlags::Command
                | NSEventModifierFlags::Control
                | NSEventModifierFlags::Option
                | NSEventModifierFlags::Function,
        ) && let Some(character) = native_printable_character(event_ref)
        {
            record(character);
        }
        event.as_ptr()
    });

    // SAFETY: the block returns either the original live NSEvent pointer (always, here) or could
    // return null to consume it. AppKit copies the block for the monitor lifetime.
    let monitor = unsafe {
        NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &handler)
    };
    if let Some(monitor) = monitor {
        // The monitor intentionally lasts for the process lifetime. AppKit owns its copied block;
        // retaining this token ensures it cannot disappear before shutdown.
        std::mem::forget(monitor);
    } else {
        INSTALLED.store(false, Ordering::Release);
        tracing::warn!("macOS native key monitor 설치 실패 — IME key-up 복구만 사용");
    }
}

#[cfg(target_os = "macos")]
fn native_printable_character(event: &objc2_app_kit::NSEvent) -> Option<char> {
    // `characters` is layout-aware and already includes Shift, so prefer it over a US key-code
    // table. Korean 2-set input still reports punctuation/digits/space here before IME consumes it.
    let character = event
        .characters()
        .and_then(|characters| single_ascii_terminator(&characters.to_string()))
        .or_else(|| {
            key_code_ascii(
                event.keyCode(),
                event
                    .modifierFlags()
                    .contains(objc2_app_kit::NSEventModifierFlags::Shift),
            )
        })?;
    is_ascii_terminator(character).then_some(character)
}

fn single_ascii_terminator(text: &str) -> Option<char> {
    let mut characters = text.chars();
    let character = characters.next()?;
    (characters.next().is_none() && is_ascii_terminator(character)).then_some(character)
}

fn is_ascii_terminator(character: char) -> bool {
    character == ' ' || character.is_ascii_digit() || character.is_ascii_punctuation()
}

/// `characters`가 비어 있는 특수 키 이벤트에만 쓰는 ANSI/Korean 2-set 물리 키 fallback.
/// 보통 경로는 위의 layout-aware 문자열이므로 다른 키보드 배열을 덮어쓰지 않는다.
#[cfg(target_os = "macos")]
fn key_code_ascii(key_code: u16, shifted: bool) -> Option<char> {
    let pair = match key_code {
        0x12 => ('1', '!'),
        0x13 => ('2', '@'),
        0x14 => ('3', '#'),
        0x15 => ('4', '$'),
        0x17 => ('5', '%'),
        0x16 => ('6', '^'),
        0x1a => ('7', '&'),
        0x1c => ('8', '*'),
        0x19 => ('9', '('),
        0x1d => ('0', ')'),
        0x18 => ('=', '+'),
        0x1b => ('-', '_'),
        0x1e => (']', '}'),
        0x21 => ('[', '{'),
        0x27 => ('\'', '"'),
        0x29 => (';', ':'),
        0x2a => ('\\', '|'),
        0x2b => (',', '<'),
        0x2c => ('/', '?'),
        0x2f => ('.', '>'),
        0x32 => ('`', '~'),
        0x31 => (' ', ' '),
        _ => return None,
    };
    Some(if shifted { pair.1 } else { pair.0 })
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn install() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_문장부호_숫자_공백만_복구후보다() {
        for character in ['.', '?', '!', '1', ' ', '~'] {
            assert!(is_ascii_terminator(character));
        }
        for character in ['a', 'Z', 'ㅁ', '\n'] {
            assert!(!is_ascii_terminator(character));
        }
    }

    #[test]
    fn 네이티브문자열은_정확히_한_글자일때만_쓴다() {
        assert_eq!(single_ascii_terminator("."), Some('.'));
        assert_eq!(single_ascii_terminator("!"), Some('!'));
        assert_eq!(single_ascii_terminator(""), None);
        assert_eq!(single_ascii_terminator(".."), None);
        assert_eq!(single_ascii_terminator("a"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ansi_물리키_fallback은_shift기호를_보존한다() {
        assert_eq!(key_code_ascii(0x2f, false), Some('.'));
        assert_eq!(key_code_ascii(0x2f, true), Some('>'));
        assert_eq!(key_code_ascii(0x12, true), Some('!'));
        assert_eq!(key_code_ascii(0x31, false), Some(' '));
    }
}
