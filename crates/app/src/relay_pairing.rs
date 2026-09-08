//! Relay 페어링 의식 — Mac 쪽 상태 기계. I/O 없음, 시계는 주입받는다.
//!
//! ```text
//! Idle ──begin(relay 연결됨)──▶ Waiting ──기기가 비밀 제시·핸드셰이크 완료──▶ Confirm
//!   ▲                             │                                           │
//!   │        cancel / 만료 / 거부 ─┴───────────────────────────────────────────┘
//!   └──────────────────── Failed(이유) ── begin ──▶ Waiting
//! ```
//!
//! 불변식:
//! - **Relay가 연결돼 있지 않으면 티켓을 발급하지 않는다.** 붙을 곳이 없는 티켓은 5분짜리
//!   비밀만 흘리는 셈이다.
//! - 승인은 `Confirm`에서만 가능하다. `Waiting`에서 온 승인, 만료 뒤에 온 승인은 **오래된
//!   행동(stale action)** 으로 거부되며 아무것도 소비하지 않는다.
//! - 비밀(`PairingSecret`)은 UI 투영에 절대 나가지 않는다. 화면에 보이는 것은 핸드셰이크
//!   transcript에서 유도된 확인 코드뿐이다.
//! - 카운트다운 **안내**는 경계(1분·30초·10초·만료)에서만 바뀐다. 매초 바뀌는 문구는 스크린
//!   리더가 매초 읽게 만든다.
//! - 재시작하면 이 값은 사라진다. 살아남은 pending 행은 Task 2 어댑터가 열릴 때 지운다.

use web_remote::relay::contract::{PairingId, RelayPermissions};
use web_remote::relay::crypto::AuthenticatedHandshake;
use web_remote::relay::pairing::{
    IssuedPairing, PAIRING_PROOF_BYTES, PAIRING_TTL_SECS, PairingApproval, PairingBinding,
    PairingRegistry, PairingRegistryError, PairingSecret,
};

/// 안내가 바뀌는 경계(남은 초). 시작 시의 전체 시간은 별도로 안내한다.
pub const ANNOUNCE_BOUNDARIES_SECS: [u64; 3] = [60, 30, 10];

/// 기기가 붙으면서 가져온 것. 승인 시 그대로 영속 어댑터에 넘긴다.
pub struct DeviceIntroduction {
    pub handshake: AuthenticatedHandshake,
    pub identity_public_sec1: [u8; 65],
    pub display_name: String,
}

impl std::fmt::Debug for DeviceIntroduction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeviceIntroduction")
            .field("display_name_bytes", &self.display_name.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairingFailure {
    /// Relay가 연결돼 있지 않아 시작하지 못했다.
    RelayNotReady,
    /// 5분 안에 기기가 붙지 않았거나, 승인 전에 마감이 지났다.
    Expired,
    /// 사용자가 거부했다.
    Rejected,
    /// 사용자가 취소했다.
    Cancelled,
    /// 기기가 잘못된 비밀을 제시했다.
    InvalidSecret,
    /// 레지스트리가 발급을 거절했다(용량·시계 역행·엔트로피).
    Registry(PairingRegistryError),
}

// `Confirm`이 든 `DeviceIntroduction`은 살아 있는 핸드셰이크 비밀(`AuthenticatedHandshake`)을
// 품는다. 크기를 맞추려고 Box로 감싸면 그 키 재료가 힙으로 복사되고 원래 스택 바이트는
// zeroize되지 않은 채 남는다 — `relay::repository::AdmissionOutcome`이 같은 이유로 같은 선택을
// 한다. 이 값은 의식 하나당 최대 한 번 만들어지므로 크기 차이를 그대로 받는다.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum Phase {
    Idle,
    Waiting {
        ticket: IssuedPairing,
        issued_at: u64,
    },
    Confirm {
        pairing_id: PairingId,
        issued_at: u64,
        code: String,
        introduction: DeviceIntroduction,
    },
    Failed(PairingFailure),
}

