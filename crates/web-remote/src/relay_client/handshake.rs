//! Mac 쪽 Relay 세션 오케스트레이션 — I/O 없음, 시계와 신원 공급자는 주입받는다.
//!
//! [`super::session::RelaySessionGate`]가 "채널을 통과한 평문만 명령이 된다"를 지킨다면, 이
//! 모듈은 **그 채널이 어떻게 서는가**를 결정한다. 순서는 하나뿐이다:
//!
//! ```text
//! 접속 ──DesktopAdmission──▶ Admitted ──PeerJoined(연결 id)──▶ 기기 Offer(서명 없음)
//!   ──▶ Mac IdentityHello(양쪽 offer에 서명) ──▶ 기기 IdentityHello ──▶ 서명 검증
//!   ──▶ 기기가 소유를 주장(0x02 페어링 증명 / 0x03 기존 기기) ──▶ 조정자가 채널을 게이트에 건다
//! ```
//!
//! 왜 기기가 **서명 없는** Offer로 먼저 문을 여는가: 서명은 양쪽 offer를 이은 canonical
//! transcript를 덮는다(`PendingHandshake::sign_peer_offer`). 그러므로 상대의 임시키를 보기
//! 전에는 어느 쪽도 서명할 수 없다. 기기가 offer를 내고, Mac이 그 offer와 자기 offer에 서명한
//! hello로 답하고, 그제서야 기기도 서명한다. Mac의 신원과 서명은 기기의 offer가 곡선 위의
//! 정상 키 쌍일 때만 와이어에 오르고, 채널은 기기의 서명까지 검증된 뒤에만 열린다.
//!
//! 실패는 전부 fail-closed다. 어긋난 레코드·순서·연결 id·서명·마감은 모두 세션 종료로 끝나며,
//! 워커는 그것을 전송 실패로 기록해 백오프한다(상대가 다시 붙을 수는 있되, 즉시는 아니다).

use relay_protocol::{AdmissionCredential, FrameType, RelayFrame, RouteId};

use crate::relay::contract::{ConnectionId, DeviceId, PairingId, RELAY_ID_BYTES};
use crate::relay::crypto::{
    AuthenticatedHandshake, PendingHandshake, RELAY_PROTOCOL_VERSION, RelayHello, RelayIdentity,
    RelayOffer, RelayRole,
};
use crate::relay::pairing::PAIRING_PROOF_BYTES;

/// 서명 없는 제시(기기 → Mac). IdentityHello에서 서명만 뺀 모양이다.
pub const HELLO_TAG_OFFER: u8 = 0x00;
/// 서명된 신원 제시. `RelayHello`가 소비하는 재료와 정확히 같은 바이트다.
pub const HELLO_TAG_IDENTITY: u8 = 0x01;
/// 새 페어링의 소유 증명(기기 → Mac).
pub const HELLO_TAG_PAIRING_PROOF: u8 = 0x02;
/// 이미 페어링된 기기의 자기 지목(기기 → Mac).
pub const HELLO_TAG_KNOWN_DEVICE: u8 = 0x03;

/// `0x00 || version u32 || role u8 || connection 16 || identity 65 || ephemeral 65`.
pub const OFFER_RECORD_BYTES: usize = 1 + 4 + 1 + RELAY_ID_BYTES + 65 + 65;
/// `0x01 || version u32 || role u8 || connection 16 || identity 65 || ephemeral 65 || sig 64`.
pub const IDENTITY_HELLO_BYTES: usize = OFFER_RECORD_BYTES + 64;
/// `0x02 || pairing_id 16 || proof 32`.
pub const PAIRING_PROOF_RECORD_BYTES: usize = 1 + RELAY_ID_BYTES + PAIRING_PROOF_BYTES;
/// `0x03 || device_id 16`.
pub const KNOWN_DEVICE_RECORD_BYTES: usize = 1 + RELAY_ID_BYTES;

/// 상대가 붙은 뒤 핸드셰이크가 끝나야 하는 시한. 붙어만 두고 아무 말도 하지 않는 상대가
/// 라우트를 무한정 점유하지 못하게 한다. 사용자 승인 대기는 이 시한에 묶이지 않는다 —
/// 그쪽 마감은 페어링 티켓의 5분이다.
pub const HANDSHAKE_DEADLINE_SECS: u64 = 30;

/// 생존 신호 주기. 서버는 **DRLY 프레임 수신 시각**으로만 유휴를 판정하며(WebSocket ping은
/// 세지 않는다) 기본 유휴 상한이 60초다. 조용한 Mac은 아무것도 보낼 것이 없으므로, 이 주기로
/// 빈 `Heartbeat`를 보내지 않으면 붙어 있는 것만으로 60초마다 라우트와 티켓이 통째로 사라진다.
pub const HEARTBEAT_INTERVAL_SECS: u64 = 20;

const ROLE_DESKTOP: u8 = 1;
const ROLE_DEVICE: u8 = 2;

/// 아직 이 기기 세션의 연결 id가 없을 때 쓰는 값. 라우트 입장 프레임은 `PeerJoined` 이전에
/// 나가므로 가리킬 연결이 없다.
const UNBOUND_CONNECTION: [u8; RELAY_ID_BYTES] = [0; RELAY_ID_BYTES];

/// 세션마다 새로 필요한 이 Mac의 신원.
///
/// `PendingHandshake::begin`이 신원을 **소비**하므로 한 번 만든 값을 다음 세션에 다시 쓸 수
/// 없다. 그래서 상태 기계는 신원이 아니라 공급자를 받는다 — 그리고 그 덕분에 상태 기계 자신은
/// Keychain을 모른 채 남는다.
pub type RelayIdentitySupplier = Box<dyn Fn() -> anyhow::Result<RelayIdentity> + Send>;

/// 주입되는 시계(UNIX 초). 실제 시각은 앱이 정한다.
pub type RelayClock = Box<dyn Fn() -> u64 + Send>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelloDecodeError {
    /// 페이로드가 비었다. DRLY는 1바이트 이상을 보장하지만 계약을 여기서도 닫는다.
    Empty,
    UnknownTag(u8),
    /// 태그는 알지만 길이가 그 태그의 고정 모양이 아니다.
    WrongLength {
        tag: u8,
        len: usize,
    },
    /// 길이는 맞지만 내용이 유효한 제시가 아니다(곡선 밖의 점, 잘못된 서명 형식 등).
    Invalid,
}

/// 와이어에 오르는 pre-E2EE 레코드. 넷 다 512바이트 상한 안이며 Relay에게는 불투명하다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HelloRecord {
    Offer(RelayOffer),
    Identity(RelayHello),
    PairingProof {
        pairing_id: PairingId,
        proof: [u8; PAIRING_PROOF_BYTES],
    },
    KnownDevice {
        device_id: DeviceId,
    },
}

fn role_byte(role: RelayRole) -> u8 {
    match role {
        RelayRole::Desktop => ROLE_DESKTOP,
        RelayRole::Device => ROLE_DEVICE,
    }
}

/// 서명 없는 제시를 와이어 바이트로 만든다.
pub fn encode_offer(offer: &RelayOffer) -> [u8; OFFER_RECORD_BYTES] {
    let mut record = [0u8; OFFER_RECORD_BYTES];
    record[0] = HELLO_TAG_OFFER;
    record[1..5].copy_from_slice(&offer.version().to_be_bytes());
    record[5] = role_byte(offer.role());
    let mut at = 6;
    record[at..at + RELAY_ID_BYTES].copy_from_slice(offer.connection_id().as_bytes());
    at += RELAY_ID_BYTES;
    record[at..at + 65].copy_from_slice(offer.identity_public_sec1());
    at += 65;
    record[at..at + 65].copy_from_slice(offer.ephemeral_public_sec1());
    record
}

/// 서명된 신원 제시를 와이어 바이트로 만든다.
pub fn encode_identity_hello(hello: &RelayHello) -> [u8; IDENTITY_HELLO_BYTES] {
    let mut record = [0u8; IDENTITY_HELLO_BYTES];
    record[..OFFER_RECORD_BYTES].copy_from_slice(&encode_offer(hello.offer()));
    record[0] = HELLO_TAG_IDENTITY;
    record[OFFER_RECORD_BYTES..].copy_from_slice(hello.signature_raw());
    record
}

pub fn encode_pairing_proof(
    pairing_id: PairingId,
    proof: &[u8; PAIRING_PROOF_BYTES],
) -> [u8; PAIRING_PROOF_RECORD_BYTES] {
    let mut record = [0u8; PAIRING_PROOF_RECORD_BYTES];
    record[0] = HELLO_TAG_PAIRING_PROOF;
    record[1..1 + RELAY_ID_BYTES].copy_from_slice(pairing_id.as_bytes());
    record[1 + RELAY_ID_BYTES..].copy_from_slice(proof);
    record
}

pub fn encode_known_device(device_id: DeviceId) -> [u8; KNOWN_DEVICE_RECORD_BYTES] {
    let mut record = [0u8; KNOWN_DEVICE_RECORD_BYTES];
    record[0] = HELLO_TAG_KNOWN_DEVICE;
    record[1..].copy_from_slice(device_id.as_bytes());
    record
}

