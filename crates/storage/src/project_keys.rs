//! Bounded project key metadata, protected credential publication, and durable GitHub outbox.
use super::*;
use deppy_core::project_keys::*;
use serde::{Serialize, de::DeserializeOwned};

type CredentialMetaColumns = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);
type WorkspaceIdentityColumns = (Option<String>, Option<i64>, Option<i64>, Option<String>);
type KeyColumns = (
    Option<String>,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
);

pub type ProjectKeyWorkspaceIdentity = (String, Option<WorkspaceFolderAnchor>, Option<uuid::Uuid>);
pub struct ProjectKeyValueCommit {
    pub credential_meta: Option<CredentialMeta>,
    pub expected_previous_pointer: Option<String>,
    pub physical_slot: String,
    pub oauth_json: Option<String>,
    pub masked_hint: Option<String>,
}

/// Worker-only recovery coordinates. Secret bytes live only in the protected bundle.
pub struct ProjectKeyFileJournal {
    pub id: String,
    pub workspace_id: String,
    pub environment: KeyEnvironment,
    pub key_id: String,
    pub source: KeySource,
    pub name: String,
    pub expected_revision: Option<u64>,
    pub protected_slot: String,
    pub phase: ProjectKeyFileJournalPhase,
    pub owner: Option<deppy_core::process_identity::ProcessIdentity>,
    pub created_at: i64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectKeyFileJournalPhase {
    Prepared,
    FileApplied,
    PublicationComplete,
}
impl ProjectKeyFileJournalPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::FileApplied => "file_applied",
            Self::PublicationComplete => "publication_complete",
        }
    }
    fn decode(s: &str) -> anyhow::Result<Self> {
        match s {
            "prepared" => Ok(Self::Prepared),
            "file_applied" => Ok(Self::FileApplied),
            "publication_complete" => Ok(Self::PublicationComplete),
            _ => anyhow::bail!("project_key_journal_invalid"),
        }
    }
}

pub(super) const MIGRATION: &str = "
CREATE TABLE project_keys (
 id TEXT PRIMARY KEY CHECK(length(CAST(id AS BLOB)) BETWEEN 1 AND 256),
 workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE CHECK(length(CAST(workspace_id AS BLOB)) BETWEEN 1 AND 256),
 environment TEXT NOT NULL CHECK(environment IN ('development','production')),
 name TEXT NOT NULL CHECK(length(CAST(name AS BLOB)) BETWEEN 1 AND 256),
 credential_id TEXT REFERENCES credentials(id),
 source_baseline_credential_id TEXT REFERENCES credentials(id),
 auth_credential_id TEXT REFERENCES credentials(id),
 revision INTEGER NOT NULL CHECK(revision>0),
 legacy_reference TEXT,
 managed INTEGER NOT NULL DEFAULT 0 CHECK(managed IN (0,1)),
 payload_json TEXT NOT NULL CHECK(length(CAST(payload_json AS BLOB))<=16384),
 UNIQUE(workspace_id,environment,legacy_reference)
);
CREATE INDEX project_keys_scope ON project_keys(workspace_id,environment,name,id);
CREATE INDEX project_keys_github_observers ON project_keys(id) WHERE json_type(payload_json,'$.github')='object';
CREATE TABLE project_key_owned_credentials (
 credential_id TEXT PRIMARY KEY REFERENCES credentials(id) ON DELETE CASCADE,
 workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 environment TEXT NOT NULL CHECK(environment IN ('development','production'))
);
CREATE TRIGGER project_key_default_binding_environment BEFORE INSERT ON workspace_credential_env
 WHEN EXISTS(SELECT 1 FROM project_key_owned_credentials WHERE credential_id=NEW.credential_id AND (workspace_id<>NEW.workspace_id OR environment<>'development'))
 BEGIN SELECT RAISE(ABORT,'project_key_credential_environment_invalid'); END;
CREATE TRIGGER project_key_default_binding_update_environment BEFORE UPDATE OF credential_id,workspace_id ON workspace_credential_env
 WHEN EXISTS(SELECT 1 FROM project_key_owned_credentials WHERE credential_id=NEW.credential_id AND (workspace_id<>NEW.workspace_id OR environment<>'development'))
 BEGIN SELECT RAISE(ABORT,'project_key_credential_environment_invalid'); END;
CREATE TRIGGER project_key_profile_value_environment BEFORE INSERT ON env_vars
 WHEN EXISTS(SELECT 1 FROM project_key_owned_credentials o JOIN env_profiles p ON p.id=NEW.profile_id WHERE o.credential_id=NEW.credential_id AND (o.workspace_id<>p.workspace_id OR o.environment<>CASE WHEN p.is_production=1 OR p.kind='production' THEN 'production' ELSE 'development' END))
 BEGIN SELECT RAISE(ABORT,'project_key_credential_environment_invalid'); END;
CREATE TRIGGER project_key_profile_update_environment BEFORE UPDATE OF credential_id,profile_id ON env_vars
 WHEN EXISTS(SELECT 1 FROM project_key_owned_credentials o JOIN env_profiles p ON p.id=NEW.profile_id WHERE o.credential_id=NEW.credential_id AND (o.workspace_id<>p.workspace_id OR o.environment<>CASE WHEN p.is_production=1 OR p.kind='production' THEN 'production' ELSE 'development' END))
 BEGIN SELECT RAISE(ABORT,'project_key_credential_environment_invalid'); END;
CREATE TRIGGER project_key_insert_owner BEFORE INSERT ON project_keys BEGIN
 SELECT CASE WHEN NEW.credential_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM credentials WHERE id=NEW.credential_id AND (workspace_id IS NULL OR workspace_id=NEW.workspace_id)) THEN RAISE(ABORT,'project_key_credential_owner_invalid') END;
 SELECT CASE WHEN NEW.source_baseline_credential_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM credentials WHERE id=NEW.source_baseline_credential_id AND (workspace_id IS NULL OR workspace_id=NEW.workspace_id)) THEN RAISE(ABORT,'project_key_baseline_owner_invalid') END;
 SELECT CASE WHEN NEW.auth_credential_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM credentials WHERE id=NEW.auth_credential_id AND (workspace_id IS NULL OR workspace_id=NEW.workspace_id)) THEN RAISE(ABORT,'project_key_auth_owner_invalid') END;
 SELECT CASE WHEN NOT EXISTS(SELECT 1 FROM project_keys WHERE id=NEW.id) AND (SELECT COUNT(*) FROM project_keys WHERE workspace_id=NEW.workspace_id AND environment=NEW.environment)>=256 THEN RAISE(ABORT,'project_key_limit') END;
END;
CREATE TRIGGER project_key_update_owner BEFORE UPDATE ON project_keys BEGIN
 SELECT CASE WHEN NEW.id<>OLD.id OR NEW.workspace_id<>OLD.workspace_id OR NEW.environment<>OLD.environment THEN RAISE(ABORT,'project_key_owner_immutable') END;
 SELECT CASE WHEN NEW.credential_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM credentials WHERE id=NEW.credential_id AND (workspace_id IS NULL OR workspace_id=NEW.workspace_id)) THEN RAISE(ABORT,'project_key_credential_owner_invalid') END;
 SELECT CASE WHEN NEW.source_baseline_credential_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM credentials WHERE id=NEW.source_baseline_credential_id AND (workspace_id IS NULL OR workspace_id=NEW.workspace_id)) THEN RAISE(ABORT,'project_key_baseline_owner_invalid') END;
 SELECT CASE WHEN NEW.auth_credential_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM credentials WHERE id=NEW.auth_credential_id AND (workspace_id IS NULL OR workspace_id=NEW.workspace_id)) THEN RAISE(ABORT,'project_key_auth_owner_invalid') END;
END;
CREATE TRIGGER project_key_credential_move_guard BEFORE UPDATE OF workspace_id ON credentials
 WHEN NEW.workspace_id IS NOT OLD.workspace_id AND (EXISTS(SELECT 1 FROM project_key_owned_credentials WHERE credential_id=OLD.id) OR (NEW.workspace_id IS NOT NULL AND EXISTS(SELECT 1 FROM project_keys WHERE (credential_id=OLD.id OR auth_credential_id=OLD.id OR source_baseline_credential_id=OLD.id) AND workspace_id<>NEW.workspace_id)))
 BEGIN SELECT RAISE(ABORT,'project_key_credential_owner_invalid'); END;
CREATE TABLE project_key_settings (
 workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE CHECK(length(CAST(workspace_id AS BLOB)) BETWEEN 1 AND 256),
 environment TEXT NOT NULL CHECK(environment IN ('development','production')),
 revision INTEGER NOT NULL CHECK(revision>0),
 watch_enabled INTEGER NOT NULL CHECK(watch_enabled IN (0,1)),
 payload_json TEXT NOT NULL CHECK(length(CAST(payload_json AS BLOB))<=16384),
 PRIMARY KEY(workspace_id,environment)
);
CREATE TABLE project_key_source_exclusions (
 workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 environment TEXT NOT NULL CHECK(environment IN ('development','production')),
 source_json TEXT NOT NULL CHECK(length(CAST(source_json AS BLOB))<=8192),
 name TEXT NOT NULL CHECK(length(CAST(name AS BLOB)) BETWEEN 1 AND 256),
 PRIMARY KEY(workspace_id,environment,source_json,name)
);
CREATE TRIGGER project_key_source_exclusion_bound BEFORE INSERT ON project_key_source_exclusions
 WHEN NOT EXISTS(SELECT 1 FROM project_key_source_exclusions WHERE workspace_id=NEW.workspace_id AND environment=NEW.environment AND source_json=NEW.source_json AND name=NEW.name)
 BEGIN SELECT CASE WHEN (SELECT COUNT(*) FROM project_key_source_exclusions WHERE workspace_id=NEW.workspace_id AND environment=NEW.environment)>=256 THEN RAISE(ABORT,'project_key_source_exclusion_limit') END; END;
CREATE TABLE project_key_file_journals (
 id TEXT PRIMARY KEY CHECK(length(CAST(id AS BLOB)) BETWEEN 1 AND 256),
 workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 environment TEXT NOT NULL CHECK(environment IN ('development','production')),
 key_id TEXT NOT NULL CHECK(length(CAST(key_id AS BLOB)) BETWEEN 1 AND 256),
 source_json TEXT NOT NULL CHECK(length(CAST(source_json AS BLOB))<=8192),
 name TEXT NOT NULL CHECK(length(CAST(name AS BLOB)) BETWEEN 1 AND 256),
 expected_revision INTEGER,
 owner_json TEXT CHECK(owner_json IS NULL OR length(CAST(owner_json AS BLOB))<=256),
 protected_slot TEXT NOT NULL CHECK(length(CAST(protected_slot AS BLOB))<=512),
 phase TEXT NOT NULL CHECK(phase IN ('prepared','file_applied','publication_complete')),
 created_at INTEGER NOT NULL,
 UNIQUE(workspace_id,source_json)
);
CREATE INDEX project_key_file_journal_slot ON project_key_file_journals(protected_slot);
CREATE TABLE project_key_legacy_introduction (
 workspace_id TEXT PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE CHECK(length(CAST(workspace_id AS BLOB)) BETWEEN 1 AND 256)
);
CREATE TABLE project_key_build_recipes (
 id TEXT PRIMARY KEY CHECK(length(CAST(id AS BLOB)) BETWEEN 1 AND 256),
 workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE CHECK(length(CAST(workspace_id AS BLOB)) BETWEEN 1 AND 256),
 environment TEXT NOT NULL CHECK(environment IN ('development','production')),
 revision INTEGER NOT NULL CHECK(revision>0),
 payload_json TEXT NOT NULL CHECK(length(CAST(payload_json AS BLOB))<=16384)
);
CREATE INDEX project_key_recipes_scope ON project_key_build_recipes(workspace_id,environment,id);
CREATE TABLE project_key_build_receipts (
 id TEXT PRIMARY KEY CHECK(length(CAST(id AS BLOB)) BETWEEN 1 AND 256),
 workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE CHECK(length(CAST(workspace_id AS BLOB)) BETWEEN 1 AND 256),
 environment TEXT NOT NULL CHECK(environment IN ('development','production')),
 started_at INTEGER NOT NULL,
 payload_json TEXT NOT NULL CHECK(length(CAST(payload_json AS BLOB))<=131072)
);
CREATE INDEX project_key_receipts_scope ON project_key_build_receipts(workspace_id,environment,started_at DESC,id);
CREATE INDEX project_key_running_receipts ON project_key_build_receipts(id) WHERE json_extract(payload_json,'$.finished_at') IS NULL;
CREATE TABLE project_key_github_outbox (
 id TEXT PRIMARY KEY CHECK(length(CAST(id AS BLOB)) BETWEEN 1 AND 256),
 key_id TEXT NOT NULL UNIQUE REFERENCES project_keys(id) ON DELETE CASCADE,
 workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE CHECK(length(CAST(workspace_id AS BLOB)) BETWEEN 1 AND 256),
 environment TEXT NOT NULL CHECK(environment IN ('development','production')),
 key_revision INTEGER NOT NULL CHECK(key_revision>0),
 next_attempt_at INTEGER NOT NULL,
 claim_until INTEGER,
 payload_json TEXT NOT NULL CHECK(length(CAST(payload_json AS BLOB))<=16384)
);
CREATE INDEX project_key_outbox_due ON project_key_github_outbox(next_attempt_at,claim_until,key_id);
CREATE INDEX project_keys_value_reference ON project_keys(credential_id);
CREATE TRIGGER project_key_legacy_plain_generation AFTER UPDATE OF plain_value,kind,credential_id,key ON env_vars
 WHEN OLD.plain_value IS NOT NEW.plain_value OR OLD.kind IS NOT NEW.kind OR OLD.credential_id IS NOT NEW.credential_id OR OLD.key IS NOT NEW.key
 BEGIN
  DELETE FROM project_key_github_outbox WHERE key_id IN (SELECT id FROM project_keys WHERE workspace_id=(SELECT workspace_id FROM env_profiles WHERE id=NEW.profile_id) AND legacy_reference='env_var:'||NEW.id AND (credential_id IS NULL OR json_extract(payload_json,'$.usage')='legacy_agent'));
  UPDATE project_keys SET
   revision=CASE WHEN revision<9223372036854775807 THEN revision+1 ELSE RAISE(ABORT,'project_key_revision_exhausted') END,
   payload_json=json_set(payload_json,'$.revision',revision+1,'$.storage_state','update_needed','$.error_code','project_key_source_changed')
  WHERE workspace_id=(SELECT workspace_id FROM env_profiles WHERE id=NEW.profile_id) AND legacy_reference='env_var:'||NEW.id AND (credential_id IS NULL OR json_extract(payload_json,'$.usage')='legacy_agent');
 END;
CREATE TRIGGER project_key_legacy_delete_generation AFTER DELETE ON env_vars
 BEGIN
  DELETE FROM project_key_github_outbox WHERE key_id IN (SELECT id FROM project_keys WHERE workspace_id=(SELECT workspace_id FROM env_profiles WHERE id=OLD.profile_id) AND legacy_reference='env_var:'||OLD.id AND (credential_id IS NULL OR json_extract(payload_json,'$.usage')='legacy_agent'));
  UPDATE project_keys SET
   revision=CASE WHEN revision<9223372036854775807 THEN revision+1 ELSE RAISE(ABORT,'project_key_revision_exhausted') END,
   payload_json=json_set(payload_json,'$.revision',revision+1,'$.storage_state','missing_source','$.error_code','project_key_source_missing')
  WHERE workspace_id=(SELECT workspace_id FROM env_profiles WHERE id=OLD.profile_id) AND legacy_reference='env_var:'||OLD.id AND (credential_id IS NULL OR json_extract(payload_json,'$.usage')='legacy_agent');
 END;
CREATE TRIGGER project_key_value_pointer_capacity BEFORE UPDATE OF keyring_username ON credentials
 WHEN OLD.keyring_username IS NOT NEW.keyring_username
 BEGIN
  SELECT CASE WHEN (SELECT COUNT(*) FROM (SELECT 1 FROM project_keys WHERE credential_id=NEW.id LIMIT 513))>512
   THEN RAISE(ABORT,'project_key_value_reference_limit') END;
 END;
CREATE TRIGGER project_key_value_pointer_generation AFTER UPDATE OF keyring_username ON credentials
 WHEN OLD.keyring_username IS NOT NEW.keyring_username
 BEGIN
  DELETE FROM project_key_github_outbox WHERE key_id IN (SELECT id FROM project_keys WHERE credential_id=NEW.id);
  UPDATE project_keys SET
   revision=CASE WHEN revision<9223372036854775807 THEN revision+1 ELSE RAISE(ABORT,'project_key_revision_exhausted') END,
   source_baseline_credential_id=NULL,
   payload_json=CASE WHEN json_type(payload_json,'$.github')='object' THEN
    json_set(payload_json,'$.revision',revision+1,'$.last_good_revision',revision+1,'$.last_good_at',CAST(strftime('%s','now') AS INTEGER),'$.source_baseline_credential_id',NULL,
     '$.storage_state',CASE WHEN json_extract(payload_json,'$.source.type')='vault' THEN 'stored' ELSE 'update_needed' END,
     '$.error_code',NULL,'$.github.state',CASE WHEN json_extract(payload_json,'$.github.state')='external_change' THEN 'external_change' ELSE 'pending' END,'$.github.error_code',NULL)
    ELSE json_set(payload_json,'$.revision',revision+1,'$.last_good_revision',revision+1,'$.last_good_at',CAST(strftime('%s','now') AS INTEGER),'$.source_baseline_credential_id',NULL,
     '$.storage_state',CASE WHEN json_extract(payload_json,'$.source.type')='vault' THEN 'stored' ELSE 'update_needed' END,'$.error_code',NULL) END
  WHERE credential_id=NEW.id;
  INSERT INTO project_key_changes(key_id,workspace_id,environment,payload_json)
   SELECT k.id,k.workspace_id,k.environment,json_object('key_id',k.id,'workspace_id',k.workspace_id,'environment',k.environment,'revision',k.revision,'occurred_at',CAST(strftime('%s','now') AS INTEGER),'kind','value_saved','storage_state',json_extract(k.payload_json,'$.storage_state'),'remote_state',json_extract(k.payload_json,'$.github.state')) FROM project_keys k WHERE k.credential_id=NEW.id AND NOT EXISTS(SELECT 1 FROM project_key_changes e WHERE e.key_id=k.id AND json_extract(e.payload_json,'$.revision')=k.revision AND json_extract(e.payload_json,'$.kind')='value_saved');
 END;

CREATE TABLE project_key_changes (
 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
 key_id TEXT NOT NULL REFERENCES project_keys(id) ON DELETE CASCADE,
 workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 environment TEXT NOT NULL CHECK(environment IN ('development','production')),
 payload_json TEXT NOT NULL CHECK(length(CAST(payload_json AS BLOB))<=4096)
);
CREATE INDEX project_key_changes_scope ON project_key_changes(workspace_id,environment,key_id,sequence DESC);
CREATE TRIGGER project_key_change_created AFTER INSERT ON project_keys BEGIN
 INSERT INTO project_key_changes(key_id,workspace_id,environment,payload_json) VALUES(NEW.id,NEW.workspace_id,NEW.environment,
  json_object('key_id',NEW.id,'workspace_id',NEW.workspace_id,'environment',NEW.environment,'revision',NEW.revision,'occurred_at',CAST(strftime('%s','now') AS INTEGER),'kind','created','storage_state',json_extract(NEW.payload_json,'$.storage_state'),'remote_state',json_extract(NEW.payload_json,'$.github.state')));
END;
CREATE TRIGGER project_key_change_updated AFTER UPDATE ON project_keys
 WHEN NEW.revision<>OLD.revision OR json_extract(NEW.payload_json,'$.storage_state') IS NOT json_extract(OLD.payload_json,'$.storage_state') OR json_extract(NEW.payload_json,'$.github.state') IS NOT json_extract(OLD.payload_json,'$.github.state') OR json_extract(NEW.payload_json,'$.github.last_sent_revision') IS NOT json_extract(OLD.payload_json,'$.github.last_sent_revision') OR json_extract(NEW.payload_json,'$.last_good_at') IS NOT json_extract(OLD.payload_json,'$.last_good_at')
 BEGIN
 INSERT INTO project_key_changes(key_id,workspace_id,environment,payload_json) VALUES(NEW.id,NEW.workspace_id,NEW.environment,
  json_object('key_id',NEW.id,'workspace_id',NEW.workspace_id,'environment',NEW.environment,'revision',NEW.revision,'occurred_at',CAST(strftime('%s','now') AS INTEGER),
   'kind',CASE WHEN json_extract(NEW.payload_json,'$.last_good_at') IS NOT json_extract(OLD.payload_json,'$.last_good_at') THEN 'value_saved' WHEN json_extract(NEW.payload_json,'$.github.state') IS NOT json_extract(OLD.payload_json,'$.github.state') OR json_extract(NEW.payload_json,'$.github.last_sent_revision') IS NOT json_extract(OLD.payload_json,'$.github.last_sent_revision') THEN 'remote_changed' WHEN json_extract(NEW.payload_json,'$.storage_state') IS NOT json_extract(OLD.payload_json,'$.storage_state') THEN 'storage_changed' ELSE 'updated' END,
   'storage_state',json_extract(NEW.payload_json,'$.storage_state'),'remote_state',json_extract(NEW.payload_json,'$.github.state')));
END;
CREATE TRIGGER project_key_change_bound AFTER INSERT ON project_key_changes BEGIN
 DELETE FROM project_key_changes WHERE key_id=NEW.key_id AND sequence IN (SELECT sequence FROM project_key_changes WHERE workspace_id=NEW.workspace_id AND environment=NEW.environment AND key_id=NEW.key_id ORDER BY sequence DESC LIMIT -1 OFFSET 32);
END;

";

fn check_scope(conn: &Connection, workspace: &str) -> anyhow::Result<()> {
    anyhow::ensure!(valid_id(workspace), "project_key_workspace_invalid");
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM workspaces WHERE id=?1)",
        [workspace],
        |r| r.get(0),
    )?;
    anyhow::ensure!(exists, "project_key_workspace_missing");
    Ok(())
}
fn encode<T: Serialize>(value: &T, max: usize) -> anyhow::Result<String> {
    let json = serde_json::to_string(value)?;
    anyhow::ensure!(json.len() <= max, "project_key_payload_limit");
    Ok(json)
}
fn decode<T: DeserializeOwned>(json: Option<String>) -> anyhow::Result<T> {
    let json = json.context("project_key_payload_limit")?;
    serde_json::from_str(&json).map_err(|_| anyhow::anyhow!("project_key_payload_invalid"))
}
fn insert_source_exclusion(
    conn: &Connection,
    workspace: &str,
    environment: KeyEnvironment,
    source: &KeySource,
    name: &str,
) -> anyhow::Result<()> {
    ProjectKeySourceExclusion {
        source: source.clone(),
        name: name.into(),
    }
    .validate()
    .map_err(anyhow::Error::msg)?;
    conn.execute("INSERT INTO project_key_source_exclusions(workspace_id,environment,source_json,name) VALUES (?1,?2,?3,?4) ON CONFLICT DO NOTHING", (workspace, environment.as_str(), encode(source,8192)?, name))?;
    Ok(())
}
fn read_file_journal(
    conn: &Connection,
    workspace: &str,
    environment: KeyEnvironment,
    id: &str,
) -> anyhow::Result<Option<ProjectKeyFileJournal>> {
    anyhow::ensure!(valid_id(id), "project_key_journal_invalid");
    let journal = conn.query_row("SELECT id,key_id,source_json,name,expected_revision,protected_slot,phase,created_at,owner_json FROM project_key_file_journals WHERE workspace_id=?1 AND environment=?2 AND id=?3", (workspace,environment.as_str(),id), |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,Option<i64>>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?,r.get::<_,i64>(7)?,r.get::<_,Option<String>>(8)?))).optional()?;
    journal
        .map(
            |(
                id,
                key_id,
                json,
                name,
                expected_revision,
                protected_slot,
                phase,
                created_at,
                owner,
            )| {
                let expected_revision = expected_revision.map(|r| r as u64);
                let source: KeySource = decode(Some(json))?;
                ProjectKeySourceExclusion {
                    source: source.clone(),
                    name: name.clone(),
                }
                .validate()
                .map_err(anyhow::Error::msg)?;
                anyhow::ensure!(
                    valid_id(&key_id) && expected_revision.is_none_or(valid_revision),
                    "project_key_journal_invalid"
                );
                validate_owned_physical_secret_slot(&id, &protected_slot)?;
                Ok(ProjectKeyFileJournal {
                    id,
                    workspace_id: workspace.into(),
                    environment,
                    key_id,
                    source,
                    name,
                    expected_revision,
                    protected_slot,
                    phase: ProjectKeyFileJournalPhase::decode(&phase)?,
                    owner: owner.map(|json| decode(Some(json))).transpose()?,
                    created_at,
                })
            },
        )
        .transpose()
}

fn write_reconciled_key(conn: &Connection, key: &ProjectKey) -> anyhow::Result<()> {
    key.validate().map_err(anyhow::Error::msg)?;
    check_credential(conn, &key.workspace_id, key.credential_id.as_deref())?;
    conn.execute("UPDATE project_keys SET name=?4,credential_id=?5,source_baseline_credential_id=?6,revision=?7,payload_json=?8 WHERE id=?1 AND workspace_id=?2 AND environment=?3",rusqlite::params![key.id,key.workspace_id,key.environment.as_str(),key.name,key.credential_id,key.source_baseline_credential_id,key.revision as i64,encode(key,PROJECT_KEY_METADATA_BYTES_MAX)?])?;
    Ok(())
}
fn reconcile_legacy_row(
    conn: &Connection,
    proposed: &ProjectKey,
    seen: &mut std::collections::HashSet<String>,
) -> anyhow::Result<()> {
    let reference = proposed
        .legacy_reference
        .as_deref()
        .context("project_key_legacy_reference_invalid")?;
    let existing_id: Option<String> = if reference.starts_with("credential_binding:") {
        conn.query_row("SELECT id FROM project_keys k WHERE workspace_id=?1 AND environment=?2 AND legacy_reference LIKE 'credential_binding:%' AND (credential_id=?3 OR legacy_reference=?4 OR (name=?5 AND NOT EXISTS(SELECT 1 FROM workspace_credential_env b WHERE b.workspace_id=k.workspace_id AND b.credential_id=k.credential_id))) ORDER BY CASE WHEN credential_id=?3 THEN 0 WHEN legacy_reference=?4 THEN 1 ELSE 2 END,id LIMIT 1",rusqlite::params![proposed.workspace_id,proposed.environment.as_str(),proposed.credential_id,reference,proposed.name],|r|r.get(0)).optional()?
    } else {
        conn.query_row("SELECT id FROM project_keys WHERE workspace_id=?1 AND environment=?2 AND legacy_reference=?3",(&proposed.workspace_id,proposed.environment.as_str(),reference),|r|r.get(0)).optional()?
    };
    if let Some(id) = existing_id {
        seen.insert(id.clone());
        let mut old = read_key(conn, &proposed.workspace_id, proposed.environment, &id)?
            .context("project_key_missing")?;
        // Explicitly detached Build/StoreOnly values remain independent of their former agent rows.
        if old.usage != KeyUsage::LegacyAgent && old.credential_id.is_some() {
            return Ok(());
        }
        let changed = old.name != proposed.name
            || (proposed.credential_id.is_some() && old.credential_id != proposed.credential_id);
        let recovered = old.storage_state == KeyStorageState::MissingSource;
        if changed || recovered {
            old.revision = next_revision(Some(old.revision))?;
            old.name = proposed.name.clone();
            old.updated_at = proposed.updated_at.max(old.updated_at);
            if proposed.credential_id.is_some() {
                if old.credential_id != proposed.credential_id {
                    old.last_good_at = None;
                }
                old.credential_id = proposed.credential_id.clone();
                old.source_baseline_credential_id = None;
                old.last_good_revision = Some(old.revision);
                old.storage_state = if old.source == KeySource::Vault {
                    KeyStorageState::Stored
                } else {
                    KeyStorageState::UpdateNeeded
                };
            } else {
                old.storage_state = KeyStorageState::UpdateNeeded;
            }
            old.error_code = if old.storage_state == KeyStorageState::Stored {
                None
            } else {
                Some("project_key_source_changed".into())
            };
            if let Some(github) = old.github.as_mut()
                && github.state != GitHubRemoteState::ExternalChange
            {
                github.state = GitHubRemoteState::Pending;
                github.error_code = None;
            }
            conn.execute(
                "DELETE FROM project_key_github_outbox WHERE key_id=?1",
                [&old.id],
            )?;
            write_reconciled_key(conn, &old)?;
        }
    } else {
        if proposed.source.is_file() {
            let excluded:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM project_key_source_exclusions WHERE workspace_id=?1 AND environment=?2 AND source_json=?3 AND name=?4)",(&proposed.workspace_id,proposed.environment.as_str(),encode(&proposed.source,8192)?,&proposed.name),|r|r.get(0))?;
            if excluded {
                return Ok(());
            }
        }
        write_key(conn, proposed)?;
        seen.insert(proposed.id.clone());
    }
    Ok(())
}
fn create_legacy_agent_binding(conn: &Connection, saved: &mut ProjectKey) -> anyhow::Result<()> {
    let credential = saved
        .credential_id
        .as_deref()
        .context("project_key_credential_missing")?;
    if saved.environment == KeyEnvironment::Development {
        Db::validate_credential_env_name(&saved.name)?;
        let collision:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workspace_credential_env WHERE workspace_id=?1 AND (env_name=?2 OR credential_id=?3))",(&saved.workspace_id,&saved.name,credential),|r|r.get(0))?;
        anyhow::ensure!(!collision, "project_key_duplicate_name");
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM workspace_credential_env WHERE workspace_id=?1",
            [&saved.workspace_id],
            |r| r.get(0),
        )?;
        anyhow::ensure!(count < 256, "project_key_legacy_limit");
        conn.execute("INSERT INTO workspace_credential_env(workspace_id,env_name,credential_id) VALUES (?1,?2,?3)",(&saved.workspace_id,&saved.name,credential))?;
        saved.legacy_reference = Some(format!("credential_binding:{}:{}", saved.name, credential));
    } else {
        let existing:Option<String>=conn.query_row("SELECT id FROM env_profiles WHERE workspace_id=?1 AND name='Project keys' AND kind='production' AND is_production=1 ORDER BY id LIMIT 1",[&saved.workspace_id],|r|r.get(0)).optional()?;
        let profile = if let Some(id) = existing {
            id
        } else {
            let id = uuid::Uuid::new_v4().to_string();
            settings_env_profile_write_admission(
                conn,
                &id,
                &saved.workspace_id,
                "Project keys",
                "production",
            )?;
            conn.execute("INSERT INTO env_profiles(id,workspace_id,name,kind,is_production,created_at,updated_at) VALUES (?1,?2,'Project keys','production',1,?3,?3)",(&id,&saved.workspace_id,&saved.updated_at.to_string()))?;
            id
        };
        settings_env_var_write_admission(
            conn,
            &profile,
            &saved.name,
            "secret",
            None,
            Some(credential),
        )?;
        let id = uuid::Uuid::new_v4().to_string();
        conn.execute("INSERT INTO env_vars(id,profile_id,key,kind,plain_value,credential_id,created_at,updated_at) VALUES (?1,?2,?3,'secret',NULL,?4,?5,?5)",(&id,&profile,&saved.name,credential,&saved.updated_at.to_string()))?;
        saved.legacy_reference = Some(format!("env_var:{id}"));
    }
    Ok(())
}

