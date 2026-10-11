use super::*;

#[test]
fn github_environment_names_use_path_segment_encoding() {
    let target = GitHubTarget {
        auth_credential_id: None,
        account_identity: "fixture-account".into(),
        scope: GitHubScope::Environment {
            owner: "owner".into(),
            repository: "repository".into(),
            environment: "prod eu+한글".into(),
        },
        secret_name: "TOKEN".into(),
        auto_send: false,
    };
    assert_eq!(
        endpoint(&target),
        "repos/owner/repository/environments/prod%20eu%2B%ED%95%9C%EA%B8%80/secrets/TOKEN"
    );
}

use crate::project_keys::{service::committed, tests::Fixture};
use secret::SecretString;
use std::{collections::VecDeque, sync::Mutex};

struct Runner {
    private_key: crypto_box::SecretKey,
    remote: Mutex<VecDeque<Option<&'static str>>>,
    calls: Mutex<Vec<Vec<String>>>,
    decrypted: Mutex<Vec<u8>>,
    deadlines: Mutex<Vec<Instant>>,
    lose_put_response: bool,
    failed_put_response: bool,
}
impl Runner {
    fn new(remote: &[Option<&'static str>]) -> Self {
        Self {
            private_key: crypto_box::SecretKey::generate(&mut crypto_box::aead::OsRng),
            remote: Mutex::new(remote.iter().copied().collect()),
            calls: Mutex::new(Vec::new()),
            decrypted: Mutex::new(Vec::new()),
            deadlines: Mutex::new(Vec::new()),
            lose_put_response: false,
            failed_put_response: false,
        }
    }
}
impl ProcessRunner for Runner {
    fn run(
        &self,
        command: &[String],
        _: &Path,
        _: &ProcessEnvironment,
        stdin: Option<&SecretString>,
        inherit: bool,
    ) -> Result<ProcessOutput, ProjectKeysError> {
        assert!(!inherit);
        assert_eq!(&command[..4], ["gh", "api", "--hostname", "github.com"]);
        self.calls.lock().unwrap().push(command.to_vec());
        let endpoint = &command[4];
        let stdout = if endpoint == "user" {
            b"{\"login\":\"fixture-account\"}".to_vec()
        } else if endpoint.ends_with("/public-key") {
            serde_json::to_vec(&serde_json::json!({"key_id":"fixture-key-id", "key":STANDARD.encode(self.private_key.public_key().as_bytes())})).unwrap()
        } else if command.windows(2).any(|pair| pair == ["--method", "PUT"]) {
            let payload: serde_json::Value = serde_json::from_str(stdin.unwrap().expose()).unwrap();
            assert_eq!(payload["key_id"], "fixture-key-id");
            let cipher = STANDARD
                .decode(payload["encrypted_value"].as_str().unwrap())
                .unwrap();
            *self.decrypted.lock().unwrap() = self.private_key.unseal(&cipher).unwrap();
            if endpoint.starts_with("orgs/") {
                assert_eq!(
                    payload["selected_repository_ids"],
                    serde_json::json!([42, 9007199254740993_u64])
                );
                assert_eq!(payload["visibility"], "selected");
                assert!(payload.get("repos").is_none());
            }
            assert!(!command.iter().any(|value| value.contains("fixture-secret")));
            if self.lose_put_response {
                return Err(ProjectKeysError::Process);
            }
            if self.failed_put_response {
                return Ok(ProcessOutput {
                    exit_code: 1,
                    stdout: Vec::new(),
                });
            }
            vec![]
        } else {
            assert!(stdin.is_none());
            match self.remote.lock().unwrap().pop_front().unwrap() {
                Some(timestamp) => format!(
                    "HTTP/2.0 200 OK\r\n\r\n{{\"name\":\"TOKEN\",\"updated_at\":\"{timestamp}\"}}"
                )
                .into_bytes(),
                None => b"HTTP/2.0 404 Not Found\r\n\r\n{}".to_vec(),
            }
        };
        Ok(ProcessOutput {
            exit_code: 0,
            stdout,
        })
    }
    fn run_until(
        &self,
        command: &[String],
        cwd: &Path,
        environment: &ProcessEnvironment,
        stdin: Option<&SecretString>,
        inherit: bool,
        deadline: Instant,
    ) -> Result<ProcessOutput, ProjectKeysError> {
        assert!(deadline.duration_since(Instant::now()) <= GITHUB_OPERATION_BUDGET);
        self.deadlines.lock().unwrap().push(deadline);
        self.run(command, cwd, environment, stdin, inherit)
    }
}
fn target() -> GitHubTarget {
    GitHubTarget {
        auth_credential_id: None,
        account_identity: "fixture-account".into(),
        scope: GitHubScope::Repository {
            owner: "owner".into(),
            repository: "repository".into(),
        },
        secret_name: "TOKEN".into(),
        auto_send: false,
    }
}
fn linked(fixture: &Fixture, mut key: ProjectKey, target: GitHubTarget) -> ProjectKey {
    key.github = Some(KeyGitHubStatus {
        target,
        state: GitHubRemoteState::Unlinked,
        last_sent_revision: None,
        remote_updated_at: None,
        error_code: None,
    });
    committed(
        fixture
            .service
            .db
            .project_key_save(
                &fixture.scope.workspace_id,
                fixture.scope.environment,
                &key,
                Some(key.revision),
            )
            .unwrap(),
    )
    .unwrap()
}
fn claim(fixture: &Fixture, key: &ProjectKey) -> GitHubOutboxEntry {
    assert!(
        fixture
            .service
            .db
            .project_key_github_enqueue(
                &fixture.scope.workspace_id,
                fixture.scope.environment,
                &key.id,
                key.revision,
                false,
                now()
            )
            .unwrap()
    );
    fixture
        .service
        .db
        .project_key_github_claim(now(), 120, 1)
        .unwrap()
        .remove(0)
}
#[test]
fn github_encrypted_put_preserves_newlines_and_numeric_organization_identities() {
    for value in [
        "fixture-secret\n",
        "fixture-secret\r\n\r\n",
        "fixture-secret\ninside\n",
    ] {
        let fixture = Fixture::new();
        let key = fixture.save(fixture.key("TOKEN", KeySource::Vault), value);
        let mut destination = target();
        destination.scope = GitHubScope::Organization {
            organization: "fixture-org".into(),
            visibility: GitHubVisibility::Selected,
            selected_repository_ids: vec![42, 9007199254740993],
        };
        let key = linked(&fixture, key, destination);
        let entry = claim(&fixture, &key);
        let runner = Runner::new(&[None, Some("2026-10-12T03:00:00Z")]);
        assert!(matches!(
            fixture.service.github_send(&entry, &runner).unwrap(),
            GitHubSendOutcome::Sent { .. }
        ));
        assert_eq!(*runner.decrypted.lock().unwrap(), value.as_bytes());
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 5);
        assert!(
            !calls
                .iter()
                .flatten()
                .any(|value| value.starts_with("repositories/"))
        );
        let deadlines = runner.deadlines.lock().unwrap();
        assert!(deadlines.iter().all(|deadline| *deadline == deadlines[0]));
    }
}
#[test]
fn github_success_without_postwrite_metadata_and_lost_put_response_pause() {
    for response in 0..3 {
        let fixture = Fixture::new();
        let key = linked(
            &fixture,
            fixture.save(fixture.key("TOKEN", KeySource::Vault), "fixture-secret"),
            target(),
        );
        let entry = claim(&fixture, &key);
        let mut runner = Runner::new(&[None, None]);
        runner.lose_put_response = response == 1;
        runner.failed_put_response = response == 2;
        let outcome = fixture.service.github_send(&entry, &runner).unwrap();
        assert_eq!(
            outcome,
            GitHubSendOutcome::ExternalChange {
                remote_updated_at: None
            }
        );
        assert!(
            fixture
                .service
                .db
                .project_key_github_ack(&entry, outcome)
                .unwrap()
        );
        let actual = fixture
            .service
            .key(&fixture.scope, &key.id, key.revision)
            .unwrap();
        let status = actual.github.unwrap();
        assert_eq!(status.state, GitHubRemoteState::ExternalChange);
        assert_eq!(status.last_sent_revision, None);
        assert!(
            fixture
                .service
                .db
                .project_key_github_claim(now() + 1000, 120, 1)
                .unwrap()
                .is_empty()
        );
    }
}
#[test]
fn github_observer_skips_active_lease_then_detects_external_change_after_ack() {
    let fixture = Fixture::new();
    let key = linked(
        &fixture,
        fixture.save(fixture.key("TOKEN", KeySource::Vault), "fixture-secret"),
        target(),
    );
    let entry = claim(&fixture, &key);
    let claimed = fixture
        .service
        .key(&fixture.scope, &key.id, key.revision)
        .unwrap();
    assert!(
        !fixture
            .service
            .observe_github(&claimed, &Runner::new(&[Some("2026-10-12T03:00:00Z")]))
            .unwrap()
    );
    assert!(
        fixture
            .service
            .db
            .project_key_github_ack(
                &entry,
                GitHubSendOutcome::Sent {
                    remote_updated_at: Some("2026-10-12T03:00:00Z".into())
                }
            )
            .unwrap()
    );
    let sent = fixture
        .service
        .key(&fixture.scope, &key.id, key.revision)
        .unwrap();
    assert!(
        fixture
            .service
            .observe_github(&sent, &Runner::new(&[Some("2026-10-12T04:00:00Z")]))
            .unwrap()
    );
    let paused = fixture
        .service
        .key(&fixture.scope, &key.id, key.revision)
        .unwrap();
    assert_eq!(
        paused.github.as_ref().unwrap().state,
        GitHubRemoteState::ExternalChange
    );
    assert_eq!(
        paused.github.as_ref().unwrap().last_sent_revision,
        Some(key.revision)
    );
    assert_eq!(paused.last_good_revision, key.last_good_revision);
    assert!(
        !fixture
            .service
            .observe_github(&sent, &Runner::new(&[Some("2026-10-12T05:00:00Z")]))
            .unwrap()
    );
}
#[test]
fn github_remote_only_observer_reads_metadata_without_secret_store_io() {
    let fixture = Fixture::new();
    let key = fixture
        .service
        .save_key(
            &fixture.scope,
            fixture.key("TOKEN", KeySource::GitHubOnly),
            None,
            ValueEdit::Keep,
        )
        .unwrap();
    let key = linked(&fixture, key, target());
    let before = fixture.store.2.load(std::sync::atomic::Ordering::Acquire);
    let mut cursor = None;
    assert_eq!(
        fixture
            .service
            .poll_github_next(&mut cursor, &Runner::new(&[Some("2026-10-12T03:00:00Z")]))
            .unwrap(),
        Some(true)
    );
    assert_eq!(cursor, Some(key.id));
    assert_eq!(
        fixture.store.2.load(std::sync::atomic::Ordering::Acquire),
        before
    );
}
#[test]
fn github_ciphertext_input_limits_and_key_validation() {
    let runner = Runner::new(&[]);
    let public = serde_json::json!({"key_id":"fixture-key-id", "key":STANDARD.encode(runner.private_key.public_key().as_bytes())});
    let value = SecretString::new("x".repeat(GITHUB_SECRET_BYTES_MAX + 1));
    assert!(matches!(
        encrypted_payload(&target(), &public, &value),
        Err(ProjectKeysError::InvalidInput)
    ));
    assert!(matches!(
        encrypted_payload(
            &target(),
            &serde_json::json!({"key_id":"a", "key":"invalid"}),
            &SecretString::new("fixture-secret".into())
        ),
        Err(ProjectKeysError::GitHubRemote)
    ));
}

