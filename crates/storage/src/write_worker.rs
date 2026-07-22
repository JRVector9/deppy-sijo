use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use mcp_store::PendingApprovalInsert;
use rusqlite::{Connection, TransactionBehavior};

/// Background SQLite writer configuration for hot metadata paths.
#[derive(Debug, Clone)]
pub struct DbWriteWorkerConfig {
    /// Debounce window before flushing coalesced status/offset writes.
    pub flush_interval: Duration,
    /// Maximum distinct sessions waiting for status updates.
    pub max_status_sessions: usize,
    /// Maximum distinct sessions waiting for log offset updates.
    pub max_log_offset_sessions: usize,
    /// Maximum durable notification-like inserts waiting for a batch.
    ///
    /// The current schema has no generic notifications table; pending approvals
    /// are the durable notification/IPC rows that need insertion batching.
    pub max_notification_batch: usize,
}

impl Default for DbWriteWorkerConfig {
    fn default() -> Self {
        Self {
            flush_interval: Duration::from_millis(50),
            max_status_sessions: 2048,
            max_log_offset_sessions: 2048,
            max_notification_batch: 1024,
        }
    }
}

/// Testable counters for the background writer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DbWriteStatsSnapshot {
    pub status_enqueued: u64,
    pub status_coalesced: u64,
    pub status_flushed: u64,
    pub status_missing: u64,
    pub log_offset_enqueued: u64,
    pub log_offset_coalesced: u64,
    pub log_offset_flushed: u64,
    pub log_offset_missing: u64,
    pub notification_enqueued: u64,
    pub notification_flushed: u64,
    pub flushes: u64,
    pub transactions: u64,
    pub queue_full: u64,
    pub errors: u64,
}

#[derive(Default)]
struct DbWriteStats {
    status_enqueued: AtomicU64,
    status_coalesced: AtomicU64,
    status_flushed: AtomicU64,
    status_missing: AtomicU64,
    log_offset_enqueued: AtomicU64,
    log_offset_coalesced: AtomicU64,
    log_offset_flushed: AtomicU64,
    log_offset_missing: AtomicU64,
    notification_enqueued: AtomicU64,
    notification_flushed: AtomicU64,
    flushes: AtomicU64,
    transactions: AtomicU64,
    queue_full: AtomicU64,
    errors: AtomicU64,
}

impl DbWriteStats {
    fn snapshot(&self) -> DbWriteStatsSnapshot {
        DbWriteStatsSnapshot {
            status_enqueued: self.status_enqueued.load(Ordering::Relaxed),
            status_coalesced: self.status_coalesced.load(Ordering::Relaxed),
            status_flushed: self.status_flushed.load(Ordering::Relaxed),
            status_missing: self.status_missing.load(Ordering::Relaxed),
            log_offset_enqueued: self.log_offset_enqueued.load(Ordering::Relaxed),
            log_offset_coalesced: self.log_offset_coalesced.load(Ordering::Relaxed),
            log_offset_flushed: self.log_offset_flushed.load(Ordering::Relaxed),
            log_offset_missing: self.log_offset_missing.load(Ordering::Relaxed),
            notification_enqueued: self.notification_enqueued.load(Ordering::Relaxed),
            notification_flushed: self.notification_flushed.load(Ordering::Relaxed),
            flushes: self.flushes.load(Ordering::Relaxed),
            transactions: self.transactions.load(Ordering::Relaxed),
            queue_full: self.queue_full.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbWriteQueueError {
    Closed,
    TooManyStatusSessions { limit: usize },
    TooManyLogOffsetSessions { limit: usize },
    NotificationQueueFull { limit: usize },
}

impl fmt::Display for DbWriteQueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "background DB writer is closed"),
            Self::TooManyStatusSessions { limit } => {
                write!(f, "too many queued session status updates (limit {limit})")
            }
            Self::TooManyLogOffsetSessions { limit } => {
                write!(
                    f,
                    "too many queued session log offset updates (limit {limit})"
                )
            }
            Self::NotificationQueueFull { limit } => {
                write!(f, "too many queued notification inserts (limit {limit})")
            }
        }
    }
}

impl std::error::Error for DbWriteQueueError {}

/// Owns the background SQLite writer thread.
pub struct DbWriteWorker {
    shared: Arc<Shared>,
    join: Option<thread::JoinHandle<()>>,
}

