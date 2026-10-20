//! Explicit execution grants; credentials remain logical keychain references only.
use super::CommandCenterStore;
use deppy_core::WorkspaceId;
use deppy_core::work_management::AgentSelection;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredExecutionDevice {
    pub id: String,
    pub name: String,
    pub endpoint: SocketAddr,
    pub pinned_fingerprint: String,
    pub workspace_id: WorkspaceId,
    pub folder: String,
    pub folder_anchor: String,
    pub allowed_agents: Vec<AgentSelection>,
    pub max_workers: u8,
    pub grant_epoch: u64,
    pub execution_granted: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredExecutionDevice {
    pub revision: u64,
    pub registration: RegisteredExecutionDevice,
    pub token_secret_id: String,
}
impl RegisteredExecutionDevice {
    pub fn validate(&self) -> anyhow::Result<()> {
        for id in [&self.id, &self.workspace_id.0, &self.folder_anchor] {
            super::valid_id(id)?;
        }
        anyhow::ensure!(
            !self.name.is_empty()
                && self.name.len() <= 256
                && !self.name.chars().any(char::is_control),
            "device name invalid"
        );
        let pin = self.pinned_fingerprint.as_bytes();
        anyhow::ensure!(
            pin.len() == 95
                && pin.iter().enumerate().all(|(i, b)| if i % 3 == 2 {
                    *b == b':'
                } else {
                    b.is_ascii_hexdigit() && !b.is_ascii_uppercase()
                }),
            "device pin invalid"
        );
        anyhow::ensure!(
            self.endpoint.port() > 0
                && self.folder.len() <= 4096
                && std::path::Path::new(&self.folder).is_absolute()
                && !self.folder.chars().any(char::is_control)
                && !std::path::Path::new(&self.folder)
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
            "device folder invalid"
        );
        anyhow::ensure!(
            (1..=8).contains(&self.max_workers)
                && self.grant_epoch > 0
                && !self.allowed_agents.is_empty()
                && self.allowed_agents.len() <= 32,
            "device grant invalid"
        );
        for (i, agent) in self.allowed_agents.iter().enumerate() {
            agent.validate()?;
            anyhow::ensure!(
                !self.allowed_agents[..i].contains(agent),
                "duplicate device agent"
            );
        }
        Ok(())
    }
}
impl CommandCenterStore {
    pub fn load_execution_devices(
        &self,
        workspace: &WorkspaceId,
    ) -> anyhow::Result<Vec<StoredExecutionDevice>> {
        read_devices(&self.conn, workspace)
    }
    pub fn save_execution_device(
        &mut self,
        device: &RegisteredExecutionDevice,
        secret_id: &str,
        expected_revision: Option<u64>,
    ) -> anyhow::Result<StoredExecutionDevice> {
        device.validate()?;
        super::valid_id(secret_id)?;
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let records = read_devices(&tx, &device.workspace_id)?;
        let previous = records.iter().find(|r| r.registration.id == device.id);
        anyhow::ensure!(
            previous.map(|r| r.revision) == expected_revision,
            "device revision conflict"
        );
        if let Some(previous) = previous {
            let mut revoked = previous.registration.clone();
            revoked.execution_granted = false;
            anyhow::ensure!(
                device.grant_epoch > previous.registration.grant_epoch
                    || *device == previous.registration
                    || (*device == revoked && secret_id == previous.token_secret_id),
                "device grant regression or identity replacement"
            );
        } else {
            anyhow::ensure!(records.len() < 32, "execution device registry full");
        }
        let revision = expected_revision
            .unwrap_or(0)
            .checked_add(1)
            .filter(|r| *r <= i64::MAX as u64)
            .ok_or_else(|| anyhow::anyhow!("device revision exhausted"))?;
        let stored = StoredExecutionDevice {
            revision,
            registration: device.clone(),
            token_secret_id: secret_id.into(),
        };
        let encoded = serde_json::to_vec(&stored)?;
        anyhow::ensure!(encoded.len() <= 32768, "device record oversized");
        tx.execute("INSERT INTO work_execution_devices(workspace_id,device_id,revision,body) VALUES(?1,?2,?3,?4) ON CONFLICT(workspace_id,device_id) DO UPDATE SET revision=excluded.revision,body=excluded.body",rusqlite::params![device.workspace_id.0,device.id,revision as i64,encoded])?;
        tx.commit()?;
        Ok(stored)
    }
}
fn read_devices(
    conn: &rusqlite::Connection,
    workspace: &WorkspaceId,
) -> anyhow::Result<Vec<StoredExecutionDevice>> {
    super::valid_id(&workspace.0)?;
    let mut statement=conn.prepare("SELECT device_id,revision,length(body),body FROM work_execution_devices WHERE workspace_id=?1 ORDER BY device_id LIMIT 33")?;
    let rows = statement.query_map([&workspace.0], |row| {
        let size: i64 = row.get(2)?;
        if !(1..=32768).contains(&size) {
            return Err(rusqlite::Error::InvalidQuery);
        }
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Vec<u8>>(3)?,
        ))
    })?;
    let mut records = vec![];
    for row in rows {
        let (id, revision, body) = row?;
        let stored: StoredExecutionDevice = serde_json::from_slice(&body)?;
        stored.registration.validate()?;
        super::valid_id(&stored.token_secret_id)?;
        anyhow::ensure!(
            stored.registration.workspace_id == *workspace
                && stored.registration.id == id
                && stored.revision == revision as u64
                && revision > 0,
            "device record identity invalid"
        );
        records.push(stored);
    }
    anyhow::ensure!(records.len() <= 32, "device registry oversized");
    Ok(records)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn device() -> RegisteredExecutionDevice {
        RegisteredExecutionDevice {
            id: "device-a".into(),
            name: "Worker A".into(),
            endpoint: "127.0.0.1:9000".parse().unwrap(),
            pinned_fingerprint: vec!["aa"; 32].join(":"),
            workspace_id: WorkspaceId("workspace-a".into()),
            folder: "/fixture/a".into(),
            folder_anchor: "anchor-a".into(),
            allowed_agents: vec![AgentSelection {
                provider: "fixture".into(),
                model: None,
                effort: None,
            }],
            max_workers: 2,
            grant_epoch: 1,
            execution_granted: true,
        }
    }
    #[test]
    fn work_devices_upgrade_verified_v2_and_reject_corrupt_rows() {
        let root =
            std::env::temp_dir().join(format!("deppy-device-upgrade-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let main = root.join("metadata.sqlite3");
        std::fs::write(&main, b"unchanged legacy bytes").unwrap();
        let conn =
            rusqlite::Connection::open(root.join(super::super::COMMAND_CENTER_FILE_NAME)).unwrap();
        conn.execute_batch(super::super::SCHEMA).unwrap();
        conn.execute_batch(super::super::REMOTE_SCHEMA).unwrap();
        conn.execute(
            "UPDATE work_store_meta SET schema_digest=?1,source_digest=?2 WHERE id=1",
            rusqlite::params![
                super::super::schema_digest(&conn).unwrap(),
                super::super::source_digest(2)
            ],
        )
        .unwrap();
        conn.pragma_update(None, "application_id", super::super::APPLICATION_ID)
            .unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
        drop(conn);
        let mut store = CommandCenterStore::open(&root).unwrap();
        let device = device();
        store
            .save_execution_device(&device, "worker-secret", None)
            .unwrap();
        let mut invalid = device.clone();
        invalid.allowed_agents = vec![];
        assert!(
            store
                .save_execution_device(&invalid, "worker-secret", Some(1))
                .is_err()
        );
        assert!(
            store
                .save_execution_device(&device, "x".repeat(129).as_str(), Some(1))
                .is_err()
        );
        store
            .conn
            .execute(
                "UPDATE work_execution_devices SET body=?1",
                [br#"{}"#.as_slice()],
            )
            .unwrap();
        assert!(store.load_execution_devices(&device.workspace_id).is_err());
        assert_eq!(std::fs::read(main).unwrap(), b"unchanged legacy bytes");
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn work_devices_roundtrip_cas_revoke_and_scope_are_durable() {
        let root = std::env::temp_dir().join(format!("deppy-devices-{}", uuid::Uuid::new_v4()));
        let mut store = CommandCenterStore::open(&root).unwrap();
        let original = device();
        let first = store
            .save_execution_device(&original, "cc-worker-token-a", None)
            .unwrap();
        assert_eq!(first.revision, 1);
        assert!(
            store
                .save_execution_device(&original, "cc-worker-token-a", None)
                .is_err()
        );
        let mut revoked = original.clone();
        revoked.execution_granted = false;
        let second = store
            .save_execution_device(&revoked, "cc-worker-token-a", Some(1))
            .unwrap();
        assert!(
            store
                .save_execution_device(&original, "cc-worker-token-a", Some(1))
                .is_err()
        );
        assert!(
            store
                .save_execution_device(&original, "cc-worker-token-a", Some(2))
                .is_err()
        );
        assert_eq!(
            store
                .load_execution_devices(&WorkspaceId("other".into()))
                .unwrap(),
            vec![]
        );
        drop(store);
        let store = CommandCenterStore::open(&root).unwrap();
        assert_eq!(
            store
                .load_execution_devices(&original.workspace_id)
                .unwrap(),
            vec![second]
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
