use deppy_core::project_keys::*;

pub const SECRET_INPUT_BYTES_MAX: usize = 256 * 1024;

/// Editable secret input. Deliberately has neither Debug, Clone nor serialization.
#[derive(Default)]
pub struct SensitiveDraft(String);
impl SensitiveDraft {
    pub fn new(value: String) -> Result<Self, ProjectKeysError> {
        if value.len() > SECRET_INPUT_BYTES_MAX || value.contains('\0') {
            let mut draft = Self(value);
            draft.clear();
            return Err(ProjectKeysError::InvalidInput);
        }
        Ok(Self(value))
    }
    pub fn text_mut(&mut self) -> &mut String {
        &mut self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn clear(&mut self) {
        // Zero the allocation before releasing it, including text removed by the editor.
        unsafe {
            let bytes = self.0.as_mut_vec();
            let capacity = bytes.capacity();
            for index in 0..capacity {
                std::ptr::write_volatile(bytes.as_mut_ptr().add(index), 0);
            }
            bytes.set_len(0);
        }
    }
    pub fn into_secret(mut self) -> Result<secret::SecretString, ProjectKeysError> {
        if self.0.len() > SECRET_INPUT_BYTES_MAX || self.0.contains('\0') {
            return Err(ProjectKeysError::InvalidInput);
        }
        Ok(secret::SecretString::new(std::mem::take(&mut self.0)))
    }
}
impl Drop for SensitiveDraft {
    fn drop(&mut self) {
        self.clear();
    }
}

pub enum ValueEdit {
    Keep,
    Replace(SensitiveDraft),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProjectKeysScope {
    pub workspace_id: String,
    pub environment: KeyEnvironment,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectKeysFence {
    pub scope: ProjectKeysScope,
    pub generation: u64,
    pub request_id: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectChoice {
    pub id: String,
    pub name: String,
    pub path: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialChoice {
    pub id: String,
    pub label: String,
    pub provider: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourcePreview {
    pub source: KeySource,
    pub names: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectKeysError {
    InvalidInput,
    Busy,
    Stale,
    ScopeChanged,
    Storage,
    Locked,
    SourceMissing,
    SourceEmpty,
    SourceChanged,
    SourceUnsafe,
    Conflict,
    Clipboard,
    Process,
    GitHubAuth,
    GitHubRemote,
    ExternalChange,
}
impl ProjectKeysError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidInput => "project_key_input_invalid",
            Self::Busy => "project_key_busy",
            Self::Stale => "project_key_stale",
            Self::ScopeChanged => "project_key_scope_changed",
            Self::Storage => "project_key_storage_failed",
            Self::Locked => "project_key_locked",
            Self::SourceMissing => "project_key_source_missing",
            Self::SourceEmpty => "project_key_source_empty",
            Self::SourceChanged => "project_key_source_changed",
            Self::SourceUnsafe => "project_key_source_unsafe",
            Self::Conflict => "project_key_conflict",
            Self::Clipboard => "project_key_clipboard_failed",
            Self::Process => "project_key_process_failed",
            Self::GitHubAuth => "project_key_github_auth_failed",
            Self::GitHubRemote => "project_key_github_failed",
            Self::ExternalChange => "project_key_github_external_change",
        }
    }
}
impl std::fmt::Display for ProjectKeysError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}
impl std::error::Error for ProjectKeysError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectKeysSnapshot {
    pub scope: ProjectKeysScope,
    pub generation: u64,
    pub keys: Vec<ProjectKey>,
    pub settings: ProjectKeySettings,
    pub recipes: Vec<ProjectBuildRecipe>,
    pub receipts: Vec<ProjectBuildReceipt>,
    pub projects: Vec<ProjectChoice>,
    pub auth_credentials: Vec<CredentialChoice>,
    pub source_preview: Option<SourcePreview>,
    pub error: Option<ProjectKeysError>,
}
impl ProjectKeysSnapshot {
    pub fn unavailable(scope: ProjectKeysScope, generation: u64, error: ProjectKeysError) -> Self {
        Self {
            settings: ProjectKeySettings {
                workspace_id: scope.workspace_id.clone(),
                environment: scope.environment,
                revision: 1,
                watch_enabled: false,
                sources: vec![],
            },
            scope,
            generation,
            keys: vec![],
            recipes: vec![],
            receipts: vec![],
            projects: vec![],
            auth_credentials: vec![],
            source_preview: None,
            error: Some(error),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictResolution {
    KeepSource,
    KeepVault,
}
pub enum ProjectKeysAction {
    Refresh {
        key_id: Option<String>,
    },
    LoadCredentials {
        after: Option<String>,
    },
    LoadKeyChanges {
        key_id: String,
        expected_revision: u64,
    },
    SetWatch {
        enabled: bool,
        expected_revision: u64,
    },
    SetSources {
        sources: Vec<KeySource>,
        expected_revision: u64,
    },
    PickSource {
        pem: bool,
    },
    PreviewSource {
        source: KeySource,
    },
    ImportSource {
        source: KeySource,
        names: Vec<String>,
    },
    SaveKey {
        key: Box<ProjectKey>,
        expected_revision: Option<u64>,
        value: ValueEdit,
    },
    DeleteKey {
        key_id: String,
        expected_revision: u64,
    },
    CopyKey {
        key_id: String,
        expected_revision: u64,
    },
    ResolveConflict {
        key_id: String,
        expected_revision: u64,
        resolution: ConflictResolution,
    },
    SaveGitHubTarget {
        key_id: String,
        expected_revision: u64,
        target: Option<Box<GitHubTarget>>,
    },
    SendGitHub {
        key_id: String,
        expected_revision: u64,
        allow_external_change: bool,
    },
    SaveRecipe {
        recipe: ProjectBuildRecipe,
        expected_revision: Option<u64>,
    },
    DeleteRecipe {
        recipe_id: String,
        expected_revision: u64,
    },
    RunRecipe {
        recipe_id: String,
        expected_revision: u64,
    },
    LegacyAdvanced,
}
pub enum ProjectKeysIntent {
    SelectScope(ProjectKeysScope),
    Request {
        fence: ProjectKeysFence,
        action: ProjectKeysAction,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectKeysAckPayload {
    KeyChanges {
        key_id: String,
        changes: Vec<ProjectKeyChange>,
    },
    Done,
    RuntimeChanged,
    Copied,
    DeletedKey {
        key_id: String,
        usage: KeyUsage,
    },
    SourcePicked(KeySource),
    Preview(SourcePreview),
    CredentialsPage {
        choices: Vec<CredentialChoice>,
        after: Option<String>,
    },
    SavedKey(Box<ProjectKey>),
    SavedRecipe(ProjectBuildRecipe),
    BuildFinished(ProjectBuildReceipt),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectKeysAck {
    pub fence: ProjectKeysFence,
    pub result: Result<ProjectKeysAckPayload, ProjectKeysError>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sensitive_draft_revalidates_editor_input_and_blank_keep_is_explicit() {
        let mut draft = SensitiveDraft::default();
        draft
            .text_mut()
            .push_str(&"x".repeat(SECRET_INPUT_BYTES_MAX + 1));
        assert!(matches!(
            draft.into_secret(),
            Err(ProjectKeysError::InvalidInput)
        ));
        let mut draft = SensitiveDraft::new("secret".into()).unwrap();
        draft.clear();
        assert!(draft.is_empty());
        assert_eq!(
            ProjectKeysError::Process.to_string(),
            "project_key_process_failed"
        );
    }
}
