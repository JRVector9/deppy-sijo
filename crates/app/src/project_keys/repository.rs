//! Storage-neutral metadata and authorization port for project API adapters.
//!
//! The composition root supplies the database adapter. These operations expose only core
//! metadata and capability digests; secret resolution remains in ProjectKeysService.

use deppy_core::{
    project_api_access::{ProjectApiAccess, ProjectApiSessionScope},
    project_keys::{KeyEnvironment, ProjectKey, ProjectKeySettings},
};

pub(crate) trait ProjectApiRepository {
    fn workspace_is_local(&self, workspace: &str) -> anyhow::Result<bool>;

    fn project_key_settings(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
    ) -> anyhow::Result<ProjectKeySettings>;

    fn project_key_list(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
    ) -> anyhow::Result<Vec<ProjectKey>>;

    fn project_api_access_list(
        &self,
        workspace: &str,
        environment: KeyEnvironment,
    ) -> anyhow::Result<Vec<ProjectApiAccess>>;

    fn project_api_granted_key_ids(
        &self,
        scope: &ProjectApiSessionScope,
        now: i64,
    ) -> anyhow::Result<Vec<String>>;

    fn project_api_capability_save(
        &self,
        scope: &ProjectApiSessionScope,
        bearer_hash: &[u8; 32],
        expires_at: i64,
        now: i64,
    ) -> anyhow::Result<()>;

    fn project_api_local_grants_refresh(
        &self,
        scope: &ProjectApiSessionScope,
        bearer_hash: &[u8; 32],
        now: i64,
    ) -> anyhow::Result<()>;

    fn project_api_grant_revoke_scope(&self, scope: &ProjectApiSessionScope) -> anyhow::Result<()>;

    #[cfg(test)]
    fn project_api_capability_allows(
        &self,
        scope: &ProjectApiSessionScope,
        bearer_hash: &[u8; 32],
        now: i64,
    ) -> anyhow::Result<bool>;
}
