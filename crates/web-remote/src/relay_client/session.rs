//! Mac 쪽 Relay 세션 게이트 — **활성화된 E2EE 채널 없이는 어떤 바이트도 명령이 되지 않는다.**
//!
//! 워커는 소켓에서 받은 원시 바이트를 그대로 넘긴다. Relay는 신뢰하지 않는 중계자이므로 그
//! 바이트는 위조됐을 수 있다. 이 게이트가 하는 일은 단 하나다: DRLY 프레임을 풀고, `Ciphertext`
//! 프레임의 페이로드를 **활성화된 `SecureChannel`로 열어** 통과한 평문만 위로 올린다. 채널이
//! 없으면 무엇이 오든 버린다. 채널이 있어도 인증에 실패하면(재생·변조·순서 어긋남) 채널을
//! 닫고 세션을 끝낸다 — 한 번 어긋난 상대를 계속 받아 줄 이유가 없다.
//!
//! 핸드셰이크 자체는 이 게이트가 하지 않는다. `Hello` 프레임은 **파싱하지 않고 그대로**
//! [`GateOutcome::Hello`]로 올라가며, 그 바이트를 해석하는 것은 옆 모듈의 상태 기계
//! ([`super::handshake::RelayHandshake`])다. 그렇게 나눠야 이 게이트가 "채널을 통과한
//! 평문만 명령이 된다"는 성질 하나만 지키는 작은 값으로 남는다.

use relay_protocol::{DecodeError, FrameType, RelayFrame, RouteId};

use crate::relay::contract::ConnectionId;
use crate::relay::crypto::{
    EncryptedEnvelope, EnvelopeHeader, RELAY_PROTOCOL_VERSION, RelayDirection, SecureChannel,
};

/// 왜 이 바이트가 명령이 되지 못했는가. 전부 조용한 폐기이며, 로그에 페이로드를 남기지 않는다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateDrop {
    /// DRLY 프레임이 아니거나 한 프레임을 초과했다.
    Malformed,
    /// 이 세션의 라우트가 아니다.
    WrongRoute,
    /// 활성화된 채널이 없다. 핸드셰이크 전에 온 암호문(또는 위조 평문)이다.
    NoChannel,
    /// 채널이 묶인 연결 id가 아니다.
    WrongConnection,
    /// 이 전송에서 의미 없는 프레임 종류(서버 제어 프레임의 되쏨 등).
    Unexpected,
}

#[derive(Debug, PartialEq, Eq)]
pub enum GateOutcome {
    /// 버렸다. 세션은 계속된다.
    Dropped(GateDrop),
    /// 채널을 통과한 평문. 이것만 명령이 될 수 있다.
    Plaintext(Vec<u8>),
    /// pre-E2EE hello 레코드. 게이트는 **해석하지 않고** 바이트 그대로 올린다.
    Hello {
        connection_id: ConnectionId,
        record: Vec<u8>,
    },
    /// 서버 제어 프레임. 세션 상태 관찰에만 쓴다 — `PeerJoined`의 연결 id가 이 기기
    /// 세션의 연결 id가 되므로 헤더의 값을 함께 올린다.
    Control {
        frame_type: FrameType,
        connection_id: ConnectionId,
    },
    /// 세션을 끝내야 한다 — 서버가 닫았거나, 채널 인증이 실패했다.
    Close,
}

pub struct RelaySessionGate {
    route: RouteId,
    channel: Option<(ConnectionId, SecureChannel)>,
}

impl RelaySessionGate {
    pub const fn new(route: RouteId) -> Self {
        Self {
            route,
            channel: None,
        }
    }

    pub const fn is_active(&self) -> bool {
        self.channel.is_some()
    }

