//! PR-18 OAuth PKCE Connector (설계문서 §1.5 v1+ OAuth, §9 Auth 모듈).
//! external browser → localhost callback → PKCE(필수) + state 검증 → token keyring 저장.
//!
//! flow를 begin/complete로 쪼개 두었다 — 브라우저·네트워크 없이도
//! PKCE/state 규칙을 테스트할 수 있고, UI는 [`run_flow`] 하나만 부른다.
//!
//! PR-H4: 401에서 토큰 획득까지의 네트워크 프리미티브 추가 —
//! WWW-Authenticate 파서([`parse_www_authenticate`]), RFC 9728/8414 발견
//! ([`discover_protected_resource`]/[`discover_authorization_server`]),
//! RFC 7591 DCR([`register_client`]), refresh([`refresh_access_token`]).
//! 401 사다리 조립과 UI 배선은 H5 몫.

mod browser;
mod callback;
mod discovery;
mod flow;
mod refresh;
mod registration;
#[cfg(test)]
mod test_support;
mod www_authenticate;

pub use browser::open_in_browser;
pub use callback::{
    CallbackParams, FIXED_CALLBACK_PORT, LocalhostCallbackServer, registration_redirect_uris,
};
pub use discovery::{
    AuthorizationServerMetadata, DiscoveryHeaders, ProtectedResourceMetadata,
    discover_authorization_server, discover_protected_resource,
};
pub use flow::{
    OAuthProviderConfig, OAuthToken, PendingAuthorization, begin, begin_with_resource, complete,
    run_flow, run_flow_with_resource,
};
pub use refresh::{
    REFRESH_MARGIN, RefreshCoordinator, RefreshOutcome, RefreshParams, refresh_access_token,
    should_refresh, should_refresh_at,
};
pub use registration::{
    DynamicRegistration, RegistrationError, RegistrationOptions, register_client,
};
pub use www_authenticate::{AuthChallenge, find_bearer_challenge, parse_www_authenticate};

use secret::SecretStore;

/// redirect URI는 localhost 또는 HTTPS만 허용한다 (설계 §1.5 / PR-18 완료 기준).
/// 이 crate가 만드는 콜백 URI는 항상 127.0.0.1 loopback이지만, provider 설정에
/// 커스텀 URI가 들어오는 경로를 대비해 공개 검증 함수로 둔다.
pub fn validate_redirect_uri(uri: &str) -> anyhow::Result<()> {
    validate_https_or_loopback(uri).map(|_| ())
}

/// URL이 https이거나 http+loopback인지 검증한다 — redirect URI·발견 요청·
/// token endpoint 공용 규칙 (PR-H4에서 추출). 평문 HTTP로 code/token/메타데이터를
/// 주고받지 않으며, localhost는 콜백·로컬 테스트 예외다.
pub fn validate_https_or_loopback(url: &str) -> anyhow::Result<oauth2::url::Url> {
    let parsed =
        oauth2::url::Url::parse(url).map_err(|e| anyhow::anyhow!("URL 파싱 실패: {url} ({e})"))?;
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" => {
            use oauth2::url::Host;
            let is_loopback = match parsed.host() {
                Some(Host::Domain(domain)) => domain == "localhost",
                Some(Host::Ipv4(ip)) => ip.is_loopback(),
                Some(Host::Ipv6(ip)) => ip.is_loopback(),
                None => false,
            };
            if is_loopback {
                Ok(parsed)
            } else {
                anyhow::bail!("http URL은 localhost만 허용: {url}")
            }
        }
        other => anyhow::bail!("URL scheme 불허: {other} ({url})"),
    }
}

/// refresh token이 저장되는 keyring entry id (access와 분리).
pub fn refresh_entry_id(credential_id: &str) -> String {
    format!("{credential_id}.refresh")
}

/// DCR client_secret이 저장되는 keyring entry id (PR-H4).
/// [`register_client`]가 client_secret을 돌려주면 호출측이 이 entry에 저장한다 —
/// client_id(비밀 아님)는 credentials 메타데이터(SQLite)에.
pub fn dcr_secret_entry_id(credential_id: &str) -> String {
    format!("{credential_id}.dcr")
}

/// 획득한 토큰을 keyring에 저장한다 (완료 기준: token keyring 저장).
/// **access token만** credential id 아래 저장한다 — env secret으로 선택되면
/// resolve 경로가 값을 그대로 주입하므로, blob이면 refresh token까지
/// 자식 프로세스에 노출된다. refresh는 별도 entry([`refresh_entry_id`])에 둔다.
/// SQLite에는 평문이 가지 않는다 (§2.1, metadata는 호출측이 credentials 테이블에).
pub fn store_token(
    store: &dyn SecretStore,
    credential_id: &str,
    token: &OAuthToken,
) -> anyhow::Result<()> {
    store.set_secret(credential_id, &token.access_token)?;
    if let Some(refresh) = &token.refresh_token
        && let Err(e) = store.set_secret(&refresh_entry_id(credential_id), refresh)
    {
        // 부분 실패 시 access 고아 entry가 남지 않게 롤백 — 호출측은 id를 버린다
        if let Err(rollback) = store.delete_secret(credential_id) {
            tracing::warn!("access token 롤백 실패 (고아 keyring entry 가능): {rollback:#}");
        }
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use secret::SecretString;

    #[test]
    fn redirect_uri는_localhost_또는_https만() {
        assert!(validate_redirect_uri("https://example.com/cb").is_ok());
        assert!(validate_redirect_uri("http://127.0.0.1:9999/callback").is_ok());
        assert!(validate_redirect_uri("http://localhost/cb").is_ok());
        assert!(validate_redirect_uri("http://[::1]:8080/cb").is_ok());

        assert!(validate_redirect_uri("http://evil.com/cb").is_err());
        assert!(validate_redirect_uri("ftp://127.0.0.1/cb").is_err());
        assert!(validate_redirect_uri("not a url").is_err());
    }

    #[test]
    fn keyring_entry_id_규약() {
        assert_eq!(refresh_entry_id("cred-1"), "cred-1.refresh");
        assert_eq!(dcr_secret_entry_id("cred-1"), "cred-1.dcr");
    }

    #[test]
    fn access는_credential_id에_refresh는_별도_entry에() {
        use std::collections::HashMap;
        use std::sync::Mutex;

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
                let map = self.0.lock().unwrap();
                map.get(id)
                    .map(|v| SecretString::new(v.clone()))
                    .ok_or_else(|| anyhow::anyhow!("no entry: {id}"))
            }
            fn delete_secret(&self, _id: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
                Ok(self.0.lock().unwrap().contains_key(id))
            }
        }

        let store = MemStore(Mutex::new(HashMap::new()));
        let token = OAuthToken {
            access_token: SecretString::new("at-123".to_owned()),
            refresh_token: Some(SecretString::new("rt-456".to_owned())),
            expires_in_secs: Some(3600),
        };
        store_token(&store, "cred-1", &token).unwrap();
        // env secret으로 선택돼도 access token만 주입된다 (blob 아님)
        assert_eq!(store.get_secret("cred-1").unwrap().expose(), "at-123");
        assert_eq!(
            store.get_secret("cred-1.refresh").unwrap().expose(),
            "rt-456"
        );
    }
}
