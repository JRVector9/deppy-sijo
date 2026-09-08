//! Deppy Relay 데이터 평면 와이어 계약 (`DRLY` v1).
//!
//! Relay는 신뢰하지 않는 중계자다. 이 계약이 Relay에게 허용하는 지식은 프로토콜 버전,
//! 불투명한 rendezvous/기기 라우팅 핸들, 연결 id, 시퀀스, 페이로드 길이, 생존 신호뿐이다.
//! 기기 표시 이름·권한·세션 id·워크스페이스 이름·터미널 내용·입력·승인 미리보기·업로드
//! 파일명은 어떤 프레임에도 존재하지 않는다.
//!
//! E2EE 이전의 서명된 identity/ephemeral hello는 최대 512바이트 **불투명** 레코드 하나이며
//! Relay는 이를 파싱하거나 로깅하지 않고 바이트 그대로 전달한다. 그러므로 정확한 불변식은
//! "Relay는 애플리케이션 평문을 받지 않는다"이지 "모든 바이트가 암호화돼 있다"가 아니다.
//!
//! 디코딩은 **선점검(preflight) 후 빌림(borrow)** 이다. 헤더를 읽어 종류별 상한을 먼저
//! 확인하고, 상한을 넘는 선언 길이는 그 바이트를 기다리거나 담기 전에 거절한다. 통과한
//! 프레임의 페이로드는 입력 버퍼를 그대로 빌린다 — 복사하지 않는다.

use std::fmt;

pub const MAGIC: [u8; 4] = *b"DRLY";
pub const PROTOCOL_VERSION: u16 = 1;

pub const ROUTE_ID_BYTES: usize = 16;
pub const CONNECTION_ID_BYTES: usize = 16;
pub const ADMISSION_CREDENTIAL_BYTES: usize = 32;
/// 라우트별 재접속 검증자 수와 절대 수명 상한.
pub const MAX_RECONNECT_GRANTS: usize = 64;
pub const MAX_RECONNECT_LIFETIME_SECS: u64 = 30 * 24 * 60 * 60;

const MAGIC_OFFSET: usize = 0;
const VERSION_OFFSET: usize = 4;
const FRAME_TYPE_OFFSET: usize = 6;
const FLAGS_OFFSET: usize = 7;
const ROUTE_OFFSET: usize = 8;
const CONNECTION_OFFSET: usize = ROUTE_OFFSET + ROUTE_ID_BYTES;
const SEQUENCE_OFFSET: usize = CONNECTION_OFFSET + CONNECTION_ID_BYTES;
const LENGTH_OFFSET: usize = SEQUENCE_OFFSET + 8;

/// 고정 52바이트 빅엔디언 헤더.
pub const HEADER_BYTES: usize = LENGTH_OFFSET + 4;

/// 서명된 hello 하나의 상한. 이보다 큰 pre-E2EE 레코드는 존재할 수 없다.
pub const MAX_HELLO_BYTES: usize = 512;
/// AES-256-GCM 레코드 하나의 상한(1 MiB 평문 + 16바이트 태그).
pub const MAX_CIPHERTEXT_BYTES: usize = 1024 * 1024 + 16;
/// 어떤 프레임도 이보다 클 수 없다. 읽기 버퍼 예약의 근거값이다.
pub const MAX_FRAME_BYTES: usize = HEADER_BYTES + MAX_CIPHERTEXT_BYTES;

macro_rules! opaque_id {
    ($name:ident, $bytes:expr) => {
        /// 불투명 식별자. 바이트는 절대 렌더링하지 않는다.
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name([u8; $bytes]);

        impl $name {
            pub const fn from_bytes(bytes: [u8; $bytes]) -> Self {
                Self(bytes)
            }

            pub const fn as_bytes(&self) -> &[u8; $bytes] {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(concat!(stringify!($name), "(REDACTED)"))
            }
        }
    };
}

opaque_id!(RouteId, ROUTE_ID_BYTES);
opaque_id!(ConnectionId, CONNECTION_ID_BYTES);

