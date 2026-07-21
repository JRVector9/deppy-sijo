//! Versioned physical keyring slots for OAuth secret bundles.
//!
//! SQLite owns the logical-id → physical-slot pointer transaction. This module only stages and
//! reconciles complete keyring bundles; it never publishes a database pointer.

use std::collections::BTreeSet;

use anyhow::Context;
use uuid::Uuid;

use crate::{SecretStore, SecretString, hex};

const SLOT_PREFIX: &str = "deppy.oauth.v1.";
const REFRESH_SUFFIX: &str = ".refresh";
const DCR_SUFFIX: &str = ".dcr";

/// Stable database identity. This is never accepted as a physical keyring username implicitly.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LogicalCredentialId(String);

impl LogicalCredentialId {
    pub fn new(value: impl Into<String>) -> anyhow::Result<Self> {
        let value = value.into();
        anyhow::ensure!(!value.is_empty(), "logical credential id is empty");
        // Hex encoding plus the fixed prefix and UUID must stay below the storage/keyring 255-byte
        // username ceiling.
        anyhow::ensure!(value.len() <= 96, "logical credential id is too long");
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for LogicalCredentialId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("LogicalCredentialId").field(&self.0).finish()
    }
}

/// Versioned physical keyring username. Access is stored at this username; refresh and DCR use
/// typed suffixes derived by this module.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhysicalSecretSlot(String);

impl PhysicalSecretSlot {
    pub fn allocate(logical_id: &LogicalCredentialId) -> Self {
        Self::with_version(logical_id, Uuid::new_v4())
    }

    pub fn with_version(logical_id: &LogicalCredentialId, version: Uuid) -> Self {
        Self(format!(
            "{SLOT_PREFIX}{}.{}",
            hex::to_hex(logical_id.as_str().as_bytes()),
            version.simple()
        ))
    }

    pub fn parse(value: impl Into<String>) -> anyhow::Result<Self> {
        let value = value.into();
        anyhow::ensure!(value.len() <= 255, "physical secret slot is too long");
        let rest = value
            .strip_prefix(SLOT_PREFIX)
            .context("physical secret slot prefix is invalid")?;
        let (logical_hex, version) = rest
            .split_once('.')
            .context("physical secret slot shape is invalid")?;
        anyhow::ensure!(
            !logical_hex.is_empty(),
            "physical secret slot logical id is empty"
        );
        let logical_bytes =
            hex::from_hex(logical_hex).context("physical slot logical id encoding")?;
        anyhow::ensure!(
            !logical_bytes.is_empty(),
            "physical slot logical id is empty"
        );
        std::str::from_utf8(&logical_bytes).context("physical slot logical id is not UTF-8")?;
        Uuid::parse_str(version).context("physical secret slot version is invalid")?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn belongs_to(&self, logical_id: &LogicalCredentialId) -> bool {
        let prefix = format!(
            "{SLOT_PREFIX}{}.",
            hex::to_hex(logical_id.as_str().as_bytes())
        );
        self.0.starts_with(&prefix)
    }

    pub fn refresh_entry_id(&self) -> String {
        format!("{}{REFRESH_SUFFIX}", self.0)
    }

    pub fn dcr_entry_id(&self) -> String {
        format!("{}{DCR_SUFFIX}", self.0)
    }
}

impl std::fmt::Debug for PhysicalSecretSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PhysicalSecretSlot").field(&self.0).finish()
    }
}

/// Owned access/refresh/DCR values. It is intentionally non-Clone and non-Serialize.
pub struct SecretBundle {
    access: SecretString,
    refresh: Option<SecretString>,
    dcr: Option<SecretString>,
}

impl SecretBundle {
    pub fn new(
        access: SecretString,
        refresh: Option<SecretString>,
        dcr: Option<SecretString>,
    ) -> Self {
        Self {
            access,
            refresh,
            dcr,
        }
    }

    pub fn access(&self) -> &SecretString {
        &self.access
    }

    pub fn refresh(&self) -> Option<&SecretString> {
        self.refresh.as_ref()
    }

    pub fn dcr(&self) -> Option<&SecretString> {
        self.dcr.as_ref()
    }

