use super::{
    contracts::*,
    service::{ProjectKeysService, storage_error},
};
use deppy_core::project_keys::KeyEnvironment;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

pub struct EnvRun {
    pub scope: ProjectKeysScope,
    pub recipe_id: Option<String>,
    pub command: Vec<String>,
    pub cwd: Option<PathBuf>,
}
pub enum CliAction {
    Gui,
    Print(String),
    Run(EnvRun),
}
pub fn dispatch(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<CliAction, ProjectKeysError> {
    let arguments = arguments.into_iter().take(161).collect::<Vec<_>>();
    if arguments.len() > 160 {
        return Err(ProjectKeysError::InvalidInput);
    }
    let rest = &arguments[arguments.len().min(1)..];
    if rest == [OsString::from("--version")] {
        return Ok(CliAction::Print(format!(
            "Deppy Sijo {}",
            env!("CARGO_PKG_VERSION")
        )));
    }
    if rest == [OsString::from("env"), OsString::from("--help")]
        || rest
            == [
                OsString::from("env"),
                OsString::from("run"),
                OsString::from("--help"),
            ]
    {
        return Ok(CliAction::Print("Usage: deppy-sijo env run --workspace ID --environment development|production [--cwd PATH] [--recipe ID | -- COMMAND ARG...]".into()));
    }
    Ok(match parse(arguments)? {
        Some(request) => CliAction::Run(request),
        None => CliAction::Gui,
    })
}
/// Parse before initializing GUI, logs, or the application instance lock.
pub fn parse(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<Option<EnvRun>, ProjectKeysError> {
    let args = arguments
        .into_iter()
        .skip(1)
        .map(|value| {
            value
                .into_string()
                .map_err(|_| ProjectKeysError::InvalidInput)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if args.first().map(String::as_str) != Some("env") {
        return Ok(None);
    }
    if args.get(1).map(String::as_str) != Some("run") {
        return Err(ProjectKeysError::InvalidInput);
    }
    let (mut workspace, mut environment, mut recipe_id, mut cwd) = (None, None, None, None);
    let mut command = vec![];
    let mut index = 2;
    while index < args.len() {
        if args[index] == "--" {
            command = args[index + 1..].to_vec();
            break;
        }
        let value = args.get(index + 1).ok_or(ProjectKeysError::InvalidInput)?;
        match args[index].as_str() {
            "--workspace" if workspace.is_none() => workspace = Some(value.clone()),
            "--environment" if environment.is_none() => {
                environment = Some(match value.as_str() {
                    "development" | "dev" => KeyEnvironment::Development,
                    "production" | "prod" => KeyEnvironment::Production,
                    _ => return Err(ProjectKeysError::InvalidInput),
                })
            }
            "--recipe" if recipe_id.is_none() => recipe_id = Some(value.clone()),
            "--cwd" if cwd.is_none() => cwd = Some(PathBuf::from(value)),
            _ => return Err(ProjectKeysError::InvalidInput),
        }
        index += 2;
    }
    let workspace_id = workspace
        .filter(|id| deppy_core::project_keys::valid_id(id))
        .ok_or(ProjectKeysError::InvalidInput)?;
    if recipe_id.is_some() != command.is_empty() {
        return Err(ProjectKeysError::InvalidInput);
    }
    if command.len() > 128 || command.iter().map(String::len).sum::<usize>() > 8192 {
        return Err(ProjectKeysError::InvalidInput);
    }
    Ok(Some(EnvRun {
        scope: ProjectKeysScope {
            workspace_id,
            environment: environment.ok_or(ProjectKeysError::InvalidInput)?,
        },
        recipe_id,
        command,
        cwd,
    }))
}
pub fn run(
    request: EnvRun,
    service: &ProjectKeysService,
    runner: &dyn super::process::ProcessRunner,
) -> Result<i32, ProjectKeysError> {
    let recipe =
        request
            .recipe_id
            .as_ref()
            .map(|id| {
                storage_error(service.db.project_key_recipe_list(
                    &request.scope.workspace_id,
                    request.scope.environment,
                ))
                .and_then(|recipes| {
                    recipes
                        .into_iter()
                        .find(|recipe| &recipe.id == id)
                        .ok_or(ProjectKeysError::InvalidInput)
                })
            })
            .transpose()?;
    let command = recipe
        .as_ref()
        .map(|recipe| recipe.command.as_slice())
        .unwrap_or(&request.command);
    let cwd = request
        .cwd
        .or_else(|| recipe.as_ref().map(|recipe| PathBuf::from(&recipe.cwd)))
        .map(Ok)
        .unwrap_or_else(|| service.root(&request.scope))?;
    let receipt = service.run_build(
        &request.scope,
        recipe.as_ref(),
        command,
        Path::new(&cwd),
        runner,
    )?;
    Ok(receipt.exit_code.unwrap_or(1))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn parse_text(args: &[&str]) -> Result<Option<EnvRun>, ProjectKeysError> {
        parse(args.iter().map(OsString::from))
    }
    #[test]
    fn cli_requires_explicit_project_and_environment_and_preserves_argv() {
        let request = parse_text(&[
            "deppy",
            "env",
            "run",
            "--workspace",
            "workspace",
            "--environment",
            "prod",
            "--",
            "echo",
            "$TOKEN",
            "a b",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(request.scope.environment, KeyEnvironment::Production);
        assert_eq!(request.command, ["echo", "$TOKEN", "a b"]);
        assert!(
            parse_text(&[
                "deppy",
                "env",
                "run",
                "--workspace",
                "workspace",
                "--",
                "echo"
            ])
            .is_err()
        );
        assert!(
            parse_text(&[
                "deppy",
                "env",
                "run",
                "--workspace",
                "workspace",
                "--environment",
                "dev",
                "--recipe",
                "recipe",
                "--",
                "echo"
            ])
            .is_err()
        );
        assert!(parse_text(&["deppy"]).unwrap().is_none());
    }
    #[test]
    fn version_and_help_are_pure_dispatch_before_paths_and_secret_store() {
        for args in [
            vec!["deppy", "--version"],
            vec!["deppy", "env", "--help"],
            vec!["deppy", "env", "run", "--help"],
        ] {
            let action = dispatch(args.into_iter().map(OsString::from)).unwrap();
            assert!(matches!(action,CliAction::Print(text)if !text.is_empty()));
        }
    }
    #[test]
    fn saved_recipe_cli_uses_latest_original_exact_argv_and_project_relative_cwd() {
        use crate::project_keys::{
            process::{ProcessEnvironment, ProcessOutput, ProcessRunner},
            tests::Fixture,
        };
        use deppy_core::project_keys::*;
        use secret::SecretString;
        use std::sync::Mutex;
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.root.join("nested")).unwrap();
        std::fs::write(fixture.root.join(".env"), "TOKEN=old-source\n").unwrap();
        fixture
            .service
            .import_source(
                &fixture.scope,
                KeySource::Dotenv {
                    file: ".env".into(),
                },
                vec!["TOKEN".into()],
            )
            .unwrap();
        let argv = vec![
            "fixture".to_string(),
            "".into(),
            "a b".into(),
            "line\narg".into(),
            "$TOKEN".into(),
        ];
        let recipe = ProjectBuildRecipe {
            id: uuid::Uuid::new_v4().to_string(),
            workspace_id: fixture.scope.workspace_id.clone(),
            environment: fixture.scope.environment,
            revision: 1,
            name: "exact arguments".into(),
            command: argv.clone(),
            cwd: "nested".into(),
        };
        fixture
            .service
            .db
            .project_key_recipe_save(
                &fixture.scope.workspace_id,
                fixture.scope.environment,
                &recipe,
                None,
            )
            .unwrap();
        std::fs::write(fixture.root.join(".env"), "TOKEN=latest-source\n").unwrap();
        struct Runner {
            argv: Vec<String>,
            cwd: PathBuf,
            seen: Mutex<bool>,
        }
        impl ProcessRunner for Runner {
            fn run(
                &self,
                argv: &[String],
                cwd: &Path,
                env: &ProcessEnvironment,
                input: Option<&SecretString>,
                inherit: bool,
            ) -> Result<ProcessOutput, ProjectKeysError> {
                assert_eq!(argv, self.argv);
                assert_eq!(
                    cwd.canonicalize().unwrap(),
                    self.cwd.canonicalize().unwrap()
                );
                assert!(inherit);
                assert!(input.is_none());
                assert!(
                    env.values
                        .iter()
                        .any(|(name, value)| name == "TOKEN" && value.expose() == "latest-source")
                );
                *self.seen.lock().unwrap() = true;
                Ok(ProcessOutput {
                    exit_code: 7,
                    stdout: vec![],
                })
            }
        }
        let runner = Runner {
            argv,
            cwd: std::fs::canonicalize(fixture.root.join("nested")).unwrap(),
            seen: Mutex::new(false),
        };
        let request = parse_text(&[
            "deppy",
            "env",
            "run",
            "--workspace",
            &fixture.scope.workspace_id,
            "--environment",
            "development",
            "--recipe",
            &recipe.id,
        ])
        .unwrap()
        .unwrap();
        assert_eq!(run(request, &fixture.service, &runner).unwrap(), 7);
        assert!(*runner.seen.lock().unwrap());
        let receipts = fixture
            .service
            .db
            .project_key_receipt_list(&fixture.scope.workspace_id, fixture.scope.environment, 10)
            .unwrap();
        assert_eq!(receipts[0].exit_code, Some(7));
        assert_eq!(receipts[0].used_keys.len(), 1);
        assert!(receipts[0].finished_at.is_some());
    }
}