/// 왜 승인/거부/취소가 거절됐는가 — 전부 오래된 행동이다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaleAction {
    /// 지금 진행 중인 의식이 없다.
    NoCeremony,
    /// 아직 기기가 붙지 않아 대조할 코드가 없다.
    NotConfirmable,
    /// 마감이 지났다. 상태는 만료로 옮겨졌다.
    Expired,
    Registry(PairingRegistryError),
}

/// UI 투영. 비밀은 여기 없다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairingProjection<'a> {
    Idle,
    Waiting { remaining_secs: u64 },
    Confirm { code: &'a str, remaining_secs: u64 },
    Failed(PairingFailure),
}

pub struct RelayPairingCeremony {
    registry: PairingRegistry,
    phase: Phase,
    /// 마지막으로 안내한 경계. 같은 경계 안에서는 다시 안내하지 않는다.
    last_announced: Option<u64>,
}

impl Default for RelayPairingCeremony {
    fn default() -> Self {
        Self::new()
    }
}

impl RelayPairingCeremony {
    pub fn new() -> Self {
        Self {
            registry: PairingRegistry::new(),
            phase: Phase::Idle,
            last_announced: None,
        }
    }

    /// 의식을 시작한다. **Relay가 연결돼 있지 않으면 티켓을 발급하지 않는다.**
    pub fn begin(&mut self, now: u64, relay_connected: bool) -> Result<(), PairingFailure> {
        if !relay_connected {
            self.phase = Phase::Failed(PairingFailure::RelayNotReady);
            return Err(PairingFailure::RelayNotReady);
        }
        self.abandon_current(now);
        match self.registry.issue(now) {
            Ok(ticket) => {
                self.phase = Phase::Waiting {
                    ticket,
                    issued_at: now,
                };
                // 시작 시 전체 시간을 한 번 안내한다.
                self.last_announced = Some(PAIRING_TTL_SECS);
                Ok(())
            }
            Err(error) => {
                self.phase = Phase::Failed(PairingFailure::Registry(error));
                Err(PairingFailure::Registry(error))
            }
        }
    }

    /// 기기가 붙어 서명 핸드셰이크가 끝났고, 페어링 비밀의 **소유 증명**(HMAC)을 제시했다.
    /// 비밀 자체는 절대 와이어에 오르지 않는다 — Relay는 증명만 본다. 증명이 맞으면 확인
    /// 단계로 넘어가고, 틀리면 실패한다. 기기가 주장한 pairing id가 이 의식의 티켓과 다르면
    /// 검증에 이르지도 못한다.
    pub fn device_presented(
        &mut self,
        now: u64,
        claimed_pairing_id: PairingId,
        proof: &[u8; PAIRING_PROOF_BYTES],
        binding: PairingBinding,
        introduction: DeviceIntroduction,
    ) -> Result<(), PairingFailure> {
        self.tick(now);
        let Phase::Waiting { ticket, issued_at } = &self.phase else {
            return Err(PairingFailure::Expired);
        };
        let pairing_id = ticket.id();
        let issued_at = *issued_at;
        if claimed_pairing_id != pairing_id {
            return Err(PairingFailure::InvalidSecret);
        }
        match self
            .registry
            .verify_proof_for_binding(pairing_id, proof, now, binding)
        {
            Ok(()) => {
                let code = introduction.handshake.confirmation_code().to_owned();
                self.phase = Phase::Confirm {
                    pairing_id,
                    issued_at,
                    code,
                    introduction,
                };
                Ok(())
            }
            Err(PairingRegistryError::Pairing(
                web_remote::relay::pairing::PairingError::InvalidSecret,
            )) => {
                // 시도 횟수는 레지스트리가 센다. 틀린 비밀 한 번으로 의식을 끝내지 않는다 —
                // 상대가 다시 시도할 수 있고, 상한은 레지스트리가 강제한다.
                Err(PairingFailure::InvalidSecret)
            }
            Err(error) => {
                self.phase = Phase::Failed(match error {
                    PairingRegistryError::Pairing(
                        web_remote::relay::pairing::PairingError::Expired,
                    ) => PairingFailure::Expired,
                    PairingRegistryError::Pairing(
                        web_remote::relay::pairing::PairingError::AttemptsExhausted,
                    ) => PairingFailure::InvalidSecret,
                    other => PairingFailure::Registry(other),
                });
                Err(match &self.phase {
                    Phase::Failed(failure) => *failure,
                    _ => PairingFailure::Registry(error),
                })
            }
        }
    }

