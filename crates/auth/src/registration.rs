//! RFC 7591 동적 클라이언트 등록 (DCR, PR-H4).
//! 공개 클라이언트(token_endpoint_auth_method "none", PKCE로 보호)로 등록하고,
//! grant_types는 AS 지원 목록과 [authorization_code, refresh_token]의 교집합을 보낸다.
//! redirect_uris에는 고정 포트와 포트 생략(임의 포트, RFC 8252 §7.3) 형태를 병기 —
//! redirect URI 정확 일치를 요구하는 비스펙 AS 대비.
//! 차용: VS Code oauth.ts `fetchDynamicRegistration`.

use std::time::Duration;

use secret::SecretString;

use crate::callback::registration_redirect_uris;
use crate::discovery::AuthorizationServerMetadata;
use crate::{oauth_http_agent, validate_https_or_loopback};

/// 등록 요청 구성.
#[derive(Debug, Clone)]
pub struct RegistrationOptions {
    /// AS 동의 화면 등에 표시될 클라이언트 이름.
    pub client_name: String,
    /// 요청 scope (비어 있으면 scope 필드 생략).
    pub scopes: Vec<String>,
}

/// 등록 결과. client_id는 비밀이 아니다 — 호출측이 credentials 메타데이터(SQLite)에
/// 저장한다. client_secret이 오면 keyring [`crate::dcr_secret_entry_id`] entry에
/// 저장해야 한다 (SQLite 평문 금지, 설계 §2.1).
#[derive(Debug)]
pub struct DynamicRegistration {
    pub client_id: String,
    /// SecretString이라 Debug로도 평문이 새지 않는다.
    pub client_secret: Option<SecretString>,
}

/// DCR 실패 분류 — H5가 "수동 client_id 입력 폴백"으로 넘어갈지 판단한다.
pub enum RegistrationError {
    /// AS가 DCR을 지원하지 않음 (registration_endpoint 없음 / 404 / code grant 미지원).
    Unsupported(String),
    /// AS가 등록 요청을 거부 (4xx + RFC 7591 §3.2.2 error 응답).
    Rejected(String),
    /// 네트워크/응답 형식 등 기타 실패.
    Other(anyhow::Error),
}

impl std::fmt::Debug for RegistrationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let category = match self {
            Self::Unsupported(_) => "unsupported",
            Self::Rejected(_) => "rejected",
            Self::Other(_) => "other",
        };
        formatter
            .debug_struct("RegistrationError")
            .field("category", &category)
            .field("detail", &"REDACTED")
            .finish()
    }
}

impl std::fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(_) => f.write_str("OAuth dynamic registration is unsupported"),
            Self::Rejected(_) => f.write_str("OAuth dynamic registration was rejected"),
            Self::Other(_) => f.write_str("OAuth dynamic registration failed"),
        }
    }
}

impl std::error::Error for RegistrationError {}

