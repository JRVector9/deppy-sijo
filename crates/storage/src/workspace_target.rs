//! Authoritative Local/SSH workspace target persistence.
use super::{
    Db, SETTINGS_ROW_BYTES_MAX, SETTINGS_SNAPSHOT_BYTES_MAX, SETTINGS_WORKSPACE_LIMIT_MAX,
    WorkspaceRow, settings_workspace_write_admission, validate_workspace_text_input,
};
use anyhow::Context;
use deppy_core::workspace_target::{
    RemotePath, SshConnectionProfile, SshWorkspaceTarget, WorkspaceTarget,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

pub(super) const MIGRATION: &str = "
CREATE TABLE ssh_connection_profiles (
    id TEXT PRIMARY KEY CHECK(typeof(id)='text' AND length(CAST(id AS BLOB)) BETWEEN 1 AND 256),
    destination TEXT NOT NULL CHECK(typeof(destination)='text' AND length(CAST(destination AS BLOB)) BETWEEN 1 AND 1024),
    port INTEGER CHECK(port IS NULL OR (typeof(port)='integer' AND port BETWEEN 1 AND 65535)),
    identity_file TEXT CHECK(identity_file IS NULL OR (typeof(identity_file)='text' AND length(CAST(identity_file AS BLOB)) BETWEEN 1 AND 4096))
);
CREATE TABLE workspace_targets (
    workspace_id TEXT PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK(kind IN ('local','ssh')),
    root TEXT NOT NULL,
    connection_id TEXT REFERENCES ssh_connection_profiles(id), host TEXT, port INTEGER, account TEXT,
    host_key_sha256 TEXT, remote_workspace_id TEXT,
    CHECK((kind='local' AND connection_id IS NULL AND host IS NULL AND port IS NULL
        AND account IS NULL AND host_key_sha256 IS NULL AND remote_workspace_id IS NULL)
      OR (kind='ssh' AND connection_id IS NOT NULL AND host IS NOT NULL AND port IS NOT NULL
        AND account IS NOT NULL AND host_key_sha256 IS NOT NULL AND remote_workspace_id IS NOT NULL
        AND length(CAST(root AS BLOB)) BETWEEN 1 AND 4096
        AND length(CAST(connection_id AS BLOB)) BETWEEN 1 AND 256
        AND length(CAST(host AS BLOB)) BETWEEN 1 AND 253 AND port BETWEEN 1 AND 65535
        AND length(CAST(account AS BLOB)) BETWEEN 1 AND 256
        AND length(CAST(host_key_sha256 AS BLOB))=50
        AND length(CAST(remote_workspace_id AS BLOB)) BETWEEN 1 AND 256))
);
CREATE UNIQUE INDEX workspace_ssh_identity ON workspace_targets(host,port,account,root) WHERE kind='ssh';
INSERT INTO workspace_targets(workspace_id,kind,root) SELECT id,'local',path FROM workspaces;
CREATE TRIGGER workspace_target_local_insert AFTER INSERT ON workspaces
BEGIN INSERT INTO workspace_targets(workspace_id,kind,root) VALUES(NEW.id,'local',NEW.path); END;
CREATE TRIGGER workspace_target_local_update AFTER UPDATE OF path ON workspaces
WHEN EXISTS(SELECT 1 FROM workspace_targets WHERE workspace_id=NEW.id AND kind='local')
BEGIN UPDATE workspace_targets SET root=NEW.path WHERE workspace_id=NEW.id; END;
CREATE TRIGGER workspace_target_ssh_path_guard BEFORE UPDATE OF path,path_dev,path_ino ON workspaces
WHEN EXISTS(SELECT 1 FROM workspace_targets WHERE workspace_id=NEW.id AND kind='ssh')
 AND (NEW.path!='' OR NEW.path_dev IS NOT NULL OR NEW.path_ino IS NOT NULL)
BEGIN SELECT RAISE(ABORT,'workspace_target_requires_local'); END;
CREATE TRIGGER workspace_target_ssh_volume_guard BEFORE INSERT ON workspace_volume_identities
WHEN EXISTS(SELECT 1 FROM workspace_targets WHERE workspace_id=NEW.workspace_id AND kind='ssh')
BEGIN SELECT RAISE(ABORT,'workspace_target_requires_local'); END;
";

#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceTargetRow {
    pub row: WorkspaceRow,
    pub target: WorkspaceTarget,
}
#[derive(Debug, Clone, PartialEq)]
pub struct SshWorkspaceFindOrCreateResult {
    pub row: WorkspaceTargetRow,
    pub created: bool,
}

