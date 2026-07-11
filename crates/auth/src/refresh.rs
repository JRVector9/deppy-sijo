//! Refresh 교환 (PR-H4): oauth2 5 `exchange_refresh_token`(ureq sync) 기반.
//! 정책 — 만료 5분 전 선제 갱신([`should_refresh`]), credential id 단위
//! single-flight([`RefreshCoordinator`]), AS가 거부하면 access+refresh keyring
//! 폐기 후 "재승인 필요" 반환. 차용: VS Code DynamicAuthProvider의 5분 마진과
//! 실패 시 세션 폐기 (single-flight는 deppy 방식 — VS Code는 단일 스레드 이벤트
//! 루프 + Sequencer로 암묵 해결).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use oauth2::basic::BasicClient;
use oauth2::{
    ClientId, ClientSecret, HttpClientError, RefreshToken, RequestTokenError, TokenResponse,
    TokenUrl,
};
use secret::{SecretStore, SecretString};

use crate::{OAuthToken, refresh_entry_id, store_token};

/// 만료 전 선제 refresh 마진 (VS Code DynamicAuthProvider와 동일한 5분).
pub const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

/// 지금 refresh해야 하는가 — 만료 5분 전부터 true.
/// 만료 시각(credentials 메타데이터, 비밀 아님)을 모르면 선제 갱신하지 않는다 —
/// 만료를 401로 알게 되면 H5 사다리가 처리한다.
pub fn should_refresh(expires_at: Option<SystemTime>) -> bool {
    should_refresh_at(expires_at, SystemTime::now())
}

/// [`should_refresh`]의 시각 주입판 (테스트용 + 호출측 시계 통일용).
pub fn should_refresh_at(expires_at: Option<SystemTime>, now: SystemTime) -> bool {
    match expires_at {
        Some(expires_at) => now + REFRESH_MARGIN >= expires_at,
        None => false,
    }
}

/// credential id 단위 refresh single-flight 조율자.
/// 슬롯 mutex를 잡은 호출만 네트워크로 나간다. 대기 중 다른 호출이 끝냈으면
/// (완료 시각이 내 진입 이후) 재발사하지 않고 결과를 공유받는다 — 회전된
/// refresh token 재사용(AS의 replay 감지 → grant 통째 폐기)을 막는다.
#[derive(Default)]
pub struct RefreshCoordinator {
    slots: Mutex<HashMap<String, Arc<Slot>>>,
}

#[derive(Default)]
struct Slot {
    /// (완료 시각, 공유용 결과 요약) — 토큰 평문은 공유하지 않는다 (keyring 재조회로 충분).
    last: Mutex<Option<(Instant, SharedOutcome)>>,
}

#[derive(Clone)]
enum SharedOutcome {
    Refreshed,
    ReauthorizationRequired(String),
    Failed(String),
}

impl RefreshCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    fn slot(&self, credential_id: &str) -> Arc<Slot> {
        let mut slots = self.slots.lock().expect("refresh slots lock");
        Arc::clone(slots.entry(credential_id.to_owned()).or_default())
    }
}

/// refresh 교환 파라미터 (비밀은 client_secret뿐 — Debug에서 자동 은닉).
#[derive(Debug)]
pub struct RefreshParams {
    pub token_url: String,
    pub client_id: String,
    /// DCR이 client_secret을 발급했다면 keyring `{id}.dcr`에서 꺼내 넣는다.
    pub client_secret: Option<SecretString>,
    /// RFC 8707 resource — MCP 서버 canonical URL.
    pub resource: Option<String>,
}

/// refresh 결과.
#[derive(Debug)]
pub enum RefreshOutcome {
    /// 이 호출이 직접 갱신했고 keyring 저장까지 끝났다.
    /// 만료 시각 메타데이터(expires_in_secs) 갱신은 호출측 몫.
    Refreshed(OAuthToken),
    /// 대기 중 다른 호출이 이미 갱신을 끝냈다 — access token은 keyring 재조회로 얻는다.
    AlreadyRefreshed,
    /// AS가 refresh를 거부 — access+refresh keyring 폐기 완료, 재승인(브라우저 flow) 필요.
    ReauthorizationRequired { reason: String },
}