/// 레코드 하나를 판정한다. **모양이 정확히 맞지 않으면 거절한다** — 남는 바이트도, 모자란
/// 바이트도 허용하지 않는다.
pub fn decode_hello_record(record: &[u8]) -> Result<HelloRecord, HelloDecodeError> {
    let Some((tag, body)) = record.split_first() else {
        return Err(HelloDecodeError::Empty);
    };
    match *tag {
        HELLO_TAG_OFFER | HELLO_TAG_IDENTITY => {
            let expected = if *tag == HELLO_TAG_OFFER {
                OFFER_RECORD_BYTES
            } else {
                IDENTITY_HELLO_BYTES
            };
            require_len(*tag, record.len(), expected)?;
            let version = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
            let role = match body[4] {
                ROLE_DESKTOP => RelayRole::Desktop,
                ROLE_DEVICE => RelayRole::Device,
                _ => return Err(HelloDecodeError::Invalid),
            };
            let mut at = 5;
            let connection_id = ConnectionId::from_bytes(fixed_id(&body[at..at + RELAY_ID_BYTES]));
            at += RELAY_ID_BYTES;
            let identity = &body[at..at + 65];
            at += 65;
            let ephemeral = &body[at..at + 65];
            at += 65;
            if *tag == HELLO_TAG_OFFER {
                RelayOffer::from_webcrypto_parts(version, role, connection_id, identity, ephemeral)
                    .map(HelloRecord::Offer)
                    .map_err(|_| HelloDecodeError::Invalid)
            } else {
                let signature = &body[at..at + 64];
                RelayHello::from_webcrypto_parts(
                    version,
                    role,
                    connection_id,
                    identity,
                    ephemeral,
                    signature,
                )
                .map(HelloRecord::Identity)
                .map_err(|_| HelloDecodeError::Invalid)
            }
        }
        HELLO_TAG_PAIRING_PROOF => {
            require_len(*tag, record.len(), PAIRING_PROOF_RECORD_BYTES)?;
            let mut proof = [0u8; PAIRING_PROOF_BYTES];
            proof.copy_from_slice(&body[RELAY_ID_BYTES..]);
            Ok(HelloRecord::PairingProof {
                pairing_id: PairingId::from_bytes(fixed_id(&body[..RELAY_ID_BYTES])),
                proof,
            })
        }
        HELLO_TAG_KNOWN_DEVICE => {
            require_len(*tag, record.len(), KNOWN_DEVICE_RECORD_BYTES)?;
            Ok(HelloRecord::KnownDevice {
                device_id: DeviceId::from_bytes(fixed_id(body)),
            })
        }
        unknown => Err(HelloDecodeError::UnknownTag(unknown)),
    }
}

fn require_len(tag: u8, actual: usize, expected: usize) -> Result<(), HelloDecodeError> {
    if actual == expected {
        Ok(())
    } else {
        Err(HelloDecodeError::WrongLength { tag, len: actual })
    }
}

fn fixed_id(bytes: &[u8]) -> [u8; RELAY_ID_BYTES] {
    let mut id = [0u8; RELAY_ID_BYTES];
    id.copy_from_slice(bytes);
    id
}

/// 왜 세션을 끊었는가. 전부 조용한 종료이며 상대에게 사유를 알리지 않는다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandshakeFailure {
    /// 라우트 입장이 허가되기 전에 온 프레임.
    NotAdmitted,
    /// 상대가 붙기 전에 온 hello.
    NotJoined,
    /// 이 기기 세션의 연결 id가 아니다.
    WrongConnection,
    /// 레코드를 해석할 수 없다.
    MalformedRecord,
    /// 이 단계에서 올 수 없는 레코드다(순서 어긋남·재생·중복).
    UnexpectedRecord,
    /// 서명·transcript 검증에 실패했다.
    Unauthenticated,
    /// 이 Mac의 신원을 가져오지 못했다(Keychain 거부 등).
    IdentityUnavailable,
    /// 시한 안에 핸드셰이크가 끝나지 않았다.
    DeadlineExceeded,
    /// 조정자가 주장을 거절했다(증명 실패·사용자 거부·취소).
    Rejected,
    /// 상대가 떨어졌다.
    PeerLeft,
}

/// 서명이 검증된 상대. 여기서부터는 "누구인가"가 아니라 "무엇을 허락할 것인가"의 문제다.
#[derive(Debug)]
pub struct AuthenticatedPeer {
    pub handshake: AuthenticatedHandshake,
    pub identity_public_sec1: [u8; 65],
    pub connection_id: ConnectionId,
    /// 주입된 시계로 찍은 관측 시각. 저장소 인가 판정이 이 값을 쓴다.
    pub observed_at: u64,
}

/// 새 페어링 주장. 증명 검증은 페어링 비밀을 소유한 조정자(앱)가 한다 — 이 상태 기계는
/// 레지스트리를 모른다.
#[derive(Debug)]
pub struct PairingClaim {
    pub pairing_id: PairingId,
    pub proof: [u8; PAIRING_PROOF_BYTES],
    pub peer: AuthenticatedPeer,
}

/// 이미 페어링된 기기의 주장. 저장소 조회와 취소 확인은 조정자가 한다.
#[derive(Debug)]
pub struct KnownDeviceClaim {
    pub device_id: DeviceId,
    pub peer: AuthenticatedPeer,
}

/// 프레임 하나를 처리한 뒤 조정자가 무엇을 해야 하는가.
#[derive(Debug)]
pub enum HandshakeStep {
    /// 아무 결정도 필요 없다. 계속 받는다.
    Continue,
    /// 새 페어링 — 사용자에게 확인 코드를 보이고 승인을 받아야 한다.
    Pairing(Box<PairingClaim>),
    /// 이미 페어링된 기기 — 저장소를 조회해 바로 활성화할 수 있다.
    Known(Box<KnownDeviceClaim>),
    /// 세션을 끊는다.
    Fail(HandshakeFailure),
}

enum Phase {
    /// 라우트 입장 자격증명을 냈고 허가를 기다린다.
    Admitting,
    /// 입장 허가됨. 상대를 기다린다.
    Waiting,
    /// 상대가 붙었다 — 이 기기 세션의 연결 id가 정해졌다. 기기의 서명 없는 offer를 기다린다.
    Joined {
        connection_id: ConnectionId,
        deadline: u64,
    },
    /// 기기 offer를 받아 이 Mac의 서명된 hello를 내보냈다. 기기의 서명된 hello를 기다린다.
    Offered {
        pending: Box<PendingHandshake>,
        connection_id: ConnectionId,
        peer_identity_public_sec1: [u8; 65],
        deadline: u64,
    },
    /// 양쪽 hello가 오갔고 서명이 검증됐다. 기기의 소유 주장을 기다린다.
    Authenticated {
        peer: Box<AuthenticatedPeer>,
        deadline: u64,
    },
    /// 주장을 조정자에게 넘겼다. 승인(또는 저장소 판정)을 기다린다 — 이 대기는 핸드셰이크
    /// 시한이 아니라 페어링 티켓의 마감에 묶인다.
    Proposed { connection_id: ConnectionId },
    /// 채널이 게이트에 걸렸다.
    Active,
    /// 끝났다. 이 세션에서 더 받아 줄 것은 없다.
    Failed,
}

/// Mac 쪽 세션 상태 기계. 소켓도 저장소도 레지스트리도 모른다.
pub struct RelayHandshake {
    route: RouteId,
    admission: AdmissionCredential,
    identity: RelayIdentitySupplier,
    clock: RelayClock,
    phase: Phase,
    outbound: Vec<Vec<u8>>,
    sequence: u64,
    /// 진행 중인 페어링의 기기 입장 티켓. 라우트는 세션과 함께 사라지므로(서버는 Mac이
    /// 끊기면 라우트와 티켓을 통째로 지운다) 새 세션이 입장 허가를 받을 때마다 다시 게시한다.
    ticket: Option<AdmissionCredential>,
    /// 이 세션에서 티켓을 이미 게시했는가. 같은 핸들의 재게시는 서버가 거절 프레임으로 답한다.
    ticket_published: bool,
    /// 마지막으로 프레임을 큐에 넣은 시각. 생존 신호 주기를 이 값으로 잰다.
    last_queued_at: u64,
    /// 조정자가 주장을 거절했다 — 다음 tick에서 세션을 끝낸다. **큐를 비우지는 않는다**:
    /// 같은 drain에서 나온 `TicketRevoke`가 함께 사라지면 1회용 티켓이 서버에 남는다.
    close_requested: bool,
}

impl RelayHandshake {
    pub fn new(
        route: RouteId,
        admission: AdmissionCredential,
        identity: RelayIdentitySupplier,
        clock: RelayClock,
    ) -> Self {
        Self {
            route,
            admission,
            identity,
            clock,
            phase: Phase::Failed,
            outbound: Vec::new(),
            sequence: 0,
            ticket: None,
            ticket_published: false,
            last_queued_at: 0,
            close_requested: false,
        }
    }

