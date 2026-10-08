//! Worker-authoritative, expiring remote geometry control. Client request numbers
//! remain a web correlation; random owner ids and worker epochs authorize writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TerminalControlRequest {
    Query,
    Acquire {
        owner: [u8; 16],
        expected_epoch: u64,
        ttl_ms: u32,
    },
    Renew {
        owner: [u8; 16],
        lease_epoch: u64,
        ttl_ms: u32,
    },
    Release {
        owner: [u8; 16],
        lease_epoch: u64,
    },
    Resize {
        owner: [u8; 16],
        lease_epoch: u64,
        revision: u64,
        cols: u16,
        rows: u16,
    },
}

pub const TERMINAL_CONTROL_TTL_MAX_MS: u32 = 45_000;

impl TerminalControlRequest {
    pub(crate) fn is_valid(self) -> bool {
        match self {
            Self::Query => true,
            Self::Acquire { owner, ttl_ms, .. } => {
                owner != [0; 16] && (1..=TERMINAL_CONTROL_TTL_MAX_MS).contains(&ttl_ms)
            }
            Self::Renew {
                owner,
                lease_epoch,
                ttl_ms,
            } => {
                owner != [0; 16]
                    && lease_epoch > 0
                    && (1..=TERMINAL_CONTROL_TTL_MAX_MS).contains(&ttl_ms)
            }
            Self::Release { owner, lease_epoch } => owner != [0; 16] && lease_epoch > 0,
            Self::Resize {
                owner,
                lease_epoch,
                revision,
                ..
            } => owner != [0; 16] && lease_epoch > 0 && revision > 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TerminalControlState {
    pub epoch: u64,
    pub owner: Option<[u8; 16]>,
    pub lease_ms: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TerminalControlStatus {
    Owned,
    InUse,
    Released,
    Expired,
    Unavailable,
    Stale,
    InvalidSize,
    ResizeFailed,
    RestoreFailed,
}

#[derive(Clone, Copy)]
pub(crate) struct NativeGeometry {
    pub cols: u16,
    pub rows: u16,
    pub token: Option<crate::ResizeToken>,
}

#[derive(Clone, Copy)]
pub(crate) struct ResizeReplay {
    pub revision: u64,
    pub target: (u16, u16),
    pub status: TerminalControlStatus,
    pub stamp: Option<crate::ResizeStamp>,
}

#[derive(Clone, Copy)]
pub(crate) struct ControlLease {
    pub owner: [u8; 16],
    pub epoch: u64,
    pub expires_at: std::time::Instant,
    pub resize: Option<ResizeReplay>,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ControlRecord {
    pub epoch: u64,
    pub lease: Option<ControlLease>,
    pub wanted: Option<NativeGeometry>,
    pub released: Option<(
        [u8; 16],
        u64,
        TerminalControlStatus,
        Option<crate::ResizeStamp>,
    )>,
}

impl ControlRecord {
    pub fn state(self, now: std::time::Instant) -> TerminalControlState {
        TerminalControlState {
            epoch: self.epoch,
            owner: self.lease.map(|lease| lease.owner),
            lease_ms: self.lease.map_or(0, |lease| {
                lease
                    .expires_at
                    .saturating_duration_since(now)
                    .as_millis()
                    .clamp(1, u128::from(TERMINAL_CONTROL_TTL_MAX_MS)) as u32
            }),
        }
    }
}
