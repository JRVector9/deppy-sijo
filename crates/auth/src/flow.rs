//! OAuthPKCE flow (설계 §1.5 v1+ OAuth): PKCE 필수(S256), state 검증,
//! redirect URI는 loopback 콜백 서버가 만든다.
//! begin(URL 생성)과 complete(state 검증 + code 교환)로 나눠 테스트 가능하게 유지.

use std::time::Duration;

use anyhow::{Context, bail};
use oauth2::basic::{
    BasicErrorResponse, BasicRevocationErrorResponse, BasicTokenIntrospectionResponse,
    BasicTokenType,
};
use oauth2::{
    AuthType, AuthUrl, AuthorizationCode, Client, ClientId, ClientSecret, CsrfToken,
    EndpointNotSet, EndpointSet, ExtraTokenFields, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, Scope, StandardRevocableToken, StandardTokenResponse, TokenResponse, TokenUrl,
};
use secret::SecretString;

use crate::callback::{CallbackParams, LocalhostCallbackServer};

/// Slack `oauth.v2.user.access`는 표준 token 필드 외에 승인된 workspace를
/// `team.id`(구 응답은 `team_id`)로 준다. 다른 provider에서는 모두 None이다.
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
struct ProviderExtraTokenFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    team: Option<ProviderWorkspace>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    team_id: Option<String>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct ProviderWorkspace {
    id: String,
}

impl ExtraTokenFields for ProviderExtraTokenFields {}

type ProviderTokenResponse = StandardTokenResponse<ProviderExtraTokenFields, BasicTokenType>;

/// BasicClient의 endpoint/error 규약은 유지하면서 provider 확장 token 필드를
/// 보존하는 클라이언트 (oauth2 5 typestate).
type ProviderClient<
    HasAuthUrl = EndpointNotSet,
    HasDeviceAuthUrl = EndpointNotSet,
    HasIntrospectionUrl = EndpointNotSet,
    HasRevocationUrl = EndpointNotSet,
    HasTokenUrl = EndpointNotSet,
> = Client<
    BasicErrorResponse,
    ProviderTokenResponse,
    BasicTokenIntrospectionResponse,
    StandardRevocableToken,
    BasicRevocationErrorResponse,
    HasAuthUrl,
    HasDeviceAuthUrl,
    HasIntrospectionUrl,
    HasRevocationUrl,
    HasTokenUrl,
>;

type ConfiguredClient =
    ProviderClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

/// 사용자가 입력하는 provider 설정 (§11.0 mcp_servers.url과 별개 — 커넥터 등록 폼).
pub struct OAuthProviderConfig {
    pub auth_url: String,
    pub token_url: String,
    pub client_id: String,
    /// confidential client secret. SecretString이라 Debug에도 평문이 노출되지 않는다.
    pub client_secret: Option<SecretString>,
    /// true면 RFC 6749 `client_secret_post`로 token endpoint body에 인증한다.
    /// false는 oauth2 기본인 `client_secret_basic`이다.
    pub client_secret_post: bool,
    pub scopes: Vec<String>,
    /// provider authorize 힌트. Slack은 이전 승인에서 확인한 workspace ID를
    /// `team` 파라미터로 넣어 다음 승인 대상을 고정한다.
    pub extra_authorize_params: Vec<(String, String)>,
}

impl std::fmt::Debug for OAuthProviderConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthProviderConfig")
            .field("endpoints", &"REDACTED")
            .field("client", &"REDACTED")
            .field("has_client_secret", &self.client_secret.is_some())
            .field("client_secret_post", &self.client_secret_post)
            .field("scope_count", &self.scopes.len())
            .field("extra_parameter_count", &self.extra_authorize_params.len())
            .finish()
    }
}

/// begin이 만든 진행 상태 — 브라우저가 돌아올 때까지 보관한다.
pub struct PendingAuthorization {
    pub authorize_url: String,
    client: ConfiguredClient,
    state: CsrfToken,
    pkce_verifier: PkceCodeVerifier,
    /// RFC 8707 resource — authorize에 실었다면 token 교환에도 같이 싣는다.
    resource: Option<String>,
}

