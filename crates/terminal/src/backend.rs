use crate::change_set::TerminalChangeSet;
use crate::viewport_snapshot::TerminalViewportSnapshot;

/// 설계문서 4.2 TerminalRenderModel.
pub enum TerminalRenderModel {
    CellGrid,
    /// LibGhosttyBackend Mode B용 (v1.x) — 현재 구현체 없음
    ExternalSurface,
}

/// ExternalSurface 렌더 모델의 surface 핸들 (설계문서 4.2).
/// v0에서는 사용처가 없다 — LibGhosttyBackend Mode B에서 구체화.
pub struct TerminalExternalSurfaceHandle;

/// 설계문서 4.2 TerminalBackend trait.
/// `bracketed_paste`는 설계 trait에 없지만 PR-05 완료 기준(bracketed paste)이
/// 입력 경로에서 모드 조회를 요구해 추가했다.
pub trait TerminalBackend {
    fn feed(&mut self, bytes: &[u8]) -> anyhow::Result<TerminalChangeSet>;
    fn resize(&mut self, cols: u16, rows: u16) -> anyhow::Result<()>;
    fn render_model(&self) -> TerminalRenderModel;

    fn viewport_snapshot(&self) -> Option<TerminalViewportSnapshot>;
    fn external_surface(&self) -> Option<TerminalExternalSurfaceHandle>;

    fn scroll(&mut self, delta: i32);
    fn reset(&mut self);

    fn bracketed_paste(&self) -> bool;

    /// 현재 화면(스크롤 무시, 실제 grid)의 텍스트 — status detector용 경량 조회.
    /// TerminalViewportSnapshot을 만들지 않는다 (설계문서 PR-12: hidden session 규칙).
    fn screen_text(&self) -> String;
}