    /// 새 세션이 열렸다. 라우트 소유 자격증명을 내고 허가를 기다린다.
    ///
    /// 세션마다 처음부터 다시 시작한다 — 이전 세션의 연결 id·상대·시퀀스를 물려받으면
    /// 끊긴 세션의 상태로 새 상대를 판정하게 된다.
    pub fn session_started(&mut self) {
        self.phase = Phase::Admitting;
        self.outbound.clear();
        self.sequence = 0;
        self.ticket_published = false;
        self.close_requested = false;
        self.last_queued_at = self.now();
        let credential = *self.admission.as_bytes();
        self.queue(FrameType::DesktopAdmission, UNBOUND_CONNECTION, &credential);
    }

    /// 페어링 의식이 시작됐다 — 기기 입장 티켓을 Relay에 게시한다. 아직 입장 허가 전이면
    /// 허가 직후 나가고, 세션이 바뀌면 다시 나간다. 티켓 자체는 5분 1회용이다.
    pub fn publish_ticket(&mut self, handle: AdmissionCredential) {
        self.ticket = Some(handle);
        self.ticket_published = false;
        self.publish_ticket_if_admitted();
    }

    /// 의식이 끝났다(취소·거부·만료·승인). 게시된 티켓이 있으면 회수한다 — 기기가 이미
    /// 소비한 뒤라면 서버에는 남은 것이 없고, 회수는 그저 아무 일도 하지 않는다.
    pub fn revoke_ticket(&mut self) {
        let Some(handle) = self.ticket.take() else {
            return;
        };
        if self.ticket_published && self.is_admitted() {
            self.queue(
                FrameType::TicketRevoke,
                UNBOUND_CONNECTION,
                handle.as_bytes(),
            );
        }
        self.ticket_published = false;
    }

    fn publish_ticket_if_admitted(&mut self) {
        if self.ticket_published || !self.is_admitted() {
            return;
        }
        let Some(handle) = self.ticket else {
            return;
        };
        self.queue(
            FrameType::TicketPublish,
            UNBOUND_CONNECTION,
            handle.as_bytes(),
        );
        self.ticket_published = true;
    }

    /// 라우트 입장이 허가된 세션인가. 티켓 게시·회수는 그 뒤에만 서버가 받는다.
    const fn is_admitted(&self) -> bool {
        !matches!(self.phase, Phase::Admitting | Phase::Failed)
    }

    /// 세션이 끝났다. 남은 상대와 나갈 프레임을 모두 버린다.
    pub fn session_ended(&mut self) {
        self.phase = Phase::Failed;
        self.outbound.clear();
    }

    /// 조정자가 채널을 게이트에 걸었다.
    pub fn activated(&mut self) {
        if matches!(self.phase, Phase::Proposed { .. }) {
            self.phase = Phase::Active;
        }
    }

    /// 조정자가 주장을 거절했다(증명 실패·저장소 불일치·사용자 거부). 세션은 다음 tick에서
    /// 끝난다 — 이미 큐에 든 제어 프레임(특히 `TicketRevoke`)은 그 전에 나가야 한다.
    pub fn rejected(&mut self) {
        self.phase = Phase::Failed;
        self.close_requested = true;
    }

    /// 프레임이 하나도 오지 않는 동안에도 워커가 부르는 주기 점검.
    ///
    /// 셋을 여기서 한다: (1) 조정자의 거절을 세션 종료로 옮기고, (2) 마감이 지난 핸드셰이크를
    /// 끝내며, (3) 생존 신호를 채운다. 이 tick이 없으면 마감은 **상대가 말을 걸어야만**
    /// 평가되고("아무 말도 하지 않는 상대"가 정확히 그 경우다), 조용한 세션은 서버 유휴
    /// 상한에 걸려 끊긴다.
    pub fn tick(&mut self) -> HandshakeStep {
        if self.close_requested {
            self.close_requested = false;
            return self.fail(HandshakeFailure::Rejected);
        }
        if let Some(failure) = self.expired() {
            return self.fail(failure);
        }
        if self.is_admitted()
            && self.now().saturating_sub(self.last_queued_at) >= HEARTBEAT_INTERVAL_SECS
        {
            let connection = self
                .session_connection()
                .map_or(UNBOUND_CONNECTION, |id| *id.as_bytes());
            self.queue(FrameType::Heartbeat, connection, &[]);
        }
        HandshakeStep::Continue
    }

