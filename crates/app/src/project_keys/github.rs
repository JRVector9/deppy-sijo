use super::{
    contracts::*,
    process::{ProcessEnvironment, ProcessOutput, ProcessRunner},
    service::{ProjectKeysService, now, storage_error},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use deppy_core::project_keys::*;
use secret::{LogicalCredentialId, PhysicalSecretSlot};
use std::{
    path::Path,
    time::{Duration, Instant},
};

const GITHUB_SECRET_BYTES_MAX: usize = 48 * 1024;
const GITHUB_OPERATION_BUDGET: Duration = Duration::from_secs(90);

struct GitHubOperation<'a> {
    runner: &'a dyn ProcessRunner,
    deadline: Instant,
}
impl ProcessRunner for GitHubOperation<'_> {
    fn run(
        &self,
        command: &[String],
        cwd: &Path,
        environment: &ProcessEnvironment,
        stdin: Option<&secret::SecretString>,
        inherit_output: bool,
    ) -> Result<ProcessOutput, ProjectKeysError> {
        self.runner.run_until(
            command,
            cwd,
            environment,
            stdin,
            inherit_output,
            self.deadline,
        )
    }
}

fn path_segment(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            result.push(char::from(byte));
        } else {
            use std::fmt::Write;
            let _ = write!(&mut result, "%{byte:02X}");
        }
    }
    result
}

fn endpoint(target: &GitHubTarget) -> String {
    match &target.scope {
        GitHubScope::Repository { owner, repository } => format!(
            "repos/{owner}/{repository}/actions/secrets/{}",
            target.secret_name
        ),
        GitHubScope::Environment {
            owner,
            repository,
            environment,
        } => format!(
            "repos/{owner}/{repository}/environments/{}/secrets/{}",
            path_segment(environment),
            target.secret_name
        ),
        GitHubScope::Organization { organization, .. } => {
            format!("orgs/{organization}/actions/secrets/{}", target.secret_name)
        }
    }
}

fn api_args(endpoint: &str) -> Vec<String> {
    args(&["gh", "api", "--hostname", "github.com", endpoint])
}

