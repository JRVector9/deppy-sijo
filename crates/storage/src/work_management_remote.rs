//! Durable worker admission receipts in the opt-in Command Center sidecar.
use super::CommandCenterStore;
use deppy_core::WorkspaceId;
use deppy_core::work_management::{AttemptId, OperationId, RuntimeIncarnationId};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteOperationIdentity {
    pub workspace_id: WorkspaceId,
    pub device_id: String,
    pub attempt_id: AttemptId,
    pub operation_id: OperationId,
    pub worker_incarnation: RuntimeIncarnationId,
    pub request_digest: String,
    pub grant_epoch: u64,
    pub lease_epoch: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteOperationResult {
    Launched {
        session_id: u64,
        execution_generation: u64,
    },
    Rejected {
        code: String,
    },
    ObservationUnknown,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteOperationClaim {
    New,
    Existing(RemoteOperationResult),
    Conflict,
}
/// Read-only lookup never inserts, claims, or converts a missing record into permission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteOperationLookup {
    Missing,
    Existing(RemoteOperationResult),
    Conflict,
}
fn validate_identity(id: &RemoteOperationIdentity) -> anyhow::Result<()> {
    for text in [
        &id.workspace_id.0,
        &id.device_id,
        &id.attempt_id.0,
        &id.operation_id.0,
        &id.worker_incarnation.0,
    ] {
        super::valid_id(text)?;
    }
    anyhow::ensure!(
        id.request_digest.len() == 64
            && id
                .request_digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "remote operation digest invalid"
    );
    anyhow::ensure!(
        id.grant_epoch > 0
            && id.grant_epoch <= i64::MAX as u64
            && id.lease_epoch > 0
            && id.lease_epoch <= i64::MAX as u64,
        "remote operation epoch invalid"
    );
    Ok(())
}
fn validate_result(result: &RemoteOperationResult) -> anyhow::Result<()> {
    match result {
        RemoteOperationResult::Launched {
            session_id,
            execution_generation,
        } => anyhow::ensure!(
            *session_id > 0 && *execution_generation > 0,
            "remote operation result invalid"
        ),
        RemoteOperationResult::Rejected { code } => super::valid_id(code)?,
        RemoteOperationResult::ObservationUnknown => {}
    }
    Ok(())
}
fn read_operation(
    conn: &rusqlite::Connection,
    id: &RemoteOperationIdentity,
) -> anyhow::Result<Option<(RemoteOperationIdentity, Option<RemoteOperationResult>)>> {
    use rusqlite::OptionalExtension;
    let row:Option<(Vec<u8>,Option<Vec<u8>>)>=conn.query_row("SELECT identity,result FROM work_remote_operations WHERE workspace_id=?1 AND (operation_id=?2 OR attempt_id=?3)",rusqlite::params![id.workspace_id.0,id.operation_id.0,id.attempt_id.0],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
    row.map(|(identity, result)| {
        anyhow::ensure!(
            identity.len() <= 2048 && result.as_ref().is_none_or(|r| r.len() <= 1024),
            "remote operation record oversized"
        );
        let identity = serde_json::from_slice(&identity)?;
        validate_identity(&identity)?;
        let result: Option<RemoteOperationResult> =
            result.map(|r| serde_json::from_slice(&r)).transpose()?;
        if let Some(result) = &result {
            validate_result(result)?;
        }
        Ok((identity, result))
    })
    .transpose()
}
impl CommandCenterStore {
    pub fn lookup_remote_operation(
        &self,
        identity: &RemoteOperationIdentity,
    ) -> anyhow::Result<RemoteOperationLookup> {
        validate_identity(identity)?;
        Ok(match read_operation(&self.conn, identity)? {
            None => RemoteOperationLookup::Missing,
            Some((old, result)) if old == *identity => RemoteOperationLookup::Existing(
                result.unwrap_or(RemoteOperationResult::ObservationUnknown),
            ),
            Some(_) => RemoteOperationLookup::Conflict,
        })
    }

    pub fn claim_remote_operation(
        &mut self,
        identity: &RemoteOperationIdentity,
    ) -> anyhow::Result<RemoteOperationClaim> {
        validate_identity(identity)?;
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some((old, result)) = read_operation(&tx, identity)? {
            return Ok(if old == *identity {
                RemoteOperationClaim::Existing(
                    result.unwrap_or(RemoteOperationResult::ObservationUnknown),
                )
            } else {
                RemoteOperationClaim::Conflict
            });
        }
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM work_remote_operations WHERE workspace_id=?1",
            [&identity.workspace_id.0],
            |row| row.get(0),
        )?;
        anyhow::ensure!(count < 256, "remote operation journal full");
        let encoded = serde_json::to_vec(identity)?;
        anyhow::ensure!(encoded.len() <= 2048, "remote operation identity oversized");
        tx.execute("INSERT INTO work_remote_operations(workspace_id,operation_id,attempt_id,identity,result) VALUES(?1,?2,?3,?4,NULL)",rusqlite::params![identity.workspace_id.0,identity.operation_id.0,identity.attempt_id.0,encoded])?;
        tx.commit()?;
        Ok(RemoteOperationClaim::New)
    }
    pub fn resolve_remote_operation(
        &mut self,
        identity: &RemoteOperationIdentity,
        result: &RemoteOperationResult,
    ) -> anyhow::Result<()> {
        validate_identity(identity)?;
        validate_result(result)?;
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let Some((old, previous)) = read_operation(&tx, identity)? else {
            anyhow::bail!("remote operation missing")
        };
        anyhow::ensure!(old == *identity, "remote operation identity conflict");
        if let Some(previous) = previous {
            anyhow::ensure!(previous == *result, "remote operation result is immutable");
            return Ok(());
        }
        let encoded = serde_json::to_vec(result)?;
        anyhow::ensure!(encoded.len() <= 1024, "remote operation result oversized");
        tx.execute(
            "UPDATE work_remote_operations SET result=?3 WHERE workspace_id=?1 AND operation_id=?2",
            rusqlite::params![identity.workspace_id.0, identity.operation_id.0, encoded],
        )?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn readonly_remote_lookup_never_claims_and_returns_only_exact_immutable_receipts() {
        let root =
            std::env::temp_dir().join(format!("deppy-query-journal-{}", uuid::Uuid::new_v4()));
        let mut store = CommandCenterStore::open(&root).unwrap();
        let id = identity();
        assert_eq!(
            store.lookup_remote_operation(&id).unwrap(),
            RemoteOperationLookup::Missing
        );
        assert_eq!(
            store.claim_remote_operation(&id).unwrap(),
            RemoteOperationClaim::New
        );
        assert_eq!(
            store.lookup_remote_operation(&id).unwrap(),
            RemoteOperationLookup::Existing(RemoteOperationResult::ObservationUnknown)
        );
        let result = RemoteOperationResult::Launched {
            session_id: 4,
            execution_generation: 7,
        };
        store.resolve_remote_operation(&id, &result).unwrap();
        assert_eq!(
            store.lookup_remote_operation(&id).unwrap(),
            RemoteOperationLookup::Existing(result.clone())
        );
        let mut wrong = id.clone();
        wrong.request_digest = "b".repeat(64);
        assert_eq!(
            store.lookup_remote_operation(&wrong).unwrap(),
            RemoteOperationLookup::Conflict
        );
        assert_eq!(
            store.lookup_remote_operation(&id).unwrap(),
            RemoteOperationLookup::Existing(result)
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    fn identity() -> RemoteOperationIdentity {
        RemoteOperationIdentity {
            workspace_id: WorkspaceId("workspace-a".into()),
            device_id: "worker-a".into(),
            attempt_id: AttemptId("attempt-a".into()),
            operation_id: OperationId("op-a".into()),
            worker_incarnation: RuntimeIncarnationId("runtime-a".into()),
            request_digest: "a".repeat(64),
            grant_epoch: 1,
            lease_epoch: 1,
        }
    }
    #[test]
    fn remote_journal_claim_survives_lost_reply_and_cannot_rebind_or_regress() {
        let root = std::env::temp_dir().join(format!("deppy-journal-{}", uuid::Uuid::new_v4()));
        let mut store = CommandCenterStore::open(&root).unwrap();
        let id = identity();
        assert_eq!(
            store.claim_remote_operation(&id).unwrap(),
            RemoteOperationClaim::New
        );
        drop(store);
        let mut store = CommandCenterStore::open(&root).unwrap();
        assert_eq!(
            store.claim_remote_operation(&id).unwrap(),
            RemoteOperationClaim::Existing(RemoteOperationResult::ObservationUnknown)
        );
        let launched = RemoteOperationResult::Launched {
            session_id: 1,
            execution_generation: 9,
        };
        store.resolve_remote_operation(&id, &launched).unwrap();
        assert_eq!(
            store.claim_remote_operation(&id).unwrap(),
            RemoteOperationClaim::Existing(launched.clone())
        );
        let mut wrong = id.clone();
        wrong.request_digest = "b".repeat(64);
        assert_eq!(
            store.claim_remote_operation(&wrong).unwrap(),
            RemoteOperationClaim::Conflict
        );
        wrong = id.clone();
        wrong.operation_id = OperationId("new-op".into());
        assert_eq!(
            store.claim_remote_operation(&wrong).unwrap(),
            RemoteOperationClaim::Conflict
        );
        assert!(
            store
                .resolve_remote_operation(&id, &RemoteOperationResult::ObservationUnknown)
                .is_err()
        );
        store.resolve_remote_operation(&id, &launched).unwrap();
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn remote_journal_two_connections_claim_exactly_once() {
        let root =
            std::env::temp_dir().join(format!("deppy-journal-race-{}", uuid::Uuid::new_v4()));
        CommandCenterStore::open(&root).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let root = root.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut store = CommandCenterStore::open(&root).unwrap();
                    barrier.wait();
                    store.claim_remote_operation(&identity()).unwrap()
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(
            results
                .iter()
                .filter(|r| **r == RemoteOperationClaim::New)
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|r| **r
                    == RemoteOperationClaim::Existing(RemoteOperationResult::ObservationUnknown))
                .count(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn remote_journal_upgrades_verified_v1_sidecar_without_touching_legacy_metadata() {
        let root =
            std::env::temp_dir().join(format!("deppy-journal-upgrade-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let main = root.join("metadata.sqlite3");
        std::fs::write(&main, b"legacy fixture bytes").unwrap();
        let conn =
            rusqlite::Connection::open(root.join(super::super::COMMAND_CENTER_FILE_NAME)).unwrap();
        conn.execute_batch(super::super::SCHEMA).unwrap();
        conn.execute(
            "UPDATE work_store_meta SET schema_digest=?1,source_digest=?2 WHERE id=1",
            rusqlite::params![
                super::super::schema_digest(&conn).unwrap(),
                super::super::source_digest(1)
            ],
        )
        .unwrap();
        conn.pragma_update(None, "application_id", super::super::APPLICATION_ID)
            .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        drop(conn);
        let mut store = CommandCenterStore::open(&root).unwrap();
        assert_eq!(
            store.claim_remote_operation(&identity()).unwrap(),
            RemoteOperationClaim::New
        );
        assert_eq!(
            store
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            super::super::SCHEMA_VERSION
        );
        drop(store);
        assert_eq!(std::fs::read(&main).unwrap(), b"legacy fixture bytes");
        std::fs::remove_dir_all(root).unwrap();
    }
}
