mod redaction;

pub use redaction::{RedactionService, StreamRedactor};

use anyhow::Context;

/// keyring 좌표: service는 앱 번들 ID 고정, username은 credential id.
pub const KEYRING_SERVICE: &str = "app.vector9.deppy-sijo";

/// secret 평문 래퍼. Debug 출력은 항상 REDACTED (설계문서 PR-02 완료 기준).
/// Serialize/Display를 구현하지 않아 config/SQLite/log로의 우발적 유출을 컴파일 단계에서 막는다.
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// 평문 접근. keyring 저장/env 주입 등 명시적 사용처에서만 호출한다.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString(REDACTED)")
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
/// 접근 직렬화(설계문서 1.4): 쓰기/삭제는 UI 스레드, 읽기(get)는 runtime
/// worker 단일 스레드에서만 일어난다 — 동일 credential 동시 접근 없음.
pub trait SecretStore: Send + Sync {
    fn set_secret(&self, id: &str, secret: &SecretString) -> anyhow::Result<()>;
    /// spawn 직전 env 주입에서만 호출 (설계문서 6.3, PR-09)
    fn get_secret(&self, id: &str) -> anyhow::Result<SecretString>;
    fn delete_secret(&self, id: &str) -> anyhow::Result<()>;
    /// secret 존재 여부. 확인된 부재는 Ok(false), 조회 오류(일시 장애 등)는 Err —
    /// "없음"과 "오류"를 구별해야 하는 경로(암호화 키 get-or-create)에서 쓴다.
    fn has_secret(&self, id: &str) -> anyhow::Result<bool>;
}

/// keyring-core 기본 store 기반 구현. 사용 전 플랫폼 store가
/// `keyring_core::set_default_store`로 등록되어 있어야 한다.
pub struct KeyringSecretStore;

/// 설계문서 1.4: 동일 credential 멀티스레드 접근은 신뢰 불가 —
/// UI(set/delete)와 runtime worker(get)가 겹치지 않도록 프로세스 전역 직렬화.
static KEYRING_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl KeyringSecretStore {
    fn entry(&self, id: &str) -> anyhow::Result<keyring_core::Entry> {
        keyring_core::Entry::new(KEYRING_SERVICE, id)
            .with_context(|| format!("keyring entry 생성 실패: {id}"))
    }
}

impl SecretStore for KeyringSecretStore {
    fn set_secret(&self, id: &str, secret: &SecretString) -> anyhow::Result<()> {
        let _serial = KEYRING_SERIAL.lock().expect("keyring serial lock");
        self.entry(id)?
            .set_password(secret.expose())
            .with_context(|| format!("keyring 저장 실패: {id}"))
    }

    fn get_secret(&self, id: &str) -> anyhow::Result<SecretString> {
        let _serial = KEYRING_SERIAL.lock().expect("keyring serial lock");
        let password = self
            .entry(id)?
            .get_password()
            .with_context(|| format!("keyring 조회 실패: {id}"))?;
        Ok(SecretString::new(password))
    }

    fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
        let _serial = KEYRING_SERIAL.lock().expect("keyring serial lock");
        match self.entry(id)?.delete_credential() {
            Ok(()) => Ok(()),
            // 이미 없는 entry 삭제는 성공으로 취급 (metadata/keyring drift 복구 허용)
            Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(e) => Err(e).with_context(|| format!("keyring 삭제 실패: {id}")),
        }
    }

    fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
        let _serial = KEYRING_SERIAL.lock().expect("keyring serial lock");
        match self.entry(id)?.get_password() {
            Ok(_) => Ok(true),
            // 확인된 부재만 false — 그 외 오류는 "없음"으로 오인하면 안 된다 (키 덮어쓰기 방지)
            Err(keyring_core::Error::NoEntry) => Ok(false),
            Err(e) => Err(e).with_context(|| format!("keyring 존재 확인 실패: {id}")),
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
}
