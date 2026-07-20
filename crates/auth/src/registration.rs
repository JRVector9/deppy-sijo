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
#[derive(Debug)]
pub enum RegistrationError {
    /// AS가 DCR을 지원하지 않음 (registration_endpoint 없음 / 404 / code grant 미지원).
    Unsupported(String),
    /// AS가 등록 요청을 거부 (4xx + RFC 7591 §3.2.2 error 응답).
    Rejected(String),
    /// 네트워크/응답 형식 등 기타 실패.
    Other(anyhow::Error),
}

impl std::fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(message) => write!(f, "DCR 미지원: {message}"),
            Self::Rejected(message) => write!(f, "DCR 거부: {message}"),
            Self::Other(error) => write!(f, "DCR 실패: {error:#}"),
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
        .set("Content-Type", "application/json")
        .set("Accept", "application/json")
        .send_string(&body.to_string());
    let response = match response {
        Ok(response) => response,
        Err(ureq::Error::Status(404, _)) => {
            return Err(RegistrationError::Unsupported(format!(
                "registration endpoint 404: {endpoint}"
            )));
        }
        Err(ureq::Error::Status(code, response)) if (400..500).contains(&code) => {
            return Err(RegistrationError::Rejected(rejection_reason(
                code, response,
            )));
        }
        Err(e) => {
            return Err(RegistrationError::Other(anyhow::anyhow!(
                "등록 요청 실패: {e}"
            )));
        }
    };

    // redirects(0)이라 3xx는 Err이 아니라 Ok로 온다 (ureq는 4xx+만 Err) —
    // 따라가지 않고 거부한다 (CWE-918 SSRF·헤더 누출 방지, oauth_http_agent 참조)
    let status = response.status();
    if (300..400).contains(&status) {
        return Err(RegistrationError::Other(anyhow::anyhow!(
            "redirect 거부 (HTTP {status}) — 등록 요청은 redirect를 따라가지 않습니다"
        )));
    }

    // RFC 7591 §3.2.1 성공은 201 — 200을 주는 실서버도 수용 (2xx는 ureq가 Ok로 준다)
    let body = response
        .into_string()
        .map_err(|e| RegistrationError::Other(anyhow::anyhow!("등록 응답 본문 읽기 실패: {e}")))?;
    let parsed: RegistrationResponse = serde_json::from_str(&body).map_err(|e| {
        RegistrationError::Other(anyhow::anyhow!("등록 응답 파싱 실패 (client_id 필수): {e}"))
    })?;
    Ok(DynamicRegistration {
        client_id: parsed.client_id,
        client_secret: parsed.client_secret.map(SecretString::new),
    })
}

/// RFC 7591 §3.2.2 에러 응답에서 사유 추출 — 형식이 아니면 상태코드 + 본문 앞부분.
fn rejection_reason(code: u16, response: ureq::Response) -> String {
    #[derive(serde::Deserialize)]
    struct ErrorBody {
        error: String,
        error_description: Option<String>,
    }
    let body = response.into_string().unwrap_or_default();
    match serde_json::from_str::<ErrorBody>(&body) {
        Ok(parsed) => match parsed.error_description {
            Some(description) => format!("HTTP {code} {}: {description}", parsed.error),
            None => format!("HTTP {code} {}", parsed.error),
        },
        Err(_) => {
            let truncated: String = body.chars().take(200).collect();
            format!("HTTP {code}: {truncated}")
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
        assert!(error.to_string().contains("404"), "{error}");
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
        let message = error.to_string();
        assert!(message.contains("invalid_redirect_uri"), "{message}");
        assert!(message.contains("loopback only"), "{message}");
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
        assert!(error.to_string().contains("redirect 거부"), "{error}");
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
}
