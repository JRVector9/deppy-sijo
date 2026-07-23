//! macOS 시스템 메모리 압박 감지 (docs/runaway-protection-roadmap.md C1).
//!
//! `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE`를 구독해 warning/critical 레벨을
//! lock-free 전역 상태로 노출한다. 커널 push가 `request_repaint()`로 `logic()`을
//! 깨우는 구조라 상시 폴링이 없다. 이 모듈은 신호만 제공한다 — 소비(비상 플러시
//! C2, 압박 알림 C3)는 `logic()` 쪽이 담당한다.

use std::sync::atomic::{AtomicU8, Ordering};

/// discriminant 순서가 곧 심각도 — Ord 비교로 격상(escalation)을 판정한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum PressureLevel {
    Normal = 0,
    Warn = 1,
    Critical = 2,
}

static LEVEL: AtomicU8 = AtomicU8::new(0);
/// take_level_transition()이 마지막으로 관측한 레벨 — 전이 1회 감지용.
static LAST_OBSERVED: AtomicU8 = AtomicU8::new(0);

fn decode(value: u8) -> PressureLevel {
    match value {
        2 => PressureLevel::Critical,
        1 => PressureLevel::Warn,
        _ => PressureLevel::Normal,
    }
}

/// `logic()` 프레임에서 호출 — 마지막 관측 이후 레벨이 바뀌었으면 (이전, 현재)를
/// 반환한다. 소비자가 하나(logic 루프)라는 전제의 단순 전이 감지.
pub fn take_level_transition() -> Option<(PressureLevel, PressureLevel)> {
    let current = LEVEL.load(Ordering::Acquire);
    let previous = LAST_OBSERVED.swap(current, Ordering::AcqRel);
    (previous != current).then(|| (decode(previous), decode(current)))
}

/// GCD 콜백 없이 소비 로직을 결정적으로 테스트하기 위한 주입 지점
/// (`NativePrintableKeyDown::for_test`와 동일한 관례).
#[cfg(test)]
pub fn test_set_level(level: PressureLevel) {
    LEVEL.store(level as u8, Ordering::Release);
}

#[cfg(target_os = "macos")]
pub fn install(ctx: egui::Context) {
    use std::sync::OnceLock;
    use std::sync::atomic::AtomicBool;

    use dispatch2::{_dispatch_source_type_memorypressure, DispatchObject, DispatchSource};

    // 프로세스당 1회 설치 (native_key_monitor와 동일 가드).
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    static EGUI_CTX: OnceLock<egui::Context> = OnceLock::new();
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }
    let _ = EGUI_CTX.set(ctx);

    extern "C" fn on_pressure(context: *mut std::ffi::c_void) {
        // SAFETY: context는 install()이 leak한 DispatchSource 포인터 — 소스는
        // 프로세스 수명 동안 해제/이동되지 않는다(mem::forget으로 영구 보존).
        let source = unsafe { &*(context.cast_const().cast::<DispatchSource>()) };
        let flags = source.data();
        // 이벤트 병합(coalescing)으로 여러 비트가 동시에 설 수 있다 — 보수적으로
        // 높은 심각도를 우선한다. 놓친 해소는 다음 NORMAL 단독 이벤트가 바로잡는다.
        let level = if flags & 0x4 != 0 {
            PressureLevel::Critical
        } else if flags & 0x2 != 0 {
            PressureLevel::Warn
        } else {
            PressureLevel::Normal
        };
        LEVEL.store(level as u8, Ordering::Release);
        if let Some(ctx) = EGUI_CTX.get() {
            ctx.request_repaint();
        }
    }

    // mask 0x7 = NORMAL|WARN|CRITICAL — NORMAL 복귀도 받아야 압박 에피소드 해소를
    // 관측한다. 기본 글로벌 큐(None): 핸들러는 원자적 쓰기 + repaint 요청뿐이라
    // 전용 직렬 큐가 필요 없다.
    // SAFETY: memorypressure 소스 타입은 handle을 무시하고(0), mask는 위 flag
    // 조합만 유효하다. 반환 소스는 아래에서 활성화 전에 context/handler를 먼저
    // 설정한다(Apple 문서의 요구 순서).
    let source = unsafe {
        DispatchSource::new(
            std::ptr::addr_of!(_dispatch_source_type_memorypressure).cast_mut(),
            0,
            0x7,
            None,
        )
    };
    let raw: *const DispatchSource = &*source;
    // SAFETY: raw는 위에서 leak 예정인 소스 자신 — 핸들러 수명 동안 항상 유효하다.
    unsafe { source.set_context(raw.cast_mut().cast()) };
    source.set_event_handler_f(on_pressure);
    source.activate();
    // 소스를 프로세스 수명 동안 보존 — drop되면 구독이 끊긴다.
    std::mem::forget(source);
}

#[cfg(not(target_os = "macos"))]
pub fn install(_ctx: egui::Context) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_install은_중복_호출에도_안전하다() {
        // 실제 dispatch source 생성/컨텍스트/핸들러/활성화 FFI 경로 스모크 —
        // 두 번째 호출은 INSTALLED 가드로 no-op이어야 한다.
        install(egui::Context::default());
        install(egui::Context::default());
    }

    #[test]
    fn 레벨_전이는_한_번만_보고된다() {
        test_set_level(PressureLevel::Normal);
        let _ = take_level_transition();
        test_set_level(PressureLevel::Warn);
        assert_eq!(
            take_level_transition(),
            Some((PressureLevel::Normal, PressureLevel::Warn))
        );
        // 같은 레벨 유지 중에는 다시 보고하지 않는다.
        assert_eq!(take_level_transition(), None);
        test_set_level(PressureLevel::Critical);
        assert_eq!(
            take_level_transition(),
            Some((PressureLevel::Warn, PressureLevel::Critical))
        );
        test_set_level(PressureLevel::Normal);
        assert_eq!(
            take_level_transition(),
            Some((PressureLevel::Critical, PressureLevel::Normal))
        );
    }
}
