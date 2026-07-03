//! Terminal emulation 격리 crate (설계문서 4장 / 9장).
//! alacritty_terminal 타입은 이 crate 밖으로 노출하지 않는다 —
//! UI는 TerminalBackend / TerminalViewportSnapshot / renderer만 본다.

mod alacritty_backend;
mod backend;
mod change_set;
pub mod input_mapper;
pub mod renderer_egui;
mod viewport_snapshot;

pub use alacritty_backend::AlacrittyBackend;
pub use backend::{TerminalBackend, TerminalExternalSurfaceHandle, TerminalRenderModel};
pub use change_set::TerminalChangeSet;
pub use viewport_snapshot::{
    CellRange, CursorShape, CursorSnapshot, TerminalCell, TerminalViewportSnapshot,
};
