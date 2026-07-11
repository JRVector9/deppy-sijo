//! 웹푸시 (VAPID) — 앱(폰 브라우저)이 닫혀 있어도 승인 요청을 알린다 (계획 PR-P4).
//!
//! 데스크톱 앱이 켜져 있는 한, 이 계층이 pending 승인/세션 상태를 폰의 푸시 서비스로 보낸다.
//! 폰 브라우저가 꺼져 있어도 서비스 워커(assets/sw.js)가 깨어 알림을 띄운다.
//!
//! ## 암호 스택 (수제 — 이 트랙 최대 리스크, RFC 벡터로 상쇄)
//!   - **VAPID JWT (RFC 8292)**: ES256(P-256 + SHA-256). aud=endpoint origin, exp≤24h,
//!     sub=고정 mailto. `Authorization: vapid t=<jwt>, k=<공개키 base64url>`.
//!   - **본문 암호화 (RFC 8291 aes128gcm)**: 서버 임시 ECDH(P-256) → HKDF-SHA256으로 IKM →
//!     HKDF로 CEK(16)/NONCE(12) → AES-128-GCM 단일 레코드. RFC 8291 부록 A 고정 벡터로
//!     라운드트립 단위 테스트한다([`tests::rfc8291_부록_고정벡터_왕복`]).
//!   - 개인키(VAPID)는 keyring(SecretStore, tls_identity 관례). 공개키만 JS에 노출.
//!
//! ## 스레드/리소스 규율
//!   - 발송은 **전용 스레드** 하나가 담당한다(WS/대시보드 브리지 스레드를 네트워크 I/O로
//!     블로킹하지 않는다).
//!   - 승인은 **구독이 ≥1일 때만** 5초 주기로 DB를 폴링한다 — 구독 0이면 스레드가 park해
//!     폴링이 완전히 정지한다(0%p). 상태(완료/입력대기)는 P2 대시보드 브리지가 이미 보는
//!     이벤트를 [`PushHandle::notify_session`]으로 넘겨받는다(새 구독 없이 재사용).
//!   - 이미 알림 보낸 승인 id/세션 상태는 인메모리로 기억해 중복 발송하지 않는다.
//!
//! ## 페이로드 최소화 (redaction 확장 — push service 제3자 경유)
//!   본문은 **종류/제목/개수만** 담는다. 도구 인자·로그·서버/도구 이름은 절대 싣지 않는다
//!   (RFC 8291로 E2E 암호화되지만 방어적으로 최소화). 딥링크는 셸 열기/포커스뿐이다.
//!
//! ## iOS 제약
//!   iOS 웹푸시는 **홈 화면 설치형(standalone) + 사용자 제스처 + iOS 16.4+** 에서만 동작한다
//!   (assets/app.js가 제스처 안에서 구독한다). 데스크톱 notify-rust 알림과는 **중복 억제 없음**
//!   — 다른 기기이므로 각자 알린다.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hkdf::Hkdf;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use secret::{SecretStore, SecretString};
use sha2::Sha256;

use crate::http::Response;

/// VAPID 개인키 keyring entry id. rotation 시 `-2`로 올린다 (tls_identity·pairing 관례).
const VAPID_KEY_ID: &str = "web-push-vapid-key-1";
/// VAPID `sub` 클레임(RFC 8292 §2.1) — 연락 URI 고정값. 개인정보 아님.
const VAPID_SUB: &str = "mailto:deppy-sijo@localhost";
/// VAPID JWT 유효기간 — RFC 8292는 24h 상한. 여유를 두어 12h.
const JWT_TTL_SECS: u64 = 12 * 60 * 60;
/// 승인 DB 폴링 주기 — 구독이 ≥1일 때만 적용(계획: 구독 0이면 폴링 정지).
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// 푸시 서비스 TTL 헤더(RFC 8030) — 오프라인 폰이 온라인 될 때까지 보관할 시간(초).
const PUSH_TTL_SECS: u32 = 24 * 60 * 60;
/// Urgency 헤더(RFC 8030) — 승인/상태는 즉시성이 중요.
const PUSH_URGENCY: &str = "high";
/// RFC 8188 레코드 크기(rs). 단일 레코드라 평문+17바이트보다 크기만 하면 된다 — RFC 8291
/// 부록 예제와 동일하게 4096.
const RECORD_SIZE: u32 = 4096;
/// 발송 HTTP 타임아웃 — 죽은 endpoint가 스레드를 무한정 잡지 않게.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

// ─────────────────────────────────────────────────────────────────────────────
// VAPID 키 (ES256)
// ─────────────────────────────────────────────────────────────────────────────

/// VAPID 서명 신원 — ES256(P-256) 개인키 + 노출용 공개키(uncompressed base64url).
pub struct VapidKey {
    signing: p256::ecdsa::SigningKey,
    /// 65바이트 uncompressed SEC1 공개키(0x04‖X‖Y)의 base64url — JS applicationServerKey +
    /// JWT `k=` 파라미터. 노출용 값이라 미리 인코딩해 둔다.
    public_b64: String,
}

impl std::fmt::Debug for VapidKey {
    /// 개인키는 Debug에 싣지 않는다(tls_identity 관례 — 유출 방지).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VapidKey")
            .field("public_b64", &self.public_b64)
            .field("signing", &"<elided>")
            .finish()
    }
}

impl VapidKey {
    /// 개인키 32바이트 스칼라에서 복원한다.
    fn from_scalar(bytes: &[u8]) -> anyhow::Result<Self> {
        let signing = p256::ecdsa::SigningKey::from_slice(bytes)
            .context("VAPID 개인키가 유효한 P-256 스칼라가 아님")?;
        let point = signing.verifying_key().to_encoded_point(false);
        let public_raw: [u8; 65] = point
            .as_bytes()
            .try_into()
            .context("VAPID 공개키 uncompressed 인코딩 길이 오류")?;
        let public_b64 = URL_SAFE_NO_PAD.encode(public_raw);
        Ok(Self {
            signing,
            public_b64,
        })
    }

    /// 새 키쌍을 만든다(OS 엔트로피). 테스트(서버 통합)도 쓰도록 crate 가시성.
    pub(crate) fn generate() -> Self {
        // from_scalar는 유효 스칼라에만 성공 — random_p256_scalar가 이미 검증한 바이트라 안전.
        Self::from_scalar(&random_p256_scalar()).expect("검증된 스칼라로 VAPID 키 생성")
    }

    /// 개인키 스칼라 32바이트(keyring 저장용 hex 인코딩 전).
    fn scalar_bytes(&self) -> [u8; 32] {
        self.signing.to_bytes().into()
    }