/// access token을 refresh한다. 성공 시 새 토큰은 이미 keyring에 저장돼 있다
/// ([`crate::store_token`] 규약 — access는 credential id, refresh는 `{id}.refresh`).
/// 동시 호출은 credential id 단위로 직렬화되고, 대기 중 끝난 갱신은 재발사 없이 공유한다.
pub fn refresh_access_token(
    coordinator: &RefreshCoordinator,
    http: &ureq::Agent,
    store: &dyn SecretStore,
    credential_id: &str,
    params: &RefreshParams,
) -> anyhow::Result<RefreshOutcome> {
    let slot = coordinator.slot(credential_id);
    let entered = Instant::now();
    // 선행 refresh가 진행 중이면 여기서 대기한다 (single-flight)
    let mut last = slot.last.lock().expect("refresh slot lock");
    if let Some((finished_at, shared)) = last.as_ref()
        && *finished_at >= entered
    {
        // 내가 기다리는 동안 선행 호출이 끝냈다 — 재발사하지 않고 결과 공유
        return match shared {
            SharedOutcome::Refreshed => Ok(RefreshOutcome::AlreadyRefreshed),
            SharedOutcome::ReauthorizationRequired(reason) => {
                Ok(RefreshOutcome::ReauthorizationRequired {
                    reason: reason.clone(),
                })
            }
            SharedOutcome::Failed(message) => Err(anyhow::anyhow!("선행 refresh 실패: {message}")),
        };
    }
    let result = do_refresh(http, store, credential_id, params);
    let shared = match &result {
        Ok(RefreshOutcome::ReauthorizationRequired { reason }) => {
            SharedOutcome::ReauthorizationRequired(reason.clone())
        }
        Ok(_) => SharedOutcome::Refreshed,
        Err(e) => SharedOutcome::Failed(format!("{e:#}")),
    };
    *last = Some((Instant::now(), shared));
    result
}

fn do_refresh(
    http: &ureq::Agent,
    store: &dyn SecretStore,
    credential_id: &str,
    params: &RefreshParams,
) -> anyhow::Result<RefreshOutcome> {
    let refresh_id = refresh_entry_id(credential_id);
    match store.has_secret(&refresh_id) {
        Ok(true) => {}
        Ok(false) => {
            // refresh token 자체가 없다 — 네트워크 없이 곧장 재승인 요구
            return Ok(RefreshOutcome::ReauthorizationRequired {
                reason: "저장된 refresh token 없음".to_owned(),
            });
        }
        Err(e) => return Err(e.context("refresh token 존재 확인 실패")),
    }
    let refresh_token = store
        .get_secret(&refresh_id)
        .context("refresh token 조회 실패")?;

    crate::validate_https_or_loopback(&params.token_url)
        .context("token URL은 HTTPS(또는 로컬 테스트용 loopback)여야 합니다")?;
    let client = BasicClient::new(ClientId::new(params.client_id.clone()))
        .set_token_uri(TokenUrl::new(params.token_url.clone()).context("token URL 파싱 실패")?);
    let client = match &params.client_secret {
        Some(secret) => client.set_client_secret(ClientSecret::new(secret.expose().to_owned())),
        None => client,
    };

    let grant = RefreshToken::new(refresh_token.expose().to_owned());
    let mut request = client.exchange_refresh_token(&grant);
    if let Some(resource) = &params.resource {
        // RFC 8707: refresh 교환에도 resource를 실어 대상 리소스를 고정한다
        request = request.add_extra_param("resource", resource.as_str());
    }
    match request.request(http) {
        Ok(response) => {
            let token = OAuthToken {
                access_token: SecretString::new(response.access_token().secret().clone()),
                // RFC 6749 §6: 응답에 refresh_token이 없으면 회전 없음 — 기존 것을 유지
                refresh_token: Some(
                    response
                        .refresh_token()
                        .map(|t| SecretString::new(t.secret().clone()))
                        .unwrap_or(refresh_token),
                ),
                expires_in_secs: response.expires_in().map(|d| d.as_secs()),
            };
            store_token(store, credential_id, &token).context("갱신 토큰 저장 실패")?;
            Ok(RefreshOutcome::Refreshed(token))
        }
        // AS가 명시적으로 거부(invalid_grant 등) — 이 grant는 죽었다.
        // access+refresh를 폐기하고 재승인을 요구한다. 네트워크 일시 장애는
        // Transient 분기(Err)로 가며 폐기하지 않는다 — 토큰이 살아 있을 수 있다.
        Err(error) => match classify_refresh_error(error) {
            RefreshFailure::Rejected(reason) => {
                discard_tokens(store, credential_id);
                Ok(RefreshOutcome::ReauthorizationRequired { reason })
            }
            RefreshFailure::Transient(error) => Err(error).context("refresh 교환 실패"),
        },
    }
}