pub struct OAuthToken {
    pub access_token: SecretString,
    pub refresh_token: Option<SecretString>,
    pub expires_in_secs: Option<u64>,
    /// provider가 token 응답으로 확정해 준 workspace ID. 인증 힌트로만
    /// 쓰며 비밀이 아니다. Slack 외 provider는 보통 None이다.
    pub provider_workspace_id: Option<String>,
}

impl PendingAuthorization {
    /// Local callback listener가 이번 flow의 redirect만 수락하도록 비교할 CSRF state.
    /// 호출자는 로그/영속 없이 callback 검증에만 사용해야 한다.
    pub fn state(&self) -> &str {
        self.state.secret()
    }
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
            .field(
                "has_provider_workspace_id",
                &self.provider_workspace_id.is_some(),
            )
            .finish()
    }
}

/// authorize URL을 만든다. PKCE(S256)와 state는 언제나 포함 — 옵션이 아니다.
/// auth/token URL은 HTTPS 또는 loopback만 허용한다 (평문 HTTP로 code/token 전송 금지).
pub fn begin(
    config: &OAuthProviderConfig,
    redirect_uri: &str,
) -> anyhow::Result<PendingAuthorization> {
    begin_with_resource(config, redirect_uri, None)
}

/// [`begin`] + RFC 8707 `resource` 파라미터 (PR-H4).
/// resource(MCP 서버 canonical URL — H5가 넘긴다)는 authorize URL과
/// token 교환 요청 양쪽에 첨부된다. None이면 기존 begin과 동일.
pub fn begin_with_resource(
    config: &OAuthProviderConfig,
    redirect_uri: &str,
    resource: Option<&str>,
) -> anyhow::Result<PendingAuthorization> {
    crate::validate_redirect_uri(redirect_uri)?;
    crate::validate_redirect_uri(&config.auth_url)
        .context("auth URL은 HTTPS(또는 로컬 테스트용 loopback)여야 합니다")?;
    crate::validate_redirect_uri(&config.token_url)
        .context("token URL은 HTTPS(또는 로컬 테스트용 loopback)여야 합니다")?;

    let client = ProviderClient::new(ClientId::new(config.client_id.clone()))
        .set_auth_uri(AuthUrl::new(config.auth_url.clone()).context("auth URL 파싱 실패")?)
        .set_token_uri(TokenUrl::new(config.token_url.clone()).context("token URL 파싱 실패")?)
        .set_redirect_uri(
            RedirectUrl::new(redirect_uri.to_owned()).context("redirect URL 파싱 실패")?,
        );
    let client = match &config.client_secret {
        Some(secret) => client.set_client_secret(ClientSecret::new(secret.expose().to_owned())),
        None => client,
    };
    let client = if config.client_secret_post {
        client.set_auth_type(AuthType::RequestBody)
    } else {
        client
    };

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let mut request = client
        .authorize_url(CsrfToken::new_random)
        .add_scopes(config.scopes.iter().map(|s| Scope::new(s.clone())))
        .set_pkce_challenge(pkce_challenge);
    if let Some(resource) = resource {
        // RFC 8707: 발급 대상 리소스를 authorize 단계부터 고정한다
        request = request.add_extra_param("resource", resource.to_owned());
    }
    for (name, value) in &config.extra_authorize_params {
        request = request.add_extra_param(name, value);
    }
    let (authorize_url, state) = request.url();

    Ok(PendingAuthorization {
        authorize_url: authorize_url.to_string(),
        client,
        state,
        pkce_verifier,
        resource: resource.map(str::to_owned),
    })
}

/// 콜백 파라미터로 flow를 끝낸다: state 검증(불일치 즉시 거부) 후
/// code + PKCE verifier로 token 교환.
pub fn complete(
    pending: PendingAuthorization,
    params: CallbackParams,
) -> anyhow::Result<OAuthToken> {
    complete_with_timeout(pending, params, Duration::from_secs(30))
}

