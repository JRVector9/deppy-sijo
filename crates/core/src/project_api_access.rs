//! Value-free project API descriptions and explicit execution capabilities.
use crate::project_keys::{KeyDelivery, KeyEnvironment, KeyKind, KeyStorageState, KeyUsage};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectApiAccess {
    pub key_id: String,
    pub workspace_id: String,
    pub environment: KeyEnvironment,
    pub revision: u64,
    pub execution_name: Option<String>,
    pub service_name: Option<String>,
    pub description: Option<String>,
    pub documentation_url: Option<String>,
    pub agent_enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectApiActorKind {
    LocalSession,
    CloudConnection,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectApiSessionScope {
    pub workspace_id: String,
    pub environment: KeyEnvironment,
    pub principal_id: String,
    pub session_id: String,
    pub generation: String,
    pub actor_kind: ProjectApiActorKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectApiOperation {
    Environment,
    Recipe,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectApiGrant {
    pub scope: ProjectApiSessionScope,
    pub key_id: String,
    pub access_revision: u64,
    pub operation: ProjectApiOperation,
    pub recipe_id: Option<String>,
    pub expires_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectApiCatalogEntry {
    pub key_id: String,
    pub name: String,
    pub execution_name: Option<String>,
    pub kind: KeyKind,
    pub usage: KeyUsage,
    pub delivery: KeyDelivery,
    pub revision: u64,
    pub state: KeyStorageState,
    pub service_name: Option<String>,
    pub description: Option<String>,
    pub documentation_url: Option<String>,
    pub agent_enabled: bool,
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub use_method: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectApiCatalog {
    pub workspace_id: String,
    pub environment: KeyEnvironment,
    pub entries: Vec<ProjectApiCatalogEntry>,
}

impl ProjectApiAccess {
    pub fn validate(&self) -> Result<(), &'static str> {
        use crate::project_keys::{valid_id, valid_metadata_text, valid_revision};
        if !valid_id(&self.key_id)
            || !valid_id(&self.workspace_id)
            || !valid_revision(self.revision)
            || self
                .execution_name
                .as_ref()
                .is_some_and(|name| !crate::credential_env::valid_name(name))
            || (self.agent_enabled && self.execution_name.is_none())
            || self
                .service_name
                .as_ref()
                .is_some_and(|value| !valid_metadata_text(value, 256))
            || self
                .description
                .as_ref()
                .is_some_and(|value| !valid_metadata_text(value, 2048))
            || self.documentation_url.as_ref().is_some_and(|value| {
                !valid_metadata_text(value, 2048)
                    || value.chars().any(char::is_whitespace)
                    || value.strip_prefix("https://").is_none_or(|rest| {
                        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
                        authority.is_empty() || authority.contains('@') || authority.contains('\\')
                    })
            })
        {
            return Err("project_api_metadata_invalid");
        }
        Ok(())
    }
}
impl ProjectApiSessionScope {
    pub fn validate(&self) -> Result<(), &'static str> {
        if [
            &self.workspace_id,
            &self.principal_id,
            &self.session_id,
            &self.generation,
        ]
        .iter()
        .any(|id| !crate::project_keys::valid_id(id))
        {
            return Err("project_api_scope_invalid");
        }
        Ok(())
    }
}
impl ProjectApiGrant {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.scope.validate()?;
        if !crate::project_keys::valid_id(&self.key_id)
            || !crate::project_keys::valid_revision(self.access_revision)
            || self.expires_at < 0
            || self
                .recipe_id
                .as_ref()
                .is_some_and(|id| !crate::project_keys::valid_id(id))
            || (self.operation == ProjectApiOperation::Recipe) != self.recipe_id.is_some()
            || (self.scope.actor_kind == ProjectApiActorKind::CloudConnection
                && self.operation != ProjectApiOperation::Recipe)
        {
            return Err("project_api_grant_invalid");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_api_access_accepts_descriptions_without_secret_fields() {
        let access = ProjectApiAccess {
            key_id: "key".into(),
            workspace_id: "workspace".into(),
            environment: KeyEnvironment::Development,
            revision: 1,
            execution_name: Some("TOKEN".into()),
            service_name: Some("Weather service".into()),
            description: Some("Forecast lookup".into()),
            documentation_url: Some("https://example.test/docs".into()),
            agent_enabled: true,
        };
        assert!(access.validate().is_ok());
        let json = format!("{access:?}");
        for field in ["credential", "digest", "secret_value", "source_path"] {
            assert!(!json.contains(field));
        }
        let mut invalid = access;
        invalid.documentation_url = Some("file:///private/secret".into());
        assert!(invalid.validate().is_err());
        invalid.documentation_url = None;
        invalid.service_name = Some("x".repeat(257));
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn project_api_grant_requires_exact_actor_and_recipe_for_cloud() {
        let mut grant = ProjectApiGrant {
            scope: ProjectApiSessionScope {
                workspace_id: "workspace".into(),
                environment: KeyEnvironment::Development,
                principal_id: "principal".into(),
                session_id: "session".into(),
                generation: "generation".into(),
                actor_kind: ProjectApiActorKind::LocalSession,
            },
            key_id: "key".into(),
            access_revision: 1,
            operation: ProjectApiOperation::Environment,
            recipe_id: None,
            expires_at: 100,
        };
        assert!(grant.validate().is_ok());
        grant.scope.actor_kind = ProjectApiActorKind::CloudConnection;
        assert!(grant.validate().is_err());
        grant.operation = ProjectApiOperation::Recipe;
        grant.recipe_id = Some("recipe".into());
        assert!(grant.validate().is_ok());
        grant.scope.generation.clear();
        assert!(grant.validate().is_err());
    }
}
