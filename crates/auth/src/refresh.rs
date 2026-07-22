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
    AuthType, ClientId, ClientSecret, RefreshToken, RequestTokenError, TokenResponse, TokenUrl,
};
use secret::{
    LogicalCredentialId, PhysicalSecretSlot, SecretStore, SecretString, read_secret_bundle,
};

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
pub struct RefreshParams {
    pub token_url: String,
    pub client_id: String,
    /// DCR이 client_secret을 발급했다면 keyring `{id}.dcr`에서 꺼내 넣는다.
    pub client_secret: Option<SecretString>,
    /// AS 메타데이터가 `client_secret_post`를 요구하면 true.
    pub client_secret_post: bool,
    /// RFC 8707 resource — MCP 서버 canonical URL.
    pub resource: Option<String>,
}

impl std::fmt::Debug for RefreshParams {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RefreshParams")
            .field("binding", &"REDACTED")
            .field("has_client_secret", &self.client_secret.is_some())
            .field("client_secret_post", &self.client_secret_post)
            .field("has_resource", &self.resource.is_some())
            .finish()
    }
}

/// refresh 결과.
pub enum RefreshOutcome {
    /// 이 호출이 직접 갱신했다. Legacy [`refresh_access_token`]은 기존 username에 저장한
    /// 뒤 반환하고, [`refresh_access_token_for_slot`]은 publish callback의 pointer commit이
    /// 성공한 뒤 반환한다.
    Refreshed(OAuthToken),
    /// 대기 중 다른 호출이 이미 갱신을 끝냈다 — access token은 keyring 재조회로 얻는다.
    AlreadyRefreshed,
    /// AS가 refresh를 거부했다. Legacy 경로는 기존 access+refresh를 폐기하며, typed slot
    /// 경로는 pointer transaction/reconciliation을 위해 기존 slot을 건드리지 않는다.
    ReauthorizationRequired { reason: String },
}

impl std::fmt::Debug for RefreshOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refreshed(_) => formatter.write_str("Refreshed(REDACTED)"),
            Self::AlreadyRefreshed => formatter.write_str("AlreadyRefreshed"),
            Self::ReauthorizationRequired { .. } => {
                formatter.write_str("ReauthorizationRequired(REDACTED)")
            }
        }
    }
}

/// access token을 refresh한다. 성공 시 새 토큰은 이미 keyring에 저장돼 있다
/// ([`crate::store_token`] 규약 — access는 credential id, refresh는 `{id}.refresh`).
/// 동시 호출은 credential id 단위로 직렬화되고, 대기 중 끝난 갱신은 재발사 없이 공유한다.
/// 요청은 redirect 금지 [`oauth_http_agent`]로만 나간다 (H4 리뷰 P1) —
/// 호출측 Agent 주입 대신 timeout을 받는다.
pub fn refresh_access_token(
    coordinator: &RefreshCoordinator,
    timeout: Duration,
    store: &dyn SecretStore,
    credential_id: &str,
    params: &RefreshParams,
) -> anyhow::Result<RefreshOutcome> {
    coordinate_refresh(coordinator, credential_id, || {
        do_refresh_legacy(timeout, store, credential_id, params)
    })
}

/// Exchanges the refresh token read from a resolved physical slot but does not overwrite that
/// slot itself. The `publish` callback runs inside the credential's single-flight critical section;
/// it receives the DCR secret already read from the current bundle and must stage the returned
/// token plus DCR, publish the new pointer transactionally, and return success only after that
/// commit. Waiters receive `AlreadyRefreshed` only after `publish` succeeds.
/// A rejected grant remains present for startup reconciliation and is never automatically retried.
pub fn refresh_access_token_for_slot(
    coordinator: &RefreshCoordinator,
    timeout: Duration,
    store: &dyn SecretStore,
    logical_id: &LogicalCredentialId,
    current_slot: &PhysicalSecretSlot,
    params: &RefreshParams,
    publish: impl FnOnce(&OAuthToken, Option<&SecretString>) -> anyhow::Result<()>,
) -> anyhow::Result<RefreshOutcome> {
    anyhow::ensure!(
        current_slot.belongs_to(logical_id),
        "current physical slot does not belong to logical credential"
    );
    refresh_access_token_for_slot_with(
        coordinator,
        store,
        logical_id,
        current_slot,
        |refresh_token| exchange_refresh(timeout, refresh_token, params),
        publish,
    )
}

