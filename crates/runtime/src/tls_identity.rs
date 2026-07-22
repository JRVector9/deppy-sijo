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

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::Path;

use anyhow::Context;
use secret::hex::{from_hex, to_hex};
use secret::{SecretStore, SecretString};
use sha2::{Digest, Sha256};

/// 현재 TLS 개인키의 keyring entry id. rotation 시 `-2`로 올린다 (audit key와 동일 관례).
const TLS_KEY_ID: &str = "remote-tls-key-1";

/// Deppy가 생성하는 단일-SAN self-signed leaf는 1 KiB 미만이다. 향후 알고리즘/extension
/// 여유를 넉넉히 두되 손상·교체 파일이 RAM을 점유하지 못하도록 DER를 64 KiB로 제한한다.
const MAX_CERT_DER_BYTES: usize = 64 * 1024;
const MIN_CERT_DER_BYTES: usize = 128;

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
    /// key DER와 인증서/지문을 Debug에 싣지 않는다. 지문은 명시적 `fingerprint()` 호출로만
    /// 얻어 diagnostics의 우발적 trust-anchor 노출을 막는다.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsIdentity")
            .field("cert_der", &"[PUBLIC CERT ELIDED]")
            .field("key_der", &"[REDACTED]")
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
    let key_exists = store.has_secret(TLS_KEY_ID)?;
    let cert_state = cert_file_state(cert_path)?;
    if key_exists && cert_state == CertFileState::Regular {
        return load_identity(store, cert_path);
    }

    let _lock = IDENTITY_CREATE.lock().expect("tls identity create lock");
    // 프로세스 간 락 — GUI와 deppy-mcp-proxy가 같은 data dir을 공유하므로, 두 프로세스가
    // 동시에 최초 생성에 들어와 keyring 키(A)와 디스크 cert(B)가 인터리브로 미스매치되는
    // 것을 막는다 (codex P2). in-process Mutex는 스레드만 직렬화하므로 파일락을 겹친다.
    let lock_path = cert_path.with_extension("crt.lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| static_error("tls_identity_create_dir_failed"))?;
    }
    let lock_file = open_identity_lock(&lock_path)?;
    lock_file
        .lock()
        .map_err(|_| static_error("tls_identity_lock_failed"))?;
    validate_open_lock(&lock_file, &lock_path)?;
    // 락 안에서 재확인 — 다른 스레드/프로세스가 방금 만들었을 수 있다
    let key_exists = store.has_secret(TLS_KEY_ID)?;
    let cert_state = cert_file_state(cert_path)?;
    if key_exists && cert_state == CertFileState::Regular {
        return load_identity(store, cert_path);
    }

    // 확인된 부재(키 또는 cert) → 쌍 재생성. 지문이 바뀌므로 클라이언트는 재-TOFU.
    // stale cert를 먼저 지운다 — 아래 새 키 저장 후 cert 쓰기가 실패해도, 다음 호출이
    // (키 있음 + cert 없음) → 재생성 경로로 복구되지 (키 있음 + 옛 cert)로 미스매치
    // 신원을 로드하지 않는다 (codex P2).
    if cert_state == CertFileState::Regular {
        std::fs::remove_file(cert_path)
            .map_err(|_| static_error("tls_identity_stale_cert_remove_failed"))?;
    }
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(vec!["deppy-remote".to_owned()])
            .context("자기서명 TLS 인증서 생성 실패")?;
    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialize_der();
    validate_cert_der(&cert_der)?;

    // 키를 먼저 keyring에 확정하고 나서 cert를 디스크에 쓴다 — cert 쓰기가 실패해도
    // 다음 호출이 (키 있음 + cert 없음) → 재생성 경로로 자연 복구된다.
    store
        .set_secret(TLS_KEY_ID, &SecretString::new(to_hex(&key_der)))
        .context("TLS 개인키 keyring 저장 실패")?;
    if let Some(parent) = cert_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| static_error("tls_identity_create_dir_failed"))?;
    }
    // tmp+rename 원자 쓰기 — 중단/디스크 오류로 빈/부분 DER가 cert_path에 남아
    // fast-path(is_file)가 손상 cert를 로드하는 일이 없게 한다 (codex P2).
    deppy_core::fs::atomic_write(cert_path, &cert_der)
        .map_err(|_| static_error("tls_identity_cert_atomic_write_failed"))?;

    Ok(TlsIdentity { cert_der, key_der })
}

