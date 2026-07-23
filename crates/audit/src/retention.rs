//! Bounded, caller-driven audit retention.
//!
//! There is deliberately no timer or background worker here. A storage owner invokes retention at
//! an existing write/startup boundary. `Prepared` rows are never candidates: they must first be
//! reconciled to `Unknown` by the durable authorization-owner recovery path.

use anyhow::Context as _;
use rusqlite::{Connection, params_from_iter};
#[cfg(test)]
use rusqlite::{Transaction, TransactionBehavior};

/// Production ceiling for retained finalized rows. Callers may choose a lower value, never a
/// higher one. The byte ceiling normally becomes binding first for input-bearing records.
pub const AUDIT_RETENTION_MAX_FINALIZED_ITEMS: usize = 4_096;
/// Logical UTF-8/blob bytes retained by finalized rows. SQLite page high-water marks are reusable
/// and intentionally are not VACUUMed on the foreground path.
pub const AUDIT_RETENTION_MAX_LOGICAL_BYTES: usize = 8 * 1024 * 1024;
/// Finalized audit history is retained for at most 30 days.
pub const AUDIT_RETENTION_MAX_AGE_SECONDS: i64 = 30 * 24 * 60 * 60;
/// A lifecycle transaction may delete at most this many whole finalized rows.
pub const AUDIT_RETENTION_DELETE_BATCH_ITEMS: usize = 64;
/// A lifecycle or normalization transaction may delete at most this many logical row bytes.
pub const AUDIT_RETENTION_DELETE_BATCH_LOGICAL_BYTES: usize = 8 * 1024 * 1024;

/// Opaque signal that a caller-owned lifecycle transaction must roll back before legacy
/// retention is normalized in separate bounded transactions. The private field prevents other
/// crates from manufacturing the signal while still allowing an `anyhow::Error` downcast.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AuditRetentionNormalizationRequired {
    _private: (),
}

impl std::fmt::Debug for AuditRetentionNormalizationRequired {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AuditRetentionNormalizationRequired")
    }
}

impl std::fmt::Display for AuditRetentionNormalizationRequired {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("audit_retention_normalization_required")
    }
}

impl std::error::Error for AuditRetentionNormalizationRequired {}

const FINALIZED_PREDICATE: &str = "lifecycle IN ('succeeded', 'failed', 'unknown', 'denied')";

/// Index required by the bounded retention window. Building it is a one-time schema migration;
/// every steady-state retention call thereafter visits only the policy window plus one fixed
/// normalization batch and one sentinel row.
pub const MIGRATION_AUDIT_RETENTION: &str = "
CREATE INDEX idx_tool_audit_retention_finalized
    ON tool_audit_logs(COALESCE(completed_at, created_at) DESC, id DESC)
    WHERE lifecycle IN ('succeeded', 'failed', 'unknown', 'denied');
";

/// Explicit finalized-row retention policy. Construction rejects zero, overflow, and values above
/// the production ceilings so a downstream adapter cannot silently weaken the resource contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditRetentionPolicy {
    max_finalized_items: usize,
    max_logical_bytes: usize,
    max_age_seconds: i64,
}

impl AuditRetentionPolicy {
    pub const fn production() -> Self {
        Self {
            max_finalized_items: AUDIT_RETENTION_MAX_FINALIZED_ITEMS,
            max_logical_bytes: AUDIT_RETENTION_MAX_LOGICAL_BYTES,
            max_age_seconds: AUDIT_RETENTION_MAX_AGE_SECONDS,
        }
    }

    pub fn try_new(
        max_finalized_items: usize,
        max_logical_bytes: usize,
        max_age_seconds: i64,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            max_finalized_items > 0,
            "audit retention item limit must be positive"
        );
        anyhow::ensure!(
            max_finalized_items <= AUDIT_RETENTION_MAX_FINALIZED_ITEMS,
            "audit retention item limit exceeds production ceiling"
        );
        anyhow::ensure!(
            max_logical_bytes > 0,
            "audit retention byte limit must be positive"
        );
        anyhow::ensure!(
            max_logical_bytes <= AUDIT_RETENTION_MAX_LOGICAL_BYTES,
            "audit retention byte limit exceeds production ceiling"
        );
        anyhow::ensure!(max_age_seconds > 0, "audit retention age must be positive");
        anyhow::ensure!(
            max_age_seconds <= AUDIT_RETENTION_MAX_AGE_SECONDS,
            "audit retention age exceeds production ceiling"
        );
        Ok(Self {
            max_finalized_items,
            max_logical_bytes,
            max_age_seconds,
        })
    }

    pub const fn max_finalized_items(self) -> usize {
        self.max_finalized_items
    }

    pub const fn max_logical_bytes(self) -> usize {
        self.max_logical_bytes
    }

    pub const fn max_age_seconds(self) -> i64 {
        self.max_age_seconds
    }
}

