//! remote TLS 서버 신원 (설계doc remote-tls-delta §2.2, 단계 C-1).
//!
//! 자기서명 인증서 + TOFU 지문 핀닝 모델의 서버 측 절반:
//!   - **개인키는 keyring에만** 저장한다 (§1.4 — 평문 디스크 금지). PKCS#8 DER를 hex로.
//!   - **인증서(공개)는 디스크**(DER)에 둔다 — 클라이언트에 지문을 대역외 전달할 때
//!     사용자가 파일로 확인할 수 있다.
//!   - 지문 = SHA-256(cert DER), "ab:cd:…" hex 표기 — C-3 TOFU 핀닝의 신뢰 앵커.
//!
//! 재생성 규칙: 키의 **확인된 부재**(has_secret=false) 또는 cert 파일 부재 → 키+cert를
//! 쌍으로 재생성한다(자기서명이라 cert만 재생성해도 지문이 바뀌므로 쌍이 단순·동등).
//! 지문이 바뀌면 클라이언트는 재-TOFU해야 한다. keyring **오류**는 부재와 구분해 bail —
//! 일시 장애를 부재로 오판해 살아있는 키를 덮어쓰지 않는다 (audit crypto와 동일 원칙).
//!
//! 만료: rcgen 기본 유효기간을 그대로 쓴다. TOFU 모델에서 검증 기준은 지문이며(SSH처럼),
//! 만료 처리 정책은 C-3 verifier 소관 (설계doc Open Question 2).

use std::path::Path;

use anyhow::Context;
use secret::hex::{from_hex, to_hex};
use secret::{SecretStore, SecretString};
use sha2::{Digest, Sha256};

/// 현재 TLS 개인키의 keyring entry id. rotation 시 `-2`로 올린다 (audit key와 동일 관례).
const TLS_KEY_ID: &str = "remote-tls-key-1";

/// 키+cert 쌍 생성 직렬화 — 두 스레드가 동시에 부재를 보고 서로 다른 키를 만드는 것 방지.
static IDENTITY_CREATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 서버 TLS 신원: rustls에 넘길 cert/key DER 쌍 (C-2에서 소비).
pub struct TlsIdentity {
    /// 인증서 DER (공개 — 디스크 파일과 동일 바이트).
    pub cert_der: Vec<u8>,
    /// PKCS#8 개인키 DER. keyring에서 읽은 값 — 디스크에 쓰지 말 것.
    pub key_der: Vec<u8>,
}

impl TlsIdentity {
    /// TOFU 핀닝용 지문: SHA-256(cert DER)를 "ab:cd:…"로.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.cert_der)
    }
}

impl std::fmt::Debug for TlsIdentity {
    /// key_der는 개인키이므로 Debug에 싣지 않는다 (유출 방지) — 지문만 노출.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsIdentity")
            .field("fingerprint", &self.fingerprint())
            .field("key_der", &"<elided>")
            .finish()
    }
}

/// cert DER의 SHA-256 지문을 "ab:cd:…" hex로.
pub fn fingerprint(cert_der: &[u8]) -> String {
    let digest = Sha256::digest(cert_der);
    let hex: Vec<String> = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex.join(":")
}

/// keyring 키 + 디스크 cert를 로드하고, 확인된 부재면 쌍으로 새로 만든다.
pub fn get_or_create_identity(
    store: &dyn SecretStore,
    cert_path: &Path,
) -> anyhow::Result<TlsIdentity> {
    // 빠른 경로: 둘 다 있으면 그대로 로드 (생성 락 불필요)
    if store.has_secret(TLS_KEY_ID)? && cert_path.is_file() {
        return load_identity(store, cert_path);
    }

    let _lock = IDENTITY_CREATE.lock().expect("tls identity create lock");
    // 프로세스 간 락 — GUI와 deppy-mcp-proxy가 같은 data dir을 공유하므로, 두 프로세스가
    // 동시에 최초 생성에 들어와 keyring 키(A)와 디스크 cert(B)가 인터리브로 미스매치되는
    // 것을 막는다 (codex P2). in-process Mutex는 스레드만 직렬화하므로 파일락을 겹친다.
    let lock_path = cert_path.with_extension("crt.lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cert 디렉터리 생성 실패: {}", parent.display()))?;
    }
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("TLS 신원 락 파일 열기 실패: {}", lock_path.display()))?;
    lock_file
        .lock()
        .with_context(|| format!("TLS 신원 락 획득 실패: {}", lock_path.display()))?;
    // 락 안에서 재확인 — 다른 스레드/프로세스가 방금 만들었을 수 있다
    if store.has_secret(TLS_KEY_ID)? && cert_path.is_file() {
        return load_identity(store, cert_path);
    }

    // 확인된 부재(키 또는 cert) → 쌍 재생성. 지문이 바뀌므로 클라이언트는 재-TOFU.
    // stale cert를 먼저 지운다 — 아래 새 키 저장 후 cert 쓰기가 실패해도, 다음 호출이
    // (키 있음 + cert 없음) → 재생성 경로로 복구되지 (키 있음 + 옛 cert)로 미스매치
    // 신원을 로드하지 않는다 (codex P2).
    if cert_path.is_file() {
        std::fs::remove_file(cert_path)
            .with_context(|| format!("stale cert 제거 실패: {}", cert_path.display()))?;
    }
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(vec!["deppy-remote".to_owned()])
            .context("자기서명 TLS 인증서 생성 실패")?;
    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialize_der();

    // 키를 먼저 keyring에 확정하고 나서 cert를 디스크에 쓴다 — cert 쓰기가 실패해도
    // 다음 호출이 (키 있음 + cert 없음) → 재생성 경로로 자연 복구된다.
    store
        .set_secret(TLS_KEY_ID, &SecretString::new(to_hex(&key_der)))
        .context("TLS 개인키 keyring 저장 실패")?;
    if let Some(parent) = cert_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cert 디렉터리 생성 실패: {}", parent.display()))?;
    }
    // tmp+rename 원자 쓰기 — 중단/디스크 오류로 빈/부분 DER가 cert_path에 남아
    // fast-path(is_file)가 손상 cert를 로드하는 일이 없게 한다 (codex P2).
    deppy_core::fs::atomic_write(cert_path, &cert_der)
        .with_context(|| format!("cert 원자 기록 실패: {}", cert_path.display()))?;

    Ok(TlsIdentity { cert_der, key_der })
}

