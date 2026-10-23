//! Value-free API projection for CLI and scoped agent tools.
use super::{
    contracts::*,
    repository::ProjectApiRepository,
    service::{ProjectKeysService, storage_error},
};
use deppy_core::{project_api_access::*, project_keys::*};

impl ProjectKeysService {
    pub fn save_api_key(
        &self,
        scope: &ProjectKeysScope,
        mut key: ProjectKey,
        mut expected: Option<u64>,
        value: ValueEdit,
        access: ProjectApiAccess,
        expected_access: Option<u64>,
    ) -> Result<ProjectKey, ProjectKeysError> {
        access
            .validate()
            .map_err(|_| ProjectKeysError::InvalidInput)?;
        if access.workspace_id != scope.workspace_id
            || access.environment != scope.environment
            || access.key_id != key.id
            || key.workspace_id != scope.workspace_id
            || key.environment != scope.environment
            || access
                .execution_name
                .as_ref()
                .is_some_and(|name| name != &key.name)
            || (access.execution_name.is_none()
                && (key.usage != KeyUsage::StoreOnly || access.agent_enabled))
            || (access.agent_enabled && key.source == KeySource::GitHubOnly)
        {
            return Err(ProjectKeysError::InvalidInput);
        }
        let current_key = storage_error(self.db.project_key_get(
            &scope.workspace_id,
            scope.environment,
            &key.id,
        ))?;
        if current_key.as_ref().map(|key| key.revision) != expected {
            return Err(ProjectKeysError::Stale);
        }
        let current_access = storage_error(
            self.db
                .project_api_access_list(&scope.workspace_id, scope.environment),
        )?
        .into_iter()
        .find(|existing| existing.key_id == key.id);
        if current_access.as_ref().map(|access| access.revision) != expected_access {
            return Err(ProjectKeysError::Stale);
        }
        if access.execution_name.is_none() {
            key.name = if let Some(current) = &current_key {
                current.name.clone()
            } else {
                let id =
                    uuid::Uuid::parse_str(&key.id).map_err(|_| ProjectKeysError::InvalidInput)?;
                format!("DEPPY_API_{}", id.simple().to_string().to_ascii_uppercase())
            };
        }
        key.validate().map_err(|_| ProjectKeysError::InvalidInput)?;
        let mut denied = access.clone();
        denied.agent_enabled = false;
        let staged_access_revision = if current_key.is_none() {
            if key.source == KeySource::Vault && key.source_priority == 0 {
                key.source_priority = 1000;
            }
            key = super::service::committed(storage_error(
                self.db.project_api_prepare_new_key(&key, &denied),
            )?)?;
            expected = Some(key.revision);
            1
        } else {
            if current_key
                .as_ref()
                .is_some_and(|current| current.name != key.name)
            {
                let bound = super::service::committed(storage_error(
                    self.db.project_api_bind_execution_name(
                        &access,
                        expected.ok_or(ProjectKeysError::Stale)?,
                        expected_access.ok_or(ProjectKeysError::InvalidInput)?,
                        super::service::now(),
                    ),
                )?)?;
                expected = Some(bound.revision);
            }
            super::service::committed(storage_error(
                self.db.project_api_access_save(&denied, expected_access),
            )?)?
            .revision
        };
        // The deny metadata and label exist before any protected value publication.
        let saved = self.save_key(scope, key, expected, value)?;
        if access.agent_enabled {
            super::service::committed(storage_error(
                self.db
                    .project_api_access_save(&access, Some(staged_access_revision)),
            )?)?;
        }
        Ok(saved)
    }
    #[cfg(test)]
    fn project_api_catalog(
        &self,
        scope: &ProjectKeysScope,
        actor: Option<&ProjectApiSessionScope>,
        at: i64,
    ) -> Result<ProjectApiCatalog, ProjectKeysError> {
        project_api_catalog_from_db(&self.db, scope, actor, at)
    }
}