impl DbWriteWorker {
    /// Opens a WAL SQLite connection on the worker thread and starts batching.
    pub fn spawn(path: impl AsRef<Path>, config: DbWriteWorkerConfig) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let shared = Arc::new(Shared {
            state: Mutex::new(PendingState::default()),
            condvar: Condvar::new(),
            stats: Arc::new(DbWriteStats::default()),
        });
        {
            let mut state = shared.state.lock().expect("DB writer mutex poisoned");
            state.configure_limits(&config);
        }
        let thread_shared = Arc::clone(&shared);
        let thread_config = config.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let join = thread::spawn(move || {
            let conn = open_worker_connection(&path);
            match conn {
                Ok(conn) => {
                    let _ = ready_tx.send(Ok(()));
                    worker_loop(conn, thread_config, thread_shared);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("{e:#}")));
                }
            }
        });

        match ready_rx
            .recv()
            .context("DB writer startup channel closed")?
        {
            Ok(()) => Ok(Self {
                shared,
                join: Some(join),
            }),
            Err(message) => {
                let _ = join.join();
                anyhow::bail!(message);
            }
        }
    }

    pub fn handle(&self) -> DbWriteHandle {
        DbWriteHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    pub fn stats(&self) -> DbWriteStatsSnapshot {
        self.shared.stats.snapshot()
    }

    pub fn flush(&self) -> anyhow::Result<DbWriteStatsSnapshot> {
        self.handle().flush()
    }

    pub fn shutdown(mut self) -> anyhow::Result<DbWriteStatsSnapshot> {
        self.request_shutdown();
        if let Some(join) = self.join.take() {
            join.join()
                .map_err(|_| anyhow::anyhow!("DB writer thread panicked"))?;
        }
        Ok(self.shared.stats.snapshot())
    }

    fn request_shutdown(&self) {
        let mut state = self.shared.state.lock().expect("DB writer mutex poisoned");
        state.closed = true;
        self.shared.condvar.notify_one();
    }
}

