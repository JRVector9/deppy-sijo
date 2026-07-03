//! OAuthPKCE flow (설계 §1.5 v1+ OAuth): PKCE 필수(S256), state 검증,
//! redirect URI는 loopback 콜백 서버가 만든다.
//! begin(URL 생성)과 complete(state 검증 + code 교환)로 나눠 테스트 가능하게 유지.

use std::time::Duration;

use anyhow::{Context, bail};
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, CsrfToken, EndpointNotSet, EndpointSet,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use secret::SecretString;

use crate::callback::{CallbackParams, LocalhostCallbackServer};

/// authorize + token endpoint가 설정된 클라이언트 (oauth2 5 typestate).
type ConfiguredClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

/// 사용자가 입력하는 provider 설정 (§11.0 mcp_servers.url과 별개 — 커넥터 등록 폼).
#[derive(Debug, Clone)]
pub struct OAuthProviderConfig {
    pub auth_url: String,
    pub token_url: String,
    pub client_id: String,
    pub scopes: Vec<String>,
}

/// begin이 만든 진행 상태 — 브라우저가 돌아올 때까지 보관한다.
pub struct PendingAuthorization {
    pub authorize_url: String,
    client: ConfiguredClient,
    state: CsrfToken,
    pkce_verifier: PkceCodeVerifier,
}

pub struct OAuthToken {
    pub access_token: SecretString,
    pub refresh_token: Option<SecretString>,
    pub expires_in_secs: Option<u64>,
}

// 토큰 평문이 로그/panic 메시지로 새지 않게 은닉 (§2.1)
impl std::fmt::Debug for OAuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthToken")
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("expires_in_secs", &self.expires_in_secs)
            .finish()
    }
}

/// authorize URL을 만든다. PKCE(S256)와 state는 언제나 포함 — 옵션이 아니다.
/// auth/token URL은 HTTPS 또는 loopback만 허용한다 (평문 HTTP로 code/token 전송 금지).
pub fn begin(
    config: &OAuthProviderConfig,
    redirect_uri: &str,
) -> anyhow::Result<PendingAuthorization> {
    crate::validate_redirect_uri(redirect_uri)?;
    crate::validate_redirect_uri(&config.auth_url)
        .context("auth URL은 HTTPS(또는 로컬 테스트용 loopback)여야 합니다")?;
    crate::validate_redirect_uri(&config.token_url)
        .context("token URL은 HTTPS(또는 로컬 테스트용 loopback)여야 합니다")?;

    let client = BasicClient::new(ClientId::new(config.client_id.clone()))
        .set_auth_uri(AuthUrl::new(config.auth_url.clone()).context("auth URL 파싱 실패")?)
        .set_token_uri(TokenUrl::new(config.token_url.clone()).context("token URL 파싱 실패")?)
        .set_redirect_uri(
            RedirectUrl::new(redirect_uri.to_owned()).context("redirect URL 파싱 실패")?,
        );

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let (authorize_url, state) = client
        .authorize_url(CsrfToken::new_random)
        .add_scopes(config.scopes.iter().map(|s| Scope::new(s.clone())))
        .set_pkce_challenge(pkce_challenge)
        .url();

    Ok(PendingAuthorization {
        authorize_url: authorize_url.to_string(),
        client,
        state,
        pkce_verifier,
    })
}

/// 콜백 파라미터로 flow를 끝낸다: state 검증(불일치 즉시 거부) 후
/// code + PKCE verifier로 token 교환.
pub fn complete(
    pending: PendingAuthorization,
    params: CallbackParams,
) -> anyhow::Result<OAuthToken> {
    if params.state != *pending.state.secret() {
        bail!("state 불일치 — CSRF 의심, authorization을 거부합니다");
    }
    // 응답이 멎은 token endpoint에 flow가 영구히 매달리지 않게 timeout
    let http: ureq::Agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .build();
    let response = pending
        .client
        .exchange_code(AuthorizationCode::new(params.code))
        .set_pkce_verifier(pending.pkce_verifier)
        .request(&http)
        .context("token 교환 실패")?;
    Ok(OAuthToken {
        access_token: SecretString::new(response.access_token().secret().clone()),
        refresh_token: response
            .refresh_token()
            .map(|t| SecretString::new(t.secret().clone())),
        expires_in_secs: response.expires_in().map(|d| d.as_secs()),
    })
}

