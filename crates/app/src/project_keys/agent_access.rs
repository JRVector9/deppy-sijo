//! Current-session API capabilities and selected-key execution.
use super::{
    contracts::*,
    process::ProcessRunner,
    repository::ProjectApiRepository,
    service::{ProjectKeysService, now, storage_error},
};
use deppy_core::{project_api_access::*, project_keys::ProjectBuildReceipt};
use std::path::Path;

pub struct AgentApiBootstrap {
    pub args: Vec<String>,
    pub artifacts: Option<BootstrapArtifacts>,
    pub capability: Option<secret::SecretString>,
    pub unavailable_reason: Option<&'static str>,
}
pub struct BootstrapArtifacts {
    pub directory: std::path::PathBuf,
}
impl Drop for BootstrapArtifacts {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.directory.join("rules/project-apis.mdc"));
        let _ = std::fs::remove_file(self.directory.join(".cursor-plugin/plugin.json"));
        let _ = std::fs::remove_dir(self.directory.join("rules"));
        let _ = std::fs::remove_dir(self.directory.join(".cursor-plugin"));
        let _ = std::fs::remove_dir(&self.directory);
    }
}
pub fn prepare_local_bootstrap(
    db: &dyn ProjectApiRepository,
    provider: &str,
    scope: &ProjectApiSessionScope,
    executable: &Path,
    remote: bool,
) -> Result<AgentApiBootstrap, ProjectKeysError> {
    scope
        .validate()
        .map_err(|_| ProjectKeysError::InvalidInput)?;
    if scope.actor_kind != ProjectApiActorKind::LocalSession || scope.principal_id != "local-agent"
    {
        return Err(ProjectKeysError::InvalidInput);
    }
    let unavailable = if remote || !storage_error(db.workspace_is_local(&scope.workspace_id))? {
        Some("remote_secret_adapter_unavailable")
    } else if !matches!(
        provider,
        "claude" | "codex" | "cursor" | "cursor-agent" | "grok"
    ) {
        Some("provider_additive_context_unavailable")
    } else {
        None
    };
    if unavailable.is_some() {
        return Ok(AgentApiBootstrap {
            args: vec![],
            artifacts: None,
            capability: None,
            unavailable_reason: unavailable,
        });
    }
    let instructions = local_api_instructions(scope, executable)?;
    let mut artifacts = None;
    let args = match provider {
        "claude" => vec!["--append-system-prompt".into(), instructions.clone()],
        "grok" => vec!["--rules".into(), instructions.clone()],
        "codex" => {
            let name = format!("mcp_servers.deppy_api_{}", uuid::Uuid::new_v4().simple());
            let command = executable.to_str().ok_or(ProjectKeysError::InvalidInput)?;
            let arguments = vec![
                "env",
                "mcp",
                "--workspace",
                &scope.workspace_id,
                "--environment",
                scope.environment.as_str(),
                "--session",
                &scope.session_id,
                "--generation",
                &scope.generation,
            ];
            vec![
                "-c".into(),
                format!(
                    "{name}.command={}",
                    serde_json::to_string(command).map_err(|_| ProjectKeysError::InvalidInput)?
                ),
                "-c".into(),
                format!(
                    "{name}.args={}",
                    serde_json::to_string(&arguments)
                        .map_err(|_| ProjectKeysError::InvalidInput)?
                ),
                "-c".into(),
                format!("{name}.env_vars=[\"DEPPY_PROJECT_API_CAPABILITY\"]"),
            ]
        }
        "cursor" | "cursor-agent" => {
            let directory =
                std::env::temp_dir().join(format!("deppy-project-api-{}", uuid::Uuid::new_v4()));
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder
                .create(&directory)
                .map_err(|_| ProjectKeysError::Storage)?;
            let guard = BootstrapArtifacts { directory };
            std::fs::create_dir(guard.directory.join(".cursor-plugin"))
                .map_err(|_| ProjectKeysError::Storage)?;
            std::fs::create_dir(guard.directory.join("rules"))
                .map_err(|_| ProjectKeysError::Storage)?;
            let manifest = serde_json::json!({"name":format!("deppy-project-api-{}",uuid::Uuid::new_v4().simple()),"version":"1.0.0","description":"Current Deppy project API access","rules":"./rules/"});
            write_bootstrap_file(
                &guard.directory.join(".cursor-plugin/plugin.json"),
                serde_json::to_string(&manifest)
                    .map_err(|_| ProjectKeysError::Storage)?
                    .as_bytes(),
            )?;
            write_bootstrap_file(&guard.directory.join("rules/project-apis.mdc"),format!("---\ndescription: Current Deppy project API access\nalwaysApply: true\n---\n{instructions}\n").as_bytes())?;
            let args = vec![
                "--plugin-dir".into(),
                guard.directory.to_string_lossy().into_owned(),
            ];
            artifacts = Some(guard);
            args
        }
        _ => unreachable!(),
    };
    let at = now();
    let capability =
        super::session_capability::issue_local_capability(db, scope, at + 24 * 60 * 60, at)?;
    let digest = super::session_capability::capability_digest(capability.expose())?;
    if let Err(error) = storage_error(db.project_api_local_grants_refresh(scope, &digest, at)) {
        let _ = db.project_api_grant_revoke_scope(scope);
        return Err(error);
    }
    Ok(AgentApiBootstrap {
        args,
        artifacts,
        capability: Some(capability),
        unavailable_reason: None,
    })
}
fn write_bootstrap_file(path: &Path, bytes: &[u8]) -> Result<(), ProjectKeysError> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options
        .open(path)
        .and_then(|mut file| file.write_all(bytes))
        .map_err(|_| ProjectKeysError::Storage)
}
pub(super) fn local_api_instructions(
    scope: &ProjectApiSessionScope,
    executable: &Path,
) -> Result<String, ProjectKeysError> {
    let executable = executable
        .to_str()
        .filter(|value| {
            executable.is_absolute() && value.len() <= 4096 && !value.chars().any(char::is_control)
        })
        .ok_or(ProjectKeysError::InvalidInput)?;
    let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
    let prefix = format!("{} env", quote(executable));
    let scope_args = format!(
        "--workspace {} --environment {} --session {} --generation {}",
        quote(&scope.workspace_id),
        scope.environment.as_str(),
        quote(&scope.session_id),
        quote(&scope.generation)
    );
    Ok(format!(
        "Deppy registered project APIs are available for this project/environment/session only. Discover metadata using `{prefix} list {scope_args}`. The catalog contains no secret values. Use only available key IDs by running `{prefix} run {scope_args} --key KEY_ID [--key OTHER_ID] -- COMMAND ARG...` (or `--recipe RECIPE_ID` for a saved recipe). This resolves current values only into that command's child environment. Never print credentials, request a secret dump, or write credentials into project files. An unavailable API requires the owner to update its registration."
    ))
}

