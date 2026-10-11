//! Project/environment key metadata. Secret bytes belong only to the secret store and workers.
use serde::{Deserialize, Serialize};

pub const PROJECT_KEYS_MAX: usize = 256;
pub const PROJECT_KEY_METADATA_BYTES_MAX: usize = 16 * 1024;
pub const PROJECT_KEY_SOURCES_MAX: usize = 16;
pub const PROJECT_KEY_RECIPES_MAX: usize = 32;
pub const PROJECT_KEY_RECEIPTS_MAX: usize = 100;
pub const PROJECT_KEY_CHANGES_MAX: usize = 32;
pub const PROJECT_KEY_ID_BYTES_MAX: usize = 256;
pub const PROJECT_KEY_PATH_BYTES_MAX: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyEnvironment {
    Development,
    Production,
}
impl KeyEnvironment {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Development => "development",
            Self::Production => "production",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KeySource {
    Vault,
    Dotenv { file: String },
    PemFile { path: String },
    GitHubOnly,
}
impl KeySource {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Dotenv { file }
                if !crate::env_sources::valid_files(std::slice::from_ref(file)) =>
            {
                Err("project_key_dotenv_invalid")
            }
            Self::PemFile { path } if !valid_metadata_text(path, PROJECT_KEY_PATH_BYTES_MAX) => {
                Err("project_key_path_invalid")
            }
            _ => Ok(()),
        }
    }
    pub fn is_file(&self) -> bool {
        matches!(self, Self::Dotenv { .. } | Self::PemFile { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyKind {
    ApiToken,
    ClientSecret,
    PrivateKey,
    Configuration,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyUsage {
    Build,
    StoreOnly,
    LegacyAgent,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyDelivery {
    Environment,
    FilePath,
    TemporaryFile,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyStorageState {
    NotStored,
    Stored,
    UpdateNeeded,
    Locked,
    MissingSource,
    EmptySource,
    Conflict,
    Failed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitHubRemoteState {
    Unlinked,
    Pending,
    Sent,
    Failed,
    ExternalChange,
}

/// Stable remote identity and authentication reference. Neither credentials nor values are present.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GitHubScope {
    Repository {
        owner: String,
        repository: String,
    },
    Environment {
        owner: String,
        repository: String,
        environment: String,
    },
    Organization {
        organization: String,
        visibility: GitHubVisibility,
        selected_repository_ids: Vec<u64>,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitHubVisibility {
    All,
    Private,
    Selected,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubTarget {
    /// None explicitly selects official gh's authenticated account; Some selects a token reference.
    pub auth_credential_id: Option<String>,
    pub account_identity: String,
    pub scope: GitHubScope,
    pub secret_name: String,
    pub auto_send: bool,
}
impl GitHubTarget {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_metadata_text(&self.account_identity, 256)
            || !crate::credential_env::valid_name(&self.secret_name)
            || self
                .secret_name
                .get(..7)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("GITHUB_"))
            || self
                .auth_credential_id
                .as_ref()
                .is_some_and(|id| !valid_id(id))
        {
            return Err("project_key_github_identity_invalid");
        }
        let component = |s: &str| {
            !s.is_empty()
                && s.len() <= 256
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        };
        match &self.scope {
            GitHubScope::Repository { owner, repository }
                if component(owner) && component(repository) =>
            {
                Ok(())
            }
            GitHubScope::Environment {
                owner,
                repository,
                environment,
            } if component(owner)
                && component(repository)
                && valid_metadata_text(environment, 256) =>
            {
                Ok(())
            }
            GitHubScope::Organization {
                organization,
                visibility,
                selected_repository_ids,
            } if component(organization)
                && selected_repository_ids.len() <= 256
                && selected_repository_ids
                    .iter()
                    .all(|id| *id > 0 && *id <= i64::MAX as u64)
                && (if *visibility == GitHubVisibility::Selected {
                    !selected_repository_ids.is_empty()
                } else {
                    selected_repository_ids.is_empty()
                })
                && !selected_repository_ids
                    .iter()
                    .enumerate()
                    .any(|(i, id)| selected_repository_ids[..i].contains(id)) =>
            {
                Ok(())
            }
            _ => Err("project_key_github_scope_invalid"),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyGitHubStatus {
    pub target: GitHubTarget,
    pub state: GitHubRemoteState,
    pub last_sent_revision: Option<u64>,
    pub remote_updated_at: Option<String>,
    /// An allowlisted error code, never raw subprocess or network output.
    pub error_code: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectKey {
    pub id: String,
    pub workspace_id: String,
    pub environment: KeyEnvironment,
    pub name: String,
    pub credential_id: Option<String>,
    /// Protected source comparison baseline stored in this credential bundle, never a digest here.
    pub source_baseline_credential_id: Option<String>,
    pub source: KeySource,
    pub kind: KeyKind,
    pub usage: KeyUsage,
    pub delivery: KeyDelivery,
    /// Higher priority wins; legacy duplicates remain visible until explicitly resolved.
    pub source_priority: i32,
    pub required: bool,
    /// Local metadata/value generation. Storage assigns initial 1 and increments on CAS save.
    pub revision: u64,
    pub last_good_revision: Option<u64>,
    /// Time of actual protected value publication. Legacy values with unknown save time remain None.
    #[serde(default)]
    pub last_good_at: Option<i64>,
    pub storage_state: KeyStorageState,
    pub created_at: i64,
    pub updated_at: i64,
    pub source_checked_at: Option<i64>,
    pub error_code: Option<String>,
    pub github: Option<KeyGitHubStatus>,
    /// Stable legacy row reference; never a value. Introduced only through legacy import.
    pub legacy_reference: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectKeyChangeKind {
    Created,
    Updated,
    ValueSaved,
    StorageChanged,
    RemoteChanged,
}

/// Bounded audit event. No values, digests, credential pointers, process output, or error messages.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectKeyChange {
    pub sequence: u64,
    pub key_id: String,
    pub workspace_id: String,
    pub environment: KeyEnvironment,
    pub revision: u64,
    pub occurred_at: i64,
    pub kind: ProjectKeyChangeKind,
    pub storage_state: KeyStorageState,
    pub remote_state: Option<GitHubRemoteState>,
}
impl ProjectKeyChange {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_revision(self.sequence)
            || !valid_revision(self.revision)
            || !valid_id(&self.key_id)
            || !valid_id(&self.workspace_id)
            || self.occurred_at < 0
        {
            return Err("project_key_change_invalid");
        }
        Ok(())
    }
}
impl ProjectKey {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_id(&self.id)
            || !valid_id(&self.workspace_id)
            || !crate::credential_env::valid_name(&self.name)
            || self.credential_id.as_ref().is_some_and(|id| !valid_id(id))
            || self
                .source_baseline_credential_id
                .as_ref()
                .is_some_and(|id| !valid_id(id))
            || !valid_revision(self.revision)
            || self
                .last_good_revision
                .is_some_and(|r| !valid_revision(r) || r > self.revision)
            || self
                .legacy_reference
                .as_ref()
                .is_some_and(|s| !valid_metadata_text(s, 1024))
            || !valid_error_code(self.error_code.as_deref())
        {
            return Err("project_key_metadata_invalid");
        }
        self.source.validate()?;
        if self.created_at < 0
            || self.updated_at < self.created_at
            || self
                .last_good_at
                .is_some_and(|t| t < 0 || self.last_good_revision.is_none())
            || self.source_checked_at.is_some_and(|t| t < 0)
            || (self.required && self.usage == KeyUsage::StoreOnly)
            || self
                .source_baseline_credential_id
                .as_ref()
                .is_some_and(|id| Some(id) != self.credential_id.as_ref() || !self.source.is_file())
            || (self.delivery == KeyDelivery::FilePath
                && (!matches!(self.source, KeySource::PemFile { .. })
                    || self.kind != KeyKind::PrivateKey))
            || (self.delivery == KeyDelivery::TemporaryFile && self.kind != KeyKind::PrivateKey)
            || (matches!(self.source, KeySource::GitHubOnly)
                && (self.credential_id.is_some()
                    || self.last_good_revision.is_some()
                    || self.usage != KeyUsage::StoreOnly
                    || self.required
                    || self.storage_state == KeyStorageState::Stored))
        {
            return Err("project_key_source_usage_invalid");
        }
        if self.storage_state == KeyStorageState::Stored
            && (self.credential_id.is_none() || self.last_good_revision.is_none())
        {
            return Err("project_key_stored_reference_missing");
        }
        if let Some(github) = &self.github {
            github.target.validate()?;
            if github
                .last_sent_revision
                .is_some_and(|r| !valid_revision(r) || r > self.revision)
                || github
                    .remote_updated_at
                    .as_ref()
                    .is_some_and(|s| !valid_metadata_text(s, 128))
                || !valid_error_code(github.error_code.as_deref())
            {
                return Err("project_key_github_metadata_invalid");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectKeySettings {
    pub workspace_id: String,
    pub environment: KeyEnvironment,
    pub revision: u64,
    pub watch_enabled: bool,
    /// Selected ordered sources only; no searching other files. Later dotenv entries win.
    pub sources: Vec<KeySource>,
}
impl ProjectKeySettings {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_id(&self.workspace_id)
            || !valid_revision(self.revision)
            || self.sources.len() > PROJECT_KEY_SOURCES_MAX
        {
            return Err("project_key_settings_invalid");
        }
        for (i, source) in self.sources.iter().enumerate() {
            source.validate()?;
            if !source.is_file() || self.sources[..i].contains(source) {
                return Err("project_key_settings_source_invalid");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectBuildRecipe {
    pub id: String,
    pub workspace_id: String,
    pub environment: KeyEnvironment,
    pub revision: u64,
    pub name: String,
    /// Executable and arguments, never an implicit shell interpolation.
    pub command: Vec<String>,
    pub cwd: String,
}
impl ProjectBuildRecipe {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_id(&self.id)
            || !valid_id(&self.workspace_id)
            || !valid_revision(self.revision)
            || !valid_metadata_text(&self.name, 256)
            || !valid_metadata_text(&self.cwd, PROJECT_KEY_PATH_BYTES_MAX)
            || self.command.is_empty()
            || self.command.len() > 128
            || self.command[0].is_empty()
            || self
                .command
                .iter()
                .any(|s| s.len() > 4096 || s.contains('\0'))
            || self.command.iter().map(String::len).sum::<usize>() > 8192
        {
            return Err("project_key_recipe_invalid");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectKeyUsedVersion {
    pub key_id: String,
    pub name: String,
    pub revision: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectBuildReceipt {
    /// Kernel identity of the Deppy/CLI process responsible for settling this build.
    #[serde(default)]
    pub owner: Option<crate::process_identity::ProcessIdentity>,
    pub id: String,
    pub workspace_id: String,
    pub environment: KeyEnvironment,
    pub recipe_id: Option<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub used_keys: Vec<ProjectKeyUsedVersion>,
}
impl ProjectBuildReceipt {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_id(&self.id)
            || !valid_id(&self.workspace_id)
            || self.recipe_id.as_ref().is_some_and(|id| !valid_id(id))
            || self.owner.is_some_and(|owner| {
                owner.pid == 0
                    || owner.pid > i32::MAX as u32
                    || owner.birth.microseconds >= 1_000_000
            })
            || self.used_keys.len() > PROJECT_KEYS_MAX
            || self.finished_at.is_some_and(|t| t < self.started_at)
        {
            return Err("project_key_receipt_invalid");
        }
        for (i, key) in self.used_keys.iter().enumerate() {
            if !valid_id(&key.key_id)
                || !crate::credential_env::valid_name(&key.name)
                || !valid_revision(key.revision)
                || self.used_keys[..i]
                    .iter()
                    .any(|other| other.key_id == key.key_id)
            {
                return Err("project_key_receipt_key_invalid");
            }
        }
        Ok(())
    }
}

/// A deleted file-origin name remains excluded from automatic discovery until explicitly imported.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectKeySourceExclusion {
    pub source: KeySource,
    pub name: String,
}
impl ProjectKeySourceExclusion {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.source.is_file() || !crate::credential_env::valid_name(&self.name) {
            return Err("project_key_source_exclusion_invalid");
        }
        self.source.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectKeyCas<T> {
    Committed(T),
    Stale { current_revision: Option<u64> },
}
/// Durable outbox claim. `claim_token` fences a previous process or expired in-flight attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubOutboxEntry {
    pub id: String,
    pub workspace_id: String,
    pub environment: KeyEnvironment,
    pub key_id: String,
    pub key_revision: u64,
    pub target: GitHubTarget,
    pub attempts: u32,
    pub next_attempt_at: i64,
    pub claim_token: Option<String>,
    pub claim_until: Option<i64>,
    pub error_code: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitHubSendOutcome {
    Sent {
        remote_updated_at: Option<String>,
    },
    Retry {
        error_code: String,
        next_attempt_at: i64,
    },
    ExternalChange {
        remote_updated_at: Option<String>,
    },
}

pub fn valid_id(value: &str) -> bool {
    valid_metadata_text(value, PROJECT_KEY_ID_BYTES_MAX)
}
pub fn valid_revision(value: u64) -> bool {
    value > 0 && value <= i64::MAX as u64
}
pub fn valid_metadata_text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
/// Durable failure vocabulary. Raw provider/subprocess text is never accepted as an error code.
pub fn valid_error_code(value: Option<&str>) -> bool {
    value.is_none_or(|code| {
        matches!(
            code,
            "project_key_source_missing"
                | "project_key_source_empty"
                | "project_key_source_changed"
                | "project_key_source_unsafe"
                | "project_key_locked"
                | "project_key_conflict"
                | "project_key_storage_failed"
                | "project_key_github_auth_failed"
                | "project_key_github_failed"
                | "project_key_github_external_change"
                | "project_key_process_failed"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn github_targets_require_exact_scope() {
        let mut target = GitHubTarget {
            auth_credential_id: None,
            account_identity: "test-account".into(),
            scope: GitHubScope::Repository {
                owner: "owner".into(),
                repository: "repo".into(),
            },
            secret_name: "TOKEN".into(),
            auto_send: false,
        };
        assert!(target.validate().is_ok());
        target.scope = GitHubScope::Organization {
            organization: "owner".into(),
            visibility: GitHubVisibility::Selected,
            selected_repository_ids: vec![],
        };
        assert!(target.validate().is_err());
        target.scope = GitHubScope::Environment {
            owner: "owner".into(),
            repository: "repo".into(),
            environment: "production".into(),
        };
        assert!(target.validate().is_ok());
    }
    #[test]
    fn project_keys_github_secret_names_reject_reserved_prefix_case_insensitively() {
        let mut target = GitHubTarget {
            auth_credential_id: None,
            account_identity: "test-account".into(),
            scope: GitHubScope::Repository {
                owner: "owner".into(),
                repository: "repo".into(),
            },
            secret_name: "DEPLOY_TOKEN".into(),
            auto_send: false,
        };
        for name in ["GITHUB_TOKEN", "github_token", "GiThUb_TOKEN", "GITHUB_"] {
            target.secret_name = name.into();
            assert_eq!(
                target.validate(),
                Err("project_key_github_identity_invalid"),
                "reserved GitHub secret name: {name}"
            );
        }
        for name in [
            "DEPLOY_TOKEN",
            "DEPPY_GITHUB_TOKEN",
            "GITHUB",
            "GITHUBISH_TOKEN",
        ] {
            target.secret_name = name.into();
            assert!(
                target.validate().is_ok(),
                "valid alternate GitHub secret name: {name}"
            );
        }
    }
    #[test]
    fn bounded_sources_reject_dotenv_path_traversal_and_duplicate_selection() {
        assert!(
            KeySource::Dotenv {
                file: "../.env".into()
            }
            .validate()
            .is_err()
        );
        let source = KeySource::Dotenv {
            file: ".env".into(),
        };
        assert!(
            ProjectKeySettings {
                workspace_id: "w".into(),
                environment: KeyEnvironment::Development,
                revision: 1,
                watch_enabled: true,
                sources: vec![source.clone(), source]
            }
            .validate()
            .is_err()
        );
    }
    #[test]
    fn project_keys_validation_rejects_remote_only_local_values_and_wrong_delivery_scope() {
        let mut key = ProjectKey {
            id: "key".into(),
            workspace_id: "workspace".into(),
            environment: KeyEnvironment::Development,
            name: "GITHUB_TOKEN".into(),
            credential_id: None,
            source_baseline_credential_id: None,
            source: KeySource::Vault,
            kind: KeyKind::ApiToken,
            usage: KeyUsage::StoreOnly,
            delivery: KeyDelivery::Environment,
            source_priority: 0,
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
        assert!(key.validate().is_ok());
        key.required = true;
        assert!(key.validate().is_err());
        key.required = false;
        key.delivery = KeyDelivery::TemporaryFile;
        assert!(key.validate().is_err());
        key.delivery = KeyDelivery::Environment;
        key.source = KeySource::GitHubOnly;
        // An explicitly remote-only row may be created before its exact target is linked.
        assert!(key.validate().is_ok());
        key.github = Some(KeyGitHubStatus {
            target: GitHubTarget {
                auth_credential_id: None,
                account_identity: "test-account".into(),
                scope: GitHubScope::Repository {
                    owner: "owner".into(),
                    repository: "repo".into(),
                },
                secret_name: "TOKEN".into(),
                auto_send: false,
            },
            state: GitHubRemoteState::ExternalChange,
            last_sent_revision: None,
            remote_updated_at: None,
            error_code: None,
        });
        assert!(key.validate().is_ok());
        key.credential_id = Some("local".into());
        assert!(key.validate().is_err());
        key.credential_id = None;
        key.usage = KeyUsage::Build;
        assert!(key.validate().is_err());
        key.usage = KeyUsage::StoreOnly;
        key.error_code = Some("lowercase_secret_token".into());
        assert!(key.validate().is_err());
        key.error_code = Some("project_key_source_missing".into());
        assert!(key.validate().is_ok());
    }
}
