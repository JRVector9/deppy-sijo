//! OAuth 발견 (PR-H4): RFC 9728 보호 리소스 메타데이터(PRM) + RFC 8414 AS 메타데이터.
//! 401 챌린지에서 출발해 authorize/token/register endpoint를 알아내는 네트워크
//! 프리미티브 — 401 사다리 조립은 H5 몫.
//!
//! 차용: VS Code oauth.ts `fetchResourceMetadata`(RFC 9728 resource 일치 강제,
//! 실패 수집 → 종합 에러) / `fetchAuthorizationServerMetadata` +
//! `getDefaultMetadataForUrl`(well-known 3경로 → 기본 endpoint 폴백).
//! 커스텀 헤더의 same-origin 제한은 extHostMcp.ts `sameOriginHeaders`.

use std::time::Duration;

use anyhow::{Context, bail};
use oauth2::url::Url;

use crate::{oauth_http_agent, validate_https_or_loopback};

/// RFC 9728 보호 리소스 메타데이터 (필요 필드만 — 나머지는 무시).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProtectedResourceMetadata {
    /// 보호 리소스 식별자 — 서버 URL과 정규화 후 정확히 일치해야 한다 (RFC 9728 §3.3).
    pub resource: String,
    /// 이 리소스를 관할하는 authorization server(issuer) 목록.
    #[serde(default)]
    pub authorization_servers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes_supported: Option<Vec<String>>,
}

/// RFC 8414 authorization server 메타데이터 (필요 필드만 — 나머지는 무시).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AuthorizationServerMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
    /// 생략 시 RFC 8414 기본값은 ["authorization_code", "implicit"] — 해석은 소비자(DCR) 몫.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_types_supported: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes_supported: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_methods_supported: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_challenge_methods_supported: Option<Vec<String>>,
}

/// 발견 요청에 same-origin일 때만 부착하는 커스텀 헤더 (MCP-Protocol-Version 등).
/// 기준 origin은 MCP 서버 URL — 교차 출처(다른 host의 AS/PRM)로 헤더가 새지 않게 한다.
#[derive(Debug, Clone)]
pub struct DiscoveryHeaders {
    origin: Url,
    headers: Vec<(String, String)>,
}

impl DiscoveryHeaders {
    /// `origin_url`(MCP 서버 URL) 기준의 same-origin 헤더 정책을 만든다.
    pub fn new(origin_url: &str, headers: Vec<(String, String)>) -> anyhow::Result<Self> {
        let origin = Url::parse(origin_url)
            .with_context(|| format!("origin URL 파싱 실패: {origin_url}"))?;
        Ok(Self { origin, headers })
    }

    /// 대상이 same-origin일 때만 헤더 목록을 준다 (아니면 빈 목록).
    fn for_target(&self, target: &Url) -> &[(String, String)] {
        if target.origin() == self.origin.origin() {
            &self.headers
        } else {
            &[]
        }
    }
}