/// RFC 7591 동적 등록 실행. 성공 시 client_id(+선택 client_secret)를 돌려준다.
/// 요청은 redirect 금지 [`oauth_http_agent`]로만 나간다 (H4 리뷰 P1) —
/// 호출측 Agent 주입 대신 timeout을 받는다.
pub fn register_client(
    timeout: Duration,
    metadata: &AuthorizationServerMetadata,
    options: &RegistrationOptions,
) -> Result<DynamicRegistration, RegistrationError> {
    let endpoint = metadata.registration_endpoint.as_deref().ok_or_else(|| {
        RegistrationError::Unsupported("AS 메타데이터에 registration_endpoint 없음".to_owned())
    })?;
    validate_https_or_loopback(endpoint).map_err(RegistrationError::Other)?;

    // grant_types = AS 지원 목록 ∩ {authorization_code, refresh_token}.
    // 목록 생략 시 RFC 8414 기본(authorization_code 포함)으로 간주해 둘 다 요청한다.
    const DESIRED_GRANTS: [&str; 2] = ["authorization_code", "refresh_token"];
    let grant_types: Vec<&str> = match &metadata.grant_types_supported {
        Some(supported) => DESIRED_GRANTS
            .into_iter()
            .filter(|grant| supported.iter().any(|s| s == grant))
            .collect(),
        None => DESIRED_GRANTS.to_vec(),
    };
    if !grant_types.contains(&"authorization_code") {
        return Err(RegistrationError::Unsupported(
            "AS가 authorization_code grant를 지원하지 않습니다".to_owned(),
        ));
    }

    let mut body = serde_json::json!({
        "client_name": options.client_name,
        "redirect_uris": registration_redirect_uris(),
        "grant_types": grant_types,
        "response_types": ["code"],
        // 공개 클라이언트 — secret 없이 PKCE로 token 교환
        "token_endpoint_auth_method": "none",
    });
    if !options.scopes.is_empty() {
        body["scope"] = options.scopes.join(" ").into();
    }

    let http = oauth_http_agent(timeout);
    let response = http
        .post(endpoint)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .send(&body.to_string());
    let response = response.map_err(|_| {
        RegistrationError::Other(anyhow::anyhow!("OAuth registration request failed"))
    })?;
    let code = response.status().as_u16();
    if code == 404 {
        return Err(RegistrationError::Unsupported(
            "registration endpoint unavailable".to_owned(),
        ));
    }
    if (400..500).contains(&code) {
        return Err(RegistrationError::Rejected(rejection_reason(
            code, response,
        )));
    }
    if code >= 500 {
        return Err(RegistrationError::Other(anyhow::anyhow!(
            "OAuth registration request failed"
        )));
    }

    // 자동 redirect를 끄고 status를 응답으로 보존하므로 3xx는 여기서 거부한다 —
    // 따라가지 않고 거부한다 (CWE-918 SSRF·헤더 누출 방지, oauth_http_agent 참조)
    let status = response.status().as_u16();
    if (300..400).contains(&status) {
        return Err(RegistrationError::Other(anyhow::anyhow!(
            "redirect 거부 (HTTP {status}) — 등록 요청은 redirect를 따라가지 않습니다"
        )));
    }

    // RFC 7591 §3.2.1 성공은 201 — 200을 주는 실서버도 수용 (2xx는 ureq가 Ok로 준다)
    let body = crate::http::read_ureq_body_bounded(response).map_err(|_| {
        RegistrationError::Other(anyhow::anyhow!("OAuth registration response is invalid"))
    })?;
    let parsed: RegistrationResponse = serde_json::from_slice(&body).map_err(|_| {
        RegistrationError::Other(anyhow::anyhow!("OAuth registration response is invalid"))
    })?;
    Ok(DynamicRegistration {
        client_id: parsed.client_id,
        client_secret: parsed.client_secret.map(SecretString::new),
    })
}

/// RFC 7591 §3.2.2 에러 응답에서 사유 추출 — 형식이 아니면 상태코드 + 본문 앞부분.
fn rejection_reason(code: u16, response: ureq::http::Response<ureq::Body>) -> String {
    #[derive(serde::Deserialize)]
    struct ErrorBody {
        error: String,
        error_description: Option<String>,
    }
    let Ok(body) = crate::http::read_ureq_body_bounded(response) else {
        return format!("HTTP {code}: bounded error response unavailable");
    };
    match serde_json::from_slice::<ErrorBody>(&body) {
        Ok(parsed) => match parsed.error_description {
            Some(description) => format!("HTTP {code} {}: {description}", parsed.error),
            None => format!("HTTP {code} {}", parsed.error),
        },
        Err(_) => {
            format!("HTTP {code}: invalid error response")
        }
    }
}

#[derive(serde::Deserialize)]
struct RegistrationResponse {
    client_id: String,
    client_secret: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callback::FIXED_CALLBACK_PORT;
    use crate::test_support::{MockHttpServer, MockResponse};

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn metadata(
        registration_endpoint: Option<String>,
        grants: Option<Vec<&str>>,
    ) -> AuthorizationServerMetadata {
        AuthorizationServerMetadata {
            issuer: "https://as.example".to_owned(),
            authorization_endpoint: "https://as.example/authorize".to_owned(),
            token_endpoint: "https://as.example/token".to_owned(),
            registration_endpoint,
            grant_types_supported: grants.map(|list| list.into_iter().map(str::to_owned).collect()),
            scopes_supported: None,
            token_endpoint_auth_methods_supported: None,
            code_challenge_methods_supported: None,
        }
    }

