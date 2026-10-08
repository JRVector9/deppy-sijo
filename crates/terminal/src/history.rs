//! Immutable history requests; source identities are independent of native scroll position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TerminalHistoryMetadata {
    pub generation: u64,
    pub first_line: u64,
    pub total_lines: u32,
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
}

impl TerminalHistoryQuery {
    pub fn live() -> Self {
        Self {
            anchor: None,
            delta: 0,
            reset: true,
        }
    }
}

pub struct TerminalHistorySnapshot {
    pub snapshot: crate::TerminalViewportSnapshot,
    pub expired: bool,
}