/// UI용 원스톱: 콜백 서버 bind → 브라우저 열기 → 승인 대기 → token 교환.
/// 승인 대기까지 블로킹이므로 UI는 백그라운드 스레드에서 부른다.
pub fn run_flow(config: &OAuthProviderConfig, timeout: Duration) -> anyhow::Result<OAuthToken> {
    let server = LocalhostCallbackServer::bind()?;
    let pending = begin(config, server.redirect_uri())?;
    crate::open_in_browser(&pending.authorize_url)?;
    let params = server.wait_for_callback(timeout, pending.state.secret())?;
    complete(pending, params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn config(token_url: &str) -> OAuthProviderConfig {
        OAuthProviderConfig {
            auth_url: "https://provider.example/authorize".to_owned(),
            token_url: token_url.to_owned(),
            client_id: "client-123".to_owned(),
            scopes: vec!["mcp.read".to_owned()],
        }
    }

    fn query_map(url: &str) -> HashMap<String, String> {
        oauth2::url::Url::parse(url)
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    #[test]
    fn authorize_url에_pkce와_state가_반드시_포함() {
        let pending = begin(
            &config("https://provider.example/token"),
            "http://127.0.0.1:9/callback",
        )
        .unwrap();
        let query = query_map(&pending.authorize_url);
        // PKCE 필수 (완료 기준) — S256만 사용
        assert_eq!(query["code_challenge_method"], "S256");
        assert!(!query["code_challenge"].is_empty());
        // state 필수 (완료 기준)
        assert!(!query["state"].is_empty());
        assert_eq!(query["client_id"], "client-123");
        assert_eq!(query["redirect_uri"], "http://127.0.0.1:9/callback");
        assert_eq!(query["scope"], "mcp.read");
    }

    #[test]
    fn 비보안_redirect_uri는_begin에서_거부() {
        let result = begin(
            &config("https://provider.example/token"),
            "http://evil.com/callback",
        );
        assert!(result.is_err());
    }

    #[test]
    fn state_불일치는_토큰_교환_없이_거부() {
        let pending = begin(
            &config("https://provider.example/token"),
            "http://127.0.0.1:9/callback",
        )
        .unwrap();
        let result = complete(
            pending,
            CallbackParams {
                code: "any-code".to_owned(),
                state: "attacker-forged".to_owned(),
            },
        );
        // token endpoint에 닿기 전에 실패해야 한다 (네트워크 없는 URL이므로
        // state 검증을 통과했다면 교환 시도 에러가 났을 것 — 메시지로 구분)
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("state 불일치"), "{message}");
    }

    /// mock token endpoint로 전체 교환 검증: PKCE verifier가 요청에 실리고
    /// 응답 토큰이 파싱된다.
    #[test]
    fn 올바른_state와_code_verifier로_토큰_교환() {
        // 한 요청만 받는 mock token endpoint
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let token_url = format!(
            "http://127.0.0.1:{}/token",
            listener.local_addr().unwrap().port()
        );
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            let body = r#"{"access_token":"at-ok","token_type":"bearer","refresh_token":"rt-ok","expires_in":3600}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            request
        });

        let pending = begin(&config(&token_url), "http://127.0.0.1:9/callback").unwrap();
        let state = pending.state.secret().clone();
        let token = complete(
            pending,
            CallbackParams {
                code: "auth-code-1".to_owned(),
                state,
            },
        )
        .unwrap();

        assert_eq!(token.access_token.expose(), "at-ok");
        assert_eq!(token.refresh_token.unwrap().expose(), "rt-ok");
        assert_eq!(token.expires_in_secs, Some(3600));

        // token 요청 body에 code와 PKCE verifier가 실려 있어야 한다 (PKCE 필수 실증)
        let request = server.join().unwrap();
        assert!(request.contains("code=auth-code-1"), "{request}");
        assert!(request.contains("code_verifier="), "{request}");
        assert!(
            request.contains("grant_type=authorization_code"),
            "{request}"
        );
    }
}
