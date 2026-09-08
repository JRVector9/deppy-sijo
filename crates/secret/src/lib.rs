mod bundle;
mod diagnostic_scan;
pub mod hex;
#[cfg(target_os = "macos")]
mod macos;
mod redaction;
pub mod token;

pub use bundle::{
    BundleDeleteResult, BundleEntryPresence, LogicalCredentialId, PhysicalSecretSlot,
    ReconcileSecretSlotsResult, SecretBundle, SecretBundleRef, SecretBundleStagePlan,
    StagedSecretBundle, delete_secret_bundle, inspect_secret_bundle, list_secret_bundle_slots,
    read_secret_bundle, reconcile_orphan_secret_slots, stage_secret_bundle,
};
pub use diagnostic_scan::{
    DIAGNOSTIC_SCAN_MAX_FINDINGS, DIAGNOSTIC_SCAN_MAX_INPUT_BYTES, DiagnosticFindingCount,
    DiagnosticFindingKind, DiagnosticScanReport, scan_diagnostic_bytes,
};
pub use redaction::{
    RedactionCapacityError, RedactionClock, RedactionCorpusLimits, RedactionCorpusStats,
    RedactionLease, RedactionService, StreamRedactor,
};

use anyhow::Context;

/// keyring 좌표: service는 앱 번들 ID 고정, username은 credential id.
pub const KEYRING_SERVICE: &str = "app.vector9.deppy-sijo";

/// Maximum number of distinct versioned OAuth bundle slots retained by one inventory pass.
/// This is a compile-time production ceiling; callers cannot raise it at runtime.
pub const VERSIONED_SECRET_BUNDLE_SLOT_CEILING: usize = 4_096;
/// One slot has at most access/refresh/DCR entries. The extra entry is retained only long enough
/// to prove overflow and fail closed.
pub const SECRET_PREFIX_ENTRY_PROBE_CEILING: usize = VERSIONED_SECRET_BUNDLE_SLOT_CEILING * 3 + 1;
const SECRET_PREFIX_ENTRY_LIMIT: usize = SECRET_PREFIX_ENTRY_PROBE_CEILING - 1;

/// secret 평문 래퍼. Debug 출력은 항상 REDACTED (설계문서 PR-02 완료 기준).
/// Serialize/Display를 구현하지 않아 config/SQLite/log로의 우발적 유출을 컴파일 단계에서 막는다.
pub struct SecretString(
    String,
    #[cfg(test)] Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
);

impl SecretString {
    pub fn new(value: String) -> Self {
        Self(
            value,
            #[cfg(test)]
            None,
        )
    }

    /// 평문 접근. keyring 저장/env 주입 등 명시적 사용처에서만 호출한다.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Consumes the zeroizing wrapper and transfers its plaintext `String` allocation without a
    /// copy.
    ///
    /// The caller assumes responsibility for the plaintext's lifetime after this call. Keep it as
    /// short as possible and move it promptly into the next zeroizing owner; an ordinary `String`
    /// does not overwrite its allocation when dropped. The consumed wrapper drops with an empty
    /// buffer and therefore cannot erase the transferred allocation.
    pub fn into_string(mut self) -> String {
        std::mem::take(&mut self.0)
    }