fn refresh_access_token_for_slot_with(
    coordinator: &RefreshCoordinator,
    store: &dyn SecretStore,
    logical_id: &LogicalCredentialId,
    current_slot: &PhysicalSecretSlot,
    exchange: impl FnOnce(SecretString) -> anyhow::Result<RefreshOutcome>,
    publish: impl FnOnce(&OAuthToken, Option<&SecretString>) -> anyhow::Result<()>,
) -> anyhow::Result<RefreshOutcome> {
    coordinate_refresh(coordinator, logical_id.as_str(), || {
        let current = read_secret_bundle(store, current_slot)
            .context("current physical OAuth bundle read failed")?;
        let (_, refresh_token, dcr_secret) = current.into_parts();
        let Some(refresh_token) = refresh_token else {
            return Ok(RefreshOutcome::ReauthorizationRequired {
                reason: "저장된 refresh token 없음".to_owned(),
            });
        };
        let outcome = exchange(refresh_token)?;
        if let RefreshOutcome::Refreshed(token) = &outcome {
            publish(token, dcr_secret.as_ref()).context("refreshed OAuth bundle publish failed")?;
        }
        Ok(outcome)
    })
}

fn coordinate_refresh(
    coordinator: &RefreshCoordinator,
    coordination_id: &str,
    refresh: impl FnOnce() -> anyhow::Result<RefreshOutcome>,
) -> anyhow::Result<RefreshOutcome> {
    let slot = coordinator.slot(coordination_id);
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
    let result = refresh();
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

fn do_refresh_legacy(
    timeout: Duration,
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

    let outcome = exchange_refresh(timeout, refresh_token, params)?;
    match &outcome {
        RefreshOutcome::Refreshed(token) => {
            store_token(store, credential_id, token).context("갱신 토큰 저장 실패")?;
        }
        RefreshOutcome::ReauthorizationRequired { .. } => {
            discard_tokens(store, credential_id);
        }
        RefreshOutcome::AlreadyRefreshed => {}
    }
    Ok(outcome)
}

fn exchange_refresh(
    timeout: Duration,
    refresh_token: SecretString,
    params: &RefreshParams,
) -> anyhow::Result<RefreshOutcome> {
    crate::validate_https_or_loopback(&params.token_url)
        .context("token URL은 HTTPS(또는 로컬 테스트용 loopback)여야 합니다")?;
    let client = BasicClient::new(ClientId::new(params.client_id.clone()))
        .set_token_uri(TokenUrl::new(params.token_url.clone()).context("token URL 파싱 실패")?);
    let client = match &params.client_secret {
        Some(secret) => client.set_client_secret(ClientSecret::new(secret.expose().to_owned())),
        None => client,
    };
    let client = if params.client_secret_post {
        client.set_auth_type(AuthType::RequestBody)
    } else {
        client
    };

    let grant = RefreshToken::new(refresh_token.expose().to_owned());
    let mut request = client.exchange_refresh_token(&grant);
    if let Some(resource) = &params.resource {
        // RFC 8707: refresh 교환에도 resource를 실어 대상 리소스를 고정한다
        request = request.add_extra_param("resource", resource.as_str());
    }
    // redirect 금지 Agent (H4 리뷰 P1) — token 응답의 302를 따라가면 refresh token이
    // redirect 대상으로 흘러갈 수 있다. 302는 oauth2가 비200 응답으로 에러 처리하며
    // 아래 분류에서 Transient(폐기 없음)로 떨어진다.
    let http = crate::http::BoundedOAuthHttpClient::new(timeout);
    match request.request(&http) {
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
                provider_workspace_id: None,
            };
            Ok(RefreshOutcome::Refreshed(token))
        }
        // AS가 명시적으로 거부(invalid_grant 등) — 이 grant는 죽었다.
        // access+refresh를 폐기하고 재승인을 요구한다. 네트워크 일시 장애는
        // Transient 분기(Err)로 가며 폐기하지 않는다 — 토큰이 살아 있을 수 있다.
        Err(error) => match classify_refresh_error(error) {
            RefreshFailure::Rejected(reason) => {
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
    RequestTokenError<crate::http::BoundedOAuthHttpError, oauth2::basic::BasicErrorResponse>;

/// The bounded adapter preserves 4xx response status/body, allowing oauth2 to parse typed RFC 6749
/// errors. All non-server-response failures are collapsed to sanitized transient categories.
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
        RequestTokenError::Request(_) => {
            RefreshFailure::Transient(anyhow::anyhow!("OAuth refresh request failed"))
        }
        RequestTokenError::Parse(_, _) => {
            RefreshFailure::Transient(anyhow::anyhow!("OAuth refresh response is invalid"))
        }
        RequestTokenError::Other(_) => {
            RefreshFailure::Transient(anyhow::anyhow!("OAuth refresh response was rejected"))
        }
    }
}

/// keyring 규약: 삭제는 access(credential id)와 refresh(`{id}.refresh`) 두 entry 모두.
/// DCR client_secret(`{id}.dcr`)은 등록 정보라 유지한다 — 재승인 flow가 재사용.
fn discard_tokens(store: &dyn SecretStore, credential_id: &str) {
    if store.delete_secret(credential_id).is_err() {
        tracing::warn!("access token deletion failed; orphan reconciliation is required");
    }
    if store
        .delete_secret(&refresh_entry_id(credential_id))
        .is_err()
    {
        tracing::warn!("refresh token deletion failed; orphan reconciliation is required");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MemStore, MockHttpServer, MockResponse};
    use secret::{SecretBundleStagePlan, stage_secret_bundle};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const TIMEOUT: Duration = Duration::from_secs(5);

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
            client_secret_post: false,
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
        let mut request_params = params(server.url("/token"));
        request_params.client_secret = Some(SecretString::new("refresh-secret".to_owned()));
        request_params.client_secret_post = true;
        let outcome =
            refresh_access_token(&coordinator, TIMEOUT, &store, "cred", &request_params).unwrap();
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
        assert!(body.contains("client_secret=refresh-secret"), "{body}");
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
            TIMEOUT,
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
            TIMEOUT,
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
            TIMEOUT,
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
    fn refresh는_redirect를_따라가지_않고_토큰을_보존한다() {
        // H4 리뷰 P1 (CWE-918): token endpoint의 302를 따라가면 refresh token이
        // redirect 대상으로 유출된다. redirect는 AS의 명시적 거부가 아니므로 폐기도 없다.
        let server =
            MockHttpServer::start(|_| MockResponse::redirect(302, "https://evil.example/token"));
        let store = seeded_store();
        let result = refresh_access_token(
            &RefreshCoordinator::new(),
            TIMEOUT,
            &store,
            "cred",
            &params(server.url("/token")),
        );
        assert!(result.is_err());
        // redirect 대상으로 재요청 없음 — 목 서버가 받은 요청은 1건뿐
        assert_eq!(server.requests().len(), 1);
        // 토큰은 살아 있어야 한다 (Transient 취급)
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
            TIMEOUT,
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
    fn typed_slot_refresh_token이_없으면_slot을_변경하지_않는다() {
        let store = MemStore::default();
        let logical = LogicalCredentialId::new("logical-refresh").unwrap();
        let physical = PhysicalSecretSlot::allocate(&logical);
        let plan =
            SecretBundleStagePlan::with_slot(logical.clone(), physical.clone(), None).unwrap();
        let access = SecretString::new("existing-access-token".to_owned());
        stage_secret_bundle(
            &store,
            &plan,
            secret::SecretBundleRef::new(&access, None, None),
        )
        .unwrap();

        let outcome = refresh_access_token_for_slot(
            &RefreshCoordinator::new(),
            TIMEOUT,
            &store,
            &logical,
            &physical,
            &params("https://as.example/token".to_owned()),
            |_, _| panic!("missing refresh token must not publish"),
        )
        .unwrap();
        assert!(matches!(
            outcome,
            RefreshOutcome::ReauthorizationRequired { .. }
        ));
        assert_eq!(
            store.value(physical.as_str()).as_deref(),
            Some("existing-access-token")
        );
        assert_eq!(store.value(&physical.refresh_entry_id()), None);
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
                refresh_access_token(&coordinator, TIMEOUT, &*store, "cred", &request_params)
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

    #[test]
    fn typed_refresh_publish실패는_waiter에게_성공으로_공유되지_않는다() {
        use std::sync::{Barrier, mpsc};

        let store = Arc::new(MemStore::default());
        let logical = Arc::new(LogicalCredentialId::new("logical-race").unwrap());
        let physical = Arc::new(PhysicalSecretSlot::allocate(&logical));
        let plan = SecretBundleStagePlan::with_slot(
            logical.as_ref().clone(),
            physical.as_ref().clone(),
            None,
        )
        .unwrap();
        let access = SecretString::new("existing-access-token".to_owned());
        let refresh = SecretString::new("existing-refresh-token".to_owned());
        let dcr = SecretString::new("existing-dcr-secret".to_owned());
        stage_secret_bundle(
            &*store,
            &plan,
            secret::SecretBundleRef::new(&access, Some(&refresh), Some(&dcr)),
        )
        .unwrap();

        let coordinator = Arc::new(RefreshCoordinator::new());
        let publish_started = Arc::new(Barrier::new(2));
        let publish_release = Arc::new(Barrier::new(2));
        let first = {
            let store = Arc::clone(&store);
            let logical = Arc::clone(&logical);
            let physical = Arc::clone(&physical);
            let coordinator = Arc::clone(&coordinator);
            let publish_started = Arc::clone(&publish_started);
            let publish_release = Arc::clone(&publish_release);
            std::thread::spawn(move || {
                refresh_access_token_for_slot_with(
                    &coordinator,
                    &*store,
                    &logical,
                    &physical,
                    |_| {
                        Ok(RefreshOutcome::Refreshed(OAuthToken {
                            access_token: SecretString::new("rotated-at".to_owned()),
                            refresh_token: Some(SecretString::new("rotated-rt".to_owned())),
                            expires_in_secs: Some(3600),
                            provider_workspace_id: None,
                        }))
                    },
                    |_, dcr_secret| {
                        assert_eq!(
                            dcr_secret.map(SecretString::expose),
                            Some("existing-dcr-secret")
                        );
                        publish_started.wait();
                        publish_release.wait();
                        anyhow::bail!("injected pointer publish failure")
                    },
                )
            })
        };
        publish_started.wait();

        let (done_tx, done_rx) = mpsc::channel();
        let second = {
            let store = Arc::clone(&store);
            let logical = Arc::clone(&logical);
            let physical = Arc::clone(&physical);
            let coordinator = Arc::clone(&coordinator);
            std::thread::spawn(move || {
                let result = refresh_access_token_for_slot_with(
                    &coordinator,
                    &*store,
                    &logical,
                    &physical,
                    |_| panic!("waiter must not exchange after leader failure"),
                    |_, _| panic!("waiter must not publish after leader failure"),
                );
                done_tx.send(result).unwrap();
            })
        };

        assert!(
            matches!(
                done_rx.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "waiter completed before leader publish finished"
        );
        publish_release.wait();
        assert!(first.join().unwrap().is_err());
        let waiter = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            waiter.is_err(),
            "waiter must receive shared publish failure"
        );
        second.join().unwrap();
        assert_eq!(
            store.value(physical.as_str()).as_deref(),
            Some("existing-access-token")
        );
    }

    #[test]
    fn typed_refresh_rejects_slot_from_another_logical_id_before_keyring_access() {
        let store = MemStore::default();
        let logical = LogicalCredentialId::new("expected-logical").unwrap();
        let other = LogicalCredentialId::new("other-logical").unwrap();
        let other_slot = PhysicalSecretSlot::allocate(&other);

        let result = refresh_access_token_for_slot(
            &RefreshCoordinator::new(),
            TIMEOUT,
            &store,
            &logical,
            &other_slot,
            &params("https://as.example/token".to_owned()),
            |_, _| panic!("mismatched slot must not publish"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn refresh_debug는_binding과_failure_detail을_숨긴다() {
        let params = RefreshParams {
            token_url: "https://auth.example.test/token".to_owned(),
            client_id: "client-identifier".to_owned(),
            client_secret: Some(SecretString::new("client-secret-value".to_owned())),
            client_secret_post: true,
            resource: Some("https://mcp.example.test/mcp".to_owned()),
        };
        let params_debug = format!("{params:?}");
        for forbidden in [
            "auth.example.test",
            "client-identifier",
            "client-secret-value",
            "mcp.example.test",
        ] {
            assert!(!params_debug.contains(forbidden), "{params_debug}");
        }

        let outcome = RefreshOutcome::ReauthorizationRequired {
            reason: "provider raw failure".to_owned(),
        };
        assert!(!format!("{outcome:?}").contains("provider raw failure"));
    }
}