/// Mac 승인 자격증명과 기기 입장 핸들이 공유하는 불투명 32바이트 값.
///
/// 페어링 비밀이 아니다 — Relay는 페어링 비밀을 절대 보지 않는다. 이 값은 큐/라우트
/// 자원을 할당하기 전 입장만 통제하며, 신원 증명은 그 뒤 종단 간 핸드셰이크가 한다.
#[derive(Clone, Copy)]
pub struct AdmissionCredential([u8; ADMISSION_CREDENTIAL_BYTES]);

impl AdmissionCredential {
    pub const fn from_bytes(bytes: [u8; ADMISSION_CREDENTIAL_BYTES]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; ADMISSION_CREDENTIAL_BYTES] {
        &self.0
    }

    /// 상수 시간 비교. 첫 불일치에서 빠져나오면 자격증명을 바이트 단위로 캐낼 수 있다.
    pub fn matches(&self, other: &Self) -> bool {
        self.0
            .iter()
            .zip(other.0.iter())
            .fold(0u8, |accumulator, (left, right)| {
                accumulator | (left ^ right)
            })
            == 0
    }
}

impl fmt::Debug for AdmissionCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AdmissionCredential(REDACTED)")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FrameType {
    /// 불투명 서명 hello (pre-E2EE, 바이트 그대로 전달).
    Hello = 0x01,
    /// AES-GCM 애플리케이션 레코드.
    Ciphertext = 0x02,
    /// 생존 신호. 페이로드 없음.
    Heartbeat = 0x03,
    /// 종료 코드 하나. 텍스트 사유는 존재하지 않는다.
    Close = 0x04,
    /// Mac이 라우트를 소유하겠다고 제시하는 자격증명.
    DesktopAdmission = 0x10,
    /// 기기가 제시하는 1회용 입장 핸들.
    DeviceAdmission = 0x11,
    /// Mac이 라우트에 입장 핸들 하나를 등록한다.
    TicketPublish = 0x12,
    /// Mac이 등록한 입장 핸들을 즉시 무효화한다.
    TicketRevoke = 0x13,
    /// 재접속 검증자와 절대 만료를 게시한다. 페어링 티켓과 별개다.
    ReconnectPublish = 0x14,
    /// 재접속 검증자를 회수한다.
    ReconnectRevoke = 0x15,
    /// 불투명 재접속 grant로 자원 입장만 요청한다.
    ReconnectAdmission = 0x16,
    /// Mac이 저장된 검증자 복원을 끝냈다.
    ReconnectSync = 0x17,
    /// 검증자 게시 완료.
    ReconnectPublished = 0x24,
    /// 서버 → 클라이언트 입장 허가.
    Admitted = 0x20,
    /// 서버 → 클라이언트 거절 코드 하나.
    Rejected = 0x21,
    /// 라우트의 상대가 붙었다.
    PeerJoined = 0x22,
    /// 라우트의 상대가 떨어졌다.
    PeerLeft = 0x23,
}

impl FrameType {
    const fn from_byte(byte: u8) -> Option<Self> {
        Some(match byte {
            0x01 => Self::Hello,
            0x02 => Self::Ciphertext,
            0x03 => Self::Heartbeat,
            0x04 => Self::Close,
            0x10 => Self::DesktopAdmission,
            0x11 => Self::DeviceAdmission,
            0x12 => Self::TicketPublish,
            0x13 => Self::TicketRevoke,
            0x14 => Self::ReconnectPublish,
            0x15 => Self::ReconnectRevoke,
            0x16 => Self::ReconnectAdmission,
            0x17 => Self::ReconnectSync,
            0x24 => Self::ReconnectPublished,
            0x20 => Self::Admitted,
            0x21 => Self::Rejected,
            0x22 => Self::PeerJoined,
            0x23 => Self::PeerLeft,
            _ => return None,
        })
    }

