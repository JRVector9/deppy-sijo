//! 브라우저 페어링 토큰 — keyring 영속 (tls_identity 관례: **확인된 부재**에만 생성,
//! keyring 오류는 부재로 오판하지 않고 bail — 살아있는 토큰을 덮어쓰지 않는다).
//! 재시작 후에도 같은 토큰이라 폰이 재페어링하지 않아도 된다.

use anyhow::Context;
use secret::{SecretStore, SecretString};

/// keyring entry id. rotation 시 `-2`로 올린다 (tls_identity·audit key와 동일 관례).
const WEB_TOKEN_ID: &str = "web-remote-token-1";

/// 저장된 페어링 토큰을 읽고, 확인된 부재면 새로 만들어 저장한다.
/// GUI 단일 스레드(설정 UI)에서만 부른다 — 생성 경합 직렬화는 불필요.
pub fn get_or_create_token(store: &dyn SecretStore) -> anyhow::Result<String> {
    if store
        .has_secret(WEB_TOKEN_ID)
        .context("페어링 토큰 존재 확인 실패")?
    {
        return Ok(store
            .get_secret(WEB_TOKEN_ID)
            .context("페어링 토큰 읽기 실패")?
            .expose()
            .to_owned());
    }
    rotate_token(store)
}

/// 토큰 재발급 — 새 토큰을 만들어 저장한다. 기존 페어링(QR/브라우저 저장분)은 무효가 된다.
pub fn rotate_token(store: &dyn SecretStore) -> anyhow::Result<String> {
    let token = new_token();
    store
        .set_secret(WEB_TOKEN_ID, &SecretString::new(token.clone()))
        .context("페어링 토큰 저장 실패")?;
    Ok(token)
}

/// 32바이트 랜덤(uuid v4 ×2 ≈ 244bit 엔트로피) hex 64자 — remote.rs new_auth_token 관례.
/// hex라 URL/QR에 인코딩 없이 실을 수 있다.
fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// 폰이 열 접속 URL. ts.net 호스트명이 있으면 tailscale HTTPS(serve가 443 종단),
/// 없으면 로컬 확인용 평문 URL. 토큰이 실리므로 표시는 마스킹하고 전달은 QR/복사로 한다.
pub fn access_url(hostname: Option<&str>, port: u16, token: &str) -> String {
    match hostname.map(str::trim).filter(|host| !host.is_empty()) {
        Some(host) => format!("https://{host}/?token={token}"),
        None => format!("http://127.0.0.1:{port}/?token={token}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// in-memory SecretStore — 실제 keyring을 건드리지 않는다 (tls_identity 테스트 관례).
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
                .map(|value| SecretString::new(value.clone()))
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

    #[test]
    fn 최초_생성_후_재호출은_같은_토큰() {
        let store = MemStore::default();
        let first = get_or_create_token(&store).unwrap();
        let second = get_or_create_token(&store).unwrap();
        assert_eq!(first, second);
        // hex 64자 (32바이트 상당) + URL-safe
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn 재발급은_새_토큰으로_교체한다() {
        let store = MemStore::default();
        let first = get_or_create_token(&store).unwrap();
        let rotated = rotate_token(&store).unwrap();
        assert_ne!(first, rotated);
        // 이후 조회는 새 토큰
        assert_eq!(get_or_create_token(&store).unwrap(), rotated);
    }

    #[test]
    fn keyring_오류는_부재로_오판하지_않고_실패한다() {
        let err = get_or_create_token(&BrokenStore).unwrap_err();
        assert!(format!("{err:#}").contains("keyring"), "{err:#}");
    }

    #[test]
    fn 접속_url은_호스트명_유무로_https와_로컬을_가른다() {
        assert_eq!(
            access_url(Some("mac.tail.ts.net"), 8737, "tok"),
            "https://mac.tail.ts.net/?token=tok"
        );
        // 공백/빈 호스트명은 미설정으로 취급
        assert_eq!(
            access_url(Some("  "), 8737, "tok"),
            "http://127.0.0.1:8737/?token=tok"
        );
        assert_eq!(
            access_url(None, 9999, "tok"),
            "http://127.0.0.1:9999/?token=tok"
        );
    }
}