    /// 사용자가 코드를 대조하고 승인했다. **`Confirm`에서만** 가능하며, 검증된 승인과 기기
    /// 소개를 돌려준다. 호출자는 이를 Task 2 조정자(`PendingAdmission`)에 넘긴다.
    pub fn approve(
        &mut self,
        now: u64,
    ) -> Result<(PairingApproval, DeviceIntroduction), StaleAction> {
        let was_active = self.is_active();
        self.tick(now);
        if was_active && !self.is_active() {
            // 이 호출에서 막 만료됐다 — "의식이 없다"가 아니라 "늦었다"가 정확한 답이다.
            return Err(StaleAction::Expired);
        }
        match &self.phase {
            Phase::Idle | Phase::Failed(_) => Err(StaleAction::NoCeremony),
            Phase::Waiting { .. } => Err(StaleAction::NotConfirmable),
            Phase::Confirm { pairing_id, .. } => {
                let pairing_id = *pairing_id;
                match self.registry.consume(pairing_id, now) {
                    Ok(approval) => {
                        let Phase::Confirm { introduction, .. } =
                            std::mem::replace(&mut self.phase, Phase::Idle)
                        else {
                            unreachable!("phase checked above");
                        };
                        self.last_announced = None;
                        Ok((approval, introduction))
                    }
                    Err(error) => {
                        self.phase = Phase::Failed(PairingFailure::Registry(error));
                        Err(StaleAction::Registry(error))
                    }
                }
            }
        }
    }

    /// 사용자가 거부했다. 확인 단계에서만 의미가 있다.
    pub fn reject(&mut self, now: u64) -> Result<(), StaleAction> {
        let was_active = self.is_active();
        self.tick(now);
        if was_active && !self.is_active() {
            return Err(StaleAction::Expired);
        }
        match &self.phase {
            Phase::Idle | Phase::Failed(_) => Err(StaleAction::NoCeremony),
            Phase::Waiting { .. } => Err(StaleAction::NotConfirmable),
            Phase::Confirm { pairing_id, .. } => {
                let pairing_id = *pairing_id;
                let _ = self.registry.reject(pairing_id, now);
                self.phase = Phase::Failed(PairingFailure::Rejected);
                self.last_announced = None;
                Ok(())
            }
        }
    }

    /// 사용자가 취소했다. 기다리는 중이든 확인 중이든 티켓을 무효화한다.
    pub fn cancel(&mut self, now: u64) -> Result<(), StaleAction> {
        match &self.phase {
            Phase::Idle | Phase::Failed(_) => Err(StaleAction::NoCeremony),
            Phase::Waiting { .. } | Phase::Confirm { .. } => {
                self.abandon_current(now);
                self.phase = Phase::Failed(PairingFailure::Cancelled);
                self.last_announced = None;
                Ok(())
            }
        }
    }

    /// 시계를 흘린다. 마감이 지났으면 만료로 옮긴다.
    pub fn tick(&mut self, now: u64) {
        let deadline = match &self.phase {
            Phase::Waiting { issued_at, .. } | Phase::Confirm { issued_at, .. } => {
                issued_at.saturating_add(PAIRING_TTL_SECS)
            }
            _ => return,
        };
        if now >= deadline {
            self.abandon_current(now);
            self.phase = Phase::Failed(PairingFailure::Expired);
            self.last_announced = None;
        }
    }

    /// 남은 초. 진행 중인 의식이 없으면 `None`.
    pub fn remaining_secs(&self, now: u64) -> Option<u64> {
        match &self.phase {
            Phase::Waiting { issued_at, .. } | Phase::Confirm { issued_at, .. } => Some(
                issued_at
                    .saturating_add(PAIRING_TTL_SECS)
                    .saturating_sub(now),
            ),
            _ => None,
        }
    }