    /// 조정자가 `PendingAdmission::approve`(새 페어링) 또는 `confirm_admitted`(이미 페어링된
    /// 기기)로 얻은 채널을 건다. 이 순간부터 이 연결 id의 `Ciphertext` 프레임만 평문이 될 수
    /// 있다.
    ///
    /// 연결 id는 **채널 자신에게서** 읽는다 — 별도 인자로 받으면 핸드셰이크가 묶인 것과 다른
    /// 연결에 채널을 거는 실수가 타입으로 가능해진다.
    pub fn activate(&mut self, channel: SecureChannel) {
        self.deactivate();
        self.channel = Some((channel.connection_id(), channel));
    }

    /// 취소·권한 강등·세션 종료. 채널을 닫고 잊는다.
    pub fn deactivate(&mut self) {
        if let Some((_, mut channel)) = self.channel.take() {
            channel.close();
        }
    }

    /// 활성 채널로 평문을 봉인해 이 연결의 `Ciphertext` 프레임을 만든다. 채널이 없으면 `None` —
    /// 게이트를 거치지 않고 나가는 평문은 없다. 봉인 실패(닫힌 채널·상한 초과·시퀀스 고갈)는
    /// 채널을 닫는다: 한 번 어긋난 송신 순서를 이어 가면 상대는 그 뒤를 전부 버린다.
    pub fn seal(&mut self, plaintext: &[u8]) -> Option<Vec<u8>> {
        let sealed = {
            let (connection_id, channel) = self.channel.as_mut()?;
            channel
                .seal(plaintext)
                .map(|envelope| (*connection_id, envelope))
        };
        let (connection_id, envelope) = match sealed {
            Ok(sealed) => sealed,
            Err(_) => {
                self.deactivate();
                return None;
            }
        };
        // DRLY 헤더의 시퀀스는 봉투의 시퀀스와 같은 값이다 — 상대 게이트가 헤더에서 읽는다.
        let Ok(frame) = RelayFrame::new(
            FrameType::Ciphertext,
            self.route,
            relay_protocol::ConnectionId::from_bytes(*connection_id.as_bytes()),
            envelope.header().sequence,
            envelope.ciphertext_and_tag(),
        ) else {
            // 봉인이 이미 송신 시퀀스를 소비했다. 이 프레임을 못 내보내면 상대는 그 뒤의
            // 모든 암호문을 순서 위반으로 버린다 — 조용히 어긋난 채로 두지 않는다.
            self.deactivate();
            return None;
        };
        Some(frame.to_vec())
    }

    /// 원시 바이트 하나를 판정한다. **여기서 `Plaintext`로 나오지 않은 바이트는 명령이 아니다.**
    pub fn receive(&mut self, raw: &[u8]) -> GateOutcome {
        let frame = match RelayFrame::decode(raw) {
            Ok((frame, consumed)) if consumed == raw.len() => frame,
            Ok(_) | Err(DecodeError::Incomplete { .. }) => {
                return GateOutcome::Dropped(GateDrop::Malformed);
            }
            Err(_) => return GateOutcome::Dropped(GateDrop::Malformed),
        };
        if frame.route_id() != self.route {
            return GateOutcome::Dropped(GateDrop::WrongRoute);
        }

        let connection_id = ConnectionId::from_bytes(*frame.connection_id().as_bytes());
        match frame.frame_type() {
            FrameType::Ciphertext => self.open(&frame),
            FrameType::Hello => GateOutcome::Hello {
                connection_id,
                record: frame.payload().to_vec(),
            },
            FrameType::Admitted
            | FrameType::PeerJoined
            | FrameType::PeerLeft
            | FrameType::Heartbeat => GateOutcome::Control {
                frame_type: frame.frame_type(),
                connection_id,
            },
            FrameType::Close | FrameType::Rejected => GateOutcome::Close,
            FrameType::DesktopAdmission
            | FrameType::DeviceAdmission
            | FrameType::TicketPublish
            | FrameType::TicketRevoke => GateOutcome::Dropped(GateDrop::Unexpected),
        }
    }