fn load_identity(store: &dyn SecretStore, cert_path: &Path) -> anyhow::Result<TlsIdentity> {
    let cert_der = read_cert_der_stable(cert_path)?;
    let key_hex = store
        .get_secret(TLS_KEY_ID)
        .context("TLS 개인키 keyring 읽기 실패")?;
    let key_der = from_hex(key_hex.expose()).context("keyring의 TLS 키가 hex가 아님")?;
    Ok(TlsIdentity { cert_der, key_der })
}

fn open_identity_lock(path: &Path) -> anyhow::Result<File> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !is_stable_regular_file(&metadata) => {
            return Err(static_error("tls_identity_lock_not_regular"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(static_error("tls_identity_lock_metadata_failed")),
    }

    let mut options = OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .map_err(|_| static_error("tls_identity_lock_open_failed"))?;
    validate_open_lock(&file, path)?;
    Ok(file)
}

fn validate_open_lock(file: &File, path: &Path) -> anyhow::Result<()> {
    let handle_metadata = file
        .metadata()
        .map_err(|_| static_error("tls_identity_lock_metadata_failed"))?;
    if !is_stable_regular_file(&handle_metadata) {
        return Err(static_error("tls_identity_lock_not_regular"));
    }
    let path_metadata =
        std::fs::symlink_metadata(path).map_err(|_| static_error("tls_identity_lock_replaced"))?;
    if !stable_metadata_matches(&handle_metadata, &path_metadata) {
        return Err(static_error("tls_identity_lock_replaced"));
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CertFileState {
    Missing,
    Regular,
}

/// Path 존재 판정도 symlink를 따라가지 않는다. dangling symlink/special file을 "missing"으로
/// 오인해 identity를 덮어쓰지 않고 fail-closed한다.
fn cert_file_state(path: &Path) -> anyhow::Result<CertFileState> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(CertFileState::Regular),
        Ok(_) => Err(static_error("tls_identity_cert_not_regular")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(CertFileState::Missing),
        Err(_) => Err(static_error("tls_identity_cert_metadata_failed")),
    }
}

fn read_cert_der_stable(path: &Path) -> anyhow::Result<Vec<u8>> {
    read_cert_der_stable_with_hook(path, || {})
}

fn read_cert_der_stable_with_hook(
    path: &Path,
    after_admission: impl FnOnce(),
) -> anyhow::Result<Vec<u8>> {
    let mut file = open_cert_nofollow(path)?;
    let before = file
        .metadata()
        .map_err(|_| static_error("tls_identity_cert_metadata_failed"))?;
    if !is_stable_regular_file(&before) {
        return Err(static_error("tls_identity_cert_not_regular"));
    }
    let declared_len = usize::try_from(before.len())
        .map_err(|_| static_error("tls_identity_cert_size_exceeded"))?;
    if declared_len > MAX_CERT_DER_BYTES {
        return Err(static_error("tls_identity_cert_size_exceeded"));
    }

    // Test hook also documents the admitted-file race boundary. Production passes a no-op.
    after_admission();

    let mut bytes = vec![0u8; declared_len];
    file.read_exact(&mut bytes)
        .map_err(|_| static_error("tls_identity_cert_changed_during_read"))?;
    let mut plus_one = [0u8; 1];
    let extra = file
        .read(&mut plus_one)
        .map_err(|_| static_error("tls_identity_cert_read_failed"))?;
    if extra != 0 {
        return Err(static_error("tls_identity_cert_changed_during_read"));
    }

    let after = file
        .metadata()
        .map_err(|_| static_error("tls_identity_cert_metadata_failed"))?;
    let path_after =
        std::fs::symlink_metadata(path).map_err(|_| static_error("tls_identity_cert_replaced"))?;
    if !stable_metadata_matches(&before, &after) || !stable_metadata_matches(&after, &path_after) {
        return Err(static_error("tls_identity_cert_replaced"));
    }
    validate_cert_der(&bytes)?;
    Ok(bytes)
}

fn open_cert_nofollow(path: &Path) -> anyhow::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT: open the link itself so metadata admission can reject it.
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options
        .open(path)
        .map_err(|_| static_error("tls_identity_cert_open_failed"))
}

fn is_stable_regular_file(metadata: &std::fs::Metadata) -> bool {
    if !metadata.file_type().is_file() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
}

#[cfg(unix)]
fn stable_metadata_matches(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    is_stable_regular_file(left)
        && is_stable_regular_file(right)
        && left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(windows)]
fn stable_metadata_matches(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    is_stable_regular_file(left)
        && is_stable_regular_file(right)
        && left.file_size() == right.file_size()
        && left.creation_time() == right.creation_time()
        && left.last_write_time() == right.last_write_time()
        && left.file_attributes() == right.file_attributes()
}

#[cfg(not(any(unix, windows)))]
fn stable_metadata_matches(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    is_stable_regular_file(left)
        && is_stable_regular_file(right)
        && left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
}

/// 전체 X.509 파서를 중복하지 않되, DER Certificate의 canonical outer SEQUENCE와 정확한
/// length envelope를 검증한다. 실제 key/cert 적합성은 소비 시 rustls가 추가 검증한다.
fn validate_cert_der(cert_der: &[u8]) -> anyhow::Result<()> {
    if cert_der.len() < MIN_CERT_DER_BYTES || cert_der.len() > MAX_CERT_DER_BYTES {
        return Err(static_error("tls_identity_cert_size_invalid"));
    }
    if cert_der.first() != Some(&0x30) {
        return Err(static_error("tls_identity_cert_invalid_der"));
    }
    let (header_len, content_len) = decode_der_length(&cert_der[1..])?;
    let content_start = 1usize
        .checked_add(header_len)
        .ok_or_else(|| static_error("tls_identity_cert_invalid_der"))?;
    let total_len = content_start
        .checked_add(content_len)
        .ok_or_else(|| static_error("tls_identity_cert_invalid_der"))?;
    if total_len != cert_der.len() || cert_der.get(content_start) != Some(&0x30) {
        return Err(static_error("tls_identity_cert_invalid_der"));
    }
    Ok(())
}

fn decode_der_length(bytes: &[u8]) -> anyhow::Result<(usize, usize)> {
    let first = *bytes
        .first()
        .ok_or_else(|| static_error("tls_identity_cert_invalid_der"))?;
    if first < 0x80 {
        return Ok((1, usize::from(first)));
    }
    let count = usize::from(first & 0x7f);
    if count == 0 || count > std::mem::size_of::<usize>() || bytes.len() <= count {
        return Err(static_error("tls_identity_cert_invalid_der"));
    }
    let length_bytes = &bytes[1..=count];
    if length_bytes.first() == Some(&0) {
        return Err(static_error("tls_identity_cert_invalid_der"));
    }
    let mut length = 0usize;
    for byte in length_bytes {
        length = length
            .checked_mul(256)
            .and_then(|value| value.checked_add(usize::from(*byte)))
            .ok_or_else(|| static_error("tls_identity_cert_invalid_der"))?;
    }
    if length < 128 {
        return Err(static_error("tls_identity_cert_invalid_der"));
    }
    Ok((1 + count, length))
}

fn static_error(code: &'static str) -> anyhow::Error {
    anyhow::Error::msg(code)
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
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("deppy-tls-{}-{tag}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("remote-tls.crt")
    }

    fn synthetic_cert_der(total_len: usize) -> Vec<u8> {
        assert!((132..=65_539).contains(&total_len));
        let content_len = total_len - 4;
        let mut cert = vec![0u8; total_len];
        cert[0] = 0x30;
        cert[1] = 0x82;
        cert[2] = (content_len >> 8) as u8;
        cert[3] = content_len as u8;
        cert[4] = 0x30;
        cert
    }

    #[test]
    fn 최초_생성은_키와_cert를_만들고_지문형식이_맞다() {
        let store = MemStore::default();
        let path = temp_cert_path("create");
        let id = get_or_create_identity(&store, &path).unwrap();
        assert!(store.has_secret(TLS_KEY_ID).unwrap());
        assert!(path.is_file());
        assert!(id.cert_der.len() < 1024);
        assert!(id.cert_der.len() <= MAX_CERT_DER_BYTES);
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
    fn 반복_reload는_cert_buffer를_상한내에서_매번_반환하고_보유하지_않는다() {
        let store = MemStore::default();
        let path = temp_cert_path("repeated");
        let first = get_or_create_identity(&store, &path).unwrap();
        let expected_fingerprint = first.fingerprint();
        let expected_len = first.cert_der.len();
        drop(first);
        for _ in 0..64 {
            let identity = get_or_create_identity(&store, &path).unwrap();
            assert_eq!(identity.fingerprint(), expected_fingerprint);
            assert_eq!(identity.cert_der.len(), expected_len);
            assert!(identity.cert_der.capacity() <= MAX_CERT_DER_BYTES);
        }
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
        std::fs::write(&path, synthetic_cert_der(512)).unwrap();
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

    #[test]
    fn cert_size_zero_exact_plus_one_contract() {
        let path = temp_cert_path("size-limits");
        std::fs::write(&path, []).unwrap();
        assert_eq!(
            read_cert_der_stable(&path).unwrap_err().to_string(),
            "tls_identity_cert_size_invalid"
        );

        let exact = synthetic_cert_der(MAX_CERT_DER_BYTES);
        std::fs::write(&path, &exact).unwrap();
        assert_eq!(read_cert_der_stable(&path).unwrap(), exact);

        std::fs::write(&path, synthetic_cert_der(MAX_CERT_DER_BYTES + 1)).unwrap();
        assert_eq!(
            read_cert_der_stable(&path).unwrap_err().to_string(),
            "tls_identity_cert_size_exceeded"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn malformed_der는_fail_closed한다() {
        let store = MemStore::default();
        let path = temp_cert_path("malformed");
        let _ = get_or_create_identity(&store, &path).unwrap();
        std::fs::write(&path, vec![0x41; MIN_CERT_DER_BYTES]).unwrap();
        assert_eq!(
            get_or_create_identity(&store, &path)
                .unwrap_err()
                .to_string(),
            "tls_identity_cert_invalid_der"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink와_special_file은_부재로_오인하지_않는다() {
        use std::os::unix::fs::symlink;

        let store = MemStore::default();
        let path = temp_cert_path("symlink");
        let _ = get_or_create_identity(&store, &path).unwrap();
        let target = path.with_extension("target");
        std::fs::rename(&path, &target).unwrap();
        symlink(&target, &path).unwrap();
        assert_eq!(
            get_or_create_identity(&store, &path)
                .unwrap_err()
                .to_string(),
            "tls_identity_cert_not_regular"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();

        let special_path = temp_cert_path("special");
        std::fs::create_dir(&special_path).unwrap();
        assert_eq!(
            get_or_create_identity(&MemStore::default(), &special_path)
                .unwrap_err()
                .to_string(),
            "tls_identity_cert_not_regular"
        );
        std::fs::remove_dir_all(special_path.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn lock_symlink_fifo_directory는_block없이_fail_closed한다() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::symlink;

        let cert_path = temp_cert_path("lock-symlink");
        let lock_path = cert_path.with_extension("crt.lock");
        let target = cert_path.with_extension("lock-target");
        std::fs::write(&target, b"lock").unwrap();
        symlink(&target, &lock_path).unwrap();
        assert_eq!(
            get_or_create_identity(&MemStore::default(), &cert_path)
                .unwrap_err()
                .to_string(),
            "tls_identity_lock_not_regular"
        );
        std::fs::remove_dir_all(cert_path.parent().unwrap()).unwrap();

        let cert_path = temp_cert_path("lock-fifo");
        let lock_path = cert_path.with_extension("crt.lock");
        let lock_path_c = std::ffi::CString::new(lock_path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(lock_path_c.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        assert_eq!(
            get_or_create_identity(&MemStore::default(), &cert_path)
                .unwrap_err()
                .to_string(),
            "tls_identity_lock_not_regular"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        std::fs::remove_dir_all(cert_path.parent().unwrap()).unwrap();

        let cert_path = temp_cert_path("lock-directory");
        let lock_path = cert_path.with_extension("crt.lock");
        std::fs::create_dir(&lock_path).unwrap();
        assert_eq!(
            get_or_create_identity(&MemStore::default(), &cert_path)
                .unwrap_err()
                .to_string(),
            "tls_identity_lock_not_regular"
        );
        std::fs::remove_dir_all(cert_path.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn replacement_growth_shrink는_stable_read를_실패시킨다() {
        use std::io::Write;

        let replacement_path = temp_cert_path("replacement");
        std::fs::write(&replacement_path, synthetic_cert_der(512)).unwrap();
        let replacement = replacement_path.with_extension("new");
        std::fs::write(&replacement, synthetic_cert_der(512)).unwrap();
        assert_eq!(
            read_cert_der_stable_with_hook(&replacement_path, || {
                std::fs::rename(&replacement, &replacement_path).unwrap();
            })
            .unwrap_err()
            .to_string(),
            "tls_identity_cert_replaced"
        );
        std::fs::remove_dir_all(replacement_path.parent().unwrap()).unwrap();

        let growth_path = temp_cert_path("growth");
        std::fs::write(&growth_path, synthetic_cert_der(512)).unwrap();
        assert_eq!(
            read_cert_der_stable_with_hook(&growth_path, || {
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&growth_path)
                    .unwrap()
                    .write_all(&[0])
                    .unwrap();
            })
            .unwrap_err()
            .to_string(),
            "tls_identity_cert_changed_during_read"
        );
        std::fs::remove_dir_all(growth_path.parent().unwrap()).unwrap();

        let shrink_path = temp_cert_path("shrink");
        std::fs::write(&shrink_path, synthetic_cert_der(512)).unwrap();
        assert_eq!(
            read_cert_der_stable_with_hook(&shrink_path, || {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&shrink_path)
                    .unwrap()
                    .set_len(256)
                    .unwrap();
            })
            .unwrap_err()
            .to_string(),
            "tls_identity_cert_changed_during_read"
        );
        std::fs::remove_dir_all(shrink_path.parent().unwrap()).unwrap();
    }

    #[test]
    fn identity_debug는_cert_fingerprint_key를_노출하지_않는다() {
        let identity = TlsIdentity {
            cert_der: synthetic_cert_der(512),
            key_der: b"private-key-marker".to_vec(),
        };
        let fingerprint = identity.fingerprint();
        let debug = format!("{identity:?}");
        assert!(!debug.contains(&fingerprint));
        assert!(!debug.contains("private-key-marker"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn production_source_has_bounded_nofollow_reads_and_sanitized_diagnostics() {
        let production = include_str!("tls_identity.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        for forbidden in [
            "std::fs::read(",
            "cert_path.is_file()",
            ".display()",
            "read_to_end",
            "read_to_string",
            "String::from_utf8_lossy",
            "tracing::",
        ] {
            assert!(!production.contains(forbidden), "found {forbidden}");
        }
        assert!(production.contains("MAX_CERT_DER_BYTES"));
        assert!(production.contains("plus_one"));
        assert!(production.contains("libc::O_NOFOLLOW"));
        assert!(production.contains("libc::O_NONBLOCK"));
        assert!(production.contains("FILE_FLAG_OPEN_REPARSE_POINT"));
        assert!(production.contains("open_identity_lock"));
        assert!(production.contains("validate_open_lock"));
        assert!(!production.contains(".open(&lock_path)"));
    }
}