#[test]
fn github_case_insensitive_secret_name_accepts_canonical_metadata() {
    let fixture = Fixture::new();
    let mut destination = target();
    destination.secret_name = "token".into();
    let key = linked(
        &fixture,
        fixture.save(fixture.key("TOKEN", KeySource::Vault), "fixture-secret"),
        destination,
    );
    let entry = claim(&fixture, &key);
    let outcome = fixture
        .service
        .github_send(&entry, &Runner::new(&[None, Some("2026-10-12T03:00:00Z")]))
        .unwrap();
    assert!(matches!(outcome, GitHubSendOutcome::Sent { .. }));
}
#[test]
fn github_case_insensitive_account_identity_is_still_exact_account() {
    let fixture = Fixture::new();
    let mut destination = target();
    destination.account_identity = "Fixture-Account".into();
    let key = linked(
        &fixture,
        fixture.save(fixture.key("TOKEN", KeySource::Vault), "fixture-secret"),
        destination,
    );
    let entry = claim(&fixture, &key);
    let outcome = fixture
        .service
        .github_send(&entry, &Runner::new(&[None, Some("2026-10-12T03:00:00Z")]))
        .unwrap();
    assert!(matches!(outcome, GitHubSendOutcome::Sent { .. }));
}

#[test]
fn github_account_case_folding_never_accepts_a_different_identity() {
    let fixture = Fixture::new();
    let mut destination = target();
    destination.account_identity = "another-account".into();
    let key = linked(
        &fixture,
        fixture.save(fixture.key("TOKEN", KeySource::Vault), "fixture-secret"),
        destination,
    );
    let entry = claim(&fixture, &key);
    let runner = Runner::new(&[]);
    assert!(matches!(
        fixture.service.github_send(&entry, &runner),
        Err(ProjectKeysError::GitHubAuth)
    ));
    assert_eq!(runner.calls.lock().unwrap().len(), 1);
    assert!(runner.decrypted.lock().unwrap().is_empty());
}