/// Fail closed for explicit SSH targets, including their intentionally empty legacy path.
/// A missing workspace keeps legacy missing-id behavior at the caller.
pub(super) fn require_local(conn: &Connection, id: &str) -> anyhow::Result<()> {
    let kind: Option<Option<String>> = conn.query_row(
        "SELECT target.kind FROM workspaces workspace LEFT JOIN workspace_targets target ON target.workspace_id=workspace.id WHERE workspace.id=?1",
        [id], |row| row.get(0),
    ).optional()?;
    if let Some(kind) = kind {
        anyhow::ensure!(
            kind.as_deref() == Some("local"),
            "workspace_target_requires_local"
        );
    }
    Ok(())
}

fn read_target(conn: &Connection, id: &str) -> anyhow::Result<Option<WorkspaceTarget>> {
    type TargetFields = (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<u16>,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        Option<i64>,
        Option<i64>,
    );
    let fields: Option<TargetFields> = conn.query_row(
        "SELECT target.kind,target.root,target.connection_id,target.host,target.port,target.account,
         target.host_key_sha256,target.remote_workspace_id,workspace.path,workspace.path_dev,workspace.path_ino
         FROM workspaces workspace LEFT JOIN workspace_targets target ON target.workspace_id=workspace.id WHERE workspace.id=?1",
        [id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?,row.get(8)?,row.get(9)?,row.get(10)?)),
    ).optional()?;
    let Some((
        kind,
        root,
        connection_id,
        host,
        port,
        account,
        fingerprint,
        remote_id,
        legacy_path,
        dev,
        ino,
    )) = fields
    else {
        return Ok(None);
    };
    match kind.as_str() {
        "local" => {
            anyhow::ensure!(
                root == legacy_path,
                "workspace_target_local_projection_mismatch"
            );
            Ok(Some(WorkspaceTarget::Local { root }))
        }
        "ssh" => {
            anyhow::ensure!(
                legacy_path.is_empty() && dev.is_none() && ino.is_none(),
                "workspace_target_ssh_projection_invalid"
            );
            Ok(Some(WorkspaceTarget::Ssh(SshWorkspaceTarget::new(
                connection_id.context("workspace_target_connection_missing")?,
                host.context("workspace_target_host_missing")?,
                port.context("workspace_target_port_missing")?,
                account.context("workspace_target_account_missing")?,
                fingerprint.context("workspace_target_host_key_missing")?,
                remote_id.context("workspace_target_remote_id_missing")?,
                RemotePath::new(root)?,
            )?)))
        }
        _ => anyhow::bail!("workspace_target_kind_invalid"),
    }
}

fn read_row(conn: &Connection, id: &str) -> anyhow::Result<Option<WorkspaceTargetRow>> {
    let row = conn
        .query_row(
            "SELECT id,name,path,created_at FROM workspaces WHERE id=?1",
            [id],
            |row| {
                Ok(WorkspaceRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    path: row.get(2)?,
                    created_at: row.get(3)?,
                })
            },
        )
        .optional()?;
    row.map(|row| {
        Ok(WorkspaceTargetRow {
            target: read_target(conn, id)?.context("workspace_target_missing")?,
            row,
        })
    })
    .transpose()
}

fn profile_from_sql(row: &rusqlite::Row<'_>) -> rusqlite::Result<SshConnectionProfile> {
    Ok(SshConnectionProfile {
        id: row.get(0)?,
        destination: row.get(1)?,
        port: row.get(2)?,
        identity_file: row.get(3)?,
    })
}