pub fn project_api_catalog_from_db(
    db: &dyn ProjectApiRepository,
    scope: &ProjectKeysScope,
    actor: Option<&ProjectApiSessionScope>,
    at: i64,
) -> Result<ProjectApiCatalog, ProjectKeysError> {
    if !valid_id(&scope.workspace_id)
        || at < 0
        || actor.is_some_and(|actor| {
            actor.validate().is_err()
                || actor.workspace_id != scope.workspace_id
                || actor.environment != scope.environment
        })
    {
        return Err(ProjectKeysError::InvalidInput);
    }
    let local = storage_error(db.workspace_is_local(&scope.workspace_id))?;
    let settings = storage_error(db.project_key_settings(&scope.workspace_id, scope.environment))?;
    let keys = storage_error(db.project_key_list(&scope.workspace_id, scope.environment))?;
    let granted = actor
        .map(|actor| storage_error(db.project_api_granted_key_ids(actor, at)))
        .transpose()?;
    let mut entries = Vec::new();
    for access in storage_error(db.project_api_access_list(&scope.workspace_id, scope.environment))?
    {
        let Some(key) = keys.iter().find(|key| key.id == access.key_id) else {
            continue;
        };
        if granted.as_ref().is_some_and(|ids| !ids.contains(&key.id)) {
            continue;
        }
        let unavailable_reason = if !local {
            Some("remote_secret_adapter_unavailable")
        } else if actor
            .is_some_and(|actor| actor.actor_kind == ProjectApiActorKind::CloudConnection)
        {
            Some("cloud_secret_adapter_unavailable")
        } else if !access.agent_enabled {
            Some("agent_access_disabled")
        } else if access.execution_name.as_ref() != Some(&key.name) {
            Some("execution_binding_missing")
        } else if key.source == KeySource::GitHubOnly {
            Some("github_only")
        } else if key.source.is_file() && !settings.sources.contains(&key.source) {
            Some("source_disconnected")
        } else if key.storage_state != KeyStorageState::Stored || key.last_good_revision.is_none() {
            Some("key_unavailable")
        } else {
            None
        };
        entries.push(ProjectApiCatalogEntry {
            key_id: key.id.clone(),
            name: access
                .service_name
                .clone()
                .or_else(|| access.execution_name.clone())
                .unwrap_or_else(|| "Stored API".into()),
            execution_name: access.execution_name,
            kind: key.kind,
            usage: key.usage,
            delivery: key.delivery,
            revision: key.revision,
            state: key.storage_state,
            service_name: access.service_name,
            description: access.description,
            documentation_url: access.documentation_url,
            agent_enabled: access.agent_enabled,
            available: unavailable_reason.is_none(),
            unavailable_reason: unavailable_reason.map(str::to_owned),
            use_method: unavailable_reason
                .is_none()
                .then(|| "deppy-sijo env run --key".into()),
        });
    }
    Ok(ProjectApiCatalog {
        workspace_id: scope.workspace_id.clone(),
        environment: scope.environment,
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_keys::tests::Fixture;
    use std::sync::atomic::Ordering;

    #[test]
    fn project_api_save_keeps_unbound_store_only_private_and_enables_only_explicit_binding() {
        let fixture = Fixture::new();
        let mut key = fixture.key("", KeySource::Vault);
        key.usage = KeyUsage::StoreOnly;
        key.required = false;
        let mut access = ProjectApiAccess {
            key_id: key.id.clone(),
            workspace_id: key.workspace_id.clone(),
            environment: key.environment,
            revision: 1,
            execution_name: None,
            service_name: Some("Stored service".into()),
            description: None,
            documentation_url: None,
            agent_enabled: false,
        };
        let saved = fixture
            .service
            .save_api_key(
                &fixture.scope,
                key,
                None,
                ValueEdit::Replace(SensitiveDraft::new("fixture-private".into()).unwrap()),
                access.clone(),
                None,
            )
            .unwrap();
        assert!(saved.name.starts_with("DEPPY_API_"));
        assert_eq!(saved.usage, KeyUsage::StoreOnly);
        let catalog = fixture
            .service
            .project_api_catalog(&fixture.scope, None, 1)
            .unwrap();
        assert!(catalog.entries[0].execution_name.is_none());
        assert!(!catalog.entries[0].available);
        let mut bound = saved.clone();
        bound.name = "SERVICE_TOKEN".into();
        access.execution_name = Some(bound.name.clone());
        access.agent_enabled = true;
        let bound = fixture
            .service
            .save_api_key(
                &fixture.scope,
                bound,
                Some(saved.revision),
                ValueEdit::Keep,
                access,
                Some(1),
            )
            .unwrap();
        assert_eq!(bound.usage, KeyUsage::StoreOnly);
        assert!(
            fixture
                .service
                .project_api_catalog(&fixture.scope, None, 1)
                .unwrap()
                .entries[0]
                .available
        );
        assert_eq!(
            fixture
                .service
                .snapshot(&fixture.scope, 1)
                .unwrap()
                .api_access
                .len(),
            1
        );
    }

    #[test]
    fn project_api_save_rejects_stale_policy_before_writing_secret() {
        let fixture = Fixture::new();
        let key = fixture.save(fixture.key("TOKEN", KeySource::Vault), "old-fixture");
        let access = ProjectApiAccess {
            key_id: key.id.clone(),
            workspace_id: key.workspace_id.clone(),
            environment: key.environment,
            revision: 1,
            execution_name: Some(key.name.clone()),
            service_name: None,
            description: None,
            documentation_url: None,
            agent_enabled: true,
        };
        fixture
            .service
            .db
            .project_api_access_save(&access, None)
            .unwrap();
        let calls = fixture.store.2.load(Ordering::Acquire);
        assert_eq!(
            fixture.service.save_api_key(
                &fixture.scope,
                key.clone(),
                Some(key.revision),
                ValueEdit::Replace(SensitiveDraft::new("new-fixture".into()).unwrap()),
                access,
                None
            ),
            Err(ProjectKeysError::Stale)
        );
        assert_eq!(fixture.store.2.load(Ordering::Acquire), calls);
        assert_eq!(
            fixture
                .service
                .key(&fixture.scope, &key.id, key.revision)
                .unwrap(),
            key
        );
    }

    #[test]
    fn project_api_failed_value_save_preserves_disabled_label_before_secret_publication() {
        let fixture = Fixture::new();
        let mut key = fixture.key("SERVICE_TOKEN", KeySource::Vault);
        key.usage = KeyUsage::StoreOnly;
        key.required = false;
        let access = ProjectApiAccess {
            key_id: key.id.clone(),
            workspace_id: key.workspace_id.clone(),
            environment: key.environment,
            revision: 1,
            execution_name: Some(key.name.clone()),
            service_name: Some("Retained service label".into()),
            description: None,
            documentation_url: None,
            agent_enabled: true,
        };
        fixture.store.1.store(true, Ordering::Release);
        assert!(
            fixture
                .service
                .save_api_key(
                    &fixture.scope,
                    key,
                    None,
                    ValueEdit::Replace(SensitiveDraft::new("unpublished-fixture".into()).unwrap()),
                    access,
                    None
                )
                .is_err()
        );
        let metadata = fixture
            .service
            .db
            .project_api_access_list(&fixture.scope.workspace_id, fixture.scope.environment)
            .unwrap();
        assert_eq!(metadata.len(), 1);
        assert_eq!(
            metadata[0].service_name.as_deref(),
            Some("Retained service label")
        );
        assert!(!metadata[0].agent_enabled);
        let catalog = fixture
            .service
            .project_api_catalog(&fixture.scope, None, 1)
            .unwrap();
        assert!(!catalog.entries[0].available);
    }

    #[test]
    fn project_api_catalog_is_metadata_only_and_preserves_unavailable_states() {
        let fixture = Fixture::new();
        let mut key = fixture.key("SERVICE_TOKEN", KeySource::Vault);
        key.usage = KeyUsage::StoreOnly;
        key.required = false;
        let key = fixture.save(key, "synthetic-secret-never-in-catalog");
        fixture
            .service
            .db
            .project_api_access_save(
                &ProjectApiAccess {
                    key_id: key.id.clone(),
                    workspace_id: fixture.scope.workspace_id.clone(),
                    environment: fixture.scope.environment,
                    revision: 1,
                    execution_name: Some(key.name.clone()),
                    service_name: Some("Weather service".into()),
                    description: None,
                    documentation_url: None,
                    agent_enabled: true,
                },
                None,
            )
            .unwrap();
        let calls = fixture.store.2.load(Ordering::Acquire);
        let catalog = fixture
            .service
            .project_api_catalog(&fixture.scope, None, 1)
            .unwrap();
        assert_eq!(
            fixture.store.2.load(Ordering::Acquire),
            calls,
            "metadata may not read protected values"
        );
        assert_eq!(catalog.entries.len(), 1);
        assert!(catalog.entries[0].available);
        assert_eq!(catalog.entries[0].usage, KeyUsage::StoreOnly);
        let json = serde_json::to_string(&catalog).unwrap();
        for forbidden in [
            "synthetic-secret-never-in-catalog",
            "credential_id",
            "source_baseline",
            "source_path",
            "digest",
        ] {
            assert!(!json.contains(forbidden));
        }
        let other = ProjectKeysScope {
            workspace_id: fixture.scope.workspace_id.clone(),
            environment: KeyEnvironment::Production,
        };
        assert!(
            fixture
                .service
                .project_api_catalog(&other, None, 1)
                .unwrap()
                .entries
                .is_empty()
        );
        let mut locked = key.clone();
        locked.storage_state = KeyStorageState::Locked;
        fixture
            .service
            .db
            .project_key_save(
                &key.workspace_id,
                key.environment,
                &locked,
                Some(key.revision),
            )
            .unwrap();
        let locked = fixture
            .service
            .project_api_catalog(&fixture.scope, None, 1)
            .unwrap();
        assert!(!locked.entries[0].available);
        assert_eq!(locked.entries[0].state, KeyStorageState::Locked);
    }

    #[test]
    fn project_api_catalog_never_advertises_internal_binding_or_ssh_local_secrets() {
        let fixture = Fixture::new();
        let mut key = fixture.key("DEPPY_API_INTERNAL", KeySource::Vault);
        key.usage = KeyUsage::StoreOnly;
        key.required = false;
        let key = fixture.save(key, "synthetic-vault-value");
        fixture
            .service
            .db
            .project_api_access_save(
                &ProjectApiAccess {
                    key_id: key.id,
                    workspace_id: fixture.scope.workspace_id.clone(),
                    environment: fixture.scope.environment,
                    revision: 1,
                    execution_name: None,
                    service_name: Some("Stored API".into()),
                    description: None,
                    documentation_url: None,
                    agent_enabled: false,
                },
                None,
            )
            .unwrap();
        let catalog = fixture
            .service
            .project_api_catalog(&fixture.scope, None, 1)
            .unwrap();
        assert_eq!(catalog.entries[0].name, "Stored API");
        assert!(catalog.entries[0].execution_name.is_none());
        assert!(!catalog.entries[0].available);
        assert!(
            !serde_json::to_string(&catalog)
                .unwrap()
                .contains("DEPPY_API_INTERNAL")
        );
    }

    #[test]
    fn project_api_cloud_catalog_marks_unimplemented_execution_unavailable() {
        let fixture = Fixture::new();
        let key = fixture.save(fixture.key("TOKEN", KeySource::Vault), "fixture-value");
        fixture
            .service
            .db
            .project_api_access_save(
                &ProjectApiAccess {
                    key_id: key.id.clone(),
                    workspace_id: key.workspace_id.clone(),
                    environment: key.environment,
                    revision: 1,
                    execution_name: Some(key.name.clone()),
                    service_name: None,
                    description: None,
                    documentation_url: None,
                    agent_enabled: true,
                },
                None,
            )
            .unwrap();
        let recipe = ProjectBuildRecipe {
            id: "recipe".into(),
            workspace_id: key.workspace_id.clone(),
            environment: key.environment,
            revision: 1,
            name: "Fixture recipe".into(),
            command: vec!["fixture".into()],
            cwd: ".".into(),
        };
        fixture
            .service
            .db
            .project_key_recipe_save(&key.workspace_id, key.environment, &recipe, None)
            .unwrap();
        let actor = ProjectApiSessionScope {
            workspace_id: key.workspace_id,
            environment: key.environment,
            principal_id: "cloud".into(),
            session_id: "session".into(),
            generation: "generation".into(),
            actor_kind: ProjectApiActorKind::CloudConnection,
        };
        fixture
            .service
            .db
            .project_api_grant_save(
                &ProjectApiGrant {
                    scope: actor.clone(),
                    key_id: key.id,
                    access_revision: 1,
                    operation: ProjectApiOperation::Recipe,
                    recipe_id: Some(recipe.id),
                    expires_at: 100,
                },
                1,
            )
            .unwrap();
        let catalog = fixture
            .service
            .project_api_catalog(&fixture.scope, Some(&actor), 2)
            .unwrap();
        assert_eq!(catalog.entries.len(), 1);
        assert!(!catalog.entries[0].available);
        assert_eq!(
            catalog.entries[0].unavailable_reason.as_deref(),
            Some("cloud_secret_adapter_unavailable")
        );
        assert!(catalog.entries[0].use_method.is_none());
    }
}
