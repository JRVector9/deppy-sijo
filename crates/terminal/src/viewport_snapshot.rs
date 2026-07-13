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
    /// SGR 텍스트 속성 (2026-07-14 B-1). backend가 이 flag들을 그냥 버리고 있었다 —
    /// bold/italic/underline이 화면에 전혀 반영되지 않았다. 비트 하나로 유지해
    /// 셀 크기 증가를 최소화한다(정렬 포함 기존 12B → 12B, wide/spacer 옆 패딩 활용).
    pub attrs: CellAttrs,
}

/// 셀의 SGR 텍스트 속성 비트셋 (B-1). INVERSE/HIDDEN은 backend가 이미 fg/bg·문자에
/// 반영하므로 여기에 없다 — 렌더러가 알아야 하는 것만 담는다.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CellAttrs(pub u8);

impl CellAttrs {
    pub const BOLD: u8 = 1 << 0;
    pub const ITALIC: u8 = 1 << 1;
    pub const UNDERLINE: u8 = 1 << 2;
    pub const STRIKEOUT: u8 = 1 << 3;
    /// SGR 2 — 밝기를 낮춘다(색을 어둡게).
    pub const DIM: u8 = 1 << 4;

    pub const fn empty() -> Self {
        Self(0)
    }
    pub const fn contains(self, bit: u8) -> bool {
        self.0 & bit != 0
    }
    pub fn set(&mut self, bit: u8, on: bool) {
        if on {
            self.0 |= bit;
        } else {
            self.0 &= !bit;
        }
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
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
