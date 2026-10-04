//! Runtime fixtures must inject their own store rather than choose an ambient keychain backend.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use secret::{SecretStore, SecretString};

#[derive(Default)]
struct MemorySecretStore(Mutex<BTreeMap<String, SecretString>>);

impl SecretStore for MemorySecretStore {
    fn set_secret(&self, id: &str, value: &SecretString) -> anyhow::Result<()> {
        self.0
            .lock()
            .expect("test secret store lock")
            .insert(id.to_owned(), SecretString::new(value.expose().to_owned()));
        Ok(())
    }

    fn get_secret(&self, id: &str) -> anyhow::Result<SecretString> {
        self.0
            .lock()
            .expect("test secret store lock")
            .get(id)
            .map(|value| SecretString::new(value.expose().to_owned()))
            .ok_or_else(|| anyhow::anyhow!("runtime_test_secret_missing"))
    }

    fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
        self.0.lock().expect("test secret store lock").remove(id);
        Ok(())
    }

    fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
        Ok(self
            .0
            .lock()
            .expect("test secret store lock")
            .contains_key(id))
    }

    fn list_secret_ids(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        Ok(self
            .0
            .lock()
            .expect("test secret store lock")
            .keys()
            .filter(|id| id.starts_with(prefix))
            .cloned()
            .collect())
    }
}

/// Each fixture owns an empty store. Share that fixture's Arc explicitly with its runtime worker.
pub(crate) fn test_store() -> Arc<dyn SecretStore> {
    Arc::new(MemorySecretStore::default())
}

#[test]
fn pr18_test_store_is_private_per_fixture_and_shared_only_by_arc() {
    let first = test_store();
    let second = test_store();
    assert!(!Arc::ptr_eq(&first, &second));
    first
        .set_secret("same-coordinate", &SecretString::new("first-value".into()))
        .unwrap();
    assert!(!second.has_secret("same-coordinate").unwrap());
    second
        .set_secret("same-coordinate", &SecretString::new("second-value".into()))
        .unwrap();

    let reader = Arc::clone(&first);
    let observed = std::thread::spawn(move || reader.get_secret("same-coordinate").unwrap())
        .join()
        .unwrap();
    assert_eq!(observed.expose(), "first-value");
    assert_eq!(
        second.get_secret("same-coordinate").unwrap().expose(),
        "second-value"
    );
    assert!(test_store().get_secret("same-coordinate").is_err());
}

#[test]
fn pr18_test_store_preserves_overwrite_missing_and_idempotent_delete() {
    let store = test_store();
    assert!(!store.has_secret("fixture-id").unwrap());
    let missing = store.get_secret("fixture-id").unwrap_err();
    assert_eq!(missing.to_string(), "runtime_test_secret_missing");
    store.delete_secret("fixture-id").unwrap();

    store
        .set_secret("fixture-id", &SecretString::new("old-value".into()))
        .unwrap();
    let old_read = store.get_secret("fixture-id").unwrap();
    store
        .set_secret("fixture-id", &SecretString::new("new-value".into()))
        .unwrap();
    assert_eq!(old_read.expose(), "old-value");
    let new_read = store.get_secret("fixture-id").unwrap();
    assert_eq!(new_read.expose(), "new-value");
    assert_eq!(format!("{new_read:?}"), "SecretString(REDACTED)");
    assert!(store.has_secret("fixture-id").unwrap());
    store.delete_secret("fixture-id").unwrap();
    store.delete_secret("fixture-id").unwrap();
    assert!(!store.has_secret("fixture-id").unwrap());
    assert!(store.get_secret("fixture-id").is_err());
}

#[test]
fn pr18_test_store_prefix_inventory_matches_secret_store_contract() {
    let store = test_store();
    for id in ["other", "fixture-z", "fixture-a"] {
        store
            .set_secret(id, &SecretString::new("private-value".into()))
            .unwrap();
    }
    assert_eq!(
        store.list_secret_ids("fixture-").unwrap(),
        ["fixture-a", "fixture-z"]
    );
    assert_eq!(
        store.list_secret_ids_bounded("fixture-").unwrap(),
        ["fixture-a", "fixture-z"]
    );
    assert!(store.list_secret_ids_bounded("absent-").unwrap().is_empty());
}

#[test]
fn pr18_runtime_test_helpers_never_select_native_keyring_backend() {
    for (name, source) in [
        ("in_process.rs", include_str!("in_process.rs")),
        ("remote.rs", include_str!("remote.rs")),
    ] {
        for forbidden in [
            "secret::KeyringSecretStore",
            "keyring_core::set_default_store",
        ] {
            assert!(
                !source.contains(forbidden),
                "{name} must inject a private in-memory SecretStore; found {forbidden}"
            );
        }
    }
}