    pub fn as_ref(&self) -> SecretBundleRef<'_> {
        SecretBundleRef::new(self.access(), self.refresh(), self.dcr())
    }

    pub fn into_parts(self) -> (SecretString, Option<SecretString>, Option<SecretString>) {
        (self.access, self.refresh, self.dcr)
    }
}

impl std::fmt::Debug for SecretBundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretBundle(REDACTED)")
    }
}

/// Borrowed bundle used to stage an OAuth token without cloning secret-bearing values.
pub struct SecretBundleRef<'a> {
    access: &'a SecretString,
    refresh: Option<&'a SecretString>,
    dcr: Option<&'a SecretString>,
}

impl<'a> SecretBundleRef<'a> {
    pub fn new(
        access: &'a SecretString,
        refresh: Option<&'a SecretString>,
        dcr: Option<&'a SecretString>,
    ) -> Self {
        Self {
            access,
            refresh,
            dcr,
        }
    }
}

impl std::fmt::Debug for SecretBundleRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretBundleRef(REDACTED)")
    }
}

/// Adapter-neutral stage plan. The new physical slot is written before the database pointer swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretBundleStagePlan {
    logical_id: LogicalCredentialId,
    new_slot: PhysicalSecretSlot,
    previous_slot: Option<PhysicalSecretSlot>,
}

impl SecretBundleStagePlan {
    pub fn allocate(
        logical_id: LogicalCredentialId,
        previous_slot: Option<PhysicalSecretSlot>,
    ) -> anyhow::Result<Self> {
        let new_slot = PhysicalSecretSlot::allocate(&logical_id);
        Self::with_slot(logical_id, new_slot, previous_slot)
    }