    /// 지금 안내해야 할 남은 초. **경계를 새로 넘었을 때만** `Some`이다.
    ///
    /// 매초 바뀌는 안내는 스크린 리더가 매초 읽게 만든다. 1분·30초·10초·만료에서만 바뀐다.
    pub fn announcement(&mut self, now: u64) -> Option<u64> {
        let remaining = self.remaining_secs(now)?;
        // 남은 시간이 걸치는 **가장 작은** 경계. 45초면 60초 경계, 30초면 30초 경계다.
        let boundary = if remaining == 0 {
            Some(0)
        } else {
            ANNOUNCE_BOUNDARIES_SECS
                .iter()
                .copied()
                .filter(|boundary| remaining <= *boundary)
                .min()
        };
        let boundary = boundary?;
        if self.last_announced == Some(boundary) {
            return None;
        }
        self.last_announced = Some(boundary);
        Some(boundary)
    }

    pub fn projection(&self, now: u64) -> PairingProjection<'_> {
        match &self.phase {
            Phase::Idle => PairingProjection::Idle,
            Phase::Waiting { .. } => PairingProjection::Waiting {
                remaining_secs: self.remaining_secs(now).unwrap_or(0),
            },
            Phase::Confirm { code, .. } => PairingProjection::Confirm {
                code,
                remaining_secs: self.remaining_secs(now).unwrap_or(0),
            },
            Phase::Failed(failure) => PairingProjection::Failed(*failure),
        }
    }

    pub const fn is_active(&self) -> bool {
        matches!(self.phase, Phase::Waiting { .. } | Phase::Confirm { .. })
    }

    /// 기기에 전달할 1회용 비밀. **UI 투영이 아니라** QR/셸 링크 생성기만 부른다(Task 6).
    /// 비밀은 `Waiting`에서만 존재한다 — 기기가 붙은 뒤에는 더 이상 필요 없다.
    pub fn ticket_for_device(&self) -> Option<(PairingId, &PairingSecret)> {
        match &self.phase {
            Phase::Waiting { ticket, .. } => Some((ticket.id(), ticket.secret())),
            _ => None,
        }
    }

    /// 1차 릴리스의 고정 권한. 편집은 없다.
    pub fn first_release_permissions() -> RelayPermissions {
        RelayPermissions::default()
    }

    fn abandon_current(&mut self, now: u64) {
        let id = match &self.phase {
            Phase::Waiting { ticket, .. } => Some(ticket.id()),
            Phase::Confirm { pairing_id, .. } => Some(*pairing_id),
            _ => None,
        };
        if let Some(id) = id {
            let _ = self.registry.reject(id, now);
        }
    }
}

impl std::fmt::Debug for RelayPairingCeremony {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let phase = match &self.phase {
            Phase::Idle => "Idle",
            Phase::Waiting { .. } => "Waiting",
            Phase::Confirm { .. } => "Confirm",
            Phase::Failed(_) => "Failed",
        };
        formatter
            .debug_struct("RelayPairingCeremony")
            .field("phase", &phase)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use web_remote::relay::contract::ConnectionId;
    use web_remote::relay::crypto::{
        PendingHandshake, RELAY_PROTOCOL_VERSION, RelayIdentity, RelayRole,
    };

    const NOW: u64 = 1_800_000_000;

