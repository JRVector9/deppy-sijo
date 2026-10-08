use crate::visible_cells::VisibleCells;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// 설계문서 8.1 TerminalViewportSnapshot. UI가 보는 유일한 화면 상태.
/// serde는 remote transport(PR-19) 직렬화용 — visible_cells의 Arc는 serde rc feature.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TerminalViewportSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub cursor: CursorSnapshot,
    /// row-major, cols * rows개
    pub visible_cells: VisibleCells,
    /// Sorted sparse full text for cells whose NFC contains multiple scalars.
    pub graphemes: Arc<[CellGrapheme]>,
    /// 직전 take_snapshot 이후 바뀐 셀 범위 — renderer_egui가 행 갤리 캐시 무효화에,
    /// 세션 로직이 dirty 추적에 소비한다 (2026-07-13 감사: "미소비" 주석 stale 교정).
    pub dirty_ranges: Vec<CellRange>,
    pub title: Option<String>,
    pub scroll_offset: i32,
    pub is_alt_screen: bool,
    pub history: Option<crate::TerminalHistoryMetadata>,
}

/// A cell's full grapheme, stored only when a scalar cannot represent it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CellGrapheme {
    pub index: usize,
    pub text: String,
}

/// Avoid a fresh Arc allocation on every ASCII-only snapshot.
pub fn share_cell_graphemes(entries: Vec<CellGrapheme>) -> Arc<[CellGrapheme]> {
    if entries.is_empty() {
        static EMPTY: std::sync::OnceLock<Arc<[CellGrapheme]>> = std::sync::OnceLock::new();
        Arc::clone(EMPTY.get_or_init(|| Arc::from([])))
    } else {
        entries.into()
    }
}

/// Peer limits bound sparse text without imposing allocations on ASCII cells.
pub const MAX_CELL_GRAPHEME_BYTES: usize = 16 * 1024;
pub const MAX_CELL_GRAPHEME_CHARS: usize = 4096;

pub fn validate_cell_graphemes(
    cells: &[TerminalCell],
    graphemes: &[CellGrapheme],
) -> Result<(), &'static str> {
    let mut previous = None;
    for entry in graphemes {
        let Some(cell) = cells.get(entry.index) else {
            return Err("grapheme index out of bounds");
        };
        if previous.is_some_and(|index| index >= entry.index) {
            return Err("grapheme indices must be unique and sorted");
        }
        if cell.wide_spacer() || entry.text.len() > MAX_CELL_GRAPHEME_BYTES {
            return Err("invalid grapheme owner or byte limit");
        }
        let mut chars = entry.text.chars();
        if chars.next() != Some(cell.c) || chars.next().is_none() {
            return Err("grapheme must contain its base and multiple scalars");
        }
        if entry.text.chars().count() > MAX_CELL_GRAPHEME_CHARS
            || entry.text.chars().any(char::is_control)
        {
            return Err("invalid grapheme scalar content or count");
        }
        // Alacritty appends every width-0 scalar to the preceding owner cell. ZWSP,
        // bidi and other format scalars can form separate UAX29 clusters, so requiring
        // exactly one cluster would reject valid native snapshots. Preserve that
        // engine contract. A backend's joined emoji cluster may include spacing
        // scalars; allow it only as one cluster fitting this owner's 1/2-cell span.
        let native_zero_width_suffix = entry.text.chars().skip(1).all(|c| c.width() == Some(0));
        if !native_zero_width_suffix
            && (entry.text.graphemes(true).take(2).count() != 1
                || entry.text.width() > if cell.wide() { 2 } else { 1 })
        {
            return Err("sparse text contains spacing glyphs outside its owner cell");
        }
        previous = Some(entry.index);
    }
    Ok(())
}

impl TerminalViewportSnapshot {
    pub fn cell_grapheme(&self, index: usize) -> Option<&str> {
        self.graphemes
            .binary_search_by_key(&index, |entry| entry.index)
            .ok()
            .map(|i| self.graphemes[i].text.as_str())
    }

    pub fn push_cell_text(&self, index: usize, output: &mut String) {
        if let Some(text) = self.cell_grapheme(index) {
            output.push_str(text);
        } else if let Some(cell) = self.visible_cells.get(index) {
            output.push(cell.c);
        }
    }

    pub fn row_graphemes(&self, row: usize) -> &[CellGrapheme] {
        let start = row * self.cols as usize;
        let end = start + self.cols as usize;
        let first = self.graphemes.partition_point(|entry| entry.index < start);
        let last = self.graphemes.partition_point(|entry| entry.index < end);
        &self.graphemes[first..last]
    }

    /// 이 셀이 **진짜 wide 글자의 뒷칸**인가.
    ///
    /// 백엔드는 성질이 다른 둘을 같은 `wide_spacer` 비트로 평탄화한다:
    /// - **뒷칸** (alacritty `WIDE_CHAR_SPACER`, ghostty `SpacerTail`) — 같은 행 **앞 칸**의
    ///   2칸 글자가 소유한다. 글자의 일부다.
    /// - **행 끝 필러** (alacritty `LEADING_WIDE_CHAR_SPACER`, ghostty `SpacerHead`) — 2칸
    ///   글자가 행 끝에 들어가지 못해 다음 줄로 밀릴 때 그 행 마지막 칸에 남는 빈 자리다.
    ///   소유자는 **다음 줄 0열**이라 이 행에는 아무 글자도 없다.
    ///
    /// 둘을 구분하지 않으면 ① 눈에는 빈 행인데 "내용 있음"으로 판정되어 더블클릭에 강조
    /// 막대가 생기고 ② 선택 강조가 끝점보다 한 칸 더 칠해진다(2026-08-18 리뷰가 alacritty
    /// 백엔드로 실측). 구분 기준은 **같은 행의 앞 칸이 실제 소유자(`wide`)인가**다.
    pub fn is_trailing_wide_spacer(&self, index: usize) -> bool {
        let cols = self.cols as usize;
        if cols == 0
            || !self
                .visible_cells
                .get(index)
                .is_some_and(|cell| cell.wide_spacer())
        {
            return false;
        }
        if index.is_multiple_of(cols) {
            // 행 0열 — 앞 칸이 같은 행에 없다(행 끝 필러의 소유자가 여기 온다).
            return false;
        }
        self.visible_cells
            .get(index - 1)
            .is_some_and(|owner| owner.wide())
    }
}

