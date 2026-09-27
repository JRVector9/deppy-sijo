use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;

const PRUNE_ANSWERS_SQL: &str = "UPDATE operations SET message='' WHERE tool='notify' AND message<>'' AND id NOT IN (SELECT operation_id FROM answer_completion ORDER BY sequence DESC LIMIT 100)";

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
    pub fn recent(&self) -> anyhow::Result<Vec<Record>> {
        let mut stmt = self.0.prepare("SELECT id,tool,workspace,session,created,outcome,message FROM operations WHERE rowid IN (SELECT rowid FROM operations ORDER BY rowid DESC LIMIT 500) OR id IN (SELECT operation_id FROM answer_completion ORDER BY sequence DESC LIMIT 100) ORDER BY (SELECT sequence FROM answer_completion WHERE operation_id=operations.id) DESC, rowid DESC")?;
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

#[cfg(test)]
mod efficiency_tests {
    use super::*;
    use rusqlite::StatementStatus;
    use std::time::Instant;

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