    /// 나갈 프레임을 꺼낸다. 워커가 이 값을 소켓으로 보낸다.
    pub fn take_outbound(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.outbound)
    }

    pub const fn is_proposed(&self) -> bool {
        matches!(self.phase, Phase::Proposed { .. })
    }

    /// 조정자에게 넘어간 주장의 연결 id. 조정자는 승인된 채널의 연결 id가 이 값과 같을 때만
    /// 게이트에 건다 — 그 사이 세션이 바뀌었으면 그 채널은 죽은 세션의 것이다.
    pub const fn proposed_connection(&self) -> Option<ConnectionId> {
        match self.phase {
            Phase::Proposed { connection_id } => Some(connection_id),
            _ => None,
        }
    }

    pub const fn is_active(&self) -> bool {
        matches!(self.phase, Phase::Active)
    }

    /// 서버 제어 프레임 하나.
    pub fn control(&mut self, frame_type: FrameType, connection_id: ConnectionId) -> HandshakeStep {
        if let Some(failure) = self.expired() {
            return self.fail(failure);
        }
        match frame_type {
            // 중복 허가는 무시한다 — 서버가 두 번 보냈다고 세션을 끊을 이유는 없다.
            FrameType::Admitted => {
                if matches!(self.phase, Phase::Admitting) {
                    self.phase = Phase::Waiting;
                    // 입장이 허가됐으니 기다리던 티켓을 게시한다.
                    self.publish_ticket_if_admitted();
                }
                HandshakeStep::Continue
            }
            FrameType::PeerJoined => match self.phase {
                Phase::Admitting => self.fail(HandshakeFailure::NotAdmitted),
                Phase::Waiting => {
                    self.phase = Phase::Joined {
                        connection_id,
                        deadline: self.now().saturating_add(HANDSHAKE_DEADLINE_SECS),
                    };
                    HandshakeStep::Continue
                }
                // 이 라우트의 상대는 하나다. 두 번째 합류는 세션을 갈아치우려는 시도다.
                _ => self.fail(HandshakeFailure::UnexpectedRecord),
            },
            FrameType::PeerLeft => self.fail(HandshakeFailure::PeerLeft),
            _ => HandshakeStep::Continue,
        }
    }

    /// hello 레코드 하나. **여기가 유일한 채널 개설 경로다.**
    pub fn hello(&mut self, connection_id: ConnectionId, record: &[u8]) -> HandshakeStep {
        if let Some(failure) = self.expired() {
            return self.fail(failure);
        }
        match &self.phase {
            Phase::Admitting => return self.fail(HandshakeFailure::NotAdmitted),
            Phase::Waiting => return self.fail(HandshakeFailure::NotJoined),
            Phase::Proposed { .. } | Phase::Active | Phase::Failed => {
                return self.fail(HandshakeFailure::UnexpectedRecord);
            }
            Phase::Joined { .. } | Phase::Offered { .. } | Phase::Authenticated { .. } => {}
        }
        // DRLY 헤더의 연결 id가 이 기기 세션의 것과 다르면 다른 세션의 프레임이다.
        if self.session_connection() != Some(connection_id) {
            return self.fail(HandshakeFailure::WrongConnection);
        }
        let record = match decode_hello_record(record) {
            Ok(record) => record,
            Err(_) => return self.fail(HandshakeFailure::MalformedRecord),
        };
        match (&self.phase, record) {
            (Phase::Joined { .. }, HelloRecord::Offer(offer)) => {
                self.answer_offer(connection_id, offer)
            }
            (Phase::Offered { .. }, HelloRecord::Identity(hello)) => {
                self.authenticate(connection_id, hello)
            }
            (Phase::Authenticated { .. }, HelloRecord::PairingProof { pairing_id, proof }) => {
                let Some(peer) = self.take_peer(connection_id) else {
                    return self.fail(HandshakeFailure::UnexpectedRecord);
                };
                HandshakeStep::Pairing(Box::new(PairingClaim {
                    pairing_id,
                    proof,
                    peer,
                }))
            }
            (Phase::Authenticated { .. }, HelloRecord::KnownDevice { device_id }) => {
                let Some(peer) = self.take_peer(connection_id) else {
                    return self.fail(HandshakeFailure::UnexpectedRecord);
                };
                HandshakeStep::Known(Box::new(KnownDeviceClaim { device_id, peer }))
            }
            // offer 전의 hello, offer의 재생, 신원 제시 전의 소유 주장 — 전부 순서가 어긋났다.
            _ => self.fail(HandshakeFailure::UnexpectedRecord),
        }
    }

    /// 기기의 서명 없는 offer를 받았다. 이 Mac의 핸드셰이크를 시작하고 **양쪽 offer에 서명한**
    /// hello를 내보낸다. 서명 자체는 아직 검증할 것이 없다 — 기기의 서명은 다음 레코드다.
    fn answer_offer(&mut self, connection_id: ConnectionId, offer: RelayOffer) -> HandshakeStep {
        if offer.role() != RelayRole::Device {
            return self.fail(HandshakeFailure::UnexpectedRecord);
        }
        if offer.connection_id() != connection_id {
            return self.fail(HandshakeFailure::WrongConnection);
        }
        if offer.version() != RELAY_PROTOCOL_VERSION {
            return self.fail(HandshakeFailure::Unauthenticated);
        }
        let Ok(identity) = (self.identity)() else {
            return self.fail(HandshakeFailure::IdentityUnavailable);
        };
        let peer_identity_public_sec1 = *offer.identity_public_sec1();
        let Ok(pending) = PendingHandshake::begin(
            identity,
            peer_identity_public_sec1.to_vec(),
            RelayRole::Desktop,
            RELAY_PROTOCOL_VERSION,
            connection_id,
        ) else {
            return self.fail(HandshakeFailure::IdentityUnavailable);
        };
        let Ok(own_hello) = pending.sign_peer_offer(&offer) else {
            return self.fail(HandshakeFailure::Unauthenticated);
        };
        let record = encode_identity_hello(&own_hello);
        self.queue(FrameType::Hello, *connection_id.as_bytes(), &record);
        self.phase = Phase::Offered {
            pending: Box::new(pending),
            connection_id,
            peer_identity_public_sec1,
            deadline: self.now().saturating_add(HANDSHAKE_DEADLINE_SECS),
        };
        HandshakeStep::Continue
    }

    /// 기기의 서명된 hello를 검증한다. offer 때 제시한 신원과 다르면 다른 상대다.
    fn authenticate(&mut self, connection_id: ConnectionId, hello: RelayHello) -> HandshakeStep {
        if hello.role() != RelayRole::Device || hello.connection_id() != connection_id {
            return self.fail(HandshakeFailure::UnexpectedRecord);
        }
        let Phase::Offered {
            pending,
            peer_identity_public_sec1,
            ..
        } = std::mem::replace(&mut self.phase, Phase::Failed)
        else {
            return self.fail(HandshakeFailure::UnexpectedRecord);
        };
        if *hello.identity_public_sec1() != peer_identity_public_sec1 {
            return self.fail(HandshakeFailure::Unauthenticated);
        }
        let Ok(handshake) = pending.finish(hello) else {
            return self.fail(HandshakeFailure::Unauthenticated);
        };
        self.phase = Phase::Authenticated {
            peer: Box::new(AuthenticatedPeer {
                handshake,
                identity_public_sec1: peer_identity_public_sec1,
                connection_id,
                observed_at: self.now(),
            }),
            deadline: self.now().saturating_add(HANDSHAKE_DEADLINE_SECS),
        };
        HandshakeStep::Continue
    }

    fn take_peer(&mut self, connection_id: ConnectionId) -> Option<AuthenticatedPeer> {
        let Phase::Authenticated { peer, .. } =
            std::mem::replace(&mut self.phase, Phase::Proposed { connection_id })
        else {
            return None;
        };
        Some(*peer)
    }

    fn session_connection(&self) -> Option<ConnectionId> {
        match &self.phase {
            Phase::Joined { connection_id, .. } | Phase::Offered { connection_id, .. } => {
                Some(*connection_id)
            }
            Phase::Authenticated { peer, .. } => Some(peer.connection_id),
            Phase::Proposed { connection_id } => Some(*connection_id),
            _ => None,
        }
    }

    /// 진행 중인 핸드셰이크의 시한이 지났는가. 승인 대기(`Proposed`)는 여기 걸리지 않는다.
    fn expired(&self) -> Option<HandshakeFailure> {
        let deadline = match &self.phase {
            Phase::Joined { deadline, .. }
            | Phase::Offered { deadline, .. }
            | Phase::Authenticated { deadline, .. } => *deadline,
            _ => return None,
        };
        (self.now() >= deadline).then_some(HandshakeFailure::DeadlineExceeded)
    }

    fn fail(&mut self, failure: HandshakeFailure) -> HandshakeStep {
        self.phase = Phase::Failed;
        self.outbound.clear();
        HandshakeStep::Fail(failure)
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    fn queue(
        &mut self,
        frame_type: FrameType,
        connection_id: [u8; RELAY_ID_BYTES],
        payload: &[u8],
    ) {
        let Ok(frame) = RelayFrame::new(
            frame_type,
            self.route,
            relay_protocol::ConnectionId::from_bytes(connection_id),
            self.sequence,
            payload,
        ) else {
            // 상한을 넘는 프레임은 만들어지지 않는다. 만들 수 없으면 보내지 않는다.
            return;
        };
        self.sequence = self.sequence.saturating_add(1);
        self.last_queued_at = self.now();
        self.outbound.push(frame.to_vec());
    }
}