    /// JS·JWT에 노출할 공개키 base64url(uncompressed).
    pub fn public_key_b64url(&self) -> &str {
        &self.public_b64
    }

    /// VAPID JWT(RFC 8292)를 서명한다. `aud`=endpoint origin, `exp`=now+TTL(≤24h), sub 고정.
    fn sign_jwt(&self, aud: &str, now_secs: u64) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        // 클레임은 세 필드만 — aud/exp/sub. 문자열 이스케이프가 필요 없는 값(aud=origin,
        // sub=고정 mailto)이라 수제 조립해도 안전하다.
        let exp = now_secs + JWT_TTL_SECS;
        let claims = format!(r#"{{"aud":"{aud}","exp":{exp},"sub":"{VAPID_SUB}"}}"#);
        let claims = URL_SAFE_NO_PAD.encode(claims);
        let signing_input = format!("{header}.{claims}");
        // ES256: ECDSA(P-256, SHA-256) — signature는 고정 64바이트 r‖s(JWS 규약).
        use p256::ecdsa::signature::Signer;
        let signature: p256::ecdsa::Signature = self.signing.sign(signing_input.as_bytes());
        let sig_b64 = URL_SAFE_NO_PAD.encode(signature.to_bytes());
        format!("{signing_input}.{sig_b64}")
    }

    /// `Authorization: vapid` 헤더 값(RFC 8292 §3.1) — t=JWT, k=공개키.
    fn authorization_header(&self, aud: &str, now_secs: u64) -> String {
        format!(
            "vapid t={}, k={}",
            self.sign_jwt(aud, now_secs),
            self.public_b64
        )
    }
}

/// keyring의 VAPID 키를 읽고, **확인된 부재**면 새로 만들어 저장한다 (tls_identity 관례:
/// keyring 오류는 부재로 오판하지 않고 bail — 살아있는 키를 덮어쓰지 않는다). app이 호출해
/// 서버에 주입한다(SecretStore 접근이 app 소유).
pub fn get_or_create_vapid_key(store: &dyn SecretStore) -> anyhow::Result<VapidKey> {
    if store
        .has_secret(VAPID_KEY_ID)
        .context("VAPID 키 존재 확인 실패")?
    {
        let hex = store
            .get_secret(VAPID_KEY_ID)
            .context("VAPID 키 keyring 읽기 실패")?;
        let bytes = from_hex(hex.expose()).context("keyring의 VAPID 키가 hex가 아님")?;
        return VapidKey::from_scalar(&bytes);
    }
    let key = VapidKey::generate();
    store
        .set_secret(
            VAPID_KEY_ID,
            &SecretString::new(to_hex(&key.scalar_bytes())),
        )
        .context("VAPID 개인키 keyring 저장 실패")?;
    Ok(key)
}

// ─────────────────────────────────────────────────────────────────────────────
// RFC 8291 aes128gcm 본문 암호화
// ─────────────────────────────────────────────────────────────────────────────

/// RFC 8291 §3.4 + RFC 8188에 따라 CEK(16)/NONCE(12)를 유도한다. 인자는 모두 raw 바이트.
/// 테스트가 RFC 부록 A의 CEK/NONCE와 대조한다.
fn derive_content_keys(
    ecdh_secret: &[u8],
    auth_secret: &[u8],
    ua_public: &[u8; 65],
    as_public: &[u8; 65],
    salt: &[u8; 16],
) -> anyhow::Result<([u8; 16], [u8; 12])> {
    // 1) IKM = HKDF(salt=auth_secret, ikm=ecdh_secret, info="WebPush: info"‖0x00‖ua‖as, L=32)
    let mut key_info = Vec::with_capacity(14 + 65 + 65);
    key_info.extend_from_slice(b"WebPush: info\0");
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public);
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth_secret), ecdh_secret)
        .expand(&key_info, &mut ikm)
        .map_err(|_| anyhow::anyhow!("IKM HKDF-Expand 실패"))?;

    // 2) PRK = HKDF-Extract(salt, IKM); CEK/NONCE = HKDF-Expand(정보 문자열, L)
    let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0u8; 16];
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
        .map_err(|_| anyhow::anyhow!("CEK HKDF-Expand 실패"))?;
    let mut nonce = [0u8; 12];
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce)
        .map_err(|_| anyhow::anyhow!("NONCE HKDF-Expand 실패"))?;
    Ok((cek, nonce))
}

/// 단일 aes128gcm 레코드 본문을 만든다(RFC 8188 헤더 ‖ 암호문). 서버 임시키(`as_secret`)와
/// `salt`를 인자로 받아 **결정적**으로 만든다 — RFC 벡터 대조가 가능한 테스트 seam.
fn seal(
    ua_public: &p256::PublicKey,
    auth_secret: &[u8],
    plaintext: &[u8],
    as_secret: &p256::SecretKey,
    salt: &[u8; 16],
) -> anyhow::Result<Vec<u8>> {
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes128Gcm, KeyInit, Nonce};

    let as_public_point = as_secret.public_key().to_encoded_point(false);
    let as_public: [u8; 65] = as_public_point
        .as_bytes()
        .try_into()
        .context("서버 임시 공개키 인코딩 길이 오류")?;
    let ua_public_point = ua_public.to_encoded_point(false);
    let ua_public_raw: [u8; 65] = ua_public_point
        .as_bytes()
        .try_into()
        .context("수신자 공개키 인코딩 길이 오류")?;

    // ECDH(P-256) 공유 비밀 = 공유점의 X 좌표(32바이트).
    let shared = p256::ecdh::diffie_hellman(as_secret.to_nonzero_scalar(), ua_public.as_affine());
    let (cek, nonce) = derive_content_keys(
        shared.raw_secret_bytes(),
        auth_secret,
        &ua_public_raw,
        &as_public,
        salt,
    )?;

    // 레코드 = 평문 ‖ 0x02(마지막 레코드 패딩 구분자). 단일 레코드라 추가 패딩 없음.
    let mut record = Vec::with_capacity(plaintext.len() + 1);
    record.extend_from_slice(plaintext);
    record.push(0x02);
    let cipher =
        Aes128Gcm::new_from_slice(&cek).map_err(|_| anyhow::anyhow!("AES-128 키 길이 오류"))?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), record.as_slice())
        .map_err(|_| anyhow::anyhow!("AES-128-GCM 암호화 실패"))?;

    // RFC 8188 헤더: salt(16) ‖ rs(4, big-endian) ‖ idlen(1) ‖ keyid(=as_public, 65) ‖ 암호문
    let mut body = Vec::with_capacity(16 + 4 + 1 + 65 + ciphertext.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(as_public.len() as u8);
    body.extend_from_slice(&as_public);
    body.extend_from_slice(&ciphertext);
    Ok(body)
}