    #[cfg(test)]
    fn observe_zeroized_drop(&mut self, observer: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        self.1 = Some(observer);
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString(REDACTED)")
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        // SAFETY: this value exclusively owns the String and only overwrites existing bytes.
        for byte in unsafe { self.0.as_mut_vec() } {
            // SAFETY: `byte` is an exclusively borrowed byte in the owned allocation.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
        #[cfg(test)]
        if let Some(observer) = &self.1 {
            observer.store(
                self.0.as_bytes().iter().all(|byte| *byte == 0),
                std::sync::atomic::Ordering::Release,
            );
        }
    }
}

/// UI 표시용 힌트. 평문 복원이 불가능하도록 끝 4자만 남긴다 (8자 미만은 전부 마스킹).
pub fn masked_hint(secret: &str) -> String {
    if secret.chars().count() >= 8 {
        let tail: String = secret
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("****{tail}")
    } else {
        "****".to_owned()
    }
}

/// secret 저장소 추상화 (설계문서 Secret 모듈).
/// 모든 호출은 composition root가 조립한 service/coordinator worker에서만 수행하며,
/// render 경로는 이 trait을 보거나 호출하지 않는다.
pub trait SecretStore: Send + Sync {
    fn set_secret(&self, id: &str, secret: &SecretString) -> anyhow::Result<()>;
    /// spawn 직전 env 주입에서만 호출 (설계문서 6.3, PR-09)
    fn get_secret(&self, id: &str) -> anyhow::Result<SecretString>;
    /// Deletes an entry idempotently. A confirmed missing entry is success.
    fn delete_secret(&self, id: &str) -> anyhow::Result<()>;
    /// secret 존재 여부. 확인된 부재는 Ok(false), 조회 오류(일시 장애 등)는 Err —
    /// "없음"과 "오류"를 구별해야 하는 경로(암호화 키 get-or-create)에서 쓴다.
    fn has_secret(&self, id: &str) -> anyhow::Result<bool>;
    /// Returns usernames with the requested prefix. Implementations that cannot enumerate entries
    /// must return an error rather than pretending the inventory is empty.
    fn list_secret_ids(&self, _prefix: &str) -> anyhow::Result<Vec<String>> {
        anyhow::bail!("secret store does not support entry enumeration")
    }

    /// Returns a sorted, deduplicated prefix inventory under the fixed production ceiling.
    ///
    /// The default preserves compatibility with stores that only implement `list_secret_ids`.
    /// Such legacy implementations may allocate their source `Vec` before this method can reject
    /// it; production keyring storage overrides this method and bounds the retained filtered
    /// inventory at the earliest iterator stage exposed by `keyring-core`.
    fn list_secret_ids_bounded(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        let ids = self.list_secret_ids(prefix)?;
        collect_bounded_secret_ids(ids)
    }
}

fn collect_bounded_secret_ids(
    ids: impl IntoIterator<Item = String>,
) -> anyhow::Result<Vec<String>> {
    let mut ids = ids
        .into_iter()
        .take(SECRET_PREFIX_ENTRY_PROBE_CEILING)
        .collect::<Vec<_>>();
    anyhow::ensure!(
        ids.len() <= SECRET_PREFIX_ENTRY_LIMIT,
        "secret entry inventory exceeds fixed prefix ceiling"
    );
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// keyring-core 기본 store 기반 구현. 사용 전 플랫폼 store가
/// `keyring_core::set_default_store`로 등록되어 있어야 한다.
pub struct KeyringSecretStore;

/// 설계문서 1.4: 동일 credential 멀티스레드 접근은 신뢰 불가 —
/// UI(set/delete)와 runtime worker(get)가 겹치지 않도록 프로세스 전역 직렬화.
static KEYRING_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl KeyringSecretStore {
    #[cfg(any(not(target_os = "macos"), test))]
    fn entry(&self, id: &str) -> anyhow::Result<keyring_core::Entry> {
        keyring_core::Entry::new(KEYRING_SERVICE, id)
            .with_context(|| format!("keyring entry 생성 실패: {id}"))
    }
}

impl SecretStore for KeyringSecretStore {
    fn set_secret(&self, id: &str, secret: &SecretString) -> anyhow::Result<()> {
        let _serial = KEYRING_SERIAL.lock().expect("keyring serial lock");
        #[cfg(all(target_os = "macos", not(test)))]
        {
            macos::set(id, secret).context("keyring 저장 실패")
        }
        #[cfg(any(not(target_os = "macos"), test))]
        {
            self.entry(id)?
                .set_password(secret.expose())
                .with_context(|| format!("keyring 저장 실패: {id}"))
        }
    }

    fn get_secret(&self, id: &str) -> anyhow::Result<SecretString> {
        let _serial = KEYRING_SERIAL.lock().expect("keyring serial lock");
        #[cfg(all(target_os = "macos", not(test)))]
        {
            macos::get(id).context("keyring 조회 실패")
        }
        #[cfg(any(not(target_os = "macos"), test))]
        {
            let password = self
                .entry(id)?
                .get_password()
                .with_context(|| format!("keyring 조회 실패: {id}"))?;
            Ok(SecretString::new(password))
        }
    }

    fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
        let _serial = KEYRING_SERIAL.lock().expect("keyring serial lock");
        #[cfg(all(target_os = "macos", not(test)))]
        {
            macos::delete(id).context("keyring 삭제 실패")
        }
        #[cfg(any(not(target_os = "macos"), test))]
        {
            match self.entry(id)?.delete_credential() {
                Ok(()) => Ok(()),
                // 이미 없는 entry 삭제는 성공으로 취급 (metadata/keyring drift 복구 허용)
                Err(keyring_core::Error::NoEntry) => Ok(()),
                Err(e) => Err(e).with_context(|| format!("keyring 삭제 실패: {id}")),
            }
        }
    }

    fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
        let _serial = KEYRING_SERIAL.lock().expect("keyring serial lock");
        #[cfg(all(target_os = "macos", not(test)))]
        {
            macos::has(id).context("keyring 존재 확인 실패")
        }
        #[cfg(any(not(target_os = "macos"), test))]
        {
            match self.entry(id)?.get_password() {
                Ok(_) => Ok(true),
                // 확인된 부재만 false — 그 외 오류는 "없음"으로 오인하면 안 된다 (키 덮어쓰기 방지)
                Err(keyring_core::Error::NoEntry) => Ok(false),
                Err(e) => Err(e).with_context(|| format!("keyring 존재 확인 실패: {id}")),
            }
        }
    }

