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
mod http;
mod mcp_oauth;
mod metadata;
mod refresh;
mod registration;
mod slack;
#[cfg(test)]
mod test_support;
mod www_authenticate;

pub use browser::open_in_browser;
pub use callback::{
    CallbackBindError, CallbackParams, FIXED_CALLBACK_BIND_ATTEMPTS,
    FIXED_CALLBACK_BIND_RETRY_DELAY, FIXED_CALLBACK_PORT, LocalhostCallbackServer,
    bind_fixed_localhost_cancellable, registration_redirect_uris,
};
pub use discovery::{
    AuthorizationServerMetadata, DiscoveryHeaders, OAUTH_DISCOVERY_RESPONSE_MAX_BYTES,
    ProtectedResourceMetadata, discover_authorization_server, discover_protected_resource,
};
pub use flow::{
    OAuthProviderConfig, OAuthToken, PendingAuthorization, begin, begin_with_resource, complete,
    run_flow, run_flow_with_resource, run_flow_with_resource_fixed_localhost,
};
pub use http::OAUTH_HTTP_RESPONSE_MAX_BYTES;
pub use mcp_oauth::{
    McpOAuthChallenge, McpOAuthDiscovery, McpOAuthPrimitiveError, PreparedOAuthProviderConfig,
    canonical_mcp_oauth_resource, discover_mcp_oauth_cancellable, oauth_authority_display,
    parse_scopes_bounded, prepare_oauth_provider_config, select_token_endpoint_auth_method,
};
pub use metadata::{
    StoredOAuthMetadata, StoredOAuthMetadataDraft, StoredOAuthMetadataError,
    StoredOAuthMetadataLimits, TokenEndpointAuthMethod,
};
pub use refresh::{
    REFRESH_COORDINATOR_ACTIVE_ID_LIMIT, REFRESH_COORDINATOR_ID_BYTES_MAX,
    REFRESH_COORDINATOR_RETAINED_ID_BYTES_MAX, REFRESH_MARGIN, RefreshCoordinator,
    RefreshCoordinatorStats, RefreshOutcome, RefreshParams, refresh_access_token,
    refresh_access_token_for_slot, should_refresh, should_refresh_at,
};
pub use registration::{
    DynamicRegistration, RegistrationError, RegistrationOptions, register_client,
};
pub use slack::{
    SLACK_MCP_URL, SLACK_OAUTH_RESOURCE, SLACK_WORKSPACE_HTML_MAX_BYTES, SlackWorkspaceError,
    SlackWorkspaceTarget, is_valid_slack_team_id, normalize_slack_workspace_domain,
    parse_slack_workspace_html, resolve_slack_workspace_cancellable, slack_callback_redirect_uri,
    slack_mcp_enable_url,
};
pub use www_authenticate::{AuthChallenge, find_bearer_challenge, parse_www_authenticate};

use std::time::Duration;

use secret::{
    SecretBundleRef, SecretBundleStagePlan, SecretStore, SecretString, StagedSecretBundle,
    stage_secret_bundle,
};

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

/// OAuth 네트워크 요청(발견·DCR·refresh·token 교환) 전용 ureq Agent (H4 리뷰 P1).
/// 자동 redirect를 금지한다 — 따라가면 https→http 다운그레이드, 내부망/클라우드
/// 메타데이터 SSRF(CWE-918), cross-origin으로의 커스텀 헤더 누출이 가능하다.
/// crates/mcp http.rs(H2)와 같은 정책이되, well-known/token은 직접 응답이 정상이라
/// 수동 추적 없이 명확한 에러로 끝낸다. ureq 2는 3xx를 Ok로 돌려주므로
/// (Err은 4xx+만) 각 호출부가 상태를 보고 거부한다.
/// discovery/registration/refresh는 timeout만 받아 내부에서 이 Agent를 만든다 —
/// 호출측이 redirect 허용 Agent를 주입할 수 없다.
pub fn oauth_http_agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .redirects(0)
        // 응답 없는 endpoint에 flow가 매달리지 않게 연결·전체 시간 상한
        // (OAuth 응답은 소형 JSON — SSE 스트리밍이 없어 전체 timeout이 안전하다)
        .timeout_connect(timeout)
        .timeout(timeout)
        .build()
}

/// refresh token이 저장되는 keyring entry id (access와 분리).
///
/// 신규 회전 경로는 logical credential ID가 아니라 resolved physical keyring username을
/// 넘겨야 한다. Typed code should prefer [`secret::PhysicalSecretSlot::refresh_entry_id`].
pub fn refresh_entry_id(keyring_username: &str) -> String {
    format!("{keyring_username}.refresh")
}

