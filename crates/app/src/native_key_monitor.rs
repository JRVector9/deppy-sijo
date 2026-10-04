//! macOS가 IME Commit에 사용한 printable key-down을 winit에 넘기지 않는 경우를 위한
//! 앱 내부 이벤트 관찰기.
//!
//! AppKit local monitor는 이벤트를 winit의 NSView가 처리하기 전에 호출된다. 여기서는
//! 영문자와 일반 단축키를 제외한 ASCII 문장부호/숫자/공백, 그리고 egui가 이미지-only
//! 클립보드에서 소비해 버리는 Command+C/V의 원본 key-down만 짧게 보관하고 이벤트 자체는
//! 수정 없이 그대로 돌려준다. 실제 PTY 전송 여부와 Text/Commit/clipboard 중복 제거는
//! 터미널 키보드 소유권을 아는 `WorkspaceUi`가 결정한다.

use std::collections::VecDeque;
#[cfg(not(test))]
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NativePrintableKeyDown {
    pub(crate) character: char,
    /// This physical key was observed after Return in the same drained batch.
    pub(crate) after_submit: bool,
    observed_at: Instant,
}

#[derive(Default)]
pub(crate) struct NativeKeyDownBatch {
    pub(crate) printable: Vec<NativePrintableKeyDown>,
    pub(crate) clipboard_paste: bool,
    pub(crate) clipboard_copy: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeKeyDown {
    Printable(NativePrintableKeyDown),
    Submit { observed_at: Instant },
    ClipboardPaste { observed_at: Instant },
    ClipboardCopy { observed_at: Instant },
}

impl NativeKeyDown {
    fn fresh(self) -> bool {
        match self {
            Self::Printable(key_down) => key_down.fresh(),
            Self::Submit { observed_at }
            | Self::ClipboardPaste { observed_at }
            | Self::ClipboardCopy { observed_at } => observed_at.elapsed() <= NATIVE_KEY_MAX_AGE,
        }
    }
}

impl NativePrintableKeyDown {
    #[cfg(test)]
    pub(crate) fn for_test(character: char) -> Self {
        Self {
            character,
            after_submit: false,
            observed_at: Instant::now(),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_after_submit(character: char) -> Self {
        Self {
            character,
            after_submit: true,
            observed_at: Instant::now(),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_observed_at(character: char, observed_at: Instant) -> Self {
        Self {
            character,
            after_submit: false,
            observed_at,
        }
    }

    pub(crate) fn observed_before(self, instant: Instant) -> bool {
        self.observed_at <= instant
    }

    fn fresh(self) -> bool {
        self.observed_at.elapsed() <= NATIVE_KEY_MAX_AGE
    }
}

const NATIVE_KEY_MAX_AGE: Duration = Duration::from_millis(500);
const NATIVE_KEY_QUEUE_CAPACITY: usize = 64;

#[cfg(not(test))]
static KEY_DOWNS: OnceLock<Mutex<VecDeque<NativeKeyDown>>> = OnceLock::new();

// Each offscreen harness owns its injected native batch. The app retains the
// process-wide monitor queue; parallel Rust tests must not drain each other's input.
#[cfg(test)]
std::thread_local! {
    static TEST_KEY_DOWNS: std::cell::RefCell<VecDeque<NativeKeyDown>> =
        std::cell::RefCell::new(VecDeque::with_capacity(NATIVE_KEY_QUEUE_CAPACITY));
}

fn with_key_downs<R>(run: impl FnOnce(&mut VecDeque<NativeKeyDown>) -> R) -> Option<R> {
    #[cfg(test)]
    {
        Some(TEST_KEY_DOWNS.with(|queue| run(&mut queue.borrow_mut())))
    }
    #[cfg(not(test))]
    {
        let queue = KEY_DOWNS
            .get_or_init(|| Mutex::new(VecDeque::with_capacity(NATIVE_KEY_QUEUE_CAPACITY)));
        let Ok(mut queue) = queue.lock() else {
            return None;
        };
        Some(run(&mut queue))
    }
}

fn record(key_down: NativeKeyDown) {
    let _ = with_key_downs(|key_downs| {
        if key_downs.len() == NATIVE_KEY_QUEUE_CAPACITY {
            key_downs.pop_front();
        }
        key_downs.push_back(key_down);
    });
}

fn record_printable(character: char) {
    record(NativeKeyDown::Printable(NativePrintableKeyDown {
        character,
        after_submit: false,
        observed_at: Instant::now(),
    }));
}

fn record_submit() {
    record(NativeKeyDown::Submit {
        observed_at: Instant::now(),
    });
}

fn record_clipboard_paste() {
    record(NativeKeyDown::ClipboardPaste {
        observed_at: Instant::now(),
    });
}

fn record_clipboard_copy() {
    record(NativeKeyDown::ClipboardCopy {
        observed_at: Instant::now(),
    });
}

/// 이번 egui 프레임 직전에 AppKit이 본 printable/clipboard key-down을 모두 꺼낸다.
/// 오래됐거나 터미널 UI가 비활성인 프레임의 레코드는 다음 입력에 섞이지 않도록
/// 재사용하지 않는다. 같은 프레임의 Command+V key repeat은 paste 1회로 합친다.
pub(crate) fn drain() -> NativeKeyDownBatch {
    with_key_downs(|key_downs| collect_batch(key_downs.drain(..))).unwrap_or_default()
}

fn collect_batch(key_downs: impl IntoIterator<Item = NativeKeyDown>) -> NativeKeyDownBatch {
    let mut batch = NativeKeyDownBatch::default();
    let mut seen_submit = false;
    for key_down in key_downs.into_iter().filter(|key_down| key_down.fresh()) {
        match key_down {
            NativeKeyDown::Printable(mut key_down) => {
                key_down.after_submit = seen_submit;
                batch.printable.push(key_down);
            }
            NativeKeyDown::Submit { .. } => seen_submit = true,
            NativeKeyDown::ClipboardPaste { .. } => batch.clipboard_paste = true,
            NativeKeyDown::ClipboardCopy { .. } => batch.clipboard_copy = true,
        }
    }
    batch
}

/// drain하지 않고 fresh한 Command+V key-down이 있는지만 본다. 파일 트리 ⌘V 게이트가
/// 터미널의 drain(prepare_frame)보다 **같은 프레임 먼저** 신호를 봐야 붙여넣기 소유권을
/// 정하고 터미널 이중 처리를 누를 수 있다 — 실제 키보드 소유자가 batch를 소비한다.
pub(crate) fn peek_clipboard_paste() -> bool {
    with_key_downs(|key_downs| {
        key_downs
            .iter()
            .any(|kd| matches!(kd, NativeKeyDown::ClipboardPaste { .. }) && kd.fresh())
    })
    .unwrap_or(false)
}

/// drain하지 않고 fresh한 Command+C key-down이 있는지만 본다. 파일 트리가 터미널보다
/// 먼저 복사 소유권을 정할 수 있게 하며, 실제 소비는 WorkspaceUi의 drain에 맡긴다.
pub(crate) fn peek_clipboard_copy() -> bool {
    with_key_downs(|key_downs| {
        key_downs
            .iter()
            .any(|kd| matches!(kd, NativeKeyDown::ClipboardCopy { .. }) && kd.fresh())
    })
    .unwrap_or(false)
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
        if native_clipboard_paste_event(event_ref) {
            record_clipboard_paste();
        } else if native_clipboard_copy_event(event_ref) {
            record_clipboard_copy();
        } else if matches!(event_ref.keyCode(), 0x24 | 0x4c)
            && !modifiers.intersects(
                NSEventModifierFlags::Command
                    | NSEventModifierFlags::Control
                    | NSEventModifierFlags::Option
                    | NSEventModifierFlags::Function,
            )
        {
            record_submit();
        } else if !modifiers.intersects(
            NSEventModifierFlags::Command
                | NSEventModifierFlags::Control
                | NSEventModifierFlags::Option
                | NSEventModifierFlags::Function,
        ) && let Some(character) = native_printable_character(event_ref)
        {
            record_printable(character);
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
fn native_clipboard_copy_event(event: &objc2_app_kit::NSEvent) -> bool {
    use objc2_app_kit::NSEventModifierFlags;

    let modifiers = event.modifierFlags();
    let characters = event
        .charactersIgnoringModifiers()
        .map(|characters| characters.to_string());
    is_clipboard_copy_key(
        event.keyCode(),
        characters.as_deref(),
        modifiers.contains(NSEventModifierFlags::Command),
        modifiers.intersects(
            NSEventModifierFlags::Control
                | NSEventModifierFlags::Option
                | NSEventModifierFlags::Function,
        ),
    )
}

#[cfg(target_os = "macos")]
fn native_clipboard_paste_event(event: &objc2_app_kit::NSEvent) -> bool {
    use objc2_app_kit::NSEventModifierFlags;

    let modifiers = event.modifierFlags();
    let characters = event
        .charactersIgnoringModifiers()
        .map(|characters| characters.to_string());
    is_clipboard_paste_key(
        event.keyCode(),
        characters.as_deref(),
        modifiers.contains(NSEventModifierFlags::Command),
        modifiers.intersects(
            NSEventModifierFlags::Control
                | NSEventModifierFlags::Option
                | NSEventModifierFlags::Function,
        ),
    )
}

/// `charactersIgnoringModifiers`가 Latin `v`를 주는 배열은 논리 키를 따르고, 한글처럼
/// 비-Latin 문자열을 주거나 비어 있으면 ANSI V 물리 키(0x09)를 fallback으로 쓴다.
/// Control/Option/Function 조합은 terminal/app shortcut일 수 있어 clipboard paste로
/// 해석하지 않는다. Shift는 macOS의 Paste and Match Style 계열과 기존 egui 동작을
/// 보존하기 위해 허용한다.
fn is_clipboard_paste_key(
    key_code: u16,
    characters_ignoring_modifiers: Option<&str>,
    command: bool,
    conflicting_modifier: bool,
) -> bool {
    if !command || conflicting_modifier {
        return false;
    }
    characters_ignoring_modifiers.is_some_and(|characters| characters.eq_ignore_ascii_case("v"))
        || key_code == 0x09
}

fn is_clipboard_copy_key(
    key_code: u16,
    characters_ignoring_modifiers: Option<&str>,
    command: bool,
    conflicting_modifier: bool,
) -> bool {
    if !command || conflicting_modifier {
        return false;
    }
    match characters_ignoring_modifiers {
        Some(characters) if characters.eq_ignore_ascii_case("c") => true,
        Some(characters) if characters.is_ascii() => false,
        Some(_) | None => key_code == 0x08,
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
pub(crate) mod tests {
    pub(crate) fn record_paste() {
        super::record_clipboard_paste();
    }
    use super::*;

    #[test]
    fn test_native_queue_is_local_to_its_owner_thread() {
        record_paste();
        let foreign_batch = std::thread::spawn(drain).join().unwrap();
        assert!(
            !foreign_batch.clipboard_paste,
            "another harness drained this test's native paste"
        );
        assert!(drain().clipboard_paste);
    }

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

    #[test]
    fn command_v는_논리키와_한글배열_물리키_fallback으로_잡는다() {
        assert!(is_clipboard_paste_key(0x30, Some("v"), true, false));
        assert!(is_clipboard_paste_key(0x09, Some("ㅍ"), true, false));
        assert!(is_clipboard_paste_key(0x09, None, true, false));
        assert!(!is_clipboard_paste_key(0x09, Some("v"), false, false));
        assert!(!is_clipboard_paste_key(0x09, Some("v"), true, true));
        assert!(!is_clipboard_paste_key(0x08, Some("c"), true, false));
    }

    #[test]
    fn command_c는_논리키와_한글배열_물리키_fallback으로_잡는다() {
        assert!(is_clipboard_copy_key(0x30, Some("c"), true, false));
        assert!(is_clipboard_copy_key(0x08, Some("ㅊ"), true, false));
        assert!(is_clipboard_copy_key(0x08, None, true, false));
        assert!(!is_clipboard_copy_key(0x08, Some("x"), true, false));
        assert!(!is_clipboard_copy_key(0x08, Some("c"), false, false));
        assert!(!is_clipboard_copy_key(0x08, Some("c"), true, true));
        assert!(!is_clipboard_copy_key(0x09, Some("v"), true, false));
    }

    #[test]
    fn enter_전후_문장부호의_물리순서를_보존한다() {
        let batch = collect_batch([
            NativeKeyDown::Printable(NativePrintableKeyDown::for_test('.')),
            NativeKeyDown::Submit {
                observed_at: Instant::now(),
            },
            NativeKeyDown::Printable(NativePrintableKeyDown::for_test(',')),
        ]);
        assert_eq!(batch.printable.len(), 2);
        assert!(!batch.printable[0].after_submit);
        assert!(batch.printable[1].after_submit);
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
