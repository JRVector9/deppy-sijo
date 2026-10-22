//! Immutable bounded command/manual verification evidence in the opt-in sidecar only.
use super::CommandCenterStore;
use deppy_core::WorkspaceId;
use deppy_core::work_management::*;
use rusqlite::{Connection, OptionalExtension};

pub const VERIFICATION_HISTORY_MAX_COUNT: usize = 256;
pub const VERIFICATION_RECORD_MAX_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationClaim {
    New,
    Existing(CommandVerificationEvidence),
    Conflict,
}

fn encode(evidence: &VerificationEvidence) -> anyhow::Result<Vec<u8>> {
    evidence.validate()?;
    let bytes = serde_json::to_vec(evidence)?;
    anyhow::ensure!(
        bytes.len() <= VERIFICATION_RECORD_MAX_BYTES,
        "verification record oversized"
    );
    Ok(bytes)
}
fn read(
    conn: &Connection,
    workspace: &WorkspaceId,
    id: &VerificationId,
) -> anyhow::Result<Option<VerificationEvidence>> {
    let bytes: Option<Vec<u8>> = conn.query_row("SELECT body FROM work_verification_evidence WHERE workspace_id=?1 AND verification_id=?2", rusqlite::params![workspace.0, id.0], |r| r.get(0)).optional()?;
    bytes
        .map(|bytes| decode(&bytes, workspace, Some(id)))
        .transpose()
}
fn decode(
    bytes: &[u8],
    workspace: &WorkspaceId,
    id: Option<&VerificationId>,
) -> anyhow::Result<VerificationEvidence> {
    anyhow::ensure!(
        bytes.len() <= VERIFICATION_RECORD_MAX_BYTES,
        "verification record oversized"
    );
    let evidence: VerificationEvidence = serde_json::from_slice(bytes)?;
    evidence.validate()?;
    anyhow::ensure!(
        &evidence.target().workspace_id == workspace && id.is_none_or(|id| evidence.id() == id),
        "verification owner mismatch"
    );
    Ok(evidence)
}
fn insert(conn: &Connection, evidence: &VerificationEvidence) -> anyhow::Result<()> {
    let bytes = encode(evidence)?;
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM work_verification_evidence WHERE workspace_id=?1",
        [&evidence.target().workspace_id.0],
        |r| r.get(0),
    )?;
    anyhow::ensure!(
        count < VERIFICATION_HISTORY_MAX_COUNT as i64,
        "verification history full"
    );
    conn.execute("INSERT INTO work_verification_evidence(workspace_id,verification_id,body) VALUES(?1,?2,?3)", rusqlite::params![evidence.target().workspace_id.0, evidence.id().0, bytes])?;
    Ok(())
}
fn same_request(a: &CommandVerificationEvidence, b: &CommandVerificationEvidence) -> bool {
    a.id == b.id && a.target == b.target && a.spec == b.spec && a.started_at_ms == b.started_at_ms
}
impl CommandCenterStore {
    pub fn start_command_verification(
        &mut self,
        evidence: &CommandVerificationEvidence,
    ) -> anyhow::Result<VerificationClaim> {
        evidence.validate()?;
        anyhow::ensure!(
            evidence.outcome == CommandVerificationOutcome::Running,
            "verification must start running"
        );
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(old) = read(&tx, &evidence.target.workspace_id, &evidence.id)? {
            return Ok(match old {
                VerificationEvidence::AppLocal(old) if same_request(&old, evidence) => {
                    VerificationClaim::Existing(old)
                }
                _ => VerificationClaim::Conflict,
            });
        }
        insert(&tx, &VerificationEvidence::AppLocal(evidence.clone()))?;
        tx.commit()?;
        Ok(VerificationClaim::New)
    }
    pub fn finish_command_verification(
        &mut self,
        evidence: &CommandVerificationEvidence,
    ) -> anyhow::Result<()> {
        evidence.validate()?;
        anyhow::ensure!(
            evidence.outcome != CommandVerificationOutcome::Running,
            "verification has no terminal outcome"
        );
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let Some(VerificationEvidence::AppLocal(old)) =
            read(&tx, &evidence.target.workspace_id, &evidence.id)?
        else {
            anyhow::bail!("command verification claim missing");
        };
        anyhow::ensure!(
            same_request(&old, evidence),
            "verification request identity conflict"
        );
        if old.outcome != CommandVerificationOutcome::Running {
            anyhow::ensure!(old == *evidence, "verification terminal evidence immutable");
            return Ok(());
        }
        let bytes = encode(&VerificationEvidence::AppLocal(evidence.clone()))?;
        tx.execute("UPDATE work_verification_evidence SET body=?3 WHERE workspace_id=?1 AND verification_id=?2", rusqlite::params![evidence.target.workspace_id.0, evidence.id.0, bytes])?;
        tx.commit()?;
        Ok(())
    }
    pub fn record_manual_verification(
        &mut self,
        evidence: &ManualVerificationEvidence,
    ) -> anyhow::Result<()> {
        let evidence = VerificationEvidence::UserManual(evidence.clone());
        evidence.validate()?;
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(old) = read(&tx, &evidence.target().workspace_id, evidence.id())? {
            anyhow::ensure!(old == evidence, "manual verification evidence immutable");
            return Ok(());
        }
        insert(&tx, &evidence)?;
        tx.commit()?;
        Ok(())
    }
    pub fn load_verifications(
        &self,
        workspace: &WorkspaceId,
    ) -> anyhow::Result<Vec<VerificationEvidence>> {
        super::valid_id(&workspace.0)?;
        let mut query = self.conn.prepare("SELECT body FROM work_verification_evidence WHERE workspace_id=?1 ORDER BY ordinal LIMIT ?2")?;
        let rows = query.query_map(
            rusqlite::params![workspace.0, VERIFICATION_HISTORY_MAX_COUNT as i64 + 1],
            |r| r.get::<_, Vec<u8>>(0),
        )?;
        let mut evidence = Vec::new();
        for row in rows {
            evidence.push(decode(&row?, workspace, None)?);
        }
        anyhow::ensure!(
            evidence.len() <= VERIFICATION_HISTORY_MAX_COUNT,
            "verification history oversized"
        );
        Ok(evidence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn evidence() -> CommandVerificationEvidence {
        CommandVerificationEvidence {
            id: VerificationId::new(),
            target: VerificationTarget {
                workspace_id: WorkspaceId("workspace".into()),
                task_id: TaskId::new(),
                attempt_id: AttemptId::new(),
                result_revision: ResultRevision(1),
                source_revision: Some("actual-provider-source".into()),
                artifact_revision: None,
            },
            spec: VerificationSpec {
                command: "printf 'durable evidence'".into(),
                cwd: "/tmp".into(),
                timeout_ms: 100,
                output_limit_bytes: 1024,
            },
            started_at_ms: 10,
            ended_at_ms: None,
            outcome: CommandVerificationOutcome::Running,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            error: None,
        }
    }
    #[test]
    fn verification_journal_preserves_exact_immutable_evidence_and_manual_origin() {
        let root =
            std::env::temp_dir().join(format!("deppy-verification-store-{}", uuid::Uuid::new_v4()));
        let mut store = CommandCenterStore::open(&root).unwrap();
        let initial = evidence();
        assert_eq!(
            store.start_command_verification(&initial).unwrap(),
            VerificationClaim::New
        );
        assert_eq!(
            store.start_command_verification(&initial).unwrap(),
            VerificationClaim::Existing(initial.clone())
        );
        let mut conflict = initial.clone();
        conflict.target.result_revision = ResultRevision(2);
        assert_eq!(
            store.start_command_verification(&conflict).unwrap(),
            VerificationClaim::Conflict
        );
        let mut final_evidence = initial.clone();
        final_evidence.outcome = CommandVerificationOutcome::Passed;
        final_evidence.exit_code = Some(0);
        final_evidence.ended_at_ms = Some(15);
        final_evidence.stdout = "durable evidence".into();
        store.finish_command_verification(&final_evidence).unwrap();
        let manual = ManualVerificationEvidence {
            id: VerificationId::new(),
            target: initial.target.clone(),
            recorded_at_ms: 16,
            passed: true,
            evidence: "User personally inspected fixture".into(),
        };
        store.record_manual_verification(&manual).unwrap();
        drop(store);
        let mut store = CommandCenterStore::open(&root).unwrap();
        assert_eq!(
            store
                .load_verifications(&initial.target.workspace_id)
                .unwrap(),
            vec![
                VerificationEvidence::AppLocal(final_evidence.clone()),
                VerificationEvidence::UserManual(manual)
            ]
        );
        conflict = final_evidence;
        conflict.stdout = "tampered".into();
        assert!(store.finish_command_verification(&conflict).is_err());
        assert!(
            store
                .load_verifications(&WorkspaceId("other".into()))
                .unwrap()
                .is_empty()
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn verification_journal_upgrades_v3_without_changing_legacy_database_or_snapshot() {
        let root =
            std::env::temp_dir().join(format!("deppy-verification-v3-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(
            root.join("metadata.sqlite3"),
            b"untouched original metadata",
        )
        .unwrap();
        let conn = Connection::open(root.join(super::super::COMMAND_CENTER_FILE_NAME)).unwrap();
        conn.execute_batch(super::super::SCHEMA).unwrap();
        conn.execute_batch(super::super::REMOTE_SCHEMA).unwrap();
        conn.execute_batch(super::super::DEVICE_SCHEMA).unwrap();
        conn.execute(
            "UPDATE work_store_meta SET schema_digest=?1,source_digest=?2 WHERE id=1",
            rusqlite::params![
                super::super::schema_digest(&conn).unwrap(),
                super::super::source_digest(3)
            ],
        )
        .unwrap();
        conn.pragma_update(None, "application_id", super::super::APPLICATION_ID)
            .unwrap();
        conn.pragma_update(None, "user_version", 3).unwrap();
        drop(conn);
        let mut store = CommandCenterStore::open(&root).unwrap();
        assert_eq!(
            store.start_command_verification(&evidence()).unwrap(),
            VerificationClaim::New
        );
        assert_eq!(
            std::fs::read(root.join("metadata.sqlite3")).unwrap(),
            b"untouched original metadata"
        );
        drop(store);
        let conn = Connection::open(root.join(super::super::COMMAND_CENTER_FILE_NAME)).unwrap();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            4
        );
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn verification_journal_bounds_history_and_rejects_oversized_output_atomically() {
        let root = std::env::temp_dir().join(format!(
            "deppy-verification-bounds-{}",
            uuid::Uuid::new_v4()
        ));
        let mut store = CommandCenterStore::open(&root).unwrap();
        let initial = evidence();
        store.start_command_verification(&initial).unwrap();
        let mut oversized = initial.clone();
        oversized.outcome = CommandVerificationOutcome::Passed;
        oversized.exit_code = Some(0);
        oversized.ended_at_ms = Some(11);
        oversized.stdout = "x".repeat(1025);
        assert!(store.finish_command_verification(&oversized).is_err());
        assert_eq!(
            store
                .load_verifications(&initial.target.workspace_id)
                .unwrap(),
            vec![VerificationEvidence::AppLocal(initial.clone())]
        );
        for _ in 1..VERIFICATION_HISTORY_MAX_COUNT {
            store.start_command_verification(&evidence()).unwrap();
        }
        assert!(store.start_command_verification(&evidence()).is_err());
        assert_eq!(
            store
                .load_verifications(&initial.target.workspace_id)
                .unwrap()
                .len(),
            VERIFICATION_HISTORY_MAX_COUNT
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