    /// 실제 서명 핸드셰이크로 기기 소개와 바인딩을 만든다. `PairingBinding::new`는 크레이트
    /// 내부라 앱은 이 경로로만 만들 수 있다 — 결과적으로 테스트가 프로덕션 경로를 밟는다.
    fn introduce(connection: u8) -> (DeviceIntroduction, PairingBinding) {
        let desktop_identity = RelayIdentity::generate().unwrap();
        let device_identity = RelayIdentity::generate().unwrap();
        let device_public = *device_identity.public_key_sec1();
        let connection = ConnectionId::from_bytes([connection; 16]);
        let desktop = PendingHandshake::begin(
            desktop_identity,
            device_public.to_vec(),
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
        let device_hello = device.sign_peer_offer(desktop.offer()).unwrap();
        let handshake = desktop.finish(device_hello).unwrap();
        let binding = handshake.pairing_binding();
        (
            DeviceIntroduction {
                handshake,
                identity_public_sec1: device_public,
                display_name: "phone".to_owned(),
            },
            binding,
        )
    }

    fn secret_clone(secret: &PairingSecret) -> PairingSecret {
        let mut bytes = *secret.expose();
        PairingSecret::take_from_bytes(&mut bytes)
    }

    /// 기기가 붙은 상태까지 진행시킨다. 기기는 비밀이 아니라 **증명**을 제시한다.
    fn confirmed(connection: u8) -> RelayPairingCeremony {
        let mut ceremony = RelayPairingCeremony::new();
        ceremony.begin(NOW, true).unwrap();
        let (pairing_id, secret) = ceremony.ticket_for_device().unwrap();
        let secret = secret_clone(secret);
        let (introduction, binding) = introduce(connection);
        let proof = web_remote::relay::pairing::pairing_proof(&secret, &binding);
        ceremony
            .device_presented(NOW + 1, pairing_id, &proof, binding, introduction)
            .unwrap();
        ceremony
    }

    #[test]
    fn a_fresh_ceremony_is_idle_and_has_nothing_to_show() {
        let ceremony = RelayPairingCeremony::new();
        assert_eq!(ceremony.projection(NOW), PairingProjection::Idle);
        assert_eq!(ceremony.remaining_secs(NOW), None);
        assert!(!ceremony.is_active());
        assert!(ceremony.ticket_for_device().is_none());
    }

    /// Relay가 연결돼 있지 않으면 티켓을 발급하지 않는다.
    #[test]
    fn pairing_cannot_begin_until_relay_is_connected() {
        let mut ceremony = RelayPairingCeremony::new();
        assert_eq!(
            ceremony.begin(NOW, false),
            Err(PairingFailure::RelayNotReady)
        );
        assert_eq!(
            ceremony.projection(NOW),
            PairingProjection::Failed(PairingFailure::RelayNotReady)
        );
        assert!(
            ceremony.ticket_for_device().is_none(),
            "티켓이 발급되면 안 된다"
        );

        assert!(ceremony.begin(NOW, true).is_ok());
        assert!(matches!(
            ceremony.projection(NOW),
            PairingProjection::Waiting {
                remaining_secs: PAIRING_TTL_SECS
            }
        ));
    }

    #[test]
    fn the_projection_never_carries_the_secret() {
        let mut ceremony = RelayPairingCeremony::new();
        ceremony.begin(NOW, true).unwrap();
        let (_, secret) = ceremony.ticket_for_device().unwrap();
        let secret_hex: String = secret
            .expose()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let rendered = format!("{:?}", ceremony.projection(NOW));
        assert!(!rendered.contains(&secret_hex));
        assert!(!format!("{ceremony:?}").contains(&secret_hex));
    }

    #[test]
    fn a_device_with_the_right_secret_moves_to_confirm_with_the_transcript_code() {
        let ceremony = confirmed(0x11);
        let PairingProjection::Confirm {
            code,
            remaining_secs,
        } = ceremony.projection(NOW + 1)
        else {
            panic!("확인 단계여야 한다");
        };
        assert_eq!(code.len(), 6, "확인 코드는 6자리다");
        assert!(code.chars().all(|character| character.is_ascii_digit()));
        assert_eq!(remaining_secs, PAIRING_TTL_SECS - 1);
        assert!(
            ceremony.ticket_for_device().is_none(),
            "기기가 붙은 뒤에는 비밀이 더 이상 필요 없다"
        );
    }

    #[test]
    fn a_device_with_the_wrong_secret_is_refused_without_ending_the_ceremony() {
        let mut ceremony = RelayPairingCeremony::new();
        ceremony.begin(NOW, true).unwrap();
        let (pairing_id, _) = ceremony.ticket_for_device().unwrap();
        let mut wrong = [0x5a; 32];
        let wrong = PairingSecret::take_from_bytes(&mut wrong);
        let (introduction, binding) = introduce(0x12);
        let proof = web_remote::relay::pairing::pairing_proof(&wrong, &binding);
        assert_eq!(
            ceremony.device_presented(NOW + 1, pairing_id, &proof, binding, introduction),
            Err(PairingFailure::InvalidSecret)
        );
        assert!(
            matches!(
                ceremony.projection(NOW + 1),
                PairingProjection::Waiting { .. }
            ),
            "틀린 비밀 한 번으로 의식을 끝내지 않는다 — 상한은 레지스트리가 센다"
        );
    }

    /// 다른 티켓 id를 주장하는 기기는 증명이 맞아도 검증에 이르지 못한다.
    #[test]
    fn a_claim_for_a_different_pairing_id_never_reaches_verification() {
        let mut ceremony = RelayPairingCeremony::new();
        ceremony.begin(NOW, true).unwrap();
        let (_, secret) = ceremony.ticket_for_device().unwrap();
        let secret = secret_clone(secret);
        let (introduction, binding) = introduce(0x16);
        let proof = web_remote::relay::pairing::pairing_proof(&secret, &binding);
        let other = PairingId::from_bytes([0x77; 16]);
        assert_eq!(
            ceremony.device_presented(NOW + 1, other, &proof, binding, introduction),
            Err(PairingFailure::InvalidSecret)
        );
        assert!(ceremony.is_active());
    }

    /// 승인은 확인 단계에서만 가능하다. 그 밖의 승인은 전부 오래된 행동이다.
    #[test]
    fn approval_outside_confirm_is_a_stale_action_and_consumes_nothing() {
        let mut ceremony = RelayPairingCeremony::new();
        assert_eq!(ceremony.approve(NOW).err(), Some(StaleAction::NoCeremony));

        ceremony.begin(NOW, true).unwrap();
        assert_eq!(
            ceremony.approve(NOW + 1).err(),
            Some(StaleAction::NotConfirmable),
            "기기가 붙기 전의 승인은 대조할 코드가 없다"
        );
        assert!(ceremony.is_active(), "오래된 승인이 의식을 망치면 안 된다");

        let mut ceremony = confirmed(0x13);
        assert_eq!(
            ceremony.approve(NOW + PAIRING_TTL_SECS).err(),
            Some(StaleAction::Expired),
            "마감 시각의 승인은 만료다"
        );
        assert_eq!(
            ceremony.projection(NOW + PAIRING_TTL_SECS),
            PairingProjection::Failed(PairingFailure::Expired)
        );
    }

    #[test]
    fn approval_in_confirm_yields_the_verified_approval_and_the_introduction() {
        let mut ceremony = confirmed(0x14);
        let (approval, introduction) = ceremony.approve(NOW + 2).unwrap();
        assert_eq!(introduction.display_name, "phone");
        assert_eq!(
            ceremony.projection(NOW + 2),
            PairingProjection::Idle,
            "승인이 끝나면 의식은 비워진다"
        );
        // 승인은 1회용이다 — 같은 티켓을 다시 소비할 수 없다.
        assert_eq!(
            ceremony.approve(NOW + 3).err(),
            Some(StaleAction::NoCeremony)
        );
        // 승인 값은 여기서 끝난다. `PairingApproval`은 Drop을 구현하지 않으므로 `drop()`은
        // 수명만 늘릴 뿐이다 — 소비했다는 사실만 남긴다.
        let _consumed = approval;
    }

    #[test]
    fn reject_and_cancel_end_the_ceremony_and_invalidate_the_ticket() {
        let mut ceremony = confirmed(0x15);
        assert!(ceremony.reject(NOW + 2).is_ok());
        assert_eq!(
            ceremony.projection(NOW + 2),
            PairingProjection::Failed(PairingFailure::Rejected)
        );
        assert_eq!(
            ceremony.approve(NOW + 3).err(),
            Some(StaleAction::NoCeremony)
        );

        let mut ceremony = RelayPairingCeremony::new();
        ceremony.begin(NOW, true).unwrap();
        assert!(ceremony.cancel(NOW + 1).is_ok());
        assert_eq!(
            ceremony.projection(NOW + 1),
            PairingProjection::Failed(PairingFailure::Cancelled)
        );
        assert!(ceremony.ticket_for_device().is_none());
        assert_eq!(ceremony.cancel(NOW + 2), Err(StaleAction::NoCeremony));
    }

    #[test]
    fn the_ceremony_expires_at_exactly_the_five_minute_deadline() {
        let mut ceremony = RelayPairingCeremony::new();
        ceremony.begin(NOW, true).unwrap();
        ceremony.tick(NOW + PAIRING_TTL_SECS - 1);
        assert!(ceremony.is_active());
        assert_eq!(ceremony.remaining_secs(NOW + PAIRING_TTL_SECS - 1), Some(1));

        ceremony.tick(NOW + PAIRING_TTL_SECS);
        assert!(!ceremony.is_active());
        assert_eq!(
            ceremony.projection(NOW + PAIRING_TTL_SECS),
            PairingProjection::Failed(PairingFailure::Expired)
        );
    }

    /// 안내는 경계에서만 바뀐다. 매초 바뀌는 안내는 스크린 리더가 매초 읽게 만든다.
    #[test]
    fn announcements_change_only_at_meaningful_boundaries() {
        let mut ceremony = RelayPairingCeremony::new();
        ceremony.begin(NOW, true).unwrap();

        // 시작 직후 4분 59초 → 4분 1초: 아무 경계도 넘지 않았다.
        for elapsed in 1..(PAIRING_TTL_SECS - 60) {
            assert_eq!(
                ceremony.announcement(NOW + elapsed),
                None,
                "{elapsed}초 경과에 안내가 나가면 안 된다"
            );
        }
        // 1분 경계.
        assert_eq!(ceremony.announcement(NOW + PAIRING_TTL_SECS - 60), Some(60));
        for elapsed in (PAIRING_TTL_SECS - 59)..(PAIRING_TTL_SECS - 30) {
            assert_eq!(ceremony.announcement(NOW + elapsed), None);
        }
        assert_eq!(ceremony.announcement(NOW + PAIRING_TTL_SECS - 30), Some(30));
        assert_eq!(ceremony.announcement(NOW + PAIRING_TTL_SECS - 29), None);
        assert_eq!(ceremony.announcement(NOW + PAIRING_TTL_SECS - 10), Some(10));
        assert_eq!(ceremony.announcement(NOW + PAIRING_TTL_SECS - 1), None);
        // 만료.
        assert_eq!(ceremony.announcement(NOW + PAIRING_TTL_SECS), Some(0));
        assert_eq!(ceremony.announcement(NOW + PAIRING_TTL_SECS + 1), None);
    }

    /// 재시작하면 의식은 사라진다 — 새 값은 언제나 Idle이다. 살아남은 pending 행은
    /// Task 2 어댑터가 열릴 때 지운다.
    #[test]
    fn a_restart_never_resumes_a_ceremony() {
        let mut before = RelayPairingCeremony::new();
        before.begin(NOW, true).unwrap();
        assert!(before.is_active());
        drop(before);

        let after = RelayPairingCeremony::new();
        assert_eq!(after.projection(NOW + 1), PairingProjection::Idle);
        assert!(!after.is_active());
    }

    #[test]
    fn first_release_permissions_are_fixed_view_only() {
        let permissions = RelayPairingCeremony::first_release_permissions();
        use web_remote::relay::contract::RelayAction;
        assert!(permissions.allows(RelayAction::View));
        for action in [
            RelayAction::Input,
            RelayAction::Key,
            RelayAction::Scroll,
            RelayAction::Switch,
            RelayAction::Upload,
            RelayAction::Approval,
        ] {
            assert!(!permissions.allows(action), "{action:?}");
        }
    }
}