    pub fn with_slot(
        logical_id: LogicalCredentialId,
        new_slot: PhysicalSecretSlot,
        previous_slot: Option<PhysicalSecretSlot>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            new_slot.belongs_to(&logical_id),
            "physical slot does not belong to logical credential"
        );
        anyhow::ensure!(
            previous_slot.as_ref() != Some(&new_slot),
            "new physical slot must differ from previous slot"
        );
        anyhow::ensure!(
            previous_slot
                .as_ref()
                .is_none_or(|slot| slot.belongs_to(&logical_id)),
            "previous physical slot does not belong to logical credential"
        );
        Ok(Self {
            logical_id,
            new_slot,
            previous_slot,
        })
    }

    pub fn logical_id(&self) -> &LogicalCredentialId {
        &self.logical_id
    }

    pub fn new_slot(&self) -> &PhysicalSecretSlot {
        &self.new_slot
    }

    pub fn previous_slot(&self) -> Option<&PhysicalSecretSlot> {
        self.previous_slot.as_ref()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BundleEntryPresence {
    pub access: bool,
    pub refresh: bool,
    pub dcr: bool,
}

impl BundleEntryPresence {
    pub fn count(self) -> usize {
        usize::from(self.access) + usize::from(self.refresh) + usize::from(self.dcr)
    }

    pub fn is_empty(self) -> bool {
        self.count() == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedSecretBundle {
    pub logical_id: LogicalCredentialId,
    pub new_slot: PhysicalSecretSlot,
    pub previous_slot: Option<PhysicalSecretSlot>,
    pub entries: BundleEntryPresence,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BundleDeleteResult {
    pub entries_deleted: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileSecretSlotsResult {
    pub slots_seen: usize,
    pub orphan_slots_deleted: usize,
    pub entries_deleted: usize,
}

pub fn inspect_secret_bundle(
    store: &dyn SecretStore,
    slot: &PhysicalSecretSlot,
) -> anyhow::Result<BundleEntryPresence> {
    Ok(BundleEntryPresence {
        access: store.has_secret(slot.as_str())?,
        refresh: store.has_secret(&slot.refresh_entry_id())?,
        dcr: store.has_secret(&slot.dcr_entry_id())?,
    })
}

pub fn stage_secret_bundle(
    store: &dyn SecretStore,
    plan: &SecretBundleStagePlan,
    bundle: SecretBundleRef<'_>,
) -> anyhow::Result<StagedSecretBundle> {
    let before = inspect_secret_bundle(store, plan.new_slot())?;
    anyhow::ensure!(before.is_empty(), "new physical secret slot already exists");

    let slot = plan.new_slot();
    let refresh_id = slot.refresh_entry_id();
    let dcr_id = slot.dcr_entry_id();
    let write_result = (|| {
        store.set_secret(slot.as_str(), bundle.access)?;
        if let Some(refresh) = bundle.refresh {
            store.set_secret(&refresh_id, refresh)?;
        }
        if let Some(dcr) = bundle.dcr {
            store.set_secret(&dcr_id, dcr)?;
        }
        Ok::<(), anyhow::Error>(())
    })();
    if let Err(error) = write_result {
        let mut rollback_errors = Vec::new();
        // A backend may persist an entry and still report an error. The slot was confirmed empty,
        // so remove every entry that this stage could have touched, including the failed write.
        let mut attempted = vec![slot.as_str()];
        if bundle.refresh.is_some() {
            attempted.push(&refresh_id);
        }
        if bundle.dcr.is_some() {
            attempted.push(&dcr_id);
        }
        for id in attempted.into_iter().rev() {
            if let Err(rollback) = store.delete_secret(id) {
                rollback_errors.push(format!("{id}: {rollback:#}"));
            }
        }
        if rollback_errors.is_empty() {
            return Err(error).context("physical secret bundle stage failed; rollback complete");
        }
        return Err(error).context(format!(
            "physical secret bundle stage failed; rollback errors: {}",
            rollback_errors.join(", ")
        ));
    }

    Ok(StagedSecretBundle {
        logical_id: plan.logical_id.clone(),
        new_slot: plan.new_slot.clone(),
        previous_slot: plan.previous_slot.clone(),
        entries: BundleEntryPresence {
            access: true,
            refresh: bundle.refresh.is_some(),
            dcr: bundle.dcr.is_some(),
        },
    })
}

pub fn read_secret_bundle(
    store: &dyn SecretStore,
    slot: &PhysicalSecretSlot,
) -> anyhow::Result<SecretBundle> {
    let access = store
        .get_secret(slot.as_str())
        .context("physical secret bundle access read failed")?;
    let refresh_id = slot.refresh_entry_id();
    let refresh = store
        .has_secret(&refresh_id)?
        .then(|| store.get_secret(&refresh_id))
        .transpose()
        .context("physical secret bundle refresh read failed")?;
    let dcr_id = slot.dcr_entry_id();
    let dcr = store
        .has_secret(&dcr_id)?
        .then(|| store.get_secret(&dcr_id))
        .transpose()
        .context("physical secret bundle DCR read failed")?;
    Ok(SecretBundle::new(access, refresh, dcr))
}

pub fn delete_secret_bundle(
    store: &dyn SecretStore,
    slot: &PhysicalSecretSlot,
) -> anyhow::Result<BundleDeleteResult> {
    let presence = inspect_secret_bundle(store, slot)?;
    let entries = [
        (slot.as_str().to_owned(), presence.access),
        (slot.refresh_entry_id(), presence.refresh),
        (slot.dcr_entry_id(), presence.dcr),
    ];
    let mut errors = Vec::new();
    let mut deleted = 0;
    for (id, existed) in entries {
        if let Err(error) = store.delete_secret(&id) {
            errors.push(format!("{id}: {error:#}"));
        } else if existed {
            deleted += 1;
        }
    }
    anyhow::ensure!(
        errors.is_empty(),
        "physical secret bundle delete failed: {}",
        errors.join(", ")
    );
    Ok(BundleDeleteResult {
        entries_deleted: deleted,
    })
}

pub fn list_secret_bundle_slots(
    store: &dyn SecretStore,
    logical_id: Option<&LogicalCredentialId>,
) -> anyhow::Result<Vec<PhysicalSecretSlot>> {
    let entries = store.list_secret_ids(SLOT_PREFIX)?;
    let mut slots = BTreeSet::new();
    for entry in entries {
        let base = entry
            .strip_suffix(REFRESH_SUFFIX)
            .or_else(|| entry.strip_suffix(DCR_SUFFIX))
            .unwrap_or(&entry);
        let Ok(slot) = PhysicalSecretSlot::parse(base.to_owned()) else {
            continue;
        };
        if logical_id.is_none_or(|logical| slot.belongs_to(logical)) {
            slots.insert(slot);
        }
    }
    Ok(slots.into_iter().collect())
}

/// Deletes every versioned OAuth slot not present in the database-provided referenced set.
/// Call this during startup before accepting secret-backed work, when no stage/publish is racing.
pub fn reconcile_orphan_secret_slots(
    store: &dyn SecretStore,
    referenced: &BTreeSet<PhysicalSecretSlot>,
) -> anyhow::Result<ReconcileSecretSlotsResult> {
    let slots = list_secret_bundle_slots(store, None)?;
    let mut result = ReconcileSecretSlotsResult {
        slots_seen: slots.len(),
        ..ReconcileSecretSlotsResult::default()
    };
    for slot in slots {
        if referenced.contains(&slot) {
            continue;
        }
        let deleted = delete_secret_bundle(store, &slot)?;
        result.orphan_slots_deleted += 1;
        result.entries_deleted += deleted.entries_deleted;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct MemStore {
        entries: Mutex<HashMap<String, String>>,
        fail_on: Mutex<Option<String>>,
        fail_after_write_on: Mutex<Option<String>>,
    }

    impl MemStore {
        fn fail_on(&self, id: String) {
            *self.fail_on.lock().unwrap() = Some(id);
        }

        fn fail_after_write_on(&self, id: String) {
            *self.fail_after_write_on.lock().unwrap() = Some(id);
        }
    }

    impl SecretStore for MemStore {
        fn set_secret(&self, id: &str, secret: &SecretString) -> anyhow::Result<()> {
            if self.fail_on.lock().unwrap().as_deref() == Some(id) {
                anyhow::bail!("injected write failure")
            }
            self.entries
                .lock()
                .unwrap()
                .insert(id.to_owned(), secret.expose().to_owned());
            if self.fail_after_write_on.lock().unwrap().as_deref() == Some(id) {
                anyhow::bail!("injected failure after write")
            }
            Ok(())
        }

        fn get_secret(&self, id: &str) -> anyhow::Result<SecretString> {
            self.entries
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .map(SecretString::new)
                .context("missing entry")
        }

        fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
            self.entries.lock().unwrap().remove(id);
            Ok(())
        }

        fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
            Ok(self.entries.lock().unwrap().contains_key(id))
        }

        fn list_secret_ids(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
            Ok(self
                .entries
                .lock()
                .unwrap()
                .keys()
                .filter(|id| id.starts_with(prefix))
                .cloned()
                .collect())
        }
    }

    fn logical() -> LogicalCredentialId {
        LogicalCredentialId::new("cred-oauth").unwrap()
    }

    fn slot(version: u128) -> PhysicalSecretSlot {
        PhysicalSecretSlot::with_version(&logical(), Uuid::from_u128(version))
    }

    fn bundle(access: &str, refresh: &str, dcr: &str) -> SecretBundle {
        SecretBundle::new(
            SecretString::new(access.to_owned()),
            Some(SecretString::new(refresh.to_owned())),
            Some(SecretString::new(dcr.to_owned())),
        )
    }

    #[test]
    fn logical_id_and_physical_slot_cannot_be_confused() {
        let logical = logical();
        let physical = slot(1);
        assert_ne!(logical.as_str(), physical.as_str());
        assert!(physical.belongs_to(&logical));
        assert_eq!(
            PhysicalSecretSlot::parse(physical.as_str().to_owned()).unwrap(),
            physical
        );
        assert!(PhysicalSecretSlot::parse(logical.as_str().to_owned()).is_err());
    }

    #[test]
    fn complete_bundle_is_staged_read_and_deleted_as_one_typed_slot() {
        let store = MemStore::default();
        let plan = SecretBundleStagePlan::with_slot(logical(), slot(2), None).unwrap();
        let secrets = bundle("access-2", "refresh-2", "dcr-2");
        let staged = stage_secret_bundle(&store, &plan, secrets.as_ref()).unwrap();
        assert_eq!(staged.entries.count(), 3);
        assert_eq!(list_secret_bundle_slots(&store, None).unwrap(), [slot(2)]);
        let read = read_secret_bundle(&store, &slot(2)).unwrap();
        assert_eq!(read.access().expose(), "access-2");
        assert_eq!(read.refresh().unwrap().expose(), "refresh-2");
        assert_eq!(read.dcr().unwrap().expose(), "dcr-2");
        assert_eq!(
            delete_secret_bundle(&store, &slot(2))
                .unwrap()
                .entries_deleted,
            3
        );
        assert!(inspect_secret_bundle(&store, &slot(2)).unwrap().is_empty());
    }

    #[test]
    fn stage_failure_rolls_back_every_entry_written_in_new_slot() {
        let store = MemStore::default();
        let new_slot = slot(3);
        store.fail_on(new_slot.dcr_entry_id());
        let plan = SecretBundleStagePlan::with_slot(logical(), new_slot.clone(), None).unwrap();
        assert!(
            stage_secret_bundle(
                &store,
                &plan,
                bundle("a-token", "r-token", "d-token").as_ref()
            )
            .is_err()
        );
        assert!(inspect_secret_bundle(&store, &new_slot).unwrap().is_empty());
    }

    #[test]
    fn write_then_error_is_also_rolled_back_from_new_slot() {
        let store = MemStore::default();
        let new_slot = slot(4);
        store.fail_after_write_on(new_slot.dcr_entry_id());
        let plan = SecretBundleStagePlan::with_slot(logical(), new_slot.clone(), None).unwrap();
        assert!(
            stage_secret_bundle(
                &store,
                &plan,
                bundle("a-token", "r-token", "d-token").as_ref()
            )
            .is_err()
        );
        assert!(inspect_secret_bundle(&store, &new_slot).unwrap().is_empty());
    }

    #[test]
    fn stage_plan_rejects_previous_slot_from_another_logical_credential() {
        let current = logical();
        let other = LogicalCredentialId::new("other-credential").unwrap();
        let result = SecretBundleStagePlan::with_slot(
            current.clone(),
            PhysicalSecretSlot::allocate(&current),
            Some(PhysicalSecretSlot::allocate(&other)),
        );
        assert!(result.is_err());
        assert!(SecretBundleStagePlan::allocate(current, Some(slot(9))).is_ok());
    }

    #[test]
    fn startup_reconciliation_keeps_referenced_and_deletes_all_orphan_versions() {
        let store = MemStore::default();
        for version in 10..14 {
            let physical = slot(version);
            let plan = SecretBundleStagePlan::with_slot(logical(), physical, None).unwrap();
            stage_secret_bundle(
                &store,
                &plan,
                bundle("access-token", "refresh-token", "dcr-secret").as_ref(),
            )
            .unwrap();
        }
        let referenced = BTreeSet::from([slot(13)]);
        let result = reconcile_orphan_secret_slots(&store, &referenced).unwrap();
        assert_eq!(result.slots_seen, 4);
        assert_eq!(result.orphan_slots_deleted, 3);
        assert_eq!(result.entries_deleted, 9);
        assert_eq!(list_secret_bundle_slots(&store, None).unwrap(), [slot(13)]);
    }

    #[test]
    fn one_hundred_rotations_leave_one_slot_and_three_entries() {
        let store = MemStore::default();
        let mut previous = None;
        for version in 100..200 {
            let next = slot(version);
            let plan = SecretBundleStagePlan::with_slot(logical(), next.clone(), previous.clone())
                .unwrap();
            stage_secret_bundle(
                &store,
                &plan,
                bundle("access-token", "refresh-token", "dcr-secret").as_ref(),
            )
            .unwrap();
            if let Some(old) = previous.replace(next) {
                delete_secret_bundle(&store, &old).unwrap();
            }
        }
        assert_eq!(list_secret_bundle_slots(&store, None).unwrap().len(), 1);
        assert_eq!(store.entries.lock().unwrap().len(), 3);
    }
}