/// RFC 9728 PRM 발견. 후보 순서: ①401 챌린지의 `resource_metadata` URL →
/// ②`{origin}/.well-known/oauth-protected-resource{path}`(path-insertion) → ③root.
/// **응답 `resource`가 서버 URL과 정규화 후 일치하지 않으면 그 후보를 거부한다** —
/// 다른 리소스의 메타데이터로 토큰을 오발급받는 것을 막는 핵심 검증.
/// 모든 후보 실패 시 시도 URL과 원인을 모아 종합 에러로 보고한다.
/// 요청은 redirect 금지 [`oauth_http_agent`]로만 나간다 (H4 리뷰 P1) —
/// 호출측 Agent 주입 대신 timeout을 받는다.
pub fn discover_protected_resource(
    timeout: Duration,
    server_url: &str,
    resource_metadata_url: Option<&str>,
    headers: Option<&DiscoveryHeaders>,
) -> anyhow::Result<ProtectedResourceMetadata> {
    let server = validate_https_or_loopback(server_url).context("MCP 서버 URL 검증 실패")?;
    let http = oauth_http_agent(timeout);
    let origin = server.origin().ascii_serialization();
    let path = well_known_path_suffix(&server);

    let mut candidates: Vec<String> = Vec::new();
    let mut push_unique = |url: String| {
        if !candidates.contains(&url) {
            candidates.push(url);
        }
    };
    if let Some(url) = resource_metadata_url {
        push_unique(url.to_owned());
    }
    push_unique(format!(
        "{origin}/.well-known/oauth-protected-resource{path}"
    ));
    push_unique(format!("{origin}/.well-known/oauth-protected-resource"));

    let mut failures: Vec<String> = Vec::new();
    for candidate in &candidates {
        match try_protected_resource(&http, candidate, &server, headers) {
            Ok(metadata) => return Ok(metadata),
            Err(e) => failures.push(format!("- {candidate}: {e:#}")),
        }
    }
    bail!(
        "보호 리소스 메타데이터 발견 실패 (서버 {server_url}):\n{}",
        failures.join("\n")
    )
}

fn try_protected_resource(
    http: &ureq::Agent,
    candidate: &str,
    server: &Url,
    headers: Option<&DiscoveryHeaders>,
) -> anyhow::Result<ProtectedResourceMetadata> {
    let value = fetch_discovery_json(http, candidate, headers)?;
    let metadata: ProtectedResourceMetadata =
        serde_json::from_value(value).context("RFC 9728 메타데이터 형식이 아님")?;
    if !url_matches(server, &metadata.resource) {
        bail!(
            "resource 불일치: 메타데이터 resource={} ≠ 서버 URL (RFC 9728 §3.3 — 거부)",
            metadata.resource
        );
    }
    Ok(metadata)
}

/// RFC 8414 AS 메타데이터 발견. 후보 순서:
/// ①`/.well-known/oauth-authorization-server{path}`(path-insertion) →
/// ②`/.well-known/openid-configuration{path}`(path-insertion) →
/// ③`{path}/.well-known/openid-configuration`(path-addition).
/// **응답 `issuer`는 요청한 AS URL과 정규화 후 정확히 일치해야 한다 (RFC 8414 §3.3)**
/// — PRM의 resource 검증과 대칭. 불일치 응답이 하나라도 있으면 폴백 없이 거부한다.
/// 그 외(404 등 발견 미구현) 전부 실패 시에만 기본 endpoint(`/authorize`, `/token`,
/// `/register`)로 폴백한다 (VS Code `getDefaultMetadataForUrl` 차용).
/// 요청은 redirect 금지 [`oauth_http_agent`]로만 나간다 (H4 리뷰 P1) —
/// 호출측 Agent 주입 대신 timeout을 받는다.
pub fn discover_authorization_server(
    timeout: Duration,
    authorization_server: &str,
    headers: Option<&DiscoveryHeaders>,
) -> anyhow::Result<AuthorizationServerMetadata> {
    let issuer = validate_https_or_loopback(authorization_server).context("AS URL 검증 실패")?;
    let http = oauth_http_agent(timeout);
    let origin = issuer.origin().ascii_serialization();
    let path = well_known_path_suffix(&issuer);

    let mut candidates: Vec<String> = Vec::new();
    for url in [
        format!("{origin}/.well-known/oauth-authorization-server{path}"),
        format!("{origin}/.well-known/openid-configuration{path}"),
        format!("{origin}{path}/.well-known/openid-configuration"),
    ] {
        if !candidates.contains(&url) {
            candidates.push(url);
        }
    }

    let mut failures: Vec<String> = Vec::new();
    let mut issuer_mismatch = false;
    for candidate in &candidates {
        match try_authorization_server(&http, candidate, headers) {
            Ok(metadata) => {
                // RFC 8414 §3.3: issuer 값은 메타데이터 조회에 쓴 issuer 식별자와
                // 정확히 일치해야 한다 — 불일치면 다른(또는 조작된) AS의 메타데이터로,
                // 그 endpoint로 code/token을 보내면 안 된다.
                if url_matches(&issuer, &metadata.issuer) {
                    return Ok(metadata);
                }
                issuer_mismatch = true;
                failures.push(format!(
                    "- {candidate}: issuer 불일치: 메타데이터 issuer={} ≠ 요청 AS URL (RFC 8414 §3.3 — 거부)",
                    metadata.issuer
                ));
            }
            Err(e) => failures.push(format!("- {candidate}: {e:#}")),
        }
    }
    // issuer 불일치는 "발견 미구현"(404 등)과 달리 이 origin의 OAuth 응답이
    // 조작·오배선됐다는 적극적 신호 — 신뢰할 수 없는 응답을 본 이상 추측 기반
    // 기본 endpoint 폴백으로 진행하지 않고 명확히 거부한다.
    if issuer_mismatch {
        bail!(
            "AS 메타데이터 발견 실패 — issuer 불일치 응답 감지 ({authorization_server}):\n{}",
            failures.join("\n")
        );
    }
    tracing::warn!(
        "AS 메타데이터 발견 실패 — 기본 endpoint로 폴백 ({authorization_server}):\n{}",
        failures.join("\n")
    );
    Ok(default_authorization_server_metadata(&issuer))
}