/// refresh 교환 에러 분류 결과.
enum RefreshFailure {
    /// AS의 명시적 거부 (RFC 6749 §5.2 error 응답) — 토큰 폐기 대상.
    Rejected(String),
    /// 일시 장애/기타 — 폐기하지 않는다.
    Transient(anyhow::Error),
}

type RefreshExchangeError =
    RequestTokenError<HttpClientError<ureq::Error>, oauth2::basic::BasicErrorResponse>;

/// oauth2 5의 ureq 어댑터는 4xx/5xx를 `ureq::Error::Status`로 감싼 Request 에러로
/// 돌려준다 (`ServerResponse`에 도달하지 않음) — 여기서 RFC 6749 §5.2 error 응답을
/// 직접 복원해 "AS 거부"와 "일시 장애"를 가른다.
fn classify_refresh_error(error: RefreshExchangeError) -> RefreshFailure {
    match error {
        // 정석 경로 — 어댑터가 4xx 응답을 그대로 넘겨주게 되면 여기로 온다
        RequestTokenError::ServerResponse(response) => {
            RefreshFailure::Rejected(match response.error_description() {
                Some(description) => {
                    format!("AS 거부: {} ({description})", response.error().as_ref())
                }
                None => format!("AS 거부: {}", response.error().as_ref()),
            })
        }
        RequestTokenError::Request(HttpClientError::Reqwest(boxed)) => match *boxed {
            ureq::Error::Status(status, response) if (400..500).contains(&status) => {
                #[derive(serde::Deserialize)]
                struct TokenErrorBody {
                    error: String,
                    error_description: Option<String>,
                }
                let body = response.into_string().unwrap_or_default();
                match serde_json::from_str::<TokenErrorBody>(&body) {
                    Ok(parsed) => RefreshFailure::Rejected(match parsed.error_description {
                        Some(description) => format!("AS 거부: {} ({description})", parsed.error),
                        None => format!("AS 거부: {}", parsed.error),
                    }),
                    // RFC 6749 형식이 아닌 4xx — 명시적 거부로 단정하지 않는다
                    Err(_) => RefreshFailure::Transient(anyhow::anyhow!(
                        "token endpoint HTTP {status}: {}",
                        body.chars().take(200).collect::<String>()
                    )),
                }
            }
            other => RefreshFailure::Transient(anyhow::Error::new(other)),
        },
        other => RefreshFailure::Transient(anyhow::Error::new(other)),
    }
}