impl std::fmt::Debug for RelayHandshake {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let phase = match &self.phase {
            Phase::Admitting => "Admitting",
            Phase::Waiting => "Waiting",
            Phase::Joined { .. } => "Joined",
            Phase::Offered { .. } => "Offered",
            Phase::Authenticated { .. } => "Authenticated",
            Phase::Proposed { .. } => "Proposed",
            Phase::Active => "Active",
            Phase::Failed => "Failed",
        };
        formatter
            .debug_struct("RelayHandshake")
            .field("phase", &phase)
            .field("outbound", &self.outbound.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::relay::contract::RelayPermissions;
    use crate::relay::crypto::SecureChannel;
    use crate::relay::pairing::{PairingBinding, PairingRegistry, PairingSecret, pairing_proof};
    use crate::relay::repository::RelayDeviceRecord;
    use crate::relay_client::session::{GateOutcome, RelaySessionGate};

    const ROUTE: RouteId = RouteId::from_bytes([0x41; RELAY_ID_BYTES]);
    const CONNECTION: [u8; RELAY_ID_BYTES] = *b"connection-a-001";
    const OTHER_CONNECTION: [u8; RELAY_ID_BYTES] = *b"connection-b-002";
    const ADMISSION: [u8; 32] = [0x7e; 32];
    const DESKTOP_IDENTITY_SCALAR: [u8; 32] = [0x11; 32];
    const DEVICE_IDENTITY_SCALAR: [u8; 32] = [0x22; 32];
    const ROGUE_IDENTITY_SCALAR: [u8; 32] = [0x33; 32];
    const DEVICE_EPHEMERAL_SCALAR: [u8; 32] = [0x55; 32];
    const NOW: u64 = 1_800_000_000;
    const DEVICE_ID: [u8; RELAY_ID_BYTES] = [0x20; RELAY_ID_BYTES];

    // ---------------------------------------------------------------- Mac 쪽 하네스

    /// 앱 싱크가 하는 배선을 그대로 축약한 조정자: 게이트와 상태 기계를 잇는다.
    struct Desktop {
        gate: RelaySessionGate,
        handshake: RelayHandshake,
        clock: Arc<AtomicU64>,
        identity_calls: Arc<AtomicU64>,
    }

    /// 조정자가 한 프레임을 처리한 결과.
    #[derive(Debug)]
    enum Handled {
        Continue,
        Plaintext(Vec<u8>),
        Pairing(Box<PairingClaim>),
        Known(Box<KnownDeviceClaim>),
        Closed(Option<HandshakeFailure>),
    }

    impl Desktop {
        fn new() -> Self {
            Self::with_identity(DESKTOP_IDENTITY_SCALAR)
        }

        fn with_identity(scalar: [u8; 32]) -> Self {
            let clock = Arc::new(AtomicU64::new(NOW));
            let ticking = Arc::clone(&clock);
            let identity_calls = Arc::new(AtomicU64::new(0));
            let counting = Arc::clone(&identity_calls);
            let mut handshake = RelayHandshake::new(
                ROUTE,
                AdmissionCredential::from_bytes(ADMISSION),
                Box::new(move || {
                    counting.fetch_add(1, Ordering::SeqCst);
                    RelayIdentity::from_private_scalar(scalar)
                }),
                Box::new(move || ticking.load(Ordering::SeqCst)),
            );
            handshake.session_started();
            Self {
                gate: RelaySessionGate::new(ROUTE),
                handshake,
                clock,
                identity_calls,
            }
        }

        fn advance(&self, seconds: u64) {
            self.clock.fetch_add(seconds, Ordering::SeqCst);
        }

        fn outbound(&mut self) -> Vec<Vec<u8>> {
            self.handshake.take_outbound()
        }

        /// 원시 바이트 하나를 받는다. 앱 싱크의 `accept`와 같은 순서다.
        fn receive(&mut self, frame: &[u8]) -> Handled {
            let step = match self.gate.receive(frame) {
                GateOutcome::Plaintext(plaintext) => return Handled::Plaintext(plaintext),
                GateOutcome::Close => return Handled::Closed(None),
                GateOutcome::Dropped(_) => return Handled::Continue,
                GateOutcome::Control {
                    frame_type,
                    connection_id,
                } => self.handshake.control(frame_type, connection_id),
                GateOutcome::Hello {
                    connection_id,
                    record,
                } => self.handshake.hello(connection_id, &record),
            };
            match step {
                HandshakeStep::Continue => Handled::Continue,
                HandshakeStep::Fail(failure) => Handled::Closed(Some(failure)),
                HandshakeStep::Pairing(claim) => Handled::Pairing(claim),
                HandshakeStep::Known(claim) => Handled::Known(claim),
            }
        }

        fn activate(&mut self, channel: SecureChannel) {
            self.gate.activate(channel);
            self.handshake.activated();
        }

        /// 서버 제어 프레임을 먹인다.
        fn server(&mut self, frame_type: FrameType, connection: [u8; RELAY_ID_BYTES]) -> Handled {
            self.receive(&server_frame(frame_type, connection))
        }

        /// 입장 → 상대 합류까지.
        fn joined(&mut self) {
            let admission = self.outbound();
            assert_eq!(admission.len(), 1, "세션 시작에 입장 프레임 하나");
            let (frame, _) = RelayFrame::decode(&admission[0]).unwrap();
            assert_eq!(frame.frame_type(), FrameType::DesktopAdmission);
            assert_eq!(frame.payload(), &ADMISSION);
            assert!(matches!(
                self.server(FrameType::Admitted, UNBOUND_CONNECTION),
                Handled::Continue
            ));
            assert!(matches!(
                self.server(FrameType::PeerJoined, CONNECTION),
                Handled::Continue
            ));
        }
    }

    // ---------------------------------------------------------------- 기기 쪽 시뮬레이터

    /// 브라우저 셸이 할 일을 Task 1 원시 요소로 그대로 수행한다.
    struct Device {
        pending: Option<PendingHandshake>,
        authenticated: Option<AuthenticatedHandshake>,
        sequence: u64,
        connection: ConnectionId,
        identity_public_sec1: [u8; 65],
        fingerprint: [u8; 32],
    }

    impl Device {
        fn new() -> Self {
            Self::with_identity(DEVICE_IDENTITY_SCALAR)
        }

        fn with_identity(scalar: [u8; 32]) -> Self {
            let connection = ConnectionId::from_bytes(CONNECTION);
            // 기기는 Mac의 신원 공개키를 페어링 링크(또는 저장소)에서 안다.
            let desktop_identity = RelayIdentity::from_private_scalar(DESKTOP_IDENTITY_SCALAR)
                .unwrap()
                .public_key_sec1()
                .to_vec();
            let identity = RelayIdentity::from_private_scalar(scalar).unwrap();
            let identity_public_sec1 = *identity.public_key_sec1();
            let fingerprint = identity.fingerprint();
            let pending = PendingHandshake::begin_with_ephemeral_for_test(
                identity,
                desktop_identity,
                RelayRole::Device,
                RELAY_PROTOCOL_VERSION,
                connection,
                DEVICE_EPHEMERAL_SCALAR,
            )
            .unwrap();
            Self {
                pending: Some(pending),
                authenticated: None,
                sequence: 0,
                connection,
                identity_public_sec1,
                fingerprint,
            }
        }

        fn identity_public_sec1(&self) -> [u8; 65] {
            self.identity_public_sec1
        }

        fn fingerprint(&self) -> [u8; 32] {
            self.fingerprint
        }

        /// 첫 레코드: 서명 없는 offer.
        fn offer_record(&self) -> Vec<u8> {
            encode_offer(self.pending.as_ref().unwrap().offer()).to_vec()
        }

        /// Mac의 서명된 hello를 받아 검증하고, 자기 서명된 hello를 만든다.
        fn answer(&mut self, desktop_hello_frame: &[u8]) -> Vec<u8> {
            let (frame, _) = RelayFrame::decode(desktop_hello_frame).unwrap();
            assert_eq!(frame.frame_type(), FrameType::Hello);
            let HelloRecord::Identity(desktop_hello) =
                decode_hello_record(frame.payload()).unwrap()
            else {
                panic!("Mac은 서명된 hello로 답해야 한다");
            };
            assert_eq!(desktop_hello.role(), RelayRole::Desktop);
            let pending = self.pending.take().unwrap();
            let own_hello = pending.sign_peer_offer(desktop_hello.offer()).unwrap();
            // Mac의 서명이 이 기기의 offer를 덮지 않으면(다른 기기 행세) 여기서 실패한다 —
            // 그 경우도 기기 hello는 만들어져 Mac 쪽 거절을 시험한다.
            self.authenticated = pending.finish(desktop_hello).ok();
            encode_identity_hello(&own_hello).to_vec()
        }

        fn binding(&self) -> PairingBinding {
            let authenticated = self.authenticated.as_ref().unwrap();
            // Mac이 계산하는 바인딩: 연결 id, **기기** 지문, transcript 해시.
            PairingBinding::new(
                self.connection,
                self.fingerprint(),
                authenticated.pairing_binding().transcript_hash(),
            )
        }

        fn frame(&mut self, frame_type: FrameType, payload: &[u8]) -> Vec<u8> {
            let frame = RelayFrame::new(
                frame_type,
                ROUTE,
                relay_protocol::ConnectionId::from_bytes(*self.connection.as_bytes()),
                self.sequence,
                payload,
            )
            .unwrap();
            self.sequence = self.sequence.saturating_add(1);
            frame.to_vec()
        }

        fn hello_frame(&mut self, record: &[u8]) -> Vec<u8> {
            self.frame(FrameType::Hello, record)
        }

        /// 활성화된 기기 채널로 봉인한 암호문 프레임.
        fn sealed(&mut self, plaintext: &[u8]) -> Vec<u8> {
            let channel = self
                .authenticated
                .take()
                .expect("기기 핸드셰이크")
                .confirm_device_for_test();
            let mut channel = channel;
            let envelope = channel.seal(plaintext).unwrap();
            let sequence = envelope.header().sequence;
            let frame = RelayFrame::new(
                FrameType::Ciphertext,
                ROUTE,
                relay_protocol::ConnectionId::from_bytes(*self.connection.as_bytes()),
                sequence,
                envelope.ciphertext_and_tag(),
            )
            .unwrap();
            frame.to_vec()
        }
    }

    /// 서버가 보내는 제어 프레임.
    fn server_frame(frame_type: FrameType, connection: [u8; RELAY_ID_BYTES]) -> Vec<u8> {
        RelayFrame::new(
            frame_type,
            ROUTE,
            relay_protocol::ConnectionId::from_bytes(connection),
            0,
            &[],
        )
        .unwrap()
        .to_vec()
    }

    /// 입장·합류·offer·양쪽 hello까지 진행시킨다. 다음은 기기의 소유 주장이다.
    fn authenticated_pair() -> (Desktop, Device) {
        let mut desktop = Desktop::new();
        let mut device = Device::new();
        desktop.joined();

        let offer = device.hello_frame(&device.offer_record());
        assert!(matches!(desktop.receive(&offer), Handled::Continue));
        let outbound = desktop.outbound();
        assert_eq!(outbound.len(), 1, "offer에는 Mac hello 하나로 답한다");
        let device_hello = device.answer(&outbound[0]);
        let device_hello = device.hello_frame(&device_hello);
        assert!(matches!(desktop.receive(&device_hello), Handled::Continue));
        assert!(
            desktop.outbound().is_empty(),
            "기기 hello 검증 뒤에는 Mac이 더 보낼 것이 없다"
        );
        (desktop, device)
    }

    fn known_record(identity_public_sec1: [u8; 65], revoked_at: Option<u64>) -> RelayDeviceRecord {
        RelayDeviceRecord::new(
            DeviceId::from_bytes(DEVICE_ID),
            identity_public_sec1,
            "phone".to_owned(),
            RelayPermissions::default(),
            NOW - 100,
            NOW + 86_400,
            None,
            revoked_at,
        )
        .unwrap()
    }

    // ---------------------------------------------------------------- 성공 경로

    #[test]
    fn a_new_pairing_reaches_an_active_channel_only_after_a_verified_proof() {
        let (mut desktop, mut device) = authenticated_pair();

        // 조정자(앱)가 발급한 티켓의 비밀을 기기가 페어링 링크로 받았다.
        let mut registry = PairingRegistry::new();
        let issued = registry.issue(NOW).unwrap();
        let mut secret_bytes = *issued.secret().expose();
        let device_secret = PairingSecret::take_from_bytes(&mut secret_bytes);
        let binding = device.binding();
        let proof = pairing_proof(&device_secret, &binding);
        let record = encode_pairing_proof(issued.id(), &proof);
        let claim = match desktop.receive(&device.hello_frame(&record)) {
            Handled::Pairing(claim) => claim,
            other => panic!("페어링 주장이어야 한다: {other:?}"),
        };
        assert_eq!(claim.pairing_id, issued.id());
        assert_eq!(
            claim.peer.identity_public_sec1,
            device.identity_public_sec1()
        );
        assert!(desktop.handshake.is_proposed());

        // 조정자: 증명 검증 → 소비 → 확정 → 게이트 활성화. (앱은 여기서 사용자 승인을 끼운다.)
        let mac_binding = claim.peer.handshake.pairing_binding();
        assert_eq!(mac_binding, binding, "양쪽이 같은 바인딩을 계산한다");
        registry
            .verify_proof_for_binding(claim.pairing_id, &claim.proof, NOW, mac_binding)
            .unwrap();
        let approval = registry.consume(claim.pairing_id, NOW).unwrap();
        let channel = claim.peer.handshake.confirm(approval).unwrap();
        desktop.activate(channel);
        assert!(desktop.handshake.is_active());

        // 이제야 암호문이 평문이 된다.
        let sealed = device.sealed(br#"{"type":"request_keyframe"}"#);
        match desktop.receive(&sealed) {
            Handled::Plaintext(plaintext) => {
                assert_eq!(plaintext, br#"{"type":"request_keyframe"}"#);
            }
            other => panic!("활성 채널은 평문을 낸다: {other:?}"),
        }
        assert_eq!(
            desktop.identity_calls.load(Ordering::SeqCst),
            1,
            "세션당 신원을 정확히 한 번 가져온다"
        );
    }

    #[test]
    fn a_known_device_activates_without_a_ticket_when_the_stored_record_admits_it() {
        let (mut desktop, mut device) = authenticated_pair();
        let record = encode_known_device(DeviceId::from_bytes(DEVICE_ID));
        let claim = match desktop.receive(&device.hello_frame(&record)) {
            Handled::Known(claim) => claim,
            other => panic!("기존 기기 주장이어야 한다: {other:?}"),
        };
        assert_eq!(claim.device_id, DeviceId::from_bytes(DEVICE_ID));

        let stored = known_record(device.identity_public_sec1(), None);
        let channel = claim
            .peer
            .handshake
            .confirm_admitted(&stored, claim.peer.observed_at)
            .unwrap();
        desktop.activate(channel);
        let sealed = device.sealed(b"hello");
        assert!(matches!(desktop.receive(&sealed), Handled::Plaintext(p) if p == b"hello"));
    }

    // ---------------------------------------------------------------- 실패 경로

    #[test]
    fn a_revoked_or_foreign_stored_record_never_activates() {
        let (mut desktop, mut device) = authenticated_pair();
        let record = encode_known_device(DeviceId::from_bytes(DEVICE_ID));
        let Handled::Known(claim) = desktop.receive(&device.hello_frame(&record)) else {
            panic!("기존 기기 주장");
        };
        // 취소된 기록.
        let revoked = known_record(device.identity_public_sec1(), Some(NOW - 1));
        assert!(
            claim
                .peer
                .handshake
                .confirm_admitted(&revoked, claim.peer.observed_at)
                .is_err()
        );

        let (mut desktop2, mut device2) = authenticated_pair();
        let Handled::Known(claim2) = desktop2.receive(&device2.hello_frame(&record)) else {
            panic!("기존 기기 주장");
        };
        // 다른 기기의 키가 든 기록.
        let rogue = *RelayIdentity::from_private_scalar(ROGUE_IDENTITY_SCALAR)
            .unwrap()
            .public_key_sec1();
        assert!(
            claim2
                .peer
                .handshake
                .confirm_admitted(&known_record(rogue, None), claim2.peer.observed_at)
                .is_err()
        );
        let _ = (&mut desktop, &mut device2);
    }

    #[test]
    fn a_wrong_pairing_proof_is_rejected_by_the_registry() {
        let (mut desktop, mut device) = authenticated_pair();
        let mut registry = PairingRegistry::new();
        let issued = registry.issue(NOW).unwrap();
        let mut wrong = [0x5a; 32];
        let wrong = PairingSecret::take_from_bytes(&mut wrong);
        let proof = pairing_proof(&wrong, &device.binding());
        let Handled::Pairing(claim) =
            desktop.receive(&device.hello_frame(&encode_pairing_proof(issued.id(), &proof)))
        else {
            panic!("페어링 주장");
        };
        assert!(
            registry
                .verify_proof_for_binding(
                    claim.pairing_id,
                    &claim.proof,
                    NOW,
                    claim.peer.handshake.pairing_binding()
                )
                .is_err()
        );
        desktop.handshake.rejected();
        assert!(!desktop.handshake.is_proposed());
        assert!(!desktop.gate.is_active());
    }

    #[test]
    fn a_signed_hello_before_an_offer_is_out_of_order() {
        let mut desktop = Desktop::new();
        let mut device = Device::new();
        desktop.joined();
        // 기기가 offer 없이 (고정 스칼라로 미리 계산한) 서명된 hello를 먼저 낸다.
        let other = Device::new();
        let desktop_pending = PendingHandshake::begin_with_ephemeral_for_test(
            RelayIdentity::from_private_scalar(DESKTOP_IDENTITY_SCALAR).unwrap(),
            other.identity_public_sec1().to_vec(),
            RelayRole::Desktop,
            RELAY_PROTOCOL_VERSION,
            ConnectionId::from_bytes(CONNECTION),
            [0x44; 32],
        )
        .unwrap();
        let premature = device
            .pending
            .as_ref()
            .unwrap()
            .sign_peer_offer(desktop_pending.offer())
            .unwrap();
        let frame = device.hello_frame(&encode_identity_hello(&premature));
        assert!(matches!(
            desktop.receive(&frame),
            Handled::Closed(Some(HandshakeFailure::UnexpectedRecord))
        ));
        assert!(desktop.outbound().is_empty(), "Mac의 신원은 나가지 않는다");
    }

    #[test]
    fn an_offer_replay_after_the_desktop_answered_ends_the_session() {
        let mut desktop = Desktop::new();
        let mut device = Device::new();
        desktop.joined();
        let offer = device.offer_record();
        assert!(matches!(
            desktop.receive(&device.hello_frame(&offer)),
            Handled::Continue
        ));
        let _ = desktop.outbound();
        assert!(matches!(
            desktop.receive(&device.hello_frame(&offer)),
            Handled::Closed(Some(HandshakeFailure::UnexpectedRecord))
        ));
    }

    #[test]
    fn a_device_hello_with_a_different_identity_than_its_offer_is_unauthenticated() {
        let mut desktop = Desktop::new();
        let mut device = Device::new();
        desktop.joined();
        assert!(matches!(
            desktop.receive(&device.hello_frame(&device.offer_record())),
            Handled::Continue
        ));
        let mac_hello = desktop.outbound().remove(0);
        // offer는 기기 A가 냈는데, 서명된 hello는 다른 신원(rogue)이 만든다.
        let mut rogue = Device::with_identity(ROGUE_IDENTITY_SCALAR);
        let rogue_hello = rogue.answer(&mac_hello);
        let frame = device.hello_frame(&rogue_hello);
        assert!(matches!(
            desktop.receive(&frame),
            Handled::Closed(Some(HandshakeFailure::Unauthenticated))
        ));
    }

    #[test]
    fn a_tampered_signature_is_unauthenticated() {
        let mut desktop = Desktop::new();
        let mut device = Device::new();
        desktop.joined();
        assert!(matches!(
            desktop.receive(&device.hello_frame(&device.offer_record())),
            Handled::Continue
        ));
        let mac_hello = desktop.outbound().remove(0);
        let mut hello = device.answer(&mac_hello);
        let last = hello.len() - 1;
        hello[last] ^= 0x01;
        let frame = device.hello_frame(&hello);
        assert!(matches!(
            desktop.receive(&frame),
            Handled::Closed(Some(HandshakeFailure::Unauthenticated))
                | Handled::Closed(Some(HandshakeFailure::MalformedRecord))
        ));
    }

    #[test]
    fn a_frame_from_another_connection_is_rejected() {
        let mut desktop = Desktop::new();
        let device = Device::new();
        desktop.joined();
        let offer = device.offer_record();
        let frame = RelayFrame::new(
            FrameType::Hello,
            ROUTE,
            relay_protocol::ConnectionId::from_bytes(OTHER_CONNECTION),
            0,
            &offer,
        )
        .unwrap()
        .to_vec();
        assert!(matches!(
            desktop.receive(&frame),
            Handled::Closed(Some(HandshakeFailure::WrongConnection))
        ));
    }

    #[test]
    fn a_claim_before_authentication_and_a_malformed_record_are_fatal() {
        let mut desktop = Desktop::new();
        let mut device = Device::new();
        desktop.joined();
        let claim = encode_known_device(DeviceId::from_bytes(DEVICE_ID));
        assert!(matches!(
            desktop.receive(&device.hello_frame(&claim)),
            Handled::Closed(Some(HandshakeFailure::UnexpectedRecord))
        ));

        let mut desktop = Desktop::new();
        let mut device = Device::new();
        desktop.joined();
        let mut garbage = device.offer_record();
        garbage.truncate(OFFER_RECORD_BYTES - 1);
        assert!(matches!(
            desktop.receive(&device.hello_frame(&garbage)),
            Handled::Closed(Some(HandshakeFailure::MalformedRecord))
        ));
    }

    #[test]
    fn hello_before_admission_or_before_a_peer_joins_is_fatal() {
        let mut desktop = Desktop::new();
        let mut device = Device::new();
        let _ = desktop.outbound();
        assert!(matches!(
            desktop.receive(&device.hello_frame(&device.offer_record())),
            Handled::Closed(Some(HandshakeFailure::NotAdmitted))
        ));

        let mut desktop = Desktop::new();
        let mut device = Device::new();
        let _ = desktop.outbound();
        desktop.server(FrameType::Admitted, UNBOUND_CONNECTION);
        assert!(matches!(
            desktop.receive(&device.hello_frame(&device.offer_record())),
            Handled::Closed(Some(HandshakeFailure::NotJoined))
        ));
    }

    #[test]
    fn the_handshake_deadline_ends_a_silent_session_but_not_an_approval_wait() {
        let mut desktop = Desktop::new();
        let mut device = Device::new();
        desktop.joined();
        desktop.advance(HANDSHAKE_DEADLINE_SECS);
        assert!(matches!(
            desktop.receive(&device.hello_frame(&device.offer_record())),
            Handled::Closed(Some(HandshakeFailure::DeadlineExceeded))
        ));

        // 승인 대기는 핸드셰이크 시한에 묶이지 않는다 — 5분 페어링 마감이 따로 있다.
        let (mut desktop, mut device) = authenticated_pair();
        let record = encode_known_device(DeviceId::from_bytes(DEVICE_ID));
        assert!(matches!(
            desktop.receive(&device.hello_frame(&record)),
            Handled::Known(_)
        ));
        desktop.advance(HANDSHAKE_DEADLINE_SECS * 4);
        assert!(desktop.handshake.is_proposed());
        // 하지만 그 사이에 오는 hello는 순서 위반이다.
        assert!(matches!(
            desktop.receive(&device.hello_frame(&record)),
            Handled::Closed(Some(HandshakeFailure::UnexpectedRecord))
        ));
    }

    #[test]
    fn a_second_peer_or_a_peer_leaving_ends_the_session() {
        let mut desktop = Desktop::new();
        desktop.joined();
        assert!(matches!(
            desktop.server(FrameType::PeerJoined, OTHER_CONNECTION),
            Handled::Closed(Some(HandshakeFailure::UnexpectedRecord))
        ));

        let mut desktop = Desktop::new();
        desktop.joined();
        assert!(matches!(
            desktop.server(FrameType::PeerLeft, CONNECTION),
            Handled::Closed(Some(HandshakeFailure::PeerLeft))
        ));
    }

    #[test]
    fn an_unavailable_identity_never_leaks_an_offer_answer() {
        let mut desktop = Desktop::new();
        desktop.handshake = RelayHandshake::new(
            ROUTE,
            AdmissionCredential::from_bytes(ADMISSION),
            Box::new(|| anyhow::bail!("keychain denied")),
            Box::new(|| NOW),
        );
        desktop.handshake.session_started();
        let mut device = Device::new();
        desktop.joined();
        assert!(matches!(
            desktop.receive(&device.hello_frame(&device.offer_record())),
            Handled::Closed(Some(HandshakeFailure::IdentityUnavailable))
        ));
        assert!(desktop.outbound().is_empty());
    }

    #[test]
    fn a_new_session_starts_from_scratch() {
        let (mut desktop, _device) = authenticated_pair();
        desktop.handshake.session_ended();
        desktop.handshake.session_started();
        let admission = desktop.outbound();
        assert_eq!(admission.len(), 1);
        let (frame, _) = RelayFrame::decode(&admission[0]).unwrap();
        assert_eq!(frame.sequence(), 0, "시퀀스도 처음부터다");
        assert!(!desktop.handshake.is_active());
    }

    // ---------------------------------------------------------------- 티켓

    fn frame_types(frames: &[Vec<u8>]) -> Vec<FrameType> {
        frames
            .iter()
            .map(|raw| RelayFrame::decode(raw).unwrap().0.frame_type())
            .collect()
    }

    #[test]
    fn a_ticket_is_published_after_admission_and_again_on_every_new_session() {
        let handle = AdmissionCredential::from_bytes([0xc3; 32]);
        let mut desktop = Desktop::new();
        // 입장 허가 전에 게시를 요청하면 허가 직후에 나간다.
        desktop.handshake.publish_ticket(handle);
        assert_eq!(
            frame_types(&desktop.outbound()),
            [FrameType::DesktopAdmission],
            "허가 전에는 티켓이 나가지 않는다"
        );
        desktop.server(FrameType::Admitted, UNBOUND_CONNECTION);
        let published = desktop.outbound();
        assert_eq!(frame_types(&published), [FrameType::TicketPublish]);
        let (frame, _) = RelayFrame::decode(&published[0]).unwrap();
        assert!(frame.admission_credential().unwrap().matches(&handle));
        // 같은 세션에서는 다시 나가지 않는다.
        desktop.server(FrameType::Admitted, UNBOUND_CONNECTION);
        assert!(desktop.outbound().is_empty());

        // 세션이 바뀌면 라우트가 새로 생기므로 다시 게시한다.
        desktop.handshake.session_ended();
        desktop.handshake.session_started();
        desktop.server(FrameType::Admitted, UNBOUND_CONNECTION);
        assert_eq!(
            frame_types(&desktop.outbound()),
            [FrameType::DesktopAdmission, FrameType::TicketPublish]
        );

        // 회수는 게시된 티켓에만 나간다.
        desktop.handshake.revoke_ticket();
        assert_eq!(frame_types(&desktop.outbound()), [FrameType::TicketRevoke]);
        desktop.handshake.revoke_ticket();
        assert!(desktop.outbound().is_empty());
        // 회수된 티켓은 다음 세션에 되살아나지 않는다.
        desktop.handshake.session_ended();
        desktop.handshake.session_started();
        desktop.server(FrameType::Admitted, UNBOUND_CONNECTION);
        assert_eq!(
            frame_types(&desktop.outbound()),
            [FrameType::DesktopAdmission]
        );
    }

    #[test]
    fn revoking_a_ticket_that_was_never_published_sends_nothing() {
        let mut desktop = Desktop::new();
        let _ = desktop.outbound();
        desktop
            .handshake
            .publish_ticket(AdmissionCredential::from_bytes([1; 32]));
        desktop.handshake.revoke_ticket();
        desktop.server(FrameType::Admitted, UNBOUND_CONNECTION);
        assert!(
            desktop.outbound().is_empty(),
            "게시되지 않은 티켓은 회수할 것도 없다"
        );
    }

    // ---------------------------------------------------------------- 주기 tick

    /// 프레임이 하나도 오지 않아도 시간만으로 판정돼야 하는 것들.
    #[test]
    fn the_idle_tick_keeps_the_session_alive_and_still_enforces_the_deadline() {
        let mut desktop = Desktop::new();
        desktop.joined();
        let _ = desktop.outbound();

        // 주기 전에는 아무것도 나가지 않는다.
        assert!(matches!(desktop.handshake.tick(), HandshakeStep::Continue));
        assert!(desktop.outbound().is_empty());

        // 주기가 지나면 빈 생존 신호가 이 세션의 연결 id로 나간다.
        desktop.advance(HEARTBEAT_INTERVAL_SECS);
        assert!(matches!(desktop.handshake.tick(), HandshakeStep::Continue));
        let beats = desktop.outbound();
        assert_eq!(frame_types(&beats), [FrameType::Heartbeat]);
        let (frame, _) = RelayFrame::decode(&beats[0]).unwrap();
        assert!(frame.payload().is_empty());
        assert_eq!(frame.connection_id().as_bytes(), &CONNECTION);
        assert_eq!(frame.route_id(), ROUTE);

        // 생존 신호가 마감을 미루지는 않는다 — 침묵하는 상대는 시한에서 끝난다.
        desktop.advance(HANDSHAKE_DEADLINE_SECS);
        assert!(matches!(
            desktop.handshake.tick(),
            HandshakeStep::Fail(HandshakeFailure::DeadlineExceeded)
        ));
    }

    /// 거절은 세션을 끝내되, **이미 큐에 든 티켓 회수는 나간 뒤**여야 한다.
    #[test]
    fn a_rejection_ends_the_session_without_swallowing_a_queued_ticket_revoke() {
        let (mut desktop, mut device) = authenticated_pair();
        desktop
            .handshake
            .publish_ticket(AdmissionCredential::from_bytes([0xc3; 32]));
        let _ = desktop.outbound();
        let record = encode_known_device(DeviceId::from_bytes(DEVICE_ID));
        assert!(matches!(
            desktop.receive(&device.hello_frame(&record)),
            Handled::Known(_)
        ));

        // 조정자가 같은 창에서 회수와 거절을 낸다(앱의 drain 순서 그대로).
        desktop.handshake.revoke_ticket();
        desktop.handshake.rejected();
        assert_eq!(
            frame_types(&desktop.outbound()),
            [FrameType::TicketRevoke],
            "거절이 회수 프레임을 삼키면 1회용 티켓이 서버에 남는다"
        );
        assert!(matches!(
            desktop.handshake.tick(),
            HandshakeStep::Fail(HandshakeFailure::Rejected)
        ));
    }

    // ---------------------------------------------------------------- 레코드 모양

    #[test]
    fn every_record_has_exactly_one_shape_and_the_gate_never_parses_them() {
        let device = Device::new();
        let offer = device.offer_record();
        assert_eq!(offer.len(), OFFER_RECORD_BYTES);
        assert_eq!(offer[0], HELLO_TAG_OFFER);
        assert!(matches!(
            decode_hello_record(&offer),
            Ok(HelloRecord::Offer(_))
        ));
        assert!(matches!(
            decode_hello_record(&offer[..offer.len() - 1]),
            Err(HelloDecodeError::WrongLength { .. })
        ));
        assert_eq!(decode_hello_record(&[]), Err(HelloDecodeError::Empty));
        assert_eq!(
            decode_hello_record(&[0x09, 1, 2]),
            Err(HelloDecodeError::UnknownTag(0x09))
        );
        let known = encode_known_device(DeviceId::from_bytes(DEVICE_ID));
        assert_eq!(known.len(), KNOWN_DEVICE_RECORD_BYTES);
        let proof = encode_pairing_proof(PairingId::from_bytes([1; 16]), &[2; 32]);
        assert_eq!(proof.len(), PAIRING_PROOF_RECORD_BYTES);
        // 가장 큰 레코드도 512바이트 상한 안이다 — 계약이 깨지면 컴파일이 멈춘다.
        const { assert!(IDENTITY_HELLO_BYTES <= relay_protocol::MAX_HELLO_BYTES) };

        let gate = include_str!("session.rs")
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .unwrap();
        assert!(
            !gate.contains("decode_hello_record"),
            "게이트는 hello를 해석하지 않는다 — 해석은 이 상태 기계만 한다"
        );
    }

    // ---------------------------------------------------------------- 고정 벡터

    /// `tests/fixtures/relay-hello-v1.json`은 Rust와 독립적으로 쓴 인코더가 만든 벡터다 —
    /// 코드가 아니라 명세를 고정한다. 브라우저 셸은 같은 파일로 자기 인코더를 검증한다.
    #[test]
    fn the_v1_hello_fixture_matches_this_implementation() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/relay-hello-v1.json")).unwrap();
        fn hex(value: &serde_json::Value) -> Vec<u8> {
            let text = value.as_str().unwrap();
            (0..text.len())
                .step_by(2)
                .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
                .collect()
        }
        fn fixed<const N: usize>(bytes: &[u8]) -> [u8; N] {
            bytes.try_into().unwrap()
        }
        let desktop = &fixture["desktop"];
        let device = &fixture["device"];
        assert_eq!(fixture["protocol_version"], RELAY_PROTOCOL_VERSION);
        assert_eq!(fixture["record_bytes"]["offer"], OFFER_RECORD_BYTES);
        assert_eq!(
            fixture["record_bytes"]["identity_hello"],
            IDENTITY_HELLO_BYTES
        );
        assert_eq!(
            fixture["record_bytes"]["pairing_proof"],
            PAIRING_PROOF_RECORD_BYTES
        );
        assert_eq!(
            fixture["record_bytes"]["known_device"],
            KNOWN_DEVICE_RECORD_BYTES
        );
        assert_eq!(fixture["tags"]["identity_hello"], HELLO_TAG_IDENTITY);
        assert_eq!(fixture["tags"]["pairing_proof"], HELLO_TAG_PAIRING_PROOF);
        assert_eq!(fixture["tags"]["known_device"], HELLO_TAG_KNOWN_DEVICE);
        assert_eq!(fixture["roles"]["desktop"], ROLE_DESKTOP);
        assert_eq!(fixture["roles"]["device"], ROLE_DEVICE);

        let connection = ConnectionId::from_bytes(fixed(&hex(&fixture["connection_id_hex"])));
        let desktop_identity = RelayIdentity::from_private_scalar(fixed(&hex(
            &desktop["identity_private_scalar_hex"],
        )))
        .unwrap();
        let device_identity =
            RelayIdentity::from_private_scalar(fixed(&hex(&device["identity_private_scalar_hex"])))
                .unwrap();
        assert_eq!(
            desktop_identity.public_key_sec1().to_vec(),
            hex(&desktop["identity_public_sec1_hex"])
        );
        assert_eq!(
            device_identity.fingerprint().to_vec(),
            hex(&device["identity_fingerprint_hex"])
        );
        let device_fingerprint = device_identity.fingerprint();
        let desktop_public = desktop_identity.public_key_sec1().to_vec();
        let device_public = device_identity.public_key_sec1().to_vec();
        let desktop_pending = PendingHandshake::begin_with_ephemeral_for_test(
            desktop_identity,
            device_public,
            RelayRole::Desktop,
            RELAY_PROTOCOL_VERSION,
            connection,
            fixed(&hex(&desktop["ephemeral_private_scalar_hex"])),
        )
        .unwrap();
        let device_pending = PendingHandshake::begin_with_ephemeral_for_test(
            device_identity,
            desktop_public,
            RelayRole::Device,
            RELAY_PROTOCOL_VERSION,
            connection,
            fixed(&hex(&device["ephemeral_private_scalar_hex"])),
        )
        .unwrap();
        assert_eq!(
            encode_offer(desktop_pending.offer()).to_vec(),
            hex(&desktop["offer_record_hex"])
        );
        assert_eq!(
            encode_offer(device_pending.offer()).to_vec(),
            hex(&device["offer_record_hex"])
        );

        // 서명은 RFC 6979 결정적이므로 바이트까지 같다.
        let desktop_hello = desktop_pending
            .sign_peer_offer(device_pending.offer())
            .unwrap();
        let device_hello = device_pending
            .sign_peer_offer(desktop_pending.offer())
            .unwrap();
        assert_eq!(
            encode_identity_hello(&desktop_hello).to_vec(),
            hex(&desktop["identity_hello_record_hex"])
        );
        assert_eq!(
            encode_identity_hello(&device_hello).to_vec(),
            hex(&device["identity_hello_record_hex"])
        );

        // fixture의 레코드가 그대로 상대를 인증시킨다.
        let HelloRecord::Identity(fixture_desktop_hello) =
            decode_hello_record(&hex(&desktop["identity_hello_record_hex"])).unwrap()
        else {
            panic!("desktop identity hello");
        };
        let HelloRecord::Identity(fixture_device_hello) =
            decode_hello_record(&hex(&device["identity_hello_record_hex"])).unwrap()
        else {
            panic!("device identity hello");
        };
        let desktop_auth = desktop_pending.finish(fixture_device_hello).unwrap();
        let device_auth = device_pending.finish(fixture_desktop_hello).unwrap();
        assert_eq!(
            desktop_auth.confirmation_code(),
            fixture["confirmation_code"].as_str().unwrap()
        );
        assert_eq!(
            device_auth.confirmation_code(),
            desktop_auth.confirmation_code()
        );
        let transcript_hash: [u8; 32] = fixed(&hex(&fixture["transcript_hash_hex"]));
        assert_eq!(
            desktop_auth.pairing_binding().transcript_hash(),
            transcript_hash
        );

        // 소유 증명: HMAC-SHA256(secret, transcript_hash || connection || device_fingerprint).
        let proof_fixture = &fixture["pairing_proof"];
        let mut message = transcript_hash.to_vec();
        message.extend_from_slice(connection.as_bytes());
        message.extend_from_slice(&device_fingerprint);
        assert_eq!(message, hex(&proof_fixture["message_hex"]));
        let mut secret_bytes: [u8; 32] = fixed(&hex(&proof_fixture["secret_hex"]));
        let secret = PairingSecret::take_from_bytes(&mut secret_bytes);
        let binding = PairingBinding::new(connection, device_fingerprint, transcript_hash);
        assert_eq!(binding, desktop_auth.pairing_binding());
        let proof = pairing_proof(&secret, &binding);
        assert_eq!(proof.to_vec(), hex(&proof_fixture["proof_hex"]));
        let pairing_id = PairingId::from_bytes(fixed(&hex(&proof_fixture["pairing_id_hex"])));
        assert_eq!(
            encode_pairing_proof(pairing_id, &proof).to_vec(),
            hex(&proof_fixture["record_hex"])
        );
        assert_eq!(
            decode_hello_record(&hex(&proof_fixture["record_hex"])).unwrap(),
            HelloRecord::PairingProof { pairing_id, proof }
        );

        let known = &fixture["known_device"];
        let device_id = DeviceId::from_bytes(fixed(&hex(&known["device_id_hex"])));
        assert_eq!(
            encode_known_device(device_id).to_vec(),
            hex(&known["record_hex"])
        );
        assert_eq!(
            decode_hello_record(&hex(&known["record_hex"])).unwrap(),
            HelloRecord::KnownDevice { device_id }
        );

        for reject in fixture["reject"].as_array().unwrap() {
            let record = hex(&reject["record_hex"]);
            assert!(
                decode_hello_record(&record).is_err(),
                "{} must be rejected",
                reject["name"]
            );
        }
    }
}
