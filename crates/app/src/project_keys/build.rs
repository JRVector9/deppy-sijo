use super::{
    contracts::*,
    process::{ProcessEnvironment, ProcessRunner},
    service::{ProjectKeysService, now, storage_error},
};
use deppy_core::project_keys::*;
use secret::SecretString;
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};

struct TemporaryKeys {
    directory: PathBuf,
    files: Vec<PathBuf>,
}

impl TemporaryKeys {
    fn create() -> Result<Self, ProjectKeysError> {
        let directory =
            std::env::temp_dir().join(format!("deppy-build-keys-{}", uuid::Uuid::new_v4()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&directory)
            .map_err(|_| ProjectKeysError::Process)?;
        Ok(Self {
            directory,
            files: vec![],
        })
    }
    fn write(&mut self, value: &SecretString) -> Result<String, ProjectKeysError> {
        let path = self.directory.join(format!("{}.pem", uuid::Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&path).map_err(|_| ProjectKeysError::Process)?;
        self.files.push(path.clone());
        file.write_all(value.expose().as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|_| ProjectKeysError::Process)?;
        Ok(path.to_string_lossy().into())
    }
}
impl Drop for TemporaryKeys {
    fn drop(&mut self) {
        for path in &self.files {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir(&self.directory);
    }
}

impl ProjectKeysService {
    pub fn run_build(
        &self,
        scope: &ProjectKeysScope,
        recipe: Option<&ProjectBuildRecipe>,
        command: &[String],
        cwd: &Path,
        runner: &dyn ProcessRunner,
    ) -> Result<ProjectBuildReceipt, ProjectKeysError> {
        self.run_build_selected(scope, recipe, command, cwd, runner, None)
    }

    pub(super) fn run_build_selected(
        &self,
        scope: &ProjectKeysScope,
        recipe: Option<&ProjectBuildRecipe>,
        command: &[String],
        cwd: &Path,
        runner: &dyn ProcessRunner,
        selected: Option<(
            &deppy_core::project_api_access::ProjectApiSessionScope,
            &[String],
        )>,
    ) -> Result<ProjectBuildReceipt, ProjectKeysError> {
        if command.is_empty()
            || command.len() > 128
            || command.iter().map(String::len).sum::<usize>() > 8192
            || command.iter().any(|arg| arg.contains('\0'))
        {
            return Err(ProjectKeysError::InvalidInput);
        }
        let root = self.root_handle(scope)?;
        let cwd = root.open_directory(cwd)?;
        let settings = storage_error(
            self.db
                .project_key_settings(&scope.workspace_id, scope.environment),
        )?;
        // Preflight performs the same bounded fresh-original scan as explicit Retry.
        self.refresh_scope(scope)?;
        let keys = storage_error(
            self.db
                .project_key_list(&scope.workspace_id, scope.environment),
        )?;
        let mut effective: BTreeMap<String, ProjectKey> = BTreeMap::new();
        let mut disconnected_required = Vec::new();
        for mut key in keys.into_iter().filter(|key| {
            selected.map_or_else(
                || matches!(key.usage, KeyUsage::Build | KeyUsage::LegacyAgent),
                |(_, ids)| ids.contains(&key.id),
            )
        }) {
            if selected.is_some() {
                key.required = true;
            }
            if key.source.is_file() {
                let Some(index) = settings
                    .sources
                    .iter()
                    .position(|source| *source == key.source)
                else {
                    if key.required {
                        disconnected_required.push(key.name.clone());
                    }
                    continue;
                };
                key.source_priority = index as i32 + 1;
            }
            if let Some(previous) = effective.get(&key.name) {
                if previous.source_priority == key.source_priority {
                    return Err(ProjectKeysError::Conflict);
                }
                if previous.source_priority > key.source_priority {
                    continue;
                }
            }
            effective.insert(key.name.clone(), key);
        }
        if disconnected_required
            .iter()
            .any(|name| !effective.contains_key(name))
        {
            return Err(ProjectKeysError::SourceMissing);
        }
        if selected.is_some_and(|(_, ids)| effective.len() != ids.len()) {
            return Err(ProjectKeysError::Conflict);
        }
        let mut temporary = TemporaryKeys::create()?;
        let mut environment = ProcessEnvironment::default();
        environment
            .remove_names
            .push("DEPPY_PROJECT_API_CAPABILITY".into());
        let mut used_keys = Vec::new();
        // Remove every registered name in either environment before injecting this snapshot.
        // This also removes optional/missing values from an old shell's ambient environment.
        for environment_kind in [KeyEnvironment::Development, KeyEnvironment::Production] {
            for key in storage_error(
                self.db
                    .project_key_list(&scope.workspace_id, environment_kind),
            )? {
                if !environment.remove_names.contains(&key.name) {
                    environment.remove_names.push(key.name);
                }
            }
        }
        for key in effective.into_values() {
            if matches!(key.source, KeySource::GitHubOnly) {
                if key.required {
                    return Err(ProjectKeysError::Locked);
                }
                continue;
            }
            if key.storage_state == KeyStorageState::Conflict {
                return Err(ProjectKeysError::Conflict);
            }
            if matches!(
                key.storage_state,
                KeyStorageState::MissingSource
                    | KeyStorageState::EmptySource
                    | KeyStorageState::Locked
                    | KeyStorageState::Failed
                    | KeyStorageState::UpdateNeeded
                    | KeyStorageState::NotStored
            ) {
                if key.required {
                    return Err(ProjectKeysError::Locked);
                }
                continue;
            }
            let bundle = match self.bundle(scope, &key) {
                Ok(bundle) => bundle,
                Err(
                    ProjectKeysError::Locked
                    | ProjectKeysError::SourceMissing
                    | ProjectKeysError::SourceEmpty,
                ) if !key.required => continue,
                Err(error) => return Err(error),
            };
            if bundle.access().expose().is_empty() {
                if key.required {
                    return Err(ProjectKeysError::Locked);
                }
                continue;
            }
            if selected.is_some() {
                environment
                    .output_secrets
                    .push(SecretString::new(bundle.access().expose().into()));
            }
            let value = if matches!(
                key.delivery,
                KeyDelivery::FilePath | KeyDelivery::TemporaryFile
            ) {
                SecretString::new(temporary.write(bundle.access())?)
            } else {
                SecretString::new(bundle.access().expose().into())
            };
            environment.values.push((key.name.clone(), value));
            used_keys.push(ProjectKeyUsedVersion {
                key_id: key.id,
                name: key.name,
                revision: key.last_good_revision.ok_or(ProjectKeysError::Locked)?,
            });
        }
        let mut receipt = ProjectBuildReceipt {
            owner: deppy_core::process_identity::ProcessIdentity::capture(std::process::id()),
            id: uuid::Uuid::new_v4().to_string(),
            workspace_id: scope.workspace_id.clone(),
            environment: scope.environment,
            recipe_id: recipe.map(|recipe| recipe.id.clone()),
            started_at: now(),
            finished_at: None,
            exit_code: None,
            used_keys,
        };
        // Receipt preflight rejects a rotation that happened while gathering the immutable values.
        storage_error(self.db.project_key_receipt_save(&receipt))?;
        // Every failure after publishing a running receipt must settle that receipt.
        let result = self
            .root_handle(scope)
            .and_then(|root| {
                if let Some((actor, ids)) = selected {
                    self.check_agent_grants(actor, ids, recipe.map(|recipe| recipe.id.as_str()))?;
                }
                Ok(root)
            })
            .and_then(|_| runner.run_in_directory(command, &cwd, &environment, None, true));
        // PEM files are needed only by the child; a busy receipt database must not retain them.
        drop(temporary);
        receipt.finished_at = Some(now());
        receipt.exit_code = Some(result.as_ref().map(|output| output.exit_code).unwrap_or(1));
        storage_error(self.db.project_key_receipt_save(&receipt))?;
        result.map(|_| receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_keys::{process::ProcessOutput, tests::Fixture};
    use std::sync::{Mutex, atomic::Ordering};

    fn selected_actor(
        fixture: &Fixture,
        key: &ProjectKey,
    ) -> deppy_core::project_api_access::ProjectApiSessionScope {
        use deppy_core::project_api_access::*;
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
            session_id: "output-session".into(),
            generation: "output-generation".into(),
            actor_kind: ProjectApiActorKind::LocalSession,
        };
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
                    expires_at: now() + 60,
                },
                now(),
            )
            .unwrap();
        actor
    }

    // This helper runs in an isolated real PTY so its stdout and stderr cross the same
    // boundary that an already-running agent sees, with no launch-time secret corpus.
    #[cfg(unix)]
    #[test]
    fn project_api_selected_output_pty_helper() {
        let mode = std::env::var("DEPPY_SELECTED_OUTPUT_FIXTURE").unwrap_or_default();
        if !matches!(
            mode.as_str(),
            "1" | "blocked" | "tostop" | "prompt" | "cross"
        ) {
            return;
        }
        let fixture = Fixture::new();
        let mut key = fixture.key("SELECTED_TOKEN", KeySource::Vault);
        key.usage = KeyUsage::StoreOnly;
        key.required = false;
        let key = fixture.save(key, "fresh-selected-fixture-token");
        let actor = selected_actor(&fixture, &key);
        if mode == "cross" {
            fixture.service.run_agent_command(
                &actor,
                std::slice::from_ref(&key.id),
                None,
                &["/bin/sh".into(), "-c".into(),
                    "printf 'cross='; printf '%.8s' \"$SELECTED_TOKEN\"; sleep 0.04; printf '%s\\n' \"${SELECTED_TOKEN#????????}\" >&2".into()],
                Path::new("."),
                &crate::project_keys::process::NativeProcessRunner::for_cli(),
            ).unwrap();
            println!("selected-output-complete");
            return;
        }
        if mode == "prompt" {
            let runner = crate::project_keys::process::NativeProcessRunner::for_cli();
            let receipt = fixture.service.run_agent_command(
                &actor,
                std::slice::from_ref(&key.id),
                None,
                &["/bin/sh".into(), "-c".into(), concat!(
                    "printf '\\033['; sleep 0.04; printf '31mtoken-stdout-prompt?\\033[0m'; ",
                    "read first; test \"$first\" = fixture-answer || exit 1; ",
                    "printf '\\342' >&2; sleep 0.04; printf '\\234\\223 token-stderr-prompt?' >&2; ",
                    "read second; test \"$second\" = fixture-answer || exit 1; ",
                    "printf 'token-output='; printf '%.8s' \"$SELECTED_TOKEN\"; printf '\\033['; sleep 0.04; ",
                    "printf '31m%s\\033[0m\\n' \"${SELECTED_TOKEN#????????}\"; ",
                    "printf 'token-output=%s\\n' \"$SELECTED_TOKEN\" >&2",
                ).into()],
                Path::new("."),
                &runner,
            ).unwrap();
            assert_eq!(receipt.exit_code, Some(0));
            let mut pem = fixture.key("SELECTED_PEM", KeySource::Vault);
            pem.usage = KeyUsage::StoreOnly;
            pem.required = false;
            pem.kind = KeyKind::PrivateKey;
            pem.delivery = KeyDelivery::TemporaryFile;
            let pem = fixture.save(pem, "private-selected-fixture-pem-value");
            let actor = selected_actor(&fixture, &pem);
            let receipt = fixture.service.run_agent_command(
                &actor,
                std::slice::from_ref(&pem.id),
                None,
                &["/bin/sh".into(), "-c".into(), concat!(
                    "printf 'pem-stdout-prompt?'; read first; test \"$first\" = fixture-answer || exit 1; ",
                    "printf 'pem-stderr-prompt?' >&2; read second; test \"$second\" = fixture-answer || exit 1; ",
                    "printf 'pem-output='; cat \"$SELECTED_PEM\"; printf '\\n'; ",
                    "printf 'pem-output=' >&2; cat \"$SELECTED_PEM\" >&2; printf '\\n' >&2",
                ).into()],
                Path::new("."),
                &runner,
            ).unwrap();
            assert_eq!(receipt.exit_code, Some(0));
            println!("selected-output-complete");
            return;
        }
        if mode == "tostop" {
            // This subprocess owns the isolated PTY. Ensure the regression does not depend
            // on a launching shell's ignored/blocked job-control signal state.
            let mut signals = unsafe { std::mem::zeroed::<libc::sigset_t>() };
            unsafe {
                libc::sigemptyset(&mut signals);
                libc::sigaddset(&mut signals, libc::SIGTTOU);
                libc::signal(libc::SIGTTOU, libc::SIG_DFL);
            }
            assert_eq!(
                unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, &signals, std::ptr::null_mut()) },
                0
            );
            let mut attributes = unsafe { std::mem::zeroed::<libc::termios>() };
            assert_eq!(
                unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut attributes) },
                0
            );
            attributes.c_lflag |= libc::TOSTOP;
            assert_eq!(
                unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &attributes) },
                0
            );
        }
        if mode == "blocked" {
            use std::{
                sync::Arc,
                time::{Duration, Instant},
            };
            // Fill the isolated helper's stdout pipe before Native starts, then restore its
            // original flags. The parent deliberately does not read until the helper exits.
            let flags = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_GETFL) };
            assert!(flags >= 0);
            assert_eq!(
                unsafe {
                    libc::fcntl(libc::STDOUT_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK)
                },
                0
            );
            let padding = [b'x'; 4096];
            loop {
                let count = unsafe {
                    libc::write(libc::STDOUT_FILENO, padding.as_ptr().cast(), padding.len())
                };
                if count < 0 {
                    assert_eq!(
                        std::io::Error::last_os_error().kind(),
                        std::io::ErrorKind::WouldBlock
                    );
                    break;
                }
                assert!(count > 0);
            }
            assert_eq!(
                unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_SETFL, flags) },
                0
            );
            let runner = Arc::new(crate::project_keys::process::NativeProcessRunner::for_cli());
            let cancel = runner.clone();
            let timer = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                cancel.cancel();
            });
            let started = Instant::now();
            let result = fixture.service.run_agent_command(
                &actor,
                std::slice::from_ref(&key.id),
                None,
                &["/bin/sh".into(), "-c".into(),
                    "while :; do printf 'blocked-stdout=%s\\n' \"$SELECTED_TOKEN\"; printf 'blocked-stderr=%s\\n' \"$SELECTED_TOKEN\" >&2; done".into()],
                Path::new("."),
                runner.as_ref(),
            );
            timer.join().unwrap();
            assert!(matches!(result, Err(ProjectKeysError::Process)));
            assert!(started.elapsed() < Duration::from_secs(1));
            assert!(runner.wait_for_idle(Duration::from_millis(100)));
            assert_eq!(
                unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_GETFL) },
                flags
            );
            drop(fixture);
            // libtest's completion banner would itself block on the deliberately full pipe.
            std::process::exit(0);
        }
        let runner = crate::project_keys::process::NativeProcessRunner::for_cli();
        let command = [
            "/bin/sh".into(),
            "-c".into(),
            concat!(
                "printf 'stdout='; printf '%.8s' \"$SELECTED_TOKEN\"; sleep 0.04; ",
                "printf '%s\\n' \"${SELECTED_TOKEN#????????}\"; ",
                "printf 'stderr=' >&2; printf '%.8s' \"$SELECTED_TOKEN\" >&2; sleep 0.04; ",
                "printf '%s\\n' \"${SELECTED_TOKEN#????????}\" >&2",
            )
            .into(),
        ];
        let first = fixture
            .service
            .run_agent_command(
                &actor,
                std::slice::from_ref(&key.id),
                None,
                &command,
                Path::new("."),
                &runner,
            )
            .unwrap();
        assert_eq!(first.exit_code, Some(0));
        let rotated = fixture
            .service
            .save_key(
                &fixture.scope,
                key.clone(),
                Some(key.revision),
                ValueEdit::Replace(
                    SensitiveDraft::new("rotated-selected-fixture-token".into()).unwrap(),
                ),
            )
            .unwrap();
        let second = fixture
            .service
            .run_agent_command(
                &actor,
                std::slice::from_ref(&key.id),
                None,
                &command,
                Path::new("."),
                &runner,
            )
            .unwrap();
        assert_eq!(second.exit_code, Some(0));
        assert_eq!(
            second.used_keys[0].revision,
            rotated.last_good_revision.unwrap()
        );

        let mut pem = fixture.key("SELECTED_PEM", KeySource::Vault);
        pem.usage = KeyUsage::StoreOnly;
        pem.required = false;
        pem.kind = KeyKind::PrivateKey;
        pem.delivery = KeyDelivery::TemporaryFile;
        let pem = fixture.save(pem, "private-selected-fixture-pem-value");
        let pem_actor = selected_actor(&fixture, &pem);
        let receipt = fixture
            .service
            .run_agent_command(
                &pem_actor,
                std::slice::from_ref(&pem.id),
                None,
                &[
                    "/bin/sh".into(),
                    "-c".into(),
                    concat!(
                        "printf 'pem-stdout='; head -c 8 \"$SELECTED_PEM\"; sleep 0.04; ",
                        "tail -c +9 \"$SELECTED_PEM\"; printf '\\n'; ",
                        "printf 'pem-stderr=' >&2; head -c 8 \"$SELECTED_PEM\" >&2; sleep 0.04; ",
                        "tail -c +9 \"$SELECTED_PEM\" >&2; printf '\\n' >&2",
                    )
                    .into(),
                ],
                Path::new("."),
                &runner,
            )
            .unwrap();
        assert_eq!(receipt.exit_code, Some(0));
        if mode == "tostop" {
            let mut signals = unsafe { std::mem::zeroed::<libc::sigset_t>() };
            assert_eq!(
                unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut signals) },
                0
            );
            assert_eq!(unsafe { libc::sigismember(&signals, libc::SIGTTOU) }, 0);
            let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
            assert_eq!(
                unsafe { libc::sigaction(libc::SIGTTOU, std::ptr::null(), &mut action) },
                0
            );
            assert_eq!(action.sa_sigaction, libc::SIG_DFL);
        }
        println!("selected-output-complete");
    }

    #[cfg(unix)]
    #[test]
    fn project_api_selected_native_output_cancels_with_blocked_consumer() {
        use std::{
            process::{Command, Stdio},
            time::{Duration, Instant},
        };
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "project_keys::build::tests::project_api_selected_output_pty_helper",
                "--exact",
                "--nocapture",
            ])
            .env("DEPPY_SELECTED_OUTPUT_FIXTURE", "blocked")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("selected output cancellation blocked behind its consumer");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        // Only drain after exit: this test must not accidentally relieve the backpressure.
        let output = child.wait_with_output().unwrap();
        assert!(
            status.success(),
            "blocked-consumer helper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    fn project_api_selected_native_output_is_redacted_before_pty_and_session_logs() {
        check_selected_output_pty("1");
    }

    #[cfg(unix)]
    #[test]
    fn project_api_selected_native_output_survives_tostop_terminal() {
        check_selected_output_pty("tostop");
    }

    #[cfg(unix)]
    #[test]
    fn project_api_selected_native_output_keeps_interactive_prompts_visible() {
        check_selected_output_pty("prompt");
    }

    #[cfg(unix)]
    #[test]
    fn project_api_selected_native_output_redacts_shared_stdout_stderr() {
        check_selected_output_pty("cross");
    }

    #[cfg(unix)]
    #[test]
    fn project_api_selected_native_output_redacts_shared_regular_file() {
        use std::process::{Command, Stdio};
        let fixture = Fixture::new();
        let path = fixture.root.join("shared-output.log");
        let output = std::fs::File::create(&path).unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "project_keys::build::tests::project_api_selected_output_pty_helper",
                "--exact",
                "--nocapture",
            ])
            .env("DEPPY_SELECTED_OUTPUT_FIXTURE", "cross")
            .stdin(Stdio::null())
            .stdout(Stdio::from(output.try_clone().unwrap()))
            .stderr(Stdio::from(output))
            .status()
            .unwrap();
        let output = std::fs::read_to_string(path).unwrap();
        assert!(status.success(), "shared-file helper failed: {output}");
        assert!(!output.contains("fresh-selected-fixture-token"));
        assert_eq!(output.matches("cross=[REDACTED]").count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn project_api_selected_native_output_preserves_distinct_destinations() {
        use std::process::{Command, Stdio};
        let fixture = Fixture::new();
        let stdout_path = fixture.root.join("stdout.log");
        let stderr_path = fixture.root.join("stderr.log");
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "project_keys::build::tests::project_api_selected_output_pty_helper",
                "--exact",
                "--nocapture",
            ])
            .env("DEPPY_SELECTED_OUTPUT_FIXTURE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::from(std::fs::File::create(&stdout_path).unwrap()))
            .stderr(Stdio::from(std::fs::File::create(&stderr_path).unwrap()))
            .status()
            .unwrap();
        let stdout = std::fs::read_to_string(stdout_path).unwrap();
        let stderr = std::fs::read_to_string(stderr_path).unwrap();
        assert!(
            status.success(),
            "distinct-output helper failed: {stdout} {stderr}"
        );
        assert_eq!(stdout.matches("stdout=[REDACTED]").count(), 3);
        assert!(!stdout.contains("stderr="));
        assert_eq!(stderr.matches("stderr=[REDACTED]").count(), 3);
        assert!(!stderr.contains("stdout="));
        for value in [
            "fresh-selected-fixture-token",
            "rotated-selected-fixture-token",
            "private-selected-fixture-pem-value",
        ] {
            assert!(!stdout.contains(value));
            assert!(!stderr.contains(value));
        }
    }

    #[cfg(unix)]
    fn check_selected_output_pty(mode: &str) {
        use std::{
            io::Read,
            os::{fd::FromRawFd, unix::process::CommandExt},
            process::{Command, Stdio},
            time::{Duration, Instant},
        };
        let mut master_fd = -1;
        let mut slave_fd = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let mut master = unsafe { std::fs::File::from_raw_fd(master_fd) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave_fd) };
        let flags = unsafe { libc::fcntl(master_fd, libc::F_GETFL) };
        assert_eq!(
            unsafe { libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "project_keys::build::tests::project_api_selected_output_pty_helper",
                "--exact",
                "--nocapture",
            ])
            .env("DEPPY_SELECTED_OUTPUT_FIXTURE", mode)
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0
                    || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as libc::c_ulong, 0) < 0
                    || libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp()) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let fixture = Fixture::new();
        let mut log =
            storage::SessionLogWriter::open_key(&fixture.root, "selected-output").unwrap();
        // Deliberately empty: the long-running session never resolved these fresh values.
        let mut parent_redactor = secret::RedactionService::new().stream_redactor();
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut output = Vec::new();
        let mut status = None;
        let prompts = [
            "token-stdout-prompt?",
            "token-stderr-prompt?",
            "pem-stdout-prompt?",
            "pem-stderr-prompt?",
        ];
        let mut answered = 0;
        loop {
            let mut bytes = [0u8; 1024];
            match master.read(&mut bytes) {
                Ok(count) if count > 0 => {
                    output.extend_from_slice(&bytes[..count]);
                    log.append_output(&parent_redactor.redact_chunk(&bytes[..count]))
                        .unwrap();
                    if mode == "prompt"
                        && answered < prompts.len()
                        && String::from_utf8_lossy(&output).contains(prompts[answered])
                    {
                        master.write_all(b"fixture-answer\n").unwrap();
                        answered += 1;
                    }
                    continue;
                }
                Ok(_) => break,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if status.is_some() {
                        break;
                    }
                }
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(error) => panic!("PTY read failed: {error}"),
            }
            if status.is_none() {
                status = child.try_wait().unwrap();
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("selected output helper timed out");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let status = status.unwrap_or_else(|| child.wait().unwrap());
        assert!(
            status.success(),
            "selected output helper failed ({status}): {}",
            String::from_utf8_lossy(&output)
        );
        log.append_output(&parent_redactor.flush()).unwrap();
        log.flush();
        let terminal = String::from_utf8(output).unwrap();
        assert!(terminal.contains("selected-output-complete"));
        let ansi = std::fs::read_to_string(
            storage::SessionLogWriter::ansi_path(&fixture.root, "selected-output").unwrap(),
        )
        .unwrap();
        let plain =
            std::fs::read_to_string(fixture.root.join("selected-output/redacted.plain.txt"))
                .unwrap();
        for text in [&terminal, &ansi, &plain] {
            for value in [
                "fresh-selected-fixture-token",
                "rotated-selected-fixture-token",
                "private-selected-fixture-pem-value",
            ] {
                assert!(
                    !text.contains(value),
                    "selected secret crossed output boundary"
                );
            }
            if mode == "cross" {
                assert_eq!(text.matches("cross=[REDACTED]").count(), 1);
                assert_eq!(text.matches("[REDACTED]").count(), 1);
            } else if mode == "prompt" {
                assert_eq!(answered, prompts.len());
                assert_eq!(text.matches("[REDACTED]").count(), 4);
                for prompt in prompts {
                    assert_eq!(text.matches(prompt).count(), 1);
                }
            } else {
                assert_eq!(text.matches("[REDACTED]").count(), 6);
                assert_eq!(text.matches("stdout=[REDACTED]").count(), 3);
                assert_eq!(text.matches("stderr=[REDACTED]").count(), 3);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn project_api_selected_redaction_capacity_rejects_before_native_spawn() {
        let fixture = Fixture::new();
        let mut key = fixture.key("SELECTED_TOKEN", KeySource::Vault);
        key.usage = KeyUsage::StoreOnly;
        key.required = false;
        let value = "x".repeat(secret::RedactionCorpusLimits::PRODUCTION.max_bytes / 128 + 1);
        assert!(
            secret::RedactionService::new()
                .acquire_execution_lease(&[&SecretString::new(value.clone())])
                .is_err()
        );
        let key = fixture.save(key, &value);
        let actor = selected_actor(&fixture, &key);
        let result = fixture.service.run_agent_command(
            &actor,
            std::slice::from_ref(&key.id),
            None,
            &[
                "/bin/sh".into(),
                "-c".into(),
                "touch unexpected-spawn".into(),
            ],
            Path::new("."),
            &crate::project_keys::process::NativeProcessRunner::for_cli(),
        );
        assert!(
            !fixture.root.join("unexpected-spawn").exists(),
            "unprotectable selected API child was spawned"
        );
        assert!(matches!(result, Err(ProjectKeysError::Process)));
    }

    struct BlockingReceiptRunner {
        db_path: PathBuf,
        lock: Mutex<Option<rusqlite::Connection>>,
        path: std::sync::mpsc::SyncSender<PathBuf>,
    }
    impl ProcessRunner for BlockingReceiptRunner {
        fn run(
            &self,
            _: &[String],
            _: &Path,
            environment: &ProcessEnvironment,
            _: Option<&SecretString>,
            _: bool,
        ) -> Result<ProcessOutput, ProjectKeysError> {
            let path = PathBuf::from(environment.values[0].1.expose());
            assert!(path.exists());
            let lock = rusqlite::Connection::open(&self.db_path).unwrap();
            lock.execute_batch("BEGIN IMMEDIATE").unwrap();
            *self.lock.lock().unwrap() = Some(lock);
            self.path.send(path).unwrap();
            Ok(ProcessOutput {
                exit_code: 0,
                stdout: vec![],
            })
        }
    }
    #[test]
    fn temporary_pem_is_removed_before_blocked_receipt_finalization() {
        let fixture = Fixture::new();
        let mut key = fixture.key("PRIVATE_KEY", KeySource::Vault);
        key.kind = KeyKind::PrivateKey;
        key.delivery = KeyDelivery::TemporaryFile;
        fixture.save(key, "fixture-pem-bytes");
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let runner = std::sync::Arc::new(BlockingReceiptRunner {
            db_path: fixture.root.join("metadata.sqlite3"),
            lock: Mutex::new(None),
            path: tx,
        });
        let running = runner.clone();
        let build = std::thread::spawn(move || {
            fixture.service.run_build(
                &fixture.scope,
                None,
                &["fixture".into()],
                Path::new("."),
                running.as_ref(),
            )
        });
        let path = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_millis(300);
        while path.exists() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let cleaned_before_publication = !path.exists();
        runner
            .lock
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .execute_batch("ROLLBACK")
            .unwrap();
        build.join().unwrap().unwrap();
        assert!(
            cleaned_before_publication,
            "plaintext PEM must be gone while final receipt DB save is blocked"
        );
    }
    struct ReplacingStore {
        inner: std::sync::Arc<crate::project_keys::tests::MemoryStore>,
        paths: Mutex<Option<(PathBuf, PathBuf)>>,
    }
    impl secret::SecretStore for ReplacingStore {
        fn set_secret(&self, id: &str, value: &SecretString) -> anyhow::Result<()> {
            self.inner.set_secret(id, value)
        }
        fn get_secret(&self, id: &str) -> anyhow::Result<SecretString> {
            let value = self.inner.get_secret(id)?;
            if let Some((root, old)) = self.paths.lock().unwrap().take() {
                std::fs::rename(&root, old)?;
                std::fs::create_dir(root)?;
            }
            Ok(value)
        }
        fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
            self.inner.delete_secret(id)
        }
        fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
            self.inner.has_secret(id)
        }
        fn list_secret_ids(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
            self.inner.list_secret_ids(prefix)
        }
    }
    #[test]
    fn workspace_failure_after_receipt_publication_settles_receipt() {
        let mut fixture = Fixture::new();
        fixture.save(fixture.key("TOKEN", KeySource::Vault), "fixture-token");
        let old = fixture.root.with_extension("receipt-original");
        fixture.service.secrets = std::sync::Arc::new(ReplacingStore {
            inner: fixture.store.clone(),
            paths: Mutex::new(Some((fixture.root.clone(), old.clone()))),
        });
        let result = fixture.service.run_build(
            &fixture.scope,
            None,
            &["fixture".into()],
            Path::new("."),
            &ObservedRunner::default(),
        );
        let receipts = fixture
            .service
            .db
            .project_key_receipt_list(&fixture.scope.workspace_id, fixture.scope.environment, 10)
            .unwrap();
        std::fs::remove_dir_all(old).unwrap();
        assert!(matches!(result, Err(ProjectKeysError::SourceChanged)));
        assert_eq!(receipts.len(), 1);
        assert!(receipts[0].finished_at.is_some());
        assert_eq!(receipts[0].exit_code, Some(1));
        assert!(receipts[0].owner.is_some());
    }
    #[derive(Default)]
    struct ObservedRunner(Mutex<Vec<String>>);
    impl ProcessRunner for ObservedRunner {
        fn run(
            &self,
            _: &[String],
            _: &Path,
            environment: &ProcessEnvironment,
            _: Option<&SecretString>,
            _: bool,
        ) -> Result<ProcessOutput, ProjectKeysError> {
            assert!(environment.values.is_empty());
            *self.0.lock().unwrap() = environment.remove_names.clone();
            Ok(ProcessOutput {
                exit_code: 0,
                stdout: vec![],
            })
        }
    }
    #[test]
    fn unavailable_optional_vault_bundle_is_omitted_and_ambient_name_scrubbed() {
        let fixture = Fixture::new();
        let mut key = fixture.key("OPTIONAL_TOKEN", KeySource::Vault);
        key.required = false;
        fixture.save(key, "fixture-token");
        fixture.store.1.store(true, Ordering::Release);
        let runner = ObservedRunner::default();
        let receipt = fixture
            .service
            .run_build(
                &fixture.scope,
                None,
                &["fixture".into()],
                Path::new("."),
                &runner,
            )
            .unwrap();
        assert!(receipt.used_keys.is_empty());
        assert!(runner.0.lock().unwrap().contains(&"OPTIONAL_TOKEN".into()));
    }
    #[test]
    fn optional_empty_legacy_configuration_keeps_build_available_and_scrubs_name() {
        let fixture = Fixture::new();
        let profile = fixture
            .service
            .db
            .insert_env_profile(&fixture.scope.workspace_id, "custom", "custom")
            .unwrap();
        fixture
            .service
            .db
            .upsert_env_var(
                &profile,
                "API_URL",
                &storage::EnvValue::Plain(String::new()),
            )
            .unwrap();
        let runner = ObservedRunner::default();
        let receipt = fixture
            .service
            .run_build(
                &fixture.scope,
                None,
                &["fixture".into()],
                Path::new("."),
                &runner,
            )
            .unwrap();
        assert!(receipt.used_keys.is_empty());
        assert!(runner.0.lock().unwrap().contains(&"API_URL".into()));
        let keys = fixture
            .service
            .db
            .project_key_list(&fixture.scope.workspace_id, fixture.scope.environment)
            .unwrap();
        assert_eq!(
            keys.iter()
                .find(|key| key.name == "API_URL")
                .unwrap()
                .storage_state,
            KeyStorageState::EmptySource
        );
    }
}