impl ProjectKeysService {
    pub fn run_agent_command(
        &self,
        actor: &ProjectApiSessionScope,
        key_ids: &[String],
        recipe_id: Option<&str>,
        command: &[String],
        cwd: &Path,
        runner: &dyn ProcessRunner,
    ) -> Result<ProjectBuildReceipt, ProjectKeysError> {
        actor
            .validate()
            .map_err(|_| ProjectKeysError::InvalidInput)?;
        if key_ids.is_empty()
            || key_ids.len() > deppy_core::project_keys::PROJECT_KEYS_MAX
            || key_ids.iter().enumerate().any(|(index, id)| {
                !deppy_core::project_keys::valid_id(id) || key_ids[..index].contains(id)
            })
            || (actor.actor_kind == ProjectApiActorKind::CloudConnection && recipe_id.is_none())
        {
            return Err(ProjectKeysError::InvalidInput);
        }
        let scope = ProjectKeysScope {
            workspace_id: actor.workspace_id.clone(),
            environment: actor.environment,
        };
        if storage_error(self.db.workspace_target(&scope.workspace_id))?
            .is_none_or(|target| target.local_root().is_none())
        {
            return Err(ProjectKeysError::Locked);
        }
        let recipe = recipe_id
            .map(|id| {
                storage_error(
                    self.db
                        .project_key_recipe_list(&scope.workspace_id, scope.environment),
                )?
                .into_iter()
                .find(|recipe| recipe.id == id)
                .ok_or(ProjectKeysError::InvalidInput)
            })
            .transpose()?;
        if let Some(recipe) = &recipe
            && (command != recipe.command || cwd != Path::new(&recipe.cwd))
        {
            return Err(ProjectKeysError::InvalidInput);
        }
        self.check_agent_grants(actor, key_ids, recipe_id)?;
        self.run_build_selected(
            &scope,
            recipe.as_ref(),
            command,
            cwd,
            runner,
            Some((actor, key_ids)),
        )
    }