/// Fixed snapshot payload: char + RGB + RGB + 7 flag bits, normally aligned to 12 bytes.
/// Grapheme text stays in the snapshot's sparse side table.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerminalCell {
    pub c: char,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    flags: u8,
}

impl TerminalCell {
    const ATTR_MASK: u8 = 0x1f;
    const WIDE: u8 = 1 << 5;
    const SPACER: u8 = 1 << 6;

    pub const fn new(
        c: char,
        fg: [u8; 3],
        bg: [u8; 3],
        wide: bool,
        wide_spacer: bool,
        attrs: CellAttrs,
    ) -> Self {
        assert!(
            attrs.0 & !Self::ATTR_MASK == 0,
            "unsupported cell attributes"
        );
        Self {
            c,
            fg,
            bg,
            flags: attrs.0
                | if wide { Self::WIDE } else { 0 }
                | if wide_spacer { Self::SPACER } else { 0 },
        }
    }
    pub const fn wide(self) -> bool {
        self.flags & Self::WIDE != 0
    }
    pub const fn wide_spacer(self) -> bool {
        self.flags & Self::SPACER != 0
    }
    pub const fn attrs(self) -> CellAttrs {
        CellAttrs(self.flags & Self::ATTR_MASK)
    }
    pub fn set_wide_spacer(&mut self, value: bool) {
        if value {
            self.flags |= Self::SPACER;
        } else {
            self.flags &= !Self::SPACER;
        }
    }
}

/// Keep the original six-field postcard/JSON contract independent of memory layout.
#[derive(serde::Serialize, serde::Deserialize)]
struct TerminalCellWire {
    c: char,
    fg: [u8; 3],
    bg: [u8; 3],
    wide: bool,
    wide_spacer: bool,
    attrs: CellAttrs,
}
impl serde::Serialize for TerminalCell {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        TerminalCellWire {
            c: self.c,
            fg: self.fg,
            bg: self.bg,
            wide: self.wide(),
            wide_spacer: self.wide_spacer(),
            attrs: self.attrs(),
        }
        .serialize(serializer)
    }
}
impl<'de> serde::Deserialize<'de> for TerminalCell {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = TerminalCellWire::deserialize(deserializer)?;
        if wire.attrs.0 & !Self::ATTR_MASK != 0 {
            return Err(serde::de::Error::custom("unsupported cell attributes"));
        }
        Ok(Self::new(
            wire.c,
            wire.fg,
            wire.bg,
            wire.wide,
            wire.wide_spacer,
            wire.attrs,
        ))
    }
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

#[cfg(test)]
mod compact_tests {
    use super::*;
    #[test]
    fn sparse_text_accepts_single_joined_cluster_only_with_its_owner_width() {
        let full = "👩\u{200d}💻";
        let entry = CellGrapheme {
            index: 0,
            text: full.into(),
        };
        let wide = TerminalCell::new('👩', [0; 3], [0; 3], true, false, CellAttrs::empty());
        assert!(validate_cell_graphemes(&[wide], std::slice::from_ref(&entry)).is_ok());
        let narrow = TerminalCell::new('👩', [0; 3], [0; 3], false, false, CellAttrs::empty());
        assert!(validate_cell_graphemes(&[narrow], &[entry]).is_err());
    }

    #[test]
    fn sparse_text_rejects_two_spacing_clusters() {
        for text in ["ab", "a\u{301}b", "a\u{200b}b"] {
            let cell = TerminalCell::new('a', [0; 3], [0; 3], false, false, CellAttrs::empty());
            assert!(
                validate_cell_graphemes(
                    &[cell],
                    &[CellGrapheme {
                        index: 0,
                        text: text.into()
                    }]
                )
                .is_err()
            );
        }
    }

    #[test]
    fn compact_cell_preserves_all_supported_flag_combinations() {
        for attrs in 0..32 {
            for wide in [false, true] {
                for spacer in [false, true] {
                    let mut cell = TerminalCell::new(
                        '한',
                        [1, 2, 3],
                        [4, 5, 6],
                        wide,
                        spacer,
                        CellAttrs(attrs),
                    );
                    assert_eq!(cell.wide(), wide);
                    assert_eq!(cell.wide_spacer(), spacer);
                    assert_eq!(cell.attrs(), CellAttrs(attrs));
                    cell.set_wide_spacer(!spacer);
                    assert_eq!(cell.wide_spacer(), !spacer);
                    assert_eq!(cell.wide(), wide);
                    assert_eq!(cell.attrs(), CellAttrs(attrs));
                }
            }
        }
    }

    #[test]
    fn terminal_cell_payload_is_twelve_bytes() {
        assert_eq!(std::mem::size_of::<TerminalCell>(), 12);
        assert_eq!(std::mem::align_of::<TerminalCell>(), 4);
    }
}