// A legacy projection is introduced for display without enrolling its source or value. Only
// explicit project management can fence the ordinary writer; legacy pointer save times cannot.
const MANAGED_KEY_SQL: &str = "(k.managed=1 OR k.legacy_reference IS NULL
 OR json_extract(k.payload_json,'$.usage')<>'legacy_agent'
 OR json_extract(k.payload_json,'$.source_checked_at') IS NOT NULL
 OR k.source_baseline_credential_id IS NOT NULL
 OR json_type(k.payload_json,'$.github')='object'
 OR EXISTS(SELECT 1 FROM project_key_owned_credentials o
    WHERE o.credential_id=k.credential_id AND o.workspace_id=k.workspace_id
      AND o.environment=k.environment))";

/// Inspect only ownership and source/name metadata after obtaining the write lock. A preloaded
/// legacy plan is not authority to overwrite a newly managed value or recreate an excluded name.
fn dotenv_row_managed(conn: &Connection, profile_id: &str, key: &str) -> anyhow::Result<bool> {
    anyhow::ensure!(
        bounded_id_is_valid(profile_id) && deppy_core::credential_env::valid_name(key),
        "project_key_source_invalid"
    );
    let sql = format!(
        "SELECT EXISTS(SELECT 1 FROM env_profiles p WHERE p.id=?1 AND (EXISTS(SELECT 1 FROM project_keys k WHERE k.workspace_id=p.workspace_id AND k.environment='development' AND {MANAGED_KEY_SQL} AND (k.legacy_reference IN (SELECT 'env_var:'||v.id FROM env_vars v WHERE v.profile_id=p.id AND v.key=?2) OR (p.kind='dotenv' AND p.is_production=0 AND k.name=?2 AND json_extract(k.payload_json,'$.source.type')='dotenv' AND json_extract(k.payload_json,'$.source.file')=p.name))) OR (p.kind='dotenv' AND p.is_production=0 AND EXISTS(SELECT 1 FROM project_key_source_exclusions e WHERE e.workspace_id=p.workspace_id AND e.environment='development' AND e.name=?2 AND json_extract(e.source_json,'$.type')='dotenv' AND json_extract(e.source_json,'$.file')=p.name)) OR (p.kind='dotenv' AND EXISTS(SELECT 1 FROM project_key_file_journals j WHERE j.workspace_id=p.workspace_id AND j.phase IN ('prepared','file_applied') AND json_extract(j.source_json,'$.type')='dotenv' AND json_extract(j.source_json,'$.file')=p.name))))"
    );
    Ok(conn.query_row(&sql, (profile_id, key), |r| r.get(0))?)
}

fn remove_obsolete_unowned_dotenv_projection(
    conn: &Connection,
    profile_id: &str,
    key: &str,
    replacement_credential: Option<&str>,
    deleting: bool,
) -> anyhow::Result<()> {
    // A projection of a pruned/replaced ordinary binding must not keep its old credential live.
    // Protected/explicit keys never reach this removal through the old-writer admission guard.
    let sql = format!(
        "DELETE FROM project_keys AS k WHERE k.environment='development' AND NOT {MANAGED_KEY_SQL} AND (?4 OR k.credential_id IS NOT ?3) AND EXISTS(SELECT 1 FROM env_vars v JOIN env_profiles p ON p.id=v.profile_id WHERE v.profile_id=?1 AND v.key=?2 AND k.workspace_id=p.workspace_id AND k.legacy_reference='env_var:'||v.id)"
    );
    conn.execute(&sql, (profile_id, key, replacement_credential, deleting))?;
    Ok(())
}