/// keyring 규약: 삭제는 access(credential id)와 refresh(`{id}.refresh`) 두 entry 모두.
/// DCR client_secret(`{id}.dcr`)은 등록 정보라 유지한다 — 재승인 flow가 재사용.
fn discard_tokens(store: &dyn SecretStore, credential_id: &str) {
    if let Err(e) = store.delete_secret(credential_id) {
        tracing::warn!("access token 폐기 실패 ({credential_id}): {e:#}");
    }
    if let Err(e) = store.delete_secret(&refresh_entry_id(credential_id)) {
        tracing::warn!("refresh token 폐기 실패 ({credential_id}): {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MemStore, MockHttpServer, MockResponse};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn agent() -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(5))
            .build()
    }

    fn seeded_store() -> MemStore {
        let store = MemStore::default();
        store.seed("cred", "old-at");
        store.seed(&refresh_entry_id("cred"), "old-rt");
        store
    }

    fn params(token_url: String) -> RefreshParams {
        RefreshParams {
            token_url,
            client_id: "client-1".to_owned(),
            client_secret: None,
            resource: Some("https://mcp.example/api".to_owned()),
        }
    }

    #[test]
    fn 만료_5분_전부터_선제_갱신_판정() {
        let now = SystemTime::now();
        // 10분 남음 — 아직 아니다
        assert!(!should_refresh_at(
            Some(now + Duration::from_secs(600)),
            now
        ));
        // 4분 남음 — 마진 안
        assert!(should_refresh_at(Some(now + Duration::from_secs(240)), now));
        // 이미 만료
        assert!(should_refresh_at(Some(now - Duration::from_secs(1)), now));
        // 만료 시각 미상 — 선제 갱신하지 않는다
        assert!(!should_refresh_at(None, now));
    }

    #[test]
    fn refresh_성공_시_새_토큰을_keyring에_저장하고_resource를_싣는다() {
        let server = MockHttpServer::start(|_| {
            MockResponse::json(
                200,
                r#"{"access_token":"new-at","token_type":"bearer","refresh_token":"new-rt","expires_in":3600}"#,
            )
        });
        let store = seeded_store();
        let coordinator = RefreshCoordinator::new();
        let outcome = refresh_access_token(
            &coordinator,
            &agent(),
            &store,
            "cred",
            &params(server.url("/token")),
        )
        .unwrap();
        let RefreshOutcome::Refreshed(token) = outcome else {
            panic!("Refreshed가 아님: {outcome:?}");
        };
        assert_eq!(token.access_token.expose(), "new-at");
        assert_eq!(token.expires_in_secs, Some(3600));
        // keyring 규약대로 저장 (access = id, refresh = {id}.refresh)
        assert_eq!(store.value("cred").as_deref(), Some("new-at"));
        assert_eq!(
            store.value(&refresh_entry_id("cred")).as_deref(),
            Some("new-rt")
        );

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        let body = &requests[0].body;
        assert!(body.contains("grant_type=refresh_token"), "{body}");
        assert!(body.contains("refresh_token=old-rt"), "{body}");
        // RFC 8707 resource 파라미터
        assert!(
            body.contains("resource=https%3A%2F%2Fmcp.example%2Fapi"),
            "{body}"
        );
    }

    #[test]
    fn 응답에_refresh_token이_없으면_기존_것을_유지() {
        let server = MockHttpServer::start(|_| {
            MockResponse::json(
                200,
                r#"{"access_token":"new-at","token_type":"bearer","expires_in":60}"#,
            )
        });
        let store = seeded_store();
        let outcome = refresh_access_token(
            &RefreshCoordinator::new(),
            &agent(),
            &store,
            "cred",
            &params(server.url("/token")),
        )
        .unwrap();
        assert!(matches!(outcome, RefreshOutcome::Refreshed(_)));
        assert_eq!(store.value("cred").as_deref(), Some("new-at"));
        // 회전 없음 — refresh entry는 기존 값 유지
        assert_eq!(
            store.value(&refresh_entry_id("cred")).as_deref(),
            Some("old-rt")
        );
    }

    #[test]
    fn invalid_grant면_토큰_폐기_후_재승인_필요() {
        let server = MockHttpServer::start(|_| {
            MockResponse::json(
                400,
                r#"{"error":"invalid_grant","error_description":"revoked"}"#,
            )
        });
        let store = seeded_store();
        let outcome = refresh_access_token(
            &RefreshCoordinator::new(),
            &agent(),
            &store,
            "cred",
            &params(server.url("/token")),
        )
        .unwrap();
        let RefreshOutcome::ReauthorizationRequired { reason } = outcome else {
            panic!("ReauthorizationRequired가 아님: {outcome:?}");
        };
        assert!(reason.contains("invalid_grant"), "{reason}");
        // access + refresh 두 entry 모두 폐기
        assert_eq!(store.value("cred"), None);
        assert_eq!(store.value(&refresh_entry_id("cred")), None);
    }

    #[test]
    fn 일시_장애는_폐기하지_않고_에러() {
        // 5xx + 비JSON 본문 — AS의 명시적 거부가 아니다
        let server = MockHttpServer::start(|_| MockResponse::text(500, "oops"));
        let store = seeded_store();
        let result = refresh_access_token(
            &RefreshCoordinator::new(),
            &agent(),
            &store,
            "cred",
            &params(server.url("/token")),
        );
        assert!(result.is_err());
        // 토큰은 살아 있어야 한다
        assert_eq!(store.value("cred").as_deref(), Some("old-at"));
        assert_eq!(
            store.value(&refresh_entry_id("cred")).as_deref(),
            Some("old-rt")
        );
    }

    #[test]
    fn refresh_token이_없으면_네트워크_없이_재승인_필요() {
        let store = MemStore::default();
        store.seed("cred", "old-at"); // access만 있고 refresh 없음
        let outcome = refresh_access_token(
            &RefreshCoordinator::new(),
            &agent(),
            &store,
            "cred",
            &params("https://as.example/token".to_owned()),
        )
        .unwrap();
        let RefreshOutcome::ReauthorizationRequired { reason } = outcome else {
            panic!("ReauthorizationRequired가 아님: {outcome:?}");
        };
        assert!(reason.contains("refresh token"), "{reason}");
    }

    #[test]
    fn 동시_refresh는_한_번만_발사() {
        let hits = Arc::new(AtomicUsize::new(0));
        let server = {
            let hits = Arc::clone(&hits);
            MockHttpServer::start(move |_| {
                hits.fetch_add(1, Ordering::SeqCst);
                // 두 번째 호출이 대기 상태에 들어갈 시간을 넉넉히 확보
                std::thread::sleep(Duration::from_millis(700));
                MockResponse::json(
                    200,
                    r#"{"access_token":"new-at","token_type":"bearer","refresh_token":"new-rt"}"#,
                )
            })
        };
        let store = Arc::new(seeded_store());
        let coordinator = Arc::new(RefreshCoordinator::new());
        let request_params = Arc::new(params(server.url("/token")));

        let spawn_call = |delay: Duration| {
            let store = Arc::clone(&store);
            let coordinator = Arc::clone(&coordinator);
            let request_params = Arc::clone(&request_params);
            std::thread::spawn(move || {
                std::thread::sleep(delay);
                refresh_access_token(&coordinator, &agent(), &*store, "cred", &request_params)
            })
        };
        let first = spawn_call(Duration::ZERO);
        let second = spawn_call(Duration::from_millis(150));
        let outcomes = [
            first.join().expect("first join").unwrap(),
            second.join().expect("second join").unwrap(),
        ];

        // 네트워크 refresh는 정확히 1회
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        // 한쪽은 직접 갱신, 다른 쪽은 공유 (스레드 순서 무관)
        assert_eq!(
            outcomes
                .iter()
                .filter(|o| matches!(o, RefreshOutcome::Refreshed(_)))
                .count(),
            1,
            "{outcomes:?}"
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|o| matches!(o, RefreshOutcome::AlreadyRefreshed))
                .count(),
            1,
            "{outcomes:?}"
        );
        assert_eq!(store.value("cred").as_deref(), Some("new-at"));
    }
}