impl Db {
    pub fn upsert_ssh_connection_profile(
        &self,
        profile: &SshConnectionProfile,
    ) -> anyhow::Result<()> {
        profile.validate()?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let (count, exists): (i64, bool) = tx.query_row("SELECT COUNT(*),EXISTS(SELECT 1 FROM ssh_connection_profiles WHERE id=?1) FROM ssh_connection_profiles", [&profile.id], |row| Ok((row.get(0)?,row.get(1)?)))?;
        anyhow::ensure!(
            exists || count < SETTINGS_WORKSPACE_LIMIT_MAX as i64,
            "ssh_connection_profile_limit"
        );
        tx.execute("INSERT INTO ssh_connection_profiles(id,destination,port,identity_file) VALUES(?1,?2,?3,?4)
            ON CONFLICT(id) DO UPDATE SET destination=excluded.destination,port=excluded.port,identity_file=excluded.identity_file",
            rusqlite::params![profile.id,profile.destination,profile.port,profile.identity_file])?;
        tx.commit()?;
        Ok(())
    }
    pub fn ssh_connection_profile(&self, id: &str) -> anyhow::Result<Option<SshConnectionProfile>> {
        anyhow::ensure!(
            !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control),
            "ssh_connection_profile_id_invalid"
        );
        let profile: Option<SshConnectionProfile> = self
            .conn
            .query_row(
                "SELECT id,destination,port,identity_file FROM ssh_connection_profiles WHERE id=?1",
                [id],
                profile_from_sql,
            )
            .optional()?;
        if let Some(profile) = &profile {
            profile.validate()?;
        }
        Ok(profile)
    }
    pub fn list_ssh_connection_profiles(&self) -> anyhow::Result<Vec<SshConnectionProfile>> {
        let mut stmt=self.conn.prepare("SELECT id,destination,port,identity_file FROM ssh_connection_profiles ORDER BY id LIMIT ?1")?;
        let profiles = stmt
            .query_map([SETTINGS_WORKSPACE_LIMIT_MAX as i64 + 1], profile_from_sql)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        anyhow::ensure!(
            profiles.len() <= SETTINGS_WORKSPACE_LIMIT_MAX,
            "ssh_connection_profile_limit"
        );
        for profile in &profiles {
            profile.validate()?;
        }
        Ok(profiles)
    }
    pub fn workspace_target(&self, id: &str) -> anyhow::Result<Option<WorkspaceTarget>> {
        validate_workspace_text_input(id, "id")?;
        read_target(&self.conn, id)
    }
    pub fn workspace_with_target(&self, id: &str) -> anyhow::Result<Option<WorkspaceTargetRow>> {
        validate_workspace_text_input(id, "id")?;
        let tx = self.conn.unchecked_transaction()?;
        let row = read_row(&tx, id)?;
        tx.commit()?;
        Ok(row)
    }
    pub fn list_workspaces_with_targets(&self) -> anyhow::Result<Vec<WorkspaceTargetRow>> {
        let tx = self.conn.unchecked_transaction()?;
        let (count, max_bytes, total_bytes): (i64, i64, i64) = tx.query_row(
            "SELECT COUNT(*),COALESCE(MAX(row_bytes),0),COALESCE(SUM(row_bytes),0)
             FROM (SELECT length(CAST(workspace.id AS BLOB))+length(CAST(name AS BLOB))
                 +length(CAST(path AS BLOB))+length(CAST(created_at AS BLOB))
                 +COALESCE(length(CAST(target.root AS BLOB)),0)
                 +COALESCE(length(CAST(connection_id AS BLOB)),0)+COALESCE(length(CAST(host AS BLOB)),0)
                 +COALESCE(length(CAST(account AS BLOB)),0)+COALESCE(length(CAST(host_key_sha256 AS BLOB)),0)
                 +COALESCE(length(CAST(remote_workspace_id AS BLOB)),0) AS row_bytes
                 FROM workspaces workspace LEFT JOIN workspace_targets target ON target.workspace_id=workspace.id)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        anyhow::ensure!(
            count <= SETTINGS_WORKSPACE_LIMIT_MAX as i64
                && max_bytes <= SETTINGS_ROW_BYTES_MAX as i64
                && total_bytes <= SETTINGS_SNAPSHOT_BYTES_MAX as i64,
            "workspace_target_projection_limit"
        );
        let mut stmt = tx.prepare("SELECT id FROM workspaces ORDER BY created_at,id")?;
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        let rows = ids
            .iter()
            .map(|id| read_row(&tx, id)?.context("workspace_target_missing"))
            .collect::<anyhow::Result<Vec<_>>>()?;
        tx.commit()?;
        Ok(rows)
    }
    /// The validated server identity, never an alias or a local path, owns duplicate admission.
    /// Replacing a host key or a remote workspace id requires a separate explicit recovery flow.
    pub fn find_or_create_ssh_workspace(
        &self,
        name: &str,
        target: &SshWorkspaceTarget,
    ) -> anyhow::Result<SshWorkspaceFindOrCreateResult> {
        target.validate()?;
        validate_workspace_text_input(name, "name")?;
        let identity = target.identity();
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let existing: Option<String>=tx.query_row(
            "SELECT workspace_id FROM workspace_targets WHERE kind='ssh' AND host=?1 AND port=?2 AND account=?3 AND root=?4",
            (&identity.host,identity.port,&identity.account,identity.canonical_root.as_str()), |row|row.get(0),
        ).optional()?;
        if let Some(id) = existing {
            let row = read_row(&tx, &id)?.context("workspace_target_missing")?;
            let WorkspaceTarget::Ssh(stored) = &row.target else {
                anyhow::bail!("workspace_target_kind_invalid");
            };
            anyhow::ensure!(
                stored.host_key_sha256 == target.host_key_sha256,
                "workspace_target_host_key_conflict"
            );
            anyhow::ensure!(
                stored.remote_workspace_id == target.remote_workspace_id,
                "workspace_target_remote_workspace_conflict"
            );
            tx.commit()?;
            return Ok(SshWorkspaceFindOrCreateResult {
                row,
                created: false,
            });
        }
        let id = uuid::Uuid::new_v4().to_string();
        settings_workspace_write_admission(&tx, &id, name, "", None, None)?;
        tx.execute("INSERT INTO workspaces(id,name,path,created_at,updated_at) VALUES(?1,?2,'',strftime('%Y-%m-%dT%H:%M:%fZ','now'),strftime('%Y-%m-%dT%H:%M:%fZ','now'))",(&id,name))?;
        tx.execute("UPDATE workspace_targets SET kind='ssh',root=?2,connection_id=?3,host=?4,port=?5,account=?6,host_key_sha256=?7,remote_workspace_id=?8 WHERE workspace_id=?1",
            rusqlite::params![id,target.canonical_root.as_str(),target.connection_id,identity.host,target.port,target.account,target.host_key_sha256,target.remote_workspace_id])?;
        let row = read_row(&tx, &id)?.context("workspace_target_missing")?;
        tx.commit()?;
        Ok(SshWorkspaceFindOrCreateResult { row, created: true })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{MIGRATIONS, WorkspaceFolderAnchor};
    use super::*;
    use deppy_core::workspace_target::RemotePath;

    fn profile() -> SshConnectionProfile {
        SshConnectionProfile {
            id: "alias".into(),
            destination: "dev-alias".into(),
            port: None,
            identity_file: None,
        }
    }
    fn ssh_target() -> SshWorkspaceTarget {
        SshWorkspaceTarget::new(
            "alias".into(),
            "server.example".into(),
            22,
            "ubuntu".into(),
            "SHA256:abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG".into(),
            "server-workspace".into(),
            RemotePath::new("/home/ubuntu/project").unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn workspace_target_ssh_duplicates_scope_host_port_account_and_root() {
        let db = Db::open_in_memory().unwrap();
        let target = ssh_target();
        db.upsert_ssh_connection_profile(&profile()).unwrap();
        let first = db.find_or_create_ssh_workspace("remote", &target).unwrap();
        assert!(first.created);
        let mut alias = target.clone();
        alias.connection_id = "second-alias".into();
        alias.host = "SERVER.EXAMPLE".into();
        let same = db.find_or_create_ssh_workspace("same", &alias).unwrap();
        assert!(!same.created);
        assert_eq!(first.row.row.id, same.row.row.id);
        for change in 0..4 {
            let mut other = target.clone();
            match change {
                0 => other.host = "other.example".into(),
                1 => other.port = 2222,
                2 => other.account = "root".into(),
                _ => other.canonical_root = RemotePath::new("/other").unwrap(),
            }
            assert!(
                db.find_or_create_ssh_workspace("different", &other)
                    .unwrap()
                    .created
            );
        }
        assert_eq!(db.list_workspaces_with_targets().unwrap().len(), 5);
        let mut changed_key = target.clone();
        changed_key.host_key_sha256 = "SHA256:ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefg".into();
        assert!(
            db.find_or_create_ssh_workspace("changed key", &changed_key)
                .is_err()
        );
        assert_eq!(db.list_workspaces_with_targets().unwrap().len(), 5);
    }

    #[test]
    fn workspace_target_ssh_never_exposes_or_records_a_local_path_or_volume() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_ssh_connection_profile(&profile()).unwrap();
        let remote = db
            .find_or_create_ssh_workspace("remote", &ssh_target())
            .unwrap()
            .row;
        assert_eq!(remote.row.path, "");
        assert!(remote.target.local_root().is_none());
        let id = &remote.row.id;
        assert!(db.workspace_path(id).is_err());
        assert!(db.workspace_anchor(id).is_err());
        assert!(db.set_workspace_path(id, "/tmp/local").is_err());
        assert!(db.set_workspace_anchor(id, Some(1), Some(2)).is_err());
        assert!(
            db.set_workspace_path_and_anchor_with_volume(
                id,
                "/tmp/local",
                Some(1),
                Some(2),
                Some(uuid::Uuid::new_v4())
            )
            .is_err()
        );
        let anchor = WorkspaceFolderAnchor { dev: 1, ino: 2 };
        assert!(
            super::super::workspace_identity::record(
                &db.conn,
                id,
                "/home/ubuntu/project",
                anchor,
                Some(uuid::Uuid::new_v4())
            )
            .is_err()
        );
        assert_eq!(db.workspace_target(id).unwrap(), Some(remote.target));
        let anchors: (Option<i64>, Option<i64>) = db
            .conn
            .query_row(
                "SELECT path_dev,path_ino FROM workspaces WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(anchors, (None, None));
        let volumes: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM workspace_volume_identities WHERE workspace_id=?1",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(volumes, 0);
    }

    #[test]
    fn workspace_target_remote_deserialization_cannot_bypass_validation() {
        let value = serde_json::to_value(ssh_target()).unwrap();
        for (field, invalid) in [
            ("canonical_root", "relative"),
            ("canonical_root", "/home/../etc"),
            ("host_key_sha256", "unverified"),
            ("account", "user\ncommand"),
        ] {
            let mut invalid_value = value.clone();
            invalid_value[field] = invalid.into();
            assert!(
                serde_json::from_value::<SshWorkspaceTarget>(invalid_value).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn workspace_target_legacy_migration_keeps_paths_volume_proofs_and_ended_sessions() {
        let dir =
            std::env::temp_dir().join(format!("deppy-workspace-target-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.sqlite3");
        // Before the target migration is added, this fixture includes every existing migration.
        let legacy_migrations = if MIGRATIONS
            .last()
            .is_some_and(|sql| sql.contains("CREATE TABLE workspace_targets"))
        {
            &MIGRATIONS[..MIGRATIONS.len() - 1]
        } else {
            MIGRATIONS
        };
        let legacy = storage_core::open_with_migrations(&path, legacy_migrations).unwrap();
        let volume = uuid::Uuid::new_v4().to_string();
        legacy.execute("INSERT INTO workspaces(id,name,path,created_at,updated_at,path_dev,path_ino) VALUES('legacy','legacy','/Volumes/외장/é','created','updated',11,22)", []).unwrap();
        legacy.execute("INSERT INTO workspace_volume_identities VALUES('legacy','/Volumes/외장/é',11,22,?1)", [&volume]).unwrap();
        legacy.execute("INSERT INTO sessions(id,workspace_id,session_kind,title,command,args_json,cwd,status,created_at,updated_at,last_log_offset) VALUES('ended','legacy','shell','title','sh','[]','/','exited','created','updated',0)", []).unwrap();
        let config_path = dir.join("config.toml");
        let config_bytes = b"[ui]\nclosed_workspace_ids = [\"legacy\"]\n";
        std::fs::write(&config_path, config_bytes).unwrap();
        drop(legacy);
        let db = Db::open(&path).unwrap();
        assert_eq!(
            db.workspace_target("legacy").unwrap(),
            Some(WorkspaceTarget::Local {
                root: "/Volumes/외장/é".into()
            })
        );
        assert_eq!(db.workspace_anchor("legacy").unwrap(), Some((11, 22)));
        let proof: String = db
            .conn
            .query_row(
                "SELECT volume_uuid FROM workspace_volume_identities WHERE workspace_id='legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(proof, volume);
        assert_eq!(db.list_cloud_ended_sessions().unwrap()[0].id, "ended");
        assert_eq!(std::fs::read(&config_path).unwrap(), config_bytes);
        drop(db);
        let reopened = Db::open(&path).unwrap();
        assert!(matches!(
            reopened.workspace_target("legacy").unwrap(),
            Some(WorkspaceTarget::Local { .. })
        ));
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn workspace_target_migration_preserves_legacy_local_rows() {
        let db = Db::open_in_memory().unwrap();
        let exists: bool = db.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workspace_targets')",
            [], |row| row.get(0),
        ).unwrap();
        assert!(
            exists,
            "every migrated database must have an authoritative typed workspace target table"
        );
        let id = db.create_workspace("legacy").unwrap();
        db.set_workspace_path_and_anchor(&id, "/Volumes/외장/project", Some(11), Some(22))
            .unwrap();
        let target: (String, String) = db
            .conn
            .query_row(
                "SELECT kind, root FROM workspace_targets WHERE workspace_id=?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            target,
            ("local".to_owned(), "/Volumes/외장/project".to_owned())
        );
    }
    #[test]
    fn workspace_target_ssh_profile_preserves_alias_and_options_after_reopen() {
        let dir = std::env::temp_dir().join(format!("deppy-ssh-profile-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.sqlite3");
        let profile = SshConnectionProfile {
            id: "alias".into(),
            destination: "ubuntu@dev-alias".into(),
            port: Some(2222),
            identity_file: Some("~/.ssh/id_ed25519".into()),
        };
        let db = Db::open(&path).unwrap();
        db.upsert_ssh_connection_profile(&profile).unwrap();
        assert_eq!(
            db.list_ssh_connection_profiles().unwrap(),
            vec![profile.clone()]
        );
        db.find_or_create_ssh_workspace("remote", &ssh_target())
            .unwrap();
        let mut updated = profile.clone();
        updated.identity_file = Some("/Users/me/.ssh/other".into());
        db.upsert_ssh_connection_profile(&updated).unwrap();
        drop(db);
        let reopened = Db::open(&path).unwrap();
        assert_eq!(
            reopened.ssh_connection_profile("alias").unwrap(),
            Some(updated)
        );
        assert_eq!(reopened.ssh_connection_profile("missing").unwrap(), None);
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn workspace_target_ssh_profile_rejects_invalid_deserialized_data_before_write() {
        let db = Db::open_in_memory().unwrap();
        let mut profile = SshConnectionProfile {
            id: "alias".into(),
            destination: "dev-alias".into(),
            port: None,
            identity_file: None,
        };
        let mut value = serde_json::to_value(&profile).unwrap();
        value["destination"] = "host\ncommand".into();
        assert!(serde_json::from_value::<SshConnectionProfile>(value).is_err());
        profile.port = Some(0);
        assert!(db.upsert_ssh_connection_profile(&profile).is_err());
        assert!(db.list_ssh_connection_profiles().unwrap().is_empty());
    }
    #[test]
    fn workspace_target_typed_inventory_rejects_aggregate_budget_before_materializing() {
        let db = Db::open_in_memory().unwrap();
        let large_name = "n".repeat(900_000);
        for index in 0..5 {
            db.conn.execute("INSERT INTO workspaces(id,name,path,created_at,updated_at) VALUES(?1,?2,'','created','updated')", (format!("workspace-{index}"),&large_name)).unwrap();
        }
        let error = match db.list_workspaces_with_targets() {
            Err(error) => error,
            Ok(_) => panic!("oversized typed inventory must fail before reading rows"),
        };
        assert!(
            error
                .to_string()
                .contains("workspace_target_projection_limit")
        );
    }
}