/// [`complete`]와 동일한 authorization-code 교환을 수행하되 token endpoint의
/// blocking HTTP 상한을 호출자가 지정한다. `timeout`은 0일 수 없으며 교환은 절대
/// 재시도하지 않는다. CSRF state와 timeout은 모두 token endpoint 접속 전에 검증된다.
pub fn complete_with_timeout(
    pending: PendingAuthorization,
    params: CallbackParams,
    timeout: Duration,
) -> anyhow::Result<OAuthToken> {
    if params.state != *pending.state.secret() {
        bail!("state 불일치 — CSRF 의심, authorization을 거부합니다");
    }
    if timeout.is_zero() {
        bail!("OAuth token exchange timeout은 0보다 커야 합니다");
    }
    // 응답이 멎은 token endpoint에 flow가 영구히 매달리지 않게 timeout.
    // redirect 금지도 함께 강제된다 (H4 리뷰 P1) — token 응답의 302를 따라가면
    // code/PKCE verifier가 redirect 대상으로 유출될 수 있다 (CWE-918).
    let http = crate::http::BoundedOAuthHttpClient::new(timeout);
    let mut request = pending
        .client
        .exchange_code(AuthorizationCode::new(params.code))
        .set_pkce_verifier(pending.pkce_verifier);
    if let Some(resource) = &pending.resource {
        // RFC 8707: authorize에 실었던 resource를 token 교환에도 동일하게
        request = request.add_extra_param("resource", resource.as_str());
    }
    let response = request
        .request(&http)
        .map_err(|_| anyhow::anyhow!("OAuth token exchange failed"))?;
    let provider_workspace_id = response
        .extra_fields()
        .team
        .as_ref()
        .map(|team| team.id.clone())
        .or_else(|| response.extra_fields().team_id.clone());
    Ok(OAuthToken {
        access_token: SecretString::new(response.access_token().secret().clone()),
        refresh_token: response
            .refresh_token()
            .map(|t| SecretString::new(t.secret().clone())),
        expires_in_secs: response.expires_in().map(|d| d.as_secs()),
        provider_workspace_id,
    })
}

/// UI용 원스톱: 콜백 서버 bind → 브라우저 열기 → 승인 대기 → token 교환.
/// 승인 대기까지 블로킹이므로 UI는 백그라운드 스레드에서 부른다.
pub fn run_flow(config: &OAuthProviderConfig, timeout: Duration) -> anyhow::Result<OAuthToken> {
    run_flow_with_resource(config, timeout, None)
}

/// [`run_flow`] + RFC 8707 `resource` 파라미터 (H5의 401 사다리 진입점).
pub fn run_flow_with_resource(
    config: &OAuthProviderConfig,
    timeout: Duration,
    resource: Option<&str>,
) -> anyhow::Result<OAuthToken> {
    let server = LocalhostCallbackServer::bind()?;
    let pending = begin_with_resource(config, server.redirect_uri(), resource)?;
    crate::open_in_browser(&pending.authorize_url)?;
    let params = server.wait_for_callback(timeout, pending.state.secret())?;
    complete(pending, params)
}