fn encrypted_payload(
    target: &GitHubTarget,
    public_key: &serde_json::Value,
    plaintext: &secret::SecretString,
) -> Result<secret::SecretString, ProjectKeysError> {
    if plaintext.expose().len() > GITHUB_SECRET_BYTES_MAX {
        return Err(ProjectKeysError::InvalidInput);
    }
    let key_id = public_key
        .get("key_id")
        .and_then(|value| value.as_str())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 256
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
        })
        .ok_or(ProjectKeysError::GitHubRemote)?;
    let public_key = public_key
        .get("key")
        .and_then(|value| value.as_str())
        .filter(|value| value.len() <= 128)
        .ok_or(ProjectKeysError::GitHubRemote)?;
    let decoded = STANDARD
        .decode(public_key)
        .map_err(|_| ProjectKeysError::GitHubRemote)?;
    let key =
        crypto_box::PublicKey::from_slice(&decoded).map_err(|_| ProjectKeysError::GitHubRemote)?;
    let ciphertext = key
        .seal(&mut crypto_box::aead::OsRng, plaintext.expose().as_bytes())
        .map_err(|_| ProjectKeysError::GitHubRemote)?;
    let mut payload =
        serde_json::json!({"encrypted_value": STANDARD.encode(ciphertext), "key_id": key_id});
    if let GitHubScope::Organization {
        visibility,
        selected_repository_ids,
        ..
    } = &target.scope
    {
        payload["visibility"] = serde_json::json!(match visibility {
            GitHubVisibility::All => "all",
            GitHubVisibility::Private => "private",
            GitHubVisibility::Selected => "selected",
        });
        if *visibility == GitHubVisibility::Selected {
            payload["selected_repository_ids"] = serde_json::json!(selected_repository_ids);
        }
    }
    serde_json::to_string(&payload)
        .map(secret::SecretString::new)
        .map_err(|_| ProjectKeysError::GitHubRemote)
}
fn json(output: ProcessOutput) -> Result<serde_json::Value, ProjectKeysError> {
    if output.exit_code != 0 {
        return Err(ProjectKeysError::GitHubRemote);
    }
    serde_json::from_slice(&output.stdout).map_err(|_| ProjectKeysError::GitHubRemote)
}
fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).into()).collect()
}
impl ProjectKeysService {
    fn github_auth(
        &self,
        scope: &ProjectKeysScope,
        target: &GitHubTarget,
        runner: &dyn ProcessRunner,
    ) -> Result<ProcessEnvironment, ProjectKeysError> {
        target
            .validate()
            .map_err(|_| ProjectKeysError::InvalidInput)?;
        let mut environment = ProcessEnvironment::default();
        if let Some(id) = &target.auth_credential_id {
            let (_, location) = storage_error(self.db.project_key_auth_credential(
                &scope.workspace_id,
                scope.environment,
                id,
            ))?
            .ok_or(ProjectKeysError::GitHubAuth)?;
            let value =
                if let Ok(slot) = PhysicalSecretSlot::parse(location.keyring_username.clone()) {
                    let logical =
                        LogicalCredentialId::new(id).map_err(|_| ProjectKeysError::GitHubAuth)?;
                    if !slot.belongs_to(&logical) {
                        return Err(ProjectKeysError::GitHubAuth);
                    }
                    secret::read_secret_bundle(self.secrets.as_ref(), &slot)
                        .map_err(|_| ProjectKeysError::GitHubAuth)?
                        .into_parts()
                        .0
                } else {
                    self.secrets
                        .get_secret(&location.keyring_username)
                        .map_err(|_| ProjectKeysError::GitHubAuth)?
                };
            if value.expose().is_empty() || value.expose().len() > SECRET_INPUT_BYTES_MAX {
                return Err(ProjectKeysError::GitHubAuth);
            }
            environment.values.push(("GH_TOKEN".into(), value));
        }
        let user = json(
            runner
                .run(&api_args("user"), Path::new("/"), &environment, None, false)
                .map_err(|_| ProjectKeysError::GitHubAuth)?,
        )?;
        if user
            .get("login")
            .and_then(|value| value.as_str())
            .is_none_or(|login| !login.eq_ignore_ascii_case(&target.account_identity))
        {
            return Err(ProjectKeysError::GitHubAuth);
        }
        Ok(environment)
    }
    fn github_metadata(
        &self,
        target: &GitHubTarget,
        environment: &ProcessEnvironment,
        runner: &dyn ProcessRunner,
    ) -> Result<Option<String>, ProjectKeysError> {
        let mut command = api_args(&endpoint(target));
        command.push("--include".into());
        let output = runner.run(&command, Path::new("/"), environment, None, false)?;
        let text =
            std::str::from_utf8(&output.stdout).map_err(|_| ProjectKeysError::GitHubRemote)?;
        let status = text
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("HTTP/"))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or(ProjectKeysError::GitHubRemote)?;
        if status == 404 {
            return Ok(None);
        }
        if status != 200 || output.exit_code != 0 {
            return Err(ProjectKeysError::GitHubRemote);
        }
        let body = text
            .split_once("\r\n\r\n")
            .or_else(|| text.split_once("\n\n"))
            .map(|(_, body)| body)
            .ok_or(ProjectKeysError::GitHubRemote)?;
        let value: serde_json::Value =
            serde_json::from_str(body).map_err(|_| ProjectKeysError::GitHubRemote)?;
        let timestamp = value
            .get("updated_at")
            .and_then(|value| value.as_str())
            .filter(|value| valid_metadata_text(value, 128))
            .ok_or(ProjectKeysError::GitHubRemote)?;
        if value
            .get("name")
            .and_then(|value| value.as_str())
            .is_none_or(|name| !name.eq_ignore_ascii_case(&target.secret_name))
        {
            return Err(ProjectKeysError::GitHubRemote);
        }
        Ok(Some(timestamp.into()))
    }
    pub fn observe_github(
        &self,
        key: &ProjectKey,
        runner: &dyn ProcessRunner,
    ) -> Result<bool, ProjectKeysError> {
        let operation = GitHubOperation {
            runner,
            deadline: Instant::now() + GITHUB_OPERATION_BUDGET,
        };
        let runner = &operation as &dyn ProcessRunner;
        let scope = ProjectKeysScope {
            workspace_id: key.workspace_id.clone(),
            environment: key.environment,
        };
        self.key(&scope, &key.id, key.revision)?;
        let status = key.github.as_ref().ok_or(ProjectKeysError::InvalidInput)?;
        let environment = self.github_auth(&scope, &status.target, runner)?;
        let remote = self.github_metadata(&status.target, &environment, runner)?;
        storage_error(self.db.project_key_github_observe_cas(
            &scope.workspace_id,
            scope.environment,
            &key.id,
            key.revision,
            status,
            remote,
            now(),
        ))
    }
    pub fn poll_github_next(
        &self,
        cursor: &mut Option<String>,
        runner: &dyn ProcessRunner,
    ) -> Result<Option<bool>, ProjectKeysError> {
        let mut page = storage_error(
            self.db
                .project_key_github_observer_page(1, cursor.as_deref()),
        )?;
        if page.is_empty() && cursor.is_some() {
            *cursor = None;
            page = storage_error(self.db.project_key_github_observer_page(1, None))?;
        }
        let Some(key) = page.into_iter().next() else {
            return Ok(None);
        };
        // Advance before IO so one unavailable account does not starve other linked targets.
        *cursor = Some(key.id.clone());
        self.observe_github(&key, runner).map(Some)
    }
    pub fn github_send(
        &self,
        entry: &GitHubOutboxEntry,
        runner: &dyn ProcessRunner,
    ) -> Result<GitHubSendOutcome, ProjectKeysError> {
        let operation = GitHubOperation {
            runner,
            deadline: Instant::now() + GITHUB_OPERATION_BUDGET,
        };
        let runner = &operation as &dyn ProcessRunner;
        let scope = ProjectKeysScope {
            workspace_id: entry.workspace_id.clone(),
            environment: entry.environment,
        };
        let mut key = self.key(&scope, &entry.key_id, entry.key_revision)?;
        if key.source.is_file() {
            self.observe_key(&scope, &key)?;
            key = self.key(&scope, &entry.key_id, entry.key_revision)?;
        }
        let status = key
            .github
            .as_ref()
            .filter(|status| status.target == entry.target)
            .ok_or(ProjectKeysError::Stale)?;
        if matches!(
            key.storage_state,
            KeyStorageState::Conflict
                | KeyStorageState::MissingSource
                | KeyStorageState::EmptySource
                | KeyStorageState::Locked
                | KeyStorageState::UpdateNeeded
                | KeyStorageState::Failed
                | KeyStorageState::NotStored
        ) {
            return Err(ProjectKeysError::Conflict);
        }
        let environment = self.github_auth(&scope, &entry.target, runner)?;
        let remote = self.github_metadata(&entry.target, &environment, runner)?;
        if remote != status.remote_updated_at {
            return Ok(GitHubSendOutcome::ExternalChange {
                remote_updated_at: remote,
            });
        }
        let bundle = self.bundle(&scope, &key)?;
        if bundle.access().expose().len() > GITHUB_SECRET_BYTES_MAX {
            return Err(ProjectKeysError::InvalidInput);
        }
        let endpoint = endpoint(&entry.target);
        let public_key_endpoint = format!(
            "{}/public-key",
            endpoint
                .rsplit_once('/')
                .ok_or(ProjectKeysError::InvalidInput)?
                .0
        );
        let public_key = json(runner.run(
            &api_args(&public_key_endpoint),
            Path::new("/"),
            &environment,
            None,
            false,
        )?)?;
        // Encrypt the original bytes directly: gh secret set strips trailing CR/LF.
        // Numeric organization repository identities are retained in the encrypted PUT body.
        let payload = encrypted_payload(&entry.target, &public_key, bundle.access())?;
        let mut command = api_args(&endpoint);
        command.extend(args(&["--method", "PUT", "--input", "-"]));
        // Recheck local revision and durable claim after all network preflight.
        self.key(&scope, &entry.key_id, entry.key_revision)?;
        if !storage_error(self.db.project_key_github_claim_is_current(entry, now()))? {
            return Err(ProjectKeysError::Stale);
        }
        let result = match runner.run(
            &command,
            Path::new("/"),
            &environment,
            Some(&payload),
            false,
        ) {
            Ok(result) => result,
            // A timed-out/lost response may follow a completed PUT. Do not automatically overwrite again.
            Err(_) => {
                return Ok(GitHubSendOutcome::ExternalChange {
                    remote_updated_at: None,
                });
            }
        };
        if result.exit_code != 0 {
            // gh can report failure after the server accepted the write (e.g. response loss).
            return Ok(GitHubSendOutcome::ExternalChange {
                remote_updated_at: None,
            });
        }
        // A successful write without readable metadata remains value-unverified and paused.
        // Never mark it Sent or silently retry a potentially completed remote side effect.
        match self.github_metadata(&entry.target, &environment, runner) {
            Ok(Some(remote)) => Ok(GitHubSendOutcome::Sent {
                remote_updated_at: Some(remote),
            }),
            Ok(None) | Err(_) => Ok(GitHubSendOutcome::ExternalChange {
                remote_updated_at: None,
            }),
        }
    }
    pub fn drain_github(&self, runner: &dyn ProcessRunner) -> Result<(), ProjectKeysError> {
        storage_error(self.db.project_key_github_reconcile_pending(now(), 16))?;
        let entries = storage_error(self.db.project_key_github_claim(now(), 120, 1))?;
        for entry in entries {
            let outcome = match self.github_send(&entry, runner) {
                Ok(outcome) => outcome,
                Err(ProjectKeysError::Stale) => continue,
                Err(error) => GitHubSendOutcome::Retry {
                    error_code: error.code().into(),
                    next_attempt_at: now() + ((1_i64 << entry.attempts.min(8)) * 15).min(3600),
                },
            };
            storage_error(self.db.project_key_github_ack(&entry, outcome))?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "github_tests.rs"]
mod tests;