/// 구독(base64url p256dh/auth)에 보낼 aes128gcm 본문을 만든다. 서버 임시 ECDH 키와 salt를
/// 매 호출 무작위로 뽑는다(RFC 8291 요건).
fn encrypt_payload(p256dh_b64: &str, auth_b64: &str, plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    let ua_bytes = decode_b64_loose(p256dh_b64).context("구독 p256dh 디코드 실패")?;
    let ua_public = p256::PublicKey::from_sec1_bytes(&ua_bytes)
        .context("구독 p256dh가 유효한 P-256 점이 아님")?;
    let auth_secret = decode_b64_loose(auth_b64).context("구독 auth 디코드 실패")?;
    anyhow::ensure!(auth_secret.len() == 16, "auth secret은 16바이트여야 함");

    let as_secret = p256::SecretKey::from_slice(&random_p256_scalar())
        .expect("검증된 스칼라로 임시 ECDH 키 생성");
    let mut salt = [0u8; 16];
    getrandom::getrandom(&mut salt).expect("OS 엔트로피(salt) 획득 실패");
    seal(&ua_public, &auth_secret, plaintext, &as_secret, &salt)
}

// ─────────────────────────────────────────────────────────────────────────────
// 발송 트랜스포트 (테스트 주입 가능)
// ─────────────────────────────────────────────────────────────────────────────

/// 트랜스포트 네트워크 실패 — 상태코드조차 받지 못한 경우(재시도 대상). 상세는 로그로만.
#[derive(Debug)]
pub struct TransportError;

/// 푸시 endpoint로의 단일 POST. 발송 계층과 네트워크를 분리해 테스트가 410/실패를 주입한다.
pub trait PushTransport: Send + Sync {
    /// Ok(status)=HTTP 상태코드 수신, Err=네트워크 실패(재시도 대상).
    fn post(
        &self,
        endpoint: &str,
        ttl: u32,
        urgency: &str,
        authorization: &str,
        body: &[u8],
    ) -> Result<u16, TransportError>;
}

/// 운영용 ureq(sync) 트랜스포트.
struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    fn new() -> Self {
        Self {
            agent: ureq::AgentBuilder::new().timeout(HTTP_TIMEOUT).build(),
        }
    }
}

impl PushTransport for UreqTransport {
    fn post(
        &self,
        endpoint: &str,
        ttl: u32,
        urgency: &str,
        authorization: &str,
        body: &[u8],
    ) -> Result<u16, TransportError> {
        let result = self
            .agent
            .post(endpoint)
            .set("TTL", &ttl.to_string())
            .set("Urgency", urgency)
            .set("Authorization", authorization)
            .set("Content-Encoding", "aes128gcm")
            .set("Content-Type", "application/octet-stream")
            .send_bytes(body);
        match result {
            Ok(resp) => Ok(resp.status()),
            // ureq는 4xx/5xx를 Err(Status)로 준다 — 상태코드는 정상 수신이므로 Ok로 되돌린다.
            Err(ureq::Error::Status(code, _)) => Ok(code),
            // 네트워크/전송 실패 — 재시도 대상.
            Err(ureq::Error::Transport(_)) => Err(TransportError),
        }
    }
}

/// 한 구독으로의 발송 결과.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    /// 2xx — last_ok_at 갱신.
    Ok,
    /// 404/410 — 죽은 구독, 즉시 삭제.
    Gone,
    /// 재시도 1회 후에도 실패 — 이번은 포기(구독은 유지).
    Failed,
}

// ─────────────────────────────────────────────────────────────────────────────
// 알림 페이로드 (종류/제목/개수만)
// ─────────────────────────────────────────────────────────────────────────────

/// 발송할 알림 한 건. 페이로드에는 종류/제목/개수만 담는다(도구 인자·로그 금지).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Notification {
    /// pending 승인 — 개수 포함.
    Approval { count: usize },
    /// 세션 완료.
    SessionDone,
    /// 세션 입력 대기.
    SessionWaiting,
}

impl Notification {
    /// sw.js가 알림으로 렌더할 최소 JSON. `tag`로 같은 종류 알림을 합친다(스택 방지).
    fn payload(&self) -> String {
        match self {
            Notification::Approval { count } => serde_json::json!({
                "kind": "approval",
                "title": "승인 요청",
                "count": count,
                "tag": "deppy-approval",
            }),
            Notification::SessionDone => serde_json::json!({
                "kind": "done",
                "title": "세션 완료",
                "tag": "deppy-session",
            }),
            Notification::SessionWaiting => serde_json::json!({
                "kind": "waiting",
                "title": "입력 대기",
                "tag": "deppy-session",
            }),
        }
        .to_string()
    }
}

/// 대시보드 브리지가 넘기는 세션 상태 알림(입력대기/완료). 승인은 DB 폴링이 담당하므로 여기 없다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionKind {
    Done,
    Waiting,
}

// ─────────────────────────────────────────────────────────────────────────────
// PushManager — 전용 발송 스레드 + 구독/트리거 상태
// ─────────────────────────────────────────────────────────────────────────────

struct PushInner {
    /// 즉시 1회 재평가 강제(구독 등록 직후).
    force: bool,
    /// 대시보드 브리지가 넣은 세션 상태 알림 큐.
    jobs: VecDeque<(u64, SessionKind)>,
    /// 이미 푸시한 pending 승인 id — 중복 발송 방지. pending에서 사라지면 정리(유계).
    notified_approvals: HashSet<String>,
    /// 세션별 마지막으로 알린 상태 — 같은 전이 반복 발송 방지.
    notified_status: HashMap<u64, SessionKind>,
}

struct PushShared {
    inner: Mutex<PushInner>,
    cvar: Condvar,
    /// 발송 스레드 전용 DB 연결(대시보드와 별도 — SQLite 다중 연결 관례). 구독 CRUD·승인 폴링.
    db: Mutex<storage::Db>,
    vapid: VapidKey,
    transport: Box<dyn PushTransport>,
    /// 현재 구독 수(폴링 게이트). 0이면 스레드가 park해 폴링이 정지한다.
    sub_count: AtomicUsize,
    stop: AtomicBool,
    /// 승인 DB 폴링 횟수(테스트: 구독 0에서 폴링 정지 검증).
    poll_count: AtomicU64,
    /// 실제 전송(transport.post) 호출 수(테스트: 중복 억제/410 정리 검증).
    sent_count: AtomicU64,
}

/// 복제 가능한 핸들 — 접속 스레드(구독 등록)·대시보드 브리지(상태 알림)가 공유한다.
#[derive(Clone)]
pub struct PushHandle {
    shared: Arc<PushShared>,
}