    fn options() -> RegistrationOptions {
        RegistrationOptions {
            client_name: "deppy-sijo".to_owned(),
            scopes: vec!["mcp.read".to_owned(), "mcp.write".to_owned()],
        }
    }

    #[test]
    fn 등록_성공과_요청_본문_규약() {
        let server = MockHttpServer::start(|_| {
            MockResponse::json(201, r#"{"client_id":"cid-1","client_secret":"cs-1"}"#)
        });
        let grants = Some(vec![
            "authorization_code",
            "refresh_token",
            "client_credentials",
        ]);
        let registration = register_client(
            TIMEOUT,
            &metadata(Some(server.url("/register")), grants),
            &options(),
        )
        .unwrap();
        assert_eq!(registration.client_id, "cid-1");
        assert_eq!(
            registration.client_secret.as_ref().map(|s| s.expose()),
            Some("cs-1")
        );

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].header("content-type"), Some("application/json"));
        let body: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
        // 공개 클라이언트
        assert_eq!(body["token_endpoint_auth_method"], "none");
        assert_eq!(body["response_types"], serde_json::json!(["code"]));
        // grant 교집합 — AS만 지원하는 client_credentials는 빠진다
        assert_eq!(
            body["grant_types"],
            serde_json::json!(["authorization_code", "refresh_token"])
        );
        // 고정 포트 + 포트 생략(임의 포트) 병기
        assert_eq!(
            body["redirect_uris"],
            serde_json::json!([
                format!("http://127.0.0.1:{FIXED_CALLBACK_PORT}/callback"),
                "http://127.0.0.1/callback"
            ])
        );
        assert_eq!(body["scope"], "mcp.read mcp.write");
        assert_eq!(body["client_name"], "deppy-sijo");
    }

    #[test]
    fn registration_endpoint_없으면_미지원() {
        let error = register_client(TIMEOUT, &metadata(None, None), &options()).unwrap_err();
        assert!(
            matches!(error, RegistrationError::Unsupported(_)),
            "{error}"
        );
    }

    #[test]
    fn endpoint_404는_미지원() {
        let server = MockHttpServer::start(|_| MockResponse::json(404, "{}"));
        let error = register_client(
            TIMEOUT,
            &metadata(Some(server.url("/register")), None),
            &options(),
        )
        .unwrap_err();
        assert!(
            matches!(error, RegistrationError::Unsupported(_)),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            "OAuth dynamic registration is unsupported"
        );
    }

    #[test]
    fn 등록_거부는_에러_사유를_담는다() {
        let server = MockHttpServer::start(|_| {
            MockResponse::json(
                400,
                r#"{"error":"invalid_redirect_uri","error_description":"loopback only"}"#,
            )
        });
        let error = register_client(
            TIMEOUT,
            &metadata(Some(server.url("/register")), None),
            &options(),
        )
        .unwrap_err();
        assert!(matches!(error, RegistrationError::Rejected(_)), "{error}");
        assert_eq!(error.to_string(), "OAuth dynamic registration was rejected");
    }

    #[test]
    fn grant_목록_생략_시_기본_두_grant를_요청() {
        let server = MockHttpServer::start(|_| MockResponse::json(201, r#"{"client_id":"cid-2"}"#));
        let registration = register_client(
            TIMEOUT,
            &metadata(Some(server.url("/register")), None),
            &options(),
        )
        .unwrap();
        assert_eq!(registration.client_id, "cid-2");
        assert!(registration.client_secret.is_none());
        let body: serde_json::Value = serde_json::from_str(&server.requests()[0].body).unwrap();
        assert_eq!(
            body["grant_types"],
            serde_json::json!(["authorization_code", "refresh_token"])
        );
    }

    #[test]
    fn 등록은_redirect를_따라가지_않고_에러() {
        // H4 리뷰 P1 (CWE-918): 따라가면 client_id를 주는 경로가 있어도 도달하지 않는다
        let server = MockHttpServer::start(|req| {
            if req.path == "/redirected" {
                MockResponse::json(201, r#"{"client_id":"stolen"}"#)
            } else {
                MockResponse::redirect(302, "/redirected")
            }
        });
        let error = register_client(
            TIMEOUT,
            &metadata(Some(server.url("/register")), None),
            &options(),
        )
        .unwrap_err();
        assert!(matches!(error, RegistrationError::Other(_)), "{error}");
        assert_eq!(error.to_string(), "OAuth dynamic registration failed");
        // redirect 대상 경로로는 요청이 가지 않았다
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests.iter().all(|r| r.path != "/redirected"));
    }

    #[test]
    fn authorization_code_미지원_as는_요청_없이_미지원() {
        let server = MockHttpServer::start(|_| MockResponse::json(201, r#"{"client_id":"x"}"#));
        let error = register_client(
            TIMEOUT,
            &metadata(
                Some(server.url("/register")),
                Some(vec!["client_credentials"]),
            ),
            &options(),
        )
        .unwrap_err();
        assert!(
            matches!(error, RegistrationError::Unsupported(_)),
            "{error}"
        );
        assert_eq!(server.requests().len(), 0);
    }

    #[test]
    fn registration_error_debug는_raw_detail을_숨긴다() {
        let error = RegistrationError::Rejected(
            "https://auth.example.test rejected client-secret-value".to_owned(),
        );
        let debug = format!("{error:?}");
        let display = error.to_string();
        assert!(!debug.contains("auth.example.test"), "{debug}");
        assert!(!debug.contains("client-secret-value"), "{debug}");
        assert!(!display.contains("auth.example.test"), "{display}");
        assert!(!display.contains("client-secret-value"), "{display}");
    }

    #[test]
    fn registration_success_body_accepts_exact_limit_and_rejects_plus_one() {
        let prefix = r#"{"client_id":"cid-bounded","padding":""#;
        let suffix = r#""}"#;
        let exact_padding = crate::OAUTH_HTTP_RESPONSE_MAX_BYTES - prefix.len() - suffix.len();
        let exact_body = format!("{prefix}{}{suffix}", "x".repeat(exact_padding));
        assert_eq!(exact_body.len(), crate::OAUTH_HTTP_RESPONSE_MAX_BYTES);
        let exact_server =
            MockHttpServer::start(move |_| MockResponse::json(201, exact_body.clone()));
        let registration = register_client(
            TIMEOUT,
            &metadata(Some(exact_server.url("/register")), None),
            &options(),
        )
        .unwrap();
        assert_eq!(registration.client_id, "cid-bounded");

        let plus_one = "x".repeat(crate::OAUTH_HTTP_RESPONSE_MAX_BYTES + 1);
        let plus_one_server =
            MockHttpServer::start(move |_| MockResponse::json(201, plus_one.clone()));
        let error = register_client(
            TIMEOUT,
            &metadata(Some(plus_one_server.url("/register")), None),
            &options(),
        )
        .unwrap_err();
        assert!(matches!(error, RegistrationError::Other(_)));
        assert_eq!(error.to_string(), "OAuth dynamic registration failed");
    }

    #[test]
    fn registration_rejection_body_is_bounded_and_formatting_is_sanitized() {
        let oversized = "https://secret.example/"
            .repeat(crate::OAUTH_HTTP_RESPONSE_MAX_BYTES / "https://secret.example/".len() + 1);
        let server = MockHttpServer::start(move |_| MockResponse::json(400, oversized.clone()));
        let error = register_client(
            TIMEOUT,
            &metadata(Some(server.url("/register")), None),
            &options(),
        )
        .unwrap_err();
        assert!(matches!(error, RegistrationError::Rejected(_)));
        assert!(!error.to_string().contains("secret.example"));
        assert!(!format!("{error:?}").contains("secret.example"));
    }
}
