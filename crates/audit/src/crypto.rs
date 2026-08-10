//! input_encrypted_blob AEAD 암호화 (설계문서 7장):
//! - AEAD = XChaCha20-Poly1305 (24바이트 nonce라 랜덤 nonce가 안전)
//! - key는 keyring 보관, key id를 blob에 함께 저장 → rotation 시 재암호화 없이 id로 구분
//!
//! blob 형식: `[u8 key_id_len][key_id utf8][nonce(24)][ciphertext+tag]`.
//! 감사 로그의 redacted json과 별개로 **전체(원본) 입력**을 암호화해 보존한다 —
//! 평시엔 redacted만 보고, 사고 조사 시 key로 원문을 복원한다.

use anyhow::{Context, anyhow, ensure};
use chacha20poly1305::{
    Key,
    XChaCha20Poly1305,
    XNonce,
    // 0.11(aead 0.6)에서 `OsRng` 재노출과 `generate_key`/`generate_nonce`가 사라지고
    // `Generate`로 통합됐다. `generate()`는 시스템 CSPRNG를 쓴다 — 우리가 RNG를
    // 직접 들고 다닐 이유가 없어졌다. **알고리즘·blob 포맷은 그대로다.**
    aead::{Aead, Generate, KeyInit},
};
use secret::hex::{from_hex, to_hex};
use secret::{SecretStore, SecretString};

/// 현재 audit 암호화 키의 keyring entry id. rotation 시 `-2` 등으로 올리면 새 키가
/// 생성되고, 기존 blob은 내장된 옛 id로 계속 복호화된다 (키는 keyring에 남아 있음).
const AUDIT_KEY_ID: &str = "audit-encryption-key-1";

const NONCE_LEN: usize = 24;

/// 키 생성 직렬화 락 — 두 첫-쓰기가 동시에 서로 다른 키를 만들어 덮어쓰는 것 방지.
static KEY_CREATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn read_key(store: &dyn SecretStore) -> anyhow::Result<Vec<u8>> {
    let key = from_hex(store.get_secret(AUDIT_KEY_ID)?.expose()).context("audit 키 hex 손상")?;
    ensure!(key.len() == 32, "audit 키 길이가 32가 아님");
    Ok(key)
}

/// keyring에서 audit 키를 가져오거나 **확인된 부재일 때만** 생성한다. 반환: (key_id, 키).
/// 조회 오류(일시 장애 등)를 "없음"으로 오인해 새 키로 덮어쓰면 기존 blob이 복호 불가가
/// 되므로, 생성은 has_secret==false일 때만 하고 직렬화한다 (codex 리뷰).
fn get_or_create_key(store: &dyn SecretStore) -> anyhow::Result<(String, Vec<u8>)> {
    // 흔한 경로: 이미 있으면 락 없이 읽는다.
    if let Ok(key) = read_key(store) {
        return Ok((AUDIT_KEY_ID.to_owned(), key));
    }
    // 없거나 오류 — 생성/재확인을 직렬화한다.
    let _lock = KEY_CREATE.lock().expect("audit key create lock");
    // 락 안에서 존재를 확실히 확인 (다른 스레드가 방금 만들었거나, 위 읽기가 일시 장애였을 수).
    if store.has_secret(AUDIT_KEY_ID)? {
        return Ok((AUDIT_KEY_ID.to_owned(), read_key(store)?));
    }
    // 확인된 부재 → 새 키 생성·저장.
    let key = chacha20poly1305::Key::generate();
    store.set_secret(AUDIT_KEY_ID, &SecretString::new(to_hex(&key)))?;
    Ok((AUDIT_KEY_ID.to_owned(), key.to_vec()))
}

/// 평문을 AEAD 암호화해 blob으로 만든다. key는 keyring에서 get-or-create.
pub fn encrypt_input(store: &dyn SecretStore, plaintext: &str) -> anyhow::Result<Vec<u8>> {
    let (key_id, key) = get_or_create_key(store)?;
    ensure!(key_id.len() <= u8::MAX as usize, "key_id가 너무 김");
    let key = Key::try_from(key.as_slice()).map_err(|_| anyhow!("audit 키 길이가 32가 아님"))?;
    let cipher = XChaCha20Poly1305::new(&key);
    let nonce = XNonce::generate();
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .map_err(|e| anyhow!("audit 입력 암호화 실패: {e}"))?;

    let mut blob = Vec::with_capacity(1 + key_id.len() + NONCE_LEN + ciphertext.len());
    blob.push(key_id.len() as u8);
    blob.extend_from_slice(key_id.as_bytes());
    blob.extend_from_slice(nonce.as_slice());
    blob.extend_from_slice(&ciphertext);
    Ok(blob)
}