    fn open(&mut self, frame: &RelayFrame<'_>) -> GateOutcome {
        let Some((connection_id, channel)) = self.channel.as_mut() else {
            return GateOutcome::Dropped(GateDrop::NoChannel);
        };
        // DRLY 헤더의 연결 id(16바이트)는 E2EE 계약의 연결 id와 같은 바이트다.
        if frame.connection_id().as_bytes() != connection_id.as_bytes() {
            return GateOutcome::Dropped(GateDrop::WrongConnection);
        }
        let header = EnvelopeHeader {
            version: RELAY_PROTOCOL_VERSION,
            connection_id: *connection_id,
            // Mac은 기기→데스크톱 방향만 연다. 반대 방향 프레임은 인증에서 걸린다.
            direction: RelayDirection::DeviceToDesktop,
            sequence: frame.sequence(),
        };
        let Ok(envelope) = EncryptedEnvelope::from_webcrypto_parts(header, frame.payload()) else {
            return GateOutcome::Dropped(GateDrop::Malformed);
        };
        match channel.open(&envelope) {
            Ok(plaintext) => GateOutcome::Plaintext(plaintext),
            Err(_) => {
                // 재생·변조·순서 어긋남 — 한 번 어긋난 상대를 계속 받아 줄 이유가 없다.
                self.deactivate();
                GateOutcome::Close
            }
        }
    }
}

