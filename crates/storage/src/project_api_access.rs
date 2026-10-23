//! API descriptions and explicit session grants. No secret-store access belongs here.
use super::*;
use deppy_core::{project_api_access::*, project_keys::*};

pub(crate) const MIGRATION: &str = r#"
CREATE TABLE project_api_access (
 key_id TEXT PRIMARY KEY REFERENCES project_keys(id) ON DELETE CASCADE,
 workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 environment TEXT NOT NULL CHECK(environment IN ('development','production')),
 revision INTEGER NOT NULL CHECK(revision>0),
 internal_identity INTEGER NOT NULL DEFAULT 0 CHECK(internal_identity IN (0,1)),
 metadata_json TEXT NOT NULL CHECK(length(CAST(metadata_json AS BLOB))<=8192)
);
CREATE INDEX idx_project_api_scope ON project_api_access(workspace_id,environment);
CREATE TABLE project_api_grants (
 key_id TEXT NOT NULL REFERENCES project_api_access(key_id) ON DELETE CASCADE,
 workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 environment TEXT NOT NULL CHECK(environment IN ('development','production')),
 principal_id TEXT NOT NULL, session_id TEXT NOT NULL, generation TEXT NOT NULL,
 actor_kind TEXT NOT NULL CHECK(actor_kind IN ('local_session','cloud_connection')),
 operation TEXT NOT NULL CHECK(operation IN ('environment','recipe')),
 recipe_id TEXT NOT NULL, access_revision INTEGER NOT NULL CHECK(access_revision>0),
 expires_at INTEGER NOT NULL CHECK(expires_at>=0),
 PRIMARY KEY(key_id,workspace_id,environment,principal_id,session_id,generation,actor_kind,operation,recipe_id)
);
CREATE INDEX idx_project_api_grant_scope ON project_api_grants(workspace_id,environment,principal_id,session_id,generation);
"#;

fn actor_name(actor: ProjectApiActorKind) -> &'static str {
    match actor {
        ProjectApiActorKind::LocalSession => "local_session",
        ProjectApiActorKind::CloudConnection => "cloud_connection",
    }
}
fn operation_name(operation: ProjectApiOperation) -> &'static str {
    match operation {
        ProjectApiOperation::Environment => "environment",
        ProjectApiOperation::Recipe => "recipe",
    }
}

