use super::{
    contracts::*,
    repository::ProjectApiRepository,
    service::{ProjectKeysService, storage_error},
};
use deppy_core::project_api_access::{ProjectApiActorKind, ProjectApiSessionScope};
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
    pub key_ids: Vec<String>,
    pub actor: Option<deppy_core::project_api_access::ProjectApiSessionScope>,
}
pub struct EnvList {
    pub scope: ProjectKeysScope,
    pub actor: Option<deppy_core::project_api_access::ProjectApiSessionScope>,
}
pub enum CliAction {
    Gui,
    Print(String),
    Run(EnvRun),
    List(EnvList),
    Mcp(EnvList),
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
    if rest.first() == Some(&OsString::from("env"))
        && matches!(
            rest.get(1).and_then(|value| value.to_str()),
            Some("list" | "mcp")
        )
        && rest.get(2) != Some(&OsString::from("--help"))
    {
        let request = parse_list(rest)?;
        return if rest.get(1) == Some(&OsString::from("mcp")) {
            if request.actor.is_none() {
                Err(ProjectKeysError::InvalidInput)
            } else {
                Ok(CliAction::Mcp(request))
            }
        } else {
            Ok(CliAction::List(request))
        };
    }
    if rest == [OsString::from("env"), OsString::from("--help")]
        || rest
            == [
                OsString::from("env"),
                OsString::from("run"),
                OsString::from("--help"),
            ]
        || rest
            == [
                OsString::from("env"),
                OsString::from("list"),
                OsString::from("--help"),
            ]
        || rest
            == [
                OsString::from("env"),
                OsString::from("mcp"),
                OsString::from("--help"),
            ]
    {
        return Ok(CliAction::Print("Usage: deppy-sijo env list --workspace ID --environment development|production [--session ID --generation ID]\ndeppy-sijo env run --workspace ID --environment development|production [--cwd PATH] [--session ID --generation ID --key ID ...] [--recipe ID | -- COMMAND ARG...]".into()));
    }
    Ok(match parse(arguments)? {
        Some(request) => CliAction::Run(request),
        None => CliAction::Gui,
    })
}
fn local_actor(
    scope: &ProjectKeysScope,
    session: Option<String>,
    generation: Option<String>,
) -> Result<Option<ProjectApiSessionScope>, ProjectKeysError> {
    match (session, generation) {
        (None, None) => Ok(None),
        (Some(session_id), Some(generation)) => {
            let actor = ProjectApiSessionScope {
                workspace_id: scope.workspace_id.clone(),
                environment: scope.environment,
                principal_id: "local-agent".into(),
                session_id,
                generation,
                actor_kind: ProjectApiActorKind::LocalSession,
            };
            actor
                .validate()
                .map_err(|_| ProjectKeysError::InvalidInput)?;
            Ok(Some(actor))
        }
        _ => Err(ProjectKeysError::InvalidInput),
    }
}
fn parse_environment(value: &str) -> Result<KeyEnvironment, ProjectKeysError> {
    match value {
        "development" | "dev" => Ok(KeyEnvironment::Development),
        "production" | "prod" => Ok(KeyEnvironment::Production),
        _ => Err(ProjectKeysError::InvalidInput),
    }
}
fn parse_list(args: &[OsString]) -> Result<EnvList, ProjectKeysError> {
    let args = args
        .iter()
        .map(|value| value.to_str().ok_or(ProjectKeysError::InvalidInput))
        .collect::<Result<Vec<_>, _>>()?;
    let (mut workspace, mut environment, mut session, mut generation) = (None, None, None, None);
    let mut index = 2;
    while index < args.len() {
        if args[index] == "--json" {
            index += 1;
            continue;
        }
        let value = *args.get(index + 1).ok_or(ProjectKeysError::InvalidInput)?;
        match args[index] {
            "--workspace" if workspace.is_none() => workspace = Some(value.to_owned()),
            "--environment" if environment.is_none() => {
                environment = Some(parse_environment(value)?)
            }
            "--session" if session.is_none() => session = Some(value.to_owned()),
            "--generation" if generation.is_none() => generation = Some(value.to_owned()),
            _ => return Err(ProjectKeysError::InvalidInput),
        }
        index += 2;
    }
    let scope = ProjectKeysScope {
        workspace_id: workspace
            .filter(|id| deppy_core::project_keys::valid_id(id))
            .ok_or(ProjectKeysError::InvalidInput)?,
        environment: environment.ok_or(ProjectKeysError::InvalidInput)?,
    };
    let actor = local_actor(&scope, session, generation)?;
    Ok(EnvList { scope, actor })
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
    let (mut session, mut generation) = (None, None);
    let mut key_ids = Vec::new();
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
                environment = Some(parse_environment(value)?)
            }
            "--recipe" if recipe_id.is_none() => recipe_id = Some(value.clone()),
            "--cwd" if cwd.is_none() => cwd = Some(PathBuf::from(value)),
            "--session" if session.is_none() => session = Some(value.clone()),
            "--generation" if generation.is_none() => generation = Some(value.clone()),
            "--key"
                if deppy_core::project_keys::valid_id(value)
                    && !key_ids.contains(value)
                    && key_ids.len() < deppy_core::project_keys::PROJECT_KEYS_MAX =>
            {
                key_ids.push(value.clone())
            }
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
    let scope = ProjectKeysScope {
        workspace_id,
        environment: environment.ok_or(ProjectKeysError::InvalidInput)?,
    };
    let actor = local_actor(&scope, session, generation)?;
    if key_ids.is_empty() == actor.is_some()
        || recipe_id
            .as_ref()
            .is_some_and(|id| !deppy_core::project_keys::valid_id(id))
    {
        return Err(ProjectKeysError::InvalidInput);
    }
    Ok(Some(EnvRun {
        scope,
        recipe_id,
        command,
        cwd,
        key_ids,
        actor,
    }))
}
pub fn list(request: EnvList, db: &dyn ProjectApiRepository) -> Result<String, ProjectKeysError> {
    if let Some(actor) = &request.actor {
        authorize_inherited(db, actor)?;
    }
    let catalog = super::catalog::project_api_catalog_from_db(
        db,
        &request.scope,
        request.actor.as_ref(),
        super::service::now(),
    )?;
    serde_json::to_string(&catalog).map_err(|_| ProjectKeysError::Storage)
}
pub fn serve(
    request: EnvList,
    db: &dyn ProjectApiRepository,
    input: impl std::io::BufRead,
    output: impl std::io::Write,
) -> Result<(), ProjectKeysError> {
    let actor = request.actor.clone().ok_or(ProjectKeysError::Locked)?;
    serve_checked(request, db, input, output, || {
        authorize_inherited(db, &actor)
    })
}
fn authorize_inherited(
    db: &dyn ProjectApiRepository,
    actor: &ProjectApiSessionScope,
) -> Result<(), ProjectKeysError> {
    let bearer = secret::SecretString::new(
        std::env::var(super::session_capability::CAPABILITY_ENV)
            .map_err(|_| ProjectKeysError::Locked)?,
    );
    let digest = super::session_capability::capability_digest(bearer.expose())?;
    db.project_api_local_grants_refresh(actor, &digest, super::service::now())
        .map_err(|_| ProjectKeysError::Locked)
}
pub fn authorize_selected_request(
    request: &EnvRun,
    db: &dyn ProjectApiRepository,
) -> Result<(), ProjectKeysError> {
    if let Some(actor) = &request.actor {
        authorize_inherited(db, actor)?;
    }
    Ok(())
}
fn serve_checked(
    request: EnvList,
    db: &dyn ProjectApiRepository,
    mut input: impl std::io::BufRead,
    mut output: impl std::io::Write,
    mut authorize: impl FnMut() -> Result<(), ProjectKeysError>,
) -> Result<(), ProjectKeysError> {
    use serde_json::{Value, json};
    let actor = request.actor.as_ref().ok_or(ProjectKeysError::Locked)?;
    if actor.workspace_id != request.scope.workspace_id
        || actor.environment != request.scope.environment
    {
        return Err(ProjectKeysError::InvalidInput);
    }
    let executable = std::env::current_exe().map_err(|_| ProjectKeysError::Process)?;
    let instructions = super::agent_access::local_api_instructions(actor, &executable)?;
    loop {
        let mut line = Vec::new();
        let mut bounded = std::io::Read::take(&mut input, 65537);
        let size = std::io::BufRead::read_until(&mut bounded, b'\n', &mut line)
            .map_err(|_| ProjectKeysError::Process)?;
        if size == 0 {
            return Ok(());
        }
        if size > 65536 {
            return Err(ProjectKeysError::InvalidInput);
        }
        authorize()?;
        let message: Value =
            serde_json::from_slice(&line).map_err(|_| ProjectKeysError::InvalidInput)?;
        if message["jsonrpc"] != "2.0" || !message.is_object() {
            return Err(ProjectKeysError::InvalidInput);
        }
        let Some(id) = message.get("id") else {
            continue;
        };
        let result = match message["method"].as_str() {
            Some("initialize") => {
                let version = message["params"]["protocolVersion"]
                    .as_str()
                    .filter(|version| {
                        matches!(
                            *version,
                            "2024-11-05" | "2025-03-26" | "2025-06-18" | "2025-11-25"
                        )
                    })
                    .unwrap_or("2024-11-05");
                Ok(
                    json!({"protocolVersion":version,"capabilities":{"tools":{}},"serverInfo":{"name":"deppy-project-apis","version":env!("CARGO_PKG_VERSION")},"instructions":instructions}),
                )
            }
            Some("ping") => Ok(json!({})),
            Some("tools/list") => Ok(
                json!({"tools":[{"name":"list_project_apis","description":"Discover the current session's registered project API metadata and availability, without secret values. Use the provided env run command with an explicit key subset.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true}}]}),
            ),
            Some("tools/call")
                if message["params"]["name"] == "list_project_apis"
                    && message["params"].get("arguments").is_none_or(|arguments| {
                        arguments.as_object().is_some_and(serde_json::Map::is_empty)
                    }) =>
            {
                let catalog = super::catalog::project_api_catalog_from_db(
                    db,
                    &request.scope,
                    Some(actor),
                    super::service::now(),
                )?;
                Ok(
                    json!({"content":[{"type":"text","text":serde_json::to_string(&catalog).map_err(|_|ProjectKeysError::Storage)?}],"isError":false}),
                )
            }
            Some("tools/call") => Err((-32602, "invalid_arguments")),
            _ => Err((-32601, "method_not_found")),
        };
        let reply = match result {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err((code, message)) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
            }
        };
        serde_json::to_writer(&mut output, &reply).map_err(|_| ProjectKeysError::Process)?;
        output
            .write_all(b"\n")
            .and_then(|_| output.flush())
            .map_err(|_| ProjectKeysError::Process)?;
    }
}
pub fn run(
    request: EnvRun,
    service: &ProjectKeysService,
    runner: &dyn super::process::ProcessRunner,
) -> Result<i32, ProjectKeysError> {
    authorize_selected_request(&request, &service.db)?;
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
    let receipt = if let Some(actor) = &request.actor {
        service.run_agent_command(
            actor,
            &request.key_ids,
            request.recipe_id.as_deref(),
            command,
            Path::new(&cwd),
            runner,
        )?
    } else {
        service.run_build(
            &request.scope,
            recipe.as_ref(),
            command,
            Path::new(&cwd),
            runner,
        )?
    };
    Ok(receipt.exit_code.unwrap_or(1))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn parse_text(args: &[&str]) -> Result<Option<EnvRun>, ProjectKeysError> {
        parse(args.iter().map(OsString::from))
    }
    #[test]
    fn project_api_cli_list_is_value_free_and_requires_explicit_scope() {
        use crate::project_keys::tests::Fixture;
        let action = dispatch(
            [
                "deppy",
                "env",
                "list",
                "--workspace",
                "workspace",
                "--environment",
                "prod",
            ]
            .map(OsString::from),
        )
        .unwrap();
        assert!(
            matches!(action,CliAction::List(request) if request.scope.workspace_id=="workspace" && request.scope.environment==KeyEnvironment::Production)
        );
        assert!(
            dispatch(["deppy", "env", "list", "--workspace", "workspace"].map(OsString::from))
                .is_err()
        );
        let fixture = Fixture::new();
        let calls = fixture.store.2.load(std::sync::atomic::Ordering::Acquire);
        let json = list(
            EnvList {
                scope: fixture.scope.clone(),
                actor: None,
            },
            &fixture.service.db,
        )
        .unwrap();
        assert!(json.contains("entries"));
        assert_eq!(
            fixture.store.2.load(std::sync::atomic::Ordering::Acquire),
            calls
        );
        for forbidden in ["credential_id", "source_baseline", "digest"] {
            assert!(!json.contains(forbidden));
        }
    }
    #[test]
    fn project_api_cli_explicit_keys_require_current_session_and_unique_subset() {
        let args = [
            "deppy",
            "env",
            "run",
            "--workspace",
            "workspace",
            "--environment",
            "dev",
            "--session",
            "session",
            "--generation",
            "generation",
            "--key",
            "key",
            "--",
            "fixture",
        ];
        let request = parse_text(&args).unwrap().unwrap();
        assert_eq!(request.key_ids, ["key"]);
        assert_eq!(request.actor.unwrap().session_id, "session");
        assert!(
            parse_text(&[
                "deppy",
                "env",
                "run",
                "--workspace",
                "workspace",
                "--environment",
                "dev",
                "--key",
                "key",
                "--",
                "fixture"
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
                "--session",
                "session",
                "--generation",
                "generation",
                "--key",
                "key",
                "--key",
                "key",
                "--",
                "fixture"
            ])
            .is_err()
        );
    }
    #[test]
    fn project_api_mcp_bootstrap_has_real_initialize_tool_discovery_and_value_free_result() {
        use crate::project_keys::tests::Fixture;
        let action = dispatch(
            [
                "deppy",
                "env",
                "mcp",
                "--workspace",
                "workspace",
                "--environment",
                "dev",
                "--session",
                "session",
                "--generation",
                "generation",
            ]
            .map(OsString::from),
        )
        .unwrap();
        assert!(matches!(action, CliAction::Mcp(_)));
        let fixture = Fixture::new();
        let actor = ProjectApiSessionScope {
            workspace_id: fixture.scope.workspace_id.clone(),
            environment: fixture.scope.environment,
            principal_id: "local-agent".into(),
            session_id: "session".into(),
            generation: "generation".into(),
            actor_kind: ProjectApiActorKind::LocalSession,
        };
        let input=b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"list_project_apis\",\"arguments\":{}}}\n";
        let mut output = Vec::new();
        let mut authorizations = 0;
        serve_checked(
            EnvList {
                scope: fixture.scope.clone(),
                actor: Some(actor),
            },
            &fixture.service.db,
            &input[..],
            &mut output,
            || {
                authorizations += 1;
                Ok(())
            },
        )
        .unwrap();
        assert!(authorizations >= 3);
        let replies = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(replies.len(), 3);
        assert!(
            replies[0]["result"]["instructions"]
                .as_str()
                .unwrap()
                .contains("env run")
        );
        assert_eq!(
            replies[1]["result"]["tools"][0]["name"],
            "list_project_apis"
        );
        assert!(
            replies[2]["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("entries")
        );
        let mut denied = Vec::new();
        assert!(
            serve_checked(
                EnvList {
                    scope: fixture.scope.clone(),
                    actor: None
                },
                &fixture.service.db,
                &input[..],
                &mut denied,
                || Err(ProjectKeysError::Locked)
            )
            .is_err()
        );
        assert!(denied.is_empty());
    }
    #[test]
    fn project_api_mcp_no_input_tool_accepts_omitted_arguments_only_as_empty_object() {
        use crate::project_keys::tests::Fixture;
        let fixture = Fixture::new();
        let actor = ProjectApiSessionScope {
            workspace_id: fixture.scope.workspace_id.clone(),
            environment: fixture.scope.environment,
            principal_id: "local-agent".into(),
            session_id: "session".into(),
            generation: "generation".into(),
            actor_kind: ProjectApiActorKind::LocalSession,
        };
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"list_project_apis\",\"arguments\":{}}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"list_project_apis\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"list_project_apis\",\"arguments\":null}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"list_project_apis\",\"arguments\":{\"workspace_id\":\"other\"}}}\n",
        );
        let mut output = Vec::new();
        serve_checked(
            EnvList {
                scope: fixture.scope.clone(),
                actor: Some(actor),
            },
            &fixture.service.db,
            input.as_bytes(),
            &mut output,
            || Ok(()),
        )
        .unwrap();
        let replies = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(replies.len(), 4);
        assert_eq!(replies[0]["result"]["isError"], false);
        assert_eq!(replies[1]["result"], replies[0]["result"]);
        for reply in &replies[2..] {
            assert_eq!(reply["error"]["code"], -32602);
            assert!(reply.get("result").is_none());
        }
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
