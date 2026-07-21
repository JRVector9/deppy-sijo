use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use connector_contract::{ConnectorSnapshot, Revision};

pub(crate) struct SnapshotCell {
    revision: AtomicU64,
    value: Mutex<Arc<ConnectorSnapshot>>,
}

impl SnapshotCell {
    pub(crate) fn new() -> Self {
        Self {
            revision: AtomicU64::new(0),
            value: Mutex::new(Arc::new(ConnectorSnapshot::default())),
        }
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    pub(crate) fn current(&self) -> Arc<ConnectorSnapshot> {
        Arc::clone(&self.value.lock().expect("connector snapshot lock"))
    }

    pub(crate) fn publish(&self, mut next: ConnectorSnapshot) -> u64 {
        let revision = self.revision.load(Ordering::Relaxed).saturating_add(1);
        next.revision = Revision(revision);
        *self.value.lock().expect("connector snapshot lock") = Arc::new(next);
        self.revision.store(revision, Ordering::Release);
        revision
    }
}

/// Per-consumer latest-only reader. It clones the `Arc` only after the atomic
/// revision changes; unchanged render frames touch only one atomic.
pub struct SnapshotReader {
    cell: Arc<SnapshotCell>,
    seen_revision: u64,
    current: Arc<ConnectorSnapshot>,
}

impl SnapshotReader {
    pub(crate) fn new(cell: Arc<SnapshotCell>) -> Self {
        let seen_revision = cell.revision();
        let current = cell.current();
        Self {
            cell,
            seen_revision,
            current,
        }
    }

    pub fn refresh(&mut self) -> bool {
        let revision = self.cell.revision();
        if revision == self.seen_revision {
            return false;
        }
        self.current = self.cell.current();
        self.seen_revision = revision;
        true
    }

    pub fn snapshot(&self) -> &Arc<ConnectorSnapshot> {
        &self.current
    }

    pub fn revision(&self) -> Revision {
        Revision(self.seen_revision)
    }
}