fn try_authorization_server(
    http: &ureq::Agent,
    candidate: &str,
    headers: Option<&DiscoveryHeaders>,
) -> anyhow::Result<AuthorizationServerMetadata> {
    let value = fetch_discovery_json(http, candidate, headers)?;
    serde_json::from_value(value).context("RFC 8414 메타데이터 형식이 아님")
}

/// 발견 실패 시의 기본 endpoint — origin 기준 `/authorize`, `/token`, `/register`
/// (VS Code처럼 issuer의 path를 무시하고 루트에 붙인다).
fn default_authorization_server_metadata(issuer: &Url) -> AuthorizationServerMetadata {
    let origin = issuer.origin().ascii_serialization();
    AuthorizationServerMetadata {
        issuer: issuer.to_string(),
        authorization_endpoint: format!("{origin}/authorize"),
        token_endpoint: format!("{origin}/token"),
        registration_endpoint: Some(format!("{origin}/register")),
        grant_types_supported: None,
        scopes_supported: None,
        token_endpoint_auth_methods_supported: None,
        code_challenge_methods_supported: None,
    }
}

/// 발견 후보 URL 하나를 GET해서 JSON으로 파싱한다.
/// 커스텀 헤더는 same-origin 대상에만 부착된다.
fn fetch_discovery_json(
    http: &ureq::Agent,
    url: &str,
    headers: Option<&DiscoveryHeaders>,
) -> anyhow::Result<serde_json::Value> {
    let parsed = validate_https_or_loopback(url)?;
    let mut request = http.get(url).set("Accept", "application/json");
    if let Some(policy) = headers {
        for (name, value) in policy.for_target(&parsed) {
            request = request.set(name, value);
        }
    }
    let response = request.call().map_err(|e| match e {
        ureq::Error::Status(code, _) => anyhow::anyhow!("HTTP {code}"),
        other => anyhow::anyhow!("요청 실패: {other}"),
    })?;
    // redirects(0)이라 3xx는 Err이 아니라 여기로 그대로 온다 (ureq는 4xx+만 Err) —
    // 따라가지 않고 명확히 거부한다. redirect 추적은 https→http 다운그레이드·SSRF·
    // 헤더 누출 경로다 (CWE-918, oauth_http_agent 참조). 정상 well-known은 직접 응답한다.
    let status = response.status();
    if (300..400).contains(&status) {
        let location = response.header("Location").unwrap_or("<없음>");
        bail!(
            "redirect 거부 (HTTP {status} → {location}) — OAuth 발견 요청은 redirect를 따라가지 않습니다"
        );
    }
    let body = response.into_string().context("응답 본문 읽기 실패")?;
    serde_json::from_str(&body).context("JSON 파싱 실패")
}

