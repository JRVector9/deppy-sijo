use std::sync::Arc;

/// 설계문서 8.1 TerminalViewportSnapshot. UI가 보는 유일한 화면 상태.
/// serde는 remote transport(PR-19) 직렬화용 — visible_cells의 Arc는 serde rc feature.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TerminalViewportSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub cursor: CursorSnapshot,
    /// row-major, cols * rows개
    pub visible_cells: Arc<[TerminalCell]>,
    /// 직전 take_snapshot 이후 바뀐 셀 범위 — renderer_egui가 행 갤리 캐시 무효화에,
    /// 세션 로직이 dirty 추적에 소비한다 (2026-07-13 감사: "미소비" 주석 stale 교정).
    pub dirty_ranges: Vec<CellRange>,
    pub title: Option<String>,
    pub scroll_offset: i32,
    pub is_alt_screen: bool,
}

/// 색상은 backend에서 RGB로 해석을 끝낸다 — UI는 팔레트를 모른다.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TerminalCell {
    pub c: char,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    /// wide char(한글 등)의 첫 셀 — 2셀 폭으로 렌더링
    pub wide: bool,
    /// wide char 뒤의 자리 채움 셀 — 렌더링하지 않는다
    pub wide_spacer: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CursorSnapshot {
    pub col: u16,
    pub row: u16,
    pub shape: CursorShape,
    pub visible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CellRange {
    pub start: usize,
    pub end: usize,
}