/// blob을 복호화한다. 내장 key_id로 keyring 키를 찾으므로 rotation된 옛 blob도 복원된다.
pub fn decrypt_input(store: &dyn SecretStore, blob: &[u8]) -> anyhow::Result<String> {
    let key_id_len = *blob.first().context("blob이 비어 있음")? as usize;
    let rest = &blob[1..];
    ensure!(
        rest.len() >= key_id_len + NONCE_LEN,
        "blob 길이가 헤더보다 짧음"
    );
    let key_id = std::str::from_utf8(&rest[..key_id_len]).context("key_id가 utf8이 아님")?;
    let nonce = XNonce::try_from(&rest[key_id_len..key_id_len + NONCE_LEN])
        .map_err(|_| anyhow!("nonce 길이가 {NONCE_LEN}이 아님"))?;
    let ciphertext = &rest[key_id_len + NONCE_LEN..];

    let key = from_hex(store.get_secret(key_id)?.expose()).context("audit 키 hex 손상")?;
    ensure!(key.len() == 32, "audit 키 길이가 32가 아님");
    let key = Key::try_from(key.as_slice()).map_err(|_| anyhow!("audit 키 길이가 32가 아님"))?;
    let cipher = XChaCha20Poly1305::new(&key);
    let plaintext = cipher
        .decrypt(&nonce, ciphertext)
        .map_err(|e| anyhow!("audit 입력 복호화 실패(위변조 또는 키 불일치): {e}"))?;
    String::from_utf8(plaintext).context("복호 평문이 utf8이 아님")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 테스트용 인메모리 SecretStore (keyring 불필요).
    #[derive(Default)]
    struct MemStore(Mutex<std::collections::HashMap<String, String>>);

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
                .ok_or_else(|| anyhow!("없음: {id}"))
        }
        fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().remove(id);
            Ok(())
        }
        fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
            Ok(self.0.lock().unwrap().contains_key(id))
        }
    }

    /// **교차 버전 호환성 고정 벡터.**
    ///
    /// 이 blob은 chacha20poly1305 **0.10.1**(구버전)이 만든 것이다 — 고정 키(0x07×32)와
    /// 고정 nonce(0x09×24)로 뽑아 hex로 박아 뒀다. 크레이트를 0.11로 올리면서 API가
    /// 바뀌었는데(`generate_key`/`generate_nonce` → `Generate`), **저장된 감사 원문을
    /// 여전히 읽을 수 있는지**가 이 변경의 진짜 위험이었다.
    ///
    /// XChaCha20-Poly1305는 표준이라 포맷이 같을 «것»이라는 추론에 기대지 않고, 실제
    /// 구버전 산출물로 복호화를 확인한다. 이 테스트가 깨지면 기존 감사 로그를 못 읽는다.
    #[test]
    fn 구버전_0_10이_만든_blob을_그대로_복호화한다() {
        const OLD_BLOB_HEX: &str = "1661756469742d656e6372797074696f6e2d6b65792d3109090909090909090909090909090909090909090909090954c27266fe06598e6f82e390d053778a539fd8fb24aeb9f4a7e6b27be5c48b0ab69ad24931";
        let blob: Vec<u8> = (0..OLD_BLOB_HEX.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&OLD_BLOB_HEX[i..i + 2], 16).expect("hex"))
            .collect();

        // blob 구조: [key_id_len][key_id][nonce(24)][ciphertext+tag]
        let key_id_len = blob[0] as usize;
        let rest = &blob[1..];
        assert_eq!(&rest[..key_id_len], AUDIT_KEY_ID.as_bytes());
        let nonce = XNonce::try_from(&rest[key_id_len..key_id_len + NONCE_LEN]).expect("nonce");
        let ciphertext = &rest[key_id_len + NONCE_LEN..];

        let cipher = XChaCha20Poly1305::new(&Key::from([7u8; 32]));
        let plaintext = cipher
            .decrypt(&nonce, ciphertext)
            .expect("구버전 blob을 새 크레이트로 복호화하지 못했다 — 기존 감사 로그가 유실된다");
        assert_eq!(String::from_utf8(plaintext).unwrap(), "감사 원문 payload");
    }

    #[test]
    fn roundtrip_원문_복원() {
        let store = MemStore::default();
        let plain = r#"{"path":"/etc/secret","token":"sk-abc123"}"#;
        let blob = encrypt_input(&store, plain).unwrap();
        assert!(!blob.is_empty());
        // 원문이 blob에 평문으로 남아 있지 않아야 한다
        assert!(!String::from_utf8_lossy(&blob).contains("sk-abc123"));
        assert_eq!(decrypt_input(&store, &blob).unwrap(), plain);
    }

    #[test]
    fn 위변조_감지() {
        let store = MemStore::default();
        let mut blob = encrypt_input(&store, "hello").unwrap();
        // ciphertext 마지막 바이트 뒤집기 → AEAD 인증 실패
        let last = blob.len() - 1;
        blob[last] ^= 0xff;
        assert!(decrypt_input(&store, &blob).is_err());
    }

    #[test]
    fn rotation_옛_blob은_옛_key_id로_복호() {
        let store = MemStore::default();
        // 현재 키로 암호화
        let blob = encrypt_input(&store, "old data").unwrap();
        // 키를 "회전" — 같은 id의 키를 다른 값으로 바꾸면 복호 실패해야 한다(위변조/키불일치),
        // 그러나 다른 id면 옛 blob은 그대로 복호된다. 여기선 id 내장 검증:
        // blob 헤더의 key_id가 현재 AUDIT_KEY_ID와 일치.
        let key_id_len = blob[0] as usize;
        let key_id = std::str::from_utf8(&blob[1..1 + key_id_len]).unwrap();
        assert_eq!(key_id, AUDIT_KEY_ID);
        // 키가 keyring에 있는 한 복호 가능
        assert_eq!(decrypt_input(&store, &blob).unwrap(), "old data");
    }
}
