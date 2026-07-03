use std::sync::Arc;

/// 설계문서 8.1 TerminalViewportSnapshot. UI가 보는 유일한 화면 상태.
pub struct TerminalViewportSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub cursor: CursorSnapshot,
    /// row-major, cols * rows개
    pub visible_cells: Arc<[TerminalCell]>,
    /// 렌더 최적화용 (PR-21에서 소비 — 현재 렌더러는 전체를 그린다)
    pub dirty_ranges: Vec<CellRange>,
    pub title: Option<String>,
    pub scroll_offset: i32,
    pub is_alt_screen: bool,
}

/// 색상은 backend에서 RGB로 해석을 끝낸다 — UI는 팔레트를 모른다.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerminalCell {
    pub c: char,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    /// wide char(한글 등)의 첫 셀 — 2셀 폭으로 렌더링
    pub wide: bool,
    /// wide char 뒤의 자리 채움 셀 — 렌더링하지 않는다
    pub wide_spacer: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CursorSnapshot {
    pub col: u16,
    pub row: u16,
    pub shape: CursorShape,
    pub visible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellRange {
    pub start: usize,
    pub end: usize,
}