/// 발송 스레드를 소유하는 매니저 — 서버가 보유하고 shutdown 시 stop+join한다.
pub struct PushManager {
    handle: PushHandle,
    thread: Option<JoinHandle<()>>,
}

impl PushManager {
    /// 운영용(ureq 트랜스포트) 발송 매니저를 띄운다. `db_path`로 자체 DB 연결을 연다.
    pub fn spawn(db_path: PathBuf, vapid: VapidKey) -> anyhow::Result<Self> {
        Self::spawn_with_transport(db_path, vapid, Box::new(UreqTransport::new()))
    }

    /// 트랜스포트를 주입해 띄운다(테스트: 410/실패 시뮬레이션).
    fn spawn_with_transport(
        db_path: PathBuf,
        vapid: VapidKey,
        transport: Box<dyn PushTransport>,
    ) -> anyhow::Result<Self> {
        let db = storage::Db::open(&db_path).context("web-remote 웹푸시 DB 열기 실패")?;
        let initial = db.count_web_push_subscriptions().unwrap_or(0).max(0) as usize;
        let shared = Arc::new(PushShared {
            inner: Mutex::new(PushInner {
                force: false,
                jobs: VecDeque::new(),
                notified_approvals: HashSet::new(),
                notified_status: HashMap::new(),
            }),
            cvar: Condvar::new(),
            db: Mutex::new(db),
            vapid,
            transport,
            sub_count: AtomicUsize::new(initial),
            stop: AtomicBool::new(false),
            poll_count: AtomicU64::new(0),
            sent_count: AtomicU64::new(0),
        });
        let thread = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("web-remote-push".into())
                .spawn(move || run(&shared))
                .context("web-remote 푸시 스레드 생성 실패")?
        };
        Ok(Self {
            handle: PushHandle { shared },
            thread: Some(thread),
        })
    }

    /// 접속 스레드/브리지에 공유할 핸들.
    pub fn handle(&self) -> PushHandle {
        self.handle.clone()
    }

    /// 발송 스레드를 정지·join한다(서버 shutdown).
    pub fn stop_and_join(mut self) {
        self.handle.shared.stop.store(true, Ordering::SeqCst);
        self.handle.shared.cvar.notify_all();
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl PushHandle {
    /// JS에 노출할 VAPID 공개키(base64url).
    pub fn vapid_public_key(&self) -> &str {
        self.shared.vapid.public_key_b64url()
    }

    /// 구독을 등록/갱신한다(POST /push/subscribe). DB에 쓰고 스레드를 깨워 즉시 재평가시킨다.
    pub fn add_subscription(&self, endpoint: &str, p256dh: &str, auth: &str) -> anyhow::Result<()> {
        {
            let db = self.shared.db.lock().expect("push db lock");
            db.upsert_web_push_subscription(endpoint, p256dh, auth, now_secs() as i64)?;
            let count = db.count_web_push_subscriptions().unwrap_or(0).max(0) as usize;
            self.shared.sub_count.store(count, Ordering::SeqCst);
        }
        {
            let mut inner = self.shared.inner.lock().expect("push inner lock");
            inner.force = true;
        }
        self.shared.cvar.notify_all();
        Ok(())
    }

    /// 대시보드 브리지가 세션 상태 전이를 넘긴다(입력대기/완료만). 비-blocking(큐 적재 + 깨움).
    /// 승인 상태(NeedsApproval)는 DB 폴링이 담당하므로 여기서는 무시한다(중복 방지).
    pub fn notify_session(&self, session: u64, status: runtime::SessionStatus) {
        let kind = match status {
            runtime::SessionStatus::Done => SessionKind::Done,
            runtime::SessionStatus::Waiting => SessionKind::Waiting,
            _ => return,
        };
        {
            let mut inner = self.shared.inner.lock().expect("push inner lock");
            inner.jobs.push_back((session, kind));
        }
        self.shared.cvar.notify_all();
    }

    /// 지금까지의 승인 DB 폴링 횟수(테스트).
    pub fn poll_count(&self) -> u64 {
        self.shared.poll_count.load(Ordering::SeqCst)
    }

    /// 지금까지의 실제 전송 호출 수(테스트).
    pub fn sent_count(&self) -> u64 {
        self.shared.sent_count.load(Ordering::SeqCst)
    }
}

/// 발송 스레드 본체. cvar 대기 → 세션 알림 drain + 승인 폴링 → 발송을 반복한다.
fn run(shared: &Arc<PushShared>) {
    loop {
        let jobs;
        let do_poll;
        {
            let mut inner = shared.inner.lock().expect("push inner lock");
            // 대기: stop / force / 세션 작업 / (구독>0 && 폴링 주기) 중 하나까지.
            loop {
                if shared.stop.load(Ordering::SeqCst) {
                    return;
                }
                let subs = shared.sub_count.load(Ordering::SeqCst);
                let ready = inner.force || !inner.jobs.is_empty() || subs > 0;
                if ready {
                    break;
                }
                // 구독 0 + 작업 없음 — 무기한 park(타이머 없음 → 폴링 완전 정지, CPU 0).
                inner = shared.cvar.wait(inner).expect("push cvar wait");
            }
            if shared.stop.load(Ordering::SeqCst) {
                return;
            }
            let subs = shared.sub_count.load(Ordering::SeqCst);
            // 세션 작업 큐를 통째로 꺼내 락 밖에서 처리한다(구독 0이면 아래에서 skip).
            jobs = std::mem::take(&mut inner.jobs);
            do_poll = subs > 0;
            inner.force = false;
        }

        // 세션 상태 알림 — 중복 억제(같은 세션·같은 상태는 1회).
        for (session, kind) in jobs {
            let subs = shared.sub_count.load(Ordering::SeqCst);
            if subs == 0 {
                continue;
            }
            let already = {
                let mut inner = shared.inner.lock().expect("push inner lock");
                let dup = inner.notified_status.get(&session) == Some(&kind);
                if !dup {
                    inner.notified_status.insert(session, kind);
                }
                dup
            };
            if !already {
                let note = match kind {
                    SessionKind::Done => Notification::SessionDone,
                    SessionKind::Waiting => Notification::SessionWaiting,
                };
                broadcast(shared, &note);
            }
        }

        // 승인 폴링 — 구독>0에서만. 새 pending id가 있을 때만 1건 발송(개수 포함).
        if do_poll {
            shared.poll_count.fetch_add(1, Ordering::SeqCst);
            poll_and_notify_approvals(shared);
            // 다음 폴링 주기까지 대기(구독>0). 그 사이 force/세션작업/stop이 깨운다.
            let inner = shared.inner.lock().expect("push inner lock");
            if !inner.force && inner.jobs.is_empty() && !shared.stop.load(Ordering::SeqCst) {
                let _ = shared.cvar.wait_timeout(inner, POLL_INTERVAL);
            }
        }
    }
}

/// pending 승인을 폴링해 새 id가 있으면 개수 알림을 1건 보낸다. 이미 알린 id는 건너뛰고,
/// pending에서 사라진 id는 기억에서 지운다(유계).
fn poll_and_notify_approvals(shared: &Arc<PushShared>) {
    let pending = {
        let db = shared.db.lock().expect("push db lock");
        match db.list_pending_approvals() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("웹푸시 승인 폴링 실패: {e:#}");
                return;
            }
        }
    };
    let current_ids: HashSet<String> = pending.iter().map(|row| row.id.clone()).collect();
    let has_new = {
        let mut inner = shared.inner.lock().expect("push inner lock");
        // pending에서 사라진 id 정리(해소된 승인) — 인메모리 기록 유계.
        inner
            .notified_approvals
            .retain(|id| current_ids.contains(id));
        let has_new = pending
            .iter()
            .any(|row| !inner.notified_approvals.contains(&row.id));
        if has_new {
            for id in &current_ids {
                inner.notified_approvals.insert(id.clone());
            }
        }
        has_new
    };
    if has_new {
        broadcast(
            shared,
            &Notification::Approval {
                count: pending.len(),
            },
        );
    }
}

