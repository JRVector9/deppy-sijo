use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;

pub struct History(Connection);
#[derive(Debug, PartialEq)]
pub enum Claim {
    New,
    Existing(Value),
}
#[derive(Clone)]
pub struct Record {
    pub id: String,
    pub tool: String,
    pub workspace: String,
    pub session: String,
    pub created: i64,
    pub outcome: String,
    pub message: String,
}
impl History {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        // Do not follow a user-controlled symlink to an unrelated DB.
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            anyhow::ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                "history_file_type"
            );
        }
        let db = Self(Connection::open(path)?);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
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
        anyhow::ensure!(
            !id.is_empty()
                && id.len() <= 128
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':')),
            "invalid_operation_id"
        );
        let hash = Sha256::digest(serde_json::to_vec(&(tool, args))?).to_vec();
        let tx = self.0.unchecked_transaction()?;
        let row: Option<(Vec<u8>, String)> = tx
            .query_row(
                "SELECT fingerprint,outcome FROM operations WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((previous, outcome)) = row {
            anyhow::ensure!(previous == hash, "operation_id_conflict");
            return Ok(Claim::Existing(serde_json::from_str(&outcome)?));
        }
        let count: i64 = tx.query_row("SELECT count(*) FROM operations", [], |r| r.get(0))?;
        // Hard cap preserves at-most-once tombstones. Never evict a claim and later replay input.
        anyhow::ensure!(count < 100_000, "operation_history_full");
        tx.execute("INSERT INTO operations(id,fingerprint,tool,workspace,session,created,outcome) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![id,hash,tool,workspace,session,crate::now() as i64,json!({"status":"unknown","retry":false}).to_string()])?;
        tx.commit()?;
        Ok(Claim::New)
    }
    pub fn finish(&self, id: &str, outcome: &Value, message: &str) -> anyhow::Result<()> {
        anyhow::ensure!(message.len() <= crate::MAX_ANSWER, "answer_too_large");
        let tx = self.0.unchecked_transaction()?;
        anyhow::ensure!(
            tx.execute(
                "UPDATE operations SET outcome=?2,message=?3 WHERE id=?1",
                params![id, outcome.to_string(), message]
            )? == 1,
            "operation_not_claimed"
        );
        // Retain the latest 500 full answers; keep all bounded deduplication tombstones.
        tx.execute("UPDATE operations SET message='' WHERE rowid NOT IN (SELECT rowid FROM operations ORDER BY rowid DESC LIMIT 500)", [])?;
        tx.commit()?;
        Ok(())
    }
    pub fn recent(&self) -> anyhow::Result<Vec<Record>> {
        let mut stmt = self.0.prepare("SELECT id,tool,workspace,session,created,outcome,message FROM operations ORDER BY rowid DESC LIMIT 500")?;
        Ok(stmt
            .query_map([], |r| {
                Ok(Record {
                    id: r.get(0)?,
                    tool: r.get(1)?,
                    workspace: r.get(2)?,
                    session: r.get(3)?,
                    created: r.get(4)?,
                    outcome: r.get(5)?,
                    message: r.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }
}