fn legacy_binding_matches(conn: &Connection, old: &ProjectKey) -> anyhow::Result<bool> {
    let Some(reference) = old.legacy_reference.as_deref() else {
        return Ok(true);
    };
    if let Some(id) = reference.strip_prefix("env_var:") {
        Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM env_vars v JOIN env_profiles p ON p.id=v.profile_id WHERE v.id=?1 AND p.workspace_id=?2 AND v.key=?3 AND v.credential_id IS ?4 AND (p.is_production=1 OR p.kind='production')=?5)",rusqlite::params![id,old.workspace_id,old.name,old.credential_id,old.environment==KeyEnvironment::Production],|r|r.get(0))?)
    } else if let Some(binding) = reference.strip_prefix("credential_binding:") {
        anyhow::ensure!(
            old.environment == KeyEnvironment::Development,
            "project_key_legacy_scope_invalid"
        );
        let (_name, _) = binding
            .split_once(':')
            .context("project_key_legacy_reference_invalid")?;
        Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM workspace_credential_env WHERE workspace_id=?1 AND env_name=?2 AND credential_id IS ?3)",rusqlite::params![old.workspace_id,old.name,old.credential_id],|r|r.get(0))?)
    } else {
        Ok(true)
    }
}
fn legacy_env_credential(
    conn: &Connection,
    key: &ProjectKey,
) -> anyhow::Result<Option<Option<String>>> {
    let Some(id) = key
        .legacy_reference
        .as_deref()
        .and_then(|reference| reference.strip_prefix("env_var:"))
    else {
        return Ok(None);
    };
    let row: Option<(String,Option<String>)> = conn.query_row("SELECT v.kind,CASE WHEN length(CAST(v.credential_id AS BLOB))<=256 THEN v.credential_id END FROM env_vars v JOIN env_profiles p ON p.id=v.profile_id WHERE v.id=?1 AND p.workspace_id=?2 AND v.key=?3 AND (p.is_production=1 OR p.kind='production')=?4",rusqlite::params![id,key.workspace_id,key.name,key.environment==KeyEnvironment::Production],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
    match row {
        Some((kind, None)) if kind == "plain" => Ok(Some(None)),
        Some((kind, Some(id))) if kind == "secret" && valid_id(&id) => {
            check_credential(conn, &key.workspace_id, Some(&id))?;
            Ok(Some(Some(id)))
        }
        Some(_) => anyhow::bail!("project_key_legacy_reference_invalid"),
        None => Ok(None),
    }
}
fn legacy_binding_present(conn: &Connection, old: &ProjectKey) -> anyhow::Result<bool> {
    let Some(reference) = old.legacy_reference.as_deref() else {
        return Ok(false);
    };
    if let Some(id) = reference.strip_prefix("env_var:") {
        Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM env_vars v JOIN env_profiles p ON p.id=v.profile_id WHERE v.id=?1 AND p.workspace_id=?2 AND (p.is_production=1 OR p.kind='production')=?3)",rusqlite::params![id,old.workspace_id,old.environment==KeyEnvironment::Production],|row|row.get(0))?)
    } else if let Some(binding) = reference.strip_prefix("credential_binding:") {
        let (_name, _) = binding
            .split_once(':')
            .context("project_key_legacy_reference_invalid")?;
        Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM workspace_credential_env WHERE workspace_id=?1 AND env_name=?2)",(&old.workspace_id,&old.name),|row|row.get(0))?)
    } else {
        Ok(false)
    }
}
fn mirror_legacy_binding(
    conn: &Connection,
    old: &ProjectKey,
    saved: &ProjectKey,
) -> anyhow::Result<()> {
    if old.credential_id == saved.credential_id && legacy_binding_matches(conn, old)? {
        return Ok(());
    }
    let Some(reference) = old.legacy_reference.as_deref() else {
        return Ok(());
    };
    let credential = saved
        .credential_id
        .as_deref()
        .context("project_key_credential_missing")?;
    if let Some(id) = reference.strip_prefix("env_var:") {
        let previous =
            legacy_env_credential(conn, old)?.context("project_key_legacy_binding_changed")?;
        let profile:String=conn.query_row("SELECT v.profile_id FROM env_vars v JOIN env_profiles p ON p.id=v.profile_id WHERE v.id=?1 AND p.workspace_id=?2 AND v.key=?3 AND v.credential_id IS ?4",rusqlite::params![id,old.workspace_id,old.name,previous],|r|r.get(0))?;
        settings_env_var_write_admission(
            conn,
            &profile,
            &saved.name,
            "secret",
            None,
            Some(credential),
        )?;
        let affected=conn.execute("UPDATE env_vars SET kind='secret',plain_value=NULL,credential_id=?2,updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?1 AND credential_id IS ?3",rusqlite::params![id,credential,previous])?;
        anyhow::ensure!(affected == 1, "project_key_legacy_binding_changed");
    } else if let Some(binding) = reference.strip_prefix("credential_binding:") {
        let (_name, _) = binding
            .split_once(':')
            .context("project_key_legacy_reference_invalid")?;
        let affected=conn.execute("DELETE FROM workspace_credential_env WHERE workspace_id=?1 AND env_name=?2 AND credential_id IS ?3",rusqlite::params![old.workspace_id,old.name,old.credential_id])?;
        anyhow::ensure!(affected == 1, "project_key_legacy_binding_changed");
        conn.execute("INSERT INTO workspace_credential_env(workspace_id,env_name,credential_id) VALUES (?1,?2,?3)",rusqlite::params![old.workspace_id,old.name,credential])?;
    }
    Ok(())
}
fn read_api_credential_meta(
    conn: &Connection,
    workspace: &str,
    id: &str,
) -> anyhow::Result<Option<CredentialMeta>> {
    let raw:Option<CredentialMetaColumns>=conn.query_row("SELECT CASE WHEN length(CAST(id AS BLOB))<=256 THEN id END,CASE WHEN length(CAST(provider AS BLOB))<=1024 THEN provider END,CASE WHEN length(CAST(label AS BLOB))<=1024 THEN label END,credential_kind,workspace_id FROM credentials WHERE id=?1 AND (workspace_id IS NULL OR workspace_id=?2) AND credential_kind IN ('api_key','token')",(id,workspace),|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
    raw.map(|(id, provider, label, kind, owner)| {
        let meta = CredentialMeta {
            id: id.context("project_key_credential_invalid")?,
            provider: provider.context("project_key_credential_invalid")?,
            label: label.context("project_key_credential_invalid")?,
            credential_kind: kind.context("project_key_credential_invalid")?,
            workspace_id: owner,
            masked_hint: None,
        };
        anyhow::ensure!(
            valid_id(&meta.id)
                && valid_metadata_text(&meta.provider, 1024)
                && valid_metadata_text(&meta.label, 1024),
            "project_key_credential_invalid"
        );
        Ok(meta)
    })
    .transpose()
}
fn read_key(
    conn: &Connection,
    workspace: &str,
    environment: KeyEnvironment,
    id: &str,
) -> anyhow::Result<Option<ProjectKey>> {
    anyhow::ensure!(valid_id(id), "project_key_id_invalid");
    let raw:Option<KeyColumns>=conn.query_row(
        "SELECT CASE WHEN length(CAST(payload_json AS BLOB))<=16384 THEN payload_json END,revision,credential_id,auth_credential_id,source_baseline_credential_id FROM project_keys WHERE id=?1 AND workspace_id=?2 AND environment=?3",
        (id,workspace,environment.as_str()),|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
    raw.map(|(json, revision, credential, auth, baseline)| {
        let key: ProjectKey = decode(json)?;
        key.validate().map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            key.id == id
                && key.workspace_id == workspace
                && key.environment == environment
                && key.revision == revision as u64
                && key.credential_id == credential
                && key.source_baseline_credential_id == baseline
                && key
                    .github
                    .as_ref()
                    .and_then(|g| g.target.auth_credential_id.clone())
                    == auth,
            "project_key_column_mismatch"
        );
        Ok(key)
    })
    .transpose()
}
fn scoped_revision(
    conn: &Connection,
    table: &str,
    workspace: &str,
    environment: KeyEnvironment,
    id: &str,
) -> anyhow::Result<Option<u64>> {
    let sql = format!("SELECT workspace_id,environment,revision FROM {table} WHERE id=?1");
    let row: Option<(String, String, i64)> = conn
        .query_row(&sql, [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    if let Some((owner, env, revision)) = row {
        anyhow::ensure!(
            owner == workspace && env == environment.as_str(),
            "project_key_owner_invalid"
        );
        anyhow::ensure!(revision > 0, "project_key_revision_invalid");
        Ok(Some(revision as u64))
    } else {
        Ok(None)
    }
}
fn next_revision(current: Option<u64>) -> anyhow::Result<u64> {
    let next = current
        .unwrap_or(0)
        .checked_add(1)
        .context("project_key_revision_exhausted")?;
    anyhow::ensure!(valid_revision(next), "project_key_revision_exhausted");
    Ok(next)
}
fn check_credential(conn: &Connection, workspace: &str, id: Option<&str>) -> anyhow::Result<()> {
    if let Some(id) = id {
        anyhow::ensure!(valid_id(id), "project_key_credential_invalid");
        let allowed:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM credentials WHERE id=?1 AND (workspace_id IS NULL OR workspace_id=?2))",(id,workspace),|r|r.get(0))?;
        anyhow::ensure!(allowed, "project_key_credential_owner_invalid");
    }
    Ok(())
}
const AUTH_ELIGIBILITY_SQL: &str = "SELECT EXISTS(SELECT 1 FROM credentials c WHERE c.id=?1 AND (c.workspace_id IS NULL OR c.workspace_id=?2) AND c.credential_kind IN ('api_key','token') AND NOT EXISTS(SELECT 1 FROM project_key_owned_credentials o WHERE o.credential_id=c.id AND (o.workspace_id<>?2 OR o.environment<>?3)) AND (c.workspace_id IS NULL OR NOT EXISTS(SELECT 1 FROM project_keys k WHERE k.credential_id=c.id OR k.source_baseline_credential_id=c.id) OR EXISTS(SELECT 1 FROM project_keys k WHERE k.credential_id=c.id AND json_extract(k.payload_json,'$.usage')='store_only' AND json_extract(k.payload_json,'$.kind')='api_token' AND json_extract(k.payload_json,'$.source.type')='vault' AND (k.environment=?3 OR COALESCE(k.legacy_reference,'') LIKE 'credential:%'))))";
fn auth_credential_eligible(
    conn: &Connection,
    workspace: &str,
    environment: KeyEnvironment,
    id: &str,
) -> anyhow::Result<bool> {
    anyhow::ensure!(valid_id(id), "project_key_credential_invalid");
    Ok(conn.query_row(
        AUTH_ELIGIBILITY_SQL,
        (id, workspace, environment.as_str()),
        |r| r.get(0),
    )?)
}
fn fail_github_auth(conn: &Connection, key: &mut ProjectKey, now: i64) -> anyhow::Result<()> {
    let github = key.github.as_mut().context("project_key_github_missing")?;
    github.state = GitHubRemoteState::Failed;
    github.error_code = Some("project_key_github_auth_failed".into());
    key.updated_at = key.updated_at.max(now);
    key.validate().map_err(anyhow::Error::msg)?;
    // Existing reference columns stay unchanged. Only a remote failure is persisted: accepting a new
    // or edited authentication reference still requires write_key's full eligibility validation.
    let affected=conn.execute("UPDATE project_keys SET payload_json=?4 WHERE id=?1 AND workspace_id=?2 AND environment=?3 AND revision=?5",rusqlite::params![key.id,key.workspace_id,key.environment.as_str(),encode(key,PROJECT_KEY_METADATA_BYTES_MAX)?,key.revision as i64])?;
    anyhow::ensure!(affected == 1, "project_key_missing");
    conn.execute(
        "DELETE FROM project_key_github_outbox WHERE key_id=?1",
        [&key.id],
    )?;
    Ok(())
}
fn enroll_owned_credential(
    conn: &Connection,
    workspace: &str,
    environment: KeyEnvironment,
    credential: &str,
) -> anyhow::Result<()> {
    let existing: Option<(String,String)> = conn.query_row("SELECT workspace_id,environment FROM project_key_owned_credentials WHERE credential_id=?1",[credential],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
    anyhow::ensure!(
        existing
            .as_ref()
            .is_none_or(|(owner, env)| owner == workspace && env == environment.as_str()),
        "project_key_credential_environment_invalid"
    );
    conn.execute("INSERT INTO project_key_owned_credentials(credential_id,workspace_id,environment) SELECT id,?2,?3 FROM credentials WHERE id=?1 AND workspace_id=?2 ON CONFLICT(credential_id) DO NOTHING",(credential,workspace,environment.as_str()))?;
    Ok(())
}
fn write_key(conn: &Connection, key: &ProjectKey) -> anyhow::Result<()> {
    key.validate().map_err(anyhow::Error::msg)?;
    check_credential(conn, &key.workspace_id, key.credential_id.as_deref())?;
    if let Some(credential) = key.credential_id.as_deref() {
        let wrong_environment: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM project_key_owned_credentials WHERE credential_id=?1 AND (workspace_id<>?2 OR environment<>?3))",(credential,&key.workspace_id,key.environment.as_str()),|row|row.get(0))?;
        anyhow::ensure!(
            !wrong_environment,
            "project_key_credential_environment_invalid"
        );
        if key.legacy_reference.is_none() {
            enroll_owned_credential(conn, &key.workspace_id, key.environment, credential)?;
        }
    }
    check_credential(
        conn,
        &key.workspace_id,
        key.source_baseline_credential_id.as_deref(),
    )?;
    let auth = key
        .github
        .as_ref()
        .and_then(|g| g.target.auth_credential_id.as_deref());
    check_credential(conn, &key.workspace_id, auth)?;
    if let Some(auth) = auth {
        anyhow::ensure!(
            auth_credential_eligible(conn, &key.workspace_id, key.environment, auth)?,
            "project_key_auth_environment_invalid"
        );
    }
    let json = encode(key, PROJECT_KEY_METADATA_BYTES_MAX)?;
    conn.execute("INSERT INTO project_keys(id,workspace_id,environment,name,credential_id,source_baseline_credential_id,auth_credential_id,revision,legacy_reference,payload_json) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT(id) DO UPDATE SET name=excluded.name,credential_id=excluded.credential_id,source_baseline_credential_id=excluded.source_baseline_credential_id,auth_credential_id=excluded.auth_credential_id,revision=excluded.revision,legacy_reference=excluded.legacy_reference,payload_json=excluded.payload_json",rusqlite::params![key.id,key.workspace_id,key.environment.as_str(),key.name,key.credential_id,key.source_baseline_credential_id,auth,key.revision as i64,key.legacy_reference,json])?;
    Ok(())
}
fn check_save(
    conn: &Connection,
    workspace: &str,
    environment: KeyEnvironment,
    key: &ProjectKey,
    expected: Option<u64>,
) -> anyhow::Result<ProjectKeyCas<ProjectKey>> {
    check_scope(conn, workspace)?;
    anyhow::ensure!(
        expected.is_none_or(valid_revision),
        "project_key_revision_invalid"
    );
    anyhow::ensure!(
        key.workspace_id == workspace && key.environment == environment && valid_id(&key.id),
        "project_key_owner_invalid"
    );
    let current = scoped_revision(conn, "project_keys", workspace, environment, &key.id)?;
    if current != expected {
        return Ok(ProjectKeyCas::Stale {
            current_revision: current,
        });
    }
    let mut saved = key.clone();
    saved.revision = next_revision(current)?;
    let previous = read_key(conn, workspace, environment, &saved.id)?;
    // Metadata-only saves cannot manufacture or overwrite an actual protected publication time.
    saved.last_good_at = previous.as_ref().and_then(|old| old.last_good_at);
    if let Some(old) = previous.as_ref() {
        // A validated unchanged last-good value can recover after missing/failed source generations.
        if saved.storage_state == KeyStorageState::Stored
            && saved.credential_id == old.credential_id
            && saved.last_good_revision.is_some()
            && saved.last_good_revision == old.last_good_revision
        {
            saved.last_good_revision = Some(saved.revision);
        }
        if let (Some(current), Some(incoming)) = (old.github.as_ref(), saved.github.as_mut()) {
            let mut same_target = incoming.target.clone();
            same_target.auto_send = current.target.auto_send;
            if same_target == current.target {
                incoming.state = current.state;
                incoming.last_sent_revision = current.last_sent_revision;
                incoming.remote_updated_at = current.remote_updated_at.clone();
                incoming.error_code = current.error_code.clone();
            } else if current.state == GitHubRemoteState::ExternalChange {
                // Target edits also require an explicit send before resuming an external-change pause.
                incoming.state = GitHubRemoteState::ExternalChange;
                incoming.last_sent_revision = None;
                incoming.remote_updated_at = None;
                incoming.error_code = None;
            }
        }
    }
    saved.validate().map_err(anyhow::Error::msg)?;
    // Different explicitly selected file origins stay visible; the same source/name cannot be added twice.
    let other_ids = {
        let mut stmt=conn.prepare("SELECT id FROM project_keys WHERE workspace_id=?1 AND environment=?2 AND name=?3 AND id<>?4 LIMIT 256")?;
        stmt.query_map(
            rusqlite::params![workspace, environment.as_str(), saved.name, saved.id],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?
    };
    let same_source = other_ids
        .into_iter()
        .map(|id| {
            read_key(conn, workspace, environment, &id)
                .map(|key| key.is_some_and(|key| key.source == saved.source))
        })
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .any(|same| same);
    if same_source {
        anyhow::ensure!(
            previous
                .as_ref()
                .is_some_and(|old| old.name == saved.name && old.source == saved.source),
            "project_key_duplicate_name"
        );
    }
    if let Some(old) = previous {
        anyhow::ensure!(old.name == saved.name, "project_key_name_immutable");
        anyhow::ensure!(
            old.legacy_reference == saved.legacy_reference,
            "project_key_legacy_reference_immutable"
        );
    } else {
        anyhow::ensure!(
            saved.legacy_reference.is_none(),
            "project_key_legacy_reference_reserved"
        );
    }
    Ok(ProjectKeyCas::Committed(saved))
}
fn enqueue(
    conn: &Connection,
    key: &mut ProjectKey,
    allow_external: bool,
    now: i64,
) -> anyhow::Result<bool> {
    let Some(github) = key.github.as_mut() else {
        return Ok(false);
    };
    if github.state == GitHubRemoteState::ExternalChange && !allow_external {
        return Ok(false);
    }
    anyhow::ensure!(
        key.credential_id.is_some()
            && key.last_good_revision == Some(key.revision)
            && key.storage_state == KeyStorageState::Stored,
        "project_key_send_value_unavailable"
    );
    let entry = GitHubOutboxEntry {
        id: uuid::Uuid::new_v4().to_string(),
        workspace_id: key.workspace_id.clone(),
        environment: key.environment,
        key_id: key.id.clone(),
        key_revision: key.revision,
        target: github.target.clone(),
        attempts: 0,
        next_attempt_at: now,
        claim_token: None,
        claim_until: None,
        error_code: None,
    };
    let json = encode(&entry, PROJECT_KEY_METADATA_BYTES_MAX)?;
    conn.execute("INSERT INTO project_key_github_outbox(id,key_id,workspace_id,environment,key_revision,next_attempt_at,claim_until,payload_json) VALUES (?1,?2,?3,?4,?5,?6,NULL,?7) ON CONFLICT(key_id) DO UPDATE SET id=excluded.id,key_revision=excluded.key_revision,next_attempt_at=excluded.next_attempt_at,claim_until=NULL,payload_json=excluded.payload_json",rusqlite::params![entry.id,entry.key_id,entry.workspace_id,entry.environment.as_str(),entry.key_revision as i64,now,json])?;
    github.state = GitHubRemoteState::Pending;
    github.error_code = None;
    write_key(conn, key)?;
    Ok(true)
}
fn record_value_publication(conn: &Connection, key: &ProjectKey) -> anyhow::Result<()> {
    conn.execute("INSERT INTO project_key_changes(key_id,workspace_id,environment,payload_json) SELECT id,workspace_id,environment,json_object('key_id',id,'workspace_id',workspace_id,'environment',environment,'revision',revision,'occurred_at',json_extract(payload_json,'$.last_good_at'),'kind','value_saved','storage_state',json_extract(payload_json,'$.storage_state'),'remote_state',json_extract(payload_json,'$.github.state')) FROM project_keys k WHERE id=?1 AND NOT EXISTS(SELECT 1 FROM project_key_changes e WHERE e.key_id=k.id AND json_extract(e.payload_json,'$.revision')=k.revision AND json_extract(e.payload_json,'$.kind')='value_saved')",[&key.id])?;
    Ok(())
}
fn finish_save(conn: &Connection, mut key: ProjectKey) -> anyhow::Result<ProjectKey> {
    // A newer local generation invalidates all in-flight old sends, even with automatic sends off.
    conn.execute(
        "DELETE FROM project_key_github_outbox WHERE key_id=?1",
        [&key.id],
    )?;
    if let Some(github) = &mut key.github
        && github.state != GitHubRemoteState::ExternalChange
    {
        github.state = GitHubRemoteState::Pending;
        github.error_code = None;
    }
    write_key(conn, &key)?;
    // A successful explicit/project worker save enrolls this row. Lazy introduction and
    // reconciliation use write_key directly and preserve the existing control flag.
    let claimed = conn.execute(
        "UPDATE project_keys SET managed=1 WHERE id=?1 AND workspace_id=?2 AND environment=?3 AND revision=?4",
        rusqlite::params![key.id, key.workspace_id, key.environment.as_str(), key.revision as i64],
    )?;
    anyhow::ensure!(claimed == 1, "project_key_missing");
    if key
        .github
        .as_ref()
        .is_some_and(|g| g.target.auto_send && g.state != GitHubRemoteState::ExternalChange)
        && key.storage_state == KeyStorageState::Stored
        && key.last_good_revision == Some(key.revision)
    {
        let now = key.updated_at;
        enqueue(conn, &mut key, false, now)?;
    }
    Ok(key)
}

impl Db {
    /// A coherent bounded root/anchor/volume lookup for source and build identity preflight.
    pub fn project_key_workspace_identity(
        &self,
        workspace: &str,
    ) -> anyhow::Result<Option<ProjectKeyWorkspaceIdentity>> {
        anyhow::ensure!(valid_id(workspace), "project_key_workspace_invalid");
        let tx = self.conn.unchecked_transaction()?;
        let row:Option<WorkspaceIdentityColumns>=tx.query_row("SELECT CASE WHEN typeof(w.path)='text' AND length(CAST(w.path AS BLOB))<=4096 THEN w.path END,w.path_dev,w.path_ino,CASE WHEN typeof(v.volume_uuid)='text' AND length(CAST(v.volume_uuid AS BLOB))=36 THEN v.volume_uuid END FROM workspaces w LEFT JOIN workspace_volume_identities v ON v.workspace_id=w.id AND v.path=w.path AND v.path_dev=w.path_dev AND v.path_ino=w.path_ino WHERE w.id=?1",[workspace],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        let result = row
            .map(|(path, dev, ino, volume)| {
                let path = path.context("project_key_workspace_path_invalid")?;
                anyhow::ensure!(
                    !path.chars().any(char::is_control),
                    "project_key_workspace_path_invalid"
                );
                let anchor = match (dev, ino) {
                    (Some(dev), Some(ino)) => Some(WorkspaceFolderAnchor { dev, ino }),
                    (None, None) => None,
                    _ => anyhow::bail!("project_key_workspace_anchor_invalid"),
                };
                let volume = volume
                    .map(|value| {
                        let id = uuid::Uuid::parse_str(&value)
                            .map_err(|_| anyhow::anyhow!("project_key_workspace_volume_invalid"))?;
                        anyhow::ensure!(!id.is_nil(), "project_key_workspace_volume_invalid");
                        Ok(id)
                    })
                    .transpose()?;
                Ok((path, anchor, volume))
            })
            .transpose()?;
        tx.commit()?;
        Ok(result)
    }
    /// Record a first identity only while the stored legacy path is still exactly the observed path.
    /// Established identities require the existing explicit workspace-rebind workflow.
    pub fn project_key_enroll_workspace_identity(
        &self,
        workspace: &str,
        expected_path: &str,
        anchor: WorkspaceFolderAnchor,
        volume: Option<uuid::Uuid>,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(
            valid_id(workspace) && valid_metadata_text(expected_path, PROJECT_KEY_PATH_BYTES_MAX),
            "project_key_workspace_path_invalid"
        );
        anyhow::ensure!(
            volume.is_none_or(|v| !v.is_nil()),
            "project_key_workspace_volume_invalid"
        );
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let Some(mut row) = settings_workspace_row_for_update(&tx, workspace)? else {
            return Ok(false);
        };
        if row.path != expected_path || row.path_dev.is_some() || row.path_ino.is_some() {
            return Ok(false);
        }
        row.path_dev = Some(anchor.dev);
        row.path_ino = Some(anchor.ino);
        settings_workspace_update_admission(&tx, &row)?;
        let changed=tx.execute("UPDATE workspaces SET path_dev=?3,path_ino=?4,updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?1 AND path=?2 AND path_dev IS NULL AND path_ino IS NULL",rusqlite::params![workspace,expected_path,anchor.dev,anchor.ino])?;
        anyhow::ensure!(changed == 1, "project_key_workspace_identity_changed");
        workspace_identity::record(&tx, workspace, expected_path, anchor, volume)?;
        tx.commit()?;
        Ok(true)
    }
    /// Check the durable claim immediately before any remote write; a prior process/target cannot send.
    pub fn project_key_github_claim_is_current(
        &self,
        entry: &GitHubOutboxEntry,
        now: i64,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(
            valid_id(&entry.id) && valid_id(&entry.key_id) && valid_id(&entry.workspace_id),
            "project_key_outbox_invalid"
        );
        if entry.claim_token.is_none() || entry.claim_until.is_none_or(|until| until <= now) {
            return Ok(false);
        }
        let tx = self.conn.unchecked_transaction()?;
        let json:Option<Option<String>>=tx.query_row("SELECT CASE WHEN length(CAST(payload_json AS BLOB))<=16384 THEN payload_json END FROM project_key_github_outbox WHERE id=?1 AND key_id=?2 AND workspace_id=?3 AND environment=?4",rusqlite::params![entry.id,entry.key_id,entry.workspace_id,entry.environment.as_str()],|r|r.get(0)).optional()?;
        let current = json.map(decode::<GitHubOutboxEntry>).transpose()?;
        let key = read_key(&tx, &entry.workspace_id, entry.environment, &entry.key_id)?;
        let matches = current.as_ref() == Some(entry)
            && key.as_ref().is_some_and(|key| {
                key.revision == entry.key_revision
                    && key.storage_state == KeyStorageState::Stored
                    && key.last_good_revision == Some(entry.key_revision)
                    && key.github.as_ref().is_some_and(|github| {
                        github.target == entry.target
                            && github.state != GitHubRemoteState::ExternalChange
                    })
            });
        tx.commit()?;
        Ok(matches)
    }

    pub fn project_key_get(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        id: &str,
    ) -> anyhow::Result<Option<ProjectKey>> {
        check_scope(&self.conn, workspace)?;
        read_key(&self.conn, workspace, environment, id)
    }
    /// Newest-first, bounded metadata audit history for one selected project/environment key.
    pub fn project_key_changes(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        key_id: &str,
    ) -> anyhow::Result<Vec<ProjectKeyChange>> {
        check_scope(&self.conn, workspace)?;
        anyhow::ensure!(valid_id(key_id), "project_key_id_invalid");
        let rows = {
            let mut stmt = self.conn.prepare("SELECT CASE WHEN length(CAST(payload_json AS BLOB))<=4096 THEN json_set(payload_json,'$.sequence',sequence) END FROM project_key_changes WHERE workspace_id=?1 AND environment=?2 AND key_id=?3 ORDER BY sequence DESC LIMIT 32")?;
            stmt.query_map((workspace, environment.as_str(), key_id), |row| {
                row.get::<_, Option<String>>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        rows.into_iter()
            .map(|json| {
                let change: ProjectKeyChange = decode(json)?;
                change.validate().map_err(anyhow::Error::msg)?;
                anyhow::ensure!(
                    change.workspace_id == workspace
                        && change.environment == environment
                        && change.key_id == key_id,
                    "project_key_owner_invalid"
                );
                Ok(change)
            })
            .collect()
    }
    pub fn project_key_save(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        key: &ProjectKey,
        expected_revision: Option<u64>,
    ) -> anyhow::Result<ProjectKeyCas<ProjectKey>> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let result = match check_save(&tx, workspace, environment, key, expected_revision)? {
            ProjectKeyCas::Committed(mut saved) => {
                if saved.usage == KeyUsage::LegacyAgent
                    && saved.source == KeySource::Vault
                    && saved.legacy_reference.is_none()
                    && saved.storage_state == KeyStorageState::Stored
                {
                    create_legacy_agent_binding(&tx, &mut saved)?;
                }
                ProjectKeyCas::Committed(finish_save(&tx, saved)?)
            }
            stale => stale,
        };
        tx.commit()?;
        Ok(result)
    }
    /// Publish a protected, staged secret pointer and local metadata/outbox in one transaction.
    pub fn project_key_commit_value(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        key: &ProjectKey,
        expected_revision: Option<u64>,
        commit: &ProjectKeyValueCommit,
    ) -> anyhow::Result<ProjectKeyCas<ProjectKey>> {
        self.project_key_commit_value_inner(
            workspace,
            environment,
            key,
            expected_revision,
            commit,
            None,
        )
    }
    pub fn project_key_commit_value_with_file_journal(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        key: &ProjectKey,
        expected_revision: Option<u64>,
        commit: &ProjectKeyValueCommit,
        journal_id: &str,
    ) -> anyhow::Result<ProjectKeyCas<ProjectKey>> {
        self.project_key_commit_value_inner(
            workspace,
            environment,
            key,
            expected_revision,
            commit,
            Some(journal_id),
        )
    }
    fn project_key_commit_value_inner(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        key: &ProjectKey,
        expected_revision: Option<u64>,
        commit: &ProjectKeyValueCommit,
        journal_id: Option<&str>,
    ) -> anyhow::Result<ProjectKeyCas<ProjectKey>> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(id) = journal_id {
            let journal = read_file_journal(&tx, workspace, environment, id)?
                .context("project_key_journal_missing")?;
            anyhow::ensure!(
                journal.key_id == key.id
                    && journal.source == key.source
                    && journal.name == key.name
                    && journal.expected_revision == expected_revision
                    && journal.phase == ProjectKeyFileJournalPhase::FileApplied,
                "project_key_journal_invalid"
            );
        }
        let ProjectKeyCas::Committed(mut saved) =
            check_save(&tx, workspace, environment, key, expected_revision)?
        else {
            return Ok(ProjectKeyCas::Stale {
                current_revision: scoped_revision(
                    &tx,
                    "project_keys",
                    workspace,
                    environment,
                    &key.id,
                )?,
            });
        };
        let previous = read_key(&tx, workspace, environment, &saved.id)?;
        if saved.usage == KeyUsage::LegacyAgent
            && previous
                .as_ref()
                .is_some_and(|old| old.legacy_reference.is_some())
            && !legacy_binding_matches(&tx, previous.as_ref().unwrap())?
            && legacy_env_credential(&tx, previous.as_ref().unwrap())?.is_none()
        {
            return Ok(ProjectKeyCas::Stale {
                current_revision: expected_revision,
            });
        }
        // Preserve the environment provenance of a detached legacy value. It must not become a
        // newly discovered Development store-only reference after its Production binding rotates.
        if let Some(old) = previous
            .as_ref()
            .and_then(|old| old.credential_id.as_deref())
            && saved.credential_id.as_deref() != Some(old)
        {
            let other_environment:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM project_keys WHERE credential_id=?1 AND workspace_id=?2 AND environment<>?3)",(old,workspace,environment.as_str()),|r|r.get(0))?;
            if !other_environment {
                enroll_owned_credential(&tx, workspace, environment, old)?;
            }
        }
        let logical = saved
            .credential_id
            .as_deref()
            .context("project_key_credential_missing")?;
        validate_owned_physical_secret_slot(logical, &commit.physical_slot)?;
        if let Some(json) = commit.oauth_json.as_deref() {
            validate_oauth_metadata_json(json)?;
        }
        if let Some(pointer) = commit.expected_previous_pointer.as_deref() {
            check_credential(&tx, workspace, Some(logical))?;
            let owned:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM credentials WHERE id=?1 AND workspace_id=?2 AND credential_kind IN ('api_key','token'))",(logical,workspace),|r|r.get(0))?;
            anyhow::ensure!(owned, "project_key_value_requires_owned_credential");
            if !Self::publish_credential_secret_slot_in_transaction(
                &tx,
                logical,
                pointer,
                &commit.physical_slot,
                commit.oauth_json.as_deref(),
                commit.masked_hint.as_deref(),
            )? {
                tx.commit()?;
                return Ok(ProjectKeyCas::Stale {
                    current_revision: expected_revision,
                });
            }
        } else {
            let meta = commit
                .credential_meta
                .as_ref()
                .context("project_key_credential_metadata_missing")?;
            anyhow::ensure!(
                meta.id == logical
                    && meta.workspace_id.as_deref() == Some(workspace)
                    && matches!(meta.credential_kind.as_str(), "api_key" | "token"),
                "project_key_credential_owner_invalid"
            );
            Self::insert_credential_with_secret_slot_in_transaction(
                &tx,
                meta,
                &commit.physical_slot,
                commit.oauth_json.as_deref(),
            )?;
            enroll_owned_credential(&tx, workspace, environment, logical)?;
        }
        if saved.usage == KeyUsage::LegacyAgent
            && let Some(old) = previous.as_ref()
        {
            mirror_legacy_binding(&tx, old, &saved)?;
        }
        if saved.usage == KeyUsage::LegacyAgent
            && saved.source == KeySource::Vault
            && saved.legacy_reference.is_none()
        {
            create_legacy_agent_binding(&tx, &mut saved)?;
        }
        saved.storage_state = KeyStorageState::Stored;
        saved.last_good_revision = Some(saved.revision);
        saved.last_good_at = Some(deppy_core::time::unix_secs_i64());
        saved.source_checked_at = saved.last_good_at;
        saved.error_code = None;
        let saved = finish_save(&tx, saved)?;
        record_value_publication(&tx, &saved)?;
        if let Some(id) = journal_id {
            tx.execute("UPDATE project_key_file_journals SET phase='publication_complete' WHERE id=?1 AND workspace_id=?2 AND environment=?3", (id, workspace, environment.as_str()))?;
        }
        tx.commit()?;
        Ok(ProjectKeyCas::Committed(saved))
    }
    pub fn project_key_delete(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        id: &str,
        expected_revision: u64,
    ) -> anyhow::Result<ProjectKeyCas<()>> {
        check_scope(&self.conn, workspace)?;
        anyhow::ensure!(
            valid_id(id) && valid_revision(expected_revision),
            "project_key_input_invalid"
        );
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = scoped_revision(&tx, "project_keys", workspace, environment, id)?;
        let result = if current != Some(expected_revision) {
            ProjectKeyCas::Stale {
                current_revision: current,
            }
        } else {
            let key = read_key(&tx, workspace, environment, id)?.context("project_key_missing")?;
            // Build/StoreOnly copies deliberately leave the old agent-auth binding alone.
            if key.usage == KeyUsage::LegacyAgent
                && !legacy_binding_matches(&tx, &key)?
                && legacy_binding_present(&tx, &key)?
                && legacy_env_credential(&tx, &key)?.is_none()
            {
                return Ok(ProjectKeyCas::Stale {
                    current_revision: current,
                });
            }
            if key.usage == KeyUsage::LegacyAgent
                && legacy_binding_present(&tx, &key)?
                && let Some(reference) = key.legacy_reference.as_deref()
            {
                if let Some(legacy_id) = reference.strip_prefix("env_var:") {
                    let current = legacy_env_credential(&tx, &key)?
                        .context("project_key_legacy_binding_changed")?;
                    let affected = tx.execute("DELETE FROM env_vars WHERE id=?1 AND credential_id IS ?2 AND key=?3 AND profile_id IN (SELECT id FROM env_profiles WHERE workspace_id=?4 AND (is_production=1 OR kind='production')=?5)",rusqlite::params![legacy_id,current,key.name,workspace,environment==KeyEnvironment::Production])?;
                    anyhow::ensure!(affected == 1, "project_key_legacy_binding_changed");
                } else if let Some(binding) = reference.strip_prefix("credential_binding:") {
                    let (_name, _) = binding
                        .split_once(':')
                        .context("project_key_legacy_reference_invalid")?;
                    let affected = tx.execute("DELETE FROM workspace_credential_env WHERE workspace_id=?1 AND env_name=?2 AND credential_id IS ?3",rusqlite::params![workspace,key.name,key.credential_id])?;
                    anyhow::ensure!(affected == 1, "project_key_legacy_binding_changed");
                }
            }
            if key.source.is_file() {
                insert_source_exclusion(&tx, workspace, environment, &key.source, &key.name)?;
            }
            tx.execute("DELETE FROM project_keys WHERE id=?1", [id])?;
            if let Some(credential) = key.credential_id.as_deref() {
                let pointer: Option<String> = tx
                    .query_row(
                        "SELECT keyring_username FROM credentials WHERE id=?1 AND workspace_id=?2",
                        (credential, workspace),
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(pointer) = pointer {
                    let affected = tx.execute("DELETE FROM credentials WHERE id=?1 AND workspace_id=?2 AND keyring_username=?3 AND NOT EXISTS(SELECT 1 FROM env_vars WHERE credential_id=?1) AND NOT EXISTS(SELECT 1 FROM workspace_credential_env WHERE credential_id=?1) AND NOT EXISTS(SELECT 1 FROM project_keys WHERE credential_id=?1 OR source_baseline_credential_id=?1 OR auth_credential_id=?1) AND NOT EXISTS(SELECT 1 FROM mcp_servers,json_each(COALESCE(mcp_servers.env_credentials_json,'{}')) WHERE json_each.value=?1)",rusqlite::params![credential,workspace,pointer])?;
                    if affected == 1 {
                        Self::orphan_published_physical_slot_if_versioned(
                            &tx, credential, &pointer,
                        )?;
                    }
                }
            }
            ProjectKeyCas::Committed(())
        };
        tx.commit()?;
        Ok(result)
    }
    /// Pin the physical value pointer and key revision in one SQLite snapshot before secret-store IO.
    /// Old credential writers advance key generations through the durable pointer trigger.
    pub fn project_key_value_location(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        key_id: &str,
        expected_revision: u64,
    ) -> anyhow::Result<Option<(CredentialMeta, CredentialSecretLocation)>> {
        check_scope(&self.conn, workspace)?;
        anyhow::ensure!(
            valid_revision(expected_revision),
            "project_key_revision_invalid"
        );
        let tx = self.conn.unchecked_transaction()?;
        let result = if let Some(key) = read_key(&tx, workspace, environment, key_id)? {
            if key.revision == expected_revision
                && (key.usage != KeyUsage::LegacyAgent || legacy_binding_matches(&tx, &key)?)
            {
                if let Some(id) = key.credential_id.as_deref() {
                    if let Some(meta) = read_api_credential_meta(&tx, workspace, id)? {
                        Some((
                            meta,
                            Self::credential_secret_location_in_snapshot(&tx, id)?
                                .context("project_key_credential_missing")?,
                        ))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        tx.commit()?;
        Ok(result)
    }
    /// Current selected legacy profile coordinate. No plain value or protected slot is read.
    pub fn project_key_legacy_env_profile(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        key_id: &str,
        expected_revision: u64,
    ) -> anyhow::Result<Option<String>> {
        check_scope(&self.conn, workspace)?;
        anyhow::ensure!(
            valid_revision(expected_revision),
            "project_key_revision_invalid"
        );
        let tx = self.conn.unchecked_transaction()?;
        let Some(key) = read_key(&tx, workspace, environment, key_id)? else {
            return Ok(None);
        };
        if key.revision != expected_revision {
            return Ok(None);
        }
        let Some(id) = key
            .legacy_reference
            .as_deref()
            .and_then(|reference| reference.strip_prefix("env_var:"))
        else {
            return Ok(None);
        };
        anyhow::ensure!(valid_id(id), "project_key_legacy_reference_invalid");
        let profile:Option<Option<String>>=tx.query_row("SELECT CASE WHEN length(CAST(p.id AS BLOB))<=256 THEN p.id END FROM env_vars v JOIN env_profiles p ON p.id=v.profile_id WHERE v.id=?1 AND v.key=?2 AND p.workspace_id=?3 AND (p.is_production=1 OR p.kind='production')=?4",rusqlite::params![id,key.name,workspace,environment==KeyEnvironment::Production],|r|r.get(0)).optional()?;
        let profile = profile
            .map(|id| {
                let id = id.context("project_key_legacy_scope_invalid")?;
                anyhow::ensure!(valid_id(&id), "project_key_legacy_scope_invalid");
                Ok(id)
            })
            .transpose()?;
        tx.commit()?;
        Ok(profile)
    }
    /// Current protected legacy env-row binding, pinned to its project-key generation for worker enrollment.
    pub fn project_key_legacy_value_location(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        key_id: &str,
        expected_revision: u64,
    ) -> anyhow::Result<Option<(CredentialMeta, CredentialSecretLocation)>> {
        check_scope(&self.conn, workspace)?;
        anyhow::ensure!(
            valid_revision(expected_revision),
            "project_key_revision_invalid"
        );
        let tx = self.conn.unchecked_transaction()?;
        let result = if let Some(key) = read_key(&tx, workspace, environment, key_id)? {
            if key.revision == expected_revision && key.usage == KeyUsage::LegacyAgent {
                if let Some(Some(credential)) = legacy_env_credential(&tx, &key)? {
                    read_api_credential_meta(&tx, workspace, &credential)?
                        .map(|meta| {
                            Self::credential_secret_location_in_snapshot(&tx, &credential).and_then(
                                |location| {
                                    Ok((meta, location.context("project_key_credential_missing")?))
                                },
                            )
                        })
                        .transpose()?
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        tx.commit()?;
        Ok(result)
    }
    /// Worker-only legacy configuration read. Never include the returned value in a snapshot or log.
    /// Metadata introduction remains value-free; protected enrollment can preserve old plain config semantics.
    pub fn project_key_legacy_configuration_value(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        key_id: &str,
        expected_revision: u64,
    ) -> anyhow::Result<Option<String>> {
        check_scope(&self.conn, workspace)?;
        anyhow::ensure!(
            valid_revision(expected_revision),
            "project_key_revision_invalid"
        );
        let tx = self.conn.unchecked_transaction()?;
        let Some(key) = read_key(&tx, workspace, environment, key_id)? else {
            return Ok(None);
        };
        if key.revision != expected_revision
            || (key.credential_id.is_some() && key.usage != KeyUsage::LegacyAgent)
        {
            return Ok(None);
        }
        let Some(id) = key
            .legacy_reference
            .as_deref()
            .and_then(|reference| reference.strip_prefix("env_var:"))
        else {
            return Ok(None);
        };
        let raw:Option<Option<String>>=tx.query_row("SELECT CASE WHEN typeof(v.plain_value)='text' AND length(CAST(v.plain_value AS BLOB))<=65536 THEN v.plain_value END FROM env_vars v JOIN env_profiles p ON p.id=v.profile_id WHERE v.id=?1 AND p.workspace_id=?2 AND v.key=?3 AND (p.is_production=1 OR p.kind='production')=?4 AND v.kind='plain' AND v.credential_id IS NULL",rusqlite::params![id,workspace,key.name,environment==KeyEnvironment::Production],|row|row.get(0)).optional()?;
        let value = raw
            .map(|value| value.context("project_key_legacy_configuration_limit"))
            .transpose()?;
        if let Some(value) = value.as_ref() {
            Self::validate_env_var_for_persistence(&key.name, &EnvValue::Plain(value.clone()))?;
        }
        tx.commit()?;
        Ok(value)
    }
    /// Introduce existing references once. No plaintext env values, hints, or secret-store coordinates are read.
    pub fn project_key_list(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
    ) -> anyhow::Result<Vec<ProjectKey>> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_scope(&tx, workspace)?;
        Self::project_key_introduce_legacy(&tx, workspace)?;
        let ids = {
            let mut stmt=tx.prepare("SELECT CASE WHEN length(CAST(id AS BLOB))<=256 THEN id END FROM project_keys WHERE workspace_id=?1 AND environment=?2 ORDER BY name,id LIMIT 257")?;
            stmt.query_map((workspace, environment.as_str()), |r| {
                r.get::<_, Option<String>>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        anyhow::ensure!(ids.len() <= PROJECT_KEYS_MAX, "project_key_limit");
        let keys = ids
            .into_iter()
            .map(|id| {
                read_key(
                    &tx,
                    workspace,
                    environment,
                    &id.context("project_key_id_invalid")?,
                )?
                .context("project_key_missing")
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        tx.commit()?;
        Ok(keys)
    }
    fn project_key_introduce_legacy(conn: &Connection, workspace: &str) -> anyhow::Result<()> {
        let mut seen = std::collections::HashSet::new();
        // Materialize only selected bounded metadata columns. No plain_value/Keychain value reads.
        let raw = {
            let mut stmt=conn.prepare("SELECT CASE WHEN length(CAST(v.id AS BLOB))<=256 THEN v.id END,CASE WHEN length(CAST(v.key AS BLOB))<=256 THEN v.key END,CASE WHEN length(CAST(v.credential_id AS BLOB))<=256 THEN v.credential_id END,CASE WHEN length(CAST(p.name AS BLOB))<=255 THEN p.name END,CASE WHEN length(CAST(p.kind AS BLOB))<=64 THEN p.kind END,(p.is_production=1 OR p.kind='production') FROM env_vars v JOIN env_profiles p ON p.id=v.profile_id WHERE p.workspace_id=?1 ORDER BY p.name,v.id LIMIT 513")?;
            stmt.query_map([workspace], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, bool>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        anyhow::ensure!(
            raw.len() <= PROJECT_KEYS_MAX * 2,
            "project_key_legacy_limit"
        );
        let source_files = Self::env_source_files_in_snapshot(conn, workspace)?;
        let now = deppy_core::time::unix_secs_i64();
        for (id, name, credential, file, profile_kind, production) in raw {
            let id = id.context("project_key_legacy_id_invalid")?;
            let name = name.context("project_key_legacy_name_invalid")?;
            let file = file.context("project_key_legacy_source_invalid")?;
            let source = if profile_kind.as_deref() == Some("dotenv")
                && deppy_core::env_sources::valid_files(std::slice::from_ref(&file))
            {
                KeySource::Dotenv { file: file.clone() }
            } else {
                KeySource::Vault
            };
            let priority = source_files
                .iter()
                .position(|f| f == &file)
                .map_or(0, |i| i as i32 + 1);
            let stored = credential.is_some();
            let key = ProjectKey {
                id: uuid::Uuid::new_v4().to_string(),
                workspace_id: workspace.into(),
                environment: if production {
                    KeyEnvironment::Production
                } else {
                    KeyEnvironment::Development
                },
                name,
                credential_id: credential,
                source_baseline_credential_id: None,
                source,
                kind: if stored {
                    KeyKind::ApiToken
                } else {
                    KeyKind::Configuration
                },
                usage: KeyUsage::LegacyAgent,
                delivery: KeyDelivery::Environment,
                source_priority: priority,
                required: false,
                revision: 1,
                last_good_revision: stored.then_some(1),
                last_good_at: None,
                storage_state: if stored {
                    KeyStorageState::Stored
                } else {
                    KeyStorageState::NotStored
                },
                created_at: now,
                updated_at: now,
                source_checked_at: None,
                error_code: None,
                github: None,
                legacy_reference: Some(format!("env_var:{id}")),
            };
            reconcile_legacy_row(conn, &key, &mut seen)?;
        }
        let bindings = {
            let mut stmt=conn.prepare("SELECT CASE WHEN length(CAST(env_name AS BLOB))<=256 THEN env_name END,CASE WHEN length(CAST(credential_id AS BLOB))<=256 THEN credential_id END FROM workspace_credential_env WHERE workspace_id=?1 ORDER BY env_name LIMIT 257")?;
            stmt.query_map([workspace], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        anyhow::ensure!(
            bindings.len() <= PROJECT_KEYS_MAX,
            "project_key_legacy_limit"
        );
        for (name, credential) in bindings {
            let name = name.context("project_key_legacy_name_invalid")?;
            let credential = credential.context("project_key_legacy_id_invalid")?;
            let key = ProjectKey {
                id: uuid::Uuid::new_v4().to_string(),
                workspace_id: workspace.into(),
                environment: KeyEnvironment::Development,
                name: name.clone(),
                credential_id: Some(credential.clone()),
                source_baseline_credential_id: None,
                source: KeySource::Vault,
                kind: KeyKind::ApiToken,
                usage: KeyUsage::LegacyAgent,
                delivery: KeyDelivery::Environment,
                source_priority: 1000,
                required: false,
                revision: 1,
                last_good_revision: Some(1),
                last_good_at: None,
                storage_state: KeyStorageState::Stored,
                created_at: now,
                updated_at: now,
                source_checked_at: None,
                error_code: None,
                github: None,
                legacy_reference: Some(format!("credential_binding:{name}:{credential}")),
            };
            reconcile_legacy_row(conn, &key, &mut seen)?;
        }
        let unbound = {
            let mut stmt=conn.prepare("SELECT CASE WHEN length(CAST(c.id AS BLOB))<=256 THEN c.id END,CASE WHEN length(CAST(c.label AS BLOB))<=256 THEN c.label END FROM credentials c WHERE c.workspace_id=?1 AND c.credential_kind IN ('api_key','token') AND NOT EXISTS(SELECT 1 FROM project_key_owned_credentials o WHERE o.credential_id=c.id) AND NOT EXISTS(SELECT 1 FROM project_keys k WHERE k.credential_id=c.id OR k.auth_credential_id=c.id OR k.source_baseline_credential_id=c.id) AND NOT EXISTS(SELECT 1 FROM workspace_credential_env b WHERE b.workspace_id=?1 AND b.credential_id=c.id) AND NOT EXISTS(SELECT 1 FROM env_vars v JOIN env_profiles p ON p.id=v.profile_id WHERE p.workspace_id=?1 AND v.credential_id=c.id) ORDER BY c.id LIMIT 257")?;
            stmt.query_map([workspace], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        anyhow::ensure!(
            unbound.len() <= PROJECT_KEYS_MAX,
            "project_key_legacy_limit"
        );
        for (id, label) in unbound {
            let credential = id.context("project_key_legacy_id_invalid")?;
            let label = label.context("project_key_legacy_name_invalid")?;
            // Labels can be display prose; derive an explicit deterministic valid name for unbound storage.
            let name = if deppy_core::credential_env::valid_name(&label) {
                label
            } else {
                format!(
                    "STORED_{}",
                    credential
                        .bytes()
                        .map(|b| if b.is_ascii_alphanumeric() {
                            b as char
                        } else {
                            '_'
                        })
                        .collect::<String>()
                )
            };
            let key = ProjectKey {
                id: uuid::Uuid::new_v4().to_string(),
                workspace_id: workspace.into(),
                environment: KeyEnvironment::Development,
                name,
                credential_id: Some(credential.clone()),
                source_baseline_credential_id: None,
                source: KeySource::Vault,
                kind: KeyKind::ApiToken,
                usage: KeyUsage::StoreOnly,
                delivery: KeyDelivery::Environment,
                source_priority: 0,
                required: false,
                revision: 1,
                last_good_revision: Some(1),
                last_good_at: None,
                storage_state: KeyStorageState::Stored,
                created_at: now,
                updated_at: now,
                source_checked_at: None,
                error_code: None,
                github: None,
                legacy_reference: Some(format!("credential:{credential}")),
            };
            reconcile_legacy_row(conn, &key, &mut seen)?;
        }
        let previous = {
            let mut stmt=conn.prepare("SELECT environment,id FROM project_keys WHERE workspace_id=?1 AND json_extract(payload_json,'$.usage')='legacy_agent' AND (legacy_reference LIKE 'env_var:%' OR legacy_reference LIKE 'credential_binding:%') ORDER BY environment,id LIMIT 513")?;
            stmt.query_map([workspace], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        anyhow::ensure!(
            previous.len() <= PROJECT_KEYS_MAX * 2,
            "project_key_legacy_limit"
        );
        for (env, id) in previous {
            if seen.contains(&id) {
                continue;
            }
            let environment = if env == "development" {
                KeyEnvironment::Development
            } else {
                KeyEnvironment::Production
            };
            let mut key =
                read_key(conn, workspace, environment, &id)?.context("project_key_missing")?;
            if key.storage_state == KeyStorageState::MissingSource {
                continue;
            }
            key.revision = next_revision(Some(key.revision))?;
            key.storage_state = KeyStorageState::MissingSource;
            key.error_code = Some("project_key_source_missing".into());
            key.updated_at = now.max(key.updated_at);
            conn.execute(
                "DELETE FROM project_key_github_outbox WHERE key_id=?1",
                [&id],
            )?;
            write_reconciled_key(conn, &key)?;
        }
        conn.execute(
            "INSERT INTO project_key_legacy_introduction(workspace_id) VALUES (?1) ON CONFLICT DO NOTHING",
            [workspace],
        )?;
        Ok(())
    }
    /// Metadata-only ownership claims shared by runtime filtering and legacy writer admission.
    pub fn project_key_managed_ids(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
    ) -> anyhow::Result<Vec<String>> {
        check_scope(&self.conn, workspace)?;
        let sql = format!(
            "SELECT CASE WHEN length(CAST(k.id AS BLOB))<=256 THEN k.id END FROM project_keys k WHERE k.workspace_id=?1 AND k.environment=?2 AND {MANAGED_KEY_SQL} ORDER BY k.id LIMIT 257"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map((workspace, environment.as_str()), |row| {
                row.get::<_, Option<String>>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        anyhow::ensure!(rows.len() <= PROJECT_KEYS_MAX, "project_key_limit");
        rows.into_iter()
            .map(|id| {
                let id = id.context("project_key_metadata_invalid")?;
                anyhow::ensure!(valid_id(&id), "project_key_metadata_invalid");
                Ok(id)
            })
            .collect()
    }
    /// Legacy dotenv-worker writes only; explicit Advanced edits keep the ordinary upsert API.
    pub fn project_key_dotenv_upsert_unmanaged(
        &self,
        profile_id: &str,
        key: &str,
        value: &EnvValue,
    ) -> anyhow::Result<bool> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if dotenv_row_managed(&tx, profile_id, key)? {
            return Ok(false);
        }
        Self::validate_env_var_for_persistence(key, value)?;
        let credential = match value {
            EnvValue::Secret { credential_id } => Some(credential_id.as_str()),
            EnvValue::Plain(_) => None,
        };
        remove_obsolete_unowned_dotenv_projection(&tx, profile_id, key, credential, false)?;
        Self::upsert_env_var_in_transaction(&tx, profile_id, key, value)?;
        tx.commit()?;
        Ok(true)
    }
    pub fn project_key_dotenv_delete_unmanaged(
        &self,
        profile_id: &str,
        key: &str,
    ) -> anyhow::Result<bool> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if dotenv_row_managed(&tx, profile_id, key)? {
            return Ok(false);
        }
        remove_obsolete_unowned_dotenv_projection(&tx, profile_id, key, None, true)?;
        tx.execute(
            "DELETE FROM env_vars WHERE profile_id=?1 AND key=?2",
            (profile_id, key),
        )?;
        tx.commit()?;
        Ok(true)
    }
    pub fn project_key_source_exclusions(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
    ) -> anyhow::Result<Vec<ProjectKeySourceExclusion>> {
        check_scope(&self.conn, workspace)?;
        let mut stmt=self.conn.prepare("SELECT source_json,name FROM project_key_source_exclusions WHERE workspace_id=?1 AND environment=?2 ORDER BY source_json,name LIMIT 257")?;
        let rows = stmt
            .query_map((workspace, environment.as_str()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        anyhow::ensure!(
            rows.len() <= PROJECT_KEYS_MAX,
            "project_key_source_exclusion_limit"
        );
        rows.into_iter()
            .map(|(json, name)| {
                let exclusion = ProjectKeySourceExclusion {
                    source: decode(Some(json))?,
                    name,
                };
                exclusion.validate().map_err(anyhow::Error::msg)?;
                Ok(exclusion)
            })
            .collect()
    }
    pub fn project_key_source_exclusion_clear(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        source: &KeySource,
        name: &str,
    ) -> anyhow::Result<bool> {
        check_scope(&self.conn, workspace)?;
        ProjectKeySourceExclusion {
            source: source.clone(),
            name: name.into(),
        }
        .validate()
        .map_err(anyhow::Error::msg)?;
        Ok(self.conn.execute("DELETE FROM project_key_source_exclusions WHERE workspace_id=?1 AND environment=?2 AND source_json=?3 AND name=?4",(workspace,environment.as_str(),encode(source,8192)?,name))?==1)
    }
    #[allow(clippy::too_many_arguments)] // Keep scope, metadata CAS, and protected journal coordinates explicit.
    pub fn project_key_file_journal_begin(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        key: &ProjectKey,
        expected_revision: Option<u64>,
        journal_id: &str,
        protected_slot: &str,
        now: i64,
    ) -> anyhow::Result<ProjectKeyCas<ProjectKeyFileJournal>> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let ProjectKeyCas::Stale { current_revision } =
            check_save(&tx, workspace, environment, key, expected_revision)?
        {
            return Ok(ProjectKeyCas::Stale { current_revision });
        }
        anyhow::ensure!(key.source.is_file(), "project_key_journal_invalid");
        validate_owned_physical_secret_slot(journal_id, protected_slot)?;
        let staged:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM physical_secret_slot_ledger WHERE physical_slot=?1 AND logical_credential_id=?2 AND state='staging')",(protected_slot,journal_id),|r|r.get(0))?;
        anyhow::ensure!(staged, "project_key_journal_slot_invalid");
        let count:i64=tx.query_row("SELECT COUNT(*) FROM project_key_file_journals WHERE workspace_id=?1 AND environment=?2",(workspace,environment.as_str()),|r|r.get(0))?;
        anyhow::ensure!(count < 16, "project_key_journal_limit");
        if let Some(auth) = key
            .github
            .as_ref()
            .and_then(|g| g.target.auth_credential_id.as_deref())
        {
            anyhow::ensure!(
                auth_credential_eligible(&tx, workspace, environment, auth)?,
                "project_key_auth_environment_invalid"
            );
        }
        tx.execute("INSERT INTO project_key_file_journals(id,workspace_id,environment,key_id,source_json,name,expected_revision,protected_slot,phase,created_at,owner_json) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'prepared',?9,?10)",rusqlite::params![journal_id,workspace,environment.as_str(),key.id,encode(&key.source,8192)?,key.name,expected_revision.map(|r|r as i64),protected_slot,now,deppy_core::process_identity::ProcessIdentity::capture(std::process::id()).map(|owner|encode(&owner,256)).transpose()?])?;
        let journal = read_file_journal(&tx, workspace, environment, journal_id)?
            .context("project_key_journal_missing")?;
        tx.commit()?;
        Ok(ProjectKeyCas::Committed(journal))
    }
    pub fn project_key_file_journal_get(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        id: &str,
    ) -> anyhow::Result<Option<ProjectKeyFileJournal>> {
        check_scope(&self.conn, workspace)?;
        read_file_journal(&self.conn, workspace, environment, id)
    }
    pub fn project_key_file_journal_source_pending(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        source: &KeySource,
    ) -> anyhow::Result<bool> {
        check_scope(&self.conn, workspace)?;
        source.validate().map_err(anyhow::Error::msg)?;
        anyhow::ensure!(source.is_file(), "project_key_journal_invalid");
        Ok(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM project_key_file_journals WHERE workspace_id=?1 AND source_json=?3 AND phase IN ('prepared','file_applied'))",(workspace,environment.as_str(),encode(source,8192)?),|r|r.get(0))?)
    }
    pub fn project_key_file_journal_mark_applied(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        id: &str,
    ) -> anyhow::Result<bool> {
        check_scope(&self.conn, workspace)?;
        anyhow::ensure!(valid_id(id), "project_key_journal_invalid");
        Ok(self.conn.execute("UPDATE project_key_file_journals SET phase='file_applied' WHERE workspace_id=?1 AND environment=?2 AND id=?3 AND phase='prepared'",(workspace,environment.as_str(),id))?==1)
    }
    pub fn project_key_file_journals_page(
        &self,
        limit: usize,
        after: Option<&str>,
    ) -> anyhow::Result<Vec<ProjectKeyFileJournal>> {
        anyhow::ensure!(
            (1..=16).contains(&limit) && after.is_none_or(valid_id),
            "project_key_journal_limit"
        );
        let tx = self.conn.unchecked_transaction()?;
        let rows = {
            let mut stmt=tx.prepare("SELECT workspace_id,environment,id FROM project_key_file_journals WHERE (?1 IS NULL OR id>?1) ORDER BY id LIMIT ?2")?;
            stmt.query_map((after, limit as i64), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        let journals = rows
            .into_iter()
            .map(|(workspace, env, id)| {
                let environment = if env == "development" {
                    KeyEnvironment::Development
                } else {
                    KeyEnvironment::Production
                };
                read_file_journal(&tx, &workspace, environment, &id)?
                    .context("project_key_journal_missing")
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        tx.commit()?;
        Ok(journals)
    }
    pub fn project_key_file_journal_finish(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        id: &str,
        expected_phase: ProjectKeyFileJournalPhase,
    ) -> anyhow::Result<bool> {
        check_scope(&self.conn, workspace)?;
        anyhow::ensure!(valid_id(id), "project_key_journal_invalid");
        Ok(self.conn.execute("DELETE FROM project_key_file_journals WHERE workspace_id=?1 AND environment=?2 AND id=?3 AND phase=?4",(workspace,environment.as_str(),id,expected_phase.as_str()))?==1)
    }

    pub fn project_key_settings(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
    ) -> anyhow::Result<ProjectKeySettings> {
        check_scope(&self.conn, workspace)?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let json:Option<(Option<String>,i64,bool)>=tx.query_row("SELECT CASE WHEN length(CAST(payload_json AS BLOB))<=16384 THEN payload_json END,revision,watch_enabled FROM project_key_settings WHERE workspace_id=?1 AND environment=?2",(workspace,environment.as_str()),|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let settings = if let Some((json, revision, watch_enabled)) = json {
            let settings: ProjectKeySettings = decode(json)?;
            settings.validate().map_err(anyhow::Error::msg)?;
            anyhow::ensure!(
                settings.workspace_id == workspace
                    && settings.environment == environment
                    && settings.revision == revision as u64
                    && settings.watch_enabled == watch_enabled,
                "project_key_owner_invalid"
            );
            settings
        } else {
            let settings = ProjectKeySettings {
                workspace_id: workspace.into(),
                environment,
                revision: 1,
                watch_enabled: environment == KeyEnvironment::Development,
                sources: if environment == KeyEnvironment::Development {
                    Self::env_source_files_in_snapshot(&tx, workspace)?
                        .into_iter()
                        .map(|file| KeySource::Dotenv { file })
                        .collect()
                } else {
                    vec![]
                },
            };
            settings.validate().map_err(anyhow::Error::msg)?;
            tx.execute("INSERT INTO project_key_settings(workspace_id,environment,revision,watch_enabled,payload_json) VALUES (?1,?2,1,?3,?4)",rusqlite::params![workspace,environment.as_str(),settings.watch_enabled,encode(&settings,PROJECT_KEY_METADATA_BYTES_MAX)?])?;
            settings
        };
        tx.commit()?;
        Ok(settings)
    }
    pub fn project_key_save_settings(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        settings: &ProjectKeySettings,
        expected_revision: Option<u64>,
    ) -> anyhow::Result<ProjectKeyCas<ProjectKeySettings>> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_scope(&tx, workspace)?;
        anyhow::ensure!(
            settings.workspace_id == workspace && settings.environment == environment,
            "project_key_owner_invalid"
        );
        let current:Option<i64>=tx.query_row("SELECT revision FROM project_key_settings WHERE workspace_id=?1 AND environment=?2",(workspace,environment.as_str()),|r|r.get(0)).optional()?;
        let current = current.map(|r| r as u64);
        if current != expected_revision {
            return Ok(ProjectKeyCas::Stale {
                current_revision: current,
            });
        }
        let mut saved = settings.clone();
        saved.revision = next_revision(current)?;
        saved.validate().map_err(anyhow::Error::msg)?;
        tx.execute("INSERT INTO project_key_settings(workspace_id,environment,revision,watch_enabled,payload_json) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(workspace_id,environment) DO UPDATE SET revision=excluded.revision,watch_enabled=excluded.watch_enabled,payload_json=excluded.payload_json",rusqlite::params![workspace,environment.as_str(),saved.revision as i64,saved.watch_enabled,encode(&saved,PROJECT_KEY_METADATA_BYTES_MAX)?])?;
        if environment == KeyEnvironment::Development {
            let files = saved
                .sources
                .iter()
                .filter_map(|source| {
                    if let KeySource::Dotenv { file } = source {
                        Some(file.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            Self::validate_env_source_files(&files)?;
            tx.execute("INSERT INTO workspace_env_sources(workspace_id,files_json) VALUES (?1,?2) ON CONFLICT(workspace_id) DO UPDATE SET files_json=excluded.files_json",(workspace,serde_json::to_string(&files)?))?;
        }
        tx.commit()?;
        Ok(ProjectKeyCas::Committed(saved))
    }
    /// Complete-or-error convenience; use the cursor API for large installations.
    pub fn project_key_watch_scopes(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<ProjectKeySettings>> {
        anyhow::ensure!((1..=128).contains(&limit), "project_key_watch_limit");
        let rows = self.project_key_watch_scopes_page_raw(limit + 1, None)?;
        anyhow::ensure!(rows.len() <= limit, "project_key_watch_limit");
        Ok(rows)
    }
    /// Stable ordered pages allow an inactive-project watcher to visit every configured scope.
    pub fn project_key_watch_scopes_page(
        &self,
        limit: usize,
        after: Option<(&str, KeyEnvironment)>,
    ) -> anyhow::Result<Vec<ProjectKeySettings>> {
        anyhow::ensure!((1..=128).contains(&limit), "project_key_watch_limit");
        if let Some((id, _)) = after {
            anyhow::ensure!(valid_id(id), "project_key_workspace_invalid");
        }
        self.project_key_watch_scopes_page_raw(limit, after)
    }
    fn project_key_watch_scopes_page_raw(
        &self,
        limit: usize,
        after: Option<(&str, KeyEnvironment)>,
    ) -> anyhow::Result<Vec<ProjectKeySettings>> {
        let mut stmt=self.conn.prepare("SELECT CASE WHEN length(CAST(payload_json AS BLOB))<=16384 THEN payload_json END,workspace_id,environment,revision,watch_enabled FROM project_key_settings WHERE watch_enabled=1 AND (?2 IS NULL OR workspace_id>?2 OR (workspace_id=?2 AND environment>?3)) ORDER BY workspace_id,environment LIMIT ?1")?;
        let rows = stmt
            .query_map(
                rusqlite::params![
                    limit as i64,
                    after.map(|v| v.0),
                    after.map(|v| v.1.as_str())
                ],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, bool>(4)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(json, owner, environment, revision, watch)| {
                let settings: ProjectKeySettings = decode(json)?;
                settings.validate().map_err(anyhow::Error::msg)?;
                anyhow::ensure!(
                    settings.workspace_id == owner
                        && settings.environment.as_str() == environment
                        && settings.revision == revision as u64
                        && settings.watch_enabled == watch,
                    "project_key_settings_column_mismatch"
                );
                Ok(settings)
            })
            .collect()
    }
    /// Explicit API-token authentication choices. Hints and secret-store coordinates are omitted.
    pub fn project_key_auth_credentials(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
    ) -> anyhow::Result<Vec<CredentialMeta>> {
        let rows = self.project_key_auth_credentials_page_raw(
            workspace,
            environment,
            PROJECT_KEYS_MAX + 1,
            None,
        )?;
        anyhow::ensure!(
            rows.len() <= PROJECT_KEYS_MAX,
            "project_key_auth_credential_limit"
        );
        Ok(rows)
    }
    /// Stable ID pages. Explicit managed vault authentication tokens stay in their own environment.
    pub fn project_key_auth_credentials_page(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        limit: usize,
        after: Option<&str>,
    ) -> anyhow::Result<Vec<CredentialMeta>> {
        anyhow::ensure!(
            (1..=PROJECT_KEYS_MAX).contains(&limit) && after.is_none_or(valid_id),
            "project_key_auth_credential_limit"
        );
        self.project_key_auth_credentials_page_raw(workspace, environment, limit, after)
    }
    fn project_key_auth_credentials_page_raw(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        limit: usize,
        after: Option<&str>,
    ) -> anyhow::Result<Vec<CredentialMeta>> {
        check_scope(&self.conn, workspace)?;
        let tx = self.conn.unchecked_transaction()?;
        let ids = {
            let mut stmt=tx.prepare("SELECT CASE WHEN length(CAST(c.id AS BLOB))<=256 THEN c.id END FROM credentials c WHERE (c.workspace_id IS NULL OR c.workspace_id=?1) AND c.credential_kind IN ('api_key','token') AND NOT EXISTS(SELECT 1 FROM project_key_owned_credentials o WHERE o.credential_id=c.id AND (o.workspace_id<>?1 OR o.environment<>?4)) AND (?3 IS NULL OR c.id>?3) AND (c.workspace_id IS NULL OR NOT EXISTS(SELECT 1 FROM project_keys k WHERE k.credential_id=c.id OR k.source_baseline_credential_id=c.id) OR EXISTS(SELECT 1 FROM project_keys k WHERE k.credential_id=c.id AND json_extract(k.payload_json,'$.usage')='store_only' AND json_extract(k.payload_json,'$.kind')='api_token' AND json_extract(k.payload_json,'$.source.type')='vault' AND (k.environment=?4 OR COALESCE(k.legacy_reference,'') LIKE 'credential:%'))) ORDER BY c.id LIMIT ?2")?;
            stmt.query_map(
                rusqlite::params![workspace, limit as i64, after, environment.as_str()],
                |r| r.get::<_, Option<String>>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?
        };
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(meta) = read_api_credential_meta(
                &tx,
                workspace,
                &id.context("project_key_credential_invalid")?,
            )? {
                result.push(meta);
            }
        }
        tx.commit()?;
        Ok(result)
    }
    /// Exact environment-aware authentication read. A managed production token cannot authenticate development.
    pub fn project_key_auth_credential(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        id: &str,
    ) -> anyhow::Result<Option<(CredentialMeta, CredentialSecretLocation)>> {
        check_scope(&self.conn, workspace)?;
        let tx = self.conn.unchecked_transaction()?;
        let result = if auth_credential_eligible(&tx, workspace, environment, id)? {
            if let Some(meta) = read_api_credential_meta(&tx, workspace, id)? {
                Some((
                    meta,
                    Self::credential_secret_location_in_snapshot(&tx, id)?
                        .context("project_key_credential_missing")?,
                ))
            } else {
                None
            }
        } else {
            None
        };
        tx.commit()?;
        Ok(result)
    }
    /// Exact bounded owner/global lookup; never returns a foreign-project credential or OAuth kind.
    pub fn project_key_credential(
        &self,
        workspace: &str,
        id: &str,
    ) -> anyhow::Result<Option<(CredentialMeta, CredentialSecretLocation)>> {
        check_scope(&self.conn, workspace)?;
        anyhow::ensure!(valid_id(id), "project_key_credential_invalid");
        let tx = self.conn.unchecked_transaction()?;
        let result = if let Some(meta) = read_api_credential_meta(&tx, workspace, id)? {
            Some((
                meta,
                Self::credential_secret_location_in_snapshot(&tx, id)?
                    .context("project_key_credential_missing")?,
            ))
        } else {
            None
        };
        tx.commit()?;
        Ok(result)
    }
    pub fn project_key_recipe_list(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
    ) -> anyhow::Result<Vec<ProjectBuildRecipe>> {
        check_scope(&self.conn, workspace)?;
        let mut stmt=self.conn.prepare("SELECT CASE WHEN length(CAST(payload_json AS BLOB))<=16384 THEN payload_json END FROM project_key_build_recipes WHERE workspace_id=?1 AND environment=?2 ORDER BY id LIMIT 33")?;
        let rows = stmt
            .query_map((workspace, environment.as_str()), |r| {
                r.get::<_, Option<String>>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        anyhow::ensure!(
            rows.len() <= PROJECT_KEY_RECIPES_MAX,
            "project_key_recipe_limit"
        );
        rows.into_iter()
            .map(|json| {
                let recipe: ProjectBuildRecipe = decode(json)?;
                recipe.validate().map_err(anyhow::Error::msg)?;
                anyhow::ensure!(
                    recipe.workspace_id == workspace && recipe.environment == environment,
                    "project_key_owner_invalid"
                );
                Ok(recipe)
            })
            .collect()
    }
    pub fn project_key_recipe_save(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        recipe: &ProjectBuildRecipe,
        expected_revision: Option<u64>,
    ) -> anyhow::Result<ProjectKeyCas<ProjectBuildRecipe>> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_scope(&tx, workspace)?;
        anyhow::ensure!(
            recipe.workspace_id == workspace
                && recipe.environment == environment
                && valid_id(&recipe.id),
            "project_key_owner_invalid"
        );
        let current = scoped_revision(
            &tx,
            "project_key_build_recipes",
            workspace,
            environment,
            &recipe.id,
        )?;
        if current != expected_revision {
            return Ok(ProjectKeyCas::Stale {
                current_revision: current,
            });
        }
        if current.is_none() {
            let count:i64=tx.query_row("SELECT COUNT(*) FROM project_key_build_recipes WHERE workspace_id=?1 AND environment=?2",(workspace,environment.as_str()),|r|r.get(0))?;
            anyhow::ensure!(
                count < PROJECT_KEY_RECIPES_MAX as i64,
                "project_key_recipe_limit"
            );
        }
        let mut saved = recipe.clone();
        saved.revision = next_revision(current)?;
        saved.validate().map_err(anyhow::Error::msg)?;
        tx.execute("INSERT INTO project_key_build_recipes(id,workspace_id,environment,revision,payload_json) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET revision=excluded.revision,payload_json=excluded.payload_json",rusqlite::params![saved.id,workspace,environment.as_str(),saved.revision as i64,encode(&saved,PROJECT_KEY_METADATA_BYTES_MAX)?])?;
        tx.commit()?;
        Ok(ProjectKeyCas::Committed(saved))
    }
    pub fn project_key_recipe_delete(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        id: &str,
        expected_revision: u64,
    ) -> anyhow::Result<ProjectKeyCas<()>> {
        check_scope(&self.conn, workspace)?;
        anyhow::ensure!(
            valid_id(id) && valid_revision(expected_revision),
            "project_key_input_invalid"
        );
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current =
            scoped_revision(&tx, "project_key_build_recipes", workspace, environment, id)?;
        let result = if current != Some(expected_revision) {
            ProjectKeyCas::Stale {
                current_revision: current,
            }
        } else {
            tx.execute("DELETE FROM project_key_build_recipes WHERE id=?1", [id])?;
            ProjectKeyCas::Committed(())
        };
        tx.commit()?;
        Ok(result)
    }
    pub fn project_key_receipt_save(&self, receipt: &ProjectBuildReceipt) -> anyhow::Result<()> {
        receipt.validate().map_err(anyhow::Error::msg)?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_scope(&tx, &receipt.workspace_id)?;
        let existing:Option<(String,String,Option<String>)>=tx.query_row("SELECT workspace_id,environment,CASE WHEN length(CAST(payload_json AS BLOB))<=131072 THEN payload_json END FROM project_key_build_receipts WHERE id=?1",[&receipt.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if let Some((owner, env, json)) = existing {
            anyhow::ensure!(
                owner == receipt.workspace_id && env == receipt.environment.as_str(),
                "project_key_owner_invalid"
            );
            let old: ProjectBuildReceipt = decode(json)?;
            anyhow::ensure!(
                old.owner == receipt.owner
                    && old.used_keys == receipt.used_keys
                    && old.started_at == receipt.started_at
                    && old.recipe_id == receipt.recipe_id
                    && old.finished_at.is_none(),
                "project_key_receipt_snapshot_immutable"
            );
        } else {
            if let Some(recipe_id) = receipt.recipe_id.as_deref() {
                anyhow::ensure!(
                    scoped_revision(
                        &tx,
                        "project_key_build_recipes",
                        &receipt.workspace_id,
                        receipt.environment,
                        recipe_id
                    )?
                    .is_some(),
                    "project_key_recipe_missing"
                );
            }
            for used in &receipt.used_keys {
                let key = read_key(
                    &tx,
                    &receipt.workspace_id,
                    receipt.environment,
                    &used.key_id,
                )?
                .context("project_key_receipt_key_missing")?;
                anyhow::ensure!(
                    key.name == used.name && key.last_good_revision == Some(used.revision),
                    "project_key_receipt_generation_invalid"
                );
            }
        }
        tx.execute("INSERT INTO project_key_build_receipts(id,workspace_id,environment,started_at,payload_json) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET payload_json=excluded.payload_json",rusqlite::params![receipt.id,receipt.workspace_id,receipt.environment.as_str(),receipt.started_at,encode(receipt,128*1024)?])?;
        // Keep recent history bounded, while running receipts remain available for completion.
        tx.execute("DELETE FROM project_key_build_receipts WHERE workspace_id=?1 AND environment=?2 AND id IN (SELECT id FROM project_key_build_receipts WHERE workspace_id=?1 AND environment=?2 AND json_extract(payload_json,'$.finished_at') IS NOT NULL ORDER BY started_at DESC,id LIMIT -1 OFFSET 100)",(&receipt.workspace_id,receipt.environment.as_str()))?;
        let running:i64=tx.query_row("SELECT COUNT(*) FROM project_key_build_receipts WHERE workspace_id=?1 AND environment=?2 AND json_extract(payload_json,'$.finished_at') IS NULL",(&receipt.workspace_id,receipt.environment.as_str()),|r|r.get(0))?;
        anyhow::ensure!(running <= 32, "project_key_running_receipt_limit");
        tx.commit()?;
        Ok(())
    }
    /// Independent startup recovery scans all running builds, including inactive project scopes.
    pub fn project_key_running_receipts_page(
        &self,
        limit: usize,
        after: Option<&str>,
    ) -> anyhow::Result<Vec<ProjectBuildReceipt>> {
        anyhow::ensure!(
            (1..=16).contains(&limit) && after.is_none_or(valid_id),
            "project_key_receipt_limit"
        );
        let mut stmt=self.conn.prepare("SELECT CASE WHEN length(CAST(payload_json AS BLOB))<=131072 THEN payload_json END FROM project_key_build_receipts WHERE json_extract(payload_json,'$.finished_at') IS NULL AND (?1 IS NULL OR id>?1) ORDER BY id LIMIT ?2")?;
        let rows = stmt
            .query_map((after, limit as i64), |r| r.get::<_, Option<String>>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|json| {
                let receipt: ProjectBuildReceipt = decode(json)?;
                receipt.validate().map_err(anyhow::Error::msg)?;
                Ok(receipt)
            })
            .collect()
    }
    /// The caller must positively establish that this exact process identity is dead; unknown probes are not death.
    pub fn project_key_receipt_recover_abandoned(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        id: &str,
        expected_owner: deppy_core::process_identity::ProcessIdentity,
        now: i64,
    ) -> anyhow::Result<bool> {
        check_scope(&self.conn, workspace)?;
        anyhow::ensure!(valid_id(id), "project_key_receipt_invalid");
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let json:Option<Option<String>>=tx.query_row("SELECT CASE WHEN length(CAST(payload_json AS BLOB))<=131072 THEN payload_json END FROM project_key_build_receipts WHERE workspace_id=?1 AND environment=?2 AND id=?3",(workspace,environment.as_str(),id),|r|r.get(0)).optional()?;
        let Some(json) = json else {
            return Ok(false);
        };
        let mut receipt: ProjectBuildReceipt = decode(json)?;
        receipt.validate().map_err(anyhow::Error::msg)?;
        if receipt.owner != Some(expected_owner) || receipt.finished_at.is_some() {
            return Ok(false);
        }
        receipt.finished_at = Some(now.max(receipt.started_at));
        receipt.exit_code = Some(-1);
        tx.execute(
            "UPDATE project_key_build_receipts SET payload_json=?2 WHERE id=?1",
            (&receipt.id, encode(&receipt, 128 * 1024)?),
        )?;
        tx.execute("DELETE FROM project_key_build_receipts WHERE workspace_id=?1 AND environment=?2 AND id IN (SELECT id FROM project_key_build_receipts WHERE workspace_id=?1 AND environment=?2 AND json_extract(payload_json,'$.finished_at') IS NOT NULL ORDER BY started_at DESC,id LIMIT -1 OFFSET 100)",(workspace,environment.as_str()))?;
        tx.commit()?;
        Ok(true)
    }
    pub fn project_key_receipt_list(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        limit: usize,
    ) -> anyhow::Result<Vec<ProjectBuildReceipt>> {
        check_scope(&self.conn, workspace)?;
        anyhow::ensure!(
            (1..=PROJECT_KEY_RECEIPTS_MAX).contains(&limit),
            "project_key_receipt_limit"
        );
        let mut stmt=self.conn.prepare("SELECT CASE WHEN length(CAST(payload_json AS BLOB))<=131072 THEN payload_json END FROM project_key_build_receipts WHERE workspace_id=?1 AND environment=?2 ORDER BY started_at DESC,id LIMIT ?3")?;
        let rows = stmt
            .query_map(
                rusqlite::params![workspace, environment.as_str(), limit as i64],
                |r| r.get::<_, Option<String>>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|json| {
                let receipt: ProjectBuildReceipt = decode(json)?;
                receipt.validate().map_err(anyhow::Error::msg)?;
                anyhow::ensure!(
                    receipt.workspace_id == workspace && receipt.environment == environment,
                    "project_key_owner_invalid"
                );
                Ok(receipt)
            })
            .collect()
    }
    pub fn project_key_github_enqueue(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        key_id: &str,
        expected_revision: u64,
        allow_external_change: bool,
        now: i64,
    ) -> anyhow::Result<bool> {
        check_scope(&self.conn, workspace)?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let Some(mut key) = read_key(&tx, workspace, environment, key_id)? else {
            return Ok(false);
        };
        if key.revision != expected_revision {
            return Ok(false);
        }
        let queued = enqueue(&tx, &mut key, allow_external_change, now)?;
        tx.commit()?;
        Ok(queued)
    }
    /// Recover missing automatic work after legacy credential publication, including watch-disabled scopes.
    /// Existing retry/claim tickets are untouched; successful inserts disappear from the next bounded page.
    pub fn project_key_github_reconcile_pending(
        &self,
        now: i64,
        limit: usize,
    ) -> anyhow::Result<usize> {
        anyhow::ensure!(
            now >= 0 && (1..=128).contains(&limit),
            "project_key_outbox_reconcile_limit"
        );
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let candidates = {
            // A failed candidate re-enters recovery only after its auth becomes eligible. Invalid
            // first-page entries cannot repeatedly starve healthy work in later bounded batches.
            let eligible_auth = AUTH_ELIGIBILITY_SQL
                .replacen("SELECT ", "", 1)
                .replace("?1", "candidate.auth_credential_id")
                .replace("?2", "candidate.workspace_id")
                .replace("?3", "candidate.environment");
            let sql = format!(
                "SELECT workspace_id,environment,id FROM project_keys candidate WHERE json_extract(payload_json,'$.storage_state')='stored' AND json_extract(payload_json,'$.last_good_revision')=revision AND (json_extract(payload_json,'$.github.state')='pending' OR (json_extract(payload_json,'$.github.state')='failed' AND json_extract(payload_json,'$.github.error_code')='project_key_github_auth_failed' AND (auth_credential_id IS NULL OR {eligible_auth}))) AND json_extract(payload_json,'$.github.target.auto_send')=1 AND NOT EXISTS(SELECT 1 FROM project_key_github_outbox q WHERE q.key_id=candidate.id) ORDER BY workspace_id,environment,id LIMIT ?1"
            );
            let mut stmt = tx.prepare(&sql)?;
            stmt.query_map([limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        let mut count = 0;
        for (workspace, environment, id) in candidates {
            let environment = match environment.as_str() {
                "development" => KeyEnvironment::Development,
                "production" => KeyEnvironment::Production,
                _ => anyhow::bail!("project_key_environment_invalid"),
            };
            let mut key =
                read_key(&tx, &workspace, environment, &id)?.context("project_key_missing")?;
            if let Some(auth) = key
                .github
                .as_ref()
                .and_then(|github| github.target.auth_credential_id.as_deref())
                && !auth_credential_eligible(&tx, &workspace, environment, auth)?
            {
                fail_github_auth(&tx, &mut key, now)?;
                continue;
            }
            count += usize::from(enqueue(&tx, &mut key, false, now)?);
        }
        tx.commit()?;
        Ok(count)
    }
    pub fn project_key_github_claim(
        &self,
        now: i64,
        lease_seconds: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<GitHubOutboxEntry>> {
        anyhow::ensure!(
            (1..=16).contains(&limit) && (1..=300).contains(&lease_seconds),
            "project_key_outbox_claim_limit"
        );
        let until = now
            .checked_add(lease_seconds)
            .context("project_key_time_invalid")?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let rows = {
            let mut stmt=tx.prepare("SELECT CASE WHEN length(CAST(payload_json AS BLOB))<=16384 THEN payload_json END FROM project_key_github_outbox WHERE next_attempt_at<=?1 AND (claim_until IS NULL OR claim_until<=?1) ORDER BY next_attempt_at,key_id LIMIT ?2")?;
            stmt.query_map((now, limit as i64), |r| r.get::<_, Option<String>>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut claimed = vec![];
        for json in rows {
            let mut entry: GitHubOutboxEntry = decode(json)?;
            entry.target.validate().map_err(anyhow::Error::msg)?;
            anyhow::ensure!(
                valid_id(&entry.id)
                    && valid_id(&entry.key_id)
                    && valid_id(&entry.workspace_id)
                    && valid_revision(entry.key_revision),
                "project_key_outbox_invalid"
            );
            let key = read_key(&tx, &entry.workspace_id, entry.environment, &entry.key_id)?;
            if !key.as_ref().is_some_and(|k| {
                k.revision == entry.key_revision
                    && k.storage_state == KeyStorageState::Stored
                    && k.last_good_revision == Some(entry.key_revision)
                    && k.github.as_ref().is_some_and(|g| {
                        g.target == entry.target && g.state != GitHubRemoteState::ExternalChange
                    })
            }) {
                tx.execute(
                    "DELETE FROM project_key_github_outbox WHERE id=?1",
                    [&entry.id],
                )?;
                continue;
            }
            let mut key = key.context("project_key_missing")?;
            if let Some(auth) = key
                .github
                .as_ref()
                .and_then(|github| github.target.auth_credential_id.as_deref())
                && !auth_credential_eligible(&tx, &entry.workspace_id, entry.environment, auth)?
            {
                fail_github_auth(&tx, &mut key, now)?;
                continue;
            }
            entry.claim_token = Some(uuid::Uuid::new_v4().to_string());
            entry.claim_until = Some(until);
            entry.attempts = entry.attempts.saturating_add(1);
            tx.execute(
                "UPDATE project_key_github_outbox SET claim_until=?2,payload_json=?3 WHERE id=?1",
                rusqlite::params![
                    entry.id,
                    until,
                    encode(&entry, PROJECT_KEY_METADATA_BYTES_MAX)?
                ],
            )?;
            claimed.push(entry);
        }
        tx.commit()?;
        Ok(claimed)
    }
    pub fn project_key_github_ack(
        &self,
        entry: &GitHubOutboxEntry,
        outcome: GitHubSendOutcome,
    ) -> anyhow::Result<bool> {
        self.project_key_github_ack_at(entry, outcome, deppy_core::time::unix_secs_i64())
    }
    /// Explicit clock API supports deterministic lease/restart tests and worker time snapshots.
    pub fn project_key_github_ack_at(
        &self,
        entry: &GitHubOutboxEntry,
        outcome: GitHubSendOutcome,
        now: i64,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(
            valid_id(&entry.id) && valid_id(&entry.key_id) && valid_id(&entry.workspace_id),
            "project_key_outbox_invalid"
        );
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current:Option<Option<String>>=tx.query_row("SELECT CASE WHEN length(CAST(payload_json AS BLOB))<=16384 THEN payload_json END FROM project_key_github_outbox WHERE id=?1 AND key_id=?2 AND workspace_id=?3 AND environment=?4",rusqlite::params![entry.id,entry.key_id,entry.workspace_id,entry.environment.as_str()],|r|r.get(0)).optional()?;
        let Some(current) = current else {
            return Ok(false);
        };
        let mut current: GitHubOutboxEntry = decode(current)?;
        if current != *entry
            || current.claim_token.is_none()
            || current.claim_until.is_none_or(|until| until <= now)
        {
            return Ok(false);
        }
        let Some(mut key) = read_key(&tx, &entry.workspace_id, entry.environment, &entry.key_id)?
        else {
            return Ok(false);
        };
        if key.revision != entry.key_revision {
            return Ok(false);
        }
        let Some(github) = key.github.as_mut() else {
            return Ok(false);
        };
        if github.target != entry.target || github.state == GitHubRemoteState::ExternalChange {
            return Ok(false);
        }
        match outcome {
            GitHubSendOutcome::Sent { remote_updated_at } => {
                anyhow::ensure!(
                    remote_updated_at
                        .as_ref()
                        .is_none_or(|s| valid_metadata_text(s, 128)),
                    "project_key_remote_time_invalid"
                );
                github.state = GitHubRemoteState::Sent;
                github.last_sent_revision = Some(entry.key_revision);
                github.remote_updated_at = remote_updated_at;
                github.error_code = None;
                tx.execute(
                    "DELETE FROM project_key_github_outbox WHERE id=?1",
                    [&entry.id],
                )?;
            }
            GitHubSendOutcome::Retry {
                error_code,
                next_attempt_at,
            } => {
                anyhow::ensure!(
                    valid_error_code(Some(&error_code)) && next_attempt_at > now,
                    "project_key_retry_invalid"
                );
                github.state = GitHubRemoteState::Failed;
                github.error_code = Some(error_code.clone());
                current.error_code = Some(error_code);
                current.next_attempt_at = next_attempt_at;
                current.claim_token = None;
                current.claim_until = None;
                tx.execute("UPDATE project_key_github_outbox SET next_attempt_at=?2,claim_until=NULL,payload_json=?3 WHERE id=?1",rusqlite::params![entry.id,next_attempt_at,encode(&current,PROJECT_KEY_METADATA_BYTES_MAX)?])?;
            }
            GitHubSendOutcome::ExternalChange { remote_updated_at } => {
                anyhow::ensure!(
                    remote_updated_at
                        .as_ref()
                        .is_none_or(|s| valid_metadata_text(s, 128)),
                    "project_key_remote_time_invalid"
                );
                github.state = GitHubRemoteState::ExternalChange;
                github.remote_updated_at = remote_updated_at;
                github.error_code = None;
                tx.execute(
                    "DELETE FROM project_key_github_outbox WHERE id=?1",
                    [&entry.id],
                )?;
            }
        }
        write_key(&tx, &key)?;
        tx.commit()?;
        Ok(true)
    }
    /// Linked rows across every project/environment, including inactive or watch-disabled scopes.
    /// A global stable ID cursor avoids materializing workspace catalogs or reading any secret location.
    pub fn project_key_github_observer_page(
        &self,
        limit: usize,
        after: Option<&str>,
    ) -> anyhow::Result<Vec<ProjectKey>> {
        anyhow::ensure!(
            (1..=16).contains(&limit) && after.is_none_or(valid_id),
            "project_key_observer_limit"
        );
        let tx = self.conn.unchecked_transaction()?;
        let rows = {
            let mut stmt=tx.prepare("SELECT workspace_id,environment,id FROM project_keys WHERE json_type(payload_json,'$.github')='object' AND (?1 IS NULL OR id>?1) ORDER BY id LIMIT ?2")?;
            stmt.query_map((after, limit as i64), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        let mut keys = Vec::with_capacity(rows.len());
        for (workspace, environment, id) in rows {
            let environment = match environment.as_str() {
                "development" => KeyEnvironment::Development,
                "production" => KeyEnvironment::Production,
                _ => anyhow::bail!("project_key_environment_invalid"),
            };
            let key =
                read_key(&tx, &workspace, environment, &id)?.context("project_key_missing")?;
            anyhow::ensure!(key.github.is_some(), "project_key_github_missing");
            keys.push(key);
        }
        tx.commit()?;
        Ok(keys)
    }
    /// Metadata cannot prove value equality. A stale poll cannot override a newer remote acknowledgment.
    #[allow(clippy::too_many_arguments)] // Both key and full remote status are independent race fences.
    pub fn project_key_github_observe_cas(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
        key_id: &str,
        expected_revision: u64,
        expected_status: &KeyGitHubStatus,
        remote_updated_at: Option<String>,
        now: i64,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(
            now >= 0 && valid_revision(expected_revision),
            "project_key_observation_invalid"
        );
        anyhow::ensure!(
            remote_updated_at
                .as_ref()
                .is_none_or(|s| valid_metadata_text(s, 128)),
            "project_key_remote_time_invalid"
        );
        check_scope(&self.conn, workspace)?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let Some(mut key) = read_key(&tx, workspace, environment, key_id)? else {
            return Ok(false);
        };
        if key.revision != expected_revision {
            return Ok(false);
        }
        let Some(github) = key.github.as_mut() else {
            return Ok(false);
        };
        if github != expected_status {
            return Ok(false);
        }
        if github.remote_updated_at != remote_updated_at {
            // Our own remote write can become visible before its fenced acknowledgment commits.
            let active:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM project_key_github_outbox WHERE key_id=?1 AND workspace_id=?2 AND environment=?3 AND key_revision=?4 AND claim_until>?5)",rusqlite::params![key_id,workspace,environment.as_str(),expected_revision as i64,now],|row|row.get(0))?;
            if active {
                return Ok(false);
            }
            github.state = GitHubRemoteState::ExternalChange;
            github.remote_updated_at = remote_updated_at;
            github.error_code = None;
            tx.execute(
                "DELETE FROM project_key_github_outbox WHERE key_id=?1",
                [key_id],
            )?;
            key.validate().map_err(anyhow::Error::msg)?;
            let affected=tx.execute("UPDATE project_keys SET payload_json=?4 WHERE id=?1 AND workspace_id=?2 AND environment=?3 AND revision=?5",rusqlite::params![key.id,key.workspace_id,key.environment.as_str(),encode(&key,PROJECT_KEY_METADATA_BYTES_MAX)?,key.revision as i64])?;
            anyhow::ensure!(affected == 1, "project_key_missing");
        }
        tx.commit()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const DEV: KeyEnvironment = KeyEnvironment::Development;
    const PROD: KeyEnvironment = KeyEnvironment::Production;
    fn database() -> (Db, String, String) {
        let db = Db::open_in_memory().unwrap();
        let a = db.create_workspace("a").unwrap();
        let b = db.create_workspace("b").unwrap();
        (db, a, b)
    }
    fn credential(db: &Db, workspace: &str, id: &str) {
        db.insert_credential(&CredentialMeta {
            id: id.into(),
            provider: "project_keys".into(),
            label: "TOKEN".into(),
            credential_kind: "api_key".into(),
            masked_hint: None,
            workspace_id: Some(workspace.into()),
        })
        .unwrap();
    }
    fn key(workspace: &str, id: &str, name: &str) -> ProjectKey {
        ProjectKey {
            id: id.into(),
            workspace_id: workspace.into(),
            environment: DEV,
            name: name.into(),
            credential_id: None,
            source_baseline_credential_id: None,
            source: KeySource::Vault,
            kind: KeyKind::ApiToken,
            usage: KeyUsage::Build,
            delivery: KeyDelivery::Environment,
            source_priority: 0,
            required: true,
            revision: 1,
            last_good_revision: None,
            last_good_at: None,
            storage_state: KeyStorageState::NotStored,
            created_at: 10,
            updated_at: 10,
            source_checked_at: None,
            error_code: None,
            github: None,
            legacy_reference: None,
        }
    }
    fn stored_key(db: &Db, workspace: &str, id: &str, name: &str) -> ProjectKey {
        credential(db, workspace, id);
        let mut k = key(workspace, id, name);
        k.credential_id = Some(id.into());
        k.last_good_revision = Some(1);
        k.storage_state = KeyStorageState::Stored;
        committed(db.project_key_save(workspace, DEV, &k, None).unwrap())
    }
    fn committed<T>(value: ProjectKeyCas<T>) -> T {
        match value {
            ProjectKeyCas::Committed(value) => value,
            _ => panic!("expected committed"),
        }
    }
    fn target() -> GitHubTarget {
        GitHubTarget {
            auth_credential_id: None,
            account_identity: "test-account".into(),
            scope: GitHubScope::Repository {
                owner: "owner".into(),
                repository: "repo".into(),
            },
            secret_name: "TOKEN".into(),
            auto_send: false,
        }
    }
    fn linked(db: &Db, workspace: &str) -> ProjectKey {
        let mut k = stored_key(db, workspace, "key-remote", "TOKEN");
        k.github = Some(KeyGitHubStatus {
            target: target(),
            state: GitHubRemoteState::Pending,
            last_sent_revision: None,
            remote_updated_at: None,
            error_code: None,
        });
        committed(db.project_key_save(workspace, DEV, &k, Some(1)).unwrap())
    }
    #[test]
    fn project_keys_environment_ownership_duplicates_and_cas_are_enforced() {
        let (db, a, b) = database();
        let first = committed(
            db.project_key_save(&a, DEV, &key(&a, "one", "TOKEN"), None)
                .unwrap(),
        );
        let mut production = key(&a, "prod", "TOKEN");
        production.environment = PROD;
        db.project_key_save(&a, PROD, &production, None).unwrap();
        assert!(db.project_key_get(&a, PROD, "one").unwrap().is_none());
        assert!(
            db.project_key_save(&a, DEV, &key(&a, "duplicate", "TOKEN"), None)
                .is_err()
        );
        assert!(
            db.project_key_save(&b, DEV, &key(&b, "one", "TOKEN"), Some(1))
                .is_err()
        );
        assert_eq!(
            db.project_key_save(&a, DEV, &first, None).unwrap(),
            ProjectKeyCas::Stale {
                current_revision: Some(1)
            }
        );
        let mut change = first.clone();
        change.usage = KeyUsage::StoreOnly;
        change.required = false;
        assert_eq!(
            committed(db.project_key_save(&a, DEV, &change, Some(1)).unwrap()).revision,
            2
        );
        assert_eq!(
            db.project_key_delete(&a, DEV, "one", 1).unwrap(),
            ProjectKeyCas::Stale {
                current_revision: Some(2)
            }
        );
        assert!(db.project_key_delete(&b, DEV, "one", 2).is_err());
        assert_eq!(
            db.project_key_delete(&a, DEV, "one", 2).unwrap(),
            ProjectKeyCas::Committed(())
        );
    }
    #[test]
    fn project_keys_last_good_survives_missing_empty_failure_and_conflict() {
        let (db, a, _) = database();
        let mut current = stored_key(&db, &a, "ref", "TOKEN");
        for state in [
            KeyStorageState::MissingSource,
            KeyStorageState::EmptySource,
            KeyStorageState::Failed,
            KeyStorageState::Conflict,
        ] {
            let expected = current.revision;
            current.storage_state = state;
            current.error_code = Some("project_key_source_missing".into());
            current = committed(
                db.project_key_save(&a, DEV, &current, Some(expected))
                    .unwrap(),
            );
            assert_eq!(current.last_good_revision, Some(1));
            assert_eq!(current.credential_id.as_deref(), Some("ref"));
        }
        assert_eq!(
            db.credential_secret_location("ref")
                .unwrap()
                .unwrap()
                .keyring_username,
            "ref"
        );
    }
    #[test]
    fn project_keys_legacy_introduction_keeps_override_rows_and_never_reads_plaintext() {
        let (db, a, _) = database();
        credential(&db, &a, "dotenv");
        credential(&db, &a, "manual");
        let profile = db.insert_env_profile(&a, ".env", "dotenv").unwrap();
        db.upsert_env_var(
            &profile,
            "TOKEN",
            &EnvValue::Secret {
                credential_id: "dotenv".into(),
            },
        )
        .unwrap();
        db.upsert_env_var(
            &profile,
            "PUBLIC_SETTING",
            &EnvValue::Plain("SENTINEL_MUST_NOT_LEAK".into()),
        )
        .unwrap();
        db.set_credential_env_binding(&a, "manual", Some("TOKEN"))
            .unwrap();
        let list = db.project_key_list(&a, DEV).unwrap();
        assert_eq!(list.iter().filter(|k| k.name == "TOKEN").count(), 2);
        let dotenv = list
            .iter()
            .find(|k| k.credential_id.as_deref() == Some("dotenv"))
            .unwrap();
        let manual = list
            .iter()
            .find(|k| k.credential_id.as_deref() == Some("manual"))
            .unwrap();
        assert_eq!(
            dotenv.source,
            KeySource::Dotenv {
                file: ".env".into()
            }
        );
        assert!(manual.source_priority > dotenv.source_priority);
        assert!(list.iter().all(|k| k.usage == KeyUsage::LegacyAgent));
        let json = serde_json::to_string(&list).unwrap();
        assert!(!json.contains("SENTINEL_MUST_NOT_LEAK"));
        assert!(!format!("{list:?}").contains("SENTINEL_MUST_NOT_LEAK"));
        assert_eq!(db.project_key_list(&a, DEV).unwrap(), list);
        assert!(db.project_key_list(&a, PROD).unwrap().is_empty());
        let public = list.iter().find(|k| k.name == "PUBLIC_SETTING").unwrap();
        assert_eq!(public.storage_state, KeyStorageState::NotStored);
    }
    #[test]
    fn project_keys_settings_are_independent_and_bounded() {
        let (db, a, b) = database();
        let mut dev = db.project_key_settings(&a, DEV).unwrap();
        let prod = db.project_key_settings(&a, PROD).unwrap();
        assert_eq!(dev.sources.len(), 2);
        assert!(prod.sources.is_empty());
        assert!(!prod.watch_enabled);
        dev.watch_enabled = false;
        let saved = committed(
            db.project_key_save_settings(&a, DEV, &dev, Some(1))
                .unwrap(),
        );
        assert_eq!(saved.revision, 2);
        assert!(
            db.project_key_save_settings(&b, DEV, &dev, Some(1))
                .is_err()
        );
        assert_eq!(
            db.project_key_save_settings(&a, DEV, &dev, Some(1))
                .unwrap(),
            ProjectKeyCas::Stale {
                current_revision: Some(2)
            }
        );
        assert!(!db.project_key_settings(&a, DEV).unwrap().watch_enabled);
        assert!(!db.project_key_settings(&a, PROD).unwrap().watch_enabled);
        assert!(db.project_key_watch_scopes(129).is_err());
        assert!(db.project_key_watch_scopes(128).unwrap().is_empty());
    }
    #[test]
    fn project_keys_credential_references_block_cleanup_and_owner_moves() {
        let (db, a, b) = database();
        let mut current = stored_key(&db, &a, "owned", "TOKEN");
        credential(&db, &a, "auth");
        current.source = KeySource::Dotenv {
            file: ".env".into(),
        };
        current.source_baseline_credential_id = Some("owned".into());
        let mut gh = target();
        gh.auth_credential_id = Some("auth".into());
        current.github = Some(KeyGitHubStatus {
            target: gh,
            state: GitHubRemoteState::Pending,
            last_sent_revision: None,
            remote_updated_at: None,
            error_code: None,
        });
        committed(db.project_key_save(&a, DEV, &current, Some(1)).unwrap());
        for id in ["owned", "auth"] {
            assert!(db.credential_in_use(id).unwrap());
            assert!(!db.delete_credential_if_unused(id).unwrap());
            assert!(!db.delete_credential_if_unused_cas(id, id).unwrap());
            assert!(
                db.conn
                    .execute(
                        "UPDATE credentials SET workspace_id=?2 WHERE id=?1",
                        (id, &b)
                    )
                    .is_err()
            );
        }
        credential(&db, &b, "foreign");
        current.credential_id = Some("foreign".into());
        current.source_baseline_credential_id = None;
        assert!(db.project_key_save(&a, DEV, &current, Some(2)).is_err());
    }
    #[test]
    fn project_keys_outbox_coalesces_and_fences_old_ack_and_external_change() {
        let (db, a, _) = database();
        let mut k = linked(&db, &a);
        assert!(
            db.project_key_github_enqueue(&a, DEV, &k.id, k.revision, false, 10)
                .unwrap()
        );
        let first = db.project_key_github_claim(10, 60, 1).unwrap().remove(0);
        assert!(db.project_key_github_claim(11, 60, 1).unwrap().is_empty());
        let prior = k.revision;
        k.updated_at = 11;
        k = committed(db.project_key_save(&a, DEV, &k, Some(prior)).unwrap());
        assert!(
            !db.project_key_github_ack_at(
                &first,
                GitHubSendOutcome::Sent {
                    remote_updated_at: Some("remote-old".into())
                },
                12
            )
            .unwrap()
        );
        assert!(
            db.project_key_github_enqueue(&a, DEV, &k.id, k.revision, false, 12)
                .unwrap()
        );
        let second = db.project_key_github_claim(12, 60, 1).unwrap().remove(0);
        assert!(
            db.project_key_github_ack_at(
                &second,
                GitHubSendOutcome::Sent {
                    remote_updated_at: Some("remote-new".into())
                },
                13
            )
            .unwrap()
        );
        let saved = db.project_key_get(&a, DEV, &k.id).unwrap().unwrap();
        assert_eq!(
            saved.github.as_ref().unwrap().last_sent_revision,
            Some(k.revision)
        );
        assert_eq!(saved.revision, k.revision);
        db.project_key_github_observe_cas(
            &a,
            DEV,
            &k.id,
            k.revision,
            db.project_key_get(&a, DEV, &k.id)
                .unwrap()
                .unwrap()
                .github
                .as_ref()
                .unwrap(),
            Some("remote-external".into()),
            100,
        )
        .unwrap();
        assert!(
            !db.project_key_github_enqueue(&a, DEV, &k.id, k.revision, false, 14)
                .unwrap()
        );
        assert!(
            db.project_key_github_enqueue(&a, DEV, &k.id, k.revision, true, 14)
                .unwrap()
        );
        let resumed = db.project_key_github_claim(14, 60, 1).unwrap().remove(0);
        assert_eq!(resumed.key_revision, k.revision);
    }
    #[test]
    fn project_keys_outbox_retry_and_expired_claim_survive_restart() {
        let path = std::env::temp_dir().join(format!(
            "deppy-project-key-restart-{}.sqlite3",
            uuid::Uuid::new_v4()
        ));
        let a;
        let key_id;
        let expired;
        {
            let db = Db::open(&path).unwrap();
            a = db.create_workspace("a").unwrap();
            let k = linked(&db, &a);
            key_id = k.id.clone();
            db.project_key_github_enqueue(&a, DEV, &k.id, k.revision, false, 10)
                .unwrap();
            let first = db.project_key_github_claim(10, 5, 1).unwrap().remove(0);
            db.project_key_github_ack_at(
                &first,
                GitHubSendOutcome::Retry {
                    error_code: "project_key_github_failed".into(),
                    next_attempt_at: 20,
                },
                11,
            )
            .unwrap();
            assert!(db.project_key_github_claim(19, 5, 1).unwrap().is_empty());
            expired = db.project_key_github_claim(20, 5, 1).unwrap().remove(0);
        }
        {
            let db = Db::open(&path).unwrap();
            assert!(
                !db.project_key_github_ack_at(
                    &expired,
                    GitHubSendOutcome::Sent {
                        remote_updated_at: None
                    },
                    26
                )
                .unwrap()
            );
            let fresh = db.project_key_github_claim(26, 30, 1).unwrap().remove(0);
            assert_eq!(fresh.attempts, 3);
            assert_ne!(fresh.claim_token, expired.claim_token);
            assert!(
                !db.project_key_github_ack_at(
                    &expired,
                    GitHubSendOutcome::Sent {
                        remote_updated_at: None
                    },
                    27
                )
                .unwrap()
            );
            assert!(
                db.project_key_github_ack_at(
                    &fresh,
                    GitHubSendOutcome::ExternalChange {
                        remote_updated_at: Some("other-writer".into())
                    },
                    27
                )
                .unwrap()
            );
            assert_eq!(
                db.project_key_get(&a, DEV, &key_id)
                    .unwrap()
                    .unwrap()
                    .github
                    .unwrap()
                    .state,
                GitHubRemoteState::ExternalChange
            );
            assert!(db.project_key_github_claim(50, 30, 1).unwrap().is_empty());
        }
        std::fs::remove_file(&path).unwrap();
    }
    #[test]
    fn project_keys_build_receipt_snapshot_is_immutable_and_scoped() {
        let (db, a, b) = database();
        let k = stored_key(&db, &a, "build-key", "TOKEN");
        let recipe = ProjectBuildRecipe {
            id: "recipe".into(),
            workspace_id: a.clone(),
            environment: DEV,
            revision: 1,
            name: "build".into(),
            command: vec!["cargo".into(), "build".into()],
            cwd: "/tmp/project".into(),
        };
        committed(db.project_key_recipe_save(&a, DEV, &recipe, None).unwrap());
        assert_eq!(db.project_key_recipe_list(&a, DEV).unwrap(), vec![recipe]);
        assert!(db.project_key_recipe_list(&a, PROD).unwrap().is_empty());
        let mut receipt = ProjectBuildReceipt {
            owner: None,
            id: "receipt".into(),
            workspace_id: a.clone(),
            environment: DEV,
            recipe_id: Some("recipe".into()),
            started_at: 10,
            finished_at: None,
            exit_code: None,
            used_keys: vec![ProjectKeyUsedVersion {
                key_id: k.id,
                name: k.name,
                revision: 1,
            }],
        };
        db.project_key_receipt_save(&receipt).unwrap();
        let mut wrong = receipt.clone();
        wrong.workspace_id = b;
        assert!(db.project_key_receipt_save(&wrong).is_err());
        wrong = receipt.clone();
        wrong.used_keys[0].revision = 2;
        assert!(db.project_key_receipt_save(&wrong).is_err());
        receipt.finished_at = Some(20);
        receipt.exit_code = Some(0);
        db.project_key_receipt_save(&receipt).unwrap();
        assert_eq!(
            db.project_key_receipt_list(&a, DEV, 10).unwrap(),
            vec![receipt.clone()]
        );
        assert!(db.project_key_receipt_save(&receipt).is_err());
    }
    #[test]
    fn project_keys_value_publication_and_metadata_rollback_together() {
        let (db, a, _) = database();
        let logical = secret::LogicalCredentialId::new("atomic-key").unwrap();
        let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        db.register_physical_secret_slot_staging(logical.as_str(), slot.as_str())
            .unwrap();
        let mut k = key(&a, "atomic", "TOKEN");
        k.credential_id = Some(logical.as_str().into());
        let commit = ProjectKeyValueCommit {
            credential_meta: Some(CredentialMeta {
                id: logical.as_str().into(),
                provider: "project_keys".into(),
                label: "TOKEN".into(),
                credential_kind: "api_key".into(),
                masked_hint: None,
                workspace_id: Some(a.clone()),
            }),
            expected_previous_pointer: None,
            physical_slot: slot.as_str().into(),
            oauth_json: None,
            masked_hint: None,
        };
        db.conn.execute_batch("CREATE TRIGGER fail_key BEFORE INSERT ON project_keys BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
        assert!(
            db.project_key_commit_value(&a, DEV, &k, None, &commit)
                .is_err()
        );
        assert!(
            db.credential_secret_location(logical.as_str())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.physical_secret_slots_for_reconciliation(1).unwrap()[0].state,
            PhysicalSecretSlotState::Staging
        );
        db.conn.execute_batch("DROP TRIGGER fail_key;").unwrap();
        let saved = committed(
            db.project_key_commit_value(&a, DEV, &k, None, &commit)
                .unwrap(),
        );
        assert_eq!(saved.last_good_revision, Some(1));
        assert_eq!(saved.storage_state, KeyStorageState::Stored);
        assert_eq!(
            db.credential_secret_location(logical.as_str())
                .unwrap()
                .unwrap()
                .keyring_username,
            slot.as_str()
        );
        let next = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        db.register_physical_secret_slot_staging(logical.as_str(), next.as_str())
            .unwrap();
        let replacement = ProjectKeyValueCommit {
            credential_meta: None,
            expected_previous_pointer: Some(slot.as_str().into()),
            physical_slot: next.as_str().into(),
            oauth_json: None,
            masked_hint: None,
        };
        assert_eq!(
            db.project_key_commit_value(&a, DEV, &saved, None, &replacement)
                .unwrap(),
            ProjectKeyCas::Stale {
                current_revision: Some(1)
            }
        );
        assert_eq!(
            db.credential_secret_location(logical.as_str())
                .unwrap()
                .unwrap()
                .keyring_username,
            slot.as_str()
        );
        db.conn.execute_batch("CREATE TRIGGER fail_key_update BEFORE UPDATE ON project_keys BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
        assert!(
            db.project_key_commit_value(&a, DEV, &saved, Some(1), &replacement)
                .is_err()
        );
        assert_eq!(
            db.credential_secret_location(logical.as_str())
                .unwrap()
                .unwrap()
                .keyring_username,
            slot.as_str()
        );
        db.conn
            .execute_batch("DROP TRIGGER fail_key_update;")
            .unwrap();
        assert_eq!(
            committed(
                db.project_key_commit_value(&a, DEV, &saved, Some(1), &replacement)
                    .unwrap()
            )
            .revision,
            2
        );
        let ledger = db.physical_secret_slots_for_reconciliation(2).unwrap();
        assert_eq!(
            ledger
                .iter()
                .find(|row| row.physical_slot == slot.as_str())
                .unwrap()
                .state,
            PhysicalSecretSlotState::Orphan
        );
        assert_eq!(
            ledger
                .iter()
                .find(|row| row.physical_slot == next.as_str())
                .unwrap()
                .state,
            PhysicalSecretSlotState::Published
        );
    }
    #[test]
    fn project_keys_append_only_migration_preserves_legacy_database() {
        let path = std::env::temp_dir().join(format!(
            "deppy-project-key-migration-{}.sqlite3",
            uuid::Uuid::new_v4()
        ));
        {
            let old =
                storage_core::open_with_migrations(&path, &MIGRATIONS[..MIGRATIONS.len() - 1])
                    .unwrap();
            old.execute("INSERT INTO workspaces(id,name,path,created_at,updated_at) VALUES ('legacy','existing','/tmp/existing','now','now')",[]).unwrap();
        }
        {
            let db = Db::open(&path).unwrap();
            assert_eq!(
                storage_core::read_user_version(&db.conn).unwrap(),
                MIGRATIONS.len()
            );
            assert_eq!(
                db.conn
                    .query_row("SELECT name FROM workspaces WHERE id='legacy'", [], |r| {
                        r.get::<_, String>(0)
                    })
                    .unwrap(),
                "existing"
            );
            assert!(db.project_key_list("legacy", DEV).unwrap().is_empty());
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(path.with_extension("sqlite3.bak")).unwrap();
    }
    #[test]
    fn project_keys_metadata_schema_serialization_and_debug_have_no_secret_value_field() {
        let (db, a, _) = database();
        let k = committed(
            db.project_key_save(&a, DEV, &key(&a, "safe", "TOKEN"), None)
                .unwrap(),
        );
        let json = serde_json::to_value(&k).unwrap();
        for forbidden in [
            "value",
            "secret_value",
            "plain_value",
            "access",
            "refresh",
            "source_digest",
            "raw_hash",
        ] {
            assert!(json.get(forbidden).is_none());
        }
        let columns = {
            let mut stmt = db.conn.prepare("PRAGMA table_info(project_keys)").unwrap();
            stmt.query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert!(!columns.iter().any(|column| column == "plain_value"
            || column == "secret_value"
            || column == "value"));
        assert!(!valid_error_code(Some("lowercase_secret_token")));
        assert!(valid_error_code(Some("project_key_github_failed")));
        let mut invalid = k.clone();
        invalid.error_code = Some("Bearer sensitive-token".into());
        assert!(db.project_key_save(&a, DEV, &invalid, Some(1)).is_err());
        invalid = k.clone();
        invalid.source = KeySource::Dotenv {
            file: "../outside".into(),
        };
        assert!(db.project_key_save(&a, DEV, &invalid, Some(1)).is_err());
    }
    #[test]
    fn project_keys_shared_legacy_credential_bindings_remain_project_scoped() {
        let (db, a, b) = database();
        db.insert_credential(&CredentialMeta {
            id: "shared".into(),
            provider: "github".into(),
            label: "Shared".into(),
            credential_kind: "token".into(),
            masked_hint: None,
            workspace_id: None,
        })
        .unwrap();
        db.set_credential_env_binding(&a, "shared", Some("TOKEN_A"))
            .unwrap();
        db.set_credential_env_binding(&b, "shared", Some("TOKEN_B"))
            .unwrap();
        let first = db.project_key_list(&a, DEV).unwrap();
        let second = db.project_key_list(&b, DEV).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_ne!(first[0].id, second[0].id);
        assert_eq!(first[0].name, "TOKEN_A");
        assert_eq!(second[0].name, "TOKEN_B");
        assert_eq!(first[0].credential_id, second[0].credential_id);
        assert_ne!(first[0].legacy_reference, second[0].legacy_reference);
        let logical = secret::LogicalCredentialId::new("shared").unwrap();
        let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        db.register_physical_secret_slot_staging("shared", slot.as_str())
            .unwrap();
        let shared_publication = ProjectKeyValueCommit {
            credential_meta: None,
            expected_previous_pointer: Some("shared".into()),
            physical_slot: slot.as_str().into(),
            oauth_json: None,
            masked_hint: None,
        };
        assert!(
            db.project_key_commit_value(&a, DEV, &first[0], Some(1), &shared_publication)
                .is_err()
        );
        assert_eq!(
            db.credential_secret_location("shared")
                .unwrap()
                .unwrap()
                .keyring_username,
            "shared"
        );
    }
    #[test]
    fn project_keys_auth_choices_include_explicit_shared_and_reject_foreign_or_oauth() {
        let (db, a, b) = database();
        credential(&db, &a, "own");
        credential(&db, &b, "foreign");
        db.insert_credential(&CredentialMeta {
            id: "shared".into(),
            provider: "github".into(),
            label: "Shared token".into(),
            credential_kind: "token".into(),
            masked_hint: Some("SHOULD_NOT_BE_READ".into()),
            workspace_id: None,
        })
        .unwrap();
        db.insert_credential(&CredentialMeta {
            id: "oauth".into(),
            provider: "github".into(),
            label: "OAuth".into(),
            credential_kind: "oauth".into(),
            masked_hint: None,
            workspace_id: Some(a.clone()),
        })
        .unwrap();
        let choices = db.project_key_auth_credentials(&a, DEV).unwrap();
        assert_eq!(choices.len(), 2);
        assert!(choices.iter().all(|c| c.masked_hint.is_none()));
        assert!(db.project_key_credential(&a, "shared").unwrap().is_some());
        assert!(db.project_key_credential(&a, "own").unwrap().is_some());
        assert!(db.project_key_credential(&a, "foreign").unwrap().is_none());
        assert!(db.project_key_credential(&a, "oauth").unwrap().is_none());
    }
    #[test]
    fn project_keys_watch_cursor_visits_every_scope_without_silent_truncation() {
        let (db, a, b) = database();
        db.project_key_settings(&a, DEV).unwrap();
        db.project_key_settings(&b, DEV).unwrap();
        assert!(db.project_key_watch_scopes(1).is_err());
        let first = db.project_key_watch_scopes_page(1, None).unwrap();
        assert_eq!(first.len(), 1);
        let second = db
            .project_key_watch_scopes_page(1, Some((&first[0].workspace_id, first[0].environment)))
            .unwrap();
        assert_eq!(second.len(), 1);
        assert_ne!(first[0].workspace_id, second[0].workspace_id);
        assert!(
            db.project_key_watch_scopes_page(
                1,
                Some((&second[0].workspace_id, second[0].environment))
            )
            .unwrap()
            .is_empty()
        );
        assert_eq!(db.project_key_watch_scopes(2).unwrap().len(), 2);
    }
    fn detached_commit(db: &Db, workspace: &str, id: &str) -> ProjectKeyValueCommit {
        let logical = secret::LogicalCredentialId::new(id).unwrap();
        let physical = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        db.register_physical_secret_slot_staging(id, physical.as_str())
            .unwrap();
        ProjectKeyValueCommit {
            credential_meta: Some(CredentialMeta {
                id: id.into(),
                provider: "project_keys".into(),
                label: "TOKEN".into(),
                credential_kind: "api_key".into(),
                masked_hint: None,
                workspace_id: Some(workspace.into()),
            }),
            expected_previous_pointer: None,
            physical_slot: physical.as_str().into(),
            oauth_json: None,
            masked_hint: None,
        }
    }
    #[test]
    fn project_keys_detached_legacy_agent_replace_updates_only_selected_binding_atomically() {
        let (db, a, b) = database();
        db.insert_credential(&CredentialMeta {
            id: "shared".into(),
            provider: "github".into(),
            label: "Shared".into(),
            credential_kind: "token".into(),
            masked_hint: None,
            workspace_id: None,
        })
        .unwrap();
        db.set_credential_env_binding(&a, "shared", Some("TOKEN"))
            .unwrap();
        db.set_credential_env_binding(&b, "shared", Some("TOKEN"))
            .unwrap();
        let mut selected = db.project_key_list(&a, DEV).unwrap().remove(0);
        let other = db.project_key_list(&b, DEV).unwrap().remove(0);
        selected.credential_id = Some("owned-replacement".into());
        let publication = detached_commit(&db, &a, "owned-replacement");
        let saved = committed(
            db.project_key_commit_value(&a, DEV, &selected, Some(1), &publication)
                .unwrap(),
        );
        assert_eq!(saved.last_good_revision, Some(2));
        assert_eq!(
            db.list_credential_env_bindings(&a).unwrap()[0].credential_id,
            "owned-replacement"
        );
        assert_eq!(
            db.list_credential_env_bindings(&b).unwrap()[0].credential_id,
            "shared"
        );
        assert_eq!(
            db.project_key_get(&b, DEV, &other.id)
                .unwrap()
                .unwrap()
                .credential_id
                .as_deref(),
            Some("shared")
        );
        // Stale legacy mutation is fenced even when the metadata revision has not changed.
        db.set_credential_env_binding(&a, "owned-replacement", None)
            .unwrap();
        let mut next = saved.clone();
        next.credential_id = Some("next".into());
        let publication = detached_commit(&db, &a, "next");
        assert_eq!(
            db.project_key_commit_value(&a, DEV, &next, Some(2), &publication)
                .unwrap(),
            ProjectKeyCas::Stale {
                current_revision: Some(2)
            }
        );
        assert!(db.credential_secret_location("next").unwrap().is_none());
    }
    #[test]
    fn project_keys_detached_env_reference_updates_only_selected_row_and_preserves_build_scope() {
        let (db, a, _) = database();
        credential(&db, &a, "original");
        let first = db.insert_env_profile(&a, ".env", "dotenv").unwrap();
        let second = db.insert_env_profile(&a, ".env.local", "dotenv").unwrap();
        for profile in [&first, &second] {
            db.upsert_env_var(
                profile,
                "TOKEN",
                &EnvValue::Secret {
                    credential_id: "original".into(),
                },
            )
            .unwrap();
        }
        let keys = db.project_key_list(&a, DEV).unwrap();
        let mut selected = keys
            .iter()
            .find(|k| {
                k.source
                    == KeySource::Dotenv {
                        file: ".env".into(),
                    }
            })
            .unwrap()
            .clone();
        let mut separate = keys
            .iter()
            .find(|k| {
                k.source
                    == KeySource::Dotenv {
                        file: ".env.local".into(),
                    }
            })
            .unwrap()
            .clone();
        selected.credential_id = Some("selected-new".into());
        let publication = detached_commit(&db, &a, "selected-new");
        committed(
            db.project_key_commit_value(&a, DEV, &selected, Some(1), &publication)
                .unwrap(),
        );
        assert_eq!(
            db.list_env_vars(&first).unwrap()[0].value,
            EnvValue::Secret {
                credential_id: "selected-new".into()
            }
        );
        assert_eq!(
            db.list_env_vars(&second).unwrap()[0].value,
            EnvValue::Secret {
                credential_id: "original".into()
            }
        );
        separate.usage = KeyUsage::Build;
        separate.credential_id = Some("build-only".into());
        let publication = detached_commit(&db, &a, "build-only");
        committed(
            db.project_key_commit_value(&a, DEV, &separate, Some(1), &publication)
                .unwrap(),
        );
        assert_eq!(
            db.list_env_vars(&second).unwrap()[0].value,
            EnvValue::Secret {
                credential_id: "original".into()
            }
        );
    }
    #[test]
    fn project_keys_workspace_identity_first_enrollment_is_path_cas_and_never_rebinds() {
        let (db, a, _) = database();
        db.set_workspace_path(&a, "/tmp/project").unwrap();
        assert_eq!(
            db.project_key_workspace_identity(&a).unwrap(),
            Some(("/tmp/project".into(), None, None))
        );
        let anchor = WorkspaceFolderAnchor { dev: 1, ino: 2 };
        let volume = uuid::Uuid::new_v4();
        assert!(
            !db.project_key_enroll_workspace_identity(&a, "/tmp/wrong", anchor, Some(volume))
                .unwrap()
        );
        assert!(
            db.project_key_enroll_workspace_identity(&a, "/tmp/project", anchor, Some(volume))
                .unwrap()
        );
        assert_eq!(
            db.project_key_workspace_identity(&a).unwrap(),
            Some(("/tmp/project".into(), Some(anchor), Some(volume)))
        );
        assert!(
            !db.project_key_enroll_workspace_identity(
                &a,
                "/tmp/project",
                WorkspaceFolderAnchor { dev: 1, ino: 3 },
                None
            )
            .unwrap()
        );
        assert!(
            db.project_key_workspace_identity("missing")
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn project_keys_github_pre_send_check_fences_expiry_and_new_generation() {
        let (db, a, _) = database();
        let mut key = linked(&db, &a);
        db.project_key_github_enqueue(&a, DEV, &key.id, key.revision, false, 10)
            .unwrap();
        let entry = db.project_key_github_claim(10, 30, 1).unwrap().remove(0);
        assert!(db.project_key_github_claim_is_current(&entry, 11).unwrap());
        assert!(!db.project_key_github_claim_is_current(&entry, 40).unwrap());
        let revision = key.revision;
        key.updated_at = 12;
        committed(db.project_key_save(&a, DEV, &key, Some(revision)).unwrap());
        assert!(!db.project_key_github_claim_is_current(&entry, 12).unwrap());
    }
    #[test]
    fn project_keys_review_managed_production_is_not_imported_to_development() {
        let (db, a, _) = database();
        credential(&db, &a, "prod-managed");
        let mut prod = key(&a, "prod-key", "PROD_TOKEN");
        prod.environment = PROD;
        prod.credential_id = Some("prod-managed".into());
        prod.storage_state = KeyStorageState::Stored;
        prod.last_good_revision = Some(1);
        committed(db.project_key_save(&a, PROD, &prod, None).unwrap());
        assert!(db.project_key_list(&a, DEV).unwrap().is_empty());
        assert_eq!(db.project_key_list(&a, PROD).unwrap().len(), 1);
        let (db, a, _) = database();
        credential(&db, &a, "legacy-prod");
        let profile = db
            .insert_env_profile(&a, "production", "production")
            .unwrap();
        db.upsert_env_var(
            &profile,
            "TOKEN",
            &EnvValue::Secret {
                credential_id: "legacy-prod".into(),
            },
        )
        .unwrap();
        assert!(db.project_key_list(&a, DEV).unwrap().is_empty());
        let mut prod = db.project_key_list(&a, PROD).unwrap().remove(0);
        assert_eq!(prod.environment, PROD);
        prod.credential_id = Some("new-prod".into());
        let publication = detached_commit(&db, &a, "new-prod");
        committed(
            db.project_key_commit_value(&a, PROD, &prod, Some(1), &publication)
                .unwrap(),
        );
        assert_eq!(
            db.list_env_vars(&profile).unwrap()[0].value,
            EnvValue::Secret {
                credential_id: "new-prod".into()
            }
        );
        assert!(db.project_key_list(&a, DEV).unwrap().is_empty());
    }
    #[test]
    fn project_keys_review_stale_save_cannot_clear_external_change_or_lose_sent_metadata() {
        let (db, a, _) = database();
        let mut key = linked(&db, &a);
        key.github.as_mut().unwrap().target.auto_send = true;
        key = committed(db.project_key_save(&a, DEV, &key, Some(2)).unwrap());
        db.project_key_github_observe_cas(
            &a,
            DEV,
            &key.id,
            key.revision,
            db.project_key_get(&a, DEV, &key.id)
                .unwrap()
                .unwrap()
                .github
                .as_ref()
                .unwrap(),
            Some("external".into()),
            100,
        )
        .unwrap();
        key.required = false;
        let saved = committed(
            db.project_key_save(&a, DEV, &key, Some(key.revision))
                .unwrap(),
        );
        assert_eq!(
            saved.github.as_ref().unwrap().state,
            GitHubRemoteState::ExternalChange
        );
        assert_eq!(
            saved.github.as_ref().unwrap().remote_updated_at.as_deref(),
            Some("external")
        );
        assert!(db.project_key_github_claim(100, 30, 1).unwrap().is_empty());
        assert!(
            !db.project_key_github_enqueue(&a, DEV, &saved.id, saved.revision, false, 100)
                .unwrap()
        );
        assert!(
            db.project_key_github_enqueue(&a, DEV, &saved.id, saved.revision, true, 100)
                .unwrap()
        );
        let claim = db.project_key_github_claim(100, 30, 1).unwrap().remove(0);
        db.project_key_github_ack_at(
            &claim,
            GitHubSendOutcome::Sent {
                remote_updated_at: Some("sent".into()),
            },
            101,
        )
        .unwrap();
        let mut stale = saved;
        stale.required = true;
        let refreshed = committed(
            db.project_key_save(&a, DEV, &stale, Some(stale.revision))
                .unwrap(),
        );
        assert_eq!(
            refreshed.github.as_ref().unwrap().last_sent_revision,
            Some(stale.revision)
        );
        assert_eq!(
            refreshed
                .github
                .as_ref()
                .unwrap()
                .remote_updated_at
                .as_deref(),
            Some("sent")
        );
    }
    #[test]
    fn project_keys_review_full_capacity_allows_existing_updates_and_remote_ack() {
        let (db, a, _) = database();
        let key = linked(&db, &a);
        for n in 1..256 {
            committed(
                db.project_key_save(
                    &a,
                    DEV,
                    &super::tests::key(&a, &format!("id-{n}"), &format!("TOKEN_{n}")),
                    None,
                )
                .unwrap(),
            );
        }
        assert!(
            db.project_key_save(&a, DEV, &super::tests::key(&a, "over", "OVER"), None)
                .is_err()
        );
        let mut changed = key.clone();
        changed.required = false;
        let saved = committed(
            db.project_key_save(&a, DEV, &changed, Some(key.revision))
                .unwrap(),
        );
        db.project_key_github_enqueue(&a, DEV, &saved.id, saved.revision, false, 10)
            .unwrap();
        let entry = db.project_key_github_claim(10, 30, 1).unwrap().remove(0);
        assert!(
            db.project_key_github_ack_at(
                &entry,
                GitHubSendOutcome::Sent {
                    remote_updated_at: None
                },
                11
            )
            .unwrap()
        );
        assert_eq!(db.project_key_list(&a, DEV).unwrap().len(), 256);
        assert_eq!(db.project_key_managed_ids(&a, DEV).unwrap().len(), 256);
    }
    #[test]
    fn project_keys_review_recovery_promotes_valid_unchanged_last_good_for_sending() {
        let (db, a, _) = database();
        let mut key = linked(&db, &a);
        key.storage_state = KeyStorageState::MissingSource;
        key = committed(db.project_key_save(&a, DEV, &key, Some(2)).unwrap());
        assert_eq!(key.last_good_revision, Some(2));
        key.storage_state = KeyStorageState::Stored;
        key.github.as_mut().unwrap().target.auto_send = true;
        key = committed(db.project_key_save(&a, DEV, &key, Some(3)).unwrap());
        assert_eq!(key.last_good_revision, Some(4));
        assert_eq!(
            db.project_key_github_claim(10, 30, 1).unwrap()[0].key_revision,
            4
        );
    }
    #[test]
    fn project_keys_review_auth_choices_exclude_managed_values_and_page_real_tokens() {
        let (db, a, _) = database();
        db.project_key_list(&a, DEV).unwrap();
        for (environment, prefix) in [(DEV, "dev"), (PROD, "prod")] {
            for n in 0..128 {
                let id = format!("{prefix}-{n}");
                credential(&db, &a, &id);
                let mut key = key(&a, &id, &format!("TOKEN_{n}"));
                key.environment = environment;
                key.credential_id = Some(id);
                key.storage_state = KeyStorageState::Stored;
                key.last_good_revision = Some(1);
                committed(db.project_key_save(&a, environment, &key, None).unwrap());
            }
        }
        credential(&db, &a, "auth-only");
        let choices = db.project_key_auth_credentials(&a, DEV).unwrap();
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].id, "auth-only");
        let mut stored_auth = key(&a, "stored-auth", "GITHUB_TOKEN");
        credential(&db, &a, "stored-auth");
        stored_auth.usage = KeyUsage::StoreOnly;
        stored_auth.required = false;
        stored_auth.credential_id = Some("stored-auth".into());
        stored_auth.storage_state = KeyStorageState::Stored;
        stored_auth.last_good_revision = Some(1);
        committed(db.project_key_save(&a, DEV, &stored_auth, None).unwrap());
        assert!(
            db.project_key_auth_credentials(&a, DEV)
                .unwrap()
                .iter()
                .any(|c| c.id == "stored-auth")
        );
        credential(&db, &a, "auth-other");
        let first = db
            .project_key_auth_credentials_page(&a, DEV, 1, None)
            .unwrap();
        let second = db
            .project_key_auth_credentials_page(&a, DEV, 1, Some(&first[0].id))
            .unwrap();
        assert_eq!(second.len(), 1);
        assert_ne!(first[0].id, second[0].id);
        let third = db
            .project_key_auth_credentials_page(&a, DEV, 1, Some(&second[0].id))
            .unwrap();
        assert_eq!(third.len(), 1);
        assert!(
            db.project_key_auth_credentials_page(&a, DEV, 1, Some(&third[0].id))
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn project_keys_review_managed_auth_tokens_are_environment_scoped_in_choices_lookup_and_save() {
        let (db, a, _) = database();
        db.project_key_list(&a, DEV).unwrap();
        credential(&db, &a, "prod-auth");
        let mut token = key(&a, "prod-auth", "GITHUB_TOKEN");
        token.environment = PROD;
        token.usage = KeyUsage::StoreOnly;
        token.required = false;
        token.credential_id = Some("prod-auth".into());
        token.storage_state = KeyStorageState::Stored;
        token.last_good_revision = Some(1);
        committed(db.project_key_save(&a, PROD, &token, None).unwrap());
        assert!(db.project_key_auth_credentials(&a, DEV).unwrap().is_empty());
        assert_eq!(
            db.project_key_auth_credentials(&a, PROD).unwrap()[0].id,
            "prod-auth"
        );
        assert!(
            db.project_key_auth_credential(&a, DEV, "prod-auth")
                .unwrap()
                .is_none()
        );
        assert!(
            db.project_key_auth_credential(&a, PROD, "prod-auth")
                .unwrap()
                .is_some()
        );
        let mut development = key(&a, "dev-key", "TOKEN");
        let mut target = target();
        target.auth_credential_id = Some("prod-auth".into());
        development.github = Some(KeyGitHubStatus {
            target,
            state: GitHubRemoteState::Pending,
            last_sent_revision: None,
            remote_updated_at: None,
            error_code: None,
        });
        assert!(db.project_key_save(&a, DEV, &development, None).is_err());
        let mut production_target = development.clone();
        production_target.id = "production-target".into();
        production_target.environment = PROD;
        committed(
            db.project_key_save(&a, PROD, &production_target, None)
                .unwrap(),
        );
        committed(db.project_key_delete(&a, PROD, &token.id, 1).unwrap());
        assert!(
            db.project_key_auth_credential(&a, DEV, "prod-auth")
                .unwrap()
                .is_none()
        );
        assert!(db.project_key_auth_credentials(&a, DEV).unwrap().is_empty());
        assert!(
            db.project_key_auth_credential(&a, PROD, "prod-auth")
                .unwrap()
                .is_some()
        );
        assert!(db.project_key_save(&a, DEV, &development, None).is_err());
        assert!(
            db.conn
                .execute(
                    "UPDATE credentials SET workspace_id=NULL WHERE id='prod-auth'",
                    []
                )
                .is_err()
        );
        db.insert_credential(&CredentialMeta {
            id: "shared-auth".into(),
            provider: "github".into(),
            label: "Shared token".into(),
            credential_kind: "token".into(),
            masked_hint: None,
            workspace_id: None,
        })
        .unwrap();
        assert!(
            db.project_key_auth_credential(&a, DEV, "shared-auth")
                .unwrap()
                .is_some()
        );
        assert!(
            db.project_key_auth_credential(&a, PROD, "shared-auth")
                .unwrap()
                .is_some()
        );
    }
    #[test]
    fn project_keys_legacy_plain_configuration_worker_read_and_enrollment_are_scoped() {
        let (db, a, b) = database();
        let profile = db.insert_env_profile(&a, "custom", "custom").unwrap();
        db.upsert_env_var(
            &profile,
            "PUBLIC_SETTING",
            &EnvValue::Plain("public-config".into()),
        )
        .unwrap();
        let mut key = db.project_key_list(&a, DEV).unwrap().remove(0);
        assert_eq!(key.kind, KeyKind::Configuration);
        assert_eq!(
            db.project_key_legacy_configuration_value(&a, DEV, &key.id, 1)
                .unwrap()
                .as_deref(),
            Some("public-config")
        );
        assert!(
            db.project_key_legacy_configuration_value(&b, DEV, &key.id, 1)
                .unwrap()
                .is_none()
        );
        assert!(
            db.project_key_legacy_configuration_value(&a, PROD, &key.id, 1)
                .unwrap()
                .is_none()
        );
        assert!(
            db.project_key_legacy_configuration_value(&a, DEV, &key.id, 2)
                .unwrap()
                .is_none()
        );
        db.upsert_env_var(
            &profile,
            "PUBLIC_SETTING",
            &EnvValue::Plain("updated-public-config".into()),
        )
        .unwrap();
        let changed = db.project_key_get(&a, DEV, &key.id).unwrap().unwrap();
        assert_eq!(changed.revision, 2);
        assert_eq!(changed.storage_state, KeyStorageState::UpdateNeeded);
        assert!(
            db.project_key_legacy_configuration_value(&a, DEV, &key.id, 1)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.project_key_legacy_configuration_value(&a, DEV, &key.id, 2)
                .unwrap()
                .as_deref(),
            Some("updated-public-config")
        );
        key.credential_id = Some("stale-config".into());
        let stale_commit = detached_commit(&db, &a, "stale-config");
        assert_eq!(
            db.project_key_commit_value(&a, DEV, &key, Some(1), &stale_commit)
                .unwrap(),
            ProjectKeyCas::Stale {
                current_revision: Some(2)
            }
        );
        assert!(
            db.credential_secret_location("stale-config")
                .unwrap()
                .is_none()
        );
        key = changed;
        key.credential_id = Some("protected-config".into());
        let commit = detached_commit(&db, &a, "protected-config");
        let stored = committed(
            db.project_key_commit_value(&a, DEV, &key, Some(2), &commit)
                .unwrap(),
        );
        assert_eq!(stored.revision, 3);
        assert_eq!(stored.kind, KeyKind::Configuration);
        assert_eq!(
            db.list_env_vars(&profile).unwrap()[0].value,
            EnvValue::Secret {
                credential_id: "protected-config".into()
            }
        );
        assert!(
            db.project_key_legacy_configuration_value(&a, DEV, &key.id, 2)
                .unwrap()
                .is_none()
        );
        let plain: Option<String> = db
            .conn
            .query_row(
                "SELECT plain_value FROM env_vars WHERE profile_id=?1",
                [&profile],
                |row| row.get(0),
            )
            .unwrap();
        assert!(plain.is_none());
    }
    #[test]
    fn project_keys_post_enrollment_legacy_plain_and_secret_edits_pin_fresh_source_generation() {
        let (db, a, b) = database();
        let profile = db.insert_env_profile(&a, "custom", "custom").unwrap();
        db.upsert_env_var(&profile, "CONFIG", &EnvValue::Plain("first".into()))
            .unwrap();
        let mut key = db.project_key_list(&a, DEV).unwrap().remove(0);
        key.credential_id = Some("first-owned".into());
        let first = detached_commit(&db, &a, "first-owned");
        let key = committed(
            db.project_key_commit_value(&a, DEV, &key, Some(1), &first)
                .unwrap(),
        );
        db.upsert_env_var(&profile, "CONFIG", &EnvValue::Plain("latest".into()))
            .unwrap();
        let mut changed = db.project_key_get(&a, DEV, &key.id).unwrap().unwrap();
        assert_eq!(changed.revision, key.revision + 1);
        assert_eq!(changed.last_good_at, key.last_good_at);
        assert!(
            db.project_key_value_location(&a, DEV, &key.id, changed.revision)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.project_key_legacy_configuration_value(&a, DEV, &key.id, changed.revision)
                .unwrap()
                .as_deref(),
            Some("latest")
        );
        changed.credential_id = Some("latest-owned".into());
        let commit = detached_commit(&db, &a, "latest-owned");
        let latest = committed(
            db.project_key_commit_value(&a, DEV, &changed, Some(changed.revision), &commit)
                .unwrap(),
        );
        assert_eq!(latest.revision, changed.revision + 1);
        assert_eq!(latest.storage_state, KeyStorageState::Stored);
        credential(&db, &a, "advanced-current");
        db.upsert_env_var(
            &profile,
            "CONFIG",
            &EnvValue::Secret {
                credential_id: "advanced-current".into(),
            },
        )
        .unwrap();
        let mut changed = db.project_key_get(&a, DEV, &key.id).unwrap().unwrap();
        assert!(
            db.project_key_value_location(&a, DEV, &key.id, changed.revision)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.project_key_legacy_value_location(&a, DEV, &key.id, changed.revision)
                .unwrap()
                .unwrap()
                .0
                .id,
            "advanced-current"
        );
        assert!(
            db.project_key_legacy_value_location(&b, DEV, &key.id, changed.revision)
                .unwrap()
                .is_none()
        );
        changed.credential_id = Some("protected-current".into());
        let commit = detached_commit(&db, &a, "protected-current");
        let refreshed = committed(
            db.project_key_commit_value(&a, DEV, &changed, Some(changed.revision), &commit)
                .unwrap(),
        );
        assert_eq!(refreshed.revision, changed.revision + 1);
        assert_eq!(
            db.list_env_vars(&profile).unwrap()[0].value,
            EnvValue::Secret {
                credential_id: "protected-current".into()
            }
        );
    }
    #[test]
    fn project_keys_change_history_is_ordered_bounded_scoped_value_free_and_saved_time_survives() {
        let (db, a, b) = database();
        let mut key = key(&a, "history-key", "HISTORY_TOKEN");
        key.credential_id = Some("history-value".into());
        let publication = detached_commit(&db, &a, "history-value");
        let mut key = committed(
            db.project_key_commit_value(&a, DEV, &key, None, &publication)
                .unwrap(),
        );
        let saved_at = key.last_good_at.unwrap();
        let initial = db.project_key_changes(&a, DEV, &key.id).unwrap();
        assert_eq!(initial[0].kind, ProjectKeyChangeKind::ValueSaved);
        assert_eq!(initial.last().unwrap().kind, ProjectKeyChangeKind::Created);
        assert!(
            initial
                .windows(2)
                .all(|pair| pair[0].sequence > pair[1].sequence)
        );
        assert!(db.project_key_changes(&b, DEV, &key.id).unwrap().is_empty());
        assert!(
            db.project_key_changes(&a, PROD, &key.id)
                .unwrap()
                .is_empty()
        );
        for n in 0..40 {
            let expected = key.revision;
            key.storage_state = if n % 2 == 0 {
                KeyStorageState::MissingSource
            } else {
                KeyStorageState::Locked
            };
            key.last_good_at = Some(0);
            key = committed(db.project_key_save(&a, DEV, &key, Some(expected)).unwrap());
            assert_eq!(key.last_good_at, Some(saved_at));
        }
        let changes = db.project_key_changes(&a, DEV, &key.id).unwrap();
        assert_eq!(changes.len(), PROJECT_KEY_CHANGES_MAX);
        assert_eq!(changes[0].revision, key.revision);
        assert!(
            changes
                .windows(2)
                .all(|pair| pair[0].sequence > pair[1].sequence)
        );
        let value = serde_json::to_value(&changes).unwrap();
        for event in value.as_array().unwrap() {
            for forbidden in [
                "value",
                "secret_value",
                "plain_value",
                "credential_id",
                "source_baseline_credential_id",
                "keyring_username",
                "physical_slot",
                "source_digest",
                "error_code",
            ] {
                assert!(event.get(forbidden).is_none());
            }
        }
        let serialized = serde_json::to_string(&changes).unwrap();
        assert!(!serialized.contains(&publication.physical_slot));
        assert!(!format!("{changes:?}").contains(&publication.physical_slot));
        let remote = linked(&db, &a);
        db.project_key_github_enqueue(&a, DEV, &remote.id, remote.revision, false, 10)
            .unwrap();
        let entry = db.project_key_github_claim(10, 60, 1).unwrap().remove(0);
        db.project_key_github_ack_at(
            &entry,
            GitHubSendOutcome::Sent {
                remote_updated_at: None,
            },
            11,
        )
        .unwrap();
        assert_eq!(
            db.project_key_changes(&a, DEV, &remote.id).unwrap()[0].kind,
            ProjectKeyChangeKind::RemoteChanged
        );
    }
    #[test]
    fn project_keys_legacy_plain_delete_fences_old_read_and_allows_fresh_metadata_cleanup() {
        let (db, a, _) = database();
        let profile = db.insert_env_profile(&a, "custom", "custom").unwrap();
        db.upsert_env_var(
            &profile,
            "PUBLIC_CONFIG",
            &EnvValue::Plain("configuration".into()),
        )
        .unwrap();
        let mut key = db.project_key_list(&a, DEV).unwrap().remove(0);
        assert_eq!(
            db.project_key_legacy_configuration_value(&a, DEV, &key.id, 1)
                .unwrap()
                .as_deref(),
            Some("configuration")
        );
        db.delete_env_var(&profile, "PUBLIC_CONFIG").unwrap();
        let deleted = db.project_key_get(&a, DEV, &key.id).unwrap().unwrap();
        assert_eq!(deleted.revision, 2);
        assert_eq!(deleted.storage_state, KeyStorageState::MissingSource);
        assert!(
            db.project_key_legacy_configuration_value(&a, DEV, &key.id, 1)
                .unwrap()
                .is_none()
        );
        key.credential_id = Some("stale-deleted-configuration".into());
        let commit = detached_commit(&db, &a, "stale-deleted-configuration");
        assert_eq!(
            db.project_key_commit_value(&a, DEV, &key, Some(1), &commit)
                .unwrap(),
            ProjectKeyCas::Stale {
                current_revision: Some(2)
            }
        );
        assert!(
            db.credential_secret_location("stale-deleted-configuration")
                .unwrap()
                .is_none()
        );
        committed(
            db.project_key_delete(&a, DEV, &deleted.id, deleted.revision)
                .unwrap(),
        );
        assert!(db.project_key_get(&a, DEV, &deleted.id).unwrap().is_none());
    }
    #[test]
    fn project_keys_legacy_profile_delete_advances_its_plain_key_generation() {
        let (mut db, a, _) = database();
        let profile = db.insert_env_profile(&a, "custom", "custom").unwrap();
        db.upsert_env_var(&profile, "CONFIG", &EnvValue::Plain("configuration".into()))
            .unwrap();
        let key = db.project_key_list(&a, DEV).unwrap().remove(0);
        db.delete_env_profile(&profile).unwrap();
        let changed = db.project_key_get(&a, DEV, &key.id).unwrap().unwrap();
        assert_eq!(changed.revision, key.revision + 1);
        assert_eq!(changed.storage_state, KeyStorageState::MissingSource);
        committed(
            db.project_key_delete(&a, DEV, &changed.id, changed.revision)
                .unwrap(),
        );
    }
    #[test]
    fn project_keys_old_credential_publication_advances_value_generation_and_fences_old_sends() {
        let (db, a, _) = database();
        let mut key = linked(&db, &a);
        key.github.as_mut().unwrap().target.auto_send = true;
        key = committed(db.project_key_save(&a, DEV, &key, Some(2)).unwrap());
        let claim = db.project_key_github_claim(10, 60, 1).unwrap().remove(0);
        let logical =
            secret::LogicalCredentialId::new(key.credential_id.as_deref().unwrap()).unwrap();
        let physical = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        db.register_physical_secret_slot_staging(logical.as_str(), physical.as_str())
            .unwrap();
        assert!(
            db.publish_credential_secret_slot_cas(
                logical.as_str(),
                logical.as_str(),
                physical.as_str(),
                None,
                None
            )
            .unwrap()
        );
        let changed = db.project_key_get(&a, DEV, &key.id).unwrap().unwrap();
        assert_eq!(changed.revision, key.revision + 1);
        assert_eq!(changed.last_good_revision, Some(changed.revision));
        assert_eq!(changed.storage_state, KeyStorageState::Stored);
        assert!(
            db.project_key_value_location(&a, DEV, &key.id, key.revision)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.project_key_value_location(&a, DEV, &key.id, changed.revision)
                .unwrap()
                .unwrap()
                .1
                .keyring_username,
            physical.as_str()
        );
        assert!(!db.project_key_github_claim_is_current(&claim, 11).unwrap());
        assert!(
            !db.project_key_github_ack_at(
                &claim,
                GitHubSendOutcome::Sent {
                    remote_updated_at: None
                },
                11
            )
            .unwrap()
        );
        let mut settings = db.project_key_settings(&a, DEV).unwrap();
        settings.watch_enabled = false;
        committed(
            db.project_key_save_settings(&a, DEV, &settings, Some(1))
                .unwrap(),
        );
        assert_eq!(db.project_key_github_reconcile_pending(12, 1).unwrap(), 1);
        assert_eq!(db.project_key_github_reconcile_pending(12, 1).unwrap(), 0);
        let next_send = db.project_key_github_claim(12, 60, 1).unwrap().remove(0);
        assert_eq!(next_send.key_revision, changed.revision);
        assert!(
            db.project_key_github_claim_is_current(&next_send, 13)
                .unwrap()
        );
        assert!(
            db.project_key_github_ack_at(
                &next_send,
                GitHubSendOutcome::Sent {
                    remote_updated_at: Some("sent-new-generation".into())
                },
                13
            )
            .unwrap()
        );
        let changed = db.project_key_get(&a, DEV, &key.id).unwrap().unwrap();
        assert_eq!(
            changed.github.as_ref().unwrap().last_sent_revision,
            Some(changed.revision)
        );
        let mut file = changed.clone();
        file.source = KeySource::Dotenv {
            file: ".env".into(),
        };
        file.source_baseline_credential_id = file.credential_id.clone();
        file = committed(
            db.project_key_save(&a, DEV, &file, Some(file.revision))
                .unwrap(),
        );
        db.project_key_github_observe_cas(
            &a,
            DEV,
            &file.id,
            file.revision,
            db.project_key_get(&a, DEV, &file.id)
                .unwrap()
                .unwrap()
                .github
                .as_ref()
                .unwrap(),
            Some("external".into()),
            100,
        )
        .unwrap();
        let next = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        db.register_physical_secret_slot_staging(logical.as_str(), next.as_str())
            .unwrap();
        assert!(
            db.publish_credential_secret_slot_cas(
                logical.as_str(),
                physical.as_str(),
                next.as_str(),
                None,
                None
            )
            .unwrap()
        );
        let changed = db.project_key_get(&a, DEV, &key.id).unwrap().unwrap();
        assert_eq!(changed.storage_state, KeyStorageState::UpdateNeeded);
        assert_eq!(changed.revision, file.revision + 1);
        assert!(changed.source_baseline_credential_id.is_none());
        assert_eq!(
            changed.github.as_ref().unwrap().state,
            GitHubRemoteState::ExternalChange
        );
        assert_eq!(db.project_key_github_reconcile_pending(14, 128).unwrap(), 0);
        assert!(db.project_key_github_claim(14, 60, 1).unwrap().is_empty());
    }
    #[test]
    fn project_keys_delete_removes_only_selected_legacy_binding_and_retires_unused_owned_value() {
        let (db, a, b) = database();
        db.insert_credential(&CredentialMeta {
            id: "shared-delete".into(),
            provider: "github".into(),
            label: "shared".into(),
            credential_kind: "token".into(),
            masked_hint: None,
            workspace_id: None,
        })
        .unwrap();
        db.set_credential_env_binding(&a, "shared-delete", Some("TOKEN"))
            .unwrap();
        db.set_credential_env_binding(&b, "shared-delete", Some("TOKEN"))
            .unwrap();
        let selected = db.project_key_list(&a, DEV).unwrap().remove(0);
        let other = db.project_key_list(&b, DEV).unwrap().remove(0);
        committed(
            db.project_key_delete(&a, DEV, &selected.id, selected.revision)
                .unwrap(),
        );
        assert!(db.list_credential_env_bindings(&a).unwrap().is_empty());
        assert_eq!(
            db.list_credential_env_bindings(&b).unwrap()[0].credential_id,
            "shared-delete"
        );
        assert!(db.project_key_get(&b, DEV, &other.id).unwrap().is_some());
        assert!(
            db.credential_secret_location("shared-delete")
                .unwrap()
                .is_some()
        );
        let mut owned = key(&a, "owned-delete-key", "OWNED_TOKEN");
        owned.credential_id = Some("owned-delete-value".into());
        let publication = detached_commit(&db, &a, "owned-delete-value");
        let owned = committed(
            db.project_key_commit_value(&a, DEV, &owned, None, &publication)
                .unwrap(),
        );
        committed(
            db.project_key_delete(&a, DEV, &owned.id, owned.revision)
                .unwrap(),
        );
        assert!(
            db.credential_secret_location("owned-delete-value")
                .unwrap()
                .is_none()
        );
        let ledger = db.physical_secret_slots_for_reconciliation(8).unwrap();
        assert_eq!(
            ledger
                .iter()
                .find(|entry| entry.physical_slot == publication.physical_slot)
                .unwrap()
                .state,
            PhysicalSecretSlotState::Orphan
        );
    }
    #[test]
    fn project_keys_delete_legacy_env_row_preserves_other_alias_and_fences_changed_binding() {
        let (db, a, _) = database();
        credential(&db, &a, "shared-owned");
        let first = db.insert_env_profile(&a, ".env", "dotenv").unwrap();
        let second = db.insert_env_profile(&a, ".env.local", "dotenv").unwrap();
        for profile in [&first, &second] {
            db.upsert_env_var(
                profile,
                "TOKEN",
                &EnvValue::Secret {
                    credential_id: "shared-owned".into(),
                },
            )
            .unwrap();
        }
        let keys = db.project_key_list(&a, DEV).unwrap();
        let selected = keys
            .iter()
            .find(|key| {
                key.source
                    == KeySource::Dotenv {
                        file: ".env".into(),
                    }
            })
            .unwrap();
        let other = keys.iter().find(|key| key.id != selected.id).unwrap();
        committed(
            db.project_key_delete(&a, DEV, &selected.id, selected.revision)
                .unwrap(),
        );
        assert!(db.list_env_vars(&first).unwrap().is_empty());
        assert_eq!(db.list_env_vars(&second).unwrap().len(), 1);
        assert!(
            db.credential_secret_location("shared-owned")
                .unwrap()
                .is_some()
        );
        credential(&db, &a, "changed-binding");
        db.upsert_env_var(
            &second,
            "TOKEN",
            &EnvValue::Secret {
                credential_id: "changed-binding".into(),
            },
        )
        .unwrap();
        assert_eq!(
            db.project_key_delete(&a, DEV, &other.id, other.revision)
                .unwrap(),
            ProjectKeyCas::Stale {
                current_revision: Some(other.revision + 1)
            }
        );
        assert!(db.project_key_get(&a, DEV, &other.id).unwrap().is_some());
        assert_eq!(
            db.list_env_vars(&second).unwrap()[0].value,
            EnvValue::Secret {
                credential_id: "changed-binding".into()
            }
        );
    }
    #[test]
    fn project_keys_review_invalid_auth_recovery_does_not_block_healthy_projects_and_repairs() {
        let (db, a, b) = database();
        let mut token = stored_key(&db, &a, "auth-token", "GITHUB_TOKEN");
        token.usage = KeyUsage::StoreOnly;
        token.required = false;
        token = committed(
            db.project_key_save(&a, DEV, &token, Some(token.revision))
                .unwrap(),
        );
        let mut invalid = stored_key(&db, &a, "invalid-auto", "BAD_VALUE");
        let mut configured = target();
        configured.auth_credential_id = token.credential_id.clone();
        configured.auto_send = true;
        invalid.github = Some(KeyGitHubStatus {
            target: configured.clone(),
            state: GitHubRemoteState::Pending,
            last_sent_revision: None,
            remote_updated_at: None,
            error_code: None,
        });
        invalid = committed(
            db.project_key_save(&a, DEV, &invalid, Some(invalid.revision))
                .unwrap(),
        );
        let mut invalid_queued = stored_key(&db, &a, "invalid-queued", "BAD_QUEUED");
        invalid_queued.github = Some(KeyGitHubStatus {
            target: configured,
            state: GitHubRemoteState::Pending,
            last_sent_revision: None,
            remote_updated_at: None,
            error_code: None,
        });
        invalid_queued = committed(
            db.project_key_save(&a, DEV, &invalid_queued, Some(invalid_queued.revision))
                .unwrap(),
        );
        let mut healthy = Vec::new();
        for id in ["healthy-queued", "healthy-missing"] {
            credential(&db, &b, id);
            let mut key = key(&b, id, &id.replace('-', "_").to_ascii_uppercase());
            key.environment = PROD;
            key.credential_id = Some(id.into());
            key.last_good_revision = Some(1);
            key.storage_state = KeyStorageState::Stored;
            let mut configured = target();
            configured.auto_send = true;
            key.github = Some(KeyGitHubStatus {
                target: configured,
                state: GitHubRemoteState::Pending,
                last_sent_revision: None,
                remote_updated_at: None,
                error_code: None,
            });
            healthy.push(committed(
                db.project_key_save(&b, PROD, &key, None).unwrap(),
            ));
        }
        token.usage = KeyUsage::Build;
        token = committed(
            db.project_key_save(&a, DEV, &token, Some(token.revision))
                .unwrap(),
        );
        assert!(
            db.project_key_save(&a, DEV, &invalid, Some(invalid.revision))
                .is_err()
        );
        for key in [&invalid, &healthy[1]] {
            let logical =
                secret::LogicalCredentialId::new(key.credential_id.as_deref().unwrap()).unwrap();
            let slot = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
            db.register_physical_secret_slot_staging(logical.as_str(), slot.as_str())
                .unwrap();
            assert!(
                db.publish_credential_secret_slot_cas(
                    logical.as_str(),
                    logical.as_str(),
                    slot.as_str(),
                    None,
                    None
                )
                .unwrap()
            );
        }
        let now = deppy_core::time::unix_secs_i64() + 5;
        assert_eq!(
            db.project_key_github_reconcile_pending(now, 128).unwrap(),
            1
        );
        let rejected = db.project_key_get(&a, DEV, &invalid.id).unwrap().unwrap();
        assert_eq!(
            rejected.github.as_ref().unwrap().state,
            GitHubRemoteState::Failed
        );
        assert_eq!(
            rejected.github.as_ref().unwrap().error_code.as_deref(),
            Some("project_key_github_auth_failed")
        );
        assert_eq!(rejected.revision, invalid.revision + 1);
        let ready = db.project_key_github_claim(now, 60, 16).unwrap();
        assert_eq!(ready.len(), 2);
        assert!(
            ready
                .iter()
                .all(|entry| entry.workspace_id == b && entry.environment == PROD)
        );
        for entry in ready {
            assert!(
                db.project_key_github_ack_at(
                    &entry,
                    GitHubSendOutcome::Sent {
                        remote_updated_at: None
                    },
                    now + 1
                )
                .unwrap()
            );
        }
        assert_eq!(
            db.project_key_github_reconcile_pending(now + 1, 1).unwrap(),
            0
        );
        assert!(
            db.project_key_github_claim(now + 1, 60, 16)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.project_key_get(&a, DEV, &invalid_queued.id)
                .unwrap()
                .unwrap()
                .github
                .unwrap()
                .state,
            GitHubRemoteState::Failed
        );
        token.usage = KeyUsage::StoreOnly;
        committed(
            db.project_key_save(&a, DEV, &token, Some(token.revision))
                .unwrap(),
        );
        assert_eq!(
            db.project_key_github_reconcile_pending(now + 2, 128)
                .unwrap(),
            2
        );
        let repaired = db.project_key_github_claim(now + 2, 60, 16).unwrap();
        assert_eq!(repaired.len(), 2);
        assert!(
            repaired
                .iter()
                .all(|entry| entry.workspace_id == a && entry.environment == DEV)
        );
        assert!(
            repaired
                .iter()
                .any(|entry| entry.key_id == invalid.id && entry.key_revision == rejected.revision)
        );
        for entry in repaired {
            assert!(
                db.project_key_github_ack_at(
                    &entry,
                    GitHubSendOutcome::Sent {
                        remote_updated_at: None
                    },
                    now + 3
                )
                .unwrap()
            );
        }
        assert_eq!(
            db.project_key_get(&a, DEV, &invalid.id)
                .unwrap()
                .unwrap()
                .github
                .unwrap()
                .last_sent_revision,
            Some(rejected.revision)
        );
    }
    #[test]
    fn project_keys_remote_only_metadata_can_be_created_before_linking_without_local_value() {
        let (db, a, _) = database();
        let mut remote = key(&a, "remote-only", "REMOTE_TOKEN");
        remote.source = KeySource::GitHubOnly;
        remote.usage = KeyUsage::StoreOnly;
        remote.required = false;
        let remote = committed(db.project_key_save(&a, DEV, &remote, None).unwrap());
        assert!(remote.github.is_none());
        assert!(remote.credential_id.is_none());
        assert!(remote.last_good_revision.is_none());
        assert!(remote.last_good_at.is_none());
        assert!(
            db.project_key_value_location(&a, DEV, &remote.id, remote.revision)
                .unwrap()
                .is_none()
        );
        assert!(
            !db.project_key_github_enqueue(&a, DEV, &remote.id, remote.revision, false, 10)
                .unwrap()
        );
        let mut linked = remote.clone();
        linked.github = Some(KeyGitHubStatus {
            target: target(),
            state: GitHubRemoteState::Unlinked,
            last_sent_revision: None,
            remote_updated_at: None,
            error_code: None,
        });
        let linked = committed(
            db.project_key_save(&a, DEV, &linked, Some(remote.revision))
                .unwrap(),
        );
        assert!(
            db.project_key_github_enqueue(&a, DEV, &linked.id, linked.revision, false, 10)
                .is_err()
        );
        assert!(db.project_key_github_claim(10, 60, 16).unwrap().is_empty());
        assert_eq!(
            db.project_key_changes(&a, DEV, &remote.id)
                .unwrap()
                .last()
                .unwrap()
                .kind,
            ProjectKeyChangeKind::Created
        );
    }
    #[test]
    fn project_keys_remote_observation_cas_fences_own_send_ack_scope_and_cancels_external_work() {
        let (db, a, b) = database();
        let key = linked(&db, &a);
        db.project_key_github_enqueue(&a, DEV, &key.id, key.revision, false, 10)
            .unwrap();
        let before = db.project_key_get(&a, DEV, &key.id).unwrap().unwrap();
        let expected = before.github.as_ref().unwrap();
        let entry = db.project_key_github_claim(10, 60, 1).unwrap().remove(0);
        // The timestamp of our own write can be visible while its acknowledgment is still pending.
        assert!(
            !db.project_key_github_observe_cas(
                &a,
                DEV,
                &key.id,
                key.revision,
                expected,
                Some("own-write".into()),
                11
            )
            .unwrap()
        );
        assert!(db.project_key_github_claim_is_current(&entry, 12).unwrap());
        assert!(
            db.project_key_github_ack_at(
                &entry,
                GitHubSendOutcome::Sent {
                    remote_updated_at: Some("own-write".into())
                },
                12
            )
            .unwrap()
        );
        let sent = db.project_key_get(&a, DEV, &key.id).unwrap().unwrap();
        let sent_status = sent.github.as_ref().unwrap();
        assert!(
            !db.project_key_github_observe_cas(
                &a,
                DEV,
                &key.id,
                key.revision,
                expected,
                Some("old-poll".into()),
                13
            )
            .unwrap()
        );
        assert_eq!(db.project_key_get(&a, DEV, &key.id).unwrap().unwrap(), sent);
        let changes = db.project_key_changes(&a, DEV, &key.id).unwrap();
        assert!(
            db.project_key_github_observe_cas(
                &a,
                DEV,
                &key.id,
                key.revision,
                sent_status,
                Some("own-write".into()),
                13
            )
            .unwrap()
        );
        assert_eq!(db.project_key_changes(&a, DEV, &key.id).unwrap(), changes);
        assert!(
            !db.project_key_github_observe_cas(
                &b,
                DEV,
                &key.id,
                key.revision,
                sent_status,
                Some("external".into()),
                13
            )
            .unwrap()
        );
        assert!(
            !db.project_key_github_observe_cas(
                &a,
                PROD,
                &key.id,
                key.revision,
                sent_status,
                Some("external".into()),
                13
            )
            .unwrap()
        );
        assert!(
            !db.project_key_github_observe_cas(
                &a,
                DEV,
                &key.id,
                key.revision + 1,
                sent_status,
                Some("external".into()),
                13
            )
            .unwrap()
        );
        let mut wrong_target = sent_status.clone();
        wrong_target.target.secret_name = "OTHER_TARGET".into();
        assert!(
            !db.project_key_github_observe_cas(
                &a,
                DEV,
                &key.id,
                key.revision,
                &wrong_target,
                Some("external".into()),
                13
            )
            .unwrap()
        );
        assert!(
            db.project_key_github_observe_cas(
                &a,
                DEV,
                &key.id,
                key.revision,
                sent_status,
                Some("external".into()),
                14
            )
            .unwrap()
        );
        let changed = db.project_key_get(&a, DEV, &key.id).unwrap().unwrap();
        assert_eq!(changed.revision, sent.revision);
        assert_eq!(changed.last_good_revision, sent.last_good_revision);
        assert_eq!(changed.last_good_at, sent.last_good_at);
        assert_eq!(
            changed.github.as_ref().unwrap().last_sent_revision,
            Some(sent.revision)
        );
        assert_eq!(
            changed.github.as_ref().unwrap().state,
            GitHubRemoteState::ExternalChange
        );
        db.project_key_github_enqueue(&a, DEV, &key.id, key.revision, true, 15)
            .unwrap();
        let queued = db.project_key_get(&a, DEV, &key.id).unwrap().unwrap();
        assert!(
            db.project_key_github_observe_cas(
                &a,
                DEV,
                &key.id,
                key.revision,
                queued.github.as_ref().unwrap(),
                Some("external-again".into()),
                16
            )
            .unwrap()
        );
        assert!(db.project_key_github_claim(16, 60, 1).unwrap().is_empty());
        assert_eq!(
            db.project_key_get(&a, DEV, &key.id)
                .unwrap()
                .unwrap()
                .github
                .unwrap()
                .last_sent_revision,
            Some(sent.revision)
        );
    }
    #[test]
    fn project_keys_remote_observer_cursor_includes_inactive_scopes_and_remote_only_without_values()
    {
        let (db, a, b) = database();
        let first = linked(&db, &a);
        let mut settings = db.project_key_settings(&a, DEV).unwrap();
        settings.watch_enabled = false;
        committed(
            db.project_key_save_settings(&a, DEV, &settings, Some(1))
                .unwrap(),
        );
        let mut production = key(&b, "observer-production", "PROD_TOKEN");
        production.environment = PROD;
        production.github = Some(KeyGitHubStatus {
            target: target(),
            state: GitHubRemoteState::Unlinked,
            last_sent_revision: None,
            remote_updated_at: None,
            error_code: None,
        });
        committed(db.project_key_save(&b, PROD, &production, None).unwrap());
        let mut remote = key(&b, "observer-remote-only", "REMOTE_TOKEN");
        remote.source = KeySource::GitHubOnly;
        remote.usage = KeyUsage::StoreOnly;
        remote.required = false;
        remote.github = production.github.clone();
        let remote = committed(db.project_key_save(&b, DEV, &remote, None).unwrap());
        committed(
            db.project_key_save(&a, DEV, &key(&a, "not-observed", "UNLINKED_TOKEN"), None)
                .unwrap(),
        );
        let mut observed = Vec::new();
        let mut cursor = None;
        loop {
            let page = db
                .project_key_github_observer_page(1, cursor.as_deref())
                .unwrap();
            if page.is_empty() {
                break;
            }
            cursor = Some(page[0].id.clone());
            observed.extend(page);
        }
        assert_eq!(observed.len(), 3);
        assert!(observed.windows(2).all(|pair| pair[0].id < pair[1].id));
        assert!(observed.iter().any(|key| key.id == first.id));
        assert!(
            observed
                .iter()
                .any(|key| key.workspace_id == b && key.environment == PROD)
        );
        assert!(db.project_key_github_observer_page(17, None).is_err());
        assert!(
            db.project_key_github_observe_cas(
                &b,
                DEV,
                &remote.id,
                remote.revision,
                remote.github.as_ref().unwrap(),
                Some("observed-remote-only".into()),
                20
            )
            .unwrap()
        );
        let remote = db.project_key_get(&b, DEV, &remote.id).unwrap().unwrap();
        assert_eq!(
            remote.github.as_ref().unwrap().state,
            GitHubRemoteState::ExternalChange
        );
        assert_eq!(
            remote.github.as_ref().unwrap().remote_updated_at.as_deref(),
            Some("observed-remote-only")
        );
        assert!(remote.credential_id.is_none());
        assert!(remote.last_good_revision.is_none());
        assert!(remote.last_good_at.is_none());
        assert!(
            db.project_key_value_location(&b, DEV, &remote.id, remote.revision)
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn project_keys_distinct_sources_keep_duplicate_names_visible_with_explicit_precedence() {
        let (db, a, _) = database();
        let mut first = key(&a, "first", "TOKEN");
        first.source = KeySource::Dotenv {
            file: ".env".into(),
        };
        let mut second = key(&a, "second", "TOKEN");
        second.source = KeySource::Dotenv {
            file: ".env.local".into(),
        };
        committed(db.project_key_save(&a, DEV, &first, None).unwrap());
        committed(db.project_key_save(&a, DEV, &second, None).unwrap());
        assert_eq!(db.project_key_list(&a, DEV).unwrap().len(), 2);
        let mut duplicate = key(&a, "third", "TOKEN");
        duplicate.source = first.source.clone();
        assert!(db.project_key_save(&a, DEV, &duplicate, None).is_err());
    }
    #[test]
    fn project_keys_legacy_reconciliation_tracks_advanced_names_disconnects_and_new_rows() {
        let (db, a, b) = database();
        credential(&db, &a, "first");
        db.set_credential_env_binding(&a, "first", Some("OLD_NAME"))
            .unwrap();
        let profile = db.insert_env_profile(&a, "custom", "custom").unwrap();
        db.upsert_env_var(
            &profile,
            "OLD_CONFIG",
            &EnvValue::Plain("PRIVATE_SENTINEL".into()),
        )
        .unwrap();
        let rows = db.project_key_list(&a, DEV).unwrap();
        let initial = rows.iter().find(|k| k.name == "OLD_NAME").unwrap().clone();
        let config = rows
            .iter()
            .find(|k| k.name == "OLD_CONFIG")
            .unwrap()
            .clone();
        db.set_credential_env_binding(&a, "first", Some("RENAMED"))
            .unwrap();
        let renamed = db
            .project_key_list(&a, DEV)
            .unwrap()
            .into_iter()
            .find(|k| k.id == initial.id)
            .unwrap();
        assert_eq!(renamed.name, "RENAMED");
        assert!(renamed.revision > initial.revision);
        assert_eq!(renamed.storage_state, KeyStorageState::Stored);
        assert!(legacy_binding_matches(&db.conn, &renamed).unwrap());
        let legacy_id = config
            .legacy_reference
            .as_deref()
            .unwrap()
            .strip_prefix("env_var:")
            .unwrap();
        db.conn
            .execute(
                "UPDATE env_vars SET key='NEW_CONFIG' WHERE id=?1",
                [legacy_id],
            )
            .unwrap();
        let changed = db
            .project_key_list(&a, DEV)
            .unwrap()
            .into_iter()
            .find(|k| k.id == config.id)
            .unwrap();
        assert_eq!(changed.name, "NEW_CONFIG");
        assert_eq!(
            db.project_key_legacy_configuration_value(&a, DEV, &changed.id, changed.revision)
                .unwrap()
                .as_deref(),
            Some("PRIVATE_SENTINEL")
        );
        db.set_credential_env_binding(&a, "first", None).unwrap();
        let missing = db
            .project_key_list(&a, DEV)
            .unwrap()
            .into_iter()
            .find(|k| k.id == initial.id)
            .unwrap();
        assert_eq!(missing.storage_state, KeyStorageState::MissingSource);
        assert!(missing.revision > renamed.revision);
        assert_eq!(
            db.project_key_list(&a, DEV)
                .unwrap()
                .into_iter()
                .find(|k| k.id == initial.id)
                .unwrap(),
            missing
        );
        credential(&db, &a, "second");
        db.set_credential_env_binding(&a, "second", Some("NEW_TOKEN"))
            .unwrap();
        db.upsert_env_var(
            &profile,
            "NEW_PUBLIC",
            &EnvValue::Plain("also-private".into()),
        )
        .unwrap();
        let production = db
            .insert_env_profile(&a, "production", "production")
            .unwrap();
        db.upsert_env_var(
            &production,
            "PROD_PUBLIC",
            &EnvValue::Plain("prod-private".into()),
        )
        .unwrap();
        let fresh = db.project_key_list(&a, DEV).unwrap();
        assert!(
            fresh
                .iter()
                .any(|k| k.name == "NEW_TOKEN" && k.storage_state == KeyStorageState::Stored)
        );
        assert!(fresh.iter().any(|k| k.name == "NEW_PUBLIC"));
        assert!(!fresh.iter().any(|k| k.name == "PROD_PUBLIC"));
        assert_eq!(
            db.project_key_list(&a, PROD).unwrap()[0].name,
            "PROD_PUBLIC"
        );
        assert!(db.project_key_list(&b, DEV).unwrap().is_empty());
        assert!(
            !serde_json::to_string(&fresh)
                .unwrap()
                .contains("PRIVATE_SENTINEL")
        );
    }
    #[test]
    fn project_keys_new_legacy_agent_values_and_source_order_preserve_environment_scope() {
        let (db, a, _) = database();
        for (environment, id) in [(DEV, "new-dev"), (PROD, "new-prod")] {
            let mut value = key(&a, id, "TOKEN");
            value.environment = environment;
            value.usage = KeyUsage::LegacyAgent;
            value.credential_id = Some(id.into());
            let publication = detached_commit(&db, &a, id);
            let saved = committed(
                db.project_key_commit_value(&a, environment, &value, None, &publication)
                    .unwrap(),
            );
            assert!(saved.legacy_reference.is_some());
            assert!(saved.source_checked_at.is_some());
            assert!(legacy_binding_matches(&db.conn, &saved).unwrap());
            assert_eq!(db.project_key_list(&a, environment).unwrap(), vec![saved]);
        }
        assert_eq!(
            db.list_credential_env_bindings(&a).unwrap()[0].credential_id,
            "new-dev"
        );
        assert_eq!(db.list_credential_env_bindings(&a).unwrap().len(), 1);
        assert!(
            db.set_credential_env_binding(&a, "new-prod", Some("PROD_LEAK"))
                .is_err()
        );
        let dev_profile = db.insert_env_profile(&a, "custom", "custom").unwrap();
        assert!(
            db.upsert_env_var(
                &dev_profile,
                "PROD_LEAK",
                &EnvValue::Secret {
                    credential_id: "new-prod".into()
                }
            )
            .is_err()
        );
        let mut settings = db.project_key_settings(&a, DEV).unwrap();
        settings.sources = vec![
            KeySource::Dotenv {
                file: ".env.local".into(),
            },
            KeySource::PemFile {
                path: "key.pem".into(),
            },
            KeySource::Dotenv {
                file: ".env".into(),
            },
        ];
        committed(
            db.project_key_save_settings(&a, DEV, &settings, Some(settings.revision))
                .unwrap(),
        );
        assert_eq!(db.env_source_files(&a).unwrap(), vec![".env.local", ".env"]);
        let mut prod = db.project_key_settings(&a, PROD).unwrap();
        prod.sources = vec![KeySource::Dotenv {
            file: ".env.production".into(),
        }];
        committed(
            db.project_key_save_settings(&a, PROD, &prod, Some(prod.revision))
                .unwrap(),
        );
        assert_eq!(db.env_source_files(&a).unwrap(), vec![".env.local", ".env"]);
    }
    #[test]
    fn project_keys_deleted_source_names_survive_restart_and_explicit_import_is_scoped() {
        let path = std::env::temp_dir().join(format!(
            "deppy-project-key-exclusions-{}.sqlite3",
            uuid::Uuid::new_v4()
        ));
        let db = Db::open(&path).unwrap();
        let a = db.create_workspace("a").unwrap();
        let b = db.create_workspace("b").unwrap();
        let mut value = key(&a, "file-value", "TOKEN");
        value.source = KeySource::Dotenv {
            file: ".env".into(),
        };
        let saved = committed(db.project_key_save(&a, DEV, &value, None).unwrap());
        assert!(matches!(
            db.project_key_delete(&a, DEV, &saved.id, saved.revision)
                .unwrap(),
            ProjectKeyCas::Committed(())
        ));
        let expected = ProjectKeySourceExclusion {
            source: saved.source.clone(),
            name: saved.name.clone(),
        };
        assert_eq!(
            db.project_key_source_exclusions(&a, DEV).unwrap(),
            vec![expected.clone()]
        );
        drop(db);
        let db = Db::open(&path).unwrap();
        assert_eq!(
            db.project_key_source_exclusions(&a, DEV).unwrap(),
            vec![expected]
        );
        assert!(
            db.project_key_source_exclusions(&a, PROD)
                .unwrap()
                .is_empty()
        );
        assert!(
            db.project_key_source_exclusions(&b, DEV)
                .unwrap()
                .is_empty()
        );
        assert!(
            !db.project_key_source_exclusion_clear(&a, PROD, &saved.source, "TOKEN")
                .unwrap()
        );
        assert!(
            db.project_key_source_exclusion_clear(&a, DEV, &saved.source, "TOKEN")
                .unwrap()
        );
        assert!(
            db.project_key_source_exclusions(&a, DEV)
                .unwrap()
                .is_empty()
        );
        for i in 0..256 {
            insert_source_exclusion(&db.conn, &a, DEV, &saved.source, &format!("NAME_{i}"))
                .unwrap();
        }
        insert_source_exclusion(&db.conn, &a, DEV, &saved.source, "NAME_0").unwrap();
        assert!(insert_source_exclusion(&db.conn, &a, DEV, &saved.source, "OVERFLOW").is_err());
        assert_eq!(
            db.project_key_source_exclusions(&a, DEV).unwrap().len(),
            256
        );
    }
    #[test]
    fn project_keys_abandoned_receipts_recover_exact_owner_without_touching_active_or_unknown() {
        use deppy_core::process_identity::{ProcessBirth, ProcessIdentity};
        let (db, a, b) = database();
        let dead = ProcessIdentity {
            pid: 123,
            birth: ProcessBirth {
                seconds: 1,
                microseconds: 2,
            },
        };
        let live = ProcessIdentity {
            pid: 123,
            birth: ProcessBirth {
                seconds: 3,
                microseconds: 4,
            },
        };
        for i in 0..32 {
            let receipt = ProjectBuildReceipt {
                owner: if i == 0 {
                    None
                } else {
                    Some(if i == 1 { live } else { dead })
                },
                id: format!("receipt-{i:02}"),
                workspace_id: a.clone(),
                environment: DEV,
                recipe_id: None,
                started_at: 10,
                finished_at: None,
                exit_code: None,
                used_keys: vec![],
            };
            db.project_key_receipt_save(&receipt).unwrap();
        }
        let next = ProjectBuildReceipt {
            owner: Some(live),
            id: "receipt-new".into(),
            workspace_id: a.clone(),
            environment: DEV,
            recipe_id: None,
            started_at: 20,
            finished_at: None,
            exit_code: None,
            used_keys: vec![],
        };
        assert!(db.project_key_receipt_save(&next).is_err());
        assert!(
            !db.project_key_receipt_recover_abandoned(&a, DEV, "receipt-00", dead, 30)
                .unwrap()
        );
        assert!(
            !db.project_key_receipt_recover_abandoned(&a, DEV, "receipt-01", dead, 30)
                .unwrap()
        );
        assert!(
            !db.project_key_receipt_recover_abandoned(&b, DEV, "receipt-02", dead, 30)
                .unwrap()
        );
        assert!(
            !db.project_key_receipt_recover_abandoned(&a, PROD, "receipt-02", dead, 30)
                .unwrap()
        );
        let first = db.project_key_running_receipts_page(16, None).unwrap();
        assert_eq!(first.len(), 16);
        let second = db
            .project_key_running_receipts_page(16, Some(&first.last().unwrap().id))
            .unwrap();
        assert_eq!(second.len(), 16);
        assert!(
            db.project_key_receipt_recover_abandoned(&a, DEV, "receipt-02", dead, 30)
                .unwrap()
        );
        assert!(
            !db.project_key_receipt_recover_abandoned(&a, DEV, "receipt-02", dead, 31)
                .unwrap()
        );
        db.project_key_receipt_save(&next).unwrap();
        let recovered = db
            .project_key_receipt_list(&a, DEV, 100)
            .unwrap()
            .into_iter()
            .find(|r| r.id == "receipt-02")
            .unwrap();
        assert_eq!(recovered.finished_at, Some(30));
        assert_eq!(recovered.exit_code, Some(-1));
        assert!(db.project_key_running_receipts_page(17, None).is_err());
    }
    #[test]
    fn project_keys_file_journal_preflights_and_pins_recovery_until_atomic_publication() {
        let path = std::env::temp_dir().join(format!(
            "deppy-project-key-journal-{}.sqlite3",
            uuid::Uuid::new_v4()
        ));
        let db = Db::open(&path).unwrap();
        let a = db.create_workspace("a").unwrap();
        let b = db.create_workspace("b").unwrap();
        let mut value = key(&a, "file-key", "TOKEN");
        value.source = KeySource::Dotenv {
            file: ".env".into(),
        };
        value.credential_id = Some("owned-value".into());
        let commit = detached_commit(&db, &a, "owned-value");
        let journal_slot = detached_commit(&db, &a, "journal-one").physical_slot;
        assert!(matches!(
            db.project_key_file_journal_begin(
                &a,
                DEV,
                &value,
                Some(1),
                "journal-one",
                &journal_slot,
                10
            )
            .unwrap(),
            ProjectKeyCas::Stale { .. }
        ));
        let journal = committed(
            db.project_key_file_journal_begin(
                &a,
                DEV,
                &value,
                None,
                "journal-one",
                &journal_slot,
                10,
            )
            .unwrap(),
        );
        assert_eq!(journal.phase, ProjectKeyFileJournalPhase::Prepared);
        assert!(journal.owner.is_some());
        let legacy_profile = db.insert_env_profile(&a, ".env", "dotenv").unwrap();
        assert!(
            !db.project_key_dotenv_upsert_unmanaged(
                &legacy_profile,
                "PUBLIC_VALUE",
                &EnvValue::Plain("public-config".into())
            )
            .unwrap()
        );
        assert!(
            !db.project_key_dotenv_delete_unmanaged(&legacy_profile, "PUBLIC_VALUE")
                .unwrap()
        );
        assert!(
            db.project_key_file_journal_source_pending(&a, DEV, &value.source)
                .unwrap()
        );
        assert!(
            db.project_key_file_journal_source_pending(&a, PROD, &value.source)
                .unwrap()
        );
        assert!(
            !db.project_key_file_journal_source_pending(&b, DEV, &value.source)
                .unwrap()
        );
        assert_eq!(
            db.project_key_file_journal_get(&a, DEV, "journal-one")
                .unwrap()
                .unwrap()
                .phase,
            ProjectKeyFileJournalPhase::Prepared
        );
        assert!(
            db.project_key_file_journal_get(&b, DEV, "journal-one")
                .unwrap()
                .is_none()
        );
        let orphan = db
            .physical_secret_slots_for_reconciliation(10)
            .unwrap()
            .into_iter()
            .find(|r| r.physical_slot == journal_slot)
            .unwrap();
        let mut cleanup_called = false;
        assert!(
            !db.recover_physical_secret_slot_cas(&orphan, || {
                cleanup_called = true;
                Ok(())
            })
            .unwrap()
        );
        assert!(!cleanup_called);
        assert!(
            !db.project_key_file_journal_mark_applied(&b, DEV, "journal-one")
                .unwrap()
        );
        assert!(
            db.project_key_file_journal_mark_applied(&a, DEV, "journal-one")
                .unwrap()
        );
        drop(db);
        let db = Db::open(&path).unwrap();
        let recovered = db.project_key_file_journals_page(16, None).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].phase, ProjectKeyFileJournalPhase::FileApplied);
        let saved = committed(
            db.project_key_commit_value_with_file_journal(
                &a,
                DEV,
                &value,
                None,
                &commit,
                "journal-one",
            )
            .unwrap(),
        );
        assert_eq!(saved.storage_state, KeyStorageState::Stored);
        assert!(
            db.project_key_dotenv_upsert_unmanaged(
                &legacy_profile,
                "PUBLIC_VALUE",
                &EnvValue::Plain("public-config".into())
            )
            .unwrap()
        );
        assert!(
            !db.project_key_file_journal_source_pending(&a, DEV, &value.source)
                .unwrap()
        );
        assert_eq!(
            db.project_key_file_journals_page(16, None).unwrap()[0].phase,
            ProjectKeyFileJournalPhase::PublicationComplete
        );
        assert!(
            !db.project_key_file_journal_finish(
                &a,
                DEV,
                "journal-one",
                ProjectKeyFileJournalPhase::FileApplied
            )
            .unwrap()
        );
        assert!(
            db.project_key_file_journal_finish(
                &a,
                DEV,
                "journal-one",
                ProjectKeyFileJournalPhase::PublicationComplete
            )
            .unwrap()
        );
        assert!(
            db.recover_physical_secret_slot_cas(&orphan, || Ok(()))
                .unwrap()
        );
        let duplicate_slot = detached_commit(&db, &a, "journal-two").physical_slot;
        let mut duplicate = value.clone();
        duplicate.id = "duplicate".into();
        assert!(
            db.project_key_file_journal_begin(
                &a,
                DEV,
                &duplicate,
                None,
                "journal-two",
                &duplicate_slot,
                20
            )
            .is_err()
        );
        assert!(
            db.project_key_file_journals_page(16, None)
                .unwrap()
                .is_empty()
        );
        assert!(db.project_key_file_journals_page(17, None).is_err());
        drop(db);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn project_keys_metadata_keep_claim_is_durable_and_failed_cas_never_claims() {
        let (db, a, _) = database();
        let profile = db.insert_env_profile(&a, ".env", "dotenv").unwrap();
        db.upsert_env_var(&profile, "PORT", &EnvValue::Plain("3000".into()))
            .unwrap();
        let mut key = db.project_key_list(&a, DEV).unwrap().remove(0);
        assert!(db.project_key_managed_ids(&a, DEV).unwrap().is_empty());
        key.required = true;
        assert!(matches!(
            db.project_key_save(&a, DEV, &key, Some(key.revision + 1))
                .unwrap(),
            ProjectKeyCas::Stale { .. }
        ));
        assert!(db.project_key_managed_ids(&a, DEV).unwrap().is_empty());
        let saved = committed(
            db.project_key_save(&a, DEV, &key, Some(key.revision))
                .unwrap(),
        );
        assert!(saved.source_checked_at.is_none());
        assert_eq!(
            db.project_key_managed_ids(&a, DEV).unwrap(),
            std::slice::from_ref(&saved.id)
        );
        // Reconciliation preserves an explicit metadata-only claim without inventing source I/O.
        assert_eq!(
            db.project_key_list(&a, DEV).unwrap(),
            std::slice::from_ref(&saved)
        );
        assert!(
            !db.project_key_dotenv_upsert_unmanaged(
                &profile,
                "PORT",
                &EnvValue::Plain("4000".into())
            )
            .unwrap()
        );
        assert!(
            !db.project_key_dotenv_delete_unmanaged(&profile, "PORT")
                .unwrap()
        );
        db.upsert_env_var(&profile, "PORT", &EnvValue::Plain("5000".into()))
            .unwrap();
        db.project_key_list(&a, DEV).unwrap();
        assert_eq!(db.project_key_managed_ids(&a, DEV).unwrap(), [saved.id]);
        assert!(
            db.list_env_vars(&profile)
                .unwrap()
                .iter()
                .any(|row| { row.key == "PORT" && row.value == EnvValue::Plain("5000".into()) })
        );
    }
    #[test]
    fn project_keys_linked_legacy_projection_is_managed_before_value_publication() {
        let (db, a, _) = database();
        credential(&db, &a, "ordinary-token");
        let profile = db.insert_env_profile(&a, ".env", "dotenv").unwrap();
        db.upsert_env_var(
            &profile,
            "TOKEN",
            &EnvValue::Secret {
                credential_id: "ordinary-token".into(),
            },
        )
        .unwrap();
        let mut key = db.project_key_list(&a, DEV).unwrap().remove(0);
        assert!(db.project_key_managed_ids(&a, DEV).unwrap().is_empty());
        key.github = Some(KeyGitHubStatus {
            target: target(),
            state: GitHubRemoteState::Pending,
            last_sent_revision: None,
            remote_updated_at: None,
            error_code: None,
        });
        let linked = committed(
            db.project_key_save(&a, DEV, &key, Some(key.revision))
                .unwrap(),
        );
        assert!(linked.source_checked_at.is_none());
        assert_eq!(db.project_key_managed_ids(&a, DEV).unwrap(), [linked.id]);
        assert!(
            !db.project_key_dotenv_delete_unmanaged(&profile, "TOKEN")
                .unwrap(),
            "an explicit GitHub target cannot be pruned with an ordinary legacy plan"
        );
        assert_eq!(db.list_env_vars(&profile).unwrap().len(), 1);
    }
    #[test]
    fn project_keys_unowned_dotenv_projection_keeps_update_prune_and_secret_retirement() {
        let (db, a, _) = database();
        let profile = db.insert_env_profile(&a, ".env", "dotenv").unwrap();
        for name in ["REMOVED_TOKEN", "REPLACED_CONFIG"] {
            let staged = detached_commit(&db, &a, name);
            db.insert_credential_with_secret_slot(
                staged.credential_meta.as_ref().unwrap(),
                &staged.physical_slot,
                None,
            )
            .unwrap();
            db.upsert_env_var(
                &profile,
                name,
                &EnvValue::Secret {
                    credential_id: name.into(),
                },
            )
            .unwrap();
        }
        db.upsert_env_var(&profile, "PORT", &EnvValue::Plain("3000".into()))
            .unwrap();
        let introduced = db.project_key_list(&a, DEV).unwrap();
        assert!(db.project_key_managed_ids(&a, DEV).unwrap().is_empty());
        let port = introduced.iter().find(|k| k.name == "PORT").unwrap();
        let removed = introduced
            .iter()
            .find(|k| k.name == "REMOVED_TOKEN")
            .unwrap();
        let logical = secret::LogicalCredentialId::new("REMOVED_TOKEN").unwrap();
        let next = secret::PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        let old = db
            .project_key_value_location(&a, DEV, &removed.id, removed.revision)
            .unwrap()
            .unwrap();
        db.register_physical_secret_slot_staging(logical.as_str(), next.as_str())
            .unwrap();
        assert!(
            db.publish_credential_secret_slot_cas(
                logical.as_str(),
                &old.1.keyring_username,
                next.as_str(),
                None,
                None,
            )
            .unwrap()
        );
        // A legacy pointer rotation records a save time, but does not enroll project ownership.
        assert!(
            db.project_key_get(&a, DEV, &removed.id)
                .unwrap()
                .unwrap()
                .last_good_at
                .is_some()
        );
        assert!(db.project_key_managed_ids(&a, DEV).unwrap().is_empty());
        assert!(
            db.project_key_dotenv_upsert_unmanaged(
                &profile,
                "PORT",
                &EnvValue::Plain("4000".into())
            )
            .unwrap(),
            "lazy metadata introduction must not stop ordinary dotenv updates"
        );
        let same_port = db.project_key_get(&a, DEV, &port.id).unwrap().unwrap();
        assert_eq!(same_port.id, port.id);
        assert!(same_port.revision > port.revision);
        assert!(
            db.project_key_dotenv_upsert_unmanaged(
                &profile,
                "REMOVED_TOKEN",
                &EnvValue::Secret {
                    credential_id: "REMOVED_TOKEN".into()
                },
            )
            .unwrap()
        );
        assert!(db.project_key_get(&a, DEV, &removed.id).unwrap().is_some());
        assert!(
            db.project_key_dotenv_delete_unmanaged(&profile, "REMOVED_TOKEN")
                .unwrap()
        );
        assert!(db.project_key_get(&a, DEV, &removed.id).unwrap().is_none());
        assert!(
            db.delete_credential_if_unused_cas("REMOVED_TOKEN", next.as_str())
                .unwrap(),
            "a pruned unowned projection must not retain the old protected credential"
        );
        let retired: String = db
            .conn
            .query_row(
                "SELECT state FROM physical_secret_slot_ledger WHERE physical_slot=?1",
                [next.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retired, "orphan");
        let replaced_pointer: String = db
            .conn
            .query_row(
                "SELECT keyring_username FROM credentials WHERE id='REPLACED_CONFIG'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            db.project_key_dotenv_upsert_unmanaged(
                &profile,
                "REPLACED_CONFIG",
                &EnvValue::Plain("public-configuration".into()),
            )
            .unwrap()
        );
        assert!(
            db.delete_credential_if_unused_cas("REPLACED_CONFIG", &replaced_pointer)
                .unwrap(),
            "Secret to Plain must not leave an obsolete projection credential reference"
        );
        let refreshed = db.project_key_list(&a, DEV).unwrap();
        assert_eq!(refreshed.len(), 2);
        let plain = refreshed
            .iter()
            .find(|k| k.name == "REPLACED_CONFIG")
            .unwrap();
        assert_eq!(plain.kind, KeyKind::Configuration);
        assert!(plain.credential_id.is_none());
    }
    #[test]
    fn project_keys_late_dotenv_writer_cannot_replace_or_prune_managed_publication() {
        let (db, a, b) = database();
        credential(&db, &a, "preloaded-old");
        let profile = db.insert_env_profile(&a, ".env", "dotenv").unwrap();
        let preloaded = EnvValue::Secret {
            credential_id: "preloaded-old".into(),
        };
        db.upsert_env_var(&profile, "TOKEN", &preloaded).unwrap();
        let mut enrolled = db.project_key_list(&a, DEV).unwrap().remove(0);
        enrolled.credential_id = Some("managed-current".into());
        let commit = detached_commit(&db, &a, "managed-current");
        let saved = committed(
            db.project_key_commit_value(&a, DEV, &enrolled, Some(enrolled.revision), &commit)
                .unwrap(),
        );
        assert!(saved.source_checked_at.is_some());
        assert_eq!(
            db.project_key_managed_ids(&a, DEV).unwrap(),
            std::slice::from_ref(&saved.id)
        );
        assert!(db.project_key_managed_ids(&a, PROD).unwrap().is_empty());
        assert!(db.project_key_managed_ids(&b, DEV).unwrap().is_empty());
        // The old plan was prepared before this enrollment and arrives after the fresh protected value.
        assert!(
            !db.project_key_dotenv_upsert_unmanaged(&profile, "TOKEN", &preloaded)
                .unwrap()
        );
        assert!(
            !db.project_key_dotenv_delete_unmanaged(&profile, "TOKEN")
                .unwrap()
        );
        assert_eq!(
            db.list_env_vars(&profile).unwrap()[0].value,
            EnvValue::Secret {
                credential_id: "managed-current".into()
            }
        );
        assert_eq!(
            db.project_key_get(&a, DEV, &saved.id).unwrap(),
            Some(saved.clone())
        );
        assert!(
            db.project_key_value_location(&a, DEV, &saved.id, saved.revision)
                .unwrap()
                .is_some()
        );
        let excluded = KeySource::Dotenv {
            file: ".env".into(),
        };
        insert_source_exclusion(&db.conn, &a, DEV, &excluded, "REMOVED_TOKEN").unwrap();
        assert!(
            !db.project_key_dotenv_upsert_unmanaged(&profile, "REMOVED_TOKEN", &preloaded)
                .unwrap()
        );
        // A newly managed source/name is protected even before the old writer creates its env row.
        let mut fresh = key(&a, "source-only", "FRESH_TOKEN");
        fresh.source = KeySource::Dotenv {
            file: ".env".into(),
        };
        committed(db.project_key_save(&a, DEV, &fresh, None).unwrap());
        assert!(
            !db.project_key_dotenv_upsert_unmanaged(&profile, "FRESH_TOKEN", &preloaded)
                .unwrap()
        );
        // An unrelated workspace/source and normal unowned configuration retain old-writer behavior.
        let other = db.insert_env_profile(&b, ".env", "dotenv").unwrap();
        assert!(
            db.project_key_dotenv_upsert_unmanaged(
                &other,
                "PUBLIC_VALUE",
                &EnvValue::Plain("public-config".into())
            )
            .unwrap()
        );
        assert!(
            db.project_key_dotenv_upsert_unmanaged(
                &profile,
                "PUBLIC_VALUE",
                &EnvValue::Plain("public-config".into())
            )
            .unwrap()
        );
        assert!(
            db.project_key_dotenv_delete_unmanaged(&profile, "PUBLIC_VALUE")
                .unwrap()
        );
        assert_eq!(db.list_env_vars(&other).unwrap().len(), 1);
        // Advanced explicit edits are still authoritative and advance the existing generation fence.
        db.upsert_env_var(&profile, "TOKEN", &preloaded).unwrap();
        assert_eq!(db.list_env_vars(&profile).unwrap()[0].value, preloaded);
        let changed = db.project_key_get(&a, DEV, &saved.id).unwrap().unwrap();
        assert!(changed.revision > saved.revision);
        assert_eq!(changed.storage_state, KeyStorageState::UpdateNeeded);
        assert!(
            db.project_key_managed_ids(&a, DEV)
                .unwrap()
                .contains(&saved.id)
        );
        assert!(
            !db.project_key_dotenv_delete_unmanaged(&profile, "TOKEN")
                .unwrap()
        );
    }
    #[test]
    fn project_keys_selected_legacy_profile_coordinate_is_current_owned_and_metadata_only() {
        let (db, a, b) = database();
        let selected = db.insert_env_profile(&a, "selected", "custom").unwrap();
        let other = db.insert_env_profile(&a, "other", "custom").unwrap();
        db.upsert_env_var(
            &selected,
            "CONFIG",
            &EnvValue::Plain("PRIVATE_SELECTED_VALUE".into()),
        )
        .unwrap();
        db.upsert_env_var(
            &other,
            "CONFIG",
            &EnvValue::Plain("PRIVATE_OTHER_VALUE".into()),
        )
        .unwrap();
        let rows = db.project_key_list(&a, DEV).unwrap();
        let selected_key = rows
            .iter()
            .find(|key| {
                db.project_key_legacy_env_profile(&a, DEV, &key.id, key.revision)
                    .unwrap()
                    .as_deref()
                    == Some(selected.as_str())
            })
            .unwrap();
        assert_eq!(
            db.project_key_legacy_env_profile(&a, DEV, &selected_key.id, selected_key.revision)
                .unwrap(),
            Some(selected.clone())
        );
        assert!(
            db.project_key_legacy_env_profile(&a, DEV, &selected_key.id, selected_key.revision + 1)
                .unwrap()
                .is_none()
        );
        assert!(
            db.project_key_legacy_env_profile(&b, DEV, &selected_key.id, selected_key.revision)
                .unwrap()
                .is_none()
        );
        assert!(
            db.project_key_legacy_env_profile(&a, PROD, &selected_key.id, selected_key.revision)
                .unwrap()
                .is_none()
        );
        let legacy_id = selected_key
            .legacy_reference
            .as_deref()
            .unwrap()
            .strip_prefix("env_var:")
            .unwrap();
        db.conn
            .execute(
                "UPDATE env_vars SET key='RENAMED_CONFIG' WHERE id=?1",
                [legacy_id],
            )
            .unwrap();
        assert!(
            db.project_key_legacy_env_profile(&a, DEV, &selected_key.id, selected_key.revision)
                .unwrap()
                .is_none()
        );
        let current = db
            .project_key_get(&a, DEV, &selected_key.id)
            .unwrap()
            .unwrap();
        assert!(
            db.project_key_legacy_env_profile(&a, DEV, &current.id, current.revision)
                .unwrap()
                .is_none()
        );
        let current = db
            .project_key_list(&a, DEV)
            .unwrap()
            .into_iter()
            .find(|key| key.id == selected_key.id)
            .unwrap();
        assert_eq!(
            db.project_key_legacy_env_profile(&a, DEV, &current.id, current.revision)
                .unwrap(),
            Some(selected.clone())
        );
        assert!(
            !format!(
                "{:?}",
                db.project_key_legacy_env_profile(&a, DEV, &current.id, current.revision)
                    .unwrap()
            )
            .contains("PRIVATE_SELECTED_VALUE")
        );
        let production = db
            .insert_env_profile(&a, "production", "production")
            .unwrap();
        db.upsert_env_var(
            &production,
            "PROD_CONFIG",
            &EnvValue::Plain("PRIVATE_PRODUCTION_VALUE".into()),
        )
        .unwrap();
        let production_key = db.project_key_list(&a, PROD).unwrap().remove(0);
        assert_eq!(
            db.project_key_legacy_env_profile(
                &a,
                PROD,
                &production_key.id,
                production_key.revision
            )
            .unwrap(),
            Some(production.clone())
        );
        db.conn
            .execute(
                "UPDATE env_profiles SET kind='custom',is_production=0 WHERE id=?1",
                [&production],
            )
            .unwrap();
        assert!(
            db.project_key_legacy_env_profile(
                &a,
                PROD,
                &production_key.id,
                production_key.revision
            )
            .unwrap()
            .is_none()
        );
        let vault = stored_key(&db, &a, "unbound-vault", "TOKEN");
        assert!(
            db.project_key_legacy_env_profile(&a, DEV, &vault.id, vault.revision)
                .unwrap()
                .is_none()
        );
    }
}