/// 모든 구독에 알림을 보낸다. 성공은 last_ok_at 갱신, 410/404는 구독 삭제, 그 외 실패는 포기.
fn broadcast(shared: &Arc<PushShared>, note: &Notification) {
    let subs = {
        let db = shared.db.lock().expect("push db lock");
        db.list_web_push_subscriptions().unwrap_or_default()
    };
    if subs.is_empty() {
        return;
    }
    let payload = note.payload();
    let mut changed = false;
    for sub in subs {
        match deliver(shared, &sub, payload.as_bytes()) {
            Delivery::Ok => {
                let db = shared.db.lock().expect("push db lock");
                let _ = db.touch_web_push_subscription(&sub.endpoint, now_secs() as i64);
            }
            Delivery::Gone => {
                let db = shared.db.lock().expect("push db lock");
                let _ = db.delete_web_push_subscription(&sub.endpoint);
                changed = true;
                tracing::info!("웹푸시 구독 만료(410/404) — 삭제");
            }
            Delivery::Failed => {
                tracing::warn!("웹푸시 발송 실패(재시도 후 포기)");
            }
        }
    }
    if changed {
        let count = shared
            .db
            .lock()
            .expect("push db lock")
            .count_web_push_subscriptions()
            .unwrap_or(0)
            .max(0) as usize;
        shared.sub_count.store(count, Ordering::SeqCst);
    }
}

/// 한 구독으로 발송한다: VAPID JWT + RFC 8291 암호화 + POST(재시도 1회). 암호화 실패는
/// 즉시 포기(구독 데이터가 깨진 경우 — 재시도 무의미).
fn deliver(
    shared: &Arc<PushShared>,
    sub: &storage::WebPushSubscriptionRow,
    payload: &[u8],
) -> Delivery {
    let Some(aud) = endpoint_origin(&sub.endpoint) else {
        tracing::warn!("웹푸시 endpoint origin 파싱 실패 — 건너뜀");
        return Delivery::Failed;
    };
    let body = match encrypt_payload(&sub.p256dh, &sub.auth, payload) {
        Ok(body) => body,
        Err(e) => {
            tracing::warn!("웹푸시 본문 암호화 실패: {e:#}");
            return Delivery::Failed;
        }
    };
    let authorization = shared.vapid.authorization_header(&aud, now_secs());

    // 재시도 1회 — 폭주 방지. 410/404는 재시도 없이 즉시 Gone.
    for attempt in 0..2 {
        shared.sent_count.fetch_add(1, Ordering::SeqCst);
        match shared.transport.post(
            &sub.endpoint,
            PUSH_TTL_SECS,
            PUSH_URGENCY,
            &authorization,
            &body,
        ) {
            Ok(status) if (200..300).contains(&status) => return Delivery::Ok,
            Ok(404) | Ok(410) => return Delivery::Gone,
            Ok(_) => {
                if attempt == 1 {
                    return Delivery::Failed;
                }
            }
            Err(TransportError) => {
                if attempt == 1 {
                    return Delivery::Failed;
                }
            }
        }
    }
    Delivery::Failed
}

// ─────────────────────────────────────────────────────────────────────────────
// HTTP 라우팅 — GET /push/vapid, POST /push/subscribe (토큰 게이트 필수)
// ─────────────────────────────────────────────────────────────────────────────

/// `/push/*` 요청을 처리한다. 해당 경로가 아니면 None(정적 라우팅으로 흘려보냄). push가
/// 비활성(VAPID 키 없음)이면 404. 등록 엔드포인트는 페어링 토큰 게이트 필수(무단 등록 차단).
pub fn route(
    head: &crate::http::RequestHead,
    body: &[u8],
    token: &str,
    push: Option<&PushHandle>,
) -> Option<Response> {
    match (head.method.as_str(), head.path.as_str()) {
        ("GET", "/push/vapid") => Some(vapid_key_response(&head.query, token, push)),
        ("POST", "/push/subscribe") => Some(subscribe_response(&head.query, body, token, push)),
        _ => None,
    }
}

/// VAPID 공개키를 JSON으로 노출한다(구독 UI의 applicationServerKey). 토큰 게이트.
fn vapid_key_response(query: &str, token: &str, push: Option<&PushHandle>) -> Response {
    if !token_query_matches(query, token) {
        return Response::plain(401, "unauthorized");
    }
    let Some(push) = push else {
        return Response::plain(404, "push disabled");
    };
    let body = serde_json::json!({ "key": push.vapid_public_key() }).to_string();
    Response {
        status: 200,
        content_type: "application/json",
        body: std::borrow::Cow::Owned(body.into_bytes()),
    }
}

/// 구독 등록 — 토큰 게이트 + JSON 본문 {endpoint, keys:{p256dh, auth}}.
fn subscribe_response(
    query: &str,
    body: &[u8],
    token: &str,
    push: Option<&PushHandle>,
) -> Response {
    if !token_query_matches(query, token) {
        return Response::plain(401, "unauthorized");
    }
    let Some(push) = push else {
        return Response::plain(404, "push disabled");
    };
    let parsed: Result<SubscribeRequest, _> = serde_json::from_slice(body);
    let Ok(sub) = parsed else {
        return Response::plain(400, "bad subscription");
    };
    if sub.endpoint.is_empty() || sub.keys.p256dh.is_empty() || sub.keys.auth.is_empty() {
        return Response::plain(400, "bad subscription");
    }
    match push.add_subscription(&sub.endpoint, &sub.keys.p256dh, &sub.keys.auth) {
        Ok(()) => Response::plain(201, "subscribed"),
        Err(e) => {
            tracing::warn!("웹푸시 구독 등록 실패: {e:#}");
            Response::plain(500, "subscribe failed")
        }
    }
}