impl Drop for DbWriteWorker {
    fn drop(&mut self) {
        self.request_shutdown();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[derive(Clone)]
pub struct DbWriteHandle {
    shared: Arc<Shared>,
}

impl DbWriteHandle {
    /// Debounced status update. Multiple updates for the same session collapse
    /// to the latest status before SQLite is touched.
    pub fn try_update_session_status(
        &self,
        session_id: impl Into<String>,
        status: impl Into<String>,
    ) -> Result<(), DbWriteQueueError> {
        let session_id = session_id.into();
        let status = status.into();
        let mut state = self.shared.state.lock().expect("DB writer mutex poisoned");
        if state.closed {
            return Err(DbWriteQueueError::Closed);
        }
        if !state.status_updates.contains_key(&session_id)
            && state.status_updates.len() >= state.configured_status_limit()
        {
            self.shared.stats.queue_full.fetch_add(1, Ordering::Relaxed);
            return Err(DbWriteQueueError::TooManyStatusSessions {
                limit: state.configured_status_limit(),
            });
        }
        if state.status_updates.insert(session_id, status).is_some() {
            self.shared
                .stats
                .status_coalesced
                .fetch_add(1, Ordering::Relaxed);
        }
        self.shared
            .stats
            .status_enqueued
            .fetch_add(1, Ordering::Relaxed);
        self.shared.condvar.notify_one();
        Ok(())
    }

    /// Batched log offset update. Repeated offsets for a session collapse to
    /// the greatest offset so progress never moves backwards.
    pub fn try_update_session_log_offset(
        &self,
        session_id: impl Into<String>,
        offset: u64,
    ) -> Result<(), DbWriteQueueError> {
        let session_id = session_id.into();
        let mut state = self.shared.state.lock().expect("DB writer mutex poisoned");
        if state.closed {
            return Err(DbWriteQueueError::Closed);
        }
        if !state.log_offsets.contains_key(&session_id)
            && state.log_offsets.len() >= state.configured_log_offset_limit()
        {
            self.shared.stats.queue_full.fetch_add(1, Ordering::Relaxed);
            return Err(DbWriteQueueError::TooManyLogOffsetSessions {
                limit: state.configured_log_offset_limit(),
            });
        }
        match state.log_offsets.get_mut(&session_id) {
            Some(existing) => {
                *existing = (*existing).max(offset);
                self.shared
                    .stats
                    .log_offset_coalesced
                    .fetch_add(1, Ordering::Relaxed);
            }
            None => {
                state.log_offsets.insert(session_id, offset);
            }
        }
        self.shared
            .stats
            .log_offset_enqueued
            .fetch_add(1, Ordering::Relaxed);
        self.shared.condvar.notify_one();
        Ok(())
    }

    /// Batches durable notification-like pending approval inserts.
    pub fn try_insert_pending_approval(
        &self,
        row: PendingApprovalInsert,
    ) -> Result<(), DbWriteQueueError> {
        let mut state = self.shared.state.lock().expect("DB writer mutex poisoned");
        if state.closed {
            return Err(DbWriteQueueError::Closed);
        }
        if state.pending_approvals.len() >= state.configured_notification_limit() {
            self.shared.stats.queue_full.fetch_add(1, Ordering::Relaxed);
            return Err(DbWriteQueueError::NotificationQueueFull {
                limit: state.configured_notification_limit(),
            });
        }
        state.pending_approvals.push_back(row);
        self.shared
            .stats
            .notification_enqueued
            .fetch_add(1, Ordering::Relaxed);
        self.shared.condvar.notify_one();
        Ok(())
    }

    /// Blocks until currently queued writes are flushed.
    pub fn flush(&self) -> anyhow::Result<DbWriteStatsSnapshot> {
        let (tx, rx) = mpsc::sync_channel(1);
        {
            let mut state = self.shared.state.lock().expect("DB writer mutex poisoned");
            if state.closed {
                anyhow::bail!("background DB writer is closed");
            }
            state.flush_waiters.push(tx);
            self.shared.condvar.notify_one();
        }
        rx.recv().context("DB writer flush channel closed")?
    }

    pub fn stats(&self) -> DbWriteStatsSnapshot {
        self.shared.stats.snapshot()
    }
}

struct Shared {
    state: Mutex<PendingState>,
    condvar: Condvar,
    stats: Arc<DbWriteStats>,
}

#[derive(Default)]
struct PendingState {
    status_updates: HashMap<String, String>,
    log_offsets: HashMap<String, u64>,
    pending_approvals: VecDeque<PendingApprovalInsert>,
    flush_waiters: Vec<mpsc::SyncSender<anyhow::Result<DbWriteStatsSnapshot>>>,
    closed: bool,
    limits: WorkerLimits,
}

#[derive(Debug, Clone, Copy)]
struct WorkerLimits {
    status_sessions: usize,
    log_offset_sessions: usize,
    notifications: usize,
}

impl Default for WorkerLimits {
    fn default() -> Self {
        let config = DbWriteWorkerConfig::default();
        Self {
            status_sessions: config.max_status_sessions,
            log_offset_sessions: config.max_log_offset_sessions,
            notifications: config.max_notification_batch,
        }
    }
}

impl PendingState {
    fn configure_limits(&mut self, config: &DbWriteWorkerConfig) {
        self.limits = WorkerLimits {
            status_sessions: config.max_status_sessions,
            log_offset_sessions: config.max_log_offset_sessions,
            notifications: config.max_notification_batch,
        };
    }

    fn configured_status_limit(&self) -> usize {
        self.limits.status_sessions
    }

    fn configured_log_offset_limit(&self) -> usize {
        self.limits.log_offset_sessions
    }

    fn configured_notification_limit(&self) -> usize {
        self.limits.notifications
    }

    fn has_pending(&self) -> bool {
        !self.status_updates.is_empty()
            || !self.log_offsets.is_empty()
            || !self.pending_approvals.is_empty()
    }

    fn reached_batch_limit(&self) -> bool {
        self.status_updates.len() >= self.limits.status_sessions
            || self.log_offsets.len() >= self.limits.log_offset_sessions
            || self.pending_approvals.len() >= self.limits.notifications
    }

    fn take_batch(&mut self) -> WriteBatch {
        WriteBatch {
            status_updates: std::mem::take(&mut self.status_updates),
            log_offsets: std::mem::take(&mut self.log_offsets),
            pending_approvals: self.pending_approvals.drain(..).collect(),
        }
    }
}

struct WriteBatch {
    status_updates: HashMap<String, String>,
    log_offsets: HashMap<String, u64>,
    pending_approvals: Vec<PendingApprovalInsert>,
}

impl WriteBatch {
    fn is_empty(&self) -> bool {
        self.status_updates.is_empty()
            && self.log_offsets.is_empty()
            && self.pending_approvals.is_empty()
    }
}

fn open_worker_connection(path: &Path) -> anyhow::Result<Connection> {
    storage_core::open_with_migrations(path, crate::db::MIGRATIONS)
}

fn worker_loop(mut conn: Connection, config: DbWriteWorkerConfig, shared: Arc<Shared>) {
    loop {
        let (batch, waiters, should_exit) = {
            let mut state = shared.state.lock().expect("DB writer mutex poisoned");
            while !state.closed && !state.has_pending() && state.flush_waiters.is_empty() {
                state = shared
                    .condvar
                    .wait(state)
                    .expect("DB writer mutex poisoned");
            }

            if !state.flush_waiters.is_empty() && !state.has_pending() {
                let waiters = std::mem::take(&mut state.flush_waiters);
                let should_exit = state.closed;
                (WriteBatch::empty(), waiters, should_exit)
            } else {
                let deadline = Instant::now() + config.flush_interval;
                while !state.closed
                    && state.flush_waiters.is_empty()
                    && state.has_pending()
                    && !state.reached_batch_limit()
                {
                    let now = Instant::now();
                    if now >= deadline {
                        break;
                    }
                    let timeout = deadline.saturating_duration_since(now);
                    let (guard, wait_result) = shared
                        .condvar
                        .wait_timeout(state, timeout)
                        .expect("DB writer mutex poisoned");
                    state = guard;
                    if wait_result.timed_out() {
                        break;
                    }
                }
                let batch = state.take_batch();
                let waiters = std::mem::take(&mut state.flush_waiters);
                let should_exit = state.closed;
                (batch, waiters, should_exit)
            }
        };

        let result = flush_batch(&mut conn, batch, &shared.stats);
        reply_to_waiters(waiters, result, &shared.stats);
        if should_exit {
            break;
        }
    }
}

impl WriteBatch {
    fn empty() -> Self {
        Self {
            status_updates: HashMap::new(),
            log_offsets: HashMap::new(),
            pending_approvals: Vec::new(),
        }
    }
}

fn flush_batch(
    conn: &mut Connection,
    batch: WriteBatch,
    stats: &DbWriteStats,
) -> anyhow::Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    // Pending-approval cap probes must serialize with every other writer before they read the
    // current count/byte budget. IMMEDIATE also keeps mixed status/log/approval batches atomic.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

    if !batch.status_updates.is_empty() {
        let mut stmt = tx.prepare(
            "UPDATE sessions
             SET status = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE id = ?1",
        )?;
        for (session_id, status) in &batch.status_updates {
            let affected = stmt
                .execute((session_id, status))
                .with_context(|| format!("session status batch update 실패: {session_id}"))?;
            if affected == 0 {
                stats.status_missing.fetch_add(1, Ordering::Relaxed);
            } else {
                stats.status_flushed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    if !batch.log_offsets.is_empty() {
        let mut stmt = tx.prepare(
            "UPDATE sessions
             SET last_log_offset = ?2,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE id = ?1",
        )?;
        for (session_id, offset) in &batch.log_offsets {
            let affected = stmt
                .execute((session_id, i64::try_from(*offset).unwrap_or(i64::MAX)))
                .with_context(|| format!("session log offset batch update 실패: {session_id}"))?;
            if affected == 0 {
                stats.log_offset_missing.fetch_add(1, Ordering::Relaxed);
            } else {
                stats.log_offset_flushed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    if !batch.pending_approvals.is_empty() {
        let inserted = mcp_store::insert_pending_approval_batch(&tx, &batch.pending_approvals)?;
        stats
            .notification_flushed
            .fetch_add(inserted as u64, Ordering::Relaxed);
    }

    tx.commit().context("DB write batch commit 실패")?;
    stats.flushes.fetch_add(1, Ordering::Relaxed);
    stats.transactions.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

fn reply_to_waiters(
    waiters: Vec<mpsc::SyncSender<anyhow::Result<DbWriteStatsSnapshot>>>,
    result: anyhow::Result<()>,
    stats: &DbWriteStats,
) {
    if waiters.is_empty() {
        if result.is_err() {
            stats.errors.fetch_add(1, Ordering::Relaxed);
        }
        return;
    }

    match result {
        Ok(()) => {
            let snapshot = stats.snapshot();
            for waiter in waiters {
                let _ = waiter.send(Ok(snapshot.clone()));
            }
        }
        Err(e) => {
            stats.errors.fetch_add(1, Ordering::Relaxed);
            let message = format!("{e:#}");
            for waiter in waiters {
                let _ = waiter.send(Err(anyhow::anyhow!(message.clone())));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_db(name: &str) -> (PathBuf, PathBuf) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "deppy-db-worker-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        (dir, path)
    }

    fn seed_session(path: &Path, session_id: &str) {
        let conn = storage_core::open_with_migrations(path, crate::db::MIGRATIONS).unwrap();
        conn.execute(
            "INSERT INTO workspaces (id, name, path, created_at, updated_at)
             VALUES ('ws-1', 'test', '', 'now', 'now')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions
               (id, workspace_id, session_kind, agent_id, title, command, args_json,
                cwd, status, created_at, updated_at, last_log_offset)
             VALUES (?1, 'ws-1', 'shell', NULL, 'sh', 'sh', '[]',
                '/', 'running', 'now', 'now', 0)",
            [session_id],
        )
        .unwrap();
    }

    fn pending(id: &str, created_at: i64) -> PendingApprovalInsert {
        #[allow(clippy::needless_update)]
        PendingApprovalInsert {
            id: id.to_owned(),
            server_id: "srv".to_owned(),
            tool_name: "read_file".to_owned(),
            arguments_preview: "path=/tmp/x".to_owned(),
            schema_hash: None,
            created_at,
            pane_id: None,
        }
    }

    fn handle_without_worker(config: DbWriteWorkerConfig) -> DbWriteHandle {
        let shared = Arc::new(Shared {
            state: Mutex::new(PendingState::default()),
            condvar: Condvar::new(),
            stats: Arc::new(DbWriteStats::default()),
        });
        shared.state.lock().unwrap().configure_limits(&config);
        DbWriteHandle { shared }
    }

    #[test]
    fn status와_log_offset은_session별로_coalesce된다() {
        let (dir, path) = temp_db("coalesce");
        seed_session(&path, "sess-1");
        let worker = DbWriteWorker::spawn(
            &path,
            DbWriteWorkerConfig {
                flush_interval: Duration::from_secs(30),
                ..DbWriteWorkerConfig::default()
            },
        )
        .unwrap();
        let handle = worker.handle();

        handle
            .try_update_session_status("sess-1", "waiting")
            .unwrap();
        handle
            .try_update_session_status("sess-1", "running")
            .unwrap();
        handle.try_update_session_status("sess-1", "done").unwrap();
        handle.try_update_session_log_offset("sess-1", 10).unwrap();
        handle.try_update_session_log_offset("sess-1", 7).unwrap();
        handle.try_update_session_log_offset("sess-1", 128).unwrap();

        let stats = handle.flush().unwrap();
        assert_eq!(stats.status_enqueued, 3);
        assert_eq!(stats.status_coalesced, 2);
        assert_eq!(stats.status_flushed, 1);
        assert_eq!(stats.log_offset_enqueued, 3);
        assert_eq!(stats.log_offset_coalesced, 2);
        assert_eq!(stats.log_offset_flushed, 1);
        assert_eq!(stats.transactions, 1);

        drop(worker);
        let conn = Connection::open(&path).unwrap();
        let (status, offset): (String, i64) = conn
            .query_row(
                "SELECT status, last_log_offset FROM sessions WHERE id = 'sess-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "done");
        assert_eq!(offset, 128);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pending_approval_queue_limit을_지킨다() {
        let handle = handle_without_worker(DbWriteWorkerConfig {
            max_notification_batch: 2,
            ..DbWriteWorkerConfig::default()
        });
        handle
            .try_insert_pending_approval(pending("a", 10))
            .unwrap();
        handle
            .try_insert_pending_approval(pending("b", 20))
            .unwrap();
        assert_eq!(
            handle.try_insert_pending_approval(pending("c", 30)),
            Err(DbWriteQueueError::NotificationQueueFull { limit: 2 })
        );
        let stats = handle.stats();
        assert_eq!(stats.notification_enqueued, 2);
        assert_eq!(stats.queue_full, 1);
    }

    #[test]
    fn pending_approval_insert는_batch된다() {
        let (dir, path) = temp_db("pending");
        let worker = DbWriteWorker::spawn(
            &path,
            DbWriteWorkerConfig {
                flush_interval: Duration::from_secs(30),
                ..DbWriteWorkerConfig::default()
            },
        )
        .unwrap();
        let handle = worker.handle();

        handle
            .try_insert_pending_approval(pending("a", 10))
            .unwrap();
        handle
            .try_insert_pending_approval(pending("b", 20))
            .unwrap();
        let stats = handle.flush().unwrap();
        assert_eq!(stats.notification_enqueued, 2);
        assert_eq!(stats.notification_flushed, 2);

        drop(worker);
        let conn = Connection::open(&path).unwrap();
        let ids: Vec<String> = conn
            .prepare("SELECT id FROM pending_approvals ORDER BY created_at")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(ids, vec!["a", "b"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn worker_connection_keeps_wal_enabled() {
        let (dir, path) = temp_db("wal");
        let worker = DbWriteWorker::spawn(&path, DbWriteWorkerConfig::default()).unwrap();
        let conn = Connection::open(&path).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        drop(worker);
        drop(conn);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