    /// 종류별 허용 페이로드 길이 범위. 모든 프레임은 정확히 하나의 모양만 가진다.
    const fn payload_bounds(self) -> (usize, usize) {
        match self {
            Self::Hello => (1, MAX_HELLO_BYTES),
            Self::Ciphertext => (1, MAX_CIPHERTEXT_BYTES),
            Self::Heartbeat
            | Self::Admitted
            | Self::PeerJoined
            | Self::PeerLeft
            | Self::ReconnectSync => (0, 0),
            Self::Close | Self::Rejected => (2, 2),
            Self::ReconnectPublish => (40, 40),
            Self::ReconnectRevoke | Self::ReconnectAdmission | Self::ReconnectPublished => (32, 32),
            Self::DesktopAdmission
            | Self::DeviceAdmission
            | Self::TicketPublish
            | Self::TicketRevoke => (ADMISSION_CREDENTIAL_BYTES, ADMISSION_CREDENTIAL_BYTES),
        }
    }
}

/// 종료·거절 사유. 코드 하나이며 사람이 읽는 문자열을 와이어에 싣지 않는다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum RejectionCode {
    MalformedFrame = 0x0001,
    UnsupportedVersion = 0x0002,
    CredentialRejected = 0x0003,
    TicketUnknown = 0x0004,
    TicketConsumed = 0x0005,
    RouteBusy = 0x0006,
    RouteUnknown = 0x0007,
    RateLimited = 0x0008,
    QueueOverflow = 0x0009,
    IdleTimeout = 0x000a,
    CapacityReached = 0x000b,
    PeerDisconnected = 0x000c,
    ShuttingDown = 0x000d,
}

impl RejectionCode {
    pub const fn to_bytes(self) -> [u8; 2] {
        (self as u16).to_be_bytes()
    }

    pub const fn from_bytes(bytes: [u8; 2]) -> Option<Self> {
        Some(match u16::from_be_bytes(bytes) {
            0x0001 => Self::MalformedFrame,
            0x0002 => Self::UnsupportedVersion,
            0x0003 => Self::CredentialRejected,
            0x0004 => Self::TicketUnknown,
            0x0005 => Self::TicketConsumed,
            0x0006 => Self::RouteBusy,
            0x0007 => Self::RouteUnknown,
            0x0008 => Self::RateLimited,
            0x0009 => Self::QueueOverflow,
            0x000a => Self::IdleTimeout,
            0x000b => Self::CapacityReached,
            0x000c => Self::PeerDisconnected,
            0x000d => Self::ShuttingDown,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// 아직 프레임 하나를 완성할 바이트가 모자란다. `needed`는 지금까지 확인된 최소 길이다.
    Incomplete {
        needed: usize,
    },
    BadMagic,
    UnsupportedVersion(u16),
    ReservedFlagsSet,
    UnknownFrameType(u8),
    /// 선언된 길이가 종류별 상한을 넘었다. 그 바이트는 기다리지도, 담지도 않는다.
    PayloadTooLarge {
        declared: usize,
        maximum: usize,
    },
    /// 선언된 길이가 종류별 최소를 밑돈다.
    PayloadTooSmall {
        declared: usize,
        minimum: usize,
    },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Incomplete { needed } => write!(formatter, "relay frame incomplete ({needed})"),
            Self::BadMagic => formatter.write_str("relay frame magic mismatch"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "relay protocol version {version} unsupported")
            }
            Self::ReservedFlagsSet => formatter.write_str("relay frame reserved flags set"),
            Self::UnknownFrameType(byte) => {
                write!(formatter, "relay frame type {byte:#04x} unknown")
            }
            Self::PayloadTooLarge { declared, maximum } => {
                write!(formatter, "relay payload {declared} exceeds {maximum}")
            }
            Self::PayloadTooSmall { declared, minimum } => {
                write!(formatter, "relay payload {declared} below {minimum}")
            }
        }
    }
}

impl std::error::Error for DecodeError {}

