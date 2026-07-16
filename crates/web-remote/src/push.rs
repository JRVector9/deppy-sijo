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
use std::time::Duration;

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use deppy_core::time::unix_secs;
use hkdf::Hkdf;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use secret::hex::{from_hex, to_hex};
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
/// 세션 상태 알림의 총 발송 시도 횟수(첫 시도 + 재시도). 승인은 DB 폴링이 스스로 재수렴하지만
/// 세션 전이는 (session,kind)가 생애 1회라 재시도 큐가 없으면 일시 장애 = 영구 유실이다.
const SESSION_SEND_ATTEMPTS: u32 = 3;
/// `notified_status` 상한 — 초과 시 오래된(사전순 낮은) 키부터 버린다. UUID라 단조성은
/// 없지만 상한 유지가 목적이다. 원문:
/// 세션 id는 런타임에서 단조 증가(next_id)하므로 초과 시 가장 낮은
/// (=가장 오래된) id부터 버린다. 승인 기록은 pending 목록으로 자기정리되지만 세션 상태는
/// "사라짐" 신호가 없어 상한으로 유계화한다.
const MAX_NOTIFIED_SESSIONS: usize = 256;
/// 등록 가능한 구독 수 상한(계정 전체). 개인용 1~2기기 가정 + 여유. 죽은 endpoint를 다수
/// 등록해 발송 스레드를 HTTP_TIMEOUT×재시도×구독수만큼 붙잡는 지연을 막는다.
const MAX_SUBSCRIPTIONS: usize = 8;

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
        // 클레임(aud/exp/sub)은 serde_json으로 조립한다 — 수제 문자열 조립은 aud에 따옴표·제어
        // 문자가 섞이면 JSON을 깨뜨린다(등록 검증이 있어도 이스케이프는 직렬화기에 맡긴다).
        // Notification::payload와 같은 방식.
        let claims = serde_json::json!({
            "aud": aud,
            "exp": now_secs + JWT_TTL_SECS,
            "sub": VAPID_SUB,
        })
        .to_string();
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
    /// 재시도 1회 후에도 실패(5xx/429/타임아웃/DNS) — **일시 장애**로 본다. 구독은 유지하고
    /// 호출자가 중복 억제 마킹을 보류해 다음 주기에 재시도한다.
    Failed,
    /// 구독 데이터가 깨져 발송 자체가 불가(endpoint origin 파싱·본문 암호화 실패). 재시도해도
    /// 결과가 같으므로 **재시도 대상이 아니다** — 5초마다 무의미한 재발송이 도는 것을 막는다.
    Broken,
}

/// 한 번의 브로드캐스트 결과 — 호출자가 "중복 억제 마킹을 커밋할지" 판단하는 근거.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct BroadcastOutcome {
    /// 2xx로 실제 전달된 구독 수.
    delivered: usize,
    /// 일시 장애로 실패한 구독 수(재시도 대상).
    failed: usize,
}