impl Default for AuditRetentionPolicy {
    fn default() -> Self {
        Self::production()
    }
}

/// Exact result from a successful foreground retention pass. It intentionally contains no
/// operation, tool, server, URL, input, token, or raw error values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditPruneReport {
    deleted_finalized_items: usize,
    deleted_logical_bytes: usize,
    remaining_finalized_items: usize,
    remaining_logical_bytes: usize,
}

impl AuditPruneReport {
    pub const fn deleted_finalized_items(self) -> usize {
        self.deleted_finalized_items
    }

    pub const fn deleted_logical_bytes(self) -> usize {
        self.deleted_logical_bytes
    }

    pub const fn remaining_finalized_items(self) -> usize {
        self.remaining_finalized_items
    }

    pub const fn remaining_logical_bytes(self) -> usize {
        self.remaining_logical_bytes
    }
}

/// Progress from one explicit legacy-normalization transaction. `complete = false` means the
/// caller may schedule another bounded batch; it must not busy-loop or admit lifecycle writes yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditNormalizationReport {
    deleted_finalized_items: usize,
    deleted_logical_bytes: usize,
    complete: bool,
}

impl AuditNormalizationReport {
    pub const fn deleted_finalized_items(self) -> usize {
        self.deleted_finalized_items
    }

    pub const fn deleted_logical_bytes(self) -> usize {
        self.deleted_logical_bytes
    }