impl Db {
    /// Create a valueless key and its deny policy together, before protected publication.
    pub fn project_api_prepare_new_key(
        &self,
        key: &ProjectKey,
        access: &ProjectApiAccess,
    ) -> anyhow::Result<ProjectKeyCas<ProjectKey>> {
        key.validate().map_err(anyhow::Error::msg)?;
        access.validate().map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            !access.agent_enabled
                && access.key_id == key.id
                && access.workspace_id == key.workspace_id
                && access.environment == key.environment
                && key.credential_id.is_none()
                && key.source_baseline_credential_id.is_none()
                && key.last_good_revision.is_none()
                && key.storage_state == KeyStorageState::NotStored
                && key.source == KeySource::Vault
                && access
                    .execution_name
                    .as_ref()
                    .is_none_or(|name| name == &key.name),
            "project_api_placeholder_invalid"
        );
        let internal = access.execution_name.is_none();
        if internal {
            let id = uuid::Uuid::parse_str(&key.id)?;
            anyhow::ensure!(
                key.usage == KeyUsage::StoreOnly
                    && key.name
                        == format!("DEPPY_API_{}", id.simple().to_string().to_ascii_uppercase()),
                "project_api_placeholder_invalid"
            );
        }
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let ProjectKeyCas::Committed(key) =
            super::project_keys::check_save(&tx, &key.workspace_id, key.environment, key, None)?
        else {
            return Ok(ProjectKeyCas::Stale {
                current_revision: self
                    .project_key_get(&key.workspace_id, key.environment, &key.id)?
                    .map(|key| key.revision),
            });
        };
        let key = super::project_keys::finish_save(&tx, key)?;
        let mut access = access.clone();
        access.revision = 1;
        tx.execute("INSERT INTO project_api_access(key_id,workspace_id,environment,revision,internal_identity,metadata_json) VALUES(?1,?2,?3,1,?4,?5)",rusqlite::params![access.key_id,access.workspace_id,access.environment.as_str(),internal,serde_json::to_string(&access)?])?;
        tx.commit()?;
        Ok(ProjectKeyCas::Committed(key))
    }

    /// Only an internally created, still-unbound API identity may receive its first binding.
    pub fn project_api_bind_execution_name(
        &self,
        access: &ProjectApiAccess,
        key_revision: u64,
        access_revision: u64,
        at: i64,
    ) -> anyhow::Result<ProjectKeyCas<ProjectKey>> {
        access.validate().map_err(anyhow::Error::msg)?;
        let name = access
            .execution_name
            .as_ref()
            .context("project_api_binding_missing")?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let mut key = self
            .project_key_get(&access.workspace_id, access.environment, &access.key_id)?
            .context("project_api_key_missing")?;
        let current = self
            .project_api_access_list(&access.workspace_id, access.environment)?
            .into_iter()
            .find(|row| row.key_id == access.key_id)
            .context("project_api_metadata_missing")?;
        if key.revision != key_revision || current.revision != access_revision {
            return Ok(ProjectKeyCas::Stale {
                current_revision: Some(key.revision),
            });
        }
        let internal: bool = tx.query_row(
            "SELECT internal_identity FROM project_api_access WHERE key_id=?1",
            [&key.id],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            internal
                && current.execution_name.is_none()
                && !current.agent_enabled
                && key.source == KeySource::Vault
                && key.usage == KeyUsage::StoreOnly
                && key.legacy_reference.is_none()
                && key.github.is_none()
                && at >= 0,
            "project_api_binding_immutable"
        );
        let duplicate:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM project_keys WHERE workspace_id=?1 AND environment=?2 AND id<>?3 AND name=?4 AND json_extract(payload_json,'$.source.type')='vault')",rusqlite::params![key.workspace_id,key.environment.as_str(),key.id,name],|row|row.get(0))?;
        anyhow::ensure!(!duplicate, "project_key_duplicate_name");
        key.name = name.clone();
        key.revision = key
            .revision
            .checked_add(1)
            .filter(|revision| valid_revision(*revision))
            .context("project_api_revision_exhausted")?;
        key.updated_at = key.updated_at.max(at);
        key.validate().map_err(anyhow::Error::msg)?;
        tx.execute(
            "UPDATE project_keys SET name=?2,revision=?3,payload_json=?4 WHERE id=?1",
            rusqlite::params![
                key.id,
                key.name,
                key.revision as i64,
                serde_json::to_string(&key)?
            ],
        )?;
        tx.execute(
            "UPDATE project_api_access SET internal_identity=0 WHERE key_id=?1",
            [&key.id],
        )?;
        tx.execute("DELETE FROM project_api_grants WHERE key_id=?1", [&key.id])?;
        tx.commit()?;
        Ok(ProjectKeyCas::Committed(key))
    }
    pub fn project_api_local_grants_refresh(
        &self,
        scope: &ProjectApiSessionScope,
        bearer_hash: &[u8; 32],
        now: i64,
    ) -> anyhow::Result<()> {
        scope.validate().map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            scope.actor_kind == ProjectApiActorKind::LocalSession && now >= 0,
            "project_api_scope_invalid"
        );
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        anyhow::ensure!(
            self.project_api_capability_allows(scope, bearer_hash, now)?,
            "project_api_permission_denied"
        );
        anyhow::ensure!(
            self.workspace_target(&scope.workspace_id)?
                .is_some_and(|target| target.local_root().is_some()),
            "project_api_remote_unavailable"
        );
        let expires:i64=tx.query_row("SELECT expires_at FROM project_api_capabilities WHERE workspace_id=?1 AND environment=?2 AND principal_id=?3 AND session_id=?4 AND generation=?5",rusqlite::params![scope.workspace_id,scope.environment.as_str(),scope.principal_id,scope.session_id,scope.generation],|row|row.get(0))?;
        tx.execute("DELETE FROM project_api_grants WHERE expires_at<=?1", [now])?;
        for access in self
            .project_api_access_list(&scope.workspace_id, scope.environment)?
            .into_iter()
            .filter(|access| access.agent_enabled)
        {
            let Some(key) =
                self.project_key_get(&scope.workspace_id, scope.environment, &access.key_id)?
            else {
                continue;
            };
            if access.execution_name.as_ref() != Some(&key.name)
                || key.source == KeySource::GitHubOnly
            {
                continue;
            }
            let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM project_api_grants WHERE key_id=?1 AND workspace_id=?2 AND environment=?3 AND principal_id=?4 AND session_id=?5 AND generation=?6 AND actor_kind='local_session' AND operation='environment' AND recipe_id='')",rusqlite::params![access.key_id,scope.workspace_id,scope.environment.as_str(),scope.principal_id,scope.session_id,scope.generation],|row|row.get(0))?;
            if !exists {
                let count: i64 =
                    tx.query_row("SELECT COUNT(*) FROM project_api_grants", [], |row| {
                        row.get(0)
                    })?;
                anyhow::ensure!(count < 8192, "project_api_grant_capacity");
            }
            tx.execute("INSERT INTO project_api_grants(key_id,workspace_id,environment,principal_id,session_id,generation,actor_kind,operation,recipe_id,access_revision,expires_at) VALUES(?1,?2,?3,?4,?5,?6,'local_session','environment','',?7,?8) ON CONFLICT DO UPDATE SET access_revision=excluded.access_revision,expires_at=excluded.expires_at",rusqlite::params![access.key_id,scope.workspace_id,scope.environment.as_str(),scope.principal_id,scope.session_id,scope.generation,access.revision as i64,expires])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn project_api_granted_key_ids(
        &self,
        scope: &ProjectApiSessionScope,
        now: i64,
    ) -> anyhow::Result<Vec<String>> {
        scope.validate().map_err(anyhow::Error::msg)?;
        anyhow::ensure!(now >= 0, "project_api_scope_invalid");
        let mut stmt=self.conn.prepare("SELECT DISTINCT grant_row.key_id FROM project_api_grants grant_row JOIN project_api_access access ON access.key_id=grant_row.key_id AND access.workspace_id=grant_row.workspace_id AND access.environment=grant_row.environment AND access.revision=grant_row.access_revision JOIN project_keys key ON key.id=access.key_id AND key.workspace_id=access.workspace_id AND key.environment=access.environment WHERE grant_row.workspace_id=?1 AND grant_row.environment=?2 AND grant_row.principal_id=?3 AND grant_row.session_id=?4 AND grant_row.generation=?5 AND grant_row.actor_kind=?6 AND grant_row.expires_at>?7 AND json_extract(access.metadata_json,'$.agent_enabled')=1 AND json_extract(access.metadata_json,'$.execution_name')=key.name AND ((grant_row.operation='environment' AND grant_row.actor_kind='local_session' AND grant_row.recipe_id='') OR (grant_row.operation='recipe' AND EXISTS(SELECT 1 FROM project_key_build_recipes recipe WHERE recipe.id=grant_row.recipe_id AND recipe.workspace_id=grant_row.workspace_id AND recipe.environment=grant_row.environment))) ORDER BY grant_row.key_id LIMIT 257")?;
        let rows = stmt
            .query_map(
                rusqlite::params![
                    scope.workspace_id,
                    scope.environment.as_str(),
                    scope.principal_id,
                    scope.session_id,
                    scope.generation,
                    actor_name(scope.actor_kind),
                    now
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        anyhow::ensure!(
            rows.len() <= PROJECT_KEYS_MAX && rows.iter().all(|id| valid_id(id)),
            "project_api_capacity"
        );
        Ok(rows)
    }
    pub fn project_api_access_list(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
    ) -> anyhow::Result<Vec<ProjectApiAccess>> {
        anyhow::ensure!(valid_id(workspace), "project_api_scope_invalid");
        let mut stmt=self.conn.prepare("SELECT metadata_json FROM project_api_access WHERE workspace_id=?1 AND environment=?2 ORDER BY key_id LIMIT 257")?;
        let json = stmt
            .query_map(rusqlite::params![workspace, environment.as_str()], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        anyhow::ensure!(json.len() <= PROJECT_KEYS_MAX, "project_api_capacity");
        json.into_iter()
            .map(|json| {
                let access: ProjectApiAccess = serde_json::from_str(&json)?;
                access.validate().map_err(anyhow::Error::msg)?;
                anyhow::ensure!(
                    access.workspace_id == workspace && access.environment == environment,
                    "project_api_scope_invalid"
                );
                Ok(access)
            })
            .collect()
    }
    pub fn project_api_access_save(
        &self,
        access: &ProjectApiAccess,
        expected: Option<u64>,
    ) -> anyhow::Result<ProjectKeyCas<ProjectApiAccess>> {
        access.validate().map_err(anyhow::Error::msg)?;
        let tx = self.conn.unchecked_transaction()?;
        let key = self
            .project_key_get(&access.workspace_id, access.environment, &access.key_id)?
            .context("project_api_key_missing")?;
        anyhow::ensure!(
            access
                .execution_name
                .as_ref()
                .is_none_or(|name| name == &key.name)
                && (access.execution_name.is_some()
                    || (key.usage == KeyUsage::StoreOnly && !access.agent_enabled))
                && !(access.agent_enabled && key.source == KeySource::GitHubOnly),
            "project_api_binding_invalid"
        );
        let current: Option<i64> = tx
            .query_row(
                "SELECT revision FROM project_api_access WHERE key_id=?1",
                [&access.key_id],
                |row| row.get(0),
            )
            .optional()?;
        let current = current
            .map(|revision| {
                u64::try_from(revision)
                    .ok()
                    .filter(|revision| valid_revision(*revision))
                    .context("project_api_revision_invalid")
            })
            .transpose()?;
        if current != expected {
            return Ok(ProjectKeyCas::Stale {
                current_revision: current,
            });
        }
        let mut saved = access.clone();
        saved.revision = current
            .unwrap_or(0)
            .checked_add(1)
            .filter(|revision| valid_revision(*revision))
            .context("project_api_revision_exhausted")?;
        let json = serde_json::to_string(&saved)?;
        anyhow::ensure!(json.len() <= 8192, "project_api_metadata_invalid");
        tx.execute("INSERT INTO project_api_access(key_id,workspace_id,environment,revision,metadata_json) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(key_id) DO UPDATE SET revision=excluded.revision,metadata_json=excluded.metadata_json",rusqlite::params![saved.key_id,saved.workspace_id,saved.environment.as_str(),saved.revision as i64,json])?;
        tx.execute(
            "DELETE FROM project_api_grants WHERE key_id=?1",
            [&saved.key_id],
        )?;
        tx.commit()?;
        Ok(ProjectKeyCas::Committed(saved))
    }
    pub fn project_api_grant_save(&self, grant: &ProjectApiGrant, now: i64) -> anyhow::Result<()> {
        grant.validate().map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            now >= 0
                && grant.expires_at > now
                && grant.expires_at.saturating_sub(now) <= 24 * 60 * 60,
            "project_api_expiry_invalid"
        );
        let tx = self.conn.unchecked_transaction()?;
        let access = self
            .project_api_access_list(&grant.scope.workspace_id, grant.scope.environment)?
            .into_iter()
            .find(|access| access.key_id == grant.key_id)
            .context("project_api_key_missing")?;
        anyhow::ensure!(
            access.agent_enabled && access.revision == grant.access_revision,
            "project_api_permission_denied"
        );
        self.project_key_get(
            &grant.scope.workspace_id,
            grant.scope.environment,
            &grant.key_id,
        )?
        .context("project_api_key_missing")?;
        if let Some(recipe_id) = &grant.recipe_id {
            anyhow::ensure!(
                self.project_key_recipe_list(&grant.scope.workspace_id, grant.scope.environment)?
                    .iter()
                    .any(|recipe| &recipe.id == recipe_id),
                "project_api_recipe_missing"
            );
        }
        tx.execute("DELETE FROM project_api_grants WHERE expires_at<=?1", [now])?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM project_api_grants", [], |row| {
            row.get(0)
        })?;
        anyhow::ensure!(count < 8192, "project_api_grant_capacity");
        tx.execute("INSERT INTO project_api_grants(key_id,workspace_id,environment,principal_id,session_id,generation,actor_kind,operation,recipe_id,access_revision,expires_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) ON CONFLICT DO UPDATE SET access_revision=excluded.access_revision,expires_at=excluded.expires_at",rusqlite::params![grant.key_id,grant.scope.workspace_id,grant.scope.environment.as_str(),grant.scope.principal_id,grant.scope.session_id,grant.scope.generation,actor_name(grant.scope.actor_kind),operation_name(grant.operation),grant.recipe_id.as_deref().unwrap_or(""),grant.access_revision as i64,grant.expires_at])?;
        tx.commit()?;
        Ok(())
    }
    pub fn project_api_grant_allows(
        &self,
        scope: &ProjectApiSessionScope,
        key_id: &str,
        operation: ProjectApiOperation,
        recipe_id: Option<&str>,
        now: i64,
    ) -> anyhow::Result<bool> {
        scope.validate().map_err(anyhow::Error::msg)?;
        if !valid_id(key_id)
            || now < 0
            || (scope.actor_kind == ProjectApiActorKind::CloudConnection
                && operation != ProjectApiOperation::Recipe)
            || (operation == ProjectApiOperation::Recipe) != recipe_id.is_some()
        {
            return Ok(false);
        }
        self.conn.query_row("SELECT EXISTS(SELECT 1 FROM project_api_grants grant_row JOIN project_api_access access ON access.key_id=grant_row.key_id AND access.workspace_id=grant_row.workspace_id AND access.environment=grant_row.environment AND access.revision=grant_row.access_revision JOIN project_keys key ON key.id=access.key_id AND key.workspace_id=access.workspace_id AND key.environment=access.environment WHERE grant_row.key_id=?1 AND grant_row.workspace_id=?2 AND grant_row.environment=?3 AND grant_row.principal_id=?4 AND grant_row.session_id=?5 AND grant_row.generation=?6 AND grant_row.actor_kind=?7 AND grant_row.operation=?8 AND grant_row.recipe_id=?9 AND grant_row.expires_at>?10 AND json_extract(access.metadata_json,'$.agent_enabled')=1 AND json_extract(access.metadata_json,'$.execution_name')=key.name AND (grant_row.operation='environment' OR EXISTS(SELECT 1 FROM project_key_build_recipes recipe WHERE recipe.id=grant_row.recipe_id AND recipe.workspace_id=grant_row.workspace_id AND recipe.environment=grant_row.environment)))",rusqlite::params![key_id,scope.workspace_id,scope.environment.as_str(),scope.principal_id,scope.session_id,scope.generation,actor_name(scope.actor_kind),operation_name(operation),recipe_id.unwrap_or(""),now],|row|row.get(0)).map_err(Into::into)
    }
    pub fn project_api_grant_revoke_scope(
        &self,
        scope: &ProjectApiSessionScope,
    ) -> anyhow::Result<()> {
        scope.validate().map_err(anyhow::Error::msg)?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM project_api_grants WHERE workspace_id=?1 AND environment=?2 AND principal_id=?3 AND session_id=?4 AND generation=?5 AND actor_kind=?6",rusqlite::params![scope.workspace_id,scope.environment.as_str(),scope.principal_id,scope.session_id,scope.generation,actor_name(scope.actor_kind)])?;
        if scope.actor_kind == ProjectApiActorKind::LocalSession {
            self.project_api_capability_revoke_scope(scope)?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Db, ProjectKey, ProjectApiAccess, ProjectApiSessionScope) {
        let db = Db::open_in_memory().unwrap();
        let workspace = db.create_workspace("API fixture").unwrap();
        let key = ProjectKey {
            id: "key".into(),
            workspace_id: workspace.clone(),
            environment: KeyEnvironment::Development,
            name: "WEATHER_TOKEN".into(),
            credential_id: None,
            source_baseline_credential_id: None,
            source: KeySource::Vault,
            kind: KeyKind::ApiToken,
            usage: KeyUsage::StoreOnly,
            delivery: KeyDelivery::Environment,
            source_priority: 1000,
            required: false,
            revision: 1,
            last_good_revision: None,
            last_good_at: None,
            storage_state: KeyStorageState::NotStored,
            created_at: 1,
            updated_at: 1,
            source_checked_at: None,
            error_code: None,
            github: None,
            legacy_reference: None,
        };
        db.project_key_save(&workspace, KeyEnvironment::Development, &key, None)
            .unwrap();
        let access = ProjectApiAccess {
            key_id: key.id.clone(),
            workspace_id: workspace.clone(),
            environment: key.environment,
            revision: 1,
            execution_name: Some("WEATHER_TOKEN".into()),
            service_name: Some("Weather service".into()),
            description: None,
            documentation_url: None,
            agent_enabled: true,
        };
        let scope = ProjectApiSessionScope {
            workspace_id: workspace,
            environment: key.environment,
            principal_id: "local-agent".into(),
            session_id: "pane".into(),
            generation: "runtime-1-execution-1".into(),
            actor_kind: ProjectApiActorKind::LocalSession,
        };
        (db, key, access, scope)
    }

    #[test]
    fn project_api_metadata_is_scoped_cas_and_does_not_promote_store_only() {
        let (db, key, access, _) = fixture();
        let saved = match db.project_api_access_save(&access, None).unwrap() {
            ProjectKeyCas::Committed(saved) => saved,
            _ => panic!("initial save"),
        };
        assert_eq!(saved.revision, 1);
        assert_eq!(
            db.project_key_get(&key.workspace_id, key.environment, &key.id)
                .unwrap()
                .unwrap()
                .usage,
            KeyUsage::StoreOnly
        );
        assert!(
            db.project_api_access_list(&key.workspace_id, KeyEnvironment::Production)
                .unwrap()
                .is_empty()
        );
        let other = db.create_workspace("other").unwrap();
        assert!(
            db.project_api_access_list(&other, key.environment)
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            db.project_api_access_save(&access, None).unwrap(),
            ProjectKeyCas::Stale { .. }
        ));
        let json = serde_json::to_string(&saved).unwrap();
        for forbidden in ["credential_id", "digest", "access_token", "source_path"] {
            assert!(!json.contains(forbidden));
        }
    }

    #[test]
    fn project_api_grants_fence_actor_environment_generation_policy_and_revocation() {
        let (db, key, access, scope) = fixture();
        db.project_api_access_save(&access, None).unwrap();
        let grant = ProjectApiGrant {
            scope: scope.clone(),
            key_id: key.id.clone(),
            access_revision: 1,
            operation: ProjectApiOperation::Environment,
            recipe_id: None,
            expires_at: 100,
        };
        assert!(
            !db.project_api_grant_allows(&scope, &key.id, grant.operation, None, 1)
                .unwrap()
        );
        db.project_api_grant_save(&grant, 1).unwrap();
        assert!(
            db.project_api_grant_allows(&scope, &key.id, grant.operation, None, 2)
                .unwrap()
        );
        for altered in 0..4 {
            let mut wrong = scope.clone();
            match altered {
                0 => wrong.principal_id.push('x'),
                1 => wrong.session_id.push('x'),
                2 => wrong.generation.push('x'),
                _ => wrong.environment = KeyEnvironment::Production,
            };
            assert!(
                !db.project_api_grant_allows(&wrong, &key.id, grant.operation, None, 2)
                    .unwrap()
            );
        }
        assert!(
            !db.project_api_grant_allows(&scope, &key.id, grant.operation, None, 100)
                .unwrap()
        );
        let mut disabled = access.clone();
        disabled.agent_enabled = false;
        db.project_api_access_save(&disabled, Some(1)).unwrap();
        assert!(
            !db.project_api_grant_allows(&scope, &key.id, grant.operation, None, 2)
                .unwrap()
        );
        db.project_api_grant_revoke_scope(&scope).unwrap();
        assert!(
            !db.project_api_grant_allows(&scope, &key.id, grant.operation, None, 2)
                .unwrap()
        );
    }

    #[test]
    fn project_api_grant_projection_is_bounded_exact_scope_without_nested_metadata_decodes() {
        let (db, key, access, scope) = fixture();
        db.project_api_access_save(&access, None).unwrap();
        db.project_api_grant_save(
            &ProjectApiGrant {
                scope: scope.clone(),
                key_id: key.id.clone(),
                access_revision: 1,
                operation: ProjectApiOperation::Environment,
                recipe_id: None,
                expires_at: 100,
            },
            1,
        )
        .unwrap();
        assert_eq!(db.project_api_granted_key_ids(&scope, 2).unwrap(), [key.id]);
        let mut wrong = scope.clone();
        wrong.generation.push('x');
        assert!(
            db.project_api_granted_key_ids(&wrong, 2)
                .unwrap()
                .is_empty()
        );
        assert!(
            db.project_api_granted_key_ids(&scope, 100)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn project_api_cloud_recipe_grant_is_revoked_by_recipe_update_and_recreation() {
        let (db, key, access, mut scope) = fixture();
        scope.actor_kind = ProjectApiActorKind::CloudConnection;
        db.project_api_access_save(&access, None).unwrap();
        let recipe = ProjectBuildRecipe {
            id: "recipe".into(),
            workspace_id: scope.workspace_id.clone(),
            environment: scope.environment,
            revision: 1,
            name: "Fixture recipe".into(),
            command: vec!["fixture-first".into()],
            cwd: ".".into(),
        };
        db.project_key_recipe_save(&scope.workspace_id, scope.environment, &recipe, None)
            .unwrap();
        let grant = ProjectApiGrant {
            scope: scope.clone(),
            key_id: key.id.clone(),
            access_revision: 1,
            operation: ProjectApiOperation::Recipe,
            recipe_id: Some(recipe.id.clone()),
            expires_at: 100,
        };
        db.project_api_grant_save(&grant, 1).unwrap();
        assert!(
            db.project_api_grant_allows(&scope, &key.id, grant.operation, Some(&recipe.id), 2)
                .unwrap()
        );
        let mut changed = recipe.clone();
        changed.command = vec!["fixture-other".into()];
        db.project_key_recipe_save(&scope.workspace_id, scope.environment, &changed, Some(1))
            .unwrap();
        assert!(
            !db.project_api_grant_allows(&scope, &key.id, grant.operation, Some(&recipe.id), 2)
                .unwrap()
        );
        db.project_api_grant_save(&grant, 2).unwrap();
        db.project_key_recipe_delete(&scope.workspace_id, scope.environment, &recipe.id, 2)
            .unwrap();
        db.project_key_recipe_save(&scope.workspace_id, scope.environment, &recipe, None)
            .unwrap();
        assert!(
            !db.project_api_grant_allows(&scope, &key.id, grant.operation, Some(&recipe.id), 3)
                .unwrap()
        );
    }
    #[test]
    fn project_api_live_capability_discovers_new_explicit_policy_but_revoke_prevents_refresh() {
        let (db, key, access, scope) = fixture();
        let hash = [7; 32];
        db.project_api_capability_save(&scope, &hash, 100, 1)
            .unwrap();
        db.project_api_local_grants_refresh(&scope, &hash, 2)
            .unwrap();
        assert!(
            db.project_api_granted_key_ids(&scope, 2)
                .unwrap()
                .is_empty()
        );
        db.project_api_access_save(&access, None).unwrap();
        db.project_api_local_grants_refresh(&scope, &hash, 3)
            .unwrap();
        assert!(
            db.project_api_grant_allows(&scope, &key.id, ProjectApiOperation::Environment, None, 3)
                .unwrap()
        );
        db.project_api_grant_revoke_scope(&scope).unwrap();
        assert!(!db.project_api_capability_allows(&scope, &hash, 4).unwrap());
        assert!(
            db.project_api_local_grants_refresh(&scope, &hash, 4)
                .is_err()
        );
        assert!(
            !db.project_api_grant_allows(
                &scope,
                &key.id,
                ProjectApiOperation::Environment,
                None,
                4
            )
            .unwrap()
        );
    }
}