impl BroadcastOutcome {
    /// 중복 억제 마킹("이미 알렸다")을 커밋해도 되는가.
    ///
    /// 설계 판단 — 발송 실패가 마킹을 롤백하지 않아 생기던 **무음 유실**을 막되, 재시도가
    /// 중복 알림·폭주로 번지지 않게 한다:
    ///   - **하나라도 전달**(delivered>0)되면 커밋한다. 실패한 기기 하나 때문에 마킹을 미루면
    ///     이미 받은 기기에 같은 알림이 다시 간다. 페이로드는 endpoint별 상태가 아니라
    ///     "종류/개수" 요약이라, 실패한 기기는 다음 새 승인·상태 전이에서 자연히 재수렴한다.
    ///   - **전량 실패**(delivered==0 && failed>0)면 커밋하지 않는다 → 다음 폴링 주기(승인)나
    ///     재시도 큐(세션)가 같은 알림을 다시 보낸다. 폭주는 "시도당 재시도 1회" 상한과
    ///     폴링 주기가 막는다.
    ///   - **재시도 대상이 없음**(delivered==0 && failed==0: 구독 0건 / 전부 410으로 삭제 /
    ///     키 손상)이면 커밋한다. 다시 보내도 결과가 같으므로 재시도해봐야 무한 반복뿐이다.
    ///     410으로 구독이 사라진 경우는 그 구독이 목록에서 빠지므로 어차피 재시도 대상이 아니다.
    fn should_commit(self) -> bool {
        self.delivered > 0 || self.failed == 0
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 알림 페이로드 (종류/제목/개수만)
// ─────────────────────────────────────────────────────────────────────────────

/// 발송할 알림 한 건. 페이로드에는 종류/제목/개수만 담는다(도구 인자·로그 금지).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Notification {
    /// pending 승인 — 개수 포함.
    Approval { count: usize },
    /// 세션 완료 — 딥링크용 **영속 UUID** 포함 (P6c + I1).
    SessionDone { session: String },
    /// 세션 입력 대기 — 딥링크용 **영속 UUID** 포함 (P6c + I1).
    SessionWaiting { session: String },
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
            // session id는 딥링크(알림 탭 → 그 세션 화면)용 — 민감정보 아님(worker-로컬
            // 순번). 클라이언트는 대시보드에 실재하는 id일 때만 자동 시청한다 (P6c).
            Notification::SessionDone { session } => serde_json::json!({
                "kind": "done",
                "title": "세션 완료",
                "tag": "deppy-session",
                "session": session,
            }),
            Notification::SessionWaiting { session } => serde_json::json!({
                "kind": "waiting",
                "title": "입력 대기",
                "tag": "deppy-session",
                "session": session,
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

/// 발송 대기 중인 세션 상태 알림 한 건.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionJob {
    /// 영속 세션 UUID (I1) — 딥링크가 재시작 후에도 같은 세션을 가리킨다.
    session: String,
    kind: SessionKind,
    /// 지금까지 시도한 횟수(0 = 아직 미시도). SESSION_SEND_ATTEMPTS에서 포기한다.
    attempts: u32,
}

struct PushInner {
    /// 즉시 1회 재평가 강제(구독 등록 직후).
    force: bool,
    /// 대시보드 브리지가 넣은 세션 상태 알림 큐.
    jobs: VecDeque<SessionJob>,
    /// 전량 실패해 **다음 주기에** 재시도할 세션 알림. 스레드 깨움 조건(ready/wait)에 넣지
    /// 않는 것이 핵심 — 넣으면 실패 즉시 같은 발송이 반복돼 폭주한다. 구독>0이면 어차피 폴링
    /// 주기마다 깨므로 그때 jobs와 함께 처리된다(구독 0이면 보낼 곳이 없어 처리하지 않는다).
    retry_jobs: VecDeque<SessionJob>,
    /// 이미 푸시한 pending 승인 id — 중복 발송 방지. pending에서 사라지면 정리(유계).
    notified_approvals: HashSet<String>,
    /// 세션별 마지막으로 알린 상태 — 같은 전이 반복 발송 방지. MAX_NOTIFIED_SESSIONS로 유계.
    notified_status: HashMap<String, SessionKind>,
}

struct PushShared {
    inner: Mutex<PushInner>,
    cvar: Condvar,
    /// 발송 스레드 전용 DB 연결(대시보드와 별도 — SQLite 다중 연결 관례). 구독 CRUD·승인 폴링.
    db: Mutex<storage::Db>,
    vapid: VapidKey,
    transport: Box<dyn PushTransport>,
    /// 승인 폴링 주기 — 운영은 POLL_INTERVAL. 테스트만 짧게 주입해 "다음 주기 재시도"를 빠르게
    /// 관찰한다.
    poll_interval: Duration,
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
        Self::spawn_with_interval(db_path, vapid, transport, POLL_INTERVAL)
    }

    /// 트랜스포트 + 폴링 주기를 주입해 띄운다(테스트: 재시도를 5초 기다리지 않게).
    fn spawn_with_interval(
        db_path: PathBuf,
        vapid: VapidKey,
        transport: Box<dyn PushTransport>,
        poll_interval: Duration,
    ) -> anyhow::Result<Self> {
        let db = storage::Db::open(&db_path).context("web-remote 웹푸시 DB 열기 실패")?;
        let initial = db.count_web_push_subscriptions().unwrap_or(0).max(0) as usize;
        let shared = Arc::new(PushShared {
            inner: Mutex::new(PushInner {
                force: false,
                jobs: VecDeque::new(),
                retry_jobs: VecDeque::new(),
                notified_approvals: HashSet::new(),
                notified_status: HashMap::new(),
            }),
            cvar: Condvar::new(),
            db: Mutex::new(db),
            vapid,
            transport,
            poll_interval,
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

    /// 구독을 등록/갱신한다(POST /push/subscribe). endpoint 정책(https·공인 주소)과 구독 수
    /// 상한을 통과해야 DB에 쓰고, 스레드를 깨워 즉시 재평가시킨다.
    pub fn add_subscription(
        &self,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
    ) -> Result<(), SubscribeError> {
        if !endpoint_allowed(endpoint) {
            return Err(SubscribeError::BadEndpoint);
        }
        {
            let db = self.shared.db.lock().expect("push db lock");
            // 상한 검사는 DB 락 안에서 — 동시 등록이 상한을 넘겨 삽입하는 경쟁을 막는다.
            // 이미 등록된 endpoint의 재등록(브라우저 키 회전)은 새 구독이 아니라 상한과 무관하다.
            let existing = db
                .list_web_push_subscriptions()
                .map_err(SubscribeError::Storage)?;
            if existing.len() >= MAX_SUBSCRIPTIONS
                && !existing.iter().any(|row| row.endpoint == endpoint)
            {
                return Err(SubscribeError::TooManySubscriptions);
            }
            db.upsert_web_push_subscription(endpoint, p256dh, auth, unix_secs() as i64)
                .map_err(SubscribeError::Storage)?;
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
    pub fn notify_session(&self, session: String, status: runtime::SessionStatus) {
        let kind = match status {
            runtime::SessionStatus::Done => SessionKind::Done,
            runtime::SessionStatus::Waiting => SessionKind::Waiting,
            _ => return,
        };
        {
            let mut inner = self.shared.inner.lock().expect("push inner lock");
            inner.jobs.push_back(SessionJob {
                session,
                kind,
                attempts: 0,
            });
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
            // retry_jobs는 **깨움 조건이 아니다** — 넣으면 전량 실패가 즉시 재시도로 이어져
            // 폭주한다. 구독>0이면 폴링 주기마다 깨므로 그때 함께 처리된다.
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
            // 이번 주기 작업 = 지난 주기에 전량 실패한 재시도분 + 브리지가 새로 넣은 알림.
            // 락 밖에서 처리한다(구독 0이면 아래에서 skip).
            let mut batch = std::mem::take(&mut inner.retry_jobs);
            batch.extend(inner.jobs.drain(..));
            jobs = batch;
            do_poll = subs > 0;
            inner.force = false;
        }

        // 세션 상태 알림 — 중복 억제(같은 세션·같은 상태는 1회). 마킹은 **발송 결과를 보고**
        // 커밋한다(BroadcastOutcome::should_commit) — 전량 실패면 마킹하지 않고 재시도 큐에
        // 넣는다. 세션 전이는 생애 1회라 여기서 버리면 영구 유실이다.
        for job in jobs {
            if shared.sub_count.load(Ordering::SeqCst) == 0 {
                continue;
            }
            let already = {
                let inner = shared.inner.lock().expect("push inner lock");
                inner.notified_status.get(&job.session) == Some(&job.kind)
            };
            if already {
                continue;
            }
            let note = match job.kind {
                SessionKind::Done => Notification::SessionDone {
                    session: job.session.clone(),
                },
                SessionKind::Waiting => Notification::SessionWaiting {
                    session: job.session.clone(),
                },
            };
            let outcome = broadcast(shared, &note);
            let attempts = job.attempts + 1;
            if outcome.should_commit() {
                let mut inner = shared.inner.lock().expect("push inner lock");
                remember_status(&mut inner.notified_status, &job.session, job.kind);
            } else if attempts < SESSION_SEND_ATTEMPTS {
                let mut inner = shared.inner.lock().expect("push inner lock");
                inner.retry_jobs.push_back(SessionJob {
                    attempts,
                    ..job.clone()
                });
            } else {
                tracing::warn!(
                    attempts,
                    "웹푸시 세션 알림 재시도 상한 초과 — 포기(구독 측 지속 장애)"
                );
            }
        }

        // 승인 폴링 — 구독>0에서만. 새 pending id가 있을 때만 1건 발송(개수 포함).
        if do_poll {
            shared.poll_count.fetch_add(1, Ordering::SeqCst);
            poll_and_notify_approvals(shared);
            // 다음 폴링 주기까지 대기(구독>0). 그 사이 force/세션작업/stop이 깨운다.
            // retry_jobs는 조건에서 제외 — 재시도는 다음 주기에 한 번만(폭주 방지).
            let inner = shared.inner.lock().expect("push inner lock");
            if !inner.force && inner.jobs.is_empty() && !shared.stop.load(Ordering::SeqCst) {
                let _ = shared.cvar.wait_timeout(inner, shared.poll_interval);
            }
        }
    }
}

/// 세션 상태 마킹을 기록하고 상한을 지킨다. 세션 id는 런타임에서 단조 증가하므로 상한 초과 시
/// 가장 낮은 id(=가장 오래된 세션)부터 버린다 — 그 세션은 이미 끝나 같은 전이가 다시 오지 않는다.
/// (승인 기록은 pending 목록으로 retain 정리되지만, 세션은 "사라짐" 신호가 없어 상한을 쓴다.)
fn remember_status(notified: &mut HashMap<String, SessionKind>, session: &str, kind: SessionKind) {
    notified.insert(session.to_owned(), kind);
    while notified.len() > MAX_NOTIFIED_SESSIONS {
        // UUID라 단조성은 없다 — 사전순 최소 키를 버려 상한만 유지한다.
        let Some(oldest) = notified.keys().min().cloned() else {
            break;
        };
        notified.remove(&oldest);
    }
}

/// pending 승인을 폴링해 새 id가 있으면 개수 알림을 1건 보낸다. 이미 알린 id는 건너뛰고,
/// pending에서 사라진 id는 기억에서 지운다(유계).
///
/// 마킹("이미 알렸다")은 **발송이 성공한 뒤에만** 커밋한다 — 전량 실패인데 마킹부터 하면 다음
/// 폴링에서 같은 id가 이미 알린 것으로 보여 그 승인은 영구히 알림 없이 묻힌다.
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
        pending
            .iter()
            .any(|row| !inner.notified_approvals.contains(&row.id))
    };
    if !has_new {
        return;
    }
    let outcome = broadcast(
        shared,
        &Notification::Approval {
            count: pending.len(),
        },
    );
    if !outcome.should_commit() {
        // 전량 실패 — 마킹하지 않는다. 다음 폴링 주기가 같은 pending을 다시 알린다.
        return;
    }
    let mut inner = shared.inner.lock().expect("push inner lock");
    for id in current_ids {
        inner.notified_approvals.insert(id);
    }
}

/// 모든 구독에 알림을 보낸다. 성공은 last_ok_at 갱신, 410/404는 구독 삭제, 그 외 실패는 이번
/// 시도 포기. 결과([`BroadcastOutcome`])로 호출자가 중복 억제 마킹 커밋 여부를 정한다.
fn broadcast(shared: &Arc<PushShared>, note: &Notification) -> BroadcastOutcome {
    let subs = {
        let db = shared.db.lock().expect("push db lock");
        db.list_web_push_subscriptions().unwrap_or_default()
    };
    let mut outcome = BroadcastOutcome::default();
    if subs.is_empty() {
        return outcome;
    }
    let payload = note.payload();
    let mut changed = false;
    for sub in subs {
        // shutdown 중이면 잔여 구독 발송을 중단한다 — stop_and_join(앱 종료, UI 스레드)이
        // 구독 수 × HTTP 타임아웃만큼 기다리지 않게 한다. 남는 대기는 진행 중이던
        // POST 1건의 타임아웃뿐이다. 미발송분은 in-memory 마킹과 함께 사라지므로
        // 재시작 후 같은 pending이 다시 알림된다.
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        match deliver(shared, &sub, payload.as_bytes()) {
            Delivery::Ok => {
                outcome.delivered += 1;
                let db = shared.db.lock().expect("push db lock");
                let _ = db.touch_web_push_subscription(&sub.endpoint, unix_secs() as i64);
            }
            Delivery::Gone => {
                let db = shared.db.lock().expect("push db lock");
                let _ = db.delete_web_push_subscription(&sub.endpoint);
                changed = true;
                tracing::info!("웹푸시 구독 만료(410/404) — 삭제");
            }
            Delivery::Failed => {
                outcome.failed += 1;
                tracing::warn!("웹푸시 발송 실패(재시도 후 포기) — 다음 주기 재시도 대상");
            }
            Delivery::Broken => {
                tracing::warn!("웹푸시 구독 데이터 손상 — 발송 불가(재시도 무의미)");
            }
        }
    }
    if outcome.delivered == 0 && outcome.failed > 0 {
        tracing::warn!(
            failed = outcome.failed,
            "웹푸시 전량 실패 — 중복 억제 마킹 보류(다음 주기 재시도)"
        );
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
    outcome
}

/// 한 구독으로 발송한다: VAPID JWT + RFC 8291 암호화 + POST(재시도 1회). 암호화 실패는
/// 즉시 포기(구독 데이터가 깨진 경우 — 재시도 무의미 → Broken).
fn deliver(
    shared: &Arc<PushShared>,
    sub: &storage::WebPushSubscriptionRow,
    payload: &[u8],
) -> Delivery {
    let Some(aud) = endpoint_origin(&sub.endpoint) else {
        tracing::warn!("웹푸시 endpoint origin 파싱 실패 — 건너뜀");
        return Delivery::Broken;
    };
    let body = match encrypt_payload(&sub.p256dh, &sub.auth, payload) {
        Ok(body) => body,
        Err(e) => {
            tracing::warn!("웹푸시 본문 암호화 실패: {e:#}");
            return Delivery::Broken;
        }
    };
    let authorization = shared.vapid.authorization_header(&aud, unix_secs());

    // 재시도 1회 — 폭주 방지. 410/404는 재시도 없이 즉시 Gone.
    for attempt in 0..2 {
        // shutdown 중엔 재시도를 생략한다 (stop_and_join 대기 단축).
        if attempt > 0 && shared.stop.load(Ordering::SeqCst) {
            return Delivery::Failed;
        }
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
    if !crate::static_srv::token_param_matches(query, token) {
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

/// 구독 등록 — 토큰 게이트 + JSON 본문 {endpoint, keys:{p256dh, auth}}. endpoint는 정책 검증을
/// 통과해야 한다(https 공인 주소 + 구독 수 상한).
fn subscribe_response(
    query: &str,
    body: &[u8],
    token: &str,
    push: Option<&PushHandle>,
) -> Response {
    if !crate::static_srv::token_param_matches(query, token) {
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
        Err(SubscribeError::BadEndpoint) => {
            // endpoint는 로그하지 않는다(토큰 보유자가 넣은 임의 문자열).
            tracing::warn!("웹푸시 구독 endpoint 거부 — https 공인 주소만 허용");
            Response::plain(400, "bad endpoint")
        }
        Err(SubscribeError::TooManySubscriptions) => {
            tracing::warn!(limit = MAX_SUBSCRIPTIONS, "웹푸시 구독 수 상한 초과 — 거부");
            Response::plain(403, "subscription limit reached")
        }
        Err(SubscribeError::Storage(e)) => {
            tracing::warn!("웹푸시 구독 등록 실패: {e:#}");
            Response::plain(500, "subscribe failed")
        }
    }
}

/// 구독 등록 거부 사유 — HTTP 상태 매핑용.
#[derive(Debug)]
pub enum SubscribeError {
    /// endpoint 정책 위반(https 아님 / 내부·사설 주소) → 400.
    BadEndpoint,
    /// 계정 전체 구독 수 상한 초과 → 403.
    TooManySubscriptions,
    /// DB 오류 → 500.
    Storage(anyhow::Error),
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

// ─────────────────────────────────────────────────────────────────────────────
// 소도구
// ─────────────────────────────────────────────────────────────────────────────

/// endpoint URL에서 origin(scheme://authority)을 뽑는다 — VAPID `aud`. url 크레이트 없이 파싱.
/// authority는 path/query/fragment 앞까지다(`?`/`#`가 aud에 새지 않게).
fn endpoint_origin(endpoint: &str) -> Option<String> {
    let (scheme, rest) = endpoint.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    let authority = endpoint_authority(rest);
    if authority.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{authority}"))
}

/// `://` 뒤에서 authority(host[:port])만 잘라낸다 — path/query/fragment 제거.
fn endpoint_authority(rest: &str) -> &str {
    rest.split(['/', '?', '#']).next().unwrap_or("")
}

/// 구독 endpoint가 등록 가능한 주소인지 검사한다(SSRF 방어).
///
/// 토큰 보유자라도 앱이 내부망으로 POST하게 만들 수 없어야 한다(대시보드에 흔적이 남지 않는
/// 발송 채널). 규칙:
///   (a) scheme은 **https만** — 평문 http나 다른 scheme은 거부.
///   (b) userinfo(`user@host`)는 거부 — 정상 push 서비스에 없고 host 오인의 원인.
///   (c) host가 loopback/사설/링크로컬/CGNAT/유니크로컬 **IP 리터럴**이거나 localhost 계열
///       이름이면 거부.
///
/// 한계: DNS 이름이 사설 IP로 해석되는 경우(DNS rebinding)는 여기서 막지 못한다 — 해석은 발송
/// 시점 트랜스포트가 하므로 완전 차단하려면 커넥터 수준 제어가 필요하다. 잔여 리스크로 남긴다
/// (공격자는 이미 페어링 토큰을 가진 상태이고, 발송 본문은 종류/개수 요약뿐이다).
fn endpoint_allowed(endpoint: &str) -> bool {
    let Some((scheme, rest)) = endpoint.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    let authority = endpoint_authority(rest);
    if authority.is_empty() || authority.contains('@') {
        return false;
    }
    // host 추출 — IPv6 리터럴은 `[::1]:443` 형태.
    let host = if let Some(after) = authority.strip_prefix('[') {
        match after.split_once(']') {
            Some((host, _port)) => host,
            None => return false,
        }
    } else {
        authority.split(':').next().unwrap_or("")
    };
    if host.is_empty() {
        return false;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return !is_internal_ip(&ip);
    }
    // DNS 이름 — loopback 별칭과 mDNS(.local)만 거부한다.
    let lower = host.to_ascii_lowercase();
    !(lower == "localhost" || lower.ends_with(".localhost") || lower.ends_with(".local"))
}

/// 내부망 IP인가 — loopback/사설/링크로컬/CGNAT(tailscale 대역)/유니크로컬/멀티캐스트 등.
fn is_internal_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()          // 127.0.0.0/8
                || v4.is_private()    // 10/8, 172.16/12, 192.168/16
                || v4.is_link_local() // 169.254/16
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || a == 0                            // 0.0.0.0/8 ("this network" — 0.0.0.0=localhost 우회)
                || (a == 100 && (64..128).contains(&b)) // 100.64/10 CGNAT(tailscale 대역)
        }
        std::net::IpAddr::V6(v6) => {
            let head = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (head & 0xfe00) == 0xfc00 // fc00::/7 unique local
                || (head & 0xffc0) == 0xfe80 // fe80::/10 link local
                // ::ffff:a.b.c.d — IPv4-mapped로 내부 IPv4를 우회 등록하는 경로 차단.
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_internal_ip(&std::net::IpAddr::V4(v4)))
        }
    }
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

    /// P6c: 세션 알림 페이로드에 딥링크용 session id가 실린다(승인 알림에는 없다).
    #[test]
    fn 세션_알림_페이로드는_딥링크용_세션_id를_싣는다() {
        let done = Notification::SessionDone {
            session: "u7".into(),
        }
        .payload();
        assert!(done.contains(r#""session":"u7""#), "{done}");
        assert!(done.contains(r#""kind":"done""#), "{done}");
        let waiting = Notification::SessionWaiting {
            session: "u42".into(),
        }
        .payload();
        assert!(waiting.contains(r#""session":"u42""#), "{waiting}");
        // 승인 알림은 세션 개념이 없다 — 개수만
        let approval = Notification::Approval { count: 3 }.payload();
        assert!(!approval.contains("session"), "{approval}");
        assert!(approval.contains(r#""count":3"#), "{approval}");
    }

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
        // 클레임은 serde_json으로 조립한다 — 유효한 JSON이어야 한다(수제 조립 회귀 방지).
        let parsed: serde_json::Value =
            serde_json::from_str(&claims).expect("클레임이 유효한 JSON이 아님");
        assert_eq!(parsed["aud"], "https://push.example.net");
        assert_eq!(parsed["sub"], VAPID_SUB);
        // exp = now + 12h(JWT_TTL_SECS) — RFC 8292의 24h 상한 이내.
        assert_eq!(parsed["exp"], 1_000_000 + JWT_TTL_SECS);
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
        // query/fragment는 authority(=aud)에 섞이지 않는다.
        assert_eq!(
            endpoint_origin("https://push.example?x=1#f"),
            Some("https://push.example".to_owned())
        );
        assert_eq!(endpoint_origin("not-a-url"), None);
    }

    // ── P2: endpoint SSRF 방어 + 구독 수 상한 ───────────────────────────────

    #[test]
    fn endpoint_검증은_https_공인주소만_허용한다() {
        // 정상 push 서비스.
        assert!(endpoint_allowed(
            "https://fcm.googleapis.com/fcm/send/abc123"
        ));
        assert!(endpoint_allowed(
            "https://updates.push.services.mozilla.com/wpush/v2/xxx"
        ));
        assert!(endpoint_allowed("https://web.push.apple.com/QA/x?y=1"));
        assert!(
            endpoint_allowed("https://8.8.8.8/x"),
            "공인 IP 리터럴은 허용"
        );
        assert!(
            endpoint_allowed("https://172.32.0.1/x"),
            "172.32는 사설 아님"
        );

        // (a) scheme — https만.
        assert!(!endpoint_allowed("http://push.example/x"));
        assert!(!endpoint_allowed("file:///etc/passwd"));
        assert!(!endpoint_allowed("push.example/x"));
        assert!(!endpoint_allowed(""));

        // (b) userinfo로 host를 감추는 시도.
        assert!(!endpoint_allowed("https://fcm.googleapis.com@127.0.0.1/x"));

        // (c) loopback / 사설 / 링크로컬 / CGNAT / 0.0.0.0/8.
        assert!(!endpoint_allowed("https://127.0.0.1:8080/x"));
        assert!(!endpoint_allowed("https://localhost/x"));
        assert!(!endpoint_allowed("https://app.localhost/x"));
        assert!(!endpoint_allowed("https://nas.local/x"));
        assert!(!endpoint_allowed("https://10.0.0.5/x"));
        assert!(!endpoint_allowed("https://172.16.3.9/x"));
        assert!(!endpoint_allowed("https://172.31.255.254/x"));
        assert!(!endpoint_allowed("https://192.168.0.10/x"));
        assert!(!endpoint_allowed(
            "https://169.254.169.254/latest/meta-data"
        ));
        assert!(!endpoint_allowed("https://100.101.102.103/x"), "CGNAT");
        assert!(!endpoint_allowed("https://0.0.0.0/x"));
        // IPv6 loopback / ULA / 링크로컬 / IPv4-mapped 우회.
        assert!(!endpoint_allowed("https://[::1]:8443/x"));
        assert!(!endpoint_allowed("https://[fc00::1]/x"));
        assert!(!endpoint_allowed("https://[fe80::1]/x"));
        assert!(!endpoint_allowed("https://[::ffff:127.0.0.1]/x"));
    }

    #[test]
    fn route_subscribe_내부주소_endpoint는_400이고_저장되지_않는다() {
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
        for endpoint in [
            "http://push.example/x",
            "https://127.0.0.1:9000/x",
            "https://192.168.1.7/x",
            "https://[::1]/x",
            "https://localhost/x",
            "https://169.254.169.254/x",
        ] {
            let body = format!(r#"{{"endpoint":"{endpoint}","keys":{{"p256dh":"k","auth":"a"}}}}"#);
            let resp = route(&head, body.as_bytes(), TEST_TOKEN, Some(&handle)).unwrap();
            assert_eq!(resp.status, 400, "{endpoint} 가 거부되지 않았다");
        }
        let db = storage::Db::open(&db_path).unwrap();
        assert_eq!(
            db.count_web_push_subscriptions().unwrap(),
            0,
            "거부된 endpoint가 DB에 저장됐다"
        );
        // 정상 push 서비스 endpoint는 통과한다.
        let body =
            br#"{"endpoint":"https://fcm.googleapis.com/fcm/send/abc","keys":{"p256dh":"k","auth":"a"}}"#;
        assert_eq!(
            route(&head, body, TEST_TOKEN, Some(&handle))
                .unwrap()
                .status,
            201
        );

        mgr.stop_and_join();
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn 구독_수_상한을_넘으면_등록을_거부한다() {
        let db_path = temp_db();
        let mgr = PushManager::spawn_with_transport(
            db_path.clone(),
            VapidKey::generate(),
            Box::new(FakeTransport::default()),
        )
        .unwrap();
        let handle = mgr.handle();
        for i in 0..MAX_SUBSCRIPTIONS {
            handle
                .add_subscription(
                    &format!("https://push.example/dev{i}"),
                    RFC_UA_PUBLIC,
                    RFC_AUTH,
                )
                .unwrap();
        }
        // 상한 초과 — 새 endpoint는 거부.
        let err = handle
            .add_subscription("https://push.example/extra", RFC_UA_PUBLIC, RFC_AUTH)
            .unwrap_err();
        assert!(
            matches!(err, SubscribeError::TooManySubscriptions),
            "{err:?}"
        );
        // 이미 등록된 endpoint의 재등록(브라우저 키 회전)은 상한과 무관하게 허용.
        handle
            .add_subscription("https://push.example/dev0", RFC_UA_PUBLIC, RFC_AUTH)
            .unwrap();
        let db = storage::Db::open(&db_path).unwrap();
        assert_eq!(
            db.count_web_push_subscriptions().unwrap() as usize,
            MAX_SUBSCRIPTIONS,
            "상한을 넘겨 저장됐다"
        );
        // HTTP 계층은 403으로 응답한다.
        let head = parse_head(&format!(
            "POST /push/subscribe?token={TEST_TOKEN} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
        ));
        let body =
            br#"{"endpoint":"https://push.example/extra2","keys":{"p256dh":"k","auth":"a"}}"#;
        assert_eq!(
            route(&head, body, TEST_TOKEN, Some(&handle))
                .unwrap()
                .status,
            403
        );

        mgr.stop_and_join();
        let _ = std::fs::remove_file(&db_path);
    }

    // ── 발송 스레드: 폴링 게이트/중복 억제/410 정리 ─────────────────────────

    /// 테스트용 짧은 폴링 주기 — "다음 주기 재시도"를 5초 기다리지 않는다.
    const FAST_POLL: Duration = Duration::from_millis(40);

    /// 가짜 트랜스포트의 응답 — 상태코드 또는 네트워크 실패(타임아웃/DNS).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Reply {
        Status(u16),
        /// 상태코드조차 받지 못한 경우(ureq Transport 오류에 해당).
        Network,
    }

    /// 프로그래밍 가능한 가짜 트랜스포트 — endpoint별 응답을 지정하고(실행 중 변경 가능:
    /// 장애→회복 시나리오) 호출을 기록한다. 지정이 없으면 201.
    #[derive(Default)]
    struct FakeTransport {
        responses: StdMutex<HashMap<String, Reply>>,
        calls: StdMutex<Vec<String>>,
    }
    impl FakeTransport {
        fn with(responses: HashMap<String, Reply>) -> Self {
            Self {
                responses: StdMutex::new(responses),
                calls: StdMutex::new(Vec::new()),
            }
        }
        /// 발송 중 응답을 바꾼다(예: 503 → 201 회복).
        fn set(&self, endpoint: &str, reply: Reply) {
            self.responses
                .lock()
                .unwrap()
                .insert(endpoint.to_owned(), reply);
        }
        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
        fn calls_to(&self, endpoint: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|e| e.as_str() == endpoint)
                .count()
        }
        /// 호출 수가 잠시(폴링 주기 몇 번) 변하지 않을 때까지 기다린다 — 재발송이 멎었는지 판정.
        fn settle(&self) -> usize {
            wait_until(|| {
                let before = self.call_count();
                std::thread::sleep(FAST_POLL * 3);
                before == self.call_count()
            });
            self.call_count()
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
            let reply = self
                .responses
                .lock()
                .unwrap()
                .get(endpoint)
                .copied()
                .unwrap_or(Reply::Status(201));
            match reply {
                Reply::Status(code) => Ok(code),
                Reply::Network => Err(TransportError),
            }
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
        db.insert_pending_approval(id, "srv", "tool", "{}", None, 1, None)
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
        responses.insert("https://push.example/dead".to_owned(), Reply::Status(410));
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
        handle.notify_session("u7".to_owned(), runtime::SessionStatus::Done);
        assert!(
            wait_until(|| transport.call_count() > base),
            "세션 완료 발송 없음"
        );
        let after = transport.call_count();
        // 같은 세션·같은 상태 재통지는 억제.
        handle.notify_session("u7".to_owned(), runtime::SessionStatus::Done);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(transport.call_count(), after, "세션 완료가 중복 발송됨");
        // 상태 외 값(Running)은 무시.
        handle.notify_session("u7".to_owned(), runtime::SessionStatus::Running);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(transport.call_count(), after, "무관 상태가 발송됨");
        mgr.stop_and_join();
        let _ = std::fs::remove_file(&db_path);
    }

    // ── P1: 발송 실패는 중복 억제 마킹을 커밋하지 않는다(무음 유실 방지) ──────────

    #[test]
    fn 승인_발송이_전량_실패하면_다음_주기에_재시도한다() {
        let db_path = temp_db();
        insert_pending(&db_path, "appr-flaky");
        let endpoint = "https://push.example/flaky";
        let transport = Arc::new(FakeTransport::with(HashMap::from([(
            endpoint.to_owned(),
            Reply::Status(503),
        )])));
        let mgr = PushManager::spawn_with_interval(
            db_path.clone(),
            VapidKey::generate(),
            Box::new(SharedTransport(Arc::clone(&transport))),
            FAST_POLL,
        )
        .unwrap();
        let handle = mgr.handle();
        handle
            .add_subscription(endpoint, RFC_UA_PUBLIC, RFC_AUTH)
            .unwrap();

        // 발송 1회 = POST 2회(재시도 1회 포함). 전량 실패면 마킹이 커밋되지 않아 다음 폴링
        // 주기에 같은 승인을 다시 보낸다 → POST가 4회 이상으로 늘어난다.
        assert!(
            wait_until(|| transport.call_count() >= 4),
            "전량 실패인데 다음 주기 재시도가 없었다(마킹이 먼저 커밋됨 — 영구 유실)"
        );

        // 회복(201) → 성공하면 그제서야 마킹이 커밋돼 재발송이 멎는다.
        transport.set(endpoint, Reply::Status(201));
        let settled = transport.settle();
        std::thread::sleep(FAST_POLL * 4);
        assert_eq!(
            transport.call_count(),
            settled,
            "성공 후에도 재발송이 계속된다(마킹이 커밋되지 않음)"
        );

        mgr.stop_and_join();
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn 승인_발송이_부분_성공하면_마킹을_커밋한다() {
        let db_path = temp_db();
        let _ = storage::Db::open(&db_path).unwrap(); // 파일 생성(승인은 구독 등록 후에 넣는다)
        let good = "https://push.example/good";
        let bad = "https://push.example/bad";
        // bad는 네트워크 실패(타임아웃/DNS) — 재시도 1회 후 Failed.
        let transport = Arc::new(FakeTransport::with(HashMap::from([(
            bad.to_owned(),
            Reply::Network,
        )])));
        let mgr = PushManager::spawn_with_interval(
            db_path.clone(),
            VapidKey::generate(),
            Box::new(SharedTransport(Arc::clone(&transport))),
            FAST_POLL,
        )
        .unwrap();
        let handle = mgr.handle();
        handle
            .add_subscription(good, RFC_UA_PUBLIC, RFC_AUTH)
            .unwrap();
        handle
            .add_subscription(bad, RFC_UA_PUBLIC, RFC_AUTH)
            .unwrap();
        // 두 구독이 모두 등록된 뒤에 승인을 넣어야 한 번의 발송이 둘 다 대상으로 한다.
        wait_until(|| handle.poll_count() >= 1);
        insert_pending(&db_path, "appr-partial");

        assert!(
            wait_until(|| transport.calls_to(good) >= 1),
            "정상 구독으로의 발송이 없었다"
        );
        // 하나라도 전달됐으면 마킹 커밋 — 실패한 기기 때문에 재발송하면 성공한 기기에 중복
        // 알림이 간다. 실패 기기는 다음 새 승인에서 재수렴한다.
        let settled = transport.settle();
        std::thread::sleep(FAST_POLL * 4);
        assert_eq!(
            transport.call_count(),
            settled,
            "부분 성공인데 재발송됐다(성공한 기기에 중복 알림)"
        );
        assert_eq!(
            transport.calls_to(good),
            1,
            "성공 구독에 중복 발송됐다: {settled}"
        );
        assert_eq!(
            transport.calls_to(bad),
            2,
            "실패 구독은 발송당 재시도 1회(총 2회 POST)여야 한다"
        );

        mgr.stop_and_join();
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn 세션_알림_전량_실패는_마킹하지_않고_상한까지_재시도한다() {
        let db_path = temp_db();
        let endpoint = "https://push.example/session-flaky";
        let transport = Arc::new(FakeTransport::with(HashMap::from([(
            endpoint.to_owned(),
            Reply::Status(500),
        )])));
        let mgr = PushManager::spawn_with_interval(
            db_path.clone(),
            VapidKey::generate(),
            Box::new(SharedTransport(Arc::clone(&transport))),
            FAST_POLL,
        )
        .unwrap();
        let handle = mgr.handle();
        handle
            .add_subscription(endpoint, RFC_UA_PUBLIC, RFC_AUTH)
            .unwrap();
        wait_until(|| handle.poll_count() >= 1);
        let base = transport.call_count();

        // 세션 전이는 생애 1회라 실패를 버리면 영구 유실 — 재시도 큐가 다음 주기에 다시 보낸다.
        handle.notify_session("u7".to_owned(), runtime::SessionStatus::Done);
        assert!(
            wait_until(|| transport.call_count() >= base + 4),
            "세션 알림 전량 실패인데 다음 주기 재시도가 없었다"
        );
        // 재시도 상한(SESSION_SEND_ATTEMPTS)까지만 — 그 뒤로는 폭주하지 않고 포기한다.
        let capped = base + 2 * SESSION_SEND_ATTEMPTS as usize;
        assert!(
            wait_until(|| transport.call_count() >= capped),
            "재시도 상한까지 시도하지 않았다"
        );
        std::thread::sleep(FAST_POLL * 4);
        assert_eq!(
            transport.call_count(),
            capped,
            "재시도 상한을 넘겨 계속 시도한다(폭주)"
        );

        // 전량 실패는 마킹을 남기지 않았다 — 회복 후 같은 전이가 다시 오면 정상 발송된다.
        transport.set(endpoint, Reply::Status(201));
        handle.notify_session("u7".to_owned(), runtime::SessionStatus::Done);
        assert!(
            wait_until(|| transport.call_count() > capped),
            "실패가 마킹으로 굳어 재통지가 억제됐다(무음 유실)"
        );
        // 성공 뒤에는 다시 중복 억제된다(기존 동작 회귀 없음).
        let after = transport.settle();
        handle.notify_session("u7".to_owned(), runtime::SessionStatus::Done);
        std::thread::sleep(FAST_POLL * 4);
        assert_eq!(
            transport.call_count(),
            after,
            "성공 후 같은 전이가 중복 발송됐다"
        );

        mgr.stop_and_join();
        let _ = std::fs::remove_file(&db_path);
    }

    // ── P3-1: notified_status 유계화 ────────────────────────────────────────

    #[test]
    fn notified_status는_상한을_넘지_않고_오래된_키부터_버린다() {
        let mut notified: HashMap<String, SessionKind> = HashMap::new();
        let count = MAX_NOTIFIED_SESSIONS + 10;
        // 세션 키는 영속 UUID다(I1) — 단조성은 없으므로 사전순 최소 키부터 버린다.
        // 자리수를 맞춰 사전순 = 생성순이 되게 만든다(테스트 결정성).
        let key = |i: usize| format!("uuid-{i:06}");
        for i in 1..=count {
            remember_status(&mut notified, &key(i), SessionKind::Done);
        }
        assert_eq!(
            notified.len(),
            MAX_NOTIFIED_SESSIONS,
            "세션 기록이 상한 없이 쌓인다"
        );
        assert!(
            !notified.contains_key(&key(1)),
            "가장 오래된 키가 남아 있다"
        );
        assert!(notified.contains_key(&key(count)), "최신 키가 버려졌다");
        // 같은 키 재기록은 크기를 늘리지 않고 상태만 갱신한다.
        remember_status(&mut notified, &key(count), SessionKind::Waiting);
        assert_eq!(notified.len(), MAX_NOTIFIED_SESSIONS);
        assert_eq!(notified.get(&key(count)), Some(&SessionKind::Waiting));
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