#[derive(serde::Deserialize)]
struct SubscribeRequest {
    endpoint: String,
    keys: SubscribeKeys,
}

#[derive(serde::Deserialize)]
struct SubscribeKeys {
    p256dh: String,
    auth: String,
}

/// query의 `token=` 파라미터를 상수시간 비교한다(static_srv 게이트와 동일 규약).
fn token_query_matches(query: &str, expected: &str) -> bool {
    let Some(provided) = query.split('&').find_map(|kv| kv.strip_prefix("token=")) else {
        return false;
    };
    crate::static_srv::token_matches(expected, provided.as_bytes())
}

// ─────────────────────────────────────────────────────────────────────────────
// 소도구
// ─────────────────────────────────────────────────────────────────────────────

/// 현재 epoch 초.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// endpoint URL에서 origin(scheme://authority)을 뽑는다 — VAPID `aud`. url 크레이트 없이 파싱.
fn endpoint_origin(endpoint: &str) -> Option<String> {
    let (scheme, rest) = endpoint.split_once("://")?;
    if scheme.is_empty() || rest.is_empty() {
        return None;
    }
    let authority = rest.split('/').next().unwrap_or(rest);
    if authority.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{authority}"))
}

/// base64url(그리고 표준 base64) 관대 디코드 — 브라우저 구독 키는 보통 base64url 무패딩이지만
/// 패딩/표준 알파벳도 받아들인다.
fn decode_b64_loose(s: &str) -> anyhow::Result<Vec<u8>> {
    let normalized: String = s
        .trim()
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            other => other,
        })
        .filter(|c| *c != '=')
        .collect();
    URL_SAFE_NO_PAD
        .decode(normalized.as_bytes())
        .context("base64url 디코드 실패")
}

/// 유효한 P-256 스칼라 32바이트를 뽑는다(범위 밖은 재시도 — 확률 ~2^-32로 사실상 1회).
fn random_p256_scalar() -> [u8; 32] {
    loop {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes).expect("OS 엔트로피 획득 실패");
        if p256::SecretKey::from_slice(&bytes).is_ok() {
            return bytes;
        }
    }
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(s: &str) -> anyhow::Result<Vec<u8>> {
    let bytes = s.as_bytes();
    anyhow::ensure!(bytes.len().is_multiple_of(2), "hex 길이가 홀수");
    bytes
        .chunks_exact(2)
        .map(|pair| {
            let hi = hex_digit(pair[0])?;
            let lo = hex_digit(pair[1])?;
            Ok(hi << 4 | lo)
        })
        .collect()
}