    pub const fn is_complete(self) -> bool {
        self.complete
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RetentionRow {
    row_id: i64,
    logical_bytes: usize,
    expired: bool,
}

struct RetentionWindow {
    rows: Vec<RetentionRow>,
    truncated: bool,
    total_logical_bytes: usize,
}

// Every variable-width persisted field is counted in bytes. CAST-to-BLOB is required because
// SQLite length(TEXT) counts Unicode scalar values rather than UTF-8 bytes.
const LOGICAL_BYTES_SQL: &str = "
    length(CAST(id AS BLOB))
  + COALESCE(length(CAST(workspace_id AS BLOB)), 0)
  + COALESCE(length(CAST(session_id AS BLOB)), 0)
  + COALESCE(length(CAST(server_id AS BLOB)), 0)
  + length(CAST(tool_name AS BLOB))
  + COALESCE(length(CAST(input_redacted_json AS BLOB)), 0)
  + COALESCE(length(input_encrypted_blob), 0)
  + length(CAST(decision AS BLOB))
  + length(CAST(created_at AS BLOB))
  + COALESCE(length(CAST(operation_id AS BLOB)), 0)
  + length(CAST(lifecycle AS BLOB))
  + COALESCE(length(CAST(outcome_error_code AS BLOB)), 0)
  + COALESCE(length(CAST(completed_at AS BLOB)), 0)
  + COALESCE(length(CAST(authorization_scope AS BLOB)), 0)
  + COALESCE(length(CAST(authorization_run_id AS BLOB)), 0)";

/// Enforces retention inside the same transaction as a lifecycle mutation. This function either
/// reaches the hard policy ceiling with one bounded delete or returns
/// [`AuditRetentionNormalizationRequired`]; callers must roll the whole transaction back on any
/// error. It never scans or deletes `Prepared` rows.
pub fn prune_audit_logs_in_transaction(
    conn: &Connection,
    policy: AuditRetentionPolicy,
    now_epoch_seconds: i64,
) -> anyhow::Result<AuditPruneReport> {
    require_transaction(conn)?;
    let window = load_retention_window(conn, policy, now_epoch_seconds)?;
    let candidates = retention_candidates(&window, policy)?;
    let (candidate_items, candidate_bytes) = candidate_totals(&candidates)?;
    if window.truncated
        || candidate_items > AUDIT_RETENTION_DELETE_BATCH_ITEMS
        || candidate_bytes > AUDIT_RETENTION_DELETE_BATCH_LOGICAL_BYTES
    {
        return Err(AuditRetentionNormalizationRequired { _private: () }.into());
    }

    delete_candidates(conn, &candidates)?;
    let remaining_items = window
        .rows
        .len()
        .checked_sub(candidate_items)
        .context("audit retention remaining item count underflow")?;
    let remaining_bytes = window
        .total_logical_bytes
        .checked_sub(candidate_bytes)
        .context("audit retention remaining byte count underflow")?;
    anyhow::ensure!(
        remaining_items <= policy.max_finalized_items,
        "audit retention item limit was not enforced"
    );
    anyhow::ensure!(
        remaining_bytes <= policy.max_logical_bytes,
        "audit retention byte limit was not enforced"
    );

    Ok(AuditPruneReport {
        deleted_finalized_items: candidate_items,
        deleted_logical_bytes: candidate_bytes,
        remaining_finalized_items: remaining_items,
        remaining_logical_bytes: remaining_bytes,
    })
}

/// Deletes one bounded batch from an oversized legacy database inside a caller-owned transaction.
/// A false `is_complete()` result is durable progress, not permission to commit a new lifecycle
/// mutation. Callers schedule another batch from an explicit startup/maintenance edge; no polling
/// worker is required or provided.
pub fn normalize_audit_retention_batch_in_transaction(
    conn: &Connection,
    policy: AuditRetentionPolicy,
    now_epoch_seconds: i64,
) -> anyhow::Result<AuditNormalizationReport> {
    require_transaction(conn)?;
    let window = load_retention_window(conn, policy, now_epoch_seconds)?;
    let candidates = retention_candidates(&window, policy)?;
    let mut selected = Vec::with_capacity(AUDIT_RETENTION_DELETE_BATCH_ITEMS);
    let mut selected_bytes = 0_usize;
    for candidate in &candidates {
        if selected.len() == AUDIT_RETENTION_DELETE_BATCH_ITEMS {
            break;
        }
        let projected_bytes = selected_bytes
            .checked_add(candidate.logical_bytes)
            .context("audit retention normalization byte count overflow")?;
        if projected_bytes > AUDIT_RETENTION_DELETE_BATCH_LOGICAL_BYTES {
            break;
        }
        selected.push(*candidate);
        selected_bytes = projected_bytes;
    }
    if selected.is_empty() && !candidates.is_empty() {
        anyhow::bail!("audit_retention_row_exceeds_normalization_byte_limit");
    }
    delete_candidates(conn, &selected)?;
    let complete = !window.truncated && selected.len() == candidates.len();
    Ok(AuditNormalizationReport {
        deleted_finalized_items: selected.len(),
        deleted_logical_bytes: selected_bytes,
        complete,
    })
}

fn require_transaction(conn: &Connection) -> anyhow::Result<()> {
    anyhow::ensure!(
        !conn.is_autocommit(),
        "audit retention requires a caller-owned transaction"
    );
    Ok(())
}

fn load_retention_window(
    conn: &Connection,
    policy: AuditRetentionPolicy,
    now_epoch_seconds: i64,
) -> anyhow::Result<RetentionWindow> {
    anyhow::ensure!(
        now_epoch_seconds >= 0,
        "audit retention clock must be at or after the Unix epoch"
    );
    let cutoff_epoch_seconds = now_epoch_seconds
        .checked_sub(policy.max_age_seconds)
        .context("audit retention cutoff underflow")?;
    let cutoff: String = conn
        .query_row(
            "SELECT strftime('%Y-%m-%dT%H:%M:%fZ', ?1, 'unixepoch')",
            [cutoff_epoch_seconds],
            |row| row.get(0),
        )
        .context("audit retention cutoff conversion failed")?;
    let scan_limit = policy
        .max_finalized_items
        .checked_add(AUDIT_RETENTION_DELETE_BATCH_ITEMS)
        .and_then(|value| value.checked_add(1))
        .context("audit retention scan limit overflow")?;
    let sql_scan_limit =
        i64::try_from(scan_limit).context("audit retention scan limit conversion failed")?;
    let select_sql = format!(
        "SELECT rowid, {LOGICAL_BYTES_SQL},
                COALESCE(completed_at, created_at) < ?1
         FROM tool_audit_logs INDEXED BY idx_tool_audit_retention_finalized
         WHERE {FINALIZED_PREDICATE}
         ORDER BY COALESCE(completed_at, created_at) DESC, id DESC
         LIMIT ?2"
    );
    let mut statement = conn
        .prepare(&select_sql)
        .context("audit retention bounded window prepare failed")?;
    let mut query = statement
        .query((&cutoff, sql_scan_limit))
        .context("audit retention bounded window query failed")?;
    let mut rows = Vec::with_capacity(scan_limit);
    let mut total_logical_bytes = 0_usize;
    while let Some(row) = query
        .next()
        .context("audit retention bounded window read failed")?
    {
        let logical_bytes = usize::try_from(
            row.get::<_, i64>(1)
                .context("audit retention row byte count read failed")?,
        )
        .context("audit retention row byte count conversion failed")?;
        total_logical_bytes = total_logical_bytes
            .checked_add(logical_bytes)
            .context("audit retention bounded window byte count overflow")?;
        rows.push(RetentionRow {
            row_id: row
                .get(0)
                .context("audit retention row identifier read failed")?,
            logical_bytes,
            expired: row.get(2).context("audit retention row age read failed")?,
        });
    }
    let truncated = rows.len() == scan_limit;
    if truncated {
        let sentinel = rows.pop().context("audit retention sentinel row missing")?;
        total_logical_bytes = total_logical_bytes
            .checked_sub(sentinel.logical_bytes)
            .context("audit retention sentinel byte count underflow")?;
    }
    Ok(RetentionWindow {
        rows,
        truncated,
        total_logical_bytes,
    })
}

fn retention_candidates(
    window: &RetentionWindow,
    policy: AuditRetentionPolicy,
) -> anyhow::Result<Vec<RetentionRow>> {
    let mut cumulative_bytes = 0_usize;
    let mut candidates = Vec::with_capacity(AUDIT_RETENTION_DELETE_BATCH_ITEMS);
    for (index, row) in window.rows.iter().enumerate() {
        cumulative_bytes = cumulative_bytes
            .checked_add(row.logical_bytes)
            .context("audit retention cumulative byte count overflow")?;
        let item_rank = index
            .checked_add(1)
            .context("audit retention item rank overflow")?;
        if row.expired
            || item_rank > policy.max_finalized_items
            || cumulative_bytes > policy.max_logical_bytes
        {
            candidates.push(*row);
        }
    }
    Ok(candidates)
}

fn candidate_totals(candidates: &[RetentionRow]) -> anyhow::Result<(usize, usize)> {
    let logical_bytes = candidates.iter().try_fold(0_usize, |total, candidate| {
        total
            .checked_add(candidate.logical_bytes)
            .context("audit retention candidate byte count overflow")
    })?;
    Ok((candidates.len(), logical_bytes))
}

fn delete_candidates(conn: &Connection, candidates: &[RetentionRow]) -> anyhow::Result<()> {
    if candidates.is_empty() {
        return Ok(());
    }
    anyhow::ensure!(
        candidates.len() <= AUDIT_RETENTION_DELETE_BATCH_ITEMS,
        "audit retention delete item batch exceeded"
    );
    let (_, logical_bytes) = candidate_totals(candidates)?;
    anyhow::ensure!(
        logical_bytes <= AUDIT_RETENTION_DELETE_BATCH_LOGICAL_BYTES,
        "audit retention delete byte batch exceeded"
    );
    let placeholders = std::iter::repeat_n("?", candidates.len())
        .collect::<Vec<_>>()
        .join(",");
    let delete_sql = format!(
        "DELETE FROM tool_audit_logs
         WHERE rowid IN ({placeholders})
           AND {FINALIZED_PREDICATE}"
    );
    let affected = conn
        .execute(
            &delete_sql,
            params_from_iter(candidates.iter().map(|candidate| candidate.row_id)),
        )
        .context("audit retention bounded delete failed")?;
    anyhow::ensure!(
        affected == candidates.len(),
        "audit retention candidate changed inside transaction"
    );
    Ok(())
}

#[cfg(test)]
pub(crate) fn prune_audit_logs_for_test(
    conn: &Connection,
    policy: AuditRetentionPolicy,
    now_epoch_seconds: i64,
) -> anyhow::Result<AuditPruneReport> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .context("audit retention test transaction start failed")?;
    let report = prune_audit_logs_in_transaction(&tx, policy, now_epoch_seconds)?;
    tx.commit()
        .context("audit retention test transaction commit failed")?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        MIGRATION_AUDIT_LIFECYCLE, MIGRATION_AUDIT_RETENTION, MIGRATION_AUTHORIZATION_OWNERS,
        MIGRATION_SQL,
    };
    use rusqlite::OptionalExtension as _;