impl DecodeError {
    /// 이 오류를 상대에게 알릴 때 쓰는 코드. 어떤 변형도 내부 상태를 문자열로 흘리지 않는다.
    pub const fn rejection_code(&self) -> RejectionCode {
        match self {
            Self::UnsupportedVersion(_) => RejectionCode::UnsupportedVersion,
            _ => RejectionCode::MalformedFrame,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeError {
    pub declared: usize,
    pub minimum: usize,
    pub maximum: usize,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "relay payload {} outside {}..={}",
            self.declared, self.minimum, self.maximum
        )
    }
}

impl std::error::Error for EncodeError {}

/// 한 프레임을 빌려 본 것. 페이로드는 입력 버퍼를 그대로 가리킨다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelayFrame<'a> {
    frame_type: FrameType,
    route_id: RouteId,
    connection_id: ConnectionId,
    sequence: u64,
    payload: &'a [u8],
}

impl<'a> RelayFrame<'a> {
    pub fn new(
        frame_type: FrameType,
        route_id: RouteId,
        connection_id: ConnectionId,
        sequence: u64,
        payload: &'a [u8],
    ) -> Result<Self, EncodeError> {
        let (minimum, maximum) = frame_type.payload_bounds();
        if payload.len() < minimum || payload.len() > maximum {
            return Err(EncodeError {
                declared: payload.len(),
                minimum,
                maximum,
            });
        }
        Ok(Self {
            frame_type,
            route_id,
            connection_id,
            sequence,
            payload,
        })
    }

    pub const fn frame_type(&self) -> FrameType {
        self.frame_type
    }

    pub const fn route_id(&self) -> RouteId {
        self.route_id
    }

    pub const fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub const fn payload(&self) -> &'a [u8] {
        self.payload
    }

    pub const fn encoded_len(&self) -> usize {
        HEADER_BYTES + self.payload.len()
    }

    /// 승인 자격증명/입장 핸들을 실은 프레임이면 그 값을 돌려준다.
    pub fn admission_credential(&self) -> Option<AdmissionCredential> {
        match self.frame_type {
            FrameType::DesktopAdmission
            | FrameType::ReconnectAdmission
            | FrameType::DeviceAdmission
            | FrameType::TicketPublish
            | FrameType::TicketRevoke => Some(AdmissionCredential::from_bytes(
                self.payload.try_into().ok()?,
            )),
            _ => None,
        }
    }

    /// 종료/거절 프레임의 알려진 코드. 모르는 코드는 문자열로 승격하지 않고 `None`이다.
    pub fn rejection_code(&self) -> Option<RejectionCode> {
        match self.frame_type {
            FrameType::Close | FrameType::Rejected => {
                RejectionCode::from_bytes(self.payload.try_into().ok()?)
            }
            _ => None,
        }
    }

    pub fn encode_into(&self, output: &mut Vec<u8>) {
        output.reserve(self.encoded_len());
        output.extend_from_slice(&MAGIC);
        output.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        output.push(self.frame_type as u8);
        output.push(0);
        output.extend_from_slice(self.route_id.as_bytes());
        output.extend_from_slice(self.connection_id.as_bytes());
        output.extend_from_slice(&self.sequence.to_be_bytes());
        output.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        output.extend_from_slice(self.payload);
    }

    pub fn to_vec(&self) -> Vec<u8> {
        let mut output = Vec::with_capacity(self.encoded_len());
        self.encode_into(&mut output);
        output
    }

    /// 한 프레임을 디코딩하고 소비한 바이트 수를 함께 돌려준다.
    ///
    /// 순서가 곧 보안이다: magic → 버전 → 예약 플래그 → 종류 → **선언 길이 상한** →
    /// 그제서야 페이로드 존재 여부. 상한 검사가 버퍼링보다 먼저이므로, 거대한 길이를
    /// 선언한 상대는 그 바이트를 보내기도 전에 끊긴다.
    pub fn decode(input: &'a [u8]) -> Result<(Self, usize), DecodeError> {
        if input.len() < HEADER_BYTES {
            return Err(DecodeError::Incomplete {
                needed: HEADER_BYTES,
            });
        }
        if input[MAGIC_OFFSET..MAGIC_OFFSET + 4] != MAGIC {
            return Err(DecodeError::BadMagic);
        }
        let version = u16::from_be_bytes([input[VERSION_OFFSET], input[VERSION_OFFSET + 1]]);
        if version != PROTOCOL_VERSION {
            return Err(DecodeError::UnsupportedVersion(version));
        }
        let Some(frame_type) = FrameType::from_byte(input[FRAME_TYPE_OFFSET]) else {
            return Err(DecodeError::UnknownFrameType(input[FRAME_TYPE_OFFSET]));
        };
        if input[FLAGS_OFFSET] != 0 {
            return Err(DecodeError::ReservedFlagsSet);
        }

        let declared = u32::from_be_bytes([
            input[LENGTH_OFFSET],
            input[LENGTH_OFFSET + 1],
            input[LENGTH_OFFSET + 2],
            input[LENGTH_OFFSET + 3],
        ]) as usize;
        let (minimum, maximum) = frame_type.payload_bounds();
        if declared > maximum {
            return Err(DecodeError::PayloadTooLarge { declared, maximum });
        }
        if declared < minimum {
            return Err(DecodeError::PayloadTooSmall { declared, minimum });
        }

        let total = HEADER_BYTES + declared;
        if input.len() < total {
            return Err(DecodeError::Incomplete { needed: total });
        }

        let mut route = [0u8; ROUTE_ID_BYTES];
        route.copy_from_slice(&input[ROUTE_OFFSET..ROUTE_OFFSET + ROUTE_ID_BYTES]);
        let mut connection = [0u8; CONNECTION_ID_BYTES];
        connection
            .copy_from_slice(&input[CONNECTION_OFFSET..CONNECTION_OFFSET + CONNECTION_ID_BYTES]);
        let mut sequence = [0u8; 8];
        sequence.copy_from_slice(&input[SEQUENCE_OFFSET..SEQUENCE_OFFSET + 8]);

        Ok((
            Self {
                frame_type,
                route_id: RouteId::from_bytes(route),
                connection_id: ConnectionId::from_bytes(connection),
                sequence: u64::from_be_bytes(sequence),
                payload: &input[HEADER_BYTES..total],
            },
            total,
        ))
    }
}