/// well-known path-insertion에 붙일 서버 path. 루트("/")면 빈 문자열,
/// trailing slash는 제거해 후보 URL을 안정화한다.
fn well_known_path_suffix(url: &Url) -> &str {
    url.path().trim_end_matches('/')
}

/// 정규화 후 URL 정확 일치 검사 — RFC 9728 §3.3 `resource`와 RFC 8414 §3.3
/// `issuer` 검증 공용. Url 파싱이 scheme/host 소문자화를 해 주고, 여기서는 기본
/// 포트와 trailing slash만 추가로 흡수한다 — 그 이상은 완화하지 않는다
/// (fragment가 있으면 RFC 8707/8414 위반, 불일치).
fn url_matches(expected: &Url, actual: &str) -> bool {
    let Ok(actual) = Url::parse(actual) else {
        return false;
    };
    if actual.fragment().is_some() {
        return false;
    }
    actual.scheme() == expected.scheme()
        && actual.host_str() == expected.host_str()
        && actual.port_or_known_default() == expected.port_or_known_default()
        && actual.path().trim_end_matches('/') == expected.path().trim_end_matches('/')
        && actual.query() == expected.query()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MockHttpServer, MockResponse, RecordedRequest};

    const TIMEOUT: Duration = Duration::from_secs(5);

    /// 요청의 Host 헤더로 이 목 서버 자신을 가리키는 resource 값을 만든다
    /// (포트가 bind 후에 정해지는 문제 회피).
    fn own_resource(req: &RecordedRequest, path: &str) -> String {
        format!("http://{}{path}", req.header("host").unwrap_or_default())
    }

    fn prm_json(resource: String) -> String {
        serde_json::json!({
            "resource": resource,
            "authorization_servers": ["https://as.example"],
        })
        .to_string()
    }

    fn as_metadata_json(req: &RecordedRequest) -> String {
        let issuer = own_resource(req, "/tenant");
        serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/authorize"),
            "token_endpoint": format!("{issuer}/token"),
            "registration_endpoint": format!("{issuer}/register"),
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "token_endpoint_auth_methods_supported": ["client_secret_post"],
        })
        .to_string()
    }

    #[test]
    fn 챌린지의_resource_metadata_url을_먼저_쓴다() {
        let server = MockHttpServer::start(|req| {
            if req.path == "/custom/prm" {
                MockResponse::json(200, prm_json(own_resource(req, "/mcp")))
            } else {
                MockResponse::json(404, "{}")
            }
        });
        let metadata = discover_protected_resource(
            TIMEOUT,
            &server.url("/mcp"),
            Some(&server.url("/custom/prm")),
            None,
        )
        .unwrap();
        assert_eq!(metadata.authorization_servers, vec!["https://as.example"]);
        // 챌린지 URL이 성공하면 well-known 후보는 시도하지 않는다
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/custom/prm");
    }

    #[test]
    fn path_insertion_well_known_경로로_발견() {
        let server = MockHttpServer::start(|req| {
            if req.path == "/.well-known/oauth-protected-resource/mcp" {
                MockResponse::json(200, prm_json(own_resource(req, "/mcp")))
            } else {
                MockResponse::json(404, "{}")
            }
        });
        let metadata =
            discover_protected_resource(TIMEOUT, &server.url("/mcp"), None, None).unwrap();
        assert_eq!(metadata.resource, server.url("/mcp"));
        assert_eq!(
            server.requests()[0].path,
            "/.well-known/oauth-protected-resource/mcp"
        );
    }

    #[test]
    fn path_insertion_실패_시_root로_폴백() {
        let server = MockHttpServer::start(|req| {
            if req.path == "/.well-known/oauth-protected-resource" {
                MockResponse::json(200, prm_json(own_resource(req, "/mcp")))
            } else {
                MockResponse::json(404, "{}")
            }
        });
        let metadata =
            discover_protected_resource(TIMEOUT, &server.url("/mcp"), None, None).unwrap();
        assert_eq!(metadata.resource, server.url("/mcp"));
        let paths: Vec<_> = server.requests().iter().map(|r| r.path.clone()).collect();
        assert_eq!(
            paths,
            vec![
                "/.well-known/oauth-protected-resource/mcp",
                "/.well-known/oauth-protected-resource"
            ]
        );
    }

    #[test]
    fn resource_불일치는_거부하고_종합_에러로_보고() {
        // 핵심 보안: 다른 리소스를 가리키는 메타데이터로 토큰을 받으면 안 된다
        let server = MockHttpServer::start(|req| {
            if req
                .path
                .starts_with("/.well-known/oauth-protected-resource")
            {
                MockResponse::json(200, prm_json("https://evil.example/mcp".to_owned()))
            } else {
                MockResponse::json(404, "{}")
            }
        });
        let error =
            discover_protected_resource(TIMEOUT, &server.url("/mcp"), None, None).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("resource 불일치"), "{message}");
        // 시도한 후보 URL들이 모두 보고된다
        assert!(
            message.contains("/.well-known/oauth-protected-resource/mcp"),
            "{message}"
        );
        assert!(message.contains("발견 실패"), "{message}");
    }

    #[test]
    fn resource는_trailing_slash_정규화_후_일치() {
        let server = MockHttpServer::start(|req| {
            if req.path == "/.well-known/oauth-protected-resource/mcp" {
                // 메타데이터는 slash 없는 형태, 서버 URL은 slash 있는 형태
                MockResponse::json(200, prm_json(own_resource(req, "/mcp")))
            } else {
                MockResponse::json(404, "{}")
            }
        });
        let metadata = discover_protected_resource(
            TIMEOUT,
            &format!("{}/mcp/", server.base_url()),
            None,
            None,
        )
        .unwrap();
        assert_eq!(metadata.resource, server.url("/mcp"));
    }

    #[test]
    fn 커스텀_헤더는_same_origin일_때만_부착() {
        let make_handler = |mcp_path: &'static str| {
            move |req: &RecordedRequest| {
                if req
                    .path
                    .starts_with("/.well-known/oauth-protected-resource")
                    || req.path == "/prm"
                {
                    MockResponse::json(200, prm_json(own_resource(req, mcp_path)))
                } else {
                    MockResponse::json(404, "{}")
                }
            }
        };
        // same-origin: 서버 자신의 well-known에는 헤더가 붙는다
        let same = MockHttpServer::start(make_handler("/mcp"));
        let headers = DiscoveryHeaders::new(
            &same.url("/mcp"),
            vec![("MCP-Protocol-Version".to_owned(), "2025-11-25".to_owned())],
        )
        .unwrap();
        discover_protected_resource(TIMEOUT, &same.url("/mcp"), None, Some(&headers)).unwrap();
        assert_eq!(
            same.requests()[0].header("mcp-protocol-version"),
            Some("2025-11-25")
        );

        // cross-origin: 다른 origin의 챌린지 URL에는 붙지 않는다
        let mcp = MockHttpServer::start(make_handler("/mcp"));
        let cross_resource = mcp.url("/mcp");
        let cross = MockHttpServer::start(move |_req| {
            MockResponse::json(200, prm_json(cross_resource.clone()))
        });
        let headers = DiscoveryHeaders::new(
            &mcp.url("/mcp"),
            vec![("MCP-Protocol-Version".to_owned(), "2025-11-25".to_owned())],
        )
        .unwrap();
        discover_protected_resource(
            TIMEOUT,
            &mcp.url("/mcp"),
            Some(&cross.url("/prm")),
            Some(&headers),
        )
        .unwrap();
        assert_eq!(cross.requests().len(), 1);
        assert_eq!(cross.requests()[0].header("mcp-protocol-version"), None);
    }

    #[test]
    fn 비loopback_http_서버는_네트워크_없이_거부() {
        let error =
            discover_protected_resource(TIMEOUT, "http://192.0.2.1/mcp", None, None).unwrap_err();
        assert!(
            format!("{error:#}").contains("localhost만 허용"),
            "{error:#}"
        );
    }

    #[test]
    fn as_발견_oauth_authorization_server_path_insertion() {
        let server = MockHttpServer::start(|req| {
            if req.path == "/.well-known/oauth-authorization-server/tenant" {
                MockResponse::json(200, as_metadata_json(req))
            } else {
                MockResponse::json(404, "{}")
            }
        });
        let metadata =
            discover_authorization_server(TIMEOUT, &server.url("/tenant"), None).unwrap();
        assert_eq!(metadata.issuer, server.url("/tenant"));
        assert_eq!(
            metadata.token_endpoint,
            format!("{}/token", server.url("/tenant"))
        );
        assert_eq!(
            metadata.grant_types_supported,
            Some(vec![
                "authorization_code".to_owned(),
                "refresh_token".to_owned()
            ])
        );
        assert_eq!(
            metadata.token_endpoint_auth_methods_supported,
            Some(vec!["client_secret_post".to_owned()])
        );
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn as_발견_openid_configuration_path_insertion_폴백() {
        let server = MockHttpServer::start(|req| {
            if req.path == "/.well-known/openid-configuration/tenant" {
                MockResponse::json(200, as_metadata_json(req))
            } else {
                MockResponse::json(404, "{}")
            }
        });
        let metadata =
            discover_authorization_server(TIMEOUT, &server.url("/tenant"), None).unwrap();
        assert_eq!(metadata.issuer, server.url("/tenant"));
        let paths: Vec<_> = server.requests().iter().map(|r| r.path.clone()).collect();
        assert_eq!(
            paths,
            vec![
                "/.well-known/oauth-authorization-server/tenant",
                "/.well-known/openid-configuration/tenant"
            ]
        );
    }

    #[test]
    fn as_발견_path_addition_폴백() {
        let server = MockHttpServer::start(|req| {
            if req.path == "/tenant/.well-known/openid-configuration" {
                MockResponse::json(200, as_metadata_json(req))
            } else {
                MockResponse::json(404, "{}")
            }
        });
        let metadata =
            discover_authorization_server(TIMEOUT, &server.url("/tenant"), None).unwrap();
        assert_eq!(metadata.issuer, server.url("/tenant"));
        assert_eq!(server.requests().len(), 3);
    }

    #[test]
    fn as_발견_전부_실패하면_기본_endpoint_폴백() {
        let server = MockHttpServer::start(|_| MockResponse::json(404, "{}"));
        let metadata =
            discover_authorization_server(TIMEOUT, &server.url("/tenant"), None).unwrap();
        // path가 있으면 후보 3개를 모두 시도한 뒤 폴백
        assert_eq!(server.requests().len(), 3);
        assert_eq!(metadata.issuer, server.url("/tenant"));
        assert_eq!(
            metadata.authorization_endpoint,
            format!("{}/authorize", server.base_url())
        );
        assert_eq!(
            metadata.token_endpoint,
            format!("{}/token", server.base_url())
        );
        assert_eq!(
            metadata.registration_endpoint,
            Some(format!("{}/register", server.base_url()))
        );
    }

    #[test]
    fn as_발견_루트_url은_중복_후보를_제거한다() {
        // path가 없으면 openid-configuration의 insertion과 addition이 같아진다
        let server = MockHttpServer::start(|_| MockResponse::json(404, "{}"));
        let metadata = discover_authorization_server(TIMEOUT, server.base_url(), None).unwrap();
        assert_eq!(server.requests().len(), 2);
        assert_eq!(
            metadata.token_endpoint,
            format!("{}/token", server.base_url())
        );
    }

    #[test]
    fn prm_발견은_redirect를_따라가지_않고_거부() {
        // H4 리뷰 P1 (CWE-918): 따라가면 유효한 PRM을 주는 경로가 있어도 도달하지 않는다
        let server = MockHttpServer::start(|req| {
            if req.path == "/redirected" {
                MockResponse::json(200, prm_json(own_resource(req, "/mcp")))
            } else {
                MockResponse::redirect(302, "/redirected")
            }
        });
        let error =
            discover_protected_resource(TIMEOUT, &server.url("/mcp"), None, None).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("redirect 거부"), "{message}");
        // redirect 대상 경로로는 요청이 가지 않았다
        assert!(server.requests().iter().all(|r| r.path != "/redirected"));
    }

    #[test]
    fn as_발견은_redirect를_따라가지_않고_기본_endpoint_폴백() {
        let server = MockHttpServer::start(|req| {
            if req.path == "/redirected" {
                MockResponse::json(200, as_metadata_json(req))
            } else {
                MockResponse::redirect(302, "/redirected")
            }
        });
        // redirect 거부는 404처럼 "그 후보 실패" — 검증된 origin 기반 기본 endpoint로 폴백
        let metadata =
            discover_authorization_server(TIMEOUT, &server.url("/tenant"), None).unwrap();
        assert!(server.requests().iter().all(|r| r.path != "/redirected"));
        assert_eq!(server.requests().len(), 3);
        assert_eq!(
            metadata.token_endpoint,
            format!("{}/token", server.base_url())
        );
    }

    #[test]
    fn as_issuer_불일치는_거부하고_기본_endpoint로_폴백하지_않는다() {
        // 핵심 보안 (RFC 8414 §3.3): 다른 AS를 가리키는 메타데이터의 endpoint로
        // code/token을 보내면 안 되고, 조작 신호가 있으니 기본 endpoint 폴백도 금지
        let server = MockHttpServer::start(|req| {
            if req.path == "/.well-known/oauth-authorization-server/tenant" {
                MockResponse::json(
                    200,
                    serde_json::json!({
                        "issuer": "https://evil.example/tenant",
                        "authorization_endpoint": "https://evil.example/authorize",
                        "token_endpoint": "https://evil.example/token",
                    })
                    .to_string(),
                )
            } else {
                MockResponse::json(404, "{}")
            }
        });
        let error =
            discover_authorization_server(TIMEOUT, &server.url("/tenant"), None).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("issuer 불일치"), "{message}");
        assert!(message.contains("https://evil.example/tenant"), "{message}");
        // 나머지 후보들은 계속 시도된다 (첫 후보만 조작된 다중 테넌트 오배선 대비)
        assert_eq!(server.requests().len(), 3);
    }

    #[test]
    fn as_issuer는_trailing_slash_정규화_후_일치() {
        let server = MockHttpServer::start(|req| {
            if req.path == "/.well-known/oauth-authorization-server/tenant" {
                // 메타데이터 issuer는 slash 있는 형태, 요청 AS URL은 slash 없는 형태
                let issuer = own_resource(req, "/tenant/");
                MockResponse::json(
                    200,
                    serde_json::json!({
                        "issuer": issuer,
                        "authorization_endpoint": format!("{issuer}authorize"),
                        "token_endpoint": format!("{issuer}token"),
                    })
                    .to_string(),
                )
            } else {
                MockResponse::json(404, "{}")
            }
        });
        let metadata =
            discover_authorization_server(TIMEOUT, &server.url("/tenant"), None).unwrap();
        assert_eq!(metadata.issuer, server.url("/tenant/"));
    }
}
