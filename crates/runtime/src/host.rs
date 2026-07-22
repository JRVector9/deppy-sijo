use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use crate::{PersistConfig, RuntimeCommand, RuntimeEventReceiver, RuntimeEventStream};

/// A secret resolved at the last responsible moment for a runtime launch.
///
/// The wrapper deliberately implements neither `Clone` nor serialization. Its
/// debug representation never exposes the value. The runtime eventually moves
/// the value into a child-process environment, where the existing PTY cleanup
/// owns its remaining lifetime.
pub struct RuntimeSecret(secret::SecretString);

impl RuntimeSecret {
    pub fn new(value: String) -> Self {
        Self(secret::SecretString::new(value))
    }

    pub(crate) fn from_secret_string(value: secret::SecretString) -> Self {
        Self(value)
    }

    pub(crate) fn as_secret_string(&self) -> &secret::SecretString {
        &self.0
    }

    pub(crate) fn into_string(self) -> String {
        self.0.into_string()
    }
}

impl fmt::Debug for RuntimeSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RuntimeSecret(REDACTED)")
    }
}

/// App-owned port from a logical credential id to the currently published
/// physical secret value. Implementations may consult storage/keyring, while
/// runtime remains independent of those concrete adapter types.
pub trait RuntimeSecretResolver: Send + Sync + 'static {
    fn resolve(&self, logical_credential_id: &str) -> anyhow::Result<RuntimeSecret>;
}

/// Arguments needed to create one workspace runtime. This is inert data: merely
/// constructing it or a factory starts no thread, process, timer, or polling.
pub struct RuntimeHostConfig {
    pub output_batch_ms: u64,
    pub logs_root: PathBuf,
    pub persist: Option<PersistConfig>,
    pub cwd: Option<PathBuf>,
    pub extra_env: Vec<(String, String)>,
}

pub type RuntimeWake = Arc<dyn Fn() + Send + Sync>;
pub type RuntimeCommandDispatcher = Arc<dyn Fn(RuntimeCommand) -> anyhow::Result<()> + Send + Sync>;

/// Final app-facing runtime lifecycle boundary.
///
/// `shutdown` is synchronous and must not return before the worker and child
/// process cleanup have completed. Implementations must also make `Drop`
/// preserve that guarantee.
pub trait RuntimeHost: RuntimeEventStream + Send {
    fn submit(&self, command: RuntimeCommand) -> anyhow::Result<()>;
    fn command_dispatcher(&self) -> Option<RuntimeCommandDispatcher>;
    fn subscribe_with_wake(&self, wake: RuntimeWake) -> RuntimeEventReceiver;
    fn subscribe_with_wake_background(&self, wake: RuntimeWake) -> RuntimeEventReceiver;
    fn shutdown(&mut self);
}

pub trait RuntimeHostFactory: Send + Sync {
    fn create(&self, config: RuntimeHostConfig) -> anyhow::Result<Box<dyn RuntimeHost>>;
}

/// Production in-process factory. Holding this value is side-effect free; the
/// existing single runtime worker is started only by `create`.
pub struct InProcessRuntimeHostFactory {
    resolver: Arc<dyn RuntimeSecretResolver>,
    redaction: secret::RedactionService,
}

impl InProcessRuntimeHostFactory {
    pub fn new(
        resolver: Arc<dyn RuntimeSecretResolver>,
        redaction: secret::RedactionService,
    ) -> Self {
        Self {
            resolver,
            redaction,
        }
    }
}

impl RuntimeHostFactory for InProcessRuntimeHostFactory {
    fn create(&self, config: RuntimeHostConfig) -> anyhow::Result<Box<dyn RuntimeHost>> {
        Ok(Box::new(
            crate::InProcessRuntimeClient::try_new_with_resolver(
                config.output_batch_ms,
                Arc::clone(&self.resolver),
                config.logs_root,
                self.redaction.clone(),
                config.persist,
                config.cwd,
                config.extra_env,
            )?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingResolver(AtomicUsize);

    impl RuntimeSecretResolver for CountingResolver {
        fn resolve(&self, _logical_credential_id: &str) -> anyhow::Result<RuntimeSecret> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(RuntimeSecret::new("not-used".to_owned()))
        }
    }

    #[test]
    fn runtime_secret_debug_is_always_redacted() {
        let secret = RuntimeSecret::new("unique-plaintext-marker".to_owned());
        let debug = format!("{secret:?}");
        assert_eq!(debug, "RuntimeSecret(REDACTED)");
        assert!(!debug.contains("unique-plaintext-marker"));
    }

    #[test]
    fn runtime_secret_transfers_the_original_plaintext_allocation() {
        let plaintext = "unique-transfer-marker".to_owned();
        let pointer = plaintext.as_ptr();
        let capacity = plaintext.capacity();
        let secret = RuntimeSecret::new(plaintext);
        let transferred = secret.into_string();

        assert_eq!(transferred.as_ptr(), pointer);
        assert_eq!(transferred.capacity(), capacity);
        assert_eq!(transferred, "unique-transfer-marker");
    }

    #[test]
    fn factory_is_inert_and_host_rejects_invalid_correlation_before_enqueue() {
        let resolver = Arc::new(CountingResolver(AtomicUsize::new(0)));
        let factory =
            InProcessRuntimeHostFactory::new(resolver.clone(), secret::RedactionService::new());
        assert_eq!(resolver.0.load(Ordering::Relaxed), 0);

        let mut host = factory
            .create(RuntimeHostConfig {
                output_batch_ms: 5,
                logs_root: std::env::temp_dir()
                    .join(format!("deppy-runtime-host-test-{}", std::process::id())),
                persist: None,
                cwd: None,
                extra_env: Vec::new(),
            })
            .unwrap();
        let invalid = RuntimeCommand::SpawnAgent {
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
            agent_config_id: Some("x".repeat(crate::command::AGENT_CONFIG_ID_MAX_BYTES + 1)),
            command: "/bin/echo".to_owned(),
            args: Vec::new(),
            env_plain: Vec::new(),
            env_secrets: Vec::new(),
            waiting_regex: None,
            approval_regex: None,
            error_regex: None,
            done_regex: None,
        };
        assert!(host.submit(invalid.clone()).is_err());
        let dispatcher = host.command_dispatcher().expect("live dispatcher");
        assert!(dispatcher(invalid).is_err());
        assert_eq!(resolver.0.load(Ordering::Relaxed), 0);
        host.shutdown();
    }
}