/// 버퍼 하나를 프레임 열로 훑는다. 첫 오류에서 멈추고 다시 이어가지 않는다 —
/// 어긋난 스트림에서 프레임 경계를 되찾으려는 시도 자체가 공격면이다.
pub struct RelayFrames<'a> {
    remaining: &'a [u8],
    poisoned: bool,
}

impl<'a> RelayFrames<'a> {
    pub const fn new(input: &'a [u8]) -> Self {
        Self {
            remaining: input,
            poisoned: false,
        }
    }

    /// 아직 프레임을 이루지 못한 나머지 바이트.
    pub const fn remainder(&self) -> &'a [u8] {
        self.remaining
    }
}

impl<'a> Iterator for RelayFrames<'a> {
    type Item = Result<RelayFrame<'a>, DecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.poisoned || self.remaining.is_empty() {
            return None;
        }
        match RelayFrame::decode(self.remaining) {
            Ok((frame, consumed)) => {
                self.remaining = &self.remaining[consumed..];
                Some(Ok(frame))
            }
            Err(DecodeError::Incomplete { .. }) => None,
            Err(error) => {
                self.poisoned = true;
                Some(Err(error))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUTE: RouteId = RouteId::from_bytes([0x11; ROUTE_ID_BYTES]);
    const CONNECTION: ConnectionId = ConnectionId::from_bytes([0x22; CONNECTION_ID_BYTES]);

    fn frame(frame_type: FrameType, payload: &[u8]) -> Vec<u8> {
        RelayFrame::new(frame_type, ROUTE, CONNECTION, 7, payload)
            .unwrap()
            .to_vec()
    }

    #[test]
    fn header_layout_is_fixed_big_endian() {
        let encoded = frame(FrameType::Ciphertext, b"ct");
        assert_eq!(HEADER_BYTES, 52);
        assert_eq!(&encoded[0..4], b"DRLY");
        assert_eq!(&encoded[4..6], &1u16.to_be_bytes());
        assert_eq!(encoded[6], FrameType::Ciphertext as u8);
        assert_eq!(encoded[7], 0, "flags are reserved and always zero in v1");
        assert_eq!(&encoded[8..24], &[0x11; ROUTE_ID_BYTES]);
        assert_eq!(&encoded[24..40], &[0x22; CONNECTION_ID_BYTES]);
        assert_eq!(&encoded[40..48], &7u64.to_be_bytes());
        assert_eq!(&encoded[48..52], &2u32.to_be_bytes());
        assert_eq!(&encoded[52..], b"ct");
    }

    #[test]
    fn decoding_borrows_the_payload_and_reports_consumed_bytes() {
        let mut buffer = frame(FrameType::Ciphertext, b"payload");
        buffer.extend_from_slice(b"trailing");
        let (decoded, consumed) = RelayFrame::decode(&buffer).unwrap();
        assert_eq!(consumed, HEADER_BYTES + 7);
        assert_eq!(decoded.payload(), b"payload");
        assert_eq!(decoded.route_id(), ROUTE);
        assert_eq!(decoded.connection_id(), CONNECTION);
        assert_eq!(decoded.sequence(), 7);
        assert_eq!(decoded.frame_type(), FrameType::Ciphertext);
        assert!(
            std::ptr::eq(decoded.payload().as_ptr(), buffer[HEADER_BYTES..].as_ptr()),
            "decoding must borrow, never copy"
        );
    }

    #[test]
    fn a_short_buffer_is_incomplete_not_invalid() {
        let encoded = frame(FrameType::Ciphertext, b"payload");
        for length in 0..encoded.len() {
            let error = RelayFrame::decode(&encoded[..length]).unwrap_err();
            let DecodeError::Incomplete { needed } = error else {
                panic!("a truncated frame is incomplete, not invalid: {error:?}");
            };
            assert!(needed > length && needed <= encoded.len());
        }
        assert!(RelayFrame::decode(&encoded).is_ok());
    }

    #[test]
    fn magic_version_flags_and_frame_type_fail_closed() {
        let good = frame(FrameType::Heartbeat, b"");

        let mut bad_magic = good.clone();
        bad_magic[0] = b'X';
        assert!(matches!(
            RelayFrame::decode(&bad_magic),
            Err(DecodeError::BadMagic)
        ));

        let mut bad_version = good.clone();
        bad_version[4..6].copy_from_slice(&2u16.to_be_bytes());
        assert!(matches!(
            RelayFrame::decode(&bad_version),
            Err(DecodeError::UnsupportedVersion(2))
        ));

        let mut bad_flags = good.clone();
        bad_flags[7] = 1;
        assert!(matches!(
            RelayFrame::decode(&bad_flags),
            Err(DecodeError::ReservedFlagsSet)
        ));

        let mut bad_type = good.clone();
        bad_type[6] = 0x7f;
        assert!(matches!(
            RelayFrame::decode(&bad_type),
            Err(DecodeError::UnknownFrameType(0x7f))
        ));
    }

    /// 선언된 길이가 상한을 넘으면 그 바이트를 기다리거나 담지 않고 즉시 거절한다.
    #[test]
    fn an_oversized_declared_length_is_rejected_before_buffering() {
        let mut header = frame(FrameType::Ciphertext, b"x");
        header.truncate(HEADER_BYTES);
        header[48..52].copy_from_slice(&(MAX_CIPHERTEXT_BYTES as u32 + 1).to_be_bytes());
        let error = RelayFrame::decode(&header).unwrap_err();
        assert!(
            matches!(error, DecodeError::PayloadTooLarge { .. }),
            "{error:?}"
        );

        let mut hello = frame(FrameType::Hello, b"x");
        hello.truncate(HEADER_BYTES);
        hello[48..52].copy_from_slice(&(MAX_HELLO_BYTES as u32 + 1).to_be_bytes());
        assert!(matches!(
            RelayFrame::decode(&hello),
            Err(DecodeError::PayloadTooLarge { .. })
        ));

        let mut absurd = header.clone();
        absurd[48..52].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(
            RelayFrame::decode(&absurd),
            Err(DecodeError::PayloadTooLarge { .. })
        ));
    }

    #[test]
    fn every_frame_type_has_one_fixed_payload_shape() {
        assert_eq!(MAX_HELLO_BYTES, 512);
        assert_eq!(ADMISSION_CREDENTIAL_BYTES, 32);

        for (frame_type, allowed, rejected) in [
            (FrameType::Hello, vec![1usize, MAX_HELLO_BYTES], vec![0]),
            (FrameType::Ciphertext, vec![1], vec![0]),
            (FrameType::Heartbeat, vec![0], vec![1]),
            (FrameType::ReconnectSync, vec![0], vec![1]),
            (FrameType::Close, vec![2], vec![0, 1, 3]),
            (FrameType::Admitted, vec![0], vec![1]),
            (FrameType::Rejected, vec![2], vec![0, 1]),
            (FrameType::PeerJoined, vec![0], vec![1]),
            (FrameType::PeerLeft, vec![0], vec![1]),
            (FrameType::DesktopAdmission, vec![32], vec![0, 31, 33]),
            (FrameType::DeviceAdmission, vec![32], vec![0, 31, 33]),
            (FrameType::TicketPublish, vec![32], vec![31]),
            (FrameType::TicketRevoke, vec![32], vec![31]),
        ] {
            for length in allowed {
                let payload = vec![0x5a; length];
                let encoded = frame(frame_type, &payload);
                let (decoded, _) = RelayFrame::decode(&encoded).unwrap();
                assert_eq!(decoded.payload().len(), length, "{frame_type:?}");
            }
            for length in rejected {
                let payload = vec![0x5a; length];
                assert!(
                    RelayFrame::new(frame_type, ROUTE, CONNECTION, 0, &payload).is_err(),
                    "{frame_type:?} must reject a {length}-byte payload"
                );
            }
        }
    }

    /// 상한을 넘는 페이로드는 인코딩 단계에서 이미 막힌다 — 서버가 그런 프레임을
    /// 만들어 내보낼 방법 자체가 없다.
    #[test]
    fn encoding_refuses_to_build_an_out_of_bounds_frame() {
        assert!(
            RelayFrame::new(
                FrameType::Hello,
                ROUTE,
                CONNECTION,
                0,
                &vec![0u8; MAX_HELLO_BYTES + 1]
            )
            .is_err()
        );
        assert!(
            RelayFrame::new(
                FrameType::Ciphertext,
                ROUTE,
                CONNECTION,
                0,
                &vec![0u8; MAX_CIPHERTEXT_BYTES + 1]
            )
            .is_err()
        );
        assert_eq!(MAX_FRAME_BYTES, HEADER_BYTES + MAX_CIPHERTEXT_BYTES);
    }

    #[test]
    fn close_and_rejection_carry_a_code_never_text() {
        let encoded = frame(
            FrameType::Rejected,
            &RejectionCode::TicketUnknown.to_bytes(),
        );
        let (decoded, _) = RelayFrame::decode(&encoded).unwrap();
        assert_eq!(
            decoded.rejection_code().unwrap(),
            RejectionCode::TicketUnknown
        );
        assert_eq!(decoded.payload().len(), 2);

        let unknown = frame(FrameType::Rejected, &[0xff, 0xff]);
        let (decoded, _) = RelayFrame::decode(&unknown).unwrap();
        assert!(
            decoded.rejection_code().is_none(),
            "an unknown code stays opaque instead of becoming text"
        );
    }

    #[test]
    fn frames_iterate_and_stop_at_the_first_invalid_frame() {
        let mut stream = frame(FrameType::Heartbeat, b"");
        stream.extend_from_slice(&frame(FrameType::Ciphertext, b"one"));
        let mut broken = frame(FrameType::Ciphertext, b"two");
        broken[0] = b'X';
        stream.extend_from_slice(&broken);

        let mut frames = RelayFrames::new(&stream);
        assert_eq!(
            frames.next().unwrap().unwrap().frame_type(),
            FrameType::Heartbeat
        );
        assert_eq!(frames.next().unwrap().unwrap().payload(), b"one");
        assert!(matches!(frames.next().unwrap(), Err(DecodeError::BadMagic)));
        assert!(frames.next().is_none(), "a poisoned stream never resumes");
    }

    #[test]
    fn opaque_ids_never_render_their_bytes() {
        assert_eq!(format!("{ROUTE:?}"), "RouteId(REDACTED)");
        assert_eq!(format!("{CONNECTION:?}"), "ConnectionId(REDACTED)");
        let credential = AdmissionCredential::from_bytes([0x33; ADMISSION_CREDENTIAL_BYTES]);
        assert_eq!(format!("{credential:?}"), "AdmissionCredential(REDACTED)");
        assert!(!format!("{credential:?}").contains("33"));
    }

    /// 두 서로 다른 자격증명 비교는 길이와 무관하게 상수 시간이어야 한다.
    #[test]
    fn admission_credentials_compare_in_constant_time() {
        let left = AdmissionCredential::from_bytes([0x01; ADMISSION_CREDENTIAL_BYTES]);
        let same = AdmissionCredential::from_bytes([0x01; ADMISSION_CREDENTIAL_BYTES]);
        let mut differs_last = [0x01; ADMISSION_CREDENTIAL_BYTES];
        differs_last[ADMISSION_CREDENTIAL_BYTES - 1] = 0x02;
        let mut differs_first = [0x01; ADMISSION_CREDENTIAL_BYTES];
        differs_first[0] = 0x02;

        assert!(left.matches(&same));
        assert!(!left.matches(&AdmissionCredential::from_bytes(differs_last)));
        assert!(!left.matches(&AdmissionCredential::from_bytes(differs_first)));

        let production = production_source();
        assert!(
            !production.contains("self.0 == other.0"),
            "credential equality must not short-circuit"
        );
        assert!(
            production.contains("fold"),
            "constant-time fold is required"
        );
        assert!(
            !production.contains("impl PartialEq for AdmissionCredential"),
            "deriving equality would reintroduce a short-circuit"
        );
    }

    /// 이 크레이트는 의존성이 없다. 비밀·E2EE·UI·저장소가 데이터 평면으로 새는 경로를
    /// 처음부터 만들지 않는다.
    #[test]
    fn the_wire_contract_has_no_dependencies_and_no_application_vocabulary() {
        let manifest = include_str!("../Cargo.toml");
        let dependencies = manifest
            .split("[dependencies]")
            .nth(1)
            .expect("a dependencies section must exist so its emptiness is explicit");
        assert!(
            dependencies.trim().is_empty(),
            "relay-protocol must stay dependency-free: {dependencies}"
        );

        let production = production_source();
        for forbidden in [
            "workspace",
            "session_id",
            "display_name",
            "permission",
            "terminal",
            "upload",
            "approval",
            "filename",
            "token",
            "SecretString",
        ] {
            assert!(
                !production.to_lowercase().contains(forbidden),
                "the relay must never learn about {forbidden}"
            );
        }
    }

    #[test]
    fn reconnect_wire_accepts_only_the_new_fixed_record_sizes() {
        for (tag, length) in [(0x14, 40), (0x15, 32), (0x16, 32), (0x24, 32)] {
            let mut bytes = RelayFrame::new(
                FrameType::Hello,
                RouteId::from_bytes([1; 16]),
                ConnectionId::from_bytes([2; 16]),
                0,
                &vec![3; length],
            )
            .unwrap()
            .to_vec();
            bytes[6] = tag;
            assert!(RelayFrame::decode(&bytes).is_ok(), "reconnect tag {tag:#x}");
            bytes[51] = (length + 1) as u8;
            bytes.push(3);
            assert!(matches!(
                RelayFrame::decode(&bytes),
                Err(DecodeError::PayloadTooLarge { .. })
            ));
        }
    }

    fn production_source() -> &'static str {
        include_str!("lib.rs")
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .unwrap()
    }
}