fn hex_digit(byte: u8) -> anyhow::Result<u8> {
    (byte as char)
        .to_digit(16)
        .map(|d| d as u8)
        .with_context(|| format!("hex가 아닌 바이트: 0x{byte:02x}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    // ── RFC 8291 부록 A 고정 벡터 ────────────────────────────────────────────
    const RFC_AS_PRIVATE: &str = "yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw";
    const RFC_UA_PUBLIC: &str =
        "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    const RFC_AUTH: &str = "BTBZMqHH6r4Tts7J_aSIgg";
    const RFC_SALT: &str = "DGv6ra1nlYgDCS1FRnbzlw";
    const RFC_PLAINTEXT: &str = "When I grow up, I want to be a watermelon";
    const RFC_CEK: &str = "oIhVW04MRdy2XN9CiKLxTg";
    const RFC_NONCE: &str = "4h_95klXJ5E_qnoN";
    /// RFC 8291 §5의 전체 본문(헤더‖암호문, Content-Length 145) — base64url 무패딩.
    const RFC_BODY: &str = "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN";

    fn ub(s: &str) -> Vec<u8> {
        decode_b64_loose(s).unwrap()
    }

    #[test]
    fn rfc8291_부록_고정벡터_왕복() {
        // RFC의 고정 서버 임시키 + salt + 수신자 키로 암호화하면 §5의 본문 바이트가 나온다.
        let as_secret = p256::SecretKey::from_slice(&ub(RFC_AS_PRIVATE)).unwrap();
        let ua_public = p256::PublicKey::from_sec1_bytes(&ub(RFC_UA_PUBLIC)).unwrap();
        let auth = ub(RFC_AUTH);
        let salt: [u8; 16] = ub(RFC_SALT).try_into().unwrap();

        // 먼저 CEK/NONCE 중간값을 부록 A와 대조(실패 지점 국소화).
        let as_public_point = as_secret.public_key().to_encoded_point(false);
        let as_public: [u8; 65] = as_public_point.as_bytes().try_into().unwrap();
        let ua_public_point = ua_public.to_encoded_point(false);
        let ua_public_raw: [u8; 65] = ua_public_point.as_bytes().try_into().unwrap();
        let shared =
            p256::ecdh::diffie_hellman(as_secret.to_nonzero_scalar(), ua_public.as_affine());
        let (cek, nonce) = derive_content_keys(
            shared.raw_secret_bytes(),
            &auth,
            &ua_public_raw,
            &as_public,
            &salt,
        )
        .unwrap();
        assert_eq!(cek.to_vec(), ub(RFC_CEK), "CEK가 RFC 부록 A와 불일치");
        assert_eq!(nonce.to_vec(), ub(RFC_NONCE), "NONCE가 RFC 부록 A와 불일치");

        // 전체 본문(헤더‖암호문)이 §5와 바이트 단위로 일치.
        let body = seal(
            &ua_public,
            &auth,
            RFC_PLAINTEXT.as_bytes(),
            &as_secret,
            &salt,
        )
        .unwrap();
        assert_eq!(body, ub(RFC_BODY), "aes128gcm 본문이 RFC §5와 불일치");
    }

    #[test]
    fn 암호화는_무작위_임시키로도_유효한_구조를_만든다() {
        // 무작위 salt/임시키로 암호화해도 헤더 구조(salt16‖rs4‖idlen1‖keyid65)가 맞다.
        let body = encrypt_payload(RFC_UA_PUBLIC, RFC_AUTH, b"hi").unwrap();
        assert!(body.len() > 16 + 4 + 1 + 65, "본문이 헤더보다 커야 함");
        // rs = 4096 (big-endian 00 00 10 00)
        assert_eq!(&body[16..20], &[0x00, 0x00, 0x10, 0x00]);
        // idlen = 65 (keyid = 서버 임시 공개키)
        assert_eq!(body[20], 65, "keyid 길이(idlen)는 65");
    }

    // ── VAPID JWT ────────────────────────────────────────────────────────────
    #[test]
    fn vapid_jwt는_공개키로_검증되고_클레임이_맞다() {
        use p256::ecdsa::signature::Verifier;
        let key = VapidKey::generate();
        let jwt = key.sign_jwt("https://push.example.net", 1_000_000);
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "JWT는 3개 세그먼트");

        // 서명 검증(공개키로).
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        let sig_bytes = decode_b64_loose(parts[2]).unwrap();
        let signature = p256::ecdsa::Signature::from_slice(&sig_bytes).unwrap();
        let verifying = key.signing.verifying_key();
        assert!(
            verifying
                .verify(signing_input.as_bytes(), &signature)
                .is_ok(),
            "VAPID JWT 서명이 공개키로 검증되지 않음"
        );

        // 헤더/클레임 내용.
        let header = String::from_utf8(decode_b64_loose(parts[0]).unwrap()).unwrap();
        assert!(header.contains(r#""alg":"ES256""#), "{header}");
        let claims = String::from_utf8(decode_b64_loose(parts[1]).unwrap()).unwrap();
        assert!(
            claims.contains(r#""aud":"https://push.example.net""#),
            "{claims}"
        );
        assert!(claims.contains(r#""sub":"mailto:"#), "{claims}");
        // exp = now + 12h, 24h 상한 이내.
        assert!(
            claims.contains(&format!(r#""exp":{}"#, 1_000_000 + JWT_TTL_SECS)),
            "{claims}"
        );
    }

    #[test]
    fn vapid_authorization_헤더_형식() {
        let key = VapidKey::generate();
        let header = key.authorization_header("https://push.example.net", 42);
        assert!(header.starts_with("vapid t="), "{header}");
        assert!(header.contains(", k="), "{header}");
        assert!(header.contains(key.public_key_b64url()), "{header}");
    }

    #[test]
    fn vapid_키_keyring_라운드트립_같은_공개키() {
        let store = MemStore::default();
        let a = get_or_create_vapid_key(&store).unwrap();
        let b = get_or_create_vapid_key(&store).unwrap();
        assert_eq!(a.public_key_b64url(), b.public_key_b64url());
        assert_eq!(a.scalar_bytes(), b.scalar_bytes());
    }

    #[test]
    fn vapid_keyring_오류는_부재로_오판하지_않는다() {
        let err = get_or_create_vapid_key(&BrokenStore).unwrap_err();
        assert!(format!("{err:#}").contains("keyring") || format!("{err:#}").contains("VAPID"));
    }

    // ── endpoint origin 파싱 ─────────────────────────────────────────────────
    #[test]
    fn endpoint_origin_추출() {
        assert_eq!(
            endpoint_origin("https://fcm.googleapis.com/fcm/send/abc123"),
            Some("https://fcm.googleapis.com".to_owned())
        );
        assert_eq!(
            endpoint_origin("https://updates.push.services.mozilla.com/wpush/v2/xxx"),
            Some("https://updates.push.services.mozilla.com".to_owned())
        );
        assert_eq!(endpoint_origin("not-a-url"), None);
    }

    // ── 발송 스레드: 폴링 게이트/중복 억제/410 정리 ─────────────────────────

    /// 프로그래밍 가능한 가짜 트랜스포트 — endpoint별 응답을 지정하고 호출을 기록한다.
    #[derive(Default)]
    struct FakeTransport {
        /// endpoint → 반환할 상태코드(없으면 201).
        responses: StdMutex<HashMap<String, u16>>,
        calls: StdMutex<Vec<String>>,
    }
    impl FakeTransport {
        fn with(responses: HashMap<String, u16>) -> Self {
            Self {
                responses: StdMutex::new(responses),
                calls: StdMutex::new(Vec::new()),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }
    impl PushTransport for FakeTransport {
        fn post(
            &self,
            endpoint: &str,
            _t: u32,
            _u: &str,
            _a: &str,
            _b: &[u8],
        ) -> Result<u16, TransportError> {
            self.calls.lock().unwrap().push(endpoint.to_owned());
            Ok(*self.responses.lock().unwrap().get(endpoint).unwrap_or(&201))
        }
    }

    #[derive(Default)]
    struct MemStore(StdMutex<HashMap<String, String>>);
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
                .map(|v| SecretString::new(v.clone()))
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

    fn temp_db() -> PathBuf {
        std::env::temp_dir().join(format!(
            "web-push-test-{}.db",
            uuid::Uuid::new_v4().simple()
        ))
    }

    fn insert_pending(db_path: &std::path::Path, id: &str) {
        let db = storage::Db::open(db_path).unwrap();
        db.insert_pending_approval(id, "srv", "tool", "{}", None, 1)
            .unwrap();
    }

    fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        cond()
    }

    #[test]
    fn 구독0에서는_승인_폴링이_정지한다() {
        let db_path = temp_db();
        let _ = storage::Db::open(&db_path).unwrap(); // 파일 생성
        let mgr = PushManager::spawn_with_transport(
            db_path.clone(),
            VapidKey::generate(),
            Box::new(FakeTransport::default()),
        )
        .unwrap();
        // 구독 0 — 잠시 기다려도 폴링 0.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(mgr.handle().poll_count(), 0, "구독 0인데 폴링이 돌았다");
        mgr.stop_and_join();
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn 구독_등록하면_즉시_폴링하고_승인_1건을_발송한다() {
        let db_path = temp_db();
        insert_pending(&db_path, "appr-1");
        let transport = Arc::new(FakeTransport::default());
        let mgr = PushManager::spawn_with_transport(
            db_path.clone(),
            VapidKey::generate(),
            Box::new(SharedTransport(Arc::clone(&transport))),
        )
        .unwrap();
        let handle = mgr.handle();
        handle
            .add_subscription("https://push.example/one", RFC_UA_PUBLIC, RFC_AUTH)
            .unwrap();
        assert!(
            wait_until(|| transport.call_count() >= 1),
            "구독 등록 후 발송이 없었다"
        );
        assert!(handle.poll_count() >= 1, "등록 후 폴링이 안 돌았다");
        mgr.stop_and_join();
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn 같은_승인은_중복_발송하지_않는다() {
        let db_path = temp_db();
        insert_pending(&db_path, "appr-dup");
        let transport = Arc::new(FakeTransport::default());
        let mgr = PushManager::spawn_with_transport(
            db_path.clone(),
            VapidKey::generate(),
            Box::new(SharedTransport(Arc::clone(&transport))),
        )
        .unwrap();
        let handle = mgr.handle();
        handle
            .add_subscription("https://push.example/dup", RFC_UA_PUBLIC, RFC_AUTH)
            .unwrap();
        // 첫 발송 대기.
        assert!(wait_until(|| transport.call_count() >= 1));
        let after_first = transport.call_count();
        // 폴링이 몇 번 더 돌 시간을 줘도(같은 pending) 추가 발송이 없어야 한다.
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            transport.call_count(),
            after_first,
            "같은 승인이 중복 발송됐다"
        );
        mgr.stop_and_join();
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn gone_410_응답은_구독을_삭제한다() {
        let db_path = temp_db();
        insert_pending(&db_path, "appr-410");
        let mut responses = HashMap::new();
        responses.insert("https://push.example/dead".to_owned(), 410u16);
        let transport = Arc::new(FakeTransport::with(responses));
        let mgr = PushManager::spawn_with_transport(
            db_path.clone(),
            VapidKey::generate(),
            Box::new(SharedTransport(Arc::clone(&transport))),
        )
        .unwrap();
        let handle = mgr.handle();
        handle
            .add_subscription("https://push.example/dead", RFC_UA_PUBLIC, RFC_AUTH)
            .unwrap();
        // 410을 받으면 구독이 삭제돼 count가 0으로 돌아온다.
        assert!(
            wait_until(|| {
                let db = storage::Db::open(&db_path).unwrap();
                db.count_web_push_subscriptions().unwrap() == 0
            }),
            "410인데 구독이 삭제되지 않았다"
        );
        mgr.stop_and_join();
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn 세션_완료_알림은_중복_억제된다() {
        let db_path = temp_db();
        let transport = Arc::new(FakeTransport::default());
        let mgr = PushManager::spawn_with_transport(
            db_path.clone(),
            VapidKey::generate(),
            Box::new(SharedTransport(Arc::clone(&transport))),
        )
        .unwrap();
        let handle = mgr.handle();
        // 구독이 있어야 세션 알림을 실제로 보낸다.
        handle
            .add_subscription("https://push.example/s", RFC_UA_PUBLIC, RFC_AUTH)
            .unwrap();
        wait_until(|| handle.poll_count() >= 1);
        let base = transport.call_count();
        handle.notify_session(7, runtime::SessionStatus::Done);
        assert!(
            wait_until(|| transport.call_count() > base),
            "세션 완료 발송 없음"
        );
        let after = transport.call_count();
        // 같은 세션·같은 상태 재통지는 억제.
        handle.notify_session(7, runtime::SessionStatus::Done);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(transport.call_count(), after, "세션 완료가 중복 발송됨");
        // 상태 외 값(Running)은 무시.
        handle.notify_session(7, runtime::SessionStatus::Running);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(transport.call_count(), after, "무관 상태가 발송됨");
        mgr.stop_and_join();
        let _ = std::fs::remove_file(&db_path);
    }

    /// Arc<FakeTransport>를 Box<dyn PushTransport>로 넘기기 위한 래퍼(테스트가 관찰을 공유).
    struct SharedTransport(Arc<FakeTransport>);
    impl PushTransport for SharedTransport {
        fn post(&self, e: &str, t: u32, u: &str, a: &str, b: &[u8]) -> Result<u16, TransportError> {
            self.0.post(e, t, u, a, b)
        }
    }

    // ── HTTP 라우팅 (route) ──────────────────────────────────────────────────
    const TEST_TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn parse_head(raw: &str) -> crate::http::RequestHead {
        crate::http::read_request_head(&mut std::io::BufReader::new(raw.as_bytes())).unwrap()
    }

    #[test]
    fn route_vapid_공개키는_토큰_게이트_통과시_200() {
        let db_path = temp_db();
        let mgr = PushManager::spawn_with_transport(
            db_path.clone(),
            VapidKey::generate(),
            Box::new(FakeTransport::default()),
        )
        .unwrap();
        let handle = mgr.handle();
        let expected_key = handle.vapid_public_key().to_owned();

        // 올바른 토큰 → 200 + 공개키.
        let head = parse_head(&format!(
            "GET /push/vapid?token={TEST_TOKEN} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
        ));
        let resp = route(&head, &[], TEST_TOKEN, Some(&handle)).expect("push 경로여야 함");
        assert_eq!(resp.status, 200);
        let body = String::from_utf8(resp.body.to_vec()).unwrap();
        assert!(body.contains(&expected_key), "{body}");

        // 잘못된 토큰 → 401.
        let bad = parse_head("GET /push/vapid?token=wrong HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert_eq!(
            route(&bad, &[], TEST_TOKEN, Some(&handle)).unwrap().status,
            401
        );

        mgr.stop_and_join();
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn route_push_비활성이면_404() {
        let head = parse_head(&format!(
            "GET /push/vapid?token={TEST_TOKEN} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
        ));
        // push=None → 404(비활성). 비-push 경로는 None(정적 라우팅으로).
        assert_eq!(route(&head, &[], TEST_TOKEN, None).unwrap().status, 404);
        let other = parse_head("GET /app.js HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert!(route(&other, &[], TEST_TOKEN, None).is_none());
    }

    #[test]
    fn route_subscribe_등록은_201_그리고_db에_저장된다() {
        let db_path = temp_db();
        let mgr = PushManager::spawn_with_transport(
            db_path.clone(),
            VapidKey::generate(),
            Box::new(FakeTransport::default()),
        )
        .unwrap();
        let handle = mgr.handle();

        let head = parse_head(&format!(
            "POST /push/subscribe?token={TEST_TOKEN} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
        ));
        let body = br#"{"endpoint":"https://push.example/z","keys":{"p256dh":"k","auth":"a"}}"#;
        let resp = route(&head, body, TEST_TOKEN, Some(&handle)).unwrap();
        assert_eq!(resp.status, 201, "{}", String::from_utf8_lossy(&resp.body));
        // DB에 실제로 저장됐다.
        let db = storage::Db::open(&db_path).unwrap();
        let subs = db.list_web_push_subscriptions().unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].endpoint, "https://push.example/z");

        // 잘못된 토큰 → 401(등록 안 됨), 기형 JSON → 400.
        let bad_token =
            parse_head("POST /push/subscribe?token=x HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert_eq!(
            route(&bad_token, body, TEST_TOKEN, Some(&handle))
                .unwrap()
                .status,
            401
        );
        assert_eq!(
            route(&head, b"not json", TEST_TOKEN, Some(&handle))
                .unwrap()
                .status,
            400
        );

        mgr.stop_and_join();
        let _ = std::fs::remove_file(&db_path);
    }
}