    pub(super) fn check_agent_grants(
        &self,
        actor: &ProjectApiSessionScope,
        key_ids: &[String],
        recipe_id: Option<&str>,
    ) -> Result<(), ProjectKeysError> {
        let (operation, grant_recipe) = if actor.actor_kind == ProjectApiActorKind::LocalSession {
            (ProjectApiOperation::Environment, None)
        } else {
            (ProjectApiOperation::Recipe, recipe_id)
        };
        for id in key_ids {
            if !storage_error(self.db.project_api_grant_allows(
                actor,
                id,
                operation,
                grant_recipe,
                now(),
            ))? {
                return Err(ProjectKeysError::Locked);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_keys::{
        process::{ProcessEnvironment, ProcessOutput},
        tests::Fixture,
    };
    use deppy_core::project_keys::*;
    use secret::SecretString;
    use std::sync::Mutex;
    #[derive(Default)]
    struct Recorder(Mutex<Vec<Vec<(String, String)>>>);
    impl ProcessRunner for Recorder {
        fn run(
            &self,
            _: &[String],
            _: &Path,
            env: &ProcessEnvironment,
            _: Option<&SecretString>,
            _: bool,
        ) -> Result<ProcessOutput, ProjectKeysError> {
            self.0.lock().unwrap().push(
                env.values
                    .iter()
                    .map(|(k, v)| (k.clone(), v.expose().into()))
                    .collect(),
            );
            Ok(ProcessOutput {
                exit_code: 0,
                stdout: vec![],
            })
        }
    }

    #[test]
    fn project_api_selected_execution_uses_latest_value_without_promoting_other_keys() {
        let fixture = Fixture::new();
        let mut selected = fixture.key("SERVICE_TOKEN", KeySource::Vault);
        selected.usage = KeyUsage::StoreOnly;
        selected.required = false;
        let selected = fixture.save(selected, "first-fixture-value");
        fixture.save(
            fixture.key("UNSELECTED_BUILD", KeySource::Vault),
            "unselected-fixture-value",
        );
        fixture
            .service
            .db
            .project_api_access_save(
                &ProjectApiAccess {
                    key_id: selected.id.clone(),
                    workspace_id: fixture.scope.workspace_id.clone(),
                    environment: fixture.scope.environment,
                    revision: 1,
                    execution_name: Some(selected.name.clone()),
                    service_name: Some("Service".into()),
                    description: None,
                    documentation_url: None,
                    agent_enabled: true,
                },
                None,
            )
            .unwrap();
        let actor = ProjectApiSessionScope {
            workspace_id: fixture.scope.workspace_id.clone(),
            environment: fixture.scope.environment,
            principal_id: "local".into(),
            session_id: "session".into(),
            generation: "execution".into(),
            actor_kind: ProjectApiActorKind::LocalSession,
        };
        fixture
            .service
            .db
            .project_api_grant_save(
                &ProjectApiGrant {
                    scope: actor.clone(),
                    key_id: selected.id.clone(),
                    access_revision: 1,
                    operation: ProjectApiOperation::Environment,
                    recipe_id: None,
                    expires_at: super::super::service::now() + 60,
                },
                super::super::service::now(),
            )
            .unwrap();
        let recorder = Recorder::default();
        let first = fixture
            .service
            .run_agent_command(
                &actor,
                std::slice::from_ref(&selected.id),
                None,
                &["fixture".into()],
                Path::new("."),
                &recorder,
            )
            .unwrap();
        let rotated = fixture
            .service
            .save_key(
                &fixture.scope,
                selected.clone(),
                Some(selected.revision),
                ValueEdit::Replace(SensitiveDraft::new("rotated-fixture-value".into()).unwrap()),
            )
            .unwrap();
        let second = fixture
            .service
            .run_agent_command(
                &actor,
                std::slice::from_ref(&selected.id),
                None,
                &["fixture".into()],
                Path::new("."),
                &recorder,
            )
            .unwrap();
        assert_eq!(first.used_keys.len(), 1);
        assert_eq!(second.used_keys.len(), 1);
        assert_eq!(
            second.used_keys[0].revision,
            rotated.last_good_revision.unwrap()
        );
        assert_eq!(
            *recorder.0.lock().unwrap(),
            vec![
                vec![("SERVICE_TOKEN".into(), "first-fixture-value".into())],
                vec![("SERVICE_TOKEN".into(), "rotated-fixture-value".into())]
            ]
        );
        fixture
            .service
            .db
            .project_api_grant_revoke_scope(&actor)
            .unwrap();
        assert!(
            fixture
                .service
                .run_agent_command(
                    &actor,
                    std::slice::from_ref(&selected.id),
                    None,
                    &["fixture".into()],
                    Path::new("."),
                    &recorder
                )
                .is_err()
        );
        assert_eq!(recorder.0.lock().unwrap().len(), 2);
    }

    #[test]
    fn project_api_cloud_read_scope_never_grants_secret_environment_execution() {
        let fixture = Fixture::new();
        let key = fixture.save(fixture.key("TOKEN", KeySource::Vault), "fixture-value");
        let actor = ProjectApiSessionScope {
            workspace_id: fixture.scope.workspace_id.clone(),
            environment: fixture.scope.environment,
            principal_id: "cloud".into(),
            session_id: "shared-session".into(),
            generation: "generation".into(),
            actor_kind: ProjectApiActorKind::CloudConnection,
        };
        let recorder = Recorder::default();
        assert!(
            fixture
                .service
                .run_agent_command(
                    &actor,
                    &[key.id],
                    None,
                    &["fixture".into()],
                    Path::new("."),
                    &recorder
                )
                .is_err()
        );
        assert!(recorder.0.lock().unwrap().is_empty());
    }

    #[test]
    fn project_api_generic_key_save_preserves_immutable_binding() {
        let fixture = Fixture::new();
        let key = fixture.save(
            fixture.key("ORIGINAL_TOKEN", KeySource::Vault),
            "fixture-value",
        );
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
        let actor = ProjectApiSessionScope {
            workspace_id: key.workspace_id.clone(),
            environment: key.environment,
            principal_id: "local".into(),
            session_id: "session".into(),
            generation: "generation".into(),
            actor_kind: ProjectApiActorKind::LocalSession,
        };
        let at = super::super::service::now();
        fixture
            .service
            .db
            .project_api_grant_save(
                &ProjectApiGrant {
                    scope: actor.clone(),
                    key_id: key.id.clone(),
                    access_revision: 1,
                    operation: ProjectApiOperation::Environment,
                    recipe_id: None,
                    expires_at: at + 60,
                },
                at,
            )
            .unwrap();
        let mut changed = key.clone();
        changed.name = "DIFFERENT_TOKEN".into();
        assert_eq!(
            fixture
                .service
                .save_key(&fixture.scope, changed, Some(key.revision), ValueEdit::Keep),
            Err(ProjectKeysError::Storage)
        );
        let recorder = Recorder::default();
        fixture
            .service
            .run_agent_command(
                &actor,
                &[key.id],
                None,
                &["fixture".into()],
                Path::new("."),
                &recorder,
            )
            .unwrap();
        assert_eq!(recorder.0.lock().unwrap()[0][0].0, "ORIGINAL_TOKEN");
    }

    #[test]
    fn project_api_bootstrap_is_model_visible_without_overwriting_codex_instructions() {
        let fixture = Fixture::new();
        let key = fixture.save(
            fixture.key("TOKEN", KeySource::Vault),
            "fixture-private-value",
        );
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
        let actor = ProjectApiSessionScope {
            workspace_id: key.workspace_id,
            environment: key.environment,
            principal_id: "local-agent".into(),
            session_id: "session".into(),
            generation: "generation".into(),
            actor_kind: ProjectApiActorKind::LocalSession,
        };
        let calls = fixture.store.2.load(std::sync::atomic::Ordering::Acquire);
        let claude = prepare_local_bootstrap(
            &fixture.service.db,
            "claude",
            &actor,
            Path::new("/fixture/deppy"),
            false,
        )
        .unwrap();
        assert_eq!(claude.args[0], "--append-system-prompt");
        assert!(claude.args[1].contains("env list"));
        assert!(claude.args[1].contains(&actor.workspace_id));
        assert!(!claude.args[1].contains("fixture-private-value"));
        assert!(
            fixture
                .service
                .db
                .project_api_grant_allows(
                    &actor,
                    &key.id,
                    ProjectApiOperation::Environment,
                    None,
                    super::super::service::now()
                )
                .unwrap()
        );
        let codex = prepare_local_bootstrap(
            &fixture.service.db,
            "codex",
            &actor,
            Path::new("/fixture/deppy"),
            false,
        )
        .unwrap();
        let args = codex.args.join(" ");
        assert!(args.contains("mcp_servers.deppy_api_"));
        assert!(args.contains("env"));
        assert!(args.contains("mcp"));
        assert!(
            args.contains(".env_vars=[\"DEPPY_PROJECT_API_CAPABILITY\"]"),
            "Codex stdio must explicitly forward only the capability environment name"
        );
        assert!(!args.contains("developer_instructions"));
        assert!(!args.contains("fixture-private-value"));
        assert_eq!(
            fixture.store.2.load(std::sync::atomic::Ordering::Acquire),
            calls
        );
    }
    #[test]
    fn project_api_bootstrap_cursor_artifacts_are_scoped_and_unsupported_adapters_do_not_grant() {
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
        let actor = ProjectApiSessionScope {
            workspace_id: key.workspace_id,
            environment: key.environment,
            principal_id: "local-agent".into(),
            session_id: "session".into(),
            generation: "generation".into(),
            actor_kind: ProjectApiActorKind::LocalSession,
        };
        for (provider, remote) in [("kimi", false), ("claude", true)] {
            let result = prepare_local_bootstrap(
                &fixture.service.db,
                provider,
                &actor,
                Path::new("/fixture/deppy"),
                remote,
            )
            .unwrap();
            assert!(result.args.is_empty());
            assert!(result.unavailable_reason.is_some());
            assert!(
                !fixture
                    .service
                    .db
                    .project_api_grant_allows(
                        &actor,
                        &key.id,
                        ProjectApiOperation::Environment,
                        None,
                        super::super::service::now()
                    )
                    .unwrap()
            );
        }
        let cursor = prepare_local_bootstrap(
            &fixture.service.db,
            "cursor",
            &actor,
            Path::new("/fixture/deppy"),
            false,
        )
        .unwrap();
        assert_eq!(cursor.args[0], "--plugin-dir");
        let directory = cursor.artifacts.as_ref().unwrap().directory.clone();
        assert!(directory.join(".cursor-plugin/plugin.json").is_file());
        let rule = std::fs::read_to_string(directory.join("rules/project-apis.mdc")).unwrap();
        assert!(rule.contains("alwaysApply: true"));
        assert!(rule.contains("env list"));
        drop(cursor);
        assert!(!directory.exists());
    }
    #[test]
    fn project_api_selected_child_does_not_inherit_session_capability() {
        struct ChildRecorder;
        impl ProcessRunner for ChildRecorder {
            fn run(
                &self,
                _: &[String],
                _: &Path,
                env: &ProcessEnvironment,
                _: Option<&SecretString>,
                _: bool,
            ) -> Result<ProcessOutput, ProjectKeysError> {
                assert!(
                    env.remove_names
                        .iter()
                        .any(|name| name == "DEPPY_PROJECT_API_CAPABILITY")
                );
                assert!(
                    env.values
                        .iter()
                        .all(|(name, _)| name != "DEPPY_PROJECT_API_CAPABILITY")
                );
                Ok(ProcessOutput {
                    exit_code: 0,
                    stdout: vec![],
                })
            }
        }
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
        let actor = ProjectApiSessionScope {
            workspace_id: key.workspace_id,
            environment: key.environment,
            principal_id: "local-agent".into(),
            session_id: "session".into(),
            generation: "generation".into(),
            actor_kind: ProjectApiActorKind::LocalSession,
        };
        let at = super::super::service::now();
        fixture
            .service
            .db
            .project_api_grant_save(
                &ProjectApiGrant {
                    scope: actor.clone(),
                    key_id: key.id.clone(),
                    access_revision: 1,
                    operation: ProjectApiOperation::Environment,
                    recipe_id: None,
                    expires_at: at + 60,
                },
                at,
            )
            .unwrap();
        fixture
            .service
            .run_agent_command(
                &actor,
                &[key.id],
                None,
                &["fixture".into()],
                Path::new("."),
                &ChildRecorder,
            )
            .unwrap();
    }

    #[test]
    fn project_api_local_saved_recipe_uses_authenticated_environment_key_subset() {
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
        let actor = ProjectApiSessionScope {
            workspace_id: key.workspace_id.clone(),
            environment: key.environment,
            principal_id: "local-agent".into(),
            session_id: "session".into(),
            generation: "generation".into(),
            actor_kind: ProjectApiActorKind::LocalSession,
        };
        let recipe = ProjectBuildRecipe {
            id: "recipe".into(),
            workspace_id: key.workspace_id,
            environment: key.environment,
            revision: 1,
            name: "Fixture recipe".into(),
            command: vec!["fixture".into()],
            cwd: ".".into(),
        };
        fixture
            .service
            .db
            .project_key_recipe_save(&actor.workspace_id, actor.environment, &recipe, None)
            .unwrap();
        let at = super::super::service::now();
        fixture
            .service
            .db
            .project_api_grant_save(
                &ProjectApiGrant {
                    scope: actor.clone(),
                    key_id: key.id.clone(),
                    access_revision: 1,
                    operation: ProjectApiOperation::Environment,
                    recipe_id: None,
                    expires_at: at + 60,
                },
                at,
            )
            .unwrap();
        let recorder = Recorder::default();
        let receipt = fixture
            .service
            .run_agent_command(
                &actor,
                &[key.id],
                Some(&recipe.id),
                &recipe.command,
                Path::new(&recipe.cwd),
                &recorder,
            )
            .unwrap();
        assert_eq!(receipt.used_keys.len(), 1);
        assert_eq!(recorder.0.lock().unwrap().len(), 1);
    }
}