    const NOW: i64 = 2_000_000_000;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATION_SQL).unwrap();
        conn.execute_batch(MIGRATION_AUDIT_LIFECYCLE).unwrap();
        conn.execute_batch(MIGRATION_AUTHORIZATION_OWNERS).unwrap();
        conn.execute_batch(MIGRATION_AUDIT_RETENTION).unwrap();
        conn
    }

    fn insert_row(
        conn: &Connection,
        id: &str,
        lifecycle: &str,
        completed_epoch_seconds: i64,
        payload: &str,
    ) {
        let completed_at = (lifecycle != "prepared").then_some(completed_epoch_seconds);
        conn.execute(
            "INSERT INTO tool_audit_logs (
                 id, operation_id, workspace_id, session_id, server_id, tool_name,
                 input_redacted_json, input_encrypted_blob, decision, created_at,
                 lifecycle, outcome_error_code, completed_at, authorization_scope,
                 authorization_run_id
             ) VALUES (
                 ?1, ?2, 'workspace', 'session', 'server', 'tool', ?3, NULL, 'allow_once',
                 strftime('%Y-%m-%dT%H:%M:%fZ', ?4, 'unixepoch'), ?5,
                 CASE WHEN ?5 = 'unknown' THEN 'delivery_unknown' END,
                 CASE WHEN ?6 IS NULL THEN NULL
                      ELSE strftime('%Y-%m-%dT%H:%M:%fZ', ?6, 'unixepoch') END,
                 'scope', 'run'
             )",
            rusqlite::params![
                id,
                format!("operation-{id}"),
                payload,
                completed_epoch_seconds - 1,
                lifecycle,
                completed_at
            ],
        )
        .unwrap();
    }

    fn row_exists(conn: &Connection, id: &str) -> bool {
        conn.query_row("SELECT 1 FROM tool_audit_logs WHERE id = ?1", [id], |row| {
            row.get::<_, i64>(0)
        })
        .optional()
        .unwrap()
        .is_some()
    }

    fn finalized_bytes(conn: &Connection) -> usize {
        usize::try_from(
            conn.query_row(
                &format!(
                    "SELECT COALESCE(SUM({LOGICAL_BYTES_SQL}), 0)
                     FROM tool_audit_logs WHERE {FINALIZED_PREDICATE}"
                ),
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn normalize_batch(
        conn: &Connection,
        policy: AuditRetentionPolicy,
    ) -> AuditNormalizationReport {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
        let report = normalize_audit_retention_batch_in_transaction(&tx, policy, NOW).unwrap();
        tx.commit().unwrap();
        report
    }

    #[test]
    fn item_limit_plus_one_prunes_oldest_deterministically() {
        let conn = test_conn();
        insert_row(&conn, "a-oldest", "succeeded", NOW - 3, "{}");
        insert_row(&conn, "b-middle", "failed", NOW - 2, "{}");
        insert_row(&conn, "c-newest", "denied", NOW - 1, "{}");

        let policy = AuditRetentionPolicy::try_new(
            2,
            AUDIT_RETENTION_MAX_LOGICAL_BYTES,
            AUDIT_RETENTION_MAX_AGE_SECONDS,
        )
        .unwrap();
        let report = prune_audit_logs_for_test(&conn, policy, NOW).unwrap();

        assert_eq!(report.deleted_finalized_items(), 1);
        assert_eq!(report.remaining_finalized_items(), 2);
        assert!(!row_exists(&conn, "a-oldest"));
        assert!(row_exists(&conn, "b-middle"));
        assert!(row_exists(&conn, "c-newest"));
    }

    #[test]
    fn logical_byte_limit_plus_one_prunes_whole_oldest_trace() {
        let conn = test_conn();
        insert_row(&conn, "old", "succeeded", NOW - 2, "old-payload");
        insert_row(&conn, "new", "unknown", NOW - 1, "new-payload");
        let total = finalized_bytes(&conn);
        let byte_limit = total - 1;
        let policy = AuditRetentionPolicy::try_new(
            AUDIT_RETENTION_MAX_FINALIZED_ITEMS,
            byte_limit,
            AUDIT_RETENTION_MAX_AGE_SECONDS,
        )
        .unwrap();

        let report = prune_audit_logs_for_test(&conn, policy, NOW).unwrap();

        assert_eq!(report.deleted_finalized_items(), 1);
        assert!(report.deleted_logical_bytes() > 0);
        assert!(report.remaining_logical_bytes() <= byte_limit);
        assert!(!row_exists(&conn, "old"));
        assert!(row_exists(&conn, "new"));
    }

    #[test]
    fn prepared_is_never_pruned_but_recovered_unknown_is_eligible() {
        let conn = test_conn();
        insert_row(&conn, "prepared-a", "prepared", 1, "old-and-large");
        insert_row(&conn, "prepared-b", "prepared", 1, "old-and-large");
        insert_row(&conn, "finalized", "succeeded", 1, "old");
        let policy = AuditRetentionPolicy::try_new(1, 1, 1).unwrap();

        prune_audit_logs_for_test(&conn, policy, NOW).unwrap();
        assert!(row_exists(&conn, "prepared-a"));
        assert!(row_exists(&conn, "prepared-b"));
        assert!(!row_exists(&conn, "finalized"));

        conn.execute(
            "UPDATE tool_audit_logs
             SET lifecycle = 'unknown', outcome_error_code = 'owner_superseded',
                 completed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 1, 'unixepoch')
             WHERE id = 'prepared-a'",
            [],
        )
        .unwrap();
        prune_audit_logs_for_test(&conn, policy, NOW).unwrap();
        assert!(!row_exists(&conn, "prepared-a"));
        assert!(row_exists(&conn, "prepared-b"));
    }

    #[test]
    fn age_limit_prunes_even_when_item_and_byte_limits_have_room() {
        let conn = test_conn();
        insert_row(
            &conn,
            "expired",
            "succeeded",
            NOW - AUDIT_RETENTION_MAX_AGE_SECONDS - 1,
            "{}",
        );
        insert_row(&conn, "current", "succeeded", NOW - 1, "{}");

        let report =
            prune_audit_logs_for_test(&conn, AuditRetentionPolicy::production(), NOW).unwrap();

        assert_eq!(report.deleted_finalized_items(), 1);
        assert!(!row_exists(&conn, "expired"));
        assert!(row_exists(&conn, "current"));
    }

    #[test]
    fn foreground_oversize_fails_closed_and_bounded_batches_make_progress() {
        let conn = test_conn();
        let policy = AuditRetentionPolicy::try_new(
            1,
            AUDIT_RETENTION_MAX_LOGICAL_BYTES,
            AUDIT_RETENTION_MAX_AGE_SECONDS,
        )
        .unwrap();
        for index in 0..=(AUDIT_RETENTION_DELETE_BATCH_ITEMS + 1) {
            insert_row(
                &conn,
                &format!("legacy-{index:03}"),
                "succeeded",
                NOW - i64::try_from(index).unwrap() - 1,
                "{}",
            );
        }

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        tx.execute(
            "INSERT INTO tool_audit_logs
                (id, tool_name, decision, created_at, lifecycle, completed_at)
             VALUES ('new-mutation', 'tool', 'allow_once',
                     strftime('%Y-%m-%dT%H:%M:%fZ', ?1, 'unixepoch'), 'succeeded',
                     strftime('%Y-%m-%dT%H:%M:%fZ', ?1, 'unixepoch'))",
            [NOW],
        )
        .unwrap();
        let error = prune_audit_logs_in_transaction(&tx, policy, NOW).unwrap_err();
        assert_eq!(error.to_string(), "audit_retention_normalization_required");
        assert!(
            error
                .downcast_ref::<AuditRetentionNormalizationRequired>()
                .is_some()
        );
        assert_eq!(
            format!(
                "{:?}",
                error
                    .downcast_ref::<AuditRetentionNormalizationRequired>()
                    .unwrap()
            ),
            "AuditRetentionNormalizationRequired"
        );
        drop(tx);
        assert!(!row_exists(&conn, "new-mutation"));

        let first = normalize_batch(&conn, policy);
        assert_eq!(
            first.deleted_finalized_items(),
            AUDIT_RETENTION_DELETE_BATCH_ITEMS
        );
        assert!(first.deleted_logical_bytes() <= AUDIT_RETENTION_DELETE_BATCH_LOGICAL_BYTES);
        assert!(!first.is_complete());
        let second = normalize_batch(&conn, policy);
        assert_eq!(second.deleted_finalized_items(), 1);
        assert!(second.is_complete());
        assert!(row_exists(&conn, "legacy-000"));
    }

    #[test]
    fn retention_window_uses_partial_ordering_index_without_temp_sort() {
        let conn = test_conn();
        let query = format!(
            "EXPLAIN QUERY PLAN
             SELECT rowid, {LOGICAL_BYTES_SQL}
             FROM tool_audit_logs INDEXED BY idx_tool_audit_retention_finalized
             WHERE {FINALIZED_PREDICATE}
             ORDER BY COALESCE(completed_at, created_at) DESC, id DESC
             LIMIT 4161"
        );
        let mut statement = conn.prepare(&query).unwrap();
        let details = statement
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n");
        assert!(
            details.contains("idx_tool_audit_retention_finalized"),
            "{details}"
        );
        assert!(!details.contains("TEMP B-TREE"), "{details}");
    }

    #[test]
    fn production_api_exposes_only_caller_owned_retention_operations() {
        let lib = include_str!("lib.rs");
        assert!(!lib.contains("AuditRetentionPolicy, prune_audit_logs,"));
        assert!(lib.contains("normalize_audit_retention_batch_in_transaction"));
        assert!(lib.contains("prune_audit_logs_in_transaction"));
        let source = include_str!("retention.rs");
        let test_helper = source
            .split_once("pub(crate) fn prune_audit_logs_for_test")
            .unwrap()
            .0;
        assert!(!test_helper.contains("pub fn prune_audit_logs("));
    }

    #[test]
    fn delete_failure_rolls_back_without_touching_prepared_or_finalized_rows() {
        let conn = test_conn();
        insert_row(&conn, "prepared", "prepared", 1, "{}");
        insert_row(&conn, "expired", "succeeded", 1, "{}");
        conn.execute_batch(
            "CREATE TRIGGER fail_audit_retention
             BEFORE DELETE ON tool_audit_logs
             BEGIN SELECT RAISE(ABORT, 'injected audit retention failure'); END;",
        )
        .unwrap();

        let result =
            prune_audit_logs_for_test(&conn, AuditRetentionPolicy::try_new(1, 1, 1).unwrap(), NOW);

        assert!(result.is_err());
        assert!(row_exists(&conn, "prepared"));
        assert!(row_exists(&conn, "expired"));
        assert!(conn.is_autocommit());
    }

    #[test]
    fn caller_owned_transaction_can_commit_transition_and_prune_atomically() {
        let mut conn = test_conn();
        insert_row(&conn, "prepared", "prepared", NOW - 2, "{}");
        insert_row(&conn, "old-finalized", "succeeded", 1, "{}");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        tx.execute(
            "UPDATE tool_audit_logs
             SET lifecycle = 'unknown', outcome_error_code = 'delivery_unknown',
                 completed_at = strftime('%Y-%m-%dT%H:%M:%fZ', ?1, 'unixepoch')
             WHERE id = 'prepared'",
            [NOW - 1],
        )
        .unwrap();

        let report = prune_audit_logs_in_transaction(
            &tx,
            AuditRetentionPolicy::try_new(1, AUDIT_RETENTION_MAX_LOGICAL_BYTES, 1).unwrap(),
            NOW,
        )
        .unwrap();
        assert_eq!(report.remaining_finalized_items(), 1);
        tx.commit().unwrap();

        assert!(row_exists(&conn, "prepared"));
        assert!(!row_exists(&conn, "old-finalized"));
    }

    #[test]
    fn policy_cannot_exceed_production_ceilings() {
        assert!(
            AuditRetentionPolicy::try_new(
                AUDIT_RETENTION_MAX_FINALIZED_ITEMS + 1,
                AUDIT_RETENTION_MAX_LOGICAL_BYTES,
                AUDIT_RETENTION_MAX_AGE_SECONDS,
            )
            .is_err()
        );
        assert!(
            AuditRetentionPolicy::try_new(
                AUDIT_RETENTION_MAX_FINALIZED_ITEMS,
                AUDIT_RETENTION_MAX_LOGICAL_BYTES + 1,
                AUDIT_RETENTION_MAX_AGE_SECONDS,
            )
            .is_err()
        );
        assert!(
            AuditRetentionPolicy::try_new(
                AUDIT_RETENTION_MAX_FINALIZED_ITEMS,
                AUDIT_RETENTION_MAX_LOGICAL_BYTES,
                AUDIT_RETENTION_MAX_AGE_SECONDS + 1,
            )
            .is_err()
        );
    }
}