/// DCR client_secret이 저장되는 keyring entry id (PR-H4).
/// [`register_client`]가 client_secret을 돌려주면 호출측이 이 entry에 저장한다 —
/// client_id(비밀 아님)는 credentials 메타데이터(SQLite)에.
pub fn dcr_secret_entry_id(keyring_username: &str) -> String {
    format!("{keyring_username}.dcr")
}

/// Stages an access/refresh/DCR OAuth bundle in a new physical slot without publishing storage
/// metadata. The caller must publish `staged.new_slot`, then delete `staged.previous_slot` only
/// after the database commit succeeds.
pub fn stage_oauth_token_bundle(
    store: &dyn SecretStore,
    plan: &SecretBundleStagePlan,
    token: &OAuthToken,
    dcr_secret: Option<&SecretString>,
) -> anyhow::Result<StagedSecretBundle> {
    stage_secret_bundle(
        store,
        plan,
        SecretBundleRef::new(
            &token.access_token,
            token.refresh_token.as_ref(),
            dcr_secret,
        ),
    )
}

/// 획득한 토큰을 keyring에 저장한다 (완료 기준: token keyring 저장).
/// **access token만** credential id 아래 저장한다 — env secret으로 선택되면
/// resolve 경로가 값을 그대로 주입하므로, blob이면 refresh token까지
/// 자식 프로세스에 노출된다. refresh는 별도 entry([`refresh_entry_id`])에 둔다.
/// SQLite에는 평문이 가지 않는다 (§2.1, metadata는 호출측이 credentials 테이블에).
pub fn store_token(
    store: &dyn SecretStore,
    keyring_username: &str,
    token: &OAuthToken,
) -> anyhow::Result<()> {
    store.set_secret(keyring_username, &token.access_token)?;
    if let Some(refresh) = &token.refresh_token
        && let Err(e) = store.set_secret(&refresh_entry_id(keyring_username), refresh)
    {
        // 부분 실패 시 access 고아 entry가 남지 않게 롤백 — 호출측은 id를 버린다
        if store.delete_secret(keyring_username).is_err() {
            tracing::warn!("access token rollback failed; orphan reconciliation is required");
        }
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use secret::{
        LogicalCredentialId, PhysicalSecretSlot, SecretString, inspect_secret_bundle,
        read_secret_bundle,
    };

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
            provider_workspace_id: None,
        };
        store_token(&store, "cred-1", &token).unwrap();
        // env secret으로 선택돼도 access token만 주입된다 (blob 아님)
        assert_eq!(store.get_secret("cred-1").unwrap().expose(), "at-123");
        assert_eq!(
            store.get_secret("cred-1.refresh").unwrap().expose(),
            "rt-456"
        );
    }

    #[test]
    fn oauth_token과_dcr은_typed_physical_bundle로_stage된다() {
        use std::collections::HashMap;
        use std::sync::Mutex;

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
                    .cloned()
                    .map(SecretString::new)
                    .ok_or_else(|| anyhow::anyhow!("no entry: {id}"))
            }
            fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
                self.0.lock().unwrap().remove(id);
                Ok(())
            }
            fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
                Ok(self.0.lock().unwrap().contains_key(id))
            }
        }

        let store = MemStore::default();
        let logical = LogicalCredentialId::new("credential-typed").unwrap();
        let physical = PhysicalSecretSlot::allocate(&logical);
        let plan = SecretBundleStagePlan::with_slot(logical, physical.clone(), None).unwrap();
        let token = OAuthToken {
            access_token: SecretString::new("access-token-typed".to_owned()),
            refresh_token: Some(SecretString::new("refresh-token-typed".to_owned())),
            expires_in_secs: Some(3600),
            provider_workspace_id: None,
        };
        let dcr = SecretString::new("dcr-secret-typed".to_owned());

        let staged = stage_oauth_token_bundle(&store, &plan, &token, Some(&dcr)).unwrap();
        assert_eq!(staged.new_slot, physical);
        assert_eq!(staged.entries.count(), 3);
        assert_eq!(inspect_secret_bundle(&store, &physical).unwrap().count(), 3);
        let read = read_secret_bundle(&store, &physical).unwrap();
        assert_eq!(read.access().expose(), "access-token-typed");
        assert_eq!(read.refresh().unwrap().expose(), "refresh-token-typed");
        assert_eq!(read.dcr().unwrap().expose(), "dcr-secret-typed");
    }
}
