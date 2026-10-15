use crate::management::{
    CloudRun, MAX_RUN_BYTES, MAX_RUNS, OperationContext, RunPage, RunQuery, operation_key,
    validate_identity, validate_operation_id,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;

const PRUNE_ANSWERS_SQL: &str = "UPDATE operations SET message='' WHERE tool='notify' AND message<>'' AND id NOT IN (SELECT operation_id FROM answer_completion ORDER BY sequence DESC LIMIT 100)";

fn completion_sql_time(value: u64) -> anyhow::Result<i64> {
    i64::try_from(value).map_err(|_| anyhow::anyhow!("invalid_completion_receipt"))
}

pub struct History(Connection);
#[derive(Debug, PartialEq)]
pub enum Claim {
    New,
    Existing(Value),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub tool: String,
    pub workspace: String,
    pub session: String,
    pub created: i64,
    pub outcome: String,
    pub message: String,
    pub connection_id: Option<String>,
    pub run_id: Option<String>,
    pub generation: Option<String>,
    pub sequence: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryQuery {
    pub connection_id: String,
    pub workspace: Option<String>,
    pub session: Option<String>,
    pub generation: Option<String>,
    pub run_id: Option<String>,
    pub before: Option<i64>,
    pub limit: usize,
    pub max_bytes: usize,
}
impl Default for HistoryQuery {
    fn default() -> Self {
        Self {
            connection_id: "manual-legacy".into(),
            workspace: None,
            session: None,
            generation: None,
            run_id: None,
            before: None,
            limit: 100,
            max_bytes: 3 * 1024 * 1024,
        }
    }
}
impl HistoryQuery {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_identity(&self.connection_id)?;
        for value in [
            &self.workspace,
            &self.session,
            &self.generation,
            &self.run_id,
        ]
        .into_iter()
        .flatten()
        {
            validate_identity(value)?;
        }
        anyhow::ensure!(
            self.limit > 0
                && self.limit <= 100
                && self.max_bytes > 0
                && self.max_bytes <= 3 * 1024 * 1024
                && self.before.is_none_or(|c| c > 0),
            "invalid_history_query"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryPage {
    pub records: Vec<Record>,
    pub next_cursor: Option<i64>,
    pub truncated: bool,
}

// Validate sizes in SQL before SQLite materializes externally modified TEXT values.
const RECORD_COLUMNS: &str = "CASE WHEN length(CAST(COALESCE(c.external_id,o.id) AS BLOB))<=128 THEN COALESCE(c.external_id,o.id) END, CASE WHEN length(CAST(o.tool AS BLOB))<=64 THEN o.tool END, CASE WHEN length(CAST(o.workspace AS BLOB))<=128 THEN o.workspace END, CASE WHEN length(CAST(o.session AS BLOB))<=128 THEN o.session END, o.created, CASE WHEN length(CAST(o.outcome AS BLOB))<=2048 THEN o.outcome END, CASE WHEN length(CAST(o.message AS BLOB))<=16384 THEN o.message END, CASE WHEN length(CAST(COALESCE(c.connection_id,'manual-legacy') AS BLOB))<=128 THEN COALESCE(c.connection_id,'manual-legacy') END, CASE WHEN c.run_id IS NULL OR length(CAST(c.run_id AS BLOB))<=128 THEN c.run_id ELSE CAST(0 AS BLOB) END, CASE WHEN c.generation IS NULL OR length(CAST(c.generation AS BLOB))<=128 THEN c.generation ELSE CAST(0 AS BLOB) END, o.rowid";
fn record_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Record> {
    Ok(Record {
        id: r.get(0)?,
        tool: r.get(1)?,
        workspace: r.get(2)?,
        session: r.get(3)?,
        created: r.get(4)?,
        outcome: r.get(5)?,
        message: r.get(6)?,
        connection_id: Some(r.get(7)?),
        run_id: r.get(8)?,
        generation: r.get(9)?,
        sequence: r.get(10)?,
    })
}
impl History {
    pub fn claim_completion_scoped(
        &self,
        tool: &str,
        args: &Value,
        identity: &crate::CompletionIdentity,
    ) -> anyhow::Result<Claim> {
        identity.validate()?;
        let context = OperationContext {
            connection_id: identity.connection_id.clone(),
            operation_id: identity.scope.operation_id.clone(),
            run_id: identity.scope.run_id.clone(),
            generation: identity.scope.generation.clone(),
        };
        context.validate()?;
        let key = operation_key(&context.connection_id, &context.operation_id)?;
        self.claim_internal(
            &key,
            tool,
            args,
            &identity.scope.workspace,
            &identity.scope.session_id,
            Some(&context),
            Some(identity),
        )
    }
    pub fn finish_completion(
        &self,
        identity: &crate::CompletionIdentity,
        receipt: &crate::CompletionReceipt,
    ) -> anyhow::Result<()> {
        identity.validate()?;
        receipt.validate()?;
        anyhow::ensure!(
            identity == &receipt.identity,
            "completion_identity_mismatch"
        );
        anyhow::ensure!(
            receipt.expires_at <= crate::now().saturating_add(crate::COMPLETION_RECEIPT_TTL),
            "invalid_completion_receipt"
        );
        let key = operation_key(&identity.connection_id, &identity.scope.operation_id)?;
        let record = serde_json::to_string(receipt)?;
        anyhow::ensure!(record.len() <= 4096, "completion_result_too_large");
        let tx = self.0.unchecked_transaction()?;
        let row: Option<(String, Option<String>)> = tx.query_row(
            "SELECT CASE WHEN length(CAST(identity AS BLOB))<=2048 THEN identity END,CASE WHEN receipt IS NULL OR length(CAST(receipt AS BLOB))<=4096 THEN receipt ELSE CAST(0 AS BLOB) END FROM local_completion_receipts WHERE operation_key=?1",
            [&key], |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional()?;
        let (stored_identity, existing) =
            row.ok_or_else(|| anyhow::anyhow!("completion_not_claimed"))?;
        let stored_identity: crate::CompletionIdentity = serde_json::from_str(&stored_identity)?;
        stored_identity.validate()?;
        anyhow::ensure!(&stored_identity == identity, "completion_identity_mismatch");
        if let Some(existing) = existing {
            let existing: crate::CompletionReceipt = serde_json::from_str(&existing)?;
            existing.validate()?;
            anyhow::ensure!(existing == *receipt, "completion_receipt_conflict");
            return Ok(());
        }
        let outcome: String = tx.query_row("SELECT CASE WHEN length(CAST(outcome AS BLOB))<=2048 THEN outcome END FROM operations WHERE id=?1", [&key], |r| r.get(0))?;
        let outcome: Value = serde_json::from_str(&outcome)?;
        anyhow::ensure!(
            outcome == receipt.admission || outcome == json!({"status":"unknown","retry":false}),
            "completion_admission_conflict"
        );
        // The old get_operation path remains admission-only. Private stream metadata
        // lives exclusively in this ledger and is never copied into operations.
        let admission = serde_json::to_string(&receipt.admission)?;
        anyhow::ensure!(admission.len() <= 2048, "history_outcome_too_large");
        tx.execute(
            "UPDATE operations SET outcome=?2 WHERE id=?1",
            params![key, admission],
        )?;
        tx.execute("UPDATE local_completion_receipts SET receipt=?2,expires=?3 WHERE operation_key=?1 AND receipt IS NULL", params![key,record,completion_sql_time(receipt.expires_at)?])?;
        tx.commit()?;
        Ok(())
    }
    pub fn get_completion(
        &self,
        identity: &crate::CompletionIdentity,
    ) -> anyhow::Result<Option<crate::CompletionReceipt>> {
        identity.validate()?;
        let key = operation_key(&identity.connection_id, &identity.scope.operation_id)?;
        let row: Option<(String, Option<String>)> = self.0.query_row(
            "SELECT CASE WHEN length(CAST(identity AS BLOB))<=2048 THEN identity END,CASE WHEN receipt IS NULL OR length(CAST(receipt AS BLOB))<=4096 THEN receipt ELSE CAST(0 AS BLOB) END FROM local_completion_receipts WHERE operation_key=?1 AND expires>?2",
            params![key,completion_sql_time(crate::now())?], |r| Ok((r.get(0)?,r.get(1)?)),
        ).optional()?;
        let Some((stored_identity, record)) = row else {
            return Ok(None);
        };
        let stored_identity: crate::CompletionIdentity = serde_json::from_str(&stored_identity)?;
        stored_identity.validate()?;
        if stored_identity != *identity {
            return Ok(None);
        }
        record
            .map(|record| {
                let receipt: crate::CompletionReceipt = serde_json::from_str(&record)?;
                receipt.validate()?;
                anyhow::ensure!(
                    receipt.identity == *identity,
                    "completion_identity_mismatch"
                );
                Ok(receipt)
            })
            .transpose()
    }
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        // Do not follow a user-controlled symlink to an unrelated DB.
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            anyhow::ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                "history_file_type"
            );
        }
        // macOS /var is itself a symlink; resolve the trusted parent, while leaving
        // the final database component subject to SQLite's NOFOLLOW check.
        let file_name = path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("history_filename"))?;
        let resolved = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .canonicalize()?
            .join(file_name);
        let db = Self(Connection::open_with_flags(
            &resolved,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&resolved, std::fs::Permissions::from_mode(0o600))?;
        }
        db.init()?;
        Ok(db)
    }
    pub fn open_memory() -> anyhow::Result<Self> {
        let db = Self(Connection::open_in_memory()?);
        db.init()?;
        Ok(db)
    }
    fn init(&self) -> anyhow::Result<()> {
        self.0.busy_timeout(std::time::Duration::from_millis(100))?;
        self.0.execute_batch("PRAGMA journal_mode=DELETE; CREATE TABLE IF NOT EXISTS operations(id TEXT PRIMARY KEY, fingerprint BLOB NOT NULL, tool TEXT NOT NULL, workspace TEXT NOT NULL, session TEXT NOT NULL, created INTEGER NOT NULL, outcome TEXT NOT NULL, message TEXT NOT NULL DEFAULT '');")?;
        // Only retained answer bodies need pruning; input tombstones must remain
        // durable without making every completion scan the full operation history.
        self.0.execute_batch("CREATE INDEX IF NOT EXISTS operations_retained_answers ON operations(id) WHERE tool='notify' AND message<>'';")?;
        // A separate completion sequence also migrates existing answers without
        // changing operation fingerprints or at-most-once tombstones.
        self.0.execute_batch("CREATE TABLE IF NOT EXISTS answer_completion(sequence INTEGER PRIMARY KEY AUTOINCREMENT, operation_id TEXT NOT NULL UNIQUE); INSERT OR IGNORE INTO answer_completion(operation_id) SELECT id FROM operations WHERE tool='notify' AND message<>'' ORDER BY rowid; UPDATE operations SET message='' WHERE tool='notify' AND message<>'' AND id NOT IN (SELECT operation_id FROM answer_completion ORDER BY sequence DESC LIMIT 100);")?;
        self.0.execute_batch("CREATE TABLE IF NOT EXISTS operation_context(operation_key TEXT PRIMARY KEY,connection_id TEXT NOT NULL,external_id TEXT NOT NULL,run_id TEXT,generation TEXT NOT NULL,UNIQUE(connection_id,external_id)); CREATE INDEX IF NOT EXISTS operation_context_actor ON operation_context(connection_id,generation,operation_key); CREATE TABLE IF NOT EXISTS cloud_runs(connection_id TEXT NOT NULL,id TEXT NOT NULL,workspace TEXT NOT NULL,session TEXT NOT NULL,generation TEXT NOT NULL,state TEXT NOT NULL,record TEXT NOT NULL,PRIMARY KEY(connection_id,id)); CREATE INDEX IF NOT EXISTS cloud_runs_actor ON cloud_runs(connection_id);")?;
        self.0.execute_batch("CREATE TABLE IF NOT EXISTS local_completion_receipts(operation_key TEXT PRIMARY KEY,identity TEXT NOT NULL,created INTEGER NOT NULL,expires INTEGER NOT NULL,receipt TEXT); CREATE INDEX IF NOT EXISTS local_completion_expiry ON local_completion_receipts(expires) WHERE receipt IS NOT NULL;")?;
        self.restore_runs()?;
        self.restore_completions()?;
        Ok(())
    }
    pub fn claim(
        &self,
        id: &str,
        tool: &str,
        args: &Value,
        workspace: &str,
        session: &str,
    ) -> anyhow::Result<Claim> {
        validate_operation_id(id)?;
        self.claim_internal(id, tool, args, workspace, session, None, None)
    }
    pub fn claim_scoped(
        &self,
        id: &str,
        tool: &str,
        args: &Value,
        workspace: &str,
        session: &str,
        context: &OperationContext,
    ) -> anyhow::Result<Claim> {
        context.validate()?;
        anyhow::ensure!(id == context.operation_id, "operation_context_mismatch");
        let key = operation_key(&context.connection_id, id)?;
        self.claim_internal(&key, tool, args, workspace, session, Some(context), None)
    }
    #[allow(clippy::too_many_arguments)]
    fn claim_internal(
        &self,
        key: &str,
        tool: &str,
        args: &Value,
        workspace: &str,
        session: &str,
        context: Option<&OperationContext>,
        completion: Option<&crate::CompletionIdentity>,
    ) -> anyhow::Result<Claim> {
        anyhow::ensure!(
            tool.len() <= 64 && workspace.len() <= 128 && session.len() <= 128,
            "history_identity_too_large"
        );
        let encoded = serde_json::to_vec(&(tool, args))?;
        anyhow::ensure!(encoded.len() <= 65_600, "history_arguments_too_large");
        let hash = Sha256::digest(encoded).to_vec();
        let tx = self.0.unchecked_transaction()?;
        let row: Option<(Vec<u8>, String, String, String)> = tx
            .query_row(
                "SELECT CASE WHEN length(fingerprint)<=32 THEN fingerprint END, CASE WHEN length(CAST(outcome AS BLOB))<=2048 THEN outcome END, CASE WHEN length(CAST(workspace AS BLOB))<=128 THEN workspace END, CASE WHEN length(CAST(session AS BLOB))<=128 THEN session END FROM operations WHERE id=?1",
                [key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        if let Some((previous, outcome, previous_workspace, previous_session)) = row {
            anyhow::ensure!(previous == hash, "operation_id_conflict");
            anyhow::ensure!(outcome.len() <= 2048, "history_outcome_too_large");
            if let Some(context) = context {
                let old_context: Option<(String, String, Option<String>, String)> = tx.query_row("SELECT CASE WHEN length(CAST(connection_id AS BLOB))<=128 THEN connection_id END,CASE WHEN length(CAST(external_id AS BLOB))<=128 THEN external_id END,CASE WHEN run_id IS NULL OR length(CAST(run_id AS BLOB))<=128 THEN run_id ELSE CAST(0 AS BLOB) END,CASE WHEN length(CAST(generation AS BLOB))<=128 THEN generation END FROM operation_context WHERE operation_key=?1", [key], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
                if let Some((connection_id, external_id, run_id, generation)) = old_context {
                    anyhow::ensure!(
                        connection_id == context.connection_id
                            && external_id == context.operation_id
                            && run_id == context.run_id
                            && generation == context.generation
                            && previous_workspace == workspace
                            && previous_session == session,
                        "operation_context_conflict"
                    );
                } else {
                    anyhow::ensure!(
                        context.connection_id == "manual-legacy",
                        "operation_context_missing"
                    );
                }
            }
            if let Some(identity) = completion {
                let original: Option<String> = tx.query_row("SELECT CASE WHEN length(CAST(identity AS BLOB))<=2048 THEN identity END FROM local_completion_receipts WHERE operation_key=?1", [key], |r| r.get(0)).optional()?;
                let original = original.ok_or_else(|| {
                    anyhow::anyhow!("completion_operation_already_claimed_no_effect")
                })?;
                let original: crate::CompletionIdentity = serde_json::from_str(&original)?;
                original.validate()?;
                anyhow::ensure!(original == *identity, "completion_identity_mismatch");
            }
            return Ok(Claim::Existing(serde_json::from_str(&outcome)?));
        }
        if let Some(identity) = completion {
            tx.execute(
                "DELETE FROM local_completion_receipts WHERE receipt IS NOT NULL AND expires<=?1",
                [completion_sql_time(crate::now())?],
            )?;
            let (count, pending): (i64,i64) = tx.query_row("SELECT count(*),count(*) FILTER (WHERE receipt IS NULL) FROM local_completion_receipts", [], |r| Ok((r.get(0)?,r.get(1)?)))?;
            anyhow::ensure!(
                count < i64::try_from(crate::MAX_COMPLETION_RECEIPTS)?
                    && pending < i64::try_from(crate::MAX_COMPLETION_WATCHES)?,
                "completion_history_full_no_effect"
            );
            tx.execute("INSERT INTO local_completion_receipts(operation_key,identity,created,expires) VALUES(?1,?2,?3,?4)", params![key,serde_json::to_string(identity)?,completion_sql_time(crate::now())?,completion_sql_time(crate::now().saturating_add(3600))?])?;
        }
        let count: i64 = tx.query_row("SELECT count(*) FROM operations", [], |r| r.get(0))?;
        // Hard cap preserves at-most-once tombstones. Never evict a claim and later replay input.
        anyhow::ensure!(count < 100_000, "operation_history_full");
        tx.execute("INSERT INTO operations(id,fingerprint,tool,workspace,session,created,outcome) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![key,hash,tool,workspace,session,crate::now() as i64,json!({"status":"unknown","retry":false}).to_string()])?;
        if let Some(context) = context {
            tx.execute("INSERT INTO operation_context(operation_key,connection_id,external_id,run_id,generation) VALUES(?1,?2,?3,?4,?5)", params![key,context.connection_id,context.operation_id,context.run_id,context.generation])?;
        }
        tx.commit()?;
        Ok(Claim::New)
    }
    fn restore_completions(&self) -> anyhow::Result<()> {
        let mut statement = self.0.prepare("SELECT CASE WHEN length(CAST(identity AS BLOB))<=2048 THEN identity END,created FROM local_completion_receipts WHERE receipt IS NULL LIMIT 65")?;
        let pending = statement
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        anyhow::ensure!(
            pending.len() <= crate::MAX_COMPLETION_WATCHES,
            "completion_history_full_no_effect"
        );
        drop(statement);
        for (identity, created_at) in pending {
            let identity: crate::CompletionIdentity = serde_json::from_str(&identity)?;
            identity.validate()?;
            let admission = self
                .get_operation(&identity.scope.operation_id, &identity.connection_id)?
                .ok_or_else(|| anyhow::anyhow!("completion_not_claimed"))?;
            let receipt = crate::CompletionReceipt {
                event_id: identity.event_id(),
                identity: identity.clone(),
                admission,
                outcome: json!({"status":"unknown","reason":"server_restarted"}),
                created_at: u64::try_from(created_at)?,
                completed_at: None,
                expires_at: crate::now() + crate::COMPLETION_RECEIPT_TTL,
            };
            self.finish_completion(&identity, &receipt)?;
        }
        self.0.execute(
            "DELETE FROM local_completion_receipts WHERE receipt IS NOT NULL AND expires<=?1",
            [completion_sql_time(crate::now())?],
        )?;
        Ok(())
    }
    pub fn finish(&self, id: &str, outcome: &Value, message: &str) -> anyhow::Result<()> {
        validate_operation_id(id)?;
        self.finish_internal(id, outcome, message)
    }
    pub fn finish_scoped(
        &self,
        id: &str,
        connection_id: &str,
        outcome: &Value,
        message: &str,
    ) -> anyhow::Result<()> {
        let key = operation_key(connection_id, id)?;
        anyhow::ensure!(
            self.get_operation(id, connection_id)?.is_some(),
            "operation_not_claimed"
        );
        self.finish_internal(&key, outcome, message)
    }
    fn finish_internal(&self, id: &str, outcome: &Value, message: &str) -> anyhow::Result<()> {
        anyhow::ensure!(message.len() <= crate::MAX_ANSWER, "answer_too_large");
        anyhow::ensure!(
            outcome.to_string().len() <= 2048,
            "history_outcome_too_large"
        );
        let tx = self.0.unchecked_transaction()?;
        anyhow::ensure!(
            tx.execute(
                "UPDATE operations SET outcome=?2,message=?3 WHERE id=?1",
                params![id, outcome.to_string(), message]
            )? == 1,
            "operation_not_claimed"
        );
        if !message.is_empty() {
            tx.execute("INSERT OR IGNORE INTO answer_completion(operation_id) SELECT id FROM operations WHERE id=?1 AND tool='notify'", [id])?;
        }
        // Notifications retain at most 100 items. Input audit churn must never
        // invalidate a still-visible answer link. Preserve answer bodies on their
        // own horizon while keeping all bounded idempotency tombstones.
        tx.execute(PRUNE_ANSWERS_SQL, [])?;
        tx.commit()?;
        Ok(())
    }
    pub fn recent_page(&self, max_bytes: usize) -> anyhow::Result<HistoryPage> {
        anyhow::ensure!(
            max_bytes > 0 && max_bytes <= 3 * 1024 * 1024,
            "invalid_history_budget"
        );
        let mut stmt = self.0.prepare(&format!("SELECT {RECORD_COLUMNS} FROM operations o LEFT JOIN operation_context c ON c.operation_key=o.id WHERE o.rowid IN (SELECT rowid FROM operations ORDER BY rowid DESC LIMIT 500) OR o.id IN (SELECT operation_id FROM answer_completion ORDER BY sequence DESC LIMIT 100) ORDER BY (SELECT sequence FROM answer_completion WHERE operation_id=o.id) DESC, o.rowid DESC"))?;
        let mut rows = stmt.query([])?;
        let mut records = Vec::new();
        let mut bytes = 0usize;
        let mut truncated = false;
        while let Some(row) = rows.next()? {
            let record = record_row(row)?;
            // This is a retained Rust snapshot, not serialized JSON. Escaped
            // newlines must not double the charged answer-body allocation.
            let charge = std::mem::size_of::<Record>()
                + record.id.len()
                + record.tool.len()
                + record.workspace.len()
                + record.session.len()
                + record.outcome.len()
                + record.message.len()
                + record.connection_id.as_ref().map_or(0, String::len)
                + record.run_id.as_ref().map_or(0, String::len)
                + record.generation.as_ref().map_or(0, String::len);
            if bytes.saturating_add(charge) > max_bytes {
                truncated = true;
                break;
            }
            bytes += charge;
            records.push(record);
        }
        records.shrink_to_fit();
        let next_cursor = if truncated {
            records.last().map(|r| r.sequence)
        } else {
            None
        };
        Ok(HistoryPage {
            records,
            next_cursor,
            truncated,
        })
    }
    pub fn recent(&self) -> anyhow::Result<Vec<Record>> {
        self.recent_bounded(3 * 1024 * 1024)
    }
    /// Retained audit/answer rows are bounded before SQLite materializes their TEXT bodies.
    /// Oversized externally modified rows fail closed rather than allocating their contents.
    pub fn recent_bounded(&self, max_bytes: usize) -> anyhow::Result<Vec<Record>> {
        Ok(self.recent_page(max_bytes)?.records)
    }
    /// Read-only receipt lookup. A missing/wrong actor is indistinguishable from an unknown ID.
    pub fn get_operation(&self, id: &str, connection_id: &str) -> anyhow::Result<Option<Value>> {
        let key = operation_key(connection_id, id)?;
        let outcome: Option<String> = self.0.query_row("SELECT CASE WHEN length(CAST(o.outcome AS BLOB))<=2048 THEN o.outcome END FROM operations o LEFT JOIN operation_context c ON c.operation_key=o.id WHERE o.id=?1 AND (c.connection_id=?2 AND c.external_id=?3 OR c.operation_key IS NULL AND ?2='manual-legacy')", params![key,connection_id,id], |r| r.get(0)).optional()?;
        outcome
            .map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }
    pub fn get_operation_for_target(
        &self,
        id: &str,
        connection_id: &str,
        workspace: &str,
        session: &str,
        generation: &str,
    ) -> anyhow::Result<Option<Value>> {
        let key = operation_key(connection_id, id)?;
        for value in [workspace, session, generation] {
            validate_identity(value)?;
        }
        let outcome: Option<String> = self.0.query_row("SELECT CASE WHEN length(CAST(o.outcome AS BLOB))<=2048 THEN o.outcome END FROM operations o LEFT JOIN operation_context c ON c.operation_key=o.id WHERE o.id=?1 AND o.workspace=?4 AND o.session=?5 AND (c.connection_id=?2 AND c.external_id=?3 AND c.generation=?6 OR c.operation_key IS NULL AND ?2='manual-legacy')", params![key,connection_id,id,workspace,session,generation], |r| r.get(0)).optional()?;
        outcome
            .map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }
    pub fn query(&self, query: &HistoryQuery) -> anyhow::Result<HistoryPage> {
        query.validate()?;
        let mut stmt = self.0.prepare(&format!("SELECT {RECORD_COLUMNS} FROM operations o LEFT JOIN operation_context c ON c.operation_key=o.id WHERE COALESCE(c.connection_id,'manual-legacy')=?1 AND (?2 IS NULL OR o.workspace=?2) AND (?3 IS NULL OR o.session=?3) AND (?4 IS NULL OR c.generation=?4) AND (?5 IS NULL OR c.run_id=?5) AND (?6 IS NULL OR o.rowid<?6) ORDER BY o.rowid DESC LIMIT ?7"))?;
        let mut rows = stmt.query(params![
            query.connection_id,
            query.workspace,
            query.session,
            query.generation,
            query.run_id,
            query.before,
            query.limit as i64 + 1
        ])?;
        let mut records = vec![];
        let mut bytes = 0usize;
        let mut truncated = false;
        while let Some(row) = rows.next()? {
            let record = record_row(row)?;
            let charge = serde_json::to_vec(&record)?.len();
            if records.len() == query.limit || bytes.saturating_add(charge) > query.max_bytes {
                truncated = true;
                break;
            }
            bytes += charge;
            records.push(record);
        }
        let next_cursor = if truncated {
            records.last().map(|r| r.sequence)
        } else {
            None
        };
        Ok(HistoryPage {
            records,
            next_cursor,
            truncated,
        })
    }
    pub fn save_run(&self, run: &CloudRun) -> anyhow::Result<()> {
        run.validate()?;
        let record = serde_json::to_string(run)?;
        let tx = self.0.unchecked_transaction()?;
        let old: Option<String> = tx.query_row("SELECT CASE WHEN length(CAST(record AS BLOB))<=32768 THEN record END FROM cloud_runs WHERE connection_id=?1 AND id=?2", params![run.connection_id,run.id], |r| r.get(0)).optional()?;
        if let Some(old) = old {
            let old: CloudRun = serde_json::from_str(&old)?;
            old.validate_successor(run)?;
        } else {
            anyhow::ensure!(
                run.revision == 0 && run.state == crate::management::RunState::Queued,
                "run_not_started"
            );
            run.ensure_recovery_headroom()?;
            let count: i64 = tx.query_row("SELECT count(*) FROM cloud_runs", [], |r| r.get(0))?;
            // Never discard unfinished work or overwrite another actor's record.
            anyhow::ensure!(count < MAX_RUNS as i64, "run_history_full");
        }
        tx.execute("INSERT INTO cloud_runs(connection_id,id,workspace,session,generation,state,record) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(connection_id,id) DO UPDATE SET state=excluded.state,record=excluded.record", params![run.connection_id,run.id,run.workspace,run.session,run.generation,serde_json::to_string(&run.state)?,record])?;
        tx.commit()?;
        Ok(())
    }
    pub fn get_run(&self, connection_id: &str, id: &str) -> anyhow::Result<Option<CloudRun>> {
        validate_identity(connection_id)?;
        validate_identity(id)?;
        let record: Option<String> = self.0.query_row("SELECT CASE WHEN length(CAST(record AS BLOB))<=32768 THEN record END FROM cloud_runs WHERE connection_id=?1 AND id=?2", params![connection_id,id], |r| r.get(0)).optional()?;
        record
            .map(|record| {
                let run: CloudRun = serde_json::from_str(&record)?;
                run.validate()?;
                anyhow::ensure!(
                    run.connection_id == connection_id && run.id == id,
                    "run_lookup_mismatch"
                );
                Ok(run)
            })
            .transpose()
    }
    pub fn list_runs(&self, query: &RunQuery) -> anyhow::Result<RunPage> {
        query.validate()?;
        let state = query.state.map(|s| serde_json::to_string(&s)).transpose()?;
        let mut stmt = self.0.prepare("SELECT rowid,CASE WHEN length(CAST(record AS BLOB))<=32768 THEN record END FROM cloud_runs WHERE (?1 IS NULL OR connection_id=?1) AND (?2 IS NULL OR workspace=?2) AND (?3 IS NULL OR session=?3) AND (?4 IS NULL OR generation=?4) AND (?5 IS NULL OR state=?5) AND (?6 IS NULL OR rowid<?6) ORDER BY rowid DESC LIMIT ?7")?;
        let mut rows = stmt.query(params![
            query.connection_id,
            query.workspace,
            query.session,
            query.generation,
            state,
            query.before,
            query.limit as i64 + 1
        ])?;
        let mut runs = vec![];
        let mut bytes = 0usize;
        let mut truncated = false;
        let mut cursor = None;
        while let Some(row) = rows.next()? {
            let sequence: i64 = row.get(0)?;
            let record: String = row.get(1)?;
            anyhow::ensure!(record.len() <= MAX_RUN_BYTES, "run_too_large");
            if runs.len() == query.limit || bytes.saturating_add(record.len()) > query.max_bytes {
                truncated = true;
                break;
            }
            let run: CloudRun = serde_json::from_str(&record)?;
            run.validate()?;
            // The payload must agree with the actor filters even after an external DB edit.
            anyhow::ensure!(
                query
                    .connection_id
                    .as_ref()
                    .is_none_or(|id| *id == run.connection_id)
                    && query
                        .workspace
                        .as_ref()
                        .is_none_or(|id| *id == run.workspace)
                    && query.session.as_ref().is_none_or(|id| *id == run.session)
                    && query
                        .generation
                        .as_ref()
                        .is_none_or(|id| *id == run.generation)
                    && query.state.is_none_or(|state| state == run.state),
                "run_filter_mismatch"
            );
            bytes += record.len();
            cursor = Some(sequence);
            runs.push(run);
        }
        Ok(RunPage {
            runs,
            next_cursor: if truncated { cursor } else { None },
            truncated,
        })
    }
    fn restore_runs(&self) -> anyhow::Result<()> {
        let tx = self.0.unchecked_transaction()?;
        let count: i64 = tx.query_row("SELECT count(*) FROM cloud_runs", [], |r| r.get(0))?;
        anyhow::ensure!(count <= MAX_RUNS as i64, "run_history_full");
        let mut stmt = tx.prepare("SELECT rowid,CASE WHEN length(CAST(record AS BLOB))<=32768 THEN record END FROM cloud_runs")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let sequence: i64 = row.get(0)?;
            let record: String = row.get(1)?;
            let mut run: CloudRun = serde_json::from_str(&record)?;
            run.validate()?;
            let old_revision = run.revision;
            run.restore(crate::now() as i64)?;
            if run.revision != old_revision {
                tx.execute(
                    "UPDATE cloud_runs SET state=?2,record=?3 WHERE rowid=?1",
                    params![
                        sequence,
                        serde_json::to_string(&run.state)?,
                        serde_json::to_string(&run)?
                    ],
                )?;
            }
        }
        drop(rows);
        drop(stmt);
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod management_tests {
    use super::*;
    use crate::{CloudRun, OperationContext, RunQuery, RunState, RunUpdate};

    fn context(actor: &str, id: &str) -> OperationContext {
        OperationContext {
            connection_id: actor.into(),
            operation_id: id.into(),
            run_id: Some("run-1".into()),
            generation: "generation-1".into(),
        }
    }
    #[test]
    fn same_external_id_is_independent_and_receipts_are_connection_scoped() {
        let db = History::open_memory().unwrap();
        let args = json!({"text":"one"});
        for actor in ["actor-a", "actor-b"] {
            assert_eq!(
                db.claim_scoped(
                    "same-id",
                    "send_text",
                    &args,
                    "w",
                    "s",
                    &context(actor, "same-id")
                )
                .unwrap(),
                Claim::New
            );
        }
        db.finish_scoped(
            "same-id",
            "actor-a",
            &json!({"status":"admitted","actor":"a"}),
            "",
        )
        .unwrap();
        assert_eq!(
            db.get_operation("same-id", "actor-a").unwrap(),
            Some(json!({"status":"admitted","actor":"a"}))
        );
        assert_eq!(
            db.get_operation("same-id", "actor-b").unwrap(),
            Some(json!({"status":"unknown","retry":false}))
        );
        assert!(db.get_operation("same-id", "actor-c").unwrap().is_none());
        assert!(
            db.finish_scoped("same-id", "actor-c", &json!({"status":"admitted"}), "")
                .is_err()
        );
        assert_eq!(
            db.0.query_row("SELECT count(*) FROM operations", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn targeted_receipt_lookup_cannot_borrow_another_session_or_generation_permission() {
        let db = History::open_memory().unwrap();
        db.claim_scoped(
            "receipt",
            "send_text",
            &json!({"text":"once"}),
            "w",
            "private-pane",
            &context("actor-a", "receipt"),
        )
        .unwrap();
        db.finish_scoped("receipt", "actor-a", &json!({"status":"queued"}), "")
            .unwrap();
        assert!(
            db.get_operation_for_target("receipt", "actor-a", "w", "other-pane", "generation-1")
                .unwrap()
                .is_none(),
            "an allowed pane cannot expose a receipt from the original unshared pane"
        );
        assert!(
            db.get_operation_for_target("receipt", "actor-a", "w", "private-pane", "generation-2")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.get_operation_for_target("receipt", "actor-a", "w", "private-pane", "generation-1")
                .unwrap(),
            Some(json!({"status":"queued"}))
        );
        db.claim(
            "legacy",
            "send_text",
            &json!({"text":"legacy"}),
            "w",
            "legacy-pane",
        )
        .unwrap();
        assert!(
            db.get_operation_for_target("legacy", "manual-legacy", "w", "other-pane", "g")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.get_operation_for_target("legacy", "manual-legacy", "w", "legacy-pane", "g")
                .unwrap(),
            Some(json!({"status":"unknown","retry":false}))
        );
    }

    #[test]
    fn cloud_review_valid_newline_answers_fit_retained_memory_snapshot_budget() {
        let db = History::open_memory().unwrap();
        let message = "\n".repeat(crate::MAX_ANSWER);
        for index in 0..100 {
            let id = format!("answer-{index}");
            db.claim(&id, "notify", &json!({}), "w", "s").unwrap();
            db.finish(&id, &json!({"status":"stored"}), &message)
                .unwrap();
        }
        let snapshot = db.recent_bounded(3 * 1024 * 1024);
        assert!(
            snapshot.is_ok(),
            "permitted answer bodies must not make the whole snapshot unavailable through JSON escaping"
        );
        assert_eq!(snapshot.unwrap().len(), 100);
    }
    #[test]
    fn cloud_review_small_snapshot_returns_a_page_with_explicit_truncation() {
        let db = History::open_memory().unwrap();
        for index in 0..3 {
            let id = format!("answer-{index}");
            db.claim(&id, "notify", &json!({}), "w", "s").unwrap();
            db.finish(&id, &json!({"status":"stored"}), &"x".repeat(1024))
                .unwrap();
        }
        let page = db.recent_page(1800).unwrap();
        assert_eq!(page.records.len(), 1);
        assert!(page.truncated);
        assert!(page.next_cursor.is_some());
    }
    #[test]
    fn legacy_scoped_claim_obeys_existing_tombstone_and_new_scope_binds_target() {
        let db = History::open_memory().unwrap();
        let args = json!({"text":"one"});
        db.claim("legacy", "send_text", &args, "w", "s").unwrap();
        assert_eq!(
            db.claim_scoped(
                "legacy",
                "send_text",
                &args,
                "w",
                "s",
                &context("manual-legacy", "legacy")
            )
            .unwrap(),
            Claim::Existing(json!({"status":"unknown","retry":false}))
        );
        assert!(db.get_operation("legacy", "actor-a").unwrap().is_none());
        assert_eq!(
            db.claim_scoped(
                "new",
                "send_text",
                &args,
                "w",
                "s",
                &context("actor-a", "new")
            )
            .unwrap(),
            Claim::New
        );
        assert!(
            db.claim_scoped(
                "new",
                "send_text",
                &args,
                "w",
                "other",
                &context("actor-a", "new")
            )
            .is_err()
        );
        let mut changed = context("actor-a", "new");
        changed.generation = "new-generation".into();
        assert!(
            db.claim_scoped("new", "send_text", &args, "w", "s", &changed)
                .is_err()
        );
        assert!(
            db.claim_scoped(
                "new",
                "send_text",
                &args,
                "w",
                "s",
                &context("actor-a", "different")
            )
            .is_err()
        );
    }
    #[test]
    fn history_pages_filter_actor_and_generation_with_bounded_cursor() {
        let db = History::open_memory().unwrap();
        for index in 0..4 {
            let id = format!("op-{index}");
            db.claim_scoped(
                &id,
                "notify",
                &json!({"message":"answer"}),
                "w",
                "s",
                &context("actor-a", &id),
            )
            .unwrap();
            db.finish_scoped(&id, "actor-a", &json!({"status":"stored"}), "answer")
                .unwrap();
        }
        db.claim_scoped(
            "other",
            "notify",
            &json!({}),
            "w",
            "s",
            &context("actor-b", "other"),
        )
        .unwrap();
        let query = HistoryQuery {
            connection_id: "actor-a".into(),
            generation: Some("generation-1".into()),
            limit: 2,
            ..HistoryQuery::default()
        };
        let page = db.query(&query).unwrap();
        assert_eq!(page.records.len(), 2);
        assert!(page.truncated);
        assert!(
            page.records
                .iter()
                .all(|r| r.connection_id.as_deref() == Some("actor-a")
                    && r.run_id.as_deref() == Some("run-1")
                    && r.generation.as_deref() == Some("generation-1"))
        );
        let next = db
            .query(&HistoryQuery {
                before: page.next_cursor,
                ..query.clone()
            })
            .unwrap();
        assert_eq!(next.records.len(), 2);
        assert!(!next.truncated);
        assert_ne!(page.records[0].id, next.records[0].id);
        let small = db
            .query(&HistoryQuery {
                max_bytes: 1,
                ..query.clone()
            })
            .unwrap();
        assert!(small.records.is_empty() && small.truncated);
        assert!(
            db.query(&HistoryQuery {
                limit: 101,
                ..query
            })
            .is_err()
        );
    }
    #[test]
    fn durable_runs_reject_late_saves_and_restore_unfinished_as_unknown() {
        let path = std::env::temp_dir().join(format!(
            "deppy-cloud-run-test-{}.sqlite",
            uuid::Uuid::new_v4()
        ));
        let mut run = CloudRun::new(
            "run",
            "actor-a",
            "A",
            "w",
            "s",
            "g",
            "title",
            "instruction",
            100,
        )
        .unwrap();
        {
            let db = History::open(&path).unwrap();
            db.save_run(&run).unwrap();
            run.apply_report(
                &run.actor(),
                RunUpdate {
                    state: RunState::Running,
                    progress: Some(10),
                    message: "working".into(),
                    result: None,
                    artifacts: vec![],
                },
                101,
            )
            .unwrap();
            db.save_run(&run).unwrap();
            let mut older = run.clone();
            older.revision = 0;
            assert!(db.save_run(&older).is_err());
            let mut other = run.clone();
            other.connection_id = "actor-b".into();
            assert!(db.save_run(&other).is_err());
        }
        let db = History::open(&path).unwrap();
        let restored = db
            .list_runs(&RunQuery {
                connection_id: Some("actor-a".into()),
                ..RunQuery::default()
            })
            .unwrap();
        assert_eq!(restored.runs.len(), 1);
        assert_eq!(restored.runs[0].state, RunState::Unknown);
        assert!(restored.runs[0].revision > run.revision);
        assert!(db.save_run(&run).is_err());
        assert!(
            db.list_runs(&RunQuery {
                connection_id: Some("actor-b".into()),
                ..RunQuery::default()
            })
            .unwrap()
            .runs
            .is_empty()
        );
        drop(db);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn query_rejects_oversized_external_rows_before_materialization() {
        let db = History::open_memory().unwrap();
        db.claim_scoped(
            "row",
            "notify",
            &json!({}),
            "w",
            "s",
            &context("actor-a", "row"),
        )
        .unwrap();
        db.0.execute("UPDATE operations SET message=zeroblob(1000000)", [])
            .unwrap();
        assert!(
            db.query(&HistoryQuery {
                connection_id: "actor-a".into(),
                ..HistoryQuery::default()
            })
            .is_err()
        );
    }

    #[test]
    fn durable_run_pages_support_reused_ids_and_do_not_skip_byte_limited_rows() {
        let db = History::open_memory().unwrap();
        for actor in ["actor-a", "actor-b"] {
            for id in ["same-id", "other-id"] {
                db.save_run(
                    &CloudRun::new(
                        id,
                        actor,
                        "Agent",
                        "w",
                        "s",
                        "g",
                        "title",
                        "instruction",
                        100,
                    )
                    .unwrap(),
                )
                .unwrap();
            }
        }
        let query = RunQuery {
            connection_id: Some("actor-a".into()),
            limit: 1,
            ..RunQuery::default()
        };
        let page = db.list_runs(&query).unwrap();
        assert_eq!(page.runs.len(), 1);
        assert!(page.truncated);
        let next = db
            .list_runs(&RunQuery {
                before: page.next_cursor,
                ..query.clone()
            })
            .unwrap();
        assert_eq!(next.runs.len(), 1);
        assert!(!next.truncated);
        assert_ne!(page.runs[0].id, next.runs[0].id);
        assert_eq!(
            db.list_runs(&RunQuery {
                connection_id: Some("actor-b".into()),
                ..RunQuery::default()
            })
            .unwrap()
            .runs
            .len(),
            2
        );
        let small = db
            .list_runs(&RunQuery {
                max_bytes: 1,
                ..query
            })
            .unwrap();
        assert!(small.runs.is_empty() && small.truncated && small.next_cursor.is_none());
    }

    #[test]
    fn exact_actor_scoped_get_run_retrieves_beyond_first_page() {
        let db = History::open_memory().unwrap();
        for index in 0..101 {
            db.save_run(
                &CloudRun::new(
                    &format!("run-{index}"),
                    "actor-a",
                    "Agent A",
                    "w",
                    "s",
                    "g",
                    "title",
                    "instruction",
                    100,
                )
                .unwrap(),
            )
            .unwrap();
        }
        db.save_run(
            &CloudRun::new(
                "run-0",
                "actor-b",
                "Agent B",
                "w",
                "s",
                "g",
                "other",
                "instruction",
                100,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            !db.list_runs(&RunQuery {
                connection_id: Some("actor-a".into()),
                ..RunQuery::default()
            })
            .unwrap()
            .runs
            .iter()
            .any(|run| run.id == "run-0")
        );
        let original = db.get_run("actor-a", "run-0").unwrap();
        assert!(
            original.is_some(),
            "exact lookup must find an older persisted run outside the bounded first page"
        );
        assert_eq!(original.unwrap().connection_name, "Agent A");
        assert_eq!(
            db.get_run("actor-b", "run-0").unwrap().unwrap().title,
            "other"
        );
        assert!(db.get_run("actor-c", "run-0").unwrap().is_none());
        db.0.execute("UPDATE cloud_runs SET record=zeroblob(1000000) WHERE connection_id='actor-a' AND id='run-0'", []).unwrap();
        assert!(db.get_run("actor-a", "run-0").is_err());
    }

    #[test]
    fn durable_save_requires_existing_cancel_request_and_terminal_result_is_immutable() {
        let db = History::open_memory().unwrap();
        let mut run = CloudRun::new(
            "run",
            "actor-a",
            "A",
            "w",
            "s",
            "g",
            "title",
            "instruction",
            100,
        )
        .unwrap();
        db.save_run(&run).unwrap();
        let mut forged = run.clone();
        forged.state = RunState::Cancelled;
        forged.cancel_requested_at = Some(101);
        forged.cancel_acknowledged_at = Some(101);
        forged.updated = 101;
        forged.heartbeat = 101;
        forged.revision = 1;
        assert!(db.save_run(&forged).is_err());
        run.request_cancel(101).unwrap();
        db.save_run(&run).unwrap();
        run.acknowledge_cancel(&run.actor(), "stopped", 102)
            .unwrap();
        db.save_run(&run).unwrap();
        let mut late = run.clone();
        late.state = RunState::Completed;
        late.cancel_acknowledged_at = None;
        late.updated = 103;
        late.heartbeat = 103;
        late.revision += 1;
        assert!(db.save_run(&late).is_err());
        db.save_run(&run).unwrap();
        assert_eq!(
            db.list_runs(&RunQuery::default()).unwrap().runs[0].state,
            RunState::Cancelled
        );
    }
}

#[cfg(test)]
mod efficiency_tests {
    use super::*;
    use rusqlite::StatementStatus;
    use std::time::Instant;

    #[test]
    fn cloud_management_migration_adds_durable_tables_without_rewriting_claims() {
        let db = History::open_memory().unwrap();
        let args = json!({"text":"once"});
        assert_eq!(
            db.claim("old-input", "send_text", &args, "w", "s").unwrap(),
            Claim::New
        );
        let before: Vec<u8> =
            db.0.query_row(
                "SELECT fingerprint FROM operations WHERE id='old-input'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        db.init().unwrap();
        let tables: i64 = db.0.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('operation_context','cloud_runs')", [], |r| r.get(0)).unwrap();
        assert_eq!(
            tables, 2,
            "migration must add scoped attribution and durable run storage"
        );
        let after: Vec<u8> =
            db.0.query_row(
                "SELECT fingerprint FROM operations WHERE id='old-input'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(before, after);
        assert_eq!(
            db.claim("old-input", "send_text", &args, "w", "s").unwrap(),
            Claim::Existing(json!({"status":"unknown","retry":false}))
        );
    }

    fn populated(input_count: i64) -> History {
        let db = History::open_memory().unwrap();
        db.0.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<?1) INSERT INTO operations(id,fingerprint,tool,workspace,session,created,outcome,message) SELECT 'i'||x,X'00','send_text','w','s',0,'{}','' FROM n",[input_count]).unwrap();
        for index in 0..100 {
            let id = format!("a{index}");
            db.0.execute(
                "INSERT INTO operations VALUES(?1,X'00','notify','w','s',0,'{}','answer')",
                [&id],
            )
            .unwrap();
            db.0.execute(
                "INSERT INTO answer_completion(operation_id) VALUES(?1)",
                [&id],
            )
            .unwrap();
        }
        db
    }
    fn cleanup_steps(db: &History) -> i32 {
        let mut stmt = db.0.prepare(PRUNE_ANSWERS_SQL).unwrap();
        stmt.execute([]).unwrap();
        stmt.get_status(StatementStatus::VmStep)
    }
    #[test]
    fn answer_cleanup_work_does_not_scale_with_input_tombstones() {
        let small = populated(1000);
        let large = populated(99000);
        let a = cleanup_steps(&small);
        let b = cleanup_steps(&large);
        assert!(
            b <= a + 5000,
            "input tombstones changed cleanup work: {a} -> {b} VM steps"
        );
        assert_eq!(
            large
                .0
                .query_row("SELECT count(*) FROM operations", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            99100
        );
        assert_eq!(
            large
                .0
                .query_row(
                    "SELECT count(*) FROM operations WHERE message<>''",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            100
        );
    }
    #[test]
    fn legacy_history_reopen_preserves_tombstones_and_bounds_answers() {
        let path = std::env::temp_dir().join(format!(
            "deppy-legacy-history-{}.sqlite",
            uuid::Uuid::new_v4()
        ));
        let args = json!({"message":"answer"});
        let hash = Sha256::digest(serde_json::to_vec(&("notify", &args)).unwrap()).to_vec();
        {
            let db = Connection::open(&path).unwrap();
            db.execute_batch("CREATE TABLE operations(id TEXT PRIMARY KEY,fingerprint BLOB NOT NULL,tool TEXT NOT NULL,workspace TEXT NOT NULL,session TEXT NOT NULL,created INTEGER NOT NULL,outcome TEXT NOT NULL,message TEXT NOT NULL DEFAULT ''); CREATE TABLE answer_completion(sequence INTEGER PRIMARY KEY AUTOINCREMENT,operation_id TEXT NOT NULL UNIQUE);").unwrap();
            db.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<101) INSERT INTO operations SELECT 'a'||x,?1,'notify','w','s',0,'{}','answer' FROM n",[&hash]).unwrap();
            db.execute("INSERT INTO answer_completion(operation_id) SELECT id FROM operations ORDER BY rowid",[]).unwrap();
        }
        let db = History::open(&path).unwrap();
        assert_eq!(
            db.recent()
                .unwrap()
                .iter()
                .filter(|r| !r.message.is_empty())
                .count(),
            100
        );
        assert_eq!(
            db.claim("a1", "notify", &args, "w", "s").unwrap(),
            Claim::Existing(json!({}))
        );
        assert_eq!(
            db.0.query_row("SELECT count(*) FROM operations", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            101
        );
        assert!(cleanup_steps(&db) < 5000);
        drop(db);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    #[ignore = "measures real History completion, recent reads, SQL steps and index footprint"]
    fn measure_history_completion_and_cleanup_scaling() {
        for size in [1000, 10000, 99000] {
            let db = populated(size);
            let pages: i64 =
                db.0.query_row("PRAGMA page_count", [], |r| r.get(0))
                    .unwrap();
            let page_bytes: i64 =
                db.0.query_row("PRAGMA page_size", [], |r| r.get(0))
                    .unwrap();
            let steps = cleanup_steps(&db);
            let started = Instant::now();
            for _ in 0..100 {
                db.finish("i1", &json!({"status":"admitted"}), "").unwrap();
            }
            let finish_us = started.elapsed().as_secs_f64() * 1e6 / 100.0;
            let started = Instant::now();
            for _ in 0..30 {
                assert!(db.recent().unwrap().len() <= 600);
            }
            let recent_us = started.elapsed().as_secs_f64() * 1e6 / 30.0;
            eprintln!(
                "history sqlite={} inputs={size} steps={steps} finish_us={finish_us:.3} recent_us={recent_us:.3} db_bytes={}",
                rusqlite::version(),
                pages * page_bytes
            );
        }
    }
}
