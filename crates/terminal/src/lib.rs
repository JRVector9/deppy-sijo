//! Terminal emulation 격리 crate (설계문서 4장 / 9장).
//! alacritty_terminal 타입은 이 crate 밖으로 노출하지 않는다 —
//! UI는 TerminalBackend / TerminalViewportSnapshot / renderer만 본다.

mod alacritty_backend;
mod backend;
mod change_set;
#[cfg(feature = "ghostty-backend")]
mod ghostty_backend;
pub mod input_mapper;
pub mod renderer_egui;
mod viewport_snapshot;

pub use alacritty_backend::AlacrittyBackend;
#[cfg(feature = "ghostty-backend")]
pub use ghostty_backend::GhosttyBackend;

/// 기본 백엔드 팩토리 — Session이 사용한다. `ghostty-backend` feature 빌드에서
/// `DEPPY_TERM_BACKEND=ghostty`면 LibGhosttyBackend, 그 외 AlacrittyBackend (A/B 실측용).
/// 어느 엔진이 선택됐는지 stderr로 남긴다 (A/B 러너가 확인).
pub fn new_default_backend(
    cols: u16,
    rows: u16,
    scrollback_lines: usize,
) -> Box<dyn TerminalBackend> {
    #[cfg(feature = "ghostty-backend")]
    if std::env::var("DEPPY_TERM_BACKEND").is_ok_and(|v| v.eq_ignore_ascii_case("ghostty")) {
        match ghostty_backend::GhosttyBackend::new(cols, rows, scrollback_lines) {
            Ok(backend) => {
                eprintln!("[terminal] backend=ghostty (cols={cols} rows={rows})");
                return Box::new(backend);
            }
            Err(e) => eprintln!("[terminal] ghostty backend 생성 실패 — alacritty 폴백: {e:#}"),
        }
    }
    Box::new(AlacrittyBackend::new(cols, rows, scrollback_lines))
}
pub use backend::{
    ScrollbackMatch, ScrollbackSearchResult, TERMINAL_GLOBAL_CACHE_BUDGET_BYTES, TerminalBackend,
    TerminalCacheBudget, TerminalCacheClass, TerminalCacheEvent, TerminalCacheEventKind,
    TerminalCacheFootprint, TerminalExternalSurfaceHandle, TerminalRenderModel, fold_char,
    substring_matches,
};
pub use change_set::TerminalChangeSet;
pub use viewport_snapshot::{
    CellRange, CursorShape, CursorSnapshot, TerminalCell, TerminalViewportSnapshot,
};