/// provider console에 redirect URL을 사전 등록해야 하는 데스크톱 client용 flow.
/// `http://localhost:47456/callback`을 고정 사용하며 포트가 점유됐으면 임의 포트로
/// 바꾸지 않고 실패해 redirect URI 불일치를 방지한다.
pub fn run_flow_with_resource_fixed_localhost(
    config: &OAuthProviderConfig,
    timeout: Duration,
    resource: Option<&str>,
) -> anyhow::Result<OAuthToken> {
    let server = LocalhostCallbackServer::bind_fixed_localhost()?;
    let pending = begin_with_resource(config, server.redirect_uri(), resource)?;
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
    use std::sync::mpsc;
    use std::time::Instant;

    fn config(token_url: &str) -> OAuthProviderConfig {
        OAuthProviderConfig {
            auth_url: "https://provider.example/authorize".to_owned(),
            token_url: token_url.to_owned(),
            client_id: "client-123".to_owned(),
            client_secret: None,
            client_secret_post: false,
            scopes: vec!["mcp.read".to_owned()],
            extra_authorize_params: Vec::new(),
        }
    }

    fn query_map(url: &str) -> HashMap<String, String> {
        oauth2::url::Url::parse(url)
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    /// 요청 전체(헤더 + Content-Length 본문)를 읽는다. 일부만 읽고 소켓을 닫으면
    /// 클라이언트 write가 RST를 맞아 병렬 테스트에서 교환이 간헐 실패한다 (macOS).
    fn read_full_request(stream: &mut std::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        let header_end = loop {
            let n = stream.read(&mut chunk).unwrap();
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            if n == 0 {
                break buf.len();
            }
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
        let content_length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0);
        while buf.len() < header_end + content_length {
            let n = stream.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        String::from_utf8_lossy(&buf).into_owned()
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
    fn authorize_url에_provider_hint를_추가한다() {
        let mut config = config("https://provider.example/token");
        config.extra_authorize_params = vec![("team".to_owned(), "T0ACREG25T6".to_owned())];
        let pending = begin(&config, "http://127.0.0.1:9/callback").unwrap();
        assert_eq!(query_map(&pending.authorize_url)["team"], "T0ACREG25T6");
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

    #[test]
    fn zero_timeout은_token_endpoint_접속_전에_거부() {
        crate::http::reset_http_call_count();
        let pending = begin(
            &config("http://127.0.0.1:9/token"),
            "http://127.0.0.1:9/callback",
        )
        .unwrap();
        let state = pending.state.secret().clone();

        let result = complete_with_timeout(
            pending,
            CallbackParams {
                code: "must-not-leave-process".to_owned(),
                state,
            },
            Duration::ZERO,
        );

        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("timeout"), "{message}");
        assert_eq!(
            crate::http::http_call_count(),
            0,
            "zero timeout must make zero token endpoint calls"
        );
    }

    #[test]
    fn configured_short_timeout_bounds_stalled_exchange_without_retry() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let token_url = format!(
            "http://127.0.0.1:{}/token",
            listener.local_addr().unwrap().port()
        );
        let (release_tx, release_rx) = mpsc::channel();
        let (observed_tx, observed_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_full_request(&mut stream);

            // A correct short-timeout client returns first and releases this stalled response.
            // The fallback prevents a regression from making this test wait for the 30s wrapper.
            let _ = release_rx.recv_timeout(Duration::from_secs(3));
            listener.set_nonblocking(true).unwrap();
            let mut request_count = 1usize;
            loop {
                match listener.accept() {
                    Ok((_stream, _)) => request_count += 1,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("stalled token endpoint accept failed: {error}"),
                }
            }
            observed_tx.send((request_count, request)).unwrap();
        });

        let pending = begin(&config(&token_url), "http://127.0.0.1:9/callback").unwrap();
        let state = pending.state.secret().clone();
        let started = Instant::now();
        let result = complete_with_timeout(
            pending,
            CallbackParams {
                code: "single-use-auth-code".to_owned(),
                state,
            },
            Duration::from_millis(75),
        );
        let elapsed = started.elapsed();
        release_tx.send(()).unwrap();
        let (request_count, request) = observed_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        server.join().unwrap();

        assert!(result.is_err());
        assert!(
            elapsed < Duration::from_secs(2),
            "configured short timeout was ignored: {elapsed:?}"
        );
        assert_eq!(request_count, 1, "authorization code exchange was retried");
        assert!(request.contains("code=single-use-auth-code"), "{request}");
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
            let request = read_full_request(&mut stream);
            let body = r#"{"access_token":"at-ok","token_type":"bearer","refresh_token":"rt-ok","expires_in":3600,"team":{"id":"T0ACREG25T6","name":"Vector9"}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            request
        });

        let mut confidential = config(&token_url);
        confidential.client_secret = Some(SecretString::new("client-secret-456".to_owned()));
        confidential.client_secret_post = true;
        let pending = begin(&confidential, "http://127.0.0.1:9/callback").unwrap();
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
        assert_eq!(token.provider_workspace_id.as_deref(), Some("T0ACREG25T6"));

        // token 요청 body에 code와 PKCE verifier가 실려 있어야 한다 (PKCE 필수 실증)
        let request = server.join().unwrap();
        assert!(request.contains("code=auth-code-1"), "{request}");
        assert!(request.contains("code_verifier="), "{request}");
        assert!(
            request.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A9%2Fcallback"),
            "authorize에 쓴 redirect URI가 token 교환에 동일하게 실리지 않음: {request}"
        );
        assert!(
            request.contains("client_secret=client-secret-456"),
            "confidential client secret가 request body에 없음: {request}"
        );
        assert!(
            request.contains("grant_type=authorization_code"),
            "{request}"
        );
    }

    /// redirect 금지 (H4 리뷰 P1, CWE-918): token endpoint가 302를 줘도 따라가지
    /// 않는다 — code/PKCE verifier가 redirect 대상으로 흘러가는 것을 차단.
    #[test]
    fn token_endpoint_redirect는_따라가지_않고_에러() {
        // redirect 대상 — 여기로는 어떤 연결도 오면 안 된다
        let target = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        target.set_nonblocking(true).unwrap();
        let target_url = format!(
            "http://127.0.0.1:{}/steal",
            target.local_addr().unwrap().port()
        );

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let token_url = format!(
            "http://127.0.0.1:{}/token",
            listener.local_addr().unwrap().port()
        );
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_full_request(&mut stream);
            write!(
                stream,
                "HTTP/1.1 302 Found\r\nLocation: {target_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        });

        let pending = begin(&config(&token_url), "http://127.0.0.1:9/callback").unwrap();
        let state = pending.state.secret().clone();
        let result = complete(
            pending,
            CallbackParams {
                code: "auth-code-3".to_owned(),
                state,
            },
        );
        server.join().unwrap();
        assert!(result.is_err());
        // complete는 동기 실행이므로, redirect를 따라갔다면 이미 연결이 와 있어야 한다
        assert!(
            target.accept().is_err(),
            "redirect를 따라가 token 요청이 유출됐다"
        );
    }

    /// RFC 8707 (PR-H4): resource가 authorize URL과 token 교환 양쪽에 실린다.
    /// resource 없는 기존 begin 경로는 resource 파라미터를 만들지 않는다.
    #[test]
    fn resource_파라미터가_authorize_url과_token_교환에_실린다() {
        // 기존 begin 경로 — resource 없음
        let pending = begin(
            &config("https://provider.example/token"),
            "http://127.0.0.1:9/callback",
        )
        .unwrap();
        assert!(!query_map(&pending.authorize_url).contains_key("resource"));

        // resource 지정 경로
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let token_url = format!(
            "http://127.0.0.1:{}/token",
            listener.local_addr().unwrap().port()
        );
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_full_request(&mut stream);
            let body = r#"{"access_token":"at-ok","token_type":"bearer"}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            request
        });

        let pending = begin_with_resource(
            &config(&token_url),
            "http://127.0.0.1:9/callback",
            Some("https://mcp.example/api"),
        )
        .unwrap();
        let query = query_map(&pending.authorize_url);
        assert_eq!(query["resource"], "https://mcp.example/api");

        let state = pending.state.secret().clone();
        complete(
            pending,
            CallbackParams {
                code: "auth-code-2".to_owned(),
                state,
            },
        )
        .unwrap();
        let request = server.join().unwrap();
        assert!(
            request.contains("resource=https%3A%2F%2Fmcp.example%2Fapi"),
            "{request}"
        );
    }
}