    fn list_secret_ids(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        self.list_secret_ids_bounded(prefix)
    }

    fn list_secret_ids_bounded(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        let _serial = KEYRING_SERIAL.lock().expect("keyring serial lock");
        #[cfg(all(target_os = "macos", not(test)))]
        {
            macos::list(prefix).context("keyring inventory 조회 실패")
        }
        #[cfg(any(not(target_os = "macos"), test))]
        {
            let spec = std::collections::HashMap::from([("service", KEYRING_SERVICE)]);
            // `Entry::search` returns a platform-owned Vec, so its source allocation cannot be bounded
            // without a streaming keyring-core API. From the first iterator stage available to us,
            // nonmatching service/prefix rows are dropped immediately and only limit+1 matching
            // usernames are retained. The +1 probe proves overflow without retaining the full flood.
            let matching_users = keyring_core::Entry::search(&spec)
                .context("keyring entry inventory 조회 실패")?
                .into_iter()
                .filter_map(|entry| entry.get_specifiers())
                .filter_map(|(service, user)| (service == KEYRING_SERVICE).then_some(user))
                .filter(|user| user.starts_with(prefix));
            collect_bounded_secret_ids(matching_users)
        }
    }
}

/// 플랫폼 keyring store를 기본 store로 등록한다.
/// 설계문서 1.4: insecure fallback 금지 — 미지원 플랫폼은 등록하지 않고,
/// credential 조작 시점에 NoDefaultStore 에러로 표면화된다.
pub fn init_platform_store() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let store = apple_native_keyring_store::keychain::Store::new()
            .context("macOS keychain store 초기화 실패")?;
        keyring_core::set_default_store(store);
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        let store = windows_native_keyring_store::store::Store::new()
            .context("Windows credential store 초기화 실패")?;
        keyring_core::set_default_store(store);
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        anyhow::bail!("지원되지 않는 플랫폼: keyring store 없음 (Linux는 설계문서 1.8 참조)")
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn init_mock_store() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            // mock store는 test only (설계문서 1.4)
            keyring_core::set_default_store(keyring_core::mock::Store::new().unwrap());
        });
    }

    #[test]
    fn debug_출력은_redacted() {
        let secret = SecretString::new("sk-live-abcdef123456".into());
        assert_eq!(format!("{secret:?}"), "SecretString(REDACTED)");
    }

    #[test]
    fn into_string은_평문_allocation을_복사없이_호출자에게_이전한다() {
        let marker = "unique-secret-allocation-marker-4f63f32e";
        let mut plaintext = String::with_capacity(256);
        plaintext.push_str(marker);
        let allocation = plaintext.as_ptr();
        let capacity = plaintext.capacity();

        let transferred = SecretString::new(plaintext).into_string();

        assert_eq!(transferred, marker);
        assert_eq!(transferred.as_ptr(), allocation);
        assert_eq!(transferred.capacity(), capacity);
    }

    #[test]
    fn secret_string_drop은_평문_buffer를_계속_zeroize한다() {
        let zeroized = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut secret = SecretString::new("drop-zeroization-marker".to_owned());
        secret.observe_zeroized_drop(std::sync::Arc::clone(&zeroized));

        drop(secret);

        assert!(zeroized.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn masked_hint는_평문을_노출하지_않는다() {
        assert_eq!(masked_hint("sk-live-abcdef123456"), "****3456");
        assert_eq!(masked_hint("short"), "****");
        assert_eq!(masked_hint(""), "****");
    }

    #[test]
    fn keyring_roundtrip() {
        init_mock_store();
        let store = KeyringSecretStore;
        let secret = SecretString::new("test-secret-value".into());
        store.set_secret("cred-roundtrip", &secret).unwrap();
        assert_eq!(
            store.get_secret("cred-roundtrip").unwrap().expose(),
            "test-secret-value"
        );
        store.delete_secret("cred-roundtrip").unwrap();
        assert!(store.get_secret("cred-roundtrip").is_err());
    }

    #[test]
    fn 없는_entry_삭제는_성공() {
        init_mock_store();
        KeyringSecretStore.delete_secret("cred-missing").unwrap();
    }

    #[test]
    fn keyring_inventory는_service와_prefix로_제한된다() {
        init_mock_store();
        let store = KeyringSecretStore;
        store
            .set_secret(
                "deppy.oauth.v1.test.inventory",
                &SecretString::new("inventory-secret".to_owned()),
            )
            .unwrap();
        store
            .set_secret(
                "legacy-credential",
                &SecretString::new("legacy-secret".to_owned()),
            )
            .unwrap();
        assert_eq!(
            store.list_secret_ids("deppy.oauth.v1.test").unwrap(),
            ["deppy.oauth.v1.test.inventory"]
        );
        store
            .delete_secret("deppy.oauth.v1.test.inventory")
            .unwrap();
        store.delete_secret("legacy-credential").unwrap();
    }

    #[test]
    fn bounded_inventory_accepts_zero_and_exact_fixed_limit() {
        assert!(
            collect_bounded_secret_ids(std::iter::empty())
                .unwrap()
                .is_empty()
        );
        let exact = collect_bounded_secret_ids(
            (0..SECRET_PREFIX_ENTRY_LIMIT).map(|index| format!("slot-{index:05}")),
        )
        .unwrap();
        assert_eq!(exact.len(), SECRET_PREFIX_ENTRY_LIMIT);
    }

    #[test]
    fn bounded_inventory_probes_only_limit_plus_one_then_fails_closed() {
        let consumed = Cell::new(0usize);
        let flood = (0..SECRET_PREFIX_ENTRY_PROBE_CEILING + 100).map(|index| {
            consumed.set(consumed.get() + 1);
            format!("slot-{index:05}")
        });
        assert!(collect_bounded_secret_ids(flood).is_err());
        assert_eq!(consumed.get(), SECRET_PREFIX_ENTRY_PROBE_CEILING);
    }
}
