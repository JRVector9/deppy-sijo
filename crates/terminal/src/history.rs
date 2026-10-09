//! Immutable history requests; source identities are independent of native scroll position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TerminalHistoryMetadata {
    pub generation: u64,
    pub first_line: u64,
    pub total_lines: u32,
    /// Native PTY geometry, independent of the queried display window.
    pub native_rows: u16,
    /// Source identity of the first native live row, even when the display
    /// window begins earlier or contains only a suffix of the native screen.
    pub live_first_line: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TerminalHistoryAnchor {
    pub generation: u64,
    pub first_line: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TerminalHistoryQuery {
    pub anchor: Option<TerminalHistoryAnchor>,
    pub delta: i32,
    pub reset: bool,
    /// None preserves native-sized reads. Display reads are capped separately
    /// and return at most the available source rows. At offset0 the returned
    /// window ends at the native live end; anchored reads preserve their start
    /// until a source boundary requires clamping.
    #[serde(default)]
    pub rows: Option<u16>,
}

pub const HISTORY_QUERY_MAX_ROWS: u16 = 256;
pub const HISTORY_QUERY_MAX_CELLS: usize = 65_536;

impl TerminalHistoryQuery {
    pub fn live() -> Self {
        Self {
            anchor: None,
            delta: 0,
            reset: true,
            rows: None,
        }
    }
}

pub struct TerminalHistorySnapshot {
    pub snapshot: crate::TerminalViewportSnapshot,
    pub expired: bool,
}