impl std::fmt::Debug for RelaySessionGate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RelaySessionGate")
            .field("route", &self.route)
            .field("active", &self.channel.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::crypto::{PendingHandshake, RelayIdentity, RelayRole};
    use crate::relay::pairing::PairingRegistry;

    const ROUTE: RouteId = RouteId::from_bytes([0x41; 16]);
    const CONNECTION: [u8; 16] = [0x22; 16];

    /// 실제 핸드셰이크로 양쪽 채널을 만든다.
    fn channels() -> (SecureChannel, SecureChannel) {
        let connection = ConnectionId::from_bytes(CONNECTION);
        let desktop_identity = RelayIdentity::generate().unwrap();
        let device_identity = RelayIdentity::generate().unwrap();
        let device_public = device_identity.public_key_sec1().to_vec();
        let desktop = PendingHandshake::begin(
            desktop_identity,
            device_public,
            RelayRole::Desktop,
            RELAY_PROTOCOL_VERSION,
            connection,
        )
        .unwrap();
        let device = PendingHandshake::begin(
            device_identity,
            desktop.identity().public_key_sec1().to_vec(),
            RelayRole::Device,
            RELAY_PROTOCOL_VERSION,
            connection,
        )
        .unwrap();
        let desktop_hello = desktop.sign_peer_offer(device.offer()).unwrap();
        let device_hello = device.sign_peer_offer(desktop.offer()).unwrap();
        let desktop = desktop.finish(device_hello).unwrap();
        let device = device.finish(desktop_hello).unwrap();

        let mut registry = PairingRegistry::new();
        let issued = registry.issue(1_800_000_000).unwrap();
        registry
            .verify_secret_for_binding(
                issued.id(),
                issued.secret(),
                1_800_000_000,
                desktop.pairing_binding(),
            )
            .unwrap();
        let approval = registry.consume(issued.id(), 1_800_000_000).unwrap();
        (
            desktop.confirm(approval).unwrap(),
            device.confirm_device_for_test(),
        )
    }

    fn ciphertext_frame(connection: [u8; 16], sequence: u64, payload: &[u8]) -> Vec<u8> {
        RelayFrame::new(
            FrameType::Ciphertext,
            ROUTE,
            relay_protocol::ConnectionId::from_bytes(connection),
            sequence,
            payload,
        )
        .unwrap()
        .to_vec()
    }

    /// 핵심 성질: 채널이 없으면 **어떤 바이트도** 평문이 되지 않는다. 신뢰하지 않는 Relay가
    /// 평문 JSON 명령을 그대로 밀어 넣어도 버려진다.
    #[test]
    fn without_an_active_channel_no_bytes_become_a_command() {
        let mut gate = RelaySessionGate::new(ROUTE);
        let forged = br#"{"type":"watch","session":"3"}"#;

        // 원시 JSON — DRLY 프레임조차 아니다.
        assert_eq!(
            gate.receive(forged),
            GateOutcome::Dropped(GateDrop::Malformed)
        );
        // DRLY로 감싼 평문 JSON — 채널이 없으니 버린다.
        assert_eq!(
            gate.receive(&ciphertext_frame(CONNECTION, 0, forged)),
            GateOutcome::Dropped(GateDrop::NoChannel)
        );
        // Hello는 **해석되지 않은 바이트**로 올라갈 뿐, 명령이 되지 않는다.
        let hello = RelayFrame::new(
            FrameType::Hello,
            ROUTE,
            relay_protocol::ConnectionId::from_bytes(CONNECTION),
            0,
            forged,
        )
        .unwrap()
        .to_vec();
        assert_eq!(
            gate.receive(&hello),
            GateOutcome::Hello {
                connection_id: ConnectionId::from_bytes(CONNECTION),
                record: forged.to_vec(),
            }
        );
        assert!(!gate.is_active(), "hello 하나로 채널이 서지 않는다");
    }

    /// 채널이 있어도 그 채널로 봉인되지 않은 바이트는 인증에서 걸리고 세션을 끝낸다.
    #[test]
    fn a_forged_frame_on_an_active_channel_fails_authentication_and_closes() {
        let (desktop, _device) = channels();
        let mut gate = RelaySessionGate::new(ROUTE);
        gate.activate(desktop);
        assert!(gate.is_active());

        let forged = vec![0x5a; 64];
        assert_eq!(
            gate.receive(&ciphertext_frame(CONNECTION, 0, &forged)),
            GateOutcome::Close
        );
        assert!(!gate.is_active(), "인증 실패는 채널을 닫는다");
        // 닫힌 뒤에는 진짜 프레임도 더 받지 않는다.
        assert_eq!(
            gate.receive(&ciphertext_frame(CONNECTION, 1, &forged)),
            GateOutcome::Dropped(GateDrop::NoChannel)
        );
    }

    /// 기기 채널로 봉인한 프레임만 평문이 된다. 순서·연결 id·라우트가 어긋나면 안 된다.
    #[test]
    fn only_frames_sealed_by_the_peer_channel_become_plaintext() {
        let (desktop, mut device) = channels();
        let mut gate = RelaySessionGate::new(ROUTE);
        gate.activate(desktop);

        let envelope = device.seal(br#"{"type":"request_keyframe"}"#).unwrap();
        let frame = ciphertext_frame(
            CONNECTION,
            envelope.header().sequence,
            envelope.ciphertext_and_tag(),
        );
        assert_eq!(
            gate.receive(&frame),
            GateOutcome::Plaintext(br#"{"type":"request_keyframe"}"#.to_vec())
        );

        // 같은 프레임을 다시 보내면 재생이다 — 닫힌다.
        assert_eq!(gate.receive(&frame), GateOutcome::Close);
    }

    #[test]
    fn wrong_route_or_connection_is_dropped_before_any_decryption() {
        let (desktop, mut device) = channels();
        let mut gate = RelaySessionGate::new(ROUTE);
        gate.activate(desktop);
        let envelope = device.seal(b"x").unwrap();

        let other_route = RelayFrame::new(
            FrameType::Ciphertext,
            RouteId::from_bytes([0x99; 16]),
            relay_protocol::ConnectionId::from_bytes(CONNECTION),
            envelope.header().sequence,
            envelope.ciphertext_and_tag(),
        )
        .unwrap()
        .to_vec();
        assert_eq!(
            gate.receive(&other_route),
            GateOutcome::Dropped(GateDrop::WrongRoute)
        );

        let other_connection = ciphertext_frame(
            [0x77; 16],
            envelope.header().sequence,
            envelope.ciphertext_and_tag(),
        );
        assert_eq!(
            gate.receive(&other_connection),
            GateOutcome::Dropped(GateDrop::WrongConnection)
        );
        assert!(
            gate.is_active(),
            "라우트/연결 불일치는 채널을 건드리지 않는다"
        );
    }

    #[test]
    fn server_control_frames_are_observed_and_close_ends_the_session() {
        let mut gate = RelaySessionGate::new(ROUTE);
        for frame_type in [
            FrameType::Admitted,
            FrameType::PeerJoined,
            FrameType::PeerLeft,
            FrameType::Heartbeat,
        ] {
            let frame = RelayFrame::new(
                frame_type,
                ROUTE,
                relay_protocol::ConnectionId::from_bytes(CONNECTION),
                0,
                &[],
            )
            .unwrap()
            .to_vec();
            assert_eq!(
                gate.receive(&frame),
                GateOutcome::Control {
                    frame_type,
                    connection_id: ConnectionId::from_bytes(CONNECTION),
                }
            );
        }
        let close = RelayFrame::new(
            FrameType::Close,
            ROUTE,
            relay_protocol::ConnectionId::from_bytes(CONNECTION),
            0,
            &relay_protocol::RejectionCode::PeerDisconnected.to_bytes(),
        )
        .unwrap()
        .to_vec();
        assert_eq!(gate.receive(&close), GateOutcome::Close);
    }

    #[test]
    fn deactivation_closes_the_channel_and_is_idempotent() {
        let (desktop, _device) = channels();
        let mut gate = RelaySessionGate::new(ROUTE);
        gate.activate(desktop);
        gate.deactivate();
        assert!(!gate.is_active());
        gate.deactivate();
        assert_eq!(
            gate.receive(&ciphertext_frame(CONNECTION, 0, b"x")),
            GateOutcome::Dropped(GateDrop::NoChannel)
        );
    }

    /// 이 모듈에는 평문 파서가 없다. `ClientMsg::parse`나 `from_utf8`가 여기 들어오면
    /// 게이트가 아니라 구멍이 된다.
    #[test]
    fn the_gate_never_parses_application_messages_itself() {
        let production = include_str!("session.rs")
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .unwrap();
        for forbidden in ["ClientMsg", "from_utf8", "serde_json"] {
            assert!(!production.contains(forbidden), "{forbidden}");
        }
    }

    /// 송신도 같은 게이트를 지난다 — 채널 없이는 봉인할 것이 없고, 봉인된 프레임은 상대 채널이
    /// 정확히 연다.
    #[test]
    fn sealing_requires_an_active_channel_and_the_peer_opens_the_frame() {
        let (desktop, mut device) = channels();
        let mut gate = RelaySessionGate::new(ROUTE);
        assert!(
            gate.seal(b"nothing").is_none(),
            "채널 없이 나가는 평문은 없다"
        );

        gate.activate(desktop);
        let first = gate.seal(br#"{"type":"dashboard"}"#).unwrap();
        let second = gate.seal(b"second").unwrap();
        for (raw, expected) in [
            (first, &br#"{"type":"dashboard"}"#[..]),
            (second, b"second"),
        ] {
            let (frame, consumed) = RelayFrame::decode(&raw).unwrap();
            assert_eq!(consumed, raw.len());
            assert_eq!(frame.frame_type(), FrameType::Ciphertext);
            assert_eq!(frame.route_id(), ROUTE);
            assert_eq!(frame.connection_id().as_bytes(), &CONNECTION);
            let envelope = EncryptedEnvelope::from_webcrypto_parts(
                EnvelopeHeader {
                    version: RELAY_PROTOCOL_VERSION,
                    connection_id: ConnectionId::from_bytes(CONNECTION),
                    direction: RelayDirection::DesktopToDevice,
                    sequence: frame.sequence(),
                },
                frame.payload(),
            )
            .unwrap();
            assert_eq!(device.open(&envelope).unwrap(), expected);
        }
        assert!(gate.is_active());
    }
}