fn load_identity(store: &dyn SecretStore, cert_path: &Path) -> anyhow::Result<TlsIdentity> {
    let key_hex = store
        .get_secret(TLS_KEY_ID)
        .context("TLS 개인키 keyring 읽기 실패")?;
    let key_der = from_hex(key_hex.expose()).context("keyring의 TLS 키가 hex가 아님")?;
    let cert_der = std::fs::read(cert_path)
        .with_context(|| format!("cert 읽기 실패: {}", cert_path.display()))?;
    Ok(TlsIdentity { cert_der, key_der })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// in-memory SecretStore — 실제 keyring을 건드리지 않는다.
    #[derive(Default)]
    struct MemStore(Mutex<HashMap<String, String>>);

    impl SecretStore for MemStore {
        fn set_secret(&self, id: &str, secret: &SecretString) -> anyhow::Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert(id.to_owned(), secret.expose().to_owned());
            Ok(())
        }
        fn get_secret(&self, id: &str) -> anyhow::Result<SecretString> {
            self.0
                .lock()
                .unwrap()
                .get(id)
                .map(|v| SecretString::new(v.clone()))
                .context("없음")
        }
        fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().remove(id);
            Ok(())
        }
        fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
            Ok(self.0.lock().unwrap().contains_key(id))
        }
    }

    /// 모든 호출이 실패하는 store — keyring 장애 시나리오.
    struct BrokenStore;
    impl SecretStore for BrokenStore {
        fn set_secret(&self, _: &str, _: &SecretString) -> anyhow::Result<()> {
            anyhow::bail!("keyring 장애")
        }
        fn get_secret(&self, _: &str) -> anyhow::Result<SecretString> {
            anyhow::bail!("keyring 장애")
        }
        fn delete_secret(&self, _: &str) -> anyhow::Result<()> {
            anyhow::bail!("keyring 장애")
        }
        fn has_secret(&self, _: &str) -> anyhow::Result<bool> {
            anyhow::bail!("keyring 장애")
        }
    }

    fn temp_cert_path(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("deppy-tls-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("remote-tls.crt")
    }

    #[test]
    fn 최초_생성은_키와_cert를_만들고_지문형식이_맞다() {
        let store = MemStore::default();
        let path = temp_cert_path("create");
        let id = get_or_create_identity(&store, &path).unwrap();
        assert!(store.has_secret(TLS_KEY_ID).unwrap());
        assert!(path.is_file());
        // 지문: 32바이트 → "ab:cd:…" (95자)
        let fp = id.fingerprint();
        assert_eq!(fp.len(), 95, "{fp}");
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit() || c == ':'));
        // 디스크 cert와 메모리 cert가 같은 바이트 (지문 일치)
        assert_eq!(std::fs::read(&path).unwrap(), id.cert_der);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn 재호출은_같은_신원을_돌려준다() {
        let store = MemStore::default();
        let path = temp_cert_path("stable");
        let first = get_or_create_identity(&store, &path).unwrap();
        let second = get_or_create_identity(&store, &path).unwrap();
        assert_eq!(first.fingerprint(), second.fingerprint());
        assert_eq!(first.key_der, second.key_der);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn cert_파일이_사라지면_쌍으로_재생성돼_지문이_바뀐다() {
        let store = MemStore::default();
        let path = temp_cert_path("regen");
        let first = get_or_create_identity(&store, &path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let second = get_or_create_identity(&store, &path).unwrap();
        assert_ne!(first.fingerprint(), second.fingerprint());
        assert!(path.is_file());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn 손상된_비ascii_keyring값은_패닉없이_err() {
        let store = MemStore::default();
        store
            .set_secret(TLS_KEY_ID, &SecretString::new("aéa0".to_owned()))
            .unwrap();
        let path = temp_cert_path("corrupt");
        std::fs::write(&path, b"dummy-cert").unwrap();
        let err = get_or_create_identity(&store, &path).unwrap_err();
        assert!(format!("{err:#}").contains("hex"), "{err:#}");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn 키만_사라지면_stale_cert를_지우고_쌍_재생성() {
        let store = MemStore::default();
        let path = temp_cert_path("stale");
        let first = get_or_create_identity(&store, &path).unwrap();
        // keyring 키만 소실 (cert는 남음) — 옛 cert+새 키 미스매치가 생기면 안 된다
        store.delete_secret(TLS_KEY_ID).unwrap();
        let second = get_or_create_identity(&store, &path).unwrap();
        assert_ne!(first.fingerprint(), second.fingerprint());
        // 디스크 cert가 새 신원과 일치 (stale 아님)
        assert_eq!(std::fs::read(&path).unwrap(), second.cert_der);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn keyring_오류는_부재로_오판하지_않고_실패한다() {
        let path = temp_cert_path("broken");
        let err = get_or_create_identity(&BrokenStore, &path).unwrap_err();
        assert!(format!("{err:#}").contains("keyring"), "{err:#}");
        // cert도 쓰지 않았어야 한다 (키 확정 전 cert 쓰기 금지)
        assert!(!path.is_file());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
